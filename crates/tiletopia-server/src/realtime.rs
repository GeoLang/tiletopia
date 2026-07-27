//! WebSocket real-time data layer.
//!
//! Clients connect to /api/v1/realtime/{room} and receive
//! live sensor/IoT data updates as JSON messages.
//!
//! Also supports collaboration: presence tracking, cursor sharing, and chat.
//!
//! The room id is whatever string the viewer joins on, usually an asset uuid.
//! It is only ever a map key, never a path or a query.
//!
//! # Handshake contract
//!
//! A connection needs a valid JWT of any role. A browser cannot set the
//! Authorization header on a WebSocket handshake, so the token is offered as a
//! subprotocol instead:
//!
//! ```js
//! new WebSocket(`ws://host/api/v1/realtime/${room}`, ["bearer", jwt])
//! ```
//!
//! which sends `Sec-WebSocket-Protocol: bearer, <jwt>`. The order is fixed: the
//! marker `bearer` first, the token second. The 101 response echoes
//! `Sec-WebSocket-Protocol: bearer` and never the token. Non-browser clients may
//! send `Authorization: Bearer <jwt>` instead and offer no subprotocol, in which
//! case the response carries no subprotocol either. No credential, or one that
//! does not validate, is 401 before the upgrade.
//!
//! # Identity
//!
//! Every message a client sends is re-serialized with `user_id` set to the JWT
//! `sub` before it reaches anyone else, so a room member cannot act as another
//! member. A client's own `user_id` is ignored, including in the echo it gets
//! back. `user_name` stays client-chosen: it is a display label, not identity.
//!
//! # Rooms and connections
//!
//! A room exists only while a connection holds it: the first connection creates
//! it, the last one out drops it with its broadcast channel. The account that
//! created a room is charged for it until it is empty, up to
//! [`MAX_ROOMS_PER_USER`]. A connection that would go past that limit upgrades
//! and is then closed with [`ROOM_LIMIT_CLOSE_CODE`], because a browser cannot
//! read the status of a failed handshake but can read a close code.
//!
//! Presence is per connection, not per account: two tabs of one account are two
//! connections, and the account leaves the roster when the last of them goes.

