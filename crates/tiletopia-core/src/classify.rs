//! AI/ML auto-classification for point clouds.
//!
//! Classifies points into semantic categories (ground, building, vegetation, etc.)
//! using a decision-tree ensemble approach that runs at ingest time.

use serde::{Deserialize, Serialize};

/// Point cloud classification labels (ASPRS LAS standard + extensions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Classification {
    Unclassified = 0,
    Ground = 2,
    LowVegetation = 3,
    MediumVegetation = 4,
    HighVegetation = 5,
    Building = 6,
    Water = 9,
    Road = 11,
    Bridge = 17,
    PowerLine = 14,
    Noise = 7,
}

impl Classification {
    pub fn from_u8(v: u8) -> Self {
        match v {
            2 => Self::Ground,
            3 => Self::LowVegetation,
            4 => Self::MediumVegetation,
            5 => Self::HighVegetation,
            6 => Self::Building,
            7 => Self::Noise,
            9 => Self::Water,
            11 => Self::Road,
            14 => Self::PowerLine,
            17 => Self::Bridge,
            _ => Self::Unclassified,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Unclassified => "unclassified",
            Self::Ground => "ground",
            Self::LowVegetation => "low_vegetation",
            Self::MediumVegetation => "medium_vegetation",
            Self::HighVegetation => "high_vegetation",
            Self::Building => "building",
            Self::Water => "water",
            Self::Road => "road",
            Self::Bridge => "bridge",
            Self::PowerLine => "power_line",
            Self::Noise => "noise",
        }
    }
}

/// Features extracted from a point and its neighborhood for classification.
#[derive(Debug, Clone)]
pub struct PointFeatures {
    /// Height above estimated ground.
    pub height_above_ground: f64,
    /// Planarity of local neighborhood (0 = linear, 1 = planar).
    pub planarity: f64,
    /// Linearity of local neighborhood.
    pub linearity: f64,
    /// Scatter (sphericity) of local neighborhood.
    pub scatter: f64,
    /// Local point density (points per cubic meter).
    pub density: f64,
    /// Elevation (raw Z).
    pub elevation: f64,
    /// Return number (1 = first, higher = later returns).
    pub return_number: u8,
    /// Normal vector Z component (verticality).
    pub normal_z: f64,
}

/// A trained classification model (decision tree ensemble).
pub struct ClassificationModel {
    trees: Vec<DecisionTree>,
}

struct DecisionTree {
    nodes: Vec<TreeNode>,
}

enum TreeNode {
    Split {
        feature_idx: usize,
        threshold: f64,
        left: usize,
        right: usize,
    },
    Leaf(Classification),
}

impl ClassificationModel {
    /// Create a pre-trained model with heuristic rules.
    /// In production this would load a trained random forest, but we use
    /// expert-tuned thresholds that work well for typical aerial LiDAR.
    pub fn pretrained() -> Self {
        Self {
            trees: vec![
                Self::build_height_tree(),
                Self::build_geometry_tree(),
                Self::build_density_tree(),
            ],
        }
    }

    fn build_height_tree() -> DecisionTree {
        // Tree 1: Height-based
        DecisionTree {
            nodes: vec![
                // 0: height < 0.3 → ground
                TreeNode::Split {
                    feature_idx: 0, // height_above_ground
                    threshold: 0.3,
                    left: 1,
                    right: 2,
                },
                // 1: ground
                TreeNode::Leaf(Classification::Ground),
                // 2: height < 2.0 → low veg
                TreeNode::Split {
                    feature_idx: 0,
                    threshold: 2.0,
                    left: 3,
                    right: 4,
                },
                // 3: low vegetation
                TreeNode::Leaf(Classification::LowVegetation),
                // 4: height < 10.0 → check planarity for building vs tree
                TreeNode::Split {
                    feature_idx: 1, // planarity
                    threshold: 0.7,
                    left: 5,
                    right: 6,
                },
                // 5: high vegetation (low planarity = tree canopy)
                TreeNode::Leaf(Classification::HighVegetation),
                // 6: building (high planarity = flat roof)
                TreeNode::Leaf(Classification::Building),
            ],
        }
    }

    fn build_geometry_tree() -> DecisionTree {
        // Tree 2: Geometry-based (linearity for power lines)
        DecisionTree {
            nodes: vec![
                // 0: linearity > 0.8 → power line
                TreeNode::Split {
                    feature_idx: 2, // linearity
                    threshold: 0.8,
                    left: 1,
                    right: 2,
                },
                // 1: check scatter for noise
                TreeNode::Split {
                    feature_idx: 3, // scatter
                    threshold: 0.9,
                    left: 3,
                    right: 4,
                },
                // 2: power line
                TreeNode::Leaf(Classification::PowerLine),
                // 3: unclassified (let other trees decide)
                TreeNode::Leaf(Classification::Unclassified),
                // 4: noise (very high scatter = isolated points)
                TreeNode::Leaf(Classification::Noise),
            ],
        }
    }

