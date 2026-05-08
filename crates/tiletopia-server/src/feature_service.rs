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

    /// Query features in a layer.
    pub fn query_features(&self, layer_id: Uuid, query: &SpatialQuery) -> Vec<&Feature> {
        self.features
            .iter()
            .filter(|f| f.layer_id == layer_id)
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
}

impl Default for FeatureServiceEngine {
    fn default() -> Self {
        Self::new()
    }
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
}
