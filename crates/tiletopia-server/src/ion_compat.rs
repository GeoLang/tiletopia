//! Cesium Ion REST API compatibility layer.
//!
//! Implements the subset of the Ion API that CesiumJS calls, so existing apps
//! using `Cesium.IonResource` can point at Tiletopia instead.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, AssetType};

// ─── Ion-format response types ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct IonAsset {
    id: i64,
    #[serde(rename = "type")]
    asset_type: String,
    name: String,
    description: String,
    status: String,
    #[serde(rename = "percentComplete")]
    percent_complete: u8,
    #[serde(rename = "dateAdded")]
    date_added: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
struct IonAssetList {
    items: Vec<IonAsset>,
}

#[derive(Debug, Serialize)]
struct IonEndpoint {
    #[serde(rename = "type")]
    endpoint_type: String,
    url: String,
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[derive(Debug, Serialize)]
struct IonToken {
    id: String,
    name: String,
    token: String,
    #[serde(rename = "dateAdded")]
    date_added: String,
    #[serde(rename = "dateModified")]
    date_modified: String,
    #[serde(rename = "dateLastUsed")]
    date_last_used: String,
    #[serde(rename = "isDefault")]
    is_default: bool,
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateIonAssetRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub asset_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateIonTokenRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

// ─── Routes ──────────────────────────────────────────────────────────────────

pub fn ion_compat_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/assets", get(list_assets).post(create_asset))
        .route("/v1/assets/{id}", get(get_asset))
        .route("/v1/assets/{id}/endpoint", get(get_endpoint))
        .route("/v1/tokens", get(list_tokens).post(create_token))
}

fn map_asset_type(asset_type: &AssetType) -> String {
    match asset_type {
        AssetType::PointCloud | AssetType::Model => "3DTILES".to_string(),
        AssetType::Terrain => "TERRAIN".to_string(),
        AssetType::Imagery => "IMAGERY".to_string(),
    }
}

fn ion_status(status: &crate::AssetStatus) -> String {
    match status {
        crate::AssetStatus::Ready => "COMPLETE".to_string(),
        crate::AssetStatus::Tiling => "IN_PROGRESS".to_string(),
        crate::AssetStatus::Uploading => "AWAITING_FILES".to_string(),
        crate::AssetStatus::Error => "ERROR".to_string(),
    }
}

fn percent_for_status(status: &crate::AssetStatus) -> u8 {
    match status {
        crate::AssetStatus::Ready => 100,
        crate::AssetStatus::Tiling => 50,
        crate::AssetStatus::Uploading => 0,
        crate::AssetStatus::Error => 0,
    }
}

/// Deterministic i64 from the first 8 bytes of a UUID (for Ion's numeric asset IDs).
fn uuid_to_ion_id(id: Uuid) -> i64 {
    let bytes = id.as_bytes();
    i64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
    .abs()
}

fn to_ion_asset(asset: &crate::Asset) -> IonAsset {
    IonAsset {
        id: uuid_to_ion_id(asset.id),
        asset_type: map_asset_type(&asset.asset_type),
        name: asset.name.clone(),
        description: asset.description.clone(),
        status: ion_status(&asset.status),
        percent_complete: percent_for_status(&asset.status),
        date_added: asset.created_at.to_rfc3339(),
        bytes: asset.size_bytes,
    }
}

fn get_base_url() -> String {
    std::env::var("TILETOPIA_ION_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string())
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn list_assets(State(state): State<Arc<AppState>>) -> Result<Json<IonAssetList>, StatusCode> {
    let assets = state
        .db
        .list_assets()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let items: Vec<IonAsset> = assets.iter().map(to_ion_asset).collect();
    Ok(Json(IonAssetList { items }))
}

async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<IonAsset>, StatusCode> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(to_ion_asset(&asset)))
}

async fn create_asset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateIonAssetRequest>,
) -> Result<(StatusCode, Json<IonAsset>), StatusCode> {
    let asset_type = match req.asset_type.as_deref() {
        Some("3DTILES") => AssetType::Model,
        Some("TERRAIN") => AssetType::Terrain,
        Some("IMAGERY") => AssetType::Imagery,
        _ => AssetType::PointCloud,
    };

    let asset = crate::Asset {
        id: Uuid::new_v4(),
        name: req.name,
        asset_type,
        status: crate::AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: req.description,
        tags: Vec::new(),
    };

    state
        .db
        .create_asset(&asset)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(to_ion_asset(&asset))))
}

async fn get_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<IonEndpoint>, StatusCode> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let base_url = get_base_url();
    let endpoint_type = map_asset_type(&asset.asset_type);

    Ok(Json(IonEndpoint {
        endpoint_type,
        url: format!("{}/api/v1/assets/{}/tileset.json", base_url, asset.id),
        access_token: String::new(),
    }))
}

async fn list_tokens() -> Json<Vec<IonToken>> {
    Json(vec![IonToken {
        id: "default".to_string(),
        name: "Default Token".to_string(),
        token: "tiletopia-default-token".to_string(),
        date_added: chrono::Utc::now().to_rfc3339(),
        date_modified: chrono::Utc::now().to_rfc3339(),
        date_last_used: chrono::Utc::now().to_rfc3339(),
        is_default: true,
        scopes: vec!["assets:read".into(), "assets:write".into()],
    }])
}

async fn create_token(Json(req): Json<CreateIonTokenRequest>) -> (StatusCode, Json<IonToken>) {
    let token = IonToken {
        id: Uuid::new_v4().to_string(),
        name: req.name,
        token: format!("tt_{}", Uuid::new_v4().to_string().replace('-', "")),
        date_added: chrono::Utc::now().to_rfc3339(),
        date_modified: chrono::Utc::now().to_rfc3339(),
        date_last_used: chrono::Utc::now().to_rfc3339(),
        is_default: false,
        scopes: if req.scopes.is_empty() {
            vec!["assets:read".into()]
        } else {
            req.scopes
        },
    };
    (StatusCode::CREATED, Json(token))
}
