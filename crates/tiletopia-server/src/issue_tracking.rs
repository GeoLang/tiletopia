//! Issue/Defect Tracking — location-pinned issues with status workflows.
//!
//! For construction sites: report defects, attach photos, track resolution,
//! assign to teams, with geospatial coordinates linking to 3D models.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A tracked issue/defect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub issue_type: IssueType,
    pub severity: Severity,
    pub status: IssueStatus,
    pub location: IssueLocation,
    pub assignee: Option<String>,
    pub reporter: String,
    pub tags: Vec<String>,
    pub attachments: Vec<Attachment>,
    pub comments: Vec<Comment>,
    pub due_date: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Issue type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IssueType {
    Defect,
    SafetyHazard,
    QualityIssue,
    DesignConflict,
    Rfi, // Request for Information
    ChangeOrder,
    Observation,
    Punchlist,
}

/// Severity level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

/// Issue status (workflow).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IssueStatus {
    Open,
    InProgress,
    UnderReview,
    Resolved,
    Closed,
    Reopened,
    Wontfix,
}

/// Geospatial location of the issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueLocation {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: Option<f64>,
    pub floor_level: Option<i8>,
    pub zone: Option<String>,
    pub linked_asset_id: Option<Uuid>,
    pub camera_position: Option<[f64; 3]>, // 3D viewpoint where issue was captured
}

/// File attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub url: String,
}

/// A comment on an issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: Uuid,
    pub author: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

/// Issue tracker engine.
pub struct IssueTracker {
    issues: Vec<Issue>,
}

impl IssueTracker {
    /// Create with demo data.
    pub fn new() -> Self {
        Self {
            issues: demo_issues(),
        }
    }

    /// Create a new issue.
    pub fn create_issue(
        &mut self,
        title: String,
        description: String,
        issue_type: IssueType,
        severity: Severity,
        location: IssueLocation,
        reporter: String,
    ) -> &Issue {
        let now = Utc::now();
        let issue = Issue {
            id: Uuid::new_v4(),
            title,
            description,
            issue_type,
            severity,
            status: IssueStatus::Open,
            location,
            assignee: None,
            reporter,
            tags: Vec::new(),
            attachments: Vec::new(),
            comments: Vec::new(),
            due_date: None,
            created_at: now,
            updated_at: now,
            resolved_at: None,
        };
        self.issues.push(issue);
        self.issues.last().unwrap()
    }

    /// Transition issue status following valid workflow rules.
    pub fn transition_status(&mut self, id: Uuid, new_status: IssueStatus) -> Result<(), String> {
        let issue = self
            .issues
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| "Issue not found".to_string())?;

        let valid = matches!(
            (&issue.status, &new_status),
            (IssueStatus::Open, IssueStatus::InProgress)
                | (IssueStatus::Open, IssueStatus::Wontfix)
                | (IssueStatus::InProgress, IssueStatus::UnderReview)
                | (IssueStatus::InProgress, IssueStatus::Open)
                | (IssueStatus::UnderReview, IssueStatus::Resolved)
                | (IssueStatus::UnderReview, IssueStatus::InProgress)
                | (IssueStatus::Resolved, IssueStatus::Closed)
                | (IssueStatus::Resolved, IssueStatus::Reopened)
                | (IssueStatus::Closed, IssueStatus::Reopened)
                | (IssueStatus::Reopened, IssueStatus::InProgress)
        );

        if !valid {
            return Err(format!(
                "Invalid transition: {:?} → {:?}",
                issue.status, new_status
            ));
        }

