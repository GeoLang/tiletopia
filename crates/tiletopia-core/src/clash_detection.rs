//! BIM clash detection — compare IFC model against as-built point cloud.

/// An element from the BIM model (simplified geometry).
#[derive(Debug, Clone)]
pub struct BimElement {
    pub id: String,
    pub element_type: BimElementType,
    pub bbox_min: [f64; 3],
    pub bbox_max: [f64; 3],
    pub vertices: Vec<[f64; 3]>,
}

/// Types of BIM elements.
#[derive(Debug, Clone, PartialEq)]
pub enum BimElementType {
    Wall,
    Column,
    Beam,
    Slab,
    Pipe,
    Duct,
    Equipment,
    Other(String),
}

/// A detected clash between BIM elements or between BIM and reality.
#[derive(Debug, Clone)]
pub struct Clash {
    pub clash_type: ClashType,
    pub element_a: String,
    pub element_b: Option<String>,
    pub location: [f64; 3],
    pub distance: f64,
    pub severity: ClashSeverity,
}

/// Type of clash.
#[derive(Debug, Clone, PartialEq)]
pub enum ClashType {
    /// Two BIM elements overlap
    HardClash,
    /// Elements are too close (clearance violation)
    SoftClash,
    /// As-built deviates from design
    DesignDeviation,
    /// BIM element missing from reality
    MissingElement,
    /// Reality has element not in BIM
    UnplannedElement,
}

/// Severity level.
#[derive(Debug, Clone, PartialEq, PartialOrd)]
pub enum ClashSeverity {
    Critical,
    Major,
    Minor,
    Info,
}

/// Configuration for clash detection.
#[derive(Debug, Clone)]
pub struct ClashConfig {
    pub hard_clash_tolerance: f64,
    pub soft_clash_clearance: f64,
    pub deviation_threshold: f64,
    pub missing_element_ratio: f64,
}

impl Default for ClashConfig {
    fn default() -> Self {
        Self {
            hard_clash_tolerance: 0.01, // 1cm overlap
            soft_clash_clearance: 0.1,  // 10cm minimum clearance
            deviation_threshold: 0.05,  // 5cm deviation
            missing_element_ratio: 0.3, // 30% points missing = element absent
        }
    }
}

/// Check if two bounding boxes overlap.
fn bbox_overlap(a_min: &[f64; 3], a_max: &[f64; 3], b_min: &[f64; 3], b_max: &[f64; 3]) -> bool {
    a_min[0] <= b_max[0]
        && a_max[0] >= b_min[0]
        && a_min[1] <= b_max[1]
        && a_max[1] >= b_min[1]
        && a_min[2] <= b_max[2]
        && a_max[2] >= b_min[2]
}

/// Compute minimum distance between two bounding boxes.
fn bbox_distance(a_min: &[f64; 3], a_max: &[f64; 3], b_min: &[f64; 3], b_max: &[f64; 3]) -> f64 {
    let mut dist_sq = 0.0;
    for i in 0..3 {
        if a_max[i] < b_min[i] {
            dist_sq += (b_min[i] - a_max[i]).powi(2);
        } else if b_max[i] < a_min[i] {
            dist_sq += (a_min[i] - b_max[i]).powi(2);
        }
    }
    dist_sq.sqrt()
}

/// Detect hard and soft clashes between BIM elements.
pub fn detect_element_clashes(elements: &[BimElement], config: &ClashConfig) -> Vec<Clash> {
    let mut clashes = Vec::new();

    for i in 0..elements.len() {
        for j in (i + 1)..elements.len() {
            let a = &elements[i];
            let b = &elements[j];

            if bbox_overlap(&a.bbox_min, &a.bbox_max, &b.bbox_min, &b.bbox_max) {
                clashes.push(Clash {
                    clash_type: ClashType::HardClash,
                    element_a: a.id.clone(),
                    element_b: Some(b.id.clone()),
                    location: [
                        (a.bbox_min[0].max(b.bbox_min[0]) + a.bbox_max[0].min(b.bbox_max[0])) / 2.0,
                        (a.bbox_min[1].max(b.bbox_min[1]) + a.bbox_max[1].min(b.bbox_max[1])) / 2.0,
                        (a.bbox_min[2].max(b.bbox_min[2]) + a.bbox_max[2].min(b.bbox_max[2])) / 2.0,
                    ],
                    distance: 0.0,
                    severity: ClashSeverity::Critical,
                });
            } else {
                let dist = bbox_distance(&a.bbox_min, &a.bbox_max, &b.bbox_min, &b.bbox_max);
                if dist < config.soft_clash_clearance {
                    clashes.push(Clash {
                        clash_type: ClashType::SoftClash,
                        element_a: a.id.clone(),
                        element_b: Some(b.id.clone()),
                        location: [
                            (a.bbox_max[0] + b.bbox_min[0]) / 2.0,
                            (a.bbox_max[1] + b.bbox_min[1]) / 2.0,
                            (a.bbox_max[2] + b.bbox_min[2]) / 2.0,
                        ],
                        distance: dist,
                        severity: if dist < config.hard_clash_tolerance {
                            ClashSeverity::Major
                        } else {
                            ClashSeverity::Minor
                        },
                    });
                }
            }
        }
    }
    clashes
}

