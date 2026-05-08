//! tiletopia-ingest: geospatial format readers
//!
//! Parse point clouds (LAS/LAZ/E57/PLY), 3D models (glTF/OBJ/CityGML/IFC),
//! terrain (GeoTIFF/DTED), and vector data (Shapefile/GeoJSON/KML).

pub mod las_reader;

use thiserror::Error;

/// Ingest errors.
#[derive(Debug, Error)]
pub enum IngestError {
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A 3D point with optional colour and classification.
#[derive(Debug, Clone, Copy)]
pub struct Point3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub classification: u8,
    pub intensity: u16,
}

/// Source data type.
#[derive(Debug, Clone)]
pub enum SourceData {
    PointCloud(Vec<Point3D>),
    // Future: Mesh, Terrain, Vector
}

/// Read a point cloud file (LAS/LAZ).
pub fn read_point_cloud(path: &std::path::Path) -> Result<Vec<Point3D>, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("las" | "laz") => las_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}
