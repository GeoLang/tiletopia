//! Webhooks through the real router and the real delivery queue: an editor
//! subscribes, the server signs and delivers what actually happened, and every
//! way a delivery or a route can be refused is refused.
//!
//! Its own binary because the auth middleware only enforces when
//! `TILETOPIA_JWT_SECRET` is set, which cannot be done in a binary whose other
//! cases expect it unset.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use tiletopia_server::db::ModelPlacement;
use tiletopia_server::webhooks::{
    DELIVERY_HEADER, EVENT_HEADER, MAX_ATTEMPTS, SIGNATURE_HEADER, WebhookEvent,
};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

/// A cube the native mesh tiler can tile, so a job reaches Done without the
/// external tiler.
const CUBE_OBJ: &str = "\
v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 2 3 4\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";

/// How long a case waits for a background worker to get somewhere.
const WAIT_LIMIT: Duration = Duration::from_secs(60);

struct Answer {
    status: StatusCode,
    body: serde_json::Value,
    text: String,
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
        text: String::from_utf8_lossy(&bytes).into_owned(),
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

/// One received delivery, as the receiver saw it.
#[derive(Debug, Clone)]
struct Received {
    event: String,
    signature: String,
    delivery_id: String,
    body: Vec<u8>,
}

type ReceivedDeliveries = Arc<Mutex<Vec<Received>>>;

/// A loopback receiver answering `status` to every POST, recording what it got.
async fn receiver(status: StatusCode) -> (String, ReceivedDeliveries) {
    let received: ReceivedDeliveries = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&received);
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: HeaderMap, body: axum::body::Bytes| {
            let recorder = Arc::clone(&recorder);
            async move {
                let header = |name: &str| {
                    headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                };
                recorder.lock().unwrap().push(Received {
                    event: header(EVENT_HEADER),
                    signature: header(SIGNATURE_HEADER),
                    delivery_id: header(DELIVERY_HEADER),
                    body: body.to_vec(),
                });
                status
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), received)
}

/// The `sha256=<hex>` a receiver computes for itself, with no help from the code
/// that produced the header.
fn expected_signature(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256={hex}")
}

/// Subscribe through the real route, answering the new id and its one-time
/// secret.
async fn subscribe(
    state: &Arc<AppState>,
    bearer: &str,
    url: &str,
    events: &[&str],
) -> (Uuid, String) {
    let answer = send(
        state,
        json_request(
            "POST",
            "/api/v1/webhooks",
            bearer,
            serde_json::json!({ "url": url, "events": events }),
        ),
    )
    .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.text);
    let id = Uuid::parse_str(answer.body["subscription"]["id"].as_str().unwrap()).unwrap();
    let secret = answer.body["secret"].as_str().unwrap().to_string();
    (id, secret)
}

/// An asset row owned by `owner`, with `contents` staged where a tiling job
/// reads its input.
async fn staged_asset(state: &Arc<AppState>, owner: &str, contents: &str) -> (Uuid, String) {
    let asset = Asset {
        id: Uuid::new_v4(),
        name: "cube".into(),
        asset_type: AssetType::Model,
        status: AssetStatus::Ready,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: contents.len() as u64,
        description: String::new(),
        tags: Vec::new(),
        owner_id: Some(owner.to_string()),
    };
    state.db.create_asset(&asset).await.unwrap();

    let input_dir = state.data_dir.join(asset.id.to_string()).join("input");
    std::fs::create_dir_all(&input_dir).unwrap();
    let input_path = input_dir.join("cube.obj");
    std::fs::write(&input_path, contents).unwrap();
    (asset.id, input_path.to_string_lossy().into_owned())
}

