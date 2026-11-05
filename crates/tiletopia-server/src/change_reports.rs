use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::{
    Extension, Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteRow, SqliteSynchronous};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::audit::AuditedResource;
use crate::db::Database;
use crate::element_store::{
    ComparableElement, ElementsInGlobalIdOrder, Location, open_immutable_file, store_path,
};
use crate::elements::{
    bounded_integer, continuation_position, element_error, invalid_parameter, permitted_asset,
    server_failure,
};
use crate::ion_compat::IonError;
use crate::model_versions::version_of;
use crate::{AppState, Asset, may_modify_asset, may_view_asset, users};

const REPORTS_DIR: &str = "change_reports";
const DEFAULT_CHANGES_PER_PAGE: i64 = 100;
const MOST_CHANGES_PER_PAGE: i64 = 1000;

// about a millimetre on the ground
const LOCATION_TOLERANCE_DEGREES: f64 = 1e-8;
const LOCATION_TOLERANCE_METRES: f64 = 1e-3;

// a comparison holds two sqlite worker threads and reads both stores end to end
static COMPARISON_SLOTS: Semaphore = Semaphore::const_new(2);

const REPORT_COLUMNS: &str =
    "id, from_asset_id, to_asset_id, status, created_at, added, removed, changed, unchanged, error";

const CHANGE_KINDS: [&str; 3] = ["added", "removed", "changed"];

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReportStatus {
    NotStarted,
    InProgress,
    Complete,
    Error,
}

impl ReportStatus {
    fn stored_name(self) -> &'static str {
        match self {
            ReportStatus::NotStarted => "NOT_STARTED",
            ReportStatus::InProgress => "IN_PROGRESS",
            ReportStatus::Complete => "COMPLETE",
            ReportStatus::Error => "ERROR",
        }
    }

    fn from_stored_name(name: &str) -> Result<Self, sqlx::Error> {
        match name {
            "NOT_STARTED" => Ok(ReportStatus::NotStarted),
            "IN_PROGRESS" => Ok(ReportStatus::InProgress),
            "COMPLETE" => Ok(ReportStatus::Complete),
            "ERROR" => Ok(ReportStatus::Error),
            other => Err(sqlx::Error::Decode(
                format!("{other} is not a change report status").into(),
            )),
        }
    }

    fn percent_complete(self) -> Option<u8> {
        match self {
            ReportStatus::NotStarted | ReportStatus::Error => Some(0),
            ReportStatus::InProgress => None,
            ReportStatus::Complete => Some(100),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ChangeCounts {
    pub added: i64,
    pub removed: i64,
    pub changed: i64,
    pub unchanged: i64,
}

#[derive(Debug, Clone)]
pub struct ChangeReport {
    pub id: Uuid,
    pub from_asset_id: Uuid,
    pub to_asset_id: Uuid,
    pub status: ReportStatus,
    pub created_at: String,
    pub counts: ChangeCounts,
    pub error: Option<String>,
}

fn report_path(data_dir: &Path, report_id: Uuid) -> PathBuf {
    data_dir
        .join(REPORTS_DIR)
        .join(format!("{report_id}.sqlite"))
}

fn stored_uuid(row: &SqliteRow, column: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(&row.try_get::<String, _>(column)?)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

fn report_from(row: &SqliteRow) -> Result<ChangeReport, sqlx::Error> {
    Ok(ChangeReport {
        id: stored_uuid(row, "id")?,
        from_asset_id: stored_uuid(row, "from_asset_id")?,
        to_asset_id: stored_uuid(row, "to_asset_id")?,
        status: ReportStatus::from_stored_name(&row.try_get::<String, _>("status")?)?,
        created_at: row.try_get("created_at")?,
        counts: ChangeCounts {
            added: row.try_get("added")?,
            removed: row.try_get("removed")?,
            changed: row.try_get("changed")?,
            unchanged: row.try_get("unchanged")?,
        },
        error: row.try_get("error")?,
    })
}

async fn report_by_id(db: &Database, id: Uuid) -> Result<Option<ChangeReport>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {REPORT_COLUMNS} FROM change_reports WHERE id = ?"
    )))
    .bind(id.to_string())
    .fetch_optional(&db.pool)
    .await?;
    row.as_ref().map(report_from).transpose()
}

