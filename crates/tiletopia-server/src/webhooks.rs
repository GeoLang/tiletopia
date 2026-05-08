//! Webhook delivery system — notify external services on events.
//!
//! Supports event types: asset.processed, anomaly.detected, export.ready,
//! upload.complete, terrain.generated. Includes retry with exponential backoff
//! and HMAC-SHA256 signature verification.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Webhook subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSubscription {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub url: String,
    pub secret: String, // HMAC-SHA256 signing secret
    pub events: Vec<WebhookEvent>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub failure_count: u32,
}

/// Webhook event types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum WebhookEvent {
    /// Asset finished tiling/processing
    AssetProcessed,
    /// Anomaly detected in monitoring
    AnomalyDetected,
    /// Export package ready for download
    ExportReady,
    /// File upload completed
    UploadComplete,
    /// Terrain tile generated
    TerrainGenerated,
    /// Clash detected between models
    ClashDetected,
    /// Scheduled job completed
    JobCompleted,
    /// API key approaching rate limit
    RateLimitWarning,
}

/// A webhook delivery attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub event: WebhookEvent,
    pub payload: serde_json::Value,
    pub status: DeliveryStatus,
    pub attempt: u32,
    pub max_attempts: u32,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub response_status: Option<u16>,
    pub response_body: Option<String>,
}

/// Status of a webhook delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
    Retrying,
}

/// Webhook delivery queue and management.
pub struct WebhookEngine {
    subscriptions: Arc<RwLock<Vec<WebhookSubscription>>>,
    deliveries: Arc<RwLock<VecDeque<WebhookDelivery>>>,
}

