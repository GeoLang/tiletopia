//! tiletopia-terrain: quantized mesh terrain from heightmaps
//!
//! Generates quantized mesh terrain tiles from GeoTIFF/DTED/HGT heightmaps
//! using Delaunay triangulation with geometric error-based simplification.

/// Heightmap grid (row-major, top-left origin).
pub struct Heightmap {
    pub width: u32,
    pub height: u32,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub elevations: Vec<f32>,
}

/// Quantized mesh tile output.
pub struct QuantizedMeshTile {
    pub x: u32,
    pub y: u32,
    pub level: u32,
    pub data: Vec<u8>,
}

/// Generate terrain tiles from a heightmap.
pub fn generate_terrain(
    _heightmap: &Heightmap,
    _max_level: u32,
    _geometric_error_threshold: f64,
) -> Vec<QuantizedMeshTile> {
    tracing::info!("Terrain generation not yet implemented");
    Vec::new()
}
