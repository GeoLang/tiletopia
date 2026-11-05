use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

const SCRATCH_DIR: &str = "coordination_scratch";

pub(crate) fn scratch_root(data_dir: &Path) -> PathBuf {
    data_dir.join(SCRATCH_DIR)
}

pub(crate) async fn new_scratch_dir(data_dir: &Path) -> std::io::Result<PathBuf> {
    let directory = scratch_root(data_dir).join(Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&directory).await?;
    Ok(directory)
}

pub(crate) async fn download_and_remove(
    directory: &Path,
    file_name: &str,
    content_type: &str,
) -> std::io::Result<Response> {
    let file = tokio::fs::File::open(directory.join(file_name)).await?;
    // an open handle keeps the bytes readable after the directory entry is gone
    tokio::fs::remove_dir_all(directory).await?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{file_name}\""),
            ),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}