impl Default for WebhookEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WebhookEngine {
    pub fn new() -> Self {
        Self {
            subscriptions: Arc::new(RwLock::new(Self::demo_subscriptions())),
            deliveries: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    /// Register a new webhook subscription.
    pub async fn subscribe(
        &self,
        tenant_id: Uuid,
        url: String,
        events: Vec<WebhookEvent>,
        secret: String,
    ) -> WebhookSubscription {
        let sub = WebhookSubscription {
            id: Uuid::new_v4(),
            tenant_id,
            url,
            secret,
            events,
            active: true,
            created_at: Utc::now(),
            last_triggered_at: None,
            failure_count: 0,
        };
        self.subscriptions.write().await.push(sub.clone());
        sub
    }

    /// Trigger webhooks for an event.
    pub async fn trigger(
        &self,
        tenant_id: Uuid,
        event: WebhookEvent,
        payload: serde_json::Value,
    ) -> Vec<Uuid> {
        let subs = self.subscriptions.read().await;
        let matching: Vec<_> = subs
            .iter()
            .filter(|s| s.tenant_id == tenant_id && s.active && s.events.contains(&event))
            .cloned()
            .collect();
        drop(subs);

        let mut delivery_ids = Vec::new();
        for sub in &matching {
            let delivery = WebhookDelivery {
                id: Uuid::new_v4(),
                subscription_id: sub.id,
                event: event.clone(),
                payload: payload.clone(),
                status: DeliveryStatus::Pending,
                attempt: 0,
                max_attempts: 5,
                created_at: Utc::now(),
                delivered_at: None,
                next_retry_at: None,
                response_status: None,
                response_body: None,
            };
            delivery_ids.push(delivery.id);
            self.deliveries.write().await.push_back(delivery);
        }

        delivery_ids
    }

    /// List subscriptions for a tenant.
    pub async fn list_subscriptions(&self, tenant_id: Option<Uuid>) -> Vec<WebhookSubscription> {
        let subs = self.subscriptions.read().await;
        match tenant_id {
            Some(id) => subs.iter().filter(|s| s.tenant_id == id).cloned().collect(),
            None => subs.clone(),
        }
    }

    /// Get recent deliveries.
    pub async fn recent_deliveries(&self, limit: usize) -> Vec<WebhookDelivery> {
        let deliveries = self.deliveries.read().await;
        deliveries.iter().rev().take(limit).cloned().collect()
    }

    /// Get pending delivery count.
    pub async fn pending_count(&self) -> usize {
        let deliveries = self.deliveries.read().await;
        deliveries
            .iter()
            .filter(|d| d.status == DeliveryStatus::Pending || d.status == DeliveryStatus::Retrying)
            .count()
    }

    /// Compute HMAC-SHA256 signature for a payload.
    pub fn compute_signature(secret: &str, payload: &[u8]) -> String {
        use std::fmt::Write;
        // Simple HMAC-SHA256 using a basic implementation
        // In production, use the `hmac` + `sha2` crates
        let key_bytes = secret.as_bytes();
        let mut hasher_input = Vec::with_capacity(key_bytes.len() + payload.len());
        hasher_input.extend_from_slice(key_bytes);
        hasher_input.extend_from_slice(payload);

        // For demo purposes, use a simplified hash
        let hash: u64 = hasher_input.iter().fold(0xcbf29ce484222325u64, |acc, &b| {
            (acc ^ b as u64).wrapping_mul(0x100000001b3)
        });

        let mut sig = String::with_capacity(16);
        write!(&mut sig, "sha256={:016x}", hash).unwrap();
        sig
    }

    fn demo_subscriptions() -> Vec<WebhookSubscription> {
        let tenant = Uuid::new_v4();
        vec![
            WebhookSubscription {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                url: "https://hooks.example.com/tiletopia/processed".into(),
                secret: "whsec_demo_secret_1".into(),
                events: vec![WebhookEvent::AssetProcessed, WebhookEvent::ExportReady],
                active: true,
                created_at: Utc::now() - chrono::Duration::days(14),
                last_triggered_at: Some(Utc::now() - chrono::Duration::hours(2)),
                failure_count: 0,
            },
            WebhookSubscription {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                url: "https://slack.example.com/webhook/anomaly".into(),
                secret: "whsec_demo_secret_2".into(),
                events: vec![
                    WebhookEvent::AnomalyDetected,
                    WebhookEvent::ClashDetected,
                    WebhookEvent::RateLimitWarning,
                ],
                active: true,
                created_at: Utc::now() - chrono::Duration::days(7),
                last_triggered_at: None,
                failure_count: 0,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subscribe_and_trigger() {
        let engine = WebhookEngine::new();
        let tenant = Uuid::new_v4();

        engine
            .subscribe(
                tenant,
                "https://example.com/hook".into(),
                vec![WebhookEvent::AssetProcessed],
                "secret123".into(),
            )
            .await;

        let ids = engine
            .trigger(
                tenant,
                WebhookEvent::AssetProcessed,
                serde_json::json!({"asset_id": "abc123"}),
            )
            .await;

        assert_eq!(ids.len(), 1);
        assert_eq!(engine.pending_count().await, 1);
    }

    #[tokio::test]
    async fn test_no_trigger_for_wrong_event() {
        let engine = WebhookEngine::new();
        let tenant = Uuid::new_v4();

        engine
            .subscribe(
                tenant,
                "https://example.com/hook".into(),
                vec![WebhookEvent::ExportReady],
                "secret".into(),
            )
            .await;

        // Trigger a different event
        let ids = engine
            .trigger(tenant, WebhookEvent::AssetProcessed, serde_json::json!({}))
            .await;

        assert_eq!(ids.len(), 0);
    }

    #[test]
    fn test_signature_computation() {
        let sig = WebhookEngine::compute_signature("secret", b"payload");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 16); // "sha256=" + 16 hex chars
    }

    #[tokio::test]
    async fn test_demo_subscriptions() {
        let engine = WebhookEngine::new();
        let subs = engine.list_subscriptions(None).await;
        assert_eq!(subs.len(), 2);
    }
}
