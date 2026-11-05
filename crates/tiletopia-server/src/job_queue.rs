//! Async job queue for tiling operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::AssetType;
use crate::db::{Database, JobRecord, JobStatus, ModelPlacement};
use crate::element_store;
use crate::elevation::{DemStore, JOB_BUDGET_BYTES};
use crate::ion_upload::remove_dir_if_present;
use crate::photogrammetry::{
    self, DELETED_ASSET_ERROR, NODE_ODM_URL_ENV, NodeOdm, PhotogrammetryPreset,
};
use crate::terrain_bundle;
use crate::webhooks::{WebhookEvent, WebhookQueue};
use tiletopia_core::glb_writer::MeshCompression;
use tiletopia_core::mesh_tiler::TextureCompression;
use tiletopia_ingest::ifc_reader::{MapConversionPlacement, SitePlacement};
use tiletopia_store::TileStore;

/// 3D Tiles version asked of the external tiler.
const TILES_VERSION: &str = "1.1";

/// How much of the external tiler's stderr a failed job reports.
const STDERR_LINES_IN_ERROR: usize = 20;

/// What a native mesh job says when nothing tells it where the model sits.
const NO_PLACEMENT_ERROR: &str = "the native mesh tiler has no coordinates for this model: \
     upload it with longitude and latitude";

const NO_SPLAT_PLACEMENT_ERROR: &str = "the splat tiler has no coordinates for this capture: \
     upload it with longitude and latitude";

const INTERRUPTED_JOB_ERROR: &str = "the server restarted during this job, tile the asset again";

#[derive(Debug, Clone)]
struct TiledAsset {
    tiles_written: u64,
    dropped_elements: Option<String>,
}

pub struct JobQueue {
    db: Arc<Database>,
    data_dir: PathBuf,
    #[allow(dead_code)]
    store: Arc<dyn TileStore>,
    external_tiler_jar: Option<PathBuf>,
    node_odm: Option<NodeOdm>,
    running_reconstructions: Mutex<HashMap<Uuid, CancellationToken>>,
    tiling_lock: tokio::sync::Mutex<()>,
    webhooks: Arc<WebhookQueue>,
    elevation_store: Arc<DemStore>,
    #[cfg(test)]
    tiler_overlap_probe: Arc<tiler_overlap::Probe>,
}