use axum::{
    extract::{
        Extension, Path, Request, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, broadcast};

use crate::{AppState, auth::Claims};

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

impl CollabMessage {
    /// Force the sender id to the authenticated subject, so nothing a client
    /// claims about who it is survives. `user_name` is left alone: it is a
    /// display label the client picks, not identity.
    fn with_sender(mut self, sub: &str) -> Self {
        match &mut self {
            CollabMessage::Join { user_id, .. }
            | CollabMessage::Leave { user_id, .. }
            | CollabMessage::Cursor { user_id, .. }
            | CollabMessage::Chat { user_id, .. }
            | CollabMessage::ViewChanged { user_id, .. } => *user_id = sub.to_string(),
            // server-originated, a client never sends one
            CollabMessage::Presence { .. } => {}
        }
        self
    }
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

/// Identifies one WebSocket connection, so presence can be refcounted per
/// connection rather than per account.
pub type ConnId = u64;

/// One account's presence in a room and the connections holding it.
struct PresenceUser {
    entry: PresenceEntry,
    connections: HashSet<ConnId>,
}

/// Tracks which users are present in which asset session.
pub struct PresenceTracker {
    /// asset_id -> user_id -> presence
    sessions: RwLock<HashMap<String, HashMap<String, PresenceUser>>>,
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

    pub async fn join(&self, asset_id: &str, user_id: &str, user_name: &str, conn: ConnId) {
        let mut sessions = self.sessions.write().await;
        let asset_users = sessions.entry(asset_id.to_string()).or_default();
        let color_idx = asset_users.len() % PRESENCE_COLORS.len();
        match asset_users.get_mut(user_id) {
            // already here on another connection: keep the color, take the newest name
            Some(user) => {
                user.entry.user_name = user_name.to_string();
                user.connections.insert(conn);
            }
            None => {
                asset_users.insert(
                    user_id.to_string(),
                    PresenceUser {
                        entry: PresenceEntry {
                            user_id: user_id.to_string(),
                            user_name: user_name.to_string(),
                            color: PRESENCE_COLORS[color_idx].to_string(),
                        },
                        connections: HashSet::from([conn]),
                    },
                );
            }
        }
    }

    /// Drop one connection's claim on a user's presence. The user leaves the
    /// roster only when the last of their connections goes, so closing one tab
    /// keeps an account that is still connected elsewhere, and a stale
    /// connection's late cleanup cannot remove one that has reconnected.
    pub async fn leave(&self, asset_id: &str, user_id: &str, conn: ConnId) {
        let mut sessions = self.sessions.write().await;
        let Some(asset_users) = sessions.get_mut(asset_id) else {
            return;
        };
        if let Some(user) = asset_users.get_mut(user_id) {
            user.connections.remove(&conn);
            if user.connections.is_empty() {
                asset_users.remove(user_id);
            }
        }
        if asset_users.is_empty() {
            sessions.remove(asset_id);
        }
    }

    pub async fn get_presence(&self, asset_id: &str) -> Vec<PresenceEntry> {
        let sessions = self.sessions.read().await;
        sessions
            .get(asset_id)
            .map(|m| m.values().map(|u| u.entry.clone()).collect())
            .unwrap_or_default()
    }
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Real-time state ─────────────────────────────────────────────────────────

/// How many rooms one account may hold open at once. Rooms are created by
/// whoever joins one first, so without a cap a single token could fill memory
/// with broadcast channels. A viewer holds one room per asset it has open, so a
/// couple of dozen covers legitimate multi-tab, multi-asset use.
pub const MAX_ROOMS_PER_USER: usize = 32;

/// Close code sent to a connection refused by [`MAX_ROOMS_PER_USER`]. In the
/// private-use range 4000-4999, picked to echo HTTP 429.
pub const ROOM_LIMIT_CLOSE_CODE: u16 = 4029;

/// A live room: its broadcast channel, the account charged for it, and how many
/// connections hold it open.
struct Room {
    tx: broadcast::Sender<String>,
    owner: String,
    connections: usize,
}

/// Rooms and the per-owner counts derived from them. Both maps are only ever
/// touched together, under one lock, so the counts cannot drift.
#[derive(Default)]
struct Rooms {
    by_id: HashMap<String, Room>,
    /// user_id -> rooms that user created and still holds
    owned: HashMap<String, usize>,
}

/// Real-time state — broadcast channel per room.
pub struct RealtimeState {
    rooms: RwLock<Rooms>,
    /// Presence tracker for collaboration.
    pub presence: PresenceTracker,
    next_conn_id: AtomicU64,
}

impl RealtimeState {
    pub fn new() -> Self {
        Self {
            rooms: RwLock::new(Rooms::default()),
            presence: PresenceTracker::new(),
            next_conn_id: AtomicU64::new(0),
        }
    }

    pub fn new_conn_id(&self) -> ConnId {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Take one connection's hold on a room, creating it if this is the first
    /// connection. `None` when `user` already holds [`MAX_ROOMS_PER_USER`]
    /// rooms; joining a room someone else created is not charged to `user`.
    /// Every success must be paired with a [`Self::release_room`].
    async fn acquire_room(&self, room: &str, user: &str) -> Option<broadcast::Sender<String>> {
        let mut rooms = self.rooms.write().await;
        if let Some(existing) = rooms.by_id.get_mut(room) {
            existing.connections += 1;
            return Some(existing.tx.clone());
        }
        let owned = rooms.owned.get(user).copied().unwrap_or(0);
        if owned >= MAX_ROOMS_PER_USER {
            return None;
        }
        let (tx, _) = broadcast::channel(256);
        rooms.by_id.insert(
            room.to_string(),
            Room {
                tx: tx.clone(),
                owner: user.to_string(),
                connections: 1,
            },
        );
        rooms.owned.insert(user.to_string(), owned + 1);
        Some(tx)
    }

    /// Release one connection's hold. An empty room is dropped along with its
    /// channel, which also frees the owner's slot.
    async fn release_room(&self, room: &str) {
        let mut rooms = self.rooms.write().await;
        let owner = {
            let Some(existing) = rooms.by_id.get_mut(room) else {
                return;
            };
            existing.connections -= 1;
            if existing.connections > 0 {
                return;
            }
            existing.owner.clone()
        };
        rooms.by_id.remove(room);
        if let Some(owned) = rooms.owned.get_mut(&owner) {
            *owned -= 1;
            if *owned == 0 {
                rooms.owned.remove(&owner);
            }
        }
    }

    /// Push a real-time update to all connected clients in a room.
    pub async fn push_update(&self, room: &str, data: String) {
        let rooms = self.rooms.read().await;
        if let Some(existing) = rooms.by_id.get(room) {
            let _ = existing.tx.send(data);
        }
    }
}

impl Default for RealtimeState {
    fn default() -> Self {
        Self::new()
    }
}

/// Longest room id we accept. Rooms are created on demand and each one holds a
/// broadcast channel, so the key is bounded. How many a client may hold at once
/// is bounded by [`MAX_ROOMS_PER_USER`].
const MAX_ROOM_ID_LEN: usize = 128;

/// Room join gate: any valid JWT may connect, viewer role included, because
/// collaboration is presence, cursors and chat rather than a write to stored
/// data. That matches the annotation routes. Anonymous is rejected.
///
/// This is a layer rather than a check inside [`ws_handler`] so the rejection
/// happens before the upgrade handshake is looked at, and so it holds even in
/// the no-secret development mode where [`crate::auth::auth_middleware`] passes
/// everything through.
pub async fn require_room_join(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let token = crate::auth::request_token(request.headers(), request.uri())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = crate::users::claims_from_token(token)?;
    // the handler stamps every message with these claims
    request.extensions_mut().insert(claims);
    Ok(next.run(request).await)
}

/// WebSocket upgrade handler.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(room): Path<String>,
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, StatusCode> {
    if room.len() > MAX_ROOM_ID_LEN {
        return Err(StatusCode::BAD_REQUEST);
    }
    // echoing the marker is required for a browser to accept the 101; the token
    // itself is never echoed
    let upgrade = ws.protocols([crate::auth::BEARER_SUBPROTOCOL]);
    let Some(tx) = state.realtime.acquire_room(&room, &claims.sub).await else {
        return Ok(upgrade.on_upgrade(close_over_room_limit));
    };
    let rx = tx.subscribe();
    let conn = state.realtime.new_conn_id();
    Ok(upgrade
        .on_upgrade(move |socket| handle_socket(socket, tx, rx, room, claims.sub, conn, state)))
}

/// Refuse a connection that would go past [`MAX_ROOMS_PER_USER`]. It holds no
/// room and no presence, so nothing needs releasing.
async fn close_over_room_limit(mut socket: WebSocket) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: ROOM_LIMIT_CLOSE_CODE,
            reason: "room limit".into(),
        })))
        .await;
}

