//! Global terrain from open DEMs — Copernicus/SRTM stitching.
//!
//! Enables planetary-scale terrain tiling from freely available elevation data.
//! Supports SRTM (30m/90m), Copernicus DEM (30m), and ASTER GDEM.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Supported global DEM sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DemSource {
    /// SRTM 1-arc-second (30m) — 60°S to 60°N.
    Srtm30,
    /// SRTM 3-arc-second (90m) — 60°S to 60°N.
    Srtm90,
    /// Copernicus DEM 30m — global coverage.
    Copernicus30,
    /// ASTER GDEM v3 — global coverage.
    AsterGdem,
}

impl DemSource {
    /// Resolution in arc-seconds.
    pub fn resolution_arcsec(&self) -> f64 {
        match self {
            Self::Srtm30 => 1.0,
            Self::Srtm90 => 3.0,
            Self::Copernicus30 => 1.0,
            Self::AsterGdem => 1.0,
        }
    }

    /// Approximate resolution in meters at equator.
    pub fn resolution_meters(&self) -> f64 {
        self.resolution_arcsec() * 30.87 // 1 arcsec ≈ 30.87m at equator
    }

    /// Tile naming convention for this source.
    pub fn tile_name(&self, lat: i32, lon: i32) -> String {
        let ns = if lat >= 0 { "N" } else { "S" };
        let ew = if lon >= 0 { "E" } else { "W" };
        match self {
            Self::Srtm30 | Self::Srtm90 => {
                format!(
                    "{}{:02}{}_{}{:03}{}",
                    ns,
                    lat.unsigned_abs(),
                    ew,
                    ns,
                    lon.unsigned_abs(),
                    ew
                )
            }
            Self::Copernicus30 => {
                format!(
                    "Copernicus_DSM_COG_10_{}{:02}_00_{}{:03}_00_DEM",
                    ns,
                    lat.unsigned_abs(),
                    ew,
                    lon.unsigned_abs()
                )
            }
            Self::AsterGdem => {
                format!(
                    "ASTGTMV003_{}{:02}{}{:03}",
                    ns,
                    lat.unsigned_abs(),
                    ew,
                    lon.unsigned_abs()
                )
            }
        }
    }

    /// Coverage bounds [south, north] in degrees latitude.
    pub fn latitude_coverage(&self) -> (f64, f64) {
        match self {
            Self::Srtm30 | Self::Srtm90 => (-60.0, 60.0),
            Self::Copernicus30 | Self::AsterGdem => (-90.0, 90.0),
        }
    }
}

/// A DEM tile (1°×1° cell).
#[derive(Debug, Clone)]
pub struct DemTile {
    /// Southwest corner latitude (integer degrees).
    pub lat: i32,
    /// Southwest corner longitude (integer degrees).
    pub lon: i32,
    /// Elevation data (row-major, south to north).
    pub elevations: Vec<f32>,
    /// Number of samples in each direction.
    pub samples: u32,
    /// No-data value.
    pub nodata: f32,
}

impl DemTile {
    /// Get elevation at a geographic coordinate (bilinear interpolation).
    pub fn sample(&self, lat: f64, lon: f64) -> Option<f32> {
        let local_lat = lat - self.lat as f64;
        let local_lon = lon - self.lon as f64;

        if !(0.0..=1.0).contains(&local_lat) || !(0.0..=1.0).contains(&local_lon) {
            return None;
        }

        let fx = local_lon * (self.samples - 1) as f64;
        let fy = local_lat * (self.samples - 1) as f64;
        let x0 = fx as u32;
        let y0 = fy as u32;
        let x1 = (x0 + 1).min(self.samples - 1);
        let y1 = (y0 + 1).min(self.samples - 1);
        let dx = fx - x0 as f64;
        let dy = fy - y0 as f64;

        let v00 = self.elevations[(y0 * self.samples + x0) as usize];
        let v10 = self.elevations[(y0 * self.samples + x1) as usize];
        let v01 = self.elevations[(y1 * self.samples + x0) as usize];
        let v11 = self.elevations[(y1 * self.samples + x1) as usize];

        // Skip nodata
        if v00 == self.nodata || v10 == self.nodata || v01 == self.nodata || v11 == self.nodata {
            return None;
        }

        let val = v00 as f64 * (1.0 - dx) * (1.0 - dy)
            + v10 as f64 * dx * (1.0 - dy)
            + v01 as f64 * (1.0 - dx) * dy
            + v11 as f64 * dx * dy;

        Some(val as f32)
    }
}

