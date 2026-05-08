//! Terrain Analysis — slope, aspect, hillshade, viewshed, watershed.
//!
//! Raster-based terrain operations on DEM data for site analysis,
//! visibility studies, and hydrological modeling.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A terrain analysis result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerrainAnalysisResult {
    pub id: Uuid,
    pub analysis_type: AnalysisType,
    pub input_dem: String,
    pub bounds: [f64; 4],
    pub resolution_m: f64,
    pub statistics: AnalysisStats,
}

/// Type of terrain analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AnalysisType {
    Slope,
    Aspect,
    Hillshade,
    Viewshed,
    Watershed,
    ContourLines,
    CutFill,
    FlowDirection,
    FlowAccumulation,
}

/// Analysis statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisStats {
    pub min_value: f64,
    pub max_value: f64,
    pub mean_value: f64,
    pub std_dev: f64,
    pub cell_count: u64,
    pub nodata_count: u64,
}

/// Slope analysis parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlopeParams {
    pub output_unit: SlopeUnit,
    pub method: SlopeMethod,
}

/// Slope output units.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SlopeUnit {
    Degrees,
    Percent,
    Radians,
}

/// Slope calculation method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SlopeMethod {
    Horn,       // 3x3 Horn's method
    ZevenbergenThorne, // smoother for gentle terrain
}

/// Hillshade parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HillshadeParams {
    pub azimuth_deg: f64,  // sun azimuth (315 = NW default)
    pub altitude_deg: f64, // sun altitude (45 default)
    pub z_factor: f64,     // vertical exaggeration
}

impl Default for HillshadeParams {
    fn default() -> Self {
        Self {
            azimuth_deg: 315.0,
            altitude_deg: 45.0,
            z_factor: 1.0,
        }
    }
}

/// Viewshed parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewshedParams {
    pub observer_position: [f64; 2], // [lon, lat]
    pub observer_height_m: f64,
    pub target_height_m: f64,
    pub max_radius_m: f64,
}

/// Viewshed result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewshedResult {
    pub observer: [f64; 2],
    pub visible_area_m2: f64,
    pub visible_percentage: f64,
    pub max_visible_distance_m: f64,
    pub total_analyzed_cells: u64,
    pub visible_cells: u64,
}

/// Contour line parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContourParams {
    pub interval_m: f64,     // contour interval
    pub base_contour_m: f64, // reference elevation
    pub smooth_factor: f64,  // 0 = no smoothing
}

/// A contour line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContourLine {
    pub elevation_m: f64,
    pub is_index: bool, // index contours (every 5th typically)
    pub vertices: Vec<[f64; 2]>,
    pub length_m: f64,
}

/// Compute slope for a DEM grid (simplified demo).
pub fn compute_slope(grid: &[Vec<f64>], cell_size_m: f64, params: &SlopeParams) -> TerrainAnalysisResult {
    let rows = grid.len();
    let cols = if rows > 0 { grid[0].len() } else { 0 };
    let mut slopes = Vec::new();

    for i in 1..rows.saturating_sub(1) {
        for j in 1..cols.saturating_sub(1) {
            let dz_dx = (grid[i][j + 1] - grid[i][j - 1]) / (2.0 * cell_size_m);
            let dz_dy = (grid[i + 1][j] - grid[i - 1][j]) / (2.0 * cell_size_m);
            let slope_rad = (dz_dx * dz_dx + dz_dy * dz_dy).sqrt().atan();
            let slope_val = match params.output_unit {
                SlopeUnit::Degrees => slope_rad.to_degrees(),
                SlopeUnit::Percent => slope_rad.tan() * 100.0,
                SlopeUnit::Radians => slope_rad,
            };
            slopes.push(slope_val);
        }
    }

    let mean = if slopes.is_empty() { 0.0 } else { slopes.iter().sum::<f64>() / slopes.len() as f64 };
    let min = slopes.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = slopes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let variance = slopes.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / slopes.len().max(1) as f64;

    TerrainAnalysisResult {
        id: Uuid::new_v4(),
        analysis_type: AnalysisType::Slope,
        input_dem: "input.tif".into(),
        bounds: [-122.5, 37.7, -122.3, 37.9],
        resolution_m: cell_size_m,
        statistics: AnalysisStats {
            min_value: min,
            max_value: max,
            mean_value: mean,
            std_dev: variance.sqrt(),
            cell_count: slopes.len() as u64,
            nodata_count: 0,
        },
    }
}

