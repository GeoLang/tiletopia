//! Cesium Ion REST API compatibility layer.
//!
//! Implements the subset of the Ion API that CesiumJS calls, so existing apps
//! using `Cesium.IonResource` can point at Tiletopia instead.

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    middleware,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, AssetType, terrain_bundle, users};

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
    /// CesiumJS maps this without checking it is there when it builds the
    /// credits for a provider, so an endpoint missing it throws before the
    /// first tile is asked for.
    attributions: Vec<serde_json::Value>,
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

/// Anonymous Ion-compat reads. GET-only asset/token discovery that public
/// CesiumJS clients use to resolve assets; served without auth like the native
/// tile-data GETs. Kept separate from the mutating routes so a POST can never
/// ride the read exemption.
pub fn ion_compat_read_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/assets", get(list_assets))
        .route("/v1/assets/{id}", get(get_asset))
        .route("/v1/assets/{id}/endpoint", get(get_endpoint))
        .route("/v1/tokens", get(list_tokens))
}

/// Mutating Ion-compat routes, behind the auth layer. Asset creation is an
/// Edit-tier write (editor or admin); token minting issues a bearer credential
/// so it is admin-only, matching how the rest of tiletopia gates privileged
/// management behind require_admin.
pub fn ion_compat_write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/assets",
            post(create_asset).layer(middleware::from_fn(users::require_editor)),
        )
        .route(
            "/v1/tokens",
            post(create_token).layer(middleware::from_fn(users::require_admin)),
        )
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

fn to_ion_asset(asset: &crate::Asset, ion_id: i64) -> IonAsset {
    IonAsset {
        id: ion_id,
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

/// An Ion client only ever has the number it read off the asset list, and
/// `IonImageryProvider.fromAssetId` will not even accept anything else. Uuids
/// still resolve, so a link written against the native asset id keeps working.
async fn resolve_asset(
    state: &AppState,
    id: &str,
) -> Result<Option<(crate::Asset, i64)>, sqlx::Error> {
    if let Ok(ion_id) = id.parse::<i64>() {
        return state.db.get_asset_by_ion_id(ion_id).await;
    }
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(None);
    };
    state.db.get_asset_with_ion_id(uuid).await
}

// ─── Handlers ────────────────────────────────────────────────────────────────

async fn list_assets(State(state): State<Arc<AppState>>) -> Result<Json<IonAssetList>, StatusCode> {
    let assets = state
        .db
        .list_assets_with_ion_ids()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let items: Vec<IonAsset> = assets
        .iter()
        .map(|(asset, ion_id)| to_ion_asset(asset, *ion_id))
        .collect();
    Ok(Json(IonAssetList { items }))
}

async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<IonAsset>, StatusCode> {
    let (asset, ion_id) = resolve_asset(&state, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(to_ion_asset(&asset, ion_id)))
}

async fn create_asset(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateIonAssetRequest>,
) -> Result<(StatusCode, Json<IonAsset>), StatusCode> {
    // the route sits behind require_editor, so a valid token is always present
    let owner_id = users::claims_from_headers(&headers)?.sub;

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
        owner_id: Some(owner_id),
    };

    let ion_id = state
        .db
        .create_asset(&asset)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(to_ion_asset(&asset, ion_id))))
}

/// Status plus a message, because the only thing a CesiumJS developer sees when
/// this call fails is the response body in the console.
type EndpointError = (StatusCode, Json<serde_json::Value>);

fn endpoint_error(status: StatusCode, message: impl Into<String>) -> EndpointError {
    (status, Json(json!({ "message": message.into() })))
}

async fn get_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<IonEndpoint>, EndpointError> {
    let (asset, _) = resolve_asset(&state, &id)
        .await
        .map_err(|_| endpoint_error(StatusCode::INTERNAL_SERVER_ERROR, "asset lookup failed"))?
        .ok_or_else(|| endpoint_error(StatusCode::NOT_FOUND, format!("no asset {id}")))?;

    let base_url = get_base_url();
    let url = match asset.asset_type {
        // CesiumTerrainProvider appends layer.json to whatever url arrives
        // here, so it has to be the directory of a quantized-mesh bundle. A
        // tileset.json url does not even fail loudly: the 404 on layer.json is
        // read as a pre-metadata heightmap layer, and every tile 404s after.
        AssetType::Terrain => {
            let bundle = asset.id.to_string();
            if !terrain_bundle::bundle_exists(&state, &bundle).await {
                return Err(endpoint_error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "terrain asset {bundle} has no terrain to serve: put a quantized-mesh bundle at <data-dir>/terrain_bundles/{bundle}/"
                    ),
                ));
            }
            format!("{base_url}/api/v1/terrain/bundles/{bundle}/")
        }
        // nothing here serves image tiles, and cesium reads an IMAGERY url as a
        // TMS root, so a tileset.json would send it after tilemapresource.xml
        AssetType::Imagery => {
            return Err(endpoint_error(
                StatusCode::NOT_IMPLEMENTED,
                format!(
                    "asset {id} is imagery, and tiletopia has no imagery tiling: there is no tile pyramid for an imagery provider to read"
                ),
            ));
        }
        _ => format!("{base_url}/api/v1/assets/{}/tileset.json", asset.id),
    };

    Ok(Json(IonEndpoint {
        endpoint_type: map_asset_type(&asset.asset_type),
        url,
        access_token: String::new(),
        attributions: Vec::new(),
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
