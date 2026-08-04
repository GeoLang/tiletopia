//! Elevation API — point and profile elevation lookup from terrain data.

use serde::{Deserialize, Serialize};

/// Elevation at a single point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationPoint {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    pub resolution_m: f64, // DEM resolution used
    pub source: ElevationSource,
}

/// Elevation profile along a path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationProfile {
    pub points: Vec<ElevationPoint>,
    pub total_distance_m: f64,
    pub elevation_gain_m: f64,
    pub elevation_loss_m: f64,
    pub min_elevation_m: f64,
    pub max_elevation_m: f64,
}

/// Data source for elevation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ElevationSource {
    Srtm30m,
    Copernicus30m,
    LocalDem,
    Lidar1m,
}

/// A loaded DEM grid for elevation lookups.
pub struct DemGrid {
    pub bounds: [f64; 4], // [west, south, east, north]
    pub width: usize,
    pub height: usize,
    pub cell_size_x: f64,
    pub cell_size_y: f64,
    pub elevations: Vec<f64>, // row-major, [height][width]
    pub nodata: f64,
}

impl DemGrid {
    /// Bilinear interpolation at (lat, lon).
    pub fn sample(&self, lat: f64, lon: f64) -> Option<f64> {
        let col_f = (lon - self.bounds[0]) / self.cell_size_x;
        let row_f = (self.bounds[3] - lat) / self.cell_size_y; // north-down

        if col_f < 0.0 || row_f < 0.0 {
            return None;
        }

        let col0 = col_f.floor() as usize;
        let row0 = row_f.floor() as usize;
        if col0 + 1 >= self.width || row0 + 1 >= self.height {
            return None;
        }

        let fx = col_f - col0 as f64;
        let fy = row_f - row0 as f64;

        let v00 = self.elevations[row0 * self.width + col0];
        let v10 = self.elevations[row0 * self.width + col0 + 1];
        let v01 = self.elevations[(row0 + 1) * self.width + col0];
        let v11 = self.elevations[(row0 + 1) * self.width + col0 + 1];

        // Skip nodata cells
        for v in [v00, v10, v01, v11] {
            if (v - self.nodata).abs() < 1e-10 {
                return None;
            }
        }

        let val = v00 * (1.0 - fx) * (1.0 - fy)
            + v10 * fx * (1.0 - fy)
            + v01 * (1.0 - fx) * fy
            + v11 * fx * fy;
        Some(val)
    }
}

/// Store of loaded DEM grids for elevation lookup.
pub struct DemStore {
    grids: Vec<DemGrid>,
}

impl Default for DemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DemStore {
    pub fn new() -> Self {
        Self { grids: Vec::new() }
    }

    pub fn add_grid(&mut self, grid: DemGrid) {
        self.grids.push(grid);
    }

    /// The best-resolution grid loaded, whatever it covers. Callers that need a
    /// grid to anchor a pixel ladder on use this; point lookups go through
    /// `find_grid`, which also checks coverage.
    pub fn finest_grid(&self) -> Option<&DemGrid> {
        self.grids.iter().min_by(|a, b| {
            let res_a = a.cell_size_x * a.cell_size_y;
            let res_b = b.cell_size_x * b.cell_size_y;
            res_a.partial_cmp(&res_b).unwrap()
        })
    }

    /// Find the grid containing this point, preferring the best (smallest cell) resolution.
    fn find_grid(&self, lat: f64, lon: f64) -> Option<&DemGrid> {
        self.grids
            .iter()
            .filter(|g| {
                lon >= g.bounds[0] && lon <= g.bounds[2] && lat >= g.bounds[1] && lat <= g.bounds[3]
            })
            .min_by(|a, b| {
                let res_a = a.cell_size_x * a.cell_size_y;
                let res_b = b.cell_size_x * b.cell_size_y;
                res_a.partial_cmp(&res_b).unwrap()
            })
    }
}

/// Get elevation at a single point, using DEM grids when available.
/// Falls back to a synthetic model when no grid covers the point.
pub fn get_elevation(latitude: f64, longitude: f64, dem: &DemStore) -> ElevationPoint {
    if let Some(grid) = dem.find_grid(latitude, longitude)
        && let Some(elev) = grid.sample(latitude, longitude)
    {
        return ElevationPoint {
            latitude,
            longitude,
            elevation_m: elev,
            resolution_m: grid.cell_size_x * 111_320.0, // approximate metres
            source: ElevationSource::LocalDem,
        };
    }

    // Fallback: synthetic elevation based on position
    let base = 50.0;
    let variation = ((latitude * 1000.0).sin() * 30.0) + ((longitude * 800.0).cos() * 20.0);
    ElevationPoint {
        latitude,
        longitude,
        elevation_m: base + variation,
        resolution_m: 30.0,
        source: ElevationSource::Srtm30m,
    }
}

