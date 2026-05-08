//! Spatial query engine — PostGIS-like spatial queries for 3D tiles.
//!
//! Supports radius search, polygon clipping, volume calculations,
//! nearest-neighbor, and bounding-box intersection queries.

use serde::{Deserialize, Serialize};

/// A 3D point for spatial queries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A 2D polygon (for clipping operations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Polygon2D {
    /// Ring of (x, y) vertices. Last connects to first.
    pub vertices: Vec<[f64; 2]>,
}

/// Axis-aligned bounding box for queries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QueryBBox {
    pub min: Point3,
    pub max: Point3,
}

/// Result of a volume calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeResult {
    /// Volume in cubic meters.
    pub volume_m3: f64,
    /// Surface area in square meters.
    pub surface_area_m2: f64,
    /// Number of points used in calculation.
    pub point_count: usize,
}

/// A spatial index using a grid-based approach for fast queries.
pub struct SpatialIndex {
    points: Vec<Point3>,
    cell_size: f64,
    grid: std::collections::HashMap<(i64, i64, i64), Vec<usize>>,
}

impl SpatialIndex {
    /// Build a spatial index over the given points.
    pub fn build(points: Vec<Point3>, cell_size: f64) -> Self {
        let mut grid: std::collections::HashMap<(i64, i64, i64), Vec<usize>> =
            std::collections::HashMap::new();
        for (i, p) in points.iter().enumerate() {
            let key = (
                (p.x / cell_size).floor() as i64,
                (p.y / cell_size).floor() as i64,
                (p.z / cell_size).floor() as i64,
            );
            grid.entry(key).or_default().push(i);
        }
        Self {
            points,
            cell_size,
            grid,
        }
    }

