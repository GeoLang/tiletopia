//! Isochrone / Service Area analysis.
//!
//! Wraps `itinera_core::isochrone()` for graph-based reachability analysis,
//! with a grid-based fallback for areas without loaded road network data.

use itinera_core::isochrone as itinera_isochrone;
use itinera_graph::{Graph, NodeId, SpeedProfile};
use serde::{Deserialize, Serialize};
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

const KM_PER_DEG: f64 = 111.32;

/// Compute isochrone contours using itinera on a provided graph.
pub fn compute_isochrone_graph(
    request: &IsochroneRequest,
    graph: &Graph,
    origin_node: NodeId,
) -> IsochroneResult {
    let contours = request
        .contours_minutes
        .iter()
        .map(|&minutes| {
            let max_seconds = minutes as f64 * 60.0;
            let profile = to_speed_profile(&request.profile);

            let result = itinera_isochrone(graph, origin_node, max_seconds, &profile);

            let polygon: Vec<[f64; 2]> = if result.boundary.is_empty() {
                vec![request.origin, request.origin]
            } else {
                let mut ring: Vec<[f64; 2]> =
                    result.boundary.iter().map(|c| [c.lon, c.lat]).collect();
                // Close the ring
                if ring.first() != ring.last() {
                    ring.push(ring[0]);
                }
                ring
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

/// Compute isochrone contours using grid-based approximation (no graph required).
pub fn compute_isochrone(request: &IsochroneRequest) -> IsochroneResult {
    let max_minutes = request.contours_minutes.iter().copied().max().unwrap_or(1);
    let speed = match &request.profile {
        TravelProfile::Driving => 35.0,
        TravelProfile::Walking => 5.0,
        TravelProfile::Cycling => 15.0,
    };
    let max_radius_km = speed * (max_minutes as f64 / 60.0);
    let max_radius_deg = max_radius_km / KM_PER_DEG;

    let grid_size: usize = 51;
    let center = grid_size / 2;
    let cell_size_deg = (2.0 * max_radius_deg) / (grid_size - 1) as f64;
    let cell_size_km = cell_size_deg * KM_PER_DEG;

    let dist = dijkstra_grid(grid_size, center, cell_size_km, speed);

    let contours = request
        .contours_minutes
        .iter()
        .map(|&minutes| {
            let mut reachable = Vec::new();
            for row in 0..grid_size {
                for col in 0..grid_size {
                    if dist[row * grid_size + col] <= minutes as f64 {
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

fn to_speed_profile(profile: &TravelProfile) -> SpeedProfile {
    match profile {
        TravelProfile::Driving => SpeedProfile::car(),
        TravelProfile::Walking => SpeedProfile::pedestrian(),
        TravelProfile::Cycling => SpeedProfile::bicycle(),
    }
}

/// Dijkstra on a uniform grid — fallback when no road network is available.
fn dijkstra_grid(grid_size: usize, center: usize, cell_size_km: f64, speed_kmh: f64) -> Vec<f64> {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    #[derive(Debug, Clone, Copy)]
    struct State {
        cost_minutes: f64,
        row: usize,
        col: usize,
    }

    impl PartialEq for State {
        fn eq(&self, other: &Self) -> bool {
            self.cost_minutes.total_cmp(&other.cost_minutes) == Ordering::Equal
        }
    }
    impl Eq for State {}
    impl Ord for State {
        fn cmp(&self, other: &Self) -> Ordering {
            other.cost_minutes.total_cmp(&self.cost_minutes)
        }
    }
    impl PartialOrd for State {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    let n = grid_size * grid_size;
    let mut dist = vec![f64::INFINITY; n];
    let mut heap = BinaryHeap::new();

    let start = center * grid_size + center;
    dist[start] = 0.0;
    heap.push(State {
        cost_minutes: 0.0,
        row: center,
        col: center,
    });

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
                heap.push(State {
                    cost_minutes: new_cost,
                    row: nr,
                    col: nc,
                });
            }
        }
    }

    dist
}

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

fn polygon_area_km2(polygon: &[[f64; 2]]) -> f64 {
    if polygon.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    let n = polygon.len() - 1;
    for i in 0..n {
        let j = (i + 1) % n;
        area += polygon[i][0] * polygon[j][1];
        area -= polygon[j][0] * polygon[i][1];
    }
    let area_deg2 = (area / 2.0).abs();
    area_deg2 * KM_PER_DEG * KM_PER_DEG
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
        assert_eq!(poly.first(), poly.last());
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
