//! glTF EXT_structural_metadata — rich semantic queries on mesh features.
//!
//! Implements the 3D Tiles 1.1 metadata extension for attaching structured
//! properties to vertices, faces, and tiles for semantic querying.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Property type for metadata schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PropertyType {
    Scalar,
    Vec2,
    Vec3,
    Vec4,
    Mat2,
    Mat3,
    Mat4,
    String,
    Boolean,
    Enum,
}

/// Component type for numeric properties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComponentType {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float32,
    Float64,
}

/// A metadata property definition in a schema class.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyDefinition {
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub property_type: PropertyType,
    pub component_type: Option<ComponentType>,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

/// A metadata class (schema definition).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataClass {
    pub name: String,
    pub description: Option<String>,
    pub properties: HashMap<String, PropertyDefinition>,
}

/// A metadata enum type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEnum {
    pub name: String,
    pub description: Option<String>,
    pub value_type: ComponentType,
    pub values: Vec<EnumValue>,
}

/// A single enum value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnumValue {
    pub name: String,
    pub value: i64,
    pub description: Option<String>,
}

/// The full metadata schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSchema {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub classes: HashMap<String, MetadataClass>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub enums: HashMap<String, MetadataEnum>,
}

/// Property values for a specific entity (tile, feature, vertex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyTable {
    pub class_name: String,
    pub count: usize,
    pub properties: HashMap<String, PropertyValues>,
}

/// Typed property value storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "values")]
pub enum PropertyValues {
    ScalarInt(Vec<i64>),
    ScalarFloat(Vec<f64>),
    String(Vec<String>),
    Boolean(Vec<bool>),
    Vec3Float(Vec<[f64; 3]>),
}

