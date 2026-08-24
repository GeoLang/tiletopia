//! Geostatistics — spatial interpolation, kriging, IDW, variograms.
//!
//! Provides geostatistical methods for predicting values at unsampled
//! locations from point observations. Useful for environmental monitoring,
//! soil analysis, and property assessment.

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest sample count a dense kriging solve accepts. The solve factors an
/// n+3 square matrix, so this bounds one request's memory and its cost.
pub const MAX_SAMPLES: usize = 500;

/// Largest grid an interpolation may fill.
pub const MAX_GRID_CELLS: usize = 1_000_000;

/// Two samples this close in coordinate units are one location, and a repeated
/// location gives the kriging matrix two identical rows.
const DUPLICATE_DISTANCE: f64 = 1e-9;

/// A pivot smaller than this share of the largest matrix entry means the system
/// is singular.
const SINGULAR_PIVOT_RATIO: f64 = 1e-12;

/// Universal kriging fits a constant plus an x and a y term.
const DRIFT_TERMS: usize = 3;

/// Why an interpolation could not be computed.
#[derive(Debug, thiserror::Error)]
pub enum GeostatisticsError {
    #[error("interpolation needs at least one sample")]
    NoSamples,
    #[error("{count} samples is past the {MAX_SAMPLES} a dense kriging solve accepts")]
    TooManySamples { count: usize },
    #[error("sample {index} has a coordinate or value that is not a finite number")]
    NonFiniteSample { index: usize },
    #[error("samples {first} and {second} sit at the same location")]
    DuplicateLocation { first: usize, second: usize },
    #[error("bounds must be [min_x, min_y, max_x, max_y], finite, with each min below its max")]
    InvalidBounds,
    #[error("resolution must be a finite number above zero")]
    InvalidResolution,
    #[error("a {cells} cell grid is past the {MAX_GRID_CELLS} allowed")]
    GridTooLarge { cells: usize },
    #[error("IDW power must be a finite number above zero")]
    InvalidPower,
    #[error(
        "a variogram needs a finite sill above zero, a finite range above zero, and a nugget between zero and the sill"
    )]
    UnusableVariogram,
    #[error("simple kriging needs a known_mean that is a finite number")]
    NonFiniteMean,
    #[error("universal kriging fits a linear drift, which needs at least {DRIFT_TERMS} samples")]
    TooFewForDrift,
    #[error("the kriging system is singular, these samples cannot support this method")]
    SingularSystem,
}

