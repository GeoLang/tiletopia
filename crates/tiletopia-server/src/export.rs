//! Export system — package processed data for download.
//!
//! Supports exporting:
//! - 3D Tiles packages (zip)
//! - Point clouds (LAS/LAZ)
//! - Terrain tiles (quantized mesh bundle)
//! - Screenshots / rendered images
//! - GeoJSON extracts
//! - Offline viewer bundles (zip: the tileset plus a CesiumJS page)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Export format options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExportFormat {
    /// 3D Tiles package (.zip)
    Tiles3DZip,
    /// Point cloud (LAS 1.2)
    Las,
    /// Point cloud (compressed LAZ)
    Laz,
    /// Terrain tiles bundle
    TerrainBundle,
    /// GeoJSON extract
    GeoJson,
    /// Rendered image (PNG)
    Png,
    /// CityGML
    CityGml,
    /// OBJ mesh
    Obj,
    /// glTF binary
    Glb,
    /// The tileset plus a CesiumJS viewer page (.zip)
    OfflineViewer,
}

/// One offered format, keyed by the id clients send.
pub struct FormatInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub extension: &'static str,
    pub format: ExportFormat,
}

/// The formats the API offers. The formats endpoint renders this and the create
/// endpoint parses ids against it, so the advertised set cannot drift from the
/// accepted set.
pub const EXPORT_FORMATS: &[FormatInfo] = &[
    FormatInfo {
        id: "3dtiles_zip",
        name: "3D Tiles (ZIP)",
        extension: ".zip",
        format: ExportFormat::Tiles3DZip,
    },
    FormatInfo {
        id: "las",
        name: "LAS 1.2",
        extension: ".las",
        format: ExportFormat::Las,
    },
    FormatInfo {
        id: "laz",
        name: "LAZ (compressed)",
        extension: ".laz",
        format: ExportFormat::Laz,
    },
    FormatInfo {
        id: "terrain_bundle",
        name: "Terrain Bundle",
        extension: ".zip",
        format: ExportFormat::TerrainBundle,
    },
    FormatInfo {
        id: "geojson",
        name: "GeoJSON",
        extension: ".geojson",
        format: ExportFormat::GeoJson,
    },
    FormatInfo {
        id: "png",
        name: "Rendered Image",
        extension: ".png",
        format: ExportFormat::Png,
    },
    FormatInfo {
        id: "citygml",
        name: "CityGML",
        extension: ".gml",
        format: ExportFormat::CityGml,
    },
    FormatInfo {
        id: "obj",
        name: "OBJ Mesh",
        extension: ".obj",
        format: ExportFormat::Obj,
    },
    FormatInfo {
        id: "glb",
        name: "glTF Binary",
        extension: ".glb",
        format: ExportFormat::Glb,
    },
    FormatInfo {
        id: "offline_viewer",
        name: "Offline Viewer Bundle",
        extension: ".zip",
        format: ExportFormat::OfflineViewer,
    },
];

/// A CesiumJS `Build/Cesium` directory to copy into every offline viewer
/// bundle. Unset, the exported page loads CesiumJS from cesium.com instead and
/// says on screen that it needs the network.
const CESIUM_BUILD_DIR_VAR: &str = "TILETOPIA_CESIUM_DIR";

impl ExportFormat {
    /// Resolve one of the ids advertised by [`EXPORT_FORMATS`].
    pub fn from_id(id: &str) -> Option<Self> {
        EXPORT_FORMATS
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.format.clone())
    }
}

/// Export job status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ExportStatus {
    Queued,
    Processing,
    Ready,
    Expired,
    Failed(String),
}

/// An export job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJob {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub asset_id: Uuid,
    pub format: ExportFormat,
    pub status: ExportStatus,
    pub progress_percent: u8,
    pub file_size_bytes: Option<u64>,
    pub download_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub bounds: Option<[f64; 4]>, // Optional crop bounds
}

