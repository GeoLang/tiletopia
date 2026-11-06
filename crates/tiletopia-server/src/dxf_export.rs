use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::get,
};
use serde::Deserialize;
use tiletopia_ingest::MeshData;

use crate::db::JobRecord;
use crate::elements::{element_error, invalid_parameter, permitted_asset, server_failure};
use crate::elevation::JOB_BUDGET_BYTES;
use crate::ion_compat::IonError;
use crate::job_queue::{Tiler, job_with_input_in_asset_dir, move_into_tileset_frame, tiler_for};
use crate::upload::lowercase_extension;
use crate::{AppState, Asset, AssetStatus, may_view_asset, scratch_files};

const DXF_CONTENT_TYPE: &str = "application/dxf";
// autocad INSUNITS code for metres, the unit every mesh reader answers in
const INSUNITS_METRES: i32 = 6;

const MODEL_SPACE_RECORD: &str = "11";

pub fn dxf_export_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/assets/{id}/dxf", get(export_dxf))
}

#[derive(Debug, Deserialize)]
struct DxfParameters {
    #[serde(rename = "cutHeight")]
    cut_height: Option<String>,
}

fn parsed_cut_height(raw: Option<&str>) -> Result<Option<f64>, IonError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    raw.parse::<f64>()
        .ok()
        .filter(|height| height.is_finite())
        .map(Some)
        .ok_or_else(|| invalid_parameter(format!("cutHeight {raw:?} is not a finite number")))
}

pub(crate) struct MeshSource {
    pub(crate) job: JobRecord,
    pub(crate) extension: String,
    pub(crate) source_is_z_up: bool,
}

pub(crate) async fn mesh_source(
    state: &AppState,
    asset: &Asset,
    id: &str,
) -> Result<MeshSource, IonError> {
    if !matches!(asset.status, AssetStatus::Ready) {
        return Err(element_error(
            StatusCode::CONFLICT,
            "ASSET_NOT_READY",
            format!("asset {id} has status {:?}", asset.status),
        ));
    }

    let job = state
        .db
        .list_jobs_for_asset(asset.id)
        .await
        .map_err(|error| server_failure("the asset's tiling job could not be read", error))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            element_error(
                StatusCode::NOT_FOUND,
                "ASSET_HAS_NO_MESH_SOURCE",
                format!("asset {id} was never tiled, so it has no source file to read"),
            )
        })?;
    let job = job_with_input_in_asset_dir(&job, &state.data_dir.join(asset.id.to_string()));
    let extension = lowercase_extension(Path::new(&job.input_path));
    let Ok(Tiler::Native { source_is_z_up }) = tiler_for(&extension) else {
        return Err(element_error(
            StatusCode::NOT_FOUND,
            "ASSET_HAS_NO_MESH_SOURCE",
            format!("asset {id}'s source is a .{extension}, which no mesh reader takes"),
        ));
    };
    if !Path::new(&job.input_path).is_file() {
        return Err(element_error(
            StatusCode::NOT_FOUND,
            "ASSET_HAS_NO_MESH_SOURCE",
            format!("asset {id}'s source file is gone"),
        ));
    }
    Ok(MeshSource {
        job,
        extension,
        source_is_z_up,
    })
}

pub(crate) fn read_meshes_in_tileset_frame(
    job: &JobRecord,
    extension: &str,
) -> Result<Vec<MeshData>, String> {
    let path = Path::new(&job.input_path);
    if extension == "dxf" {
        let upload_unit = job
            .placement
            .units
            .as_deref()
            .map(tiletopia_ingest::dxf_reader::DrawingUnit::named)
            .transpose()
            .map_err(|error| error.to_string())?;
        return tiletopia_ingest::dxf_reader::read(path, JOB_BUDGET_BYTES, upload_unit)
            .map_err(|error| error.to_string());
    }
    // ifc-lite-geometry recurses on nested solids and placements, and a stack overflow aborts the server
    ifc_lite_processing::run_on_geometry_pool(|| {
        let mut meshes = tiletopia_ingest::read_mesh(path, JOB_BUDGET_BYTES)
            .map_err(|error| error.to_string())?;
        if extension == "ifc" {
            let source = tiletopia_ingest::ifc_reader::mapped_ifc(path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))?;
            move_into_tileset_frame(&job.placement, Some(&*source), &mut meshes)?;
        }
        Ok(meshes)
    })
    .map_err(|error| format!("could not start the geometry threads: {error}"))?
}

fn z_up(position: [f64; 3], source_is_z_up: bool) -> [f64; 3] {
    if source_is_z_up {
        return position;
    }
    [position[0], -position[2], position[1]]
}

