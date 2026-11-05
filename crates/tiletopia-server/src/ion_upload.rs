use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Extension,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tiletopia_core::glb_writer::MeshCompression;
use tiletopia_core::mesh_tiler::TextureCompression;
use tiletopia_ingest::crs_detect::parse_epsg_code;
use tiletopia_ingest::dxf_reader::drawing_unit;
use uuid::Uuid;

use crate::audit::AuditedResource;
use crate::db::{Database, ModelPlacement};
use crate::imagery::{self, ImageryForm, PreparedUpload};
use crate::ion_compat::{IonAsset, IonError, ion_error, resolve_asset, to_ion_asset};
use crate::job_queue::{is_splat_capture, stored_input_path};
use crate::photogrammetry::PhotogrammetryPreset;
use crate::upload::lowercase_extension;
use crate::users::to_hex;
use crate::zip_unpack::unpack_zip;
use crate::{
    AppState, Asset, AssetStatus, AssetType, may_modify_asset, model_versions, upload, users,
};

// s3 sdks address a bucket whose name is no valid hostname path-style
pub const UPLOAD_BUCKET: &str = "tiletopia_ion_uploads";

// cesium ion's upload credentials last 12 hours
const CREDENTIAL_LIFETIME_SECONDS: i64 = 12 * 60 * 60;

// 16 assets at MAX_UPLOAD_BYTES come to 64 GiB staged per editor
pub const MOST_ASSETS_AWAITING_FILES_PER_OWNER: i64 = 16;

// zips name up to 10,000 entries, and each name can run to the 4 KiB of a path
const MOST_NAMED_FILES: usize = 20;

const STAGING_DIR: &str = "ion_upload";
const PARTS_DIR: &str = "ion_upload_parts";
const UNPACK_DIR: &str = "ion_upload_unpacked";

const MESH_OPTIONS: &[&str] = &["position", "geometryCompression", "textureFormat"];
const BIM_CAD_OPTIONS: &[&str] = &[
    "position",
    "geometryCompression",
    "textureFormat",
    BASE_ASSET_OPTION,
    EXISTING_FILE_OPTION,
];
const BASE_ASSET_OPTION: &str = "baseAssetId";
const EXISTING_FILE_OPTION: &str = "existingFileBehavior";

struct IonSource {
    source_type: &'static str,
    ion_type: &'static str,
    asset_type: AssetType,
    extensions: &'static [&'static str],
    options: &'static [&'static str],
}

static ION_SOURCES: [IonSource; 10] = [
    IonSource {
        source_type: "POINT_CLOUD",
        ion_type: "3DTILES",
        asset_type: AssetType::PointCloud,
        extensions: &["las", "laz", "e57", "ply"],
        options: &["inputCrs", "position"],
    },
    IonSource {
        source_type: "3D_MODEL",
        ion_type: "3DTILES",
        asset_type: AssetType::Model,
        extensions: &["gltf", "glb", "obj", "fbx", "dae"],
        options: MESH_OPTIONS,
    },
    IonSource {
        source_type: "3D_CAPTURE",
        ion_type: "3DTILES",
        asset_type: AssetType::Model,
        extensions: &["gltf", "glb", "obj", "dae"],
        options: MESH_OPTIONS,
    },
    IonSource {
        source_type: "BIM_CAD",
        ion_type: "3DTILES",
        asset_type: AssetType::Model,
        extensions: &["ifc", "gltf", "glb", "obj", "dxf"],
        options: BIM_CAD_OPTIONS,
    },
    IonSource {
        source_type: "CITYGML",
        ion_type: "3DTILES",
        asset_type: AssetType::Model,
        extensions: &["gml"],
        options: MESH_OPTIONS,
    },
    IonSource {
        source_type: "KML",
        ion_type: "3DTILES",
        asset_type: AssetType::Vector,
        extensions: &["kml"],
        options: &[],
    },
    IonSource {
        source_type: "VECTOR_DATA",
        ion_type: "3DTILES",
        asset_type: AssetType::Vector,
        extensions: &["geojson", "topojson", "gpkg", "kml"],
        options: &[],
    },
    // ion's name for VECTOR_DATA before 2026-08-31
    IonSource {
        source_type: "GEOJSON",
        ion_type: "3DTILES",
        asset_type: AssetType::Vector,
        extensions: &["geojson"],
        options: &[],
    },
    IonSource {
        source_type: "RASTER_IMAGERY",
        ion_type: "IMAGERY",
        asset_type: AssetType::Imagery,
        // jpeg, png and j2k carry no georeference and ion's options have no way to send one
        extensions: &["tif", "tiff", "jp2"],
        options: &[],
    },
    IonSource {
        source_type: "RASTER_TERRAIN",
        ion_type: "TERRAIN",
        asset_type: AssetType::Terrain,
        extensions: &["tif", "tiff", "hgt", "dt0", "dt1", "dt2"],
        options: &[],
    },
];

