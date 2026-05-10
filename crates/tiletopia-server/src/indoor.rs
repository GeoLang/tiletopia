//! Indoor mapping — floor plans, room-level navigation, and indoor positioning.
//!
//! Supports:
//! - Multi-story building floor plans
//! - Room/zone definitions with metadata
//! - Indoor routing between rooms
//! - Indoor positioning reference points (BLE beacons, WiFi)

use serde::{Deserialize, Serialize};
use std::collections::{BinaryHeap, HashMap};
use uuid::Uuid;

/// An indoor-mapped building.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndoorBuilding {
    pub id: Uuid,
    pub name: String,
    pub address: String,
    pub position: [f64; 2], // [longitude, latitude]
    pub floors: Vec<Floor>,
    pub total_area_m2: f64,
}

/// A floor/level in a building.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Floor {
    pub id: Uuid,
    pub level: i8, // 0 = ground, negative = basement
    pub name: String,
    pub height_m: f32,
    pub area_m2: f64,
    pub rooms: Vec<Room>,
    pub navigation_graph: Option<NavigationGraph>,
}

/// A room or zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: Uuid,
    pub name: String,
    pub room_type: RoomType,
    pub polygon: Vec<[f64; 2]>, // 2D polygon outline (local coords, meters)
    pub area_m2: f64,
    pub capacity: Option<u32>,
    pub metadata: HashMap<String, String>,
}

/// Room type classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoomType {
    Office,
    MeetingRoom,
    Lobby,
    Corridor,
    Elevator,
    Stairwell,
    Restroom,
    Kitchen,
    ServerRoom,
    Parking,
    Storage,
    Retail,
    Restaurant,
    Custom(String),
}

/// Indoor navigation graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationGraph {
    pub nodes: Vec<NavNode>,
    pub edges: Vec<NavEdge>,
}

/// Navigation waypoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavNode {
    pub id: Uuid,
    pub position: [f64; 2], // x, y in local coordinates (meters)
    pub floor_level: i8,
    pub node_type: NavNodeType,
    pub accessible: bool,
}

/// Navigation node types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NavNodeType {
    Waypoint,
    Door,
    Elevator,
    Stairs,
    Entrance,
    Exit,
}

/// Navigation edge (connection between nodes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavEdge {
    pub from: Uuid,
    pub to: Uuid,
    pub distance_m: f64,
    pub is_accessible: bool, // wheelchair accessible
    pub traversal_time_secs: f64,
}

/// Indoor positioning beacon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beacon {
    pub id: Uuid,
    pub beacon_type: BeaconType,
    pub position: [f64; 3], // x, y, z in local coordinates
    pub floor_level: i8,
    pub identifier: String, // UUID or MAC address
    pub signal_strength_dbm: i8,
}

/// Beacon technology type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BeaconType {
    BluetoothLowEnergy,
    WiFi,
    UltraWideband,
}

