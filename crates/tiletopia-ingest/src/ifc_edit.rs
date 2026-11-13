use ifc_lite_core::{DecodedEntity, EntityDecoder, EntityScanner, IfcType, ifc_type_from_keyword};

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
}

#[derive(Debug)]
pub struct PlacementChange {
    pub bytes: Vec<u8>,
    pub pivot: [f64; 3],
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
) -> Result<PlacementChange, EditError> {
    finite_vector(delta)?;
    let (point_id, pivot) = placement_point(source, express_id)?;
    let moved = [
        pivot[0] + delta[0],
        pivot[1] + delta[1],
        pivot[2] + delta[2],
    ];
    Ok(PlacementChange {
        bytes: rewrite_coordinates(source, point_id, &moved)?,
        pivot,
    })
}

pub fn yaw_placement(
    source: &[u8],
    express_id: u32,
    radians: f64,
) -> Result<PlacementChange, EditError> {
    if !radians.is_finite() {
        return Err(EditError::InvalidValue);
    }
    let (axis_id, pivot) = placement_axis(source, express_id)?;
    let axis = decode(source, axis_id)?;
    if axis.ifc_type != IfcType::IfcAxis2Placement3D {
        return Err(EditError::NotAPlacement);
    }
    let (sin, cos) = radians.sin_cos();
    let bytes = if let Some(direction_id) = axis.get_ref(2) {
        let (x, y, z) = direction_ratios(source, direction_id)?;
        let rotated = [x * cos - y * sin, x * sin + y * cos, z];
        rewrite_coordinates(source, direction_id, &rotated)?
    } else {
        let mut next_id = max_id(source) + 1;
        let direction = format!(
            "#{next_id}=IFCDIRECTION(({},{},0.));\n",
            step_number(cos),
            step_number(sin)
        );
        next_id += 1;
        let _ = next_id;
        let direction_id = next_id_before_append(source);
        let with_direction = append_entities(source, &direction)?;
        replace_attribute(&with_direction, axis_id, 2, &format!("#{direction_id}"))?
    };
    Ok(PlacementChange { bytes, pivot })
}

fn next_id_before_append(source: &[u8]) -> u32 {
    max_id(source) + 1
}

