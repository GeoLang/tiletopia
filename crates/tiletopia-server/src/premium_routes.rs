//! Premium feature API routes.
//!
//! Wires up all premium modules into axum routers.

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, classification, cog, elevation,
    export::{EXPORT_FORMATS, ExportFormat, ExportJob, ExportStatus},
    feature_service, flight_planning, geocoding, geoprocessing, geostatistics, indoor, isochrone,
    map_matching, map_tiles, metering, mobile, multispectral, osm_buildings, routing,
    scan_registration, scheduler, stac, static_map, terrain_analysis,
    terrain_api::Refusal,
    users,
};

/// Routes for API key management.
pub fn api_key_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/api-keys", get(list_api_keys))
        .route("/api/v1/api-keys/usage", get(get_usage))
}

async fn list_api_keys(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.api_key_store;
    let keys = store.list_keys(None).await;
    Json(serde_json::json!({
        "keys": keys.iter().map(|k| serde_json::json!({
            "id": k.id,
            "name": k.name,
            "prefix": &k.key_hash[..12],
            "permissions": k.permissions,
            "created_at": k.created_at,
            "last_used_at": k.last_used_at,
            "revoked": k.revoked,
            "rate_limit": {
                "requests_per_second": k.rate_limit.requests_per_second,
                "requests_per_day": k.rate_limit.requests_per_day,
            }
        })).collect::<Vec<_>>()
    }))
}

async fn get_usage(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.api_key_store;
    let keys = store.list_keys(None).await;
    let usages: Vec<_> = keys
        .iter()
        .map(|k| {
            serde_json::json!({
                "key_id": k.id,
                "name": k.name,
            })
        })
        .collect();
    Json(serde_json::json!({ "usage": usages }))
}

/// Routes for metering and billing.
pub fn metering_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/metering/summary", get(metering_summary))
        .route("/api/v1/metering/pricing", get(pricing_tiers))
}

async fn metering_summary(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.metering_store;
    let now = chrono::Utc::now();
    let period_start = now - chrono::Duration::days(30);
    let tenant_id = Uuid::nil(); // demo
    let summary = store.get_summary(tenant_id, period_start, now).await;
    Json(serde_json::json!(summary))
}

async fn pricing_tiers() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "tiers": [
            metering::PricingTier::free(),
            metering::PricingTier::pro(),
            metering::PricingTier::enterprise()
        ]
    }))
}

/// Routes for webhooks.
pub fn webhook_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/webhooks", get(list_webhooks))
        .route("/api/v1/webhooks/events", get(webhook_event_types))
}

async fn list_webhooks(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.webhook_engine;
    let subs = engine.list_subscriptions(None).await;
    Json(serde_json::json!({
        "subscriptions": subs,
        "pending_deliveries": engine.pending_count().await
    }))
}

async fn webhook_event_types() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "event_types": [
            "asset_processed", "anomaly_detected", "export_ready",
            "upload_complete", "terrain_generated", "clash_detected",
            "job_completed", "rate_limit_warning"
        ]
    }))
}

/// Routes for workspaces/organizations.
pub fn workspace_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/workspaces", get(list_orgs))
        .route("/api/v1/workspaces/teams", get(list_teams))
        .route("/api/v1/workspaces/projects", get(list_projects))
}

async fn list_orgs(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.workspace_store;
    let orgs = store.list_orgs().await;
    Json(serde_json::json!({ "organizations": orgs }))
}

async fn list_teams(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.workspace_store;
    let orgs = store.list_orgs().await;
    if let Some(org) = orgs.first() {
        let teams = store.list_teams(org.id).await;
        Json(serde_json::json!({ "teams": teams }))
    } else {
        Json(serde_json::json!({ "teams": [] }))
    }
}

async fn list_projects(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.workspace_store;
    let orgs = store.list_orgs().await;
    if let Some(org) = orgs.first() {
        let projects = store.list_projects(org.id).await;
        Json(serde_json::json!({ "projects": projects }))
    } else {
        Json(serde_json::json!({ "projects": [] }))
    }
}

/// Routes for export jobs.
pub fn export_routes() -> Router<Arc<AppState>> {
    // starting an export is compute against an asset, so it sits in the Edit
    // tier alongside upload rather than with the reads below
    let write_routes = Router::new()
        .route("/api/v1/exports", post(create_export))
        .layer(middleware::from_fn(users::require_editor));

    Router::new()
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/exports/formats", get(export_formats))
        .route("/api/v1/exports/{id}", get(get_export))
        .route("/api/v1/exports/download/{id}", get(download_export))
        .merge(write_routes)
}

async fn list_exports(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let tenant_id = tenant_from_headers(&headers)?;
    let jobs = state.export_engine.list_exports(Some(tenant_id)).await;
    Ok(Json(serde_json::json!({ "exports": jobs })))
}

async fn export_formats() -> Json<serde_json::Value> {
    let formats: Vec<serde_json::Value> = EXPORT_FORMATS
        .iter()
        .map(|f| serde_json::json!({"id": f.id, "name": f.name, "extension": f.extension}))
        .collect();
    Json(serde_json::json!({ "formats": formats }))
}

/// The caller's own id doubles as their tenant, since the JWT carries no tenant
/// claim.
fn tenant_from_headers(headers: &HeaderMap) -> Result<Uuid, StatusCode> {
    let claims = users::claims_from_headers(headers)?;
    Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::UNAUTHORIZED)
}

/// A job the caller owns, or 404 so job ids of other tenants stay invisible.
async fn owned_job(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    id: Uuid,
) -> Result<ExportJob, StatusCode> {
    let tenant_id = tenant_from_headers(headers)?;
    let job = state
        .export_engine
        .get_export(id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    if job.tenant_id != tenant_id {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(job)
}

#[derive(Deserialize)]
struct CreateExportRequest {
    asset_id: Uuid,
    format: String,
    #[serde(default)]
    bounds: Option<[f64; 4]>,
}

async fn create_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateExportRequest>,
) -> Result<(StatusCode, Json<ExportJob>), StatusCode> {
    let tenant_id = tenant_from_headers(&headers)?;
    let format = ExportFormat::from_id(&req.format).ok_or(StatusCode::BAD_REQUEST)?;
    let job = state
        .export_engine
        .create_export(tenant_id, req.asset_id, format, req.bounds)
        .await;

    // encoding runs off the request so the caller can poll the job it just got
    let job_id = job.id;
    let worker_state = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(reason) = worker_state
            .export_engine
            .execute_export(job_id, &worker_state.data_dir)
            .await
        {
            tracing::warn!("export {job_id} failed: {reason}");
        }
    });

    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn get_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ExportJob>, StatusCode> {
    Ok(Json(owned_job(&state, &headers, id).await?))
}

async fn download_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response, StatusCode> {
    let job = owned_job(&state, &headers, id).await?;
    if job.status != ExportStatus::Ready {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = crate::export::exported_file(&state.data_dir, id).ok_or(StatusCode::NOT_FOUND)?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(StatusCode::NOT_FOUND)?
        .to_string();
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

/// Routes for the scheduler.
pub fn scheduler_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/scheduler/jobs", get(list_scheduled_jobs))
        .route("/api/v1/scheduler/stats", get(scheduler_stats))
        .route("/api/v1/scheduler/runs", get(recent_runs))
}

async fn list_scheduled_jobs(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let sched = &state.scheduler;
    let jobs = sched.list_jobs(None).await;
    Json(serde_json::json!({ "jobs": jobs }))
}

async fn scheduler_stats(State(state): State<Arc<AppState>>) -> Json<scheduler::SchedulerStats> {
    let sched = &state.scheduler;
    Json(sched.stats().await)
}

async fn recent_runs(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let sched = &state.scheduler;
    let runs = sched.recent_runs(20).await;
    Json(serde_json::json!({ "runs": runs }))
}

/// Routes for plugins/marketplace.
pub fn plugin_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/plugins", get(list_plugins))
        .route("/api/v1/plugins/pipelines", get(list_pipelines))
}

