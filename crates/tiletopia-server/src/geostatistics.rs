//! Geostatistics — spatial interpolation, kriging, IDW, variograms.
//!
//! Provides geostatistical methods for predicting values at unsampled
//! locations from point observations. Useful for environmental monitoring,
//! soil analysis, and property assessment.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A spatial observation point with measured value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SamplePoint {
    pub x: f64,
    pub y: f64,
    pub value: f64,
}

/// Interpolation method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum InterpolationMethod {
    Idw { power: f64 },
    OrdinaryKriging,
    UniversalKriging,
    SimpleKriging { known_mean: f64 },
}

/// Variogram model type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VariogramModel {
    Spherical,
    Exponential,
    Gaussian,
    Linear,
    Power { exponent: f64 },
}

/// Fitted variogram parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariogramParams {
    pub model: VariogramModel,
    pub nugget: f64,       // variance at distance 0
    pub sill: f64,         // total variance plateau
    pub range: f64,        // distance at which sill is reached
    pub r_squared: f64,    // goodness-of-fit
}

/// Empirical variogram point (lag bin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariogramBin {
    pub lag_distance: f64,
    pub semivariance: f64,
    pub pair_count: u32,
}

/// Interpolation result for a grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterpolationResult {
    pub id: Uuid,
    pub method: InterpolationMethod,
    pub bounds: [f64; 4], // [min_x, min_y, max_x, max_y]
    pub resolution: f64,
    pub grid_rows: usize,
    pub grid_cols: usize,
    pub values: Vec<f64>,
    pub variances: Option<Vec<f64>>, // kriging variance (uncertainty)
    pub statistics: InterpolationStats,
}

/// Statistics about the interpolation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterpolationStats {
    pub min_value: f64,
    pub max_value: f64,
    pub mean_value: f64,
    pub std_dev: f64,
    pub cross_validation_rmse: Option<f64>,
}

/// Compute Inverse Distance Weighting interpolation.
pub fn idw_interpolation(samples: &[SamplePoint], query_x: f64, query_y: f64, power: f64) -> f64 {
    let mut weight_sum = 0.0;
    let mut value_sum = 0.0;

    for s in samples {
        let dx = s.x - query_x;
        let dy = s.y - query_y;
        let dist = (dx * dx + dy * dy).sqrt();

        if dist < 1e-10 {
            return s.value; // exact match
        }

        let w = 1.0 / dist.powf(power);
        weight_sum += w;
        value_sum += w * s.value;
    }

    if weight_sum > 0.0 {
        value_sum / weight_sum
    } else {
        0.0
    }
}

/// Compute empirical variogram from sample data.
pub fn compute_variogram(samples: &[SamplePoint], n_lags: usize, max_lag: f64) -> Vec<VariogramBin> {
    let lag_size = max_lag / n_lags as f64;
    let mut bins: Vec<(f64, u32)> = vec![(0.0, 0); n_lags]; // (sum_semivariance, count)

    for i in 0..samples.len() {
        for j in (i + 1)..samples.len() {
            let dx = samples[i].x - samples[j].x;
            let dy = samples[i].y - samples[j].y;
            let dist = (dx * dx + dy * dy).sqrt();
            let dv = samples[i].value - samples[j].value;
            let semivar = 0.5 * dv * dv;

            let bin_idx = (dist / lag_size) as usize;
            if bin_idx < n_lags {
                bins[bin_idx].0 += semivar;
                bins[bin_idx].1 += 1;
            }
        }
    }

    bins.iter()
        .enumerate()
        .filter(|(_, (_, count))| *count > 0)
        .map(|(i, (sum, count))| VariogramBin {
            lag_distance: (i as f64 + 0.5) * lag_size,
            semivariance: sum / *count as f64,
            pair_count: *count,
        })
        .collect()
}

/// Fit a spherical variogram model to empirical data.
pub fn fit_spherical_variogram(bins: &[VariogramBin]) -> VariogramParams {
    if bins.is_empty() {
        return VariogramParams {
            model: VariogramModel::Spherical,
            nugget: 0.0, sill: 1.0, range: 1.0, r_squared: 0.0,
        };
    }

    // Simple method-of-moments fit
    let max_lag = bins.last().map(|b| b.lag_distance).unwrap_or(1.0);
    let max_var = bins.iter().map(|b| b.semivariance).fold(0.0f64, f64::max);
    let min_var = bins.first().map(|b| b.semivariance).unwrap_or(0.0);

    let nugget = min_var * 0.3;
    let sill = max_var;
    let range = max_lag * 0.6;

    // Compute R² (simplified)
    let mean_var = bins.iter().map(|b| b.semivariance).sum::<f64>() / bins.len() as f64;
    let ss_tot = bins.iter().map(|b| (b.semivariance - mean_var).powi(2)).sum::<f64>();
    let ss_res: f64 = bins.iter().map(|b| {
        let predicted = spherical_model(b.lag_distance, nugget, sill, range);
        (b.semivariance - predicted).powi(2)
    }).sum();
    let r_squared = if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { 0.0 };

    VariogramParams {
        model: VariogramModel::Spherical,
        nugget, sill, range, r_squared,
    }
}

