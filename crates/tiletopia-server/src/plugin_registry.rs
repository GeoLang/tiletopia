//! Plugin registry — persistent plugin management with SQLite storage.
//!
//! Manages installed plugins with CRUD operations and enable/disable toggles.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::AppState;

/// Plugin manifest describing a plugin's identity and capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub license: String,
    pub entry_point: String,
    pub capabilities: Vec<PluginCapability>,
    pub config_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginCapability {
    Ingest,
    Transform,
    Export,
    Visualize,
    Api,
}

/// A plugin that has been installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub installed_at: DateTime<Utc>,
    pub enabled: bool,
    pub config: serde_json::Value,
}

// ─── Request types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InstallPluginRequest {
    pub manifest: PluginManifest,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_config() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Deserialize)]
pub struct UpdatePluginConfigRequest {
    pub config: serde_json::Value,
}

// ─── Routes ──────────────────────────────────────────────────────────────────

pub fn plugin_registry_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/plugins/registry",
            get(list_plugins).post(install_plugin),
        )
        .route(
            "/api/v1/plugins/registry/{id}",
            get(get_plugin).delete(uninstall_plugin),
        )
        .route(
            "/api/v1/plugins/registry/{id}/config",
            axum::routing::put(update_config),
        )
        .route(
            "/api/v1/plugins/registry/{id}/enable",
            axum::routing::post(enable_plugin),
        )
        .route(
            "/api/v1/plugins/registry/{id}/disable",
            axum::routing::post(disable_plugin),
        )
}

async fn list_plugins(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<InstalledPlugin>>, StatusCode> {
    state
        .db
        .list_plugins()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn install_plugin(
    State(state): State<Arc<AppState>>,
    Json(req): Json<InstallPluginRequest>,
) -> Result<(StatusCode, Json<InstalledPlugin>), StatusCode> {
    let plugin = InstalledPlugin {
        manifest: req.manifest,
        installed_at: Utc::now(),
        enabled: true,
        config: req.config,
    };

    state
        .db
        .install_plugin(&plugin)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(plugin)))
}

async fn get_plugin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<InstalledPlugin>, StatusCode> {
    state
        .db
        .get_plugin(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn update_config(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePluginConfigRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .db
        .update_plugin_config(&id, &req.config)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "status": "updated" })))
}

async fn uninstall_plugin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    state
        .db
        .delete_plugin(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn enable_plugin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .db
        .set_plugin_enabled(&id, true)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "status": "enabled" })))
}

async fn disable_plugin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    state
        .db
        .set_plugin_enabled(&id, false)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "status": "disabled" })))
}
