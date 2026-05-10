//! Feature Service — CRUD operations on spatial features (like WFS-T or ArcGIS Feature Layers).
//!
//! Supports creating, reading, updating, and deleting geographic features
//! with arbitrary attributes, spatial indexing, and query capabilities.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A feature layer (collection of features with a schema).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureLayer {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub geometry_type: GeometryType,
    pub crs: String,
    pub fields: Vec<FieldSchema>,
    pub feature_count: u64,
    pub extent: [f64; 4], // [west, south, east, north]
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Geometry type for the layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GeometryType {
    Point,
    MultiPoint,
    LineString,
    MultiLineString,
    Polygon,
    MultiPolygon,
}

/// Field schema definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldSchema {
    pub name: String,
    pub field_type: FieldType,
    pub nullable: bool,
    pub default_value: Option<serde_json::Value>,
}

/// Field data types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    Date,
    DateTime,
    Json,
}

/// A spatial feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feature {
    pub id: Uuid,
    pub layer_id: Uuid,
    pub geometry: FeatureGeometry,
    pub properties: HashMap<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Feature geometry (GeoJSON-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureGeometry {
    #[serde(rename = "type")]
    pub geom_type: String,
    pub coordinates: serde_json::Value,
}

/// Spatial query filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialQuery {
    pub bbox: Option<[f64; 4]>,
    pub intersects: Option<FeatureGeometry>,
    pub within_distance_m: Option<(f64, [f64; 2])>, // (distance, center)
    pub where_clause: Option<String>,
    pub limit: usize,
    pub offset: usize,
    pub order_by: Option<String>,
}

/// Feature service engine.
pub struct FeatureServiceEngine {
    layers: Vec<FeatureLayer>,
    features: Vec<Feature>,
}

impl FeatureServiceEngine {
    /// Create with demo data.
    pub fn new() -> Self {
        let (layers, features) = demo_data();
        Self { layers, features }
    }

    /// List all feature layers.
    pub fn list_layers(&self) -> &[FeatureLayer] {
        &self.layers
    }

    /// Get a specific layer.
    pub fn get_layer(&self, id: Uuid) -> Option<&FeatureLayer> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// Query features in a layer with spatial filtering.
    pub fn query_features(&self, layer_id: Uuid, query: &SpatialQuery) -> Vec<&Feature> {
        self.features
            .iter()
            .filter(|f| f.layer_id == layer_id)
            .filter(|f| {
                if let Some(bbox) = &query.bbox {
                    if let Some(centroid) = feature_centroid(f) {
                        centroid[0] >= bbox[0]
                            && centroid[0] <= bbox[2]
                            && centroid[1] >= bbox[1]
                            && centroid[1] <= bbox[3]
                    } else {
                        true
                    }
                } else {
                    true
                }
            })
            .filter(|f| {
                if let Some((dist, center)) = &query.within_distance_m {
                    if let Some(c) = feature_centroid(f) {
                        haversine_m(c[1], c[0], center[1], center[0]) <= *dist
                    } else {
                        true
                    }
                } else {
                    true
                }
            })
            .skip(query.offset)
            .take(query.limit)
            .collect()
    }

    /// Get feature count for a layer.
    pub fn feature_count(&self, layer_id: Uuid) -> usize {
        self.features
            .iter()
            .filter(|f| f.layer_id == layer_id)
            .count()
    }

    /// Get a single feature by ID.
    pub fn get_feature(&self, id: Uuid) -> Option<&Feature> {
        self.features.iter().find(|f| f.id == id)
    }

