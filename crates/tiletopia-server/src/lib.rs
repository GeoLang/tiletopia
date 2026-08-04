//! tiletopia-server: HTTP tile server and REST API
//!
//! Serves 3D Tiles tilesets, manages assets, streams tiles with
//! view-dependent LOD, and provides WebSocket for real-time data.

pub mod admin;
pub mod analysis;
pub mod analysis_tiles;
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
pub mod db;
pub mod demo;
pub mod dynamic_raster;
pub mod elevation;
pub mod encryption;
pub mod entity_linking;
pub mod export;
pub mod feature_service;
pub mod federation;
pub mod flight_planning;
pub mod flythrough;
pub mod geocoding;
pub mod geofence;
pub mod geoprocessing;
pub mod geostatistics;
pub mod http_cache;
pub mod indoor;
pub mod ion_compat;
pub mod isochrone;
pub mod issue_tracking;
pub mod job_queue;
pub mod map_matching;
pub mod map_tiles;
pub mod marketplace;
pub mod metering;
pub mod metrics;
pub mod mobile;
pub mod model_registry;
pub mod multispectral;
pub mod offline_export;
pub mod osm_buildings;
pub mod photogrammetry;
pub mod plugin_registry;
pub mod plugins;
pub mod portal;
pub mod premium_routes;
pub mod priority_queue;
pub mod realtime;
pub mod reports;
pub mod retention;
pub mod routing;
pub mod scan_registration;
pub mod scheduler;
pub mod scripting;
pub mod stac;
pub mod static_map;
pub mod stories;
pub mod stories_api;
pub mod temporal;
pub mod tenant;
pub mod terrain_analysis;
pub mod terrain_api;
pub mod terrain_rgb;
pub mod upload;
pub mod users;
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
use std::time::Instant;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

/// Server application state.
pub struct AppState {
    pub db: Arc<db::Database>,
    pub store: Arc<dyn tiletopia_store::TileStore>,
    pub data_dir: std::path::PathBuf,
    pub job_queue: Arc<job_queue::JobQueue>,
    pub realtime: realtime::RealtimeState,
    pub demo: demo::DemoState,
    pub catalog: catalog::OpenDataCatalog,
    pub started_at: Instant,
    pub api_key_store: api_keys::ApiKeyStore,
    pub metering_store: metering::MeteringStore,
    pub webhook_engine: webhooks::WebhookEngine,
    pub workspace_store: workspaces::WorkspaceStore,
    pub export_engine: export::ExportEngine,
    pub scheduler: scheduler::Scheduler,
    pub plugin_registry: plugins::PluginRegistry,
    pub photogrammetry_engine: photogrammetry::PhotogrammetryEngine,
    pub classification_engine: classification::ClassificationEngine,
    pub model_registry: model_registry::ModelRegistry,
    pub collaboration_engine: collaboration::CollaborationEngine,
    pub versioning_engine: versioning::VersioningEngine,
    pub bim4d_engine: bim4d::Bim4DEngine,
    pub cog_engine: cog::CogEngine,
    pub routing_engine: routing::RoutingEngine,
    pub map_tile_engine: map_tiles::MapTileEngine,
    pub feature_service_engine: feature_service::FeatureServiceEngine,
    pub issue_tracker: issue_tracking::IssueTracker,
    /// Shared, so the analysis tile engines can sample it from their own graph.
    pub elevation_store: Arc<elevation::DemStore>,
    pub analysis_engines: analysis_tiles::AnalysisEngines,
    pub entity_link_store: entity_linking::EntityLinkStore,
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
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// JWT `sub` of the creator, `None` for rows created before ownership
    /// existed. Never serialized: it is an internal authz field and would leak
    /// user ids to every reader of the asset list.
    #[serde(default, skip_serializing)]
    pub owner_id: Option<String>,
}

/// Whether these claims may run a destructive write on an asset with this
/// owner. Admins may touch anything; a `None` owner is a legacy row from
/// before ownership existed, so any caller that got past the Edit-tier gate
/// may modify it.
pub fn may_modify_asset(claims: &auth::Claims, owner_id: Option<&str>) -> bool {
    match owner_id {
        None => true,
        Some(owner) => claims.can_admin() || owner == claims.sub,
    }
}

