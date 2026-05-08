//! AI-powered anomaly detection for structural deformation,
//! vegetation encroachment, and construction progress deviation.

use std::collections::HashMap;

/// Type of anomaly detected.
#[derive(Debug, Clone, PartialEq)]
pub enum AnomalyType {
    StructuralDeformation,
    VegetationEncroachment,
    ConstructionDeviation,
    UnexpectedChange,
    Subsidence,
}

/// A detected anomaly with location and severity.
#[derive(Debug, Clone)]
pub struct Anomaly {
    pub anomaly_type: AnomalyType,
    pub location: [f64; 3],
    pub severity: f64, // 0.0 to 1.0
    pub description: String,
    pub confidence: f64,
}

/// Configuration for anomaly detection.
#[derive(Debug, Clone)]
pub struct AnomalyConfig {
    pub deformation_threshold: f64,
    pub encroachment_distance: f64,
    pub deviation_tolerance: f64,
    pub min_confidence: f64,
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            deformation_threshold: 0.05, // 5cm
            encroachment_distance: 1.0,  // 1m
            deviation_tolerance: 0.1,    // 10cm
            min_confidence: 0.7,
        }
    }
}

/// Detect structural deformation by comparing point clouds from two epochs.
///
/// Returns anomalies where displacement exceeds threshold.
pub fn detect_deformation(
    epoch_a: &[[f64; 3]],
    epoch_b: &[[f64; 3]],
    config: &AnomalyConfig,
) -> Vec<Anomaly> {
    let mut anomalies = Vec::new();

    // Build spatial grid for nearest-neighbor lookup
    let cell_size = config.deformation_threshold * 10.0;
    let mut grid: HashMap<(i64, i64, i64), Vec<usize>> = HashMap::new();

    for (i, p) in epoch_a.iter().enumerate() {
        let key = (
            (p[0] / cell_size).floor() as i64,
            (p[1] / cell_size).floor() as i64,
            (p[2] / cell_size).floor() as i64,
        );
        grid.entry(key).or_default().push(i);
    }

    for point_b in epoch_b {
        let key = (
            (point_b[0] / cell_size).floor() as i64,
            (point_b[1] / cell_size).floor() as i64,
            (point_b[2] / cell_size).floor() as i64,
        );

        // Search neighboring cells
        let mut min_dist = f64::MAX;
        let mut nearest_a = [0.0; 3];
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let nkey = (key.0 + dx, key.1 + dy, key.2 + dz);
                    if let Some(indices) = grid.get(&nkey) {
                        for &i in indices {
                            let pa = &epoch_a[i];
                            let dist = ((pa[0] - point_b[0]).powi(2)
                                + (pa[1] - point_b[1]).powi(2)
                                + (pa[2] - point_b[2]).powi(2))
                            .sqrt();
                            if dist < min_dist {
                                min_dist = dist;
                                nearest_a = *pa;
                            }
                        }
                    }
                }
            }
        }

        if min_dist > config.deformation_threshold && min_dist < cell_size {
            let severity = (min_dist / (config.deformation_threshold * 10.0)).min(1.0);
            let confidence = 1.0 - (min_dist / cell_size);
            if confidence >= config.min_confidence {
                anomalies.push(Anomaly {
                    anomaly_type: AnomalyType::StructuralDeformation,
                    location: *point_b,
                    severity,
                    description: format!(
                        "Deformation of {:.3}m detected (from [{:.2},{:.2},{:.2}])",
                        min_dist, nearest_a[0], nearest_a[1], nearest_a[2]
                    ),
                    confidence,
                });
            }
        }
    }
    anomalies
}

/// Detect vegetation encroachment near structures.
///
/// `structure_points` are building/infrastructure points.
/// `vegetation_points` are vegetation-classified points.
pub fn detect_encroachment(
    structure_points: &[[f64; 3]],
    vegetation_points: &[[f64; 3]],
    config: &AnomalyConfig,
) -> Vec<Anomaly> {
    let mut anomalies = Vec::new();
    let threshold_sq = config.encroachment_distance * config.encroachment_distance;

    for veg in vegetation_points {
        let mut min_dist_sq = f64::MAX;
        let mut nearest_struct = [0.0; 3];

        for struc in structure_points {
            let dsq = (veg[0] - struc[0]).powi(2)
                + (veg[1] - struc[1]).powi(2)
                + (veg[2] - struc[2]).powi(2);
            if dsq < min_dist_sq {
                min_dist_sq = dsq;
                nearest_struct = *struc;
            }
        }

        if min_dist_sq < threshold_sq {
            let dist = min_dist_sq.sqrt();
            let severity = 1.0 - (dist / config.encroachment_distance);
            anomalies.push(Anomaly {
                anomaly_type: AnomalyType::VegetationEncroachment,
                location: *veg,
                severity,
                description: format!(
                    "Vegetation {:.2}m from structure at [{:.1},{:.1},{:.1}]",
                    dist, nearest_struct[0], nearest_struct[1], nearest_struct[2]
                ),
                confidence: 0.9,
            });
        }
    }
    anomalies
}

