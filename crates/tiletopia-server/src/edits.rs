use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::Json,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tiletopia_ingest::MeshData;
use tiletopia_ingest::ifc_edit::{
    EditError, add_proxy, change_relation, delete_product, mesh_volume, replace_mesh,
    translate_placement, yaw_placement,
};
use tiletopia_ingest::ifc_elements::element_parent;
use tiletopia_ingest::ifc_reader::map_conversion_placement;
use tokio::sync::{Mutex, OwnedMutexGuard};
use uuid::Uuid;

use crate::db::JobRecord;
use crate::dxf_export::{mesh_source, read_meshes_in_tileset_frame};
use crate::element_store::{
    PropertyStoreError, StoredElementId, delete_stored_element, insert_stored_proxy, open_store,
    store_parent, stored_element_id,
};
use crate::elements::{
    element_error, invalid_parameter, no_element_store, permitted_asset, server_failure,
};
use crate::{AppState, may_modify_asset, may_view_asset, users};

const MAX_PREVIEW_VERTICES: usize = 200_000;
pub(crate) const EDITS_FILE: &str = "element-edits.json";

static ASSET_WRITE_LOCKS: Mutex<BTreeMap<Uuid, Arc<Mutex<()>>>> = Mutex::const_new(BTreeMap::new());

pub async fn lock_asset_writes(asset_id: Uuid) -> OwnedMutexGuard<()> {
    let lock = {
        let mut locks = ASSET_WRITE_LOCKS.lock().await;
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(asset_id).or_default())
    };
    lock.lock_owned().await
}

pub(crate) async fn lock_asset_for_edit(
    state: &AppState,
    asset_id: Uuid,
    id: &str,
) -> Result<(OwnedMutexGuard<()>, crate::Asset), crate::ion_compat::IonError> {
    let writes = lock_asset_writes(asset_id).await;
    // run_job marks an asset tiling under this lock, so a status read before it can be stale
    let asset = state
        .db
        .get_asset(asset_id)
        .await
        .map_err(|error| server_failure("the asset could not be read", error))?
        .ok_or_else(|| {
            element_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                format!("no asset {id}"),
            )
        })?;
    Ok((writes, asset))
}

