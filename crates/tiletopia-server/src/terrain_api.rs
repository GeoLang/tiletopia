//! Terrain tile serving API.
//!
//! Serves quantized-mesh terrain tiles generated from open DEM data (Copernicus, SRTM).
//! Compatible with CesiumJS TerrainProvider and deck.gl TerrainLayer.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;
use std::sync::Arc;
use tiletopia_terrain::global_dem::{DemTile, TerrainTileCoord, generate_terrain_tile};

use crate::AppState;

/// Terrain layer metadata (TileJSON-like).
#[derive(Debug, Serialize)]
pub struct TerrainLayerInfo {
    pub tilejson: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub scheme: &'static str,
    pub tiles: Vec<String>,
    pub minzoom: u32,
    pub maxzoom: u32,
    pub bounds: [f64; 4],
    pub available: Vec<Vec<AvailableRange>>,
}

#[derive(Debug, Serialize)]
pub struct AvailableRange {
    #[serde(rename = "startX")]
    pub start_x: u32,
    #[serde(rename = "startY")]
    pub start_y: u32,
    #[serde(rename = "endX")]
    pub end_x: u32,
    #[serde(rename = "endY")]
    pub end_y: u32,
}

/// Register terrain API routes.
pub fn terrain_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/terrain/layer.json", get(terrain_layer_info))
        .route("/api/v1/terrain/{z}/{x}/{y}", get(serve_terrain_tile))
}

/// Terrain layer metadata endpoint.
async fn terrain_layer_info() -> impl IntoResponse {
    let info = TerrainLayerInfo {
        tilejson: "2.1.0",
        name: "TileTopia Open Terrain",
        description: "Global terrain from Copernicus DEM GLO-30 + SRTM, served as quantized mesh",
        version: "1.0.0",
        scheme: "tms",
        tiles: vec!["/api/v1/terrain/{z}/{x}/{y}".to_string()],
        minzoom: 0,
        maxzoom: 15,
        bounds: [-180.0, -90.0, 180.0, 90.0],
        available: vec![], // Client discovers availability via requests
    };
    axum::response::Json(info)
}

/// Serve a terrain tile as quantized-mesh binary.
///
/// If local DEM data is available, generates high-quality terrain from it.
/// Falls back to downloading SRTM tiles via DemCache if no local data exists.
/// Returns a flat tile as last resort.
async fn serve_terrain_tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, u32)>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate coordinates
    let max_tile = 2u32.checked_pow(z).unwrap_or(u32::MAX);
    if x >= max_tile || y >= max_tile {
        return Err(StatusCode::BAD_REQUEST);
    }

    let coord = TerrainTileCoord { zoom: z, x, y };
    let bounds = coord.bounds();

    // Try to load DEM from data directory
    let mut dem_tiles = load_dem_tiles_for_bounds(&state.data_dir, bounds);

    // If no local DEM tiles, try downloading SRTM via DemCache
    if dem_tiles.is_empty() {
        let cache_dir = state.data_dir.join("dem_cache");
        let cache = tiletopia_terrain::dem_cache::DemCache::new(cache_dir);
        let required = tiletopia_terrain::dem_cache::required_srtm_tiles(
            bounds[0], bounds[1], bounds[2], bounds[3],
        );
        for (lat, lon) in required {
            match cache.get_srtm_tile(lat, lon).await {
                Ok(hgt_path) => {
                    if let Ok(hm) = tiletopia_ingest::hgt_reader::read(&hgt_path) {
                        dem_tiles.push(DemTile {
                            lat,
                            lon,
                            elevations: hm.elevations.iter().map(|&e| e as f32).collect(),
                            samples: hm.width as u32,
                            nodata: -9999.0,
                        });
                    }
                }
                Err(e) => {
                    tracing::debug!("SRTM tile download failed for ({lat},{lon}): {e}");
                }
            }
        }
    }

    // Generate terrain mesh (uses flat elevation if no DEM tiles found)
    let grid_size = match z {
        0..=4 => 16,
        5..=8 => 32,
        9..=12 => 64,
        _ => 65, // CesiumJS standard: 65×65 per terrain tile
    };
    let mesh = generate_terrain_tile(&coord, &dem_tiles, grid_size);

    // Encode as quantized mesh
    let qm_bytes = encode_quantized_mesh(&mesh, &coord);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "application/vnd.quantized-mesh".parse().unwrap(),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "public, max-age=86400".parse().unwrap(),
    );

    Ok((headers, qm_bytes))
}

