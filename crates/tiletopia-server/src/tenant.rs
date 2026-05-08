//! Multi-tenant isolation.
//!
//! Each tenant (org/project) has isolated asset storage and access control.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A tenant represents an isolated organization or project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub max_storage_bytes: u64,
    pub used_storage_bytes: u64,
    pub max_assets: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Tenant-scoped request context.
#[derive(Debug, Clone)]
pub struct TenantContext {
    pub tenant_id: Uuid,
    pub tenant_slug: String,
}

impl Tenant {
    pub fn new(name: String, slug: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            name,
            slug,
            max_storage_bytes: 100 * 1024 * 1024 * 1024, // 100 GB default
            used_storage_bytes: 0,
            max_assets: 1000,
            created_at: chrono::Utc::now(),
        }
    }

    /// Check if tenant has capacity for more storage.
    pub fn has_storage_capacity(&self, bytes: u64) -> bool {
        self.used_storage_bytes + bytes <= self.max_storage_bytes
    }

    /// Check if tenant has capacity for more assets.
    pub fn has_asset_capacity(&self, current_count: u32) -> bool {
        current_count < self.max_assets
    }

    /// Get the storage directory path for this tenant.
    pub fn storage_path(&self) -> String {
        format!("tenants/{}", self.id)
    }
}

/// Extract tenant context from request headers.
pub fn extract_tenant_from_header(header_value: &str) -> Option<TenantContext> {
    let tenant_id = Uuid::parse_str(header_value).ok()?;
    Some(TenantContext {
        tenant_id,
        tenant_slug: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tenant_creation() {
        let t = Tenant::new("Acme Corp".into(), "acme".into());
        assert_eq!(t.name, "Acme Corp");
        assert!(t.has_storage_capacity(1024));
    }

    #[test]
    fn test_storage_capacity() {
        let mut t = Tenant::new("Test".into(), "test".into());
        t.max_storage_bytes = 1000;
        t.used_storage_bytes = 900;
        assert!(t.has_storage_capacity(100));
        assert!(!t.has_storage_capacity(101));
    }

    #[test]
    fn test_extract_tenant_header() {
        let id = Uuid::new_v4();
        let ctx = extract_tenant_from_header(&id.to_string()).unwrap();
        assert_eq!(ctx.tenant_id, id);
        assert!(extract_tenant_from_header("not-a-uuid").is_none());
    }
}
