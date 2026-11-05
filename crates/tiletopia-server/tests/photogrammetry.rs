mod common;

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{texture_png, zip_of};
use serde_json::{Value, json};
use tiletopia_core::spatial::WGS84_A;
use tiletopia_server::db::{JobRecord, JobStatus};
use tiletopia_server::photogrammetry::{NODE_ODM_URL_ENV, NodeOdm, TIMEOUT_ENV};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const TASK_UUID: &str = "fake-task";
const JOB_DEADLINE: Duration = Duration::from_secs(60);
const EDITOR: &str = "photogrammetry-editor";
const CUBE_OBJ: &[u8] = b"v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 2 3 4\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";
// a day outlasts every JOB_DEADLINE wait in a test
const HELD_TASK_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

const TEXTURE_NAME: &str = "odm_textured_model_geo_material0000_map_Kd.png";
// a diamond 20 grid metres across, between 10 and 20 metres up
const TEXTURED_OBJ: &str = "mtllib odm_textured_model_geo.mtl\nusemtl material0000\n\
v -10 0 10\nv 10 0 10\nv 0 10 20\nv 0 -10 20\n\
vt 0 0\nvt 1 0\nvt 0 1\nvt 1 1\nvn 0 0 1\n\
f 1/1/1 2/2/1 3/3/1\nf 1/1/1 4/4/1 2/2/1\n";
// the diamond's centre sits on utm 32N's central meridian, 10 grid metres north of the equator
const COORDS: &str = "WGS84 UTM 32N\n500000 10\n";
const CENTRAL_MERIDIAN_DEGREES: f64 = 9.0;
const CENTRAL_SCALE_FACTOR: f64 = 0.9996;
const CENTRE_HEIGHT: f64 = 15.0;

#[derive(Clone, Copy)]
enum Ending {
    Complete,
    CompleteWithoutTexture,
    Fail,
    RunForever,
    CompleteOnceReleased,
    DownloadForever,
}

#[derive(Default)]
struct NodeCalls {
    options: Option<String>,
    outputs: Option<String>,
    uploaded: Vec<String>,
    committed: bool,
    cancelled: bool,
    removed: bool,
    released: bool,
}

#[derive(Clone)]
struct FakeNode {
    ending: Ending,
    result_zip: Arc<Vec<u8>>,
    calls: Arc<Mutex<NodeCalls>>,
}

async fn init_task(State(node): State<FakeNode>, mut multipart: Multipart) -> Json<Value> {
    while let Some(field) = multipart.next_field().await.unwrap() {
        let name = field.name().unwrap().to_string();
        let text = field.text().await.unwrap();
        let mut calls = node.calls.lock().unwrap();
        match name.as_str() {
            "options" => calls.options = Some(text),
            "outputs" => calls.outputs = Some(text),
            _ => {}
        }
    }
    Json(json!({ "uuid": TASK_UUID }))
}

async fn upload_photo(
    State(node): State<FakeNode>,
    Path(uuid): Path<String>,
    mut multipart: Multipart,
) -> Json<Value> {
    assert_eq!(uuid, TASK_UUID);
    while let Some(field) = multipart.next_field().await.unwrap() {
        assert_eq!(field.name(), Some("images"));
        let file_name = field.file_name().unwrap().to_string();
        assert!(!field.bytes().await.unwrap().is_empty());
        node.calls.lock().unwrap().uploaded.push(file_name);
    }
    Json(json!({ "success": true }))
}

async fn commit_task(State(node): State<FakeNode>, Path(uuid): Path<String>) -> Json<Value> {
    node.calls.lock().unwrap().committed = true;
    Json(json!({ "uuid": uuid }))
}

async fn task_info(State(node): State<FakeNode>) -> Json<Value> {
    let completed = json!({ "status": { "code": 40 }, "progress": 100 });
    let running = json!({ "status": { "code": 20 }, "progress": 50 });
    Json(match node.ending {
        Ending::Complete | Ending::CompleteWithoutTexture | Ending::DownloadForever => completed,
        Ending::Fail => json!({
            "status": { "code": 30, "errorMessage": "Not enough memory" },
            "progress": 40
        }),
        Ending::RunForever => running,
        Ending::CompleteOnceReleased => {
            if node.calls.lock().unwrap().released {
                completed
            } else {
                running
            }
        }
    })
}

