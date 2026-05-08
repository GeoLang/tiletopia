//! Routing / navigation engine.
//!
//! Provides shortest-path computation over road/pedestrian networks
//! with turn-by-turn directions. Supports multiple profiles:
//! driving, walking, cycling.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use uuid::Uuid;

/// A routing graph (road network).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingGraph {
    pub id: Uuid,
    pub name: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub bounds: [f64; 4], // [west, south, east, north]
}

/// A node in the routing graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: u64,
    pub latitude: f64,
    pub longitude: f64,
    pub node_type: NodeType,
}

/// Node type in the network.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NodeType {
    Intersection,
    DeadEnd,
    RoadJunction,
    TrafficSignal,
    RoundaboutEntry,
}

/// An edge (road segment) in the routing graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from_node: u64,
    pub to_node: u64,
    pub distance_m: f64,
    pub duration_secs: f64,
    pub road_class: RoadClass,
    pub name: Option<String>,
    pub oneway: bool,
    pub max_speed_kmh: u8,
}

/// Road classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoadClass {
    Motorway,
    Trunk,
    Primary,
    Secondary,
    Tertiary,
    Residential,
    Service,
    Footway,
    Cycleway,
    Path,
}

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

/// Routing engine.
pub struct RoutingEngine {
    graph: RoutingGraph,
}

/// Priority queue state for Dijkstra.
#[derive(Clone, PartialEq)]
struct State {
    cost: u64, // in milliseconds
    node_id: u64,
}

impl Eq for State {}

impl Ord for State {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.cmp(&self.cost) // reversed for min-heap
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
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
        // Find nearest nodes to origin and destination
        let origin_node = self.nearest_node(request.origin[0], request.origin[1])?;
        let dest_node = self.nearest_node(request.destination[0], request.destination[1])?;

        // Run Dijkstra
        let path = self.dijkstra(origin_node, dest_node, &request.profile)?;