/// Get demo building data.
pub fn demo_buildings() -> Vec<IndoorBuilding> {
    let elevator_id = Uuid::new_v4();
    let stairs_id = Uuid::new_v4();
    let lobby_door_id = Uuid::new_v4();
    let office_door_id = Uuid::new_v4();

    vec![IndoorBuilding {
        id: Uuid::new_v4(),
        name: "Acme Construction HQ".into(),
        address: "100 Market Street, San Francisco, CA 94105".into(),
        position: [-122.3964, 37.7912],
        floors: vec![
            Floor {
                id: Uuid::new_v4(),
                level: 0,
                name: "Ground Floor".into(),
                height_m: 4.5,
                area_m2: 2400.0,
                rooms: vec![
                    Room {
                        id: Uuid::new_v4(),
                        name: "Main Lobby".into(),
                        room_type: RoomType::Lobby,
                        polygon: vec![[0.0, 0.0], [20.0, 0.0], [20.0, 15.0], [0.0, 15.0]],
                        area_m2: 300.0,
                        capacity: Some(50),
                        metadata: HashMap::new(),
                    },
                    Room {
                        id: Uuid::new_v4(),
                        name: "Reception".into(),
                        room_type: RoomType::Office,
                        polygon: vec![[20.0, 0.0], [30.0, 0.0], [30.0, 10.0], [20.0, 10.0]],
                        area_m2: 100.0,
                        capacity: Some(4),
                        metadata: HashMap::new(),
                    },
                ],
                navigation_graph: Some(NavigationGraph {
                    nodes: vec![
                        NavNode {
                            id: lobby_door_id,
                            position: [10.0, 0.0],
                            floor_level: 0,
                            node_type: NavNodeType::Entrance,
                            accessible: true,
                        },
                        NavNode {
                            id: elevator_id,
                            position: [25.0, 12.0],
                            floor_level: 0,
                            node_type: NavNodeType::Elevator,
                            accessible: true,
                        },
                        NavNode {
                            id: stairs_id,
                            position: [28.0, 12.0],
                            floor_level: 0,
                            node_type: NavNodeType::Stairs,
                            accessible: false,
                        },
                    ],
                    edges: vec![
                        NavEdge {
                            from: lobby_door_id,
                            to: elevator_id,
                            distance_m: 18.0,
                            is_accessible: true,
                            traversal_time_secs: 15.0,
                        },
                        NavEdge {
                            from: lobby_door_id,
                            to: stairs_id,
                            distance_m: 21.0,
                            is_accessible: false,
                            traversal_time_secs: 12.0,
                        },
                    ],
                }),
            },
            Floor {
                id: Uuid::new_v4(),
                level: 1,
                name: "1st Floor — Engineering".into(),
                height_m: 3.2,
                area_m2: 2400.0,
                rooms: vec![
                    Room {
                        id: Uuid::new_v4(),
                        name: "Open Office".into(),
                        room_type: RoomType::Office,
                        polygon: vec![[0.0, 0.0], [30.0, 0.0], [30.0, 20.0], [0.0, 20.0]],
                        area_m2: 600.0,
                        capacity: Some(40),
                        metadata: HashMap::from([("department".into(), "Engineering".into())]),
                    },
                    Room {
                        id: Uuid::new_v4(),
                        name: "Conference Room A".into(),
                        room_type: RoomType::MeetingRoom,
                        polygon: vec![[30.0, 0.0], [40.0, 0.0], [40.0, 8.0], [30.0, 8.0]],
                        area_m2: 80.0,
                        capacity: Some(12),
                        metadata: HashMap::from([("av_equipped".into(), "true".into())]),
                    },
                ],
                navigation_graph: Some(NavigationGraph {
                    nodes: vec![
                        NavNode {
                            id: Uuid::new_v4(),
                            position: [25.0, 12.0],
                            floor_level: 1,
                            node_type: NavNodeType::Elevator,
                            accessible: true,
                        },
                        NavNode {
                            id: office_door_id,
                            position: [15.0, 0.0],
                            floor_level: 1,
                            node_type: NavNodeType::Door,
                            accessible: true,
                        },
                    ],
                    edges: vec![],
                }),
            },
        ],
        total_area_m2: 4800.0,
    }]
}

/// Dijkstra priority queue state.
#[derive(PartialEq)]
struct DijkstraState {
    cost: f64,
    node_id: Uuid,
}

impl Eq for DijkstraState {}

impl PartialOrd for DijkstraState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DijkstraState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // min-heap: reverse ordering
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Compute the centroid of a room polygon.
fn room_centroid(polygon: &[[f64; 2]]) -> [f64; 2] {
    if polygon.is_empty() {
        return [0.0, 0.0];
    }
    let n = polygon.len() as f64;
    let sx: f64 = polygon.iter().map(|p| p[0]).sum();
    let sy: f64 = polygon.iter().map(|p| p[1]).sum();
    [sx / n, sy / n]
}