async fn task_output() -> Json<Value> {
    Json(json!([
        "Running ODM",
        "MemoryError: the point cloud does not fit"
    ]))
}

async fn download_all(State(node): State<FakeNode>) -> axum::response::Response {
    let zip_header = [(header::CONTENT_TYPE, "application/zip")];
    if matches!(node.ending, Ending::DownloadForever) {
        let chunks = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let chunk = axum::body::Bytes::from_static(&[0u8; 1024]);
            Some((Ok::<_, std::convert::Infallible>(chunk), ()))
        });
        return (zip_header, Body::from_stream(chunks)).into_response();
    }
    (zip_header, node.result_zip.to_vec()).into_response()
}

async fn cancel_task(State(node): State<FakeNode>) -> Json<Value> {
    node.calls.lock().unwrap().cancelled = true;
    Json(json!({ "success": true }))
}

async fn remove_task(State(node): State<FakeNode>) -> Json<Value> {
    node.calls.lock().unwrap().removed = true;
    Json(json!({ "success": true }))
}

fn odm_result_zip(with_texture: bool) -> Vec<u8> {
    let material = format!("newmtl material0000\nKd 1 1 1\nmap_Kd {TEXTURE_NAME}\n");
    let texture_path = format!("odm_texturing/{TEXTURE_NAME}");
    let texture = texture_png();
    let mut entries: Vec<(&str, &[u8])> = vec![
        (
            "odm_texturing/odm_textured_model_geo.obj",
            TEXTURED_OBJ.as_bytes(),
        ),
        (
            "odm_texturing/odm_textured_model_geo.mtl",
            material.as_bytes(),
        ),
        ("odm_georeferencing/coords.txt", COORDS.as_bytes()),
    ];
    if with_texture {
        entries.push((texture_path.as_str(), texture.as_slice()));
    }
    zip_of(&entries)
}

async fn start_fake_node(ending: Ending) -> (NodeOdm, Arc<Mutex<NodeCalls>>) {
    let calls = Arc::new(Mutex::new(NodeCalls::default()));
    let with_texture = !matches!(ending, Ending::CompleteWithoutTexture);
    let node = FakeNode {
        ending,
        result_zip: Arc::new(odm_result_zip(with_texture)),
        calls: Arc::clone(&calls),
    };
    let app = Router::new()
        .route("/task/new/init", post(init_task))
        .route("/task/new/upload/{uuid}", post(upload_photo))
        .route("/task/new/commit/{uuid}", post(commit_task))
        .route("/task/{uuid}/info", get(task_info))
        .route("/task/{uuid}/output", get(task_output))
        .route("/task/{uuid}/download/all.zip", get(download_all))
        .route("/task/cancel", post(cancel_task))
        .route("/task/remove", post(remove_task))
        .with_state(node);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let node_odm = NodeOdm {
        base_url: format!("http://{address}"),
        timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(20),
    };
    (node_odm, calls)
}

fn degrees(whole: u32) -> exif::Value {
    let rational = |num| exif::Rational { num, denom: 1 };
    exif::Value::Rational(vec![rational(whole), rational(0), rational(0)])
}

fn gps_field(tag: exif::Tag, value: exif::Value) -> exif::Field {
    exif::Field {
        tag,
        ifd_num: exif::In::PRIMARY,
        value,
    }
}

