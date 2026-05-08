//! Premium feature API routes.
//!
//! Wires up all premium modules into axum routers.

use axum::{Json, Router, routing::get};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, api_keys, bim4d, classification, cog, collaboration, export, geocoding, indoor,
    map_tiles, metering, mobile, photogrammetry, plugins, routing, scheduler, stac, versioning,
    webhooks, workspaces,
};

/// Routes for API key management.
pub fn api_key_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/api-keys", get(list_api_keys))
        .route("/api/v1/api-keys/usage", get(get_usage))
}

async fn list_api_keys() -> Json<serde_json::Value> {
    let store = api_keys::ApiKeyStore::new();
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

async fn get_usage() -> Json<serde_json::Value> {
    let store = api_keys::ApiKeyStore::new();
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

async fn metering_summary() -> Json<serde_json::Value> {
    let store = metering::MeteringStore::new();
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

async fn list_webhooks() -> Json<serde_json::Value> {
    let engine = webhooks::WebhookEngine::new();
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

async fn list_orgs() -> Json<serde_json::Value> {
    let store = workspaces::WorkspaceStore::new();
    let orgs = store.list_orgs().await;
    Json(serde_json::json!({ "organizations": orgs }))
}

async fn list_teams() -> Json<serde_json::Value> {
    let store = workspaces::WorkspaceStore::new();
    let orgs = store.list_orgs().await;
    if let Some(org) = orgs.first() {
        let teams = store.list_teams(org.id).await;
        Json(serde_json::json!({ "teams": teams }))
    } else {
        Json(serde_json::json!({ "teams": [] }))
    }
}

async fn list_projects() -> Json<serde_json::Value> {
    let store = workspaces::WorkspaceStore::new();
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
    Router::new()
        .route("/api/v1/exports", get(list_exports))
        .route("/api/v1/exports/formats", get(export_formats))
}

async fn list_exports() -> Json<serde_json::Value> {
    let engine = export::ExportEngine::new();
    let jobs = engine.list_exports(None).await;
    Json(serde_json::json!({ "exports": jobs }))
}

async fn export_formats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "formats": [
            {"id": "3dtiles_zip", "name": "3D Tiles (ZIP)", "extension": ".zip"},
            {"id": "las", "name": "LAS 1.4", "extension": ".las"},
            {"id": "laz", "name": "LAZ (compressed)", "extension": ".laz"},
            {"id": "terrain_bundle", "name": "Terrain Bundle", "extension": ".zip"},
            {"id": "geojson", "name": "GeoJSON", "extension": ".geojson"},
            {"id": "png", "name": "Rendered Image", "extension": ".png"},
            {"id": "citygml", "name": "CityGML", "extension": ".gml"},
            {"id": "obj", "name": "OBJ Mesh", "extension": ".obj"},
            {"id": "glb", "name": "glTF Binary", "extension": ".glb"}
        ]
    }))
}

/// Routes for the scheduler.
pub fn scheduler_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/scheduler/jobs", get(list_scheduled_jobs))
        .route("/api/v1/scheduler/stats", get(scheduler_stats))
        .route("/api/v1/scheduler/runs", get(recent_runs))
}

async fn list_scheduled_jobs() -> Json<serde_json::Value> {
    let sched = scheduler::Scheduler::new();
    let jobs = sched.list_jobs(None).await;
    Json(serde_json::json!({ "jobs": jobs }))
}

async fn scheduler_stats() -> Json<scheduler::SchedulerStats> {
    let sched = scheduler::Scheduler::new();
    Json(sched.stats().await)
}

async fn recent_runs() -> Json<serde_json::Value> {
    let sched = scheduler::Scheduler::new();
    let runs = sched.recent_runs(20).await;
    Json(serde_json::json!({ "runs": runs }))
}

/// Routes for plugins/marketplace.
pub fn plugin_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/plugins", get(list_plugins))
        .route("/api/v1/plugins/pipelines", get(list_pipelines))
}

async fn list_plugins() -> Json<serde_json::Value> {
    let registry = plugins::PluginRegistry::new();
    let all = registry.list_plugins(None).await;
    Json(serde_json::json!({ "plugins": all }))
}