async fn list_plugins(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let registry = &state.plugin_registry;
    let all = registry.list_plugins(None).await;
    Json(serde_json::json!({ "plugins": all }))
}

async fn list_pipelines(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let registry = &state.plugin_registry;
    let pipelines = registry.list_pipelines().await;
    Json(serde_json::json!({ "pipelines": pipelines }))
}

/// Routes for mobile SDK.
pub fn mobile_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/mobile/config", get(mobile_config))
        .route("/api/v1/mobile/offline", get(offline_packages))
}

async fn mobile_config() -> Json<mobile::SdkConfig> {
    // Default high-end config for demo
    let caps = mobile::DeviceCapabilities {
        platform: mobile::Platform::Ios,
        sdk_version: "1.0.0".into(),
        screen_density: 3.0,
        gpu_tier: mobile::GpuTier::High,
        available_memory_mb: 4096,
        network_type: mobile::NetworkType::Wifi,
        supports_webgl2: true,
        supports_3d_tiles: true,
        max_texture_size: 4096,
    };
    Json(mobile::generate_sdk_config(&caps))
}

async fn offline_packages() -> Json<serde_json::Value> {
    let packages = mobile::available_offline_packages();
    Json(serde_json::json!({ "packages": packages }))
}

// ─── Gap-closing feature routes ─────────────────────────────────────────────

/// Routes for photogrammetry pipeline.
pub fn photogrammetry_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/photogrammetry/projects",
            get(list_photogrammetry_projects),
        )
        .route("/api/v1/photogrammetry/presets", get(quality_presets))
}

async fn list_photogrammetry_projects(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let engine = &state.photogrammetry_engine;
    let projects = engine.list_projects(None).await;
    Json(serde_json::json!({ "projects": projects }))
}

async fn quality_presets() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "presets": ["Draft", "Medium", "High", "Ultra"]
    }))
}

/// Routes for point cloud classification.
pub fn classification_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/classification/models",
            get(list_classification_models),
        )
        .route("/api/v1/classification/classes", get(list_classes))
}

async fn list_classification_models() -> Json<serde_json::Value> {
    let models = classification::ClassificationEngine::available_models();
    Json(serde_json::json!({ "models": models }))
}

async fn list_classes(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.classification_engine;
    let jobs = engine.list_jobs(None).await;
    Json(serde_json::json!({ "jobs": jobs }))
}

/// Routes for real-time collaboration.
pub fn collaboration_routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/v1/collaboration/sessions",
        get(list_collaboration_sessions),
    )
}

async fn list_collaboration_sessions(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let engine = &state.collaboration_engine;
    let sessions = engine.list_sessions().await;
    Json(serde_json::json!({ "sessions": sessions }))
}

/// Routes for asset versioning.
pub fn versioning_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/versioning/assets", get(list_versioned_assets))
}

async fn list_versioned_assets(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.versioning_engine;
    let assets = engine.list_assets().await;
    Json(serde_json::json!({ "assets": assets }))
}

/// Routes for BIM 4D scheduling.
pub fn bim4d_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/bim4d/projects", get(list_bim4d_projects))
}

async fn list_bim4d_projects(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.bim4d_engine;
    let projects = engine.list_projects().await;
    Json(serde_json::json!({ "projects": projects }))
}

/// Routes for geocoding.
pub fn geocoding_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/geocoding/search", get(geocode_search))
        .route("/api/v1/geocoding/reverse", get(geocode_reverse))
}

#[derive(Deserialize)]
struct GeocodeQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct ReverseGeocodeQuery {
    lat: Option<f64>,
    lon: Option<f64>,
}

async fn geocode_search(Query(params): Query<GeocodeQuery>) -> Json<serde_json::Value> {
    let query = params.q.unwrap_or_else(|| "Golden Gate Bridge".into());
    // Try live Nominatim first, fall back to demo
    match geocoding::geocode_nominatim(&query).await {
        Ok(result) => Json(serde_json::json!(result)),
        Err(_) => {
            let result = geocoding::geocode(&query);
            Json(serde_json::json!(result))
        }
    }
}

async fn geocode_reverse(Query(params): Query<ReverseGeocodeQuery>) -> Json<serde_json::Value> {
    let lat = params.lat.unwrap_or(37.7749);
    let lon = params.lon.unwrap_or(-122.4194);
    match geocoding::reverse_geocode_nominatim(lat, lon).await {
        Ok(place) => Json(serde_json::json!(place)),
        Err(_) => {
            let place = geocoding::reverse_geocode(lat, lon);
            Json(serde_json::json!(place))
        }
    }
}

/// Routes for STAC catalog.
pub fn stac_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/stac", get(stac_root))
        .route("/api/v1/stac/collections", get(stac_collections))
        .route("/api/v1/stac/search", get(stac_search))
}

async fn stac_root() -> Json<serde_json::Value> {
    let catalog = stac::root_catalog();
    Json(serde_json::json!(catalog))
}

async fn stac_collections() -> Json<serde_json::Value> {
    let colls = stac::collections();
    Json(serde_json::json!({ "collections": colls }))
}

#[derive(Deserialize)]
struct StacSearchQuery {
    bbox: Option<String>,
    datetime: Option<String>,
    collections: Option<String>,
    limit: Option<u32>,
}

