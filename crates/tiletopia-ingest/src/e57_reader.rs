//! E57 point cloud reader.

use crate::{IngestError, Point3D};
use e57::CartesianCoordinate;
use std::path::Path;

/// Read an E57 file into a vector of points.
pub fn read(path: &Path) -> Result<Vec<Point3D>, IngestError> {
    let mut reader = e57::E57Reader::from_file(path)
        .map_err(|e| IngestError::ParseError(format!("E57 open error: {e}")))?;

    let mut points = Vec::new();

    for pc in reader.pointclouds() {
        let iter = reader
            .pointcloud_simple(&pc)
            .map_err(|e| IngestError::ParseError(format!("E57 pointcloud error: {e}")))?;

        for p in iter {
            let p = p.map_err(|e| IngestError::ParseError(format!("E57 point error: {e}")))?;

            let (x, y, z) = match p.cartesian {
                CartesianCoordinate::Valid { x, y, z }
                | CartesianCoordinate::Direction { x, y, z } => (x, y, z),
                CartesianCoordinate::Invalid => continue,
            };

            let (r, g, b) = match p.color {
                Some(c) => (
                    (c.red * 255.0) as u8,
                    (c.green * 255.0) as u8,
                    (c.blue * 255.0) as u8,
                ),
                None => (0, 0, 0),
            };

            points.push(Point3D {
                x,
                y,
                z,
                r,
                g,
                b,
                classification: 0,
                intensity: p.intensity.map(|i| (i * 65535.0) as u16).unwrap_or(0),
            });
        }
    }

    tracing::info!("Read {} points from {}", points.len(), path.display());
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_nonexistent_file() {
        let result = read(Path::new("/tmp/nonexistent_e57_file.e57"));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_fn_signature() {
        // Verify the function accepts a Path and returns the expected type.
        let _: fn(&Path) -> Result<Vec<Point3D>, IngestError> = read;
    }
}
