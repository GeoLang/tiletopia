use draco_gltf::{ComponentType, MeshIndex, PrimitiveIndex, ValidationProfile};
use serde_json::Value;
use tiletopia_core::glb_writer::{
    GlbMesh, InstancedMesh, MeshCompression, MetadataProperty, MetadataValues, TextureData,
    TileMetadata, write_compressed_glb, write_glb,
};
use tiletopia_core::mesh_tiler::{MeshData, MeshTilingConfig, tile_meshes};

const GRID_QUADS_PER_SIDE: u32 = 16;
const UNREFERENCED_VERTEX_X: f32 = 1000.0;
const UNREFERENCED_VERTEX_COUNT: u32 = 5;
const ASSET_IDS: [&str; 2] = ["asset-a", "asset-b"];

#[derive(Clone, Copy, Debug)]
struct Corner {
    position: [f32; 3],
    normal: [f32; 3],
    texcoord: [f32; 2],
    color: [u8; 4],
    feature_id: u32,
}

#[derive(Clone, Copy)]
struct Tolerance {
    position: f32,
    normal: f32,
    texcoord: f32,
}

// these are half a 16 bit step over the grid and its unreferenced vertices, and one 8 bit octahedral step on a unit normal
const MESHOPT: Tolerance = Tolerance {
    position: (UNREFERENCED_VERTEX_X + UNREFERENCED_VERTEX_COUNT as f32) / 65535.0 / 2.0 * 1.01,
    normal: 0.02,
    texcoord: 0.0,
};

// these are one 14 bit step over 16 metres, one 12 bit step over a unit range and the octahedral error of 10 bits
const DRACO: Tolerance = Tolerance {
    position: 16.0 / 16383.0 * 1.01,
    normal: 0.005,
    texcoord: 1.0 / 4095.0 * 1.01,
};

fn close<const N: usize>(left: [f32; N], right: [f32; N], tolerance: f32) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| (left - right).abs() <= tolerance)
}

fn same_corner(left: Corner, right: Corner, tolerance: Tolerance) -> bool {
    close(left.position, right.position, tolerance.position)
        && close(left.normal, right.normal, tolerance.normal)
        && close(left.texcoord, right.texcoord, tolerance.texcoord)
        && left.color == right.color
        && left.feature_id == right.feature_id
}

fn assert_same_triangles(written: &[[Corner; 3]], decoded: &[[Corner; 3]], tolerance: Tolerance) {
    assert_eq!(decoded.len(), written.len());
    for triangle in written {
        // meshopt and draco rotate a triangle's corners without changing its winding
        let found = decoded.iter().any(|candidate| {
            (0..3).any(|rotation| {
                (0..3).all(|corner| {
                    same_corner(
                        triangle[corner],
                        candidate[(corner + rotation) % 3],
                        tolerance,
                    )
                })
            })
        });
        assert!(found, "{triangle:?} is not in the decoded tile");
    }
}

fn png_bytes() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn grid_corner(x: u32, z: u32) -> Corner {
    let (x, z) = (x as f32, z as f32);
    let slope: f32 = 0.1;
    let length = (1.0 + slope * slope).sqrt();
    Corner {
        position: [x, (x + z) * slope, z],
        normal: [-slope / length, 1.0 / length, 0.0],
        texcoord: [
            x / GRID_QUADS_PER_SIDE as f32,
            z / GRID_QUADS_PER_SIDE as f32,
        ],
        color: [x as u8 * 10, z as u8 * 10, 200, 255],
        feature_id: u32::from(x >= GRID_QUADS_PER_SIDE as f32 / 2.0),
    }
}

fn grid_mesh_with_unreferenced_vertices() -> GlbMesh {
    let side = GRID_QUADS_PER_SIDE + 1;
    let mut corners: Vec<Corner> = (0..side)
        .flat_map(|z| (0..side).map(move |x| grid_corner(x, z)))
        .collect();
    for extra in 0..UNREFERENCED_VERTEX_COUNT {
        corners.push(Corner {
            position: [UNREFERENCED_VERTEX_X + extra as f32, 0.0, 0.0],
            ..corners[0]
        });
    }

    let mut indices = Vec::new();
    for z in 0..GRID_QUADS_PER_SIDE {
        for x in 0..GRID_QUADS_PER_SIDE {
            let (near, far) = (z * side + x, (z + 1) * side + x);
            indices.extend_from_slice(&[near, far, near + 1, near + 1, far, far + 1]);
        }
    }

    GlbMesh {
        positions: corners.iter().map(|corner| corner.position).collect(),
        normals: Some(corners.iter().map(|corner| corner.normal).collect()),
        indices: Some(indices),
        colors: Some(corners.iter().map(|corner| corner.color).collect()),
        texcoords: Some(corners.iter().map(|corner| corner.texcoord).collect()),
        metadata: Some(TileMetadata {
            class_name: "asset".to_string(),
            properties: vec![MetadataProperty {
                name: "asset_id".to_string(),
                values: MetadataValues::String(ASSET_IDS.map(str::to_string).to_vec()),
            }],
        }),
        feature_ids: Some(corners.iter().map(|corner| corner.feature_id).collect()),
        texture: Some(TextureData {
            image_data: png_bytes(),
            mime_type: "image/png".to_string(),
            width: 4,
            height: 4,
        }),
        base_color_factor: None,
        node_translation: None,
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    }
}

fn written_triangles(mesh: &GlbMesh) -> Vec<[Corner; 3]> {
    let corner = |index: u32| {
        let index = index as usize;
        Corner {
            position: mesh.positions[index],
            normal: mesh.normals.as_ref().unwrap()[index],
            texcoord: mesh.texcoords.as_ref().unwrap()[index],
            color: mesh.colors.as_ref().unwrap()[index],
            feature_id: mesh.feature_ids.as_ref().unwrap()[index],
        }
    };
    mesh.indices
        .as_ref()
        .unwrap()
        .as_chunks::<3>()
        .0
        .iter()
        .map(|triangle| triangle.map(corner))
        .collect()
}

