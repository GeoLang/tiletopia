use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::get,
};
use serde::{Deserialize, Serialize};
use tiletopia_ingest::MeshData;

use crate::dxf_export::{mesh_source, read_meshes_in_tileset_frame};
use crate::elements::{element_error, invalid_parameter, permitted_asset, server_failure};
use crate::ion_compat::IonError;
use crate::{AppState, may_view_asset};

pub fn section_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/assets/{id}/section", get(get_section))
}

#[derive(Debug, Deserialize)]
struct SectionParameters {
    axis: Option<String>,
    position: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    fn index(self) -> usize {
        match self {
            Axis::X => 0,
            Axis::Y => 1,
            Axis::Z => 2,
        }
    }

    pub(crate) fn plane_axes(self) -> [usize; 2] {
        let index = self.index();
        [(index + 1) % 3, (index + 2) % 3]
    }
}

pub(crate) fn parsed_axis(raw: Option<&str>) -> Result<Axis, IonError> {
    match raw {
        Some("x") => Ok(Axis::X),
        Some("y") => Ok(Axis::Y),
        Some("z") => Ok(Axis::Z),
        Some(other) => Err(invalid_parameter(format!(
            "axis {other:?} is not x, y or z"
        ))),
        None => Err(invalid_parameter("axis is required, one of x, y or z")),
    }
}

pub(crate) fn parsed_position(raw: Option<&str>) -> Result<f64, IonError> {
    let raw = raw.ok_or_else(|| invalid_parameter("position is required, in metres"))?;
    raw.parse::<f64>()
        .ok()
        .filter(|position| position.is_finite())
        .ok_or_else(|| invalid_parameter(format!("position {raw:?} is not a finite number")))
}

pub(crate) fn not_ifc_section(id: &str, extension: &str) -> IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "ASSET_HAS_NO_MESH_SOURCE",
        format!(
            "asset {id}'s source is a .{extension}, and only an IFC has element ids to section"
        ),
    )
}

