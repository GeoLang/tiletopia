use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{DefaultBodyLimit, Multipart, Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::value::RawValue;
use sqlx::Row;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::bcf::remove_dir_if_present;
use crate::db::Database;
use crate::elements::{element_error, permitted_asset, server_failure};
use crate::ion_compat::IonError;
use crate::job_queue::job_with_input_in_asset_dir;
use crate::upload::{lowercase_extension, write_field};
use crate::{AppState, Asset, AssetStatus, may_modify_asset, may_view_asset, scratch_files, users};

pub const IDS_CHECK_URL_ENV: &str = "TILETOPIA_IDS_CHECK_URL";
pub const IDS_CHECK_TOKEN_ENV: &str = "TILETOPIA_IDS_CHECK_TOKEN";

const IDS_DIR: &str = "ids";
const STORED_IDS_FILE: &str = "specification.ids";
const REPORT_FILE: &str = "report.json";
const UPLOAD_FILE: &str = "upload.ids";

const MOST_IDS_UPLOAD_BYTES: usize = 16 * 1024 * 1024;
const MOST_CHECK_DURATION: Duration = Duration::from_secs(60 * 60);
const SIDECAR_BODY_CHARACTERS_IN_ERROR: usize = 500;

// ifcopenshell.open holds the whole model in the sidecar's memory for the length of a check
const CONCURRENT_CHECKS: usize = 1;

#[derive(Clone)]
pub struct IdsCheckSidecar {
    url: String,
    token: String,
    pub check_slots: Arc<Semaphore>,
}

impl IdsCheckSidecar {
    pub fn new(url: String, token: String) -> Self {
        Self {
            url,
            token,
            check_slots: Arc::new(Semaphore::new(CONCURRENT_CHECKS)),
        }
    }
}

pub fn sidecar_from_env() -> Result<Option<IdsCheckSidecar>, String> {
    let raw = match std::env::var(IDS_CHECK_URL_ENV) {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{IDS_CHECK_URL_ENV} is not valid unicode"));
        }
    };
    let url = raw.trim();
    reqwest::Url::parse(url).map_err(|error| format!("{IDS_CHECK_URL_ENV}={raw}: {error}"))?;
    let token = std::env::var(IDS_CHECK_TOKEN_ENV).map_err(|error| {
        format!("{IDS_CHECK_URL_ENV} is set, so {IDS_CHECK_TOKEN_ENV} must be too: {error}")
    })?;
    let token = token.trim();
    if token.is_empty() {
        return Err(format!(
            "{IDS_CHECK_URL_ENV} is set, so {IDS_CHECK_TOKEN_ENV} must not be empty"
        ));
    }
    Ok(Some(IdsCheckSidecar::new(
        url.trim_end_matches('/').to_string(),
        token.to_string(),
    )))
}

pub fn ids_check_routes() -> Router<Arc<AppState>> {
    let write_routes = Router::new()
        .route(
            "/v1/assets/{id}/ids",
            post(start_check).layer(DefaultBodyLimit::max(MOST_IDS_UPLOAD_BYTES)),
        )
        .layer(middleware::from_fn(users::require_editor));
    Router::new()
        .route("/v1/assets/{id}/ids", get(get_check))
        .merge(write_routes)
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Running,
    Done,
    Failed,
}

impl CheckStatus {
    fn stored_name(self) -> &'static str {
        match self {
            CheckStatus::Running => "running",
            CheckStatus::Done => "done",
            CheckStatus::Failed => "failed",
        }
    }

    fn from_stored_name(name: &str) -> Result<Self, sqlx::Error> {
        match name {
            "running" => Ok(CheckStatus::Running),
            "done" => Ok(CheckStatus::Done),
            "failed" => Ok(CheckStatus::Failed),
            other => Err(sqlx::Error::Decode(
                format!("{other} is not an IDS check status").into(),
            )),
        }
    }
}

#[derive(Debug, Serialize)]
struct CheckRequest {
    ifc_path: String,
    ids_path: String,
}

