//! tiletopia-store: pluggable storage backends
//!
//! Abstraction over local filesystem, S3, GCS, and Azure Blob storage
//! for reading/writing tiles and assets.

#[cfg(feature = "azure")]
pub mod azure;
#[cfg(feature = "gcs")]
pub mod gcs;
pub mod hybrid;
pub mod s3;

use bytes::Bytes;
use std::path::PathBuf;

/// Storage backend trait.
#[async_trait::async_trait]
pub trait TileStore: Send + Sync {
    /// Read a tile or asset by key.
    async fn get(&self, key: &str) -> Result<Bytes, StoreError>;

    /// Write a tile or asset.
    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError>;

    /// Delete a tile or asset.
    async fn delete(&self, key: &str) -> Result<(), StoreError>;

    /// List keys with a given prefix.
    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;

    /// Check if a key exists.
    async fn exists(&self, key: &str) -> Result<bool, StoreError>;
}

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Other(String),
}

/// Local filesystem storage backend.
pub struct LocalStore {
    pub root: PathBuf,
}

impl LocalStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve(&self, key: &str) -> PathBuf {
        // Prevent path traversal
        let sanitized = key.replace("..", "").replace('\\', "/");
        self.root.join(sanitized)
    }
}

#[async_trait::async_trait]
impl TileStore for LocalStore {
    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        let path = self.resolve(key);
        let data = tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(key.to_string())
            } else {
                StoreError::Io(e)
            }
        })?;
        Ok(Bytes::from(data))
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError> {
        let path = self.resolve(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &data).await?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.resolve(key);
        tokio::fs::remove_file(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(key.to_string())
            } else {
                StoreError::Io(e)
            }
        })
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let dir = self.resolve(prefix);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            if let Some(name) = entry.file_name().to_str() {
                let key = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}/{name}")
                };
                entries.push(key);
            }
        }
        Ok(entries)
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let path = self.resolve(key);
        Ok(path.exists())
    }
}