/// Compute hillshade value for a single cell.
pub fn hillshade_cell(dz_dx: f64, dz_dy: f64, params: &HillshadeParams) -> u8 {
    let azimuth_rad = (360.0 - params.azimuth_deg + 90.0).to_radians();
    let altitude_rad = params.altitude_deg.to_radians();
    let slope = (params.z_factor * (dz_dx * dz_dx + dz_dy * dz_dy).sqrt()).atan();
    let aspect = dz_dy.atan2(-dz_dx);

    let hs = 255.0
        * (altitude_rad.cos() * slope.cos()
            + altitude_rad.sin() * slope.sin() * (azimuth_rad - aspect).cos());
    hs.clamp(0.0, 255.0) as u8
}

/// Compute viewshed from an observer point (simplified).
#[allow(clippy::needless_range_loop)]
pub fn compute_viewshed(dem_grid: &[Vec<f64>], cell_size_m: f64, params: &ViewshedParams) -> ViewshedResult {
    let rows = dem_grid.len();
    let cols = if rows > 0 { dem_grid[0].len() } else { 0 };
    let total_cells = (rows * cols) as u64;

    // Simplified: count cells within radius that have line-of-sight
    let radius_cells = (params.max_radius_m / cell_size_m) as usize;
    let center_row = rows / 2;
    let center_col = cols / 2;
    let observer_elev = if center_row < rows && center_col < cols {
        dem_grid[center_row][center_col] + params.observer_height_m
    } else {
        params.observer_height_m
    };

    let mut visible_cells = 0u64;
    let mut max_dist = 0.0f64;

    for i in center_row.saturating_sub(radius_cells)..=(center_row + radius_cells).min(rows - 1) {
        for j in center_col.saturating_sub(radius_cells)..=(center_col + radius_cells).min(cols - 1) {
            let di = i as f64 - center_row as f64;
            let dj = j as f64 - center_col as f64;
            let dist = (di * di + dj * dj).sqrt() * cell_size_m;
            if dist <= params.max_radius_m && dist > 0.0 {
                let target_elev = dem_grid[i][j] + params.target_height_m;
                let angle = (target_elev - observer_elev) / dist;
                // Simplified LOS: visible if angle > -0.1
                if angle > -0.1 {
                    visible_cells += 1;
                    max_dist = max_dist.max(dist);
                }
            }
        }
    }

    let analyzed = total_cells.min((radius_cells as u64 * 2 + 1).pow(2));
    ViewshedResult {
        observer: params.observer_position,
        visible_area_m2: visible_cells as f64 * cell_size_m * cell_size_m,
        visible_percentage: if analyzed > 0 { visible_cells as f64 / analyzed as f64 * 100.0 } else { 0.0 },
        max_visible_distance_m: max_dist,
        total_analyzed_cells: analyzed,
        visible_cells,
    }
}

