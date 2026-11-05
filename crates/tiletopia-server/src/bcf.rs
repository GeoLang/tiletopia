use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Multipart, Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Json, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::bcf_xml::{
    BcfArchive, BcfProject, BcfTopic, BcfViewpoint, SnapshotFormat, checked_topic, read_archive,
    referenced_ifc_guids, write_archive,
};
use crate::db::Database;
use crate::element_store::{known_global_ids, store_path};
use crate::elements::{
    ReadableStore, bounded_integer, continuation_position, element_error, open_readable_store,
    permitted_asset, server_failure, store_error,
};
use crate::ion_compat::IonError;
use crate::model_versions::base_asset_id_of;
use crate::upload::write_field;
use crate::zip_unpack::unpack_zip;
use crate::{AppState, may_modify_asset, may_view_asset, scratch_files, users};

const SNAPSHOTS_DIR: &str = "bcf_snapshots";
const UPLOAD_FILE: &str = "upload.bcf";
const UNPACKED_DIR: &str = "unpacked";
const STAGED_DIR: &str = "staged";
const REPLACED_DIR: &str = "replaced";
const BCF_CONTENT_TYPE: &str = "application/zip";

const MOST_BCF_UPLOAD_BYTES: usize = 256 * 1024 * 1024;
const MOST_TOPIC_BODY_BYTES: usize = 64 * 1024 * 1024;

const DEFAULT_TOPICS_PER_PAGE: i64 = 100;
const MOST_TOPICS_PER_PAGE: i64 = 1000;

pub fn bcf_routes() -> Router<Arc<AppState>> {
    let write_routes = Router::new()
        .route(
            "/v1/assets/{id}/bcf/import",
            post(import_bcf).layer(DefaultBodyLimit::max(MOST_BCF_UPLOAD_BYTES)),
        )
        .route(
            "/v1/assets/{id}/bcf/topics/{guid}",
            put(put_topic)
                .delete(delete_topic)
                .layer(DefaultBodyLimit::max(MOST_TOPIC_BODY_BYTES)),
        )
        .layer(middleware::from_fn(users::require_editor));
    Router::new()
        .route("/v1/assets/{id}/bcf/export", get(export_bcf))
        .route("/v1/assets/{id}/bcf/topics", get(list_topics))
        .route("/v1/assets/{id}/bcf/topics/{guid}", get(get_topic))
        .route(
            "/v1/assets/{id}/bcf/topics/{guid}/viewpoints/{viewpoint_guid}/snapshot",
            get(get_snapshot),
        )
        .merge(write_routes)
}

fn bcf_refusal(message: impl Into<String>) -> IonError {
    element_error(StatusCode::BAD_REQUEST, "INVALID_BCF", message)
}

fn no_topic(guid: &str) -> IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "TOPIC_NOT_FOUND",
        format!("no BCF topic {guid} on this model"),
    )
}

fn read_failure(error: impl std::fmt::Display) -> IonError {
    server_failure("the BCF topics could not be read", error)
}

fn write_failure(error: impl std::fmt::Display) -> IonError {
    server_failure("the BCF topics could not be written", error)
}

fn topic_directory(data_dir: &Path, base_asset_id: Uuid, topic_guid: &str) -> PathBuf {
    data_dir
        .join(SNAPSHOTS_DIR)
        .join(base_asset_id.to_string())
        .join(topic_guid)
}

fn snapshot_path(
    data_dir: &Path,
    base_asset_id: Uuid,
    topic_guid: &str,
    viewpoint_guid: &str,
    format: SnapshotFormat,
) -> PathBuf {
    topic_directory(data_dir, base_asset_id, topic_guid)
        .join(format!("{viewpoint_guid}.{}", format.extension()))
}

pub(crate) async fn remove_dir_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("removing {} failed: {error}", path.display())),
    }
}

pub async fn delete_topics_of_base(
    db: &Database,
    data_dir: &Path,
    base_asset_id: Uuid,
) -> Result<(), String> {
    let describe = |error: sqlx::Error| format!("deleting BCF topics failed: {error}");
    sqlx::query("DELETE FROM bcf_topics WHERE base_asset_id = ?")
        .bind(base_asset_id.to_string())
        .execute(&db.pool)
        .await
        .map_err(describe)?;
    sqlx::query("DELETE FROM bcf_projects WHERE base_asset_id = ?")
        .bind(base_asset_id.to_string())
        .execute(&db.pool)
        .await
        .map_err(describe)?;
    remove_dir_if_present(&data_dir.join(SNAPSHOTS_DIR).join(base_asset_id.to_string())).await
}