/// Forward an item search to the configured STAC upstream. With none configured
/// this refuses: a viewer drawing footprints has no way to tell invented items
/// from a real catalog's answer.
async fn stac_search(Query(params): Query<StacSearchQuery>) -> Response {
    let searched = async {
        // the request is read before the configuration, so a typo in bbox says
        // so instead of being hidden behind a missing upstream
        let params = stac::SearchParams::from_query(
            params.bbox.as_deref(),
            params.datetime.as_deref(),
            params.collections.as_deref(),
            params.limit,
        )
        .map_err(stac::SearchError::BadRequest)?;
        let api = stac::upstream_api().ok_or(stac::SearchError::NoUpstream)?;
        stac::search(&api, &params).await
    };
    match searched.await {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            tracing::warn!("stac search refused: {e}");
            (
                e.status(),
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Routes for indoor mapping.
pub fn indoor_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/indoor/buildings", get(list_buildings))
}

async fn list_buildings() -> Json<serde_json::Value> {
    let buildings = indoor::demo_buildings();
    Json(serde_json::json!({ "buildings": buildings }))
}

/// Routes for Cloud Optimized GeoTIFF.
pub fn cog_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/cog/datasets", get(list_cog_datasets))
        .route("/api/v1/cog/datasets/{id}/window", get(read_cog_window))
        .route("/api/v1/cog/stats", get(cog_stats))
}

/// Read a pixel window out of a registered COG. The read opens the source and
/// makes range requests, so it runs on a blocking thread.
async fn read_cog_window(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(window): Query<cog::WindowRequest>,
) -> Response {
    let read =
        tokio::task::spawn_blocking(move || state.cog_engine.read_window(&id, &window)).await;
    match read {
        Ok(Ok(window)) => Json(window).into_response(),
        Ok(Err(e)) => {
            tracing::warn!("cog window refused: {e}");
            (
                e.status(),
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("cog window read panicked: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn list_cog_datasets(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.cog_engine;
    let datasets = engine.list_datasets().to_vec();
    Json(serde_json::json!({ "datasets": datasets }))
}

async fn cog_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.cog_engine;
    let datasets = engine.list_datasets();
    let total_bytes: u64 = datasets.iter().map(|d| d.file_size_bytes).sum();
    Json(serde_json::json!({
        "dataset_count": datasets.len(),
        "total_size_bytes": total_bytes
    }))
}

/// Routes for routing/navigation.
pub fn routing_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/routing/stats", get(routing_stats))
        .route("/api/v1/routing/route", get(compute_route))
}

async fn routing_stats(State(state): State<Arc<AppState>>) -> Json<routing::RoutingStats> {
    let engine = &state.routing_engine;
    Json(engine.stats())
}

#[derive(Deserialize)]
struct RouteQuery {
    origin_lon: Option<f64>,
    origin_lat: Option<f64>,
    dest_lon: Option<f64>,
    dest_lat: Option<f64>,
    profile: Option<String>,
}

async fn compute_route(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RouteQuery>,
) -> Json<serde_json::Value> {
    let engine = &state.routing_engine;
    let profile = match params.profile.as_deref() {
        Some("walking") => routing::RoutingProfile::Walking,
        Some("cycling") => routing::RoutingProfile::Cycling,
        _ => routing::RoutingProfile::Driving,
    };
    let req = routing::RouteRequest {
        origin: [
            params.origin_lon.unwrap_or(-122.4194),
            params.origin_lat.unwrap_or(37.7749),
        ],
        destination: [
            params.dest_lon.unwrap_or(-122.4100),
            params.dest_lat.unwrap_or(37.7800),
        ],
        profile,
        alternatives: false,
    };
    match engine.compute_route(&req) {
        Some(route) => Json(serde_json::json!(route)),
        None => Json(serde_json::json!({"error": "No route found"})),
    }
}

/// Routes for 2D map tiles (XYZ, MVT, styles).
pub fn map_tile_routes() -> Router<Arc<AppState>> {
    // Cache hit rates and size are operational telemetry, not map data, so this
    // takes the same Admin gate as /api/v1/admin/stats. The rest of the group is
    // tile-source metadata a viewer reads anonymously.
    let cache_stats = Router::new()
        .route("/api/v1/tiles/cache/stats", get(tile_cache_stats))
        .layer(middleware::from_fn(crate::users::require_admin));

    Router::new()
        .route("/api/v1/tiles/sources", get(list_tile_sources))
        .route("/api/v1/tiles/styles", get(list_tile_styles))
        .route("/api/v1/tiles/layers", get(list_vector_layers))
        .route("/api/v1/tiles/{source_id}/tilejson", get(get_tilejson))
        .merge(cache_stats)
}

async fn list_tile_sources(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.map_tile_engine;
    let sources = engine.list_sources().to_vec();
    Json(serde_json::json!({ "sources": sources }))
}

async fn list_tile_styles(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.map_tile_engine;
    let styles = engine.list_styles().to_vec();
    Json(serde_json::json!({ "styles": styles }))
}

async fn tile_cache_stats(State(state): State<Arc<AppState>>) -> Json<map_tiles::CacheStats> {
    let engine = &state.map_tile_engine;
    Json(engine.cache_stats().clone())
}

async fn list_vector_layers(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.map_tile_engine;
    let vector_source = engine
        .list_sources()
        .iter()
        .find(|s| s.source_type == map_tiles::TileSourceType::VectorGeoJson)
        .map(|s| s.id);
    match vector_source {
        Some(id) => {
            let layers = engine.vector_layers(id).unwrap_or_default();
            Json(serde_json::json!({ "layers": layers }))
        }
        None => Json(serde_json::json!({ "layers": [] })),
    }
}

async fn get_tilejson(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(source_id): axum::extract::Path<Uuid>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let engine = &state.map_tile_engine;
    engine
        .tilejson(source_id)
        .map(Json)
        .ok_or(axum::http::StatusCode::NOT_FOUND)
}

// ─── Batch 2: Competitive gap-closing routes ────────────────────────────────

/// Routes for isochrone/travel-time analysis.
pub fn isochrone_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/isochrone/compute", get(compute_isochrone))
        .route("/api/v1/isochrone/profiles", get(isochrone_profiles))
}

#[derive(Deserialize)]
struct IsochroneQuery {
    lon: f64,
    lat: f64,
    minutes: Option<String>, // comma-separated: "5,10,15"
    profile: Option<String>,
    concavity: Option<f64>,
}

const DEFAULT_CONTOUR_MINUTES: &str = "5,10,15";
const ISOCHRONE_PROFILES: [&str; 3] = ["driving", "walking", "cycling"];
const ISOCHRONE_DENOISE: f32 = 0.5;

fn bad_request(reason: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, reason)
}

fn parse_travel_profile(name: &str) -> Option<isochrone::TravelProfile> {
    match name {
        "driving" => Some(isochrone::TravelProfile::Driving),
        "walking" => Some(isochrone::TravelProfile::Walking),
        "cycling" => Some(isochrone::TravelProfile::Cycling),
        _ => None,
    }
}

impl IsochroneQuery {
    fn into_request(self) -> Result<isochrone::IsochroneRequest, (StatusCode, String)> {
        if !(-180.0..=180.0).contains(&self.lon) || !(-90.0..=90.0).contains(&self.lat) {
            return Err(bad_request(format!(
                "lon must be within -180..180 and lat within -90..90, got {},{}",
                self.lon, self.lat
            )));
        }

        let contours_minutes = self
            .minutes
            .as_deref()
            .unwrap_or(DEFAULT_CONTOUR_MINUTES)
            .split(',')
            .map(|entry| {
                entry.trim().parse::<u32>().map_err(|_| {
                    bad_request(format!(
                        "minutes must be a comma-separated list of whole numbers, got '{entry}'"
                    ))
                })
            })
            .collect::<Result<Vec<u32>, _>>()?;

        let profile = match self.profile.as_deref() {
            Some(name) => parse_travel_profile(name).ok_or_else(|| {
                bad_request(format!(
                    "unknown profile '{name}'; valid options: {}",
                    ISOCHRONE_PROFILES.join(", ")
                ))
            })?,
            None => isochrone::TravelProfile::Driving,
        };

        let concavity = self.concavity.unwrap_or(itinera_core::DEFAULT_CONCAVITY);
        if concavity < 0.0 || concavity.is_nan() {
            return Err(bad_request(format!(
                "concavity must be zero or greater, got {concavity}"
            )));
        }

        Ok(isochrone::IsochroneRequest {
            origin: [self.lon, self.lat],
            profile,
            contours_minutes,
            denoise: ISOCHRONE_DENOISE,
            concavity,
        })
    }
}

async fn compute_isochrone(
    Query(params): Query<IsochroneQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let result = isochrone::compute_isochrone(&params.into_request()?);
    Ok(Json(serde_json::json!(result)))
}

async fn isochrone_profiles() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "profiles": ISOCHRONE_PROFILES }))
}

