//! GeoTIFF heightmap reader.

use crate::{Heightmap, IngestError};
use std::path::Path;
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;

/// Parse geographic bounds from GeoTIFF tags.
///
/// Tries (in order):
/// 1. ModelTransformation tag (34264) — 4×4 affine matrix
/// 2. ModelPixelScale (33550) + ModelTiepoint (33922)
/// 3. Fallback placeholder bounds
fn parse_geo_bounds<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
    width: usize,
    height: usize,
) -> [f64; 4] {
    // ModelTransformation (34264): 16 doubles forming a 4×4 affine matrix
    if let Ok(matrix) = decoder.get_tag_f64_vec(Tag::Unknown(34264))
        && matrix.len() >= 16
    {
        let west = matrix[3];
        let north = matrix[7];
        let east = west + width as f64 * matrix[0];
        let south = north + height as f64 * matrix[5];
        return [west, south, east, north];
    }

    // ModelPixelScale (33550): [ScaleX, ScaleY, ScaleZ]
    // ModelTiepoint (33922): [I, J, K, X, Y, Z]
    if let (Ok(scale), Ok(tiepoint)) = (
        decoder.get_tag_f64_vec(Tag::Unknown(33550)),
        decoder.get_tag_f64_vec(Tag::Unknown(33922)),
    ) && scale.len() >= 2
        && tiepoint.len() >= 6
    {
        let scale_x = scale[0];
        let scale_y = scale[1];
        let (i, j) = (tiepoint[0], tiepoint[1]);
        let (x, y) = (tiepoint[3], tiepoint[4]);
        let west = x - i * scale_x;
        let north = y + j * scale_y;
        let east = west + width as f64 * scale_x;
        let south = north - height as f64 * scale_y;
        return [west, south, east, north];
    }

    // Fallback placeholder bounds
    [0.0, 0.0, width as f64 / 3600.0, height as f64 / 3600.0]
}

/// Read a GeoTIFF DEM/DTM into a Heightmap.
pub fn read(path: &Path) -> Result<Heightmap, IngestError> {
    let file = std::fs::File::open(path)?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| IngestError::ParseError(format!("TIFF decode error: {e}")))?;

    let (width, height) = decoder
        .dimensions()
        .map_err(|e| IngestError::ParseError(format!("TIFF dimensions error: {e}")))?;
    let width = width as usize;
    let height = height as usize;

    let bounds = parse_geo_bounds(&mut decoder, width, height);

    let image = decoder
        .read_image()
        .map_err(|e| IngestError::ParseError(format!("TIFF read error: {e}")))?;

    let elevations: Vec<f64> = match image {
        DecodingResult::F32(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::F64(data) => data,
        DecodingResult::U16(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::I16(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::U32(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::I32(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::U8(data) => data.into_iter().map(|v| v as f64).collect(),
        DecodingResult::I8(data) => data.into_iter().map(|v| v as f64).collect(),
        _ => {
            return Err(IngestError::ParseError(
                "unsupported TIFF sample format".into(),
            ));
        }
    };

    if elevations.len() != width * height {
        return Err(IngestError::ParseError(format!(
            "expected {} samples, got {}",
            width * height,
            elevations.len()
        )));
    }

    tracing::info!(
        "Read {}×{} heightmap from {} (min={:.1}, max={:.1})",
        width,
        height,
        path.display(),
        elevations.iter().copied().fold(f64::INFINITY, f64::min),
        elevations.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    );

    Ok(Heightmap {
        width,
        height,
        elevations,
        bounds,
        nodata: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fallback_bounds() {
        let width = 3600;
        let height = 1800;
        // Create a minimal TIFF in memory with no geo tags
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut buf).unwrap();
            let data = vec![0u8; width * height];
            encoder
                .write_image::<tiff::encoder::colortype::Gray8>(width as u32, height as u32, &data)
                .unwrap();
        }
        buf.set_position(0);
        let mut decoder = Decoder::new(buf).unwrap();
        let bounds = parse_geo_bounds(&mut decoder, width, height);
        assert_eq!(
            bounds,
            [0.0, 0.0, width as f64 / 3600.0, height as f64 / 3600.0]
        );
    }
}