/// Detect deviations between BIM elements and as-built point cloud.
pub fn detect_design_deviation(
    elements: &[BimElement],
    point_cloud: &[[f64; 3]],
    config: &ClashConfig,
) -> Vec<Clash> {
    let mut clashes = Vec::new();

    for element in elements {
        // Find points near this element's bounding box
        let expanded_min = [
            element.bbox_min[0] - config.deviation_threshold,
            element.bbox_min[1] - config.deviation_threshold,
            element.bbox_min[2] - config.deviation_threshold,
        ];
        let expanded_max = [
            element.bbox_max[0] + config.deviation_threshold,
            element.bbox_max[1] + config.deviation_threshold,
            element.bbox_max[2] + config.deviation_threshold,
        ];

        let nearby_points: Vec<&[f64; 3]> = point_cloud
            .iter()
            .filter(|p| {
                p[0] >= expanded_min[0]
                    && p[0] <= expanded_max[0]
                    && p[1] >= expanded_min[1]
                    && p[1] <= expanded_max[1]
                    && p[2] >= expanded_min[2]
                    && p[2] <= expanded_max[2]
            })
            .collect();

        if nearby_points.is_empty() {
            // Element might be missing from reality
            clashes.push(Clash {
                clash_type: ClashType::MissingElement,
                element_a: element.id.clone(),
                element_b: None,
                location: [
                    (element.bbox_min[0] + element.bbox_max[0]) / 2.0,
                    (element.bbox_min[1] + element.bbox_max[1]) / 2.0,
                    (element.bbox_min[2] + element.bbox_max[2]) / 2.0,
                ],
                distance: 0.0,
                severity: ClashSeverity::Major,
            });
            continue;
        }

        // Check deviation of points from element vertices
        for point in &nearby_points {
            let min_dist = element
                .vertices
                .iter()
                .map(|v| {
                    ((point[0] - v[0]).powi(2)
                        + (point[1] - v[1]).powi(2)
                        + (point[2] - v[2]).powi(2))
                    .sqrt()
                })
                .fold(f64::MAX, f64::min);

            if min_dist > config.deviation_threshold {
                clashes.push(Clash {
                    clash_type: ClashType::DesignDeviation,
                    element_a: element.id.clone(),
                    element_b: None,
                    location: **point,
                    distance: min_dist,
                    severity: if min_dist > config.deviation_threshold * 3.0 {
                        ClashSeverity::Major
                    } else {
                        ClashSeverity::Minor
                    },
                });
            }
        }
    }
    clashes
}

/// Summary report of clash detection.
#[derive(Debug, Clone)]
pub struct ClashReport {
    pub total_clashes: usize,
    pub hard_clashes: usize,
    pub soft_clashes: usize,
    pub deviations: usize,
    pub missing_elements: usize,
    pub critical_count: usize,
    pub elements_checked: usize,
}