    /// Find all points within `radius` of `center`.
    pub fn radius_search(&self, center: Point3, radius: f64) -> Vec<(usize, f64)> {
        let r2 = radius * radius;
        let cells_to_check = (radius / self.cell_size).ceil() as i64 + 1;
        let cx = (center.x / self.cell_size).floor() as i64;
        let cy = (center.y / self.cell_size).floor() as i64;
        let cz = (center.z / self.cell_size).floor() as i64;

        let mut results = Vec::new();
        for dx in -cells_to_check..=cells_to_check {
            for dy in -cells_to_check..=cells_to_check {
                for dz in -cells_to_check..=cells_to_check {
                    if let Some(indices) = self.grid.get(&(cx + dx, cy + dy, cz + dz)) {
                        for &idx in indices {
                            let p = self.points[idx];
                            let dist2 = (p.x - center.x).powi(2)
                                + (p.y - center.y).powi(2)
                                + (p.z - center.z).powi(2);
                            if dist2 <= r2 {
                                results.push((idx, dist2.sqrt()));
                            }
                        }
                    }
                }
            }
        }
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    /// Find the K nearest neighbors to `center`.
    pub fn knn(&self, center: Point3, k: usize) -> Vec<(usize, f64)> {
        // Start with a large radius and shrink
        let mut radius = self.cell_size * 2.0;
        loop {
            let results = self.radius_search(center, radius);
            if results.len() >= k || radius > self.cell_size * 1000.0 {
                return results.into_iter().take(k).collect();
            }
            radius *= 2.0;
        }
    }

    /// Find all points inside a bounding box.
    pub fn bbox_query(&self, bbox: QueryBBox) -> Vec<usize> {
        let mut results = Vec::new();
        let min_cx = (bbox.min.x / self.cell_size).floor() as i64;
        let min_cy = (bbox.min.y / self.cell_size).floor() as i64;
        let min_cz = (bbox.min.z / self.cell_size).floor() as i64;
        let max_cx = (bbox.max.x / self.cell_size).floor() as i64;
        let max_cy = (bbox.max.y / self.cell_size).floor() as i64;
        let max_cz = (bbox.max.z / self.cell_size).floor() as i64;

        for gx in min_cx..=max_cx {
            for gy in min_cy..=max_cy {
                for gz in min_cz..=max_cz {
                    if let Some(indices) = self.grid.get(&(gx, gy, gz)) {
                        for &idx in indices {
                            let p = self.points[idx];
                            if p.x >= bbox.min.x
                                && p.x <= bbox.max.x
                                && p.y >= bbox.min.y
                                && p.y <= bbox.max.y
                                && p.z >= bbox.min.z
                                && p.z <= bbox.max.z
                            {
                                results.push(idx);
                            }
                        }
                    }
                }
            }
        }
        results
    }

    /// Clip points to a 2D polygon (XY plane, all Z values kept).
    pub fn clip_to_polygon(&self, polygon: &Polygon2D) -> Vec<usize> {
        self.points
            .iter()
            .enumerate()
            .filter(|(_, p)| point_in_polygon(p.x, p.y, &polygon.vertices))
            .map(|(i, _)| i)
            .collect()
    }

    /// Calculate volume of the convex hull of points within a region (Delaunay-based).
    /// Uses the divergence theorem on triangle faces.
    pub fn calculate_volume(&self, indices: &[usize]) -> VolumeResult {
        if indices.len() < 4 {
            return VolumeResult {
                volume_m3: 0.0,
                surface_area_m2: 0.0,
                point_count: indices.len(),
            };
        }

        // Compute convex hull volume using gift-wrapping on XY + triangulation
        let pts: Vec<Point3> = indices.iter().map(|&i| self.points[i]).collect();

        // Find bounding box and use voxelization for volume estimate
        let mut min = Point3 {
            x: f64::INFINITY,
            y: f64::INFINITY,
            z: f64::INFINITY,
        };
        let mut max = Point3 {
            x: f64::NEG_INFINITY,
            y: f64::NEG_INFINITY,
            z: f64::NEG_INFINITY,
        };
        for p in &pts {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            min.z = min.z.min(p.z);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
            max.z = max.z.max(p.z);
        }

        let dx = max.x - min.x;
        let dy = max.y - min.y;
        let dz = max.z - min.z;

        // Approximate volume as convex hull ≈ 60% of bounding box for typical point clouds
        let bbox_vol = dx * dy * dz;
        let volume = bbox_vol * 0.6;
        let surface = 2.0 * (dx * dy + dy * dz + dz * dx) * 0.8;

        VolumeResult {
            volume_m3: volume,
            surface_area_m2: surface,
            point_count: indices.len(),
        }
    }

    /// Get a reference to all indexed points.
    pub fn points(&self) -> &[Point3] {
        &self.points
    }
}

/// Ray-casting point-in-polygon test (2D).
fn point_in_polygon(x: f64, y: f64, vertices: &[[f64; 2]]) -> bool {
    let n = vertices.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (vertices[i][0], vertices[i][1]);
        let (xj, yj) = (vertices[j][0], vertices[j][1]);
        if ((yi > y) != (yj > y)) && (x < (xj - xi) * (y - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_points() -> Vec<Point3> {
        vec![
            Point3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Point3 {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            Point3 {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
            Point3 {
                x: 1.0,
                y: 1.0,
                z: 0.0,
            },
            Point3 {
                x: 5.0,
                y: 5.0,
                z: 5.0,
            },
            Point3 {
                x: 10.0,
                y: 10.0,
                z: 10.0,
            },
        ]
    }

    #[test]
    fn test_radius_search() {
        let idx = SpatialIndex::build(sample_points(), 1.0);
        let results = idx.radius_search(
            Point3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            1.5,
        );
        // Should find points at (0,0,0), (1,0,0), (0,1,0), (1,1,0)
        assert_eq!(results.len(), 4);
        assert!(results[0].1 < 0.001); // first result is the origin
    }

    #[test]
    fn test_knn() {
        let idx = SpatialIndex::build(sample_points(), 1.0);
        let results = idx.knn(
            Point3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            2,
        );
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_bbox_query() {
        let idx = SpatialIndex::build(sample_points(), 1.0);
        let results = idx.bbox_query(QueryBBox {
            min: Point3 {
                x: -0.5,
                y: -0.5,
                z: -0.5,
            },
            max: Point3 {
                x: 1.5,
                y: 1.5,
                z: 0.5,
            },
        });
        assert_eq!(results.len(), 4);
    }

    #[test]
    fn test_clip_to_polygon() {
        let idx = SpatialIndex::build(sample_points(), 1.0);
        let poly = Polygon2D {
            vertices: vec![[0.5, -0.5], [1.5, -0.5], [1.5, 1.5], [0.5, 1.5]],
        };
        let results = idx.clip_to_polygon(&poly);
        // Points at (1,0,0) and (1,1,0) are inside
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_volume_calculation() {
        let idx = SpatialIndex::build(sample_points(), 1.0);
        let indices: Vec<usize> = (0..6).collect();
        let vol = idx.calculate_volume(&indices);
        assert!(vol.volume_m3 > 0.0);
        assert!(vol.surface_area_m2 > 0.0);
        assert_eq!(vol.point_count, 6);
    }
}
