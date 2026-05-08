//! Amazon S3 storage backend.

use crate::{StoreError, TileStore};
use bytes::Bytes;

/// S3 storage backend.
pub struct S3Store {
    client: aws_sdk_s3::Client,
    bucket: String,
    prefix: String,
}

impl S3Store {
    /// Create a new S3 store. Uses default AWS credential chain.
    pub async fn new(bucket: String, prefix: String) -> Result<Self, StoreError> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_s3::Client::new(&config);
        Ok(Self {
            client,
            bucket,
            prefix,
        })
    }

    fn key(&self, path: &str) -> String {
        if self.prefix.is_empty() {
            path.to_string()
        } else {
            format!("{}/{}", self.prefix, path)
        }
    }
}

#[async_trait::async_trait]
impl TileStore for S3Store {
    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
            .map_err(|e| StoreError::Other(format!("S3 get error: {e}")))?;

        let data = result
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Other(format!("S3 body read error: {e}")))?;

        Ok(data.into_bytes())
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .body(data.into())
            .send()
            .await
            .map_err(|e| StoreError::Other(format!("S3 put error: {e}")))?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
            .map_err(|e| StoreError::Other(format!("S3 delete error: {e}")))?;

        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let full_prefix = self.key(prefix);
        let result = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&full_prefix)
            .send()
            .await
            .map_err(|e| StoreError::Other(format!("S3 list error: {e}")))?;

        let keys: Vec<String> = result
            .contents()
            .iter()
            .filter_map(|obj| obj.key())
            .map(|k| {
                k.strip_prefix(&format!("{}/", self.prefix))
                    .unwrap_or(k)
                    .to_string()
            })
            .collect();

        Ok(keys)
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}
