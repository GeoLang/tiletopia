//! tiletopia-server: HTTP tile server and REST API
//!
//! Serves 3D Tiles tilesets, manages assets, streams tiles with
//! view-dependent LOD, and provides WebSocket for real-time data.

pub mod annotations;
pub mod api_keys;
pub mod arvr;
pub mod audit;
pub mod auth;
pub mod bim4d;
pub mod catalog;
pub mod cicd;
pub mod classification;
pub mod cloud_store;
pub mod cluster;
pub mod cog;
pub mod collaboration;
pub mod crdt;
pub mod dashboard;
pub mod demo;
pub mod encryption;
pub mod export;
pub mod federation;
pub mod flythrough;
pub mod geocoding;
pub mod geofence;
pub mod indoor;
pub mod map_tiles;
pub mod marketplace;
pub mod metering;
pub mod metrics;
pub mod mobile;
pub mod offline_export;
pub mod photogrammetry;
pub mod plugins;
pub mod premium_routes;
pub mod priority_queue;
pub mod rbac;
pub mod realtime;
pub mod reports;
pub mod retention;
pub mod routing;
pub mod scheduler;
pub mod scripting;
pub mod stac;
pub mod stories;
pub mod streaming;
pub mod temporal;
pub mod tenant;
pub mod terrain_api;
pub mod upload;
pub mod versioning;
pub mod webhook;
pub mod webhooks;
pub mod whitelabel;
pub mod workspaces;

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::Json,
    routing::get,
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
    pub realtime: realtime::RealtimeState,
    pub demo: demo::DemoState,
    pub catalog: catalog::OpenDataCatalog,
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
        .route(
            "/api/v1/assets",
            get(list_assets).post(upload::upload_asset),
        )
        .route("/api/v1/assets/{id}", get(get_asset).delete(delete_asset))
        .route("/api/v1/assets/{id}/tileset.json", get(get_tileset))
        .route("/api/v1/assets/{id}/tiles/{path}", get(get_tile))
        .route(
            "/api/v1/assets/{id}/tile",
            axum::routing::post(start_tiling),
        )
        .route(
            "/api/v1/assets/{id}/upload/init",
            axum::routing::post(streaming::init_streaming_upload),
        )
        .route(
            "/api/v1/assets/{id}/upload/chunk",
            axum::routing::post(streaming::upload_chunk),
        )
        .route(
            "/api/v1/assets/{id}/upload/complete",
            axum::routing::post(streaming::complete_streaming_upload),
        )
        .route("/api/v1/health", get(health))
        .route("/metrics", get(metrics::metrics_handler))
        .merge(demo::demo_routes())
        .merge(catalog::catalog_routes())
        .merge(terrain_api::terrain_routes())
        .merge(premium_routes::api_key_routes())
        .merge(premium_routes::metering_routes())
        .merge(premium_routes::webhook_routes())
        .merge(premium_routes::workspace_routes())
        .merge(premium_routes::export_routes())
        .merge(premium_routes::scheduler_routes())
        .merge(premium_routes::plugin_routes())
        .merge(premium_routes::mobile_routes())
        .merge(premium_routes::photogrammetry_routes())
        .merge(premium_routes::classification_routes())
        .merge(premium_routes::collaboration_routes())
        .merge(premium_routes::versioning_routes())
        .merge(premium_routes::bim4d_routes())
        .merge(premium_routes::geocoding_routes())
        .merge(premium_routes::stac_routes())
        .merge(premium_routes::indoor_routes())
        .merge(premium_routes::cog_routes())
        .merge(premium_routes::routing_routes())
        .merge(premium_routes::map_tile_routes())
        .layer(middleware::from_fn(auth::auth_middleware))
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

async fn delete_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    let mut assets = state.assets.write().await;
    let pos = assets
        .iter()
        .position(|a| a.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    assets.remove(pos);

    // Remove asset directory
    let asset_dir = state.data_dir.join(id.to_string());
    let _ = tokio::fs::remove_dir_all(&asset_dir).await;

    Ok(StatusCode::NO_CONTENT)
}

async fn get_tileset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<tiletopia_core::Tileset>, StatusCode> {
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
    // Sanitize tile path to prevent directory traversal
    if tile_path.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let file_path = state
        .data_dir
        .join(id.to_string())
        .join("tiles")
        .join(&tile_path);
    tokio::fs::read(&file_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
}

/// Start a tiling job for an uploaded asset.
async fn start_tiling(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    // Find the asset and update status
    {
        let mut assets = state.assets.write().await;
        let asset = assets
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(StatusCode::NOT_FOUND)?;
        asset.status = AssetStatus::Tiling;
    }

    // Spawn tiling job in background
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let asset_dir = state_clone.data_dir.join(id.to_string());
        let input_dir = asset_dir.join("input");

        // Find the input file
        let mut entries = match tokio::fs::read_dir(&input_dir).await {
            Ok(e) => e,
            Err(_) => return,
        };
        let input_path = match entries.next_entry().await {
            Ok(Some(entry)) => entry.path(),
            _ => return,
        };

        // Run tiling
        let result = tokio::task::spawn_blocking(
            move || -> Result<tiletopia_core::octree::OctreeStats, String> {
                let points =
                    tiletopia_ingest::read_point_cloud(&input_path).map_err(|e| e.to_string())?;
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
                tiletopia_core::tileset::tile_point_cloud(octree_points, &asset_dir, &config)
                    .map_err(|e| e.to_string())
            },
        )
        .await;

        let mut assets = state_clone.assets.write().await;
        if let Some(asset) = assets.iter_mut().find(|a| a.id == id) {
            match result {
                Ok(Ok(stats)) => {
                    asset.status = AssetStatus::Ready;
                    asset.tile_count = stats.total_nodes as u64;
                    tracing::info!("Tiling complete for {}: {} nodes", id, stats.total_nodes);
                    tracing::info!("metric: tiling_jobs_completed");
                }
                _ => {
                    asset.status = AssetStatus::Error;
                    tracing::error!("Tiling failed for {}", id);
                    tracing::info!("metric: tiling_jobs_failed");
                }
            }
        }
    });

    Ok(StatusCode::ACCEPTED)
}
