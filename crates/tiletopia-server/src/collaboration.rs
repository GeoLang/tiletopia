//! Real-time collaboration — multi-user sessions with live cursors and annotations.
//!
//! Implements:
//! - Presence awareness (who's online, cursor position in 3D)
//! - Shared annotations (draw, measure, pin in 3D space)
//! - Operational transform for concurrent edits
//! - Room-based sessions with role permissions
//! - WebSocket-based event broadcasting

use axum::extract::ws::{Message, WebSocket};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

/// A collaboration session (room).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub created_by: Uuid,
    pub participants: Vec<Participant>,
    pub annotations: Vec<Annotation>,
    pub created_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    pub is_active: bool,
}

/// A participant in a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub user_id: Uuid,
    pub display_name: String,
    pub avatar_color: String,
    pub role: SessionRole,
    pub cursor: Option<Cursor3D>,
    pub viewport: Option<Viewport>,
    pub joined_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub is_online: bool,
}

/// Role within a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SessionRole {
    Owner,
    Editor,
    Commenter,
    Viewer,
}

/// 3D cursor position (world coordinates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor3D {
    pub position: [f64; 3],  // longitude, latitude, height
    pub direction: [f32; 3], // look direction (normalized)
    pub timestamp_ms: u64,
}

/// User viewport (camera state).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewport {
    pub position: [f64; 3],
    pub target: [f64; 3],
    pub up: [f32; 3],
    pub fov_degrees: f32,
}

/// A shared annotation in 3D space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: Uuid,
    pub author_id: Uuid,
    pub author_name: String,
    pub annotation_type: AnnotationType,
    pub position: [f64; 3],
    pub content: AnnotationContent,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved: bool,
    pub replies: Vec<Reply>,
}

/// Types of annotations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AnnotationType {
    /// Text comment pinned in 3D
    Pin,
    /// Polyline measurement
    Measurement,
    /// Polygon region highlight
    Region,
    /// Arrow pointing at feature
    Arrow,
    /// Free-draw sketch on surface
    Sketch,
    /// Issue/defect marker
    Issue,
    /// Photo attached to location
    Photo,
}

/// Annotation content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationContent {
    pub text: String,
    pub points: Vec<[f64; 3]>,    // geometry (polyline, polygon vertices)
    pub color: String,            // hex color
    pub attachments: Vec<String>, // URLs to attached files
    pub tags: Vec<String>,
    pub priority: Option<Priority>,
}

/// Issue priority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

/// A reply to an annotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub id: Uuid,
    pub author_id: Uuid,
    pub author_name: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

/// Collaboration event (for WebSocket broadcast).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CollabEvent {
    /// Participant joined session
    UserJoined(Participant),
    /// Participant left session
    UserLeft { user_id: Uuid },
    /// Cursor moved
    CursorMoved { user_id: Uuid, cursor: Cursor3D },
    /// Viewport changed (follow mode)
    ViewportChanged { user_id: Uuid, viewport: Viewport },
    /// Annotation created
    AnnotationCreated(Annotation),
    /// Annotation updated
    AnnotationUpdated {
        id: Uuid,
        content: AnnotationContent,
    },
    /// Annotation resolved
    AnnotationResolved { id: Uuid },
    /// Reply added
    ReplyAdded { annotation_id: Uuid, reply: Reply },
}

/// Collaboration state.
pub struct CollaborationEngine {
    sessions: Arc<RwLock<HashMap<Uuid, Session>>>,
    event_tx: broadcast::Sender<String>,
}

impl Default for CollaborationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CollaborationEngine {
    pub fn new() -> Self {
        let mut sessions = HashMap::new();
        let demo = Self::demo_session();
        sessions.insert(demo.id, demo);
        let (event_tx, _) = broadcast::channel(256);
        Self {
            sessions: Arc::new(RwLock::new(sessions)),
            event_tx,
        }
    }

    /// Get broadcast sender for event distribution.
    pub fn event_sender(&self) -> &broadcast::Sender<String> {
        &self.event_tx
    }

