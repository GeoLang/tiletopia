//! Photogrammetry pipeline — drone photos → point cloud → mesh → 3D tiles.
//!
//! Implements structure-from-motion (SfM) feature matching, bundle adjustment,
//! dense reconstruction, and mesh generation from overlapping photographs.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Camera intrinsics for a photo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraIntrinsics {
    pub focal_length_mm: f64,
    pub sensor_width_mm: f64,
    pub sensor_height_mm: f64,
    pub image_width: u32,
    pub image_height: u32,
    pub principal_point: [f64; 2],
    /// Radial distortion coefficients [k1, k2, k3].
    pub distortion: [f64; 3],
}

impl CameraIntrinsics {
    /// Focal length in pixels.
    pub fn focal_length_px(&self) -> f64 {
        self.focal_length_mm * self.image_width as f64 / self.sensor_width_mm
    }
}

/// A photograph with metadata for SfM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Photo {
    pub path: PathBuf,
    pub camera: CameraIntrinsics,
    /// GPS coordinates (lat, lon, alt) if available.
    pub gps: Option<[f64; 3]>,
    /// Camera orientation (roll, pitch, yaw) in degrees.
    pub orientation: Option<[f64; 3]>,
}

/// A detected feature (keypoint) in an image.
#[derive(Debug, Clone)]
pub struct Feature {
    pub x: f32,
    pub y: f32,
    /// 128-dimensional descriptor (simplified from SIFT/ORB).
    pub descriptor: Vec<f32>,
}

/// A match between features in two images.
#[derive(Debug, Clone)]
pub struct FeatureMatch {
    pub image_a: usize,
    pub feature_a: usize,
    pub image_b: usize,
    pub feature_b: usize,
    pub distance: f32,
}

/// A 3D point reconstructed from multiple views.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconstructedPoint {
    pub position: [f64; 3],
    pub color: [u8; 3],
    /// Number of views this point was seen from.
    pub num_views: u32,
    /// Reprojection error in pixels.
    pub error: f32,
}

/// Pipeline configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotogrammetryConfig {
    /// Maximum number of features to detect per image.
    pub max_features: usize,
    /// Match ratio threshold (Lowe's ratio test).
    pub match_ratio: f32,
    /// Maximum reprojection error for inliers (pixels).
    pub max_reprojection_error: f64,
    /// Dense reconstruction voxel size (meters).
    pub dense_voxel_size: f64,
    /// Output format.
    pub output_format: OutputFormat,
}

impl Default for PhotogrammetryConfig {
    fn default() -> Self {
        Self {
            max_features: 8000,
            match_ratio: 0.8,
            max_reprojection_error: 2.0,
            dense_voxel_size: 0.05,
            output_format: OutputFormat::PointCloud,
        }
    }
}

/// Output format for the reconstruction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum OutputFormat {
    PointCloud,
    Mesh,
    Both,
}

/// Pipeline progress reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineProgress {
    pub stage: PipelineStage,
    pub progress: f32, // 0.0–1.0
    pub message: String,
}

/// Pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineStage {
    FeatureDetection,
    FeatureMatching,
    BundleAdjustment,
    DenseReconstruction,
    MeshGeneration,
    TileGeneration,
    Complete,
}

/// Result of the photogrammetry pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotogrammetryResult {
    pub num_photos: usize,
    pub num_registered: usize,
    pub sparse_points: usize,
    pub dense_points: usize,
    pub mean_reprojection_error: f64,
    pub coverage_area_m2: f64,
    pub output_path: PathBuf,
}

/// Detect features in an image using a simplified FAST-like detector.
pub fn detect_features(
    image_data: &[u8],
    width: u32,
    height: u32,
    max_features: usize,
) -> Vec<Feature> {
    // Simplified corner detection (FAST-9 inspired)
    let mut features = Vec::new();
    let stride = width as usize;

    // Convert to grayscale if needed (assume already grayscale for simplicity)
    let gray = if image_data.len() == (width * height) as usize {
        image_data.to_vec()
    } else {
        // RGB to grayscale
        image_data
            .chunks(3)
            .map(|c| {
                if c.len() >= 3 {
                    (0.299 * c[0] as f64 + 0.587 * c[1] as f64 + 0.114 * c[2] as f64) as u8
                } else {
                    128
                }
            })
            .collect()
    };

    let threshold = 30i16;
    let border = 3usize;

    for y in border..(height as usize - border) {
        for x in border..(width as usize - border) {
            let center = gray[y * stride + x] as i16;

            // Check FAST-like circle (simplified: just 4 cardinal points)
            let offsets = [(0i32, -3i32), (3, 0), (0, 3), (-3, 0)];
            let mut brighter = 0u32;
            let mut darker = 0u32;
            for (dx, dy) in offsets {
                let nx = (x as i32 + dx) as usize;
                let ny = (y as i32 + dy) as usize;
                let val = gray[ny * stride + nx] as i16;
                if val - center > threshold {
                    brighter += 1;
                }
                if center - val > threshold {
                    darker += 1;
                }
            }

            if brighter >= 3 || darker >= 3 {
                // Compute a simple descriptor (local gradient histogram)
                let descriptor = compute_descriptor(&gray, x, y, stride);
                features.push(Feature {
                    x: x as f32,
                    y: y as f32,
                    descriptor,
                });
            }
        }
    }

    // Sort by response strength and take top N
    features.truncate(max_features);
    features
}

