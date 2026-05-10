//! Google Cloud Storage backend.

use crate::{StoreError, TileStore};
use bytes::Bytes;
use cloud_storage::Client;

/// Google Cloud Storage backend.
pub struct GcsStore {
    client: Client,
    bucket: String,
    prefix: String,
}

impl GcsStore {
    /// Create a new GCS store. Uses default application credentials.
    pub fn new(bucket: String, prefix: String) -> Result<Self, StoreError> {
        let client =
            Client::default();
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
impl TileStore for GcsStore {
    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        let data = self
            .client
            .object()
            .download(&self.bucket, &self.key(key))
            .await
            .map_err(|e| {
                if e.to_string().contains("404") || e.to_string().contains("Not Found") {
                    StoreError::NotFound(key.to_string())
                } else {
                    StoreError::Other(format!("GCS get error: {e}"))
                }
            })?;

        Ok(Bytes::from(data))
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError> {
        self.client
            .object()
            .create(
                &self.bucket,
                data.to_vec(),
                &self.key(key),
                "application/octet-stream",
            )
            .await
            .map_err(|e| StoreError::Other(format!("GCS put error: {e}")))?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.client
            .object()
            .delete(&self.bucket, &self.key(key))
            .await
            .map_err(|e| StoreError::Other(format!("GCS delete error: {e}")))?;

        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let full_prefix = self.key(prefix);
        let objects = self
            .client
            .object()
            .list(&self.bucket, cloud_storage::ListRequest {
                prefix: Some(full_prefix.clone()),
                ..Default::default()
            })
            .await
            .map_err(|e| StoreError::Other(format!("GCS list error: {e}")))?;

        use futures::StreamExt;
        let mut keys = Vec::new();
        tokio::pin!(objects);
        while let Some(result) = objects.next().await {
            let page = result
                .map_err(|e| StoreError::Other(format!("GCS list error: {e}")))?;
            for obj in page.items {
                let name = obj
                    .name
                    .strip_prefix(&format!("{}/", self.prefix))
                    .unwrap_or(&obj.name)
                    .to_string();
                keys.push(name);
            }
        }

        Ok(keys)
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        match self
            .client
            .object()
            .read(&self.bucket, &self.key(key))
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                if e.to_string().contains("404") || e.to_string().contains("Not Found") {
                    Ok(false)
                } else {
                    Err(StoreError::Other(format!("GCS exists error: {e}")))
                }
            }
        }
    }
}
