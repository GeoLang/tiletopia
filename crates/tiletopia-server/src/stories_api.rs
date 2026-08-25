//! Stories API — persistent CRUD endpoints for narrated 3D presentations.
//!
//! Stores stories in SQLite, supports public sharing via share tokens.

use axum::{
    Extension, Router,
    extract::{Path, State},
    http::StatusCode,
    response::Json,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use crate::AppState;
use crate::audit::AuditedResource;

/// A persistent story (narrated 3D presentation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Story {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub author_id: Option<Uuid>,
    pub slides: Vec<Slide>,
    pub is_public: bool,
    pub share_token: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub camera: CameraPosition,
    pub asset_ids: Vec<Uuid>,
    pub annotations: Vec<SlideAnnotation>,
    pub style: Option<serde_json::Value>,
    pub duration_seconds: f32,
    pub transition: SlideTransition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraPosition {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlideAnnotation {
    pub text: String,
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlideTransition {
    Fly,
    Cut,
    Fade,
}

// ─── Request / Response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateStoryRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub slides: Vec<Slide>,
    #[serde(default)]
    pub is_public: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateStoryRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub slides: Option<Vec<Slide>>,
    pub is_public: Option<bool>,
}

// ─── Routes ──────────────────────────────────────────────────────────────────

pub fn story_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/stories", get(list_stories).post(create_story))
        .route(
            "/api/v1/stories/{id}",
            get(get_story).put(update_story).delete(delete_story),
        )
        .route("/api/v1/stories/share/{token}", get(get_shared_story))
}

async fn list_stories(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Story>>, StatusCode> {
    state
        .db
        .list_stories()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn create_story(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateStoryRequest>,
) -> Result<(StatusCode, Extension<AuditedResource>, Json<Story>), StatusCode> {
    let share_token = if req.is_public {
        Some(Uuid::new_v4().to_string().replace('-', ""))
    } else {
        None
    };

    let now = Utc::now();
    let story = Story {
        id: Uuid::new_v4(),
        title: req.title,
        description: req.description,
        author_id: None,
        slides: req.slides,
        is_public: req.is_public,
        share_token,
        created_at: now,
        updated_at: now,
    };

    state
        .db
        .create_story(&story)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(story.id.to_string())),
        Json(story),
    ))
}

async fn get_story(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<Story>, StatusCode> {
    state
        .db
        .get_story(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn update_story(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateStoryRequest>,
) -> Result<Json<Story>, StatusCode> {
    let mut story = state
        .db
        .get_story(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some(title) = req.title {
        story.title = title;
    }
    if let Some(description) = req.description {
        story.description = description;
    }
    if let Some(slides) = req.slides {
        story.slides = slides;
    }
    if let Some(is_public) = req.is_public {
        story.is_public = is_public;
        if is_public && story.share_token.is_none() {
            story.share_token = Some(Uuid::new_v4().to_string().replace('-', ""));
        }
    }
    story.updated_at = Utc::now();

    state
        .db
        .update_story(&story)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(story))
}

async fn delete_story(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    state
        .db
        .delete_story(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_shared_story(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> Result<Json<Story>, StatusCode> {
    state
        .db
        .get_story_by_share_token(&token)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}
