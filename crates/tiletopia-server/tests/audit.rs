//! The audit trail through the real router: an editor's mutation lands a row, a
//! refused one lands nothing, and only an instance admin can read the trail
//! back.
//!
//! Its own binary because the auth middleware only enforces when
//! `TILETOPIA_JWT_SECRET` is set, which cannot be done in a binary whose other
//! cases expect it unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use tiletopia_server::audit::{AuditAction, AuditEntry, AuditQuery};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
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

async fn seed_asset(state: &Arc<AppState>, owner: &str) -> Uuid {
    let asset = Asset {
        id: Uuid::new_v4(),
        name: "scan".into(),
        asset_type: AssetType::PointCloud,
        status: AssetStatus::Ready,
        created_at: Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: String::new(),
        tags: Vec::new(),
        owner_id: Some(owner.to_string()),
    };
    state.db.create_asset(&asset).await.unwrap();
    asset.id
}

async fn all_entries(state: &Arc<AppState>) -> Vec<AuditEntry> {
    state.audit_log.query(&AuditQuery::default()).await.unwrap()
}

/// A mutation that passed the editor gate and answered 2xx is recorded once,
/// naming who did it and what they did it to.
#[tokio::test]
async fn an_editor_mutation_lands_one_row() {
    let state = common::test_state().await;
    let editor = common::token("editor-1", "editor");
    let asset_id = seed_asset(&state, "editor-1").await;

    let answer = send(
        &state,
        json_request(
            "POST",
            &format!("/api/v1/assets/{asset_id}/annotations"),
            &editor,
            serde_json::json!({"text": "here", "longitude": 7.4, "latitude": 43.7}),
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED);

    let entries = all_entries(&state).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.user_id, "editor-1");
    assert_eq!(entry.action, AuditAction::Create);
    assert_eq!(entry.resource_type, "annotation");
    assert_eq!(entry.resource_id, asset_id.to_string());
    assert!(entry.success);
    assert!(entry.details.contains("201"), "{}", entry.details);
}

/// Starting an export copies data out of the instance, so the trail records who
/// asked for it. The export routes read the tenant off the token subject, so the
/// caller's subject is a uuid here rather than a name.
#[tokio::test]
async fn starting_an_export_lands_one_row() {
    let state = common::test_state().await;
    let editor_id = Uuid::new_v4();
    let editor = common::token(&editor_id.to_string(), "editor");
    let asset_id = seed_asset(&state, &editor_id.to_string()).await;

    let answer = send(
        &state,
        json_request(
            "POST",
            "/api/v1/exports",
            &editor,
            serde_json::json!({"asset_id": asset_id, "format": "offline_viewer"}),
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED);

    let entries = all_entries(&state).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.user_id, editor_id.to_string());
    assert_eq!(entry.action, AuditAction::Export);
    assert_eq!(entry.resource_type, "export");
    assert!(entry.success);
    assert!(entry.details.contains("202"), "{}", entry.details);
}

