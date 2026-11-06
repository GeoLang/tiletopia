use crate::fbx_reader::Affine;
use crate::{BYTES_HELD_PER_VERTEX, IngestError, MeshData};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

// autocad starts a binary DXF with this sentinel
const BINARY_DXF_SENTINEL: &[u8] = b"AutoCAD Binary DXF";

// ACIS entities carry their shape as SAT or SAB modeler data, not as faces
const ACIS_ENTITY_TYPES: [&str; 3] = ["3DSOLID", "BODY", "REGION"];

const MAX_INSERT_DEPTH: usize = 64;
const MAX_BLOCK_PLACEMENTS: usize = 1_000_000;

const POLYLINE_CLOSED_IN_M: i64 = 1;
const POLYLINE_POLYGON_MESH: i64 = 16;
const POLYLINE_CLOSED_IN_N: i64 = 32;
const POLYLINE_POLYFACE_MESH: i64 = 64;
// polyface and polygon mesh vertices set bit 64, polyface face records leave it clear
const VERTEX_HAS_POSITION: i64 = 64;

#[derive(Clone, Copy, Debug)]
pub struct DrawingUnit {
    code: i64,
    pub name: &'static str,
    metres: f64,
}

// codes from the autocad INSUNITS system variable reference
#[rustfmt::skip]
const DRAWING_UNITS: [DrawingUnit; 21] = [
    DrawingUnit { code: 1, name: "inches", metres: 0.0254 },
    DrawingUnit { code: 2, name: "feet", metres: 0.3048 },
    DrawingUnit { code: 3, name: "miles", metres: 1609.344 },
    DrawingUnit { code: 4, name: "millimetres", metres: 0.001 },
    DrawingUnit { code: 5, name: "centimetres", metres: 0.01 },
    DrawingUnit { code: 6, name: "metres", metres: 1.0 },
    DrawingUnit { code: 7, name: "kilometres", metres: 1000.0 },
    DrawingUnit { code: 8, name: "microinches", metres: 0.0254e-6 },
    DrawingUnit { code: 9, name: "mils", metres: 0.0254e-3 },
    DrawingUnit { code: 10, name: "yards", metres: 0.9144 },
    DrawingUnit { code: 11, name: "angstroms", metres: 1e-10 },
    DrawingUnit { code: 12, name: "nanometres", metres: 1e-9 },
    DrawingUnit { code: 13, name: "micrometres", metres: 1e-6 },
    DrawingUnit { code: 14, name: "decimetres", metres: 0.1 },
    DrawingUnit { code: 15, name: "decametres", metres: 10.0 },
    DrawingUnit { code: 16, name: "hectometres", metres: 100.0 },
    DrawingUnit { code: 17, name: "gigametres", metres: 1e9 },
    DrawingUnit { code: 18, name: "astronomical-units", metres: 149_597_870_700.0 },
    DrawingUnit { code: 19, name: "light-years", metres: 9_460_730_472_580_800.0 },
    DrawingUnit { code: 20, name: "parsecs", metres: 3.085_677_581_491_367e16 },
    DrawingUnit { code: 21, name: "us-survey-feet", metres: 1200.0 / 3937.0 },
];

impl DrawingUnit {
    pub fn named(name: &str) -> Result<Self, IngestError> {
        DRAWING_UNITS
            .iter()
            .find(|unit| unit.name == name)
            .copied()
            .ok_or_else(|| {
                let names: Vec<&str> = DRAWING_UNITS.iter().map(|unit| unit.name).collect();
                IngestError::ParseError(format!(
                    "units {name}: accepted values are {}",
                    names.join(", ")
                ))
            })
    }

    fn coded(code: i64) -> Result<Option<Self>, IngestError> {
        if code == 0 {
            return Ok(None);
        }
        let unit = DRAWING_UNITS
            .iter()
            .find(|unit| unit.code == code)
            .copied()
            .ok_or_else(|| {
                IngestError::ParseError(format!("$INSUNITS {code} is not a unit code"))
            })?;
        Ok(Some(unit))
    }
}

fn open_groups(path: &Path) -> Result<GroupReader<BufReader<File>>, IngestError> {
    let mut lines = BufReader::new(File::open(path)?);
    if lines.fill_buf()?.starts_with(BINARY_DXF_SENTINEL) {
        return Err(IngestError::ParseError(
            "binary DXF is not read, save the drawing as ASCII DXF".to_string(),
        ));
    }
    Ok(GroupReader {
        lines,
        line_number: 0,
        held: None,
    })
}

