//! Mesh tiling pipeline — spatial subdivision, LOD generation, and 3D Tiles output for meshes.

use crate::bounding_volume::Aabb;
use crate::compression::simplify_mesh;
use crate::glb_writer::{self, GlbMesh, MetadataProperty, MetadataValues, TextureData, TileMetadata};
use crate::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};
use image::GenericImageView;
use std::io;
use std::path::Path;

/// Input mesh data (mirrors `tiletopia_ingest::MeshData`), carrying the one
/// material the GLB writer takes per mesh.
#[derive(Debug, Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// glTF UV space. Empty, or one per position.
    pub texcoords: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub name: String,
    /// The source element's stable identity, written into the tile as the
    /// `asset_id` feature property.
    pub asset_id: Option<String>,
    pub base_color_factor: Option<[f32; 4]>,
    pub texture: Option<TextureData>,
}

/// The `EXT_structural_metadata` class and property the tiler writes asset ids
/// under. The viewer joins a picked feature on this property.
const ASSET_CLASS_NAME: &str = "asset";
const ASSET_ID_PROPERTY: &str = "asset_id";

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
    /// Column-major 4x4 written as the root tile's `transform`, placing the
    /// model's local coordinates on the globe. Omitted from the tileset when None.
    pub root_transform: Option<[f64; 16]>,
    /// Whether the input is z-up and the written glTF has to be rotated to
    /// y-up. Bounding volumes stay in the z-up frame the tile transform names.
    pub content_y_up: bool,
}

impl Default for MeshTilingConfig {
    fn default() -> Self {
        Self {
            max_triangles_per_tile: 65536,
            max_depth: 10,
            simplification_ratio: 0.5,
            min_geometric_error: 0.1,
            root_transform: None,
            content_y_up: false,
        }
    }
}