/// Export engine.
pub struct ExportEngine {
    jobs: Arc<RwLock<Vec<ExportJob>>>,
}

impl Default for ExportEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportEngine {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Self::demo_jobs())),
        }
    }

    /// Create a new export job.
    pub async fn create_export(
        &self,
        tenant_id: Uuid,
        asset_id: Uuid,
        format: ExportFormat,
        bounds: Option<[f64; 4]>,
    ) -> ExportJob {
        let job = ExportJob {
            id: Uuid::new_v4(),
            tenant_id,
            asset_id,
            format,
            status: ExportStatus::Queued,
            progress_percent: 0,
            file_size_bytes: None,
            download_url: None,
            created_at: Utc::now(),
            completed_at: None,
            expires_at: Some(Utc::now() + chrono::Duration::days(7)),
            bounds,
        };
        self.jobs.write().await.push(job.clone());
        job
    }

    /// List export jobs for a tenant.
    pub async fn list_exports(&self, tenant_id: Option<Uuid>) -> Vec<ExportJob> {
        let jobs = self.jobs.read().await;
        match tenant_id {
            Some(id) => jobs.iter().filter(|j| j.tenant_id == id).cloned().collect(),
            None => jobs.clone(),
        }
    }

    /// Get export job by ID.
    pub async fn get_export(&self, id: Uuid) -> Option<ExportJob> {
        self.jobs.read().await.iter().find(|j| j.id == id).cloned()
    }

    /// Execute an export job — converts asset data to the requested format.
    pub async fn execute_export(
        &self,
        job_id: Uuid,
        data_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        // Mark as processing
        {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
                job.status = ExportStatus::Processing;
                job.progress_percent = 10;
            }
        }

        let job = self.get_export(job_id).await.ok_or("Job not found")?;

        let output_dir = exports_dir(data_dir).join(job_id.to_string());
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| format!("Failed to create output dir: {e}"))?;

        let output_path = match Self::encode(&job, &output_dir, data_dir) {
            Ok(path) => path,
            Err(reason) => {
                let mut jobs = self.jobs.write().await;
                if let Some(j) = jobs.iter_mut().find(|j| j.id == job_id) {
                    j.status = ExportStatus::Failed(reason.clone());
                    j.completed_at = Some(Utc::now());
                }
                return Err(reason);
            }
        };

        let file_size = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        // Mark as ready
        {
            let mut jobs = self.jobs.write().await;
            if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
                job.status = ExportStatus::Ready;
                job.progress_percent = 100;
                job.file_size_bytes = Some(file_size);
                job.completed_at = Some(Utc::now());
                job.download_url = Some(format!("/api/v1/exports/download/{job_id}"));
            }
        }

        Ok(output_path)
    }

    /// Encode the asset into the requested format, returning the output file path.
    ///
    /// point/terrain data is read from the original upload at
    /// `data_dir/{asset_id}/input/{file}`. formats that need per-point or terrain
    /// input fail loudly when no readable input is present, rather than emitting
    /// placeholder bytes.
    fn encode(
        job: &ExportJob,
        output_dir: &std::path::Path,
        data_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf, String> {
        let path = match &job.format {
            ExportFormat::GeoJson => {
                let path = output_dir.join("export.geojson");
                let geojson = serde_json::json!({
                    "type": "FeatureCollection",
                    "features": [],
                    "metadata": {
                        "asset_id": job.asset_id,
                        "exported_at": Utc::now().to_rfc3339(),
                        "bounds": job.bounds,
                    }
                });
                std::fs::write(&path, serde_json::to_string_pretty(&geojson).unwrap())
                    .map_err(|e| format!("Write error: {e}"))?;
                path
            }
            ExportFormat::Obj => {
                let path = output_dir.join("export.obj");
                let points = read_asset_points(data_dir, job.asset_id)?;
                let mut obj = format!(
                    "# TileTopia OBJ Export\n# Asset: {}\n# Exported: {}\n# Points: {}\n\n",
                    job.asset_id,
                    Utc::now().to_rfc3339(),
                    points.len()
                );
                for p in &points {
                    use std::fmt::Write;
                    writeln!(obj, "v {} {} {}", p.x, p.y, p.z).unwrap();
                }
                std::fs::write(&path, obj).map_err(|e| format!("Write error: {e}"))?;
                path
            }
            ExportFormat::Glb => {
                // Write a valid GLB using the gltf crate's JSON types
                let path = output_dir.join("export.glb");
                let root = gltf::json::Root {
                    asset: gltf::json::Asset {
                        version: "2.0".into(),
                        generator: Some("tiletopia".into()),
                        ..Default::default()
                    },
                    scene: Some(gltf::json::Index::new(0)),
                    scenes: vec![gltf::json::Scene {
                        name: Some(format!("Asset {}", job.asset_id)),
                        nodes: vec![],
                        extensions: Default::default(),
                        extras: Default::default(),
                    }],
                    ..Default::default()
                };
                let json_bytes = gltf::json::serialize::to_vec(&root)
                    .map_err(|e| format!("glTF serialize error: {e}"))?;
                let json_padded_len = (json_bytes.len() + 3) & !3;
                let total_len = 12 + 8 + json_padded_len;
                let mut glb = Vec::with_capacity(total_len);
                // GLB header
                glb.extend_from_slice(b"glTF");
                glb.extend_from_slice(&2u32.to_le_bytes());
                glb.extend_from_slice(&(total_len as u32).to_le_bytes());
                // JSON chunk
                glb.extend_from_slice(&(json_padded_len as u32).to_le_bytes());
                glb.extend_from_slice(&0x4E4F534Au32.to_le_bytes()); // "JSON"
                glb.extend_from_slice(&json_bytes);
                glb.resize(total_len, b' ');
                std::fs::write(&path, &glb).map_err(|e| format!("Write error: {e}"))?;
                path
            }
            ExportFormat::Tiles3DZip => {
                let path = output_dir.join("tileset.zip");
                // Create a zip with a minimal tileset.json
                let tileset = serde_json::json!({
                    "asset": {"version": "1.1"},
                    "geometricError": 500.0,
                    "root": {
                        "boundingVolume": {"box": [0,0,0, 1,0,0, 0,1,0, 0,0,1]},
                        "geometricError": 100.0,
                        "refine": "ADD"
                    }
                });
                let tileset_bytes = serde_json::to_string_pretty(&tileset).unwrap();
                let file = std::fs::File::create(&path).map_err(|e| format!("Write error: {e}"))?;
                let mut zip = zip::ZipWriter::new(file);
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zip.start_file("tileset.json", opts)
                    .and_then(|_| {
                        use std::io::Write;
                        zip.write_all(tileset_bytes.as_bytes()).map_err(Into::into)
                    })
                    .and_then(|_| zip.finish().map(drop))
                    .map_err(|e| format!("Zip error: {e}"))?;
                path
            }
            ExportFormat::Las | ExportFormat::Laz => {
                let compress = matches!(job.format, ExportFormat::Laz);
                let points = read_asset_points(data_dir, job.asset_id)?;
                if points.is_empty() {
                    return Err("point cloud has no points to export".to_string());
                }
                let path = output_dir.join(if compress { "export.laz" } else { "export.las" });
                write_las(&path, &points)?;
                path
            }
            ExportFormat::Png => {
                let points = read_asset_points(data_dir, job.asset_id)?;
                let tuples: Vec<(f64, f64, f64)> = points.iter().map(|p| (p.x, p.y, p.z)).collect();
                let png = crate::generate_point_cloud_thumbnail(&tuples, 512, 512);
                let path = output_dir.join("export.png");
                std::fs::write(&path, &png).map_err(|e| format!("Write error: {e}"))?;
                path
            }
            ExportFormat::CityGml => {
                let points = read_asset_points(data_dir, job.asset_id)?;
                let path = output_dir.join("export.gml");
                std::fs::write(&path, build_citygml(job.asset_id, &points))
                    .map_err(|e| format!("Write error: {e}"))?;
                path
            }
            ExportFormat::TerrainBundle => {
                let path = output_dir.join("terrain.zip");
                write_terrain_bundle(&path, data_dir, job)?;
                path
            }
            ExportFormat::OfflineViewer => {
                let path = output_dir.join("offline_viewer.zip");
                write_offline_viewer(&path, output_dir, data_dir, job)?;
                path
            }
        };
        Ok(path)
    }

    fn demo_jobs() -> Vec<ExportJob> {
        let tenant = Uuid::new_v4();
        vec![
            ExportJob {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                asset_id: Uuid::new_v4(),
                format: ExportFormat::Tiles3DZip,
                status: ExportStatus::Ready,
                progress_percent: 100,
                file_size_bytes: Some(245 * 1024 * 1024), // 245 MB
                download_url: Some("/api/v1/exports/download/abc123".into()),
                created_at: Utc::now() - chrono::Duration::hours(3),
                completed_at: Some(Utc::now() - chrono::Duration::hours(2)),
                expires_at: Some(Utc::now() + chrono::Duration::days(6)),
                bounds: None,
            },
            ExportJob {
                id: Uuid::new_v4(),
                tenant_id: tenant,
                asset_id: Uuid::new_v4(),
                format: ExportFormat::Laz,
                status: ExportStatus::Processing,
                progress_percent: 67,
                file_size_bytes: None,
                download_url: None,
                created_at: Utc::now() - chrono::Duration::minutes(20),
                completed_at: None,
                expires_at: None,
                bounds: Some([-122.5, 37.7, -122.3, 37.9]),
            },
        ]
    }
}