const ACCEPTED_TYPES: &str = "3DTILES, IMAGERY and TERRAIN";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IonTiling {
    pub source_type: String,
    pub placement: ModelPlacement,
    pub mesh_compression: MeshCompression,
    pub texture_compression: TextureCompression,
}

#[derive(Debug, Clone)]
pub struct IonUpload {
    pub asset_id: Uuid,
    pub access_key: String,
    pub secret_access_key: String,
    pub session_token: String,
    pub expires_at: DateTime<Utc>,
    pub tiling: IonTiling,
}

pub(crate) fn upload_prefix(asset_id: Uuid) -> String {
    format!("sources/{asset_id}/")
}

pub(crate) fn staging_dir(data_dir: &Path, asset_id: Uuid) -> PathBuf {
    data_dir.join(asset_id.to_string()).join(STAGING_DIR)
}

pub(crate) fn parts_dir(data_dir: &Path, asset_id: Uuid) -> PathBuf {
    data_dir.join(asset_id.to_string()).join(PARTS_DIR)
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateIonAssetRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "type")]
    asset_type: Option<String>,
    options: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    attribution: String,
    from: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IonSourceOptions {
    position: Option<[f64; 3]>,
    input_crs: Option<String>,
    geometry_compression: Option<String>,
    texture_format: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadLocation {
    endpoint: String,
    bucket: &'static str,
    prefix: String,
    access_key: String,
    secret_access_key: String,
    session_token: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct OnComplete {
    method: &'static str,
    url: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreatedIonAsset {
    asset_metadata: IonAsset,
    upload_location: UploadLocation,
    on_complete: OnComplete,
}

fn bad_request(message: impl Into<String>) -> IonError {
    ion_error(StatusCode::BAD_REQUEST, message)
}

fn accepted_sources() -> String {
    ION_SOURCES
        .iter()
        .map(|source| format!("{} for {}", source.source_type, source.ion_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn requested_source(
    request: &CreateIonAssetRequest,
) -> Result<(&'static IonSource, IonTiling), IonError> {
    let ion_type = match request.asset_type.as_deref() {
        Some(ion_type @ ("3DTILES" | "IMAGERY" | "TERRAIN")) => ion_type,
        Some(other) => {
            return Err(bad_request(format!(
                "type {other}: accepted types are {ACCEPTED_TYPES}"
            )));
        }
        None => {
            return Err(bad_request(format!(
                "type is required: accepted types are {ACCEPTED_TYPES}"
            )));
        }
    };

    if request.from.is_some() {
        return Err(bad_request(
            "from: this server imports nothing, so send the source file to the uploadLocation of a request without from",
        ));
    }
    if !request.attribution.is_empty() {
        return Err(bad_request(
            "attribution: this server stores none, so a value that is not empty is refused",
        ));
    }

    let mut options = request.options.clone().unwrap_or_default();
    let source_type = match options.remove("sourceType") {
        Some(serde_json::Value::String(source_type)) => source_type,
        _ => {
            return Err(bad_request(format!(
                "options.sourceType is required: accepted values are {}",
                accepted_sources()
            )));
        }
    };
    let source = ION_SOURCES
        .iter()
        .find(|source| source.source_type == source_type)
        .ok_or_else(|| {
            bad_request(format!(
                "sourceType {source_type}: accepted values are {}",
                accepted_sources()
            ))
        })?;
    if source.ion_type != ion_type {
        return Err(bad_request(format!(
            "sourceType {source_type} makes a {} asset, not {ion_type}",
            source.ion_type
        )));
    }

    let unsupported: Vec<&str> = options
        .keys()
        .map(String::as_str)
        .filter(|name| !source.options.iter().any(|option| option == name))
        .collect();
    if !unsupported.is_empty() {
        let applied = match source.options {
            [] => "none".to_string(),
            names => names.join(", "),
        };
        return Err(bad_request(format!(
            "options {} are not applied by this server, so they are refused: the options it applies to {source_type} are {applied}",
            unsupported.join(", ")
        )));
    }

    let options: IonSourceOptions = serde_json::from_value(serde_json::Value::Object(options))
        .map_err(|error| bad_request(format!("options: {error}")))?;
    Ok((source, tiling_settings(source, options)?))
}

fn tiling_settings(source: &IonSource, options: IonSourceOptions) -> Result<IonTiling, IonError> {
    let (longitude, latitude) = match options.position {
        None => (None, None),
        Some([longitude, latitude, height]) => {
            let on_the_globe = (-180.0..=180.0).contains(&longitude)
                && (-90.0..=90.0).contains(&latitude)
                && height.is_finite();
            if !on_the_globe {
                return Err(bad_request(format!(
                    "position [{longitude}, {latitude}, {height}]: expected longitude, latitude and height in EPSG:4326 degrees and metres"
                )));
            }
            if height != 0.0 {
                return Err(bad_request(format!(
                    "position height {height}: models are placed on the ellipsoid, send a height of 0"
                )));
            }
            (Some(longitude), Some(latitude))
        }
    };

    if let Some(crs) = &options.input_crs
        && parse_epsg_code(crs).is_none()
    {
        return Err(bad_request(format!(
            "inputCrs {crs}: expected an EPSG code such as EPSG:32632"
        )));
    }

    // ion compresses with DRACO when geometryCompression is left out
    let default_mesh_compression = if source.options.contains(&"geometryCompression") {
        MeshCompression::Draco
    } else {
        MeshCompression::None
    };
    let mesh_compression = match options.geometry_compression.as_deref() {
        None => default_mesh_compression,
        Some("NONE") => MeshCompression::None,
        Some("DRACO") => MeshCompression::Draco,
        Some("MESHOPT") => MeshCompression::Meshopt,
        Some(other) => {
            return Err(bad_request(format!(
                "geometryCompression {other}: accepted values are NONE, DRACO and MESHOPT"
            )));
        }
    };
    let texture_compression = match options.texture_format.as_deref() {
        None | Some("AUTO") => TextureCompression::None,
        Some("KTX2") => TextureCompression::Ktx2,
        Some(other) => {
            return Err(bad_request(format!(
                "textureFormat {other}: accepted values are AUTO and KTX2"
            )));
        }
    };

    Ok(IonTiling {
        source_type: source.source_type.to_string(),
        placement: ModelPlacement {
            longitude,
            latitude,
            crs: options.input_crs,
            units: None,
        },
        mesh_compression,
        texture_compression,
    })
}

fn random_hex(byte_count: usize) -> String {
    use rand::TryRngCore;

    let mut bytes = vec![0u8; byte_count];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random source is readable");
    to_hex(&bytes)
}

fn requested_base_ion_id(request: &CreateIonAssetRequest) -> Result<Option<i64>, IonError> {
    let Some(options) = &request.options else {
        return Ok(None);
    };
    let Some(base_asset_id) = options.get(BASE_ASSET_OPTION) else {
        return match options.get(EXISTING_FILE_OPTION) {
            Some(_) => Err(bad_request(format!(
                "{EXISTING_FILE_OPTION} applies only alongside {BASE_ASSET_OPTION}"
            ))),
            None => Ok(None),
        };
    };
    let base_ion_id = base_asset_id
        .as_i64()
        .ok_or_else(|| bad_request(format!("{BASE_ASSET_OPTION} takes an Ion asset id")))?;
    // every version keeps its own files on disk, which is ion's only accepted value
    if options
        .get(EXISTING_FILE_OPTION)
        .and_then(|value| value.as_str())
        != Some("KEEP")
    {
        return Err(bad_request(format!(
            "{EXISTING_FILE_OPTION} is required alongside {BASE_ASSET_OPTION} and must be KEEP"
        )));
    }
    Ok(Some(base_ion_id))
}

pub(crate) async fn create_asset(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CreateIonAssetRequest>,
) -> Result<
    (
        StatusCode,
        Extension<AuditedResource>,
        Json<CreatedIonAsset>,
    ),
    IonError,
> {
    // the route sits behind require_editor, so a valid token is always present
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| ion_error(status, "no valid token"))?;

    let (source, tiling) = requested_source(&request)?;
    let base_asset = match requested_base_ion_id(&request)? {
        Some(base_ion_id) => Some(
            model_versions::checked_base_asset(&state, &claims, &base_ion_id.to_string())
                .await
                .map_err(|(status, message)| ion_error(status, message))?,
        ),
        None => None,
    };

    let asset = Asset {
        id: Uuid::new_v4(),
        name: request.name,
        asset_type: source.asset_type.clone(),
        status: AssetStatus::Uploading,
        created_at: Utc::now(),
        tile_count: 0,
        size_bytes: 0,
        description: request.description,
        tags: Vec::new(),
        owner_id: model_versions::owner_of_upload(base_asset.as_ref(), claims.sub),
    };
    let ion_id = state.db.create_asset(&asset).await.map_err(|_| {
        ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store the asset",
        )
    })?;
    if let Some(base_asset) = &base_asset
        && let Err(error) = model_versions::add_version(&state.db, asset.id, base_asset.id).await
    {
        tracing::error!("recording asset {} as a version failed: {error}", asset.id);
        if let Err(error) = state.db.delete_asset(asset.id).await {
            tracing::error!("removing asset {} failed: {error}", asset.id);
        }
        return Err(ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not record the new version",
        ));
    }

    // AWS access key ids are 20 upper case characters and secret keys 40
    let upload = IonUpload {
        asset_id: asset.id,
        access_key: format!("TT{}", random_hex(9).to_uppercase()),
        secret_access_key: random_hex(20),
        session_token: random_hex(32),
        expires_at: asset.created_at + chrono::Duration::seconds(CREDENTIAL_LIFETIME_SECONDS),
        tiling,
    };
    let refusal = match state.db.create_ion_upload(&upload).await {
        Ok(true) => None,
        Ok(false) => Some(ion_error(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "one editor has {MOST_ASSETS_AWAITING_FILES_PER_OWNER} assets awaiting files at most: call uploadComplete on one or wait for it to expire"
            ),
        )),
        Err(error) => {
            tracing::error!(
                "storing the upload credentials of asset {} failed: {error}",
                asset.id
            );
            Some(ion_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store the upload credentials",
            ))
        }
    };
    if let Some(refusal) = refusal {
        if let Err(error) = state.db.delete_asset(asset.id).await {
            tracing::error!(
                "removing asset {} without credentials failed: {error}",
                asset.id
            );
        }
        return Err(refusal);
    }

    let base_url = &state.ion_base_url;
    // ion's openapi spec answers this call with 200
    Ok((
        StatusCode::OK,
        Extension(AuditedResource(asset.id.to_string())),
        Json(CreatedIonAsset {
            asset_metadata: to_ion_asset(&asset, ion_id, true, None),
            upload_location: UploadLocation {
                endpoint: base_url.clone(),
                bucket: UPLOAD_BUCKET,
                prefix: upload_prefix(asset.id),
                access_key: upload.access_key,
                secret_access_key: upload.secret_access_key,
                session_token: upload.session_token,
            },
            on_complete: OnComplete {
                method: "POST",
                url: format!("{base_url}/v1/assets/{ion_id}/uploadComplete"),
                fields: serde_json::Map::new(),
            },
        }),
    ))
}

async fn staged_files(staging: &Path) -> Result<Vec<PathBuf>, IonError> {
    let read_failed = |error: std::io::Error| {
        tracing::error!("reading {} failed: {error}", staging.display());
        ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read the uploaded files",
        )
    };
    let mut entries = match tokio::fs::read_dir(staging).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_failed(error)),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next_entry().await.map_err(read_failed)? {
        files.push(entry.path());
    }
    files.sort();
    Ok(files)
}

