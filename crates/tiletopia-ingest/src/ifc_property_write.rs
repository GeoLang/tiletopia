use ifc_lite_core::parser::StepNumber;
use ifc_lite_core::step_encoding::step_string_literal_end;
use ifc_lite_core::{
    AttributeValue, EntityDecoder, EntityScanner, IfcType, StepListItems, ifc_type_from_keyword,
    read_step_number, skip_step_comment, slot,
};
use std::collections::{HashMap, HashSet};

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
}

enum Member {
    Writable { name: String, slot: usize },
    Unwritable { name: String },
}

enum Target {
    Value { express_id: u32, slot: usize },
    Unwritable,
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
            IfcType::IfcRelDefinesByProperties => property_relations.push((start, end)),
            IfcType::IfcRelDefinesByType => type_relations.push((start, end)),
            _ => {}
        }
    }
    if !spans.contains_key(&express_id) {
        return Err(PropertyWriteError::MissingElement);
    }

    let mut decoder = EntityDecoder::new(source);
    let mut definitions_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (start, end) in property_relations {
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
    let mut targets: HashMap<String, Target> = HashMap::new();
    let mut seen_definitions = HashSet::new();
    for definition_id in definitions_of.get(&express_id).into_iter().flatten() {
        if !seen_definitions.insert(*definition_id) {
            continue;
        }
        collect_definition(
            &mut decoder,
            &spans,
            *definition_id,
            &mut counts,
            &mut targets,
        );
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
            for name in definition_base_keys(&mut decoder, &spans, definition_id) {
                if counts.contains_key(&name) {
                    continue;
                }
                type_keys.insert(name);
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
    let Target::Value { express_id, slot } = *target else {
        return Err(PropertyWriteError::NotASingleValue);
    };
    let Some(&(record_start, record_end)) = spans.get(&express_id) else {
        return Err(PropertyWriteError::MissingProperty);
    };
    let Some((attribute_start, attribute_end)) =
        attribute_span(source, record_start, record_end, slot)
    else {
        return Err(PropertyWriteError::KindMismatch);
    };
    let literal = literal_span(source, attribute_start, attribute_end)?;
    let replacement = replacement_bytes(source, &literal, value)?;
    let (start, end) = literal_range(&literal);
    let mut rewritten = Vec::with_capacity(source.len() - (end - start) + replacement.len());
    rewritten.extend_from_slice(&source[..start]);
    rewritten.extend_from_slice(&replacement);
    rewritten.extend_from_slice(&source[end..]);
    Ok(rewritten)
}

fn literal_range(literal: &LiteralSpan) -> (usize, usize) {
    match literal {
        LiteralSpan::Text { start, end }
        | LiteralSpan::Number { start, end, .. }
        | LiteralSpan::Enumeration { start, end } => (*start, *end),
    }
}

fn collect_definition(
    decoder: &mut EntityDecoder<'_>,
    spans: &HashMap<u32, (usize, usize)>,
    definition_id: u32,
    counts: &mut HashMap<String, u32>,
    targets: &mut HashMap<String, Target>,
) {
    let Some(&(start, end)) = spans.get(&definition_id) else {
        return;
    };
    let Ok(definition) = decoder.decode_at(start, end) else {
        return;
    };
    let (set_name, member_ids) = match definition.ifc_type {
        IfcType::IfcPropertySet => (
            definition
                .string_at(slot::IfcPropertySet::NAME)
                .unwrap_or("")
                .to_string(),
            definition
                .ref_list_at(slot::IfcPropertySet::HAS_PROPERTIES)
                .unwrap_or_default(),
        ),
        IfcType::IfcElementQuantity => (
            definition
                .string_at(slot::IfcElementQuantity::NAME)
                .unwrap_or("")
                .to_string(),
            definition
                .ref_list_at(slot::IfcElementQuantity::QUANTITIES)
                .unwrap_or_default(),
        ),
        _ => return,
    };
    for member_id in member_ids {
        let Some(&(member_start, member_end)) = spans.get(&member_id) else {
            continue;
        };
        let Ok(member) = decoder.decode_at(member_start, member_end) else {
            continue;
        };
        let Some(classified) = classify_member(&definition, &member) else {
            continue;
        };
        let name = match &classified {
            Member::Writable { name, .. } | Member::Unwritable { name } => name,
        };
        let base = property_key(&set_name, name);
        let occurrence = counts.entry(base.clone()).or_insert(0);
        let key = match *occurrence {
            0 => base,
            later => format!("{base}[{later}]"),
        };
        *occurrence += 1;
        let target = match classified {
            Member::Writable { slot, .. } => Target::Value {
                express_id: member_id,
                slot,
            },
            Member::Unwritable { .. } => Target::Unwritable,
        };
        targets.insert(key, target);
    }
}

fn definition_base_keys(
    decoder: &mut EntityDecoder<'_>,
    spans: &HashMap<u32, (usize, usize)>,
    definition_id: u32,
) -> Vec<String> {
    let mut counts = HashMap::new();
    let mut targets = HashMap::new();
    collect_definition(decoder, spans, definition_id, &mut counts, &mut targets);
    let mut names: Vec<String> = counts.into_keys().collect();
    names.sort();
    names
}

fn classify_member(
    definition: &ifc_lite_core::DecodedEntity,
    member: &ifc_lite_core::DecodedEntity,
) -> Option<Member> {
    let name = member.get_string(0).filter(|name| !name.is_empty())?;
    match definition.ifc_type {
        IfcType::IfcPropertySet => classify_property(member, name),
        IfcType::IfcElementQuantity => classify_quantity(member, name),
        _ => None,
    }
}

fn classify_property(member: &ifc_lite_core::DecodedEntity, name: &str) -> Option<Member> {
    match member.ifc_type {
        IfcType::IfcPropertySingleValue => {
            let nominal = member.get(SINGLE_VALUE_SLOT)?;
            if nominal_missing(nominal) {
                return None;
            }
            Some(Member::Writable {
                name: name.to_string(),
                slot: SINGLE_VALUE_SLOT,
            })
        }
        IfcType::IfcPropertyEnumeratedValue
        | IfcType::IfcPropertyListValue
        | IfcType::IfcPropertyBoundedValue
        | IfcType::IfcPropertyTableValue
        | IfcType::IfcPropertyReferenceValue
        | IfcType::IfcComplexProperty => Some(Member::Unwritable {
            name: name.to_string(),
        }),
        _ => None,
    }
}

fn classify_quantity(member: &ifc_lite_core::DecodedEntity, name: &str) -> Option<Member> {
    match member.ifc_type {
        IfcType::IfcQuantityLength
        | IfcType::IfcQuantityArea
        | IfcType::IfcQuantityVolume
        | IfcType::IfcQuantityWeight
        | IfcType::IfcQuantityTime
        | IfcType::IfcQuantityCount
        | IfcType::IfcQuantityNumber => {
            let value = member.get(QUANTITY_VALUE_SLOT)?;
            if !value.as_float().is_some_and(f64::is_finite) {
                return None;
            }
            Some(Member::Writable {
                name: name.to_string(),
                slot: QUANTITY_VALUE_SLOT,
            })
        }
        _ => None,
    }
}

fn nominal_missing(value: &AttributeValue) -> bool {
    value.is_null() || matches!(value, AttributeValue::Typed { value, .. } if value.is_null())
}

fn bytes_offset(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let offset = inner.as_ptr().addr().checked_sub(outer.as_ptr().addr())?;
    let end = offset.checked_add(inner.len())?;
    (end <= outer.len()).then_some(offset)
}

fn property_key(set_name: &str, entry_name: &str) -> String {
    if set_name.is_empty() {
        entry_name.to_string()
    } else {
        format!("{set_name}.{entry_name}")
    }
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
    for character in text.chars() {
        if character == '\'' || character == '\\' {
            encoded.push(character);
        }
        encoded.push(character);
    }
    encoded.push('\'');
    encoded
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
