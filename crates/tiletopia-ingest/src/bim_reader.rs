//! BIM/IFC metadata reader.
//!
//! Extracts structural metadata from IFC files and preserves it
//! alongside the geometry for rich 3D Tiles batch tables.

use std::collections::HashMap;
use std::path::Path;

/// BIM element types.
#[derive(Debug, Clone, PartialEq)]
pub enum BimElementType {
    Wall,
    Slab,
    Column,
    Beam,
    Door,
    Window,
    Roof,
    Stair,
    Railing,
    Space,
    Site,
    Building,
    Storey,
    Furniture,
    Other(String),
}

/// A BIM element with geometry and metadata.
#[derive(Debug, Clone)]
pub struct BimElement {
    pub id: String,
    pub global_id: String,
    pub name: String,
    pub element_type: BimElementType,
    pub storey: Option<String>,
    pub material: Option<String>,
    /// Vertex positions (flattened [x, y, z, ...])
    pub vertices: Vec<f64>,
    /// Triangle indices (flattened [i0, i1, i2, ...])
    pub indices: Vec<u32>,
    /// Arbitrary key-value properties from IFC property sets.
    pub properties: HashMap<String, PropertyValue>,
}

/// IFC property value types.
#[derive(Debug, Clone)]
pub enum PropertyValue {
    String(String),
    Real(f64),
    Integer(i64),
    Boolean(bool),
    Label(String),
}

/// Result of reading a BIM file.
#[derive(Debug, Clone)]
pub struct BimModel {
    pub filename: String,
    pub schema: String, // IFC2X3, IFC4, IFC4X3
    pub elements: Vec<BimElement>,
    pub project_name: Option<String>,
    pub site_name: Option<String>,
    pub building_name: Option<String>,
    pub storeys: Vec<String>,
}

/// Errors from BIM reading.
#[derive(Debug, thiserror::Error)]
pub enum BimError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("IFC parse error: {0}")]
    ParseError(String),
    #[error("unsupported IFC schema: {0}")]
    UnsupportedSchema(String),
}

/// Read a BIM/IFC file and extract elements with metadata.
///
/// This is a simplified IFC STEP parser that extracts key entities.
/// For production use, integrate with a full IFC SDK.
pub fn read_ifc(path: &Path) -> Result<BimModel, BimError> {
    let content = std::fs::read_to_string(path)?;

    let schema = extract_schema(&content).unwrap_or_else(|| "IFC4".into());
    let project_name = extract_entity_name(&content, "IFCPROJECT");
    let site_name = extract_entity_name(&content, "IFCSITE");
    let building_name = extract_entity_name(&content, "IFCBUILDING");
    let storeys = extract_storeys(&content);
    let elements = extract_elements(&content);

    Ok(BimModel {
        filename: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        schema,
        elements,
        project_name,
        site_name,
        building_name,
        storeys,
    })
}

fn extract_schema(content: &str) -> Option<String> {
    for line in content.lines() {
        if !line.starts_with("FILE_SCHEMA") {
            continue;
        }
        let start = line.find('\'')?;
        let end = line[start + 1..].find('\'')?;
        return Some(line[start + 1..start + 1 + end].to_string());
    }
    None
}

fn extract_entity_name(content: &str, entity_type: &str) -> Option<String> {
    for line in content.lines() {
        if !line.contains(entity_type) {
            continue;
        }
        let start = line.find('\'')?;
        let end = line[start + 1..].find('\'')?;
        return Some(line[start + 1..start + 1 + end].to_string());
    }
    None
}

fn extract_storeys(content: &str) -> Vec<String> {
    let mut storeys = Vec::new();
    for line in content.lines() {
        if !line.contains("IFCBUILDINGSTOREY") {
            continue;
        }
        if let Some(start) = line.find('\'') {
            let end = line[start + 1..].find('\'').unwrap_or(0);
            if end > 0 {
                storeys.push(line[start + 1..start + 1 + end].to_string());
            }
        }
    }
    storeys
}

fn extract_elements(content: &str) -> Vec<BimElement> {
    let mut elements = Vec::new();
    let type_map = [
        ("IFCWALL", BimElementType::Wall),
        ("IFCSLAB", BimElementType::Slab),
        ("IFCCOLUMN", BimElementType::Column),
        ("IFCBEAM", BimElementType::Beam),
        ("IFCDOOR", BimElementType::Door),
        ("IFCWINDOW", BimElementType::Window),
        ("IFCROOF", BimElementType::Roof),
        ("IFCSTAIR", BimElementType::Stair),
        ("IFCRAILING", BimElementType::Railing),
        ("IFCFURNISHINGELEMENT", BimElementType::Furniture),
    ];

    for line in content.lines() {
        for (ifc_type, bim_type) in &type_map {
            if line.contains(ifc_type) && line.contains('=') {
                let id = line
                    .split('=')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches('#')
                    .to_string();
                let name = extract_first_quoted(line).unwrap_or_default();
                let global_id = extract_global_id(line).unwrap_or_default();
                elements.push(BimElement {
                    id: id.clone(),
                    global_id,
                    name,
                    element_type: bim_type.clone(),
                    storey: None,
                    material: None,
                    vertices: Vec::new(),
                    indices: Vec::new(),
                    properties: HashMap::new(),
                });
            }
        }
    }
    elements
}

fn extract_first_quoted(line: &str) -> Option<String> {
    let start = line.find('\'')?;
    let end = line[start + 1..].find('\'')?;
    Some(line[start + 1..start + 1 + end].to_string())
}

fn extract_global_id(line: &str) -> Option<String> {
    // GlobalId is typically the first argument after the entity type
    let paren = line.find('(')?;
    let args = &line[paren + 1..];
    let first_arg = args.split(',').next()?.trim().trim_matches('\'');
    Some(first_arg.to_string())
}

/// Convert BIM elements to batch table for 3D Tiles.
pub fn elements_to_batch_table(elements: &[BimElement]) -> serde_json::Value {
    let ids: Vec<&str> = elements.iter().map(|e| e.global_id.as_str()).collect();
    let names: Vec<&str> = elements.iter().map(|e| e.name.as_str()).collect();
    let types: Vec<String> = elements
        .iter()
        .map(|e| format!("{:?}", e.element_type))
        .collect();
    let storeys: Vec<&str> = elements
        .iter()
        .map(|e| e.storey.as_deref().unwrap_or(""))
        .collect();

    serde_json::json!({
        "globalId": ids,
        "name": names,
        "type": types,
        "storey": storeys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_schema() {
        let content = "FILE_SCHEMA(('IFC4'));";
        assert_eq!(extract_schema(content), Some("IFC4".into()));
    }

    #[test]
    fn test_extract_elements() {
        let content = "#123= IFCWALL('2O2Fr$t4X7Zf8NOew3FNr2','Wall-001');
#456= IFCSLAB('3P3Gs$u5Y8Ag9OPfx4GOt3','Slab-001');";
        let elements = extract_elements(content);
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0].element_type, BimElementType::Wall);
        assert_eq!(elements[1].element_type, BimElementType::Slab);
    }

    #[test]
    fn test_batch_table() {
        let elements = vec![BimElement {
            id: "1".into(),
            global_id: "abc123".into(),
            name: "Wall-001".into(),
            element_type: BimElementType::Wall,
            storey: Some("Level 1".into()),
            material: None,
            vertices: vec![],
            indices: vec![],
            properties: HashMap::new(),
        }];
        let table = elements_to_batch_table(&elements);
        assert_eq!(table["globalId"][0], "abc123");
        assert_eq!(table["name"][0], "Wall-001");
    }
}
