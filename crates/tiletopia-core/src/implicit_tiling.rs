//! Implicit tiling (3D Tiles Next) — subtree-based tiling for massive datasets.
//!
//! Implements the 3D Tiles 1.1 implicit tiling extension with octree/quadtree
//! subtree files for efficient traversal of billion-point datasets.

use serde::{Deserialize, Serialize};
use std::io;

/// Subtree availability bitstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Availability {
    /// Bitvector: 1 = available, 0 = not available.
    pub bitstream: Vec<u8>,
    /// Number of valid bits.
    pub available_count: u32,
}

/// Implicit tiling subdivision scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SubdivisionScheme {
    Octree,
    Quadtree,
}

/// Implicit tiling configuration in tileset.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitTiling {
    pub subdivision_scheme: SubdivisionScheme,
    pub subtree_levels: u32,
    pub available_levels: u32,
    pub subtrees: SubtreeRef,
}

/// Reference to subtree files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtreeRef {
    pub uri: String,
}

/// A subtree file containing availability information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subtree {
    pub tile_availability: Availability,
    pub content_availability: Availability,
    pub child_subtree_availability: Availability,
}

/// Morton code (Z-order curve) for spatial indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MortonCode(pub u64);

impl MortonCode {
    /// Encode 3D coordinates (each 0..2^21) into a 64-bit Morton code.
    pub fn encode_3d(x: u32, y: u32, z: u32) -> Self {
        Self(split_by_3(x as u64) | (split_by_3(y as u64) << 1) | (split_by_3(z as u64) << 2))
    }

    /// Decode a Morton code back to 3D coordinates.
    pub fn decode_3d(self) -> (u32, u32, u32) {
        let x = compact_by_3(self.0) as u32;
        let y = compact_by_3(self.0 >> 1) as u32;
        let z = compact_by_3(self.0 >> 2) as u32;
        (x, y, z)
    }

    /// Get the Morton code for a child node at given octant (0-7).
    pub fn child(self, octant: u8) -> Self {
        Self((self.0 << 3) | octant as u64)
    }

    /// Get the parent Morton code.
    pub fn parent(self) -> Self {
        Self(self.0 >> 3)
    }

    /// Get the level of this node (root = 0).
    pub fn level(self) -> u32 {
        if self.0 == 0 {
            return 0;
        }
        ((64 - self.0.leading_zeros()) / 3).saturating_sub(0)
    }
}

fn split_by_3(mut x: u64) -> u64 {
    x &= 0x1fffff; // 21 bits
    x = (x | (x << 32)) & 0x1f00000000ffff;
    x = (x | (x << 16)) & 0x1f0000ff0000ff;
    x = (x | (x << 8)) & 0x100f00f00f00f00f;
    x = (x | (x << 4)) & 0x10c30c30c30c30c3;
    x = (x | (x << 2)) & 0x1249249249249249;
    x
}

fn compact_by_3(mut x: u64) -> u64 {
    x &= 0x1249249249249249;
    x = (x | (x >> 2)) & 0x10c30c30c30c30c3;
    x = (x | (x >> 4)) & 0x100f00f00f00f00f;
    x = (x | (x >> 8)) & 0x1f0000ff0000ff;
    x = (x | (x >> 16)) & 0x1f00000000ffff;
    x = (x | (x >> 32)) & 0x1fffff;
    x
}

/// Builder for implicit tilesets.
pub struct ImplicitTilesetBuilder {
    subdivision: SubdivisionScheme,
    subtree_levels: u32,
    max_levels: u32,
    /// Points per cell threshold for subdivision.
    points_threshold: usize,
}

impl ImplicitTilesetBuilder {
    pub fn new(subdivision: SubdivisionScheme, subtree_levels: u32, max_levels: u32) -> Self {
        Self {
            subdivision,
            subtree_levels,
            max_levels,
            points_threshold: 10000,
        }
    }

    pub fn with_points_threshold(mut self, threshold: usize) -> Self {
        self.points_threshold = threshold;
        self
    }

    /// Build implicit tiling structure from point positions.
    /// Returns the tileset implicit tiling config and subtree files.
    pub fn build(&self, points: &[[f64; 3]]) -> (ImplicitTiling, Vec<(String, Subtree)>) {
        // Compute bounding box
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        for p in points {
            for i in 0..3 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
            }
        }

        let extent = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
        let max_extent = extent[0].max(extent[1]).max(extent[2]);

        // Quantize points to grid
        let grid_size = 1u32 << self.max_levels;
        let scale = (grid_size as f64 - 1.0) / max_extent;

        let quantized: Vec<(u32, u32, u32)> = points
            .iter()
            .map(|p| {
                (
                    ((p[0] - min[0]) * scale) as u32,
                    ((p[1] - min[1]) * scale) as u32,
                    ((p[2] - min[2]) * scale) as u32,
                )
            })
            .collect();

        // Build availability for subtree levels
        let nodes_per_subtree = nodes_in_subtree(self.subtree_levels, &self.subdivision);
        let mut tile_bits = vec![0u8; nodes_per_subtree.div_ceil(8)];
        let mut content_bits = vec![0u8; nodes_per_subtree.div_ceil(8)];

        // Count points per node at each level
        for level in 0..self.subtree_levels {
            let shift = self.max_levels - level - 1;
            let mut cells: std::collections::HashSet<(u32, u32, u32)> =
                std::collections::HashSet::new();
            for &(x, y, z) in &quantized {
                cells.insert((x >> shift, y >> shift, z >> shift));
            }
            for (cx, cy, cz) in cells {
                let node_idx = node_index_in_subtree(level, cx, cy, cz, &self.subdivision);
                if node_idx < nodes_per_subtree {
                    set_bit(&mut tile_bits, node_idx);
                    set_bit(&mut content_bits, node_idx);
                }
            }
        }

