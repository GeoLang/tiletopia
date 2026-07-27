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

/// Deepest zoom the tile route serves, and the deepest level layer.json
/// advertises as available.
const MAX_TERRAIN_ZOOM: u32 = 15;

/// Terrain layer metadata, as CesiumTerrainProvider's layer.json parser reads it.
#[derive(Debug, Serialize)]
pub struct TerrainLayerInfo {
    pub tilejson: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub version: &'static str,
    pub format: &'static str,
    pub projection: &'static str,
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
        // bumped past 1.0.0: tiles are cached 24h and pre-1.0.1 meshes were latitude-mirrored
        version: "1.0.1",
        format: "quantized-mesh-1.0",
        projection: "EPSG:4326",
        scheme: "tms",
        // relative to the layer.json URL, so it survives any proxy prefix
        tiles: vec!["{z}/{x}/{y}.terrain?v={version}".to_string()],
        minzoom: 0,
        maxzoom: MAX_TERRAIN_ZOOM,
        bounds: [-180.0, -90.0, 180.0, 90.0],
        available: full_availability(MAX_TERRAIN_ZOOM),
    };
    axum::response::Json(info)
}

/// Every tile of every level is generated on demand, so availability is the
/// full pyramid. Cesium needs it to know how deep it may refine: with no
/// `available` array it treats depth as unknown and keeps requesting past
/// maxzoom.
fn full_availability(max_zoom: u32) -> Vec<Vec<AvailableRange>> {
    (0..=max_zoom)
        .map(|z| {
            let (x_tiles, y_tiles) = TerrainTileCoord::grid_at_zoom(z);
            vec![AvailableRange {
                start_x: 0,
                start_y: 0,
                end_x: x_tiles - 1,
                end_y: y_tiles - 1,
            }]
        })
        .collect()
}

/// Serve a terrain tile as quantized-mesh binary.
///
/// If local DEM data is available, generates high-quality terrain from it.
/// Falls back to downloading a bounded set of SRTM tiles via DemCache if no
/// local data exists. Returns a flat tile as last resort.
async fn serve_terrain_tile(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let coord = parse_tile_coord(z, x, &y).ok_or(StatusCode::BAD_REQUEST)?;
    let dem_tiles = dem_tiles_for_bounds(&state, coord.bounds()).await;

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

/// Parse a tile coordinate from the path, where Cesium sends y as `{y}.terrain`.
fn parse_tile_coord(z: u32, x: u32, y: &str) -> Option<TerrainTileCoord> {
    let y: u32 = y.strip_suffix(".terrain").unwrap_or(y).parse().ok()?;
    if z > MAX_TERRAIN_ZOOM {
        return None;
    }
    let (x_tiles, y_tiles) = TerrainTileCoord::grid_at_zoom(z);
    if x >= x_tiles || y >= y_tiles {
        return None;
    }
    Some(TerrainTileCoord { zoom: z, x, y })
}

/// DEM tiles covering these bounds: local files first, then a bounded set of
/// SRTM downloads when there is nothing on disk.
///
/// Shared by the quantized-mesh and terrain-RGB endpoints so the fetch bound
/// and the HGT orientation are decided in one place.
pub(crate) async fn dem_tiles_for_bounds(state: &AppState, bounds: [f64; 4]) -> Vec<DemTile> {
    let mut dem_tiles = load_dem_tiles_for_bounds(&state.data_dir, bounds);
    if !dem_tiles.is_empty() {
        return dem_tiles;
    }

    let cache = tiletopia_terrain::dem_cache::DemCache::new(state.data_dir.join("dem_cache"));
    for (lat, lon) in srtm_tiles_to_fetch(bounds) {
        match cache.get_srtm_tile(lat, lon).await {
            Ok(hgt_path) => match dem_tile_from_hgt(&hgt_path, lat, lon) {
                Ok(tile) => dem_tiles.push(tile),
                Err(e) => tracing::warn!("SRTM tile {} unusable: {e}", hgt_path.display()),
            },
            Err(e) => tracing::debug!("SRTM tile download failed for ({lat},{lon}): {e}"),
        }
    }
    dem_tiles
}

/// Read a cached HGT file into a DEM tile.
///
/// HGT rows run north-to-south, the opposite of the order [`DemTile`] samples,
/// so this must go through [`DemTile::from_north_up`]. Getting it wrong mirrors
/// every elevation about the tile's mid-latitude, which reads as plausible
/// terrain in the wrong place rather than as an error.
fn dem_tile_from_hgt(
    path: &std::path::Path,
    lat: i32,
    lon: i32,
) -> Result<DemTile, std::io::Error> {
    let hm = tiletopia_ingest::hgt_reader::read(path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let elevations = hm.elevations.iter().map(|&e| e as f32).collect();
    DemTile::from_north_up(lat, lon, elevations, hm.width as u32, -9999.0).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}×{} is not a usable DEM grid", hm.width, hm.height),
        )
    })
}