pub fn edit_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/assets/{id}/element-edits", get(get_edits))
        .route(
            "/v1/assets/{id}/elements/{element_id}/volume",
            get(get_volume),
        )
        .route(
            "/v1/assets/{id}/edits",
            post(post_edit).layer(middleware::from_fn(users::require_editor)),
        )
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum EditBody {
    Translate {
        #[serde(rename = "elementId")]
        element_id: String,
        delta: [f64; 3],
    },
    Yaw {
        #[serde(rename = "elementId")]
        element_id: String,
        radians: f64,
    },
    Delete {
        #[serde(rename = "elementId")]
        element_id: String,
    },
    Add {
        origin: [f64; 3],
        size: [f64; 3],
    },
    Replace {
        #[serde(rename = "elementId")]
        element_id: String,
        positions: Vec<f64>,
        indices: Vec<u32>,
    },
    Relation {
        #[serde(rename = "elementId")]
        element_id: String,
        #[serde(rename = "relationId")]
        relation_id: u32,
        add: bool,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EditResponse {
    element_id: String,
    drawn: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredEdit {
    element_id: String,
    kind: String,
    hide_original: bool,
    positions: Vec<f64>,
    indices: Vec<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct EditFile {
    edits: Vec<StoredEdit>,
}

#[derive(Debug, Serialize)]
struct VolumeBody {
    volume: f64,
}

#[derive(Clone)]
struct Preview {
    positions: Vec<f64>,
    indices: Vec<u32>,
}

struct Applied {
    bytes: Vec<u8>,
    element_id: String,
    express_id: Option<i64>,
    kind: &'static str,
    hide_original: bool,
    read_back: bool,
    fallback: Option<Preview>,
    store: StoreChange,
}

enum StoreChange {
    None,
    Delete,
    Insert,
    Parent(Option<i64>),
}

async fn get_edits(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<EditFile>, crate::ion_compat::IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let path = state.data_dir.join(asset.id.to_string()).join(EDITS_FILE);
    let bytes = read_if_present(&path)
        .await
        .map_err(|error| server_failure("the edit previews could not be read", error))?;
    parsed_edit_file(bytes.as_deref())
        .map(Json)
        .map_err(|error| server_failure("the edit previews could not be read", error))
}

async fn read_if_present(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parsed_edit_file(bytes: Option<&[u8]>) -> serde_json::Result<EditFile> {
    match bytes {
        Some(bytes) => serde_json::from_slice(bytes),
        None => Ok(EditFile::default()),
    }
}

async fn get_volume(
    State(state): State<Arc<AppState>>,
    RoutePath((id, element_id)): RoutePath<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<VolumeBody>, crate::ion_compat::IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_view_asset).await?;
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(not_ifc(&id, &source.extension));
    }
    let job = source.job;
    let build_slot = state.elevation_store.build_slot().await;
    let asked_for = element_id.clone();
    let (positions, indices) = tokio::task::spawn_blocking(move || {
        let _build_slot = build_slot;
        element_triangles(&job, &asked_for)
    })
    .await
    .map_err(|error| server_failure("the volume could not be read", error))?
    .map_err(|reason| server_failure("the volume could not be read", reason))?;
    if positions.is_empty() {
        return Err(element_error(
            StatusCode::NOT_FOUND,
            "ELEMENT_HAS_NO_MESH",
            format!("asset {id} has no mesh for element {element_id}"),
        ));
    }
    let volume = mesh_volume(&points_of(&positions), &indices)
        .map_err(|error| server_failure("the volume could not be read", format!("{error:?}")))?;
    Ok(Json(VolumeBody { volume }))
}

async fn post_edit(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(body): Json<EditBody>,
) -> Result<Json<EditResponse>, crate::ion_compat::IonError> {
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    let (_writes, asset) = lock_asset_for_edit(&state, asset.id, &id).await?;
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(not_ifc(&id, &source.extension));
    }
    let asset_dir = state.data_dir.join(asset.id.to_string());
    let identity = load_identity(&asset_dir, &id, &asset.status, body.element_id()).await?;
    let edits_path = asset_dir.join(EDITS_FILE);
    let previous_edits = read_if_present(&edits_path)
        .await
        .map_err(|error| server_failure("the edit previews could not be read", error))?;
    let edit_file = parsed_edit_file(previous_edits.as_deref())
        .map_err(|error| server_failure("the edit previews could not be read", error))?;
    let path = PathBuf::from(&source.job.input_path);
    let job = source.job.clone();
    let source_path = path.clone();
    let _build_slot = state.elevation_store.build_slot().await;
    let (previous, applied) = tokio::task::spawn_blocking(move || {
        let previous = std::fs::read(&source_path).map_err(EditFailure::Io)?;
        let applied = apply_edit(
            &previous,
            &body,
            identity.as_ref().map(|item| item.express_id),
        )?;
        Ok::<_, EditFailure>((previous, applied))
    })
    .await
    .map_err(|error| server_failure("the edit could not be written", error))?
    .map_err(|error| refused(&id, &error))?;

    replace_file(&path, &applied.bytes)
        .await
        .map_err(|error| server_failure("the edit could not be written", error))?;
    let preview = match read_preview(&job, &previous, &applied).await {
        Ok(preview) => preview,
        Err(error) => {
            restore_file(&path, Some(&previous)).await;
            return Err(server_failure("the edit could not be drawn", error));
        }
    };
    let drawn = preview
        .as_ref()
        .is_some_and(|item| !item.positions.is_empty());
    if let Some(record) = stored_record(&applied, preview.as_ref(), drawn)
        && let Err(error) = write_edit_file(&edits_path, with_edit(edit_file, record)).await
    {
        restore_file(&path, Some(&previous)).await;
        return Err(server_failure("the edit preview could not be saved", error));
    }
    if let Err(error) = apply_store(&asset_dir, &applied).await {
        restore_file(&path, Some(&previous)).await;
        restore_file(&edits_path, previous_edits.as_deref()).await;
        return Err(error);
    }
    Ok(Json(EditResponse {
        element_id: applied.element_id,
        drawn,
    }))
}

async fn load_identity(
    asset_dir: &Path,
    asset_id: &str,
    status: &crate::AssetStatus,
    element_id: Option<&str>,
) -> Result<Option<StoredElementId>, crate::ion_compat::IonError> {
    let Some(element_id) = element_id else {
        return Ok(None);
    };
    let mut connection = open_store(asset_dir)
        .await
        .map_err(|error| server_failure("the element store could not be read", error))?
        .ok_or_else(|| no_element_store(asset_id, status))?;
    stored_element_id(&mut connection, element_id)
        .await
        .map_err(|error| server_failure("the element store could not be read", error))?
        .map(Some)
        .ok_or_else(|| missing_element(asset_id, element_id))
}

async fn read_preview(
    job: &JobRecord,
    previous: &[u8],
    applied: &Applied,
) -> Result<Option<Preview>, String> {
    if !applied.read_back {
        return Ok(applied.fallback.clone());
    }
    let job = job.clone();
    let element_id = applied.element_id.clone();
    let fallback = applied.fallback.clone();
    let previous = previous.to_vec();
    tokio::task::spawn_blocking(move || {
        let (positions, indices) = element_triangles(&job, &element_id)?;
        if !positions.is_empty() {
            return Ok(capped_preview(positions, indices));
        }
        let local = map_conversion_placement(&previous)
            .map_err(|error| error.to_string())?
            .is_none();
        Ok(local.then_some(fallback).flatten())
    })
    .await
    .map_err(|error| error.to_string())?
}

fn stored_record(applied: &Applied, preview: Option<&Preview>, drawn: bool) -> Option<StoredEdit> {
    if applied.kind == "relation" {
        return None;
    }
    let hide_original = applied.kind == "delete" || (applied.hide_original && drawn);
    if !hide_original && !drawn {
        return None;
    }
    let preview = preview.cloned().unwrap_or(Preview {
        positions: Vec::new(),
        indices: Vec::new(),
    });
    Some(StoredEdit {
        element_id: applied.element_id.clone(),
        kind: applied.kind.to_string(),
        hide_original,
        positions: if drawn { preview.positions } else { Vec::new() },
        indices: if drawn { preview.indices } else { Vec::new() },
    })
}

async fn apply_store(
    asset_dir: &Path,
    applied: &Applied,
) -> Result<(), crate::ion_compat::IonError> {
    match applied.store {
        StoreChange::None => Ok(()),
        StoreChange::Delete => delete_stored_element(asset_dir, &applied.element_id)
            .await
            .map(|_| ())
            .map_err(stored_edit_failure),
        StoreChange::Insert => {
            let express_id = applied.express_id.ok_or_else(|| {
                server_failure("the added element has no id", "missing express id")
            })?;
            insert_stored_proxy(asset_dir, express_id, &applied.element_id)
                .await
                .map_err(stored_edit_failure)
        }
        StoreChange::Parent(parent) => store_parent(asset_dir, &applied.element_id, parent)
            .await
            .map_err(stored_edit_failure),
    }
}

fn apply_edit(
    source: &[u8],
    body: &EditBody,
    express_id: Option<i64>,
) -> Result<Applied, EditFailure> {
    let express = || -> Result<u32, EditFailure> {
        u32::try_from(express_id.ok_or(EditError::MissingElement)?)
            .map_err(|_| EditFailure::Refused(EditError::MissingElement))
    };
    match body {
        EditBody::Translate { element_id, delta } => {
            let bytes =
                translate_placement(source, express()?, *delta).map_err(EditFailure::Refused)?;
            Ok(geometry(bytes, element_id, "translate"))
        }
        EditBody::Yaw {
            element_id,
            radians,
        } => {
            let bytes =
                yaw_placement(source, express()?, *radians).map_err(EditFailure::Refused)?;
            Ok(geometry(bytes, element_id, "yaw"))
        }
        EditBody::Replace {
            element_id,
            positions,
            indices,
        } => {
            let bytes = replace_mesh(source, express()?, positions, indices)
                .map_err(EditFailure::Refused)?;
            Ok(geometry(bytes, element_id, "replace"))
        }
        EditBody::Delete { element_id } => {
            let bytes = delete_product(source, express()?).map_err(EditFailure::Refused)?;
            Ok(Applied {
                bytes,
                element_id: element_id.clone(),
                express_id,
                kind: "delete",
                hide_original: true,
                read_back: false,
                fallback: Some(Preview {
                    positions: Vec::new(),
                    indices: Vec::new(),
                }),
                store: StoreChange::Delete,
            })
        }
        EditBody::Add { origin, size } => {
            let added = add_proxy(source, *origin, *size).map_err(EditFailure::Refused)?;
            let fallback = box_preview(&added.positions, &added.indices);
            Ok(Applied {
                bytes: added.bytes,
                element_id: added.global_id,
                express_id: Some(i64::from(added.express_id)),
                kind: "add",
                hide_original: false,
                read_back: true,
                fallback: Some(fallback),
                store: StoreChange::Insert,
            })
        }
        EditBody::Relation {
            element_id,
            relation_id,
            add,
        } => {
            let element = express()?;
            let bytes = change_relation(source, *relation_id, element, *add)
                .map_err(EditFailure::Refused)?;
            let parent = element_parent(&bytes, element)
                .map_err(|error| EditFailure::Io(std::io::Error::other(error)))?;
            Ok(Applied {
                bytes,
                element_id: element_id.clone(),
                express_id,
                kind: "relation",
                hide_original: false,
                read_back: false,
                fallback: None,
                store: StoreChange::Parent(parent.map(i64::from)),
            })
        }
    }
}

fn geometry(bytes: Vec<u8>, element_id: &str, kind: &'static str) -> Applied {
    Applied {
        bytes,
        element_id: element_id.to_string(),
        express_id: None,
        kind,
        hide_original: true,
        read_back: true,
        fallback: None,
        store: StoreChange::None,
    }
}

fn box_preview(positions: &[[f64; 3]], indices: &[u32]) -> Preview {
    let mut flat = Vec::with_capacity(positions.len() * 3);
    for point in positions {
        flat.extend(point);
    }
    Preview {
        positions: flat,
        indices: indices.to_vec(),
    }
}

fn element_triangles(job: &JobRecord, element_id: &str) -> Result<(Vec<f64>, Vec<u32>), String> {
    let meshes = read_meshes_in_tileset_frame(job, "ifc")?;
    Ok(triangles_of(&meshes, element_id))
}

fn capped_preview(positions: Vec<f64>, indices: Vec<u32>) -> Option<Preview> {
    (positions.len() / 3 <= MAX_PREVIEW_VERTICES).then_some(Preview { positions, indices })
}

fn triangles_of(meshes: &[MeshData], global_id: &str) -> (Vec<f64>, Vec<u32>) {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for mesh in meshes {
        if mesh.asset_id.as_deref() != Some(global_id) {
            continue;
        }
        let base = (positions.len() / 3) as u32;
        for point in &mesh.positions {
            positions.extend(point);
        }
        indices.extend(mesh.indices.iter().map(|index| base + index));
    }
    (positions, indices)
}

fn points_of(positions: &[f64]) -> Vec<[f64; 3]> {
    positions.as_chunks::<3>().0.to_vec()
}

enum EditFailure {
    Io(std::io::Error),
    Refused(EditError),
}

impl From<EditError> for EditFailure {
    fn from(error: EditError) -> Self {
        EditFailure::Refused(error)
    }
}

fn refused(asset_id: &str, error: &EditFailure) -> crate::ion_compat::IonError {
    match error {
        EditFailure::Io(error) => server_failure("the edit could not be written", error),
        EditFailure::Refused(EditError::MissingElement) => {
            missing_element(asset_id, "that element")
        }
        EditFailure::Refused(EditError::NotAPlacement) => element_error(
            StatusCode::BAD_REQUEST,
            "NOT_A_PLACEMENT",
            "the element has no cartesian placement to move",
        ),
        EditFailure::Refused(EditError::NotARelation) => element_error(
            StatusCode::BAD_REQUEST,
            "NOT_A_RELATION",
            "that express id is not a containment, aggregate, or nest relation",
        ),
        EditFailure::Refused(EditError::TooLarge) => {
            invalid_parameter("the replacement mesh is too large")
        }
        EditFailure::Refused(EditError::InvalidMesh) => {
            invalid_parameter("the mesh indices do not form triangles")
        }
        EditFailure::Refused(EditError::InvalidValue) => {
            invalid_parameter("the numbers have to be finite")
        }
        EditFailure::Refused(EditError::NoStructure) => element_error(
            StatusCode::BAD_REQUEST,
            "NO_STRUCTURE",
            "the IFC has no building or site to contain the element",
        ),
        EditFailure::Refused(EditError::StillReferenced) => element_error(
            StatusCode::CONFLICT,
            "STILL_REFERENCED",
            "an entity that is not a relation refers to the element, so deleting it breaks that entity",
        ),
        EditFailure::Refused(EditError::UnsupportedSchema) => element_error(
            StatusCode::CONFLICT,
            "UNSUPPORTED_SCHEMA",
            "adding or reshaping an element writes tessellated geometry, which needs an IFC4 or later file",
        ),
        EditFailure::Refused(EditError::AlreadyRelated) => element_error(
            StatusCode::CONFLICT,
            "ALREADY_RELATED",
            "the element is already in a relation of that kind, and IFC allows it only one",
        ),
        EditFailure::Refused(EditError::UnusableLengthUnit) => element_error(
            StatusCode::CONFLICT,
            "UNUSABLE_LENGTH_UNIT",
            "the IFC's length unit has no usable conversion factor",
        ),
    }
}

fn with_edit(mut file: EditFile, edit: StoredEdit) -> EditFile {
    if let Some(existing) = file
        .edits
        .iter_mut()
        .find(|item| item.element_id == edit.element_id)
    {
        *existing = edit;
    } else {
        file.edits.push(edit);
    }
    file
}

async fn write_edit_file(path: &Path, file: EditFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    replace_file(path, &bytes).await
}

async fn restore_file(path: &Path, previous: Option<&[u8]>) {
    let restored = match previous {
        Some(bytes) => replace_file(path, bytes).await,
        None => crate::element_store::remove_file_if_present(path).map_err(std::io::Error::other),
    };
    if let Err(error) = restored {
        tracing::error!(
            "{} could not be restored after a failed edit: {error}",
            path.display()
        );
    }
}

pub(crate) async fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let writing = writing_path(path)?;
    let written = match tokio::fs::write(&writing, bytes).await {
        Ok(()) => tokio::fs::rename(&writing, path).await,
        Err(error) => Err(error),
    };
    if written.is_err() {
        let _ = tokio::fs::remove_file(&writing).await;
    }
    written
}

pub(crate) fn writing_path(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the path has no file name",
            )
        })?;
    Ok(path.with_file_name(format!(".{file_name}.{}.writing", Uuid::new_v4())))
}