/// A mutation the server refused says nothing about the data, and recording it
/// would let a caller fill the table by being refused in a loop.
#[tokio::test]
async fn a_refused_mutation_lands_nothing() {
    let state = common::test_state().await;
    let viewer = common::token("viewer-1", "viewer");
    let editor = common::token("editor-1", "editor");
    let asset_id = seed_asset(&state, "editor-1").await;

    // refused by the editor gate
    let refused = send(
        &state,
        json_request(
            "POST",
            &format!("/api/v1/assets/{asset_id}/annotations"),
            &viewer,
            serde_json::json!({"text": "here", "longitude": 7.4, "latitude": 43.7}),
        ),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // past the gate, refused by the handler
    let missing = send(
        &state,
        empty_request(
            "DELETE",
            &format!("/api/v1/assets/{}", Uuid::new_v4()),
            &editor,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    // an unauthenticated write never reaches the audit layer at all
    let anonymous = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/assets/{asset_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(state.audit_log.count().await.unwrap(), 0);
}

/// The trail names who touched what and when, which is what a caller without
/// admin must not be handed.
#[tokio::test]
async fn only_an_admin_reads_the_trail() {
    let state = common::test_state().await;

    for role in ["viewer", "editor"] {
        let answer = send(
            &state,
            empty_request("GET", "/api/v1/audit", &common::token("someone", role)),
        )
        .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{role}");
    }

    let anonymous = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri("/api/v1/audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let admin = send(
        &state,
        empty_request("GET", "/api/v1/audit", &common::token("admin-1", "admin")),
    )
    .await;
    assert_eq!(admin.status, StatusCode::OK);
    assert_eq!(admin.body, serde_json::json!([]));
}

/// An admin reads back what the write routes recorded, and the filters narrow
/// it to one caller, one action or one kind of resource.
#[tokio::test]
async fn an_admin_reads_back_the_filtered_trail() {
    let state = common::test_state().await;
    let admin_token = common::token("admin-1", "admin");
    let editor = common::token("editor-1", "editor");
    let asset_id = seed_asset(&state, "editor-1").await;

    let created = send(
        &state,
        json_request(
            "POST",
            &format!("/api/v1/assets/{asset_id}/annotations"),
            &editor,
            serde_json::json!({"text": "here", "longitude": 7.4, "latitude": 43.7}),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let annotation_id = created.body["id"].as_str().unwrap().to_owned();

    let deleted = send(
        &state,
        empty_request(
            "DELETE",
            &format!("/api/v1/assets/{asset_id}/annotations/{annotation_id}"),
            &editor,
        ),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    let subscribed = send(
        &state,
        json_request(
            "POST",
            "/api/v1/webhooks",
            &admin_token,
            serde_json::json!({"url": "https://example.invalid/hook", "events": ["job.completed"]}),
        ),
    )
    .await;
    assert_eq!(subscribed.status, StatusCode::CREATED);

    let read = |query: &str| {
        let uri = format!("/api/v1/audit{query}");
        let token = admin_token.clone();
        let state = Arc::clone(&state);
        async move { send(&state, empty_request("GET", &uri, &token)).await }
    };

    let all = read("").await;
    assert_eq!(all.status, StatusCode::OK);
    assert_eq!(all.body.as_array().unwrap().len(), 3);

    let by_user = read("?user_id=editor-1").await;
    let rows = by_user.body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["user_id"] == "editor-1"));

    let by_resource = read("?resource_type=webhook").await;
    let rows = by_resource.body.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["user_id"], "admin-1");
    assert_eq!(rows[0]["action"], "Create");

    let by_action = read("?action=Delete").await;
    let rows = by_action.body.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["resource_id"], annotation_id);

    let capped = read("?limit=1").await;
    assert_eq!(capped.body.as_array().unwrap().len(), 1);

    // a filter this build cannot read is refused, so a typo does not read as
    // "nothing happened"
    let nonsense = read("?action=Deleted").await;
    assert_eq!(nonsense.status, StatusCode::BAD_REQUEST);

    // "Z" rather than "+00:00": a raw + in a query value decodes as a space
    let tomorrow =
        (Utc::now() + Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let after_the_fact = read(&format!("?from={tomorrow}")).await;
    assert_eq!(after_the_fact.body.as_array().unwrap().len(), 0);
}

/// The sweep retires what is past the retention window and leaves the rest.
#[tokio::test]
async fn the_sweep_deletes_rows_past_the_window() {
    let state = common::test_state().await;
    let now = Utc::now();

    let entry = |age_days: i64| AuditEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: now - Duration::days(age_days),
        user_id: "editor-1".into(),
        action: AuditAction::Delete,
        resource_type: "asset".into(),
        resource_id: Uuid::new_v4().to_string(),
        details: "{}".into(),
        ip_address: None,
        org_id: None,
        success: true,
    };
    for age_days in [45, 31, 29, 0] {
        state.db.create_audit_entry(&entry(age_days)).await.unwrap();
    }
    assert_eq!(state.audit_log.count().await.unwrap(), 4);

    assert_eq!(state.audit_log.sweep(now, 30).await, 2);
    assert_eq!(state.audit_log.count().await.unwrap(), 2);

    // nothing left to take
    assert_eq!(state.audit_log.sweep(now, 30).await, 0);

    let left = all_entries(&state).await;
    assert!(
        left.iter()
            .all(|entry| entry.timestamp > now - Duration::days(30)),
        "{left:?}"
    );
}

/// A create names no id in its path, so the handler hands the id it chose back
/// on the response and the row carries that.
#[tokio::test]
async fn a_create_records_the_id_the_handler_chose() {
    let state = common::test_state().await;
    let admin = common::token("admin-1", "admin");

    let uploaded = send(&state, upload_request(&admin, "scan.glb")).await;
    assert_eq!(uploaded.status, StatusCode::CREATED);
    let asset_id = uploaded.body["id"].as_str().unwrap().to_owned();

    let key = send(
        &state,
        json_request(
            "POST",
            "/api/v1/api-keys",
            &admin,
            serde_json::json!({"name": "reader", "permissions": ["read"], "tier": "free"}),
        ),
    )
    .await;
    assert_eq!(key.status, StatusCode::CREATED);
    let key_id = key.body["api_key"]["id"].as_str().unwrap().to_owned();

    let recorded = |resource_type: &str| {
        let entries = &state;
        let resource_type = resource_type.to_owned();
        async move {
            all_entries(entries)
                .await
                .into_iter()
                .find(|entry| entry.resource_type == resource_type)
                .unwrap_or_else(|| panic!("no {resource_type} row"))
        }
    };

    assert_eq!(recorded("asset").await.resource_id, asset_id);
    assert_eq!(recorded("api_key").await.resource_id, key_id);
}

/// The address is the direct peer off the socket, never a header, so a caller
/// cannot write it. A request that arrived without connect info records none
/// rather than a guess.
#[tokio::test]
async fn the_row_carries_the_peer_address() {
    let state = common::test_state().await;
    let editor = common::token("editor-1", "editor");
    let asset_id = seed_asset(&state, "editor-1").await;

    let annotation = |longitude: f64| {
        json_request(
            "POST",
            &format!("/api/v1/assets/{asset_id}/annotations"),
            &editor,
            serde_json::json!({"text": "here", "longitude": longitude, "latitude": 43.7}),
        )
    };

    let mut with_peer = annotation(7.4);
    with_peer
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, 7],
            51234,
        ))));
    // a forged header must not reach the row
    with_peer
        .headers_mut()
        .insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
    assert_eq!(send(&state, with_peer).await.status, StatusCode::CREATED);

    assert_eq!(
        send(&state, annotation(7.5)).await.status,
        StatusCode::CREATED
    );

    let entries = all_entries(&state).await;
    assert_eq!(entries.len(), 2, "{entries:?}");
    let addresses: Vec<Option<String>> = entries
        .iter()
        .map(|entry| entry.ip_address.clone())
        .collect();
    // newest first, so the one with no connect info comes back first
    assert_eq!(addresses, vec![None, Some("203.0.113.7".to_owned())]);
}

/// A multipart upload of one tiny file, which is what `POST /api/v1/assets`
/// takes.
fn upload_request(bearer: &str, filename: &str) -> Request<Body> {
    const BOUNDARY: &str = "tiletopiaauditboundary";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\n\r\nglTF-bytes\r\n--{BOUNDARY}--\r\n"
    );
    Request::builder()
        .method("POST")
        .uri("/api/v1/assets")
        .header("authorization", format!("Bearer {bearer}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}
