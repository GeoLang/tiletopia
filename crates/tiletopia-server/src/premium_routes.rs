//! Premium feature API routes.
//!
//! Wires up all premium modules into axum routers.

use axum::{Json, Router, routing::get};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, api_keys, bim4d, classification, cog, collaboration, elevation, export,
    feature_service, flight_planning, geocoding, geoprocessing, geostatistics, indoor, isochrone,
    issue_tracking, map_matching, map_tiles, metering, mobile, multispectral, photogrammetry,
    plugins, routing, scan_registration, scheduler, stac, static_map, terrain_analysis, versioning,
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

// ─── Batch 2: Competitive gap-closing routes ────────────────────────────────

/// Routes for isochrone/travel-time analysis.
pub fn isochrone_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/isochrone/compute", get(compute_isochrone_demo))
        .route("/api/v1/isochrone/profiles", get(isochrone_profiles))
}

async fn compute_isochrone_demo() -> Json<serde_json::Value> {
    let request = isochrone::IsochroneRequest {
        origin: [-122.4194, 37.7749],
        profile: isochrone::TravelProfile::Driving,
        contours_minutes: vec![5, 10, 15],
        denoise: 0.5,
    };
    let result = isochrone::compute_isochrone(&request);
    Json(serde_json::json!(result))
}

async fn isochrone_profiles() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "profiles": ["Walking", "Cycling", "Driving", "PublicTransit"]
    }))
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
        .route("/api/v1/features/query", get(query_features_demo))
}

async fn list_feature_layers() -> Json<serde_json::Value> {
    let engine = feature_service::FeatureServiceEngine::new();
    let layers = engine.list_layers();
    Json(serde_json::json!({ "layers": layers }))
}

async fn query_features_demo() -> Json<serde_json::Value> {
    let engine = feature_service::FeatureServiceEngine::new();
    let layers = engine.list_layers();
    if let Some(layer) = layers.first() {
        let query = feature_service::SpatialQuery {
            bbox: None,
            intersects: None,
            within_distance_m: None,
            where_clause: None,
            limit: 100,
            offset: 0,
            order_by: None,
        };
        let features = engine.query_features(layer.id, &query);
        Json(serde_json::json!({ "type": "FeatureCollection", "features": features }))
    } else {
        Json(serde_json::json!({ "type": "FeatureCollection", "features": [] }))
    }
}

/// Routes for elevation service.
pub fn elevation_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/elevation/point", get(elevation_point))
        .route("/api/v1/elevation/profile", get(elevation_profile_demo))
}

async fn elevation_point() -> Json<serde_json::Value> {
    let elev = elevation::get_elevation(37.7749, -122.4194);
    Json(serde_json::json!(elev))
}

async fn elevation_profile_demo() -> Json<serde_json::Value> {
    let path = vec![[-122.42, 37.77], [-122.41, 37.78], [-122.40, 37.79]];
    let profile = elevation::get_profile(&path);
    Json(serde_json::json!(profile))
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
        .route("/api/v1/static-map/render", get(static_map_info))
        .route("/api/v1/static-map/formats", get(static_map_formats))
}

async fn static_map_info() -> Json<serde_json::Value> {
    let req = static_map::StaticMapRequest {
        center: Some([-122.4194, 37.7749]),
        zoom: Some(14.0),
        bbox: None,
        width: 800,
        height: 600,
        format: static_map::ImageFormat::Png,
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

async fn list_issues() -> Json<serde_json::Value> {
    let tracker = issue_tracking::IssueTracker::new();
    let issues = tracker.list_issues(None);
    Json(serde_json::json!({ "issues": issues }))
}

async fn issue_stats() -> Json<serde_json::Value> {
    let tracker = issue_tracking::IssueTracker::new();
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
pub fn geostatistics_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/geostatistics/methods", get(geostat_methods))
        .route("/api/v1/geostatistics/demo", get(geostat_demo))
}

async fn geostat_methods() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "methods": ["IDW", "OrdinaryKriging", "UniversalKriging", "SimpleKriging"],
        "variogram_models": ["Spherical", "Exponential", "Gaussian", "Linear", "Power"]
    }))
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
    );
    Json(serde_json::json!({
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
    let classification = multispectral::classify_ndvi(&ndvi, 0.25);
    Json(serde_json::json!({
        "ndvi_values": ndvi,
        "classification": classification,
        "min": ndvi.iter().cloned().fold(f64::INFINITY, f64::min),
        "max": ndvi.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
    }))
}
