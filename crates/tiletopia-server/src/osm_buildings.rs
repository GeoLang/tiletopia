//! OSM Building extrusion and 3D Tiles generation.
//!
//! Parses OpenStreetMap building footprints and extrudes them into 3D geometry,
//! similar to Cesium Ion's OSM Buildings layer. Supports building:levels,
//! height, min_height, and roof shape tags.

use serde::{Deserialize, Serialize};

// ─── Data Types ──────────────────────────────────────────────────────────────

/// A 2D coordinate (longitude, latitude or projected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coord2D {
    pub x: f64,
    pub y: f64,
}

/// OSM building tags relevant to 3D extrusion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildingTags {
    pub building: String,        // "yes", "residential", "commercial", etc.
    pub height: Option<f64>,     // meters
    pub min_height: Option<f64>, // meters (for floating parts)
    pub building_levels: Option<u32>,
    pub building_min_level: Option<u32>,
    pub roof_shape: Option<RoofShape>,
    pub roof_height: Option<f64>,
    pub name: Option<String>,
    pub building_colour: Option<String>,
    pub roof_colour: Option<String>,
}

/// Supported roof shapes for extrusion.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum RoofShape {
    #[default]
    Flat,
    Gabled,
    Hipped,
    Pyramidal,
    Skillion,
    Dome,
}

/// An OSM building footprint with tags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsmBuilding {
    pub osm_id: u64,
    pub footprint: Vec<Coord2D>, // closed polygon (first == last)
    pub tags: BuildingTags,
}

/// Request to extrude buildings in a bounding box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtrudeBuildingsRequest {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub level_height_meters: Option<f64>,   // default: 3.0
    pub default_height_meters: Option<f64>, // fallback: 10.0
    pub include_roof_geometry: Option<bool>,
    pub output_format: Option<OutputFormat>,
}

/// Output format for extruded buildings.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum OutputFormat {
    #[serde(rename = "3dtiles")]
    #[default]
    Tiles3D,
    #[serde(rename = "glb")]
    Glb,
    #[serde(rename = "geojson")]
    GeoJson,
}

// ─── 3D Mesh Types ──────────────────────────────────────────────────────────

/// A 3D vertex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vertex3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// A triangle (indices into vertex array).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triangle {
    pub v0: u32,
    pub v1: u32,
    pub v2: u32,
}

/// Extruded building mesh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildingMesh {
    pub osm_id: u64,
    pub vertices: Vec<Vertex3D>,
    pub triangles: Vec<Triangle>,
    pub height: f64,
    pub min_height: f64,
    pub roof_shape: RoofShape,
    pub name: Option<String>,
    pub wall_color: [u8; 3],
    pub roof_color: [u8; 3],
}

/// Response for extruded buildings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtrudeBuildingsResponse {
    pub buildings: Vec<BuildingMesh>,
    pub total_vertices: u64,
    pub total_triangles: u64,
    pub bounding_box: BoundingBox3D,
}

/// 3D bounding box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox3D {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

// ─── Configuration ───────────────────────────────────────────────────────────

/// Default meters per building level.
const DEFAULT_LEVEL_HEIGHT: f64 = 3.0;
/// Default building height when no tags present.
const DEFAULT_BUILDING_HEIGHT: f64 = 10.0;
/// Default wall color (light gray).
const DEFAULT_WALL_COLOR: [u8; 3] = [200, 200, 200];
/// Default roof color (dark gray).
const DEFAULT_ROOF_COLOR: [u8; 3] = [140, 140, 140];

// ─── Core Logic ──────────────────────────────────────────────────────────────

/// Compute the effective building height from tags.
pub fn compute_building_height(tags: &BuildingTags, level_height: f64) -> f64 {
    if let Some(h) = tags.height {
        return h;
    }
    if let Some(levels) = tags.building_levels {
        return levels as f64 * level_height;
    }
    DEFAULT_BUILDING_HEIGHT
}

/// Compute the effective min_height (base elevation offset).
pub fn compute_min_height(tags: &BuildingTags, level_height: f64) -> f64 {
    if let Some(h) = tags.min_height {
        return h;
    }
    if let Some(min_level) = tags.building_min_level {
        return min_level as f64 * level_height;
    }
    0.0
}

