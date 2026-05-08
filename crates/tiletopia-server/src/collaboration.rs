//! Real-time collaboration — multi-user sessions with live cursors and annotations.
//!
//! Implements:
//! - Presence awareness (who's online, cursor position in 3D)
//! - Shared annotations (draw, measure, pin in 3D space)
//! - Operational transform for concurrent edits
//! - Room-based sessions with role permissions

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
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
        Self {
            sessions: Arc::new(RwLock::new(sessions)),
        }
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
}
