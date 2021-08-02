//!
//! # GDSII Import & Export Module
//!

use super::*;

/// # Gds21 Converter
/// Converts a [raw::Library] to a GDSII library ([gds21::GdsLibrary]).
/// The sole valid top-level entity for conversion is always a [Library].
#[derive(Debug)]
pub struct GdsExporter {
    pub lib: Library,
}
impl GdsExporter {
    pub fn export(lib: Library) -> LayoutResult<gds21::GdsLibrary> {
        Self { lib }.export_all()
    }
    fn export_all(self) -> LayoutResult<gds21::GdsLibrary> {
        if self.lib.libs.len() > 0 {
            return self.err("No nested libraries to GDS (yet)");
        }
        // Create a new Gds Library
        let mut lib = gds21::GdsLibrary::new(&self.lib.name);
        // Set its distance units
        // In all cases the GDSII "user units" are set to 1µm.
        lib.units = match self.lib.units {
            Unit::Micro => gds21::GdsUnits::new(1.0, 1e-6),
            Unit::Nano => gds21::GdsUnits::new(1e-3, 1e-9),
            Unit::Angstrom => gds21::GdsUnits::new(1e-4, 1e-10),
        };
        // And convert each of our `cells` into its `structs`
        lib.structs = self
            .lib
            .cells
            .iter()
            .map(|c| self.export_cell(c))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(lib)
    }
    /// Convert a [Cell] to a [gds21::GdsStruct] cell-definition
    fn export_cell(&self, cell: &Cell) -> LayoutResult<gds21::GdsStruct> {
        let mut elems = Vec::with_capacity(cell.elems.len() + cell.insts.len());
        // Convert each [Instance]
        for inst in cell.insts.iter() {
            elems.push(self.export_instance(inst)?.into());
        }
        // Convert each [Element]
        // Note each can produce more than one [GdsElement]
        for elem in cell.elems.iter() {
            for gdselem in self.export_element(elem)?.into_iter() {
                elems.push(gdselem);
            }
        }
        let mut s = gds21::GdsStruct::new(&cell.name);
        s.elems = elems;
        Ok(s)
    }
    /// Convert an [Instance] to a GDS instance, AKA [gds21::GdsStructRef]
    fn export_instance(&self, inst: &Instance) -> LayoutResult<gds21::GdsStructRef> {
        Ok(gds21::GdsStructRef {
            name: inst.cell_name.clone(),
            xy: self.export_point(&inst.p0)?,
            strans: None, //FIXME!
            ..Default::default()
        })
    }
    /// Convert an [Element] into one or more [gds21::GdsElement]
    ///
    /// Our [Element]s often correspond to more than one GDSII element,
    /// notably in the case in which a polygon is annotated with a net-name.
    /// Here, the net-name is an attribute of the polygon [Element].
    /// In GDSII, text is "free floating" as a separate element.
    ///
    /// GDS shapes are flattened vectors of (x,y) coordinates,
    /// and include an explicit repetition of their origin for closure.
    /// So an N-sided polygon is described by a 2*(N+1)-entry vector.
    ///
    pub fn export_element(&self, elem: &Element) -> LayoutResult<Vec<gds21::GdsElement>> {
        let layer = self
            .lib
            .layers
            .get(elem.layer)
            .ok_or(LayoutError::msg("Layer Not Defined"))?;
        let datatype = layer
            .num(&elem.purpose)
            .ok_or(LayoutError::msg(format!(
                "LayerPurpose Not Defined for {}, {:?}",
                layer.layernum, elem.purpose
            )))?
            .clone();

        let xy: Vec<gds21::GdsPoint> = match &elem.inner {
            Shape::Rect { p0, p1 } => {
                let x0 = p0.x.try_into()?;
                let y0 = p0.y.try_into()?;
                let x1 = p1.x.try_into()?;
                let y1 = p1.y.try_into()?;
                gds21::GdsPoint::vec(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)])
            }
            Shape::Poly { pts } => {
                // Flatten our points-vec, converting to 32-bit along the way
                let mut xy = Vec::new();
                for p in pts.iter() {
                    xy.push(self.export_point(p)?);
                }
                // Add the origin a second time, to "close" the polygon
                xy.push(self.export_point(&pts[0])?);
                xy
            }
            Shape::Path { .. } => todo!(),
        };
        // Initialize our vector of elements with the shape
        let mut gds_elems = vec![gds21::GdsBoundary {
            layer: layer.layernum,
            datatype,
            xy,
            ..Default::default()
        }
        .into()];
        // If there's an assigned net, create a corresponding text-element
        if let Some(name) = &elem.net {
            let texttype = layer
                .num(&LayerPurpose::Label)
                .ok_or(LayoutError::msg("Text Layer Not Defined"))?
                .clone();

            // Text is placed in the shape's (at least rough) center
            let loc = elem.inner.center();
            // Rotate that text 90 degrees for mostly-vertical shapes
            let strans = match elem.inner.orientation() {
                Dir::Horiz => None,
                Dir::Vert => Some(gds21::GdsStrans {
                    angle: Some(90.0),
                    ..Default::default()
                }),
            };
            gds_elems.push(
                gds21::GdsTextElem {
                    string: name.into(),
                    layer: layer.layernum,
                    texttype,
                    xy: self.export_point(&loc)?,
                    strans,
                    ..Default::default()
                }
                .into(),
            )
        }
        Ok(gds_elems)
    }
    pub fn export_point(&self, pt: &Point) -> LayoutResult<gds21::GdsPoint> {
        let x = pt.x.try_into()?;
        let y = pt.y.try_into()?;
        Ok(gds21::GdsPoint::new(x, y))
    }
    /// Error creation helper
    fn err<T>(&self, msg: impl Into<String>) -> LayoutResult<T> {
        Err(LayoutError::Export(msg.into()))
    }
}
/// # GDSII Importer
///
#[derive(Debug)]
pub struct GdsImporter {
    pub layers: Layers,
    ctx_stack: Vec<ImportContext>,
    unsupported: Vec<gds21::GdsElement>,
}
impl GdsImporter {
    /// Import a [gds21::GdsLibrary] into a [Library]
    /// FIXME: optionally provide layer definitions
    pub fn import(lib: gds21::GdsLibrary) -> LayoutResult<Library> {
        let mut importer = Self {
            layers: Layers::default(),
            ctx_stack: vec![ImportContext::Library(lib.name.clone())],
            unsupported: vec![],
        };
        let mut rv = importer.import_lib(&lib)?;
        let Self {
            layers,
            unsupported,
            ..
        } = importer;
        if unsupported.len() > 0 {
            println!(
                "Read {} Unsupported GDS Elements: {:?}",
                unsupported.len(),
                unsupported
            );
        }
        rv.layers = layers;
        Ok(rv)
    }
    /// Internal implementation method. Convert all, starting from our top-level [gds21::GdsLibrary].
    fn import_lib(&mut self, gdslib: &gds21::GdsLibrary) -> LayoutResult<Library> {
        // Check our GDS doesn't (somehow) include any unsupported features
        if gdslib.libdirsize.is_some()
            || gdslib.srfname.is_some()
            || gdslib.libsecur.is_some()
            || gdslib.reflibs.is_some()
            || gdslib.fonts.is_some()
            || gdslib.attrtable.is_some()
            || gdslib.generations.is_some()
            || gdslib.format_type.is_some()
        {
            return self.err("Unsupported GDSII Feature");
        }
        // Create a new [Library]
        let mut lib = Library::default();
        // Give it the same name as the GDS
        lib.name = gdslib.name.clone();
        // Set its distance units
        lib.units = self.import_units(&gdslib.units)?;
        // And convert each of its `structs` into our `cells`
        lib.cells = gdslib
            .structs
            .iter()
            .map(|x| self.import_cell(x))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(lib)
    }
    /// Import our [Unit]s
    fn import_units(&mut self, units: &gds21::GdsUnits) -> LayoutResult<Unit> {
        self.ctx_stack.push(ImportContext::Units);
        // Peel out the GDS "database unit", the one of its numbers that really matters
        let gdsunit = units.dbunit();
        // FIXME: intermediate/ calculated units. Only our enumerated values are thus far supported
        // Note: sadly many real-life GDSII files set, for example "1nm" units,
        // but do so with the floating-point number *next to* 1e-9.
        // These files presumably rely on other software "converging" to 1nm, as we do here.
        let rv = if (gdsunit - 1e-10).abs() < 1e-13 {
            Unit::Angstrom
        } else if (gdsunit - 1e-9).abs() < 1e-12 {
            Unit::Nano
        } else if (gdsunit - 1e-6).abs() < 1e-9 {
            Unit::Micro
        } else {
            return self.err(format!("Unsupported GDSII Unit: {:10.3e}", gdsunit));
        };
        self.ctx_stack.pop();
        Ok(rv)
    }
    /// Import a GDS Cell ([gds21::GdsStruct]) into a [Cell]
    fn import_cell(&mut self, strukt: &gds21::GdsStruct) -> LayoutResult<Cell> {
        let mut cell = Cell::default();
        let name = strukt.name.clone();
        cell.name = name.clone();
        self.ctx_stack.push(ImportContext::Cell(name));
        // Importing each cell requires at least two passes over its elements.
        // In the first pass we add each [Instance] and geometric element,
        // And keep a list of [gds21::GdsTextElem] on the side.
        let mut texts: Vec<&gds21::GdsTextElem> = Vec::new();
        let mut elems: SlotMap<ElementKey, Element> = SlotMap::with_key();
        // Also keep a hash of by-layer elements, to aid in text-assignment in our second pass
        let mut layers: HashMap<i16, Vec<ElementKey>> = HashMap::new();
        for elem in &strukt.elems {
            /// A quick local enum, indicating whether each GDS element causes us
            /// to add a new [Element]. If so, more stuff is to be done.
            enum AddingAnElement {
                Yes(Element),
                No(()),
            }
            use gds21::GdsElement::*;
            use AddingAnElement::{No, Yes};
            let e = match elem {
                GdsBoundary(ref x) => Yes(self.import_boundary(x)?),
                GdsPath(ref x) => Yes(self.import_path(x)?),
                GdsBox(ref x) => Yes(self.import_box(x)?),
                GdsArrayRef(ref x) => No(cell.insts.extend(self.import_instance_array(x)?)),
                GdsStructRef(ref x) => No(cell.insts.push(self.import_instance(x)?)),
                GdsTextElem(ref x) => No(texts.push(x)),
                // GDSII "Node" elements are fairly rare, and are not supported.
                // (Maybe some day we'll even learn what they are.)
                GdsNode(ref x) => No(self.unsupported.push(x.clone().into())),
            };
            // If we got a new element, add it to our per-layer hash
            if let Yes(e) = e {
                let layernum = match self.layers.get(e.layer) {
                    Some(l) => l.layernum,
                    None => return self.err("Internal error: element added to invalid layer"),
                };
                let ekey = elems.insert(e);
                if let Some(ref mut bucket) = layers.get_mut(&layernum) {
                    bucket.push(ekey);
                } else {
                    layers.insert(layernum, vec![ekey]);
                }
            }
        }
        // Pass two: sort out whether each [gds21::GdsTextElem] is a net-label,
        // And if so, assign it as a net-name on each intersecting [Element].
        // Text elements which do not overlap a geometric element on the same layer
        // are converted to annotations.
        for textelem in &texts {
            let loc = self.import_point(&textelem.xy)?;
            if let Some(layer) = layers.get(&textelem.layer) {
                // Layer exists in geometry; see which elements intersect with this text
                let mut hit = false;
                for ekey in layer.iter() {
                    let elem = elems.get_mut(*ekey).unwrap();
                    if elem.inner.contains(&loc) {
                        // Label lands inside this element.
                        // Check whether we have an existing label.
                        // If so, it better be the same net name!
                        // FIXME: casing, as usual with all EDA crap.
                        // Here we support case *insensitive* GDSes, and lower-case everything.
                        // Many GDS seem to mix and match upper and lower case,
                        // essentially using the case-insensitivity for connections (bleh).
                        let lower_case_name = textelem.string.to_lowercase();
                        if let Some(pname) = &elem.net {
                            if *pname != lower_case_name {
                                return self.err(format!(
                                    "GDSII labels shorting nets {} and {} on layer {}",
                                    pname,
                                    textelem.string.clone(),
                                    textelem.layer
                                ));
                            }
                        }
                        elem.net = Some(lower_case_name);
                        hit = true;
                    }
                }
                // If we've hit at least one, carry onto the next TextElement
                if hit {
                    continue;
                }
            }
            // No hits (or a no-shape Layer). Create an annotation instead.
            cell.annotations.push(TextElement {
                string: textelem.string.clone(),
                loc,
            });
        }
        // Pull the elements out of the local slot-map, into the vector that [Cell] wants
        cell.elems = elems.drain().map(|(_k, v)| v).collect();
        self.ctx_stack.pop();
        Ok(cell)
    }
    /// Import a [gds21::GdsBoundary] into an [Element]
    fn import_boundary(&mut self, x: &gds21::GdsBoundary) -> LayoutResult<Element> {
        self.ctx_stack.push(ImportContext::Geometry);
        let mut pts: Vec<Point> = self.import_point_vec(&x.xy)?;
        if pts[0] != *pts.last().unwrap() {
            return self.err("GDS Boundary must start and end at the same point");
        }
        // Pop the redundant last entry
        pts.pop();
        // Check for Rectangles; they help
        let inner = if pts.len() == 4
            && ((pts[0].x == pts[1].x // Clockwise
                && pts[1].y == pts[2].y
                && pts[2].x == pts[3].x
                && pts[3].y == pts[0].y)
                || (pts[0].y == pts[1].y // Counter-clockwise
                    && pts[1].x == pts[2].x
                    && pts[2].y == pts[3].y
                    && pts[3].x == pts[0].x))
        {
            // That makes this a Rectangle.
            Shape::Rect {
                p0: pts[0].clone(),
                p1: pts[2].clone(),
            }
        } else {
            // Otherwise, it's a polygon
            Shape::Poly { pts }
        };

        // Grab (or create) its [Layer]
        let (layer, purpose) = self.import_element_layer(x)?;
        // Create the Element, and insert it in our slotmap
        let e = Element {
            net: None,
            layer,
            purpose,
            inner,
        };
        self.ctx_stack.pop();
        Ok(e)
    }
    /// Import a [gds21::GdsBox] into an [Element]
    fn import_box(&mut self, x: &gds21::GdsBox) -> LayoutResult<Element> {
        self.ctx_stack.push(ImportContext::Geometry);

        // GDS stores *five* coordinates per box (for whatever reason).
        // This does not check fox "box validity", and imports the
        // first and third of those five coordinates,
        // which are by necessity for a valid [GdsBox] located at opposite corners.
        let inner = Shape::Rect {
            p0: self.import_point(&x.xy[0])?,
            p1: self.import_point(&x.xy[2])?,
        };

        // Grab (or create) its [Layer]
        let (layer, purpose) = self.import_element_layer(x)?;
        // Create the Element, and insert it in our slotmap
        let e = Element {
            net: None,
            layer,
            purpose,
            inner,
        };
        self.ctx_stack.pop();
        Ok(e)
    }
    /// Import a [gds21::GdsPath] into an [Element]
    fn import_path(&mut self, x: &gds21::GdsPath) -> LayoutResult<Element> {
        self.ctx_stack.push(ImportContext::Geometry);

        let pts = self.import_point_vec(&x.xy)?;
        let width = if let Some(w) = x.width {
            w as usize
        } else {
            return self.err("Invalid nonspecifed GDS Path width ");
        };
        // Create the shape
        let inner = Shape::Path { width, pts };

        // Grab (or create) its [Layer]
        let (layer, purpose) = self.import_element_layer(x)?;
        // Create the Element, and insert it in our slotmap
        let e = Element {
            net: None,
            layer,
            purpose,
            inner,
        };
        self.ctx_stack.pop();
        Ok(e)
    }
    /// Import a [gds21::GdsStructRef] cell/struct-instance into an [Instance]
    fn import_instance(&mut self, sref: &gds21::GdsStructRef) -> LayoutResult<Instance> {
        let inst_name = "".into(); // FIXME
        let cname = sref.name.clone();
        let cell = CellRef::Name(cname.clone()); // FIXME
        self.ctx_stack.push(ImportContext::Instance(cname.clone()));
        let p0 = self.import_point(&sref.xy)?;
        let mut inst = Instance {
            inst_name,
            cell_name: cname,
            cell,
            p0,
            reflect: false, // FIXME!
            angle: None,    // FIXME!
        };
        if let Some(strans) = &sref.strans {
            // FIXME: interpretation of the "absolute" settings
            if strans.abs_mag || strans.abs_angle {
                return self.err("Unsupported GDSII Instance: Absolute");
            }
            if strans.mag.is_some() || strans.angle.is_some() {
                println!("Warning support for instance orientation in-progress");
            }
            inst.reflect = strans.reflected;
            inst.angle = strans.angle;
        }
        self.ctx_stack.pop();
        Ok(inst)
    }
    /// Import a (two-dimensional) [gds21::GdsArrayRef] into [Instance]s
    fn import_instance_array(&mut self, aref: &gds21::GdsArrayRef) -> LayoutResult<Vec<Instance>> {
        let inst_name = "".to_string(); // FIXME
        let cell_name = aref.name.clone();
        self.ctx_stack.push(ImportContext::Array(cell_name.clone()));
        let cell = CellRef::Name(aref.name.clone()); // FIXME

        // Convert its three (x,y) coordinates
        let p0 = self.import_point(&aref.xy[0])?;
        let p1 = self.import_point(&aref.xy[1])?;
        let p2 = self.import_point(&aref.xy[2])?;
        // Check for (thus far) unsupported non-rectangular arrays
        if p0.y != p1.y || p0.x != p2.x {
            return self.err("Invalid Non-Rectangular GDS Array");
        }
        // Sort out the inter-element spacing
        let width = p1.x - p0.x;
        let height = p2.y - p0.y;
        let xstep = width / (aref.cols as isize);
        let ystep = height / (aref.rows as isize);
        // Grab the reflection/ rotation settings
        // FIXME: these need *actual* support
        let mut reflect = false;
        let mut angle = None;
        if let Some(strans) = &aref.strans {
            // FIXME: interpretation of the "absolute" settings
            if strans.abs_mag || strans.abs_angle {
                return self.err("Unsupported GDSII Instance: Absolute");
            }
            if strans.mag.is_some() || strans.angle.is_some() {
                println!("Warning support for instance orientation in-progress");
            }
            angle = strans.angle;
            reflect = strans.reflected;
        }
        // Create the Instances
        let mut insts = Vec::with_capacity((aref.rows * aref.cols) as usize);
        for ix in 0..(aref.cols as isize) {
            let x = p0.x + ix * xstep;
            for iy in 0..(aref.rows as isize) {
                let y = p0.y + iy * ystep;
                insts.push(Instance {
                    inst_name: inst_name.clone(),
                    cell_name: cell_name.clone(),
                    cell: cell.clone(),
                    p0: Point::new(x, y),
                    reflect, // FIXME!
                    angle,   // FIXME!
                });
            }
        }
        self.ctx_stack.pop();
        Ok(insts)
    }
    /// Import a [Point]
    fn import_point(&mut self, pt: &gds21::GdsPoint) -> LayoutResult<Point> {
        let x = pt.x.try_into()?;
        let y = pt.y.try_into()?;
        Ok(Point::new(x, y))
    }
    /// Import a vector of [Point]s
    fn import_point_vec(&mut self, pts: &Vec<gds21::GdsPoint>) -> LayoutResult<Vec<Point>> {
        pts.iter()
            .map(|p| self.import_point(p))
            .collect::<Result<Vec<_>, _>>()
    }
    /// Get the ([LayerKey], [LayerPurpose]) pair for a GDS element implementing its [gds21::HasLayer] trait.
    /// Layers are created if they do not already exist,
    /// although this may eventually be a per-importer setting.
    fn import_element_layer(
        &mut self,
        elem: &impl gds21::HasLayer,
    ) -> LayoutResult<(LayerKey, LayerPurpose)> {
        let spec = elem.layerspec();
        self.layers.get_or_insert(spec.layer, spec.xtype)
    }
    /// Error creation helper
    fn err<T>(&self, msg: impl Into<String>) -> LayoutResult<T> {
        Err(LayoutError::Import {
            stack: self.ctx_stack.clone(),
            message: msg.into(),
        })
    }
}