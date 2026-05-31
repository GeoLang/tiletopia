//! Process-local LRU tile cache.

use bytes::Bytes;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{CacheError, CacheResult, CacheStats, TileCache, TileCacheKey};

struct CachedTile {
    data: Bytes,
    expires: Instant,
}

/// In-memory LRU tile cache.
pub struct LruTileCache {
    cache: Mutex<LruCache<String, CachedTile>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl LruTileCache {
    /// Create a new LRU cache with the given maximum number of entries.
    pub fn new(capacity: usize) -> Self {
        Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(10000).unwrap()),
            )),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }
}

#[async_trait::async_trait]
impl TileCache for LruTileCache {
    async fn get(&self, key: &TileCacheKey) -> CacheResult<Bytes> {
        let cache_key = key.to_key_string();
        let mut cache = self.cache.lock().unwrap();
        match cache.get(&cache_key) {
            Some(entry) if entry.expires > Instant::now() => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Ok(entry.data.clone())
            }
            Some(_) => {
                // Expired — remove it
                cache.pop(&cache_key);
                self.misses.fetch_add(1, Ordering::Relaxed);
                Err(CacheError::Miss)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Err(CacheError::Miss)
            }
        }
    }

    async fn put(&self, key: &TileCacheKey, data: Bytes, ttl: Duration) -> CacheResult<()> {
        let cache_key = key.to_key_string();
        let mut cache = self.cache.lock().unwrap();
        cache.put(
            cache_key,
            CachedTile {
                data,
                expires: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn invalidate(&self, key: &TileCacheKey) -> CacheResult<()> {
        let cache_key = key.to_key_string();
        let mut cache = self.cache.lock().unwrap();
        cache.pop(&cache_key);
        Ok(())
    }

    async fn invalidate_layer(&self, layer: &str) -> CacheResult<()> {
        let prefix = format!("tile:{layer}:");
        let mut cache = self.cache.lock().unwrap();
        let keys_to_remove: Vec<String> = cache
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys_to_remove {
            cache.pop(&key);
        }
        Ok(())
    }

    async fn flush(&self) -> CacheResult<()> {
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
        Ok(())
    }

    async fn stats(&self) -> CacheStats {
        let cache = self.cache.lock().unwrap();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            size_bytes: 0,
            entry_count: cache.len() as u64,
        }
    }
}