struct DecodedAttribute {
    semantic: String,
    component_type: u32,
    bytes: Vec<u8>,
}

struct DecodedPrimitive {
    attributes: Vec<DecodedAttribute>,
    indices: Vec<u32>,
    positions: Vec<[f64; 3]>,
}

impl DecodedPrimitive {
    fn attribute(&self, semantic: &str) -> &DecodedAttribute {
        self.attributes
            .iter()
            .find(|attribute| attribute.semantic == semantic)
            .unwrap_or_else(|| panic!("the tile decodes to no {semantic}"))
    }

    fn vertex_count(&self) -> usize {
        self.positions.len()
    }

    fn positions_f32(&self) -> Vec<[f32; 3]> {
        self.positions
            .iter()
            .map(|position| position.map(|component| component as f32))
            .collect()
    }
}

fn attribute_values<T: bytemuck::Pod>(geometry: &DecodedPrimitive, semantic: &str) -> Vec<T> {
    bytemuck::pod_collect_to_vec(&geometry.attribute(semantic).bytes)
}

fn decoded_normals(geometry: &DecodedPrimitive) -> Vec<[f32; 3]> {
    let attribute = geometry.attribute("NORMAL");
    // meshopt tiles hold normalized BYTE normals under KHR_mesh_quantization, draco tiles hold floats
    match attribute.component_type {
        code if code == ComponentType::F32 as u32 => bytemuck::pod_collect_to_vec(&attribute.bytes),
        code if code == ComponentType::I8 as u32 => attribute
            .bytes
            .as_chunks::<3>()
            .0
            .iter()
            .map(|normal| normal.map(|component| (component as i8 as f32 / 127.0).max(-1.0)))
            .collect(),
        other => panic!("normals decoded with component type {other}"),
    }
}

fn decoded_feature_ids(geometry: &DecodedPrimitive) -> Vec<u32> {
    let attribute = geometry.attribute("_FEATURE_ID_0");
    let bytes = &attribute.bytes;
    // draco stores feature ids as UNSIGNED_SHORT or FLOAT and meshopt keeps UNSIGNED_INT
    match attribute.component_type {
        code if code == ComponentType::U16 as u32 => bytemuck::pod_collect_to_vec::<u8, u16>(bytes)
            .into_iter()
            .map(u32::from)
            .collect(),
        code if code == ComponentType::U32 as u32 => bytemuck::pod_collect_to_vec(bytes),
        code if code == ComponentType::F32 as u32 => bytemuck::pod_collect_to_vec::<u8, f32>(bytes)
            .into_iter()
            .map(|id| id as u32)
            .collect(),
        other => panic!("feature ids decoded with component type {other}"),
    }
}

fn positions_in_node_parent_frame(
    document: &Value,
    mesh: usize,
    position: &DecodedAttribute,
) -> Vec<[f64; 3]> {
    let node = document["nodes"]
        .as_array()
        .expect("a nodes array")
        .iter()
        .find(|node| node["mesh"].as_u64() == Some(mesh as u64))
        .expect("a node draws the mesh");
    let translation = |axis: usize| node["translation"][axis].as_f64().unwrap_or(0.0);
    let scale = |axis: usize| node["scale"][axis].as_f64().unwrap_or(1.0);
    // meshopt tiles hold UNSIGNED_SHORT positions under KHR_mesh_quantization, draco and uncompressed tiles hold floats
    let stored: Vec<[f64; 3]> = match position.component_type {
        code if code == ComponentType::F32 as u32 => {
            bytemuck::pod_collect_to_vec::<u8, [f32; 3]>(&position.bytes)
                .into_iter()
                .map(|position| position.map(f64::from))
                .collect()
        }
        code if code == ComponentType::U16 as u32 => {
            bytemuck::pod_collect_to_vec::<u8, [u16; 3]>(&position.bytes)
                .into_iter()
                .map(|position| position.map(f64::from))
                .collect()
        }
        other => panic!("positions decoded with component type {other}"),
    };
    stored
        .into_iter()
        .map(|position| [0, 1, 2].map(|axis| translation(axis) + scale(axis) * position[axis]))
        .collect()
}

fn decoded_mesh_geometry(glb: &[u8], mesh: usize) -> DecodedPrimitive {
    let import =
        draco_gltf::parse(glb, ValidationProfile::Gltf20).expect("draco-gltf parses the tile");
    let (document, _) = glb_document(glb);
    let primitive = &document["meshes"][mesh]["primitives"][0];
    let (attributes, indices) = if primitive["extensions"]
        .get("KHR_draco_mesh_compression")
        .is_some()
    {
        let geometry = import
            .read_primitive(PrimitiveIndex::new(MeshIndex::from(mesh), 0))
            .expect("draco-gltf reads the primitive");
        let attributes: Vec<DecodedAttribute> = geometry
            .attributes()
            .iter()
            .map(|attribute| DecodedAttribute {
                semantic: attribute.semantic().to_string(),
                component_type: attribute.component_type() as u32,
                bytes: attribute.bytes().to_vec(),
            })
            .collect();
        let indices =
            bytemuck::pod_collect_to_vec(geometry.indices().expect("decoded indices").bytes());
        (attributes, indices)
    } else {
        // draco-gltf's read_primitive allows only float normals under Gltf20, even with KHR_mesh_quantization required
        let source = import.accessor_source();
        let read = |accessor: &Value| {
            source
                .read_accessor(accessor.as_u64().expect("an accessor index") as usize)
                .expect("draco-gltf reads the accessor")
        };
        let attributes: Vec<DecodedAttribute> = primitive["attributes"]
            .as_object()
            .expect("an attributes object")
            .iter()
            .map(|(semantic, accessor)| {
                let data = read(accessor);
                DecodedAttribute {
                    semantic: semantic.clone(),
                    component_type: data.component_type,
                    bytes: data.bytes,
                }
            })
            .collect();
        let index_data = read(&primitive["indices"]);
        let indices = match index_data.component_type {
            code if code == ComponentType::U32 as u32 => {
                bytemuck::pod_collect_to_vec(&index_data.bytes)
            }
            code if code == ComponentType::U16 as u32 => {
                bytemuck::pod_collect_to_vec::<u8, u16>(&index_data.bytes)
                    .into_iter()
                    .map(u32::from)
                    .collect()
            }
            other => panic!("indices decoded with component type {other}"),
        };
        (attributes, indices)
    };
    let position = attributes
        .iter()
        .find(|attribute| attribute.semantic == "POSITION")
        .expect("the tile decodes to no POSITION");
    let positions = positions_in_node_parent_frame(&document, mesh, position);
    DecodedPrimitive {
        attributes,
        indices,
        positions,
    }
}