async fn report_of_pair(
    db: &Database,
    from_asset_id: Uuid,
    to_asset_id: Uuid,
) -> Result<Option<ChangeReport>, sqlx::Error> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {REPORT_COLUMNS} FROM change_reports WHERE from_asset_id = ? AND to_asset_id = ?"
    )))
    .bind(from_asset_id.to_string())
    .bind(to_asset_id.to_string())
    .fetch_optional(&db.pool)
    .await?;
    row.as_ref().map(report_from).transpose()
}

async fn set_report_status(
    db: &Database,
    id: Uuid,
    status: ReportStatus,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE change_reports SET status = ?, error = ? WHERE id = ?")
        .bind(status.stored_name())
        .bind(error)
        .bind(id.to_string())
        .execute(&db.pool)
        .await?;
    Ok(())
}

async fn remove_report_file(data_dir: &Path, report_id: Uuid) -> Result<(), String> {
    let path = report_path(data_dir, report_id);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("removing {} failed: {error}", path.display())),
    }
}

async fn delete_report(db: &Database, data_dir: &Path, report_id: Uuid) -> Result<(), String> {
    sqlx::query("DELETE FROM change_reports WHERE id = ?")
        .bind(report_id.to_string())
        .execute(&db.pool)
        .await
        .map_err(|error| format!("deleting change report {report_id} failed: {error}"))?;
    remove_report_file(data_dir, report_id).await
}

pub async fn delete_reports_of_asset(
    db: &Database,
    data_dir: &Path,
    asset_id: Uuid,
) -> Result<(), String> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM change_reports WHERE from_asset_id = ? OR to_asset_id = ?",
    )
    .bind(asset_id.to_string())
    .bind(asset_id.to_string())
    .fetch_all(&db.pool)
    .await
    .map_err(|error| format!("reading the change reports of {asset_id} failed: {error}"))?;
    for id in ids {
        let report_id = Uuid::parse_str(&id)
            .map_err(|error| format!("change report id {id:?} is not a uuid: {error}"))?;
        delete_report(db, data_dir, report_id).await?;
    }
    Ok(())
}

pub async fn fail_interrupted_reports(db: &Database) -> Result<u64, String> {
    let result = sqlx::query(
        "UPDATE change_reports SET status = 'ERROR', \
         error = 'the server restarted during this comparison, send it again' \
         WHERE status IN ('NOT_STARTED', 'IN_PROGRESS')",
    )
    .execute(&db.pool)
    .await
    .map_err(|error| format!("failing the interrupted change reports failed: {error}"))?;
    Ok(result.rows_affected())
}

pub fn change_report_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/v1/assets/compare",
            post(create_report).layer(middleware::from_fn(users::require_editor)),
        )
        .route("/v1/assets/{id}/change-reports", get(reports_of_asset))
        .route("/v1/change-reports/{id}", get(get_report))
        .route("/v1/change-reports/{id}/changes", get(list_changes))
}

fn report_metadata(report: &ChangeReport, bytes: u64) -> serde_json::Value {
    let mut metadata = json!({
        "id": report.id,
        "type": "CHANGE_DETECTION",
        "name": format!("Changes from {} to {}", report.from_asset_id, report.to_asset_id),
        "description": "",
        "status": report.status,
        "dateAdded": report.created_at,
        "bytes": bytes,
        "fromAssetId": report.from_asset_id,
        "toAssetId": report.to_asset_id,
        "summary": report.counts,
        "error": report.error,
    });
    if let Some(percent_complete) = report.status.percent_complete() {
        metadata["percentComplete"] = json!(percent_complete);
    }
    metadata
}

fn no_report(id: &str) -> IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        format!("no change report {id}"),
    )
}

fn hide_asset_as_report(id: &str, answer: IonError) -> IonError {
    if answer.0 == StatusCode::NOT_FOUND {
        return no_report(id);
    }
    answer
}

