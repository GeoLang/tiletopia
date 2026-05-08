//! Level-of-detail computation and 3D Tiles tree generation from an octree.

use crate::bounding_volume::Aabb;
use crate::octree::OctreeNode;
use crate::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};

/// Generate a `Tileset` (tileset.json) from an octree.
/// `base_url` is the path prefix for tile content URIs (e.g. "tiles/").
pub fn generate_tileset(root: &OctreeNode, base_url: &str) -> Tileset {
    let root_tile = generate_tile(root, base_url, &mut NodePath::new());
    let geometric_error = root_tile.geometric_error;

    Tileset {
        asset: TilesetAsset {
            version: "1.1".to_string(),
            tileset_version: Some("1.0.0".to_string()),
            generator: Some("tiletopia".to_string()),
        },
        geometric_error,
        root: root_tile,
    }
}

/// Path tracker for generating unique tile filenames.
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

/// Compute geometric error for a node based on its spatial extent.
/// Geometric error ≈ half the diagonal of the bounding box at this level.
fn geometric_error_for(bounds: &Aabb) -> f64 {
    let h = bounds.half_extents();
    (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt()
}

fn generate_tile(node: &OctreeNode, base_url: &str, path: &mut NodePath) -> Tile {
    let bounds = node.bounds();
    let ge = geometric_error_for(bounds);
    let bounding_volume = BoundingVolume::Box {
        r#box: bounds.to_3dtiles_box(),
    };

    match node {
        OctreeNode::Leaf { points, .. } => {
            let content = if points.is_empty() {
                None
            } else {
                Some(TileContent {
                    uri: format!("{base_url}{}", path.to_filename()),
                })
            };
            Tile {
                bounding_volume,
                geometric_error: 0.0, // leaf has zero error
                content,
                children: Vec::new(),
                refine: Some(Refine::Add),
                transform: None,
            }
        }
        OctreeNode::Internal {
            children,
            lod_points,
            ..
        } => {
            let content = if lod_points.is_empty() {
                None
            } else {
                Some(TileContent {
                    uri: format!("{base_url}{}", path.to_filename()),
                })
            };

            let mut child_tiles = Vec::new();
            for (i, child) in children.iter().enumerate() {
                if let Some(child_node) = child {
                    path.push(i as u8);
                    child_tiles.push(generate_tile(child_node, base_url, path));
                    path.pop();
                }
            }

            Tile {
                bounding_volume,
                geometric_error: ge,
                content,
                children: child_tiles,
                refine: Some(Refine::Add),
                transform: None,
            }
        }
    }
}
