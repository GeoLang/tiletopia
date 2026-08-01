//! Demo data routes — exposes premium features via real computations.
//!
//! Seeds sample data on startup and serves it through API endpoints
//! so the frontend can display real results.

use axum::{Router, extract::State, response::Json, routing::get};
use serde::Serialize;
use std::sync::Arc;

use crate::AppState;
use crate::audit::{AuditAction, AuditLog, AuditQuery};
use crate::stories::{CameraPosition, Slide, Story, StorySettings, StoryStore, TransitionType};

/// Demo state held alongside AppState.
pub struct DemoState {
    pub audit_log: AuditLog,
    pub story_store: StoryStore,
}

impl Default for DemoState {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoState {
    /// Create and seed demo state with sample data.
    pub fn new() -> Self {
        let audit_log = AuditLog::new(1000);
        let mut story_store = StoryStore::new();

        // Seed audit entries
        audit_log.log_action(
            "admin@company.com",
            AuditAction::Upload,
            "asset",
            "site_scan_v3",
            "Uploaded site_scan_v3.las (2.4 GB)",
            true,
        );
        audit_log.log_action(
            "sarah@company.com",
            AuditAction::Read,
            "tileset",
            "building-A",
            "Viewed tileset/building-A",
            true,
        );
        audit_log.log_action(
            "admin@company.com",
            AuditAction::PermissionChange,
            "user",
            "sarah@company.com",
            "Granted Editor role to sarah@company.com",
            true,
        );
        audit_log.log_action(
            "client@external.io",
            AuditAction::Delete,
            "tileset",
            "old-scan",
            "Attempted to delete tileset/old — DENIED",
            false,
        );
        audit_log.log_action(
            "client@external.io",
            AuditAction::Read,
            "tileset",
            "building-A",
            "Viewed tileset/building-A",
            true,
        );
        audit_log.log_action(
            "admin@company.com",
            AuditAction::Export,
            "audit",
            "log",
            "Exported audit_log_2026-05.json",
            true,
        );
        audit_log.log_action(
            "sarah@company.com",
            AuditAction::Login,
            "session",
            "session-847",
            "Login via OIDC (auth0)",
            true,
        );

        // Seed a story
        let story = Story {
            id: "demo-story-1".into(),
            title: "Site Progress Report Q2 2026".into(),
            description: "Quarterly construction progress update".into(),
            author_id: "admin@company.com".into(),
            slides: vec![
                Slide {
                    id: "slide-1".into(),
                    title: Some("Overview".into()),
                    camera: CameraPosition {
                        longitude: -74.006,
                        latitude: 40.7128,
                        height: 500.0,
                        heading: 0.0,
                        pitch: -45.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(5.0),
                    narration: None,
                    overlays: vec![],
                    visible_layers: vec!["terrain".into(), "buildings".into()],
                    time_of_day: Some(10.0),
                },
                Slide {
                    id: "slide-2".into(),
                    title: Some("East Wing Foundation".into()),
                    camera: CameraPosition {
                        longitude: -74.0055,
                        latitude: 40.7125,
                        height: 120.0,
                        heading: 45.0,
                        pitch: -30.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(4.0),
                    narration: None,
                    overlays: vec![],
                    visible_layers: vec!["terrain".into(), "point-cloud".into()],
                    time_of_day: Some(10.0),
                },
                Slide {
                    id: "slide-3".into(),
                    title: Some("West Wing Steel Frame".into()),
                    camera: CameraPosition {
                        longitude: -74.0065,
                        latitude: 40.713,
                        height: 145.0,
                        heading: 320.0,
                        pitch: -15.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(4.0),
                    narration: None,
                    overlays: vec![],
                    visible_layers: vec!["terrain".into(), "bim-model".into()],
                    time_of_day: Some(14.0),
                },
                Slide {
                    id: "slide-4".into(),
                    title: Some("Cut/Fill Analysis".into()),
                    camera: CameraPosition {
                        longitude: -74.0058,
                        latitude: 40.7132,
                        height: 200.0,
                        heading: 180.0,
                        pitch: -60.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(5.0),
                    narration: None,
                    overlays: vec![],
                    visible_layers: vec!["terrain".into(), "heatmap".into()],
                    time_of_day: Some(16.0),
                },
            ],
            settings: StorySettings {
                auto_play: true,
                loop_playback: false,
                default_slide_duration_secs: 5.0,
                transition_type: TransitionType::Fly,
                background_audio_url: None,
            },
            published: true,
            created_at: "2026-05-01T10:00:00Z".into(),
            updated_at: "2026-05-08T14:30:00Z".into(),
        };
        story_store.create(story);

        Self {
            audit_log,
            story_store,
        }
    }
}

/// Register demo API routes.
pub fn demo_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/demo/measurement", get(measurement_handler))
        .route("/api/v1/demo/anomaly", get(anomaly_handler))
        .route("/api/v1/demo/clash", get(clash_handler))
        .route("/api/v1/demo/audit", get(audit_handler))
        .route("/api/v1/demo/rbac", get(rbac_handler))
        .route("/api/v1/demo/stories", get(stories_handler))
}

// ─── Measurement ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MeasurementResult {
    distance_m: f64,
    polyline_length_m: f64,
    area_m2: f64,
    volume_m3: f64,
    cut_volume_m3: f64,
    fill_volume_m3: f64,
    slope_percent: f64,
    bearing_degrees: f64,
}

async fn measurement_handler() -> Json<MeasurementResult> {
    use tiletopia_core::measurement::*;

    let p1 = MeasurePoint::new(0.0, 0.0, 0.0);
    let p2 = MeasurePoint::new(100.0, 80.0, 12.0);

    let polyline = vec![
        MeasurePoint::new(0.0, 0.0, 0.0),
        MeasurePoint::new(50.0, 0.0, 3.0),
        MeasurePoint::new(50.0, 60.0, 6.0),
        MeasurePoint::new(100.0, 80.0, 12.0),
    ];

    let polygon = vec![
        MeasurePoint::new(0.0, 0.0, 0.0),
        MeasurePoint::new(60.0, 0.0, 2.0),
        MeasurePoint::new(60.0, 45.0, 4.0),
        MeasurePoint::new(0.0, 45.0, 1.0),
    ];

    let mesh = Surface {
        vertices: vec![
            MeasurePoint::new(0.0, 0.0, 0.0),
            MeasurePoint::new(10.0, 0.0, 0.0),
            MeasurePoint::new(10.0, 10.0, 0.0),
            MeasurePoint::new(0.0, 10.0, 0.0),
            MeasurePoint::new(0.0, 0.0, 5.0),
            MeasurePoint::new(10.0, 0.0, 5.0),
            MeasurePoint::new(10.0, 10.0, 5.0),
            MeasurePoint::new(0.0, 10.0, 5.0),
        ],
        triangles: vec![
            [0, 1, 2],
            [0, 2, 3],
            [4, 6, 5],
            [4, 7, 6],
            [0, 4, 5],
            [0, 5, 1],
            [2, 6, 7],
            [2, 7, 3],
            [0, 3, 7],
            [0, 7, 4],
            [1, 5, 6],
            [1, 6, 2],
        ],
    };

    // Cut/fill: 3x3 grid, cell_size 5m
    let reference_heights = vec![5.2, 4.8, 5.5, 5.0, 6.1, 5.3, 4.9, 5.4, 5.1];
    let design_heights = vec![4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0, 4.0];

    let dist = distance_3d(&p1, &p2);
    let length = polyline_length(&polyline);
    let area = polygon_area_3d(&polygon);
    let volume = mesh_volume(&mesh);
    let cf = cut_fill_volume(&reference_heights, &design_heights, 3, 3, 5.0);
    let slope = slope_percent(&p1, &p2);
    let bearing_rad = bearing(&p1, &p2);

    Json(MeasurementResult {
        distance_m: (dist * 100.0).round() / 100.0,
        polyline_length_m: (length * 100.0).round() / 100.0,
        area_m2: (area * 100.0).round() / 100.0,
        volume_m3: (volume.abs() * 100.0).round() / 100.0,
        cut_volume_m3: (cf.cut_volume * 100.0).round() / 100.0,
        fill_volume_m3: (cf.fill_volume * 100.0).round() / 100.0,
        slope_percent: (slope * 100.0).round() / 100.0,
        bearing_degrees: ((bearing_rad.to_degrees().rem_euclid(360.0)) * 100.0).round() / 100.0,
    })
}

// ─── Anomaly Detection ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnomalyResult {
    deformation_alerts: Vec<DeformationAlert>,
    encroachment_alerts: Vec<EncroachmentAlert>,
    outlier_stats: OutlierStats,
}

#[derive(Serialize)]
struct DeformationAlert {
    grid_cell: [usize; 2],
    delta_m: f64,
    severity: String,
}

#[derive(Serialize)]
struct EncroachmentAlert {
    zone_name: String,
    points_in_buffer: usize,
    min_distance_m: f64,
}

#[derive(Serialize)]
struct OutlierStats {
    total_points: usize,
    removed: usize,
    percent_removed: f64,
    z_threshold: f64,
}

async fn anomaly_handler() -> Json<AnomalyResult> {
    use tiletopia_core::anomaly::*;

    let mut epoch1 = Vec::new();
    let mut epoch2 = Vec::new();

    // 10x10 grid, 1m spacing — center deforms by 0.25m
    for i in 0..10 {
        for j in 0..10 {
            let x = i as f64;
            let y = j as f64;
            epoch1.push([x, y, 0.0]);
            let z = if (3..7).contains(&i) && (3..7).contains(&j) {
                0.25 // 25cm deformation in center
            } else {
                0.0
            };
            epoch2.push([x, y, z]);
        }
    }

    let config = AnomalyConfig {
        deformation_threshold: 0.1,
        ..Default::default()
    };

    let deformations = detect_deformation(&epoch1, &epoch2, &config);

    let boundary = vec![
        [5.0, 5.0, 0.0],
        [6.0, 5.0, 0.0],
        [6.0, 6.0, 0.0],
        [5.0, 6.0, 0.0],
    ];
    let encroach_config = AnomalyConfig {
        encroachment_distance: 2.0,
        ..Default::default()
    };
    let encroachments = detect_encroachment(&boundary, &epoch2, &encroach_config);

    // Statistical outlier removal on survey data
    // Dense 20x20 grid at 0.5m spacing with 5 extreme outliers at z=±100m
    let mut outlier_cloud: Vec<[f64; 3]> = Vec::new();
    for i in 0..20 {
        for j in 0..20 {
            let z = 10.0 + 0.5 * ((i as f64 * 0.7).sin() + (j as f64 * 0.5).cos());
            outlier_cloud.push([i as f64 * 0.5, j as f64 * 0.5, z]);
        }
    }
    // Inject extreme outliers (z=100+ when grid is z≈10)
    outlier_cloud.push([5.0, 5.0, 110.0]);
    outlier_cloud.push([3.0, 3.0, -80.0]);
    outlier_cloud.push([7.0, 7.0, 120.0]);
    outlier_cloud.push([2.0, 8.0, -95.0]);
    outlier_cloud.push([8.0, 2.0, 105.0]);
    let total_pts = outlier_cloud.len();
    let inlier_indices = statistical_outlier_removal(&outlier_cloud, 6, 2.0);
    let removed = total_pts - inlier_indices.len();

    Json(AnomalyResult {
        deformation_alerts: deformations
            .iter()
            .map(|d| DeformationAlert {
                grid_cell: [
                    (d.location[0] / 5.0) as usize,
                    (d.location[1] / 5.0) as usize,
                ],
                delta_m: (d.severity * 0.5 * 1000.0).round() / 1000.0,
                severity: if d.severity > 0.6 {
                    "HIGH".into()
                } else {
                    "MEDIUM".into()
                },
            })
            .collect(),
        encroachment_alerts: vec![EncroachmentAlert {
            zone_name: "Protected Wetland".into(),
            points_in_buffer: encroachments.len(),
            min_distance_m: 2.1,
        }],
        outlier_stats: OutlierStats {
            total_points: total_pts,
            removed,
            percent_removed: (removed as f64 / total_pts as f64) * 100.0,
            z_threshold: 2.0,
        },
    })
}

// ─── Clash Detection ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ClashResult {
    clashes: Vec<ClashItem>,
    total_elements: usize,
    hard_count: usize,
    soft_count: usize,
}

#[derive(Serialize)]
struct ClashItem {
    clash_type: String,
    element_a: String,
    element_b: String,
    detail: String,
    severity: String,
}

async fn clash_handler() -> Json<ClashResult> {
    use tiletopia_core::clash_detection::*;

    let elements = vec![
        BimElement {
            id: "pipe-MEP-204".into(),
            element_type: BimElementType::Pipe,
            bbox_min: [5.0, 5.0, 2.0],
            bbox_max: [5.3, 5.3, 8.0],
            vertices: vec![[5.15, 5.15, 2.0], [5.15, 5.15, 8.0]],
        },
        BimElement {
            id: "beam-STR-112".into(),
            element_type: BimElementType::Beam,
            bbox_min: [4.0, 5.0, 4.8],
            bbox_max: [7.0, 5.4, 5.2],
            vertices: vec![[4.0, 5.2, 5.0], [7.0, 5.2, 5.0]],
        },
        BimElement {
            id: "duct-HVAC-89".into(),
            element_type: BimElementType::Duct,
            bbox_min: [10.0, 10.0, 3.0],
            bbox_max: [10.6, 10.6, 9.0],
            vertices: vec![[10.3, 10.3, 3.0], [10.3, 10.3, 9.0]],
        },
        BimElement {
            id: "column-STR-45".into(),
            element_type: BimElementType::Column,
            bbox_min: [10.0, 10.0, 0.0],
            bbox_max: [10.5, 10.5, 12.0],
            vertices: vec![[10.25, 10.25, 0.0], [10.25, 10.25, 12.0]],
        },
        BimElement {
            id: "wall-ARC-67".into(),
            element_type: BimElementType::Wall,
            bbox_min: [20.0, 0.0, 0.0],
            bbox_max: [20.3, 15.0, 3.0],
            vertices: vec![[20.15, 0.0, 1.5], [20.15, 15.0, 1.5]],
        },
        // Close-proximity elements for soft clashes
        BimElement {
            id: "pipe-MEP-301".into(),
            element_type: BimElementType::Pipe,
            bbox_min: [20.4, 5.0, 1.0],
            bbox_max: [20.6, 5.3, 2.5],
            vertices: vec![[20.5, 5.15, 1.0], [20.5, 5.15, 2.5]],
        },
        BimElement {
            id: "duct-HVAC-102".into(),
            element_type: BimElementType::Duct,
            bbox_min: [5.4, 5.0, 5.3],
            bbox_max: [5.7, 5.3, 8.0],
            vertices: vec![[5.55, 5.15, 5.3], [5.55, 5.15, 8.0]],
        },
    ];

    let config = ClashConfig {
        hard_clash_tolerance: 0.0,
        soft_clash_clearance: 0.15,
        ..Default::default()
    };

    let clashes = detect_element_clashes(&elements, &config);

    let hard_count = clashes
        .iter()
        .filter(|c| matches!(c.clash_type, ClashType::HardClash))
        .count();
    let soft_count = clashes
        .iter()
        .filter(|c| matches!(c.clash_type, ClashType::SoftClash))
        .count();

    Json(ClashResult {
        clashes: clashes
            .iter()
            .map(|c| ClashItem {
                clash_type: match c.clash_type {
                    ClashType::HardClash => "HARD".into(),
                    ClashType::SoftClash => "SOFT".into(),
                    _ => "OTHER".into(),
                },
                element_a: c.element_a.clone(),
                element_b: c.element_b.clone().unwrap_or_default(),
                detail: format!(
                    "{}: {:.3}m",
                    match c.clash_type {
                        ClashType::HardClash => "Overlap",
                        ClashType::SoftClash => "Clearance",
                        _ => "Other",
                    },
                    c.distance
                ),
                severity: match c.clash_type {
                    ClashType::HardClash => "Critical".into(),
                    ClashType::SoftClash => "Warning".into(),
                    _ => "Info".into(),
                },
            })
            .collect(),
        total_elements: elements.len(),
        hard_count,
        soft_count,
    })
}

// ─── Audit Log ───────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct AuditParams {
    user_id: Option<String>,
    action: Option<String>,
    resource_type: Option<String>,
    limit: Option<usize>,
}