/// Routes for geoprocessing operations.
pub fn geoprocessing_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/geoprocessing/operations", get(list_geo_operations))
        .route("/api/v1/geoprocessing/demo", get(geoprocessing_demo))
}

async fn list_geo_operations() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "operations": ["Buffer", "ConvexHull", "Centroid", "Simplify", "Intersection", "Union", "Difference"]
    }))
}

async fn geoprocessing_demo() -> Json<serde_json::Value> {
    let polygon = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]];
    let geom = geoprocessing::Geometry {
        geom_type: geoprocessing::GeomType::Polygon,
        coordinates: polygon.clone(),
    };
    let buffered = geoprocessing::buffer(&geom, 100.0);
    let centroid = geoprocessing::centroid(&polygon);
    let hull = geoprocessing::convex_hull(&polygon);
    Json(serde_json::json!({
        "original_vertices": polygon.len(),
        "buffered_vertices": buffered.buffered.coordinates.len(),
        "buffer_area_m2": buffered.area_m2,
        "centroid": centroid,
        "hull_vertices": hull.len()
    }))
}

/// Routes for feature service (WFS-like).
pub fn feature_service_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/features/layers", get(list_feature_layers))
        .route("/api/v1/features/query", get(query_features))
}

async fn list_feature_layers(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let engine = &state.feature_service_engine;
    let layers = engine.list_layers();
    Json(serde_json::json!({ "layers": layers }))
}

#[derive(Deserialize)]
struct FeatureQuery {
    layer: Option<String>,
    bbox: Option<String>, // "minx,miny,maxx,maxy"
    limit: Option<usize>,
    offset: Option<usize>,
    #[serde(rename = "where")]
    where_clause: Option<String>,
}

async fn query_features(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FeatureQuery>,
) -> Json<serde_json::Value> {
    let engine = &state.feature_service_engine;
    let layers = engine.list_layers();
    let layer = if let Some(name) = &params.layer {
        layers.iter().find(|l| l.name == *name)
    } else {
        layers.first()
    };
    if let Some(layer) = layer {
        let bbox = params.bbox.and_then(|b| {
            let parts: Vec<f64> = b.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if parts.len() == 4 {
                Some([parts[0], parts[1], parts[2], parts[3]])
            } else {
                None
            }
        });
        let query = feature_service::SpatialQuery {
            bbox,
            intersects: None,
            within_distance_m: None,
            where_clause: params.where_clause,
            limit: params.limit.unwrap_or(100),
            offset: params.offset.unwrap_or(0),
            order_by: None,
        };
        let features = engine.query_features(layer.id, &query);
        Json(serde_json::json!({ "type": "FeatureCollection", "features": features }))
    } else {
        Json(serde_json::json!({ "type": "FeatureCollection", "features": [] }))
    }
}

/// Routes for elevation service.
///
/// Both read the DEM stores [`crate::elevation`] serves: a loaded grid, a tile
/// staged under the data directory, then the SRTM cache. A location none of
/// them covers is a 404 naming it, never an invented height.
pub fn elevation_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/elevation/point", get(elevation_point))
        .route("/api/v1/elevation/profile", get(elevation_profile))
}

/// Points one profile request may ask for. Each one is a DEM sample, and the
/// route is on the anonymous read surface.
const MAX_PROFILE_POINTS: usize = 512;

#[derive(Deserialize)]
struct ElevationQuery {
    lat: f64,
    lon: f64,
}

async fn elevation_point(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ElevationQuery>,
) -> Result<Json<elevation::ElevationPoint>, Refusal> {
    let (lat, lon) = (params.lat, params.lon);
    if !elevation::on_the_globe(lon, lat) {
        return Err(bad_coordinates());
    }
    let field = state
        .elevation_sources()
        .field([lon, lat, lon, lat])
        .await?;
    Ok(Json(field.point(lat, lon)?))
}

#[derive(Deserialize)]
struct ProfileQuery {
    /// `lon,lat` pairs separated by `;`, in the order they are walked.
    path: String,
}

async fn elevation_profile(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProfileQuery>,
) -> Result<Json<elevation::ElevationProfile>, Refusal> {
    let path = parse_path(&params.path)?;
    let bounds = elevation::bounds_of(&path).ok_or_else(bad_coordinates)?;
    let field = state.elevation_sources().field(bounds).await?;
    Ok(Json(field.profile(&path)?))
}

/// Parse `lon,lat;lon,lat` into the points to walk.
fn parse_path(raw: &str) -> Result<Vec<[f64; 2]>, Refusal> {
    let mut path = Vec::new();
    for pair in raw.split(';').filter(|p| !p.trim().is_empty()) {
        let mut parts = pair.split(',').map(|v| v.trim().parse::<f64>());
        let (Some(Ok(lon)), Some(Ok(lat)), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(refuse_request(format!(
                "path point {pair:?} is not lon,lat in degrees"
            )));
        };
        if !elevation::on_the_globe(lon, lat) {
            return Err(bad_coordinates());
        }
        path.push([lon, lat]);
    }
    if path.len() < 2 {
        return Err(refuse_request(
            "path needs at least two lon,lat points separated by ;".into(),
        ));
    }
    if path.len() > MAX_PROFILE_POINTS {
        return Err(refuse_request(format!(
            "path has {} points, past the {MAX_PROFILE_POINTS} point cap",
            path.len()
        )));
    }
    Ok(path)
}

fn bad_coordinates() -> Refusal {
    refuse_request("lon must be within -180..180 and lat within -90..90".into())
}

/// A 400 in the refusal type the elevation handlers answer with.
fn refuse_request(reason: String) -> Refusal {
    bad_request(reason).into_response().into()
}

/// Routes for map matching.
pub fn map_matching_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/map-matching/match", get(map_match_demo))
}

async fn map_match_demo() -> Json<serde_json::Value> {
    let request = map_matching::MapMatchRequest {
        trace: vec![
            map_matching::GpsPoint {
                latitude: 37.7749,
                longitude: -122.4194,
                timestamp: None,
                accuracy_m: Some(5.0),
                speed_mps: None,
                bearing_deg: None,
            },
            map_matching::GpsPoint {
                latitude: 37.7755,
                longitude: -122.4180,
                timestamp: None,
                accuracy_m: Some(5.0),
                speed_mps: None,
                bearing_deg: None,
            },
            map_matching::GpsPoint {
                latitude: 37.7760,
                longitude: -122.4165,
                timestamp: None,
                accuracy_m: Some(5.0),
                speed_mps: None,
                bearing_deg: None,
            },
            map_matching::GpsPoint {
                latitude: 37.7768,
                longitude: -122.4150,
                timestamp: None,
                accuracy_m: Some(5.0),
                speed_mps: None,
                bearing_deg: None,
            },
        ],
        profile: map_matching::MatchProfile::Driving,
        search_radius_m: 50.0,
    };
    let result = map_matching::match_trace(&request);
    Json(serde_json::json!(result))
}

/// Routes for static map rendering.
pub fn static_map_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/static-map/render", get(static_map_render))
        .route("/api/v1/static-map/formats", get(static_map_formats))
}

#[derive(Deserialize)]
struct StaticMapQuery {
    lon: Option<f64>,
    lat: Option<f64>,
    zoom: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    format: Option<String>,
}

