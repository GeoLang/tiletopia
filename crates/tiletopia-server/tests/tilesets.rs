#![cfg(feature = "martin")]

//! The whole vector tileset loop: upload, build, serve, delete. Every case that
//! needs a real build skips itself when tippecanoe is not installed.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tiletopia_server::map_tiles::martin_backend::MartinTileBackend;
use tiletopia_server::tilesets::{TilesetBuilder, register_ready_tilesets, tippecanoe_version};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;

const JWT_SECRET_ENV: &str = "TILETOPIA_JWT_SECRET";
const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";

/// Two points far enough apart that tippecanoe picks maxzoom 0, so the archive
/// holds tile 0/0/0 and the test does not have to guess a zoom.
const FIXTURE_GEOJSON: &str = r#"{"type":"FeatureCollection","features":[
{"type":"Feature","properties":{"name":"west"},"geometry":{"type":"Point","coordinates":[-40,-20]}},
{"type":"Feature","properties":{"name":"east"},"geometry":{"type":"Point","coordinates":[40,20]}}]}"#;

/// How long a build of the fixture above is given before the test gives up.
const BUILD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Put the auth middleware into its enforcing state, once for the binary. Every
/// case here signs with the same secret, so there is nothing to serialize.
fn signing_secret() -> &'static str {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // safe: the only writer in this test binary, before any request runs
        unsafe {
            std::env::set_var(JWT_SECRET_ENV, TEST_SECRET);
            std::env::remove_var("TILETOPIA_AUTH_DISABLED");
        }
    });
    TEST_SECRET
}

fn token(subject: &str, role: &str) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let claims = serde_json::json!({
        "sub": subject,
        "exp": chrono::Utc::now().timestamp() + 300,
        "role": role,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(signing_secret().as_bytes()),
    )
    .unwrap()
}

/// `false` when tippecanoe is not installed, with the reason printed. Nothing
/// else in the suite needs the binary.
fn tippecanoe_installed() -> bool {
    match tippecanoe_version() {
        Some(version) => {
            println!("building with {version}");
            true
        }
        None => {
            println!("skipping: tippecanoe is not on PATH");
            false
        }
    }
}

async fn state_with_builder() -> (Arc<AppState>, tokio::task::JoinHandle<()>) {
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;
    let builder = Arc::new(TilesetBuilder::new(
        Arc::clone(&state.db),
        state.data_dir.clone(),
        state.tileset_dir.clone(),
        state.martin_backend.clone(),
    ));
    let worker = builder.start();
    (state, worker)
}

async fn upload(
    state: &Arc<AppState>,
    token: &str,
    filename: &str,
    contents: &str,
) -> (StatusCode, serde_json::Value) {
    let boundary = "tiletopiatilesetboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"{filename}\"\r\n\r\n{contents}\r\n--{boundary}--\r\n"
    );
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/tilesets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

async fn request(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    token: &str,
) -> (StatusCode, Vec<u8>) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn json(state: &Arc<AppState>, uri: &str, token: &str) -> serde_json::Value {
    let (status, body) = request(state, "GET", uri, token).await;
    assert_eq!(status, StatusCode::OK, "{uri}");
    serde_json::from_slice(&body).unwrap()
}

/// Poll the record until the build leaves `building`, and hand back the record.
async fn await_build(state: &Arc<AppState>, id: &str, token: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + BUILD_DEADLINE;
    loop {
        let record = json(state, &format!("/api/v1/tilesets/{id}"), token).await;
        if record["status"] != "building" {
            return record;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the build never left 'building'"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_geojson_upload_builds_an_archive_that_serves_tiles_until_it_is_deleted() {
    if !tippecanoe_installed() {
        return;
    }
    let editor = token("tileset-editor", "editor");
    let (state, worker) = state_with_builder().await;

    let (status, accepted) = upload(&state, &editor, "city roads.geojson", FIXTURE_GEOJSON).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();
    assert_eq!(accepted["job_id"], accepted["tileset"]["id"]);
    assert_eq!(accepted["tileset"]["status"], "building");
    assert_eq!(accepted["tileset"]["layer_name"], "city_roads");
    let argv: Vec<String> = serde_json::from_value(accepted["tileset"]["argv"].clone()).unwrap();
    assert_eq!(argv[0], "tippecanoe");
    assert!(
        argv.contains(&"--drop-densest-as-needed".to_string()),
        "{argv:?}"
    );

    let record = await_build(&state, &id, &editor).await;
    assert_eq!(record["status"], "ready", "{record}");
    assert_eq!(record["source_id"], id);
    assert!(record["built_at"].is_string(), "{record}");
    assert!(record["size_bytes"].as_u64().unwrap() > 0, "{record}");
    assert!(record["error"].is_null(), "{record}");

    // the archive really is on disk under the key the row names
    let archive = state
        .tileset_dir
        .join(record["object_key"].as_str().unwrap());
    assert!(archive.is_file(), "{}", archive.display());

    let listed = json(&state, "/api/v1/tilesets", &editor).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["id"], serde_json::Value::String(id.clone()));

    let tilejson = json(&state, &format!("/martin/{id}"), &editor).await;
    assert!(tilejson["tiles"].is_array(), "{tilejson}");

    let (status, tile) = request(&state, "GET", &format!("/martin/{id}/0/0/0"), &editor).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!tile.is_empty());

    let (status, _) = request(&state, "DELETE", &format!("/api/v1/tilesets/{id}"), &editor).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!archive.exists(), "the archive outlived its row");

    for uri in [
        format!("/api/v1/tilesets/{id}"),
        format!("/martin/{id}"),
        format!("/martin/{id}/0/0/0"),
    ] {
        let (status, _) = request(&state, "GET", &uri, &editor).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    worker.abort();
}