pub(crate) type Ring = Vec<[f64; 3]>;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ElementPolygon {
    pub(crate) element_id: String,
    pub(crate) rings: Vec<Ring>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Section {
    pub(crate) axis: Axis,
    position: f64,
    pub(crate) open_chains_dropped: u64,
    pub(crate) polygons: Vec<ElementPolygon>,
}

fn crossing(first: [f64; 3], second: [f64; 3], axis: usize, position: f64) -> [f64; 3] {
    let (low, high) = if first[axis] < second[axis] {
        (first, second)
    } else {
        (second, first)
    };
    let along = (position - low[axis]) / (high[axis] - low[axis]);
    let mut point: [f64; 3] = std::array::from_fn(|coordinate| {
        low[coordinate] + along * (high[coordinate] - low[coordinate])
    });
    point[axis] = position;
    point
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Below,
    On,
    Above,
}

fn side_of_plane(coordinate: f64, position: f64) -> Side {
    if (coordinate - position).abs() <= WELD_DISTANCE_METRES {
        Side::On
    } else if coordinate < position {
        Side::Below
    } else {
        Side::Above
    }
}

fn on_plane(point: [f64; 3], axis: usize, position: f64) -> [f64; 3] {
    let mut snapped = point;
    snapped[axis] = position;
    snapped
}

struct PlaneCut {
    crossings: Vec<[[f64; 3]; 2]>,
    edges_in_plane: Vec<[[f64; 3]; 2]>,
}

fn with_solid_on_the_left(
    ends: [[f64; 3]; 2],
    corners: &[[f64; 3]; 3],
    axis: usize,
) -> [[f64; 3]; 2] {
    let [along, across] = [(axis + 1) % 3, (axis + 2) % 3];
    let first_edge: [f64; 3] =
        std::array::from_fn(|component| corners[1][component] - corners[0][component]);
    let second_edge: [f64; 3] =
        std::array::from_fn(|component| corners[2][component] - corners[0][component]);
    let normal_along =
        first_edge[across] * second_edge[axis] - first_edge[axis] * second_edge[across];
    let normal_across =
        first_edge[axis] * second_edge[along] - first_edge[along] * second_edge[axis];
    let direction = [
        ends[1][along] - ends[0][along],
        ends[1][across] - ends[0][across],
    ];
    // an outward face normal to the right of the segment, seen from the cut axis, puts the solid on its left
    let solid_on_the_left = normal_along * direction[1] - normal_across * direction[0];
    if solid_on_the_left < 0.0 {
        [ends[1], ends[0]]
    } else {
        ends
    }
}

fn plane_cut(mesh: &MeshData, axis: usize, position: f64) -> PlaneCut {
    let mut cut = PlaneCut {
        crossings: Vec::new(),
        edges_in_plane: Vec::new(),
    };
    for triangle in mesh.indices.as_chunks::<3>().0 {
        let corners: [[f64; 3]; 3] =
            std::array::from_fn(|corner| mesh.positions[triangle[corner] as usize]);
        let all_finite = corners
            .iter()
            .flatten()
            .all(|coordinate| coordinate.is_finite());
        if !all_finite {
            continue;
        }
        let sides = corners.map(|corner| side_of_plane(corner[axis], position));
        let corners_on_plane = sides.iter().filter(|side| **side == Side::On).count();
        if corners_on_plane == 3 {
            continue;
        }
        if corners_on_plane == 2 {
            let off = sides
                .iter()
                .position(|side| *side != Side::On)
                .expect("one corner is off the plane");
            cut.edges_in_plane.push(with_solid_on_the_left(
                [
                    on_plane(corners[(off + 1) % 3], axis, position),
                    on_plane(corners[(off + 2) % 3], axis, position),
                ],
                &corners,
                axis,
            ));
            continue;
        }
        let mut ends = [[0.0; 3]; 2];
        let mut end_count = 0;
        for index in 0..3 {
            let next = (index + 1) % 3;
            if sides[index] == Side::On {
                ends[end_count] = on_plane(corners[index], axis, position);
                end_count += 1;
            } else if sides[next] != Side::On && sides[index] != sides[next] {
                ends[end_count] = crossing(corners[index], corners[next], axis, position);
                end_count += 1;
            }
        }
        if end_count == 2 {
            cut.crossings
                .push(with_solid_on_the_left(ends, &corners, axis));
        }
    }
    cut
}

// ifc-lite stores positions as f32, whose spacing stays under 0.1 mm within 1 km of a mesh's origin
const WELD_DISTANCE_METRES: f64 = 1e-4;

#[derive(Default)]
struct WeldedPoints {
    points: Vec<[f64; 3]>,
    by_cell: HashMap<(i64, i64), Vec<usize>>,
}

impl WeldedPoints {
    fn index_of(&mut self, point: [f64; 3], plane_axes: [usize; 2]) -> usize {
        let cell_of = |axis: usize| (point[axis] / WELD_DISTANCE_METRES).floor() as i64;
        let cell = (cell_of(plane_axes[0]), cell_of(plane_axes[1]));
        let is_near = |index: &usize| {
            let welded = self.points[*index];
            let squared: f64 = plane_axes
                .iter()
                .map(|&axis| (welded[axis] - point[axis]).powi(2))
                .sum();
            squared <= WELD_DISTANCE_METRES * WELD_DISTANCE_METRES
        };
        let near = (-1..=1)
            .flat_map(|across| (-1..=1).map(move |up| (cell.0 + across, cell.1 + up)))
            .filter_map(|neighbour| self.by_cell.get(&neighbour))
            .flatten()
            .copied()
            .find(|index| is_near(index));
        if let Some(index) = near {
            return index;
        }
        let index = self.points.len();
        self.points.push(point);
        self.by_cell.entry(cell).or_default().push(index);
        index
    }
}

struct Chains {
    closed: Vec<Vec<usize>>,
    open: u64,
}

fn unused_segment_at(point: usize, touching: &[Vec<usize>], used: &[bool]) -> Option<usize> {
    touching[point]
        .iter()
        .copied()
        .find(|&segment| !used[segment])
}

fn other_end(segment: [usize; 2], point: usize) -> usize {
    if segment[0] == point {
        segment[1]
    } else {
        segment[0]
    }
}

fn chained(segments: &[[usize; 2]], point_count: usize) -> Chains {
    let mut touching = vec![Vec::new(); point_count];
    for (index, segment) in segments.iter().enumerate() {
        touching[segment[0]].push(index);
        touching[segment[1]].push(index);
    }
    let mut used = vec![false; segments.len()];
    let mut chains = Chains {
        closed: Vec::new(),
        open: 0,
    };
    for (first, &[start, first_end]) in segments.iter().enumerate() {
        if used[first] {
            continue;
        }
        used[first] = true;
        let mut end = first_end;
        let mut ring = vec![start, end];
        while end != start {
            let Some(segment) = unused_segment_at(end, &touching, &used) else {
                break;
            };
            used[segment] = true;
            end = other_end(segments[segment], end);
            ring.push(end);
        }
        if end == start {
            ring.pop();
            chains.closed.push(ring);
            continue;
        }
        let mut back = start;
        while let Some(segment) = unused_segment_at(back, &touching, &used) {
            used[segment] = true;
            back = other_end(segments[segment], back);
        }
        chains.open += 1;
    }
    chains
}

fn planar(point: [f64; 3], plane_axes: [usize; 2]) -> [f64; 2] {
    [point[plane_axes[0]], point[plane_axes[1]]]
}

fn signed_area(ring: &[[f64; 2]]) -> f64 {
    let doubled: f64 = ring
        .iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(current, next)| current[0] * next[1] - next[0] * current[1])
        .sum();
    doubled / 2.0
}