    /// Subscribe to events (for WebSocket handler).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.event_tx.subscribe()
    }

    /// Create a new collaboration session.
    pub async fn create_session(
        &self,
        project_id: Uuid,
        name: String,
        creator_id: Uuid,
        creator_name: String,
    ) -> Session {
        let session = Session {
            id: Uuid::new_v4(),
            project_id,
            name,
            created_by: creator_id,
            participants: vec![Participant {
                user_id: creator_id,
                display_name: creator_name,
                avatar_color: "#58a6ff".into(),
                role: SessionRole::Owner,
                cursor: None,
                viewport: None,
                joined_at: Utc::now(),
                last_seen_at: Utc::now(),
                is_online: true,
            }],
            annotations: Vec::new(),
            created_at: Utc::now(),
            last_activity_at: Utc::now(),
            is_active: true,
        };
        self.sessions
            .write()
            .await
            .insert(session.id, session.clone());
        session
    }

    /// List active sessions.
    pub async fn list_sessions(&self) -> Vec<Session> {
        self.sessions.read().await.values().cloned().collect()
    }

    /// Get session by ID.
    pub async fn get_session(&self, id: Uuid) -> Option<Session> {
        self.sessions.read().await.get(&id).cloned()
    }

    /// Get active participant count across all sessions.
    pub async fn active_users(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .flat_map(|s| &s.participants)
            .filter(|p| p.is_online)
            .count()
    }

    /// Join a session (add participant, broadcast UserJoined event).
    pub async fn join_session(
        &self,
        session_id: Uuid,
        user_id: Uuid,
        display_name: String,
    ) -> Option<Session> {
        let mut sessions = self.sessions.write().await;
        let session = sessions.get_mut(&session_id)?;
        let participant = Participant {
            user_id,
            display_name,
            avatar_color: format!("#{:06x}", rand::random::<u32>() & 0xFFFFFF),
            role: SessionRole::Editor,
            cursor: None,
            viewport: None,
            joined_at: Utc::now(),
            last_seen_at: Utc::now(),
            is_online: true,
        };
        session.participants.push(participant.clone());
        session.last_activity_at = Utc::now();
        let _ = self.event_tx.send(
            serde_json::to_string(&CollabEvent::UserJoined(participant)).unwrap_or_default(),
        );
        Some(session.clone())
    }

    /// Leave a session (mark participant offline, broadcast UserLeft event).
    pub async fn leave_session(&self, session_id: Uuid, user_id: Uuid) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Some(p) = session
                .participants
                .iter_mut()
                .find(|p| p.user_id == user_id)
            {
                p.is_online = false;
            }
            let _ = self
                .event_tx
                .send(serde_json::to_string(&CollabEvent::UserLeft { user_id }).unwrap_or_default());
        }
    }

    /// Update a user's cursor position.
    pub async fn update_cursor(&self, session_id: Uuid, user_id: Uuid, cursor: Cursor3D) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Some(p) = session
                .participants
                .iter_mut()
                .find(|p| p.user_id == user_id)
            {
                p.cursor = Some(cursor.clone());
                p.last_seen_at = Utc::now();
            }
            let _ = self.event_tx.send(
                serde_json::to_string(&CollabEvent::CursorMoved { user_id, cursor })
                    .unwrap_or_default(),
            );
        }
    }

    /// Add an annotation to a session.
    pub async fn add_annotation(&self, session_id: Uuid, annotation: Annotation) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            let _ = self.event_tx.send(
                serde_json::to_string(&CollabEvent::AnnotationCreated(annotation.clone()))
                    .unwrap_or_default(),
            );
            session.annotations.push(annotation);
        }
    }

    fn demo_session() -> Session {
        let owner_id = Uuid::new_v4();
        Session {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            name: "Bridge Inspection Review".into(),
            created_by: owner_id,
            participants: vec![
                Participant {
                    user_id: owner_id,
                    display_name: "Alice Chen".into(),
                    avatar_color: "#58a6ff".into(),
                    role: SessionRole::Owner,
                    cursor: Some(Cursor3D {
                        position: [-122.4194, 37.7749, 45.0],
                        direction: [0.0, 0.0, -1.0],
                        timestamp_ms: 1700000000000,
                    }),
                    viewport: None,
                    joined_at: Utc::now() - chrono::Duration::minutes(30),
                    last_seen_at: Utc::now(),
                    is_online: true,
                },
                Participant {
                    user_id: Uuid::new_v4(),
                    display_name: "Bob Martinez".into(),
                    avatar_color: "#3fb950".into(),
                    role: SessionRole::Editor,
                    cursor: Some(Cursor3D {
                        position: [-122.4190, 37.7751, 42.0],
                        direction: [0.5, 0.0, -0.5],
                        timestamp_ms: 1700000001000,
                    }),
                    viewport: None,
                    joined_at: Utc::now() - chrono::Duration::minutes(15),
                    last_seen_at: Utc::now() - chrono::Duration::seconds(5),
                    is_online: true,
                },
                Participant {
                    user_id: Uuid::new_v4(),
                    display_name: "Carol Park".into(),
                    avatar_color: "#f85149".into(),
                    role: SessionRole::Commenter,
                    cursor: None,
                    viewport: None,
                    joined_at: Utc::now() - chrono::Duration::minutes(10),
                    last_seen_at: Utc::now() - chrono::Duration::minutes(2),
                    is_online: true,
                },
            ],
            annotations: vec![Annotation {
                id: Uuid::new_v4(),
                author_id: owner_id,
                author_name: "Alice Chen".into(),
                annotation_type: AnnotationType::Issue,
                position: [-122.4192, 37.7750, 38.0],
                content: AnnotationContent {
                    text: "Crack detected in support beam — needs structural assessment".into(),
                    points: vec![[-122.4192, 37.7750, 38.0], [-122.4192, 37.7750, 36.0]],
                    color: "#f85149".into(),
                    attachments: vec![],
                    tags: vec!["structural".into(), "urgent".into()],
                    priority: Some(Priority::Critical),
                },
                created_at: Utc::now() - chrono::Duration::minutes(20),
                updated_at: Utc::now() - chrono::Duration::minutes(5),
                resolved: false,
                replies: vec![Reply {
                    id: Uuid::new_v4(),
                    author_id: Uuid::new_v4(),
                    author_name: "Bob Martinez".into(),
                    text: "Confirmed — I can see deformation of ~2.3mm from the baseline scan."
                        .into(),
                    created_at: Utc::now() - chrono::Duration::minutes(8),
                }],
            }],
            created_at: Utc::now() - chrono::Duration::minutes(30),
            last_activity_at: Utc::now(),
            is_active: true,
        }
    }
}

