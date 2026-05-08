//! Drone/UAV Flight Planning — mission paths for aerial surveys.
//!
//! Generates optimized flight plans for photogrammetry, LiDAR scanning,
//! and inspection missions over defined areas of interest.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A flight mission plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlightPlan {
    pub id: Uuid,
    pub name: String,
    pub mission_type: MissionType,
    pub area_of_interest: Vec<[f64; 2]>, // polygon [lon, lat]
    pub waypoints: Vec<Waypoint>,
    pub parameters: FlightParameters,
    pub statistics: FlightStats,
}

/// Mission type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MissionType {
    /// Grid/lawnmower for orthophoto
    GridMapping,
    /// Double grid for 3D reconstruction
    DoubleGrid,
    /// Circular orbit for structure inspection
    Orbit,
    /// Corridor along a linear feature (road, pipeline)
    Corridor,
    /// Manual waypoint-based mission
    FreeForm,
}

/// A waypoint in the flight path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Waypoint {
    pub sequence: u32,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: f64, // AGL (above ground level)
    pub speed_mps: f64,
    pub action: WaypointAction,
    pub gimbal_pitch_deg: f64, // -90 = nadir, 0 = horizon
}

/// Action at a waypoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WaypointAction {
    Fly,
    TakePhoto,
    StartVideo,
    StopVideo,
    Hover(f64), // hover duration in seconds
    ChangeSpeed(f64),
}

/// Flight parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlightParameters {
    pub altitude_m: f64,
    pub speed_mps: f64,
    pub overlap_front_pct: f64, // 70–80% typical
    pub overlap_side_pct: f64,  // 60–70% typical
    pub gsd_cm_per_px: f64,     // ground sampling distance
    pub camera: CameraSpec,
    pub max_flight_time_min: f64,
    pub home_position: [f64; 2], // takeoff/landing [lon, lat]
}

/// Camera specification for GSD calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraSpec {
    pub name: String,
    pub sensor_width_mm: f64,
    pub sensor_height_mm: f64,
    pub focal_length_mm: f64,
    pub image_width_px: u32,
    pub image_height_px: u32,
}

/// Computed flight statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlightStats {
    pub total_distance_m: f64,
    pub estimated_flight_time_min: f64,
    pub photo_count: u32,
    pub coverage_area_m2: f64,
    pub battery_usage_pct: f64,
    pub waypoint_count: u32,
}

/// Generate a grid mapping flight plan.
pub fn generate_grid_plan(
    area: &[[f64; 2]],
    altitude_m: f64,
    overlap_front: f64,
    overlap_side: f64,
) -> FlightPlan {
    let camera = CameraSpec {
        name: "DJI Mavic 3 Enterprise".into(),
        sensor_width_mm: 17.3,
        sensor_height_mm: 13.0,
        focal_length_mm: 12.3,
        image_width_px: 5280,
        image_height_px: 3956,
    };

    // Calculate GSD
    let gsd_cm = (altitude_m * camera.sensor_width_mm * 100.0)
        / (camera.focal_length_mm * camera.image_width_px as f64);

    // Calculate spacing between lines
    let footprint_width_m = altitude_m * camera.sensor_width_mm / camera.focal_length_mm;
    let footprint_height_m = altitude_m * camera.sensor_height_mm / camera.focal_length_mm;
    let line_spacing_m = footprint_width_m * (1.0 - overlap_side / 100.0);
    let photo_spacing_m = footprint_height_m * (1.0 - overlap_front / 100.0);

    // Generate waypoints (simplified grid over bounding box)
    let (min_lon, max_lon, min_lat, max_lat) = bounding_box(area);
    let width_m = haversine(min_lat, min_lon, min_lat, max_lon);
    let height_m = haversine(min_lat, min_lon, max_lat, min_lon);

    let num_lines = (width_m / line_spacing_m).ceil() as u32;
    let photos_per_line = (height_m / photo_spacing_m).ceil() as u32;
    let total_photos = num_lines * photos_per_line;

    let mut waypoints = Vec::new();
    let mut seq = 0u32;
    for line in 0..num_lines.min(20) {
        // cap for demo
        let lon = min_lon + (max_lon - min_lon) * (line as f64 / num_lines.max(1) as f64);
        let (start_lat, end_lat) = if line % 2 == 0 {
            (min_lat, max_lat)
        } else {
            (max_lat, min_lat)
        };

        waypoints.push(Waypoint {
            sequence: seq,
            latitude: start_lat,
            longitude: lon,
            altitude_m,
            speed_mps: 8.0,
            action: WaypointAction::TakePhoto,
            gimbal_pitch_deg: -90.0,
        });
        seq += 1;

        waypoints.push(Waypoint {
            sequence: seq,
            latitude: end_lat,
            longitude: lon,
            altitude_m,
            speed_mps: 8.0,
            action: WaypointAction::TakePhoto,
            gimbal_pitch_deg: -90.0,
        });
        seq += 1;
    }

    let total_distance_m = num_lines as f64 * height_m + (num_lines - 1) as f64 * line_spacing_m;
    let flight_time_min = total_distance_m / (8.0 * 60.0); // at 8 m/s

    FlightPlan {
        id: Uuid::new_v4(),
        name: "Grid Survey".into(),
        mission_type: MissionType::GridMapping,
        area_of_interest: area.to_vec(),
        waypoints,
        parameters: FlightParameters {
            altitude_m,
            speed_mps: 8.0,
            overlap_front_pct: overlap_front,
            overlap_side_pct: overlap_side,
            gsd_cm_per_px: gsd_cm,
            camera,
            max_flight_time_min: 40.0,
            home_position: [min_lon, min_lat],
        },
        statistics: FlightStats {
            total_distance_m,
            estimated_flight_time_min: flight_time_min,
            photo_count: total_photos,
            coverage_area_m2: width_m * height_m,
            battery_usage_pct: (flight_time_min / 40.0 * 100.0).min(100.0),
            waypoint_count: seq,
        },
    }
}