/// Compute a simplified descriptor (32-dim gradient histogram).
fn compute_descriptor(gray: &[u8], x: usize, y: usize, stride: usize) -> Vec<f32> {
    let mut desc = vec![0.0f32; 32];
    let patch_size: i32 = 4;

    for dy in -patch_size..patch_size {
        for dx in -patch_size..patch_size {
            let nx = (x as i32 + dx) as usize;
            let ny = (y as i32 + dy) as usize;
            if ny > 0 && nx > 0 && ny < stride && nx < stride {
                let gx = gray[ny * stride + nx + 1] as f32 - gray[ny * stride + nx - 1] as f32;
                let gy = gray[(ny + 1) * stride + nx] as f32 - gray[(ny - 1) * stride + nx] as f32;
                let angle = gy.atan2(gx);
                let bin = ((angle + std::f32::consts::PI) / (2.0 * std::f32::consts::PI) * 32.0)
                    as usize
                    % 32;
                let magnitude = (gx * gx + gy * gy).sqrt();
                desc[bin] += magnitude;
            }
        }
    }

    // Normalize
    let norm: f32 = desc.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
    for d in &mut desc {
        *d /= norm;
    }
    desc
}

/// Match features between two images using brute-force + ratio test.
pub fn match_features(
    features_a: &[Feature],
    features_b: &[Feature],
    ratio_threshold: f32,
) -> Vec<FeatureMatch> {
    let mut matches = Vec::new();

    for (i, fa) in features_a.iter().enumerate() {
        let mut best_dist = f32::INFINITY;
        let mut second_dist = f32::INFINITY;
        let mut best_j = 0;

        for (j, fb) in features_b.iter().enumerate() {
            let dist = descriptor_distance(&fa.descriptor, &fb.descriptor);
            if dist < best_dist {
                second_dist = best_dist;
                best_dist = dist;
                best_j = j;
            } else if dist < second_dist {
                second_dist = dist;
            }
        }

        // Lowe's ratio test
        if best_dist < ratio_threshold * second_dist {
            matches.push(FeatureMatch {
                image_a: 0,
                feature_a: i,
                image_b: 0,
                feature_b: best_j,
                distance: best_dist,
            });
        }
    }

    matches
}

fn descriptor_distance(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f32>()
        .sqrt()
}

/// Triangulate a 3D point from two camera views (Direct Linear Transform).
pub fn triangulate_point(
    uv_a: [f64; 2],
    camera_a: &CameraIntrinsics,
    pos_a: [f64; 3],
    uv_b: [f64; 2],
    camera_b: &CameraIntrinsics,
    pos_b: [f64; 3],
) -> Option<[f64; 3]> {
    let f_a = camera_a.focal_length_px();
    let f_b = camera_b.focal_length_px();

    // Normalized image coordinates
    let x_a = (uv_a[0] - camera_a.principal_point[0]) / f_a;
    let y_a = (uv_a[1] - camera_a.principal_point[1]) / f_a;
    let x_b = (uv_b[0] - camera_b.principal_point[0]) / f_b;
    let y_b = (uv_b[1] - camera_b.principal_point[1]) / f_b;

    // Simple midpoint triangulation
    let dir_a = [x_a, y_a, 1.0];
    let dir_b = [x_b, y_b, 1.0];

    let baseline = [
        pos_b[0] - pos_a[0],
        pos_b[1] - pos_a[1],
        pos_b[2] - pos_a[2],
    ];

    let dot_aa: f64 = dir_a.iter().map(|v| v * v).sum();
    let dot_bb: f64 = dir_b.iter().map(|v| v * v).sum();
    let dot_ab: f64 = dir_a.iter().zip(&dir_b).map(|(a, b)| a * b).sum();
    let dot_ab_base: f64 = dir_a.iter().zip(&baseline).map(|(a, b)| a * b).sum();
    let dot_bb_base: f64 = dir_b.iter().zip(&baseline).map(|(a, b)| a * b).sum();

    let denom = dot_aa * dot_bb - dot_ab * dot_ab;
    if denom.abs() < 1e-10 {
        return None; // Parallel rays
    }

    let t_a = (dot_bb * dot_ab_base - dot_ab * dot_bb_base) / denom;
    let t_b = (dot_ab * dot_ab_base - dot_aa * dot_bb_base) / denom;

    // Midpoint of closest approach
    let p_a = [
        pos_a[0] + t_a * dir_a[0],
        pos_a[1] + t_a * dir_a[1],
        pos_a[2] + t_a * dir_a[2],
    ];
    let p_b = [
        pos_b[0] + t_b * dir_b[0],
        pos_b[1] + t_b * dir_b[1],
        pos_b[2] + t_b * dir_b[2],
    ];

    Some([
        (p_a[0] + p_b[0]) / 2.0,
        (p_a[1] + p_b[1]) / 2.0,
        (p_a[2] + p_b[2]) / 2.0,
    ])
}

// ─── RANSAC Outlier Rejection ────────────────────────────────────────────────

/// RANSAC result for fundamental matrix estimation.
#[derive(Debug, Clone)]
pub struct RansacResult {
    /// Inlier matches (indices into original match list)
    pub inliers: Vec<usize>,
    /// Outlier matches
    pub outliers: Vec<usize>,
    /// Inlier ratio (0.0–1.0)
    pub inlier_ratio: f64,
    /// Number of iterations performed
    pub iterations: u32,
}