fn parsed_topic(text: &str) -> Result<BcfTopic, sqlx::Error> {
    serde_json::from_str(text).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

async fn stored_topic(
    db: &Database,
    base_asset_id: Uuid,
    guid: &str,
) -> Result<Option<BcfTopic>, sqlx::Error> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT topic FROM bcf_topics WHERE base_asset_id = ? AND guid = ?")
            .bind(base_asset_id.to_string())
            .bind(guid)
            .fetch_optional(&db.pool)
            .await?;
    text.as_deref().map(parsed_topic).transpose()
}

async fn all_topics(db: &Database, base_asset_id: Uuid) -> Result<Vec<BcfTopic>, sqlx::Error> {
    let texts: Vec<String> =
        sqlx::query_scalar("SELECT topic FROM bcf_topics WHERE base_asset_id = ? ORDER BY rowid")
            .bind(base_asset_id.to_string())
            .fetch_all(&db.pool)
            .await?;
    texts.iter().map(|text| parsed_topic(text)).collect()
}

async fn stored_project(db: &Database, base_asset_id: Uuid) -> Result<BcfProject, sqlx::Error> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT project FROM bcf_projects WHERE base_asset_id = ?")
            .bind(base_asset_id.to_string())
            .fetch_optional(&db.pool)
            .await?;
    match text {
        Some(text) => {
            serde_json::from_str(&text).map_err(|error| sqlx::Error::Decode(Box::new(error)))
        }
        None => Ok(BcfProject::default()),
    }
}

fn encoded<T: Serialize>(value: &T) -> Result<String, sqlx::Error> {
    serde_json::to_string(value).map_err(|error| sqlx::Error::Encode(Box::new(error)))
}

struct StoredTopicCounts {
    created: u64,
    updated: u64,
}

async fn store_archive(
    connection: &mut sqlx::SqliteConnection,
    base_asset_id: Uuid,
    archive: &BcfArchive,
) -> Result<StoredTopicCounts, sqlx::Error> {
    let base = base_asset_id.to_string();
    let mut counts = StoredTopicCounts {
        created: 0,
        updated: 0,
    };
    for topic in &archive.topics {
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM bcf_topics WHERE base_asset_id = ? AND guid = ?")
                .bind(&base)
                .bind(&topic.guid)
                .fetch_optional(&mut *connection)
                .await?;
        match existing {
            Some(_) => counts.updated += 1,
            None => counts.created += 1,
        }
        sqlx::query(
            "INSERT INTO bcf_topics (base_asset_id, guid, topic) VALUES (?, ?, ?) \
             ON CONFLICT (base_asset_id, guid) DO UPDATE SET topic = excluded.topic",
        )
        .bind(&base)
        .bind(&topic.guid)
        .bind(encoded(topic)?)
        .execute(&mut *connection)
        .await?;
    }
    sqlx::query(
        "INSERT INTO bcf_projects (base_asset_id, project) VALUES (?, ?) \
         ON CONFLICT (base_asset_id) DO UPDATE SET project = excluded.project",
    )
    .bind(&base)
    .bind(encoded(&archive.project)?)
    .execute(&mut *connection)
    .await?;
    Ok(counts)
}

async fn stage_snapshots(
    staged_root: &Path,
    base_asset_id: Uuid,
    archive: &BcfArchive,
) -> Result<(), String> {
    for snapshot in &archive.snapshots {
        let target = snapshot_path(
            staged_root,
            base_asset_id,
            &snapshot.topic_guid,
            &snapshot.viewpoint_guid,
            snapshot.format,
        );
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        // two viewpoints in one markup may name the same snapshot file
        tokio::fs::copy(&snapshot.source, &target)
            .await
            .map_err(|error| format!("could not store {}: {error}", target.display()))?;
    }
    Ok(())
}

struct DirectoryMove {
    from: PathBuf,
    to: PathBuf,
}