/// Parse a CSS-style color string into RGB.
fn parse_color(color_str: &str, default: [u8; 3]) -> [u8; 3] {
    let s = color_str.trim().trim_start_matches('#');
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(default[0]);
        let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(default[1]);
        let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(default[2]);
        [r, g, b]
    } else {
        default
    }
}

/// Extrude a single building footprint into a 3D mesh.
///
/// Creates wall quads (2 triangles each) and a roof cap.
pub fn extrude_building(
    building: &OsmBuilding,
    level_height: f64,
    include_roof: bool,
) -> BuildingMesh {
    let height = compute_building_height(&building.tags, level_height);
    let min_height = compute_min_height(&building.tags, level_height);
    let roof_shape = building.tags.roof_shape.unwrap_or(RoofShape::Flat);
    let roof_height = building.tags.roof_height.unwrap_or(0.0);

    let wall_color = building
        .tags
        .building_colour
        .as_deref()
        .map(|c| parse_color(c, DEFAULT_WALL_COLOR))
        .unwrap_or(DEFAULT_WALL_COLOR);
    let roof_color = building
        .tags
        .roof_colour
        .as_deref()
        .map(|c| parse_color(c, DEFAULT_ROOF_COLOR))
        .unwrap_or(DEFAULT_ROOF_COLOR);

    let mut vertices = Vec::new();
    let mut triangles = Vec::new();

    let footprint = &building.footprint;
    let n = if footprint.len() > 1
        && footprint.first().map(|f| (f.x, f.y)) == footprint.last().map(|l| (l.x, l.y))
    {
        footprint.len() - 1 // skip closing vertex
    } else {
        footprint.len()
    };

    if n < 3 {
        return BuildingMesh {
            osm_id: building.osm_id,
            vertices,
            triangles,
            height,
            min_height,
            roof_shape,
            name: building.tags.name.clone(),
            wall_color,
            roof_color,
        };
    }

    // Generate wall geometry
    for i in 0..n {
        let j = (i + 1) % n;
        let base_idx = vertices.len() as u32;

        // Bottom-left, bottom-right, top-left, top-right
        vertices.push(Vertex3D {
            x: footprint[i].x,
            y: footprint[i].y,
            z: min_height,
        });
        vertices.push(Vertex3D {
            x: footprint[j].x,
            y: footprint[j].y,
            z: min_height,
        });
        vertices.push(Vertex3D {
            x: footprint[i].x,
            y: footprint[i].y,
            z: height,
        });
        vertices.push(Vertex3D {
            x: footprint[j].x,
            y: footprint[j].y,
            z: height,
        });

        // Two triangles per wall quad
        triangles.push(Triangle {
            v0: base_idx,
            v1: base_idx + 1,
            v2: base_idx + 2,
        });
        triangles.push(Triangle {
            v0: base_idx + 1,
            v1: base_idx + 3,
            v2: base_idx + 2,
        });
    }

    // Generate roof
    if include_roof {
        match roof_shape {
            RoofShape::Flat => {
                generate_flat_roof(&footprint[..n], height, &mut vertices, &mut triangles);
            }
            RoofShape::Gabled => {
                generate_gabled_roof(
                    &footprint[..n],
                    height,
                    roof_height.max(2.0),
                    &mut vertices,
                    &mut triangles,
                );
            }
            RoofShape::Hipped | RoofShape::Pyramidal => {
                generate_pyramidal_roof(
                    &footprint[..n],
                    height,
                    roof_height.max(2.0),
                    &mut vertices,
                    &mut triangles,
                );
            }
            _ => {
                generate_flat_roof(&footprint[..n], height, &mut vertices, &mut triangles);
            }
        }
    }

    // Generate bottom cap
    generate_flat_roof(&footprint[..n], min_height, &mut vertices, &mut triangles);

    BuildingMesh {
        osm_id: building.osm_id,
        vertices,
        triangles,
        height,
        min_height,
        roof_shape,
        name: building.tags.name.clone(),
        wall_color,
        roof_color,
    }
}