        // Build route from path
        Some(self.build_route(&path, &request.profile))
    }

    /// Find nearest graph node to a coordinate.
    fn nearest_node(&self, longitude: f64, latitude: f64) -> Option<u64> {
        self.graph
            .nodes
            .iter()
            .min_by(|a, b| {
                let da = (a.longitude - longitude).powi(2) + (a.latitude - latitude).powi(2);
                let db = (b.longitude - longitude).powi(2) + (b.latitude - latitude).powi(2);
                da.partial_cmp(&db).unwrap_or(Ordering::Equal)
            })
            .map(|n| n.id)
    }

    /// Dijkstra shortest path.
    fn dijkstra(&self, start: u64, end: u64, profile: &RoutingProfile) -> Option<Vec<u64>> {
        let mut dist: HashMap<u64, u64> = HashMap::new();
        let mut prev: HashMap<u64, u64> = HashMap::new();
        let mut heap = BinaryHeap::new();

        dist.insert(start, 0);
        heap.push(State {
            cost: 0,
            node_id: start,
        });

        // Build adjacency from edges
        let mut adj: HashMap<u64, Vec<&GraphEdge>> = HashMap::new();
        for edge in &self.graph.edges {
            adj.entry(edge.from_node).or_default().push(edge);
            if !edge.oneway {
                // Create reverse direction implicitly
                adj.entry(edge.to_node).or_default().push(edge);
            }
        }

        while let Some(State { cost, node_id }) = heap.pop() {
            if node_id == end {
                // Reconstruct path
                let mut path = vec![end];
                let mut current = end;
                while let Some(&p) = prev.get(&current) {
                    path.push(p);
                    current = p;
                }
                path.reverse();
                return Some(path);
            }

            if cost > *dist.get(&node_id).unwrap_or(&u64::MAX) {
                continue;
            }

            if let Some(edges) = adj.get(&node_id) {
                for edge in edges {
                    let neighbor = if edge.from_node == node_id {
                        edge.to_node
                    } else {
                        edge.from_node
                    };

                    let edge_cost = self.edge_cost(edge, profile);
                    let next_cost = cost + edge_cost;

                    if next_cost < *dist.get(&neighbor).unwrap_or(&u64::MAX) {
                        dist.insert(neighbor, next_cost);
                        prev.insert(neighbor, node_id);
                        heap.push(State {
                            cost: next_cost,
                            node_id: neighbor,
                        });
                    }
                }
            }
        }

        None // no path found
    }

    /// Calculate edge cost based on profile.
    fn edge_cost(&self, edge: &GraphEdge, profile: &RoutingProfile) -> u64 {
        match profile {
            RoutingProfile::Driving => (edge.duration_secs * 1000.0) as u64,
            RoutingProfile::Walking => {
                // Walking speed ~5 km/h
                (edge.distance_m / 1.4 * 1000.0) as u64
            }
            RoutingProfile::Cycling => {
                // Cycling ~15 km/h, prefer cycleways
                let factor = if edge.road_class == RoadClass::Cycleway {
                    0.8
                } else {
                    1.0
                };
                (edge.distance_m / 4.2 * 1000.0 * factor) as u64
            }
        }
    }

    /// Build route response from node path.
    fn build_route(&self, path: &[u64], profile: &RoutingProfile) -> Route {
        let node_map: HashMap<u64, &GraphNode> =
            self.graph.nodes.iter().map(|n| (n.id, n)).collect();
        let mut geometry = Vec::new();
        let mut total_distance = 0.0;
        let mut total_duration = 0.0;
        let mut steps = Vec::new();

        for node_id in path {
            if let Some(node) = node_map.get(node_id) {
                geometry.push([node.longitude, node.latitude]);
            }
        }

        // Build steps from edges
        for window in path.windows(2) {
            let (from, to) = (window[0], window[1]);
            if let Some(edge) = self.graph.edges.iter().find(|e| {
                (e.from_node == from && e.to_node == to)
                    || (!e.oneway && e.from_node == to && e.to_node == from)
            }) {
                total_distance += edge.distance_m;
                total_duration += match profile {
                    RoutingProfile::Driving => edge.duration_secs,
                    RoutingProfile::Walking => edge.distance_m / 1.4,
                    RoutingProfile::Cycling => edge.distance_m / 4.2,
                };
                steps.push(RouteStep {
                    instruction: format!("Continue on {}", edge.name.as_deref().unwrap_or("road")),
                    distance_m: edge.distance_m,
                    duration_secs: edge.duration_secs,
                    maneuver: Maneuver::Straight,
                    road_name: edge.name.clone(),
                });
            }
        }

        // Add depart and arrive
        if !steps.is_empty() {
            steps[0].maneuver = Maneuver::Depart;
            steps[0].instruction = format!(
                "Depart on {}",
                steps[0].road_name.as_deref().unwrap_or("road")
            );
        }
        steps.push(RouteStep {
            instruction: "Arrive at destination".into(),
            distance_m: 0.0,
            duration_secs: 0.0,
            maneuver: Maneuver::Arrive,
            road_name: None,
        });

        Route {
            id: Uuid::new_v4(),
            distance_m: total_distance,
            duration_secs: total_duration,
            geometry,
            steps,
            profile: profile.clone(),
        }
    }

    /// Get graph statistics.
    pub fn stats(&self) -> RoutingStats {
        RoutingStats {
            node_count: self.graph.nodes.len(),
            edge_count: self.graph.edges.len(),
            graph_name: self.graph.name.clone(),
            bounds: self.graph.bounds,
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

/// Demo routing graph (simplified SF downtown).
fn demo_graph() -> RoutingGraph {
    RoutingGraph {
        id: Uuid::new_v4(),
        name: "SF Downtown".into(),
        bounds: [-122.42, 37.77, -122.39, 37.80],
        nodes: vec![
            GraphNode {
                id: 1,
                latitude: 37.7749,
                longitude: -122.4194,
                node_type: NodeType::Intersection,
            },
            GraphNode {
                id: 2,
                latitude: 37.7760,
                longitude: -122.4180,
                node_type: NodeType::TrafficSignal,
            },
            GraphNode {
                id: 3,
                latitude: 37.7780,
                longitude: -122.4160,
                node_type: NodeType::Intersection,
            },
            GraphNode {
                id: 4,
                latitude: 37.7790,
                longitude: -122.4140,
                node_type: NodeType::TrafficSignal,
            },
            GraphNode {
                id: 5,
                latitude: 37.7800,
                longitude: -122.4100,
                node_type: NodeType::Intersection,
            },
            GraphNode {
                id: 6,
                latitude: 37.7770,
                longitude: -122.4150,
                node_type: NodeType::Intersection,
            },
            GraphNode {
                id: 7,
                latitude: 37.7755,
                longitude: -122.4120,
                node_type: NodeType::RoadJunction,
            },
        ],
        edges: vec![
            GraphEdge {
                from_node: 1,
                to_node: 2,
                distance_m: 150.0,
                duration_secs: 12.0,
                road_class: RoadClass::Primary,
                name: Some("Market Street".into()),
                oneway: false,
                max_speed_kmh: 40,
            },
            GraphEdge {
                from_node: 2,
                to_node: 3,
                distance_m: 230.0,
                duration_secs: 18.0,
                road_class: RoadClass::Primary,
                name: Some("Market Street".into()),
                oneway: false,
                max_speed_kmh: 40,
            },
            GraphEdge {
                from_node: 3,
                to_node: 4,
                distance_m: 180.0,
                duration_secs: 14.0,
                road_class: RoadClass::Primary,
                name: Some("Market Street".into()),
                oneway: false,
                max_speed_kmh: 40,
            },
            GraphEdge {
                from_node: 4,
                to_node: 5,
                distance_m: 350.0,
                duration_secs: 28.0,
                road_class: RoadClass::Primary,
                name: Some("Market Street".into()),
                oneway: false,
                max_speed_kmh: 40,
            },
            GraphEdge {
                from_node: 1,
                to_node: 6,
                distance_m: 280.0,
                duration_secs: 22.0,
                road_class: RoadClass::Secondary,
                name: Some("Mission Street".into()),
                oneway: false,
                max_speed_kmh: 35,
            },
            GraphEdge {
                from_node: 6,
                to_node: 7,
                distance_m: 320.0,
                duration_secs: 25.0,
                road_class: RoadClass::Secondary,
                name: Some("Mission Street".into()),
                oneway: false,
                max_speed_kmh: 35,
            },
            GraphEdge {
                from_node: 7,
                to_node: 5,
                distance_m: 500.0,
                duration_secs: 40.0,
                road_class: RoadClass::Secondary,
                name: Some("Mission Street".into()),
                oneway: false,
                max_speed_kmh: 35,
            },
            GraphEdge {
                from_node: 2,
                to_node: 6,
                distance_m: 200.0,
                duration_secs: 16.0,
                road_class: RoadClass::Tertiary,
                name: Some("3rd Street".into()),
                oneway: false,
                max_speed_kmh: 30,
            },
            GraphEdge {
                from_node: 3,
                to_node: 7,
                distance_m: 350.0,
                duration_secs: 28.0,
                road_class: RoadClass::Tertiary,
                name: Some("5th Street".into()),
                oneway: false,
                max_speed_kmh: 30,
            },
        ],
    }
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
        assert_eq!(stats.edge_count, 9);
    }
}
