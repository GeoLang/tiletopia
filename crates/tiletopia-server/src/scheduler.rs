//! Scheduled processing — cron-based terrain regeneration, DEM updates, monitoring.
//!
//! Supports recurring and one-shot jobs with priority, retry, and dependency chains.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A scheduled job definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub description: String,
    pub job_type: JobType,
    pub schedule: Schedule,
    pub status: JobStatus,
    pub priority: Priority,
    pub last_run_at: Option<DateTime<Utc>>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub run_count: u32,
    pub failure_count: u32,
    pub created_at: DateTime<Utc>,
    pub config: serde_json::Value,
}

/// Types of scheduled jobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobType {
    /// Regenerate terrain tiles for a region
    TerrainRegeneration,
    /// Download latest DEM data from source
    DemUpdate,
    /// Run anomaly detection on monitored assets
    AnomalyMonitoring,
    /// Generate change detection report
    ChangeDetection,
    /// Cleanup expired exports
    ExportCleanup,
    /// Compute storage usage metrics
    StorageMetrics,
    /// Refresh catalog data sources
    CatalogRefresh,
    /// Custom user-defined pipeline
    CustomPipeline,
}

/// Schedule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Schedule {
    /// Run once at a specific time
    OneShot(DateTime<Utc>),
    /// Cron expression (e.g., "0 0 * * *" for daily at midnight)
    Cron(String),
    /// Run every N seconds
    Interval(u64),
}

/// Job execution status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Active,
    Paused,
    Running,
    Completed,
    Failed(String),
}

/// Job priority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low,
    Normal,
    High,
    Critical,
}

/// A single job execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    pub id: Uuid,
    pub job_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<f64>,
    pub status: RunStatus,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RunStatus {
    Running,
    Success,
    Failed(String),
    Cancelled,
}

