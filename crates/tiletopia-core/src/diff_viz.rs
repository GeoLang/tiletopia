//! Change detection visualization — temporal diffs rendered as heatmaps.
//!
//! Compares two point cloud epochs and generates visual change maps,
//! volume-change reports, and 4D time-slider metadata.

use serde::{Deserialize, Serialize};

/// A change event between two epochs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEvent {
    /// Grid cell center (x, y, z).
    pub center: [f64; 3],
    /// Height change (positive = growth, negative = removal).
    pub delta_height: f64,
    /// Volume change in cubic meters.
    pub delta_volume: f64,
    /// Classification of change.
    pub change_type: ChangeType,
    /// Confidence score (0.0–1.0).
    pub confidence: f64,
}

/// Type of detected change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    /// New structure appeared.
    Addition,
    /// Structure was removed.
    Removal,
    /// Structure height/shape changed.
    Modification,
    /// Ground level changed (erosion/deposition).
    TerrainChange,
    /// No significant change.
    NoChange,
}

/// A heatmap cell for visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeatmapCell {
    pub x: f64,
    pub y: f64,
    /// Normalized change magnitude (0.0–1.0).
    pub intensity: f64,
    /// RGBA color for rendering.
    pub color: [u8; 4],
}

/// Change detection report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeReport {
    pub epoch_a: String,
    pub epoch_b: String,
    pub total_area_m2: f64,
    pub changed_area_m2: f64,
    pub volume_added_m3: f64,
    pub volume_removed_m3: f64,
    pub events: Vec<ChangeEvent>,
    pub heatmap: Vec<HeatmapCell>,
}

/// Configuration for change detection.
#[derive(Debug, Clone)]
pub struct ChangeDetectionConfig {
    /// Grid cell size in meters.
    pub cell_size: f64,
    /// Minimum height change to register (meters).
    pub min_change_threshold: f64,
    /// Confidence threshold for reporting.
    pub confidence_threshold: f64,
}

impl Default for ChangeDetectionConfig {
    fn default() -> Self {
        Self {
            cell_size: 1.0,
            min_change_threshold: 0.3,
            confidence_threshold: 0.5,
        }
    }
}

/// A point with timestamp for temporal analysis.
#[derive(Debug, Clone)]
pub struct TemporalPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Detect changes between two point cloud epochs.
pub fn detect_changes(
    epoch_a: &[TemporalPoint],
    epoch_b: &[TemporalPoint],
    config: &ChangeDetectionConfig,
) -> ChangeReport {
    // Grid both epochs
    let grid_a = grid_points(epoch_a, config.cell_size);
    let grid_b = grid_points(epoch_b, config.cell_size);

    let mut events = Vec::new();
    let mut heatmap = Vec::new();
    let mut volume_added = 0.0;
    let mut volume_removed = 0.0;
    let mut changed_cells = 0usize;

    // Collect all cells from both epochs
    let all_cells: std::collections::HashSet<(i64, i64)> =
        grid_a.keys().chain(grid_b.keys()).copied().collect();
    let total_cells = all_cells.len();

    for cell_key in &all_cells {
        let stats_a = grid_a.get(cell_key);
        let stats_b = grid_b.get(cell_key);

        let (mean_a, count_a) = stats_a.map(|s| (s.mean_z, s.count)).unwrap_or((0.0, 0));
        let (mean_b, count_b) = stats_b.map(|s| (s.mean_z, s.count)).unwrap_or((0.0, 0));

        let delta_height = mean_b - mean_a;
        let cell_area = config.cell_size * config.cell_size;
        let delta_volume = delta_height * cell_area;

        // Determine change type
        let change_type = if count_a == 0 && count_b > 0 {
            ChangeType::Addition
        } else if count_a > 0 && count_b == 0 {
            ChangeType::Removal
        } else if delta_height.abs() > config.min_change_threshold {
            if delta_height.abs() < 1.0 {
                ChangeType::TerrainChange
            } else {
                ChangeType::Modification
            }
        } else {
            ChangeType::NoChange
        };

        // Confidence based on point density
        let density_factor = ((count_a + count_b) as f64 / 20.0).min(1.0);
        let confidence = density_factor * (delta_height.abs() / 5.0).min(1.0);

        if change_type != ChangeType::NoChange && confidence >= config.confidence_threshold {
            let center_x = cell_key.0 as f64 * config.cell_size + config.cell_size / 2.0;
            let center_y = cell_key.1 as f64 * config.cell_size + config.cell_size / 2.0;

            events.push(ChangeEvent {
                center: [center_x, center_y, mean_b],
                delta_height,
                delta_volume,
                change_type,
                confidence,
            });

            if delta_volume > 0.0 {
                volume_added += delta_volume;
            } else {
                volume_removed += delta_volume.abs();
            }
            changed_cells += 1;

            // Generate heatmap cell
            let intensity = (delta_height.abs() / 5.0).min(1.0);
            let color = change_to_color(change_type, intensity);
            heatmap.push(HeatmapCell {
                x: center_x,
                y: center_y,
                intensity,
                color,
            });
        }
    }

    let cell_area = config.cell_size * config.cell_size;
    ChangeReport {
        epoch_a: "epoch_a".to_string(),
        epoch_b: "epoch_b".to_string(),
        total_area_m2: total_cells as f64 * cell_area,
        changed_area_m2: changed_cells as f64 * cell_area,
        volume_added_m3: volume_added,
        volume_removed_m3: volume_removed,
        events,
        heatmap,
    }
}

/// Time-slider metadata for 4D replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeSliderEntry {
    pub timestamp: String,
    pub tileset_version: String,
    pub change_summary: Option<ChangeReport>,
}

