//! Offline viewer export.
//!
//! Bundles a tileset with a CesiumJS viewer page, for delivery on a USB stick or
//! into a network with no route out.
//!
//! The bundle carries CesiumJS itself only when [`OfflineExportConfig`] names a
//! local build to copy in. The Docker image ships one and points
//! `TILETOPIA_CESIUM_DIR` at it. With no build to copy, the page loads CesiumJS
//! from cesium.com and needs the network the first time it opens, and it says
//! so on screen.

use std::path::{Path, PathBuf};

/// The CesiumJS build the viewer page falls back to when the bundle carries no
/// copy of the library.
const CESIUM_CDN_BASE: &str = "https://cesium.com/downloads/cesiumjs/releases/1.119/Build/Cesium";

/// Where a copied CesiumJS build sits inside the bundle, and where the page
/// looks for one.
const CESIUM_DIR: &str = "cesium";

/// Names that sit beside the tiles in an asset directory and have no place in a
/// viewer bundle: the original upload, and the external tiler's scratch space.
const NOT_PART_OF_THE_TILESET: &[&str] = &["input", "temp"];

/// Configuration for offline export.
#[derive(Debug, Clone)]
pub struct OfflineExportConfig {
    /// Title shown in the viewer.
    pub title: String,
    /// A CesiumJS `Build/Cesium` directory to copy into the bundle, which is
    /// what makes the bundle work with no network. Without one the page falls
    /// back to cesium.com.
    pub cesium_build_dir: Option<PathBuf>,
    /// Base path for tile URLs in the exported viewer.
    pub base_path: String,
    /// Optional Cesium Ion access token (for base imagery only).
    pub cesium_token: Option<String>,
}

impl Default for OfflineExportConfig {
    fn default() -> Self {
        Self {
            title: "TileTopia Offline Viewer".into(),
            cesium_build_dir: None,
            base_path: "./tiles".into(),
            cesium_token: None,
        }
    }
}

/// Export a tileset directory as an offline viewer bundle: `index.html`, the
/// tiles under `tiles/`, a `serve.py` that serves the directory over HTTP, and a
/// copy of CesiumJS under `cesium/` when the config names a build.
pub fn export_offline_viewer(
    tileset_dir: &Path,
    output_dir: &Path,
    config: &OfflineExportConfig,
) -> std::io::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;
    copy_tileset(tileset_dir, &tiles_dir)?;

    let carries_cesium = match &config.cesium_build_dir {
        Some(build) if build.is_dir() => {
            copy_dir_recursive(build, &output_dir.join(CESIUM_DIR))?;
            true
        }
        _ => false,
    };

    let html = generate_viewer_html(config, carries_cesium);
    std::fs::write(output_dir.join("index.html"), html)?;

    // browsers refuse module and worker loads over file://, so the bundle needs
    // something to serve it
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

