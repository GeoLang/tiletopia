//! Webhook subscriptions and their delivery.
//!
//! A subscription is a row in SQLite: a target URL, the event types it wants,
//! and an HMAC-SHA256 signing secret. Pending deliveries are in process memory,
//! so a restart drops whatever has not been delivered and nothing is queued for
//! an event that happens while the process is down. That queue has a cap, past
//! which an event is dropped rather than queued.
//!
//! The signing secret is stored as it was generated, not as a digest the way
//! [`crate::api_keys`] stores a key: signing a payload needs the secret itself.
//! The create response is the only place it is handed back, and nothing
//! serializes it after that.
//!
//! A subscription URL is a request this server makes on a caller's behalf, so an
//! editor can aim one at an address only this host can reach. Delivery does not
//! follow redirects, sends only the payload this server built, and never reads
//! the response body.

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db::Database;
use crate::users::to_hex;

/// Prefix every signing secret carries, so a leaked one is recognizable in a
/// log or a commit.
pub const SECRET_PREFIX: &str = "whsec_";

/// Bytes of OS randomness behind a signing secret.
const SECRET_RANDOM_BYTES: usize = 32;

/// Attempts one event gets on one subscription, first try included. After the
/// last one the delivery is dropped: nothing re-queues it, and the failure is
/// what [`WebhookQueue::recent_deliveries`] reports.
pub const MAX_ATTEMPTS: u32 = 4;

/// Delay before the second attempt. Each further attempt doubles it, so the
/// four attempts land at 0, 30, 90 and 210 seconds.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(30);

/// How long a receiver has to answer before the attempt counts as failed.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the worker looks for a delivery that is due.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Finished deliveries kept for the history route. Oldest are dropped.
const HISTORY_LIMIT: usize = 200;

/// Deliveries that may be waiting at once. Past this an event is dropped
/// instead of queued.
const MAX_PENDING: usize = 10_000;

/// Header carrying `sha256=<hex>` over the request body.
pub const SIGNATURE_HEADER: &str = "X-TileTopia-Signature";

/// Header carrying the event type name.
pub const EVENT_HEADER: &str = "X-TileTopia-Event";

/// Header carrying the delivery id, so a receiver can tell a retry from a new
/// event.
pub const DELIVERY_HEADER: &str = "X-TileTopia-Delivery";

/// What a subscription can ask for. Every variant is emitted by a real code
/// path: the two job ones from [`crate::job_queue`] when a tiling job settles,
/// and the third from the asset delete route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookEvent {
    JobCompleted,
    JobFailed,
    AssetDeleted,
}

impl WebhookEvent {
    pub const ALL: [WebhookEvent; 3] = [
        WebhookEvent::JobCompleted,
        WebhookEvent::JobFailed,
        WebhookEvent::AssetDeleted,
    ];

    /// Exact match, so an unknown or misspelled event is refused at subscribe
    /// rather than landing in a subscription that never fires. Same rule as
    /// [`crate::api_keys::Permission::from_name`].
    pub fn from_name(name: &str) -> Option<WebhookEvent> {
        WebhookEvent::ALL.into_iter().find(|e| e.name() == name)
    }

    pub fn name(&self) -> &'static str {
        match self {
            WebhookEvent::JobCompleted => "job.completed",
            WebhookEvent::JobFailed => "job.failed",
            WebhookEvent::AssetDeleted => "asset.deleted",
        }
    }
}

impl Serialize for WebhookEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

/// A subscription as it is stored.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookSubscription {
    pub id: Uuid,
    pub url: String,
    pub events: Vec<WebhookEvent>,
    /// The HMAC key. Never serialized, so no handler can hand it out by
    /// returning this struct.
    #[serde(skip_serializing)]
    pub secret: String,
    /// JWT `sub` of the editor who created it.
    pub created_by: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// A finished delivery. One row per event per subscription, whatever the
/// outcome.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookDelivery {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub event: WebhookEvent,
    pub url: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub response_status: Option<u16>,
    /// Why the last attempt failed. Never the response body: that is not read.
    pub error: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    Delivered,
    Failed,
}

/// A delivery waiting for its next attempt.
#[derive(Debug, Clone)]
struct PendingDelivery {
    id: Uuid,
    subscription_id: Uuid,
    event: WebhookEvent,
    url: String,
    secret: String,
    body: Vec<u8>,
    attempts: u32,
    next_attempt_at: DateTime<Utc>,
    queued_at: DateTime<Utc>,
}