async fn move_topic_directories(
    data_dir: &Path,
    scratch: &Path,
    base_asset_id: Uuid,
    archive: &BcfArchive,
    moves: &mut Vec<DirectoryMove>,
) -> Result<(), String> {
    for topic in &archive.topics {
        let live = topic_directory(data_dir, base_asset_id, &topic.guid);
        let replaced = topic_directory(&scratch.join(REPLACED_DIR), base_asset_id, &topic.guid);
        let staged = topic_directory(&scratch.join(STAGED_DIR), base_asset_id, &topic.guid);
        for (from, to) in [(live.clone(), replaced), (staged, live)] {
            let exists = tokio::fs::try_exists(&from)
                .await
                .map_err(|error| format!("could not read {}: {error}", from.display()))?;
            if !exists {
                continue;
            }
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
            }
            tokio::fs::rename(&from, &to).await.map_err(|error| {
                format!(
                    "moving {} to {} failed: {error}",
                    from.display(),
                    to.display()
                )
            })?;
            moves.push(DirectoryMove { from, to });
        }
    }
    Ok(())
}

async fn undo_moves(moves: &[DirectoryMove]) {
    for moved in moves.iter().rev() {
        if let Err(error) = tokio::fs::rename(&moved.to, &moved.from).await {
            tracing::error!(
                "moving {} back to {} failed: {error}",
                moved.to.display(),
                moved.from.display()
            );
        }
    }
}

async fn unknown_ids_in(store: &mut ReadableStore, ids: Vec<String>) -> Result<u64, sqlx::Error> {
    let known = known_global_ids(&mut store.connection, &ids).await?;
    Ok(ids.len() as u64 - known.len() as u64)
}

async fn import_unpacked(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    base_asset_id: Uuid,
    scratch: &Path,
) -> Result<serde_json::Value, IonError> {
    let zip_path = scratch.join(UPLOAD_FILE);
    let unpacked_dir = scratch.join(UNPACKED_DIR);
    let unpack_destination = unpacked_dir.clone();
    let archive = tokio::task::spawn_blocking(move || {
        let unpacked = unpack_zip(&zip_path, &unpack_destination)?;
        read_archive(&unpack_destination, &unpacked.relative_paths)
    })
    .await
    .map_err(read_failure)?
    .map_err(bcf_refusal)?;

    let (ids, mut references) = referenced_ifc_guids(&archive.topics);
    {
        let mut store = open_readable_store(state, headers, id).await?;
        references.unknown_ids = unknown_ids_in(&mut store, ids.into_iter().collect())
            .await
            .map_err(store_error)?;
    }

    stage_snapshots(&scratch.join(STAGED_DIR), base_asset_id, &archive)
        .await
        .map_err(write_failure)?;
    let mut transaction = state.db.pool.begin().await.map_err(write_failure)?;
    let topic_counts = store_archive(&mut transaction, base_asset_id, &archive)
        .await
        .map_err(write_failure)?;
    let mut moves = Vec::new();
    let committed = match move_topic_directories(
        &state.data_dir,
        scratch,
        base_asset_id,
        &archive,
        &mut moves,
    )
    .await
    {
        Ok(()) => transaction.commit().await.map_err(write_failure),
        Err(reason) => Err(write_failure(reason)),
    };
    if let Err(answer) = committed {
        undo_moves(&moves).await;
        return Err(answer);
    }

    Ok(json!({
        "topics": archive.topics.len(),
        "created": topic_counts.created,
        "updated": topic_counts.updated,
        "comments": archive.topics.iter().map(|topic| topic.comments.len()).sum::<usize>(),
        "viewpoints": archive.topics.iter().map(|topic| topic.viewpoints.len()).sum::<usize>(),
        "snapshots": archive.snapshots.len(),
        "elementReferences": references,
        "dropped": archive.dropped,
    }))
}

async fn import_bcf(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    if !store_path(&state.data_dir.join(asset.id.to_string())).is_file() {
        return Err(element_error(
            StatusCode::NOT_FOUND,
            "ASSET_HAS_NO_ELEMENT_STORE",
            format!(
                "asset {id} has no element store to map the file's IFC GUIDs to: only an IFC model gets one, when its tiling job finishes"
            ),
        ));
    }
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;

    let scratch = scratch_files::new_scratch_dir(&state.data_dir)
        .await
        .map_err(write_failure)?;
    let imported = async {
        let mut has_file = false;
        while let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|_| bcf_refusal("malformed multipart body"))?
        {
            if field.name() == Some("file") {
                write_field(&mut field, &scratch.join(UPLOAD_FILE))
                    .await
                    .map_err(|(status, message)| element_error(status, "INVALID_BCF", message))?;
                has_file = true;
            }
        }
        if !has_file {
            return Err(bcf_refusal("no file field in the upload"));
        }
        import_unpacked(&state, &headers, &id, base_asset_id, &scratch).await
    }
    .await;
    if let Err(reason) = remove_dir_if_present(&scratch).await {
        tracing::warn!("{reason}");
    }
    imported.map(Json)
}

