use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use geoplumb::chunk::Chunk;
use geoplumb::element::Transform;
use geoplumb::elements::{
    Aspect, Contours, FlowAccumulation, FlowDirection, Hillshade, Reproject, Slope, StreamOrder,
    Viewshed, Watershed,
};
use geoplumb::resample::resample_to_grid;
use geoplumb::window::{Bbox, WindowReq};
use geoplumb::{Crs, Engine, Graph, NodeId};
use image::{Rgba, RgbaImage};
use serde::Deserialize;
use serde_json::{Value, json};
use terrano_core::{Raster, VIEWSHED_VISIBLE, aspect, slope};
use tiletopia_core::crs::{Coord3D, CrsDef, Route, Transformer};
use topoi_core::LineString;
use topoi_core::geojson::FeatureGeometry;

use crate::AppState;
use crate::analysis_tiles::{CHUNK_BUDGET_BYTES, open_dem};
use crate::elevation::{ElevationField, ElevationGap, METERS_PER_DEG_LAT, on_the_globe};
use crate::photogrammetry::placement::{UTM_NORTH_EPSG_BASE, UTM_SOUTH_EPSG_BASE, UTM_ZONES};
use crate::terrain_api::Refusal;

const NODATA: f64 = -9999.0;

/// Grid the handlers sample onto, in cells per side. The cap bounds one
/// anonymous request's work; the floor keeps a 3x3 terrain kernel meaningful.
const RESOLUTION_RANGE: (usize, usize) = (8, 256);
const DEFAULT_RESOLUTION: usize = 160;
const DEFAULT_FLOOD_RESOLUTION: usize = 96;

/// Upstream cell counts span orders of magnitude, so the accumulation ramp is
/// logarithmic and saturates at this many decades of upstream cells.
const ACCUMULATION_DECADES: f64 = 4.0;

/// Hue step between consecutive basin ids. The golden angle keeps neighbouring
/// labels far apart on the wheel.
const BASIN_HUE_STEP: f64 = 137.5;

const UTM_ZONE_WIDTH_DEG: f64 = 6.0;

// utm cells per output cell grow with 1 / cos(latitude) and pass 16 near 86 degrees
const MAX_UTM_CELLS_PER_OUTPUT_CELL: f64 = 16.0;

const STREAM_THRESHOLD_SHARE_OF_REGION: f64 = 0.01;

/// A cell that drains nowhere: a pit, or flat ground.
const UNDRAINED_GREY: [u8; 4] = [128, 128, 128, 255];

pub fn analysis_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/analysis/viewshed", post(viewshed_handler))
        .route("/api/v1/analysis/flood", post(flood))
        .route("/api/v1/analysis/terrain", post(terrain))
        .route("/api/v1/analysis/solar", post(solar))
}

// ── request bodies ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ViewshedReq {
    observer: [f64; 2], // [lon, lat]
    height_m: f64,
    radius_m: f64,
    resolution: Option<usize>,
}

#[derive(Deserialize)]
struct FloodReq {
    level_m: f64,
    bbox: [f64; 4], // [west, south, east, north]
    resolution: Option<usize>,
}

#[derive(Deserialize, Default)]
struct TerrainParams {
    azimuth: Option<f64>,
    altitude: Option<f64>,
    interval: Option<f64>,
    threshold: Option<f64>,
}

#[derive(Deserialize)]
struct TerrainReq {
    op: String,
    bbox: [f64; 4],
    resolution: Option<usize>,
    #[serde(default)]
    params: TerrainParams,
}

#[derive(Deserialize)]
struct SolarReq {
    bbox: [f64; 4],
    date: String, // YYYY-MM-DD
    resolution: Option<usize>,
}

// ── handlers ────────────────────────────────────────────────────────────────

async fn viewshed_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ViewshedReq>,
) -> Result<Response, Refusal> {
    let [lon, lat] = req.observer;
    if !on_the_globe(lon, lat) {
        return Err(bad_request("observer must be [lon, lat] in degrees".into()));
    }
    if !(req.radius_m.is_finite() && req.radius_m > 0.0) || !req.height_m.is_finite() {
        return Err(bad_request(
            "radius_m must be a positive number of metres and height_m a number".into(),
        ));
    }
    let side = resolution(req.resolution, DEFAULT_RESOLUTION);
    let bbox = square_around(lon, lat, req.radius_m);
    let (grid, (observer_x, observer_y)) = {
        let utm = Utm::at_centre_of(bbox)?;
        (utm.grid(bbox, side)?, utm.project(lon, lat)?)
    };
    let viewshed = Viewshed {
        observer_x,
        observer_y,
        observer_height: req.height_m,
        radius: req.radius_m,
    };
    let built = terrain_engine(&state, grid, move |graph, utm_dem| {
        onto_lonlat(graph, utm_dem, Box::new(viewshed), bbox, side)
    })
    .await?;
    covered_utm_dem(&built, &grid, bbox).await?;
    let visible = pulled_onto_lonlat(&built, bbox, side).await?;

    // cell_squares and cell_meters take the box through the outermost cell centres
    let [west, south, east, north] = bbox;
    let half_x = (east - west) / side as f64 / 2.0;
    let half_y = (north - south) / side as f64 / 2.0;
    let centres = [west + half_x, south + half_y, east - half_x, north - half_y];
    let (polygons, cells) = cell_squares(&visible, centres, |v| v == VIEWSHED_VISIBLE);
    let (dx, dy) = cell_meters(centres, side, side);
    let features: Vec<Value> = if polygons.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "type": "Feature",
            "geometry": { "type": "MultiPolygon", "coordinates": polygons },
            "properties": {
                "observer": [lon, lat],
                "height_m": req.height_m,
                "radius_m": req.radius_m,
                "visible_cells": cells,
                "visible_area_m2": cells as f64 * dx * dy
            }
        })]
    };
    Ok(Json(json!({ "type": "FeatureCollection", "features": features })).into_response())
}