pub(crate) async fn remove_dir_if_present(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("removing {} failed: {error}", path.display())),
    }
}

async fn fail_upload(state: &AppState, mut asset: Asset, answer: IonError) -> IonError {
    asset.status = AssetStatus::Error;
    if let Err(error) = state.db.update_asset(&asset).await {
        tracing::error!("setting asset {} to error failed: {error}", asset.id);
    }
    if let Err(error) = remove_dir_if_present(&state.data_dir.join(asset.id.to_string())).await {
        tracing::error!("{error}");
    }
    answer
}

fn named_files<P: AsRef<Path>>(paths: &[P]) -> String {
    let mut names: Vec<String> = paths
        .iter()
        .take(MOST_NAMED_FILES)
        .map(|path| path.as_ref().display().to_string())
        .collect();
    if paths.len() > MOST_NAMED_FILES {
        names.push(format!("and {} more", paths.len() - MOST_NAMED_FILES));
    }
    names.join(", ")
}

struct SourceFiles {
    directory: PathBuf,
    relative_paths: Vec<PathBuf>,
    total_bytes: u64,
}

fn is_zip_archive(path: &Path) -> bool {
    // ifc's ifcZIP container is a plain zip around one .ifc
    matches!(lowercase_extension(path).as_str(), "zip" | "ifczip")
}