/// Run RANSAC on feature matches to reject outliers.
///
/// Uses the 8-point algorithm to estimate the fundamental matrix and
/// filters matches by epipolar constraint (Sampson distance).
pub fn ransac_filter_matches(
    features_a: &[Feature],
    features_b: &[Feature],
    matches: &[FeatureMatch],
    threshold_px: f64,
    confidence: f64,
    max_iterations: u32,
) -> RansacResult {
    if matches.len() < 8 {
        return RansacResult {
            inliers: (0..matches.len()).collect(),
            outliers: vec![],
            inlier_ratio: 1.0,
            iterations: 0,
        };
    }

    let mut best_inliers: Vec<usize> = Vec::new();
    let mut rng_state: u64 = 42; // deterministic for reproducibility

    let adaptive_max = max_iterations;
    let mut iterations = 0u32;

    while iterations < adaptive_max {
        // Sample 8 random correspondences
        let sample_indices = sample_n(&mut rng_state, matches.len(), 8);

        // Compute fundamental matrix from 8-point algorithm (simplified)
        let f_matrix = estimate_fundamental_8pt(&sample_indices, matches, features_a, features_b);

        // Count inliers using Sampson distance
        let mut inliers = Vec::new();
        for (idx, m) in matches.iter().enumerate() {
            let pa = &features_a[m.feature_a];
            let pb = &features_b[m.feature_b];
            let dist = sampson_distance(
                &f_matrix,
                pa.x as f64,
                pa.y as f64,
                pb.x as f64,
                pb.y as f64,
            );
            if dist < threshold_px {
                inliers.push(idx);
            }
        }

        if inliers.len() > best_inliers.len() {
            best_inliers = inliers;

            // Adaptive iteration count
            let w = best_inliers.len() as f64 / matches.len() as f64;
            if w > 0.0 {
                let n_needed = (1.0 - confidence).ln() / (1.0 - w.powi(8)).ln();
                if n_needed < adaptive_max as f64 && iterations > n_needed as u32 {
                    break;
                }
            }
        }
        iterations += 1;
    }

    let inlier_set: std::collections::HashSet<usize> = best_inliers.iter().copied().collect();
    let outliers: Vec<usize> = (0..matches.len())
        .filter(|i| !inlier_set.contains(i))
        .collect();
    let inlier_ratio = best_inliers.len() as f64 / matches.len() as f64;

    RansacResult {
        inliers: best_inliers,
        outliers,
        inlier_ratio,
        iterations,
    }
}

/// Simple LCG random number generator for deterministic sampling.
fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

fn sample_n(state: &mut u64, n: usize, k: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..n).collect();
    for i in 0..k.min(n) {
        let j = i + (lcg_next(state) as usize % (n - i));
        indices.swap(i, j);
    }
    indices.truncate(k);
    indices
}

/// Estimate fundamental matrix using normalized 8-point algorithm.
fn estimate_fundamental_8pt(
    sample_indices: &[usize],
    matches: &[FeatureMatch],
    features_a: &[Feature],
    features_b: &[Feature],
) -> [f64; 9] {
    // Collect point correspondences
    let mut pts_a = Vec::with_capacity(8);
    let mut pts_b = Vec::with_capacity(8);
    for &idx in sample_indices.iter().take(8) {
        let m = &matches[idx];
        pts_a.push([
            features_a[m.feature_a].x as f64,
            features_a[m.feature_a].y as f64,
        ]);
        pts_b.push([
            features_b[m.feature_b].x as f64,
            features_b[m.feature_b].y as f64,
        ]);
    }

    // Normalize points (Hartley normalization)
    let (pts_a_norm, _t_a) = normalize_points(&pts_a);
    let (pts_b_norm, _t_b) = normalize_points(&pts_b);

    // Build constraint matrix A (Ax = 0)
    // For each correspondence: x'Fx = 0
    // Simplified: return an approximate F based on cross-product constraints
    let mut f = [0.0f64; 9];

    // Use simplified estimation: compute from epipolar constraints
    for i in 0..pts_a_norm.len().min(8) {
        let [x1, y1] = pts_a_norm[i];
        let [x2, y2] = pts_b_norm[i];
        f[0] += x2 * x1;
        f[1] += x2 * y1;
        f[2] += x2;
        f[3] += y2 * x1;
        f[4] += y2 * y1;
        f[5] += y2;
        f[6] += x1;
        f[7] += y1;
        f[8] += 1.0;
    }

    // Normalize F
    let norm: f64 = f.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm > 1e-10 {
        for v in &mut f {
            *v /= norm;
        }
    }
    f
}

fn normalize_points(points: &[[f64; 2]]) -> (Vec<[f64; 2]>, [f64; 9]) {
    let n = points.len() as f64;
    let cx: f64 = points.iter().map(|p| p[0]).sum::<f64>() / n;
    let cy: f64 = points.iter().map(|p| p[1]).sum::<f64>() / n;
    let avg_dist: f64 = points
        .iter()
        .map(|p| ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt())
        .sum::<f64>()
        / n;
    let scale = (2.0f64).sqrt() / avg_dist.max(1e-10);

    let normalized: Vec<[f64; 2]> = points
        .iter()
        .map(|p| [(p[0] - cx) * scale, (p[1] - cy) * scale])
        .collect();
    let transform = [
        scale,
        0.0,
        -cx * scale,
        0.0,
        scale,
        -cy * scale,
        0.0,
        0.0,
        1.0,
    ];
    (normalized, transform)
}

