//! Team workspaces and organization management.
//!
//! Multi-tenant workspace system with:
//! - Organizations (billing entity)
//! - Teams within orgs (access grouping)
//! - Projects (asset collections with shared access)
//! - Invitations and role management

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// An organization (top-level billing entity).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub plan: PlanTier,
    pub created_at: DateTime<Utc>,
    pub owner_id: Uuid,
    pub member_count: u32,
    pub storage_used_bytes: u64,
}

/// A team within an organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub description: String,
    pub members: Vec<TeamMember>,
    pub created_at: DateTime<Utc>,
}

/// A team member with a role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: TeamRole,
    pub joined_at: DateTime<Utc>,
}

/// Roles within a team.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TeamRole {
    Owner,
    Admin,
    Editor,
    Viewer,
}

/// A project (shared asset collection).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub description: String,
    pub team_ids: Vec<Uuid>,
    pub asset_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Subscription plan tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PlanTier {
    Free,
    Pro,
    Enterprise,
}

/// Pending invitation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invitation {
    pub id: Uuid,
    pub org_id: Uuid,
    pub email: String,
    pub role: TeamRole,
    pub invited_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub accepted: bool,
}

/// Workspace management state.
pub struct WorkspaceStore {
    orgs: Arc<RwLock<Vec<Organization>>>,
    teams: Arc<RwLock<Vec<Team>>>,
    projects: Arc<RwLock<Vec<Project>>>,
    invitations: Arc<RwLock<Vec<Invitation>>>,
}

impl Default for WorkspaceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceStore {
    pub fn new() -> Self {
        let (orgs, teams, projects, invitations) = Self::demo_data();
        Self {
            orgs: Arc::new(RwLock::new(orgs)),
            teams: Arc::new(RwLock::new(teams)),
            projects: Arc::new(RwLock::new(projects)),
            invitations: Arc::new(RwLock::new(invitations)),
        }
    }

    pub async fn list_orgs(&self) -> Vec<Organization> {
        self.orgs.read().await.clone()
    }

    pub async fn get_org(&self, id: Uuid) -> Option<Organization> {
        self.orgs.read().await.iter().find(|o| o.id == id).cloned()
    }

    pub async fn list_teams(&self, org_id: Uuid) -> Vec<Team> {
        self.teams
            .read()
            .await
            .iter()
            .filter(|t| t.org_id == org_id)
            .cloned()
            .collect()
    }

    pub async fn list_projects(&self, org_id: Uuid) -> Vec<Project> {
        self.projects
            .read()
            .await
            .iter()
            .filter(|p| p.org_id == org_id)
            .cloned()
            .collect()
    }

    pub async fn list_invitations(&self, org_id: Uuid) -> Vec<Invitation> {
        self.invitations
            .read()
            .await
            .iter()
            .filter(|i| i.org_id == org_id && !i.accepted)
            .cloned()
            .collect()
    }

    pub async fn create_invitation(
        &self,
        org_id: Uuid,
        email: String,
        role: TeamRole,
        invited_by: Uuid,
    ) -> Invitation {
        let inv = Invitation {
            id: Uuid::new_v4(),
            org_id,
            email,
            role,
            invited_by,
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::days(7),
            accepted: false,
        };
        self.invitations.write().await.push(inv.clone());
        inv
    }

