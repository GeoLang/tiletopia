//! Texture images for mesh readers: probing, re-encoding and path resolution.
//!
//! Every path here degrades to `None` with a warning instead of failing the
//! read, so a mesh with a broken texture still tiles untextured.

use crate::Texture;
use image::ImageFormat;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const PNG_MIME: &str = "image/png";
const JPEG_MIME: &str = "image/jpeg";
/// Independent of the upload cap. 4096² is 16 megapixels.
const MAX_TEXTURE_PIXELS: u64 = 4096 * 4096;
const MAX_TEXTURE_SIDE: u32 = 8192;

/// Read a texture from an image file.
pub fn from_file(path: &Path) -> Option<Texture> {
    match std::fs::read(path) {
        Ok(bytes) => from_bytes(bytes, &path.display().to_string()),
        Err(error) => {
            tracing::warn!(
                "texture {} not read, mesh stays untextured: {error}",
                path.display()
            );
            None
        }
    }
}

/// Build a texture from encoded image bytes. PNG and JPEG go through as they
/// are, any other encoding is decoded and re-encoded as PNG because glTF takes
/// only those two. `source` names the image in warnings.
pub fn from_bytes(bytes: Vec<u8>, source: &str) -> Option<Texture> {
    let reader = match image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!("texture {source} unreadable, mesh stays untextured: {error}");
            return None;
        }
    };
    let format = reader.format();
    let dimensions = match reader.into_dimensions() {
        Ok(dimensions) => dimensions,
        Err(error) => {
            tracing::warn!("texture {source} undecodable, mesh stays untextured: {error}");
            return None;
        }
    };
    if !texture_fits(dimensions.0, dimensions.1, source) {
        return None;
    }

    match format {
        Some(ImageFormat::Png) => Some(Texture {
            image_data: bytes,
            mime_type: PNG_MIME.to_string(),
            width: dimensions.0,
            height: dimensions.1,
        }),
        Some(ImageFormat::Jpeg) => Some(Texture {
            image_data: bytes,
            mime_type: JPEG_MIME.to_string(),
            width: dimensions.0,
            height: dimensions.1,
        }),
        _ => reencode_as_png(&bytes, source),
    }
}

/// Build a texture from raw RGBA8 pixels.
pub fn from_rgba8(pixels: Vec<u8>, width: u32, height: u32, source: &str) -> Option<Texture> {
    if !texture_fits(width, height, source) {
        return None;
    }
    let Some(image) = image::RgbaImage::from_raw(width, height, pixels) else {
        tracing::warn!(
            "texture {source} has fewer pixels than {width}x{height}, mesh stays untextured"
        );
        return None;
    };
    encode_png(&image::DynamicImage::ImageRgba8(image), source)
}

fn texture_fits(width: u32, height: u32, source: &str) -> bool {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width > MAX_TEXTURE_SIDE || height > MAX_TEXTURE_SIDE || pixels > MAX_TEXTURE_PIXELS {
        tracing::warn!("texture {source} is {width}x{height}, mesh stays untextured");
        return false;
    }
    true
}

fn reencode_as_png(bytes: &[u8], source: &str) -> Option<Texture> {
    match image::load_from_memory(bytes) {
        Ok(image) => encode_png(&image, source),
        Err(error) => {
            tracing::warn!("texture {source} undecodable, mesh stays untextured: {error}");
            None
        }
    }
}

fn encode_png(image: &image::DynamicImage, source: &str) -> Option<Texture> {
    let mut encoded = Cursor::new(Vec::new());
    if let Err(error) = image.write_to(&mut encoded, ImageFormat::Png) {
        tracing::warn!("texture {source} not re-encodable as PNG, mesh stays untextured: {error}");
        return None;
    }
    Some(Texture {
        image_data: encoded.into_inner(),
        mime_type: PNG_MIME.to_string(),
        width: image.width(),
        height: image.height(),
    })
}

/// Where a texture named by a mesh file sits on disk. Backslashes come from
/// files authored on Windows, and an absolute path from another machine is
/// retried as a bare file name beside the mesh.
pub fn resolve(mesh_dir: &Path, name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");

    let beside_the_mesh = mesh_dir.join(&name);
    if beside_the_mesh.is_file() {
        return Some(beside_the_mesh);
    }

    let file_name = Path::new(&name).file_name()?;
    let by_file_name = mesh_dir.join(file_name);
    if by_file_name.is_file() {
        return Some(by_file_name);
    }

    let absolute = Path::new(&name);
    if absolute.is_file() {
        return Some(absolute.to_path_buf());
    }

    tracing::warn!(
        "texture {name} not found under {}, mesh stays untextured",
        mesh_dir.display()
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    #[test]
    fn png_goes_through_unchanged() {
        let bytes = png_bytes(4, 2);
        let texture = from_bytes(bytes.clone(), "test").expect("a texture");
        assert_eq!(texture.image_data, bytes);
        assert_eq!(texture.mime_type, PNG_MIME);
        assert_eq!((texture.width, texture.height), (4, 2));
    }

    #[test]
    fn a_bmp_is_re_encoded_as_png() {
        let image = image::RgbaImage::from_pixel(3, 5, image::Rgba([1, 2, 3, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Bmp)
            .unwrap();

        let texture = from_bytes(encoded.into_inner(), "test").expect("a texture");
        assert_eq!(texture.mime_type, PNG_MIME);
        assert_eq!((texture.width, texture.height), (3, 5));
        assert_eq!(
            image::guess_format(&texture.image_data).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn garbage_bytes_leave_the_mesh_untextured() {
        assert!(from_bytes(b"not an image at all".to_vec(), "test").is_none());
    }

    #[test]
    fn a_windows_path_resolves_to_the_file_beside_the_mesh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("wall.png"), png_bytes(1, 1)).unwrap();

        let resolved = resolve(dir.path(), "C:\\models\\textures\\wall.png").expect("a path");
        assert_eq!(resolved, dir.path().join("wall.png"));

        assert!(resolve(dir.path(), "missing.png").is_none());
    }

    #[test]
    fn a_missing_file_leaves_the_mesh_untextured() {
        assert!(from_file(Path::new("/nonexistent/texture.png")).is_none());
    }

    #[test]
    fn a_texture_past_the_side_cap_is_dropped() {
        assert!(from_rgba8(vec![0; 4], MAX_TEXTURE_SIDE + 1, 1, "wide").is_none());
    }
}
