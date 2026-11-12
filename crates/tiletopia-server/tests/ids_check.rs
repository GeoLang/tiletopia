mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{test_state, test_state_with_ids_check, token};
use serde_json::Value;
use tiletopia_server::db::{JobRecord, JobStatus, ModelPlacement};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, ids_check, router};
use tokio::sync::Semaphore;
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "ids-owner";
const STRANGER: &str = "ids-stranger";
const BOUNDARY: &str = "idsboundary";
const IDS_FILE_NAME: &str = "walls.ids";
const IDS_XML: &str = "<ids xmlns=\"http://standards.buildingsmart.org/IDS\"/>";
const REPORT: &str = r#"{"title": "Walls",  "status": false, "specifications": [{"name": "Fire rating", "status": false}], "total_checks": 2}"#;
const SIDECAR_ERROR: &str = "the IDS file is not valid IDS: no specification element";

struct Answer {
    status: StatusCode,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Answer {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Answer {
        status,
        bytes: bytes.to_vec(),
    }
}

fn ids_upload(file_name: &str) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(IDS_XML.as_bytes());
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn post_ids(state: &Arc<AppState>, asset_id: Uuid, subject: &str, role: &str) -> Answer {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/assets/{asset_id}/ids"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("authorization", format!("Bearer {}", token(subject, role)))
        .body(Body::from(ids_upload(IDS_FILE_NAME)))
        .unwrap();
    send(state, request).await
}

async fn get_ids(state: &Arc<AppState>, asset_id: Uuid, subject: &str) -> Answer {
    let request = Request::builder()
        .uri(format!("/v1/assets/{asset_id}/ids"))
        .header(
            "authorization",
            format!("Bearer {}", token(subject, "viewer")),
        )
        .body(Body::empty())
        .unwrap();
    send(state, request).await
}

async fn model_asset(state: &Arc<AppState>, source_file_name: &str) -> Uuid {
    let id = Uuid::new_v4();
    state
        .db
        .create_asset(&Asset {
            id,
            name: "model".to_string(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 1,
            size_bytes: 1,
            description: String::new(),
            tags: Vec::new(),
            owner_id: Some(OWNER.to_string()),
        })
        .await
        .unwrap();
    let input_path = format!("input/{source_file_name}");
    let asset_dir = state.data_dir.join(id.to_string());
    std::fs::create_dir_all(asset_dir.join("input")).unwrap();
    std::fs::write(asset_dir.join(&input_path), b"ISO-10303-21;").unwrap();
    state
        .db
        .create_job(&JobRecord {
            id: Uuid::new_v4(),
            asset_id: id,
            status: JobStatus::Done,
            progress: 1.0,
            input_path,
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement: ModelPlacement::default(),
            mesh_compression: Default::default(),
            texture_compression: Default::default(),
            photogrammetry_preset: Default::default(),
        })
        .await
        .unwrap();
    id
}

const SIDECAR_TOKEN: &str = "ids-check-test-token";
const TOKEN_REFUSAL: &str = r#"{"error": "the request has no matching bearer token"}"#;

struct MockSidecar {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    requests_arrived_by_each_answer: Arc<Mutex<Vec<usize>>>,
    gate: Arc<Semaphore>,
}

impl MockSidecar {
    fn release(&self) {
        self.gate.add_permits(1);
    }

    fn ids_check_sidecar(&self) -> ids_check::IdsCheckSidecar {
        ids_check::IdsCheckSidecar::new(self.url.clone(), SIDECAR_TOKEN.to_string())
    }
}

async fn mock_sidecar(status: StatusCode, body: &'static str) -> MockSidecar {
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let requests_arrived_by_each_answer: Arc<Mutex<Vec<usize>>> = Arc::default();
    let gate = Arc::new(Semaphore::new(0));
    let recorded = Arc::clone(&requests);
    let answered = Arc::clone(&requests_arrived_by_each_answer);
    let held = Arc::clone(&gate);
    let app = axum::Router::new().route(
        "/check",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, Json(request): Json<Value>| {
                let recorded = Arc::clone(&recorded);
                let answered = Arc::clone(&answered);
                let held = Arc::clone(&held);
                async move {
                    let expected = format!("Bearer {SIDECAR_TOKEN}");
                    let authorization = headers.get(header::AUTHORIZATION);
                    if authorization.and_then(|value| value.to_str().ok())
                        != Some(expected.as_str())
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(header::CONTENT_TYPE, "application/json")],
                            TOKEN_REFUSAL,
                        );
                    }
                    recorded.lock().unwrap().push(request);
                    let _open = held.acquire().await.unwrap();
                    let arrived = recorded.lock().unwrap().len();
                    answered.lock().unwrap().push(arrived);
                    (status, [(header::CONTENT_TYPE, "application/json")], body)
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    MockSidecar {
        url: format!("http://{address}"),
        requests,
        requests_arrived_by_each_answer,
        gate,
    }
}

const WAIT_LIMIT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

async fn first_request(sidecar: &MockSidecar) -> Value {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        if let Some(request) = sidecar.requests.lock().unwrap().first() {
            return request.clone();
        }
        assert!(Instant::now() < deadline, "the sidecar was never called");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn settled_check(state: &Arc<AppState>, asset_id: Uuid) -> Answer {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let answer = get_ids(state, asset_id, OWNER).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
        if answer.json()["status"] != "running" {
            return answer;
        }
        assert!(Instant::now() < deadline, "the check never settled");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn an_unset_sidecar_url_answers_503_naming_the_variable() {
    let state = test_state().await;
    let id = model_asset(&state, "model.ifc").await;

    let answer = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["code"], "IDS_CHECK_UNAVAILABLE");
    let message = answer.json()["message"].as_str().unwrap().to_string();
    assert!(message.contains("TILETOPIA_IDS_CHECK_URL"), "{message}");

    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.status, StatusCode::NOT_FOUND);
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn another_owners_asset_answers_404_and_a_viewer_role_cannot_start_a_check() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let stranger_post = post_ids(&state, id, STRANGER, "editor").await;
    assert_eq!(stranger_post.status, StatusCode::NOT_FOUND);
    assert_eq!(stranger_post.json()["code"], "ResourceNotFound");

    let stranger_get = get_ids(&state, id, STRANGER).await;
    assert_eq!(stranger_get.status, StatusCode::NOT_FOUND);
    assert_eq!(stranger_get.json()["code"], "ResourceNotFound");

    let owner_as_viewer = post_ids(&state, id, OWNER, "viewer").await;
    assert_eq!(owner_as_viewer.status, StatusCode::FORBIDDEN);

    assert!(sidecar.requests.lock().unwrap().is_empty());
    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn a_post_while_a_check_runs_answers_409() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let started = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    assert_eq!(started.json(), serde_json::json!({ "status": "running" }));
    first_request(&sidecar).await;

    let running = get_ids(&state, id, OWNER).await.json();
    assert_eq!(running["status"], "running");
    assert_eq!(running["idsFileName"], IDS_FILE_NAME);
    assert_eq!(running["checkedAt"], Value::Null);
    assert_eq!(running["error"], Value::Null);
    assert_eq!(running["report"], Value::Null);

    let second = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(second.status, StatusCode::CONFLICT);
    assert_eq!(second.json()["code"], "IDS_CHECK_RUNNING");

    sidecar.release();
    let settled = settled_check(&state, id).await.json();
    assert_eq!(settled["status"], "done");
    assert_eq!(sidecar.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_finished_check_answers_the_sidecar_report_verbatim() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let started = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    let settled = settled_check(&state, id).await;
    let check = settled.json();
    assert_eq!(check["status"], "done");
    assert_eq!(check["idsFileName"], IDS_FILE_NAME);
    assert_eq!(check["error"], Value::Null);
    let checked_at = check["checkedAt"].as_str().unwrap();
    chrono::DateTime::parse_from_rfc3339(checked_at).unwrap();
    let body = String::from_utf8(settled.bytes.clone()).unwrap();
    assert!(body.contains(REPORT), "{body}");
    assert_eq!(
        check["report"],
        serde_json::from_str::<Value>(REPORT).unwrap()
    );

    let request = first_request(&sidecar).await;
    assert_eq!(request["ifc_path"], format!("{id}/input/model.ifc"));
    let ids_path = request["ids_path"].as_str().unwrap();
    assert!(!ids_path.starts_with('/'), "{ids_path}");
    assert_eq!(
        std::fs::read_to_string(state.data_dir.join(ids_path)).unwrap(),
        IDS_XML
    );
}

#[tokio::test]
async fn a_refused_check_reads_failed_with_the_sidecars_error_and_can_be_sent_again() {
    let sidecar = mock_sidecar(
        StatusCode::UNPROCESSABLE_ENTITY,
        r#"{"error": "the IDS file is not valid IDS: no specification element"}"#,
    )
    .await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let started = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    let check = settled_check(&state, id).await.json();
    assert_eq!(check["status"], "failed");
    assert_eq!(check["error"], SIDECAR_ERROR);
    assert_eq!(check["report"], Value::Null);
    assert!(check["checkedAt"].is_string());

    let again = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.json());
}

#[tokio::test]
async fn a_non_ifc_asset_is_refused() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.obj").await;

    let answer = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(answer.json()["code"], "ASSET_NOT_IFC");

    assert!(sidecar.requests.lock().unwrap().is_empty());
    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn a_check_interrupted_by_a_restart_reads_failed() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let started = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    first_request(&sidecar).await;

    let failed = ids_check::fail_interrupted_checks(&state.db).await.unwrap();
    assert_eq!(failed, 1);
    let check = get_ids(&state, id, OWNER).await.json();
    assert_eq!(check["status"], "failed");
    assert!(check["error"].as_str().unwrap().contains("restarted"));
    assert!(check["checkedAt"].is_string());
    assert_eq!(check["report"], Value::Null);
}

#[tokio::test]
async fn deleting_the_asset_removes_the_ids_file_and_its_report() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    let started = post_ids(&state, id, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    assert_eq!(settled_check(&state, id).await.json()["status"], "done");
    let request = first_request(&sidecar).await;
    let ids_file = state.data_dir.join(request["ids_path"].as_str().unwrap());
    assert!(ids_file.is_file());

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/assets/{id}"))
        .header(
            "authorization",
            format!("Bearer {}", token(OWNER, "editor")),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, delete).await.status, StatusCode::NO_CONTENT);

