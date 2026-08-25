//! Admin dashboard API — statistics, health, user management.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::JobRecord;
use crate::users::{User, UserRole};
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

#[derive(Debug, Deserialize)]
pub struct SetRoleRequest {
    pub role: UserRole,
}

/// Admin-only: set a user's role. Behind `require_admin`, so a viewer can never
/// promote itself; the only self-service escalation path stays closed.
pub async fn set_user_role(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetRoleRequest>,
) -> Result<Json<User>, StatusCode> {
    let mut user = state
        .db
        .get_user(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    user.role = req.role;
    state
        .db
        .update_user(&user)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(user))
}

#[derive(Debug, Deserialize)]
pub struct SetOrgRequest {
    pub org_id: Option<Uuid>,
}

/// Admin-only: put a user in an organization, or take them out with `null`.
/// The organization must exist.
pub async fn set_user_org(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetOrgRequest>,
) -> Result<Json<User>, StatusCode> {
    let mut user = state
        .db
        .get_user(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some(org_id) = req.org_id {
        state
            .db
            .get_org(org_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
    }

    user.org_id = req.org_id;
    state
        .db
        .update_user(&user)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(user))
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