async fn export_bcf(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Response, IonError> {
    let (asset, ion_id) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;
    let topics = all_topics(&state.db, base_asset_id)
        .await
        .map_err(read_failure)?;
    let project = stored_project(&state.db, base_asset_id)
        .await
        .map_err(read_failure)?;

    let scratch = scratch_files::new_scratch_dir(&state.data_dir)
        .await
        .map_err(read_failure)?;
    let file_name = format!("model-{ion_id}.bcf");
    let output = scratch.join(&file_name);
    let data_dir = state.data_dir.clone();
    let written = tokio::task::spawn_blocking(move || {
        write_archive(
            &output,
            &project,
            &topics,
            |topic: &BcfTopic, viewpoint: &BcfViewpoint, format: SnapshotFormat| {
                snapshot_path(
                    &data_dir,
                    base_asset_id,
                    &topic.guid,
                    &viewpoint.guid,
                    format,
                )
            },
        )
    })
    .await
    .map_err(read_failure)
    .and_then(|written| written.map_err(read_failure));
    if let Err(answer) = written {
        if let Err(reason) = remove_dir_if_present(&scratch).await {
            tracing::warn!("{reason}");
        }
        return Err(answer);
    }
    scratch_files::download_and_remove(&scratch, &file_name, BCF_CONTENT_TYPE)
        .await
        .map_err(read_failure)
}

#[derive(Debug, Deserialize)]
struct TopicListParameters {
    top: Option<String>,
    #[serde(rename = "continuationToken")]
    continuation_token: Option<String>,
}

fn topic_summary(topic: &BcfTopic) -> serde_json::Value {
    json!({
        "guid": topic.guid,
        "serverAssignedId": topic.server_assigned_id,
        "title": topic.title,
        "topicType": topic.topic_type,
        "topicStatus": topic.topic_status,
        "priority": topic.priority,
        "labels": topic.labels,
        "assignedTo": topic.assigned_to,
        "dueDate": topic.due_date,
        "creationDate": topic.creation_date,
        "creationAuthor": topic.creation_author,
        "commentCount": topic.comments.len(),
        "viewpointCount": topic.viewpoints.len(),
    })
}

async fn list_topics(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<TopicListParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let top = bounded_integer(
        "top",
        parameters.top.as_deref(),
        DEFAULT_TOPICS_PER_PAGE,
        MOST_TOPICS_PER_PAGE,
    )?;
    let after = continuation_position(parameters.continuation_token.as_deref())?;
    let base = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?
        .to_string();

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bcf_topics WHERE base_asset_id = ?")
        .bind(&base)
        .fetch_one(&state.db.pool)
        .await
        .map_err(read_failure)?;
    let rows = sqlx::query(
        "SELECT rowid AS position, topic FROM bcf_topics \
         WHERE base_asset_id = ? AND rowid > ? ORDER BY rowid LIMIT ?",
    )
    .bind(&base)
    .bind(after)
    .bind(top + 1)
    .fetch_all(&state.db.pool)
    .await
    .map_err(read_failure)?;

    let has_more = rows.len() as i64 > top;
    let mut items = Vec::with_capacity(rows.len());
    let mut last_position = None;
    for row in rows.iter().take(top as usize) {
        let text: String = row.try_get("topic").map_err(read_failure)?;
        items.push(topic_summary(&parsed_topic(&text).map_err(read_failure)?));
        last_position = Some(row.try_get::<i64, _>("position").map_err(read_failure)?);
    }
    let mut body = json!({ "items": items, "total": total });
    if let Some(next) = last_position.filter(|_| has_more) {
        body["continuationToken"] = json!(next.to_string());
    }
    Ok(Json(body))
}

fn route_guid(guid: &str) -> Result<String, IonError> {
    Uuid::try_parse(guid)
        .map(|guid| guid.to_string())
        .map_err(|_| no_topic(guid))
}

async fn get_topic(
    State(state): State<Arc<AppState>>,
    RoutePath((id, guid)): RoutePath<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<BcfTopic>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let guid = route_guid(&guid)?;
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;
    stored_topic(&state.db, base_asset_id, &guid)
        .await
        .map_err(read_failure)?
        .map(Json)
        .ok_or_else(|| no_topic(&guid))
}

async fn put_topic(
    State(state): State<Arc<AppState>>,
    RoutePath((id, guid)): RoutePath<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<BcfTopic>, JsonRejection>,
) -> Result<(StatusCode, Json<BcfTopic>), IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    let guid = Uuid::try_parse(&guid)
        .map(|guid| guid.to_string())
        .map_err(|_| bcf_refusal(format!("{guid:?} is not a BCF Guid")))?;
    let Json(topic) = body.map_err(|rejection| bcf_refusal(rejection.body_text()))?;
    let mut topic = checked_topic(topic).map_err(bcf_refusal)?;
    if topic.guid != guid {
        return Err(bcf_refusal(format!(
            "the body names topic {}, and the path names {guid}",
            topic.guid
        )));
    }

    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;
    let previous = stored_topic(&state.db, base_asset_id, &guid)
        .await
        .map_err(read_failure)?;
    let previous_formats: HashMap<String, SnapshotFormat> = previous
        .iter()
        .flat_map(|previous| &previous.viewpoints)
        .filter_map(|viewpoint| Some((viewpoint.guid.clone(), viewpoint.snapshot_format?)))
        .collect();
    // snapshots exist only as files an import stored
    for viewpoint in &mut topic.viewpoints {
        viewpoint.snapshot_format = previous_formats.get(&viewpoint.guid).copied();
    }

    sqlx::query(
        "INSERT INTO bcf_topics (base_asset_id, guid, topic) VALUES (?, ?, ?) \
         ON CONFLICT (base_asset_id, guid) DO UPDATE SET topic = excluded.topic",
    )
    .bind(base_asset_id.to_string())
    .bind(&guid)
    .bind(encoded(&topic).map_err(write_failure)?)
    .execute(&state.db.pool)
    .await
    .map_err(write_failure)?;

    for (viewpoint_guid, format) in previous_formats {
        let is_kept = topic
            .viewpoints
            .iter()
            .any(|viewpoint| viewpoint.guid == viewpoint_guid);
        if is_kept {
            continue;
        }
        let path = snapshot_path(
            &state.data_dir,
            base_asset_id,
            &guid,
            &viewpoint_guid,
            format,
        );
        if let Err(error) = tokio::fs::remove_file(&path).await {
            tracing::warn!("removing {} failed: {error}", path.display());
        }
    }

    let status = if previous.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(topic)))
}

