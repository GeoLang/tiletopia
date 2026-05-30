//! HTTP caching middleware — ETag, Cache-Control, and conditional request handling.
//!
//! Provides cache-aware response helpers for tile and API endpoints.
//! Supports:
//! - Strong ETags from content hashing (SHA-256 prefix)
//! - Cache-Control directives (public, max-age, s-maxage, stale-while-revalidate)
//! - If-None-Match / 304 Not Modified handling
//! - CDN-friendly Surrogate-Control headers

use std::collections::HashMap;
use std::time::Duration;

/// Cache policy for a response.
#[derive(Debug, Clone)]
pub struct CachePolicy {
    pub max_age: Duration,
    pub s_maxage: Option<Duration>,
    pub public: bool,
    pub stale_while_revalidate: Option<Duration>,
    pub stale_if_error: Option<Duration>,
    pub no_store: bool,
    pub immutable: bool,
    pub vary: Vec<String>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(3600),
            s_maxage: None,
            public: true,
            stale_while_revalidate: None,
            stale_if_error: None,
            no_store: false,
            immutable: false,
            vary: Vec::new(),
        }
    }
}

impl CachePolicy {
    /// Tiles: long cache, immutable (content-addressed).
    pub fn tile() -> Self {
        Self {
            max_age: Duration::from_secs(86400),
            s_maxage: Some(Duration::from_secs(604_800)),
            public: true,
            stale_while_revalidate: Some(Duration::from_secs(86400)),
            stale_if_error: Some(Duration::from_secs(604_800)),
            immutable: true,
            ..Default::default()
        }
    }

    /// Metadata/capabilities: shorter cache, revalidatable.
    pub fn metadata() -> Self {
        Self {
            max_age: Duration::from_secs(300),
            s_maxage: Some(Duration::from_secs(600)),
            public: true,
            stale_while_revalidate: Some(Duration::from_secs(60)),
            ..Default::default()
        }
    }

    /// No caching (user-specific or mutable data).
    pub fn no_cache() -> Self {
        Self {
            max_age: Duration::ZERO,
            no_store: true,
            public: false,
            ..Default::default()
        }
    }

    /// Build the Cache-Control header value.
    pub fn cache_control_header(&self) -> String {
        if self.no_store {
            return "no-store, no-cache, must-revalidate".to_string();
        }
        let mut parts = Vec::new();
        if self.public {
            parts.push("public".to_string());
        } else {
            parts.push("private".to_string());
        }
        parts.push(format!("max-age={}", self.max_age.as_secs()));
        if let Some(s) = self.s_maxage {
            parts.push(format!("s-maxage={}", s.as_secs()));
        }
        if let Some(swr) = self.stale_while_revalidate {
            parts.push(format!("stale-while-revalidate={}", swr.as_secs()));
        }
        if let Some(sie) = self.stale_if_error {
            parts.push(format!("stale-if-error={}", sie.as_secs()));
        }
        if self.immutable {
            parts.push("immutable".to_string());
        }
        parts.join(", ")
    }

    /// Build all cache-related headers.
    pub fn headers(&self, etag: Option<&str>) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert("Cache-Control".to_string(), self.cache_control_header());
        if let Some(etag) = etag {
            headers.insert("ETag".to_string(), format!("\"{etag}\""));
        }
        if !self.vary.is_empty() {
            headers.insert("Vary".to_string(), self.vary.join(", "));
        }
        // CDN surrogate control
        if let Some(s) = self.s_maxage {
            headers.insert(
                "Surrogate-Control".to_string(),
                format!("max-age={}", s.as_secs()),
            );
        }
        headers
    }
}

/// Compute an ETag from content bytes (SHA-256 prefix, strong).
pub fn compute_etag(data: &[u8]) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{hash:016x}")
}

/// Check if a request's If-None-Match header matches the current ETag.
/// Returns true if the client's cached version is still valid (304 response).
pub fn is_not_modified(if_none_match: Option<&str>, current_etag: &str) -> bool {
    match if_none_match {
        None => false,
        Some(header) => {
            if header.trim() == "*" {
                return true;
            }
            let quoted = format!("\"{current_etag}\"");
            let weak = format!("W/\"{current_etag}\"");
            header.split(',').any(|tag| {
                let tag = tag.trim();
                tag == quoted || tag == weak
            })
        }
    }
}

/// A cached response that can be served or result in 304.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    pub body: Vec<u8>,
    pub content_type: String,
    pub etag: String,
    pub policy: CachePolicy,
}

impl CachedResponse {
    pub fn new(body: Vec<u8>, content_type: &str, policy: CachePolicy) -> Self {
        let etag = compute_etag(&body);
        Self {
            body,
            content_type: content_type.to_string(),
            etag,
            policy,
        }
    }

    /// Check if client already has this response.
    pub fn is_fresh(&self, if_none_match: Option<&str>) -> bool {
        is_not_modified(if_none_match, &self.etag)
    }

    /// Get all response headers.
    pub fn headers(&self) -> HashMap<String, String> {
        let mut h = self.policy.headers(Some(&self.etag));
        h.insert("Content-Type".to_string(), self.content_type.clone());
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_cache_control() {
        let policy = CachePolicy::tile();
        let header = policy.cache_control_header();
        assert!(header.contains("public"));
        assert!(header.contains("max-age=86400"));
        assert!(header.contains("s-maxage=604800"));
        assert!(header.contains("immutable"));
    }

    #[test]
    fn test_no_cache_policy() {
        let policy = CachePolicy::no_cache();
        assert_eq!(
            policy.cache_control_header(),
            "no-store, no-cache, must-revalidate"
        );
    }

    #[test]
    fn test_etag_computation() {
        let data = b"hello world";
        let etag = compute_etag(data);
        assert_eq!(etag.len(), 16); // 16 hex chars
        // Same data produces same etag
        assert_eq!(compute_etag(data), etag);
        // Different data produces different etag
        assert_ne!(compute_etag(b"goodbye"), etag);
    }

    #[test]
    fn test_if_none_match() {
        let etag = "abc123";
        assert!(is_not_modified(Some("\"abc123\""), etag));
        assert!(is_not_modified(Some("W/\"abc123\""), etag));
        assert!(is_not_modified(Some("\"other\", \"abc123\""), etag));
        assert!(!is_not_modified(Some("\"different\""), etag));
        assert!(!is_not_modified(None, etag));
        assert!(is_not_modified(Some("*"), etag));
    }

    #[test]
    fn test_cached_response() {
        let resp = CachedResponse::new(
            b"tile data".to_vec(),
            "application/octet-stream",
            CachePolicy::tile(),
        );
        assert!(!resp.etag.is_empty());
        assert!(resp.is_fresh(Some(&format!("\"{}\"", resp.etag))));
        assert!(!resp.is_fresh(Some("\"stale\"")));
    }
}