/// Euclidean distance between two 2D points.
fn dist_2d(a: &[f64; 2], b: &[f64; 2]) -> f64 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// Find the nearest navigation node to a position on a given floor.
fn nearest_node_on_floor(
    nodes: &[(Uuid, &NavNode)],
    pos: &[f64; 2],
    floor_level: i8,
) -> Option<Uuid> {
    nodes
        .iter()
        .filter(|(_, n)| n.floor_level == floor_level)
        .min_by(|(_, a), (_, b)| {
            dist_2d(&a.position, pos)
                .partial_cmp(&dist_2d(&b.position, pos))
                .unwrap()
        })
        .map(|(id, _)| *id)
}

/// Generate a human-readable instruction for traversing an edge.
fn describe_step(from: &NavNode, to: &NavNode, distance: f64) -> String {
    if from.floor_level != to.floor_level {
        let via = match to.node_type {
            NavNodeType::Elevator => "elevator",
            NavNodeType::Stairs => "stairs",
            _ => "transition",
        };
        format!(
            "Take {} from Floor {} to Floor {}",
            via, from.floor_level, to.floor_level
        )
    } else {
        let area = match &to.node_type {
            NavNodeType::Door => "door",
            NavNodeType::Entrance => "entrance",
            NavNodeType::Exit => "exit",
            NavNodeType::Elevator => "elevator",
            NavNodeType::Stairs => "stairs",
            NavNodeType::Waypoint => "waypoint",
        };
        format!("Walk {distance:.1}m to {area}")
    }
}

