use crate::ifc_edit::{
    argument_span, entity_span, exact_ref_at, max_id, reference_count, strip_one_ref,
};
use crate::ifc_elements::{
    ELEMENT_TAG_KEY, MAX_PROPERTIES_PER_ELEMENT, numbered_key, property_key, property_set_rows,
};
use ifc_lite_core::parser::StepNumber;
use ifc_lite_core::step_encoding::step_string_literal_end;
use ifc_lite_core::{
    EntityDecoder, EntityScanner, IfcType, StepListItems, ifc_type_from_keyword, read_step_number,
    skip_step_comment, slot,
};
use ifc_lite_processing::data_model::property::PropertyDecoder;
use ifc_lite_processing::data_model::quantity::{QuantityDecodeMode, decode_quantity_records};
use ifc_lite_processing::data_model::render::flat::new_property_decoder;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

pub const MAX_TEXT_BYTES: usize = 64 * 1024;
const SINGLE_VALUE_SLOT: usize = 2;
const QUANTITY_VALUE_SLOT: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyLiteral {
    Text(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PropertyWriteError {
    MissingElement,
    MissingProperty,
    DefinedOnType,
    NotASingleValue,
    KindMismatch,
    IntegerValue,
    ControlCharacter,
    EmptyNominal,
    TextTooLong,
    UnreadableEntity,
}

struct ValueSlot {
    member_id: u32,
    slot: usize,
}

struct DefinitionRow {
    key: String,
    value: Option<ValueSlot>,
}

struct DefinitionRows {
    members_slot: usize,
    rows: Vec<DefinitionRow>,
}

#[derive(Clone, Copy)]
struct WritableValue {
    member_id: u32,
    slot: usize,
    definition_id: u32,
    members_slot: usize,
}

#[derive(Clone, Copy)]
enum Target {
    Value(WritableValue),
    Unwritable,
}

struct RelationLink {
    relation_id: u32,
    related_objects: usize,
}

enum LiteralSpan {
    Text {
        start: usize,
        end: usize,
    },
    Number {
        start: usize,
        end: usize,
        integer: bool,
    },
    Enumeration {
        start: usize,
        end: usize,
    },
}

pub fn rewrite_property(
    source: &[u8],
    express_id: u32,
    key: &str,
    value: &PropertyLiteral,
) -> Result<Vec<u8>, PropertyWriteError> {
    if let PropertyLiteral::Text(text) = value {
        if text.len() > MAX_TEXT_BYTES {
            return Err(PropertyWriteError::TextTooLong);
        }
        if text.chars().any(char::is_control) {
            return Err(PropertyWriteError::ControlCharacter);
        }
    }
    if let PropertyLiteral::Number(number) = value
        && !number.is_finite()
    {
        return Err(PropertyWriteError::KindMismatch);
    }

    let mut spans = HashMap::new();
    let mut property_relations = Vec::new();
    let mut type_relations = Vec::new();
    let mut scanner = EntityScanner::new(source);
    while let Some((id, keyword, start, end)) = scanner.next_entity() {
        spans.insert(id, (start, end));
        match ifc_type_from_keyword(keyword) {
            IfcType::IfcRelDefinesByProperties => property_relations.push((id, start, end)),
            IfcType::IfcRelDefinesByType => type_relations.push((start, end)),
            _ => {}
        }
    }
    let Some(&(element_start, element_end)) = spans.get(&express_id) else {
        return Err(PropertyWriteError::MissingElement);
    };

    let mut decoder = EntityDecoder::new(source);
    let mut definitions_of: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut links: HashMap<u32, RelationLink> = HashMap::new();
    for (relation_id, start, end) in property_relations {
        let Ok(relation) = decoder.decode_at(start, end) else {
            continue;
        };
        let Some(definitions) =
            relation.ref_list_at(slot::IfcRelDefinesByProperties::RELATING_PROPERTY_DEFINITION)
        else {
            continue;
        };
        let Some(objects) = relation.ref_list_at(slot::IfcRelDefinesByProperties::RELATED_OBJECTS)
        else {
            continue;
        };
        if objects.contains(&express_id) {
            for definition_id in &definitions {
                links.entry(*definition_id).or_insert(RelationLink {
                    relation_id,
                    related_objects: objects.len(),
                });
            }
        }
        for object in objects {
            definitions_of
                .entry(object)
                .or_default()
                .extend_from_slice(&definitions);
        }
    }

    let mut type_id = None;
    for (start, end) in type_relations {
        let Ok(relation) = decoder.decode_at(start, end) else {
            continue;
        };
        let Some(relating) = relation.ref_at(slot::IfcRelDefinesByType::RELATING_TYPE) else {
            continue;
        };
        let Some(objects) = relation.ref_list_at(slot::IfcRelDefinesByType::RELATED_OBJECTS) else {
            continue;
        };
        if objects.contains(&express_id) {
            type_id.get_or_insert(relating);
        }
    }

    let mut counts: HashMap<String, u32> = HashMap::new();
    // extract_elements lists Element.Tag before every property set
    let has_tag = decoder
        .decode_at(element_start, element_end)
        .is_ok_and(|element| {
            element.ifc_type.is_subtype_of(IfcType::IfcElement)
                && element
                    .get_string(slot::IfcElement::TAG.index())
                    .is_some_and(|tag| !tag.is_empty())
        });
    if has_tag {
        numbered_key(&mut counts, ELEMENT_TAG_KEY.to_string());
    }
    let mut property_decoder = new_property_decoder(source.len());
    let mut targets: HashMap<String, Target> = HashMap::new();
    let mut seen_definitions = HashSet::new();
    for definition_id in definitions_of.get(&express_id).into_iter().flatten() {
        if !seen_definitions.insert(*definition_id) {
            continue;
        }
        let Some(definition) =
            definition_rows(&mut decoder, &mut property_decoder, &spans, *definition_id)
        else {
            continue;
        };
        for row in definition.rows {
            let target = match row.value {
                Some(value) => Target::Value(WritableValue {
                    member_id: value.member_id,
                    slot: value.slot,
                    definition_id: *definition_id,
                    members_slot: definition.members_slot,
                }),
                None => Target::Unwritable,
            };
            targets.insert(numbered_key(&mut counts, row.key), target);
        }
    }

    let mut type_keys = HashSet::new();
    if let Some(type_id) = type_id
        && let Some(&(start, end)) = spans.get(&type_id)
        && let Ok(type_object) = decoder.decode_at(start, end)
        && type_object.ifc_type.is_subtype_of(IfcType::IfcTypeObject)
    {
        let mut type_definitions = type_object
            .ref_list_at(slot::IfcTypeObject::HAS_PROPERTY_SETS)
            .unwrap_or_default();
        if let Some(related) = definitions_of.get(&type_id) {
            type_definitions.extend_from_slice(related);
        }
        for definition_id in type_definitions {
            if !seen_definitions.insert(definition_id) {
                continue;
            }
            let Some(definition) =
                definition_rows(&mut decoder, &mut property_decoder, &spans, definition_id)
            else {
                continue;
            };
            for row in definition.rows {
                if !counts.contains_key(&row.key) {
                    type_keys.insert(row.key);
                }
            }
        }
    }

    let Some(target) = targets.get(key) else {
        return Err(if type_keys.contains(key) {
            PropertyWriteError::DefinedOnType
        } else {
            PropertyWriteError::MissingProperty
        });
    };
    let Target::Value(target) = *target else {
        return Err(PropertyWriteError::NotASingleValue);
    };
    let Some(&(record_start, record_end)) = spans.get(&target.member_id) else {
        return Err(PropertyWriteError::MissingProperty);
    };
    let Some((attribute_start, attribute_end)) =
        attribute_span(source, record_start, record_end, target.slot)
    else {
        return Err(PropertyWriteError::KindMismatch);
    };
    let literal = literal_span(source, attribute_start, attribute_end)?;
    let replacement = replacement_bytes(source, &literal, value)?;
    let link = links
        .get(&target.definition_id)
        .ok_or(PropertyWriteError::UnreadableEntity)?;
    write_value(
        source,
        express_id,
        &target,
        link,
        literal_range(&literal),
        &replacement,
    )
}

fn definition_rows(
    decoder: &mut EntityDecoder<'_>,
    property_decoder: &mut PropertyDecoder,
    spans: &HashMap<u32, (usize, usize)>,
    definition_id: u32,
) -> Option<DefinitionRows> {
    let &(start, end) = spans.get(&definition_id)?;
    let definition = decoder.decode_at(start, end).ok()?;
    match definition.ifc_type {
        IfcType::IfcPropertySet => {
            let members = definition
                .ref_list_at(slot::IfcPropertySet::HAS_PROPERTIES)
                .unwrap_or_default();
            // extract_elements skips a set with more members than this
            if members.len() > MAX_PROPERTIES_PER_ELEMENT {
                return None;
            }
            let set = property_set_rows(property_decoder, decoder, &definition)?;
            Some(DefinitionRows {
                members_slot: slot::IfcPropertySet::HAS_PROPERTIES.index(),
                rows: set
                    .rows
                    .into_iter()
                    .map(|row| DefinitionRow {
                        key: property_key(&set.name, &row.value.name),
                        value: row.single_value_id.map(|member_id| ValueSlot {
                            member_id,
                            slot: SINGLE_VALUE_SLOT,
                        }),
                    })
                    .collect(),
            })
        }
        IfcType::IfcElementQuantity => {
            let members = definition
                .ref_list_at(slot::IfcElementQuantity::QUANTITIES)
                .unwrap_or_default();
            if members.len() > MAX_PROPERTIES_PER_ELEMENT {
                return None;
            }
            let set = decode_quantity_records(
                decoder,
                &definition,
                None,
                QuantityDecodeMode::AuthoredAnalysis,
            )?;
            Some(DefinitionRows {
                members_slot: slot::IfcElementQuantity::QUANTITIES.index(),
                rows: set
                    .records
                    .iter()
                    .filter(|record| record.value.value.is_finite())
                    .map(|record| DefinitionRow {
                        key: property_key(&set.name, &record.value.name),
                        value: Some(ValueSlot {
                            member_id: record.id,
                            slot: QUANTITY_VALUE_SLOT,
                        }),
                    })
                    .collect(),
            })
        }
        _ => None,
    }
}

fn write_value(
    source: &[u8],
    element_id: u32,
    target: &WritableValue,
    link: &RelationLink,
    literal: Range<usize>,
    replacement: &[u8],
) -> Result<Vec<u8>, PropertyWriteError> {
    let definition_shared =
        link.related_objects > 1 || reference_count(source, target.definition_id) > 1;
    if !definition_shared && reference_count(source, target.member_id) <= 1 {
        let mut rewritten = Vec::with_capacity(source.len() - literal.len() + replacement.len());
        rewritten.extend_from_slice(&source[..literal.start]);
        rewritten.extend_from_slice(replacement);
        rewritten.extend_from_slice(&source[literal.end..]);
        return Ok(rewritten);
    }

    let member =
        entity_span(source, target.member_id).ok_or(PropertyWriteError::UnreadableEntity)?;
    let mut changed_member = source[member.start..literal.start].to_vec();
    changed_member.extend_from_slice(replacement);
    changed_member.extend_from_slice(&source[literal.end..member.end]);
    let mut next_id = max_id(source) + 1;
    let member_copy_id = next_id;
    next_id += 1;
    let mut block = renumbered(&changed_member, member_copy_id)?;
    let point_at_member_copy =
        |members: &[u8]| swapped_reference(members, target.member_id, member_copy_id);
    if !definition_shared {
        let with_copy = insert_after(source, target.definition_id, &block)?;
        return change_argument(
            &with_copy,
            target.definition_id,
            target.members_slot,
            point_at_member_copy,
        );
    }

    let definition_copy_id = next_id;
    next_id += 1;
    let definition =
        entity_span(source, target.definition_id).ok_or(PropertyWriteError::UnreadableEntity)?;
    let definition_copy = with_new_global_id(&source[definition], definition_copy_id)?;
    block.extend(
        with_argument(&definition_copy, target.members_slot, point_at_member_copy)
            .ok_or(PropertyWriteError::UnreadableEntity)?,
    );
    let relating = slot::IfcRelDefinesByProperties::RELATING_PROPERTY_DEFINITION.index();
    let point_at_definition_copy = |definitions: &[u8]| {
        swapped_reference(definitions, target.definition_id, definition_copy_id)
    };
    if link.related_objects == 1 {
        let with_copies = insert_after(source, link.relation_id, &block)?;
        return change_argument(
            &with_copies,
            link.relation_id,
            relating,
            point_at_definition_copy,
        );
    }

    let related_objects = slot::IfcRelDefinesByProperties::RELATED_OBJECTS.index();
    let relation =
        entity_span(source, link.relation_id).ok_or(PropertyWriteError::UnreadableEntity)?;
    let relation_copy = with_new_global_id(&source[relation], next_id)?;
    let relation_copy = with_argument(&relation_copy, related_objects, |_| {
        Some(format!("(#{element_id})").into_bytes())
    })
    .and_then(|copy| with_argument(&copy, relating, point_at_definition_copy))
    .ok_or(PropertyWriteError::UnreadableEntity)?;
    block.extend(relation_copy);
    // extract_elements reads property sets in relation order
    let with_copies = insert_after(source, link.relation_id, &block)?;
    change_argument(&with_copies, link.relation_id, related_objects, |objects| {
        strip_one_ref(objects, element_id)
    })
}

fn renumbered(entity: &[u8], id: u32) -> Result<Vec<u8>, PropertyWriteError> {
    let entity = entity.trim_ascii();
    let body = entity
        .iter()
        .position(|byte| *byte == b'=')
        .ok_or(PropertyWriteError::UnreadableEntity)?;
    let mut copy = format!("#{id}").into_bytes();
    copy.extend_from_slice(&entity[body..]);
    copy.push(b'\n');
    Ok(copy)
}

fn with_new_global_id(entity: &[u8], id: u32) -> Result<Vec<u8>, PropertyWriteError> {
    let copy = renumbered(entity, id)?;
    let seed = format!("{}#{id}", String::from_utf8_lossy(entity));
    let global_id = ifc_lite_core::guid::deterministic(&seed);
    with_argument(&copy, 0, |_| Some(format!("'{global_id}'").into_bytes()))
        .ok_or(PropertyWriteError::UnreadableEntity)
}

fn with_argument(
    entity: &[u8],
    index: usize,
    change: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let argument = argument_span(entity, index)?;
    let changed = change(&entity[argument.clone()])?;
    let mut next = entity[..argument.start].to_vec();
    next.extend_from_slice(&changed);
    next.extend_from_slice(&entity[argument.end..]);
    Some(next)
}

fn change_argument(
    source: &[u8],
    entity_id: u32,
    index: usize,
    change: impl FnOnce(&[u8]) -> Option<Vec<u8>>,
) -> Result<Vec<u8>, PropertyWriteError> {
    let span = entity_span(source, entity_id).ok_or(PropertyWriteError::UnreadableEntity)?;
    let changed = with_argument(&source[span.clone()], index, change)
        .ok_or(PropertyWriteError::UnreadableEntity)?;
    let mut next = source[..span.start].to_vec();
    next.extend_from_slice(&changed);
    next.extend_from_slice(&source[span.end..]);
    Ok(next)
}

fn insert_after(
    source: &[u8],
    entity_id: u32,
    block: &[u8],
) -> Result<Vec<u8>, PropertyWriteError> {
    let span = entity_span(source, entity_id).ok_or(PropertyWriteError::UnreadableEntity)?;
    let mut next = Vec::with_capacity(source.len() + block.len() + 1);
    next.extend_from_slice(&source[..span.end]);
    if !next.ends_with(b"\n") {
        next.push(b'\n');
    }
    next.extend_from_slice(block);
    next.extend_from_slice(&source[span.end..]);
    Ok(next)
}

fn swapped_reference(argument: &[u8], from: u32, to: u32) -> Option<Vec<u8>> {
    let from_length = from.to_string().len() + 1;
    let mut next = Vec::with_capacity(argument.len() + 4);
    let mut found = false;
    let mut cursor = 0;
    while cursor < argument.len() {
        if argument[cursor] == b'#' && exact_ref_at(argument, cursor, from) {
            next.extend_from_slice(format!("#{to}").as_bytes());
            cursor += from_length;
            found = true;
            continue;
        }
        next.push(argument[cursor]);
        cursor += 1;
    }
    found.then_some(next)
}

fn literal_range(literal: &LiteralSpan) -> Range<usize> {
    match literal {
        LiteralSpan::Text { start, end }
        | LiteralSpan::Number { start, end, .. }
        | LiteralSpan::Enumeration { start, end } => *start..*end,
    }
}

fn bytes_offset(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let offset = inner.as_ptr().addr().checked_sub(outer.as_ptr().addr())?;
    let end = offset.checked_add(inner.len())?;
    (end <= outer.len()).then_some(offset)
}

fn attribute_span(
    source: &[u8],
    record_start: usize,
    record_end: usize,
    slot_index: usize,
) -> Option<(usize, usize)> {
    let record = source.get(record_start..record_end)?;
    let item = StepListItems::of_record(record)?.nth(slot_index)?;
    let offset = bytes_offset(record, item)?;
    let item_end = offset.checked_add(item.len())?;
    if item_end > record.len() {
        return None;
    }
    Some((record_start + offset, record_start + item_end))
}

fn literal_span(
    source: &[u8],
    start: usize,
    end: usize,
) -> Result<LiteralSpan, PropertyWriteError> {
    let at = skip_trivia(source, start).ok_or(PropertyWriteError::EmptyNominal)?;
    if at >= end {
        return Err(PropertyWriteError::EmptyNominal);
    }
    match source[at] {
        b'$' | b'*' => Err(PropertyWriteError::EmptyNominal),
        b'\'' => {
            let close =
                step_string_literal_end(source, at).ok_or(PropertyWriteError::KindMismatch)?;
            if close > end {
                return Err(PropertyWriteError::KindMismatch);
            }
            Ok(LiteralSpan::Text {
                start: at,
                end: close,
            })
        }
        b'.' => {
            let close = enumeration_end(source, at, end).ok_or(PropertyWriteError::KindMismatch)?;
            Ok(LiteralSpan::Enumeration {
                start: at,
                end: close,
            })
        }
        byte if byte.is_ascii_alphabetic() => {
            let keyword_end =
                keyword_end(source, at, end).ok_or(PropertyWriteError::KindMismatch)?;
            let paren = skip_trivia(source, keyword_end).ok_or(PropertyWriteError::KindMismatch)?;
            if paren >= end || source[paren] != b'(' {
                return Err(PropertyWriteError::KindMismatch);
            }
            let mut items = StepListItems::of_list(&source[paren..end])
                .ok_or(PropertyWriteError::KindMismatch)?;
            let item = items.next().ok_or(PropertyWriteError::EmptyNominal)?;
            if item.is_empty() {
                return Err(PropertyWriteError::EmptyNominal);
            }
            let item_start = bytes_offset(source, item).ok_or(PropertyWriteError::KindMismatch)?;
            literal_span(source, item_start, item_start + item.len())
        }
        _ => {
            let (number, length) =
                read_step_number(&source[at..end]).ok_or(PropertyWriteError::KindMismatch)?;
            Ok(LiteralSpan::Number {
                start: at,
                end: at + length,
                integer: matches!(number, StepNumber::Integer(_)),
            })
        }
    }
}

fn replacement_bytes(
    source: &[u8],
    literal: &LiteralSpan,
    value: &PropertyLiteral,
) -> Result<Vec<u8>, PropertyWriteError> {
    let text = match (literal, value) {
        (LiteralSpan::Text { .. }, PropertyLiteral::Text(text)) => step_string(text),
        (LiteralSpan::Number { integer: true, .. }, PropertyLiteral::Number(number)) => {
            integer_token(*number)?
        }
        (LiteralSpan::Number { integer: false, .. }, PropertyLiteral::Number(number)) => {
            if !number.is_finite() {
                return Err(PropertyWriteError::KindMismatch);
            }
            real_token(*number)
        }
        (LiteralSpan::Enumeration { start, end }, PropertyLiteral::Boolean(boolean)) => {
            let word = std::str::from_utf8(&source[start + 1..end - 1])
                .map_err(|_| PropertyWriteError::KindMismatch)?;
            if !is_boolean_word(word) {
                return Err(PropertyWriteError::KindMismatch);
            }
            if *boolean {
                ".T.".to_string()
            } else {
                ".F.".to_string()
            }
        }
        _ => return Err(PropertyWriteError::KindMismatch),
    };
    Ok(text.into_bytes())
}

fn integer_token(number: f64) -> Result<String, PropertyWriteError> {
    if !number.is_finite() {
        return Err(PropertyWriteError::KindMismatch);
    }
    let integer = number as i64;
    if integer as f64 != number {
        return Err(PropertyWriteError::IntegerValue);
    }
    Ok(integer.to_string())
}

fn real_token(number: f64) -> String {
    let rendered = number.to_string();
    if rendered.contains('.') || rendered.contains('e') || rendered.contains('E') {
        rendered
    } else {
        format!("{rendered}.")
    }
}

fn step_string(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len() + 2);
    encoded.push('\'');
    let mut wide = String::new();
    for character in text.chars() {
        // iso 10303-21 strings hold printable ascii only
        if !(' '..='~').contains(&character) {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                wide.push_str(&format!("{unit:04X}"));
            }
            continue;
        }
        close_wide_run(&mut encoded, &mut wide);
        if character == '\'' || character == '\\' {
            encoded.push(character);
        }
        encoded.push(character);
    }
    close_wide_run(&mut encoded, &mut wide);
    encoded.push('\'');
    encoded
}