fn decoded_geometry(glb: &[u8]) -> DecodedPrimitive {
    decoded_mesh_geometry(glb, 0)
}

fn decoded_triangles(glb: &[u8]) -> Vec<[Corner; 3]> {
    let geometry = decoded_geometry(glb);
    let positions = geometry.positions_f32();
    let normals = decoded_normals(&geometry);
    let texcoords: Vec<[f32; 2]> = attribute_values(&geometry, "TEXCOORD_0");
    let colors: Vec<[u8; 4]> = attribute_values(&geometry, "COLOR_0");
    let feature_ids = decoded_feature_ids(&geometry);
    geometry
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|triangle| {
            triangle.map(|index| {
                let index = index as usize;
                Corner {
                    position: positions[index],
                    normal: normals[index],
                    texcoord: texcoords[index],
                    color: colors[index],
                    feature_id: feature_ids[index],
                }
            })
        })
        .collect()
}

fn compressed(mesh: &GlbMesh, compression: MeshCompression) -> Vec<u8> {
    let mut glb = Vec::new();
    write_compressed_glb(mesh, compression, &mut glb).expect("the tile should encode");
    if let Ok(dir) = std::env::var("TILETOPIA_TILE_FIXTURE_DIR") {
        let name = serde_json::to_value(compression).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{}.glb", name.as_str().unwrap())),
            &glb,
        )
        .unwrap();
    }
    glb
}

fn glb_document(glb: &[u8]) -> (Value, Vec<u8>) {
    let glb = gltf::Glb::from_slice(glb).expect("a parseable GLB");
    (
        serde_json::from_slice(&glb.json).unwrap(),
        glb.bin.unwrap_or_default().into_owned(),
    )
}

fn view_range(view: &Value) -> std::ops::Range<usize> {
    let offset = view["byteOffset"].as_u64().unwrap() as usize;
    offset..offset + view["byteLength"].as_u64().unwrap() as usize
}

fn assert_texture_and_property_table_survive(document: &Value, bin: &[u8]) {
    let image_view = document["images"][0]["bufferView"].as_u64().unwrap() as usize;
    assert_eq!(
        bin[view_range(&document["bufferViews"][image_view])],
        png_bytes()
    );

    let property = &document["extensions"]["EXT_structural_metadata"]["propertyTables"][0]["properties"]
        ["asset_id"];
    let values = property["values"].as_u64().unwrap() as usize;
    assert_eq!(
        &bin[view_range(&document["bufferViews"][values])],
        ASSET_IDS.concat().as_bytes()
    );
    let offsets = property["stringOffsets"].as_u64().unwrap() as usize;
    let offsets: Vec<u32> =
        bytemuck::pod_collect_to_vec(&bin[view_range(&document["bufferViews"][offsets])]);
    assert_eq!(offsets, [0, 7, 14]);
}

fn assert_required(document: &Value, extension: &str) {
    for field in ["extensionsUsed", "extensionsRequired"] {
        let names = document[field].as_array().expect(field);
        assert!(
            names.iter().any(|name| name == extension),
            "{field}: {names:?}"
        );
    }
}

#[test]
fn meshopt_tile_decodes_within_normal_quantization_and_follows_the_fallback_rules() {
    let mesh = grid_mesh_with_unreferenced_vertices();
    let glb = compressed(&mesh, MeshCompression::Meshopt);
    let mut plain = Vec::new();
    write_glb(&mesh, &mut plain).unwrap();
    assert!(glb.len() < plain.len(), "{} >= {}", glb.len(), plain.len());

    assert_same_triangles(&written_triangles(&mesh), &decoded_triangles(&glb), MESHOPT);

    let (document, bin) = glb_document(&glb);
    assert_required(&document, "EXT_meshopt_compression");
    assert_texture_and_property_table_survive(&document, &bin);

    let fallback = &document["buffers"][1];
    assert_eq!(
        fallback["extensions"]["EXT_meshopt_compression"]["fallback"],
        true
    );
    assert!(fallback.get("uri").is_none());
    assert!(document["buffers"][0].get("uri").is_none());
    assert_eq!(
        document["buffers"][0]["byteLength"].as_u64().unwrap() as usize,
        bin.len()
    );

    let mut fallback_end = 0;
    let mut compressed_views = 0;
    for view in document["bufferViews"].as_array().unwrap() {
        let Some(extension) = view.pointer("/extensions/EXT_meshopt_compression") else {
            assert_eq!(
                view["buffer"], 0,
                "only compressed views use the fallback buffer"
            );
            continue;
        };
        compressed_views += 1;
        assert_eq!(view["buffer"], 1);
        assert_eq!(extension["buffer"], 0);
        assert_eq!(
            view["byteLength"].as_u64().unwrap(),
            extension["byteStride"].as_u64().unwrap() * extension["count"].as_u64().unwrap()
        );
        if let Some(stride) = view.get("byteStride") {
            assert_eq!(stride, &extension["byteStride"]);
        }
        fallback_end = fallback_end.max(view_range(view).end);

        let header = bin[extension["byteOffset"].as_u64().unwrap() as usize];
        if view["target"] == 34963 {
            assert_eq!(extension["mode"], "TRIANGLES");
            assert_eq!(header, 0xe1);
        } else {
            assert_eq!(extension["mode"], "ATTRIBUTES");
            assert_eq!(
                header, 0xa0,
                "EXT_meshopt_compression takes vertex codec version 0"
            );
        }
    }
    // positions, normals, indices, colors, texcoords, feature ids
    assert_eq!(compressed_views, 6);
    assert!(fallback["byteLength"].as_u64().unwrap() as usize >= fallback_end);
}

