use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde_json::json;
use terrano_core::{CogReader, Error as TerranoError, RangeRead};
use tiletopia_terrain::global_dem::{DemSource, TerrainTileCoord, sampled_terrain_tile};

use crate::AppState;
use crate::db::Database;
use crate::http_cache::CachePolicy;
use crate::terrain_api::{encode_quantized_mesh, full_availability, terrain_grid_size};
use crate::terrain_bundle::cache_headers;

pub const DEM_URL_VAR: &str = "TILETOPIA_GLOBAL_TERRAIN_DEM_URL";
pub const GEOID_URL_VAR: &str = "TILETOPIA_GLOBAL_TERRAIN_GEOID_URL";
pub const CACHE_DIR_VAR: &str = "TILETOPIA_GLOBAL_TERRAIN_CACHE_DIR";
pub const CACHE_MEGABYTES_VAR: &str = "TILETOPIA_GLOBAL_TERRAIN_CACHE_MB";
pub const ION_ASSET_ID_VAR: &str = "TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID";

pub const ROUTE_ROOT: &str = "/api/v1/terrain/global/";

// the glo-30 licence asks for this notice wherever adapted or modified data is communicated to the public
pub const COPERNICUS_NOTICE: &str = "produced using Copernicus WorldDEM-30 © DLR e.V. 2010-2014 and © Airbus Defence and Space GmbH 2014-2018 provided under COPERNICUS by the European Union and ESA; all rights reserved";

// a level 7 tile spans 1.40625 degrees, so it touches at most three by three one-degree cells
const FIRST_DEM_LEVEL: u32 = 7;
// level 14 is the first whose 65 samples per tile step finer than glo-30's one arc-second
const DEEPEST_LEVEL: u32 = 14;

// copernicus glo-30 cells measured 1256 header bytes and the proj egm96 grid 1085
const HEADER_BYTES: u64 = 16 * 1024;
// gdal cogs leave 8 bytes between neighbouring tiles of one row
const MERGE_GAP_BYTES: u64 = 4096;
// about 38 full-resolution copernicus tiles of 2.5 MB each
const RANGE_CACHE_BYTES: usize = 96 * 1024 * 1024;
// egm96 at 15 minutes is 1440 by 721 samples
const MAX_GEOID_SAMPLES: usize = 2_000_000;
const GLOBE_TOLERANCE_DEGREES: f64 = 1e-6;
// sampled vertex positions land a few ulps past a tile edge that sits on a whole degree
const CELL_EDGE_TOLERANCE_DEGREES: f64 = 1e-9;

type TileFailure = (StatusCode, String);

