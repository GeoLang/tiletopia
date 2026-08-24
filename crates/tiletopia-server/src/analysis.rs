//! Terrain-analysis endpoints backed by terrano-core over the server's DEM.
//!
//! Elevation comes from [`crate::elevation`]: a loaded grid, a tile staged
//! under the data directory, then the SRTM cache. Cells no DEM covers are
//! nodata, and a box no DEM covers at all is refused rather than answered with
//! a blank raster. PNG rasters are returned as `image/png`; vector results as
//! GeoJSON.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use image::{Rgba, RgbaImage};
use serde::Deserialize;
use serde_json::{Value, json};
use terrano_core::{
    Raster, VIEWSHED_VISIBLE, aspect, contours, fill_sinks, flow_accumulation, flow_direction,
    hillshade, slope, viewshed, watershed,
};

use crate::AppState;
use crate::elevation::{ElevationField, ElevationGap, METERS_PER_DEG_LAT, on_the_globe};
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
}

#[derive(Deserialize)]
struct TerrainReq {
    /// slope | aspect | hillshade | contours | flow_direction |
    /// flow_accumulation | watershed
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

/// Line-of-sight visibility from the observer, as one square per visible cell.
///
/// terrano casts a ray to every cell inside the radius, so the result is the
/// shape of what can be seen: a ridge's shadow is a hole in the polygon set,
/// which a ring around the observer could not express.
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

    let field = state.elevation_sources().field(bbox).await?;
    let dem = dem_over_bbox(&field, bbox, side, side)?;
    let (row, col) = cell_at(bbox, side, side, lon, lat);
    let visible = viewshed(&dem, row, col, req.height_m, req.radius_m);

