//! Mesh tiling pipeline — spatial subdivision, LOD generation, and 3D Tiles output for meshes.

use crate::bounding_volume::Aabb;
use crate::compression::simplify_mesh;
use crate::glb_writer::{self, GlbMesh};
use crate::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};
use image::GenericImageView;
use std::io;
use std::path::Path;

/// Input mesh data (mirrors `tiletopia_ingest::MeshData`).
#[derive(Debug, Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
    pub name: String,
}

/// Configuration for the mesh tiling pipeline.
#[derive(Debug, Clone)]
pub struct MeshTilingConfig {
    /// Maximum triangles per leaf tile.
    pub max_triangles_per_tile: usize,
    /// Maximum octree depth.
    pub max_depth: u32,
    /// Fraction of triangles to keep at each LOD level (0.0–1.0).
    pub simplification_ratio: f32,
    /// Minimum geometric error (meters) — leaves below this are not subdivided.
    pub min_geometric_error: f64,
}

impl Default for MeshTilingConfig {
    fn default() -> Self {
        Self {
            max_triangles_per_tile: 65536,
            max_depth: 10,
            simplification_ratio: 0.5,
            min_geometric_error: 0.1,
        }
    }
}

/// Statistics from a mesh tiling run.
#[derive(Debug, Clone)]
pub struct MeshTilingStats {
    pub total_triangles: usize,
    pub tile_count: usize,
    pub max_depth_reached: u32,
}

/// A node in the mesh tile tree.
pub enum MeshTileNode {
    Leaf {
        bounds: Aabb,
        mesh: GlbMesh,
        depth: u32,
    },
    Internal {
        bounds: Aabb,
        children: Vec<Box<MeshTileNode>>,
        /// Simplified mesh for this LOD level.
        lod_mesh: GlbMesh,
        depth: u32,
    },
}

impl MeshTileNode {
    fn bounds(&self) -> &Aabb {
        match self {
            MeshTileNode::Leaf { bounds, .. } => bounds,
            MeshTileNode::Internal { bounds, .. } => bounds,
        }
    }

    fn depth(&self) -> u32 {
        match self {
            MeshTileNode::Leaf { depth, .. } => *depth,
            MeshTileNode::Internal { depth, .. } => *depth,
        }
    }
}

/// Build a mesh tile tree from a set of input meshes.
pub fn build_mesh_tree(meshes: &[MeshData], config: &MeshTilingConfig) -> MeshTileNode {
    let (positions, normals, indices) = merge_meshes(meshes);
    let bounds = compute_aabb(&positions);
    build_recursive(&positions, &normals, &indices, bounds, 0, config)
}

/// Write mesh tiles (GLB files + tileset.json) to disk.
pub fn write_mesh_tileset(root: &MeshTileNode, output_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;

    write_node_glbs(root, &tiles_dir, &mut NodePath::new())?;

    let tileset = generate_mesh_tileset(root);
    let json = serde_json::to_string_pretty(&tileset).map_err(io::Error::other)?;
    std::fs::write(output_dir.join("tileset.json"), json)?;

    Ok(())
}

/// Full pipeline: build tree from meshes and write tiles to disk.
pub fn tile_meshes(
    meshes: &[MeshData],
    output_dir: &Path,
    config: &MeshTilingConfig,
) -> io::Result<MeshTilingStats> {
    let root = build_mesh_tree(meshes, config);
    write_mesh_tileset(&root, output_dir)?;

    let mut stats = MeshTilingStats {
        total_triangles: 0,
        tile_count: 0,
        max_depth_reached: 0,
    };
    collect_stats(&root, &mut stats);
    Ok(stats)
}

// ── internals ───────────────────────────────────────────────────────────────