fn unavailable(message: String) -> TileFailure {
    (StatusCode::SERVICE_UNAVAILABLE, message)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct GlobalTerrainSettings {
    pub dem_url: Option<String>,
    pub geoid_url: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub cache_bytes: u64,
    pub ion_asset_id: Option<i64>,
}

const DEFAULT_CACHE_MEGABYTES: u64 = 1024;

impl GlobalTerrainSettings {
    pub fn from_env() -> Result<Self, String> {
        let text = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let cache_megabytes = match text(CACHE_MEGABYTES_VAR) {
            None => DEFAULT_CACHE_MEGABYTES,
            Some(raw) => raw
                .parse::<u64>()
                .ok()
                .filter(|&megabytes| megabytes > 0)
                .ok_or_else(|| {
                    format!("{CACHE_MEGABYTES_VAR} is {raw:?}, expected a whole number of megabytes above 0")
                })?,
        };
        let ion_asset_id = text(ION_ASSET_ID_VAR)
            .map(|raw| {
                raw.parse::<i64>()
                    .map_err(|_| format!("{ION_ASSET_ID_VAR} is {raw:?}, expected a whole number"))
            })
            .transpose()?;
        Ok(GlobalTerrainSettings {
            dem_url: text(DEM_URL_VAR),
            geoid_url: text(GEOID_URL_VAR),
            cache_dir: text(CACHE_DIR_VAR).map(PathBuf::from),
            cache_bytes: cache_megabytes * 1024 * 1024,
            ion_asset_id,
        })
    }
}

pub struct GlobalTerrain {
    source: Result<Arc<TerrainSource>, String>,
    ion_asset_id: Option<i64>,
}

impl GlobalTerrain {
    pub fn new(settings: GlobalTerrainSettings) -> Self {
        let source = match (settings.dem_url, settings.geoid_url, settings.cache_dir) {
            (Some(dem_url), Some(geoid_url), Some(cache_dir)) => Ok(Arc::new(TerrainSource {
                dem_url: dem_url.trim_end_matches('/').to_string(),
                geoid_url,
                cache: TileCache {
                    directory: cache_dir,
                    capacity_bytes: settings.cache_bytes,
                    index: Mutex::new(None),
                },
                ranges: Arc::new(Mutex::new(RangeCache::default())),
                absent_cells: Mutex::new(HashSet::new()),
                geoid: Mutex::new(None),
            })),
            (dem_url, geoid_url, cache_dir) => {
                let missing: Vec<&str> = [
                    (dem_url.is_none(), DEM_URL_VAR),
                    (geoid_url.is_none(), GEOID_URL_VAR),
                    (cache_dir.is_none(), CACHE_DIR_VAR),
                ]
                .into_iter()
                .filter_map(|(is_missing, name)| is_missing.then_some(name))
                .collect();
                Err(format!(
                    "global terrain is not configured, set {}",
                    missing.join(", ")
                ))
            }
        };
        GlobalTerrain {
            source,
            ion_asset_id: settings.ion_asset_id,
        }
    }

    pub(crate) fn is_named_by_ion_id(&self, ion_id: &str) -> bool {
        self.ion_asset_id
            .is_some_and(|configured| ion_id.parse::<i64>() == Ok(configured))
    }

    pub async fn reserve_ion_asset_id(&self, db: &Database) -> Result<(), String> {
        if let Some(ion_id) = self.ion_asset_id {
            let holder = db.get_asset_by_ion_id(ion_id).await.map_err(|error| {
                format!("reading the asset with ion id {ion_id} failed: {error}")
            })?;
            if let Some((asset, _)) = holder {
                return Err(format!(
                    "{ION_ASSET_ID_VAR} is {ion_id}, and asset {} ({:?}) already holds that ion id: set the variable to an id no asset holds, or unset it and delete that asset first",
                    asset.id, asset.name
                ));
            }
        }
        db.set_reserved_ion_id(self.ion_asset_id)
            .await
            .map_err(|error| format!("storing {ION_ASSET_ID_VAR} failed: {error}"))
    }

    pub(crate) fn missing_configuration(&self) -> Option<&str> {
        self.source.as_ref().err().map(String::as_str)
    }

    fn source(&self) -> Result<Arc<TerrainSource>, TileFailure> {
        self.source
            .as_ref()
            .map(Arc::clone)
            .map_err(|missing| unavailable(missing.clone()))
    }
}

pub fn global_terrain_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/terrain/global/layer.json", get(global_layer_json))
        .route("/api/v1/terrain/global/{z}/{x}/{y}", get(global_tile))
}

async fn global_layer_json(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, TileFailure> {
    state.global_terrain.source()?;
    let layer = json!({
        "tilejson": "2.1.0",
        "name": "TileTopia Global Terrain",
        "description": "Copernicus DEM GLO-30 as heights above the WGS84 ellipsoid through the configured geoid. Levels below 7 carry the geoid alone",
        "version": "1.0.0",
        "format": "quantized-mesh-1.0",
        "attribution": COPERNICUS_NOTICE,
        "scheme": "tms",
        "projection": "EPSG:4326",
        "tiles": ["{z}/{x}/{y}.terrain?v={version}"],
        "minzoom": 0,
        "maxzoom": DEEPEST_LEVEL,
        "bounds": [-180.0, -90.0, 180.0, 90.0],
        "available": full_availability(DEEPEST_LEVEL),
    });
    Ok((cache_headers(CachePolicy::metadata()), Json(layer)))
}

async fn global_tile(
    State(state): State<Arc<AppState>>,
    UrlPath((z, x, y)): UrlPath<(u32, u32, String)>,
) -> Result<impl IntoResponse, TileFailure> {
    let bytes = serve_tile(&state, z, x, &y)
        .await
        .inspect_err(|(status, message)| {
            if *status == StatusCode::SERVICE_UNAVAILABLE {
                tracing::warn!("global terrain tile {z}/{x}/{y} refused: {message}");
            }
        })?;
    let mut headers = cache_headers(CachePolicy::tile());
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.quantized-mesh"),
    );
    Ok((headers, bytes))
}

