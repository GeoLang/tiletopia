//! Story writes through the real router. Its own binary because the auth middleware only enforces when
//! `TILETOPIA_JWT_SECRET` is set, which cannot be done in a binary whose other
//! cases expect it unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;
use uuid::Uuid;

struct Answer {
    status: StatusCode,
    body: serde_json::Value,
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Answer {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    }
}

fn json_request(method: &str, uri: &str, bearer: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn empty_request(method: &str, uri: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap()
}

async fn create_story(state: &Arc<AppState>, bearer: &str) -> Answer {
    send(
        state,
        json_request(
            "POST",
            "/api/v1/stories",
            bearer,
            serde_json::json!({"title": "a walk", "description": "", "slides": [], "is_public": false}),
        ),
    )
    .await
}

fn rename(story_id: &str, bearer: &str) -> Request<Body> {
    json_request(
        "PUT",
        &format!("/api/v1/stories/{story_id}"),
        bearer,
        serde_json::json!({"title": "a longer walk"}),
    )
}

fn delete(story_id: &str, bearer: &str) -> Request<Body> {
    empty_request("DELETE", &format!("/api/v1/stories/{story_id}"), bearer)
}

#[tokio::test]
async fn a_viewer_cannot_write_a_story() {
    let state = common::test_state().await;
    let editor = common::token("editor-1", "editor");
    let viewer = common::token("viewer-1", "viewer");

    let refused = create_story(&state, &viewer).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let created = create_story(&state, &editor).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let story_id = created.body["id"].as_str().unwrap().to_owned();

    assert_eq!(
        send(&state, rename(&story_id, &viewer)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&state, delete(&story_id, &viewer)).await.status,
        StatusCode::FORBIDDEN
    );

    let listed = send(&state, empty_request("GET", "/api/v1/stories", &viewer)).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_author_is_the_caller_and_only_the_author_or_an_admin_modifies() {
    let state = common::test_state().await;
    let author = common::token("editor-1", "editor");
    let other = common::token("editor-2", "editor");
    let admin = common::token("admin-1", "admin");

    let created = create_story(&state, &author).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["author_id"], "editor-1");
    let story_id = created.body["id"].as_str().unwrap().to_owned();

    assert_eq!(
        send(&state, rename(&story_id, &other)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&state, delete(&story_id, &other)).await.status,
        StatusCode::FORBIDDEN
    );

    let renamed = send(&state, rename(&story_id, &author)).await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.body["title"], "a longer walk");
    assert_eq!(renamed.body["author_id"], "editor-1");

    let by_admin = send(&state, rename(&story_id, &admin)).await;
    assert_eq!(by_admin.status, StatusCode::OK);

    assert_eq!(
        send(&state, delete(&story_id, &admin)).await.status,
        StatusCode::NO_CONTENT
    );
    let gone = send(
        &state,
        empty_request("GET", &format!("/api/v1/stories/{story_id}"), &author),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_an_unknown_story_is_not_found() {
    let state = common::test_state().await;
    let admin = common::token("admin-1", "admin");

    let missing = send(&state, delete(&Uuid::new_v4().to_string(), &admin)).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}