/// Generate a flat roof (fan triangulation from centroid).
fn generate_flat_roof(
    footprint: &[Coord2D],
    z: f64,
    vertices: &mut Vec<Vertex3D>,
    triangles: &mut Vec<Triangle>,
) {
    let n = footprint.len();
    if n < 3 {
        return;
    }

    // Compute centroid
    let cx: f64 = footprint.iter().map(|p| p.x).sum::<f64>() / n as f64;
    let cy: f64 = footprint.iter().map(|p| p.y).sum::<f64>() / n as f64;

    let center_idx = vertices.len() as u32;
    vertices.push(Vertex3D { x: cx, y: cy, z });

    let base = vertices.len() as u32;
    for p in footprint {
        vertices.push(Vertex3D { x: p.x, y: p.y, z });
    }

    for i in 0..n {
        let j = (i + 1) % n;
        triangles.push(Triangle {
            v0: center_idx,
            v1: base + i as u32,
            v2: base + j as u32,
        });
    }
}

/// Generate a gabled (ridged) roof.
fn generate_gabled_roof(
    footprint: &[Coord2D],
    wall_top: f64,
    roof_height: f64,
    vertices: &mut Vec<Vertex3D>,
    triangles: &mut Vec<Triangle>,
) {
    let n = footprint.len();
    if n < 4 {
        generate_flat_roof(footprint, wall_top + roof_height, vertices, triangles);
        return;
    }

    // Find the longest edge to determine ridge direction
    let mut max_len = 0.0f64;
    let mut ridge_start = 0;
    for i in 0..n {
        let j = (i + 1) % n;
        let dx = footprint[j].x - footprint[i].x;
        let dy = footprint[j].y - footprint[i].y;
        let len = (dx * dx + dy * dy).sqrt();
        if len > max_len {
            max_len = len;
            ridge_start = i;
        }
    }
    let ridge_end = (ridge_start + 1) % n;

    // Ridge midpoints
    let mid_a = Coord2D {
        x: (footprint[ridge_start].x + footprint[(ridge_start + n - 1) % n].x) / 2.0,
        y: (footprint[ridge_start].y + footprint[(ridge_start + n - 1) % n].y) / 2.0,
    };
    let mid_b = Coord2D {
        x: (footprint[ridge_end].x + footprint[(ridge_end + 1) % n].x) / 2.0,
        y: (footprint[ridge_end].y + footprint[(ridge_end + 1) % n].y) / 2.0,
    };

    let ridge_z = wall_top + roof_height;
    let base = vertices.len() as u32;

    // Add ridge vertices
    vertices.push(Vertex3D {
        x: mid_a.x,
        y: mid_a.y,
        z: ridge_z,
    });
    vertices.push(Vertex3D {
        x: mid_b.x,
        y: mid_b.y,
        z: ridge_z,
    });

    // Add eave vertices
    for p in footprint.iter().take(n) {
        vertices.push(Vertex3D {
            x: p.x,
            y: p.y,
            z: wall_top,
        });
    }

    // Create slope faces (simplified: triangles from each eave vertex to nearest ridge point)
    for i in 0..n {
        let j = (i + 1) % n;
        let eave_i = base + 2 + i as u32;
        let eave_j = base + 2 + j as u32;
        // Connect to closer ridge vertex
        let ridge_idx = if i <= n / 2 { base } else { base + 1 };
        triangles.push(Triangle {
            v0: ridge_idx,
            v1: eave_i,
            v2: eave_j,
        });
    }
}

/// Generate a pyramidal/hipped roof (all edges slope to center apex).
fn generate_pyramidal_roof(
    footprint: &[Coord2D],
    wall_top: f64,
    roof_height: f64,
    vertices: &mut Vec<Vertex3D>,
    triangles: &mut Vec<Triangle>,
) {
    let n = footprint.len();
    if n < 3 {
        return;
    }

    // Apex at centroid
    let cx: f64 = footprint.iter().map(|p| p.x).sum::<f64>() / n as f64;
    let cy: f64 = footprint.iter().map(|p| p.y).sum::<f64>() / n as f64;
    let apex_z = wall_top + roof_height;

    let apex_idx = vertices.len() as u32;
    vertices.push(Vertex3D {
        x: cx,
        y: cy,
        z: apex_z,
    });

    let base = vertices.len() as u32;
    for p in footprint.iter().take(n) {
        vertices.push(Vertex3D {
            x: p.x,
            y: p.y,
            z: wall_top,
        });
    }

    for i in 0..n {
        let j = (i + 1) % n;
        triangles.push(Triangle {
            v0: apex_idx,
            v1: base + i as u32,
            v2: base + j as u32,
        });
    }
}

