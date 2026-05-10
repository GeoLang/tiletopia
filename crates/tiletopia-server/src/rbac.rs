//! Role-Based Access Control (RBAC) with OIDC/SAML support.
//!
//! Uses the casbin policy engine for flexible ACL/RBAC/ABAC models.

use std::collections::{HashMap, HashSet};

/// Permission level for a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    View,
    Edit,
    Delete,
    Admin,
}

/// A role with associated permissions.
#[derive(Debug, Clone)]
pub struct Role {
    pub name: String,
    pub permissions: HashSet<Permission>,
    pub description: String,
}

/// A user identity.
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub roles: Vec<String>,
    pub org_id: Option<String>,
    pub auth_provider: AuthProvider,
}

/// Authentication provider.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthProvider {
    Local,
    Oidc { issuer: String },
    Saml { idp_url: String },
    Ldap { server: String },
}

/// OIDC configuration.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

/// RBAC policy store.
#[derive(Debug, Clone)]
pub struct RbacStore {
    roles: HashMap<String, Role>,
    user_roles: HashMap<String, Vec<String>>,
    resource_policies: HashMap<String, Vec<ResourcePolicy>>,
}

/// Policy binding a role to a resource.
#[derive(Debug, Clone)]
pub struct ResourcePolicy {
    pub role: String,
    pub resource_pattern: String, // glob pattern
    pub permissions: HashSet<Permission>,
}

impl RbacStore {
    pub fn new() -> Self {
        let mut store = Self {
            roles: HashMap::new(),
            user_roles: HashMap::new(),
            resource_policies: HashMap::new(),
        };
        // Default roles
        store.add_role(Role {
            name: "viewer".into(),
            permissions: [Permission::View].into_iter().collect(),
            description: "Read-only access".into(),
        });
        store.add_role(Role {
            name: "editor".into(),
            permissions: [Permission::View, Permission::Edit].into_iter().collect(),
            description: "View and edit access".into(),
        });
        store.add_role(Role {
            name: "admin".into(),
            permissions: [
                Permission::View,
                Permission::Edit,
                Permission::Delete,
                Permission::Admin,
            ]
            .into_iter()
            .collect(),
            description: "Full administrative access".into(),
        });
        store
    }

    pub fn add_role(&mut self, role: Role) {
        self.roles.insert(role.name.clone(), role);
    }

    pub fn assign_role(&mut self, user_id: &str, role: &str) {
        self.user_roles
            .entry(user_id.to_string())
            .or_default()
            .push(role.to_string());
    }

    pub fn revoke_role(&mut self, user_id: &str, role: &str) {
        if let Some(roles) = self.user_roles.get_mut(user_id) {
            roles.retain(|r| r != role);
        }
    }

    pub fn add_resource_policy(&mut self, resource: &str, policy: ResourcePolicy) {
        self.resource_policies
            .entry(resource.to_string())
            .or_default()
            .push(policy);
    }

