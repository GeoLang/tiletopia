//! Premium feature API routes.
//!
//! Wires up all premium modules into axum routers.

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{FromRef, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    AppState, api_keys,
    audit::AuditedResource,
    classification, cog, elevation,
    export::{EXPORT_FORMATS, ExportFormat, ExportJob, ExportStatus},
    feature_service, flight_planning, geocoding, geoprocessing, geostatistics, indoor, isochrone,
    map_matching, map_tiles, metering, mobile, multispectral, osm_buildings, routing,
    scan_registration, scheduler, stac, static_map, terrain_analysis,
    terrain_api::Refusal,
    users, webhooks,
};

/// API key management, and per-key usage.
///
/// Minting, listing, revoking and deleting are admin-only: there is no
/// self-service key management, so every key on the server was created by an
/// admin and the listing is the whole set. Usage is the one route a key may read
/// about itself, so it sits outside the admin gate and the handler decides.
pub fn api_key_routes() -> Router<Arc<AppState>> {
    let management = Router::new()
        .route(
            "/api/v1/api-keys",
            get(list_api_keys).post(create_api_key_route),
        )
        .route(
            "/api/v1/api-keys/{id}",
            axum::routing::delete(delete_api_key_route),
        )
        .route("/api/v1/api-keys/{id}/revoke", post(revoke_api_key_route))
        .layer(middleware::from_fn(users::require_admin));

    Router::new()
        .route("/api/v1/api-keys/usage", get(get_usage))
        .merge(management)
}

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub permissions: Vec<String>,
    pub tier: String,
    /// RFC 3339, and in the future. Absent means the key never expires.
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Longest key name accepted, so a listing stays readable and a name cannot be
/// used as bulk storage.
const MAX_KEY_NAME_CHARS: usize = 120;

/// A refusal carrying the reason as JSON, the same shape the credential path in
/// [`crate::auth`] answers with.
fn refuse_as_json(status: StatusCode, reason: String) -> Refusal {
    (status, Json(serde_json::json!({ "error": reason })))
        .into_response()
        .into()
}

fn bad_json_request(reason: String) -> Refusal {
    refuse_as_json(StatusCode::BAD_REQUEST, reason)
}

/// The request as the fields a key is built from, or the refusal. Unknown
/// permissions and tiers are named back to the caller and refused, never
/// widened or narrowed to something that parses.
fn parse_create_request(
    request: &CreateApiKeyRequest,
) -> Result<(Vec<api_keys::Permission>, api_keys::RateLimitTier), Refusal> {
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > MAX_KEY_NAME_CHARS {
        return Err(bad_json_request(format!(
            "name must be 1 to {MAX_KEY_NAME_CHARS} characters"
        )));
    }

    let tier = api_keys::RateLimitTier::from_name(&request.tier).ok_or_else(|| {
        let known: Vec<&str> = api_keys::RateLimitTier::ALL
            .iter()
            .map(|tier| tier.name())
            .collect();
        bad_json_request(format!(
            "unknown tier '{}' (expected {})",
            request.tier,
            known.join(", ")
        ))
    })?;

    if request.permissions.is_empty() {
        return Err(bad_json_request(
            "a key needs at least one permission".into(),
        ));
    }
    let mut permissions = Vec::new();
    for name in &request.permissions {
        let permission = api_keys::Permission::from_name(name).ok_or_else(|| {
            let known: Vec<&str> = api_keys::Permission::ALL
                .iter()
                .map(|permission| permission.name())
                .collect();
            bad_json_request(format!(
                "unknown permission '{name}' (expected {})",
                known.join(", ")
            ))
        })?;
        if !permissions.contains(&permission) {
            permissions.push(permission);
        }
    }

    if let Some(expires_at) = request.expires_at
        && expires_at <= chrono::Utc::now()
    {
        return Err(bad_json_request("expires_at is in the past".into()));
    }

    Ok((permissions, tier))
}

/// Mint a key. The plaintext is in this response and nowhere else: what is
/// stored is its SHA-256 digest, so a lost key is replaced, never recovered.
async fn create_api_key_route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<
    (
        StatusCode,
        Extension<AuditedResource>,
        Json<serde_json::Value>,
    ),
    Refusal,
> {
    // behind require_admin, so a valid admin token is always present
    let created_by = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?
        .sub;
    let (permissions, tier) = parse_create_request(&request)?;

    let plaintext = api_keys::generate_key();
    let key_hash =
        api_keys::hash_presented_key(&plaintext).expect("a freshly generated key is well formed");
    let key = api_keys::ApiKey {
        id: Uuid::new_v4(),
        name: request.name.trim().to_string(),
        key_hash,
        permissions,
        tier,
        created_by,
        created_at: chrono::Utc::now(),
        last_used_at: None,
        expires_at: request.expires_at,
        revoked: false,
    };

    state
        .db
        .create_api_key(&key)
        .await
        .map_err(|error| write_failed("storing", error))?;

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(key.id.to_string())),
        Json(serde_json::json!({
            "key": plaintext,
            "warning": "this is the only time the key is shown",
            "api_key": key,
        })),
    ))
}

/// Every key the server holds, metadata only: no plaintext, and no digest
/// either (see the `key_hash` field of [`api_keys::ApiKey`]).
async fn list_api_keys(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let keys = state.db.list_api_keys().await.map_err(read_failed)?;
    Ok(Json(serde_json::json!({ "keys": keys })))
}

