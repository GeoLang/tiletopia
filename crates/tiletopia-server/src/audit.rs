//! Who did what to what, recorded once in a layer instead of in every handler.
//!
//! A layer rather than a call per handler for the reason ptolemy's audit is one:
//! a mutation happens in dozens of handlers and what they share is the request
//! that reached them. It sits inside [`crate::auth::auth_middleware`], so a
//! request with no token or a bad one never reaches it.
//!
//! Only the routes in `AUDITED_ROUTES` are recorded, and only when they
//! answered 2xx. Recording refusals as well would let a caller fill the table by
//! being refused in a loop, and this log is meant to say what happened to the
//! data. Recording every mutating method instead of a list would bury those
//! writes under the compute-only POSTs, which this server has many of
//! (isochrones, geoprocessing, static maps).
//!
//! An audit write that fails is logged and dropped. The user's write has already
//! happened and its response is already built by the time this runs.
//!
//! Reading the log is instance-admin only. The rows name who touched what and
//! when, which is exactly what an attacker with a viewer token would want.

use axum::{
    Router,
    extract::{ConnectInfo, MatchedPath, Query, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{Json, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::AppState;
use crate::db::Database;

/// What `user_id` holds when the request carried no readable token: auth is off,
/// or the process has no configured secret.
pub const UNIDENTIFIED_USER: &str = "unidentified";

/// Rows one read returns when the caller names no limit.
pub const DEFAULT_AUDIT_LIMIT: usize = 100;

/// Rows one read may return however large a limit the caller names.
pub const MAX_AUDIT_LIMIT: usize = 1000;

/// How long a row is kept unless `TILETOPIA_AUDIT_RETENTION_DAYS` says
/// otherwise. `0` there keeps everything.
const DEFAULT_RETENTION_DAYS: i64 = 30;
const RETENTION_DAYS_VAR: &str = "TILETOPIA_AUDIT_RETENTION_DAYS";

/// How often the sweep runs. Far apart because nothing waits on it.
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Rows one delete statement takes, so no sweep holds a long write lock.
const SWEEP_BATCH: i64 = 1000;

/// Batches one sweep may run. Whatever is left goes on the next pass, an hour
/// later, so a first sweep against a long-unswept database cannot run for
/// minutes.
const SWEEP_MAX_BATCHES: usize = 50;

/// An audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub user_id: String,
    pub action: AuditAction,
    pub resource_type: String,
    pub resource_id: String,
    pub details: String,
    pub ip_address: Option<String>,
    pub org_id: Option<String>,
    pub success: bool,
}

/// The id a create handler picked, returned on its response so the row can name
/// what was created. A handler is the only thing that can set it: nothing on the
/// wire becomes a response extension.
///
/// Return it beside the body, for example
/// `(StatusCode::CREATED, Extension(AuditedResource(id.to_string())), Json(job))`.
#[derive(Clone)]
pub struct AuditedResource(pub String);

/// Types of auditable actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AuditAction {
    Create,
    Read,
    Update,
    Delete,
    Upload,
    Download,
    Login,
    Logout,
    PermissionChange,
    ConfigChange,
    Export,
    Share,
}

impl AuditAction {
    pub const ALL: [AuditAction; 12] = [
        AuditAction::Create,
        AuditAction::Read,
        AuditAction::Update,
        AuditAction::Delete,
        AuditAction::Upload,
        AuditAction::Download,
        AuditAction::Login,
        AuditAction::Logout,
        AuditAction::PermissionChange,
        AuditAction::ConfigChange,
        AuditAction::Export,
        AuditAction::Share,
    ];

    /// The name the `action` column holds and the read route filters on.
    pub fn name(&self) -> &'static str {
        match self {
            AuditAction::Create => "Create",
            AuditAction::Read => "Read",
            AuditAction::Update => "Update",
            AuditAction::Delete => "Delete",
            AuditAction::Upload => "Upload",
            AuditAction::Download => "Download",
            AuditAction::Login => "Login",
            AuditAction::Logout => "Logout",
            AuditAction::PermissionChange => "PermissionChange",
            AuditAction::ConfigChange => "ConfigChange",
            AuditAction::Export => "Export",
            AuditAction::Share => "Share",
        }
    }

    pub fn from_name(name: &str) -> Option<AuditAction> {
        AuditAction::ALL
            .into_iter()
            .find(|action| action.name() == name)
    }
}

