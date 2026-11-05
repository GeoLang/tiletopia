use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Json},
    routing::get,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use tiletopia_terrain::global_dem::{TerrainTileCoord, sampled_terrain_tile};
use uuid::Uuid;

use crate::elevation::JOB_BUDGET_BYTES;
use crate::terrain_api::{
    AvailableRange, TerrainLayerInfo, encode_quantized_mesh, terrain_grid_size,
};
use crate::{AppState, http_cache::CachePolicy, may_view_asset, users};

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

async fn list_bundles(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let root = state.data_dir.join(BUNDLE_ROOT);
    let mut names = bundle_names(&root).await.map_err(|error| {
        tracing::warn!(
            "terrain bundles: {} could not be read: {error}",
            root.display()
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let claims = headers
        .contains_key(header::AUTHORIZATION)
        .then(|| users::claims_from_headers(&headers))
        .transpose()?;
    let viewable_asset_ids: HashSet<Uuid> = match &claims {
        None => HashSet::new(),
        Some(claims) => state
            .db
            .list_assets()
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .into_iter()
            .filter(|asset| may_view_asset(claims, asset.owner_id.as_deref()))
            .map(|asset| asset.id)
            .collect(),
    };
    // asset_bundle_dir names a DEM upload's bundle after its asset id
    names.retain(|name| match Uuid::parse_str(name) {
        Ok(asset_id) => viewable_asset_ids.contains(&asset_id),
        Err(_) => true,
    });

    let policy = if claims.is_some() {
        CachePolicy::no_cache()
    } else {
        CachePolicy::metadata()
    };
    let mut response_headers = cache_headers(policy);
    // browsers reuse a cached anonymous list for a tokened fetch of the same url without this
    response_headers.insert(header::VARY, HeaderValue::from_static("authorization"));
    Ok((response_headers, Json(names)))
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

    let bytes = tokio::fs::read(tile_path(&dir, z, x, y))
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

fn tile_path(bundle_dir: &std::path::Path, z: u32, x: u32, y: u32) -> std::path::PathBuf {
    bundle_dir
        .join(z.to_string())
        .join(x.to_string())
        .join(format!("{y}.terrain"))
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

pub(crate) fn asset_bundle_dir(data_dir: &std::path::Path, asset_id: Uuid) -> std::path::PathBuf {
    data_dir.join(BUNDLE_ROOT).join(asset_id.to_string())
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

// a decoded TIFF sample of up to 8 bytes beside its f64 copy, or two f64 grids while reprojecting
const DEM_PEAK_BYTES_PER_SAMPLE: u64 = 16;
const MAX_DEM_SAMPLES: u64 = JOB_BUDGET_BYTES / DEM_PEAK_BYTES_PER_SAMPLE;

const WORLD_BOUNDS: [f64; 4] = [-180.0, -90.0, 180.0, 90.0];

pub(crate) fn write_dem_bundle(
    input_path: &std::path::Path,
    staging_dir: &std::path::Path,
    bundle_dir: &std::path::Path,
    upload_crs: Option<&str>,
) -> Result<u64, String> {
    let fallback_source_epsg = upload_crs
        .map(|crs| {
            tiletopia_ingest::crs_detect::parse_epsg_code(crs).ok_or_else(|| {
                format!(
                    "dem_crs_unsupported: crs {crs}: DEMs take an EPSG code, such as EPSG:32632"
                )
            })
        })
        .transpose()?;
    let dem = tiletopia_ingest::dem_wgs84::read_dem_wgs84(
        input_path,
        fallback_source_epsg,
        MAX_DEM_SAMPLES,
    )
    .map_err(|error| error.to_string())?;

    let write_failed =
        |error: std::io::Error| format!("writing the terrain bundle failed: {error}");
    let tiles = match write_tile_tree(&dem, staging_dir) {
        Ok(tiles) => tiles,
        Err(error) => {
            let _ = std::fs::remove_dir_all(staging_dir);
            return Err(write_failed(error));
        }
    };

    // remove_dir_all fails on a directory that does not exist
    std::fs::create_dir_all(bundle_dir).map_err(write_failed)?;
    std::fs::remove_dir_all(bundle_dir).map_err(write_failed)?;
    std::fs::rename(staging_dir, bundle_dir).map_err(write_failed)?;
    Ok(tiles)
}

const UNCOVERED_GROUND_ELEVATION_METRES: f64 = 0.0;

fn write_tile_tree(
    dem: &tiletopia_ingest::Heightmap,
    dir: &std::path::Path,
) -> std::io::Result<u64> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;

    let deepest_level = deepest_level(dem);
    let mut available = Vec::new();
    let mut tiles = 0;
    for level in 0..=deepest_level {
        // cesium fails on a root tile that has no data
        let covered = if level == 0 { WORLD_BOUNDS } else { dem.bounds };
        let coords = TerrainTileCoord::tiles_in_bounds(level, covered);
        let grid_size = terrain_grid_size(level);
        for coord in &coords {
            // read_dem_wgs84 fills voids near valid samples, so None is ground with no sample
            let mesh = sampled_terrain_tile(coord, grid_size, |lat, lon| {
                dem.elevation_at(lon, lat)
                    .unwrap_or(UNCOVERED_GROUND_ELEVATION_METRES)
            });
            let path = tile_path(dir, coord.zoom, coord.x, coord.y);
            if let Some(column) = path.parent() {
                std::fs::create_dir_all(column)?;
            }
            std::fs::write(path, encode_quantized_mesh(&mesh, coord))?;
        }
        tiles += coords.len() as u64;
        let (first, last) = (&coords[0], &coords[coords.len() - 1]);
        available.push(vec![AvailableRange {
            start_x: first.x,
            start_y: first.y,
            end_x: last.x,
            end_y: last.y,
        }]);
    }

    let layer = TerrainLayerInfo {
        tilejson: "2.1.0",
        name: "TileTopia uploaded DEM",
        description: "Quantized mesh tiled from a DEM upload",
        version: "1.0.0",
        format: "quantized-mesh-1.0",
        projection: "EPSG:4326",
        scheme: "tms",
        tiles: vec![format!("{TILE_TEMPLATE}?v={{version}}")],
        minzoom: 0,
        maxzoom: deepest_level,
        bounds: dem.bounds,
        available,
    };
    std::fs::write(dir.join(LAYER_JSON), serde_json::to_vec(&layer)?)?;
    Ok(tiles)
}

fn deepest_level(dem: &tiletopia_ingest::Heightmap) -> u32 {
    let [west, south, east, north] = dem.bounds;
    let dem_spacing =
        ((east - west) / (dem.width - 1) as f64).max((north - south) / (dem.height - 1) as f64);
    (0..MAX_BUNDLE_LEVEL)
        .find(|&level| {
            let tile_span = 180.0 / 2f64.powi(level as i32);
            tile_span / (terrain_grid_size(level) - 1) as f64 <= dem_spacing
        })
        .unwrap_or(MAX_BUNDLE_LEVEL)
}

pub(crate) fn cache_headers(policy: CachePolicy) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&policy.cache_control_header()) {
        headers.insert(header::CACHE_CONTROL, value);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dem_with_spacing(spacing_degrees: f64) -> tiletopia_ingest::Heightmap {
        tiletopia_ingest::Heightmap {
            width: 3,
            height: 3,
            elevations: vec![0.0; 9],
            bounds: [
                10.0,
                45.0,
                10.0 + 2.0 * spacing_degrees,
                45.0 + 2.0 * spacing_degrees,
            ],
            nodata: None,
        }
    }

    #[test]
    fn deepest_level_matches_the_dem_spacing() {
        // one arc-second SRTM needs level 14 of 65 by 65 tiles: 180 / 2^14 / 64 is under 1 / 3600
        assert_eq!(deepest_level(&dem_with_spacing(1.0 / 3600.0)), 14);
        assert_eq!(deepest_level(&dem_with_spacing(45.0)), 0);
        assert_eq!(deepest_level(&dem_with_spacing(1e-9)), MAX_BUNDLE_LEVEL);
    }

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
