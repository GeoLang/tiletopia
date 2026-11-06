mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, request};
use common::{PLY_FIXTURE, test_state, texture_png, token, zip_of};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiletopia_core::glb_writer::MeshCompression;
use tiletopia_core::mesh_tiler::TextureCompression;
use tiletopia_server::audit::{AuditAction, AuditQuery};
use tiletopia_server::db::JobRecord;
use tiletopia_server::ion_upload::MOST_ASSETS_AWAITING_FILES_PER_OWNER;
use tiletopia_server::upload::MAX_UPLOAD_BYTES;
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const GDAL_COG: &[u8] = include_bytes!("fixtures/imagery_rgb_gdal_cog.tif");

const HOST: &str = "localhost";
const REGION: &str = "us-east-1";
// aws-sdk 2 signs these four headers
const SIGNED_HEADERS: &str = "host;x-amz-content-sha256;x-amz-date;x-amz-security-token";

fn editor() -> String {
    token("ion-upload-owner", "editor")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn create(state: &Arc<AppState>, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/assets")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, bytes) = send(state, request).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn created(state: &Arc<AppState>, ion_type: &str, source_type: &str) -> Value {
    let (status, body) = create(
        state,
        json!({ "name": "survey", "type": ion_type, "options": { "sourceType": source_type } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn location<'a>(created: &'a Value, field: &str) -> &'a str {
    created["uploadLocation"][field].as_str().unwrap()
}

fn created_asset_id(created: &Value) -> Uuid {
    let asset_id = location(created, "prefix")
        .trim_start_matches("sources/")
        .trim_end_matches('/');
    Uuid::parse_str(asset_id).unwrap()
}

fn asset_dir(state: &AppState, created: &Value) -> std::path::PathBuf {
    state.data_dir.join(created_asset_id(created).to_string())
}

fn s3_request(
    created: &Value,
    secret_access_key: &str,
    method: &str,
    file_name: &str,
    canonical_query: &str,
    payload_hash: &str,
) -> request::Builder {
    let path = format!(
        "/{}/{}{file_name}",
        location(created, "bucket"),
        location(created, "prefix")
    );
    let session_token = location(created, "sessionToken");
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &amz_date[..8];
    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\nhost:{HOST}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\nx-amz-security-token:{session_token}\n\n{SIGNED_HEADERS}\n{payload_hash}"
    );
    let scope = format!("{date}/{REGION}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut signing_key = format!("AWS4{secret_access_key}").into_bytes();
    for part in [date, REGION, "s3", "aws4_request"] {
        signing_key = hmac_sha256(&signing_key, part);
    }
    let signature = hex(&hmac_sha256(&signing_key, &string_to_sign));

    let uri = match canonical_query {
        "" => path,
        query => format!("{path}?{query}"),
    };
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("x-amz-content-sha256", payload_hash)
        .header("x-amz-date", amz_date)
        .header("x-amz-security-token", session_token)
        .header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={SIGNED_HEADERS}, Signature={signature}",
                location(created, "accessKey")
            ),
        )
}

async fn put_file(
    state: &Arc<AppState>,
    created: &Value,
    file_name: &str,
    contents: &[u8],
) -> (StatusCode, Vec<u8>) {
    let request = s3_request(
        created,
        location(created, "secretAccessKey"),
        "PUT",
        file_name,
        "",
        &sha256_hex(contents),
    )
    .body(Body::from(contents.to_vec()))
    .unwrap();
    send(state, request).await
}

fn url_path(url: &str) -> String {
    let after_scheme = url.split_once("://").unwrap().1;
    format!("/{}", after_scheme.split_once('/').unwrap().1)
}

async fn upload_complete(state: &Arc<AppState>, created: &Value) -> (StatusCode, Value) {
    let on_complete = &created["onComplete"];
    let request = Request::builder()
        .method(on_complete["method"].as_str().unwrap())
        .uri(url_path(on_complete["url"].as_str().unwrap()))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::from(on_complete["fields"].to_string()))
        .unwrap();
    let (status, bytes) = send(state, request).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn ion_asset(state: &Arc<AppState>, created: &Value) -> Value {
    let id = created["assetMetadata"]["id"].as_i64().unwrap();
    let request = Request::builder()
        .uri(format!("/v1/assets/{id}"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(state, request).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&bytes).unwrap()
}

async fn poll_until_settled(state: &Arc<AppState>, created: &Value) -> Value {
    let worker = Arc::clone(&state.job_queue).start().await;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let asset = ion_asset(state, created).await;
        let settled = matches!(asset["status"].as_str(), Some("COMPLETE" | "ERROR"));
        if settled || Instant::now() > deadline {
            worker.abort();
            return asset;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn element_text<'a>(xml: &'a str, name: &str) -> &'a str {
    let open = format!("<{name}>");
    let start = xml.find(&open).unwrap() + open.len();
    let end = start + xml[start..].find(&format!("</{name}>")).unwrap();
    &xml[start..end]
}

#[tokio::test]
async fn a_point_cloud_goes_from_create_through_an_aws_chunked_put_to_complete() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    assert_eq!(created["assetMetadata"]["status"], "AWAITING_FILES");
    assert_eq!(location(&created, "bucket"), "tiletopia_ion_uploads");
    assert_eq!(created["onComplete"]["method"], "POST");
    assert_eq!(created["onComplete"]["fields"], json!({}));
    assert_eq!(location(&created, "endpoint"), common::TEST_ION_BASE_URL);
    assert_eq!(
        created["onComplete"]["url"],
        format!(
            "{}/v1/assets/{}/uploadComplete",
            common::TEST_ION_BASE_URL,
            created["assetMetadata"]["id"]
        )
    );

    // the aws cli 2 body: aws-chunked, unsigned, with a checksum trailer
    let (first, second) = PLY_FIXTURE.as_bytes().split_at(100);
    let mut body = Vec::new();
    for chunk in [first, second] {
        body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(chunk);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"0\r\nx-amz-checksum-crc64nvme:AAAAAAAAAAA=\r\n\r\n");
    let request = s3_request(
        &created,
        location(&created, "secretAccessKey"),
        "PUT",
        "cloud.ply",
        "",
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
    )
    .header("content-encoding", "aws-chunked")
    .header(
        "x-amz-decoded-content-length",
        PLY_FIXTURE.len().to_string(),
    )
    .header("x-amz-trailer", "x-amz-checksum-crc64nvme")
    .body(Body::from(body))
    .unwrap();
    let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("etag"));
    assert_eq!(
        ion_asset(&state, &created).await["status"],
        "AWAITING_FILES"
    );

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let input = std::fs::read(asset_dir(&state, &created).join("input/cloud.ply")).unwrap();
    assert_eq!(input, PLY_FIXTURE.as_bytes());
    assert_eq!(ion_asset(&state, &created).await["status"], "NOT_STARTED");
    let audit_entries = state.audit_log.query(&AuditQuery::default()).await.unwrap();
    assert!(
        audit_entries
            .iter()
            .any(|entry| entry.action == AuditAction::Upload && entry.resource_type == "asset"),
        "{audit_entries:?}"
    );

    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
    assert_eq!(settled["percentComplete"], 100);
    assert_eq!(settled["type"], "3DTILES");

    let id = created["assetMetadata"]["id"].as_i64().unwrap();
    let request = Request::builder()
        .uri(format!("/v1/assets/{id}/endpoint"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    let endpoint: Value = serde_json::from_slice(&bytes).unwrap();
    let tileset = Request::builder()
        .uri(url_path(endpoint["url"].as_str().unwrap()))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, tileset).await.0, StatusCode::OK);

    let (status, body) = put_file(&state, &created, "again.ply", b"late").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("InvalidAccessKeyId"));
}

