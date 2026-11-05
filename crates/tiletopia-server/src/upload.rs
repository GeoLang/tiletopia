//! File upload handler (multipart).

use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{FromRequest, Multipart, Request, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use futures::StreamExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tiletopia_core::glb_writer::MeshCompression;
use uuid::Uuid;

use crate::audit::AuditedResource;
use crate::db::ModelPlacement;
use crate::imagery::{self, ImageryForm, PreparedUpload};
use crate::job_queue::{Tiler, stored_input_path, tiler_for};
use crate::model_versions;
use crate::photogrammetry::{self, NODE_ODM_URL_ENV, PhotogrammetryPreset};
use crate::zip_unpack::unpack_zip;
use crate::{AppState, Asset, AssetStatus, AssetType, users};
use tiletopia_core::mesh_tiler::TextureCompression;
use tiletopia_ingest::dxf_reader::DrawingUnit;

/// Extensions the upload takes, named back to a caller that sent anything else.
const ACCEPTED_EXTENSIONS: &str = "las, laz, e57, ply, tif, tiff, hgt, dt0, dt1, dt2, \
     gltf, glb, obj, fbx, dae, dxf, ifc, ifczip, geojson, topojson, gpkg, kml, gml, czml, jpg, jpeg, png, jp2, j2k, zip";

/// Largest upload the server takes, on the asset and tileset routes alike.
/// Axum's own default is 2 MB, which is smaller than most of what either takes.
pub const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024 * 1024;

// world files are six numbers and WKT CRS strings run to a few KiB
pub const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;

// hyper's http/2 window delivers up to 1 MiB of file bytes inside one next_field call
pub const MAX_BYTES_AWAITING_FIELD: usize = 4 * 1024 * 1024;

type UploadError = (StatusCode, String);

struct BytesAwaitingField {
    counting: AtomicBool,
    bytes: AtomicUsize,
    most_bytes: usize,
}

pub struct UploadMultipart {
    inner: Multipart,
    awaiting_field: Arc<BytesAwaitingField>,
}

impl UploadMultipart {
    async fn with_limit_awaiting_field(
        request: Request,
        most_bytes: usize,
    ) -> Result<Self, axum::extract::multipart::MultipartRejection> {
        let awaiting_field = Arc::new(BytesAwaitingField {
            counting: AtomicBool::new(false),
            bytes: AtomicUsize::new(0),
            most_bytes,
        });
        let counted = Arc::clone(&awaiting_field);
        let request = request.map(|body| {
            Body::from_stream(body.into_data_stream().map(
                move |frame| -> Result<Bytes, axum::BoxError> {
                    let frame = frame?;
                    if !counted.counting.load(Ordering::SeqCst) {
                        return Ok(frame);
                    }
                    // Body::from puts a whole multipart body in one frame, file bytes included
                    let bytes_before_frame = counted.bytes.fetch_add(frame.len(), Ordering::SeqCst);
                    if bytes_before_frame > counted.most_bytes {
                        return Err("no multipart field within the limit".into());
                    }
                    Ok(frame)
                },
            ))
        });
        Ok(Self {
            inner: Multipart::from_request(request, &()).await?,
            awaiting_field,
        })
    }

    pub(crate) async fn next_field(
        &mut self,
    ) -> Result<Option<axum::extract::multipart::Field<'_>>, UploadError> {
        // multer buffers a preamble or field headers with no limit until the boundary or blank line arrives
        self.awaiting_field.bytes.store(0, Ordering::SeqCst);
        self.awaiting_field.counting.store(true, Ordering::SeqCst);
        let next = self.inner.next_field().await;
        self.awaiting_field.counting.store(false, Ordering::SeqCst);
        let is_past_limit =
            self.awaiting_field.bytes.load(Ordering::SeqCst) > self.awaiting_field.most_bytes;
        next.map_err(|_| {
            if !is_past_limit {
                return bad_request("malformed multipart body");
            }
            bad_request(format!(
                "the multipart body runs {} bytes without reaching a field",
                self.awaiting_field.most_bytes
            ))
        })
    }
}