fn declared_unit(path: &Path) -> Result<Option<DrawingUnit>, IngestError> {
    let mut reader = open_groups(path)?;
    while let Some(group) = reader.next_group()? {
        // group 999 is a comment, allowed between sections
        if group.code == 999 {
            continue;
        }
        if group.code != 0 || group.value.trim() != "SECTION" {
            return Err(reader.error(format!(
                "expected SECTION, found group {} {:?}",
                group.code, group.value
            )));
        }
        let name = reader.required()?;
        // a DXF that has a HEADER writes it as its first section
        if name.code != 2 || name.value.trim() != "HEADER" {
            return Ok(None);
        }
        return match read_header(&mut reader)? {
            Some(code) => DrawingUnit::coded(code),
            None => Ok(None),
        };
    }
    Ok(None)
}

pub fn drawing_unit(
    path: &Path,
    upload_unit: Option<DrawingUnit>,
) -> Result<DrawingUnit, IngestError> {
    match (declared_unit(path)?, upload_unit) {
        (Some(declared), None) => Ok(declared),
        (None, Some(upload)) => Ok(upload),
        (Some(declared), Some(upload)) if declared.code == upload.code => Ok(declared),
        (Some(declared), Some(upload)) => Err(IngestError::ParseError(format!(
            "the DXF's $INSUNITS is {}, and the units field says {}: send no units field, \
             or the drawing's own unit",
            declared.name, upload.name
        ))),
        (None, None) => Err(IngestError::ParseError(
            "the DXF's $INSUNITS names no unit: upload it with a units field, such as \
             units=millimetres"
                .to_string(),
        )),
    }
}

pub fn read(
    path: &Path,
    max_bytes_held: u64,
    upload_unit: Option<DrawingUnit>,
) -> Result<Vec<MeshData>, IngestError> {
    let unit = drawing_unit(path, upload_unit)?;
    let mut reader = open_groups(path)?;
    let mut skipped = SkippedEntities::default();
    let drawing = read_drawing(&mut reader, &mut skipped, max_bytes_held)?;

    if !skipped.flat.is_empty() {
        tracing::warn!(
            "{}: skipped entities with no surface: {}",
            path.display(),
            describe(&skipped.flat)
        );
    }
    if !skipped.extruded.is_empty() {
        tracing::warn!(
            "{}: skipped entities extruded by a thickness, which is not read: {}",
            path.display(),
            describe(&skipped.extruded)
        );
    }

    let mut placement = Placement {
        blocks: &drawing.blocks,
        source_vertices: drawing.source_vertices(),
        max_bytes_held,
        placed_blocks: 0,
        positions: Vec::new(),
        indices: Vec::new(),
    };
    placement.place(&drawing.model_space, &Affine::scaling([unit.metres; 3]), 0)?;

    if placement.indices.is_empty() {
        return Err(IngestError::ParseError(format!(
            "the DXF has no 3D geometry: no 3DFACE, polyface or polygon mesh POLYLINE or MESH \
             reaches model space. Skipped: {}",
            describe(&skipped.flat.into_iter().chain(skipped.extruded).collect())
        )));
    }

    tracing::info!(
        "Read {} triangles from {}",
        placement.indices.len() / 3,
        path.display()
    );
    Ok(vec![MeshData {
        positions: placement.positions,
        normals: Vec::new(),
        texcoords: Vec::new(),
        indices: placement.indices,
        name: path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        material: None,
        asset_id: None,
        class: None,
        storey: None,
        element_color: None,
    }])
}

fn describe(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "nothing".to_string();
    }
    counts
        .iter()
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect::<Vec<_>>()
        .join(", ")
}

struct Group {
    code: i32,
    value: String,
}

struct GroupReader<R> {
    lines: R,
    line_number: usize,
    held: Option<Group>,
}

impl<R: BufRead> GroupReader<R> {
    fn next_group(&mut self) -> Result<Option<Group>, IngestError> {
        if let Some(group) = self.held.take() {
            return Ok(Some(group));
        }
        let Some(code_line) = self.read_line()? else {
            return Ok(None);
        };
        let code: i32 = code_line
            .trim()
            .parse()
            .map_err(|_| self.error(format!("group code {code_line:?} is not a number")))?;
        let value = self
            .read_line()?
            .ok_or_else(|| self.error(format!("group code {code} has no value")))?;
        Ok(Some(Group { code, value }))
    }

    fn required(&mut self) -> Result<Group, IngestError> {
        self.next_group()?
            .ok_or_else(|| self.error("the file ends inside a section".to_string()))
    }

    fn next_in_entity(&mut self) -> Result<Option<Group>, IngestError> {
        let group = self.required()?;
        if group.code == 0 {
            self.held = Some(group);
            return Ok(None);
        }
        Ok(Some(group))
    }

    fn rest_of_entity_thickness(&mut self) -> Result<f64, IngestError> {
        let mut thickness = 0.0;
        while let Some(group) = self.next_in_entity()? {
            if group.code == 39 {
                thickness = self.real(&group)?;
            }
        }
        Ok(thickness)
    }