async fn serve_tile(
    state: &AppState,
    z: u32,
    x: u32,
    y_segment: &str,
) -> Result<Vec<u8>, TileFailure> {
    let source = state.global_terrain.source()?;
    let coord = tile_coord(z, x, y_segment)?;
    if let Some(bytes) = source.cache.read(&coord).await? {
        return Ok(bytes);
    }
    let _build_slot = state.elevation_store.build_slot().await;
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, TileFailure> {
        let bytes = source.build_tile(&coord)?;
        source.cache.store(&coord, &bytes)?;
        Ok(bytes)
    })
    .await
    .expect("global terrain tile build panicked")
}

fn tile_coord(z: u32, x: u32, y_segment: &str) -> Result<TerrainTileCoord, TileFailure> {
    let not_a_tile = || {
        (
            StatusCode::BAD_REQUEST,
            format!("{z}/{x}/{y_segment} is not a terrain tile"),
        )
    };
    let y: u32 = y_segment
        .strip_suffix(".terrain")
        .unwrap_or(y_segment)
        .parse()
        .map_err(|_| not_a_tile())?;
    if z > DEEPEST_LEVEL {
        return Err((
            StatusCode::NOT_FOUND,
            format!("level {z} is past level {DEEPEST_LEVEL}, the deepest this layer serves"),
        ));
    }
    let (x_tiles, y_tiles) = TerrainTileCoord::grid_at_zoom(z);
    if x >= x_tiles || y >= y_tiles {
        return Err(not_a_tile());
    }
    Ok(TerrainTileCoord { zoom: z, x, y })
}

struct TerrainSource {
    dem_url: String,
    geoid_url: String,
    cache: TileCache,
    ranges: Arc<Mutex<RangeCache>>,
    absent_cells: Mutex<HashSet<(i32, i32)>>,
    geoid: Mutex<Option<Arc<Geoid>>>,
}

impl TerrainSource {
    fn build_tile(&self, coord: &TerrainTileCoord) -> Result<Vec<u8>, TileFailure> {
        let geoid = self.geoid()?;
        let grid = terrain_grid_size(coord.zoom);
        let mesh = if coord.zoom < FIRST_DEM_LEVEL {
            sampled_terrain_tile(coord, grid, |latitude, longitude| {
                geoid.undulation(latitude, longitude)
            })
        } else {
            let cells = self.read_cells(coord, grid)?;
            sampled_terrain_tile(coord, grid, |latitude, longitude| {
                orthometric_height(&cells, latitude, longitude)
                    + geoid.undulation(latitude, longitude)
            })
        };
        Ok(encode_quantized_mesh(&mesh, coord))
    }

    fn geoid(&self) -> Result<Arc<Geoid>, TileFailure> {
        let mut loaded = lock(&self.geoid);
        if let Some(geoid) = loaded.as_ref() {
            return Ok(Arc::clone(geoid));
        }
        let reader = self
            .open_cog(&self.geoid_url)?
            .ok_or_else(|| unavailable(format!("{} answered 404", self.geoid_url)))?;
        let geoid = Arc::new(Geoid::read(&reader, &self.geoid_url)?);
        *loaded = Some(Arc::clone(&geoid));
        Ok(geoid)
    }

    fn read_cells(&self, coord: &TerrainTileCoord, grid: u32) -> Result<Vec<DemCell>, TileFailure> {
        let [west, south, east, north] = coord.bounds();
        let spacing = (north - south) / (grid - 1) as f64;
        let mut cells = Vec::new();
        for latitude in cell_latitude(south)..=cell_latitude(north) {
            for longitude in cell_longitude(west)..=cell_longitude(east) {
                let overlap = [
                    (longitude as f64).max(west),
                    (latitude as f64).max(south),
                    ((longitude + 1) as f64).min(east),
                    ((latitude + 1) as f64).min(north),
                ];
                cells.push(DemCell {
                    latitude,
                    longitude,
                    window: self.read_cell(latitude, longitude, overlap, spacing)?,
                });
            }
        }
        Ok(cells)
    }