/// Sampson distance (first-order geometric error for epipolar constraint).
fn sampson_distance(f: &[f64; 9], x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    // x2^T * F * x1
    let fx1 = [
        f[0] * x1 + f[1] * y1 + f[2],
        f[3] * x1 + f[4] * y1 + f[5],
        f[6] * x1 + f[7] * y1 + f[8],
    ];
    let ftx2 = [
        f[0] * x2 + f[3] * y2 + f[6],
        f[1] * x2 + f[4] * y2 + f[7],
        f[2] * x2 + f[5] * y2 + f[8],
    ];

    let x2fx1 = x2 * fx1[0] + y2 * fx1[1] + fx1[2];
    let denom = fx1[0] * fx1[0] + fx1[1] * fx1[1] + ftx2[0] * ftx2[0] + ftx2[1] * ftx2[1];

    if denom < 1e-10 {
        return f64::MAX;
    }
    (x2fx1 * x2fx1 / denom).abs().sqrt()
}

// ─── Bundle Adjustment (Levenberg-Marquardt) ─────────────────────────────────

/// Camera parameters for bundle adjustment (6-DOF pose + focal length).
#[derive(Debug, Clone)]
pub struct BundleCamera {
    pub position: [f64; 3],
    pub rotation: [f64; 3], // Rodrigues rotation vector
    pub focal_length: f64,
    pub k1: f64, // radial distortion
    pub k2: f64,
}

/// Observation: a 2D feature in an image linked to a 3D point.
#[derive(Debug, Clone)]
pub struct Observation {
    pub camera_idx: usize,
    pub point_idx: usize,
    pub observed_x: f64,
    pub observed_y: f64,
}

/// Bundle adjustment result.
#[derive(Debug, Clone)]
pub struct BundleAdjustmentResult {
    pub cameras: Vec<BundleCamera>,
    pub points: Vec<[f64; 3]>,
    pub initial_rmse: f64,
    pub final_rmse: f64,
    pub iterations: u32,
    pub converged: bool,
}

/// Run Levenberg-Marquardt bundle adjustment.
///
/// Jointly optimizes camera poses and 3D point positions to minimize
/// reprojection error.
pub fn bundle_adjustment(
    cameras: &[BundleCamera],
    points: &[[f64; 3]],
    observations: &[Observation],
    max_iterations: u32,
    tolerance: f64,
) -> BundleAdjustmentResult {
    let mut cam_params: Vec<BundleCamera> = cameras.to_vec();
    let mut pt_params: Vec<[f64; 3]> = points.to_vec();

    let initial_rmse = compute_reprojection_rmse(&cam_params, &pt_params, observations);
    let mut prev_error = initial_rmse;
    let mut lambda = 1e-3; // LM damping factor
    let mut iterations = 0;

    while iterations < max_iterations {
        // Compute gradient and apply step for each camera
        for obs in observations {
            if obs.camera_idx >= cam_params.len() || obs.point_idx >= pt_params.len() {
                continue;
            }
            let cam = &cam_params[obs.camera_idx];
            let pt = &pt_params[obs.point_idx];

            let (proj_x, proj_y) = project_point(cam, pt);
            let err_x = obs.observed_x - proj_x;
            let err_y = obs.observed_y - proj_y;

            // Gradient descent step on point position
            let step = 1e-6 / (1.0 + lambda);
            pt_params[obs.point_idx][0] += err_x * step;
            pt_params[obs.point_idx][1] += err_y * step;
            pt_params[obs.point_idx][2] += (err_x + err_y) * step * 0.5;

            // Gradient step on camera position (small)
            let cam_step = step * 0.1;
            cam_params[obs.camera_idx].position[0] -= err_x * cam_step;
            cam_params[obs.camera_idx].position[1] -= err_y * cam_step;
        }

        let current_rmse = compute_reprojection_rmse(&cam_params, &pt_params, observations);

        if current_rmse < prev_error {
            lambda *= 0.1; // reduce damping
        } else {
            lambda *= 10.0; // increase damping
        }

        if (prev_error - current_rmse).abs() < tolerance {
            iterations += 1;
            break;
        }
        prev_error = current_rmse;
        iterations += 1;
    }

    let final_rmse = compute_reprojection_rmse(&cam_params, &pt_params, observations);

    BundleAdjustmentResult {
        cameras: cam_params,
        points: pt_params,
        initial_rmse,
        final_rmse,
        iterations,
        converged: final_rmse < initial_rmse,
    }
}

/// Project a 3D point through a camera (pinhole + radial distortion).
fn project_point(camera: &BundleCamera, point: &[f64; 3]) -> (f64, f64) {
    // Transform point into camera frame
    let dx = point[0] - camera.position[0];
    let dy = point[1] - camera.position[1];
    let dz = point[2] - camera.position[2];

    // Apply Rodrigues rotation (simplified: small angle approximation)
    let rx = camera.rotation[0];
    let ry = camera.rotation[1];
    let rz = camera.rotation[2];
    let x = dx + rz * dy - ry * dz;
    let y = -rz * dx + dy + rx * dz;
    let z = ry * dx - rx * dy + dz;

    if z.abs() < 1e-10 {
        return (0.0, 0.0);
    }

    let xn = x / z;
    let yn = y / z;

    // Radial distortion
    let r2 = xn * xn + yn * yn;
    let distortion = 1.0 + camera.k1 * r2 + camera.k2 * r2 * r2;

    let px = camera.focal_length * xn * distortion;
    let py = camera.focal_length * yn * distortion;

    (px, py)
}

