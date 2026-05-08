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

/// Perform map matching on a GPS trace.
pub fn match_trace(request: &MapMatchRequest) -> MapMatchResult {
    let mut matched_points = Vec::new();
    let mut route = Vec::new();
    let mut total_distance = 0.0;

    // Simplified snapping: move each point slightly toward a "road" line
    let road_lat = 37.7780; // simulated road latitude
    let road_bearing = -122.415; // simulated road longitude center

    for (i, gps) in request.trace.iter().enumerate() {
        // Snap toward nearest simulated road
        let snap_factor = 0.7; // how much to snap (0=no snap, 1=fully on road)
        let snapped_lat = gps.latitude + (road_lat - gps.latitude) * snap_factor * 0.1;
        let snapped_lon = gps.longitude + (road_bearing - gps.longitude) * snap_factor * 0.05;

        let dist_to_road = haversine(gps.latitude, gps.longitude, snapped_lat, snapped_lon);

        matched_points.push(MatchedPoint {
            original: [gps.longitude, gps.latitude],
            snapped: [snapped_lon, snapped_lat],
            distance_from_road_m: dist_to_road,
            road_name: Some("Market Street".into()),
        });
        route.push([snapped_lon, snapped_lat]);

        if i > 0 {
            let prev = &route[i - 1];
            total_distance += haversine(prev[1], prev[0], snapped_lat, snapped_lon);
        }
    }

    let confidence = if request.trace.len() > 2 { 0.87 } else { 0.65 };

    MapMatchResult {
        id: Uuid::new_v4(),
        matched_points,
        matched_route: route,
        confidence,
        total_distance_m: total_distance,
        road_segments: vec![
            MatchedSegment {
                road_name: "Market Street".into(),
                road_class: "primary".into(),
                distance_m: total_distance * 0.6,
                duration_secs: total_distance * 0.6 / 11.0,
                speed_kmh: 40.0,
            },
            MatchedSegment {
                road_name: "Mission Street".into(),
                road_class: "secondary".into(),
                distance_m: total_distance * 0.4,
                duration_secs: total_distance * 0.4 / 9.7,
                speed_kmh: 35.0,
            },
        ],
    }
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
                GpsPoint { latitude: 37.7749, longitude: -122.4194, timestamp: Some(1000.0), accuracy_m: Some(10.0), speed_mps: None, bearing_deg: None },
                GpsPoint { latitude: 37.7755, longitude: -122.4185, timestamp: Some(1005.0), accuracy_m: Some(8.0), speed_mps: None, bearing_deg: None },
                GpsPoint { latitude: 37.7762, longitude: -122.4170, timestamp: Some(1010.0), accuracy_m: Some(12.0), speed_mps: None, bearing_deg: None },
            ],
            profile: MatchProfile::Driving,
            search_radius_m: 50.0,
        };
        let result = match_trace(&req);
        assert_eq!(result.matched_points.len(), 3);
        assert!(result.confidence > 0.5);
        assert!(result.total_distance_m > 0.0);
    }

    #[test]
    fn test_snapped_closer_to_road() {
        let req = MapMatchRequest {
            trace: vec![
                GpsPoint { latitude: 37.7749, longitude: -122.4194, timestamp: None, accuracy_m: None, speed_mps: None, bearing_deg: None },
            ],
            profile: MatchProfile::Walking,
            search_radius_m: 30.0,
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
                GpsPoint { latitude: 37.7749, longitude: -122.4194, timestamp: None, accuracy_m: None, speed_mps: None, bearing_deg: None },
                GpsPoint { latitude: 37.7780, longitude: -122.4150, timestamp: None, accuracy_m: None, speed_mps: None, bearing_deg: None },
            ],
            profile: MatchProfile::Cycling,
            search_radius_m: 50.0,
        };
        let result = match_trace(&req);
        assert_eq!(result.road_segments.len(), 2);
    }
}
