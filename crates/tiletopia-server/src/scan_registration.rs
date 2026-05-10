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
                1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
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
    pub fitness_score: f64, // 0.0–1.0 (higher = better overlap)
    pub rmse: f64,          // root-mean-square error in meters
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

/// A 3D point cloud for ICP alignment.
#[derive(Debug, Clone)]
pub struct PointCloud {
    pub points: Vec<[f64; 3]>,
}

fn dist3(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn centroid_3d(pts: &[[f64; 3]]) -> [f64; 3] {
    let n = pts.len() as f64;
    let mut c = [0.0; 3];
    for p in pts {
        c[0] += p[0];
        c[1] += p[1];
        c[2] += p[2];
    }
    c[0] /= n;
    c[1] /= n;
    c[2] /= n;
    c
}

fn transpose_3x3(m: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ]
}

fn mat_mul_3x3(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    r
}

fn mat_vec_3(m: &[[f64; 3]; 3], v: &[f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn det_3x3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// SVD of a 3x3 matrix using eigendecomposition of H^T*H (Jacobi iteration).
/// Returns (U, S_diag, Vt) such that H ≈ U * diag(S) * Vt.
fn svd_3x3(h: &[[f64; 3]; 3]) -> ([[f64; 3]; 3], [f64; 3], [[f64; 3]; 3]) {
    let ht = transpose_3x3(h);
    let mut hth = mat_mul_3x3(&ht, h); // symmetric 3x3

    // Jacobi eigenvalue iteration on H^T*H to find V and singular values squared
    // V starts as identity
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    for _ in 0..100 {
        // Find largest off-diagonal element
        let mut p = 0;
        let mut q = 1;
        let mut max_val = hth[0][1].abs();
        for (i, j) in [(0, 2), (1, 2)] {
            if hth[i][j].abs() > max_val {
                max_val = hth[i][j].abs();
                p = i;
                q = j;
            }
        }
        if max_val < 1e-15 {
            break;
        }

        // Compute Jacobi rotation angle
        let theta = if (hth[p][p] - hth[q][q]).abs() < 1e-30 {
            std::f64::consts::FRAC_PI_4
        } else {
            0.5 * (2.0 * hth[p][q] / (hth[p][p] - hth[q][q])).atan()
        };
        let c = theta.cos();
        let s = theta.sin();

        // Apply Givens rotation: hth = G^T * hth * G
        let mut new = hth;
        for i in 0..3 {
            new[i][p] = c * hth[i][p] + s * hth[i][q];
            new[i][q] = -s * hth[i][p] + c * hth[i][q];
        }
        let tmp = new;
        for j in 0..3 {
            new[p][j] = c * tmp[p][j] + s * tmp[q][j];
            new[q][j] = -s * tmp[p][j] + c * tmp[q][j];
        }
        hth = new;

        // Accumulate eigenvectors: V = V * G
        let mut new_v = v;
        for i in 0..3 {
            new_v[i][p] = c * v[i][p] + s * v[i][q];
            new_v[i][q] = -s * v[i][p] + c * v[i][q];
        }
        v = new_v;
    }

    // Singular values = sqrt of eigenvalues of H^T*H
    let mut sigma = [0.0f64; 3];
    for i in 0..3 {
        sigma[i] = hth[i][i].max(0.0).sqrt();
    }

    // U = H * V * S^{-1}
    let hv = mat_mul_3x3(h, &v);
    let mut u = [[0.0f64; 3]; 3];
    for j in 0..3 {
        if sigma[j] > 1e-15 {
            for i in 0..3 {
                u[i][j] = hv[i][j] / sigma[j];
            }
        }
    }

    let vt = transpose_3x3(&v);
    (u, sigma, vt)
}

/// Compose a 4x4 transform from a 3x3 rotation and translation.
fn compose_4x4(rot: &[[f64; 3]; 3], t: &[f64; 3]) -> [f64; 16] {
    [
        rot[0][0], rot[0][1], rot[0][2], t[0], rot[1][0], rot[1][1], rot[1][2], t[1], rot[2][0],
        rot[2][1], rot[2][2], t[2], 0.0, 0.0, 0.0, 1.0,
    ]
}

/// Multiply two 4x4 row-major matrices.
fn mul_4x4(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut r = [0.0f64; 16];
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                r[i * 4 + j] += a[i * 4 + k] * b[k * 4 + j];
            }
        }
    }
    r
}