/// Rotate z-up vectors into the y-up glTF tile content 3D Tiles expects. The
/// runtime rotates them back about x by π/2, so the two cancel.
fn z_up_to_y_up(vectors: &mut [[f32; 3]]) {
    for vector in vectors {
        *vector = [vector[0], vector[2], -vector[1]];
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

/// Build a mesh tile tree from a set of input meshes. Meshes that share a
/// material are tiled together; each further material gets its own subtree,
/// because a tile's GLB carries one material.
pub fn build_mesh_tree(meshes: &[MeshData], config: &MeshTilingConfig) -> MeshTileNode {
    let groups = group_by_material(meshes);

    let Some((first, rest)) = groups.split_first() else {
        return MeshTileNode::Leaf {
            bounds: Aabb::empty(),
            mesh: empty_glb_mesh(),
            depth: 0,
        };
    };

    if rest.is_empty() {
        return build_recursive(
            first,
            &first.indices,
            compute_aabb(&first.positions),
            0,
            config,
        );
    }

    let mut bounds = Aabb::empty();
    let mut children = Vec::with_capacity(groups.len());
    for group in &groups {
        let group_bounds = compute_aabb(&group.positions);
        bounds.expand_point(group_bounds.min);
        bounds.expand_point(group_bounds.max);
        children.push(Box::new(build_recursive(
            group,
            &group.indices,
            group_bounds,
            1,
            config,
        )));
    }

    MeshTileNode::Internal {
        bounds,
        children,
        lod_mesh: empty_glb_mesh(),
        depth: 0,
    }
}

/// Write mesh tiles (GLB files + tileset.json) to disk.
pub fn write_mesh_tileset(
    root: &MeshTileNode,
    output_dir: &Path,
    config: &MeshTilingConfig,
) -> io::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;

    write_node_glbs(root, &tiles_dir, &mut NodePath::new())?;

    let tileset = generate_mesh_tileset(root, config.root_transform);
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
    write_mesh_tileset(&root, output_dir, config)?;

    let mut stats = MeshTilingStats {
        total_triangles: 0,
        tile_count: 0,
        max_depth_reached: 0,
    };
    collect_stats(&root, &mut stats);
    Ok(stats)
}

// ── internals ───────────────────────────────────────────────────────────────

/// The meshes painted with one material, merged into one vertex and index
/// array.
struct MaterialGroup {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    texcoords: Vec<[f32; 2]>,
    indices: Vec<u32>,
    /// One per position, indexing `asset_ids`. Empty when no source mesh
    /// carried an asset id.
    feature_ids: Vec<u32>,
    /// One per feature, in feature id order.
    asset_ids: Vec<String>,
    base_color_factor: Option<[f32; 4]>,
    texture: Option<TextureData>,
}

fn group_by_material(meshes: &[MeshData]) -> Vec<MaterialGroup> {
    let mut groups: Vec<MaterialGroup> = Vec::new();
    let any_asset_id = meshes.iter().any(|mesh| mesh.asset_id.is_some());

    for mesh in meshes {
        if mesh.positions.is_empty() {
            continue;
        }
        let group = match groups.iter().position(|group| group.takes(mesh)) {
            Some(index) => &mut groups[index],
            None => {
                groups.push(MaterialGroup {
                    positions: Vec::new(),
                    normals: Vec::new(),
                    texcoords: Vec::new(),
                    indices: Vec::new(),
                    feature_ids: Vec::new(),
                    asset_ids: Vec::new(),
                    base_color_factor: mesh.base_color_factor,
                    texture: mesh.texture.clone(),
                });
                groups.last_mut().expect("just pushed")
            }
        };

        let base = group.positions.len() as u32;
        // an attribute only carries on while it lines up with the positions
        if group.normals.len() == base as usize && mesh.normals.len() == mesh.positions.len() {
            group.normals.extend_from_slice(&mesh.normals);
        }
        if group.texcoords.len() == base as usize && mesh.texcoords.len() == mesh.positions.len() {
            group.texcoords.extend_from_slice(&mesh.texcoords);
        }
        if any_asset_id {
            let feature_id = group.asset_ids.len() as u32;
            group
                .feature_ids
                .extend(std::iter::repeat_n(feature_id, mesh.positions.len()));
            group
                .asset_ids
                .push(mesh.asset_id.clone().unwrap_or_default());
        }
        group.positions.extend_from_slice(&mesh.positions);
        group.indices.extend(mesh.indices.iter().map(|i| i + base));
    }

    for group in &mut groups {
        if group.normals.len() != group.positions.len() {
            group.normals.clear();
        }
        if group.texcoords.len() != group.positions.len() {
            group.texcoords.clear();
        }
        if group.texture.is_some() && group.texcoords.is_empty() {
            tracing::warn!("mesh has a texture but no UVs, tiling it untextured");
            group.texture = None;
        }
    }

    groups
}

impl MaterialGroup {
    fn takes(&self, mesh: &MeshData) -> bool {
        if self.base_color_factor != mesh.base_color_factor {
            return false;
        }
        match (&self.texture, &mesh.texture) {
            (None, None) => true,
            (Some(mine), Some(theirs)) => mine.image_data == theirs.image_data,
            _ => false,
        }
    }
}

fn compute_aabb(positions: &[[f32; 3]]) -> Aabb {
    let mut aabb = Aabb::empty();
    for p in positions {
        aabb.expand_point([p[0] as f64, p[1] as f64, p[2] as f64]);
    }
    aabb
}

fn build_recursive(
    group: &MaterialGroup,
    indices: &[u32],
    bounds: Aabb,
    depth: u32,
    config: &MeshTilingConfig,
) -> MeshTileNode {
    let positions = &group.positions;
    let tri_count = indices.len() / 3;

    if tri_count <= config.max_triangles_per_tile || depth >= config.max_depth {
        return MeshTileNode::Leaf {
            bounds,
            mesh: make_glb_mesh(group, indices, config),
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
            mesh: make_glb_mesh(group, indices, config),
            depth,
        };
    }

    let left_bounds = aabb_from_indices(positions, &left_idx);
    let right_bounds = aabb_from_indices(positions, &right_idx);

    let left = build_recursive(group, &left_idx, left_bounds, depth + 1, config);
    let right = build_recursive(group, &right_idx, right_bounds, depth + 1, config);

    // LOD mesh for this internal node — simplified version of the full mesh at this level.
    // meshopt keeps the vertex array as it is, so texcoords still pair up.
    let lod_indices = simplify_mesh(positions, indices, config.simplification_ratio);
    let lod_mesh = make_glb_mesh(group, &lod_indices, config);

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

fn make_glb_mesh(group: &MaterialGroup, indices: &[u32], config: &MeshTilingConfig) -> GlbMesh {
    let mut positions = group.positions.clone();
    let mut normals = (!group.normals.is_empty()).then(|| group.normals.clone());
    if config.content_y_up {
        z_up_to_y_up(&mut positions);
        if let Some(normals) = &mut normals {
            z_up_to_y_up(normals);
        }
    }

    // a tile holding part of a textured mesh carries only the part of the
    // texture its triangles reach
    let split = group
        .texture
        .as_ref()
        .filter(|_| indices.len() < group.indices.len())
        .map(|texture| split_texture(texture, &group.texcoords, indices));
    let (texture, texcoords) = match split {
        Some((texture, texcoords)) => (Some(texture), Some(texcoords)),
        None => (
            group.texture.clone(),
            (!group.texcoords.is_empty()).then(|| group.texcoords.clone()),
        ),
    };

    // every tile of a group carries the whole group's vertex array, so the
    // property table covers every feature the tile can name
    let feature_ids = (!group.feature_ids.is_empty()).then(|| group.feature_ids.clone());
    let metadata = feature_ids.is_some().then(|| TileMetadata {
        class_name: ASSET_CLASS_NAME.to_string(),
        properties: vec![MetadataProperty {
            name: ASSET_ID_PROPERTY.to_string(),
            values: MetadataValues::String(group.asset_ids.clone()),
        }],
    });

    GlbMesh {
        positions,
        normals,
        indices: Some(indices.to_vec()),
        colors: None,
        texcoords,
        metadata,
        feature_ids,
        texture,
        base_color_factor: group.base_color_factor,
    }
}

fn empty_glb_mesh() -> GlbMesh {
    GlbMesh {
        positions: Vec::new(),
        normals: None,
        indices: None,
        colors: None,
        texcoords: None,
        metadata: None,
        feature_ids: None,
        texture: None,
        base_color_factor: None,
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
            return "root.glb".to_string();
        }
        let path: Vec<String> = self.segments.iter().map(u8::to_string).collect();
        format!("{}.glb", path.join("-"))
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

fn generate_mesh_tileset(root: &MeshTileNode, root_transform: Option<[f64; 16]>) -> Tileset {
    let mut root_tile = generate_tile(root, "tiles/", &mut NodePath::new());
    root_tile.transform = root_transform;
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

    // Crop region in pixel space. A UV of exactly 1 would start the crop past
    // the last pixel, so the origin stops one short of the edge.
    let crop_x = ((u_min * w as f32).floor() as u32).min(w - 1);
    let crop_y = ((v_min * h as f32).floor() as u32).min(h - 1);
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
            texcoords: Vec::new(),
            indices,
            name: "cube".into(),
            asset_id: None,
            base_color_factor: None,
            texture: None,
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
            texcoords: Vec::new(),
            indices,
            name: "grid".into(),
            asset_id: None,
            base_color_factor: None,
            texture: None,
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
        write_mesh_tileset(&tree, &dir, &config).expect("write_mesh_tileset failed");

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

    #[test]
    fn z_up_to_y_up_rotates_a_triangle_and_its_normal() {
        let mut triangle = vec![[1.0, 2.0, 3.0], [0.0, 0.0, 0.0], [-4.0, 5.0, -6.0]];
        z_up_to_y_up(&mut triangle);
        assert_eq!(
            triangle,
            vec![[1.0, 3.0, -2.0], [0.0, 0.0, 0.0], [-4.0, -6.0, -5.0]]
        );

        // the runtime rotates the other way by pi/2 about x, so the two cancel
        let mut up = vec![[0.0, 0.0, 1.0]];
        z_up_to_y_up(&mut up);
        assert_eq!(up, vec![[0.0, 1.0, 0.0]]);
    }

    #[test]
    fn y_up_content_leaves_the_bounding_volume_in_the_z_up_frame() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_y_up_test");
        let _ = std::fs::remove_dir_all(&dir);

        // a slab 4 wide, 2 deep and 6 tall in z
        let mesh = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [4.0, 2.0, 6.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: "slab".into(),
            asset_id: None,
            base_color_factor: None,
            texture: None,
        };
        let config = MeshTilingConfig {
            content_y_up: true,
            ..Default::default()
        };
        tile_meshes(&[mesh], &dir, &config).expect("tile_meshes failed");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        let half_extents = &json["root"]["boundingVolume"]["box"].as_array().unwrap()[3..];
        assert_eq!(half_extents[0].as_f64().unwrap(), 2.0);
        assert_eq!(half_extents[4].as_f64().unwrap(), 1.0);
        assert_eq!(half_extents[8].as_f64().unwrap(), 3.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_transform_reaches_the_tileset_root() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_root_transform_test");
        let _ = std::fs::remove_dir_all(&dir);

        let transform = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 100.0, 200.0, 300.0, 1.0,
        ];
        let config = MeshTilingConfig {
            root_transform: Some(transform),
            ..Default::default()
        };
        let tree = build_mesh_tree(&[cube_mesh()], &config);
        write_mesh_tileset(&tree, &dir, &config).expect("write_mesh_tileset failed");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        let written = json["root"]["transform"].as_array().expect("a transform");
        assert_eq!(written.len(), 16);
        assert_eq!(written[12].as_f64().unwrap(), 100.0);
        assert_eq!(written[14].as_f64().unwrap(), 300.0);

        let plain = MeshTilingConfig::default();
        write_mesh_tileset(&tree, &dir, &plain).expect("write_mesh_tileset failed");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        assert!(json["root"].get("transform").is_none());

        let _ = std::fs::remove_dir_all(&dir);
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

    /// A grid mesh with UVs spanning the whole texture.
    fn textured_grid_mesh(n: usize, texture: glb_writer::TextureData) -> MeshData {
        let mut mesh = grid_mesh(n);
        mesh.texcoords = mesh
            .positions
            .iter()
            .map(|p| [p[0] / n as f32, p[1] / n as f32])
            .collect();
        mesh.texture = Some(texture);
        mesh
    }

    /// The GLB's JSON, and the bytes each buffer view names.
    fn read_glb(path: &Path) -> (serde_json::Value, Vec<Vec<u8>>) {
        let bytes = std::fs::read(path).expect("a written tile");
        let glb = gltf::Glb::from_slice(&bytes).expect("a parseable GLB");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
        let bin = glb.bin.unwrap_or_default();
        let views = json["bufferViews"]
            .as_array()
            .map(|views| {
                views
                    .iter()
                    .map(|view| {
                        let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
                        let length = view["byteLength"].as_u64().unwrap() as usize;
                        bin[offset..offset + length].to_vec()
                    })
                    .collect()
            })
            .unwrap_or_default();
        (json, views)
    }

    fn texture_bytes(json: &serde_json::Value, views: &[Vec<u8>]) -> Vec<u8> {
        assert_eq!(
            json["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"], 0,
            "the material should point at the texture"
        );
        let source = json["textures"][0]["source"].as_u64().unwrap() as usize;
        let view = json["images"][source]["bufferView"].as_u64().unwrap() as usize;
        views[view].clone()
    }

    fn leaf_textures(node: &MeshTileNode, found: &mut Vec<(u32, u32)>) {
        match node {
            MeshTileNode::Leaf { mesh, .. } => {
                let texture = mesh.texture.as_ref().expect("a leaf texture");
                found.push((texture.width, texture.height));
            }
            MeshTileNode::Internal { children, .. } => {
                for child in children {
                    leaf_textures(child, found);
                }
            }
        }
    }

    #[test]
    fn a_textured_mesh_reaches_the_tile_glb() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_textured_tile_test");
        let _ = std::fs::remove_dir_all(&dir);

        let original = make_test_texture(8, 8);
        let original_bytes = original.image_data.clone();
        let mesh = textured_grid_mesh(2, original);
        tile_meshes(&[mesh], &dir, &MeshTilingConfig::default()).expect("tile_meshes failed");

        let (json, views) = read_glb(&dir.join("tiles/root.glb"));
        assert_eq!(json["images"][0]["mimeType"], "image/png");
        assert!(json["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_0"].is_number());
        // one tile holds the whole mesh, so the texture goes through uncropped
        assert_eq!(texture_bytes(&json, &views), original_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn splitting_a_textured_mesh_crops_the_texture_per_tile() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let mesh = textured_grid_mesh(32, make_test_texture(128, 128));
        let tree = build_mesh_tree(&[mesh], &config);

        let mut sizes = Vec::new();
        leaf_textures(&tree, &mut sizes);
        assert!(sizes.len() > 1, "the mesh should have been split");
        for (width, height) in &sizes {
            assert!(
                *width < 128 && *height < 128,
                "a tile's texture should be cropped, got {width}x{height}"
            );
        }
    }

    #[test]
    fn a_cropped_tile_texture_still_decodes_and_its_uvs_span_it() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let mesh = textured_grid_mesh(16, make_test_texture(64, 64));
        let tree = build_mesh_tree(&[mesh], &config);

        let MeshTileNode::Internal { children, .. } = &tree else {
            panic!("expected a split");
        };
        let (MeshTileNode::Leaf { mesh, .. } | MeshTileNode::Internal { lod_mesh: mesh, .. }) =
            &**children.first().unwrap();
        let texture = mesh.texture.as_ref().expect("a texture");
        image::load_from_memory(&texture.image_data).expect("a decodable crop");

        let texcoords = mesh.texcoords.as_ref().expect("texcoords");
        assert_eq!(texcoords.len(), mesh.positions.len());
        let used: Vec<[f32; 2]> = mesh
            .indices
            .as_ref()
            .unwrap()
            .iter()
            .map(|&i| texcoords[i as usize])
            .collect();
        let u_max = used.iter().map(|uv| uv[0]).fold(f32::MIN, f32::max);
        let v_max = used.iter().map(|uv| uv[1]).fold(f32::MIN, f32::max);
        assert!(
            (u_max - 1.0).abs() < 0.01,
            "u should reach the crop edge: {u_max}"
        );
        assert!(
            (v_max - 1.0).abs() < 0.01,
            "v should reach the crop edge: {v_max}"
        );
    }

    #[test]
    fn each_material_gets_its_own_subtree() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_two_materials_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut wall = textured_grid_mesh(2, make_test_texture(8, 8));
        wall.name = "wall".into();
        let mut roof = textured_grid_mesh(2, make_test_texture(4, 4));
        roof.name = "roof".into();

        tile_meshes(&[wall, roof], &dir, &MeshTilingConfig::default()).expect("tile_meshes failed");

        let tileset: Tileset =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        assert_eq!(tileset.root.children.len(), 2);
        assert!(
            tileset.root.content.is_none(),
            "the root holds no geometry of its own"
        );

        let sizes: Vec<(u32, u32)> = ["tiles/0.glb", "tiles/1.glb"]
            .iter()
            .map(|name| {
                let (json, views) = read_glb(&dir.join(name));
                let image = image::load_from_memory(&texture_bytes(&json, &views)).unwrap();
                (image.width(), image.height())
            })
            .collect();
        assert!(
            sizes.contains(&(8, 8)) && sizes.contains(&(4, 4)),
            "{sizes:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meshes_sharing_a_material_are_tiled_together() {
        let texture = make_test_texture(8, 8);
        let first = textured_grid_mesh(2, texture.clone());
        let second = textured_grid_mesh(2, texture);
        let tree = build_mesh_tree(&[first, second], &MeshTilingConfig::default());

        let MeshTileNode::Leaf { mesh, .. } = &tree else {
            panic!("one material should give one subtree");
        };
        assert_eq!(mesh.positions.len(), 18);
        assert_eq!(mesh.texcoords.as_ref().unwrap().len(), 18);
    }

    #[test]
    fn simplification_keeps_one_texcoord_per_vertex() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let mesh = textured_grid_mesh(32, make_test_texture(64, 64));
        let tree = build_mesh_tree(&[mesh], &config);

        let MeshTileNode::Internal { lod_mesh, .. } = &tree else {
            panic!("expected a split");
        };
        let texcoords = lod_mesh.texcoords.as_ref().expect("texcoords");
        assert_eq!(texcoords.len(), lod_mesh.positions.len());
        let highest = lod_mesh.indices.as_ref().unwrap().iter().max().unwrap();
        assert!((*highest as usize) < texcoords.len());
    }

    #[test]
    fn a_base_color_factor_reaches_the_tile_without_a_texture() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_base_color_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut mesh = cube_mesh();
        mesh.base_color_factor = Some([1.0, 0.0, 0.0, 1.0]);
        tile_meshes(&[mesh], &dir, &MeshTilingConfig::default()).expect("tile_meshes failed");

        let (json, _) = read_glb(&dir.join("tiles/root.glb"));
        assert_eq!(
            json["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"],
            serde_json::json!([1.0, 0.0, 0.0, 1.0])
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_texture_without_uvs_is_dropped() {
        let mut mesh = cube_mesh();
        mesh.texture = Some(make_test_texture(4, 4));
        let tree = build_mesh_tree(&[mesh], &MeshTilingConfig::default());

        let MeshTileNode::Leaf { mesh, .. } = &tree else {
            panic!("expected a leaf");
        };
        assert!(mesh.texture.is_none());
        assert!(mesh.texcoords.is_none());
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
            base_color_factor: None,
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