pub(crate) fn stored_input_path(file_name: &std::ffi::OsStr) -> String {
    Path::new("input")
        .join(file_name)
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn job_with_input_in_asset_dir(job: &JobRecord, asset_dir: &Path) -> JobRecord {
    let mut job_with_input = job.clone();
    // Path::join returns an absolute path unchanged, which is what older job rows hold
    job_with_input.input_path = asset_dir
        .join(&job.input_path)
        .to_string_lossy()
        .into_owned();
    job_with_input
}

fn asset_status_follows_job(asset: &crate::Asset) -> bool {
    !matches!(asset.asset_type, AssetType::Imagery | AssetType::Czml)
        || !matches!(asset.status, crate::AssetStatus::Ready)
}

fn settled_job(
    mut job: JobRecord,
    outcome: Result<TiledAsset, String>,
) -> (JobRecord, WebhookEvent) {
    job.completed_at = Some(chrono::Utc::now());
    match outcome {
        Ok(tiled) => {
            job.status = JobStatus::Done;
            job.progress = 1.0;
            job.tiles_written = tiled.tiles_written;
            job.error = tiled.dropped_elements;
            (job, WebhookEvent::JobCompleted)
        }
        Err(error) => {
            job.status = JobStatus::Failed;
            job.error = Some(error);
            (job, WebhookEvent::JobFailed)
        }
    }
}

fn unrecorded_outcome_error(outcome: Result<TiledAsset, String>, failed_write: &str) -> String {
    match outcome {
        Ok(tiled) => format!(
            "the job wrote {} tiles, but {failed_write}",
            tiled.tiles_written
        ),
        Err(error) => format!("{error}, and {failed_write}"),
    }
}

impl JobQueue {
    pub fn new(
        db: Arc<Database>,
        data_dir: PathBuf,
        store: Arc<dyn TileStore>,
        external_tiler_jar: Option<PathBuf>,
        node_odm: Option<NodeOdm>,
        webhooks: Arc<WebhookQueue>,
        elevation_store: Arc<DemStore>,
    ) -> Self {
        Self {
            db,
            data_dir,
            store,
            external_tiler_jar,
            node_odm,
            running_reconstructions: Mutex::new(HashMap::new()),
            tiling_lock: tokio::sync::Mutex::new(()),
            webhooks,
            elevation_store,
            #[cfg(test)]
            tiler_overlap_probe: Arc::default(),
        }
    }

    pub fn reconstructs_photo_sets(&self) -> bool {
        self.node_odm.is_some()
    }

    pub fn cancel_reconstruction(&self, asset_id: Uuid) {
        if let Some(cancellation) = self.running_reconstructions.lock().unwrap().get(&asset_id) {
            cancellation.cancel();
        }
    }

    async fn run_tiler<Tiled: Send + 'static>(
        &self,
        tiler: impl FnOnce() -> Result<Tiled, String> + Send + 'static,
    ) -> Result<Tiled, String> {
        let _tiling_lock = self.tiling_lock.lock().await;
        #[cfg(test)]
        let probe = Arc::clone(&self.tiler_overlap_probe);
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let _counted_tiler = probe.enter();
            tiler()
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn reconstruct(
        &self,
        job: &JobRecord,
        asset_dir: PathBuf,
        cancellation: CancellationToken,
    ) -> Result<u64, String> {
        let node = self.node_odm.clone().ok_or_else(|| {
            format!(
                "{NODE_ODM_URL_ENV} is not set, so there is no NodeODM to reconstruct the photo set"
            )
        })?;
        // check_photo_set budgets the in-memory photos of for_each_photo with a build slot too
        let photo_transfer_slot = self.elevation_store.build_slot().await;

        let runtime = tokio::runtime::Handle::current();
        let db = Arc::clone(&self.db);
        let download_job = job.clone();
        let job_with_input = job_with_input_in_asset_dir(job, &asset_dir);
        let download_dir = asset_dir.clone();
        let download_cancellation = cancellation.clone();
        tokio::task::spawn_blocking(move || {
            let report_progress = |progress: f64| {
                let mut reported = download_job.clone();
                reported.progress = progress;
                if let Err(error) = runtime.block_on(db.update_job(&reported)) {
                    tracing::warn!("Job {}: progress write failed: {}", download_job.id, error);
                }
            };
            photogrammetry::download_reconstruction(
                &node,
                &job_with_input,
                &download_dir,
                &download_cancellation,
                photo_transfer_slot,
                &report_progress,
            )
        })
        .await
        .map_err(|error| error.to_string())??;

        let job = job.clone();
        self.run_tiler(move || photogrammetry::tile_reconstruction(&job, &asset_dir, &cancellation))
            .await
    }

    pub async fn fail_interrupted_jobs(&self) -> Result<u64, sqlx::Error> {
        let interrupted_jobs = self.db.list_running_jobs().await?;
        let interrupted_count = interrupted_jobs.len() as u64;
        for job in interrupted_jobs {
            let status_follows_job = self
                .db
                .get_asset(job.asset_id)
                .await?
                .is_some_and(|asset| asset_status_follows_job(&asset));
            self.record_outcome(
                job,
                status_follows_job,
                Err(INTERRUPTED_JOB_ERROR.to_string()),
            )
            .await;
        }
        Ok(interrupted_count)
    }

    /// Start the background worker loop.
    pub async fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let reconstructing_assets: Vec<Uuid> = self
                    .running_reconstructions
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .collect();
                match self.db.next_queued_job(&reconstructing_assets).await {
                    Ok(Some(job)) => self.run_job(job).await,
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Failed to poll job queue: {}", e);
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        })
    }

    async fn run_job(self: &Arc<Self>, mut job: JobRecord) {
        job.status = JobStatus::Running;
        job.started_at = Some(chrono::Utc::now());
        if let Err(e) = self.db.update_job(&job).await {
            tracing::error!("Failed to update job {}: {}", job.id, e);
            return;
        }

        let asset_id = job.asset_id;
        let (asset_type, status_follows_job) = match self.db.get_asset(asset_id).await {
            Ok(Some(mut asset)) => {
                let status_follows_job = asset_status_follows_job(&asset);
                if status_follows_job {
                    asset.status = crate::AssetStatus::Tiling;
                    if let Err(error) = self.db.update_asset(&asset).await {
                        let reason = format!(
                            "writing the tiling status of asset {asset_id} failed: {error}"
                        );
                        self.record_outcome(job, status_follows_job, Err(reason))
                            .await;
                        return;
                    }
                }
                (asset.asset_type, status_follows_job)
            }
            Ok(None) => {
                self.finish(job, Err(format!("asset {asset_id} is gone")))
                    .await;
                return;
            }
            Err(error) => {
                self.finish(
                    job,
                    Err(format!("reading asset {asset_id} failed: {error}")),
                )
                .await;
                return;
            }
        };

        let asset_dir = self.data_dir.join(asset_id.to_string());
        if let Err(error) = element_store::remove_store(&asset_dir) {
            self.record_outcome(job, status_follows_job, Err(error))
                .await;
            return;
        }
        let reconstructs_photos = asset_type == AssetType::Model
            && photogrammetry::is_photo_set(Path::new(&job.input_path));
        if reconstructs_photos {
            let cancellation = CancellationToken::new();
            self.running_reconstructions
                .lock()
                .unwrap()
                .insert(asset_id, cancellation.clone());
            let queue = Arc::clone(self);
            tokio::spawn(async move {
                let outcome =
                    queue
                        .reconstruct(&job, asset_dir, cancellation)
                        .await
                        .map(|tiles_written| TiledAsset {
                            tiles_written,
                            dropped_elements: None,
                        });
                queue.record_outcome(job, status_follows_job, outcome).await;
                queue
                    .running_reconstructions
                    .lock()
                    .unwrap()
                    .remove(&asset_id);
            });
            return;
        }

        let bundle_dir = terrain_bundle::asset_bundle_dir(&self.data_dir, asset_id);
        let tiling_job = job_with_input_in_asset_dir(&job, &asset_dir);
        let jar = self.external_tiler_jar.clone();
        let outcome = self
            .run_tiler(move || {
                let runtime = tokio::runtime::Handle::current();
                // ifc-lite-geometry recurses on nested solids and placements, and a stack overflow aborts the server
                let tiled = ifc_lite_processing::run_on_geometry_pool(|| {
                    let _runtime_context = runtime.enter();
                    tile(
                        &asset_type,
                        &tiling_job,
                        &asset_dir,
                        &bundle_dir,
                        jar.as_deref(),
                    )
                })
                .map_err(|error| format!("could not start the tiling threads: {error}"))
                .and_then(|tiled| tiled);
                element_store::finish_build(&asset_dir, tiled.is_ok()).and(tiled)
            })
            .await;
        self.record_outcome(job, status_follows_job, outcome).await;
    }

    async fn record_outcome(
        &self,
        job: JobRecord,
        status_follows_job: bool,
        outcome: Result<TiledAsset, String>,
    ) {
        let asset_id = job.asset_id;
        let asset = match self.db.get_asset(asset_id).await {
            Ok(asset) => asset,
            Err(error) => {
                let read_failure = format!("reading asset {asset_id} failed: {error}");
                let reason = unrecorded_outcome_error(outcome, &read_failure);
                self.finish(job, Err(reason)).await;
                return;
            }
        };
        let Some(asset) = asset else {
            // tilers still writing when the delete route removed these directories recreate them
            let asset_dir = self.data_dir.join(asset_id.to_string());
            let bundle_dir = terrain_bundle::asset_bundle_dir(&self.data_dir, asset_id);
            for dir in [asset_dir, bundle_dir] {
                if let Err(error) = remove_dir_if_present(&dir).await {
                    tracing::error!("Job {}: {error}", job.id);
                }
            }
            self.finish(job, Err(DELETED_ASSET_ERROR.to_string())).await;
            return;
        };

        // the asset write goes first: a client polling the job
        // stops at Done and reads the asset straight after, so
        // an asset still saying Tiling there never corrects
        let outcome = if status_follows_job {
            self.write_asset_outcome(asset, outcome).await
        } else {
            outcome
        };
        if outcome.is_ok()
            && let Err(reason) = crate::model_versions::apply_retention(
                &self.db,
                &self.data_dir,
                &self.webhooks,
                asset_id,
            )
            .await
        {
            tracing::error!("Job {}: {reason}", job.id);
        }
        self.finish(job, outcome).await;
    }

    async fn write_asset_outcome(
        &self,
        mut asset: crate::Asset,
        outcome: Result<TiledAsset, String>,
    ) -> Result<TiledAsset, String> {
        match &outcome {
            Ok(tiled) => {
                asset.status = crate::AssetStatus::Ready;
                asset.tile_count = tiled.tiles_written;
            }
            Err(_) => asset.status = crate::AssetStatus::Error,
        }
        let Err(write_error) = self.db.update_asset(&asset).await else {
            return outcome;
        };
        tracing::error!("Asset {}: status write failed: {write_error}", asset.id);

        asset.status = crate::AssetStatus::Error;
        if let Err(error) = self.db.update_asset(&asset).await {
            tracing::error!(
                "Asset {}: it keeps its old status, writing the error status failed too: {error}",
                asset.id
            );
        }
        let failed_write = format!(
            "writing the status of asset {} failed: {write_error}",
            asset.id
        );
        Err(unrecorded_outcome_error(outcome, &failed_write))
    }

    /// Write the settled job back and tell the subscribers. Every tiling job
    /// ends here, so this is the one place the two job events are emitted from.
    async fn finish(&self, job: JobRecord, outcome: Result<TiledAsset, String>) {
        let (mut settled, mut event) = settled_job(job.clone(), outcome.clone());
        if let Err(write_error) = self.db.update_job(&settled).await {
            tracing::error!("Job {}: writing its outcome failed: {write_error}", job.id);
            let failed_write = format!("writing the job's outcome failed: {write_error}");
            let reason = unrecorded_outcome_error(outcome, &failed_write);
            (settled, event) = settled_job(job, Err(reason));
            if let Err(write_error) = self.db.update_job(&settled).await {
                tracing::error!(
                    "Job {}: it reads as running until the next restart, writing its failure failed too: {write_error}",
                    settled.id
                );
                return;
            }
        }

        match (&settled.status, &settled.error) {
            (JobStatus::Failed, Some(error)) => {
                tracing::error!("Job {} failed: {error}", settled.id)
            }
            _ => tracing::info!(
                "Job {} completed: {} tiles",
                settled.id,
                settled.tiles_written
            ),
        }
        let asset_owner_id = match self.db.get_asset(settled.asset_id).await {
            Ok(asset) => asset.and_then(|asset| asset.owner_id),
            Err(error) => {
                tracing::error!(
                    "Job {}: reading its asset's owner failed: {error}",
                    settled.id
                );
                None
            }
        };
        self.webhooks
            .emit(
                event,
                asset_owner_id.as_deref(),
                serde_json::json!({
                    "job_id": settled.id,
                    "asset_id": settled.asset_id,
                    "completed_at": settled.completed_at,
                    "tiles_written": settled.tiles_written,
                    "error": settled.error,
                }),
            )
            .await;
    }

    /// Submit a new job.
    pub async fn submit(
        &self,
        asset_id: Uuid,
        input_path: String,
        placement: ModelPlacement,
        mesh_compression: MeshCompression,
        texture_compression: TextureCompression,
        photogrammetry_preset: PhotogrammetryPreset,
    ) -> Result<JobRecord, sqlx::Error> {
        let output_format = match self.db.get_asset(asset_id).await? {
            Some(asset) if asset.asset_type == AssetType::Terrain => "quantized-mesh",
            _ => "3dtiles",
        };
        let job = JobRecord {
            id: Uuid::new_v4(),
            asset_id,
            status: JobStatus::Queued,
            progress: 0.0,
            input_path,
            output_format: output_format.to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement,
            mesh_compression,
            texture_compression,
            photogrammetry_preset,
        };
        self.db.create_job(&job).await?;
        Ok(job)
    }

    /// Get job status.
    pub async fn get_status(&self, job_id: Uuid) -> Result<Option<JobRecord>, sqlx::Error> {
        self.db.get_job(job_id).await
    }
}