#[test]
fn meshopt_compresses_instance_attributes_and_they_decode_exactly() {
    let translations = vec![[0.0, 0.0, 0.0], [5.0, 0.0, 0.0], [10.0, 2.5, -1.0]];
    let rotations = vec![[0.0, 0.0, 0.0, 1.0]; 3];
    let mesh = GlbMesh {
        positions: Vec::new(),
        normals: None,
        indices: None,
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: None,
        texture: None,
        base_color_factor: None,
        node_translation: None,
        instanced_meshes: vec![InstancedMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: vec![0, 1, 2],
            node_translation: [0.0, 0.0, 0.0],
            instance_translations: translations.clone(),
            instance_rotations: Some(rotations.clone()),
            instance_feature_ids: Some(vec![0, 1, 2]),
            colors: None,
        }],
        edge_visibility: None,
    };
    let glb = compressed(&mesh, MeshCompression::Meshopt);
    let (document, bin) = glb_document(&glb);

    for accessor in document["accessors"].as_array().unwrap() {
        let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        assert!(
            view.pointer("/extensions/EXT_meshopt_compression")
                .is_some(),
            "{accessor}"
        );
    }

    let instancing = &document["nodes"][0]["extensions"]["EXT_mesh_gpu_instancing"]["attributes"];
    let decoded = |attribute: &str| {
        let accessor = &document["accessors"][instancing[attribute].as_u64().unwrap() as usize];
        let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
        let extension = &view["extensions"]["EXT_meshopt_compression"];
        assert_eq!(extension["mode"], "ATTRIBUTES");
        let offset = extension["byteOffset"].as_u64().unwrap() as usize;
        let length = extension["byteLength"].as_u64().unwrap() as usize;
        (
            bin[offset..offset + length].to_vec(),
            extension["count"].as_u64().unwrap() as usize,
        )
    };
    let (encoded, count) = decoded("TRANSLATION");
    let decoded_translations: Vec<[f32; 3]> =
        meshopt::decode_vertex_buffer(&encoded, count).unwrap();
    assert_eq!(decoded_translations, translations);
    let (encoded, count) = decoded("ROTATION");
    let decoded_rotations: Vec<[f32; 4]> = meshopt::decode_vertex_buffer(&encoded, count).unwrap();
    assert_eq!(decoded_rotations, rotations);
    let (encoded, count) = decoded("_FEATURE_ID_0");
    let decoded_feature_ids: Vec<f32> = meshopt::decode_vertex_buffer(&encoded, count).unwrap();
    assert_eq!(decoded_feature_ids, [0.0, 1.0, 2.0]);
}

#[test]
fn draco_tile_decodes_within_quantization_and_drops_unreferenced_vertices() {
    let mesh = grid_mesh_with_unreferenced_vertices();
    let glb = compressed(&mesh, MeshCompression::Draco);
    let mut plain = Vec::new();
    write_glb(&mesh, &mut plain).unwrap();
    assert!(glb.len() < plain.len(), "{} >= {}", glb.len(), plain.len());

    assert_same_triangles(&written_triangles(&mesh), &decoded_triangles(&glb), DRACO);

    let (document, bin) = glb_document(&glb);
    assert_required(&document, "KHR_draco_mesh_compression");
    assert_texture_and_property_table_survive(&document, &bin);

    let primitive = &document["meshes"][0]["primitives"][0];
    let geometry = decoded_geometry(&glb);
    let compressed_attributes = primitive["extensions"]["KHR_draco_mesh_compression"]["attributes"]
        .as_object()
        .unwrap();
    assert_eq!(
        compressed_attributes.len(),
        primitive["attributes"].as_object().unwrap().len()
    );
    assert!(primitive["extensions"]["EXT_mesh_features"].is_object());
    for (semantic, accessor) in primitive["attributes"].as_object().unwrap() {
        assert!(compressed_attributes.contains_key(semantic), "{semantic}");
        let accessor = &document["accessors"][accessor.as_u64().unwrap() as usize];
        assert!(accessor.get("bufferView").is_none(), "{semantic}");
        assert_eq!(
            accessor["count"].as_u64().unwrap() as usize,
            geometry.vertex_count()
        );
    }

    let indices_accessor = &document["accessors"][primitive["indices"].as_u64().unwrap() as usize];
    assert!(indices_accessor.get("bufferView").is_none());
    assert_eq!(
        indices_accessor["count"].as_u64().unwrap() as usize,
        geometry.indices.len()
    );

    let positions: Vec<[f32; 3]> = attribute_values(&geometry, "POSITION");
    let position_accessor =
        &document["accessors"][primitive["attributes"]["POSITION"].as_u64().unwrap() as usize];
    for axis in 0..3 {
        let lowest = positions.iter().map(|p| p[axis]).fold(f32::MAX, f32::min);
        let highest = positions.iter().map(|p| p[axis]).fold(f32::MIN, f32::max);
        assert!(
            (position_accessor["min"][axis].as_f64().unwrap() - f64::from(lowest)).abs() < 1e-3
        );
        assert!(
            (position_accessor["max"][axis].as_f64().unwrap() - f64::from(highest)).abs() < 1e-3
        );
    }
    let highest_x = positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
    assert!(highest_x < UNREFERENCED_VERTEX_X);

    // the texture, the two property table views and the Draco stream
    assert_eq!(document["bufferViews"].as_array().unwrap().len(), 4);
    assert!(
        document["bufferViews"]
            .as_array()
            .unwrap()
            .iter()
            .all(|view| view.get("target").is_none())
    );
    assert_eq!(document["buffers"].as_array().unwrap().len(), 1);
}

