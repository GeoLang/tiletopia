//! OBJ mesh reader.

use crate::{IngestError, MeshData};
use std::path::Path;

/// Read meshes from a Wavefront OBJ file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let (models, _materials) = tobj::load_obj(
        path,
        &tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        },
    )
    .map_err(|e| IngestError::ParseError(format!("OBJ load error: {e}")))?;

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
                    mesh.positions[i * 3] as f32,
                    mesh.positions[i * 3 + 1] as f32,
                    mesh.positions[i * 3 + 2] as f32,
                ]
            })
            .collect();

        let normals: Vec<[f32; 3]> = if mesh.normals.len() == num_verts * 3 {
            (0..num_verts)
                .map(|i| {
                    [
                        mesh.normals[i * 3] as f32,
                        mesh.normals[i * 3 + 1] as f32,
                        mesh.normals[i * 3 + 2] as f32,
                    ]
                })
                .collect()
        } else {
            Vec::new()
        };

        let indices: Vec<u32> = mesh.indices.clone();
        let name = model.name;

        meshes.push(MeshData {
            positions,
            normals,
            indices,
            name,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_minimal_obj() {
        let dir = std::env::temp_dir().join("tiletopia_obj_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.obj");

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

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_nonexistent_obj() {
        let result = read(Path::new("/tmp/nonexistent_obj_file.obj"));
        assert!(result.is_err());
    }
}
