#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tiletopia_server::analysis_tiles::AnalysisEngines;
use tiletopia_server::db::JobStatus;
use tiletopia_server::{AppState, AssetStatus, router};
use tower::ServiceExt;
use uuid::Uuid;

const EDITOR: &str = "stub-tiler-editor";
const WAIT_LIMIT: Duration = Duration::from_secs(60);

const STUB_TILER: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -jar) jar="$2"; shift 2 ;;
    -i) input="$2"; shift 2 ;;
    -o) output="$2"; shift 2 ;;
    -it) input_type="$2"; shift 2 ;;
    *) shift ;;
  esac
done
: > "$jar.started"
while [ -e "$jar.hold" ]; do sleep 0.05; done
mkdir -p "$output/received" "$output/data"
cp "$input"/* "$output/received/"
printf '%s' "$input_type" > "$output/received_input_type"
printf '{"asset":{"version":"1.1"}}' > "$output/tileset.json"
printf 'tile' > "$output/data/RC0000.glb"
"#;

// each polygon position after the first is a delta, in steps of half a degree east and a quarter degree north
const QUANTIZED_TOPOLOGY: &str = r#"{
  "type": "Topology",
  "transform": {"scale": [0.5, 0.25], "translate": [100, 10]},
  "objects": {
    "field": {"type": "Polygon", "id": "west", "properties": {"crop": "wheat"}, "arcs": [[0]]}
  },
  "arcs": [[[0, 0], [2, 0], [0, 4], [-2, 0], [0, -4]]]
}"#;

const EMPTY_GEOJSON: &str = r#"{"type":"FeatureCollection","features":[]}"#;

fn put_stub_tiler_on_path() {
    // mago_command runs `java` from PATH
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        common::signing_secret();
        let dir = std::env::temp_dir().join(format!("tiletopia_stub_tiler_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = dir.join("java");
        std::fs::write(&stub, STUB_TILER).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined =
            std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path))).unwrap();
        // safe: the only PATH writer in this test binary, before any job runs
        unsafe {
            std::env::set_var("PATH", joined);
        }
    });
}

struct StubTiler {
    _jar_dir: tempfile::TempDir,
    jar: PathBuf,
    state: Arc<AppState>,
}

async fn state_with_stub_tiler() -> StubTiler {
    put_stub_tiler_on_path();
    let jar_dir = tempfile::tempdir().unwrap();
    let jar = jar_dir.path().join("mago.jar");
    let state = common::build_state(AnalysisEngines::new(), Some(jar.clone())).await;
    StubTiler {
        _jar_dir: jar_dir,
        jar,
        state,
    }
}

async fn upload(state: &Arc<AppState>, file_name: &str, contents: &str) -> Uuid {
    let boundary = "tiletopiastubtilerboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n{contents}\r\n--{boundary}--\r\n"
    );
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "editor")),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap();
    Uuid::parse_str(uploaded["id"].as_str().unwrap()).unwrap()
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[tokio::test]
async fn a_topojson_job_hands_the_external_tiler_decoded_geojson() {
    let StubTiler {
        state, _jar_dir, ..
    } = state_with_stub_tiler().await;
    let asset_id = upload(&state, "fields.topojson", QUANTIZED_TOPOLOGY).await;
    let worker = Arc::clone(&state.job_queue).start().await;

    let deadline = Instant::now() + WAIT_LIMIT;
    let job = loop {
        let job = state
            .db
            .list_jobs_for_asset(asset_id)
            .await
            .unwrap()
            .remove(0);
        if matches!(job.status, JobStatus::Done | JobStatus::Failed) {
            break job;
        }
        assert!(Instant::now() < deadline, "the job never settled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    worker.abort();
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    assert_eq!(job.tiles_written, 1);
    let asset = state.db.get_asset(asset_id).await.unwrap().unwrap();
    assert!(matches!(asset.status, AssetStatus::Ready), "{asset:?}");

    let asset_dir = state.data_dir.join(asset_id.to_string());
    assert_eq!(
        std::fs::read_to_string(asset_dir.join("received_input_type")).unwrap(),
        "geojson"
    );
    let received: Vec<std::ffi::OsString> = std::fs::read_dir(asset_dir.join("received"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(received, ["topology.geojson"]);
    let geojson: Value = serde_json::from_slice(
        &std::fs::read(asset_dir.join("received/topology.geojson")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        geojson,
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "west",
                "properties": {"crop": "wheat"},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[
                        [100.0, 10.0],
                        [101.0, 10.0],
                        [101.0, 11.0],
                        [100.0, 11.0],
                        [100.0, 10.0]
                    ]],
                },
            }],
        })
    );
}

#[tokio::test]
async fn deleting_the_asset_during_a_tiler_run_removes_what_the_tiler_recreates() {
    let StubTiler {
        state,
        jar,
        _jar_dir,
    } = state_with_stub_tiler().await;
    let job_events = common::subscribe_to_job_events(&state).await;
    let hold = suffixed(&jar, ".hold");
    std::fs::write(&hold, b"").unwrap();
    let asset_id = upload(&state, "places.geojson", EMPTY_GEOJSON).await;
    let worker = Arc::clone(&state.job_queue).start().await;

    let started = suffixed(&jar, ".started");
    let deadline = Instant::now() + WAIT_LIMIT;
    while !started.exists() {
        assert!(Instant::now() < deadline, "the tiler never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/assets/{asset_id}"))
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let asset_dir = state.data_dir.join(asset_id.to_string());
    assert!(!asset_dir.exists());

    std::fs::remove_file(&hold).unwrap();

    let event = common::first_job_event(&job_events).await;
    worker.abort();
    assert_eq!(event["event"], "job.failed", "{event}");
    assert_eq!(event["data"]["asset_id"], asset_id.to_string());
    assert!(
        event["data"]["error"].as_str().unwrap().contains("deleted"),
        "{event}"
    );
    assert!(!asset_dir.exists());
}