/// A fresh signing secret: the prefix plus 32 bytes of OS randomness in hex.
///
/// Panics if the OS random source cannot be read, rather than signing with less
/// entropy than advertised.
pub fn generate_secret() -> String {
    use rand::TryRngCore;

    let mut bytes = [0u8; SECRET_RANDOM_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random source is readable");
    format!("{SECRET_PREFIX}{}", to_hex(&bytes))
}

/// `sha256=<hex>` of the HMAC-SHA256 of `payload` under `secret`. A receiver
/// recomputes this over the raw request body.
pub fn signature(secret: &str, payload: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload);
    format!("sha256={}", to_hex(&mac.finalize().into_bytes()))
}

/// How long to wait before attempt number `attempts + 1`, or `None` when
/// `attempts` was the last one this delivery gets.
fn retry_delay(attempts: u32, base: Duration) -> Option<Duration> {
    if attempts >= MAX_ATTEMPTS {
        return None;
    }
    Some(base * 2u32.pow(attempts.saturating_sub(1)))
}

/// Queues events for the subscriptions that want them and delivers them.
pub struct WebhookQueue {
    db: Arc<Database>,
    client: reqwest::Client,
    pending: Mutex<VecDeque<PendingDelivery>>,
    history: Mutex<VecDeque<WebhookDelivery>>,
    retry_base: Duration,
}

impl WebhookQueue {
    pub fn new(db: Arc<Database>) -> Self {
        Self::with_retry_base(db, RETRY_BASE_DELAY)
    }