fn photo(seed: u32, with_gps: bool) -> Vec<u8> {
    // noise keeps the JPEG as incompressible as a real photo
    let mut state = seed;
    let image = image::RgbImage::from_fn(64, 64, |_, _| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let [r, g, b, _] = state.to_le_bytes();
        image::Rgb([r, g, b])
    });
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&image)
        .unwrap();

    let mut fields = vec![gps_field(
        exif::Tag::Make,
        exif::Value::Ascii(vec![b"fixture".to_vec()]),
    )];
    if with_gps {
        fields.extend([
            gps_field(
                exif::Tag::GPSLatitudeRef,
                exif::Value::Ascii(vec![b"N".to_vec()]),
            ),
            gps_field(exif::Tag::GPSLatitude, degrees(48)),
            gps_field(
                exif::Tag::GPSLongitudeRef,
                exif::Value::Ascii(vec![b"E".to_vec()]),
            ),
            gps_field(exif::Tag::GPSLongitude, degrees(9)),
        ]);
    }
    let mut writer = exif::experimental::Writer::new();
    for field in &fields {
        writer.push_field(field);
    }
    let mut tiff = Cursor::new(Vec::new());
    writer.write(&mut tiff, false).unwrap();

    // an APP1 segment: its marker, a big-endian length that counts itself, "Exif\0\0", then the TIFF block
    let payload = [b"Exif\0\0".as_slice(), tiff.get_ref()].concat();
    let mut tagged = jpeg[..2].to_vec();
    tagged.extend_from_slice(&[0xFF, 0xE1]);
    tagged.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    tagged.extend_from_slice(&payload);
    tagged.extend_from_slice(&jpeg[2..]);
    tagged
}

fn photo_set() -> Vec<u8> {
    let first = photo(1, true);
    let second = photo(2, true);
    zip_of(&[
        ("flight/DJI_0001.JPG", first.as_slice()),
        ("flight/DJI_0002.jpg", second.as_slice()),
    ])
}

async fn upload(
    state: &Arc<AppState>,
    file_name: &str,
    contents: &[u8],
    fields: &[(&str, &str)],
) -> (StatusCode, Value) {
    let boundary = "tiletopiaphotogrammetryboundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(contents);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

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
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, body)
}