/// Generate orbit mission around a structure.
pub fn generate_orbit_plan(center: [f64; 2], radius_m: f64, altitude_m: f64) -> FlightPlan {
    let num_points = 12u32;
    let radius_deg = radius_m / 111320.0;
    let mut waypoints = Vec::new();

    for i in 0..num_points {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (num_points as f64);
        waypoints.push(Waypoint {
            sequence: i,
            latitude: center[1] + radius_deg * angle.sin(),
            longitude: center[0] + radius_deg * angle.cos(),
            altitude_m,
            speed_mps: 3.0,
            action: WaypointAction::TakePhoto,
            gimbal_pitch_deg: -45.0,
        });
    }

    let circumference = 2.0 * std::f64::consts::PI * radius_m;
    let flight_time = circumference / (3.0 * 60.0);

    FlightPlan {
        id: Uuid::new_v4(),
        name: "Structure Orbit".into(),
        mission_type: MissionType::Orbit,
        area_of_interest: vec![center],
        waypoints,
        parameters: FlightParameters {
            altitude_m,
            speed_mps: 3.0,
            overlap_front_pct: 80.0,
            overlap_side_pct: 0.0,
            gsd_cm_per_px: 1.5,
            camera: CameraSpec {
                name: "DJI Mavic 3 Enterprise".into(),
                sensor_width_mm: 17.3,
                sensor_height_mm: 13.0,
                focal_length_mm: 12.3,
                image_width_px: 5280,
                image_height_px: 3956,
            },
            max_flight_time_min: 40.0,
            home_position: center,
        },
        statistics: FlightStats {
            total_distance_m: circumference,
            estimated_flight_time_min: flight_time,
            photo_count: num_points,
            coverage_area_m2: std::f64::consts::PI * radius_m * radius_m,
            battery_usage_pct: (flight_time / 40.0 * 100.0).min(100.0),
            waypoint_count: num_points,
        },
    }
}

fn bounding_box(area: &[[f64; 2]]) -> (f64, f64, f64, f64) {
    let min_lon = area.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
    let max_lon = area.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max);
    let min_lat = area.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
    let max_lat = area.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max);
    (min_lon, max_lon, min_lat, max_lat)
}

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
    fn test_grid_plan() {
        let area = vec![
            [-122.42, 37.77],
            [-122.41, 37.77],
            [-122.41, 37.78],
            [-122.42, 37.78],
        ];
        let plan = generate_grid_plan(&area, 80.0, 75.0, 65.0);
        assert_eq!(plan.mission_type, MissionType::GridMapping);
        assert!(plan.statistics.photo_count > 0);
        assert!(plan.parameters.gsd_cm_per_px > 0.0);
        assert!(!plan.waypoints.is_empty());
    }

    #[test]
    fn test_orbit_plan() {
        let plan = generate_orbit_plan([-122.4194, 37.7749], 50.0, 30.0);
        assert_eq!(plan.mission_type, MissionType::Orbit);
        assert_eq!(plan.waypoints.len(), 12);
        assert!(plan.statistics.total_distance_m > 300.0);
    }

    #[test]
    fn test_gsd_calculation() {
        let area = vec![[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
        let plan = generate_grid_plan(&area, 100.0, 75.0, 65.0);
        // At 100m altitude with standard camera, GSD should be ~2-3 cm
        assert!(plan.parameters.gsd_cm_per_px > 1.0);
        assert!(plan.parameters.gsd_cm_per_px < 5.0);
    }
}