/// Most one-degree SRTM tiles a single terrain request may pull from upstream.
///
/// These reads are anonymous, and a low-zoom terrain tile covers tens of
/// thousands of one-degree cells, so an unbounded fetch turns one GET into a
/// multi-terabyte download loop. Above the bound the request renders from local
/// DEM or flat terrain instead, which is what a wide tile did in practice
/// anyway: it never finished.
const MAX_SRTM_TILES_PER_REQUEST: usize = 16;

/// SRTM tiles to fetch for these bounds, empty when the area is too wide to
/// serve from upstream downloads.
fn srtm_tiles_to_fetch(bounds: [f64; 4]) -> Vec<(i32, i32)> {
    let required = tiletopia_terrain::dem_cache::required_srtm_tiles(
        bounds[0], bounds[1], bounds[2], bounds[3],
    );
    if required.len() > MAX_SRTM_TILES_PER_REQUEST {
        tracing::debug!(
            "terrain tile spans {} SRTM tiles, over the {MAX_SRTM_TILES_PER_REQUEST} fetch bound",
            required.len()
        );
        return Vec::new();
    }
    required
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

    DemTile::from_south_up(lat, lon, elevations, samples, -9999.0).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: not a square f32 DEM grid", path.display()),
        )
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

    // Cesium decodes indices with a high-water mark, which only works when
    // vertices are numbered in order of first use.
    let (positions, flat_indices) = reorder_by_first_use(mesh);

    // --- Header (88 bytes) ---
    // Center of tile in ECEF (approximate)
    let center_lon = (bounds[0] + bounds[2]) / 2.0;
    let center_lat = (bounds[1] + bounds[3]) / 2.0;
    let (cx, cy, cz) = geodetic_to_ecef(center_lat, center_lon, 0.0);
    buf.extend_from_slice(&cx.to_le_bytes()); // CenterX
    buf.extend_from_slice(&cy.to_le_bytes()); // CenterY
    buf.extend_from_slice(&cz.to_le_bytes()); // CenterZ

    // Find min/max elevation
    let (min_h, max_h): (f64, f64) = positions
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
    buf.extend_from_slice(&(positions.len() as u32).to_le_bytes());

    // Quantize positions to u16 (0..32767)
    let lon_range = bounds[2] - bounds[0];
    let lat_range = bounds[3] - bounds[1];
    let h_range: f64 = if (max_h - min_h).abs() < 1e-6 {
        1.0
    } else {
        max_h - min_h
    };

    let quantize =
        |value: f64, origin: f64, range: f64| ((value - origin) / range * 32767.0) as u16;
    let u_values: Vec<u16> = positions
        .iter()
        .map(|v| quantize(v[0], bounds[0], lon_range))
        .collect();
    let v_values: Vec<u16> = positions
        .iter()
        .map(|v| quantize(v[1], bounds[1], lat_range))
        .collect();
    let h_values: Vec<u16> = positions
        .iter()
        .map(|v| quantize(v[2], min_h, h_range))
        .collect();

    for values in [&u_values, &v_values, &h_values] {
        for d in delta_encode_u16(values) {
            buf.extend_from_slice(&d.to_le_bytes());
        }
    }

    // --- Index data ---
    buf.extend_from_slice(&(mesh.indices.len() as u32).to_le_bytes());
    for idx in high_water_mark_encode(&flat_indices) {
        buf.extend_from_slice(&(idx as u16).to_le_bytes());
    }

    // --- Edge indices (for tile stitching) ---
    let west = edge_indices(&u_values, 0, &v_values);
    let south = edge_indices(&v_values, 0, &u_values);
    let east = edge_indices(&u_values, 32767, &v_values);
    let north = edge_indices(&v_values, 32767, &u_values);
    for edge in [west, south, east, north] {
        buf.extend_from_slice(&(edge.len() as u32).to_le_bytes());
        for idx in edge {
            buf.extend_from_slice(&idx.to_le_bytes());
        }
    }

    buf
}