#[tokio::test]
async fn an_imagery_upload_is_complete_once_upload_complete_answers() {
    let state = test_state().await;
    let created = created(&state, "IMAGERY", "RASTER_IMAGERY").await;

    let (status, body) = put_file(&state, &created, "ortho.tif", GDAL_COG).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let asset = ion_asset(&state, &created).await;
    assert_eq!(asset["status"], "COMPLETE");
    assert_eq!(asset["type"], "IMAGERY");

    let id = created["assetMetadata"]["id"].as_i64().unwrap();
    let request = Request::builder()
        .uri(format!("/v1/assets/{id}/endpoint"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&state, request).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let endpoint: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(endpoint["externalType"], "URL_TEMPLATE");
    let tile = endpoint["url"]
        .as_str()
        .unwrap()
        .replace("{z}", "0")
        .replace("{x}", "0")
        .replace("{y}", "0");
    let request = Request::builder()
        .uri(url_path(&tile))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_put_with_a_bad_or_expired_signature_is_refused() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let contents = PLY_FIXTURE.as_bytes();

    let forged = s3_request(
        &created,
        "not-the-issued-secret",
        "PUT",
        "cloud.ply",
        "",
        &sha256_hex(contents),
    )
    .body(Body::from(contents.to_vec()))
    .unwrap();
    let (status, body) = send(&state, forged).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("SignatureDoesNotMatch"));

    sqlx::query("UPDATE ion_uploads SET expires_at = ?")
        .bind(chrono::Utc::now().timestamp() - 1)
        .execute(&state.db.pool)
        .await
        .unwrap();
    let (status, body) = put_file(&state, &created, "cloud.ply", contents).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("ExpiredToken"));

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"].as_str().unwrap().contains("no file"),
        "{body}"
    );
}

#[tokio::test]
async fn upload_complete_with_no_files_is_refused_and_the_asset_keeps_waiting() {
    let state = test_state().await;
    let created = created(&state, "TERRAIN", "RASTER_TERRAIN").await;

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("no file") && message.contains(location(&created, "prefix")),
        "{message}"
    );
    assert_eq!(
        ion_asset(&state, &created).await["status"],
        "AWAITING_FILES"
    );
}

