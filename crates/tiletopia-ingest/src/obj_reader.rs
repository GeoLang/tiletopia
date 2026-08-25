//! OBJ mesh reader.

use crate::{IngestError, Material, MeshData, Texture, texture_image};
use std::collections::HashMap;
use std::path::Path;

/// Read meshes from a Wavefront OBJ file. tobj starts a new model at every
/// `usemtl`, so a model already carries at most one material.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let (models, load_materials) = tobj::load_obj(
        path,
        &tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        },
    )
    .map_err(|e| IngestError::ParseError(format!("OBJ load error: {e}")))?;

    let materials = load_materials.unwrap_or_else(|e| {
        tracing::warn!(
            "{}: materials not read, meshes stay untextured: {e}",
            path.display()
        );
        Vec::new()
    });

    let mesh_dir = path.parent().unwrap_or(Path::new("."));
    let mut textures = TextureCache::default();
    let mut meshes = Vec::with_capacity(models.len());

    for model in models {
        let mesh = &model.mesh;
        let num_verts = mesh.positions.len() / 3;
        if num_verts == 0 {
            continue;
        }

        let positions: Vec<[f32; 3]> = (0..num_verts)
            .map(|i| {
                [
                    mesh.positions[i * 3],
                    mesh.positions[i * 3 + 1],
                    mesh.positions[i * 3 + 2],
                ]
            })
            .collect();

        let normals: Vec<[f32; 3]> = if mesh.normals.len() == num_verts * 3 {
            (0..num_verts)
                .map(|i| {
                    [
                        mesh.normals[i * 3],
                        mesh.normals[i * 3 + 1],
                        mesh.normals[i * 3 + 2],
                    ]
                })
                .collect()
        } else {
            Vec::new()
        };

        // OBJ counts v from the bottom of the image, glTF from the top
        let texcoords: Vec<[f32; 2]> = if mesh.texcoords.len() == num_verts * 2 {
            (0..num_verts)
                .map(|i| [mesh.texcoords[i * 2], 1.0 - mesh.texcoords[i * 2 + 1]])
                .collect()
        } else {
            Vec::new()
        };

        let material = mesh
            .material_id
            .and_then(|id| materials.get(id))
            .map(|material| Material {
                base_color_factor: material
                    .diffuse
                    .map_or([1.0; 4], |[r, g, b]| [r, g, b, 1.0]),
                texture: material
                    .diffuse_texture
                    .as_ref()
                    .and_then(|name| textures.load(mesh_dir, name)),
            });

        meshes.push(MeshData {
            positions,
            normals,
            texcoords,
            indices: mesh.indices.clone(),
            name: model.name,
            material,
            asset_id: None,
        });
    }

    tracing::info!(
        "Read {} meshes from {} ({} total vertices)",
        meshes.len(),
        path.display(),
        meshes.iter().map(|m| m.positions.len()).sum::<usize>(),
    );

    Ok(meshes)
}

/// Loads each `.mtl` texture once however many materials name it.
#[derive(Default)]
struct TextureCache {
    by_name: HashMap<String, Option<Texture>>,
}

