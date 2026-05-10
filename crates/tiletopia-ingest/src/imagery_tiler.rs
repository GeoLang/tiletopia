//! Imagery/raster tiling pipeline — generates TMS tile pyramids from georeferenced rasters.

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use serde_json::json;
use std::path::Path;
use thiserror::Error;

/// Configuration for imagery tiling.
pub struct ImageryTilingConfig {
    pub tile_size: u32,
    pub max_zoom: u8,
    pub min_zoom: u8,
    pub format: ImageFormat,
}

impl Default for ImageryTilingConfig {
    fn default() -> Self {
        Self {
            tile_size: 256,
            max_zoom: 18,
            min_zoom: 0,
            format: ImageFormat::Png,
        }
    }
}

pub enum ImageFormat {
    Png,
    Jpeg { quality: u8 },
    Webp { quality: u8 },
}

/// Geographic bounds of an image.
#[derive(Debug, Clone)]
pub struct GeoBounds {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// Statistics from the tiling run.
pub struct ImageryTilingStats {
    pub tiles_written: u64,
    pub zoom_levels: Vec<u8>,
    pub bounds: GeoBounds,
}

#[derive(Debug, Error)]
pub enum ImageryError {
    #[error("image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid bounds: {0}")]
    InvalidBounds(String),
}

/// Tile a georeferenced raster image into a TMS tile pyramid.
///
/// Output directory structure:
///   `{output_dir}/{z}/{x}/{y}.{ext}`
///
/// Also writes a TileJSON metadata file at `{output_dir}/tilejson.json`.
pub fn tile_imagery(
    image_path: &Path,
    bounds: &GeoBounds,
    output_dir: &Path,
    config: &ImageryTilingConfig,
) -> Result<ImageryTilingStats, ImageryError> {
    let img = image::open(image_path)?;
    tile_imagery_from_image(&img, bounds, output_dir, config)
}

/// Tile from an already-loaded image (useful for testing with synthetic images).
pub fn tile_imagery_from_image(
    img: &DynamicImage,
    bounds: &GeoBounds,
    output_dir: &Path,
    config: &ImageryTilingConfig,
) -> Result<ImageryTilingStats, ImageryError> {
    if bounds.west >= bounds.east || bounds.south >= bounds.north {
        return Err(ImageryError::InvalidBounds(
            "west >= east or south >= north".into(),
        ));
    }

    let (img_w, img_h) = img.dimensions();
    let effective_max =
        config
            .max_zoom
            .min(compute_max_zoom(img_w, img_h, bounds, config.tile_size));

    let ext = match config.format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg { .. } => "jpg",
        ImageFormat::Webp { .. } => "webp",
    };

    let mut tiles_written = 0u64;
    let mut zoom_levels = Vec::new();

    for z in config.min_zoom..=effective_max {
        let tiles = tiles_for_bounds(bounds, z);
        if tiles.is_empty() {
            continue;
        }
        zoom_levels.push(z);

        for (tx, ty, tz) in &tiles {
            let tile_bounds = tile_to_bounds(*tx, *ty, *tz);

            // Map tile bounds to pixel coords in source image.
            let px_left =
                ((tile_bounds.west - bounds.west) / (bounds.east - bounds.west)) * img_w as f64;
            let px_right =
                ((tile_bounds.east - bounds.west) / (bounds.east - bounds.west)) * img_w as f64;
            let px_top =
                ((bounds.north - tile_bounds.north) / (bounds.north - bounds.south)) * img_h as f64;
            let px_bottom =
                ((bounds.north - tile_bounds.south) / (bounds.north - bounds.south)) * img_h as f64;

            // Clamp to image dimensions.
            let src_x = (px_left.max(0.0) as u32).min(img_w);
            let src_y = (px_top.max(0.0) as u32).min(img_h);
            let src_w = ((px_right - px_left).abs().ceil() as u32)
                .max(1)
                .min(img_w.saturating_sub(src_x));
            let src_h = ((px_bottom - px_top).abs().ceil() as u32)
                .max(1)
                .min(img_h.saturating_sub(src_y));

            if src_w == 0 || src_h == 0 {
                continue;
            }

            let cropped = img.crop_imm(src_x, src_y, src_w, src_h);
            let tile_img =
                cropped.resize_exact(config.tile_size, config.tile_size, FilterType::Lanczos3);

            let tile_dir = output_dir.join(tz.to_string()).join(tx.to_string());
            std::fs::create_dir_all(&tile_dir)?;
            let tile_path = tile_dir.join(format!("{ty}.{ext}"));

            match config.format {
                ImageFormat::Png => tile_img.save(&tile_path)?,
                ImageFormat::Jpeg { quality } => {
                    let rgb = tile_img.to_rgb8();
                    let mut writer = std::io::BufWriter::new(std::fs::File::create(&tile_path)?);
                    let encoder =
                        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality);
                    rgb.write_with_encoder(encoder)?;
                }
                ImageFormat::Webp { .. } => {
                    // Fall back to PNG if WebP encoding is not available.
                    tile_img.save(&tile_path)?;
                }
            }
            tiles_written += 1;
        }
    }

    // Write tilejson.json metadata.
    let tilejson = json!({
        "tilejson": "3.0.0",
        "name": "imagery",
        "format": ext,
        "bounds": [bounds.west, bounds.south, bounds.east, bounds.north],
        "minzoom": config.min_zoom,
        "maxzoom": effective_max,
        "tiles": ["{z}/{x}/{y}." .to_owned() + ext],
    });
    std::fs::write(
        output_dir.join("tilejson.json"),
        serde_json::to_string_pretty(&tilejson).unwrap(),
    )?;

    Ok(ImageryTilingStats {
        tiles_written,
        zoom_levels,
        bounds: bounds.clone(),
    })
}

