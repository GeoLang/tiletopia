mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{PLY_FIXTURE, test_state};
use sqlx::AssertSqlSafe;
use tiletopia_server::db::{JobRecord, JobStatus};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType};
use uuid::Uuid;

const INJECTED_FAILURE: &str = "injected write failure";
const JOB_DEADLINE: Duration = Duration::from_secs(120);

async fn queued_point_cloud_job(state: &Arc<AppState>) -> JobRecord {
    let asset = Asset {
        id: Uuid::new_v4(),
        name: "cloud.ply".to_string(),
        asset_type: AssetType::PointCloud,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: String::new(),
        tags: Vec::new(),
        owner_id: None,
    };
    state.db.create_asset(&asset).await.unwrap();
    let input_dir = state.data_dir.join(asset.id.to_string()).join("input");
    std::fs::create_dir_all(&input_dir).unwrap();
    let input = input_dir.join("cloud.ply");
    std::fs::write(&input, PLY_FIXTURE).unwrap();
    state
        .job_queue
        .submit(
            asset.id,
            input.to_string_lossy().into_owned(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap()
}

async fn refuse_status_writes(state: &Arc<AppState>, table: &str, status: &str) {
    // sqlite's RAISE(ABORT) fails only the statement that fired the trigger
    let trigger = format!(
        "CREATE TRIGGER refuse_{table}_{status} BEFORE UPDATE ON {table} \
         WHEN NEW.status = '{status}' \
         BEGIN SELECT RAISE(ABORT, '{INJECTED_FAILURE}'); END"
    );
    sqlx::query(AssertSqlSafe(trigger))
        .execute(&state.db.pool)
        .await
        .unwrap();
}

async fn settled_job(state: &Arc<AppState>, job_id: Uuid) -> JobRecord {
    let worker = Arc::clone(&state.job_queue).start().await;
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let job = state.db.get_job(job_id).await.unwrap().unwrap();
        if matches!(job.status, JobStatus::Done | JobStatus::Failed) {
            worker.abort();
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} never settled, it reads as {:?}",
            job.status
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stored_asset(state: &Arc<AppState>, asset_id: Uuid) -> Asset {
    state.db.get_asset(asset_id).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_refused_done_write_leaves_the_job_failed_and_naming_the_write() {
    let state = test_state().await;
    let job = queued_point_cloud_job(&state).await;
    refuse_status_writes(&state, "jobs", "done").await;

    let settled = settled_job(&state, job.id).await;

    assert_eq!(settled.status, JobStatus::Failed);
    let error = settled.error.unwrap();
    assert!(error.contains(INJECTED_FAILURE), "{error}");
    assert!(error.contains("tiles"), "{error}");
    // the tiles were written before the job row write failed
    let asset = stored_asset(&state, job.asset_id).await;
    assert!(matches!(asset.status, AssetStatus::Ready), "{asset:?}");
    let asset_dir = state.data_dir.join(job.asset_id.to_string());
    assert!(asset_dir.join("tileset.json").exists());
}

#[tokio::test]
async fn a_refused_ready_write_fails_the_job_and_the_asset() {
    let state = test_state().await;
    let job = queued_point_cloud_job(&state).await;
    refuse_status_writes(&state, "assets", "ready").await;

    let settled = settled_job(&state, job.id).await;

    assert_eq!(settled.status, JobStatus::Failed);
    let error = settled.error.unwrap();
    assert!(error.contains(INJECTED_FAILURE), "{error}");
    let asset = stored_asset(&state, job.asset_id).await;
    assert!(matches!(asset.status, AssetStatus::Error), "{asset:?}");
}

#[tokio::test]
async fn a_refused_tiling_write_fails_the_job_before_a_tiler_runs() {
    let state = test_state().await;
    let job = queued_point_cloud_job(&state).await;
    refuse_status_writes(&state, "assets", "tiling").await;

    let settled = settled_job(&state, job.id).await;

    assert_eq!(settled.status, JobStatus::Failed);
    let error = settled.error.unwrap();
    assert!(error.contains(INJECTED_FAILURE), "{error}");
    let asset = stored_asset(&state, job.asset_id).await;
    assert!(matches!(asset.status, AssetStatus::Error), "{asset:?}");
    let asset_dir = state.data_dir.join(job.asset_id.to_string());
    assert!(!asset_dir.join("tileset.json").exists());
}

fn element_edit_previews(state: &Arc<AppState>, asset_id: Uuid) -> std::path::PathBuf {
    let path = state
        .data_dir
        .join(asset_id.to_string())
        .join("element-edits.json");
    std::fs::write(&path, br#"{"edits":[]}"#).unwrap();
    path
}

#[tokio::test]
async fn a_finished_tiling_job_drops_the_element_edit_previews() {
    let state = test_state().await;
    let job = queued_point_cloud_job(&state).await;
    let previews = element_edit_previews(&state, job.asset_id);

    let settled = settled_job(&state, job.id).await;

    assert_eq!(settled.status, JobStatus::Done, "{:?}", settled.error);
    assert!(!previews.exists());
}

#[tokio::test]
async fn a_failed_tiling_job_keeps_the_element_edit_previews() {
    let state = test_state().await;
    let job = queued_point_cloud_job(&state).await;
    let previews = element_edit_previews(&state, job.asset_id);
    refuse_status_writes(&state, "assets", "ready").await;

    let settled = settled_job(&state, job.id).await;

    assert_eq!(settled.status, JobStatus::Failed);
    assert!(previews.exists());
}