async fn audit_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<AuditParams>,
) -> Json<Vec<crate::audit::AuditEntry>> {
    let action = params.action.and_then(|a| match a.as_str() {
        "Create" => Some(AuditAction::Create),
        "Read" => Some(AuditAction::Read),
        "Update" => Some(AuditAction::Update),
        "Delete" => Some(AuditAction::Delete),
        "Upload" => Some(AuditAction::Upload),
        "Download" => Some(AuditAction::Download),
        "Login" => Some(AuditAction::Login),
        "Logout" => Some(AuditAction::Logout),
        "PermissionChange" => Some(AuditAction::PermissionChange),
        "Export" => Some(AuditAction::Export),
        _ => None,
    });
    let entries = state.demo.audit_log.query(&AuditQuery {
        user_id: params.user_id,
        action,
        resource_type: params.resource_type,
        limit: Some(params.limit.unwrap_or(20)),
        ..Default::default()
    });
    Json(entries)
}

// ─── RBAC ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RbacInfo {
    users: Vec<UserInfo>,
    provider: String,
}

#[derive(Serialize)]
struct UserInfo {
    email: String,
    role: String,
}

// canned sample data for the GUI's admin panel, not the server's own authz.
// the real role gates are users::require_admin / require_editor.
async fn rbac_handler() -> Json<RbacInfo> {
    Json(RbacInfo {
        users: vec![
            UserInfo {
                email: "admin@company.com".into(),
                role: "Admin".into(),
            },
            UserInfo {
                email: "sarah@company.com".into(),
                role: "Editor".into(),
            },
            UserInfo {
                email: "client@external.io".into(),
                role: "Viewer".into(),
            },
        ],
        provider: "auth0.com/company".into(),
    })
}

// ─── Stories ─────────────────────────────────────────────────────────────────

async fn stories_handler(State(state): State<Arc<AppState>>) -> Json<Vec<Story>> {
    let stories = state.demo.story_store.list_published();
    Json(stories.into_iter().cloned().collect())
}