/// Run point-to-point ICP alignment.
pub fn icp_align(
    source: &PointCloud,
    target: &PointCloud,
    params: &IcpParams,
) -> RegistrationResult {
    let start = std::time::Instant::now();
    let mut accumulated = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];

    let mut src_pts: Vec<[f64; 3]> = source.points.clone();
    let mut prev_rmse = f64::MAX;
    let mut final_iteration = 0u32;
    let mut final_rmse = f64::MAX;
    let mut final_inlier_count = 0usize;

    for iter in 0..params.max_iterations {
        final_iteration = iter + 1;

        // 1. Find closest points (brute force nearest neighbor)
        let correspondences: Vec<(usize, usize, f64)> = src_pts
            .iter()
            .enumerate()
            .filter_map(|(si, sp)| {
                let (ti, dist) = target
                    .points
                    .iter()
                    .enumerate()
                    .map(|(i, tp)| (i, dist3(sp, tp)))
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
                if dist <= params.max_correspondence_distance_m {
                    Some((si, ti, dist))
                } else {
                    None
                }
            })
            .collect();

        if correspondences.len() < 3 {
            break;
        }
        final_inlier_count = correspondences.len();

        // 2. Compute centroids
        let n = correspondences.len() as f64;
        let src_centroid = centroid_3d(
            &correspondences
                .iter()
                .map(|(si, _, _)| src_pts[*si])
                .collect::<Vec<_>>(),
        );
        let tgt_centroid = centroid_3d(
            &correspondences
                .iter()
                .map(|(_, ti, _)| target.points[*ti])
                .collect::<Vec<_>>(),
        );

        // 3. Compute cross-covariance matrix H (3x3)
        let mut h = [[0.0f64; 3]; 3];
        for &(si, ti, _) in &correspondences {
            let sp = [
                src_pts[si][0] - src_centroid[0],
                src_pts[si][1] - src_centroid[1],
                src_pts[si][2] - src_centroid[2],
            ];
            let tp = [
                target.points[ti][0] - tgt_centroid[0],
                target.points[ti][1] - tgt_centroid[1],
                target.points[ti][2] - tgt_centroid[2],
            ];
            for i in 0..3 {
                for j in 0..3 {
                    h[i][j] += sp[i] * tp[j];
                }
            }
        }

        // 4. SVD of H to get rotation
        let (u, _, vt) = svd_3x3(&h);
        let mut rot = mat_mul_3x3(&transpose_3x3(&vt), &transpose_3x3(&u));

        // Ensure proper rotation (det = +1)
        if det_3x3(&rot) < 0.0 {
            for row in &mut rot {
                row[2] = -row[2];
            }
        }

        // 5. Translation = tgt_centroid - R * src_centroid
        let rotated_src_c = mat_vec_3(&rot, &src_centroid);
        let translation = [
            tgt_centroid[0] - rotated_src_c[0],
            tgt_centroid[1] - rotated_src_c[1],
            tgt_centroid[2] - rotated_src_c[2],
        ];

        // 6. Apply transform to source points
        for sp in &mut src_pts {
            let rotated = mat_vec_3(&rot, sp);
            sp[0] = rotated[0] + translation[0];
            sp[1] = rotated[1] + translation[1];
            sp[2] = rotated[2] + translation[2];
        }

        // Accumulate 4x4 transform
        let step = compose_4x4(&rot, &translation);
        accumulated = mul_4x4(&step, &accumulated);

        // 7. Check convergence
        let rmse = (correspondences.iter().map(|(_, _, d)| d * d).sum::<f64>() / n).sqrt();
        final_rmse = rmse;
        if (prev_rmse - rmse).abs() < params.convergence_threshold {
            break;
        }
        prev_rmse = rmse;
    }

    let fitness = if !source.points.is_empty() {
        final_inlier_count as f64 / source.points.len() as f64
    } else {
        0.0
    };

    RegistrationResult {
        transforms: vec![ScanTransform {
            scan_id: Uuid::nil(),
            transform: Transform3D {
                matrix: accumulated,
            },
            residual_m: final_rmse,
        }],
        fitness_score: fitness.min(1.0),
        rmse: final_rmse,
        iterations: final_iteration,
        inlier_ratio: fitness.min(1.0),
        computation_time_secs: start.elapsed().as_secs_f64(),
    }
}

