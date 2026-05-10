//! CityJSON mesh reader.

use crate::{IngestError, MeshData};
use std::path::Path;

/// Read meshes from a CityJSON file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let data = std::fs::read_to_string(path)?;
    let root: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| IngestError::ParseError(format!("CityJSON parse error: {e}")))?;

    let doc_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if doc_type != "CityJSON" {
        return Err(IngestError::ParseError(
            "not a CityJSON file (missing \"type\":\"CityJSON\")".to_string(),
        ));
    }

    let raw_vertices = root
        .get("vertices")
        .and_then(|v| v.as_array())
        .ok_or_else(|| IngestError::ParseError("CityJSON: missing vertices array".to_string()))?;

    let vertices: Vec<[f64; 3]> = raw_vertices
        .iter()
        .map(|v| {
            let arr = v.as_array().unwrap_or(&Vec::new()).clone();
            let x = arr.first().and_then(|n| n.as_f64()).unwrap_or(0.0);
            let y = arr.get(1).and_then(|n| n.as_f64()).unwrap_or(0.0);
            let z = arr.get(2).and_then(|n| n.as_f64()).unwrap_or(0.0);
            [x, y, z]
        })
        .collect();

    // Apply transform if present
    let (scale, translate) = if let Some(transform) = root.get("transform") {
        let s = transform
            .get("scale")
            .and_then(|v| v.as_array())
            .map(|a| {
                [
                    a[0].as_f64().unwrap_or(1.0),
                    a[1].as_f64().unwrap_or(1.0),
                    a[2].as_f64().unwrap_or(1.0),
                ]
            })
            .unwrap_or([1.0, 1.0, 1.0]);
        let t = transform
            .get("translate")
            .and_then(|v| v.as_array())
            .map(|a| {
                [
                    a[0].as_f64().unwrap_or(0.0),
                    a[1].as_f64().unwrap_or(0.0),
                    a[2].as_f64().unwrap_or(0.0),
                ]
            })
            .unwrap_or([0.0, 0.0, 0.0]);
        (s, t)
    } else {
        ([1.0, 1.0, 1.0], [0.0, 0.0, 0.0])
    };

    let transformed: Vec<[f32; 3]> = vertices
        .iter()
        .map(|v| {
            [
                (v[0] * scale[0] + translate[0]) as f32,
                (v[1] * scale[1] + translate[1]) as f32,
                (v[2] * scale[2] + translate[2]) as f32,
            ]
        })
        .collect();

    let city_objects = root
        .get("CityObjects")
        .and_then(|v| v.as_object())
        .ok_or_else(|| IngestError::ParseError("CityJSON: missing CityObjects".to_string()))?;

    let mut meshes = Vec::new();

    for (name, obj) in city_objects {
        let geometries = match obj.get("geometry").and_then(|v| v.as_array()) {
            Some(g) => g,
            None => continue,
        };

        let mut positions = Vec::new();
        let mut indices = Vec::new();

        for geom in geometries {
            let boundaries = match geom.get("boundaries").and_then(|v| v.as_array()) {
                Some(b) => b,
                None => continue,
            };

            collect_triangles(boundaries, &transformed, &mut positions, &mut indices);
        }

        if !positions.is_empty() {
            meshes.push(MeshData {
                positions,
                normals: Vec::new(),
                indices,
                name: name.clone(),
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

/// Recursively collect boundary indices and fan-triangulate polygons.
fn collect_triangles(
    value: &[serde_json::Value],
    all_verts: &[[f32; 3]],
    positions: &mut Vec<[f32; 3]>,
    indices: &mut Vec<u32>,
) {
    // CityJSON boundaries can be nested: Solid → Shell → Surface → Ring → indices
    // We need to find arrays of integers (vertex index rings) and triangulate them.
    if value.is_empty() {
        return;
    }

    // Check if this is a ring of indices (all elements are integers)
    if value[0].is_number() {
        let ring: Vec<usize> = value
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as usize))
            .collect();
        if ring.len() >= 3 {
            // Fan triangulation: first vertex is the hub
            let base = positions.len() as u32;
            for &idx in &ring {
                if idx < all_verts.len() {
                    positions.push(all_verts[idx]);
                } else {
                    positions.push([0.0, 0.0, 0.0]);
                }
            }
            for i in 1..ring.len() - 1 {
                indices.push(base);
                indices.push(base + i as u32);
                indices.push(base + i as u32 + 1);
            }
        }
        return;
    }

    // Otherwise recurse into sub-arrays
    for item in value {
        if let Some(arr) = item.as_array() {
            collect_triangles(arr, all_verts, positions, indices);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_minimal_cityjson() {
        let dir = std::env::temp_dir().join("tiletopia_cityjson_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.city.json");

        let cityjson = r#"{
            "type": "CityJSON",
            "version": "1.0",
            "vertices": [
                [0, 0, 0],
                [1, 0, 0],
                [1, 1, 0],
                [0, 1, 0]
            ],
            "CityObjects": {
                "building1": {
                    "type": "Building",
                    "geometry": [{
                        "type": "Solid",
                        "lod": "2",
                        "boundaries": [[[[0, 1, 2, 3]]]]
                    }]
                }
            }
        }"#;
        std::fs::write(&path, cityjson).unwrap();

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 1);
        assert_eq!(meshes[0].positions.len(), 4);
        // Fan triangulation of a quad: 2 triangles = 6 indices
        assert_eq!(meshes[0].indices.len(), 6);
        assert_eq!(meshes[0].name, "building1");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_not_cityjson() {
        let dir = std::env::temp_dir().join("tiletopia_cityjson_bad_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not_city.json");
        std::fs::write(&path, r#"{"type":"FeatureCollection"}"#).unwrap();

        let result = read(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