async fn visible_report(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<ChangeReport, IonError> {
    let report_id = Uuid::parse_str(id).map_err(|_| no_report(id))?;
    let report = report_by_id(&state.db, report_id)
        .await
        .map_err(|error| server_failure("the change report could not be read", error))?
        .ok_or_else(|| no_report(id))?;
    for asset_id in [report.from_asset_id, report.to_asset_id] {
        permitted_asset(state, headers, &asset_id.to_string(), may_view_asset)
            .await
            .map_err(|answer| hide_asset_as_report(id, answer))?;
    }
    Ok(report)
}

#[derive(Debug, Deserialize)]
struct CompareRequest {
    #[serde(rename = "fromAssetId")]
    from_asset_id: serde_json::Value,
    #[serde(rename = "toAssetId")]
    to_asset_id: serde_json::Value,
}

fn asset_id_text(name: &str, value: &serde_json::Value) -> Result<String, IonError> {
    match value {
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::String(text) => Ok(text.clone()),
        _ => Err(invalid_parameter(format!(
            "{name} takes an asset's Ion id or UUID"
        ))),
    }
}

fn no_store(asset: &Asset) -> IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "ASSET_HAS_NO_ELEMENT_STORE",
        format!(
            "asset {} has no element store: only an IFC model gets one, when its tiling job finishes",
            asset.id
        ),
    )
}

async fn store_has_mesh_hashes(store: &Path) -> Result<bool, sqlx::Error> {
    let mut connection = open_immutable_file(store).await?;
    let columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('elements') WHERE name = 'mesh_hash'",
    )
    .fetch_one(&mut connection)
    .await?;
    Ok(columns > 0)
}

async fn create_report(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<CompareRequest>, JsonRejection>,
) -> Result<
    (
        StatusCode,
        Extension<AuditedResource>,
        Json<serde_json::Value>,
    ),
    IonError,
> {
    let Json(request) = body.map_err(|rejection| invalid_parameter(rejection.body_text()))?;
    let from_id = asset_id_text("fromAssetId", &request.from_asset_id)?;
    let to_id = asset_id_text("toAssetId", &request.to_asset_id)?;

    let (from_asset, _) = permitted_asset(&state, &headers, &from_id, may_modify_asset).await?;
    let (to_asset, _) = permitted_asset(&state, &headers, &to_id, may_modify_asset).await?;

    let lookup_failed =
        |error: sqlx::Error| server_failure("the change report could not be created", error);
    let from_version = version_of(&state.db, from_asset.id)
        .await
        .map_err(lookup_failed)?;
    let to_version = version_of(&state.db, to_asset.id)
        .await
        .map_err(lookup_failed)?;
    let (Some(from_version), Some(to_version)) = (from_version, to_version) else {
        return Err(invalid_parameter(
            "fromAssetId and toAssetId must be versions of one model",
        ));
    };
    if from_version.base_asset_id != to_version.base_asset_id {
        return Err(invalid_parameter(
            "fromAssetId and toAssetId must be versions of one model",
        ));
    }
    if from_version.version_number >= to_version.version_number {
        return Err(invalid_parameter(format!(
            "fromAssetId is version {} and toAssetId is version {}: fromAssetId must be the earlier one",
            from_version.version_number, to_version.version_number
        )));
    }

    let from_store = store_path(&state.data_dir.join(from_asset.id.to_string()));
    let to_store = store_path(&state.data_dir.join(to_asset.id.to_string()));
    if !from_store.is_file() {
        return Err(no_store(&from_asset));
    }
    if !to_store.is_file() {
        return Err(no_store(&to_asset));
    }
    for (asset, store) in [(&from_asset, &from_store), (&to_asset, &to_store)] {
        if !store_has_mesh_hashes(store).await.map_err(lookup_failed)? {
            return Err(element_error(
                StatusCode::CONFLICT,
                "ELEMENT_STORE_OUTDATED",
                format!(
                    "asset {} has an element store built before geometry hashes were stored: \
                     re-tile it with POST /api/v1/assets/{}/tile, then compare again",
                    asset.id, asset.id
                ),
            ));
        }
    }

    if let Some(existing) = report_of_pair(&state.db, from_asset.id, to_asset.id)
        .await
        .map_err(lookup_failed)?
    {
        if existing.status != ReportStatus::Error {
            return Err(element_error(
                StatusCode::CONFLICT,
                "REPORT_EXISTS",
                format!(
                    "change report {} already compares these versions",
                    existing.id
                ),
            ));
        }
        delete_report(&state.db, &state.data_dir, existing.id)
            .await
            .map_err(|reason| {
                server_failure("the failed change report could not be replaced", reason)
            })?;
    }

    let report_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO change_reports (id, from_asset_id, to_asset_id, status, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(report_id.to_string())
    .bind(from_asset.id.to_string())
    .bind(to_asset.id.to_string())
    .bind(ReportStatus::NotStarted.stored_name())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&state.db.pool)
    .await
    .map_err(lookup_failed)?;
    if inserted.rows_affected() == 0 {
        return Err(element_error(
            StatusCode::CONFLICT,
            "REPORT_EXISTS",
            "another request is already comparing these versions",
        ));
    }
    let report = report_by_id(&state.db, report_id)
        .await
        .map_err(lookup_failed)?
        .ok_or_else(|| no_report(&report_id.to_string()))?;

    tokio::spawn(run_comparison(
        Arc::clone(&state),
        report_id,
        from_store,
        to_store,
    ));

    // ion's openapi spec answers this call with 200
    Ok((
        StatusCode::OK,
        Extension(AuditedResource(report_id.to_string())),
        Json(report_metadata(&report, 0)),
    ))
}