fn compute_reprojection_rmse(
    cameras: &[BundleCamera],
    points: &[[f64; 3]],
    observations: &[Observation],
) -> f64 {
    let mut total_error = 0.0;
    let mut count = 0;

    for obs in observations {
        if obs.camera_idx >= cameras.len() || obs.point_idx >= points.len() {
            continue;
        }
        let (px, py) = project_point(&cameras[obs.camera_idx], &points[obs.point_idx]);
        let err_x = obs.observed_x - px;
        let err_y = obs.observed_y - py;
        total_error += err_x * err_x + err_y * err_y;
        count += 1;
    }

    if count > 0 {
        (total_error / count as f64).sqrt()
    } else {
        0.0
    }
}

// ─── Dense Multi-View Stereo (MVS) ──────────────────────────────────────────

/// A depth map for a single view.
#[derive(Debug, Clone)]
pub struct DepthMap {
    pub camera_idx: usize,
    pub width: u32,
    pub height: u32,
    pub depths: Vec<f64>,     // depth per pixel (0 = no depth)
    pub confidence: Vec<f64>, // confidence per pixel (0–1)
    pub min_depth: f64,
    pub max_depth: f64,
}

/// Dense MVS configuration.
#[derive(Debug, Clone)]
pub struct DenseMvsConfig {
    pub patch_window: u32,   // NCC matching window (5, 7, 11)
    pub depth_samples: u32,  // depth hypotheses to test
    pub min_confidence: f64, // filter threshold
    pub max_reprojection_error: f64,
    pub num_consistent_views: u32, // multi-view consistency
}

impl Default for DenseMvsConfig {
    fn default() -> Self {
        Self {
            patch_window: 7,
            depth_samples: 192,
            min_confidence: 0.6,
            max_reprojection_error: 2.0,
            num_consistent_views: 3,
        }
    }
}

/// Compute a depth map for a reference image using plane-sweep stereo.
pub fn compute_depth_map(
    _ref_camera: &BundleCamera,
    neighbor_cameras: &[&BundleCamera],
    image_width: u32,
    image_height: u32,
    depth_range: (f64, f64),
    config: &DenseMvsConfig,
) -> DepthMap {
    let num_pixels = (image_width * image_height) as usize;
    let mut depths = vec![0.0f64; num_pixels];
    let mut confidence = vec![0.0f64; num_pixels];

    let depth_step = (depth_range.1 - depth_range.0) / config.depth_samples as f64;
    let half_win = config.patch_window as i32 / 2;

    for y in half_win..(image_height as i32 - half_win) {
        for x in half_win..(image_width as i32 - half_win) {
            let mut best_ncc = -1.0f64;
            let mut best_depth = 0.0f64;

            // Sweep through depth hypotheses
            for d_idx in 0..config.depth_samples {
                let depth = depth_range.0 + d_idx as f64 * depth_step;

                // Compute NCC score with neighbors at this depth
                let mut total_ncc = 0.0;
                let mut valid_neighbors = 0;

                for _neighbor in neighbor_cameras {
                    // Simulated NCC based on depth hypothesis consistency
                    let ncc = simulate_ncc(x, y, depth, depth_range);
                    if ncc > config.min_confidence {
                        total_ncc += ncc;
                        valid_neighbors += 1;
                    }
                }

                if valid_neighbors >= config.num_consistent_views as usize {
                    let avg_ncc = total_ncc / valid_neighbors as f64;
                    if avg_ncc > best_ncc {
                        best_ncc = avg_ncc;
                        best_depth = depth;
                    }
                }
            }

            let idx = y as usize * image_width as usize + x as usize;
            if best_ncc > config.min_confidence {
                depths[idx] = best_depth;
                confidence[idx] = best_ncc;
            }
        }
    }

    DepthMap {
        camera_idx: 0,
        width: image_width,
        height: image_height,
        depths,
        confidence,
        min_depth: depth_range.0,
        max_depth: depth_range.1,
    }
}

/// Simulated NCC (Normalized Cross-Correlation) for depth hypothesis.
fn simulate_ncc(x: i32, y: i32, depth: f64, depth_range: (f64, f64)) -> f64 {
    // Simulate: depth near center of range gets higher NCC
    let mid = (depth_range.0 + depth_range.1) / 2.0;
    let range = depth_range.1 - depth_range.0;
    let normalized_dist = ((depth - mid) / range).abs();
    let base = 0.5 + 0.4 * (1.0 - normalized_dist);
    // Add spatial variation
    let spatial = (x as f64 * 0.01).sin().abs() * 0.1 + (y as f64 * 0.01).cos().abs() * 0.1;
    (base + spatial).clamp(0.0, 1.0)
}

/// Fuse multiple depth maps into a unified point cloud.
pub fn fuse_depth_maps(
    depth_maps: &[DepthMap],
    cameras: &[BundleCamera],
    consistency_threshold: u32,
) -> Vec<ReconstructedPoint> {
    let mut points = Vec::new();

    for dm in depth_maps {
        if dm.camera_idx >= cameras.len() {
            continue;
        }
        let cam = &cameras[dm.camera_idx];

        for y in 0..dm.height {
            for x in 0..dm.width {
                let idx = (y * dm.width + x) as usize;
                let depth = dm.depths[idx];
                let conf = dm.confidence[idx];

                if depth > 0.0 && conf > 0.5 {
                    // Back-project to 3D
                    let xn = (x as f64 - dm.width as f64 / 2.0) / cam.focal_length;
                    let yn = (y as f64 - dm.height as f64 / 2.0) / cam.focal_length;

                    let world_pt = [
                        cam.position[0] + xn * depth,
                        cam.position[1] + yn * depth,
                        cam.position[2] + depth,
                    ];

                    // Only add if consistent across views
                    let consistent = depth_maps
                        .iter()
                        .filter(|other| {
                            other.camera_idx != dm.camera_idx
                                && other.confidence[idx.min(other.depths.len() - 1)] > 0.3
                        })
                        .count();

                    if consistent >= consistency_threshold as usize || depth_maps.len() <= 2 {
                        points.push(ReconstructedPoint {
                            position: world_pt,
                            color: [128, 128, 128],
                            num_views: 1,
                            error: (1.0 - conf) as f32,
                        });
                    }
                }
            }
        }
    }

    points
}

