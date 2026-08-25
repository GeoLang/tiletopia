//! API keys: minting, resolving a presented key, and per-key rate limiting.
//!
//! A key exists as plaintext once, in the create response. What is stored is the
//! SHA-256 hex digest of the whole presented string, so the database never holds
//! anything that can be replayed. The rate limiter is process-local: a token
//! bucket and a daily counter per key id, fed by the credential path in
//! [`crate::auth`].

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::users::to_hex;

/// Prefix every key carries. It makes a leaked key recognizable in a log or a
/// commit, and it is what tells a presented digest apart from a presented key:
/// the digest has no prefix, so it never reaches a lookup.
pub const KEY_PREFIX: &str = "ttk_";

/// Bytes of OS randomness behind a key.
const KEY_RANDOM_BYTES: usize = 32;

/// Hex characters those bytes print as, and the length of the stored digest.
const KEY_HEX_CHARS: usize = KEY_RANDOM_BYTES * 2;

/// A key as it is stored: metadata plus the digest, never the key itself.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub name: String,
    /// SHA-256 hex of the plaintext key. Never serialized, so no handler can
    /// hand it out by returning this struct.
    #[serde(skip_serializing)]
    pub key_hash: String,
    pub permissions: Vec<Permission>,
    pub tier: RateLimitTier,
    /// JWT `sub` of the admin who created the key.
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
}

impl ApiKey {
    pub fn allows(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    pub fn expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expiry| expiry <= now)
    }

    pub fn rate_limit(&self) -> RateLimit {
        self.tier.limits()
    }
}

/// What a key is allowed to reach. Every variant maps to a route class in
/// [`crate::auth::route_access`]; there is no variant that reaches nothing, and
/// none that reaches an admin surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    /// Read-only catalog and dataset metadata: catalog, STAC, COG, features,
    /// geocoding.
    Read,
    /// Terrain and elevation compute.
    Terrain,
    /// Analysis compute: viewshed, flood, solar, terrain, geostatistics,
    /// geoprocessing.
    Analytics,
    /// Rendered output a caller downloads: static maps and analysis exports.
    Export,
}

impl Permission {
    pub const ALL: [Permission; 4] = [
        Permission::Read,
        Permission::Terrain,
        Permission::Analytics,
        Permission::Export,
    ];

    /// Exact match, so an unknown or misspelled permission is refused at create
    /// rather than landing in a scope. Same rule as
    /// [`crate::users::UserRole::from_claim`].
    pub fn from_name(name: &str) -> Option<Permission> {
        Permission::ALL
            .into_iter()
            .find(|permission| permission.name() == name)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Terrain => "terrain",
            Permission::Analytics => "analytics",
            Permission::Export => "export",
        }
    }
}

/// Rate limit tier a key is minted at. The row stores the tier, not the numbers,
/// so a stored key can never disagree with the tier it was sold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitTier {
    Free,
    Pro,
    Enterprise,
}

impl RateLimitTier {
    pub const ALL: [RateLimitTier; 3] = [
        RateLimitTier::Free,
        RateLimitTier::Pro,
        RateLimitTier::Enterprise,
    ];

    /// Exact match, so an unknown tier is refused instead of defaulting to one.
    pub fn from_name(name: &str) -> Option<RateLimitTier> {
        RateLimitTier::ALL
            .into_iter()
            .find(|tier| tier.name() == name)
    }

    pub fn name(&self) -> &'static str {
        match self {
            RateLimitTier::Free => "free",
            RateLimitTier::Pro => "pro",
            RateLimitTier::Enterprise => "enterprise",
        }
    }

    pub fn limits(&self) -> RateLimit {
        match self {
            RateLimitTier::Free => RateLimit {
                requests_per_second: 10,
                requests_per_day: 10_000,
            },
            RateLimitTier::Pro => RateLimit {
                requests_per_second: 100,
                requests_per_day: 500_000,
            },
            RateLimitTier::Enterprise => RateLimit {
                requests_per_second: 1000,
                requests_per_day: 0,
            },
        }
    }
}