/// Generate time-slider metadata from a sequence of epochs.
pub fn generate_time_slider(epochs: &[(String, Vec<TemporalPoint>)]) -> Vec<TimeSliderEntry> {
    let config = ChangeDetectionConfig::default();
    let mut entries = Vec::new();

    for (i, (timestamp, _points)) in epochs.iter().enumerate() {
        let change_summary = if i > 0 {
            Some(detect_changes(&epochs[i - 1].1, &epochs[i].1, &config))
        } else {
            None
        };

        entries.push(TimeSliderEntry {
            timestamp: timestamp.clone(),
            tileset_version: format!("v{}", i + 1),
            change_summary,
        });
    }

    entries
}

// --- Internal helpers ---

struct CellStats {
    mean_z: f64,
    count: usize,
}

fn grid_points(
    points: &[TemporalPoint],
    cell_size: f64,
) -> std::collections::HashMap<(i64, i64), CellStats> {
    use std::collections::HashMap;

    let mut cells: HashMap<(i64, i64), (f64, usize)> = HashMap::new();
    for p in points {
        let key = (
            (p.x / cell_size).floor() as i64,
            (p.y / cell_size).floor() as i64,
        );
        let entry = cells.entry(key).or_insert((0.0, 0));
        entry.0 += p.z;
        entry.1 += 1;
    }

    cells
        .into_iter()
        .map(|(key, (sum_z, count))| {
            (
                key,
                CellStats {
                    mean_z: sum_z / count as f64,
                    count,
                },
            )
        })
        .collect()
}

fn change_to_color(change_type: ChangeType, intensity: f64) -> [u8; 4] {
    let alpha = (intensity * 255.0) as u8;
    match change_type {
        ChangeType::Addition => [0, 255, 0, alpha],       // Green
        ChangeType::Removal => [255, 0, 0, alpha],        // Red
        ChangeType::Modification => [255, 165, 0, alpha], // Orange
        ChangeType::TerrainChange => [139, 69, 19, alpha], // Brown
        ChangeType::NoChange => [0, 0, 0, 0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_no_change() {
        let points: Vec<TemporalPoint> = (0..100)
            .map(|i| TemporalPoint {
                x: (i % 10) as f64,
                y: (i / 10) as f64,
                z: 1.0,
            })
            .collect();
        let report = detect_changes(&points, &points, &ChangeDetectionConfig::default());
        assert_eq!(report.events.len(), 0);
        assert_eq!(report.volume_added_m3, 0.0);
    }

    #[test]
    fn test_detect_addition() {
        let epoch_a = vec![];
        let epoch_b: Vec<TemporalPoint> = (0..50)
            .map(|i| TemporalPoint {
                x: (i % 5) as f64 * 0.5,
                y: (i / 5) as f64 * 0.5,
                z: 5.0,
            })
            .collect();
        let config = ChangeDetectionConfig {
            cell_size: 1.0,
            min_change_threshold: 0.1,
            confidence_threshold: 0.1,
        };
        let report = detect_changes(&epoch_a, &epoch_b, &config);
        assert!(!report.events.is_empty());
        assert!(
            report
                .events
                .iter()
                .any(|e| e.change_type == ChangeType::Addition)
        );
    }

    #[test]
    fn test_detect_removal() {
        let epoch_a: Vec<TemporalPoint> = (0..50)
            .map(|i| TemporalPoint {
                x: (i % 5) as f64 * 0.5,
                y: (i / 5) as f64 * 0.5,
                z: 5.0,
            })
            .collect();
        let epoch_b = vec![];
        let config = ChangeDetectionConfig {
            cell_size: 1.0,
            min_change_threshold: 0.1,
            confidence_threshold: 0.1,
        };
        let report = detect_changes(&epoch_a, &epoch_b, &config);
        assert!(!report.events.is_empty());
        assert!(
            report
                .events
                .iter()
                .any(|e| e.change_type == ChangeType::Removal)
        );
    }

    #[test]
    fn test_heatmap_generation() {
        let epoch_a: Vec<TemporalPoint> = (0..100)
            .map(|i| TemporalPoint {
                x: (i % 10) as f64 * 0.1,
                y: (i / 10) as f64 * 0.1,
                z: 1.0,
            })
            .collect();
        let epoch_b: Vec<TemporalPoint> = (0..100)
            .map(|i| TemporalPoint {
                x: (i % 10) as f64 * 0.1,
                y: (i / 10) as f64 * 0.1,
                z: if i < 50 { 1.0 } else { 4.0 },
            })
            .collect();
        let config = ChangeDetectionConfig {
            cell_size: 1.0,
            min_change_threshold: 0.3,
            confidence_threshold: 0.01,
        };
        let report = detect_changes(&epoch_a, &epoch_b, &config);
        assert!(!report.heatmap.is_empty());
        // All heatmap cells should have valid colors
        for cell in &report.heatmap {
            assert!(cell.intensity > 0.0);
            assert!(cell.intensity <= 1.0);
        }
    }

    #[test]
    fn test_time_slider() {
        let epoch1: Vec<TemporalPoint> = (0..20)
            .map(|i| TemporalPoint {
                x: i as f64,
                y: 0.0,
                z: 1.0,
            })
            .collect();
        let epoch2: Vec<TemporalPoint> = (0..20)
            .map(|i| TemporalPoint {
                x: i as f64,
                y: 0.0,
                z: 3.0,
            })
            .collect();
        let epochs = vec![
            ("2024-01-01".to_string(), epoch1),
            ("2024-06-01".to_string(), epoch2),
        ];
        let slider = generate_time_slider(&epochs);
        assert_eq!(slider.len(), 2);
        assert!(slider[0].change_summary.is_none());
        assert!(slider[1].change_summary.is_some());
    }
}
