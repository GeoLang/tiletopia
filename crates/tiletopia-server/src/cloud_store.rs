//! Cloud storage abstraction — local filesystem or S3-compatible object store.
//!
//! Tiles and assets can be stored either locally (for dev/self-hosted) or on
//! S3/MinIO (for cloud deployments). The storage backend is chosen at startup
//! based on environment variables.

use std::path::{Path, PathBuf};

/// Storage backend trait.
pub trait TileStore: Send + Sync {
    /// Store a tile blob.
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError>;
    /// Retrieve a tile blob.
    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError>;
    /// Check if a tile exists.
    fn exists(&self, key: &str) -> Result<bool, StoreError>;
    /// Delete a tile.
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// List tiles with a prefix.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
}

/// Storage errors.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("S3 error: {0}")]
    S3(String),
}

/// Local filesystem storage backend.
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl TileStore for LocalStore {
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.root.join(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let path = self.root.join(key);
        if !path.exists() {
            return Err(StoreError::NotFound(key.to_string()));
        }
        Ok(std::fs::read(&path)?)
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.root.join(key).exists())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.root.join(key);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let dir = self.root.join(prefix);
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && let Some(name) = entry.file_name().to_str()
            {
                keys.push(format!("{}/{}", prefix, name));
            }
        }
        Ok(keys)
    }
}

/// S3-compatible storage backend (works with AWS S3, MinIO, R2, etc.)
///
/// Uses environment variables:
/// - `AWS_S3_BUCKET` — bucket name
/// - `AWS_REGION` — region (default: us-east-1)
/// - `AWS_ENDPOINT_URL` — custom endpoint for MinIO/R2 (optional)
/// - Standard AWS credential chain (env vars, ~/.aws/credentials, IAM role)
pub struct S3Store {
    bucket: String,
    region: String,
    endpoint: Option<String>,
}

impl S3Store {
    /// Create from environment variables.
    pub fn from_env() -> Option<Self> {
        let bucket = std::env::var("AWS_S3_BUCKET").ok()?;
        let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let endpoint = std::env::var("AWS_ENDPOINT_URL").ok();
        Some(Self {
            bucket,
            region,
            endpoint,
        })
    }

    /// Get the bucket name.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Get the region.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Get custom endpoint (for MinIO/R2).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Build AWS SDK config, honoring custom endpoint if set.
    async fn aws_config(&self) -> aws_config::SdkConfig {
        let mut loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).region(aws_config::Region::new(self.region.clone()));
        if let Some(ep) = &self.endpoint {
            loader = loader.endpoint_url(ep);
        }
        loader.load().await
    }
}

impl TileStore for S3Store {
    fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let rt = tokio::runtime::Handle::current();
        let bucket = self.bucket.clone();
        let key = key.to_string();
        let body = data.to_vec();
        rt.block_on(async {
            let config = self.aws_config().await;
            let client = aws_sdk_s3::Client::new(&config);
            client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(body.into())
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        })
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        let rt = tokio::runtime::Handle::current();
        let bucket = self.bucket.clone();
        let key = key.to_string();
        rt.block_on(async {
            let config = self.aws_config().await;
            let client = aws_sdk_s3::Client::new(&config);
            let resp = client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("NoSuchKey") || msg.contains("not found") {
                        StoreError::NotFound(key.clone())
                    } else {
                        StoreError::S3(msg)
                    }
                })?;
            let data = resp
                .body
                .collect()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?
                .into_bytes()
                .to_vec();
            Ok(data)
        })
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let rt = tokio::runtime::Handle::current();
        let bucket = self.bucket.clone();
        let key = key.to_string();
        rt.block_on(async {
            let config = self.aws_config().await;
            let client = aws_sdk_s3::Client::new(&config);
            match client
                .head_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("NotFound") || msg.contains("404") || msg.contains("NoSuchKey")
                    {
                        Ok(false)
                    } else {
                        Err(StoreError::S3(msg))
                    }
                }
            }
        })
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let rt = tokio::runtime::Handle::current();
        let bucket = self.bucket.clone();
        let key = key.to_string();
        rt.block_on(async {
            let config = self.aws_config().await;
            let client = aws_sdk_s3::Client::new(&config);
            client
                .delete_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            Ok(())
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let rt = tokio::runtime::Handle::current();
        let bucket = self.bucket.clone();
        let prefix = prefix.to_string();
        rt.block_on(async {
            let config = self.aws_config().await;
            let client = aws_sdk_s3::Client::new(&config);
            let resp = client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(&prefix)
                .send()
                .await
                .map_err(|e| StoreError::S3(e.to_string()))?;
            let keys: Vec<String> = resp
                .contents()
                .iter()
                .filter_map(|obj| obj.key().map(|k| k.to_string()))
                .collect();
            Ok(keys)
        })
    }
}

/// Create the appropriate store based on environment.
/// Returns S3Store if AWS_S3_BUCKET is set, otherwise LocalStore.
pub fn create_store(data_dir: impl AsRef<Path>) -> Box<dyn TileStore> {
    if let Some(s3) = S3Store::from_env() {
        tracing::info!(
            bucket = %s3.bucket(),
            region = %s3.region(),
            endpoint = ?s3.endpoint(),
            "Using S3 storage backend"
        );
        Box::new(s3)
    } else {
        tracing::info!(path = %data_dir.as_ref().display(), "Using local filesystem storage");
        Box::new(LocalStore::new(data_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_store_crud() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path());

        // Put
        store.put("tiles/0/0/0.terrain", b"test data").unwrap();
        assert!(store.exists("tiles/0/0/0.terrain").unwrap());

        // Get
        let data = store.get("tiles/0/0/0.terrain").unwrap();
        assert_eq!(data, b"test data");

        // List
        let keys = store.list("tiles/0/0").unwrap();
        assert_eq!(keys.len(), 1);
        assert!(keys[0].contains("0.terrain"));

        // Delete
        store.delete("tiles/0/0/0.terrain").unwrap();
        assert!(!store.exists("tiles/0/0/0.terrain").unwrap());
    }

    #[test]
    fn test_local_store_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path());
        let err = store.get("nonexistent").unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[test]
    fn test_create_store_defaults_to_local() {
        // Without AWS_S3_BUCKET env var, should return local store
        // (Don't remove env vars in tests — not safe in Rust 2024)
        // Just verify that create_store with no S3 env returns a working store
        let dir = tempfile::tempdir().unwrap();
        let store = create_store(dir.path());
        assert!(!store.exists("nonexistent").unwrap());
    }
}