    /// Create a new layer.
    pub fn create_layer(
        &mut self,
        name: String,
        description: String,
        geometry_type: GeometryType,
        crs: String,
        fields: Vec<FieldSchema>,
    ) -> FeatureLayer {
        let layer = FeatureLayer {
            id: Uuid::new_v4(),
            name,
            description,
            geometry_type,
            crs,
            fields,
            feature_count: 0,
            extent: [0.0, 0.0, 0.0, 0.0],
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        self.layers.push(layer.clone());
        layer
    }

    /// Add a feature to a layer.
    pub fn add_feature(
        &mut self,
        layer_id: Uuid,
        geometry: FeatureGeometry,
        properties: HashMap<String, serde_json::Value>,
    ) -> Option<Feature> {
        if self.get_layer(layer_id).is_none() {
            return None;
        }
        let feature = Feature {
            id: Uuid::new_v4(),
            layer_id,
            geometry,
            properties,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        self.features.push(feature.clone());
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == layer_id) {
            layer.feature_count += 1;
            layer.updated_at = Utc::now();
        }
        Some(feature)
    }

    /// Update a feature's properties.
    pub fn update_feature(
        &mut self,
        id: Uuid,
        properties: HashMap<String, serde_json::Value>,
    ) -> Option<&Feature> {
        if let Some(f) = self.features.iter_mut().find(|f| f.id == id) {
            f.properties = properties;
            f.updated_at = Utc::now();
            let id = f.id;
            // Re-borrow to return the updated reference
            return self.features.iter().find(|f| f.id == id);
        }
        None
    }

    /// Delete a feature.
    pub fn delete_feature(&mut self, id: Uuid) -> bool {
        let before = self.features.len();
        let layer_id = self
            .features
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.layer_id);
        self.features.retain(|f| f.id != id);
        if let Some(lid) = layer_id {
            if let Some(layer) = self.layers.iter_mut().find(|l| l.id == lid) {
                layer.feature_count = layer.feature_count.saturating_sub(1);
            }
        }
        self.features.len() < before
    }
}

impl Default for FeatureServiceEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the centroid of a feature's geometry from its GeoJSON coordinates.
fn feature_centroid(f: &Feature) -> Option<[f64; 2]> {
    match f.geometry.geom_type.as_str() {
        "Point" => {
            let coords: [f64; 2] = serde_json::from_value(f.geometry.coordinates.clone()).ok()?;
            Some(coords)
        }
        "LineString" => {
            let coords: Vec<[f64; 2]> =
                serde_json::from_value(f.geometry.coordinates.clone()).ok()?;
            if coords.is_empty() {
                return None;
            }
            let n = coords.len() as f64;
            Some([
                coords.iter().map(|c| c[0]).sum::<f64>() / n,
                coords.iter().map(|c| c[1]).sum::<f64>() / n,
            ])
        }
        "Polygon" => {
            let rings: Vec<Vec<[f64; 2]>> =
                serde_json::from_value(f.geometry.coordinates.clone()).ok()?;
            let ring = rings.first()?;
            if ring.is_empty() {
                return None;
            }
            let n = ring.len() as f64;
            Some([
                ring.iter().map(|c| c[0]).sum::<f64>() / n,
                ring.iter().map(|c| c[1]).sum::<f64>() / n,
            ])
        }
        _ => None,
    }
}

/// Haversine distance in meters.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

