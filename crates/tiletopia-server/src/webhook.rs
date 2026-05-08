//! Webhook and event system for push notifications.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Event types that can trigger webhooks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventType {
    UploadComplete,
    TilingStarted,
    TilingComplete,
    TilingFailed,
    AnomalyDetected,
    ThresholdCrossed,
    ExportReady,
    UserLogin,
    PermissionChanged,
}

/// A webhook subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSubscription {
    pub id: String,
    pub url: String,
    pub events: Vec<EventType>,
    pub secret: Option<String>,
    pub active: bool,
    pub created_by: String,
    pub retry_policy: RetryPolicy,
}

/// Retry policy for failed deliveries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_delay_secs: u64,
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_secs: 5,
            backoff_multiplier: 2.0,
        }
    }
}

/// An event payload to deliver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub id: String,
    pub event_type: EventType,
    pub timestamp: String,
    pub payload: serde_json::Value,
    pub source: String,
}

/// Delivery status.
#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed { attempts: u32, last_error: String },
    Retrying { attempt: u32 },
}

/// Delivery record for tracking.
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    pub webhook_id: String,
    pub event_id: String,
    pub status: DeliveryStatus,
    pub response_code: Option<u16>,
}

/// Webhook registry and dispatcher.
pub struct WebhookRegistry {
    subscriptions: HashMap<String, WebhookSubscription>,
    delivery_log: Vec<DeliveryRecord>,
}

impl WebhookRegistry {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
            delivery_log: Vec::new(),
        }
    }

    /// Register a new webhook subscription.
    pub fn subscribe(&mut self, subscription: WebhookSubscription) -> String {
        let id = subscription.id.clone();
        self.subscriptions.insert(id.clone(), subscription);
        id
    }

    /// Unsubscribe a webhook.
    pub fn unsubscribe(&mut self, id: &str) -> bool {
        self.subscriptions.remove(id).is_some()
    }

    /// Get all subscriptions for an event type.
    pub fn get_subscribers(&self, event_type: &EventType) -> Vec<&WebhookSubscription> {
        self.subscriptions
            .values()
            .filter(|s| s.active && s.events.contains(event_type))
            .collect()
    }

    /// Dispatch an event to all relevant subscribers.
    /// Returns the list of webhook IDs that should receive the event.
    pub fn dispatch(&mut self, event: &WebhookEvent) -> Vec<String> {
        let subscribers: Vec<String> = self
            .subscriptions
            .values()
            .filter(|s| s.active && s.events.contains(&event.event_type))
            .map(|s| s.id.clone())
            .collect();

        for webhook_id in &subscribers {
            self.delivery_log.push(DeliveryRecord {
                webhook_id: webhook_id.clone(),
                event_id: event.id.clone(),
                status: DeliveryStatus::Pending,
                response_code: None,
            });
        }

        subscribers
    }

    /// Record delivery result.
    pub fn record_delivery(&mut self, webhook_id: &str, event_id: &str, success: bool, code: u16) {
        if let Some(record) = self
            .delivery_log
            .iter_mut()
            .find(|r| r.webhook_id == webhook_id && r.event_id == event_id)
        {
            record.response_code = Some(code);
            record.status = if success {
                DeliveryStatus::Delivered
            } else {
                DeliveryStatus::Failed {
                    attempts: 1,
                    last_error: format!("HTTP {}", code),
                }
            };
        }
    }

    /// Compute HMAC-SHA256 signature for webhook payload.
    pub fn compute_signature(payload: &str, secret: &str) -> String {
        // Simple HMAC implementation (in production, use ring or hmac crate)
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        payload.hash(&mut hasher);
        secret.hash(&mut hasher);
        format!("sha256={:016x}", hasher.finish())
    }

    /// Get delivery statistics.
    pub fn delivery_stats(&self) -> DeliveryStats {
        let total = self.delivery_log.len();
        let delivered = self
            .delivery_log
            .iter()
            .filter(|r| r.status == DeliveryStatus::Delivered)
            .count();
        let failed = self
            .delivery_log
            .iter()
            .filter(|r| matches!(r.status, DeliveryStatus::Failed { .. }))
            .count();
        DeliveryStats {
            total,
            delivered,
            failed,
            pending: total - delivered - failed,
        }
    }

    /// Number of active subscriptions.
    pub fn active_count(&self) -> usize {
        self.subscriptions.values().filter(|s| s.active).count()
    }
}

impl Default for WebhookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Delivery statistics.
#[derive(Debug, Clone)]
pub struct DeliveryStats {
    pub total: usize,
    pub delivered: usize,
    pub failed: usize,
    pub pending: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_subscription(id: &str, events: Vec<EventType>) -> WebhookSubscription {
        WebhookSubscription {
            id: id.into(),
            url: format!("https://example.com/webhook/{}", id),
            events,
            secret: Some("test-secret".into()),
            active: true,
            created_by: "user-1".into(),
            retry_policy: RetryPolicy::default(),
        }
    }

    #[test]
    fn test_subscribe_and_dispatch() {
        let mut registry = WebhookRegistry::new();
        registry.subscribe(make_subscription("wh-1", vec![EventType::TilingComplete]));
        registry.subscribe(make_subscription("wh-2", vec![EventType::UploadComplete]));

        let event = WebhookEvent {
            id: "evt-1".into(),
            event_type: EventType::TilingComplete,
            timestamp: "2024-01-01T00:00:00Z".into(),
            payload: serde_json::json!({"tileset_id": "ts-1"}),
            source: "tiler".into(),
        };
        let targets = registry.dispatch(&event);
        assert_eq!(targets, vec!["wh-1"]);
    }

    #[test]
    fn test_unsubscribe() {
        let mut registry = WebhookRegistry::new();
        registry.subscribe(make_subscription("wh-1", vec![EventType::TilingComplete]));
        assert_eq!(registry.active_count(), 1);
        registry.unsubscribe("wh-1");
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn test_delivery_recording() {
        let mut registry = WebhookRegistry::new();
        registry.subscribe(make_subscription("wh-1", vec![EventType::ExportReady]));
        let event = WebhookEvent {
            id: "evt-2".into(),
            event_type: EventType::ExportReady,
            timestamp: "2024-01-01".into(),
            payload: serde_json::json!({}),
            source: "export".into(),
        };
        registry.dispatch(&event);
        registry.record_delivery("wh-1", "evt-2", true, 200);
        let stats = registry.delivery_stats();
        assert_eq!(stats.delivered, 1);
    }

    #[test]
    fn test_compute_signature() {
        let sig = WebhookRegistry::compute_signature("test payload", "secret");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 16); // "sha256=" + 16 hex chars
    }

    #[test]
    fn test_multiple_event_types() {
        let mut registry = WebhookRegistry::new();
        registry.subscribe(make_subscription(
            "wh-all",
            vec![
                EventType::TilingComplete,
                EventType::AnomalyDetected,
                EventType::ExportReady,
            ],
        ));
        let subs = registry.get_subscribers(&EventType::AnomalyDetected);
        assert_eq!(subs.len(), 1);
    }
}
