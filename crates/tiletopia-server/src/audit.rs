//! Immutable audit trail for compliance (SOC2, ISO 27001).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

/// An audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub user_id: String,
    pub action: AuditAction,
    pub resource_type: String,
    pub resource_id: String,
    pub details: String,
    pub ip_address: Option<String>,
    pub org_id: Option<String>,
    pub success: bool,
}

/// Types of auditable actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AuditAction {
    Create,
    Read,
    Update,
    Delete,
    Upload,
    Download,
    Login,
    Logout,
    PermissionChange,
    ConfigChange,
    Export,
    Share,
}

/// Query filter for audit log.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub user_id: Option<String>,
    pub action: Option<AuditAction>,
    pub resource_type: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
}

/// Thread-safe audit log store.
#[derive(Clone)]
pub struct AuditLog {
    entries: Arc<RwLock<VecDeque<AuditEntry>>>,
    max_entries: usize,
}

impl AuditLog {
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Arc::new(RwLock::new(VecDeque::new())),
            max_entries,
        }
    }

    /// Record an audit event (append-only).
    pub fn record(&self, entry: AuditEntry) {
        let mut entries = self.entries.write().unwrap();
        if entries.len() >= self.max_entries {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Record a simple action.
    pub fn log_action(
        &self,
        user_id: &str,
        action: AuditAction,
        resource_type: &str,
        resource_id: &str,
        details: &str,
        success: bool,
    ) {
        self.record(AuditEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now(),
            user_id: user_id.to_string(),
            action,
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
            details: details.to_string(),
            ip_address: None,
            org_id: None,
            success,
        });
    }

    /// Query the audit log with filters.
    pub fn query(&self, filter: &AuditQuery) -> Vec<AuditEntry> {
        let entries = self.entries.read().unwrap();
        let limit = filter.limit.unwrap_or(100);

        entries
            .iter()
            .filter(|e| {
                if filter.user_id.as_ref().is_some_and(|uid| e.user_id != *uid) {
                    return false;
                }
                if filter
                    .action
                    .as_ref()
                    .is_some_and(|action| e.action != *action)
                {
                    return false;
                }
                if filter
                    .resource_type
                    .as_ref()
                    .is_some_and(|rt| e.resource_type != *rt)
                {
                    return false;
                }
                if filter.from.is_some_and(|from| e.timestamp < from) {
                    return false;
                }
                if filter.to.is_some_and(|to| e.timestamp > to) {
                    return false;
                }
                true
            })
            .rev()
            .take(limit)
            .cloned()
            .collect()
    }

    /// Get total entry count.
    pub fn count(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    /// Export audit log as JSON for compliance reporting.
    pub fn export_json(&self) -> String {
        let entries = self.entries.read().unwrap();
        serde_json::to_string_pretty(&*entries).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_count() {
        let log = AuditLog::new(1000);
        log.log_action(
            "user-1",
            AuditAction::Create,
            "tileset",
            "ts-1",
            "Created new tileset",
            true,
        );
        log.log_action(
            "user-1",
            AuditAction::Upload,
            "asset",
            "a-1",
            "Uploaded LAS file",
            true,
        );
        assert_eq!(log.count(), 2);
    }

    #[test]
    fn test_max_entries_eviction() {
        let log = AuditLog::new(3);
        for i in 0..5 {
            log.log_action("u", AuditAction::Read, "t", &format!("r-{}", i), "", true);
        }
        assert_eq!(log.count(), 3);
    }

    #[test]
    fn test_query_by_user() {
        let log = AuditLog::new(100);
        log.log_action("alice", AuditAction::Create, "project", "p1", "", true);
        log.log_action("bob", AuditAction::Create, "project", "p2", "", true);
        log.log_action("alice", AuditAction::Update, "project", "p1", "", true);
        let results = log.query(&AuditQuery {
            user_id: Some("alice".into()),
            ..Default::default()
        });
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_query_by_action() {
        let log = AuditLog::new(100);
        log.log_action("u", AuditAction::Login, "auth", "session", "", true);
        log.log_action("u", AuditAction::Create, "tileset", "t1", "", true);
        log.log_action("u", AuditAction::Login, "auth", "session2", "", true);
        let results = log.query(&AuditQuery {
            action: Some(AuditAction::Login),
            ..Default::default()
        });
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_export_json() {
        let log = AuditLog::new(100);
        log.log_action(
            "u",
            AuditAction::Delete,
            "asset",
            "a1",
            "Removed old data",
            true,
        );
        let json = log.export_json();
        assert!(json.contains("Delete"));
        assert!(json.contains("Removed old data"));
    }
}
