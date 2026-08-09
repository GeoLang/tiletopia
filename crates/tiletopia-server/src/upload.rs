//! File upload handler (multipart).

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::{AppState, Asset, AssetStatus, AssetType, users};

/// The created asset, plus the tiling job the upload queued. Only asset types
/// that tile on upload get a `job_id`; the rest tile via `/assets/{id}/tile`.
#[derive(Debug, Serialize)]
pub struct UploadResponse {
    #[serde(flatten)]
    pub asset: Asset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
}

/// Handle multipart file upload.
pub async fn upload_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), StatusCode> {
    // the route sits behind require_editor, so a valid token is always present
    let owner_id = users::claims_from_headers(&headers)?.sub;

    let mut name = None;
    let mut data = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "name" => {
                name = Some(field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?);
            }
            "file" => {
                let file_name = field.file_name().unwrap_or("upload").to_string();
                if name.is_none() {
                    name = Some(file_name);
                }
                data = Some(field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?);
            }
            _ => {}
        }
    }

    let file_data = data.ok_or(StatusCode::BAD_REQUEST)?;
    let asset_name = name.unwrap_or_else(|| "unnamed".into());

    // Detect asset type from extension
    let asset_type = detect_asset_type(&asset_name);

    let id = Uuid::new_v4();
    let asset_dir = state.data_dir.join(id.to_string());
    tokio::fs::create_dir_all(&asset_dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Write uploaded file
    let input_path = asset_dir.join("input").join(&asset_name);
    tokio::fs::create_dir_all(input_path.parent().unwrap())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tokio::fs::write(&input_path, &file_data)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let asset = Asset {
        id,
        name: asset_name,
        asset_type,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: file_data.len() as u64,
        description: String::new(),
        tags: vec![],
        owner_id: Some(owner_id),
    };

    state
        .db
        .create_asset(&asset)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // point clouds get tiled to 3d tiles right away; the job worker flips the
    // asset to Ready and tileset.json becomes servable
    let job_id = if asset.asset_type == AssetType::PointCloud {
        let job = state
            .job_queue
            .submit(asset.id, input_path.to_string_lossy().into_owned())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Some(job.id)
    } else {
        None
    };

    tracing::info!("Uploaded asset {} ({} bytes)", asset.id, file_data.len());
    tracing::info!("metric: assets_uploaded");

    Ok((StatusCode::CREATED, Json(UploadResponse { asset, job_id })))
}

fn detect_asset_type(name: &str) -> AssetType {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "las" | "laz" | "e57" | "ply" => AssetType::PointCloud,
        "tif" | "tiff" | "hgt" | "dt0" | "dt1" | "dt2" => AssetType::Terrain,
        "gltf" | "glb" | "obj" | "fbx" | "ifc" | "dae" => AssetType::Model,
        "jpg" | "jpeg" | "png" | "jp2" => AssetType::Imagery,
        _ => AssetType::PointCloud,
    }
}
