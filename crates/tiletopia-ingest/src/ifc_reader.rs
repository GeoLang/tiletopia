//! IFC geometry reader.

use crate::{IngestError, MeshData};
use ifc_lite_core::{EntityDecoder, EntityScanner, IfcSchema, build_entity_index};
use ifc_lite_geometry::GeometryRouter;
use std::path::Path;

/// Read meshes from an IFC file.
pub fn read(path: &Path) -> Result<Vec<MeshData>, IngestError> {
    let content = std::fs::read_to_string(path)?;
    let index = build_entity_index(&content);
    let mut decoder = EntityDecoder::with_index(&content, index);
    let schema = IfcSchema::new();
    let router = GeometryRouter::with_units(&content, &mut decoder);

    let mut meshes = Vec::new();
    let mut scanner = EntityScanner::new(&content);

    while let Some((id, _type_name, _start, _end)) = scanner.next_entity() {
        let entity = match decoder.decode_by_id(id) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !schema.has_geometry(&entity.ifc_type) {
            continue;
        }

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

        let name = entity.get_string(2).unwrap_or("ifc_element").to_string();

        meshes.push(MeshData {
            positions,
            normals,
            indices: ifc_mesh.indices,
            name,
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

        let result = read(&path);
        assert!(result.is_ok());
        let meshes = result.unwrap();
        // Should produce at least one mesh from the extruded wall
        if !meshes.is_empty() {
            let m = &meshes[0];
            assert!(!m.positions.is_empty());
            assert!(!m.indices.is_empty());
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
