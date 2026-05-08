//! API key management and rate limiting.
//!
//! Every customer gets an API key. Keys have:
//! - Configurable rate limits (requests/second, requests/day)
//! - Scoped permissions (read-only, read-write, admin)
//! - Usage tracking (for billing)
//! - Revocation support

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// API key with metadata and rate limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub key_hash: String, // SHA-256 of the actual key (never store plaintext)
    pub name: String,
    pub owner_id: Uuid,
    pub permissions: Vec<Permission>,
    pub rate_limit: RateLimit,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}

/// Permission scope for an API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Permission {
    /// Read assets, tiles, catalog
    Read,
    /// Upload and modify assets
    Write,
    /// Manage users, keys, settings
    Admin,
    /// Access terrain generation APIs
    Terrain,
    /// Access analytics/measurement APIs
    Analytics,
    /// Access export APIs
    Export,
}

/// Rate limiting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    /// Max requests per second
    pub requests_per_second: u32,
    /// Max requests per day (0 = unlimited)
    pub requests_per_day: u64,
    /// Max upload bytes per day
    pub upload_bytes_per_day: u64,
    /// Max tile requests per day
    pub tile_requests_per_day: u64,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            requests_per_second: 100,
            requests_per_day: 100_000,
            upload_bytes_per_day: 10 * 1024 * 1024 * 1024, // 10 GB
            tile_requests_per_day: 1_000_000,
        }
    }
}

/// Predefined rate limit tiers.
impl RateLimit {
    pub fn free_tier() -> Self {
        Self {
            requests_per_second: 10,
            requests_per_day: 10_000,
            upload_bytes_per_day: 1024 * 1024 * 1024, // 1 GB
            tile_requests_per_day: 100_000,
        }
    }

    pub fn pro_tier() -> Self {
        Self {
            requests_per_second: 100,
            requests_per_day: 500_000,
            upload_bytes_per_day: 50 * 1024 * 1024 * 1024, // 50 GB
            tile_requests_per_day: 5_000_000,
        }
    }

    pub fn enterprise_tier() -> Self {
        Self {
            requests_per_second: 1000,
            requests_per_day: 0,      // unlimited
            upload_bytes_per_day: 0,  // unlimited
            tile_requests_per_day: 0, // unlimited
        }
    }
}

/// Token bucket for rate limiting.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: std::time::Instant,
}

impl TokenBucket {
    fn new(max_tokens: f64, refill_rate: f64) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate,
            last_refill: std::time::Instant::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rate limiter state.
pub struct RateLimiter {
    buckets: RwLock<HashMap<Uuid, TokenBucket>>,
    daily_counts: RwLock<HashMap<Uuid, DailyCounter>>,
}

#[derive(Debug)]
struct DailyCounter {
    requests: u64,
    tile_requests: u64,
    upload_bytes: u64,
    reset_at: DateTime<Utc>,
}

impl DailyCounter {
    fn new() -> Self {
        Self {
            requests: 0,
            tile_requests: 0,
            upload_bytes: 0,
            reset_at: Utc::now() + chrono::Duration::days(1),
        }
    }

    fn maybe_reset(&mut self) {
        if Utc::now() >= self.reset_at {
            self.requests = 0;
            self.tile_requests = 0;
            self.upload_bytes = 0;
            self.reset_at = Utc::now() + chrono::Duration::days(1);
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: RwLock::new(HashMap::new()),
            daily_counts: RwLock::new(HashMap::new()),
        }
    }

    /// Check if a request is allowed for the given key.
    pub async fn check_rate_limit(&self, key_id: Uuid, rate_limit: &RateLimit) -> RateLimitResult {
        // Check per-second rate
        let mut buckets = self.buckets.write().await;
        let bucket = buckets.entry(key_id).or_insert_with(|| {
            TokenBucket::new(
                rate_limit.requests_per_second as f64,
                rate_limit.requests_per_second as f64,
            )
        });

        if !bucket.try_consume() {
            return RateLimitResult::Denied {
                reason: "Rate limit exceeded (per-second)".into(),
                retry_after_ms: (1000.0 / rate_limit.requests_per_second as f64) as u64,
            };
        }
        drop(buckets);

        // Check daily limit
        if rate_limit.requests_per_day > 0 {
            let mut counters = self.daily_counts.write().await;
            let counter = counters.entry(key_id).or_insert_with(DailyCounter::new);
            counter.maybe_reset();

            if counter.requests >= rate_limit.requests_per_day {
                return RateLimitResult::Denied {
                    reason: "Daily request limit exceeded".into(),
                    retry_after_ms: 0,
                };
            }
            counter.requests += 1;
        }

        RateLimitResult::Allowed
    }

    /// Record tile request for daily tracking.
    pub async fn record_tile_request(&self, key_id: Uuid) {
        let mut counters = self.daily_counts.write().await;
        if let Some(counter) = counters.get_mut(&key_id) {
            counter.tile_requests += 1;
        }
    }

    /// Record upload bytes for daily tracking.
    pub async fn record_upload(&self, key_id: Uuid, bytes: u64) {
        let mut counters = self.daily_counts.write().await;
        if let Some(counter) = counters.get_mut(&key_id) {
            counter.upload_bytes += bytes;
        }
    }