fn missing_element(asset_id: &str, element_id: &str) -> crate::ion_compat::IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "ELEMENT_NOT_FOUND",
        format!("asset {asset_id} has no element {element_id}"),
    )
}

fn not_ifc(asset_id: &str, extension: &str) -> crate::ion_compat::IonError {
    element_error(
        StatusCode::CONFLICT,
        "SOURCE_IS_NOT_IFC",
        format!("asset {asset_id}'s source is a .{extension}, which cannot be edited as IFC"),
    )
}

fn stored_edit_failure(error: PropertyStoreError) -> crate::ion_compat::IonError {
    match error {
        PropertyStoreError::Missing => element_error(
            StatusCode::NOT_FOUND,
            "ELEMENT_NOT_FOUND",
            "the element is not in the store",
        ),
        PropertyStoreError::Database(error) => {
            server_failure("the element store could not be written", error)
        }
        PropertyStoreError::Io(error) => {
            server_failure("the element store could not be written", error)
        }
    }
}

impl EditBody {
    fn element_id(&self) -> Option<&str> {
        match self {
            EditBody::Add { .. } => None,
            EditBody::Translate { element_id, .. }
            | EditBody::Yaw { element_id, .. }
            | EditBody::Delete { element_id }
            | EditBody::Replace { element_id, .. }
            | EditBody::Relation { element_id, .. } => Some(element_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_box_volume_is_the_absolute_tetrahedron_sum() {
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
        ];
        let indices = vec![
            0, 3, 2, 0, 2, 1, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 3, 7, 6, 3, 6, 2, 0, 4, 7, 0, 7,
            3, 1, 2, 6, 1, 6, 5,
        ];
        assert!((mesh_volume(&positions, &indices).unwrap() - 1.0).abs() < 1.0e-9);
    }

    #[test]
    fn an_edit_the_file_schema_lacks_is_a_conflict() {
        let (status, body) = refused("asset", &EditFailure::Refused(EditError::UnsupportedSchema));
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0["code"], "UNSUPPORTED_SCHEMA");
    }

    #[test]
    fn triangles_keep_only_the_named_element() {
        let mesh = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: "wall".to_string(),
            material: None,
            asset_id: Some("wall".to_string()),
            class: None,
            storey: None,
            element_color: None,
        };
        let (positions, indices) = triangles_of(&[mesh], "wall");
        assert_eq!(positions.len(), 9);
        assert_eq!(indices, vec![0, 1, 2]);
    }

