//! WebSocket real-time data layer.
//!
//! Clients connect to /api/v1/realtime/{asset_id} and receive
//! live sensor/IoT data updates as JSON messages.

use axum::{
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

/// Real-time state — broadcast channel per asset.
pub struct RealtimeState {
    /// Broadcast sender for real-time updates. Keyed by asset ID.
    pub channels: tokio::sync::RwLock<std::collections::HashMap<Uuid, broadcast::Sender<String>>>,
}

impl RealtimeState {
    pub fn new() -> Self {
        Self {
            channels: tokio::sync::RwLock::new(std::collections::HashMap::new()),
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
    ws.on_upgrade(move |socket| handle_socket(socket, rx))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
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
                        // Echo back or handle commands
                        tracing::debug!("WS received: {}", text);
                    }
                    _ => {}
                }
            }
        }
    }
}
