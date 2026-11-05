mod node_odm;
mod photo_set;
mod placement;

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use axum::{Json, Router, routing::get};
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::db::JobRecord;
use crate::elevation::JOB_BUDGET_BYTES;
use crate::upload::MAX_UPLOAD_BYTES;

pub use node_odm::{NODE_ODM_URL_ENV, NodeOdm, TIMEOUT_ENV};
pub(crate) use photo_set::check_photo_set;

pub const PHOTO_SET_EXTENSION: &str = "zip";

const RECONSTRUCTION_DIR: &str = "reconstruction";
const RESULT_ZIP: &str = "all.zip";
pub(crate) const DELETED_ASSET_ERROR: &str = "the asset was deleted during this job";
// odm writes the textured mesh and its pages here, and its utm zone and offset to coords.txt
const TEXTURING_DIR: &str = "odm_texturing";
const TEXTURED_MODEL: &str = "odm_textured_model_geo.obj";
const COORDS_FILE: &str = "odm_georeferencing/coords.txt";
// texture_image drops a texture past 4096 by 4096 pixels of area
const TEXTURE_PAGE_SIDE: u32 = 4096;
const TEXTURE_PAGE_JPEG_QUALITY: u8 = 90;
// image decodes at most four bytes a pixel
const DECODED_BYTES_PER_PIXEL: u64 = 4;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotogrammetryPreset {
    Fast,
    #[default]
    Default,
    HighQuality,
}

struct PresetSettings {
    feature_quality: &'static str,
    point_cloud_quality: &'static str,
    mesh_vertices: u32,
    mesh_octree_depth: u32,
}

impl PhotogrammetryPreset {
    pub const ALL: [Self; 3] = [Self::Fast, Self::Default, Self::HighQuality];

    fn settings(self) -> PresetSettings {
        match self {
            Self::Fast => PresetSettings {
                feature_quality: "medium",
                point_cloud_quality: "low",
                mesh_vertices: 100_000,
                mesh_octree_depth: 10,
            },
            // odm's own defaults
            Self::Default => PresetSettings {
                feature_quality: "high",
                point_cloud_quality: "medium",
                mesh_vertices: 200_000,
                mesh_octree_depth: 11,
            },
            Self::HighQuality => PresetSettings {
                feature_quality: "ultra",
                point_cloud_quality: "high",
                mesh_vertices: 300_000,
                mesh_octree_depth: 12,
            },
        }
    }

    pub fn odm_options(self) -> serde_json::Value {
        let settings = self.settings();
        serde_json::json!([
            { "name": "feature-quality", "value": settings.feature_quality },
            { "name": "pc-quality", "value": settings.point_cloud_quality },
            { "name": "mesh-size", "value": settings.mesh_vertices },
            { "name": "mesh-octree-depth", "value": settings.mesh_octree_depth },
            // odm textures a second, 2.5D mesh for the orthophoto unless told to use the 3D one
            { "name": "use-3dmesh", "value": true },
            { "name": "skip-orthophoto", "value": true },
            { "name": "skip-report", "value": true },
        ])
    }
}

pub fn photogrammetry_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/api/v1/photogrammetry/presets", get(list_presets))
}

async fn list_presets() -> Json<serde_json::Value> {
    let presets: Vec<serde_json::Value> = PhotogrammetryPreset::ALL
        .into_iter()
        .map(|preset| serde_json::json!({ "name": preset, "odm_options": preset.odm_options() }))
        .collect();
    Json(serde_json::json!({ "presets": presets }))
}

pub fn is_photo_set(input_path: &Path) -> bool {
    input_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(PHOTO_SET_EXTENSION))
}

fn stop_if_deleted(cancellation: &CancellationToken) -> Result<(), String> {
    if cancellation.is_cancelled() {
        return Err(DELETED_ASSET_ERROR.to_string());
    }
    Ok(())
}

pub(crate) fn download_reconstruction(
    node: &NodeOdm,
    job: &JobRecord,
    asset_dir: &Path,
    cancellation: &CancellationToken,
    photo_transfer_slot: OwnedSemaphorePermit,
    report_progress: &dyn Fn(f64),
) -> Result<(), String> {
    stop_if_deleted(cancellation)?;
    let work_dir = asset_dir.join(RECONSTRUCTION_DIR);
    let _ = std::fs::remove_dir_all(&work_dir);
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("could not create {}: {e}", work_dir.display()))?;
    let outcome = node_odm::run_task(
        node,
        job,
        &work_dir.join(RESULT_ZIP),
        cancellation,
        photo_transfer_slot,
        report_progress,
    );
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&work_dir);
    }
    outcome
}

