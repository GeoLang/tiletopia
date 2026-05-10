//! Federation — connect multiple TileTopia instances into a unified dataset.
//!
//! Enables distributed tiling across sites with transparent proxy,
//! unified search, and cross-instance tile serving.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A remote TileTopia instance in the federation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationPeer {
    /// Unique peer ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Base URL of the remote instance.
    pub url: String,
    /// API key for authentication (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Whether this peer is currently reachable.
    #[serde(default)]
    pub healthy: bool,
    /// Last health check timestamp.
    pub last_check: Option<String>,
    /// Geographic region this peer covers (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<[f64; 4]>, // [west, south, east, north]
}

/// Federation registry managing all connected peers.
pub struct FederationRegistry {
    peers: Arc<RwLock<HashMap<String, FederationPeer>>>,
}

impl FederationRegistry {
    pub fn new() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new peer.
    pub async fn add_peer(&self, peer: FederationPeer) {
        let mut peers = self.peers.write().await;
        peers.insert(peer.id.clone(), peer);
    }

    /// Remove a peer.
    pub async fn remove_peer(&self, peer_id: &str) -> bool {
        let mut peers = self.peers.write().await;
        peers.remove(peer_id).is_some()
    }

    /// List all registered peers.
    pub async fn list_peers(&self) -> Vec<FederationPeer> {
        let peers = self.peers.read().await;
        peers.values().cloned().collect()
    }

    /// Get a specific peer.
    pub async fn get_peer(&self, peer_id: &str) -> Option<FederationPeer> {
        let peers = self.peers.read().await;
        peers.get(peer_id).cloned()
    }

    /// Health-check all peers (updates healthy status).
    pub async fn health_check_all(&self) {
        let peer_list: Vec<FederationPeer> = {
            let peers = self.peers.read().await;
            peers.values().cloned().collect()
        };

        for peer in peer_list {
            let healthy = check_peer_health(&peer.url).await;
            let mut peers = self.peers.write().await;
            if let Some(p) = peers.get_mut(&peer.id) {
                p.healthy = healthy;
                p.last_check = Some(chrono::Utc::now().to_rfc3339());
            }
        }
    }

    /// Find peers that cover a geographic region.
    pub async fn find_peers_for_region(&self, bounds: [f64; 4]) -> Vec<FederationPeer> {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|p| {
                p.healthy && p.region.map(|r| regions_overlap(r, bounds)).unwrap_or(true) // Peers without region are global
            })
            .cloned()
            .collect()
    }
}

impl Default for FederationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A federated query that spans multiple instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedQuery {
    /// Search text (asset names, tags).
    pub query: Option<String>,
    /// Geographic bounds filter.
    pub bounds: Option<[f64; 4]>,
    /// Maximum results per peer.
    pub limit: Option<usize>,
}

/// A federated search result from a remote peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedResult {
    pub peer_id: String,
    pub peer_name: String,
    pub assets: Vec<FederatedAsset>,
}

/// A remote asset reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedAsset {
    pub id: String,
    pub name: String,
    pub asset_type: String,
    /// Full URL to proxy through.
    pub tileset_url: String,
}

/// Execute a federated search across all healthy peers.
pub async fn federated_search(
    registry: &FederationRegistry,
    query: &FederatedQuery,
) -> Vec<FederatedResult> {
    let peers = if let Some(bounds) = query.bounds {
        registry.find_peers_for_region(bounds).await
    } else {
        registry
            .list_peers()
            .await
            .into_iter()
            .filter(|p| p.healthy)
            .collect()
    };

    let mut results = Vec::new();
    for peer in peers {
        match query_peer(&peer, query).await {
            Ok(assets) => {
                results.push(FederatedResult {
                    peer_id: peer.id.clone(),
                    peer_name: peer.name.clone(),
                    assets,
                });
            }
            Err(e) => {
                tracing::warn!("Federation query to {} failed: {}", peer.name, e);
            }
        }
    }
    results
}

// --- Internal helpers ---

async fn check_peer_health(base_url: &str) -> bool {
    let url = format!("{}/api/v1/health", base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            tracing::debug!("Health check failed for {base_url}: {e}");
            false
        }
    }
}

async fn query_peer(
    peer: &FederationPeer,
    query: &FederatedQuery,
) -> Result<Vec<FederatedAsset>, String> {
    let url = format!("{}/api/v1/assets", peer.url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.get(&url);
    if let Some(key) = &peer.api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    if let Some(q) = &query.query {
        req = req.query(&[("q", q.as_str())]);
    }
    if let Some(bounds) = &query.bounds {
        req = req.query(&[("bbox", &format!("{},{},{},{}", bounds[0], bounds[1], bounds[2], bounds[3]))]);
    }
    if let Some(limit) = query.limit {
        req = req.query(&[("limit", &limit.to_string())]);
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Vec<FederatedAsset>>()
        .await
        .map_err(|e| e.to_string())
}

fn regions_overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    // [west, south, east, north]
    !(a[2] < b[0] || b[2] < a[0] || a[3] < b[1] || b[3] < a[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_and_list_peers() {
        let registry = FederationRegistry::new();
        registry
            .add_peer(FederationPeer {
                id: "peer1".to_string(),
                name: "Site A".to_string(),
                url: "https://site-a.example.com".to_string(),
                api_key: None,
                healthy: true,
                last_check: None,
                region: Some([-180.0, -90.0, 0.0, 90.0]),
            })
            .await;
        registry
            .add_peer(FederationPeer {
                id: "peer2".to_string(),
                name: "Site B".to_string(),
                url: "https://site-b.example.com".to_string(),
                api_key: Some("secret".to_string()),
                healthy: true,
                last_check: None,
                region: Some([0.0, -90.0, 180.0, 90.0]),
            })
            .await;
        let peers = registry.list_peers().await;
        assert_eq!(peers.len(), 2);
    }

    #[tokio::test]
    async fn test_remove_peer() {
        let registry = FederationRegistry::new();
        registry
            .add_peer(FederationPeer {
                id: "p1".to_string(),
                name: "Test".to_string(),
                url: "http://localhost".to_string(),
                api_key: None,
                healthy: true,
                last_check: None,
                region: None,
            })
            .await;
        assert!(registry.remove_peer("p1").await);
        assert!(!registry.remove_peer("p1").await);
    }

    #[tokio::test]
    async fn test_find_peers_for_region() {
        let registry = FederationRegistry::new();
        registry
            .add_peer(FederationPeer {
                id: "us".to_string(),
                name: "US West".to_string(),
                url: "https://us.example.com".to_string(),
                api_key: None,
                healthy: true,
                last_check: None,
                region: Some([-130.0, 25.0, -60.0, 50.0]),
            })
            .await;
        registry
            .add_peer(FederationPeer {
                id: "eu".to_string(),
                name: "Europe".to_string(),
                url: "https://eu.example.com".to_string(),
                api_key: None,
                healthy: true,
                last_check: None,
                region: Some([-10.0, 35.0, 40.0, 70.0]),
            })
            .await;

        // Query for a US location
        let results = registry
            .find_peers_for_region([-100.0, 30.0, -90.0, 40.0])
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "us");
    }

    #[test]
    fn test_regions_overlap() {
        assert!(regions_overlap(
            [0.0, 0.0, 10.0, 10.0],
            [5.0, 5.0, 15.0, 15.0]
        ));
        assert!(!regions_overlap(
            [0.0, 0.0, 10.0, 10.0],
            [20.0, 20.0, 30.0, 30.0]
        ));
    }
}
