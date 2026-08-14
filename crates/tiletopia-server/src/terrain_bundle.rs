//! Prebuilt quantized-mesh terrain bundles.
//!
//! A bundle is the output of an external tiler (ctb-tile and friends) copied
//! under `<data-dir>/terrain_bundles/<name>/`: a `layer.json` beside a
//! `{z}/{x}/{y}.terrain` tree. Serving one gives CesiumJS a terrain source that
//! needs no Ion token and no network beyond this server, which the on-demand
//! `/api/v1/terrain/` routes cannot do because they reach for SRTM.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Json},
    routing::get,
};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{AppState, http_cache::CachePolicy};

/// Directory under the data dir that holds every bundle.
const BUNDLE_ROOT: &str = "terrain_bundles";

const LAYER_JSON: &str = "layer.json";

/// Format prefix these routes serve, matching the one CesiumTerrainProvider
/// accepts. Anything else, `heightmap-1.0` included, is refused rather than
/// served under a content type that does not describe it.
const QUANTIZED_MESH_PREFIX: &str = "quantized-mesh-1.";

/// Tiling schemes CesiumTerrainProvider accepts. It throws on any other value,
/// `xyz` included, so a bundle carrying one is refused here where the reason
/// can be logged instead of surfacing as a client-side RuntimeError.
const SUPPORTED_SCHEMES: [&str; 2] = ["tms", "slippyMap"];

/// Likewise for the projection, which also decides the root tile count: two
/// tiles across at level 0 for EPSG:4326, one for EPSG:3857.
const SUPPORTED_PROJECTIONS: [&str; 2] = ["EPSG:4326", "EPSG:3857"];

const QUANTIZED_MESH_CONTENT_TYPE: &str = "application/vnd.quantized-mesh";

/// Tile template written into every bundle's layer.json. Relative, so it
/// resolves against the layer.json URL behind any proxy prefix, and fixed, so a
/// bundle carrying an absolute URL cannot send Cesium back off this server.
const TILE_TEMPLATE: &str = "{z}/{x}/{y}.terrain";

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

pub fn terrain_bundle_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/terrain/bundles", get(list_bundles))
        .route(
            "/api/v1/terrain/bundles/{bundle}/layer.json",
            get(bundle_layer_json),
        )
        .route(
            "/api/v1/terrain/bundles/{bundle}/{z}/{x}/{y}",
            get(bundle_tile),
        )
    // /api/v1/terrain/{z}/{x}/{y} also matches three segments, but axum
    // prefers the literal "bundles" over its {z}, the same way the terrain-rgb
    // routes already sit beside it.
}