/// Tile one asset and report how many tiles it wrote. Blocking.
fn tile(
    asset_type: &AssetType,
    job: &JobRecord,
    asset_dir: &Path,
    bundle_dir: &Path,
    external_tiler_jar: Option<&Path>,
) -> Result<TiledAsset, String> {
    let input_path = Path::new(&job.input_path);
    let placement = &job.placement;
    let tiles_written = match asset_type {
        AssetType::PointCloud => tile_point_cloud_upload(input_path, asset_dir, placement)?,
        AssetType::Model | AssetType::Vector => {
            let extension = input_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            match tiler_for(&extension)? {
                Tiler::Native { source_is_z_up } => {
                    return tile_native(job, asset_dir, &extension, source_is_z_up);
                }
                Tiler::External(input_type) => {
                    let input_dir = if extension == "topojson" {
                        let decoded_dir = asset_dir.join("topojson_decoded");
                        std::fs::create_dir_all(&decoded_dir).map_err(|e| {
                            format!("could not create {}: {e}", decoded_dir.display())
                        })?;
                        crate::topojson::write_geojson(
                            input_path,
                            &decoded_dir.join("topology.geojson"),
                        )?;
                        decoded_dir
                    } else {
                        asset_dir.join("input")
                    };
                    run_external_tiler(
                        asset_dir,
                        &input_dir,
                        input_type,
                        placement,
                        external_tiler_jar,
                    )?
                }
            }
        }
        AssetType::Terrain => terrain_bundle::write_dem_bundle(
            input_path,
            &asset_dir.join("terrain_staging"),
            bundle_dir,
            placement.crs.as_deref(),
        )?,
        AssetType::Imagery => return Err("imagery assets are not tiled".to_string()),
        AssetType::Czml => {
            return Err("CZML assets are served as uploaded, not tiled".to_string());
        }
    };
    Ok(TiledAsset {
        tiles_written,
        dropped_elements: None,
    })
}

pub(crate) fn is_splat_capture(input_path: &Path) -> Result<bool, String> {
    let is_ply = input_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ply"));
    Ok(is_ply
        && tiletopia_ingest::splat_ply_reader::is_splat_ply(input_path)
            .map_err(|e| e.to_string())?)
}

fn tile_point_cloud_upload(
    input_path: &Path,
    asset_dir: &Path,
    placement: &ModelPlacement,
) -> Result<u64, String> {
    if is_splat_capture(input_path)? {
        tile_splats(input_path, asset_dir, placement)
    } else {
        tile_point_cloud(input_path, asset_dir, placement.crs.as_deref())
    }
}

fn tile_splats(
    input_path: &Path,
    asset_dir: &Path,
    placement: &ModelPlacement,
) -> Result<u64, String> {
    let (Some(longitude), Some(latitude)) = (placement.longitude, placement.latitude) else {
        return Err(NO_SPLAT_PLACEMENT_ERROR.to_string());
    };
    let cloud = tiletopia_ingest::splat_ply_reader::read(input_path, JOB_BUDGET_BYTES)
        .map_err(|e| e.to_string())?;
    let root_transform = tiletopia_core::spatial::enu_to_ecef_matrix(
        latitude.to_radians(),
        longitude.to_radians(),
        0.0,
    );
    let tiles = tiletopia_core::splat_tiler::tile_splats(&cloud, asset_dir, root_transform)
        .map_err(|e| e.to_string())?;
    Ok(tiles as u64)
}

fn tile_point_cloud(
    input_path: &Path,
    asset_dir: &Path,
    upload_crs: Option<&str>,
) -> Result<u64, String> {
    let fallback_source_epsg = upload_crs
        .map(|crs| {
            tiletopia_ingest::crs_detect::parse_epsg_code(crs).ok_or_else(|| {
                format!("crs {crs}: point clouds take an EPSG code, such as EPSG:32632")
            })
        })
        .transpose()?;
    let points = tiletopia_ingest::read_point_cloud_ecef(input_path, fallback_source_epsg)
        .map_err(|e| e.to_string())?;
    let octree_points: Vec<tiletopia_core::octree::OctreePoint> = points
        .into_iter()
        .map(|p| tiletopia_core::octree::OctreePoint {
            position: [p.x, p.y, p.z],
            color: [p.r, p.g, p.b],
            intensity: p.intensity,
            classification: p.classification,
        })
        .collect();

    let config = tiletopia_core::tileset::TilingConfig::default();
    let stats = tiletopia_core::tileset::tile_point_cloud(octree_points, asset_dir, &config)
        .map_err(|e| e.to_string())?;
    Ok(stats.total_nodes as u64)
}

#[derive(Debug, PartialEq)]
pub(crate) enum Tiler {
    /// This repository's own readers and mesh tiler. `source_is_z_up` says the
    /// format's own coordinates are z-up, so the written glTF gets rotated.
    Native { source_is_z_up: bool },
    /// mago-3d-tiler, carrying the `-it` value for the format.
    External(&'static str),
}

/// Which tiler takes this file extension. Mesh formats are all native, and so
/// is an IFC. Vector formats are mago only, because the native readers take no
/// vector input.
pub(crate) fn tiler_for(extension: &str) -> Result<Tiler, String> {
    let mesh = |source_is_z_up| Ok(Tiler::Native { source_is_z_up });
    match extension {
        "ifc" | "ifczip" => mesh(true),
        "gltf" | "glb" => mesh(false),
        "obj" => mesh(false),
        // the reader turns the file's GlobalSettings UpAxis into y up itself
        "fbx" => mesh(false),
        // dae_reader converts the file's up_axis to y up
        "dae" => mesh(false),
        // dxf world coordinates are z up
        "dxf" => mesh(true),
        "gml" => mesh(true),
        "geojson" => Ok(Tiler::External("geojson")),
        "topojson" => Ok(Tiler::External("geojson")),
        "gpkg" => Ok(Tiler::External("gpkg")),
        "kml" => Ok(Tiler::External("kml")),
        other => Err(format!(
            "{other}: neither the native tiler nor the external one takes this format"
        )),
    }
}

pub(crate) fn move_into_tileset_frame(
    placement: &ModelPlacement,
    ifc_source: Option<&[u8]>,
    meshes: &mut [tiletopia_ingest::MeshData],
) -> Result<Option<MapConversionPlacement>, String> {
    let upload_places_the_model = placement.longitude.is_some() && placement.latitude.is_some();
    let map_conversion = match ifc_source {
        Some(source) if !upload_places_the_model => {
            tiletopia_ingest::ifc_reader::map_conversion_placement(source)
                .map_err(|e| e.to_string())?
        }
        _ => None,
    };
    let Some(map_conversion) = map_conversion else {
        return Ok(None);
    };
    if map_conversion
        .flat_frame_is_within_tolerance(meshes)
        .map_err(|e| e.to_string())?
    {
        return Ok(None);
    }
    map_conversion
        .place_in_east_north_up(meshes)
        .map_err(|e| e.to_string())?;
    Ok(Some(map_conversion))
}

/// Tile a mesh with this repository's own readers and mesh tiler. The source
/// coordinates are metres, placed by longitude and latitude alone.
fn tile_native(
    job: &JobRecord,
    asset_dir: &Path,
    extension: &str,
    source_is_z_up: bool,
) -> Result<TiledAsset, String> {
    let input_path = Path::new(&job.input_path);
    let ifc_source = if extension == "ifc" {
        let source = tiletopia_ingest::ifc_reader::mapped_ifc(input_path)
            .map_err(|e| format!("could not read {}: {e}", input_path.display()))?;
        Some(source)
    } else {
        None
    };
    let mut origin = mesh_origin(ifc_source.as_deref(), &job.placement)?;

    let mut dropped_elements = None;
    let mut read = if let Some(source) = &ifc_source {
        tiletopia_ingest::ifc_reader::read_elements(source, JOB_BUDGET_BYTES).map(|elements| {
            dropped_elements = elements.dropped.summary();
            elements.meshes
        })
    } else if extension == "dxf" {
        let upload_unit = job
            .placement
            .units
            .as_deref()
            .map(tiletopia_ingest::dxf_reader::DrawingUnit::named)
            .transpose()
            .map_err(|e| e.to_string())?;
        tiletopia_ingest::dxf_reader::read(input_path, JOB_BUDGET_BYTES, upload_unit)
    } else {
        tiletopia_ingest::read_mesh(input_path, JOB_BUDGET_BYTES)
    }
    .map_err(|e| e.to_string())?;
    if read.is_empty() {
        return Err(match dropped_elements {
            Some(dropped_elements) => {
                format!("the {extension} holds no geometry: {dropped_elements}")
            }
            None => format!("the {extension} holds no geometry"),
        });
    }
    let east_north_up_placement =
        move_into_tileset_frame(&job.placement, ifc_source.as_deref(), &mut read)?;
    if let Some(placement) = &east_north_up_placement {
        origin = SitePlacement {
            x_axis_from_east_radians: 0.0,
            ..placement.origin
        };
    }

    let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> =
        read.into_iter().map(Into::into).collect();
    let meshes = match &ifc_source {
        Some(source) => element_store::build_store_and_label_meshes(
            source,
            asset_dir,
            tiletopia_ingest::ifc_elements::ModelOrigin {
                longitude: origin.longitude,
                latitude: origin.latitude,
                height: origin.elevation,
                x_axis_from_east_radians: origin.x_axis_from_east_radians,
            },
            east_north_up_placement.as_ref(),
            meshes,
        )?,
        None => meshes,
    };

    // the tileset's frame is the ENU one the root transform names, so a z-up
    // source rotates the written glTF only
    let config = tiletopia_core::mesh_tiler::MeshTilingConfig {
        root_transform: Some(with_x_axis_from_east(
            tiletopia_core::spatial::enu_to_ecef_matrix(
                origin.latitude.to_radians(),
                origin.longitude.to_radians(),
                origin.elevation,
            ),
            origin.x_axis_from_east_radians,
        )),
        content_y_up: source_is_z_up,
        mesh_compression: job.mesh_compression,
        texture_compression: job.texture_compression,
        cad_edges: extension == "ifc",
        ..Default::default()
    };
    let stats = tiletopia_core::mesh_tiler::tile_meshes(&meshes, asset_dir, &config)
        .map_err(|e| e.to_string())?;
    Ok(TiledAsset {
        tiles_written: stats.tile_count as u64,
        dropped_elements,
    })
}

fn with_x_axis_from_east(east_north_up: [f64; 16], x_axis_from_east_radians: f64) -> [f64; 16] {
    let (sine, cosine) = x_axis_from_east_radians.sin_cos();
    let mut rotated = east_north_up;
    for row in 0..3 {
        rotated[row] = cosine * east_north_up[row] + sine * east_north_up[4 + row];
        rotated[4 + row] = cosine * east_north_up[4 + row] - sine * east_north_up[row];
    }
    rotated
}

/// Longitude, latitude and height the mesh's local coordinates sit at. The
/// upload's placement wins, at height 0, and an IFC's own site answers otherwise.
fn mesh_origin(
    ifc_source: Option<&[u8]>,
    placement: &ModelPlacement,
) -> Result<SitePlacement, String> {
    if let (Some(longitude), Some(latitude)) = (placement.longitude, placement.latitude) {
        let x_axis_from_east_radians = ifc_source
            .map(tiletopia_ingest::ifc_reader::x_axis_from_east_radians)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(0.0);
        return Ok(SitePlacement {
            longitude,
            latitude,
            elevation: 0.0,
            x_axis_from_east_radians,
        });
    }

    // an IFC is always tiled natively, so the jar is no answer for one
    let source = ifc_source.ok_or(NO_PLACEMENT_ERROR)?;

    tiletopia_ingest::ifc_reader::site_placement(source)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "the IFC has no site coordinates, upload it with longitude and latitude".to_string()
        })
}

