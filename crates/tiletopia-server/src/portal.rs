//! Portal content catalog — persistent CRUD for the viewer's item catalog.
//!
//! Items are owned by the authenticated user. Listing returns the owner's own
//! items plus any shared as public or org. Only the owner may delete.

use axum::{
    Extension, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::Json,
    routing::{delete, get},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;
use crate::audit::AuditedResource;

/// A catalog item as exchanged with the viewer. `owner_id` is the authz key and
/// is never serialized; the viewer sees only the display `owner` name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalItem {
    pub id: Uuid,
    #[serde(skip)]
    pub owner_id: Uuid,
    pub title: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub sharing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    pub created: DateTime<Utc>,
    pub modified: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<Extent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Geographic bounding box matching the viewer's extent shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extent {
    pub xmin: f64,
    pub ymin: f64,
    pub xmax: f64,
    pub ymax: f64,
}

#[derive(Debug, Deserialize)]
pub struct CreatePortalItemRequest {
    pub title: String,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default = "default_sharing")]
    pub sharing: String,
    #[serde(default)]
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub extent: Option<Extent>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

fn default_sharing() -> String {
    "private".into()
}

pub fn portal_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/portal/items", get(list_items).post(create_item))
        .route("/api/v1/portal/items/{id}", delete(delete_item))
}

async fn list_items(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Json<Vec<PortalItem>>, StatusCode> {
    let claims = crate::users::extract_claims(&request)?;
    let viewer_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    state
        .db
        .list_portal_items_for_viewer(viewer_id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn create_item(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<(StatusCode, Extension<AuditedResource>, Json<PortalItem>), StatusCode> {
    let claims = crate::users::extract_claims(&request)?;
    let owner_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    // claims already consumed the headers, so read the body manually.
    let body = axum::body::to_bytes(request.into_body(), 1024 * 256)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let req: CreatePortalItemRequest =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;

    // display owner comes from the user record, not the client payload.
    let owner = state
        .db
        .get_user(owner_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(|u| u.name)
        .unwrap_or_default();

    let now = Utc::now();
    let item = PortalItem {
        id: Uuid::new_v4(),
        owner_id,
        title: req.title,
        item_type: req.item_type,
        owner,
        description: req.description,
        tags: req.tags,
        sharing: req.sharing,
        thumbnail: req.thumbnail,
        created: now,
        modified: now,
        extent: req.extent,
        metadata: req.metadata,
    };

    state
        .db
        .create_portal_item(&item)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(item.id.to_string())),
        Json(item),
    ))
}

async fn delete_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    request: Request,
) -> Result<StatusCode, StatusCode> {
    let claims = crate::users::extract_claims(&request)?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let item = state
        .db
        .get_portal_item(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if item.owner_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    state
        .db
        .delete_portal_item(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}
