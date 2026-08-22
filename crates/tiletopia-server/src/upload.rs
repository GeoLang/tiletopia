//! File upload handler (multipart).

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::ModelPlacement;
use crate::{AppState, Asset, AssetStatus, AssetType, users};

/// Extensions the upload takes, named back to a caller that sent anything else.
const ACCEPTED_EXTENSIONS: &str = "las, laz, e57, ply, tif, tiff, hgt, dt0, dt1, dt2, \
     gltf, glb, obj, fbx, dae, ifc, geojson, gpkg, kml, gml, jpg, jpeg, png, jp2";

type UploadError = (StatusCode, String);

/// The created asset, plus the tiling job the upload queued. Only asset types
/// that tile on upload get a `job_id`; the rest tile via `/assets/{id}/tile`.
#[derive(Debug, Serialize)]
pub struct UploadResponse {
    #[serde(flatten)]
    pub asset: Asset,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
}

fn bad_request(message: impl Into<String>) -> UploadError {
    (StatusCode::BAD_REQUEST, message.into())
}

fn server_error(message: impl Into<String>) -> UploadError {
    (StatusCode::INTERNAL_SERVER_ERROR, message.into())
}

/// Handle multipart file upload.
pub async fn upload_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), UploadError> {
    // the route sits behind require_editor, so a valid token is always present
    let owner_id = users::claims_from_headers(&headers)
        .map_err(|status| (status, String::new()))?
        .sub;

    let mut name = None;
    let mut data = None;
    let mut longitude = None;
    let mut latitude = None;
    let mut crs = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart body"))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "name" => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("name is not text"))?,
                );
            }
            "longitude" => longitude = Some(read_degrees(field, "longitude").await?),
            "latitude" => latitude = Some(read_degrees(field, "latitude").await?),
            "crs" => {
                crs = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("crs is not text"))?,
                );
            }
            "file" => {
                let file_name = field.file_name().unwrap_or("upload").to_string();
                if name.is_none() {
                    name = Some(file_name);
                }
                data = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|_| bad_request("could not read the uploaded file"))?,
                );
            }
            _ => {}
        }
    }

    if longitude.is_some() != latitude.is_some() {
        return Err(bad_request(
            "longitude and latitude place a model together, so one without the other is refused",
        ));
    }

    let file_data = data.ok_or_else(|| bad_request("no file field in the upload"))?;
    let asset_name = name.unwrap_or_else(|| "unnamed".into());

    let asset_type = detect_asset_type(&asset_name).ok_or_else(|| {
        bad_request(format!(
            "{asset_name}: unrecognised extension. accepted extensions are {ACCEPTED_EXTENSIONS}"
        ))
    })?;

    let id = Uuid::new_v4();
    let asset_dir = state.data_dir.join(id.to_string());
    tokio::fs::create_dir_all(&asset_dir)
        .await
        .map_err(|_| server_error("could not create the asset directory"))?;

    // Write uploaded file
    let input_path = asset_dir.join("input").join(&asset_name);
    tokio::fs::create_dir_all(input_path.parent().unwrap())
        .await
        .map_err(|_| server_error("could not create the input directory"))?;
    tokio::fs::write(&input_path, &file_data)
        .await
        .map_err(|_| server_error("could not write the uploaded file"))?;

    let asset = Asset {
        id,
        name: asset_name,
        asset_type,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes: file_data.len() as u64,
        description: String::new(),
        tags: vec![],
        owner_id: Some(owner_id),
    };

    state
        .db
        .create_asset(&asset)
        .await
        .map_err(|_| server_error("could not store the asset"))?;

    // everything with a tiler behind it gets tiled to 3d tiles right away; the
    // job worker flips the asset to Ready and tileset.json becomes servable
    let job_id = if tiles_on_upload(&asset.asset_type) {
        let placement = ModelPlacement {
            longitude,
            latitude,
            crs,
        };
        let job = state
            .job_queue
            .submit(
                asset.id,
                input_path.to_string_lossy().into_owned(),
                placement,
            )
            .await
            .map_err(|_| server_error("could not queue the tiling job"))?;
        Some(job.id)
    } else {
        None
    };

    tracing::info!("Uploaded asset {} ({} bytes)", asset.id, file_data.len());
    tracing::info!("metric: assets_uploaded");

    Ok((StatusCode::CREATED, Json(UploadResponse { asset, job_id })))
}

async fn read_degrees(
    field: axum::extract::multipart::Field<'_>,
    name: &str,
) -> Result<f64, UploadError> {
    let text = field
        .text()
        .await
        .map_err(|_| bad_request(format!("{name} is not text")))?;
    text.trim()
        .parse()
        .map_err(|_| bad_request(format!("{name} is not a number: {text}")))
}

fn tiles_on_upload(asset_type: &AssetType) -> bool {
    matches!(
        asset_type,
        AssetType::PointCloud | AssetType::Model | AssetType::Vector
    )
}

fn detect_asset_type(name: &str) -> Option<AssetType> {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "las" | "laz" | "e57" | "ply" => Some(AssetType::PointCloud),
        "tif" | "tiff" | "hgt" | "dt0" | "dt1" | "dt2" => Some(AssetType::Terrain),
        "gltf" | "glb" | "obj" | "fbx" | "ifc" | "dae" => Some(AssetType::Model),
        "geojson" | "gpkg" | "kml" | "gml" => Some(AssetType::Vector),
        "jpg" | "jpeg" | "png" | "jp2" => Some(AssetType::Imagery),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_accepted_extension_maps_to_its_asset_type() {
        for ext in ["las", "laz", "e57", "ply"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::PointCloud)
            );
        }
        for ext in ["tif", "tiff", "hgt", "dt0", "dt1", "dt2"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Terrain)
            );
        }
        for ext in ["gltf", "glb", "obj", "fbx", "ifc", "dae"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Model)
            );
        }
        for ext in ["geojson", "gpkg", "kml", "gml"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Vector)
            );
        }
        for ext in ["jpg", "jpeg", "png", "jp2"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Imagery)
            );
        }
    }

    #[test]
    fn an_unknown_extension_is_no_asset_type_at_all() {
        assert_eq!(detect_asset_type("notes.txt"), None);
        assert_eq!(detect_asset_type("no-extension"), None);
    }

    #[test]
    fn point_clouds_models_and_vectors_tile_on_upload() {
        assert!(tiles_on_upload(&AssetType::PointCloud));
        assert!(tiles_on_upload(&AssetType::Model));
        assert!(tiles_on_upload(&AssetType::Vector));
        assert!(!tiles_on_upload(&AssetType::Terrain));
        assert!(!tiles_on_upload(&AssetType::Imagery));
    }
}