#[tokio::test]
async fn unknown_types_and_source_types_are_refused_naming_the_accepted_values() {
    let state = test_state().await;

    let (status, body) = create(
        &state,
        json!({ "name": "model", "type": "GLTF", "options": { "sourceType": "3D_MODEL" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("3DTILES, IMAGERY and TERRAIN"),
        "{message}"
    );

    let (status, body) = create(
        &state,
        json!({ "name": "scan", "type": "3DTILES", "options": { "sourceType": "LIDAR" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("LIDAR")
            && message.contains("POINT_CLOUD for 3DTILES")
            && message.contains("RASTER_IMAGERY for IMAGERY"),
        "{message}"
    );

    let (status, body) = create(
        &state,
        json!({ "name": "city", "type": "3DTILES", "options": { "sourceType": "CITYGML", "clampToTerrain": true } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("clampToTerrain"), "{message}");

    let (status, _) = create(&state, json!({ "name": "untyped" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = create(
        &state,
        json!({ "name": "scan", "type": "TERRAIN", "options": { "sourceType": "POINT_CLOUD" } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("makes a 3DTILES asset"), "{message}");

    let (status, body) = create(
        &state,
        json!({
            "name": "imported",
            "type": "3DTILES",
            "options": { "sourceType": "POINT_CLOUD" },
            "from": { "type": "S3" },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("from"), "{message}");

    let (status, body) = create(
        &state,
        json!({
            "name": "credited",
            "type": "3DTILES",
            "options": { "sourceType": "POINT_CLOUD" },
            "attribution": "NASA",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("attribution"), "{message}");

    // viewtopia polls an uploading asset until its status changes, so a refusal leaves no row
    assert!(state.db.list_assets().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_abandoned_upload_is_expired_by_the_sweep() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let (status, _) = put_file(&state, &created, "partial.ply", b"ply\nformat").await;
    assert_eq!(status, StatusCode::OK);
    assert!(asset_dir(&state, &created).exists());

    sqlx::query("UPDATE ion_uploads SET expires_at = ?")
        .bind(chrono::Utc::now().timestamp() - 1)
        .execute(&state.db.pool)
        .await
        .unwrap();
    // the worker the serve path starts
    let worker = Arc::clone(&state.scheduler).start();
    let deadline = Instant::now() + Duration::from_secs(120);
    while ion_asset(&state, &created).await["status"] != "ERROR"
        || asset_dir(&state, &created).exists()
    {
        assert!(
            Instant::now() < deadline,
            "the scheduler never expired the upload"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    worker.abort();

    let (status, body) = put_file(&state, &created, "partial.ply", b"more").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("InvalidAccessKeyId"));
}

async fn queued_job(state: &Arc<AppState>, created: &Value) -> JobRecord {
    let (status, _) = put_file(state, created, "statue.glb", b"read when the job runs").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(state, created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let mut jobs = state
        .db
        .list_jobs_for_asset(created_asset_id(created))
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1);
    jobs.remove(0)
}

#[tokio::test]
async fn a_model_job_carries_the_options_of_the_create_call() {
    let state = test_state().await;

    let (status, created) = create(
        &state,
        json!({
            "name": "statue",
            "type": "3DTILES",
            "options": {
                "sourceType": "3D_MODEL",
                "position": [7.4, 43.7, 0.0],
                "geometryCompression": "MESHOPT",
                "textureFormat": "KTX2",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let job = queued_job(&state, &created).await;
    assert_eq!(job.placement.longitude, Some(7.4));
    assert_eq!(job.placement.latitude, Some(43.7));
    assert_eq!(job.mesh_compression, MeshCompression::Meshopt);
    assert_eq!(job.texture_compression, TextureCompression::Ktx2);
    assert!(job.input_path.ends_with("input/statue.glb"), "{job:?}");

    // ion's spec has DRACO and AUTO as the defaults
    let (status, created) = create(
        &state,
        json!({
            "name": "capture",
            "type": "3DTILES",
            "options": { "sourceType": "3D_CAPTURE", "position": [7.4, 43.7, 0.0] },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let job = queued_job(&state, &created).await;
    assert_eq!(job.mesh_compression, MeshCompression::Draco);
    assert_eq!(job.texture_compression, TextureCompression::None);
}

#[tokio::test]
async fn a_model_tiles_at_the_position_and_compressions_of_the_create_call() {
    let state = test_state().await;
    let (status, created) = create(
        &state,
        json!({
            "name": "statue",
            "type": "3DTILES",
            "options": {
                "sourceType": "3D_MODEL",
                "position": [7.4, 43.7, 0.0],
                "geometryCompression": "MESHOPT",
                "textureFormat": "KTX2",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let texture = texture_png();
    let zip = zip_of(&[
        ("statue.obj", TEXTURED_OBJ.as_bytes()),
        ("statue.mtl", MATERIALS.as_bytes()),
        ("wall.png", texture.as_slice()),
    ]);
    let (status, _) = put_file(&state, &created, "statue.zip", &zip).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");

    let tileset_dir = asset_dir(&state, &created);
    let tileset: Value =
        serde_json::from_slice(&std::fs::read(tileset_dir.join("tileset.json")).unwrap()).unwrap();
    let transform: Vec<f64> = serde_json::from_value(tileset["root"]["transform"].clone()).unwrap();
    let expected =
        tiletopia_core::spatial::enu_to_ecef_matrix(43.7f64.to_radians(), 7.4f64.to_radians(), 0.0);
    assert_eq!(transform.len(), expected.len());
    for (index, (written, wanted)) in transform.iter().zip(expected).enumerate() {
        assert!(
            (written - wanted).abs() < 1e-9,
            "element {index}: {written} != {wanted}"
        );
    }

    let root_tile = std::fs::read(tileset_dir.join("tiles/root.glb")).unwrap();
    let glb = gltf::Glb::from_slice(&root_tile).unwrap();
    let document: Value = serde_json::from_slice(&glb.json).unwrap();
    assert_eq!(
        document["extensionsRequired"],
        json!([
            "KHR_texture_basisu",
            "KHR_mesh_quantization",
            "EXT_meshopt_compression"
        ]),
        "{document}"
    );
    assert_eq!(document["images"][0]["mimeType"], "image/ktx2");
}

#[tokio::test]
async fn a_tiling_asset_reports_in_progress_at_its_running_jobs_progress() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let (status, _) = put_file(&state, &created, "cloud.ply", PLY_FIXTURE.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let mut job = only_job(&state, &created).await;
    job.status = tiletopia_server::db::JobStatus::Running;
    job.progress = 0.25;
    state.db.update_job(&job).await.unwrap();
    let mut asset = state
        .db
        .get_asset(created_asset_id(&created))
        .await
        .unwrap()
        .unwrap();
    asset.status = AssetStatus::Tiling;
    state.db.update_asset(&asset).await.unwrap();

    let reported = ion_asset(&state, &created).await;
    assert_eq!(reported["status"], "IN_PROGRESS");
    assert_eq!(reported["percentComplete"], 25);

    let request = Request::builder()
        .uri("/v1/assets")
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    let listing: Value = serde_json::from_slice(&bytes).unwrap();
    let listed = listing["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == created["assetMetadata"]["id"])
        .unwrap();
    assert_eq!(listed["status"], "IN_PROGRESS");
    assert_eq!(listed["percentComplete"], 25);
}

#[tokio::test]
async fn a_file_the_source_type_does_not_take_fails_the_asset() {
    let state = test_state().await;
    let created = created(&state, "IMAGERY", "RASTER_IMAGERY").await;
    let (status, _) = put_file(&state, &created, "ortho.jpg", b"jpeg").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("ortho.jpg") && message.contains("tif"),
        "{message}"
    );

    assert_eq!(ion_asset(&state, &created).await["status"], "ERROR");
    assert!(!asset_dir(&state, &created).exists());
    let (status, body) = put_file(&state, &created, "ortho.tif", GDAL_COG).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("InvalidAccessKeyId"));
}

#[tokio::test]
async fn two_source_files_fail_the_asset_naming_both() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    for file_name in ["east.ply", "west.ply"] {
        let (status, _) = put_file(&state, &created, file_name, PLY_FIXTURE.as_bytes()).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("east.ply") && message.contains("west.ply"),
        "{message}"
    );
    assert_eq!(ion_asset(&state, &created).await["status"], "ERROR");
}

#[tokio::test]
async fn another_editor_cannot_complete_the_upload() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let (status, _) = put_file(&state, &created, "cloud.ply", PLY_FIXTURE.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);

    let request = Request::builder()
        .method("POST")
        .uri(url_path(created["onComplete"]["url"].as_str().unwrap()))
        .header(
            "authorization",
            format!("Bearer {}", token("ion-upload-stranger", "editor")),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        ion_asset(&state, &created).await["status"],
        "AWAITING_FILES"
    );
}

#[tokio::test]
async fn only_an_uploading_asset_nothing_can_finish_is_failed() {
    let state = test_state().await;
    let stranded = Asset {
        id: Uuid::new_v4(),
        name: "stranded".into(),
        asset_type: AssetType::PointCloud,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: String::new(),
        tags: Vec::new(),
        owner_id: None,
    };
    state.db.create_asset(&stranded).await.unwrap();
    let awaiting_files = created(&state, "3DTILES", "POINT_CLOUD").await;
    let job_queued = created(&state, "3DTILES", "POINT_CLOUD").await;
    let (status, _) = put_file(&state, &job_queued, "cloud.ply", PLY_FIXTURE.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(&state, &job_queued).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_eq!(state.db.fail_stranded_uploads().await.unwrap(), 1);

    let stored = state.db.get_asset(stranded.id).await.unwrap().unwrap();
    assert!(matches!(stored.status, AssetStatus::Error));
    assert_eq!(
        ion_asset(&state, &awaiting_files).await["status"],
        "AWAITING_FILES"
    );
    assert_eq!(
        ion_asset(&state, &job_queued).await["status"],
        "NOT_STARTED"
    );
}

#[tokio::test]
async fn a_multipart_upload_joins_its_parts_in_part_number_order() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let secret = location(&created, "secretAccessKey");

    let request = s3_request(
        &created,
        secret,
        "POST",
        "cloud.ply",
        "uploads=",
        &sha256_hex(b""),
    )
    .body(Body::empty())
    .unwrap();
    let (status, body) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    let xml = String::from_utf8(body).unwrap();
    let upload_id = element_text(&xml, "UploadId").to_string();

    let (first, second) = PLY_FIXTURE.as_bytes().split_at(120);
    let mut etags = Vec::new();
    for (part_number, contents) in [(2, second), (1, first)] {
        let request = s3_request(
            &created,
            secret,
            "PUT",
            "cloud.ply",
            &format!("partNumber={part_number}&uploadId={upload_id}"),
            &sha256_hex(contents),
        )
        .body(Body::from(contents.to_vec()))
        .unwrap();
        let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()["etag"].to_str().unwrap().to_string();
        etags.push((part_number, etag));
    }
    etags.sort();

    let parts: String = etags
        .iter()
        .map(|(part_number, etag)| {
            format!("<Part><ETag>{etag}</ETag><PartNumber>{part_number}</PartNumber></Part>")
        })
        .collect();
    let complete = format!(
        "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">{parts}</CompleteMultipartUpload>"
    );
    let request = s3_request(
        &created,
        secret,
        "POST",
        "cloud.ply",
        &format!("uploadId={upload_id}"),
        &sha256_hex(complete.as_bytes()),
    )
    .body(Body::from(complete))
    .unwrap();
    let (status, body) = send(&state, request).await;
    let xml = String::from_utf8(body).unwrap();
    assert_eq!(status, StatusCode::OK, "{xml}");
    assert!(xml.contains("<CompleteMultipartUploadResult"), "{xml}");
    assert_eq!(
        element_text(&xml, "Location"),
        format!(
            "{}/{}/{}cloud.ply",
            common::TEST_ION_BASE_URL,
            location(&created, "bucket"),
            location(&created, "prefix")
        )
    );

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let input = std::fs::read(asset_dir(&state, &created).join("input/cloud.ply")).unwrap();
    assert_eq!(input, PLY_FIXTURE.as_bytes());
}

fn fill_staging_to_within(
    state: &AppState,
    created: &Value,
    free_bytes: u64,
) -> std::path::PathBuf {
    let staging = asset_dir(state, created).join("ion_upload");
    std::fs::create_dir_all(&staging).unwrap();
    let first_file = std::fs::File::create(staging.join("first.ply")).unwrap();
    // set_len leaves a sparse file, which reaches the cap without writing 4 GiB
    first_file
        .set_len(MAX_UPLOAD_BYTES as u64 - free_bytes)
        .unwrap();
    staging
}

#[tokio::test]
async fn a_second_file_past_the_credentials_cap_is_refused_and_stores_nothing() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let staging = fill_staging_to_within(&state, &created, 10);

    let (status, body) = put_file(&state, &created, "second.ply", &[0u8; 11]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("<Code>EntityTooLarge</Code>"));
    assert!(!staging.join("second.ply").exists());
    let parts = asset_dir(&state, &created).join("ion_upload_parts");
    assert_eq!(std::fs::read_dir(parts).unwrap().count(), 0);

    let (status, _) = put_file(&state, &created, "second.ply", &[0u8; 10]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(staging.join("second.ply").exists());

    let (status, body) = put_file(&state, &created, "third.ply", &[0u8; 1]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("<Code>EntityTooLarge</Code>"));
    assert!(!staging.join("third.ply").exists());
}

async fn put_part(
    state: &Arc<AppState>,
    created: &Value,
    upload_id: &str,
    part_number: u32,
    contents: &[u8],
) -> StatusCode {
    let request = s3_request(
        created,
        location(created, "secretAccessKey"),
        "PUT",
        "cloud.ply",
        &format!("partNumber={part_number}&uploadId={upload_id}"),
        &sha256_hex(contents),
    )
    .body(Body::from(contents.to_vec()))
    .unwrap();
    send(state, request).await.0
}

#[tokio::test]
async fn a_retried_put_or_part_counts_once_against_the_credentials_cap() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let staging = fill_staging_to_within(&state, &created, 100);
    let secret = location(&created, "secretAccessKey");

    for contents in [[1u8; 80], [2u8; 80]] {
        let (status, _) = put_file(&state, &created, "a.ply", &contents).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(std::fs::read(staging.join("a.ply")).unwrap(), [2u8; 80]);

    let request = s3_request(
        &created,
        secret,
        "POST",
        "cloud.ply",
        "uploads=",
        &sha256_hex(b""),
    )
    .body(Body::empty())
    .unwrap();
    let (status, body) = send(&state, request).await;
    assert_eq!(status, StatusCode::OK);
    let xml = String::from_utf8(body).unwrap();
    let upload_id = element_text(&xml, "UploadId");

    let last_part_one = [4u8; 15];
    assert_eq!(
        put_part(&state, &created, upload_id, 1, &[3u8; 15]).await,
        StatusCode::OK
    );
    assert_eq!(
        put_part(&state, &created, upload_id, 1, &last_part_one).await,
        StatusCode::OK
    );
    assert_eq!(
        put_part(&state, &created, upload_id, 2, &[5u8; 6]).await,
        StatusCode::BAD_REQUEST
    );
    let part_two = [5u8; 5];
    assert_eq!(
        put_part(&state, &created, upload_id, 2, &part_two).await,
        StatusCode::OK
    );

    let complete = format!(
        "<CompleteMultipartUpload><Part><ETag>{}</ETag><PartNumber>1</PartNumber></Part><Part><ETag>{}</ETag><PartNumber>2</PartNumber></Part></CompleteMultipartUpload>",
        &sha256_hex(&last_part_one)[..32],
        &sha256_hex(&part_two)[..32]
    );
    let request = s3_request(
        &created,
        secret,
        "POST",
        "cloud.ply",
        &format!("uploadId={upload_id}"),
        &sha256_hex(complete.as_bytes()),
    )
    .body(Body::from(complete))
    .unwrap();
    assert_eq!(send(&state, request).await.0, StatusCode::OK);
    assert_eq!(
        std::fs::metadata(staging.join("cloud.ply")).unwrap().len(),
        20
    );

    let (status, _) = put_file(&state, &created, "tiny.ply", &[0u8; 1]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_cut_off_incoming_file_is_deleted_by_the_first_request_after_a_restart() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let parts = asset_dir(&state, &created).join("ion_upload_parts");
    std::fs::create_dir_all(&parts).unwrap();
    let cut_off = parts.join("cut-off.incoming");
    std::fs::write(&cut_off, [0u8; 40]).unwrap();

    let (status, _) = put_file(&state, &created, "cloud.ply", PLY_FIXTURE.as_bytes()).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!cut_off.exists());
}

#[tokio::test]
async fn an_owner_at_the_cap_of_assets_awaiting_files_is_refused_until_one_expires() {
    let state = test_state().await;
    let source =
        json!({ "name": "survey", "type": "3DTILES", "options": { "sourceType": "POINT_CLOUD" } });
    for _ in 0..MOST_ASSETS_AWAITING_FILES_PER_OWNER {
        created(&state, "3DTILES", "POINT_CLOUD").await;
    }

    let (status, body) = create(&state, source.clone()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("awaiting files"),
        "{body}"
    );
    assert_eq!(
        state.db.list_assets().await.unwrap().len(),
        MOST_ASSETS_AWAITING_FILES_PER_OWNER as usize
    );

    let another_editor = Request::builder()
        .method("POST")
        .uri("/v1/assets")
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", token("ion-upload-other-owner", "editor")),
        )
        .body(Body::from(source.to_string()))
        .unwrap();
    assert_eq!(send(&state, another_editor).await.0, StatusCode::OK);

    sqlx::query("UPDATE ion_uploads SET expires_at = ?")
        .bind(chrono::Utc::now().timestamp() - 1)
        .execute(&state.db.pool)
        .await
        .unwrap();
    tiletopia_server::ion_upload::expire_abandoned_uploads(
        &state.db,
        &state.data_dir,
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    let (status, body) = create(&state, source).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_refusal_of_many_source_files_names_some_and_counts_the_rest() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    for index in 0..25 {
        let (status, _) = put_file(&state, &created, &format!("{index:02}.ply"), b"").await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = upload_complete(&state, &created).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("25 source files"), "{message}");
    assert!(message.contains("19.ply, and 5 more"), "{message}");
    assert!(!message.contains("20.ply"), "{message}");
}

const TEXTURED_OBJ: &str = "mtllib statue.mtl\nusemtl wall\n\
v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\n\
vt 0 0\nvt 1 0\nvt 0 1\nvt 1 1\n\
f 1/1 2/2 3/3\nf 2/2 4/4 3/3\n";
const MATERIALS: &str = "newmtl wall\nKd 1 1 1\nmap_Kd wall.png\n";

async fn created_placed_model(state: &Arc<AppState>) -> Value {
    let (status, created) = create(
        state,
        json!({
            "name": "statue",
            "type": "3DTILES",
            "options": {
                "sourceType": "3D_MODEL",
                "position": [7.4, 43.7, 0.0],
                "geometryCompression": "NONE",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created
}

async fn only_job(state: &Arc<AppState>, created: &Value) -> JobRecord {
    let mut jobs = state
        .db
        .list_jobs_for_asset(created_asset_id(created))
        .await
        .unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    jobs.remove(0)
}

async fn assert_root_tile_has_a_texture(state: &Arc<AppState>, created: &Value) {
    let request = Request::builder()
        .uri(format!(
            "/api/v1/assets/{}/tiles/root.glb",
            created_asset_id(created)
        ))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, tile) = send(state, request).await;
    assert_eq!(status, StatusCode::OK);
    let glb = gltf::Glb::from_slice(&tile).unwrap();
    let document: Value = serde_json::from_slice(&glb.json).unwrap();
    assert!(document["images"][0]["bufferView"].is_u64(), "{document}");
}

async fn assert_refused_naming(state: &Arc<AppState>, created: &Value, zip: &[u8], named: &[&str]) {
    let (status, _) = put_file(state, created, "upload.zip", zip).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(state, created).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().unwrap();
    for name in named {
        assert!(message.contains(name), "{name} is not in {message}");
    }
    assert_eq!(ion_asset(state, created).await["status"], "ERROR");
    assert!(!asset_dir(state, created).exists());
}

#[tokio::test]
async fn an_obj_zip_with_materials_and_a_texture_reaches_a_ready_asset_without_nodeodm() {
    let state = test_state().await;
    assert!(!state.job_queue.reconstructs_photo_sets());
    let created = created_placed_model(&state).await;
    let texture = texture_png();
    let zip = zip_of(&[
        ("statue/statue.obj", TEXTURED_OBJ.as_bytes()),
        ("statue/statue.mtl", MATERIALS.as_bytes()),
        ("statue/wall.png", texture.as_slice()),
        ("__MACOSX/statue/._statue.obj", b"resource fork".as_slice()),
    ]);
    let (status, _) = put_file(&state, &created, "statue.zip", &zip).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let job = only_job(&state, &created).await;
    assert!(job.input_path.ends_with("input/statue.obj"), "{job:?}");

    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
    assert_root_tile_has_a_texture(&state, &created).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/assets/{}/tile",
            created_asset_id(&created)
        ))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let retile: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(retile["input_path"], job.input_path);
}

#[tokio::test]
async fn an_obj_with_its_materials_as_separate_files_reaches_a_ready_asset() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let texture = texture_png();
    for (file_name, contents) in [
        ("statue.obj", TEXTURED_OBJ.as_bytes()),
        ("statue.mtl", MATERIALS.as_bytes()),
        ("wall.png", texture.as_slice()),
    ] {
        let (status, _) = put_file(&state, &created, file_name, contents).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
    assert_root_tile_has_a_texture(&state, &created).await;
}

#[tokio::test]
async fn a_gltf_zip_finds_its_external_buffer() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let positions = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
    let normals = [[0.0f32, 0.0, 1.0]; 3];
    let buffer: Vec<u8> = positions
        .iter()
        .chain(&normals)
        .flatten()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let document = json!({
        "asset": { "version": "2.0" },
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0 }],
        "meshes": [{ "primitives": [{ "attributes": { "POSITION": 0, "NORMAL": 1 } }] }],
        "buffers": [{ "uri": "mesh.bin", "byteLength": buffer.len() }],
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": 36 },
            { "buffer": 0, "byteOffset": 36, "byteLength": 36 },
        ],
        "accessors": [
            { "bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3",
              "min": [0.0, 0.0, 0.0], "max": [1.0, 1.0, 0.0] },
            { "bufferView": 1, "componentType": 5126, "count": 3, "type": "VEC3" },
        ],
    })
    .to_string();
    let zip = zip_of(&[
        ("mesh.gltf", document.as_bytes()),
        ("mesh.bin", buffer.as_slice()),
    ]);
    let (status, _) = put_file(&state, &created, "mesh.zip", &zip).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
}

#[tokio::test]
async fn a_zip_entry_that_leaves_the_upload_is_refused() {
    let state = test_state().await;
    for entry_name in [
        "../escape.obj",
        "statue/../../escape.obj",
        "/etc/escape.obj",
        "..\\escape.obj",
    ] {
        let created = created_placed_model(&state).await;
        let zip = zip_of(&[(entry_name, TEXTURED_OBJ.as_bytes())]);
        assert_refused_naming(&state, &created, &zip, &[entry_name, "leaves"]).await;
        assert!(!state.data_dir.join("escape.obj").exists());
    }
}

#[tokio::test]
async fn a_zip_with_a_symbolic_link_is_refused() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_symlink(
            "statue.obj",
            "/etc/passwd",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    let zip = writer.finish().unwrap().into_inner();
    assert_refused_naming(&state, &created, &zip, &["statue.obj", "symbolic link"]).await;
}

#[tokio::test]
async fn a_zip_with_several_or_no_main_files_is_refused_naming_them() {
    let state = test_state().await;

    let created = created_placed_model(&state).await;
    let zip = zip_of(&[
        ("a/first.obj", TEXTURED_OBJ.as_bytes()),
        ("b/second.glb", b"glb".as_slice()),
        ("statue.mtl", MATERIALS.as_bytes()),
    ]);
    assert_refused_naming(&state, &created, &zip, &["a/first.obj", "b/second.glb"]).await;

    let created = created_placed_model(&state).await;
    let zip = zip_of(&[("statue.mtl", MATERIALS.as_bytes())]);
    assert_refused_naming(&state, &created, &zip, &["statue.mtl", "3D_MODEL"]).await;
}

#[tokio::test]
async fn a_zip_of_photos_is_no_3d_capture_source() {
    let state = test_state().await;
    let (status, created) = create(
        &state,
        json!({
            "name": "survey",
            "type": "3DTILES",
            "options": { "sourceType": "3D_CAPTURE", "position": [7.4, 43.7, 0.0] },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let zip = zip_of(&[
        ("IMG_0001.jpg", b"jpeg".as_slice()),
        ("IMG_0002.jpg", b"jpeg".as_slice()),
    ]);
    assert_refused_naming(&state, &created, &zip, &["IMG_0001.jpg", "3D_CAPTURE"]).await;
    assert!(
        state
            .db
            .list_jobs_for_asset(created_asset_id(&created))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_zip_that_unpacks_past_the_limit_is_refused() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let zeros = vec![0u8; 64 * 1024 * 1024];
    let zip = zip_of(&[("statue.obj", zeros.as_slice())]);
    assert_refused_naming(&state, &created, &zip, &["unpacks to more than"]).await;
}

#[tokio::test]
async fn a_zip_with_too_many_entries_is_refused() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let names: Vec<String> = (0..10_001).map(|index| format!("{index}.png")).collect();
    let entries: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), b"".as_slice()))
        .collect();
    assert_refused_naming(&state, &created, &zip_of(&entries), &["10001 entries"]).await;
}

#[tokio::test]
async fn a_zip_beside_other_files_is_refused() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let (status, _) = put_file(&state, &created, "statue.obj", TEXTURED_OBJ.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);
    let zip = zip_of(&[("statue.mtl", MATERIALS.as_bytes())]);
    assert_refused_naming(&state, &created, &zip, &["statue.obj", "upload.zip"]).await;
}

async fn created_placed_bim_model(state: &Arc<AppState>) -> Value {
    let (status, created) = create(
        state,
        json!({
            "name": "plan",
            "type": "3DTILES",
            "options": {
                "sourceType": "BIM_CAD",
                "position": [7.4, 43.7, 0.0],
                "geometryCompression": "NONE",
            },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created
}

#[tokio::test]
async fn an_ifczip_under_bim_cad_queues_its_single_ifc() {
    let state = test_state().await;
    let created = created_placed_bim_model(&state).await;
    let ifczip = zip_of(&[("nested/model.ifc", b"ISO-10303-21;".as_slice())]);
    let (status, _) = put_file(&state, &created, "site.ifczip", &ifczip).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        only_job(&state, &created).await.input_path,
        "input/model.ifc"
    );
}

fn dxf_with_insunits(unit_code: &str) -> String {
    format!(
        "0\nSECTION\n2\nHEADER\n9\n$INSUNITS\n70\n{unit_code}\n0\nENDSEC\n\
         0\nSECTION\n2\nENTITIES\n0\n3DFACE\n\
         10\n0\n20\n0\n30\n0\n11\n1000\n21\n0\n31\n0\n\
         12\n0\n22\n1000\n32\n0\n13\n0\n23\n1000\n33\n0\n\
         0\nENDSEC\n0\nEOF\n"
    )
}

#[tokio::test]
async fn a_dxf_that_names_its_unit_tiles_under_bim_cad() {
    const MILLIMETRES_INSUNITS: &str = "4";
    let state = test_state().await;
    let created = created_placed_bim_model(&state).await;
    let dxf = dxf_with_insunits(MILLIMETRES_INSUNITS);
    let (status, _) = put_file(&state, &created, "plan.dxf", dxf.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
}

#[tokio::test]
async fn a_unitless_dxf_is_refused_at_upload_complete_since_ion_has_no_units_option() {
    let state = test_state().await;
    let (status, body) = create(
        &state,
        json!({
            "name": "plan",
            "type": "3DTILES",
            "options": { "sourceType": "BIM_CAD", "units": "metres" },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("units"), "{message}");

    let created = created_placed_bim_model(&state).await;
    let dxf = dxf_with_insunits("0");
    let (status, _) = put_file(&state, &created, "plan.dxf", dxf.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("plan.dxf") && message.contains("$INSUNITS"),
        "{message}"
    );
    assert_eq!(ion_asset(&state, &created).await["status"], "ERROR");
}

const SPLAT_PLY_PROPERTIES: [&str; 14] = [
    "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1", "scale_2",
    "rot_0", "rot_1", "rot_2", "rot_3",
];

fn splat_ply() -> Vec<u8> {
    let mut bytes = format!(
        "ply\nformat binary_little_endian 1.0\nelement vertex 3\n{}end_header\n",
        SPLAT_PLY_PROPERTIES
            .map(|name| format!("property float {name}\n"))
            .concat()
    )
    .into_bytes();
    for splat in 0..3 {
        let offset = splat as f32;
        let row = [
            offset,
            2.0 * offset,
            -offset,
            0.1,
            0.2,
            0.3,
            1.0,
            -3.0,
            -3.0,
            -3.0,
            1.0,
            0.0,
            0.0,
            0.0,
        ];
        for value in row {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

async fn created_placed_point_cloud(state: &Arc<AppState>) -> Value {
    let (status, created) = create(
        state,
        json!({
            "name": "capture",
            "type": "3DTILES",
            "options": { "sourceType": "POINT_CLOUD", "position": [7.4, 43.7, 0.0] },
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    created
}

#[tokio::test]
async fn a_splat_ply_sent_with_a_position_tiles_at_that_position() {
    let state = test_state().await;
    let created = created_placed_point_cloud(&state).await;
    let (status, _) = put_file(&state, &created, "capture.ply", &splat_ply()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let job = only_job(&state, &created).await;
    assert_eq!(job.placement.longitude, Some(7.4));
    assert_eq!(job.placement.latitude, Some(43.7));

    let settled = poll_until_settled(&state, &created).await;
    assert_eq!(settled["status"], "COMPLETE", "{settled}");
}

#[tokio::test]
async fn a_splat_ply_without_a_position_is_refused_at_upload_complete() {
    let state = test_state().await;
    let created = created(&state, "3DTILES", "POINT_CLOUD").await;
    let (status, _) = put_file(&state, &created, "capture.ply", &splat_ply()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("capture.ply") && message.contains("options.position"),
        "{message}"
    );
    assert_eq!(ion_asset(&state, &created).await["status"], "ERROR");
    assert!(
        state
            .db
            .list_jobs_for_asset(created_asset_id(&created))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_position_on_a_point_cloud_that_is_no_splat_capture_is_refused() {
    let state = test_state().await;
    let created = created_placed_point_cloud(&state).await;
    let (status, _) = put_file(&state, &created, "cloud.ply", PLY_FIXTURE.as_bytes()).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = upload_complete(&state, &created).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("options.position"), "{message}");
    assert_eq!(ion_asset(&state, &created).await["status"], "ERROR");
}

#[tokio::test]
async fn a_re_tile_of_a_zip_upload_finds_its_main_file_after_the_data_directory_moves() {
    let state = test_state().await;
    let created = created_placed_model(&state).await;
    let zip = zip_of(&[
        ("statue/statue.obj", TEXTURED_OBJ.as_bytes()),
        ("statue/statue.mtl", MATERIALS.as_bytes()),
    ]);
    let (status, _) = put_file(&state, &created, "statue.zip", &zip).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = upload_complete(&state, &created).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        only_job(&state, &created).await.input_path,
        "input/statue.obj"
    );

    let asset_id = created_asset_id(&created);
    sqlx::query("UPDATE jobs SET input_path = ?")
        .bind(format!("/moved/away/{asset_id}/input/statue.obj"))
        .execute(&state.db.pool)
        .await
        .unwrap();

    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/assets/{asset_id}/tile"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let (status, bytes) = send(&state, request).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let re_tile: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(re_tile["input_path"], "input/statue.obj");
}