fn generate_viewer_html(config: &OfflineExportConfig, carries_cesium: bool) -> String {
    let token_js = config.cesium_token.as_deref().map_or(
        "// No Cesium Ion token — using offline imagery".to_string(),
        |t| format!("Cesium.Ion.defaultAccessToken = '{}';", t),
    );

    // CesiumJS otherwise derives where its workers and assets live from its own
    // script tag, which a viewer that reorders the head would break
    let (base_url_js, script_src, widgets_href) = if carries_cesium {
        (
            format!("<script>window.CESIUM_BASE_URL = './{CESIUM_DIR}/';</script>\n"),
            format!("./{CESIUM_DIR}/Cesium.js"),
            format!("./{CESIUM_DIR}/Widgets/widgets.css"),
        )
    } else {
        (
            String::new(),
            format!("{CESIUM_CDN_BASE}/Cesium.js"),
            format!("{CESIUM_CDN_BASE}/Widgets/widgets.css"),
        )
    };

    // in the copied case the note stays hidden unless the copy fails to load
    let (note_hidden, note) = if carries_cesium {
        (
            " hidden",
            format!(
                "CesiumJS did not load from ./{CESIUM_DIR}/Cesium.js, so this bundle is incomplete."
            ),
        )
    } else {
        (
            "",
            format!(
                "This bundle carries no copy of CesiumJS: the page loads it from {CESIUM_CDN_BASE}, so it needs network access the first time it opens. To make it work offline, put a CesiumJS Build/Cesium directory beside index.html as ./{CESIUM_DIR} and reload."
            ),
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>{title}</title>
{base_url_js}<script src="{script_src}"></script>
<link href="{widgets_href}" rel="stylesheet">
<style>
html, body, #cesiumContainer {{ width: 100%; height: 100%; margin: 0; padding: 0; overflow: hidden; }}
#cesium-note {{ position: absolute; z-index: 1; left: 0; right: 0; top: 0; margin: 0; padding: 12px 16px;
  font: 14px/1.4 sans-serif; color: #111; background: #ffd; border-bottom: 1px solid #cc9; }}
#cesium-note[hidden] {{ display: none; }}
</style>
</head>
<body>
<p id="cesium-note"{note_hidden}>{note}</p>
<div id="cesiumContainer"></div>
<script>
const note = document.getElementById('cesium-note');
if (typeof Cesium === 'undefined') {{
    note.hidden = false;
}} else {{
    note.remove();
{token_js}
    const viewer = new Cesium.Viewer('cesiumContainer', {{
        terrain: undefined,
        baseLayer: false,
    }});
    Cesium.Cesium3DTileset.fromUrl('{base_path}/tileset.json').then(tileset => {{
        viewer.scene.primitives.add(tileset);
        viewer.zoomTo(tileset);
    }});
}}
</script>
</body>
</html>"#,
        title = config.title,
        base_url_js = base_url_js,
        script_src = script_src,
        widgets_href = widgets_href,
        note_hidden = note_hidden,
        note = note,
        token_js = token_js,
        base_path = config.base_path,
    )
}

/// Copy a tileset, leaving behind the things an asset directory keeps beside it.
fn copy_tileset(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if NOT_PART_OF_THE_TILESET
            .iter()
            .any(|skipped| name.as_os_str() == *skipped)
        {
            continue;
        }
        let dest_path = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
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

    /// Stage an asset directory the way the tilers leave one: the tileset beside
    /// the upload it was built from.
    fn stage_asset_dir(dir: &Path) {
        fs::create_dir_all(dir.join("tiles")).unwrap();
        fs::create_dir_all(dir.join("input")).unwrap();
        fs::write(dir.join("tileset.json"), r#"{"asset":{}}"#).unwrap();
        fs::write(dir.join("tiles/0.pnts"), b"tile").unwrap();
        fs::write(dir.join("input/cloud.las"), b"the original upload").unwrap();
    }

    #[test]
    fn a_bundle_with_no_cesium_copy_says_it_needs_the_network() {
        let html = generate_viewer_html(&OfflineExportConfig::default(), false);
        assert!(html.contains("cesiumContainer"));
        assert!(html.contains("./tiles/tileset.json"));
        assert!(html.contains(CESIUM_CDN_BASE));
        assert!(html.contains("needs network access"), "{html}");
        // the note is on screen from the start, so a page that never loads
        // Cesium still explains itself
        assert!(html.contains(r#"<p id="cesium-note">"#), "{html}");
    }

    #[test]
    fn a_bundle_carrying_cesium_names_no_remote_host() {
        let config = OfflineExportConfig {
            cesium_build_dir: Some(PathBuf::from("/somewhere/Build/Cesium")),
            ..Default::default()
        };
        let html = generate_viewer_html(&config, true);
        assert!(html.contains("./cesium/Cesium.js"));
        assert!(html.contains("./cesium/Widgets/widgets.css"));
        assert!(html.contains("window.CESIUM_BASE_URL = './cesium/';"));
        assert!(!html.contains("https://"), "{html}");
        assert!(html.contains(r#"<p id="cesium-note" hidden>"#), "{html}");
    }

    #[test]
    fn the_bundle_holds_the_tileset_and_not_the_upload() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("asset");
        let out = tmp.path().join("output");
        stage_asset_dir(&src);

        let config = OfflineExportConfig {
            title: "Test Export".into(),
            ..Default::default()
        };
        export_offline_viewer(&src, &out, &config).unwrap();

        assert!(out.join("index.html").exists());
        assert!(out.join("serve.py").exists());
        assert!(out.join("tiles/tileset.json").exists());
        assert!(out.join("tiles/tiles/0.pnts").exists());
        assert!(!out.join("tiles/input").exists());
        assert!(!out.join(CESIUM_DIR).exists());
    }

    #[test]
    fn a_named_cesium_build_is_copied_into_the_bundle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("asset");
        let out = tmp.path().join("output");
        let build = tmp.path().join("Build/Cesium");
        stage_asset_dir(&src);
        fs::create_dir_all(build.join("Widgets")).unwrap();
        fs::write(build.join("Cesium.js"), b"// the library").unwrap();
        fs::write(build.join("Widgets/widgets.css"), b"/* widgets */").unwrap();

        let config = OfflineExportConfig {
            cesium_build_dir: Some(build),
            ..Default::default()
        };
        export_offline_viewer(&src, &out, &config).unwrap();

        assert!(out.join("cesium/Cesium.js").exists());
        assert!(out.join("cesium/Widgets/widgets.css").exists());
        let html = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(!html.contains("cesium.com"), "{html}");
    }

    /// A configured directory that is not there leaves the page on the CDN
    /// rather than pointing it at a copy the bundle does not hold.
    #[test]
    fn a_missing_cesium_build_falls_back_to_the_cdn() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("asset");
        let out = tmp.path().join("output");
        stage_asset_dir(&src);

        let config = OfflineExportConfig {
            cesium_build_dir: Some(tmp.path().join("no-such-build")),
            ..Default::default()
        };
        export_offline_viewer(&src, &out, &config).unwrap();

        assert!(!out.join(CESIUM_DIR).exists());
        let html = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(html.contains(CESIUM_CDN_BASE), "{html}");
    }
}