/// Query filter for audit log.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub user_id: Option<String>,
    pub action: Option<AuditAction>,
    pub resource_type: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

impl AuditQuery {
    /// Rows this read may return, whatever the caller asked for.
    pub fn effective_limit(&self) -> i64 {
        self.limit
            .unwrap_or(DEFAULT_AUDIT_LIMIT)
            .min(MAX_AUDIT_LIMIT) as i64
    }
}

/// The audit trail, stored in the same SQLite database as the assets it
/// describes.
pub struct AuditLog {
    db: Arc<Database>,
}

impl AuditLog {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Append one entry. A failed write is logged and dropped: the mutation it
    /// describes has already happened and its response is already built.
    pub async fn record(&self, entry: AuditEntry) {
        if let Err(error) = self.db.create_audit_entry(&entry).await {
            tracing::warn!(
                "recording audit entry for {} on {}/{} failed: {error}",
                entry.user_id,
                entry.resource_type,
                entry.resource_id
            );
        }
    }

    /// Matching entries, newest first.
    pub async fn query(&self, filter: &AuditQuery) -> Result<Vec<AuditEntry>, sqlx::Error> {
        self.db.query_audit_entries(filter).await
    }

    pub async fn count(&self) -> Result<i64, sqlx::Error> {
        self.db.count_audit_entries().await
    }

    /// Delete everything older than `retention_days` before `now`. Returns how
    /// many rows went.
    pub async fn sweep(&self, now: DateTime<Utc>, retention_days: i64) -> u64 {
        let Some(cutoff) = now.checked_sub_signed(chrono::Duration::days(retention_days)) else {
            return 0;
        };
        let mut deleted = 0;
        for _ in 0..SWEEP_MAX_BATCHES {
            match self
                .db
                .delete_audit_entries_before(cutoff, SWEEP_BATCH)
                .await
            {
                Ok(batch) => {
                    deleted += batch;
                    if batch < SWEEP_BATCH as u64 {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!("sweeping the audit log failed: {error}");
                    break;
                }
            }
        }
        deleted
    }

    /// Start the retention sweep. `None` when `TILETOPIA_AUDIT_RETENTION_DAYS`
    /// turns it off, in which case nothing is ever deleted.
    pub fn start(self: Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        let days = retention_days_from_env()?;
        Some(tokio::spawn(async move {
            loop {
                self.sweep(Utc::now(), days).await;
                tokio::time::sleep(SWEEP_INTERVAL).await;
            }
        }))
    }
}

/// The retention window `TILETOPIA_AUDIT_RETENTION_DAYS` asks for. `None` turns
/// the sweep off: that is `0`, a negative number, or a value that is not a whole
/// number of days at all, where deleting on a guess is the wrong way to be
/// wrong. Unset takes the default.
///
/// Split from the environment read so it can be tested without mutating the
/// process environment, which edition 2024 makes unsafe.
fn parse_retention_days(raw: Option<&str>) -> Option<i64> {
    let days = match raw {
        None => DEFAULT_RETENTION_DAYS,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(days) => days,
            Err(_) => {
                tracing::warn!(
                    "{RETENTION_DAYS_VAR} is not a whole number of days, keeping everything"
                );
                0
            }
        },
    };
    (days > 0).then_some(days)
}

fn retention_days_from_env() -> Option<i64> {
    parse_retention_days(std::env::var(RETENTION_DAYS_VAR).ok().as_deref())
}

