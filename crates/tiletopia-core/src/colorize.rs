//! Point cloud colorization from orthophotos/imagery.
//!
//! Projects 3D points onto 2D imagery to assign RGB color values,
//! supporting multiple image sources with camera parameters.

use serde::{Deserialize, Serialize};

/// Camera model for image-to-world projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraModel {
    /// Camera position in world coordinates (x, y, z).
    pub position: [f64; 3],
    /// 3x3 rotation matrix (row-major) from world to camera.
    pub rotation: [f64; 9],
    /// Focal length in pixels.
    pub focal_length: f64,
    /// Principal point (cx, cy) in pixels.
    pub principal_point: [f64; 2],
    /// Image dimensions (width, height).
    pub image_size: [u32; 2],
}

/// A simple RGB image buffer.
#[derive(Debug, Clone)]
pub struct ImageBuffer {
    pub width: u32,
    pub height: u32,
    /// RGB data, row-major, 3 bytes per pixel.
    pub data: Vec<u8>,
}

/// An orthoimage with geographic bounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrthoImage {
    /// Geographic bounds [min_x, min_y, max_x, max_y].
    pub bounds: [f64; 4],
    pub width: u32,
    pub height: u32,
    /// RGB data, row-major.
    #[serde(skip)]
    pub data: Vec<u8>,
}

/// Result of colorization for a single point.
#[derive(Debug, Clone, Copy)]
pub struct PointColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// Confidence (0.0–1.0), based on angle/distance to image.
    pub confidence: f32,
}

impl Default for PointColor {
    fn default() -> Self {
        Self {
            r: 128,
            g: 128,
            b: 128,
            confidence: 0.0,
        }
    }
}

/// Colorize points from an orthoimage (nadir/top-down projection).
pub fn colorize_from_ortho(points: &[[f64; 3]], ortho: &OrthoImage) -> Vec<PointColor> {
    let x_scale = ortho.width as f64 / (ortho.bounds[2] - ortho.bounds[0]);
    let y_scale = ortho.height as f64 / (ortho.bounds[3] - ortho.bounds[1]);

    points
        .iter()
        .map(|p| {
            let px = ((p[0] - ortho.bounds[0]) * x_scale) as i64;
            let py = ((ortho.bounds[3] - p[1]) * y_scale) as i64; // flip Y

            if px < 0 || py < 0 || px >= ortho.width as i64 || py >= ortho.height as i64 {
                return PointColor::default();
            }

            let idx = ((py as u32 * ortho.width + px as u32) * 3) as usize;
            if idx + 2 >= ortho.data.len() {
                return PointColor::default();
            }

            PointColor {
                r: ortho.data[idx],
                g: ortho.data[idx + 1],
                b: ortho.data[idx + 2],
                confidence: 1.0,
            }
        })
        .collect()
}

/// Colorize points from a perspective camera image.
pub fn colorize_from_camera(
    points: &[[f64; 3]],
    image: &ImageBuffer,
    camera: &CameraModel,
) -> Vec<PointColor> {
    points
        .iter()
        .map(|p| {
            // Transform world point to camera coordinates
            let dx = p[0] - camera.position[0];
            let dy = p[1] - camera.position[1];
            let dz = p[2] - camera.position[2];

            let r = &camera.rotation;
            let cam_x = r[0] * dx + r[1] * dy + r[2] * dz;
            let cam_y = r[3] * dx + r[4] * dy + r[5] * dz;
            let cam_z = r[6] * dx + r[7] * dy + r[8] * dz;

            // Point must be in front of camera
            if cam_z <= 0.0 {
                return PointColor::default();
            }

            // Project to image plane
            let px = (camera.focal_length * cam_x / cam_z + camera.principal_point[0]) as i64;
            let py = (camera.focal_length * cam_y / cam_z + camera.principal_point[1]) as i64;

            if px < 0
                || py < 0
                || px >= camera.image_size[0] as i64
                || py >= camera.image_size[1] as i64
            {
                return PointColor::default();
            }

            let idx = ((py as u32 * image.width + px as u32) * 3) as usize;
            if idx + 2 >= image.data.len() {
                return PointColor::default();
            }

            // Confidence based on distance and viewing angle
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();
            let confidence = (1.0 / (1.0 + dist / 100.0)) as f32;

            PointColor {
                r: image.data[idx],
                g: image.data[idx + 1],
                b: image.data[idx + 2],
                confidence,
            }
        })
        .collect()
}

