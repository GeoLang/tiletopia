//! Plugin system — user-defined processing pipelines.
//!
//! Allows users to extend TileTopia with custom:
//! - Data transformations (filter, colorize, classify)
//! - Processing steps (custom tiling parameters)
//! - Visualization layers (custom renderers)
//! - Integrations (Slack, Teams, custom webhooks)
//!
//! Plugins are defined as WASM modules or JSON pipeline configs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A registered plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plugin {
    pub id: Uuid,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub plugin_type: PluginType,
    pub status: PluginStatus,
    pub config_schema: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub downloads: u64,
    pub rating: f32,
}

/// Plugin types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginType {
    /// Transform point cloud data (filter, classify, colorize)
    PointCloudTransform,
    /// Custom tiling algorithm parameters
    TilingConfig,
    /// Visualization layer/shader
    Visualization,
    /// External integration (Slack, JIRA, etc.)
    Integration,
    /// Data format importer
    Importer,
    /// Data format exporter
    Exporter,
    /// Analysis pipeline (custom computation)
    Analysis,
}

/// Plugin installation status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginStatus {
    Available,
    Installed,
    Active,
    Disabled,
    Deprecated,
}

/// A pipeline definition using plugins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub steps: Vec<PipelineStep>,
    pub created_at: DateTime<Utc>,
}

/// A step in a processing pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub plugin_id: Uuid,
    pub name: String,
    pub config: serde_json::Value,
    pub order: u32,
}

/// Plugin registry.
pub struct PluginRegistry {
    plugins: Arc<RwLock<Vec<Plugin>>>,
    pipelines: Arc<RwLock<Vec<Pipeline>>>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: Arc::new(RwLock::new(Self::marketplace_plugins())),
            pipelines: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// List all available plugins.
    pub async fn list_plugins(&self, plugin_type: Option<&PluginType>) -> Vec<Plugin> {
        let plugins = self.plugins.read().await;
        match plugin_type {
            Some(t) => plugins
                .iter()
                .filter(|p| &p.plugin_type == t)
                .cloned()
                .collect(),
            None => plugins.clone(),
        }
    }

    /// Get plugin by ID.
    pub async fn get_plugin(&self, id: Uuid) -> Option<Plugin> {
        self.plugins
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// Create a pipeline.
    pub async fn create_pipeline(
        &self,
        name: String,
        description: String,
        steps: Vec<PipelineStep>,
    ) -> Pipeline {
        let pipeline = Pipeline {
            id: Uuid::new_v4(),
            name,
            description,
            steps,
            created_at: Utc::now(),
        };
        self.pipelines.write().await.push(pipeline.clone());
        pipeline
    }

    /// List pipelines.
    pub async fn list_pipelines(&self) -> Vec<Pipeline> {
        self.pipelines.read().await.clone()
    }

    /// Simulated plugin marketplace with built-in and community plugins.
    fn marketplace_plugins() -> Vec<Plugin> {
        vec![
            Plugin {
                id: Uuid::new_v4(),
                name: "Ground Classification".into(),
                version: "2.1.0".into(),
                description:
                    "Classify ground vs non-ground points using progressive morphological filter"
                        .into(),
                author: "TileTopia".into(),
                plugin_type: PluginType::PointCloudTransform,
                status: PluginStatus::Active,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cell_size": {"type": "number", "default": 1.0},
                        "slope_threshold": {"type": "number", "default": 0.3},
                        "window_size": {"type": "integer", "default": 18}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(180),
                downloads: 12_450,
                rating: 4.8,
            },
            Plugin {
                id: Uuid::new_v4(),
                name: "Noise Filter (SOR)".into(),
                version: "1.3.0".into(),
                description: "Statistical outlier removal for noisy point clouds".into(),
                author: "TileTopia".into(),
                plugin_type: PluginType::PointCloudTransform,
                status: PluginStatus::Active,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "k_neighbors": {"type": "integer", "default": 12},
                        "std_multiplier": {"type": "number", "default": 2.0}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(120),
                downloads: 8_920,
                rating: 4.6,
            },
            Plugin {
                id: Uuid::new_v4(),
                name: "Height Colorizer".into(),
                version: "1.0.2".into(),
                description: "Colorize point clouds by elevation using configurable color ramps"
                    .into(),
                author: "Community".into(),
                plugin_type: PluginType::Visualization,
                status: PluginStatus::Available,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "min_height": {"type": "number"},
                        "max_height": {"type": "number"},
                        "color_ramp": {"type": "string", "enum": ["viridis", "terrain", "spectral"]}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(60),
                downloads: 3_200,
                rating: 4.2,
            },
            Plugin {
                id: Uuid::new_v4(),
                name: "Slack Notifications".into(),
                version: "1.1.0".into(),
                description: "Send processing notifications to Slack channels".into(),
                author: "TileTopia".into(),
                plugin_type: PluginType::Integration,
                status: PluginStatus::Installed,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "webhook_url": {"type": "string"},
                        "channel": {"type": "string"},
                        "events": {"type": "array", "items": {"type": "string"}}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(90),
                downloads: 5_600,
                rating: 4.5,
            },
            Plugin {
                id: Uuid::new_v4(),
                name: "E57 Importer".into(),
                version: "0.9.0".into(),
                description: "Import ASTM E57 scan files with full metadata preservation".into(),
                author: "Community".into(),
                plugin_type: PluginType::Importer,
                status: PluginStatus::Available,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "coordinate_system": {"type": "string"},
                        "intensity_normalize": {"type": "boolean", "default": true}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(30),
                downloads: 1_850,
                rating: 4.0,
            },
            Plugin {
                id: Uuid::new_v4(),
                name: "Volume Calculator".into(),
                version: "2.0.0".into(),
                description: "Compute cut/fill volumes between two surfaces or temporal scans"
                    .into(),
                author: "TileTopia".into(),
                plugin_type: PluginType::Analysis,
                status: PluginStatus::Active,
                config_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "reference_surface": {"type": "string"},
                        "comparison_surface": {"type": "string"},
                        "grid_resolution": {"type": "number", "default": 0.5}
                    }
                }),
                created_at: Utc::now() - chrono::Duration::days(150),
                downloads: 7_300,
                rating: 4.7,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_all_plugins() {
        let registry = PluginRegistry::new();
        let plugins = registry.list_plugins(None).await;
        assert_eq!(plugins.len(), 6);
    }

    #[tokio::test]
    async fn test_filter_by_type() {
        let registry = PluginRegistry::new();
        let transforms = registry
            .list_plugins(Some(&PluginType::PointCloudTransform))
            .await;
        assert_eq!(transforms.len(), 2);
    }

    #[tokio::test]
    async fn test_create_pipeline() {
        let registry = PluginRegistry::new();
        let plugins = registry.list_plugins(None).await;

        let pipeline = registry
            .create_pipeline(
                "LiDAR Processing".into(),
                "Standard point cloud cleanup".into(),
                vec![
                    PipelineStep {
                        plugin_id: plugins[0].id,
                        name: "Classify Ground".into(),
                        config: serde_json::json!({"cell_size": 1.0}),
                        order: 1,
                    },
                    PipelineStep {
                        plugin_id: plugins[1].id,
                        name: "Remove Outliers".into(),
                        config: serde_json::json!({"k_neighbors": 12}),
                        order: 2,
                    },
                ],
            )
            .await;

        assert_eq!(pipeline.steps.len(), 2);
        let pipelines = registry.list_pipelines().await;
        assert_eq!(pipelines.len(), 1);
    }
}
