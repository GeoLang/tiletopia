//! PLY point cloud reader.

use crate::{IngestError, Point3D};
use ply_rs::parser::Parser;
use ply_rs::ply::{DefaultElement, Property};
use std::io::BufReader;
use std::path::Path;

fn prop_f64(element: &DefaultElement, key: &str) -> f64 {
    match element.get(key) {
        Some(Property::Float(v)) => *v as f64,
        Some(Property::Double(v)) => *v,
        _ => 0.0,
    }
}

fn prop_u8(element: &DefaultElement, key: &str) -> u8 {
    match element.get(key) {
        Some(Property::UChar(v)) => *v,
        Some(Property::UInt(v)) => *v as u8,
        Some(Property::Float(v)) => *v as u8,
        _ => 0,
    }
}

/// Read a PLY file into a vector of points.
pub fn read(path: &Path) -> Result<Vec<Point3D>, IngestError> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let parser = Parser::<DefaultElement>::new();

    let ply = parser
        .read_ply(&mut reader)
        .map_err(|e| IngestError::ParseError(format!("PLY parse error: {e}")))?;

    let vertices = ply
        .payload
        .get("vertex")
        .ok_or_else(|| IngestError::ParseError("PLY: no vertex element found".to_string()))?;

    let points: Vec<Point3D> = vertices
        .iter()
        .map(|v| Point3D {
            x: prop_f64(v, "x"),
            y: prop_f64(v, "y"),
            z: prop_f64(v, "z"),
            r: prop_u8(v, "red"),
            g: prop_u8(v, "green"),
            b: prop_u8(v, "blue"),
            classification: 0,
            intensity: 0,
        })
        .collect();

    tracing::info!("Read {} points from {}", points.len(), path.display());
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_minimal_ply() {
        let dir = std::env::temp_dir().join("tiletopia_ply_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.ply");

        let ply_content = "\
ply\r
format ascii 1.0\r
element vertex 3\r
property float x\r
property float y\r
property float z\r
property uchar red\r
property uchar green\r
property uchar blue\r
end_header\r
1.0 2.0 3.0 255 0 0\r
4.0 5.0 6.0 0 255 0\r
7.0 8.0 9.0 0 0 255\r
";
        std::fs::write(&path, ply_content).unwrap();

        let points = read(&path).unwrap();
        assert_eq!(points.len(), 3);
        assert!((points[0].x - 1.0).abs() < 1e-6);
        assert!((points[0].y - 2.0).abs() < 1e-6);
        assert!((points[0].z - 3.0).abs() < 1e-6);
        assert_eq!(points[0].r, 255);
        assert_eq!(points[0].g, 0);
        assert_eq!(points[0].b, 0);
        assert_eq!(points[2].b, 255);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_nonexistent_ply() {
        let result = read(Path::new("/tmp/nonexistent_ply_file.ply"));
        assert!(result.is_err());
    }
}
