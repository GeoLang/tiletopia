//! 3D Tiles binary tile writers (.pnts format).
//!
//! Implements the Point Cloud (pnts) tile format per the 3D Tiles spec.
//! See: https://github.com/CesiumGS/3d-tiles/tree/main/specification/TileFormats/PointCloud

use crate::octree::OctreePoint;
use std::io::{self, Write};

/// Magic bytes for .pnts files.
const PNTS_MAGIC: &[u8; 4] = b"pnts";

/// .pnts version.
const PNTS_VERSION: u32 = 1;

/// Write a set of points as a .pnts tile to the given writer.
pub fn write_pnts<W: Write>(points: &[OctreePoint], writer: &mut W) -> io::Result<()> {
    if points.is_empty() {
        return Ok(());
    }

    let num_points = points.len() as u32;

    // Feature table JSON
    let feature_table_json = format!(
        "{{\"POINTS_LENGTH\":{num_points},\"POSITION\":{{\"byteOffset\":0}},\"RGB\":{{\"byteOffset\":{}}}}}\n",
        num_points as usize * 12 // 3 * f32
    );

    // Pad JSON to 8-byte alignment
    let json_bytes = feature_table_json.as_bytes();
    let json_padding = (8 - (json_bytes.len() % 8)) % 8;
    let padded_json_len = json_bytes.len() + json_padding;

    // Feature table binary: positions (3×f32) + colors (3×u8, padded to 4-byte)
    let positions_size = num_points as usize * 12; // 3 × f32
    let colors_size = num_points as usize * 3; // 3 × u8
    let colors_padding = (4 - (colors_size % 4)) % 4;
    let binary_size = positions_size + colors_size + colors_padding;

    // Total size
    let header_size = 28u32; // magic(4) + version(4) + byteLength(4) + ftJSON(4) + ftBinary(4) + btJSON(4) + btBinary(4)
    let total_size = header_size + padded_json_len as u32 + binary_size as u32;

    // Write header
    writer.write_all(PNTS_MAGIC)?;
    writer.write_all(&PNTS_VERSION.to_le_bytes())?;
    writer.write_all(&total_size.to_le_bytes())?;
    writer.write_all(&(padded_json_len as u32).to_le_bytes())?;
    writer.write_all(&(binary_size as u32).to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?; // batch table JSON length
    writer.write_all(&0u32.to_le_bytes())?; // batch table binary length

    // Write feature table JSON (padded)
    writer.write_all(json_bytes)?;
    for _ in 0..json_padding {
        writer.write_all(b" ")?;
    }

    // Write positions as f32 (relative to first point for precision)
    let origin = points[0].position;
    for p in points {
        let x = (p.position[0] - origin[0]) as f32;
        let y = (p.position[1] - origin[1]) as f32;
        let z = (p.position[2] - origin[2]) as f32;
        writer.write_all(&x.to_le_bytes())?;
        writer.write_all(&y.to_le_bytes())?;
        writer.write_all(&z.to_le_bytes())?;
    }

    // Write RGB colors
    for p in points {
        writer.write_all(&p.color)?;
    }
    for _ in 0..colors_padding {
        writer.write_all(&[0u8])?;
    }

    Ok(())
}

/// Write a tileset.json and all .pnts tiles from an octree to a directory.
pub fn write_tileset_to_dir(
    root: &crate::octree::OctreeNode,
    output_dir: &std::path::Path,
) -> io::Result<()> {
    std::fs::create_dir_all(output_dir)?;

    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;

    // Generate tileset.json
    let tileset = crate::lod::generate_tileset(root, "tiles/");
    let tileset_json = serde_json::to_string_pretty(&tileset).map_err(io::Error::other)?;
    std::fs::write(output_dir.join("tileset.json"), tileset_json)?;

    // Write all tile files
    write_node_tiles(root, &tiles_dir, &mut NodePath::new())
}

struct NodePath {
    segments: Vec<u8>,
}

impl NodePath {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    fn push(&mut self, idx: u8) {
        self.segments.push(idx);
    }

    fn pop(&mut self) {
        self.segments.pop();
    }

    fn to_filename(&self) -> String {
        if self.segments.is_empty() {
            "root.pnts".to_string()
        } else {
            let s: String = self.segments.iter().map(|i| char::from(b'0' + i)).collect();
            format!("{s}.pnts")
        }
    }
}

fn write_node_tiles(
    node: &crate::octree::OctreeNode,
    tiles_dir: &std::path::Path,
    path: &mut NodePath,
) -> io::Result<()> {
    match node {
        crate::octree::OctreeNode::Leaf { points, .. } => {
            if !points.is_empty() {
                let filename = path.to_filename();
                let mut file = std::fs::File::create(tiles_dir.join(&filename))?;
                write_pnts(points, &mut file)?;
            }
        }
        crate::octree::OctreeNode::Internal {
            children,
            lod_points,
            ..
        } => {
            if !lod_points.is_empty() {
                let filename = path.to_filename();
                let mut file = std::fs::File::create(tiles_dir.join(&filename))?;
                write_pnts(lod_points, &mut file)?;
            }
            for (i, child) in children.iter().enumerate() {
                if let Some(child_node) = child {
                    path.push(i as u8);
                    write_node_tiles(child_node, tiles_dir, path)?;
                    path.pop();
                }
            }
        }
    }
    Ok(())
}