async fn static_map_render(Query(params): Query<StaticMapQuery>) -> Json<serde_json::Value> {
    let format = match params.format.as_deref() {
        Some("jpeg") | Some("jpg") => static_map::ImageFormat::Jpeg,
        Some("webp") => static_map::ImageFormat::Webp,
        Some("svg") => static_map::ImageFormat::Svg,
        Some("pdf") => static_map::ImageFormat::Pdf,
        _ => static_map::ImageFormat::Png,
    };
    let req = static_map::StaticMapRequest {
        center: Some([
            params.lon.unwrap_or(-122.4194),
            params.lat.unwrap_or(37.7749),
        ]),
        zoom: Some(params.zoom.unwrap_or(14.0)),
        bbox: None,
        width: params.width.unwrap_or(800),
        height: params.height.unwrap_or(600),
        format,
        style_id: None,
        markers: vec![],
        overlays: vec![],
        dpi: 72,
    };
    let result = static_map::render_static_map(&req);
    Json(serde_json::json!({
        "width": result.width,
        "height": result.height,
        "format": format!("{:?}", result.format),
        "size_bytes": result.size_bytes,
        "bbox": result.bbox,
        "render_time_ms": result.render_time_ms
    }))
}

async fn static_map_formats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "formats": ["PNG", "JPEG", "WebP", "SVG", "PDF"],
        "max_width": 4096,
        "max_height": 4096
    }))
}

/// Routes for drone flight planning.
pub fn flight_planning_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/flight-planning/generate",
            get(generate_flight_demo),
        )
        .route("/api/v1/flight-planning/patterns", get(flight_patterns))
}

async fn generate_flight_demo() -> Json<serde_json::Value> {
    let area = vec![
        [-122.42, 37.77],
        [-122.41, 37.77],
        [-122.41, 37.78],
        [-122.42, 37.78],
        [-122.42, 37.77],
    ];
    let plan = flight_planning::generate_grid_plan(&area, 80.0, 0.8, 0.7);
    Json(serde_json::json!({
        "waypoints": plan.waypoints.len(),
        "total_distance_m": plan.statistics.total_distance_m,
        "estimated_duration_min": plan.statistics.estimated_flight_time_min,
        "gsd_cm": plan.parameters.gsd_cm_per_px,
        "coverage_area_m2": plan.statistics.coverage_area_m2
    }))
}

async fn flight_patterns() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "patterns": ["Grid/Lawnmower", "Double Grid/Crosshatch", "Orbit/POI", "Corridor", "Free Flight"]
    }))
}

/// Routes for scan registration (ICP).
pub fn scan_registration_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/scan-registration/demo",
            get(scan_registration_demo),
        )
        .route(
            "/api/v1/scan-registration/methods",
            get(registration_methods),
        )
}

async fn scan_registration_demo() -> Json<serde_json::Value> {
    let reg = scan_registration::demo_registration();
    Json(serde_json::json!({
        "id": reg.id,
        "scans": reg.scans.len(),
        "method": format!("{:?}", reg.method),
        "status": format!("{:?}", reg.status),
        "result": reg.result
    }))
}

async fn registration_methods() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "methods": ["PointToPoint", "PointToPlane", "GeneralizedIcp", "Ndt", "FeatureBased"]
    }))
}

/// Routes for issue/defect tracking.
pub fn issue_tracking_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/issues", get(list_issues))
        .route("/api/v1/issues/stats", get(issue_stats))
}

async fn list_issues(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tracker = &state.issue_tracker;
    let issues = tracker.list_issues(None);
    Json(serde_json::json!({ "issues": issues }))
}

async fn issue_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tracker = &state.issue_tracker;
    let stats = tracker.stats();
    Json(serde_json::json!(stats))
}

/// Routes for terrain analysis.
pub fn terrain_analysis_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/terrain-analysis/operations",
            get(terrain_operations),
        )
        .route("/api/v1/terrain-analysis/demo", get(terrain_analysis_demo))
}

async fn terrain_operations() -> Json<serde_json::Value> {
    let ops = terrain_analysis::available_analyses();
    Json(serde_json::json!({ "operations": ops }))
}

async fn terrain_analysis_demo() -> Json<serde_json::Value> {
    // Simple 5x5 DEM
    let dem = vec![
        vec![100.0, 105.0, 110.0, 108.0, 103.0],
        vec![102.0, 108.0, 115.0, 112.0, 106.0],
        vec![105.0, 112.0, 120.0, 118.0, 110.0],
        vec![103.0, 110.0, 116.0, 114.0, 108.0],
        vec![100.0, 106.0, 112.0, 110.0, 105.0],
    ];
    let slope_params = terrain_analysis::SlopeParams {
        output_unit: terrain_analysis::SlopeUnit::Degrees,
        method: terrain_analysis::SlopeMethod::Horn,
    };
    let result = terrain_analysis::compute_slope(&dem, 10.0, &slope_params);
    Json(serde_json::json!({
        "analysis": "slope",
        "statistics": result.statistics,
        "resolution_m": result.resolution_m
    }))
}

/// Routes for geostatistics.
///
/// Generic over the state so a test can serve the same route table without an
/// `AppState`: no handler here reads any.
pub fn geostatistics_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/geostatistics/methods", get(geostat_methods))
        .route("/api/v1/geostatistics/demo", get(geostat_demo))
        .route(
            "/api/v1/geostatistics/interpolate",
            post(geostat_interpolate),
        )
}

async fn geostat_methods() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "methods": ["IDW", "OrdinaryKriging", "UniversalKriging", "SimpleKriging"],
        "variogram_models": ["Spherical", "Exponential", "Gaussian", "Linear", "Power"],
        "max_samples": geostatistics::MAX_SAMPLES,
        "max_grid_cells": geostatistics::MAX_GRID_CELLS
    }))
}

#[derive(Deserialize)]
struct InterpolateRequest {
    samples: Vec<geostatistics::SamplePoint>,
    bounds: [f64; 4],
    resolution: f64,
    method: geostatistics::InterpolationMethod,
}

async fn geostat_interpolate(
    Json(request): Json<InterpolateRequest>,
) -> Result<Json<geostatistics::InterpolationResult>, (StatusCode, String)> {
    geostatistics::interpolate_grid(
        &request.samples,
        request.bounds,
        request.resolution,
        &request.method,
    )
    .map(Json)
    .map_err(|refusal| (refusal.status(), refusal.to_string()))
}

async fn geostat_demo() -> Json<serde_json::Value> {
    let samples = vec![
        geostatistics::SamplePoint {
            x: 0.0,
            y: 0.0,
            value: 10.0,
        },
        geostatistics::SamplePoint {
            x: 1.0,
            y: 0.0,
            value: 12.0,
        },
        geostatistics::SamplePoint {
            x: 0.0,
            y: 1.0,
            value: 11.0,
        },
        geostatistics::SamplePoint {
            x: 1.0,
            y: 1.0,
            value: 13.0,
        },
        geostatistics::SamplePoint {
            x: 0.5,
            y: 0.5,
            value: 11.5,
        },
    ];
    let result = geostatistics::interpolate_grid(
        &samples,
        [0.0, 0.0, 1.0, 1.0],
        0.25,
        &geostatistics::InterpolationMethod::Idw { power: 2.0 },
    )
    .expect("the demo's own five samples interpolate");
    Json(serde_json::json!({
        "demo": "five invented samples on a unit square, not measured data. \
                 POST /api/v1/geostatistics/interpolate to interpolate your own.",
        "grid_rows": result.grid_rows,
        "grid_cols": result.grid_cols,
        "statistics": result.statistics,
        "morans_i": geostatistics::morans_i(&samples, 1.5)
    }))
}