    fn read_cell(
        &self,
        latitude: i32,
        longitude: i32,
        overlap: [f64; 4],
        spacing: f64,
    ) -> Result<Option<CellWindow>, TileFailure> {
        if lock(&self.absent_cells).contains(&(latitude, longitude)) {
            return Ok(None);
        }
        let name = DemSource::Copernicus30.tile_name(latitude, longitude);
        let url = format!("{}/{name}/{name}.tif", self.dem_url);
        let Some(reader) = self.open_cog(&url)? else {
            lock(&self.absent_cells).insert((latitude, longitude));
            return Ok(None);
        };
        let level = (0..reader.levels().len())
            .filter(|&level| PointGrid::of_level(&reader, level).step_y <= spacing * (1.0 + 1e-9))
            .max_by(|&a, &b| {
                PointGrid::of_level(&reader, a)
                    .step_y
                    .total_cmp(&PointGrid::of_level(&reader, b).step_y)
            })
            .unwrap_or(0);
        let grid = PointGrid::of_level(&reader, level);
        let [west, south, east, north] = overlap;
        let first_column = grid.column(west).floor() as usize;
        let last_column = grid.column(east).ceil() as usize;
        let first_row = grid.row(north).floor() as usize;
        let last_row = grid.row(south).ceil() as usize;
        let (columns, rows) = (last_column - first_column + 1, last_row - first_row + 1);
        let heights = reader
            .read_window(level, first_column, first_row, columns, rows)
            .map_err(|e| unavailable(format!("{url} could not be read: {e}")))?
            .into_data();
        if heights.iter().any(|height| !height.is_finite()) {
            return Err(unavailable(format!(
                "{url} has no height somewhere in rows {first_row} to {last_row} and columns {first_column} to {last_column} of level {level}"
            )));
        }
        Ok(Some(CellWindow {
            grid,
            first_column,
            first_row,
            columns,
            rows,
            heights,
        }))
    }

    fn open_cog(&self, url: &str) -> Result<Option<CogReader<RemoteCog>>, TileFailure> {
        let unreadable = |e: TerranoError| unavailable(format!("{url} could not be read: {e}"));
        let Some(header) = cached_header(&self.ranges, url).map_err(unreadable)? else {
            return Ok(None);
        };
        let cog = RemoteCog {
            url: url.to_string(),
            header,
            ranges: Arc::clone(&self.ranges),
        };
        CogReader::open(cog).map(Some).map_err(unreadable)
    }
}

fn cell_latitude(latitude: f64) -> i32 {
    // copernicus cells start their rows at the north edge, so a whole-degree latitude is the first row of the cell south of it
    ((latitude - CELL_EDGE_TOLERANCE_DEGREES).ceil() as i32 - 1).clamp(-90, 89)
}

fn cell_longitude(longitude: f64) -> i32 {
    ((longitude + CELL_EDGE_TOLERANCE_DEGREES).floor() as i32).clamp(-180, 179)
}

fn orthometric_height(cells: &[DemCell], latitude: f64, longitude: f64) -> f64 {
    let (wanted_latitude, wanted_longitude) = (cell_latitude(latitude), cell_longitude(longitude));
    let cell = cells
        .iter()
        .find(|cell| cell.latitude == wanted_latitude && cell.longitude == wanted_longitude)
        .expect("every cell under the tile is read before it is sampled");
    // the aws registry entry for copernicus dem says ocean has no cells and a height of zero
    cell.window
        .as_ref()
        .map_or(0.0, |window| window.height(latitude, longitude))
}

struct DemCell {
    latitude: i32,
    longitude: i32,
    window: Option<CellWindow>,
}

struct CellWindow {
    grid: PointGrid,
    first_column: usize,
    first_row: usize,
    columns: usize,
    rows: usize,
    heights: Vec<f64>,
}

impl CellWindow {
    fn height(&self, latitude: f64, longitude: f64) -> f64 {
        let column = self.grid.column(longitude) - self.first_column as f64;
        let row = self.grid.row(latitude) - self.first_row as f64;
        let (left, top) = (column.floor() as usize, row.floor() as usize);
        let right = (left + 1).min(self.columns - 1);
        let bottom = (top + 1).min(self.rows - 1);
        let at = |x: usize, y: usize| self.heights[y * self.columns + x];
        bilinear(
            [
                at(left, top),
                at(right, top),
                at(left, bottom),
                at(right, bottom),
            ],
            column - left as f64,
            row - top as f64,
        )
    }
}

