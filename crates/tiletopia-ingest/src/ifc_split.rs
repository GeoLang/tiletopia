use crate::IngestError;
use ifc_lite_core::{IfcType, has_geometry_by_name, ifc_type_from_keyword};
use rustc_hash::{FxHashMap, FxHashSet};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct IfcPiece {
    pub group_id: usize,
    pub path: PathBuf,
}

pub fn group_count(
    source_len: u64,
    max_source_for_one_read: u64,
    max_piece_bytes: u64,
) -> Result<Option<usize>, IngestError> {
    if max_piece_bytes == 0 {
        return Err(IngestError::ParseError(
            "an IFC piece size must be greater than zero".to_string(),
        ));
    }
    if source_len <= max_source_for_one_read {
        return Ok(None);
    }
    let groups = source_len.div_ceil(max_piece_bytes).max(2);
    let groups = usize::try_from(groups).map_err(|_| {
        IngestError::ParseError(format!(
            "an IFC of {source_len} bytes needs more pieces than this machine can count"
        ))
    })?;
    Ok(Some(groups))
}

pub fn split_ifc(
    source: &[u8],
    output_dir: &Path,
    group_count: usize,
) -> Result<Vec<IfcPiece>, IngestError> {
    if group_count == 0 {
        return Err(IngestError::ParseError(
            "an IFC split needs at least one group".to_string(),
        ));
    }
    let mut indexed = index_ifc(source)?;
    let elements = ids_of_types(&indexed.index, is_element);
    if elements.is_empty() {
        return Ok(Vec::new());
    }
    let spatial = ids_of_types(&indexed.index, is_spatial);
    let shared = shared_ids(&indexed.index, source, &spatial, &elements)?;
    let voids = void_fill_from_source(&indexed.index, source);
    let aggregates = aggregate_from_source(&indexed.index, source, &elements);
    let styles = style_maps_from_source(&indexed.index, source);
    let clusters = build_clusters(&elements, &voids, &aggregates);
    let bins = distribute(&clusters, group_count);
    let relations = relation_entries(&indexed.index, source, &spatial, &elements)?;
    let mut groups = resolve_groups(
        source,
        output_dir,
        &bins,
        &GroupInputs {
            index: &indexed.index,
            shared: &shared,
            elements: &elements,
            voids: &voids,
            aggregates: &aggregates,
            styles: &styles,
            relations: &relations,
        },
    )?;
    let slot_count = indexed.index.slot_count();
    let slots = std::mem::take(&mut indexed.index.slot_of);
    let membership = group_membership(&groups, &slots, slot_count);
    for group in &mut groups {
        group.file_ids = FxHashSet::default();
    }
    indexed.index.release_graph();
    write_groups(
        source,
        indexed.header_end,
        indexed.footer_start,
        &groups,
        &slots,
        &membership,
    )?;
    tracing::info!(files = groups.len(), "split the IFC into smaller files");
    Ok(groups
        .into_iter()
        .map(|group| IfcPiece {
            group_id: group.group_id,
            path: group.path,
        })
        .collect())
}

struct IndexedIfc {
    header_end: usize,
    footer_start: usize,
    index: LineIndex,
}

struct StepEntity {
    id: u32,
    type_name: String,
    start: usize,
    end: usize,
    paren: usize,
    refs: Vec<u32>,
}

struct SpecialText {
    start: usize,
    end: usize,
    paren: usize,
}

struct LineIndex {
    slot_of: FxHashMap<u32, u32>,
    ids: Vec<u32>,
    types: Vec<u16>,
    type_names: Vec<String>,
    type_of_name: FxHashMap<String, u16>,
    ref_start: Vec<u32>,
    ref_len: Vec<u32>,
    refs: Vec<u32>,
    special: FxHashMap<u32, SpecialText>,
}

impl LineIndex {
    fn new() -> Self {
        Self {
            slot_of: FxHashMap::default(),
            ids: Vec::new(),
            types: Vec::new(),
            type_names: Vec::new(),
            type_of_name: FxHashMap::default(),
            ref_start: Vec::new(),
            ref_len: Vec::new(),
            refs: Vec::new(),
            special: FxHashMap::default(),
        }
    }

    fn slot_count(&self) -> usize {
        self.ids.len()
    }

    fn type_name(&self, id: u32) -> Option<&str> {
        let slot = *self.slot_of.get(&id)? as usize;
        self.type_names
            .get(self.types[slot] as usize)
            .map(String::as_str)
    }

    fn refs_of(&self, id: u32) -> Option<&[u32]> {
        let slot = *self.slot_of.get(&id)? as usize;
        let start = self.ref_start[slot] as usize;
        let end = start + self.ref_len[slot] as usize;
        self.refs.get(start..end)
    }

    fn insert(&mut self, entity: &StepEntity) -> Result<(), IngestError> {
        let type_id = self.intern(&entity.type_name)?;
        let slot = if let Some(slot) = self.slot_of.get(&entity.id).copied() {
            slot as usize
        } else {
            let slot = self.ids.len();
            let slot_u32 = u32::try_from(slot).map_err(|_| {
                IngestError::ParseError(
                    "the IFC has more entities than a split can index".to_string(),
                )
            })?;
            self.slot_of.insert(entity.id, slot_u32);
            self.ids.push(entity.id);
            self.types.push(type_id);
            self.ref_start.push(0);
            self.ref_len.push(0);
            slot
        };
        self.ids[slot] = entity.id;
        self.types[slot] = type_id;
        let start = u32::try_from(self.refs.len()).map_err(|_| {
            IngestError::ParseError(
                "the IFC has more references than a split can index".to_string(),
            )
        })?;
        self.refs.extend(&entity.refs);
        self.ref_start[slot] = start;
        self.ref_len[slot] = u32::try_from(entity.refs.len()).map_err(|_| {
            IngestError::ParseError(format!(
                "IFC entity #{} has more references than a split can index",
                entity.id
            ))
        })?;
        if keeps_text(&entity.type_name) {
            self.special.insert(
                entity.id,
                SpecialText {
                    start: entity.start,
                    end: entity.end,
                    paren: entity.paren,
                },
            );
        }
        Ok(())
    }

    fn intern(&mut self, name: &str) -> Result<u16, IngestError> {
        if let Some(id) = self.type_of_name.get(name).copied() {
            return Ok(id);
        }
        let id = u16::try_from(self.type_names.len()).map_err(|_| {
            IngestError::ParseError("the IFC has more than 65535 entity types".to_string())
        })?;
        self.type_of_name.insert(name.to_string(), id);
        self.type_names.push(name.to_string());
        Ok(id)
    }