/// Configuration for global terrain generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalTerrainConfig {
    /// DEM source to use.
    pub source: DemSource,
    /// Bounding box to generate terrain for [west, south, east, north] in degrees.
    pub bounds: [f64; 4],
    /// Maximum zoom level (terrain LOD depth).
    pub max_zoom: u32,
    /// Whether to fill voids (e.g., SRTM voids from radar shadow).
    pub fill_voids: bool,
    /// Whether to apply ocean mask (set ocean areas to 0).
    pub apply_ocean_mask: bool,
}

/// A terrain tile at a specific zoom/x/y (TMS scheme).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerrainTileCoord {
    pub zoom: u32,
    pub x: u32,
    pub y: u32,
}

impl TerrainTileCoord {
    /// Get geographic bounds of this tile.
    pub fn bounds(&self) -> [f64; 4] {
        let n = 2u32.pow(self.zoom) as f64;
        let west = self.x as f64 / n * 360.0 - 180.0;
        let east = (self.x + 1) as f64 / n * 360.0 - 180.0;
        let south = tile_y_to_lat((self.y + 1) as f64 / n);
        let north = tile_y_to_lat(self.y as f64 / n);
        [west, south, east, north]
    }

    /// List all tiles at a given zoom level within bounds.
    pub fn tiles_in_bounds(zoom: u32, bounds: [f64; 4]) -> Vec<Self> {
        let n = 2u32.pow(zoom);
        let x_min = ((bounds[0] + 180.0) / 360.0 * n as f64).floor() as u32;
        let x_max = ((bounds[2] + 180.0) / 360.0 * n as f64).ceil() as u32;
        let y_min = (lat_to_tile_y(bounds[3]) * n as f64).floor() as u32;
        let y_max = (lat_to_tile_y(bounds[1]) * n as f64).ceil() as u32;

        let mut tiles = Vec::new();
        for x in x_min..x_max.min(n) {
            for y in y_min..y_max.min(n) {
                tiles.push(Self { zoom, x, y });
            }
        }
        tiles
    }
}

/// Generate a terrain mesh for a specific tile from DEM data.
pub fn generate_terrain_tile(
    coord: &TerrainTileCoord,
    dem_tiles: &[DemTile],
    grid_size: u32,
) -> TerrainMesh {
    let bounds = coord.bounds();
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    let dx = (bounds[2] - bounds[0]) / (grid_size - 1) as f64;
    let dy = (bounds[3] - bounds[1]) / (grid_size - 1) as f64;

    for row in 0..grid_size {
        for col in 0..grid_size {
            let lon = bounds[0] + col as f64 * dx;
            let lat = bounds[1] + row as f64 * dy;

            let elevation = sample_dem_tiles(dem_tiles, lat, lon).unwrap_or(0.0);
            vertices.push([lon, lat, elevation as f64]);
        }
    }

    // Generate triangle indices
    for row in 0..(grid_size - 1) {
        for col in 0..(grid_size - 1) {
            let tl = row * grid_size + col;
            let tr = tl + 1;
            let bl = tl + grid_size;
            let br = bl + 1;
            indices.push([tl, bl, tr]);
            indices.push([tr, bl, br]);
        }
    }

    TerrainMesh {
        vertices,
        indices,
        bounds,
        zoom: coord.zoom,
    }
}

/// A generated terrain mesh.
#[derive(Debug, Clone)]
pub struct TerrainMesh {
    pub vertices: Vec<[f64; 3]>, // [lon, lat, elevation]
    pub indices: Vec<[u32; 3]>,
    pub bounds: [f64; 4],
    pub zoom: u32,
}

