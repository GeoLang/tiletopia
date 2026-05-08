//! 3D annotation layers.
//!
//! Server-side persistence of user-drawn annotations in 3D space.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Annotation geometry types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AnnotationGeometry {
    /// A point marker.
    Point { position: [f64; 3] },
    /// A polyline.
    Polyline { positions: Vec<[f64; 3]> },
    /// A polygon (closed ring).
    Polygon { positions: Vec<[f64; 3]> },
    /// A 3D box (center + half-extents).
    Box {
        center: [f64; 3],
        half_extents: [f64; 3],
    },
    /// A sphere.
    Sphere { center: [f64; 3], radius: f64 },
}

/// Visual style for annotations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationStyle {
    #[serde(default = "default_color")]
    pub color: String,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default = "default_line_width")]
    pub line_width: f32,
    #[serde(default)]
    pub fill: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

fn default_color() -> String {
    "#ff0000".into()
}
fn default_opacity() -> f32 {
    1.0
}
fn default_line_width() -> f32 {
    2.0
}

/// A single annotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub layer_id: Uuid,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub geometry: AnnotationGeometry,
    pub style: AnnotationStyle,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub properties: serde_json::Value,
}

/// An annotation layer groups related annotations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationLayer {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub name: String,
    pub visible: bool,
    pub annotations: Vec<Annotation>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl AnnotationLayer {
    pub fn new(asset_id: Uuid, name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            asset_id,
            name,
            visible: true,
            annotations: Vec::new(),
            created_at: chrono::Utc::now(),
        }
    }

    pub fn add_annotation(&mut self, mut annotation: Annotation) -> Uuid {
        annotation.id = Uuid::new_v4();
        annotation.layer_id = self.id;
        annotation.created_at = chrono::Utc::now();
        let id = annotation.id;
        self.annotations.push(annotation);
        id
    }

    pub fn remove_annotation(&mut self, id: Uuid) -> bool {
        let len = self.annotations.len();
        self.annotations.retain(|a| a.id != id);
        self.annotations.len() < len
    }

    pub fn find_in_radius(&self, center: [f64; 3], radius: f64) -> Vec<&Annotation> {
        self.annotations
            .iter()
            .filter(|a| {
                let pos = match &a.geometry {
                    AnnotationGeometry::Point { position } => *position,
                    AnnotationGeometry::Sphere { center, .. } => *center,
                    AnnotationGeometry::Box { center, .. } => *center,
                    AnnotationGeometry::Polyline { positions }
                    | AnnotationGeometry::Polygon { positions } => {
                        if positions.is_empty() {
                            return false;
                        }
                        positions[0]
                    }
                };
                let dx = pos[0] - center[0];
                let dy = pos[1] - center[1];
                let dz = pos[2] - center[2];
                (dx * dx + dy * dy + dz * dz).sqrt() <= radius
            })
            .collect()
    }
}

/// In-memory annotation store.
#[derive(Debug, Default)]
pub struct AnnotationStore {
    pub layers: Vec<AnnotationLayer>,
}

impl AnnotationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_layer(&mut self, asset_id: Uuid, name: String) -> Uuid {
        let layer = AnnotationLayer::new(asset_id, name);
        let id = layer.id;
        self.layers.push(layer);
        id
    }

    pub fn get_layers_for_asset(&self, asset_id: Uuid) -> Vec<&AnnotationLayer> {
        self.layers
            .iter()
            .filter(|l| l.asset_id == asset_id)
            .collect()
    }

    pub fn get_layer_mut(&mut self, layer_id: Uuid) -> Option<&mut AnnotationLayer> {
        self.layers.iter_mut().find(|l| l.id == layer_id)
    }

    /// Save all annotations to a JSON file.
    pub fn save_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(&self.layers).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Load annotations from a JSON file.
    pub fn load_from_file(path: &std::path::Path) -> std::io::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        let layers: Vec<AnnotationLayer> = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self { layers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annotation_layer() {
        let asset_id = Uuid::new_v4();
        let mut layer = AnnotationLayer::new(asset_id, "Notes".into());
        let ann = Annotation {
            id: Uuid::nil(),
            asset_id,
            layer_id: Uuid::nil(),
            label: "Crack".into(),
            description: Some("Structural crack".into()),
            geometry: AnnotationGeometry::Point {
                position: [1.0, 2.0, 3.0],
            },
            style: AnnotationStyle {
                color: "#ff0000".into(),
                opacity: 1.0,
                line_width: 2.0,
                fill: false,
                icon: None,
            },
            created_by: "user1".into(),
            created_at: chrono::Utc::now(),
            updated_at: None,
            properties: serde_json::json!({}),
        };
        let id = layer.add_annotation(ann);
        assert_eq!(layer.annotations.len(), 1);
        assert!(layer.remove_annotation(id));
        assert!(layer.annotations.is_empty());
    }

    #[test]
    fn test_find_in_radius() {
        let asset_id = Uuid::new_v4();
        let mut layer = AnnotationLayer::new(asset_id, "Test".into());
        for i in 0..10 {
            let ann = Annotation {
                id: Uuid::nil(),
                asset_id,
                layer_id: Uuid::nil(),
                label: format!("Point {}", i),
                description: None,
                geometry: AnnotationGeometry::Point {
                    position: [i as f64, 0.0, 0.0],
                },
                style: AnnotationStyle {
                    color: "#00ff00".into(),
                    opacity: 1.0,
                    line_width: 1.0,
                    fill: false,
                    icon: None,
                },
                created_by: "test".into(),
                created_at: chrono::Utc::now(),
                updated_at: None,
                properties: serde_json::json!({}),
            };
            layer.add_annotation(ann);
        }
        let found = layer.find_in_radius([0.0, 0.0, 0.0], 3.5);
        assert_eq!(found.len(), 4); // 0, 1, 2, 3
    }
}