async fn flood(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FloodReq>,
) -> Result<Response, Refusal> {
    let bbox = checked_bbox(req.bbox)?;
    let side = resolution(req.resolution, DEFAULT_FLOOD_RESOLUTION);
    let field = state.elevation_sources().field(bbox).await?;
    let dem = dem_over_bbox(&field, bbox, side, side)?;

    let (polygons, cells) = cell_squares(&dem, bbox, |v| v < req.level_m);
    let (dx, dy) = cell_meters(bbox, side, side);
    let features: Vec<Value> = if polygons.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "type": "Feature",
            "geometry": { "type": "MultiPolygon", "coordinates": polygons },
            "properties": {
                "level_m": req.level_m,
                "flooded_cells": cells,
                "flooded_area_m2": cells as f64 * dx * dy
            }
        })]
    };
    Ok(Json(json!({ "type": "FeatureCollection", "features": features })).into_response())
}

struct RasterOp {
    element: Box<dyn Transform>,
    color: Box<dyn Fn(f64) -> [u8; 4] + Send>,
}

fn raster_op(op: &str, params: &TerrainParams, grid: &UtmGrid) -> Result<RasterOp, Refusal> {
    let (crs, region) = (grid.crs, grid.envelope);
    Ok(match op {
        "slope" => RasterOp {
            element: Box::new(Slope),
            color: Box::new(slope_color),
        },
        "aspect" => RasterOp {
            element: Box::new(Aspect),
            color: Box::new(aspect_color),
        },
        "hillshade" => RasterOp {
            element: Box::new(Hillshade::new(
                params.azimuth.unwrap_or(315.0),
                params.altitude.unwrap_or(45.0),
            )),
            color: Box::new(hillshade_color),
        },
        "flow_direction" => RasterOp {
            element: Box::new(FlowDirection { crs, region }),
            color: Box::new(flow_direction_color),
        },
        "flow_accumulation" => {
            let cell_area_m2 = grid.cell_size_m * grid.cell_size_m;
            RasterOp {
                element: Box::new(FlowAccumulation { crs, region }),
                color: Box::new(move |area_m2: f64| accumulation_color(area_m2 / cell_area_m2)),
            }
        }
        "watershed" => RasterOp {
            element: Box::new(Watershed { crs, region }),
            color: Box::new(category_color),
        },
        "stream_order" => {
            let threshold = match params.threshold {
                None => region.width() * region.height() * STREAM_THRESHOLD_SHARE_OF_REGION,
                Some(area_m2) if area_m2.is_finite() && area_m2 > 0.0 => area_m2,
                Some(area_m2) => {
                    return Err(bad_request(format!(
                        "threshold must be a positive area in square metres, got {area_m2}"
                    )));
                }
            };
            RasterOp {
                element: Box::new(StreamOrder {
                    crs,
                    region,
                    threshold,
                }),
                color: Box::new(category_color),
            }
        }
        other => {
            return Err(bad_request(format!(
                "unknown op {other:?}, expected slope, aspect, hillshade, contours, \
                 flow_direction, flow_accumulation, watershed or stream_order"
            )));
        }
    })
}

async fn terrain(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TerrainReq>,
) -> Result<Response, Refusal> {
    let bbox = checked_bbox(req.bbox)?;
    let side = resolution(req.resolution, DEFAULT_RESOLUTION);
    let grid = Utm::at_centre_of(bbox)?.grid(bbox, side)?;
    if req.op == "contours" {
        return contour_lines(&state, grid, bbox, side, req.params.interval).await;
    }
    let RasterOp { element, color } = raster_op(&req.op, &req.params, &grid)?;
    let built = terrain_engine(&state, grid, move |graph, utm_dem| {
        onto_lonlat(graph, utm_dem, element, bbox, side)
    })
    .await?;
    covered_utm_dem(&built, &grid, bbox).await?;
    let band = pulled_onto_lonlat(&built, bbox, side).await?;
    Ok(png_response(raster_png(&band, color)))
}

