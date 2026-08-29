//! FBX binary mesh reader.

use crate::{IngestError, Material, MeshData, Texture, texture_image};
use fbxcel_dom::any::AnyDocument;
use fbxcel_dom::v7400::Document;
use fbxcel_dom::v7400::data::mesh::layer::{TypedLayerElementHandle, normal::Normals, uv::Uv};
use fbxcel_dom::v7400::data::mesh::{TriangleVertexIndex, TriangleVertices};
use fbxcel_dom::v7400::object::material::MaterialHandle;
use fbxcel_dom::v7400::object::property::loaders::PrimitiveLoader;
use fbxcel_dom::v7400::object::{TypedObjectHandle, geometry::TypedGeometryHandle};
use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;

/// FBX positions are in `UnitScaleFactor` centimetres per file unit, 1 when absent.
const CENTIMETRES_PER_METRE: f64 = 100.0;

/// How the file's axes and units land in the y-up metre frame the tiler writes.
struct SourceFrame {
    x_from: usize,
    y_from: usize,
    z_from: usize,
    x_sign: f32,
    y_sign: f32,
    z_sign: f32,
    scale: f32,
}

impl SourceFrame {
    fn position(&self, v: [f32; 3]) -> [f32; 3] {
        [
            v[self.x_from] * self.x_sign * self.scale,
            v[self.y_from] * self.y_sign * self.scale,
            v[self.z_from] * self.z_sign * self.scale,
        ]
    }

    fn direction(&self, v: [f32; 3]) -> [f32; 3] {
        [
            v[self.x_from] * self.x_sign,
            v[self.y_from] * self.y_sign,
            v[self.z_from] * self.z_sign,
        ]
    }
}

/// Read meshes from an FBX binary file. Positions come out y-up whatever the
/// file's `GlobalSettings` `UpAxis` says, and a mesh painted with several
/// materials comes out as one mesh per material.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let doc = match AnyDocument::from_seekable_reader(reader)
        .map_err(|e| IngestError::ParseError(format!("FBX load error: {e}")))?
    {
        AnyDocument::V7400(_, doc) => doc,
        _ => return Err(IngestError::ParseError("unsupported FBX version".into())),
    };

    let frame = source_frame(&doc);
    let mesh_dir = path.parent().unwrap_or(Path::new("."));
    let mut meshes = Vec::new();

    for obj in doc.objects() {
        let TypedObjectHandle::Geometry(TypedGeometryHandle::Mesh(mesh_handle)) = obj.get_typed()
        else {
            continue;
        };

        let polygon_vertices = mesh_handle
            .polygon_vertices()
            .map_err(|e| IngestError::ParseError(format!("FBX polygon vertices: {e}")))?;
        let triangles = polygon_vertices
            .triangulate_each(|_, polygon, triangles| {
                for corner in 1..polygon.len().saturating_sub(1) {
                    triangles.push([polygon[0], polygon[corner], polygon[corner + 1]]);
                }
                Ok(())
            })
            .map_err(|e| IngestError::ParseError(format!("FBX triangulation: {e}")))?;
        if triangles.is_empty() {
            continue;
        }

        let mut uv = None;
        let mut normals_layer = None;
        let mut material_indices = None;
        for layer in mesh_handle.layers() {
            for entry in layer.layer_element_entries() {
                match entry.typed_layer_element() {
                    Ok(TypedLayerElementHandle::Uv(handle)) if uv.is_none() => {
                        uv = handle.uv().ok();
                    }
                    Ok(TypedLayerElementHandle::Normal(handle)) if normals_layer.is_none() => {
                        normals_layer = handle.normals().ok();
                    }
                    Ok(TypedLayerElementHandle::Material(handle)) if material_indices.is_none() => {
                        material_indices = handle.materials().ok();
                    }
                    _ => {}
                }
            }
        }

        let material_handles: Vec<MaterialHandle> = mesh_handle
            .models()
            .next()
            .map(|model| model.materials().collect())
            .unwrap_or_default();

        let name = mesh_handle.name().unwrap_or("mesh").to_string();
        let mut groups: HashMap<u32, MaterialGroup> = HashMap::new();

        for triangle_vertex in triangles.triangle_vertex_indices() {
            let Some(control_point) = triangles.control_point(triangle_vertex) else {
                continue;
            };
            let Some(control_point_index) = triangles.control_point_index(triangle_vertex) else {
                continue;
            };
            let position = frame.position([
                control_point.x as f32,
                control_point.y as f32,
                control_point.z as f32,
            ]);
            let texcoord = uv
                .as_ref()
                .map(|uv| read_uv(uv, &triangles, triangle_vertex));
            let normal = normals_layer.as_ref().and_then(|normals| {
                read_normal(normals, &triangles, triangle_vertex).map(|n| frame.direction(n))
            });
            let material_index = material_indices
                .as_ref()
                .and_then(|indices| indices.material_index(&triangles, triangle_vertex).ok())
                .map_or(0, |index| index.to_u32());

            groups.entry(material_index).or_default().push_vertex(
                control_point_index.to_u32(),
                position,
                texcoord,
                normal,
            );
        }

        let mut group_indices: Vec<u32> = groups.keys().copied().collect();
        group_indices.sort_unstable();
        let named_per_material = group_indices.len() > 1;

        for material_index in group_indices {
            let group = groups.remove(&material_index).expect("a listed group");
            let material = material_handles
                .get(material_index as usize)
                .map(|handle| read_material(handle, mesh_dir));
            let name = if named_per_material {
                format!("{name}.{material_index}")
            } else {
                name.clone()
            };
            meshes.push(group.into_mesh(name, material, normals_layer.is_some()));
        }
    }

    tracing::info!(
        "Read {} meshes from {} ({} total vertices)",
        meshes.len(),
        path.display(),
        meshes.iter().map(|m| m.positions.len()).sum::<usize>(),
    );

    Ok(meshes)
}

