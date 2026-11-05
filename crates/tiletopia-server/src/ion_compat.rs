//! Cesium Ion REST API compatibility layer.
//!
//! Implements the subset of the Ion API that CesiumJS calls, so existing apps
//! using `Cesium.IonResource` can point at Tiletopia instead.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::Json,
    routing::{get, post},
};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, AssetType, czml, global_terrain, imagery, ion_upload, may_view_asset, terrain_bundle,
    users,
};

// ─── Ion-format response types ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct IonAsset {
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
    #[serde(rename = "externalType", skip_serializing_if = "Option::is_none")]
    external_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<serde_json::Value>,
}

// ─── Routes ──────────────────────────────────────────────────────────────────

pub fn ion_compat_read_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/assets", get(list_assets))
        .route("/v1/assets/{id}", get(get_asset))
        .route("/v1/assets/{id}/endpoint", get(get_endpoint))
}

/// Mutating Ion-compat routes, behind the auth layer. Asset creation is an
/// Edit-tier write (editor or admin).
pub fn ion_compat_write_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/assets",
            post(ion_upload::create_asset).layer(middleware::from_fn(users::require_editor)),
        )
        .route(
            "/v1/assets/{id}/uploadComplete",
            post(ion_upload::upload_complete).layer(middleware::from_fn(users::require_editor)),
        )
}

fn map_asset_type(asset_type: &AssetType) -> String {
    match asset_type {
        AssetType::PointCloud | AssetType::Model | AssetType::Vector => "3DTILES".to_string(),
        AssetType::Terrain => "TERRAIN".to_string(),
        AssetType::Imagery => "IMAGERY".to_string(),
        AssetType::Czml => "CZML".to_string(),
    }
}

fn ion_status(status: &crate::AssetStatus, awaiting_files: bool) -> String {
    match status {
        crate::AssetStatus::Ready => "COMPLETE".to_string(),
        crate::AssetStatus::Tiling => "IN_PROGRESS".to_string(),
        crate::AssetStatus::Uploading if awaiting_files => "AWAITING_FILES".to_string(),
        crate::AssetStatus::Uploading => "NOT_STARTED".to_string(),
        crate::AssetStatus::Error => "ERROR".to_string(),
    }
}

fn percent_complete(status: &crate::AssetStatus, running_job_progress: Option<f64>) -> u8 {
    match status {
        crate::AssetStatus::Ready => 100,
        crate::AssetStatus::Tiling => {
            running_job_progress.map_or(0, |progress| (progress * 100.0) as u8)
        }
        crate::AssetStatus::Uploading => 0,
        crate::AssetStatus::Error => 0,
    }
}

pub(crate) fn to_ion_asset(
    asset: &crate::Asset,
    ion_id: i64,
    awaiting_files: bool,
    running_job_progress: Option<f64>,
) -> IonAsset {
    IonAsset {
        id: ion_id,
        asset_type: map_asset_type(&asset.asset_type),
        name: asset.name.clone(),
        description: asset.description.clone(),
        status: ion_status(&asset.status, awaiting_files),
        percent_complete: percent_complete(&asset.status, running_job_progress),
        date_added: asset.created_at.to_rfc3339(),
        bytes: asset.size_bytes,
    }
}

pub const ION_BASE_URL_VAR: &str = "TILETOPIA_ION_BASE_URL";

pub fn ion_base_url_from_env() -> Result<String, String> {
    checked_ion_base_url(std::env::var(ION_BASE_URL_VAR).ok().as_deref())
}

fn checked_ion_base_url(raw: Option<&str>) -> Result<String, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Err(format!(
            "{ION_BASE_URL_VAR} is not set, set it to the http or https origin clients reach this server at, such as https://tiles.example.com"
        ));
    };
    let url = reqwest::Url::parse(raw).map_err(|error| {
        format!(
            "{ION_BASE_URL_VAR} is {raw:?}: {error}, expected an http or https origin such as https://tiles.example.com"
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "{ION_BASE_URL_VAR} is {raw:?}, expected an http or https origin such as https://tiles.example.com"
        ));
    }
    let origin = url.origin().ascii_serialization();
    if raw != origin {
        return Err(format!(
            "{ION_BASE_URL_VAR} is {raw:?}, expected exactly {origin:?}: a path, query or trailing slash breaks S3 Signature V4 on the Ion upload route and the URLs joined to this value"
        ));
    }
    Ok(origin)
}

