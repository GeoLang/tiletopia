//! glTF/GLB mesh reader.

use crate::{IngestError, Material, MeshData, Texture, texture_image};
use std::path::Path;

/// The glTF default base colour, which says nothing a material has to carry.
const WHITE: [f32; 4] = [1.0; 4];

/// Read meshes from a glTF or GLB file. One primitive is one mesh, which is
/// also where glTF puts one material.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let (gltf, buffers, images) = gltf::import(path)
        .map_err(|e| IngestError::ParseError(format!("glTF import error: {e}")))?;

    let mut meshes = Vec::new();

    for mesh in gltf.meshes() {
        for primitive in mesh.primitives() {
            let reader = primitive.reader(|buffer| Some(&buffers[buffer.index()]));

            let positions: Vec<[f32; 3]> = reader
                .read_positions()
                .map(|iter| iter.collect())
                .unwrap_or_default();

            if positions.is_empty() {
                continue;
            }

            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|iter| iter.collect())
                .unwrap_or_default();

            let indices: Vec<u32> = reader
                .read_indices()
                .map(|iter| iter.into_u32().collect())
                .unwrap_or_default();

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
            let pixels = to_rgba8(data, &name)?;
            texture_image::from_rgba8(pixels, data.width, data.height, &name)
        }
    }
}

fn to_rgba8(data: &gltf::image::Data, name: &str) -> Option<Vec<u8>> {
    use gltf::image::Format;

    let channels = match data.format {
        Format::R8 => 1,
        Format::R8G8 => 2,
        Format::R8G8B8 => 3,
        Format::R8G8B8A8 => 4,
        other => {
            tracing::warn!("texture {name} is {other:?}, mesh stays untextured");
            return None;
        }
    };

    Some(
        data.pixels
            .chunks_exact(channels)
            .flat_map(|pixel| match channels {
                1 => [pixel[0], pixel[0], pixel[0], 255],
                2 => [pixel[0], pixel[0], pixel[0], pixel[1]],
                3 => [pixel[0], pixel[1], pixel[2], 255],
                _ => [pixel[0], pixel[1], pixel[2], pixel[3]],
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        tiletopia_core::glb_writer::write_glb_file(&written, &path).unwrap();

        assert!(read(&path).unwrap()[0].material.is_none());
    }
}