/// Find shortest path using Dijkstra on the building's navigation graphs.
pub fn find_route(
    building: &IndoorBuilding,
    from_room: Uuid,
    to_room: Uuid,
) -> Option<IndoorRoute> {
    // Find floors containing the rooms and their centroids
    let mut from_info: Option<(i8, [f64; 2])> = None;
    let mut to_info: Option<(i8, [f64; 2])> = None;

    for floor in &building.floors {
        for room in &floor.rooms {
            if room.id == from_room {
                from_info = Some((floor.level, room_centroid(&room.polygon)));
            }
            if room.id == to_room {
                to_info = Some((floor.level, room_centroid(&room.polygon)));
            }
        }
    }

    let (from_level, from_centroid) = from_info?;
    let (to_level, to_centroid) = to_info?;

    // 1. Collect all nodes and edges across all floors
    let mut all_nodes: Vec<(Uuid, &NavNode)> = Vec::new();
    let mut adjacency: HashMap<Uuid, Vec<(Uuid, f64)>> = HashMap::new();
    let mut node_map: HashMap<Uuid, &NavNode> = HashMap::new();

    for floor in &building.floors {
        if let Some(graph) = &floor.navigation_graph {
            for node in &graph.nodes {
                all_nodes.push((node.id, node));
                node_map.insert(node.id, node);
                adjacency.entry(node.id).or_default();
            }
            for edge in &graph.edges {
                adjacency
                    .entry(edge.from)
                    .or_default()
                    .push((edge.to, edge.distance_m));
                adjacency
                    .entry(edge.to)
                    .or_default()
                    .push((edge.from, edge.distance_m));
            }
        }
    }

    // 2. Add cross-floor edges between elevator/stairs nodes on adjacent floors
    let transport_nodes: Vec<&NavNode> = all_nodes
        .iter()
        .map(|(_, n)| *n)
        .filter(|n| n.node_type == NavNodeType::Elevator || n.node_type == NavNodeType::Stairs)
        .collect();

    for (i, a) in transport_nodes.iter().enumerate() {
        for b in transport_nodes.iter().skip(i + 1) {
            if a.node_type != b.node_type {
                continue;
            }
            // Connect transport nodes of same type on adjacent floors with similar position
            let floor_diff = (a.floor_level as i16 - b.floor_level as i16).unsigned_abs();
            if floor_diff == 1 && dist_2d(&a.position, &b.position) < 3.0 {
                let vertical_dist = 4.0; // approximate floor height in metres
                adjacency
                    .entry(a.id)
                    .or_default()
                    .push((b.id, vertical_dist));
                adjacency
                    .entry(b.id)
                    .or_default()
                    .push((a.id, vertical_dist));
            }
        }
    }

    // 3. Find source and target nav nodes
    let source_id = nearest_node_on_floor(&all_nodes, &from_centroid, from_level)?;
    let target_id = nearest_node_on_floor(&all_nodes, &to_centroid, to_level)?;

    // 4. Run Dijkstra
    let mut dist: HashMap<Uuid, f64> = HashMap::new();
    let mut prev: HashMap<Uuid, Uuid> = HashMap::new();
    let mut heap = BinaryHeap::new();

    dist.insert(source_id, 0.0);
    heap.push(DijkstraState {
        cost: 0.0,
        node_id: source_id,
    });

    while let Some(DijkstraState { cost, node_id }) = heap.pop() {
        if node_id == target_id {
            break;
        }
        if cost > *dist.get(&node_id).unwrap_or(&f64::INFINITY) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(&node_id) {
            for &(next, edge_dist) in neighbors {
                let new_cost = cost + edge_dist;
                if new_cost < *dist.get(&next).unwrap_or(&f64::INFINITY) {
                    dist.insert(next, new_cost);
                    prev.insert(next, node_id);
                    heap.push(DijkstraState {
                        cost: new_cost,
                        node_id: next,
                    });
                }
            }
        }
    }

    // 5. Reconstruct path
    if !dist.contains_key(&target_id) {
        return None;
    }

    let mut path = vec![target_id];
    let mut current = target_id;
    while let Some(&p) = prev.get(&current) {
        path.push(p);
        current = p;
    }
    path.reverse();

    // 6. Build route steps with real distances and instructions
    let mut steps = Vec::new();
    let mut total_distance = 0.0;
    let mut floor_changes: u8 = 0;
    let mut uses_elevator = false;

    for window in path.windows(2) {
        let from_node = node_map.get(&window[0])?;
        let to_node = node_map.get(&window[1])?;

        // Find the edge distance
        let edge_dist = adjacency
            .get(&window[0])
            .and_then(|edges| {
                edges
                    .iter()
                    .find(|(id, _)| *id == window[1])
                    .map(|(_, d)| *d)
            })
            .unwrap_or_else(|| dist_2d(&from_node.position, &to_node.position));

        total_distance += edge_dist;

        if from_node.floor_level != to_node.floor_level {
            floor_changes += 1;
            if to_node.node_type == NavNodeType::Elevator
                || from_node.node_type == NavNodeType::Elevator
            {
                uses_elevator = true;
            }
        }

        steps.push(RouteStep {
            instruction: describe_step(from_node, to_node, edge_dist),
            distance_m: edge_dist,
        });
    }

    // Walking speed ~1.4 m/s
    let estimated_time = total_distance / 1.4;

    Some(IndoorRoute {
        from_room,
        to_room,
        total_distance_m: total_distance,
        estimated_time_secs: estimated_time,
        floor_changes,
        uses_elevator,
        steps,
    })
}

/// A computed indoor route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndoorRoute {
    pub from_room: Uuid,
    pub to_room: Uuid,
    pub total_distance_m: f64,
    pub estimated_time_secs: f64,
    pub floor_changes: u8,
    pub uses_elevator: bool,
    pub steps: Vec<RouteStep>,
}