/// The command that turns `input_dir` into 3D Tiles under `output_dir`.
pub fn mago_command(
    jar: &Path,
    input_dir: &Path,
    output_dir: &Path,
    input_type: &str,
    placement: &ModelPlacement,
) -> Command {
    let mut command = Command::new("java");
    command
        .arg("-jar")
        .arg(jar)
        .arg("-i")
        .arg(input_dir)
        .arg("-o")
        .arg(output_dir)
        .arg("-it")
        .arg(input_type)
        .arg("-tv")
        .arg(TILES_VERSION)
        .arg("-q");
    if let Some(crs) = &placement.crs {
        command.arg("-c").arg(crs);
    }
    if let (Some(longitude), Some(latitude)) = (placement.longitude, placement.latitude) {
        command
            .arg("-lon")
            .arg(longitude.to_string())
            .arg("-lat")
            .arg(latitude.to_string());
    }
    command
}

fn run_external_tiler(
    asset_dir: &Path,
    input_dir: &Path,
    input_type: &str,
    placement: &ModelPlacement,
    external_tiler_jar: Option<&Path>,
) -> Result<u64, String> {
    let jar = external_tiler_jar.ok_or_else(|| {
        "TILETOPIA_MAGO_JAR is not set, so there is no external tiler to run".to_string()
    })?;

    let output = mago_command(jar, input_dir, asset_dir, input_type, placement)
        .output()
        .map_err(|e| format!("could not run the external tiler: {e}"))?;

    // mago writes a scratch directory beside the tiles and usually clears it
    let _ = std::fs::remove_dir_all(asset_dir.join("temp"));

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr
            .lines()
            .rev()
            .take(STDERR_LINES_IN_ERROR)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(format!(
            "the external tiler {}: {}",
            output.status,
            tail.join("\n")
        ));
    }

    if !asset_dir.join("tileset.json").exists() {
        return Err("tiler exited 0 but wrote no tileset.json".to_string());
    }

    let tiles = std::fs::read_dir(asset_dir.join("data"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count() as u64
        })
        .unwrap_or(0);
    Ok(tiles)
}

#[cfg(test)]
mod tiler_overlap {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    // a tiler stays this long for a second one to overlap it, which an unlocked second tiler reaches first
    const OVERLAP_WAIT: Duration = Duration::from_secs(1);

    #[derive(Default, Clone, Copy)]
    pub(super) struct Counts {
        running: usize,
        pub(super) entered: usize,
        pub(super) most_running: usize,
    }

    #[derive(Default)]
    pub(super) struct Probe {
        counts: Mutex<Counts>,
        tiler_entered: Condvar,
    }