fn bilinear(
    [top_left, top_right, bottom_left, bottom_right]: [f64; 4],
    across: f64,
    down: f64,
) -> f64 {
    top_left * (1.0 - across) * (1.0 - down)
        + top_right * across * (1.0 - down)
        + bottom_left * (1.0 - across) * down
        + bottom_right * across * down
}

struct PointGrid {
    first_x: f64,
    first_y: f64,
    step_x: f64,
    step_y: f64,
    last_column: f64,
    last_row: f64,
}

impl PointGrid {
    fn of_level<R: RangeRead>(reader: &CogReader<R>, level: usize) -> PointGrid {
        let meta = reader.meta();
        let base = &reader.levels()[0];
        let this = &reader.levels()[level];
        let step_x = meta.pixel_width * base.width as f64 / this.width as f64;
        let step_y = meta.pixel_height * base.height as f64 / this.height as f64;
        // terrano moves pixel-is-point tiepoints to the corner of the north-west pixel
        let west_edge = meta.origin_x;
        let north_edge = meta.origin_y;
        PointGrid {
            first_x: west_edge + step_x / 2.0,
            first_y: north_edge - step_y / 2.0,
            step_x,
            step_y,
            last_column: (this.width - 1) as f64,
            last_row: (this.height - 1) as f64,
        }
    }

    fn column(&self, longitude: f64) -> f64 {
        ((longitude - self.first_x) / self.step_x).clamp(0.0, self.last_column)
    }

    fn row(&self, latitude: f64) -> f64 {
        ((self.first_y - latitude) / self.step_y).clamp(0.0, self.last_row)
    }
}

struct Geoid {
    grid: PointGrid,
    width: usize,
    height: usize,
    undulations: Vec<f64>,
}

impl Geoid {
    fn read(reader: &CogReader<RemoteCog>, url: &str) -> Result<Geoid, TileFailure> {
        let level = &reader.levels()[0];
        let (width, height) = (level.width, level.height);
        if width * height > MAX_GEOID_SAMPLES {
            return Err(unavailable(format!(
                "{url} holds {width}x{height} undulations, more than the {MAX_GEOID_SAMPLES} kept in memory, use a coarser grid such as EGM96 at 15 minutes"
            )));
        }
        let grid = PointGrid::of_level(reader, 0);
        let covers_globe = (width as f64 * grid.step_x - 360.0).abs() < GLOBE_TOLERANCE_DEGREES
            && (grid.first_y - 90.0).abs() < GLOBE_TOLERANCE_DEGREES
            && ((height - 1) as f64 * grid.step_y - 180.0).abs() < GLOBE_TOLERANCE_DEGREES;
        if !covers_globe {
            return Err(unavailable(format!(
                "{url} does not cover the globe from pole to pole and all the way round"
            )));
        }
        let undulations = reader
            .read_window(0, 0, 0, width, height)
            .map_err(|e| unavailable(format!("{url} could not be read: {e}")))?
            .into_data();
        if undulations.iter().any(|undulation| !undulation.is_finite()) {
            return Err(unavailable(format!("{url} has samples with no undulation")));
        }
        Ok(Geoid {
            grid,
            width,
            height,
            undulations,
        })
    }

    fn undulation(&self, latitude: f64, longitude: f64) -> f64 {
        let column =
            ((longitude - self.grid.first_x) / self.grid.step_x).rem_euclid(self.width as f64);
        let row = self.grid.row(latitude);
        let (left, top) = (column.floor() as usize % self.width, row.floor() as usize);
        let right = (left + 1) % self.width;
        let bottom = (top + 1).min(self.height - 1);
        let at = |x: usize, y: usize| self.undulations[y * self.width + x];
        bilinear(
            [
                at(left, top),
                at(right, top),
                at(left, bottom),
                at(right, bottom),
            ],
            column - column.floor(),
            row - top as f64,
        )
    }
}

struct RemoteCog {
    url: String,
    header: Arc<Vec<u8>>,
    ranges: Arc<Mutex<RangeCache>>,
}