fn close_wide_run(encoded: &mut String, wide: &mut String) {
    if wide.is_empty() {
        return;
    }
    encoded.push_str("\\X2\\");
    encoded.push_str(wide);
    encoded.push_str("\\X0\\");
    wide.clear();
}

fn is_boolean_word(word: &str) -> bool {
    word.eq_ignore_ascii_case("T")
        || word.eq_ignore_ascii_case("F")
        || word.eq_ignore_ascii_case("TRUE")
        || word.eq_ignore_ascii_case("FALSE")
}

fn skip_trivia(bytes: &[u8], mut position: usize) -> Option<usize> {
    loop {
        while matches!(bytes.get(position), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            position += 1;
        }
        if bytes.get(position) == Some(&b'/') && bytes.get(position + 1) == Some(&b'*') {
            position = skip_step_comment(bytes, position)?;
            continue;
        }
        return Some(position);
    }
}

fn keyword_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if !bytes.get(start).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let mut position = start + 1;
    while position < limit && bytes[position].is_ascii_alphanumeric() {
        position += 1;
    }
    Some(position)
}

fn enumeration_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'.') {
        return None;
    }
    let mut position = start + 1;
    while position < limit && bytes[position] != b'.' {
        if bytes[position].is_ascii_whitespace() {
            return None;
        }
        position += 1;
    }
    (position < limit && bytes[position] == b'.').then_some(position + 1)
}