/// One audited route: the mutation, and the kind of thing it acts on.
struct AuditedRoute {
    method: &'static str,
    /// The axum route template, never a raw path: a caller-supplied segment can
    /// be anything, including the name of another route.
    template: &'static str,
    action: AuditAction,
    resource_type: &'static str,
}

/// Every mutation recorded, and nothing else. A route added to the server does
/// not audit until it is added here.
static AUDITED_ROUTES: &[AuditedRoute] = &[
    AuditedRoute {
        method: "POST",
        template: "/api/v1/assets",
        action: AuditAction::Create,
        resource_type: "asset",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/assets/{id}",
        action: AuditAction::Delete,
        resource_type: "asset",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/assets/{id}/tile",
        action: AuditAction::Update,
        resource_type: "asset",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/assets/{id}/annotations",
        action: AuditAction::Create,
        resource_type: "annotation",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/assets/{id}/annotations/{annotation_id}",
        action: AuditAction::Delete,
        resource_type: "annotation",
    },
    // the asset it copies out is in the body, not the path, so the row names the
    // export rather than the asset
    AuditedRoute {
        method: "POST",
        template: "/api/v1/exports",
        action: AuditAction::Export,
        resource_type: "export",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/tilesets",
        action: AuditAction::Create,
        resource_type: "tileset",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/tilesets/{id}",
        action: AuditAction::Delete,
        resource_type: "tileset",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/plugins/registry",
        action: AuditAction::Create,
        resource_type: "plugin",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/plugins/registry/{id}",
        action: AuditAction::Delete,
        resource_type: "plugin",
    },
    AuditedRoute {
        method: "PUT",
        template: "/api/v1/plugins/registry/{id}/config",
        action: AuditAction::ConfigChange,
        resource_type: "plugin",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/plugins/registry/{id}/enable",
        action: AuditAction::ConfigChange,
        resource_type: "plugin",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/plugins/registry/{id}/disable",
        action: AuditAction::ConfigChange,
        resource_type: "plugin",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/webhooks",
        action: AuditAction::Create,
        resource_type: "webhook",
    },
    AuditedRoute {
        method: "PUT",
        template: "/api/v1/webhooks/{id}",
        action: AuditAction::Update,
        resource_type: "webhook",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/webhooks/{id}",
        action: AuditAction::Delete,
        resource_type: "webhook",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/scheduler/jobs",
        action: AuditAction::Create,
        resource_type: "scheduled_job",
    },
    AuditedRoute {
        method: "PUT",
        template: "/api/v1/scheduler/jobs/{id}",
        action: AuditAction::Update,
        resource_type: "scheduled_job",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/scheduler/jobs/{id}",
        action: AuditAction::Delete,
        resource_type: "scheduled_job",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/orgs",
        action: AuditAction::Create,
        resource_type: "organization",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/admin/users/{id}",
        action: AuditAction::Delete,
        resource_type: "user",
    },
    AuditedRoute {
        method: "PUT",
        template: "/api/v1/admin/users/{id}/role",
        action: AuditAction::PermissionChange,
        resource_type: "user",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/api-keys",
        action: AuditAction::Create,
        resource_type: "api_key",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/api-keys/{id}/revoke",
        action: AuditAction::PermissionChange,
        resource_type: "api_key",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/api-keys/{id}",
        action: AuditAction::Delete,
        resource_type: "api_key",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/stories",
        action: AuditAction::Create,
        resource_type: "story",
    },
    AuditedRoute {
        method: "PUT",
        template: "/api/v1/stories/{id}",
        action: AuditAction::Update,
        resource_type: "story",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/stories/{id}",
        action: AuditAction::Delete,
        resource_type: "story",
    },
    AuditedRoute {
        method: "POST",
        template: "/api/v1/portal/items",
        action: AuditAction::Create,
        resource_type: "portal_item",
    },
    AuditedRoute {
        method: "DELETE",
        template: "/api/v1/portal/items/{id}",
        action: AuditAction::Delete,
        resource_type: "portal_item",
    },
    // the row names the catalog dataset that was copied in, which the path
    // carries; the asset it created is in the body
    AuditedRoute {
        method: "POST",
        template: "/api/v1/catalog/{dataset_id}/add",
        action: AuditAction::Create,
        resource_type: "catalog_entry",
    },
    AuditedRoute {
        method: "PUT",
        template: CALLER_IS_THE_RESOURCE,
        action: AuditAction::Update,
        resource_type: "user",
    },
    // the Ion facade reaches the same asset table and mints the same kind of
    // bearer credential, so leaving it out would be a way to mutate unaudited
    AuditedRoute {
        method: "POST",
        template: "/v1/assets",
        action: AuditAction::Create,
        resource_type: "asset",
    },
    AuditedRoute {
        method: "POST",
        template: "/v1/tokens",
        action: AuditAction::Create,
        resource_type: "token",
    },
];

