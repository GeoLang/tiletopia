//! tiletopia-store: pluggable storage backends
//!
//! Abstraction over local filesystem, S3, GCS, and Azure Blob storage
//! for reading/writing tiles and assets.

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
}
