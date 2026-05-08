//! Isochrone / Service Area analysis.
//!
//! Computes reachability polygons: "show me everywhere reachable
//! within N minutes" by driving, walking, or cycling.

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

/// Compute isochrone contours from an origin point.
pub fn compute_isochrone(request: &IsochroneRequest) -> IsochroneResult {
    let contours = request
        .contours_minutes
        .iter()
        .map(|&minutes| {
            let radius_km = request.profile.speed_kmh() * (minutes as f64 / 60.0);
            let radius_deg = radius_km / 111.32; // approx degrees at equator
            // Generate a simplified polygon (circle approximation with 16 vertices)
            let polygon = generate_polygon(request.origin, radius_deg, 16);
            let area_km2 = std::f64::consts::PI * radius_km * radius_km;
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

/// Generate an approximate polygon (circle) around a point.
fn generate_polygon(center: [f64; 2], radius_deg: f64, num_points: usize) -> Vec<[f64; 2]> {
    let mut points = Vec::with_capacity(num_points + 1);
    for i in 0..num_points {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (num_points as f64);
        let lon = center[0] + radius_deg * angle.cos();
        let lat = center[1] + radius_deg * angle.sin();
        points.push([lon, lat]);
    }
    points.push(points[0]); // close the ring
    points
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
}
