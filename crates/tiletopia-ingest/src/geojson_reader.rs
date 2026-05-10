//! GeoJSON vector reader.

use crate::{IngestError, VectorFeature, VectorGeometry};
use std::collections::HashMap;
use std::path::Path;

/// Read a GeoJSON file into vector features.
pub fn read(path: &Path) -> Result<Vec<VectorFeature>, IngestError> {
    let data = std::fs::read_to_string(path)?;
    let geojson: geojson::GeoJson = data
        .parse()
        .map_err(|e| IngestError::ParseError(format!("GeoJSON parse error: {e}")))?;

    let features = match geojson {
        geojson::GeoJson::FeatureCollection(fc) => fc.features,
        geojson::GeoJson::Feature(f) => vec![f],
        geojson::GeoJson::Geometry(g) => {
            vec![geojson::Feature {
                geometry: Some(g),
                ..Default::default()
            }]
        }
    };

    let mut result = Vec::with_capacity(features.len());

    for feature in features {
        let geometry = match feature.geometry {
            Some(g) => convert_geometry(g.value)?,
            None => continue,
        };

        let properties = feature
            .properties
            .map(|props| {
                props
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let s = match v {
                            serde_json::Value::String(s) => s,
                            serde_json::Value::Null => return None,
                            other => other.to_string(),
                        };
                        Some((k, s))
                    })
                    .collect::<HashMap<String, String>>()
            })
            .unwrap_or_default();

        result.push(VectorFeature {
            geometry,
            properties,
        });
    }

    tracing::info!("Read {} features from {}", result.len(), path.display());
    Ok(result)
}

fn convert_geometry(value: geojson::GeometryValue) -> Result<VectorGeometry, IngestError> {
    match value {
        geojson::GeometryValue::Point { coordinates } => {
            Ok(VectorGeometry::Point(coordinates[0], coordinates[1]))
        }
        geojson::GeometryValue::MultiPoint { coordinates } => Ok(VectorGeometry::MultiPoint(
            coordinates.into_iter().map(|c| (c[0], c[1])).collect(),
        )),
        geojson::GeometryValue::LineString { coordinates } => Ok(VectorGeometry::LineString(
            coordinates.into_iter().map(|c| (c[0], c[1])).collect(),
        )),
        geojson::GeometryValue::MultiLineString { coordinates } => {
            Ok(VectorGeometry::MultiLineString(
                coordinates
                    .into_iter()
                    .map(|line| line.into_iter().map(|c| (c[0], c[1])).collect())
                    .collect(),
            ))
        }
        geojson::GeometryValue::Polygon { coordinates } => Ok(VectorGeometry::Polygon(
            coordinates
                .into_iter()
                .map(|ring| ring.into_iter().map(|c| (c[0], c[1])).collect())
                .collect(),
        )),
        geojson::GeometryValue::MultiPolygon { coordinates } => {
            let polys: Vec<Vec<Vec<(f64, f64)>>> = coordinates
                .into_iter()
                .map(|poly| {
                    poly.into_iter()
                        .map(|ring| ring.into_iter().map(|c| (c[0], c[1])).collect())
                        .collect()
                })
                .collect();
            Ok(VectorGeometry::MultiPolygon(polys))
        }
        geojson::GeometryValue::GeometryCollection { .. } => Err(IngestError::UnsupportedFormat(
            "GeometryCollection".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_point_feature() {
        let dir = std::env::temp_dir().join("tiletopia_geojson_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.geojson");

        let geojson = r#"{
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [10.0, 20.0] },
                    "properties": { "name": "TestPoint" }
                }
            ]
        }"#;
        std::fs::write(&path, geojson).unwrap();

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
    fn test_read_polygon_feature() {
        let dir = std::env::temp_dir().join("tiletopia_geojson_poly_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("poly.geojson");

        let geojson = r#"{
            "type": "Feature",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[0,0],[1,0],[1,1],[0,1],[0,0]]]
            },
            "properties": null
        }"#;
        std::fs::write(&path, geojson).unwrap();

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

    #[test]
    fn test_read_invalid_json() {
        let dir = std::env::temp_dir().join("tiletopia_geojson_bad_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.geojson");
        std::fs::write(&path, "not json at all").unwrap();

        let result = read(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