async fn run_comparison(
    state: Arc<AppState>,
    report_id: Uuid,
    from_store: PathBuf,
    to_store: PathBuf,
) {
    let Err(reason) = compare_into_report(&state, report_id, &from_store, &to_store).await else {
        return;
    };
    tracing::error!("change report {report_id} failed: {reason}");
    if let Err(error) =
        set_report_status(&state.db, report_id, ReportStatus::Error, Some(&reason)).await
    {
        tracing::error!(
            "change report {report_id}: it reads as unfinished until the next restart, writing its failure failed too: {error}"
        );
    }
}

async fn compare_into_report(
    state: &AppState,
    report_id: Uuid,
    from_store: &Path,
    to_store: &Path,
) -> Result<(), String> {
    let _slot = COMPARISON_SLOTS
        .acquire()
        .await
        .map_err(|error| format!("no comparison slot: {error}"))?;
    set_report_status(&state.db, report_id, ReportStatus::InProgress, None)
        .await
        .map_err(|error| format!("writing the report's progress failed: {error}"))?;
    let counts = write_report(&state.data_dir, report_id, from_store, to_store).await?;
    sqlx::query(
        "UPDATE change_reports SET status = ?, added = ?, removed = ?, changed = ?, \
             unchanged = ? WHERE id = ?",
    )
    .bind(ReportStatus::Complete.stored_name())
    .bind(counts.added)
    .bind(counts.removed)
    .bind(counts.changed)
    .bind(counts.unchanged)
    .bind(report_id.to_string())
    .execute(&state.db.pool)
    .await
    .map_err(|error| format!("writing the report's result failed: {error}"))?;
    Ok(())
}

struct ReportWriter {
    connection: SqliteConnection,
    counts: ChangeCounts,
}

struct ChangeRow<'a> {
    kind: &'a str,
    element: &'a ComparableElement,
    changed_keys: &'a [String],
    geometry_changed: bool,
    location_changed: bool,
}