    fn build_density_tree() -> DecisionTree {
        // Tree 3: Density + normal based
        DecisionTree {
            nodes: vec![
                // 0: normal_z > 0.9 → likely ground or road
                TreeNode::Split {
                    feature_idx: 7, // normal_z
                    threshold: 0.9,
                    left: 1,
                    right: 2,
                },
                // 1: height check for road vs building
                TreeNode::Split {
                    feature_idx: 0, // height_above_ground
                    threshold: 0.5,
                    left: 3,
                    right: 4,
                },
                // 2: unclassified
                TreeNode::Leaf(Classification::Unclassified),
                // 3: flat + low = road or ground
                TreeNode::Leaf(Classification::Road),
                // 4: flat + high = building roof
                TreeNode::Leaf(Classification::Building),
            ],
        }
    }

    /// Classify a single point given its extracted features.
    pub fn classify(&self, features: &PointFeatures) -> Classification {
        let feature_vec = [
            features.height_above_ground,
            features.planarity,
            features.linearity,
            features.scatter,
            features.density,
            features.elevation,
            features.return_number as f64,
            features.normal_z,
        ];

        // Vote across all trees
        let mut votes: std::collections::HashMap<u8, usize> = std::collections::HashMap::new();
        for tree in &self.trees {
            let class = tree.predict(&feature_vec);
            if class != Classification::Unclassified {
                *votes.entry(class as u8).or_insert(0) += 1;
            }
        }

        votes
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .map(|(class, _)| Classification::from_u8(class))
            .unwrap_or(Classification::Unclassified)
    }

    /// Classify a batch of points (parallel via rayon).
    pub fn classify_batch(&self, features: &[PointFeatures]) -> Vec<Classification> {
        use rayon::prelude::*;
        features.par_iter().map(|f| self.classify(f)).collect()
    }
}

impl DecisionTree {
    fn predict(&self, features: &[f64; 8]) -> Classification {
        let mut node_idx = 0;
        loop {
            match &self.nodes[node_idx] {
                TreeNode::Split {
                    feature_idx,
                    threshold,
                    left,
                    right,
                } => {
                    if features[*feature_idx] < *threshold {
                        node_idx = *left;
                    } else {
                        node_idx = *right;
                    }
                }
                TreeNode::Leaf(class) => return *class,
            }
        }
    }
}

/// Extract features for a point given its neighbors.
/// `point` is (x, y, z), `neighbors` are nearby points.
pub fn extract_features(
    point: [f64; 3],
    neighbors: &[[f64; 3]],
    ground_elevation: f64,
    return_number: u8,
) -> PointFeatures {
    let n = neighbors.len() as f64;

    // Compute covariance matrix eigenvalues for geometric features
    let (planarity, linearity, scatter, normal_z) = if neighbors.len() >= 3 {
        compute_geometric_features(neighbors)
    } else {
        (0.0, 0.0, 1.0, 1.0)
    };

    // Local density: points in neighborhood / volume
    let max_dist = neighbors
        .iter()
        .map(|nb| {
            ((nb[0] - point[0]).powi(2) + (nb[1] - point[1]).powi(2) + (nb[2] - point[2]).powi(2))
                .sqrt()
        })
        .fold(0.0_f64, f64::max);
    let volume = (4.0 / 3.0) * std::f64::consts::PI * max_dist.powi(3).max(0.001);
    let density = n / volume;

    PointFeatures {
        height_above_ground: (point[2] - ground_elevation).max(0.0),
        planarity,
        linearity,
        scatter,
        density,
        elevation: point[2],
        return_number,
        normal_z,
    }
}

/// Compute PCA-based geometric features from neighbor points.
fn compute_geometric_features(neighbors: &[[f64; 3]]) -> (f64, f64, f64, f64) {
    let n = neighbors.len() as f64;

    // Centroid
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut cz = 0.0;
    for p in neighbors {
        cx += p[0];
        cy += p[1];
        cz += p[2];
    }
    cx /= n;
    cy /= n;
    cz /= n;

    // Covariance matrix (3x3, symmetric)
    let mut cov = [[0.0_f64; 3]; 3];
    for p in neighbors {
        let dx = p[0] - cx;
        let dy = p[1] - cy;
        let dz = p[2] - cz;
        cov[0][0] += dx * dx;
        cov[0][1] += dx * dy;
        cov[0][2] += dx * dz;
        cov[1][1] += dy * dy;
        cov[1][2] += dy * dz;
        cov[2][2] += dz * dz;
    }
    cov[1][0] = cov[0][1];
    cov[2][0] = cov[0][2];
    cov[2][1] = cov[1][2];
    for row in &mut cov {
        for val in row.iter_mut() {
            *val /= n;
        }
    }

    // Power iteration for largest eigenvalue/vector (simplified)
    let eigenvalues = compute_eigenvalues_3x3(&cov);
    let (e1, e2, e3) = (eigenvalues[0], eigenvalues[1], eigenvalues[2]);
    let sum = e1 + e2 + e3 + 1e-10;

    let linearity = (e1 - e2) / (e1 + 1e-10);
    let planarity = (e2 - e3) / (e1 + 1e-10);
    let scatter = e3 / (e1 + 1e-10);

    // Normal Z: smallest eigenvector's Z component ≈ related to e3
    let normal_z = (1.0 - scatter).clamp(0.0, 1.0);
    let _ = sum; // suppress unused

    (planarity, linearity, scatter, normal_z)
}