    let (polygons, cells) = cell_squares(&visible, bbox, |v| v == VIEWSHED_VISIBLE);
    let (dx, dy) = cell_meters(bbox, side, side);
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

async fn terrain(
    State(state): State<Arc<AppState>>,
    Json(req): Json<TerrainReq>,
) -> Result<Response, Refusal> {
    let bbox = checked_bbox(req.bbox)?;
    let side = resolution(req.resolution, DEFAULT_RESOLUTION);
    let field = state.elevation_sources().field(bbox).await?;
    let dem = dem_over_bbox(&field, bbox, side, side)?;

    match req.op.as_str() {
        "slope" => Ok(png_response(raster_png(&slope(&dem), slope_color))),
        "aspect" => Ok(png_response(aspect_png(&dem))),
        "hillshade" => {
            let azimuth = req.params.azimuth.unwrap_or(315.0);
            let altitude = req.params.altitude.unwrap_or(45.0);
            let shaded = hillshade(&dem, azimuth, altitude);
            Ok(png_response(raster_png(&shaded, hillshade_color)))
        }
        "contours" => {
            let interval = req
                .params
                .interval
                .unwrap_or_else(|| default_interval(&dem));
            Ok(Json(contours_geojson(&dem, bbox, interval)).into_response())
        }
        "flow_direction" => Ok(png_response(raster_png(
            &routed(&dem),
            flow_direction_color,
        ))),
        "flow_accumulation" => Ok(png_response(raster_png(
            &flow_accumulation(&routed(&dem)),
            accumulation_color,
        ))),
        "watershed" => Ok(png_response(raster_png(
            &watershed(&routed(&dem)),
            basin_color,
        ))),
        other => Err(bad_request(format!(
            "unknown op {other:?}, expected slope, aspect, hillshade, contours, \
             flow_direction, flow_accumulation or watershed"
        ))),
    }
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

/// Sample the server's elevation into a terrano raster. Row 0 is north, and a
/// cell no DEM covers is nodata.
///
/// A box no DEM covers at all is a gap rather than a raster of holes: every op
/// here would answer an empty image, which reads as flat ground.
fn dem_over_bbox(
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
        return Err(ElevationGap::NoCoverage(format!(
            "no elevation data staged for this area ({west}, {south}, {east}, {north})"
        )));
    }
    Ok(Raster::from_vec(width, height, data, (dx + dy) / 2.0, NODATA).expect("grid dims match"))
}

fn cell_lonlat(bbox: [f64; 4], width: usize, height: usize, row: usize, col: usize) -> (f64, f64) {
    let [w, s, e, n] = bbox;
    let lon = w + (col as f64) * (e - w) / (width - 1).max(1) as f64;
    let lat = n - (row as f64) * (n - s) / (height - 1).max(1) as f64;
    (lon, lat)
}

/// The cell nearest a point on the grid a bbox is sampled onto, the inverse of
/// [`cell_lonlat`].
fn cell_at(bbox: [f64; 4], width: usize, height: usize, lon: f64, lat: f64) -> (usize, usize) {
    let [w, s, e, n] = bbox;
    let on_axis = |fraction: f64, cells: usize| {
        (fraction * (cells - 1).max(1) as f64)
            .round()
            .clamp(0.0, (cells - 1) as f64) as usize
    };
    (
        on_axis((n - lat) / (n - s), height),
        on_axis((lon - w) / (e - w), width),
    )
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

// ── hydrology ───────────────────────────────────────────────────────────────

/// D8 flow directions over a depression-free copy of the DEM.
///
/// A raw DEM's pits swallow every path that reaches them, so accumulation
/// stops short of the outlet and basins split along the pits.
fn routed(dem: &Raster) -> Raster {
    flow_direction(&fill_sinks(dem))
}

// ── contours ────────────────────────────────────────────────────────────────

fn default_interval(dem: &Raster) -> f64 {
    let (mn, mx) = range(dem);
    ((mx - mn) / 10.0).max(1.0)
}

/// terrano contour vertices are world meters with origin bottom-left
/// (x = col*cell east, y = (height-1-row)*cell north). Map back to lon/lat.
fn contours_geojson(dem: &Raster, bbox: [f64; 4], interval: f64) -> Value {
    let lines = contours(dem, interval, 0.0);
    let [w, s, e, n] = bbox;
    let cell = dem.cell_size;
    let wf = (dem.width() - 1).max(1) as f64;
    let hf = (dem.height() - 1).max(1) as f64;
    let features: Vec<Value> = lines
        .iter()
        .filter(|l| l.vertices.len() >= 2)
        .map(|l| {
            let coords: Vec<[f64; 2]> = l
                .vertices
                .iter()
                .map(|&(x, y)| {
                    let lon = w + (x / cell) / wf * (e - w);
                    let lat = s + (y / cell) / hf * (n - s);
                    [lon, lat]
                })
                .collect();
            json!({
                "type": "Feature",
                "geometry": { "type": "LineString", "coordinates": coords },
                "properties": { "level": l.level }
            })
        })
        .collect();
    json!({ "type": "FeatureCollection", "features": features })
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
/// terrano's aspect is 0=East measured counter-clockwise, so physical south is
/// 270 degrees (verified empirically, not per its docstring).
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
    let sun_az = 270.0_f64.to_radians(); // due south at noon in terrano aspect space
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

/// Upstream cell count on a log ramp: a few hundred cells is already a channel,
/// and a basin outlet is thousands.
fn accumulation_color(v: f64) -> [u8; 4] {
    let decades = (v.max(0.0) + 1.0).log10() / ACCUMULATION_DECADES;
    let [r, g, b] = ramp(decades.clamp(0.0, 1.0));
    [r, g, b, 255]
}

/// Basin ids are labels, not magnitudes, so neighbouring ids get distant hues.
fn basin_color(v: f64) -> [u8; 4] {
    let [r, g, b] = hsv_to_rgb(v * BASIN_HUE_STEP, 0.75, 0.95);
    [r, g, b, 255]
}

fn aspect_png(dem: &Raster) -> Vec<u8> {
    raster_png(&aspect(dem), |v| {
        let [r, g, b] = hsv_to_rgb(v, 0.9, 1.0);
        [r, g, b, 255]
    })
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
    fn hillshade_png_decodes() {
        let dem = ramp_dem();
        let bytes = raster_png(&hillshade(&dem, 315.0, 45.0), hillshade_color);
        let img = image::load_from_memory(&bytes).expect("valid png");
        assert_eq!(img.width(), 5);
        assert_eq!(img.height(), 5);
    }

    fn cone_dem() -> Raster {
        // radial cone: peak at centre, so contours are closed loops.
        let (w, h) = (21usize, 21usize);
        let (cx, cy) = (10.0, 10.0);
        let mut data = vec![0.0; w * h];
        for row in 0..h {
            for col in 0..w {
                let d = ((col as f64 - cx).powi(2) + (row as f64 - cy).powi(2)).sqrt();
                data[row * w + col] = (100.0 - d * 5.0).max(0.0);
            }
        }
        Raster::from_vec(w, h, data, 10.0, NODATA).unwrap()
    }

    #[test]
    fn contours_are_ordered_and_closed() {
        let dem = cone_dem();
        let fc = contours_geojson(&dem, [0.0, 0.0, 1.0, 1.0], 20.0);
        let feats = fc["features"].as_array().unwrap();
        assert!(!feats.is_empty(), "cone should yield contour lines");
        // at least one contour of a cone is a closed loop (first vertex ~= last)
        let closed = feats.iter().any(|f| {
            let c = f["geometry"]["coordinates"].as_array().unwrap();
            if c.len() < 4 {
                return false;
            }
            let first = &c[0];
            let last = &c[c.len() - 1];
            (first[0].as_f64().unwrap() - last[0].as_f64().unwrap()).abs() < 1e-9
                && (first[1].as_f64().unwrap() - last[1].as_f64().unwrap()).abs() < 1e-9
        });
        assert!(closed, "expected at least one closed contour loop");
    }

    /// A wall down the middle column, seen from the west edge.
    fn walled_dem(side: usize) -> Raster {
        let mut data = vec![0.0; side * side];
        for row in 0..side {
            data[row * side + side / 2] = 400.0;
        }
        Raster::from_vec(side, side, data, 100.0, NODATA).unwrap()
    }

    #[test]
    fn the_viewshed_leaves_out_what_the_wall_hides() {
        let side = 21usize;
        let bbox = [7.0, 43.0, 7.2, 43.2];
        let dem = walled_dem(side);
        let observer = cell_lonlat(bbox, side, side, side / 2, 0);
        let (row, col) = cell_at(bbox, side, side, observer.0, observer.1);
        assert_eq!((row, col), (side / 2, 0), "the observer lands on its cell");

        let visible = viewshed(&dem, row, col, 2.0, f64::INFINITY);
        assert_eq!(
            visible.get(row, 2).unwrap(),
            VIEWSHED_VISIBLE,
            "the ground in front of the wall"
        );
        assert_eq!(
            visible.get(row, side - 1).unwrap(),
            terrano_core::VIEWSHED_HIDDEN,
            "the ground behind the wall"
        );

        let (polygons, cells) = cell_squares(&visible, bbox, |v| v == VIEWSHED_VISIBLE);
        assert_eq!(polygons.len() as u64, cells);
        // every column up to and including the wall, and nothing past it
        assert_eq!(cells, ((side / 2 + 1) * side) as u64);

        // flat ground the same size is seen whole
        let flat = Raster::from_vec(side, side, vec![0.0; side * side], 100.0, NODATA).unwrap();
        let (_, open) = cell_squares(&viewshed(&flat, row, col, 2.0, f64::INFINITY), bbox, |v| {
            v == VIEWSHED_VISIBLE
        });
        assert_eq!(open, (side * side) as u64);
    }

    #[test]
    fn a_cell_lookup_round_trips_through_its_coordinates() {
        let bbox = [7.0, 43.0, 8.0, 44.5];
        for (row, col) in [(0usize, 0usize), (5, 9), (15, 15)] {
            let (lon, lat) = cell_lonlat(bbox, 16, 16, row, col);
            assert_eq!(cell_at(bbox, 16, 16, lon, lat), (row, col));
        }
        // a point outside the box clamps onto the edge cell
        assert_eq!(cell_at(bbox, 16, 16, 0.0, 0.0), (15, 0));
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

    /// The valley DEM the hydrology ops are read over: a V draining south.
    ///
    /// The walls are steeper than the fall, so a cell on them drains diagonally
    /// into the channel rather than straight down its own column, and the
    /// channel gathers what the valley collects.
    fn valley_dem() -> Raster {
        let (width, height) = (9usize, 9usize);
        let mut data = vec![0.0; width * height];
        for row in 0..height {
            for col in 0..width {
                let from_channel = (col as f64 - 4.0).abs();
                data[row * width + col] = (height - 1 - row) as f64 * 5.0 + from_channel * 20.0;
            }
        }
        Raster::from_vec(width, height, data, 30.0, NODATA).unwrap()
    }

    #[test]
    fn flow_accumulates_down_the_channel() {
        let dem = valley_dem();
        let accumulation = flow_accumulation(&routed(&dem));
        let head = accumulation.get(1, 4).unwrap();
        let mouth = accumulation.get(7, 4).unwrap();
        assert!(
            mouth > head,
            "the channel gathers water downstream: {mouth} vs {head}"
        );
        // the channel carries more than the slope beside it
        assert!(accumulation.get(7, 4).unwrap() > accumulation.get(7, 0).unwrap());
    }

    #[test]
    fn flow_directions_point_downhill() {
        let dem = valley_dem();
        let directions = routed(&dem);
        // south is D8 code 4, and row 0 is north
        assert_eq!(directions.get(2, 4).unwrap(), 4.0);
    }

    #[test]
    fn watershed_labels_the_basin() {
        let dem = valley_dem();
        let basins = watershed(&routed(&dem));
        let labels: Vec<f64> = basins
            .data()
            .iter()
            .copied()
            .filter(|v| !basins.is_nodata(*v) && *v > 0.0)
            .collect();
        assert!(!labels.is_empty(), "a valley has at least one basin");
        assert!(labels.iter().all(|v| *v >= 1.0), "ids start at 1");
    }

    #[test]
    fn the_hydrology_ops_render() {
        let dem = valley_dem();
        let directions = routed(&dem);
        for bytes in [
            raster_png(&directions, flow_direction_color),
            raster_png(&flow_accumulation(&directions), accumulation_color),
            raster_png(&watershed(&directions), basin_color),
        ] {
            let img = image::load_from_memory(&bytes).expect("valid png");
            assert_eq!((img.width(), img.height()), (9, 9));
        }
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
        // south-facing: elevation high at north (row 0), low at south (terrano aspect 270).
        // north-facing: the reverse (terrano aspect 90).
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
        assert!(
            mean_irr(&south) > mean_irr(&north),
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
}