fn triangle_with_feature_id(feature_id: u32) -> GlbMesh {
    GlbMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: None,
        indices: Some(vec![0, 1, 2]),
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: Some(vec![feature_id; 3]),
        texture: None,
        base_color_factor: None,
        node_translation: None,
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    }
}

#[test]
fn draco_stores_feature_ids_in_the_smallest_type_that_keeps_them_exact() {
    for (feature_id, component_type) in [
        (65_535, ComponentType::U16),
        (65_536, ComponentType::F32),
        (1 << 24, ComponentType::F32),
    ] {
        let mesh = triangle_with_feature_id(feature_id);
        let mut glb = Vec::new();
        write_compressed_glb(&mesh, MeshCompression::Draco, &mut glb).unwrap();

        let geometry = decoded_geometry(&glb);
        assert_eq!(decoded_feature_ids(&geometry), [feature_id; 3]);

        let (document, _) = glb_document(&glb);
        let accessor = document["meshes"][0]["primitives"][0]["attributes"]["_FEATURE_ID_0"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(
            document["accessors"][accessor]["componentType"],
            component_type.to_gltf()
        );
    }

    let mut glb = Vec::new();
    let error = write_compressed_glb(
        &triangle_with_feature_id((1 << 24) + 1),
        MeshCompression::Draco,
        &mut glb,
    )
    .unwrap_err();
    assert!(error.to_string().contains("16777217"), "{error}");
}

#[test]
fn no_compression_writes_the_uncompressed_bytes() {
    let mesh = grid_mesh_with_unreferenced_vertices();
    let glb = compressed(&mesh, MeshCompression::None);
    let mut plain = Vec::new();
    write_glb(&mesh, &mut plain).unwrap();
    assert_eq!(glb, plain);

    let (document, _) = glb_document(&glb);
    assert!(document.get("extensionsRequired").is_none());
    assert_eq!(document["buffers"].as_array().unwrap().len(), 1);
}

#[test]
fn a_mesh_with_no_triangles_is_written_uncompressed() {
    let mesh = GlbMesh {
        positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
        normals: None,
        indices: Some(Vec::new()),
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: None,
        texture: None,
        base_color_factor: None,
        node_translation: None,
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    };
    let mut plain = Vec::new();
    write_glb(&mesh, &mut plain).unwrap();
    for compression in [MeshCompression::Meshopt, MeshCompression::Draco] {
        let mut glb = Vec::new();
        write_compressed_glb(&mesh, compression, &mut glb).unwrap();
        assert_eq!(glb, plain);
    }
}

#[test]
fn degenerate_triangles_do_not_stop_either_encoder() {
    let mesh = GlbMesh {
        positions: vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
        ],
        normals: None,
        indices: Some(vec![0, 1, 2, 1, 1, 2, 1, 3, 2]),
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: None,
        texture: None,
        base_color_factor: None,
        node_translation: None,
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    };

    let decoded = |compression| {
        let mut glb = Vec::new();
        write_compressed_glb(&mesh, compression, &mut glb).unwrap();
        let geometry = decoded_geometry(&glb);
        let positions = geometry.positions_f32();
        let triangles: Vec<[[f32; 3]; 3]> = geometry
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|triangle| triangle.map(|index| positions[index as usize]))
            .collect();
        triangles
    };

    let reaches = |triangles: &[[[f32; 3]; 3]], corner: [f32; 3]| {
        triangles
            .iter()
            .flatten()
            .any(|decoded| close(*decoded, corner, 1e-3))
    };
    for compression in [MeshCompression::Meshopt, MeshCompression::Draco] {
        let triangles = decoded(compression);
        assert!(triangles.len() >= 2, "{compression:?}: {triangles:?}");
        for corner in &mesh.positions {
            assert!(reaches(&triangles, *corner), "{compression:?}: {corner:?}");
        }
    }
}

