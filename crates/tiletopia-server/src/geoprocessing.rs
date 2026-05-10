//! Geoprocessing / Spatial Analysis — buffer, union, intersection, dissolve, clip.
//!
//! Provides common vector geometry operations used in GIS analysis workflows,
//! equivalent to ESRI's geoprocessing toolbox or PostGIS spatial functions.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A geometry for processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geometry {
    #[serde(rename = "type")]
    pub geom_type: GeomType,
    pub coordinates: Vec<[f64; 2]>,
}

/// Geometry types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GeomType {
    Point,
    LineString,
    Polygon,
    MultiPolygon,
}

/// A geoprocessing job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoprocessingJob {
    pub id: Uuid,
    pub operation: GeoOperation,
    pub status: JobStatus,
    pub input_feature_count: u32,
    pub output_feature_count: Option<u32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Available geoprocessing operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GeoOperation {
    /// Buffer — expand geometry by distance
    Buffer { distance_m: f64 },
    /// Union — merge multiple geometries into one
    Union,
    /// Intersection — area common to two geometries
    Intersection,
    /// Difference — subtract one geometry from another
    Difference,
    /// Dissolve — merge features sharing an attribute value
    Dissolve { field: String },
    /// Clip — cut features to a boundary
    Clip,
    /// Convex Hull — smallest convex polygon containing all points
    ConvexHull,
    /// Centroid — compute center point of geometry
    Centroid,
    /// Simplify — reduce vertex count (Douglas-Peucker)
    Simplify { tolerance: f64 },
    /// Voronoi — Thiessen polygons from point set
    Voronoi,
}

/// Job status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed(String),
}

/// Buffer result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferResult {
    pub original: Geometry,
    pub buffered: Geometry,
    pub distance_m: f64,
    pub area_m2: f64,
}

/// Compute a buffer around a polygon (simplified circle approximation).
pub fn buffer(geometry: &Geometry, distance_m: f64) -> BufferResult {
    let distance_deg = distance_m / 111320.0; // meters to degrees (approx)
    let buffered_coords: Vec<[f64; 2]> = geometry
        .coordinates
        .iter()
        .map(|coord| {
            // Expand each point outward from centroid
            let cx: f64 = geometry.coordinates.iter().map(|c| c[0]).sum::<f64>()
                / geometry.coordinates.len() as f64;
            let cy: f64 = geometry.coordinates.iter().map(|c| c[1]).sum::<f64>()
                / geometry.coordinates.len() as f64;
            let dx = coord[0] - cx;
            let dy = coord[1] - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist < 1e-10 {
                *coord
            } else {
                let scale = (dist + distance_deg) / dist;
                [cx + dx * scale, cy + dy * scale]
            }
        })
        .collect();

    let area_m2 = polygon_area_m2(&buffered_coords);

    BufferResult {
        original: geometry.clone(),
        buffered: Geometry {
            geom_type: GeomType::Polygon,
            coordinates: buffered_coords,
        },
        distance_m,
        area_m2,
    }
}

/// Compute convex hull of a point set (Graham scan).
pub fn convex_hull(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }

    let mut pts: Vec<[f64; 2]> = points.to_vec();
    // Find lowest point (min y, then min x)
    pts.sort_by(|a, b| {
        a[1].partial_cmp(&b[1])
            .unwrap()
            .then(a[0].partial_cmp(&b[0]).unwrap())
    });
    let pivot = pts[0];

    // Sort by polar angle
    pts[1..].sort_by(|a, b| {
        let angle_a = (a[1] - pivot[1]).atan2(a[0] - pivot[0]);
        let angle_b = (b[1] - pivot[1]).atan2(b[0] - pivot[0]);
        angle_a.partial_cmp(&angle_b).unwrap()
    });

    let mut hull: Vec<[f64; 2]> = Vec::new();
    for p in &pts {
        while hull.len() >= 2 && cross(&hull[hull.len() - 2], &hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(*p);
    }
    hull.push(hull[0]); // close ring
    hull
}