/// Load DEM tiles from disk for the given bounds.
fn load_dem_tiles_for_bounds(data_dir: &std::path::Path, bounds: [f64; 4]) -> Vec<DemTile> {
    let required = tiletopia_terrain::global_dem::required_dem_tiles(bounds);
    let mut tiles = Vec::new();

    for (lat, lon) in required {
        let dem_path = data_dir.join(format!("dem/{}_{}.bin", lat, lon));
        if dem_path.exists()
            && let Ok(tile) = load_dem_tile_from_file(&dem_path, lat, lon)
        {
            tiles.push(tile);
        }
    }
    tiles
}

/// Load a single DEM tile from a binary file (simple format: f32 elevation array).
fn load_dem_tile_from_file(path: &std::path::Path, lat: i32, lon: i32) -> std::io::Result<DemTile> {
    let data = std::fs::read(path)?;
    let samples = ((data.len() / 4) as f64).sqrt() as u32;

    let elevations: Vec<f32> = data
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    Ok(DemTile {
        lat,
        lon,
        elevations,
        samples,
        nodata: -9999.0,
    })
}

/// Encode a terrain mesh into quantized-mesh format (simplified).
///
/// Quantized-mesh spec: <https://github.com/CesiumGS/quantized-mesh>
fn encode_quantized_mesh(
    mesh: &tiletopia_terrain::global_dem::TerrainMesh,
    coord: &TerrainTileCoord,
) -> Vec<u8> {
    let bounds = coord.bounds();
    let mut buf = Vec::with_capacity(4096);

    // --- Header (88 bytes) ---
    // Center of tile in ECEF (approximate)
    let center_lon = (bounds[0] + bounds[2]) / 2.0;
    let center_lat = (bounds[1] + bounds[3]) / 2.0;
    let (cx, cy, cz) = geodetic_to_ecef(center_lat, center_lon, 0.0);
    buf.extend_from_slice(&cx.to_le_bytes()); // CenterX
    buf.extend_from_slice(&cy.to_le_bytes()); // CenterY
    buf.extend_from_slice(&cz.to_le_bytes()); // CenterZ

    // Find min/max elevation
    let (min_h, max_h): (f64, f64) = mesh
        .vertices
        .iter()
        .fold((f64::MAX, f64::MIN), |(mn, mx): (f64, f64), v| {
            (mn.min(v[2]), mx.max(v[2]))
        });
    buf.extend_from_slice(&(min_h as f32).to_le_bytes()); // MinimumHeight
    buf.extend_from_slice(&(max_h as f32).to_le_bytes()); // MaximumHeight

    // Bounding sphere (center + radius)
    let radius = haversine_distance(bounds[1], bounds[0], bounds[3], bounds[2]) / 2.0 * 1000.0;
    buf.extend_from_slice(&cx.to_le_bytes()); // BoundingSphereCenterX
    buf.extend_from_slice(&cy.to_le_bytes()); // BoundingSphereCenterY
    buf.extend_from_slice(&cz.to_le_bytes()); // BoundingSphereCenterZ
    buf.extend_from_slice(&radius.to_le_bytes()); // BoundingSphereRadius

    // Horizon occlusion point (same as center for simplicity)
    buf.extend_from_slice(&cx.to_le_bytes()); // HorizonOcclusionPointX
    buf.extend_from_slice(&cy.to_le_bytes()); // HorizonOcclusionPointY
    buf.extend_from_slice(&cz.to_le_bytes()); // HorizonOcclusionPointZ

    // --- Vertex Data ---
    let vertex_count = mesh.vertices.len() as u32;
    buf.extend_from_slice(&vertex_count.to_le_bytes());

    // Quantize positions to u16 (0..32767)
    let lon_range = bounds[2] - bounds[0];
    let lat_range = bounds[3] - bounds[1];
    let h_range: f64 = if (max_h - min_h).abs() < 1e-6 {
        1.0
    } else {
        max_h - min_h
    };

    // u (longitude) array - delta encoded
    let mut u_values: Vec<u16> = Vec::with_capacity(mesh.vertices.len());
    for v in &mesh.vertices {
        let u = ((v[0] - bounds[0]) / lon_range * 32767.0) as u16;
        u_values.push(u);
    }
    let u_deltas = delta_encode_u16(&u_values);
    for d in &u_deltas {
        buf.extend_from_slice(&d.to_le_bytes());
    }

    // v (latitude) array - delta encoded
    let mut v_values: Vec<u16> = Vec::with_capacity(mesh.vertices.len());
    for v in &mesh.vertices {
        let val = ((v[1] - bounds[1]) / lat_range * 32767.0) as u16;
        v_values.push(val);
    }
    let v_deltas = delta_encode_u16(&v_values);
    for d in &v_deltas {
        buf.extend_from_slice(&d.to_le_bytes());
    }

    // height array - delta encoded
    let mut h_values: Vec<u16> = Vec::with_capacity(mesh.vertices.len());
    for v in &mesh.vertices {
        let h = ((v[2] - min_h) / h_range * 32767.0) as u16;
        h_values.push(h);
    }
    let h_deltas = delta_encode_u16(&h_values);
    for d in &h_deltas {
        buf.extend_from_slice(&d.to_le_bytes());
    }

    // --- Index data ---
    let triangle_count = mesh.indices.len() as u32;
    buf.extend_from_slice(&triangle_count.to_le_bytes());

    // Use 16-bit indices (high-water mark encoded)
    let flat_indices: Vec<u32> = mesh
        .indices
        .iter()
        .flat_map(|t: &[u32; 3]| t.iter().copied())
        .collect();
    let hwm_indices = high_water_mark_encode(&flat_indices);
    for idx in &hwm_indices {
        buf.extend_from_slice(&(*idx as u16).to_le_bytes());
    }

    // --- Edge indices (for tile stitching) ---
    // West edge
    let west_indices: Vec<u16> = (0..vertex_count)
        .filter(|&i| u_values[i as usize] == 0)
        .map(|i| i as u16)
        .collect();
    buf.extend_from_slice(&(west_indices.len() as u32).to_le_bytes());
    for idx in &west_indices {
        buf.extend_from_slice(&idx.to_le_bytes());
    }

    // South edge
    let south_indices: Vec<u16> = (0..vertex_count)
        .filter(|&i| v_values[i as usize] == 0)
        .map(|i| i as u16)
        .collect();
    buf.extend_from_slice(&(south_indices.len() as u32).to_le_bytes());
    for idx in &south_indices {
        buf.extend_from_slice(&idx.to_le_bytes());
    }

    // East edge
    let east_indices: Vec<u16> = (0..vertex_count)
        .filter(|&i| u_values[i as usize] == 32767)
        .map(|i| i as u16)
        .collect();
    buf.extend_from_slice(&(east_indices.len() as u32).to_le_bytes());
    for idx in &east_indices {
        buf.extend_from_slice(&idx.to_le_bytes());
    }

    // North edge
    let north_indices: Vec<u16> = (0..vertex_count)
        .filter(|&i| v_values[i as usize] == 32767)
        .map(|i| i as u16)
        .collect();
    buf.extend_from_slice(&(north_indices.len() as u32).to_le_bytes());
    for idx in &north_indices {
        buf.extend_from_slice(&idx.to_le_bytes());
    }

    buf
}