    fn demo_data() -> (Vec<Organization>, Vec<Team>, Vec<Project>, Vec<Invitation>) {
        let org_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let team_eng = Uuid::new_v4();
        let team_ops = Uuid::new_v4();

        let orgs = vec![Organization {
            id: org_id,
            name: "Acme Construction".into(),
            slug: "acme-construction".into(),
            plan: PlanTier::Pro,
            created_at: Utc::now() - chrono::Duration::days(90),
            owner_id,
            member_count: 12,
            storage_used_bytes: 45 * 1024 * 1024 * 1024, // 45 GB
        }];

        let teams = vec![
            Team {
                id: team_eng,
                org_id,
                name: "Engineering".into(),
                description: "Site engineering and survey team".into(),
                members: vec![
                    TeamMember {
                        user_id: owner_id,
                        email: "alice@acme.com".into(),
                        display_name: "Alice Chen".into(),
                        role: TeamRole::Owner,
                        joined_at: Utc::now() - chrono::Duration::days(90),
                    },
                    TeamMember {
                        user_id: Uuid::new_v4(),
                        email: "bob@acme.com".into(),
                        display_name: "Bob Martinez".into(),
                        role: TeamRole::Editor,
                        joined_at: Utc::now() - chrono::Duration::days(60),
                    },
                    TeamMember {
                        user_id: Uuid::new_v4(),
                        email: "carol@acme.com".into(),
                        display_name: "Carol Park".into(),
                        role: TeamRole::Editor,
                        joined_at: Utc::now() - chrono::Duration::days(45),
                    },
                ],
                created_at: Utc::now() - chrono::Duration::days(90),
            },
            Team {
                id: team_ops,
                org_id,
                name: "Operations".into(),
                description: "Field operations and inspections".into(),
                members: vec![
                    TeamMember {
                        user_id: Uuid::new_v4(),
                        email: "dave@acme.com".into(),
                        display_name: "Dave Wilson".into(),
                        role: TeamRole::Admin,
                        joined_at: Utc::now() - chrono::Duration::days(80),
                    },
                    TeamMember {
                        user_id: Uuid::new_v4(),
                        email: "eve@acme.com".into(),
                        display_name: "Eve Johnson".into(),
                        role: TeamRole::Viewer,
                        joined_at: Utc::now() - chrono::Duration::days(30),
                    },
                ],
                created_at: Utc::now() - chrono::Duration::days(80),
            },
        ];

        let projects = vec![
            Project {
                id: Uuid::new_v4(),
                org_id,
                name: "Highway 101 Expansion".into(),
                description: "LiDAR surveys and BIM models for highway expansion project".into(),
                team_ids: vec![team_eng, team_ops],
                asset_count: 47,
                created_at: Utc::now() - chrono::Duration::days(60),
                updated_at: Utc::now() - chrono::Duration::hours(6),
            },
            Project {
                id: Uuid::new_v4(),
                org_id,
                name: "Downtown Bridge Inspection".into(),
                description: "Structural monitoring with drone photogrammetry".into(),
                team_ids: vec![team_eng],
                asset_count: 12,
                created_at: Utc::now() - chrono::Duration::days(14),
                updated_at: Utc::now() - chrono::Duration::days(1),
            },
        ];

        let invitations = vec![Invitation {
            id: Uuid::new_v4(),
            org_id,
            email: "frank@acme.com".into(),
            role: TeamRole::Viewer,
            invited_by: owner_id,
            created_at: Utc::now() - chrono::Duration::hours(12),
            expires_at: Utc::now() + chrono::Duration::days(6),
            accepted: false,
        }];

        (orgs, teams, projects, invitations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_workspace_demo_data() {
        let store = WorkspaceStore::new();
        let orgs = store.list_orgs().await;
        assert_eq!(orgs.len(), 1);
        assert_eq!(orgs[0].name, "Acme Construction");
        assert_eq!(orgs[0].plan, PlanTier::Pro);
    }

    #[tokio::test]
    async fn test_list_teams() {
        let store = WorkspaceStore::new();
        let orgs = store.list_orgs().await;
        let teams = store.list_teams(orgs[0].id).await;
        assert_eq!(teams.len(), 2);
        assert!(teams.iter().any(|t| t.name == "Engineering"));
    }

    #[tokio::test]
    async fn test_list_projects() {
        let store = WorkspaceStore::new();
        let orgs = store.list_orgs().await;
        let projects = store.list_projects(orgs[0].id).await;
        assert_eq!(projects.len(), 2);
    }

    #[tokio::test]
    async fn test_create_invitation() {
        let store = WorkspaceStore::new();
        let orgs = store.list_orgs().await;
        let inv = store
            .create_invitation(
                orgs[0].id,
                "new@acme.com".into(),
                TeamRole::Editor,
                orgs[0].owner_id,
            )
            .await;
        assert!(!inv.accepted);
        assert_eq!(inv.email, "new@acme.com");
    }
}