/// Whether these claims may see this asset in a listing. Admins see every
/// asset, everyone else sees their own plus the legacy ownerless rows.
///
/// Same owner rule as [`may_modify_asset`] today, kept separate because the two
/// decisions are free to diverge: this one has no Edit-tier gate in front of it.
/// It hides other tenants' asset metadata, not their tiles, which are public by
/// design (see [`auth::is_public_read`]).
pub fn may_view_asset(claims: &auth::Claims, owner_id: Option<&str>) -> bool {
    match owner_id {
        None => true,
        Some(owner) => claims.can_admin() || owner == claims.sub,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    // Auth routes — placed BEFORE the auth middleware layer so they don't require auth
    let auth_routes = Router::new()
        .route("/api/v1/auth/signup", axum::routing::post(users::signup))
        .route("/api/v1/auth/login", axum::routing::post(users::login));

    // Admin routes — require Admin role
    let admin_routes = Router::new()
        .route("/api/v1/admin/stats", get(admin::get_stats))
        .route("/api/v1/admin/health", get(admin::get_health))
        .route("/api/v1/admin/users", get(admin::list_users))
        .route("/api/v1/admin/jobs", get(admin::list_jobs))
        .route(
            "/api/v1/admin/users/{id}",
            axum::routing::delete(admin::delete_user),
        )
        .route(
            "/api/v1/admin/users/{id}/role",
            axum::routing::put(admin::set_user_role),
        )
        .layer(middleware::from_fn(users::require_admin));

    // Org routes (admin only)
    let org_routes = Router::new()
        .route(
            "/api/v1/orgs",
            get(users::list_orgs).post(users::create_org),
        )
        .layer(middleware::from_fn(users::require_admin));

    // Native asset writes — Edit tier, same gate as the Ion-compat
    // POST /v1/assets so an upload cannot be done at viewer level by going
    // through the native API instead. Asset reads stay on the main router.
    let asset_write_routes = Router::new()
        .route("/api/v1/assets", axum::routing::post(upload::upload_asset))
        .route("/api/v1/assets/{id}", axum::routing::delete(delete_asset))
        .route(
            "/api/v1/assets/{id}/tile",
            axum::routing::post(start_tiling),
        )
        .layer(middleware::from_fn(users::require_editor));

    // Annotations are asset content, so writing one is an asset mutation: same
    // Edit tier as the writes above, plus the owner-or-admin check the handlers
    // do. Listing them stays on the main router, readable with any valid token.
    let annotation_write_routes = Router::new()
        .route(
            "/api/v1/assets/{id}/annotations",
            axum::routing::post(create_annotation),
        )
        .route(
            "/api/v1/assets/{id}/annotations/{annotation_id}",
            axum::routing::delete(delete_annotation),
        )
        .layer(middleware::from_fn(users::require_editor));

    // Realtime collaboration websocket. Any valid JWT may join a room; the gate
    // is a layer of its own so an anonymous handshake is refused before the
    // upgrade, and so it holds in the no-secret development mode too.
    let realtime_routes = Router::new()
        .route("/api/v1/realtime/{room}", get(realtime::ws_handler))
        .layer(middleware::from_fn(realtime::require_room_join));

    Router::new()
        .route("/api/v1/assets", get(list_assets))
        .route("/api/v1/assets/{id}", get(get_asset))
        .route("/api/v1/assets/{id}/tileset.json", get(get_tileset))
        // {path} captures one segment, which is all a tile name ever is: the
        // tilers encode octree depth into the filename rather than into
        // directories, so node [0,3,7] is "037.glb", not "0/3/7.glb" (see
        // to_filename in tiletopia-core lod.rs:44, mesh_tiler.rs:286, tile.rs:137,
        // and the matching tiles_dir.join in their writers). If a tiler ever
        // emits nested child URIs this needs {*path}, and the matching arm in
        // auth::is_public_read has to widen with it.
        .route("/api/v1/assets/{id}/tiles/{path}", get(get_tile))
        .route("/api/v1/assets/{id}/thumbnail", get(get_thumbnail))
        .route("/api/v1/assets/{id}/annotations", get(list_annotations))
        .route("/api/v1/jobs/{id}", get(get_job_status))
        .route("/api/v1/users/me", get(users::get_me).put(users::update_me))
        .merge(asset_write_routes)
        .merge(annotation_write_routes)
        .merge(realtime_routes)
        .merge(admin_routes)
        .merge(org_routes)
        .route("/api/v1/health", get(health))
        .route("/metrics", get(metrics::metrics_handler))
        .merge(demo::demo_routes())
        .merge(catalog::catalog_routes())
        .merge(terrain_api::terrain_routes())
        .merge(terrain_rgb::terrain_rgb_routes())
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
        .merge(model_registry::model_registry_routes())
        .merge(premium_routes::collaboration_routes())
        .merge(premium_routes::versioning_routes())
        .merge(premium_routes::bim4d_routes())
        .merge(premium_routes::geocoding_routes())
        .merge(premium_routes::stac_routes())
        .merge(premium_routes::indoor_routes())
        .merge(premium_routes::cog_routes())
        .merge(premium_routes::routing_routes())
        .merge(premium_routes::map_tile_routes())
        // Batch 2: competitive gap-closing
        .merge(premium_routes::isochrone_routes())
        .merge(premium_routes::geoprocessing_routes())
        .merge(premium_routes::feature_service_routes())
        .merge(premium_routes::elevation_routes())
        .merge(premium_routes::map_matching_routes())
        .merge(premium_routes::static_map_routes())
        .merge(premium_routes::flight_planning_routes())
        .merge(premium_routes::scan_registration_routes())
        .merge(premium_routes::issue_tracking_routes())
        .merge(premium_routes::terrain_analysis_routes())
        .merge(analysis::analysis_routes())
        .merge(analysis_tiles::analysis_tile_routes())
        .merge(premium_routes::geostatistics_routes())
        .merge(premium_routes::multispectral_routes())
        .merge(premium_routes::osm_buildings_routes())
        .merge(premium_routes::entity_linking_routes())
        .merge(stories_api::story_routes())
        .merge(portal::portal_routes())
        .merge(catalog::add_dataset_routes())
        .merge(plugin_registry::plugin_registry_routes())
        .merge(ion_compat::ion_compat_read_routes())
        .merge(ion_compat::ion_compat_write_routes())
        .layer(middleware::from_fn(auth::auth_middleware))
        .merge(auth_routes)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct AssetQuery {
    pub q: Option<String>,
    pub tag: Option<String>,
    pub asset_type: Option<String>,
    pub status: Option<String>,
}

/// List assets visible to the caller. A token is required even in the
/// no-secret development mode, because the answer depends on who is asking.
async fn list_assets(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<AssetQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<Asset>>, StatusCode> {
    let claims = users::claims_from_headers(&headers)?;
    let has_filter = query.q.is_some()
        || query.tag.is_some()
        || query.asset_type.is_some()
        || query.status.is_some();

    let assets = if has_filter {
        state
            .db
            .search_assets(
                query.q.as_deref(),
                query.tag.as_deref(),
                query.asset_type.as_deref(),
                query.status.as_deref(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        state
            .db
            .list_assets()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let visible = assets
        .into_iter()
        .filter(|a| may_view_asset(&claims, a.owner_id.as_deref()))
        .collect();
    Ok(Json(visible))
}

async fn get_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Asset>, StatusCode> {
    state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn delete_asset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let claims = users::claims_from_headers(&headers)?;
    if !may_modify_asset(&claims, asset.owner_id.as_deref()) {
        return Err(StatusCode::FORBIDDEN);
    }

    state
        .db
        .delete_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Remove asset directory
    let asset_dir = state.data_dir.join(id.to_string());
    let _ = tokio::fs::remove_dir_all(&asset_dir).await;

    Ok(StatusCode::NO_CONTENT)
}

async fn get_tileset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<tiletopia_core::Tileset>, StatusCode> {
    let key = format!("{}/tileset.json", id);
    let data = state
        .store
        .get(&key)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let tileset: tiletopia_core::Tileset =
        serde_json::from_slice(&data).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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
    let key = format!("{}/tiles/{}", id, tile_path);
    let data = state
        .store
        .get(&key)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(data.to_vec())
}

/// Start a tiling job for an uploaded asset.
async fn start_tiling(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> Result<(StatusCode, Json<db::JobRecord>), StatusCode> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let claims = users::claims_from_headers(&headers)?;
    if !may_modify_asset(&claims, asset.owner_id.as_deref()) {
        return Err(StatusCode::FORBIDDEN);
    }

    // Find the input file
    let input_dir = state.data_dir.join(id.to_string()).join("input");
    let mut entries = tokio::fs::read_dir(&input_dir)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let input_path = entries
        .next_entry()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?
        .path()
        .to_string_lossy()
        .to_string();

    let job = state
        .job_queue
        .submit(id, input_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn get_job_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<db::JobRecord>, StatusCode> {
    state
        .db
        .get_job(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// ─── Annotation Endpoints ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct CreateAnnotation {
    id: Option<String>,
    text: String,
    longitude: f64,
    latitude: f64,
    #[serde(default)]
    height: f64,
}

async fn list_annotations(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<db::AnnotationRecord>>, StatusCode> {
    let annotations = state
        .db
        .list_annotations(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(annotations))
}

/// Claims allowed to write annotations on this asset, or the refusal. Behind
/// `require_editor`, so a valid token is always present and only the per-asset
/// owner-or-admin rule is left to check.
async fn annotation_writer(
    state: &AppState,
    asset_id: Uuid,
    headers: &axum::http::HeaderMap,
) -> Result<auth::Claims, StatusCode> {
    let asset = state
        .db
        .get_asset(asset_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let claims = users::claims_from_headers(headers)?;
    if !may_modify_asset(&claims, asset.owner_id.as_deref()) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(claims)
}

async fn create_annotation(
    State(state): State<Arc<AppState>>,
    Path(asset_id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateAnnotation>,
) -> Result<(StatusCode, Json<db::AnnotationRecord>), StatusCode> {
    let author = annotation_writer(&state, asset_id, &headers).await?.sub;

    let ann = db::AnnotationRecord {
        id: body
            .id
            .and_then(|s| Uuid::parse_str(&s).ok())
            .unwrap_or_else(Uuid::new_v4),
        asset_id,
        text: body.text,
        longitude: body.longitude,
        latitude: body.latitude,
        height: body.height,
        created_at: chrono::Utc::now(),
        created_by: Some(author),
    };
    state
        .db
        .create_annotation(&ann)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(ann)))
}

async fn delete_annotation(
    State(state): State<Arc<AppState>>,
    Path((asset_id, annotation_id)): Path<(Uuid, Uuid)>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, StatusCode> {
    annotation_writer(&state, asset_id, &headers).await?;

    // scoped to the asset in the path, so owning one asset is not a way to
    // delete an annotation that hangs off someone else's
    let deleted = state
        .db
        .delete_annotation(asset_id, annotation_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if deleted == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_thumbnail(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<
    (
        StatusCode,
        [(axum::http::header::HeaderName, &'static str); 1],
        Vec<u8>,
    ),
    StatusCode,
> {
    let thumb_path = state.data_dir.join(id.to_string()).join("thumbnail.png");
    let data = tokio::fs::read(&thumb_path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        data,
    ))
}

/// Generate a simple top-down point cloud thumbnail (256×256 PNG).
pub fn generate_point_cloud_thumbnail(
    points: &[(f64, f64, f64)],
    width: u32,
    height: u32,
) -> Vec<u8> {
    use image::{ImageBuffer, Rgba};

    let mut img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(width, height, Rgba([0, 0, 0, 255]));

    if points.is_empty() {
        let mut buf = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut buf);
        image::ImageEncoder::write_image(
            encoder,
            img.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap_or(());
        return buf;
    }

    // Compute XY bounds
    let (mut min_x, mut max_x) = (f64::MAX, f64::MIN);
    let (mut min_y, mut max_y) = (f64::MAX, f64::MIN);
    for &(x, y, _) in points {
        if x < min_x {
            min_x = x;
        }
        if x > max_x {
            max_x = x;
        }
        if y < min_y {
            min_y = y;
        }
        if y > max_y {
            max_y = y;
        }
    }
    let range_x = (max_x - min_x).max(1e-6);
    let range_y = (max_y - min_y).max(1e-6);

    for &(x, y, _) in points {
        let px = ((x - min_x) / range_x * (width as f64 - 1.0)) as u32;
        let py = ((y - min_y) / range_y * (height as f64 - 1.0)) as u32;
        if px < width && py < height {
            img.put_pixel(px, py, Rgba([255, 255, 255, 255]));
        }
    }

    let mut buf = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut buf);
    image::ImageEncoder::write_image(
        encoder,
        img.as_raw(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .unwrap_or(());
    buf
}