#[cfg(test)]
mod tests {
    use super::{MAX_TEXT_BYTES, PropertyLiteral, PropertyWriteError, rewrite_property};
    use crate::ifc_elements::{ModelOrigin, PropertyValue, extract_elements};
    use std::collections::HashMap;

    fn file(data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\nFILE_NAME('','',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n#1=IFCPROJECT('project0000000000000000',$,'P',$,$,$,$,$,$);\n{data}ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn read_properties(source: &[u8]) -> HashMap<u32, HashMap<String, PropertyValue>> {
        let origin = ModelOrigin {
            longitude: 0.0,
            latitude: 0.0,
            height: 0.0,
            x_axis_from_east_radians: 0.0,
        };
        let mut properties = HashMap::new();
        extract_elements(source, &HashMap::new(), origin, None, |element| {
            properties.insert(
                element.express_id,
                element
                    .properties
                    .into_iter()
                    .map(|property| (property.key, property.value))
                    .collect(),
            );
            Ok(())
        })
        .unwrap();
        properties
    }

    fn text(value: &str) -> PropertyValue {
        PropertyValue::Text(value.to_string())
    }

    #[test]
    fn a_property_set_two_walls_share_changes_on_the_edited_wall_only() {
        let source = file(
            "#30=IFCWALL('0wall00000000000000030',$,'A',$,$,$,$,$,$);\n\
             #31=IFCWALL('0wall00000000000000031',$,'B',$,$,$,$,$,$);\n\
             #43=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);\n\
             #44=IFCPROPERTYSET('0pset00000000000000044',$,'Pset_WallCommon',$,(#43));\n\
             #47=IFCRELDEFINESBYPROPERTIES('0rel000000000000000047',$,$,$,(#30,#31),#44);\n",
        );
        let rewritten = rewrite_property(
            source.as_bytes(),
            30,
            "Pset_WallCommon.FireRating",
            &PropertyLiteral::Text("REI90".to_string()),
        )
        .unwrap();
        let properties = read_properties(&rewritten);
        assert_eq!(properties[&30]["Pset_WallCommon.FireRating"], text("REI90"));
        assert_eq!(properties[&31]["Pset_WallCommon.FireRating"], text("REI60"));
    }

    #[test]
    fn a_value_two_property_sets_share_changes_in_the_edited_set_only() {
        let source = file(
            "#30=IFCWALL('0wall00000000000000030',$,'A',$,$,$,$,$,$);\n\
             #31=IFCWALL('0wall00000000000000031',$,'B',$,$,$,$,$,$);\n\
             #43=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);\n\
             #44=IFCPROPERTYSET('0pset00000000000000044',$,'Pset_WallCommon',$,(#43));\n\
             #45=IFCPROPERTYSET('0pset00000000000000045',$,'Pset_WallCommon',$,(#43));\n\
             #47=IFCRELDEFINESBYPROPERTIES('0rel000000000000000047',$,$,$,(#30),#44);\n\
             #48=IFCRELDEFINESBYPROPERTIES('0rel000000000000000048',$,$,$,(#31),#45);\n",
        );
        let rewritten = rewrite_property(
            source.as_bytes(),
            31,
            "Pset_WallCommon.FireRating",
            &PropertyLiteral::Text("EI30".to_string()),
        )
        .unwrap();
        let properties = read_properties(&rewritten);
        assert_eq!(properties[&30]["Pset_WallCommon.FireRating"], text("REI60"));
        assert_eq!(properties[&31]["Pset_WallCommon.FireRating"], text("EI30"));
    }

    #[test]
    fn a_property_set_the_wall_shares_with_its_type_changes_on_the_wall_only() {
        let source = file(
            "#30=IFCWALL('0wall00000000000000030',$,'A',$,$,$,$,$,$);\n\
             #31=IFCWALL('0wall00000000000000031',$,'B',$,$,$,$,$,$);\n\
             #33=IFCWALLTYPE('0type00000000000000033',$,'T',$,$,(#44),$,$,$,.STANDARD.);\n\
             #34=IFCRELDEFINESBYTYPE('0rel000000000000000034',$,$,$,(#31),#33);\n\
             #43=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);\n\
             #44=IFCPROPERTYSET('0pset00000000000000044',$,'Pset_WallCommon',$,(#43));\n\
             #47=IFCRELDEFINESBYPROPERTIES('0rel000000000000000047',$,$,$,(#30),#44);\n",
        );
        let rewritten = rewrite_property(
            source.as_bytes(),
            30,
            "Pset_WallCommon.FireRating",
            &PropertyLiteral::Text("REI90".to_string()),
        )
        .unwrap();
        let properties = read_properties(&rewritten);
        assert_eq!(properties[&30]["Pset_WallCommon.FireRating"], text("REI90"));
        assert_eq!(properties[&31]["Pset_WallCommon.FireRating"], text("REI60"));
    }

    #[test]
    fn the_writer_numbers_repeated_keys_as_the_element_reader_does() {
        let source = file(
            "#30=IFCWALL('0wall00000000000000030',$,'A',$,$,$,$,'T-1',$);\n\
             #40=IFCPROPERTYSINGLEVALUE('Tag',$,IFCLABEL('from the set'),$);\n\
             #41=IFCPROPERTYSET('0pset00000000000000041',$,'Element',$,(#40));\n\
             #42=IFCRELDEFINESBYPROPERTIES('0rel000000000000000042',$,$,$,(#30),#41);\n\
             #43=IFCPROPERTYSINGLEVALUE('FireRating',$,#1,$);\n\
             #44=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);\n\
             #45=IFCPROPERTYSET('0pset00000000000000045',$,'Pset_WallCommon',$,(#43,#44));\n\
             #46=IFCRELDEFINESBYPROPERTIES('0rel000000000000000046',$,$,$,(#30),#45);\n",
        );
        let before = read_properties(source.as_bytes());
        assert_eq!(before[&30]["Element.Tag"], text("T-1"));
        assert_eq!(before[&30]["Element.Tag[1]"], text("from the set"));
        assert_eq!(before[&30]["Pset_WallCommon.FireRating"], text("REI60"));

        let tag = rewrite_property(
            source.as_bytes(),
            30,
            "Element.Tag[1]",
            &PropertyLiteral::Text("edited".to_string()),
        )
        .unwrap();
        assert_eq!(read_properties(&tag)[&30]["Element.Tag[1]"], text("edited"));
        let rating = rewrite_property(
            source.as_bytes(),
            30,
            "Pset_WallCommon.FireRating",
            &PropertyLiteral::Text("REI90".to_string()),
        )
        .unwrap();
        assert_eq!(
            read_properties(&rating)[&30]["Pset_WallCommon.FireRating"],
            text("REI90")
        );
    }

    #[test]
    fn text_outside_printable_ascii_is_written_as_x2_escapes_and_reads_back() {
        let edited = "T\u{fc}r \u{20ac}5 \u{1F600} it's a\\b";
        let rewritten = rewrite(
            30,
            "Pset_WallCommon.FireRating",
            PropertyLiteral::Text(edited.to_string()),
        )
        .unwrap();
        assert!(rewritten.is_ascii());
        let written = String::from_utf8(rewritten.clone()).unwrap();
        assert!(
            written.contains(
                "IFCLABEL('T\\X2\\00FC\\X0\\r \\X2\\20AC\\X0\\5 \\X2\\D83DDE00\\X0\\ it''s a\\\\b')"
            ),
            "{written}"
        );
        assert_eq!(
            read_properties(&rewritten)[&30]["Pset_WallCommon.FireRating"],
            text(edited)
        );
    }

    const MODEL: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');
FILE_NAME('small.ifc','2026-11-12',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#30=IFCWALL('0wall$outer0000000000a',$,'Exterior Wall',$,$,$,$,'SENSOR-17',.SOLIDWALL.);
#31=IFCWALL('0wall_inner0000000000a',$,'Interior Wall',$,$,$,$,$,.SOLIDWALL.);
#32=IFCDOOR('0door00000000000000000',$,'Fire Door',$,$,$,$,$,$,$,.DOOR.,.SINGLE_SWING_LEFT.,$);
#33=IFCDOORTYPE('0doortype0000000000000',$,'Steel Door',$,$,(#45),$,$,$,.DOOR.,.SINGLE_SWING_LEFT.,.F.,$);
#43=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);
#46=IFCPROPERTYSINGLEVALUE('IsExternal',$,IFCBOOLEAN(.T.),$);
#70=IFCPROPERTYENUMERATEDVALUE('Status',$,(IFCLABEL('Open')),$);
#71=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('A''B'),$);
#72=IFCPROPERTYSINGLEVALUE('Count',$,IFCINTEGER(3),$);
#73=IFCPROPERTYSINGLEVALUE('Blank',$,IFCLABEL($),$);
#44=IFCPROPERTYSET('0pset00000000000000001',$,'Pset_WallCommon',$,(#43,#46,#70,#71,#72,#73));
#47=IFCRELDEFINESBYPROPERTIES('0rel000000000000000007',$,$,$,(#30),#44);
#48=IFCQUANTITYLENGTH('Length',$,$,4000.,$);
#49=IFCELEMENTQUANTITY('0qset00000000000000001',$,'Qto_WallBaseQuantities',$,$,(#48));
#50=IFCRELDEFINESBYPROPERTIES('0rel000000000000000008',$,$,$,(#30),#49);
#52=IFCQUANTITYLENGTH('Length',$,$,2500.,$);
#53=IFCELEMENTQUANTITY('0qset00000000000000002',$,'Qto_WallBaseQuantities',$,$,(#52));
#54=IFCRELDEFINESBYPROPERTIES('0rel000000000000000009',$,$,$,(#31),#53);
#45=IFCPROPERTYSET('0pset00000000000000002',$,'Pset_DoorCommon',$,(#51));
#51=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('EI30'),$);
#42=IFCRELDEFINESBYTYPE('0rel000000000000000006',$,$,$,(#32),#33);
#74=IFCPROPERTYSINGLEVALUE('Note',$,IFCLOGICAL(.U.),$);
#75=IFCPROPERTYSET('0pset00000000000000003',$,'Pset_Note',$,(#74));
#76=IFCRELDEFINESBYPROPERTIES('0rel000000000000000011',$,$,$,(#30),#75);
ENDSEC;
END-ISO-10303-21;
";

    fn rewrite(
        express_id: u32,
        key: &str,
        value: PropertyLiteral,
    ) -> Result<Vec<u8>, PropertyWriteError> {
        rewrite_property(MODEL.as_bytes(), express_id, key, &value)
    }

    #[test]
    fn rewrites_a_label_a_boolean_and_a_real_in_place() {
        let text = rewrite(
            30,
            "Pset_WallCommon.FireRating",
            PropertyLiteral::Text("REI90".to_string()),
        )
        .unwrap();
        let text = String::from_utf8(text).unwrap();
        assert_eq!(text.matches("IFCLABEL('REI90')").count(), 1);
        assert!(!text.contains("IFCLABEL('REI60')"));
        assert_eq!(text.matches("IFCPROPERTYSET(").count(), 3);

        let boolean = rewrite(
            30,
            "Pset_WallCommon.IsExternal",
            PropertyLiteral::Boolean(false),
        )
        .unwrap();
        let boolean = String::from_utf8(boolean).unwrap();
        assert!(boolean.contains("IFCBOOLEAN(.F.)"));
        assert!(!boolean.contains("IFCBOOLEAN(.T.)"));

        let length = rewrite(
            30,
            "Qto_WallBaseQuantities.Length",
            PropertyLiteral::Number(4100.0),
        )
        .unwrap();
        let length = String::from_utf8(length).unwrap();
        assert!(length.contains("4100."));
        assert!(length.contains("2500."));
        assert!(!length.contains("4000."));
    }

    #[test]
    fn quotes_text_and_keeps_the_second_copy_suffixed() {
        let quoted = rewrite(
            30,
            "Pset_WallCommon.FireRating[1]",
            PropertyLiteral::Text("it's\\x".to_string()),
        )
        .unwrap();
        let quoted = String::from_utf8(quoted).unwrap();
        assert!(quoted.contains("IFCLABEL('it''s\\\\x')"));
        assert!(quoted.contains("IFCLABEL('REI60')"));
    }

    #[test]
    fn refuses_the_cases_that_must_not_change_the_file() {
        assert_eq!(
            rewrite(
                30,
                "Pset_WallCommon.FireRating",
                PropertyLiteral::Number(1.0)
            ),
            Err(PropertyWriteError::KindMismatch)
        );
        assert_eq!(
            rewrite(30, "Pset_WallCommon.Count", PropertyLiteral::Number(3.5)),
            Err(PropertyWriteError::IntegerValue)
        );
        assert_eq!(
            rewrite(30, "Pset_WallCommon.Count", PropertyLiteral::Number(4.0))
                .map(|bytes| String::from_utf8(bytes).unwrap().contains("IFCINTEGER(4)")),
            Ok(true)
        );
        assert_eq!(
            rewrite(
                30,
                "Pset_WallCommon.Status",
                PropertyLiteral::Text("Closed".to_string())
            ),
            Err(PropertyWriteError::NotASingleValue)
        );
        assert_eq!(
            rewrite(
                30,
                "Pset_WallCommon.Blank",
                PropertyLiteral::Text("x".to_string())
            ),
            Err(PropertyWriteError::MissingProperty)
        );
        assert_eq!(
            rewrite(
                32,
                "Pset_DoorCommon.FireRating",
                PropertyLiteral::Text("EI90".to_string())
            ),
            Err(PropertyWriteError::DefinedOnType)
        );
        assert_eq!(
            rewrite(
                30,
                "Element.Tag",
                PropertyLiteral::Text("other".to_string())
            ),
            Err(PropertyWriteError::MissingProperty)
        );
        assert_eq!(
            rewrite(
                30,
                "Pset_DoorCommon.FireRating[1]",
                PropertyLiteral::Text("x".to_string())
            ),
            Err(PropertyWriteError::MissingProperty)
        );
        assert_eq!(
            rewrite(
                99,
                "Pset_WallCommon.FireRating",
                PropertyLiteral::Text("x".to_string())
            ),
            Err(PropertyWriteError::MissingElement)
        );
        assert_eq!(
            rewrite(30, "Pset_Note.Note", PropertyLiteral::Boolean(true)),
            Err(PropertyWriteError::KindMismatch)
        );
        assert_eq!(
            rewrite(
                30,
                "Pset_WallCommon.FireRating",
                PropertyLiteral::Text("a\nb".to_string())
            ),
            Err(PropertyWriteError::ControlCharacter)
        );
        assert_eq!(
            rewrite(
                30,
                "Pset_WallCommon.FireRating",
                PropertyLiteral::Text("x".repeat(MAX_TEXT_BYTES + 1))
            ),
            Err(PropertyWriteError::TextTooLong)
        );
    }
}
