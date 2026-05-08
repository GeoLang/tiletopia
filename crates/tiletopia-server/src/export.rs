//! Export system — package processed data for download.
//!
//! Supports exporting:
//! - 3D Tiles packages (zip)
//! - Point clouds (LAS/LAZ)
//! - Terrain tiles (quantized mesh bundle)
//! - Screenshots / rendered images
//! - GeoJSON extracts

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Export format options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExportFormat {
    /// 3D Tiles package (.zip)
    Tiles3DZip,
    /// Point cloud (LAS 1.4)
    Las,
    /// Point cloud (compressed LAZ)
    Laz,
    /// Terrain tiles bundle
    TerrainBundle,
    /// GeoJSON extract
    GeoJson,
    /// Rendered image (PNG)
    Png,
    /// CityGML
    CityGml,
    /// OBJ mesh
    Obj,
    /// glTF binary
    Glb,
}

/// Export job status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExportStatus {
    Queued,
    Processing,
    Ready,
    Expired,
    Failed(String),
}

/// An export job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJob {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub asset_id: Uuid,
    pub format: ExportFormat,
    pub status: ExportStatus,
    pub progress_percent: u8,
    pub file_size_bytes: Option<u64>,
    pub download_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub bounds: Option<[f64; 4]>, // Optional crop bounds
}

/// Export engine.
pub struct ExportEngine {
    jobs: Arc<RwLock<Vec<ExportJob>>>,
}

impl Default for ExportEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportEngine {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Self::demo_jobs())),
        }
    }

    /// Create a new export job.
    pub async fn create_export(
        &self,
        tenant_id: Uuid,
        asset_id: Uuid,
        format: ExportFormat,
        bounds: Option<[f64; 4]>,
    ) -> ExportJob {
        let job = ExportJob {
            id: Uuid::new_v4(),
            tenant_id,
            asset_id,
            format,
            status: ExportStatus::Queued,
            progress_percent: 0,
            file_size_bytes: None,
            download_url: None,
            created_at: Utc::now(),
            completed_at: None,
            expires_at: Some(Utc::now() + chrono::Duration::days(7)),
            bounds,
        };
        self.jobs.write().await.push(job.clone());
        job
    }

    /// List export jobs for a tenant.
    pub async fn list_exports(&self, tenant_id: Option<Uuid>) -> Vec<ExportJob> {
        let jobs = self.jobs.read().await;
        match tenant_id {
            Some(id) => jobs.iter().filter(|j| j.tenant_id == id).cloned().collect(),
            None => jobs.clone(),
        }
    }

    /// Get export job by ID.
    pub async fn get_export(&self, id: Uuid) -> Option<ExportJob> {
        self.jobs.read().await.iter().find(|j| j.id == id).cloned()
    }

    fn demo_jobs() -> Vec<ExportJob> {
        let tenant = Uuid::new_v4();
        vec![
            ExportJob {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                asset_id: Uuid::new_v4(),
                format: ExportFormat::Tiles3DZip,
                status: ExportStatus::Ready,
                progress_percent: 100,
                file_size_bytes: Some(245 * 1024 * 1024), // 245 MB
                download_url: Some("/api/v1/exports/download/abc123".into()),
                created_at: Utc::now() - chrono::Duration::hours(3),
                completed_at: Some(Utc::now() - chrono::Duration::hours(2)),
                expires_at: Some(Utc::now() + chrono::Duration::days(6)),
                bounds: None,
            },
            ExportJob {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                asset_id: Uuid::new_v4(),
                format: ExportFormat::Laz,
                status: ExportStatus::Processing,
                progress_percent: 67,
                file_size_bytes: None,
                download_url: None,
                created_at: Utc::now() - chrono::Duration::minutes(20),
                completed_at: None,
                expires_at: None,
                bounds: Some([-122.5, 37.7, -122.3, 37.9]),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_export() {
        let engine = ExportEngine::new();
        let job = engine
            .create_export(
                Uuid::new_v4(),
                Uuid::new_v4(),
                ExportFormat::GeoJson,
                Some([-122.5, 37.7, -122.3, 37.9]),
            )
            .await;
        assert_eq!(job.status, ExportStatus::Queued);
        assert_eq!(job.progress_percent, 0);
    }

    #[tokio::test]
    async fn test_demo_exports() {
        let engine = ExportEngine::new();
        let jobs = engine.list_exports(None).await;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|j| j.status == ExportStatus::Ready));
    }
}