/// Cross product of vectors OA and OB.
fn cross(o: &[f64; 2], a: &[f64; 2], b: &[f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Compute centroid of a polygon.
pub fn centroid(polygon: &[[f64; 2]]) -> [f64; 2] {
    let n = polygon.len() as f64;
    let cx = polygon.iter().map(|p| p[0]).sum::<f64>() / n;
    let cy = polygon.iter().map(|p| p[1]).sum::<f64>() / n;
    [cx, cy]
}

/// Simplify a polyline using Douglas-Peucker algorithm.
pub fn simplify(points: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut max_dist = 0.0;
    let mut index = 0;
    let end = points.len() - 1;

    for i in 1..end {
        let d = perpendicular_distance(&points[i], &points[0], &points[end]);
        if d > max_dist {
            max_dist = d;
            index = i;
        }
    }

    if max_dist > tolerance {
        let mut left = simplify(&points[..=index], tolerance);
        let right = simplify(&points[index..], tolerance);
        left.pop(); // remove duplicate
        left.extend_from_slice(&right);
        left
    } else {
        vec![points[0], points[end]]
    }
}

/// Perpendicular distance from point to line segment.
fn perpendicular_distance(point: &[f64; 2], line_start: &[f64; 2], line_end: &[f64; 2]) -> f64 {
    let dx = line_end[0] - line_start[0];
    let dy = line_end[1] - line_start[1];
    let mag = (dx * dx + dy * dy).sqrt();
    if mag < 1e-10 {
        let pdx = point[0] - line_start[0];
        let pdy = point[1] - line_start[1];
        return (pdx * pdx + pdy * pdy).sqrt();
    }
    ((point[0] - line_start[0]) * dy - (point[1] - line_start[1]) * dx).abs() / mag
}

/// Compute polygon area in square meters (Shoelface formula + lat correction).
fn polygon_area_m2(coords: &[[f64; 2]]) -> f64 {
    let n = coords.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        area += coords[i][0] * coords[j][1];
        area -= coords[j][0] * coords[i][1];
    }
    let area_deg2 = (area / 2.0).abs();
    // Convert from deg² to m² (approximate at mid-latitude)
    let mid_lat = coords.iter().map(|c| c[1]).sum::<f64>() / n as f64;
    let m_per_deg_lat = 111320.0;
    let m_per_deg_lon = 111320.0 * mid_lat.to_radians().cos();
    area_deg2 * m_per_deg_lat * m_per_deg_lon
}

/// List available geoprocessing operations.
pub fn available_operations() -> Vec<&'static str> {
    vec![
        "Buffer",
        "Union",
        "Intersection",
        "Difference",
        "Dissolve",
        "Clip",
        "ConvexHull",
        "Centroid",
        "Simplify",
        "Voronoi",
    ]
}

/// Clip subject polygon by convex clip polygon (Sutherland-Hodgman algorithm).
pub fn polygon_intersection(subject: &[[f64; 2]], clip: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut output = subject.to_vec();

    let clip_len = if clip.len() > 1 && clip.first() == clip.last() {
        clip.len() - 1 // skip closing vertex for edge iteration
    } else {
        clip.len()
    };

    for i in 0..clip_len {
        if output.is_empty() {
            break;
        }
        let edge_start = clip[i];
        let edge_end = clip[(i + 1) % clip_len];

        let input = output.clone();
        output.clear();

        let n = input.len();
        for j in 0..n {
            let current = input[j];
            let previous = input[(j + n - 1) % n];

            let curr_inside = is_inside(&current, &edge_start, &edge_end);
            let prev_inside = is_inside(&previous, &edge_start, &edge_end);

            if curr_inside {
                if !prev_inside
                    && let Some(p) = line_intersect(&previous, &current, &edge_start, &edge_end)
                {
                    output.push(p);
                }
                output.push(current);
            } else if prev_inside
                && let Some(p) = line_intersect(&previous, &current, &edge_start, &edge_end)
            {
                output.push(p);
            }
        }
    }
    output
}

/// Test if a point is on the inside (left side) of a directed edge.
fn is_inside(point: &[f64; 2], edge_start: &[f64; 2], edge_end: &[f64; 2]) -> bool {
    (edge_end[0] - edge_start[0]) * (point[1] - edge_start[1])
        - (edge_end[1] - edge_start[1]) * (point[0] - edge_start[0])
        >= 0.0
}

/// Find the intersection point of two line segments (p1-p2 and p3-p4).
fn line_intersect(p1: &[f64; 2], p2: &[f64; 2], p3: &[f64; 2], p4: &[f64; 2]) -> Option<[f64; 2]> {
    let x1 = p1[0];
    let y1 = p1[1];
    let x2 = p2[0];
    let y2 = p2[1];
    let x3 = p3[0];
    let y3 = p3[1];
    let x4 = p4[0];
    let y4 = p4[1];
    let denom = (x1 - x2) * (y3 - y4) - (y1 - y2) * (x3 - x4);
    if denom.abs() < 1e-10 {
        return None;
    }
    let t = ((x1 - x3) * (y3 - y4) - (y1 - y3) * (x3 - x4)) / denom;
    Some([x1 + t * (x2 - x1), y1 + t * (y2 - y1)])
}

