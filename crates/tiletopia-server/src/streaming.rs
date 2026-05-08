//! Streaming upload support.
//!
//! Handles chunked/resumable uploads for files of any size.
//! Implements a simple protocol: client sends chunks with offset headers.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::AppState;

/// Upload session for tracking multi-chunk uploads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSession {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub filename: String,
    pub total_size: u64,
    pub uploaded_bytes: u64,
    pub chunk_size: u64,
    pub complete: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Response for initiating a streaming upload.
#[derive(Debug, Serialize)]
pub struct InitUploadResponse {
    pub session_id: Uuid,
    pub asset_id: Uuid,
    pub chunk_size: u64,
}

/// Initialize a streaming upload session.
pub async fn init_streaming_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<InitUploadResponse>, StatusCode> {
    let filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("upload.bin")
        .to_string();
    let total_size: u64 = headers
        .get("x-total-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let asset_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();

    // Create upload directory
    let upload_dir = state.data_dir.join(asset_id.to_string()).join("input");
    tokio::fs::create_dir_all(&upload_dir)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Create empty file
    let file_path = upload_dir.join(&filename);
    tokio::fs::File::create(&file_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(InitUploadResponse {
        session_id,
        asset_id,
        chunk_size: 8 * 1024 * 1024, // 8 MB default chunks
    }))
}

/// Receive a chunk of a streaming upload.
pub async fn upload_chunk(
    State(state): State<Arc<AppState>>,
    Path(asset_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    let offset: u64 = headers
        .get("x-chunk-offset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("upload.bin");

    // Sanitize filename
    let safe_filename = std::path::Path::new(filename)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("upload.bin");

    let file_path = state
        .data_dir
        .join(asset_id.to_string())
        .join("input")
        .join(safe_filename);

    // Open file and write at offset
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&file_path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut file = file;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.write_all(&body)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.flush()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

/// Complete a streaming upload and trigger processing.
pub async fn complete_streaming_upload(
    State(_state): State<Arc<AppState>>,
    Path(asset_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "asset_id": asset_id,
        "status": "uploaded",
        "message": "Upload complete. Use POST /api/v1/assets/{id}/tile to start tiling."
    }))
}

// Need AsyncSeekExt
use tokio::io::AsyncSeekExt;
