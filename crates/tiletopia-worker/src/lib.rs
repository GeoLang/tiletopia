//! tiletopia-worker: async tiling pipeline
//!
//! Background job processing for converting source data into 3D Tiles.
//! Supports progress tracking, cancellation, and job persistence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Job status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// A tiling job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilingJob {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub status: JobStatus,
    pub progress: f32,
    pub input_path: String,
    pub output_format: OutputFormat,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub points_processed: u64,
    pub tiles_written: u64,
}

/// Output format for tiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// 3D Tiles 1.1 (GLB content)
    Tiles3d,
    /// Quantized mesh terrain
    QuantizedMesh,
}

impl TilingJob {
    pub fn new(asset_id: Uuid, input_path: String, output_format: OutputFormat) -> Self {
        Self {
            id: Uuid::new_v4(),
            asset_id,
            status: JobStatus::Queued,
            progress: 0.0,
            input_path,
            output_format,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
        }
    }
}
