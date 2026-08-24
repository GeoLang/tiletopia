//! On-demand analysis XYZ tiles over the geoplumb pull engine.
//!
//! `GET /api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders the terrain ops the
//! one-shot `POST /api/v1/analysis/terrain` serves, but tile by tile: a graph of
//! DEM source -> reproject to web mercator -> op, pulled per tile by the engine,
//! which caches chunks and coalesces concurrent pulls of the same chunk.
//!
//! Elevation comes from [`crate::elevation`] by default: a loaded grid, a tile
//! staged under the data directory, then the SRTM cache, the same stores the
//! one-shot endpoints read. Set `TILETOPIA_ANALYSIS_DEM_BBOX` and the engines
//! read Copernicus GLO-30 COGs over STAC instead, streaming the window each
//! tile needs. Colors for those ops come from the one-shot renderer, so the
//! panel preview and the live layer agree.
//!
//! Unlike the one-shot endpoints, a window no DEM covers is nodata rather than
//! a refusal: it renders transparent, which is what a map library wants for
//! ground it has no data for.
//!
//! The `ndvi` op has no one-shot counterpart and no DEM to read: it takes
//! sentinel-2 red and nir over STAC (band math on a median composite of the
//! last month),
//! so it serves only when the bbox variable is set, and fails loud otherwise
//! rather than inventing vegetation.
//!
//! `GET /api/v1/analysis/export/{op}?bbox=west,south,east,north&resolution=<m/px>`
//! pulls the same engines once over a whole bbox and answers a deflate web
//! mercator COG. Unlike the tile route it is auth-gated: one request can cost
//! millions of pixels, so it is not part of the anonymous read surface.
//!
//! Known limit: an engine reads the source as it stood when the engine was
//! built, on the first request for that op. A DEM loaded afterwards is not
//! picked up until the server restarts.

use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::future::BoxFuture;
use geoplumb::caps::{
    CapsPattern, CapsSet, Constraint, Crs, Dtype, RasterPattern, ResRange, SetField,
};
use geoplumb::chunk::{Chunk, RasterChunk};
use geoplumb::element::{Source, Transform};
use geoplumb::elements::{BandMath, Composite, Hillshade, Reproject, Slope, StacSearch, StacSrc};
use geoplumb::resample::resample_to_grid;
use geoplumb::tile::{XyzTile, render_tile};
use geoplumb::window::{Bbox, GridSpec, WindowReq};
use geoplumb::{Engine, Graph, NodeId};
use serde::Deserialize;
use terrano_core::{BandedRaster, CogParams, Raster, write_cog_bands};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::timeout;

use crate::AppState;
use crate::analysis;
use crate::elevation::{ElevationField, ElevationSources};
use crate::terrain_api::Refusal;

/// Zooms past this are refused: the tile maths shifts by `z`, and no viewer
/// asks for more than sub-metre pixels anyway.
const MAX_ZOOM: u8 = 22;

/// Chunk cache budget per engine.
const CHUNK_BUDGET_BYTES: usize = 64 << 20;

/// How many engines to keep. A viewer runs one or two analysis layers at a
/// time, and each engine holds a cache of its own.
const MAX_ENGINES: usize = 8;

/// Ladder anchor when no grid is loaded: one arc-second, the spacing of the
/// SRTM tiles the store falls back to.
const SRTM_RESOLUTION_DEG: f64 = 1.0 / 3600.0;

/// Angles are keyed in tenths of a degree, so a turn is this many steps.
const AZIMUTH_STEPS: i64 = 3600;

/// Anchor bbox, `west,south,east,north` in degrees. Setting it puts the engines
/// on Copernicus GLO-30 over STAC, unset leaves them on the server's own DEM
/// stores. Coverage is not bound to it: tiles past it search lazily.
pub const BBOX_VAR: &str = "TILETOPIA_ANALYSIS_DEM_BBOX";

/// STAC API root, for pointing the search at a mirror of the default.
pub const STAC_API_VAR: &str = "TILETOPIA_ANALYSIS_STAC_API";

const DEFAULT_STAC_API: &str = "https://earth-search.aws.element84.com/v1";

/// Copernicus GLO-30, global 30 m elevation, one COG per degree square.
const STAC_COLLECTION: &str = "cop-dem-glo-30";
const STAC_ASSET: &str = "data";

/// Sentinel-2 L2A surface reflectance, one COG per band.
const S2_COLLECTION: &str = "sentinel-2-l2a";
const S2_ASSETS: [&str; 2] = ["red", "nir"];

/// How far behind now the ndvi composite reaches. Sentinel-2 revisits every
/// five days, so a month holds enough items for the median to shed clouds.
const NDVI_WINDOW_DAYS: i64 = 30;

/// NDVI in digital numbers. Reflectance is (dn - 1000) / 10000 since
/// processing baseline 04.00 (every item the trailing window can see), so
/// the offset cancels in the numerator but not the denominator.
const NDVI_EXPR: &str = "(b1 - b0) / (b1 + b0 - 2000)";

/// Renders allowed in flight, one core each. A cold tile is a few hundred
/// milliseconds of CPU and the route is anonymous, so without a cap one caller
/// pins every core.
fn default_render_slots() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// How long a request waits for a slot before it is refused. A viewer opening a
/// screen of tiles queues briefly rather than losing tiles, since a map library
/// does not retry a 503, while a flood still sheds at the rate the slots allow.
/// Waiters cannot pile up past the connections held open across this wait.
const RENDER_WAIT: Duration = Duration::from_secs(2);