async fn contour_lines(
    state: &AppState,
    grid: UtmGrid,
    bbox: [f64; 4],
    side: usize,
    interval: Option<f64>,
) -> Result<Response, Refusal> {
    if let Some(interval) = interval
        && !(interval.is_finite() && interval > 0.0)
    {
        return Err(bad_request(format!(
            "interval must be a positive number of metres, got {interval}"
        )));
    }
    let dem_only = terrain_engine(state, grid, |_, utm_dem| utm_dem).await?;
    let dem = covered_utm_dem(&dem_only, &grid, bbox).await?;
    let interval = interval.unwrap_or_else(|| default_interval(&dem));
    let built = terrain_engine(state, grid, move |graph, utm_dem| {
        onto_lonlat(
            graph,
            utm_dem,
            Box::new(Contours {
                interval,
                base: 0.0,
            }),
            bbox,
            side,
        )
    })
    .await?;
    let window = lonlat_window(bbox, side);
    let contours = built
        .engine
        .pull(built.output, window)
        .await
        .and_then(Chunk::into_vector)
        .map_err(analysis_failure)?
        .dissolve()
        .crop_to(&window.bbox);

    let mut features = Vec::new();
    for contour in &contours.features {
        let level = &contour.properties["elevation"];
        let lines: &[LineString] = match &contour.geometry {
            FeatureGeometry::LineString(line) => std::slice::from_ref(line),
            FeatureGeometry::MultiLineString(lines) => lines.linestrings(),
            other => unreachable!("contours are lines, got {other:?}"),
        };
        for line in lines.iter().filter(|line| line.coords().len() >= 2) {
            let coordinates: Vec<[f64; 2]> = line.coords().iter().map(|c| [c.x, c.y]).collect();
            features.push(json!({
                "type": "Feature",
                "geometry": { "type": "LineString", "coordinates": coordinates },
                "properties": { "level": level }
            }));
        }
    }
    Ok(Json(json!({ "type": "FeatureCollection", "features": features })).into_response())
}

async fn solar(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SolarReq>,
) -> Result<Response, Refusal> {
    let bbox = checked_bbox(req.bbox)?;
    let side = resolution(req.resolution, DEFAULT_RESOLUTION);
    let day = day_of_year(&req.date)
        .ok_or_else(|| bad_request(format!("date {:?} is not YYYY-MM-DD", req.date)))?;
    let field = state.elevation_sources().field(bbox).await?;
    let dem = dem_over_bbox(&field, bbox, side, side)?;

    let [_, south, _, north] = bbox;
    let irradiance = solar_irradiance(&dem, (south + north) / 2.0, day);
    Ok(png_response(solar_png(&irradiance)))
}

// ── request checking ────────────────────────────────────────────────────────

fn bad_request(reason: String) -> Refusal {
    (StatusCode::BAD_REQUEST, reason).into_response().into()
}

/// A bbox the handlers can sample: on the globe, and covering ground.
fn checked_bbox(bbox: [f64; 4]) -> Result<[f64; 4], Refusal> {
    let [west, south, east, north] = bbox;
    if !on_the_globe(west, south) || !on_the_globe(east, north) || west >= east || south >= north {
        return Err(bad_request(format!(
            "bbox {bbox:?} must be west,south,east,north in degrees and cover ground"
        )));
    }
    Ok(bbox)
}

fn resolution(requested: Option<usize>, default: usize) -> usize {
    let (min, max) = RESOLUTION_RANGE;
    requested.unwrap_or(default).clamp(min, max)
}

fn no_dem_over(bbox: [f64; 4]) -> ElevationGap {
    let [west, south, east, north] = bbox;
    ElevationGap::NoCoverage(format!(
        "no elevation data staged for this area ({west}, {south}, {east}, {north})"
    ))
}

