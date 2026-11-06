//! glTF/GLB mesh reader.

use crate::fbx_reader::Affine;
use crate::{IngestError, Material, MeshData, Texture, texture_image};
use gltf::mesh::Mode;
use std::path::Path;

/// The glTF default base colour, which says nothing a material has to carry.
const WHITE: [f32; 4] = [1.0; 4];

fn refuse_files_outside_the_mesh_directory(document: &gltf::Document) -> Result<(), IngestError> {
    let buffer_uris = document
        .buffers()
        .filter_map(|buffer| match buffer.source() {
            gltf::buffer::Source::Uri(uri) => Some(uri),
            gltf::buffer::Source::Bin => None,
        });
    let image_uris = document.images().filter_map(|image| match image.source() {
        gltf::image::Source::Uri { uri, .. } => Some(uri),
        gltf::image::Source::View { .. } => None,
    });
    for uri in buffer_uris.chain(image_uris) {
        if uri.starts_with("data:") {
            continue;
        }
        // gltf::import percent-decodes a relative uri before joining it to the directory
        let stays_inside = !uri.contains(':')
            && percent_encoding::percent_decode_str(uri)
                .decode_utf8()
                .is_ok_and(|decoded| texture_image::stays_inside_the_mesh_directory(&decoded));
        if !stays_inside {
            return Err(IngestError::ParseError(format!(
                "glTF names {uri}, which is outside the upload's directory"
            )));
        }
    }
    Ok(())
}

