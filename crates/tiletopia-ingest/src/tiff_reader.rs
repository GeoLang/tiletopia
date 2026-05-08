//! GeoTIFF heightmap reader.

use crate::{Heightmap, IngestError};
use std::path::Path;
use tiff::decoder::{Decoder, DecodingResult};

/// Read a GeoTIFF DEM/DTM into a Heightmap.
pub fn read(path: &Path) -> Result<Heightmap, IngestError> {
    let file = std::fs::File::open(path)?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| IngestError::ParseError(format!("TIFF decode error: {e}")))?;

    let (width, height) = decoder.dimensions()
        .map_err(|e| IngestError::ParseError(format!("TIFF dimensions error: {e}")))?;
    let width = width as usize;
    let height = height as usize;

    let image = decoder.read_image()
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
        _ => return Err(IngestError::ParseError("unsupported TIFF sample format".into())),
    };

    if elevations.len() != width * height {
        return Err(IngestError::ParseError(format!(
            "expected {} samples, got {}",
            width * height,
            elevations.len()
        )));
    }

    // Default bounds — in a real GeoTIFF these come from the GeoKeys/ModelTiepoint tags.
    // For now, use placeholder bounds. A full implementation would parse TIFF GeoKeys.
    let bounds = [0.0, 0.0, width as f64 / 3600.0, height as f64 / 3600.0];

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