pub fn delete_product(source: &[u8], express_id: u32) -> Result<Vec<u8>, EditError> {
    let span = entity_span(source, express_id).ok_or(EditError::MissingElement)?;
    Ok(omit_span_and_ref(source, span, express_id))
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
    let mut next = max_id(source) + 1;
    let owner = first_of(source, IfcType::IfcOwnerHistory);
    let context = first_of(source, IfcType::IfcGeometricRepresentationContext);
    let global_id = ifc_guid(next);
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
        "#{point_list}=IFCCARTESIANPOINTLIST3D(({}));\n",
        local
            .iter()
            .map(|point| format!(
                "({},{},{})",
                step_number(point[0]),
                step_number(point[1]),
                step_number(point[2])
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
        step_number(origin[0]),
        step_number(origin[1]),
        step_number(origin[2])
    ));
    let axis = next;
    next += 1;
    block.push_str(&format!("#{axis}=IFCAXIS2PLACEMENT3D(#{location},$,$);\n"));
    let placement = next;
    next += 1;
    block.push_str(&format!("#{placement}=IFCLOCALPLACEMENT($,#{axis});\n"));
    let proxy = next;
    block.push_str(&format!(
        "#{proxy}=IFCBUILDINGELEMENTPROXY('{global_id}',{owner_token},'Added element',$,$,#{placement},#{definition},$);\n"
    ));
    let mut bytes = append_entities(source, &block)?;
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
                step_number(point[0]),
                step_number(point[1]),
                step_number(point[2])
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    block.push_str(&format!(
        "#{point_list}=IFCCARTESIANPOINTLIST3D(({coords}));\n"
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
    let with_geometry = append_entities(source, &block)?;
    replace_attribute(&with_geometry, express_id, 6, &format!("#{definition}"))
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
        insert_ref(source, relation_id, slot, element_id)
    } else {
        remove_ref(source, relation_id, slot, element_id)
    }
}

pub fn mesh_volume(positions: &[[f64; 3]], indices: &[u32]) -> f64 {
    let mut sum = 0.0;
    for triangle in indices.as_chunks::<3>().0 {
        let Some(a) = positions.get(triangle[0] as usize) else {
            continue;
        };
        let Some(b) = positions.get(triangle[1] as usize) else {
            continue;
        };
        let Some(c) = positions.get(triangle[2] as usize) else {
            continue;
        };
        sum += a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
            + a[2] * (b[0] * c[1] - b[1] * c[0]);
    }
    (sum / 6.0).abs()
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

fn placement_point(source: &[u8], express_id: u32) -> Result<(u32, [f64; 3]), EditError> {
    let (axis_id, _) = placement_axis(source, express_id)?;
    let axis = decode(source, axis_id)?;
    let point_id = axis.get_ref(0).ok_or(EditError::NotAPlacement)?;
    let point = cartesian_point(source, point_id)?;
    Ok((point_id, point))
}

fn placement_axis(source: &[u8], express_id: u32) -> Result<(u32, [f64; 3]), EditError> {
    let product = decode(source, express_id)?;
    let placement_id = product.get_ref(5).ok_or(EditError::NotAPlacement)?;
    let mut current = placement_id;
    for _ in 0..16 {
        let placement = decode(source, current)?;
        if placement.ifc_type == IfcType::IfcAxis2Placement3D {
            let point_id = placement.get_ref(0).ok_or(EditError::NotAPlacement)?;
            return Ok((current, cartesian_point(source, point_id)?));
        }
        if placement.ifc_type == IfcType::IfcLocalPlacement {
            current = placement.get_ref(1).ok_or(EditError::NotAPlacement)?;
            continue;
        }
        return Err(EditError::NotAPlacement);
    }
    Err(EditError::NotAPlacement)
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

fn entity_span(source: &[u8], id: u32) -> Option<std::ops::Range<usize>> {
    let mut scanner = EntityScanner::new(source);
    while let Some((found, _, start, mut end)) = scanner.next_entity() {
        if found != id {
            continue;
        }
        if (end == 0 || source.get(end - 1) != Some(&b';'))
            && let Some(semi) = source[end..].iter().position(|byte| *byte == b';')
        {
            end += semi + 1;
        }
        while matches!(source.get(end), Some(b'\n' | b'\r')) {
            end += 1;
        }
        return Some(start..end);
    }
    None
}

fn max_id(source: &[u8]) -> u32 {
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

fn append_entities(source: &[u8], block: &str) -> Result<Vec<u8>, EditError> {
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
    next.extend_from_slice(block.as_bytes());
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
    let guid = ifc_guid(relation);
    let block = format!(
        "#{relation}=IFCRELCONTAINEDINSPATIALSTRUCTURE('{guid}',{owner},$,$,(#{proxy}),#{structure});\n"
    );
    append_entities(source, &block)
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

fn argument_span(entity: &[u8], index: usize) -> Option<std::ops::Range<usize>> {
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

fn omit_span_and_ref(source: &[u8], span: std::ops::Range<usize>, id: u32) -> Vec<u8> {
    let mut next: Vec<u8> = Vec::with_capacity(source.len());
    let mut cursor = 0usize;
    while cursor < source.len() {
        if cursor == span.start {
            cursor = span.end;
            continue;
        }
        if cursor >= span.start && cursor < span.end {
            cursor += 1;
            continue;
        }
        if source[cursor] == b'#' && exact_ref_at(source, cursor, id) {
            let token_end = cursor + 1 + id.to_string().len();
            if next.iter().rev().find(|byte| !byte.is_ascii_whitespace()) == Some(&b',') {
                while next.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
                    next.pop();
                }
                next.pop();
            } else {
                let mut look = token_end;
                while source
                    .get(look)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    look += 1;
                }
                if source.get(look) == Some(&b',') {
                    cursor = look + 1;
                    continue;
                }
            }
            cursor = token_end;
            continue;
        }
        next.push(source[cursor]);
        cursor += 1;
    }
    next
}

fn exact_ref_at(source: &[u8], start: usize, id: u32) -> bool {
    let digits = id.to_string();
    let bytes = digits.as_bytes();
    let end = start + 1 + bytes.len();
    source.get(start + 1..end) == Some(bytes)
        && !source.get(end).is_some_and(|byte| byte.is_ascii_digit())
}

fn strip_one_ref(list: &[u8], id: u32) -> Option<Vec<u8>> {
    let mut found = false;
    let mut next: Vec<u8> = Vec::with_capacity(list.len());
    let mut cursor = 0usize;
    while cursor < list.len() {
        if list[cursor] == b'#' && exact_ref_at(list, cursor, id) {
            found = true;
            let token_end = cursor + 1 + id.to_string().len();
            if next.iter().rev().find(|byte| !byte.is_ascii_whitespace()) == Some(&b',') {
                while next.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
                    next.pop();
                }
                next.pop();
            } else {
                let mut look = token_end;
                while list
                    .get(look)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    look += 1;
                }
                if list.get(look) == Some(&b',') {
                    cursor = look + 1;
                    continue;
                }
            }
            cursor = token_end;
            continue;
        }
        next.push(list[cursor]);
        cursor += 1;
    }
    found.then_some(next)
}

fn ifc_guid(salt: u32) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz_$";
    let mut value = 0x9E37_79B9_7F4A_7C15u64 ^ ((salt as u64) << 17) ^ salt as u64;
    let mut out = String::with_capacity(22);
    for _ in 0..22 {
        out.push(ALPHABET[(value % 64) as usize] as char);
        value = value
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
    }
    out
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
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\nFILE_NAME('','',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n{data}ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn sample() -> String {
        file(
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
        let point = point_text(std::str::from_utf8(&moved.bytes).unwrap(), 13);
        assert!(point.contains("11.0,2.0,2.0"), "{point}");
        assert_eq!(moved.pivot, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_yaw_writes_a_ref_direction() {
        let turned = yaw_placement(sample().as_bytes(), 10, std::f64::consts::FRAC_PI_2).unwrap();
        let text = String::from_utf8(turned.bytes).unwrap();
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
        let aggregate = point_text(&text, 30);
        assert!(aggregate.contains("()"), "{aggregate}");
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
        assert!((mesh_volume(&added.positions, &added.indices) - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn replacing_the_mesh_points_the_product_at_the_new_shape() {
        let positions = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
        let indices = [0, 1, 2];
        let replaced = replace_mesh(sample().as_bytes(), 10, &positions, &indices).unwrap();
        let text = String::from_utf8(replaced).unwrap();
        let wall = point_text(&text, 10);
        assert!(
            wall.contains("IFCPRODUCTDEFINITIONSHAPE") || wall.contains('#'),
            "{wall}"
        );
        assert!(text.contains("IFCPOLYGONALFACESET"));
        assert!(
            !wall.contains(",$,$);") && !wall.ends_with(",$,$);\n")
                || wall.contains("IFCPRODUCTDEFINITIONSHAPE")
                || !wall.contains(",$,$,$")
        );
        assert!(
            wall.contains("IFCINDEXED") || text.contains(&format!("#{}", max_id(text.as_bytes())))
        );
        let representation = argument_span(wall.as_bytes(), 6).unwrap();
        let token = &wall.as_bytes()[representation];
        assert_ne!(token, b"$");
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
        assert!((mesh_volume(&positions, &indices) - 1.0).abs() < 1.0e-6);
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
        assert!((mesh_volume(&positions, &indices) - 6.0).abs() < 1.0e-6);
        indices.push(99);
        assert!((mesh_volume(&positions, &indices) - 6.0).abs() < 1.0e-6);
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
        let twice = yaw_placement(&once.bytes, 10, std::f64::consts::FRAC_PI_2).unwrap();
        let text = String::from_utf8(twice.bytes).unwrap();
        let direction = text
            .lines()
            .find(|line| line.contains("IFCDIRECTION"))
            .unwrap();
        assert!(direction.contains("-1.0"), "{direction}");
        assert!(point_text(&text, 13).contains("(1.,2.,3.)"));
    }

    #[test]
    fn deleting_a_longer_id_leaves_the_shorter_one() {
        let deleted = delete_product(sample().as_bytes(), 100).unwrap();
        let text = String::from_utf8(deleted).unwrap();
        assert!(text.contains("#10="));
        assert!(!text.contains("#100="));
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

    #[test]
    fn a_wall_is_not_a_relation() {
        assert_eq!(
            change_relation(sample().as_bytes(), 10, 100, true).unwrap_err(),
            EditError::NotARelation
        );
    }
}
