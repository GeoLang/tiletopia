//! Async job queue for tiling operations.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

use crate::AssetType;
use crate::db::{Database, JobRecord, JobStatus, ModelPlacement};
use tiletopia_store::TileStore;

/// 3D Tiles version asked of the external tiler.
const TILES_VERSION: &str = "1.1";

/// How much of the external tiler's stderr a failed job reports.
const STDERR_LINES_IN_ERROR: usize = 20;

pub struct JobQueue {
    db: Arc<Database>,
    data_dir: PathBuf,
    #[allow(dead_code)]
    store: Arc<dyn TileStore>,
    external_tiler_jar: Option<PathBuf>,
}

impl JobQueue {
    pub fn new(
        db: Arc<Database>,
        data_dir: PathBuf,
        store: Arc<dyn TileStore>,
        external_tiler_jar: Option<PathBuf>,
    ) -> Self {
        Self {
            db,
            data_dir,
            store,
            external_tiler_jar,
        }
    }

    /// Start the background worker loop.
    pub async fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.db.next_queued_job().await {
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

    async fn run_job(&self, mut job: JobRecord) {
        job.status = JobStatus::Running;
        job.started_at = Some(chrono::Utc::now());
        if let Err(e) = self.db.update_job(&job).await {
            tracing::error!("Failed to update job {}: {}", job.id, e);
            return;
        }

        let asset_id = job.asset_id;
        let asset_type = match self.db.get_asset(asset_id).await {
            Ok(Some(mut asset)) => {
                asset.status = crate::AssetStatus::Tiling;
                let _ = self.db.update_asset(&asset).await;
                asset.asset_type
            }
            _ => {
                self.finish(job, Err(format!("asset {asset_id} is gone")))
                    .await;
                return;
            }
        };

        let asset_dir = self.data_dir.join(asset_id.to_string());
        let input_path = PathBuf::from(&job.input_path);
        let placement = job.placement.clone();
        let jar = self.external_tiler_jar.clone();

        let result = tokio::task::spawn_blocking(move || {
            tile(
                &asset_type,
                &input_path,
                &asset_dir,
                &placement,
                jar.as_deref(),
            )
        })
        .await;

        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => Err(error.to_string()),
        };

        // the asset write goes first: a client polling the job
        // stops at Done and reads the asset straight after, so
        // an asset still saying Tiling there never corrects
        if let Ok(Some(mut asset)) = self.db.get_asset(asset_id).await {
            match &outcome {
                Ok(tiles) => {
                    asset.status = crate::AssetStatus::Ready;
                    asset.tile_count = *tiles;
                }
                Err(_) => asset.status = crate::AssetStatus::Error,
            }
            if let Err(error) = self.db.update_asset(&asset).await {
                tracing::error!(
                    "Job {}: asset {} status write failed: {}",
                    job.id,
                    asset_id,
                    error
                );
            }
        }

        self.finish(job, outcome).await;
    }

    async fn finish(&self, mut job: JobRecord, outcome: Result<u64, String>) {
        job.completed_at = Some(chrono::Utc::now());
        match outcome {
            Ok(tiles) => {
                job.status = JobStatus::Done;
                job.progress = 1.0;
                job.tiles_written = tiles;
                tracing::info!("Job {} completed: {} tiles", job.id, tiles);
            }
            Err(error) => {
                job.status = JobStatus::Failed;
                tracing::error!("Job {} failed: {}", job.id, error);
                job.error = Some(error);
            }
        }
        let _ = self.db.update_job(&job).await;
    }