/// Routes for multispectral imagery.
pub fn multispectral_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/multispectral/indices", get(spectral_indices))
        .route("/api/v1/multispectral/sensors", get(spectral_sensors))
        .route("/api/v1/multispectral/demo", get(multispectral_demo))
}

async fn spectral_indices() -> Json<serde_json::Value> {
    let indices = multispectral::supported_indices();
    Json(serde_json::json!({ "indices": indices }))
}

async fn spectral_sensors() -> Json<serde_json::Value> {
    let sensors = multispectral::supported_sensors();
    Json(serde_json::json!({ "sensors": sensors }))
}

async fn multispectral_demo() -> Json<serde_json::Value> {
    let red = vec![0.1, 0.2, 0.3, 0.05, 0.15, 0.25, 0.08, 0.12, 0.18];
    let nir = vec![0.5, 0.4, 0.3, 0.8, 0.6, 0.35, 0.7, 0.55, 0.45];
    let ndvi = multispectral::compute_ndvi(&red, &nir);
    let blue = [0.05; 9];
    let evi = multispectral::compute_evi(&nir, &red, &blue);
    let classification = multispectral::classify_ndvi(&ndvi, 0.25);
    Json(serde_json::json!({
        "ndvi_values": ndvi,
        "evi_values": evi,
        "classification": classification,
        "statistics": {
            "ndvi_min": ndvi.iter().cloned().fold(f64::INFINITY, f64::min),
            "ndvi_max": ndvi.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            "ndvi_mean": ndvi.iter().sum::<f64>() / ndvi.len() as f64,
        }
    }))
}

// ─── OSM Buildings Routes ────────────────────────────────────────────────────

/// Routes for OSM building extrusion and 3D generation.
pub fn osm_buildings_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/osm-buildings/extrude", get(extrude_osm_buildings))
        .route("/api/v1/osm-buildings/parse", get(parse_osm_data))
        .route("/api/v1/osm-buildings/info", get(osm_buildings_info))
}

async fn extrude_osm_buildings() -> Json<serde_json::Value> {
    // Demo: extrude a sample set of buildings with Empire State Building tiers + neighbors
    let c = |x: f64, y: f64| osm_buildings::Coord2D { x, y };

    // Empire State Building — tiered profile (base, setback 1, setback 2, tower)
    let esb_base = osm_buildings::OsmBuilding {
        osm_id: 1001,
        footprint: vec![
            c(-73.9868, 40.7475),
            c(-73.9838, 40.7475),
            c(-73.9838, 40.7495),
            c(-73.9868, 40.7495),
            c(-73.9868, 40.7475),
        ],
        tags: osm_buildings::BuildingTags {
            building: "commercial".to_string(),
            height: Some(86.0),
            min_height: None,
            building_levels: Some(6),
            building_min_level: None,
            roof_shape: Some(osm_buildings::RoofShape::Flat),
            roof_height: None,
            name: Some("Empire State Building (base)".to_string()),
            building_colour: Some("#d4c5a9".to_string()),
            roof_colour: Some("#c4b599".to_string()),
        },
    };
    let esb_setback1 = osm_buildings::OsmBuilding {
        osm_id: 1002,
        footprint: vec![
            c(-73.9863, 40.7478),
            c(-73.9843, 40.7478),
            c(-73.9843, 40.7492),
            c(-73.9863, 40.7492),
            c(-73.9863, 40.7478),
        ],
        tags: osm_buildings::BuildingTags {
            building: "commercial".to_string(),
            height: Some(186.0),
            min_height: Some(86.0),
            building_levels: Some(25),
            building_min_level: Some(6),
            roof_shape: Some(osm_buildings::RoofShape::Flat),
            roof_height: None,
            name: Some("Empire State Building (setback 1)".to_string()),
            building_colour: Some("#cbb89c".to_string()),
            roof_colour: Some("#baa88c".to_string()),
        },
    };
    let esb_mid = osm_buildings::OsmBuilding {
        osm_id: 1003,
        footprint: vec![
            c(-73.9858, 40.7481),
            c(-73.9848, 40.7481),
            c(-73.9848, 40.7490),
            c(-73.9858, 40.7490),
            c(-73.9858, 40.7481),
        ],
        tags: osm_buildings::BuildingTags {
            building: "commercial".to_string(),
            height: Some(320.0),
            min_height: Some(186.0),
            building_levels: Some(50),
            building_min_level: Some(31),
            roof_shape: Some(osm_buildings::RoofShape::Flat),
            roof_height: None,
            name: Some("Empire State Building (mid)".to_string()),
            building_colour: Some("#c0a880".to_string()),
            roof_colour: Some("#b0987a".to_string()),
        },
    };
    let esb_tower = osm_buildings::OsmBuilding {
        osm_id: 1004,
        footprint: vec![
            c(-73.9856, 40.7483),
            c(-73.9850, 40.7483),
            c(-73.9850, 40.7488),
            c(-73.9856, 40.7488),
            c(-73.9856, 40.7483),
        ],
        tags: osm_buildings::BuildingTags {
            building: "commercial".to_string(),
            height: Some(443.0),
            min_height: Some(320.0),
            building_levels: Some(22),
            building_min_level: Some(81),
            roof_shape: Some(osm_buildings::RoofShape::Pyramidal),
            roof_height: Some(20.0),
            name: Some("Empire State Building (tower)".to_string()),
            building_colour: Some("#b89870".to_string()),
            roof_colour: Some("#8b7355".to_string()),
        },
    };

    // Surrounding buildings
    let neighbors = vec![
        osm_buildings::OsmBuilding {
            osm_id: 2001,
            footprint: vec![
                c(-73.9835, 40.7488),
                c(-73.9825, 40.7488),
                c(-73.9825, 40.7495),
                c(-73.9835, 40.7495),
                c(-73.9835, 40.7488),
            ],
            tags: osm_buildings::BuildingTags {
                building: "commercial".to_string(),
                height: Some(80.0),
                min_height: None,
                building_levels: Some(16),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Flat),
                roof_height: None,
                name: Some("Office Tower A".to_string()),
                building_colour: Some("#b8c4d0".to_string()),
                roof_colour: None,
            },
        },
        osm_buildings::OsmBuilding {
            osm_id: 2002,
            footprint: vec![
                c(-73.9875, 40.7476),
                c(-73.9865, 40.7476),
                c(-73.9865, 40.7484),
                c(-73.9875, 40.7484),
                c(-73.9875, 40.7476),
            ],
            tags: osm_buildings::BuildingTags {
                building: "commercial".to_string(),
                height: Some(120.0),
                min_height: None,
                building_levels: Some(28),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Flat),
                roof_height: None,
                name: Some("Office Tower B".to_string()),
                building_colour: Some("#a0b0c0".to_string()),
                roof_colour: None,
            },
        },
        osm_buildings::OsmBuilding {
            osm_id: 2003,
            footprint: vec![
                c(-73.9840, 40.7468),
                c(-73.9830, 40.7468),
                c(-73.9830, 40.7476),
                c(-73.9840, 40.7476),
                c(-73.9840, 40.7468),
            ],
            tags: osm_buildings::BuildingTags {
                building: "residential".to_string(),
                height: Some(65.0),
                min_height: None,
                building_levels: Some(14),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Gabled),
                roof_height: Some(3.0),
                name: Some("Residential Block".to_string()),
                building_colour: Some("#c8b090".to_string()),
                roof_colour: Some("#8b4513".to_string()),
            },
        },
        osm_buildings::OsmBuilding {
            osm_id: 2004,
            footprint: vec![
                c(-73.9870, 40.7492),
                c(-73.9860, 40.7492),
                c(-73.9860, 40.7500),
                c(-73.9870, 40.7500),
                c(-73.9870, 40.7492),
            ],
            tags: osm_buildings::BuildingTags {
                building: "commercial".to_string(),
                height: Some(95.0),
                min_height: None,
                building_levels: Some(20),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Hipped),
                roof_height: Some(5.0),
                name: Some("Hotel Plaza".to_string()),
                building_colour: Some("#d0c8b0".to_string()),
                roof_colour: Some("#6b5b47".to_string()),
            },
        },
        osm_buildings::OsmBuilding {
            osm_id: 2005,
            footprint: vec![
                c(-73.9828, 40.7478),
                c(-73.9818, 40.7478),
                c(-73.9818, 40.7485),
                c(-73.9828, 40.7485),
                c(-73.9828, 40.7478),
            ],
            tags: osm_buildings::BuildingTags {
                building: "commercial".to_string(),
                height: Some(150.0),
                min_height: None,
                building_levels: Some(35),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Flat),
                roof_height: None,
                name: Some("Glass Tower".to_string()),
                building_colour: Some("#90b8d8".to_string()),
                roof_colour: None,
            },
        },
        osm_buildings::OsmBuilding {
            osm_id: 2006,
            footprint: vec![
                c(-73.9880, 40.7488),
                c(-73.9872, 40.7488),
                c(-73.9872, 40.7496),
                c(-73.9880, 40.7496),
                c(-73.9880, 40.7488),
            ],
            tags: osm_buildings::BuildingTags {
                building: "office".to_string(),
                height: Some(50.0),
                min_height: None,
                building_levels: Some(12),
                building_min_level: None,
                roof_shape: Some(osm_buildings::RoofShape::Flat),
                roof_height: None,
                name: Some("Low-rise Office".to_string()),
                building_colour: Some("#c0c0c0".to_string()),
                roof_colour: None,
            },
        },
    ];

    let mut buildings = vec![esb_base, esb_setback1, esb_mid, esb_tower];
    buildings.extend(neighbors);
    let request = osm_buildings::ExtrudeBuildingsRequest {
        min_lon: -74.0,
        min_lat: 40.7,
        max_lon: -73.9,
        max_lat: 40.8,
        level_height_meters: None,
        default_height_meters: None,
        include_roof_geometry: Some(true),
        output_format: None,
    };
    let result = osm_buildings::extrude_buildings(&buildings, &request);
    let meshes: Vec<serde_json::Value> = result
        .buildings
        .iter()
        .map(|b| {
            serde_json::json!({
                "osm_id": b.osm_id,
                "name": b.name,
                "height": b.height,
                "min_height": b.min_height,
                "wall_color": b.wall_color,
                "roof_color": b.roof_color,
                "roof_shape": format!("{:?}", b.roof_shape),
                "vertices": b.vertices.iter().map(|v| [v.x, v.y, v.z]).collect::<Vec<_>>(),
                "normals": b.normals,
                "triangles": b.triangles.iter().map(|t| [t.v0, t.v1, t.v2]).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "buildings_extruded": result.buildings.len(),
        "total_vertices": result.total_vertices,
        "total_triangles": result.total_triangles,
        "bounding_box": {
            "min": result.bounding_box.min,
            "max": result.bounding_box.max,
        },
        "meshes": meshes,
        "sample": result.buildings.first().map(|b| serde_json::json!({
            "osm_id": b.osm_id,
            "name": b.name,
            "height": b.height,
            "wall_color": b.wall_color,
            "roof_color": b.roof_color,
            "vertex_count": b.vertices.len(),
            "triangle_count": b.triangles.len(),
        })),
    }))
}

