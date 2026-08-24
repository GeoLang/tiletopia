#![cfg(feature = "martin")]

//! The whole vector tileset loop: upload, build, serve, delete. Every case that
//! needs a real build skips itself when tippecanoe is not installed.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tiletopia_server::db::{TilesetRecord, TilesetStatus};
use tiletopia_server::map_tiles::martin_backend::MartinTileBackend;
use tiletopia_server::tilesets::{
    MAX_NAME_CHARS, TilesetBuilder, register_ready_tilesets, tippecanoe_version,
};
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
    let worker = builder_for(&state).start();
    (state, worker)
}

fn builder_for(state: &Arc<AppState>) -> Arc<TilesetBuilder> {
    Arc::new(
        TilesetBuilder::new(
            Arc::clone(&state.db),
            state.data_dir.clone(),
            state.tileset_dir.clone(),
            state.martin_backend.clone(),
            state.current_tileset_build.clone(),
        )
        .unwrap(),
    )
}

async fn upload(
    state: &Arc<AppState>,
    token: &str,
    filename: &str,
    contents: &str,
) -> (StatusCode, serde_json::Value) {
    upload_named(state, token, filename, contents, None).await
}

async fn upload_named(
    state: &Arc<AppState>,
    token: &str,
    filename: &str,
    contents: &str,
    name: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let boundary = "tiletopiatilesetboundary";
    let named = name
        .map(|name| {
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n{name}\r\n"
            )
        })
        .unwrap_or_default();
    let body = format!(
        "{named}--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
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

/// Every string anywhere in the value, so a path can be looked for wherever it
/// might sit.
fn strings_in(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items.iter().flat_map(strings_in).collect(),
        serde_json::Value::Object(fields) => fields.values().flat_map(strings_in).collect(),
        _ => Vec::new(),
    }
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

    // the TileJSON a client can actually use: this server's tile URL, the layer
    // list, the uploaded name, and none of the build's paths
    let tilejson = json(&state, &format!("/martin/{id}"), &editor).await;
    assert_eq!(
        tilejson["tiles"],
        serde_json::json!([format!("/martin/{id}/{{z}}/{{x}}/{{y}}")]),
        "{tilejson}"
    );
    assert_eq!(tilejson["name"], "city roads.geojson", "{tilejson}");
    assert_eq!(
        tilejson["vector_layers"][0]["id"], "city_roads",
        "{tilejson}"
    );
    for text in strings_in(&tilejson) {
        assert!(
            !text.starts_with('/') || text == format!("/martin/{id}/{{z}}/{{x}}/{{y}}"),
            "an absolute path reached the client: {text}"
        );
    }

    // tippecanoe writes gzipped mvt, so the response must say so or no
    // browser client can decode the body
    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/martin/{id}/0/0/0"))
                .header("authorization", format!("Bearer {editor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_ENCODING)
            .map(|value| value.to_str().unwrap().to_string()),
        Some("gzip".to_string())
    );
    let tile = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!tile.is_empty());
    assert_eq!(&tile[..2], [0x1f, 0x8b], "not a gzip body");

    // a coordinate outside the zoom's grid is the client's mistake
    let (status, _) = request(
        &state,
        "GET",
        &format!("/martin/{id}/0/99999/99999"),
        &editor,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

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
async fn a_build_whose_row_was_deleted_leaves_no_archive_or_source() {
    if !tippecanoe_installed() {
        return;
    }
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // no worker runs, so the build is driven by hand after the row is gone,
    // which is the delete-during-build race without having to win it
    let (_, accepted) = upload(&state, &editor, "roads.geojson", FIXTURE_GEOJSON).await;
    let id: uuid::Uuid = accepted["tileset"]["id"].as_str().unwrap().parse().unwrap();
    let record = state.db.claim_tileset_build().await.unwrap().unwrap();
    assert_eq!(state.db.delete_tileset(id).await.unwrap(), 1);

    builder_for(&state).build(record).await;

    assert!(!state.martin_backend.contains(&id.to_string()).await);
    assert!(!state.tileset_dir.join(format!("{id}.pmtiles")).exists());
    assert!(state.db.get_tileset(id).await.unwrap().is_none());
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

/// The journal tippecanoe keeps beside an archive while it writes.
fn journal_of(archive: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{}-journal", archive.display()))
}

/// A one-tile PMTiles archive, so a source can be registered without a build.
fn write_archive(path: &std::path::Path) {
    use pmtiles::{PmTilesWriter, TileCoord, TileType};
    let file = std::fs::File::create(path).unwrap();
    let mut writer = PmTilesWriter::new(TileType::Mvt).create(file).unwrap();
    writer
        .add_tile(TileCoord::new(0, 0, 0).unwrap(), b"tile bytes")
        .unwrap();
    writer.finalize().unwrap();
}

/// A ready registry row with its archive registered, without running a build.
async fn register_tileset(state: &Arc<AppState>, owner: &str, name: &str) -> String {
    let id = uuid::Uuid::new_v4();
    let object_key = format!("{id}.pmtiles");
    let archive = state.tileset_dir.join(&object_key);
    write_archive(&archive);
    let record = TilesetRecord {
        id,
        name: name.to_string(),
        status: TilesetStatus::Ready,
        source_id: id.to_string(),
        object_key,
        original_filename: format!("{name}.geojson"),
        layer_name: "roads".to_string(),
        argv: Vec::new(),
        size_bytes: archive.metadata().unwrap().len(),
        created_at: chrono::Utc::now(),
        built_at: Some(chrono::Utc::now()),
        error: None,
        owner_id: owner.to_string(),
        started_at: None,
    };
    state.db.create_tileset(&record).await.unwrap();
    state
        .martin_backend
        .add_pmtiles(&record.source_id, &archive)
        .await
        .unwrap();
    id.to_string()
}

/// The source ids the catalog shows this caller, sorted so the order the
/// backend hands them back in does not matter.
async fn catalog_ids(state: &Arc<AppState>, token: &str) -> Vec<String> {
    let catalog = json(state, "/martin/catalog", token).await;
    let mut ids: Vec<String> = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

fn sorted(ids: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_build_a_restart_interrupted_finishes_when_it_is_queued_again() {
    if !tippecanoe_installed() {
        return;
    }
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // no worker runs, so the row is claimed by hand and left the way a restart
    // finds one: claimed, its input still on disk, a fragment of the archive
    // and the journal beside it
    let (_, accepted) = upload(&state, &editor, "roads.geojson", FIXTURE_GEOJSON).await;
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();
    let claimed = state.db.claim_tileset_build().await.unwrap().unwrap();
    assert_eq!(claimed.id.to_string(), id);

    let archive = state.tileset_dir.join(format!("{id}.pmtiles"));
    std::fs::write(&archive, b"half an archive").unwrap();
    std::fs::write(journal_of(&archive), b"half a journal").unwrap();
    let input = state
        .data_dir
        .join("tileset_builds")
        .join(&id)
        .join("source.geojson");
    assert!(input.is_file(), "{}", input.display());

    assert_eq!(state.db.requeue_claimed_tileset_builds().await.unwrap(), 1);
    let worker = builder_for(&state).start();

    let record = await_build(&state, &id, &editor).await;
    assert_eq!(record["status"], "ready", "{record}");
    assert!(!journal_of(&archive).exists());
    assert!(archive.metadata().unwrap().len() > b"half an archive".len() as u64);

    let (status, tile) = request(&state, "GET", &format!("/martin/{id}/0/0/0"), &editor).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!tile.is_empty());

    worker.abort();
}

#[tokio::test]
async fn a_failed_build_takes_the_journal_with_it() {
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // a run that writes the journal and then fails, which is the state a killed
    // tippecanoe leaves the tileset directory in
    let id = uuid::Uuid::new_v4();
    let archive = state.tileset_dir.join(format!("{id}.pmtiles"));
    let journal = journal_of(&archive);
    let record = TilesetRecord {
        id,
        name: "killed".to_string(),
        status: TilesetStatus::Building,
        source_id: id.to_string(),
        object_key: format!("{id}.pmtiles"),
        original_filename: "roads.geojson".to_string(),
        layer_name: "roads".to_string(),
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("touch '{}'; exit 3", journal.display()),
        ],
        size_bytes: 0,
        created_at: chrono::Utc::now(),
        built_at: None,
        error: None,
        owner_id: "tileset-editor".to_string(),
        started_at: None,
    };
    state.db.create_tileset(&record).await.unwrap();

    builder_for(&state).build(record).await;

    let row = state.db.get_tileset(id).await.unwrap().unwrap();
    assert_eq!(row.status, TilesetStatus::Failed);
    assert!(!journal.exists(), "the journal outlived the failed build");
    assert!(!archive.exists());
}

#[tokio::test]
async fn deleting_a_tileset_takes_the_journal_with_it() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let (status, accepted) = upload(&state, &editor, "roads.geojson", FIXTURE_GEOJSON).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = accepted["tileset"]["id"].as_str().unwrap().to_string();
    let archive = state.tileset_dir.join(format!("{id}.pmtiles"));
    std::fs::write(&archive, b"an archive").unwrap();
    std::fs::write(journal_of(&archive), b"a journal a killed build left").unwrap();

    let (status, _) = request(&state, "DELETE", &format!("/api/v1/tilesets/{id}"), &editor).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!archive.exists());
    assert!(
        !journal_of(&archive).exists(),
        "the journal outlived the row"
    );
}

#[tokio::test]
async fn a_name_past_the_cap_is_refused_and_never_stored() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let longest = "n".repeat(MAX_NAME_CHARS);
    let (status, accepted) = upload_named(
        &state,
        &editor,
        "roads.geojson",
        FIXTURE_GEOJSON,
        Some(&longest),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["tileset"]["name"], longest);

    for name in [
        "n".repeat(MAX_NAME_CHARS + 1),
        // far past the cap: the bytes are counted as they arrive, so this one is
        // refused rather than buffered and stored
        "n".repeat(4 * 1024 * 1024),
    ] {
        let (status, _) = upload_named(
            &state,
            &editor,
            "roads.geojson",
            FIXTURE_GEOJSON,
            Some(&name),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{} characters", name.len());
    }

    let listed = json(&state, "/api/v1/tilesets", &editor).await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
}

#[tokio::test]
async fn the_catalog_shows_an_operator_archive_to_everyone_and_a_tileset_to_its_owner() {
    let owner = token("catalog-owner", "editor");
    let stranger = token("catalog-stranger", "editor");
    let admin = token("catalog-admin", "admin");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let owned = register_tileset(&state, "catalog-owner", "owned roads").await;
    let others = register_tileset(&state, "catalog-stranger", "other roads").await;

    // an archive from TILETOPIA_PMTILES_DIR has no registry row and stays
    // visible to every signed-in caller
    let operator = state.tileset_dir.join("basemap.pmtiles");
    write_archive(&operator);
    state
        .martin_backend
        .add_pmtiles("basemap", &operator)
        .await
        .unwrap();

    assert_eq!(
        catalog_ids(&state, &owner).await,
        sorted(&["basemap", &owned])
    );
    assert_eq!(
        catalog_ids(&state, &stranger).await,
        sorted(&["basemap", &others])
    );
    assert_eq!(
        catalog_ids(&state, &admin).await,
        sorted(&["basemap", &owned, &others])
    );

    // the tile and TileJSON routes stay open to any signed-in caller: a shared
    // viewer reads another member's tileset by id
    let (status, _) = request(&state, "GET", &format!("/martin/{owned}"), &stranger).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(&state, "GET", &format!("/martin/{owned}/0/0/0"), &stranger).await;
    assert_eq!(status, StatusCode::OK);
}

/// How long a build a delete has cancelled would otherwise sleep for, far
/// longer than the case is willing to wait.
const CANCELLED_BUILD_SLEEP: &str = "30";

/// What a cancelled build is given to be killed, cleaned up after and reported,
/// wide enough that a loaded machine does not fail the case.
const CANCEL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// A claimed row whose build is this command rather than tippecanoe, so a case
/// can hold a build open or finish one without the binary. The command is built
/// from the archive path the builder will look for.
async fn queue_build(
    state: &Arc<AppState>,
    owner: &str,
    argv: impl FnOnce(&std::path::Path) -> Vec<String>,
) -> TilesetRecord {
    let id = uuid::Uuid::new_v4();
    let object_key = format!("{id}.pmtiles");
    let record = TilesetRecord {
        id,
        name: "stand-in build".to_string(),
        status: TilesetStatus::Building,
        source_id: id.to_string(),
        argv: argv(&state.tileset_dir.join(&object_key)),
        object_key,
        original_filename: "roads.geojson".to_string(),
        layer_name: "roads".to_string(),
        size_bytes: 0,
        created_at: chrono::Utc::now(),
        built_at: None,
        error: None,
        owner_id: owner.to_string(),
        started_at: None,
    };
    state.db.create_tileset(&record).await.unwrap();
    record
}

/// Wait until the worker holds the slot for this build, so a delete cannot land
/// before there is a child to kill.
async fn await_running_build(state: &Arc<AppState>, id: uuid::Uuid) {
    let deadline = std::time::Instant::now() + CANCEL_DEADLINE;
    while state.current_tileset_build.building() != Some(id) {
        assert!(
            std::time::Instant::now() < deadline,
            "the build never took the slot"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn deleting_a_building_tileset_kills_the_build_instead_of_waiting_it_out() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let record = queue_build(&state, "tileset-editor", |_| {
        vec!["sleep".to_string(), CANCELLED_BUILD_SLEEP.to_string()]
    })
    .await;
    let id = record.id;
    let builder = builder_for(&state);
    let build = tokio::spawn(async move { builder.build(record).await });
    await_running_build(&state, id).await;

    let (status, _) = request(&state, "DELETE", &format!("/api/v1/tilesets/{id}"), &editor).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    tokio::time::timeout(CANCEL_DEADLINE, build)
        .await
        .expect("the build outlived the delete")
        .unwrap();

    assert!(!state.tileset_dir.join(format!("{id}.pmtiles")).exists());
    assert!(!state.martin_backend.contains(&id.to_string()).await);
    assert!(state.db.get_tileset(id).await.unwrap().is_none());
    assert!(state.current_tileset_build.building().is_none());
}

#[tokio::test]
async fn deleting_a_tileset_that_is_not_building_leaves_the_running_build_alone() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    let other = register_tileset(&state, "tileset-editor", "other roads").await;
    let record = queue_build(&state, "tileset-editor", |_| {
        vec!["sleep".to_string(), CANCELLED_BUILD_SLEEP.to_string()]
    })
    .await;
    let id = record.id;
    let builder = builder_for(&state);
    let build = tokio::spawn(async move { builder.build(record).await });
    await_running_build(&state, id).await;

    let (status, _) = request(
        &state,
        "DELETE",
        &format!("/api/v1/tilesets/{other}"),
        &editor,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!state.tileset_dir.join(format!("{other}.pmtiles")).exists());
    assert!(!state.martin_backend.contains(&other).await);
    assert!(
        state
            .db
            .get_tileset(other.parse().unwrap())
            .await
            .unwrap()
            .is_none()
    );

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(state.current_tileset_build.building(), Some(id));
    assert!(!build.is_finished(), "the other delete killed this build");

    // stop the sleep rather than let the case wait it out
    assert_eq!(state.db.delete_tileset(id).await.unwrap(), 1);
    state.current_tileset_build.cancel(id);
    tokio::time::timeout(CANCEL_DEADLINE, build)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_finished_build_leaves_the_slot_empty_so_a_later_delete_cancels_nothing() {
    let editor = token("tileset-editor", "editor");
    let state = common::build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await;

    // a build that writes a real archive, which is a whole successful run
    // without needing tippecanoe
    let prebuilt = state.data_dir.join("prebuilt.pmtiles");
    write_archive(&prebuilt);
    let record = queue_build(&state, "tileset-editor", |archive| {
        vec![
            "cp".to_string(),
            prebuilt.display().to_string(),
            archive.display().to_string(),
        ]
    })
    .await;
    let id = record.id;

    builder_for(&state).build(record).await;

    assert_eq!(
        state.db.get_tileset(id).await.unwrap().unwrap().status,
        TilesetStatus::Ready
    );
    assert!(state.current_tileset_build.building().is_none());

    let other = register_tileset(&state, "tileset-editor", "other roads").await;
    let (status, _) = request(
        &state,
        "DELETE",
        &format!("/api/v1/tilesets/{other}"),
        &editor,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(state.current_tileset_build.building().is_none());
    assert_eq!(
        state.db.get_tileset(id).await.unwrap().unwrap().status,
        TilesetStatus::Ready
    );
}