    fn read_line(&mut self) -> Result<Option<String>, IngestError> {
        let mut bytes = Vec::new();
        if self.lines.read_until(b'\n', &mut bytes)? == 0 {
            return Ok(None);
        }
        self.line_number += 1;
        let text = String::from_utf8_lossy(&bytes);
        Ok(Some(text.trim_end_matches(['\r', '\n']).to_string()))
    }

    fn real(&self, group: &Group) -> Result<f64, IngestError> {
        group.value.trim().parse().map_err(|_| {
            self.error(format!(
                "group {} value {:?} is not a number",
                group.code, group.value
            ))
        })
    }

    fn integer(&self, group: &Group) -> Result<i64, IngestError> {
        group.value.trim().parse().map_err(|_| {
            self.error(format!(
                "group {} value {:?} is not an integer",
                group.code, group.value
            ))
        })
    }

    fn count(&self, group: &Group) -> Result<usize, IngestError> {
        usize::try_from(self.integer(group)?).map_err(|_| {
            self.error(format!(
                "group {} value {} is not a count",
                group.code, group.value
            ))
        })
    }

    fn error(&self, message: String) -> IngestError {
        IngestError::ParseError(format!("DXF line {}: {message}", self.line_number))
    }
}

#[derive(Default)]
struct SkippedEntities {
    flat: BTreeMap<String, usize>,
    extruded: BTreeMap<String, usize>,
}

impl SkippedEntities {
    fn add(&mut self, kind: &str, thickness: f64) {
        let counts = if thickness == 0.0 {
            &mut self.flat
        } else {
            &mut self.extruded
        };
        *counts.entry(kind.to_string()).or_default() += 1;
    }
}

#[derive(Default)]
struct Part {
    positions: Vec<[f64; 3]>,
    triangles: Vec<[u32; 3]>,
    inserts: Vec<Insert>,
}

impl Part {
    fn add_polygon(&mut self, corners: &[[f64; 3]]) {
        let first = self.positions.len() as u32;
        self.positions.extend_from_slice(corners);
        let indexes: Vec<u32> = (first..first + corners.len() as u32).collect();
        self.add_fan(&indexes);
    }

    fn add_fan(&mut self, indexes: &[u32]) {
        for pair in indexes[1..].windows(2) {
            self.triangles.push([indexes[0], pair[0], pair[1]]);
        }
    }
}

struct Block {
    base_point: [f64; 3],
    part: Part,
}

struct Insert {
    block_name: String,
    insertion_point: [f64; 3],
    scale: [f64; 3],
    rotation_degrees: f64,
    columns: u32,
    rows: u32,
    column_spacing: f64,
    row_spacing: f64,
    extrusion: [f64; 3],
}

impl Insert {
    fn transform(&self, column: u32, row: u32, base_point: [f64; 3]) -> Affine {
        let cell_offset = [
            f64::from(column) * self.column_spacing,
            f64::from(row) * self.row_spacing,
            0.0,
        ];
        object_to_world(self.extrusion)
            .times(Affine::translation(self.insertion_point))
            .times(Affine::rotation_about(2, self.rotation_degrees))
            .times(Affine::translation(cell_offset))
            .times(Affine::scaling(self.scale))
            .times(Affine::translation(base_point.map(|c| -c)))
    }
}

pub(crate) fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalized(v: [f64; 3]) -> [f64; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    v.map(|c| c / length)
}

fn object_to_world(extrusion: [f64; 3]) -> Affine {
    let normal = normalized(extrusion);
    // the dxf reference's arbitrary axis algorithm, 1/64 is its cutoff
    let near_world_z = normal[0].abs() < 1.0 / 64.0 && normal[1].abs() < 1.0 / 64.0;
    let x_axis = normalized(if near_world_z {
        cross([0.0, 1.0, 0.0], normal)
    } else {
        cross([0.0, 0.0, 1.0], normal)
    });
    let y_axis = normalized(cross(normal, x_axis));
    Affine {
        linear: [0, 1, 2].map(|row| [x_axis[row], y_axis[row], normal[row]]),
        translation: [0.0; 3],
    }
}

struct Drawing {
    blocks: HashMap<String, Block>,
    model_space: Part,
}

impl Drawing {
    fn source_vertices(&self) -> usize {
        self.model_space.positions.len()
            + self
                .blocks
                .values()
                .map(|block| block.part.positions.len())
                .sum::<usize>()
    }
}