fn merge_meshes(meshes: &[MeshData]) -> (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<u32>) {
    let total_verts: usize = meshes.iter().map(|m| m.positions.len()).sum();
    let total_idx: usize = meshes.iter().map(|m| m.indices.len()).sum();

    let mut positions = Vec::with_capacity(total_verts);
    let mut normals = Vec::with_capacity(total_verts);
    let mut indices = Vec::with_capacity(total_idx);

    for m in meshes {
        let base = positions.len() as u32;
        positions.extend_from_slice(&m.positions);
        normals.extend_from_slice(&m.normals);
        indices.extend(m.indices.iter().map(|&i| i + base));
    }

    (positions, normals, indices)
}

fn compute_aabb(positions: &[[f32; 3]]) -> Aabb {
    let mut aabb = Aabb::empty();
    for p in positions {
        aabb.expand_point([p[0] as f64, p[1] as f64, p[2] as f64]);
    }
    aabb
}

fn build_recursive(
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    indices: &[u32],
    bounds: Aabb,
    depth: u32,
    config: &MeshTilingConfig,
) -> MeshTileNode {
    let tri_count = indices.len() / 3;

    if tri_count <= config.max_triangles_per_tile || depth >= config.max_depth {
        return MeshTileNode::Leaf {
            bounds,
            mesh: make_glb_mesh(positions, normals, indices),
            depth,
        };
    }

    // Find longest axis and split along its median.
    let he = bounds.half_extents();
    let axis = if he[0] >= he[1] && he[0] >= he[2] {
        0
    } else if he[1] >= he[2] {
        1
    } else {
        2
    };

    let center = bounds.center();
    let split = center[axis];

    // Classify triangles by centroid position along the split axis.
    let mut left_idx = Vec::new();
    let mut right_idx = Vec::new();

    for tri in indices.as_chunks::<3>().0 {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        let centroid = (positions[i0][axis] + positions[i1][axis] + positions[i2][axis]) / 3.0;
        if (centroid as f64) < split {
            left_idx.extend_from_slice(tri);
        } else {
            right_idx.extend_from_slice(tri);
        }
    }

    // Degenerate split — everything fell to one side.
    if left_idx.is_empty() || right_idx.is_empty() {
        return MeshTileNode::Leaf {
            bounds,
            mesh: make_glb_mesh(positions, normals, indices),
            depth,
        };
    }

    let left_bounds = aabb_from_indices(positions, &left_idx);
    let right_bounds = aabb_from_indices(positions, &right_idx);

    let left = build_recursive(
        positions,
        normals,
        &left_idx,
        left_bounds,
        depth + 1,
        config,
    );
    let right = build_recursive(
        positions,
        normals,
        &right_idx,
        right_bounds,
        depth + 1,
        config,
    );

    // LOD mesh for this internal node — simplified version of the full mesh at this level.
    let lod_indices = simplify_mesh(positions, indices, config.simplification_ratio);
    let lod_mesh = make_glb_mesh(positions, normals, &lod_indices);

    MeshTileNode::Internal {
        bounds,
        children: vec![Box::new(left), Box::new(right)],
        lod_mesh,
        depth,
    }
}

fn aabb_from_indices(positions: &[[f32; 3]], indices: &[u32]) -> Aabb {
    let mut aabb = Aabb::empty();
    for &i in indices {
        let p = positions[i as usize];
        aabb.expand_point([p[0] as f64, p[1] as f64, p[2] as f64]);
    }
    aabb
}

fn make_glb_mesh(positions: &[[f32; 3]], normals: &[[f32; 3]], indices: &[u32]) -> GlbMesh {
    let normals_opt = if normals.is_empty() {
        None
    } else {
        Some(normals.to_vec())
    };
    GlbMesh {
        positions: positions.to_vec(),
        normals: normals_opt,
        indices: Some(indices.to_vec()),
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: None,
        texture: None,
    }
}

// ── tree traversal helpers ──────────────────────────────────────────────────

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
            "root.glb".to_string()
        } else {
            let s: String = self.segments.iter().map(|i| char::from(b'0' + i)).collect();
            format!("{s}.glb")
        }
    }
}

