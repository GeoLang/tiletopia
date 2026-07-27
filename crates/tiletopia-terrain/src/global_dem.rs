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
///
/// Built only through [`DemTile::new`], so [`DemTile::sample`] can index the
/// grid without checking it.
#[derive(Debug, Clone)]
pub struct DemTile {
    /// Southwest corner latitude (integer degrees).
    lat: i32,
    /// Southwest corner longitude (integer degrees).
    lon: i32,
    /// Elevation data (row-major, south to north).
    elevations: Vec<f32>,
    /// Number of samples in each direction.
    samples: u32,
    /// No-data value.
    nodata: f32,
}

impl DemTile {
    /// Build a tile, or `None` when the grid cannot be sampled: bilinear
    /// interpolation needs at least 2 samples per axis and exactly `samples²`
    /// elevations. A DEM file read while it was still being written arrives
    /// here as a 0×0 grid.
    pub fn new(
        lat: i32,
        lon: i32,
        elevations: Vec<f32>,
        samples: u32,
        nodata: f32,
    ) -> Option<Self> {
        if samples < 2 || elevations.len() != (samples as usize).pow(2) {
            return None;
        }
        Some(Self {
            lat,
            lon,
            elevations,
            samples,
            nodata,
        })
    }

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

/// A terrain tile in the geographic (EPSG:4326) TMS scheme Cesium's
/// quantized-mesh terrain uses: two square tiles at zoom 0, y counting north
/// from the south pole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerrainTileCoord {
    pub zoom: u32,
    pub x: u32,
    pub y: u32,
}

impl TerrainTileCoord {
    /// Tile counts on each axis at a zoom level: (x, y).
    pub fn grid_at_zoom(zoom: u32) -> (u32, u32) {
        let y_tiles = 1u32.checked_shl(zoom).unwrap_or(u32::MAX);
        (y_tiles.saturating_mul(2), y_tiles)
    }

    /// Get geographic bounds of this tile.
    pub fn bounds(&self) -> [f64; 4] {
        let span = 180.0 / 2f64.powi(self.zoom as i32);
        let west = self.x as f64 * span - 180.0;
        let south = self.y as f64 * span - 90.0;
        [west, south, west + span, south + span]
    }

    /// List all tiles at a given zoom level within bounds.
    pub fn tiles_in_bounds(zoom: u32, bounds: [f64; 4]) -> Vec<Self> {
        let (x_tiles, y_tiles) = Self::grid_at_zoom(zoom);
        let span = 180.0 / 2f64.powi(zoom as i32);
        let x_min = ((bounds[0] + 180.0) / span).floor().max(0.0) as u32;
        let x_max = ((bounds[2] + 180.0) / span).ceil() as u32;
        let y_min = ((bounds[1] + 90.0) / span).floor().max(0.0) as u32;
        let y_max = ((bounds[3] + 90.0) / span).ceil() as u32;

        let mut tiles = Vec::new();
        for x in x_min..x_max.min(x_tiles) {
            for y in y_min..y_max.min(y_tiles) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_dem_tile(lat: i32, lon: i32, elevation: f32) -> DemTile {
        let samples = 10;
        DemTile::new(
            lat,
            lon,
            vec![elevation; (samples * samples) as usize],
            samples,
            -9999.0,
        )
        .unwrap()
    }

    #[test]
    fn dem_tile_rejects_grids_it_cannot_sample() {
        // the live crash: a DEM file read mid-write yields a 0×0 grid, and
        // sampling it underflowed `samples - 1` into a wild index
        assert!(DemTile::new(43, 7, vec![], 0, -9999.0).is_none());
        assert!(DemTile::new(43, 7, vec![1.0], 1, -9999.0).is_none());
        // elevation count must match the declared grid
        assert!(DemTile::new(43, 7, vec![1.0; 3], 2, -9999.0).is_none());
        assert!(DemTile::new(43, 7, vec![1.0; 4], 2, -9999.0).is_some());
    }

    #[test]
    fn every_in_tile_coordinate_samples_in_bounds() {
        // the invariant DemTile::new buys: no coordinate inside the cell can
        // index past the grid, for the smallest grid there is
        let tile = DemTile::new(43, 7, vec![10.0, 20.0, 30.0, 40.0], 2, -9999.0).unwrap();
        for step in 0..=100 {
            let t = step as f64 / 100.0;
            assert!(tile.sample(43.0 + t, 7.0 + t).is_some());
            assert!(tile.sample(43.0 + t, 8.0 - t).is_some());
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
        // zoom 0 is two square tiles covering the whole globe
        let west_root = TerrainTileCoord {
            zoom: 0,
            x: 0,
            y: 0,
        };
        assert_eq!(west_root.bounds(), [-180.0, -90.0, 0.0, 90.0]);
        let east_root = TerrainTileCoord {
            zoom: 0,
            x: 1,
            y: 0,
        };
        assert_eq!(east_root.bounds(), [0.0, -90.0, 180.0, 90.0]);

        // y counts north from the south pole
        let coord = TerrainTileCoord {
            zoom: 1,
            x: 0,
            y: 0,
        };
        assert_eq!(coord.bounds(), [-180.0, -90.0, -90.0, 0.0]);
    }

    #[test]
    fn test_grid_at_zoom() {
        assert_eq!(TerrainTileCoord::grid_at_zoom(0), (2, 1));
        assert_eq!(TerrainTileCoord::grid_at_zoom(3), (16, 8));
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
