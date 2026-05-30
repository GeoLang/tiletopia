//! Routing / navigation engine.
//!
//! Wraps the `itinera` ecosystem crate for shortest-path computation
//! with turn-by-turn directions. Supports multiple profiles:
//! driving, walking, cycling.

use itinera_core::{Route as ItineraRoute, RouteStep as ItineraStep, StepManeuver, dijkstra};
use itinera_graph::{Coord, Edge, Graph, Node, NodeId, SpeedProfile};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Route computation profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoutingProfile {
    Driving,
    Walking,
    Cycling,
}

/// A route request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRequest {
    pub origin: [f64; 2],      // [longitude, latitude]
    pub destination: [f64; 2], // [longitude, latitude]
    pub profile: RoutingProfile,
    pub alternatives: bool,
}

/// A computed route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub id: Uuid,
    pub distance_m: f64,
    pub duration_secs: f64,
    pub geometry: Vec<[f64; 2]>, // [[lon, lat], ...]
    pub steps: Vec<RouteStep>,
    pub profile: RoutingProfile,
}

/// A step in turn-by-turn directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStep {
    pub instruction: String,
    pub distance_m: f64,
    pub duration_secs: f64,
    pub maneuver: Maneuver,
    pub road_name: Option<String>,
}

/// Turn maneuver type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Maneuver {
    Depart,
    TurnLeft,
    TurnRight,
    SlightLeft,
    SlightRight,
    SharpLeft,
    SharpRight,
    UTurn,
    Straight,
    Merge,
    RoundaboutExit(u8),
    Arrive,
}

/// Routing engine backed by itinera.
pub struct RoutingEngine {
    graph: Graph,
}

impl RoutingEngine {
    /// Create engine with demo data.
    pub fn new() -> Self {
        Self {
            graph: demo_graph(),
        }
    }

    /// Compute route between two points.
    pub fn compute_route(&self, request: &RouteRequest) -> Option<Route> {
        let origin_coord = Coord {
            lon: request.origin[0],
            lat: request.origin[1],
        };
        let dest_coord = Coord {
            lon: request.destination[0],
            lat: request.destination[1],
        };

        let origin_node = self.graph.nearest_node(origin_coord)?;
        let dest_node = self.graph.nearest_node(dest_coord)?;

        let profile = match request.profile {
            RoutingProfile::Driving => SpeedProfile::car(),
            RoutingProfile::Walking => SpeedProfile::pedestrian(),
            RoutingProfile::Cycling => SpeedProfile::bicycle(),
        };

        let itinera_route = dijkstra(&self.graph, origin_node, dest_node, &profile).ok()?;
        Some(convert_route(itinera_route, &request.profile))
    }

    /// Get graph statistics.
    pub fn stats(&self) -> RoutingStats {
        RoutingStats {
            node_count: self.graph.num_nodes(),
            edge_count: self.graph.num_edges(),
            graph_name: "SF Downtown".into(),
            bounds: [-122.42, 37.77, -122.39, 37.80],
        }
    }
}

impl Default for RoutingEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Routing engine statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub graph_name: String,
    pub bounds: [f64; 4],
}

/// Convert an itinera Route to the tiletopia API Route.
fn convert_route(r: ItineraRoute, profile: &RoutingProfile) -> Route {
    let geometry: Vec<[f64; 2]> = r.geometry.iter().map(|c| [c.lon, c.lat]).collect();
    let steps: Vec<RouteStep> = r
        .steps
        .iter()
        .map(|s| RouteStep {
            instruction: format_instruction(s),
            distance_m: s.distance_m,
            duration_secs: s.duration_s,
            maneuver: convert_maneuver(&s.maneuver),
            road_name: s.name.clone(),
        })
        .collect();

    Route {
        id: Uuid::new_v4(),
        distance_m: r.distance_m,
        duration_secs: r.duration_s,
        geometry,
        steps,
        profile: profile.clone(),
    }
}

fn convert_maneuver(m: &StepManeuver) -> Maneuver {
    match m {
        StepManeuver::Depart => Maneuver::Depart,
        StepManeuver::Arrive => Maneuver::Arrive,
        StepManeuver::TurnLeft => Maneuver::TurnLeft,
        StepManeuver::TurnRight => Maneuver::TurnRight,
        StepManeuver::TurnSlightLeft => Maneuver::SlightLeft,
        StepManeuver::TurnSlightRight => Maneuver::SlightRight,
        StepManeuver::TurnSharpLeft => Maneuver::SharpLeft,
        StepManeuver::TurnSharpRight => Maneuver::SharpRight,
        StepManeuver::Continue => Maneuver::Straight,
        StepManeuver::UTurn => Maneuver::UTurn,
        StepManeuver::Roundabout { exit_number } => Maneuver::RoundaboutExit(*exit_number),
        StepManeuver::Merge => Maneuver::Merge,
        StepManeuver::Fork { .. } => Maneuver::Straight,
    }
}

fn format_instruction(step: &ItineraStep) -> String {
    let road = step.name.as_deref().unwrap_or("road");
    match &step.maneuver {
        StepManeuver::Depart => format!("Depart on {road}"),
        StepManeuver::Arrive => "Arrive at destination".into(),
        StepManeuver::TurnLeft => format!("Turn left onto {road}"),
        StepManeuver::TurnRight => format!("Turn right onto {road}"),
        StepManeuver::TurnSlightLeft => format!("Slight left onto {road}"),
        StepManeuver::TurnSlightRight => format!("Slight right onto {road}"),
        StepManeuver::Continue => format!("Continue on {road}"),
        _ => format!("Continue on {road}"),
    }
}