/// Union two polygons (approximation: convex hull of all vertices).
pub fn polygon_union(a: &[[f64; 2]], b: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut all_points: Vec<[f64; 2]> = a.to_vec();
    all_points.extend_from_slice(b);
    convex_hull(&all_points)
}

/// Compute difference A - B (vertices of A not inside B, plus intersection boundary points).
pub fn polygon_difference(a: &[[f64; 2]], b: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut result = Vec::new();
    for point in a {
        if !point_in_polygon(point, b) {
            result.push(*point);
        }
    }
    // Add intersection points along A's edges with B's edges
    let a_len = a.len();
    let b_len = b.len();
    for i in 0..a_len {
        let a1 = a[i];
        let a2 = a[(i + 1) % a_len];
        for j in 0..b_len {
            let b1 = b[j];
            let b2 = b[(j + 1) % b_len];
            if let Some(p) = line_intersect(&a1, &a2, &b1, &b2) {
                result.push(p);
            }
        }
    }
    if result.len() >= 3 {
        convex_hull(&result)
    } else {
        result
    }
}

/// Ray-casting point-in-polygon test.
fn point_in_polygon(point: &[f64; 2], polygon: &[[f64; 2]]) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        if ((polygon[i][1] > point[1]) != (polygon[j][1] > point[1]))
            && (point[0]
                < (polygon[j][0] - polygon[i][0]) * (point[1] - polygon[i][1])
                    / (polygon[j][1] - polygon[i][1])
                    + polygon[i][0])
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer() {
        let geom = Geometry {
            geom_type: GeomType::Polygon,
            coordinates: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]],
        };
        let result = buffer(&geom, 100.0);
        assert!(result.area_m2 > 0.0);
        assert_eq!(result.distance_m, 100.0);
    }

    #[test]
    fn test_convex_hull() {
        let points = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [0.5, 0.5], // interior point
            [1.0, 1.0],
            [0.0, 1.0],
        ];
        let hull = convex_hull(&points);
        // Interior point should be excluded
        assert!(hull.len() <= 5); // 4 corners + closing point
    }

    #[test]
    fn test_centroid() {
        let polygon = vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let c = centroid(&polygon);
        assert!((c[0] - 1.0).abs() < 0.001);
        assert!((c[1] - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_simplify() {
        let points = vec![
            [0.0, 0.0],
            [0.1, 0.001], // nearly collinear
            [0.2, 0.0],
            [0.3, 0.5],
            [0.4, 0.0],
        ];
        let simplified = simplify(&points, 0.01);
        assert!(simplified.len() < points.len());
    }

    #[test]
    fn test_available_operations() {
        let ops = available_operations();
        assert_eq!(ops.len(), 10);
        assert!(ops.contains(&"Buffer"));
    }

    #[test]
    fn test_polygon_intersection() {
        // Two overlapping squares
        let subject = vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let clip = vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]];
        let result = polygon_intersection(&subject, &clip);
        assert!(result.len() >= 3, "intersection should produce a polygon");
        // All result points should be within both input bboxes' overlap: [1,1] to [2,2]
        for p in &result {
            assert!(p[0] >= 1.0 - 1e-9 && p[0] <= 2.0 + 1e-9);
            assert!(p[1] >= 1.0 - 1e-9 && p[1] <= 2.0 + 1e-9);
        }
    }

    #[test]
    fn test_polygon_union() {
        let a = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let b = vec![[2.0, 0.0], [3.0, 0.0], [3.0, 1.0], [2.0, 1.0]];
        let result = polygon_union(&a, &b);
        // Convex hull should contain all input points
        assert!(result.len() >= 4);
    }

    #[test]
    fn test_polygon_difference() {
        let a = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        let b = vec![[2.0, 2.0], [6.0, 2.0], [6.0, 6.0], [2.0, 6.0]];
        let result = polygon_difference(&a, &b);
        // Should retain points of A that are outside B
        assert!(!result.is_empty());
        // [0,0], [4,0], [0,4] are outside B, [4,4] is inside B
        assert!(result.iter().any(|p| p[0] < 1.0 && p[1] < 1.0));
    }

    #[test]
    fn test_point_in_polygon() {
        let poly = vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 4.0]];
        assert!(point_in_polygon(&[2.0, 2.0], &poly));
        assert!(!point_in_polygon(&[5.0, 5.0], &poly));
    }
}