/// Scheduler state.
pub struct Scheduler {
    jobs: Arc<RwLock<Vec<ScheduledJob>>>,
    runs: Arc<RwLock<Vec<JobRun>>>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        let (jobs, runs) = Self::demo_data();
        Self {
            jobs: Arc::new(RwLock::new(jobs)),
            runs: Arc::new(RwLock::new(runs)),
        }
    }

    /// List all scheduled jobs.
    pub async fn list_jobs(&self, tenant_id: Option<Uuid>) -> Vec<ScheduledJob> {
        let jobs = self.jobs.read().await;
        match tenant_id {
            Some(id) => jobs.iter().filter(|j| j.tenant_id == id).cloned().collect(),
            None => jobs.clone(),
        }
    }

    /// Create a new scheduled job.
    pub async fn create_job(
        &self,
        tenant_id: Uuid,
        name: String,
        job_type: JobType,
        schedule: Schedule,
        config: serde_json::Value,
    ) -> ScheduledJob {
        let next_run = match &schedule {
            Schedule::OneShot(t) => Some(*t),
            Schedule::Interval(secs) => Some(Utc::now() + chrono::Duration::seconds(*secs as i64)),
            Schedule::Cron(_) => Some(Utc::now() + chrono::Duration::hours(1)), // simplified
        };

        let job = ScheduledJob {
            id: Uuid::new_v4(),
            tenant_id,
            name,
            description: String::new(),
            job_type,
            schedule,
            status: JobStatus::Active,
            priority: Priority::Normal,
            last_run_at: None,
            next_run_at: next_run,
            run_count: 0,
            failure_count: 0,
            created_at: Utc::now(),
            config,
        };
        self.jobs.write().await.push(job.clone());
        job
    }

    /// Get recent job runs.
    pub async fn recent_runs(&self, limit: usize) -> Vec<JobRun> {
        let runs = self.runs.read().await;
        runs.iter().rev().take(limit).cloned().collect()
    }

    /// Get job stats.
    pub async fn stats(&self) -> SchedulerStats {
        let jobs = self.jobs.read().await;
        let runs = self.runs.read().await;
        SchedulerStats {
            total_jobs: jobs.len(),
            active_jobs: jobs
                .iter()
                .filter(|j| j.status == JobStatus::Active)
                .count(),
            running_jobs: jobs
                .iter()
                .filter(|j| j.status == JobStatus::Running)
                .count(),
            total_runs: runs.len(),
            successful_runs: runs
                .iter()
                .filter(|r| r.status == RunStatus::Success)
                .count(),
            failed_runs: runs
                .iter()
                .filter(|r| matches!(r.status, RunStatus::Failed(_)))
                .count(),
        }
    }

    fn demo_data() -> (Vec<ScheduledJob>, Vec<JobRun>) {
        let tenant = Uuid::new_v4();
        let terrain_job_id = Uuid::new_v4();
        let monitoring_job_id = Uuid::new_v4();

        let jobs = vec![
            ScheduledJob {
                id: terrain_job_id,
                tenant_id: tenant,
                name: "Nightly Terrain Refresh".into(),
                description: "Regenerate terrain tiles from latest Copernicus DEM updates".into(),
                job_type: JobType::TerrainRegeneration,
                schedule: Schedule::Cron("0 2 * * *".into()), // 2 AM daily
                status: JobStatus::Active,
                priority: Priority::Normal,
                last_run_at: Some(Utc::now() - chrono::Duration::hours(22)),
                next_run_at: Some(Utc::now() + chrono::Duration::hours(2)),
                run_count: 28,
                failure_count: 1,
                created_at: Utc::now() - chrono::Duration::days(30),
                config: serde_json::json!({
                    "bounds": [-125.0, 24.0, -66.0, 50.0],
                    "max_zoom": 12,
                    "source": "Copernicus30"
                }),
            },
            ScheduledJob {
                id: monitoring_job_id,
                tenant_id: tenant,
                name: "Structural Monitoring".into(),
                description: "Hourly deformation check on bridge assets".into(),
                job_type: JobType::AnomalyMonitoring,
                schedule: Schedule::Interval(3600), // every hour
                status: JobStatus::Active,
                priority: Priority::High,
                last_run_at: Some(Utc::now() - chrono::Duration::minutes(45)),
                next_run_at: Some(Utc::now() + chrono::Duration::minutes(15)),
                run_count: 720,
                failure_count: 3,
                created_at: Utc::now() - chrono::Duration::days(30),
                config: serde_json::json!({
                    "asset_ids": ["bridge-001", "bridge-002"],
                    "threshold_mm": 5.0,
                    "alert_email": "ops@acme.com"
                }),
            },
            ScheduledJob {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                name: "Weekly Export Cleanup".into(),
                description: "Remove expired export packages to free storage".into(),
                job_type: JobType::ExportCleanup,
                schedule: Schedule::Cron("0 3 * * 0".into()), // Sunday 3 AM
                status: JobStatus::Active,
                priority: Priority::Low,
                last_run_at: Some(Utc::now() - chrono::Duration::days(3)),
                next_run_at: Some(Utc::now() + chrono::Duration::days(4)),
                run_count: 4,
                failure_count: 0,
                created_at: Utc::now() - chrono::Duration::days(28),
                config: serde_json::json!({"max_age_days": 7}),
            },
        ];

        let runs = vec![
            JobRun {
                id: Uuid::new_v4(),
                job_id: terrain_job_id,
                started_at: Utc::now() - chrono::Duration::hours(22),
                completed_at: Some(Utc::now() - chrono::Duration::hours(21)),
                duration_secs: Some(3247.0),
                status: RunStatus::Success,
                output: Some("Generated 1,247 terrain tiles (zoom 0-12)".into()),
            },
            JobRun {
                id: Uuid::new_v4(),
                job_id: monitoring_job_id,
                started_at: Utc::now() - chrono::Duration::minutes(45),
                completed_at: Some(Utc::now() - chrono::Duration::minutes(44)),
                duration_secs: Some(42.0),
                status: RunStatus::Success,
                output: Some("All clear: max deformation 0.3mm (threshold: 5mm)".into()),
            },
        ];

        (jobs, runs)
    }
}

/// Scheduler statistics.
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStats {
    pub total_jobs: usize,
    pub active_jobs: usize,
    pub running_jobs: usize,
    pub total_runs: usize,
    pub successful_runs: usize,
    pub failed_runs: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_scheduler_demo_data() {
        let scheduler = Scheduler::new();
        let jobs = scheduler.list_jobs(None).await;
        assert_eq!(jobs.len(), 3);
    }

    #[tokio::test]
    async fn test_create_job() {
        let scheduler = Scheduler::new();
        let job = scheduler
            .create_job(
                Uuid::new_v4(),
                "Test Job".into(),
                JobType::CatalogRefresh,
                Schedule::Interval(600),
                serde_json::json!({}),
            )
            .await;
        assert_eq!(job.status, JobStatus::Active);
        assert_eq!(job.run_count, 0);
    }

    #[tokio::test]
    async fn test_scheduler_stats() {
        let scheduler = Scheduler::new();
        let stats = scheduler.stats().await;
        assert_eq!(stats.total_jobs, 3);
        assert_eq!(stats.active_jobs, 3);
        assert_eq!(stats.total_runs, 2);
        assert_eq!(stats.successful_runs, 2);
    }
}