fn read_drawing<R: BufRead>(
    reader: &mut GroupReader<R>,
    skipped: &mut SkippedEntities,
    max_bytes_held: u64,
) -> Result<Drawing, IngestError> {
    let mut drawing = Drawing {
        blocks: HashMap::new(),
        model_space: Part::default(),
    };
    while let Some(group) = reader.next_group()? {
        // group 999 is a comment, allowed between sections
        if group.code == 999 {
            continue;
        }
        if group.code != 0 {
            return Err(reader.error(format!(
                "expected SECTION, found group {} {:?}",
                group.code, group.value
            )));
        }
        match group.value.trim() {
            "EOF" => break,
            "SECTION" => {}
            other => return Err(reader.error(format!("expected SECTION, found {other}"))),
        }
        let name = reader.required()?;
        if name.code != 2 {
            return Err(reader.error("SECTION has no name".to_string()));
        }
        match name.value.trim() {
            "BLOCKS" => read_blocks(reader, &mut drawing.blocks, skipped)?,
            "ENTITIES" => read_entity_list(reader, "ENDSEC", &mut drawing.model_space, skipped)?,
            _ => skip_section(reader)?,
        }

        let bytes_held = (drawing.source_vertices() as u64).saturating_mul(BYTES_HELD_PER_VERTEX);
        if bytes_held > max_bytes_held {
            return Err(IngestError::ParseError(format!(
                "DXF stores {} vertices at {BYTES_HELD_PER_VERTEX} bytes each, past the \
                 {max_bytes_held} byte limit",
                drawing.source_vertices()
            )));
        }
    }
    Ok(drawing)
}

fn is_end_of_section(group: &Group) -> bool {
    group.code == 0 && group.value.trim() == "ENDSEC"
}

fn read_header<R: BufRead>(reader: &mut GroupReader<R>) -> Result<Option<i64>, IngestError> {
    loop {
        let group = reader.required()?;
        if is_end_of_section(&group) {
            return Ok(None);
        }
        if group.code == 9 && group.value.trim() == "$INSUNITS" {
            let value = reader.required()?;
            return Ok(Some(reader.integer(&value)?));
        }
    }
}

fn skip_section<R: BufRead>(reader: &mut GroupReader<R>) -> Result<(), IngestError> {
    while !is_end_of_section(&reader.required()?) {}
    Ok(())
}

fn read_blocks<R: BufRead>(
    reader: &mut GroupReader<R>,
    blocks: &mut HashMap<String, Block>,
    skipped: &mut SkippedEntities,
) -> Result<(), IngestError> {
    loop {
        let group = reader.required()?;
        if is_end_of_section(&group) {
            return Ok(());
        }
        if group.code != 0 || group.value.trim() != "BLOCK" {
            return Err(reader.error(format!(
                "expected BLOCK, found group {} {:?}",
                group.code, group.value
            )));
        }
        let mut name = None;
        let mut base_point = [0.0; 3];
        while let Some(group) = reader.next_in_entity()? {
            match group.code {
                2 => name = Some(group.value.trim().to_string()),
                10 | 20 | 30 => base_point[group.code as usize / 10 - 1] = reader.real(&group)?,
                _ => {}
            }
        }
        let name = name.ok_or_else(|| reader.error("BLOCK has no name".to_string()))?;
        let mut part = Part::default();
        read_entity_list(reader, "ENDBLK", &mut part, skipped)?;
        blocks.insert(name, Block { base_point, part });
    }
}

fn read_entity_list<R: BufRead>(
    reader: &mut GroupReader<R>,
    end: &str,
    part: &mut Part,
    skipped: &mut SkippedEntities,
) -> Result<(), IngestError> {
    loop {
        let group = reader.required()?;
        if group.code != 0 {
            return Err(reader.error(format!(
                "expected an entity, found group {} {:?}",
                group.code, group.value
            )));
        }
        let kind = group.value.trim();
        if kind == end {
            reader.rest_of_entity_thickness()?;
            return Ok(());
        }
        match kind {
            "3DFACE" => read_face(reader, part)?,
            "POLYLINE" => read_polyline(reader, part, skipped)?,
            "MESH" => read_subdivision_mesh(reader, part)?,
            "INSERT" => part.inserts.push(read_insert(reader)?),
            "SEQEND" => {
                reader.rest_of_entity_thickness()?;
            }
            kind if ACIS_ENTITY_TYPES.contains(&kind) => {
                return Err(IngestError::ParseError(format!(
                    "the DXF holds a {kind}, an ACIS solid whose geometry is binary SAT data \
                     and is not read: export the drawing with its solids converted to meshes"
                )));
            }
            kind => {
                let thickness = reader.rest_of_entity_thickness()?;
                skipped.add(kind, thickness);
            }
        }
    }
}

fn read_face<R: BufRead>(reader: &mut GroupReader<R>, part: &mut Part) -> Result<(), IngestError> {
    let mut corners = [[0.0; 3]; 4];
    let mut fourth_corner_given = false;
    while let Some(group) = reader.next_in_entity()? {
        if let 10..=13 | 20..=23 | 30..=33 = group.code {
            let corner = group.code as usize % 10;
            corners[corner][group.code as usize / 10 - 1] = reader.real(&group)?;
            fourth_corner_given |= corner == 3;
        }
    }
    // a three-cornered 3DFACE repeats its third corner as the fourth
    let corner_count = if fourth_corner_given && corners[3] != corners[2] {
        4
    } else {
        3
    };
    part.add_polygon(&corners[..corner_count]);
    Ok(())
}

