use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, Query, State},
    http::HeaderMap,
    response::Json,
    routing::get,
};
use serde::{Deserialize, Serialize};
use tiletopia_ingest::MeshData;

use crate::dxf_export::{mesh_source, read_meshes_in_tileset_frame};
use crate::elements::{invalid_parameter, permitted_asset, server_failure};
use crate::section::{Axis, not_ifc_section, parsed_axis, parsed_position, section_of};
use crate::{AppState, may_view_asset};

const SHARP_DOT: f64 = 0.93969262078; // cos(20 degrees)
const OCCLUSION_METRES: f64 = 0.01;
const GRID: usize = 128;

pub fn drawing_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/assets/{id}/drawing", get(get_drawing))
}

#[derive(Debug, Deserialize)]
struct DrawingParameters {
    axis: Option<String>,
    position: Option<String>,
    flip: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DrawingLine {
    pub(crate) element_id: String,
    pub(crate) hidden: bool,
    pub(crate) start: [f64; 3],
    pub(crate) end: [f64; 3],
    pub(crate) paper_start: [f64; 2],
    pub(crate) paper_end: [f64; 2],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Drawing {
    pub(crate) axis: Axis,
    pub(crate) lines: Vec<DrawingLine>,
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.iter().zip(right).map(|(one, other)| one * other).sum()
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn sub(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|axis| left[axis] - right[axis])
}

fn paper(axis: Axis, point: [f64; 3]) -> [f64; 2] {
    let [along, across] = axis.plane_axes();
    [point[along], point[across]]
}

fn kept(point: [f64; 3], axis: usize, position: f64, flip: bool) -> bool {
    if flip {
        point[axis] >= position
    } else {
        point[axis] <= position
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct EdgeKey {
    low: [i64; 3],
    high: [i64; 3],
}

fn quantized(point: [f64; 3]) -> [i64; 3] {
    std::array::from_fn(|axis| (point[axis] * 1.0e4).round() as i64)
}

fn edge_key(first: [f64; 3], second: [f64; 3]) -> EdgeKey {
    let left = quantized(first);
    let right = quantized(second);
    if left <= right {
        EdgeKey {
            low: left,
            high: right,
        }
    } else {
        EdgeKey {
            low: right,
            high: left,
        }
    }
}

struct EdgeRecord {
    start: [f64; 3],
    end: [f64; 3],
    element_id: String,
    normals: Vec<[f64; 3]>,
}

fn unit(vector: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot(vector, vector).sqrt();
    (length > 1.0e-12).then(|| std::array::from_fn(|axis| vector[axis] / length))
}

fn view_direction(axis: usize) -> [f64; 3] {
    let mut direction = [0.0; 3];
    direction[axis] = -1.0;
    direction
}

fn facing_camera(normal: [f64; 3], view: [f64; 3]) -> bool {
    dot(normal, view) < 0.0
}

fn worth_drawing(normals: &[[f64; 3]], view: [f64; 3]) -> bool {
    if normals.len() < 2 {
        return true;
    }
    let sharp = dot(normals[0], normals[1]) < SHARP_DOT;
    let silhouette = facing_camera(normals[0], view) != facing_camera(normals[1], view);
    sharp || silhouette
}

struct DepthGrid {
    origin: [f64; 2],
    cell: f64,
    depth: Vec<f64>,
}

fn raster_depth(
    mesh: &MeshData,
    axis_index: usize,
    view: [f64; 3],
    bounds: ([f64; 2], [f64; 2]),
) -> DepthGrid {
    let span = [
        (bounds.1[0] - bounds.0[0]).max(1.0e-6),
        (bounds.1[1] - bounds.0[1]).max(1.0e-6),
    ];
    let cell = span[0].max(span[1]) / GRID as f64;
    let mut depth = vec![f64::NEG_INFINITY; GRID * GRID];
    let axis = match axis_index {
        0 => Axis::X,
        1 => Axis::Y,
        _ => Axis::Z,
    };
    for triangle in mesh.indices.as_chunks::<3>().0 {
        let corners: [[f64; 3]; 3] =
            std::array::from_fn(|corner| mesh.positions[triangle[corner] as usize]);
        if corners
            .iter()
            .flatten()
            .any(|coordinate| !coordinate.is_finite())
        {
            continue;
        }
        let normal = match unit(cross(
            sub(corners[1], corners[0]),
            sub(corners[2], corners[0]),
        )) {
            Some(normal) => normal,
            None => continue,
        };
        if !facing_camera(normal, view) {
            continue;
        }
        let flat = corners.map(|corner| paper(axis, corner));
        let min = [
            flat.iter()
                .map(|point| point[0])
                .fold(f64::INFINITY, f64::min),
            flat.iter()
                .map(|point| point[1])
                .fold(f64::INFINITY, f64::min),
        ];
        let max = [
            flat.iter()
                .map(|point| point[0])
                .fold(f64::NEG_INFINITY, f64::max),
            flat.iter()
                .map(|point| point[1])
                .fold(f64::NEG_INFINITY, f64::max),
        ];
        let cell_of =
            |coordinate: f64, origin: f64| ((coordinate - origin) / cell).floor() as isize;
        let start = [
            cell_of(min[0], bounds.0[0]).clamp(0, GRID as isize - 1),
            cell_of(min[1], bounds.0[1]).clamp(0, GRID as isize - 1),
        ];
        let end = [
            cell_of(max[0], bounds.0[0]).clamp(0, GRID as isize - 1),
            cell_of(max[1], bounds.0[1]).clamp(0, GRID as isize - 1),
        ];
        for row in start[1]..=end[1] {
            for column in start[0]..=end[0] {
                let sample = [
                    bounds.0[0] + (column as f64 + 0.5) * cell,
                    bounds.0[1] + (row as f64 + 0.5) * cell,
                ];
                let Some(weight) = barycentric(flat, sample) else {
                    continue;
                };
                let at = weight[0] * corners[0][axis_index]
                    + weight[1] * corners[1][axis_index]
                    + weight[2] * corners[2][axis_index];
                let slot = row as usize * GRID + column as usize;
                if at > depth[slot] {
                    depth[slot] = at;
                }
            }
        }
    }
    DepthGrid {
        origin: bounds.0,
        cell,
        depth,
    }
}

fn barycentric(corners: [[f64; 2]; 3], point: [f64; 2]) -> Option<[f64; 3]> {
    let area = (corners[1][0] - corners[0][0]) * (corners[2][1] - corners[0][1])
        - (corners[2][0] - corners[0][0]) * (corners[1][1] - corners[0][1]);
    if area.abs() < 1.0e-12 {
        return None;
    }
    let weight_b = ((point[0] - corners[0][0]) * (corners[2][1] - corners[0][1])
        - (corners[2][0] - corners[0][0]) * (point[1] - corners[0][1]))
        / area;
    let weight_c = ((corners[1][0] - corners[0][0]) * (point[1] - corners[0][1])
        - (point[0] - corners[0][0]) * (corners[1][1] - corners[0][1]))
        / area;
    let weight_a = 1.0 - weight_b - weight_c;
    (weight_a >= -1.0e-6 && weight_b >= -1.0e-6 && weight_c >= -1.0e-6)
        .then_some([weight_a, weight_b, weight_c])
}

fn depth_at(grid: &DepthGrid, point: [f64; 2]) -> Option<f64> {
    let column = ((point[0] - grid.origin[0]) / grid.cell).floor() as isize;
    let row = ((point[1] - grid.origin[1]) / grid.cell).floor() as isize;
    if column < 0 || row < 0 || column >= GRID as isize || row >= GRID as isize {
        return None;
    }
    let depth = grid.depth[row as usize * GRID + column as usize];
    depth.is_finite().then_some(depth)
}

pub(crate) fn project_drawing(
    meshes: &[MeshData],
    axis: Axis,
    position: Option<f64>,
    flip: bool,
) -> Drawing {
    let axis_index = axis.index();
    let view = view_direction(axis_index);
    let mut edges: HashMap<EdgeKey, EdgeRecord> = HashMap::new();
    let mut paper_bounds: Option<([f64; 2], [f64; 2])> = None;
    let include = |point: [f64; 3]| match position {
        Some(position) => kept(point, axis_index, position, flip),
        None => true,
    };
    for mesh in meshes {
        let Some(element_id) = &mesh.asset_id else {
            continue;
        };
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let corners: [[f64; 3]; 3] =
                std::array::from_fn(|corner| mesh.positions[triangle[corner] as usize]);
            if corners
                .iter()
                .flatten()
                .any(|coordinate| !coordinate.is_finite())
            {
                continue;
            }
            if corners.iter().all(|corner| !include(*corner)) {
                continue;
            }
            let Some(normal) = unit(cross(
                sub(corners[1], corners[0]),
                sub(corners[2], corners[0]),
            )) else {
                continue;
            };
            for index in 0..3 {
                let start = corners[index];
                let end = corners[(index + 1) % 3];
                let record = edges
                    .entry(edge_key(start, end))
                    .or_insert_with(|| EdgeRecord {
                        start,
                        end,
                        element_id: element_id.clone(),
                        normals: Vec::new(),
                    });
                if record.normals.len() < 2 {
                    record.normals.push(normal);
                }
                for point in [paper(axis, start), paper(axis, end)] {
                    paper_bounds = Some(match paper_bounds {
                        None => (point, point),
                        Some((min, max)) => (
                            [min[0].min(point[0]), min[1].min(point[1])],
                            [max[0].max(point[0]), max[1].max(point[1])],
                        ),
                    });
                }
            }
        }
    }
    let grid = paper_bounds.map(|bounds| {
        let mut grid = DepthGrid {
            origin: bounds.0,
            cell: 1.0,
            depth: Vec::new(),
        };
        for mesh in meshes {
            let raster = raster_depth(mesh, axis_index, view, bounds);
            if grid.depth.is_empty() {
                grid = raster;
                continue;
            }
            for (slot, depth) in raster.depth.iter().enumerate() {
                if *depth > grid.depth[slot] {
                    grid.depth[slot] = *depth;
                }
            }
        }
        grid
    });
    let mut lines = Vec::new();
    for edge in edges.into_values() {
        if !worth_drawing(&edge.normals, view) {
            continue;
        }
        let (start, end) = clip_segment(edge.start, edge.end, axis_index, position, flip);
        let Some((start, end)) = start.zip(end) else {
            continue;
        };
        if quantized(start) == quantized(end) {
            continue;
        }
        let midpoint =
            std::array::from_fn(|coordinate| (start[coordinate] + end[coordinate]) * 0.5);
        let hidden = grid.as_ref().is_some_and(|grid| {
            depth_at(grid, paper(axis, midpoint))
                .is_some_and(|near| near > midpoint[axis_index] + OCCLUSION_METRES)
        });
        lines.push(DrawingLine {
            element_id: edge.element_id,
            hidden,
            start,
            end,
            paper_start: paper(axis, start),
            paper_end: paper(axis, end),
        });
    }
    if let Some(position) = position {
        for polygon in section_of(meshes, axis, position).polygons {
            for ring in polygon.rings {
                for pair in ring.iter().zip(ring.iter().cycle().skip(1)) {
                    lines.push(DrawingLine {
                        element_id: polygon.element_id.clone(),
                        hidden: false,
                        start: *pair.0,
                        end: *pair.1,
                        paper_start: paper(axis, *pair.0),
                        paper_end: paper(axis, *pair.1),
                    });
                }
            }
        }
    }
    Drawing { axis, lines }
}

fn clip_segment(
    start: [f64; 3],
    end: [f64; 3],
    axis: usize,
    position: Option<f64>,
    flip: bool,
) -> (Option<[f64; 3]>, Option<[f64; 3]>) {
    let Some(position) = position else {
        return (Some(start), Some(end));
    };
    let start_kept = kept(start, axis, position, flip);
    let end_kept = kept(end, axis, position, flip);
    if start_kept && end_kept {
        return (Some(start), Some(end));
    }
    if !start_kept && !end_kept {
        return (None, None);
    }
    let along = (position - start[axis]) / (end[axis] - start[axis]);
    let crossing = std::array::from_fn(|coordinate| {
        start[coordinate] + along * (end[coordinate] - start[coordinate])
    });
    if start_kept {
        (Some(start), Some(crossing))
    } else {
        (Some(crossing), Some(end))
    }
}

async fn get_drawing(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<DrawingParameters>,
) -> Result<Json<Drawing>, crate::ion_compat::IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let axis = parsed_axis(parameters.axis.as_deref())?;
    let position = match parameters.position.as_deref() {
        None => None,
        Some(_) => Some(parsed_position(parameters.position.as_deref())?),
    };
    let flip = match parameters.flip.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(invalid_parameter(format!(
                "flip {other:?} is not true or false"
            )));
        }
    };
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(not_ifc_section(&id, &source.extension));
    }
    let build_slot = state.elevation_store.build_slot().await;
    let drawing = tokio::task::spawn_blocking(move || {
        let _build_slot = build_slot;
        let meshes = read_meshes_in_tileset_frame(&source.job, &source.extension)?;
        Ok::<_, String>(project_drawing(&meshes, axis, position, flip))
    })
    .await
    .map_err(|error| server_failure("the drawing could not be projected", error))?
    .map_err(|reason| server_failure("the drawing could not be projected", reason))?;
    Ok(Json(drawing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_mesh(minimum: [f64; 3], maximum: [f64; 3], id: &str) -> MeshData {
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
        MeshData {
            positions: (0..8).map(corner).collect(),
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: faces
                .iter()
                .flat_map(|face| [face[0], face[1], face[2], face[0], face[2], face[3]])
                .map(|index| index as u32)
                .collect(),
            name: id.to_string(),
            material: None,
            asset_id: Some(id.to_string()),
            class: None,
            storey: None,
            element_color: None,
        }
    }

    #[test]
    fn an_edge_behind_another_box_is_hidden_and_the_near_edge_is_visible() {
        let far = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], "far");
        let near = box_mesh([0.0, 0.0, 2.0], [1.0, 1.0, 3.0], "near");
        let drawing = project_drawing(&[far, near], Axis::Z, None, false);
        let hidden = drawing
            .lines
            .iter()
            .filter(|line| line.element_id == "far" && line.hidden)
            .count();
        let visible = drawing
            .lines
            .iter()
            .filter(|line| line.element_id == "near" && !line.hidden)
            .count();
        assert!(hidden > 0, "the far box has no hidden line");
        assert!(visible > 0, "the near box has no visible line");
    }

    fn named_triangle(positions: [[f64; 3]; 3], id: &str) -> MeshData {
        MeshData {
            positions: positions.to_vec(),
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: id.to_string(),
            material: None,
            asset_id: Some(id.to_string()),
            class: None,
            storey: None,
            element_color: None,
        }
    }

    fn joins(line: &DrawingLine, start: [f64; 3], end: [f64; 3]) -> bool {
        let ends = [quantized(line.start), quantized(line.end)];
        ends.contains(&quantized(start)) && ends.contains(&quantized(end))
    }

    #[test]
    fn a_coplanar_diagonal_is_not_drawn_and_a_mesh_with_no_id_is_skipped() {
        let slab = MeshData {
            positions: vec![
                [0.0, 0.0, 0.0],
                [2.0, 0.0, 0.0],
                [2.0, 2.0, 0.0],
                [0.0, 2.0, 0.0],
            ],
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3],
            name: "slab".to_string(),
            material: None,
            asset_id: Some("slab".to_string()),
            class: None,
            storey: None,
            element_color: None,
        };
        let drawing = project_drawing(&[slab], Axis::Z, None, false);
        assert!(
            drawing
                .lines
                .iter()
                .any(|line| joins(line, [0.0, 0.0, 0.0], [2.0, 0.0, 0.0]))
        );
        assert!(
            drawing
                .lines
                .iter()
                .all(|line| !joins(line, [0.0, 0.0, 0.0], [2.0, 2.0, 0.0]))
        );

        let mut unnamed =
            named_triangle([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]], "gone");
        unnamed.asset_id = None;
        assert!(
            project_drawing(&[unnamed], Axis::Z, None, false)
                .lines
                .is_empty()
        );
    }

    #[test]
    fn a_cut_keeps_only_the_kept_side_and_draws_the_section_as_visible_lines() {
        let mesh = box_mesh([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], "box");
        let below = project_drawing(&[mesh.clone()], Axis::Z, Some(0.5), false);
        assert!(
            below
                .lines
                .iter()
                .all(|line| line.start[2] <= 0.5 + 1.0e-6 && line.end[2] <= 0.5 + 1.0e-6)
        );
        let cuts = below
            .lines
            .iter()
            .filter(|line| {
                !line.hidden
                    && (line.start[2] - 0.5).abs() < 1.0e-6
                    && (line.end[2] - 0.5).abs() < 1.0e-6
            })
            .count();
        assert!(cuts >= 4, "the cut has {cuts} lines");

        let above = project_drawing(&[mesh], Axis::Z, Some(0.5), true);
        assert!(
            above
                .lines
                .iter()
                .all(|line| line.start[2] >= 0.5 - 1.0e-6 && line.end[2] >= 0.5 - 1.0e-6)
        );
    }

    #[test]
    fn a_non_finite_triangle_is_left_out() {
        let mesh = named_triangle(
            [[0.0, 0.0, 0.0], [1.0, f64::NAN, 0.0], [0.0, 1.0, 0.0]],
            "bad",
        );
        assert!(
            project_drawing(&[mesh], Axis::Z, None, false)
                .lines
                .is_empty()
        );
    }
}
