//! Time-series predictive analytics for terrain change,
//! subsidence, and vegetation growth forecasting.

/// A time-series data point.
#[derive(Debug, Clone)]
pub struct TimeSeriesPoint {
    pub timestamp: f64, // seconds since epoch
    pub value: f64,
}

/// Forecast result with confidence interval.
#[derive(Debug, Clone)]
pub struct Forecast {
    pub timestamp: f64,
    pub predicted_value: f64,
    pub lower_bound: f64,
    pub upper_bound: f64,
    pub confidence: f64,
}

/// Linear regression model for trend extraction.
#[derive(Debug, Clone)]
pub struct LinearModel {
    pub slope: f64,
    pub intercept: f64,
    pub r_squared: f64,
}

/// Fit a linear regression to time-series data.
pub fn fit_linear(data: &[TimeSeriesPoint]) -> Option<LinearModel> {
    let n = data.len() as f64;
    if data.len() < 2 {
        return None;
    }

    let sum_x: f64 = data.iter().map(|p| p.timestamp).sum();
    let sum_y: f64 = data.iter().map(|p| p.value).sum();
    let sum_xy: f64 = data.iter().map(|p| p.timestamp * p.value).sum();
    let sum_xx: f64 = data.iter().map(|p| p.timestamp * p.timestamp).sum();

    let denom = n * sum_xx - sum_x * sum_x;
    if denom.abs() < 1e-15 {
        return None;
    }

    let slope = (n * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n;

    // R-squared
    let mean_y = sum_y / n;
    let ss_tot: f64 = data.iter().map(|p| (p.value - mean_y).powi(2)).sum();
    let ss_res: f64 = data
        .iter()
        .map(|p| {
            let predicted = slope * p.timestamp + intercept;
            (p.value - predicted).powi(2)
        })
        .sum();

    let r_squared = if ss_tot > 0.0 {
        1.0 - (ss_res / ss_tot)
    } else {
        0.0
    };

    Some(LinearModel {
        slope,
        intercept,
        r_squared,
    })
}

/// Exponential smoothing for time-series forecasting.
#[derive(Debug, Clone)]
pub struct ExponentialSmoothing {
    pub alpha: f64, // level smoothing (0-1)
    pub beta: f64,  // trend smoothing (0-1)
}

impl Default for ExponentialSmoothing {
    fn default() -> Self {
        Self {
            alpha: 0.3,
            beta: 0.1,
        }
    }
}

impl ExponentialSmoothing {
    /// Forecast future values using Holt's double exponential smoothing.
    pub fn forecast(&self, data: &[TimeSeriesPoint], horizon: usize) -> Vec<Forecast> {
        if data.len() < 2 {
            return Vec::new();
        }

        // Initialize
        let mut level = data[0].value;
        let mut trend = data[1].value - data[0].value;
        let dt = if data.len() > 1 {
            (data.last().unwrap().timestamp - data[0].timestamp) / (data.len() - 1) as f64
        } else {
            1.0
        };

        // Fit to data
        let mut residuals = Vec::new();
        for point in data.iter().skip(1) {
            let prev_level = level;
            level = self.alpha * point.value + (1.0 - self.alpha) * (level + trend);
            trend = self.beta * (level - prev_level) + (1.0 - self.beta) * trend;
            let predicted = prev_level + trend;
            residuals.push((point.value - predicted).powi(2));
        }

        // Standard error of residuals
        let mse = if residuals.is_empty() {
            0.0
        } else {
            residuals.iter().sum::<f64>() / residuals.len() as f64
        };
        let std_err = mse.sqrt();

        // Forecast
        let last_t = data.last().unwrap().timestamp;
        (1..=horizon)
            .map(|h| {
                let t = last_t + h as f64 * dt;
                let predicted = level + trend * h as f64;
                let interval = 1.96 * std_err * (h as f64).sqrt();
                Forecast {
                    timestamp: t,
                    predicted_value: predicted,
                    lower_bound: predicted - interval,
                    upper_bound: predicted + interval,
                    confidence: 0.95,
                }
            })
            .collect()
    }
}

/// Detect trend direction and rate of change.
#[derive(Debug, Clone, PartialEq)]
pub enum TrendDirection {
    Increasing,
    Decreasing,
    Stable,
}

/// Analyze trend from time-series data.
pub fn analyze_trend(
    data: &[TimeSeriesPoint],
    stability_threshold: f64,
) -> Option<(TrendDirection, f64)> {
    let model = fit_linear(data)?;
    let direction = if model.slope > stability_threshold {
        TrendDirection::Increasing
    } else if model.slope < -stability_threshold {
        TrendDirection::Decreasing
    } else {
        TrendDirection::Stable
    };
    Some((direction, model.slope))
}

/// Seasonal decomposition (simple additive).
/// Returns (trend, seasonal, residual) components.
pub fn seasonal_decompose(
    data: &[TimeSeriesPoint],
    period: usize,
) -> Option<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    if data.len() < period * 2 {
        return None;
    }

    let values: Vec<f64> = data.iter().map(|p| p.value).collect();
    let n = values.len();

    // Moving average for trend
    let mut trend = vec![0.0; n];
    let half = period / 2;
    for i in half..(n - half) {
        let window = &values[i - half..=i + half];
        trend[i] = window.iter().sum::<f64>() / window.len() as f64;
    }
    // Extend edges
    for i in 0..half {
        trend[i] = trend[half];
    }
    for i in (n - half)..n {
        trend[i] = trend[n - half - 1];
    }

    // Detrended
    let detrended: Vec<f64> = values
        .iter()
        .zip(trend.iter())
        .map(|(v, t)| v - t)
        .collect();

    // Seasonal: average over each position in the period
    let mut seasonal_pattern = vec![0.0; period];
    let mut counts = vec![0usize; period];
    for (i, d) in detrended.iter().enumerate() {
        let pos = i % period;
        seasonal_pattern[pos] += d;
        counts[pos] += 1;
    }
    for (s, c) in seasonal_pattern.iter_mut().zip(counts.iter()) {
        if *c > 0 {
            *s /= *c as f64;
        }
    }

    let seasonal: Vec<f64> = (0..n).map(|i| seasonal_pattern[i % period]).collect();
    let residual: Vec<f64> = values
        .iter()
        .zip(trend.iter())
        .zip(seasonal.iter())
        .map(|((v, t), s)| v - t - s)
        .collect();

    Some((trend, seasonal, residual))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linear_fit() {
        let data: Vec<TimeSeriesPoint> = (0..10)
            .map(|i| TimeSeriesPoint {
                timestamp: i as f64,
                value: 2.0 * i as f64 + 1.0,
            })
            .collect();
        let model = fit_linear(&data).unwrap();
        assert!((model.slope - 2.0).abs() < 1e-10);
        assert!((model.intercept - 1.0).abs() < 1e-10);
        assert!((model.r_squared - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_exponential_smoothing_forecast() {
        let data: Vec<TimeSeriesPoint> = (0..20)
            .map(|i| TimeSeriesPoint {
                timestamp: i as f64,
                value: i as f64 * 0.5 + 10.0,
            })
            .collect();
        let es = ExponentialSmoothing::default();
        let forecasts = es.forecast(&data, 5);
        assert_eq!(forecasts.len(), 5);
        // Should predict increasing values
        assert!(forecasts[0].predicted_value > 10.0);
        assert!(forecasts[4].predicted_value > forecasts[0].predicted_value);
    }

    #[test]
    fn test_analyze_trend() {
        let data: Vec<TimeSeriesPoint> = (0..10)
            .map(|i| TimeSeriesPoint {
                timestamp: i as f64,
                value: i as f64 * 3.0,
            })
            .collect();
        let (direction, rate) = analyze_trend(&data, 0.01).unwrap();
        assert_eq!(direction, TrendDirection::Increasing);
        assert!((rate - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_seasonal_decompose() {
        // Create data with known seasonality
        let data: Vec<TimeSeriesPoint> = (0..24)
            .map(|i| {
                let seasonal = (i % 4) as f64 * 2.0;
                TimeSeriesPoint {
                    timestamp: i as f64,
                    value: 10.0 + seasonal,
                }
            })
            .collect();
        let result = seasonal_decompose(&data, 4);
        assert!(result.is_some());
        let (trend, seasonal, _residual) = result.unwrap();
        assert_eq!(trend.len(), 24);
        assert_eq!(seasonal.len(), 24);
        // Seasonal pattern should repeat
        assert!((seasonal[0] - seasonal[4]).abs() < 1.0);
    }

    #[test]
    fn test_forecast_confidence_intervals() {
        let data: Vec<TimeSeriesPoint> = (0..30)
            .map(|i| TimeSeriesPoint {
                timestamp: i as f64,
                value: (i as f64 * 0.1).sin() * 5.0 + 20.0,
            })
            .collect();
        let es = ExponentialSmoothing {
            alpha: 0.5,
            beta: 0.2,
        };
        let forecasts = es.forecast(&data, 3);
        for f in &forecasts {
            assert!(f.lower_bound <= f.predicted_value);
            assert!(f.upper_bound >= f.predicted_value);
            assert!((f.confidence - 0.95).abs() < 1e-10);
        }
    }
}