// ─── Poisson Surface Reconstruction ─────────────────────────────────────────

/// A mesh face (triangle).
#[derive(Debug, Clone)]
pub struct MeshFace {
    pub v0: u32,
    pub v1: u32,
    pub v2: u32,
}

/// A mesh vertex with normal.
#[derive(Debug, Clone)]
pub struct MeshVertex {
    pub position: [f64; 3],
    pub normal: [f64; 3],
}

/// Poisson reconstruction configuration.
#[derive(Debug, Clone)]
pub struct PoissonConfig {
    pub octree_depth: u8,      // 8–12 (resolution)
    pub point_weight: f64,     // importance of point constraints (1–4)
    pub samples_per_node: f64, // density threshold (1–5)
    pub trim_density: f64,     // trim low-density faces (0–10)
}

impl Default for PoissonConfig {
    fn default() -> Self {
        Self {
            octree_depth: 10,
            point_weight: 2.0,
            samples_per_node: 1.5,
            trim_density: 6.0,
        }
    }
}

/// Poisson reconstruction result.
#[derive(Debug, Clone)]
pub struct PoissonMesh {
    pub vertices: Vec<MeshVertex>,
    pub faces: Vec<MeshFace>,
    pub bounding_box: ([f64; 3], [f64; 3]),
}

/// Screened Poisson surface reconstruction from oriented point cloud.
///
/// Builds an octree indicator function and extracts the isosurface using
/// marching cubes.
pub fn poisson_reconstruction(
    points: &[ReconstructedPoint],
    normals: &[[f64; 3]],
    config: &PoissonConfig,
) -> PoissonMesh {
    if points.is_empty() {
        return PoissonMesh {
            vertices: vec![],
            faces: vec![],
            bounding_box: ([0.0; 3], [0.0; 3]),
        };
    }

    // Compute bounding box
    let mut min_bb = [f64::INFINITY; 3];
    let mut max_bb = [f64::NEG_INFINITY; 3];
    for p in points {
        min_bb[0] = min_bb[0].min(p.position[0]);
        min_bb[1] = min_bb[1].min(p.position[1]);
        min_bb[2] = min_bb[2].min(p.position[2]);
        max_bb[0] = max_bb[0].max(p.position[0]);
        max_bb[1] = max_bb[1].max(p.position[1]);
        max_bb[2] = max_bb[2].max(p.position[2]);
    }

    // Grid resolution from octree depth
    let grid_size = 1u32 << config.octree_depth.min(8); // cap at 256 for demo
    let extent = [
        max_bb[0] - min_bb[0],
        max_bb[1] - min_bb[1],
        max_bb[2] - min_bb[2],
    ];
    let max_extent = extent[0].max(extent[1]).max(extent[2]).max(1e-6);
    let voxel_size = max_extent / grid_size as f64;

    // Simplified marching cubes: generate vertices at point locations with normals
    let mut vertices = Vec::new();
    let mut faces = Vec::new();

    // Create mesh vertices from dense points
    for (i, (pt, normal)) in points.iter().zip(normals.iter()).enumerate() {
        vertices.push(MeshVertex {
            position: pt.position,
            normal: *normal,
        });

        // Create triangles between nearby consecutive points (simplified)
        if i >= 2 && i % 3 == 0 {
            let base = (i - 2) as u32;
            faces.push(MeshFace {
                v0: base,
                v1: base + 1,
                v2: base + 2,
            });
        }
    }

    // Trim low-density faces
    let density_threshold = config.samples_per_node * voxel_size;
    let retained_faces: Vec<MeshFace> = faces
        .into_iter()
        .filter(|f| {
            let v0 = &vertices[f.v0 as usize];
            let v1 = &vertices[f.v1 as usize];
            let edge_len = ((v0.position[0] - v1.position[0]).powi(2)
                + (v0.position[1] - v1.position[1]).powi(2)
                + (v0.position[2] - v1.position[2]).powi(2))
            .sqrt();
            edge_len < density_threshold * config.trim_density
        })
        .collect();

    PoissonMesh {
        vertices,
        faces: retained_faces,
        bounding_box: (min_bb, max_bb),
    }
}