    fn release_graph(&mut self) {
        self.types.clear();
        self.type_names.clear();
        self.type_of_name.clear();
        self.ref_start.clear();
        self.ref_len.clear();
        self.refs.clear();
        self.special.clear();
    }
}

fn keeps_text(type_name: &str) -> bool {
    type_name.starts_with("IFCREL")
        || type_name == "IFCSTYLEDITEM"
        || type_name == "IFCMATERIALDEFINITIONREPRESENTATION"
        || type_name == "IFCPRESENTATIONLAYERASSIGNMENT"
}

fn should_rewrite(type_name: &str) -> bool {
    if type_name == "IFCRELVOIDSELEMENT" || type_name == "IFCRELFILLSELEMENT" {
        return false;
    }
    type_name.starts_with("IFCREL") || type_name == "IFCPRESENTATIONLAYERASSIGNMENT"
}

fn is_element(type_name: &str) -> bool {
    !is_spatial(type_name)
        && (has_geometry_by_name(type_name)
            || ifc_type_from_keyword(type_name).is_subtype_of(IfcType::IfcProduct))
}

fn is_spatial(type_name: &str) -> bool {
    let ifc_type = ifc_type_from_keyword(type_name);
    ifc_type == IfcType::IfcProject
        || (ifc_type.is_subtype_of(IfcType::IfcSpatialElement)
            && !ifc_type.is_subtype_of(IfcType::IfcSpace))
}

fn is_georeference(type_name: &str) -> bool {
    matches!(
        type_name,
        "IFCMAPCONVERSION"
            | "IFCMAPCONVERSIONSCALED"
            | "IFCPROJECTEDCRS"
            | "IFCGEOGRAPHICCRS"
            | "IFCWELLKNOWNTEXT"
            | "IFCRIGIDOPERATION"
    )
}

fn index_ifc(source: &[u8]) -> Result<IndexedIfc, IngestError> {
    let mut cursor = 0usize;
    let mut header_end = None;
    while cursor < source.len() {
        cursor = skip_trivia(source, cursor)?;
        if cursor >= source.len() {
            break;
        }
        if keyword_at(source, cursor, b"DATA") {
            let after = skip_trivia(source, cursor + 4)?;
            if source.get(after) == Some(&b';') {
                cursor = after + 1;
                if source.get(cursor) == Some(&b'\r') {
                    cursor += 1;
                }
                if source.get(cursor) == Some(&b'\n') {
                    cursor += 1;
                }
                header_end = Some(cursor);
                break;
            }
        }
        cursor = consume_statement(source, cursor)?;
    }
    let header_end = header_end
        .ok_or_else(|| IngestError::ParseError("the IFC has no DATA section".to_string()))?;
    let mut index = LineIndex::new();
    let mut footer_start = None;
    while cursor < source.len() {
        cursor = skip_trivia(source, cursor)?;
        if cursor >= source.len() {
            break;
        }
        if keyword_at(source, cursor, b"ENDSEC") {
            footer_start = Some(cursor);
            break;
        }
        let entity = parse_entity(source, cursor)?;
        index.insert(&entity)?;
        cursor = entity.end;
    }
    let footer_start = footer_start
        .ok_or_else(|| IngestError::ParseError("the IFC DATA section has no ENDSEC".to_string()))?;
    Ok(IndexedIfc {
        header_end,
        footer_start,
        index,
    })
}