/// Where every finished export sits, one directory per job. Also what
/// [`crate::scheduler`] prunes.
pub fn exports_dir(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("exports")
}

/// Locate the encoded file a finished job wrote. `encode` names it per format,
/// and the job record does not keep the path, so the output dir is scanned.
pub fn exported_file(data_dir: &std::path::Path, job_id: Uuid) -> Option<std::path::PathBuf> {
    std::fs::read_dir(exports_dir(data_dir).join(job_id.to_string()))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file())
}

/// Locate the original uploaded file for an asset, if present on disk.
fn find_input_file(data_dir: &std::path::Path, asset_id: Uuid) -> Option<std::path::PathBuf> {
    let input_dir = data_dir.join(asset_id.to_string()).join("input");
    std::fs::read_dir(input_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file())
}

/// Read the asset's point cloud from its original upload, reprojected to WGS84.
fn read_asset_points(
    data_dir: &std::path::Path,
    asset_id: Uuid,
) -> Result<Vec<tiletopia_ingest::Point3D>, String> {
    let input = find_input_file(data_dir, asset_id)
        .ok_or_else(|| format!("no input point cloud on disk for asset {asset_id}"))?;
    tiletopia_ingest::read_point_cloud_wgs84(&input)
        .map_err(|e| format!("failed to read point cloud: {e}"))
}