impl ReportWriter {
    async fn create(path: &Path) -> Result<Self, sqlx::Error> {
        // the build file is thrown away on any failure, so it needs no journal
        let mut connection = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Off)
            .synchronous(SqliteSynchronous::Off)
            .connect()
            .await?;
        sqlx::query(
            "CREATE TABLE changes (
                position INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                element_id TEXT NOT NULL,
                label TEXT,
                ifc_class TEXT NOT NULL,
                changed_keys TEXT NOT NULL,
                geometry_changed INTEGER NOT NULL,
                location_changed INTEGER NOT NULL
            )",
        )
        .execute(&mut connection)
        .await?;
        sqlx::query("BEGIN").execute(&mut connection).await?;
        Ok(Self {
            connection,
            counts: ChangeCounts::default(),
        })
    }

    async fn insert(&mut self, row: ChangeRow<'_>) -> Result<(), sqlx::Error> {
        let changed_keys = serde_json::to_string(row.changed_keys)
            .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        sqlx::query(
            "INSERT INTO changes (kind, element_id, label, ifc_class, changed_keys, \
             geometry_changed, location_changed) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.kind)
        .bind(&row.element.global_id)
        .bind(&row.element.label)
        .bind(&row.element.ifc_class)
        .bind(changed_keys)
        .bind(row.geometry_changed)
        .bind(row.location_changed)
        .execute(&mut self.connection)
        .await?;
        Ok(())
    }

    async fn added(&mut self, element: &ComparableElement) -> Result<(), sqlx::Error> {
        self.counts.added += 1;
        self.insert(ChangeRow {
            kind: "added",
            element,
            changed_keys: &[],
            geometry_changed: false,
            location_changed: false,
        })
        .await
    }

    async fn removed(&mut self, element: &ComparableElement) -> Result<(), sqlx::Error> {
        self.counts.removed += 1;
        self.insert(ChangeRow {
            kind: "removed",
            element,
            changed_keys: &[],
            geometry_changed: false,
            location_changed: false,
        })
        .await
    }

    async fn compared(
        &mut self,
        older: &ComparableElement,
        newer: &ComparableElement,
    ) -> Result<(), sqlx::Error> {
        let changed_keys = changed_keys(older, newer);
        let geometry_changed = older.mesh_hash != newer.mesh_hash;
        let location_changed = location_changed(older.location.as_ref(), newer.location.as_ref());
        if changed_keys.is_empty() && !geometry_changed && !location_changed {
            self.counts.unchanged += 1;
            return Ok(());
        }
        self.counts.changed += 1;
        self.insert(ChangeRow {
            kind: "changed",
            element: newer,
            changed_keys: &changed_keys,
            geometry_changed,
            location_changed,
        })
        .await
    }

    async fn finish(mut self) -> Result<ChangeCounts, sqlx::Error> {
        sqlx::query("CREATE INDEX changes_by_kind ON changes (kind, position)")
            .execute(&mut self.connection)
            .await?;
        sqlx::query("COMMIT").execute(&mut self.connection).await?;
        self.connection.close().await?;
        Ok(self.counts)
    }
}

fn changed_keys(older: &ComparableElement, newer: &ComparableElement) -> Vec<String> {
    let mut keys = Vec::new();
    let root_fields = [
        ("@label", older.label != newer.label),
        ("@classFullName", older.ifc_class != newer.ifc_class),
        ("@storey", older.storey != newer.storey),
        ("@parent", older.parent_id != newer.parent_id),
    ];
    for (name, differs) in root_fields {
        if differs {
            keys.push(name.to_string());
        }
    }
    let mut property_keys: Vec<String> = older
        .properties
        .iter()
        .filter(|(key, value)| newer.properties.get(*key) != Some(*value))
        .map(|(key, _)| key.clone())
        .chain(
            newer
                .properties
                .keys()
                .filter(|key| !older.properties.contains_key(*key))
                .cloned(),
        )
        .collect();
    property_keys.sort_unstable();
    keys.extend(property_keys);
    keys
}

fn location_changed(older: Option<&Location>, newer: Option<&Location>) -> bool {
    let (Some(older), Some(newer)) = (older, newer) else {
        return older.is_some() != newer.is_some();
    };
    let degrees_apart = (older.center.longitude - newer.center.longitude)
        .abs()
        .max((older.center.latitude - newer.center.latitude).abs());
    let metres_apart = (older.center.height - newer.center.height)
        .abs()
        .max((older.radius - newer.radius).abs());
    degrees_apart > LOCATION_TOLERANCE_DEGREES || metres_apart > LOCATION_TOLERANCE_METRES
}

