//! Admin dashboard API — statistics, health, user management.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::JobRecord;
use crate::users::User;
use crate::{AppState, Asset};

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub total_assets: u64,
    pub total_users: u64,
    pub total_storage_bytes: u64,
    pub total_tiles: u64,
    pub active_jobs: u64,
    pub recent_uploads: Vec<Asset>,
    pub storage_by_type: HashMap<String, u64>,
}

#[derive(Debug, Serialize)]
pub struct SystemHealth {
    pub uptime_seconds: u64,
    pub memory_used_bytes: u64,
    pub cpu_usage_percent: f32,
    pub disk_free_bytes: u64,
    pub version: String,
}

pub async fn get_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<DashboardStats>, StatusCode> {
    let total_assets = state
        .db
        .count_assets()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_users = state
        .db
        .count_users()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_storage_bytes = state
        .db
        .total_storage_bytes()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let recent_uploads = state
        .db
        .recent_assets(10)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Count active (running) jobs
    let jobs = state
        .db
        .list_recent_jobs(100)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let active_jobs = jobs
        .iter()
        .filter(|j| j.status == crate::db::JobStatus::Running)
        .count() as u64;
    let total_tiles: u64 = recent_uploads.iter().map(|a| a.tile_count).sum();

    Ok(Json(DashboardStats {
        total_assets,
        total_users,
        total_storage_bytes,
        total_tiles,
        active_jobs,
        recent_uploads,
        storage_by_type: HashMap::new(),
    }))
}

pub async fn get_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SystemHealth>, StatusCode> {
    let uptime = state.started_at.elapsed().as_secs();

    // Best-effort system stats
    let memory_used_bytes = {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/self/statm")
                .ok()
                .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|pages| pages * 4096)
                .unwrap_or(0)
        }
        #[cfg(not(target_os = "linux"))]
        {
            0u64
        }
    };

    Ok(Json(SystemHealth {
        uptime_seconds: uptime,
        memory_used_bytes,
        cpu_usage_percent: 0.0,
        disk_free_bytes: 0,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

pub async fn list_users(State(state): State<Arc<AppState>>) -> Result<Json<Vec<User>>, StatusCode> {
    let users = state
        .db
        .list_users()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(users))
}

pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<JobRecord>>, StatusCode> {
    let jobs = state
        .db
        .list_recent_jobs(50)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(jobs))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    state
        .db
        .delete_user(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