    /// Submit a new job.
    pub async fn submit(
        &self,
        asset_id: Uuid,
        input_path: String,
        placement: ModelPlacement,
    ) -> Result<JobRecord, sqlx::Error> {
        let job = JobRecord {
            id: Uuid::new_v4(),
            asset_id,
            status: JobStatus::Queued,
            progress: 0.0,
            input_path,
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement,
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
    input_path: &Path,
    asset_dir: &Path,
    placement: &ModelPlacement,
    external_tiler_jar: Option<&Path>,
) -> Result<u64, String> {
    match asset_type {
        AssetType::PointCloud => tile_point_cloud(input_path, asset_dir),
        AssetType::Model | AssetType::Vector => {
            let extension = input_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            match tiler_for(&extension)? {
                Tiler::Native => tile_ifc(input_path, asset_dir, placement),
                Tiler::External(input_type) => {
                    run_external_tiler(asset_dir, input_type, placement, external_tiler_jar)
                }
            }
        }
        AssetType::Terrain | AssetType::Imagery => {
            Err("terrain and imagery assets are not tiled to 3D Tiles".to_string())
        }
    }
}

fn tile_point_cloud(input_path: &Path, asset_dir: &Path) -> Result<u64, String> {
    let points = tiletopia_ingest::read_point_cloud(input_path).map_err(|e| e.to_string())?;
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

#[derive(Debug)]
enum Tiler {
    /// This repository's own readers and mesh tiler.
    Native,
    /// mago-3d-tiler, carrying the `-it` value for the format.
    External(&'static str),
}

/// Which tiler takes this file extension. Both `gltf` and `glb` are accepted
/// for either glTF spelling, so the extension goes through as it is.
fn tiler_for(extension: &str) -> Result<Tiler, String> {
    match extension {
        "ifc" => Ok(Tiler::Native),
        "gltf" => Ok(Tiler::External("gltf")),
        "glb" => Ok(Tiler::External("glb")),
        "obj" => Ok(Tiler::External("obj")),
        "fbx" => Ok(Tiler::External("fbx")),
        "geojson" => Ok(Tiler::External("geojson")),
        "gpkg" => Ok(Tiler::External("gpkg")),
        "kml" => Ok(Tiler::External("kml")),
        "gml" => Ok(Tiler::External("citygml")),
        other => Err(format!(
            "{other}: neither the native tiler nor the external one takes this format"
        )),
    }
}

/// Tile an IFC with this repository's own reader and mesh tiler. The upload's
/// `crs` is ignored here: the IFC's own coordinates are metres, placed by
/// longitude and latitude alone.
fn tile_ifc(
    input_path: &Path,
    asset_dir: &Path,
    placement: &ModelPlacement,
) -> Result<u64, String> {
    let (longitude, latitude, height) = ifc_origin(input_path, placement)?;

    let read = tiletopia_ingest::read_mesh(input_path).map_err(|e| e.to_string())?;
    if read.is_empty() {
        return Err("the IFC holds no geometry".to_string());
    }

    let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> = read
        .into_iter()
        .map(|mesh| tiletopia_core::mesh_tiler::MeshData {
            positions: mesh.positions,
            normals: mesh.normals,
            indices: mesh.indices,
            name: mesh.name,
        })
        .collect();

    // the IFC's own coordinates are z-up, and the tileset's frame is the ENU
    // one the root transform names, so only the written glTF is rotated
    let config = tiletopia_core::mesh_tiler::MeshTilingConfig {
        root_transform: Some(tiletopia_core::spatial::enu_to_ecef_matrix(
            latitude.to_radians(),
            longitude.to_radians(),
            height,
        )),
        content_y_up: true,
        ..Default::default()
    };
    let stats = tiletopia_core::mesh_tiler::tile_meshes(&meshes, asset_dir, &config)
        .map_err(|e| e.to_string())?;
    Ok(stats.tile_count as u64)
}

/// Longitude, latitude and height the IFC's local coordinates sit at. The
/// upload's placement wins, at height 0, and the IfcSite answers otherwise.
fn ifc_origin(input_path: &Path, placement: &ModelPlacement) -> Result<(f64, f64, f64), String> {
    if let (Some(longitude), Some(latitude)) = (placement.longitude, placement.latitude) {
        return Ok((longitude, latitude, 0.0));
    }

    let site = tiletopia_ingest::ifc_reader::site_placement(input_path)
        .map_err(|e| e.to_string())?
        .ok_or("the IFC has no site coordinates, upload it with longitude and latitude")?;
    Ok((site.longitude, site.latitude, site.elevation))
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
    input_type: &str,
    placement: &ModelPlacement,
    external_tiler_jar: Option<&Path>,
) -> Result<u64, String> {
    let jar = external_tiler_jar.ok_or_else(|| {
        "TILETOPIA_MAGO_JAR is not set, so there is no external tiler to run".to_string()
    })?;

    let output = mago_command(
        jar,
        &asset_dir.join("input"),
        asset_dir,
        input_type,
        placement,
    )
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
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn argv(command: &Command) -> Vec<&OsStr> {
        command.get_args().collect()
    }

    #[test]
    fn mago_command_carries_crs_and_placement() {
        let placement = ModelPlacement {
            longitude: Some(10.0),
            latitude: Some(20.0),
            crs: Some("3857".to_string()),
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

    #[test]
    fn ifc_goes_to_the_native_tiler_and_dae_to_neither() {
        assert!(matches!(tiler_for("ifc"), Ok(Tiler::Native)));
        assert!(matches!(tiler_for("obj"), Ok(Tiler::External("obj"))));
        assert!(matches!(tiler_for("gml"), Ok(Tiler::External("citygml"))));

        let error = tiler_for("dae").unwrap_err();
        assert!(error.contains("dae"), "{error}");
        assert!(error.contains("native tiler"), "{error}");
        assert!(error.contains("external one"), "{error}");
    }
}