/// Extrude all buildings and compute aggregate statistics.
pub fn extrude_buildings(
    buildings: &[OsmBuilding],
    request: &ExtrudeBuildingsRequest,
) -> ExtrudeBuildingsResponse {
    let level_height = request.level_height_meters.unwrap_or(DEFAULT_LEVEL_HEIGHT);
    let include_roof = request.include_roof_geometry.unwrap_or(true);

    let mut meshes = Vec::with_capacity(buildings.len());
    let mut total_vertices = 0u64;
    let mut total_triangles = 0u64;
    let mut min_bb = [f64::INFINITY; 3];
    let mut max_bb = [f64::NEG_INFINITY; 3];

    for building in buildings {
        let mesh = extrude_building(building, level_height, include_roof);
        for v in &mesh.vertices {
            min_bb[0] = min_bb[0].min(v.x);
            min_bb[1] = min_bb[1].min(v.y);
            min_bb[2] = min_bb[2].min(v.z);
            max_bb[0] = max_bb[0].max(v.x);
            max_bb[1] = max_bb[1].max(v.y);
            max_bb[2] = max_bb[2].max(v.z);
        }
        total_vertices += mesh.vertices.len() as u64;
        total_triangles += mesh.triangles.len() as u64;
        meshes.push(mesh);
    }

    if meshes.is_empty() {
        min_bb = [0.0; 3];
        max_bb = [0.0; 3];
    }

    ExtrudeBuildingsResponse {
        buildings: meshes,
        total_vertices,
        total_triangles,
        bounding_box: BoundingBox3D {
            min: min_bb,
            max: max_bb,
        },
    }
}