/// What the limiter enforces, and nothing else.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RateLimit {
    pub requests_per_second: u32,
    /// 0 means unlimited.
    pub requests_per_day: u64,
}

/// A fresh key: the prefix plus 32 bytes of OS randomness in hex.
///
/// Panics if the OS random source cannot be read, rather than minting a key with
/// less entropy than advertised.
pub fn generate_key() -> String {
    use rand::TryRngCore;

    let mut bytes = [0u8; KEY_RANDOM_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random source is readable");
    format!("{KEY_PREFIX}{}", to_hex(&bytes))
}

/// The stored digest for a presented key, or `None` when the string is not
/// shaped like a key this server mints.
///
/// The shape check runs first, so a caller presenting the stored digest, a
/// bearer token, or anything else never reaches a database lookup.
pub fn hash_presented_key(presented: &str) -> Option<String> {
    let hex = presented.strip_prefix(KEY_PREFIX)?;
    let well_formed = hex.len() == KEY_HEX_CHARS
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !well_formed {
        return None;
    }
    Some(to_hex(&Sha256::digest(presented.as_bytes())))
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
    fn new(max_tokens: f64, refill_rate: f64, now: std::time::Instant) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            refill_rate,
            last_refill: now,
        }
    }

    fn try_consume(&mut self, now: std::time::Instant) -> bool {
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

/// Per-key rate limiting and usage counters. Process-local: it is reset by a
/// restart and not shared between replicas.
pub struct RateLimiter {
    buckets: RwLock<HashMap<Uuid, TokenBucket>>,
    daily_counts: RwLock<HashMap<Uuid, DailyCounter>>,
    clock: std::sync::RwLock<Clock>,
}

/// What the per-second buckets read as the current time.
pub type Clock = Arc<dyn Fn() -> std::time::Instant + Send + Sync>;

#[derive(Debug)]
struct DailyCounter {
    requests: u64,
    reset_at: DateTime<Utc>,
}

impl DailyCounter {
    fn new() -> Self {
        Self {
            requests: 0,
            reset_at: Utc::now() + chrono::Duration::days(1),
        }
    }

    fn maybe_reset(&mut self) {
        if Utc::now() >= self.reset_at {
            self.requests = 0;
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
            clock: std::sync::RwLock::new(Arc::new(std::time::Instant::now)),
        }
    }

    /// Replace the wall clock, so a test can hold time still and count tokens
    /// instead of racing the refill.
    pub fn set_clock(&self, clock: Clock) {
        *self.clock.write().expect("clock lock") = clock;
    }

    fn now(&self) -> std::time::Instant {
        (self.clock.read().expect("clock lock"))()
    }

    /// Spend one request against this key's limits.
    pub async fn check_rate_limit(&self, key_id: Uuid, rate_limit: &RateLimit) -> RateLimitResult {
        let now = self.now();
        let mut buckets = self.buckets.write().await;
        let bucket = buckets.entry(key_id).or_insert_with(|| {
            TokenBucket::new(
                rate_limit.requests_per_second as f64,
                rate_limit.requests_per_second as f64,
                now,
            )
        });

        if !bucket.try_consume(now) {
            return RateLimitResult::Denied {
                reason: "rate limit exceeded (per second)".into(),
                retry_after_ms: (1000.0 / rate_limit.requests_per_second as f64).ceil() as u64,
            };
        }
        drop(buckets);

        let mut counters = self.daily_counts.write().await;
        let counter = counters.entry(key_id).or_insert_with(DailyCounter::new);
        counter.maybe_reset();

        // counted for every tier, so usage is real even where the daily limit is
        // unlimited and there is nothing to compare against
        if rate_limit.requests_per_day > 0 && counter.requests >= rate_limit.requests_per_day {
            return RateLimitResult::Denied {
                reason: "daily request limit exceeded".into(),
                retry_after_ms: (counter.reset_at - Utc::now()).num_milliseconds().max(0) as u64,
            };
        }
        counter.requests += 1;

        RateLimitResult::Allowed
    }

    /// Today's usage for a key, or `None` when it has made no request since the
    /// process started.
    pub async fn get_usage(&self, key_id: Uuid) -> Option<UsageSnapshot> {
        let counters = self.daily_counts.read().await;
        counters.get(&key_id).map(|counter| UsageSnapshot {
            requests_today: counter.requests,
            resets_at: counter.reset_at,
        })
    }
}

/// Result of a rate limit check.
#[derive(Debug, Clone, Serialize)]
pub enum RateLimitResult {
    Allowed,
    Denied { reason: String, retry_after_ms: u64 },
}

/// Current usage for a key.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UsageSnapshot {
    pub requests_today: u64,
    pub resets_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_carries_the_prefix_and_32_bytes_of_hex() {
        let key = generate_key();
        assert!(key.starts_with(KEY_PREFIX));
        let hex = key.strip_prefix(KEY_PREFIX).unwrap();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn two_generated_keys_differ() {
        assert_ne!(generate_key(), generate_key());
    }

    #[test]
    fn a_key_hashes_to_64_hex_characters_that_are_not_the_key() {
        let key = generate_key();
        let hash = hash_presented_key(&key).unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!hash.contains(KEY_PREFIX));
        assert!(!key.contains(&hash));
        // same key, same digest, so a lookup by hash finds the row
        assert_eq!(hash_presented_key(&key).unwrap(), hash);
    }

    #[test]
    fn presenting_the_stored_hash_is_not_a_key() {
        let hash = hash_presented_key(&generate_key()).unwrap();
        assert_eq!(hash_presented_key(&hash), None);
    }

    #[test]
    fn malformed_keys_never_produce_a_digest() {
        let key = generate_key();
        let hex = key.strip_prefix(KEY_PREFIX).unwrap();
        for presented in [
            "",
            "ttk_",
            hex,                                    // the random half with no prefix
            &format!("ttk{hex}"),                   // no separator
            &format!("ttk_{}", &hex[..63]),         // one character short
            &format!("ttk_{hex}0"),                 // one character long
            &format!("ttk_{}", hex.to_uppercase()), // a case we never mint
            &format!("ttk_{}z", &hex[..63]),        // not hex
            &format!("Bearer {key}"),
            &key.replace("ttk_", "TTK_"),
        ] {
            assert_eq!(hash_presented_key(presented), None, "{presented}");
        }
    }

    #[test]
    fn permission_names_parse_exactly() {
        for permission in Permission::ALL {
            assert_eq!(Permission::from_name(permission.name()), Some(permission));
        }
        // near misses on a real one are not a permission, and neither is the
        // admin scope this enum deliberately does not have
        for name in ["", "admin", "write", "Read", "READ", " read", "read "] {
            assert_eq!(Permission::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn tier_names_parse_exactly() {
        for tier in RateLimitTier::ALL {
            assert_eq!(RateLimitTier::from_name(tier.name()), Some(tier));
        }
        for name in ["", "Free", "unlimited", "pro "] {
            assert_eq!(RateLimitTier::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn tiers_climb_and_enterprise_has_no_daily_cap() {
        let free = RateLimitTier::Free.limits();
        let pro = RateLimitTier::Pro.limits();
        let enterprise = RateLimitTier::Enterprise.limits();

        assert!(pro.requests_per_second > free.requests_per_second);
        assert!(enterprise.requests_per_second > pro.requests_per_second);
        assert_eq!(enterprise.requests_per_day, 0);
    }

    #[test]
    fn an_expiry_in_the_past_is_expired() {
        let now = Utc::now();
        let key = |expires_at| ApiKey {
            id: Uuid::new_v4(),
            name: "k".into(),
            key_hash: "0".repeat(64),
            permissions: vec![Permission::Read],
            tier: RateLimitTier::Free,
            created_by: "admin".into(),
            created_at: now,
            last_used_at: None,
            expires_at,
            revoked: false,
        };
        assert!(!key(None).expired_at(now));
        assert!(!key(Some(now + chrono::Duration::seconds(1))).expired_at(now));
        assert!(key(Some(now - chrono::Duration::seconds(1))).expired_at(now));
        // the instant it expires, it is expired
        assert!(key(Some(now)).expired_at(now));
    }

    #[test]
    fn a_key_hash_never_serializes() {
        let key = ApiKey {
            id: Uuid::new_v4(),
            name: "listing".into(),
            key_hash: "a".repeat(64),
            permissions: vec![Permission::Read],
            tier: RateLimitTier::Pro,
            created_by: "admin".into(),
            created_at: Utc::now(),
            last_used_at: None,
            expires_at: None,
            revoked: false,
        };
        let json = serde_json::to_string(&key).unwrap();
        assert!(!json.contains(&key.key_hash), "{json}");
        assert!(!json.contains("key_hash"), "{json}");
        assert!(json.contains("\"read\""), "{json}");
        assert!(json.contains("\"pro\""), "{json}");
    }

    #[tokio::test]
    async fn the_limiter_allows_a_first_request() {
        let limiter = RateLimiter::new();
        let result = limiter
            .check_rate_limit(Uuid::new_v4(), &RateLimitTier::Free.limits())
            .await;
        assert!(matches!(result, RateLimitResult::Allowed));
    }

    #[tokio::test]
    async fn the_limiter_denies_a_burst_past_the_bucket_with_retry_timing() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let limit = RateLimit {
            requests_per_second: 2,
            requests_per_day: 1000,
        };

        // the bucket starts full
        for _ in 0..2 {
            assert!(matches!(
                limiter.check_rate_limit(key_id, &limit).await,
                RateLimitResult::Allowed
            ));
        }
        match limiter.check_rate_limit(key_id, &limit).await {
            RateLimitResult::Denied {
                reason,
                retry_after_ms,
            } => {
                assert!(reason.contains("per second"), "{reason}");
                assert_eq!(retry_after_ms, 500);
            }
            RateLimitResult::Allowed => panic!("a third request inside one second was allowed"),
        }
    }

    #[tokio::test]
    async fn the_daily_limit_denies_with_the_time_until_it_resets() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let limit = RateLimit {
            requests_per_second: 100,
            requests_per_day: 2,
        };

        for _ in 0..2 {
            assert!(matches!(
                limiter.check_rate_limit(key_id, &limit).await,
                RateLimitResult::Allowed
            ));
        }
        match limiter.check_rate_limit(key_id, &limit).await {
            RateLimitResult::Denied {
                reason,
                retry_after_ms,
            } => {
                assert!(reason.contains("daily"), "{reason}");
                assert!(retry_after_ms > 0, "{retry_after_ms}");
            }
            RateLimitResult::Allowed => panic!("a third request past a cap of two was allowed"),
        }
    }

    #[tokio::test]
    async fn usage_counts_every_tier_including_the_uncapped_one() {
        let limiter = RateLimiter::new();
        let key_id = Uuid::new_v4();
        let enterprise = RateLimitTier::Enterprise.limits();

        assert!(limiter.get_usage(key_id).await.is_none());
        for _ in 0..3 {
            limiter.check_rate_limit(key_id, &enterprise).await;
        }
        assert_eq!(limiter.get_usage(key_id).await.unwrap().requests_today, 3);
    }

    #[tokio::test]
    async fn usage_is_per_key() {
        let limiter = RateLimiter::new();
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        let limits = RateLimitTier::Pro.limits();

        limiter.check_rate_limit(one, &limits).await;
        limiter.check_rate_limit(one, &limits).await;
        limiter.check_rate_limit(two, &limits).await;

        assert_eq!(limiter.get_usage(one).await.unwrap().requests_today, 2);
        assert_eq!(limiter.get_usage(two).await.unwrap().requests_today, 1);
    }
}