fn contains(ring: &[[f64; 2]], point: [f64; 2]) -> bool {
    let crossings = ring
        .iter()
        .zip(ring.iter().cycle().skip(1))
        .filter(|(current, next)| {
            let straddles = (current[1] > point[1]) != (next[1] > point[1]);
            straddles
                && point[0]
                    < current[0]
                        + (point[1] - current[1]) * (next[0] - current[0]) / (next[1] - current[1])
        })
        .count();
    crossings % 2 == 1
}

struct CutRing {
    points: Ring,
    is_hole: bool,
}

fn polygons_by_orientation(rings: Vec<CutRing>, plane_axes: [usize; 2]) -> Vec<Vec<Ring>> {
    let flat: Vec<Vec<[f64; 2]>> = rings
        .iter()
        .map(|ring| {
            ring.points
                .iter()
                .map(|&point| planar(point, plane_axes))
                .collect()
        })
        .collect();
    let areas: Vec<f64> = flat.iter().map(|ring| signed_area(ring).abs()).collect();
    let hole_flags: Vec<bool> = rings.iter().map(|ring| ring.is_hole).collect();

    let mut polygon_of_ring: Vec<Option<usize>> = vec![None; rings.len()];
    let mut polygons: Vec<Vec<Ring>> = Vec::new();
    let mut holes = Vec::new();
    for (index, ring) in rings.into_iter().enumerate() {
        if ring.is_hole {
            holes.push((index, ring.points));
        } else {
            polygon_of_ring[index] = Some(polygons.len());
            polygons.push(vec![ring.points]);
        }
    }
    for (index, hole) in holes {
        let smallest_container = (0..flat.len())
            .filter(|&other| {
                !hole_flags[other]
                    && areas[other] > areas[index]
                    && contains(&flat[other], flat[index][0])
            })
            .min_by(|left, right| areas[*left].total_cmp(&areas[*right]));
        match smallest_container {
            Some(outer) => {
                let polygon = polygon_of_ring[outer].expect("every outer ring starts a polygon");
                polygons[polygon].push(hole);
            }
            None => polygons.push(vec![hole]),
        }
    }
    polygons
}

fn runs_against_its_faces(
    ring: &[usize],
    flat: &[[f64; 2]],
    votes: &HashMap<(usize, usize), i64>,
) -> bool {
    let vote = |from: usize, to: usize| votes.get(&(from, to)).copied().unwrap_or(0);
    let agreement: i64 = ring
        .iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(&from, &to)| vote(from, to) - vote(to, from))
        .sum();
    signed_area(flat) * (agreement as f64) < 0.0
}

