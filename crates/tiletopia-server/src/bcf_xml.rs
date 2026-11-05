use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use quick_xml::Reader;
use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const BCF_VERSION: &str = "3.0";
const VERSION_FILE: &str = "bcf.version";
const EXTENSIONS_FILE: &str = "extensions.xml";
const PROJECT_FILE: &str = "project.bcfp";
const DOCUMENTS_FILE: &str = "documents.xml";
const MARKUP_FILE: &str = "markup.bcf";

const MOST_XML_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MOST_XML_BYTES_PER_ARCHIVE: u64 = 256 * 1024 * 1024;
const MOST_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
const MOST_XML_DEPTH: usize = 32;

const PNG_SIGNATURE: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const JPEG_SIGNATURE: &[u8] = &[0xFF, 0xD8, 0xFF];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfExtensions {
    #[serde(default)]
    pub topic_types: Vec<String>,
    #[serde(default)]
    pub topic_statuses: Vec<String>,
    #[serde(default)]
    pub priorities: Vec<String>,
    #[serde(default)]
    pub topic_labels: Vec<String>,
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub snippet_types: Vec<String>,
    #[serde(default)]
    pub stages: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfProject {
    pub project_id: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub extensions: BcfExtensions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfFile {
    pub ifc_project: Option<String>,
    pub ifc_spatial_structure_element: Option<String>,
    pub is_external: bool,
    pub filename: Option<String>,
    pub date: Option<String>,
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfExternalSnippet {
    pub snippet_type: String,
    pub reference: String,
    pub reference_schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfExternalDocument {
    pub guid: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfComment {
    pub guid: String,
    pub date: String,
    pub author: String,
    pub comment: Option<String>,
    pub viewpoint_guid: Option<String>,
    pub modified_date: Option<String>,
    pub modified_author: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfComponent {
    pub ifc_guid: Option<String>,
    pub originating_system: Option<String>,
    pub authoring_tool_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfVisibility {
    pub default_visibility: bool,
    pub spaces_visible: bool,
    pub space_boundaries_visible: bool,
    pub openings_visible: bool,
    #[serde(default)]
    pub exceptions: Vec<BcfComponent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfColoring {
    pub color: String,
    pub components: Vec<BcfComponent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BcfCamera {
    Orthogonal {
        camera_view_point: [f64; 3],
        camera_direction: [f64; 3],
        camera_up_vector: [f64; 3],
        view_to_world_scale: f64,
        aspect_ratio: f64,
    },
    Perspective {
        camera_view_point: [f64; 3],
        camera_direction: [f64; 3],
        camera_up_vector: [f64; 3],
        field_of_view: f64,
        aspect_ratio: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfLine {
    pub start_point: [f64; 3],
    pub end_point: [f64; 3],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfClippingPlane {
    pub location: [f64; 3],
    pub direction: [f64; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotFormat {
    Png,
    Jpg,
}

impl SnapshotFormat {
    pub fn extension(self) -> &'static str {
        match self {
            SnapshotFormat::Png => "png",
            SnapshotFormat::Jpg => "jpg",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            SnapshotFormat::Png => "image/png",
            SnapshotFormat::Jpg => "image/jpeg",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfViewpoint {
    pub guid: String,
    pub index: Option<i32>,
    pub snapshot_format: Option<SnapshotFormat>,
    pub camera: Option<BcfCamera>,
    #[serde(default)]
    pub selection: Vec<BcfComponent>,
    pub visibility: Option<BcfVisibility>,
    #[serde(default)]
    pub coloring: Vec<BcfColoring>,
    #[serde(default)]
    pub lines: Vec<BcfLine>,
    #[serde(default)]
    pub clipping_planes: Vec<BcfClippingPlane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BcfTopic {
    pub guid: String,
    pub server_assigned_id: Option<String>,
    pub topic_type: String,
    pub topic_status: String,
    #[serde(default)]
    pub reference_links: Vec<String>,
    pub title: String,
    pub priority: Option<String>,
    pub index: Option<i32>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub creation_date: String,
    pub creation_author: String,
    pub modified_date: Option<String>,
    pub modified_author: Option<String>,
    pub due_date: Option<String>,
    pub assigned_to: Option<String>,
    pub stage: Option<String>,
    pub description: Option<String>,
    pub external_bim_snippet: Option<BcfExternalSnippet>,
    #[serde(default)]
    pub external_documents: Vec<BcfExternalDocument>,
    #[serde(default)]
    pub related_topics: Vec<String>,
    #[serde(default)]
    pub files: Vec<BcfFile>,
    #[serde(default)]
    pub comments: Vec<BcfComment>,
    #[serde(default)]
    pub viewpoints: Vec<BcfViewpoint>,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DroppedContent {
    pub(crate) documents: u64,
    pub(crate) internal_document_references: u64,
    pub(crate) internal_bim_snippets: u64,
    pub(crate) bitmaps: u64,
    pub(crate) snapshots_not_png_or_jpeg: u64,
    pub(crate) missing_viewpoint_files: u64,
    pub(crate) missing_snapshot_files: u64,
}

pub(crate) struct ImportedSnapshot {
    pub(crate) topic_guid: String,
    pub(crate) viewpoint_guid: String,
    pub(crate) source: PathBuf,
    pub(crate) format: SnapshotFormat,
}

pub(crate) struct BcfArchive {
    pub(crate) project: BcfProject,
    pub(crate) topics: Vec<BcfTopic>,
    pub(crate) snapshots: Vec<ImportedSnapshot>,
    pub(crate) dropped: DroppedContent,
}

struct XmlNode {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<XmlNode>,
}

fn opened_node(start: &BytesStart<'_>) -> Result<XmlNode, String> {
    let mut attributes = Vec::new();
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let name = String::from_utf8_lossy(attribute.key.local_name().as_ref()).into_owned();
        let value = attribute
            .unescape_value()
            .map_err(|error| error.to_string())?
            .into_owned();
        attributes.push((name, value));
    }
    Ok(XmlNode {
        name: String::from_utf8_lossy(start.local_name().as_ref()).into_owned(),
        attributes,
        text: String::new(),
        children: Vec::new(),
    })
}

fn parse_xml(bytes: &[u8]) -> Result<XmlNode, String> {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut open: Vec<XmlNode> = Vec::new();
    let mut root = None;
    loop {
        let event = reader.read_event().map_err(|error| error.to_string())?;
        match event {
            Event::Start(start) => {
                if open.len() >= MOST_XML_DEPTH {
                    return Err(format!("elements nest deeper than {MOST_XML_DEPTH} levels"));
                }
                if open.is_empty() && root.is_some() {
                    return Err("the document has two root elements".to_string());
                }
                open.push(opened_node(&start)?);
            }
            Event::End(_) => {
                let closed = open.pop().ok_or("an end tag with no start tag")?;
                match open.last_mut() {
                    Some(parent) => parent.children.push(closed),
                    None => root = Some(closed),
                }
            }
            Event::Text(text) => {
                if let Some(node) = open.last_mut() {
                    let content = text.xml10_content().map_err(|error| error.to_string())?;
                    node.text.push_str(&content);
                }
            }
            Event::CData(data) => {
                if let Some(node) = open.last_mut() {
                    let content = data.xml10_content().map_err(|error| error.to_string())?;
                    node.text.push_str(&content);
                }
            }
            Event::GeneralRef(reference) => {
                let Some(node) = open.last_mut() else {
                    continue;
                };
                if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|error| error.to_string())?
                {
                    node.text.push(character);
                    continue;
                }
                let name = reference.decode().map_err(|error| error.to_string())?;
                let resolved = resolve_predefined_entity(&name)
                    .ok_or_else(|| format!("the entity &{name}; is not one XML defines"))?;
                node.text.push_str(resolved);
            }
            // a document type declaration is how an XML file defines the entities used for expansion attacks
            Event::DocType(_) => {
                return Err("a document type declaration is refused".to_string());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !open.is_empty() {
        return Err("the document ends inside an element".to_string());
    }
    root.ok_or_else(|| "the document has no root element".to_string())
}

impl XmlNode {
    fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|child| child.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlNode> {
        self.children.iter().filter(move |child| child.name == name)
    }

    fn grandchildren<'a>(&'a self, list: &str, item: &'a str) -> Vec<&'a XmlNode> {
        match self.child(list) {
            Some(list) => list.children_named(item).collect(),
            None => Vec::new(),
        }
    }

    fn attribute(&self, name: &str) -> Option<String> {
        self.attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    fn own_text(&self) -> Option<String> {
        let text = self.text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn child_text(&self, name: &str) -> Option<String> {
        self.child(name).and_then(XmlNode::own_text)
    }

    fn required_child_text(&self, name: &str) -> Result<String, String> {
        self.child_text(name)
            .ok_or_else(|| format!("{} has no {name}", self.name))
    }

    fn required_attribute(&self, name: &str) -> Result<String, String> {
        self.attribute(name)
            .ok_or_else(|| format!("{} has no {name} attribute", self.name))
    }

    fn texts_of(&self, list: &str, item: &str) -> Vec<String> {
        self.grandchildren(list, item)
            .into_iter()
            .filter_map(XmlNode::own_text)
            .collect()
    }

    fn boolean_attribute(&self, name: &str, default: bool) -> Result<bool, String> {
        match self.attribute(name).as_deref() {
            None => Ok(default),
            Some("true" | "1") => Ok(true),
            Some("false" | "0") => Ok(false),
            Some(other) => Err(format!("{name} {other:?} is not a boolean")),
        }
    }

    fn number(&self, name: &str) -> Result<f64, String> {
        let text = self.required_child_text(name)?;
        let number: f64 = text
            .parse()
            .map_err(|_| format!("{} {name} {text:?} is not a number", self.name))?;
        if !number.is_finite() {
            return Err(format!("{} {name} {text:?} is not finite", self.name));
        }
        Ok(number)
    }

    fn optional_integer(&self, name: &str) -> Result<Option<i32>, String> {
        self.child_text(name)
            .map(|text| {
                text.parse()
                    .map_err(|_| format!("{} {name} {text:?} is not an integer", self.name))
            })
            .transpose()
    }

    fn point(&self, name: &str) -> Result<[f64; 3], String> {
        let point = self
            .child(name)
            .ok_or_else(|| format!("{} has no {name}", self.name))?;
        Ok([point.number("X")?, point.number("Y")?, point.number("Z")?])
    }
}

fn canonical_guid(guid: &str) -> Result<String, String> {
    Uuid::try_parse(guid)
        .map(|guid| guid.to_string())
        .map_err(|_| format!("{guid:?} is not a BCF Guid"))
}

fn is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn is_color(color: &str) -> bool {
    matches!(color.len(), 6 | 8) && color.chars().all(|digit| digit.is_ascii_hexdigit())
}

fn checked_camera(camera: &BcfCamera) -> Result<(), String> {
    let (vectors, aspect_ratio) = match camera {
        BcfCamera::Orthogonal {
            camera_view_point,
            camera_direction,
            camera_up_vector,
            view_to_world_scale,
            aspect_ratio,
        } => {
            if !view_to_world_scale.is_finite() {
                return Err("viewToWorldScale is not finite".to_string());
            }
            (
                [camera_view_point, camera_direction, camera_up_vector],
                *aspect_ratio,
            )
        }
        BcfCamera::Perspective {
            camera_view_point,
            camera_direction,
            camera_up_vector,
            field_of_view,
            aspect_ratio,
        } => {
            let is_in_range = *field_of_view > 0.0 && *field_of_view < 180.0;
            if !is_in_range {
                return Err(format!(
                    "fieldOfView {field_of_view} is outside 0 to 180 degrees"
                ));
            }
            (
                [camera_view_point, camera_direction, camera_up_vector],
                *aspect_ratio,
            )
        }
    };
    let is_positive = aspect_ratio > 0.0 && aspect_ratio.is_finite();
    if !is_positive {
        return Err(format!("aspectRatio {aspect_ratio} is not positive"));
    }
    let all_finite = vectors
        .iter()
        .all(|vector| vector.iter().all(|coordinate| coordinate.is_finite()));
    if !all_finite {
        return Err("a camera vector is not finite".to_string());
    }
    Ok(())
}

pub(crate) fn checked_topic(mut topic: BcfTopic) -> Result<BcfTopic, String> {
    topic.guid = canonical_guid(&topic.guid)?;
    let required = [
        ("title", &topic.title),
        ("topicType", &topic.topic_type),
        ("topicStatus", &topic.topic_status),
        ("creationDate", &topic.creation_date),
        ("creationAuthor", &topic.creation_author),
    ];
    for (name, value) in required {
        if is_blank(value) {
            return Err(format!("topic {}: {name} is required", topic.guid));
        }
    }
    for related in &mut topic.related_topics {
        *related = canonical_guid(related)?;
    }
    for document in &mut topic.external_documents {
        document.guid = canonical_guid(&document.guid)?;
    }
    for comment in &mut topic.comments {
        comment.guid = canonical_guid(&comment.guid)?;
        if is_blank(&comment.date) || is_blank(&comment.author) {
            return Err(format!(
                "comment {}: date and author are required",
                comment.guid
            ));
        }
        if let Some(viewpoint) = &comment.viewpoint_guid {
            comment.viewpoint_guid = Some(canonical_guid(viewpoint)?);
        }
    }
    for viewpoint in &mut topic.viewpoints {
        viewpoint.guid = canonical_guid(&viewpoint.guid)?;
        match &viewpoint.camera {
            Some(camera) => checked_camera(camera)?,
            None => {
                let holds_view_state = !viewpoint.selection.is_empty()
                    || viewpoint.visibility.is_some()
                    || !viewpoint.coloring.is_empty()
                    || !viewpoint.lines.is_empty()
                    || !viewpoint.clipping_planes.is_empty();
                // visinfo.xsd requires a camera in every viewpoint file
                if holds_view_state {
                    return Err(format!(
                        "viewpoint {} has view state and no camera",
                        viewpoint.guid
                    ));
                }
            }
        }
        for coloring in &viewpoint.coloring {
            if !is_color(&coloring.color) {
                return Err(format!(
                    "color {:?} is not 6 or 8 hex digits",
                    coloring.color
                ));
            }
        }
        let all_finite = viewpoint
            .lines
            .iter()
            .flat_map(|line| [line.start_point, line.end_point])
            .chain(
                viewpoint
                    .clipping_planes
                    .iter()
                    .flat_map(|plane| [plane.location, plane.direction]),
            )
            .all(|point| point.iter().all(|coordinate| coordinate.is_finite()));
        if !all_finite {
            return Err(format!(
                "viewpoint {}: a line or clipping plane coordinate is not finite",
                viewpoint.guid
            ));
        }
    }
    Ok(topic)
}

fn read_component(node: &XmlNode) -> BcfComponent {
    BcfComponent {
        ifc_guid: node.attribute("IfcGuid"),
        originating_system: node.child_text("OriginatingSystem"),
        authoring_tool_id: node.child_text("AuthoringToolId"),
    }
}

fn read_camera(root: &XmlNode) -> Result<Option<BcfCamera>, String> {
    if let Some(camera) = root.child("OrthogonalCamera") {
        return Ok(Some(BcfCamera::Orthogonal {
            camera_view_point: camera.point("CameraViewPoint")?,
            camera_direction: camera.point("CameraDirection")?,
            camera_up_vector: camera.point("CameraUpVector")?,
            view_to_world_scale: camera.number("ViewToWorldScale")?,
            aspect_ratio: camera.number("AspectRatio")?,
        }));
    }
    if let Some(camera) = root.child("PerspectiveCamera") {
        return Ok(Some(BcfCamera::Perspective {
            camera_view_point: camera.point("CameraViewPoint")?,
            camera_direction: camera.point("CameraDirection")?,
            camera_up_vector: camera.point("CameraUpVector")?,
            field_of_view: camera.number("FieldOfView")?,
            aspect_ratio: camera.number("AspectRatio")?,
        }));
    }
    Ok(None)
}

fn read_visibility(node: &XmlNode) -> Result<BcfVisibility, String> {
    let hints = node.child("ViewSetupHints");
    let hint = |name: &str| match hints {
        Some(hints) => hints.boolean_attribute(name, false),
        None => Ok(false),
    };
    Ok(BcfVisibility {
        default_visibility: node.boolean_attribute("DefaultVisibility", false)?,
        spaces_visible: hint("SpacesVisible")?,
        space_boundaries_visible: hint("SpaceBoundariesVisible")?,
        openings_visible: hint("OpeningsVisible")?,
        exceptions: node
            .grandchildren("Exceptions", "Component")
            .into_iter()
            .map(read_component)
            .collect(),
    })
}

fn read_viewpoint_file(
    viewpoint: &mut BcfViewpoint,
    root: &XmlNode,
    dropped: &mut DroppedContent,
) -> Result<(), String> {
    if root.name != "VisualizationInfo" {
        return Err(format!("the root element is {}", root.name));
    }
    viewpoint.camera = read_camera(root)?;
    if let Some(components) = root.child("Components") {
        viewpoint.selection = components
            .grandchildren("Selection", "Component")
            .into_iter()
            .map(read_component)
            .collect();
        viewpoint.visibility = components
            .child("Visibility")
            .map(read_visibility)
            .transpose()?;
        viewpoint.coloring = components
            .grandchildren("Coloring", "Color")
            .into_iter()
            .map(|color| {
                Ok(BcfColoring {
                    color: color.required_attribute("Color")?,
                    components: color
                        .grandchildren("Components", "Component")
                        .into_iter()
                        .map(read_component)
                        .collect(),
                })
            })
            .collect::<Result<_, String>>()?;
    }
    viewpoint.lines = root
        .grandchildren("Lines", "Line")
        .into_iter()
        .map(|line| {
            Ok(BcfLine {
                start_point: line.point("StartPoint")?,
                end_point: line.point("EndPoint")?,
            })
        })
        .collect::<Result<_, String>>()?;
    viewpoint.clipping_planes = root
        .grandchildren("ClippingPlanes", "ClippingPlane")
        .into_iter()
        .map(|plane| {
            Ok(BcfClippingPlane {
                location: plane.point("Location")?,
                direction: plane.point("Direction")?,
            })
        })
        .collect::<Result<_, String>>()?;
    dropped.bitmaps += root.grandchildren("Bitmaps", "Bitmap").len() as u64;
    Ok(())
}

fn read_capped(path: &Path, most_bytes: u64) -> Result<Vec<u8>, String> {
    let describe = |error: std::io::Error| format!("{}: {error}", path.display());
    let bytes = std::fs::metadata(path).map_err(describe)?.len();
    if bytes > most_bytes {
        return Err(format!(
            "{} is {bytes} bytes, past the {most_bytes} byte limit",
            path.display()
        ));
    }
    std::fs::read(path).map_err(describe)
}

struct ArchiveReading<'a> {
    directory: &'a Path,
    xml_bytes_left: u64,
    dropped: DroppedContent,
    snapshots: Vec<ImportedSnapshot>,
}

impl ArchiveReading<'_> {
    fn parse(&mut self, relative_path: &Path) -> Result<XmlNode, String> {
        let name = relative_path.display().to_string();
        let bytes = read_capped(&self.directory.join(relative_path), MOST_XML_FILE_BYTES)
            .map_err(|reason| format!("{name}: {reason}"))?;
        self.xml_bytes_left = self
            .xml_bytes_left
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| {
                format!("the archive holds more than {MOST_XML_BYTES_PER_ARCHIVE} bytes of XML")
            })?;
        parse_xml(&bytes).map_err(|reason| format!("{name}: {reason}"))
    }
}

fn file_in_topic_folder(folder: &Path, name: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) if !name.contains(['/', '\\', ':']) => {
            Ok(folder.join(name))
        }
        _ => Err(format!("{name:?} is not one file name in the topic folder")),
    }
}

fn snapshot_format_of(path: &Path) -> Result<Option<SnapshotFormat>, String> {
    let describe = |error: std::io::Error| format!("{}: {error}", path.display());
    let mut signature = [0u8; 8];
    let mut file = File::open(path).map_err(describe)?;
    let read = file.read(&mut signature).map_err(describe)?;
    if signature[..read].starts_with(PNG_SIGNATURE) {
        return Ok(Some(SnapshotFormat::Png));
    }
    if signature[..read].starts_with(JPEG_SIGNATURE) {
        return Ok(Some(SnapshotFormat::Jpg));
    }
    Ok(None)
}

fn read_markup_viewpoint(
    node: &XmlNode,
    relative_folder: &Path,
    topic_guid: &str,
    reading: &mut ArchiveReading<'_>,
) -> Result<BcfViewpoint, String> {
    let guid = canonical_guid(&node.required_attribute("Guid")?)?;
    let mut viewpoint = BcfViewpoint {
        guid: guid.clone(),
        index: node.optional_integer("Index")?,
        snapshot_format: None,
        camera: None,
        selection: Vec::new(),
        visibility: None,
        coloring: Vec::new(),
        lines: Vec::new(),
        clipping_planes: Vec::new(),
    };
    let topic_directory = reading.directory.join(relative_folder);

    if let Some(file_name) = node.child_text("Viewpoint") {
        let path = file_in_topic_folder(&topic_directory, &file_name)?;
        if path.is_file() {
            let relative = relative_folder.join(&file_name);
            let root = reading.parse(&relative)?;
            read_viewpoint_file(&mut viewpoint, &root, &mut reading.dropped)
                .map_err(|reason| format!("{}: {reason}", relative.display()))?;
        } else {
            reading.dropped.missing_viewpoint_files += 1;
        }
    }

    if let Some(file_name) = node.child_text("Snapshot") {
        let path = file_in_topic_folder(&topic_directory, &file_name)?;
        if !path.is_file() {
            reading.dropped.missing_snapshot_files += 1;
            return Ok(viewpoint);
        }
        let bytes = std::fs::metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
        if bytes > MOST_SNAPSHOT_BYTES {
            return Err(format!(
                "{file_name} is {bytes} bytes, past the {MOST_SNAPSHOT_BYTES} byte snapshot limit"
            ));
        }
        match snapshot_format_of(&path)? {
            Some(format) => {
                viewpoint.snapshot_format = Some(format);
                reading.snapshots.push(ImportedSnapshot {
                    topic_guid: topic_guid.to_string(),
                    viewpoint_guid: guid,
                    source: path,
                    format,
                });
            }
            None => reading.dropped.snapshots_not_png_or_jpeg += 1,
        }
    }
    Ok(viewpoint)
}

fn read_file_reference(node: &XmlNode) -> Result<BcfFile, String> {
    Ok(BcfFile {
        ifc_project: node.attribute("IfcProject"),
        ifc_spatial_structure_element: node.attribute("IfcSpatialStructureElement"),
        is_external: node.boolean_attribute("IsExternal", true)?,
        filename: node.child_text("Filename"),
        date: node.child_text("Date"),
        reference: node.child_text("Reference"),
    })
}

fn read_comment(node: &XmlNode) -> Result<BcfComment, String> {
    Ok(BcfComment {
        guid: node.required_attribute("Guid")?,
        date: node.required_child_text("Date")?,
        author: node.required_child_text("Author")?,
        comment: node.child_text("Comment"),
        viewpoint_guid: node
            .child("Viewpoint")
            .and_then(|viewpoint| viewpoint.attribute("Guid")),
        modified_date: node.child_text("ModifiedDate"),
        modified_author: node.child_text("ModifiedAuthor"),
    })
}

fn read_markup(
    root: &XmlNode,
    relative_folder: &Path,
    reading: &mut ArchiveReading<'_>,
) -> Result<BcfTopic, String> {
    if root.name != "Markup" {
        return Err(format!("the root element is {}", root.name));
    }
    let topic = root.child("Topic").ok_or("Markup has no Topic")?;
    let guid = canonical_guid(&topic.required_attribute("Guid")?)?;

    let external_bim_snippet = match topic.child("BimSnippet") {
        Some(snippet) if snippet.boolean_attribute("IsExternal", false)? => {
            Some(BcfExternalSnippet {
                snippet_type: snippet.required_attribute("SnippetType")?,
                reference: snippet.required_child_text("Reference")?,
                reference_schema: snippet.required_child_text("ReferenceSchema")?,
            })
        }
        Some(_) => {
            reading.dropped.internal_bim_snippets += 1;
            None
        }
        None => None,
    };

    let mut external_documents = Vec::new();
    for reference in topic.grandchildren("DocumentReferences", "DocumentReference") {
        let Some(url) = reference.child_text("Url") else {
            reading.dropped.internal_document_references += 1;
            continue;
        };
        external_documents.push(BcfExternalDocument {
            guid: reference.required_attribute("Guid")?,
            url,
            description: reference.child_text("Description"),
        });
    }

    let mut viewpoints = Vec::new();
    for viewpoint in topic.grandchildren("Viewpoints", "ViewPoint") {
        viewpoints.push(read_markup_viewpoint(
            viewpoint,
            relative_folder,
            &guid,
            reading,
        )?);
    }

    checked_topic(BcfTopic {
        guid: guid.clone(),
        server_assigned_id: topic.attribute("ServerAssignedId"),
        topic_type: topic.required_attribute("TopicType")?,
        topic_status: topic.required_attribute("TopicStatus")?,
        reference_links: topic.texts_of("ReferenceLinks", "ReferenceLink"),
        title: topic.required_child_text("Title")?,
        priority: topic.child_text("Priority"),
        index: topic.optional_integer("Index")?,
        labels: topic.texts_of("Labels", "Label"),
        creation_date: topic.required_child_text("CreationDate")?,
        creation_author: topic.required_child_text("CreationAuthor")?,
        modified_date: topic.child_text("ModifiedDate"),
        modified_author: topic.child_text("ModifiedAuthor"),
        due_date: topic.child_text("DueDate"),
        assigned_to: topic.child_text("AssignedTo"),
        stage: topic.child_text("Stage"),
        description: topic.child_text("Description"),
        external_bim_snippet,
        external_documents,
        related_topics: topic
            .grandchildren("RelatedTopics", "RelatedTopic")
            .into_iter()
            .map(|related| related.required_attribute("Guid"))
            .collect::<Result<_, String>>()?,
        files: match root.child("Header") {
            Some(header) => header
                .grandchildren("Files", "File")
                .into_iter()
                .map(read_file_reference)
                .collect::<Result<_, String>>()?,
            None => Vec::new(),
        },
        comments: topic
            .grandchildren("Comments", "Comment")
            .into_iter()
            .map(read_comment)
            .collect::<Result<_, String>>()?,
        viewpoints,
    })
}

fn read_extensions(root: &XmlNode) -> BcfExtensions {
    BcfExtensions {
        topic_types: root.texts_of("TopicTypes", "TopicType"),
        topic_statuses: root.texts_of("TopicStatuses", "TopicStatus"),
        priorities: root.texts_of("Priorities", "Priority"),
        topic_labels: root.texts_of("TopicLabels", "TopicLabel"),
        users: root.texts_of("Users", "User"),
        snippet_types: root.texts_of("SnippetTypes", "SnippetType"),
        stages: root.texts_of("Stages", "Stage"),
    }
}

pub(crate) fn read_archive(
    directory: &Path,
    relative_paths: &[PathBuf],
) -> Result<BcfArchive, String> {
    let mut reading = ArchiveReading {
        directory,
        xml_bytes_left: MOST_XML_BYTES_PER_ARCHIVE,
        dropped: DroppedContent::default(),
        snapshots: Vec::new(),
    };

    let has_file = |name: &str| relative_paths.iter().any(|path| path == Path::new(name));
    if !has_file(VERSION_FILE) {
        return Err(format!("the archive has no {VERSION_FILE}"));
    }
    let version = reading.parse(Path::new(VERSION_FILE))?;
    let version_id = version.attribute("VersionId");
    if version_id.as_deref() != Some(BCF_VERSION) {
        return Err(format!(
            "{VERSION_FILE} says version {}, and only BCF {BCF_VERSION} is read",
            version_id.as_deref().unwrap_or("nothing")
        ));
    }

    let mut project = BcfProject::default();
    if has_file(EXTENSIONS_FILE) {
        project.extensions = read_extensions(&reading.parse(Path::new(EXTENSIONS_FILE))?);
    }
    if has_file(PROJECT_FILE) {
        let info = reading.parse(Path::new(PROJECT_FILE))?;
        if let Some(described) = info.child("Project") {
            project.project_id = described.attribute("ProjectId");
            project.name = described.child_text("Name");
        }
    }
    if has_file(DOCUMENTS_FILE) {
        let documents = reading.parse(Path::new(DOCUMENTS_FILE))?;
        reading.dropped.documents = documents.grandchildren("Documents", "Document").len() as u64;
    }

    let mut topics: BTreeMap<String, BcfTopic> = BTreeMap::new();
    for relative_path in relative_paths {
        let mut parts = relative_path.components();
        let (Some(Component::Normal(_)), Some(Component::Normal(file_name)), None) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if file_name != MARKUP_FILE {
            continue;
        }
        let relative_folder = relative_path
            .parent()
            .expect("a path of two components has a parent");
        let root = reading.parse(relative_path)?;
        let topic = read_markup(&root, relative_folder, &mut reading)
            .map_err(|reason| format!("{}: {reason}", relative_path.display()))?;
        let guid = topic.guid.clone();
        if topics.insert(guid.clone(), topic).is_some() {
            return Err(format!("two topic folders hold topic {guid}"));
        }
    }

    Ok(BcfArchive {
        project,
        topics: topics.into_values().collect(),
        snapshots: reading.snapshots,
        dropped: reading.dropped,
    })
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ElementReferenceCounts {
    pub(crate) total: u64,
    pub(crate) distinct_ids: u64,
    pub(crate) unknown_ids: u64,
    pub(crate) without_ifc_guid: u64,
}

pub(crate) fn referenced_ifc_guids(
    topics: &[BcfTopic],
) -> (BTreeSet<String>, ElementReferenceCounts) {
    let mut ids = BTreeSet::new();
    let mut counts = ElementReferenceCounts::default();
    let mut add_id = |id: &str, counts: &mut ElementReferenceCounts| {
        counts.total += 1;
        ids.insert(id.to_string());
    };
    for topic in topics {
        for file in &topic.files {
            for id in [&file.ifc_project, &file.ifc_spatial_structure_element]
                .into_iter()
                .flatten()
            {
                add_id(id, &mut counts);
            }
        }
        for viewpoint in &topic.viewpoints {
            let exceptions = viewpoint
                .visibility
                .iter()
                .flat_map(|visibility| &visibility.exceptions);
            let colored = viewpoint
                .coloring
                .iter()
                .flat_map(|coloring| &coloring.components);
            for component in viewpoint.selection.iter().chain(exceptions).chain(colored) {
                match &component.ifc_guid {
                    Some(id) => add_id(id, &mut counts),
                    None => counts.without_ifc_guid += 1,
                }
            }
        }
    }
    counts.distinct_ids = ids.len() as u64;
    (ids, counts)
}

const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

fn push_text_element(xml: &mut String, name: &str, value: &str) {
    xml.push_str(&format!("<{name}>{}</{name}>", escape(value)));
}

fn push_optional_element(xml: &mut String, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        push_text_element(xml, name, value);
    }
}

fn push_attribute(xml: &mut String, name: &str, value: Option<&String>) {
    if let Some(value) = value {
        xml.push_str(&format!(" {name}=\"{}\"", escape(value.as_str())));
    }
}

fn push_list(xml: &mut String, list: &str, item: &str, values: &[String]) {
    xml.push_str(&format!("<{list}>"));
    for value in values {
        push_text_element(xml, item, value);
    }
    xml.push_str(&format!("</{list}>"));
}

fn push_point(xml: &mut String, name: &str, point: &[f64; 3]) {
    xml.push_str(&format!(
        "<{name}><X>{}</X><Y>{}</Y><Z>{}</Z></{name}>",
        point[0], point[1], point[2]
    ));
}

fn push_component(xml: &mut String, component: &BcfComponent) {
    xml.push_str("<Component");
    push_attribute(xml, "IfcGuid", component.ifc_guid.as_ref());
    xml.push('>');
    push_optional_element(
        xml,
        "OriginatingSystem",
        component.originating_system.as_ref(),
    );
    push_optional_element(xml, "AuthoringToolId", component.authoring_tool_id.as_ref());
    xml.push_str("</Component>");
}

fn viewpoint_file_name(viewpoint: &BcfViewpoint) -> String {
    format!("Viewpoint_{}.bcfv", viewpoint.guid)
}

fn snapshot_file_name(viewpoint: &BcfViewpoint, format: SnapshotFormat) -> String {
    format!("Snapshot_{}.{}", viewpoint.guid, format.extension())
}

fn markup_xml(topic: &BcfTopic) -> String {
    let mut xml = String::from(XML_DECLARATION);
    xml.push_str("<Markup>");
    if !topic.files.is_empty() {
        xml.push_str("<Header><Files>");
        for file in &topic.files {
            xml.push_str("<File");
            push_attribute(&mut xml, "IfcProject", file.ifc_project.as_ref());
            push_attribute(
                &mut xml,
                "IfcSpatialStructureElement",
                file.ifc_spatial_structure_element.as_ref(),
            );
            xml.push_str(&format!(" IsExternal=\"{}\">", file.is_external));
            push_optional_element(&mut xml, "Filename", file.filename.as_ref());
            push_optional_element(&mut xml, "Date", file.date.as_ref());
            push_optional_element(&mut xml, "Reference", file.reference.as_ref());
            xml.push_str("</File>");
        }
        xml.push_str("</Files></Header>");
    }

    xml.push_str("<Topic");
    push_attribute(&mut xml, "Guid", Some(&topic.guid));
    push_attribute(
        &mut xml,
        "ServerAssignedId",
        topic.server_assigned_id.as_ref(),
    );
    push_attribute(&mut xml, "TopicType", Some(&topic.topic_type));
    push_attribute(&mut xml, "TopicStatus", Some(&topic.topic_status));
    xml.push('>');
    if !topic.reference_links.is_empty() {
        push_list(
            &mut xml,
            "ReferenceLinks",
            "ReferenceLink",
            &topic.reference_links,
        );
    }
    push_text_element(&mut xml, "Title", &topic.title);
    push_optional_element(&mut xml, "Priority", topic.priority.as_ref());
    if let Some(index) = topic.index {
        push_text_element(&mut xml, "Index", &index.to_string());
    }
    if !topic.labels.is_empty() {
        push_list(&mut xml, "Labels", "Label", &topic.labels);
    }
    push_text_element(&mut xml, "CreationDate", &topic.creation_date);
    push_text_element(&mut xml, "CreationAuthor", &topic.creation_author);
    push_optional_element(&mut xml, "ModifiedDate", topic.modified_date.as_ref());
    push_optional_element(&mut xml, "ModifiedAuthor", topic.modified_author.as_ref());
    push_optional_element(&mut xml, "DueDate", topic.due_date.as_ref());
    push_optional_element(&mut xml, "AssignedTo", topic.assigned_to.as_ref());
    push_optional_element(&mut xml, "Stage", topic.stage.as_ref());
    push_optional_element(&mut xml, "Description", topic.description.as_ref());
    if let Some(snippet) = &topic.external_bim_snippet {
        xml.push_str("<BimSnippet");
        push_attribute(&mut xml, "SnippetType", Some(&snippet.snippet_type));
        xml.push_str(" IsExternal=\"true\">");
        push_text_element(&mut xml, "Reference", &snippet.reference);
        push_text_element(&mut xml, "ReferenceSchema", &snippet.reference_schema);
        xml.push_str("</BimSnippet>");
    }
    if !topic.external_documents.is_empty() {
        xml.push_str("<DocumentReferences>");
        for document in &topic.external_documents {
            xml.push_str("<DocumentReference");
            push_attribute(&mut xml, "Guid", Some(&document.guid));
            xml.push('>');
            push_text_element(&mut xml, "Url", &document.url);
            push_optional_element(&mut xml, "Description", document.description.as_ref());
            xml.push_str("</DocumentReference>");
        }
        xml.push_str("</DocumentReferences>");
    }
    if !topic.related_topics.is_empty() {
        xml.push_str("<RelatedTopics>");
        for related in &topic.related_topics {
            xml.push_str("<RelatedTopic");
            push_attribute(&mut xml, "Guid", Some(related));
            xml.push_str("/>");
        }
        xml.push_str("</RelatedTopics>");
    }
    if !topic.comments.is_empty() {
        xml.push_str("<Comments>");
        for comment in &topic.comments {
            xml.push_str("<Comment");
            push_attribute(&mut xml, "Guid", Some(&comment.guid));
            xml.push('>');
            push_text_element(&mut xml, "Date", &comment.date);
            push_text_element(&mut xml, "Author", &comment.author);
            push_optional_element(&mut xml, "Comment", comment.comment.as_ref());
            if comment.viewpoint_guid.is_some() {
                xml.push_str("<Viewpoint");
                push_attribute(&mut xml, "Guid", comment.viewpoint_guid.as_ref());
                xml.push_str("/>");
            }
            push_optional_element(&mut xml, "ModifiedDate", comment.modified_date.as_ref());
            push_optional_element(&mut xml, "ModifiedAuthor", comment.modified_author.as_ref());
            xml.push_str("</Comment>");
        }
        xml.push_str("</Comments>");
    }
    if !topic.viewpoints.is_empty() {
        xml.push_str("<Viewpoints>");
        for viewpoint in &topic.viewpoints {
            xml.push_str("<ViewPoint");
            push_attribute(&mut xml, "Guid", Some(&viewpoint.guid));
            xml.push('>');
            if viewpoint.camera.is_some() {
                push_text_element(&mut xml, "Viewpoint", &viewpoint_file_name(viewpoint));
            }
            if let Some(format) = viewpoint.snapshot_format {
                push_text_element(&mut xml, "Snapshot", &snapshot_file_name(viewpoint, format));
            }
            if let Some(index) = viewpoint.index {
                push_text_element(&mut xml, "Index", &index.to_string());
            }
            xml.push_str("</ViewPoint>");
        }
        xml.push_str("</Viewpoints>");
    }
    xml.push_str("</Topic></Markup>");
    xml
}

fn visualization_xml(viewpoint: &BcfViewpoint, camera: &BcfCamera) -> String {
    let mut xml = String::from(XML_DECLARATION);
    xml.push_str("<VisualizationInfo");
    push_attribute(&mut xml, "Guid", Some(&viewpoint.guid));
    xml.push_str("><Components>");
    xml.push_str("<Selection>");
    for component in &viewpoint.selection {
        push_component(&mut xml, component);
    }
    xml.push_str("</Selection>");
    if let Some(visibility) = &viewpoint.visibility {
        xml.push_str(&format!(
            "<Visibility DefaultVisibility=\"{}\"><ViewSetupHints SpacesVisible=\"{}\" \
             SpaceBoundariesVisible=\"{}\" OpeningsVisible=\"{}\"/><Exceptions>",
            visibility.default_visibility,
            visibility.spaces_visible,
            visibility.space_boundaries_visible,
            visibility.openings_visible
        ));
        for component in &visibility.exceptions {
            push_component(&mut xml, component);
        }
        xml.push_str("</Exceptions></Visibility>");
    }
    xml.push_str("<Coloring>");
    for coloring in &viewpoint.coloring {
        xml.push_str("<Color");
        push_attribute(&mut xml, "Color", Some(&coloring.color));
        xml.push_str("><Components>");
        for component in &coloring.components {
            push_component(&mut xml, component);
        }
        xml.push_str("</Components></Color>");
    }
    xml.push_str("</Coloring></Components>");

    match camera {
        BcfCamera::Orthogonal {
            camera_view_point,
            camera_direction,
            camera_up_vector,
            view_to_world_scale,
            aspect_ratio,
        } => {
            xml.push_str("<OrthogonalCamera>");
            push_point(&mut xml, "CameraViewPoint", camera_view_point);
            push_point(&mut xml, "CameraDirection", camera_direction);
            push_point(&mut xml, "CameraUpVector", camera_up_vector);
            xml.push_str(&format!(
                "<ViewToWorldScale>{view_to_world_scale}</ViewToWorldScale>\
                 <AspectRatio>{aspect_ratio}</AspectRatio></OrthogonalCamera>"
            ));
        }
        BcfCamera::Perspective {
            camera_view_point,
            camera_direction,
            camera_up_vector,
            field_of_view,
            aspect_ratio,
        } => {
            xml.push_str("<PerspectiveCamera>");
            push_point(&mut xml, "CameraViewPoint", camera_view_point);
            push_point(&mut xml, "CameraDirection", camera_direction);
            push_point(&mut xml, "CameraUpVector", camera_up_vector);
            xml.push_str(&format!(
                "<FieldOfView>{field_of_view}</FieldOfView>\
                 <AspectRatio>{aspect_ratio}</AspectRatio></PerspectiveCamera>"
            ));
        }
    }

    xml.push_str("<Lines>");
    for line in &viewpoint.lines {
        xml.push_str("<Line>");
        push_point(&mut xml, "StartPoint", &line.start_point);
        push_point(&mut xml, "EndPoint", &line.end_point);
        xml.push_str("</Line>");
    }
    xml.push_str("</Lines><ClippingPlanes>");
    for plane in &viewpoint.clipping_planes {
        xml.push_str("<ClippingPlane>");
        push_point(&mut xml, "Location", &plane.location);
        push_point(&mut xml, "Direction", &plane.direction);
        xml.push_str("</ClippingPlane>");
    }
    xml.push_str("</ClippingPlanes></VisualizationInfo>");
    xml
}

fn with_values(listed: &[String], used: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values = listed.to_vec();
    for value in used {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    values
}

fn extensions_xml(extensions: &BcfExtensions, topics: &[BcfTopic]) -> String {
    // extensions.xsd lists the values a topic may take
    let topic_types = with_values(
        &extensions.topic_types,
        topics.iter().map(|topic| topic.topic_type.clone()),
    );
    let topic_statuses = with_values(
        &extensions.topic_statuses,
        topics.iter().map(|topic| topic.topic_status.clone()),
    );
    let priorities = with_values(
        &extensions.priorities,
        topics.iter().filter_map(|topic| topic.priority.clone()),
    );
    let topic_labels = with_values(
        &extensions.topic_labels,
        topics.iter().flat_map(|topic| topic.labels.clone()),
    );
    let users = with_values(
        &extensions.users,
        topics.iter().filter_map(|topic| topic.assigned_to.clone()),
    );
    let snippet_types = with_values(
        &extensions.snippet_types,
        topics
            .iter()
            .filter_map(|topic| topic.external_bim_snippet.as_ref())
            .map(|snippet| snippet.snippet_type.clone()),
    );
    let stages = with_values(
        &extensions.stages,
        topics.iter().filter_map(|topic| topic.stage.clone()),
    );

    let mut xml = String::from(XML_DECLARATION);
    xml.push_str("<Extensions>");
    push_list(&mut xml, "TopicTypes", "TopicType", &topic_types);
    push_list(&mut xml, "TopicStatuses", "TopicStatus", &topic_statuses);
    push_list(&mut xml, "Priorities", "Priority", &priorities);
    push_list(&mut xml, "TopicLabels", "TopicLabel", &topic_labels);
    push_list(&mut xml, "Users", "User", &users);
    push_list(&mut xml, "SnippetTypes", "SnippetType", &snippet_types);
    push_list(&mut xml, "Stages", "Stage", &stages);
    xml.push_str("</Extensions>");
    xml
}

fn project_xml(project: &BcfProject) -> String {
    let mut xml = String::from(XML_DECLARATION);
    xml.push_str("<ProjectInfo><Project");
    push_attribute(&mut xml, "ProjectId", project.project_id.as_ref());
    xml.push('>');
    push_optional_element(&mut xml, "Name", project.name.as_ref());
    xml.push_str("</Project></ProjectInfo>");
    xml
}

pub(crate) fn write_archive(
    output: &Path,
    project: &BcfProject,
    topics: &[BcfTopic],
    snapshot_path: impl Fn(&BcfTopic, &BcfViewpoint, SnapshotFormat) -> PathBuf,
) -> Result<(), String> {
    let describe = |error: zip::result::ZipError| format!("writing the BCF zip failed: {error}");
    let written = |error: std::io::Error| format!("writing the BCF zip failed: {error}");
    let file = File::create(output).map_err(written)?;
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let add_text = |archive: &mut zip::ZipWriter<File>, name: String, text: String| {
        archive.start_file(name, options).map_err(describe)?;
        archive.write_all(text.as_bytes()).map_err(written)
    };

    add_text(
        &mut archive,
        VERSION_FILE.to_string(),
        format!("{XML_DECLARATION}<Version VersionId=\"{BCF_VERSION}\"/>"),
    )?;
    add_text(
        &mut archive,
        EXTENSIONS_FILE.to_string(),
        extensions_xml(&project.extensions, topics),
    )?;
    if project.project_id.is_some() {
        add_text(&mut archive, PROJECT_FILE.to_string(), project_xml(project))?;
    }
    for topic in topics {
        add_text(
            &mut archive,
            format!("{}/{MARKUP_FILE}", topic.guid),
            markup_xml(topic),
        )?;
        for viewpoint in &topic.viewpoints {
            if let Some(camera) = &viewpoint.camera {
                add_text(
                    &mut archive,
                    format!("{}/{}", topic.guid, viewpoint_file_name(viewpoint)),
                    visualization_xml(viewpoint, camera),
                )?;
            }
            if let Some(format) = viewpoint.snapshot_format {
                let mut snapshot =
                    File::open(snapshot_path(topic, viewpoint, format)).map_err(written)?;
                archive
                    .start_file(
                        format!("{}/{}", topic.guid, snapshot_file_name(viewpoint, format)),
                        options,
                    )
                    .map_err(describe)?;
                std::io::copy(&mut snapshot, &mut archive).map_err(written)?;
            }
        }
    }
    archive.finish().map_err(describe)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_definitions_are_refused_before_any_expansion() {
        let bomb = br#"<?xml version="1.0"?>
            <!DOCTYPE Markup [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;">]>
            <Markup><Topic Guid="x">&b;</Topic></Markup>"#;
        let reason = parse_xml(bomb).err().expect("a declared entity is refused");
        assert!(reason.contains("document type"), "{reason}");
    }

    #[test]
    fn predefined_entities_and_character_references_are_resolved() {
        let node = parse_xml(b"<a>x &amp; y &#65;</a>").unwrap();
        assert_eq!(node.text, "x & y A");
        assert!(parse_xml(b"<a>&unknown;</a>").is_err());
    }

    #[test]
    fn a_viewpoint_file_outside_its_topic_folder_is_refused() {
        for name in [
            "../x.bcfv",
            "a/b.bcfv",
            "..",
            "/etc/passwd",
            "c:\\x",
            "a\\b",
        ] {
            assert!(
                file_in_topic_folder(Path::new("/f"), name).is_err(),
                "{name}"
            );
        }
        assert_eq!(
            file_in_topic_folder(Path::new("/f"), "v.bcfv").unwrap(),
            Path::new("/f/v.bcfv")
        );
    }
}