    pub(super) struct RunningTiler<'probe>(&'probe Probe);

    impl Drop for RunningTiler<'_> {
        fn drop(&mut self) {
            self.0.counts.lock().unwrap().running -= 1;
        }
    }

    impl Probe {
        pub(super) fn enter(&self) -> RunningTiler<'_> {
            let mut counts = self.counts.lock().unwrap();
            counts.running += 1;
            counts.entered += 1;
            counts.most_running = counts.most_running.max(counts.running);
            self.tiler_entered.notify_all();
            let waited = self
                .tiler_entered
                .wait_timeout_while(counts, OVERLAP_WAIT, |counts| counts.running < 2)
                .unwrap();
            drop(waited);
            RunningTiler(self)
        }

        pub(super) fn counts(&self) -> Counts {
            *self.counts.lock().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    const THREE_POINT_PLY: &str = "ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\n\
        property float y\nproperty float z\nproperty uchar red\nproperty uchar green\n\
        property uchar blue\nend_header\n1 2 3 255 0 0\n4 5 6 0 255 0\n7 8 9 0 0 255\n";

    async fn queue_with_point_cloud_job(queue: &JobQueue, data_dir: &Path) -> JobRecord {
        let asset = crate::Asset {
            id: Uuid::new_v4(),
            name: "scan.ply".to_string(),
            asset_type: AssetType::PointCloud,
            status: crate::AssetStatus::Uploading,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: Vec::new(),
            owner_id: None,
        };
        queue.db.create_asset(&asset).await.unwrap();
        let input_dir = data_dir.join(asset.id.to_string()).join("input");
        std::fs::create_dir_all(&input_dir).unwrap();
        let input = input_dir.join("scan.ply");
        std::fs::write(&input, THREE_POINT_PLY).unwrap();
        queue
            .submit(
                asset.id,
                input.to_string_lossy().into_owned(),
                ModelPlacement::default(),
                MeshCompression::None,
                TextureCompression::None,
                PhotogrammetryPreset::Default,
            )
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_jobs_never_tile_at_once() {
        let dir = tempfile::tempdir().unwrap();
        // sqlite shared-cache memory databases answer concurrent writers with table locks
        let db_url = format!("sqlite://{}?mode=rwc", dir.path().join("jobs.db").display());
        let db = Arc::new(Database::new(&db_url).await.unwrap());
        db.migrate().await.unwrap();
        let queue = Arc::new(JobQueue::new(
            Arc::clone(&db),
            dir.path().to_path_buf(),
            Arc::new(tiletopia_store::LocalStore::new(dir.path().to_path_buf())),
            None,
            None,
            Arc::new(WebhookQueue::new(
                Arc::clone(&db),
                platform_webhook::PrivateHosts::Refused,
            )),
            Arc::new(DemStore::new()),
        ));
        let first = queue_with_point_cloud_job(&queue, dir.path()).await;
        let second = queue_with_point_cloud_job(&queue, dir.path()).await;

        tokio::join!(queue.run_job(first.clone()), queue.run_job(second.clone()));

        for job in [first, second] {
            let stored = db.get_job(job.id).await.unwrap().unwrap();
            assert_eq!(stored.status, JobStatus::Done, "{:?}", stored.error);
        }
        let counts = queue.tiler_overlap_probe.counts();
        assert_eq!(counts.entered, 2);
        assert_eq!(counts.most_running, 1);
    }

    #[test]
    fn a_mesh_job_tiles_natively_and_a_vector_job_needs_the_external_tiler() {
        let dir = tempfile::tempdir().unwrap();
        let bundle_dir = dir.path().join("bundle");

        let mesh = dir.path().join("cube.obj");
        std::fs::write(&mesh, "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n").unwrap();
        let mesh_dir = dir.path().join("mesh_asset");
        tile(
            &AssetType::Model,
            &dxf_job(&mesh, None),
            &mesh_dir,
            &bundle_dir,
            None,
        )
        .unwrap();
        assert!(mesh_dir.join("tileset.json").exists());

        for extension in ["geojson", "gpkg", "kml"] {
            let vector = dir.path().join(format!("places.{extension}"));
            std::fs::write(&vector, "read by the external tiler only").unwrap();
            let vector_dir = dir.path().join(format!("{extension}_asset"));
            let error = tile(
                &AssetType::Vector,
                &dxf_job(&vector, None),
                &vector_dir,
                &bundle_dir,
                None,
            )
            .unwrap_err();
            assert!(error.contains("TILETOPIA_MAGO_JAR"), "{extension}: {error}");
            assert!(!vector_dir.join("tileset.json").exists(), "{extension}");
        }
    }

    fn argv(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn mago_command_carries_crs_and_placement() {
        let placement = ModelPlacement {
            longitude: Some(10.0),
            latitude: Some(20.0),
            crs: Some("3857".to_string()),
            units: None,
        };
        let command = mago_command(
            Path::new("/opt/mago/mago-3d-tiler.jar"),
            Path::new("/data/asset/input"),
            Path::new("/data/asset"),
            "obj",
            &placement,
        );

        assert_eq!(command.get_program(), OsStr::new("java"));
        assert_eq!(
            argv(&command),
            [
                "-jar",
                "/opt/mago/mago-3d-tiler.jar",
                "-i",
                "/data/asset/input",
                "-o",
                "/data/asset",
                "-it",
                "obj",
                "-tv",
                "1.1",
                "-q",
                "-c",
                "3857",
                "-lon",
                "10",
                "-lat",
                "20",
            ]
        );
    }

    #[test]
    fn mago_command_without_crs_or_placement_stops_after_quiet() {
        let command = mago_command(
            Path::new("/opt/mago/mago-3d-tiler.jar"),
            Path::new("/data/asset/input"),
            Path::new("/data/asset"),
            "glb",
            &ModelPlacement::default(),
        );

        assert_eq!(
            argv(&command),
            [
                "-jar",
                "/opt/mago/mago-3d-tiler.jar",
                "-i",
                "/data/asset/input",
                "-o",
                "/data/asset",
                "-it",
                "glb",
                "-tv",
                "1.1",
                "-q",
            ]
        );
    }

    const Z_UP: Tiler = Tiler::Native {
        source_is_z_up: true,
    };
    const Y_UP: Tiler = Tiler::Native {
        source_is_z_up: false,
    };

    #[test]
    fn meshes_are_tiled_natively_and_vectors_go_to_mago() {
        assert_eq!(tiler_for("ifc").unwrap(), Z_UP);
        assert_eq!(tiler_for("gltf").unwrap(), Y_UP);
        assert_eq!(tiler_for("glb").unwrap(), Y_UP);
        assert_eq!(tiler_for("obj").unwrap(), Y_UP);
        assert_eq!(tiler_for("fbx").unwrap(), Y_UP);
        assert_eq!(tiler_for("dae").unwrap(), Y_UP);
        assert_eq!(tiler_for("dxf").unwrap(), Z_UP);
        assert_eq!(tiler_for("gml").unwrap(), Z_UP);
        assert_eq!(tiler_for("geojson").unwrap(), Tiler::External("geojson"));
        assert_eq!(tiler_for("topojson").unwrap(), Tiler::External("geojson"));
        assert_eq!(tiler_for("gpkg").unwrap(), Tiler::External("gpkg"));
        assert_eq!(tiler_for("kml").unwrap(), Tiler::External("kml"));
    }

    const UNITLESS_TRIANGLE_DXF: &str = "0\nSECTION\n2\nHEADER\n9\n$INSUNITS\n70\n0\n0\nENDSEC\n\
        0\nSECTION\n2\nENTITIES\n0\n3DFACE\n\
        10\n0\n20\n0\n30\n0\n11\n1000\n21\n0\n31\n0\n\
        12\n0\n22\n1000\n32\n0\n13\n0\n23\n1000\n33\n0\n\
        0\nENDSEC\n0\nEOF\n";

    fn dxf_job(input_path: &Path, units: Option<&str>) -> JobRecord {
        JobRecord {
            id: Uuid::new_v4(),
            asset_id: Uuid::new_v4(),
            status: JobStatus::Queued,
            progress: 0.0,
            input_path: input_path.to_string_lossy().into_owned(),
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement: ModelPlacement {
                longitude: Some(10.0),
                latitude: Some(20.0),
                crs: None,
                units: units.map(str::to_string),
            },
            mesh_compression: MeshCompression::None,
            texture_compression: TextureCompression::None,
            photogrammetry_preset: PhotogrammetryPreset::Default,
        }
    }

    #[test]
    fn a_unitless_dxf_tiles_only_at_the_scale_its_units_field_names() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("face.dxf");
        std::fs::write(&input, UNITLESS_TRIANGLE_DXF).unwrap();
        let asset_dir = dir.path().join("asset");
        let bundle_dir = dir.path().join("bundle");

        let error = tile(
            &AssetType::Model,
            &dxf_job(&input, None),
            &asset_dir,
            &bundle_dir,
            None,
        )
        .unwrap_err();
        assert!(error.contains("$INSUNITS"), "{error}");

        tile(
            &AssetType::Model,
            &dxf_job(&input, Some("millimetres")),
            &asset_dir,
            &bundle_dir,
            None,
        )
        .unwrap();
        let json = std::fs::read_to_string(asset_dir.join("tileset.json")).unwrap();
        let tileset: tiletopia_core::Tileset = serde_json::from_str(&json).unwrap();
        let tiletopia_core::BoundingVolume::Box { r#box } = tileset.root.bounding_volume else {
            panic!("mesh root is not a box");
        };
        // a 1000 by 1000 millimetre triangle is a metre square box, z up
        assert!((r#box[3] - 0.5).abs() < 1e-6, "{:?}", r#box);
        assert!((r#box[7] - 0.5).abs() < 1e-6, "{:?}", r#box);
        assert!(r#box[11].abs() < 1e-6, "{:?}", r#box);
    }

    #[test]
    fn a_quarter_turn_points_the_model_x_axis_north() {
        let east_north_up = tiletopia_core::spatial::enu_to_ecef_matrix(
            20f64.to_radians(),
            10f64.to_radians(),
            0.0,
        );

        let rotated = with_x_axis_from_east(east_north_up, std::f64::consts::FRAC_PI_2);

        for row in 0..3 {
            assert!((rotated[row] - east_north_up[4 + row]).abs() < 1e-12);
            assert!((rotated[4 + row] + east_north_up[row]).abs() < 1e-12);
        }
        assert_eq!(rotated[8..], east_north_up[8..]);
    }

    #[test]
    fn no_rotation_leaves_the_east_north_up_frame_alone() {
        let east_north_up = tiletopia_core::spatial::enu_to_ecef_matrix(
            20f64.to_radians(),
            10f64.to_radians(),
            0.0,
        );

        assert_eq!(with_x_axis_from_east(east_north_up, 0.0), east_north_up);
    }

    const BOXES_EASTING: f64 = 600_000.0;
    const BOXES_NORTHING: f64 = 5_000_000.0;
    const BOXES_HEIGHT: f64 = 100.0;
    // the model's x axis points 36.87 degrees north of grid east
    const BOXES_X_AXIS_ABSCISSA: f64 = 0.8;
    const BOXES_X_AXIS_ORDINATE: f64 = 0.6;
    const BOX_HALF_HEIGHT_METRES: f64 = 1.5;
    // the BIM placement check allows 0.1 m or 0.1 percent of the distance from the map origin, whichever is larger
    const LEAST_PLACEMENT_TOLERANCE_METRES: f64 = 0.1;
    const PLACEMENT_TOLERANCE_SHARE_OF_DISTANCE: f64 = 0.001;
    const CONTENT_TOLERANCE_METRES: f64 = 1e-3;

    fn two_boxes_ifc(far_box_x_metres: f64) -> String {
        format!(
            "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');\n\
             FILE_NAME('boxes.ifc','2024-01-01',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\n\
             ENDSEC;\nDATA;\n\
             #1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);\n\
             #10=IFCCARTESIANPOINT((0.,0.,0.));\n\
             #11=IFCAXIS2PLACEMENT3D(#10,$,$);\n\
             #12=IFCLOCALPLACEMENT($,#11);\n\
             #20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);\n\
             #21=IFCDIRECTION((0.,0.,1.));\n\
             #22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);\n\
             #30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));\n\
             #31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));\n\
             #40=IFCWALL('0002',$,'NearBox',$,$,#12,#31,$,$);\n\
             #50=IFCCARTESIANPOINT(({far_box_x_metres:.1},0.,0.));\n\
             #51=IFCAXIS2PLACEMENT3D(#50,$,$);\n\
             #52=IFCLOCALPLACEMENT($,#51);\n\
             #53=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,4.0,2.0);\n\
             #54=IFCEXTRUDEDAREASOLID(#53,#11,#21,3.0);\n\
             #55=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#54));\n\
             #56=IFCPRODUCTDEFINITIONSHAPE($,$,(#55));\n\
             #57=IFCWALL('0003',$,'FarBox',$,$,#52,#56,$,$);\n\
             #60=IFCPROJECTEDCRS('EPSG:32632',$,$,$,$,$,$);\n\
             #61=IFCMAPCONVERSION($,#60,{BOXES_EASTING:.1},{BOXES_NORTHING:.1},{BOXES_HEIGHT:.1},\
             {BOXES_X_AXIS_ABSCISSA:.1},{BOXES_X_AXIS_ORDINATE:.1},1.);\n\
             ENDSEC;\nEND-ISO-10303-21;\n"
        )
    }

    fn box_local_centers(far_box_x_metres: f64) -> [[f64; 3]; 2] {
        [
            [0.0, 0.0, BOX_HALF_HEIGHT_METRES],
            [far_box_x_metres, 0.0, BOX_HALF_HEIGHT_METRES],
        ]
    }

    fn expected_ecef_of(local: [f64; 3]) -> [f64; 3] {
        use tiletopia_core::crs::{Coord3D, CrsDef, Transformer};
        let geodetic = Transformer::new(
            CrsDef::Epsg(u32::from(UTM_32_NORTH_EPSG)),
            CrsDef::Epsg(4326),
        )
        .transform(Coord3D {
            x: BOXES_EASTING + BOXES_X_AXIS_ABSCISSA * local[0] - BOXES_X_AXIS_ORDINATE * local[1],
            y: BOXES_NORTHING + BOXES_X_AXIS_ORDINATE * local[0] + BOXES_X_AXIS_ABSCISSA * local[1],
            z: BOXES_HEIGHT + local[2],
        })
        .unwrap();
        let ecef = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(4978))
            .transform(geodetic)
            .unwrap();
        [ecef.x, ecef.y, ecef.z]
    }

    fn distance_between(from: [f64; 3], to: [f64; 3]) -> f64 {
        from.iter()
            .zip(to)
            .map(|(start, end)| (end - start).powi(2))
            .sum::<f64>()
            .sqrt()
    }

    fn assert_within_placement_tolerance(
        content_center: [f64; 3],
        root: &[f64; 16],
        local: [f64; 3],
    ) {
        let placed: [f64; 3] = std::array::from_fn(|row| {
            (0..3)
                .map(|column| root[4 * column + row] * content_center[column])
                .sum::<f64>()
                + root[12 + row]
        });
        let expected = expected_ecef_of(local);
        let tolerance = LEAST_PLACEMENT_TOLERANCE_METRES.max(
            PLACEMENT_TOLERANCE_SHARE_OF_DISTANCE
                * distance_between(expected_ecef_of([0.0; 3]), expected),
        );
        let error = distance_between(placed, expected);
        assert!(
            error <= tolerance,
            "the box at {local:?} is {error} m from {expected:?}, over the {tolerance} m tolerance"
        );
    }

    fn push_leaf_vertices_in_the_tile_frame(
        tile: &serde_json::Value,
        asset_dir: &Path,
        vertices: &mut Vec<[f64; 3]>,
    ) {
        if let Some(children) = tile["children"]
            .as_array()
            .filter(|children| !children.is_empty())
        {
            for child in children {
                push_leaf_vertices_in_the_tile_frame(child, asset_dir, vertices);
            }
            return;
        }
        let Some(uri) = tile["content"]["uri"].as_str() else {
            return;
        };
        let bytes = std::fs::read(asset_dir.join(uri)).unwrap();
        let glb = gltf::Glb::from_slice(&bytes).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        let bin = glb.bin.expect("a binary chunk");
        for node in json["nodes"].as_array().unwrap() {
            let Some(mesh) = node["mesh"].as_u64() else {
                continue;
            };
            let translation: [f64; 3] =
                std::array::from_fn(|axis| node["translation"][axis].as_f64().unwrap_or(0.0));
            for primitive in json["meshes"][mesh as usize]["primitives"]
                .as_array()
                .unwrap()
            {
                let accessor = &json["accessors"]
                    [primitive["attributes"]["POSITION"].as_u64().unwrap() as usize];
                let view = &json["bufferViews"][accessor["bufferView"].as_u64().unwrap() as usize];
                let start = (view["byteOffset"].as_u64().unwrap_or(0)
                    + accessor["byteOffset"].as_u64().unwrap_or(0))
                    as usize;
                let stride = view["byteStride"].as_u64().unwrap_or(12) as usize;
                for vertex in 0..accessor["count"].as_u64().unwrap() as usize {
                    let at = start + vertex * stride;
                    let y_up: [f64; 3] = std::array::from_fn(|axis| {
                        let component = bin[at + 4 * axis..at + 4 * axis + 4].try_into().unwrap();
                        f64::from(f32::from_le_bytes(component)) + translation[axis]
                    });
                    // 3D Tiles runtimes turn y-up glTF content into the z-up tile frame
                    vertices.push([y_up[0], -y_up[2], y_up[1]]);
                }
            }
        }
    }

    async fn tiled_box_centers(far_box_x_metres: f64) -> ([[f64; 3]; 2], [f64; 16]) {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("boxes.ifc");
        std::fs::write(&input, two_boxes_ifc(far_box_x_metres)).unwrap();
        let mut job = dxf_job(&input, None);
        job.placement = ModelPlacement::default();
        let asset_dir = dir.path().join("asset");
        let tiled_dir = asset_dir.clone();
        let bundle_dir = dir.path().join("bundle");
        // the element store calls tokio's Handle::block_on, which panics on a runtime worker thread
        tokio::task::spawn_blocking(move || {
            tile(&AssetType::Model, &job, &tiled_dir, &bundle_dir, None)
        })
        .await
        .unwrap()
        .unwrap();

        let tileset: serde_json::Value =
            serde_json::from_slice(&std::fs::read(asset_dir.join("tileset.json")).unwrap())
                .unwrap();
        let root: [f64; 16] =
            std::array::from_fn(|index| tileset["root"]["transform"][index].as_f64().unwrap());
        let mut vertices = Vec::new();
        push_leaf_vertices_in_the_tile_frame(&tileset["root"], &asset_dir, &mut vertices);
        let center_of = |is_far_box: bool| {
            let mut bounds = tiletopia_core::bounding_volume::Aabb::empty();
            for vertex in &vertices {
                if (distance_between([0.0; 3], *vertex) > far_box_x_metres / 2.0) == is_far_box {
                    bounds.expand_point(*vertex);
                }
            }
            bounds.center()
        };
        ([center_of(false), center_of(true)], root)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn boxes_twenty_kilometres_apart_land_at_their_map_coordinates() {
        let far_box_x_metres = 20_000.0;

        let (centers, root) = tiled_box_centers(far_box_x_metres).await;

        for (center, local) in centers.into_iter().zip(box_local_centers(far_box_x_metres)) {
            assert_within_placement_tolerance(center, &root, local);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn boxes_twenty_metres_apart_keep_their_ifc_coordinates_in_one_flat_frame() {
        let far_box_x_metres = 20.0;

        let (centers, root) = tiled_box_centers(far_box_x_metres).await;

        for (center, local) in centers.into_iter().zip(box_local_centers(far_box_x_metres)) {
            assert!(
                distance_between(center, local) < CONTENT_TOLERANCE_METRES,
                "the box at {local:?} is at {center:?} in the tile frame"
            );
            assert_within_placement_tolerance(center, &root, local);
        }
    }

    const FAR_BOX_GLOBAL_ID: &str = "0003";
    const FAR_STOREY_GLOBAL_ID: &str = "0004";

    fn unplaced_job_reading(dir: &Path, ifc: &str) -> JobRecord {
        let input = dir.join("boxes.ifc");
        std::fs::write(&input, ifc).unwrap();
        let mut job = dxf_job(&input, None);
        job.placement = ModelPlacement::default();
        job
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_section_through_a_box_twenty_kilometres_out_lands_on_the_tiled_box() {
        let far_box_x_metres = 20_000.0;
        let ([_, tiled_far_box], _) = tiled_box_centers(far_box_x_metres).await;
        let dir = tempfile::tempdir().unwrap();
        let job = unplaced_job_reading(dir.path(), &two_boxes_ifc(far_box_x_metres));

        let meshes = crate::dxf_export::read_meshes_in_tileset_frame(&job, "ifc").unwrap();
        let section =
            crate::section::section_of(&meshes, crate::section::Axis::Z, tiled_far_box[2]);

        let far_box_rings: Vec<&crate::section::Ring> = section
            .polygons
            .iter()
            .filter(|polygon| polygon.element_id == FAR_BOX_GLOBAL_ID)
            .flat_map(|polygon| &polygon.rings)
            .collect();
        assert_eq!(far_box_rings.len(), 1, "{:?}", section.polygons);
        let mut cut_bounds = tiletopia_core::bounding_volume::Aabb::empty();
        for point in far_box_rings[0] {
            cut_bounds.expand_point(*point);
        }
        let cut_center = cut_bounds.center();
        for axis in 0..2 {
            assert!(
                (cut_center[axis] - tiled_far_box[axis]).abs() < LEAST_PLACEMENT_TOLERANCE_METRES,
                "the cut is centred on {cut_center:?} and the tiled box on {tiled_far_box:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_storey_twenty_kilometres_out_gets_the_height_its_meshes_are_placed_at() {
        let dir = tempfile::tempdir().unwrap();
        let ifc = two_boxes_ifc(20_000.0).replace(
            "ENDSEC;\nEND-ISO",
            &format!(
                "#70=IFCBUILDINGSTOREY('{FAR_STOREY_GLOBAL_ID}',$,'FarLevel',$,$,#52,$,$,.ELEMENT.,0.);\n\
                 ENDSEC;\nEND-ISO"
            ),
        );
        let job = unplaced_job_reading(dir.path(), &ifc);
        let asset_dir = dir.path().join("asset");
        let tiled_dir = asset_dir.clone();
        let bundle_dir = dir.path().join("bundle");
        // the element store calls tokio's Handle::block_on, which panics on a runtime worker thread
        tokio::task::spawn_blocking(move || {
            tile(&AssetType::Model, &job, &tiled_dir, &bundle_dir, None)
                .and_then(|_| element_store::finish_build(&tiled_dir, true))
        })
        .await
        .unwrap()
        .unwrap();

        let mut store = element_store::open_store(&asset_dir)
            .await
            .unwrap()
            .expect("an element store");
        let storey = element_store::get_element(&mut store, FAR_STOREY_GLOBAL_ID)
            .await
            .unwrap()
            .expect("the storey");
        // the flat frame puts a storey 20 km from the map origin 31 m above its OrthogonalHeight
        let base_height = storey.base_height.expect("a base height");
        assert!(
            (base_height - BOXES_HEIGHT).abs() < LEAST_PLACEMENT_TOLERANCE_METRES,
            "{base_height}"
        );
    }

    #[test]
    fn txt_goes_to_neither_tiler() {
        let error = tiler_for("txt").unwrap_err();
        assert!(error.contains("txt"), "{error}");
        assert!(error.contains("native tiler"), "{error}");
        assert!(error.contains("external one"), "{error}");
    }

    const UTM_32_NORTH_EPSG: u16 = 32632;
    const UTM_32_NORTH_UPLOAD_CRS: &str = "EPSG:32632";
    const UTM_33_NORTH_UPLOAD_CRS: &str = "EPSG:32633";
    const LAS_GEO_KEY_DIRECTORY_RECORD_ID: u16 = 34735;
    const PROJECTED_CRS_GEO_KEY: u16 = 3072;
    // UTM 32N (500000, 0) sits exactly on 9 degrees east at the equator
    const FIXTURE_ORIGIN_EASTING: f64 = 500_000.0;
    const FIXTURE_ORIGIN_LONGITUDE: f64 = 9.0;
    // every fixture point is within 11.4 m of the origin: 5 m east or west, 10 m north, 2 m up
    const FIXTURE_CENTER_TOLERANCE_METRES: f64 = 12.0;
    const FIXTURE_FILE_NAME: &str = "scan.las";
    const PNTS_HEADER_BYTES: usize = 28;
    const PNTS_FEATURE_TABLE_JSON_LENGTH_OFFSET: usize = 12;
    const PNTS_POSITION_BYTES: usize = 12;
    const STORED_POSITION_TOLERANCE_METRES: f64 = 0.01;

    fn geo_key_directory(epsg: u16) -> Vec<u8> {
        [1, 1, 0, 1, PROJECTED_CRS_GEO_KEY, 0, 1, epsg]
            .iter()
            .flat_map(|value: &u16| value.to_le_bytes())
            .collect()
    }

    fn write_fixture_las(path: &Path, epsg: Option<u16>) {
        let mut builder = las::Builder::new(Default::default()).unwrap();
        builder.version = las::Version::new(1, 2);
        let millimetres = |offset| las::Transform {
            scale: 0.001,
            offset,
        };
        builder.transforms = las::Vector {
            x: millimetres(FIXTURE_ORIGIN_EASTING),
            y: millimetres(0.0),
            z: millimetres(0.0),
        };
        if let Some(epsg) = epsg {
            builder.vlrs.push(las::Vlr {
                user_id: "LASF_Projection".to_string(),
                record_id: LAS_GEO_KEY_DIRECTORY_RECORD_ID,
                description: String::new(),
                data: geo_key_directory(epsg),
            });
        }
        let mut writer = las::Writer::from_path(path, builder.into_header().unwrap()).unwrap();
        for east in -5..=5 {
            for north in 0..=10 {
                writer
                    .write_point(las::Point {
                        x: FIXTURE_ORIGIN_EASTING + f64::from(east),
                        y: f64::from(north),
                        z: f64::from(north % 3),
                        ..Default::default()
                    })
                    .unwrap();
            }
        }
        writer.close().unwrap();
    }

    fn tile_fixture(dir: &Path, epsg: Option<u16>, upload_crs: Option<&str>) -> PathBuf {
        let input = dir.join(FIXTURE_FILE_NAME);
        write_fixture_las(&input, epsg);
        let asset_dir = dir.join("asset");
        tile_point_cloud(&input, &asset_dir, upload_crs).unwrap();
        asset_dir
    }

    fn tile_fixture_root_box_center(epsg: Option<u16>, upload_crs: Option<&str>) -> [f64; 3] {
        let dir = tempfile::tempdir().unwrap();
        let asset_dir = tile_fixture(dir.path(), epsg, upload_crs);

        let json = std::fs::read_to_string(asset_dir.join("tileset.json")).unwrap();
        let tileset: tiletopia_core::Tileset = serde_json::from_str(&json).unwrap();
        let tiletopia_core::BoundingVolume::Box { r#box } = tileset.root.bounding_volume else {
            panic!("point cloud root is not a box");
        };
        [r#box[0], r#box[1], r#box[2]]
    }

    fn assert_at_fixture_earth_location(center: [f64; 3]) {
        let expected = tiletopia_core::spatial::geodetic_to_ecef(
            0.0,
            FIXTURE_ORIGIN_LONGITUDE.to_radians(),
            0.0,
        );
        for axis in 0..3 {
            assert!(
                (center[axis] - expected[axis]).abs() < FIXTURE_CENTER_TOLERANCE_METRES,
                "root box center {center:?}, expected near {expected:?}"
            );
        }
    }

    #[test]
    fn projected_point_cloud_is_tiled_at_its_earth_location() {
        assert_at_fixture_earth_location(tile_fixture_root_box_center(
            Some(UTM_32_NORTH_EPSG),
            None,
        ));
    }

    #[test]
    fn point_cloud_without_a_geo_key_is_placed_by_the_upload_crs() {
        assert_at_fixture_earth_location(tile_fixture_root_box_center(
            None,
            Some(UTM_32_NORTH_UPLOAD_CRS),
        ));
    }

    #[test]
    fn point_cloud_geo_key_wins_over_the_upload_crs() {
        assert_at_fixture_earth_location(tile_fixture_root_box_center(
            Some(UTM_32_NORTH_EPSG),
            Some(UTM_33_NORTH_UPLOAD_CRS),
        ));
    }

    #[test]
    fn point_cloud_upload_crs_that_is_not_an_epsg_code_fails_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join(FIXTURE_FILE_NAME);
        write_fixture_las(&input, None);
        let error = tile_point_cloud(&input, &dir.path().join("asset"), Some("utm32")).unwrap_err();
        assert!(error.contains("utm32"), "{error}");
    }

    #[test]
    fn point_cloud_tile_positions_plus_rtc_center_are_earth_positions() {
        let dir = tempfile::tempdir().unwrap();
        let asset_dir = tile_fixture(dir.path(), Some(UTM_32_NORTH_EPSG), None);
        let tile = std::fs::read(asset_dir.join("tiles/root.pnts")).unwrap();

        let json_start = PNTS_HEADER_BYTES;
        let json_length = u32::from_le_bytes(
            tile[PNTS_FEATURE_TABLE_JSON_LENGTH_OFFSET..PNTS_FEATURE_TABLE_JSON_LENGTH_OFFSET + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let feature_table: serde_json::Value =
            serde_json::from_slice(&tile[json_start..json_start + json_length]).unwrap();
        let rtc_center: [f64; 3] = serde_json::from_value(feature_table["RTC_CENTER"].clone())
            .expect("feature table has an RTC_CENTER");
        let point_count = feature_table["POINTS_LENGTH"].as_u64().unwrap() as usize;
        let positions_start = json_start
            + json_length
            + feature_table["POSITION"]["byteOffset"].as_u64().unwrap() as usize;
        let positions = &tile[positions_start..positions_start + point_count * PNTS_POSITION_BYTES];

        let earth_positions =
            tiletopia_ingest::read_point_cloud_ecef(&dir.path().join(FIXTURE_FILE_NAME), None)
                .unwrap();
        assert_eq!(point_count, earth_positions.len());
        let offsets: Vec<f64> = positions
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f64::from(f32::from_le_bytes(*bytes)))
            .collect();
        for offset in offsets.as_chunks::<3>().0 {
            let absolute = [
                rtc_center[0] + offset[0],
                rtc_center[1] + offset[1],
                rtc_center[2] + offset[2],
            ];
            let nearest = earth_positions
                .iter()
                .map(|p| {
                    ((p.x - absolute[0]).powi(2)
                        + (p.y - absolute[1]).powi(2)
                        + (p.z - absolute[2]).powi(2))
                    .sqrt()
                })
                .fold(f64::INFINITY, f64::min);
            assert!(
                nearest < STORED_POSITION_TOLERANCE_METRES,
                "{absolute:?} is {nearest} m from the nearest fixture point"
            );
        }
    }

    #[test]
    fn point_cloud_without_a_crs_is_tiled_in_its_own_coordinates() {
        let center = tile_fixture_root_box_center(None, None);
        assert!(
            (center[0] - FIXTURE_ORIGIN_EASTING).abs() < FIXTURE_CENTER_TOLERANCE_METRES,
            "root box center {center:?}"
        );
    }

    const SPLAT_PLY_PROPERTIES: [&str; 14] = [
        "x", "y", "z", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1", "scale_2",
        "rot_0", "rot_1", "rot_2", "rot_3",
    ];

    fn write_splat_ply(path: &Path) {
        let mut bytes = format!(
            "ply\nformat binary_little_endian 1.0\nelement vertex 3\n{}end_header\n",
            SPLAT_PLY_PROPERTIES
                .map(|name| format!("property float {name}\n"))
                .concat()
        )
        .into_bytes();
        for splat in 0..3 {
            let offset = splat as f32;
            let row = [
                offset,
                2.0 * offset,
                -offset,
                0.1,
                0.2,
                0.3,
                1.0,
                -3.0,
                -3.0,
                -3.0,
                1.0,
                0.0,
                0.0,
                0.0,
            ];
            for value in row {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn an_ordinary_ply_goes_to_the_point_cloud_tiler() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("scan.ply");
        std::fs::write(
            &input,
            "ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nproperty float y\n\
             property float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\n\
             end_header\n1 2 3 255 0 0\n4 5 6 0 255 0\n7 8 9 0 0 255\n",
        )
        .unwrap();
        let asset_dir = dir.path().join("asset");

        tile_point_cloud_upload(&input, &asset_dir, &ModelPlacement::default()).unwrap();

        assert!(asset_dir.join("tiles/root.pnts").exists());
        assert!(!asset_dir.join("tiles/root.glb").exists());
    }

    #[test]
    fn a_splat_ply_goes_to_the_splat_tiler_at_its_placement() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.ply");
        write_splat_ply(&input);
        let asset_dir = dir.path().join("asset");
        let placement = ModelPlacement {
            longitude: Some(10.0),
            latitude: Some(20.0),
            crs: None,
            units: None,
        };

        tile_point_cloud_upload(&input, &asset_dir, &placement).unwrap();

        assert!(asset_dir.join("tiles/root.glb").exists());
        assert!(!asset_dir.join("tiles/root.pnts").exists());
        let tileset: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(asset_dir.join("tileset.json")).unwrap())
                .unwrap();
        assert_eq!(
            tileset["extensions"]["3DTILES_content_gltf"]["extensionsRequired"],
            serde_json::json!([
                "KHR_gaussian_splatting",
                "KHR_gaussian_splatting_compression_spz_2"
            ])
        );
        let transform: Vec<f64> =
            serde_json::from_value(tileset["root"]["transform"].clone()).unwrap();
        let expected = tiletopia_core::spatial::enu_to_ecef_matrix(
            20f64.to_radians(),
            10f64.to_radians(),
            0.0,
        );
        assert_eq!(transform, expected.to_vec());
    }

    #[test]
    fn a_splat_ply_without_longitude_and_latitude_fails_naming_them() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.ply");
        write_splat_ply(&input);
        let placement = ModelPlacement {
            crs: Some(UTM_32_NORTH_UPLOAD_CRS.to_string()),
            ..Default::default()
        };

        let error =
            tile_point_cloud_upload(&input, &dir.path().join("asset"), &placement).unwrap_err();

        assert!(error.contains("longitude and latitude"), "{error}");
    }
}