/// Renumber the mesh's vertices in order of first use by its triangle list,
/// returning the reordered positions and the flat index list.
///
/// High-water-mark encoding can only express an index that has been seen
/// before or is exactly the next unused one, so any other numbering decodes to
/// garbage. Vertices no triangle references are dropped.
fn reorder_by_first_use(
    mesh: &tiletopia_terrain::global_dem::TerrainMesh,
) -> (Vec<[f64; 3]>, Vec<u32>) {
    let mut new_of_old = vec![u32::MAX; mesh.vertices.len()];
    let mut positions = Vec::with_capacity(mesh.vertices.len());
    let mut flat = Vec::with_capacity(mesh.indices.len() * 3);

    for old in mesh.indices.iter().flatten().copied() {
        let slot = &mut new_of_old[old as usize];
        if *slot == u32::MAX {
            *slot = positions.len() as u32;
            positions.push(mesh.vertices[old as usize]);
        }
        flat.push(*slot);
    }
    (positions, flat)
}

/// Vertices lying on one tile edge, sorted along the edge as the quantized-mesh
/// spec requires so Cesium can stitch and skirt them.
fn edge_indices(across: &[u16], across_value: u16, along: &[u16]) -> Vec<u16> {
    let mut indices: Vec<u16> = (0..across.len() as u16)
        .filter(|&i| across[i as usize] == across_value)
        .collect();
    indices.sort_by_key(|&i| along[i as usize]);
    indices
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
///
/// Each index is written as `highest - index`, and the high-water mark advances
/// whenever that code is zero, mirroring Cesium's decoder. Requires indices
/// numbered in order of first use, so `index <= highest` always holds.
fn high_water_mark_encode(indices: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(indices.len());
    let mut highest: u32 = 0;
    for &idx in indices {
        result.push(highest - idx);
        if idx == highest {
            highest += 1;
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
    fn availability_covers_every_level() {
        let levels = full_availability(MAX_TERRAIN_ZOOM);
        assert_eq!(levels.len() as u32, MAX_TERRAIN_ZOOM + 1);

        let root = &levels[0][0];
        assert_eq!(
            (root.start_x, root.start_y, root.end_x, root.end_y),
            (0, 0, 1, 0)
        );

        let deepest = &levels[MAX_TERRAIN_ZOOM as usize][0];
        let (x_tiles, y_tiles) = TerrainTileCoord::grid_at_zoom(MAX_TERRAIN_ZOOM);
        assert_eq!((deepest.end_x, deepest.end_y), (x_tiles - 1, y_tiles - 1));
    }

    #[test]
    fn tile_path_accepts_what_cesium_sends() {
        // both zoom-0 roots of the geographic scheme
        assert!(parse_tile_coord(0, 0, "0.terrain").is_some());
        assert!(parse_tile_coord(0, 1, "0.terrain").is_some());
        // bare form, still served for deck.gl and manual pokes
        assert!(parse_tile_coord(0, 1, "0").is_some());

        assert!(parse_tile_coord(0, 2, "0").is_none()); // past 2 tiles at zoom 0
        assert!(parse_tile_coord(0, 0, "1").is_none()); // past 1 tile at zoom 0
        assert!(parse_tile_coord(MAX_TERRAIN_ZOOM + 1, 0, "0").is_none());
        assert!(parse_tile_coord(0, 0, "0.png").is_none());
    }

    #[test]
    fn wide_tiles_do_not_fetch_srtm() {
        // zoom 0 spans the globe: ~62k one-degree tiles, so nothing is fetched
        let world = TerrainTileCoord {
            zoom: 0,
            x: 0,
            y: 0,
        };
        assert!(srtm_tiles_to_fetch(world.bounds()).is_empty());

        let zoom_4 = TerrainTileCoord {
            zoom: 4,
            x: 8,
            y: 5,
        };
        assert!(srtm_tiles_to_fetch(zoom_4.bounds()).is_empty());
    }

    #[test]
    fn narrow_tiles_fetch_a_bounded_set() {
        let coord = TerrainTileCoord {
            zoom: 12,
            x: 2200,
            y: 1400,
        };
        let tiles = srtm_tiles_to_fetch(coord.bounds());
        assert!(!tiles.is_empty());
        assert!(tiles.len() <= MAX_SRTM_TILES_PER_REQUEST);
    }

    #[test]
    fn hgt_rows_keep_their_latitude_through_the_reader() {
        // guards the seam the flip lived in: hgt_reader hands back north-up
        // rows, DemTile samples south-up
        let dir = std::env::temp_dir().join("tiletopia_hgt_orientation_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("N43E007.hgt");

        // 61×61, alpine along the north edge and sea level along the south
        let side = 61usize;
        let mut raw = Vec::with_capacity(side * side * 2);
        for row in 0..side {
            let height = (1658.0 * (1.0 - row as f64 / (side - 1) as f64)) as i16;
            for _ in 0..side {
                raw.extend_from_slice(&height.to_be_bytes());
            }
        }
        std::fs::write(&path, &raw).unwrap();

        let tile = dem_tile_from_hgt(&path, 43, 7).unwrap();
        assert!(
            tile.sample(43.97, 7.4).unwrap() > 1500.0,
            "north edge must stay alpine"
        );
        assert!(
            tile.sample(43.03, 7.4).unwrap() < 100.0,
            "south edge must stay coast"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_dem_files_are_skipped_not_sampled() {
        let dir = std::env::temp_dir().join("tiletopia_truncated_dem_test/dem");
        std::fs::create_dir_all(&dir).unwrap();

        // empty file: the shape that crashed the sampler
        let empty = dir.join("43_7.bin");
        std::fs::write(&empty, []).unwrap();
        assert!(load_dem_tile_from_file(&empty, 43, 7).is_err());

        // half a grid written so far
        let partial = dir.join("43_8.bin");
        std::fs::write(&partial, vec![0u8; 4 * 10]).unwrap();
        assert!(load_dem_tile_from_file(&partial, 43, 8).is_err());

        // the bounds scan drops both rather than propagating a failure
        let data_dir = dir.parent().unwrap();
        assert!(load_dem_tiles_for_bounds(data_dir, [7.0, 43.0, 9.0, 44.0]).is_empty());

        std::fs::remove_dir_all(data_dir).ok();
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

    /// Decode a quantized mesh the way CesiumTerrainProvider does, so the
    /// encoder is checked against the reader it has to satisfy.
    fn decode_quantized_mesh(bytes: &[u8]) -> DecodedMesh {
        let mut pos = 88; // header
        let u32_at = |pos: &mut usize| {
            let v = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            v
        };
        let vertex_count = u32_at(&mut pos) as usize;

        let zigzag_delta = |pos: &mut usize| {
            let mut values = Vec::with_capacity(vertex_count);
            let mut running: i32 = 0;
            for _ in 0..vertex_count {
                let raw = u16::from_le_bytes(bytes[*pos..*pos + 2].try_into().unwrap()) as i32;
                running += (raw >> 1) ^ -(raw & 1);
                values.push(running as u16);
                *pos += 2;
            }
            values
        };
        let u = zigzag_delta(&mut pos);
        let v = zigzag_delta(&mut pos);
        let heights = zigzag_delta(&mut pos);

        let triangle_count = u32_at(&mut pos) as usize;
        let mut indices = Vec::with_capacity(triangle_count * 3);
        let mut highest: u32 = 0;
        for _ in 0..triangle_count * 3 {
            let code = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as u32;
            indices.push(highest.wrapping_sub(code));
            if code == 0 {
                highest += 1;
            }
            pos += 2;
        }

        let mut edges = Vec::new();
        for _ in 0..4 {
            let count = u32_at(&mut pos) as usize;
            let mut edge = Vec::with_capacity(count);
            for _ in 0..count {
                edge.push(u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()));
                pos += 2;
            }
            edges.push(edge);
        }

        assert_eq!(pos, bytes.len(), "decoder consumed the whole buffer");
        DecodedMesh {
            u,
            v,
            heights,
            indices,
            edges,
        }
    }

    struct DecodedMesh {
        u: Vec<u16>,
        v: Vec<u16>,
        heights: Vec<u16>,
        indices: Vec<u32>,
        edges: Vec<Vec<u16>>,
    }

    #[test]
    fn quantized_mesh_round_trips_through_cesiums_decoding() {
        let coord = TerrainTileCoord {
            zoom: 0,
            x: 1,
            y: 0,
        };
        let grid = 16;
        let mut mesh = tiletopia_terrain::global_dem::generate_terrain_tile(&coord, &[], grid);
        // vary the elevations so the height quantization is exercised too
        for (i, v) in mesh.vertices.iter_mut().enumerate() {
            v[2] = (i % 37) as f64 * 41.0;
        }
        let decoded = decode_quantized_mesh(&encode_quantized_mesh(&mesh, &coord));

        let vertex_count = grid as usize * grid as usize;
        assert_eq!(decoded.u.len(), vertex_count);
        assert_eq!(decoded.heights.len(), vertex_count);
        assert_eq!(decoded.indices.len(), mesh.indices.len() * 3);
        assert!(decoded.indices.iter().all(|&i| (i as usize) < vertex_count));

        // decoded triangles must address the same positions the mesh had
        let (positions, _) = reorder_by_first_use(&mesh);
        for (triangle, decoded) in mesh.indices.iter().zip(decoded.indices.chunks(3)) {
            for (&old, &new) in triangle.iter().zip(decoded) {
                assert_eq!(positions[new as usize], mesh.vertices[old as usize]);
            }
        }

        // dequantizing must land back on the original lon/lat/height
        let bounds = coord.bounds();
        let (min_h, max_h) = (0.0, 36.0 * 41.0);
        for (i, position) in positions.iter().enumerate() {
            let lon = bounds[0] + decoded.u[i] as f64 / 32767.0 * (bounds[2] - bounds[0]);
            let lat = bounds[1] + decoded.v[i] as f64 / 32767.0 * (bounds[3] - bounds[1]);
            let height = min_h + decoded.heights[i] as f64 / 32767.0 * (max_h - min_h);
            assert!(
                (lon - position[0]).abs() < 0.01,
                "lon {lon} vs {}",
                position[0]
            );
            assert!(
                (lat - position[1]).abs() < 0.01,
                "lat {lat} vs {}",
                position[1]
            );
            assert!(
                (height - position[2]).abs() < 0.1,
                "height {height} vs {}",
                position[2]
            );
        }

        // each edge of a full grid holds one row, sorted along the edge
        for (edge, along) in decoded
            .edges
            .iter()
            .zip([&decoded.v, &decoded.u, &decoded.v, &decoded.u])
        {
            assert_eq!(edge.len(), grid as usize);
            assert!(
                edge.windows(2)
                    .all(|w| along[w[0] as usize] < along[w[1] as usize])
            );
        }
    }
}