/// Generate demo layers and features.
fn demo_data() -> (Vec<FeatureLayer>, Vec<Feature>) {
    let buildings_layer_id = Uuid::new_v4();
    let roads_layer_id = Uuid::new_v4();

    let layers = vec![
        FeatureLayer {
            id: buildings_layer_id,
            name: "Buildings".into(),
            description: "Building footprints with attributes".into(),
            geometry_type: GeometryType::Polygon,
            crs: "EPSG:4326".into(),
            fields: vec![
                FieldSchema {
                    name: "name".into(),
                    field_type: FieldType::String,
                    nullable: true,
                    default_value: None,
                },
                FieldSchema {
                    name: "height_m".into(),
                    field_type: FieldType::Float,
                    nullable: false,
                    default_value: Some(serde_json::json!(10.0)),
                },
                FieldSchema {
                    name: "floors".into(),
                    field_type: FieldType::Integer,
                    nullable: false,
                    default_value: Some(serde_json::json!(1)),
                },
                FieldSchema {
                    name: "year_built".into(),
                    field_type: FieldType::Integer,
                    nullable: true,
                    default_value: None,
                },
            ],
            feature_count: 3,
            extent: [-122.42, 37.77, -122.40, 37.79],
            created_at: Utc::now() - chrono::Duration::days(30),
            updated_at: Utc::now(),
        },
        FeatureLayer {
            id: roads_layer_id,
            name: "Roads".into(),
            description: "Road centerlines with classification".into(),
            geometry_type: GeometryType::LineString,
            crs: "EPSG:4326".into(),
            fields: vec![
                FieldSchema {
                    name: "name".into(),
                    field_type: FieldType::String,
                    nullable: false,
                    default_value: None,
                },
                FieldSchema {
                    name: "road_class".into(),
                    field_type: FieldType::String,
                    nullable: false,
                    default_value: None,
                },
                FieldSchema {
                    name: "lanes".into(),
                    field_type: FieldType::Integer,
                    nullable: false,
                    default_value: Some(serde_json::json!(2)),
                },
                FieldSchema {
                    name: "speed_limit".into(),
                    field_type: FieldType::Integer,
                    nullable: true,
                    default_value: None,
                },
            ],
            feature_count: 2,
            extent: [-122.42, 37.77, -122.40, 37.80],
            created_at: Utc::now() - chrono::Duration::days(14),
            updated_at: Utc::now(),
        },
    ];

    let features = vec![
        Feature {
            id: Uuid::new_v4(),
            layer_id: buildings_layer_id,
            geometry: FeatureGeometry {
                geom_type: "Polygon".into(),
                coordinates: serde_json::json!([[
                    [-122.41, 37.78],
                    [-122.409, 37.78],
                    [-122.409, 37.781],
                    [-122.41, 37.781],
                    [-122.41, 37.78]
                ]]),
            },
            properties: HashMap::from([
                ("name".into(), serde_json::json!("City Hall")),
                ("height_m".into(), serde_json::json!(94.0)),
                ("floors".into(), serde_json::json!(5)),
                ("year_built".into(), serde_json::json!(1915)),
            ]),
            created_at: Utc::now() - chrono::Duration::days(20),
            updated_at: Utc::now(),
        },
        Feature {
            id: Uuid::new_v4(),
            layer_id: buildings_layer_id,
            geometry: FeatureGeometry {
                geom_type: "Polygon".into(),
                coordinates: serde_json::json!([[
                    [-122.405, 37.785],
                    [-122.404, 37.785],
                    [-122.404, 37.786],
                    [-122.405, 37.786],
                    [-122.405, 37.785]
                ]]),
            },
            properties: HashMap::from([
                ("name".into(), serde_json::json!("Office Tower A")),
                ("height_m".into(), serde_json::json!(120.0)),
                ("floors".into(), serde_json::json!(30)),
                ("year_built".into(), serde_json::json!(2018)),
            ]),
            created_at: Utc::now() - chrono::Duration::days(10),
            updated_at: Utc::now(),
        },
        Feature {
            id: Uuid::new_v4(),
            layer_id: roads_layer_id,
            geometry: FeatureGeometry {
                geom_type: "LineString".into(),
                coordinates: serde_json::json!([
                    [-122.42, 37.78],
                    [-122.41, 37.78],
                    [-122.40, 37.78]
                ]),
            },
            properties: HashMap::from([
                ("name".into(), serde_json::json!("Market Street")),
                ("road_class".into(), serde_json::json!("primary")),
                ("lanes".into(), serde_json::json!(4)),
                ("speed_limit".into(), serde_json::json!(40)),
            ]),
            created_at: Utc::now() - chrono::Duration::days(14),
            updated_at: Utc::now(),
        },
    ];

    (layers, features)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_layers() {
        let engine = FeatureServiceEngine::new();
        assert_eq!(engine.list_layers().len(), 2);
    }

    #[test]
    fn test_query_features() {
        let engine = FeatureServiceEngine::new();
        let layer = &engine.list_layers()[0];
        let query = SpatialQuery {
            bbox: None,
            intersects: None,
            within_distance_m: None,
            where_clause: None,
            limit: 100,
            offset: 0,
            order_by: None,
        };
        let features = engine.query_features(layer.id, &query);
        assert_eq!(features.len(), 2);
    }

    #[test]
    fn test_feature_count() {
        let engine = FeatureServiceEngine::new();
        let roads_layer = engine
            .list_layers()
            .iter()
            .find(|l| l.name == "Roads")
            .unwrap();
        assert_eq!(engine.feature_count(roads_layer.id), 1);
    }

    #[test]
    fn test_pagination() {
        let engine = FeatureServiceEngine::new();
        let layer = &engine.list_layers()[0];
        let query = SpatialQuery {
            bbox: None,
            intersects: None,
            within_distance_m: None,
            where_clause: None,
            limit: 1,
            offset: 0,
            order_by: None,
        };
        let page1 = engine.query_features(layer.id, &query);
        assert_eq!(page1.len(), 1);
    }

    #[test]
    fn test_bbox_filter() {
        let engine = FeatureServiceEngine::new();
        let layer = &engine.list_layers()[0]; // Buildings
        // Bbox that only contains City Hall (around -122.41, 37.78)
        let query = SpatialQuery {
            bbox: Some([-122.411, 37.779, -122.408, 37.782]),
            intersects: None,
            within_distance_m: None,
            where_clause: None,
            limit: 100,
            offset: 0,
            order_by: None,
        };
        let features = engine.query_features(layer.id, &query);
        assert_eq!(features.len(), 1);
    }

    #[test]
    fn test_within_distance_filter() {
        let engine = FeatureServiceEngine::new();
        let layer = &engine.list_layers()[0]; // Buildings
        // Center near City Hall, radius 100m — should only find City Hall
        let query = SpatialQuery {
            bbox: None,
            intersects: None,
            within_distance_m: Some((100.0, [-122.4095, 37.7805])),
            where_clause: None,
            limit: 100,
            offset: 0,
            order_by: None,
        };
        let features = engine.query_features(layer.id, &query);
        assert_eq!(features.len(), 1);
    }

    #[test]
    fn test_create_layer() {
        let mut engine = FeatureServiceEngine::new();
        let layer = engine.create_layer(
            "Parks".into(),
            "Park boundaries".into(),
            GeometryType::Polygon,
            "EPSG:4326".into(),
            vec![],
        );
        assert_eq!(layer.name, "Parks");
        assert_eq!(engine.list_layers().len(), 3);
    }

    #[test]
    fn test_add_and_delete_feature() {
        let mut engine = FeatureServiceEngine::new();
        let layer_id = engine.list_layers()[0].id;
        let geom = FeatureGeometry {
            geom_type: "Point".into(),
            coordinates: serde_json::json!([-122.41, 37.78]),
        };
        let feature = engine.add_feature(layer_id, geom, HashMap::new()).unwrap();
        assert_eq!(engine.feature_count(layer_id), 3);

        assert!(engine.delete_feature(feature.id));
        assert_eq!(engine.feature_count(layer_id), 2);
    }

    #[test]
    fn test_update_feature() {
        let mut engine = FeatureServiceEngine::new();
        let feature_id = engine.features[0].id;
        let mut props = HashMap::new();
        props.insert("name".into(), serde_json::json!("Renamed Building"));
        let updated = engine.update_feature(feature_id, props).unwrap();
        assert_eq!(
            updated.properties.get("name").unwrap(),
            &serde_json::json!("Renamed Building")
        );
    }
}