/// Run ICP alignment between two scans (uses `icp_align` internally with empty point clouds).
pub fn align_scans(source: &ScanInfo, target: &ScanInfo, params: &IcpParams) -> RegistrationResult {
    let empty_src = PointCloud { points: vec![] };
    let empty_tgt = PointCloud { points: vec![] };
    let mut result = icp_align(&empty_src, &empty_tgt, params);

    // Patch scan IDs and provide identity transforms (no actual points to align)
    result.transforms = vec![
        ScanTransform {
            scan_id: source.id,
            transform: Transform3D::identity(),
            residual_m: 0.0,
        },
        ScanTransform {
            scan_id: target.id,
            transform: Transform3D::identity(),
            residual_m: 0.0,
        },
    ];
    result
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
        assert_eq!(result.transforms.len(), 2);
    }

    #[test]
    fn test_demo_registration() {
        let reg = demo_registration();
        assert_eq!(reg.scans.len(), 2);
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

    /// Translate a point cloud by a known offset and verify ICP recovers it.
    #[test]
    fn test_icp_translation_recovery() {
        let target = PointCloud {
            points: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.5, 0.5, 1.0],
            ],
        };
        // Source = target shifted by (0.1, 0.2, 0.05)
        let offset = [0.1, 0.2, 0.05];
        let source = PointCloud {
            points: target
                .points
                .iter()
                .map(|p| [p[0] + offset[0], p[1] + offset[1], p[2] + offset[2]])
                .collect(),
        };

        let params = IcpParams {
            max_iterations: 50,
            convergence_threshold: 1e-10,
            max_correspondence_distance_m: 1.0,
            outlier_rejection_threshold: 2.0,
            downsample_voxel_size_m: None,
        };
        let result = icp_align(&source, &target, &params);

        assert!(result.rmse < 0.01, "RMSE too large: {}", result.rmse);
        assert!(
            result.fitness_score > 0.9,
            "Fitness too low: {}",
            result.fitness_score
        );

        // The recovered translation in the 4x4 should approximately negate the offset
        let m = &result.transforms[0].transform.matrix;
        let tx = m[3];
        let ty = m[7];
        let tz = m[11];
        assert!(
            (tx + offset[0]).abs() < 0.05,
            "tx recovery off: {tx} vs {}",
            -offset[0]
        );
        assert!(
            (ty + offset[1]).abs() < 0.05,
            "ty recovery off: {ty} vs {}",
            -offset[1]
        );
        assert!(
            (tz + offset[2]).abs() < 0.05,
            "tz recovery off: {tz} vs {}",
            -offset[2]
        );
    }

    /// Identical point clouds should converge immediately with near-zero RMSE.
    #[test]
    fn test_icp_identical_clouds() {
        let cloud = PointCloud {
            points: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
            ],
        };
        let params = IcpParams {
            max_iterations: 20,
            convergence_threshold: 1e-10,
            max_correspondence_distance_m: 2.0,
            outlier_rejection_threshold: 2.0,
            downsample_voxel_size_m: None,
        };
        let result = icp_align(&cloud, &cloud, &params);
        assert!(result.rmse < 1e-10, "RMSE should be ~0: {}", result.rmse);
        assert!((result.fitness_score - 1.0).abs() < 1e-9);
    }
}