pub fn analysis_tile_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/analysis/xyz/{op}/{z}/{x}/{y}", get(analysis_tile))
        .route("/api/v1/analysis/export/{op}", get(analysis_export))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Hillshade,
    Slope,
    Ndvi,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        match s {
            "hillshade" => Some(Op::Hillshade),
            "slope" => Some(Op::Slope),
            "ndvi" => Some(Op::Ndvi),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Op::Hillshade => "hillshade",
            Op::Slope => "slope",
            Op::Ndvi => "ndvi",
        }
    }
}

#[derive(Deserialize, Default)]
pub struct TileParams {
    azimuth: Option<f64>,
    altitude: Option<f64>,
}

/// Identifies an engine. Angles are held in tenths of a degree so a client
/// sliding a slider cannot spawn an engine per float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EngineKey {
    op: Op,
    azimuth: i64,
    altitude: i64,
}

impl EngineKey {
    /// `None` when an angle is not finite, which the handler answers with 400.
    /// Finite angles are folded into the domain the kernel actually has, a turn
    /// of azimuth and a quarter of altitude, before they are keyed: the engine
    /// map is a cache an anonymous caller would otherwise flush by walking the
    /// number line, and 375 degrees is the same sun as 15.
    fn new(op: Op, params: &TileParams) -> Option<EngineKey> {
        let angle = |v: Option<f64>, default: f64| match v {
            Some(v) if !v.is_finite() => None,
            Some(v) => Some(v),
            None => Some(default),
        };
        let deci = |v: f64| (v * 10.0).round() as i64;
        // same defaults as the one-shot terrain endpoint
        let azimuth = angle(params.azimuth, 315.0)?;
        let altitude = angle(params.altitude, 45.0)?;
        Some(match op {
            Op::Hillshade => EngineKey {
                op,
                // after rounding, so a hair under a full turn folds onto zero
                azimuth: deci(azimuth).rem_euclid(AZIMUTH_STEPS),
                altitude: deci(altitude.clamp(0.0, 90.0)),
            },
            Op::Slope | Op::Ndvi => EngineKey {
                op,
                azimuth: 0,
                altitude: 0,
            },
        })
    }
}

/// Where the analysis engines read elevation.
#[derive(Debug, Clone, PartialEq)]
enum SourceConfig {
    /// The server's own DEM: loaded grids, the staged one-degree files, then
    /// the SRTM cache. Nodata where none of them covers the window.
    Staged,
    /// Copernicus GLO-30 over STAC, searched lazily per pulled window.
    Stac(StacConfig),
    /// The environment names a source that cannot be honoured. Tiles fail with
    /// this instead of quietly serving synthetic terrain under a real-data
    /// layer, and [`startup_check`] reports it before the server serves at all.
    Misconfigured(String),
}

#[derive(Debug, Clone, PartialEq)]
struct StacConfig {
    api: String,
    bbox: [f64; 4],
}

impl StacConfig {
    /// geoplumb anchors the grid on the bbox's most recent item at open, then
    /// searches lazily per pulled window in cached two-degree blocks, so tiles
    /// past the bbox resolve too. A tile needing more than 32 cold block
    /// searches fails, which rules out roughly zoom 5 and below.
    fn dem_search(&self) -> StacSearch {
        StacSearch::new(&self.api, STAC_COLLECTION, STAC_ASSET, self.bbox)
    }

    /// Sentinel-2 red and nir over the same anchor bbox, every item of the
    /// last month reduced to a per-pixel median, so clouds and swath edges
    /// fall out of the stack. The window anchors on `now` at engine build
    /// and holds until the server restarts, like everything a source reads.
    fn ndvi_search(&self, now: chrono::DateTime<chrono::Utc>) -> StacSearch {
        let start = now - chrono::Duration::days(NDVI_WINDOW_DAYS);
        let mut search = StacSearch::new(&self.api, S2_COLLECTION, S2_ASSETS[0], self.bbox);
        search.assets = S2_ASSETS.iter().map(|a| a.to_string()).collect();
        search.datetime = Some(format!("{}/..", start.format("%Y-%m-%dT%H:%M:%SZ")));
        search.composite = Composite::Median;
        search
    }
}

/// Refuse to serve on an analysis DEM configuration that cannot be honoured,
/// the way the auth secret is checked: a typo in the bbox would otherwise only
/// show up as 500s on the tile route.
pub fn startup_check() -> Result<(), String> {
    source_from_env().map(|_| ())
}

fn source_from_env() -> Result<SourceConfig, String> {
    source_config(
        std::env::var(BBOX_VAR).ok().as_deref(),
        std::env::var(STAC_API_VAR).ok().as_deref(),
    )
}

/// The [`startup_check`] rule over its two inputs, so it is testable without
/// touching process-global environment variables. No bbox means no STAC source,
/// which is what keeps the default build and its tests off the network.
fn source_config(bbox: Option<&str>, api: Option<&str>) -> Result<SourceConfig, String> {
    let Some(raw) = bbox.map(str::trim).filter(|b| !b.is_empty()) else {
        return Ok(SourceConfig::Staged);
    };
    let api = api
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or(DEFAULT_STAC_API);
    Ok(SourceConfig::Stac(StacConfig {
        api: api.to_string(),
        bbox: parse_bbox(raw, BBOX_VAR)?,
    }))
}

