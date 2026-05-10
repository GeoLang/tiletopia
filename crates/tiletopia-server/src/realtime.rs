//! WebSocket real-time data layer.
//!
//! Clients connect to /api/v1/realtime/{asset_id} and receive
//! live sensor/IoT data updates as JSON messages.
//!
//! Also supports collaboration: presence tracking, cursor sharing, and chat.

use axum::{
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

// ─── Collaboration message types ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CollabMessage {
    Join {
        user_id: String,
        asset_id: String,
        user_name: String,
    },
    Leave {
        user_id: String,
        asset_id: String,
    },
    Cursor {
        user_id: String,
        longitude: f64,
        latitude: f64,
        height: f64,
    },
    Chat {
        user_id: String,
        user_name: String,
        message: String,
        timestamp: String,
    },
    Presence {
        users: Vec<PresenceEntry>,
    },
    ViewChanged {
        user_id: String,
        camera: CameraPosition,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceEntry {
    pub user_id: String,
    pub user_name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraPosition {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub heading: f64,
    pub pitch: f64,
    pub roll: f64,
}

// ─── Presence tracker ────────────────────────────────────────────────────────

/// Tracks which users are present in which asset session.
pub struct PresenceTracker {
    /// asset_id -> user_id -> entry
    sessions: RwLock<HashMap<String, HashMap<String, PresenceEntry>>>,
}

const PRESENCE_COLORS: &[&str] = &[
    "#e06c75", "#61afef", "#98c379", "#d19a66", "#c678dd", "#56b6c2", "#e5c07b", "#be5046",
];

impl PresenceTracker {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn join(&self, asset_id: &str, user_id: &str, user_name: &str) {
        let mut sessions = self.sessions.write().await;
        let asset_users = sessions.entry(asset_id.to_string()).or_default();
        let color_idx = asset_users.len() % PRESENCE_COLORS.len();
        asset_users.insert(
            user_id.to_string(),
            PresenceEntry {
                user_id: user_id.to_string(),
                user_name: user_name.to_string(),
                color: PRESENCE_COLORS[color_idx].to_string(),
            },
        );
    }

    pub async fn leave(&self, asset_id: &str, user_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(asset_users) = sessions.get_mut(asset_id) {
            asset_users.remove(user_id);
            if asset_users.is_empty() {
                sessions.remove(asset_id);
            }
        }
    }

    pub async fn get_presence(&self, asset_id: &str) -> Vec<PresenceEntry> {
        let sessions = self.sessions.read().await;
        sessions
            .get(asset_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Real-time state ─────────────────────────────────────────────────────────

/// Real-time state — broadcast channel per asset.
pub struct RealtimeState {
    /// Broadcast sender for real-time updates. Keyed by asset ID.
    pub channels: RwLock<HashMap<Uuid, broadcast::Sender<String>>>,
    /// Presence tracker for collaboration.
    pub presence: PresenceTracker,
}

impl RealtimeState {
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            presence: PresenceTracker::new(),
        }
    }

    pub async fn get_or_create_channel(&self, asset_id: Uuid) -> broadcast::Sender<String> {
        let channels = self.channels.read().await;
        if let Some(tx) = channels.get(&asset_id) {
            return tx.clone();
        }
        drop(channels);

        let mut channels = self.channels.write().await;
        let (tx, _) = broadcast::channel(256);
        channels.entry(asset_id).or_insert(tx).clone()
    }

    /// Push a real-time update to all connected clients for an asset.
    pub async fn push_update(&self, asset_id: Uuid, data: String) {
        let channels = self.channels.read().await;
        if let Some(tx) = channels.get(&asset_id) {
            let _ = tx.send(data);
        }
    }
}

impl Default for RealtimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// WebSocket upgrade handler.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(asset_id): Path<Uuid>,
    State(state): State<Arc<RealtimeState>>,
) -> Response {
    let tx = state.get_or_create_channel(asset_id).await;
    let rx = tx.subscribe();
    ws.on_upgrade(move |socket| handle_socket(socket, tx, rx, asset_id, state))
}

async fn handle_socket(
    mut socket: WebSocket,
    tx: broadcast::Sender<String>,
    mut rx: broadcast::Receiver<String>,
    asset_id: Uuid,
    state: Arc<RealtimeState>,
) {
    let mut current_user_id: Option<String> = None;
    let asset_key = asset_id.to_string();

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(data) => {
                        if socket.send(Message::Text(data.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket client lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(collab_msg) = serde_json::from_str::<CollabMessage>(&text) {
                            match &collab_msg {
                                CollabMessage::Join { user_id, user_name, .. } => {
                                    state.presence.join(&asset_key, user_id, user_name).await;
                                    current_user_id = Some(user_id.clone());
                                    // Broadcast presence update
                                    let users = state.presence.get_presence(&asset_key).await;
                                    let presence_msg = CollabMessage::Presence { users };
                                    if let Ok(json) = serde_json::to_string(&presence_msg) {
                                        let _ = tx.send(json);
                                    }
                                }
                                CollabMessage::Leave { user_id, .. } => {
                                    state.presence.leave(&asset_key, user_id).await;
                                    let users = state.presence.get_presence(&asset_key).await;
                                    let presence_msg = CollabMessage::Presence { users };
                                    if let Ok(json) = serde_json::to_string(&presence_msg) {
                                        let _ = tx.send(json);
                                    }
                                }
                                CollabMessage::Cursor { .. }
                                | CollabMessage::Chat { .. }
                                | CollabMessage::ViewChanged { .. } => {
                                    // Broadcast to all other clients
                                    let _ = tx.send(text.to_string());
                                }
                                CollabMessage::Presence { .. } => {
                                    // Server-originated only, ignore from clients
                                }
                            }
                        } else {
                            tracing::debug!("WS received non-collab message: {}", text);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Clean up presence on disconnect
    if let Some(user_id) = current_user_id {
        state.presence.leave(&asset_key, &user_id).await;
        let users = state.presence.get_presence(&asset_key).await;
        let presence_msg = CollabMessage::Presence { users };
        if let Ok(json) = serde_json::to_string(&presence_msg) {
            let _ = state.push_update(asset_id, json).await;
        }
    }
}