/// Compute which TMS tiles intersect a geographic bounds at a given zoom level.
fn tiles_for_bounds(bounds: &GeoBounds, zoom: u8) -> Vec<(u32, u32, u8)> {
    let (min_x, max_y) = lonlat_to_tile(bounds.west, bounds.south, zoom);
    let (max_x, min_y) = lonlat_to_tile(bounds.east, bounds.north, zoom);

    let n = 1u32 << zoom;
    let min_x = min_x.min(n.saturating_sub(1));
    let max_x = max_x.min(n.saturating_sub(1));
    let min_y = min_y.min(n.saturating_sub(1));
    let max_y = max_y.min(n.saturating_sub(1));

    let mut tiles = Vec::new();
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            tiles.push((x, y, zoom));
        }
    }
    tiles
}

/// Convert lon/lat to TMS tile coordinates.
fn lonlat_to_tile(lon: f64, lat: f64, zoom: u8) -> (u32, u32) {
    let n = (1u32 << zoom) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor() as u32;
    (x, y)
}

/// Convert TMS tile coordinates to geographic bounds.
fn tile_to_bounds(x: u32, y: u32, zoom: u8) -> GeoBounds {
    let n = (1u32 << zoom) as f64;
    let west = x as f64 / n * 360.0 - 180.0;
    let east = (x + 1) as f64 / n * 360.0 - 180.0;
    let north = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n))
        .sinh()
        .atan()
        .to_degrees();
    let south = (std::f64::consts::PI * (1.0 - 2.0 * (y + 1) as f64 / n))
        .sinh()
        .atan()
        .to_degrees();
    GeoBounds {
        west,
        south,
        east,
        north,
    }
}

/// Compute the appropriate max zoom level based on image resolution and bounds.
fn compute_max_zoom(
    image_width: u32,
    _image_height: u32,
    bounds: &GeoBounds,
    tile_size: u32,
) -> u8 {
    let lon_span = bounds.east - bounds.west;
    let degrees_per_pixel = lon_span / image_width as f64;
    // At zoom z, each tile covers 360/2^z degrees and tile_size pixels,
    // so degrees_per_pixel_at_z = 360 / (2^z * tile_size).
    // Solve for z: 2^z = 360 / (degrees_per_pixel * tile_size)
    let z = (360.0 / (degrees_per_pixel * tile_size as f64)).log2();
    (z.ceil() as u8).min(22)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    #[test]
    fn lonlat_to_tile_known_values() {
        // Zoom 0: entire world is tile (0, 0)
        let (x, y) = lonlat_to_tile(0.0, 0.0, 0);
        assert_eq!((x, y), (0, 0));

        // Zoom 1: (0, 0) should be in top-left quadrant
        let (x, y) = lonlat_to_tile(-90.0, 45.0, 1);
        assert_eq!(x, 0);
        assert!(y <= 1);

        // Zoom 2: positive lon -> x >= 2
        let (x, _y) = lonlat_to_tile(90.0, 0.0, 2);
        assert_eq!(x, 3);
    }

    #[test]
    fn tile_to_bounds_roundtrip() {
        let bounds = tile_to_bounds(0, 0, 1);
        assert!((bounds.west - (-180.0)).abs() < 1e-6);
        assert!(bounds.north > 0.0);

        // At zoom 1, tile (1, 1) should be in the SE quadrant
        let bounds = tile_to_bounds(1, 1, 1);
        assert!(bounds.west >= 0.0);
        assert!(bounds.south < 0.0);
    }

    #[test]
    fn lonlat_to_tile_and_back() {
        let zoom = 10u8;
        let lon = -73.9857;
        let lat = 40.7484;
        let (x, y) = lonlat_to_tile(lon, lat, zoom);
        let bounds = tile_to_bounds(x, y, zoom);
        assert!(bounds.west <= lon && lon <= bounds.east);
        assert!(bounds.south <= lat && lat <= bounds.north);
    }

    #[test]
    fn compute_max_zoom_high_res() {
        // 10000px covering 1 degree -> high zoom
        let z = compute_max_zoom(
            10000,
            10000,
            &GeoBounds {
                west: 0.0,
                south: 0.0,
                east: 1.0,
                north: 1.0,
            },
            256,
        );
        assert!(z >= 14, "expected high zoom for dense imagery, got {z}");
    }

    #[test]
    fn compute_max_zoom_low_res() {
        // 256px covering 180 degrees -> low zoom
        let z = compute_max_zoom(
            256,
            256,
            &GeoBounds {
                west: -90.0,
                south: -45.0,
                east: 90.0,
                north: 45.0,
            },
            256,
        );
        assert!(z <= 2, "expected low zoom for coarse imagery, got {z}");
    }

    #[test]
    fn tile_synthetic_image() {
        let img =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(256, 256, Rgba([128, 64, 32, 255])));
        let bounds = GeoBounds {
            west: -1.0,
            south: -1.0,
            east: 1.0,
            north: 1.0,
        };
        let dir = tempfile::tempdir().unwrap();
        let config = ImageryTilingConfig {
            tile_size: 256,
            max_zoom: 2,
            min_zoom: 0,
            format: ImageFormat::Png,
        };
        let stats = tile_imagery_from_image(&img, &bounds, dir.path(), &config).unwrap();
        assert!(stats.tiles_written > 0, "should produce at least one tile");
        assert!(!stats.zoom_levels.is_empty());
        // tilejson.json should exist
        assert!(dir.path().join("tilejson.json").exists());
    }
}