async fn parse_osm_data() -> Json<serde_json::Value> {
    // Demo: parse sample Overpass response
    let sample = serde_json::json!({
        "elements": [
            {
                "type": "way",
                "id": 2001,
                "tags": {
                    "building": "residential",
                    "building:levels": "5",
                    "roof:shape": "gabled",
                    "name": "Sample Apartment"
                },
                "geometry": [
                    {"lon": 2.349, "lat": 48.864},
                    {"lon": 2.350, "lat": 48.864},
                    {"lon": 2.350, "lat": 48.865},
                    {"lon": 2.349, "lat": 48.865},
                    {"lon": 2.349, "lat": 48.864}
                ]
            }
        ]
    });
    let buildings = osm_buildings::parse_overpass_buildings(&sample);
    Json(serde_json::json!({
        "parsed_count": buildings.len(),
        "buildings": buildings.iter().map(|b| serde_json::json!({
            "osm_id": b.osm_id,
            "name": b.tags.name,
            "building_type": b.tags.building,
            "levels": b.tags.building_levels,
            "roof_shape": b.tags.roof_shape,
            "footprint_vertices": b.footprint.len(),
        })).collect::<Vec<_>>()
    }))
}

async fn osm_buildings_info() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "feature": "OSM Building Extrusion",
        "description": "Parse OpenStreetMap building footprints and extrude them into 3D meshes for visualization as 3D Tiles",
        "capabilities": [
            "Parse OSM Overpass API building data",
            "Extrude 2D polygons to 3D meshes with walls and caps",
            "Support building:levels, height, min_height tags",
            "Multiple roof shapes: flat, gabled, hipped, pyramidal, skillion, dome",
            "Custom building and roof colors from OSM tags",
            "Output as 3D Tiles, GLB, or GeoJSON",
            "Batch extrusion for entire city regions",
            "Multi-view consistency for depth fusion"
        ],
        "supported_tags": [
            "building", "height", "min_height", "building:levels",
            "building:min_level", "roof:shape", "roof:height",
            "building:colour", "roof:colour", "name"
        ],
        "output_formats": ["3dtiles", "glb", "geojson"],
        "roof_shapes": ["flat", "gabled", "hipped", "pyramidal", "skillion", "dome"],
        "competitive_note": "Equivalent to Cesium Ion OSM Buildings — fully self-hosted, no per-tile streaming fees"
    }))
}

// ─── Entity Linking Routes ──────────────────────────────────────────────────

/// Routes for entity linking (mapping external IDs to 3D assets).
pub fn entity_linking_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/entity-links", get(list_entity_links))
        .route(
            "/api/v1/entity-links/by-entity/{entity_id}",
            get(query_entity_links),
        )
        .route(
            "/api/v1/entity-links/nearby",
            get(query_entity_links_by_position),
        )
}

async fn list_entity_links(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let store = &state.entity_link_store;
    let links = store.list(None);
    Json(serde_json::json!({ "links": links }))
}