    assert!(!ids_file.exists());
    assert!(!state.data_dir.join(id.to_string()).exists());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ids_checks WHERE asset_id = ?")
        .bind(id.to_string())
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn a_second_assets_check_reaches_the_sidecar_only_after_the_first_frees_its_slot() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let first = model_asset(&state, "first.ifc").await;
    let second = model_asset(&state, "second.ifc").await;

    let started = post_ids(&state, first, OWNER, "editor").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json());
    first_request(&sidecar).await;
    let check_slots = &state.ids_check.as_ref().unwrap().check_slots;
    assert_eq!(
        check_slots.available_permits(),
        0,
        "the running check holds no slot"
    );
    let queued = post_ids(&state, second, OWNER, "editor").await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{}", queued.json());

    sidecar.release();
    assert_eq!(settled_check(&state, first).await.json()["status"], "done");
    assert_eq!(settled_check(&state, second).await.json()["status"], "done");

    assert_eq!(
        *sidecar.requests_arrived_by_each_answer.lock().unwrap(),
        vec![1, 2]
    );
    let requests = sidecar.requests.lock().unwrap();
    assert_eq!(requests[0]["ifc_path"], format!("{first}/input/first.ifc"));
    assert_eq!(
        requests[1]["ifc_path"],
        format!("{second}/input/second.ifc")
    );
}

