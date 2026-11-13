use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use tiletopia_core::glb_writer::{
    GlbMesh, MetadataProperty, MetadataValues, TileMetadata, write_glb,
};
use tiletopia_core::spatial::geodetic_to_ecef;
use tokio::sync::Semaphore;

use crate::AppState;
use crate::global_terrain::GlobalTerrain;
use crate::http_cache::CachePolicy;
use crate::ion_compat::ion_asset_id_setting;
use crate::terrain_bundle::cache_headers;
use crate::tile_cache::{TileAddress, TileCache, cache_bytes_setting, setting_text};

pub const OVERPASS_URL_VAR: &str = "TILETOPIA_OSM_BUILDINGS_OVERPASS_URL";
pub const CACHE_DIR_VAR: &str = "TILETOPIA_OSM_BUILDINGS_CACHE_DIR";
pub const CACHE_MEGABYTES_VAR: &str = "TILETOPIA_OSM_BUILDINGS_CACHE_MB";
pub const ION_ASSET_ID_VAR: &str = "TILETOPIA_OSM_BUILDINGS_ION_ASSET_ID";

pub const TILESET_PATH: &str = "/api/v1/buildings/osm/tileset.json";
pub const COPYRIGHT: &str = "© OpenStreetMap contributors";

// a level 15 tile is 0.011 by 0.0055 degrees, 1.2 by 0.6 km at the equator
const LEAF_LEVEL: u32 = 15;
const SUBTREE_LEVELS: u32 = 4;
// cesium refines a level 14 tile of 50 m error within 2.7 km on a 1000 pixel tall view at screen space error 16
const ROOT_GEOMETRIC_ERROR: f64 = 50.0 * (1 << 14) as f64;
// the dead sea shore is near -430 m and the highest towns near 5100 m, plus the tallest tower
const LOWEST_HEIGHT: f64 = -500.0;
const HIGHEST_HEIGHT: f64 = 6000.0;

// the overpass-api.de usage policy asks clients to rate-limit their calls
const OVERPASS_SLOTS: usize = 2;
const MAX_OVERPASS_BYTES: usize = 32_000_000;
// overpass-api.de answers these when it is busy, and cesium never refetches a tile that failed
const RETRIED_OVERPASS_STATUSES: [StatusCode; 3] = [
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::BAD_GATEWAY,
    StatusCode::GATEWAY_TIMEOUT,
];
const OVERPASS_RETRY_PAUSE: Duration = Duration::from_secs(2);

// openstreetmap counts one building level as 3.2 metres
const METRES_PER_LEVEL: f64 = 3.2;
const DEFAULT_LEVELS: f64 = 3.0;

type TileFailure = (StatusCode, String);

fn unavailable(message: impl Into<String>) -> TileFailure {
    (StatusCode::SERVICE_UNAVAILABLE, message.into())
}

fn not_found(message: impl Into<String>) -> TileFailure {
    (StatusCode::NOT_FOUND, message.into())
}

pub struct OsmBuildingsSettings {
    pub overpass_url: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_bytes: u64,
    pub ion_asset_id: Option<i64>,
}

