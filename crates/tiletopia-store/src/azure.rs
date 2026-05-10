//! Azure Blob Storage backend.

use crate::{StoreError, TileStore};
use azure_storage::StorageCredentials;
use azure_storage_blobs::prelude::*;
use bytes::Bytes;

/// Azure Blob Storage backend.
pub struct AzureStore {
    client: ContainerClient,
    prefix: String,
}

impl AzureStore {
    /// Create a new Azure store using connection string authentication.
    pub fn new(
        account: &str,
        access_key: &str,
        container: &str,
        prefix: String,
    ) -> Result<Self, StoreError> {
        let credentials = StorageCredentials::access_key(account, access_key.to_string());
        let client = BlobServiceClient::new(account, credentials).container_client(container);

        Ok(Self { client, prefix })
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
impl TileStore for AzureStore {
    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        let blob = self.client.blob_client(&self.key(key));
        let response = blob.get_content().await.map_err(|e| {
            let msg = e.to_string();
            if msg.contains("BlobNotFound") || msg.contains("404") {
                StoreError::NotFound(key.to_string())
            } else {
                StoreError::Other(format!("Azure get error: {e}"))
            }
        })?;

        Ok(Bytes::from(response))
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError> {
        let blob = self.client.blob_client(&self.key(key));
        blob.put_block_blob(data)
            .await
            .map_err(|e| StoreError::Other(format!("Azure put error: {e}")))?;

        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let blob = self.client.blob_client(&self.key(key));
        blob.delete()
            .await
            .map_err(|e| StoreError::Other(format!("Azure delete error: {e}")))?;

        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let full_prefix = self.key(prefix);
        let mut keys = Vec::new();
        let mut stream = self
            .client
            .list_blobs()
            .prefix(full_prefix.clone())
            .into_stream();

        use futures::StreamExt;
        while let Some(result) = stream.next().await {
            let page = result.map_err(|e| StoreError::Other(format!("Azure list error: {e}")))?;
            for blob in page.blobs.blobs() {
                let name = blob
                    .name
                    .strip_prefix(&format!("{}/", self.prefix))
                    .unwrap_or(&blob.name)
                    .to_string();
                keys.push(name);
            }
        }

        Ok(keys)
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        let blob = self.client.blob_client(&self.key(key));
        match blob.get_properties().await {
            Ok(_) => Ok(true),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("BlobNotFound") || msg.contains("404") {
                    Ok(false)
                } else {
                    Err(StoreError::Other(format!("Azure exists error: {e}")))
                }
            }
        }
    }
}