#[test]
fn split_tiles_of_either_compression_cover_the_mesh_with_only_their_own_vertices() {
    let side = GRID_QUADS_PER_SIDE + 1;
    let corners: Vec<Corner> = (0..side)
        .flat_map(|z| (0..side).map(move |x| grid_corner(x, z)))
        .collect();
    let mut indices = Vec::new();
    for z in 0..GRID_QUADS_PER_SIDE {
        for x in 0..GRID_QUADS_PER_SIDE {
            let (near, far) = (z * side + x, (z + 1) * side + x);
            indices.extend_from_slice(&[near, far, near + 1, near + 1, far, far + 1]);
        }
    }
    let mesh = MeshData {
        positions: corners
            .iter()
            .map(|corner| corner.position.map(f64::from))
            .collect(),
        normals: corners.iter().map(|corner| corner.normal).collect(),
        texcoords: Vec::new(),
        indices,
        name: "grid".into(),
        asset_id: Some("asset-a".into()),
        class: None,
        storey: None,
        element_color: None,
        base_color_factor: None,
        texture: None,
    };

    for compression in [MeshCompression::Meshopt, MeshCompression::Draco] {
        let dir = std::env::temp_dir().join(format!(
            "tiletopia_compressed_tiles_{compression:?}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            mesh_compression: compression,
            ..Default::default()
        };
        tile_meshes(std::slice::from_ref(&mesh), &dir, &config).expect("tiling should succeed");

        let tiles: Vec<std::path::PathBuf> = std::fs::read_dir(dir.join("tiles"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert!(tiles.len() > 1, "the mesh should split");

        let mut tile_triangles = 0;
        for tile in &tiles {
            let glb = std::fs::read(tile).unwrap();
            let geometry = decoded_geometry(&glb);
            let indices = &geometry.indices;
            tile_triangles += indices.len() / 3;

            let mut referenced = indices.clone();
            referenced.sort_unstable();
            referenced.dedup();
            assert_eq!(geometry.vertex_count(), referenced.len(), "{tile:?}");
            assert!(
                geometry.vertex_count() < mesh.positions.len(),
                "{tile:?} carries the whole vertex array"
            );

            assert!(
                decoded_feature_ids(&geometry)
                    .iter()
                    .all(|feature_id| *feature_id == 0)
            );
        }
        assert_eq!(tile_triangles, mesh.indices.len() / 3, "{compression:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

const ALL_MESH_COMPRESSIONS: [MeshCompression; 3] = [
    MeshCompression::None,
    MeshCompression::Meshopt,
    MeshCompression::Draco,
];
const ELEMENT_GRID_QUADS_DEEP: u32 = 8;
const ELEMENT_A_LAST_X: u32 = 21;
const ELEMENT_B_LAST_X: u32 = 32;
const ONE_CENTIMETRE: f64 = 0.01;
const FAR_FROM_THE_ORIGIN: [f64; 3] = [500_000.125, 5_800_000.375, 123.0625];

fn element_grid(origin: [f64; 3], first_x: u32, last_x: u32, asset_id: &str) -> MeshData {
    let stride = last_x - first_x + 1;
    let positions: Vec<[f64; 3]> = (0..=ELEMENT_GRID_QUADS_DEEP)
        .flat_map(move |y| {
            (first_x..=last_x).map(move |x| {
                [
                    origin[0] + f64::from(x),
                    origin[1] + f64::from(y),
                    origin[2],
                ]
            })
        })
        .collect();
    let mut indices = Vec::new();
    for y in 0..ELEMENT_GRID_QUADS_DEEP {
        for x in 0..last_x - first_x {
            let near = y * stride + x;
            let far = near + stride;
            indices.extend_from_slice(&[near, near + 1, far, near + 1, far + 1, far]);
        }
    }
    MeshData {
        positions,
        normals: Vec::new(),
        texcoords: Vec::new(),
        indices,
        name: asset_id.to_string(),
        asset_id: Some(asset_id.to_string()),
        class: None,
        storey: None,
        element_color: None,
        base_color_factor: None,
        texture: None,
    }
}

fn two_touching_elements(origin: [f64; 3]) -> [MeshData; 2] {
    [
        element_grid(origin, 0, ELEMENT_A_LAST_X, "element-a"),
        element_grid(origin, ELEMENT_A_LAST_X, ELEMENT_B_LAST_X, "element-b"),
    ]
}

struct WrittenTiles {
    dir: std::path::PathBuf,
    tileset: Value,
}

impl Drop for WrittenTiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl WrittenTiles {
    fn every_tile(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.dir.join("tiles"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    }
}

fn write_tiles(meshes: &[MeshData], config: &MeshTilingConfig) -> WrittenTiles {
    let dir =
        std::env::temp_dir().join(format!("tiletopia_element_tiles_{}", uuid::Uuid::new_v4()));
    tile_meshes(meshes, &dir, config).expect("tiling should succeed");
    let tileset =
        serde_json::from_slice(&std::fs::read(dir.join("tileset.json")).unwrap()).unwrap();
    WrittenTiles { dir, tileset }
}

fn string_column(document: &Value, bin: &[u8], name: &str) -> Vec<String> {
    let property =
        &document["extensions"]["EXT_structural_metadata"]["propertyTables"][0]["properties"][name];
    let values_view = property["values"].as_u64().unwrap() as usize;
    let offsets_view = property["stringOffsets"].as_u64().unwrap() as usize;
    let values = &bin[view_range(&document["bufferViews"][values_view])];
    let offsets: Vec<u32> =
        bytemuck::pod_collect_to_vec(&bin[view_range(&document["bufferViews"][offsets_view])]);
    offsets
        .windows(2)
        .map(|pair| String::from_utf8(values[pair[0] as usize..pair[1] as usize].to_vec()).unwrap())
        .collect()
}

struct DecodedTile {
    document: Value,
    bin: Vec<u8>,
    local_positions: Vec<[f32; 3]>,
    node_translation: [f64; 3],
    feature_ids: Vec<u32>,
}

impl DecodedTile {
    fn read(path: &std::path::Path) -> Self {
        let glb = std::fs::read(path).unwrap();
        let geometry = decoded_geometry(&glb);
        let (document, bin) = glb_document(&glb);
        let translation = &document["nodes"][0]["translation"];
        let node_translation = [0usize, 1, 2].map(|axis| {
            translation[axis]
                .as_f64()
                .expect("a tile's node has a translation")
        });
        Self {
            local_positions: geometry
                .positions
                .iter()
                .map(|position| {
                    [0, 1, 2].map(|axis| (position[axis] - node_translation[axis]) as f32)
                })
                .collect(),
            node_translation,
            feature_ids: decoded_feature_ids(&geometry),
            document,
            bin,
        }
    }

    fn gltf_positions(&self) -> Vec<[f64; 3]> {
        self.local_positions
            .iter()
            .map(|local| [0, 1, 2].map(|axis| self.node_translation[axis] + f64::from(local[axis])))
            .collect()
    }

    fn column_names(&self) -> Vec<String> {
        let table = &self.document["extensions"]["EXT_structural_metadata"]["propertyTables"][0];
        let mut names: Vec<String> = table["properties"]
            .as_object()
            .expect("a property table")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    fn column(&self, name: &str) -> Vec<String> {
        string_column(&self.document, &self.bin, name)
    }

    fn assert_the_table_has_one_row_per_feature_id(&self) {
        let row_count = self.column("asset_id").len();
        let mut distinct = self.feature_ids.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct, (0..row_count as u32).collect::<Vec<u32>>());

        let table = &self.document["extensions"]["EXT_structural_metadata"]["propertyTables"][0];
        assert_eq!(table["count"], row_count);
        let feature_id = &self.document["meshes"][0]["primitives"][0]["extensions"]["EXT_mesh_features"]
            ["featureIds"][0];
        assert_eq!(feature_id["featureCount"], row_count);
        assert_eq!(feature_id["propertyTable"], 0);
    }

    fn assert_every_vertex_sits_on_the_element_its_row_names(&self) {
        let rows = self.column("asset_id");
        for (position, feature_id) in self.gltf_positions().iter().zip(&self.feature_ids) {
            let (low, high) = match rows[*feature_id as usize].as_str() {
                "element-a" => (0, ELEMENT_A_LAST_X),
                "element-b" => (ELEMENT_A_LAST_X, ELEMENT_B_LAST_X),
                other => panic!("no element is named {other}"),
            };
            assert!(
                position[0] > f64::from(low) - ONE_CENTIMETRE
                    && position[0] < f64::from(high) + ONE_CENTIMETRE,
                "{position:?} is not on {}",
                rows[*feature_id as usize]
            );
        }
    }
}

#[test]
fn each_tile_table_lists_only_the_elements_with_triangles_in_that_tile() {
    let meshes = two_touching_elements([0.0; 3]);
    for compression in ALL_MESH_COMPRESSIONS {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 150,
            mesh_compression: compression,
            ..Default::default()
        };
        let written = write_tiles(&meshes, &config);

        let tables: Vec<Vec<String>> = written
            .every_tile()
            .iter()
            .map(|path| {
                let tile = DecodedTile::read(path);
                tile.assert_the_table_has_one_row_per_feature_id();
                tile.assert_every_vertex_sits_on_the_element_its_row_names();
                tile.column("asset_id")
            })
            .collect();

        let tiles_naming = |element: &str| {
            tables
                .iter()
                .filter(|table| table.iter().any(|row| row == element))
                .count()
        };
        assert!(
            tiles_naming("element-a") > 1,
            "{compression:?}: element-a's fragments should carry its id in several tiles: {tables:?}"
        );
        assert!(tiles_naming("element-b") > 0, "{compression:?}: {tables:?}");
    }
}

#[test]
fn a_mesh_far_from_the_origin_decodes_within_a_centimetre_in_every_tile() {
    let meshes = two_touching_elements(FAR_FROM_THE_ORIGIN);
    let input: Vec<[f64; 3]> = meshes
        .iter()
        .flat_map(|mesh| mesh.positions.iter().copied())
        .collect();
    let distance_to_input = |position: [f64; 3]| {
        input
            .iter()
            .map(|vertex| {
                (0..3)
                    .map(|axis| (vertex[axis] - position[axis]).powi(2))
                    .sum::<f64>()
                    .sqrt()
            })
            .fold(f64::INFINITY, f64::min)
    };

    for content_y_up in [false, true] {
        for compression in ALL_MESH_COMPRESSIONS {
            let config = MeshTilingConfig {
                max_triangles_per_tile: 100,
                content_y_up,
                mesh_compression: compression,
                ..Default::default()
            };
            let written = write_tiles(&meshes, &config);
            let bounding_box: Vec<f64> = written.tileset["root"]["boundingVolume"]["box"]
                .as_array()
                .expect("a box")
                .iter()
                .map(|value| value.as_f64().unwrap())
                .collect();

            for path in written.every_tile() {
                let tile = DecodedTile::read(&path);
                for local in &tile.local_positions {
                    assert!(
                        local.iter().all(|coordinate| coordinate.abs() < 40.0),
                        "{path:?} holds a vertex at {local:?}, far from its node"
                    );
                }
                for position in tile.gltf_positions() {
                    // 3d tiles runtimes map glTF (x, y, z) to (x, -z, y)
                    let tile_frame = [position[0], -position[2], position[1]];
                    let source = if content_y_up { tile_frame } else { position };
                    let error = distance_to_input(source);
                    assert!(
                        error < ONE_CENTIMETRE,
                        "{compression:?} y up {content_y_up}: {source:?} is {error} m off"
                    );
                    for axis in 0..3usize {
                        let half_extent = bounding_box[3 + axis * 4];
                        assert!(
                            (tile_frame[axis] - bounding_box[axis]).abs()
                                < half_extent + ONE_CENTIMETRE,
                            "{tile_frame:?} is outside the root box {bounding_box:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn a_mesh_without_class_or_storey_has_no_such_columns() {
    let meshes = two_touching_elements([0.0; 3]);
    for compression in ALL_MESH_COMPRESSIONS {
        let config = MeshTilingConfig {
            mesh_compression: compression,
            ..Default::default()
        };
        let written = write_tiles(&meshes, &config);
        let tile = DecodedTile::read(&written.dir.join("tiles/root.glb"));

        assert_eq!(tile.column_names(), ["asset_id"]);
        let schema_columns = tile.document["extensions"]["EXT_structural_metadata"]["schema"]
            ["classes"]["asset"]["properties"]
            .as_object()
            .unwrap();
        assert!(!schema_columns.contains_key("class"));
        assert!(!schema_columns.contains_key("storey"));
    }
}

#[test]
fn class_and_storey_columns_line_up_with_asset_ids_in_a_tile_that_has_them() {
    let mut wall = element_grid([0.0; 3], 0, 4, "wall");
    wall.class = Some("IfcWall".to_string());
    wall.storey = Some("Level 1".to_string());
    let plain = element_grid([0.0; 3], 100, 104, "plain");
    let meshes = [wall, plain];

    for compression in ALL_MESH_COMPRESSIONS {
        let whole = MeshTilingConfig {
            mesh_compression: compression,
            ..Default::default()
        };
        let written = write_tiles(&meshes, &whole);
        let tile = DecodedTile::read(&written.dir.join("tiles/root.glb"));
        assert_eq!(tile.column_names(), ["asset_id", "class", "storey"]);
        assert_eq!(tile.column("asset_id"), ["wall", "plain"]);
        assert_eq!(tile.column("class"), ["IfcWall", ""]);
        assert_eq!(tile.column("storey"), ["Level 1", ""]);

        let split = MeshTilingConfig {
            max_triangles_per_tile: 64,
            mesh_compression: compression,
            ..Default::default()
        };
        let written = write_tiles(&meshes, &split);
        let tiles: Vec<DecodedTile> = written
            .every_tile()
            .iter()
            .map(|path| DecodedTile::read(path))
            .collect();
        assert_eq!(tiles.len(), 2, "{compression:?}: one tile per element");
        let tile_of = |asset_id: &str| {
            tiles
                .iter()
                .find(|tile| tile.column("asset_id") == [asset_id])
                .unwrap_or_else(|| panic!("{compression:?}: a tile holding only {asset_id}"))
        };
        let wall_tile = tile_of("wall");
        assert_eq!(wall_tile.column_names(), ["asset_id", "class", "storey"]);
        assert_eq!(wall_tile.column("class"), ["IfcWall"]);
        assert_eq!(tile_of("plain").column_names(), ["asset_id"]);
    }
}

#[test]
fn a_repeated_element_keeps_one_geometry_and_its_feature_ids_under_every_compression() {
    const DOOR_COUNT: usize = 6;
    const DOOR_SPACING: f64 = 20.0;
    const DOOR_SIDE: u32 = 8;
    let door_vertex_count = ((DOOR_SIDE + 1) * (DOOR_SIDE + 1)) as usize;
    let mut meshes: Vec<MeshData> = (0..DOOR_COUNT)
        .map(|number| {
            element_grid(
                [number as f64 * DOOR_SPACING, 0.0, 0.0],
                0,
                DOOR_SIDE,
                &format!("door-{number}"),
            )
        })
        .collect();
    meshes.push(element_grid([-100.0, 0.0, 0.0], 0, 31, "wall"));

    for compression in ALL_MESH_COMPRESSIONS {
        let config = MeshTilingConfig {
            mesh_compression: compression,
            ..Default::default()
        };
        let written = write_tiles(&meshes, &config);
        let glb = std::fs::read(written.dir.join("tiles/root.glb")).unwrap();
        let (document, bin) = glb_document(&glb);
        assert_required(&document, "EXT_mesh_gpu_instancing");

        let nodes = document["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 2, "{compression:?}");
        let node = &nodes[1];
        let instancing = &node["extensions"]["EXT_mesh_gpu_instancing"]["attributes"];
        let instance_values = |attribute: &str| -> Vec<f32> {
            let accessor = &document["accessors"][instancing[attribute].as_u64().unwrap() as usize];
            let view = &document["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
            assert!(view.get("target").is_none());
            let meshopt = view.pointer("/extensions/EXT_meshopt_compression");
            assert_eq!(
                meshopt.is_some(),
                compression == MeshCompression::Meshopt,
                "{compression:?} {attribute}"
            );
            let Some(extension) = meshopt else {
                assert_eq!(view["buffer"], 0);
                return bytemuck::pod_collect_to_vec(&bin[view_range(view)]);
            };
            assert_eq!(extension["mode"], "ATTRIBUTES");
            let offset = extension["byteOffset"].as_u64().unwrap() as usize;
            let length = extension["byteLength"].as_u64().unwrap() as usize;
            let count = extension["count"].as_u64().unwrap() as usize;
            let encoded = &bin[offset..offset + length];
            match extension["byteStride"].as_u64().unwrap() {
                4 => meshopt::decode_vertex_buffer::<f32>(encoded, count).unwrap(),
                12 => meshopt::decode_vertex_buffer::<[f32; 3]>(encoded, count)
                    .unwrap()
                    .concat(),
                other => panic!("no instance attribute has byte stride {other}"),
            }
        };
        let translations: Vec<[f32; 3]> =
            instance_values("TRANSLATION").as_chunks::<3>().0.to_vec();
        let feature_ids: Vec<f32> = instance_values("_FEATURE_ID_0");
        assert_eq!(translations.len(), DOOR_COUNT, "{compression:?}");
        assert_eq!(feature_ids.len(), DOOR_COUNT, "{compression:?}");

        let geometry = decoded_mesh_geometry(&glb, node["mesh"].as_u64().unwrap() as usize);
        assert_eq!(
            geometry.vertex_count(),
            door_vertex_count,
            "{compression:?}"
        );
        let locals: Vec<[f32; 3]> = attribute_values(&geometry, "POSITION");
        let node_translation: [f64; 3] =
            std::array::from_fn(|axis| node["translation"][axis].as_f64().unwrap());

        let rows = string_column(&document, &bin, "asset_id");
        assert_eq!(rows.len(), DOOR_COUNT + 1, "{compression:?}");
        let mut named_doors = Vec::new();
        for (instance, feature_id) in feature_ids.iter().enumerate() {
            let row = &rows[*feature_id as usize];
            let number: usize = row
                .strip_prefix("door-")
                .expect("an instance names a door")
                .parse()
                .unwrap();
            named_doors.push(number);
            for local in &locals {
                let x = node_translation[0]
                    + f64::from(translations[instance][0])
                    + f64::from(local[0]);
                let from_door = x - number as f64 * DOOR_SPACING;
                assert!(
                    from_door > -ONE_CENTIMETRE
                        && from_door < f64::from(DOOR_SIDE) + ONE_CENTIMETRE,
                    "{compression:?}: {row} has a vertex at x {x}"
                );
            }
        }
        named_doors.sort_unstable();
        assert_eq!(named_doors, (0..DOOR_COUNT).collect::<Vec<usize>>());

        let wall = decoded_geometry(&glb);
        let wall_rows: Vec<&String> = decoded_feature_ids(&wall)
            .iter()
            .map(|feature_id| &rows[*feature_id as usize])
            .collect();
        assert!(
            wall_rows.iter().all(|row| *row == "wall"),
            "{compression:?}"
        );
    }
}
