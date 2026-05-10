//! Hybrid hot/cold storage with automatic tiering.
//!
//! Wraps a "hot" local store and a "cold" remote store, promoting objects
//! on read and offering time-based demotion.

use crate::{StoreError, TileStore};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Hybrid store combining a fast local ("hot") tier with a remote ("cold") tier.
pub struct HybridStore {
    hot: Arc<dyn TileStore>,
    cold: Arc<dyn TileStore>,
    access_log: Arc<RwLock<HashMap<String, Instant>>>,
}

impl HybridStore {
    pub fn new(hot: Arc<dyn TileStore>, cold: Arc<dyn TileStore>) -> Self {
        Self {
            hot,
            cold,
            access_log: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Promote a key from cold to hot storage.
    pub async fn promote(&self, key: &str) -> Result<(), StoreError> {
        let data = self.cold.get(key).await?;
        self.hot.put(key, data).await?;
        self.cold.delete(key).await?;
        self.access_log.write().await.insert(key.to_string(), Instant::now());
        Ok(())
    }

    /// Demote a key from hot to cold storage.
    pub async fn demote(&self, key: &str) -> Result<(), StoreError> {
        let data = self.hot.get(key).await?;
        self.cold.put(key, data).await?;
        self.hot.delete(key).await?;
        self.access_log.write().await.remove(key);
        Ok(())
    }

    /// Move all hot keys not accessed within `max_age` to cold storage.
    pub async fn tier_cold(&self, max_age: Duration) -> Result<Vec<String>, StoreError> {
        let now = Instant::now();
        let stale_keys: Vec<String> = {
            let log = self.access_log.read().await;
            log.iter()
                .filter(|(_, last)| now.duration_since(**last) > max_age)
                .map(|(k, _)| k.clone())
                .collect()
        };

        let mut demoted = Vec::new();
        for key in &stale_keys {
            match self.demote(key).await {
                Ok(()) => demoted.push(key.clone()),
                Err(StoreError::NotFound(_)) => {
                    // Already gone from hot — just clean up the access log
                    self.access_log.write().await.remove(key);
                }
                Err(e) => {
                    tracing::warn!("tier_cold: failed to demote {key}: {e}");
                }
            }
        }

        Ok(demoted)
    }

    async fn touch(&self, key: &str) {
        self.access_log.write().await.insert(key.to_string(), Instant::now());
    }
}

#[async_trait::async_trait]
impl TileStore for HybridStore {
    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        // Try hot first
        match self.hot.get(key).await {
            Ok(data) => {
                self.touch(key).await;
                return Ok(data);
            }
            Err(StoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }

        // Fall back to cold, promote on hit
        let data = self.cold.get(key).await?;
        if let Err(e) = self.hot.put(key, data.clone()).await {
            tracing::warn!("hybrid: failed to promote {key} to hot: {e}");
        }
        self.touch(key).await;
        Ok(data)
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<(), StoreError> {
        self.hot.put(key, data).await?;
        self.touch(key).await;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let hot_result = self.hot.delete(key).await;
        let cold_result = self.cold.delete(key).await;
        self.access_log.write().await.remove(key);

        match (&hot_result, &cold_result) {
            (Err(StoreError::NotFound(_)), Err(StoreError::NotFound(_))) => {
                Err(StoreError::NotFound(key.to_string()))
            }
            _ => {
                // At least one succeeded
                Ok(())
            }
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let hot_keys = self.hot.list(prefix).await?;
        let cold_keys = self.cold.list(prefix).await?;

        let mut all: Vec<String> = hot_keys;
        for k in cold_keys {
            if !all.contains(&k) {
                all.push(k);
            }
        }
        all.sort();
        Ok(all)
    }

    async fn exists(&self, key: &str) -> Result<bool, StoreError> {
        if self.hot.exists(key).await? {
            return Ok(true);
        }
        self.cold.exists(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalStore;
    use tempfile::tempdir;

    fn make_stores() -> (Arc<dyn TileStore>, Arc<dyn TileStore>, tempfile::TempDir, tempfile::TempDir) {
        let hot_dir = tempdir().unwrap();
        let cold_dir = tempdir().unwrap();
        let hot: Arc<dyn TileStore> = Arc::new(LocalStore::new(hot_dir.path().to_path_buf()));
        let cold: Arc<dyn TileStore> = Arc::new(LocalStore::new(cold_dir.path().to_path_buf()));
        (hot, cold, hot_dir, cold_dir)
    }

    #[tokio::test]
    async fn test_put_get_from_hot() {
        let (hot, cold, _hd, _cd) = make_stores();
        let store = HybridStore::new(hot, cold);

        store.put("tile/1", Bytes::from("hello")).await.unwrap();
        let data = store.get("tile/1").await.unwrap();
        assert_eq!(data, Bytes::from("hello"));
    }

    #[tokio::test]
    async fn test_promote_on_cold_read() {
        let (hot, cold, _hd, _cd) = make_stores();

        // Put directly into cold
        cold.put("tile/2", Bytes::from("from-cold")).await.unwrap();

        let store = HybridStore::new(hot.clone(), cold);
        let data = store.get("tile/2").await.unwrap();
        assert_eq!(data, Bytes::from("from-cold"));

        // Should now exist in hot
        let hot_data = hot.get("tile/2").await.unwrap();
        assert_eq!(hot_data, Bytes::from("from-cold"));
    }

    #[tokio::test]
    async fn test_delete_from_both() {
        let (hot, cold, _hd, _cd) = make_stores();
        hot.put("tile/3", Bytes::from("h")).await.unwrap();
        cold.put("tile/3", Bytes::from("c")).await.unwrap();

        let store = HybridStore::new(hot.clone(), cold.clone());
        store.delete("tile/3").await.unwrap();

        assert!(!hot.exists("tile/3").await.unwrap());
        assert!(!cold.exists("tile/3").await.unwrap());
    }

    #[tokio::test]
    async fn test_list_union() {
        let (hot, cold, _hd, _cd) = make_stores();
        hot.put("a/1", Bytes::from("h")).await.unwrap();
        cold.put("a/2", Bytes::from("c")).await.unwrap();

        let store = HybridStore::new(hot, cold);
        let keys = store.list("a").await.unwrap();
        assert!(keys.contains(&"a/1".to_string()));
        assert!(keys.contains(&"a/2".to_string()));
    }

    #[tokio::test]
    async fn test_demote() {
        let (hot, cold, _hd, _cd) = make_stores();
        let store = HybridStore::new(hot.clone(), cold.clone());
        store.put("tile/4", Bytes::from("data")).await.unwrap();

        store.demote("tile/4").await.unwrap();
        assert!(!hot.exists("tile/4").await.unwrap());
        assert!(cold.exists("tile/4").await.unwrap());
    }

    #[tokio::test]
    async fn test_tier_cold() {
        let (hot, cold, _hd, _cd) = make_stores();
        let store = HybridStore::new(hot.clone(), cold.clone());
        store.put("tile/5", Bytes::from("old")).await.unwrap();

        // Force instant staleness
        let demoted = store.tier_cold(Duration::from_secs(0)).await.unwrap();
        assert!(demoted.contains(&"tile/5".to_string()));
        assert!(!hot.exists("tile/5").await.unwrap());
        assert!(cold.exists("tile/5").await.unwrap());
    }
}