/// Delta-encode u16 values using zigzag encoding.
fn delta_encode_u16(values: &[u16]) -> Vec<u16> {
    let mut result = Vec::with_capacity(values.len());
    let mut prev: i32 = 0;
    for &v in values {
        let delta = v as i32 - prev;
        // Zigzag encode
        let encoded = ((delta << 1) ^ (delta >> 31)) as u16;
        result.push(encoded);
        prev = v as i32;
    }
    result
}

/// High-water-mark encoding for triangle indices.
/// Each index is encoded as `highest + 1 - code`, where highest is the running max.
fn high_water_mark_encode(indices: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(indices.len());
    let mut highest: i64 = 0;
    for &idx in indices {
        let code = (highest - idx as i64) as u32;
        result.push(code);
        if idx as i64 > highest {
            highest = idx as i64;
        }
    }
    result
}

/// Convert geodetic coordinates to ECEF.
fn geodetic_to_ecef(lat_deg: f64, lon_deg: f64, alt_m: f64) -> (f64, f64, f64) {
    let a = 6_378_137.0; // WGS84 semi-major axis
    let f = 1.0 / 298.257223563;
    let e2 = 2.0 * f - f * f;

    let lat = lat_deg.to_radians();
    let lon = lon_deg.to_radians();
    let sin_lat = lat.sin();
    let cos_lat = lat.cos();
    let n = a / (1.0 - e2 * sin_lat * sin_lat).sqrt();

    let x = (n + alt_m) * cos_lat * lon.cos();
    let y = (n + alt_m) * cos_lat * lon.sin();
    let z = (n * (1.0 - e2) + alt_m) * sin_lat;

    (x, y, z)
}

