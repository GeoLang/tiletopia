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
    #[cfg(feature = "plugin-dylib")]
    loaded_libraries: Vec<libloading::Library>,
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
        self.ingest_plugins
            .iter()
            .find(|p| p.can_handle(path))
            .map(|p| p.as_ref())
    }

    /// List all registered ingest plugins.
    pub fn list_ingest(&self) -> Vec<PluginInfo> {
        self.ingest_plugins.iter().map(|p| p.info()).collect()
    }

    /// List all registered export plugins.
    pub fn list_export(&self) -> Vec<PluginInfo> {
        self.export_plugins.iter().map(|p| p.info()).collect()
    }

    /// Load an ingest plugin from a shared library path.
    ///
    /// The library must export a `tiletopia_plugin_create` symbol with signature
    /// `fn() -> Box<dyn IngestPlugin>`. The loaded library is kept alive for the
    /// lifetime of this registry.
    ///
    /// # Safety
    ///
    /// Loading shared libraries is inherently unsafe:
    /// - The library must be compiled with the same Rust compiler version and ABI.
    /// - The `tiletopia_plugin_create` symbol must return a valid `Box<dyn IngestPlugin>`.
    /// - Only load trusted plugins — a malicious library can execute arbitrary code.
    #[cfg(feature = "plugin-dylib")]
    pub unsafe fn load_dylib(&mut self, path: &Path) -> Result<(), PluginError> {
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| PluginError::LoadError(format!("failed to load library: {e}")))?;

        let plugin: Box<dyn IngestPlugin> = unsafe {
            let create_fn: libloading::Symbol<unsafe fn() -> Box<dyn IngestPlugin>> =
                lib.get(b"tiletopia_plugin_create").map_err(|e| {
                    PluginError::LoadError(format!(
                        "symbol 'tiletopia_plugin_create' not found: {e}"
                    ))
                })?;
            create_fn()
        };

        self.register_ingest(plugin);
        self.loaded_libraries.push(lib);
        Ok(())
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
            path.extension().is_some_and(|e| e == "xyz")
        }
        fn read_points(&self, _path: &Path) -> Result<Vec<PluginPoint>, PluginError> {
            Ok(vec![PluginPoint {
                x: 1.0,
                y: 2.0,
                z: 3.0,
                r: 255,
                g: 0,
                b: 0,
                attributes: vec![],
            }])
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

    #[test]
    #[cfg(feature = "plugin-dylib")]
    fn test_load_dylib_nonexistent_path() {
        let mut reg = PluginRegistry::new();
        let result = unsafe { reg.load_dylib(Path::new("/nonexistent/plugin.so")) };
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::LoadError(msg) => {
                assert!(msg.contains("failed to load library"));
            }
            other => panic!("expected LoadError, got {other:?}"),
        }
    }
}
