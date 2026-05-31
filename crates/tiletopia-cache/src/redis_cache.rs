//! Redis-based distributed tile cache.

use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::{CacheError, CacheResult, CacheStats, TileCache, TileCacheKey};

/// Redis tile cache configuration.
pub struct RedisCacheConfig {
    /// Redis connection URL (redis://host:port/db).
    pub url: String,
    /// Key prefix to namespace tiles.
    pub prefix: String,
}

/// Redis-backed tile cache.
pub struct RedisCache {
    conn: ConnectionManager,
    prefix: String,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl RedisCache {
    /// Connect to Redis.
    pub async fn new(config: RedisCacheConfig) -> CacheResult<Self> {
        let client =
            redis::Client::open(config.url).map_err(|e| CacheError::Connection(e.to_string()))?;
        let conn = ConnectionManager::new(client)
            .await
            .map_err(|e| CacheError::Connection(e.to_string()))?;
        Ok(Self {
            conn,
            prefix: config.prefix,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    fn prefixed_key(&self, key: &TileCacheKey) -> String {
        format!("{}:{}", self.prefix, key.to_key_string())
    }
}

#[async_trait::async_trait]
impl TileCache for RedisCache {
    async fn get(&self, key: &TileCacheKey) -> CacheResult<Bytes> {
        let redis_key = self.prefixed_key(key);
        let mut conn = self.conn.clone();
        let result: Option<Vec<u8>> = conn
            .get(&redis_key)
            .await
            .map_err(|e| CacheError::Internal(e.to_string()))?;

        match result {
            Some(data) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Ok(Bytes::from(data))
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Err(CacheError::Miss)
            }
        }
    }

    async fn put(&self, key: &TileCacheKey, data: Bytes, ttl: Duration) -> CacheResult<()> {
        let redis_key = self.prefixed_key(key);
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, String>(&redis_key, data.to_vec(), ttl.as_secs())
            .await
            .map_err(|e| CacheError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn invalidate(&self, key: &TileCacheKey) -> CacheResult<()> {
        let redis_key = self.prefixed_key(key);
        let mut conn = self.conn.clone();
        conn.del::<_, i64>(&redis_key)
            .await
            .map_err(|e| CacheError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn invalidate_layer(&self, layer: &str) -> CacheResult<()> {
        let pattern = format!("{}:tile:{}:*", self.prefix, layer);
        let mut conn = self.conn.clone();
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(&pattern)
            .query_async(&mut conn)
            .await
            .map_err(|e| CacheError::Internal(e.to_string()))?;

        if !keys.is_empty() {
            conn.del::<_, i64>(keys)
                .await
                .map_err(|e| CacheError::Internal(e.to_string()))?;
        }
        Ok(())
    }

    async fn flush(&self) -> CacheResult<()> {
        let pattern = format!("{}:*", self.prefix);
        let mut conn = self.conn.clone();
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(&pattern)
            .query_async(&mut conn)
            .await
            .map_err(|e| CacheError::Internal(e.to_string()))?;

        if !keys.is_empty() {
            conn.del::<_, i64>(keys)
                .await
                .map_err(|e| CacheError::Internal(e.to_string()))?;
        }
        Ok(())
    }

    async fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            size_bytes: 0, // Redis doesn't easily expose per-prefix size
            entry_count: 0,
        }
    }
}
