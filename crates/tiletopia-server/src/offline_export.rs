//! Offline viewer export.
//!
//! Bundles a tileset with a self-contained CesiumJS viewer
//! for offline delivery (USB stick, air-gapped environments).

use std::path::Path;

/// Configuration for offline export.
#[derive(Debug, Clone)]
pub struct OfflineExportConfig {
    /// Title shown in the viewer.
    pub title: String,
    /// Whether to include the full CesiumJS library.
    pub include_cesium: bool,
    /// Base path for tile URLs in the exported viewer.
    pub base_path: String,
    /// Optional Cesium Ion access token (for base imagery only).
    pub cesium_token: Option<String>,
}

impl Default for OfflineExportConfig {
    fn default() -> Self {
        Self {
            title: "TileTopia Offline Viewer".into(),
            include_cesium: true,
            base_path: "./tiles".into(),
            cesium_token: None,
        }
    }
}

/// Export a tileset directory as a self-contained offline viewer.
pub fn export_offline_viewer(
    tileset_dir: &Path,
    output_dir: &Path,
    config: &OfflineExportConfig,
) -> std::io::Result<()> {
    // Create output structure
    std::fs::create_dir_all(output_dir)?;
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;

    // Copy tileset files
    copy_dir_recursive(tileset_dir, &tiles_dir)?;

    // Generate index.html
    let html = generate_viewer_html(config);
    std::fs::write(output_dir.join("index.html"), html)?;

    // Generate a simple HTTP server script for local viewing
    let server_script = r#"#!/usr/bin/env python3
"""Simple HTTP server for offline viewer. Run: python3 serve.py"""
import http.server, os
os.chdir(os.path.dirname(os.path.abspath(__file__)))
print("Serving at http://localhost:8080")
http.server.HTTPServer(("", 8080), http.server.SimpleHTTPRequestHandler).serve_forever()
"#;
    std::fs::write(output_dir.join("serve.py"), server_script)?;

    Ok(())
}

fn generate_viewer_html(config: &OfflineExportConfig) -> String {
    let token_js = config.cesium_token.as_deref().map_or(
        "// No Cesium Ion token — using offline imagery".to_string(),
        |t| format!("Cesium.Ion.defaultAccessToken = '{}';", t),
    );

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>{title}</title>
<script src="https://cesium.com/downloads/cesiumjs/releases/1.119/Build/Cesium/Cesium.js"></script>
<link href="https://cesium.com/downloads/cesiumjs/releases/1.119/Build/Cesium/Widgets/widgets.css" rel="stylesheet">
<style>
html, body, #cesiumContainer {{ width: 100%; height: 100%; margin: 0; padding: 0; overflow: hidden; }}
</style>
</head>
<body>
<div id="cesiumContainer"></div>
<script>
{token_js}
const viewer = new Cesium.Viewer('cesiumContainer', {{
    terrain: undefined,
    baseLayer: false,
}});
Cesium.Cesium3DTileset.fromUrl('{base_path}/tileset.json').then(tileset => {{
    viewer.scene.primitives.add(tileset);
    viewer.zoomTo(tileset);
}});
</script>
</body>
</html>"#,
        title = config.title,
        token_js = token_js,
        base_path = config.base_path,
    )
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_generate_viewer_html() {
        let config = OfflineExportConfig::default();
        let html = generate_viewer_html(&config);
        assert!(html.contains("cesiumContainer"));
        assert!(html.contains("tileset.json"));
    }

    #[test]
    fn test_export_offline_viewer() {
        let tmp = std::env::temp_dir().join("tiletopia_offline_test");
        let src = tmp.join("src_tiles");
        let out = tmp.join("output");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("tileset.json"), r#"{"asset":{}}"#).unwrap();

        let config = OfflineExportConfig {
            title: "Test Export".into(),
            ..Default::default()
        };
        export_offline_viewer(&src, &out, &config).unwrap();

        assert!(out.join("index.html").exists());
        assert!(out.join("tiles/tileset.json").exists());
        assert!(out.join("serve.py").exists());

        let _ = fs::remove_dir_all(&tmp);
    }
}