struct Vertex {
    flags: i64,
    position: [f64; 3],
    face_indexes: [i64; 4],
}

fn read_vertex<R: BufRead>(reader: &mut GroupReader<R>) -> Result<Vertex, IngestError> {
    let mut vertex = Vertex {
        flags: 0,
        position: [0.0; 3],
        face_indexes: [0; 4],
    };
    while let Some(group) = reader.next_in_entity()? {
        match group.code {
            10 | 20 | 30 => vertex.position[group.code as usize / 10 - 1] = reader.real(&group)?,
            70 => vertex.flags = reader.integer(&group)?,
            71..=74 => vertex.face_indexes[group.code as usize - 71] = reader.integer(&group)?,
            _ => {}
        }
    }
    Ok(vertex)
}

fn read_polyline<R: BufRead>(
    reader: &mut GroupReader<R>,
    part: &mut Part,
    skipped: &mut SkippedEntities,
) -> Result<(), IngestError> {
    let mut flags = 0;
    let mut mesh_m = 0;
    let mut mesh_n = 0;
    let mut thickness = 0.0;
    while let Some(group) = reader.next_in_entity()? {
        match group.code {
            70 => flags = reader.integer(&group)?,
            71 => mesh_m = reader.count(&group)?,
            72 => mesh_n = reader.count(&group)?,
            39 => thickness = reader.real(&group)?,
            _ => {}
        }
    }
    let is_polyface = flags & POLYLINE_POLYFACE_MESH != 0;
    let is_polygon_mesh = flags & POLYLINE_POLYGON_MESH != 0;

    let first_vertex = part.positions.len();
    loop {
        let group = reader.required()?;
        match group.value.trim() {
            "SEQEND" => {
                reader.rest_of_entity_thickness()?;
                break;
            }
            "VERTEX" => {}
            other => {
                return Err(reader.error(format!("POLYLINE ends at {other} with no SEQEND")));
            }
        }
        let vertex = read_vertex(reader)?;
        if !is_polyface && !is_polygon_mesh {
            continue;
        }
        if vertex.flags & VERTEX_HAS_POSITION != 0 {
            part.positions.push(vertex.position);
            continue;
        }
        let vertices_so_far = part.positions.len() - first_vertex;
        let mut corners = Vec::with_capacity(4);
        // a negative index hides the edge that starts at that vertex, zero ends the list
        for index in vertex
            .face_indexes
            .into_iter()
            .take_while(|&index| index != 0)
        {
            let number = index.unsigned_abs() as usize;
            if number > vertices_so_far {
                return Err(reader.error(format!(
                    "polyface face names vertex {number}, and only {vertices_so_far} come before it"
                )));
            }
            corners.push((first_vertex + number - 1) as u32);
        }
        if corners.len() >= 3 {
            part.add_fan(&corners);
        }
    }

    if is_polygon_mesh {
        return add_polygon_mesh(part, first_vertex, mesh_m, mesh_n, flags);
    }
    if !is_polyface {
        skipped.add("POLYLINE", thickness);
    }
    Ok(())
}

fn add_polygon_mesh(
    part: &mut Part,
    first_vertex: usize,
    mesh_m: usize,
    mesh_n: usize,
    flags: i64,
) -> Result<(), IngestError> {
    let vertex_count = part.positions.len() - first_vertex;
    if mesh_m < 2 || mesh_n < 2 || vertex_count != mesh_m * mesh_n {
        return Err(IngestError::ParseError(format!(
            "polygon mesh POLYLINE of {mesh_m} by {mesh_n} has {vertex_count} vertices: \
             smoothed polygon meshes are not read"
        )));
    }
    let rows = if flags & POLYLINE_CLOSED_IN_M != 0 {
        mesh_m
    } else {
        mesh_m - 1
    };
    let columns = if flags & POLYLINE_CLOSED_IN_N != 0 {
        mesh_n
    } else {
        mesh_n - 1
    };
    let index = |row: usize, column: usize| {
        (first_vertex + (row % mesh_m) * mesh_n + column % mesh_n) as u32
    };
    for row in 0..rows {
        for column in 0..columns {
            part.add_fan(&[
                index(row, column),
                index(row, column + 1),
                index(row + 1, column + 1),
                index(row + 1, column),
            ]);
        }
    }
    Ok(())
}