#[tokio::test]
async fn a_file_tippecanoe_refuses_fails_the_build_and_keeps_its_stderr() {
    if !tippecanoe_installed() {
        return;
    }
    let editor = token("tileset-editor", "editor");
    let (state, worker) = state_with_builder().await;

    let (status, accepted) = upload(&state, &editor, "broken.geojson", "not json at all").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();

    let record = await_build(&state, &id, &editor).await;
    assert_eq!(record["status"], "failed", "{record}");
    let error = record["error"].as_str().unwrap();
    assert!(error.contains("tippecanoe"), "{error}");
    assert!(
        error.len() <= 8 * 1024 + 200,
        "the stderr tail is unbounded"
    );
    assert_eq!(record["size_bytes"], 0);

    // a failed build leaves nothing to serve and no half-written archive
    let (status, _) = request(&state, "GET", &format!("/martin/{id}/0/0/0"), &editor).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        !state
            .tileset_dir
            .join(record["object_key"].as_str().unwrap())
            .exists()
    );

    worker.abort();
}

#[tokio::test]
async fn a_ready_archive_re_registers_after_a_restart() {
    if !tippecanoe_installed() {
        return;
    }
    let editor = token("tileset-editor", "editor");
    let (state, worker) = state_with_builder().await;

    let (_, accepted) = upload(&state, &editor, "roads.geojson", FIXTURE_GEOJSON).await;
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();
    let record = await_build(&state, &id, &editor).await;
    assert_eq!(record["status"], "ready", "{record}");
    worker.abort();

    // a fresh backend is what a restart has: no source until the registry is
    // read back
    let restarted = MartinTileBackend::new();
    assert!(!restarted.contains(&id).await);
    register_ready_tilesets(&state.db, &restarted, &state.tileset_dir)
        .await
        .unwrap();
    assert!(restarted.contains(&id).await);

    let tile = restarted.get_tile(&id, 0, 0, 0).await.unwrap();
    assert!(!tile.is_empty());
}

#[tokio::test]
async fn an_extension_tippecanoe_cannot_read_is_refused_before_any_build() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let (status, _) = upload(&state, &editor, "roads.shp", "shapefile bytes").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let listed = json(&state, "/api/v1/tilesets", &editor).await;
    assert!(listed.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn an_upload_past_the_default_body_limit_is_taken() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // axum refuses a body over 2 MB unless the route raises the limit, and no
    // file worth tiling is smaller than that
    let mut big = String::from(r#"{"type":"FeatureCollection","features":["#);
    let mut count = 0;
    while big.len() < 3 * 1024 * 1024 {
        let longitude = count as f64 % 180.0 - 90.0;
        if count > 0 {
            big.push(',');
        }
        big.push_str(&format!(
            r#"{{"type":"Feature","properties":{{"name":"feature {count}"}},"geometry":{{"type":"Point","coordinates":[{longitude},0]}}}}"#
        ));
        count += 1;
    }
    big.push_str("]}");
    assert!(big.len() > 2 * 1024 * 1024);

    let (status, accepted) = upload(&state, &editor, "big.geojson", &big).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");

    // the bytes reached disk rather than being held for the record
    let id = accepted["tileset"]["id"].as_str().unwrap();
    let input = state
        .data_dir
        .join("tileset_builds")
        .join(id)
        .join("source.geojson");
    assert_eq!(std::fs::metadata(&input).unwrap().len() as usize, big.len());
}

#[tokio::test]
async fn a_viewer_may_read_the_list_but_not_upload_or_delete() {
    let viewer = token("tileset-viewer", "viewer");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let (status, _) = upload(&state, &viewer, "roads.geojson", FIXTURE_GEOJSON).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request(
        &state,
        "DELETE",
        &format!("/api/v1/tilesets/{}", uuid::Uuid::new_v4()),
        &viewer,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let listed = json(&state, "/api/v1/tilesets", &viewer).await;
    assert!(listed.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn another_editor_cannot_read_or_delete_someone_elses_tileset() {
    let owner = token("tileset-owner", "editor");
    let stranger = token("tileset-stranger", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // no builder is started, so the row stays in 'building' and the test is
    // about who may see it, not about tippecanoe
    let (status, accepted) = upload(&state, &owner, "roads.geojson", FIXTURE_GEOJSON).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();

    for method in ["GET", "DELETE"] {
        let (status, _) =
            request(&state, method, &format!("/api/v1/tilesets/{id}"), &stranger).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
    }

    let listed = json(&state, "/api/v1/tilesets", &stranger).await;
    assert!(listed.as_array().unwrap().is_empty());
    let owned = json(&state, "/api/v1/tilesets", &owner).await;
    assert_eq!(owned.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn the_tileset_routes_refuse_a_tokenless_request() {
    signing_secret();
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri("/api/v1/tilesets")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
