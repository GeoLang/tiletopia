use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, State},
    http::HeaderMap,
    response::Json,
    routing::get,
};
use serde::Serialize;
use tiletopia_ingest::ifc_alignment::{AlignmentCurve, alignments_of};
use tiletopia_ingest::ifc_reader::{map_conversion_placement, mapped_ifc};

use crate::dxf_export::mesh_source;
use crate::elements::{invalid_parameter, permitted_asset, server_failure};
use crate::elevation::JOB_BUDGET_BYTES;
use crate::{AppState, may_view_asset};

pub fn alignment_routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/assets/{id}/alignments", get(get_alignments))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AlignmentBody {
    id: String,
    name: String,
    length: f64,
    approximate: bool,
    stations: Vec<StationBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StationBody {
    distance: f64,
    position: [f64; 3],
    tangent: [f64; 3],
}

async fn get_alignments(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<AlignmentBody>>, crate::ion_compat::IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(invalid_parameter(format!(
            "asset {id}'s source is a .{}, which has no alignments",
            source.extension
        )));
    }
    let job = source.job;
    let build_slot = state.elevation_store.build_slot().await;
    let alignments = tokio::task::spawn_blocking(move || {
        let _build_slot = build_slot;
        alignments_in_frame(&job)
    })
    .await
    .map_err(|error| server_failure("the alignments could not be read", error))?
    .map_err(|reason| server_failure("the alignments could not be read", reason))?;
    Ok(Json(alignments))
}

fn alignments_in_frame(job: &crate::db::JobRecord) -> Result<Vec<AlignmentBody>, String> {
    let path = std::path::Path::new(&job.input_path);
    let bytes = mapped_ifc(path).map_err(|error| error.to_string())?;
    let mut curves = alignments_of(&bytes);
    place_curves(job, &bytes, &mut curves)?;
    Ok(curves
        .into_iter()
        .map(|curve| AlignmentBody {
            id: curve.id,
            name: curve.name,
            length: curve.length,
            approximate: curve.approximate,
            stations: curve
                .stations
                .into_iter()
                .map(|station| StationBody {
                    distance: station.distance,
                    position: station.position,
                    tangent: station.tangent,
                })
                .collect(),
        })
        .collect())
}

fn place_curves(
    job: &crate::db::JobRecord,
    source: &[u8],
    curves: &mut [AlignmentCurve],
) -> Result<(), String> {
    let upload_places = job.placement.longitude.is_some() && job.placement.latitude.is_some();
    if upload_places {
        return Ok(());
    }
    let Some(placement) = map_conversion_placement(source).map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let path = std::path::Path::new(&job.input_path);
    let meshes =
        tiletopia_ingest::read_mesh(path, JOB_BUDGET_BYTES).map_err(|error| error.to_string())?;
    if placement
        .flat_frame_is_within_tolerance(&meshes)
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    for curve in curves {
        for station in &mut curve.stations {
            let stepped: [f64; 3] =
                std::array::from_fn(|axis| station.position[axis] + station.tangent[axis] * 0.01);
            let position = placement
                .east_north_up_of(station.position)
                .map_err(|error| error.to_string())?;
            let stepped = placement
                .east_north_up_of(stepped)
                .map_err(|error| error.to_string())?;
            let delta: [f64; 3] = std::array::from_fn(|axis| stepped[axis] - position[axis]);
            let span = delta.iter().map(|axis| axis * axis).sum::<f64>().sqrt();
            station.position = position;
            if span > 1.0e-9 {
                station.tangent = std::array::from_fn(|axis| delta[axis] / span);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::place_curves;
    use crate::db::{JobRecord, JobStatus, ModelPlacement};
    use tiletopia_ingest::ifc_alignment::{AlignmentCurve, AlignmentStation};

    fn job(longitude: Option<f64>, latitude: Option<f64>) -> JobRecord {
        JobRecord {
            id: uuid::Uuid::nil(),
            asset_id: uuid::Uuid::nil(),
            status: JobStatus::Done,
            progress: 1.0,
            input_path: String::new(),
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement: ModelPlacement {
                longitude,
                latitude,
                ..ModelPlacement::default()
            },
            mesh_compression: Default::default(),
            texture_compression: Default::default(),
            photogrammetry_preset: Default::default(),
        }
    }

    fn curve() -> AlignmentCurve {
        AlignmentCurve {
            id: "road".to_string(),
            name: "Road".to_string(),
            length: 1.0,
            approximate: false,
            stations: vec![AlignmentStation {
                distance: 0.0,
                position: [1.0, 2.0, 3.0],
                tangent: [1.0, 0.0, 0.0],
            }],
        }
    }

    const BARE_IFC: &[u8] = b"ISO-10303-21;\nHEADER;\nFILE_SCHEMA(('IFC4X3'));\nENDSEC;\nDATA;\nENDSEC;\nEND-ISO-10303-21;\n";

    #[test]
    fn an_upload_that_placed_the_model_keeps_the_file_stations() {
        let mut curves = vec![curve()];
        place_curves(&job(Some(-71.0), Some(42.0)), b"", &mut curves).unwrap();
        assert_eq!(curves[0].stations[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(curves[0].stations[0].tangent, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn a_file_with_no_map_conversion_keeps_the_file_stations() {
        let mut curves = vec![curve()];
        place_curves(&job(None, None), BARE_IFC, &mut curves).unwrap();
        assert_eq!(curves[0].stations[0].position, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn one_coordinate_is_not_an_upload_placement() {
        let mut curves = vec![curve()];
        place_curves(&job(Some(-71.0), None), BARE_IFC, &mut curves).unwrap();
        assert_eq!(curves[0].stations[0].position, [1.0, 2.0, 3.0]);
        assert_eq!(curves[0].stations[0].tangent, [1.0, 0.0, 0.0]);
    }
}