/// Run the queue until `wanted` deliveries have arrived, or give up.
async fn wait_for_deliveries(received: &ReceivedDeliveries, wanted: usize) -> Vec<Received> {
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    while std::time::Instant::now() < deadline {
        let seen = received.lock().unwrap().clone();
        if seen.len() >= wanted {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "only {} of {wanted} deliveries arrived",
        received.lock().unwrap().len()
    );
}

/// Drive both workers until the asset's only job settles, so the job's event is
/// queued for delivery.
async fn settle_only_job(state: &Arc<AppState>, asset_id: Uuid) -> tiletopia_server::db::JobRecord {
    use tiletopia_server::db::JobStatus;

    let worker = Arc::clone(&state.job_queue).start().await;
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    let mut settled = None;
    while std::time::Instant::now() < deadline {
        let jobs = state.db.list_jobs_for_asset(asset_id).await.unwrap();
        let current = jobs.into_iter().next().expect("a job for the asset");
        if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
            settled = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    worker.abort();
    settled.expect("job never settled")
}

/// The round trip: an editor subscribes, an asset is deleted, and the receiver
/// gets a signed payload describing that deletion.
#[tokio::test]
async fn a_subscription_receives_a_signed_payload_for_the_event_it_asked_for() {
    let state = common::test_state().await;
    let editor = common::token("editor-one", "editor");
    let (url, received) = receiver(StatusCode::OK).await;
    let (subscription_id, secret) =
        subscribe(&state, &editor, &url, &[WebhookEvent::AssetDeleted.name()]).await;

    // the worker the serve path starts, not a hand-driven delivery
    let worker = Arc::clone(&state.webhooks).start();

    let (asset_id, _) = staged_asset(&state, "editor-one", CUBE_OBJ).await;
    let deleted = send(
        &state,
        empty_request("DELETE", &format!("/api/v1/assets/{asset_id}"), &editor),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);

    let deliveries = wait_for_deliveries(&received, 1).await;
    worker.abort();
    assert_eq!(deliveries.len(), 1);
    let delivery = &deliveries[0];

    assert_eq!(delivery.event, "asset.deleted");
    assert_eq!(
        delivery.signature,
        expected_signature(&secret, &delivery.body),
        "the signature does not verify against the secret and the body"
    );
    assert_ne!(
        delivery.signature,
        expected_signature("whsec_not-the-secret", &delivery.body)
    );
    assert!(Uuid::parse_str(&delivery.delivery_id).is_ok());

    let payload: serde_json::Value = serde_json::from_slice(&delivery.body).unwrap();
    assert_eq!(payload["event"], "asset.deleted");
    assert_eq!(payload["data"]["asset_id"], asset_id.to_string());
    assert_eq!(payload["data"]["name"], "cube");
    assert_eq!(payload["data"]["deleted_by"], "editor-one");
    assert!(payload["occurred_at"].as_str().is_some());

    // and the history route answers the real delivery
    let history = send(
        &state,
        empty_request("GET", "/api/v1/webhooks/deliveries", &editor),
    )
    .await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.text);
    let listed = &history.body["deliveries"][0];
    assert_eq!(listed["id"], delivery.delivery_id);
    assert_eq!(listed["subscription_id"], subscription_id.to_string());
    assert_eq!(listed["status"], "delivered");
    assert_eq!(listed["attempts"], 1);
    assert_eq!(listed["response_status"], 200);
    assert!(!history.text.contains(&secret), "{}", history.text);
}

#[tokio::test]
async fn a_failing_receiver_is_retried_with_backoff_and_stops_at_the_bound() {
    let state = common::test_state().await;
    let editor = common::token("editor-two", "editor");
    let (url, received) = receiver(StatusCode::INTERNAL_SERVER_ERROR).await;
    subscribe(&state, &editor, &url, &[WebhookEvent::AssetDeleted.name()]).await;

    let (asset_id, _) = staged_asset(&state, "editor-two", CUBE_OBJ).await;
    let deleted = send(
        &state,
        empty_request("DELETE", &format!("/api/v1/assets/{asset_id}"), &editor),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);

    // one attempt per pass, so the passes count the retries
    let started = std::time::Instant::now();
    let deadline = std::time::Instant::now() + WAIT_LIMIT;
    while std::time::Instant::now() < deadline {
        state.webhooks.deliver_due().await;
        if state.webhooks.pending_count().await == 0 {
            break;
        }
        tokio::time::sleep(common::WEBHOOK_RETRY_BASE * 4).await;
    }

    let attempts = received.lock().unwrap().len();
    assert_eq!(
        attempts, MAX_ATTEMPTS as usize,
        "attempted {attempts} times"
    );
    // the retries waited: base, then double, then double again
    let backoff: Duration = (0..MAX_ATTEMPTS - 1)
        .map(|attempt| common::WEBHOOK_RETRY_BASE * 2u32.pow(attempt))
        .sum();
    assert!(
        started.elapsed() >= backoff,
        "{} attempts inside {:?}, less than the {backoff:?} the backoff asks for",
        attempts,
        started.elapsed()
    );
    // every attempt carried the same delivery id, so a receiver can tell a retry
    // from a new event
    let ids: std::collections::BTreeSet<String> = received
        .lock()
        .unwrap()
        .iter()
        .map(|delivery| delivery.delivery_id.clone())
        .collect();
    assert_eq!(ids.len(), 1);

    // nothing is left to retry, and the giving up is what the history reports
    assert_eq!(state.webhooks.pending_count().await, 0);
    state.webhooks.deliver_due().await;
    tokio::time::sleep(common::WEBHOOK_RETRY_BASE * 8).await;
    assert_eq!(received.lock().unwrap().len(), MAX_ATTEMPTS as usize);

    let history = send(
        &state,
        empty_request("GET", "/api/v1/webhooks/deliveries", &editor),
    )
    .await;
    let listed = &history.body["deliveries"][0];
    assert_eq!(listed["status"], "failed");
    assert_eq!(listed["attempts"], MAX_ATTEMPTS);
    assert_eq!(listed["response_status"], 500);
    assert!(
        listed["error"].as_str().unwrap().contains("500"),
        "{}",
        history.text
    );
}

#[tokio::test]
async fn a_deleted_or_paused_subscription_and_one_wanting_another_event_receive_nothing() {
    let state = common::test_state().await;
    let editor = common::token("editor-three", "editor");
    let worker = Arc::clone(&state.webhooks).start();

    let (wanted_url, wanted) = receiver(StatusCode::OK).await;
    subscribe(
        &state,
        &editor,
        &wanted_url,
        &[WebhookEvent::AssetDeleted.name()],
    )
    .await;

    let (other_event_url, other_event) = receiver(StatusCode::OK).await;
    subscribe(
        &state,
        &editor,
        &other_event_url,
        &[WebhookEvent::JobCompleted.name()],
    )
    .await;

    let (unsubscribed_url, unsubscribed) = receiver(StatusCode::OK).await;
    let (unsubscribed_id, _) = subscribe(
        &state,
        &editor,
        &unsubscribed_url,
        &[WebhookEvent::AssetDeleted.name()],
    )
    .await;
    let gone = send(
        &state,
        empty_request(
            "DELETE",
            &format!("/api/v1/webhooks/{unsubscribed_id}"),
            &editor,
        ),
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text);

    let (paused_url, paused) = receiver(StatusCode::OK).await;
    let (paused_id, _) = subscribe(
        &state,
        &editor,
        &paused_url,
        &[WebhookEvent::AssetDeleted.name()],
    )
    .await;
    let update = send(
        &state,
        json_request(
            "PUT",
            &format!("/api/v1/webhooks/{paused_id}"),
            &editor,
            serde_json::json!({
                "url": paused_url,
                "events": [WebhookEvent::AssetDeleted.name()],
                "active": false
            }),
        ),
    )
    .await;
    assert_eq!(update.status, StatusCode::OK, "{}", update.text);

    let (asset_id, _) = staged_asset(&state, "editor-three", CUBE_OBJ).await;
    send(
        &state,
        empty_request("DELETE", &format!("/api/v1/assets/{asset_id}"), &editor),
    )
    .await;

    wait_for_deliveries(&wanted, 1).await;
    // long enough for anything wrongly queued to have been attempted too
    tokio::time::sleep(Duration::from_millis(500)).await;
    worker.abort();

    assert_eq!(state.webhooks.pending_count().await, 0);
    assert!(other_event.lock().unwrap().is_empty());
    assert!(unsubscribed.lock().unwrap().is_empty());
    assert!(paused.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_write_routes_refuse_a_viewer_token() {
    let state = common::test_state().await;
    let editor = common::token("editor-four", "editor");
    let viewer = common::token("viewer", "viewer");
    let (url, _) = receiver(StatusCode::OK).await;
    let (id, _) = subscribe(&state, &editor, &url, &[WebhookEvent::AssetDeleted.name()]).await;

    let body = serde_json::json!({ "url": url, "events": [WebhookEvent::AssetDeleted.name()] });
    for request in [
        json_request("POST", "/api/v1/webhooks", &viewer, body.clone()),
        json_request(
            "PUT",
            &format!("/api/v1/webhooks/{id}"),
            &viewer,
            body.clone(),
        ),
        empty_request("DELETE", &format!("/api/v1/webhooks/{id}"), &viewer),
    ] {
        let uri = request.uri().to_string();
        let method = request.method().to_string();
        let answer = send(&state, request).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // and nothing was created or removed along the way
    let listing = send(&state, empty_request("GET", "/api/v1/webhooks", &editor)).await;
    let subscriptions = listing.body["subscriptions"].as_array().unwrap();
    assert_eq!(subscriptions.len(), 1);
    assert_eq!(subscriptions[0]["id"], id.to_string());
}

#[tokio::test]
async fn the_listing_shows_a_caller_its_own_subscriptions_and_an_admin_every_one() {
    let state = common::test_state().await;
    let mine = common::token("editor-five", "editor");
    let theirs = common::token("editor-six", "editor");
    let (url, _) = receiver(StatusCode::OK).await;

    let (my_id, my_secret) =
        subscribe(&state, &mine, &url, &[WebhookEvent::AssetDeleted.name()]).await;
    let (their_id, _) = subscribe(&state, &theirs, &url, &[WebhookEvent::JobFailed.name()]).await;

    let listing = send(&state, empty_request("GET", "/api/v1/webhooks", &mine)).await;
    assert_eq!(listing.status, StatusCode::OK, "{}", listing.text);
    let subscriptions = listing.body["subscriptions"].as_array().unwrap();
    assert_eq!(subscriptions.len(), 1, "{}", listing.text);
    assert_eq!(subscriptions[0]["id"], my_id.to_string());
    assert_eq!(subscriptions[0]["events"][0], "asset.deleted");
    // no listing hands the signing secret back
    assert!(!listing.text.contains(&my_secret), "{}", listing.text);
    assert!(!listing.text.contains("secret"), "{}", listing.text);

    let all = send(
        &state,
        empty_request("GET", "/api/v1/webhooks", &common::token("root", "admin")),
    )
    .await;
    let subscriptions = all.body["subscriptions"].as_array().unwrap();
    assert_eq!(subscriptions.len(), 2, "{}", all.text);

    // somebody else's subscription is not there to change either
    for request in [
        json_request(
            "PUT",
            &format!("/api/v1/webhooks/{their_id}"),
            &mine,
            serde_json::json!({ "url": url, "events": [WebhookEvent::AssetDeleted.name()] }),
        ),
        empty_request("DELETE", &format!("/api/v1/webhooks/{their_id}"), &mine),
    ] {
        let answer = send(&state, request).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.text);
    }
}

#[tokio::test]
async fn subscribing_refuses_an_unknown_event_and_a_url_that_is_not_http() {
    let state = common::test_state().await;
    let editor = common::token("editor-seven", "editor");

    for (body, expected) in [
        (
            serde_json::json!({ "url": "https://example.com/hook", "events": ["asset.created"] }),
            "asset.created",
        ),
        (
            serde_json::json!({ "url": "https://example.com/hook", "events": [] }),
            "at least one event",
        ),
        (
            serde_json::json!({ "url": "file:///etc/passwd", "events": ["asset.deleted"] }),
            "not http or https",
        ),
        (
            serde_json::json!({ "url": "/hook", "events": ["asset.deleted"] }),
            "not a URL",
        ),
        (
            serde_json::json!({
                "url": "https://user:pass@example.com/hook", "events": ["asset.deleted"]
            }),
            "username or password",
        ),
    ] {
        let answer = send(
            &state,
            json_request("POST", "/api/v1/webhooks", &editor, body),
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{}", answer.text);
        assert!(
            answer.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains(expected),
            "{}",
            answer.text
        );
    }
}

/// The events route advertises exactly what the server emits: one subscription
/// takes every advertised event, and the three real paths that emit fill it.
#[tokio::test]
async fn the_advertised_event_types_are_exactly_the_ones_the_server_emits() {
    let state = common::test_state().await;
    let editor = common::token("editor-eight", "editor");
    let worker = Arc::clone(&state.webhooks).start();

    let advertised = send(
        &state,
        empty_request("GET", "/api/v1/webhooks/events", &editor),
    )
    .await;
    assert_eq!(advertised.status, StatusCode::OK, "{}", advertised.text);
    let advertised: Vec<String> = advertised.body["event_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_string())
        .collect();
    assert!(!advertised.is_empty());

    let (url, received) = receiver(StatusCode::OK).await;
    let events: Vec<&str> = advertised.iter().map(String::as_str).collect();
    subscribe(&state, &editor, &url, &events).await;

    // a mesh with coordinates tiles, so this job reaches Done
    let (done_asset, done_input) = staged_asset(&state, "editor-eight", CUBE_OBJ).await;
    state
        .job_queue
        .submit(
            done_asset,
            done_input,
            ModelPlacement {
                longitude: Some(10.0),
                latitude: Some(20.0),
                crs: None,
            },
        )
        .await
        .unwrap();
    let settled = settle_only_job(&state, done_asset).await;
    assert_eq!(
        settled.status,
        tiletopia_server::db::JobStatus::Done,
        "error: {:?}",
        settled.error
    );

    // the same mesh with nowhere to put it fails
    let (failed_asset, failed_input) = staged_asset(&state, "editor-eight", CUBE_OBJ).await;
    state
        .job_queue
        .submit(failed_asset, failed_input, ModelPlacement::default())
        .await
        .unwrap();
    let settled = settle_only_job(&state, failed_asset).await;
    assert_eq!(settled.status, tiletopia_server::db::JobStatus::Failed);

    let (deleted_asset, _) = staged_asset(&state, "editor-eight", CUBE_OBJ).await;
    send(
        &state,
        empty_request(
            "DELETE",
            &format!("/api/v1/assets/{deleted_asset}"),
            &editor,
        ),
    )
    .await;

    let deliveries = wait_for_deliveries(&received, advertised.len()).await;
    worker.abort();
    let emitted: std::collections::BTreeSet<String> = deliveries
        .iter()
        .map(|delivery| delivery.event.clone())
        .collect();
    let advertised: std::collections::BTreeSet<String> = advertised.into_iter().collect();
    assert_eq!(
        emitted, advertised,
        "the advertised event types and the emitted ones differ"
    );
}