/// Detect construction deviation from BIM design model.
///
/// `as_built` points are the surveyed reality.
/// `design_points` are the BIM model vertices.
pub fn detect_construction_deviation(
    as_built: &[[f64; 3]],
    design_points: &[[f64; 3]],
    config: &AnomalyConfig,
) -> Vec<Anomaly> {
    let mut anomalies = Vec::new();

    for built_pt in as_built {
        let mut min_dist = f64::MAX;
        for design_pt in design_points {
            let dist = ((built_pt[0] - design_pt[0]).powi(2)
                + (built_pt[1] - design_pt[1]).powi(2)
                + (built_pt[2] - design_pt[2]).powi(2))
            .sqrt();
            if dist < min_dist {
                min_dist = dist;
            }
        }

        if min_dist > config.deviation_tolerance {
            let severity = (min_dist / (config.deviation_tolerance * 5.0)).min(1.0);
            anomalies.push(Anomaly {
                anomaly_type: AnomalyType::ConstructionDeviation,
                location: *built_pt,
                severity,
                description: format!("Deviation of {:.3}m from design", min_dist),
                confidence: 0.85,
            });
        }
    }
    anomalies
}

/// Statistical anomaly score using Z-score method.
/// Returns indices of points that deviate significantly from neighbors.
pub fn statistical_outlier_removal(
    points: &[[f64; 3]],
    k_neighbors: usize,
    std_multiplier: f64,
) -> Vec<usize> {
    if points.len() <= k_neighbors {
        return Vec::new();
    }

    // Compute mean distance to k nearest neighbors for each point
    let mut mean_distances: Vec<f64> = Vec::with_capacity(points.len());

    for (i, p) in points.iter().enumerate() {
        let mut dists: Vec<f64> = points
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, q)| {
                ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
            })
            .collect();
        dists.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let k = k_neighbors.min(dists.len());
        let mean_d: f64 = dists[..k].iter().sum::<f64>() / k as f64;
        mean_distances.push(mean_d);
    }

    // Compute global mean and std
    let global_mean = mean_distances.iter().sum::<f64>() / mean_distances.len() as f64;
    let variance = mean_distances
        .iter()
        .map(|d| (d - global_mean).powi(2))
        .sum::<f64>()
        / mean_distances.len() as f64;
    let std_dev = variance.sqrt();

    let threshold = global_mean + std_multiplier * std_dev;

    mean_distances
        .iter()
        .enumerate()
        .filter(|(_, d)| **d > threshold)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_deformation() {
        let epoch_a = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        let epoch_b = vec![[0.0, 0.0, 0.2], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        let config = AnomalyConfig {
            deformation_threshold: 0.05,
            min_confidence: 0.5,
            ..Default::default()
        };
        let anomalies = detect_deformation(&epoch_a, &epoch_b, &config);
        assert!(!anomalies.is_empty());
        assert_eq!(
            anomalies[0].anomaly_type,
            AnomalyType::StructuralDeformation
        );
    }

    #[test]
    fn test_detect_encroachment() {
        let structures = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
        let vegetation = vec![[0.5, 0.0, 0.0], [20.0, 0.0, 0.0]];
        let config = AnomalyConfig {
            encroachment_distance: 1.0,
            ..Default::default()
        };
        let anomalies = detect_encroachment(&structures, &vegetation, &config);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(
            anomalies[0].anomaly_type,
            AnomalyType::VegetationEncroachment
        );
    }

    #[test]
    fn test_detect_construction_deviation() {
        let as_built = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.5]]; // second pt off
        let design = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let config = AnomalyConfig {
            deviation_tolerance: 0.1,
            ..Default::default()
        };
        let anomalies = detect_construction_deviation(&as_built, &design, &config);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(
            anomalies[0].anomaly_type,
            AnomalyType::ConstructionDeviation
        );
    }

    #[test]
    fn test_statistical_outlier_removal() {
        let mut points: Vec<[f64; 3]> = (0..20).map(|i| [i as f64 * 0.1, 0.0, 0.0]).collect();
        // Add an outlier far away
        points.push([100.0, 100.0, 100.0]);
        let outliers = statistical_outlier_removal(&points, 5, 2.0);
        assert!(outliers.contains(&20)); // The outlier
    }

    #[test]
    fn test_anomaly_config_default() {
        let config = AnomalyConfig::default();
        assert!((config.deformation_threshold - 0.05).abs() < 1e-10);
        assert!((config.min_confidence - 0.7).abs() < 1e-10);
    }
}