/// Merge colors from multiple sources using confidence-weighted blending.
pub fn merge_colors(color_sets: &[Vec<PointColor>]) -> Vec<PointColor> {
    if color_sets.is_empty() {
        return Vec::new();
    }
    let n = color_sets[0].len();

    (0..n)
        .map(|i| {
            let mut total_weight = 0.0f64;
            let mut r_sum = 0.0f64;
            let mut g_sum = 0.0f64;
            let mut b_sum = 0.0f64;

            for set in color_sets {
                if i < set.len() && set[i].confidence > 0.0 {
                    let w = set[i].confidence as f64;
                    r_sum += set[i].r as f64 * w;
                    g_sum += set[i].g as f64 * w;
                    b_sum += set[i].b as f64 * w;
                    total_weight += w;
                }
            }

            if total_weight < 0.001 {
                return PointColor::default();
            }

            PointColor {
                r: (r_sum / total_weight).round() as u8,
                g: (g_sum / total_weight).round() as u8,
                b: (b_sum / total_weight).round() as u8,
                confidence: (total_weight / color_sets.len() as f64) as f32,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_colorize_from_ortho() {
        // 4x4 red image
        let mut data = vec![0u8; 4 * 4 * 3];
        for i in (0..data.len()).step_by(3) {
            data[i] = 255; // R
            data[i + 1] = 0;
            data[i + 2] = 0;
        }
        let ortho = OrthoImage {
            bounds: [0.0, 0.0, 4.0, 4.0],
            width: 4,
            height: 4,
            data,
        };
        let points = vec![[1.0, 1.0, 0.0], [2.0, 2.0, 5.0]];
        let colors = colorize_from_ortho(&points, &ortho);
        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0].r, 255);
        assert_eq!(colors[0].g, 0);
        assert_eq!(colors[0].confidence, 1.0);
    }

    #[test]
    fn test_colorize_out_of_bounds() {
        let ortho = OrthoImage {
            bounds: [0.0, 0.0, 4.0, 4.0],
            width: 4,
            height: 4,
            data: vec![128; 4 * 4 * 3],
        };
        let points = vec![[-1.0, -1.0, 0.0], [10.0, 10.0, 0.0]];
        let colors = colorize_from_ortho(&points, &ortho);
        assert_eq!(colors[0].confidence, 0.0);
        assert_eq!(colors[1].confidence, 0.0);
    }

    #[test]
    fn test_colorize_from_camera() {
        // Camera at origin, looking down Z axis
        let camera = CameraModel {
            position: [0.0, 0.0, 0.0],
            rotation: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], // identity
            focal_length: 100.0,
            principal_point: [50.0, 50.0],
            image_size: [100, 100],
        };
        // Green 100x100 image
        let mut data = vec![0u8; 100 * 100 * 3];
        for i in (0..data.len()).step_by(3) {
            data[i + 1] = 200; // G
        }
        let image = ImageBuffer {
            width: 100,
            height: 100,
            data,
        };
        // Point directly in front of camera
        let points = vec![[0.0, 0.0, 10.0]];
        let colors = colorize_from_camera(&points, &image, &camera);
        assert_eq!(colors.len(), 1);
        assert_eq!(colors[0].g, 200);
        assert!(colors[0].confidence > 0.0);
    }

    #[test]
    fn test_merge_colors() {
        let set1 = vec![PointColor {
            r: 200,
            g: 0,
            b: 0,
            confidence: 1.0,
        }];
        let set2 = vec![PointColor {
            r: 0,
            g: 200,
            b: 0,
            confidence: 1.0,
        }];
        let merged = merge_colors(&[set1, set2]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].r, 100); // Average of 200 and 0
        assert_eq!(merged[0].g, 100);
    }
}
