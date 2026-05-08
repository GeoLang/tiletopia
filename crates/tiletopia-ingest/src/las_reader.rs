//! LAS/LAZ point cloud reader

use crate::{IngestError, Point3D};
use std::path::Path;

/// Read a LAS/LAZ file into a vector of points.
pub fn read(path: &Path) -> Result<Vec<Point3D>, IngestError> {
    let mut reader =
        las::Reader::from_path(path).map_err(|e| IngestError::ParseError(e.to_string()))?;

    let points: Vec<Point3D> = reader
        .points()
        .filter_map(|p| {
            let p = p.ok()?;
            Some(Point3D {
                x: p.x,
                y: p.y,
                z: p.z,
                r: (p.color.map(|c| (c.red >> 8) as u8).unwrap_or(0)),
                g: (p.color.map(|c| (c.green >> 8) as u8).unwrap_or(0)),
                b: (p.color.map(|c| (c.blue >> 8) as u8).unwrap_or(0)),
                classification: p.classification.into(),
                intensity: p.intensity,
            })
        })
        .collect();

    tracing::info!("Read {} points from {}", points.len(), path.display());
    Ok(points)
}