    /// The same queue with a different delay before the second attempt, which
    /// is what the delivery tests use to exhaust the retries in milliseconds.
    pub fn with_retry_base(db: Arc<Database>, retry_base: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DELIVERY_TIMEOUT)
            // a receiver must not be able to bounce a signed request at another
            // host
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the reqwest client builds with rustls");
        Self {
            db,
            client,
            pending: Mutex::new(VecDeque::new()),
            history: Mutex::new(VecDeque::new()),
            retry_base,
        }
    }

    /// Start the background worker loop.
    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.deliver_due().await;
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
    }

    /// Queue this event for every active subscription that names it. Reading the
    /// subscriptions is the only thing that can fail, and it only logs: an event
    /// is a side effect of work that already happened.
    pub async fn emit(&self, event: WebhookEvent, data: serde_json::Value) {
        let subscriptions = match self.db.list_webhook_subscriptions().await {
            Ok(subscriptions) => subscriptions,
            Err(error) => {
                tracing::error!(
                    "reading webhook subscriptions for {} failed: {error}",
                    event.name()
                );
                return;
            }
        };

        let queued_at = Utc::now();
        let body = serde_json::to_vec(&serde_json::json!({
            "event": event.name(),
            "occurred_at": queued_at.to_rfc3339(),
            "data": data,
        }))
        .expect("the event payload serializes");

        let mut pending = self.pending.lock().await;
        for subscription in subscriptions
            .iter()
            .filter(|s| s.active && s.events.contains(&event))
        {
            // a receiver that is down holds its deliveries here for as long as
            // the retries last, so the queue is capped rather than growing with
            // every event the server produces
            if pending.len() >= MAX_PENDING {
                tracing::error!(
                    "webhook queue is at {MAX_PENDING} pending deliveries, dropping {} for {}",
                    event.name(),
                    subscription.url
                );
                break;
            }
            pending.push_back(PendingDelivery {
                id: Uuid::new_v4(),
                subscription_id: subscription.id,
                event,
                url: subscription.url.clone(),
                secret: subscription.secret.clone(),
                body: body.clone(),
                attempts: 0,
                next_attempt_at: queued_at,
                queued_at,
            });
        }
    }

    /// Attempt every delivery whose next attempt is due.
    pub async fn deliver_due(&self) {
        let now = Utc::now();
        let due = {
            let mut pending = self.pending.lock().await;
            let mut due = Vec::new();
            let mut later = VecDeque::with_capacity(pending.len());
            while let Some(delivery) = pending.pop_front() {
                if delivery.next_attempt_at <= now {
                    due.push(delivery);
                } else {
                    later.push_back(delivery);
                }
            }
            *pending = later;
            due
        };

        for delivery in due {
            self.attempt(delivery).await;
        }
    }

    async fn attempt(&self, mut delivery: PendingDelivery) {
        let result = self
            .client
            .post(&delivery.url)
            .header("Content-Type", "application/json")
            .header(
                SIGNATURE_HEADER,
                signature(&delivery.secret, &delivery.body),
            )
            .header(EVENT_HEADER, delivery.event.name())
            .header(DELIVERY_HEADER, delivery.id.to_string())
            .body(delivery.body.clone())
            .send()
            .await;
        delivery.attempts += 1;

        // the response body is never read, so a receiver cannot make this
        // process buffer whatever it feels like answering
        let failure = match result {
            Ok(response) if response.status().is_success() => {
                self.finish(
                    &delivery,
                    DeliveryStatus::Delivered,
                    Some(response.status().as_u16()),
                    None,
                )
                .await;
                return;
            }
            Ok(response) => (
                Some(response.status().as_u16()),
                format!("receiver answered {}", response.status()),
            ),
            Err(error) => (None, error.to_string()),
        };
        let (response_status, error) = failure;

        match retry_delay(delivery.attempts, self.retry_base) {
            Some(delay) => {
                tracing::warn!(
                    "webhook delivery {} to {} failed on attempt {} of {MAX_ATTEMPTS}: {error}",
                    delivery.id,
                    delivery.url,
                    delivery.attempts
                );
                delivery.next_attempt_at = Utc::now()
                    + chrono::Duration::from_std(delay)
                        .unwrap_or_else(|_| chrono::Duration::zero());
                self.pending.lock().await.push_back(delivery);
            }
            None => {
                tracing::error!(
                    "webhook delivery {} to {} dropped after {} attempts: {error}",
                    delivery.id,
                    delivery.url,
                    delivery.attempts
                );
                self.finish(
                    &delivery,
                    DeliveryStatus::Failed,
                    response_status,
                    Some(error),
                )
                .await;
            }
        }
    }

    async fn finish(
        &self,
        delivery: &PendingDelivery,
        status: DeliveryStatus,
        response_status: Option<u16>,
        error: Option<String>,
    ) {
        let mut history = self.history.lock().await;
        if history.len() == HISTORY_LIMIT {
            history.pop_front();
        }
        history.push_back(WebhookDelivery {
            id: delivery.id,
            subscription_id: delivery.subscription_id,
            event: delivery.event,
            url: delivery.url.clone(),
            status,
            attempts: delivery.attempts,
            response_status,
            error,
            queued_at: delivery.queued_at,
            finished_at: Utc::now(),
        });
    }

    /// Deliveries still waiting for an attempt. One being attempted right now
    /// counts as neither pending nor finished.
    pub async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// Finished deliveries, newest first.
    pub async fn recent_deliveries(&self, limit: usize) -> Vec<WebhookDelivery> {
        let history = self.history.lock().await;
        history.iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_secret_carries_the_prefix_and_32_bytes_of_hex() {
        let secret = generate_secret();
        let hex = secret.strip_prefix(SECRET_PREFIX).unwrap();
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(generate_secret(), generate_secret());
    }

    #[test]
    fn a_signature_is_sha256_of_the_body_under_the_secret() {
        let signed = signature("whsec_key", b"{\"event\":\"job.completed\"}");
        assert_eq!(signed.len(), "sha256=".len() + 64);
        // known-answer, so a change to the construction shows up here
        assert_eq!(
            signature("secret", b"payload"),
            "sha256=b82fcb791acec57859b989b430a826488ce2e479fdf92326bd0a2e8375a42ba4"
        );
    }

    #[test]
    fn a_signature_changes_with_the_body_and_with_the_secret() {
        let one = signature("secret-one", b"body");
        assert_ne!(one, signature("secret-two", b"body"));
        assert_ne!(one, signature("secret-one", b"body "));
    }

    #[test]
    fn event_names_parse_exactly() {
        for event in WebhookEvent::ALL {
            assert_eq!(WebhookEvent::from_name(event.name()), Some(event));
        }
        for name in ["", "job", "JobCompleted", "job.completed ", "asset.created"] {
            assert_eq!(WebhookEvent::from_name(name), None, "{name}");
        }
    }

    #[test]
    fn an_event_serializes_as_the_name_it_parses_from() {
        for event in WebhookEvent::ALL {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(json, format!("\"{}\"", event.name()));
        }
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_the_attempt_bound() {
        let base = Duration::from_secs(30);
        assert_eq!(retry_delay(1, base), Some(Duration::from_secs(30)));
        assert_eq!(retry_delay(2, base), Some(Duration::from_secs(60)));
        assert_eq!(retry_delay(3, base), Some(Duration::from_secs(120)));
        assert_eq!(retry_delay(MAX_ATTEMPTS, base), None);
        assert_eq!(retry_delay(MAX_ATTEMPTS + 1, base), None);
    }

    #[test]
    fn a_subscription_never_serializes_its_secret() {
        let subscription = WebhookSubscription {
            id: Uuid::new_v4(),
            url: "https://example.com/hook".into(),
            events: vec![WebhookEvent::JobFailed],
            secret: generate_secret(),
            created_by: "editor".into(),
            active: true,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&subscription).unwrap();
        assert!(!json.contains(&subscription.secret), "{json}");
        assert!(!json.contains("secret"), "{json}");
        assert!(json.contains("job.failed"), "{json}");
    }
}