    fn part(id: &str, origin: f64) -> MeshData {
        MeshData {
            positions: vec![
                [origin, 0.0, 0.0],
                [origin + 1.0, 0.0, 0.0],
                [origin, 1.0, 0.0],
            ],
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: id.to_string(),
            material: None,
            asset_id: Some(id.to_string()),
            class: None,
            storey: None,
            element_color: None,
        }
    }

    fn record_of(kind: &'static str, hide_original: bool) -> Applied {
        Applied {
            bytes: Vec::new(),
            element_id: "wall".to_string(),
            express_id: None,
            kind,
            hide_original,
            read_back: true,
            fallback: None,
            store: StoreChange::None,
        }
    }

    #[test]
    fn triangles_join_every_part_of_one_element_and_skip_the_rest() {
        let (positions, indices) = triangles_of(
            &[part("wall", 0.0), part("door", 5.0), part("wall", 2.0)],
            "wall",
        );
        assert_eq!(positions.len(), 18);
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
        assert!((positions[9] - 2.0).abs() < 1.0e-9);
    }

    #[test]
    fn an_element_past_the_vertex_cap_keeps_its_triangles_and_draws_no_preview() {
        let mut first = part("wall", 0.0);
        first.positions = vec![[0.0, 0.0, 0.0]; MAX_PREVIEW_VERTICES];
        let (positions, indices) = triangles_of(&[first], "wall");
        assert!(capped_preview(positions, indices).is_some());

        let mut over = part("wall", 0.0);
        over.positions = vec![[0.0, 0.0, 0.0]; MAX_PREVIEW_VERTICES + 1];
        let (positions, indices) = triangles_of(&[over, part("wall", 1.0)], "wall");
        assert_eq!(positions.len(), (MAX_PREVIEW_VERTICES + 4) * 3);
        assert_eq!(indices.len(), 6);
        assert!(capped_preview(positions, indices).is_none());
    }