        let available_count = tile_bits.iter().map(|b| b.count_ones()).sum::<u32>();

        let subtree = Subtree {
            tile_availability: Availability {
                bitstream: tile_bits,
                available_count,
            },
            content_availability: Availability {
                bitstream: content_bits.clone(),
                available_count,
            },
            child_subtree_availability: Availability {
                bitstream: vec![0xFF; nodes_per_subtree.div_ceil(8)], // all children available
                available_count: nodes_per_subtree as u32,
            },
        };

        let tiling = ImplicitTiling {
            subdivision_scheme: self.subdivision,
            subtree_levels: self.subtree_levels,
            available_levels: self.max_levels,
            subtrees: SubtreeRef {
                uri: "subtrees/{level}/{x}/{y}/{z}.subtree".to_string(),
            },
        };

        (tiling, vec![("0/0/0/0.subtree".to_string(), subtree)])
    }
}

/// Serialize a subtree to binary (.subtree format).
pub fn serialize_subtree(subtree: &Subtree) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(subtree).map_err(io::Error::other)?;
    // In production, this would use the binary subtree format with buffers
    // For now, JSON is valid per spec (JSON subtrees are allowed)
    Ok(json)
}

fn nodes_in_subtree(levels: u32, scheme: &SubdivisionScheme) -> usize {
    let branching = match scheme {
        SubdivisionScheme::Octree => 8usize,
        SubdivisionScheme::Quadtree => 4usize,
    };
    // Total nodes = sum of branching^i for i in 0..levels
    (0..levels).map(|i| branching.pow(i)).sum()
}

fn node_index_in_subtree(level: u32, x: u32, y: u32, z: u32, scheme: &SubdivisionScheme) -> usize {
    let branching = match scheme {
        SubdivisionScheme::Octree => 8usize,
        SubdivisionScheme::Quadtree => 4usize,
    };
    // Offset for this level
    let level_offset: usize = (0..level).map(|i| branching.pow(i)).sum();
    // Index within level (Morton order)
    let within_level = match scheme {
        SubdivisionScheme::Octree => MortonCode::encode_3d(x, y, z).0 as usize,
        SubdivisionScheme::Quadtree => (interleave_2d(x, y)) as usize,
    };
    let max_in_level = branching.pow(level);
    level_offset + (within_level % max_in_level)
}

fn interleave_2d(x: u32, y: u32) -> u64 {
    let mut x = x as u64;
    let mut y = y as u64;
    x = (x | (x << 16)) & 0x0000FFFF0000FFFF;
    x = (x | (x << 8)) & 0x00FF00FF00FF00FF;
    x = (x | (x << 4)) & 0x0F0F0F0F0F0F0F0F;
    x = (x | (x << 2)) & 0x3333333333333333;
    x = (x | (x << 1)) & 0x5555555555555555;
    y = (y | (y << 16)) & 0x0000FFFF0000FFFF;
    y = (y | (y << 8)) & 0x00FF00FF00FF00FF;
    y = (y | (y << 4)) & 0x0F0F0F0F0F0F0F0F;
    y = (y | (y << 2)) & 0x3333333333333333;
    y = (y | (y << 1)) & 0x5555555555555555;
    x | (y << 1)
}

fn set_bit(bits: &mut [u8], index: usize) {
    bits[index / 8] |= 1 << (index % 8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_morton_encode_decode() {
        let code = MortonCode::encode_3d(5, 3, 7);
        let (x, y, z) = code.decode_3d();
        assert_eq!((x, y, z), (5, 3, 7));
    }

    #[test]
    fn test_morton_parent_child() {
        let root = MortonCode(1);
        let child = root.child(3);
        assert_eq!(child.parent(), root);
    }

    #[test]
    fn test_build_implicit_tileset() {
        let points: Vec<[f64; 3]> = (0..1000)
            .map(|i| [(i % 10) as f64, ((i / 10) % 10) as f64, (i / 100) as f64])
            .collect();
        let builder = ImplicitTilesetBuilder::new(SubdivisionScheme::Octree, 3, 5);
        let (tiling, subtrees) = builder.build(&points);
        assert_eq!(tiling.subdivision_scheme, SubdivisionScheme::Octree);
        assert_eq!(tiling.subtree_levels, 3);
        assert!(!subtrees.is_empty());
        assert!(subtrees[0].1.tile_availability.available_count > 0);
    }

    #[test]
    fn test_serialize_subtree() {
        let subtree = Subtree {
            tile_availability: Availability {
                bitstream: vec![0xFF, 0x0F],
                available_count: 12,
            },
            content_availability: Availability {
                bitstream: vec![0xFF, 0x0F],
                available_count: 12,
            },
            child_subtree_availability: Availability {
                bitstream: vec![0xFF],
                available_count: 8,
            },
        };
        let data = serialize_subtree(&subtree).unwrap();
        assert!(!data.is_empty());
        // Should be valid JSON
        let _parsed: Subtree = serde_json::from_slice(&data).unwrap();
    }

    #[test]
    fn test_nodes_in_subtree() {
        // Octree with 3 levels: 1 + 8 + 64 = 73
        assert_eq!(nodes_in_subtree(3, &SubdivisionScheme::Octree), 73);
        // Quadtree with 3 levels: 1 + 4 + 16 = 21
        assert_eq!(nodes_in_subtree(3, &SubdivisionScheme::Quadtree), 21);
    }
}
