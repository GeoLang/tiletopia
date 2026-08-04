//! On-demand terrain-analysis XYZ tiles over the geoplumb pull engine.
//!
//! `GET /api/v1/analysis/xyz/{op}/{z}/{x}/{y}.png` renders the same ops the
//! one-shot `POST /api/v1/analysis/terrain` serves, but tile by tile: a graph of
//! DEM source -> reproject to web mercator -> op, pulled per tile by the engine,
//! which caches chunks and coalesces concurrent pulls of the same chunk.
//!
//! Elevation comes from `elevation::get_elevation`, so a loaded DEM serves and
//! the deterministic synthetic field fills in elsewhere, the same honesty story
//! the one-shot endpoints have. Colors come from the one-shot renderer, so the
//! panel preview and the live layer agree.
//!
//! Known limit: an engine samples the DEM store as it stood when the engine was
//! built, on the first request for that op. A DEM loaded afterwards is not
//! picked up until the server restarts.

use std::sync::{Arc, Mutex};

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
use geoplumb::elements::{Hillshade, Reproject, Slope};
use geoplumb::tile::{XyzTile, render_tile};
use geoplumb::window::{GridSpec, WindowReq};
use geoplumb::{Engine, Graph, NodeId};
use serde::Deserialize;
use terrano_core::{BandedRaster, Raster};

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

#[derive(Deserialize)]
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
    fn new(op: Op, params: &TileParams) -> EngineKey {
        let deci = |v: f64| (v * 10.0).round() as i64;
        match op {
            // same defaults as the one-shot terrain endpoint
            Op::Hillshade => EngineKey {
                op,
                azimuth: deci(params.azimuth.unwrap_or(315.0)),
                altitude: deci(params.altitude.unwrap_or(45.0)),
            },
            Op::Slope => EngineKey {
                op,
                azimuth: 0,
                altitude: 0,
            },
        }
    }
}

/// Engines built on demand, one per op and parameter set. Building one solves
/// the graph, and it then holds the chunk cache that makes the next tile cheap,
/// so engines are kept and shared rather than rebuilt per request.
#[derive(Default)]
pub struct AnalysisEngines {
    built: Mutex<Vec<(EngineKey, Arc<Engine>, NodeId)>>,
}

impl AnalysisEngines {
    pub fn new() -> Self {
        AnalysisEngines::default()
    }

    fn get_or_build(
        &self,
        key: EngineKey,
        store: &Arc<DemStore>,
    ) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
        let mut built = self.built.lock().expect("engine map lock");
        if let Some((_, engine, node)) = built.iter().find(|(k, _, _)| *k == key) {
            return Ok((Arc::clone(engine), *node));
        }
        let (engine, node) = build_engine(key, Arc::clone(store))?;
        if built.len() >= MAX_ENGINES {
            built.remove(0);
        }
        built.push((key, Arc::clone(&engine), node));
        Ok((engine, node))
    }
}

/// DEM source, then the reprojection, then the op: the terrain kernels read
/// their cell size off the raster they are handed, so they have to run on the
/// metric grid rather than on degrees. A web mercator metre is stretched by
/// 1/cos(latitude), so a slope tile reads shallower than the one-shot endpoint,
/// which samples in ground metres.
fn build_engine(key: EngineKey, store: Arc<DemStore>) -> geoplumb::Result<(Arc<Engine>, NodeId)> {
    let mut graph = Graph::new();
    let dem = graph.add_source(Box::new(DemSource::new(store)));
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

    let (engine, node) = state
        .analysis_engines
        .get_or_build(EngineKey::new(op, &params), &state.elevation_store)
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

    #[test]
    fn hillshade_params_key_separate_engines() {
        let key = |azimuth| {
            EngineKey::new(
                Op::Hillshade,
                &TileParams {
                    azimuth: Some(azimuth),
                    altitude: None,
                },
            )
        };
        assert_ne!(key(315.0), key(45.0));
        // jitter below a tenth of a degree lands on the same engine
        assert_eq!(key(315.0), key(315.001));
    }
}