/// An Ion client only ever has the number it read off the asset list, and
/// `IonImageryProvider.fromAssetId` will not even accept anything else. Uuids
/// still resolve, so a link written against the native asset id keeps working.
pub(crate) async fn resolve_asset(
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

async fn list_assets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<IonAssetList>, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let assets = state
        .db
        .list_assets_with_ion_ids()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let awaiting_files = state
        .db
        .asset_ids_awaiting_files()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let running_job_progress = state
        .db
        .running_job_progress()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let items: Vec<IonAsset> = assets
        .iter()
        .filter(|(asset, _)| may_view_asset(&claims, asset.owner_id.as_deref()))
        .map(|(asset, ion_id)| {
            to_ion_asset(
                asset,
                *ion_id,
                awaiting_files.contains(&asset.id),
                running_job_progress.get(&asset.id).copied(),
            )
        })
        .collect();
    Ok(Json(IonAssetList { items }))
}

async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<IonAsset>, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let (asset, ion_id) = resolve_asset(&state, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter(|(asset, _)| may_view_asset(&claims, asset.owner_id.as_deref()))
        .ok_or(StatusCode::NOT_FOUND)?;
    let awaiting_files = state
        .db
        .asset_ids_awaiting_files()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let running_job_progress = state
        .db
        .running_job_progress()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(to_ion_asset(
        &asset,
        ion_id,
        awaiting_files.contains(&asset.id),
        running_job_progress.get(&asset.id).copied(),
    )))
}

/// Status plus a message, because the only thing a CesiumJS developer sees when
/// this call fails is the response body in the console.
pub(crate) type IonError = (StatusCode, Json<serde_json::Value>);

pub(crate) fn ion_error(status: StatusCode, message: impl Into<String>) -> IonError {
    (status, Json(json!({ "message": message.into() })))
}

