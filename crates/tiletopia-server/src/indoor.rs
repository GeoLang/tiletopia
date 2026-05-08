//! Indoor mapping — floor plans, room-level navigation, and indoor positioning.
//!
//! Supports:
//! - Multi-story building floor plans
//! - Room/zone definitions with metadata
//! - Indoor routing between rooms
//! - Indoor positioning reference points (BLE beacons, WiFi)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// Calculate route between two rooms (simplified).
pub fn find_route(
    building: &IndoorBuilding,
    from_room: Uuid,
    to_room: Uuid,
) -> Option<IndoorRoute> {
    // Find floors containing the rooms
    let mut from_floor: Option<i8> = None;
    let mut to_floor: Option<i8> = None;

    for floor in &building.floors {
        for room in &floor.rooms {
            if room.id == from_room {
                from_floor = Some(floor.level);
            }
            if room.id == to_room {
                to_floor = Some(floor.level);
            }
        }
    }

    let from_level = from_floor?;
    let to_level = to_floor?;

    Some(IndoorRoute {
        from_room,
        to_room,
        total_distance_m: 45.0 + (from_level - to_level).unsigned_abs() as f64 * 4.0,
        estimated_time_secs: 60.0 + (from_level - to_level).unsigned_abs() as f64 * 15.0,
        floor_changes: (from_level - to_level).unsigned_abs(),
        uses_elevator: from_level != to_level,
        steps: vec![
            RouteStep {
                instruction: "Exit room".into(),
                distance_m: 5.0,
            },
            RouteStep {
                instruction: "Walk to elevator".into(),
                distance_m: 20.0,
            },
            RouteStep {
                instruction: "Enter destination room".into(),
                distance_m: 10.0,
            },
        ],
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
        let room1 = building.floors[1].rooms[0].id;
        let room2 = building.floors[1].rooms[1].id;
        let route = find_route(building, room1, room2).unwrap();
        assert_eq!(route.floor_changes, 0);
    }

    #[test]
    fn test_find_route_cross_floor() {
        let buildings = demo_buildings();
        let building = &buildings[0];
        let room_ground = building.floors[0].rooms[0].id;
        let room_first = building.floors[1].rooms[0].id;
        let route = find_route(building, room_ground, room_first).unwrap();
        assert_eq!(route.floor_changes, 1);
        assert!(route.uses_elevator);
    }
}
