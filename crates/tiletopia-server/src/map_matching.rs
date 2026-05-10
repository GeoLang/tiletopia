//! Map Matching — snap GPS traces to the road network.
//!
//! Takes noisy GPS coordinates and aligns them to the most likely
//! road segments using a Hidden Markov Model approach.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A raw GPS trace point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpsPoint {
    pub latitude: f64,
    pub longitude: f64,
    pub timestamp: Option<f64>, // Unix epoch seconds
    pub accuracy_m: Option<f64>,
    pub speed_mps: Option<f64>,
    pub bearing_deg: Option<f64>,
}

/// Map matching request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapMatchRequest {
    pub trace: Vec<GpsPoint>,
    pub profile: MatchProfile,
    pub search_radius_m: f64,
}

/// Matching profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MatchProfile {
    Driving,
    Walking,
    Cycling,
}

/// Map matching result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapMatchResult {
    pub id: Uuid,
    pub matched_points: Vec<MatchedPoint>,
    pub matched_route: Vec<[f64; 2]>, // snapped coordinates
    pub confidence: f64,              // 0.0–1.0
    pub total_distance_m: f64,
    pub road_segments: Vec<MatchedSegment>,
}

/// A matched (snapped) point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedPoint {
    pub original: [f64; 2],        // [lon, lat]
    pub snapped: [f64; 2],         // [lon, lat]
    pub distance_from_road_m: f64, // perpendicular distance to road
    pub road_name: Option<String>,
}

/// A matched road segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedSegment {
    pub road_name: String,
    pub road_class: String,
    pub distance_m: f64,
    pub duration_secs: f64,
    pub speed_kmh: f64,
}

/// A road segment in the network.
#[derive(Debug, Clone)]
pub struct RoadSegment {
    pub id: Uuid,
    pub name: String,
    pub road_class: String,
    pub geometry: Vec<[f64; 2]>, // [lon, lat] polyline
    pub speed_limit_kmh: f64,
    pub oneway: bool,
}

/// A road network for map matching.
pub struct RoadNetwork {
    pub segments: Vec<RoadSegment>,
}

impl RoadNetwork {
    pub fn new(segments: Vec<RoadSegment>) -> Self {
        Self { segments }
    }

    /// Find candidate road segments within search_radius_m of a point.
    /// Returns (segment_index, distance_m, snapped_point).
    fn candidates(&self, lon: f64, lat: f64, radius_m: f64) -> Vec<(usize, f64, [f64; 2])> {
        let mut result = Vec::new();
        for (idx, seg) in self.segments.iter().enumerate() {
            if seg.geometry.len() < 2 {
                continue;
            }
            let mut best_dist = f64::MAX;
            let mut best_snap = [lon, lat];
            for w in seg.geometry.windows(2) {
                let (d, snap) = point_to_segment_distance([lon, lat], w[0], w[1]);
                let d_m = haversine(lat, lon, snap[1], snap[0]);
                if d_m < best_dist {
                    best_dist = d_m;
                    best_snap = snap;
                }
                let _ = d; // use haversine-based distance for accuracy
            }
            if best_dist <= radius_m {
                result.push((idx, best_dist, best_snap));
            }
        }
        result
    }
}

/// Perpendicular distance from a point to a line segment, and the nearest point.
/// All coordinates are [lon, lat]. Returns (distance_degrees, snapped_point).
fn point_to_segment_distance(
    point: [f64; 2],
    seg_start: [f64; 2],
    seg_end: [f64; 2],
) -> (f64, [f64; 2]) {
    let dx = seg_end[0] - seg_start[0];
    let dy = seg_end[1] - seg_start[1];
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-14 {
        let d = ((point[0] - seg_start[0]).powi(2) + (point[1] - seg_start[1]).powi(2)).sqrt();
        return (d, seg_start);
    }
    // Project point onto the line defined by seg_start -> seg_end
    let t = ((point[0] - seg_start[0]) * dx + (point[1] - seg_start[1]) * dy) / len_sq;
    let t = t.clamp(0.0, 1.0);
    let snap = [seg_start[0] + t * dx, seg_start[1] + t * dy];
    let d = ((point[0] - snap[0]).powi(2) + (point[1] - snap[1]).powi(2)).sqrt();
    (d, snap)
}

