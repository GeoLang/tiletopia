//! On-demand terrain-analysis XYZ tiles over the geoplumb pull engine.
//!
//! `GET /api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders the same ops the
//! one-shot `POST /api/v1/analysis/terrain` serves, but tile by tile: a graph of
//! DEM source -> reproject to web mercator -> op, pulled per tile by the engine,
//! which caches chunks and coalesces concurrent pulls of the same chunk.
//!
//! Elevation comes from `elevation::get_elevation` by default, so a loaded DEM
//! serves and the deterministic synthetic field fills in elsewhere, the same
//! honesty story the one-shot endpoints have. Set `TILETOPIA_ANALYSIS_DEM_BBOX`
//! and the engines read Copernicus GLO-30 COGs over STAC instead, streaming the
//! window each tile needs. Colors come from the one-shot renderer either way, so
//! the panel preview and the live layer agree.
//!
//! Known limit: an engine reads the source as it stood when the engine was
//! built, on the first request for that op. A DEM loaded afterwards is not
//! picked up until the server restarts.

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
use geoplumb::elements::{Hillshade, Reproject, Slope, StacSearch, StacSrc};
use geoplumb::tile::{XyzTile, render_tile};
use geoplumb::window::{GridSpec, WindowReq};
use geoplumb::{Engine, Graph, NodeId};
use serde::Deserialize;
use terrano_core::{BandedRaster, Raster};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::timeout;

use crate::AppState;
use crate::analysis;
use crate::elevation::{self, DemStore};

/// Zooms past this are refused: the tile maths shifts by `z`, and no viewer
/// asks for more than sub-metre pixels anyway.
const MAX_ZOOM: u8 = 22;

/// Chunk cache budget per engine.
const CHUNK_BUDGET_BYTES: usize = 64 << 20;

/// How many engines to keep. A viewer runs one or two analysis layers at a
/// time, and each engine holds a cache of its own.
const MAX_ENGINES: usize = 8;

/// Ladder anchor when no DEM is loaded, roughly 11 m at the equator. Only the
/// ladder depends on it: the synthetic field is continuous and samples at any
/// resolution.
const SYNTHETIC_RESOLUTION_DEG: f64 = 1e-4;

/// Angles are keyed in tenths of a degree, so a turn is this many steps.
const AZIMUTH_STEPS: i64 = 3600;

/// Anchor bbox, `west,south,east,north` in degrees. Setting it puts the engines
/// on real elevation, unset leaves them on the DEM store and its synthetic
/// fallback. Coverage is not bound to it: tiles past it search lazily.
pub const BBOX_VAR: &str = "TILETOPIA_ANALYSIS_DEM_BBOX";

/// STAC API root, for pointing the search at a mirror of the default.
pub const STAC_API_VAR: &str = "TILETOPIA_ANALYSIS_STAC_API";

const DEFAULT_STAC_API: &str = "https://earth-search.aws.element84.com/v1";

