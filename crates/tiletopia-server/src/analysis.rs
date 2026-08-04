//! Terrain-analysis endpoints backed by terrano-core over the elevation DEM.
//!
//! Elevation comes from `elevation::get_elevation`, which serves loaded DEM grids
//! and falls back to a deterministic synthetic field when no grid covers the area.
//! Results are therefore honest for whatever DEM is loaded, and a real demo surface
//! otherwise. PNG rasters are returned as `image/png`; vector results as GeoJSON.

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
use terrano_core::{Raster, aspect, contours, hillshade, slope};

use crate::AppState;
use crate::elevation::{self, DemStore};

const NODATA: f64 = -9999.0;
const METERS_PER_DEG_LAT: f64 = 111_320.0;

pub fn analysis_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/analysis/viewshed", post(viewshed))
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
    rays: Option<usize>,
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
    op: String, // slope | aspect | hillshade | contours
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

async fn viewshed(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ViewshedReq>,
) -> Result<Response, StatusCode> {
    let [lon, lat] = req.observer;
    let rays = req.rays.unwrap_or(120).clamp(8, 720);
    let sample = |plat: f64, plon: f64| {
        elevation::get_elevation(plat, plon, &state.elevation_store).elevation_m
    };
    let ring = viewshed_ring(&sample, lon, lat, req.height_m, req.radius_m, rays, 160);
    let area = ring_area_m2(&ring, lat);
    let fc = json!({
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "geometry": { "type": "Polygon", "coordinates": [ring] },
            "properties": { "visible_area_m2": area, "observer": [lon, lat], "radius_m": req.radius_m }
        }]
    });
    Ok(Json(fc).into_response())
}