/// Determine which DEM tile files are needed for a given bounds.
pub fn required_dem_tiles(bounds: [f64; 4]) -> Vec<(i32, i32)> {
    let lon_min = bounds[0].floor() as i32;
    let lon_max = bounds[2].ceil() as i32;
    let lat_min = bounds[1].floor() as i32;
    let lat_max = bounds[3].ceil() as i32;

    let mut tiles = Vec::new();
    for lat in lat_min..lat_max {
        for lon in lon_min..lon_max {
            tiles.push((lat, lon));
        }
    }
    tiles
}

/// Check if DEM tiles exist on disk for the given source and bounds.
pub fn check_dem_availability(
    data_dir: &Path,
    source: DemSource,
    bounds: [f64; 4],
) -> Vec<(i32, i32, bool)> {
    let tiles = required_dem_tiles(bounds);
    tiles
        .into_iter()
        .map(|(lat, lon)| {
            let name = source.tile_name(lat, lon);
            let path = data_dir.join(format!("{}.tif", name));
            (lat, lon, path.exists())
        })
        .collect()
}

// --- Internal helpers ---

fn sample_dem_tiles(tiles: &[DemTile], lat: f64, lon: f64) -> Option<f32> {
    for tile in tiles {
        if let Some(v) = tile.sample(lat, lon) {
            return Some(v);
        }
    }
    None
}

fn lat_to_tile_y(lat: f64) -> f64 {
    let lat_rad = lat.to_radians();
    (1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0
}

fn tile_y_to_lat(y: f64) -> f64 {
    let n = std::f64::consts::PI * (1.0 - 2.0 * y);
    n.sinh().atan().to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_dem_tile(lat: i32, lon: i32, elevation: f32) -> DemTile {
        let samples = 10;
        DemTile {
            lat,
            lon,
            elevations: vec![elevation; (samples * samples) as usize],
            samples,
            nodata: -9999.0,
        }
    }

    #[test]
    fn test_dem_tile_sample() {
        let tile = flat_dem_tile(45, 10, 500.0);
        let val = tile.sample(45.5, 10.5).unwrap();
        assert!((val - 500.0).abs() < 0.01);
    }

    #[test]
    fn test_dem_tile_out_of_bounds() {
        let tile = flat_dem_tile(45, 10, 500.0);
        assert!(tile.sample(44.0, 10.5).is_none()); // below tile
        assert!(tile.sample(46.5, 10.5).is_none()); // above tile
    }

    #[test]
    fn test_required_dem_tiles() {
        let tiles = required_dem_tiles([10.0, 45.0, 12.5, 47.3]);
        assert!(tiles.contains(&(45, 10)));
        assert!(tiles.contains(&(46, 11)));
        assert_eq!(tiles.len(), 3 * 3); // 3 lat × 3 lon
    }

    #[test]
    fn test_terrain_tile_bounds() {
        let coord = TerrainTileCoord {
            zoom: 1,
            x: 0,
            y: 0,
        };
        let bounds = coord.bounds();
        // At zoom 1, tile (0,0) covers western hemisphere, northern half
        assert!((bounds[0] - (-180.0)).abs() < 0.01);
        assert!((bounds[2] - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_generate_terrain_tile() {
        let dem = flat_dem_tile(45, 10, 300.0);
        let coord = TerrainTileCoord {
            zoom: 5,
            x: 17,
            y: 11,
        };
        let bounds = coord.bounds();
        // Only generate if our DEM covers this tile
        let mesh = generate_terrain_tile(&coord, &[dem], 4);
        assert_eq!(mesh.vertices.len(), 16); // 4x4 grid
        assert_eq!(mesh.indices.len(), 18); // 3x3 quads × 2 triangles
        assert_eq!(mesh.bounds, bounds);
    }

    #[test]
    fn test_dem_source_properties() {
        assert_eq!(DemSource::Srtm30.resolution_arcsec(), 1.0);
        assert_eq!(DemSource::Srtm90.resolution_arcsec(), 3.0);
        assert!((DemSource::Srtm30.resolution_meters() - 30.87).abs() < 0.1);
    }

    #[test]
    fn test_tiles_in_bounds() {
        let tiles = TerrainTileCoord::tiles_in_bounds(2, [-10.0, 40.0, 10.0, 50.0]);
        assert!(!tiles.is_empty());
        for t in &tiles {
            assert_eq!(t.zoom, 2);
        }
    }
}