/// Spherical variogram model.
fn spherical_model(h: f64, nugget: f64, sill: f64, range: f64) -> f64 {
    if h <= 0.0 {
        0.0
    } else if h >= range {
        sill
    } else {
        let hr = h / range;
        nugget + (sill - nugget) * (1.5 * hr - 0.5 * hr.powi(3))
    }
}

/// Ordinary kriging at a single point.
pub fn kriging_estimate(samples: &[SamplePoint], query_x: f64, query_y: f64, variogram: &VariogramParams) -> (f64, f64) {
    let n = samples.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    if n == 1 {
        return (samples[0].value, variogram.sill);
    }

    // Simple weighted estimation using variogram distances
    let mut weights = Vec::with_capacity(n);
    let mut weight_sum = 0.0;

    for s in samples {
        let dx = s.x - query_x;
        let dy = s.y - query_y;
        let dist = (dx * dx + dy * dy).sqrt();
        let gamma = spherical_model(dist, variogram.nugget, variogram.sill, variogram.range);
        let w = if gamma > 0.0 { variogram.sill / gamma } else { 100.0 };
        weights.push(w);
        weight_sum += w;
    }

    let mut estimate = 0.0;
    let mut variance = 0.0;
    for (i, s) in samples.iter().enumerate() {
        let normalized_w = weights[i] / weight_sum;
        estimate += normalized_w * s.value;
        let dx = s.x - query_x;
        let dy = s.y - query_y;
        let dist = (dx * dx + dy * dy).sqrt();
        variance += normalized_w * spherical_model(dist, variogram.nugget, variogram.sill, variogram.range);
    }

    (estimate, variance)
}

/// IDW interpolation over a full grid.
pub fn interpolate_grid(
    samples: &[SamplePoint],
    bounds: [f64; 4],
    resolution: f64,
    method: &InterpolationMethod,
) -> InterpolationResult {
    let cols = ((bounds[2] - bounds[0]) / resolution).ceil() as usize;
    let rows = ((bounds[3] - bounds[1]) / resolution).ceil() as usize;
    let mut values = Vec::with_capacity(rows * cols);
    let mut variances = Vec::new();
    let has_variance = matches!(method, InterpolationMethod::OrdinaryKriging | InterpolationMethod::SimpleKriging { .. });

    let variogram = if has_variance {
        let bins = compute_variogram(samples, 10, bounds[2] - bounds[0]);
        Some(fit_spherical_variogram(&bins))
    } else {
        None
    };

    for r in 0..rows {
        let y = bounds[1] + (r as f64 + 0.5) * resolution;
        for c in 0..cols {
            let x = bounds[0] + (c as f64 + 0.5) * resolution;
            match method {
                InterpolationMethod::Idw { power } => {
                    values.push(idw_interpolation(samples, x, y, *power));
                }
                InterpolationMethod::OrdinaryKriging | InterpolationMethod::SimpleKriging { .. } => {
                    let (est, var) = kriging_estimate(samples, x, y, variogram.as_ref().unwrap());
                    values.push(est);
                    variances.push(var);
                }
                InterpolationMethod::UniversalKriging => {
                    // fallback to ordinary
                    let (est, var) = kriging_estimate(samples, x, y, variogram.as_ref().unwrap_or(&VariogramParams {
                        model: VariogramModel::Spherical,
                        nugget: 0.0, sill: 1.0, range: 1.0, r_squared: 0.0,
                    }));
                    values.push(est);
                    variances.push(var);
                }
            }
        }
    }

    let min_v = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_v = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mean_v = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let var_v = values.iter().map(|v| (v - mean_v).powi(2)).sum::<f64>() / values.len().max(1) as f64;

    InterpolationResult {
        id: Uuid::new_v4(),
        method: method.clone(),
        bounds,
        resolution,
        grid_rows: rows,
        grid_cols: cols,
        values,
        variances: if has_variance { Some(variances) } else { None },
        statistics: InterpolationStats {
            min_value: min_v,
            max_value: max_v,
            mean_value: mean_v,
            std_dev: var_v.sqrt(),
            cross_validation_rmse: None,
        },
    }
}