async fn query_entity_links(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(entity_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let store = &state.entity_link_store;
    let links = store.query_by_entity(&entity_id);
    Json(serde_json::json!({ "links": links }))
}

#[derive(Deserialize)]
struct NearbyQuery {
    x: f64,
    y: f64,
    z: f64,
    radius: Option<f64>,
}

async fn query_entity_links_by_position(
    State(state): State<Arc<AppState>>,
    Query(params): Query<NearbyQuery>,
) -> Json<serde_json::Value> {
    let store = &state.entity_link_store;
    let radius = params.radius.unwrap_or(100.0);
    let links = store.query_by_position([params.x, params.y, params.z], radius);
    Json(serde_json::json!({ "links": links }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isochrone_query(minutes: Option<&str>, profile: Option<&str>) -> IsochroneQuery {
        IsochroneQuery {
            lon: -122.4194,
            lat: 37.7749,
            minutes: minutes.map(str::to_string),
            profile: profile.map(str::to_string),
            concavity: None,
        }
    }

    fn reason(result: Result<isochrone::IsochroneRequest, (StatusCode, String)>) -> String {
        let (status, reason) = result.expect_err("expected a rejection");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        reason
    }

    #[test]
    fn test_isochrone_query_defaults() {
        let request = isochrone_query(None, None).into_request().unwrap();

        assert_eq!(request.origin, [-122.4194, 37.7749]);
        assert_eq!(request.contours_minutes, vec![5, 10, 15]);
        assert_eq!(request.profile, isochrone::TravelProfile::Driving);
        assert_eq!(request.concavity, itinera_core::DEFAULT_CONCAVITY);
    }

    #[test]
    fn test_isochrone_query_accepts_every_listed_profile() {
        for name in ISOCHRONE_PROFILES {
            assert!(
                isochrone_query(None, Some(name)).into_request().is_ok(),
                "profiles endpoint lists '{name}' but compute rejects it"
            );
        }
    }

    #[test]
    fn test_isochrone_query_rejects_unknown_profile() {
        let rejection = reason(isochrone_query(None, Some("teleport")).into_request());
        assert!(rejection.contains("teleport"), "{rejection}");
    }

    #[test]
    fn test_isochrone_query_rejects_unparseable_minutes() {
        let rejection = reason(isochrone_query(Some("5,soon,15"), None).into_request());
        assert!(rejection.contains("soon"), "{rejection}");
    }

    #[test]
    fn test_isochrone_query_rejects_out_of_range_origin() {
        let mut query = isochrone_query(None, None);
        query.lat = 91.0;
        reason(query.into_request());
    }

    #[test]
    fn test_isochrone_query_rejects_bad_concavity() {
        for concavity in [-1.0, f64::NAN] {
            let mut query = isochrone_query(None, None);
            query.concavity = Some(concavity);
            reason(query.into_request());
        }
    }

    #[test]
    fn test_isochrone_query_keeps_a_valid_concavity() {
        let mut query = isochrone_query(None, None);
        query.concavity = Some(0.5);

        assert_eq!(query.into_request().unwrap().concavity, 0.5);
    }

    /// POST a body to the real geostatistics route table and read the answer.
    async fn interpolate(body: serde_json::Value) -> (StatusCode, String) {
        use tower::ServiceExt;

        let response = geostatistics_routes::<()>()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/geostatistics/interpolate")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn geostat_samples(count: usize) -> Vec<serde_json::Value> {
        (0..count)
            .map(|i| {
                let step = i as f64;
                serde_json::json!({ "x": step, "y": (i % 7) as f64, "value": 10.0 + step })
            })
            .collect()
    }

    fn interpolate_body(method: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "samples": geostat_samples(9),
            "bounds": [0.0, 0.0, 8.0, 6.0],
            "resolution": 2.0,
            "method": method
        })
    }

    #[tokio::test]
    async fn interpolate_answers_a_kriged_grid_with_a_variance_per_cell() {
        let (status, body) =
            interpolate(interpolate_body(serde_json::json!("OrdinaryKriging"))).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let result: geostatistics::InterpolationResult = serde_json::from_str(&body).unwrap();
        assert_eq!(result.grid_cols, 4);
        assert_eq!(result.grid_rows, 3);
        assert_eq!(result.values.len(), 12);
        assert_eq!(result.variances.unwrap().len(), 12);
        assert!(result.values.iter().all(|v| v.is_finite()));
    }

    #[tokio::test]
    async fn interpolate_answers_the_three_kriging_methods_differently() {
        // cell 7 sits at (7, 3), off the samples so no method just repeats one
        const OFF_SAMPLE_CELL: usize = 7;
        let mut answers = Vec::new();
        for method in [
            serde_json::json!("OrdinaryKriging"),
            serde_json::json!("UniversalKriging"),
            serde_json::json!({ "SimpleKriging": { "known_mean": 0.0 } }),
        ] {
            let (status, body) = interpolate(interpolate_body(method.clone())).await;
            assert_eq!(status, StatusCode::OK, "{method}: {body}");
            let result: geostatistics::InterpolationResult = serde_json::from_str(&body).unwrap();
            answers.push((method, result.values[OFF_SAMPLE_CELL]));
        }

        for (left, right) in [(0, 1), (0, 2), (1, 2)] {
            assert!(
                (answers[left].1 - answers[right].1).abs() > 1e-6,
                "{} answered {} and {} answered {}",
                answers[left].0,
                answers[left].1,
                answers[right].0,
                answers[right].1
            );
        }
    }

    #[tokio::test]
    async fn interpolate_refuses_an_empty_sample_list() {
        let mut body = interpolate_body(serde_json::json!("OrdinaryKriging"));
        body["samples"] = serde_json::json!([]);

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("at least one sample"), "{reason}");
    }

    #[tokio::test]
    async fn interpolate_refuses_bounds_with_no_extent() {
        let mut body = interpolate_body(serde_json::json!("OrdinaryKriging"));
        body["bounds"] = serde_json::json!([8.0, 0.0, 8.0, 6.0]);

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("bounds"), "{reason}");
    }

    #[tokio::test]
    async fn interpolate_refuses_a_grid_past_the_cell_cap() {
        let mut body = interpolate_body(serde_json::json!({ "Idw": { "power": 2.0 } }));
        body["resolution"] = serde_json::json!(0.001);

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            reason.contains(&geostatistics::MAX_GRID_CELLS.to_string()),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn interpolate_refuses_more_samples_than_the_solve_accepts() {
        let mut body = interpolate_body(serde_json::json!("OrdinaryKriging"));
        body["samples"] = serde_json::json!(geostat_samples(geostatistics::MAX_SAMPLES + 1));

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            reason.contains(&geostatistics::MAX_SAMPLES.to_string()),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn interpolate_refuses_samples_stacked_at_one_location() {
        let mut body = interpolate_body(serde_json::json!("OrdinaryKriging"));
        body["samples"] = serde_json::json!([
            { "x": 1.0, "y": 1.0, "value": 10.0 },
            { "x": 1.0, "y": 1.0, "value": 12.0 },
            { "x": 4.0, "y": 3.0, "value": 14.0 },
        ]);

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("same location"), "{reason}");
    }

    #[tokio::test]
    async fn interpolate_refuses_a_singular_universal_kriging_system() {
        let mut body = interpolate_body(serde_json::json!("UniversalKriging"));
        // collinear samples leave the x and y drift rows dependent
        body["samples"] = serde_json::json!(
            (0..5)
                .map(|i| serde_json::json!({
                    "x": i as f64,
                    "y": 2.0 * i as f64,
                    "value": 10.0 + i as f64
                }))
                .collect::<Vec<_>>()
        );

        let (status, reason) = interpolate(body).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reason}");
        assert!(reason.contains("singular"), "{reason}");
    }
}