impl<S: Send + Sync> FromRequest<S> for UploadMultipart {
    type Rejection = axum::extract::multipart::MultipartRejection;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Self::Rejection> {
        Self::with_limit_awaiting_field(request, MAX_BYTES_AWAITING_FIELD).await
    }
}

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

pub(crate) fn lowercase_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

pub(crate) async fn prepare_input(
    state: &AppState,
    input_path: &Path,
    asset_dir: &Path,
    detected_type: AssetType,
    imagery_form: ImageryForm,
) -> Result<PreparedUpload, String> {
    let extension = lowercase_extension(input_path);
    let is_photo_set = photogrammetry::is_photo_set(input_path);
    let _build_slot = if imagery::takes_imagery_form(&extension) || is_photo_set {
        Some(state.elevation_store.build_slot().await)
    } else {
        None
    };
    let input_path = input_path.to_path_buf();
    let asset_dir = asset_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if is_photo_set {
            photogrammetry::check_photo_set(&input_path)?;
        }
        imagery::prepare_upload(&input_path, &asset_dir, detected_type, &imagery_form)
    })
    .await
    .expect("imagery preparation task panicked")
}

/// Handle multipart file upload.
pub async fn upload_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: UploadMultipart,
) -> Result<(StatusCode, Extension<AuditedResource>, Json<UploadResponse>), UploadError> {
    // the route sits behind require_editor, so a valid token is always present
    let claims = users::claims_from_headers(&headers).map_err(|status| (status, String::new()))?;

    let id = Uuid::new_v4();
    let asset_dir = state.data_dir.join(id.to_string());

    let UploadFields {
        name,
        world_file,
        bounds,
        requested_asset_type,
        streamed_path,
        longitude,
        latitude,
        crs,
        mesh_compression,
        texture_compression,
        units,
        photogrammetry_preset,
        base_asset_id,
    } = match read_fields(&mut multipart, &asset_dir).await {
        Ok(fields) => fields,
        Err(error) => return Err(discard(&asset_dir, error).await),
    };

    let base_asset = match &base_asset_id {
        Some(base_asset_id) => {
            match model_versions::checked_base_asset(&state, &claims, base_asset_id).await {
                Ok(base_asset) => Some(base_asset),
                Err(error) => return Err(discard(&asset_dir, error).await),
            }
        }
        None => None,
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

    let Some(detected_type) = detect_asset_type(&asset_name) else {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: unrecognised extension. accepted extensions are {ACCEPTED_EXTENSIONS}"
            )),
        )
        .await);
    };
    if base_asset.is_some() && detected_type != AssetType::Model {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: base_asset_id applies to a model upload"
            )),
        )
        .await);
    }

    let imagery_form = ImageryForm {
        world_file,
        bounds,
        crs: crs.clone(),
        asset_type: requested_asset_type,
    };
    let extension = lowercase_extension(Path::new(&asset_name));
    let is_photo_set = photogrammetry::is_photo_set(Path::new(&asset_name));
    if let Some(refusal) = photo_set_refusal(
        &state,
        is_photo_set,
        &PhotoSetForm {
            places_a_model: longitude.is_some() || crs.is_some(),
            texture_compression,
            photogrammetry_preset,
        },
    ) {
        return Err(discard(&asset_dir, bad_request(format!("{asset_name}: {refusal}"))).await);
    }
    let tiles_natively = matches!(tiler_for(&extension), Ok(Tiler::Native { .. }));
    if crs.is_some() && tiles_natively {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: crs does not apply to meshes the native tiler takes, which are placed by longitude and latitude"
            )),
        )
        .await);
    }
    if texture_compression != TextureCompression::None && !tiles_natively {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: texture_compression applies to meshes the native tiler takes"
            )),
        )
        .await);
    }
    if units.is_some() && extension != "dxf" {
        return Err(discard(
            &asset_dir,
            bad_request(format!("{asset_name}: units applies to dxf uploads")),
        )
        .await);
    }
    if imagery_form.places_a_raster() && !imagery::takes_imagery_form(&extension) {
        return Err(discard(
            &asset_dir,
            bad_request(format!(
                "{asset_name}: world_file, bounds and asset_type apply to tif, tiff, jpg, jpeg, png, jp2 and j2k uploads"
            )),
        )
        .await);
    }

    let input_path = asset_dir.join("input").join(&asset_name);
    let size_bytes = match finish_input(&streamed_path, &input_path).await {
        Ok(size_bytes) => size_bytes,
        Err(error) => return Err(discard(&asset_dir, error).await),
    };

    let input_path = if extension == "ifczip" {
        let zip_path = input_path;
        let scratch_dir = asset_dir.join("ifczip_unpacked");
        let unpacked =
            tokio::task::spawn_blocking(move || unpack_single_ifc(&zip_path, &scratch_dir))
                .await
                .expect("IFC zip unpack task panicked");
        match unpacked {
            Ok(ifc_path) => ifc_path,
            Err(reason) => {
                return Err(
                    discard(&asset_dir, bad_request(format!("{asset_name}: {reason}"))).await,
                );
            }
        }
    } else {
        input_path
    };

    if extension == "dxf" {
        let dxf_path = input_path.clone();
        let checked = tokio::task::spawn_blocking(move || {
            tiletopia_ingest::dxf_reader::drawing_unit(&dxf_path, units)
        })
        .await
        .expect("DXF unit check task panicked");
        if let Err(reason) = checked {
            return Err(discard(&asset_dir, bad_request(format!("{asset_name}: {reason}"))).await);
        }
    }

    let PreparedUpload {
        asset_type,
        tiles_ready,
    } = match prepare_input(&state, &input_path, &asset_dir, detected_type, imagery_form).await {
        Ok(prepared) => prepared,
        Err(reason) => {
            return Err(discard(&asset_dir, bad_request(format!("{asset_name}: {reason}"))).await);
        }
    };

    let asset = Asset {
        id,
        name: asset_name,
        asset_type,
        status: if tiles_ready {
            AssetStatus::Ready
        } else {
            AssetStatus::Uploading
        },
        created_at: chrono::Utc::now(),
        tile_count: 0,
        size_bytes,
        description: String::new(),
        tags: vec![],
        owner_id: model_versions::owner_of_upload(base_asset.as_ref(), claims.sub),
    };

    if state.db.create_asset(&asset).await.is_err() {
        return Err(discard(&asset_dir, server_error("could not store the asset")).await);
    }
    if let Some(base_asset) = &base_asset
        && let Err(error) = model_versions::add_version(&state.db, asset.id, base_asset.id).await
    {
        tracing::error!("recording asset {} as a version failed: {error}", asset.id);
        if let Err(error) = state.db.delete_asset(asset.id).await {
            tracing::error!("removing asset {} failed: {error}", asset.id);
        }
        return Err(discard(&asset_dir, server_error("could not record the new version")).await);
    }

    let job_id = if tiles_on_upload(&asset.asset_type) {
        let placement = ModelPlacement {
            longitude,
            latitude,
            crs,
            units: units.map(|unit| unit.name.to_string()),
        };
        let job = state
            .job_queue
            .submit(
                asset.id,
                stored_input_path(
                    input_path
                        .file_name()
                        .expect("an input path ends in the checked asset name"),
                ),
                placement,
                mesh_compression,
                texture_compression,
                photogrammetry_preset.unwrap_or_default(),
            )
            .await
            .map_err(|_| server_error("could not queue the tiling job"))?;
        Some(job.id)
    } else {
        None
    };

    tracing::info!("Uploaded asset {} ({} bytes)", asset.id, size_bytes);
    tracing::info!("metric: assets_uploaded");

    Ok((
        StatusCode::CREATED,
        Extension(AuditedResource(asset.id.to_string())),
        Json(UploadResponse { asset, job_id }),
    ))
}