/// Generate contour lines from a DEM grid (simplified).
#[allow(clippy::needless_range_loop)]
pub fn generate_contours(dem_grid: &[Vec<f64>], cell_size_m: f64, params: &ContourParams) -> Vec<ContourLine> {
    let rows = dem_grid.len();
    let cols = if rows > 0 { dem_grid[0].len() } else { 0 };

    let min_elev = dem_grid.iter().flat_map(|row| row.iter()).cloned().fold(f64::INFINITY, f64::min);
    let max_elev = dem_grid.iter().flat_map(|row| row.iter()).cloned().fold(f64::NEG_INFINITY, f64::max);

    let mut contours = Vec::new();
    let mut elev = (min_elev / params.interval_m).ceil() * params.interval_m;

    while elev <= max_elev {
        let index_interval = params.interval_m * 5.0;
        let is_index = (elev / index_interval).fract().abs() < 0.01;

        // Simplified: just mark where the contour crosses
        let mut vertices = Vec::new();
        for i in 0..rows.saturating_sub(1) {
            for j in 0..cols.saturating_sub(1) {
                let z = dem_grid[i][j];
                let z_right = dem_grid[i][j + 1];
                if (z <= elev && z_right > elev) || (z > elev && z_right <= elev) {
                    let t = (elev - z) / (z_right - z);
                    vertices.push([j as f64 + t, i as f64]);
                }
            }
        }

        if !vertices.is_empty() {
            let length_m = vertices.len() as f64 * cell_size_m;
            contours.push(ContourLine { elevation_m: elev, is_index, vertices, length_m });
        }

        elev += params.interval_m;
    }

    contours
}

/// List available terrain analysis operations.
pub fn available_analyses() -> Vec<&'static str> {
    vec![
        "Slope", "Aspect", "Hillshade", "Viewshed", "Watershed",
        "ContourLines", "CutFill", "FlowDirection", "FlowAccumulation",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_slope() {
        // Simple tilted plane: z = x * 0.5
        let grid = vec![
            vec![0.0, 5.0, 10.0, 15.0, 20.0],
            vec![0.0, 5.0, 10.0, 15.0, 20.0],
            vec![0.0, 5.0, 10.0, 15.0, 20.0],
            vec![0.0, 5.0, 10.0, 15.0, 20.0],
            vec![0.0, 5.0, 10.0, 15.0, 20.0],
        ];
        let params = SlopeParams { output_unit: SlopeUnit::Degrees, method: SlopeMethod::Horn };
        let result = compute_slope(&grid, 10.0, &params);
        assert!(result.statistics.mean_value > 0.0);
        assert_eq!(result.analysis_type, AnalysisType::Slope);
    }

    #[test]
    fn test_hillshade_cell() {
        let params = HillshadeParams::default();
        let hs = hillshade_cell(0.0, 0.0, &params); // flat surface
        assert!(hs > 100); // should be bright (lit surface)
    }

    #[test]
    fn test_viewshed() {
        let grid = vec![
            vec![100.0, 100.0, 100.0, 100.0, 100.0],
            vec![100.0, 100.0, 100.0, 100.0, 100.0],
            vec![100.0, 100.0, 100.0, 100.0, 100.0],
            vec![100.0, 100.0, 100.0, 100.0, 100.0],
            vec![100.0, 100.0, 100.0, 100.0, 100.0],
        ];
        let params = ViewshedParams {
            observer_position: [0.0, 0.0],
            observer_height_m: 1.7,
            target_height_m: 0.0,
            max_radius_m: 100.0,
        };
        let result = compute_viewshed(&grid, 10.0, &params);
        assert!(result.visible_cells > 0);
        assert!(result.visible_percentage > 0.0);
    }

    #[test]
    fn test_generate_contours() {
        let grid = vec![
            vec![0.0, 10.0, 20.0, 30.0],
            vec![5.0, 15.0, 25.0, 35.0],
            vec![10.0, 20.0, 30.0, 40.0],
            vec![15.0, 25.0, 35.0, 45.0],
        ];
        let params = ContourParams { interval_m: 10.0, base_contour_m: 0.0, smooth_factor: 0.0 };
        let contours = generate_contours(&grid, 10.0, &params);
        assert!(!contours.is_empty());
    }

    #[test]
    fn test_available_analyses() {
        assert_eq!(available_analyses().len(), 9);
    }
}