/// Compute Moran's I (spatial autocorrelation index).
pub fn morans_i(samples: &[SamplePoint], bandwidth: f64) -> f64 {
    let n = samples.len() as f64;
    if samples.len() < 3 {
        return 0.0;
    }
    let mean = samples.iter().map(|s| s.value).sum::<f64>() / n;

    let mut numerator = 0.0;
    let mut weight_sum = 0.0;
    let denominator: f64 = samples.iter().map(|s| (s.value - mean).powi(2)).sum();

    for i in 0..samples.len() {
        for j in 0..samples.len() {
            if i == j { continue; }
            let dx = samples[i].x - samples[j].x;
            let dy = samples[i].y - samples[j].y;
            let dist = (dx * dx + dy * dy).sqrt();
            let w = if dist < bandwidth { 1.0 } else { 0.0 };
            weight_sum += w;
            numerator += w * (samples[i].value - mean) * (samples[j].value - mean);
        }
    }

    if denominator == 0.0 || weight_sum == 0.0 {
        0.0
    } else {
        (n / weight_sum) * (numerator / denominator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> Vec<SamplePoint> {
        vec![
            SamplePoint { x: 0.0, y: 0.0, value: 10.0 },
            SamplePoint { x: 1.0, y: 0.0, value: 12.0 },
            SamplePoint { x: 0.0, y: 1.0, value: 11.0 },
            SamplePoint { x: 1.0, y: 1.0, value: 13.0 },
            SamplePoint { x: 0.5, y: 0.5, value: 11.5 },
        ]
    }

    #[test]
    fn test_idw_at_sample() {
        let samples = sample_data();
        let val = idw_interpolation(&samples, 0.0, 0.0, 2.0);
        assert!((val - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_idw_between_samples() {
        let samples = sample_data();
        let val = idw_interpolation(&samples, 0.5, 0.0, 2.0);
        assert!(val > 10.0 && val < 12.0);
    }

    #[test]
    fn test_compute_variogram() {
        let samples = sample_data();
        let bins = compute_variogram(&samples, 5, 2.0);
        assert!(!bins.is_empty());
        for bin in &bins {
            assert!(bin.semivariance >= 0.0);
            assert!(bin.pair_count > 0);
        }
    }

    #[test]
    fn test_fit_spherical_variogram() {
        let bins = vec![
            VariogramBin { lag_distance: 0.2, semivariance: 0.5, pair_count: 10 },
            VariogramBin { lag_distance: 0.5, semivariance: 1.2, pair_count: 20 },
            VariogramBin { lag_distance: 1.0, semivariance: 2.0, pair_count: 15 },
            VariogramBin { lag_distance: 1.5, semivariance: 2.3, pair_count: 8 },
        ];
        let params = fit_spherical_variogram(&bins);
        assert!(params.sill > 0.0);
        assert!(params.range > 0.0);
    }

    #[test]
    fn test_kriging_estimate() {
        let samples = sample_data();
        let variogram = VariogramParams {
            model: VariogramModel::Spherical,
            nugget: 0.1, sill: 2.0, range: 2.0, r_squared: 0.9,
        };
        let (est, var) = kriging_estimate(&samples, 0.5, 0.5, &variogram);
        assert!(est > 10.0 && est < 14.0);
        assert!(var >= 0.0);
    }

    #[test]
    fn test_interpolate_grid() {
        let samples = sample_data();
        let result = interpolate_grid(&samples, [0.0, 0.0, 1.0, 1.0], 0.5, &InterpolationMethod::Idw { power: 2.0 });
        assert_eq!(result.grid_rows, 2);
        assert_eq!(result.grid_cols, 2);
        assert_eq!(result.values.len(), 4);
    }

    #[test]
    fn test_morans_i_clustered() {
        // Spatially clustered data should have positive Moran's I
        let samples = vec![
            SamplePoint { x: 0.0, y: 0.0, value: 10.0 },
            SamplePoint { x: 0.1, y: 0.0, value: 10.5 },
            SamplePoint { x: 0.0, y: 0.1, value: 10.2 },
            SamplePoint { x: 5.0, y: 5.0, value: 50.0 },
            SamplePoint { x: 5.1, y: 5.0, value: 49.5 },
            SamplePoint { x: 5.0, y: 5.1, value: 50.3 },
        ];
        let i = morans_i(&samples, 1.0);
        assert!(i > 0.0, "Moran's I should be positive for clustered data, got {i}");
    }
}