fn parse_bbox(raw: &str, label: &str) -> Result<[f64; 4], String> {
    let bad =
        |detail: String| format!("{label} {detail}, expected west,south,east,north in degrees");
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 4 {
        return Err(bad(format!("has {} values", parts.len())));
    }
    let mut bbox = [0.0; 4];
    for (slot, part) in bbox.iter_mut().zip(&parts) {
        *slot = part
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| bad(format!("has {part:?} where a number belongs")))?;
    }
    let [west, south, east, north] = bbox;
    if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
        return Err(bad("has a longitude outside -180..180".into()));
    }
    if !(-90.0..=90.0).contains(&south) || !(-90.0..=90.0).contains(&north) {
        return Err(bad("has a latitude outside -90..90".into()));
    }
    if west >= east || south >= north {
        return Err(bad("covers no ground".into()));
    }
    Ok(bbox)
}

/// Engines built on demand, one per op and parameter set. Building one solves
/// the graph, and it then holds the chunk cache that makes the next tile cheap,
/// so engines are kept and shared rather than rebuilt per request.
pub struct AnalysisEngines {
    built: Mutex<Vec<(EngineKey, Arc<Engine>, NodeId)>>,
    source: SourceConfig,
    render_slots: Semaphore,
    render_wait: Duration,
}

impl Default for AnalysisEngines {
    fn default() -> Self {
        AnalysisEngines::with_render_limits(default_render_slots(), RENDER_WAIT)
    }
}

impl AnalysisEngines {
    pub fn new() -> Self {
        AnalysisEngines::default()
    }

    /// Same, with the cap and the wait set explicitly. Zero slots never frees
    /// one, which is how a test reaches the saturated path without waiting out
    /// the real horizon.
    pub fn with_render_limits(slots: usize, wait: Duration) -> Self {
        AnalysisEngines {
            built: Mutex::new(Vec::new()),
            source: source_from_env().unwrap_or_else(SourceConfig::Misconfigured),
            render_slots: Semaphore::new(slots),
            render_wait: wait,
        }
    }

    /// A render slot, waiting up to the horizon for one to free, `None` when it
    /// does not. The permit is held for the pull, so the slot count is the
    /// number of tiles being computed at once.
    async fn render_slot(&self) -> Option<SemaphorePermit<'_>> {
        timeout(self.render_wait, self.render_slots.acquire())
            .await
            .ok()?
            .ok()
    }

    /// A poisoned map is still a usable cache: the entries are `Arc`s a panicked
    /// builder never got to touch, and refusing them would take the route down
    /// until restart.
    fn lock(&self) -> MutexGuard<'_, Vec<(EngineKey, Arc<Engine>, NodeId)>> {
        self.built.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn get_or_build(
        &self,
        key: EngineKey,
        elevation: &ElevationSources,
    ) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
        if let Some(hit) = find(&self.lock(), key) {
            return Ok(hit);
        }
        // off the lock and off the async worker: solving the graph builds two
        // projections and a STAC source searches the api, and holding the map
        // across it would serialize every tile request behind one build
        let elevation = elevation.clone();
        let source = self.source.clone();
        let (engine, node) =
            tokio::task::spawn_blocking(move || build_engine(key, &source, elevation))
                .await
                .expect("engine build task panicked")?;

        let mut built = self.lock();
        // another request may have built this key while this one was solving
        if let Some(hit) = find(&built, key) {
            return Ok(hit);
        }
        if built.len() >= MAX_ENGINES {
            built.remove(0);
        }
        built.push((key, Arc::clone(&engine), node));
        Ok((engine, node))
    }
}

fn find(
    built: &[(EngineKey, Arc<Engine>, NodeId)],
    key: EngineKey,
) -> Option<(Arc<Engine>, NodeId)> {
    built
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, engine, node)| (Arc::clone(engine), *node))
}

/// Terrain ops run source -> reprojection -> op: the kernels read their cell
/// size off the raster they are handed, so they have to run on the metric grid
/// rather than on degrees. A web mercator metre is stretched by 1/cos(latitude),
/// so a slope tile reads shallower than the one-shot endpoint, which samples in
/// ground metres. NDVI is per pixel and indifferent to the grid, so it computes
/// before the reprojection, on one band instead of two.
fn build_engine(
    key: EngineKey,
    source: &SourceConfig,
    elevation: ElevationSources,
) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
    // opening a STAC source searches the api, which is why this whole function
    // runs on a blocking thread
    if let SourceConfig::Misconfigured(detail) = source {
        return Err(geoplumb::Error::Source(detail.clone()));
    }
    let mut graph = Graph::new();
    let out = match key.op {
        Op::Ndvi => {
            let SourceConfig::Stac(cfg) = source else {
                return Err(geoplumb::Error::Source(format!(
                    "ndvi tiles read sentinel-2 over stac, set {BBOX_VAR}"
                )));
            };
            let src = StacSrc::open(&cfg.ndvi_search(chrono::Utc::now()))?;
            let s2 = graph.add_source(Box::new(src));
            let ndvi = graph.add_transform(s2, Box::new(BandMath::new(NDVI_EXPR)?));
            graph.add_transform(ndvi, Box::new(Reproject::new(Crs::WEB_MERCATOR)))
        }
        Op::Hillshade | Op::Slope => {
            let terrain: Box<dyn Source> = match source {
                SourceConfig::Stac(cfg) => Box::new(StacSrc::open(&cfg.dem_search())?),
                _ => Box::new(DemSource::new(elevation)),
            };
            let dem = graph.add_source(terrain);
            let merc = graph.add_transform(dem, Box::new(Reproject::new(Crs::WEB_MERCATOR)));
            let element: Box<dyn Transform> = match key.op {
                Op::Hillshade => Box::new(Hillshade::new(
                    key.azimuth as f64 / 10.0,
                    key.altitude as f64 / 10.0,
                )),
                _ => Box::new(Slope),
            };
            graph.add_transform(merc, element)
        }
    };
    Ok((Arc::new(Engine::new(graph, CHUNK_BUDGET_BYTES)?), out))
}

