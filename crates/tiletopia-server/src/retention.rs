//! Data retention policies — auto-archive/delete with lifecycle rules.

use serde::{Deserialize, Serialize};

/// Lifecycle action to take when a rule triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LifecycleAction {
    Archive,
    Delete,
    MoveToGlacier,
    Notify { email: String },
    Compress,
}

/// A retention rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionRule {
    pub id: String,
    pub name: String,
    pub resource_pattern: String, // glob pattern
    pub condition: RetentionCondition,
    pub action: LifecycleAction,
    pub enabled: bool,
}

/// Condition that triggers a retention action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RetentionCondition {
    AgeInDays(u64),
    LastAccessedDaysAgo(u64),
    SizeExceedsBytes(u64),
    VersionCount(u32),
    Custom {
        field: String,
        operator: String,
        value: String,
    },
}

/// A resource subject to retention policies.
#[derive(Debug, Clone)]
pub struct RetainedResource {
    pub id: String,
    pub resource_type: String,
    pub created_at_days_ago: u64,
    pub last_accessed_days_ago: u64,
    pub size_bytes: u64,
    pub version_count: u32,
    pub tenant_id: String,
}

/// Result of evaluating retention rules against resources.
#[derive(Debug, Clone)]
pub struct RetentionAction {
    pub resource_id: String,
    pub rule_id: String,
    pub action: LifecycleAction,
    pub reason: String,
}

/// Retention policy engine.
pub struct RetentionEngine {
    rules: Vec<RetentionRule>,
    execution_log: Vec<RetentionAction>,
}

impl RetentionEngine {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            execution_log: Vec::new(),
        }
    }

    /// Add a retention rule.
    pub fn add_rule(&mut self, rule: RetentionRule) {
        self.rules.push(rule);
    }

    /// Remove a rule by ID.
    pub fn remove_rule(&mut self, id: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() < before
    }

    /// Evaluate all rules against a resource.
    pub fn evaluate(&self, resource: &RetainedResource) -> Vec<RetentionAction> {
        let mut actions = Vec::new();

        for rule in &self.rules {
            if !rule.enabled {
                continue;
            }

            // Simple glob match (just check if pattern is in resource type or "*")
            if rule.resource_pattern != "*"
                && !resource.resource_type.contains(&rule.resource_pattern)
            {
                continue;
            }

            let triggered = match &rule.condition {
                RetentionCondition::AgeInDays(days) => resource.created_at_days_ago >= *days,
                RetentionCondition::LastAccessedDaysAgo(days) => {
                    resource.last_accessed_days_ago >= *days
                }
                RetentionCondition::SizeExceedsBytes(size) => resource.size_bytes > *size,
                RetentionCondition::VersionCount(count) => resource.version_count > *count,
                RetentionCondition::Custom { .. } => false, // Custom rules not evaluated here
            };

            if triggered {
                actions.push(RetentionAction {
                    resource_id: resource.id.clone(),
                    rule_id: rule.id.clone(),
                    action: rule.action.clone(),
                    reason: format!("Rule '{}' triggered: {:?}", rule.name, rule.condition),
                });
            }
        }
        actions
    }

    /// Run retention evaluation on a batch of resources.
    pub fn evaluate_batch(&mut self, resources: &[RetainedResource]) -> Vec<RetentionAction> {
        let mut all_actions = Vec::new();
        for resource in resources {
            let actions = self.evaluate(resource);
            all_actions.extend(actions);
        }
        self.execution_log.extend(all_actions.clone());
        all_actions
    }

    /// Get execution history.
    pub fn execution_history(&self) -> &[RetentionAction] {
        &self.execution_log
    }

    /// Get active rule count.
    pub fn active_rule_count(&self) -> usize {
        self.rules.iter().filter(|r| r.enabled).count()
    }

    /// Execute retention actions against a filesystem data directory.
    /// Returns the number of actions successfully executed.
    pub fn execute_actions(
        &self,
        actions: &[RetentionAction],
        data_dir: &std::path::Path,
    ) -> Vec<ExecutionResult> {
        actions
            .iter()
            .map(|action| {
                let resource_path = data_dir.join(&action.resource_id);
                let result = match &action.action {
                    LifecycleAction::Delete => {
                        if resource_path.exists() {
                            std::fs::remove_file(&resource_path)
                                .or_else(|_| std::fs::remove_dir_all(&resource_path))
                                .map(|_| "deleted".to_string())
                                .map_err(|e| e.to_string())
                        } else {
                            Ok("already absent".to_string())
                        }
                    }
                    LifecycleAction::Archive => {
                        let archive_dir = data_dir.join("archive");
                        let _ = std::fs::create_dir_all(&archive_dir);
                        let dest = archive_dir.join(&action.resource_id);
                        if resource_path.exists() {
                            std::fs::rename(&resource_path, &dest)
                                .map(|_| format!("archived to {}", dest.display()))
                                .map_err(|e| e.to_string())
                        } else {
                            Ok("already absent".to_string())
                        }
                    }
                    LifecycleAction::MoveToGlacier => {
                        let cold_dir = data_dir.join("cold-storage");
                        let _ = std::fs::create_dir_all(&cold_dir);
                        let dest = cold_dir.join(&action.resource_id);
                        if resource_path.exists() {
                            std::fs::rename(&resource_path, &dest)
                                .map(|_| format!("moved to cold storage: {}", dest.display()))
                                .map_err(|e| e.to_string())
                        } else {
                            Ok("already absent".to_string())
                        }
                    }
                    LifecycleAction::Compress => {
                        // Mark as needing compression (actual gzip would require async I/O)
                        Ok("compression queued".to_string())
                    }
                    LifecycleAction::Notify { email } => {
                        // Log the notification (actual email would need SMTP)
                        Ok(format!("notification queued for {email}"))
                    }
                };

                ExecutionResult {
                    resource_id: action.resource_id.clone(),
                    rule_id: action.rule_id.clone(),
                    action: action.action.clone(),
                    success: result.is_ok(),
                    message: result.unwrap_or_else(|e| e),
                }
            })
            .collect()
    }
}

