//! glTF/GLB mesh reader.

use crate::{IngestError, MeshData};
use std::path::Path;

/// Read meshes from a glTF or GLB file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let (gltf, buffers, _) = gltf::import(path)
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

            let name = mesh.name().unwrap_or("unnamed").to_string();

            meshes.push(MeshData {
                positions,
                normals,
                indices,
                name,
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