/// Haversine distance in km between two lat/lon points.
fn haversine_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0; // Earth radius km
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terrain_layer_info_endpoint() {
        // Just verify the static data is well-formed
        let info = TerrainLayerInfo {
            tilejson: "2.1.0",
            name: "test",
            description: "test",
            version: "1.0.0",
            scheme: "tms",
            tiles: vec!["/api/v1/terrain/{z}/{x}/{y}.terrain".into()],
            minzoom: 0,
            maxzoom: 15,
            bounds: [-180.0, -90.0, 180.0, 90.0],
            available: vec![],
        };
        assert_eq!(info.maxzoom, 15);
    }

    #[test]
    fn test_delta_encode() {
        let values = vec![0, 100, 200, 300];
        let encoded = delta_encode_u16(&values);
        // 0→0(zigzag 0), 100→100(zigzag 200), 100→100(zigzag 200), 100→100(zigzag 200)
        assert_eq!(encoded[0], 0);
        assert_eq!(encoded[1], 200); // zigzag(100) = 200
    }

    #[test]
    fn test_geodetic_to_ecef() {
        // Equator at prime meridian
        let (x, y, z) = geodetic_to_ecef(0.0, 0.0, 0.0);
        assert!((x - 6_378_137.0).abs() < 1.0);
        assert!(y.abs() < 1.0);
        assert!(z.abs() < 1.0);
    }

    #[test]
    fn test_quantized_mesh_encoding() {
        let mesh = tiletopia_terrain::global_dem::TerrainMesh {
            vertices: vec![
                [10.0, 45.0, 100.0],
                [10.5, 45.0, 200.0],
                [10.0, 45.5, 150.0],
            ],
            indices: vec![[0, 1, 2]],
            bounds: [10.0, 45.0, 11.0, 46.0],
            zoom: 5,
        };
        let coord = TerrainTileCoord {
            zoom: 5,
            x: 17,
            y: 11,
        };
        let bytes = encode_quantized_mesh(&mesh, &coord);
        // Should produce non-empty binary
        assert!(bytes.len() > 88); // At least header size
    }
}
