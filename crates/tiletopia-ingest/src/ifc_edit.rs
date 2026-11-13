use std::collections::{HashMap, HashSet};

use ifc_lite_core::parser::header::first_declared_schema;
use ifc_lite_core::{
    DecodedEntity, EntityDecoder, EntityScanner, IfcType, SchemaVersion, ifc_type_from_keyword,
};

const MAX_MESH_VERTICES: usize = 30_000;
const MAX_MESH_TRIANGLES: usize = 60_000;
const MAX_COORDINATE: f64 = 1.0e7;

#[derive(Debug, PartialEq)]
pub enum EditError {
    MissingElement,
    NotAPlacement,
    NotARelation,
    TooLarge,
    InvalidMesh,
    InvalidValue,
    NoStructure,
    StillReferenced,
    UnsupportedSchema,
    AlreadyRelated,
    UnusableLengthUnit,
}

#[derive(Debug)]
pub struct AddedElement {
    pub bytes: Vec<u8>,
    pub express_id: u32,
    pub global_id: String,
    pub positions: Vec<[f64; 3]>,
    pub indices: Vec<u32>,
}

pub fn translate_placement(
    source: &[u8],
    express_id: u32,
    delta: [f64; 3],
) -> Result<Vec<u8>, EditError> {
    finite_vector(delta)?;
    let metres_per_unit = metres_per_unit(source)?;
    let (source, axis_id) = own_axis_placement(source, express_id)?;
    let point_id = decode(&source, axis_id)?
        .get_ref(0)
        .ok_or(EditError::NotAPlacement)?;
    let location = cartesian_point(&source, point_id)?;
    let (source, point_id) = unshared(&source, axis_id, 0, point_id)?;
    let moved: [f64; 3] =
        std::array::from_fn(|axis| location[axis] + delta[axis] / metres_per_unit);
    rewrite_coordinates(&source, point_id, &moved)
}

pub fn yaw_placement(source: &[u8], express_id: u32, radians: f64) -> Result<Vec<u8>, EditError> {
    if !radians.is_finite() {
        return Err(EditError::InvalidValue);
    }
    let (source, axis_id) = own_axis_placement(source, express_id)?;
    let axis = decode(&source, axis_id)?;
    let (sin, cos) = radians.sin_cos();
    if let Some(direction_id) = axis.get_ref(2) {
        let (x, y, z) = direction_ratios(&source, direction_id)?;
        let rotated = [x * cos - y * sin, x * sin + y * cos, z];
        let (source, direction_id) = unshared(&source, axis_id, 2, direction_id)?;
        return rewrite_coordinates(&source, direction_id, &rotated);
    }
    let direction_id = max_id(&source) + 1;
    let direction = format!(
        "#{direction_id}=IFCDIRECTION(({},{},0.));\n",
        step_number(cos),
        step_number(sin)
    );
    let with_direction = append_entities(&source, direction.as_bytes())?;
    replace_attribute(&with_direction, axis_id, 2, &format!("#{direction_id}"))
}

fn own_axis_placement(source: &[u8], express_id: u32) -> Result<(Vec<u8>, u32), EditError> {
    let (source, local_id) = own_local_placement(source, express_id)?;
    let axis_id = decode(&source, local_id)?
        .get_ref(1)
        .ok_or(EditError::NotAPlacement)?;
    if decode(&source, axis_id)?.ifc_type != IfcType::IfcAxis2Placement3D {
        return Err(EditError::NotAPlacement);
    }
    unshared(&source, local_id, 1, axis_id)
}

fn own_local_placement(source: &[u8], express_id: u32) -> Result<(Vec<u8>, u32), EditError> {
    let local_id = decode(source, express_id)?
        .get_ref(5)
        .ok_or(EditError::NotAPlacement)?;
    if decode(source, local_id)?.ifc_type != IfcType::IfcLocalPlacement {
        return Err(EditError::NotAPlacement);
    }
    let uses = placement_uses(source)?;
    let placed_with_another_product = uses
        .products_placed_by
        .get(&local_id)
        .is_some_and(|products| products.iter().any(|product| *product != express_id));
    if !placed_with_another_product {
        return Ok((source.to_vec(), local_id));
    }
    let followers = decomposition_below(source, express_id)?;
    let (mut bytes, copy_id) = unshared(source, express_id, 5, local_id)?;
    for child in uses
        .placements_relative_to
        .get(&local_id)
        .map_or(&[][..], Vec::as_slice)
    {
        let placed_only_followers = uses
            .products_placed_by
            .get(child)
            .is_some_and(|products| products.iter().all(|product| followers.contains(product)));
        if placed_only_followers {
            bytes = replace_attribute(&bytes, *child, 0, &format!("#{copy_id}"))?;
        }
    }
    Ok((bytes, copy_id))
}

#[derive(Default)]
struct PlacementUses {
    products_placed_by: HashMap<u32, Vec<u32>>,
    placements_relative_to: HashMap<u32, Vec<u32>>,
}

fn placement_uses(source: &[u8]) -> Result<PlacementUses, EditError> {
    let mut uses = PlacementUses::default();
    let mut scanner = EntityScanner::new(source);
    let mut decoder = EntityDecoder::new(source);
    while let Some((id, keyword, start, end)) = scanner.next_entity() {
        let ifc_type = ifc_type_from_keyword(keyword);
        let is_product = ifc_type.is_subtype_of(IfcType::IfcProduct);
        if !is_product && ifc_type != IfcType::IfcLocalPlacement {
            continue;
        }
        let entity = decoder
            .decode_at(start, end)
            .map_err(|_| EditError::MissingElement)?;
        if is_product {
            if let Some(placement) = entity.get_ref(5) {
                uses.products_placed_by
                    .entry(placement)
                    .or_default()
                    .push(id);
            }
        } else if let Some(parent) = entity.get_ref(0) {
            uses.placements_relative_to
                .entry(parent)
                .or_default()
                .push(id);
        }
    }
    Ok(uses)
}

fn decomposition_below(source: &[u8], express_id: u32) -> Result<HashSet<u32>, EditError> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut scanner = EntityScanner::new(source);
    let mut decoder = EntityDecoder::new(source);
    while let Some((_, keyword, start, end)) = scanner.next_entity() {
        // each of these relations names the host or whole at attribute 4 and its openings, fillings or parts at 5
        if !matches!(
            ifc_type_from_keyword(keyword),
            IfcType::IfcRelVoidsElement
                | IfcType::IfcRelFillsElement
                | IfcType::IfcRelAggregates
                | IfcType::IfcRelNests
        ) {
            continue;
        }
        let relation = decoder
            .decode_at(start, end)
            .map_err(|_| EditError::MissingElement)?;
        let Some(parent) = relation.get_ref(4) else {
            continue;
        };
        let related: Vec<u32> = match relation.get_ref(5) {
            Some(single) => vec![single],
            None => relation
                .get_list(5)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| item.as_entity_ref())
                .collect(),
        };
        children.entry(parent).or_default().extend(related);
    }
    let mut below = HashSet::new();
    let mut pending = vec![express_id];
    while let Some(parent) = pending.pop() {
        for child in children.get(&parent).into_iter().flatten() {
            if below.insert(*child) {
                pending.push(*child);
            }
        }
    }
    Ok(below)
}