async fn flood(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FloodReq>,
) -> Result<Response, StatusCode> {
    let res = req.resolution.unwrap_or(96).clamp(8, 256);
    let dem = dem_over_bbox(&state.elevation_store, req.bbox, res, res);
    let (polys, cells) = flood_polygons(&dem, req.bbox, req.level_m);
    let (dx, dy) = cell_meters(req.bbox, res, res);
    let features: Vec<Value> = if polys.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "type": "Feature",
            "geometry": { "type": "MultiPolygon", "coordinates": polys },
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
) -> Result<Response, StatusCode> {
    let res = req.resolution.unwrap_or(160).clamp(8, 256);
    let dem = dem_over_bbox(&state.elevation_store, req.bbox, res, res);
    match req.op.as_str() {
        "slope" => Ok(png_response(slope_png(&dem))),
        "aspect" => Ok(png_response(aspect_png(&dem))),
        "hillshade" => {
            let az = req.params.azimuth.unwrap_or(315.0);
            let alt = req.params.altitude.unwrap_or(45.0);
            Ok(png_response(hillshade_png(&dem, az, alt)))
        }
        "contours" => {
            let interval = req
                .params
                .interval
                .unwrap_or_else(|| default_interval(&dem));
            Ok(Json(contours_geojson(&dem, req.bbox, interval)).into_response())
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

async fn solar(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SolarReq>,
) -> Result<Response, StatusCode> {
    let res = req.resolution.unwrap_or(160).clamp(8, 256);
    let doy = day_of_year(&req.date).ok_or(StatusCode::BAD_REQUEST)?;
    let dem = dem_over_bbox(&state.elevation_store, req.bbox, res, res);
    let [_, s, _, n] = req.bbox;
    let mid_lat = (s + n) / 2.0;
    let irr = solar_irradiance(&dem, mid_lat, doy);
    Ok(png_response(solar_png(&irr)))
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

/// Sample the elevation store into a terrano raster. Row 0 is north.
fn dem_over_bbox(store: &DemStore, bbox: [f64; 4], width: usize, height: usize) -> Raster {
    let [w, s, e, n] = bbox;
    let (dx, dy) = cell_meters(bbox, width, height);
    let cell = (dx + dy) / 2.0;
    let mut data = vec![0.0; width * height];
    for row in 0..height {
        let lat = n - (row as f64) * (n - s) / (height - 1).max(1) as f64;
        for col in 0..width {
            let lon = w + (col as f64) * (e - w) / (width - 1).max(1) as f64;
            data[row * width + col] = elevation::get_elevation(lat, lon, store).elevation_m;
        }
    }
    Raster::from_vec(width, height, data, cell, NODATA).expect("grid dims match")
}

// ── viewshed (radial line-of-sight sweep) ───────────────────────────────────

/// Cast `rays` radial lines out to `radius_m`, marching `steps` samples each.
/// A sample is visible when its elevation angle from the observer eye exceeds
/// every closer sample on the ray; the returned ring holds the farthest visible
/// point per azimuth, so the polygon area grows with how much terrain is seen.
fn viewshed_ring<F: Fn(f64, f64) -> f64>(
    sample: &F,
    lon: f64,
    lat: f64,
    height_m: f64,
    radius_m: f64,
    rays: usize,
    steps: usize,
) -> Vec<[f64; 2]> {
    let eye = sample(lat, lon) + height_m;
    let m_lat = METERS_PER_DEG_LAT;
    let m_lon = METERS_PER_DEG_LAT * lat.to_radians().cos();
    let mut ring = Vec::with_capacity(rays + 1);
    for i in 0..rays {
        // azimuth from north, clockwise: east = sin, north = cos
        let az = (i as f64) / (rays as f64) * std::f64::consts::TAU;
        let (saz, caz) = az.sin_cos();
        let mut max_angle = f64::NEG_INFINITY;
        let mut visible_r = 0.0;
        for k in 1..=steps {
            let r = radius_m * (k as f64) / (steps as f64);
            let plon = lon + (r * saz) / m_lon;
            let plat = lat + (r * caz) / m_lat;
            let angle = (sample(plat, plon) - eye).atan2(r);
            if angle > max_angle {
                max_angle = angle;
                visible_r = r;
            }
        }
        ring.push([
            lon + (visible_r * saz) / m_lon,
            lat + (visible_r * caz) / m_lat,
        ]);
    }
    if let Some(&first) = ring.first() {
        ring.push(first);
    }
    ring
}

fn ring_area_m2(ring: &[[f64; 2]], lat0: f64) -> f64 {
    if ring.len() < 4 {
        return 0.0;
    }
    let mx = METERS_PER_DEG_LAT * lat0.to_radians().cos();
    let my = METERS_PER_DEG_LAT;
    let mut a = 0.0;
    for i in 0..ring.len() - 1 {
        let (x1, y1) = (ring[i][0] * mx, ring[i][1] * my);
        let (x2, y2) = (ring[i + 1][0] * mx, ring[i + 1][1] * my);
        a += x1 * y2 - x2 * y1;
    }
    a.abs() / 2.0
}

// ── flood (threshold + per-cell polygonize) ─────────────────────────────────

fn cell_lonlat(bbox: [f64; 4], width: usize, height: usize, row: usize, col: usize) -> (f64, f64) {
    let [w, s, e, n] = bbox;
    let lon = w + (col as f64) * (e - w) / (width - 1).max(1) as f64;
    let lat = n - (row as f64) * (n - s) / (height - 1).max(1) as f64;
    (lon, lat)
}

/// Return MultiPolygon coordinates (one square per below-level cell) and the cell count.
fn flood_polygons(dem: &Raster, bbox: [f64; 4], level: f64) -> (Vec<Value>, u64) {
    let (w, h) = (dem.width(), dem.height());
    let [west, south, east, north] = bbox;
    let hx = (east - west) / (w - 1).max(1) as f64 / 2.0;
    let hy = (north - south) / (h - 1).max(1) as f64 / 2.0;
    let mut polys = Vec::new();
    let mut count = 0u64;
    for row in 0..h {
        for col in 0..w {
            let v = dem.get(row, col).unwrap();
            if dem.is_nodata(v) || v >= level {
                continue;
            }
            count += 1;
            let (lon, lat) = cell_lonlat(bbox, w, h, row, col);
            let ring = json!([
                [lon - hx, lat - hy],
                [lon + hx, lat - hy],
                [lon + hx, lat + hy],
                [lon - hx, lat + hy],
                [lon - hx, lat - hy]
            ]);
            polys.push(json!([ring]));
        }
    }
    (polys, count)
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

fn hillshade_png(dem: &Raster, azimuth: f64, altitude: f64) -> Vec<u8> {
    raster_png(&hillshade(dem, azimuth, altitude), hillshade_color)
}

fn slope_png(dem: &Raster) -> Vec<u8> {
    raster_png(&slope(dem), slope_color)
}

fn aspect_png(dem: &Raster) -> Vec<u8> {
    let asp = aspect(dem);
    raster_png(&asp, |v| {
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

    #[test]
    fn viewshed_peak_sees_more_than_pit() {
        // meters from the observer at (0,0).
        let dist_m = |lat: f64, lon: f64| {
            let mx = METERS_PER_DEG_LAT;
            ((lat * mx).powi(2) + (lon * mx).powi(2)).sqrt()
        };
        // peak: terrain falls away monotonically -> visible to the full radius.
        let peak = |lat: f64, lon: f64| 1000.0 - dist_m(lat, lon) * 0.5;
        // crater: rim at 100 m rises above the eye then falls, occluding everything
        // beyond it -> visible extent capped near the rim.
        let crater = |lat: f64, lon: f64| 1000.0 + 60.0 - (dist_m(lat, lon) - 100.0).abs() * 0.6;
        let peak_ring = viewshed_ring(&peak, 0.0, 0.0, 2.0, 500.0, 120, 200);
        let crater_ring = viewshed_ring(&crater, 0.0, 0.0, 2.0, 500.0, 120, 200);
        let peak_area = ring_area_m2(&peak_ring, 0.0);
        let crater_area = ring_area_m2(&crater_ring, 0.0);
        assert!(
            peak_area > crater_area * 2.0,
            "peak {peak_area} vs crater {crater_area}"
        );
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
        let (_, low) = flood_polygons(&dem, bbox, 1.5);
        let (_, high) = flood_polygons(&dem, bbox, 3.5);
        assert!(high > low, "high {high} should exceed low {low}");
        assert!(low > 0);
    }

    #[test]
    fn hillshade_png_decodes() {
        let dem = ramp_dem();
        let bytes = hillshade_png(&dem, 315.0, 45.0);
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
}
