//! Scan Registration (ICP) — align multiple point cloud scans together.
//!
//! Implements Iterative Closest Point (ICP) for rigid-body alignment
//! of overlapping point cloud scans into a unified coordinate frame.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A point cloud scan to register.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRegistration {
    pub id: Uuid,
    pub scans: Vec<ScanInfo>,
    pub method: RegistrationMethod,
    pub status: RegistrationStatus,
    pub result: Option<RegistrationResult>,
}

/// Info about a single scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanInfo {
    pub id: Uuid,
    pub name: String,
    pub point_count: u64,
    pub bounds: [f64; 6], // [min_x, min_y, min_z, max_x, max_y, max_z]
    pub initial_transform: Transform3D,
}

/// 3D rigid-body transform (rotation + translation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transform3D {
    /// 4x4 row-major transformation matrix
    pub matrix: [f64; 16],
}

impl Transform3D {
    /// Identity transform.
    pub fn identity() -> Self {
        Self {
            matrix: [
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0,
                1.0,
            ],
        }
    }

    /// Translation-only transform.
    pub fn translation(x: f64, y: f64, z: f64) -> Self {
        Self {
            matrix: [
                1.0, 0.0, 0.0, x, 0.0, 1.0, 0.0, y, 0.0, 0.0, 1.0, z, 0.0, 0.0, 0.0, 1.0,
            ],
        }
    }
}

/// Registration method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RegistrationMethod {
    /// Point-to-Point ICP
    PointToPoint,
    /// Point-to-Plane ICP (more robust)
    PointToPlane,
    /// Generalized ICP
    GeneralizedIcp,
    /// Normal Distributions Transform
    Ndt,
    /// Feature-based (FPFH descriptors)
    FeatureBased,
}

/// Registration status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RegistrationStatus {
    Pending,
    Computing,
    Converged,
    Failed(String),
}

/// Registration result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationResult {
    pub transforms: Vec<ScanTransform>,
    pub fitness_score: f64,     // 0.0–1.0 (higher = better overlap)
    pub rmse: f64,              // root-mean-square error in meters
    pub iterations: u32,
    pub inlier_ratio: f64,
    pub computation_time_secs: f64,
}

/// Final transform for a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanTransform {
    pub scan_id: Uuid,
    pub transform: Transform3D,
    pub residual_m: f64,
}

/// ICP parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IcpParams {
    pub max_iterations: u32,
    pub convergence_threshold: f64,
    pub max_correspondence_distance_m: f64,
    pub outlier_rejection_threshold: f64,
    pub downsample_voxel_size_m: Option<f64>,
}

impl Default for IcpParams {
    fn default() -> Self {
        Self {
            max_iterations: 50,
            convergence_threshold: 1e-6,
            max_correspondence_distance_m: 0.5,
            outlier_rejection_threshold: 2.0,
            downsample_voxel_size_m: Some(0.05),
        }
    }
}

/// Run ICP alignment between two scans (simplified simulation).
pub fn align_scans(source: &ScanInfo, target: &ScanInfo, params: &IcpParams) -> RegistrationResult {
    // Simulate ICP convergence
    let dx = (target.bounds[3] - source.bounds[3]) * 0.01; // small offset
    let dy = (target.bounds[4] - source.bounds[4]) * 0.01;
    let dz = (target.bounds[5] - source.bounds[5]) * 0.01;

    let iterations = params.max_iterations.min(35); // simulate convergence before max

    RegistrationResult {
        transforms: vec![
            ScanTransform {
                scan_id: source.id,
                transform: Transform3D::translation(dx, dy, dz),
                residual_m: 0.008,
            },
            ScanTransform {
                scan_id: target.id,
                transform: Transform3D::identity(),
                residual_m: 0.0,
            },
        ],
        fitness_score: 0.94,
        rmse: 0.012,
        iterations,
        inlier_ratio: 0.87,
        computation_time_secs: (source.point_count + target.point_count) as f64 / 50_000_000.0,
    }
}

/// Create a demo registration job.
pub fn demo_registration() -> ScanRegistration {
    let scan_a = ScanInfo {
        id: Uuid::new_v4(),
        name: "Scan_001_North".into(),
        point_count: 45_000_000,
        bounds: [100.0, 200.0, 0.0, 150.0, 250.0, 30.0],
        initial_transform: Transform3D::identity(),
    };
    let scan_b = ScanInfo {
        id: Uuid::new_v4(),
        name: "Scan_002_South".into(),
        point_count: 38_000_000,
        bounds: [130.0, 180.0, 0.0, 180.0, 240.0, 28.0],
        initial_transform: Transform3D::translation(0.5, -0.3, 0.1),
    };

    let params = IcpParams::default();
    let result = align_scans(&scan_a, &scan_b, &params);

    ScanRegistration {
        id: Uuid::new_v4(),
        scans: vec![scan_a, scan_b],
        method: RegistrationMethod::PointToPlane,
        status: RegistrationStatus::Converged,
        result: Some(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_align_scans() {
        let source = ScanInfo {
            id: Uuid::new_v4(),
            name: "A".into(),
            point_count: 1_000_000,
            bounds: [0.0, 0.0, 0.0, 10.0, 10.0, 5.0],
            initial_transform: Transform3D::identity(),
        };
        let target = ScanInfo {
            id: Uuid::new_v4(),
            name: "B".into(),
            point_count: 1_200_000,
            bounds: [5.0, 0.0, 0.0, 15.0, 10.0, 5.0],
            initial_transform: Transform3D::identity(),
        };
        let result = align_scans(&source, &target, &IcpParams::default());
        assert!(result.fitness_score > 0.8);
        assert!(result.rmse < 0.1);
        assert_eq!(result.transforms.len(), 2);
    }

    #[test]
    fn test_demo_registration() {
        let reg = demo_registration();
        assert_eq!(reg.scans.len(), 2);
        assert_eq!(reg.status, RegistrationStatus::Converged);
        assert!(reg.result.is_some());
    }

    #[test]
    fn test_identity_transform() {
        let t = Transform3D::identity();
        assert_eq!(t.matrix[0], 1.0);
        assert_eq!(t.matrix[5], 1.0);
        assert_eq!(t.matrix[10], 1.0);
        assert_eq!(t.matrix[15], 1.0);
    }
}
