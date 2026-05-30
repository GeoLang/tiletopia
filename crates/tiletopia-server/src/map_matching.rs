//! Map Matching — snap GPS traces to the road network.
//!
//! Delegates to `itinera_match` for the HMM-based Viterbi algorithm.
//! This module re-exports types and provides the server-facing API.

use itinera_match::match_trace as itinera_match_trace;
pub use itinera_match::{
    GpsPoint, MapMatchRequest, MapMatchResult, MatchProfile, MatchedPoint, MatchedSegment,
    RoadNetwork, RoadSegment,
};

/// Perform map matching on a GPS trace using the demo road network.
pub fn match_trace(request: &MapMatchRequest) -> MapMatchResult {
    let network = RoadNetwork::demo();
    itinera_match_trace(request, &network)
}

/// Perform map matching with a provided road network.
pub fn match_trace_with_network(
    request: &MapMatchRequest,
    network: &RoadNetwork,
) -> MapMatchResult {
    itinera_match_trace(request, network)
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
        let mp = &result.matched_points[0];
        assert_ne!(mp.original, mp.snapped);
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