/// Send the room's presence list to everyone in it.
async fn broadcast_presence(tx: &broadcast::Sender<String>, state: &AppState, room: &str) {
    let users = state.realtime.presence.get_presence(room).await;
    if let Ok(json) = serde_json::to_string(&CollabMessage::Presence { users }) {
        let _ = tx.send(json);
    }
}

async fn handle_socket(
    mut socket: WebSocket,
    tx: broadcast::Sender<String>,
    mut rx: broadcast::Receiver<String>,
    room: String,
    sub: String,
    conn: ConnId,
    state: Arc<AppState>,
) {
    let mut joined = false;

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
                            // the client's own idea of who it is never leaves this line
                            let collab_msg = collab_msg.with_sender(&sub);
                            match &collab_msg {
                                CollabMessage::Join { user_name, .. } => {
                                    state.realtime.presence.join(&room, &sub, user_name, conn).await;
                                    joined = true;
                                    broadcast_presence(&tx, &state, &room).await;
                                }
                                CollabMessage::Leave { .. } => {
                                    state.realtime.presence.leave(&room, &sub, conn).await;
                                    joined = false;
                                    broadcast_presence(&tx, &state, &room).await;
                                }
                                CollabMessage::Cursor { .. }
                                | CollabMessage::Chat { .. }
                                | CollabMessage::ViewChanged { .. } => {
                                    // rebroadcast the stamped message, never the client's text
                                    if let Ok(json) = serde_json::to_string(&collab_msg) {
                                        let _ = tx.send(json);
                                    }
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
    if joined {
        state.realtime.presence.leave(&room, &sub, conn).await;
        broadcast_presence(&tx, &state, &room).await;
    }
    // the local tx still reaches everyone left, even once the room is gone
    state.realtime.release_room(&room).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(mut entries: Vec<PresenceEntry>) -> Vec<String> {
        entries.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        entries.into_iter().map(|e| e.user_id).collect()
    }

    #[tokio::test]
    async fn two_connections_of_one_user_hold_presence_until_both_leave() {
        let tracker = PresenceTracker::new();
        tracker.join("room", "ann", "Ann", 1).await;
        tracker.join("room", "ann", "Ann", 2).await;
        tracker.join("room", "bob", "Bob", 3).await;

        // one tab closes: the account is still in the roster
        tracker.leave("room", "ann", 1).await;
        assert_eq!(names(tracker.get_presence("room").await), ["ann", "bob"]);

        // the second one closes: now it is gone
        tracker.leave("room", "ann", 2).await;
        assert_eq!(names(tracker.get_presence("room").await), ["bob"]);
    }

    #[tokio::test]
    async fn stale_cleanup_does_not_remove_a_reconnected_user() {
        let tracker = PresenceTracker::new();
        tracker.join("room", "ann", "Ann", 1).await;
        // the reconnect lands before the dead connection's cleanup runs
        tracker.join("room", "ann", "Ann", 2).await;
        tracker.leave("room", "ann", 1).await;
        assert_eq!(names(tracker.get_presence("room").await), ["ann"]);

        // a cleanup that runs twice, or for a connection that never joined,
        // cannot take the live one down either
        tracker.leave("room", "ann", 1).await;
        tracker.leave("room", "ann", 99).await;
        assert_eq!(names(tracker.get_presence("room").await), ["ann"]);
    }

    #[tokio::test]
    async fn empty_session_is_reclaimed() {
        let tracker = PresenceTracker::new();
        tracker.join("room", "ann", "Ann", 1).await;
        tracker.leave("room", "ann", 1).await;
        assert!(tracker.sessions.read().await.is_empty());
    }

    #[tokio::test]
    async fn room_is_reclaimed_when_the_last_connection_leaves() {
        let state = RealtimeState::new();
        assert!(state.acquire_room("room", "ann").await.is_some());
        assert!(state.acquire_room("room", "bob").await.is_some());

        state.release_room("room").await;
        assert_eq!(state.rooms.read().await.by_id.len(), 1);

        state.release_room("room").await;
        let rooms = state.rooms.read().await;
        assert!(rooms.by_id.is_empty());
        // the owner's slot goes with it, no zero entry left behind
        assert!(rooms.owned.is_empty());
    }

    #[tokio::test]
    async fn a_user_may_hold_only_max_rooms_per_user_rooms() {
        let state = RealtimeState::new();
        for i in 0..MAX_ROOMS_PER_USER {
            assert!(
                state
                    .acquire_room(&format!("room-{i}"), "ann")
                    .await
                    .is_some(),
                "room {i} is within the cap"
            );
        }
        assert!(state.acquire_room("one-too-many", "ann").await.is_none());
        // and the refused room was not created
        assert_eq!(state.rooms.read().await.by_id.len(), MAX_ROOMS_PER_USER);

        // freeing one lets the next through
        state.release_room("room-0").await;
        assert!(state.acquire_room("one-too-many", "ann").await.is_some());
        assert!(state.acquire_room("another", "ann").await.is_none());
    }

    #[tokio::test]
    async fn the_cap_charges_the_creator_not_a_later_joiner() {
        let state = RealtimeState::new();
        for i in 0..MAX_ROOMS_PER_USER {
            state
                .acquire_room(&format!("room-{i}"), "ann")
                .await
                .unwrap();
        }
        // bob created nothing, so he can still create a room of his own
        assert!(state.acquire_room("bobs-room", "bob").await.is_some());
        // and ann, at her cap, can still join a room she does not own
        assert!(state.acquire_room("bobs-room", "ann").await.is_some());
        assert_eq!(state.rooms.read().await.owned.get("bob"), Some(&1));
    }
}
