//! Async job queue for tiling operations.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

use crate::AssetType;
use crate::db::{Database, JobRecord, JobStatus, ModelPlacement};
use crate::webhooks::{WebhookEvent, WebhookQueue};
use tiletopia_store::TileStore;

/// 3D Tiles version asked of the external tiler.
const TILES_VERSION: &str = "1.1";

/// How much of the external tiler's stderr a failed job reports.
const STDERR_LINES_IN_ERROR: usize = 20;

/// What a native mesh job says when nothing tells it where the model sits.
const NO_PLACEMENT_ERROR: &str = "the native mesh tiler has no coordinates for this model: \
     upload it with longitude and latitude";

pub struct JobQueue {
    db: Arc<Database>,
    data_dir: PathBuf,
    #[allow(dead_code)]
    store: Arc<dyn TileStore>,
    external_tiler_jar: Option<PathBuf>,
    webhooks: Arc<WebhookQueue>,
}

impl JobQueue {
    pub fn new(
        db: Arc<Database>,
        data_dir: PathBuf,
        store: Arc<dyn TileStore>,
        external_tiler_jar: Option<PathBuf>,
        webhooks: Arc<WebhookQueue>,
    ) -> Self {
        Self {
            db,
            data_dir,
            store,
            external_tiler_jar,
            webhooks,
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

    /// Write the settled job back and tell the subscribers. Every tiling job
    /// ends here, so this is the one place the two job events are emitted from.
    async fn finish(&self, mut job: JobRecord, outcome: Result<u64, String>) {
        job.completed_at = Some(chrono::Utc::now());
        let event = match outcome {
            Ok(tiles) => {
                job.status = JobStatus::Done;
                job.progress = 1.0;
                job.tiles_written = tiles;
                tracing::info!("Job {} completed: {} tiles", job.id, tiles);
                WebhookEvent::JobCompleted
            }
            Err(error) => {
                job.status = JobStatus::Failed;
                tracing::error!("Job {} failed: {}", job.id, error);
                job.error = Some(error);
                WebhookEvent::JobFailed
            }
        };
        let _ = self.db.update_job(&job).await;

        self.webhooks
            .emit(
                event,
                serde_json::json!({
                    "job_id": job.id,
                    "asset_id": job.asset_id,
                    "completed_at": job.completed_at,
                    "tiles_written": job.tiles_written,
                    "error": job.error,
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
        AssetType::PointCloud => tile_point_cloud(input_path, asset_dir, placement.crs.as_deref()),
        AssetType::Model | AssetType::Vector => {
            let extension = input_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_lowercase();
            match tiler_for(&extension)? {
                Tiler::Native { source_is_z_up } => {
                    tile_native(input_path, asset_dir, &extension, source_is_z_up, placement)
                }
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
enum Tiler {
    /// This repository's own readers and mesh tiler. `source_is_z_up` says the
    /// format's own coordinates are z-up, so the written glTF gets rotated.
    Native { source_is_z_up: bool },
    /// mago-3d-tiler, carrying the `-it` value for the format.
    External(&'static str),
}

/// Which tiler takes this file extension. Mesh formats are all native, and so
/// is an IFC. Vector formats are mago only, because the native readers take no
/// vector input.
fn tiler_for(extension: &str) -> Result<Tiler, String> {
    let mesh = |source_is_z_up| Ok(Tiler::Native { source_is_z_up });
    match extension {
        "ifc" => mesh(true),
        "gltf" | "glb" => mesh(false),
        "obj" => mesh(false),
        // the reader turns the file's GlobalSettings UpAxis into y up itself
        "fbx" => mesh(false),
        "gml" => mesh(true),
        "geojson" => Ok(Tiler::External("geojson")),
        "gpkg" => Ok(Tiler::External("gpkg")),
        "kml" => Ok(Tiler::External("kml")),
        other => Err(format!(
            "{other}: neither the native tiler nor the external one takes this format"
        )),
    }
}

/// Tile a mesh with this repository's own readers and mesh tiler. The upload's
/// `crs` is ignored here: the source coordinates are metres, placed by
/// longitude and latitude alone.
fn tile_native(
    input_path: &Path,
    asset_dir: &Path,
    extension: &str,
    source_is_z_up: bool,
    placement: &ModelPlacement,
) -> Result<u64, String> {
    let (longitude, latitude, height) = mesh_origin(input_path, extension, placement)?;

    let read = tiletopia_ingest::read_mesh(input_path).map_err(|e| e.to_string())?;
    if read.is_empty() {
        return Err(format!("the {extension} holds no geometry"));
    }

    let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> =
        read.into_iter().map(Into::into).collect();

    // the tileset's frame is the ENU one the root transform names, so a z-up
    // source rotates the written glTF only
    let config = tiletopia_core::mesh_tiler::MeshTilingConfig {
        root_transform: Some(tiletopia_core::spatial::enu_to_ecef_matrix(
            latitude.to_radians(),
            longitude.to_radians(),
            height,
        )),
        content_y_up: source_is_z_up,
        ..Default::default()
    };
    let stats = tiletopia_core::mesh_tiler::tile_meshes(&meshes, asset_dir, &config)
        .map_err(|e| e.to_string())?;
    Ok(stats.tile_count as u64)
}

/// Longitude, latitude and height the mesh's local coordinates sit at. The
/// upload's placement wins, at height 0, and an IFC's own site answers otherwise.
fn mesh_origin(
    input_path: &Path,
    extension: &str,
    placement: &ModelPlacement,
) -> Result<(f64, f64, f64), String> {
    if let (Some(longitude), Some(latitude)) = (placement.longitude, placement.latitude) {
        return Ok((longitude, latitude, 0.0));
    }

    // an IFC is always tiled natively, so the jar is no answer for one
    if extension != "ifc" {
        return Err(NO_PLACEMENT_ERROR.to_string());
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
        assert_eq!(tiler_for("gml").unwrap(), Z_UP);
        assert_eq!(tiler_for("geojson").unwrap(), Tiler::External("geojson"));
        assert_eq!(tiler_for("gpkg").unwrap(), Tiler::External("gpkg"));
        assert_eq!(tiler_for("kml").unwrap(), Tiler::External("kml"));
    }

    #[test]
    fn dae_and_txt_go_to_neither_tiler() {
        for extension in ["dae", "txt"] {
            let error = tiler_for(extension).unwrap_err();
            assert!(error.contains(extension), "{error}");
            assert!(error.contains("native tiler"), "{error}");
            assert!(error.contains("external one"), "{error}");
        }
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
}
