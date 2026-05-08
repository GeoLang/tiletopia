//! Elevation API — point and profile elevation lookup from terrain data.

use serde::{Deserialize, Serialize};

/// Elevation at a single point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationPoint {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    pub resolution_m: f64, // DEM resolution used
    pub source: ElevationSource,
}

/// Elevation profile along a path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationProfile {
    pub points: Vec<ElevationPoint>,
    pub total_distance_m: f64,
    pub elevation_gain_m: f64,
    pub elevation_loss_m: f64,
    pub min_elevation_m: f64,
    pub max_elevation_m: f64,
}

/// Data source for elevation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ElevationSource {
    Srtm30m,
    Copernicus30m,
    LocalDem,
    Lidar1m,
}

/// Get elevation at a single point (demo: uses simplified model).
pub fn get_elevation(latitude: f64, longitude: f64) -> ElevationPoint {
    // Simulate elevation based on position (SF Bay Area terrain-like)
    let base = 50.0;
    let variation = ((latitude * 1000.0).sin() * 30.0) + ((longitude * 800.0).cos() * 20.0);
    ElevationPoint {
        latitude,
        longitude,
        elevation_m: base + variation,
        resolution_m: 30.0,
        source: ElevationSource::Srtm30m,
    }
}

/// Get elevation along a path (list of [lat, lon] pairs).
pub fn get_profile(path: &[[f64; 2]]) -> ElevationProfile {
    let mut points = Vec::new();
    let mut total_distance = 0.0;
    let mut elevation_gain = 0.0;
    let mut elevation_loss = 0.0;

    for (i, coord) in path.iter().enumerate() {
        let pt = get_elevation(coord[0], coord[1]);
        if i > 0 {
            let prev = &points[i - 1];
            let dist = haversine_distance(prev, &pt);
            total_distance += dist;
            let diff: f64 = pt.elevation_m - prev.elevation_m;
            if diff > 0.0 {
                elevation_gain += diff;
            } else {
                elevation_loss += diff.abs();
            }
        }
        points.push(pt);
    }

    let min_elev = points
        .iter()
        .map(|p| p.elevation_m)
        .fold(f64::INFINITY, f64::min);
    let max_elev = points
        .iter()
        .map(|p| p.elevation_m)
        .fold(f64::NEG_INFINITY, f64::max);

    ElevationProfile {
        points,
        total_distance_m: total_distance,
        elevation_gain_m: elevation_gain,
        elevation_loss_m: elevation_loss,
        min_elevation_m: min_elev,
        max_elevation_m: max_elev,
    }
}

/// Batch elevation lookup.
pub fn get_elevations(locations: &[[f64; 2]]) -> Vec<ElevationPoint> {
    locations
        .iter()
        .map(|loc| get_elevation(loc[0], loc[1]))
        .collect()
}

/// Haversine distance between two elevation points (in meters).
fn haversine_distance(a: &ElevationPoint, b: &ElevationPoint) -> f64 {
    let r = 6_371_000.0; // Earth radius in meters
    let dlat = (b.latitude - a.latitude).to_radians();
    let dlon = (b.longitude - a.longitude).to_radians();
    let lat1 = a.latitude.to_radians();
    let lat2 = b.latitude.to_radians();
    let a_val = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a_val.sqrt().asin();
    r * c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_elevation() {
        let pt = get_elevation(37.7749, -122.4194);
        assert!((pt.latitude - 37.7749).abs() < 0.0001);
        assert!(pt.elevation_m > -500.0 && pt.elevation_m < 5000.0);
    }

    #[test]
    fn test_get_profile() {
        let path = vec![
            [37.7749, -122.4194],
            [37.7760, -122.4180],
            [37.7780, -122.4160],
        ];
        let profile = get_profile(&path);
        assert_eq!(profile.points.len(), 3);
        assert!(profile.total_distance_m > 0.0);
        assert!(profile.min_elevation_m <= profile.max_elevation_m);
    }

    #[test]
    fn test_batch_elevations() {
        let locations = vec![[37.7749, -122.4194], [40.7128, -74.0060]];
        let results = get_elevations(&locations);
        assert_eq!(results.len(), 2);
    }
}