/// Kill a key, keeping the row. A revoked key stays refused forever, and the
/// listing still shows that it existed.
async fn revoke_api_key_route(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Refusal> {
    let revoked = state
        .db
        .revoke_api_key(id)
        .await
        .map_err(|error| write_failed("revoking", error))?;
    if revoked == 0 {
        return Err(refuse_as_json(StatusCode::NOT_FOUND, "no such key".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Drop the row entirely, for a key an admin wants out of the listing. Revoking
/// is what a leaked key needs; this is cleanup.
async fn delete_api_key_route(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Refusal> {
    let deleted = state
        .db
        .delete_api_key(id)
        .await
        .map_err(|error| write_failed("deleting", error))?;
    if deleted == 0 {
        return Err(refuse_as_json(StatusCode::NOT_FOUND, "no such key".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn read_failed(error: sqlx::Error) -> Refusal {
    tracing::error!("reading api keys failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        "reading the keys failed".into(),
    )
}

fn write_failed(what: &str, error: sqlx::Error) -> Refusal {
    tracing::error!("{what} an api key failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{what} the key failed"),
    )
}

/// Today's request count. A key sees its own; an admin sees every key.
async fn get_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    key: Option<axum::Extension<api_keys::ApiKey>>,
) -> Result<Json<serde_json::Value>, Refusal> {
    // the middleware resolved the key, so a caller presenting one reads only its
    // own row without a second lookup
    let keys = match key {
        Some(axum::Extension(key)) => vec![key],
        None => {
            let claims = users::claims_from_headers(&headers)
                .map_err(|status| Refusal::from(status.into_response()))?;
            if !claims.can_admin() {
                return Err(refuse_as_json(StatusCode::FORBIDDEN, "admin only".into()));
            }
            state.db.list_api_keys().await.map_err(read_failed)?
        }
    };

    let mut usage = Vec::with_capacity(keys.len());
    for key in keys {
        let today = state.api_key_rate_limiter.get_usage(key.id).await;
        usage.push(serde_json::json!({
            "key_id": key.id,
            "name": key.name,
            "tier": key.tier,
            "requests_per_second": key.rate_limit().requests_per_second,
            "requests_per_day": key.rate_limit().requests_per_day,
            "requests_today": today.map(|today| today.requests_today).unwrap_or(0),
            "resets_at": today.map(|today| today.resets_at),
        }));
    }
    Ok(Json(serde_json::json!({ "usage": usage })))
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

/// Webhook subscriptions and their delivery history.
///
/// Creating, changing and deleting one is a write, so it takes the Edit tier
/// like an asset write, plus the creator-or-admin check the handlers do. The
/// reads need a valid token because the answer depends on who is asking: a
/// caller sees its own subscriptions and their deliveries, an admin sees every
/// one, the same stance the asset listing takes.
pub fn webhook_routes() -> Router<Arc<AppState>> {
    let write_routes = Router::new()
        .route("/api/v1/webhooks", post(create_webhook))
        .route(
            "/api/v1/webhooks/{id}",
            axum::routing::put(update_webhook).delete(delete_webhook),
        )
        .layer(middleware::from_fn(users::require_editor));

    Router::new()
        .route("/api/v1/webhooks", get(list_webhooks))
        .route("/api/v1/webhooks/events", get(webhook_event_types))
        .route("/api/v1/webhooks/deliveries", get(list_webhook_deliveries))
        .merge(write_routes)
}

/// Longest target URL accepted, so a URL cannot be used as bulk storage.
const MAX_WEBHOOK_URL_CHARS: usize = 2048;

/// Finished deliveries the history route answers with.
const WEBHOOK_DELIVERY_PAGE: usize = 50;

#[derive(Debug, Deserialize)]
pub struct WebhookSubscriptionRequest {
    pub url: String,
    pub events: Vec<String>,
    /// Absent means active. A paused subscription keeps its row and its secret
    /// and is skipped by delivery.
    #[serde(default = "active_by_default")]
    pub active: bool,
}

fn active_by_default() -> bool {
    true
}

/// The request as the fields a subscription is built from, or the refusal. An
/// unknown event name is named back to the caller, never dropped: a subscription
/// that silently wants fewer events than it asked for is worse than a refusal.
fn parse_webhook_request(
    request: &WebhookSubscriptionRequest,
) -> Result<(String, Vec<webhooks::WebhookEvent>), Refusal> {
    let url = request.url.trim();
    if url.chars().count() > MAX_WEBHOOK_URL_CHARS {
        return Err(bad_json_request(format!(
            "url must be at most {MAX_WEBHOOK_URL_CHARS} characters"
        )));
    }
    // absolute http(s) only: a relative target has nothing to send to, and any
    // other scheme would ask reqwest for something it does not speak. Userinfo
    // is refused so a subscription cannot smuggle credentials into the request
    // this server makes.
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| bad_json_request(format!("url is not a URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(bad_json_request(format!(
            "url scheme '{}' is not http or https",
            parsed.scheme()
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(bad_json_request(
            "url must not carry a username or password".into(),
        ));
    }

    if request.events.is_empty() {
        return Err(bad_json_request(
            "a subscription needs at least one event".into(),
        ));
    }
    let mut events = Vec::new();
    for name in &request.events {
        let event = webhooks::WebhookEvent::from_name(name).ok_or_else(|| {
            let known: Vec<&str> = webhooks::WebhookEvent::ALL
                .iter()
                .map(|event| event.name())
                .collect();
            bad_json_request(format!(
                "unknown event '{name}' (expected {})",
                known.join(", ")
            ))
        })?;
        if !events.contains(&event) {
            events.push(event);
        }
    }

    Ok((url.to_string(), events))
}

fn webhook_read_failed(error: sqlx::Error) -> Refusal {
    tracing::error!("reading webhook subscriptions failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        "reading the subscriptions failed".into(),
    )
}

fn webhook_write_failed(what: &str, error: sqlx::Error) -> Refusal {
    tracing::error!("{what} a webhook subscription failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{what} the subscription failed"),
    )
}

/// The subscription this caller may change, or the refusal. Behind
/// `require_editor`, so a valid token is always present and only the per-row
/// creator-or-admin rule is left to check. A subscription somebody else created
/// is a 404, not a 403: an editor has no business learning that it exists.
async fn writable_subscription(
    state: &AppState,
    id: Uuid,
    headers: &HeaderMap,
) -> Result<webhooks::WebhookSubscription, Refusal> {
    let claims = users::claims_from_headers(headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    let subscription = state
        .db
        .get_webhook_subscription(id)
        .await
        .map_err(webhook_read_failed)?
        .filter(|subscription| claims.can_admin() || subscription.created_by == claims.sub)
        .ok_or_else(|| refuse_as_json(StatusCode::NOT_FOUND, "no such subscription".into()))?;
    Ok(subscription)
}

/// Register a subscription. The secret is in this response and nowhere else:
/// signing needs it, so it is stored as it is, but no listing hands it back and
/// a lost one is replaced by a new subscription.
async fn create_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<WebhookSubscriptionRequest>,
) -> Result<
    (
        StatusCode,
        Extension<AuditedResource>,
        Json<serde_json::Value>,
    ),
    Refusal,
> {
    // behind require_editor, so a valid token is always present
    let created_by = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?
        .sub;
    let (url, events) = parse_webhook_request(&request)?;

    let subscription = webhooks::WebhookSubscription {
        id: Uuid::new_v4(),
        url,
        events,
        secret: webhooks::generate_secret(),
        created_by,
        active: request.active,
        created_at: chrono::Utc::now(),
    };
    state
        .db
        .create_webhook_subscription(&subscription)
        .await
        .map_err(|error| webhook_write_failed("storing", error))?;

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(subscription.id.to_string())),
        Json(serde_json::json!({
            "secret": subscription.secret,
            "warning": "this is the only time the secret is shown",
            "signature_header": webhooks::SIGNATURE_HEADER,
            "subscription": subscription,
        })),
    ))
}

async fn update_webhook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<WebhookSubscriptionRequest>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let mut subscription = writable_subscription(&state, id, &headers).await?;
    let (url, events) = parse_webhook_request(&request)?;

    state
        .db
        .update_webhook_subscription(id, &url, &events, request.active)
        .await
        .map_err(|error| webhook_write_failed("updating", error))?;

    subscription.url = url;
    subscription.events = events;
    subscription.active = request.active;
    Ok(Json(serde_json::json!({ "subscription": subscription })))
}

async fn delete_webhook(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, Refusal> {
    writable_subscription(&state, id, &headers).await?;
    state
        .db
        .delete_webhook_subscription(id)
        .await
        .map_err(|error| webhook_write_failed("deleting", error))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's subscriptions, or every one for an admin, and how many
/// deliveries are waiting for an attempt.
async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Refusal> {
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    let subscriptions: Vec<webhooks::WebhookSubscription> = state
        .db
        .list_webhook_subscriptions()
        .await
        .map_err(webhook_read_failed)?
        .into_iter()
        .filter(|subscription| claims.can_admin() || subscription.created_by == claims.sub)
        .collect();

    Ok(Json(serde_json::json!({
        "subscriptions": subscriptions,
        "pending_deliveries": state.webhooks.pending_count().await,
    })))
}

/// Deliveries this server has finished attempting, newest first, for the
/// caller's own subscriptions or for every one when an admin asks.
async fn list_webhook_deliveries(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Refusal> {
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    let mine: Vec<Uuid> = state
        .db
        .list_webhook_subscriptions()
        .await
        .map_err(webhook_read_failed)?
        .into_iter()
        .filter(|subscription| claims.can_admin() || subscription.created_by == claims.sub)
        .map(|subscription| subscription.id)
        .collect();

    let deliveries: Vec<webhooks::WebhookDelivery> = state
        .webhooks
        .recent_deliveries(WEBHOOK_DELIVERY_PAGE)
        .await
        .into_iter()
        .filter(|delivery| mine.contains(&delivery.subscription_id))
        .collect();

    Ok(Json(serde_json::json!({ "deliveries": deliveries })))
}

/// Every event a subscription can ask for, which is every event the server
/// emits.
async fn webhook_event_types() -> Json<serde_json::Value> {
    let names: Vec<&str> = webhooks::WebhookEvent::ALL
        .iter()
        .map(|event| event.name())
        .collect();
    Json(serde_json::json!({ "event_types": names }))
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
) -> Result<(StatusCode, Extension<AuditedResource>, Json<ExportJob>), StatusCode> {
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

    Ok((
        StatusCode::ACCEPTED,
        Extension(AuditedResource(job_id.to_string())),
        Json(job),
    ))
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
    // an expired job says so rather than reading as one that never finished:
    // only expire_due sets Expired, and it needs an expires_at to compare
    if let Some(expired_at) = job
        .expires_at
        .filter(|_| job.status == ExportStatus::Expired)
    {
        return Ok((
            StatusCode::GONE,
            Json(serde_json::json!({
                "error": "this export has expired and its files have been retired",
                "expired_at": expired_at.to_rfc3339(),
            })),
        )
            .into_response());
    }
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

/// Scheduled jobs, and what the worker has done with them.
///
/// Creating, enabling, disabling and deleting one is a write, so it takes the
/// Edit tier like an asset write, plus the creator-or-admin check the handlers
/// do. The reads need a valid token because the answer depends on who is asking:
/// a caller sees its own jobs, an admin sees every one, the same stance the
/// webhook and asset listings take.
pub fn scheduler_routes() -> Router<Arc<AppState>> {
    let write_routes = Router::new()
        .route("/api/v1/scheduler/jobs", post(create_scheduled_job))
        .route(
            "/api/v1/scheduler/jobs/{id}",
            axum::routing::put(update_scheduled_job).delete(delete_scheduled_job),
        )
        .layer(middleware::from_fn(users::require_editor));

    Router::new()
        .route("/api/v1/scheduler/jobs", get(list_scheduled_jobs))
        .route("/api/v1/scheduler/actions", get(scheduler_action_kinds))
        .route("/api/v1/scheduler/jobs/{id}", get(get_scheduled_job))
        .merge(write_routes)
}

/// Longest job name accepted, so a listing stays readable and a name cannot be
/// used as bulk storage.
const MAX_SCHEDULED_JOB_NAME_CHARS: usize = 120;

#[derive(Debug, Deserialize)]
pub struct CreateScheduledJobRequest {
    pub name: String,
    /// The action, tagged by `kind`. `GET /api/v1/scheduler/actions` lists them.
    pub action: serde_json::Value,
    /// The schedule, tagged by `kind`: `interval`, `cron` or `one_shot`.
    pub schedule: serde_json::Value,
    /// Absent means enabled. A disabled job keeps its row and never comes due.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// The request as the name, action and schedule a job is built from, or the
/// refusal. An action or a schedule this server does not run is named back to
/// the caller, never widened to something that parses.
fn parse_scheduled_job_request(
    request: &CreateScheduledJobRequest,
) -> Result<(String, scheduler::ScheduledAction, scheduler::Schedule), Refusal> {
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > MAX_SCHEDULED_JOB_NAME_CHARS {
        return Err(bad_json_request(format!(
            "name must be 1 to {MAX_SCHEDULED_JOB_NAME_CHARS} characters"
        )));
    }

    let action: scheduler::ScheduledAction = serde_json::from_value(request.action.clone())
        .map_err(|error| {
            bad_json_request(format!(
                "action is not one this server runs (expected {}): {error}",
                scheduler::ScheduledAction::KINDS.join(", ")
            ))
        })?;
    action.check().map_err(bad_json_request)?;

    let schedule: scheduler::Schedule =
        serde_json::from_value(request.schedule.clone()).map_err(|error| {
            bad_json_request(format!(
                "schedule is not one this server keeps (expected {}): {error}",
                scheduler::Schedule::KINDS.join(", ")
            ))
        })?;

    Ok((name.to_string(), action, schedule))
}

#[derive(Debug, Deserialize)]
pub struct UpdateScheduledJobRequest {
    pub enabled: bool,
}

fn scheduled_job_read_failed(error: sqlx::Error) -> Refusal {
    tracing::error!("reading scheduled jobs failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        "reading the scheduled jobs failed".into(),
    )
}

fn scheduled_job_write_failed(what: &str, error: sqlx::Error) -> Refusal {
    tracing::error!("{what} a scheduled job failed: {error}");
    refuse_as_json(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{what} the scheduled job failed"),
    )
}

/// The job this caller may change, or the refusal. Behind `require_editor`, so a
/// valid token is always present and only the per-row creator-or-admin rule is
/// left to check. A job somebody else created is a 404, not a 403: an editor has
/// no business learning that it exists.
async fn writable_scheduled_job(
    state: &AppState,
    id: Uuid,
    headers: &HeaderMap,
) -> Result<scheduler::ScheduledJob, Refusal> {
    let claims = users::claims_from_headers(headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    state
        .db
        .get_scheduled_job(id)
        .await
        .map_err(scheduled_job_read_failed)?
        .filter(|job| claims.can_admin() || job.created_by == claims.sub)
        .ok_or_else(|| refuse_as_json(StatusCode::NOT_FOUND, "no such scheduled job".into()))
}

/// Schedule a job. The first run comes from the schedule, so a cron expression
/// this server cannot read and a one-shot already in the past are each a 400
/// rather than a row that never runs.
async fn create_scheduled_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CreateScheduledJobRequest>,
) -> Result<
    (
        StatusCode,
        Extension<AuditedResource>,
        Json<serde_json::Value>,
    ),
    Refusal,
> {
    // behind require_editor, so a valid token is always present
    let created_by = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?
        .sub;

    let (name, action, schedule) = parse_scheduled_job_request(&request)?;
    let first_run = schedule
        .first_run(chrono::Utc::now())
        .map_err(bad_json_request)?;

    let job = scheduler::ScheduledJob {
        id: Uuid::new_v4(),
        name,
        action,
        schedule,
        enabled: request.enabled,
        created_by,
        created_at: chrono::Utc::now(),
        next_run: Some(first_run),
        last_run: None,
        last_outcome: None,
        run_count: 0,
        consecutive_failures: 0,
    };
    state
        .db
        .create_scheduled_job(&job)
        .await
        .map_err(|error| scheduled_job_write_failed("storing", error))?;

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(job.id.to_string())),
        Json(serde_json::json!({ "job": job })),
    ))
}

/// Enable or disable a job. Enabling one recomputes the next run from now, so a
/// job that sat disabled past its time does not fire the moment it comes back.
async fn update_scheduled_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(request): Json<UpdateScheduledJobRequest>,
) -> Result<Json<serde_json::Value>, Refusal> {
    let mut job = writable_scheduled_job(&state, id, &headers).await?;

    let next_run = if request.enabled {
        job.schedule.first_run(chrono::Utc::now()).ok()
    } else {
        job.next_run
    };
    state
        .db
        .set_scheduled_job_enabled(id, request.enabled, next_run)
        .await
        .map_err(|error| scheduled_job_write_failed("updating", error))?;

    job.enabled = request.enabled;
    job.next_run = next_run;
    Ok(Json(serde_json::json!({ "job": job })))
}

async fn delete_scheduled_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<StatusCode, Refusal> {
    writable_scheduled_job(&state, id, &headers).await?;
    state
        .db
        .delete_scheduled_job(id)
        .await
        .map_err(|error| scheduled_job_write_failed("deleting", error))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's jobs, or every one for an admin, out of the database.
async fn list_scheduled_jobs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Refusal> {
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    let jobs: Vec<scheduler::ScheduledJob> = state
        .db
        .list_scheduled_jobs()
        .await
        .map_err(scheduled_job_read_failed)?
        .into_iter()
        .filter(|job| claims.can_admin() || job.created_by == claims.sub)
        .collect();

    Ok(Json(serde_json::json!({ "jobs": jobs })))
}

async fn get_scheduled_job(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Refusal> {
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| Refusal::from(status.into_response()))?;
    let job = state
        .db
        .get_scheduled_job(id)
        .await
        .map_err(scheduled_job_read_failed)?
        .filter(|job| claims.can_admin() || job.created_by == claims.sub)
        .ok_or_else(|| refuse_as_json(StatusCode::NOT_FOUND, "no such scheduled job".into()))?;
    Ok(Json(serde_json::json!({ "job": job })))
}

/// Every action a job may ask for, which is every action the worker runs.
async fn scheduler_action_kinds() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "action_kinds": scheduler::ScheduledAction::KINDS }))
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

/// Routes for STAC catalog. The root is this server's own, the collection list
/// and item search are proxies of the configured upstream.
pub fn stac_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/stac", get(stac_root))
        .route("/api/v1/stac/collections", get(stac_collections))
        .route("/api/v1/stac/search", get(stac_search))
}

async fn stac_root() -> Json<serde_json::Value> {
    let catalog = stac::root_catalog(stac::upstream_api().is_some());
    Json(serde_json::json!(catalog))
}

/// The upstream's answer, or its error as the status that error carries.
fn stac_answer(what: &str, result: Result<serde_json::Value, stac::UpstreamError>) -> Response {
    match result {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            tracing::warn!("stac {what} refused: {e}");
            (
                e.status(),
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// Ask the configured STAC upstream for its collections. With none configured
/// this refuses, the way search does: a client cannot tell an invented
/// collection from one a catalog holds.
async fn stac_collections() -> Response {
    let listed = async {
        let api = stac::upstream_api().ok_or(stac::UpstreamError::NoUpstream)?;
        stac::collections(&api).await
    };
    stac_answer("collections", listed.await)
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
        .map_err(stac::UpstreamError::BadRequest)?;
        let api = stac::upstream_api().ok_or(stac::UpstreamError::NoUpstream)?;
        stac::search(&api, &params).await
    };
    stac_answer("search", searched.await)
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

/// Read a pixel window out of a registered COG. The read makes range requests
/// through the reader kept open on that source, so it runs on a blocking thread.
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
    let datasets = state.cog_engine.list_datasets();
    Json(serde_json::json!({ "datasets": datasets }))
}

async fn cog_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let datasets = state.cog_engine.list_datasets();
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
pub fn geoprocessing_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/v1/geoprocessing/operations", get(list_geo_operations))
        .route("/api/v1/geoprocessing/demo", get(geoprocessing_demo))
        .route("/api/v1/geoprocessing/run", post(run_geoprocessing))
}

async fn list_geo_operations() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "operations": geoprocessing::OPERATIONS }))
}

