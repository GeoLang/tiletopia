use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::get,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::auth::Claims;
use crate::db::Database;
use crate::elements::{bounded_integer, element_error, permitted_asset, server_failure};
use crate::ion_compat::{IonError, resolve_asset};
use crate::webhooks::{WebhookEvent, WebhookQueue};
use crate::{
    AppState, Asset, AssetStatus, AssetType, bcf, change_reports, element_store, ids_check,
    may_modify_asset, may_view_asset, scratch_files, users,
};

const MOST_VERSIONS_KEPT: usize = 5;
const MOST_BYTES_KEPT_PER_MODEL: u64 = 20 * 1024 * 1024 * 1024;

const DEFAULT_REVISIONS_PER_PAGE: i64 = 100;
const MOST_REVISIONS_PER_PAGE: i64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AssetVersion {
    pub asset_id: Uuid,
    pub base_asset_id: Uuid,
    pub version_number: i64,
}

pub fn model_version_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/assets/{id}/tileset-revisions", get(tileset_revisions))
}

fn stored_uuid(text: &str) -> Result<Uuid, sqlx::Error> {
    Uuid::parse_str(text).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

fn version_from(row: &sqlx::sqlite::SqliteRow) -> Result<AssetVersion, sqlx::Error> {
    Ok(AssetVersion {
        asset_id: stored_uuid(&row.try_get::<String, _>("asset_id")?)?,
        base_asset_id: stored_uuid(&row.try_get::<String, _>("base_asset_id")?)?,
        version_number: row.try_get("version_number")?,
    })
}

pub async fn version_of(
    db: &Database,
    asset_id: Uuid,
) -> Result<Option<AssetVersion>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT asset_id, base_asset_id, version_number FROM asset_versions WHERE asset_id = ?",
    )
    .bind(asset_id.to_string())
    .fetch_optional(&db.pool)
    .await?;
    row.as_ref().map(version_from).transpose()
}

pub async fn base_asset_id_of(db: &Database, asset_id: Uuid) -> Result<Uuid, sqlx::Error> {
    // assets with no re-upload have no row
    Ok(version_of(db, asset_id)
        .await?
        .map_or(asset_id, |version| version.base_asset_id))
}

pub async fn add_version(
    db: &Database,
    new_asset_id: Uuid,
    existing_asset_id: Uuid,
) -> Result<AssetVersion, sqlx::Error> {
    let mut transaction = db.pool.begin().await?;
    let known_base: Option<String> =
        sqlx::query_scalar("SELECT base_asset_id FROM asset_versions WHERE asset_id = ?")
            .bind(existing_asset_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
    let base_asset_id = match known_base {
        Some(base_asset_id) => base_asset_id,
        None => {
            let base_asset_id = existing_asset_id.to_string();
            sqlx::query(
                "INSERT INTO asset_versions (asset_id, base_asset_id, version_number) VALUES (?, ?, 1)",
            )
            .bind(&base_asset_id)
            .bind(&base_asset_id)
            .execute(&mut *transaction)
            .await?;
            base_asset_id
        }
    };
    let version_number: i64 = sqlx::query_scalar(
        "INSERT INTO asset_versions (asset_id, base_asset_id, version_number) \
         SELECT ?, ?, MAX(version_number) + 1 FROM asset_versions WHERE base_asset_id = ? \
         RETURNING version_number",
    )
    .bind(new_asset_id.to_string())
    .bind(&base_asset_id)
    .bind(&base_asset_id)
    .fetch_one(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(AssetVersion {
        asset_id: new_asset_id,
        base_asset_id: stored_uuid(&base_asset_id)?,
        version_number,
    })
}

pub async fn versions_newest_first(
    db: &Database,
    base_asset_id: Uuid,
) -> Result<Vec<AssetVersion>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT asset_id, base_asset_id, version_number FROM asset_versions \
         WHERE base_asset_id = ? ORDER BY version_number DESC",
    )
    .bind(base_asset_id.to_string())
    .fetch_all(&db.pool)
    .await?;
    rows.iter().map(version_from).collect()
}

pub(crate) async fn checked_base_asset(
    state: &AppState,
    claims: &Claims,
    id: &str,
) -> Result<Asset, (StatusCode, String)> {
    let resolved = resolve_asset(state, id).await.map_err(|error| {
        tracing::error!("base asset lookup failed: {error}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "base asset lookup failed".to_string(),
        )
    })?;
    let Some((asset, _)) =
        resolved.filter(|(asset, _)| may_modify_asset(claims, asset.owner_id.as_deref()))
    else {
        return Err((StatusCode::NOT_FOUND, format!("no base asset {id}")));
    };
    if asset.asset_type != AssetType::Model {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("base asset {id} is not a model, and only a model has versions"),
        ));
    }
    Ok(asset)
}

pub(crate) fn owner_of_upload(base_asset: Option<&Asset>, uploader: String) -> Option<String> {
    match base_asset {
        Some(base_asset) => base_asset.owner_id.clone(),
        None => Some(uploader),
    }
}

#[derive(Debug, Deserialize)]
struct RevisionParameters {
    limit: Option<String>,
    page: Option<String>,
}