/// HMM-based map matching using Viterbi algorithm.
pub fn match_trace_hmm(request: &MapMatchRequest, network: &RoadNetwork) -> MapMatchResult {
    let sigma_z = 4.07; // GPS noise standard deviation (meters)
    let beta = 2.0; // transition probability parameter

    let n_points = request.trace.len();
    if n_points == 0 {
        return MapMatchResult {
            id: Uuid::new_v4(),
            matched_points: vec![],
            matched_route: vec![],
            confidence: 0.0,
            total_distance_m: 0.0,
            road_segments: vec![],
        };
    }

    // Collect candidates for each GPS point
    let all_candidates: Vec<Vec<(usize, f64, [f64; 2])>> = request
        .trace
        .iter()
        .map(|gps| network.candidates(gps.longitude, gps.latitude, request.search_radius_m))
        .collect();

    // If any observation has no candidates, fall back to nearest for those
    for (i, cands) in all_candidates.iter().enumerate() {
        if cands.is_empty() {
            tracing::debug!(point_index = i, "no road candidates within search radius");
        }
    }

    // Viterbi algorithm
    // State: candidate index within each observation's candidate list
    // viterbi_prob[t][j] = log probability of best path ending at candidate j at time t
    // viterbi_prev[t][j] = predecessor candidate index at time t-1

    let mut viterbi_prob: Vec<Vec<f64>> = Vec::with_capacity(n_points);
    let mut viterbi_prev: Vec<Vec<usize>> = Vec::with_capacity(n_points);

    // Initialization: emission probabilities for first observation
    if all_candidates[0].is_empty() {
        // No candidates at all — return a minimal result
        return build_fallback_result(request);
    }
    let init_probs: Vec<f64> = all_candidates[0]
        .iter()
        .map(|(_, dist, _)| emission_log_prob(*dist, sigma_z))
        .collect();
    viterbi_prob.push(init_probs);
    viterbi_prev.push(vec![0; all_candidates[0].len()]);

    // Recursion
    for t in 1..n_points {
        if all_candidates[t].is_empty() {
            // No candidates — carry forward with penalty
            viterbi_prob.push(vec![]);
            viterbi_prev.push(vec![]);
            continue;
        }
        let prev_cands = &all_candidates[t - 1];
        let curr_cands = &all_candidates[t];
        let prev_probs = &viterbi_prob[t - 1];

        let gps_dist = haversine(
            request.trace[t - 1].latitude,
            request.trace[t - 1].longitude,
            request.trace[t].latitude,
            request.trace[t].longitude,
        );

        let mut probs = vec![f64::NEG_INFINITY; curr_cands.len()];
        let mut prevs = vec![0usize; curr_cands.len()];

        for (j, (curr_seg_idx, curr_dist, _)) in curr_cands.iter().enumerate() {
            let emission = emission_log_prob(*curr_dist, sigma_z);

            for (i, (prev_seg_idx, _, prev_snap)) in prev_cands.iter().enumerate() {
                if prev_probs.is_empty() {
                    continue;
                }
                let prev_snap_pt = *prev_snap;
                let curr_snap_pt = curr_cands[j].2;

                // Route distance approximated by great-circle between snapped points
                let route_dist = haversine(
                    prev_snap_pt[1],
                    prev_snap_pt[0],
                    curr_snap_pt[1],
                    curr_snap_pt[0],
                );

                let transition = transition_log_prob(route_dist, gps_dist, beta);

                // Connectivity bonus: same or adjacent segments are preferred
                let connectivity_bonus = if curr_seg_idx == prev_seg_idx {
                    0.5_f64.ln()
                } else {
                    0.0
                };

                let prob = prev_probs[i] + transition + emission + connectivity_bonus;
                if prob > probs[j] {
                    probs[j] = prob;
                    prevs[j] = i;
                }
            }
        }
        viterbi_prob.push(probs);
        viterbi_prev.push(prevs);
    }

    // Backtrack to find optimal sequence
    let mut best_sequence: Vec<Option<usize>> = vec![None; n_points];

    // Find best final state
    let last_valid = (0..n_points).rev().find(|&t| !viterbi_prob[t].is_empty());
    if let Some(last_t) = last_valid {
        let mut best_j = 0;
        let mut best_p = f64::NEG_INFINITY;
        for (j, &p) in viterbi_prob[last_t].iter().enumerate() {
            if p > best_p {
                best_p = p;
                best_j = j;
            }
        }
        best_sequence[last_t] = Some(best_j);

        // Trace back
        let mut j = best_j;
        for t in (1..=last_t).rev() {
            if !viterbi_prev[t].is_empty() {
                j = viterbi_prev[t][j];
                if !viterbi_prob[t - 1].is_empty() {
                    best_sequence[t - 1] = Some(j);
                }
            }
        }
    }

    // Build result from optimal sequence
    let mut matched_points = Vec::with_capacity(n_points);
    let mut route = Vec::with_capacity(n_points);
    let mut total_distance = 0.0;
    let mut segment_distances: std::collections::HashMap<usize, f64> =
        std::collections::HashMap::new();
    let mut confidence_sum = 0.0;
    let mut confidence_count = 0;

    for (t, gps) in request.trace.iter().enumerate() {
        if let Some(cand_idx) = best_sequence[t] {
            if cand_idx < all_candidates[t].len() {
                let (seg_idx, dist, snap) = &all_candidates[t][cand_idx];
                let seg = &network.segments[*seg_idx];
                matched_points.push(MatchedPoint {
                    original: [gps.longitude, gps.latitude],
                    snapped: *snap,
                    distance_from_road_m: *dist,
                    road_name: Some(seg.name.clone()),
                });
                route.push(*snap);

                if t > 0 {
                    if let Some(prev) = route.get(route.len().wrapping_sub(2)) {
                        let d = haversine(prev[1], prev[0], snap[1], snap[0]);
                        total_distance += d;
                        *segment_distances.entry(*seg_idx).or_insert(0.0) += d;
                    }
                }

                // Confidence based on emission probability (closer = better)
                let conf = (-0.5 * (dist / sigma_z).powi(2)).exp();
                confidence_sum += conf;
                confidence_count += 1;
                continue;
            }
        }
        // Fallback: no candidate matched
        matched_points.push(MatchedPoint {
            original: [gps.longitude, gps.latitude],
            snapped: [gps.longitude, gps.latitude],
            distance_from_road_m: 0.0,
            road_name: None,
        });
        route.push([gps.longitude, gps.latitude]);
    }

    let confidence = if confidence_count > 0 {
        (confidence_sum / confidence_count as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Build matched road segments
    let mut road_segments = Vec::new();
    for (seg_idx, dist_m) in &segment_distances {
        let seg = &network.segments[*seg_idx];
        let speed = match request.profile {
            MatchProfile::Driving => seg.speed_limit_kmh,
            MatchProfile::Cycling => seg.speed_limit_kmh.min(25.0),
            MatchProfile::Walking => 5.0,
        };
        let duration = if speed > 0.0 {
            dist_m / (speed * 1000.0 / 3600.0)
        } else {
            0.0
        };
        road_segments.push(MatchedSegment {
            road_name: seg.name.clone(),
            road_class: seg.road_class.clone(),
            distance_m: *dist_m,
            duration_secs: duration,
            speed_kmh: speed,
        });
    }
    // Sort segments by distance descending for consistent output
    road_segments.sort_by(|a, b| b.distance_m.partial_cmp(&a.distance_m).unwrap());

    MapMatchResult {
        id: Uuid::new_v4(),
        matched_points,
        matched_route: route,
        confidence,
        total_distance_m: total_distance,
        road_segments,
    }
}

/// Gaussian emission log-probability: how likely a GPS reading is given distance to road.
fn emission_log_prob(distance_m: f64, sigma_z: f64) -> f64 {
    -0.5 * (distance_m / sigma_z).powi(2) - (sigma_z * (2.0 * std::f64::consts::PI).sqrt()).ln()
}

/// Exponential transition log-probability: penalizes mismatch between route and GPS distances.
fn transition_log_prob(route_dist: f64, gps_dist: f64, beta: f64) -> f64 {
    let diff = (route_dist - gps_dist).abs();
    -diff / beta - beta.ln()
}

/// Fallback result when no candidates are found.
fn build_fallback_result(request: &MapMatchRequest) -> MapMatchResult {
    let matched_points: Vec<MatchedPoint> = request
        .trace
        .iter()
        .map(|gps| MatchedPoint {
            original: [gps.longitude, gps.latitude],
            snapped: [gps.longitude, gps.latitude],
            distance_from_road_m: 0.0,
            road_name: None,
        })
        .collect();
    let route: Vec<[f64; 2]> = request
        .trace
        .iter()
        .map(|gps| [gps.longitude, gps.latitude])
        .collect();
    MapMatchResult {
        id: Uuid::new_v4(),
        matched_points,
        matched_route: route,
        confidence: 0.0,
        total_distance_m: 0.0,
        road_segments: vec![],
    }
}

/// Create a demo road network around San Francisco.
pub fn demo_network() -> RoadNetwork {
    RoadNetwork::new(vec![
        RoadSegment {
            id: Uuid::new_v4(),
            name: "Market Street".into(),
            road_class: "primary".into(),
            geometry: vec![
                [-122.4260, 37.7700],
                [-122.4200, 37.7740],
                [-122.4150, 37.7770],
                [-122.4100, 37.7800],
                [-122.4050, 37.7830],
            ],
            speed_limit_kmh: 40.0,
            oneway: false,
        },
        RoadSegment {
            id: Uuid::new_v4(),
            name: "Mission Street".into(),
            road_class: "secondary".into(),
            geometry: vec![
                [-122.4260, 37.7685],
                [-122.4200, 37.7720],
                [-122.4150, 37.7750],
                [-122.4100, 37.7780],
                [-122.4050, 37.7810],
            ],
            speed_limit_kmh: 35.0,
            oneway: false,
        },
        RoadSegment {
            id: Uuid::new_v4(),
            name: "3rd Street".into(),
            road_class: "secondary".into(),
            geometry: vec![
                [-122.3940, 37.7700],
                [-122.3940, 37.7750],
                [-122.3940, 37.7800],
                [-122.3940, 37.7850],
            ],
            speed_limit_kmh: 35.0,
            oneway: false,
        },
        RoadSegment {
            id: Uuid::new_v4(),
            name: "Howard Street".into(),
            road_class: "secondary".into(),
            geometry: vec![
                [-122.4260, 37.7730],
                [-122.4200, 37.7730],
                [-122.4150, 37.7730],
                [-122.4100, 37.7730],
            ],
            speed_limit_kmh: 30.0,
            oneway: true,
        },
        RoadSegment {
            id: Uuid::new_v4(),
            name: "Folsom Street".into(),
            road_class: "secondary".into(),
            geometry: vec![
                [-122.4260, 37.7715],
                [-122.4200, 37.7715],
                [-122.4150, 37.7715],
                [-122.4100, 37.7715],
            ],
            speed_limit_kmh: 30.0,
            oneway: true,
        },
    ])
}

/// Perform map matching on a GPS trace using the demo road network.
pub fn match_trace(request: &MapMatchRequest) -> MapMatchResult {
    let network = demo_network();
    match_trace_hmm(request, &network)
}

/// Haversine distance in meters.
fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371_000.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_match() {
        let req = MapMatchRequest {
            trace: vec![
                GpsPoint {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    timestamp: Some(1000.0),
                    accuracy_m: Some(10.0),
                    speed_mps: None,
                    bearing_deg: None,
                },
                GpsPoint {
                    latitude: 37.7755,
                    longitude: -122.4185,
                    timestamp: Some(1005.0),
                    accuracy_m: Some(8.0),
                    speed_mps: None,
                    bearing_deg: None,
                },
                GpsPoint {
                    latitude: 37.7762,
                    longitude: -122.4170,
                    timestamp: Some(1010.0),
                    accuracy_m: Some(12.0),
                    speed_mps: None,
                    bearing_deg: None,
                },
            ],
            profile: MatchProfile::Driving,
            search_radius_m: 200.0,
        };
        let result = match_trace(&req);
        assert_eq!(result.matched_points.len(), 3);
        assert!(result.confidence > 0.0);
        assert!(result.total_distance_m > 0.0);
    }

    #[test]
    fn test_snapped_closer_to_road() {
        let req = MapMatchRequest {
            trace: vec![GpsPoint {
                latitude: 37.7749,
                longitude: -122.4194,
                timestamp: None,
                accuracy_m: None,
                speed_mps: None,
                bearing_deg: None,
            }],
            profile: MatchProfile::Walking,
            search_radius_m: 200.0,
        };
        let result = match_trace(&req);
        // Snapped point should be different from original
        let mp = &result.matched_points[0];
        assert_ne!(mp.original, mp.snapped);
    }

    #[test]
    fn test_road_segments() {
        let req = MapMatchRequest {
            trace: vec![
                GpsPoint {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    timestamp: None,
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
                GpsPoint {
                    latitude: 37.7780,
                    longitude: -122.4150,
                    timestamp: None,
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
            ],
            profile: MatchProfile::Cycling,
            search_radius_m: 200.0,
        };
        let result = match_trace(&req);
        assert!(!result.road_segments.is_empty());
    }

    #[test]
    fn test_hmm_different_traces_produce_different_results() {
        let network = demo_network();

        // Trace near Market Street
        let req_market = MapMatchRequest {
            trace: vec![
                GpsPoint {
                    latitude: 37.7740,
                    longitude: -122.4200,
                    timestamp: Some(0.0),
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
                GpsPoint {
                    latitude: 37.7770,
                    longitude: -122.4150,
                    timestamp: Some(5.0),
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
            ],
            profile: MatchProfile::Driving,
            search_radius_m: 200.0,
        };

        // Trace near Mission Street (further south)
        let req_mission = MapMatchRequest {
            trace: vec![
                GpsPoint {
                    latitude: 37.7720,
                    longitude: -122.4200,
                    timestamp: Some(0.0),
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
                GpsPoint {
                    latitude: 37.7750,
                    longitude: -122.4150,
                    timestamp: Some(5.0),
                    accuracy_m: None,
                    speed_mps: None,
                    bearing_deg: None,
                },
            ],
            profile: MatchProfile::Driving,
            search_radius_m: 200.0,
        };

        let result_market = match_trace_hmm(&req_market, &network);
        let result_mission = match_trace_hmm(&req_mission, &network);

        // Different traces should snap to different roads
        let market_road = result_market.matched_points[0].road_name.as_deref();
        let mission_road = result_mission.matched_points[0].road_name.as_deref();
        assert_ne!(
            market_road, mission_road,
            "different traces should match to different roads"
        );
    }

    #[test]
    fn test_point_to_segment_distance() {
        // Point directly on segment
        let (d, snap) = point_to_segment_distance([0.5, 0.0], [0.0, 0.0], [1.0, 0.0]);
        assert!(d < 1e-10);
        assert!((snap[0] - 0.5).abs() < 1e-10);

        // Point above segment midpoint
        let (d, snap) = point_to_segment_distance([0.5, 1.0], [0.0, 0.0], [1.0, 0.0]);
        assert!((d - 1.0).abs() < 1e-10);
        assert!((snap[0] - 0.5).abs() < 1e-10);
        assert!(snap[1].abs() < 1e-10);
    }

    #[test]
    fn test_empty_trace() {
        let req = MapMatchRequest {
            trace: vec![],
            profile: MatchProfile::Driving,
            search_radius_m: 50.0,
        };
        let result = match_trace(&req);
        assert!(result.matched_points.is_empty());
        assert_eq!(result.confidence, 0.0);
    }
}