impl RangeRead for RemoteCog {
    fn read_range(&self, offset: u64, len: u64) -> Result<Vec<u8>, TerranoError> {
        Ok(self.read_ranges(&[(offset, len)])?.remove(0))
    }

    fn read_ranges(&self, ranges: &[(u64, u64)]) -> Result<Vec<Vec<u8>>, TerranoError> {
        let in_header = |&(offset, len): &(u64, u64)| offset + len <= self.header.len() as u64;
        let past_header: Vec<(u64, u64)> = ranges
            .iter()
            .copied()
            .filter(|range| !in_header(range))
            .collect();
        let mut fetched = cached_ranges(&self.ranges, &self.url, &past_header)?.into_iter();
        Ok(ranges
            .iter()
            .map(|range| {
                if in_header(range) {
                    let (offset, len) = (range.0 as usize, range.1 as usize);
                    self.header[offset..offset + len].to_vec()
                } else {
                    fetched
                        .next()
                        .expect("one fetched range for every range past the header")
                }
            })
            .collect())
    }
}

type RangeKey = (String, u64, u64);

struct CachedRange {
    bytes: Arc<Vec<u8>>,
    last_used: u64,
}

#[derive(Default)]
struct RangeCache {
    entries: HashMap<RangeKey, CachedRange>,
    total_bytes: usize,
    tick: u64,
}

impl RangeCache {
    fn get(&mut self, key: &RangeKey) -> Option<Arc<Vec<u8>>> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = tick;
        Some(Arc::clone(&entry.bytes))
    }

    fn insert(&mut self, key: RangeKey, bytes: Arc<Vec<u8>>) {
        self.tick += 1;
        self.total_bytes += bytes.len();
        let entry = CachedRange {
            bytes,
            last_used: self.tick,
        };
        if let Some(replaced) = self.entries.insert(key, entry) {
            self.total_bytes -= replaced.bytes.len();
        }
        while self.total_bytes > RANGE_CACHE_BYTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            let removed = self
                .entries
                .remove(&oldest)
                .expect("the oldest key was just found");
            self.total_bytes -= removed.bytes.len();
        }
    }
}

fn cached_header(
    cache: &Mutex<RangeCache>,
    url: &str,
) -> Result<Option<Arc<Vec<u8>>>, TerranoError> {
    let key = (url.to_string(), 0, HEADER_BYTES);
    if let Some(header) = lock(cache).get(&key) {
        return Ok(Some(header));
    }
    let Some(header) = fetch_range(url, 0, HEADER_BYTES)? else {
        return Ok(None);
    };
    let header = Arc::new(header);
    lock(cache).insert(key, Arc::clone(&header));
    Ok(Some(header))
}

fn cached_ranges(
    cache: &Mutex<RangeCache>,
    url: &str,
    wanted: &[(u64, u64)],
) -> Result<Vec<Vec<u8>>, TerranoError> {
    let mut found: Vec<Option<Arc<Vec<u8>>>> = {
        let mut cached = lock(cache);
        wanted
            .iter()
            .map(|&(offset, len)| cached.get(&(url.to_string(), offset, len)))
            .collect()
    };
    let mut missing: Vec<usize> = (0..wanted.len())
        .filter(|&index| found[index].is_none())
        .collect();
    missing.sort_by_key(|&index| wanted[index].0);

    let end_of = |index: usize| wanted[index].0 + wanted[index].1;
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for index in missing {
        match runs.last_mut() {
            Some(run)
                if wanted[index].0
                    <= run
                        .iter()
                        .map(|&member| end_of(member))
                        .max()
                        .expect("a run holds at least one range")
                        + MERGE_GAP_BYTES =>
            {
                run.push(index)
            }
            _ => runs.push(vec![index]),
        }
    }

    for run in runs {
        let start = wanted[run[0]].0;
        let end = run
            .iter()
            .map(|&member| end_of(member))
            .max()
            .expect("a run holds at least one range");
        let fetched = fetch_range(url, start, end - start)?.ok_or_else(|| {
            TerranoError::Format(format!("{url}: answered 404 to a read past its header"))
        })?;
        if fetched.len() as u64 != end - start {
            return Err(TerranoError::Format(format!(
                "{url}: answered {} bytes for a range of {}",
                fetched.len(),
                end - start
            )));
        }
        let mut cached = lock(cache);
        for index in run {
            let (offset, len) = wanted[index];
            let at = (offset - start) as usize;
            let piece = Arc::new(fetched[at..at + len as usize].to_vec());
            cached.insert((url.to_string(), offset, len), Arc::clone(&piece));
            found[index] = Some(piece);
        }
    }

    Ok(found
        .into_iter()
        .map(|piece| piece.expect("every range is cached or fetched").to_vec())
        .collect())
}

