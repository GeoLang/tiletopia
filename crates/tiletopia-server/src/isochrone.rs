//! Isochrone / Service Area analysis.
//!
//! Computes reachability polygons: "show me everywhere reachable
//! within N minutes" by driving, walking, or cycling.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use uuid::Uuid;

/// An isochrone request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsochroneRequest {
    pub origin: [f64; 2], // [longitude, latitude]
    pub profile: TravelProfile,
    pub contours_minutes: Vec<u32>, // e.g., [5, 10, 15]
    pub denoise: f32,               // 0.0–1.0 smoothing factor
}

/// Travel profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TravelProfile {
    Driving,
    Walking,
    Cycling,
}

/// An isochrone result with multiple contours.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsochroneResult {
    pub id: Uuid,
    pub origin: [f64; 2],
    pub profile: TravelProfile,
    pub contours: Vec<IsochroneContour>,
}

/// A single time-distance contour (polygon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsochroneContour {
    pub minutes: u32,
    pub polygon: Vec<[f64; 2]>, // exterior ring [[lon, lat], ...]
    pub area_km2: f64,
}

impl TravelProfile {
    /// Average speed in km/h for the profile.
    fn speed_kmh(&self) -> f64 {
        match self {
            TravelProfile::Driving => 35.0,
            TravelProfile::Walking => 5.0,
            TravelProfile::Cycling => 15.0,
        }
    }
}

const GRID_SIZE: usize = 51;
const KM_PER_DEG: f64 = 111.32;

#[derive(Debug, Clone, Copy)]
struct DijkstraState {
    cost_minutes: f64,
    row: usize,
    col: usize,
}

impl PartialEq for DijkstraState {
    fn eq(&self, other: &Self) -> bool {
        self.cost_minutes.total_cmp(&other.cost_minutes) == Ordering::Equal
    }
}

impl Eq for DijkstraState {}

impl Ord for DijkstraState {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap
        other.cost_minutes.total_cmp(&self.cost_minutes)
    }
}

impl PartialOrd for DijkstraState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Run Dijkstra on a grid and return travel time (minutes) to each cell.
fn dijkstra_grid(grid_size: usize, center: usize, cell_size_km: f64, speed_kmh: f64) -> Vec<f64> {
    let n = grid_size * grid_size;
    let mut dist = vec![f64::INFINITY; n];
    let mut heap = BinaryHeap::new();

    let start = center * grid_size + center;
    dist[start] = 0.0;
    heap.push(DijkstraState {
        cost_minutes: 0.0,
        row: center,
        col: center,
    });

    // 8-connected neighbors: orthogonal + diagonal
    let neighbors: [(i32, i32, f64); 8] = [
        (-1, 0, 1.0),
        (1, 0, 1.0),
        (0, -1, 1.0),
        (0, 1, 1.0),
        (-1, -1, std::f64::consts::SQRT_2),
        (-1, 1, std::f64::consts::SQRT_2),
        (1, -1, std::f64::consts::SQRT_2),
        (1, 1, std::f64::consts::SQRT_2),
    ];

    while let Some(state) = heap.pop() {
        let idx = state.row * grid_size + state.col;
        if state.cost_minutes > dist[idx] {
            continue;
        }

        for &(dr, dc, dist_factor) in &neighbors {
            let nr = state.row as i32 + dr;
            let nc = state.col as i32 + dc;
            if nr < 0 || nr >= grid_size as i32 || nc < 0 || nc >= grid_size as i32 {
                continue;
            }
            let nr = nr as usize;
            let nc = nc as usize;
            let edge_km = cell_size_km * dist_factor;
            let edge_minutes = edge_km / speed_kmh * 60.0;
            let new_cost = state.cost_minutes + edge_minutes;
            let nidx = nr * grid_size + nc;
            if new_cost < dist[nidx] {
                dist[nidx] = new_cost;
                heap.push(DijkstraState {
                    cost_minutes: new_cost,
                    row: nr,
                    col: nc,
                });
            }
        }
    }

    dist
}

/// Compute the convex hull of a set of points using Andrew's monotone chain algorithm.
fn convex_hull(mut points: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        if !points.is_empty() {
            points.push(points[0]);
        }
        return points;
    }

    points.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();

    if points.len() < 3 {
        points.push(points[0]);
        return points;
    }

    let mut hull: Vec<[f64; 2]> = Vec::new();

    // Lower hull
    for &p in &points {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }

    // Upper hull
    let lower_len = hull.len();
    for &p in points.iter().rev().skip(1) {
        while hull.len() > lower_len && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
        {
            hull.pop();
        }
        hull.push(p);
    }

    hull
}

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Compute the area of a polygon in km² using the shoelace formula.
fn polygon_area_km2(polygon: &[[f64; 2]]) -> f64 {
    if polygon.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    let n = polygon.len() - 1; // last point == first (closed ring)
    for i in 0..n {
        let j = (i + 1) % n;
        area += polygon[i][0] * polygon[j][1];
        area -= polygon[j][0] * polygon[i][1];
    }
    let area_deg2 = (area / 2.0).abs();
    area_deg2 * KM_PER_DEG * KM_PER_DEG
}