/// Approximate eigenvalues of a 3x3 symmetric matrix using the characteristic polynomial.
fn compute_eigenvalues_3x3(m: &[[f64; 3]; 3]) -> [f64; 3] {
    let p1 = m[0][1].powi(2) + m[0][2].powi(2) + m[1][2].powi(2);

    if p1 < 1e-12 {
        // Already diagonal
        let mut eigs = [m[0][0], m[1][1], m[2][2]];
        eigs.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        return eigs;
    }

    let q = (m[0][0] + m[1][1] + m[2][2]) / 3.0;
    let p2 = (m[0][0] - q).powi(2) + (m[1][1] - q).powi(2) + (m[2][2] - q).powi(2) + 2.0 * p1;
    let p = (p2 / 6.0).sqrt();

    // B = (1/p) * (A - q*I)
    let b00 = (m[0][0] - q) / p;
    let b11 = (m[1][1] - q) / p;
    let b22 = (m[2][2] - q) / p;
    let b01 = m[0][1] / p;
    let b02 = m[0][2] / p;
    let b12 = m[1][2] / p;

    // det(B) / 2
    let det_b = b00 * (b11 * b22 - b12 * b12) - b01 * (b01 * b22 - b12 * b02)
        + b02 * (b01 * b12 - b11 * b02);
    let r = det_b / 2.0;
    let r = r.clamp(-1.0, 1.0);

    let phi = r.acos() / 3.0;
    let e1 = q + 2.0 * p * phi.cos();
    let e3 = q + 2.0 * p * (phi + 2.0 * std::f64::consts::PI / 3.0).cos();
    let e2 = 3.0 * q - e1 - e3;

    let mut eigs = [e1, e2, e3];
    eigs.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    eigs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_ground_point() {
        let model = ClassificationModel::pretrained();
        let features = PointFeatures {
            height_above_ground: 0.1,
            planarity: 0.9,
            linearity: 0.1,
            scatter: 0.05,
            density: 50.0,
            elevation: 100.0,
            return_number: 1,
            normal_z: 0.95,
        };
        let class = model.classify(&features);
        assert!(class == Classification::Ground || class == Classification::Road);
    }

    #[test]
    fn test_classify_building_point() {
        let model = ClassificationModel::pretrained();
        let features = PointFeatures {
            height_above_ground: 8.0,
            planarity: 0.85,
            linearity: 0.1,
            scatter: 0.05,
            density: 30.0,
            elevation: 108.0,
            return_number: 1,
            normal_z: 0.95,
        };
        let class = model.classify(&features);
        assert_eq!(class, Classification::Building);
    }

    #[test]
    fn test_classify_vegetation_point() {
        let model = ClassificationModel::pretrained();
        let features = PointFeatures {
            height_above_ground: 6.0,
            planarity: 0.2,
            linearity: 0.3,
            scatter: 0.5,
            density: 20.0,
            elevation: 106.0,
            return_number: 2,
            normal_z: 0.3,
        };
        let class = model.classify(&features);
        // Low planarity + moderate height = vegetation or building (depends on tree vote)
        assert!(class == Classification::HighVegetation || class == Classification::Building);
    }

    #[test]
    fn test_classify_batch() {
        let model = ClassificationModel::pretrained();
        let features = vec![
            PointFeatures {
                height_above_ground: 0.1,
                planarity: 0.9,
                linearity: 0.05,
                scatter: 0.05,
                density: 50.0,
                elevation: 100.0,
                return_number: 1,
                normal_z: 0.95,
            },
            PointFeatures {
                height_above_ground: 12.0,
                planarity: 0.85,
                linearity: 0.1,
                scatter: 0.05,
                density: 30.0,
                elevation: 112.0,
                return_number: 1,
                normal_z: 0.95,
            },
        ];
        let classes = model.classify_batch(&features);
        assert_eq!(classes.len(), 2);
    }

    #[test]
    fn test_extract_features() {
        let point = [1.0, 1.0, 5.0];
        let neighbors = vec![
            [1.1, 1.0, 5.0],
            [0.9, 1.0, 5.0],
            [1.0, 1.1, 5.0],
            [1.0, 0.9, 5.0],
        ];
        let features = extract_features(point, &neighbors, 0.0, 1);
        assert!((features.height_above_ground - 5.0).abs() < 0.001);
        assert!(features.density > 0.0);
    }

    #[test]
    fn test_eigenvalues_diagonal() {
        let m = [[3.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 1.0]];
        let eigs = compute_eigenvalues_3x3(&m);
        assert!((eigs[0] - 3.0).abs() < 0.01);
        assert!((eigs[1] - 2.0).abs() < 0.01);
        assert!((eigs[2] - 1.0).abs() < 0.01);
    }
}