pub(crate) fn section_of(meshes: &[MeshData], axis: Axis, position: f64) -> Section {
    let plane_axes = axis.plane_axes();
    let mut section = Section {
        axis,
        position,
        open_chains_dropped: 0,
        polygons: Vec::new(),
    };
    let mut meshes_without_id = 0usize;
    // rings enclosing less than one weld cell are a plane touching an edge or a corner
    let smallest_ring_area = WELD_DISTANCE_METRES * WELD_DISTANCE_METRES;
    for mesh in meshes {
        let Some(element_id) = &mesh.asset_id else {
            meshes_without_id += 1;
            continue;
        };
        let cut = plane_cut(mesh, axis.index(), position);
        let mut welded = WeldedPoints::default();
        let mut votes: HashMap<(usize, usize), i64> = HashMap::new();
        let mut segments: Vec<[usize; 2]> = Vec::new();
        for ends in cut.crossings {
            let [first, second] = ends.map(|end| welded.index_of(end, plane_axes));
            *votes.entry((first, second)).or_default() += 1;
            segments.push([first, second]);
        }
        // the triangles above and below an edge lying in the plane both give that edge
        let mut edges_in_plane = HashSet::new();
        for ends in cut.edges_in_plane {
            let [first, second] = ends.map(|end| welded.index_of(end, plane_axes));
            *votes.entry((first, second)).or_default() += 1;
            if edges_in_plane.insert((first.min(second), first.max(second))) {
                segments.push([first, second]);
            }
        }
        segments.retain(|ends| ends[0] != ends[1]);
        let chains = chained(&segments, welded.points.len());
        section.open_chains_dropped += chains.open;
        let rings: Vec<CutRing> = chains
            .closed
            .into_iter()
            .filter_map(|ring| {
                let flat: Vec<[f64; 2]> = ring
                    .iter()
                    .map(|&point| planar(welded.points[point], plane_axes))
                    .collect();
                if ring.len() < 3 || signed_area(&flat).abs() < smallest_ring_area {
                    return None;
                }
                Some(CutRing {
                    is_hole: runs_against_its_faces(&ring, &flat, &votes),
                    points: ring.iter().map(|&point| welded.points[point]).collect(),
                })
            })
            .collect();
        for polygon_rings in polygons_by_orientation(rings, plane_axes) {
            section.polygons.push(ElementPolygon {
                element_id: element_id.clone(),
                rings: polygon_rings,
            });
        }
    }
    if meshes_without_id > 0 {
        tracing::warn!(
            "{meshes_without_id} meshes carry no GlobalId and are left out of the section"
        );
    }
    section
}