/// Compute isochrone contours from an origin point.
pub fn compute_isochrone(request: &IsochroneRequest) -> IsochroneResult {
    let max_minutes = request.contours_minutes.iter().copied().max().unwrap_or(1);
    let speed = request.profile.speed_kmh();
    let max_radius_km = speed * (max_minutes as f64 / 60.0);
    let max_radius_deg = max_radius_km / KM_PER_DEG;

    let center = GRID_SIZE / 2;
    let cell_size_deg = (2.0 * max_radius_deg) / (GRID_SIZE - 1) as f64;
    let cell_size_km = cell_size_deg * KM_PER_DEG;

    let dist = dijkstra_grid(GRID_SIZE, center, cell_size_km, speed);

    let contours = request
        .contours_minutes
        .iter()
        .map(|&minutes| {
            let mut reachable = Vec::new();
            for row in 0..GRID_SIZE {
                for col in 0..GRID_SIZE {
                    if dist[row * GRID_SIZE + col] <= minutes as f64 {
                        let lon = request.origin[0] + (col as f64 - center as f64) * cell_size_deg;
                        let lat = request.origin[1] + (center as f64 - row as f64) * cell_size_deg;
                        reachable.push([lon, lat]);
                    }
                }
            }

            let polygon = if reachable.is_empty() {
                vec![request.origin, request.origin]
            } else {
                convex_hull(reachable)
            };
            let area_km2 = polygon_area_km2(&polygon);
            IsochroneContour {
                minutes,
                polygon,
                area_km2,
            }
        })
        .collect();

    IsochroneResult {
        id: Uuid::new_v4(),
        origin: request.origin,
        profile: request.profile.clone(),
        contours,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_isochrone() {
        let req = IsochroneRequest {
            origin: [-122.4194, 37.7749],
            profile: TravelProfile::Driving,
            contours_minutes: vec![5, 10, 15],
            denoise: 0.5,
        };
        let result = compute_isochrone(&req);
        assert_eq!(result.contours.len(), 3);
        assert!(result.contours[0].area_km2 < result.contours[1].area_km2);
        assert!(result.contours[1].area_km2 < result.contours[2].area_km2);
    }

    #[test]
    fn test_walking_isochrone() {
        let req = IsochroneRequest {
            origin: [0.0, 0.0],
            profile: TravelProfile::Walking,
            contours_minutes: vec![10],
            denoise: 0.0,
        };
        let result = compute_isochrone(&req);
        assert_eq!(result.contours.len(), 1);
        // Walking 10 min at 5 km/h = ~0.83 km radius → ~2.17 km²
        assert!(result.contours[0].area_km2 > 1.0);
        assert!(result.contours[0].area_km2 < 5.0);
    }

    #[test]
    fn test_polygon_closed() {
        let req = IsochroneRequest {
            origin: [-73.9857, 40.7484],
            profile: TravelProfile::Cycling,
            contours_minutes: vec![5],
            denoise: 0.0,
        };
        let result = compute_isochrone(&req);
        let poly = &result.contours[0].polygon;
        assert_eq!(poly.first(), poly.last()); // ring is closed
    }

    #[test]
    fn test_dijkstra_origin_zero() {
        let dist = dijkstra_grid(11, 5, 1.0, 60.0);
        assert_eq!(dist[5 * 11 + 5], 0.0);
        // Adjacent orthogonal cell: 1 km at 60 km/h = 1 minute
        assert!((dist[5 * 11 + 6] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_convex_hull_triangle() {
        let points = vec![[0.0, 0.0], [1.0, 0.0], [0.5, 1.0]];
        let hull = convex_hull(points);
        assert_eq!(hull.first(), hull.last());
        assert!(hull.len() >= 4); // 3 points + closing
    }

    #[test]
    fn test_larger_contour_contains_smaller() {
        let req = IsochroneRequest {
            origin: [10.0, 50.0],
            profile: TravelProfile::Driving,
            contours_minutes: vec![5, 15],
            denoise: 0.0,
        };
        let result = compute_isochrone(&req);
        assert!(result.contours[0].area_km2 < result.contours[1].area_km2);
    }
}
