//! tiletopia-cache: Fast tile caching layer
//!
//! Provides a `TileCache` trait and implementations for:
//! - **Redis** — distributed, shared cache across instances
//! - **Memcached** — high-throughput in-memory caching
//! - **LRU** — process-local in-memory cache (no network)
//!
//! The cache sits between the tile request handler and the
//! tile renderer/store, providing sub-millisecond responses
//! for hot tiles.

#[cfg(feature = "memcached")]
pub mod memcached;
#[cfg(feature = "redis")]
pub mod redis_cache;

pub mod lru_cache;

use bytes::Bytes;
use std::time::Duration;

/// Error type for cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache miss")]
    Miss,
    #[error("connection error: {0}")]
    Connection(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Result type for cache operations.
pub type CacheResult<T> = Result<T, CacheError>;

/// Tile cache key components.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TileCacheKey {
    /// Layer/tileset name.
    pub layer: String,
    /// Zoom level.
    pub z: u32,
    /// Column.
    pub x: u32,
    /// Row.
    pub y: u32,
    /// Tile format (e.g., "mvt", "png", "webp", "pbf").
    pub format: String,
}

impl TileCacheKey {
    /// Produces a string cache key.
    pub fn to_key_string(&self) -> String {
        format!(
            "tile:{}:{}/{}/{}:{}",
            self.layer, self.z, self.x, self.y, self.format
        )
    }
}

/// Core trait for tile cache backends.
#[async_trait::async_trait]
pub trait TileCache: Send + Sync {
    /// Get a cached tile. Returns `CacheError::Miss` if not cached.
    async fn get(&self, key: &TileCacheKey) -> CacheResult<Bytes>;

    /// Store a tile in the cache with a TTL.
    async fn put(&self, key: &TileCacheKey, data: Bytes, ttl: Duration) -> CacheResult<()>;

    /// Invalidate a specific tile.
    async fn invalidate(&self, key: &TileCacheKey) -> CacheResult<()>;

    /// Invalidate all tiles for a layer.
    async fn invalidate_layer(&self, layer: &str) -> CacheResult<()>;

    /// Flush the entire cache.
    async fn flush(&self) -> CacheResult<()>;

    /// Get cache statistics.
    async fn stats(&self) -> CacheStats;
}

/// Cache statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub size_bytes: u64,
    pub entry_count: u64,
}