pub(crate) fn tile_reconstruction(
    job: &JobRecord,
    asset_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<u64, String> {
    let work_dir = asset_dir.join(RECONSTRUCTION_DIR);
    let outcome = stop_if_deleted(cancellation)
        .and_then(|()| tile_downloaded_result(job, asset_dir, &work_dir, cancellation));
    let _ = std::fs::remove_dir_all(&work_dir);
    outcome
}

fn tile_downloaded_result(
    job: &JobRecord,
    asset_dir: &Path,
    work_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<u64, String> {
    let result_zip = work_dir.join(RESULT_ZIP);
    extract_reconstruction(&result_zip, work_dir, cancellation)?;
    std::fs::remove_file(&result_zip)
        .map_err(|e| format!("could not remove {}: {e}", result_zip.display()))?;

    stop_if_deleted(cancellation)?;
    let texturing_dir = work_dir.join(TEXTURING_DIR);
    shrink_texture_pages(&texturing_dir)?;

    stop_if_deleted(cancellation)?;
    let coords_path = work_dir.join(COORDS_FILE);
    let coords = std::fs::read_to_string(coords_path)
        .map_err(|e| format!("ODM wrote no {COORDS_FILE}, so the model has no placement: {e}"))?;
    let georeference = placement::parse_coords(&coords)?;

    let mut meshes =
        tiletopia_ingest::read_mesh(&texturing_dir.join(TEXTURED_MODEL), JOB_BUDGET_BYTES)
            .map_err(|e| format!("ODM's textured model: {e}"))?;
    if meshes.is_empty() {
        return Err("ODM's textured model holds no geometry".to_string());
    }
    // tiletopia_ingest leaves a mesh untextured when its texture cannot be read
    let has_texture = meshes.iter().any(|mesh| {
        mesh.material
            .as_ref()
            .is_some_and(|material| material.texture.is_some())
    });
    if !has_texture {
        return Err(format!(
            "ODM's {TEXTURED_MODEL} has no readable texture, so the job writes no untextured tiles"
        ));
    }
    let root_transform = placement::place_in_east_north_up(&mut meshes, &georeference)?;

    let meshes: Vec<tiletopia_core::mesh_tiler::MeshData> =
        meshes.into_iter().map(Into::into).collect();
    // gltf is y up and the east-north-up frame is z up
    let config = tiletopia_core::mesh_tiler::MeshTilingConfig {
        root_transform: Some(root_transform),
        content_y_up: true,
        mesh_compression: job.mesh_compression,
        texture_compression: job.texture_compression,
        ..Default::default()
    };
    stop_if_deleted(cancellation)?;
    let stats = tiletopia_core::mesh_tiler::tile_meshes(&meshes, asset_dir, &config)
        .map_err(|e| e.to_string())?;
    Ok(stats.tile_count as u64)
}

fn is_kept_result(path: &Path) -> bool {
    path == Path::new(COORDS_FILE) || path.parent() == Some(Path::new(TEXTURING_DIR))
}

fn extract_reconstruction(
    result_zip: &Path,
    work_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let file = File::open(result_zip)
        .map_err(|e| format!("could not open {}: {e}", result_zip.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|e| format!("NodeODM's result is not a readable zip: {e}"))?;
    let most_bytes = MAX_UPLOAD_BYTES as u64;
    let mut written = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("NodeODM's result entry {index}: {e}"))?;
        if !entry.is_file() {
            continue;
        }
        let path = entry.enclosed_name().ok_or_else(|| {
            format!(
                "NodeODM's result names {}, which leaves its own directory",
                entry.name()
            )
        })?;
        if !is_kept_result(&path) {
            continue;
        }
        stop_if_deleted(cancellation)?;
        let target = work_dir.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let mut out = File::create(&target)
            .map_err(|e| format!("could not create {}: {e}", target.display()))?;
        let allowance = most_bytes - written + 1;
        written += std::io::copy(&mut (&mut entry).take(allowance), &mut out)
            .map_err(|e| format!("NodeODM's result entry {}: {e}", path.display()))?;
        if written > most_bytes {
            return Err(format!(
                "NodeODM's textured model unpacks to more than {most_bytes} bytes"
            ));
        }
    }
    Ok(())
}

fn shrink_texture_pages(texturing_dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(texturing_dir)
        .map_err(|e| format!("ODM wrote no {TEXTURING_DIR}: {e}"))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let is_image = path.extension().is_some_and(|extension| {
            ["png", "jpg", "jpeg"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
        if is_image {
            shrink_texture_page(&path)?;
        }
    }
    Ok(())
}

fn shrink_texture_page(path: &Path) -> Result<(), String> {
    let name = path.display();
    let (width, height) =
        image::image_dimensions(path).map_err(|e| format!("texture page {name}: {e}"))?;
    if width <= TEXTURE_PAGE_SIDE && height <= TEXTURE_PAGE_SIDE {
        return Ok(());
    }
    let decoded_bytes = u64::from(width) * u64::from(height) * DECODED_BYTES_PER_PIXEL;
    if decoded_bytes > JOB_BUDGET_BYTES {
        return Err(format!(
            "texture page {name} is {width}x{height} pixels, which decodes past the \
             {JOB_BUDGET_BYTES} byte job budget"
        ));
    }
    let page = image::open(path).map_err(|e| format!("texture page {name}: {e}"))?;
    let shrunk = page
        .resize(
            TEXTURE_PAGE_SIDE,
            TEXTURE_PAGE_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .into_rgb8();
    drop(page);
    // texture_image reads the format from the bytes, so the page keeps the name the .mtl gives it
    let mut out = std::io::BufWriter::new(
        File::create(path).map_err(|e| format!("texture page {name}: {e}"))?,
    );
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, TEXTURE_PAGE_JPEG_QUALITY)
        .encode_image(&shrunk)
        .map_err(|e| format!("texture page {name}: {e}"))?;
    out.flush().map_err(|e| format!("texture page {name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracting_a_result_after_the_delete_leaves_the_removed_directory_removed() {
        let dir = tempfile::tempdir().unwrap();
        let result_zip = dir.path().join(RESULT_ZIP);
        let mut writer = zip::ZipWriter::new(File::create(&result_zip).unwrap());
        writer
            .start_file(COORDS_FILE, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"WGS84 UTM 32N\n500000 10\n").unwrap();
        writer.finish().unwrap();
        let asset_dir = dir.path().join("deleted_asset");
        let cancellation = CancellationToken::new();

        extract_reconstruction(
            &result_zip,
            &asset_dir.join(RECONSTRUCTION_DIR),
            &cancellation,
        )
        .unwrap();
        assert!(
            asset_dir
                .join(RECONSTRUCTION_DIR)
                .join(COORDS_FILE)
                .exists()
        );
        std::fs::remove_dir_all(&asset_dir).unwrap();

        cancellation.cancel();
        let error = extract_reconstruction(
            &result_zip,
            &asset_dir.join(RECONSTRUCTION_DIR),
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, DELETED_ASSET_ERROR);
        assert!(!asset_dir.exists());
    }

    #[test]
    fn presets_map_to_odm_options() {
        let option = |preset: PhotogrammetryPreset, name: &str| {
            preset
                .odm_options()
                .as_array()
                .unwrap()
                .iter()
                .find(|option| option["name"] == name)
                .map(|option| option["value"].clone())
                .unwrap_or_else(|| panic!("{preset:?} has no {name}"))
        };
        let expected = [
            (PhotogrammetryPreset::Fast, "medium", "low", 100_000, 10),
            (PhotogrammetryPreset::Default, "high", "medium", 200_000, 11),
            (
                PhotogrammetryPreset::HighQuality,
                "ultra",
                "high",
                300_000,
                12,
            ),
        ];
        for (preset, features, point_cloud, vertices, depth) in expected {
            assert_eq!(option(preset, "feature-quality"), features);
            assert_eq!(option(preset, "pc-quality"), point_cloud);
            assert_eq!(option(preset, "mesh-size"), vertices);
            assert_eq!(option(preset, "mesh-octree-depth"), depth);
            assert_eq!(option(preset, "use-3dmesh"), true);
            assert_eq!(option(preset, "skip-orthophoto"), true);
        }
    }

    #[test]
    fn preset_names_are_the_ones_uploads_send() {
        let names: Vec<serde_json::Value> = PhotogrammetryPreset::ALL
            .iter()
            .map(|preset| serde_json::json!(preset))
            .collect();
        assert_eq!(names, ["fast", "default", "high_quality"]);
    }

    #[test]
    fn a_texture_page_wider_than_the_page_side_is_shrunk() {
        let dir = tempfile::tempdir().unwrap();
        let page = dir.path().join("page_map_Kd.png");
        image::RgbImage::from_pixel(TEXTURE_PAGE_SIDE + 4, 8, image::Rgb([200, 100, 50]))
            .save(&page)
            .unwrap();
        let small = dir.path().join("small_map_Kd.png");
        image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]))
            .save(&small)
            .unwrap();
        let small_bytes = std::fs::read(&small).unwrap();

        shrink_texture_pages(dir.path()).unwrap();

        let bytes = std::fs::read(&page).unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
        let shrunk = image::load_from_memory(&bytes).unwrap();
        assert_eq!(shrunk.width(), TEXTURE_PAGE_SIDE);
        assert_eq!(std::fs::read(&small).unwrap(), small_bytes);
    }

    #[test]
    fn only_the_textured_model_and_coords_are_kept_from_the_result() {
        assert!(is_kept_result(Path::new(COORDS_FILE)));
        assert!(is_kept_result(Path::new(
            "odm_texturing/odm_textured_model_geo.obj"
        )));
        assert!(!is_kept_result(Path::new(
            "odm_texturing/nir/odm_textured_model_geo.obj"
        )));
        assert!(!is_kept_result(Path::new(
            "odm_orthophoto/odm_orthophoto.tif"
        )));
    }
}
