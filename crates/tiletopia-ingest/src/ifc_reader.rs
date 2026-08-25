//! IFC geometry reader.

use crate::{IngestError, MeshData};
use ifc_lite_core::{
    AttributeValue, EntityDecoder, EntityScanner, build_entity_index, has_geometry_by_name,
};
use ifc_lite_geometry::GeometryRouter;
use std::path::Path;

/// 0-based attribute positions every IfcRoot subtype shares.
const GLOBAL_ID_INDEX: usize = 0;
const NAME_INDEX: usize = 2;

/// 0-based attribute positions on IFCSITE.
const REF_LATITUDE_INDEX: usize = 9;
const REF_LONGITUDE_INDEX: usize = 10;
const REF_ELEVATION_INDEX: usize = 11;

const MINUTES_PER_DEGREE: f64 = 60.0;
const SECONDS_PER_DEGREE: f64 = 3600.0;
const MILLIONTHS_PER_SECOND: f64 = 1_000_000.0;

/// Where an IfcSite says the model sits, in degrees and metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SitePlacement {
    pub longitude: f64,
    pub latitude: f64,
    pub elevation: f64,
}

/// Read meshes from an IFC file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let content = std::fs::read_to_string(path)?;
    let index = build_entity_index(&content);
    let mut decoder = EntityDecoder::with_index(&content, index);
    let router = GeometryRouter::with_units(&content, &mut decoder);

    let mut meshes = Vec::new();
    let mut scanner = EntityScanner::new(&content);

    // has_geometry_by_name follows the EXPRESS inheritance graph, so an
    // IfcProduct subtype no hardcoded list names still reaches the router
    while let Some((id, type_name, _start, _end)) = scanner.next_entity() {
        if !has_geometry_by_name(type_name) {
            continue;
        }

        let entity = match decoder.decode_by_id(id) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let ifc_mesh = match router.process_element(&entity, &mut decoder) {
            Ok(m) if !m.is_empty() => m,
            _ => continue,
        };

        let num_verts = ifc_mesh.positions.len() / 3;
        let positions: Vec<[f32; 3]> = (0..num_verts)
            .map(|i| {
                [
                    ifc_mesh.positions[i * 3],
                    ifc_mesh.positions[i * 3 + 1],
                    ifc_mesh.positions[i * 3 + 2],
                ]
            })
            .collect();

        let num_normals = ifc_mesh.normals.len() / 3;
        let normals: Vec<[f32; 3]> = (0..num_normals)
            .map(|i| {
                [
                    ifc_mesh.normals[i * 3],
                    ifc_mesh.normals[i * 3 + 1],
                    ifc_mesh.normals[i * 3 + 2],
                ]
            })
            .collect();

        let name = entity.get_string(NAME_INDEX).unwrap_or("ifc_element").to_string();

        meshes.push(MeshData {
            positions,
            normals,
            texcoords: Vec::new(),
            indices: ifc_mesh.indices,
            name,
            material: None,
            asset_id: entity.get_string(GLOBAL_ID_INDEX).map(str::to_string),
        });
    }

    tracing::info!(
        "Read {} meshes from {} ({} total vertices)",
        meshes.len(),
        path.display(),
        meshes.iter().map(|m| m.positions.len()).sum::<usize>(),
    );

    Ok(meshes)
}

/// Read the first IfcSite's reference latitude, longitude and elevation.
/// None when the file has no site, or the site leaves the coordinates unset.
pub fn site_placement(path: &Path) -> Result<Option<SitePlacement>, IngestError> {
    let content = std::fs::read_to_string(path)?;
    let index = build_entity_index(&content);
    let mut decoder = EntityDecoder::with_index(&content, index);
    let mut scanner = EntityScanner::new(&content);

    while let Some((id, type_name, _start, _end)) = scanner.next_entity() {
        if !type_name.eq_ignore_ascii_case("IFCSITE") {
            continue;
        }
        let Ok(site) = decoder.decode_by_id(id) else {
            continue;
        };
        let latitude = site.get_list(REF_LATITUDE_INDEX).and_then(plane_angle);
        let longitude = site.get_list(REF_LONGITUDE_INDEX).and_then(plane_angle);
        let (Some(latitude), Some(longitude)) = (latitude, longitude) else {
            continue;
        };
        return Ok(Some(SitePlacement {
            longitude,
            latitude,
            elevation: site.get_float(REF_ELEVATION_INDEX).unwrap_or(0.0),
        }));
    }

    Ok(None)
}

/// Degrees from an IfcCompoundPlaneAngleMeasure: degrees, minutes, seconds and
/// optionally millionths of a second. IFC carries one sign across the whole
/// measure, so the first nonzero component sets it.
fn plane_angle(components: &[AttributeValue]) -> Option<f64> {
    let values: Vec<i64> = components
        .iter()
        .map(AttributeValue::as_int)
        .collect::<Option<_>>()?;
    if values.is_empty() {
        return None;
    }

    let negative = values
        .iter()
        .find(|value| **value != 0)
        .is_some_and(|value| *value < 0);
    let part = |position: usize| values.get(position).copied().unwrap_or(0).abs() as f64;
    let magnitude = part(0)
        + part(1) / MINUTES_PER_DEGREE
        + part(2) / SECONDS_PER_DEGREE
        + part(3) / (SECONDS_PER_DEGREE * MILLIONTHS_PER_SECOND);

    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_nonexistent_file() {
        let result = read(Path::new("/tmp/nonexistent_ifc_file.ifc"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_minimal_ifc() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("minimal.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('minimal.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        let result = read(&path);
        assert!(result.is_ok());
        // Minimal IFC with no geometry should produce empty mesh list
        let meshes = result.unwrap();
        assert!(meshes.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_mesh_triangulates_a_real_ifc4x3_file() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bath_csg_solid.ifc");

        let meshes = crate::read_mesh(&path).expect("the fixture should read");
        let triangles: usize = meshes.iter().map(|m| m.indices.len() / 3).sum();
        assert!(!meshes.is_empty(), "no meshes from the fixture");
        assert!(triangles > 0, "no triangles from the fixture");
    }

    #[test]
    fn site_placement_reads_reference_coordinates() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_site_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("site.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('site.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,(51,30,0),(-0,-7,-40),12.5,$,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        let placement = site_placement(&path).unwrap().expect("a site placement");
        assert!((placement.latitude - 51.5).abs() < 1e-6, "{placement:?}");
        let expected_longitude = -(7.0 / 60.0 + 40.0 / 3600.0);
        assert!(
            (placement.longitude - expected_longitude).abs() < 1e-6,
            "{placement:?}"
        );
        assert!((placement.elevation - 12.5).abs() < 1e-6, "{placement:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn site_placement_is_none_when_the_site_leaves_them_unset() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_site_unset_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("minimal.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('minimal.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0002',$,'TestSite',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        assert_eq!(site_placement(&path).unwrap(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_ifc_with_extrusion() {
        let dir = std::env::temp_dir().join("tiletopia_ifc_extrusion_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("extrusion.ifc");

        let ifc_content = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('extrusion.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#40=IFCWALL('0002',$,'TestWall',$,$,#12,#31,$);
ENDSEC;
END-ISO-10303-21;
";
        std::fs::write(&path, ifc_content).unwrap();

        let meshes = read(&path).expect("the extrusion should read");
        let first = meshes.first().expect("a mesh from the extruded wall");
        assert!(!first.positions.is_empty());
        assert!(!first.indices.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