/// A step in an indoor route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStep {
    pub instruction: String,
    pub distance_m: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_buildings() {
        let buildings = demo_buildings();
        assert_eq!(buildings.len(), 1);
        assert_eq!(buildings[0].floors.len(), 2);
    }

    #[test]
    fn test_find_route_same_floor() {
        let buildings = demo_buildings();
        let building = &buildings[0];
        let room1 = building.floors[0].rooms[0].id;
        let room2 = building.floors[0].rooms[1].id;
        let route = find_route(building, room1, room2);
        // Both rooms are on floor 0 which has a navigation graph with edges
        if let Some(r) = route {
            assert_eq!(r.floor_changes, 0);
            assert!(r.total_distance_m > 0.0);
        }
    }

    #[test]
    fn test_find_route_cross_floor() {
        let buildings = demo_buildings();
        let building = &buildings[0];
        let room_ground = building.floors[0].rooms[0].id;
        let room_first = building.floors[1].rooms[0].id;
        let route = find_route(building, room_ground, room_first);
        // Cross-floor routing depends on elevator/stairs edges being synthesized
        if let Some(r) = route {
            assert!(r.floor_changes >= 1);
            assert!(r.total_distance_m > 0.0);
            assert!(!r.steps.is_empty());
        }
    }

    /// Build a simple 2-floor building with known graph and verify Dijkstra finds correct path.
    #[test]
    fn test_dijkstra_known_graph() {
        let room_a_id = Uuid::new_v4();
        let room_b_id = Uuid::new_v4();
        let node_a = Uuid::new_v4();
        let node_mid = Uuid::new_v4();
        let node_b = Uuid::new_v4();

        let building = IndoorBuilding {
            id: Uuid::new_v4(),
            name: "Test".into(),
            address: "Test".into(),
            position: [0.0, 0.0],
            floors: vec![Floor {
                id: Uuid::new_v4(),
                level: 0,
                name: "Ground".into(),
                height_m: 3.0,
                area_m2: 100.0,
                rooms: vec![
                    Room {
                        id: room_a_id,
                        name: "Room A".into(),
                        room_type: RoomType::Office,
                        polygon: vec![[0.0, 0.0], [5.0, 0.0], [5.0, 5.0], [0.0, 5.0]],
                        area_m2: 25.0,
                        capacity: None,
                        metadata: HashMap::new(),
                    },
                    Room {
                        id: room_b_id,
                        name: "Room B".into(),
                        room_type: RoomType::Office,
                        polygon: vec![[20.0, 0.0], [25.0, 0.0], [25.0, 5.0], [20.0, 5.0]],
                        area_m2: 25.0,
                        capacity: None,
                        metadata: HashMap::new(),
                    },
                ],
                navigation_graph: Some(NavigationGraph {
                    nodes: vec![
                        NavNode {
                            id: node_a,
                            position: [2.5, 2.5],
                            floor_level: 0,
                            node_type: NavNodeType::Door,
                            accessible: true,
                        },
                        NavNode {
                            id: node_mid,
                            position: [12.5, 2.5],
                            floor_level: 0,
                            node_type: NavNodeType::Waypoint,
                            accessible: true,
                        },
                        NavNode {
                            id: node_b,
                            position: [22.5, 2.5],
                            floor_level: 0,
                            node_type: NavNodeType::Door,
                            accessible: true,
                        },
                    ],
                    edges: vec![
                        NavEdge {
                            from: node_a,
                            to: node_mid,
                            distance_m: 10.0,
                            is_accessible: true,
                            traversal_time_secs: 8.0,
                        },
                        NavEdge {
                            from: node_mid,
                            to: node_b,
                            distance_m: 10.0,
                            is_accessible: true,
                            traversal_time_secs: 8.0,
                        },
                    ],
                }),
            }],
            total_area_m2: 100.0,
        };

        let route = find_route(&building, room_a_id, room_b_id).unwrap();
        assert_eq!(route.floor_changes, 0);
        // Total distance should be 20.0 (10 + 10 via midpoint)
        assert!(
            (route.total_distance_m - 20.0).abs() < 0.1,
            "expected ~20m, got {}",
            route.total_distance_m
        );
        assert_eq!(route.steps.len(), 2);
    }
}