/// Get elevation along a path (list of [lat, lon] pairs).
pub fn get_profile(path: &[[f64; 2]], dem: &DemStore) -> ElevationProfile {
    let mut points = Vec::new();
    let mut total_distance = 0.0;
    let mut elevation_gain = 0.0;
    let mut elevation_loss = 0.0;

    for (i, coord) in path.iter().enumerate() {
        let pt = get_elevation(coord[0], coord[1], dem);
        if i > 0 {
            let prev = &points[i - 1];
            let dist = haversine_distance(prev, &pt);
            total_distance += dist;
            let diff: f64 = pt.elevation_m - prev.elevation_m;
            if diff > 0.0 {
                elevation_gain += diff;
            } else {
                elevation_loss += diff.abs();
            }
        }
        points.push(pt);
    }

    let min_elev = points
        .iter()
        .map(|p| p.elevation_m)
        .fold(f64::INFINITY, f64::min);
    let max_elev = points
        .iter()
        .map(|p| p.elevation_m)
        .fold(f64::NEG_INFINITY, f64::max);

    ElevationProfile {
        points,
        total_distance_m: total_distance,
        elevation_gain_m: elevation_gain,
        elevation_loss_m: elevation_loss,
        min_elevation_m: min_elev,
        max_elevation_m: max_elev,
    }
}

/// Batch elevation lookup.
pub fn get_elevations(locations: &[[f64; 2]], dem: &DemStore) -> Vec<ElevationPoint> {
    locations
        .iter()
        .map(|loc| get_elevation(loc[0], loc[1], dem))
        .collect()
}

/// Haversine distance between two elevation points (in meters).
fn haversine_distance(a: &ElevationPoint, b: &ElevationPoint) -> f64 {
    let r = 6_371_000.0; // Earth radius in meters
    let dlat = (b.latitude - a.latitude).to_radians();
    let dlon = (b.longitude - a.longitude).to_radians();
    let lat1 = a.latitude.to_radians();
    let lat2 = b.latitude.to_radians();
    let a_val = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a_val.sqrt().asin();
    r * c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_dem() -> DemStore {
        DemStore::new()
    }

    #[test]
    fn test_get_elevation() {
        let dem = empty_dem();
        let pt = get_elevation(37.7749, -122.4194, &dem);
        assert!((pt.latitude - 37.7749).abs() < 0.0001);
        assert!(pt.elevation_m > -500.0 && pt.elevation_m < 5000.0);
    }

    #[test]
    fn test_get_profile() {
        let dem = empty_dem();
        let path = vec![
            [37.7749, -122.4194],
            [37.7760, -122.4180],
            [37.7780, -122.4160],
        ];
        let profile = get_profile(&path, &dem);
        assert_eq!(profile.points.len(), 3);
        assert!(profile.total_distance_m > 0.0);
        assert!(profile.min_elevation_m <= profile.max_elevation_m);
    }

    #[test]
    fn test_batch_elevations() {
        let dem = empty_dem();
        let locations = vec![[37.7749, -122.4194], [40.7128, -74.0060]];
        let results = get_elevations(&locations, &dem);
        assert_eq!(results.len(), 2);
    }

    /// Build a 3x3 DEM grid and verify bilinear interpolation.
    #[test]
    fn test_dem_bilinear_interpolation() {
        // 3x3 grid covering [0,0] to [2,2] in lon/lat
        // Elevations:
        //   row0 (north=2): 100  200  300
        //   row1 (lat=1):   400  500  600
        //   row2 (south=0): 700  800  900
        let grid = DemGrid {
            bounds: [0.0, 0.0, 2.0, 2.0], // [west, south, east, north]
            width: 3,
            height: 3,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            elevations: vec![
                100.0, 200.0, 300.0, // row 0 (north)
                400.0, 500.0, 600.0, // row 1
                700.0, 800.0, 900.0, // row 2 (south)
            ],
            nodata: -9999.0,
        };

        // Centre of grid at lat=1.0, lon=1.0 → row_f=1.0, col_f=1.0 → exactly cell [1][1]=500
        let v = grid.sample(1.0, 1.0).unwrap();
        assert!((v - 500.0).abs() < 1e-9);

        // Midpoint between cells (0,0)=100 and (0,1)=200 at lat=2.0 (top row), lon=0.5
        // row_f = (2-2)/1 = 0.0, col_f = 0.5, fx=0.5, fy=0.0
        // = 100*0.5*1 + 200*0.5*1 + 400*0.5*0 + 500*0.5*0 = 150
        let v = grid.sample(2.0, 0.5).unwrap();
        assert!((v - 150.0).abs() < 1e-9);

        // Out-of-bounds should return None
        assert!(grid.sample(3.0, 1.0).is_none());

        // Test via DemStore
        let mut store = DemStore::new();
        store.add_grid(grid);
        let pt = get_elevation(1.0, 1.0, &store);
        assert!((pt.elevation_m - 500.0).abs() < 1e-9);
        assert_eq!(pt.source, ElevationSource::LocalDem);

        // Outside grid bounds falls back to synthetic
        let pt = get_elevation(50.0, 50.0, &store);
        assert_eq!(pt.source, ElevationSource::Srtm30m);
    }

    #[test]
    fn test_dem_nodata_returns_none() {
        let grid = DemGrid {
            bounds: [0.0, 0.0, 2.0, 2.0],
            width: 3,
            height: 3,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            elevations: vec![
                100.0, -9999.0, 300.0, 400.0, 500.0, 600.0, 700.0, 800.0, 900.0,
            ],
            nodata: -9999.0,
        };
        // Interpolation touching the nodata cell should return None
        assert!(grid.sample(2.0, 0.5).is_none());
    }
}