struct PhotoSetForm {
    places_a_model: bool,
    texture_compression: TextureCompression,
    photogrammetry_preset: Option<PhotogrammetryPreset>,
}

fn photo_set_refusal(state: &AppState, is_photo_set: bool, form: &PhotoSetForm) -> Option<String> {
    if !is_photo_set {
        return form
            .photogrammetry_preset
            .map(|_| "photogrammetry_preset applies to zip photo sets".to_string());
    }
    if !state.job_queue.reconstructs_photo_sets() {
        return Some(format!(
            "{NODE_ODM_URL_ENV} is not set, so there is no NodeODM to reconstruct a photo set"
        ));
    }
    if form.places_a_model {
        return Some(
            "a photo set is placed by its photos' EXIF GPS, so longitude, latitude and crs are refused"
                .to_string(),
        );
    }
    if form.texture_compression == TextureCompression::Ktx2 {
        // mesh_tiler holds a PNG crop of a texture page for every tile before it encodes any KTX2
        return Some(
            "texture_compression ktx2 is refused on a photo set, which tiles with none".to_string(),
        );
    }
    None
}

/// The upload's fields, with the file itself already streamed to disk.
struct UploadFields {
    name: Option<String>,
    world_file: Option<String>,
    bounds: Option<String>,
    requested_asset_type: Option<String>,
    streamed_path: Option<PathBuf>,
    longitude: Option<f64>,
    latitude: Option<f64>,
    crs: Option<String>,
    mesh_compression: MeshCompression,
    texture_compression: TextureCompression,
    units: Option<DrawingUnit>,
    photogrammetry_preset: Option<PhotogrammetryPreset>,
    base_asset_id: Option<String>,
}

