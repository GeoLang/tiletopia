//! Premium feature API routes.
//!
//! Wires up all premium modules into axum routers.

use axum::{Json, Router, routing::get};
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, api_keys, export, metering, mobile, plugins, scheduler, webhooks, workspaces,
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
