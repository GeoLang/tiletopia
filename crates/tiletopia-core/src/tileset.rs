//! High-level tileset generation pipeline.
//!
//! Orchestrates: ingest → octree build → LOD → tile output.

use crate::octree::{self, OctreeConfig, OctreePoint};
use crate::tile;
use std::io;
use std::path::Path;

/// Configuration for the full tiling pipeline.
#[derive(Debug, Clone)]
pub struct TilingConfig {
    pub octree: OctreeConfig,
    /// Maximum geometric error (meters). Controls overall LOD aggressiveness.
    pub max_geometric_error: f64,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self {
            octree: OctreeConfig::default(),
            max_geometric_error: 100.0,
        }
    }
}

/// Run the full tiling pipeline: build octree from points and write tiles to disk.
pub fn tile_point_cloud(
    points: Vec<OctreePoint>,
    output_dir: &Path,
    config: &TilingConfig,
) -> io::Result<octree::OctreeStats> {
    let root = octree::build_octree(points, &config.octree);
    let stats = octree::collect_stats(&root);

    tile::write_tileset_to_dir(&root, output_dir)?;

    Ok(stats)
}

/// Run the mesh tiling pipeline: partition meshes, generate LODs, and write tiles to disk.
pub fn tile_meshes(
    meshes: &[crate::mesh_tiler::MeshData],
    output_dir: &Path,
    config: &crate::mesh_tiler::MeshTilingConfig,
) -> io::Result<crate::mesh_tiler::MeshTilingStats> {
    crate::mesh_tiler::tile_meshes(meshes, output_dir, config)
}