fn analysis_failure(error: geoplumb::Error) -> Refusal {
    match error {
        too_large @ geoplumb::Error::RegionTooLarge { .. } => bad_request(too_large.to_string()),
        geoplumb::Error::Source(detail) => {
            tracing::warn!("analysis dem read failed: {detail}");
            (StatusCode::SERVICE_UNAVAILABLE, detail)
                .into_response()
                .into()
        }
        other => {
            tracing::warn!("analysis failed: {other}");
            (StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
                .into_response()
                .into()
        }
    }
}

// ── geoplumb graphs ─────────────────────────────────────────────────────────

fn lonlat_window(bbox: [f64; 4], side: usize) -> WindowReq {
    let [west, south, east, north] = bbox;
    WindowReq {
        bbox: Bbox::new(west, south, east, north),
        // geoplumb pixels are square, so the shorter side keeps a pixel per output cell on both axes
        resolution: (east - west).min(north - south) / side as f64,
        time: None,
    }
}

struct Utm {
    crs: Crs,
    projection: Route,
}

#[derive(Clone, Copy)]
struct UtmGrid {
    crs: Crs,
    envelope: Bbox,
    cell_size_m: f64,
}

impl Utm {
    fn at_centre_of(bbox: [f64; 4]) -> Result<Utm, Refusal> {
        let [west, south, east, north] = bbox;
        let (lon, lat) = ((west + east) / 2.0, (south + north) / 2.0);
        let zone = (((lon + 180.0) / UTM_ZONE_WIDTH_DEG).floor() as u32 + 1).min(*UTM_ZONES.end());
        let base = if lat >= 0.0 {
            UTM_NORTH_EPSG_BASE
        } else {
            UTM_SOUTH_EPSG_BASE
        };
        let crs = Crs(base + zone);
        let projection = Transformer::new(CrsDef::Epsg(Crs::WGS84.0), CrsDef::Epsg(crs.0))
            .route()
            .map_err(|e| bad_request(format!("no projection from lon/lat into {crs}: {e}")))?;
        Ok(Utm { crs, projection })
    }

    fn project(&self, lon: f64, lat: f64) -> Result<(f64, f64), Refusal> {
        let projected = self
            .projection
            .apply(Coord3D {
                x: lon,
                y: lat,
                z: 0.0,
            })
            .map_err(|e| {
                bad_request(format!(
                    "({lon}, {lat}) does not project into {}: {e}",
                    self.crs
                ))
            })?;
        Ok((projected.x, projected.y))
    }

    fn grid(&self, bbox: [f64; 4], side: usize) -> Result<UtmGrid, Refusal> {
        let [west, south, east, north] = bbox;
        let mut envelope = Bbox::new(
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for step in 0..=side {
            let along = step as f64 / side as f64;
            let lon = west + along * (east - west);
            let lat = south + along * (north - south);
            for (edge_lon, edge_lat) in [(lon, south), (lon, north), (west, lat), (east, lat)] {
                let (x, y) = self.project(edge_lon, edge_lat)?;
                envelope = Bbox::new(
                    envelope.min_x.min(x),
                    envelope.min_y.min(y),
                    envelope.max_x.max(x),
                    envelope.max_y.max(y),
                );
            }
        }
        let (lon, lat) = ((west + east) / 2.0, (south + north) / 2.0);
        let (centre_x, centre_y) = self.project(lon, lat)?;
        let longest_side_cell_degrees = (east - west).max(north - south) / side as f64;
        let (step_x, step_y) = self.project(lon + longest_side_cell_degrees, lat)?;
        let cell_size_m = (step_x - centre_x).hypot(step_y - centre_y);
        let cells = (envelope.width() / cell_size_m) * (envelope.height() / cell_size_m);
        let limit = MAX_UTM_CELLS_PER_OUTPUT_CELL * (side * side) as f64;
        if !cells.is_finite() || cells > limit {
            return Err(bad_request(format!(
                "bbox {bbox:?} spans {cells:.0} cells of {} at resolution {side}, over the {limit:.0} cell limit, \
                 shrink the box or move it away from the poles",
                self.crs
            )));
        }
        Ok(UtmGrid {
            crs: self.crs,
            envelope,
            cell_size_m,
        })
    }
}

struct TerrainEngine {
    engine: Engine,
    utm_dem: NodeId,
    output: NodeId,
}

async fn terrain_engine(
    state: &AppState,
    grid: UtmGrid,
    add_output: impl FnOnce(&mut Graph, NodeId) -> NodeId + Send + 'static,
) -> Result<TerrainEngine, Refusal> {
    let source = state.analysis_engines.source.clone();
    let elevation = state.elevation_sources();
    tokio::task::spawn_blocking(move || -> geoplumb::Result<TerrainEngine> {
        let mut graph = Graph::new();
        let dem = graph.add_source(open_dem(&source, elevation)?);
        let utm_dem = graph.add_transform(
            dem,
            Box::new(Reproject::onto_grid(
                grid.crs,
                grid.envelope.min_x,
                grid.envelope.max_y,
                grid.cell_size_m,
            )),
        );
        let output = add_output(&mut graph, utm_dem);
        Ok(TerrainEngine {
            engine: Engine::new(graph, CHUNK_BUDGET_BYTES)?,
            utm_dem,
            output,
        })
    })
    .await
    .expect("terrain graph build task panicked")
    .map_err(analysis_failure)
}

fn onto_lonlat(
    graph: &mut Graph,
    utm_dem: NodeId,
    element: Box<dyn Transform>,
    bbox: [f64; 4],
    side: usize,
) -> NodeId {
    let window = lonlat_window(bbox, side);
    let result = graph.add_transform(utm_dem, element);
    graph.add_transform(
        result,
        Box::new(Reproject::onto_grid(
            Crs::WGS84,
            window.bbox.min_x,
            window.bbox.max_y,
            window.resolution,
        )),
    )
}

async fn covered_utm_dem(
    built: &TerrainEngine,
    grid: &UtmGrid,
    bbox: [f64; 4],
) -> Result<Raster, Refusal> {
    let window = WindowReq {
        bbox: grid.envelope,
        resolution: grid.cell_size_m,
        time: None,
    };
    let dem = built
        .engine
        .pull(built.utm_dem, window)
        .await
        .and_then(Chunk::into_raster)
        .map_err(analysis_failure)?;
    let band = dem.bands.into_rasters().swap_remove(0);
    if band.data().iter().all(|v| band.is_nodata(*v)) {
        return Err(no_dem_over(bbox).into());
    }
    Ok(band)
}

async fn pulled_onto_lonlat(
    built: &TerrainEngine,
    bbox: [f64; 4],
    side: usize,
) -> Result<Raster, Refusal> {
    let window = lonlat_window(bbox, side);
    let pulled = built
        .engine
        .pull(built.output, window)
        .await
        .and_then(Chunk::into_raster)
        .map_err(analysis_failure)?;
    let value_kinds = &built.engine.caps(built.output).raster().value_kinds;
    let on_grid = resample_to_grid(&pulled, &window.bbox, side, side, value_kinds);
    Ok(on_grid.bands.into_rasters().swap_remove(0))
}

// ── DEM sampling ────────────────────────────────────────────────────────────

/// meters per cell in x (lon) and y (lat) for a bbox sampled on a width x height grid.
fn cell_meters(bbox: [f64; 4], width: usize, height: usize) -> (f64, f64) {
    let [w, s, e, n] = bbox;
    let mid_lat = (s + n) / 2.0;
    let dx =
        (e - w).abs() * METERS_PER_DEG_LAT * mid_lat.to_radians().cos() / (width.max(2) - 1) as f64;
    let dy = (n - s).abs() * METERS_PER_DEG_LAT / (height.max(2) - 1) as f64;
    (dx, dy)
}

pub(crate) fn dem_over_bbox(
    field: &ElevationField,
    bbox: [f64; 4],
    width: usize,
    height: usize,
) -> Result<Raster, ElevationGap> {
    let [west, south, east, north] = bbox;
    let (dx, dy) = cell_meters(bbox, width, height);
    let mut data = vec![NODATA; width * height];
    let mut covered = 0usize;

    for row in 0..height {
        let lat = north - (row as f64) * (north - south) / (height - 1).max(1) as f64;
        for col in 0..width {
            let lon = west + (col as f64) * (east - west) / (width - 1).max(1) as f64;
            if let Some(elevation) = field.elevation_at(lat, lon) {
                data[row * width + col] = elevation;
                covered += 1;
            }
        }
    }
    if covered == 0 {
        return Err(no_dem_over(bbox));
    }
    Ok(Raster::from_vec(width, height, data, (dx + dy) / 2.0, NODATA).expect("grid dims match"))
}

fn cell_lonlat(bbox: [f64; 4], width: usize, height: usize, row: usize, col: usize) -> (f64, f64) {
    let [w, s, e, n] = bbox;
    let lon = w + (col as f64) * (e - w) / (width - 1).max(1) as f64;
    let lat = n - (row as f64) * (n - s) / (height - 1).max(1) as f64;
    (lon, lat)
}

/// The box a radius reaches from a point. What it covers past the DEM is
/// nodata, the same as any other uncovered cell.
fn square_around(lon: f64, lat: f64, radius_m: f64) -> [f64; 4] {
    let d_lat = radius_m / METERS_PER_DEG_LAT;
    let d_lon = radius_m / (METERS_PER_DEG_LAT * lat.to_radians().cos().abs().max(1e-6));
    [lon - d_lon, lat - d_lat, lon + d_lon, lat + d_lat]
}

/// Return MultiPolygon coordinates (one square per accepted cell) and the cell
/// count. Nodata cells are never accepted: there is no ground there to describe.
fn cell_squares<F: Fn(f64) -> bool>(
    raster: &Raster,
    bbox: [f64; 4],
    accept: F,
) -> (Vec<Value>, u64) {
    let (width, height) = (raster.width(), raster.height());
    let [west, south, east, north] = bbox;
    let half_x = (east - west) / (width - 1).max(1) as f64 / 2.0;
    let half_y = (north - south) / (height - 1).max(1) as f64 / 2.0;
    let mut polygons = Vec::new();
    let mut count = 0u64;

    for row in 0..height {
        for col in 0..width {
            let value = raster.get(row, col).unwrap();
            if raster.is_nodata(value) || !accept(value) {
                continue;
            }
            count += 1;
            let (lon, lat) = cell_lonlat(bbox, width, height, row, col);
            polygons.push(json!([[
                [lon - half_x, lat - half_y],
                [lon + half_x, lat - half_y],
                [lon + half_x, lat + half_y],
                [lon - half_x, lat + half_y],
                [lon - half_x, lat - half_y]
            ]]));
        }
    }
    (polygons, count)
}

// ── contours ────────────────────────────────────────────────────────────────

fn default_interval(dem: &Raster) -> f64 {
    let (mn, mx) = range(dem);
    ((mx - mn) / 10.0).max(1.0)
}

// ── solar (clear-sky insolation model) ──────────────────────────────────────

fn day_of_year(date: &str) -> Option<u32> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(chrono::Datelike::ordinal(&d))
}

/// Clear-sky beam irradiance on each cell at solar noon (W/m2).
///
/// Sun position: declination from day-of-year, altitude = 90 - |lat - decl|,
/// azimuth due south. Surface incidence uses slope + aspect from terrano.
/// terrano's aspect is compass degrees (0=North, clockwise), so due south
/// is 180.
/// This is a clear-sky approximation: no atmospheric attenuation beyond a fixed
/// 1000 W/m2 beam-normal constant, and no diffuse or shadow-casting term.
fn solar_irradiance(dem: &Raster, latitude_deg: f64, day_of_year: u32) -> Raster {
    let slope_r = slope(dem);
    let aspect_r = aspect(dem);
    let decl = 23.45_f64.to_radians()
        * (360.0 / 365.0 * (284.0 + day_of_year as f64))
            .to_radians()
            .sin();
    let alt = (std::f64::consts::FRAC_PI_2 - (latitude_deg.to_radians() - decl).abs()).max(0.0);
    let sun_az = 180.0_f64.to_radians(); // due south at noon, compass degrees
    let beam = 1000.0;
    let mut out = Raster::new(dem.width(), dem.height(), dem.cell_size, dem.nodata);
    for row in 0..dem.height() {
        for col in 0..dem.width() {
            let s = slope_r.get(row, col).unwrap();
            let a = aspect_r.get(row, col).unwrap();
            if slope_r.is_nodata(s) || aspect_r.is_nodata(a) {
                continue;
            }
            let (sr, ar) = (s.to_radians(), a.to_radians());
            let cos_i = alt.sin() * sr.cos() + alt.cos() * sr.sin() * (sun_az - ar).cos();
            out.set(row, col, beam * cos_i.max(0.0));
        }
    }
    out
}

// ── PNG rendering ───────────────────────────────────────────────────────────

fn png_response(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
}

fn range(r: &Raster) -> (f64, f64) {
    let mut mn = f64::INFINITY;
    let mut mx = f64::NEG_INFINITY;
    for &v in r.data() {
        if !r.is_nodata(v) {
            mn = mn.min(v);
            mx = mx.max(v);
        }
    }
    if mn > mx { (0.0, 1.0) } else { (mn, mx) }
}

/// Paint a raster band. Nodata is transparent, and so is NaN: a raster that
/// carries NaN nodata (the analysis tiles do) reads as no data either way.
pub(crate) fn raster_png<C: Fn(f64) -> [u8; 4]>(r: &Raster, color: C) -> Vec<u8> {
    let mut img = RgbaImage::new(r.width() as u32, r.height() as u32);
    for row in 0..r.height() {
        for col in 0..r.width() {
            let v = r.get(row, col).unwrap();
            let px = if r.is_nodata(v) || !v.is_finite() {
                [0, 0, 0, 0]
            } else {
                color(v)
            };
            img.put_pixel(col as u32, row as u32, Rgba(px));
        }
    }
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("png encode");
    buf.into_inner()
}

/// Hillshade illumination, 0..255, as opaque grey.
pub(crate) fn hillshade_color(v: f64) -> [u8; 4] {
    let g = v.clamp(0.0, 255.0) as u8;
    [g, g, g, 255]
}

/// Slope degrees over the ramp, 0..60.
pub(crate) fn slope_color(v: f64) -> [u8; 4] {
    let [r, g, b] = ramp((v / 60.0).clamp(0.0, 1.0));
    [r, g, b, 255]
}

/// D8 codes are powers of two, one per compass direction, so each gets its own
/// hue rather than a place on a ramp.
fn flow_direction_color(v: f64) -> [u8; 4] {
    if v < 1.0 {
        return UNDRAINED_GREY;
    }
    let step = v.log2().round().clamp(0.0, 7.0);
    let [r, g, b] = hsv_to_rgb(step * 45.0, 0.85, 1.0);
    [r, g, b, 255]
}

pub fn accumulation_color(cells: f64) -> [u8; 4] {
    let decades = cells.max(1.0).log10() / ACCUMULATION_DECADES;
    let [r, g, b] = ramp(decades.clamp(0.0, 1.0));
    [r, g, b, 255]
}

pub fn category_color(v: f64) -> [u8; 4] {
    let [r, g, b] = hsv_to_rgb(v * BASIN_HUE_STEP, 0.75, 0.95);
    [r, g, b, 255]
}

fn aspect_color(v: f64) -> [u8; 4] {
    let [r, g, b] = hsv_to_rgb(v, 0.9, 1.0);
    [r, g, b, 255]
}

fn solar_png(irr: &Raster) -> Vec<u8> {
    raster_png(irr, |v| {
        let [r, g, b] = ramp((v / 1000.0).clamp(0.0, 1.0));
        [r, g, b, 255]
    })
}

/// blue -> cyan -> green -> yellow -> red ramp over t in 0..1.
fn ramp(t: f64) -> [u8; 3] {
    let stops = [
        [0.0, 0.0, 0.5],
        [0.0, 0.6, 1.0],
        [0.0, 0.8, 0.3],
        [1.0, 1.0, 0.0],
        [1.0, 0.2, 0.0],
    ];
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
    let i = (t.floor() as usize).min(stops.len() - 2);
    let f = t - i as f64;
    let a = stops[i];
    let b = stops[i + 1];
    [
        ((a[0] + (b[0] - a[0]) * f) * 255.0) as u8,
        ((a[1] + (b[1] - a[1]) * f) * 255.0) as u8,
        ((a[2] + (b[2] - a[2]) * f) * 255.0) as u8,
    ]
}

fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [u8; 3] {
    let c = v * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elevation::{DemGrid, DemStore, ElevationSources};

    const TEST_BBOX: [f64; 4] = [7.0, 43.0, 8.0, 44.0];

    /// Sources with nothing staged and no SRTM fallback, so a test never
    /// reaches the disk or the network.
    fn empty_sources() -> ElevationSources {
        ElevationSources::new(
            Arc::new(DemStore::new()),
            std::env::temp_dir().join("tiletopia_analysis_no_dem"),
            String::new(),
        )
    }

    /// Sources holding one grid climbing to the north-east. It reaches a degree
    /// past [`TEST_BBOX`] on every side, since a grid interpolates only inside
    /// its own cells and the box would otherwise sample its edges as nodata.
    fn tilted_sources() -> ElevationSources {
        let (width, height) = (13usize, 13usize);
        let mut elevations = vec![0.0; width * height];
        for row in 0..height {
            for col in 0..width {
                elevations[row * width + col] = ((height - 1 - row) + col) as f64 * 10.0;
            }
        }
        let mut store = DemStore::new();
        store.add_grid(DemGrid {
            bounds: [6.0, 42.0, 9.0, 45.0],
            width,
            height,
            cell_size_x: 0.25,
            cell_size_y: 0.25,
            elevations,
            nodata: NODATA,
        });
        ElevationSources::new(
            Arc::new(store),
            std::env::temp_dir().join("tiletopia_analysis_grid"),
            String::new(),
        )
    }

    #[tokio::test]
    async fn a_box_with_no_dem_is_a_gap_not_a_blank_raster() {
        let field = empty_sources().field(TEST_BBOX).await.unwrap();
        let gap = dem_over_bbox(&field, TEST_BBOX, 16, 16).unwrap_err();
        assert!(
            gap.message().contains("no elevation data staged"),
            "{gap:?}"
        );
    }

    #[tokio::test]
    async fn a_loaded_grid_fills_the_raster() {
        let field = tilted_sources().field(TEST_BBOX).await.unwrap();
        let dem = dem_over_bbox(&field, TEST_BBOX, 16, 16).unwrap();
        assert_eq!((dem.width(), dem.height()), (16, 16));
        assert!(dem.data().iter().all(|v| !dem.is_nodata(*v)));
        // row 0 is north, and the grid climbs north-east
        assert!(dem.get(0, 15).unwrap() > dem.get(15, 0).unwrap());
        // cell size is metres on the ground, not degrees
        assert!(dem.cell_size > 1000.0, "cell {}", dem.cell_size);
    }

    fn ramp_dem() -> Raster {
        // elevation increases with column: 0,1,2,3,4 across each row.
        let (w, h) = (5usize, 5usize);
        let mut data = vec![0.0; w * h];
        for row in 0..h {
            for col in 0..w {
                data[row * w + col] = col as f64;
            }
        }
        Raster::from_vec(w, h, data, 10.0, NODATA).unwrap()
    }

    #[test]
    fn flood_area_grows_with_level() {
        let dem = ramp_dem();
        let bbox = [0.0, 0.0, 1.0, 1.0];
        let (_, low) = cell_squares(&dem, bbox, |v| v < 1.5);
        let (polygons, high) = cell_squares(&dem, bbox, |v| v < 3.5);
        assert!(high > low, "high {high} should exceed low {low}");
        assert!(low > 0);
        assert_eq!(polygons.len() as u64, high);
    }

    #[test]
    fn nodata_cells_are_never_flooded() {
        let mut dem = ramp_dem();
        dem.set(2, 2, NODATA);
        let (_, cells) = cell_squares(&dem, [0.0, 0.0, 1.0, 1.0], |_| true);
        assert_eq!(cells, 24, "the nodata cell is left out of the 25");
    }

    #[test]
    fn the_radius_sets_the_box_it_looks_over() {
        let [west, south, east, north] = square_around(7.0, 43.0, METERS_PER_DEG_LAT);
        assert!((north - 44.0).abs() < 1e-9);
        assert!((south - 42.0).abs() < 1e-9);
        // a degree of longitude is shorter this far north, so the box is wider
        assert!(east - west > north - south);
        assert!((east - 7.0 - (7.0 - west)).abs() < 1e-9);
    }

    #[test]
    fn undrained_cells_read_apart_from_routed_ones() {
        assert_eq!(flow_direction_color(0.0), UNDRAINED_GREY);
        // each of the eight D8 codes gets its own colour
        let mut colors: Vec<[u8; 4]> = (0..8).map(|d| flow_direction_color(2f64.powi(d))).collect();
        colors.sort_unstable();
        colors.dedup();
        assert_eq!(colors.len(), 8);
    }

    #[test]
    fn the_accumulation_ramp_climbs_with_upstream_area() {
        let low = accumulation_color(1.0);
        let mid = accumulation_color(100.0);
        let high = accumulation_color(10_000.0);
        assert_ne!(low, mid);
        assert_ne!(mid, high);
        // past the saturation point the colour holds instead of wrapping
        assert_eq!(accumulation_color(1e9), accumulation_color(1e12));
    }

    fn facing_dem(south_facing: bool) -> Raster {
        // south-facing: elevation high at north (row 0), low at south (compass aspect 180).
        // north-facing: the reverse (compass aspect 0).
        let (w, h) = (5usize, 5usize);
        let mut data = vec![0.0; w * h];
        for row in 0..h {
            for col in 0..w {
                let v = if south_facing {
                    (h - 1 - row) as f64
                } else {
                    row as f64
                };
                data[row * w + col] = v * 20.0;
            }
        }
        Raster::from_vec(w, h, data, 10.0, NODATA).unwrap()
    }

    fn mean_irr(r: &Raster) -> f64 {
        let mut sum = 0.0;
        let mut count = 0.0;
        for &v in r.data() {
            if !r.is_nodata(v) {
                sum += v;
                count += 1.0;
            }
        }
        if count == 0.0 { 0.0 } else { sum / count }
    }

    #[test]
    fn solar_south_facing_beats_north_facing() {
        // northern-hemisphere latitude, summer day.
        let lat = 45.0;
        let doy = day_of_year("2026-06-21").unwrap();
        let south = solar_irradiance(&facing_dem(true), lat, doy);
        let north = solar_irradiance(&facing_dem(false), lat, doy);
        // a wrong aspect convention makes the two sides equal up to rounding,
        // so require a real gap
        assert!(
            mean_irr(&south) > mean_irr(&north) + 100.0,
            "south {} vs north {}",
            mean_irr(&south),
            mean_irr(&north)
        );
    }

    #[test]
    fn a_box_that_covers_no_ground_is_refused() {
        assert!(checked_bbox([7.0, 43.0, 8.0, 44.0]).is_ok());
        for bad in [
            [8.0, 43.0, 7.0, 44.0],
            [7.0, 44.0, 8.0, 43.0],
            [7.0, 43.0, 7.0, 44.0],
            [-181.0, 43.0, 8.0, 44.0],
            [7.0, -91.0, 8.0, 44.0],
            [f64::NAN, 43.0, 8.0, 44.0],
        ] {
            assert!(checked_bbox(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_resolution_stays_inside_the_range() {
        let (min, max) = RESOLUTION_RANGE;
        assert_eq!(resolution(None, DEFAULT_RESOLUTION), DEFAULT_RESOLUTION);
        assert_eq!(resolution(Some(0), DEFAULT_RESOLUTION), min);
        assert_eq!(resolution(Some(100_000), DEFAULT_RESOLUTION), max);
    }

    #[test]
    fn the_utm_zone_is_the_one_under_the_box_centre() {
        let zone_of = |bbox: [f64; 4]| {
            Utm::at_centre_of(bbox)
                .unwrap_or_else(|_| panic!("{bbox:?} has a utm zone"))
                .crs
        };
        assert_eq!(zone_of([7.0, 46.0, 8.0, 47.0]), Crs(32632));
        assert_eq!(zone_of([11.9, 46.0, 12.3, 47.0]), Crs(32633));
        assert_eq!(zone_of([151.0, -34.0, 151.5, -33.5]), Crs(32756));
        assert_eq!(zone_of([-180.0, 10.0, -179.5, 11.0]), Crs(32601));
        // a viewshed square around an observer on the antimeridian centres on 180
        assert_eq!(zone_of([179.5, 10.0, 180.5, 11.0]), Crs(32660));
    }

    #[tokio::test]
    async fn a_region_over_the_cap_is_a_bad_request_naming_the_cap() {
        let response = analysis_failure(geoplumb::Error::RegionTooLarge {
            pixels: 4_000_000,
            cap: 1_000_000,
            resolution: 30.0,
            finest_resolution: 60.0,
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let message = String::from_utf8_lossy(&body);
        assert!(message.contains("1000000 pixel cap"), "{message}");
        assert!(message.contains("60"), "{message}");
    }
}