async fn list_pipelines() -> Json<serde_json::Value> {
    let registry = plugins::PluginRegistry::new();
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

async fn list_photogrammetry_projects() -> Json<serde_json::Value> {
    let engine = photogrammetry::PhotogrammetryEngine::new();
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

async fn list_classes() -> Json<serde_json::Value> {
    let engine = classification::ClassificationEngine::new();
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

async fn list_collaboration_sessions() -> Json<serde_json::Value> {
    let engine = collaboration::CollaborationEngine::new();
    let sessions = engine.list_sessions().await;
    Json(serde_json::json!({ "sessions": sessions }))
}

/// Routes for asset versioning.
pub fn versioning_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/versioning/assets", get(list_versioned_assets))
}

async fn list_versioned_assets() -> Json<serde_json::Value> {
    let engine = versioning::VersioningEngine::new();
    let assets = engine.list_assets().await;
    Json(serde_json::json!({ "assets": assets }))
}

/// Routes for BIM 4D scheduling.
pub fn bim4d_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/bim4d/projects", get(list_bim4d_projects))
}

async fn list_bim4d_projects() -> Json<serde_json::Value> {
    let engine = bim4d::Bim4DEngine::new();
    let projects = engine.list_projects().await;
    Json(serde_json::json!({ "projects": projects }))
}

/// Routes for geocoding.
pub fn geocoding_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/geocoding/search", get(geocode_search))
        .route("/api/v1/geocoding/reverse", get(geocode_reverse))
}

async fn geocode_search() -> Json<serde_json::Value> {
    let result = geocoding::geocode("Golden Gate Bridge");
    Json(serde_json::json!(result))
}

async fn geocode_reverse() -> Json<serde_json::Value> {
    let place = geocoding::reverse_geocode(37.7749, -122.4194);
    Json(serde_json::json!(place))
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

async fn stac_search() -> Json<serde_json::Value> {
    let items = stac::search_items(None, None, None, 10);
    Json(serde_json::json!({ "type": "FeatureCollection", "features": items }))
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
        .route("/api/v1/cog/stats", get(cog_stats))
}

async fn list_cog_datasets() -> Json<serde_json::Value> {
    let engine = cog::CogEngine::new();
    let datasets = engine.list_datasets().to_vec();
    Json(serde_json::json!({ "datasets": datasets }))
}

async fn cog_stats() -> Json<serde_json::Value> {
    let engine = cog::CogEngine::new();
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
        .route("/api/v1/routing/route", get(compute_demo_route))
}

async fn routing_stats() -> Json<routing::RoutingStats> {
    let engine = routing::RoutingEngine::new();
    Json(engine.stats())
}

async fn compute_demo_route() -> Json<serde_json::Value> {
    let engine = routing::RoutingEngine::new();
    let req = routing::RouteRequest {
        origin: [-122.4194, 37.7749],
        destination: [-122.4100, 37.7800],
        profile: routing::RoutingProfile::Driving,
        alternatives: false,
    };
    match engine.compute_route(&req) {
        Some(route) => Json(serde_json::json!(route)),
        None => Json(serde_json::json!({"error": "No route found"})),
    }
}

/// Routes for 2D map tiles (XYZ, MVT, styles).
pub fn map_tile_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/tiles/sources", get(list_tile_sources))
        .route("/api/v1/tiles/styles", get(list_tile_styles))
        .route("/api/v1/tiles/cache/stats", get(tile_cache_stats))
        .route("/api/v1/tiles/layers", get(list_vector_layers))
}

async fn list_tile_sources() -> Json<serde_json::Value> {
    let engine = map_tiles::MapTileEngine::new();
    let sources = engine.list_sources().to_vec();
    Json(serde_json::json!({ "sources": sources }))
}

async fn list_tile_styles() -> Json<serde_json::Value> {
    let engine = map_tiles::MapTileEngine::new();
    let styles = engine.list_styles().to_vec();
    Json(serde_json::json!({ "styles": styles }))
}

async fn tile_cache_stats() -> Json<map_tiles::CacheStats> {
    let engine = map_tiles::MapTileEngine::new();
    Json(engine.cache_stats().clone())
}

async fn list_vector_layers() -> Json<serde_json::Value> {
    let engine = map_tiles::MapTileEngine::new();
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