/// The one audited route whose path names no id: it acts on whoever is calling,
/// so the row names them.
const CALLER_IS_THE_RESOURCE: &str = "/api/v1/users/me";

/// Everything the row needs, taken off the request before the handler consumes
/// it.
struct Pending {
    user_id: String,
    action: AuditAction,
    resource_type: &'static str,
    resource_id: String,
    method: &'static str,
    path: String,
    ip_address: Option<String>,
}

/// The last template parameter's value, which is the thing the route acts on:
/// the annotation for `/assets/{id}/annotations/{annotation_id}`, the asset for
/// `/assets/{id}/annotations`. Empty for a create, whose id the server only
/// picks inside the handler.
fn resource_id(template: &str, path: &str) -> String {
    template
        .split('/')
        .zip(path.split('/'))
        .filter(|(segment, _)| segment.starts_with('{'))
        .map(|(_, value)| value)
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn pending(request: &Request) -> Option<Pending> {
    // no matched path means no route matched, so the fallback 404 is about to
    // answer and nothing was written
    let template = request.extensions().get::<MatchedPath>()?.as_str();
    let route = AUDITED_ROUTES
        .iter()
        .find(|route| route.method == request.method().as_str() && route.template == template)?;

    let user_id = crate::users::claims_from_headers(request.headers())
        .map(|claims| claims.sub)
        .unwrap_or_else(|_| UNIDENTIFIED_USER.to_owned());

    let resource_id = match template {
        CALLER_IS_THE_RESOURCE => user_id.clone(),
        _ => resource_id(template, request.uri().path()),
    };

    Some(Pending {
        user_id,
        action: route.action.clone(),
        resource_type: route.resource_type,
        resource_id,
        method: route.method,
        path: request.uri().path().to_owned(),
        // the direct peer, which is the proxy when one fronts the server
        ip_address: request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| peer.ip().to_string()),
    })
}

pub async fn audit_middleware(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(pending) = pending(&request) else {
        return next.run(request).await;
    };
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }

    // the query string is deliberately left out: nothing guarantees a caller did
    // not put a credential in one, and this table is read over HTTP. No route
    // takes a credential in a path segment.
    let details = serde_json::json!({
        "method": pending.method,
        "path": pending.path,
        "status": response.status().as_u16(),
    })
    .to_string();

    // a create's path names no id, so the handler hands one back
    let resource_id = match pending.resource_id.is_empty() {
        false => pending.resource_id,
        true => response
            .extensions()
            .get::<AuditedResource>()
            .map(|created| created.0.clone())
            .unwrap_or_default(),
    };

    let org_id = caller_org(&state.db, &pending.user_id).await;

    state
        .audit_log
        .record(AuditEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            user_id: pending.user_id,
            action: pending.action,
            resource_type: pending.resource_type.to_owned(),
            resource_id,
            details,
            ip_address: pending.ip_address,
            org_id,
            // only a 2xx gets this far
            success: true,
        })
        .await;

    response
}

