//! File upload handler (multipart).

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::db::ModelPlacement;
use crate::{AppState, Asset, AssetStatus, AssetType, users};

/// Extensions the upload takes, named back to a caller that sent anything else.
const ACCEPTED_EXTENSIONS: &str = "las, laz, e57, ply, tif, tiff, hgt, dt0, dt1, dt2, \
     gltf, glb, obj, fbx, dae, ifc, geojson, gpkg, kml, gml, jpg, jpeg, png, jp2";

/// Largest upload the server takes, on the asset and tileset routes alike.
/// Axum's own default is 2 MB, which is smaller than most of what either takes.
pub const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024 * 1024;

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

    let id = Uuid::new_v4();
    let asset_dir = state.data_dir.join(id.to_string());

    let UploadFields {
        name,
        streamed_path,
        longitude,
        latitude,
        crs,
    } = match read_fields(&mut multipart, &asset_dir).await {
        Ok(fields) => fields,
        Err(error) => return Err(discard(&asset_dir, error).await),
    };

    if longitude.is_some() != latitude.is_some() {
        return Err(discard(
            &asset_dir,
            bad_request(
                "longitude and latitude place a model together, so one without the other is refused",
            ),
        )
        .await);
    }

    let streamed_path = streamed_path.ok_or_else(|| bad_request("no file field in the upload"))?;
    let asset_name = name.unwrap_or_else(|| "unnamed".into());

    let Some(asset_type) = detect_asset_type(&asset_name) else {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: unrecognised extension. accepted extensions are {ACCEPTED_EXTENSIONS}"
            )),
        )
        .await);
    };

    let input_path = asset_dir.join("input").join(&asset_name);
    let size_bytes = match finish_input(&streamed_path, &input_path).await {
        Ok(size_bytes) => size_bytes,
        Err(error) => return Err(discard(&asset_dir, error).await),
    };

    let asset = Asset {
        id,
        name: asset_name,
        asset_type,
        status: AssetStatus::Uploading,
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes,
        description: String::new(),
        tags: vec![],
        owner_id: Some(owner_id),
    };

    if state.db.create_asset(&asset).await.is_err() {
        return Err(discard(&asset_dir, server_error("could not store the asset")).await);
    }

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

    tracing::info!("Uploaded asset {} ({} bytes)", asset.id, size_bytes);
    tracing::info!("metric: assets_uploaded");

    Ok((StatusCode::CREATED, Json(UploadResponse { asset, job_id })))
}

/// The upload's fields, with the file itself already streamed to disk.
struct UploadFields {
    name: Option<String>,
    streamed_path: Option<PathBuf>,
    longitude: Option<f64>,
    latitude: Option<f64>,
    crs: Option<String>,
}

async fn read_fields(
    multipart: &mut Multipart,
    asset_dir: &Path,
) -> Result<UploadFields, UploadError> {
    let mut fields = UploadFields {
        name: None,
        streamed_path: None,
        longitude: None,
        latitude: None,
        crs: None,
    };

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| bad_request("malformed multipart body"))?
    {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "name" => {
                fields.name = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("name is not text"))?,
                );
            }
            "longitude" => fields.longitude = Some(read_degrees(field, "longitude").await?),
            "latitude" => fields.latitude = Some(read_degrees(field, "latitude").await?),
            "crs" => {
                fields.crs = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("crs is not text"))?,
                );
            }
            "file" => {
                let file_name = field.file_name().unwrap_or("upload").to_string();
                if fields.name.is_none() {
                    fields.name = Some(file_name.clone());
                }
                // the bytes land before a `name` field that may still be coming
                // can rename them
                let path = asset_dir.join("input").join(&file_name);
                write_field(&mut field, &path).await?;
                fields.streamed_path = Some(path);
            }
            _ => {}
        }
    }

    Ok(fields)
}

/// Stream one multipart field to `path`. The whole point of an upload route is
/// a file too big to hold in memory, so the bytes go straight to disk.
pub(crate) async fn write_field(
    field: &mut axum::extract::multipart::Field<'_>,
    path: &Path,
) -> Result<(), UploadError> {
    use tokio::io::AsyncWriteExt;

    tokio::fs::create_dir_all(path.parent().unwrap_or(path))
        .await
        .map_err(|_| server_error("could not create the upload directory"))?;
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|_| server_error("could not create the uploaded file"))?;

    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| bad_request("could not read the uploaded file"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|_| server_error("could not write the uploaded file"))?;
    }
    file.flush()
        .await
        .map_err(|_| server_error("could not write the uploaded file"))?;
    Ok(())
}

/// Move the streamed file to the name the upload settled on and report how many
/// bytes it holds.
async fn finish_input(streamed_path: &Path, input_path: &Path) -> Result<u64, UploadError> {
    if streamed_path != input_path {
        tokio::fs::rename(streamed_path, input_path)
            .await
            .map_err(|_| server_error("could not write the uploaded file"))?;
    }
    let metadata = tokio::fs::metadata(input_path)
        .await
        .map_err(|_| server_error("could not write the uploaded file"))?;
    Ok(metadata.len())
}

/// Drop the directory the upload streamed into, so a refusal after the write
/// leaves no file behind.
async fn discard(asset_dir: &Path, error: UploadError) -> UploadError {
    let _ = tokio::fs::remove_dir_all(asset_dir).await;
    error
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