async fn get_endpoint(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<IonEndpoint>, IonError> {
    if state.global_terrain.is_named_by_ion_id(&id) {
        return global_terrain_endpoint(&state);
    }
    let claims = users::claims_from_headers(&headers).map_err(|status| {
        ion_error(
            status,
            "send the token in an Authorization: Bearer header, a query string is never a credential",
        )
    })?;
    let (asset, _) = resolve_asset(&state, &id)
        .await
        .map_err(|_| ion_error(StatusCode::INTERNAL_SERVER_ERROR, "asset lookup failed"))?
        .filter(|(asset, _)| may_view_asset(&claims, asset.owner_id.as_deref()))
        .ok_or_else(|| ion_error(StatusCode::NOT_FOUND, format!("no asset {id}")))?;

    let base_url = &state.ion_base_url;
    let url = match asset.asset_type {
        // CesiumTerrainProvider appends layer.json to whatever url arrives
        // here, so it has to be the directory of a quantized-mesh bundle. A
        // tileset.json url does not even fail loudly: the 404 on layer.json is
        // read as a pre-metadata heightmap layer, and every tile 404s after.
        AssetType::Terrain => {
            let bundle = asset.id.to_string();
            if !terrain_bundle::bundle_exists(&state, &bundle).await {
                return Err(ion_error(
                    StatusCode::NOT_FOUND,
                    format!(
                        "terrain asset {bundle} has no terrain to serve, its status is {status:?}: GET /api/v1/assets/{bundle}/jobs reports the job of a DEM upload, otherwise put a quantized-mesh bundle at <data-dir>/terrain_bundles/{bundle}/",
                        status = asset.status,
                    ),
                ));
            }
            format!("{base_url}/api/v1/terrain/bundles/{bundle}/")
        }
        // cesium reads an IMAGERY url with no externalType as a TMS root, and URL_TEMPLATE counts {y} from the north
        AssetType::Imagery => {
            let manifest = imagery::read_manifest(&state.data_dir, asset.id)
                .await
                .map_err(|reason| {
                    tracing::warn!("imagery manifest read failed: {reason}");
                    ion_error(StatusCode::INTERNAL_SERVER_ERROR, "imagery lookup failed")
                })?
                .ok_or_else(|| {
                    ion_error(
                        StatusCode::NOT_IMPLEMENTED,
                        format!(
                            "asset {id} is imagery with no tiles to serve: only a georeferenced GeoTIFF, JPEG or PNG upload is tiled"
                        ),
                    )
                })?;
            let url = format!("{base_url}{}", imagery::tile_template(asset.id));
            let options = json!({
                "url": url,
                "maximumLevel": manifest.max_zoom,
                "rectangle": manifest.rectangle_radians(),
            });
            return Ok(Json(IonEndpoint {
                endpoint_type: map_asset_type(&asset.asset_type),
                url,
                access_token: String::new(),
                attributions: Vec::new(),
                external_type: Some("URL_TEMPLATE"),
                options: Some(options),
            }));
        }
        // CzmlDataSource.load reads the endpoint url as the CZML document itself
        AssetType::Czml => format!("{base_url}{}", czml::document_path(asset.id)),
        _ => format!("{base_url}/api/v1/assets/{}/tileset.json", asset.id),
    };

    Ok(Json(IonEndpoint {
        endpoint_type: map_asset_type(&asset.asset_type),
        url,
        access_token: String::new(),
        attributions: Vec::new(),
        external_type: None,
        options: None,
    }))
}

fn global_terrain_endpoint(state: &AppState) -> Result<Json<IonEndpoint>, IonError> {
    if let Some(missing) = state.global_terrain.missing_configuration() {
        return Err(ion_error(StatusCode::SERVICE_UNAVAILABLE, missing));
    }
    Ok(Json(IonEndpoint {
        endpoint_type: map_asset_type(&AssetType::Terrain),
        url: format!("{}{}", state.ion_base_url, global_terrain::ROUTE_ROOT),
        access_token: String::new(),
        // cesium shows an ion attribution on screen only when collapsible is false
        attributions: vec![json!({
            "html": global_terrain::COPERNICUS_NOTICE,
            "collapsible": false,
        })],
        external_type: None,
        options: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_blank_base_urls_are_refused_naming_the_variable() {
        for raw in [None, Some(""), Some("   ")] {
            let error = checked_ion_base_url(raw).unwrap_err();
            assert!(
                error.starts_with("TILETOPIA_ION_BASE_URL is not set"),
                "{raw:?}: {error}"
            );
        }
    }

    #[test]
    fn malformed_base_urls_are_refused_naming_the_variable() {
        for raw in [
            "not a url",
            "tiles.example.com",
            "localhost:3000",
            "http://",
            "http:tiles.example.com",
            "ftp://tiles.example.com",
            "file:///srv/tiles",
        ] {
            let error = checked_ion_base_url(Some(raw)).unwrap_err();
            assert!(
                error.starts_with("TILETOPIA_ION_BASE_URL is "),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn base_urls_with_more_than_an_origin_are_refused_naming_the_origin() {
        for raw in [
            "https://tiles.example.com/",
            "https://tiles.example.com/tiletopia",
            "https://tiles.example.com/tiletopia/",
            "https://tiles.example.com?region=eu",
            "https://tiles.example.com#top",
            "https://user:password@tiles.example.com",
        ] {
            let error = checked_ion_base_url(Some(raw)).unwrap_err();
            assert!(
                error.starts_with("TILETOPIA_ION_BASE_URL is ")
                    && error.contains("expected exactly \"https://tiles.example.com\""),
                "{raw}: {error}"
            );
        }
    }

    #[test]
    fn origins_are_accepted_unchanged() {
        for raw in [
            "http://localhost:3000",
            "http://localhost:5174",
            "http://127.0.0.1:38473",
            "http://[::1]:3000",
            "https://tiles.example.com",
            "https://tiles.example.com:8443",
        ] {
            assert_eq!(checked_ion_base_url(Some(raw)).as_deref(), Ok(raw));
        }
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_the_base_url() {
        assert_eq!(
            checked_ion_base_url(Some(" https://tiles.example.com\n")).as_deref(),
            Ok("https://tiles.example.com")
        );
    }
}