/// Names of the bundles this server hosts, so a viewer can offer them without
/// being told what an operator dropped on disk.
async fn list_bundles(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, StatusCode> {
    let root = state.data_dir.join(BUNDLE_ROOT);
    let names = bundle_names(&root).await.map_err(|error| {
        tracing::warn!(
            "terrain bundles: {} could not be read: {error}",
            root.display()
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok((cache_headers(CachePolicy::metadata()), Json(names)))
}

/// Sorted names of the bundles under `root`. A server that was never given a
/// bundles directory hosts none, so that miss is an empty list, but any other
/// read failure is an error rather than an empty list that hides it.
async fn bundle_names(root: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_bundle_name(&name) && entry.path().join(LAYER_JSON).is_file() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

async fn bundle_layer_json(
    State(state): State<Arc<AppState>>,
    Path(bundle): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let dir = bundle_dir(&state, &bundle)?;
    let raw = tokio::fs::read(dir.join(LAYER_JSON))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let mut layer: Value = serde_json::from_slice(&raw).map_err(|error| {
        tracing::warn!("terrain bundle {bundle}: layer.json is not JSON: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    normalize_layer_json(&mut layer).map_err(|reason| {
        tracing::warn!("terrain bundle {bundle}: {reason}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if layer.get("available").is_none() {
        layer["available"] = derive_availability(&dir).await;
    }

    Ok((cache_headers(CachePolicy::metadata()), Json(layer)))
}

async fn bundle_tile(
    State(state): State<Arc<AppState>>,
    Path((bundle, z, x, y)): Path<(String, u32, u32, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let dir = bundle_dir(&state, &bundle)?;
    let y: u32 = y
        .strip_suffix(".terrain")
        .unwrap_or(&y)
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let path = dir
        .join(z.to_string())
        .join(x.to_string())
        .join(format!("{y}.terrain"));
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let mut headers = cache_headers(CachePolicy::tile());
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(QUANTIZED_MESH_CONTENT_TYPE),
    );
    // tilers gzip their tiles in place, and the bytes are served as they were
    // written rather than decompressed on every read
    if bytes.starts_with(&GZIP_MAGIC) {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok((headers, bytes))
}

/// Whether a bundle by this name is on disk with the layer.json
/// CesiumTerrainProvider asks for first.
pub(crate) async fn bundle_exists(state: &AppState, bundle: &str) -> bool {
    let Ok(dir) = bundle_dir(state, bundle) else {
        return false;
    };
    tokio::fs::try_exists(dir.join(LAYER_JSON))
        .await
        .unwrap_or(false)
}

/// Directory of a bundle, or a refusal for a name that is not one path segment
/// of ordinary characters. Axum hands back a percent-decoded segment, so `..`
/// can arrive here even though the router never matches a slash.
fn bundle_dir(state: &AppState, bundle: &str) -> Result<std::path::PathBuf, StatusCode> {
    if !is_bundle_name(bundle) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(state.data_dir.join(BUNDLE_ROOT).join(bundle))
}

fn is_bundle_name(bundle: &str) -> bool {
    !bundle.is_empty()
        && !bundle.starts_with('.')
        && bundle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Rewrite the fields Cesium reads that a bundle must not be trusted with, and
/// refuse a bundle this route cannot serve.
fn normalize_layer_json(layer: &mut Value) -> Result<(), String> {
    let object = layer
        .as_object_mut()
        .ok_or_else(|| "layer.json is not an object".to_string())?;

    match object.get("format").and_then(Value::as_str) {
        Some(format) if format.starts_with(QUANTIZED_MESH_PREFIX) => {}
        Some(other) => return Err(format!("format {other} is not served here")),
        None => return Err("layer.json names no format".to_string()),
    }
    for (field, accepted) in [
        ("scheme", SUPPORTED_SCHEMES.as_slice()),
        ("projection", SUPPORTED_PROJECTIONS.as_slice()),
    ] {
        if let Some(value) = object.get(field).and_then(Value::as_str)
            && !accepted.contains(&value)
        {
            return Err(format!("{field} {value} is not one Cesium reads"));
        }
    }

    // Cesium appends the version as a cache-busting query parameter, so the
    // template only carries it when the bundle says which version it is.
    let tile_url = match object.get("version").and_then(Value::as_str) {
        Some(_) => format!("{TILE_TEMPLATE}?v={{version}}"),
        None => TILE_TEMPLATE.to_string(),
    };
    object.insert("tiles".to_string(), json!([tile_url]));
    Ok(())
}

/// Deepest level the availability walk looks for. Well past what a
/// quantized-mesh tiler emits, and only a directory probe per level.
const MAX_BUNDLE_LEVEL: u32 = 23;

/// Availability read off the tile tree, for a bundle whose layer.json carries
/// none. A quantized-mesh layer with no `available` loads and then throws on
/// the first tile, because CesiumTerrainProvider builds a child mask off an
/// availability object it only creates when this array is there.
///
/// Cesium indexes the array by level and takes its deepest level from the
/// array's length, so a level with no tiles has to stay in place as an empty
/// range rather than shift the deeper ones up. The y values are the ones on
/// disk, which the TMS layout already numbers from the south as Cesium expects
/// here.
async fn derive_availability(dir: &std::path::Path) -> Value {
    let mut levels = Vec::new();
    for level in 0..=MAX_BUNDLE_LEVEL {
        levels.push(scan_level(&dir.join(level.to_string())).await);
    }
    while levels.last() == Some(&None) {
        levels.pop();
    }

    let ranges: Vec<Value> = levels
        .iter()
        .map(|level| match level {
            Some([start_x, start_y, end_x, end_y]) => json!([{
                "startX": start_x,
                "startY": start_y,
                "endX": end_x,
                "endY": end_y,
            }]),
            None => json!([]),
        })
        .collect();
    Value::Array(ranges)
}

/// The x/y bounding range of one level directory, or `None` when it holds no
/// tiles at all.
async fn scan_level(level_dir: &std::path::Path) -> Option<[u32; 4]> {
    let mut range: Option<[u32; 4]> = None;
    let mut columns = tokio::fs::read_dir(level_dir).await.ok()?;
    while let Ok(Some(column)) = columns.next_entry().await {
        let Some(x) = column.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(mut rows) = tokio::fs::read_dir(column.path()).await else {
            continue;
        };
        while let Ok(Some(row)) = rows.next_entry().await {
            let Some(y) = row
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".terrain"))
                .and_then(|n| n.parse().ok())
            else {
                continue;
            };
            range = Some(match range {
                None => [x, y, x, y],
                Some([sx, sy, ex, ey]) => [sx.min(x), sy.min(y), ex.max(x), ey.max(y)],
            });
        }
    }
    range
}

fn cache_headers(policy: CachePolicy) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&policy.cache_control_header()) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_names_refuse_traversal() {
        assert!(is_bundle_name("alps"));
        assert!(is_bundle_name("alps-v2"));
        assert!(is_bundle_name("alps_2026.1"));

        assert!(!is_bundle_name(".."));
        assert!(!is_bundle_name(".hidden"));
        assert!(!is_bundle_name("a/b"));
        assert!(!is_bundle_name("a\\b"));
        assert!(!is_bundle_name(""));
    }

    #[test]
    fn layer_json_tiles_always_point_back_here() {
        let mut layer = json!({
            "format": "quantized-mesh-1.0",
            "version": "1.1.0",
            "tiles": ["https://elsewhere.example/{z}/{x}/{y}.terrain"],
        });
        normalize_layer_json(&mut layer).unwrap();
        assert_eq!(layer["tiles"][0], "{z}/{x}/{y}.terrain?v={version}");
    }

    #[test]
    fn versionless_bundles_lose_the_cache_buster() {
        let mut layer = json!({ "format": "quantized-mesh-1.0" });
        normalize_layer_json(&mut layer).unwrap();
        assert_eq!(layer["tiles"][0], "{z}/{x}/{y}.terrain");
    }

    #[test]
    fn format_matches_what_cesium_accepts() {
        // cesium takes any quantized-mesh-1.x, so a bundle from a newer tiler
        // is not turned away here
        let mut newer = json!({ "format": "quantized-mesh-1.1" });
        assert!(normalize_layer_json(&mut newer).is_ok());

        let mut heightmap = json!({ "format": "heightmap-1.0" });
        assert!(normalize_layer_json(&mut heightmap).is_err());

        let mut nameless = json!({ "version": "1.0.0" });
        assert!(normalize_layer_json(&mut nameless).is_err());
    }

    #[test]
    fn schemes_and_projections_cesium_throws_on_are_refused_here() {
        let good = json!({
            "format": "quantized-mesh-1.0",
            "scheme": "tms",
            "projection": "EPSG:4326",
        });
        assert!(normalize_layer_json(&mut good.clone()).is_ok());

        let mut xyz = good.clone();
        xyz["scheme"] = json!("xyz");
        assert!(normalize_layer_json(&mut xyz).is_err());

        let mut projected = good.clone();
        projected["projection"] = json!("EPSG:2193");
        assert!(normalize_layer_json(&mut projected).is_err());

        // both are optional, and cesium defaults them to tms and EPSG:4326
        let mut bare = json!({ "format": "quantized-mesh-1.0" });
        assert!(normalize_layer_json(&mut bare).is_ok());
    }
}