/// Parse OSM Overpass JSON response into building list.
pub fn parse_overpass_buildings(json_data: &serde_json::Value) -> Vec<OsmBuilding> {
    let mut buildings = Vec::new();

    let elements = match json_data.get("elements").and_then(|e| e.as_array()) {
        Some(elems) => elems,
        None => return buildings,
    };

    for element in elements {
        let element_type = element.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if element_type != "way" && element_type != "relation" {
            continue;
        }

        let osm_id = element.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
        let tags_obj = element.get("tags");

        let building_tag = tags_obj
            .and_then(|t| t.get("building"))
            .and_then(|b| b.as_str())
            .unwrap_or("");
        if building_tag.is_empty() {
            continue;
        }

        // Parse geometry
        let geometry = match element.get("geometry").and_then(|g| g.as_array()) {
            Some(g) => g,
            None => continue,
        };

        let footprint: Vec<Coord2D> = geometry
            .iter()
            .filter_map(|node| {
                let lon = node.get("lon").and_then(|v| v.as_f64())?;
                let lat = node.get("lat").and_then(|v| v.as_f64())?;
                Some(Coord2D { x: lon, y: lat })
            })
            .collect();

        if footprint.len() < 3 {
            continue;
        }

        let tags = BuildingTags {
            building: building_tag.to_string(),
            height: tags_obj
                .and_then(|t| t.get("height"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
            min_height: tags_obj
                .and_then(|t| t.get("min_height"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
            building_levels: tags_obj
                .and_then(|t| t.get("building:levels"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
            building_min_level: tags_obj
                .and_then(|t| t.get("building:min_level"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
            roof_shape: tags_obj
                .and_then(|t| t.get("roof:shape"))
                .and_then(|v| v.as_str())
                .and_then(parse_roof_shape),
            roof_height: tags_obj
                .and_then(|t| t.get("roof:height"))
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok()),
            name: tags_obj
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            building_colour: tags_obj
                .and_then(|t| t.get("building:colour"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            roof_colour: tags_obj
                .and_then(|t| t.get("roof:colour"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        buildings.push(OsmBuilding {
            osm_id,
            footprint,
            tags,
        });
    }

    buildings
}

fn parse_roof_shape(s: &str) -> Option<RoofShape> {
    match s.to_lowercase().as_str() {
        "flat" => Some(RoofShape::Flat),
        "gabled" => Some(RoofShape::Gabled),
        "hipped" => Some(RoofShape::Hipped),
        "pyramidal" => Some(RoofShape::Pyramidal),
        "skillion" => Some(RoofShape::Skillion),
        "dome" => Some(RoofShape::Dome),
        _ => None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_square_building() -> OsmBuilding {
        OsmBuilding {
            osm_id: 12345,
            footprint: vec![
                Coord2D { x: 0.0, y: 0.0 },
                Coord2D { x: 10.0, y: 0.0 },
                Coord2D { x: 10.0, y: 10.0 },
                Coord2D { x: 0.0, y: 10.0 },
                Coord2D { x: 0.0, y: 0.0 }, // closed
            ],
            tags: BuildingTags {
                building: "yes".to_string(),
                height: Some(15.0),
                min_height: None,
                building_levels: None,
                building_min_level: None,
                roof_shape: Some(RoofShape::Flat),
                roof_height: None,
                name: Some("Test Building".to_string()),
                building_colour: None,
                roof_colour: None,
            },
        }
    }

    #[test]
    fn test_compute_building_height_explicit() {
        let tags = BuildingTags {
            building: "yes".to_string(),
            height: Some(20.0),
            min_height: None,
            building_levels: Some(5),
            building_min_level: None,
            roof_shape: None,
            roof_height: None,
            name: None,
            building_colour: None,
            roof_colour: None,
        };
        // Explicit height takes precedence over levels
        assert_eq!(compute_building_height(&tags, 3.0), 20.0);
    }

    #[test]
    fn test_compute_building_height_from_levels() {
        let tags = BuildingTags {
            building: "residential".to_string(),
            height: None,
            min_height: None,
            building_levels: Some(4),
            building_min_level: None,
            roof_shape: None,
            roof_height: None,
            name: None,
            building_colour: None,
            roof_colour: None,
        };
        assert_eq!(compute_building_height(&tags, 3.0), 12.0);
    }

    #[test]
    fn test_compute_building_height_default() {
        let tags = BuildingTags {
            building: "yes".to_string(),
            height: None,
            min_height: None,
            building_levels: None,
            building_min_level: None,
            roof_shape: None,
            roof_height: None,
            name: None,
            building_colour: None,
            roof_colour: None,
        };
        assert_eq!(compute_building_height(&tags, 3.0), DEFAULT_BUILDING_HEIGHT);
    }

    #[test]
    fn test_compute_min_height() {
        let tags = BuildingTags {
            building: "yes".to_string(),
            height: None,
            min_height: Some(6.0),
            building_levels: None,
            building_min_level: None,
            roof_shape: None,
            roof_height: None,
            name: None,
            building_colour: None,
            roof_colour: None,
        };
        assert_eq!(compute_min_height(&tags, 3.0), 6.0);
    }

    #[test]
    fn test_extrude_building_flat_roof() {
        let building = simple_square_building();
        let mesh = extrude_building(&building, 3.0, true);

        assert_eq!(mesh.osm_id, 12345);
        assert_eq!(mesh.height, 15.0);
        assert_eq!(mesh.min_height, 0.0);
        assert_eq!(mesh.roof_shape, RoofShape::Flat);
        // 4 walls * 4 vertices = 16 wall vertices + roof + bottom
        assert!(!mesh.vertices.is_empty());
        assert!(!mesh.triangles.is_empty());
        // 4 walls * 2 triangles = 8 wall triangles + roof + bottom
        assert!(mesh.triangles.len() >= 8);
    }

    #[test]
    fn test_extrude_building_gabled_roof() {
        let mut building = simple_square_building();
        building.tags.roof_shape = Some(RoofShape::Gabled);
        building.tags.roof_height = Some(4.0);

        let mesh = extrude_building(&building, 3.0, true);
        assert!(!mesh.vertices.is_empty());
        assert!(!mesh.triangles.is_empty());
    }

    #[test]
    fn test_extrude_building_pyramidal_roof() {
        let mut building = simple_square_building();
        building.tags.roof_shape = Some(RoofShape::Pyramidal);
        building.tags.roof_height = Some(5.0);

        let mesh = extrude_building(&building, 3.0, true);
        assert!(!mesh.vertices.is_empty());
    }

    #[test]
    fn test_extrude_buildings_batch() {
        let buildings = vec![simple_square_building()];
        let request = ExtrudeBuildingsRequest {
            min_lon: -1.0,
            min_lat: -1.0,
            max_lon: 11.0,
            max_lat: 11.0,
            level_height_meters: None,
            default_height_meters: None,
            include_roof_geometry: Some(true),
            output_format: None,
        };

        let response = extrude_buildings(&buildings, &request);
        assert_eq!(response.buildings.len(), 1);
        assert!(response.total_vertices > 0);
        assert!(response.total_triangles > 0);
    }

    #[test]
    fn test_extrude_empty_footprint() {
        let building = OsmBuilding {
            osm_id: 1,
            footprint: vec![Coord2D { x: 0.0, y: 0.0 }, Coord2D { x: 1.0, y: 0.0 }], // only 2 points
            tags: BuildingTags {
                building: "yes".to_string(),
                height: None,
                min_height: None,
                building_levels: None,
                building_min_level: None,
                roof_shape: None,
                roof_height: None,
                name: None,
                building_colour: None,
                roof_colour: None,
            },
        };
        let mesh = extrude_building(&building, 3.0, true);
        assert!(mesh.vertices.is_empty());
    }

    #[test]
    fn test_parse_overpass_buildings() {
        let json = serde_json::json!({
            "elements": [
                {
                    "type": "way",
                    "id": 999,
                    "tags": {
                        "building": "commercial",
                        "height": "25",
                        "building:levels": "8",
                        "name": "Office Tower",
                        "roof:shape": "flat"
                    },
                    "geometry": [
                        {"lon": 1.0, "lat": 2.0},
                        {"lon": 1.001, "lat": 2.0},
                        {"lon": 1.001, "lat": 2.001},
                        {"lon": 1.0, "lat": 2.001},
                        {"lon": 1.0, "lat": 2.0}
                    ]
                }
            ]
        });

        let buildings = parse_overpass_buildings(&json);
        assert_eq!(buildings.len(), 1);
        assert_eq!(buildings[0].osm_id, 999);
        assert_eq!(buildings[0].tags.building, "commercial");
        assert_eq!(buildings[0].tags.height, Some(25.0));
        assert_eq!(buildings[0].tags.building_levels, Some(8));
        assert_eq!(buildings[0].tags.name, Some("Office Tower".to_string()));
        assert_eq!(buildings[0].tags.roof_shape, Some(RoofShape::Flat));
    }

    #[test]
    fn test_parse_overpass_empty() {
        let json = serde_json::json!({"elements": []});
        assert!(parse_overpass_buildings(&json).is_empty());
    }

    #[test]
    fn test_parse_color() {
        assert_eq!(parse_color("#ff8800", [0, 0, 0]), [255, 136, 0]);
        assert_eq!(parse_color("aabbcc", [0, 0, 0]), [170, 187, 204]);
        assert_eq!(parse_color("invalid", [1, 2, 3]), [1, 2, 3]);
    }

    #[test]
    fn test_roof_shape_default() {
        assert_eq!(RoofShape::default(), RoofShape::Flat);
    }

    #[test]
    fn test_output_format_default() {
        assert_eq!(OutputFormat::default(), OutputFormat::Tiles3D);
    }

    #[test]
    fn test_building_with_color() {
        let mut building = simple_square_building();
        building.tags.building_colour = Some("#ff0000".to_string());
        building.tags.roof_colour = Some("#00ff00".to_string());

        let mesh = extrude_building(&building, 3.0, true);
        assert_eq!(mesh.wall_color, [255, 0, 0]);
        assert_eq!(mesh.roof_color, [0, 255, 0]);
    }

    #[test]
    fn test_bounding_box_computed() {
        let buildings = vec![simple_square_building()];
        let request = ExtrudeBuildingsRequest {
            min_lon: 0.0,
            min_lat: 0.0,
            max_lon: 10.0,
            max_lat: 10.0,
            level_height_meters: None,
            default_height_meters: None,
            include_roof_geometry: Some(true),
            output_format: None,
        };
        let response = extrude_buildings(&buildings, &request);
        assert!(response.bounding_box.max[2] >= 15.0); // height=15
    }
}