fn mesh_instances_in_default_scene(
    document: &gltf::Document,
) -> Result<Vec<(gltf::Mesh<'_>, Affine)>, IngestError> {
    // glTF leaves the scene to load up to the client when `scene` is unset
    let Some(scene) = document
        .default_scene()
        .or_else(|| document.scenes().next())
    else {
        if document.meshes().next().is_some() {
            return Err(IngestError::ParseError(
                "glTF has meshes but no scene to place them in".to_string(),
            ));
        }
        return Ok(Vec::new());
    };

    let mut visited = vec![false; document.nodes().len()];
    let mut pending: Vec<(gltf::Node<'_>, Affine)> =
        scene.nodes().map(|node| (node, Affine::IDENTITY)).collect();
    pending.reverse();
    let mut mesh_instances = Vec::new();
    while let Some((node, parent)) = pending.pop() {
        if std::mem::replace(&mut visited[node.index()], true) {
            return Err(IngestError::ParseError(format!(
                "glTF node {} has more than one parent or is its own ancestor",
                node.index()
            )));
        }
        // glTF node matrices are column-major
        let columns = node
            .transform()
            .matrix()
            .map(|column| column.map(f64::from));
        let world = parent.times(Affine {
            linear: std::array::from_fn(|row| std::array::from_fn(|column| columns[column][row])),
            translation: [columns[3][0], columns[3][1], columns[3][2]],
        });
        let children: Vec<_> = node.children().collect();
        pending.extend(children.into_iter().rev().map(|child| (child, world)));
        let Some(mesh) = node.mesh() else {
            continue;
        };
        let skinned = node.skin().is_some();
        let morphed = mesh
            .primitives()
            .any(|primitive| primitive.morph_targets().next().is_some());
        if skinned || morphed {
            tracing::warn!(
                "glTF mesh {:?}: skins and morph targets are not applied, it is read in its rest shape",
                mesh.name()
            );
        }
        // glTF skinned meshes ignore the transform of the node that holds them
        let placement = if skinned { Affine::IDENTITY } else { world };
        mesh_instances.push((mesh, placement));
    }
    Ok(mesh_instances)
}

fn checked_triangle_list(vertices: Vec<u32>) -> Result<Vec<u32>, IngestError> {
    if !vertices.len().is_multiple_of(3) {
        return Err(IngestError::ParseError(format!(
            "glTF triangle list has {} vertices, which is not a multiple of three",
            vertices.len()
        )));
    }
    Ok(vertices)
}

fn connected_triangle_count(topology: &str, vertices: &[u32]) -> Result<usize, IngestError> {
    if vertices.len() < 3 {
        return Err(IngestError::ParseError(format!(
            "glTF triangle {topology} has {} vertices, it needs at least three",
            vertices.len()
        )));
    }
    Ok(vertices.len() - 2)
}

fn strip_to_triangle_list(vertices: Vec<u32>) -> Result<Vec<u32>, IngestError> {
    let triangle_count = connected_triangle_count("strip", &vertices)?;
    Ok((0..triangle_count)
        .flat_map(|triangle| {
            let odd = triangle % 2;
            [
                vertices[triangle],
                vertices[triangle + 1 + odd],
                vertices[triangle + 2 - odd],
            ]
        })
        .collect())
}

fn fan_to_triangle_list(vertices: Vec<u32>) -> Result<Vec<u32>, IngestError> {
    let triangle_count = connected_triangle_count("fan", &vertices)?;
    Ok((0..triangle_count)
        .flat_map(|triangle| [vertices[triangle + 1], vertices[triangle + 2], vertices[0]])
        .collect())
}

pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let import_error = |e: gltf::Error| IngestError::ParseError(format!("glTF import error: {e}"));
    let gltf::Gltf { document, blob } = gltf::Gltf::open(path).map_err(import_error)?;
    refuse_files_outside_the_mesh_directory(&document)?;
    let directory = path.parent().unwrap_or_else(|| Path::new("./"));
    let buffers = gltf::import_buffers(&document, Some(directory), blob).map_err(import_error)?;
    let images = gltf::import_images(&document, Some(directory), &buffers).map_err(import_error)?;

    let mut meshes = Vec::new();

    for (mesh, world) in mesh_instances_in_default_scene(&document)? {
        for primitive in mesh.primitives() {
            let to_triangle_list: fn(Vec<u32>) -> Result<Vec<u32>, IngestError> = match primitive
                .mode()
            {
                Mode::Triangles => checked_triangle_list,
                Mode::TriangleStrip => strip_to_triangle_list,
                Mode::TriangleFan => fan_to_triangle_list,
                other => {
                    tracing::warn!(
                        "glTF mesh {:?}: {other:?} primitives are skipped, the tiler only writes triangles",
                        mesh.name()
                    );
                    continue;
                }
            };
            let reader = primitive.reader(|buffer| Some(&buffers[buffer.index()]));

            let positions: Vec<[f64; 3]> = reader
                .read_positions()
                .map(|iter| {
                    iter.map(|position| world.apply(position.map(f64::from)))
                        .collect()
                })
                .unwrap_or_default();

            if positions.is_empty() {
                continue;
            }

            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|iter| {
                    iter.map(|normal| {
                        world
                            .apply_normal(normal.map(f64::from))
                            .map(|coordinate| coordinate as f32)
                    })
                    .collect()
                })
                .unwrap_or_default();

            let vertex_order: Vec<u32> = reader
                .read_indices()
                .map(|iter| iter.into_u32().collect())
                .unwrap_or_else(|| (0..positions.len() as u32).collect());
            let mut indices = to_triangle_list(vertex_order)?;
            // glTF makes clockwise triangles front-facing under a negative determinant
            if world.is_mirroring() {
                for triangle in indices.as_chunks_mut::<3>().0 {
                    triangle.swap(1, 2);
                }
            }

            let pbr = primitive.material().pbr_metallic_roughness();
            let base_color = pbr.base_color_texture();
            let texcoord_set = base_color.as_ref().map_or(0, |info| info.tex_coord());
            let texcoords: Vec<[f32; 2]> = reader
                .read_tex_coords(texcoord_set)
                .map(|iter| iter.into_f32().collect())
                .unwrap_or_default();

            let texture =
                base_color.and_then(|info| read_texture(&info.texture(), &buffers, &images));
            let base_color_factor = pbr.base_color_factor();
            let has_material = texture.is_some() || base_color_factor != WHITE;
            let material = has_material.then_some(Material {
                base_color_factor,
                texture,
            });

            let name = mesh.name().unwrap_or("unnamed").to_string();

            meshes.push(MeshData {
                positions,
                normals,
                texcoords,
                indices,
                name,
                material,
                asset_id: None,
                class: None,
                storey: None,
                element_color: None,
            });
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

/// The base colour image, taken from the GLB buffer when it sits there and
/// from the decoded pixels when it came from a URI.
fn read_texture(
    texture: &gltf::Texture,
    buffers: &[gltf::buffer::Data],
    images: &[gltf::image::Data],
) -> Option<Texture> {
    let image = texture.source();
    let name = texture.name().unwrap_or("base colour").to_string();

    match image.source() {
        gltf::image::Source::View { view, .. } => {
            let buffer = buffers.get(view.buffer().index())?;
            let start = view.offset();
            let bytes = buffer.get(start..start + view.length())?.to_vec();
            texture_image::from_bytes(bytes, &name)
        }
        gltf::image::Source::Uri { .. } => {
            let data = images.get(image.index())?;
            let pixels = to_rgba8(data)?;
            texture_image::from_rgba8(pixels, data.width, data.height, &name)
        }
    }
}

fn to_rgba8(data: &gltf::image::Data) -> Option<Vec<u8>> {
    use gltf::image::Format;

    match data.format {
        Format::R8 => Some(expand_u8(&data.pixels, 1)),
        Format::R8G8 => Some(expand_u8(&data.pixels, 2)),
        Format::R8G8B8 => Some(expand_u8(&data.pixels, 3)),
        Format::R8G8B8A8 => Some(expand_u8(&data.pixels, 4)),
        Format::R16 => Some(expand_u16(&data.pixels, 1)),
        Format::R16G16 => Some(expand_u16(&data.pixels, 2)),
        Format::R16G16B16 => Some(expand_u16(&data.pixels, 3)),
        Format::R16G16B16A16 => Some(expand_u16(&data.pixels, 4)),
        Format::R32G32B32FLOAT => Some(expand_f32(&data.pixels, 3)),
        Format::R32G32B32A32FLOAT => Some(expand_f32(&data.pixels, 4)),
    }
}

fn expand_u8(pixels: &[u8], channels: usize) -> Vec<u8> {
    pixels
        .chunks_exact(channels)
        .flat_map(|pixel| match channels {
            1 => [pixel[0], pixel[0], pixel[0], 255],
            2 => [pixel[0], pixel[0], pixel[0], pixel[1]],
            3 => [pixel[0], pixel[1], pixel[2], 255],
            _ => [pixel[0], pixel[1], pixel[2], pixel[3]],
        })
        .collect()
}

fn u16_to_u8(bytes: &[u8]) -> u8 {
    let value = u16::from_le_bytes([bytes[0], bytes[1]]);
    (u32::from(value) * 255 / 65535) as u8
}

fn expand_u16(pixels: &[u8], channels: usize) -> Vec<u8> {
    pixels
        .chunks_exact(channels * 2)
        .flat_map(|pixel| {
            let channel = |i: usize| u16_to_u8(&pixel[i * 2..i * 2 + 2]);
            match channels {
                1 => {
                    let r = channel(0);
                    [r, r, r, 255]
                }
                2 => {
                    let r = channel(0);
                    [r, r, r, channel(1)]
                }
                3 => [channel(0), channel(1), channel(2), 255],
                _ => [channel(0), channel(1), channel(2), channel(3)],
            }
        })
        .collect()
}

fn f32_to_u8(bytes: &[u8]) -> u8 {
    let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn expand_f32(pixels: &[u8], channels: usize) -> Vec<u8> {
    pixels
        .chunks_exact(channels * 4)
        .flat_map(|pixel| {
            let channel = |i: usize| f32_to_u8(&pixel[i * 4..i * 4 + 4]);
            match channels {
                3 => [channel(0), channel(1), channel(2), 255],
                _ => [channel(0), channel(1), channel(2), channel(3)],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gltf_cannot_name_a_file_outside_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("secret.bin");
        std::fs::write(&outside, [0u8; 4]).unwrap();
        let mesh_dir = dir.path().join("asset");
        std::fs::create_dir_all(&mesh_dir).unwrap();
        std::fs::write(mesh_dir.join("beside.bin"), [0u8; 4]).unwrap();
        let path = mesh_dir.join("model.gltf");
        let with_buffer = |uri: &str| {
            serde_json::json!({
                "asset": { "version": "2.0" },
                "buffers": [{ "uri": uri, "byteLength": 4 }],
            })
            .to_string()
        };

        for uri in [
            "../secret.bin".to_string(),
            "%2e%2e/secret.bin".to_string(),
            format!("file://{}", outside.display()),
            outside.display().to_string(),
        ] {
            std::fs::write(&path, with_buffer(&uri)).unwrap();
            let error = read(&path).unwrap_err().to_string();
            assert!(error.contains("outside"), "{uri}: {error}");
        }

        std::fs::write(&path, with_buffer("beside.bin")).unwrap();
        assert!(read(&path).is_ok());
    }
    use tiletopia_core::glb_writer::{GlbMesh, TextureData};

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 8, 7, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    #[test]
    fn a_glb_texture_and_its_uvs_come_back_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("textured.glb");

        let written = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            metadata: None,
            feature_ids: None,
            texture: Some(TextureData {
                image_data: png_bytes(16, 8),
                mime_type: "image/png".to_string(),
                width: 16,
                height: 8,
            }),
            base_color_factor: None,
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        };
        tiletopia_core::glb_writer::write_glb_file(&written, &path).unwrap();

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 1);
        let mesh = &meshes[0];
        assert_eq!(mesh.positions.len(), 3);
        assert_eq!(mesh.texcoords, vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);

        let texture = mesh
            .material
            .as_ref()
            .and_then(|m| m.texture.as_ref())
            .expect("a texture");
        assert_eq!(texture.mime_type, "image/png");
        assert_eq!((texture.width, texture.height), (16, 8));
    }

    #[test]
    fn a_base_color_factor_survives_without_a_texture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("red.glb");

        let written = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: None,
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: Some([1.0, 0.0, 0.0, 1.0]),
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        };
        tiletopia_core::glb_writer::write_glb_file(&written, &path).unwrap();

        let material = read(&path).unwrap()[0]
            .material
            .clone()
            .expect("a material");
        assert_eq!(material.base_color_factor, [1.0, 0.0, 0.0, 1.0]);
        assert!(material.texture.is_none());
    }

    #[test]
    fn an_untextured_white_mesh_carries_no_material() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.glb");

        let written = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: None,
            indices: Some(vec![0, 1, 2]),
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
        tiletopia_core::glb_writer::write_glb_file(&written, &path).unwrap();

        assert!(read(&path).unwrap()[0].material.is_none());
    }

    const LINES: u32 = 1;
    const TRIANGLES: u32 = 4;
    const TRIANGLE_STRIP: u32 = 5;
    const TRIANGLE_FAN: u32 = 6;
    const TRIANGLE: [[f32; 3]; 3] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];

    fn read_gltf(
        positions: &[[f32; 3]],
        indices: Option<&[u16]>,
        mode: u32,
        scene_graph: serde_json::Value,
    ) -> Result<Vec<MeshData>, IngestError> {
        let dir = tempfile::tempdir().unwrap();
        let mut buffer = Vec::new();
        let normals = vec![[0.0f32, 0.0, 1.0]; positions.len()];
        for value in positions.iter().chain(&normals).flatten() {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        for index in indices.unwrap_or_default() {
            buffer.extend_from_slice(&index.to_le_bytes());
        }
        std::fs::write(dir.path().join("mesh.bin"), &buffer).unwrap();

        let vertex_bytes = positions.len() * 12;
        let bound = |pick: fn(f32, f32) -> f32| {
            [0, 1, 2].map(|axis| positions.iter().map(|position| position[axis]).reduce(pick))
        };
        let mut primitive = serde_json::json!({
            "attributes": { "POSITION": 0, "NORMAL": 1 },
            "mode": mode,
        });
        let mut document = serde_json::json!({
            "asset": { "version": "2.0" },
            "buffers": [{ "uri": "mesh.bin", "byteLength": buffer.len() }],
            "bufferViews": [
                { "buffer": 0, "byteOffset": 0, "byteLength": vertex_bytes },
                { "buffer": 0, "byteOffset": vertex_bytes, "byteLength": vertex_bytes },
            ],
            "accessors": [
                { "bufferView": 0, "componentType": 5126, "count": positions.len(), "type": "VEC3",
                  "min": bound(f32::min), "max": bound(f32::max) },
                { "bufferView": 1, "componentType": 5126, "count": positions.len(), "type": "VEC3" },
            ],
        });
        if let Some(indices) = indices {
            document["bufferViews"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(
                    { "buffer": 0, "byteOffset": 2 * vertex_bytes, "byteLength": indices.len() * 2 }
                ));
            document["accessors"].as_array_mut().unwrap().push(serde_json::json!(
                { "bufferView": 2, "componentType": 5123, "count": indices.len(), "type": "SCALAR" }
            ));
            primitive["indices"] = serde_json::json!(2);
        }
        document["meshes"] =
            serde_json::json!([{ "name": "test mesh", "primitives": [primitive] }]);
        let serde_json::Value::Object(scene_graph) = scene_graph else {
            panic!("the scene graph is a JSON object");
        };
        document.as_object_mut().unwrap().extend(scene_graph);
        let path = dir.path().join("mesh.gltf");
        std::fs::write(&path, document.to_string()).unwrap();
        read(&path)
    }

    fn read_triangle_with(scene_graph: serde_json::Value) -> Result<Vec<MeshData>, IngestError> {
        read_gltf(&TRIANGLE, Some(&[0, 1, 2]), TRIANGLES, scene_graph)
    }

    fn one_untransformed_node() -> serde_json::Value {
        serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0 }],
        })
    }

    fn assert_close(actual: [f64; 3], expected: [f64; 3]) {
        let near = actual
            .iter()
            .zip(expected)
            .all(|(value, wanted)| (value - wanted).abs() < 1e-5);
        assert!(near, "{actual:?} is not {expected:?}");
    }

    #[test]
    fn a_mesh_under_a_translated_node_lands_at_the_translation() {
        let meshes = read_triangle_with(serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0, "translation": [10.0, 20.0, 30.0] }],
        }))
        .unwrap();

        assert_eq!(meshes.len(), 1);
        assert_close(meshes[0].positions[0], [10.0, 20.0, 30.0]);
        assert_close(meshes[0].positions[1], [11.0, 20.0, 30.0]);
    }

    #[test]
    fn a_child_translation_applies_before_its_parent_rotation() {
        let half_angle_sine = std::f32::consts::FRAC_1_SQRT_2;
        let meshes = read_triangle_with(serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [
                { "rotation": [half_angle_sine, 0.0, 0.0, half_angle_sine], "children": [1] },
                { "mesh": 0, "translation": [0.0, 1.0, 0.0] },
            ],
        }))
        .unwrap();

        let mesh = &meshes[0];
        assert_close(mesh.positions[0], [0.0, 0.0, 1.0]);
        assert_close(mesh.positions[1], [1.0, 0.0, 1.0]);
        assert_close(mesh.normals[0].map(f64::from), [0.0, -1.0, 0.0]);
    }

    #[test]
    fn a_mesh_used_by_two_nodes_comes_out_twice() {
        let meshes = read_triangle_with(serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0, 1] }],
            "nodes": [
                { "mesh": 0, "translation": [5.0, 0.0, 0.0] },
                { "mesh": 0, "matrix": [
                    1.0, 0.0, 0.0, 0.0,
                    0.0, 1.0, 0.0, 0.0,
                    0.0, 0.0, 1.0, 0.0,
                    0.0, 0.0, -5.0, 1.0,
                ] },
            ],
        }))
        .unwrap();

        assert_eq!(meshes.len(), 2);
        assert_close(meshes[0].positions[0], [5.0, 0.0, 0.0]);
        assert_close(meshes[1].positions[0], [0.0, 0.0, -5.0]);
        assert_close(meshes[1].positions[1], [1.0, 0.0, -5.0]);
    }

    #[test]
    fn a_mirroring_scale_keeps_triangles_front_facing() {
        let meshes = read_triangle_with(serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "mesh": 0, "scale": [-1.0, 1.0, 1.0] }],
        }))
        .unwrap();

        let mesh = &meshes[0];
        assert_close(mesh.positions[1], [-1.0, 0.0, 0.0]);
        let corner = |slot: usize| mesh.positions[mesh.indices[slot] as usize];
        let (origin, second, third) = (corner(0), corner(1), corner(2));
        let edge = |to: [f64; 3]| [0, 1, 2].map(|axis| to[axis] - origin[axis]);
        let (first_edge, second_edge) = (edge(second), edge(third));
        let face_normal = [
            first_edge[1] * second_edge[2] - first_edge[2] * second_edge[1],
            first_edge[2] * second_edge[0] - first_edge[0] * second_edge[2],
            first_edge[0] * second_edge[1] - first_edge[1] * second_edge[0],
        ];
        assert_close(face_normal, [0.0, 0.0, 1.0]);
        assert_close(mesh.normals[0].map(f64::from), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_node_that_is_its_own_ancestor_is_refused() {
        let error = read_triangle_with(serde_json::json!({
            "scene": 0,
            "scenes": [{ "nodes": [0] }],
            "nodes": [{ "children": [1] }, { "mesh": 0, "children": [0] }],
        }))
        .unwrap_err()
        .to_string();

        assert!(error.contains("its own ancestor"), "{error}");
    }

    #[test]
    fn meshes_with_no_scene_are_refused() {
        let error = read_triangle_with(serde_json::json!({ "nodes": [{ "mesh": 0 }] }))
            .unwrap_err()
            .to_string();

        assert!(error.contains("no scene"), "{error}");
    }

    #[test]
    fn a_triangle_strip_becomes_a_list_with_one_winding() {
        let square: [[f32; 3]; 4] = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
        ];
        let meshes = read_gltf(&square, None, TRIANGLE_STRIP, one_untransformed_node()).unwrap();

        assert_eq!(meshes[0].indices, vec![0, 1, 2, 1, 3, 2]);
    }

    #[test]
    fn a_triangle_fan_becomes_a_list_around_its_first_index() {
        let square: [[f32; 3]; 4] = [
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        let meshes = read_gltf(
            &square,
            Some(&[3, 0, 1, 2]),
            TRIANGLE_FAN,
            one_untransformed_node(),
        )
        .unwrap();

        assert_eq!(meshes[0].indices, vec![0, 1, 3, 1, 2, 3]);
    }

    #[test]
    fn a_triangle_list_without_indices_takes_its_vertices_in_order() {
        let meshes = read_gltf(&TRIANGLE, None, TRIANGLES, one_untransformed_node()).unwrap();

        assert_eq!(meshes[0].indices, vec![0, 1, 2]);
    }

    #[test]
    fn a_strip_or_fan_under_three_vertices_is_refused() {
        let line: [[f32; 3]; 2] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        for mode in [TRIANGLE_STRIP, TRIANGLE_FAN] {
            let error = read_gltf(&line, None, mode, one_untransformed_node())
                .unwrap_err()
                .to_string();

            assert!(
                error.contains("has 2 vertices, it needs at least three"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_triangle_list_that_is_not_whole_triangles_is_refused() {
        let error = read_gltf(
            &TRIANGLE,
            Some(&[0, 1, 2, 0]),
            TRIANGLES,
            one_untransformed_node(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("not a multiple of three"), "{error}");
    }

    #[test]
    fn line_primitives_are_skipped() {
        let line: [[f32; 3]; 2] = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let meshes = read_gltf(&line, None, LINES, one_untransformed_node()).unwrap();

        assert!(meshes.is_empty());
    }

    #[test]
    fn sixteen_bit_pixels_become_rgba8() {
        let mut pixels = Vec::new();
        for value in [0u16, 65535, 32768] {
            pixels.extend_from_slice(&value.to_le_bytes());
        }
        let data = gltf::image::Data {
            format: gltf::image::Format::R16,
            width: 3,
            height: 1,
            pixels,
        };
        let rgba = to_rgba8(&data).expect("rgba");
        assert_eq!(rgba.len(), 12);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]);
        assert_eq!(rgba[8], 127);
    }

    #[test]
    fn float_pixels_become_rgba8() {
        let mut pixels = Vec::new();
        for value in [0.0f32, 1.0, 0.5] {
            pixels.extend_from_slice(&value.to_le_bytes());
        }
        pixels.extend_from_slice(&1.0f32.to_le_bytes());
        let data = gltf::image::Data {
            format: gltf::image::Format::R32G32B32A32FLOAT,
            width: 1,
            height: 1,
            pixels,
        };
        let rgba = to_rgba8(&data).expect("rgba");
        assert_eq!(rgba, vec![0, 255, 128, 255]);
    }
}