impl GeostatisticsError {
    pub fn status(&self) -> StatusCode {
        match self {
            // the request is well formed, the sample geometry is degenerate
            GeostatisticsError::SingularSystem => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

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
    pub nugget: f64,    // variance at distance 0
    pub sill: f64,      // total variance plateau
    pub range: f64,     // distance at which sill is reached
    pub r_squared: f64, // goodness-of-fit
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
pub fn compute_variogram(
    samples: &[SamplePoint],
    n_lags: usize,
    max_lag: f64,
) -> Vec<VariogramBin> {
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
            nugget: 0.0,
            sill: 1.0,
            range: 1.0,
            r_squared: 0.0,
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
    let ss_tot = bins
        .iter()
        .map(|b| (b.semivariance - mean_var).powi(2))
        .sum::<f64>();
    let ss_res: f64 = bins
        .iter()
        .map(|b| {
            let predicted = spherical_model(b.lag_distance, nugget, sill, range);
            (b.semivariance - predicted).powi(2)
        })
        .sum();
    let r_squared = if ss_tot > 0.0 {
        1.0 - ss_res / ss_tot
    } else {
        0.0
    };

    VariogramParams {
        model: VariogramModel::Spherical,
        nugget,
        sill,
        range,
        r_squared,
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

/// Semivariance at a lag under the fitted model.
///
/// Every model is capped at the sill, so `sill - semivariance` is a covariance
/// for all of them and simple kriging can use any.
fn semivariance(variogram: &VariogramParams, lag: f64) -> f64 {
    if lag <= 0.0 {
        return 0.0;
    }
    let VariogramParams {
        nugget,
        sill,
        range,
        ..
    } = *variogram;
    let structured = sill - nugget;
    let hr = lag / range;
    match variogram.model {
        VariogramModel::Spherical => spherical_model(lag, nugget, sill, range),
        VariogramModel::Exponential => nugget + structured * (1.0 - (-3.0 * hr).exp()),
        VariogramModel::Gaussian => nugget + structured * (1.0 - (-3.0 * hr * hr).exp()),
        VariogramModel::Linear => nugget + structured * hr.min(1.0),
        VariogramModel::Power { exponent } => nugget + structured * hr.powf(exponent).min(1.0),
    }
}

fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn distance(ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = ax - bx;
    let dy = ay - by;
    (dx * dx + dy * dy).sqrt()
}

/// An LU factorization with partial pivoting.
///
/// A kriging matrix depends only on the samples, so it is factored once and
/// every query point costs one substitution instead of a whole elimination.
#[derive(Debug)]
struct LuFactorization {
    /// row-major, the unit lower triangle's multipliers below the diagonal
    entries: Vec<f64>,
    /// `row_order[i]` is the original row now in position `i`
    row_order: Vec<usize>,
    size: usize,
}

impl LuFactorization {
    fn factor(mut entries: Vec<f64>, size: usize) -> Result<Self, GeostatisticsError> {
        let largest = entries.iter().fold(0.0f64, |seen, e| seen.max(e.abs()));
        let smallest_usable_pivot = largest * SINGULAR_PIVOT_RATIO;
        let mut row_order: Vec<usize> = (0..size).collect();

        for column in 0..size {
            let mut pivot_position = column;
            let mut pivot_magnitude = entries[row_order[column] * size + column].abs();
            for candidate in (column + 1)..size {
                let magnitude = entries[row_order[candidate] * size + column].abs();
                if magnitude > pivot_magnitude {
                    pivot_magnitude = magnitude;
                    pivot_position = candidate;
                }
            }
            if pivot_magnitude <= smallest_usable_pivot {
                return Err(GeostatisticsError::SingularSystem);
            }
            row_order.swap(column, pivot_position);

            let pivot_row = row_order[column];
            let pivot = entries[pivot_row * size + column];
            for &target_row in &row_order[(column + 1)..] {
                let multiplier = entries[target_row * size + column] / pivot;
                entries[target_row * size + column] = multiplier;
                for c in (column + 1)..size {
                    entries[target_row * size + c] -= multiplier * entries[pivot_row * size + c];
                }
            }
        }

        Ok(Self {
            entries,
            row_order,
            size,
        })
    }

    fn solve(&self, right_hand_side: &[f64]) -> Vec<f64> {
        let size = self.size;
        let mut solution = vec![0.0; size];

        for position in 0..size {
            let row = self.row_order[position];
            let row_start = row * size;
            let lower = &self.entries[row_start..(row_start + position)];
            let substituted = right_hand_side[row] - dot(lower, &solution[..position]);
            solution[position] = substituted;
        }

        for position in (0..size).rev() {
            let row_start = self.row_order[position] * size;
            let upper = &self.entries[(row_start + position + 1)..(row_start + size)];
            let pivot = self.entries[row_start + position];
            let substituted =
                (solution[position] - dot(upper, &solution[(position + 1)..])) / pivot;
            solution[position] = substituted;
        }

        solution
    }
}

/// Which kriging system was factored, and what the estimate needs from it.
#[derive(Debug)]
enum KrigingKind {
    /// Semivariances plus a Lagrange row holding the weights to a sum of one.
    Ordinary,
    /// Covariances around a mean the caller supplies. No Lagrange row: the
    /// weights are free and the mean carries whatever they leave.
    Simple { known_mean: f64 },
    /// Ordinary plus rows for a constant, x and y drift. Coordinates are
    /// centred on the sample centroid so the drift rows stay near the scale of
    /// the semivariances.
    Universal { center_x: f64, center_y: f64 },
}

/// A kriging system factored for one sample set, ready to estimate any number
/// of query points.
#[derive(Debug)]
pub struct KrigingSolver {
    samples: Vec<SamplePoint>,
    variogram: VariogramParams,
    kind: KrigingKind,
    factorization: LuFactorization,
}

impl KrigingSolver {
    /// Ordinary kriging: an unknown constant mean, weights summing to one.
    pub fn ordinary(
        samples: &[SamplePoint],
        variogram: VariogramParams,
    ) -> Result<Self, GeostatisticsError> {
        validate_samples(samples)?;
        validate_variogram(&variogram)?;
        let n = samples.len();
        let size = n + 1;
        let mut entries = vec![0.0; size * size];
        fill_semivariances(&mut entries, size, samples, &variogram);
        for i in 0..n {
            entries[i * size + n] = 1.0;
            entries[n * size + i] = 1.0;
        }

        Ok(Self {
            samples: samples.to_vec(),
            variogram,
            kind: KrigingKind::Ordinary,
            factorization: LuFactorization::factor(entries, size)?,
        })
    }

    /// Simple kriging: the mean is known, so the covariance system has no
    /// unbiasedness constraint.
    pub fn simple(
        samples: &[SamplePoint],
        variogram: VariogramParams,
        known_mean: f64,
    ) -> Result<Self, GeostatisticsError> {
        validate_samples(samples)?;
        validate_variogram(&variogram)?;
        if !known_mean.is_finite() {
            return Err(GeostatisticsError::NonFiniteMean);
        }
        let size = samples.len();
        let mut entries = vec![0.0; size * size];
        for i in 0..size {
            for j in 0..size {
                let lag = distance(samples[i].x, samples[i].y, samples[j].x, samples[j].y);
                entries[i * size + j] = variogram.sill - semivariance(&variogram, lag);
            }
        }

        Ok(Self {
            samples: samples.to_vec(),
            variogram,
            kind: KrigingKind::Simple { known_mean },
            factorization: LuFactorization::factor(entries, size)?,
        })
    }

    /// Universal kriging: the mean is an unknown linear function of position.
    pub fn universal(
        samples: &[SamplePoint],
        variogram: VariogramParams,
    ) -> Result<Self, GeostatisticsError> {
        validate_samples(samples)?;
        validate_variogram(&variogram)?;
        let n = samples.len();
        if n < DRIFT_TERMS {
            return Err(GeostatisticsError::TooFewForDrift);
        }
        let center_x = samples.iter().map(|s| s.x).sum::<f64>() / n as f64;
        let center_y = samples.iter().map(|s| s.y).sum::<f64>() / n as f64;

        let size = n + DRIFT_TERMS;
        let mut entries = vec![0.0; size * size];
        fill_semivariances(&mut entries, size, samples, &variogram);
        for i in 0..n {
            let drift = [1.0, samples[i].x - center_x, samples[i].y - center_y];
            for (term, value) in drift.iter().enumerate() {
                entries[i * size + n + term] = *value;
                entries[(n + term) * size + i] = *value;
            }
        }

        Ok(Self {
            samples: samples.to_vec(),
            variogram,
            kind: KrigingKind::Universal { center_x, center_y },
            factorization: LuFactorization::factor(entries, size)?,
        })
    }

    /// Estimate and kriging variance at one query point.
    pub fn estimate(&self, query_x: f64, query_y: f64) -> (f64, f64) {
        let (solution, right_hand_side) = self.solve_at(query_x, query_y);
        let n = self.samples.len();
        let weights = &solution[..n];

        match self.kind {
            KrigingKind::Simple { known_mean } => {
                let departure: f64 = weights
                    .iter()
                    .zip(&self.samples)
                    .map(|(w, s)| w * (s.value - known_mean))
                    .sum();
                let explained = dot(weights, &right_hand_side[..n]);
                (
                    known_mean + departure,
                    (self.variogram.sill - explained).max(0.0),
                )
            }
            KrigingKind::Ordinary | KrigingKind::Universal { .. } => {
                let estimate: f64 = weights
                    .iter()
                    .zip(&self.samples)
                    .map(|(w, s)| w * s.value)
                    .sum();
                // the multiplier rows carry their own right-hand side terms, so
                // the whole dot product is the variance
                let variance = dot(&solution, &right_hand_side);
                (estimate, variance.max(0.0))
            }
        }
    }

    /// The solved system and the right-hand side it was solved against. The
    /// first `samples.len()` entries of the solution are the sample weights.
    fn solve_at(&self, query_x: f64, query_y: f64) -> (Vec<f64>, Vec<f64>) {
        let n = self.samples.len();
        let size = self.factorization.size;
        let mut right_hand_side = vec![0.0; size];

        for (i, sample) in self.samples.iter().enumerate() {
            let lag = distance(sample.x, sample.y, query_x, query_y);
            let gamma = semivariance(&self.variogram, lag);
            right_hand_side[i] = match self.kind {
                KrigingKind::Simple { .. } => self.variogram.sill - gamma,
                KrigingKind::Ordinary | KrigingKind::Universal { .. } => gamma,
            };
        }
        match self.kind {
            KrigingKind::Simple { .. } => {}
            KrigingKind::Ordinary => right_hand_side[n] = 1.0,
            KrigingKind::Universal { center_x, center_y } => {
                right_hand_side[n] = 1.0;
                right_hand_side[n + 1] = query_x - center_x;
                right_hand_side[n + 2] = query_y - center_y;
            }
        }

        (self.factorization.solve(&right_hand_side), right_hand_side)
    }
}

/// The pairwise semivariance block, top left of a matrix `size` wide.
fn fill_semivariances(
    entries: &mut [f64],
    size: usize,
    samples: &[SamplePoint],
    variogram: &VariogramParams,
) {
    for i in 0..samples.len() {
        for j in 0..samples.len() {
            let lag = distance(samples[i].x, samples[i].y, samples[j].x, samples[j].y);
            entries[i * size + j] = semivariance(variogram, lag);
        }
    }
}

fn validate_samples(samples: &[SamplePoint]) -> Result<(), GeostatisticsError> {
    if samples.is_empty() {
        return Err(GeostatisticsError::NoSamples);
    }
    if samples.len() > MAX_SAMPLES {
        return Err(GeostatisticsError::TooManySamples {
            count: samples.len(),
        });
    }
    for (index, sample) in samples.iter().enumerate() {
        if !sample.x.is_finite() || !sample.y.is_finite() || !sample.value.is_finite() {
            return Err(GeostatisticsError::NonFiniteSample { index });
        }
    }
    for (second, later) in samples.iter().enumerate() {
        for (first, earlier) in samples[..second].iter().enumerate() {
            if distance(earlier.x, earlier.y, later.x, later.y) < DUPLICATE_DISTANCE {
                return Err(GeostatisticsError::DuplicateLocation { first, second });
            }
        }
    }
    Ok(())
}

fn validate_variogram(variogram: &VariogramParams) -> Result<(), GeostatisticsError> {
    let usable = variogram.sill.is_finite()
        && variogram.sill > 0.0
        && variogram.range.is_finite()
        && variogram.range > 0.0
        && variogram.nugget.is_finite()
        && variogram.nugget >= 0.0
        && variogram.nugget <= variogram.sill;
    if usable {
        Ok(())
    } else {
        Err(GeostatisticsError::UnusableVariogram)
    }
}

/// Ordinary kriging at a single point.
pub fn kriging_estimate(
    samples: &[SamplePoint],
    query_x: f64,
    query_y: f64,
    variogram: &VariogramParams,
) -> Result<(f64, f64), GeostatisticsError> {
    Ok(KrigingSolver::ordinary(samples, variogram.clone())?.estimate(query_x, query_y))
}

/// What fills each cell of a grid.
enum CellEvaluator {
    Idw { power: f64 },
    Kriging(KrigingSolver),
}

/// Interpolate a grid over `bounds` by the chosen method.
pub fn interpolate_grid(
    samples: &[SamplePoint],
    bounds: [f64; 4],
    resolution: f64,
    method: &InterpolationMethod,
) -> Result<InterpolationResult, GeostatisticsError> {
    validate_samples(samples)?;
    if !bounds.iter().all(|b| b.is_finite()) || bounds[0] >= bounds[2] || bounds[1] >= bounds[3] {
        return Err(GeostatisticsError::InvalidBounds);
    }
    if !resolution.is_finite() || resolution <= 0.0 {
        return Err(GeostatisticsError::InvalidResolution);
    }

    let cols = ((bounds[2] - bounds[0]) / resolution).ceil() as usize;
    let rows = ((bounds[3] - bounds[1]) / resolution).ceil() as usize;
    let cells = cols.saturating_mul(rows);
    if cells > MAX_GRID_CELLS {
        return Err(GeostatisticsError::GridTooLarge { cells });
    }

    let evaluator =
        match method {
            InterpolationMethod::Idw { power } => {
                if !power.is_finite() || *power <= 0.0 {
                    return Err(GeostatisticsError::InvalidPower);
                }
                CellEvaluator::Idw { power: *power }
            }
            InterpolationMethod::OrdinaryKriging => CellEvaluator::Kriging(
                KrigingSolver::ordinary(samples, fit_sample_variogram(samples))?,
            ),
            InterpolationMethod::SimpleKriging { known_mean } => CellEvaluator::Kriging(
                KrigingSolver::simple(samples, fit_sample_variogram(samples), *known_mean)?,
            ),
            InterpolationMethod::UniversalKriging => CellEvaluator::Kriging(
                KrigingSolver::universal(samples, fit_sample_variogram(samples))?,
            ),
        };
    let kriged = matches!(evaluator, CellEvaluator::Kriging(_));

    let mut values = Vec::with_capacity(cells);
    let mut variances = Vec::with_capacity(if kriged { cells } else { 0 });
    for r in 0..rows {
        let y = bounds[1] + (r as f64 + 0.5) * resolution;
        for c in 0..cols {
            let x = bounds[0] + (c as f64 + 0.5) * resolution;
            match &evaluator {
                CellEvaluator::Idw { power } => {
                    values.push(idw_interpolation(samples, x, y, *power));
                }
                CellEvaluator::Kriging(solver) => {
                    let (estimate, variance) = solver.estimate(x, y);
                    values.push(estimate);
                    variances.push(variance);
                }
            }
        }
    }

    let min_v = values.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_v = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mean_v = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let var_v =
        values.iter().map(|v| (v - mean_v).powi(2)).sum::<f64>() / values.len().max(1) as f64;

    Ok(InterpolationResult {
        id: Uuid::new_v4(),
        method: method.clone(),
        bounds,
        resolution,
        grid_rows: rows,
        grid_cols: cols,
        values,
        variances: kriged.then_some(variances),
        statistics: InterpolationStats {
            min_value: min_v,
            max_value: max_v,
            mean_value: mean_v,
            std_dev: var_v.sqrt(),
            cross_validation_rmse: None,
        },
    })
}

/// Fit a variogram over the samples' own extent, so the lag bins cover the
/// distances the solve will actually ask about.
fn fit_sample_variogram(samples: &[SamplePoint]) -> VariogramParams {
    let min_x = samples.iter().map(|s| s.x).fold(f64::INFINITY, f64::min);
    let max_x = samples
        .iter()
        .map(|s| s.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = samples.iter().map(|s| s.y).fold(f64::INFINITY, f64::min);
    let max_y = samples
        .iter()
        .map(|s| s.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let extent = distance(min_x, min_y, max_x, max_y);
    fit_spherical_variogram(&compute_variogram(samples, 10, extent))
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
            if i == j {
                continue;
            }
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
            SamplePoint {
                x: 0.0,
                y: 0.0,
                value: 10.0,
            },
            SamplePoint {
                x: 1.0,
                y: 0.0,
                value: 12.0,
            },
            SamplePoint {
                x: 0.0,
                y: 1.0,
                value: 11.0,
            },
            SamplePoint {
                x: 1.0,
                y: 1.0,
                value: 13.0,
            },
            SamplePoint {
                x: 0.5,
                y: 0.5,
                value: 11.5,
            },
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
            VariogramBin {
                lag_distance: 0.2,
                semivariance: 0.5,
                pair_count: 10,
            },
            VariogramBin {
                lag_distance: 0.5,
                semivariance: 1.2,
                pair_count: 20,
            },
            VariogramBin {
                lag_distance: 1.0,
                semivariance: 2.0,
                pair_count: 15,
            },
            VariogramBin {
                lag_distance: 1.5,
                semivariance: 2.3,
                pair_count: 8,
            },
        ];
        let params = fit_spherical_variogram(&bins);
        assert!(params.sill > 0.0);
        assert!(params.range > 0.0);
    }

    #[test]
    fn test_kriging_estimate() {
        let samples = sample_data();
        let (est, var) = kriging_estimate(&samples, 0.5, 0.5, &spherical(0.1)).unwrap();
        assert!(est > 10.0 && est < 14.0);
        assert!(var >= 0.0);
    }

    #[test]
    fn test_interpolate_grid() {
        let samples = sample_data();
        let result = interpolate_grid(
            &samples,
            [0.0, 0.0, 1.0, 1.0],
            0.5,
            &InterpolationMethod::Idw { power: 2.0 },
        )
        .unwrap();
        assert_eq!(result.grid_rows, 2);
        assert_eq!(result.grid_cols, 2);
        assert_eq!(result.values.len(), 4);
        assert!(result.variances.is_none());
    }

    fn spherical(nugget: f64) -> VariogramParams {
        VariogramParams {
            model: VariogramModel::Spherical,
            nugget,
            sill: 2.0,
            range: 2.0,
            r_squared: 0.9,
        }
    }

    /// A north-east trend, so ordinary and universal kriging cannot agree.
    fn trending_data() -> Vec<SamplePoint> {
        vec![
            SamplePoint {
                x: 0.0,
                y: 0.0,
                value: 10.0,
            },
            SamplePoint {
                x: 2.0,
                y: 0.0,
                value: 14.0,
            },
            SamplePoint {
                x: 0.0,
                y: 2.0,
                value: 16.0,
            },
            SamplePoint {
                x: 2.0,
                y: 2.0,
                value: 22.0,
            },
            SamplePoint {
                x: 1.0,
                y: 0.7,
                value: 15.0,
            },
        ]
    }

    #[test]
    fn ordinary_kriging_weights_sum_to_one() {
        let samples = sample_data();
        let solver = KrigingSolver::ordinary(&samples, spherical(0.1)).unwrap();

        let (solution, _) = solver.solve_at(0.3, 0.8);
        let weight_sum: f64 = solution[..samples.len()].iter().sum();

        assert!(
            (weight_sum - 1.0).abs() < 1e-9,
            "ordinary kriging weights must sum to 1, got {weight_sum}"
        );
    }

    #[test]
    fn ordinary_kriging_reproduces_a_sample_value_at_its_own_location() {
        let samples = sample_data();
        let solver = KrigingSolver::ordinary(&samples, spherical(0.0)).unwrap();

        for sample in &samples {
            let (estimate, variance) = solver.estimate(sample.x, sample.y);
            assert!(
                (estimate - sample.value).abs() < 1e-9,
                "kriging at ({}, {}) gave {estimate}, not the sampled {}",
                sample.x,
                sample.y,
                sample.value
            );
            assert!(variance < 1e-9, "variance at a sample should vanish");
        }
    }

    #[test]
    fn the_three_kriging_methods_disagree_on_trending_data() {
        let samples = trending_data();
        let variogram = fit_sample_variogram(&samples);
        let query = (1.6, 1.4);

        let ordinary = KrigingSolver::ordinary(&samples, variogram.clone())
            .unwrap()
            .estimate(query.0, query.1);
        let simple = KrigingSolver::simple(&samples, variogram.clone(), 12.0)
            .unwrap()
            .estimate(query.0, query.1);
        let universal = KrigingSolver::universal(&samples, variogram)
            .unwrap()
            .estimate(query.0, query.1);

        for (left_name, left, right_name, right) in [
            ("ordinary", ordinary, "simple", simple),
            ("ordinary", ordinary, "universal", universal),
            ("simple", simple, "universal", universal),
        ] {
            assert!(
                (left.0 - right.0).abs() > 1e-6,
                "{left_name} gave {} and {right_name} gave {}, they should differ",
                left.0,
                right.0
            );
        }
    }

    #[test]
    fn simple_kriging_returns_the_known_mean_far_from_every_sample() {
        let samples = sample_data();
        let known_mean = 100.0;
        let solver = KrigingSolver::simple(&samples, spherical(0.1), known_mean).unwrap();

        let (near, _) = solver.estimate(0.5, 0.5);
        let (far, far_variance) = solver.estimate(500.0, 500.0);

        assert!(
            (far - known_mean).abs() < 1e-6,
            "far from the samples simple kriging should be the known mean, got {far}"
        );
        assert!(
            (near - known_mean).abs() > 1.0,
            "among the samples it should follow the data, got {near}"
        );
        assert!((far_variance - spherical(0.1).sill).abs() < 1e-6);
    }

    #[test]
    fn samples_stacked_at_one_location_are_refused() {
        let stacked = vec![
            SamplePoint {
                x: 1.0,
                y: 1.0,
                value: 10.0,
            },
            SamplePoint {
                x: 1.0,
                y: 1.0,
                value: 12.0,
            },
            SamplePoint {
                x: 1.0,
                y: 1.0,
                value: 14.0,
            },
        ];

        let refusal = KrigingSolver::ordinary(&stacked, spherical(0.0)).expect_err("not solvable");

        assert!(matches!(
            refusal,
            GeostatisticsError::DuplicateLocation {
                first: 0,
                second: 1
            }
        ));
    }

    #[test]
    fn a_singular_kriging_system_is_an_error_not_a_nan() {
        // collinear samples leave the x and y drift rows dependent
        let collinear: Vec<SamplePoint> = (0..4)
            .map(|i| SamplePoint {
                x: i as f64,
                y: 2.0 * i as f64,
                value: 10.0 + i as f64,
            })
            .collect();

        let refusal =
            KrigingSolver::universal(&collinear, spherical(0.0)).expect_err("not solvable");

        assert!(matches!(refusal, GeostatisticsError::SingularSystem));
    }

    /// Samples lying exactly on a plane leave no residual for the covariance
    /// part, so the drift rows alone must reproduce the plane anywhere.
    #[test]
    fn universal_kriging_recovers_a_linear_trend_the_samples_lie_on() {
        let plane = |x: f64, y: f64| 3.0 + 2.0 * x - 0.5 * y;
        let samples: Vec<SamplePoint> =
            [(0.0, 0.0), (4.0, 1.0), (1.0, 3.0), (5.0, 4.0), (2.0, 6.0)]
                .into_iter()
                .map(|(x, y)| SamplePoint {
                    x,
                    y,
                    value: plane(x, y),
                })
                .collect();
        let solver = KrigingSolver::universal(&samples, spherical(0.0)).unwrap();

        for (x, y) in [(2.5, 2.5), (-3.0, 1.0), (9.0, 8.0)] {
            let (estimate, _) = solver.estimate(x, y);
            assert!(
                (estimate - plane(x, y)).abs() < 1e-6,
                "at ({x}, {y}) universal kriging gave {estimate}, not the plane's {}",
                plane(x, y)
            );
        }
    }

    #[test]
    fn universal_kriging_needs_enough_samples_for_its_drift() {
        let two = &sample_data()[..2];

        let refusal = KrigingSolver::universal(two, spherical(0.0)).expect_err("not solvable");

        assert!(matches!(refusal, GeostatisticsError::TooFewForDrift));
    }

    #[test]
    fn a_non_finite_sample_is_refused() {
        let mut samples = sample_data();
        samples[2].value = f64::NAN;

        let refusal = interpolate_grid(
            &samples,
            [0.0, 0.0, 1.0, 1.0],
            0.5,
            &InterpolationMethod::OrdinaryKriging,
        )
        .expect_err("not solvable");

        assert!(matches!(
            refusal,
            GeostatisticsError::NonFiniteSample { index: 2 }
        ));
    }

    #[test]
    fn a_kriged_grid_carries_one_variance_per_cell() {
        let samples = sample_data();
        let result = interpolate_grid(
            &samples,
            [0.0, 0.0, 1.0, 1.0],
            0.25,
            &InterpolationMethod::UniversalKriging,
        )
        .unwrap();

        let variances = result.variances.expect("kriging reports variance");
        assert_eq!(variances.len(), result.values.len());
        assert!(variances.iter().all(|v| v.is_finite() && *v >= 0.0));
        assert!(result.values.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn a_grid_past_the_cell_cap_is_refused() {
        let samples = sample_data();
        let refusal = interpolate_grid(
            &samples,
            [0.0, 0.0, 2000.0, 2000.0],
            1.0,
            &InterpolationMethod::Idw { power: 2.0 },
        )
        .expect_err("past the cap");

        assert!(matches!(
            refusal,
            GeostatisticsError::GridTooLarge { cells: 4_000_000 }
        ));
    }

    #[test]
    fn more_samples_than_the_dense_solve_accepts_are_refused() {
        let many: Vec<SamplePoint> = (0..=MAX_SAMPLES)
            .map(|i| SamplePoint {
                x: i as f64,
                y: (i * i % 97) as f64,
                value: i as f64,
            })
            .collect();

        let refusal = validate_samples(&many).expect_err("past the cap");

        assert!(matches!(refusal, GeostatisticsError::TooManySamples { .. }));
    }

    #[test]
    fn test_morans_i_clustered() {
        // Spatially clustered data should have positive Moran's I
        let samples = vec![
            SamplePoint {
                x: 0.0,
                y: 0.0,
                value: 10.0,
            },
            SamplePoint {
                x: 0.1,
                y: 0.0,
                value: 10.5,
            },
            SamplePoint {
                x: 0.0,
                y: 0.1,
                value: 10.2,
            },
            SamplePoint {
                x: 5.0,
                y: 5.0,
                value: 50.0,
            },
            SamplePoint {
                x: 5.1,
                y: 5.0,
                value: 49.5,
            },
            SamplePoint {
                x: 5.0,
                y: 5.1,
                value: 50.3,
            },
        ];
        let i = morans_i(&samples, 1.0);
        assert!(
            i > 0.0,
            "Moran's I should be positive for clustered data, got {i}"
        );
    }
}
