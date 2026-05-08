//! Server-side measurement and volume computation tools.
//!
//! Provides distance, area, cut/fill volume between surfaces.

use std::f64::consts::PI;

/// A 3D point for measurements.
#[derive(Debug, Clone, Copy)]
pub struct MeasurePoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl MeasurePoint {
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    pub fn distance_to(&self, other: &Self) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

/// Compute 3D distance between two points.
pub fn distance_3d(a: &MeasurePoint, b: &MeasurePoint) -> f64 {
    a.distance_to(b)
}

/// Compute total length of a polyline (sum of segment lengths).
pub fn polyline_length(points: &[MeasurePoint]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }
    points.windows(2).map(|w| w[0].distance_to(&w[1])).sum()
}

/// Compute area of a 3D polygon using Newell's method.
/// Projects onto best-fit plane and computes area.
pub fn polygon_area_3d(vertices: &[MeasurePoint]) -> f64 {
    if vertices.len() < 3 {
        return 0.0;
    }
    // Newell's method for normal
    let mut nx = 0.0;
    let mut ny = 0.0;
    let mut nz = 0.0;
    let n = vertices.len();
    for i in 0..n {
        let j = (i + 1) % n;
        let vi = &vertices[i];
        let vj = &vertices[j];
        nx += (vi.y - vj.y) * (vi.z + vj.z);
        ny += (vi.z - vj.z) * (vi.x + vj.x);
        nz += (vi.x - vj.x) * (vi.y + vj.y);
    }
    let mag = (nx * nx + ny * ny + nz * nz).sqrt();
    mag / 2.0
}

/// Surface representation as a triangulated mesh for volume computation.
#[derive(Debug, Clone)]
pub struct Surface {
    pub vertices: Vec<MeasurePoint>,
    pub triangles: Vec<[usize; 3]>,
}

/// Compute signed volume of a triangle with respect to origin (for closed mesh).
fn signed_volume_of_triangle(a: &MeasurePoint, b: &MeasurePoint, c: &MeasurePoint) -> f64 {
    // V = (1/6) * |a · (b × c)|
    let cross_x = b.y * c.z - b.z * c.y;
    let cross_y = b.z * c.x - b.x * c.z;
    let cross_z = b.x * c.y - b.y * c.x;
    (a.x * cross_x + a.y * cross_y + a.z * cross_z) / 6.0
}

/// Compute volume of a closed triangulated mesh using divergence theorem.
pub fn mesh_volume(surface: &Surface) -> f64 {
    surface
        .triangles
        .iter()
        .map(|tri| {
            let a = &surface.vertices[tri[0]];
            let b = &surface.vertices[tri[1]];
            let c = &surface.vertices[tri[2]];
            signed_volume_of_triangle(a, b, c)
        })
        .sum::<f64>()
        .abs()
}

/// Result of a cut/fill volume computation.
#[derive(Debug, Clone)]
pub struct CutFillResult {
    pub cut_volume: f64,
    pub fill_volume: f64,
    pub net_volume: f64,
}

/// Compute cut/fill volumes between two surfaces on a regular grid.
///
/// `reference` is the existing surface, `design` is the target.
/// Samples both on a grid and computes column volumes.
pub fn cut_fill_volume(
    reference_heights: &[f64],
    design_heights: &[f64],
    grid_cols: usize,
    grid_rows: usize,
    cell_size: f64,
) -> CutFillResult {
    assert_eq!(reference_heights.len(), grid_cols * grid_rows);
    assert_eq!(design_heights.len(), grid_cols * grid_rows);

    let cell_area = cell_size * cell_size;
    let mut cut = 0.0;
    let mut fill = 0.0;

    for i in 0..reference_heights.len() {
        let diff = design_heights[i] - reference_heights[i];
        if diff > 0.0 {
            fill += diff * cell_area;
        } else {
            cut += (-diff) * cell_area;
        }
    }

    CutFillResult {
        cut_volume: cut,
        fill_volume: fill,
        net_volume: fill - cut,
    }
}

/// Compute horizontal distance (ignoring Z).
pub fn horizontal_distance(a: &MeasurePoint, b: &MeasurePoint) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// Compute slope between two points (rise/run as percentage).
pub fn slope_percent(a: &MeasurePoint, b: &MeasurePoint) -> f64 {
    let horiz = horizontal_distance(a, b);
    if horiz < 1e-10 {
        return 0.0;
    }
    let rise = (b.z - a.z).abs();
    (rise / horiz) * 100.0
}

/// Compute bearing angle from point A to B in degrees (0 = north, clockwise).
pub fn bearing(a: &MeasurePoint, b: &MeasurePoint) -> f64 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let angle_rad = dx.atan2(dy);
    let degrees = angle_rad * 180.0 / PI;
    if degrees < 0.0 {
        degrees + 360.0
    } else {
        degrees
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance_3d() {
        let a = MeasurePoint::new(0.0, 0.0, 0.0);
        let b = MeasurePoint::new(3.0, 4.0, 0.0);
        assert!((distance_3d(&a, &b) - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_polyline_length() {
        let pts = vec![
            MeasurePoint::new(0.0, 0.0, 0.0),
            MeasurePoint::new(1.0, 0.0, 0.0),
            MeasurePoint::new(1.0, 1.0, 0.0),
        ];
        let len = polyline_length(&pts);
        assert!((len - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_polygon_area() {
        // Unit square in XY plane
        let verts = vec![
            MeasurePoint::new(0.0, 0.0, 0.0),
            MeasurePoint::new(1.0, 0.0, 0.0),
            MeasurePoint::new(1.0, 1.0, 0.0),
            MeasurePoint::new(0.0, 1.0, 0.0),
        ];
        let area = polygon_area_3d(&verts);
        assert!((area - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_mesh_volume_tetrahedron() {
        // Tetrahedron with volume 1/6
        let surface = Surface {
            vertices: vec![
                MeasurePoint::new(0.0, 0.0, 0.0),
                MeasurePoint::new(1.0, 0.0, 0.0),
                MeasurePoint::new(0.0, 1.0, 0.0),
                MeasurePoint::new(0.0, 0.0, 1.0),
            ],
            triangles: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
        };
        let vol = mesh_volume(&surface);
        assert!((vol - 1.0 / 6.0).abs() < 1e-10);
    }

    #[test]
    fn test_cut_fill() {
        let reference = vec![10.0, 10.0, 10.0, 10.0];
        let design = vec![12.0, 8.0, 11.0, 9.0];
        let result = cut_fill_volume(&reference, &design, 2, 2, 1.0);
        assert!((result.fill_volume - 3.0).abs() < 1e-10); // 2 + 1
        assert!((result.cut_volume - 3.0).abs() < 1e-10); // 2 + 1
        assert!(result.net_volume.abs() < 1e-10);
    }

    #[test]
    fn test_slope_percent() {
        let a = MeasurePoint::new(0.0, 0.0, 0.0);
        let b = MeasurePoint::new(100.0, 0.0, 10.0);
        let slope = slope_percent(&a, &b);
        assert!((slope - 10.0).abs() < 1e-10);
    }

    #[test]
    fn test_bearing() {
        let a = MeasurePoint::new(0.0, 0.0, 0.0);
        let b = MeasurePoint::new(1.0, 0.0, 0.0); // due east
        let bear = bearing(&a, &b);
        assert!((bear - 90.0).abs() < 1e-10);
    }
}
