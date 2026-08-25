//! The offline viewer export with `TILETOPIA_CESIUM_DIR` set. Its own binary
//! because it sets that variable for the whole process.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;
use uuid::Uuid;

const CESIUM_BUILD_DIR_VAR: &str = "TILETOPIA_CESIUM_DIR";

/// How many times the case reads the job before it calls the encoder stuck, and
/// how long it waits between reads.
const SETTLE_READS: usize = 200;
const BETWEEN_READS: Duration = Duration::from_millis(10);

/// The files a viewer page loads out of a copied build: the library, the
/// stylesheet its widgets need, and one worker.
const CESIUM_BUILD_FILES: &[&str] = &[
    "Cesium.js",
    "Widgets/widgets.css",
    "Workers/cesiumWorkerBootstrapper.js",
];

/// Stand-ins for the real build, so the case does not need a CesiumJS download.
fn write_fake_cesium_build(build_dir: &std::path::Path) {
    for name in CESIUM_BUILD_FILES {
        let path = build_dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("// {name}")).unwrap();
    }
}

/// Stage the tiles a finished tiling job leaves, beside the upload they were
/// built from.
fn seed_tiled_asset(data_dir: &std::path::Path, asset_id: Uuid) {
    let asset_dir = data_dir.join(asset_id.to_string());
    std::fs::create_dir_all(asset_dir.join("tiles")).unwrap();
    std::fs::create_dir_all(asset_dir.join("input")).unwrap();
    std::fs::write(
        asset_dir.join("tileset.json"),
        r#"{"asset":{"version":"1.1"}}"#,
    )
    .unwrap();
    std::fs::write(asset_dir.join("tiles/0.pnts"), b"tile bytes").unwrap();
    std::fs::write(asset_dir.join("input/cloud.las"), b"the original upload").unwrap();
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

async fn get(state: &Arc<AppState>, uri: &str, bearer: &str) -> (StatusCode, Vec<u8>) {
    send(
        state,
        Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// Read the job back until it stops queueing or encoding.
async fn settled_export(state: &Arc<AppState>, id: &str, bearer: &str) -> serde_json::Value {
    for _ in 0..SETTLE_READS {
        let (status, bytes) = get(state, &format!("/api/v1/exports/{id}"), bearer).await;
        assert_eq!(status, StatusCode::OK);
        let current: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if current["status"] != "Queued" && current["status"] != "Processing" {
            return current;
        }
        tokio::time::sleep(BETWEEN_READS).await;
    }
    panic!("export never settled");
}

#[tokio::test]
async fn a_bundle_built_with_a_cesium_dir_set_carries_the_library() {
    let cesium = tempfile::TempDir::new().unwrap();
    write_fake_cesium_build(cesium.path());
    // safe: the only writer in this test binary, before any request runs
    unsafe {
        std::env::set_var(CESIUM_BUILD_DIR_VAR, cesium.path());
    }

    let state = common::test_state().await;
    let editor = common::token(&Uuid::new_v4().to_string(), "editor");
    let asset_id = Uuid::new_v4();
    seed_tiled_asset(&state.data_dir, asset_id);

    let (status, bytes) = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/api/v1/exports")
            .header("authorization", format!("Bearer {editor}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "asset_id": asset_id, "format": "offline_viewer" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = job["id"].as_str().expect("job id").to_string();

    let settled = settled_export(&state, &id, &editor).await;
    assert_eq!(settled["status"], "Ready", "job: {settled}");

    let (status, bytes) = get(&state, &format!("/api/v1/exports/download/{id}"), &editor).await;
    assert_eq!(status, StatusCode::OK);

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    for name in CESIUM_BUILD_FILES {
        assert!(names.contains(&format!("cesium/{name}")), "{names:?}");
    }

    let mut html = String::new();
    std::io::Read::read_to_string(&mut archive.by_name("index.html").unwrap(), &mut html).unwrap();
    assert!(html.contains("./cesium/Cesium.js"), "{html}");
    assert!(
        html.contains("window.CESIUM_BASE_URL = './cesium/';"),
        "{html}"
    );
    // nothing is fetched from a host, which is what makes the bundle offline
    assert!(!html.contains("https://"), "{html}");
}