#[derive(Deserialize)]
struct GeoprocessingRunRequest {
    operation: String,
    geometry: geoprocessing::Geometry,
    #[serde(default)]
    other: Option<geoprocessing::Geometry>,
    #[serde(default)]
    distance_m: Option<f64>,
    #[serde(default)]
    tolerance: Option<f64>,
}

async fn run_geoprocessing(
    Json(request): Json<GeoprocessingRunRequest>,
) -> Result<Json<geoprocessing::GeoprocessingResult>, (StatusCode, String)> {
    let operation = geoprocessing::GeoOperation::parse(
        &request.operation,
        request.distance_m,
        request.tolerance,
    )
    .map_err(|error| bad_request(error.to_string()))?;
    geoprocessing::run(&operation, &request.geometry, request.other.as_ref())
        .map(Json)
        .map_err(|error| bad_request(error.to_string()))
}

async fn geoprocessing_demo() -> Json<serde_json::Value> {
    let square = geoprocessing::Geometry::Polygon(vec![vec![
        [0.0, 0.0],
        [0.01, 0.0],
        [0.01, 0.01],
        [0.0, 0.01],
        [0.0, 0.0],
    ]]);
    let buffered = geoprocessing::run(
        &geoprocessing::GeoOperation::Buffer { distance_m: 100.0 },
        &square,
        None,
    )
    .expect("the demo's own square buffers");
    Json(serde_json::json!({
        "demo": "one invented square near 0,0 buffered by 100 m, not your data. \
                 POST /api/v1/geoprocessing/run to run an operation on your own geometry.",
        "operation": buffered.operation,
        "geometry": buffered.geometry,
        "area_m2": buffered.area_m2,
        "length_m": buffered.length_m
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

/// The DEM stores a static map draws its base layer from. Lets the render
/// handlers take the sources as their state, so a test can drive the real route
/// table with nothing but a DEM behind it.
impl FromRef<Arc<AppState>> for elevation::ElevationSources {
    fn from_ref(state: &Arc<AppState>) -> Self {
        state.elevation_sources()
    }
}

/// Names the base layer a render was drawn on, so a caller can tell a hillshade
/// from the flat fallback without asking a second question.
const BASE_LAYER_HEADER: &str = "x-static-map-base-layer";

/// Routes for static map rendering.
pub fn static_map_routes<S>() -> Router<S>
where
    elevation::ElevationSources: FromRef<S>,
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(
            "/api/v1/static-map/render",
            get(static_map_render_query).post(static_map_render_body),
        )
        .route("/api/v1/static-map/formats", get(static_map_formats))
}

#[derive(Deserialize)]
struct StaticMapQuery {
    lon: Option<f64>,
    lat: Option<f64>,
    zoom: Option<f64>,
    /// `west,south,east,north` in degrees.
    bbox: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    format: Option<String>,
    dpi: Option<u32>,
}

/// Default image size for a GET that names no dimensions.
const STATIC_MAP_DEFAULT_SIZE: (u32, u32) = (800, 600);

impl StaticMapQuery {
    fn into_request(self) -> Result<static_map::StaticMapRequest, Refusal> {
        let format = match self.format.as_deref() {
            None => static_map::ImageFormat::Png,
            Some(name) => static_map::ImageFormat::from_name(name).ok_or_else(|| {
                let known: Vec<&str> = static_map::FORMATS.iter().map(|f| f.name()).collect();
                refuse_request(format!(
                    "format {name:?} is not rendered, expected one of {}",
                    known.join(", ")
                ))
            })?,
        };
        let bbox = match self.bbox.as_deref() {
            None => None,
            Some(text) => Some(parse_bbox(text)?),
        };
        let center = match (self.lon, self.lat) {
            (Some(lon), Some(lat)) => Some([lon, lat]),
            (None, None) => None,
            _ => {
                return Err(refuse_request(
                    "give both lon and lat, or neither".to_string(),
                ));
            }
        };
        let (default_width, default_height) = STATIC_MAP_DEFAULT_SIZE;
        Ok(static_map::StaticMapRequest {
            center,
            zoom: self.zoom,
            bbox,
            width: self.width.unwrap_or(default_width),
            height: self.height.unwrap_or(default_height),
            format,
            markers: Vec::new(),
            overlays: Vec::new(),
            dpi: self.dpi.unwrap_or(static_map::ALLOWED_DPI[0]),
        })
    }
}

fn parse_bbox(text: &str) -> Result<[f64; 4], Refusal> {
    let mut corners = [0.0; 4];
    let mut parts = text.split(',');
    for corner in corners.iter_mut() {
        let part = parts
            .next()
            .and_then(|p| p.trim().parse::<f64>().ok())
            .ok_or_else(|| {
                refuse_request(format!(
                    "bbox {text:?} must be west,south,east,north in degrees"
                ))
            })?;
        *corner = part;
    }
    if parts.next().is_some() {
        return Err(refuse_request(format!(
            "bbox {text:?} has more than four numbers"
        )));
    }
    Ok(corners)
}

async fn static_map_render_query(
    State(sources): State<elevation::ElevationSources>,
    Query(params): Query<StaticMapQuery>,
) -> Result<Response, Refusal> {
    static_map_answer(&sources, params.into_request()?).await
}

async fn static_map_render_body(
    State(sources): State<elevation::ElevationSources>,
    Json(request): Json<static_map::StaticMapRequest>,
) -> Result<Response, Refusal> {
    static_map_answer(&sources, request).await
}

/// The image bytes, with the content type of the format and the base layer the
/// render was drawn on.
async fn static_map_answer(
    sources: &elevation::ElevationSources,
    request: static_map::StaticMapRequest,
) -> Result<Response, Refusal> {
    let plan = request.plan().map_err(refuse_request)?;
    let render = static_map::render(&plan, sources).await?;
    Ok((
        [
            (header::CONTENT_TYPE, plan.format.content_type()),
            (
                axum::http::HeaderName::from_static(BASE_LAYER_HEADER),
                render.base_layer.name(),
            ),
        ],
        render.bytes,
    )
        .into_response())
}

async fn static_map_formats() -> Json<serde_json::Value> {
    let formats: Vec<serde_json::Value> = static_map::FORMATS
        .iter()
        .map(|format| {
            serde_json::json!({ "format": format.name(), "content_type": format.content_type() })
        })
        .collect();
    let base_layers: Vec<serde_json::Value> = static_map::BASE_LAYERS
        .iter()
        .map(|layer| {
            serde_json::json!({ "base_layer": layer.name(), "drawn_from": layer.drawn_from() })
        })
        .collect();
    Json(serde_json::json!({
        "formats": formats,
        "base_layers": base_layers,
        "max_width": static_map::MAX_IMAGE_SIDE,
        "max_height": static_map::MAX_IMAGE_SIDE,
        "dpi": static_map::ALLOWED_DPI
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

    /// Send a request to the real geoprocessing route table and read the answer.
    async fn geoprocessing_request(
        method: &str,
        uri: &str,
        body: Option<String>,
    ) -> (StatusCode, String) {
        use tower::ServiceExt;

        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = geoprocessing_routes::<()>()
            .oneshot(
                request
                    .body(body.map(Body::from).unwrap_or_else(Body::empty))
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

    async fn geoprocess(body: serde_json::Value) -> (StatusCode, String) {
        geoprocessing_request("POST", "/api/v1/geoprocessing/run", Some(body.to_string())).await
    }

    fn geoprocessing_square(min_x: f64, min_y: f64, side: f64) -> serde_json::Value {
        serde_json::json!({
            "type": "Polygon",
            "coordinates": [[
                [min_x, min_y],
                [min_x + side, min_y],
                [min_x + side, min_y + side],
                [min_x, min_y + side],
                [min_x, min_y],
            ]]
        })
    }

    #[tokio::test]
    async fn geoprocessing_run_buffers_a_square_into_a_rounded_multipolygon() {
        let (status, body) = geoprocess(serde_json::json!({
            "operation": "Buffer",
            "geometry": geoprocessing_square(0.0, 0.0, 0.01),
            "distance_m": 100.0
        }))
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let result: geoprocessing::GeoprocessingResult = serde_json::from_str(&body).unwrap();
        let geoprocessing::Geometry::MultiPolygon(parts) = &result.geometry else {
            panic!(
                "expected a MultiPolygon, got {}",
                result.geometry.type_name()
            );
        };
        assert_eq!(parts.len(), 1);
        assert!(
            parts[0][0].len() > 5,
            "a buffered square has rounded corners, got {} positions",
            parts[0][0].len()
        );
        assert!(result.area_m2.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn geoprocessing_run_unions_two_disjoint_squares_into_two_parts() {
        let (status, body) = geoprocess(serde_json::json!({
            "operation": "Union",
            "geometry": geoprocessing_square(0.0, 0.0, 1.0),
            "other": geoprocessing_square(2.0, 0.0, 1.0)
        }))
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let result: geoprocessing::GeoprocessingResult = serde_json::from_str(&body).unwrap();
        let geoprocessing::Geometry::MultiPolygon(parts) = &result.geometry else {
            panic!(
                "expected a MultiPolygon, got {}",
                result.geometry.type_name()
            );
        };
        assert_eq!(parts.len(), 2);
    }

    #[tokio::test]
    async fn geoprocessing_run_answers_a_centroid_point_with_no_area() {
        let (status, body) = geoprocess(serde_json::json!({
            "operation": "Centroid",
            "geometry": geoprocessing_square(0.0, 0.0, 2.0)
        }))
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let result: geoprocessing::GeoprocessingResult = serde_json::from_str(&body).unwrap();
        assert!(
            matches!(result.geometry, geoprocessing::Geometry::Point([x, y]) if (x - 1.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9),
            "{:?}",
            result.geometry
        );
        assert!(result.area_m2.is_none());
    }

    #[tokio::test]
    async fn geoprocessing_run_simplifies_a_line() {
        let (status, body) = geoprocess(serde_json::json!({
            "operation": "Simplify",
            "geometry": {
                "type": "LineString",
                "coordinates": [[0.0, 0.0], [0.1, 0.001], [0.2, 0.0], [0.3, 0.5]]
            },
            "tolerance": 0.01
        }))
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let result: geoprocessing::GeoprocessingResult = serde_json::from_str(&body).unwrap();
        let geoprocessing::Geometry::LineString(positions) = &result.geometry else {
            panic!("expected a LineString, got {}", result.geometry.type_name());
        };
        assert!(positions.len() < 4, "{positions:?}");
        assert!(result.length_m.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn geoprocessing_run_refuses_an_unknown_operation_and_names_the_accepted_set() {
        let (status, reason) = geoprocess(serde_json::json!({
            "operation": "Voronoi",
            "geometry": geoprocessing_square(0.0, 0.0, 1.0)
        }))
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        for name in geoprocessing::OPERATIONS {
            assert!(reason.contains(name), "{reason} does not name {name}");
        }
    }

    #[tokio::test]
    async fn geoprocessing_run_refuses_a_binary_operation_without_a_second_geometry() {
        let (status, reason) = geoprocess(serde_json::json!({
            "operation": "Intersection",
            "geometry": geoprocessing_square(0.0, 0.0, 1.0)
        }))
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("other"), "{reason}");
    }

    #[tokio::test]
    async fn geoprocessing_run_refuses_a_ring_with_three_positions() {
        let (status, reason) = geoprocess(serde_json::json!({
            "operation": "ConvexHull",
            "geometry": { "type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 0.0], [0.0, 0.0]]] }
        }))
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("at least 4 positions"), "{reason}");
    }

    /// JSON has no NaN and rejects a number too large for a double, so a request
    /// cannot carry a non-finite coordinate as far as `geoprocessing::run`. The
    /// finite check there still guards every other caller.
    #[tokio::test]
    async fn geoprocessing_run_refuses_a_coordinate_json_cannot_hold() {
        let body = r#"{"operation":"Centroid","geometry":{"type":"Polygon",
            "coordinates":[[[0,0],[1e400,0],[1,1],[0,0]]]}}"#;

        let (status, reason) =
            geoprocessing_request("POST", "/api/v1/geoprocessing/run", Some(body.to_string()))
                .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}");
        assert!(reason.contains("number out of range"), "{reason}");
    }

    #[tokio::test]
    async fn geoprocessing_run_refuses_buffer_without_a_distance() {
        let (status, reason) = geoprocess(serde_json::json!({
            "operation": "Buffer",
            "geometry": geoprocessing_square(0.0, 0.0, 1.0)
        }))
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("distance_m"), "{reason}");
    }

    #[tokio::test]
    async fn geoprocessing_operations_lists_only_what_run_accepts() {
        let (status, body) =
            geoprocessing_request("GET", "/api/v1/geoprocessing/operations", None).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<String> =
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["operations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_string())
                .collect();
        assert!(!listed.is_empty());
        for name in listed {
            geoprocessing::GeoOperation::parse(&name, Some(100.0), Some(0.5))
                .unwrap_or_else(|error| panic!("/operations lists '{name}' but run said {error}"));
        }
    }

    #[tokio::test]
    async fn geoprocessing_demo_says_it_is_a_demo() {
        let (status, body) = geoprocessing_request("GET", "/api/v1/geoprocessing/demo", None).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let demo = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert!(
            demo["demo"].as_str().unwrap().contains("invented"),
            "the demo payload must say it is a demo: {body}"
        );
    }

    // ── static map ──────────────────────────────────────────────────────────

    /// The box the static-map cases render, well inside [`ridged_dem`]'s grid so
    /// every pixel has elevation to shade.
    const STATIC_MAP_BBOX: &str = "7.0,43.0,7.5,43.5";

    /// Elevation sources with no grid loaded, no staged tile and no SRTM
    /// fallback, so a render falls back to the plain base layer.
    fn no_dem() -> elevation::ElevationSources {
        elevation::ElevationSources::new(
            Arc::new(elevation::DemStore::new()),
            std::env::temp_dir().join("tiletopia_static_map_no_dem"),
            String::new(),
        )
    }

    /// Sources holding one DEM grid: ground climbing north with north-south
    /// ridges across it, so a hillshade over it is not one flat tone. It reaches
    /// well past [`STATIC_MAP_BBOX`], since a grid interpolates only inside its
    /// own cells.
    fn ridged_dem() -> elevation::ElevationSources {
        const SIDE: usize = 61;
        let bounds = [6.0, 42.0, 9.0, 45.0];
        let step = (bounds[2] - bounds[0]) / (SIDE - 1) as f64;
        let mut elevations = vec![0.0; SIDE * SIDE];
        for row in 0..SIDE {
            for column in 0..SIDE {
                let latitude = bounds[3] - row as f64 * step;
                let longitude = bounds[0] + column as f64 * step;
                elevations[row * SIDE + column] = 1000.0 * (latitude - bounds[1])
                    + 400.0 * (4.0 * std::f64::consts::PI * (longitude - bounds[0])).sin();
            }
        }
        let mut store = elevation::DemStore::new();
        store.add_grid(elevation::DemGrid {
            bounds,
            width: SIDE,
            height: SIDE,
            cell_size_x: step,
            cell_size_y: step,
            elevations,
            nodata: -9999.0,
        });
        elevation::ElevationSources::new(
            Arc::new(store),
            std::env::temp_dir().join("tiletopia_static_map_grid"),
            String::new(),
        )
    }

    struct StaticMapAnswer {
        status: StatusCode,
        content_type: String,
        base_layer: String,
        bytes: Vec<u8>,
    }

    impl StaticMapAnswer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }

        /// The rendered raster, decoded back into pixels.
        fn image(&self) -> image::RgbImage {
            image::load_from_memory(&self.bytes)
                .expect("the answer decodes as an image")
                .to_rgb8()
        }
    }

    /// Drive the real static-map route table with these sources behind it.
    async fn static_map(
        sources: elevation::ElevationSources,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> StaticMapAnswer {
        use tower::ServiceExt;

        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = static_map_routes::<elevation::ElevationSources>()
            .with_state(sources)
            .oneshot(
                request
                    .body(
                        body.map(|b| Body::from(b.to_string()))
                            .unwrap_or_else(Body::empty),
                    )
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let header_text = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        let content_type = header_text("content-type");
        let base_layer = header_text(BASE_LAYER_HEADER);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        StaticMapAnswer {
            status,
            content_type,
            base_layer,
            bytes,
        }
    }

    /// A GET render of [`STATIC_MAP_BBOX`] in this format, over no DEM.
    async fn static_map_get(format: &str) -> StaticMapAnswer {
        static_map(
            no_dem(),
            "GET",
            &format!("/api/v1/static-map/render?bbox={STATIC_MAP_BBOX}&width=64&height=64&format={format}"),
            None,
        )
        .await
    }

    fn static_map_body(format: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({
            "bbox": [7.0, 43.0, 8.0, 44.0],
            "width": 101,
            "height": 101,
            "format": format
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    }

    #[tokio::test]
    async fn static_map_png_answers_png_bytes() {
        let answer = static_map_get("png").await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.content_type, "image/png");
        assert_eq!(
            &answer.bytes[0..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
        assert_eq!(answer.image().dimensions(), (64, 64));
    }

    #[tokio::test]
    async fn static_map_jpeg_answers_jpeg_bytes() {
        let answer = static_map_get("jpeg").await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.content_type, "image/jpeg");
        assert_eq!(&answer.bytes[0..3], &[0xFF, 0xD8, 0xFF]);
        assert_eq!(answer.image().dimensions(), (64, 64));
    }

    #[tokio::test]
    async fn static_map_webp_answers_a_riff_webp_container() {
        let answer = static_map_get("webp").await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.content_type, "image/webp");
        assert_eq!(&answer.bytes[0..4], b"RIFF");
        assert_eq!(&answer.bytes[8..12], b"WEBP");
        assert_eq!(answer.image().dimensions(), (64, 64));
    }

    #[tokio::test]
    async fn static_map_pdf_embeds_the_render_as_a_jpeg_xobject() {
        let answer = static_map_get("pdf").await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.content_type, "application/pdf");
        let text = String::from_utf8_lossy(&answer.bytes);
        assert!(text.starts_with("%PDF-"), "{}", &text[..16]);
        assert!(text.contains("/DCTDecode"));
        assert!(text.contains("/MediaBox [0 0 64.00 64.00]"));
        assert!(text.trim_end().ends_with("%%EOF"));
    }

    #[tokio::test]
    async fn static_map_svg_is_vector_markup_with_a_circle_per_marker() {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let answer = static_map(
            no_dem(),
            "POST",
            "/api/v1/static-map/render",
            Some(static_map_body(
                "svg",
                serde_json::json!({
                    "markers": [
                        { "longitude": 7.25, "latitude": 43.25, "color": "#ff0000", "size": "small" },
                        { "longitude": 7.75, "latitude": 43.75, "color": "#00ff00", "size": "large" }
                    ],
                    "overlays": [{
                        "overlay_type": "polygon",
                        "coordinates": [[7.2, 43.2], [7.8, 43.2], [7.8, 43.8]],
                        "stroke_color": "#0000ff",
                        "stroke_width": 2.0,
                        "fill_color": "#cccccc",
                        "fill_opacity": 0.5
                    }]
                }),
            )),
        )
        .await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.content_type, "image/svg+xml");

        let markup = answer.text();
        let mut reader = Reader::from_str(&markup);
        let mut elements: Vec<String> = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Eof => break,
                Event::Start(element) | Event::Empty(element) => {
                    elements.push(String::from_utf8_lossy(element.name().as_ref()).into_owned())
                }
                _ => {}
            }
        }
        assert_eq!(elements.iter().filter(|name| *name == "circle").count(), 2);
        assert_eq!(elements.iter().filter(|name| *name == "polygon").count(), 1);
        assert!(elements.contains(&"svg".to_string()));
        // the base layer travels inside the document, nothing is fetched
        assert!(markup.contains("href=\"data:image/png;base64,"));
        assert!(!markup.contains("href=\"http"), "{markup:.400}");
    }

    #[tokio::test]
    async fn static_map_without_dem_coverage_draws_a_plain_background() {
        let answer = static_map_get("png").await;

        assert_eq!(answer.base_layer, "plain");
        let image = answer.image();
        let first = image.get_pixel(0, 0);
        assert!(
            image.pixels().all(|pixel| pixel == first),
            "a plain base layer is one tone"
        );
    }

    #[tokio::test]
    async fn static_map_over_a_staged_dem_shades_the_terrain() {
        let answer = static_map(
            ridged_dem(),
            "GET",
            &format!("/api/v1/static-map/render?bbox={STATIC_MAP_BBOX}&width=64&height=64"),
            None,
        )
        .await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.base_layer, "hillshade");
        let image = answer.image();
        let tones: std::collections::BTreeSet<u8> =
            image.pixels().map(|pixel| pixel.0[0]).collect();
        assert!(
            tones.len() > 8,
            "a hillshade of ridged ground has many tones, got {tones:?}"
        );
    }

    #[tokio::test]
    async fn static_map_draws_a_marker_at_the_requested_point() {
        let answer = static_map(
            no_dem(),
            "POST",
            "/api/v1/static-map/render",
            Some(static_map_body(
                "png",
                serde_json::json!({
                    "markers": [
                        { "longitude": 7.5, "latitude": 43.5, "color": "#ff0000", "size": "small" }
                    ]
                }),
            )),
        )
        .await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        let image = answer.image();
        // the box centre on a 101 pixel side
        assert_eq!(image.get_pixel(50, 50).0, [255, 0, 0]);
        assert_ne!(image.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[tokio::test]
    async fn static_map_fills_a_polygon_overlay() {
        let answer = static_map(
            no_dem(),
            "POST",
            "/api/v1/static-map/render",
            Some(static_map_body(
                "png",
                serde_json::json!({
                    "overlays": [{
                        "overlay_type": "polygon",
                        "coordinates": [[7.2, 43.2], [7.8, 43.2], [7.8, 43.8], [7.2, 43.8]],
                        "stroke_color": "#000000",
                        "stroke_width": 1.0,
                        "fill_color": "#00ff00",
                        "fill_opacity": 1.0
                    }]
                }),
            )),
        )
        .await;

        assert_eq!(answer.status, StatusCode::OK, "{}", answer.text());
        let image = answer.image();
        assert_eq!(image.get_pixel(50, 50).0, [0, 255, 0]);
        assert_ne!(image.get_pixel(2, 2).0, [0, 255, 0]);
    }

    #[tokio::test]
    async fn static_map_refuses_dimensions_past_the_cap() {
        for size in ["width=0&height=64", "width=8000&height=64"] {
            let answer = static_map(
                no_dem(),
                "GET",
                &format!("/api/v1/static-map/render?bbox={STATIC_MAP_BBOX}&{size}"),
                None,
            )
            .await;

            assert_eq!(
                answer.status,
                StatusCode::BAD_REQUEST,
                "{size} was accepted"
            );
            assert!(
                answer.text().contains("width and height"),
                "{}",
                answer.text()
            );
        }
    }

    #[tokio::test]
    async fn static_map_refuses_a_format_nothing_encodes() {
        let answer = static_map(
            no_dem(),
            "GET",
            &format!("/api/v1/static-map/render?bbox={STATIC_MAP_BBOX}&format=tiff"),
            None,
        )
        .await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert!(answer.text().contains("tiff"), "{}", answer.text());
    }

    #[tokio::test]
    async fn static_map_refuses_a_box_that_covers_no_ground() {
        for bbox in ["8.0,43.0,7.0,44.0", "7.0,43.0,181.0,44.0", "7.0,43,8.0"] {
            let answer = static_map(
                no_dem(),
                "GET",
                &format!("/api/v1/static-map/render?bbox={bbox}&width=64&height=64"),
                None,
            )
            .await;

            assert_eq!(
                answer.status,
                StatusCode::BAD_REQUEST,
                "bbox {bbox} was accepted"
            );
            assert!(answer.text().contains("bbox"), "{}", answer.text());
        }
    }

    #[tokio::test]
    async fn static_map_refuses_a_render_of_nowhere_in_particular() {
        let answer = static_map(no_dem(), "GET", "/api/v1/static-map/render", None).await;

        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert!(answer.text().contains("bbox"), "{}", answer.text());
    }

    #[tokio::test]
    async fn static_map_formats_lists_only_what_it_encodes() {
        let answer = static_map(no_dem(), "GET", "/api/v1/static-map/formats", None).await;

        assert_eq!(answer.status, StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_slice(&answer.bytes).unwrap();
        let names: Vec<&str> = listed["formats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["format"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["png", "jpeg", "webp", "svg", "pdf"]);

        let base_layers: Vec<&str> = listed["base_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["base_layer"].as_str().unwrap())
            .collect();
        assert_eq!(base_layers, ["hillshade", "plain"]);
        assert!(
            listed.get("styles").is_none(),
            "there are no basemap styles to list: {listed}"
        );

        // every listed format is one a render actually answers
        for name in names {
            let answer = static_map_get(name).await;
            assert_eq!(answer.status, StatusCode::OK, "{name}: {}", answer.text());
            assert_eq!(
                answer.content_type,
                static_map::ImageFormat::from_name(name)
                    .unwrap()
                    .content_type()
            );
        }
    }

    /// GET a path off the real STAC route table.
    async fn stac_get(uri: &str) -> (StatusCode, serde_json::Value) {
        use tower::ServiceExt;

        let response = stac_routes::<()>()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    // nothing in this crate sets TILETOPIA_STAC_API, and it is read per request,
    // so these routes see no upstream

    #[tokio::test]
    async fn stac_collections_refuses_with_no_upstream_configured_the_way_search_does() {
        let (status, body) = stac_get("/api/v1/stac/collections").await;

        // a list of invented collections is the bug: a client cannot tell one
        // from a collection a catalog holds
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains(stac::UPSTREAM_API_ENV)
        );
        assert!(body.get("collections").is_none(), "{body}");

        let (search_status, search_body) = stac_get("/api/v1/stac/search").await;
        assert_eq!(status, search_status);
        assert_eq!(body, search_body);
    }

    #[tokio::test]
    async fn the_stac_root_advertises_nothing_it_cannot_answer() {
        let (status, body) = stac_get("/api/v1/stac").await;

        assert_eq!(status, StatusCode::OK);
        let rels: Vec<&str> = body["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["rel"].as_str().unwrap())
            .collect();
        assert_eq!(rels, ["self", "root"]);
        let classes: Vec<&str> = body["conformsTo"]
            .as_array()
            .unwrap()
            .iter()
            .map(|class| class.as_str().unwrap())
            .collect();
        assert_eq!(classes, ["https://api.stacspec.org/v1.0.0/core"]);
    }
}