impl PropertyValues {
    pub fn len(&self) -> usize {
        match self {
            Self::ScalarInt(v) => v.len(),
            Self::ScalarFloat(v) => v.len(),
            Self::String(v) => v.len(),
            Self::Boolean(v) => v.len(),
            Self::Vec3Float(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Query engine for structured metadata.
pub struct MetadataQuery {
    schema: MetadataSchema,
    tables: Vec<PropertyTable>,
}

/// Query filter operators.
#[derive(Debug, Clone)]
pub enum FilterOp {
    Equals(serde_json::Value),
    GreaterThan(f64),
    LessThan(f64),
    Between(f64, f64),
    Contains(String),
    In(Vec<serde_json::Value>),
}

impl MetadataQuery {
    pub fn new(schema: MetadataSchema, tables: Vec<PropertyTable>) -> Self {
        Self { schema, tables }
    }

    /// Get the schema.
    pub fn schema(&self) -> &MetadataSchema {
        &self.schema
    }

    /// Query: find all entities matching a property filter.
    pub fn filter(&self, class_name: &str, property: &str, op: &FilterOp) -> Vec<usize> {
        let mut results = Vec::new();

        for table in &self.tables {
            if table.class_name != class_name {
                continue;
            }
            if let Some(values) = table.properties.get(property) {
                match (values, op) {
                    (PropertyValues::ScalarFloat(vals), FilterOp::GreaterThan(threshold)) => {
                        for (i, v) in vals.iter().enumerate() {
                            if v > threshold {
                                results.push(i);
                            }
                        }
                    }
                    (PropertyValues::ScalarFloat(vals), FilterOp::LessThan(threshold)) => {
                        for (i, v) in vals.iter().enumerate() {
                            if v < threshold {
                                results.push(i);
                            }
                        }
                    }
                    (PropertyValues::ScalarFloat(vals), FilterOp::Between(lo, hi)) => {
                        for (i, v) in vals.iter().enumerate() {
                            if v >= lo && v <= hi {
                                results.push(i);
                            }
                        }
                    }
                    (PropertyValues::ScalarInt(vals), FilterOp::GreaterThan(threshold)) => {
                        for (i, v) in vals.iter().enumerate() {
                            if (*v as f64) > *threshold {
                                results.push(i);
                            }
                        }
                    }
                    (PropertyValues::String(vals), FilterOp::Contains(substring)) => {
                        for (i, v) in vals.iter().enumerate() {
                            if v.contains(substring.as_str()) {
                                results.push(i);
                            }
                        }
                    }
                    (PropertyValues::String(vals), FilterOp::Equals(target)) => {
                        if let Some(target_str) = target.as_str() {
                            for (i, v) in vals.iter().enumerate() {
                                if v == target_str {
                                    results.push(i);
                                }
                            }
                        }
                    }
                    (PropertyValues::Boolean(vals), FilterOp::Equals(target)) => {
                        if let Some(target_bool) = target.as_bool() {
                            for (i, v) in vals.iter().enumerate() {
                                if *v == target_bool {
                                    results.push(i);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        results
    }

    /// Get property statistics (min, max, mean) for a numeric property.
    pub fn stats(&self, class_name: &str, property: &str) -> Option<PropertyStats> {
        for table in &self.tables {
            if table.class_name != class_name {
                continue;
            }
            if let Some(PropertyValues::ScalarFloat(vals)) = table.properties.get(property) {
                if vals.is_empty() {
                    return None;
                }
                let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let sum: f64 = vals.iter().sum();
                return Some(PropertyStats {
                    min,
                    max,
                    mean: sum / vals.len() as f64,
                    count: vals.len(),
                });
            }
        }
        None
    }
}

/// Statistics for a property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub count: usize,
}

/// Create a standard building metadata schema.
pub fn building_schema() -> MetadataSchema {
    let mut properties = HashMap::new();
    properties.insert(
        "height".to_string(),
        PropertyDefinition {
            name: "height".to_string(),
            description: Some("Building height in meters".to_string()),
            property_type: PropertyType::Scalar,
            component_type: Some(ComponentType::Float32),
            required: true,
            no_data: None,
            default: None,
        },
    );
    properties.insert(
        "yearBuilt".to_string(),
        PropertyDefinition {
            name: "yearBuilt".to_string(),
            description: Some("Year of construction".to_string()),
            property_type: PropertyType::Scalar,
            component_type: Some(ComponentType::Int32),
            required: false,
            no_data: None,
            default: None,
        },
    );
    properties.insert(
        "name".to_string(),
        PropertyDefinition {
            name: "name".to_string(),
            description: Some("Building name".to_string()),
            property_type: PropertyType::String,
            component_type: None,
            required: false,
            no_data: None,
            default: None,
        },
    );
    properties.insert(
        "material".to_string(),
        PropertyDefinition {
            name: "material".to_string(),
            description: Some("Primary construction material".to_string()),
            property_type: PropertyType::String,
            component_type: None,
            required: false,
            no_data: None,
            default: None,
        },
    );

    let mut classes = HashMap::new();
    classes.insert(
        "building".to_string(),
        MetadataClass {
            name: "building".to_string(),
            description: Some("Building features".to_string()),
            properties,
        },
    );

    MetadataSchema {
        id: "tiletopia-buildings".to_string(),
        name: Some("Building Metadata".to_string()),
        description: Some("Standard building metadata schema for urban models".to_string()),
        version: Some("1.0".to_string()),
        classes,
        enums: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_query() -> MetadataQuery {
        let schema = building_schema();
        let table = PropertyTable {
            class_name: "building".to_string(),
            count: 4,
            properties: HashMap::from([
                (
                    "height".to_string(),
                    PropertyValues::ScalarFloat(vec![10.0, 25.0, 50.0, 8.0]),
                ),
                (
                    "name".to_string(),
                    PropertyValues::String(vec![
                        "Office A".to_string(),
                        "Tower B".to_string(),
                        "Skyscraper C".to_string(),
                        "House D".to_string(),
                    ]),
                ),
            ]),
        };
        MetadataQuery::new(schema, vec![table])
    }

    #[test]
    fn test_filter_greater_than() {
        let query = sample_query();
        let results = query.filter("building", "height", &FilterOp::GreaterThan(20.0));
        assert_eq!(results.len(), 2); // 25.0 and 50.0
    }

    #[test]
    fn test_filter_between() {
        let query = sample_query();
        let results = query.filter("building", "height", &FilterOp::Between(9.0, 30.0));
        assert_eq!(results.len(), 2); // 10.0 and 25.0
    }

    #[test]
    fn test_filter_string_contains() {
        let query = sample_query();
        let results = query.filter("building", "name", &FilterOp::Contains("Tower".to_string()));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], 1);
    }

    #[test]
    fn test_stats() {
        let query = sample_query();
        let stats = query.stats("building", "height").unwrap();
        assert_eq!(stats.min, 8.0);
        assert_eq!(stats.max, 50.0);
        assert!((stats.mean - 23.25).abs() < 0.01);
        assert_eq!(stats.count, 4);
    }

    #[test]
    fn test_building_schema() {
        let schema = building_schema();
        assert!(schema.classes.contains_key("building"));
        let building = &schema.classes["building"];
        assert!(building.properties.contains_key("height"));
        assert!(building.properties.contains_key("yearBuilt"));
    }
}