    /// Check if user has permission on resource.
    pub fn check_permission(&self, user_id: &str, resource: &str, required: &Permission) -> bool {
        let user_roles = match self.user_roles.get(user_id) {
            Some(r) => r,
            None => return false,
        };

        for role_name in user_roles {
            if self
                .roles
                .get(role_name)
                .is_some_and(|role| role.permissions.contains(required))
            {
                return true;
            }
            // Check resource-specific policies
            if let Some(policies) = self.resource_policies.get(resource) {
                for policy in policies {
                    if policy.role == *role_name && policy.permissions.contains(required) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Get all permissions for a user on a resource.
    pub fn get_permissions(&self, user_id: &str, resource: &str) -> HashSet<Permission> {
        let mut perms = HashSet::new();
        let user_roles = match self.user_roles.get(user_id) {
            Some(r) => r,
            None => return perms,
        };

        for role_name in user_roles {
            if let Some(role) = self.roles.get(role_name) {
                perms.extend(role.permissions.iter().cloned());
            }
            if let Some(policies) = self.resource_policies.get(resource) {
                for policy in policies {
                    if policy.role == *role_name {
                        perms.extend(policy.permissions.iter().cloned());
                    }
                }
            }
        }
        perms
    }
}

impl Default for RbacStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate an OIDC token (simplified — in production use openidconnect crate).
pub fn validate_oidc_claims(
    claims_json: &str,
    expected_issuer: &str,
    expected_audience: &str,
) -> Result<OidcClaims, String> {
    let claims: serde_json::Value =
        serde_json::from_str(claims_json).map_err(|e| format!("Invalid JSON: {}", e))?;

    let iss = claims["iss"].as_str().ok_or("Missing iss claim")?;
    if iss != expected_issuer {
        return Err(format!("Issuer mismatch: {} != {}", iss, expected_issuer));
    }

    let aud = claims["aud"].as_str().ok_or("Missing aud claim")?;
    if aud != expected_audience {
        return Err(format!(
            "Audience mismatch: {} != {}",
            aud, expected_audience
        ));
    }

    let sub = claims["sub"].as_str().ok_or("Missing sub claim")?;
    let email = claims["email"].as_str().unwrap_or("");

    Ok(OidcClaims {
        subject: sub.to_string(),
        email: email.to_string(),
        issuer: iss.to_string(),
    })
}

/// Parsed OIDC claims.
#[derive(Debug, Clone)]
pub struct OidcClaims {
    pub subject: String,
    pub email: String,
    pub issuer: String,
}

/// Create a casbin enforcer from an RBAC model string and policy CSV string.
///
/// Model defines request/policy/role/matchers definitions.
/// Policy defines concrete role-permission assignments as CSV lines.
pub async fn create_enforcer(
    model_text: &str,
    policy_csv: &str,
) -> Result<casbin::Enforcer, String> {
    use casbin::prelude::*;
    let model = DefaultModel::from_str(model_text)
        .await
        .map_err(|e| format!("Invalid casbin model: {e}"))?;
    let adapter = StringAdapter::new(policy_csv.to_string());
    Enforcer::new(model, adapter)
        .await
        .map_err(|e| format!("Failed to create enforcer: {e}"))
}

/// Default RBAC model for TileTopia.
pub const RBAC_MODEL: &str = r#"
[request_definition]
r = sub, obj, act

[policy_definition]
p = sub, obj, act

[role_definition]
g = _, _

[policy_effect]
e = some(where (p.eft == allow))

[matchers]
m = g(r.sub, p.sub) && r.obj == p.obj && r.act == p.act
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rbac_default_roles() {
        let store = RbacStore::new();
        assert!(store.roles.contains_key("viewer"));
        assert!(store.roles.contains_key("editor"));
        assert!(store.roles.contains_key("admin"));
    }

    #[test]
    fn test_assign_and_check_permission() {
        let mut store = RbacStore::new();
        store.assign_role("user-1", "viewer");
        assert!(store.check_permission("user-1", "project-a", &Permission::View));
        assert!(!store.check_permission("user-1", "project-a", &Permission::Edit));
    }

    #[test]
    fn test_admin_has_all_permissions() {
        let mut store = RbacStore::new();
        store.assign_role("admin-1", "admin");
        assert!(store.check_permission("admin-1", "any", &Permission::View));
        assert!(store.check_permission("admin-1", "any", &Permission::Edit));
        assert!(store.check_permission("admin-1", "any", &Permission::Delete));
        assert!(store.check_permission("admin-1", "any", &Permission::Admin));
    }

    #[test]
    fn test_revoke_role() {
        let mut store = RbacStore::new();
        store.assign_role("user-2", "editor");
        assert!(store.check_permission("user-2", "x", &Permission::Edit));
        store.revoke_role("user-2", "editor");
        assert!(!store.check_permission("user-2", "x", &Permission::Edit));
    }

    #[test]
    fn test_validate_oidc_claims() {
        let claims = r#"{"iss": "https://auth.example.com", "aud": "tiletopia", "sub": "user123", "email": "user@example.com"}"#;
        let result = validate_oidc_claims(claims, "https://auth.example.com", "tiletopia");
        assert!(result.is_ok());
        let c = result.unwrap();
        assert_eq!(c.subject, "user123");
        assert_eq!(c.email, "user@example.com");
    }

    #[test]
    fn test_oidc_issuer_mismatch() {
        let claims = r#"{"iss": "https://evil.com", "aud": "tiletopia", "sub": "x"}"#;
        let result = validate_oidc_claims(claims, "https://auth.example.com", "tiletopia");
        assert!(result.is_err());
    }
}