fn store_ids(state: &AppState, asset_id: Uuid, file_name: Option<&str>) {
    let ids_dir = state.data_dir.join(asset_id.to_string()).join("ids");
    std::fs::create_dir_all(&ids_dir).unwrap();
    std::fs::write(ids_dir.join("specification.ids"), IDS_XML).unwrap();
    if let Some(file_name) = file_name {
        std::fs::write(ids_dir.join("file-name"), file_name).unwrap();
    }
}

async fn request_count(sidecar: &MockSidecar, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let requests = sidecar.requests.lock().unwrap().clone();
        if requests.len() >= count {
            return requests;
        }
        assert!(
            Instant::now() < deadline,
            "the sidecar was called {count} times"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn counted(state: &AppState, query: &'static str) -> i64 {
    sqlx::query_scalar(query)
        .fetch_one(&state.db.pool)
        .await
        .unwrap()
}

fn asset_upload(file_name: &str, ids_name: Option<&str>, ids_body: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(b"ISO-10303-21;");
    if let Some(ids_name) = ids_name {
        body.extend_from_slice(
            format!(
                "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"ids\"; filename=\"{ids_name}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(ids_body);
    }
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn post_asset(state: &Arc<AppState>, body: Vec<u8>) -> Answer {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/assets")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(
            "authorization",
            format!("Bearer {}", token(OWNER, "editor")),
        )
        .body(Body::from(body))
        .unwrap();
    send(state, request).await
}

#[tokio::test]
async fn a_ready_ifc_with_a_stored_ids_file_starts_the_sidecar() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;
    store_ids(&state, id, Some(IDS_FILE_NAME));

    ids_check::check_ready_asset(&state.db, &state.data_dir, state.ids_check.clone(), id).await;
    ids_check::check_ready_asset(&state.db, &state.data_dir, state.ids_check.clone(), id).await;

    let request = first_request(&sidecar).await;
    assert_eq!(request["ifc_path"], format!("{id}/input/model.ifc"));
    assert_eq!(request["ids_path"], format!("{id}/ids/specification.ids"));
    assert_eq!(sidecar.requests.lock().unwrap().len(), 1);
    sidecar.release();
    let check = settled_check(&state, id).await.json();
    assert_eq!(check["status"], "done");
    assert_eq!(check["idsFileName"], IDS_FILE_NAME);
}

#[tokio::test]
async fn a_new_version_copies_the_previous_ids_file_and_starts_the_check() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let previous = model_asset(&state, "previous.ifc").await;
    store_ids(&state, previous, None);
    sqlx::query(
        "INSERT INTO ids_checks (asset_id, ids_file_name, status, checked_at) VALUES (?, ?, ?, ?)",
    )
    .bind(previous.to_string())
    .bind(IDS_FILE_NAME)
    .bind("done")
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&state.db.pool)
    .await
    .unwrap();
    let next = model_asset(&state, "next.ifc").await;
    tiletopia_server::model_versions::add_version(&state.db, next, previous)
        .await
        .unwrap();

    ids_check::check_ready_asset(&state.db, &state.data_dir, state.ids_check.clone(), next).await;

    let copied = state.data_dir.join(next.to_string()).join("ids");
    assert_eq!(
        std::fs::read_to_string(copied.join("specification.ids")).unwrap(),
        IDS_XML
    );
    assert_eq!(
        std::fs::read_to_string(copied.join("file-name")).unwrap(),
        IDS_FILE_NAME
    );
    assert!(
        state
            .data_dir
            .join(previous.to_string())
            .join("ids/specification.ids")
            .is_file()
    );
    let requests = request_count(&sidecar, 1).await;
    assert_eq!(requests[0]["ifc_path"], format!("{next}/input/next.ifc"));
    assert_eq!(
        requests[0]["ids_path"],
        format!("{next}/ids/specification.ids")
    );
    let check = settled_check(&state, next).await.json();
    assert_eq!(check["status"], "done");
    assert_eq!(check["idsFileName"], IDS_FILE_NAME);
}

#[tokio::test]
async fn a_ready_ifc_with_no_ids_file_does_not_call_the_sidecar() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.ifc").await;

    ids_check::check_ready_asset(&state.db, &state.data_dir, state.ids_check.clone(), id).await;

    assert!(sidecar.requests.lock().unwrap().is_empty());
    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn a_ready_non_ifc_with_an_ids_file_does_not_start_a_check() {
    let sidecar = mock_sidecar(StatusCode::OK, REPORT).await;
    sidecar.release();
    let state = test_state_with_ids_check(sidecar.ids_check_sidecar()).await;
    let id = model_asset(&state, "model.obj").await;
    store_ids(&state, id, Some(IDS_FILE_NAME));

    ids_check::check_ready_asset(&state.db, &state.data_dir, state.ids_check.clone(), id).await;

    assert!(sidecar.requests.lock().unwrap().is_empty());
    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn an_ids_file_with_no_sidecar_is_stored_as_a_failed_check() {
    let state = test_state().await;
    let id = model_asset(&state, "model.ifc").await;
    store_ids(&state, id, Some(IDS_FILE_NAME));

    ids_check::check_ready_asset(&state.db, &state.data_dir, None, id).await;

    let check = get_ids(&state, id, OWNER).await;
    assert_eq!(check.status, StatusCode::OK, "{}", check.json());
    assert_eq!(check.json()["status"], "failed");
    assert_eq!(check.json()["idsFileName"], IDS_FILE_NAME);
    let error = check.json()["error"].as_str().unwrap().to_string();
    assert!(error.contains("TILETOPIA_IDS_CHECK_URL"), "{error}");
}

fn ids_directories(state: &AppState) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&state.data_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.join("ids").is_dir() {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn an_ids_field_without_a_model_file_answers_400_and_leaves_no_directory() {
    let state = test_state().await;
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"ids\"; filename=\"{IDS_FILE_NAME}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(IDS_XML.as_bytes());
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let answer = post_asset(&state, body).await;

    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let text = String::from_utf8(answer.bytes).unwrap();
    assert!(text.contains("no file field"), "{text}");
    assert!(ids_directories(&state).is_empty());
}

#[tokio::test]
async fn an_upload_ids_field_that_is_not_an_ids_file_answers_400_before_a_job() {
    let state = test_state().await;

    let answer = post_asset(
        &state,
        asset_upload("model.ifc", Some("notes.txt"), b"not ids"),
    )
    .await;

    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let text = String::from_utf8(answer.bytes).unwrap();
    assert!(text.contains("notes.txt"), "{text}");
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM jobs").await, 0);
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM assets").await, 0);
    assert!(ids_directories(&state).is_empty());
}

#[tokio::test]
async fn an_upload_stores_an_ids_file_and_does_not_start_a_check() {
    let state = test_state().await;

    let answer = post_asset(
        &state,
        asset_upload("model.ifc", Some(IDS_FILE_NAME), IDS_XML.as_bytes()),
    )
    .await;

    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
    let uploaded = answer.json();
    let id = uploaded["id"].as_str().unwrap();
    let ids_dir = state.data_dir.join(id).join("ids");
    assert_eq!(
        std::fs::read_to_string(ids_dir.join("specification.ids")).unwrap(),
        IDS_XML
    );
    assert_eq!(
        std::fs::read_to_string(ids_dir.join("file-name")).unwrap(),
        IDS_FILE_NAME
    );
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM ids_checks").await, 0);
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM jobs").await, 1);

    let check = get_ids(&state, Uuid::parse_str(id).unwrap(), OWNER).await;
    assert_eq!(check.json()["code"], "IDS_NOT_CHECKED");
}

#[tokio::test]
async fn an_upload_ids_field_longer_than_16_mib_answers_400_before_a_job() {
    let state = test_state().await;
    let ids_body = vec![b'x'; 16 * 1024 * 1024 + 1];

    let answer = post_asset(
        &state,
        asset_upload("model.ifc", Some(IDS_FILE_NAME), &ids_body),
    )
    .await;

    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let text = String::from_utf8(answer.bytes).unwrap();
    assert!(text.contains("16 MiB"), "{text}");
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM jobs").await, 0);
    assert_eq!(counted(&state, "SELECT COUNT(*) FROM assets").await, 0);
    assert!(ids_directories(&state).is_empty());
}