impl TextureCache {
    fn load(&mut self, mesh_dir: &Path, name: &str) -> Option<Texture> {
        self.by_name
            .entry(name.to_string())
            .or_insert_with(|| {
                texture_image::resolve(mesh_dir, name)
                    .and_then(|path| texture_image::from_file(&path))
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUAD: &str = "\
v 0.0 0.0 0.0
v 1.0 0.0 0.0
v 0.0 1.0 0.0
v 1.0 1.0 0.0
vt 0.0 0.0
vt 1.0 0.0
vt 0.0 1.0
vt 1.0 1.0
f 1/1 2/2 3/3
f 2/2 4/4 3/3
";

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 100, 50, 255]));
        image.save(path).unwrap();
    }

    #[test]
    fn test_read_minimal_obj() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.obj");

        let obj_content = "\
# minimal OBJ
v 0.0 0.0 0.0
v 1.0 0.0 0.0
v 0.0 1.0 0.0
v 1.0 1.0 0.0
f 1 2 3
f 2 4 3
";
        std::fs::write(&path, obj_content).unwrap();

        let meshes = read(&path).unwrap();
        assert!(!meshes.is_empty());
        let m = &meshes[0];
        assert_eq!(m.positions.len(), 4);
        assert_eq!(m.indices.len(), 6);
        assert!(m.texcoords.is_empty());
        assert!(m.material.is_none());
    }

    #[test]
    fn test_read_nonexistent_obj() {
        let result = read(Path::new("/tmp/nonexistent_obj_file.obj"));
        assert!(result.is_err());
    }

    #[test]
    fn a_diffuse_texture_and_its_uvs_come_through() {
        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("wall.png"), 8, 4);
        std::fs::write(
            dir.path().join("test.mtl"),
            "newmtl wall\nKd 0.25 0.5 0.75\nmap_Kd wall.png\n",
        )
        .unwrap();
        let path = dir.path().join("test.obj");
        std::fs::write(&path, format!("mtllib test.mtl\nusemtl wall\n{QUAD}")).unwrap();

        let meshes = read(&path).unwrap();
        let mesh = &meshes[0];
        assert_eq!(mesh.texcoords.len(), mesh.positions.len());
        // the OBJ's v = 0 is the bottom of the image, so it flips to 1
        assert_eq!(mesh.texcoords[0], [0.0, 1.0]);
        assert_eq!(mesh.texcoords[3], [1.0, 0.0]);

        let material = mesh.material.as_ref().expect("a material");
        assert_eq!(material.base_color_factor, [0.25, 0.5, 0.75, 1.0]);
        let texture = material.texture.as_ref().expect("a texture");
        assert_eq!(texture.mime_type, "image/png");
        assert_eq!((texture.width, texture.height), (8, 4));
    }

    #[test]
    fn two_materials_split_into_two_meshes_with_their_own_textures() {
        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("wall.png"), 8, 4);
        write_png(&dir.path().join("roof.png"), 2, 2);
        std::fs::write(
            dir.path().join("test.mtl"),
            "newmtl wall\nmap_Kd wall.png\nnewmtl roof\nmap_Kd roof.png\n",
        )
        .unwrap();
        let path = dir.path().join("test.obj");
        std::fs::write(
            &path,
            "mtllib test.mtl\n\
v 0.0 0.0 0.0\nv 1.0 0.0 0.0\nv 0.0 1.0 0.0\nv 1.0 1.0 0.0\n\
vt 0.0 0.0\nvt 1.0 0.0\nvt 0.0 1.0\nvt 1.0 1.0\n\
usemtl wall\nf 1/1 2/2 3/3\n\
usemtl roof\nf 2/2 4/4 3/3\n",
        )
        .unwrap();

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 2);
        let sizes: Vec<(u32, u32)> = meshes
            .iter()
            .map(|mesh| {
                let texture = mesh
                    .material
                    .as_ref()
                    .and_then(|m| m.texture.as_ref())
                    .expect("a texture per mesh");
                (texture.width, texture.height)
            })
            .collect();
        assert!(
            sizes.contains(&(8, 4)) && sizes.contains(&(2, 2)),
            "{sizes:?}"
        );
    }

    #[test]
    fn a_missing_texture_file_leaves_the_mesh_untextured() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.mtl"),
            "newmtl wall\nKd 1.0 0.0 0.0\nmap_Kd gone.png\n",
        )
        .unwrap();
        let path = dir.path().join("test.obj");
        std::fs::write(&path, format!("mtllib test.mtl\nusemtl wall\n{QUAD}")).unwrap();

        let meshes = read(&path).unwrap();
        let material = meshes[0].material.as_ref().expect("a material");
        assert!(material.texture.is_none());
        assert_eq!(material.base_color_factor, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn a_missing_mtl_file_still_reads_the_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.obj");
        std::fs::write(&path, format!("mtllib gone.mtl\nusemtl wall\n{QUAD}")).unwrap();

        let meshes = read(&path).unwrap();
        assert_eq!(meshes[0].positions.len(), 4);
        assert!(meshes[0].material.is_none());
    }
}