async fn gather_source_files(
    staged: &[PathBuf],
    staging: &Path,
    unpack_dir: &Path,
) -> Result<SourceFiles, IonError> {
    if let [staged_file] = staged
        && is_zip_archive(staged_file)
    {
        let zip_name = staged_file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let zip_path = staged_file.clone();
        let destination = unpack_dir.to_path_buf();
        let unpacked = tokio::task::spawn_blocking(move || unpack_zip(&zip_path, &destination))
            .await
            .expect("zip unpacking task panicked")
            .map_err(|reason| bad_request(format!("{zip_name}: {reason}")))?;
        return Ok(SourceFiles {
            directory: unpack_dir.to_path_buf(),
            relative_paths: unpacked.relative_paths,
            total_bytes: unpacked.total_bytes,
        });
    }

    let relative_paths: Vec<PathBuf> = staged
        .iter()
        .filter_map(|path| path.file_name())
        .map(PathBuf::from)
        .collect();
    if staged.iter().any(|path| is_zip_archive(path)) {
        return Err(bad_request(format!(
            "{} files were uploaded ({}): a zip must be the only file of its asset",
            relative_paths.len(),
            named_files(&relative_paths)
        )));
    }
    let mut total_bytes = 0;
    for path in staged {
        let metadata = tokio::fs::metadata(path).await.map_err(|error| {
            tracing::error!("reading {} failed: {error}", path.display());
            ion_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the uploaded files",
            )
        })?;
        total_bytes += metadata.len();
    }
    Ok(SourceFiles {
        directory: staging.to_path_buf(),
        relative_paths,
        total_bytes,
    })
}