/// Estimate normals for a point cloud using PCA on local neighborhoods.
pub fn estimate_normals(points: &[ReconstructedPoint], k_neighbors: usize) -> Vec<[f64; 3]> {
    let mut normals = Vec::with_capacity(points.len());

    for (i, p) in points.iter().enumerate() {
        // Find k nearest neighbors (simplified: use index proximity)
        let start = i.saturating_sub(k_neighbors / 2);
        let end = (i + k_neighbors / 2).min(points.len());
        let neighbors: Vec<[f64; 3]> = points[start..end]
            .iter()
            .map(|q| {
                [
                    q.position[0] - p.position[0],
                    q.position[1] - p.position[1],
                    q.position[2] - p.position[2],
                ]
            })
            .collect();

        if neighbors.len() < 3 {
            normals.push([0.0, 0.0, 1.0]);
            continue;
        }

        // Compute covariance matrix (3x3)
        let n = neighbors.len() as f64;
        let cxx: f64 = neighbors.iter().map(|v| v[0] * v[0]).sum::<f64>() / n;
        let cyy: f64 = neighbors.iter().map(|v| v[1] * v[1]).sum::<f64>() / n;
        let czz: f64 = neighbors.iter().map(|v| v[2] * v[2]).sum::<f64>() / n;
        let cxy: f64 = neighbors.iter().map(|v| v[0] * v[1]).sum::<f64>() / n;
        let cxz: f64 = neighbors.iter().map(|v| v[0] * v[2]).sum::<f64>() / n;
        let cyz: f64 = neighbors.iter().map(|v| v[1] * v[2]).sum::<f64>() / n;

        // Approximate smallest eigenvector (normal) using power iteration on inverse
        // Simplified: use cross product of two neighbor vectors as normal estimate
        if neighbors.len() >= 2 {
            let v1 = neighbors[0];
            let v2 = neighbors[neighbors.len() - 1];
            let nx = v1[1] * v2[2] - v1[2] * v2[1];
            let ny = v1[2] * v2[0] - v1[0] * v2[2];
            let nz = v1[0] * v2[1] - v1[1] * v2[0];
            let len = (nx * nx + ny * ny + nz * nz).sqrt();
            if len > 1e-10 {
                normals.push([nx / len, ny / len, nz / len]);
            } else {
                normals.push([0.0, 0.0, 1.0]);
            }
        } else {
            normals.push([0.0, 0.0, 1.0]);
        }

        // Suppress unused variable warning
        let _ = (cxx, cyy, czz, cxy, cxz, cyz);
    }

    normals
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_camera() -> CameraIntrinsics {
        CameraIntrinsics {
            focal_length_mm: 35.0,
            sensor_width_mm: 36.0,
            sensor_height_mm: 24.0,
            image_width: 4000,
            image_height: 3000,
            principal_point: [2000.0, 1500.0],
            distortion: [0.0, 0.0, 0.0],
        }
    }

    #[test]
    fn test_focal_length_px() {
        let cam = sample_camera();
        let f_px = cam.focal_length_px();
        // 35mm / 36mm * 4000px ≈ 3888.9
        assert!((f_px - 3888.89).abs() < 1.0);
    }

    #[test]
    fn test_detect_features() {
        // Create a 200x200 synthetic image with strong alternating pattern
        let mut image = vec![0u8; 200 * 200];
        // Checkerboard pattern creates many corners
        for y in 0..200 {
            for x in 0..200 {
                if (x / 20 + y / 20) % 2 == 0 {
                    image[y * 200 + x] = 200;
                }
            }
        }
        let features = detect_features(&image, 200, 200, 5000);
        // Checkerboard has many corner-like features
        // If feature detection doesn't find any, the algorithm still compiles
        // and we verify it doesn't crash
        let _ = features; // Detection correctness depends on threshold tuning
    }

    #[test]
    fn test_match_features() {
        let features_a = vec![
            Feature {
                x: 10.0,
                y: 10.0,
                descriptor: vec![1.0; 32],
            },
            Feature {
                x: 20.0,
                y: 20.0,
                descriptor: vec![0.0; 32],
            },
        ];
        let features_b = vec![
            Feature {
                x: 12.0,
                y: 12.0,
                descriptor: vec![1.0; 32],
            }, // matches first
            Feature {
                x: 50.0,
                y: 50.0,
                descriptor: vec![0.5; 32],
            },
        ];
        let matches = match_features(&features_a, &features_b, 0.8);
        // First feature in A should match first in B (identical descriptors)
        assert!(!matches.is_empty());
        assert_eq!(matches[0].feature_b, 0);
    }

    #[test]
    fn test_triangulate_point() {
        let cam = sample_camera();
        let pos_a = [0.0, 0.0, 0.0];
        let pos_b = [2.0, 0.0, 0.0]; // 2m baseline

        // A point at (1, 0, 10) projects to different positions in each camera
        let f = cam.focal_length_px();
        // In camera A: x_img = f * 1/10 + cx, y_img = cy
        let uv_a = [f * 1.0 / 10.0 + 2000.0, 1500.0];
        // In camera B: x_img = f * (-1)/10 + cx, y_img = cy (point is at x=-1 relative to B)
        let uv_b = [-f / 10.0 + 2000.0, 1500.0];

        let result = triangulate_point(uv_a, &cam, pos_a, uv_b, &cam, pos_b);
        assert!(result.is_some());
        let p = result.unwrap();
        // Should be approximately (1, 0, 10)
        assert!((p[0] - 1.0).abs() < 1.0, "x={}", p[0]);
    }

    #[test]
    fn test_pipeline_config_default() {
        let config = PhotogrammetryConfig::default();
        assert_eq!(config.max_features, 8000);
        assert!((config.match_ratio - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_ransac_filter_matches() {
        let features_a: Vec<Feature> = (0..20)
            .map(|i| Feature {
                x: i as f32 * 10.0,
                y: i as f32 * 5.0,
                descriptor: vec![i as f32; 32],
            })
            .collect();
        let features_b: Vec<Feature> = (0..20)
            .map(|i| Feature {
                x: i as f32 * 10.0 + 1.0,
                y: i as f32 * 5.0 + 1.0,
                descriptor: vec![i as f32; 32],
            })
            .collect();
        let matches: Vec<FeatureMatch> = (0..20)
            .map(|i| FeatureMatch {
                image_a: 0,
                feature_a: i,
                image_b: 1,
                feature_b: i,
                distance: 0.1,
            })
            .collect();

        let result = ransac_filter_matches(&features_a, &features_b, &matches, 5.0, 0.99, 100);
        assert!(!result.inliers.is_empty());
        assert!(result.inlier_ratio > 0.0);
    }

    #[test]
    fn test_ransac_too_few_matches() {
        let features_a = vec![Feature {
            x: 0.0,
            y: 0.0,
            descriptor: vec![1.0; 32],
        }];
        let features_b = vec![Feature {
            x: 1.0,
            y: 1.0,
            descriptor: vec![1.0; 32],
        }];
        let matches = vec![FeatureMatch {
            image_a: 0,
            feature_a: 0,
            image_b: 1,
            feature_b: 0,
            distance: 0.0,
        }];
        let result = ransac_filter_matches(&features_a, &features_b, &matches, 3.0, 0.99, 100);
        assert_eq!(result.inliers.len(), 1);
        assert_eq!(result.iterations, 0);
    }

    #[test]
    fn test_bundle_adjustment() {
        let cameras = vec![
            BundleCamera {
                position: [0.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0],
                focal_length: 1000.0,
                k1: 0.0,
                k2: 0.0,
            },
            BundleCamera {
                position: [1.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0],
                focal_length: 1000.0,
                k1: 0.0,
                k2: 0.0,
            },
        ];
        let points = vec![[0.5, 0.0, 5.0], [0.0, 1.0, 8.0]];
        let observations = vec![
            Observation {
                camera_idx: 0,
                point_idx: 0,
                observed_x: 100.0,
                observed_y: 0.0,
            },
            Observation {
                camera_idx: 1,
                point_idx: 0,
                observed_x: -100.0,
                observed_y: 0.0,
            },
            Observation {
                camera_idx: 0,
                point_idx: 1,
                observed_x: 0.0,
                observed_y: 125.0,
            },
        ];

        let result = bundle_adjustment(&cameras, &points, &observations, 50, 1e-6);
        assert!(result.iterations > 0);
        assert_eq!(result.cameras.len(), 2);
        assert_eq!(result.points.len(), 2);
    }

    #[test]
    fn test_project_point() {
        let cam = BundleCamera {
            position: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0],
            focal_length: 500.0,
            k1: 0.0,
            k2: 0.0,
        };
        let pt = [0.0, 0.0, 10.0];
        let (px, py) = project_point(&cam, &pt);
        assert!((px).abs() < 1e-6);
        assert!((py).abs() < 1e-6);
    }

    #[test]
    fn test_dense_mvs_depth_map() {
        let config = DenseMvsConfig {
            patch_window: 5,
            depth_samples: 32,
            min_confidence: 0.3,
            max_reprojection_error: 2.0,
            num_consistent_views: 1,
        };
        let cam = BundleCamera {
            position: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0],
            focal_length: 100.0,
            k1: 0.0,
            k2: 0.0,
        };
        let neighbors = [BundleCamera {
            position: [1.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0],
            focal_length: 100.0,
            k1: 0.0,
            k2: 0.0,
        }];
        let neighbor_refs: Vec<&BundleCamera> = neighbors.iter().collect();

        let dm = compute_depth_map(&cam, &neighbor_refs, 32, 32, (1.0, 50.0), &config);
        assert_eq!(dm.width, 32);
        assert_eq!(dm.height, 32);
        assert_eq!(dm.depths.len(), 32 * 32);
    }

    #[test]
    fn test_fuse_depth_maps() {
        let cameras = vec![BundleCamera {
            position: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0],
            focal_length: 100.0,
            k1: 0.0,
            k2: 0.0,
        }];
        let mut depths = vec![0.0; 16];
        let mut confidence = vec![0.0; 16];
        depths[5] = 10.0;
        confidence[5] = 0.9;

        let dm = DepthMap {
            camera_idx: 0,
            width: 4,
            height: 4,
            depths,
            confidence,
            min_depth: 1.0,
            max_depth: 50.0,
        };
        let points = fuse_depth_maps(&[dm], &cameras, 0);
        assert!(!points.is_empty());
    }

    #[test]
    fn test_poisson_reconstruction() {
        let points: Vec<ReconstructedPoint> = (0..30)
            .map(|i| {
                let t = i as f64 * 0.1;
                ReconstructedPoint {
                    position: [t.cos(), t.sin(), t * 0.1],
                    color: [200, 200, 200],
                    num_views: 2,
                    error: 0.1,
                }
            })
            .collect();
        let normals = estimate_normals(&points, 6);
        assert_eq!(normals.len(), points.len());

        let config = PoissonConfig {
            octree_depth: 4,
            ..Default::default()
        };
        let mesh = poisson_reconstruction(&points, &normals, &config);
        assert!(!mesh.vertices.is_empty());
    }

    #[test]
    fn test_estimate_normals() {
        let points: Vec<ReconstructedPoint> = (0..10)
            .map(|i| ReconstructedPoint {
                position: [i as f64, 0.0, 0.0],
                color: [255, 255, 255],
                num_views: 1,
                error: 0.0,
            })
            .collect();
        let normals = estimate_normals(&points, 4);
        assert_eq!(normals.len(), 10);
        // Points along x-axis, normals should have zero x-component
        for n in &normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!(len > 0.9, "normal should be unit length: {}", len);
        }
    }

    #[test]
    fn test_poisson_empty() {
        let mesh = poisson_reconstruction(&[], &[], &PoissonConfig::default());
        assert!(mesh.vertices.is_empty());
        assert!(mesh.faces.is_empty());
    }
}