    /// Get current usage for a key.
    pub async fn get_usage(&self, key_id: Uuid) -> Option<UsageSnapshot> {
        let counters = self.daily_counts.read().await;
        counters.get(&key_id).map(|c| UsageSnapshot {
            requests_today: c.requests,
            tile_requests_today: c.tile_requests,
            upload_bytes_today: c.upload_bytes,
            resets_at: c.reset_at,
        })
    }
}

/// Result of a rate limit check.
#[derive(Debug, Clone, Serialize)]
pub enum RateLimitResult {
    Allowed,
    Denied { reason: String, retry_after_ms: u64 },
}

/// Current usage snapshot for a key.
#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub requests_today: u64,
    pub tile_requests_today: u64,
    pub upload_bytes_today: u64,
    pub resets_at: DateTime<Utc>,
}

/// API key store (in-memory with seeded demo data).
pub struct ApiKeyStore {
    keys: Arc<RwLock<Vec<ApiKey>>>,
    pub rate_limiter: RateLimiter,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyStore {
    pub fn new() -> Self {
        Self {
            keys: Arc::new(RwLock::new(Self::demo_keys())),
            rate_limiter: RateLimiter::new(),
        }
    }

    /// List all keys for an owner.
    pub async fn list_keys(&self, owner_id: Option<Uuid>) -> Vec<ApiKey> {
        let keys = self.keys.read().await;
        match owner_id {
            Some(id) => keys.iter().filter(|k| k.owner_id == id).cloned().collect(),
            None => keys.clone(),
        }
    }

    /// Get a key by its hash (for authentication).
    pub async fn get_by_hash(&self, hash: &str) -> Option<ApiKey> {
        let keys = self.keys.read().await;
        keys.iter()
            .find(|k| k.key_hash == hash && !k.revoked)
            .cloned()
    }

    /// Revoke a key.
    pub async fn revoke(&self, key_id: Uuid) -> bool {
        let mut keys = self.keys.write().await;
        if let Some(key) = keys.iter_mut().find(|k| k.id == key_id) {
            key.revoked = true;
            true
        } else {
            false
        }
    }

    fn demo_keys() -> Vec<ApiKey> {
        vec![
            ApiKey {
                id: Uuid::new_v4(),
                key_hash: "sha256:demo_production_key_hash".into(),
                name: "Production API Key".into(),
                owner_id: Uuid::new_v4(),
                permissions: vec![Permission::Read, Permission::Write, Permission::Terrain],
                rate_limit: RateLimit::pro_tier(),
                created_at: Utc::now() - chrono::Duration::days(30),
                last_used_at: Some(Utc::now() - chrono::Duration::hours(1)),
                expires_at: None,
                revoked: false,
            },
            ApiKey {
                id: Uuid::new_v4(),
                key_hash: "sha256:demo_readonly_key_hash".into(),
                name: "Frontend Read-Only Key".into(),
                owner_id: Uuid::new_v4(),
                permissions: vec![Permission::Read],
                rate_limit: RateLimit::free_tier(),
                created_at: Utc::now() - chrono::Duration::days(7),
                last_used_at: Some(Utc::now() - chrono::Duration::minutes(5)),
                expires_at: Some(Utc::now() + chrono::Duration::days(90)),
                revoked: false,
            },
            ApiKey {
                id: Uuid::new_v4(),
                key_hash: "sha256:demo_admin_key_hash".into(),
                name: "CI/CD Admin Key".into(),
                owner_id: Uuid::new_v4(),
                permissions: vec![
                    Permission::Read,
                    Permission::Write,
                    Permission::Admin,
                    Permission::Terrain,
                    Permission::Analytics,
                    Permission::Export,
                ],
                rate_limit: RateLimit::enterprise_tier(),
                created_at: Utc::now() - chrono::Duration::days(60),
                last_used_at: Some(Utc::now() - chrono::Duration::hours(12)),
                expires_at: None,
                revoked: false,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_allows_requests() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let limit = RateLimit::default();

        let result = limiter.check_rate_limit(key_id, &limit).await;
        assert!(matches!(result, RateLimitResult::Allowed));
    }

    #[tokio::test]
    async fn test_rate_limiter_denies_burst() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let limit = RateLimit {
            requests_per_second: 2,
            requests_per_day: 1000,
            upload_bytes_per_day: 0,
            tile_requests_per_day: 0,
        };

        // First 2 should succeed (bucket starts full)
        assert!(matches!(
            limiter.check_rate_limit(key_id, &limit).await,
            RateLimitResult::Allowed
        ));
        assert!(matches!(
            limiter.check_rate_limit(key_id, &limit).await,
            RateLimitResult::Allowed
        ));
        // Third should be denied
        assert!(matches!(
            limiter.check_rate_limit(key_id, &limit).await,
            RateLimitResult::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn test_api_key_store() {
        let store = ApiKeyStore::new();
        let keys = store.list_keys(None).await;
        assert_eq!(keys.len(), 3);
        assert!(keys.iter().all(|k| !k.revoked));
    }

    #[tokio::test]
    async fn test_revoke_key() {
        let store = ApiKeyStore::new();
        let keys = store.list_keys(None).await;
        let key_id = keys[0].id;
        assert!(store.revoke(key_id).await);
        let keys = store.list_keys(None).await;
        assert!(keys.iter().find(|k| k.id == key_id).unwrap().revoked);
    }

    #[test]
    fn test_rate_limit_tiers() {
        let free = RateLimit::free_tier();
        let pro = RateLimit::pro_tier();
        let enterprise = RateLimit::enterprise_tier();

        assert!(pro.requests_per_second > free.requests_per_second);
        assert!(enterprise.requests_per_second > pro.requests_per_second);
        assert_eq!(enterprise.requests_per_day, 0); // unlimited
    }
}
