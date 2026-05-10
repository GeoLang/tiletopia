//! Async job queue for tiling operations.

use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::{Database, JobRecord, JobStatus};
use tiletopia_store::TileStore;

pub struct JobQueue {
    db: Arc<Database>,
    data_dir: PathBuf,
    #[allow(dead_code)]
    store: Arc<dyn TileStore>,
}

impl JobQueue {
    pub fn new(db: Arc<Database>, data_dir: PathBuf, store: Arc<dyn TileStore>) -> Self {
        Self {
            db,
            data_dir,
            store,
        }
    }

    /// Start the background worker loop.
    pub async fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.db.next_queued_job().await {
                    Ok(Some(mut job)) => {
                        job.status = JobStatus::Running;
                        job.started_at = Some(chrono::Utc::now());
                        if let Err(e) = self.db.update_job(&job).await {
                            tracing::error!("Failed to update job {}: {}", job.id, e);
                            continue;
                        }

                        // Update asset status to Tiling
                        if let Ok(Some(mut asset)) = self.db.get_asset(job.asset_id).await {
                            asset.status = crate::AssetStatus::Tiling;
                            let _ = self.db.update_asset(&asset).await;
                        }

                        let data_dir = self.data_dir.clone();
                        let asset_id = job.asset_id;
                        let input_path = PathBuf::from(&job.input_path);

                        let result = tokio::task::spawn_blocking(move || {
                            let asset_dir = data_dir.join(asset_id.to_string());
                            let points = tiletopia_ingest::read_point_cloud(&input_path)
                                .map_err(|e| e.to_string())?;
                            let octree_points: Vec<tiletopia_core::octree::OctreePoint> = points
                                .into_iter()
                                .map(|p| tiletopia_core::octree::OctreePoint {
                                    position: [p.x, p.y, p.z],
                                    color: [p.r, p.g, p.b],
                                    intensity: p.intensity,
                                    classification: p.classification,
                                })
                                .collect();

                            let config = tiletopia_core::tileset::TilingConfig::default();
                            tiletopia_core::tileset::tile_point_cloud(
                                octree_points,
                                &asset_dir,
                                &config,
                            )
                            .map_err(|e| e.to_string())
                        })
                        .await;

                        match result {
                            Ok(Ok(stats)) => {
                                job.status = JobStatus::Done;
                                job.progress = 1.0;
                                job.completed_at = Some(chrono::Utc::now());
                                job.tiles_written = stats.total_nodes as u64;
                                let _ = self.db.update_job(&job).await;

                                if let Ok(Some(mut asset)) = self.db.get_asset(asset_id).await {
                                    asset.status = crate::AssetStatus::Ready;
                                    asset.tile_count = stats.total_nodes as u64;
                                    let _ = self.db.update_asset(&asset).await;
                                }

                                tracing::info!(
                                    "Job {} completed: {} nodes",
                                    job.id,
                                    stats.total_nodes
                                );
                            }
                            Ok(Err(e)) => {
                                job.status = JobStatus::Failed;
                                job.error = Some(e.clone());
                                job.completed_at = Some(chrono::Utc::now());
                                let _ = self.db.update_job(&job).await;

                                if let Ok(Some(mut asset)) = self.db.get_asset(asset_id).await {
                                    asset.status = crate::AssetStatus::Error;
                                    let _ = self.db.update_asset(&asset).await;
                                }

                                tracing::error!("Job {} failed: {}", job.id, e);
                            }
                            Err(e) => {
                                job.status = JobStatus::Failed;
                                job.error = Some(e.to_string());
                                job.completed_at = Some(chrono::Utc::now());
                                let _ = self.db.update_job(&job).await;

                                if let Ok(Some(mut asset)) = self.db.get_asset(asset_id).await {
                                    asset.status = crate::AssetStatus::Error;
                                    let _ = self.db.update_asset(&asset).await;
                                }

                                tracing::error!("Job {} panicked: {}", job.id, e);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Failed to poll job queue: {}", e);
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        })
    }

    /// Submit a new job.
    pub async fn submit(
        &self,
        asset_id: Uuid,
        input_path: String,
    ) -> Result<JobRecord, sqlx::Error> {
        let job = JobRecord {
            id: Uuid::new_v4(),
            asset_id,
            status: JobStatus::Queued,
            progress: 0.0,
            input_path,
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
        };
        self.db.create_job(&job).await?;
        Ok(job)
    }

    /// Get job status.
    pub async fn get_status(&self, job_id: Uuid) -> Result<Option<JobRecord>, sqlx::Error> {
        self.db.get_job(job_id).await
    }
}
