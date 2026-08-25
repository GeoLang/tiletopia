//! The API key loop through the real router: an admin mints a key, the key
//! authenticates a request, and every way it can fail is refused.
//!
//! Its own binary because the auth middleware only enforces when
//! `TILETOPIA_JWT_SECRET` is set, which cannot be done in a binary whose other
//! cases expect it unset.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::Row;
use tiletopia_server::api_keys::{ApiKey, Permission, RateLimitTier, hash_presented_key};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;

/// A read route a key can reach: dataset metadata, no claims of its own.
const READ_ROUTE: &str = "/api/v1/catalog";

/// A route in the Analytics class, so a read-only key is refused there.
const ANALYTICS_ROUTE: &str = "/api/v1/geostatistics/methods";

struct Answer {
    status: StatusCode,
    retry_after: Option<String>,
    body: serde_json::Value,
    text: String,
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Answer {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .map(|value| value.to_str().unwrap().to_string());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Answer {
        status,
        retry_after,
        body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        text,
    }
}

/// A GET carrying an API key, or nothing when `key` is `None`.
async fn get_with_key(state: &Arc<AppState>, uri: &str, key: Option<&str>) -> Answer {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(key) = key {
        request = request.header("x-api-key", key);
    }
    send(state, request.body(Body::empty()).unwrap()).await
}

async fn get_with_token(state: &Arc<AppState>, uri: &str, bearer: &str) -> Answer {
    send(
        state,
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// Mint a key through the admin route and hand back the plaintext plus its id.
async fn create_key(
    state: &Arc<AppState>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let answer = send(
        state,
        Request::builder()
            .method("POST")
            .uri("/api/v1/api-keys")
            .header(
                "authorization",
                format!("Bearer {}", common::token("root", "admin")),
            )
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    (answer.status, answer.body)
}

async fn create_read_key(state: &Arc<AppState>) -> String {
    let (status, body) = create_key(
        state,
        serde_json::json!({ "name": "reader", "permissions": ["read"], "tier": "free" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["key"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_created_key_authenticates_a_read_route() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;

    let answer = get_with_key(&state, READ_ROUTE, Some(&key)).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
}

#[tokio::test]
async fn the_same_read_without_a_credential_is_unauthorized() {
    let state = common::test_state().await;
    // the key exists, the request just does not carry it
    create_read_key(&state).await;

    let answer = get_with_key(&state, READ_ROUTE, None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_garbage_key_is_unauthorized() {
    let state = common::test_state().await;
    let real = create_read_key(&state).await;
    let hex = real.strip_prefix("ttk_").unwrap();

    for presented in [
        "garbage",
        "ttk_",
        hex,
        &format!("ttk_{}", &hex[..63]),
        &format!("ttk_{hex}0"),
        &format!("ttk_{}", "f".repeat(64)), // well formed, no such key
        &real.to_uppercase(),
    ] {
        let answer = get_with_key(&state, READ_ROUTE, Some(presented)).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{presented}");
        // the refusal names the class, never the key
        let error = answer.body["error"].as_str().unwrap_or_default();
        assert!(
            error == "malformed api key" || error == "unknown api key",
            "{error}"
        );
        assert!(!answer.text.contains(presented), "{}", answer.text);
    }
}

#[tokio::test]
async fn presenting_the_stored_hash_instead_of_the_key_is_unauthorized() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;
    let hash = hash_presented_key(&key).unwrap();

    let answer = get_with_key(&state, READ_ROUTE, Some(&hash)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.body["error"], "malformed api key");
}

#[tokio::test]
async fn a_revoked_key_is_unauthorized() {
    let state = common::test_state().await;
    let (status, created) = create_key(
        &state,
        serde_json::json!({ "name": "doomed", "permissions": ["read"], "tier": "free" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    let id = created["api_key"]["id"].as_str().unwrap();

    assert_eq!(
        get_with_key(&state, READ_ROUTE, Some(&key)).await.status,
        StatusCode::OK
    );

    let revoked = send(
        &state,
        Request::builder()
            .method("POST")
            .uri(format!("/api/v1/api-keys/{id}/revoke"))
            .header(
                "authorization",
                format!("Bearer {}", common::token("root", "admin")),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text);

    let answer = get_with_key(&state, READ_ROUTE, Some(&key)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.body["error"], "revoked api key");
}

#[tokio::test]
async fn a_deleted_key_is_unauthorized() {
    let state = common::test_state().await;
    let (status, created) = create_key(
        &state,
        serde_json::json!({ "name": "temporary", "permissions": ["read"], "tier": "free" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    let id = created["api_key"]["id"].as_str().unwrap();

    let deleted = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/api-keys/{id}"))
            .header(
                "authorization",
                format!("Bearer {}", common::token("root", "admin")),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);

    let answer = get_with_key(&state, READ_ROUTE, Some(&key)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.body["error"], "unknown api key");
}

#[tokio::test]
async fn an_expired_key_is_unauthorized() {
    let state = common::test_state().await;
    let key = "ttk_".to_string() + &"ab".repeat(32);
    state
        .db
        .create_api_key(&ApiKey {
            id: uuid::Uuid::new_v4(),
            name: "last year's key".into(),
            key_hash: hash_presented_key(&key).unwrap(),
            permissions: vec![Permission::Read],
            tier: RateLimitTier::Free,
            created_by: "root".into(),
            created_at: chrono::Utc::now() - chrono::Duration::days(400),
            last_used_at: None,
            expires_at: Some(chrono::Utc::now() - chrono::Duration::days(1)),
            revoked: false,
        })
        .await
        .unwrap();

    let answer = get_with_key(&state, READ_ROUTE, Some(&key)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.body["error"], "expired api key");
}

#[tokio::test]
async fn a_key_without_the_permission_for_a_route_class_is_forbidden() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;

    let answer = get_with_key(&state, ANALYTICS_ROUTE, Some(&key)).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.text);
    assert_eq!(answer.body["error"], "api key not permitted on this route");

    // the same route with the permission for it
    let (status, created) = create_key(
        &state,
        serde_json::json!({ "name": "analyst", "permissions": ["analytics"], "tier": "free" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let analyst = created["key"].as_str().unwrap();
    assert_eq!(
        get_with_key(&state, ANALYTICS_ROUTE, Some(analyst))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn no_key_reaches_an_admin_route_whatever_it_carries() {
    let state = common::test_state().await;
    let (status, created) = create_key(
        &state,
        serde_json::json!({
            "name": "every permission there is",
            "permissions": ["read", "terrain", "analytics", "export"],
            "tier": "enterprise"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let key = created["key"].as_str().unwrap();

    for uri in [
        "/api/v1/admin/stats",
        "/api/v1/admin/users",
        "/api/v1/api-keys",
        "/api/v1/orgs",
        "/api/v1/tiles/cache/stats",
    ] {
        let answer = get_with_key(&state, uri, Some(key)).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{uri}");
    }
}

const FREE_TIER_REQUESTS_PER_SECOND: u32 = 10;

#[tokio::test]
async fn a_burst_past_the_per_second_bucket_answers_429_with_retry_timing() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;

    // time stands still, so the bucket never refills during the burst
    let frozen = std::time::Instant::now();
    state
        .api_key_rate_limiter
        .set_clock(std::sync::Arc::new(move || frozen));

    // free tier is 10 requests a second and the bucket starts full
    for attempt in 0..FREE_TIER_REQUESTS_PER_SECOND {
        let answer = get_with_key(&state, READ_ROUTE, Some(&key)).await;
        assert_eq!(answer.status, StatusCode::OK, "attempt {attempt}");
    }

    let denied = get_with_key(&state, READ_ROUTE, Some(&key)).await;
    assert_eq!(denied.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(denied.retry_after.as_deref(), Some("1"));
    assert_eq!(denied.body["retry_after_ms"], 100);
    assert!(
        denied.body["error"]
            .as_str()
            .unwrap()
            .contains("rate limit exceeded"),
        "{}",
        denied.text
    );
}

#[tokio::test]
async fn the_stored_row_holds_a_hash_and_never_the_key() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;

    let row = sqlx::query("SELECT * FROM api_keys")
        .fetch_one(&state.db.pool)
        .await
        .unwrap();

    let stored_hash: String = row.get("key_hash");
    assert_eq!(stored_hash.len(), 64);
    assert!(stored_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(stored_hash, hash_presented_key(&key).unwrap());

    // no column holds the key, or any part of its random half
    let random_half = key.strip_prefix("ttk_").unwrap();
    for column in 0..row.len() {
        let value: Option<String> = row.try_get(column).unwrap_or(None);
        let value = value.unwrap_or_default();
        assert!(!value.contains(&key), "column {column} holds the key");
        assert!(
            !value.contains(random_half),
            "column {column} holds the key's random half"
        );
    }
}

#[tokio::test]
async fn the_listing_carries_no_key_and_no_hash() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;
    let hash = hash_presented_key(&key).unwrap();

    let answer = get_with_token(&state, "/api/v1/api-keys", &common::token("root", "admin")).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    assert!(!answer.text.contains(&key), "{}", answer.text);
    assert!(!answer.text.contains(&hash), "{}", answer.text);
    assert!(!answer.text.contains("key_hash"), "{}", answer.text);
    assert!(!answer.text.contains("ttk_"), "{}", answer.text);

    let listed = &answer.body["keys"][0];
    assert_eq!(listed["name"], "reader");
    assert_eq!(listed["tier"], "free");
    assert_eq!(listed["permissions"][0], "read");
    assert_eq!(listed["created_by"], "root");
    assert_eq!(listed["revoked"], false);
}

#[tokio::test]
async fn a_non_admin_token_cannot_manage_keys() {
    let state = common::test_state().await;
    let viewer = common::token("someone", "viewer");
    let editor = common::token("someone", "editor");

    for bearer in [&viewer, &editor] {
        let listing = get_with_token(&state, "/api/v1/api-keys", bearer).await;
        assert_eq!(listing.status, StatusCode::FORBIDDEN);

        let created = send(
            &state,
            Request::builder()
                .method("POST")
                .uri("/api/v1/api-keys")
                .header("authorization", format!("Bearer {bearer}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "mine", "permissions": ["read"], "tier": "enterprise"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(created.status, StatusCode::FORBIDDEN);

        let id = uuid::Uuid::new_v4();
        let deleted = send(
            &state,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/api-keys/{id}"))
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(deleted.status, StatusCode::FORBIDDEN);
    }

    // and no key was minted along the way
    let listing = get_with_token(&state, "/api/v1/api-keys", &common::token("root", "admin")).await;
    assert_eq!(listing.body["keys"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn an_unknown_permission_or_tier_is_refused_at_create() {
    let state = common::test_state().await;

    for (body, expected) in [
        (
            serde_json::json!({ "name": "k", "permissions": ["admin"], "tier": "free" }),
            "admin",
        ),
        (
            serde_json::json!({ "name": "k", "permissions": ["write"], "tier": "free" }),
            "write",
        ),
        (
            serde_json::json!({ "name": "k", "permissions": ["Read"], "tier": "free" }),
            "Read",
        ),
        (
            serde_json::json!({ "name": "k", "permissions": [], "tier": "free" }),
            "at least one permission",
        ),
        (
            serde_json::json!({ "name": "k", "permissions": ["read"], "tier": "unlimited" }),
            "unlimited",
        ),
        (
            serde_json::json!({ "name": " ", "permissions": ["read"], "tier": "free" }),
            "name must be",
        ),
    ] {
        let (status, answer) = create_key(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
        assert!(
            answer["error"].as_str().unwrap().contains(expected),
            "{answer}"
        );
    }

    let (status, answer) = create_key(
        &state,
        serde_json::json!({
            "name": "already dead", "permissions": ["read"], "tier": "free",
            "expires_at": "2020-01-01T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(
        answer["error"].as_str().unwrap().contains("past"),
        "{answer}"
    );
}

#[tokio::test]
async fn a_key_reads_its_own_usage_and_nobody_elses() {
    let state = common::test_state().await;
    let mine = create_read_key(&state).await;
    let (status, other) = create_key(
        &state,
        serde_json::json!({ "name": "someone else", "permissions": ["read"], "tier": "free" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{other}");

    // two reads, then the usage read itself
    for _ in 0..2 {
        assert_eq!(
            get_with_key(&state, READ_ROUTE, Some(&mine)).await.status,
            StatusCode::OK
        );
    }
    let answer = get_with_key(&state, "/api/v1/api-keys/usage", Some(&mine)).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    let usage = answer.body["usage"].as_array().unwrap();
    assert_eq!(usage.len(), 1, "{}", answer.text);
    assert_eq!(usage[0]["name"], "reader");
    assert_eq!(usage[0]["requests_today"], 3);
    assert_eq!(usage[0]["requests_per_second"], 10);

    // an admin sees every key, and the untouched one has no traffic
    let all = get_with_token(
        &state,
        "/api/v1/api-keys/usage",
        &common::token("root", "admin"),
    )
    .await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.text);
    let usage = all.body["usage"].as_array().unwrap();
    assert_eq!(usage.len(), 2);
    let idle = usage
        .iter()
        .find(|entry| entry["name"] == "someone else")
        .unwrap();
    assert_eq!(idle["requests_today"], 0);
    assert_eq!(idle["resets_at"], serde_json::Value::Null);

    // a viewer is not allowed to read anyone's usage
    let refused = get_with_token(
        &state,
        "/api/v1/api-keys/usage",
        &common::token("nobody", "viewer"),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
}

/// An `X-Api-Key` is the credential for the request that sends it. A bad key is
/// refused instead of quietly falling back to whatever bearer token came along,
/// and a good key never inherits the token's reach.
#[tokio::test]
async fn a_key_is_the_credential_and_a_bearer_token_does_not_rescue_it() {
    let state = common::test_state().await;
    let key = create_read_key(&state).await;
    let admin = common::token("root", "admin");

    let refused = send(
        &state,
        Request::builder()
            .method("GET")
            .uri(READ_ROUTE)
            .header("x-api-key", "garbage")
            .header("authorization", format!("Bearer {admin}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.body["error"], "malformed api key");

    let no_escalation = send(
        &state,
        Request::builder()
            .method("GET")
            .uri("/api/v1/admin/stats")
            .header("x-api-key", &key)
            .header("authorization", format!("Bearer {admin}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(no_escalation.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn public_reads_stay_public_with_no_credential_and_despite_a_bad_key() {
    let state = common::test_state().await;

    for uri in ["/api/v1/terrain/layer.json", "/api/v1/tiles/sources"] {
        assert_eq!(
            get_with_key(&state, uri, None).await.status,
            StatusCode::OK,
            "{uri} with no credential"
        );
        // the exemption is decided before any key is looked at
        assert_eq!(
            get_with_key(&state, uri, Some("garbage")).await.status,
            StatusCode::OK,
            "{uri} with a garbage key"
        );
    }

    // and health, which is public for a different reason
    assert_eq!(
        get_with_key(&state, "/api/v1/health", None).await.status,
        StatusCode::OK
    );
}
