//! CI/CD integration — tile-on-commit, webhook hooks, and headless validation.
//!
//! Enables automated tiling pipelines triggered by git pushes, file uploads,
//! or scheduled jobs. Includes validation of 3D Tiles output.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A CI/CD pipeline definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: Uuid,
    pub name: String,
    pub trigger: PipelineTrigger,
    pub steps: Vec<PipelineStep>,
    pub enabled: bool,
    /// Environment variables for the pipeline.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// What triggers the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PipelineTrigger {
    /// Trigger on file upload.
    Upload { asset_pattern: Option<String> },
    /// Trigger on webhook (e.g., git push).
    Webhook { secret: Option<String> },
    /// Trigger on schedule.
    Schedule { cron: String },
    /// Manual trigger.
    Manual,
}

/// A step in the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PipelineStep {
    /// Ingest a file.
    Ingest {
        format: String,
        options: IngestOptions,
    },
    /// Tile the ingested data.
    Tile { options: TileOptions },
    /// Validate the output.
    Validate { checks: Vec<ValidationCheck> },
    /// Deploy/publish the tileset.
    Deploy { target: DeployTarget },
    /// Notify on completion.
    Notify {
        channel: NotifyChannel,
        message: String,
    },
}

/// Ingest options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestOptions {
    pub crs: Option<String>,
    pub classify: bool,
    pub colorize: bool,
}

/// Tiling options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileOptions {
    pub max_points_per_tile: Option<usize>,
    pub geometric_error: Option<f64>,
    pub use_implicit_tiling: bool,
}

/// Validation checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ValidationCheck {
    /// Verify tileset.json is valid.
    TilesetSchema,
    /// Verify all referenced tiles exist.
    TileIntegrity,
    /// Verify bounding volumes are correct.
    BoundingVolumes,
    /// Verify point count matches source.
    PointCount { tolerance_percent: f64 },
    /// Verify geometric error is decreasing.
    GeometricErrorHierarchy,
}

/// Where to deploy the tileset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DeployTarget {
    /// Local server (default).
    Local,
    /// S3 bucket.
    S3 { bucket: String, prefix: String },
    /// Another TileTopia instance (federation).
    Federation { peer_id: String },
}

/// Notification channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum NotifyChannel {
    Webhook { url: String },
    Log,
}

/// Pipeline execution status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineRun {
    pub id: Uuid,
    pub pipeline_id: Uuid,
    pub status: RunStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub step_results: Vec<StepResult>,
}

/// Run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Success,
    Failed,
    Cancelled,
}

/// Result of a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub step_index: usize,
    pub status: RunStatus,
    pub message: Option<String>,
    pub duration_ms: u64,
}

/// Validate a tileset.json file.
pub fn validate_tileset(tileset_json: &str) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // Parse JSON
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(tileset_json);
    let value = match parsed {
        Ok(v) => v,
        Err(e) => {
            errors.push(ValidationError {
                check: "TilesetSchema".to_string(),
                message: format!("Invalid JSON: {}", e),
                severity: ErrorSeverity::Error,
            });
            return errors;
        }
    };

    // Check required fields
    if value.get("asset").is_none() {
        errors.push(ValidationError {
            check: "TilesetSchema".to_string(),
            message: "Missing required field: asset".to_string(),
            severity: ErrorSeverity::Error,
        });
    }
    if value.get("geometricError").is_none() {
        errors.push(ValidationError {
            check: "TilesetSchema".to_string(),
            message: "Missing required field: geometricError".to_string(),
            severity: ErrorSeverity::Error,
        });
    }
    if value.get("root").is_none() {
        errors.push(ValidationError {
            check: "TilesetSchema".to_string(),
            message: "Missing required field: root".to_string(),
            severity: ErrorSeverity::Error,
        });
    }

    // Check asset version
    if let Some(asset) = value.get("asset") {
        if let Some(version) = asset.get("version") {
            let v = version.as_str().unwrap_or("");
            if v != "1.0" && v != "1.1" {
                errors.push(ValidationError {
                    check: "TilesetSchema".to_string(),
                    message: format!("Unexpected asset version: {}", v),
                    severity: ErrorSeverity::Warning,
                });
            }
        } else {
            errors.push(ValidationError {
                check: "TilesetSchema".to_string(),
                message: "Missing asset.version".to_string(),
                severity: ErrorSeverity::Error,
            });
        }
    }

    // Check root has boundingVolume
    if let Some(root) = value.get("root") {
        if root.get("boundingVolume").is_none() {
            errors.push(ValidationError {
                check: "BoundingVolumes".to_string(),
                message: "Root tile missing boundingVolume".to_string(),
                severity: ErrorSeverity::Error,
            });
        }
        if root.get("geometricError").is_none() {
            errors.push(ValidationError {
                check: "GeometricErrorHierarchy".to_string(),
                message: "Root tile missing geometricError".to_string(),
                severity: ErrorSeverity::Error,
            });
        }
    }

    errors
}