/// The caller's organization as the users table has it now. No token carries
/// one.
async fn caller_org(db: &Database, user_id: &str) -> Option<String> {
    let id = uuid::Uuid::parse_str(user_id).ok()?;
    match db.get_user(id).await {
        Ok(user) => user?.org_id.map(|org| org.to_string()),
        Err(error) => {
            tracing::warn!(
                "reading the organization of {user_id} for the audit row failed: {error}"
            );
            None
        }
    }
}

/// Reading the trail. Instance-admin only.
pub fn audit_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/audit", get(list_audit_entries))
        .layer(axum::middleware::from_fn(crate::users::require_admin))
}

#[derive(Debug, Deserialize, Default)]
pub struct AuditQueryParams {
    pub user_id: Option<String>,
    pub action: Option<String>,
    pub resource_type: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

async fn list_audit_entries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AuditQueryParams>,
) -> Result<Json<Vec<AuditEntry>>, StatusCode> {
    // an unknown action name is refused rather than ignored, so a typo does not
    // read as "nothing happened"
    let action = match params.action.as_deref() {
        None => None,
        Some(name) => Some(AuditAction::from_name(name).ok_or(StatusCode::BAD_REQUEST)?),
    };

    let entries = state
        .audit_log
        .query(&AuditQuery {
            user_id: params.user_id,
            action,
            resource_type: params.resource_type,
            from: params.from,
            to: params.to,
            limit: params.limit,
        })
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_name_round_trips() {
        for action in AuditAction::ALL {
            assert_eq!(AuditAction::from_name(action.name()), Some(action.clone()));
        }
        assert_eq!(AuditAction::from_name("Nonsense"), None);
    }

    #[test]
    fn resource_id_is_the_last_template_parameter() {
        assert_eq!(
            resource_id("/api/v1/assets/{id}", "/api/v1/assets/abc"),
            "abc"
        );
        assert_eq!(
            resource_id(
                "/api/v1/assets/{id}/annotations/{annotation_id}",
                "/api/v1/assets/abc/annotations/def"
            ),
            "def"
        );
        assert_eq!(
            resource_id(
                "/api/v1/assets/{id}/annotations",
                "/api/v1/assets/abc/annotations"
            ),
            "abc"
        );
        assert_eq!(resource_id("/api/v1/assets", "/api/v1/assets"), "");
    }

    #[test]
    fn retention_off_for_zero_and_nonsense() {
        assert_eq!(parse_retention_days(None), Some(DEFAULT_RETENTION_DAYS));
        assert_eq!(parse_retention_days(Some("7")), Some(7));
        assert_eq!(parse_retention_days(Some(" 7 ")), Some(7));
        assert_eq!(parse_retention_days(Some("0")), None);
        assert_eq!(parse_retention_days(Some("-1")), None);
        assert_eq!(parse_retention_days(Some("forever")), None);
    }

    #[test]
    fn a_read_cannot_ask_for_more_than_the_cap() {
        assert_eq!(
            AuditQuery::default().effective_limit(),
            DEFAULT_AUDIT_LIMIT as i64
        );
        assert_eq!(
            AuditQuery {
                limit: Some(usize::MAX),
                ..Default::default()
            }
            .effective_limit(),
            MAX_AUDIT_LIMIT as i64
        );
    }

    /// Two rows for one route would write two entries per request.
    #[test]
    fn no_route_is_listed_twice() {
        for (index, route) in AUDITED_ROUTES.iter().enumerate() {
            let duplicate = AUDITED_ROUTES[index + 1..]
                .iter()
                .any(|other| other.method == route.method && other.template == route.template);
            assert!(!duplicate, "{} {}", route.method, route.template);
        }
    }
}