        if new_status == IssueStatus::Resolved {
            issue.resolved_at = Some(Utc::now());
        }
        issue.status = new_status;
        issue.updated_at = Utc::now();
        Ok(())
    }

    /// Assign an issue.
    pub fn assign(&mut self, id: Uuid, assignee: String) -> Result<(), String> {
        let issue = self
            .issues
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| "Issue not found".to_string())?;
        issue.assignee = Some(assignee);
        issue.updated_at = Utc::now();
        Ok(())
    }

    /// Add a comment to an issue.
    pub fn add_comment(&mut self, id: Uuid, author: String, text: String) -> Result<Uuid, String> {
        let issue = self
            .issues
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| "Issue not found".to_string())?;
        let comment_id = Uuid::new_v4();
        issue.comments.push(Comment {
            id: comment_id,
            author,
            text,
            created_at: Utc::now(),
        });
        issue.updated_at = Utc::now();
        Ok(comment_id)
    }

    /// Add attachment metadata.
    pub fn add_attachment(
        &mut self,
        id: Uuid,
        filename: String,
        mime_type: String,
        size_bytes: u64,
        url: String,
    ) -> Result<Uuid, String> {
        let issue = self
            .issues
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| "Issue not found".to_string())?;
        let att_id = Uuid::new_v4();
        issue.attachments.push(Attachment {
            id: att_id,
            filename,
            mime_type,
            size_bytes,
            url,
        });
        issue.updated_at = Utc::now();
        Ok(att_id)
    }

    /// Search issues by text (title + description).
    pub fn search(&self, query: &str) -> Vec<&Issue> {
        let q = query.to_lowercase();
        self.issues
            .iter()
            .filter(|i| {
                i.title.to_lowercase().contains(&q)
                    || i.description.to_lowercase().contains(&q)
                    || i.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// List all issues with optional filter.
    pub fn list_issues(&self, status: Option<&IssueStatus>) -> Vec<&Issue> {
        match status {
            Some(s) => self.issues.iter().filter(|i| &i.status == s).collect(),
            None => self.issues.iter().collect(),
        }
    }

    /// Get issue by ID.
    pub fn get_issue(&self, id: Uuid) -> Option<&Issue> {
        self.issues.iter().find(|i| i.id == id)
    }

    /// Get issue statistics.
    pub fn stats(&self) -> IssueStats {
        let mut by_status: HashMap<String, u32> = HashMap::new();
        let mut by_severity: HashMap<String, u32> = HashMap::new();
        for issue in &self.issues {
            *by_status.entry(format!("{:?}", issue.status)).or_default() += 1;
            *by_severity
                .entry(format!("{:?}", issue.severity))
                .or_default() += 1;
        }
        IssueStats {
            total: self.issues.len() as u32,
            open: self
                .issues
                .iter()
                .filter(|i| i.status == IssueStatus::Open)
                .count() as u32,
            in_progress: self
                .issues
                .iter()
                .filter(|i| i.status == IssueStatus::InProgress)
                .count() as u32,
            resolved: self
                .issues
                .iter()
                .filter(|i| i.status == IssueStatus::Resolved || i.status == IssueStatus::Closed)
                .count() as u32,
            by_status,
            by_severity,
            overdue: 0,
        }
    }
}

impl Default for IssueTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Issue statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueStats {
    pub total: u32,
    pub open: u32,
    pub in_progress: u32,
    pub resolved: u32,
    pub by_status: HashMap<String, u32>,
    pub by_severity: HashMap<String, u32>,
    pub overdue: u32,
}

fn demo_issues() -> Vec<Issue> {
    vec![
        Issue {
            id: Uuid::new_v4(),
            title: "Concrete crack in foundation wall".into(),
            description: "Hairline crack observed in north foundation wall, sector B2. Approximately 1.2m length.".into(),
            issue_type: IssueType::Defect,
            severity: Severity::High,
            status: IssueStatus::Open,
            location: IssueLocation {
                latitude: 37.7755, longitude: -122.4180,
                elevation_m: Some(2.5), floor_level: Some(0),
                zone: Some("Foundation B2".into()),
                linked_asset_id: None,
                camera_position: Some([37.7756, -122.4179, 1.6]),
            },
            assignee: Some("Structural Team".into()),
            reporter: "Site Inspector A".into(),
            tags: vec!["structural".into(), "foundation".into(), "urgent".into()],
            attachments: vec![Attachment {
                id: Uuid::new_v4(), filename: "crack_photo_001.jpg".into(),
                mime_type: "image/jpeg".into(), size_bytes: 2_400_000,
                url: "/api/v1/issues/attachments/001.jpg".into(),
            }],
            comments: vec![
                Comment { id: Uuid::new_v4(), author: "Engineer B".into(), text: "Recommend epoxy injection repair. Scheduling for next week.".into(), created_at: Utc::now() - chrono::Duration::hours(4) },
            ],
            due_date: Some(Utc::now() + chrono::Duration::days(3)),
            created_at: Utc::now() - chrono::Duration::days(1),
            updated_at: Utc::now() - chrono::Duration::hours(4),
            resolved_at: None,
        },
        Issue {
            id: Uuid::new_v4(),
            title: "Missing rebar in column C4".into(),
            description: "Pre-pour inspection reveals missing vertical rebar in column C4, floor 3.".into(),
            issue_type: IssueType::QualityIssue,
            severity: Severity::Critical,
            status: IssueStatus::InProgress,
            location: IssueLocation {
                latitude: 37.7758, longitude: -122.4175,
                elevation_m: Some(12.0), floor_level: Some(3),
                zone: Some("Column Grid C4".into()),
                linked_asset_id: None,
                camera_position: Some([37.7759, -122.4174, 11.5]),
            },
            assignee: Some("Rebar Subcontractor".into()),
            reporter: "QA Manager".into(),
            tags: vec!["rebar".into(), "structural".into(), "stop-work".into()],
            attachments: vec![],
            comments: vec![],
            due_date: Some(Utc::now() + chrono::Duration::days(1)),
            created_at: Utc::now() - chrono::Duration::hours(6),
            updated_at: Utc::now() - chrono::Duration::hours(2),
            resolved_at: None,
        },
        Issue {
            id: Uuid::new_v4(),
            title: "Safety barrier missing on floor 5 edge".into(),
            description: "Temporary safety barrier removed and not replaced after crane operation.".into(),
            issue_type: IssueType::SafetyHazard,
            severity: Severity::Critical,
            status: IssueStatus::Resolved,
            location: IssueLocation {
                latitude: 37.7760, longitude: -122.4170,
                elevation_m: Some(18.0), floor_level: Some(5),
                zone: Some("East Edge".into()),
                linked_asset_id: None,
                camera_position: None,
            },
            assignee: Some("Safety Officer".into()),
            reporter: "Foreman C".into(),
            tags: vec!["safety".into(), "fall-hazard".into()],
            attachments: vec![],
            comments: vec![
                Comment { id: Uuid::new_v4(), author: "Safety Officer".into(), text: "Barrier reinstalled and secured. Additional signage added.".into(), created_at: Utc::now() - chrono::Duration::hours(1) },
            ],
            due_date: None,
            created_at: Utc::now() - chrono::Duration::hours(8),
            updated_at: Utc::now() - chrono::Duration::hours(1),
            resolved_at: Some(Utc::now() - chrono::Duration::hours(1)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_all_issues() {
        let tracker = IssueTracker::new();
        let issues = tracker.list_issues(None);
        assert_eq!(issues.len(), 3);
    }

    #[test]
    fn test_filter_by_status() {
        let tracker = IssueTracker::new();
        let open = tracker.list_issues(Some(&IssueStatus::Open));
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn test_stats() {
        let tracker = IssueTracker::new();
        let stats = tracker.stats();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.open, 1);
        assert_eq!(stats.in_progress, 1);
        assert_eq!(stats.resolved, 1);
    }

    #[test]
    fn test_issue_has_location() {
        let tracker = IssueTracker::new();
        let issues = tracker.list_issues(None);
        for issue in issues {
            assert!(issue.location.latitude != 0.0);
        }
    }
}