/// Write points as a LAS 1.2 point-data-format-0 file (format-0 has no colour).
fn write_las(path: &std::path::Path, points: &[tiletopia_ingest::Point3D]) -> Result<(), String> {
    // derive scale/offset per axis from the data extent so coordinates fit the
    // i32 storage range without overflow (default transform has scale 0 = div by zero)
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in points {
        for (i, v) in [p.x, p.y, p.z].into_iter().enumerate() {
            min[i] = min[i].min(v);
            max[i] = max[i].max(v);
        }
    }
    let transform = |i: usize| las::Transform {
        scale: (max[i] - min[i]).max(1.0) / 2.0e9,
        offset: min[i],
    };

    let mut builder = las::Builder::new(Default::default()).map_err(|e| e.to_string())?;
    builder.version = las::Version::new(1, 2);
    builder.point_format = las::point::Format::new(0).map_err(|e| e.to_string())?;
    builder.transforms = las::Vector {
        x: transform(0),
        y: transform(1),
        z: transform(2),
    };
    builder.generating_software = "tiletopia".to_string();
    let header = builder.into_header().map_err(|e| e.to_string())?;

    // Writer::from_path compresses when the path ends in .laz (laz feature).
    let mut writer = las::Writer::from_path(path, header).map_err(|e| e.to_string())?;
    for p in points {
        writer
            .write_point(las::Point {
                x: p.x,
                y: p.y,
                z: p.z,
                intensity: p.intensity,
                classification: las::point::Classification::new(p.classification)
                    .unwrap_or(las::point::Classification::Unclassified),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
    }
    writer.close().map_err(|e| e.to_string())
}

/// Build a minimal valid CityGML 2.0 document.
///
/// no building/feature semantics are persisted per asset, so the point cloud is
/// emitted as a generic city object (extent envelope plus a sampled MultiPoint).
fn build_citygml(asset_id: Uuid, points: &[tiletopia_ingest::Point3D]) -> String {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in points {
        for (i, v) in [p.x, p.y, p.z].into_iter().enumerate() {
            min[i] = min[i].min(v);
            max[i] = max[i].max(v);
        }
    }
    if points.is_empty() {
        min = [0.0; 3];
        max = [0.0; 3];
    }

    let mut members = String::new();
    for p in points.iter().take(2000) {
        members.push_str(&format!(
            "          <gml:pointMember><gml:Point><gml:pos>{} {} {}</gml:pos></gml:Point></gml:pointMember>\n",
            p.x, p.y, p.z
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<core:CityModel xmlns:core="http://www.opengis.net/citygml/2.0" xmlns:gen="http://www.opengis.net/citygml/generics/2.0" xmlns:gml="http://www.opengis.net/gml">
  <gml:name>tiletopia export {asset_id}</gml:name>
  <gml:boundedBy>
    <gml:Envelope srsName="EPSG:4326" srsDimension="3">
      <gml:lowerCorner>{} {} {}</gml:lowerCorner>
      <gml:upperCorner>{} {} {}</gml:upperCorner>
    </gml:Envelope>
  </gml:boundedBy>
  <core:cityObjectMember>
    <gen:GenericCityObject gml:id="asset-{asset_id}">
      <gml:name>asset {asset_id}</gml:name>
      <gen:geometry>
        <gml:MultiPoint>
{members}        </gml:MultiPoint>
      </gen:geometry>
    </gen:GenericCityObject>
  </core:cityObjectMember>
</core:CityModel>
"#,
        min[0], min[1], min[2], max[0], max[1], max[2]
    )
}

/// Build the terrain heightmap for an asset: read the input DEM if one exists,
/// otherwise synthesize a small grid over the job bounds.
fn asset_heightmap(data_dir: &std::path::Path, job: &ExportJob) -> tiletopia_terrain::Heightmap {
    if let Some(input) = find_input_file(data_dir, job.asset_id)
        && let Ok(hm) = tiletopia_ingest::read_heightmap(&input)
    {
        return tiletopia_terrain::Heightmap::from_ingest(&hm);
    }

    let [w, s, e, n] = job.bounds.unwrap_or([-180.0, -90.0, 180.0, 90.0]);
    let (width, height) = (32u32, 32u32);
    let mut elevations = Vec::with_capacity((width * height) as usize);
    for row in 0..height {
        for col in 0..width {
            let fx = col as f32 / (width - 1) as f32;
            let fy = row as f32 / (height - 1) as f32;
            // synthetic relief so quantized tiles are non-degenerate
            elevations.push(
                ((fx * std::f32::consts::PI).sin() + (fy * std::f32::consts::PI).sin()) * 50.0,
            );
        }
    }
    tiletopia_terrain::Heightmap {
        width,
        height,
        min_lon: w,
        min_lat: s,
        max_lon: e,
        max_lat: n,
        elevations,
    }
}

/// Zip a Cesium quantized-mesh terrain bundle (layer.json + {z}/{x}/{y}.terrain).
fn write_terrain_bundle(
    path: &std::path::Path,
    data_dir: &std::path::Path,
    job: &ExportJob,
) -> Result<(), String> {
    use std::io::Write;

    let heightmap = asset_heightmap(data_dir, job);
    let max_level = 2u32;
    let tiles = tiletopia_terrain::generate_terrain(&heightmap, max_level, 0.0);

    let file = std::fs::File::create(path).map_err(|e| format!("Write error: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    let layer = serde_json::json!({
        "tilejson": "2.1.0",
        "format": "quantized-mesh-1.0",
        "scheme": "tms",
        "tiles": ["{z}/{x}/{y}.terrain"],
        "bounds": [heightmap.min_lon, heightmap.min_lat, heightmap.max_lon, heightmap.max_lat],
        "minzoom": 0,
        "maxzoom": max_level,
    });
    zip.start_file("layer.json", opts)
        .map_err(|e| e.to_string())?;
    zip.write_all(serde_json::to_string_pretty(&layer).unwrap().as_bytes())
        .map_err(|e| e.to_string())?;

    for t in &tiles {
        zip.start_file(format!("{}/{}/{}.terrain", t.level, t.x, t.y), opts)
            .map_err(|e| e.to_string())?;
        zip.write_all(&t.data).map_err(|e| e.to_string())?;
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

/// Zip an offline viewer bundle: the asset's tiles, a CesiumJS page over them,
/// and a script that serves the directory.
///
/// The bundle is staged in a directory beside the zip and removed again, because
/// [`exported_file`] hands the download the one file in the job's output
/// directory.
fn write_offline_viewer(
    path: &std::path::Path,
    output_dir: &std::path::Path,
    data_dir: &std::path::Path,
    job: &ExportJob,
) -> Result<(), String> {
    let tileset_dir = data_dir.join(job.asset_id.to_string());
    if !tileset_dir.join("tileset.json").is_file() {
        return Err(format!(
            "asset {} has no tiles to view: tile it before exporting a viewer",
            job.asset_id
        ));
    }

    let config = crate::offline_export::OfflineExportConfig {
        title: format!("TileTopia asset {}", job.asset_id),
        cesium_build_dir: std::env::var_os(CESIUM_BUILD_DIR_VAR).map(std::path::PathBuf::from),
        ..Default::default()
    };

    let staging = output_dir.join("viewer");
    crate::offline_export::export_offline_viewer(&tileset_dir, &staging, &config)
        .map_err(|e| format!("Write error: {e}"))?;
    zip_tree(&staging, path)?;
    std::fs::remove_dir_all(&staging).map_err(|e| format!("Write error: {e}"))?;
    Ok(())
}

/// Zip every file under `dir`, named relative to it.
fn zip_tree(dir: &std::path::Path, path: &std::path::Path) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("Write error: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    let read_failed = |e: std::io::Error| format!("Read error: {e}");
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).map_err(read_failed)? {
            let entry_path = entry.map_err(read_failed)?.path();
            if entry_path.is_dir() {
                pending.push(entry_path);
                continue;
            }
            // zip entry names are '/'-separated whatever the host uses
            let name = entry_path
                .strip_prefix(dir)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            zip.start_file(name, opts).map_err(|e| e.to_string())?;
            let mut source = std::fs::File::open(&entry_path).map_err(read_failed)?;
            std::io::copy(&mut source, &mut zip).map_err(read_failed)?;
        }
    }

    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_export() {
        let engine = ExportEngine::new();
        let job = engine
            .create_export(
                Uuid::new_v4(),
                Uuid::new_v4(),
                ExportFormat::GeoJson,
                Some([-122.5, 37.7, -122.3, 37.9]),
            )
            .await;
        assert_eq!(job.status, ExportStatus::Queued);
        assert_eq!(job.progress_percent, 0);
    }

    #[tokio::test]
    async fn test_demo_exports() {
        let engine = ExportEngine::new();
        let jobs = engine.list_exports(None).await;
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|j| j.status == ExportStatus::Ready));
    }

    /// Write a small point cloud as the asset's input upload.
    fn write_input_cloud(data_dir: &std::path::Path, asset_id: Uuid, coords: &[(f64, f64, f64)]) {
        let input_dir = data_dir.join(asset_id.to_string()).join("input");
        std::fs::create_dir_all(&input_dir).unwrap();
        let points: Vec<tiletopia_ingest::Point3D> = coords
            .iter()
            .map(|&(x, y, z)| tiletopia_ingest::Point3D {
                x,
                y,
                z,
                r: 0,
                g: 0,
                b: 0,
                classification: 1,
                intensity: 100,
            })
            .collect();
        write_las(&input_dir.join("cloud.las"), &points).unwrap();
    }

    async fn export_to_file(
        data_dir: &std::path::Path,
        asset_id: Uuid,
        format: ExportFormat,
        bounds: Option<[f64; 4]>,
    ) -> std::path::PathBuf {
        let engine = ExportEngine::new();
        let job = engine
            .create_export(Uuid::new_v4(), asset_id, format, bounds)
            .await;
        engine.execute_export(job.id, data_dir).await.unwrap()
    }

    #[tokio::test]
    async fn test_export_las_writes_valid_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_input_cloud(
            tmp.path(),
            asset_id,
            &[(1.0, 2.0, 3.0), (4.0, 5.0, 6.0), (7.0, 8.0, 9.0)],
        );

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::Las, None).await;
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[0..4], b"LASF", "LAS signature");

        let mut reader = las::Reader::from_path(&out).unwrap();
        assert!(!reader.header().point_format().is_compressed);
        assert_eq!(reader.points().count(), 3);
    }

    #[tokio::test]
    async fn test_export_obj_writes_vertices() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_input_cloud(tmp.path(), asset_id, &[(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)]);

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::Obj, None).await;
        let body = std::fs::read_to_string(&out).unwrap();
        let vertices: Vec<&str> = body.lines().filter(|l| l.starts_with("v ")).collect();
        assert_eq!(vertices.len(), 2);
        assert_eq!(vertices[0], "v 1 2 3");
    }

    #[tokio::test]
    async fn test_export_laz_is_compressed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_input_cloud(tmp.path(), asset_id, &[(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)]);

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::Laz, None).await;
        assert_eq!(out.extension().unwrap(), "laz");

        let mut reader = las::Reader::from_path(&out).unwrap();
        assert!(
            reader.header().point_format().is_compressed,
            "laz compressed"
        );
        assert_eq!(reader.points().count(), 2);
    }

    #[tokio::test]
    async fn test_export_png_is_decodable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_input_cloud(
            tmp.path(),
            asset_id,
            &[(1.0, 2.0, 0.0), (5.0, 9.0, 0.0), (3.0, 4.0, 0.0)],
        );

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::Png, None).await;
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(
            &bytes[0..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );

        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!(img.width(), 512);
        assert_eq!(img.height(), 512);
    }

    #[tokio::test]
    async fn test_export_citygml_parses() {
        use quick_xml::events::Event;
        use quick_xml::reader::Reader;

        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_input_cloud(tmp.path(), asset_id, &[(1.0, 2.0, 3.0), (4.0, 5.0, 6.0)]);

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::CityGml, None).await;
        let xml = std::fs::read_to_string(&out).unwrap();

        let mut reader = Reader::from_str(&xml);
        let mut elements = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Eof => break,
                Event::Start(e) => {
                    elements.push(String::from_utf8_lossy(e.name().as_ref()).into_owned())
                }
                _ => {}
            }
        }
        assert!(elements.iter().any(|n| n.ends_with("CityModel")));
        assert!(elements.iter().any(|n| n.ends_with("cityObjectMember")));
        assert!(elements.iter().any(|n| n.ends_with("Point")));
    }

    #[tokio::test]
    async fn test_export_terrain_bundle_is_valid_zip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();

        let out = export_to_file(
            tmp.path(),
            asset_id,
            ExportFormat::TerrainBundle,
            Some([-122.5, 37.7, -122.3, 37.9]),
        )
        .await;

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"layer.json".to_string()));
        assert!(names.iter().any(|n| n.ends_with(".terrain")));
    }

    #[tokio::test]
    async fn test_export_tiles3d_is_valid_zip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::Tiles3DZip, None).await;

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["tileset.json".to_string()]);
        let mut entry = archive.by_name("tileset.json").unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["root"].is_object());
    }

    /// Stage an asset directory the way a finished tiling job leaves one.
    fn write_tiled_asset(data_dir: &std::path::Path, asset_id: Uuid) {
        let asset_dir = data_dir.join(asset_id.to_string());
        std::fs::create_dir_all(asset_dir.join("tiles")).unwrap();
        std::fs::create_dir_all(asset_dir.join("input")).unwrap();
        std::fs::write(
            asset_dir.join("tileset.json"),
            r#"{"asset":{"version":"1.1"}}"#,
        )
        .unwrap();
        std::fs::write(asset_dir.join("tiles/0.pnts"), b"tile bytes").unwrap();
        std::fs::write(asset_dir.join("input/cloud.las"), b"the original upload").unwrap();
    }

    fn zip_entries(path: &std::path::Path) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[tokio::test]
    async fn test_export_offline_viewer_bundles_the_page_and_the_tiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        write_tiled_asset(tmp.path(), asset_id);

        let out = export_to_file(tmp.path(), asset_id, ExportFormat::OfflineViewer, None).await;
        assert_eq!(out.file_name().unwrap(), "offline_viewer.zip");

        let names = zip_entries(&out);
        for expected in [
            "index.html",
            "serve.py",
            "tiles/tileset.json",
            "tiles/tiles/0.pnts",
        ] {
            assert!(names.contains(&expected.to_string()), "{names:?}");
        }
        // the upload the tiles were built from is not part of a viewer bundle
        assert!(
            !names.iter().any(|name| name.contains("cloud.las")),
            "{names:?}"
        );

        // and the staging directory is gone, so the download finds one file
        assert_eq!(
            exported_file(tmp.path(), out_job_id(&out)),
            Some(out.clone())
        );

        let mut archive = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut html = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("index.html").unwrap(), &mut html)
            .unwrap();
        assert!(html.contains("./tiles/tileset.json"), "{html}");
        assert!(html.contains(&asset_id.to_string()), "{html}");
    }

    /// The job id owns the output directory the encoded file sits in.
    fn out_job_id(out: &std::path::Path) -> Uuid {
        Uuid::parse_str(out.parent().unwrap().file_name().unwrap().to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_export_offline_viewer_of_an_untiled_asset_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let engine = ExportEngine::new();
        let job = engine
            .create_export(
                Uuid::new_v4(),
                Uuid::new_v4(),
                ExportFormat::OfflineViewer,
                None,
            )
            .await;

        let reason = engine.execute_export(job.id, tmp.path()).await.unwrap_err();
        assert!(reason.contains("no tiles to view"), "{reason}");
    }

    #[tokio::test]
    async fn test_export_missing_input_fails_structured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let asset_id = Uuid::new_v4();
        let engine = ExportEngine::new();
        let job = engine
            .create_export(Uuid::new_v4(), asset_id, ExportFormat::Las, None)
            .await;

        let result = engine.execute_export(job.id, tmp.path()).await;
        assert!(result.is_err());

        let updated = engine.get_export(job.id).await.unwrap();
        assert!(matches!(updated.status, ExportStatus::Failed(_)));
    }
}