fn fetch_range(url: &str, offset: u64, len: u64) -> Result<Option<Vec<u8>>, TerranoError> {
    let failed = |detail: String| TerranoError::Format(format!("{url}: {detail}"));
    let last = offset + len - 1;
    let response = crate::cog::http_client()
        .get(url)
        .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
        .send()
        .map_err(|e| failed(e.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(failed(format!(
            "answered {} to a range request, reading a COG needs a host that honours Range",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .map_err(|e| failed(e.to_string()))?
        .to_vec();
    // http answers a range running past the end of a file with the bytes up to the end
    if bytes.is_empty() || bytes.len() as u64 > len {
        return Err(failed(format!(
            "answered {} bytes for a range of {len}",
            bytes.len()
        )));
    }
    Ok(Some(bytes))
}

struct TileCache {
    directory: PathBuf,
    capacity_bytes: u64,
    index: Mutex<Option<CacheIndex>>,
}

fn cache_failure(path: &Path, error: std::io::Error) -> TileFailure {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("terrain cache {}: {error}", path.display()),
    )
}

impl TileCache {
    fn tile_path(&self, coord: &TerrainTileCoord) -> PathBuf {
        self.directory
            .join(coord.zoom.to_string())
            .join(coord.x.to_string())
            .join(format!("{}.terrain", coord.y))
    }

    async fn read(&self, coord: &TerrainTileCoord) -> Result<Option<Vec<u8>>, TileFailure> {
        let path = self.tile_path(coord);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(cache_failure(&path, error)),
        }
    }

    fn store(&self, coord: &TerrainTileCoord, bytes: &[u8]) -> Result<(), TileFailure> {
        let path = self.tile_path(coord);
        let mut loaded = lock(&self.index);
        if loaded.is_none() {
            let scanned = CacheIndex::scan(&self.directory)
                .map_err(|error| cache_failure(&self.directory, error))?;
            *loaded = Some(scanned);
        }
        let index = loaded.as_mut().expect("the index was just loaded");
        if path.exists() {
            return Ok(());
        }
        let level_directory = path
            .parent()
            .expect("a tile path sits in a column directory");
        std::fs::create_dir_all(level_directory)
            .map_err(|error| cache_failure(level_directory, error))?;
        let staging = path.with_extension("partial");
        std::fs::write(&staging, bytes).map_err(|error| cache_failure(&staging, error))?;
        std::fs::rename(&staging, &path).map_err(|error| cache_failure(&path, error))?;
        index.oldest_first.push_back((path, bytes.len() as u64));
        index.total_bytes += bytes.len() as u64;
        while index.total_bytes > self.capacity_bytes {
            let Some((oldest, size)) = index.oldest_first.pop_front() else {
                break;
            };
            match std::fs::remove_file(&oldest) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(cache_failure(&oldest, error)),
            }
            index.total_bytes -= size;
        }
        Ok(())
    }
}

struct CacheIndex {
    oldest_first: VecDeque<(PathBuf, u64)>,
    total_bytes: u64,
}

fn collect_files(
    directory: &Path,
    files: &mut Vec<(SystemTime, PathBuf, u64)>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_files(&entry.path(), files)?;
        } else {
            files.push((metadata.modified()?, entry.path(), metadata.len()));
        }
    }
    Ok(())
}

impl CacheIndex {
    fn scan(directory: &Path) -> std::io::Result<CacheIndex> {
        let mut files = Vec::new();
        collect_files(directory, &mut files)?;
        files.sort_by_key(|(modified, _, _)| *modified);
        let total_bytes: u64 = files.iter().map(|(_, _, size)| size).sum();
        Ok(CacheIndex {
            oldest_first: files
                .into_iter()
                .map(|(_, path, size)| (path, size))
                .collect(),
            total_bytes,
        })
    }
}
