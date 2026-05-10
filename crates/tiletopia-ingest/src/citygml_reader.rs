//! CityGML mesh reader.

use crate::{IngestError, MeshData};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::path::Path;

/// Read meshes from a CityGML (GML) file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let data = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&data);

    let mut meshes: Vec<MeshData> = Vec::new();
    let mut all_positions: Vec<[f32; 3]> = Vec::new();
    let mut all_indices: Vec<u32> = Vec::new();
    let mut in_pos_list = false;
    let mut current_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == b"posList" || local == b"pos" {
                    in_pos_list = true;
                    current_text.clear();
                }
            }
            Ok(Event::Text(ref e)) if in_pos_list => {
                current_text.push_str(&e.decode().unwrap_or_default());
            }
            Ok(Event::End(ref e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if (local == b"posList" || local == b"pos") && in_pos_list {
                    in_pos_list = false;
                    let coords: Vec<f64> = current_text
                        .split_whitespace()
                        .filter_map(|s| s.parse::<f64>().ok())
                        .collect();

                    if coords.len() >= 9 && coords.len().is_multiple_of(3) {
                        let num_pts = coords.len() / 3;
                        let base = all_positions.len() as u32;

                        for i in 0..num_pts {
                            all_positions.push([
                                coords[i * 3] as f32,
                                coords[i * 3 + 1] as f32,
                                coords[i * 3 + 2] as f32,
                            ]);
                        }

                        // Fan triangulation
                        for i in 1..num_pts - 1 {
                            all_indices.push(base);
                            all_indices.push(base + i as u32);
                            all_indices.push(base + i as u32 + 1);
                        }
                    }
                    current_text.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IngestError::ParseError(format!(
                    "CityGML XML parse error: {e}"
                )));
            }
            _ => {}
        }
    }

    if !all_positions.is_empty() {
        meshes.push(MeshData {
            positions: all_positions,
            normals: Vec::new(),
            indices: all_indices,
            name: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("citygml")
                .to_string(),
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

/// Extract local name from a potentially namespace-prefixed XML name.
fn local_name(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b':') {
        Some(pos) => &name[pos + 1..],
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_minimal_citygml() {
        let dir = std::env::temp_dir().join("tiletopia_citygml_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.gml");

        let citygml = r#"<?xml version="1.0" encoding="UTF-8"?>
<CityModel xmlns="http://www.opengis.net/citygml/2.0"
           xmlns:gml="http://www.opengis.net/gml">
  <cityObjectMember>
    <Building>
      <lod2Solid>
        <gml:Solid>
          <gml:exterior>
            <gml:CompositeSurface>
              <gml:surfaceMember>
                <gml:Polygon>
                  <gml:exterior>
                    <gml:LinearRing>
                      <gml:posList>0 0 0 1 0 0 1 1 0 0 1 0 0 0 0</gml:posList>
                    </gml:LinearRing>
                  </gml:exterior>
                </gml:Polygon>
              </gml:surfaceMember>
            </gml:CompositeSurface>
          </gml:exterior>
        </gml:Solid>
      </lod2Solid>
    </Building>
  </cityObjectMember>
</CityModel>"#;
        std::fs::write(&path, citygml).unwrap();

        let meshes = read(&path).unwrap();
        assert_eq!(meshes.len(), 1);
        // 5 coordinates (closed polygon: 5 points)
        assert_eq!(meshes[0].positions.len(), 5);
        // Fan triangulation of 5 points: 3 triangles = 9 indices
        assert_eq!(meshes[0].indices.len(), 9);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_invalid_xml() {
        let dir = std::env::temp_dir().join("tiletopia_citygml_bad_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.gml");
        std::fs::write(&path, "<not closed").unwrap();

        // quick-xml may or may not error on this; either way should not panic
        let _ = read(&path);

        std::fs::remove_dir_all(&dir).ok();
    }
}