async fn get_section(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<SectionParameters>,
) -> Result<Json<Section>, IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let axis = parsed_axis(parameters.axis.as_deref())?;
    let position = parsed_position(parameters.position.as_deref())?;
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(not_ifc_section(&id, &source.extension));
    }

    // the mesh readers hold the whole model in memory, the same load an upload's preparation budgets for
    let build_slot = state.elevation_store.build_slot().await;
    let section = tokio::task::spawn_blocking(move || {
        let _build_slot = build_slot;
        let meshes = read_meshes_in_tileset_frame(&source.job, &source.extension)?;
        Ok::<_, String>(section_of(&meshes, axis, position))
    })
    .await
    .map_err(|error| server_failure("the section could not be computed", error))?
    .map_err(|reason| server_failure("the section could not be computed", reason))?;
    Ok(Json(section))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_mesh(minimum: [f64; 3], maximum: [f64; 3]) -> MeshData {
        let corner = |index: usize| -> [f64; 3] {
            std::array::from_fn(|axis| {
                if (index >> axis) & 1 == 1 {
                    maximum[axis]
                } else {
                    minimum[axis]
                }
            })
        };
        let faces: [[usize; 4]; 6] = [
            [0, 2, 3, 1],
            [4, 5, 7, 6],
            [0, 1, 5, 4],
            [2, 6, 7, 3],
            [0, 4, 6, 2],
            [1, 3, 7, 5],
        ];
        let indices = faces
            .iter()
            .flat_map(|face| [face[0], face[1], face[2], face[0], face[2], face[3]])
            .map(|index| index as u32)
            .collect();
        MeshData {
            positions: (0..8).map(corner).collect(),
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices,
            name: "box".to_string(),
            material: None,
            asset_id: Some("0box000000000000000000".to_string()),
            class: None,
            storey: None,
            element_color: None,
        }
    }

    fn single_ring_corners(section: &Section) -> Ring {
        assert_eq!(section.open_chains_dropped, 0);
        assert_eq!(section.polygons.len(), 1);
        let rings = &section.polygons[0].rings;
        assert_eq!(rings.len(), 1);
        let mut corners = rings[0].clone();
        corners.sort_by(|left, right| left.partial_cmp(right).unwrap());
        corners
    }

    #[test]
    fn a_cut_level_with_the_top_or_the_bottom_face_is_that_faces_outline() {
        for height in [0.0, 2.5] {
            let mesh = box_mesh([0.0, 0.0, 0.0], [4.0, 0.3, 2.5]);

            let section = section_of(&[mesh], Axis::Z, height);

            assert_eq!(
                single_ring_corners(&section),
                vec![
                    [0.0, 0.0, height],
                    [0.0, 0.3, height],
                    [4.0, 0.0, height],
                    [4.0, 0.3, height]
                ],
                "a cut at {height}"
            );
        }
    }

    #[test]
    fn an_edge_in_the_plane_shared_by_triangles_above_and_below_is_cut_once() {
        let mut stacked = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let upper = box_mesh([0.0, 0.0, 1.0], [1.0, 1.0, 2.0]);
        let offset = stacked.positions.len() as u32;
        stacked.positions.extend(upper.positions);
        stacked
            .indices
            .extend(upper.indices.iter().map(|index| index + offset));

        let section = section_of(&[stacked], Axis::Z, 1.0);

        assert_eq!(
            single_ring_corners(&section),
            vec![
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 1.0],
                [1.0, 0.0, 1.0],
                [1.0, 1.0, 1.0]
            ]
        );
    }

    #[test]
    fn an_open_surface_counts_as_a_dropped_chain() {
        let mut mesh = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        mesh.indices.truncate(mesh.indices.len() - 6);

        let section = section_of(&[mesh], Axis::Z, 0.5);

        assert_eq!(section.open_chains_dropped, 1);
        assert!(section.polygons.is_empty());
    }

    #[test]
    fn a_hole_joins_the_smallest_outer_ring_around_it_and_a_solid_inside_a_hole_is_a_new_polygon() {
        let square = |half: f64| -> Ring {
            vec![
                [-half, -half, 0.0],
                [half, -half, 0.0],
                [half, half, 0.0],
                [-half, half, 0.0],
            ]
        };
        let ring = |half: f64, is_hole: bool| CutRing {
            points: square(half),
            is_hole,
        };

        let polygons = polygons_by_orientation(
            vec![ring(1.0, false), ring(3.0, false), ring(2.0, true)],
            Axis::Z.plane_axes(),
        );

        assert_eq!(
            polygons,
            vec![vec![square(1.0)], vec![square(3.0), square(2.0)]]
        );
    }

    fn joined(mut first: MeshData, second: MeshData) -> MeshData {
        let offset = first.positions.len() as u32;
        first.positions.extend(second.positions);
        first
            .indices
            .extend(second.indices.iter().map(|index| index + offset));
        first
    }

    fn net_area(section: &Section) -> f64 {
        let plane_axes = Axis::Z.plane_axes();
        section
            .polygons
            .iter()
            .flat_map(|polygon| polygon.rings.iter().enumerate())
            .map(|(index, ring)| {
                let flat: Vec<[f64; 2]> = ring
                    .iter()
                    .map(|&point| planar(point, plane_axes))
                    .collect();
                let area = signed_area(&flat).abs();
                if index == 0 { area } else { -area }
            })
            .sum()
    }

    #[test]
    fn a_solid_inside_another_solids_outline_is_its_own_polygon_and_not_a_hole() {
        let glass = box_mesh([0.0, 0.0, 0.0], [4.0, 1.0, 2.0]);
        let bead = box_mesh([1.0, 0.25, 0.0], [2.0, 0.5, 2.0]);

        let section = section_of(&[joined(glass, bead)], Axis::Z, 1.0);

        assert_eq!(section.polygons.len(), 2);
        assert!(
            section
                .polygons
                .iter()
                .all(|polygon| polygon.rings.len() == 1)
        );
        assert!((net_area(&section) - 4.25).abs() < 1e-9);
    }

    #[test]
    fn an_inward_facing_box_inside_a_box_is_a_hole() {
        let outer = box_mesh([0.0, 0.0, 0.0], [4.0, 4.0, 2.0]);
        let mut cavity = box_mesh([1.0, 1.0, 0.5], [3.0, 3.0, 1.5]);
        for triangle in cavity.indices.chunks_mut(3) {
            triangle.swap(1, 2);
        }

        let section = section_of(&[joined(outer, cavity)], Axis::Z, 1.0);

        assert_eq!(section.polygons.len(), 1);
        assert_eq!(section.polygons[0].rings.len(), 2);
        assert!((net_area(&section) - 12.0).abs() < 1e-9);
    }
}