async fn analysis_tile(
    State(state): State<Arc<AppState>>,
    Path((op, z, x, y)): Path<(String, u8, u32, String)>,
    Query(params): Query<TileParams>,
) -> Result<Response, StatusCode> {
    let op = Op::parse(&op).ok_or(StatusCode::BAD_REQUEST)?;
    let key = EngineKey::new(op, &params).ok_or(StatusCode::BAD_REQUEST)?;
    let y: u32 = y
        .trim_end_matches(".png")
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if z > MAX_ZOOM {
        return Err(StatusCode::BAD_REQUEST);
    }
    let side = 1u32 << z;
    if x >= side || y >= side {
        return Err(StatusCode::NOT_FOUND);
    }

    // everything above is free, so the cap goes here: it is the render this
    // route hands an anonymous caller, not the request, that has to be bounded
    let Some(_slot) = state.analysis_engines.render_slot().await else {
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
        )
            .into_response());
    };

    let (engine, node) = state
        .analysis_engines
        .get_or_build(key, &state.elevation_sources())
        .await
        .map_err(|e| {
            tracing::warn!("analysis tile engine build failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let chunk = render_tile(&engine, node, XyzTile { z, x, y })
        .await
        .map_err(|e| {
            tracing::warn!("analysis tile render failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let png = tile_png(&chunk, op).ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

/// Web mercator's latitude edge: the projection sends the poles to infinity,
/// so an export bbox is clamped to the square domain first.
const MERCATOR_MAX_LAT: f64 = 85.05112878;

const WEB_MERCATOR_EXTENT: f64 = 20037508.342789244;

/// Pixels one export may cover. 4096 squared f64 pixels is 128 MiB per band in
/// flight, roughly what one authenticated request may pin.
const EXPORT_MAX_PIXELS: f64 = 4096.0 * 4096.0;

fn mercator_x(lon: f64) -> f64 {
    lon / 180.0 * WEB_MERCATOR_EXTENT
}

fn mercator_y(lat: f64) -> f64 {
    lat.to_radians().tan().asinh() / std::f64::consts::PI * WEB_MERCATOR_EXTENT
}

/// The export raster's mercator frame: anchored on the bbox's north-west
/// corner, extent rounded up to whole pixels, so the snap can only grow the
/// window east and south rather than shave the requested edges.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ExportGrid {
    west: f64,
    north: f64,
    cols: usize,
    rows: usize,
}

impl ExportGrid {
    fn bbox(&self, resolution: f64) -> Bbox {
        Bbox::new(
            self.west,
            self.north - self.rows as f64 * resolution,
            self.west + self.cols as f64 * resolution,
            self.north,
        )
    }
}

fn export_grid(bbox: [f64; 4], resolution: f64) -> Result<ExportGrid, String> {
    if !(resolution.is_finite() && resolution > 0.0) {
        return Err("resolution must be a positive number of meters per pixel".into());
    }
    let [west, south, east, north] = bbox;
    let south = south.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT);
    let north = north.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT);
    if south >= north {
        return Err(format!(
            "bbox covers no ground inside web mercator's {MERCATOR_MAX_LAT} degree latitude domain"
        ));
    }
    let (west, north) = (mercator_x(west), mercator_y(north));
    let cols = ((mercator_x(east) - west) / resolution).ceil().max(1.0);
    let rows = ((north - mercator_y(south)) / resolution).ceil().max(1.0);
    // compared as floats so an absurd resolution fails here instead of
    // overflowing the casts below
    if cols * rows > EXPORT_MAX_PIXELS {
        return Err(format!(
            "{cols} x {rows} pixels at {resolution} m/px is past the {} pixel export cap, coarsen the resolution or shrink the bbox",
            EXPORT_MAX_PIXELS as u64
        ));
    }
    Ok(ExportGrid {
        west,
        north,
        cols: cols as usize,
        rows: rows as usize,
    })
}

/// Overviews down to roughly one 512 px tile, capped where deeper levels stop
/// buying a viewer anything.
fn overview_levels(cols: usize, rows: usize) -> u32 {
    let max_side = cols.max(rows) as f64;
    (max_side / 512.0).log2().ceil().clamp(0.0, 5.0) as u32
}

#[derive(Deserialize)]
struct ExportParams {
    bbox: String,
    resolution: f64,
    azimuth: Option<f64>,
    altitude: Option<f64>,
}

fn bad_request(reason: String) -> Response {
    (StatusCode::BAD_REQUEST, reason).into_response()
}

async fn analysis_export(
    State(state): State<Arc<AppState>>,
    Path(op): Path<String>,
    Query(params): Query<ExportParams>,
) -> Result<Response, Refusal> {
    let op = Op::parse(&op).ok_or_else(|| {
        bad_request(format!(
            "unknown op {op:?}, expected hillshade, slope or ndvi"
        ))
    })?;
    let angles = TileParams {
        azimuth: params.azimuth,
        altitude: params.altitude,
    };
    let key = EngineKey::new(op, &angles)
        .ok_or_else(|| bad_request("azimuth and altitude must be finite".into()))?;
    let bbox = parse_bbox(&params.bbox, "bbox").map_err(bad_request)?;
    let grid = export_grid(bbox, params.resolution).map_err(bad_request)?;

    // the same slot the tile route takes: an export is one render, just bigger
    let Some(_slot) = state.analysis_engines.render_slot().await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
        )
            .into_response()
            .into());
    };

    let (engine, node) = state
        .analysis_engines
        .get_or_build(key, &state.elevation_sources())
        .await
        .map_err(|e| {
            tracing::warn!("analysis export engine build failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;
    let bbox_m = grid.bbox(params.resolution);
    let pulled = engine
        .pull(
            node,
            WindowReq {
                bbox: bbox_m,
                resolution: params.resolution,
            },
        )
        .await
        .map_err(|e| {
            tracing::warn!("analysis export pull failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;

    // resampling onto the exact grid and deflating the tiles are cpu work,
    // off the async worker like the engine build
    let resolution = params.resolution;
    let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let exact = resample_to_grid(
            pulled.raster().map_err(|e| e.to_string())?,
            &bbox_m,
            grid.cols,
            grid.rows,
        );
        let cog = CogParams {
            tile_width: 512,
            tile_height: 512,
            overview_levels: overview_levels(grid.cols, grid.rows),
            epsg: 3857,
            origin_x: grid.west,
            origin_y: grid.north,
            pixel_width: resolution,
            pixel_height: resolution,
            deflate: true,
            // the bands as the engine pulled them, holes left as NaN
            format: terrano_core::SampleFormat::F64,
            nodata: None,
        };
        let mut out = Cursor::new(Vec::new());
        write_cog_bands(&exact.bands, &cog, &mut out).map_err(|e| e.to_string())?;
        Ok(out.into_inner())
    })
    .await
    .expect("export encode task panicked")
    .map_err(|e| {
        tracing::warn!("analysis export encode failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    Ok((
        [
            (header::CONTENT_TYPE, "image/tiff".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}.tif\"", op.name()),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Paint the op band. The terrain ops use the one-shot endpoint's own
/// renderer, so a tile and the panel's bbox preview of the same terrain look
/// the same. NDVI has no one-shot counterpart, its ramp is defined here.
fn tile_png(chunk: &RasterChunk, op: Op) -> Option<Vec<u8>> {
    let band = chunk.bands.band(0)?;
    let color = match op {
        Op::Hillshade => analysis::hillshade_color,
        Op::Slope => analysis::slope_color,
        Op::Ndvi => ndvi_color,
    };
    Some(analysis::raster_png(band, color))
}

/// NDVI over the usual diverging ramp: -1 reads as bare brown, zero as tan,
/// +1 as deep green. Nodata never reaches this, `raster_png` clears it.
fn ndvi_color(v: f64) -> [u8; 4] {
    let v = v.clamp(-1.0, 1.0);
    let (from, to, t) = if v < 0.0 {
        ([0.42, 0.30, 0.21], [0.84, 0.79, 0.69], v + 1.0)
    } else {
        ([0.84, 0.79, 0.69], [0.0, 0.35, 0.09], v)
    };
    let [r, g, b] = [0, 1, 2].map(|i| {
        let c: f64 = from[i] + (to[i] - from[i]) * t;
        (c * 255.0).round() as u8
    });
    [r, g, b, 255]
}

/// geoplumb source over the server's DEM stores: every pixel is an
/// [`ElevationField`] sample, so loaded grids, staged tiles and the SRTM cache
/// all serve, exactly as they do for the one-shot handlers.
struct DemSource {
    elevation: ElevationSources,
    origin_x: f64,
    origin_y: f64,
    base_resolution: f64,
}

impl DemSource {
    /// The grid anchors the resolution ladder on the finest loaded DEM when
    /// there is one, so its cells land on ladder level 0 instead of between two
    /// levels. With no grid loaded it is a whole-world WGS84 grid at the SRTM
    /// spacing the store falls back to.
    fn new(elevation: ElevationSources) -> DemSource {
        let (origin_x, origin_y, base_resolution) = match elevation.grids().finest_grid() {
            Some(g) => (
                g.bounds[0],
                g.bounds[3],
                g.cell_size_x.min(g.cell_size_y).max(f64::MIN_POSITIVE),
            ),
            None => (-180.0, 90.0, SRTM_RESOLUTION_DEG),
        };
        DemSource {
            elevation,
            origin_x,
            origin_y,
            base_resolution,
        }
    }

    /// Pixels the field does not cover stay NaN, the chunk's nodata, so an
    /// uncovered tile renders transparent instead of flat ground.
    fn sample(&self, req: &WindowReq, field: &ElevationField) -> RasterChunk {
        let res = req.resolution;
        let cols = (req.bbox.width() / res).round().max(1.0) as usize;
        let rows = (req.bbox.height() / res).round().max(1.0) as usize;
        let mut data = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            let lat = req.bbox.max_y - (row as f64 + 0.5) * res;
            for col in 0..cols {
                let lon = req.bbox.min_x + (col as f64 + 0.5) * res;
                data.push(field.elevation_at(lat, lon).unwrap_or(f64::NAN));
            }
        }
        let band = Raster::from_vec(cols, rows, data, res, f64::NAN).expect("sample dims");
        RasterChunk {
            bands: BandedRaster::new(vec![band]).expect("one band"),
            bbox: req.bbox,
            resolution: res,
            crs: Crs::WGS84,
        }
    }
}

impl Source for DemSource {
    fn constraint(&self) -> Constraint {
        Constraint::Produces(CapsSet::one(CapsPattern::Raster(RasterPattern {
            dtype: SetField::one(Dtype::F64),
            bands: SetField::one(1),
            crs: SetField::one(Crs::WGS84),
            resolution: ResRange::at_least(self.base_resolution),
            chunk_px: SetField::Any,
        })))
    }

    fn grid(&self) -> GridSpec {
        GridSpec {
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            base_resolution: self.base_resolution,
            chunk_px: 256,
        }
    }

    fn read<'a>(&'a self, req: &'a WindowReq) -> BoxFuture<'a, geoplumb::Result<Chunk>> {
        Box::pin(async move {
            let bbox = req.bbox;
            let field = self
                .elevation
                .field([bbox.min_x, bbox.min_y, bbox.max_x, bbox.max_y])
                .await
                .map_err(|gap| geoplumb::Error::Source(gap.message().to_string()))?;
            Ok(Chunk::Raster(self.sample(req, &field)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoplumb::window::Bbox;

    use crate::elevation::{DemGrid, DemStore};

    /// Sources with nothing staged and no SRTM fallback, so a test never
    /// reaches the disk or the network.
    fn empty_sources() -> ElevationSources {
        ElevationSources::new(
            Arc::new(DemStore::new()),
            std::env::temp_dir().join("tiletopia_analysis_tiles_no_dem"),
            String::new(),
        )
    }

    /// Sources holding one grid over the window the read tests pull, climbing
    /// east so a hillshade has something to shade.
    fn grid_sources(cell_size_x: f64, cell_size_y: f64) -> ElevationSources {
        let bounds = [6.0, 42.0, 11.0, 44.0];
        let width = 1 + ((bounds[2] - bounds[0]) / cell_size_x).round() as usize;
        let height = 1 + ((bounds[3] - bounds[1]) / cell_size_y).round() as usize;
        let mut store = DemStore::new();
        store.add_grid(DemGrid {
            bounds,
            width,
            height,
            cell_size_x,
            cell_size_y,
            elevations: (0..width * height)
                .map(|i| (i % width) as f64 * 3.0)
                .collect(),
            nodata: -9999.0,
        });
        ElevationSources::new(
            Arc::new(store),
            std::env::temp_dir().join("tiletopia_analysis_tiles_grid"),
            String::new(),
        )
    }

    #[test]
    fn grid_anchors_on_the_finest_loaded_dem() {
        let grid = DemSource::new(grid_sources(0.5, 0.25)).grid();
        assert_eq!(grid.origin_x, 6.0);
        assert_eq!(grid.origin_y, 44.0);
        assert_eq!(grid.base_resolution, 0.25);
    }

    #[test]
    fn grid_falls_back_to_the_whole_world() {
        let grid = DemSource::new(empty_sources()).grid();
        assert_eq!(grid.origin_x, -180.0);
        assert_eq!(grid.origin_y, 90.0);
        assert_eq!(grid.base_resolution, SRTM_RESOLUTION_DEG);
    }

    /// A window is answered on exactly the requested grid: the engine chunks the
    /// request itself and a short read would misalign every chunk downstream.
    #[tokio::test]
    async fn read_fills_the_requested_window() {
        let src = DemSource::new(grid_sources(0.01, 0.01));
        let req = WindowReq {
            bbox: Bbox::new(7.0, 43.0, 7.04, 43.02),
            resolution: 0.001,
        };
        let chunk = src.read(&req).await.unwrap().into_raster().unwrap();
        assert_eq!(chunk.width(), 40);
        assert_eq!(chunk.height(), 20);
        let band = chunk.bands.band(0).unwrap();
        assert!(band.data().iter().all(|v| v.is_finite()));
    }

    /// A tile over ground no DEM covers is transparent, not flat: the pixels
    /// come back as the chunk's nodata rather than an invented elevation.
    #[tokio::test]
    async fn an_uncovered_window_reads_as_nodata() {
        let src = DemSource::new(empty_sources());
        let req = WindowReq {
            bbox: Bbox::new(7.0, 43.0, 7.04, 43.02),
            resolution: 0.001,
        };
        let chunk = src.read(&req).await.unwrap().into_raster().unwrap();
        let band = chunk.bands.band(0).unwrap();
        assert_eq!(band.data().len(), 800);
        assert!(band.data().iter().all(|v| v.is_nan()));
    }

    fn hillshade_key(azimuth: f64, altitude: f64) -> Option<EngineKey> {
        EngineKey::new(
            Op::Hillshade,
            &TileParams {
                azimuth: Some(azimuth),
                altitude: Some(altitude),
            },
        )
    }

    #[test]
    fn hillshade_params_key_separate_engines() {
        let key = |azimuth| hillshade_key(azimuth, 45.0);
        assert_ne!(key(315.0), key(45.0));
        // jitter below a tenth of a degree lands on the same engine
        assert_eq!(key(315.0), key(315.001));
    }

    /// The engine map is a cache, so a caller must not be able to mint a fresh
    /// entry out of an angle that names sun the map already holds.
    #[test]
    fn angles_fold_into_the_domain_before_keying() {
        assert_eq!(hillshade_key(375.0, 45.0), hillshade_key(15.0, 45.0));
        assert_eq!(hillshade_key(-45.0, 45.0), hillshade_key(315.0, 45.0));
        assert_eq!(hillshade_key(360.0, 45.0), hillshade_key(0.0, 45.0));
        assert_eq!(hillshade_key(1e12, 45.0), hillshade_key(1e12 % 360.0, 45.0));
        // the sun cannot go under the horizon or past the zenith
        assert_eq!(hillshade_key(315.0, 200.0), hillshade_key(315.0, 90.0));
        assert_eq!(hillshade_key(315.0, -5.0), hillshade_key(315.0, 0.0));
    }

    #[test]
    fn non_finite_angles_have_no_key() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(hillshade_key(bad, 45.0), None, "azimuth {bad}");
            assert_eq!(hillshade_key(315.0, bad), None, "altitude {bad}");
        }
        // slope ignores the angles, but a malformed request is still malformed
        assert_eq!(
            EngineKey::new(
                Op::Slope,
                &TileParams {
                    azimuth: Some(f64::NAN),
                    altitude: None,
                },
            ),
            None
        );
    }

    /// Unset means the server's own DEM, which is what keeps every other test
    /// in this repo off the network.
    #[test]
    fn no_bbox_leaves_the_engines_on_the_staged_dem() {
        assert_eq!(source_config(None, None), Ok(SourceConfig::Staged));
        assert_eq!(source_config(Some("  "), None), Ok(SourceConfig::Staged));
        // an api on its own configures nothing: the bbox is the switch
        assert_eq!(
            source_config(None, Some("https://example.test/v1")),
            Ok(SourceConfig::Staged)
        );
    }

    #[test]
    fn a_bbox_selects_the_stac_source() {
        let Ok(SourceConfig::Stac(cfg)) = source_config(Some("7.0, 46.3, 8.0,46.9"), None) else {
            panic!("expected a stac source");
        };
        assert_eq!(cfg.bbox, [7.0, 46.3, 8.0, 46.9]);
        assert_eq!(cfg.api, DEFAULT_STAC_API);
        let search = cfg.dem_search();
        assert_eq!(search.collection, STAC_COLLECTION);
        assert_eq!(search.assets, vec![STAC_ASSET.to_string()]);
        assert_eq!(search.bbox, [7.0, 46.3, 8.0, 46.9]);
    }

    /// The ndvi graph reads red and nir cogs as one two-band raster reduced
    /// to a trailing median, so the composite sheds clouds at the source.
    #[test]
    fn the_ndvi_search_names_both_bands_and_a_trailing_median() {
        let Ok(SourceConfig::Stac(cfg)) = source_config(Some("7,46,8,47"), None) else {
            panic!("expected a stac source");
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-04T12:30:00Z")
            .unwrap()
            .to_utc();
        let search = cfg.ndvi_search(now);
        assert_eq!(search.collection, S2_COLLECTION);
        assert_eq!(search.assets, vec!["red".to_string(), "nir".to_string()]);
        assert_eq!(search.datetime.as_deref(), Some("2026-07-05T12:30:00Z/.."));
        assert_eq!(search.composite, Composite::Median);
        assert_eq!(search.bbox, [7.0, 46.0, 8.0, 47.0]);
    }

    /// No synthetic vegetation: without the bbox variable an ndvi engine
    /// refuses to build, naming the variable, instead of serving something.
    #[test]
    fn ndvi_without_a_bbox_fails_naming_the_variable() {
        let key = EngineKey::new(Op::Ndvi, &TileParams::default()).expect("key");
        let built = build_engine(key, &SourceConfig::Staged, empty_sources());
        let Err(err) = built else {
            panic!("ndvi cannot build on the synthetic source");
        };
        assert!(err.to_string().contains(BBOX_VAR), "{err}");
    }

    /// The angles are sun parameters, meaningless to ndvi: they neither key
    /// separate engines nor excuse a malformed request.
    #[test]
    fn ndvi_ignores_the_angles_but_still_rejects_malformed_ones() {
        let key = |azimuth| {
            EngineKey::new(
                Op::Ndvi,
                &TileParams {
                    azimuth: Some(azimuth),
                    altitude: None,
                },
            )
        };
        assert_eq!(key(315.0), key(45.0));
        assert_eq!(key(f64::NAN), None);
    }

    /// The ramp's fixed points: brown at bare, tan at zero, green at dense.
    #[test]
    fn ndvi_colors_diverge_around_zero() {
        let [r, g, b, a] = ndvi_color(-1.0);
        assert!(r > g && g > b && a == 255, "bare ground is brown");
        let [r, g, b, _] = ndvi_color(1.0);
        assert!(g > r && g > b, "dense vegetation is green");
        let mid = ndvi_color(0.0);
        assert_eq!(mid, [214, 201, 176, 255], "zero is tan");
        // past the domain clamps rather than wrapping
        assert_eq!(ndvi_color(-2.0), ndvi_color(-1.0));
        assert_eq!(ndvi_color(2.0), ndvi_color(1.0));
    }

    #[test]
    fn the_api_url_is_overridable() {
        let Ok(SourceConfig::Stac(cfg)) =
            source_config(Some("7,46,8,47"), Some("https://stac.example.test/v1"))
        else {
            panic!("expected a stac source");
        };
        assert_eq!(cfg.api, "https://stac.example.test/v1");
    }

    /// A malformed bbox is refused rather than rounded into something servable:
    /// the whole point of the variable is that the layer is real data.
    #[test]
    fn a_malformed_bbox_is_an_error() {
        for raw in [
            "7,46,8",
            "7,46,8,47,48",
            "7,46,eight,47",
            "7,46,,47",
            "7,46,nan,47",
            "7,46,inf,47",
            // reversed or empty
            "8,46,7,47",
            "7,47,8,46",
            "7,46,7,47",
            // off the globe
            "-181,46,8,47",
            "7,-91,8,47",
        ] {
            let err = source_config(Some(raw), None).unwrap_err();
            assert!(err.starts_with(BBOX_VAR), "{raw}: {err}");
        }
    }

    /// The engines hold the failure so a tile answers 5xx, rather than falling
    /// back to synthetic terrain under a layer the operator asked to be real.
    #[test]
    fn a_broken_config_is_carried_not_swallowed() {
        let engines = AnalysisEngines::with_render_limits(1, RENDER_WAIT);
        let broken = SourceConfig::Misconfigured("bad bbox".into());
        let built = build_engine(
            EngineKey::new(Op::Slope, &TileParams::default()).expect("key"),
            &broken,
            empty_sources(),
        );
        let Err(err) = built else {
            panic!("a misconfigured source cannot build an engine");
        };
        assert!(err.to_string().contains("bad bbox"));
        // the default environment is unset, so the served engines stay synthetic
        assert_eq!(engines.source, SourceConfig::Staged);
    }

    #[test]
    fn export_grid_snaps_outward_anchored_north_west() {
        let grid = export_grid([7.0, 45.0, 7.02, 45.01], 200.0).unwrap();
        assert_eq!(grid.west, mercator_x(7.0));
        assert_eq!(grid.north, mercator_y(45.01));
        assert_eq!((grid.cols, grid.rows), (12, 8));
        let bbox = grid.bbox(200.0);
        // the snap grows the window east and south, never past the anchor
        assert_eq!(bbox.min_x, grid.west);
        assert_eq!(bbox.max_y, grid.north);
        assert!(bbox.max_x >= mercator_x(7.02));
        assert!(bbox.min_y <= mercator_y(45.0));
        assert!(bbox.max_x - mercator_x(7.02) < 200.0);
        assert!(mercator_y(45.0) - bbox.min_y < 200.0);
    }

    #[test]
    fn export_grid_clamps_polar_latitudes() {
        // a bbox reaching the pole clamps to the mercator edge instead of
        // projecting to infinity
        let grid = export_grid([0.0, 84.0, 1.0, 90.0], 1000.0).unwrap();
        assert!(grid.north.is_finite());
        // the clamp latitude is rounded a hair north of the exact edge, so
        // the projected value may overshoot the extent by well under a meter
        assert!(grid.north <= WEB_MERCATOR_EXTENT + 1e-2);
        // entirely past the edge there is nothing left to render
        let err = export_grid([0.0, 86.0, 1.0, 90.0], 1000.0).unwrap_err();
        assert!(err.contains("covers no ground"), "{err}");
    }

    #[test]
    fn export_grid_caps_pixels() {
        let err = export_grid([-180.0, -85.0, 180.0, 85.0], 1.0).unwrap_err();
        assert!(err.contains("export cap"), "{err}");
        // an absurd resolution fails the cap instead of overflowing the dims
        let err = export_grid([7.0, 45.0, 8.0, 46.0], 1e-300).unwrap_err();
        assert!(err.contains("export cap"), "{err}");
    }

    #[test]
    fn export_grid_rejects_a_bad_resolution() {
        for bad in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            let err = export_grid([7.0, 45.0, 8.0, 46.0], bad).unwrap_err();
            assert!(err.contains("resolution"), "{bad}: {err}");
        }
    }

    #[test]
    fn overview_levels_step_down_to_one_tile() {
        assert_eq!(overview_levels(1, 1), 0);
        assert_eq!(overview_levels(512, 512), 0);
        assert_eq!(overview_levels(513, 100), 1);
        assert_eq!(overview_levels(4096, 4096), 3);
        assert_eq!(overview_levels(usize::MAX, 1), 5);
    }

    #[tokio::test]
    async fn a_render_gives_up_when_no_slot_frees() {
        let engines = AnalysisEngines::with_render_limits(1, Duration::from_millis(50));
        let slot = engines.render_slot().await.expect("first slot");
        assert!(engines.render_slot().await.is_none());
        drop(slot);
        assert!(engines.render_slot().await.is_some());
    }

    /// The point of waiting at all: a viewer's second tile takes the slot the
    /// first one hands back instead of being refused while it is still busy.
    #[tokio::test]
    async fn a_render_takes_the_slot_a_finished_one_frees() {
        let engines = AnalysisEngines::with_render_limits(1, Duration::from_secs(2));
        let held = engines.render_slot().await.expect("first slot");
        let render = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
        };
        let waiter = async { engines.render_slot().await.is_some() };
        let (_, took) = tokio::join!(render, waiter);
        assert!(took);
    }
}
