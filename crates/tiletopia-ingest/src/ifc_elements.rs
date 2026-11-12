use crate::IngestError;
use crate::ifc_reader::MapConversionPlacement;
#[cfg(test)]
use ifc_lite_core::build_entity_index;
use ifc_lite_core::{
    DecodedEntity, EntityDecoder, EntityIndex, IfcType, ProjectUnits, find_project_id,
    resolve_unit_by_ref, slot,
};
use ifc_lite_geometry::GeometryRouter;
use ifc_lite_processing::data_model::property::PropertyDecoder;
use ifc_lite_processing::data_model::quantity::{QuantityDecodeMode, decode_quantity_records};
use ifc_lite_processing::data_model::render::flat::{flat_property, new_property_decoder};
use ifc_lite_processing::data_model::rows::{PropValue, ValueKind};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiletopia_core::spatial::{WGS84_A, WGS84_F, enu_to_ecef_matrix};

pub const WARNING_MISSING_GLOBAL_ID: &str = "missing_global_id";
pub const WARNING_DUPLICATE_GLOBAL_ID: &str = "duplicate_global_id";
pub const WARNING_UNDECODABLE_ELEMENT: &str = "undecodable_element";
pub const WARNING_UNDECODABLE_RELATION: &str = "undecodable_relation";
pub const WARNING_UNDECODABLE_PROPERTY_DEFINITION: &str = "undecodable_property_definition";
pub const WARNING_UNDECODABLE_MATERIAL: &str = "undecodable_material";
pub const WARNING_REJECTED_QUANTITY: &str = "rejected_quantity";
pub const WARNING_NON_FINITE_QUANTITY: &str = "non_finite_quantity";
pub const WARNING_SECOND_SPATIAL_PARENT: &str = "second_spatial_parent";
pub const WARNING_SPATIAL_CYCLE: &str = "spatial_cycle";
pub const WARNING_SPATIAL_CHAIN_TOO_DEEP: &str = "spatial_chain_too_deep";
pub const WARNING_NO_PROJECT: &str = "no_project";
pub const WARNING_UNCONTAINED_ELEMENT: &str = "uncontained_element";
pub const WARNING_PROPERTIES_TRUNCATED: &str = "properties_truncated";
pub const WARNING_TEXT_TRUNCATED: &str = "text_truncated";
pub const WARNING_UNRESOLVED_STOREY_PLACEMENT: &str = "unresolved_storey_placement";
pub const WARNING_UNRESOLVED_BUILDING_PLACEMENT: &str = "unresolved_building_placement";
pub const WARNING_STOREY_HEIGHT_FROM_ELEVATION: &str = "storey_height_from_elevation";
pub const WARNING_STOREY_WITHOUT_HEIGHT: &str = "storey_without_height";

pub type ExtractionWarnings = BTreeMap<&'static str, u64>;

const MAX_SPATIAL_DEPTH: usize = 64;
const MAX_PROPERTIES_PER_ELEMENT: usize = 10_000;
const MAX_STORED_TEXT_BYTES: usize = 64 * 1024;
const MIN_PROPERTY_ROW_BUDGET: u64 = 4_000_000;
// ifc-lite-core's COMPLEX_PROPERTY_BYTES_PER_INPUT_BYTE and ifc-lite-processing's material table allow 16 too
const TEXT_BYTES_PER_INPUT_BYTE: u64 = 16;
const MIN_TEXT_BYTE_BUDGET: u64 = 1 << 30;
const VISITS_PER_PROPERTY_ROW: u64 = 64;
const EXTRACTION_TIME_LIMIT: Duration = Duration::from_secs(600);
const MAX_CACHED_ENTITIES: usize = 1 << 18;

struct ExtractionBudgets {
    property_rows: u64,
    text_bytes: u64,
    visits: u64,
    deadline: Instant,
}