/// A validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationError {
    pub check: String,
    pub message: String,
    pub severity: ErrorSeverity,
}

/// Error severity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ErrorSeverity {
    Error,
    Warning,
    Info,
}

/// GitHub Actions workflow generator.
pub fn generate_github_actions_workflow(pipeline: &Pipeline) -> String {
    let mut yaml = String::new();
    yaml.push_str("name: TileTopia Pipeline\n");
    yaml.push_str("on:\n");

    match &pipeline.trigger {
        PipelineTrigger::Upload { .. } | PipelineTrigger::Webhook { .. } => {
            yaml.push_str("  push:\n");
            yaml.push_str("    paths:\n");
            yaml.push_str("      - 'data/**'\n");
        }
        PipelineTrigger::Schedule { cron } => {
            yaml.push_str("  schedule:\n");
            yaml.push_str(&format!("    - cron: '{}'\n", cron));
        }
        PipelineTrigger::Manual => {
            yaml.push_str("  workflow_dispatch:\n");
        }
    }

    yaml.push_str("jobs:\n");
    yaml.push_str("  tile:\n");
    yaml.push_str("    runs-on: ubuntu-latest\n");
    yaml.push_str("    steps:\n");
    yaml.push_str("      - uses: actions/checkout@v4\n");
    yaml.push_str("      - name: Install TileTopia\n");
    yaml.push_str("        run: cargo install tiletopia-cli\n");

    for (i, step) in pipeline.steps.iter().enumerate() {
        match step {
            PipelineStep::Tile { options } => {
                yaml.push_str(&format!("      - name: Step {} - Tile\n", i + 1));
                let mut cmd = "tiletopia tile --input ./data --output ./tileset".to_string();
                if options.use_implicit_tiling {
                    cmd.push_str(" --implicit");
                }
                yaml.push_str(&format!("        run: {}\n", cmd));
            }
            PipelineStep::Validate { .. } => {
                yaml.push_str(&format!("      - name: Step {} - Validate\n", i + 1));
                yaml.push_str("        run: tiletopia validate ./tileset/tileset.json\n");
            }
            _ => {}
        }
    }

    yaml
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_valid_tileset() {
        let tileset = r#"{
            "asset": { "version": "1.1" },
            "geometricError": 100.0,
            "root": {
                "boundingVolume": { "region": [0, 0, 1, 1, 0, 100] },
                "geometricError": 50.0,
                "refine": "ADD"
            }
        }"#;
        let errors = validate_tileset(tileset);
        assert_eq!(errors.len(), 0);
    }

    #[test]
    fn test_validate_missing_fields() {
        let tileset = r#"{ "asset": { "version": "1.1" } }"#;
        let errors = validate_tileset(tileset);
        assert!(errors.len() >= 2); // missing geometricError and root
    }

    #[test]
    fn test_validate_invalid_json() {
        let errors = validate_tileset("not json at all");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Invalid JSON"));
    }

    #[test]
    fn test_generate_github_actions() {
        let pipeline = Pipeline {
            id: Uuid::new_v4(),
            name: "Auto-tile".to_string(),
            trigger: PipelineTrigger::Manual,
            steps: vec![
                PipelineStep::Tile {
                    options: TileOptions {
                        max_points_per_tile: Some(50000),
                        geometric_error: None,
                        use_implicit_tiling: true,
                    },
                },
                PipelineStep::Validate {
                    checks: vec![ValidationCheck::TilesetSchema],
                },
            ],
            enabled: true,
            env: std::collections::HashMap::new(),
        };
        let yaml = generate_github_actions_workflow(&pipeline);
        assert!(yaml.contains("workflow_dispatch"));
        assert!(yaml.contains("tiletopia tile"));
        assert!(yaml.contains("--implicit"));
        assert!(yaml.contains("tiletopia validate"));
    }

    #[test]
    fn test_pipeline_run_status() {
        let run = PipelineRun {
            id: Uuid::new_v4(),
            pipeline_id: Uuid::new_v4(),
            status: RunStatus::Success,
            started_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: Some("2024-01-01T00:01:00Z".to_string()),
            step_results: vec![StepResult {
                step_index: 0,
                status: RunStatus::Success,
                message: Some("Tiled 1M points in 30s".to_string()),
                duration_ms: 30000,
            }],
        };
        assert_eq!(run.status, RunStatus::Success);
        assert_eq!(run.step_results.len(), 1);
    }
}