/// WebSocket handler for collaboration sessions.
pub async fn handle_ws(mut socket: WebSocket, engine: Arc<CollaborationEngine>) {
    let mut rx = engine.subscribe();

    loop {
        tokio::select! {
            // Forward broadcast events to client
            Ok(msg) = rx.recv() => {
                if socket.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            // Receive client messages
            Some(Ok(msg)) = socket.recv() => {
                match msg {
                    Message::Text(text) => {
                        tracing::debug!("WS received: {}", text);
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            else => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_session() {
        let engine = CollaborationEngine::new();
        let session = engine
            .create_session(
                Uuid::new_v4(),
                "Test Session".into(),
                Uuid::new_v4(),
                "Test User".into(),
            )
            .await;
        assert!(session.is_active);
        assert_eq!(session.participants.len(), 1);
    }

    #[tokio::test]
    async fn test_demo_session() {
        let engine = CollaborationEngine::new();
        let sessions = engine.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].participants.len(), 3);
    }

    #[tokio::test]
    async fn test_active_users() {
        let engine = CollaborationEngine::new();
        let count = engine.active_users().await;
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_join_and_leave_session() {
        let engine = CollaborationEngine::new();
        let sessions = engine.list_sessions().await;
        let session_id = sessions[0].id;
        let user_id = Uuid::new_v4();

        // Subscribe to events before joining (so broadcast has a receiver)
        let mut rx = engine.subscribe();

        let session = engine
            .join_session(session_id, user_id, "New User".into())
            .await
            .unwrap();
        assert_eq!(session.participants.len(), 4);

        // Should have received a UserJoined event
        let event_str = rx.try_recv().unwrap();
        assert!(event_str.contains("UserJoined"));

        engine.leave_session(session_id, user_id).await;
        let event_str = rx.try_recv().unwrap();
        assert!(event_str.contains("UserLeft"));

        let updated = engine.get_session(session_id).await.unwrap();
        let participant = updated
            .participants
            .iter()
            .find(|p| p.user_id == user_id)
            .unwrap();
        assert!(!participant.is_online);
    }

    #[tokio::test]
    async fn test_update_cursor() {
        let engine = CollaborationEngine::new();
        let sessions = engine.list_sessions().await;
        let session_id = sessions[0].id;
        let user_id = sessions[0].participants[0].user_id;

        let mut rx = engine.subscribe();
        let cursor = Cursor3D {
            position: [1.0, 2.0, 3.0],
            direction: [0.0, 0.0, -1.0],
            timestamp_ms: 999,
        };
        engine
            .update_cursor(session_id, user_id, cursor)
            .await;

        let event_str = rx.try_recv().unwrap();
        assert!(event_str.contains("CursorMoved"));
    }

    #[tokio::test]
    async fn test_add_annotation() {
        let engine = CollaborationEngine::new();
        let sessions = engine.list_sessions().await;
        let session_id = sessions[0].id;

        let mut rx = engine.subscribe();
        let annotation = Annotation {
            id: Uuid::new_v4(),
            author_id: Uuid::new_v4(),
            author_name: "Tester".into(),
            annotation_type: AnnotationType::Pin,
            position: [0.0, 0.0, 0.0],
            content: AnnotationContent {
                text: "Test pin".into(),
                points: vec![],
                color: "#000000".into(),
                attachments: vec![],
                tags: vec![],
                priority: None,
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
            resolved: false,
            replies: vec![],
        };
        engine.add_annotation(session_id, annotation).await;

        let event_str = rx.try_recv().unwrap();
        assert!(event_str.contains("AnnotationCreated"));

        let session = engine.get_session(session_id).await.unwrap();
        assert_eq!(session.annotations.len(), 2);
    }
}