fn unshared(
    source: &[u8],
    owner_id: u32,
    attribute: usize,
    child_id: u32,
) -> Result<(Vec<u8>, u32), EditError> {
    if reference_count(source, child_id) <= 1 {
        return Ok((source.to_vec(), child_id));
    }
    let copy_id = max_id(source) + 1;
    let span = entity_span(source, child_id).ok_or(EditError::MissingElement)?;
    let entity = source[span].trim_ascii_end();
    let body = entity
        .iter()
        .position(|byte| *byte == b'=')
        .ok_or(EditError::MissingElement)?;
    let mut copy = format!("#{copy_id}").into_bytes();
    copy.extend_from_slice(&entity[body..]);
    copy.push(b'\n');
    let with_copy = append_entities(source, &copy)?;
    let repointed = replace_attribute(&with_copy, owner_id, attribute, &format!("#{copy_id}"))?;
    Ok((repointed, copy_id))
}

pub(crate) fn reference_count(source: &[u8], id: u32) -> usize {
    let mut scanner = EntityScanner::new(source);
    let mut count = 0;
    while let Some((_, _, start, end)) = scanner.next_entity() {
        count += reference_depths(&source[whole_entity(source, start, end)], id).len();
    }
    count
}

pub fn delete_product(source: &[u8], express_id: u32) -> Result<Vec<u8>, EditError> {
    entity_span(source, express_id).ok_or(EditError::MissingElement)?;
    let mut replacements: Vec<(std::ops::Range<usize>, Vec<u8>)> = Vec::new();
    let mut scanner = EntityScanner::new(source);
    while let Some((id, keyword, start, end)) = scanner.next_entity() {
        let span = whole_entity(source, start, end);
        if id == express_id {
            replacements.push((span, Vec::new()));
            continue;
        }
        let entity = &source[span.clone()];
        let depths = reference_depths(entity, express_id);
        if depths.is_empty() {
            continue;
        }
        let is_relation = keyword
            .get(..6)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("IFCREL"));
        if depths.contains(&1) {
            if !is_relation {
                return Err(EditError::StillReferenced);
            }
            replacements.push((span, Vec::new()));
            continue;
        }
        let stripped = strip_list_references(entity, express_id);
        match (stripped.emptied_a_list, is_relation) {
            (false, _) => replacements.push((span, stripped.bytes)),
            (true, true) => replacements.push((span, Vec::new())),
            (true, false) => return Err(EditError::StillReferenced),
        }
    }
    let mut next = Vec::with_capacity(source.len());
    let mut cursor = 0;
    for (span, replacement) in replacements {
        next.extend_from_slice(&source[cursor..span.start]);
        next.extend_from_slice(&replacement);
        cursor = span.end;
    }
    next.extend_from_slice(&source[cursor..]);
    Ok(next)
}