fn database_failure(error: sqlx::Error) -> String {
    format!("the comparison failed: {error}")
}

async fn write_report(
    data_dir: &Path,
    report_id: Uuid,
    from_store: &Path,
    to_store: &Path,
) -> Result<ChangeCounts, String> {
    let published = report_path(data_dir, report_id);
    let building = published.with_extension("building");
    let directory = data_dir.join(REPORTS_DIR);
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;

    let counts = compare_stores(from_store, to_store, &building).await;
    let counts = match counts {
        Ok(counts) => counts,
        Err(reason) => {
            if let Err(error) = tokio::fs::remove_file(&building).await {
                tracing::warn!("removing {} failed: {error}", building.display());
            }
            return Err(reason);
        }
    };
    // the report was written with synchronous off, and a rename can reach disk before the file's pages do
    // windows refuses FlushFileBuffers on a handle opened read-only
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&building)
        .await
        .map_err(|error| format!("could not publish the report: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("could not publish the report: {error}"))?;
    tokio::fs::rename(&building, &published)
        .await
        .map_err(|error| format!("could not publish the report: {error}"))?;
    Ok(counts)
}

async fn compare_stores(
    from_store: &Path,
    to_store: &Path,
    report: &Path,
) -> Result<ChangeCounts, String> {
    let mut from_connection = open_immutable_file(from_store)
        .await
        .map_err(database_failure)?;
    let mut to_connection = open_immutable_file(to_store)
        .await
        .map_err(database_failure)?;
    let mut writer = ReportWriter::create(report)
        .await
        .map_err(database_failure)?;

    let mut older_elements = ElementsInGlobalIdOrder::new(&mut from_connection);
    let mut newer_elements = ElementsInGlobalIdOrder::new(&mut to_connection);
    let mut older = older_elements
        .next_element()
        .await
        .map_err(database_failure)?;
    let mut newer = newer_elements
        .next_element()
        .await
        .map_err(database_failure)?;

    loop {
        match (&older, &newer) {
            (None, None) => break,
            (Some(older_element), None) => {
                writer
                    .removed(older_element)
                    .await
                    .map_err(database_failure)?;
                older = older_elements
                    .next_element()
                    .await
                    .map_err(database_failure)?;
            }
            (None, Some(newer_element)) => {
                writer
                    .added(newer_element)
                    .await
                    .map_err(database_failure)?;
                newer = newer_elements
                    .next_element()
                    .await
                    .map_err(database_failure)?;
            }
            (Some(older_element), Some(newer_element)) => {
                let order = older_element.global_id.cmp(&newer_element.global_id);
                match order {
                    Ordering::Less => writer.removed(older_element).await,
                    Ordering::Greater => writer.added(newer_element).await,
                    Ordering::Equal => writer.compared(older_element, newer_element).await,
                }
                .map_err(database_failure)?;
                if order != Ordering::Greater {
                    older = older_elements
                        .next_element()
                        .await
                        .map_err(database_failure)?;
                }
                if order != Ordering::Less {
                    newer = newer_elements
                        .next_element()
                        .await
                        .map_err(database_failure)?;
                }
            }
        }
    }
    writer.finish().await.map_err(database_failure)
}

async fn reports_of_asset(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {REPORT_COLUMNS} FROM change_reports \
         WHERE from_asset_id = ? OR to_asset_id = ? ORDER BY created_at DESC"
    )))
    .bind(asset.id.to_string())
    .bind(asset.id.to_string())
    .fetch_all(&state.db.pool)
    .await
    .map_err(|error| server_failure("the change reports could not be read", error))?;
    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        let report = report_from(row)
            .map_err(|error| server_failure("the change reports could not be read", error))?;
        items.push(report_metadata(
            &report,
            report_bytes(&state.data_dir, report.id).await,
        ));
    }
    Ok(Json(json!({ "items": items })))
}

async fn report_bytes(data_dir: &Path, report_id: Uuid) -> u64 {
    tokio::fs::metadata(report_path(data_dir, report_id))
        .await
        .map_or(0, |metadata| metadata.len())
}

