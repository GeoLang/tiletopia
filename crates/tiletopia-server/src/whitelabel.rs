//! White-label / custom branding for customer-facing portals.

use serde::{Deserialize, Serialize};

/// Branding configuration for a tenant's portal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrandingConfig {
    pub tenant_id: String,
    pub app_name: String,
    pub logo_url: Option<String>,
    pub favicon_url: Option<String>,
    pub primary_color: String,
    pub secondary_color: String,
    pub accent_color: String,
    pub font_family: String,
    pub custom_domain: Option<String>,
    pub custom_css: Option<String>,
    pub footer_text: Option<String>,
    pub support_email: Option<String>,
    pub hide_powered_by: bool,
}

impl Default for BrandingConfig {
    fn default() -> Self {
        Self {
            tenant_id: String::new(),
            app_name: "TileTopia".into(),
            logo_url: None,
            favicon_url: None,
            primary_color: "#2563eb".into(),
            secondary_color: "#1e40af".into(),
            accent_color: "#f59e0b".into(),
            font_family: "Inter, system-ui, sans-serif".into(),
            custom_domain: None,
            custom_css: None,
            footer_text: None,
            support_email: None,
            hide_powered_by: false,
        }
    }
}

/// Generate CSS variables from branding config.
pub fn generate_css_variables(config: &BrandingConfig) -> String {
    let mut css = String::from(":root {\n");
    css.push_str(&format!("  --primary-color: {};\n", config.primary_color));
    css.push_str(&format!(
        "  --secondary-color: {};\n",
        config.secondary_color
    ));
    css.push_str(&format!("  --accent-color: {};\n", config.accent_color));
    css.push_str(&format!("  --font-family: {};\n", config.font_family));
    css.push_str("}\n");
    if let Some(ref custom) = config.custom_css {
        css.push_str("\n/* Custom CSS */\n");
        css.push_str(custom);
    }
    css
}

/// Generate HTML head meta tags for branding.
pub fn generate_html_head(config: &BrandingConfig) -> String {
    let mut html = String::new();
    html.push_str(&format!("  <title>{}</title>\n", config.app_name));
    if let Some(ref favicon) = config.favicon_url {
        html.push_str(&format!("  <link rel=\"icon\" href=\"{}\" />\n", favicon));
    }
    html.push_str(&format!(
        "  <style>{}</style>\n",
        generate_css_variables(config)
    ));
    html
}

/// Domain routing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainMapping {
    pub domain: String,
    pub tenant_id: String,
    pub ssl_cert_path: Option<String>,
    pub ssl_key_path: Option<String>,
}

/// Branding store for multi-tenant white-labeling.
#[derive(Debug, Clone, Default)]
pub struct BrandingStore {
    configs: std::collections::HashMap<String, BrandingConfig>,
    domain_mappings: Vec<DomainMapping>,
}

impl BrandingStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_branding(&mut self, config: BrandingConfig) {
        self.configs.insert(config.tenant_id.clone(), config);
    }

    pub fn get_branding(&self, tenant_id: &str) -> Option<&BrandingConfig> {
        self.configs.get(tenant_id)
    }

    pub fn add_domain_mapping(&mut self, mapping: DomainMapping) {
        self.domain_mappings.push(mapping);
    }

    pub fn resolve_domain(&self, domain: &str) -> Option<&str> {
        self.domain_mappings
            .iter()
            .find(|m| m.domain == domain)
            .map(|m| m.tenant_id.as_str())
    }

    pub fn tenant_count(&self) -> usize {
        self.configs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_branding() {
        let config = BrandingConfig::default();
        assert_eq!(config.app_name, "TileTopia");
        assert_eq!(config.primary_color, "#2563eb");
        assert!(!config.hide_powered_by);
    }

    #[test]
    fn test_generate_css() {
        let config = BrandingConfig {
            primary_color: "#ff0000".into(),
            secondary_color: "#00ff00".into(),
            accent_color: "#0000ff".into(),
            ..Default::default()
        };
        let css = generate_css_variables(&config);
        assert!(css.contains("--primary-color: #ff0000"));
        assert!(css.contains("--accent-color: #0000ff"));
    }

    #[test]
    fn test_branding_store() {
        let mut store = BrandingStore::new();
        store.set_branding(BrandingConfig {
            tenant_id: "acme".into(),
            app_name: "ACME 3D".into(),
            ..Default::default()
        });
        let b = store.get_branding("acme").unwrap();
        assert_eq!(b.app_name, "ACME 3D");
    }

    #[test]
    fn test_domain_mapping() {
        let mut store = BrandingStore::new();
        store.add_domain_mapping(DomainMapping {
            domain: "3d.acme.com".into(),
            tenant_id: "acme".into(),
            ssl_cert_path: None,
            ssl_key_path: None,
        });
        assert_eq!(store.resolve_domain("3d.acme.com"), Some("acme"));
        assert_eq!(store.resolve_domain("unknown.com"), None);
    }

    #[test]
    fn test_html_head_generation() {
        let config = BrandingConfig {
            app_name: "MyGIS".into(),
            favicon_url: Some("/favicon.ico".into()),
            ..Default::default()
        };
        let html = generate_html_head(&config);
        assert!(html.contains("<title>MyGIS</title>"));
        assert!(html.contains("favicon.ico"));
    }
}