async fn tileset_revisions(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<RevisionParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let claims = users::claims_from_headers(&headers).map_err(|status| {
        element_error(
            status,
            "UNAUTHORIZED",
            "version routes need Authorization: Bearer <token>",
        )
    })?;
    let limit = bounded_integer(
        "limit",
        parameters.limit.as_deref(),
        DEFAULT_REVISIONS_PER_PAGE,
        MOST_REVISIONS_PER_PAGE,
    )?;
    let page = bounded_integer(
        "page",
        parameters.page.as_deref(),
        1,
        i64::MAX / MOST_REVISIONS_PER_PAGE,
    )?;

    let lookup_failed =
        |error: sqlx::Error| server_failure("the version list could not be read", error);
    let base_asset_id = base_asset_id_of(&state.db, asset.id)
        .await
        .map_err(lookup_failed)?;
    let mut versions = versions_newest_first(&state.db, base_asset_id)
        .await
        .map_err(lookup_failed)?;
    if versions.is_empty() {
        versions.push(AssetVersion {
            asset_id: asset.id,
            base_asset_id: asset.id,
            version_number: 1,
        });
    }

    let mut visible = Vec::with_capacity(versions.len());
    for version in versions {
        let Some((member, ion_id)) = state
            .db
            .get_asset_with_ion_id(version.asset_id)
            .await
            .map_err(lookup_failed)?
        else {
            continue;
        };
        if !may_view_asset(&claims, member.owner_id.as_deref()) {
            continue;
        }
        let asset_dir = state.data_dir.join(member.id.to_string());
        visible.push(json!({
            "assetId": ion_id,
            "creationTimestamp": member.created_at.to_rfc3339(),
            "id": member.id,
            "baseAssetId": version.base_asset_id,
            "versionNumber": version.version_number,
            "name": member.name,
            "status": member.status,
            "hasElementStore": element_store::store_path(&asset_dir).is_file(),
        }));
    }

    let total = visible.len();
    let items: Vec<serde_json::Value> = visible
        .into_iter()
        .skip(((page - 1) * limit) as usize)
        .take(limit as usize)
        .collect();
    Ok(Json(json!({ "items": items, "total": total })))
}

fn directory_bytes(directory: &Path) -> std::io::Result<u64> {
    if !directory.exists() {
        return Ok(0);
    }
    let mut total = 0;
    let mut pending: Vec<PathBuf> = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

pub async fn forget_asset(db: &Database, data_dir: &Path, asset_id: Uuid) -> Result<(), String> {
    let describe = |error: sqlx::Error| format!("forgetting asset {asset_id} failed: {error}");
    change_reports::delete_reports_of_asset(db, data_dir, asset_id).await?;
    ids_check::delete_check_of_asset(db, asset_id).await?;
    let base_asset_id = base_asset_id_of(db, asset_id).await.map_err(describe)?;
    let other_versions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM asset_versions WHERE base_asset_id = ? AND asset_id != ?",
    )
    .bind(base_asset_id.to_string())
    .bind(asset_id.to_string())
    .fetch_one(&db.pool)
    .await
    .map_err(describe)?;
    if other_versions == 0 {
        bcf::delete_topics_of_base(db, data_dir, base_asset_id).await?;
    }
    Ok(())
}

async fn remove_version(
    db: &Database,
    data_dir: &Path,
    webhooks: &WebhookQueue,
    asset: &Asset,
) -> Result<(), String> {
    forget_asset(db, data_dir, asset.id).await?;
    db.delete_asset(asset.id)
        .await
        .map_err(|error| format!("removing version {} failed: {error}", asset.id))?;
    let asset_dir = data_dir.join(asset.id.to_string());
    match tokio::fs::remove_dir_all(&asset_dir).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("removing {} failed: {error}", asset_dir.display())),
    }
    webhooks
        .emit(
            WebhookEvent::AssetDeleted,
            asset.owner_id.as_deref(),
            json!({ "asset_id": asset.id, "name": asset.name, "deleted_by": "version retention" }),
        )
        .await;
    Ok(())
}

pub async fn apply_retention(
    db: &Database,
    data_dir: &Path,
    webhooks: &WebhookQueue,
    asset_id: Uuid,
) -> Result<(), String> {
    let describe = |error: sqlx::Error| format!("version retention failed: {error}");
    let Some(version) = version_of(db, asset_id).await.map_err(describe)? else {
        return Ok(());
    };
    let versions = versions_newest_first(db, version.base_asset_id)
        .await
        .map_err(describe)?;

    let mut kept_versions = 0;
    let mut kept_bytes = 0;
    for version in versions {
        let Some(member) = db.get_asset(version.asset_id).await.map_err(describe)? else {
            continue;
        };
        match member.status {
            AssetStatus::Uploading | AssetStatus::Tiling => {}
            AssetStatus::Error => {
                if kept_versions > 0 {
                    remove_version(db, data_dir, webhooks, &member).await?;
                }
            }
            AssetStatus::Ready => {
                let asset_dir = data_dir.join(member.id.to_string());
                let bytes = tokio::task::spawn_blocking(move || directory_bytes(&asset_dir))
                    .await
                    .map_err(|error| format!("version retention failed: {error}"))?
                    .map_err(|error| format!("version retention failed: {error}"))?;
                let fits = kept_versions == 0
                    || (kept_versions < MOST_VERSIONS_KEPT
                        && kept_bytes + bytes <= MOST_BYTES_KEPT_PER_MODEL);
                if fits {
                    kept_versions += 1;
                    kept_bytes += bytes;
                } else {
                    remove_version(db, data_dir, webhooks, &member).await?;
                }
            }
        }
    }
    Ok(())
}

pub async fn recover_after_restart(db: &Database, data_dir: &Path) -> Result<u64, String> {
    match tokio::fs::remove_dir_all(scratch_files::scratch_root(data_dir)).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("clearing the scratch files failed: {error}")),
    }
    change_reports::fail_interrupted_reports(db).await
}
