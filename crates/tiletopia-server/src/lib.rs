//! tiletopia-server: HTTP tile server and REST API
//!
//! Serves 3D Tiles tilesets, manages assets, streams tiles with
//! view-dependent LOD, and provides WebSocket for real-time data.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

/// Server application state.
pub struct AppState {
    pub assets: RwLock<Vec<Asset>>,
    pub data_dir: std::path::PathBuf,
}

/// A managed geospatial asset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: Uuid,
    pub name: String,
    pub asset_type: AssetType,
    pub status: AssetStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub tile_count: u64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetType {
    PointCloud,
    Terrain,
    Model,
    Imagery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetStatus {
    Uploading,
    Tiling,
    Ready,
    Error,
}

/// Build the Axum router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/assets", get(list_assets).post(create_asset))
        .route("/api/v1/assets/{id}", get(get_asset))
        .route("/api/v1/assets/{id}/tileset.json", get(get_tileset))
        .route("/api/v1/assets/{id}/tiles/{path}", get(get_tile))
        .route("/api/v1/health", get(health))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn list_assets(State(state): State<Arc<AppState>>) -> Json<Vec<Asset>> {
    let assets = state.assets.read().await;
    Json(assets.clone())
}

async fn create_asset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAssetRequest>,
) -> (StatusCode, Json<Asset>) {
    let asset = Asset {
        id: Uuid::new_v4(),
        name: req.name,
        asset_type: req.asset_type,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
    };
    state.assets.write().await.push(asset.clone());
    (StatusCode::CREATED, Json(asset))
}

#[derive(Deserialize)]
struct CreateAssetRequest {
    name: String,
    asset_type: AssetType,
}

async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Asset>, StatusCode> {
    let assets = state.assets.read().await;
    assets
        .iter()
        .find(|a| a.id == id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_tileset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<tiletopia_core::Tileset>, StatusCode> {
    // Serve tileset.json from storage
    let tileset_path = state.data_dir.join(id.to_string()).join("tileset.json");
    let data = tokio::fs::read_to_string(&tileset_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let tileset: tiletopia_core::Tileset =
        serde_json::from_str(&data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(tileset))
}

async fn get_tile(
    State(state): State<Arc<AppState>>,
    Path((id, tile_path)): Path<(Uuid, String)>,
) -> Result<Vec<u8>, StatusCode> {
    let file_path = state.data_dir.join(id.to_string()).join(&tile_path);
    tokio::fs::read(&file_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
}