async fn get_report(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, IonError> {
    let report = visible_report(&state, &headers, &id).await?;
    Ok(Json(report_metadata(
        &report,
        report_bytes(&state.data_dir, report.id).await,
    )))
}

#[derive(Debug, Deserialize)]
struct ChangesParameters {
    kind: Option<String>,
    top: Option<String>,
    #[serde(rename = "continuationToken")]
    continuation_token: Option<String>,
}

fn change_from(row: &SqliteRow) -> Result<serde_json::Value, sqlx::Error> {
    let changed_keys: String = row.try_get("changed_keys")?;
    let changed_keys: Vec<String> = serde_json::from_str(&changed_keys)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    Ok(json!({
        "kind": row.try_get::<String, _>("kind")?,
        "id": row.try_get::<String, _>("element_id")?,
        "label": row.try_get::<Option<String>, _>("label")?,
        "ifcClass": row.try_get::<String, _>("ifc_class")?,
        "changedKeys": changed_keys,
        "geometryChanged": row.try_get::<bool, _>("geometry_changed")?,
        "locationChanged": row.try_get::<bool, _>("location_changed")?,
    }))
}

async fn list_changes(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<ChangesParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let report = visible_report(&state, &headers, &id).await?;
    if report.status != ReportStatus::Complete {
        return Err(element_error(
            StatusCode::CONFLICT,
            "REPORT_NOT_COMPLETE",
            format!("change report {id} has status {:?}", report.status),
        ));
    }
    if let Some(kind) = &parameters.kind
        && !CHANGE_KINDS.contains(&kind.as_str())
    {
        return Err(invalid_parameter(format!(
            "kind {kind}: accepted values are added, removed and changed"
        )));
    }
    let top = bounded_integer(
        "top",
        parameters.top.as_deref(),
        DEFAULT_CHANGES_PER_PAGE,
        MOST_CHANGES_PER_PAGE,
    )?;
    let after = continuation_position(parameters.continuation_token.as_deref())?;

    let read_failed =
        |error: sqlx::Error| server_failure("the change report could not be read", error);
    let mut connection = open_immutable_file(&report_path(&state.data_dir, report.id))
        .await
        .map_err(read_failed)?;
    let rows = sqlx::query(
        "SELECT position, kind, element_id, label, ifc_class, changed_keys, geometry_changed, \
         location_changed FROM changes WHERE position > ? AND (? IS NULL OR kind = ?) \
         ORDER BY position LIMIT ?",
    )
    .bind(after)
    .bind(&parameters.kind)
    .bind(&parameters.kind)
    .bind(top + 1)
    .fetch_all(&mut connection)
    .await
    .map_err(read_failed)?;

    let has_more = rows.len() as i64 > top;
    let mut items = Vec::with_capacity(rows.len());
    let mut last_position = None;
    for row in rows.iter().take(top as usize) {
        items.push(change_from(row).map_err(read_failed)?);
        last_position = Some(row.try_get::<i64, _>("position").map_err(read_failed)?);
    }
    let mut body = json!({ "items": items });
    if let Some(next) = last_position.filter(|_| has_more) {
        body["continuationToken"] = json!(next.to_string());
    }
    Ok(Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with_status(status: ReportStatus) -> ChangeReport {
        ChangeReport {
            id: Uuid::new_v4(),
            from_asset_id: Uuid::new_v4(),
            to_asset_id: Uuid::new_v4(),
            status,
            created_at: "2026-11-05T12:00:00+00:00".to_string(),
            counts: ChangeCounts::default(),
            error: None,
        }
    }

    #[test]
    fn a_report_in_progress_states_no_percentage() {
        let in_progress = report_metadata(&report_with_status(ReportStatus::InProgress), 0);
        assert!(
            in_progress.get("percentComplete").is_none(),
            "{in_progress}"
        );

        let not_started = report_metadata(&report_with_status(ReportStatus::NotStarted), 0);
        assert_eq!(not_started["percentComplete"], 0);
        let complete = report_metadata(&report_with_status(ReportStatus::Complete), 0);
        assert_eq!(complete["percentComplete"], 100);
    }
}