/// Triangle vertices of one material, with vertices shared between triangles
/// that agree on both control point and UV.
#[derive(Default)]
struct MaterialGroup {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    texcoords: Vec<[f32; 2]>,
    indices: Vec<u32>,
    /// Control point, UV and normal bits of a vertex already emitted, to its index.
    emitted: HashMap<(u32, u32, u32, u32, u32, u32), u32>,
}

impl MaterialGroup {
    fn push_vertex(
        &mut self,
        control_point_index: u32,
        position: [f32; 3],
        texcoord: Option<[f32; 2]>,
        normal: Option<[f32; 3]>,
    ) {
        let texcoord = texcoord.unwrap_or([0.0, 0.0]);
        let normal = normal.unwrap_or([0.0, 0.0, 0.0]);
        let key = (
            control_point_index,
            texcoord[0].to_bits(),
            texcoord[1].to_bits(),
            normal[0].to_bits(),
            normal[1].to_bits(),
            normal[2].to_bits(),
        );
        let index = match self.emitted.get(&key) {
            Some(&index) => index,
            None => {
                let index = self.positions.len() as u32;
                self.positions.push(position);
                self.normals.push(normal);
                self.texcoords.push(texcoord);
                self.emitted.insert(key, index);
                index
            }
        };
        self.indices.push(index);
    }

    fn into_mesh(
        mut self,
        name: String,
        material: Option<Material>,
        layer_normals: bool,
    ) -> MeshData {
        let textured = material
            .as_ref()
            .is_some_and(|material| material.texture.is_some());
        if !layer_normals {
            self.normals = face_normals(&self.positions, &self.indices);
        }
        MeshData {
            positions: self.positions,
            normals: self.normals,
            texcoords: if textured { self.texcoords } else { Vec::new() },
            indices: self.indices,
            name,
            material,
            asset_id: None,
        }
    }
}

fn face_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut normals = vec![[0.0; 3]; positions.len()];
    for &[ia, ib, ic] in indices.as_chunks::<3>().0 {
        let a = positions[ia as usize];
        let b = positions[ib as usize];
        let c = positions[ic as usize];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let cross = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for index in [ia, ib, ic] {
            let n = &mut normals[index as usize];
            n[0] += cross[0];
            n[1] += cross[1];
            n[2] += cross[2];
        }
    }
    for n in &mut normals {
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len > 0.0 {
            n[0] /= len;
            n[1] /= len;
            n[2] /= len;
        }
    }
    normals
}

fn read_normal(
    normals: &Normals<'_>,
    triangles: &TriangleVertices<'_>,
    index: TriangleVertexIndex,
) -> Option<[f32; 3]> {
    let n = normals.normal(triangles, index).ok()?;
    Some([n.x as f32, n.y as f32, n.z as f32])
}