impl ExtractionBudgets {
    fn for_input(input_bytes: usize) -> Self {
        let input_bytes = input_bytes as u64;
        // property rows grow as elements times set size, so a few kilobytes of IFC can name millions
        let property_rows = MIN_PROPERTY_ROW_BUDGET.max(input_bytes);
        Self {
            property_rows,
            text_bytes: MIN_TEXT_BYTE_BUDGET
                .max(input_bytes.saturating_mul(TEXT_BYTES_PER_INPUT_BYTE)),
            visits: property_rows.saturating_mul(VISITS_PER_PROPERTY_ROW),
            deadline: Instant::now() + EXTRACTION_TIME_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelOrigin {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub x_axis_from_east_radians: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ElementLocation {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub radius: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKind {
    Subject,
    Model,
    Element,
}

impl TreeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Model => "model",
            Self::Element => "element",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Text(String),
    Number(f64),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ElementProperty {
    pub key: String,
    pub value: PropertyValue,
    pub units: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedElement {
    pub express_id: u32,
    pub global_id: String,
    pub class: String,
    pub name: Option<String>,
    pub tree_kind: TreeKind,
    pub parent_express_id: Option<u32>,
    pub storey: Option<String>,
    pub location: Option<ElementLocation>,
    pub base_height: Option<f64>,
    pub properties: Vec<ElementProperty>,
}

impl ExtractedElement {
    pub fn text_bytes(&self) -> usize {
        let property_bytes: usize = self
            .properties
            .iter()
            .map(|property| {
                let value_bytes = match &property.value {
                    PropertyValue::Text(text) => text.len(),
                    PropertyValue::Number(_) | PropertyValue::Boolean(_) => 0,
                };
                property.key.len() + value_bytes + property.units.as_ref().map_or(0, String::len)
            })
            .sum();
        self.global_id.len()
            + self.name.as_ref().map_or(0, String::len)
            + self.storey.as_ref().map_or(0, String::len)
            + property_bytes
    }
}

fn ecef_to_geodetic(ecef: [f64; 3]) -> (f64, f64, f64) {
    let [x, y, z] = ecef;
    let eccentricity_squared = 2.0 * WGS84_F - WGS84_F * WGS84_F;
    let semi_minor = WGS84_A * (1.0 - WGS84_F);
    let second_eccentricity_squared =
        (WGS84_A * WGS84_A - semi_minor * semi_minor) / (semi_minor * semi_minor);
    let distance_from_axis = x.hypot(y);
    let reduced_latitude = (z * WGS84_A).atan2(distance_from_axis * semi_minor);
    let latitude = (z + second_eccentricity_squared * semi_minor * reduced_latitude.sin().powi(3))
        .atan2(
            distance_from_axis - eccentricity_squared * WGS84_A * reduced_latitude.cos().powi(3),
        );
    let prime_vertical_radius =
        WGS84_A / (1.0 - eccentricity_squared * latitude.sin().powi(2)).sqrt();
    let height = distance_from_axis * latitude.cos() + z * latitude.sin()
        - WGS84_A * WGS84_A / prime_vertical_radius;
    (latitude, y.atan2(x), height)
}

fn geodetic_of_model_point(point: [f64; 3], origin: ModelOrigin) -> (f64, f64, f64) {
    let [model_x, model_y, up] = point;
    let (sine, cosine) = origin.x_axis_from_east_radians.sin_cos();
    let east = cosine * model_x - sine * model_y;
    let north = sine * model_x + cosine * model_y;
    let m = enu_to_ecef_matrix(
        origin.latitude.to_radians(),
        origin.longitude.to_radians(),
        origin.height,
    );
    let ecef = [
        m[0] * east + m[4] * north + m[8] * up + m[12],
        m[1] * east + m[5] * north + m[9] * up + m[13],
        m[2] * east + m[6] * north + m[10] * up + m[14],
    ];
    ecef_to_geodetic(ecef)
}

fn location_of_box(minimum: [f64; 3], maximum: [f64; 3], origin: ModelOrigin) -> ElementLocation {
    let center = std::array::from_fn(|axis| (minimum[axis] + maximum[axis]) / 2.0);
    let (latitude, longitude, height) = geodetic_of_model_point(center, origin);
    let radius = (maximum[0] - minimum[0])
        .hypot(maximum[1] - minimum[1])
        .hypot(maximum[2] - minimum[2])
        / 2.0;
    ElementLocation {
        longitude: longitude.to_degrees(),
        latitude: latitude.to_degrees(),
        height,
        radius,
    }
}

pub fn element_locations<'a>(
    meshes: impl Iterator<Item = (&'a str, &'a [[f64; 3]])>,
    origin: ModelOrigin,
) -> HashMap<String, ElementLocation> {
    let mut boxes: HashMap<&str, ([f64; 3], [f64; 3])> = HashMap::new();
    for (global_id, positions) in meshes {
        for position in positions {
            if !position.iter().all(|coordinate| coordinate.is_finite()) {
                continue;
            }
            let (minimum, maximum) = boxes.entry(global_id).or_insert((*position, *position));
            for axis in 0..3 {
                minimum[axis] = minimum[axis].min(position[axis]);
                maximum[axis] = maximum[axis].max(position[axis]);
            }
        }
    }
    boxes
        .into_iter()
        .map(|(global_id, (minimum, maximum))| {
            (
                global_id.to_string(),
                location_of_box(minimum, maximum, origin),
            )
        })
        .collect()
}

fn warn(warnings: &mut ExtractionWarnings, name: &'static str) {
    *warnings.entry(name).or_insert(0) += 1;
}

fn stored_text(text: &str, warnings: &mut ExtractionWarnings) -> String {
    if text.len() <= MAX_STORED_TEXT_BYTES {
        return text.to_string();
    }
    warn(warnings, WARNING_TEXT_TRUNCATED);
    let mut end = MAX_STORED_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn capped_references(mut references: Vec<u32>, warnings: &mut ExtractionWarnings) -> Vec<u32> {
    if references.len() > MAX_PROPERTIES_PER_ELEMENT {
        warn(warnings, WARNING_PROPERTIES_TRUNCATED);
        references.truncate(MAX_PROPERTIES_PER_ELEMENT);
    }
    references
}

pub(crate) struct ScannedSpan {
    pub express_id: u32,
    pub start: usize,
    pub end: usize,
}

pub(crate) struct ScannedRelation {
    pub express_id: u32,
    pub start: usize,
    pub end: usize,
    pub record: RelationRecord,
}

pub struct IfcScan {
    pub(crate) index: Arc<EntityIndex>,
    pub(crate) products: Vec<ScannedSpan>,
    pub(crate) relations: Vec<ScannedRelation>,
    pub(crate) georef_types: Vec<(u32, IfcType)>,
    pub(crate) true_north_context: Option<u32>,
}

#[derive(Default)]
struct ModelRelations {
    parent_of: HashMap<u32, u32>,
    type_of: HashMap<u32, u32>,
    material_of: HashMap<u32, u32>,
    definitions_of: HashMap<u32, Vec<u32>>,
    definition_links: u64,
    storey_names: HashMap<u32, String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelationRecord {
    Storey,
    Link,
}

impl ModelRelations {
    fn record_parent(&mut self, child: u32, parent: u32, warnings: &mut ExtractionWarnings) {
        if self.parent_of.contains_key(&child) {
            warn(warnings, WARNING_SECOND_SPATIAL_PARENT);
            return;
        }
        let mut ancestor = parent;
        for _ in 0..MAX_SPATIAL_DEPTH {
            if ancestor == child {
                warn(warnings, WARNING_SPATIAL_CYCLE);
                return;
            }
            let Some(next) = self.parent_of.get(&ancestor) else {
                break;
            };
            ancestor = *next;
        }
        self.parent_of.insert(child, parent);
    }

    fn record_children(
        &mut self,
        parent: Option<u32>,
        children: Option<Vec<u32>>,
        warnings: &mut ExtractionWarnings,
    ) {
        let (Some(parent), Some(children)) = (parent, children) else {
            return;
        };
        for child in children {
            self.record_parent(child, parent, warnings);
        }
    }

    fn record_definitions(
        &mut self,
        relation: &DecodedEntity,
        link_budget: u64,
    ) -> Result<(), IngestError> {
        // ifc4 allows an IfcPropertySetDefinitionSet here, ref_list_at reads either form
        let Some(definitions) =
            relation.ref_list_at(slot::IfcRelDefinesByProperties::RELATING_PROPERTY_DEFINITION)
        else {
            return Ok(());
        };
        let objects = relation
            .ref_list_at(slot::IfcRelDefinesByProperties::RELATED_OBJECTS)
            .unwrap_or_default();
        let links = (objects.len() as u64).saturating_mul(definitions.len() as u64);
        self.definition_links = self.definition_links.saturating_add(links);
        if self.definition_links > link_budget {
            return Err(IngestError::ElementRefused(format!(
                "the IFC's IfcRelDefinesByProperties relations name more than {link_budget} \
                 object and property set pairs, the most a file of its size may hold"
            )));
        }
        for object in objects {
            self.definitions_of
                .entry(object)
                .or_default()
                .extend_from_slice(&definitions);
        }
        Ok(())
    }

    fn record_relation(
        &mut self,
        relation: &DecodedEntity,
        link_budget: u64,
        warnings: &mut ExtractionWarnings,
    ) -> Result<(), IngestError> {
        match relation.ifc_type {
            IfcType::IfcRelContainedInSpatialStructure => self.record_children(
                relation.ref_at(slot::IfcRelContainedInSpatialStructure::RELATING_STRUCTURE),
                relation.ref_list_at(slot::IfcRelContainedInSpatialStructure::RELATED_ELEMENTS),
                warnings,
            ),
            IfcType::IfcRelAggregates => self.record_children(
                relation.ref_at(slot::IfcRelAggregates::RELATING_OBJECT),
                relation.ref_list_at(slot::IfcRelAggregates::RELATED_OBJECTS),
                warnings,
            ),
            IfcType::IfcRelNests => self.record_children(
                relation.ref_at(slot::IfcRelNests::RELATING_OBJECT),
                relation.ref_list_at(slot::IfcRelNests::RELATED_OBJECTS),
                warnings,
            ),
            IfcType::IfcRelVoidsElement => self.record_children(
                relation.ref_at(slot::IfcRelVoidsElement::RELATING_BUILDING_ELEMENT),
                relation
                    .ref_at(slot::IfcRelVoidsElement::RELATED_OPENING_ELEMENT)
                    .map(|opening| vec![opening]),
                warnings,
            ),
            IfcType::IfcRelDefinesByProperties => {
                return self.record_definitions(relation, link_budget);
            }
            IfcType::IfcRelDefinesByType => {
                let Some(type_object) = relation.ref_at(slot::IfcRelDefinesByType::RELATING_TYPE)
                else {
                    return Ok(());
                };
                for object in relation
                    .ref_list_at(slot::IfcRelDefinesByType::RELATED_OBJECTS)
                    .unwrap_or_default()
                {
                    self.type_of.entry(object).or_insert(type_object);
                }
            }
            IfcType::IfcRelAssociatesMaterial => {
                let Some(material) =
                    relation.ref_at(slot::IfcRelAssociatesMaterial::RELATING_MATERIAL)
                else {
                    return Ok(());
                };
                for object in relation
                    .ref_list_at(slot::IfcRelAssociatesMaterial::RELATED_OBJECTS)
                    .unwrap_or_default()
                {
                    self.material_of.entry(object).or_insert(material);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn from_spans(
        decoder: &EntityDecoder<'_>,
        spans: &[ScannedRelation],
        link_budget: u64,
        warnings: &mut ExtractionWarnings,
    ) -> Result<Self, IngestError> {
        let mut relations = Self::default();
        for span in spans {
            let Ok(entity) = decoder.decode_at_uncached(span.start, span.end) else {
                warn(warnings, WARNING_UNDECODABLE_RELATION);
                continue;
            };
            if span.record == RelationRecord::Link {
                relations.record_relation(&entity, link_budget, warnings)?;
                continue;
            }
            let label = text_at(&entity, slot::IfcRoot::NAME.index(), warnings)
                .or_else(|| text_at(&entity, slot::IfcRoot::GLOBAL_ID.index(), warnings));
            if let Some(label) = label {
                relations.storey_names.insert(span.express_id, label);
            }
        }
        Ok(relations)
    }

    fn storey_of(&self, express_id: u32, warnings: &mut ExtractionWarnings) -> Option<String> {
        let mut current = express_id;
        for _ in 0..MAX_SPATIAL_DEPTH {
            if let Some(name) = self.storey_names.get(&current) {
                return Some(name.clone());
            }
            current = *self.parent_of.get(&current)?;
        }
        warn(warnings, WARNING_SPATIAL_CHAIN_TOO_DEEP);
        None
    }
}

fn text_at(
    entity: &DecodedEntity,
    position: usize,
    warnings: &mut ExtractionWarnings,
) -> Option<String> {
    entity
        .get_string(position)
        .filter(|text| !text.is_empty())
        .map(|text| stored_text(text, warnings))
}

fn tree_kind(class: &IfcType) -> TreeKind {
    if class.is_subtype_of(IfcType::IfcBuildingStorey) {
        return TreeKind::Model;
    }
    if class.is_subtype_of(IfcType::IfcSpace) {
        return TreeKind::Element;
    }
    let is_spatial =
        *class == IfcType::IfcProject || class.is_subtype_of(IfcType::IfcSpatialStructureElement);
    if is_spatial {
        TreeKind::Subject
    } else {
        TreeKind::Element
    }
}

fn property_key(set_name: &str, entry_name: &str) -> String {
    if set_name.is_empty() {
        entry_name.to_string()
    } else {
        format!("{set_name}.{entry_name}")
    }
}

fn typed_value(property: &PropValue, warnings: &mut ExtractionWarnings) -> PropertyValue {
    match property.kind {
        ValueKind::Number => match property.value.parse::<f64>() {
            Ok(number) if number.is_finite() => PropertyValue::Number(number),
            _ => PropertyValue::Text(stored_text(&property.value, warnings)),
        },
        ValueKind::Bool => PropertyValue::Boolean(property.value == "true"),
        _ => PropertyValue::Text(stored_text(&property.value, warnings)),
    }
}

fn quantity_measure(kind: &str) -> Option<&'static str> {
    match kind {
        "Length" => Some("IFCLENGTHMEASURE"),
        "Area" => Some("IFCAREAMEASURE"),
        "Volume" => Some("IFCVOLUMEMEASURE"),
        "Weight" => Some("IFCMASSMEASURE"),
        "Time" => Some("IFCTIMEMEASURE"),
        _ => None,
    }
}

fn text_property(key: String, value: String) -> ElementProperty {
    ElementProperty {
        key,
        value: PropertyValue::Text(value),
        units: None,
    }
}

struct TypeSummary {
    properties: Vec<ElementProperty>,
    definitions: Vec<u32>,
}

#[derive(Default)]
struct MaterialMember {
    name: Option<String>,
    set: Option<String>,
    thickness: Option<f64>,
    fraction: Option<f64>,
}

fn material_name(
    decoder: &mut EntityDecoder<'_>,
    material_id: u32,
    warnings: &mut ExtractionWarnings,
) -> Option<String> {
    let Ok(material) = decoder.decode_by_id(material_id) else {
        warn(warnings, WARNING_UNDECODABLE_MATERIAL);
        return None;
    };
    if material.ifc_type != IfcType::IfcMaterial {
        return None;
    }
    material
        .string_at(slot::IfcMaterial::NAME)
        .map(|name| stored_text(name, warnings))
}

fn material_list_members(
    decoder: &mut EntityDecoder<'_>,
    list: &DecodedEntity,
    warnings: &mut ExtractionWarnings,
) -> Vec<MaterialMember> {
    let material_ids = capped_references(
        list.ref_list_at(slot::IfcMaterialList::MATERIALS)
            .unwrap_or_default(),
        warnings,
    );
    material_ids
        .into_iter()
        .map(|material_id| MaterialMember {
            name: material_name(decoder, material_id, warnings),
            ..MaterialMember::default()
        })
        .collect()
}

fn material_layer_members(
    decoder: &mut EntityDecoder<'_>,
    set: &DecodedEntity,
    warnings: &mut ExtractionWarnings,
) -> Vec<MaterialMember> {
    let set_name = set
        .string_at(slot::IfcMaterialLayerSet::LAYER_SET_NAME)
        .map(|name| stored_text(name, warnings));
    let mut members = Vec::new();
    let layer_ids = capped_references(
        set.ref_list_at(slot::IfcMaterialLayerSet::MATERIAL_LAYERS)
            .unwrap_or_default(),
        warnings,
    );
    for layer_id in layer_ids {
        let Ok(layer) = decoder.decode_by_id(layer_id) else {
            warn(warnings, WARNING_UNDECODABLE_MATERIAL);
            continue;
        };
        if !layer.ifc_type.is_subtype_of(IfcType::IfcMaterialLayer) {
            continue;
        }
        let name = layer
            .ref_at(slot::IfcMaterialLayer::MATERIAL)
            .and_then(|material_id| material_name(decoder, material_id, warnings));
        members.push(MaterialMember {
            name,
            set: set_name.clone(),
            thickness: layer.real_at(slot::IfcMaterialLayer::LAYER_THICKNESS),
            fraction: None,
        });
    }
    members
}

fn material_profile_members(
    decoder: &mut EntityDecoder<'_>,
    set: &DecodedEntity,
    warnings: &mut ExtractionWarnings,
) -> Vec<MaterialMember> {
    let set_name = set
        .string_at(slot::IfcMaterialProfileSet::NAME)
        .map(|name| stored_text(name, warnings));
    let mut members = Vec::new();
    let profile_ids = capped_references(
        set.ref_list_at(slot::IfcMaterialProfileSet::MATERIAL_PROFILES)
            .unwrap_or_default(),
        warnings,
    );
    for profile_id in profile_ids {
        let Ok(profile) = decoder.decode_by_id(profile_id) else {
            warn(warnings, WARNING_UNDECODABLE_MATERIAL);
            continue;
        };
        if !profile.ifc_type.is_subtype_of(IfcType::IfcMaterialProfile) {
            continue;
        }
        let name = profile
            .ref_at(slot::IfcMaterialProfile::MATERIAL)
            .and_then(|material_id| material_name(decoder, material_id, warnings));
        members.push(MaterialMember {
            name,
            set: set_name.clone(),
            ..MaterialMember::default()
        });
    }
    members
}

fn material_constituent_members(
    decoder: &mut EntityDecoder<'_>,
    set: &DecodedEntity,
    warnings: &mut ExtractionWarnings,
) -> Vec<MaterialMember> {
    let set_name = set
        .string_at(slot::IfcMaterialConstituentSet::NAME)
        .map(|name| stored_text(name, warnings));
    let mut members = Vec::new();
    let constituent_ids = capped_references(
        set.ref_list_at(slot::IfcMaterialConstituentSet::MATERIAL_CONSTITUENTS)
            .unwrap_or_default(),
        warnings,
    );
    for constituent_id in constituent_ids {
        let Ok(constituent) = decoder.decode_by_id(constituent_id) else {
            warn(warnings, WARNING_UNDECODABLE_MATERIAL);
            continue;
        };
        if constituent.ifc_type != IfcType::IfcMaterialConstituent {
            continue;
        }
        let name = constituent
            .ref_at(slot::IfcMaterialConstituent::MATERIAL)
            .and_then(|material_id| material_name(decoder, material_id, warnings));
        members.push(MaterialMember {
            name,
            set: set_name.clone(),
            thickness: None,
            fraction: constituent.real_at(slot::IfcMaterialConstituent::FRACTION),
        });
    }
    members
}

fn material_members(
    decoder: &mut EntityDecoder<'_>,
    material_id: u32,
    warnings: &mut ExtractionWarnings,
) -> Vec<MaterialMember> {
    let Ok(material) = decoder.decode_by_id(material_id) else {
        warn(warnings, WARNING_UNDECODABLE_MATERIAL);
        return Vec::new();
    };
    // usages name one set and never another usage, so a usage pointing at itself resolves to nothing
    let usage_target = match material.ifc_type {
        IfcType::IfcMaterialLayerSetUsage => {
            material.ref_at(slot::IfcMaterialLayerSetUsage::FOR_LAYER_SET)
        }
        IfcType::IfcMaterialProfileSetUsage => {
            material.ref_at(slot::IfcMaterialProfileSetUsage::FOR_PROFILE_SET)
        }
        _ => Some(material_id),
    };
    let Some(definition_id) = usage_target else {
        return Vec::new();
    };
    let Ok(definition) = decoder.decode_by_id(definition_id) else {
        warn(warnings, WARNING_UNDECODABLE_MATERIAL);
        return Vec::new();
    };
    match definition.ifc_type {
        IfcType::IfcMaterial => vec![MaterialMember {
            name: definition
                .string_at(slot::IfcMaterial::NAME)
                .map(|name| stored_text(name, warnings)),
            ..MaterialMember::default()
        }],
        IfcType::IfcMaterialList => material_list_members(decoder, &definition, warnings),
        IfcType::IfcMaterialLayerSet => material_layer_members(decoder, &definition, warnings),
        IfcType::IfcMaterialProfileSet => material_profile_members(decoder, &definition, warnings),
        IfcType::IfcMaterialConstituentSet => {
            material_constituent_members(decoder, &definition, warnings)
        }
        _ => Vec::new(),
    }
}

#[derive(Default)]
struct CollectedProperties {
    counts: HashMap<String, u32>,
    rows: Vec<ElementProperty>,
    truncated: bool,
}

impl CollectedProperties {
    fn add(&mut self, property: &ElementProperty) {
        if self.rows.len() >= MAX_PROPERTIES_PER_ELEMENT {
            self.truncated = true;
            return;
        }
        let occurrence = self.counts.entry(property.key.clone()).or_insert(0);
        let key = match *occurrence {
            0 => property.key.clone(),
            later => format!("{}[{later}]", property.key),
        };
        *occurrence += 1;
        self.rows.push(ElementProperty {
            key,
            value: property.value.clone(),
            units: property.units.clone(),
        });
    }

    fn add_inherited(&mut self, property: &ElementProperty) {
        if !self.counts.contains_key(&property.key) {
            self.add(property);
        }
    }
}

struct ElementReader<'a> {
    decoder: EntityDecoder<'a>,
    property_decoder: PropertyDecoder,
    units: ProjectUnits,
    relations: ModelRelations,
    definition_rows: HashMap<u32, Arc<Vec<ElementProperty>>>,
    type_summaries: HashMap<u32, Option<Arc<TypeSummary>>>,
    material_rows: HashMap<u32, Arc<Vec<ElementProperty>>>,
    warnings: ExtractionWarnings,
    budgets: ExtractionBudgets,
    text_bytes_spent: u64,
    visits_spent: u64,
}

impl ElementReader<'_> {
    fn charge_text(&mut self, bytes: usize) -> Result<(), IngestError> {
        self.text_bytes_spent = self.text_bytes_spent.saturating_add(bytes as u64);
        if self.text_bytes_spent > self.budgets.text_bytes {
            return Err(IngestError::ElementRefused(format!(
                "the IFC's elements hold more than {} bytes of property text, \
                 the most a file of its size may hold",
                self.budgets.text_bytes
            )));
        }
        Ok(())
    }

    fn charge_visits(&mut self, visits: usize) -> Result<(), IngestError> {
        self.visits_spent = self.visits_spent.saturating_add(visits as u64);
        if self.visits_spent > self.budgets.visits {
            return Err(IngestError::ElementRefused(format!(
                "reading the IFC's element properties took more than {} property visits, \
                 the most a file of its size may take",
                self.budgets.visits
            )));
        }
        if Instant::now() >= self.budgets.deadline {
            return Err(IngestError::ElementRefused(format!(
                "reading the IFC's element properties took more than {} seconds",
                EXTRACTION_TIME_LIMIT.as_secs()
            )));
        }
        Ok(())
    }

    fn admit_set_members(&mut self, references: Vec<u32>) -> Result<bool, IngestError> {
        if references.len() > MAX_PROPERTIES_PER_ELEMENT {
            warn(&mut self.warnings, WARNING_PROPERTIES_TRUNCATED);
            return Ok(false);
        }
        // ifc-lite clones a member's text once per reference, so the charge counts references
        let member_bytes: usize = references
            .iter()
            .filter_map(|id| self.decoder.get_raw_bytes(*id))
            .map(<[u8]>::len)
            .sum();
        self.charge_text(member_bytes)?;
        Ok(true)
    }

    fn unit_symbol_of(&mut self, unit_id: u32) -> Option<String> {
        resolve_unit_by_ref(&mut self.decoder, unit_id)
            .map(|(_, unit, _)| stored_text(&unit.symbol, &mut self.warnings))
    }

    fn decode_property_set(
        &mut self,
        definition: &DecodedEntity,
    ) -> Result<Vec<ElementProperty>, IngestError> {
        let references = definition
            .ref_list_at(slot::IfcPropertySet::HAS_PROPERTIES)
            .unwrap_or_default();
        if !self.admit_set_members(references)? {
            return Ok(Vec::new());
        }
        let Some(set) = self
            .property_decoder
            .decode_set(&mut self.decoder, definition)
        else {
            warn(&mut self.warnings, WARNING_UNDECODABLE_PROPERTY_DEFINITION);
            return Ok(Vec::new());
        };
        let members = definition
            .attr(slot::IfcPropertySet::HAS_PROPERTIES)
            .and_then(|attribute| self.decoder.resolve_ref_list(attribute).ok())
            .unwrap_or_default();
        let mut unit_overrides = HashMap::new();
        for member in &members {
            if member.ifc_type != IfcType::IfcPropertySingleValue {
                continue;
            }
            let name = member.string_at(slot::IfcPropertySingleValue::NAME);
            let unit = member.ref_at(slot::IfcPropertySingleValue::UNIT);
            let (Some(name), Some(unit)) = (name, unit) else {
                continue;
            };
            if let Some(symbol) = self.unit_symbol_of(unit) {
                unit_overrides.insert(name.to_string(), symbol);
            }
        }
        let set_name = stored_text(&set.name.unwrap_or_default(), &mut self.warnings);
        Ok(set
            .properties
            .iter()
            .filter_map(flat_property)
            .map(|property| {
                let value = typed_value(&property, &mut self.warnings);
                let units = match value {
                    PropertyValue::Number(_) => {
                        unit_overrides.get(&property.name).cloned().or_else(|| {
                            self.units
                                .unit_for_measure(&property.value_type)
                                .map(|unit| stored_text(&unit.symbol, &mut self.warnings))
                        })
                    }
                    _ => None,
                };
                ElementProperty {
                    key: stored_text(&property_key(&set_name, &property.name), &mut self.warnings),
                    value,
                    units,
                }
            })
            .collect())
    }

    fn decode_quantity_set(
        &mut self,
        definition: &DecodedEntity,
    ) -> Result<Vec<ElementProperty>, IngestError> {
        let references = definition
            .ref_list_at(slot::IfcElementQuantity::QUANTITIES)
            .unwrap_or_default();
        if !self.admit_set_members(references)? {
            return Ok(Vec::new());
        }
        let Some(set) = decode_quantity_records(
            &mut self.decoder,
            definition,
            None,
            QuantityDecodeMode::AuthoredAnalysis,
        ) else {
            warn(&mut self.warnings, WARNING_UNDECODABLE_PROPERTY_DEFINITION);
            return Ok(Vec::new());
        };
        if set.rejected_members > 0 {
            *self.warnings.entry(WARNING_REJECTED_QUANTITY).or_insert(0) +=
                set.rejected_members as u64;
        }
        let set_name = stored_text(&set.name, &mut self.warnings);
        let mut rows = Vec::with_capacity(set.records.len());
        for record in set.records {
            if !record.value.value.is_finite() {
                warn(&mut self.warnings, WARNING_NON_FINITE_QUANTITY);
                continue;
            }
            let units = match record.unit_id {
                Some(unit_id) => self.unit_symbol_of(unit_id),
                None => quantity_measure(record.value.kind)
                    .and_then(|measure| self.units.unit_for_measure(measure))
                    .map(|unit| stored_text(&unit.symbol, &mut self.warnings)),
            };
            rows.push(ElementProperty {
                key: stored_text(
                    &property_key(&set_name, &record.value.name),
                    &mut self.warnings,
                ),
                value: PropertyValue::Number(record.value.value),
                units,
            });
        }
        Ok(rows)
    }

    fn definition_properties(
        &mut self,
        definition_id: u32,
    ) -> Result<Arc<Vec<ElementProperty>>, IngestError> {
        if let Some(rows) = self.definition_rows.get(&definition_id) {
            return Ok(Arc::clone(rows));
        }
        let rows = match self.decoder.decode_by_id(definition_id) {
            Ok(definition) if definition.ifc_type == IfcType::IfcPropertySet => {
                self.decode_property_set(&definition)?
            }
            Ok(definition) if definition.ifc_type == IfcType::IfcElementQuantity => {
                self.decode_quantity_set(&definition)?
            }
            Ok(_) => Vec::new(),
            Err(_) => {
                warn(&mut self.warnings, WARNING_UNDECODABLE_PROPERTY_DEFINITION);
                Vec::new()
            }
        };
        let rows = Arc::new(rows);
        self.definition_rows
            .insert(definition_id, Arc::clone(&rows));
        Ok(rows)
    }

    fn type_summary(&mut self, type_id: u32) -> Option<Arc<TypeSummary>> {
        if let Some(summary) = self.type_summaries.get(&type_id) {
            return summary.clone();
        }
        let summary = self.read_type_summary(type_id).map(Arc::new);
        self.type_summaries.insert(type_id, summary.clone());
        summary
    }

    fn read_type_summary(&mut self, type_id: u32) -> Option<TypeSummary> {
        let Ok(type_object) = self.decoder.decode_by_id(type_id) else {
            warn(&mut self.warnings, WARNING_UNDECODABLE_RELATION);
            return None;
        };
        // a relation may point anywhere in a malformed file, and the slot readers assert their owner
        if !type_object.ifc_type.is_subtype_of(IfcType::IfcTypeObject) {
            warn(&mut self.warnings, WARNING_UNDECODABLE_RELATION);
            return None;
        }
        let mut properties = vec![text_property(
            "Type.Class".to_string(),
            type_object.ifc_type.name().to_string(),
        )];
        if let Some(name) = text_at(
            &type_object,
            slot::IfcTypeObject::NAME.index(),
            &mut self.warnings,
        ) {
            properties.push(text_property("Type.Name".to_string(), name));
        }
        if let Some(global_id) = text_at(
            &type_object,
            slot::IfcTypeObject::GLOBAL_ID.index(),
            &mut self.warnings,
        ) {
            properties.push(text_property("Type.GlobalId".to_string(), global_id));
        }
        let mut definitions = type_object
            .ref_list_at(slot::IfcTypeObject::HAS_PROPERTY_SETS)
            .unwrap_or_default();
        if let Some(related) = self.relations.definitions_of.get(&type_id) {
            definitions.extend_from_slice(related);
        }
        Some(TypeSummary {
            properties,
            definitions: capped_references(definitions, &mut self.warnings),
        })
    }

    fn read_material_properties(&mut self, material_id: u32) -> Vec<ElementProperty> {
        let length_unit = self
            .units
            .unit_for_measure("IFCLENGTHMEASURE")
            .map(|unit| stored_text(&unit.symbol, &mut self.warnings));
        let mut rows = Vec::new();
        for (index, member) in material_members(&mut self.decoder, material_id, &mut self.warnings)
            .into_iter()
            .enumerate()
        {
            if let Some(name) = member.name {
                rows.push(text_property(format!("Material[{index}].Name"), name));
            }
            if let Some(set) = member.set {
                rows.push(text_property(format!("Material[{index}].Set"), set));
            }
            if let Some(thickness) = member.thickness.filter(|value| value.is_finite()) {
                rows.push(ElementProperty {
                    key: format!("Material[{index}].Thickness"),
                    value: PropertyValue::Number(thickness),
                    units: length_unit.clone(),
                });
            }
            if let Some(fraction) = member.fraction.filter(|value| value.is_finite()) {
                rows.push(ElementProperty {
                    key: format!("Material[{index}].Fraction"),
                    value: PropertyValue::Number(fraction),
                    units: None,
                });
            }
        }
        rows
    }

    fn material_properties(
        &mut self,
        material_id: u32,
    ) -> Result<Arc<Vec<ElementProperty>>, IngestError> {
        if let Some(rows) = self.material_rows.get(&material_id) {
            return Ok(Arc::clone(rows));
        }
        let rows = self.read_material_properties(material_id);
        let row_text_bytes: usize = rows
            .iter()
            .map(|row| match &row.value {
                PropertyValue::Text(text) => row.key.len() + text.len(),
                PropertyValue::Number(_) | PropertyValue::Boolean(_) => row.key.len(),
            })
            .sum();
        self.charge_text(row_text_bytes)?;
        let rows = Arc::new(rows);
        self.material_rows.insert(material_id, Arc::clone(&rows));
        Ok(rows)
    }

    fn element_properties(
        &mut self,
        express_id: u32,
        storey: Option<&str>,
        tag: Option<String>,
    ) -> Result<Vec<ElementProperty>, IngestError> {
        let mut collected = CollectedProperties::default();
        if let Some(tag) = tag {
            collected.add(&text_property("Element.Tag".to_string(), tag));
        }
        let mut seen_definitions = HashSet::new();
        let own_definition_count = self
            .relations
            .definitions_of
            .get(&express_id)
            .map_or(0, Vec::len);
        self.charge_visits(own_definition_count + 1)?;
        let own_definitions = self
            .relations
            .definitions_of
            .get(&express_id)
            .cloned()
            .unwrap_or_default();
        for definition_id in own_definitions {
            if !seen_definitions.insert(definition_id) {
                continue;
            }
            let rows = self.definition_properties(definition_id)?;
            self.charge_visits(rows.len())?;
            for property in rows.iter() {
                collected.add(property);
            }
        }

        let type_id = self.relations.type_of.get(&express_id).copied();
        if let Some(summary) = type_id.and_then(|id| self.type_summary(id)) {
            self.charge_visits(summary.definitions.len())?;
            for definition_id in &summary.definitions {
                if !seen_definitions.insert(*definition_id) {
                    continue;
                }
                let rows = self.definition_properties(*definition_id)?;
                self.charge_visits(rows.len())?;
                for property in rows.iter() {
                    collected.add_inherited(property);
                }
            }
            for property in &summary.properties {
                collected.add(property);
            }
        }

        let material_id = self
            .relations
            .material_of
            .get(&express_id)
            .or_else(|| type_id.and_then(|id| self.relations.material_of.get(&id)))
            .copied();
        if let Some(material_id) = material_id {
            let rows = self.material_properties(material_id)?;
            self.charge_visits(rows.len())?;
            for property in rows.iter() {
                collected.add(property);
            }
        }

        if let Some(storey) = storey {
            collected.add(&text_property(
                "Spatial.Storey".to_string(),
                storey.to_string(),
            ));
        }

        if collected.truncated {
            warn(&mut self.warnings, WARNING_PROPERTIES_TRUNCATED);
        }
        Ok(collected.rows)
    }

    fn placement_origin(
        &mut self,
        router: &GeometryRouter,
        product: &DecodedEntity,
    ) -> Option<[f64; 3]> {
        router
            .resolve_scaled_placement_strict(product, &mut self.decoder)
            .ok()
            .map(|transform| [transform[12], transform[13], transform[14]])
            .filter(|translation| translation.iter().all(|value| value.is_finite()))
    }

    fn building_of(&mut self, express_id: u32) -> Option<DecodedEntity> {
        let mut current = express_id;
        for _ in 0..MAX_SPATIAL_DEPTH {
            current = *self.relations.parent_of.get(&current)?;
            let ancestor = self.decoder.decode_by_id(current).ok()?;
            if ancestor.ifc_type.is_subtype_of(IfcType::IfcBuilding) {
                return Some(ancestor);
            }
        }
        None
    }

    fn storey_base_point(
        &mut self,
        router: &GeometryRouter,
        storey: &DecodedEntity,
    ) -> Option<[f64; 3]> {
        if storey
            .ref_at(slot::IfcBuildingStorey::OBJECT_PLACEMENT)
            .is_some()
        {
            let origin = self.placement_origin(router, storey);
            if origin.is_none() {
                warn(&mut self.warnings, WARNING_UNRESOLVED_STOREY_PLACEMENT);
            }
            return origin;
        }
        let elevation = storey
            .real_at(slot::IfcBuildingStorey::ELEVATION)
            .filter(|elevation| elevation.is_finite());
        let building = self.building_of(storey.id);
        let (Some(elevation), Some(building)) = (elevation, building) else {
            warn(&mut self.warnings, WARNING_STOREY_WITHOUT_HEIGHT);
            return None;
        };
        // ifc Elevation is measured from the building's 0.00, the origin of its placement
        let Some([x, y, z]) = self.placement_origin(router, &building) else {
            warn(&mut self.warnings, WARNING_UNRESOLVED_BUILDING_PLACEMENT);
            return None;
        };
        warn(&mut self.warnings, WARNING_STOREY_HEIGHT_FROM_ELEVATION);
        Some([x, y, z + elevation * router.unit_scale()])
    }
}

pub fn extract_elements(
    source: &[u8],
    locations: &HashMap<String, ElementLocation>,
    origin: ModelOrigin,
    east_north_up_placement: Option<&MapConversionPlacement>,
    on_element: impl FnMut(ExtractedElement) -> Result<(), IngestError>,
) -> Result<ExtractionWarnings, IngestError> {
    let scanned = crate::ifc_reader::scan_ifc(source);
    extract_elements_from_scan(
        source,
        &scanned.scan,
        locations,
        origin,
        east_north_up_placement,
        on_element,
    )
}

pub fn extract_elements_from_scan(
    source: &[u8],
    scan: &IfcScan,
    locations: &HashMap<String, ElementLocation>,
    origin: ModelOrigin,
    east_north_up_placement: Option<&MapConversionPlacement>,
    on_element: impl FnMut(ExtractedElement) -> Result<(), IngestError>,
) -> Result<ExtractionWarnings, IngestError> {
    extract_elements_within(
        source,
        scan,
        locations,
        origin,
        east_north_up_placement,
        ExtractionBudgets::for_input(source.len()),
        on_element,
    )
}

fn extract_elements_within(
    source: &[u8],
    scan: &IfcScan,
    locations: &HashMap<String, ElementLocation>,
    origin: ModelOrigin,
    east_north_up_placement: Option<&MapConversionPlacement>,
    budgets: ExtractionBudgets,
    mut on_element: impl FnMut(ExtractedElement) -> Result<(), IngestError>,
) -> Result<ExtractionWarnings, IngestError> {
    // ifc_reader resizes the meshes the locations come from by these factors
    let axis_factors = crate::ifc_reader::axis_factors_of_scan(source, scan)?.unwrap_or([1.0; 3]);
    let mut decoder = EntityDecoder::with_arc_index(source, Arc::clone(&scan.index));
    // ifc_reader places the meshes through a router built the same way, with no RTC shift
    let router = GeometryRouter::with_units(source, &mut decoder);
    let mut warnings = ExtractionWarnings::new();
    let relations = ModelRelations::from_spans(
        &decoder,
        &scan.relations,
        budgets.property_rows,
        &mut warnings,
    )?;

    let project_id = find_project_id(source);
    let units = match project_id {
        Some(project_id) => ProjectUnits::resolve(&mut decoder, project_id),
        None => {
            warn(&mut warnings, WARNING_NO_PROJECT);
            ProjectUnits::default()
        }
    };
    let mut reader = ElementReader {
        decoder,
        property_decoder: new_property_decoder(source.len()),
        units,
        relations,
        definition_rows: HashMap::new(),
        type_summaries: HashMap::new(),
        material_rows: HashMap::new(),
        warnings,
        budgets,
        text_bytes_spent: 0,
        visits_spent: 0,
    };

    let mut property_rows = 0u64;
    let mut seen_global_ids = HashSet::new();
    for product in &scan.products {
        let express_id = product.express_id;
        let Ok(entity) = reader
            .decoder
            .decode_at_uncached(product.start, product.end)
        else {
            warn(&mut reader.warnings, WARNING_UNDECODABLE_ELEMENT);
            continue;
        };
        let Some(global_id) = text_at(
            &entity,
            slot::IfcRoot::GLOBAL_ID.index(),
            &mut reader.warnings,
        ) else {
            warn(&mut reader.warnings, WARNING_MISSING_GLOBAL_ID);
            continue;
        };
        if !seen_global_ids.insert(global_id.clone()) {
            warn(&mut reader.warnings, WARNING_DUPLICATE_GLOBAL_ID);
            continue;
        }

        let declared_parent = reader.relations.parent_of.get(&express_id).copied();
        let parent_express_id = match (declared_parent, project_id) {
            (Some(parent), _) => Some(parent),
            (None, Some(project_id)) if project_id != express_id => {
                warn(&mut reader.warnings, WARNING_UNCONTAINED_ELEMENT);
                Some(project_id)
            }
            (None, _) => None,
        };
        let storey = reader.relations.storey_of(express_id, &mut reader.warnings);
        // spatial elements keep LongName in the position an IfcElement keeps Tag
        let tag = entity
            .ifc_type
            .is_subtype_of(IfcType::IfcElement)
            .then(|| text_at(&entity, slot::IfcElement::TAG.index(), &mut reader.warnings))
            .flatten();
        let properties = reader.element_properties(express_id, storey.as_deref(), tag)?;
        let base_height = entity
            .ifc_type
            .is_subtype_of(IfcType::IfcBuildingStorey)
            .then(|| reader.storey_base_point(&router, &entity))
            .flatten()
            .map(|point| {
                let resized: [f64; 3] =
                    std::array::from_fn(|axis| point[axis] * axis_factors[axis]);
                let in_mesh_frame = match east_north_up_placement {
                    Some(placement) => placement.east_north_up_of(resized)?,
                    None => resized,
                };
                let (_, _, height) = geodetic_of_model_point(in_mesh_frame, origin);
                Ok::<f64, IngestError>(height)
            })
            .transpose()?;

        property_rows += properties.len() as u64;
        if property_rows > reader.budgets.property_rows {
            return Err(IngestError::ElementRefused(format!(
                "the IFC has more than {} element property rows, the most a file of {} bytes may hold",
                reader.budgets.property_rows,
                source.len()
            )));
        }

        let element = ExtractedElement {
            express_id,
            location: locations.get(&global_id).copied(),
            base_height,
            global_id,
            class: entity.ifc_type.name().to_string(),
            name: text_at(&entity, slot::IfcRoot::NAME.index(), &mut reader.warnings),
            tree_kind: tree_kind(&entity.ifc_type),
            parent_express_id,
            storey,
            properties,
        };
        reader.charge_text(element.text_bytes())?;
        on_element(element)?;

        if reader.decoder.cache_size() > MAX_CACHED_ENTITIES {
            reader.decoder.clear_entity_cache();
        }
    }
    Ok(reader.warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_DOOR_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('legacy.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#2=IFCUNITASSIGNMENT((#1));
#3=IFCPROJECT('legacyproject',$,'Legacy',$,$,$,$,$,#2);
#4=IFCBUILDINGSTOREY('legacystorey',$,'Ground',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCDOOR('legacydoor',$,'Door',$,$,$,$,$,2100.,900.);
#6=IFCDOORSTYLE('legacystyle',$,'Steel Style',$,$,$,$,$,.SINGLE_SWING_LEFT.,.STEEL.,.F.,.F.);
#7=IFCWALLSTANDARDCASE('legacywall',$,'Wall',$,$,$,$,$);
#8=IFCRELAGGREGATES('rel1',$,$,$,#3,(#4));
#9=IFCRELCONTAINEDINSPATIALSTRUCTURE('rel2',$,$,$,(#5,#7),#4);
#10=IFCRELDEFINESBYTYPE('rel3',$,$,$,(#5),#6);
#11=IFCMATERIAL('Concrete');
#12=IFCMATERIALLAYER(#11,200.,$);
#13=IFCMATERIALLAYERSET((#12),'Wall 200');
#14=IFCMATERIALLAYERSETUSAGE(#13,.AXIS2.,.POSITIVE.,0.);
#15=IFCRELASSOCIATESMATERIAL('rel4',$,$,$,(#7),#14);
ENDSEC;
END-ISO-10303-21;
";

    const CYCLIC_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('cycle.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCPROJECT('cycleproject',$,'Cycle',$,$,$,$,$,$);
#2=IFCSITE('cyclesite',$,'A',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('cyclebuilding',$,'B',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCRELAGGREGATES('rel1',$,$,$,#2,(#3));
#5=IFCRELAGGREGATES('rel2',$,$,$,#3,(#2));
#6=IFCRELAGGREGATES('rel3',$,$,$,#2,(#2));
#7=IFCWALL('duplicate',$,'First',$,$,$,$,$,$);
#8=IFCWALL('duplicate',$,'Second',$,$,$,$,$,$);
#9=IFCWALL($,$,'Nameless',$,$,$,$,$,$);
#10=IFCMATERIALLAYERSETUSAGE(#10,.AXIS2.,.POSITIVE.,0.,$);
#11=IFCRELASSOCIATESMATERIAL('rel4',$,$,$,(#7),#10);
ENDSEC;
END-ISO-10303-21;
";

    const MODEL_ORIGIN: ModelOrigin = ModelOrigin {
        longitude: 10.0,
        latitude: 20.0,
        height: 100.0,
        x_axis_from_east_radians: 0.5,
    };

    fn extracted(ifc: &str) -> (Vec<ExtractedElement>, ExtractionWarnings) {
        let mut elements = Vec::new();
        let warnings = extract_elements(
            ifc.as_bytes(),
            &HashMap::new(),
            MODEL_ORIGIN,
            None,
            |element| {
                elements.push(element);
                Ok(())
            },
        )
        .unwrap();
        (elements, warnings)
    }

    fn property<'a>(element: &'a ExtractedElement, key: &str) -> &'a ElementProperty {
        element
            .properties
            .iter()
            .find(|property| property.key == key)
            .unwrap_or_else(|| panic!("no {key} in {:?}", element.properties))
    }

    #[test]
    fn the_reader_scan_extracts_the_same_elements() {
        let (direct, direct_warnings) = extracted(LEGACY_DOOR_IFC);
        let scanned =
            crate::ifc_reader::read_elements(LEGACY_DOOR_IFC.as_bytes(), u64::MAX).unwrap();
        let mut from_scan = Vec::new();
        let warnings = extract_elements_from_scan(
            LEGACY_DOOR_IFC.as_bytes(),
            &scanned.scan,
            &HashMap::new(),
            MODEL_ORIGIN,
            None,
            |element| {
                from_scan.push(element);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(from_scan, direct);
        assert_eq!(warnings, direct_warnings);
    }

    #[test]
    fn ifc2x3_names_read_through_the_same_walk() {
        let (elements, warnings) = extracted(LEGACY_DOOR_IFC);
        assert_eq!(
            warnings,
            ExtractionWarnings::from([(WARNING_STOREY_WITHOUT_HEIGHT, 1)]),
            "the storey has no placement and no building"
        );

        let door = elements
            .iter()
            .find(|element| element.global_id == "legacydoor")
            .unwrap();
        assert_eq!(door.storey.as_deref(), Some("Ground"));
        assert_eq!(
            property(door, "Type.Name").value,
            PropertyValue::Text("Steel Style".to_string())
        );
        assert_eq!(
            property(door, "Type.Class").value,
            PropertyValue::Text("IfcDoorStyle".to_string())
        );

        let wall = elements
            .iter()
            .find(|element| element.global_id == "legacywall")
            .unwrap();
        assert_eq!(wall.class, "IfcWallStandardCase");
        assert_eq!(
            property(wall, "Material[0].Name").value,
            PropertyValue::Text("Concrete".to_string())
        );
        let thickness = property(wall, "Material[0].Thickness");
        assert_eq!(thickness.value, PropertyValue::Number(200.0));
        assert_eq!(thickness.units.as_deref(), Some("mm"));
    }

    #[test]
    fn storeys_with_no_geometry_are_elements_of_kind_model() {
        let (elements, _) = extracted(LEGACY_DOOR_IFC);
        let storey = elements
            .iter()
            .find(|element| element.global_id == "legacystorey")
            .unwrap();
        assert_eq!(storey.tree_kind, TreeKind::Model);
        assert_eq!(storey.location, None);
        assert_eq!(storey.storey.as_deref(), Some("Ground"));
    }

    #[test]
    fn spatial_zones_are_elements_and_spatial_structure_is_subject() {
        assert_eq!(tree_kind(&IfcType::IfcSpatialZone), TreeKind::Element);
        assert_eq!(tree_kind(&IfcType::IfcBuilding), TreeKind::Subject);
    }

    fn storeys_in_millimetres(schema: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\n\
             FILE_NAME('storeys.ifc','2026-11-05',(''),(''),'','','');\n\
             FILE_SCHEMA(('{schema}'));\nENDSEC;\nDATA;\n\
             #1=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);\n\
             #2=IFCUNITASSIGNMENT((#1));\n\
             #3=IFCPROJECT('project',$,'Project',$,$,$,$,$,#2);\n\
             #4=IFCBUILDING('building',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);\n\
             #5=IFCBUILDINGSTOREY('upper',$,'Upper',$,$,$,$,$,.ELEMENT.,3000.);\n\
             #6=IFCBUILDINGSTOREY('ground',$,'Ground',$,$,$,$,$,.ELEMENT.,0.);\n\
             #7=IFCBUILDINGSTOREY('unset',$,'Unset',$,$,$,$,$,.ELEMENT.,$);\n\
             #8=IFCBUILDINGELEMENTPROXY('proxy',$,'Proxy',$,$,$,$,$,$);\n\
             #9=IFCRELAGGREGATES('rel1',$,$,$,#3,(#4));\n\
             #10=IFCRELAGGREGATES('rel2',$,$,$,#4,(#5,#6,#7));\n\
             #11=IFCRELCONTAINEDINSPATIALSTRUCTURE('rel3',$,$,$,(#8),#5);\n\
             ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    #[test]
    fn storey_elevations_with_no_placement_become_ellipsoid_heights_in_both_schemas() {
        for schema in ["IFC2X3", "IFC4"] {
            let (elements, warnings) = extracted(&storeys_in_millimetres(schema));
            assert_eq!(
                warnings,
                ExtractionWarnings::from([
                    (WARNING_STOREY_HEIGHT_FROM_ELEVATION, 2),
                    (WARNING_STOREY_WITHOUT_HEIGHT, 1),
                ]),
                "{schema}"
            );
            let base_height = |global_id: &str| {
                elements
                    .iter()
                    .find(|element| element.global_id == global_id)
                    .unwrap_or_else(|| panic!("{schema}: no {global_id}"))
                    .base_height
            };

            let upper = base_height("upper").expect(schema);
            assert!(
                (upper - (MODEL_ORIGIN.height + 3.0)).abs() < 1e-3,
                "{schema}: {upper}"
            );
            let ground = base_height("ground").expect(schema);
            assert!(
                (ground - MODEL_ORIGIN.height).abs() < 1e-3,
                "{schema}: {ground}"
            );
            assert_eq!(base_height("unset"), None, "{schema}");
            assert_eq!(base_height("building"), None, "{schema}");
            assert_eq!(base_height("proxy"), None, "{schema}");
        }
    }

    struct PlacedStorey {
        length_unit_prefix: &'static str,
        building_z: f64,
        elevation_of_ref_height: &'static str,
        storey_z: Option<f64>,
        storey_elevation: &'static str,
    }

    impl PlacedStorey {
        fn ifc(&self) -> String {
            let storey_placement = if self.storey_z.is_some() { "#21" } else { "$" };
            format!(
                "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\n\
                 FILE_NAME('placed.ifc','2026-11-05',(''),(''),'','','');\n\
                 FILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
                 #1=IFCSIUNIT(*,.LENGTHUNIT.,{prefix},.METRE.);\n\
                 #2=IFCUNITASSIGNMENT((#1));\n\
                 #3=IFCPROJECT('project',$,'Project',$,$,$,$,$,#2);\n\
                 #4=IFCRELAGGREGATES('rel1',$,$,$,#3,(#10));\n\
                 #5=IFCRELAGGREGATES('rel2',$,$,$,#10,(#20));\n\
                 #10=IFCBUILDING('building',$,'Main',$,$,#11,$,$,.ELEMENT.,{ref_height},$,$);\n\
                 #11=IFCLOCALPLACEMENT($,#12);\n\
                 #12=IFCAXIS2PLACEMENT3D(#13,$,$);\n\
                 #13=IFCCARTESIANPOINT((0.,0.,{building_z:?}));\n\
                 #20=IFCBUILDINGSTOREY('storey',$,'Level',$,$,{storey_placement},$,$,.ELEMENT.,{elevation});\n\
                 #21=IFCLOCALPLACEMENT(#11,#22);\n\
                 #22=IFCAXIS2PLACEMENT3D(#23,$,$);\n\
                 #23=IFCCARTESIANPOINT((0.,0.,{storey_z:?}));\n\
                 ENDSEC;\nEND-ISO-10303-21;\n",
                prefix = self.length_unit_prefix,
                ref_height = self.elevation_of_ref_height,
                building_z = self.building_z,
                elevation = self.storey_elevation,
                storey_z = self.storey_z.unwrap_or(0.0),
            )
        }
    }

    const STOREY_IN_METRES: PlacedStorey = PlacedStorey {
        length_unit_prefix: "$",
        building_z: 10.0,
        elevation_of_ref_height: "$",
        storey_z: Some(3.0),
        storey_elevation: "3.",
    };

    fn storey_base_height(ifc: &str) -> (Option<f64>, ExtractionWarnings) {
        let (elements, warnings) = extracted(ifc);
        let storey = elements
            .iter()
            .find(|element| element.global_id == "storey")
            .expect("the storey");
        (storey.base_height, warnings)
    }

    fn assert_height(base_height: Option<f64>, above_origin: f64) {
        let height = base_height.expect("a base height");
        assert!(
            (height - (MODEL_ORIGIN.height + above_origin)).abs() < 1e-3,
            "{height} is not {above_origin} m above the origin"
        );
    }

    #[test]
    fn a_storey_is_placed_through_its_building_placement() {
        let (base_height, warnings) = storey_base_height(&STOREY_IN_METRES.ifc());
        assert_height(base_height, 13.0);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn elevation_of_ref_height_does_not_move_a_placed_storey() {
        let ifc = PlacedStorey {
            elevation_of_ref_height: "50.",
            storey_z: Some(0.0),
            storey_elevation: "0.",
            ..STOREY_IN_METRES
        }
        .ifc();
        let (base_height, _) = storey_base_height(&ifc);
        assert_height(base_height, 10.0);
    }

    #[test]
    fn a_storey_placement_outranks_an_elevation_that_disagrees() {
        let ifc = PlacedStorey {
            building_z: 0.0,
            storey_z: Some(4.0),
            storey_elevation: "7.",
            ..STOREY_IN_METRES
        }
        .ifc();
        let (base_height, _) = storey_base_height(&ifc);
        assert_height(base_height, 4.0);
    }

    #[test]
    fn a_millimetre_project_places_storeys_in_metres() {
        let ifc = PlacedStorey {
            length_unit_prefix: ".MILLI.",
            building_z: 10_000.0,
            storey_z: Some(3_000.0),
            storey_elevation: "3000.",
            ..STOREY_IN_METRES
        }
        .ifc();
        let (base_height, warnings) = storey_base_height(&ifc);
        assert_height(base_height, 13.0);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_storey_with_no_placement_is_its_elevation_above_its_building() {
        let ifc = PlacedStorey {
            length_unit_prefix: ".MILLI.",
            building_z: 10_000.0,
            storey_z: None,
            storey_elevation: "3000.",
            ..STOREY_IN_METRES
        }
        .ifc();
        let (base_height, warnings) = storey_base_height(&ifc);
        assert_height(base_height, 13.0);
        assert_eq!(
            warnings,
            ExtractionWarnings::from([(WARNING_STOREY_HEIGHT_FROM_ELEVATION, 1)])
        );
    }

    #[test]
    fn a_storey_placement_that_does_not_resolve_leaves_no_base_height() {
        let ifc = STOREY_IN_METRES.ifc().replace(
            "#21=IFCLOCALPLACEMENT(#11,#22)",
            "#21=IFCLOCALPLACEMENT(#11,$)",
        );
        let (base_height, warnings) = storey_base_height(&ifc);
        assert_eq!(base_height, None);
        assert_eq!(
            warnings,
            ExtractionWarnings::from([(WARNING_UNRESOLVED_STOREY_PLACEMENT, 1)])
        );
    }

    #[test]
    fn a_building_placement_that_does_not_resolve_is_counted_under_the_building() {
        let ifc = PlacedStorey {
            storey_z: None,
            ..STOREY_IN_METRES
        }
        .ifc()
        .replace("'Main',$,$,#11,", "'Main',$,$,#99,");
        let (base_height, warnings) = storey_base_height(&ifc);
        assert_eq!(base_height, None);
        assert_eq!(
            warnings,
            ExtractionWarnings::from([(WARNING_UNRESOLVED_BUILDING_PLACEMENT, 1)])
        );
    }

    #[test]
    fn malformed_relations_end_in_named_warnings() {
        let (elements, warnings) = extracted(CYCLIC_IFC);
        assert_eq!(warnings.get(WARNING_SPATIAL_CYCLE), Some(&2));
        assert_eq!(warnings.get(WARNING_DUPLICATE_GLOBAL_ID), Some(&1));
        assert_eq!(warnings.get(WARNING_MISSING_GLOBAL_ID), Some(&1));

        let walls: Vec<_> = elements
            .iter()
            .filter(|element| element.global_id == "duplicate")
            .collect();
        assert_eq!(walls.len(), 1);
        assert_eq!(walls[0].name.as_deref(), Some("First"));
        assert!(
            walls[0].properties.is_empty(),
            "a usage naming itself resolves to no material: {:?}",
            walls[0].properties
        );
    }

    #[test]
    fn a_refusal_from_the_row_writer_stops_the_extraction() {
        let mut calls = 0;
        let result = extract_elements(
            LEGACY_DOOR_IFC.as_bytes(),
            &HashMap::new(),
            MODEL_ORIGIN,
            None,
            |_| {
                calls += 1;
                Err(IngestError::ElementRefused("disk full".to_string()))
            },
        );
        assert!(matches!(result, Err(IngestError::ElementRefused(_))));
        assert_eq!(calls, 1);
    }

    const WIDE_BUDGET: u64 = u64::MAX;

    fn wide_budgets() -> ExtractionBudgets {
        ExtractionBudgets {
            property_rows: WIDE_BUDGET,
            text_bytes: WIDE_BUDGET,
            visits: WIDE_BUDGET,
            deadline: Instant::now() + Duration::from_secs(3600),
        }
    }

    fn ifc_with(data: &str) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\n\
             FILE_NAME('hostile.ifc','2026-11-05',(''),(''),'','','');\n\
             FILE_SCHEMA(('IFC4'));\nENDSEC;\nDATA;\n\
             #1=IFCPROJECT('project',$,'Project',$,$,$,$,$,$);\n\
             {data}\nENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn extraction_within(
        ifc: &str,
        budgets: ExtractionBudgets,
    ) -> Result<(Vec<ExtractedElement>, ExtractionWarnings), IngestError> {
        let mut elements = Vec::new();
        let scanned = crate::ifc_reader::scan_ifc(ifc.as_bytes());
        let warnings = extract_elements_within(
            ifc.as_bytes(),
            &scanned.scan,
            &HashMap::new(),
            MODEL_ORIGIN,
            None,
            budgets,
            |element| {
                elements.push(element);
                Ok(())
            },
        )?;
        Ok((elements, warnings))
    }

    fn refusal_within(ifc: &str, budgets: ExtractionBudgets) -> String {
        extraction_within(ifc, budgets)
            .expect_err("a refusal")
            .to_string()
    }

    fn references_to(id: u32, count: usize) -> String {
        vec![format!("#{id}"); count].join(",")
    }

    #[test]
    fn a_relation_multiplying_objects_by_definitions_is_refused_before_it_is_stored() {
        let ifc = ifc_with(
            "#2=IFCWALL('a',$,'A',$,$,$,$,$,$);\n\
             #3=IFCWALL('b',$,'B',$,$,$,$,$,$);\n\
             #4=IFCPROPERTYSET('p',$,'P',$,());\n\
             #5=IFCPROPERTYSET('q',$,'Q',$,());\n\
             #6=IFCRELDEFINESBYPROPERTIES('r',$,$,$,(#2,#3),(#4,#5));\n",
        );

        let error = refusal_within(
            &ifc,
            ExtractionBudgets {
                property_rows: 3,
                ..wide_budgets()
            },
        );
        assert!(error.contains("IfcRelDefinesByProperties"), "{error}");

        let admitted = ExtractionBudgets {
            property_rows: 4,
            ..wide_budgets()
        };
        assert!(extraction_within(&ifc, admitted).is_ok());
    }

    #[test]
    fn stored_text_is_cut_at_a_character_boundary_with_a_warning() {
        let mut warnings = ExtractionWarnings::new();

        let cut = stored_text(&"é".repeat(MAX_STORED_TEXT_BYTES), &mut warnings);

        assert_eq!(cut.len(), MAX_STORED_TEXT_BYTES);
        assert_eq!(warnings.get(WARNING_TEXT_TRUNCATED), Some(&1));

        let cut = stored_text(
            &format!("x{}", "é".repeat(MAX_STORED_TEXT_BYTES)),
            &mut warnings,
        );

        assert_eq!(cut.len(), MAX_STORED_TEXT_BYTES - 1);
        assert_eq!(warnings.get(WARNING_TEXT_TRUNCATED), Some(&2));

        let kept = stored_text(&"x".repeat(MAX_STORED_TEXT_BYTES), &mut warnings);

        assert_eq!(kept.len(), MAX_STORED_TEXT_BYTES);
        assert_eq!(warnings.get(WARNING_TEXT_TRUNCATED), Some(&2));
    }

    #[test]
    fn an_element_name_past_the_text_cap_is_cut_with_a_warning() {
        let ifc = ifc_with(&format!(
            "#2=IFCWALL('a',$,'{}',$,$,$,$,$,$);\n",
            "x".repeat(MAX_STORED_TEXT_BYTES + 1)
        ));

        let (elements, warnings) = extraction_within(&ifc, wide_budgets()).unwrap();

        let wall = elements.iter().find(|e| e.global_id == "a").unwrap();
        assert_eq!(wall.name.as_ref().unwrap().len(), MAX_STORED_TEXT_BYTES);
        assert_eq!(warnings.get(WARNING_TEXT_TRUNCATED), Some(&1));
    }

    #[test]
    fn a_material_list_past_the_cap_is_cut_before_its_members_decode() {
        let ifc = ifc_with(&format!(
            "#2=IFCMATERIAL('Concrete');\n#3=IFCMATERIALLIST(({}));\n",
            references_to(2, MAX_PROPERTIES_PER_ELEMENT + 1)
        ));
        let mut decoder =
            EntityDecoder::with_index(ifc.as_bytes(), build_entity_index(ifc.as_bytes()));
        let list = decoder.decode_by_id(3).unwrap();
        let mut warnings = ExtractionWarnings::new();

        let members = material_list_members(&mut decoder, &list, &mut warnings);

        assert_eq!(members.len(), MAX_PROPERTIES_PER_ELEMENT);
        assert_eq!(warnings.get(WARNING_PROPERTIES_TRUNCATED), Some(&1));
    }

    #[test]
    fn materials_that_do_not_decode_are_counted_and_the_rest_still_read() {
        let ifc = ifc_with(
            "#2=IFCMATERIAL('Concrete');\n\
             #3=IFCMATERIALLAYER(#2,200.,$,$,$,$,$);\n\
             #4=IFCMATERIALLAYERSET((#3,#90),'Wall 200',$);\n\
             #5=IFCWALL('layered',$,'A',$,$,$,$,$,$);\n\
             #6=IFCRELASSOCIATESMATERIAL('r1',$,$,$,(#5),#4);\n\
             #7=IFCWALL('missing',$,'B',$,$,$,$,$,$);\n\
             #8=IFCRELASSOCIATESMATERIAL('r2',$,$,$,(#7),#91);\n",
        );

        let (elements, warnings) = extraction_within(&ifc, wide_budgets()).unwrap();

        assert_eq!(warnings.get(WARNING_UNDECODABLE_MATERIAL), Some(&2));
        let layered = elements.iter().find(|e| e.global_id == "layered").unwrap();
        assert_eq!(
            property(layered, "Material[0].Name").value,
            PropertyValue::Text("Concrete".to_string())
        );
    }

    #[test]
    fn quantities_that_are_not_quantities_are_counted() {
        let ifc = ifc_with(
            "#2=IFCQUANTITYLENGTH('Length',$,$,4.,$);\n\
             #3=IFCPROPERTYSINGLEVALUE('Colour',$,IFCLABEL('red'),$);\n\
             #4=IFCELEMENTQUANTITY('q',$,'Q',$,$,(#2,#3));\n\
             #5=IFCWALL('a',$,'A',$,$,$,$,$,$);\n\
             #6=IFCRELDEFINESBYPROPERTIES('r',$,$,$,(#5),#4);\n",
        );

        let (elements, warnings) = extraction_within(&ifc, wide_budgets()).unwrap();

        assert_eq!(warnings.get(WARNING_REJECTED_QUANTITY), Some(&1));
        let wall = elements.iter().find(|e| e.global_id == "a").unwrap();
        assert_eq!(property(wall, "Q.Length").value, PropertyValue::Number(4.0));
    }

    #[test]
    fn a_property_set_past_the_cap_is_left_out_with_a_warning() {
        let ifc = ifc_with(&format!(
            "#2=IFCPROPERTYSINGLEVALUE('Value',$,IFCLABEL('v'),$);\n\
             #3=IFCPROPERTYSET('p',$,'P',$,({}));\n\
             #4=IFCWALL('a',$,'A',$,$,$,$,$,$);\n\
             #5=IFCRELDEFINESBYPROPERTIES('r',$,$,$,(#4),#3);\n",
            references_to(2, MAX_PROPERTIES_PER_ELEMENT + 1)
        ));

        let (elements, warnings) = extraction_within(&ifc, wide_budgets()).unwrap();

        let wall = elements.iter().find(|e| e.global_id == "a").unwrap();
        assert!(wall.properties.is_empty(), "{:?}", wall.properties);
        assert_eq!(warnings.get(WARNING_PROPERTIES_TRUNCATED), Some(&1));
    }

    const SET_REPEATING_ONE_VALUE: &str = "\
#2=IFCPROPERTYSINGLEVALUE('Value',$,IFCLABEL('a value repeated by reference'),$);
#3=IFCPROPERTYSET('p',$,'P',$,(#2,#2,#2,#2,#2,#2,#2,#2));
#4=IFCWALL('a',$,'A',$,$,$,$,$,$);
#5=IFCRELDEFINESBYPROPERTIES('r',$,$,$,(#4),#3);
";

    #[test]
    fn a_set_repeating_one_value_is_charged_per_reference_before_it_decodes() {
        let ifc = ifc_with(SET_REPEATING_ONE_VALUE);
        let one_member_bytes =
            EntityDecoder::with_index(ifc.as_bytes(), build_entity_index(ifc.as_bytes()))
                .get_raw_bytes(2)
                .unwrap()
                .len() as u64;

        let error = refusal_within(
            &ifc,
            ExtractionBudgets {
                text_bytes: 8 * one_member_bytes - 1,
                ..wide_budgets()
            },
        );

        assert!(error.contains("property text"), "{error}");
    }

    #[test]
    fn element_text_past_the_budget_refuses_the_model() {
        let ifc = ifc_with("#2=IFCWALL('a',$,'A wall name',$,$,$,$,$,$);\n");
        let (elements, _) = extraction_within(&ifc, wide_budgets()).unwrap();
        let written: u64 = elements.iter().map(|e| e.text_bytes() as u64).sum();

        let admitted = ExtractionBudgets {
            text_bytes: written,
            ..wide_budgets()
        };
        assert!(extraction_within(&ifc, admitted).is_ok());
        let error = refusal_within(
            &ifc,
            ExtractionBudgets {
                text_bytes: written - 1,
                ..wide_budgets()
            },
        );

        assert!(error.contains("property text"), "{error}");
    }

    const WALLS_OF_ONE_TYPE_WITH_REPEATED_DEFINITIONS: &str = "\
#2=IFCPROPERTYSET('p',$,'P',$,());
#3=IFCWALLTYPE('type',$,'T',$,$,(#2,#2,#2,#2,#2,#2),$,$,$,.STANDARD.);
#4=IFCWALL('a',$,'A',$,$,$,$,$,$);
#5=IFCWALL('b',$,'B',$,$,$,$,$,$);
#6=IFCRELDEFINESBYTYPE('r',$,$,$,(#4,#5),#3);
";

    #[test]
    fn definitions_visited_are_charged_even_when_they_emit_no_rows() {
        let ifc = ifc_with(WALLS_OF_ONE_TYPE_WITH_REPEATED_DEFINITIONS);

        let error = refusal_within(
            &ifc,
            ExtractionBudgets {
                visits: 12,
                ..wide_budgets()
            },
        );
        assert!(error.contains("visits"), "{error}");

        let admitted = ExtractionBudgets {
            visits: 1000,
            ..wide_budgets()
        };
        assert!(extraction_within(&ifc, admitted).is_ok());
    }

    #[test]
    fn a_passed_deadline_stops_the_extraction_loop() {
        let ifc = ifc_with(WALLS_OF_ONE_TYPE_WITH_REPEATED_DEFINITIONS);

        let error = refusal_within(
            &ifc,
            ExtractionBudgets {
                deadline: Instant::now(),
                ..wide_budgets()
            },
        );

        assert!(error.contains("seconds"), "{error}");
    }

    #[test]
    fn locations_bound_every_mesh_of_an_element_at_the_origin_frame() {
        let origin = ModelOrigin {
            longitude: 10.0,
            latitude: 20.0,
            height: 100.0,
            x_axis_from_east_radians: 0.0,
        };
        let first: &[[f64; 3]] = &[[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let second: &[[f64; 3]] = &[[1.0, 0.0, 0.0], [3.0, 0.0, 0.0]];
        let far: &[[f64; 3]] = &[[0.0, 0.0, 50.0]];
        let locations = element_locations(
            [("wall", first), ("wall", second), ("lamp", far)].into_iter(),
            origin,
        );

        let wall = locations["wall"];
        assert!((wall.radius - 2.0).abs() < 1e-9, "{wall:?}");
        assert!((wall.latitude - 20.0).abs() < 1e-6, "{wall:?}");
        // one metre east of the origin at 20 degrees north
        let expected_longitude =
            10.0 + (1.0 / (6_378_137.0 * 20f64.to_radians().cos())).to_degrees();
        assert!(
            (wall.longitude - expected_longitude).abs() < 1e-6,
            "{wall:?}"
        );
        assert!((wall.height - 100.0).abs() < 1e-3, "{wall:?}");

        let lamp = locations["lamp"];
        assert_eq!(lamp.radius, 0.0);
        assert!((lamp.height - 150.0).abs() < 1e-3, "{lamp:?}");
    }

    #[test]
    fn locations_turn_with_the_model_frame() {
        let origin = ModelOrigin {
            longitude: 10.0,
            latitude: 20.0,
            height: 0.0,
            x_axis_from_east_radians: std::f64::consts::FRAC_PI_2,
        };
        let on_model_x: &[[f64; 3]] = &[[100.0, 0.0, 0.0]];
        let location = element_locations([("wall", on_model_x)].into_iter(), origin)["wall"];
        // a quarter turn points model x north, and a degree of latitude is about 110 km
        assert!((location.longitude - 10.0).abs() < 1e-6, "{location:?}");
        assert!((location.latitude - 20.0009).abs() < 1e-4, "{location:?}");
    }

    #[test]
    fn the_budgets_grow_with_the_file_above_their_floors() {
        let small = ExtractionBudgets::for_input(10);
        assert_eq!(small.property_rows, MIN_PROPERTY_ROW_BUDGET);
        assert_eq!(small.text_bytes, MIN_TEXT_BYTE_BUDGET);

        let large = ExtractionBudgets::for_input(1 << 27);
        assert_eq!(large.property_rows, 1 << 27);
        assert_eq!(large.text_bytes, 1 << 31);
        assert_eq!(large.visits, (1 << 27) * VISITS_PER_PROPERTY_ROW);
    }
}