async fn read_fields(
    multipart: &mut UploadMultipart,
    asset_dir: &Path,
) -> Result<UploadFields, UploadError> {
    let mut fields = UploadFields {
        name: None,
        world_file: None,
        bounds: None,
        requested_asset_type: None,
        streamed_path: None,
        longitude: None,
        latitude: None,
        crs: None,
        mesh_compression: MeshCompression::Meshopt,
        texture_compression: TextureCompression::None,
        units: None,
        photogrammetry_preset: None,
        base_asset_id: None,
    };

    while let Some(mut field) = multipart.next_field().await? {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "name" => {
                let name = read_text_field(&mut field, "name", MAX_TEXT_FIELD_BYTES).await?;
                fields.name = Some(checked_file_name(&name)?);
            }
            "world_file" => {
                fields.world_file =
                    Some(read_text_field(&mut field, "world_file", MAX_TEXT_FIELD_BYTES).await?);
            }
            "bounds" => {
                fields.bounds =
                    Some(read_text_field(&mut field, "bounds", MAX_TEXT_FIELD_BYTES).await?);
            }
            "asset_type" => {
                fields.requested_asset_type =
                    Some(read_text_field(&mut field, "asset_type", MAX_TEXT_FIELD_BYTES).await?);
            }
            "longitude" => fields.longitude = Some(read_degrees(&mut field, "longitude").await?),
            "latitude" => fields.latitude = Some(read_degrees(&mut field, "latitude").await?),
            "crs" => {
                fields.crs = Some(read_text_field(&mut field, "crs", MAX_TEXT_FIELD_BYTES).await?);
            }
            "mesh_compression" => {
                let text =
                    read_text_field(&mut field, "mesh_compression", MAX_TEXT_FIELD_BYTES).await?;
                fields.mesh_compression = parse_mesh_compression(&text)?;
            }
            "units" => {
                let text = read_text_field(&mut field, "units", MAX_TEXT_FIELD_BYTES).await?;
                fields.units = Some(
                    DrawingUnit::named(text.trim())
                        .map_err(|reason| bad_request(reason.to_string()))?,
                );
            }
            "texture_compression" => {
                let text = read_text_field(&mut field, "texture_compression", MAX_TEXT_FIELD_BYTES)
                    .await?;
                fields.texture_compression = match text.trim() {
                    "none" => TextureCompression::None,
                    "ktx2" => TextureCompression::Ktx2,
                    other => {
                        return Err(bad_request(format!(
                            "texture_compression {other}: accepted values are ktx2 and none"
                        )));
                    }
                };
            }
            "base_asset_id" => {
                fields.base_asset_id = Some(
                    field
                        .text()
                        .await
                        .map_err(|_| bad_request("base_asset_id is not text"))?,
                );
            }
            "photogrammetry_preset" => {
                let text =
                    read_text_field(&mut field, "photogrammetry_preset", MAX_TEXT_FIELD_BYTES)
                        .await?;
                fields.photogrammetry_preset = Some(parse_photogrammetry_preset(&text)?);
            }
            "file" => {
                let file_name = checked_file_name(field.file_name().unwrap_or("upload"))?;
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

pub(crate) async fn read_text_field(
    field: &mut axum::extract::multipart::Field<'_>,
    field_name: &str,
    most_bytes: usize,
) -> Result<String, UploadError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|_| bad_request(format!("could not read {field_name}")))?
    {
        if bytes.len() + chunk.len() > most_bytes {
            return Err(bad_request(format!(
                "{field_name} is longer than {most_bytes} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| bad_request(format!("{field_name} is not text")))
}

fn parse_mesh_compression(text: &str) -> Result<MeshCompression, UploadError> {
    serde_json::from_value(serde_json::json!(text)).map_err(|_| {
        bad_request(format!(
            "mesh_compression {text}: accepted values are draco, meshopt and none"
        ))
    })
}

fn parse_photogrammetry_preset(text: &str) -> Result<PhotogrammetryPreset, UploadError> {
    serde_json::from_value(serde_json::json!(text)).map_err(|_| {
        bad_request(format!(
            "photogrammetry_preset {text}: accepted values are fast, default and high_quality"
        ))
    })
}

/// Both the multipart filename and the `name` field become a component of the
/// path the upload writes to, so a name that is anything but one file name is
/// refused rather than followed out of the asset directory.
pub(crate) fn checked_file_name(name: &str) -> Result<String, UploadError> {
    let is_one_component =
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']);
    if !is_one_component {
        return Err(bad_request(format!(
            "{name}: a name is one file name, not a path"
        )));
    }
    Ok(name.to_string())
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

fn unpack_single_ifc(zip_path: &Path, scratch_dir: &Path) -> Result<PathBuf, String> {
    let unpacked = unpack_zip(zip_path, scratch_dir)?;
    let ifc_entries: Vec<&PathBuf> = unpacked
        .relative_paths
        .iter()
        .filter(|path| lowercase_extension(path) == "ifc")
        .collect();
    let [ifc_entry] = ifc_entries.as_slice() else {
        return Err(format!(
            "an ifczip holds one .ifc, this one holds {}",
            ifc_entries.len()
        ));
    };
    let ifc_path = zip_path.with_extension("ifc");
    std::fs::rename(scratch_dir.join(ifc_entry), &ifc_path)
        .map_err(|e| format!("could not write {}: {e}", ifc_path.display()))?;
    std::fs::remove_dir_all(scratch_dir)
        .map_err(|e| format!("could not remove {}: {e}", scratch_dir.display()))?;
    std::fs::remove_file(zip_path)
        .map_err(|e| format!("could not remove {}: {e}", zip_path.display()))?;
    Ok(ifc_path)
}

/// Drop the directory the upload streamed into, so a refusal after the write
/// leaves no file behind.
async fn discard(asset_dir: &Path, error: UploadError) -> UploadError {
    let _ = tokio::fs::remove_dir_all(asset_dir).await;
    error
}

async fn read_degrees(
    field: &mut axum::extract::multipart::Field<'_>,
    name: &str,
) -> Result<f64, UploadError> {
    let text = read_text_field(field, name, MAX_TEXT_FIELD_BYTES).await?;
    text.trim()
        .parse()
        .map_err(|_| bad_request(format!("{name} is not a number: {text}")))
}

pub(crate) fn tiles_on_upload(asset_type: &AssetType) -> bool {
    matches!(
        asset_type,
        AssetType::PointCloud | AssetType::Model | AssetType::Vector | AssetType::Terrain
    )
}

fn detect_asset_type(name: &str) -> Option<AssetType> {
    let ext = lowercase_extension(Path::new(name));
    match ext.as_str() {
        "las" | "laz" | "e57" | "ply" => Some(AssetType::PointCloud),
        "tif" | "tiff" | "hgt" | "dt0" | "dt1" | "dt2" => Some(AssetType::Terrain),
        "gltf" | "glb" | "obj" | "fbx" | "ifc" | "ifczip" | "dae" | "dxf" | "zip" => {
            Some(AssetType::Model)
        }
        "geojson" | "topojson" | "gpkg" | "kml" | "gml" => Some(AssetType::Vector),
        "jpg" | "jpeg" | "png" | "jp2" | "j2k" => Some(AssetType::Imagery),
        "czml" => Some(AssetType::Czml),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::Request;

    const FIELD_HEAD: &[u8] = b"--bound\r\nContent-Disposition: form-data; name=\"crs\"\r\n\r\n";

    async fn multipart_of(body: Body) -> Multipart {
        let request = Request::builder()
            .header("content-type", "multipart/form-data; boundary=bound")
            .body(body)
            .unwrap();
        Multipart::from_request(request, &()).await.unwrap()
    }

    #[tokio::test]
    async fn a_text_field_past_the_cap_is_refused_before_the_rest_of_the_body_is_read() {
        const VALUE_FRAMES: usize = 1000;
        let frames_pulled = Arc::new(AtomicUsize::new(0));
        let pulled_by_body = Arc::clone(&frames_pulled);
        let value_frames = futures::stream::iter(0..VALUE_FRAMES).then(move |_| {
            let pulled_by_body = Arc::clone(&pulled_by_body);
            async move {
                pulled_by_body.fetch_add(1, Ordering::SeqCst);
                // multer's stream buffer pulls frames until one is pending, as hyper's body is between reads
                tokio::task::yield_now().await;
                Ok::<Bytes, std::io::Error>(Bytes::from_static(b"12345678"))
            }
        });
        let frames = futures::stream::once(async {
            Ok::<Bytes, std::io::Error>(Bytes::from_static(FIELD_HEAD))
        })
        .chain(value_frames);
        let mut multipart = multipart_of(Body::from_stream(frames)).await;
        let mut field = multipart.next_field().await.unwrap().unwrap();

        let refusal = read_text_field(&mut field, "crs", 16).await.unwrap_err();

        assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
        assert!(
            refusal.1.contains("crs is longer than 16 bytes"),
            "{}",
            refusal.1
        );
        assert!(frames_pulled.load(Ordering::SeqCst) < VALUE_FRAMES);
    }

    static FILLER_FRAME: [u8; 256] = [b'x'; 256];

    fn counted_filler_body(
        head: &'static [u8],
        filler_frames: usize,
        tail: &'static [u8],
    ) -> (Body, Arc<AtomicUsize>) {
        let frames_pulled = Arc::new(AtomicUsize::new(0));
        let pulled_by_body = Arc::clone(&frames_pulled);
        let filler = futures::stream::iter(0..filler_frames).then(move |_| {
            let pulled_by_body = Arc::clone(&pulled_by_body);
            async move {
                pulled_by_body.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok::<Bytes, std::io::Error>(Bytes::from_static(&FILLER_FRAME))
            }
        });
        let frames =
            futures::stream::once(
                async move { Ok::<Bytes, std::io::Error>(Bytes::from_static(head)) },
            )
            .chain(filler)
            .chain(futures::stream::once(async move {
                Ok::<Bytes, std::io::Error>(Bytes::from_static(tail))
            }));
        (Body::from_stream(frames), frames_pulled)
    }

    async fn multipart_limited_to(body: Body, most_bytes: usize) -> UploadMultipart {
        let request = Request::builder()
            .header("content-type", "multipart/form-data; boundary=bound")
            .body(body)
            .unwrap();
        UploadMultipart::with_limit_awaiting_field(request, most_bytes)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_body_with_no_boundary_or_no_blank_line_is_refused_after_a_bounded_number_of_frames()
    {
        const FILLER_FRAMES: usize = 1000;
        let headers_with_no_blank_line: &[u8] =
            b"--bound\r\nContent-Disposition: form-data; name=\"crs\"";
        for head in [b"".as_slice(), headers_with_no_blank_line] {
            let (body, frames_pulled) = counted_filler_body(head, FILLER_FRAMES, b"");
            let mut multipart = multipart_limited_to(body, 1024).await;

            let refusal = multipart.next_field().await.unwrap_err();

            assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
            assert!(
                refusal.1.contains("without reaching a field"),
                "{}",
                refusal.1
            );
            assert!(frames_pulled.load(Ordering::SeqCst) <= 1024 / FILLER_FRAME.len() + 2);
        }
    }

    #[tokio::test]
    async fn field_data_past_the_limit_is_not_counted_as_awaiting_a_field() {
        let (body, _) = counted_filler_body(FIELD_HEAD, 8, b"\r\n--bound--\r\n");
        let mut multipart = multipart_limited_to(body, 1024).await;

        let mut field = multipart.next_field().await.unwrap().unwrap();
        let text = read_text_field(&mut field, "crs", 4096).await.unwrap();
        drop(field);

        assert_eq!(text.len(), 8 * FILLER_FRAME.len());
        assert!(multipart.next_field().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_text_field_at_the_cap_is_read_whole() {
        let body = [FIELD_HEAD, b"EPSG:4326\r\n--bound--\r\n".as_slice()].concat();
        let mut multipart = multipart_of(Body::from(body)).await;
        let mut field = multipart.next_field().await.unwrap().unwrap();

        assert_eq!(
            read_text_field(&mut field, "crs", 9).await.unwrap(),
            "EPSG:4326"
        );
    }

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
        for ext in [
            "gltf", "glb", "obj", "fbx", "ifc", "ifczip", "dae", "dxf", "zip",
        ] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Model)
            );
        }
        for ext in ["geojson", "topojson", "gpkg", "kml", "gml"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Vector)
            );
        }
        for ext in ["jpg", "jpeg", "png", "jp2", "j2k"] {
            assert_eq!(
                detect_asset_type(&format!("a.{ext}")),
                Some(AssetType::Imagery)
            );
        }
        assert_eq!(detect_asset_type("a.czml"), Some(AssetType::Czml));
    }

    #[test]
    fn an_unknown_extension_is_no_asset_type_at_all() {
        assert_eq!(detect_asset_type("notes.txt"), None);
        assert_eq!(detect_asset_type("no-extension"), None);
    }

    #[test]
    fn a_file_named_zip_with_no_dot_is_neither_a_photo_set_nor_an_upload() {
        assert_eq!(detect_asset_type("zip"), None);
        assert!(!photogrammetry::is_photo_set(Path::new("zip")));
        assert!(photogrammetry::is_photo_set(Path::new("survey.ZIP")));
    }

    #[test]
    fn a_name_that_is_a_path_is_refused() {
        for name in [
            "../evil.glb",
            "../../evil.glb",
            "a/b.glb",
            "..\\evil.glb",
            "/etc/evil.glb",
            "..",
            ".",
            "",
        ] {
            assert!(checked_file_name(name).is_err(), "{name} should be refused");
        }
    }

    #[test]
    fn a_plain_file_name_passes_through_unchanged() {
        for name in ["site.glb", "a..b.glb", "...glb", "café tile.tif"] {
            assert_eq!(checked_file_name(name).unwrap(), name);
        }
    }

    #[test]
    fn mesh_compression_names_parse_and_anything_else_is_refused() {
        assert_eq!(
            parse_mesh_compression("draco").unwrap(),
            MeshCompression::Draco
        );
        assert_eq!(
            parse_mesh_compression("meshopt").unwrap(),
            MeshCompression::Meshopt
        );
        assert_eq!(
            parse_mesh_compression("none").unwrap(),
            MeshCompression::None
        );
        for refused in ["gzip", "Draco", "", " none"] {
            assert_eq!(
                parse_mesh_compression(refused).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn everything_but_imagery_and_czml_tiles_on_upload() {
        assert!(tiles_on_upload(&AssetType::PointCloud));
        assert!(tiles_on_upload(&AssetType::Model));
        assert!(tiles_on_upload(&AssetType::Vector));
        assert!(tiles_on_upload(&AssetType::Terrain));
        assert!(!tiles_on_upload(&AssetType::Imagery));
        assert!(!tiles_on_upload(&AssetType::Czml));
    }
}