/// Copernicus GLO-30, global 30 m elevation, one COG per degree square.
const STAC_COLLECTION: &str = "cop-dem-glo-30";
const STAC_ASSET: &str = "data";

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
    Router::new().route("/api/v1/analysis/xyz/{op}/{z}/{x}/{y}", get(analysis_tile))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Hillshade,
    Slope,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        match s {
            "hillshade" => Some(Op::Hillshade),
            "slope" => Some(Op::Slope),
            _ => None,
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
            Op::Slope => EngineKey {
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
    /// The elevation store, synthetic where no loaded grid covers the window.
    Synthetic,
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
    fn search(&self) -> StacSearch {
        StacSearch::new(&self.api, STAC_COLLECTION, STAC_ASSET, self.bbox)
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
        return Ok(SourceConfig::Synthetic);
    };
    let api = api
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or(DEFAULT_STAC_API);
    Ok(SourceConfig::Stac(StacConfig {
        api: api.to_string(),
        bbox: parse_bbox(raw)?,
    }))
}

fn parse_bbox(raw: &str) -> Result<[f64; 4], String> {
    let bad =
        |detail: String| format!("{BBOX_VAR} {detail}, expected west,south,east,north in degrees");
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
        store: &Arc<DemStore>,
    ) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
        if let Some(hit) = find(&self.lock(), key) {
            return Ok(hit);
        }
        // off the lock and off the async worker: solving the graph builds two
        // projections and a STAC source searches the api, and holding the map
        // across it would serialize every tile request behind one build
        let store = Arc::clone(store);
        let source = self.source.clone();
        let (engine, node) = tokio::task::spawn_blocking(move || build_engine(key, &source, store))
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

/// DEM source, then the reprojection, then the op: the terrain kernels read
/// their cell size off the raster they are handed, so they have to run on the
/// metric grid rather than on degrees. A web mercator metre is stretched by
/// 1/cos(latitude), so a slope tile reads shallower than the one-shot endpoint,
/// which samples in ground metres.
fn build_engine(
    key: EngineKey,
    source: &SourceConfig,
    store: Arc<DemStore>,
) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
    // opening a STAC source searches the api, which is why this whole function
    // runs on a blocking thread
    let elevation: Box<dyn Source> = match source {
        SourceConfig::Synthetic => Box::new(DemSource::new(store)),
        SourceConfig::Stac(cfg) => Box::new(StacSrc::open(&cfg.search())?),
        SourceConfig::Misconfigured(detail) => {
            return Err(geoplumb::Error::Source(detail.clone()));
        }
    };
    let mut graph = Graph::new();
    let dem = graph.add_source(elevation);
    let merc = graph.add_transform(dem, Box::new(Reproject::new(Crs::WEB_MERCATOR)));
    let element: Box<dyn Transform> = match key.op {
        Op::Hillshade => Box::new(Hillshade::new(
            key.azimuth as f64 / 10.0,
            key.altitude as f64 / 10.0,
        )),
        Op::Slope => Box::new(Slope),
    };
    let out = graph.add_transform(merc, element);
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
        .get_or_build(key, &state.elevation_store)
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

/// Paint the op band with the one-shot endpoint's own renderer, so a tile and
/// the panel's bbox preview of the same terrain look the same.
fn tile_png(chunk: &RasterChunk, op: Op) -> Option<Vec<u8>> {
    let band = chunk.bands.band(0)?;
    let color = match op {
        Op::Hillshade => analysis::hillshade_color,
        Op::Slope => analysis::slope_color,
    };
    Some(analysis::raster_png(band, color))
}

/// geoplumb source over the elevation store: every pixel is an
/// `elevation::get_elevation` sample, so loaded DEMs and the synthetic fallback
/// both serve, exactly as they do for the one-shot handlers.
struct DemSource {
    store: Arc<DemStore>,
    origin_x: f64,
    origin_y: f64,
    base_resolution: f64,
}

impl DemSource {
    /// The grid anchors the resolution ladder on the finest loaded DEM when
    /// there is one, so its cells land on ladder level 0 instead of between two
    /// levels. With no DEM loaded it is a whole-world WGS84 grid.
    fn new(store: Arc<DemStore>) -> DemSource {
        let (origin_x, origin_y, base_resolution) = match store.finest_grid() {
            Some(g) => (
                g.bounds[0],
                g.bounds[3],
                g.cell_size_x.min(g.cell_size_y).max(f64::MIN_POSITIVE),
            ),
            None => (-180.0, 90.0, SYNTHETIC_RESOLUTION_DEG),
        };
        DemSource {
            store,
            origin_x,
            origin_y,
            base_resolution,
        }
    }

    fn sample(&self, req: &WindowReq) -> RasterChunk {
        let res = req.resolution;
        let cols = (req.bbox.width() / res).round().max(1.0) as usize;
        let rows = (req.bbox.height() / res).round().max(1.0) as usize;
        let mut data = Vec::with_capacity(cols * rows);
        for row in 0..rows {
            let lat = req.bbox.max_y - (row as f64 + 0.5) * res;
            for col in 0..cols {
                let lon = req.bbox.min_x + (col as f64 + 0.5) * res;
                data.push(elevation::get_elevation(lat, lon, &self.store).elevation_m);
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
        Box::pin(async move { Ok(Chunk::Raster(self.sample(req))) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geoplumb::window::Bbox;

    #[test]
    fn grid_anchors_on_the_finest_loaded_dem() {
        let mut store = DemStore::new();
        store.add_grid(crate::elevation::DemGrid {
            bounds: [10.0, 40.0, 11.0, 41.0],
            width: 2,
            height: 2,
            cell_size_x: 0.5,
            cell_size_y: 0.25,
            elevations: vec![1.0; 4],
            nodata: -9999.0,
        });
        let grid = DemSource::new(Arc::new(store)).grid();
        assert_eq!(grid.origin_x, 10.0);
        assert_eq!(grid.origin_y, 41.0);
        assert_eq!(grid.base_resolution, 0.25);
    }

    #[test]
    fn grid_falls_back_to_the_whole_world() {
        let grid = DemSource::new(Arc::new(DemStore::new())).grid();
        assert_eq!(grid.origin_x, -180.0);
        assert_eq!(grid.origin_y, 90.0);
        assert_eq!(grid.base_resolution, SYNTHETIC_RESOLUTION_DEG);
    }

    /// A window is answered on exactly the requested grid: the engine chunks the
    /// request itself and a short read would misalign every chunk downstream.
    #[tokio::test]
    async fn read_fills_the_requested_window() {
        let src = DemSource::new(Arc::new(DemStore::new()));
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

    /// Unset means the synthetic source, which is what keeps every other test
    /// in this repo off the network.
    #[test]
    fn no_bbox_leaves_the_engines_synthetic() {
        assert_eq!(source_config(None, None), Ok(SourceConfig::Synthetic));
        assert_eq!(source_config(Some("  "), None), Ok(SourceConfig::Synthetic));
        // an api on its own configures nothing: the bbox is the switch
        assert_eq!(
            source_config(None, Some("https://example.test/v1")),
            Ok(SourceConfig::Synthetic)
        );
    }

    #[test]
    fn a_bbox_selects_the_stac_source() {
        let Ok(SourceConfig::Stac(cfg)) = source_config(Some("7.0, 46.3, 8.0,46.9"), None) else {
            panic!("expected a stac source");
        };
        assert_eq!(cfg.bbox, [7.0, 46.3, 8.0, 46.9]);
        assert_eq!(cfg.api, DEFAULT_STAC_API);
        let search = cfg.search();
        assert_eq!(search.collection, STAC_COLLECTION);
        assert_eq!(search.asset, STAC_ASSET);
        assert_eq!(search.bbox, [7.0, 46.3, 8.0, 46.9]);
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
            Arc::new(DemStore::new()),
        );
        let Err(err) = built else {
            panic!("a misconfigured source cannot build an engine");
        };
        assert!(err.to_string().contains("bad bbox"));
        // the default environment is unset, so the served engines stay synthetic
        assert_eq!(engines.source, SourceConfig::Synthetic);
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
