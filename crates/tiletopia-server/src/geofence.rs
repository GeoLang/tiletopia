//! Geofenced data residency — restrict tile storage to specific regions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Geographic region for data residency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataRegion {
    UsEast,
    UsWest,
    EuWest,
    EuCentral,
    AsiaPacific,
    Custom(String),
}

impl DataRegion {
    pub fn display_name(&self) -> &str {
        match self {
            DataRegion::UsEast => "US East (Virginia)",
            DataRegion::UsWest => "US West (Oregon)",
            DataRegion::EuWest => "EU West (Ireland)",
            DataRegion::EuCentral => "EU Central (Frankfurt)",
            DataRegion::AsiaPacific => "Asia Pacific (Tokyo)",
            DataRegion::Custom(name) => name,
        }
    }
}

/// Data residency policy for a tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidencyPolicy {
    pub tenant_id: String,
    pub allowed_regions: Vec<DataRegion>,
    pub primary_region: DataRegion,
    pub replicate_across_regions: bool,
    pub enforce_strict: bool, // If true, reject writes to non-allowed regions
}

/// Storage node in a region.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageNode {
    pub id: String,
    pub region: DataRegion,
    pub endpoint: String,
    pub capacity_bytes: u64,
    pub used_bytes: u64,
    pub healthy: bool,
}

/// Geofence enforcement result.
#[derive(Debug, Clone, PartialEq)]
pub enum GeofenceResult {
    Allowed,
    Denied { reason: String },
    Redirected { target_region: DataRegion },
}

/// Geofence policy store.
pub struct GeofenceStore {
    policies: HashMap<String, ResidencyPolicy>,
    nodes: Vec<StorageNode>,
}

impl GeofenceStore {
    pub fn new() -> Self {
        Self {
            policies: HashMap::new(),
            nodes: Vec::new(),
        }
    }

    /// Set residency policy for a tenant.
    pub fn set_policy(&mut self, policy: ResidencyPolicy) {
        self.policies.insert(policy.tenant_id.clone(), policy);
    }

    /// Add a storage node.
    pub fn add_node(&mut self, node: StorageNode) {
        self.nodes.push(node);
    }

    /// Check if a write to a region is allowed for a tenant.
    pub fn check_write(&self, tenant_id: &str, target_region: &DataRegion) -> GeofenceResult {
        match self.policies.get(tenant_id) {
            None => GeofenceResult::Allowed, // No policy = no restriction
            Some(policy) => {
                if policy.allowed_regions.contains(target_region) {
                    GeofenceResult::Allowed
                } else if policy.enforce_strict {
                    GeofenceResult::Denied {
                        reason: format!(
                            "Region {:?} not in allowed regions for tenant {}",
                            target_region, tenant_id
                        ),
                    }
                } else {
                    GeofenceResult::Redirected {
                        target_region: policy.primary_region.clone(),
                    }
                }
            }
        }
    }

    /// Get the best storage node for a tenant.
    pub fn select_node(&self, tenant_id: &str) -> Option<&StorageNode> {
        let region = self
            .policies
            .get(tenant_id)
            .map(|p| &p.primary_region)
            .unwrap_or(&DataRegion::UsEast);

        self.nodes
            .iter()
            .filter(|n| n.region == *region && n.healthy && n.used_bytes < n.capacity_bytes)
            .min_by_key(|n| n.used_bytes)
    }

    /// Get all healthy nodes in a region.
    pub fn nodes_in_region(&self, region: &DataRegion) -> Vec<&StorageNode> {
        self.nodes
            .iter()
            .filter(|n| n.region == *region && n.healthy)
            .collect()
    }

    /// Check compliance: are all tenant's data in allowed regions?
    pub fn audit_compliance(&self, tenant_id: &str) -> ComplianceReport {
        let policy = match self.policies.get(tenant_id) {
            Some(p) => p,
            None => {
                return ComplianceReport {
                    compliant: true,
                    violations: Vec::new(),
                };
            }
        };

        let violations: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| !policy.allowed_regions.contains(&n.region) && n.used_bytes > 0)
            .map(|n| {
                format!(
                    "Data found in {:?} (node {}), not in allowed regions",
                    n.region, n.id
                )
            })
            .collect();

        ComplianceReport {
            compliant: violations.is_empty(),
            violations,
        }
    }
}

impl Default for GeofenceStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compliance audit report.
#[derive(Debug, Clone)]
pub struct ComplianceReport {
    pub compliant: bool,
    pub violations: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_write_in_region() {
        let mut store = GeofenceStore::new();
        store.set_policy(ResidencyPolicy {
            tenant_id: "eu-corp".into(),
            allowed_regions: vec![DataRegion::EuWest, DataRegion::EuCentral],
            primary_region: DataRegion::EuWest,
            replicate_across_regions: false,
            enforce_strict: true,
        });
        assert_eq!(
            store.check_write("eu-corp", &DataRegion::EuWest),
            GeofenceResult::Allowed
        );
    }

    #[test]
    fn test_deny_write_outside_region() {
        let mut store = GeofenceStore::new();
        store.set_policy(ResidencyPolicy {
            tenant_id: "eu-corp".into(),
            allowed_regions: vec![DataRegion::EuWest],
            primary_region: DataRegion::EuWest,
            replicate_across_regions: false,
            enforce_strict: true,
        });
        let result = store.check_write("eu-corp", &DataRegion::UsEast);
        assert!(matches!(result, GeofenceResult::Denied { .. }));
    }

    #[test]
    fn test_redirect_non_strict() {
        let mut store = GeofenceStore::new();
        store.set_policy(ResidencyPolicy {
            tenant_id: "flex-corp".into(),
            allowed_regions: vec![DataRegion::UsWest],
            primary_region: DataRegion::UsWest,
            replicate_across_regions: false,
            enforce_strict: false,
        });
        let result = store.check_write("flex-corp", &DataRegion::AsiaPacific);
        assert_eq!(
            result,
            GeofenceResult::Redirected {
                target_region: DataRegion::UsWest
            }
        );
    }

    #[test]
    fn test_select_node() {
        let mut store = GeofenceStore::new();
        store.set_policy(ResidencyPolicy {
            tenant_id: "t1".into(),
            allowed_regions: vec![DataRegion::UsEast],
            primary_region: DataRegion::UsEast,
            replicate_across_regions: false,
            enforce_strict: true,
        });
        store.add_node(StorageNode {
            id: "n1".into(),
            region: DataRegion::UsEast,
            endpoint: "https://s3.us-east.example.com".into(),
            capacity_bytes: 1_000_000,
            used_bytes: 500_000,
            healthy: true,
        });
        let node = store.select_node("t1").unwrap();
        assert_eq!(node.id, "n1");
    }

    #[test]
    fn test_compliance_audit() {
        let mut store = GeofenceStore::new();
        store.set_policy(ResidencyPolicy {
            tenant_id: "strict".into(),
            allowed_regions: vec![DataRegion::EuCentral],
            primary_region: DataRegion::EuCentral,
            replicate_across_regions: false,
            enforce_strict: true,
        });
        let report = store.audit_compliance("strict");
        assert!(report.compliant);
    }
}