#[derive(Debug, Deserialize)]
struct SidecarRefusal {
    error: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IdsCheckAnswer {
    ids_file_name: String,
    status: CheckStatus,
    checked_at: Option<String>,
    error: Option<String>,
    report: Option<Box<RawValue>>,
}

fn ids_refusal(message: impl Into<String>) -> IonError {
    element_error(StatusCode::BAD_REQUEST, "INVALID_IDS", message)
}

fn read_failure(error: impl std::fmt::Display) -> IonError {
    server_failure("the IDS check could not be read", error)
}

fn start_failure(error: impl std::fmt::Display) -> IonError {
    server_failure("the IDS check could not be started", error)
}

fn report_path(data_dir: &Path, asset_id: Uuid) -> PathBuf {
    data_dir
        .join(asset_id.to_string())
        .join(IDS_DIR)
        .join(REPORT_FILE)
}

pub async fn delete_check_of_asset(db: &Database, asset_id: Uuid) -> Result<(), String> {
    sqlx::query("DELETE FROM ids_checks WHERE asset_id = ?")
        .bind(asset_id.to_string())
        .execute(&db.pool)
        .await
        .map_err(|error| format!("deleting the IDS check of {asset_id} failed: {error}"))?;
    Ok(())
}

pub async fn fail_interrupted_checks(db: &Database) -> Result<u64, String> {
    let result = sqlx::query(
        "UPDATE ids_checks SET status = ?, checked_at = ?, \
         error = 'the server restarted during this check, send the IDS file again' \
         WHERE status = ?",
    )
    .bind(CheckStatus::Failed.stored_name())
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(CheckStatus::Running.stored_name())
    .execute(&db.pool)
    .await
    .map_err(|error| format!("failing the interrupted IDS checks failed: {error}"))?;
    Ok(result.rows_affected())
}

async fn ifc_source_in_data_dir(
    state: &AppState,
    asset: &Asset,
    id: &str,
) -> Result<String, IonError> {
    if !matches!(asset.status, AssetStatus::Ready) {
        return Err(element_error(
            StatusCode::CONFLICT,
            "ASSET_NOT_READY",
            format!("asset {id} has status {:?}", asset.status),
        ));
    }
    let no_source =
        |message: String| element_error(StatusCode::NOT_FOUND, "ASSET_HAS_NO_IFC_SOURCE", message);
    let job = state
        .db
        .list_jobs_for_asset(asset.id)
        .await
        .map_err(start_failure)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            no_source(format!(
                "asset {id} was never tiled, so it has no source file to check"
            ))
        })?;
    let job = job_with_input_in_asset_dir(&job, &state.data_dir.join(asset.id.to_string()));
    let source = PathBuf::from(job.input_path);
    let extension = lowercase_extension(&source);
    if !matches!(extension.as_str(), "ifc" | "ifczip") {
        return Err(element_error(
            StatusCode::CONFLICT,
            "ASSET_NOT_IFC",
            format!(
                "asset {id}'s source is a .{extension}, and only an IFC model can be checked against an IDS file"
            ),
        ));
    }
    if !source.is_file() {
        return Err(no_source(format!("asset {id}'s source file is gone")));
    }
    let relative = source.strip_prefix(&state.data_dir).map_err(|_| {
        start_failure(format!(
            "{} is outside the data directory {}",
            source.display(),
            state.data_dir.display()
        ))
    })?;
    // the ids-check sidecar is a linux container whatever this server runs on
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

async fn receive_ids_file(multipart: &mut Multipart, upload: &Path) -> Result<String, IonError> {
    let mut ids_file_name = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| ids_refusal("malformed multipart body"))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field
            .file_name()
            .and_then(|name| Path::new(name).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| ids_refusal("the file field has no file name"))?;
        if lowercase_extension(Path::new(&file_name)) != "ids" {
            return Err(ids_refusal(format!("{file_name} is not an .ids file")));
        }
        write_field(&mut field, upload)
            .await
            .map_err(|(status, message)| element_error(status, "INVALID_IDS", message))?;
        ids_file_name = Some(file_name);
    }
    ids_file_name.ok_or_else(|| ids_refusal("no file field in the upload"))
}

async fn claim_check(db: &Database, asset_id: Uuid, ids_file_name: &str) -> Result<(), IonError> {
    let claimed = sqlx::query(
        "INSERT INTO ids_checks (asset_id, ids_file_name, status) VALUES (?, ?, ?) \
         ON CONFLICT (asset_id) DO UPDATE SET ids_file_name = excluded.ids_file_name, \
         status = excluded.status, checked_at = NULL, error = NULL \
         WHERE ids_checks.status != excluded.status",
    )
    .bind(asset_id.to_string())
    .bind(ids_file_name)
    .bind(CheckStatus::Running.stored_name())
    .execute(&db.pool)
    .await
    .map_err(start_failure)?;
    if claimed.rows_affected() == 0 {
        return Err(element_error(
            StatusCode::CONFLICT,
            "IDS_CHECK_RUNNING",
            format!("asset {asset_id} is already being checked against an IDS file"),
        ));
    }
    Ok(())
}

async fn store_ids_file(upload: &Path, asset_dir: &Path) -> std::io::Result<()> {
    let ids_dir = asset_dir.join(IDS_DIR);
    tokio::fs::create_dir_all(&ids_dir).await?;
    match tokio::fs::remove_file(ids_dir.join(REPORT_FILE)).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    tokio::fs::rename(upload, ids_dir.join(STORED_IDS_FILE)).await
}

async fn settle_check(
    db: &Database,
    asset_id: Uuid,
    outcome: &Result<(), String>,
) -> Result<(), sqlx::Error> {
    let (status, error) = match outcome {
        Ok(()) => (CheckStatus::Done, None),
        Err(reason) => (CheckStatus::Failed, Some(reason.as_str())),
    };
    sqlx::query("UPDATE ids_checks SET status = ?, checked_at = ?, error = ? WHERE asset_id = ?")
        .bind(status.stored_name())
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(error)
        .bind(asset_id.to_string())
        .execute(&db.pool)
        .await?;
    Ok(())
}

