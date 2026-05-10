//! tiletopia-ingest: geospatial format readers
//!
//! Parse point clouds (LAS/LAZ/E57/PLY), 3D models (glTF/OBJ/CityGML/IFC),
//! terrain (GeoTIFF/DTED), and vector data (Shapefile/GeoJSON/KML).

pub mod bim_reader;
pub mod citygml_reader;
pub mod cityjson_reader;
pub mod dted_reader;
pub mod e57_reader;
pub mod fbx_reader;
pub mod geojson_reader;
pub mod gltf_reader;
pub mod gpkg_reader;
pub mod hgt_reader;
pub mod ifc_reader;
pub mod kml_reader;
pub mod las_reader;
pub mod obj_reader;
pub mod photogrammetry;
pub mod ply_reader;
pub mod shapefile_reader;
pub mod tiff_reader;
pub mod usgs_dem_reader;

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

/// A vector feature with geometry and properties.
#[derive(Debug, Clone)]
pub struct VectorFeature {
    pub geometry: VectorGeometry,
    pub properties: std::collections::HashMap<String, String>,
}

/// Vector geometry types.
#[derive(Debug, Clone)]
pub enum VectorGeometry {
    Point(f64, f64),
    LineString(Vec<(f64, f64)>),
    Polygon(Vec<Vec<(f64, f64)>>),
    MultiPoint(Vec<(f64, f64)>),
    MultiLineString(Vec<Vec<(f64, f64)>>),
    MultiPolygon(Vec<Vec<Vec<(f64, f64)>>>),
}

/// Source data type.
#[derive(Debug, Clone)]
pub enum SourceData {
    PointCloud(Vec<Point3D>),
    Heightmap(Heightmap),
    Mesh(MeshData),
}

/// A 2D grid of elevation values.
#[derive(Debug, Clone)]
pub struct Heightmap {
    pub width: usize,
    pub height: usize,
    /// Row-major elevation values in meters.
    pub elevations: Vec<f64>,
    /// Geographic bounds [west, south, east, north] in degrees.
    pub bounds: [f64; 4],
    /// No-data value.
    pub nodata: Option<f64>,
}

/// Mesh data from glTF/OBJ/etc.
#[derive(Debug, Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    pub name: String,
}

/// Read a point cloud file (LAS/LAZ).
pub fn read_point_cloud(path: &std::path::Path) -> Result<Vec<Point3D>, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("las" | "laz") => las_reader::read(path),
        Some("e57") => e57_reader::read(path),
        Some("ply") => ply_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}

/// Read vector features from a GeoJSON, Shapefile, KML, or GeoPackage.
pub fn read_vector(path: &std::path::Path) -> Result<Vec<VectorFeature>, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("geojson" | "json") => geojson_reader::read(path),
        Some("shp") => shapefile_reader::read(path),
        Some("kml") => kml_reader::read(path),
        Some("gpkg") => gpkg_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}

/// Read a heightmap from a GeoTIFF, DTED, or HGT file.
pub fn read_heightmap(path: &std::path::Path) -> Result<Heightmap, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tif" | "tiff") => tiff_reader::read(path),
        Some("dt0" | "dt1" | "dt2") => dted_reader::read(path),
        Some("hgt") => hgt_reader::read(path),
        Some("dem") => usgs_dem_reader::read(path),
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}

/// Read a 3D mesh from a glTF/GLB, OBJ, CityGML, or CityJSON file.
pub fn read_mesh(path: &std::path::Path) -> Result<Vec<MeshData>, IngestError> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("gltf" | "glb") => gltf_reader::read(path),
        Some("obj") => obj_reader::read(path),
        Some("fbx") => fbx_reader::read(path),
        Some("ifc") => ifc_reader::read(path),
        Some("gml" | "xml") => citygml_reader::read(path),
        Some("json") => {
            // Check if it's CityJSON by peeking at the content
            let data = std::fs::read_to_string(path)?;
            if data.contains("\"CityJSON\"") {
                // Re-read via the CityJSON reader (it re-reads the file, but keeps the API clean)
                cityjson_reader::read(path)
            } else {
                Err(IngestError::UnsupportedFormat(
                    "json (not CityJSON)".to_string(),
                ))
            }
        }
        Some(ext) => Err(IngestError::UnsupportedFormat(ext.to_string())),
        None => Err(IngestError::UnsupportedFormat("unknown".to_string())),
    }
}