pub fn add_proxy(
    source: &[u8],
    origin: [f64; 3],
    size: [f64; 3],
) -> Result<AddedElement, EditError> {
    finite_vector(origin)?;
    if size
        .iter()
        .any(|axis| !axis.is_finite() || *axis <= 0.0 || *axis > 1_000.0)
    {
        return Err(EditError::InvalidValue);
    }
    let tag_list = unset_tag_list(source)?;
    let metres_per_unit = metres_per_unit(source)?;
    let mut next = max_id(source) + 1;
    let owner = first_of(source, IfcType::IfcOwnerHistory);
    let context = first_of(source, IfcType::IfcGeometricRepresentationContext);
    let global_id = ifc_guid();
    let owner_token = owner
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "$".to_string());
    let context_token = context
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "$".to_string());
    let (local, indices) = box_mesh(size);
    let positions: Vec<[f64; 3]> = local
        .iter()
        .map(|point| {
            [
                point[0] + origin[0],
                point[1] + origin[1],
                point[2] + origin[2],
            ]
        })
        .collect();
    let mut block = String::new();
    let point_list = next;
    next += 1;
    block.push_str(&format!(
        "#{point_list}=IFCCARTESIANPOINTLIST3D(({}){tag_list});\n",
        local
            .iter()
            .map(|point| format!(
                "({},{},{})",
                step_number(point[0] / metres_per_unit),
                step_number(point[1] / metres_per_unit),
                step_number(point[2] / metres_per_unit)
            ))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let mut faces = Vec::new();
    for face in BOX_FACES {
        let face_id = next;
        next += 1;
        faces.push(face_id);
        block.push_str(&format!(
            "#{face_id}=IFCINDEXEDPOLYGONALFACE(({},{},{},{}));\n",
            face[0], face[1], face[2], face[3]
        ));
    }
    let face_set = next;
    next += 1;
    block.push_str(&format!(
        "#{face_set}=IFCPOLYGONALFACESET(#{point_list},.T.,({}),$);\n",
        faces
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let shape = next;
    next += 1;
    block.push_str(&format!(
        "#{shape}=IFCSHAPEREPRESENTATION({context_token},'Body','Tessellation',(#{face_set}));\n"
    ));
    let definition = next;
    next += 1;
    block.push_str(&format!(
        "#{definition}=IFCPRODUCTDEFINITIONSHAPE($,$,(#{shape}));\n"
    ));
    let location = next;
    next += 1;
    block.push_str(&format!(
        "#{location}=IFCCARTESIANPOINT(({},{},{}));\n",
        step_number(origin[0] / metres_per_unit),
        step_number(origin[1] / metres_per_unit),
        step_number(origin[2] / metres_per_unit)
    ));
    let axis = next;
    next += 1;
    block.push_str(&format!("#{axis}=IFCAXIS2PLACEMENT3D(#{location},$,$);\n"));
    let placement = next;
    next += 1;
    block.push_str(&format!("#{placement}=IFCLOCALPLACEMENT($,#{axis});\n"));
    let proxy = next;
    block.push_str(&format!(
        "#{proxy}=IFCBUILDINGELEMENTPROXY('{global_id}',{owner_token},'Added element',$,$,#{placement},#{definition},$,$);\n"
    ));
    let mut bytes = append_entities(source, block.as_bytes())?;
    bytes = attach_containment(&bytes, proxy, &owner_token)?;
    Ok(AddedElement {
        bytes,
        express_id: proxy,
        global_id,
        positions,
        indices,
    })
}

pub fn replace_mesh(
    source: &[u8],
    express_id: u32,
    positions: &[f64],
    indices: &[u32],
) -> Result<Vec<u8>, EditError> {
    if !positions.len().is_multiple_of(3) || !indices.len().is_multiple_of(3) || indices.is_empty()
    {
        return Err(EditError::InvalidMesh);
    }
    let vertices = positions.len() / 3;
    let triangles = indices.len() / 3;
    if vertices > MAX_MESH_VERTICES || triangles > MAX_MESH_TRIANGLES {
        return Err(EditError::TooLarge);
    }
    if positions
        .iter()
        .any(|value| !value.is_finite() || value.abs() > MAX_COORDINATE)
    {
        return Err(EditError::InvalidValue);
    }
    if indices.iter().any(|index| *index as usize >= vertices) {
        return Err(EditError::InvalidMesh);
    }
    if entity_span(source, express_id).is_none() {
        return Err(EditError::MissingElement);
    }
    let tag_list = unset_tag_list(source)?;
    let metres_per_unit = metres_per_unit(source)?;
    let context = first_of(source, IfcType::IfcGeometricRepresentationContext);
    let context_token = context
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "$".to_string());
    let mut next = max_id(source) + 1;
    let mut block = String::new();
    let point_list = next;
    next += 1;
    let coords = positions
        .as_chunks::<3>()
        .0
        .iter()
        .map(|point| {
            format!(
                "({},{},{})",
                step_number(point[0] / metres_per_unit),
                step_number(point[1] / metres_per_unit),
                step_number(point[2] / metres_per_unit)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    block.push_str(&format!(
        "#{point_list}=IFCCARTESIANPOINTLIST3D(({coords}){tag_list});\n"
    ));
    let mut faces = Vec::new();
    for triangle in indices.as_chunks::<3>().0 {
        let face_id = next;
        next += 1;
        faces.push(face_id);
        block.push_str(&format!(
            "#{face_id}=IFCINDEXEDPOLYGONALFACE(({},{},{}));\n",
            triangle[0] + 1,
            triangle[1] + 1,
            triangle[2] + 1
        ));
    }
    let face_set = next;
    next += 1;
    block.push_str(&format!(
        "#{face_set}=IFCPOLYGONALFACESET(#{point_list},.T.,({}),$);\n",
        faces
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let shape = next;
    next += 1;
    block.push_str(&format!(
        "#{shape}=IFCSHAPEREPRESENTATION({context_token},'Body','Tessellation',(#{face_set}));\n"
    ));
    let definition = next;
    block.push_str(&format!(
        "#{definition}=IFCPRODUCTDEFINITIONSHAPE($,$,(#{shape}));\n"
    ));
    let with_geometry = append_entities(source, block.as_bytes())?;
    replace_attribute(&with_geometry, express_id, 6, &format!("#{definition}"))
}

fn unset_tag_list(source: &[u8]) -> Result<&'static str, EditError> {
    match first_declared_schema(source, SchemaVersion::from_file_schema) {
        // IfcCartesianPointList3D gained its optional TagList in IFC4X1
        Some(SchemaVersion::Ifc4) => Ok(""),
        Some(SchemaVersion::Ifc4x1 | SchemaVersion::Ifc4x2 | SchemaVersion::Ifc4x3) => Ok(",$"),
        _ => Err(EditError::UnsupportedSchema),
    }
}

pub fn change_relation(
    source: &[u8],
    relation_id: u32,
    element_id: u32,
    add: bool,
) -> Result<Vec<u8>, EditError> {
    let record = decode(source, relation_id)?;
    let slot = match record.ifc_type {
        IfcType::IfcRelContainedInSpatialStructure => 4,
        IfcType::IfcRelAggregates | IfcType::IfcRelNests => 5,
        _ => return Err(EditError::NotARelation),
    };
    if add {
        if is_related_by(source, record.ifc_type, slot, element_id) {
            return Err(EditError::AlreadyRelated);
        }
        insert_ref(source, relation_id, slot, element_id)
    } else {
        remove_ref(source, relation_id, slot, element_id)
    }
}

fn is_related_by(source: &[u8], relation_type: IfcType, slot: usize, element_id: u32) -> bool {
    let mut scanner = EntityScanner::new(source);
    let mut decoder = EntityDecoder::new(source);
    while let Some((_, keyword, start, end)) = scanner.next_entity() {
        if ifc_type_from_keyword(keyword) != relation_type {
            continue;
        }
        let Ok(relation) = decoder.decode_at(start, end) else {
            continue;
        };
        let related = relation.get_list(slot).unwrap_or_default();
        if related
            .iter()
            .any(|item| item.as_entity_ref() == Some(element_id))
        {
            return true;
        }
    }
    false
}

fn metres_per_unit(source: &[u8]) -> Result<f64, EditError> {
    let mut decoder = EntityDecoder::new(source);
    // ifc-lite meshes a file in metres when it has no IfcProject or its unit assignment does not decode
    let Some(project) = decoder.project_id() else {
        return Ok(1.0);
    };
    match ifc_lite_core::resolve_length_unit(&mut decoder, project) {
        Ok(unit) => unit
            .for_geometry()
            .map_err(|_| EditError::UnusableLengthUnit),
        Err(_) => Ok(1.0),
    }
}

pub fn mesh_volume(positions: &[[f64; 3]], indices: &[u32]) -> Result<f64, EditError> {
    let (triangles, remainder) = indices.as_chunks::<3>();
    if !remainder.is_empty() {
        return Err(EditError::InvalidMesh);
    }
    let mut sum = 0.0;
    for triangle in triangles {
        let corner = |index: u32| positions.get(index as usize).ok_or(EditError::InvalidMesh);
        let (a, b, c) = (
            corner(triangle[0])?,
            corner(triangle[1])?,
            corner(triangle[2])?,
        );
        sum += a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
            + a[2] * (b[0] * c[1] - b[1] * c[0]);
    }
    Ok((sum / 6.0).abs())
}

fn finite_vector(value: [f64; 3]) -> Result<(), EditError> {
    if value
        .iter()
        .any(|axis| !axis.is_finite() || axis.abs() > MAX_COORDINATE)
    {
        Err(EditError::InvalidValue)
    } else {
        Ok(())
    }
}

fn cartesian_point(source: &[u8], id: u32) -> Result<[f64; 3], EditError> {
    let mut decoder = EntityDecoder::new(source);
    let (x, y, z) = decoder
        .get_cartesian_point_fast(id)
        .ok_or(EditError::NotAPlacement)?;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Err(EditError::NotAPlacement);
    }
    Ok([x, y, z])
}

fn direction_ratios(source: &[u8], id: u32) -> Result<(f64, f64, f64), EditError> {
    let direction = decode(source, id)?;
    if direction.ifc_type != IfcType::IfcDirection {
        return Err(EditError::NotAPlacement);
    }
    let list = direction.get_list(0).ok_or(EditError::NotAPlacement)?;
    let component = |index: usize| -> Result<f64, EditError> {
        list.get(index)
            .and_then(ifc_lite_core::AttributeValue::as_float)
            .filter(|value| value.is_finite())
            .ok_or(EditError::NotAPlacement)
    };
    let x = component(0)?;
    let y = component(1)?;
    let z = list
        .get(2)
        .and_then(ifc_lite_core::AttributeValue::as_float)
        .unwrap_or(0.0);
    Ok((x, y, z))
}

fn decode(source: &[u8], id: u32) -> Result<DecodedEntity, EditError> {
    let span = entity_span(source, id).ok_or(EditError::MissingElement)?;
    let mut decoder = EntityDecoder::new(source);
    decoder
        .decode_at(span.start, span.end)
        .map_err(|_| EditError::MissingElement)
}

pub(crate) fn entity_span(source: &[u8], id: u32) -> Option<std::ops::Range<usize>> {
    let mut scanner = EntityScanner::new(source);
    while let Some((found, _, start, end)) = scanner.next_entity() {
        if found == id {
            return Some(whole_entity(source, start, end));
        }
    }
    None
}

fn whole_entity(source: &[u8], start: usize, mut end: usize) -> std::ops::Range<usize> {
    if (end == 0 || source.get(end - 1) != Some(&b';'))
        && let Some(semi) = source[end..].iter().position(|byte| *byte == b';')
    {
        end += semi + 1;
    }
    while matches!(source.get(end), Some(b'\n' | b'\r')) {
        end += 1;
    }
    start..end
}

pub(crate) fn max_id(source: &[u8]) -> u32 {
    let mut scanner = EntityScanner::new(source);
    let mut max = 0;
    while let Some((id, _, _, _)) = scanner.next_entity() {
        max = max.max(id);
    }
    max
}

fn first_of(source: &[u8], wanted: IfcType) -> Option<u32> {
    let mut scanner = EntityScanner::new(source);
    while let Some((id, keyword, _, _)) = scanner.next_entity() {
        if ifc_type_from_keyword(keyword) == wanted {
            return Some(id);
        }
    }
    None
}

fn rewrite_coordinates(
    source: &[u8],
    entity_id: u32,
    coordinates: &[f64],
) -> Result<Vec<u8>, EditError> {
    let span = entity_span(source, entity_id).ok_or(EditError::MissingElement)?;
    let entity = &source[span.start..span.end];
    let inner = coordinate_list_inner(entity).ok_or(EditError::NotAPlacement)?;
    let mut next = Vec::with_capacity(source.len() + 32);
    next.extend_from_slice(&source[..span.start + inner.start]);
    next.extend_from_slice(format_coordinates(coordinates).as_bytes());
    next.extend_from_slice(&source[span.start + inner.end..]);
    Ok(next)
}

fn coordinate_list_inner(entity: &[u8]) -> Option<std::ops::Range<usize>> {
    let open = entity.iter().position(|byte| *byte == b'(')?;
    let mut depth = 0usize;
    let mut list_open = None;
    let mut index = open;
    while index < entity.len() {
        match entity[index] {
            b'(' => {
                depth += 1;
                if depth == 2 && list_open.is_none() {
                    list_open = Some(index + 1);
                }
            }
            b')' => {
                if depth == 2
                    && let Some(start) = list_open
                {
                    return Some(start..index);
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn format_coordinates(coordinates: &[f64]) -> String {
    coordinates
        .iter()
        .map(|value| step_number(*value))
        .collect::<Vec<_>>()
        .join(",")
}

fn step_number(value: f64) -> String {
    let text = format!("{value:.8}");
    let trimmed = text.trim_end_matches('0');
    if trimmed.ends_with('.') {
        format!("{trimmed}0")
    } else {
        trimmed.to_string()
    }
}

fn append_entities(source: &[u8], block: &[u8]) -> Result<Vec<u8>, EditError> {
    let marker = b"ENDSEC;";
    let end = source
        .windows(marker.len())
        .rposition(|window| window == marker)
        .ok_or(EditError::NoStructure)?;
    let mut next = Vec::with_capacity(source.len() + block.len());
    next.extend_from_slice(&source[..end]);
    if !next.ends_with(b"\n") {
        next.push(b'\n');
    }
    next.extend_from_slice(block);
    next.extend_from_slice(&source[end..]);
    Ok(next)
}

fn attach_containment(source: &[u8], proxy: u32, owner: &str) -> Result<Vec<u8>, EditError> {
    if let Some(relation) = first_of(source, IfcType::IfcRelContainedInSpatialStructure) {
        return insert_ref(source, relation, 4, proxy);
    }
    let structure = first_of(source, IfcType::IfcBuilding)
        .or_else(|| first_of(source, IfcType::IfcSite))
        .ok_or(EditError::NoStructure)?;
    let relation = max_id(source) + 1;
    let guid = ifc_guid();
    let block = format!(
        "#{relation}=IFCRELCONTAINEDINSPATIALSTRUCTURE('{guid}',{owner},$,$,(#{proxy}),#{structure});\n"
    );
    append_entities(source, block.as_bytes())
}

fn replace_attribute(
    source: &[u8],
    entity_id: u32,
    index: usize,
    replacement: &str,
) -> Result<Vec<u8>, EditError> {
    let span = entity_span(source, entity_id).ok_or(EditError::MissingElement)?;
    let argument =
        argument_span(&source[span.start..span.end], index).ok_or(EditError::NotAPlacement)?;
    let mut next = Vec::with_capacity(source.len() + replacement.len());
    next.extend_from_slice(&source[..span.start + argument.start]);
    next.extend_from_slice(replacement.as_bytes());
    next.extend_from_slice(&source[span.start + argument.end..]);
    Ok(next)
}

fn insert_ref(
    source: &[u8],
    entity_id: u32,
    index: usize,
    reference: u32,
) -> Result<Vec<u8>, EditError> {
    let span = entity_span(source, entity_id).ok_or(EditError::MissingElement)?;
    let argument =
        argument_span(&source[span.start..span.end], index).ok_or(EditError::NotARelation)?;
    let list = &source[span.start + argument.start..span.start + argument.end];
    if !list.starts_with(b"(") || !list.ends_with(b")") {
        return Err(EditError::NotARelation);
    }
    let empty = list
        .iter()
        .all(|byte| byte.is_ascii_whitespace() || *byte == b'(' || *byte == b')');
    let token = if empty {
        format!("#{reference}")
    } else {
        format!(",#{reference}")
    };
    let mut next = Vec::with_capacity(source.len() + token.len());
    next.extend_from_slice(&source[..span.start + argument.end - 1]);
    next.extend_from_slice(token.as_bytes());
    next.extend_from_slice(&source[span.start + argument.end - 1..]);
    Ok(next)
}

fn remove_ref(
    source: &[u8],
    entity_id: u32,
    index: usize,
    reference: u32,
) -> Result<Vec<u8>, EditError> {
    let span = entity_span(source, entity_id).ok_or(EditError::MissingElement)?;
    let argument =
        argument_span(&source[span.start..span.end], index).ok_or(EditError::NotARelation)?;
    let list = &source[span.start + argument.start..span.start + argument.end];
    let stripped = strip_one_ref(list, reference).ok_or(EditError::MissingElement)?;
    let mut next = Vec::with_capacity(source.len());
    next.extend_from_slice(&source[..span.start + argument.start]);
    next.extend_from_slice(&stripped);
    next.extend_from_slice(&source[span.start + argument.end..]);
    Ok(next)
}

pub(crate) fn argument_span(entity: &[u8], index: usize) -> Option<std::ops::Range<usize>> {
    let mut cursor = 0usize;
    while cursor < entity.len() && entity[cursor] != b'(' {
        cursor += 1;
    }
    if cursor >= entity.len() {
        return None;
    }
    cursor += 1;
    let mut depth = 1usize;
    let mut argument = 0usize;
    let mut start = cursor;
    let mut in_string = false;
    while cursor < entity.len() {
        let byte = entity[cursor];
        if in_string {
            if byte == b'\'' {
                if entity.get(cursor + 1) == Some(&b'\'') {
                    cursor += 2;
                    continue;
                }
                in_string = false;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'\'' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    if argument == index {
                        return Some(start..cursor);
                    }
                    return None;
                }
            }
            b',' if depth == 1 => {
                if argument == index {
                    return Some(start..cursor);
                }
                argument += 1;
                start = cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn reference_depths(entity: &[u8], id: u32) -> Vec<usize> {
    let Some(open) = entity.iter().position(|byte| *byte == b'(') else {
        return Vec::new();
    };
    let mut depths = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut cursor = open;
    while cursor < entity.len() {
        let byte = entity[cursor];
        if in_string {
            if byte == b'\'' {
                if entity.get(cursor + 1) == Some(&b'\'') {
                    cursor += 2;
                    continue;
                }
                in_string = false;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'\'' => in_string = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'#' if exact_ref_at(entity, cursor, id) => depths.push(depth),
            _ => {}
        }
        cursor += 1;
    }
    depths
}

struct StrippedEntity {
    bytes: Vec<u8>,
    emptied_a_list: bool,
}

fn strip_list_references(entity: &[u8], id: u32) -> StrippedEntity {
    let mut next: Vec<u8> = Vec::with_capacity(entity.len());
    let mut lists: Vec<(usize, bool)> = Vec::new();
    let mut emptied_a_list = false;
    let mut in_string = false;
    let mut cursor = 0usize;
    while cursor < entity.len() {
        let byte = entity[cursor];
        if in_string {
            next.push(byte);
            if byte == b'\'' {
                if entity.get(cursor + 1) == Some(&b'\'') {
                    next.push(b'\'');
                    cursor += 2;
                    continue;
                }
                in_string = false;
            }
            cursor += 1;
            continue;
        }
        match byte {
            b'\'' => in_string = true,
            b'(' => lists.push((next.len() + 1, false)),
            b')' => {
                if let Some((members, touched)) = lists.pop() {
                    emptied_a_list |=
                        touched && next[members..].iter().all(u8::is_ascii_whitespace);
                }
            }
            b'#' if exact_ref_at(entity, cursor, id) => {
                if let Some(list) = lists.last_mut() {
                    list.1 = true;
                }
                cursor = drop_reference(entity, cursor, id, &mut next);
                continue;
            }
            _ => {}
        }
        next.push(byte);
        cursor += 1;
    }
    StrippedEntity {
        bytes: next,
        emptied_a_list,
    }
}

fn drop_reference(source: &[u8], cursor: usize, id: u32, next: &mut Vec<u8>) -> usize {
    let token_end = cursor + 1 + id.to_string().len();
    if next.iter().rev().find(|byte| !byte.is_ascii_whitespace()) == Some(&b',') {
        while next.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
            next.pop();
        }
        next.pop();
        return token_end;
    }
    let mut look = token_end;
    while source
        .get(look)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        look += 1;
    }
    if source.get(look) == Some(&b',') {
        return look + 1;
    }
    token_end
}

pub(crate) fn exact_ref_at(source: &[u8], start: usize, id: u32) -> bool {
    let digits = id.to_string();
    let bytes = digits.as_bytes();
    let end = start + 1 + bytes.len();
    source.get(start + 1..end) == Some(bytes)
        && !source.get(end).is_some_and(|byte| byte.is_ascii_digit())
}

pub(crate) fn strip_one_ref(list: &[u8], id: u32) -> Option<Vec<u8>> {
    let mut found = false;
    let mut next: Vec<u8> = Vec::with_capacity(list.len());
    let mut cursor = 0usize;
    while cursor < list.len() {
        if list[cursor] == b'#' && exact_ref_at(list, cursor, id) {
            found = true;
            cursor = drop_reference(list, cursor, id, &mut next);
            continue;
        }
        next.push(list[cursor]);
        cursor += 1;
    }
    found.then_some(next)
}

const IFC_BASE64: &[u8; 64] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";

fn ifc_guid() -> String {
    let value = uuid::Uuid::new_v4().as_u128();
    // IFC GlobalIds have two bits in the first character and six in each of the other 21
    (0..22)
        .map(|place| IFC_BASE64[((value >> (6 * (21 - place))) & 63) as usize] as char)
        .collect()
}

const BOX_FACES: [[u32; 4]; 6] = [
    [1, 4, 3, 2],
    [5, 6, 7, 8],
    [1, 2, 6, 5],
    [4, 8, 7, 3],
    [1, 5, 8, 4],
    [2, 3, 7, 6],
];

fn box_mesh(size: [f64; 3]) -> (Vec<[f64; 3]>, Vec<u32>) {
    let [x, y, z] = size;
    let corners = [
        [0.0, 0.0, 0.0],
        [x, 0.0, 0.0],
        [x, y, 0.0],
        [0.0, y, 0.0],
        [0.0, 0.0, z],
        [x, 0.0, z],
        [x, y, z],
        [0.0, y, z],
    ];
    let mut indices = Vec::new();
    for face in BOX_FACES {
        indices.extend([
            face[0] - 1,
            face[1] - 1,
            face[2] - 1,
            face[0] - 1,
            face[2] - 1,
            face[3] - 1,
        ]);
    }
    (corners.to_vec(), indices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(data: &str) -> String {
        file_in("IFC4", data)
    }

    fn file_in(schema: &str, data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\nFILE_NAME('','',(''),(''),'','','');\nFILE_SCHEMA(('{schema}'));\nENDSEC;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn sample() -> String {
        sample_in("IFC4")
    }

    fn sample_in(schema: &str) -> String {
        file_in(
            schema,
            "#1=IFCOWNERHISTORY($,$,$,$,$,$,$,$);\n\
             #2=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-05,#3,$);\n\
             #3=IFCAXIS2PLACEMENT3D(#4,$,$);\n\
             #4=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #5=IFCBUILDING('buildinggid',#1,'Building',$,$,#6,$,$,.ELEMENT.,$,$,$);\n\
             #6=IFCLOCALPLACEMENT($,#3);\n\
             #10=IFCWALL('wallgid',#1,'Wall',$,$,#11,$,$,$);\n\
             #11=IFCLOCALPLACEMENT($,#12);\n\
             #12=IFCAXIS2PLACEMENT3D(#13,$,$);\n\
             #13=IFCCARTESIANPOINT((1.,2.,3.));\n\
             #20=IFCRELCONTAINEDINSPATIALSTRUCTURE('relgid',#1,$,$,(#10,#100),#5);\n\
             #30=IFCRELAGGREGATES('agg',#1,$,$,#5,(#10));\n\
             #41=IFCAXIS2PLACEMENT3D(#100,$,$);\n\
             #100=IFCCARTESIANPOINT((4.,5.,6.));\n",
        )
    }

    fn point_text(source: &str, id: u32) -> String {
        let span = entity_span(source.as_bytes(), id).unwrap();
        String::from_utf8(source.as_bytes()[span].to_vec()).unwrap()
    }

    #[test]
    fn a_move_adds_to_the_placement_point() {
        let moved = translate_placement(sample().as_bytes(), 10, [10.0, 0.0, -1.0]).unwrap();
        let point = point_text(std::str::from_utf8(&moved).unwrap(), 13);
        assert!(point.contains("11.0,2.0,2.0"), "{point}");
    }

    #[test]
    fn a_yaw_writes_a_ref_direction() {
        let turned = yaw_placement(sample().as_bytes(), 10, std::f64::consts::FRAC_PI_2).unwrap();
        let text = String::from_utf8(turned).unwrap();
        assert!(text.contains("IFCDIRECTION"), "{text}");
        let axis = point_text(&text, 12);
        assert!(!axis.contains("$,$"), "{axis}");
    }

    #[test]
    fn deleting_an_element_drops_its_refs_and_leaves_a_longer_id() {
        let deleted = delete_product(sample().as_bytes(), 10).unwrap();
        let text = String::from_utf8(deleted).unwrap();
        assert!(!text.contains("wallgid"));
        assert!(text.contains("#100"));
        assert!(text.contains("#20="));
        assert!(!text.contains("#10="));
        let containment = point_text(&text, 20);
        assert!(containment.contains("#100"), "{containment}");
        assert!(
            strip_one_ref(containment.as_bytes(), 10).is_none(),
            "{containment}"
        );
        assert!(entity_span(text.as_bytes(), 30).is_none(), "{text}");
    }

    fn located_point(source: &[u8], product: u32) -> [f64; 3] {
        let local = decode(source, product).unwrap().get_ref(5).unwrap();
        let axis = decode(source, local).unwrap().get_ref(1).unwrap();
        let point = decode(source, axis).unwrap().get_ref(0).unwrap();
        cartesian_point(source, point).unwrap()
    }

    fn reference_direction(source: &[u8], product: u32) -> (f64, f64, f64) {
        let local = decode(source, product).unwrap().get_ref(5).unwrap();
        let axis = decode(source, local).unwrap().get_ref(1).unwrap();
        let direction = decode(source, axis).unwrap().get_ref(2).unwrap();
        direction_ratios(source, direction).unwrap()
    }

    fn two_walls_sharing_a_point_and_a_direction() -> String {
        file(
            "#1=IFCOWNERHISTORY($,$,$,$,$,$,$,$);\n\
             #4=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #5=IFCDIRECTION((1.,0.,0.));\n\
             #10=IFCWALL('a',#1,'A',$,$,#11,$,$,$);\n\
             #11=IFCLOCALPLACEMENT($,#12);\n\
             #12=IFCAXIS2PLACEMENT3D(#4,$,#5);\n\
             #20=IFCWALL('b',#1,'B',$,$,#21,$,$,$);\n\
             #21=IFCLOCALPLACEMENT($,#22);\n\
             #22=IFCAXIS2PLACEMENT3D(#4,$,#5);\n",
        )
    }

    #[test]
    fn moving_one_wall_leaves_a_wall_that_shares_its_point() {
        let source = two_walls_sharing_a_point_and_a_direction();
        let moved = translate_placement(source.as_bytes(), 10, [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(located_point(&moved, 10), [1.0, 0.0, 0.0]);
        assert_eq!(located_point(&moved, 20), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn turning_one_wall_leaves_a_wall_that_shares_its_direction() {
        let source = two_walls_sharing_a_point_and_a_direction();
        let turned = yaw_placement(source.as_bytes(), 10, std::f64::consts::FRAC_PI_2).unwrap();
        let (x, y, _) = reference_direction(&turned, 10);
        assert!(x.abs() < 1.0e-9 && (y - 1.0).abs() < 1.0e-9, "{x} {y}");
        assert_eq!(reference_direction(&turned, 20), (1.0, 0.0, 0.0));
    }

    fn two_walls_sharing_one_local_placement_each_with_an_opening() -> String {
        file(
            "#1=IFCOWNERHISTORY($,$,$,$,$,$,$,$);\n\
             #4=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #10=IFCWALL('a',#1,'A',$,$,#11,$,$,$);\n\
             #11=IFCLOCALPLACEMENT($,#12);\n\
             #12=IFCAXIS2PLACEMENT3D(#4,$,$);\n\
             #20=IFCWALL('b',#1,'B',$,$,#11,$,$,$);\n\
             #30=IFCOPENINGELEMENT('oa',#1,$,$,$,#31,$,$,$);\n\
             #31=IFCLOCALPLACEMENT(#11,#32);\n\
             #32=IFCAXIS2PLACEMENT3D(#4,$,$);\n\
             #40=IFCOPENINGELEMENT('ob',#1,$,$,$,#41,$,$,$);\n\
             #41=IFCLOCALPLACEMENT(#11,#42);\n\
             #42=IFCAXIS2PLACEMENT3D(#4,$,$);\n\
             #50=IFCRELVOIDSELEMENT('va',#1,$,$,#10,#30);\n\
             #51=IFCRELVOIDSELEMENT('vb',#1,$,$,#20,#40);\n",
        )
    }

    #[test]
    fn moving_one_of_two_walls_on_one_local_placement_takes_only_its_own_opening() {
        let source = two_walls_sharing_one_local_placement_each_with_an_opening();
        let moved = translate_placement(source.as_bytes(), 10, [1.0, 0.0, 0.0]).unwrap();
        assert_eq!(located_point(&moved, 10), [1.0, 0.0, 0.0]);
        assert_eq!(located_point(&moved, 20), [0.0, 0.0, 0.0]);
        let placement_of = |product: u32| decode(&moved, product).unwrap().get_ref(5).unwrap();
        let relative_to = |product: u32| decode(&moved, placement_of(product)).unwrap().get_ref(0);
        assert_ne!(placement_of(10), 11);
        assert_eq!(placement_of(20), 11);
        assert_eq!(relative_to(30), Some(placement_of(10)));
        assert_eq!(relative_to(40), Some(11));
    }

    #[test]
    fn moving_the_building_leaves_the_context_that_shares_its_axis() {
        let moved = translate_placement(sample().as_bytes(), 5, [0.0, 0.0, 5.0]).unwrap();
        assert_eq!(located_point(&moved, 5), [0.0, 0.0, 5.0]);
        let context = decode(&moved, 2).unwrap();
        let world = context.get_ref(4).unwrap();
        let origin = decode(&moved, world).unwrap().get_ref(0).unwrap();
        assert_eq!(cartesian_point(&moved, origin).unwrap(), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn deleting_a_wall_drops_the_relation_that_names_it_alone_and_skips_quoted_text() {
        let source = file(
            "#1=IFCOWNERHISTORY($,$,$,$,$,$,$,$);\n\
             #10=IFCWALL('wallgid',#1,'Wall',$,$,$,$,$,$);\n\
             #11=IFCOPENINGELEMENT('openinggid',#1,'Opening',$,$,$,$,$,$);\n\
             #12=IFCRELVOIDSELEMENT('voidgid',#1,$,$,#10,#11);\n\
             #13=IFCDOOR('doorgid',#1,'Door #10',$,$,$,$,$,$,$,$,$,$);\n",
        );
        let deleted = String::from_utf8(delete_product(source.as_bytes(), 10).unwrap()).unwrap();
        assert!(entity_span(deleted.as_bytes(), 10).is_none(), "{deleted}");
        assert!(entity_span(deleted.as_bytes(), 12).is_none(), "{deleted}");
        assert!(point_text(&deleted, 11).contains("'Opening'"), "{deleted}");
        assert!(point_text(&deleted, 13).contains("'Door #10'"), "{deleted}");
    }

    #[test]
    fn an_element_another_entity_needs_is_not_deleted() {
        let source = file(
            "#10=IFCWALL('wallgid',$,'Wall',$,$,$,$,$,$);\n\
             #11=IFCPRESENTATIONLAYERASSIGNMENT('Layer',$,(#10),$);\n",
        );
        assert_eq!(
            delete_product(source.as_bytes(), 10).unwrap_err(),
            EditError::StillReferenced
        );
    }

    #[test]
    fn adding_a_proxy_puts_it_in_the_containment() {
        let added = add_proxy(sample().as_bytes(), [5.0, 0.0, 0.0], [1.0, 2.0, 3.0]).unwrap();
        let text = String::from_utf8(added.bytes).unwrap();
        assert!(text.contains(&added.global_id));
        assert!(text.contains("IFCBUILDINGELEMENTPROXY"));
        let containment = point_text(&text, 20);
        assert!(
            containment.contains(&format!("#{}", added.express_id)),
            "{containment}"
        );
        assert!((mesh_volume(&added.positions, &added.indices).unwrap() - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn replacing_the_mesh_points_the_product_at_the_new_shape() {
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        let indices = [0, 1, 2];
        let replaced = replace_mesh(sample().as_bytes(), 10, &positions, &indices).unwrap();
        let definition = decode(&replaced, 10).unwrap().get_ref(6).unwrap();
        let definition = decode(&replaced, definition).unwrap();
        assert_eq!(definition.ifc_type, IfcType::IfcProductDefinitionShape);
        let shape = decode(&replaced, definition.get_refs(2).unwrap()[0]).unwrap();
        assert_eq!(shape.ifc_type, IfcType::IfcShapeRepresentation);
        assert_eq!(shape.get_ref(0), Some(2));
        let face_set = decode(&replaced, shape.get_refs(3).unwrap()[0]).unwrap();
        assert_eq!(face_set.ifc_type, IfcType::IfcPolygonalFaceSet);
        let text = String::from_utf8(replaced.clone()).unwrap();
        let point_list = point_text(&text, face_set.get_ref(0).unwrap());
        assert!(
            point_list.contains("((0.0,0.0,0.0),(1.0,0.0,0.0),(0.0,1.0,0.0))"),
            "{point_list}"
        );
        let faces = face_set.get_refs(2).unwrap();
        assert_eq!(faces.len(), 1);
        assert!(point_text(&text, faces[0]).contains("((1,2,3))"), "{text}");
    }

    fn argument_count(entity: &[u8]) -> usize {
        (0..)
            .take_while(|index| argument_span(entity, *index).is_some())
            .count()
    }

    fn assert_new_entities_have_the_schema_attributes(
        before: &[u8],
        after: &[u8],
        schema: SchemaVersion,
    ) {
        let first_new = max_id(before) + 1;
        let mut scanner = EntityScanner::new(after);
        let mut checked = 0;
        while let Some((id, keyword, start, end)) = scanner.next_entity() {
            if id < first_new {
                continue;
            }
            let names = schema
                .attribute_names(keyword)
                .unwrap_or_else(|| panic!("{schema:?} has no {keyword}"));
            let entity = &after[whole_entity(after, start, end)];
            assert_eq!(
                argument_count(entity),
                names.len(),
                "{schema:?} {}",
                String::from_utf8_lossy(entity)
            );
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn added_and_replaced_geometry_has_the_attributes_of_the_file_schema() {
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        for (label, schema) in [
            ("IFC4", SchemaVersion::Ifc4),
            ("IFC4X3_ADD2", SchemaVersion::Ifc4x3),
        ] {
            let source = sample_in(label);
            let added = add_proxy(source.as_bytes(), [5.0, 0.0, 0.0], [1.0, 2.0, 3.0]).unwrap();
            assert_new_entities_have_the_schema_attributes(source.as_bytes(), &added.bytes, schema);
            let replaced = replace_mesh(source.as_bytes(), 10, &positions, &[0, 1, 2]).unwrap();
            assert_new_entities_have_the_schema_attributes(source.as_bytes(), &replaced, schema);
        }
    }

    #[test]
    fn an_ifc2x3_file_gets_no_tessellated_geometry() {
        let source = sample_in("IFC2X3");
        assert_eq!(
            add_proxy(source.as_bytes(), [5.0, 0.0, 0.0], [1.0, 2.0, 3.0]).unwrap_err(),
            EditError::UnsupportedSchema
        );
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        assert_eq!(
            replace_mesh(source.as_bytes(), 10, &positions, &[0, 1, 2]).unwrap_err(),
            EditError::UnsupportedSchema
        );
    }

    #[test]
    fn added_global_ids_are_random_ifc_guids() {
        let first = add_proxy(sample().as_bytes(), [5.0, 0.0, 0.0], [1.0, 2.0, 3.0]).unwrap();
        let second = add_proxy(sample().as_bytes(), [5.0, 0.0, 0.0], [1.0, 2.0, 3.0]).unwrap();
        assert_ne!(first.global_id, second.global_id);
        for global_id in [&first.global_id, &second.global_id] {
            assert_eq!(global_id.len(), 22, "{global_id}");
            assert!(
                matches!(global_id.as_bytes()[0], b'0'..=b'3'),
                "{global_id}"
            );
            let value = global_id.bytes().fold(0u128, |value, character| {
                let digit = IFC_BASE64
                    .iter()
                    .position(|letter| *letter == character)
                    .unwrap();
                (value << 6) | digit as u128
            });
            assert_eq!(
                uuid::Uuid::from_u128(value).get_version_num(),
                4,
                "{global_id}"
            );
        }
    }

    #[test]
    fn a_relation_gains_and_loses_an_element() {
        let added = change_relation(sample().as_bytes(), 30, 100, true).unwrap();
        let text = String::from_utf8(added).unwrap();
        let aggregate = point_text(&text, 30);
        assert!(aggregate.contains("#100"), "{aggregate}");
        let removed = change_relation(text.as_bytes(), 30, 10, false).unwrap();
        let text = String::from_utf8(removed).unwrap();
        let aggregate = point_text(&text, 30);
        assert!(
            strip_one_ref(aggregate.as_bytes(), 10).is_none(),
            "{aggregate}"
        );
        assert!(aggregate.contains("#100"), "{aggregate}");
    }

    #[test]
    fn a_unit_cube_has_unit_volume() {
        let (positions, indices) = box_mesh([1.0, 1.0, 1.0]);
        assert!((mesh_volume(&positions, &indices).unwrap() - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn volume_stays_positive_away_from_the_origin_and_with_reversed_faces() {
        let (mut positions, mut indices) = box_mesh([1.0, 2.0, 3.0]);
        for point in &mut positions {
            point[0] += 100.0;
            point[1] += 50.0;
        }
        for triangle in indices.as_mut_slice().as_chunks_mut::<3>().0 {
            triangle.swap(0, 1);
        }
        assert!((mesh_volume(&positions, &indices).unwrap() - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn a_volume_refuses_an_index_past_the_vertices_and_a_partial_triangle() {
        let (positions, mut indices) = box_mesh([1.0, 2.0, 3.0]);
        indices.push(0);
        assert_eq!(
            mesh_volume(&positions, &indices).unwrap_err(),
            EditError::InvalidMesh
        );
        indices.extend([1, 99]);
        assert_eq!(
            mesh_volume(&positions, &indices).unwrap_err(),
            EditError::InvalidMesh
        );
    }

    #[test]
    fn a_missing_element_and_a_bare_point_cannot_be_moved() {
        let source = sample();
        assert_eq!(
            translate_placement(source.as_bytes(), 999, [1.0, 0.0, 0.0]).unwrap_err(),
            EditError::MissingElement
        );
        assert_eq!(
            translate_placement(source.as_bytes(), 13, [1.0, 0.0, 0.0]).unwrap_err(),
            EditError::NotAPlacement
        );
        assert_eq!(
            translate_placement(source.as_bytes(), 10, [f64::NAN, 0.0, 0.0]).unwrap_err(),
            EditError::InvalidValue
        );
        assert_eq!(
            yaw_placement(source.as_bytes(), 10, f64::INFINITY).unwrap_err(),
            EditError::InvalidValue
        );
    }

    #[test]
    fn a_second_quarter_turn_points_the_ref_direction_backward() {
        let once = yaw_placement(sample().as_bytes(), 10, std::f64::consts::FRAC_PI_2).unwrap();
        let twice = yaw_placement(&once, 10, std::f64::consts::FRAC_PI_2).unwrap();
        let text = String::from_utf8(twice).unwrap();
        let direction = text
            .lines()
            .find(|line| line.contains("IFCDIRECTION"))
            .unwrap();
        assert!(direction.contains("-1.0"), "{direction}");
        assert!(point_text(&text, 13).contains("(1.,2.,3.)"));
    }

    #[test]
    fn deleting_a_longer_id_leaves_the_shorter_one() {
        let source = file(
            "#10=IFCWALL('a',$,'A',$,$,$,$,$,$);\n\
             #100=IFCWALL('b',$,'B',$,$,$,$,$,$);\n\
             #20=IFCRELCONTAINEDINSPATIALSTRUCTURE('relgid',$,$,$,(#10,#100),#5);\n",
        );
        let deleted = delete_product(source.as_bytes(), 100).unwrap();
        let text = String::from_utf8(deleted).unwrap();
        assert!(text.contains("#10="));
        assert!(!text.contains("#100="));
        assert!(point_text(&text, 20).contains("(#10)"), "{text}");
        assert_eq!(
            delete_product(sample().as_bytes(), 100).unwrap_err(),
            EditError::StillReferenced
        );
        assert_eq!(
            delete_product(sample().as_bytes(), 999).unwrap_err(),
            EditError::MissingElement
        );
    }

    #[test]
    fn a_proxy_needs_a_positive_size_and_a_building_or_site() {
        assert_eq!(
            add_proxy(sample().as_bytes(), [0.0, 0.0, 0.0], [0.0, 1.0, 1.0]).unwrap_err(),
            EditError::InvalidValue
        );
        assert_eq!(
            add_proxy(sample().as_bytes(), [0.0, 0.0, 0.0], [1_001.0, 1.0, 1.0]).unwrap_err(),
            EditError::InvalidValue
        );
        let bare = file("#1=IFCWALL('wallgid',$,'Wall',$,$,$,$,$,$);\n");
        assert_eq!(
            add_proxy(bare.as_bytes(), [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]).unwrap_err(),
            EditError::NoStructure
        );
    }

    #[test]
    fn a_replacement_mesh_rejects_a_bad_index_an_empty_list_and_too_many_vertices() {
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        assert_eq!(
            replace_mesh(sample().as_bytes(), 10, &positions, &[0, 1, 3]).unwrap_err(),
            EditError::InvalidMesh
        );
        assert_eq!(
            replace_mesh(sample().as_bytes(), 10, &positions, &[]).unwrap_err(),
            EditError::InvalidMesh
        );
        let mut infinite = positions;
        infinite[2] = f64::NAN;
        assert_eq!(
            replace_mesh(sample().as_bytes(), 10, &infinite, &[0, 1, 2]).unwrap_err(),
            EditError::InvalidValue
        );
        infinite[2] = MAX_COORDINATE + 1.0;
        assert_eq!(
            replace_mesh(sample().as_bytes(), 10, &infinite, &[0, 1, 2]).unwrap_err(),
            EditError::InvalidValue
        );
        let huge = vec![0.0; (MAX_MESH_VERTICES + 1) * 3];
        assert_eq!(
            replace_mesh(sample().as_bytes(), 10, &huge, &[0, 1, 2]).unwrap_err(),
            EditError::TooLarge
        );
        assert_eq!(
            replace_mesh(sample().as_bytes(), 999, &positions, &[0, 1, 2]).unwrap_err(),
            EditError::MissingElement
        );
    }

    fn millimetre_sample() -> String {
        file(
            "#1=IFCOWNERHISTORY($,$,$,$,$,$,$,$);\n\
             #2=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);\n\
             #3=IFCUNITASSIGNMENT((#2));\n\
             #4=IFCPROJECT('projectgid',#1,'Project',$,$,$,$,$,#3);\n\
             #5=IFCBUILDING('buildinggid',#1,'Building',$,$,$,$,$,.ELEMENT.,$,$,$);\n\
             #10=IFCWALL('wallgid',#1,'Wall',$,$,#11,$,$,$);\n\
             #11=IFCLOCALPLACEMENT($,#12);\n\
             #12=IFCAXIS2PLACEMENT3D(#13,$,$);\n\
             #13=IFCCARTESIANPOINT((1000.,2000.,3000.));\n\
             #20=IFCRELCONTAINEDINSPATIALSTRUCTURE('relgid',#1,$,$,(#10),#5);\n",
        )
    }

    #[test]
    fn edits_take_metres_and_write_the_project_length_unit() {
        let source = millimetre_sample();
        let moved = translate_placement(source.as_bytes(), 10, [1.0, 0.5, 0.0]).unwrap();
        assert_eq!(located_point(&moved, 10), [2000.0, 2500.0, 3000.0]);

        let added = add_proxy(source.as_bytes(), [1.0, 2.0, 0.0], [2.0, 1.0, 3.0]).unwrap();
        assert!((mesh_volume(&added.positions, &added.indices).unwrap() - 6.0).abs() < 1.0e-6);
        let placement = decode(&added.bytes, added.express_id)
            .unwrap()
            .get_ref(5)
            .unwrap();
        let axis = decode(&added.bytes, placement).unwrap().get_ref(1).unwrap();
        let origin = decode(&added.bytes, axis).unwrap().get_ref(0).unwrap();
        assert_eq!(
            cartesian_point(&added.bytes, origin).unwrap(),
            [1000.0, 2000.0, 0.0]
        );
        let text = String::from_utf8(added.bytes).unwrap();
        assert!(text.contains("(2000.0,1000.0,3000.0)"), "{text}");

        let positions = [0.0, 0.0, 0.0, 1.5, 0.0, 0.0, 0.0, 1.0, 0.0];
        let replaced = replace_mesh(source.as_bytes(), 10, &positions, &[0, 1, 2]).unwrap();
        let text = String::from_utf8(replaced).unwrap();
        assert!(
            text.contains(
                "IFCCARTESIANPOINTLIST3D(((0.0,0.0,0.0),(1500.0,0.0,0.0),(0.0,1000.0,0.0)))"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_element_already_in_a_relation_of_that_kind_is_not_added_to_another() {
        let source = file(
            "#5=IFCBUILDING('buildinggid',$,'Building',$,$,$,$,$,.ELEMENT.,$,$,$);\n\
             #6=IFCBUILDINGSTOREY('storeygid',$,'Storey',$,$,$,$,$,.ELEMENT.,0.);\n\
             #10=IFCWALL('wallgid',$,'Wall',$,$,$,$,$,$);\n\
             #20=IFCRELCONTAINEDINSPATIALSTRUCTURE('relgid',$,$,$,(#10),#5);\n\
             #21=IFCRELCONTAINEDINSPATIALSTRUCTURE('storeyrelgid',$,$,$,(#11),#6);\n",
        );
        assert_eq!(
            change_relation(source.as_bytes(), 21, 10, true).unwrap_err(),
            EditError::AlreadyRelated
        );
        assert_eq!(
            change_relation(source.as_bytes(), 20, 10, true).unwrap_err(),
            EditError::AlreadyRelated
        );
        let removed = change_relation(source.as_bytes(), 20, 10, false).unwrap();
        let moved = change_relation(&removed, 21, 10, true).unwrap();
        let text = String::from_utf8(moved).unwrap();
        assert!(point_text(&text, 21).contains("(#11,#10)"), "{text}");
    }

    #[test]
    fn a_wall_is_not_a_relation() {
        assert_eq!(
            change_relation(sample().as_bytes(), 10, 100, true).unwrap_err(),
            EditError::NotARelation
        );
    }
}