fn at_or_below(corners: [[f64; 3]; 3], height: f64) -> Vec<[f64; 3]> {
    let mut kept = Vec::with_capacity(4);
    for index in 0..3 {
        let current = corners[index];
        let next = corners[(index + 1) % 3];
        if current[2] <= height {
            kept.push(current);
        }
        let crosses = (current[2] <= height) != (next[2] <= height);
        if crosses {
            let along = (height - current[2]) / (next[2] - current[2]);
            kept.push([
                current[0] + along * (next[0] - current[0]),
                current[1] + along * (next[1] - current[1]),
                height,
            ]);
        }
    }
    kept
}

fn write_face(output: &mut impl Write, handle: u64, corners: &[[f64; 3]]) -> std::io::Result<()> {
    write!(
        output,
        "0\n3DFACE\n5\n{handle:X}\n330\n{MODEL_SPACE_RECORD}\n100\nAcDbEntity\n8\n0\n100\nAcDbFace\n"
    )?;
    // a three-cornered 3DFACE repeats its third corner as the fourth
    let fourth_corner = corners[corners.len() - 1];
    for corner_index in 0..4 {
        let corner = corners.get(corner_index).unwrap_or(&fourth_corner);
        write!(
            output,
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            10 + corner_index,
            corner[0],
            20 + corner_index,
            corner[1],
            30 + corner_index,
            corner[2]
        )?;
    }
    Ok(())
}

const PAPER_SPACE_RECORD: &str = "12";
const PLOT_STYLE_PLACEHOLDER: &str = "1A";

type Groups = &'static [(i32, &'static str)];