fn main_source_file<'a>(
    source: &IonSource,
    relative_paths: &'a [PathBuf],
) -> Result<&'a Path, String> {
    let mut candidates: Vec<&Path> = relative_paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| {
            let extension = lowercase_extension(path);
            source
                .extensions
                .iter()
                .any(|accepted| *accepted == extension)
        })
        .collect();
    candidates.sort();
    let accepted = source.extensions.join(", ");
    match candidates[..] {
        [main_file] => Ok(main_file),
        [] => Err(format!(
            "{}: {} takes {accepted}",
            named_files(relative_paths),
            source.source_type
        )),
        _ => Err(format!(
            "{} source files were uploaded ({}): this server tiles one source file per asset, and {} takes {accepted}",
            candidates.len(),
            named_files(&candidates),
            source.source_type
        )),
    }
}

fn check_main_file(
    source: &IonSource,
    main_path: &Path,
    placement: &ModelPlacement,
) -> Result<(), String> {
    if lowercase_extension(main_path) == "dxf" {
        drawing_unit(main_path, None).map_err(|error| {
            format!(
                "{error}. The units field is on POST /api/v1/assets, and ion's BIM_CAD options have none"
            )
        })?;
    }
    if source.asset_type != AssetType::PointCloud {
        return Ok(());
    }

    let is_splat = is_splat_capture(main_path)?;
    let has_position = placement.longitude.is_some();
    if is_splat && !has_position {
        return Err(
            "a Gaussian splat PLY has no georeference, so POINT_CLOUD needs options.position for one"
                .to_string(),
        );
    }
    if !is_splat && has_position {
        return Err(
            "options.position places Gaussian splat PLY files, and a point cloud is placed by its CRS"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) async fn upload_complete(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, IonError> {
    let claims = users::claims_from_headers(&headers)
        .map_err(|status| ion_error(status, "no valid token"))?;
    let (asset, _) = resolve_asset(&state, &id)
        .await
        .map_err(|_| ion_error(StatusCode::INTERNAL_SERVER_ERROR, "asset lookup failed"))?
        .filter(|(asset, _)| may_modify_asset(&claims, asset.owner_id.as_deref()))
        .ok_or_else(|| ion_error(StatusCode::NOT_FOUND, format!("no asset {id}")))?;

    let not_awaiting_files = || {
        ion_error(
            StatusCode::CONFLICT,
            format!(
                "asset {id} is not awaiting files, its status is {:?}",
                asset.status
            ),
        )
    };
    let awaiting = state
        .db
        .asset_ids_awaiting_files()
        .await
        .map_err(|_| ion_error(StatusCode::INTERNAL_SERVER_ERROR, "upload lookup failed"))?;
    if !awaiting.contains(&asset.id) {
        return Err(not_awaiting_files());
    }

    let staged = staged_files(&staging_dir(&state.data_dir, asset.id)).await?;
    if staged.is_empty() {
        return Err(bad_request(format!(
            "no file has been uploaded to s3://{UPLOAD_BUCKET}/{}: put the source file there, then call uploadComplete",
            upload_prefix(asset.id)
        )));
    }

    let upload = state
        .db
        .take_ion_upload(asset.id)
        .await
        .map_err(|_| ion_error(StatusCode::INTERNAL_SERVER_ERROR, "upload lookup failed"))?
        .ok_or_else(not_awaiting_files)?;
    let Some(source) = ION_SOURCES
        .iter()
        .find(|source| source.source_type == upload.tiling.source_type)
    else {
        let answer = ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "sourceType {} is no longer taken by this server",
                upload.tiling.source_type
            ),
        );
        return Err(fail_upload(&state, asset, answer).await);
    };

    let asset_dir = state.data_dir.join(asset.id.to_string());
    let staging = staging_dir(&state.data_dir, asset.id);
    let unpack_dir = asset_dir.join(UNPACK_DIR);
    let sources = match gather_source_files(&staged, &staging, &unpack_dir).await {
        Ok(sources) => sources,
        Err(answer) => return Err(fail_upload(&state, asset, answer).await),
    };
    let main_file = match main_source_file(source, &sources.relative_paths) {
        Ok(main_file) => main_file,
        Err(reason) => {
            let answer = bad_request(reason);
            return Err(fail_upload(&state, asset, answer).await);
        }
    };
    let file_name = main_file.display().to_string();
    let extension = lowercase_extension(main_file);

    // the mesh readers refuse a sidecar path that climbs out of the main file's directory
    let main_path = sources.directory.join(main_file);
    let main_dir = main_path
        .parent()
        .expect("a main file has an extension, so it has a parent");
    let main_name = main_path
        .file_name()
        .expect("a main file has an extension, so it has a file name");
    let input_path = asset_dir.join("input").join(main_name);

    let checked_path = main_path.clone();
    let checked_placement = upload.tiling.placement.clone();
    let checked = tokio::task::spawn_blocking(move || {
        check_main_file(source, &checked_path, &checked_placement)
    })
    .await
    .expect("main file check task panicked");
    if let Err(reason) = checked {
        let answer = bad_request(format!("{file_name}: {reason}"));
        return Err(fail_upload(&state, asset, answer).await);
    }

    if let Err(error) = tokio::fs::rename(main_dir, asset_dir.join("input")).await {
        tracing::error!("moving {} failed: {error}", main_dir.display());
        let answer = ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{file_name}: could not be moved"),
        );
        return Err(fail_upload(&state, asset, answer).await);
    }
    for dir in [staging, unpack_dir, parts_dir(&state.data_dir, asset.id)] {
        if let Err(error) = remove_dir_if_present(&dir).await {
            tracing::warn!("{error}");
        }
    }

    let imagery_form = ImageryForm {
        asset_type: imagery::takes_imagery_form(&extension).then(|| {
            match source.asset_type {
                AssetType::Imagery => "imagery",
                _ => "terrain",
            }
            .to_string()
        }),
        ..ImageryForm::default()
    };
    let prepared = upload::prepare_input(
        &state,
        &input_path,
        &asset_dir,
        source.asset_type.clone(),
        imagery_form,
    )
    .await;
    let PreparedUpload {
        asset_type,
        tiles_ready,
    } = match prepared {
        Ok(prepared) => prepared,
        Err(reason) => {
            let answer = bad_request(format!("{file_name}: {reason}"));
            return Err(fail_upload(&state, asset, answer).await);
        }
    };

    let mut asset = asset;
    asset.asset_type = asset_type;
    asset.size_bytes = sources.total_bytes;
    asset.status = if tiles_ready {
        AssetStatus::Ready
    } else {
        AssetStatus::Uploading
    };
    if state.db.update_asset(&asset).await.is_err() {
        let answer = ion_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not store the asset",
        );
        return Err(fail_upload(&state, asset, answer).await);
    }

    if upload::tiles_on_upload(&asset.asset_type) {
        let tiling = upload.tiling;
        let submitted = state
            .job_queue
            .submit(
                asset.id,
                stored_input_path(main_name),
                tiling.placement,
                tiling.mesh_compression,
                tiling.texture_compression,
                PhotogrammetryPreset::default(),
            )
            .await;
        if let Err(error) = submitted {
            tracing::error!(
                "queueing the tiling job of asset {} failed: {error}",
                asset.id
            );
            let answer = ion_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not queue the tiling job",
            );
            return Err(fail_upload(&state, asset, answer).await);
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn expire_abandoned_uploads(
    db: &Database,
    data_dir: &Path,
    now: DateTime<Utc>,
) -> Result<usize, String> {
    let expired = db
        .take_expired_ion_uploads(now)
        .await
        .map_err(|error| format!("reading the expired ion uploads failed: {error}"))?;
    let mut failures = Vec::new();
    for asset_id in &expired {
        let set_to_error = async {
            let Some(mut asset) = db.get_asset(*asset_id).await? else {
                return Ok(());
            };
            asset.status = AssetStatus::Error;
            db.update_asset(&asset).await
        }
        .await;
        if let Err(error) = set_to_error {
            failures.push(format!("setting asset {asset_id} to error failed: {error}"));
        }
        if let Err(error) = remove_dir_if_present(&data_dir.join(asset_id.to_string())).await {
            failures.push(error);
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }
    Ok(expired.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_named(source_type: &str) -> &'static IonSource {
        ION_SOURCES
            .iter()
            .find(|source| source.source_type == source_type)
            .unwrap()
    }

    #[test]
    fn source_types_take_jp2_dxf_and_topojson_as_main_files() {
        for (source_type, file) in [
            ("RASTER_IMAGERY", "ortho.jp2"),
            ("BIM_CAD", "plan.dxf"),
            ("VECTOR_DATA", "roads.topojson"),
        ] {
            let paths = [PathBuf::from(file)];
            assert_eq!(
                main_source_file(source_named(source_type), &paths).unwrap(),
                Path::new(file)
            );
        }
    }

    #[test]
    fn a_j2k_is_refused_naming_the_file_and_what_raster_imagery_takes() {
        let paths = [PathBuf::from("ortho.j2k")];

        let error = main_source_file(source_named("RASTER_IMAGERY"), &paths).unwrap_err();

        assert!(error.contains("ortho.j2k"), "{error}");
        assert!(error.contains("jp2"), "{error}");
    }
}