fn parse_entity(source: &[u8], start: usize) -> Result<StepEntity, IngestError> {
    if source.get(start) != Some(&b'#') {
        return Err(step_error(start, "expected an IFC entity"));
    }
    let (id, mut cursor) = parse_id(source, start + 1)?;
    cursor = skip_trivia(source, cursor)?;
    if source.get(cursor) != Some(&b'=') {
        return Err(step_error(cursor, "expected '=' after an IFC id"));
    }
    cursor = skip_trivia(source, cursor + 1)?;
    let (type_name, next) = type_name_at(source, cursor)?;
    cursor = skip_trivia(source, next)?;
    if source.get(cursor) != Some(&b'(') {
        return Err(step_error(cursor, "expected '(' after an IFC type"));
    }
    let paren = cursor;
    cursor += 1;
    let mut refs = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    while cursor < source.len() {
        if !in_string && source[cursor] == b'/' && source.get(cursor + 1) == Some(&b'*') {
            cursor = skip_block_comment(source, cursor)?;
            continue;
        }
        let byte = source[cursor];
        if in_string {
            if byte == b'\'' {
                if source.get(cursor + 1) == Some(&b'\'') {
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
            b')' if depth == 0 => {
                cursor += 1;
                cursor = skip_trivia(source, cursor)?;
                if source.get(cursor) != Some(&b';') {
                    return Err(step_error(cursor, "expected ';' after an IFC entity"));
                }
                return Ok(StepEntity {
                    id,
                    type_name,
                    start,
                    end: cursor + 1,
                    paren,
                    refs,
                });
            }
            b')' => depth -= 1,
            b'#' => {
                if let Ok((reference, next)) = parse_id(source, cursor + 1) {
                    refs.push(reference);
                    cursor = next;
                    continue;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    Err(step_error(start, "the IFC entity does not end"))
}

fn entity_bounds(source: &[u8], start: usize) -> Result<(u32, usize), IngestError> {
    let entity = parse_entity(source, start)?;
    Ok((entity.id, entity.end))
}

fn parse_id(source: &[u8], mut cursor: usize) -> Result<(u32, usize), IngestError> {
    if !source.get(cursor).is_some_and(|byte| byte.is_ascii_digit()) {
        return Err(step_error(cursor, "expected an IFC id"));
    }
    let mut id: u32 = 0;
    while source.get(cursor).is_some_and(|byte| byte.is_ascii_digit()) {
        let digit = u32::from(source[cursor] - b'0');
        id = id
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
            .ok_or_else(|| step_error(cursor, "an IFC id does not fit the index"))?;
        cursor += 1;
    }
    if id == 0 {
        return Err(step_error(cursor, "IFC id 0 is not an entity"));
    }
    Ok((id, cursor))
}

fn type_name_at(source: &[u8], start: usize) -> Result<(String, usize), IngestError> {
    if !source
        .get(start)
        .is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        return Err(step_error(start, "expected an IFC type"));
    }
    let mut cursor = start + 1;
    while source
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        cursor += 1;
    }
    let name = std::str::from_utf8(&source[start..cursor])
        .map_err(|_| step_error(start, "an IFC type is not text"))?
        .to_ascii_uppercase();
    Ok((name, cursor))
}

fn consume_statement(source: &[u8], mut cursor: usize) -> Result<usize, IngestError> {
    let mut depth = 0i32;
    let mut in_string = false;
    while cursor < source.len() {
        if !in_string && source[cursor] == b'/' && source.get(cursor + 1) == Some(&b'*') {
            cursor = skip_block_comment(source, cursor)?;
            continue;
        }
        let byte = source[cursor];
        if in_string {
            if byte == b'\'' {
                if source.get(cursor + 1) == Some(&b'\'') {
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
            b')' => depth -= 1,
            b';' if depth <= 0 => return Ok(cursor + 1),
            _ => {}
        }
        cursor += 1;
    }
    Err(step_error(cursor, "the IFC statement does not end"))
}

fn skip_trivia(source: &[u8], mut cursor: usize) -> Result<usize, IngestError> {
    loop {
        while source
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += 1;
        }
        if source.get(cursor) == Some(&b'/') && source.get(cursor + 1) == Some(&b'*') {
            cursor = skip_block_comment(source, cursor)?;
            continue;
        }
        return Ok(cursor);
    }
}

fn skip_block_comment(source: &[u8], start: usize) -> Result<usize, IngestError> {
    let mut cursor = start + 2;
    while cursor + 1 < source.len() {
        if source[cursor] == b'*' && source[cursor + 1] == b'/' {
            return Ok(cursor + 2);
        }
        cursor += 1;
    }
    Err(step_error(start, "the IFC comment does not end"))
}

fn keyword_at(source: &[u8], cursor: usize, keyword: &[u8]) -> bool {
    let end = cursor + keyword.len();
    if end > source.len() || !source[cursor..end].eq_ignore_ascii_case(keyword) {
        return false;
    }
    !source
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn step_error(offset: usize, message: &str) -> IngestError {
    IngestError::ParseError(format!("{message} at byte {offset}"))
}

fn ids_of_types(index: &LineIndex, wanted: impl Fn(&str) -> bool) -> FxHashSet<u32> {
    let wanted_types: Vec<bool> = index.type_names.iter().map(|name| wanted(name)).collect();
    index
        .ids
        .iter()
        .zip(&index.types)
        .filter(|(_, type_id)| wanted_types[usize::from(**type_id)])
        .map(|(id, _)| *id)
        .collect()
}

fn shared_ids(
    index: &LineIndex,
    source: &[u8],
    spatial: &FxHashSet<u32>,
    elements: &FxHashSet<u32>,
) -> Result<FxHashSet<u32>, IngestError> {
    let mut shared = FxHashSet::default();
    for id in spatial {
        collect_deps_all(*id, index, &mut shared);
    }
    for id in &index.ids {
        if index.type_name(*id).is_some_and(is_georeference) {
            collect_deps_all(*id, index, &mut shared);
        }
    }
    add_spatial_relations(index, source, spatial, elements, &mut shared)?;
    Ok(shared)
}

fn add_spatial_relations(
    index: &LineIndex,
    source: &[u8],
    spatial: &FxHashSet<u32>,
    elements: &FxHashSet<u32>,
    shared: &mut FxHashSet<u32>,
) -> Result<(), IngestError> {
    for id in &index.ids {
        if !index.type_name(*id).is_some_and(should_rewrite) {
            continue;
        }
        let Some(text) = index.special.get(id) else {
            continue;
        };
        let arguments = relation_arguments(source, text.paren)?;
        let list_refs: Vec<u32> = arguments
            .lists
            .iter()
            .flat_map(|list| list.items.iter().filter_map(|(_, reference)| *reference))
            .collect();
        let links_only_spatial = !list_refs.is_empty()
            && list_refs
                .iter()
                .all(|reference| spatial.contains(reference))
            && !arguments
                .singles
                .iter()
                .any(|reference| elements.contains(reference));
        if links_only_spatial {
            collect_deps_all(*id, index, shared);
        }
    }
    Ok(())
}

fn collect_deps(
    start: u32,
    index: &LineIndex,
    visited: &mut FxHashSet<u32>,
    elements: &FxHashSet<u32>,
) {
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        let Some(refs) = index.refs_of(id) else {
            continue;
        };
        if !visited.insert(id) {
            continue;
        }
        for reference in refs {
            if visited.contains(reference) || elements.contains(reference) {
                continue;
            }
            stack.push(*reference);
        }
    }
}

fn collect_deps_all(start: u32, index: &LineIndex, visited: &mut FxHashSet<u32>) {
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        let Some(refs) = index.refs_of(id) else {
            continue;
        };
        if !visited.insert(id) {
            continue;
        }
        for reference in refs {
            if !visited.contains(reference) {
                stack.push(*reference);
            }
        }
    }
}

struct VoidFill {
    wall_openings: FxHashMap<u32, Vec<u32>>,
    opening_wall: FxHashMap<u32, u32>,
    opening_fillers: FxHashMap<u32, Vec<u32>>,
    filler_opening: FxHashMap<u32, u32>,
    rel_lines: FxHashMap<u32, Vec<u32>>,
}

fn void_fill_from_source(index: &LineIndex, source: &[u8]) -> VoidFill {
    let mut map = VoidFill {
        wall_openings: FxHashMap::default(),
        opening_wall: FxHashMap::default(),
        opening_fillers: FxHashMap::default(),
        filler_opening: FxHashMap::default(),
        rel_lines: FxHashMap::default(),
    };
    for id in &index.ids {
        let Some(type_name) = index.type_name(*id) else {
            continue;
        };
        let pair = if type_name == "IFCRELVOIDSELEMENT" {
            Some(true)
        } else if type_name == "IFCRELFILLSELEMENT" {
            Some(false)
        } else {
            None
        };
        let Some(is_void) = pair else {
            continue;
        };
        let Some(text) = index.special.get(id) else {
            continue;
        };
        let Ok(arguments) = split_arguments(source, text.paren) else {
            continue;
        };
        let (Some(left), Some(right)) = (
            arguments
                .get(4)
                .and_then(|range| hash_ref(source, range.clone())),
            arguments
                .get(5)
                .and_then(|range| hash_ref(source, range.clone())),
        ) else {
            continue;
        };
        if is_void {
            map.wall_openings.entry(left).or_default().push(right);
            map.opening_wall.insert(right, left);
        } else {
            map.opening_fillers.entry(left).or_default().push(right);
            map.filler_opening.insert(right, left);
        }
        map.rel_lines.entry(left).or_default().push(*id);
        map.rel_lines.entry(right).or_default().push(*id);
    }
    map
}

struct AggregateMap {
    parent_children: FxHashMap<u32, Vec<u32>>,
    child_parent: FxHashMap<u32, u32>,
    rel_lines: FxHashMap<u32, Vec<u32>>,
}

fn aggregate_from_source(
    index: &LineIndex,
    source: &[u8],
    elements: &FxHashSet<u32>,
) -> AggregateMap {
    let mut map = AggregateMap {
        parent_children: FxHashMap::default(),
        child_parent: FxHashMap::default(),
        rel_lines: FxHashMap::default(),
    };
    for id in &index.ids {
        if !matches!(
            index.type_name(*id),
            Some("IFCRELAGGREGATES" | "IFCRELNESTS")
        ) {
            continue;
        }
        let Some(text) = index.special.get(id) else {
            continue;
        };
        let Ok(arguments) = split_arguments(source, text.paren) else {
            continue;
        };
        let Some(parent) = arguments
            .get(4)
            .and_then(|range| hash_ref(source, range.clone()))
        else {
            continue;
        };
        if !elements.contains(&parent) {
            continue;
        }
        let Some(list) = arguments.get(5) else {
            continue;
        };
        let children: Vec<u32> = references_in(source, list.clone())
            .into_iter()
            .filter(|child| elements.contains(child))
            .collect();
        if children.is_empty() {
            continue;
        }
        for child in &children {
            map.child_parent.insert(*child, parent);
            map.rel_lines.entry(parent).or_default().push(*id);
            map.rel_lines.entry(*child).or_default().push(*id);
        }
        map.parent_children
            .entry(parent)
            .or_default()
            .extend(children);
    }
    map
}

struct StyleMaps {
    geometry_styles: FxHashMap<u32, Vec<u32>>,
    material_definitions: FxHashMap<u32, Vec<u32>>,
}

fn style_maps_from_source(index: &LineIndex, source: &[u8]) -> StyleMaps {
    let mut styles = StyleMaps {
        geometry_styles: FxHashMap::default(),
        material_definitions: FxHashMap::default(),
    };
    for id in &index.ids {
        let Some(type_name) = index.type_name(*id) else {
            continue;
        };
        let Some(text) = index.special.get(id) else {
            continue;
        };
        let Ok(arguments) = split_arguments(source, text.paren) else {
            continue;
        };
        if type_name == "IFCSTYLEDITEM"
            && let Some(geometry) = arguments
                .first()
                .and_then(|range| hash_ref(source, range.clone()))
        {
            styles
                .geometry_styles
                .entry(geometry)
                .or_default()
                .push(*id);
        } else if type_name == "IFCMATERIALDEFINITIONREPRESENTATION"
            && let Some(material) = arguments
                .get(3)
                .and_then(|range| hash_ref(source, range.clone()))
        {
            styles
                .material_definitions
                .entry(material)
                .or_default()
                .push(*id);
        }
    }
    styles
}

fn build_clusters(
    elements: &FxHashSet<u32>,
    voids: &VoidFill,
    aggregates: &AggregateMap,
) -> Vec<Vec<u32>> {
    let mut order: Vec<u32> = elements.iter().copied().collect();
    order.sort_unstable();
    let mut assigned = FxHashSet::default();
    let mut clusters = Vec::new();
    for id in order {
        if !assigned.insert(id) {
            continue;
        }
        let mut cluster = Vec::new();
        let mut queue = vec![id];
        let mut seen = FxHashSet::default();
        seen.insert(id);
        while let Some(current) = queue.pop() {
            if elements.contains(&current) {
                cluster.push(current);
                assigned.insert(current);
            }
            expand_void(current, voids, &mut seen, &mut queue);
            expand_aggregate(current, aggregates, &mut seen, &mut queue);
        }
        cluster.sort_unstable();
        cluster.dedup();
        if !cluster.is_empty() {
            clusters.push(cluster);
        }
    }
    clusters
}

fn expand_void(id: u32, voids: &VoidFill, seen: &mut FxHashSet<u32>, queue: &mut Vec<u32>) {
    let add = |next: u32, seen: &mut FxHashSet<u32>, queue: &mut Vec<u32>| {
        if seen.insert(next) {
            queue.push(next);
        }
    };
    if let Some(openings) = voids.wall_openings.get(&id) {
        for opening in openings {
            add(*opening, seen, queue);
            if let Some(fillers) = voids.opening_fillers.get(opening) {
                for filler in fillers {
                    add(*filler, seen, queue);
                }
            }
        }
    }
    if let Some(opening) = voids.filler_opening.get(&id) {
        add(*opening, seen, queue);
        if let Some(wall) = voids.opening_wall.get(opening) {
            add(*wall, seen, queue);
        }
    }
    if let Some(wall) = voids.opening_wall.get(&id) {
        add(*wall, seen, queue);
    }
}

fn expand_aggregate(
    id: u32,
    aggregates: &AggregateMap,
    seen: &mut FxHashSet<u32>,
    queue: &mut Vec<u32>,
) {
    let add = |next: u32, seen: &mut FxHashSet<u32>, queue: &mut Vec<u32>| {
        if seen.insert(next) {
            queue.push(next);
        }
    };
    if let Some(children) = aggregates.parent_children.get(&id) {
        for child in children {
            add(*child, seen, queue);
        }
    }
    if let Some(parent) = aggregates.child_parent.get(&id) {
        add(*parent, seen, queue);
        if let Some(siblings) = aggregates.parent_children.get(parent) {
            for sibling in siblings {
                add(*sibling, seen, queue);
            }
        }
    }
}

fn distribute(clusters: &[Vec<u32>], group_count: usize) -> Vec<Vec<u32>> {
    let mut order: Vec<usize> = (0..clusters.len()).collect();
    order.sort_by(|&left, &right| {
        clusters[right]
            .len()
            .cmp(&clusters[left].len())
            .then(clusters[left][0].cmp(&clusters[right][0]))
    });
    let mut groups = vec![Vec::new(); group_count];
    let mut sizes = vec![0usize; group_count];
    for cluster_index in order {
        let mut smallest = 0usize;
        for candidate in 1..group_count {
            if sizes[candidate] < sizes[smallest] {
                smallest = candidate;
            }
        }
        sizes[smallest] += clusters[cluster_index].len();
        groups[smallest].extend_from_slice(&clusters[cluster_index]);
    }
    groups
}

struct ListArgument {
    argument: std::ops::Range<usize>,
    items: Vec<(std::ops::Range<usize>, Option<u32>)>,
}

struct RelationArguments {
    singles: Vec<u32>,
    lists: Vec<ListArgument>,
}

fn relation_arguments(source: &[u8], paren: usize) -> Result<RelationArguments, IngestError> {
    let mut arguments = RelationArguments {
        singles: Vec::new(),
        lists: Vec::new(),
    };
    for argument in split_arguments(source, paren)? {
        let start = skip_trivia(source, argument.start)?;
        if source.get(start) == Some(&b'(') {
            let items = split_arguments(source, start)?
                .into_iter()
                .map(|item| {
                    let reference = hash_ref(source, item.clone());
                    (item, reference)
                })
                .collect();
            arguments.lists.push(ListArgument { argument, items });
        } else if let Some(reference) = hash_ref(source, argument) {
            arguments.singles.push(reference);
        }
    }
    Ok(arguments)
}

struct RelationEntry {
    id: u32,
    entity: std::ops::Range<usize>,
    single_elements: Vec<u32>,
    element_lists: Vec<ListArgument>,
    references_spatial: bool,
}

fn relation_entries(
    index: &LineIndex,
    source: &[u8],
    spatial: &FxHashSet<u32>,
    elements: &FxHashSet<u32>,
) -> Result<Vec<RelationEntry>, IngestError> {
    let mut entries = Vec::new();
    for id in &index.ids {
        if !index.type_name(*id).is_some_and(should_rewrite) {
            continue;
        }
        let Some(text) = index.special.get(id) else {
            continue;
        };
        let arguments = relation_arguments(source, text.paren)?;
        let references_spatial = arguments
            .singles
            .iter()
            .chain(arguments.lists.iter().flat_map(|list| {
                list.items
                    .iter()
                    .filter_map(|(_, reference)| reference.as_ref())
            }))
            .any(|reference| spatial.contains(reference));
        let single_elements: Vec<u32> = arguments
            .singles
            .into_iter()
            .filter(|reference| elements.contains(reference))
            .collect();
        let element_lists: Vec<ListArgument> = arguments
            .lists
            .into_iter()
            .map(|list| ListArgument {
                argument: list.argument,
                items: list
                    .items
                    .into_iter()
                    .map(|(item, reference)| (item, reference.filter(|id| elements.contains(id))))
                    .collect(),
            })
            .filter(|list| list.items.iter().any(|(_, element)| element.is_some()))
            .collect();
        if single_elements.is_empty() && element_lists.is_empty() {
            continue;
        }
        entries.push(RelationEntry {
            id: *id,
            entity: text.start..text.end,
            single_elements,
            element_lists,
            references_spatial,
        });
    }
    Ok(entries)
}

fn relation_in_group(
    source: &[u8],
    relation: &RelationEntry,
    members: &FxHashSet<u32>,
) -> Option<Vec<u8>> {
    if !relation
        .single_elements
        .iter()
        .all(|id| members.contains(id))
    {
        return None;
    }
    let mut links_a_member = !relation.single_elements.is_empty();
    let mut out = Vec::with_capacity(relation.entity.len());
    let mut copied_to = relation.entity.start;
    for list in &relation.element_lists {
        let kept: Vec<&(std::ops::Range<usize>, Option<u32>)> = list
            .items
            .iter()
            .filter(|(_, element)| element.is_none_or(|id| members.contains(&id)))
            .collect();
        if kept.is_empty() {
            return None;
        }
        links_a_member |= kept.iter().any(|(_, element)| element.is_some());
        out.extend_from_slice(&source[copied_to..list.argument.start]);
        out.push(b'(');
        for (position, (item, _)) in kept.into_iter().enumerate() {
            if position > 0 {
                out.push(b',');
            }
            out.extend_from_slice(source[item.clone()].trim_ascii());
        }
        out.push(b')');
        copied_to = list.argument.end;
    }
    if !links_a_member && !relation.references_spatial {
        return None;
    }
    out.extend_from_slice(&source[copied_to..relation.entity.end]);
    Some(out)
}

struct GroupWrite {
    group_id: usize,
    path: PathBuf,
    file_ids: FxHashSet<u32>,
    rewritten: FxHashMap<u32, Vec<u8>>,
}

struct GroupInputs<'a> {
    index: &'a LineIndex,
    shared: &'a FxHashSet<u32>,
    elements: &'a FxHashSet<u32>,
    voids: &'a VoidFill,
    aggregates: &'a AggregateMap,
    styles: &'a StyleMaps,
    relations: &'a [RelationEntry],
}

fn resolve_groups(
    source: &[u8],
    output_dir: &Path,
    bins: &[Vec<u32>],
    inputs: &GroupInputs<'_>,
) -> Result<Vec<GroupWrite>, IngestError> {
    let mut groups = Vec::new();
    for (group_id, members) in bins.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let member_set: FxHashSet<u32> = members.iter().copied().collect();
        let mut file_ids = inputs.shared.clone();
        for id in members {
            collect_deps(*id, inputs.index, &mut file_ids, inputs.elements);
        }
        for id in members {
            if let Some(rels) = inputs.voids.rel_lines.get(id) {
                for rel in rels {
                    collect_deps(*rel, inputs.index, &mut file_ids, inputs.elements);
                }
            }
            if let Some(rels) = inputs.aggregates.rel_lines.get(id) {
                for rel in rels {
                    collect_deps(*rel, inputs.index, &mut file_ids, inputs.elements);
                }
            }
        }
        resolve_styles(&mut file_ids, inputs.index, inputs.styles, inputs.elements);
        let mut rewritten = FxHashMap::default();
        for relation in inputs.relations {
            let Some(text) = relation_in_group(source, relation, &member_set) else {
                continue;
            };
            rewritten.insert(relation.id, text);
            file_ids.insert(relation.id);
            if let Some(refs) = inputs.index.refs_of(relation.id) {
                for reference in refs {
                    if !inputs.elements.contains(reference) {
                        collect_deps(*reference, inputs.index, &mut file_ids, inputs.elements);
                    }
                }
            }
        }
        groups.push(GroupWrite {
            group_id,
            path: output_dir.join(format!("{group_id}.ifc")),
            file_ids,
            rewritten,
        });
    }
    Ok(groups)
}

fn resolve_styles(
    file_ids: &mut FxHashSet<u32>,
    index: &LineIndex,
    styles: &StyleMaps,
    elements: &FxHashSet<u32>,
) {
    for (geometry, styled) in &styles.geometry_styles {
        if !file_ids.contains(geometry) {
            continue;
        }
        for id in styled {
            collect_deps(*id, index, file_ids, elements);
        }
    }
    for (material, definitions) in &styles.material_definitions {
        if !file_ids.contains(material) {
            continue;
        }
        for id in definitions {
            collect_deps(*id, index, file_ids, elements);
        }
    }
}

struct Membership {
    start: Vec<u32>,
    members: Vec<u32>,
}

fn group_membership(
    groups: &[GroupWrite],
    slots: &FxHashMap<u32, u32>,
    slot_count: usize,
) -> Membership {
    let mut start = vec![0u32; slot_count + 1];
    for group in groups {
        for id in &group.file_ids {
            let Some(slot) = slots.get(id) else {
                continue;
            };
            start[*slot as usize + 1] += 1;
        }
    }
    for index in 1..start.len() {
        start[index] += start[index - 1];
    }
    let mut cursor = start.clone();
    let mut members = vec![0u32; start[slot_count] as usize];
    for (group_index, group) in groups.iter().enumerate() {
        for id in &group.file_ids {
            let Some(slot) = slots.get(id).copied() else {
                continue;
            };
            let at = cursor[slot as usize] as usize;
            members[at] = group_index as u32;
            cursor[slot as usize] += 1;
        }
    }
    Membership { start, members }
}

impl Membership {
    fn groups_of(&self, slot: u32) -> &[u32] {
        let start = self.start[slot as usize] as usize;
        let end = self.start[slot as usize + 1] as usize;
        &self.members[start..end]
    }
}

fn write_groups(
    source: &[u8],
    header_end: usize,
    footer_start: usize,
    groups: &[GroupWrite],
    slots: &FxHashMap<u32, u32>,
    membership: &Membership,
) -> Result<(), IngestError> {
    if groups.is_empty() {
        return Ok(());
    }
    let mut writers = Vec::with_capacity(groups.len());
    for group in groups {
        if let Some(parent) = group.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(&group.path)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&source[..header_end])?;
        writers.push(writer);
    }
    let mut cursor = header_end;
    while cursor < footer_start {
        cursor = skip_trivia(source, cursor)?;
        if cursor >= footer_start || keyword_at(source, cursor, b"ENDSEC") {
            break;
        }
        let (id, end) = entity_bounds(source, cursor)?;
        if let Some(slot) = slots.get(&id).copied() {
            for group_index in membership.groups_of(slot) {
                let writer = &mut writers[*group_index as usize];
                if let Some(line) = groups[*group_index as usize].rewritten.get(&id) {
                    writer.write_all(line)?;
                } else {
                    writer.write_all(&source[cursor..end])?;
                }
                writer.write_all(b"\n")?;
            }
        }
        cursor = end;
    }
    let footer = &source[footer_start..];
    for writer in &mut writers {
        writer.write_all(footer)?;
        writer.flush()?;
    }
    Ok(())
}

fn split_arguments(
    source: &[u8],
    paren: usize,
) -> Result<Vec<std::ops::Range<usize>>, IngestError> {
    if source.get(paren) != Some(&b'(') {
        return Err(step_error(paren, "expected an IFC argument list"));
    }
    let mut cursor = paren + 1;
    let mut arguments = Vec::new();
    let mut argument_start = cursor;
    let mut depth = 0i32;
    let mut in_string = false;
    while cursor < source.len() {
        let byte = source[cursor];
        if in_string {
            if byte == b'\'' {
                if source.get(cursor + 1) == Some(&b'\'') {
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
            b')' if depth == 0 => {
                arguments.push(argument_start..cursor);
                return Ok(arguments);
            }
            b')' => depth -= 1,
            b',' if depth == 0 => {
                arguments.push(argument_start..cursor);
                argument_start = cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }
    Err(step_error(paren, "the IFC argument list does not end"))
}

fn references_in(source: &[u8], range: std::ops::Range<usize>) -> Vec<u32> {
    let bytes = &source[range.start..range.end];
    let mut refs = Vec::new();
    let mut cursor = 0usize;
    let mut in_string = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if in_string {
            if byte == b'\'' {
                if bytes.get(cursor + 1) == Some(&b'\'') {
                    cursor += 2;
                    continue;
                }
                in_string = false;
            }
            cursor += 1;
            continue;
        }
        if byte == b'\'' {
            in_string = true;
            cursor += 1;
            continue;
        }
        if byte == b'#'
            && let Ok((id, next)) = parse_id(bytes, cursor + 1)
        {
            refs.push(id);
            cursor = next;
            continue;
        }
        cursor += 1;
    }
    refs
}

fn hash_ref(source: &[u8], range: std::ops::Range<usize>) -> Option<u32> {
    let mut cursor = range.start;
    while cursor < range.end && source[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if source.get(cursor) != Some(&b'#') {
        return None;
    }
    parse_id(source, cursor + 1)
        .ok()
        .filter(|(_, end)| *end <= range.end)
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ifc(data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\n\
             FILE_NAME('fixture.ifc','2024-01-01',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\n\
             ENDSEC;\nDATA;\n{data}\nENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn texts(pieces: &[IfcPiece]) -> Vec<String> {
        pieces
            .iter()
            .map(|piece| std::fs::read_to_string(&piece.path).unwrap())
            .collect()
    }

    #[test]
    fn a_file_inside_one_read_is_not_split() {
        let groups = group_count(13 * 1024 * 1024, 100 * 1024 * 1024, 32 * 1024 * 1024).unwrap();
        assert_eq!(groups, None);
    }

    #[test]
    fn a_file_past_one_read_is_split_into_piece_sized_groups() {
        let source = 200 * 1024 * 1024;
        let piece = 32 * 1024 * 1024;
        let groups = group_count(source, 100 * 1024 * 1024, piece)
            .unwrap()
            .unwrap();
        assert_eq!(groups, usize::try_from(source.div_ceil(piece)).unwrap());
    }

    fn defines(text: &str, id: u32) -> bool {
        let definition = format!("#{id}=");
        text.lines().any(|line| line.starts_with(&definition))
    }

    fn unresolved_references(text: &str) -> Vec<u32> {
        let indexed = index_ifc(text.as_bytes()).unwrap();
        let mut missing: Vec<u32> = indexed
            .index
            .refs
            .iter()
            .copied()
            .filter(|reference| !indexed.index.slot_of.contains_key(reference))
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }

    #[test]
    fn a_zero_piece_size_is_refused() {
        let error = group_count(10, 1, 0).unwrap_err();
        assert!(error.to_string().contains("piece size"));
    }

    #[test]
    fn an_unclosed_comment_is_refused_by_name() {
        let source = ifc("#1=IFCPROJECT('0project',$,'Tower',$,$,$,$,$,$);\n/* never closed");
        let dir = tempfile::tempdir().unwrap();
        let error = split_ifc(source.as_bytes(), dir.path(), 2).unwrap_err();
        assert!(
            error.to_string().contains("comment does not end"),
            "{error}"
        );
    }

    #[test]
    fn every_product_lands_in_one_piece_and_facilities_in_all() {
        let source = ifc("\
#1=IFCPROJECT('0project',$,'Road',$,$,$,$,$,$);
#2=IFCSITE('0site',$,'Site',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCROAD('0road',$,'Road',$,$,$,$,$,.ELEMENT.,.NOTDEFINED.);
#5=IFCRELAGGREGATES('0agg1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('0agg2',$,$,$,#2,(#3));
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#30=IFCDUCTSEGMENT('0duct',$,'Duct',$,$,#12,$,$,.RIGIDSEGMENT.);
#31=IFCDISTRIBUTIONPORT('0port',$,'Port',$,$,#12,$,.SOURCE.,.DUCT.,.SUPPLYAIR.);
#32=IFCRELNESTS('0nest',$,$,$,#30,(#31));
#33=IFCFURNITURE('0chair',$,'Chair',$,$,#12,$,$,$);
#34=IFCELEMENTASSEMBLY('0truss',$,'Truss',$,$,#12,$,$,.FACTORY.,.TRUSS.);
#35=IFCPLATE('0gusset',$,'Gusset',$,$,#12,$,$,$);
#36=IFCMEMBER('0chord',$,'Chord',$,$,#12,$,$,$);
#37=IFCRELAGGREGATES('0parts',$,$,$,#34,(#35,#36));
#38=IFCREINFORCINGBAR('0bar',$,'Bar',$,$,#12,$,$,$,12.,$,$,$,$,$);
#39=IFCPAVEMENT('0pavement',$,'Pavement',$,$,#12,$,$,$);
#40=IFCCOURSE('0course',$,'Course',$,$,#12,$,$,$);
#50=IFCRELCONTAINEDINSPATIALSTRUCTURE('0cont',$,$,$,(#30,#33,#34,#38,#39,#40),#3);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 3).unwrap();
        let files = texts(&pieces);
        assert_eq!(files.len(), 3);
        for product in [30, 31, 33, 34, 35, 36, 38, 39, 40] {
            let holders = files.iter().filter(|file| defines(file, product)).count();
            assert_eq!(holders, 1, "#{product} is in {holders} pieces");
        }
        let duct_file = files.iter().find(|file| defines(file, 30)).unwrap();
        assert!(
            defines(duct_file, 31) && defines(duct_file, 32),
            "{duct_file}"
        );
        let truss_file = files.iter().find(|file| defines(file, 34)).unwrap();
        assert!(
            defines(truss_file, 35) && defines(truss_file, 36),
            "{truss_file}"
        );
        for file in &files {
            assert!(defines(file, 3), "{file}");
            assert_eq!(unresolved_references(file), Vec::<u32>::new(), "{file}");
        }
    }

    #[test]
    fn single_references_stay_single_and_both_ends_share_the_piece() {
        let source = ifc("\
#1=IFCPROJECT('0project',$,'Tower',$,$,$,$,$,$);
#2=IFCSITE('0site',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('0building',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCBUILDINGSTOREY('0storey',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCRELAGGREGATES('0rel1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('0rel2',$,$,$,#2,(#3));
#7=IFCRELAGGREGATES('0rel3',$,$,$,#3,(#4));
#30=IFCWALL('0wallA',$,'Wall A',$,$,$,$,$,.SOLIDWALL.);
#31=IFCWALL('0wallB',$,'Wall B',$,$,$,$,$,.SOLIDWALL.);
#60=IFCSPACE('0space',$,'Room',$,$,$,$,$,.ELEMENT.,.INTERNAL.,$);
#50=IFCRELCONTAINEDINSPATIALSTRUCTURE('0cont',$,$,$,(#30,#31,#60),#4);
#70=IFCRELSPACEBOUNDARY('0bound1',$,$,$,#60,#30,$,.PHYSICAL.,.INTERNAL.);
#71=IFCRELSPACEBOUNDARY('0bound2',$,$,$,#60,#31,$,.PHYSICAL.,.INTERNAL.);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 2).unwrap();
        let files = texts(&pieces);
        assert_eq!(files.len(), 2);
        for file in &files {
            assert!(!file.contains("(#60)"), "{file}");
            assert_eq!(unresolved_references(file), Vec::<u32>::new(), "{file}");
        }
        let joined = files.concat();
        let bounds_with_both_ends: Vec<&str> = joined
            .lines()
            .filter(|line| line.starts_with("#70=") || line.starts_with("#71="))
            .collect();
        assert_eq!(bounds_with_both_ends.len(), 1, "{joined}");
        let space_file = files.iter().find(|file| defines(file, 60)).unwrap();
        let wall_beside_the_space = if defines(space_file, 30) { 70 } else { 71 };
        assert!(defines(space_file, wall_beside_the_space), "{space_file}");
    }

    #[test]
    fn coupled_elements_stay_together_and_spatial_structure_is_copied() {
        let source = ifc("\
#1=IFCPROJECT('0project',$,'Tower',$,$,$,$,$,$);
#2=IFCSITE('0site',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('0building',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCBUILDINGSTOREY('0storey',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCRELAGGREGATES('0rel1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('0rel2',$,$,$,#2,(#3));
#7=IFCRELAGGREGATES('0rel3',$,$,$,#3,(#4));
#30=IFCWALL('0wallA',$,'Wall; #999 stays',$,$,$,$,$,.SOLIDWALL.);
#31=IFCWALL('0wallB',$,'Wall B',$,$,$,$,$,
.SOLIDWALL.);
#32=IFCOPENINGELEMENT('0open',$,'Opening',$,$,$,$,$,.OPENING.);
#33=IFCDOOR('0door',$,'Door',$,$,$,$,$,$,$,.DOOR.,$,$);
#34=IFCRELVOIDSELEMENT('0void',$,$,$,#30,#32);
#35=IFCRELFILLSELEMENT('0fill',$,$,$,#32,#33);
#40=IFCROOF('0roof',$,'Roof',$,$,$,$,$);
#41=IFCSLAB('0slab',$,'Slab',$,$,$,$,$,.FLOOR.);
#42=IFCRELAGGREGATES('0agg',$,$,$,#40,(#41));
#50=IFCRELCONTAINEDINSPATIALSTRUCTURE('0cont',$,$,$,(#30,#33,#31,#40,#41),#4);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 2).unwrap();
        assert_eq!(pieces.len(), 2, "{pieces:?}");
        let files = texts(&pieces);
        let wall_file = files
            .iter()
            .find(|file| file.contains("IFCRELVOIDSELEMENT"))
            .unwrap();
        assert!(wall_file.contains("#30=IFCWALL"));
        assert!(wall_file.contains("#32=IFCOPENINGELEMENT"));
        assert!(wall_file.contains("#33=IFCDOOR"));
        assert!(wall_file.contains("IFCRELFILLSELEMENT"));
        assert!(wall_file.contains("Wall; #999 stays"));
        assert!(!wall_file.contains("#40=IFCROOF"));
        let other = files
            .iter()
            .find(|file| file.contains("#40=IFCROOF"))
            .unwrap();
        assert!(other.contains("#41=IFCSLAB"));
        assert!(other.contains("#31=IFCWALL"));
        assert!(!other.contains("IFCRELVOIDSELEMENT"));
        for file in &files {
            assert!(file.contains("#1=IFCPROJECT"));
            assert!(file.contains("#4=IFCBUILDINGSTOREY"));
            assert!(file.contains("END-ISO-10303-21;"));
            assert!(file.contains("#50="));
        }
        assert!(wall_file.contains("(#30,#33)"));
        assert!(!wall_file.contains("#31"));
        assert!(other.contains("#31"));
        assert!(other.contains("#40"));
        assert!(other.contains("#41"));
        assert!(!other.contains("#30"));
    }

    #[test]
    fn every_piece_keeps_the_map_conversion() {
        let source = ifc("\
#1=IFCPROJECT('0project',$,'Tower',$,$,$,$,(#10),$);
#2=IFCSITE('0site',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCRELAGGREGATES('0rel',$,$,$,#1,(#2));
#10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,$,$);
#20=IFCPROJECTEDCRS('EPSG:32632');
#21=IFCMAPCONVERSION(#10,#20,1.,2.,3.,$,$,1.);
#30=IFCWALL('0wallA',$,'Wall A',$,$,$,$,$,.SOLIDWALL.);
#31=IFCWALL('0wallB',$,'Wall B',$,$,$,$,$,.SOLIDWALL.);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 2).unwrap();
        assert_eq!(pieces.len(), 2);
        for piece in &pieces {
            let text = std::fs::read_to_string(&piece.path).unwrap();
            assert!(text.contains("IFCMAPCONVERSION"), "{text}");
            assert!(text.contains("IFCPROJECTEDCRS"), "{text}");
        }
    }

    #[test]
    fn each_piece_of_a_styled_model_is_a_valid_ifc() {
        let source = ifc("\
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCRELAGGREGATES('rel',$,$,$,#1,(#2));
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#13=IFCDIRECTION((0.,0.,1.));
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#22=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#21));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#32=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#33=IFCPRODUCTDEFINITIONSHAPE($,$,(#32));
#40=IFCWALL('wall-a',$,'Wall A',$,$,#12,#31,$);
#41=IFCWALL('wall-b',$,'Wall B',$,$,#12,#33,$);
#50=IFCCOLOURRGB($,0.55,0.35,0.2);
#51=IFCSURFACESTYLERENDERING(#50,0.0,$,$,$,$,$,$,.NOTDEFINED.);
#52=IFCSURFACESTYLE('frame',.BOTH.,(#51));
#53=IFCSTYLEDITEM(#21,(#52),$);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 2).unwrap();
        assert_eq!(pieces.len(), 2);
        let mut coloured = 0;
        let mut plain = 0;
        for piece in &pieces {
            let text = std::fs::read_to_string(&piece.path).unwrap();
            let meshes = crate::ifc_reader::read(&piece.path, u64::MAX).unwrap();
            assert_eq!(meshes.len(), 1, "{text}");
            if text.contains("IFCSTYLEDITEM") {
                coloured += 1;
                assert!(meshes[0].element_color.is_some(), "{text}");
                assert_eq!(meshes[0].asset_id.as_deref(), Some("wall-a"));
            } else {
                plain += 1;
                assert_eq!(meshes[0].asset_id.as_deref(), Some("wall-b"));
            }
        }
        assert_eq!((coloured, plain), (1, 1));
    }

    #[test]
    fn split_pieces_can_be_read_on_two_threads() {
        let source = ifc("\
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCRELAGGREGATES('rel',$,$,$,#1,(#2));
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#13=IFCDIRECTION((0.,0.,1.));
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#22=IFCEXTRUDEDAREASOLID(#20,#11,#13,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#21));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#32=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#33=IFCPRODUCTDEFINITIONSHAPE($,$,(#32));
#40=IFCWALL('wall-a',$,'Wall A',$,$,#12,#31,$);
#41=IFCWALL('wall-b',$,'Wall B',$,$,#12,#33,$);
");
        let dir = tempfile::tempdir().unwrap();
        let pieces = split_ifc(source.as_bytes(), dir.path(), 2).unwrap();
        assert_eq!(pieces.len(), 2);
        let paths = [pieces[0].path.clone(), pieces[1].path.clone()];
        let meshes = std::thread::scope(|scope| {
            let handles =
                paths.map(|path| scope.spawn(move || crate::ifc_reader::read(&path, u64::MAX)));
            handles.map(|handle| handle.join().expect("a piece read panicked"))
        });
        let mut ids = Vec::new();
        for mesh_list in meshes {
            let mesh_list = mesh_list.expect("a piece read failed");
            assert_eq!(mesh_list.len(), 1);
            ids.push(mesh_list[0].asset_id.clone().expect("a wall id"));
        }
        ids.sort();
        assert_eq!(ids, ["wall-a", "wall-b"]);
    }
}