async fn uploaded_asset(state: &Arc<AppState>, fields: &[(&str, &str)]) -> Uuid {
    let (status, body) = upload(state, "survey.zip", &photo_set(), fields).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn uploaded_mesh(state: &Arc<AppState>) -> Uuid {
    let (status, body) = upload(
        state,
        "cube.obj",
        CUBE_OBJ,
        &[("longitude", "9"), ("latitude", "48")],
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn delete_asset(state: &Arc<AppState>, asset_id: Uuid) {
    let response = router(Arc::clone(state))
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
}

async fn post_re_tile(state: &Arc<AppState>, asset_id: Uuid) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/assets/{asset_id}/tile"))
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

async fn wait_for_job(
    state: &Arc<AppState>,
    asset_id: Uuid,
    settled: impl Fn(&JobRecord) -> bool,
) -> JobRecord {
    let deadline = Instant::now() + JOB_DEADLINE;
    while Instant::now() < deadline {
        let jobs = state.db.list_jobs_for_asset(asset_id).await.unwrap();
        if let Some(job) = jobs.into_iter().next().filter(|job| settled(job)) {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the job of asset {asset_id} never reached the state asked for");
}

fn has_ended(job: &JobRecord) -> bool {
    matches!(job.status, JobStatus::Done | JobStatus::Failed)
}

fn is_held_on_node(job: &JobRecord) -> bool {
    // nodeodm's 50 percent lands in the job between the photo upload and the tiling
    (job.progress - 0.5).abs() < 1e-9
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_set_is_reconstructed_by_node_odm_and_tiled_at_its_utm_placement() {
    let (node_odm, calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[("photogrammetry_preset", "high_quality")]).await;
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    assert!(job.tiles_written > 0);
    let asset = state.db.get_asset(asset_id).await.unwrap().unwrap();
    assert!(
        matches!(asset.status, AssetStatus::Ready),
        "{:?}",
        asset.status
    );

    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.uploaded, ["DJI_0001.JPG", "DJI_0002.jpg"]);
        assert!(calls.committed);
        assert!(calls.removed);
        assert!(!calls.cancelled);
        let options: Value = serde_json::from_str(calls.options.as_deref().unwrap()).unwrap();
        assert!(
            options
                .as_array()
                .unwrap()
                .contains(&json!({ "name": "mesh-size", "value": 300_000 })),
            "{options}"
        );
        let outputs: Value = serde_json::from_str(calls.outputs.as_deref().unwrap()).unwrap();
        assert_eq!(
            outputs,
            json!(["odm_texturing", "odm_georeferencing/coords.txt"])
        );
    }

    let asset_dir = state.data_dir.join(asset_id.to_string());
    let tileset: Value =
        serde_json::from_slice(&std::fs::read(asset_dir.join("tileset.json")).unwrap()).unwrap();
    let transform: Vec<f64> = serde_json::from_value(tileset["root"]["transform"].clone()).unwrap();
    let longitude = CENTRAL_MERIDIAN_DEGREES.to_radians();
    let expected = [
        (WGS84_A + CENTRE_HEIGHT) * longitude.cos(),
        (WGS84_A + CENTRE_HEIGHT) * longitude.sin(),
        10.0 / CENTRAL_SCALE_FACTOR,
    ];
    for axis in 0..3 {
        assert!(
            (transform[12 + axis] - expected[axis]).abs() < 0.01,
            "root transform origin {:?}, expected {expected:?}",
            &transform[12..15]
        );
    }
    assert!(!asset_dir.join("reconstruction").exists());

    let root_tile = std::fs::read(asset_dir.join("tiles/root.glb")).unwrap();
    let glb = gltf::Glb::from_slice(&root_tile).unwrap();
    let gltf_json: Value = serde_json::from_slice(&glb.json).unwrap();
    assert!(
        !gltf_json["images"].as_array().unwrap().is_empty(),
        "{gltf_json}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_set_zipped_on_a_mac_skips_its_resource_forks() {
    let (node_odm, calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    let first = photo(1, true);
    let second = photo(2, true);
    let zip = zip_of(&[
        ("flight/DJI_0001.JPG", first.as_slice()),
        ("__MACOSX/._DJI_0001.JPG", b"resource fork".as_slice()),
        ("flight/DJI_0002.jpg", second.as_slice()),
    ]);

    let (status, body) = upload(&state, "survey.zip", &zip, &[]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let asset_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    assert_eq!(
        calls.lock().unwrap().uploaded,
        ["DJI_0001.JPG", "DJI_0002.jpg"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_textured_model_whose_texture_is_missing_fails_the_job() {
    let (node_odm, _calls) = start_fake_node(Ending::CompleteWithoutTexture).await;
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.contains("no readable texture"), "{error}");
    let asset_dir = state.data_dir.join(asset_id.to_string());
    assert!(!asset_dir.join("tileset.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_set_job_without_node_odm_fails_naming_the_variable() {
    let state = common::test_state().await;
    let asset = Asset {
        id: Uuid::new_v4(),
        name: "survey.zip".to_string(),
        asset_type: AssetType::Model,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: String::new(),
        tags: vec![],
        owner_id: None,
    };
    state.db.create_asset(&asset).await.unwrap();
    state
        .job_queue
        .submit(
            asset.id,
            "survey.zip".to_string(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset.id, has_ended).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.contains(NODE_ODM_URL_ENV), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_node_odm_task_fails_the_job_with_its_message_and_console_tail() {
    let (node_odm, calls) = start_fake_node(Ending::Fail).await;
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.contains("Not enough memory"), "{error}");
    assert!(error.contains("MemoryError"), "{error}");
    let asset = state.db.get_asset(asset_id).await.unwrap().unwrap();
    assert!(
        matches!(asset.status, AssetStatus::Error),
        "{:?}",
        asset.status
    );
    let calls = calls.lock().unwrap();
    assert!(calls.cancelled && calls.removed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconstruction_past_its_timeout_is_cancelled_on_the_node() {
    let (mut node_odm, calls) = start_fake_node(Ending::RunForever).await;
    node_odm.timeout = Duration::from_secs(1);
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;

    let job = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.contains(TIMEOUT_ENV), "{error}");
    let calls = calls.lock().unwrap();
    assert!(calls.cancelled && calls.removed);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_asset_cancels_its_running_reconstruction() {
    let (node_odm, calls) = start_fake_node(Ending::RunForever).await;
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;

    let running = wait_for_job(&state, asset_id, is_held_on_node).await;
    assert_eq!(running.status, JobStatus::Running);

    delete_asset(&state, asset_id).await;

    let deadline = Instant::now() + JOB_DEADLINE;
    while !calls.lock().unwrap().removed {
        assert!(
            Instant::now() < deadline,
            "the task was never removed from the node"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(calls.lock().unwrap().cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_asset_stops_its_running_download_and_leaves_no_directory() {
    let (mut node_odm, calls) = start_fake_node(Ending::DownloadForever).await;
    node_odm.timeout = HELD_TASK_TIMEOUT;
    let state = common::state_with_node_odm(node_odm).await;
    let job_events = common::subscribe_to_job_events(&state).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;

    let asset_dir = state.data_dir.join(asset_id.to_string());
    let partial_download = asset_dir.join("reconstruction/all.zip");
    let deadline = Instant::now() + JOB_DEADLINE;
    while !std::fs::metadata(&partial_download).is_ok_and(|file| file.len() > 0) {
        assert!(Instant::now() < deadline, "the download never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    delete_asset(&state, asset_id).await;

    let event = common::first_job_event(&job_events).await;
    assert_eq!(event["event"], "job.failed", "{event}");
    assert_eq!(event["data"]["asset_id"], asset_id.to_string());
    assert!(
        event["data"]["error"].as_str().unwrap().contains("deleted"),
        "{event}"
    );
    assert!(!asset_dir.exists());
    let calls = calls.lock().unwrap();
    assert!(calls.cancelled && calls.removed);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_upload_finishes_while_a_reconstruction_is_held_open_on_the_node() {
    let (mut node_odm, calls) = start_fake_node(Ending::CompleteOnceReleased).await;
    node_odm.timeout = HELD_TASK_TIMEOUT;
    let state = common::state_with_node_odm(node_odm).await;
    let photo_asset = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;
    wait_for_job(&state, photo_asset, is_held_on_node).await;

    let mesh_asset = uploaded_mesh(&state).await;
    let mesh_job = wait_for_job(&state, mesh_asset, has_ended).await;
    assert_eq!(mesh_job.status, JobStatus::Done, "{:?}", mesh_job.error);
    assert!(mesh_job.tiles_written > 0);

    let held = wait_for_job(&state, photo_asset, |_| true).await;
    assert_eq!(held.status, JobStatus::Running, "{:?}", held.error);
    assert!(is_held_on_node(&held), "progress {}", held.progress);
    let asset = state.db.get_asset(photo_asset).await.unwrap().unwrap();
    assert!(
        matches!(asset.status, AssetStatus::Tiling),
        "{:?}",
        asset.status
    );
    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{photo_asset}"))
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let reported: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reported["status"], "IN_PROGRESS");
    assert_eq!(
        reported["percentComplete"],
        (held.progress * 100.0) as u64,
        "{reported}"
    );

    calls.lock().unwrap().released = true;
    let reconstruction = wait_for_job(&state, photo_asset, has_ended).await;
    assert_eq!(
        reconstruction.status,
        JobStatus::Done,
        "{:?}",
        reconstruction.error
    );
    assert!(reconstruction.tiles_written > 0);
    let asset = state.db.get_asset(photo_asset).await.unwrap().unwrap();
    assert!(
        matches!(asset.status, AssetStatus::Ready),
        "{:?}",
        asset.status
    );
    let calls = calls.lock().unwrap();
    assert!(calls.removed && !calls.cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_re_tile_stays_queued_until_its_assets_reconstruction_ends() {
    let (mut node_odm, calls) = start_fake_node(Ending::CompleteOnceReleased).await;
    node_odm.timeout = HELD_TASK_TIMEOUT;
    let state = common::state_with_node_odm(node_odm).await;
    let photo_asset = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;
    wait_for_job(&state, photo_asset, is_held_on_node).await;

    post_re_tile(&state, photo_asset).await;

    // the queue reaches this newer job only by passing over the re-tile
    let mesh_asset = uploaded_mesh(&state).await;
    let mesh_job = wait_for_job(&state, mesh_asset, has_ended).await;
    assert_eq!(mesh_job.status, JobStatus::Done, "{:?}", mesh_job.error);

    let jobs = state.db.list_jobs_for_asset(photo_asset).await.unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].status, JobStatus::Queued);
    assert_eq!(jobs[1].status, JobStatus::Running);

    calls.lock().unwrap().released = true;
    let re_tile = wait_for_job(&state, photo_asset, has_ended).await;
    assert_eq!(re_tile.status, JobStatus::Done, "{:?}", re_tile.error);
    let first = state.db.get_job(jobs[1].id).await.unwrap().unwrap();
    assert_eq!(first.status, JobStatus::Done, "{:?}", first.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_photo_set_re_tile_reads_its_zip_after_the_data_directory_moves() {
    let (node_odm, _) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    let asset_id = uploaded_asset(&state, &[]).await;
    Arc::clone(&state.job_queue).start().await;
    let first = wait_for_job(&state, asset_id, has_ended).await;
    assert_eq!(first.status, JobStatus::Done, "{:?}", first.error);
    assert_eq!(first.input_path, "input/survey.zip");

    sqlx::query("UPDATE jobs SET input_path = ?")
        .bind(format!("/moved/away/{asset_id}/input/survey.zip"))
        .execute(&state.db.pool)
        .await
        .unwrap();

    post_re_tile(&state, asset_id).await;

    let re_tile = wait_for_job(&state, asset_id, has_ended).await;
    assert_ne!(re_tile.id, first.id);
    assert_eq!(re_tile.status, JobStatus::Done, "{:?}", re_tile.error);
    assert_eq!(re_tile.input_path, "input/survey.zip");
}

async fn mark_newest_job_running(state: &Arc<AppState>, asset_id: Uuid) -> JobRecord {
    let mut job = state
        .db
        .list_jobs_for_asset(asset_id)
        .await
        .unwrap()
        .remove(0);
    job.status = JobStatus::Running;
    state.db.update_job(&job).await.unwrap();
    job
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_fails_jobs_left_running_and_a_re_tile_recovers_the_asset() {
    let (node_odm, _calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;

    let photo_asset = uploaded_asset(&state, &[]).await;
    let stuck_photo_job = mark_newest_job_running(&state, photo_asset).await;
    let mut stored = state.db.get_asset(photo_asset).await.unwrap().unwrap();
    stored.status = AssetStatus::Tiling;
    state.db.update_asset(&stored).await.unwrap();

    let imagery = Asset {
        id: Uuid::new_v4(),
        name: "aerial.tif".to_string(),
        asset_type: AssetType::Imagery,
        status: AssetStatus::Ready,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: String::new(),
        tags: vec![],
        owner_id: None,
    };
    state.db.create_asset(&imagery).await.unwrap();
    state
        .job_queue
        .submit(
            imagery.id,
            "aerial.tif".to_string(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    let stuck_imagery_job = mark_newest_job_running(&state, imagery.id).await;

    let mesh_asset = uploaded_mesh(&state).await;

    assert_eq!(state.job_queue.fail_interrupted_jobs().await.unwrap(), 2);

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/jobs/{}", stuck_photo_job.id))
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let polled: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(polled["status"], "failed");
    assert!(
        polled["error"].as_str().unwrap().contains("restarted"),
        "{polled}"
    );
    let stored = state.db.get_asset(photo_asset).await.unwrap().unwrap();
    assert!(
        matches!(stored.status, AssetStatus::Error),
        "{:?}",
        stored.status
    );

    // record_outcome leaves a ready imagery asset ready when a re-tile fails
    let imagery_job = state
        .db
        .get_job(stuck_imagery_job.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(imagery_job.status, JobStatus::Failed);
    let stored = state.db.get_asset(imagery.id).await.unwrap().unwrap();
    assert!(
        matches!(stored.status, AssetStatus::Ready),
        "{:?}",
        stored.status
    );

    let mesh_jobs = state.db.list_jobs_for_asset(mesh_asset).await.unwrap();
    assert_eq!(mesh_jobs[0].status, JobStatus::Queued);

    post_re_tile(&state, photo_asset).await;
    Arc::clone(&state.job_queue).start().await;
    let re_tile = wait_for_job(&state, photo_asset, has_ended).await;
    assert_eq!(re_tile.status, JobStatus::Done, "{:?}", re_tile.error);
    let stored = state.db.get_asset(photo_asset).await.unwrap().unwrap();
    assert!(
        matches!(stored.status, AssetStatus::Ready),
        "{:?}",
        stored.status
    );
}

#[tokio::test]
async fn a_photo_set_upload_without_node_odm_names_the_variable() {
    let state = common::test_state().await;
    let (status, body) = upload(&state, "survey.zip", &photo_set(), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains(NODE_ODM_URL_ENV), "{body}");
}

#[tokio::test]
async fn a_zip_of_anything_but_unique_gps_tagged_jpegs_is_refused() {
    let (node_odm, _calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    let gps_photo = photo(3, true);
    let gps_photo = gps_photo.as_slice();
    let photo_without_gps = photo(4, false);
    let mut padded_photo = gps_photo.to_vec();
    padded_photo.extend(std::iter::repeat_n(0u8, 4 * 1024 * 1024));

    let cases: [(&str, Vec<u8>, &str); 7] = [
        (
            "a path out of the zip",
            zip_of(&[("../escape.jpg", gps_photo)]),
            "leaves",
        ),
        (
            "an absolute path",
            zip_of(&[("/etc/escape.jpg", gps_photo)]),
            "leaves",
        ),
        (
            "a file that is no photo",
            zip_of(&[("a.jpg", gps_photo), ("notes.txt", b"hello".as_slice())]),
            "JPEG",
        ),
        (
            "a photo without GPS",
            zip_of(&[("plain.jpg", photo_without_gps.as_slice())]),
            "GPSLatitude",
        ),
        (
            "two photos with one name",
            zip_of(&[("a/same.jpg", gps_photo), ("b/same.jpg", gps_photo)]),
            "once",
        ),
        (
            "a photo that unpacks far past its zip",
            zip_of(&[("bomb.jpg", padded_photo.as_slice())]),
            "times the zip",
        ),
        ("an empty zip", zip_of(&[]), "no photos"),
    ];
    for (case, zip, named) in cases {
        let (status, body) = upload(&state, "survey.zip", &zip, &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{case}: {body}");
        assert!(body.to_string().contains(named), "{case}: {body}");
    }
    assert!(state.db.list_assets().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_photo_set_takes_no_placement_or_ktx2_and_only_known_presets() {
    let (node_odm, _calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    for (fields, named) in [
        (vec![("longitude", "9"), ("latitude", "48")], "EXIF GPS"),
        (vec![("crs", "EPSG:32632")], "EXIF GPS"),
        (vec![("texture_compression", "ktx2")], "ktx2"),
        (vec![("photogrammetry_preset", "ultra")], "high_quality"),
    ] {
        let (status, body) = upload(&state, "survey.zip", &photo_set(), &fields).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{fields:?}: {body}");
        assert!(body.to_string().contains(named), "{fields:?}: {body}");
    }

    let (status, body) = upload(
        &state,
        "cube.obj",
        b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n",
        &[("photogrammetry_preset", "fast")],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("zip photo sets"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_presets_route_lists_each_preset_with_its_odm_options() {
    let (node_odm, calls) = start_fake_node(Ending::Complete).await;
    let state = common::state_with_node_odm(node_odm).await;
    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri("/api/v1/photogrammetry/presets")
                .header(
                    "authorization",
                    format!("Bearer {}", common::token(EDITOR, "viewer")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = body["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|preset| preset["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fast", "default", "high_quality"]);
    assert!(
        body["presets"][0]["odm_options"]
            .as_array()
            .unwrap()
            .contains(&json!({ "name": "pc-quality", "value": "low" }))
    );

    Arc::clone(&state.job_queue).start().await;
    for preset in body["presets"].as_array().unwrap() {
        let name = preset["name"].as_str().unwrap();
        let asset_id = uploaded_asset(&state, &[("photogrammetry_preset", name)]).await;
        let job = wait_for_job(&state, asset_id, has_ended).await;
        assert_eq!(job.status, JobStatus::Done, "{name}: {:?}", job.error);
        let sent = calls.lock().unwrap().options.clone().unwrap();
        let sent: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent, preset["odm_options"], "{name}");
    }
}