/// The FBX v axis counts from the bottom of the image, glTF from the top.
fn read_uv(uv: &Uv<'_>, triangles: &TriangleVertices<'_>, index: TriangleVertexIndex) -> [f32; 2] {
    match uv.uv(triangles, index) {
        Ok(point) => [point.x as f32, 1.0 - point.y as f32],
        Err(_) => [0.0, 0.0],
    }
}

fn read_material(handle: &MaterialHandle<'_>, mesh_dir: &Path) -> Material {
    Material {
        base_color_factor: handle
            .properties()
            .diffuse_color_or_default()
            .map_or([1.0; 4], |c| [c.r as f32, c.g as f32, c.b as f32, 1.0]),
        texture: read_texture(handle, mesh_dir),
    }
}

/// The material's diffuse image, embedded in the FBX as a `Video` clip or
/// sitting beside it under the name the clip carries.
fn read_texture(handle: &MaterialHandle<'_>, mesh_dir: &Path) -> Option<Texture> {
    let clip = handle.diffuse_texture()?.video_clip()?;
    let name = clip.name().unwrap_or("diffuse").to_string();

    if let Some(content) = clip.content()
        && !content.is_empty()
    {
        return texture_image::from_bytes(content.to_vec(), &name);
    }

    let relative = clip.relative_filename().ok()?;
    texture_image::resolve(mesh_dir, relative).and_then(|path| texture_image::from_file(&path))
}

fn source_frame(doc: &Document) -> SourceFrame {
    let identity = SourceFrame {
        x_from: 0,
        y_from: 1,
        z_from: 2,
        x_sign: 1.0,
        y_sign: 1.0,
        z_sign: 1.0,
        scale: 1.0,
    };
    let Some(settings) = doc.global_settings() else {
        return identity;
    };
    let properties = settings.raw_properties();
    let read_int = |name: &str| {
        properties
            .get_property(name)
            .and_then(|property| property.load_value(PrimitiveLoader::<i32>::new()).ok())
    };
    let scale = (properties
        .get_property("UnitScaleFactor")
        .and_then(|property| property.load_value(PrimitiveLoader::<f64>::new()).ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0)
        / CENTIMETRES_PER_METRE) as f32;

    let Some(up_axis) = read_int("UpAxis") else {
        return SourceFrame { scale, ..identity };
    };
    let up_sign = read_int("UpAxisSign").unwrap_or(1);
    let front = read_int("FrontAxis").zip(read_int("FrontAxisSign"));
    let coord = read_int("CoordAxis").zip(read_int("CoordAxisSign"));

    let mut frame = if let (Some((front_axis, front_sign)), Some((coord_axis, coord_sign))) =
        (front, coord)
    {
        if ![up_axis, front_axis, coord_axis]
            .iter()
            .all(|axis| (0..=2).contains(axis))
            || up_axis == front_axis
            || up_axis == coord_axis
            || front_axis == coord_axis
        {
            tracing::warn!(
                "FBX axes {coord_axis}/{up_axis}/{front_axis} are not a permutation, reading as y up"
            );
            identity
        } else {
            SourceFrame {
                x_from: coord_axis as usize,
                y_from: up_axis as usize,
                z_from: front_axis as usize,
                x_sign: sign_f32(coord_sign),
                y_sign: sign_f32(up_sign),
                z_sign: sign_f32(front_sign),
                scale: 1.0,
            }
        }
    } else {
        match (up_axis, up_sign.signum()) {
            (1, 1) => identity,
            (1, -1) => SourceFrame {
                y_sign: -1.0,
                z_sign: -1.0,
                ..identity
            },
            (2, 1) => SourceFrame {
                y_from: 2,
                z_from: 1,
                z_sign: -1.0,
                ..identity
            },
            (2, -1) => SourceFrame {
                y_from: 2,
                y_sign: -1.0,
                z_from: 1,
                ..identity
            },
            (0, 1) => SourceFrame {
                x_from: 1,
                x_sign: -1.0,
                y_from: 0,
                ..identity
            },
            (0, -1) => SourceFrame {
                x_from: 1,
                y_from: 0,
                y_sign: -1.0,
                ..identity
            },
            _ => {
                tracing::warn!(
                    "FBX UpAxis {up_axis} sign {up_sign} is not an axis, reading as y up"
                );
                identity
            }
        }
    };
    frame.scale = scale;
    frame
}