/// Generate a summary report from clashes.
pub fn generate_clash_report(clashes: &[Clash], elements_checked: usize) -> ClashReport {
    ClashReport {
        total_clashes: clashes.len(),
        hard_clashes: clashes
            .iter()
            .filter(|c| c.clash_type == ClashType::HardClash)
            .count(),
        soft_clashes: clashes
            .iter()
            .filter(|c| c.clash_type == ClashType::SoftClash)
            .count(),
        deviations: clashes
            .iter()
            .filter(|c| c.clash_type == ClashType::DesignDeviation)
            .count(),
        missing_elements: clashes
            .iter()
            .filter(|c| c.clash_type == ClashType::MissingElement)
            .count(),
        critical_count: clashes
            .iter()
            .filter(|c| c.severity == ClashSeverity::Critical)
            .count(),
        elements_checked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hard_clash_detection() {
        let elements = vec![
            BimElement {
                id: "wall-1".into(),
                element_type: BimElementType::Wall,
                bbox_min: [0.0, 0.0, 0.0],
                bbox_max: [1.0, 0.2, 3.0],
                vertices: vec![[0.0, 0.0, 0.0], [1.0, 0.2, 3.0]],
            },
            BimElement {
                id: "pipe-1".into(),
                element_type: BimElementType::Pipe,
                bbox_min: [0.5, 0.0, 1.0],
                bbox_max: [0.6, 2.0, 1.1],
                vertices: vec![[0.5, 0.0, 1.0], [0.6, 2.0, 1.1]],
            },
        ];
        let config = ClashConfig::default();
        let clashes = detect_element_clashes(&elements, &config);
        assert_eq!(clashes.len(), 1);
        assert_eq!(clashes[0].clash_type, ClashType::HardClash);
    }

    #[test]
    fn test_soft_clash_detection() {
        let elements = vec![
            BimElement {
                id: "beam-1".into(),
                element_type: BimElementType::Beam,
                bbox_min: [0.0, 0.0, 0.0],
                bbox_max: [1.0, 0.2, 0.2],
                vertices: vec![],
            },
            BimElement {
                id: "duct-1".into(),
                element_type: BimElementType::Duct,
                bbox_min: [0.0, 0.25, 0.0], // 0.05m gap
                bbox_max: [1.0, 0.5, 0.3],
                vertices: vec![],
            },
        ];
        let config = ClashConfig::default();
        let clashes = detect_element_clashes(&elements, &config);
        assert_eq!(clashes.len(), 1);
        assert_eq!(clashes[0].clash_type, ClashType::SoftClash);
    }

    #[test]
    fn test_design_deviation() {
        let elements = vec![BimElement {
            id: "col-1".into(),
            element_type: BimElementType::Column,
            bbox_min: [0.0, 0.0, 0.0],
            bbox_max: [0.3, 0.3, 3.0],
            vertices: vec![[0.0, 0.0, 0.0], [0.3, 0.3, 3.0], [0.15, 0.15, 1.5]],
        }];
        // Point cloud with one deviated point (inside expanded bbox but far from vertices)
        let cloud = vec![[0.15, 0.15, 1.5], [0.25, 0.25, 0.5]]; // second is inside bbox but far from vertices
        let config = ClashConfig {
            deviation_threshold: 0.05,
            ..Default::default()
        };
        let clashes = detect_design_deviation(&elements, &cloud, &config);
        assert!(!clashes.is_empty());
    }

    #[test]
    fn test_missing_element() {
        let elements = vec![BimElement {
            id: "wall-99".into(),
            element_type: BimElementType::Wall,
            bbox_min: [100.0, 100.0, 0.0],
            bbox_max: [101.0, 100.2, 3.0],
            vertices: vec![],
        }];
        // No points near this element
        let cloud = vec![[0.0, 0.0, 0.0], [1.0, 1.0, 1.0]];
        let config = ClashConfig::default();
        let clashes = detect_design_deviation(&elements, &cloud, &config);
        assert_eq!(clashes.len(), 1);
        assert_eq!(clashes[0].clash_type, ClashType::MissingElement);
    }

    #[test]
    fn test_clash_report() {
        let clashes = vec![
            Clash {
                clash_type: ClashType::HardClash,
                element_a: "a".into(),
                element_b: Some("b".into()),
                location: [0.0; 3],
                distance: 0.0,
                severity: ClashSeverity::Critical,
            },
            Clash {
                clash_type: ClashType::SoftClash,
                element_a: "c".into(),
                element_b: Some("d".into()),
                location: [0.0; 3],
                distance: 0.05,
                severity: ClashSeverity::Minor,
            },
        ];
        let report = generate_clash_report(&clashes, 4);
        assert_eq!(report.total_clashes, 2);
        assert_eq!(report.hard_clashes, 1);
        assert_eq!(report.soft_clashes, 1);
        assert_eq!(report.critical_count, 1);
    }
}