/// Demo routing graph (simplified SF downtown) built with itinera types.
fn demo_graph() -> Graph {
    let nodes = vec![
        Node {
            id: NodeId(0),
            coord: Coord {
                lat: 37.7749,
                lon: -122.4194,
            },
            osm_id: 1,
            ch_level: 0,
        },
        Node {
            id: NodeId(1),
            coord: Coord {
                lat: 37.7760,
                lon: -122.4180,
            },
            osm_id: 2,
            ch_level: 0,
        },
        Node {
            id: NodeId(2),
            coord: Coord {
                lat: 37.7780,
                lon: -122.4160,
            },
            osm_id: 3,
            ch_level: 0,
        },
        Node {
            id: NodeId(3),
            coord: Coord {
                lat: 37.7790,
                lon: -122.4140,
            },
            osm_id: 4,
            ch_level: 0,
        },
        Node {
            id: NodeId(4),
            coord: Coord {
                lat: 37.7800,
                lon: -122.4100,
            },
            osm_id: 5,
            ch_level: 0,
        },
        Node {
            id: NodeId(5),
            coord: Coord {
                lat: 37.7770,
                lon: -122.4150,
            },
            osm_id: 6,
            ch_level: 0,
        },
        Node {
            id: NodeId(6),
            coord: Coord {
                lat: 37.7755,
                lon: -122.4120,
            },
            osm_id: 7,
            ch_level: 0,
        },
    ];

    let edges = vec![
        // Market Street (bidirectional)
        Edge {
            from: NodeId(0),
            to: NodeId(1),
            distance_m: 150.0,
            duration_s: 12.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(1),
            to: NodeId(0),
            distance_m: 150.0,
            duration_s: 12.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(1),
            to: NodeId(2),
            distance_m: 230.0,
            duration_s: 18.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(2),
            to: NodeId(1),
            distance_m: 230.0,
            duration_s: 18.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(2),
            to: NodeId(3),
            distance_m: 180.0,
            duration_s: 14.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(3),
            to: NodeId(2),
            distance_m: 180.0,
            duration_s: 14.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(3),
            to: NodeId(4),
            distance_m: 350.0,
            duration_s: 28.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(4),
            to: NodeId(3),
            distance_m: 350.0,
            duration_s: 28.0,
            way_id: 100,
            road_class: 3,
            oneway: false,
            name: Some("Market Street".into()),
            geometry: vec![],
        },
        // Mission Street (bidirectional)
        Edge {
            from: NodeId(0),
            to: NodeId(5),
            distance_m: 280.0,
            duration_s: 22.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(5),
            to: NodeId(0),
            distance_m: 280.0,
            duration_s: 22.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(5),
            to: NodeId(6),
            distance_m: 320.0,
            duration_s: 25.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(6),
            to: NodeId(5),
            distance_m: 320.0,
            duration_s: 25.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(6),
            to: NodeId(4),
            distance_m: 500.0,
            duration_s: 40.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(4),
            to: NodeId(6),
            distance_m: 500.0,
            duration_s: 40.0,
            way_id: 101,
            road_class: 4,
            oneway: false,
            name: Some("Mission Street".into()),
            geometry: vec![],
        },
        // Cross streets (bidirectional)
        Edge {
            from: NodeId(1),
            to: NodeId(5),
            distance_m: 200.0,
            duration_s: 16.0,
            way_id: 102,
            road_class: 5,
            oneway: false,
            name: Some("3rd Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(5),
            to: NodeId(1),
            distance_m: 200.0,
            duration_s: 16.0,
            way_id: 102,
            road_class: 5,
            oneway: false,
            name: Some("3rd Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(2),
            to: NodeId(6),
            distance_m: 350.0,
            duration_s: 28.0,
            way_id: 103,
            road_class: 5,
            oneway: false,
            name: Some("5th Street".into()),
            geometry: vec![],
        },
        Edge {
            from: NodeId(6),
            to: NodeId(2),
            distance_m: 350.0,
            duration_s: 28.0,
            way_id: 103,
            road_class: 5,
            oneway: false,
            name: Some("5th Street".into()),
            geometry: vec![],
        },
    ];

    Graph::build(nodes, edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_route() {
        let engine = RoutingEngine::new();
        let req = RouteRequest {
            origin: [-122.4194, 37.7749],
            destination: [-122.4100, 37.7800],
            profile: RoutingProfile::Driving,
            alternatives: false,
        };
        let route = engine.compute_route(&req).unwrap();
        assert!(route.distance_m > 0.0);
        assert!(!route.steps.is_empty());
    }

    #[test]
    fn test_walking_route() {
        let engine = RoutingEngine::new();
        let req = RouteRequest {
            origin: [-122.4194, 37.7749],
            destination: [-122.4140, 37.7790],
            profile: RoutingProfile::Walking,
            alternatives: false,
        };
        let route = engine.compute_route(&req).unwrap();
        assert!(route.duration_secs > 0.0);
        assert_eq!(route.profile, RoutingProfile::Walking);
    }

    #[test]
    fn test_stats() {
        let engine = RoutingEngine::new();
        let stats = engine.stats();
        assert_eq!(stats.node_count, 7);
        assert_eq!(stats.edge_count, 18);
    }
}