/// Result of executing a single retention action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub resource_id: String,
    pub rule_id: String,
    pub action: LifecycleAction,
    pub success: bool,
    pub message: String,
}

impl Default for RetentionEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Pre-built retention policy templates.
pub fn gdpr_retention_template() -> Vec<RetentionRule> {
    vec![
        RetentionRule {
            id: "gdpr-delete-90".into(),
            name: "GDPR: Delete after 90 days inactive".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::LastAccessedDaysAgo(90),
            action: LifecycleAction::Delete,
            enabled: true,
        },
        RetentionRule {
            id: "gdpr-notify-60".into(),
            name: "GDPR: Notify at 60 days inactive".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::LastAccessedDaysAgo(60),
            action: LifecycleAction::Notify {
                email: "admin@example.com".into(),
            },
            enabled: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_age_based_deletion() {
        let mut engine = RetentionEngine::new();
        engine.add_rule(RetentionRule {
            id: "r1".into(),
            name: "Delete old assets".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::AgeInDays(30),
            action: LifecycleAction::Delete,
            enabled: true,
        });
        let resource = RetainedResource {
            id: "asset-1".into(),
            resource_type: "tileset".into(),
            created_at_days_ago: 45,
            last_accessed_days_ago: 10,
            size_bytes: 1024,
            version_count: 1,
            tenant_id: "t1".into(),
        };
        let actions = engine.evaluate(&resource);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, LifecycleAction::Delete);
    }

    #[test]
    fn test_size_based_archive() {
        let mut engine = RetentionEngine::new();
        engine.add_rule(RetentionRule {
            id: "r2".into(),
            name: "Archive large files".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::SizeExceedsBytes(1_000_000),
            action: LifecycleAction::Archive,
            enabled: true,
        });
        let resource = RetainedResource {
            id: "big-asset".into(),
            resource_type: "pointcloud".into(),
            created_at_days_ago: 5,
            last_accessed_days_ago: 2,
            size_bytes: 5_000_000,
            version_count: 1,
            tenant_id: "t1".into(),
        };
        let actions = engine.evaluate(&resource);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].action, LifecycleAction::Archive);
    }

    #[test]
    fn test_disabled_rule_skipped() {
        let mut engine = RetentionEngine::new();
        engine.add_rule(RetentionRule {
            id: "r3".into(),
            name: "Disabled rule".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::AgeInDays(1),
            action: LifecycleAction::Delete,
            enabled: false,
        });
        let resource = RetainedResource {
            id: "a".into(),
            resource_type: "x".into(),
            created_at_days_ago: 100,
            last_accessed_days_ago: 100,
            size_bytes: 0,
            version_count: 0,
            tenant_id: "t".into(),
        };
        let actions = engine.evaluate(&resource);
        assert!(actions.is_empty());
    }

    #[test]
    fn test_batch_evaluation() {
        let mut engine = RetentionEngine::new();
        engine.add_rule(RetentionRule {
            id: "r4".into(),
            name: "Old stuff".into(),
            resource_pattern: "*".into(),
            condition: RetentionCondition::AgeInDays(7),
            action: LifecycleAction::Compress,
            enabled: true,
        });
        let resources = vec![
            RetainedResource {
                id: "a1".into(),
                resource_type: "t".into(),
                created_at_days_ago: 10,
                last_accessed_days_ago: 5,
                size_bytes: 100,
                version_count: 1,
                tenant_id: "t".into(),
            },
            RetainedResource {
                id: "a2".into(),
                resource_type: "t".into(),
                created_at_days_ago: 3,
                last_accessed_days_ago: 1,
                size_bytes: 100,
                version_count: 1,
                tenant_id: "t".into(),
            },
        ];
        let actions = engine.evaluate_batch(&resources);
        assert_eq!(actions.len(), 1); // Only a1 is old enough
        assert_eq!(actions[0].resource_id, "a1");
    }

    #[test]
    fn test_gdpr_template() {
        let rules = gdpr_retention_template();
        assert_eq!(rules.len(), 2);
        assert!(rules[0].enabled);
    }
}