fn sign_f32(value: i32) -> f32 {
    if value < 0 { -1.0 } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fbxcel::low::FbxVersion;
    use fbxcel::tree::v7400::{NodeId, Tree};
    use fbxcel::writer::v7400::binary::Writer;

    const GEOMETRY_ID: i64 = 100;
    const MODEL_ID: i64 = 200;
    const MATERIAL_ID: i64 = 300;
    const TEXTURE_ID: i64 = 400;
    const VIDEO_ID: i64 = 500;

    /// A unit square as two triangles, z up.
    const SQUARE_VERTICES: [f64; 12] = [
        0.0, 0.0, 0.0, // v0
        1.0, 0.0, 0.0, // v1
        1.0, 1.0, 2.0, // v2
        0.0, 1.0, 2.0, // v3
    ];
    /// Two triangles, each closed by a negated last index.
    const SQUARE_POLYGONS: [i32; 6] = [0, 1, -3, 0, 2, -4];
    const SQUARE_UVS: [f64; 8] = [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
    const SQUARE_UV_INDICES: [i32; 6] = [0, 1, 2, 0, 2, 3];

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([5, 6, 7, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    /// `name\0\u{1}Class`, the way FBX spells an object's name and class.
    fn name_class(name: &str, class: &str) -> String {
        format!("{name}\u{0}\u{1}{class}")
    }

    struct FbxBuilder {
        tree: Tree,
        properties: NodeId,
    }

    impl FbxBuilder {
        /// A file in metres, so position assertions read as written.
        fn new(up_axis: i32, up_axis_sign: i32) -> Self {
            Self::with_unit_scale(up_axis, up_axis_sign, Some(100.0))
        }

        fn with_unit_scale(up_axis: i32, up_axis_sign: i32, unit_scale: Option<f64>) -> Self {
            let mut tree = Tree::default();
            let root = tree.root().node_id();

            let settings = tree.append_new(root, "GlobalSettings");
            let properties = tree.append_new(settings, "Properties70");
            for (name, value) in [("UpAxis", up_axis), ("UpAxisSign", up_axis_sign)] {
                let property = tree.append_new(properties, "P");
                for attribute in [name, "int", "Integer", ""] {
                    tree.append_attribute(property, attribute.to_string());
                }
                tree.append_attribute(property, value);
            }

            tree.append_new(root, "Documents");
            tree.append_new(root, "Objects");
            tree.append_new(root, "Connections");
            let mut builder = Self { tree, properties };
            if let Some(unit_scale) = unit_scale {
                builder.global_double("UnitScaleFactor", unit_scale);
            }
            builder
        }

        fn global_int(&mut self, name: &str, value: i32) {
            let property = self.tree.append_new(self.properties, "P");
            for attribute in [name, "int", "Integer", ""] {
                self.tree.append_attribute(property, attribute.to_string());
            }
            self.tree.append_attribute(property, value);
        }

        fn global_double(&mut self, name: &str, value: f64) {
            let property = self.tree.append_new(self.properties, "P");
            for attribute in [name, "double", "Number", ""] {
                self.tree.append_attribute(property, attribute.to_string());
            }
            self.tree.append_attribute(property, value);
        }

        fn top_level(&self, name: &str) -> NodeId {
            self.tree
                .root()
                .children_by_name(name)
                .next()
                .unwrap_or_else(|| panic!("a {name} node"))
                .node_id()
        }

        fn object(
            &mut self,
            node_name: &str,
            id: i64,
            name: &str,
            class: &str,
            subclass: &str,
        ) -> NodeId {
            let objects = self.top_level("Objects");
            let node = self.tree.append_new(objects, node_name);
            self.tree.append_attribute(node, id);
            self.tree.append_attribute(node, name_class(name, class));
            self.tree.append_attribute(node, subclass.to_string());
            node
        }

        /// An object-to-object link, or an object-to-property one when a
        /// property label is given, the way a texture hangs off a material.
        fn connect(&mut self, source: i64, destination: i64, label: Option<&str>) {
            let connections = self.top_level("Connections");
            let connection = self.tree.append_new(connections, "C");
            let kind = if label.is_some() { "OP" } else { "OO" };
            self.tree.append_attribute(connection, kind.to_string());
            self.tree.append_attribute(connection, source);
            self.tree.append_attribute(connection, destination);
            if let Some(label) = label {
                self.tree.append_attribute(connection, label.to_string());
            }
        }

        /// A square geometry, its model, and the UV layer when asked for.
        fn square(&mut self, with_uvs: bool) {
            let geometry = self.object("Geometry", GEOMETRY_ID, "square", "Geometry", "Mesh");

            let vertices = self.tree.append_new(geometry, "Vertices");
            self.tree
                .append_attribute(vertices, SQUARE_VERTICES.to_vec());
            let polygons = self.tree.append_new(geometry, "PolygonVertexIndex");
            self.tree
                .append_attribute(polygons, SQUARE_POLYGONS.to_vec());

            if with_uvs {
                let element = self.tree.append_new(geometry, "LayerElementUV");
                self.tree.append_attribute(element, 0i32);
                let mapping = self.tree.append_new(element, "MappingInformationType");
                self.tree
                    .append_attribute(mapping, "ByPolygonVertex".to_string());
                let reference = self.tree.append_new(element, "ReferenceInformationType");
                self.tree
                    .append_attribute(reference, "IndexToDirect".to_string());
                let uv = self.tree.append_new(element, "UV");
                self.tree.append_attribute(uv, SQUARE_UVS.to_vec());
                let uv_index = self.tree.append_new(element, "UVIndex");
                self.tree
                    .append_attribute(uv_index, SQUARE_UV_INDICES.to_vec());

                let layer = self.tree.append_new(geometry, "Layer");
                self.tree.append_attribute(layer, 0i32);
                let entry = self.tree.append_new(layer, "LayerElement");
                let entry_type = self.tree.append_new(entry, "Type");
                self.tree
                    .append_attribute(entry_type, "LayerElementUV".to_string());
                let entry_index = self.tree.append_new(entry, "TypedIndex");
                self.tree.append_attribute(entry_index, 0i32);
            }

            self.object("Model", MODEL_ID, "square", "Model", "Mesh");
            self.connect(GEOMETRY_ID, MODEL_ID, None);
        }

        /// A material with a diffuse colour, and an embedded texture when given.
        fn material(&mut self, diffuse: [f64; 3], embedded: Option<Vec<u8>>) {
            let material = self.object("Material", MATERIAL_ID, "paint", "Material", "");
            let properties = self.tree.append_new(material, "Properties70");
            let color = self.tree.append_new(properties, "P");
            for attribute in ["DiffuseColor", "Color", "", "A"] {
                self.tree.append_attribute(color, attribute.to_string());
            }
            for channel in diffuse {
                self.tree.append_attribute(color, channel);
            }
            self.connect(MATERIAL_ID, MODEL_ID, None);

            let Some(image) = embedded else {
                return;
            };
            self.object("Texture", TEXTURE_ID, "diffuse", "Texture", "");
            self.connect(TEXTURE_ID, MATERIAL_ID, Some("DiffuseColor"));

            let video = self.object("Video", VIDEO_ID, "diffuse", "Video", "Clip");
            let content = self.tree.append_new(video, "Content");
            self.tree.append_attribute(content, image);
            let relative = self.tree.append_new(video, "RelativeFilename");
            self.tree
                .append_attribute(relative, "textures/diffuse.png".to_string());
            self.connect(VIDEO_ID, TEXTURE_ID, None);
        }

        fn write(self, path: &Path) {
            let file = std::fs::File::create(path).unwrap();
            let mut writer = Writer::new(file, FbxVersion::V7_4).unwrap();
            writer.write_tree(&self.tree).unwrap();
            writer.finalize_and_flush(&Default::default()).unwrap();
        }
    }

    #[test]
    fn test_read_nonexistent_file() {
        let result = read(Path::new("/tmp/nonexistent_fbx_file.fbx"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.fbx");
        std::fs::write(&path, b"").unwrap();

        let result = read(&path);
        assert!(result.is_err());
    }

    #[test]
    fn polygons_are_fan_triangulated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("square.fbx");
        let mut builder = FbxBuilder::new(1, 1);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].indices.len(), 6);
        assert_eq!(meshes[0].positions.len(), 4);
        assert_eq!(meshes[0].normals.len(), 4);
        let n = meshes[0].normals[0];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "{n:?}");
    }

    #[test]
    fn a_centimetre_file_comes_out_in_metres() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("centimetres.fbx");
        let mut builder = FbxBuilder::with_unit_scale(1, 1, Some(1.0));
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert!(
            meshes[0].positions.contains(&[0.01, 0.01, 0.02]),
            "{:?}",
            meshes[0].positions
        );
    }

    #[test]
    fn a_missing_unit_scale_factor_reads_as_centimetres() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unitless.fbx");
        let mut builder = FbxBuilder::with_unit_scale(1, 1, None);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert!(
            meshes[0].positions.contains(&[0.01, 0.01, 0.02]),
            "{:?}",
            meshes[0].positions
        );
    }

    #[test]
    fn front_and_coord_axes_turn_z_up_the_same_as_up_axis_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max.fbx");
        let mut builder = FbxBuilder::new(2, 1);
        builder.global_int("FrontAxis", 1);
        builder.global_int("FrontAxisSign", -1);
        builder.global_int("CoordAxis", 0);
        builder.global_int("CoordAxisSign", 1);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert!(
            meshes[0].positions.contains(&[1.0, 2.0, -1.0]),
            "{:?}",
            meshes[0].positions
        );
    }

    #[test]
    fn a_z_up_file_comes_out_y_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("z_up.fbx");
        let mut builder = FbxBuilder::new(2, 1);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        // the source vertex (1, 1, 2) is 2 up, so y carries the 2
        let up_most = meshes[0]
            .positions
            .iter()
            .map(|p| p[1])
            .fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(up_most, 2.0);
        assert!(
            meshes[0].positions.contains(&[1.0, 2.0, -1.0]),
            "{:?}",
            meshes[0].positions
        );
    }

    #[test]
    fn a_y_up_file_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("y_up.fbx");
        let mut builder = FbxBuilder::new(1, 1);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert!(meshes[0].positions.contains(&[1.0, 1.0, 2.0]));
    }

    #[test]
    fn an_x_up_file_comes_out_y_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x_up.fbx");
        let mut builder = FbxBuilder::new(0, 1);
        builder.square(false);
        builder.write(&path);

        let meshes = read(&path).unwrap();
        // x up sends (1, 0, 0) to (0, 1, 0)
        assert!(
            meshes[0].positions.contains(&[0.0, 1.0, 0.0]),
            "{:?}",
            meshes[0].positions
        );
    }

    #[test]
    fn an_embedded_texture_and_its_uvs_come_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("textured.fbx");
        let mut builder = FbxBuilder::new(1, 1);
        builder.square(true);
        builder.material([0.5, 0.25, 0.125], Some(png_bytes(16, 8)));
        builder.write(&path);

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 1);
        let mesh = &meshes[0];
        assert_eq!(mesh.texcoords.len(), mesh.positions.len());
        // FBX counts v from the bottom of the image, so the file's 0 flips to 1
        assert!(mesh.texcoords.contains(&[0.0, 1.0]), "{:?}", mesh.texcoords);

        let material = mesh.material.as_ref().expect("a material");
        assert_eq!(material.base_color_factor, [0.5, 0.25, 0.125, 1.0]);
        let texture = material.texture.as_ref().expect("a texture");
        assert_eq!(texture.mime_type, "image/png");
        assert_eq!((texture.width, texture.height), (16, 8));
    }

    #[test]
    fn a_texture_only_named_by_the_clip_is_loaded_from_beside_the_fbx() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("textures")).unwrap();
        std::fs::write(dir.path().join("textures/diffuse.png"), png_bytes(4, 4)).unwrap();

        let path = dir.path().join("linked.fbx");
        let mut builder = FbxBuilder::new(1, 1);
        builder.square(true);
        builder.material([1.0, 1.0, 1.0], Some(Vec::new()));
        builder.write(&path);

        let texture = read(&path).unwrap()[0]
            .material
            .as_ref()
            .and_then(|m| m.texture.clone())
            .expect("a texture");
        assert_eq!((texture.width, texture.height), (4, 4));
    }

    #[test]
    fn a_material_without_a_texture_keeps_its_diffuse_colour() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("painted.fbx");
        let mut builder = FbxBuilder::new(1, 1);
        builder.square(false);
        builder.material([1.0, 0.0, 0.0], None);
        builder.write(&path);

        let material = read(&path).unwrap()[0]
            .material
            .clone()
            .expect("a material");
        assert_eq!(material.base_color_factor, [1.0, 0.0, 0.0, 1.0]);
        assert!(material.texture.is_none());
    }
}