impl OsmBuildingsSettings {
    pub fn from_env() -> Result<Self, String> {
        Ok(OsmBuildingsSettings {
            overpass_url: setting_text(OVERPASS_URL_VAR),
            cache_dir: setting_text(CACHE_DIR_VAR).map(PathBuf::from),
            cache_bytes: cache_bytes_setting(CACHE_MEGABYTES_VAR)?,
            ion_asset_id: ion_asset_id_setting(ION_ASSET_ID_VAR)?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuildingGround {
    GlobalTerrain,
    Ellipsoid,
}

impl BuildingGround {
    fn cache_directory_name(self) -> &'static str {
        match self {
            BuildingGround::GlobalTerrain => "global-terrain",
            BuildingGround::Ellipsoid => "ellipsoid",
        }
    }

    fn description(self) -> &'static str {
        match self {
            BuildingGround::GlobalTerrain => {
                "Each building stands on the lowest global terrain height under its corners"
            }
            BuildingGround::Ellipsoid => {
                "Global terrain is not configured, so each building stands on the WGS84 ellipsoid"
            }
        }
    }
}

struct BuildingSource {
    overpass_url: String,
    client: reqwest::Client,
    overpass_slots: Semaphore,
    cache: TileCache,
    ground: BuildingGround,
}

pub struct OsmBuildings {
    source: Result<Arc<BuildingSource>, String>,
    ion_asset_id: Option<i64>,
}

impl OsmBuildings {
    pub fn new(settings: OsmBuildingsSettings, global_terrain: &GlobalTerrain) -> Self {
        let ground = if global_terrain.missing_configuration().is_none() {
            BuildingGround::GlobalTerrain
        } else {
            BuildingGround::Ellipsoid
        };
        let source = match (settings.overpass_url, settings.cache_dir) {
            (Some(overpass_url), Some(cache_dir)) => Ok(Arc::new(BuildingSource {
                overpass_url,
                client: reqwest::Client::builder()
                    .user_agent(concat!("tiletopia/", env!("CARGO_PKG_VERSION")))
                    .timeout(Duration::from_secs(60))
                    .build()
                    .expect("rustls http client"),
                overpass_slots: Semaphore::new(OVERPASS_SLOTS),
                cache: TileCache::new(
                    cache_dir.join(ground.cache_directory_name()),
                    settings.cache_bytes,
                    "glb",
                ),
                ground,
            })),
            (overpass_url, cache_dir) => {
                let missing: Vec<&str> = [
                    (overpass_url.is_none(), OVERPASS_URL_VAR),
                    (cache_dir.is_none(), CACHE_DIR_VAR),
                ]
                .into_iter()
                .filter_map(|(is_missing, name)| is_missing.then_some(name))
                .collect();
                Err(format!(
                    "global buildings are not configured, set {}",
                    missing.join(", ")
                ))
            }
        };
        OsmBuildings {
            source,
            ion_asset_id: settings.ion_asset_id,
        }
    }

    pub fn ion_asset_id(&self) -> Option<i64> {
        self.ion_asset_id
    }

    pub(crate) fn is_named_by_ion_id(&self, ion_id: &str) -> bool {
        self.ion_asset_id
            .is_some_and(|configured| ion_id.parse::<i64>() == Ok(configured))
    }

    pub(crate) fn missing_configuration(&self) -> Option<&str> {
        self.source.as_ref().err().map(String::as_str)
    }

    fn source(&self) -> Result<Arc<BuildingSource>, TileFailure> {
        self.source
            .as_ref()
            .map(Arc::clone)
            .map_err(|missing| unavailable(missing.clone()))
    }
}

pub fn osm_buildings_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(TILESET_PATH, get(tileset))
        .route(
            "/api/v1/buildings/osm/subtrees/{level}/{x}/{y}",
            get(subtree),
        )
        .route(
            "/api/v1/buildings/osm/content/{level}/{x}/{y}",
            get(content),
        )
}

async fn tileset(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, TileFailure> {
    let source = state.osm_buildings.source().inspect_err(warn_unavailable)?;
    Ok((
        cache_headers(CachePolicy::metadata()),
        Json(tileset_json(source.ground)),
    ))
}

fn tileset_json(ground: BuildingGround) -> Value {
    json!({
        "asset": { "version": "1.1" },
        "geometricError": ROOT_GEOMETRIC_ERROR,
        "extras": { "copyright": COPYRIGHT, "ground": ground.description() },
        "root": {
            "boundingVolume": {
                "region": [
                    -std::f64::consts::PI,
                    -std::f64::consts::FRAC_PI_2,
                    std::f64::consts::PI,
                    std::f64::consts::FRAC_PI_2,
                    LOWEST_HEIGHT,
                    HIGHEST_HEIGHT,
                ],
            },
            "geometricError": ROOT_GEOMETRIC_ERROR,
            "refine": "REPLACE",
            "content": { "uri": "content/{level}/{x}/{y}.glb" },
            "implicitTiling": {
                "subdivisionScheme": "QUADTREE",
                "subtreeLevels": SUBTREE_LEVELS,
                "availableLevels": LEAF_LEVEL + 1,
                "subtrees": { "uri": "subtrees/{level}/{x}/{y}.subtree" },
            },
        },
    })
}

async fn subtree(
    State(state): State<Arc<AppState>>,
    UrlPath((level, x, y_segment)): UrlPath<(u32, u32, String)>,
) -> Result<impl IntoResponse, TileFailure> {
    state.osm_buildings.source().inspect_err(warn_unavailable)?;
    let y = tile_row(&y_segment, ".subtree")?;
    let is_subtree_root = level % SUBTREE_LEVELS == 0 && level + SUBTREE_LEVELS - 1 <= LEAF_LEVEL;
    if !is_subtree_root || !is_on_grid(level, x, y) {
        return Err(not_found(format!(
            "{level}/{x}/{y_segment} is not a subtree of the global buildings"
        )));
    }
    let mut headers = cache_headers(CachePolicy::tile());
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Ok((headers, subtree_bytes(level)))
}

fn tile_row(segment: &str, extension: &str) -> Result<u32, TileFailure> {
    segment
        .strip_suffix(extension)
        .and_then(|row| row.parse().ok())
        .ok_or_else(|| not_found(format!("{segment} is not a tile row ending in {extension}")))
}

fn is_on_grid(level: u32, x: u32, y: u32) -> bool {
    let tiles = 1u64 << level;
    u64::from(x) < tiles && u64::from(y) < tiles
}

// 3d tiles 1.1 binary subtree: magic, version, json length and binary length as u64, then both chunks padded to 8 bytes
fn subtree_bytes(root_level: u32) -> Vec<u8> {
    let holds_leaves = root_level + SUBTREE_LEVELS - 1 == LEAF_LEVEL;
    let (json, binary) = if holds_leaves {
        let bitstream = leaf_content_bitstream();
        let json = json!({
            "buffers": [{ "byteLength": bitstream.len() }],
            "bufferViews": [{ "buffer": 0, "byteOffset": 0, "byteLength": bitstream.len() }],
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [{
                "bitstream": 0,
                "availableCount": 1u32 << (2 * (SUBTREE_LEVELS - 1)),
            }],
            "childSubtreeAvailability": { "constant": 0 },
        });
        (json, bitstream)
    } else {
        let json = json!({
            "tileAvailability": { "constant": 1 },
            "contentAvailability": [{ "constant": 0 }],
            "childSubtreeAvailability": { "constant": 1 },
        });
        (json, Vec::new())
    };
    let mut json = serde_json::to_vec(&json).expect("subtree json serializes");
    json.resize(json.len().next_multiple_of(8), b' ');
    let mut binary = binary;
    binary.resize(binary.len().next_multiple_of(8), 0);
    let mut bytes = Vec::with_capacity(24 + json.len() + binary.len());
    bytes.extend_from_slice(b"subt");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&(json.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(binary.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&json);
    bytes.extend_from_slice(&binary);
    bytes
}

// subtree bits run level by level, so the leaf level's bits follow every bit of the levels above it
fn leaf_content_bitstream() -> Vec<u8> {
    let levels_above: u32 = (0..SUBTREE_LEVELS - 1)
        .map(|level| 1u32 << (2 * level))
        .sum();
    let leaves = 1u32 << (2 * (SUBTREE_LEVELS - 1));
    let total = (levels_above + leaves) as usize;
    let mut bits = vec![0u8; total.div_ceil(8)];
    for index in levels_above as usize..total {
        bits[index / 8] |= 1 << (index % 8);
    }
    bits
}

async fn content(
    State(state): State<Arc<AppState>>,
    UrlPath((level, x, y_segment)): UrlPath<(u32, u32, String)>,
) -> Result<impl IntoResponse, TileFailure> {
    let source = state.osm_buildings.source().inspect_err(warn_unavailable)?;
    let y = tile_row(&y_segment, ".glb")?;
    if level != LEAF_LEVEL || !is_on_grid(level, x, y) {
        return Err(not_found(format!(
            "{level}/{x}/{y_segment} is not a level {LEAF_LEVEL} building tile"
        )));
    }
    let tile = TileAddress { level, x, y };
    let bytes = match source.cache.read(tile).await? {
        Some(bytes) => bytes,
        None => build_content(&state, &source, tile)
            .await
            .inspect_err(warn_unavailable)?,
    };
    let mut headers = cache_headers(CachePolicy::tile());
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("model/gltf-binary"),
    );
    Ok((headers, bytes))
}

fn warn_unavailable(failure: &TileFailure) {
    if failure.0 == StatusCode::SERVICE_UNAVAILABLE {
        tracing::warn!("global buildings: {}", failure.1);
    }
}

async fn build_content(
    state: &Arc<AppState>,
    source: &Arc<BuildingSource>,
    tile: TileAddress,
) -> Result<Vec<u8>, TileFailure> {
    let bounds = tile_bounds(tile);
    let body = {
        let _slot = source
            .overpass_slots
            .acquire()
            .await
            .expect("the overpass semaphore is never closed");
        fetch_overpass(&source.client, &source.overpass_url, bounds).await?
    };
    let _build_slot = state.elevation_store.build_slot().await;
    let state = Arc::clone(state);
    let source = Arc::clone(source);
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, TileFailure> {
        let buildings = buildings_in_tile(&body, bounds)?;
        let bases = match source.ground {
            BuildingGround::GlobalTerrain => terrain_bases(&state.global_terrain, &buildings)?,
            BuildingGround::Ellipsoid => vec![0.0; buildings.len()],
        };
        let bytes = tile_glb(&buildings, &bases, bounds);
        source.cache.store(tile, &bytes)?;
        Ok(bytes)
    })
    .await
    .expect("global buildings tile build panicked")
}

// [west, south, east, north] in degrees, with x counted from the west and y from the south
fn tile_bounds(tile: TileAddress) -> [f64; 4] {
    let tiles = f64::from(1u32 << tile.level);
    let width = 360.0 / tiles;
    let height = 180.0 / tiles;
    let west = -180.0 + f64::from(tile.x) * width;
    let south = -90.0 + f64::from(tile.y) * height;
    [west, south, west + width, south + height]
}

async fn fetch_overpass(
    client: &reqwest::Client,
    url: &str,
    [west, south, east, north]: [f64; 4],
) -> Result<Vec<u8>, TileFailure> {
    let query = format!(
        "[out:json][timeout:50];way[\"building\"]({south},{west},{north},{east});out body;>;out skel qt;"
    );
    let form = format!("data={}", utf8_percent_encode(&query, NON_ALPHANUMERIC));
    let send = || async {
        client
            .post(url)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form.clone())
            .send()
            .await
            .map_err(|error| unavailable(format!("{url} could not be read: {error}")))
    };
    let mut response = send().await?;
    if RETRIED_OVERPASS_STATUSES.contains(&response.status()) {
        tokio::time::sleep(OVERPASS_RETRY_PAUSE).await;
        response = send().await?;
    }
    let status = response.status();
    if !status.is_success() {
        return Err(unavailable(format!("{url} answered {status}")));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| unavailable(format!("{url} could not be read: {error}")))?;
    if bytes.len() > MAX_OVERPASS_BYTES {
        return Err(unavailable(format!(
            "{url} answered {} bytes, and the limit is {MAX_OVERPASS_BYTES}",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

struct Building {
    // longitude, latitude corners, counter-clockwise, first corner not repeated
    ring: Vec<[f64; 2]>,
    height: f64,
    colour: String,
    name: String,
    kind: String,
}

fn buildings_in_tile(body: &[u8], bounds: [f64; 4]) -> Result<Vec<Building>, TileFailure> {
    let document: Value = serde_json::from_slice(body)
        .map_err(|error| unavailable(format!("{OVERPASS_URL_VAR} did not answer JSON: {error}")))?;
    let Some(elements) = document.get("elements").and_then(Value::as_array) else {
        return Err(unavailable(format!(
            "{OVERPASS_URL_VAR} did not answer an Overpass element list"
        )));
    };
    let mut nodes = HashMap::<i64, [f64; 2]>::new();
    for element in elements {
        if element.get("type").and_then(Value::as_str) != Some("node") {
            continue;
        }
        let (Some(id), Some(longitude), Some(latitude)) = (
            element.get("id").and_then(Value::as_i64),
            element.get("lon").and_then(Value::as_f64),
            element.get("lat").and_then(Value::as_f64),
        ) else {
            return Err(unavailable(format!(
                "{OVERPASS_URL_VAR} answered a node with no id or position: {element}"
            )));
        };
        nodes.insert(id, [longitude, latitude]);
    }
    let mut buildings = Vec::new();
    for way in elements
        .iter()
        .filter(|element| element.get("type").and_then(Value::as_str) == Some("way"))
    {
        let tags = way.get("tags").and_then(Value::as_object);
        let tag = |key: &str| {
            tags.and_then(|tags| tags.get(key))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let kind = tag("building");
        if kind == "no" {
            continue;
        }
        let Some(node_ids) = way.get("nodes").and_then(Value::as_array) else {
            return Err(unavailable(format!(
                "{OVERPASS_URL_VAR} answered a way with no node list: {way}"
            )));
        };
        let mut ring = Vec::with_capacity(node_ids.len());
        for node_id in node_ids {
            let corner = node_id
                .as_i64()
                .and_then(|id| nodes.get(&id))
                .ok_or_else(|| {
                    unavailable(format!(
                        "{OVERPASS_URL_VAR} answered a way naming node {node_id} without that node"
                    ))
                })?;
            ring.push(*corner);
        }
        // an unclosed way is not an area, and openstreetmap has building ways like that
        if ring.len() < 4 || ring.first() != ring.last() {
            continue;
        }
        ring.pop();
        if !centre_is_inside(&ring, bounds) {
            continue;
        }
        if signed_plan_area(&ring) < 0.0 {
            ring.reverse();
        }
        buildings.push(Building {
            ring,
            height: building_height(&tag("height"), &tag("building:levels")),
            colour: css_colour(&tag("building:colour")),
            name: tag("name"),
            kind,
        });
    }
    Ok(buildings)
}

// overpass answers every way that touches the tile, so a way belongs to the tile holding its corner average
fn centre_is_inside(ring: &[[f64; 2]], [west, south, east, north]: [f64; 4]) -> bool {
    let count = ring.len() as f64;
    let longitude = ring.iter().map(|corner| corner[0]).sum::<f64>() / count;
    let latitude = ring.iter().map(|corner| corner[1]).sum::<f64>() / count;
    (west..east).contains(&longitude) && (south..north).contains(&latitude)
}

fn signed_plan_area(ring: &[[f64; 2]]) -> f64 {
    let plan = plan_coordinates(ring);
    let mut twice_area = 0.0;
    for (index, here) in plan.iter().enumerate() {
        let next = plan[(index + 1) % plan.len()];
        twice_area += here[0] * next[1] - next[0] * here[1];
    }
    twice_area * 0.5
}

fn plan_coordinates(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let [origin_longitude, origin_latitude] = ring[0];
    let east_scale = origin_latitude.to_radians().cos();
    ring.iter()
        .map(|&[longitude, latitude]| {
            [
                (longitude - origin_longitude) * east_scale,
                latitude - origin_latitude,
            ]
        })
        .collect()
}

// openstreetmap height tags also come in feet and with typos, and those buildings still get drawn from their levels
fn building_height(height_tag: &str, levels_tag: &str) -> f64 {
    let positive = |raw: &str| {
        raw.trim()
            .trim_end_matches('m')
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value > 0.0)
    };
    if let Some(metres) = positive(height_tag) {
        return metres;
    }
    positive(levels_tag).unwrap_or(DEFAULT_LEVELS) * METRES_PER_LEVEL
}

// createOsmBuildingsAsync's default style reads cesium#color as a css colour and falls back to white on an empty string
fn css_colour(raw: &str) -> String {
    let Some(hex) = raw.trim().strip_prefix('#') else {
        return String::new();
    };
    let is_hex = hex.chars().all(|digit| digit.is_ascii_hexdigit());
    match hex.len() {
        3 | 6 if is_hex => format!("#{}", hex.to_ascii_lowercase()),
        _ => String::new(),
    }
}

fn terrain_bases(
    global_terrain: &GlobalTerrain,
    buildings: &[Building],
) -> Result<Vec<f64>, TileFailure> {
    if buildings.is_empty() {
        return Ok(Vec::new());
    }
    let corners: Vec<[f64; 2]> = buildings
        .iter()
        .flat_map(|building| building.ring.iter().copied())
        .collect();
    let heights = global_terrain.ellipsoid_heights(&corners)?;
    let mut remaining = heights.as_slice();
    Ok(buildings
        .iter()
        .map(|building| {
            let (own, rest) = remaining.split_at(building.ring.len());
            remaining = rest;
            own.iter().copied().fold(f64::INFINITY, f64::min)
        })
        .collect())
}

fn tile_glb(buildings: &[Building], bases: &[f64], bounds: [f64; 4]) -> Vec<u8> {
    if buildings.is_empty() {
        return empty_glb();
    }
    let mut glb = Vec::new();
    write_glb(&building_mesh(buildings, bases, bounds), &mut glb)
        .expect("a glb writes into memory");
    glb
}

// cesium fails a tile whose content does not parse, and a gltf scene lists at least one node
fn empty_glb() -> Vec<u8> {
    let mut json =
        br#"{"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{}]}"#.to_vec();
    json.resize(json.len().next_multiple_of(4), b' ');
    let total = 12 + 8 + json.len();
    let mut glb = Vec::with_capacity(total);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total as u32).to_le_bytes());
    glb.extend_from_slice(&(json.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json);
    glb
}

// cesium turns a gltf (x, y, z) into (x, -z, y), so z-up (x, y, z) is written as (x, z, -y)
fn z_up_to_y_up([x, y, z]: [f64; 3]) -> [f64; 3] {
    [x, z, -y]
}

#[derive(Default)]
struct MeshArrays {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    feature_ids: Vec<u32>,
}

impl MeshArrays {
    fn push_triangle(&mut self, corners: [[f64; 3]; 3], feature_id: u32) {
        let Some(normal) = face_normal(corners) else {
            return;
        };
        for corner in corners {
            self.positions
                .push(z_up_to_y_up(corner).map(|value| value as f32));
            self.normals
                .push(z_up_to_y_up(normal).map(|value| value as f32));
            self.feature_ids.push(feature_id);
        }
    }
}

fn building_mesh(
    buildings: &[Building],
    bases: &[f64],
    [west, south, east, north]: [f64; 4],
) -> GlbMesh {
    let centre = geodetic_to_ecef(
        ((south + north) * 0.5).to_radians(),
        ((west + east) * 0.5).to_radians(),
        0.0,
    );
    let local = |[longitude, latitude]: [f64; 2], height: f64| {
        let point = geodetic_to_ecef(latitude.to_radians(), longitude.to_radians(), height);
        [
            point[0] - centre[0],
            point[1] - centre[1],
            point[2] - centre[2],
        ]
    };
    let mut arrays = MeshArrays::default();
    for (index, (building, &base)) in buildings.iter().zip(bases).enumerate() {
        let feature_id = index as u32;
        let ground: Vec<[f64; 3]> = building
            .ring
            .iter()
            .map(|&corner| local(corner, base))
            .collect();
        let roof: Vec<[f64; 3]> = building
            .ring
            .iter()
            .map(|&corner| local(corner, base + building.height))
            .collect();
        let plan = plan_coordinates(&building.ring);
        let flat: Vec<f64> = plan.iter().flatten().copied().collect();
        let triangles = earcutr::earcut(&flat, &[], 2).expect("earcut takes a finite flat ring");
        for &[a, b, c] in triangles.as_chunks::<3>().0 {
            let turn = (plan[b][0] - plan[a][0]) * (plan[c][1] - plan[a][1])
                - (plan[b][1] - plan[a][1]) * (plan[c][0] - plan[a][0]);
            let (b, c) = if turn < 0.0 { (c, b) } else { (b, c) };
            arrays.push_triangle([roof[a], roof[b], roof[c]], feature_id);
        }
        for here in 0..ground.len() {
            let next = (here + 1) % ground.len();
            arrays.push_triangle([ground[here], ground[next], roof[next]], feature_id);
            arrays.push_triangle([ground[here], roof[next], roof[here]], feature_id);
        }
    }
    let text = |field: fn(&Building) -> &String| -> Vec<String> {
        buildings
            .iter()
            .map(|building| field(building).clone())
            .collect()
    };
    GlbMesh {
        positions: arrays.positions,
        normals: Some(arrays.normals),
        indices: None,
        colors: None,
        texcoords: None,
        metadata: Some(TileMetadata {
            class_name: "building".to_string(),
            properties: vec![
                MetadataProperty {
                    name: "cesium#color".to_string(),
                    values: MetadataValues::String(text(|building| &building.colour)),
                },
                MetadataProperty {
                    name: "name".to_string(),
                    values: MetadataValues::String(text(|building| &building.name)),
                },
                MetadataProperty {
                    name: "building".to_string(),
                    values: MetadataValues::String(text(|building| &building.kind)),
                },
                MetadataProperty {
                    name: "height".to_string(),
                    values: MetadataValues::Float32(
                        buildings
                            .iter()
                            .map(|building| building.height as f32)
                            .collect(),
                    ),
                },
            ],
        }),
        feature_ids: Some(arrays.feature_ids),
        texture: None,
        base_color_factor: None,
        node_translation: Some(z_up_to_y_up(centre)),
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    }
}

fn face_normal([first, second, third]: [[f64; 3]; 3]) -> Option<[f64; 3]> {
    let edge_a = [
        second[0] - first[0],
        second[1] - first[1],
        second[2] - first[2],
    ];
    let edge_b = [
        third[0] - first[0],
        third[1] - first[1],
        third[2] - first[2],
    ];
    let cross = [
        edge_a[1] * edge_b[2] - edge_a[2] * edge_b[1],
        edge_a[2] * edge_b[0] - edge_a[0] * edge_b[2],
        edge_a[0] * edge_b[1] - edge_a[1] * edge_b[0],
    ];
    let length = (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt();
    (length > 1e-9).then(|| cross.map(|value| value / length))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overpass_answer(ways: &[(&[[f64; 2]], Value)]) -> Vec<u8> {
        let mut elements = Vec::new();
        let mut next_node_id = 1;
        for (index, (corners, tags)) in ways.iter().enumerate() {
            let node_ids: Vec<i64> = corners
                .iter()
                .map(|&[longitude, latitude]| {
                    elements.push(json!({ "type": "node", "id": next_node_id, "lat": latitude, "lon": longitude }));
                    next_node_id += 1;
                    next_node_id - 1
                })
                .collect();
            elements.push(json!({ "type": "way", "id": index, "nodes": node_ids, "tags": tags }));
        }
        serde_json::to_vec(&json!({ "elements": elements })).unwrap()
    }

    const TILE: [f64; 4] = [7.0, 46.0, 7.01, 46.005];

    fn closed(corners: &[[f64; 2]]) -> Vec<[f64; 2]> {
        let mut ring = corners.to_vec();
        ring.push(corners[0]);
        ring
    }

    #[test]
    fn the_leaf_subtree_marks_content_on_its_last_level_only() {
        let bytes = subtree_bytes(LEAF_LEVEL + 1 - SUBTREE_LEVELS);
        assert_eq!(&bytes[..4], b"subt");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        let json_length = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let binary_length = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
        assert_eq!(json_length % 8, 0);
        assert_eq!(binary_length % 8, 0);
        assert_eq!(bytes.len(), 24 + json_length + binary_length);
        let json: Value = serde_json::from_slice(&bytes[24..24 + json_length]).unwrap();
        assert_eq!(json["contentAvailability"][0]["availableCount"], 64);
        assert_eq!(json["childSubtreeAvailability"]["constant"], 0);

        let bits = &bytes[24 + json_length..];
        let bit = |index: usize| bits[index / 8] >> (index % 8) & 1;
        // levels of 1, 4 and 16 tiles come before the 64 leaves
        assert!((0..21).all(|index| bit(index) == 0));
        assert!((21..85).all(|index| bit(index) == 1));
    }

    #[test]
    fn subtrees_above_the_leaves_have_no_content_and_every_child_subtree() {
        let bytes = subtree_bytes(0);
        let json_length = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
        let json: Value = serde_json::from_slice(&bytes[24..24 + json_length]).unwrap();
        assert_eq!(json["contentAvailability"][0]["constant"], 0);
        assert_eq!(json["childSubtreeAvailability"]["constant"], 1);
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 0);
    }

    #[test]
    fn a_way_belongs_to_the_tile_holding_its_corner_average() {
        let inside = [
            [7.004, 46.002],
            [7.004, 46.003],
            [7.005, 46.003],
            [7.005, 46.002],
        ];
        let straddling_east = [
            [7.0095, 46.002],
            [7.0095, 46.003],
            [7.0125, 46.003],
            [7.0125, 46.002],
        ];
        let unclosed = [[7.002, 46.002], [7.002, 46.003], [7.003, 46.003]];
        let not_a_building = [
            [7.006, 46.002],
            [7.006, 46.003],
            [7.007, 46.003],
            [7.007, 46.002],
        ];
        let body = overpass_answer(&[
            (&closed(&inside), json!({ "building": "yes" })),
            (&closed(&straddling_east), json!({ "building": "yes" })),
            (&unclosed, json!({ "building": "yes" })),
            (&closed(&not_a_building), json!({ "building": "no" })),
        ]);
        let buildings = buildings_in_tile(&body, TILE).unwrap();
        assert_eq!(buildings.len(), 1);
        assert_eq!(buildings[0].ring.len(), 4);
        assert!(signed_plan_area(&buildings[0].ring) > 0.0);
    }

    #[test]
    fn a_way_naming_a_node_the_answer_lacks_is_refused() {
        let body = serde_json::to_vec(&json!({ "elements": [
            { "type": "way", "id": 1, "nodes": [1, 2, 3, 1], "tags": { "building": "yes" } },
        ]}))
        .unwrap();
        let refusal = buildings_in_tile(&body, TILE).err().unwrap();
        assert_eq!(refusal.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn height_comes_from_the_height_tag_then_the_levels() {
        assert_eq!(building_height("12 m", "9"), 12.0);
        assert_eq!(building_height("40'", "4"), 4.0 * METRES_PER_LEVEL);
        assert_eq!(building_height("", ""), DEFAULT_LEVELS * METRES_PER_LEVEL);
        assert_eq!(
            building_height("-3", "zero"),
            DEFAULT_LEVELS * METRES_PER_LEVEL
        );
    }

    #[test]
    fn only_hex_colours_reach_cesium_color() {
        assert_eq!(css_colour("#ABC"), "#abc");
        assert_eq!(css_colour(" #a0b0c0 "), "#a0b0c0");
        assert_eq!(css_colour("red"), "");
        assert_eq!(css_colour("#12345"), "");
    }

    #[test]
    fn walls_face_out_and_roofs_face_up() {
        let corners = [
            [7.004, 46.002],
            [7.004, 46.003],
            [7.005, 46.003],
            [7.005, 46.002],
        ];
        let body = overpass_answer(&[(
            &closed(&corners),
            json!({ "building": "yes", "height": "10" }),
        )]);
        let buildings = buildings_in_tile(&body, TILE).unwrap();
        let mesh = building_mesh(&buildings, &[100.0], TILE);
        let y_up_to_z_up = |[x, y, z]: [f32; 3]| [f64::from(x), -f64::from(z), f64::from(y)];
        let translation = mesh.node_translation.unwrap();
        let centre = [translation[0], -translation[2], translation[1]];
        let building_centre =
            geodetic_to_ecef(46.0025f64.to_radians(), 7.0045f64.to_radians(), 105.0);
        let up = {
            let length = building_centre
                .iter()
                .map(|value| value * value)
                .sum::<f64>()
                .sqrt();
            building_centre.map(|value| value / length)
        };
        let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let normals = mesh.normals.as_ref().unwrap();
        let mut roof_faces = 0;
        for (position, normal) in mesh.positions.iter().zip(normals).step_by(3) {
            let position = y_up_to_z_up(*position);
            let normal = y_up_to_z_up(*normal);
            let outward = [
                position[0] + centre[0] - building_centre[0],
                position[1] + centre[1] - building_centre[1],
                position[2] + centre[2] - building_centre[2],
            ];
            if dot(normal, up) > 0.99 {
                roof_faces += 1;
            } else {
                assert!(dot(normal, outward) > 0.0, "a wall faces into the building");
            }
        }
        assert_eq!(roof_faces, 2);
        assert_eq!(
            mesh.feature_ids.as_ref().unwrap().len(),
            mesh.positions.len()
        );
    }
}