    #[test]
    fn two_writes_of_one_file_use_different_temporary_files() {
        let path = Path::new("/data/asset/model.ifc");
        let first = writing_path(path).unwrap();
        let second = writing_path(path).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
    }

    #[test]
    fn a_delete_is_stored_hidden_even_when_it_has_no_mesh() {
        let record = stored_record(&record_of("delete", true), None, false).unwrap();
        assert!(record.hide_original);
        assert!(record.positions.is_empty());
        assert!(record.indices.is_empty());
    }

    #[test]
    fn a_translate_with_no_mesh_is_not_stored_so_the_tiles_stay() {
        assert!(stored_record(&record_of("translate", true), None, false).is_none());
    }

    #[test]
    fn a_drawn_translate_hides_the_tile_copy_and_keeps_the_preview() {
        let preview = Preview {
            positions: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0],
            indices: vec![0, 1, 2],
        };
        let record = stored_record(&record_of("translate", true), Some(&preview), true).unwrap();
        assert!(record.hide_original);
        assert_eq!(record.positions, preview.positions);
        assert_eq!(record.indices, preview.indices);
    }

    #[test]
    fn an_undrawn_add_and_a_relation_write_no_preview() {
        assert!(stored_record(&record_of("add", false), None, false).is_none());
        assert!(stored_record(&record_of("relation", false), None, false).is_none());
    }
}
