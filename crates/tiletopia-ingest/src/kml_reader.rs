//! KML vector reader.

use crate::{IngestError, VectorFeature, VectorGeometry};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::path::Path;

/// Read vector features from a KML file.
pub fn read(path: &Path) -> Result<Vec<VectorFeature>, IngestError> {
    let data = std::fs::read_to_string(path)?;
    let mut reader = Reader::from_str(&data);

    let mut features = Vec::new();
    let mut in_placemark = false;
    let mut current_name = String::new();
    let mut current_geom: Option<VectorGeometry> = None;
    let mut tag_stack: Vec<String> = Vec::new();
    let mut current_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                let local = local_name_str(e.name().as_ref());
                tag_stack.push(local.clone());

                if local.as_str() == "Placemark" {
                    in_placemark = true;
                    current_name.clear();
                    current_geom = None;
                }
                current_text.clear();
            }
            Ok(Event::Text(ref e)) => {
                current_text.push_str(&e.decode().unwrap_or_default());
            }
            Ok(Event::End(ref e)) => {
                let local = local_name_str(e.name().as_ref());

                if in_placemark {
                    match local.as_str() {
                        "name" => {
                            current_name = current_text.trim().to_string();
                        }
                        "coordinates" => {
                            let parent = tag_stack
                                .iter()
                                .rev()
                                .nth(1)
                                .map(|s| s.as_str())
                                .unwrap_or("");
                            current_geom = parse_coordinates(&current_text, parent);
                        }
                        "Placemark" => {
                            in_placemark = false;
                            if let Some(geom) = current_geom.take() {
                                let mut props = HashMap::new();
                                if !current_name.is_empty() {
                                    props.insert("name".to_string(), current_name.clone());
                                }
                                features.push(VectorFeature {
                                    geometry: geom,
                                    properties: props,
                                });
                            }
                        }
                        _ => {}
                    }
                }

                current_text.clear();
                tag_stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(IngestError::ParseError(format!("KML XML parse error: {e}")));
            }
            _ => {}
        }
    }

    tracing::info!("Read {} features from {}", features.len(), path.display());
    Ok(features)
}

/// Parse a KML <coordinates> text block into a geometry, depending on the parent element.
fn parse_coordinates(text: &str, parent: &str) -> Option<VectorGeometry> {
    let coords: Vec<(f64, f64)> = text
        .split_whitespace()
        .filter_map(|s| {
            // Each tuple is lon,lat[,alt] separated by commas
            let parts: Vec<&str> = s.split(',').collect();
            if parts.len() >= 2 {
                let lon = parts[0].parse::<f64>().ok()?;
                let lat = parts[1].parse::<f64>().ok()?;
                Some((lon, lat))
            } else {
                None
            }
        })
        .collect();

    if coords.is_empty() {
        return None;
    }

    match parent {
        "Point" => coords.first().map(|&(x, y)| VectorGeometry::Point(x, y)),
        "LineString" => Some(VectorGeometry::LineString(coords)),
        "LinearRing" | "Polygon" | "outerBoundaryIs" => Some(VectorGeometry::Polygon(vec![coords])),
        _ => {
            // Default: if single coordinate, point; if multiple, line string
            if coords.len() == 1 {
                Some(VectorGeometry::Point(coords[0].0, coords[0].1))
            } else {
                Some(VectorGeometry::LineString(coords))
            }
        }
    }
}

/// Extract local name from a potentially namespace-prefixed XML name, as a String.
fn local_name_str(name: &[u8]) -> String {
    let local = match name.iter().rposition(|&b| b == b':') {
        Some(pos) => &name[pos + 1..],
        None => name,
    };
    String::from_utf8_lossy(local).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_kml_point() {
        let dir = std::env::temp_dir().join("tiletopia_kml_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.kml");

        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark>
      <name>TestPoint</name>
      <Point>
        <coordinates>10.0,20.0,0</coordinates>
      </Point>
    </Placemark>
  </Document>
</kml>"#;
        std::fs::write(&path, kml).unwrap();

        let features = read(&path).unwrap();
        assert_eq!(features.len(), 1);
        match &features[0].geometry {
            VectorGeometry::Point(x, y) => {
                assert!((x - 10.0).abs() < 1e-10);
                assert!((y - 20.0).abs() < 1e-10);
            }
            other => panic!("expected Point, got {:?}", other),
        }
        assert_eq!(features[0].properties.get("name").unwrap(), "TestPoint");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_kml_linestring() {
        let dir = std::env::temp_dir().join("tiletopia_kml_line_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("line.kml");

        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark>
      <name>TestLine</name>
      <LineString>
        <coordinates>0,0,0 1,1,0 2,0,0</coordinates>
      </LineString>
    </Placemark>
  </Document>
</kml>"#;
        std::fs::write(&path, kml).unwrap();

        let features = read(&path).unwrap();
        assert_eq!(features.len(), 1);
        match &features[0].geometry {
            VectorGeometry::LineString(pts) => {
                assert_eq!(pts.len(), 3);
                assert!((pts[0].0).abs() < 1e-10);
                assert!((pts[1].0 - 1.0).abs() < 1e-10);
            }
            other => panic!("expected LineString, got {:?}", other),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_kml_polygon() {
        let dir = std::env::temp_dir().join("tiletopia_kml_poly_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("poly.kml");

        let kml = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2">
  <Document>
    <Placemark>
      <Polygon>
        <outerBoundaryIs>
          <LinearRing>
            <coordinates>0,0,0 1,0,0 1,1,0 0,1,0 0,0,0</coordinates>
          </LinearRing>
        </outerBoundaryIs>
      </Polygon>
    </Placemark>
  </Document>
</kml>"#;
        std::fs::write(&path, kml).unwrap();

        let features = read(&path).unwrap();
        assert_eq!(features.len(), 1);
        match &features[0].geometry {
            VectorGeometry::Polygon(rings) => {
                assert_eq!(rings.len(), 1);
                assert_eq!(rings[0].len(), 5);
            }
            other => panic!("expected Polygon, got {:?}", other),
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