fn read_subdivision_mesh<R: BufRead>(
    reader: &mut GroupReader<R>,
    part: &mut Part,
) -> Result<(), IngestError> {
    let first_vertex = part.positions.len();
    let mut subdivision_levels = 0;
    let mut vertices_left = 0;
    let mut position = [0.0; 3];
    let mut face_entries_left = 0;
    let mut face_corners_left = 0;
    let mut face = Vec::new();
    while let Some(group) = reader.next_in_entity()? {
        match group.code {
            91 => subdivision_levels = reader.integer(&group)?,
            92 => vertices_left = reader.count(&group)?,
            10 | 20 | 30 if vertices_left > 0 => {
                position[group.code as usize / 10 - 1] = reader.real(&group)?;
                if group.code == 30 {
                    part.positions.push(position);
                    vertices_left -= 1;
                }
            }
            93 => face_entries_left = reader.count(&group)?,
            // the face list is each face's corner count followed by that many vertex indexes
            90 if face_entries_left > 0 => {
                face_entries_left -= 1;
                let value = reader.count(&group)?;
                if face_corners_left == 0 {
                    face_corners_left = value;
                    face.clear();
                    continue;
                }
                let vertex_count = part.positions.len() - first_vertex;
                if value >= vertex_count {
                    return Err(
                        reader.error(format!("MESH face names vertex {value} of {vertex_count}"))
                    );
                }
                face.push((first_vertex + value) as u32);
                face_corners_left -= 1;
                if face_corners_left == 0 && face.len() >= 3 {
                    part.add_fan(&face);
                }
            }
            _ => {}
        }
    }
    if vertices_left > 0 || face_entries_left > 0 || face_corners_left > 0 {
        return Err(reader.error("MESH ends inside its vertex or face list".to_string()));
    }
    if subdivision_levels > 0 {
        tracing::warn!(
            "MESH with {subdivision_levels} subdivision levels is tiled as its unsmoothed control mesh"
        );
    }
    Ok(())
}

fn read_insert<R: BufRead>(reader: &mut GroupReader<R>) -> Result<Insert, IngestError> {
    let mut insert = Insert {
        block_name: String::new(),
        insertion_point: [0.0; 3],
        scale: [1.0; 3],
        rotation_degrees: 0.0,
        columns: 1,
        rows: 1,
        column_spacing: 0.0,
        row_spacing: 0.0,
        extrusion: [0.0, 0.0, 1.0],
    };
    while let Some(group) = reader.next_in_entity()? {
        match group.code {
            2 => insert.block_name = group.value.trim().to_string(),
            10 | 20 | 30 => {
                insert.insertion_point[group.code as usize / 10 - 1] = reader.real(&group)?
            }
            41..=43 => insert.scale[group.code as usize - 41] = reader.real(&group)?,
            50 => insert.rotation_degrees = reader.real(&group)?,
            70 => insert.columns = array_count(reader, &group)?,
            71 => insert.rows = array_count(reader, &group)?,
            44 => insert.column_spacing = reader.real(&group)?,
            45 => insert.row_spacing = reader.real(&group)?,
            210 | 220 | 230 => {
                insert.extrusion[group.code as usize / 10 - 21] = reader.real(&group)?
            }
            _ => {}
        }
    }
    if insert.block_name.is_empty() {
        return Err(reader.error("INSERT names no block".to_string()));
    }
    if insert.extrusion == [0.0; 3] {
        return Err(reader.error(format!(
            "INSERT of {} has a zero extrusion direction",
            insert.block_name
        )));
    }
    Ok(insert)
}

fn array_count<R: BufRead>(reader: &GroupReader<R>, group: &Group) -> Result<u32, IngestError> {
    match u32::try_from(reader.integer(group)?) {
        Ok(count) if count >= 1 => Ok(count),
        _ => Err(reader.error(format!(
            "INSERT {} {} is not a row or column count",
            if group.code == 70 { "columns" } else { "rows" },
            group.value.trim()
        ))),
    }
}

struct Placement<'a> {
    blocks: &'a HashMap<String, Block>,
    source_vertices: usize,
    max_bytes_held: u64,
    placed_blocks: usize,
    positions: Vec<[f64; 3]>,
    indices: Vec<u32>,
}

