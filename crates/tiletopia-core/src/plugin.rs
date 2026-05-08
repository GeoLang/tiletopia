//! Plugin system for custom ingest/export formats.
//!
//! Plugins are shared libraries (.so/.dylib/.dll) that implement the `TiletopiaPlugin` trait.

use std::path::Path;

/// Plugin metadata.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub supported_extensions: Vec<String>,
}

/// Point data produced by ingest plugins.
#[derive(Debug, Clone)]
pub struct PluginPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub attributes: Vec<(String, f64)>,
}

/// Plugin trait that custom format handlers implement.
pub trait IngestPlugin: Send + Sync {
    /// Return plugin metadata.
    fn info(&self) -> PluginInfo;

    /// Check if this plugin can handle the given file.
    fn can_handle(&self, path: &Path) -> bool;

    /// Read points from the given file.
    fn read_points(&self, path: &Path) -> Result<Vec<PluginPoint>, PluginError>;
}

/// Plugin trait for custom export formats.
pub trait ExportPlugin: Send + Sync {
    /// Return plugin metadata.
    fn info(&self) -> PluginInfo;

    /// Export tiles to custom format.
    fn export(&self, tileset_dir: &Path, output_dir: &Path) -> Result<(), PluginError>;
}

/// Plugin errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PluginError {
    #[error("plugin load error: {0}")]
    LoadError(String),
    #[error("format error: {0}")]
    FormatError(String),
    #[error("I/O error: {0}")]
    IoError(String),
}

/// Plugin registry that manages loaded plugins.
#[derive(Default)]
pub struct PluginRegistry {
    ingest_plugins: Vec<Box<dyn IngestPlugin>>,
    export_plugins: Vec<Box<dyn ExportPlugin>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an ingest plugin.
    pub fn register_ingest(&mut self, plugin: Box<dyn IngestPlugin>) {
        self.ingest_plugins.push(plugin);
    }

    /// Register an export plugin.
    pub fn register_export(&mut self, plugin: Box<dyn ExportPlugin>) {
        self.export_plugins.push(plugin);
    }

    /// Find an ingest plugin that can handle the given file.
    pub fn find_ingest(&self, path: &Path) -> Option<&dyn IngestPlugin> {
        self.ingest_plugins.iter().find(|p| p.can_handle(path)).map(|p| p.as_ref())
    }

    /// List all registered ingest plugins.
    pub fn list_ingest(&self) -> Vec<PluginInfo> {
        self.ingest_plugins.iter().map(|p| p.info()).collect()
    }

    /// List all registered export plugins.
    pub fn list_export(&self) -> Vec<PluginInfo> {
        self.export_plugins.iter().map(|p| p.info()).collect()
    }

    /// Load a plugin from a shared library path.
    ///
    /// # Safety
    /// Loading shared libraries is inherently unsafe. Only load trusted plugins.
    #[cfg(feature = "plugin-dylib")]
    pub unsafe fn load_dylib(&mut self, _path: &Path) -> Result<(), PluginError> {
        // Placeholder: would use libloading to load .so/.dylib/.dll
        Err(PluginError::LoadError("dylib loading not yet implemented".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPlugin;
    impl IngestPlugin for TestPlugin {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "test".into(),
                version: "0.1.0".into(),
                description: "Test plugin".into(),
                supported_extensions: vec!["xyz".into()],
            }
        }
        fn can_handle(&self, path: &Path) -> bool {
            path.extension().map_or(false, |e| e == "xyz")
        }
        fn read_points(&self, _path: &Path) -> Result<Vec<PluginPoint>, PluginError> {
            Ok(vec![PluginPoint { x: 1.0, y: 2.0, z: 3.0, r: 255, g: 0, b: 0, attributes: vec![] }])
        }
    }

    #[test]
    fn test_plugin_registry() {
        let mut reg = PluginRegistry::new();
        reg.register_ingest(Box::new(TestPlugin));
        assert_eq!(reg.list_ingest().len(), 1);
        assert!(reg.find_ingest(Path::new("test.xyz")).is_some());
        assert!(reg.find_ingest(Path::new("test.las")).is_none());
    }
}
