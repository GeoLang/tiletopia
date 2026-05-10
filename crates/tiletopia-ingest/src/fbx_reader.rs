//! FBX binary mesh reader.

use crate::{IngestError, MeshData};
use fbxcel_dom::any::AnyDocument;
use fbxcel_dom::v7400::object::geometry::TypedGeometryHandle;
use fbxcel_dom::v7400::object::TypedObjectHandle;
use std::io::BufReader;
use std::path::Path;

/// Read meshes from an FBX binary file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let doc = match AnyDocument::from_seekable_reader(reader)
        .map_err(|e| IngestError::ParseError(format!("FBX load error: {e}")))?
    {
        AnyDocument::V7400(_, doc) => doc,
        _ => return Err(IngestError::ParseError("unsupported FBX version".into())),
    };

    let mut meshes = Vec::new();

    for obj in doc.objects() {
        let typed = match obj.get_typed() {
            TypedObjectHandle::Geometry(geo) => geo,
            _ => continue,
        };
        let mesh_handle = match typed {
            TypedGeometryHandle::Mesh(m) => m,
            _ => continue,
        };

        let poly_verts = mesh_handle
            .polygon_vertices()
            .map_err(|e| IngestError::ParseError(format!("FBX polygon vertices: {e}")))?;

        // Extract vertex positions from raw control points.
        let positions: Vec<[f32; 3]> = poly_verts
            .raw_control_points()
            .map_err(|e| IngestError::ParseError(format!("FBX control points: {e}")))?
            .map(|pt| [pt.x as f32, pt.y as f32, pt.z as f32])
            .collect();

        if positions.is_empty() {
            continue;
        }

        // Decode FBX polygon vertex indices and triangulate.
        let raw = poly_verts.raw_polygon_vertices();
        let indices = triangulate_fbx_polygons(raw);

        let name = mesh_handle.name().unwrap_or("mesh").to_string();

        meshes.push(MeshData {
            positions,
            normals: Vec::new(),
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

/// Decode FBX polygon vertex indices and fan-triangulate polygons.
///
/// In FBX, a negative index marks the last vertex of a polygon.
/// The actual index is decoded as `!(neg_val)`.
fn triangulate_fbx_polygons(raw: &[i32]) -> Vec<u32> {
    let mut indices = Vec::new();
    let mut poly_start = 0;

    for (i, &val) in raw.iter().enumerate() {
        if val < 0 {
            // End of polygon — collect vertex indices for this polygon.
            let mut polygon = Vec::with_capacity(i - poly_start + 1);
            for j in poly_start..i {
                polygon.push(raw[j] as u32);
            }
            polygon.push((!val) as u32);

            // Fan triangulation: (v0, v1, v2), (v0, v2, v3), ...
            for k in 1..polygon.len() - 1 {
                indices.push(polygon[0]);
                indices.push(polygon[k]);
                indices.push(polygon[k + 1]);
            }

            poly_start = i + 1;
        }
    }

    indices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_nonexistent_file() {
        let result = read(Path::new("/tmp/nonexistent_fbx_file.fbx"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_empty_file() {
        let dir = std::env::temp_dir().join("tiletopia_fbx_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.fbx");
        std::fs::write(&path, b"").unwrap();

        let result = read(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_triangulate_single_triangle() {
        // A single triangle: indices 0, 1, -3 (last vertex negated-and-inverted: !(2) = -3)
        let raw = vec![0, 1, -3];
        let tris = triangulate_fbx_polygons(&raw);
        assert_eq!(tris, vec![0, 1, 2]);
    }

    #[test]
    fn test_triangulate_quad() {
        // A quad: 0, 1, 2, -4 (last vertex: !(3) = -4)
        let raw = vec![0, 1, 2, -4];
        let tris = triangulate_fbx_polygons(&raw);
        assert_eq!(tris, vec![0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn test_triangulate_multiple_polygons() {
        // Triangle then quad
        let raw = vec![0, 1, -3, 4, 5, 6, -8];
        let tris = triangulate_fbx_polygons(&raw);
        assert_eq!(tris, vec![0, 1, 2, 4, 5, 6, 4, 6, 7]);
    }
}
