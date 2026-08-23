#![cfg(feature = "martin")]

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use pmtiles::{PmTilesWriter, TileCoord, TileType};
use std::path::Path;
use tempfile::TempDir;
use tiletopia_server::map_tiles::martin_backend::{PMTILES_DIR_ENV, register_pmtiles_dir};
use tiletopia_server::router;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

const JWT_SECRET_ENV: &str = "TILETOPIA_JWT_SECRET";
const AUTH_DISABLED_ENV: &str = "TILETOPIA_AUTH_DISABLED";
const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";

const SOURCE_ID: &str = "basemap";
const TILE_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nnot a real png, just bytes to round-trip";

/// Serializes the tests here, which set process-global auth variables. This is
/// a test binary of its own, so nothing outside these tests reads them.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Take the lock and clear both auth variables, so the middleware waves the
/// request through whatever the surrounding shell had set.
async fn without_auth() -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().await;
    // safe while the lock is held: these tests are the only env readers here
    unsafe {
        std::env::remove_var(JWT_SECRET_ENV);
        std::env::remove_var(AUTH_DISABLED_ENV);
    }
    guard
}

/// Take the lock and put the middleware into its enforcing state.
async fn with_auth() -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var(JWT_SECRET_ENV, TEST_SECRET);
        std::env::remove_var(AUTH_DISABLED_ENV);
    }
    guard
}

fn viewer_token() -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let claims = serde_json::json!({
        "sub": "martin-test-user",
        "exp": chrono::Utc::now().timestamp() + 300,
        "role": "viewer",
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
    )
    .unwrap()
}

/// A one-tile PMTiles archive at `<dir>/<source_id>.pmtiles`.
fn write_archive(dir: &Path, source_id: &str) {
    let file = std::fs::File::create(dir.join(format!("{source_id}.pmtiles"))).unwrap();
    let mut writer = PmTilesWriter::new(TileType::Png).create(file).unwrap();
    writer
        .add_tile(TileCoord::new(0, 0, 0).unwrap(), TILE_BYTES)
        .unwrap();
    writer.finalize().unwrap();
}

/// An `AppState` whose martin backend was filled by the same directory scan the
/// serve command runs, so the env var's contract is under test too.
async fn state_serving_fixture(fixture: &TempDir) -> std::sync::Arc<tiletopia_server::AppState> {
    write_archive(fixture.path(), SOURCE_ID);
    let nested = fixture.path().join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    write_archive(&nested, "buried");

    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;
    unsafe {
        std::env::set_var(PMTILES_DIR_ENV, fixture.path());
    }
    register_pmtiles_dir(&state.martin_backend).await.unwrap();
    unsafe {
        std::env::remove_var(PMTILES_DIR_ENV);
    }
    state
}

#[tokio::test]
async fn pmtiles_tile_round_trips_through_the_route() {
    let _guard = without_auth().await;
    let fixture = TempDir::new().unwrap();
    let state = state_serving_fixture(&fixture).await;

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/martin/{SOURCE_ID}/0/0/0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), TILE_BYTES);
}

#[tokio::test]
async fn unknown_source_is_not_found() {
    let _guard = without_auth().await;
    let fixture = TempDir::new().unwrap();
    let state = state_serving_fixture(&fixture).await;

    // "buried" sits one directory down, and the scan does not recurse. the tile
    // route answers for an unknown source too, rather than reading it as a fault
    for uri in [
        "/martin/no-such-source",
        "/martin/buried",
        "/martin/no-such-source/0/0/0",
        "/martin/buried/0/0/0",
    ] {
        let response = router(state.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn catalog_lists_the_registered_source() {
    let _guard = without_auth().await;
    let fixture = TempDir::new().unwrap();
    let state = state_serving_fixture(&fixture).await;

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/martin/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let catalog: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let ids: Vec<&str> = catalog
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [SOURCE_ID]);
    assert_eq!(catalog[0]["kind"], "PMTiles");
}

#[tokio::test]
async fn martin_routes_refuse_a_tokenless_request() {
    let _guard = with_auth().await;
    let fixture = TempDir::new().unwrap();
    let state = state_serving_fixture(&fixture).await;
    let uri = format!("/martin/{SOURCE_ID}/0/0/0");

    let refused = router(state.clone())
        .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    // the same request with a token serves the tile, so the 401 above is the
    // auth layer and not a broken mount
    let allowed = router(state)
        .oneshot(
            Request::builder()
                .uri(&uri)
                .header("Authorization", format!("Bearer {}", viewer_token()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);

    unsafe {
        std::env::remove_var(JWT_SECRET_ENV);
    }
}