#[rustfmt::skip]
const CLASSES_TABLES_AND_BLOCKS: Groups = &[
    (0, "SECTION"), (2, "CLASSES"),
    // dxf R2000 declares these two OBJECTS types in CLASSES before they are used
    (0, "CLASS"), (1, "ACDBDICTIONARYWDFLT"), (2, "AcDbDictionaryWithDefault"),
    (3, "ObjectDBX Classes"), (90, "0"), (280, "0"), (281, "0"),
    (0, "CLASS"), (1, "ACDBPLACEHOLDER"), (2, "AcDbPlaceHolder"),
    (3, "ObjectDBX Classes"), (90, "0"), (280, "0"), (281, "0"),
    (0, "ENDSEC"),

    (0, "SECTION"), (2, "TABLES"),
    (0, "TABLE"), (2, "VPORT"), (5, "1"), (330, "0"), (100, "AcDbSymbolTable"), (70, "0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "LTYPE"), (5, "2"), (330, "0"), (100, "AcDbSymbolTable"), (70, "3"),
    (0, "LTYPE"), (5, "A"), (330, "2"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbLinetypeTableRecord"),
    (2, "ByBlock"), (70, "0"), (3, ""), (72, "65"), (73, "0"), (40, "0.0"),
    (0, "LTYPE"), (5, "B"), (330, "2"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbLinetypeTableRecord"),
    (2, "ByLayer"), (70, "0"), (3, ""), (72, "65"), (73, "0"), (40, "0.0"),
    (0, "LTYPE"), (5, "C"), (330, "2"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbLinetypeTableRecord"),
    (2, "Continuous"), (70, "0"), (3, "Solid line"), (72, "65"), (73, "0"), (40, "0.0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "LAYER"), (5, "3"), (330, "0"), (100, "AcDbSymbolTable"), (70, "1"),
    (0, "LAYER"), (5, "D"), (330, "3"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbLayerTableRecord"),
    (2, "0"), (70, "0"), (62, "7"), (6, "Continuous"), (370, "-3"),
    // autocad needs a layer's group 390 to point at a plot style object
    (390, PLOT_STYLE_PLACEHOLDER),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "STYLE"), (5, "4"), (330, "0"), (100, "AcDbSymbolTable"), (70, "1"),
    (0, "STYLE"), (5, "E"), (330, "4"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbTextStyleTableRecord"),
    (2, "Standard"), (70, "0"), (40, "0.0"), (41, "1.0"), (50, "0.0"), (71, "0"),
    (42, "2.5"), (3, "txt"), (4, ""),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "VIEW"), (5, "5"), (330, "0"), (100, "AcDbSymbolTable"), (70, "0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "UCS"), (5, "6"), (330, "0"), (100, "AcDbSymbolTable"), (70, "0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "APPID"), (5, "7"), (330, "0"), (100, "AcDbSymbolTable"), (70, "1"),
    (0, "APPID"), (5, "F"), (330, "7"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbRegAppTableRecord"),
    (2, "ACAD"), (70, "0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "DIMSTYLE"), (5, "8"), (330, "0"), (100, "AcDbSymbolTable"), (70, "1"),
    (100, "AcDbDimStyleTable"),
    // DIMSTYLE records carry their handle in group 105 instead of 5
    (0, "DIMSTYLE"), (105, "10"), (330, "8"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbDimStyleTableRecord"),
    (2, "Standard"), (70, "0"),
    (0, "ENDTAB"),

    (0, "TABLE"), (2, "BLOCK_RECORD"), (5, "9"), (330, "0"), (100, "AcDbSymbolTable"), (70, "2"),
    (0, "BLOCK_RECORD"), (5, MODEL_SPACE_RECORD), (330, "9"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbBlockTableRecord"), (2, "*Model_Space"),
    (0, "BLOCK_RECORD"), (5, PAPER_SPACE_RECORD), (330, "9"),
    (100, "AcDbSymbolTableRecord"), (100, "AcDbBlockTableRecord"), (2, "*Paper_Space"),
    (0, "ENDTAB"),
    (0, "ENDSEC"),

    (0, "SECTION"), (2, "BLOCKS"),
    (0, "BLOCK"), (5, "13"), (330, MODEL_SPACE_RECORD), (100, "AcDbEntity"), (8, "0"),
    (100, "AcDbBlockBegin"), (2, "*Model_Space"), (70, "0"),
    (10, "0.0"), (20, "0.0"), (30, "0.0"), (3, "*Model_Space"), (1, ""),
    (0, "ENDBLK"), (5, "14"), (330, MODEL_SPACE_RECORD), (100, "AcDbEntity"), (8, "0"),
    (100, "AcDbBlockEnd"),
    (0, "BLOCK"), (5, "15"), (330, PAPER_SPACE_RECORD), (100, "AcDbEntity"), (67, "1"), (8, "0"),
    (100, "AcDbBlockBegin"), (2, "*Paper_Space"), (70, "0"),
    (10, "0.0"), (20, "0.0"), (30, "0.0"), (3, "*Paper_Space"), (1, ""),
    (0, "ENDBLK"), (5, "16"), (330, PAPER_SPACE_RECORD), (100, "AcDbEntity"), (67, "1"), (8, "0"),
    (100, "AcDbBlockEnd"),
    (0, "ENDSEC"),

    (0, "SECTION"), (2, "ENTITIES"),
];

#[rustfmt::skip]
const OBJECTS_AND_END: Groups = &[
    (0, "ENDSEC"),
    (0, "SECTION"), (2, "OBJECTS"),
    // the first object in OBJECTS is the root dictionary
    (0, "DICTIONARY"), (5, "17"), (330, "0"), (100, "AcDbDictionary"), (281, "1"),
    (3, "ACAD_GROUP"), (350, "18"),
    (3, "ACAD_PLOTSTYLENAME"), (350, "19"),
    (0, "DICTIONARY"), (5, "18"), (330, "17"), (100, "AcDbDictionary"), (281, "1"),
    (0, "ACDBDICTIONARYWDFLT"), (5, "19"), (330, "17"), (100, "AcDbDictionary"), (281, "1"),
    (3, "Normal"), (350, PLOT_STYLE_PLACEHOLDER),
    (100, "AcDbDictionaryWithDefault"), (340, PLOT_STYLE_PLACEHOLDER),
    (0, "ACDBPLACEHOLDER"), (5, PLOT_STYLE_PLACEHOLDER), (330, "19"),
    (0, "ENDSEC"),
    (0, "EOF"),
];

fn write_groups(output: &mut impl Write, groups: Groups) -> std::io::Result<()> {
    for (code, value) in groups {
        write!(output, "{code}\n{value}\n")?;
    }
    Ok(())
}

struct DxfFaces {
    written: u64,
    skipped_not_finite: u64,
}

const FIRST_FACE_HANDLE: u64 = 0x1B;

fn write_dxf(
    path: &Path,
    meshes: &[MeshData],
    source_is_z_up: bool,
    cut_height: Option<f64>,
) -> std::io::Result<DxfFaces> {
    let triangles: u64 = meshes
        .iter()
        .map(|mesh| mesh.indices.len() as u64 / 3)
        .sum();
    // readers assign new handles from $HANDSEED up, and it has to exceed every handle written
    let handle_seed = FIRST_FACE_HANDLE + triangles;
    let mut output = BufWriter::new(std::fs::File::create(path)?);
    write!(
        output,
        "0\nSECTION\n2\nHEADER\n9\n$ACADVER\n1\nAC1015\n9\n$HANDSEED\n5\n{handle_seed:X}\n9\n$INSUNITS\n70\n{INSUNITS_METRES}\n0\nENDSEC\n"
    )?;
    write_groups(&mut output, CLASSES_TABLES_AND_BLOCKS)?;
    let mut faces = DxfFaces {
        written: 0,
        skipped_not_finite: 0,
    };
    for mesh in meshes {
        for triangle in mesh.indices.as_chunks::<3>().0 {
            let corners = [
                z_up(mesh.positions[triangle[0] as usize], source_is_z_up),
                z_up(mesh.positions[triangle[1] as usize], source_is_z_up),
                z_up(mesh.positions[triangle[2] as usize], source_is_z_up),
            ];
            let all_finite = corners
                .iter()
                .all(|corner| corner.iter().all(|coordinate| coordinate.is_finite()));
            if !all_finite {
                faces.skipped_not_finite += 1;
                continue;
            }
            let face = match cut_height {
                Some(height) => at_or_below(corners, height),
                None => corners.to_vec(),
            };
            if face.len() < 3 {
                continue;
            }
            write_face(&mut output, FIRST_FACE_HANDLE + faces.written, &face)?;
            faces.written += 1;
        }
    }
    write_groups(&mut output, OBJECTS_AND_END)?;
    output.flush()?;
    Ok(faces)
}

async fn export_dxf(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Query(parameters): Query<DxfParameters>,
) -> Result<Response, IonError> {
    let (asset, ion_id) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let cut_height = parsed_cut_height(parameters.cut_height.as_deref())?;
    let MeshSource {
        job,
        extension,
        source_is_z_up,
    } = mesh_source(&state, &asset, &id).await?;

    let scratch = scratch_files::new_scratch_dir(&state.data_dir)
        .await
        .map_err(|error| server_failure("the export file could not be created", error))?;
    let file_name = format!("model-{ion_id}.dxf");
    let output_path: PathBuf = scratch.join(&file_name);

    // the mesh readers hold the whole model in memory, the same load an upload's preparation budgets for
    let build_slot = state.elevation_store.build_slot().await;
    let written = tokio::task::spawn_blocking(move || {
        let _build_slot = build_slot;
        let meshes = read_meshes_in_tileset_frame(&job, &extension)?;
        write_dxf(&output_path, &meshes, source_is_z_up, cut_height)
            .map_err(|error| format!("writing the DXF failed: {error}"))
    })
    .await
    .map_err(|error| server_failure("the DXF export could not be written", error))
    .and_then(|written| {
        written.map_err(|reason| server_failure("the DXF export could not be written", reason))
    });

    let faces = match written {
        Ok(faces) => faces,
        Err(answer) => {
            remove_scratch(&scratch).await;
            return Err(answer);
        }
    };
    if faces.skipped_not_finite > 0 {
        tracing::warn!(
            "asset {id}: left {} triangles with non-finite coordinates out of the DXF",
            faces.skipped_not_finite
        );
    }
    if faces.written == 0 {
        remove_scratch(&scratch).await;
        return Err(invalid_parameter(match cut_height {
            Some(height) => format!("no geometry of asset {id} lies at or below height {height}"),
            None => format!("asset {id} has no triangles"),
        }));
    }

    scratch_files::download_and_remove(&scratch, &file_name, DXF_CONTENT_TYPE)
        .await
        .map_err(|error| server_failure("the DXF export could not be sent", error))
}

async fn remove_scratch(scratch: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(scratch).await {
        tracing::warn!("removing {} failed: {error}", scratch.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_keeps_only_what_lies_at_or_below_the_height() {
        let triangle = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 0.0, 2.0]];

        let below = at_or_below(triangle, 3.0);
        assert_eq!(below, triangle.to_vec());

        let above = at_or_below(triangle, -1.0);
        assert!(above.is_empty());

        let cut = at_or_below(triangle, 1.0);
        assert_eq!(cut.len(), 4);
        assert!(cut.iter().all(|corner| corner[2] <= 1.0));
        assert!(cut.contains(&[1.0, 0.0, 1.0]));
        assert!(cut.contains(&[0.0, 0.0, 1.0]));
    }

    const CALLING_THREAD_STACK_BYTES: usize = 64 * 1024;

    #[test]
    fn an_ifc_is_read_on_the_geometry_pool_and_not_on_the_calling_threads_stack() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tiletopia-ingest/tests/fixtures/bath_csg_solid.ifc");
        let job = JobRecord {
            id: uuid::Uuid::new_v4(),
            asset_id: uuid::Uuid::new_v4(),
            status: crate::db::JobStatus::Done,
            progress: 1.0,
            input_path: fixture.to_string_lossy().into_owned(),
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement: crate::db::ModelPlacement::default(),
            mesh_compression: Default::default(),
            texture_compression: Default::default(),
            photogrammetry_preset: Default::default(),
        };
        // ifc_lite_processing::geometry_pool spawns its threads on first use
        ifc_lite_processing::geometry_pool().unwrap();

        let meshes = std::thread::Builder::new()
            .stack_size(CALLING_THREAD_STACK_BYTES)
            .spawn(move || read_meshes_in_tileset_frame(&job, "ifc"))
            .unwrap()
            .join()
            .unwrap()
            .unwrap();

        assert!(meshes.iter().any(|mesh| !mesh.indices.is_empty()));
    }

    #[test]
    fn a_y_up_source_turns_to_z_up() {
        assert_eq!(z_up([1.0, 2.0, 3.0], false), [1.0, -3.0, 2.0]);
        assert_eq!(z_up([1.0, 2.0, 3.0], true), [1.0, 2.0, 3.0]);
    }

    fn value_after<'a>(groups: &[(i32, &'a str)], variable: &str) -> (i32, &'a str) {
        let index = groups
            .iter()
            .position(|&group| group == (9, variable))
            .unwrap_or_else(|| panic!("the header has no {variable}"));
        groups[index + 1]
    }

    #[test]
    fn an_export_is_an_r2000_drawing_in_metres_with_one_unique_owned_handle_per_entity() {
        let mesh = MeshData {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, 0.0],
                [f64::NAN, 0.0, 0.0],
            ],
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3, 0, 1, 4],
            name: "slab".to_string(),
            material: None,
            asset_id: None,
            class: None,
            storey: None,
            element_color: None,
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.dxf");

        let faces = write_dxf(&path, &[mesh], true, None).unwrap();
        assert_eq!(faces.written, 2);
        assert_eq!(faces.skipped_not_finite, 1);

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let groups: Vec<(i32, &str)> = lines
            .chunks(2)
            .map(|pair| (pair[0].parse().unwrap(), pair[1]))
            .collect();
        assert_eq!(value_after(&groups, "$ACADVER"), (1, "AC1015"));
        assert_eq!(value_after(&groups, "$INSUNITS"), (70, "6"));
        let (seed_code, seed) = value_after(&groups, "$HANDSEED");
        assert_eq!(seed_code, 5);
        let handle_seed = u64::from_str_radix(seed, 16).unwrap();

        let header_end = groups
            .iter()
            .position(|&group| group == (0, "ENDSEC"))
            .unwrap();
        let after_header = &groups[header_end + 1..];
        let mut handles = std::collections::HashSet::new();
        let mut owners = Vec::new();
        let mut face_count = 0;
        for (start, &(code, kind)) in after_header.iter().enumerate() {
            let is_entity_or_object =
                code == 0 && !matches!(kind, "SECTION" | "ENDSEC" | "ENDTAB" | "CLASS" | "EOF");
            if !is_entity_or_object {
                continue;
            }
            let body: Vec<(i32, &str)> = after_header[start + 1..]
                .iter()
                .copied()
                .take_while(|&(code, _)| code != 0)
                .collect();
            let entity_handles: Vec<&str> = body
                .iter()
                .filter(|(code, _)| *code == 5 || *code == 105)
                .map(|(_, handle)| *handle)
                .collect();
            assert_eq!(entity_handles.len(), 1, "{kind} {body:?}");
            let handle = entity_handles[0];
            assert!(handles.insert(handle), "handle {handle} repeats");
            assert!(u64::from_str_radix(handle, 16).unwrap() < handle_seed);
            let owner = body
                .iter()
                .find(|(code, _)| *code == 330)
                .unwrap_or_else(|| panic!("{kind} {handle} has no owner"))
                .1;
            owners.push(owner);
            if kind == "3DFACE" {
                face_count += 1;
                assert_eq!(
                    &body[..5],
                    &[
                        (5, handle),
                        (330, MODEL_SPACE_RECORD),
                        (100, "AcDbEntity"),
                        (8, "0"),
                        (100, "AcDbFace"),
                    ]
                );
            }
        }
        assert_eq!(face_count, 2);
        for owner in owners {
            assert!(
                owner == "0" || handles.contains(owner),
                "owner {owner} is not a handle"
            );
        }

        let read_back = tiletopia_ingest::dxf_reader::read(&path, u64::MAX, None).unwrap();
        let triangles: usize = read_back.iter().map(|mesh| mesh.indices.len() / 3).sum();
        assert_eq!(triangles, 2);
    }
}