fn write_node_glbs(node: &MeshTileNode, tiles_dir: &Path, path: &mut NodePath) -> io::Result<()> {
    let filename = path.to_filename();
    match node {
        MeshTileNode::Leaf { mesh, .. } => {
            if !mesh.positions.is_empty() {
                glb_writer::write_glb_file(mesh, &tiles_dir.join(&filename))?;
            }
        }
        MeshTileNode::Internal {
            children, lod_mesh, ..
        } => {
            if !lod_mesh.positions.is_empty() {
                glb_writer::write_glb_file(lod_mesh, &tiles_dir.join(&filename))?;
            }
            for (i, child) in children.iter().enumerate() {
                path.push(i as u8);
                write_node_glbs(child, tiles_dir, path)?;
                path.pop();
            }
        }
    }
    Ok(())
}

fn geometric_error_for(bounds: &Aabb) -> f64 {
    let h = bounds.half_extents();
    (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt()
}

fn generate_mesh_tileset(root: &MeshTileNode) -> Tileset {
    let root_tile = generate_tile(root, "tiles/", &mut NodePath::new());
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

fn generate_tile(node: &MeshTileNode, base_url: &str, path: &mut NodePath) -> Tile {
    let bounds = node.bounds();
    let bounding_volume = BoundingVolume::Box {
        r#box: bounds.to_3dtiles_box(),
    };

    match node {
        MeshTileNode::Leaf { mesh, .. } => {
            let content = if mesh.positions.is_empty() {
                None
            } else {
                Some(TileContent {
                    uri: format!("{base_url}{}", path.to_filename()),
                })
            };
            Tile {
                bounding_volume,
                geometric_error: 0.0,
                content,
                children: Vec::new(),
                refine: Some(Refine::Replace),
                transform: None,
            }
        }
        MeshTileNode::Internal {
            children, lod_mesh, ..
        } => {
            let content = if lod_mesh.positions.is_empty() {
                None
            } else {
                Some(TileContent {
                    uri: format!("{base_url}{}", path.to_filename()),
                })
            };

            let mut child_tiles = Vec::new();
            for (i, child) in children.iter().enumerate() {
                path.push(i as u8);
                child_tiles.push(generate_tile(child, base_url, path));
                path.pop();
            }

            Tile {
                bounding_volume,
                geometric_error: geometric_error_for(bounds),
                content,
                children: child_tiles,
                refine: Some(Refine::Replace),
                transform: None,
            }
        }
    }
}

fn collect_stats(node: &MeshTileNode, stats: &mut MeshTilingStats) {
    stats.tile_count += 1;
    stats.max_depth_reached = stats.max_depth_reached.max(node.depth());
    match node {
        MeshTileNode::Leaf { mesh, .. } => {
            stats.total_triangles += mesh.indices.as_ref().map_or(0, |i| i.len() / 3);
        }
        MeshTileNode::Internal { children, .. } => {
            for child in children {
                collect_stats(child, stats);
            }
        }
    }
}

/// Remap texcoords and extract texture sub-region for a subset of triangles.
///
/// Finds the bounding box of texcoords used by the given indices, crops the
/// original texture to that region, and remaps texcoords to `[0,1]` within
/// the cropped region.
pub fn split_texture(
    original: &glb_writer::TextureData,
    texcoords: &[[f32; 2]],
    indices: &[u32],
) -> (glb_writer::TextureData, Vec<[f32; 2]>) {
    // Find UV bounding box of the selected triangles.
    let mut u_min = f32::INFINITY;
    let mut u_max = f32::NEG_INFINITY;
    let mut v_min = f32::INFINITY;
    let mut v_max = f32::NEG_INFINITY;

    for &idx in indices {
        let uv = texcoords[idx as usize];
        u_min = u_min.min(uv[0]);
        u_max = u_max.max(uv[0]);
        v_min = v_min.min(uv[1]);
        v_max = v_max.max(uv[1]);
    }

    // Clamp to [0, 1].
    u_min = u_min.max(0.0);
    u_max = u_max.min(1.0);
    v_min = v_min.max(0.0);
    v_max = v_max.min(1.0);

    let u_range = (u_max - u_min).max(1e-6);
    let v_range = (v_max - v_min).max(1e-6);

    // Decode the original texture.
    let img =
        image::load_from_memory(&original.image_data).expect("texture image should be decodable");
    let (w, h) = img.dimensions();

    // Crop region in pixel space.
    let crop_x = (u_min * w as f32).floor() as u32;
    let crop_y = (v_min * h as f32).floor() as u32;
    let crop_w = ((u_range * w as f32).ceil() as u32).max(1).min(w - crop_x);
    let crop_h = ((v_range * h as f32).ceil() as u32).max(1).min(h - crop_y);

    let cropped = img.crop_imm(crop_x, crop_y, crop_w, crop_h);

    // Encode the cropped image.
    let mut buf = std::io::Cursor::new(Vec::new());
    let is_jpeg = original.mime_type == "image/jpeg";
    if is_jpeg {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
        cropped
            .to_rgb8()
            .write_with_encoder(encoder)
            .expect("JPEG encode");
    } else {
        cropped
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("PNG encode");
    }

    let new_texture = glb_writer::TextureData {
        image_data: buf.into_inner(),
        mime_type: original.mime_type.clone(),
        width: crop_w,
        height: crop_h,
    };

    // Remap texcoords to [0,1] within the cropped region.
    let remapped: Vec<[f32; 2]> = texcoords
        .iter()
        .map(|uv| [(uv[0] - u_min) / u_range, (uv[1] - v_min) / v_range])
        .collect();

    (new_texture, remapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_mesh() -> MeshData {
        #[rustfmt::skip]
        let positions = vec![
            // front
            [-0.5, -0.5,  0.5], [ 0.5, -0.5,  0.5], [ 0.5,  0.5,  0.5], [-0.5,  0.5,  0.5],
            // back
            [-0.5, -0.5, -0.5], [ 0.5, -0.5, -0.5], [ 0.5,  0.5, -0.5], [-0.5,  0.5, -0.5],
        ];
        #[rustfmt::skip]
        let indices = vec![
            0,1,2, 0,2,3,  // front
            5,4,7, 5,7,6,  // back
            4,0,3, 4,3,7,  // left
            1,5,6, 1,6,2,  // right
            3,2,6, 3,6,7,  // top
            4,5,1, 4,1,0,  // bottom
        ];
        let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
        MeshData {
            positions,
            normals,
            indices,
            name: "cube".into(),
        }
    }

    /// Generate a grid mesh with `n×n` quads (2n² triangles).
    fn grid_mesh(n: usize) -> MeshData {
        let mut positions = Vec::with_capacity((n + 1) * (n + 1));
        let mut normals = Vec::with_capacity(positions.capacity());
        let mut indices = Vec::with_capacity(n * n * 6);

        for y in 0..=n {
            for x in 0..=n {
                positions.push([x as f32, y as f32, 0.0]);
                normals.push([0.0, 0.0, 1.0]);
            }
        }
        let stride = (n + 1) as u32;
        for y in 0..n as u32 {
            for x in 0..n as u32 {
                let bl = y * stride + x;
                let br = bl + 1;
                let tl = bl + stride;
                let tr = tl + 1;
                indices.extend_from_slice(&[bl, br, tl, br, tr, tl]);
            }
        }

        MeshData {
            positions,
            normals,
            indices,
            name: "grid".into(),
        }
    }

    #[test]
    fn small_mesh_produces_leaf() {
        let config = MeshTilingConfig::default();
        let tree = build_mesh_tree(&[cube_mesh()], &config);
        assert!(
            matches!(tree, MeshTileNode::Leaf { .. }),
            "cube with 12 triangles should be a leaf"
        );
    }

    #[test]
    fn large_mesh_subdivides() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        // 256×256 grid = 131072 triangles — well above the 100 limit.
        let tree = build_mesh_tree(&[grid_mesh(256)], &config);
        assert!(
            matches!(tree, MeshTileNode::Internal { .. }),
            "large mesh should be subdivided"
        );
    }

    #[test]
    fn write_tileset_creates_files() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_tiler_test");
        let _ = std::fs::remove_dir_all(&dir);

        let config = MeshTilingConfig::default();
        let tree = build_mesh_tree(&[cube_mesh()], &config);
        write_mesh_tileset(&tree, &dir).expect("write_mesh_tileset failed");

        assert!(dir.join("tileset.json").exists());
        assert!(dir.join("tiles/root.glb").exists());

        // Validate tileset.json is parseable.
        let json = std::fs::read_to_string(dir.join("tileset.json")).unwrap();
        let tileset: Tileset = serde_json::from_str(&json).expect("invalid tileset.json");
        assert_eq!(tileset.asset.version, "1.1");
    }

    #[test]
    fn lod_has_fewer_triangles() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            simplification_ratio: 0.5,
            ..Default::default()
        };
        let mesh = grid_mesh(64); // 8192 triangles
        let tree = build_mesh_tree(&[mesh], &config);

        if let MeshTileNode::Internal { lod_mesh, .. } = &tree {
            let lod_tris = lod_mesh.indices.as_ref().map_or(0, |i| i.len() / 3);
            // LOD should have fewer triangles than the original 8192.
            assert!(
                lod_tris < 8192,
                "LOD mesh should have fewer triangles, got {lod_tris}"
            );
        } else {
            panic!("expected Internal node for 8192-triangle mesh with max 100");
        }
    }

    #[test]
    fn tile_meshes_pipeline() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_pipeline_test");
        let _ = std::fs::remove_dir_all(&dir);

        let config = MeshTilingConfig::default();
        let stats = tile_meshes(&[cube_mesh()], &dir, &config).expect("tile_meshes failed");

        assert_eq!(stats.tile_count, 1);
        assert_eq!(stats.total_triangles, 12);
        assert!(dir.join("tileset.json").exists());
    }

    fn make_test_texture(w: u32, h: u32) -> glb_writer::TextureData {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        glb_writer::TextureData {
            image_data: buf.into_inner(),
            mime_type: "image/png".to_string(),
            width: w,
            height: h,
        }
    }

    #[test]
    fn split_texture_crops_region() {
        let tex = make_test_texture(64, 64);
        // Texcoords covering the top-left quadrant.
        let texcoords = vec![[0.0, 0.0], [0.5, 0.0], [0.0, 0.5], [0.5, 0.5]];
        let indices = vec![0, 1, 2, 1, 3, 2];

        let (cropped, remapped) = split_texture(&tex, &texcoords, &indices);

        // Cropped texture should be roughly 32×32.
        assert!(cropped.width <= 33 && cropped.width >= 31);
        assert!(cropped.height <= 33 && cropped.height >= 31);

        // Remapped UVs should span [0,1].
        assert!((remapped[0][0] - 0.0).abs() < 0.01);
        assert!((remapped[1][0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn split_texture_preserves_valid_image() {
        let tex = make_test_texture(16, 16);
        let texcoords = vec![[0.25, 0.25], [0.75, 0.25], [0.5, 0.75]];
        let indices = vec![0, 1, 2];

        let (cropped, _remapped) = split_texture(&tex, &texcoords, &indices);

        // The cropped texture should be loadable.
        image::load_from_memory(&cropped.image_data).expect("cropped texture should be valid");
    }

    #[test]
    fn glb_with_texture_round_trip() {
        let tex = make_test_texture(4, 4);
        let mesh = GlbMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: Some(vec![[0.0, 0.0, 1.0]; 3]),
            indices: Some(vec![0, 1, 2]),
            colors: None,
            texcoords: Some(vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]),
            metadata: None,
            feature_ids: None,
            texture: Some(tex),
        };

        let mut buf = Vec::new();
        glb_writer::write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("GLB with texture should parse");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        assert!(json["images"].is_array());
        assert!(json["materials"].is_array());
        assert_eq!(json["meshes"][0]["primitives"][0]["material"], 0);
    }
}
