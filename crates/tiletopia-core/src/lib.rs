//! tiletopia-core: 3D Tiles tiling engine
//!
//! Spatial indexing, LOD generation, and OGC 3D Tiles 1.1 output.

pub mod anomaly;
pub mod bounding_volume;
pub mod clash_detection;
pub mod classify;
pub mod colorize;
pub mod compression;
pub mod crs;
pub mod diff;
pub mod diff_viz;
pub mod glb_writer;
pub mod gpu;
pub mod implicit_tiling;
pub mod lod;
pub mod measurement;
pub mod mesh_tiler;
pub mod metadata;
pub mod model_zoo;
pub mod octree;
pub mod onnx_inference;
pub mod plugin;
pub mod prediction;
pub mod spatial;
pub mod spatial_query;
pub mod tile;
pub mod tileset;

use serde::{Deserialize, Serialize};

/// 3D bounding volume (region, box, or sphere).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum BoundingVolume {
    /// Geographic region [west, south, east, north, min_height, max_height] in radians/meters.
    Region { region: [f64; 6] },
    /// Oriented bounding box [center_x, center_y, center_z, ...half_axes (12 floats)].
    Box { r#box: [f64; 12] },
    /// Bounding sphere [center_x, center_y, center_z, radius].
    Sphere { sphere: [f64; 4] },
}

/// A single 3D Tile node in the tileset tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tile {
    pub bounding_volume: BoundingVolume,
    pub geometric_error: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<TileContent>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<Tile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refine: Option<Refine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<[f64; 16]>,
}

/// Tile content reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileContent {
    pub uri: String,
}

/// Refinement strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Refine {
    Add,
    Replace,
}

/// Root tileset.json structure (OGC 3D Tiles 1.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tileset {
    pub asset: TilesetAsset,
    pub geometric_error: f64,
    pub root: Tile,
}

/// Tileset asset metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TilesetAsset {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tileset_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
}

impl Default for TilesetAsset {
    fn default() -> Self {
        Self {
            version: "1.1".to_string(),
            tileset_version: None,
            generator: Some("tiletopia".to_string()),
        }
    }
}