async fn start_check(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    let Some(sidecar) = state.ids_check.clone() else {
        return Err(element_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "IDS_CHECK_UNAVAILABLE",
            format!(
                "{IDS_CHECK_URL_ENV} is unset, so this server has no IDS check sidecar to call"
            ),
        ));
    };
    let ifc_path = ifc_source_in_data_dir(&state, &asset, &id).await?;

    let scratch = scratch_files::new_scratch_dir(&state.data_dir)
        .await
        .map_err(start_failure)?;
    let started = async {
        let upload = scratch.join(UPLOAD_FILE);
        let ids_file_name = receive_ids_file(&mut multipart, &upload).await?;
        claim_check(&state.db, asset.id, &ids_file_name).await?;
        let asset_dir = state.data_dir.join(asset.id.to_string());
        if let Err(error) = store_ids_file(&upload, &asset_dir).await {
            let outcome = Err(format!("storing the IDS file failed: {error}"));
            settle_check(&state.db, asset.id, &outcome)
                .await
                .map_err(start_failure)?;
            return Err(start_failure(error));
        }
        Ok::<(), IonError>(())
    }
    .await;
    if let Err(reason) = remove_dir_if_present(&scratch).await {
        tracing::warn!("{reason}");
    }
    started?;

    let request = CheckRequest {
        ifc_path,
        ids_path: format!("{}/{IDS_DIR}/{STORED_IDS_FILE}", asset.id),
    };
    tokio::spawn(run_check(Arc::clone(&state), asset.id, sidecar, request));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": CheckStatus::Running })),
    ))
}

async fn sidecar_report(
    sidecar: &IdsCheckSidecar,
    request: &CheckRequest,
) -> Result<Box<RawValue>, String> {
    // tokio's Semaphore hands out permits in the order they were asked for
    let _check_slot = sidecar
        .check_slots
        .acquire()
        .await
        .expect("the IDS check semaphore is never closed");
    let sidecar_url = &sidecar.url;
    let response = reqwest::Client::new()
        .post(format!("{sidecar_url}/check"))
        .bearer_auth(&sidecar.token)
        .json(request)
        .timeout(MOST_CHECK_DURATION)
        .send()
        .await
        .map_err(|error| {
            format!("the IDS check sidecar at {sidecar_url} could not be reached: {error}")
        })?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("reading the IDS check sidecar's answer failed: {error}"))?;
    if !status.is_success() {
        let refusal: SidecarRefusal = serde_json::from_slice(&body).map_err(|_| {
            let text: String = String::from_utf8_lossy(&body)
                .chars()
                .take(SIDECAR_BODY_CHARACTERS_IN_ERROR)
                .collect();
            format!("the IDS check sidecar answered {status} with no error field: {text}")
        })?;
        return Err(refusal.error);
    }
    serde_json::from_slice(&body).map_err(|error| {
        format!("the IDS check sidecar answered {status} with a body that is not JSON: {error}")
    })
}

async fn run_check(
    state: Arc<AppState>,
    asset_id: Uuid,
    sidecar: IdsCheckSidecar,
    request: CheckRequest,
) {
    let outcome = match sidecar_report(&sidecar, &request).await {
        // tokio::fs::write creates no missing directory, and deleting the asset removes the ids directory
        Ok(report) => tokio::fs::write(report_path(&state.data_dir, asset_id), report.get())
            .await
            .map_err(|error| format!("writing the IDS report failed: {error}")),
        Err(reason) => Err(reason),
    };
    if let Err(reason) = &outcome {
        tracing::warn!("IDS check of asset {asset_id} failed: {reason}");
    }
    if let Err(error) = settle_check(&state.db, asset_id, &outcome).await {
        tracing::error!(
            "IDS check of asset {asset_id}: it reads as running until the next restart, writing its outcome failed: {error}"
        );
    }
}

async fn get_check(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<IdsCheckAnswer>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let row = sqlx::query(
        "SELECT ids_file_name, status, checked_at, error FROM ids_checks WHERE asset_id = ?",
    )
    .bind(asset.id.to_string())
    .fetch_optional(&state.db.pool)
    .await
    .map_err(read_failure)?
    .ok_or_else(|| {
        element_error(
            StatusCode::NOT_FOUND,
            "IDS_NOT_CHECKED",
            format!("asset {id} has not been checked against an IDS file"),
        )
    })?;
    let status =
        CheckStatus::from_stored_name(&row.try_get::<String, _>("status").map_err(read_failure)?)
            .map_err(read_failure)?;
    let report = match status {
        CheckStatus::Done => {
            let text = tokio::fs::read_to_string(report_path(&state.data_dir, asset.id))
                .await
                .map_err(read_failure)?;
            Some(RawValue::from_string(text).map_err(read_failure)?)
        }
        CheckStatus::Running | CheckStatus::Failed => None,
    };
    Ok(Json(IdsCheckAnswer {
        ids_file_name: row.try_get("ids_file_name").map_err(read_failure)?,
        status,
        checked_at: row.try_get("checked_at").map_err(read_failure)?,
        error: row.try_get("error").map_err(read_failure)?,
        report,
    }))
}