impl Placement<'_> {
    fn place(&mut self, part: &Part, transform: &Affine, depth: usize) -> Result<(), IngestError> {
        let vertices_held = self.source_vertices + self.positions.len() + part.positions.len();
        if (vertices_held as u64).saturating_mul(BYTES_HELD_PER_VERTEX) > self.max_bytes_held {
            return Err(IngestError::ParseError(format!(
                "DXF stores {} vertices and its INSERTs place {} more, at \
                 {BYTES_HELD_PER_VERTEX} bytes each past the {} byte limit",
                self.source_vertices,
                self.positions.len() + part.positions.len(),
                self.max_bytes_held
            )));
        }

        let first = self.positions.len() as u32;
        self.positions.extend(
            part.positions
                .iter()
                .map(|&position| transform.apply(position)),
        );
        // a mirroring transform turns counter-clockwise triangles clockwise
        let mirroring = transform.is_mirroring();
        for &[a, b, c] in &part.triangles {
            let triangle = if mirroring { [a, c, b] } else { [a, b, c] };
            self.indices.extend(triangle.map(|index| first + index));
        }

        let blocks = self.blocks;
        for insert in &part.inserts {
            let block = blocks.get(&insert.block_name).ok_or_else(|| {
                IngestError::ParseError(format!(
                    "INSERT names block {}, which the DXF does not define",
                    insert.block_name
                ))
            })?;
            if depth >= MAX_INSERT_DEPTH {
                return Err(IngestError::ParseError(format!(
                    "INSERT of {} nests blocks deeper than {MAX_INSERT_DEPTH} levels, \
                     or a block inserts itself",
                    insert.block_name
                )));
            }
            for row in 0..insert.rows {
                for column in 0..insert.columns {
                    self.placed_blocks += 1;
                    if self.placed_blocks > MAX_BLOCK_PLACEMENTS {
                        return Err(IngestError::ParseError(format!(
                            "DXF INSERTs place more than {MAX_BLOCK_PLACEMENTS} blocks"
                        )));
                    }
                    let block_transform =
                        transform.times(insert.transform(column, row, block.base_point));
                    self.place(&block.part, &block_transform, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiletopia_core::mesh_tiler::{MeshTilingConfig, tile_meshes};

    const MILLIMETRES: &str = "4";
    const UNITLESS: &str = "0";

    type Groups = &'static [(i32, &'static str)];

    fn write_dxf(
        dir: &Path,
        insertion_units: &str,
        blocks: Groups,
        entities: &[Groups],
    ) -> std::path::PathBuf {
        let blocks_start: Groups = &[(0, "SECTION"), (2, "BLOCKS")];
        let entities_start: Groups = &[(0, "ENDSEC"), (0, "SECTION"), (2, "ENTITIES")];
        let end: Groups = &[(0, "ENDSEC"), (0, "EOF")];
        let sections: String = [blocks_start, blocks, entities_start]
            .into_iter()
            .chain(entities.iter().copied())
            .chain([end])
            .flatten()
            .map(|(code, value)| format!("{code}\n{value}\n"))
            .collect();
        let text = format!(
            "0\nSECTION\n2\nHEADER\n9\n$INSUNITS\n70\n{insertion_units}\n0\nENDSEC\n{sections}"
        );
        let path = dir.join("drawing.dxf");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[rustfmt::skip]
    const INNER_QUAD_INSERTED_BY_OUTER_BLOCK: Groups = &[
        (0, "BLOCK"), (2, "inner"), (10, "0"), (20, "0"), (30, "0"),
        (0, "3DFACE"),
        (10, "0"), (20, "0"), (30, "0"),
        (11, "1000"), (21, "0"), (31, "0"),
        (12, "1000"), (22, "1000"), (32, "0"),
        (13, "0"), (23, "1000"), (33, "0"),
        (0, "ENDBLK"),
        (0, "BLOCK"), (2, "outer"), (10, "0"), (20, "0"), (30, "0"),
        (0, "INSERT"), (2, "inner"),
        (10, "1000"), (20, "0"), (30, "0"),
        (41, "2"), (42, "2"), (43, "2"),
        (0, "ENDBLK"),
    ];

    #[rustfmt::skip]
    const THREE_CORNERED_FACE: Groups = &[
        (0, "3DFACE"),
        (10, "0"), (20, "0"), (30, "0"),
        (11, "1000"), (21, "0"), (31, "0"),
        (12, "0"), (22, "1000"), (32, "0"),
        (13, "0"), (23, "1000"), (33, "0"),
    ];

    #[rustfmt::skip]
    const POLYFACE_QUAD_AND_TRIANGLE: Groups = &[
        (0, "POLYLINE"), (66, "1"), (10, "0"), (20, "0"), (30, "0"),
        (70, "64"), (71, "5"), (72, "2"),
        (0, "VERTEX"), (10, "10000"), (20, "0"), (30, "0"), (70, "192"),
        (0, "VERTEX"), (10, "11000"), (20, "0"), (30, "0"), (70, "192"),
        (0, "VERTEX"), (10, "11000"), (20, "1000"), (30, "0"), (70, "192"),
        (0, "VERTEX"), (10, "10000"), (20, "1000"), (30, "0"), (70, "192"),
        (0, "VERTEX"), (10, "10000"), (20, "0"), (30, "2000"), (70, "192"),
        (0, "VERTEX"), (10, "0"), (20, "0"), (30, "0"), (70, "128"),
        (71, "1"), (72, "2"), (73, "3"), (74, "4"),
        (0, "VERTEX"), (10, "0"), (20, "0"), (30, "0"), (70, "128"),
        (71, "1"), (72, "2"), (73, "-5"),
        (0, "SEQEND"),
    ];

    #[rustfmt::skip]
    const OUTER_BLOCK_TURNED_AND_LIFTED: Groups = &[
        (0, "INSERT"), (2, "outer"),
        (10, "20000"), (20, "0"), (30, "5000"), (50, "90"),
    ];

    const LINE: Groups = &[(0, "LINE"), (10, "0"), (20, "0"), (11, "1"), (21, "1")];

    #[test]
    fn faces_a_polyface_and_a_nested_insert_tile_to_their_triangles_and_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_dxf(
            dir.path(),
            MILLIMETRES,
            INNER_QUAD_INSERTED_BY_OUTER_BLOCK,
            &[
                THREE_CORNERED_FACE,
                POLYFACE_QUAD_AND_TRIANGLE,
                OUTER_BLOCK_TURNED_AND_LIFTED,
                LINE,
            ],
        );

        let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> = read(&path, u64::MAX, None)
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();
        let config = MeshTilingConfig {
            content_y_up: true,
            ..Default::default()
        };
        let output = dir.path().join("tiles_out");
        let stats = tile_meshes(&meshes, &output, &config).unwrap();
        // one from the triangle face, three from the polyface, two from the nested quad
        assert_eq!(stats.total_triangles, 6);

        let tileset: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output.join("tileset.json")).unwrap())
                .unwrap();
        let bounding_box: Vec<f64> = tileset["root"]["boundingVolume"]["box"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_f64().unwrap())
            .collect();
        // x runs 0 to 20 m, y 0 to 3 m and z 0 to 5 m: the nested quad lands at x 18 to 20, y 1 to 3, z 5
        #[rustfmt::skip]
        let expected = [
            10.0, 1.5, 2.5,
            10.0, 0.0, 0.0,
            0.0, 1.5, 0.0,
            0.0, 0.0, 2.5,
        ];
        for (actual, expected) in bounding_box.iter().zip(expected) {
            assert!(
                (actual - expected).abs() < 1e-4,
                "{bounding_box:?} is not {expected:?}"
            );
        }
    }

    #[test]
    fn a_drawing_of_only_2d_entities_fails_naming_that() {
        let dir = tempfile::tempdir().unwrap();
        #[rustfmt::skip]
        let flat_entities: Groups = &[
            (0, "LWPOLYLINE"), (90, "2"), (10, "0"), (20, "0"), (10, "1"), (20, "1"),
            (0, "TEXT"), (10, "0"), (20, "0"), (40, "1"), (1, "label"),
            (0, "HATCH"), (2, "SOLID"),
        ];
        let path = write_dxf(dir.path(), MILLIMETRES, &[], &[LINE, flat_entities]);

        let error = read(&path, u64::MAX, None).unwrap_err().to_string();
        assert!(error.contains("no 3D geometry"), "{error}");
        assert!(error.contains("1 LINE"), "{error}");
        assert!(error.contains("1 TEXT"), "{error}");
    }

    #[test]
    fn an_acis_solid_fails_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let solid: Groups = &[(0, "3DSOLID"), (70, "1"), (1, "sat data")];
        let path = write_dxf(dir.path(), MILLIMETRES, &[], &[THREE_CORNERED_FACE, solid]);

        let error = read(&path, u64::MAX, None).unwrap_err().to_string();
        assert!(error.contains("3DSOLID"), "{error}");
        assert!(error.contains("ACIS"), "{error}");
    }

    fn largest_x(meshes: &[MeshData]) -> f64 {
        meshes
            .iter()
            .flat_map(|mesh| mesh.positions.iter())
            .map(|position| position[0])
            .fold(f64::MIN, f64::max)
    }

    #[test]
    fn a_unitless_drawing_is_refused_until_the_units_field_names_its_unit() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_dxf(dir.path(), UNITLESS, &[], &[THREE_CORNERED_FACE]);

        let error = read(&path, u64::MAX, None).unwrap_err().to_string();
        assert!(error.contains("$INSUNITS"), "{error}");
        assert!(error.contains("units field"), "{error}");

        let millimetres = DrawingUnit::named("millimetres").unwrap();
        let meshes = read(&path, u64::MAX, Some(millimetres)).unwrap();
        assert_eq!(largest_x(&meshes), 1.0);
    }

    #[test]
    fn a_units_field_that_disagrees_with_insunits_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_dxf(dir.path(), MILLIMETRES, &[], &[THREE_CORNERED_FACE]);

        let inches = DrawingUnit::named("inches").unwrap();
        let error = read(&path, u64::MAX, Some(inches)).unwrap_err().to_string();
        assert!(error.contains("millimetres"), "{error}");
        assert!(error.contains("inches"), "{error}");

        let millimetres = DrawingUnit::named("millimetres").unwrap();
        assert_eq!(
            largest_x(&read(&path, u64::MAX, Some(millimetres)).unwrap()),
            1.0
        );
    }

    #[test]
    fn a_unit_name_off_the_insunits_table_is_refused() {
        let error = DrawingUnit::named("furlongs").unwrap_err().to_string();
        assert!(error.contains("millimetres"), "{error}");
    }
}