async fn delete_topic(
    State(state): State<Arc<AppState>>,
    RoutePath((id, guid)): RoutePath<(String, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    let guid = route_guid(&guid)?;
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;
    let deleted = sqlx::query("DELETE FROM bcf_topics WHERE base_asset_id = ? AND guid = ?")
        .bind(base_asset_id.to_string())
        .bind(&guid)
        .execute(&state.db.pool)
        .await
        .map_err(write_failure)?
        .rows_affected();
    if deleted == 0 {
        return Err(no_topic(&guid));
    }
    remove_dir_if_present(&topic_directory(&state.data_dir, base_asset_id, &guid))
        .await
        .map_err(write_failure)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_snapshot(
    State(state): State<Arc<AppState>>,
    RoutePath((id, guid, viewpoint_guid)): RoutePath<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let guid = route_guid(&guid)?;
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(read_failure)?;
    let topic = stored_topic(&state.db, base_asset_id, &guid)
        .await
        .map_err(read_failure)?
        .ok_or_else(|| no_topic(&guid))?;
    let no_snapshot = || {
        element_error(
            StatusCode::NOT_FOUND,
            "SNAPSHOT_NOT_FOUND",
            format!("viewpoint {viewpoint_guid} of topic {guid} has no snapshot"),
        )
    };
    let viewpoint = topic
        .viewpoints
        .iter()
        .find(|viewpoint| viewpoint.guid == viewpoint_guid)
        .ok_or_else(no_snapshot)?;
    let format = viewpoint.snapshot_format.ok_or_else(no_snapshot)?;
    let path = snapshot_path(
        &state.data_dir,
        base_asset_id,
        &guid,
        &viewpoint.guid,
        format,
    );
    let bytes = tokio::fs::read(&path).await.map_err(read_failure)?;
    Ok((
        [
            (header::CONTENT_TYPE, format.content_type()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}
