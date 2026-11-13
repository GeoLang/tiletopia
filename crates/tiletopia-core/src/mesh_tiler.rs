//! Mesh tiling pipeline — spatial subdivision, LOD generation, and 3D Tiles output for meshes.

use crate::bounding_volume::Aabb;
use crate::compression::{
    SIMPLIFICATION_ERROR_FRACTION, reorder_vertices_for_meshopt, simplify_mesh,
};
use crate::glb_writer::{
    self, EdgeVisibility, GlbMesh, InstancedMesh, MeshCompression, MetadataProperty,
    MetadataValues, TextureData, TileMetadata,
};
use crate::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};
use basisu_c_sys::BasisTextureFormat;
use basisu_c_sys::common::BU_COMP_FLAGS_GEN_MIPS_CLAMP;
use basisu_c_sys::extra::{
    BasisuEncoder, BasisuEncoderParams, SourceImage, SourceImageFormat, basisu_encoder_init,
    types::Extent3d,
};
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::ops::Range;
use std::path::Path;

/// Input mesh data (mirrors `tiletopia_ingest::MeshData`), carrying the one
/// material the GLB writer takes per mesh.
#[derive(Debug, Clone)]
pub struct MeshData {
    pub positions: Vec<[f64; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// glTF UV space. Empty, or one per position.
    pub texcoords: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub name: String,
    /// The source element's stable identity, written into the tile as the
    /// `asset_id` feature property.
    pub asset_id: Option<String>,
    pub class: Option<String>,
    pub storey: Option<String>,
    pub base_color_factor: Option<[f32; 4]>,
    pub texture: Option<TextureData>,
    pub element_color: Option<[u8; 4]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VertexColors {
    Absent,
    Opaque,
    Translucent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shading {
    FromNormals,
    FromFaces,
}

// 2.5 degrees
const LEAST_COSINE_OF_A_FACE_NORMAL: f64 = 0.999;

struct WeldedGeometry {
    positions: Vec<[f64; 3]>,
    texcoords: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

/// The `EXT_structural_metadata` class and property the tiler writes asset ids
/// under. The viewer joins a picked feature on this property.
const ASSET_CLASS_NAME: &str = "asset";
const FEATURE_COLUMN_NAMES: [&str; 3] = ["asset_id", "class", "storey"];

impl MeshData {
    fn feature_columns(&self) -> [&Option<String>; FEATURE_COLUMN_NAMES.len()] {
        [&self.asset_id, &self.class, &self.storey]
    }

    fn vertex_colors(&self) -> VertexColors {
        match self.element_color {
            None => VertexColors::Absent,
            Some(color) if color[3] < u8::MAX => VertexColors::Translucent,
            Some(_) => VertexColors::Opaque,
        }
    }

    fn shading(&self) -> Shading {
        if self.normals.len() != self.positions.len() {
            return Shading::FromFaces;
        }
        let corner_follows_its_face = |corners: &[u32; 3]| {
            let [first, second, third] = corners.map(|corner| self.positions[corner as usize]);
            let face = cross(subtract(second, first), subtract(third, first));
            let face_length = dot(face, face).sqrt();
            if face_length == 0.0 {
                return true;
            }
            corners.iter().all(|&corner| {
                let normal = self.normals[corner as usize].map(f64::from);
                let normal_length = dot(normal, normal).sqrt();
                normal_length > 0.0
                    && dot(normal, face) / (normal_length * face_length)
                        >= LEAST_COSINE_OF_A_FACE_NORMAL
            })
        };
        if self
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .all(corner_follows_its_face)
        {
            Shading::FromFaces
        } else {
            Shading::FromNormals
        }
    }

    fn welded_by_position(&self) -> WeldedGeometry {
        let has_texcoords = self.texcoords.len() == self.positions.len();
        let mut welded = WeldedGeometry {
            positions: Vec::new(),
            texcoords: Vec::new(),
            indices: Vec::with_capacity(self.indices.len()),
        };
        let mut welded_index: HashMap<([u64; 3], [u32; 2]), u32> = HashMap::new();
        for &vertex in &self.indices {
            let position = self.positions[vertex as usize];
            let texcoord = if has_texcoords {
                self.texcoords[vertex as usize]
            } else {
                [0.0; 2]
            };
            // f64 negative zero has other bits than positive zero
            let key = (
                position.map(|component| (component + 0.0).to_bits()),
                texcoord.map(|component| (component + 0.0).to_bits()),
            );
            let index = *welded_index.entry(key).or_insert_with(|| {
                welded.positions.push(position);
                if has_texcoords {
                    welded.texcoords.push(texcoord);
                }
                welded.positions.len() as u32 - 1
            });
            welded.indices.push(index);
        }
        welded
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextureCompression {
    #[default]
    None,
    Ktx2,
}

/// Configuration for the mesh tiling pipeline.
#[derive(Debug, Clone)]
pub struct MeshTilingConfig {
    /// Maximum triangles per tile.
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
    pub mesh_compression: MeshCompression,
    pub texture_compression: TextureCompression,
    pub cad_edges: bool,
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
            mesh_compression: MeshCompression::None,
            texture_compression: TextureCompression::None,
            cad_edges: false,
        }
    }
}

/// Rotate z-up vectors into the y-up glTF tile content 3D Tiles expects. The
/// runtime rotates them back about x by π/2, so the two cancel.
fn z_up_to_y_up<T: Copy + std::ops::Neg<Output = T>>(vectors: &mut [[T; 3]]) {
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

struct WrittenMeshTile {
    bounds: Aabb,
    geometric_error: f64,
    content_uri: Option<String>,
    content_triangle_count: usize,
    children: Vec<WrittenMeshTile>,
    refine: Refine,
}

/// Full pipeline: build tree from meshes and write tiles to disk.
pub fn tile_meshes(
    meshes: &[MeshData],
    output_dir: &Path,
    config: &MeshTilingConfig,
) -> io::Result<MeshTilingStats> {
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;
    let root = write_mesh_tiles(meshes, &tiles_dir, config)?;

    let tileset = generate_mesh_tileset(&root, config);
    let mut tileset_file =
        io::BufWriter::new(std::fs::File::create(output_dir.join("tileset.json"))?);
    serde_json::to_writer_pretty(&mut tileset_file, &tileset).map_err(io::Error::other)?;
    tileset_file.flush()?;

    let mut stats = MeshTilingStats {
        total_triangles: 0,
        tile_count: 0,
        max_depth_reached: 0,
    };
    collect_stats(&root, 0, &mut stats);
    Ok(stats)
}

const MESH_BATCH_DIRECTORY: &str = "mesh-batches";

struct MeshBatchChild {
    tile_box: [f64; 12],
    geometric_error: f64,
    uri: String,
}

/// Parent tileset whose children are tilesets written by [`tile_meshes`].
pub struct MeshBatchTiles {
    output_dir: std::path::PathBuf,
    config: MeshTilingConfig,
    children: Vec<MeshBatchChild>,
    tile_count: usize,
    total_triangles: usize,
    max_depth_reached: u32,
}

impl MeshBatchTiles {
    pub fn new(output_dir: &Path, config: MeshTilingConfig) -> Self {
        Self {
            output_dir: output_dir.to_path_buf(),
            config,
            children: Vec::new(),
            tile_count: 0,
            total_triangles: 0,
            max_depth_reached: 0,
        }
    }

    /// Write `meshes` as one child tileset. The caller can drop `meshes` after this returns.
    pub fn write_batch(&mut self, meshes: &[MeshData]) -> io::Result<()> {
        let index = self.children.len();
        let batch_dir = self
            .output_dir
            .join(MESH_BATCH_DIRECTORY)
            .join(index.to_string());
        let mut child_config = self.config.clone();
        child_config.root_transform = None;
        let stats = tile_meshes(meshes, &batch_dir, &child_config)?;
        let tileset_path = batch_dir.join("tileset.json");
        let tileset: Tileset =
            serde_json::from_reader(io::BufReader::new(std::fs::File::open(&tileset_path)?))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not read {}: {error}",
                        tileset_path.display()
                    ))
                })?;
        let BoundingVolume::Box { r#box: tile_box } = tileset.root.bounding_volume else {
            return Err(io::Error::other(format!(
                "{} has no box bounds",
                tileset_path.display()
            )));
        };
        self.tile_count += stats.tile_count;
        self.total_triangles += stats.total_triangles;
        self.max_depth_reached = self.max_depth_reached.max(stats.max_depth_reached);
        self.children.push(MeshBatchChild {
            tile_box,
            geometric_error: tileset.geometric_error.max(tileset.root.geometric_error),
            uri: format!("{MESH_BATCH_DIRECTORY}/{index}/tileset.json"),
        });
        Ok(())
    }

    /// Record a tileset that was already written. Its box stays in this parent's frame.
    pub fn adopt_tileset(&mut self, tileset_path: &Path, uri: String) -> io::Result<()> {
        let tileset: Tileset =
            serde_json::from_reader(io::BufReader::new(std::fs::File::open(tileset_path)?))
                .map_err(|error| {
                    io::Error::other(format!(
                        "could not read {}: {error}",
                        tileset_path.display()
                    ))
                })?;
        let BoundingVolume::Box { r#box: tile_box } = tileset.root.bounding_volume else {
            return Err(io::Error::other(format!(
                "{} has no box bounds",
                tileset_path.display()
            )));
        };
        self.tile_count += count_tileset_nodes(&tileset.root);
        self.max_depth_reached = self.max_depth_reached.max(tileset_depth(&tileset.root, 0));
        self.children.push(MeshBatchChild {
            tile_box,
            geometric_error: tileset.geometric_error.max(tileset.root.geometric_error),
            uri,
        });
        Ok(())
    }

    pub fn finish(self) -> io::Result<MeshTilingStats> {
        if self.children.is_empty() {
            return Err(io::Error::other(
                "a mesh batch tileset needs at least one child",
            ));
        }
        let mut union = Aabb::empty();
        let mut geometric_error = 0.0f64;
        for child in &self.children {
            geometric_error = geometric_error.max(child.geometric_error);
            let center = [child.tile_box[0], child.tile_box[1], child.tile_box[2]];
            let half = [
                child.tile_box[3].abs(),
                child.tile_box[7].abs(),
                child.tile_box[11].abs(),
            ];
            union.expand_point([
                center[0] - half[0],
                center[1] - half[1],
                center[2] - half[2],
            ]);
            union.expand_point([
                center[0] + half[0],
                center[1] + half[1],
                center[2] + half[2],
            ]);
        }
        geometric_error = geometric_error.max(geometric_error_for(&union));
        let children = self
            .children
            .iter()
            .map(|child| Tile {
                bounding_volume: BoundingVolume::Box {
                    r#box: child.tile_box,
                },
                geometric_error: child.geometric_error,
                content: Some(TileContent {
                    uri: child.uri.clone(),
                }),
                children: Vec::new(),
                refine: None,
                transform: None,
            })
            .collect();
        let tileset = Tileset {
            asset: TilesetAsset {
                version: "1.1".to_string(),
                tileset_version: Some("1.0.0".to_string()),
                generator: Some("tiletopia".to_string()),
            },
            geometric_error,
            root: Tile {
                bounding_volume: BoundingVolume::Box {
                    r#box: union.to_3dtiles_box(),
                },
                geometric_error,
                content: None,
                children,
                refine: Some(Refine::Add),
                transform: self.config.root_transform,
            },
        };
        let mut tileset_file =
            io::BufWriter::new(std::fs::File::create(self.output_dir.join("tileset.json"))?);
        serde_json::to_writer_pretty(&mut tileset_file, &tileset).map_err(io::Error::other)?;
        tileset_file.flush()?;
        Ok(MeshTilingStats {
            total_triangles: self.total_triangles,
            tile_count: self.tile_count + 1 + self.children.len(),
            max_depth_reached: self.max_depth_reached,
        })
    }
}

/// Tile `meshes` with `config`.
///
/// When the `charge` of the whole list fits in `max_bytes_held`, this calls [`tile_meshes`]
/// once. Otherwise each batch that fits is its own child tileset under a parent tileset.
pub fn tile_meshes_within_budget(
    meshes: impl IntoIterator<Item = MeshData>,
    output_dir: &Path,
    config: &MeshTilingConfig,
    max_bytes_held: u64,
    mut charge: impl FnMut(&MeshData) -> u64,
) -> io::Result<MeshTilingStats> {
    let mut pending = Vec::new();
    let mut pending_bytes = 0u64;
    let mut batches: Option<MeshBatchTiles> = None;
    for mesh in meshes {
        let mesh_charge = charge(&mesh);
        if mesh_charge > max_bytes_held {
            return Err(io::Error::other(format!(
                "one mesh holds {mesh_charge} bytes, past the {max_bytes_held} byte limit"
            )));
        }
        #[cfg(test)]
        tests::note_resident_meshes(1);
        if pending_bytes.saturating_add(mesh_charge) > max_bytes_held && !pending.is_empty() {
            let batches =
                batches.get_or_insert_with(|| MeshBatchTiles::new(output_dir, config.clone()));
            batches.write_batch(&pending)?;
            #[cfg(test)]
            tests::note_resident_meshes(-(pending.len() as i64));
            pending.clear();
            pending_bytes = 0;
        }
        pending_bytes = pending_bytes.saturating_add(mesh_charge);
        pending.push(mesh);
    }
    if let Some(mut batches) = batches {
        if !pending.is_empty() {
            batches.write_batch(&pending)?;
            #[cfg(test)]
            tests::note_resident_meshes(-(pending.len() as i64));
            pending.clear();
        }
        return batches.finish();
    }
    tile_meshes(&pending, output_dir, config)
}

fn material_groups(meshes: &[MeshData], config: &MeshTilingConfig) -> Vec<MaterialGroup> {
    let mut groups = group_by_material(meshes);
    if config.texture_compression == TextureCompression::Ktx2 {
        // jpeg crops lose detail before the ktx2 encoder runs
        for group in &mut groups {
            group.texture = group.texture.as_ref().map(lossless_texture);
        }
    }
    groups
}

fn write_mesh_tiles(
    meshes: &[MeshData],
    tiles_dir: &Path,
    config: &MeshTilingConfig,
) -> io::Result<WrittenMeshTile> {
    let groups = material_groups(meshes, config);
    let tilers: Vec<GroupTiler> = groups
        .iter()
        .map(|group| {
            let mut group_bounds = compute_aabb(&group.positions);
            let group_center = group_bounds.center();
            for element in &group.elements {
                group_bounds.expand_point(element.bounds.min);
                group_bounds.expand_point(element.bounds.max);
            }
            GroupTiler {
                group,
                group_bounds,
                simplification_positions: group
                    .positions
                    .iter()
                    .map(|&position| offset_from(position, group_center))
                    .collect(),
                tiles_dir,
                config,
            }
        })
        .collect();
    let mut path = NodePath::new();

    match tilers.as_slice() {
        [] => Ok(WrittenMeshTile {
            bounds: Aabb::empty(),
            geometric_error: 0.0,
            content_uri: None,
            content_triangle_count: 0,
            children: Vec::new(),
            refine: Refine::Replace,
        }),
        [tiler] => tiler.tile_group(0, &mut path),
        tilers => {
            // glb_writer writes one material per tile
            let mut bounds = Aabb::empty();
            let mut children = Vec::with_capacity(tilers.len());
            for (index, tiler) in tilers.iter().enumerate() {
                bounds.expand_point(tiler.group_bounds.min);
                bounds.expand_point(tiler.group_bounds.max);
                path.push(index as u8);
                children.push(tiler.tile_group(1, &mut path)?);
                path.pop();
            }
            Ok(WrittenMeshTile {
                bounds,
                geometric_error: geometric_error_for(&bounds),
                content_uri: None,
                content_triangle_count: 0,
                children,
                refine: Refine::Replace,
            })
        }
    }
}

// ── internals ───────────────────────────────────────────────────────────────

/// The meshes painted with one material, merged into one vertex and index
/// array.
struct MaterialGroup {
    positions: Vec<[f64; 3]>,
    normals: Vec<[f32; 3]>,
    texcoords: Vec<[f32; 2]>,
    indices: Vec<u32>,
    feature_ids: Vec<u32>,
    feature_rows: Vec<[String; FEATURE_COLUMN_NAMES.len()]>,
    colors: Vec<[u8; 4]>,
    vertex_colors: VertexColors,
    shading: Shading,
    base_color_factor: Option<[f32; 4]>,
    texture: Option<TextureData>,
    elements: Vec<GroupElement>,
}

// a tile's part list and one child's list both hold a part while the tile splits, and parent content sorts references to them
pub const TILER_BYTES_PER_MESH: u64 = (std::mem::size_of::<MeshData>()
    + std::mem::size_of::<GroupElement>()
    + 2 * std::mem::size_of::<TilePart<'static>>()
    + std::mem::size_of::<&TilePart<'static>>()
    + std::mem::size_of::<(u64, Vec<(usize, [[f64; 3]; 3])>)>()
    + std::mem::size_of::<(usize, [[f64; 3]; 3])>()
    + std::mem::size_of::<[String; FEATURE_COLUMN_NAMES.len()]>())
    as u64;

struct GroupElement {
    indices: Range<usize>,
    vertices: Range<u32>,
    bounds: Aabb,
    repeated_geometry: Option<RepeatedGeometry>,
}

#[derive(Clone, Copy)]
struct RepeatedGeometry {
    first_occurrence: usize,
    instance_rotation: [f32; 4],
    instance_translation: [f64; 3],
}

fn group_by_material(meshes: &[MeshData]) -> Vec<MaterialGroup> {
    let mut groups: Vec<MaterialGroup> = Vec::new();
    let any_feature_column = meshes
        .iter()
        .any(|mesh| mesh.feature_columns().iter().any(|value| value.is_some()));

    for mesh in meshes {
        if mesh.positions.is_empty() {
            continue;
        }
        let shading = mesh.shading();
        let welded = (shading == Shading::FromFaces).then(|| mesh.welded_by_position());
        let (positions, normals, texcoords, indices) = match &welded {
            Some(welded) => (
                welded.positions.as_slice(),
                &[][..],
                welded.texcoords.as_slice(),
                welded.indices.as_slice(),
            ),
            None => (
                mesh.positions.as_slice(),
                mesh.normals.as_slice(),
                mesh.texcoords.as_slice(),
                mesh.indices.as_slice(),
            ),
        };
        let group = match groups.iter().position(|group| group.takes(mesh, shading)) {
            Some(index) => &mut groups[index],
            None => {
                groups.push(MaterialGroup {
                    positions: Vec::new(),
                    normals: Vec::new(),
                    texcoords: Vec::new(),
                    indices: Vec::new(),
                    feature_ids: Vec::new(),
                    feature_rows: Vec::new(),
                    colors: Vec::new(),
                    vertex_colors: mesh.vertex_colors(),
                    shading,
                    base_color_factor: mesh.base_color_factor,
                    texture: mesh.texture.clone(),
                    elements: Vec::new(),
                });
                groups.last_mut().expect("just pushed")
            }
        };

        let base = group.positions.len() as u32;
        // an attribute only carries on while it lines up with the positions
        if group.normals.len() == base as usize && normals.len() == positions.len() {
            group.normals.extend_from_slice(normals);
        }
        if group.texcoords.len() == base as usize && texcoords.len() == positions.len() {
            group.texcoords.extend_from_slice(texcoords);
        }
        if any_feature_column {
            let feature_id = group.feature_rows.len() as u32;
            group
                .feature_ids
                .extend(std::iter::repeat_n(feature_id, positions.len()));
            group.feature_rows.push(
                mesh.feature_columns()
                    .map(|value| value.clone().unwrap_or_default()),
            );
        }
        if let Some(color) = mesh.element_color {
            group
                .colors
                .extend(std::iter::repeat_n(color, positions.len()));
        }
        group.positions.extend_from_slice(positions);
        let first_index = group.indices.len();
        group.indices.extend(indices.iter().map(|i| i + base));
        if !indices.is_empty() {
            group.elements.push(GroupElement {
                bounds: aabb_from_indices(&group.positions, &group.indices[first_index..]),
                indices: first_index..group.indices.len(),
                vertices: base..group.positions.len() as u32,
                repeated_geometry: None,
            });
        }
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
        if group.texcoords.is_empty() {
            find_repeated_geometry(group);
        }
    }

    groups
}

fn subtract(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn scaled(vector: [f64; 3], factor: f64) -> [f64; 3] {
    vector.map(|component| component * factor)
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

const REPEATED_GEOMETRY_TOLERANCE_METRES: f64 = 1e-3;
const NORMAL_TOLERANCE: f64 = 1e-2;
const IDENTITY_ROTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn rigid_frame(positions: &[[f64; 3]]) -> Option<[[f64; 3]; 3]> {
    let least_length_squared = REPEATED_GEOMETRY_TOLERANCE_METRES.powi(2);
    let origin = positions[0];
    let from_origin = |position: &[f64; 3]| subtract(*position, origin);
    let farthest_squared = positions
        .iter()
        .map(|position| dot(from_origin(position), from_origin(position)))
        .fold(0.0, f64::max);
    if farthest_squared <= least_length_squared {
        return None;
    }
    // symmetric shapes tie for the farthest vertex, and floating point noise breaks the tie differently per copy
    let first_axis = positions
        .iter()
        .map(from_origin)
        .find(|offset| dot(*offset, *offset) >= farthest_squared / 4.0)?;
    let first_axis = scaled(first_axis, dot(first_axis, first_axis).sqrt().recip());

    let off_axis = |position: &[f64; 3]| {
        let offset = from_origin(position);
        subtract(offset, scaled(first_axis, dot(offset, first_axis)))
    };
    let widest_squared = positions
        .iter()
        .map(|position| dot(off_axis(position), off_axis(position)))
        .fold(0.0, f64::max);
    if widest_squared <= least_length_squared {
        return None;
    }
    let second_axis = positions
        .iter()
        .map(off_axis)
        .find(|offset| dot(*offset, *offset) >= widest_squared / 4.0)?;
    let second_axis = scaled(second_axis, dot(second_axis, second_axis).sqrt().recip());
    Some([first_axis, second_axis, cross(first_axis, second_axis)])
}

fn rotation_between_frames(from: &[[f64; 3]; 3], to: &[[f64; 3]; 3]) -> [f32; 4] {
    let element = |row: usize, column: usize| -> f64 {
        (0..3).map(|axis| to[axis][row] * from[axis][column]).sum()
    };
    let (m00, m01, m02) = (element(0, 0), element(0, 1), element(0, 2));
    let (m10, m11, m12) = (element(1, 0), element(1, 1), element(1, 2));
    let (m20, m21, m22) = (element(2, 0), element(2, 1), element(2, 2));

    let trace = m00 + m11 + m22;
    let [x, y, z, w] = if trace > 0.0 {
        let scale = (trace + 1.0).sqrt() * 2.0;
        [
            (m21 - m12) / scale,
            (m02 - m20) / scale,
            (m10 - m01) / scale,
            scale / 4.0,
        ]
    } else if m00 > m11 && m00 > m22 {
        let scale = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [
            scale / 4.0,
            (m01 + m10) / scale,
            (m02 + m20) / scale,
            (m21 - m12) / scale,
        ]
    } else if m11 > m22 {
        let scale = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [
            (m01 + m10) / scale,
            scale / 4.0,
            (m12 + m21) / scale,
            (m02 - m20) / scale,
        ]
    } else {
        let scale = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [
            (m02 + m20) / scale,
            (m12 + m21) / scale,
            scale / 4.0,
            (m10 - m01) / scale,
        ]
    };
    [x as f32, y as f32, z as f32, w as f32]
}

fn rotated(rotation: [f32; 4], vector: [f64; 3]) -> [f64; 3] {
    let vector_part = [
        f64::from(rotation[0]),
        f64::from(rotation[1]),
        f64::from(rotation[2]),
    ];
    let scalar_part = f64::from(rotation[3]);
    let first_turn = cross(vector_part, vector);
    let second_turn = cross(vector_part, first_turn);
    std::array::from_fn(|axis| {
        vector[axis] + 2.0 * (scalar_part * first_turn[axis] + second_turn[axis])
    })
}

const SHAPE_KEY_STEP_METRES: f64 = 0.01;

fn geometry_key(group: &MaterialGroup, element: &GroupElement) -> u64 {
    let positions = group.element_positions(element);
    let vertex_count = positions.len() as f64;
    let mut centroid = [0.0_f64; 3];
    for position in positions {
        centroid = std::array::from_fn(|axis| centroid[axis] + position[axis] / vertex_count);
    }
    let distance = |from: [f64; 3], to: [f64; 3]| {
        let offset = subtract(from, to);
        dot(offset, offset).sqrt()
    };
    let mean_from_first_vertex = positions
        .iter()
        .map(|position| distance(*position, positions[0]))
        .sum::<f64>()
        / vertex_count;
    let mean_from_centroid = positions
        .iter()
        .map(|position| distance(*position, centroid))
        .sum::<f64>()
        / vertex_count;

    let mut hasher = DefaultHasher::new();
    positions.len().hash(&mut hasher);
    group
        .colors
        .get(element.vertices.start as usize)
        .hash(&mut hasher);
    for index in &group.indices[element.indices.clone()] {
        (index - element.vertices.start).hash(&mut hasher);
    }
    // f32 source vertices differ per copy, and rounding each one lands copies on different keys
    for measure in [mean_from_first_vertex, mean_from_centroid] {
        ((measure / SHAPE_KEY_STEP_METRES).round() as i64).hash(&mut hasher);
    }
    hasher.finish()
}

fn instance_placement(
    group: &MaterialGroup,
    first_occurrence: usize,
    element: &GroupElement,
    instance_rotation: [f32; 4],
) -> Option<RepeatedGeometry> {
    let first = &group.elements[first_occurrence];
    let first_positions = group.element_positions(first);
    let positions = group.element_positions(element);
    let first_center = first.bounds.center();
    let instance_translation = subtract(
        positions[0],
        rotated(
            instance_rotation,
            subtract(first_positions[0], first_center),
        ),
    );

    let positions_match =
        first_positions
            .iter()
            .zip(positions)
            .all(|(first_position, position)| {
                let placed = rotated(instance_rotation, subtract(*first_position, first_center));
                let offset = subtract(subtract(*position, instance_translation), placed);
                offset
                    .iter()
                    .all(|component| component.abs() <= REPEATED_GEOMETRY_TOLERANCE_METRES)
            });
    if !positions_match {
        return None;
    }

    if !group.normals.is_empty() {
        let first_normals =
            &group.normals[first.vertices.start as usize..first.vertices.end as usize];
        let normals =
            &group.normals[element.vertices.start as usize..element.vertices.end as usize];
        let normals_match = first_normals
            .iter()
            .zip(normals)
            .all(|(first_normal, normal)| {
                let placed = rotated(instance_rotation, first_normal.map(f64::from));
                let offset = subtract(normal.map(f64::from), placed);
                offset
                    .iter()
                    .all(|component| component.abs() <= NORMAL_TOLERANCE)
            });
        if !normals_match {
            return None;
        }
    }

    Some(RepeatedGeometry {
        first_occurrence,
        instance_rotation,
        instance_translation,
    })
}

type ElementWithFrame = (usize, [[f64; 3]; 3]);

fn find_repeated_geometry(group: &mut MaterialGroup) {
    let mut first_occurrences: HashMap<u64, Vec<ElementWithFrame>> = HashMap::new();
    for element_index in 0..group.elements.len() {
        let element = &group.elements[element_index];
        let Some(frame) = rigid_frame(group.element_positions(element)) else {
            continue;
        };
        let candidates = first_occurrences
            .entry(geometry_key(group, element))
            .or_default();
        let matched = candidates
            .iter()
            .find_map(|(first_occurrence, first_frame)| {
                let first = &group.elements[*first_occurrence];
                let same_triangles = first.vertices.len() == element.vertices.len()
                    && group.indices[first.indices.clone()]
                        .iter()
                        .map(|index| index - first.vertices.start)
                        .eq(group.indices[element.indices.clone()]
                            .iter()
                            .map(|index| index - element.vertices.start));
                if !same_triangles {
                    return None;
                }
                instance_placement(group, *first_occurrence, element, IDENTITY_ROTATION).or_else(
                    || {
                        let rotation = rotation_between_frames(first_frame, &frame);
                        instance_placement(group, *first_occurrence, element, rotation)
                    },
                )
            });
        let repeated_geometry = match matched {
            Some(repeated_geometry) => {
                let bounds = &mut group.elements[element_index].bounds;
                bounds.min = bounds
                    .min
                    .map(|value| value - REPEATED_GEOMETRY_TOLERANCE_METRES);
                bounds.max = bounds
                    .max
                    .map(|value| value + REPEATED_GEOMETRY_TOLERANCE_METRES);
                repeated_geometry
            }
            None => {
                candidates.push((element_index, frame));
                RepeatedGeometry {
                    first_occurrence: element_index,
                    instance_rotation: IDENTITY_ROTATION,
                    instance_translation: element.bounds.center(),
                }
            }
        };
        group.elements[element_index].repeated_geometry = Some(repeated_geometry);
    }
}

impl MaterialGroup {
    fn element_positions(&self, element: &GroupElement) -> &[[f64; 3]] {
        &self.positions[element.vertices.start as usize..element.vertices.end as usize]
    }

    fn takes(&self, mesh: &MeshData, shading: Shading) -> bool {
        if self.base_color_factor != mesh.base_color_factor
            || self.vertex_colors != mesh.vertex_colors()
            || self.shading != shading
        {
            return false;
        }
        match (&self.texture, &mesh.texture) {
            (None, None) => true,
            (Some(mine), Some(theirs)) => mine.image_data == theirs.image_data,
            _ => false,
        }
    }
}

fn compute_aabb(positions: &[[f64; 3]]) -> Aabb {
    let mut aabb = Aabb::empty();
    for p in positions {
        aabb.expand_point(*p);
    }
    aabb
}

fn offset_from(position: [f64; 3], origin: [f64; 3]) -> [f32; 3] {
    [
        (position[0] - origin[0]) as f32,
        (position[1] - origin[1]) as f32,
        (position[2] - origin[2]) as f32,
    ]
}

struct GroupTiler<'a> {
    group: &'a MaterialGroup,
    group_bounds: Aabb,
    // meshopt::simplify takes f32 positions, spaced 6 cm apart at 1000 km from zero
    simplification_positions: Vec<[f32; 3]>,
    tiles_dir: &'a Path,
    config: &'a MeshTilingConfig,
}

#[derive(Clone)]
struct TilePart<'a> {
    whole_element: Option<usize>,
    indices: Cow<'a, [u32]>,
    bounds: Aabb,
}

struct ParentContent<'a> {
    parts: Vec<TilePart<'a>>,
    geometric_error: f64,
}

impl TilePart<'_> {
    fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

fn longest_axis(bounds: &Aabb) -> usize {
    let half_extents = bounds.half_extents();
    if half_extents[0] >= half_extents[1] && half_extents[0] >= half_extents[2] {
        0
    } else if half_extents[1] >= half_extents[2] {
        1
    } else {
        2
    }
}

fn total_triangles(parts: &[TilePart<'_>]) -> usize {
    parts.iter().map(TilePart::triangle_count).sum()
}

fn parts_bounds(parts: &[TilePart<'_>]) -> Aabb {
    let mut bounds = Aabb::empty();
    for part in parts {
        bounds.expand_point(part.bounds.min);
        bounds.expand_point(part.bounds.max);
    }
    bounds
}

fn split_parts_by_triangle_count<'a>(
    parts: &[TilePart<'a>],
    axis: usize,
) -> (Vec<TilePart<'a>>, Vec<TilePart<'a>>) {
    let half_of_the_triangles = total_triangles(parts) / 2;
    let mut by_center: Vec<&TilePart<'a>> = parts.iter().collect();
    by_center
        .sort_by(|left, right| left.bounds.center()[axis].total_cmp(&right.bounds.center()[axis]));

    let mut near = Vec::new();
    let mut far = Vec::new();
    let mut near_triangles = 0;
    for (position, part) in by_center.iter().enumerate() {
        let is_last = position + 1 == by_center.len();
        if near.is_empty() || (near_triangles < half_of_the_triangles && !is_last) {
            near_triangles += part.triangle_count();
            near.push((*part).clone());
        } else {
            far.push((*part).clone());
        }
    }
    (near, far)
}

// each instanced mesh adds one draw call and about 1 KB of glTF JSON
const LEAST_BYTES_SAVED_BY_AN_INSTANCED_MESH: usize = 4096;
// a VEC3 translation, a VEC4 rotation and a SCALAR feature id, all FLOAT
const MOST_BYTES_PER_INSTANCE: usize = 32;

struct InstancedElements<'p> {
    first_occurrence: usize,
    elements: Vec<usize>,
    first_element_indices: &'p [u32],
}

impl InstancedElements<'_> {
    fn saves_bytes(&self, group: &MaterialGroup) -> bool {
        let mut used_vertices = self.first_element_indices.to_vec();
        used_vertices.sort_unstable();
        used_vertices.dedup();
        let bytes_per_vertex = 12
            + if group.normals.is_empty() { 0 } else { 12 }
            + if group.feature_ids.is_empty() { 0 } else { 4 }
            + if group.colors.is_empty() { 0 } else { 4 };
        let geometry_bytes =
            used_vertices.len() * bytes_per_vertex + self.first_element_indices.len() * 4;
        let instance_count = self.elements.len();
        (instance_count - 1) * geometry_bytes
            >= instance_count * MOST_BYTES_PER_INSTANCE + LEAST_BYTES_SAVED_BY_AN_INSTANCED_MESH
    }
}

fn split_instanced_parts<'p>(
    group: &MaterialGroup,
    parts: &'p [TilePart<'_>],
    cad_edges: bool,
) -> (Vec<u32>, Vec<InstancedElements<'p>>) {
    if cad_edges {
        let mut direct_indices = Vec::new();
        for part in parts {
            direct_indices.extend_from_slice(&part.indices);
        }
        return (direct_indices, Vec::new());
    }

    let mut direct_indices = Vec::new();
    let mut by_first_occurrence: BTreeMap<usize, InstancedElements<'p>> = BTreeMap::new();
    for part in parts {
        let repeated = part.whole_element.and_then(|element| {
            let repeated = group.elements[element].repeated_geometry?;
            Some((element, repeated))
        });
        match repeated {
            Some((element, repeated)) => {
                by_first_occurrence
                    .entry(repeated.first_occurrence)
                    .or_insert_with(|| InstancedElements {
                        first_occurrence: repeated.first_occurrence,
                        elements: Vec::new(),
                        first_element_indices: &part.indices,
                    })
                    .elements
                    .push(element);
            }
            None => direct_indices.extend_from_slice(&part.indices),
        }
    }

    let mut instanced = Vec::new();
    for instanced_elements in by_first_occurrence.into_values() {
        if instanced_elements.saves_bytes(group) {
            instanced.push(instanced_elements);
            continue;
        }
        let first_start = group.elements[instanced_elements.elements[0]]
            .vertices
            .start;
        for &element in &instanced_elements.elements {
            let start = group.elements[element].vertices.start;
            direct_indices.extend(
                instanced_elements
                    .first_element_indices
                    .iter()
                    .map(|index| index - first_start + start),
            );
        }
    }
    (direct_indices, instanced)
}

impl<'a> GroupTiler<'a> {
    fn whole_element_parts(&self) -> Vec<TilePart<'a>> {
        let group: &'a MaterialGroup = self.group;
        group
            .elements
            .iter()
            .enumerate()
            .map(|(element_index, element)| TilePart {
                whole_element: Some(element_index),
                indices: Cow::Borrowed(&group.indices[element.indices.clone()]),
                bounds: element.bounds,
            })
            .collect()
    }

    fn tile_group(&self, depth: u32, path: &mut NodePath) -> io::Result<WrittenMeshTile> {
        let group = self.group;
        let parts = self.whole_element_parts();
        // pieces of one surface, like photogrammetry, leave holes when a small one is not drawn, and only they come without an asset id
        let every_element_has_an_asset_id = !group.feature_ids.is_empty()
            && group.elements.iter().all(|element| {
                let feature = group.feature_ids[element.vertices.start as usize];
                !group.feature_rows[feature as usize][0].is_empty()
            });
        if every_element_has_an_asset_id {
            self.tile_additive(self.parts_within_budget(parts), depth, path)
        } else {
            self.tile(parts, self.group_bounds, depth, path)
        }
    }

    fn parts_within_budget(&self, parts: Vec<TilePart<'a>>) -> Vec<TilePart<'a>> {
        let mut pending = parts;
        let mut within_budget = Vec::with_capacity(pending.len());
        while let Some(part) = pending.pop() {
            if part.triangle_count() <= self.config.max_triangles_per_tile {
                within_budget.push(part);
                continue;
            }
            let axis = longest_axis(&part.bounds);
            let (mut near, mut far) =
                self.split_triangles(&part.indices, axis, part.bounds.center()[axis]);
            if near.is_empty() || far.is_empty() {
                near = part.indices[..part.indices.len() / 6 * 3].to_vec();
                far = part.indices[near.len()..].to_vec();
            }
            pending.push(self.anonymous_part(near));
            pending.push(self.anonymous_part(far));
        }
        within_budget
    }

    fn anonymous_part(&self, indices: Vec<u32>) -> TilePart<'a> {
        TilePart {
            whole_element: None,
            bounds: aabb_from_indices(&self.group.positions, &indices),
            indices: Cow::Owned(indices),
        }
    }

    fn split_triangles(&self, indices: &[u32], axis: usize, split: f64) -> (Vec<u32>, Vec<u32>) {
        let positions = &self.group.positions;
        let mut near = Vec::new();
        let mut far = Vec::new();
        for triangle in indices.as_chunks::<3>().0 {
            let centroid = triangle
                .iter()
                .map(|&index| positions[index as usize][axis])
                .sum::<f64>()
                / 3.0;
            if centroid < split {
                near.extend_from_slice(triangle);
            } else {
                far.extend_from_slice(triangle);
            }
        }
        (near, far)
    }

    fn tile_additive(
        &self,
        parts: Vec<TilePart<'a>>,
        depth: u32,
        path: &mut NodePath,
    ) -> io::Result<WrittenMeshTile> {
        let bounds = parts_bounds(&parts);
        let triangle_budget = self.config.max_triangles_per_tile;
        if total_triangles(&parts) <= triangle_budget || depth >= self.config.max_depth {
            return self.leaf(&parts, bounds, path);
        }

        let mut content = parts;
        content.sort_by(|left, right| {
            geometric_error_for(&right.bounds).total_cmp(&geometric_error_for(&left.bounds))
        });
        let mut content_triangles = 0;
        let content_count = content
            .iter()
            .take_while(|part| {
                content_triangles += part.triangle_count();
                content_triangles <= triangle_budget
            })
            .count();
        let rest = content.split_off(content_count);
        let content_uri = self.write_content(&content, path)?;
        // cesium leaves the children unloaded while this tile's error is small on screen, and the largest element they hold is what goes missing
        let geometric_error = geometric_error_for(&rest[0].bounds);

        let rest_bounds = parts_bounds(&rest);
        let axis = longest_axis(&rest_bounds);
        let (near, far) = self.split_parts(&rest, axis, rest_bounds.center()[axis]);
        drop(rest);
        let mut children = Vec::with_capacity(2);
        for (index, half) in [near, far].into_iter().enumerate() {
            if half.is_empty() {
                continue;
            }
            path.push(index as u8);
            children.push(self.tile_additive(half, depth + 1, path)?);
            path.pop();
        }

        Ok(WrittenMeshTile {
            bounds,
            geometric_error,
            content_uri,
            content_triangle_count: total_triangles(&content),
            children,
            refine: Refine::Add,
        })
    }

    fn tile(
        &self,
        parts: Vec<TilePart<'a>>,
        bounds: Aabb,
        depth: u32,
        path: &mut NodePath,
    ) -> io::Result<WrittenMeshTile> {
        let tri_count = total_triangles(&parts);

        if tri_count <= self.config.max_triangles_per_tile || depth >= self.config.max_depth {
            return self.leaf(&parts, bounds, path);
        }

        let axis = longest_axis(&bounds);
        let (near, far) = self.split_parts(&parts, axis, bounds.center()[axis]);

        // Degenerate split: one part whose triangles all fall to one side.
        if near.is_empty() || far.is_empty() {
            return self.leaf(&parts, bounds, path);
        }

        let content = self.parent_content(&parts, tri_count);
        let content_uri = self.write_content(&content.parts, path)?;
        let content_triangle_count = total_triangles(&content.parts);
        drop(parts);

        let mut children = Vec::with_capacity(2);
        for (index, half) in [near, far].into_iter().enumerate() {
            let half_bounds = parts_bounds(&half);
            path.push(index as u8);
            children.push(self.tile(half, half_bounds, depth + 1, path)?);
            path.pop();
        }
        // cesium refines a tile only while its error is too large on screen, so a parent below a child would stop refinement early
        let geometric_error = children
            .iter()
            .map(|child| child.geometric_error)
            .fold(content.geometric_error, f64::max);

        Ok(WrittenMeshTile {
            bounds,
            geometric_error,
            content_uri,
            content_triangle_count,
            children,
            refine: Refine::Replace,
        })
    }

    fn split_parts(
        &self,
        parts: &[TilePart<'a>],
        axis: usize,
        split: f64,
    ) -> (Vec<TilePart<'a>>, Vec<TilePart<'a>>) {
        let mut near = Vec::new();
        let mut far = Vec::new();
        for part in parts {
            if part.triangle_count() <= self.config.max_triangles_per_tile {
                if part.bounds.center()[axis] < split {
                    near.push(part.clone());
                } else {
                    far.push(part.clone());
                }
                continue;
            }

            let (near_indices, far_indices) = self.split_triangles(&part.indices, axis, split);
            if near_indices.is_empty() {
                far.push(part.clone());
            } else if far_indices.is_empty() {
                near.push(part.clone());
            } else {
                near.push(self.anonymous_part(near_indices));
                far.push(self.anonymous_part(far_indices));
            }
        }
        if near.is_empty() || far.is_empty() {
            return split_parts_by_triangle_count(parts, axis);
        }
        (near, far)
    }

    fn parent_content(&self, parts: &[TilePart<'a>], tri_count: usize) -> ParentContent<'a> {
        let triangle_budget = self.config.max_triangles_per_tile;
        let ratio = self
            .config
            .simplification_ratio
            .min(triangle_budget as f32 / tri_count as f32);

        let mut largest_first: Vec<&TilePart<'a>> = parts.iter().collect();
        largest_first.sort_by(|left, right| {
            geometric_error_for(&right.bounds).total_cmp(&geometric_error_for(&left.bounds))
        });

        let mut simplified_first_occurrences = HashMap::new();
        let mut kept: Vec<TilePart<'a>> = Vec::new();
        let mut kept_triangles = 0;
        let mut geometric_error: f64 = 0.0;
        for part in largest_first {
            if !kept.is_empty() && kept_triangles >= triangle_budget {
                geometric_error = geometric_error.max(geometric_error_for(&part.bounds));
                break;
            }
            let (mut indices, simplification_error) =
                self.simplified_indices(part, ratio, &mut simplified_first_occurrences);
            if indices.is_empty() {
                indices = part.indices.to_vec();
            } else if indices.len() < part.indices.len() {
                geometric_error = geometric_error.max(simplification_error);
            }
            kept_triangles += indices.len() / 3;
            kept.push(TilePart {
                whole_element: part.whole_element,
                indices: Cow::Owned(indices),
                bounds: part.bounds,
            });
        }
        ParentContent {
            parts: kept,
            geometric_error,
        }
    }

    fn simplified_indices(
        &self,
        part: &TilePart<'a>,
        ratio: f32,
        simplified_first_occurrences: &mut HashMap<usize, Vec<u32>>,
    ) -> (Vec<u32>, f64) {
        let elements = &self.group.elements;
        let repeated = part.whole_element.and_then(|element_index| {
            let element = &elements[element_index];
            element
                .repeated_geometry
                .map(|repeated| (element, repeated))
        });
        // simplify_mesh's error limit is a fraction of the side it gets, and the group's side collapses small elements to nothing
        let Some((element, repeated)) = repeated else {
            let side = part.bounds.max_extent() as f32;
            return (
                simplify_mesh(&self.simplification_positions, &part.indices, ratio, side),
                f64::from(SIMPLIFICATION_ERROR_FRACTION * side),
            );
        };

        let first = &elements[repeated.first_occurrence];
        let side = first.bounds.max_extent() as f32;
        let first_indices = simplified_first_occurrences
            .entry(repeated.first_occurrence)
            .or_insert_with(|| {
                simplify_mesh(
                    &self.simplification_positions,
                    &self.group.indices[first.indices.clone()],
                    ratio,
                    side,
                )
            });
        let indices = first_indices
            .iter()
            .map(|index| index - first.vertices.start + element.vertices.start)
            .collect();
        (indices, f64::from(SIMPLIFICATION_ERROR_FRACTION * side))
    }

    fn leaf(
        &self,
        parts: &[TilePart<'a>],
        bounds: Aabb,
        path: &NodePath,
    ) -> io::Result<WrittenMeshTile> {
        Ok(WrittenMeshTile {
            bounds,
            geometric_error: 0.0,
            content_uri: self.write_content(parts, path)?,
            content_triangle_count: total_triangles(parts),
            children: Vec::new(),
            refine: Refine::Replace,
        })
    }

    fn write_content(&self, parts: &[TilePart<'a>], path: &NodePath) -> io::Result<Option<String>> {
        if total_triangles(parts) == 0 {
            return Ok(None);
        }
        let (direct_indices, instanced) =
            split_instanced_parts(self.group, parts, self.config.cad_edges);
        let mut mesh = make_glb_mesh(self.group, &direct_indices, &instanced, self.config);
        if self.config.texture_compression == TextureCompression::Ktx2 {
            mesh.texture = mesh.texture.as_ref().map(ktx2_texture).transpose()?;
        }
        let filename = path.to_filename();
        glb_writer::write_compressed_glb_file(
            &mesh,
            self.config.mesh_compression,
            &self.tiles_dir.join(&filename),
        )?;
        Ok(Some(format!("tiles/{filename}")))
    }
}

fn aabb_from_indices(positions: &[[f64; 3]], indices: &[u32]) -> Aabb {
    let mut aabb = Aabb::empty();
    for &i in indices {
        aabb.expand_point(positions[i as usize]);
    }
    aabb
}

fn values_at<T: Copy>(values: &[T], vertices: &[u32]) -> Vec<T> {
    vertices
        .iter()
        .map(|&vertex| values[vertex as usize])
        .collect()
}

fn tile_feature_id(tile_features: &[u32], group_feature_id: u32) -> u32 {
    tile_features
        .binary_search(&group_feature_id)
        .expect("every feature id is in the tile's feature list") as u32
}

fn tile_feature_table(
    group: &MaterialGroup,
    used_vertices: &[u32],
    instanced: &[InstancedElements<'_>],
) -> Option<(Vec<u32>, TileMetadata)> {
    if group.feature_ids.is_empty() {
        return None;
    }
    let mut tile_features = values_at(&group.feature_ids, used_vertices);
    for instanced_elements in instanced {
        tile_features.extend(
            instanced_elements
                .elements
                .iter()
                .map(|&element| group.feature_ids[group.elements[element].vertices.start as usize]),
        );
    }
    tile_features.sort_unstable();
    tile_features.dedup();

    let properties: Vec<MetadataProperty> = FEATURE_COLUMN_NAMES
        .iter()
        .enumerate()
        .filter_map(|(column, name)| {
            let values: Vec<String> = tile_features
                .iter()
                .map(|&feature| group.feature_rows[feature as usize][column].clone())
                .collect();
            // gltf-validator refuses a bufferView of zero bytes, which an all-empty string column needs
            let has_a_value = values.iter().any(|value| !value.is_empty());
            has_a_value.then(|| MetadataProperty {
                name: name.to_string(),
                values: MetadataValues::String(values),
            })
        })
        .collect();
    if properties.is_empty() {
        return None;
    }

    Some((
        tile_features,
        TileMetadata {
            class_name: ASSET_CLASS_NAME.to_string(),
            properties,
        },
    ))
}

fn instanced_mesh(
    group: &MaterialGroup,
    instanced: &InstancedElements<'_>,
    tile_features: Option<&[u32]>,
    config: &MeshTilingConfig,
) -> InstancedMesh {
    let first = &group.elements[instanced.first_occurrence];
    let first_listed_start = group.elements[instanced.elements[0]].vertices.start;
    let relative_indices: Vec<u32> = instanced
        .first_element_indices
        .iter()
        .map(|index| index - first_listed_start)
        .collect();
    let mut used_vertices = relative_indices.clone();
    used_vertices.sort_unstable();
    used_vertices.dedup();
    let indices: Vec<u32> = relative_indices
        .iter()
        .map(|index| {
            used_vertices
                .binary_search(index)
                .expect("every index is in the used vertex list") as u32
        })
        .collect();
    let first_vertices: Vec<u32> = used_vertices
        .iter()
        .map(|vertex| vertex + first.vertices.start)
        .collect();

    let first_center = first.bounds.center();
    let mut positions: Vec<[f32; 3]> = first_vertices
        .iter()
        .map(|&vertex| offset_from(group.positions[vertex as usize], first_center))
        .collect();
    let mut normals =
        (!group.normals.is_empty()).then(|| values_at(&group.normals, &first_vertices));

    let placements: Vec<RepeatedGeometry> = instanced
        .elements
        .iter()
        .map(|&element| {
            group.elements[element]
                .repeated_geometry
                .expect("every instanced element has a placement")
        })
        .collect();
    let translations: Vec<[f64; 3]> = placements
        .iter()
        .map(|placement| placement.instance_translation)
        .collect();
    let mut node_translation = compute_aabb(&translations).center();
    let mut instance_translations: Vec<[f32; 3]> = translations
        .iter()
        .map(|&translation| offset_from(translation, node_translation))
        .collect();
    let mut instance_rotations = placements
        .iter()
        .any(|placement| placement.instance_rotation != IDENTITY_ROTATION)
        .then(|| -> Vec<[f32; 4]> {
            placements
                .iter()
                .map(|placement| placement.instance_rotation)
                .collect()
        });

    if config.content_y_up {
        z_up_to_y_up(&mut positions);
        z_up_to_y_up(std::slice::from_mut(&mut node_translation));
        z_up_to_y_up(&mut instance_translations);
        if let Some(normals) = &mut normals {
            z_up_to_y_up(normals);
        }
        if let Some(rotations) = &mut instance_rotations {
            // a quaternion's vector part turns with the frame and its scalar part does not
            for rotation in rotations {
                *rotation = [rotation[0], rotation[2], -rotation[1], rotation[3]];
            }
        }
    }

    let instance_feature_ids = tile_features.map(|tile_features| {
        instanced
            .elements
            .iter()
            .map(|&element| {
                let vertex = group.elements[element].vertices.start as usize;
                tile_feature_id(tile_features, group.feature_ids[vertex])
            })
            .collect()
    });

    InstancedMesh {
        positions,
        normals,
        indices,
        node_translation,
        instance_translations,
        instance_rotations,
        instance_feature_ids,
        colors: (!group.colors.is_empty()).then(|| values_at(&group.colors, &first_vertices)),
    }
}

fn direct_glb_mesh(
    group: &MaterialGroup,
    indices: &[u32],
    used_vertices: &[u32],
    tile_features: Option<&[u32]>,
    config: &MeshTilingConfig,
) -> GlbMesh {
    if indices.is_empty() {
        return GlbMesh {
            positions: Vec::new(),
            normals: None,
            indices: None,
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: None,
            texture: None,
            base_color_factor: group.base_color_factor,
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        };
    }

    let tile_indices: Vec<u32> = indices
        .iter()
        .map(|index| {
            used_vertices
                .binary_search(index)
                .expect("every index is in the used vertex list") as u32
        })
        .collect();

    let mut node_translation = aabb_from_indices(&group.positions, used_vertices).center();
    let mut positions: Vec<[f32; 3]> = used_vertices
        .iter()
        .map(|&vertex| offset_from(group.positions[vertex as usize], node_translation))
        .collect();
    let mut normals = (!group.normals.is_empty()).then(|| values_at(&group.normals, used_vertices));
    if config.content_y_up {
        z_up_to_y_up(&mut positions);
        z_up_to_y_up(std::slice::from_mut(&mut node_translation));
        if let Some(normals) = &mut normals {
            z_up_to_y_up(normals);
        }
    }

    let texcoords =
        (!group.texcoords.is_empty()).then(|| values_at(&group.texcoords, used_vertices));
    // a tile holding part of a textured mesh carries only the part of the
    // texture its triangles reach
    let split = group
        .texture
        .as_ref()
        .filter(|_| indices.len() < group.indices.len())
        .map(|texture| {
            let texcoords = texcoords
                .as_ref()
                .expect("group_by_material drops a texture without texcoords");
            split_texture(texture, texcoords, &tile_indices)
        });
    let (texture, texcoords) = match split {
        Some((texture, texcoords)) => (Some(texture), Some(texcoords)),
        None => (group.texture.clone(), texcoords),
    };

    let feature_ids = tile_features.map(|tile_features| {
        used_vertices
            .iter()
            .map(|&vertex| tile_feature_id(tile_features, group.feature_ids[vertex as usize]))
            .collect()
    });

    GlbMesh {
        positions,
        normals,
        indices: Some(tile_indices),
        colors: (!group.colors.is_empty()).then(|| values_at(&group.colors, used_vertices)),
        texcoords,
        metadata: None,
        feature_ids,
        texture,
        base_color_factor: group.base_color_factor,
        node_translation: Some(node_translation),
        instanced_meshes: Vec::new(),
        edge_visibility: None,
    }
}

const HIDDEN_EDGE: u8 = 0;
const SILHOUETTE_EDGE: u8 = 1;
const HARD_EDGE: u8 = 2;
const REPEATED_HARD_EDGE: u8 = 3;
const COPLANAR_MAX_FOLD_DEGREES: f64 = 1.0;
const SMOOTH_MAX_FOLD_DEGREES: f64 = 30.0;

fn face_normal(positions: &[[f32; 3]], corners: &[u32; 3]) -> Option<[f64; 3]> {
    let &[first, second, third] = corners;
    let position = |corner: u32| positions[corner as usize].map(f64::from);
    let normal = cross(
        subtract(position(second), position(first)),
        subtract(position(third), position(first)),
    );
    let length = dot(normal, normal).sqrt();
    (length > 0.0).then(|| scaled(normal, 1.0 / length))
}

fn edge_kind(adjacent_faces: &[usize], face_normals: &[Option<[f64; 3]>]) -> u8 {
    let &[first, second] = adjacent_faces else {
        return HARD_EDGE;
    };
    let normal = |face: usize| face_normals[face].expect("only faces with a normal border an edge");
    let cosine = dot(normal(first), normal(second));
    if cosine >= COPLANAR_MAX_FOLD_DEGREES.to_radians().cos() {
        HIDDEN_EDGE
    } else if cosine >= SMOOTH_MAX_FOLD_DEGREES.to_radians().cos() {
        SILHOUETTE_EDGE
    } else {
        HARD_EDGE
    }
}

fn packed_edge_values(values: &[u8]) -> Vec<u8> {
    let mut packed = vec![0; values.len().div_ceil(4)];
    for (edge, &value) in values.iter().enumerate() {
        packed[edge / 4] |= value << ((edge % 4) * 2);
    }
    packed
}

fn edge_visibility(mesh: &GlbMesh) -> Option<EdgeVisibility> {
    let indices = mesh.indices.as_deref()?;
    // f32 negative zero has other bits than positive zero
    let position_bits =
        |vertex: usize| mesh.positions[vertex].map(|component| (component + 0.0).to_bits());
    let feature = |vertex: usize| mesh.feature_ids.as_ref().map_or(0, |ids| ids[vertex]);
    let mut welded_ids: HashMap<([u32; 3], u32), u32> = HashMap::new();
    let welded_vertices: Vec<u32> = (0..mesh.positions.len())
        .map(|vertex| {
            let next_id = welded_ids.len() as u32;
            *welded_ids
                .entry((position_bits(vertex), feature(vertex)))
                .or_insert(next_id)
        })
        .collect();
    let welded_edge = |(start, end): (u32, u32)| {
        let start = welded_vertices[start as usize];
        let end = welded_vertices[end as usize];
        (start.min(end), start.max(end))
    };
    let corner_pairs = |corners: &[u32; 3]| {
        [
            (corners[0], corners[1]),
            (corners[1], corners[2]),
            (corners[2], corners[0]),
        ]
    };

    let triangles = indices.as_chunks::<3>().0;
    let face_normals: Vec<Option<[f64; 3]>> = triangles
        .iter()
        .map(|corners| face_normal(&mesh.positions, corners))
        .collect();
    let mut adjacent_faces: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (face, corners) in triangles.iter().enumerate() {
        if face_normals[face].is_none() {
            continue;
        }
        for pair in corner_pairs(corners) {
            adjacent_faces
                .entry(welded_edge(pair))
                .or_default()
                .push(face);
        }
    }

    let mut values = Vec::with_capacity(indices.len());
    let mut silhouette_normals = Vec::new();
    let mut encoded_edges = HashSet::new();
    for (face, corners) in triangles.iter().enumerate() {
        if face_normals[face].is_none() {
            values.extend([HIDDEN_EDGE; 3]);
            continue;
        }
        for pair in corner_pairs(corners) {
            let edge = welded_edge(pair);
            let faces = &adjacent_faces[&edge];
            let first_occurrence = encoded_edges.insert(edge);
            let value = match (edge_kind(faces, &face_normals), first_occurrence) {
                (HARD_EDGE, true) => HARD_EDGE,
                (HARD_EDGE, false) => REPEATED_HARD_EDGE,
                (SILHOUETTE_EDGE, true) => {
                    silhouette_normals.extend(faces.iter().map(|&adjacent| {
                        face_normals[adjacent]
                            .expect("only faces with a normal border an edge")
                            .map(|component| component as f32)
                    }));
                    SILHOUETTE_EDGE
                }
                _ => HIDDEN_EDGE,
            };
            values.push(value);
        }
    }

    let any_drawn_edge = values.iter().any(|&value| value != HIDDEN_EDGE);
    any_drawn_edge.then(|| EdgeVisibility {
        visibility: packed_edge_values(&values),
        silhouette_normals,
    })
}

fn make_glb_mesh(
    group: &MaterialGroup,
    indices: &[u32],
    instanced: &[InstancedElements<'_>],
    config: &MeshTilingConfig,
) -> GlbMesh {
    let mut used_vertices = indices.to_vec();
    used_vertices.sort_unstable();
    used_vertices.dedup();

    let (tile_features, metadata) = tile_feature_table(group, &used_vertices, instanced).unzip();
    let tile_features = tile_features.as_deref();

    let mut mesh = direct_glb_mesh(group, indices, &used_vertices, tile_features, config);
    mesh.metadata = metadata;
    mesh.instanced_meshes = instanced
        .iter()
        .map(|elements| instanced_mesh(group, elements, tile_features, config))
        .collect();
    if config.mesh_compression == MeshCompression::Meshopt {
        reorder_vertices_for_meshopt(&mut mesh);
    }
    if config.cad_edges {
        mesh.edge_visibility = edge_visibility(&mesh);
    }
    mesh
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

fn count_tileset_nodes(tile: &Tile) -> usize {
    1 + tile.children.iter().map(count_tileset_nodes).sum::<usize>()
}

fn tileset_depth(tile: &Tile, depth: u32) -> u32 {
    tile.children.iter().fold(depth, |deepest, child| {
        deepest.max(tileset_depth(child, depth + 1))
    })
}

fn geometric_error_for(bounds: &Aabb) -> f64 {
    let h = bounds.half_extents();
    (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt()
}

fn y_up_bounds_to_z_up_box(bounds: &Aabb) -> [f64; 12] {
    // 3d tiles runtimes map glTF (x, y, z) to (x, -z, y)
    let rotated = Aabb::new(
        [bounds.min[0], -bounds.max[2], bounds.min[1]],
        [bounds.max[0], -bounds.min[2], bounds.max[1]],
    );
    rotated.to_3dtiles_box()
}

fn generate_mesh_tileset(root: &WrittenMeshTile, config: &MeshTilingConfig) -> Tileset {
    let tileset_box: fn(&Aabb) -> [f64; 12] = if config.content_y_up {
        Aabb::to_3dtiles_box
    } else {
        y_up_bounds_to_z_up_box
    };
    let mut root_tile = generate_tile(root, tileset_box);
    root_tile.transform = config.root_transform;
    // the tileset's own error has to be above zero or a viewer never visits
    // the root, and a single-tile root is a leaf whose error is zero
    let geometric_error = geometric_error_for(&root.bounds);
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

fn generate_tile(node: &WrittenMeshTile, tileset_box: fn(&Aabb) -> [f64; 12]) -> Tile {
    Tile {
        bounding_volume: BoundingVolume::Box {
            r#box: tileset_box(&node.bounds),
        },
        geometric_error: node.geometric_error,
        content: node.content_uri.clone().map(|uri| TileContent { uri }),
        children: node
            .children
            .iter()
            .map(|child| generate_tile(child, tileset_box))
            .collect(),
        refine: Some(node.refine),
        transform: None,
    }
}

fn collect_stats(node: &WrittenMeshTile, depth: u32, stats: &mut MeshTilingStats) {
    stats.tile_count += 1;
    stats.max_depth_reached = stats.max_depth_reached.max(depth);
    if node.children.is_empty() || node.refine == Refine::Add {
        stats.total_triangles += node.content_triangle_count;
    }
    for child in &node.children {
        collect_stats(child, depth + 1, stats);
    }
}

fn lossless_texture(texture: &TextureData) -> TextureData {
    if texture.mime_type == "image/png" {
        return texture.clone();
    }
    let image =
        image::load_from_memory(&texture.image_data).expect("texture image should be decodable");
    let mut buf = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut buf, image::ImageFormat::Png)
        .expect("texture encode");
    TextureData {
        image_data: buf.into_inner(),
        mime_type: "image/png".to_string(),
        width: texture.width,
        height: texture.height,
    }
}

fn ktx2_texture(texture: &TextureData) -> io::Result<TextureData> {
    let image = image::load_from_memory(&texture.image_data).map_err(io::Error::other)?;
    // KHR_texture_basisu images must have a width and height that are multiples of 4
    let width = image.width().next_multiple_of(4);
    let height = image.height().next_multiple_of(4);
    let pixels = image
        .resize_exact(width, height, image::imageops::FilterType::Triangle)
        .into_rgba8();

    basisu_encoder_init();
    let mut encoder = BasisuEncoder::new();
    encoder
        .set_image(SourceImage {
            data: pixels.as_raw(),
            format: SourceImageFormat::Rgba8,
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        })
        .map_err(io::Error::other)?;
    // KHR_texture_basisu images under a mipmap sampler should carry a full mip pyramid
    let params = BasisuEncoderParams::new_with_srgb_defaults(BasisTextureFormat::Etc1s)
        .with_flags(BU_COMP_FLAGS_GEN_MIPS_CLAMP);
    let image_data = encoder.compress(params).map_err(io::Error::other)?;

    Ok(TextureData {
        image_data,
        mime_type: glb_writer::KTX2_MIME_TYPE.to_string(),
        width,
        height,
    })
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

    let format = image::ImageFormat::from_mime_type(&original.mime_type)
        .or_else(|| image::guess_format(&original.image_data).ok())
        .unwrap_or(image::ImageFormat::Png);
    let mut buf = std::io::Cursor::new(Vec::new());
    cropped.write_to(&mut buf, format).expect("texture encode");

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
    use std::cell::Cell;

    thread_local! {
        static RESIDENT_MESHES: Cell<i64> = const { Cell::new(0) };
        static RESIDENT_MESH_PEAK: Cell<i64> = const { Cell::new(0) };
    }

    pub(super) fn note_resident_meshes(delta: i64) {
        RESIDENT_MESHES.with(|resident| {
            let updated = resident.get() + delta;
            resident.set(updated);
            RESIDENT_MESH_PEAK.with(|peak| {
                if updated > peak.get() {
                    peak.set(updated);
                }
            });
        });
    }

    pub(super) fn reset_resident_meshes() {
        RESIDENT_MESHES.with(|resident| resident.set(0));
        RESIDENT_MESH_PEAK.with(|peak| peak.set(0));
    }

    pub(super) fn resident_mesh_peak() -> i64 {
        RESIDENT_MESH_PEAK.with(Cell::get)
    }

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
            class: None,
            storey: None,
            element_color: None,
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
                positions.push([x as f64, y as f64, 0.0]);
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
            class: None,
            storey: None,
            element_color: None,
            base_color_factor: None,
            texture: None,
        }
    }

    struct WrittenTileset {
        dir: std::path::PathBuf,
        tileset: Tileset,
    }

    impl Drop for WrittenTileset {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl WrittenTileset {
        fn glb(&self, tile: &Tile) -> (serde_json::Value, Vec<Vec<u8>>) {
            let content = tile.content.as_ref().expect("the tile has content");
            read_glb(&self.dir.join(&content.uri))
        }
    }

    fn write_tileset(meshes: &[MeshData], config: &MeshTilingConfig) -> WrittenTileset {
        let dir = std::env::temp_dir().join(format!("mesh_tiler_{}", uuid::Uuid::new_v4()));
        tile_meshes(meshes, &dir, config).expect("tile_meshes failed");
        let tileset =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .expect("a parseable tileset.json");
        WrittenTileset { dir, tileset }
    }

    fn content_tiles(tile: &Tile) -> Vec<&Tile> {
        let own = tile.content.is_some().then_some(tile);
        own.into_iter()
            .chain(tile.children.iter().flat_map(content_tiles))
            .collect()
    }

    fn leaf_tiles(tile: &Tile) -> Vec<&Tile> {
        if tile.children.is_empty() {
            return vec![tile];
        }
        tile.children.iter().flat_map(leaf_tiles).collect()
    }

    fn accessor_values<T: bytemuck::Pod>(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        accessor: u64,
    ) -> Vec<T> {
        let view = json["accessors"][accessor as usize]["bufferView"]
            .as_u64()
            .expect("an accessor with a buffer view") as usize;
        bytemuck::pod_collect_to_vec(views[view].as_slice())
    }

    fn attribute_values<T: bytemuck::Pod>(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        attribute: &str,
    ) -> Option<Vec<T>> {
        let accessor = json["meshes"][0]["primitives"][0]["attributes"][attribute].as_u64()?;
        Some(accessor_values(json, views, accessor))
    }

    fn index_values(json: &serde_json::Value, views: &[Vec<u8>]) -> Vec<u32> {
        let accessor = json["meshes"][0]["primitives"][0]["indices"]
            .as_u64()
            .expect("an indices accessor");
        accessor_values(json, views, accessor)
    }

    #[test]
    fn small_mesh_produces_leaf() {
        let written = write_tileset(&[cube_mesh()], &MeshTilingConfig::default());
        let root = &written.tileset.root;
        assert!(
            root.children.is_empty(),
            "cube with 12 triangles should be a leaf"
        );
        assert_eq!(root.content.as_ref().unwrap().uri, "tiles/root.glb");
    }

    #[test]
    fn a_mesh_list_over_a_tiny_budget_writes_child_tilesets_without_holding_every_mesh() {
        let charge = |_: &MeshData| 10u64;
        let config = MeshTilingConfig::default();
        let shifted_cube = |index: usize| {
            let mut mesh = cube_mesh();
            let shift = index as f64 * 10.0;
            for position in &mut mesh.positions {
                position[0] += shift;
            }
            mesh.name = format!("cube-{index}");
            mesh
        };
        let directory = |label: &str| {
            std::env::temp_dir().join(format!("mesh_budget_{label}_{}", uuid::Uuid::new_v4()))
        };
        let remove_directory = |path: &std::path::Path| {
            let _ = std::fs::remove_dir_all(path);
        };

        let under_budget = directory("under");
        let direct = directory("direct");
        let meshes = vec![shifted_cube(0), shifted_cube(1)];
        tile_meshes_within_budget(meshes.clone(), &under_budget, &config, 25, charge)
            .expect("a list under the budget should tile");
        tile_meshes(&meshes, &direct, &config).expect("tile_meshes failed");
        let under_json = std::fs::read_to_string(under_budget.join("tileset.json"))
            .expect("the under-budget tileset");
        let direct_json =
            std::fs::read_to_string(direct.join("tileset.json")).expect("the direct tileset");
        assert_eq!(under_json, direct_json);
        assert!(!under_budget.join(MESH_BATCH_DIRECTORY).exists());
        remove_directory(&under_budget);
        remove_directory(&direct);

        reset_resident_meshes();
        let over_budget = directory("over");
        tile_meshes_within_budget((0..3).map(shifted_cube), &over_budget, &config, 10, charge)
            .expect("a list over the budget should tile");
        let peak = resident_mesh_peak();
        assert!(
            peak < 3,
            "the three meshes were resident together, peak {peak}"
        );
        let tileset: Tileset = serde_json::from_str(
            &std::fs::read_to_string(over_budget.join("tileset.json")).expect("the parent tileset"),
        )
        .expect("a parseable parent tileset");
        assert!(tileset.root.children.len() > 1);
        for child in &tileset.root.children {
            let uri = &child.content.as_ref().expect("a child tileset link").uri;
            assert!(uri.ends_with("tileset.json"), "{uri}");
            let child_path = over_budget.join(uri);
            assert!(child_path.is_file(), "{}", child_path.display());
            let child_tileset: Tileset = serde_json::from_str(
                &std::fs::read_to_string(&child_path).expect("a child tileset"),
            )
            .expect("a parseable child tileset");
            assert!(matches!(
                child_tileset.root.bounding_volume,
                BoundingVolume::Box { .. }
            ));
            assert!(matches!(child.bounding_volume, BoundingVolume::Box { .. }));
        }
        remove_directory(&over_budget);
    }

    #[test]
    fn an_adopted_tileset_stays_a_child_and_keeps_the_parent_transform() {
        let directory = std::env::temp_dir().join(format!("adopt_{}", uuid::Uuid::new_v4()));
        let child_dir = directory.join("piece");
        let root_transform = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 10.0, 20.0, 30.0, 1.0,
        ];
        let config = MeshTilingConfig {
            root_transform: Some(root_transform),
            ..MeshTilingConfig::default()
        };
        tile_meshes(&[cube_mesh()], &child_dir, &MeshTilingConfig::default())
            .expect("the child tileset");
        let mut parent = MeshBatchTiles::new(&directory, config);
        parent
            .adopt_tileset(
                &child_dir.join("tileset.json"),
                "piece/tileset.json".to_string(),
            )
            .expect("adopt the child");
        let stats = parent.finish().expect("the parent tileset");
        assert!(stats.tile_count >= 2);
        let tileset: Tileset = serde_json::from_str(
            &std::fs::read_to_string(directory.join("tileset.json")).expect("tileset.json"),
        )
        .expect("a parseable parent tileset");
        assert_eq!(tileset.root.transform, Some(root_transform));
        assert_eq!(tileset.root.children.len(), 1);
        assert!(tileset.root.children[0].transform.is_none());
        assert_eq!(
            tileset.root.children[0]
                .content
                .as_ref()
                .expect("a child link")
                .uri,
            "piece/tileset.json"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn large_mesh_subdivides() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        // 256×256 grid = 131072 triangles — well above the 100 limit.
        let written = write_tileset(&[grid_mesh(256)], &config);
        assert!(
            !written.tileset.root.children.is_empty(),
            "large mesh should be subdivided"
        );
    }

    #[test]
    fn write_tileset_creates_files() {
        let dir = std::env::temp_dir().join("tiletopia_mesh_tiler_test");
        let _ = std::fs::remove_dir_all(&dir);

        let config = MeshTilingConfig::default();
        tile_meshes(&[cube_mesh()], &dir, &config).expect("tile_meshes failed");

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
        let written = write_tileset(&[mesh], &config);
        let root = &written.tileset.root;
        assert!(
            !root.children.is_empty(),
            "expected a split of the 8192-triangle mesh with max 100"
        );

        let (json, views) = written.glb(root);
        let lod_tris = index_values(&json, &views).len() / 3;
        // LOD should have fewer triangles than the original 8192.
        assert!(
            lod_tris < 8192,
            "LOD mesh should have fewer triangles, got {lod_tris}"
        );
    }

    fn errors_never_grow_toward_the_leaves(tile: &Tile) {
        for child in &tile.children {
            assert!(
                child.geometric_error <= tile.geometric_error,
                "child error {} above parent error {}",
                child.geometric_error,
                tile.geometric_error
            );
            errors_never_grow_toward_the_leaves(child);
        }
    }

    #[test]
    fn a_parent_error_is_the_largest_element_it_leaves_out() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let cubes: Vec<MeshData> = (0..50)
            .map(|index| {
                let mut cube = cube_mesh();
                for position in &mut cube.positions {
                    position[0] += 2.0 * index as f64;
                }
                cube
            })
            .collect();

        let written = write_tileset(&cubes, &config);

        let unit_cube_half_diagonal = 0.75_f64.sqrt();
        let root = &written.tileset.root;
        assert!(!root.children.is_empty());
        assert!(
            (root.geometric_error - unit_cube_half_diagonal).abs() < 1e-2,
            "root error {}",
            root.geometric_error
        );
        errors_never_grow_toward_the_leaves(root);
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

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        assert!(json["geometricError"].as_f64().unwrap() > 0.0);
        assert_eq!(json["root"]["geometricError"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn each_leaf_glb_carries_only_the_vertices_its_triangles_reference() {
        let mesh = grid_mesh(32);
        let input_vertex_count = mesh.positions.len();
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let written = write_tileset(std::slice::from_ref(&mesh), &config);
        let leaves = leaf_tiles(&written.tileset.root);
        assert!(
            leaves.len() > 4,
            "the grid should split into several leaves"
        );

        let mut written_vertex_count = 0;
        for leaf in leaves {
            let (json, views) = written.glb(leaf);
            let position_accessor = json["meshes"][0]["primitives"][0]["attributes"]["POSITION"]
                .as_u64()
                .unwrap() as usize;
            let position_count = json["accessors"][position_accessor]["count"]
                .as_u64()
                .unwrap() as usize;
            let mut referenced = index_values(&json, &views);
            referenced.sort_unstable();
            referenced.dedup();
            assert_eq!(position_count, referenced.len(), "{:?}", leaf.content);
            written_vertex_count += position_count;
        }
        assert!(
            written_vertex_count <= 2 * input_vertex_count,
            "{written_vertex_count} vertices written for {input_vertex_count} input vertices"
        );
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
            class: None,
            storey: None,
            element_color: None,
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
    fn gltf_y_up_content_gets_its_bounding_volume_turned_to_z_up() {
        // 4 wide in x, 2 tall in glTF y, 6 deep in glTF z
        let mesh = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [4.0, 2.0, 6.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3],
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: "slab".into(),
            asset_id: None,
            class: None,
            storey: None,
            element_color: None,
            base_color_factor: None,
            texture: None,
        };
        let written = write_tileset(&[mesh], &MeshTilingConfig::default());

        let BoundingVolume::Box { r#box } = written.tileset.root.bounding_volume.clone() else {
            panic!("mesh tiles get a box");
        };
        #[rustfmt::skip]
        let expected = [
            2.0, -3.0, 1.0,
            2.0, 0.0, 0.0,
            0.0, 3.0, 0.0,
            0.0, 0.0, 1.0,
        ];
        assert_eq!(r#box, expected);
    }

    /// One triangle per asset, far enough apart that the split puts them in
    /// different leaves.
    fn triangle_at(x: f64, asset_id: Option<&str>) -> MeshData {
        MeshData {
            positions: vec![[x, 0.0, 0.0], [x + 1.0, 0.0, 0.0], [x, 1.0, 0.0]],
            normals: Vec::new(),
            texcoords: Vec::new(),
            indices: vec![0, 1, 2],
            name: "triangle".into(),
            asset_id: asset_id.map(str::to_string),
            class: None,
            storey: None,
            element_color: None,
            base_color_factor: None,
            texture: None,
        }
    }

    fn referenced_feature_ids(json: &serde_json::Value, views: &[Vec<u8>]) -> Vec<u32> {
        let feature_ids: Vec<f32> =
            attribute_values(json, views, "_FEATURE_ID_0").expect("feature ids on the tile");
        let mut reached: Vec<u32> = index_values(json, views)
            .iter()
            .map(|index| feature_ids[*index as usize] as u32)
            .collect();
        reached.sort_unstable();
        reached.dedup();
        reached
    }

    fn asset_ids(json: &serde_json::Value, views: &[Vec<u8>]) -> Vec<String> {
        let table = &json["extensions"]["EXT_structural_metadata"]["propertyTables"][0];
        assert_eq!(table["class"], "asset");
        let property = &table["properties"]["asset_id"];
        let values = &views[property["values"].as_u64().expect("asset_id values") as usize];
        let offsets_view = property["stringOffsets"]
            .as_u64()
            .expect("asset_id string offsets") as usize;
        let offsets: Vec<u32> = bytemuck::pod_collect_to_vec(views[offsets_view].as_slice());
        offsets
            .windows(2)
            .map(|pair| {
                String::from_utf8(values[pair[0] as usize..pair[1] as usize].to_vec()).unwrap()
            })
            .collect()
    }

    #[test]
    fn a_split_keeps_every_vertex_with_its_own_asset() {
        let meshes = [
            triangle_at(0.0, Some("asset-a")),
            triangle_at(10.0, Some("asset-b")),
        ];
        let config = MeshTilingConfig {
            max_triangles_per_tile: 1,
            ..Default::default()
        };

        let written = write_tileset(&meshes, &config);
        let tiles: Vec<_> = content_tiles(&written.tileset.root)
            .into_iter()
            .map(|tile| written.glb(tile))
            .collect();
        assert_eq!(tiles.len(), 2, "the two triangles should split apart");

        let reached: Vec<Vec<u32>> = tiles
            .iter()
            .map(|(json, views)| referenced_feature_ids(json, views))
            .collect();
        assert_eq!(reached, vec![vec![0], vec![0]]);

        let mut tables: Vec<Vec<String>> = tiles
            .iter()
            .map(|(json, views)| asset_ids(json, views))
            .collect();
        tables.sort_unstable();
        assert_eq!(tables, [["asset-a"], ["asset-b"]]);
    }

    #[test]
    fn a_mesh_without_an_asset_id_gets_an_empty_row_beside_one_that_has_it() {
        let meshes = [triangle_at(0.0, Some("asset-a")), triangle_at(10.0, None)];

        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);

        assert_eq!(asset_ids(&json, &views), ["asset-a", ""]);
        assert_eq!(referenced_feature_ids(&json, &views), vec![0, 1]);
    }

    #[test]
    fn a_tile_holding_only_a_mesh_without_values_carries_no_feature_table() {
        let meshes = [triangle_at(0.0, Some("asset-a")), triangle_at(10.0, None)];
        let config = MeshTilingConfig {
            max_triangles_per_tile: 1,
            ..Default::default()
        };

        let written = write_tileset(&meshes, &config);
        let tile_of_the_bare_triangle = content_tiles(&written.tileset.root)
            .into_iter()
            .find(|tile| {
                let BoundingVolume::Box { r#box } = &tile.bounding_volume else {
                    panic!("mesh tiles have box bounding volumes");
                };
                r#box[0] > 5.0
            })
            .expect("a tile around the triangle at x = 10");
        let (json, views) = written.glb(tile_of_the_bare_triangle);

        assert!(attribute_values::<f32>(&json, &views, "_FEATURE_ID_0").is_none());
        assert!(json["extensions"].get("EXT_structural_metadata").is_none());
        assert!(
            json["meshes"][0]["primitives"][0]
                .get("extensions")
                .is_none()
        );
    }

    #[test]
    fn tiles_without_any_asset_id_carry_no_feature_ids() {
        let written = write_tileset(&[cube_mesh()], &MeshTilingConfig::default());

        for leaf in leaf_tiles(&written.tileset.root) {
            let (json, views) = written.glb(leaf);
            assert!(attribute_values::<f32>(&json, &views, "_FEATURE_ID_0").is_none());
            assert!(json["extensions"].get("EXT_structural_metadata").is_none());
        }
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
        tile_meshes(&[cube_mesh()], &dir, &config).expect("tile_meshes failed");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tileset.json")).unwrap())
                .unwrap();
        let written = json["root"]["transform"].as_array().expect("a transform");
        assert_eq!(written.len(), 16);
        assert_eq!(written[12].as_f64().unwrap(), 100.0);
        assert_eq!(written[14].as_f64().unwrap(), 300.0);

        let plain = MeshTilingConfig::default();
        tile_meshes(&[cube_mesh()], &dir, &plain).expect("tile_meshes failed");
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

    #[test]
    fn a_crop_keeps_the_source_format() {
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([10, 20, 30]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        let tex = glb_writer::TextureData {
            image_data: buf.into_inner(),
            mime_type: "image/jpeg".to_string(),
            width: 16,
            height: 16,
        };
        let texcoords = vec![[0.0, 0.0], [0.5, 0.0], [0.0, 0.5]];
        let (cropped, _) = split_texture(&tex, &texcoords, &[0, 1, 2]);
        assert_eq!(cropped.mime_type, tex.mime_type);
        assert_eq!(
            image::guess_format(&cropped.image_data).unwrap(),
            image::ImageFormat::Jpeg
        );
    }

    /// A grid mesh with UVs spanning the whole texture.
    fn textured_grid_mesh(n: usize, texture: glb_writer::TextureData) -> MeshData {
        let mut mesh = grid_mesh(n);
        mesh.texcoords = mesh
            .positions
            .iter()
            .map(|p| [(p[0] / n as f64) as f32, (p[1] / n as f64) as f32])
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
        let written = write_tileset(&[mesh], &config);

        let sizes: Vec<(u32, u32)> = leaf_tiles(&written.tileset.root)
            .into_iter()
            .map(|leaf| {
                let (json, views) = written.glb(leaf);
                let image = image::load_from_memory(&texture_bytes(&json, &views)).unwrap();
                (image.width(), image.height())
            })
            .collect();
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
        let written = write_tileset(&[mesh], &config);

        let first_child = written
            .tileset
            .root
            .children
            .first()
            .expect("expected a split");
        let (json, views) = written.glb(first_child);
        image::load_from_memory(&texture_bytes(&json, &views)).expect("a decodable crop");

        let texcoords: Vec<[f32; 2]> =
            attribute_values(&json, &views, "TEXCOORD_0").expect("texcoords");
        let positions: Vec<[f32; 3]> = attribute_values(&json, &views, "POSITION").unwrap();
        assert_eq!(texcoords.len(), positions.len());
        let used: Vec<[f32; 2]> = index_values(&json, &views)
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
        let written = write_tileset(&[first, second], &MeshTilingConfig::default());
        let root = &written.tileset.root;
        assert!(
            root.children.is_empty(),
            "one material should give one subtree"
        );

        let (json, views) = written.glb(root);
        let positions: Vec<[f32; 3]> = attribute_values(&json, &views, "POSITION").unwrap();
        let texcoords: Vec<[f32; 2]> = attribute_values(&json, &views, "TEXCOORD_0").unwrap();
        assert_eq!(positions.len(), 18);
        assert_eq!(texcoords.len(), 18);
    }

    #[test]
    fn simplification_keeps_one_texcoord_per_vertex() {
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let mesh = textured_grid_mesh(32, make_test_texture(64, 64));
        let written = write_tileset(&[mesh], &config);
        let root = &written.tileset.root;
        assert!(!root.children.is_empty(), "expected a split");

        let (json, views) = written.glb(root);
        let texcoords: Vec<[f32; 2]> =
            attribute_values(&json, &views, "TEXCOORD_0").expect("texcoords");
        let positions: Vec<[f32; 3]> = attribute_values(&json, &views, "POSITION").unwrap();
        assert_eq!(texcoords.len(), positions.len());
        let highest = index_values(&json, &views).into_iter().max().unwrap();
        assert!((highest as usize) < texcoords.len());
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
    fn translucent_element_colors_tile_apart_from_opaque_ones_and_only_they_blend() {
        let brick = [180, 60, 40, 255];
        let glass = [150, 200, 255, 100];
        let mut wall = cube_mesh();
        wall.element_color = Some(brick);
        let mut pane = cube_mesh();
        pane.element_color = Some(glass);
        for position in &mut pane.positions {
            position[0] += 3.0;
        }

        let written = write_tileset(&[wall, pane], &MeshTilingConfig::default());
        let leaves = leaf_tiles(&written.tileset.root);
        assert_eq!(leaves.len(), 2);
        let mut seen = Vec::new();
        for leaf in leaves {
            let (json, views) = written.glb(leaf);
            let colors: Vec<[u8; 4]> = attribute_values(&json, &views, "COLOR_0").unwrap();
            assert!(colors.iter().all(|color| *color == colors[0]));
            let material = &json["materials"][0];
            assert_eq!(material["pbrMetallicRoughness"]["metallicFactor"], 0.0);
            let blends = material.get("alphaMode") == Some(&serde_json::json!("BLEND"));
            assert_eq!(blends, colors[0] == glass, "{:?}", colors[0]);
            seen.push(colors[0]);
        }
        seen.sort_unstable();
        let mut expected = vec![brick, glass];
        expected.sort_unstable();
        assert_eq!(seen, expected);
    }

    fn cube_with_a_vertex_per_face_corner(
        normal_of_face: impl Fn([f64; 3]) -> [f32; 3],
    ) -> MeshData {
        let mut cube = cube_mesh();
        let corners = std::mem::take(&mut cube.positions);
        let faces = std::mem::take(&mut cube.indices);
        cube.normals.clear();
        for quad in faces.chunks(6) {
            let [first, second, third] =
                [quad[0], quad[1], quad[2]].map(|corner| corners[corner as usize]);
            let face = cross(subtract(second, first), subtract(third, first));
            let base = cube.positions.len() as u32;
            for (offset, &corner) in quad.iter().enumerate() {
                cube.positions.push(corners[corner as usize]);
                cube.normals.push(normal_of_face(face));
                cube.indices.push(base + offset as u32);
            }
        }
        cube
    }

    #[test]
    fn flat_shaded_elements_weld_by_position_and_drop_their_normals() {
        let mut flat = cube_with_a_vertex_per_face_corner(|face| {
            let length = dot(face, face).sqrt();
            face.map(|component| (component / length) as f32)
        });
        flat.asset_id = Some("flat".into());
        let mut curved = moved_by(
            cube_with_a_vertex_per_face_corner(|_| [0.0, 0.6, 0.8]),
            [5.0, 0.0, 0.0],
        );
        curved.asset_id = Some("curved".into());
        let corners_per_cube = flat.positions.len();

        let written = write_tileset(&[flat, curved], &MeshTilingConfig::default());
        let leaves = leaf_tiles(&written.tileset.root);
        assert_eq!(leaves.len(), 2, "shading splits the two into groups");
        let mut seen = Vec::new();
        for leaf in leaves {
            let (json, views) = written.glb(leaf);
            let positions: Vec<[f32; 3]> = attribute_values(&json, &views, "POSITION").unwrap();
            let normals: Option<Vec<[f32; 3]>> = attribute_values(&json, &views, "NORMAL");
            assert_eq!(index_values(&json, &views).len(), 36);
            let rows = asset_ids(&json, &views);
            match rows[0].as_str() {
                "flat" => {
                    assert_eq!(positions.len(), 8);
                    assert!(normals.is_none());
                }
                "curved" => {
                    assert_eq!(positions.len(), corners_per_cube);
                    assert_eq!(normals.unwrap().len(), corners_per_cube);
                }
                other => panic!("no cube is named {other}"),
            }
            seen.push(rows[0].clone());
        }
        seen.sort_unstable();
        assert_eq!(seen, ["curved", "flat"]);
    }

    #[test]
    fn a_texture_without_uvs_is_dropped() {
        let mut mesh = cube_mesh();
        mesh.texture = Some(make_test_texture(4, 4));
        let written = write_tileset(&[mesh], &MeshTilingConfig::default());
        let root = &written.tileset.root;
        assert!(root.children.is_empty(), "expected a leaf");

        let (json, views) = written.glb(root);
        assert!(json.get("images").is_none());
        assert!(attribute_values::<[f32; 2]>(&json, &views, "TEXCOORD_0").is_none());
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
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        };

        let mut buf = Vec::new();
        glb_writer::write_glb(&mesh, &mut buf).unwrap();

        let glb = gltf::Glb::from_slice(&buf).expect("GLB with texture should parse");
        let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

        assert!(json["images"].is_array());
        assert!(json["materials"].is_array());
        assert_eq!(json["meshes"][0]["primitives"][0]["material"], 0);
    }

    fn ktx2_tile_textures(dir: &Path) -> Vec<Vec<u8>> {
        std::fs::read_dir(dir.join("tiles"))
            .unwrap()
            .map(|entry| {
                let (json, views) = read_glb(&entry.unwrap().path());
                assert_eq!(json["images"][0]["mimeType"], "image/ktx2");
                assert_eq!(
                    json["extensionsRequired"],
                    serde_json::json!(["KHR_texture_basisu"])
                );
                assert!(
                    json["extensionsUsed"]
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!("KHR_texture_basisu"))
                );
                let texture = &json["textures"][0];
                assert!(texture.get("source").is_none());
                let source = texture["extensions"]["KHR_texture_basisu"]["source"]
                    .as_u64()
                    .unwrap() as usize;
                let view = json["images"][source]["bufferView"].as_u64().unwrap() as usize;
                views[view].clone()
            })
            .collect()
    }

    #[test]
    fn ktx2_tiles_declare_khr_texture_basisu_and_hold_basislz_etc1s() {
        let dir = std::env::temp_dir().join(format!("ktx2_tiles_{}", uuid::Uuid::new_v4()));
        let mesh = textured_grid_mesh(16, make_test_texture(70, 50));
        let config = MeshTilingConfig {
            max_triangles_per_tile: 64,
            texture_compression: TextureCompression::Ktx2,
            ..Default::default()
        };
        tile_meshes(&[mesh], &dir, &config).expect("tile_meshes failed");

        let textures = ktx2_tile_textures(&dir);
        assert!(textures.len() > 1, "the mesh should have been split");
        let mut crops = 0;
        for texture in &textures {
            let reader = ktx2::Reader::new(texture).expect("a parseable KTX2");
            let header = reader.header();
            assert_eq!(
                header.supercompression_scheme,
                Some(ktx2::SupercompressionScheme::BasisLZ)
            );
            assert_eq!(header.format, None);
            assert_eq!(reader.color_model(), Some(ktx2::ColorModel::ETC1S));
            assert_eq!(
                reader.transfer_function(),
                Some(ktx2::TransferFunction::SRGB)
            );
            assert_eq!(reader.color_primaries(), Some(ktx2::ColorPrimaries::BT709));
            assert_eq!(header.pixel_width % 4, 0);
            assert_eq!(header.pixel_height % 4, 0);
            let largest_side = header.pixel_width.max(header.pixel_height);
            assert_eq!(header.level_count, largest_side.ilog2() + 1);
            if header.pixel_width < 72 || header.pixel_height < 52 {
                crops += 1;
            }
        }
        assert!(crops > 0, "split tiles should carry cropped textures");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ktx2_tiles_keep_a_table_of_only_their_own_elements() {
        let texture = make_test_texture(64, 64);
        let mut first = textured_grid_mesh(16, texture.clone());
        first.asset_id = Some("first".into());
        let mut second = textured_grid_mesh(16, texture);
        second.asset_id = Some("second".into());
        for position in &mut second.positions {
            position[0] += 100.0;
        }
        let config = MeshTilingConfig {
            max_triangles_per_tile: 512,
            texture_compression: TextureCompression::Ktx2,
            ..Default::default()
        };

        let written = write_tileset(&[first, second], &config);
        let tiles = content_tiles(&written.tileset.root);
        assert_eq!(tiles.len(), 2, "one tile per element");
        let mut tables = Vec::new();
        for tile in tiles {
            let (json, views) = written.glb(tile);
            assert_eq!(json["images"][0]["mimeType"], "image/ktx2");
            assert_eq!(referenced_feature_ids(&json, &views), vec![0]);
            tables.push(asset_ids(&json, &views));
        }
        tables.sort_unstable();
        assert_eq!(tables, [["first"], ["second"]]);
    }

    #[test]
    fn ktx2_scales_a_texture_up_to_multiples_of_four_and_keeps_its_pixels_upright() {
        let source = image::RgbaImage::from_fn(70, 50, |_, y| {
            let shade = if y < 25 { 20 } else { 230 };
            image::Rgba([shade, shade, shade, 255])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        source.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let texture = TextureData {
            image_data: png.into_inner(),
            mime_type: "image/png".to_string(),
            width: 70,
            height: 50,
        };

        let compressed = ktx2_texture(&texture).unwrap();
        assert_eq!((compressed.width, compressed.height), (72, 52));
        let header = ktx2::Reader::new(&compressed.image_data).unwrap().header();
        assert_eq!((header.pixel_width, header.pixel_height), (72, 52));

        basisu_c_sys::extra::basisu_transcoder_init();
        let transcoder = basisu_c_sys::extra::BasisuTranscoder::new(
            &compressed.image_data,
            basisu_c_sys::extra::SupportedTextureCompression::empty(),
            basisu_c_sys::extra::ChannelType::Auto,
        )
        .unwrap();
        let decoded = transcoder
            .transcode(Some(basisu_c_sys::TranscodeTargetFormat::RGBA32), None)
            .unwrap();
        let red_at = |row: usize| decoded.data[row * 72 * 4];
        assert!(red_at(0) < 60, "the dark half should stay on top");
        assert!(
            red_at(51) > 190,
            "the bright half should stay at the bottom"
        );
    }

    #[test]
    fn ktx2_keeps_a_jpeg_source_out_of_the_crops() {
        let mut jpeg = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(32, 32, image::Rgb([10, 20, 30]))
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .unwrap();
        let texture = TextureData {
            image_data: jpeg.into_inner(),
            mime_type: "image/jpeg".to_string(),
            width: 32,
            height: 32,
        };
        let config = MeshTilingConfig {
            texture_compression: TextureCompression::Ktx2,
            ..Default::default()
        };
        let groups = material_groups(&[textured_grid_mesh(16, texture)], &config);
        let group = &groups[0];
        let first_half = &group.indices[..group.indices.len() / 2];
        let crop = make_glb_mesh(group, first_half, &[], &config)
            .texture
            .expect("a cropped texture");
        assert_eq!(crop.mime_type, "image/png");
        assert_eq!(
            image::guess_format(&crop.image_data).unwrap(),
            image::ImageFormat::Png
        );
    }

    const DOOR_VERTEX_COUNT: usize = 81;
    const DOOR_TRIANGLE_COUNT: usize = 128;

    fn shaded_by_its_normals(mut mesh: MeshData) -> MeshData {
        mesh.normals = vec![[0.0, 0.6, 0.8]; mesh.positions.len()];
        mesh
    }

    fn door(asset_id: &str) -> MeshData {
        let mut door = grid_mesh(8);
        for position in &mut door.positions {
            *position = [
                position[0] * 0.25,
                position[1] * 0.125,
                0.05 * position[0] * position[1] / 64.0,
            ];
        }
        door.asset_id = Some(asset_id.to_string());
        door
    }

    fn moved_by(mut mesh: MeshData, offset: [f64; 3]) -> MeshData {
        for position in &mut mesh.positions {
            *position = std::array::from_fn(|axis| position[axis] + offset[axis]);
        }
        mesh
    }

    fn numbered_doors(count: usize) -> Vec<MeshData> {
        (0..count)
            .map(|number| {
                moved_by(
                    door(&format!("door-{number}")),
                    [number as f64 * 3.0, 0.0, 0.0],
                )
            })
            .collect()
    }

    fn instanced_nodes(json: &serde_json::Value) -> Vec<&serde_json::Value> {
        json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["extensions"]["EXT_mesh_gpu_instancing"].is_object())
            .collect()
    }

    fn instance_attribute<T: bytemuck::Pod>(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        node: &serde_json::Value,
        attribute: &str,
    ) -> Option<Vec<T>> {
        let attributes = &node["extensions"]["EXT_mesh_gpu_instancing"]["attributes"];
        let accessor = attributes[attribute].as_u64()?;
        Some(accessor_values(json, views, accessor))
    }

    fn node_mesh_attribute<T: bytemuck::Pod>(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        node: &serde_json::Value,
        attribute: &str,
    ) -> Vec<T> {
        let mesh = node["mesh"].as_u64().unwrap() as usize;
        let accessor = json["meshes"][mesh]["primitives"][0]["attributes"][attribute]
            .as_u64()
            .expect("the node's mesh has the attribute");
        accessor_values(json, views, accessor)
    }

    fn node_translation(node: &serde_json::Value) -> [f64; 3] {
        std::array::from_fn(|axis| node["translation"][axis].as_f64().unwrap())
    }

    fn turned_by_quaternion(rotation: [f32; 4], vector: [f32; 3]) -> [f64; 3] {
        let [x, y, z, w] = rotation.map(f64::from);
        let [first, second, third] = vector.map(f64::from);
        [
            (1.0 - 2.0 * (y * y + z * z)) * first
                + 2.0 * (x * y - z * w) * second
                + 2.0 * (x * z + y * w) * third,
            2.0 * (x * y + z * w) * first
                + (1.0 - 2.0 * (x * x + z * z)) * second
                + 2.0 * (y * z - x * w) * third,
            2.0 * (x * z - y * w) * first
                + 2.0 * (y * z + x * w) * second
                + (1.0 - 2.0 * (x * x + y * y)) * third,
        ]
    }

    fn assert_instances_land_on_their_sources(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        node: &serde_json::Value,
        sources: &[MeshData],
        content_y_up: bool,
    ) {
        let rows = asset_ids(json, views);
        let local_positions: Vec<[f32; 3]> = node_mesh_attribute(json, views, node, "POSITION");
        let translations: Vec<[f32; 3]> =
            instance_attribute(json, views, node, "TRANSLATION").expect("translations");
        let rotations: Option<Vec<[f32; 4]>> = instance_attribute(json, views, node, "ROTATION");
        let feature_ids: Vec<f32> =
            instance_attribute(json, views, node, "_FEATURE_ID_0").expect("feature ids");
        assert_eq!(translations.len(), feature_ids.len());
        let node_translation = node_translation(node);

        for (instance, feature_id) in feature_ids.iter().enumerate() {
            let row = &rows[*feature_id as usize];
            let source = sources
                .iter()
                .find(|mesh| mesh.asset_id.as_ref() == Some(row))
                .expect("a row naming a source mesh");
            assert_eq!(local_positions.len(), source.positions.len());
            for (local, expected) in local_positions.iter().zip(&source.positions) {
                let turned = match &rotations {
                    Some(rotations) => turned_by_quaternion(rotations[instance], *local),
                    None => local.map(f64::from),
                };
                let placed: [f64; 3] = std::array::from_fn(|axis| {
                    node_translation[axis] + f64::from(translations[instance][axis]) + turned[axis]
                });
                let expected = if content_y_up {
                    [expected[0], expected[2], -expected[1]]
                } else {
                    *expected
                };
                for axis in 0..3 {
                    assert!(
                        (placed[axis] - expected[axis]).abs() < 2e-3,
                        "{row}: {placed:?} should be at {expected:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_meshes_write_one_geometry_with_one_instance_and_feature_id_each() {
        let mut meshes = numbered_doors(6);
        meshes.push(shaded_by_its_normals(triangle_at(-20.0, Some("wall"))));

        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);

        assert_eq!(json["meshes"].as_array().unwrap().len(), 2);
        let nodes = instanced_nodes(&json);
        assert_eq!(nodes.len(), 1);
        let node = nodes[0];
        let positions: Vec<[f32; 3]> = node_mesh_attribute(&json, &views, node, "POSITION");
        assert_eq!(positions.len(), DOOR_VERTEX_COUNT);
        let translations: Vec<[f32; 3]> =
            instance_attribute(&json, &views, node, "TRANSLATION").unwrap();
        assert_eq!(translations.len(), 6);
        assert!(
            node["extensions"]["EXT_mesh_gpu_instancing"]["attributes"]
                .get("ROTATION")
                .is_none()
        );
        assert_instances_land_on_their_sources(&json, &views, node, &meshes, false);

        let rows = asset_ids(&json, &views);
        assert_eq!(rows.len(), 7);
        let feature_ids: Vec<f32> =
            instance_attribute(&json, &views, node, "_FEATURE_ID_0").unwrap();
        let mut instance_rows: Vec<&str> = feature_ids
            .iter()
            .map(|feature_id| rows[*feature_id as usize].as_str())
            .collect();
        instance_rows.sort_unstable();
        assert_eq!(
            instance_rows,
            ["door-0", "door-1", "door-2", "door-3", "door-4", "door-5"]
        );
        let wall_feature = referenced_feature_ids(&json, &views);
        assert_eq!(wall_feature.len(), 1);
        assert_eq!(rows[wall_feature[0] as usize], "wall");

        // 3d-tiles-validator reads these fields in ExtInstanceFeaturesValidator and FeatureIdAccessorValidator
        let feature_id = &node["extensions"]["EXT_instance_features"]["featureIds"][0];
        assert_eq!(feature_id["featureCount"], 7);
        assert_eq!(feature_id["attribute"], 0);
        assert_eq!(feature_id["propertyTable"], 0);
        let accessor = node["extensions"]["EXT_mesh_gpu_instancing"]["attributes"]["_FEATURE_ID_0"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(json["accessors"][accessor]["type"], "SCALAR");
        assert!(json["accessors"][accessor].get("normalized").is_none());
        let names = |field: &str| json[field].as_array().expect(field).clone();
        assert!(
            names("extensionsRequired").contains(&serde_json::json!("EXT_mesh_gpu_instancing"))
        );
        assert!(names("extensionsUsed").contains(&serde_json::json!("EXT_mesh_gpu_instancing")));
        assert!(names("extensionsUsed").contains(&serde_json::json!("EXT_instance_features")));
        assert!(!names("extensionsRequired").contains(&serde_json::json!("EXT_instance_features")));

        let bytes = std::fs::read(written.dir.join("tiles/root.glb")).unwrap();
        let glb = gltf::Glb::from_slice(&bytes).unwrap();
        assert_eq!(
            json["buffers"][0]["byteLength"].as_u64().unwrap() as usize,
            glb.bin.unwrap().len()
        );
    }

    #[test]
    fn a_tile_of_only_repeated_meshes_has_no_empty_direct_mesh() {
        let meshes = numbered_doors(6);
        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);

        assert_eq!(json["meshes"].as_array().unwrap().len(), 1);
        assert_eq!(json["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(json["scenes"][0]["nodes"], serde_json::json!([0]));
        assert_instances_land_on_their_sources(&json, &views, &json["nodes"][0], &meshes, false);
    }

    #[test]
    fn repeated_meshes_instance_per_element_color_and_carry_it() {
        let oak = [120, 80, 40, 255];
        let steel = [90, 90, 100, 255];
        let mut meshes = numbered_doors(12);
        for (number, mesh) in meshes.iter_mut().enumerate() {
            mesh.element_color = Some(if number < 6 { oak } else { steel });
        }

        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);

        assert!(json["materials"][0].get("alphaMode").is_none());
        let nodes = instanced_nodes(&json);
        assert_eq!(nodes.len(), 2);
        let mut node_colors: Vec<[u8; 4]> = nodes
            .iter()
            .map(|node| {
                let colors: Vec<[u8; 4]> = node_mesh_attribute(&json, &views, node, "COLOR_0");
                assert!(colors.iter().all(|color| *color == colors[0]));
                colors[0]
            })
            .collect();
        node_colors.sort_unstable();
        let mut expected = [oak, steel];
        expected.sort_unstable();
        assert_eq!(node_colors, expected);
        for node in nodes {
            assert_instances_land_on_their_sources(&json, &views, node, &meshes, false);
        }
    }

    #[test]
    fn rotated_copies_share_a_geometry_through_instance_rotations() {
        let meshes: Vec<MeshData> = (0..4)
            .map(|number| {
                let mut turned = door(&format!("door-{number}"));
                for position in &mut turned.positions {
                    let [x, y, z] = *position;
                    *position = match number {
                        0 => [x, y, z],
                        1 => [-y, x, z],
                        2 => [-x, -y, z],
                        _ => [y, -x, z],
                    };
                }
                moved_by(turned, [number as f64 * 5.0, 2.0, 1.0])
            })
            .collect();

        for content_y_up in [false, true] {
            let config = MeshTilingConfig {
                content_y_up,
                ..Default::default()
            };
            let written = write_tileset(&meshes, &config);
            let (json, views) = written.glb(&written.tileset.root);
            let nodes = instanced_nodes(&json);
            assert_eq!(nodes.len(), 1);
            let rotations: Vec<[f32; 4]> =
                instance_attribute(&json, &views, nodes[0], "ROTATION").expect("rotations");
            assert_eq!(rotations.len(), 4);
            for rotation in rotations {
                let length: f32 = rotation.iter().map(|value| value * value).sum();
                assert!((length.sqrt() - 1.0).abs() < 1e-5, "{rotation:?}");
            }
            assert_instances_land_on_their_sources(&json, &views, nodes[0], &meshes, content_y_up);
        }
    }

    #[test]
    fn mirrored_copies_and_copies_with_other_normals_get_their_own_geometry() {
        let mut meshes = Vec::new();
        for number in 0..4 {
            let offset = [number as f64 * 3.0, 0.0, 0.0];
            meshes.push(moved_by(door(&format!("plain-{number}")), offset));

            let mut mirrored = door(&format!("mirrored-{number}"));
            for position in &mut mirrored.positions {
                position[0] = -position[0];
            }
            meshes.push(moved_by(mirrored, [offset[0], 10.0, 0.0]));

            let mut facing_down = door(&format!("down-{number}"));
            facing_down.normals = vec![[0.0, 0.0, -1.0]; facing_down.positions.len()];
            meshes.push(moved_by(facing_down, [offset[0], 20.0, 0.0]));
        }

        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);
        let nodes = instanced_nodes(&json);
        assert_eq!(nodes.len(), 3);

        let rows = asset_ids(&json, &views);
        for node in nodes {
            let feature_ids: Vec<f32> =
                instance_attribute(&json, &views, node, "_FEATURE_ID_0").unwrap();
            assert_eq!(feature_ids.len(), 4);
            let names: Vec<&str> = feature_ids
                .iter()
                .map(|feature_id| rows[*feature_id as usize].split('-').next().unwrap())
                .collect();
            assert!(names.iter().all(|name| *name == names[0]), "{names:?}");

            let normals: Vec<[f32; 3]> = node_mesh_attribute(&json, &views, node, "NORMAL");
            assert_eq!(normals[0][2] < 0.0, names[0] == "down");
            assert_instances_land_on_their_sources(&json, &views, node, &meshes, false);
        }
    }

    fn small_cube_at(x: f64, y: f64, size: f64, asset_id: &str) -> MeshData {
        let mut cube = cube_mesh();
        for position in &mut cube.positions {
            *position = [
                position[0] * size + x,
                position[1] * size + y,
                position[2] * size,
            ];
        }
        cube.asset_id = Some(asset_id.to_string());
        cube
    }

    #[test]
    fn repeats_too_small_to_pay_for_a_draw_call_stay_in_the_merged_mesh() {
        let meshes = [
            small_cube_at(0.0, 0.0, 1.0, "first"),
            small_cube_at(5.0, 0.0, 1.0, "second"),
        ];
        let written = write_tileset(&meshes, &MeshTilingConfig::default());
        let (json, views) = written.glb(&written.tileset.root);

        assert!(instanced_nodes(&json).is_empty());
        assert!(json.get("extensionsRequired").is_none());
        assert_eq!(referenced_feature_ids(&json, &views), vec![0, 1]);
    }

    fn triangles_per_row(json: &serde_json::Value, views: &[Vec<u8>]) -> Vec<(String, usize)> {
        let feature_ids: Vec<f32> =
            attribute_values(json, views, "_FEATURE_ID_0").expect("feature ids");
        let rows = asset_ids(json, views);
        rows.iter()
            .enumerate()
            .map(|(row, name)| {
                let triangles = index_values(json, views)
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .filter(|triangle| feature_ids[triangle[0] as usize] as usize == row)
                    .count();
                (name.clone(), triangles)
            })
            .collect()
    }

    fn assert_every_vertex_is_inside_the_tile_box(
        json: &serde_json::Value,
        views: &[Vec<u8>],
        tile: &Tile,
    ) {
        let BoundingVolume::Box { r#box } = tile.bounding_volume.clone() else {
            panic!("mesh tiles get a box");
        };
        let translation = node_translation(&json["nodes"][0]);
        let locals: Vec<[f32; 3]> = attribute_values(json, views, "POSITION").unwrap();
        for local in locals {
            let gltf: [f64; 3] =
                std::array::from_fn(|axis| translation[axis] + f64::from(local[axis]));
            // 3d tiles runtimes map glTF (x, y, z) to (x, -z, y)
            let tile_frame = [gltf[0], -gltf[2], gltf[1]];
            for axis in 0..3 {
                let half_extent = r#box[3 + axis * 4];
                assert!(
                    (tile_frame[axis] - r#box[axis]).abs() <= half_extent + 1e-5,
                    "{tile_frame:?} is outside {:?}",
                    r#box
                );
            }
        }
    }

    fn assert_errors_shrink_toward_the_leaves(tile: &Tile) {
        for child in &tile.children {
            assert!(child.geometric_error <= tile.geometric_error);
            assert_errors_shrink_toward_the_leaves(child);
        }
    }

    #[test]
    fn additive_tiles_put_the_largest_elements_nearest_the_root() {
        let mut meshes: Vec<MeshData> = (0..14)
            .map(|number| small_cube_at(f64::from(number), 0.0, 1.0, &format!("cube-{number}")))
            .collect();
        meshes.push(small_cube_at(5.0, 20.0, 8.0, "slab"));
        let config = MeshTilingConfig {
            max_triangles_per_tile: 36,
            content_y_up: true,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);
        let root = &written.tileset.root;

        assert!(matches!(root.refine, Some(Refine::Add)));
        let (json, views) = written.glb(root);
        let root_rows: Vec<String> = triangles_per_row(&json, &views)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(root_rows.contains(&"slab".to_string()), "{root_rows:?}");
        let unit_cube_error = 3f64.sqrt() / 2.0;
        // element bounds carry a padding that grows with the distance from the origin
        assert!((root.geometric_error - unit_cube_error).abs() < 0.01);
        assert_errors_shrink_toward_the_leaves(root);
        for leaf in leaf_tiles(root) {
            assert_eq!(leaf.geometric_error, 0.0);
        }
    }

    #[test]
    fn elements_that_fit_stay_whole_in_one_tile_inside_its_box() {
        // the middle cube straddles the root's split plane at x = 7
        let meshes: Vec<MeshData> = (0..15)
            .map(|number| small_cube_at(f64::from(number), 0.0, 1.0, &format!("cube-{number}")))
            .collect();
        let config = MeshTilingConfig {
            max_triangles_per_tile: 36,
            content_y_up: true,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);

        let mut tiles_listing: BTreeMap<String, usize> = BTreeMap::new();
        for tile in content_tiles(&written.tileset.root) {
            let (json, views) = written.glb(tile);
            assert!(index_values(&json, &views).len() / 3 <= 36);
            assert_every_vertex_is_inside_the_tile_box(&json, &views, tile);
            for (name, triangles) in triangles_per_row(&json, &views) {
                assert_eq!(triangles, 12, "{name} should be whole");
                *tiles_listing.entry(name).or_default() += 1;
            }
        }
        assert_eq!(tiles_listing.len(), 15);
        assert!(
            tiles_listing.values().all(|count| *count == 1),
            "{tiles_listing:?}"
        );
    }

    #[test]
    fn an_element_over_the_tile_budget_splits_and_the_small_ones_beside_it_stay_whole() {
        let mut slab = grid_mesh(32);
        slab.asset_id = Some("slab".to_string());
        let mut meshes = vec![slab];
        for number in 0..5 {
            meshes.push(small_cube_at(
                f64::from(number) * 7.0,
                16.0,
                1.0,
                &format!("cube-{number}"),
            ));
        }
        let config = MeshTilingConfig {
            max_triangles_per_tile: 100,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);

        let mut tiles_listing: BTreeMap<String, usize> = BTreeMap::new();
        let mut triangles_by_name: BTreeMap<String, usize> = BTreeMap::new();
        for tile in content_tiles(&written.tileset.root) {
            let (json, views) = written.glb(tile);
            assert!(index_values(&json, &views).len() / 3 <= 100);
            for (name, triangles) in triangles_per_row(&json, &views) {
                *tiles_listing.entry(name.clone()).or_default() += 1;
                *triangles_by_name.entry(name).or_default() += triangles;
            }
        }
        assert!(tiles_listing["slab"] > 1, "{tiles_listing:?}");
        assert_eq!(triangles_by_name["slab"], 2 * 32 * 32);
        for number in 0..5 {
            let name = format!("cube-{number}");
            assert_eq!(tiles_listing[&name], 1, "{tiles_listing:?}");
            assert_eq!(triangles_by_name[&name], 12);
        }
    }

    #[test]
    fn a_parent_keeps_small_elements_whole_beside_a_large_simplified_one() {
        let mut terrain = grid_mesh(64);
        for position in &mut terrain.positions {
            position[2] = 2.0 * (position[0] * 0.8).sin() * (position[1] * 0.8).sin();
        }
        terrain.asset_id = None;
        let meshes = [
            terrain,
            small_cube_at(70.0, 0.0, 0.1, "first"),
            small_cube_at(70.0, 5.0, 0.1, "second"),
        ];
        let config = MeshTilingConfig {
            max_triangles_per_tile: 6000,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);
        assert!(!written.tileset.root.children.is_empty());

        let (json, views) = written.glb(&written.tileset.root);
        let triangles: BTreeMap<String, usize> =
            triangles_per_row(&json, &views).into_iter().collect();
        assert_eq!(triangles["first"], 12);
        assert_eq!(triangles["second"], 12);
        assert!(
            triangles[""] < 2 * 64 * 64,
            "the large element should be simplified: {triangles:?}"
        );
    }

    #[test]
    fn a_parent_holds_at_most_the_tile_budget_and_drops_the_smallest_elements_first() {
        let mut meshes = Vec::new();
        for number in 0..30 {
            meshes.push(small_cube_at(
                f64::from(number) * 4.0,
                0.0,
                1.0,
                &format!("large-{number}"),
            ));
            meshes.push(small_cube_at(
                f64::from(number) * 4.0,
                2.0,
                0.1,
                &format!("small-{number}"),
            ));
        }
        let config = MeshTilingConfig {
            max_triangles_per_tile: 240,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);

        let (json, views) = written.glb(&written.tileset.root);
        let rows = asset_ids(&json, &views);
        assert!(
            rows.iter().all(|name| name.starts_with("large-")),
            "{rows:?}"
        );
        // twenty 12 triangle cubes reach the 240 triangle budget
        assert_eq!(rows.len(), 20);
    }

    #[test]
    fn a_parent_draws_one_simplified_geometry_for_its_repeated_elements() {
        let mut meshes = numbered_doors(40);
        let oversize = shaded_by_its_normals(moved_by(grid_mesh(32), [0.0, -200.0, 0.0]));
        let oversize_triangles = oversize.indices.len() / 3;
        meshes.push(oversize);
        let config = MeshTilingConfig {
            max_triangles_per_tile: 2000,
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!("mesh_tiler_{}", uuid::Uuid::new_v4()));
        let stats = tile_meshes(&meshes, &dir, &config).expect("tile_meshes failed");
        assert_eq!(
            stats.total_triangles,
            40 * DOOR_TRIANGLE_COUNT + oversize_triangles
        );

        let (json, views) = read_glb(&dir.join("tiles/root.glb"));
        let _ = std::fs::remove_dir_all(&dir);
        let nodes = instanced_nodes(&json);
        assert_eq!(nodes.len(), 1);
        let positions: Vec<[f32; 3]> = node_mesh_attribute(&json, &views, nodes[0], "POSITION");
        assert!(
            positions.len() < DOOR_VERTEX_COUNT,
            "the geometry should be simplified, it has {} vertices",
            positions.len()
        );
        let feature_ids: Vec<f32> =
            instance_attribute(&json, &views, nodes[0], "_FEATURE_ID_0").unwrap();
        assert!(feature_ids.len() > 1);
        let rows = asset_ids(&json, &views);
        let mut named: Vec<&String> = feature_ids
            .iter()
            .map(|feature_id| &rows[*feature_id as usize])
            .collect();
        named.sort_unstable();
        named.dedup();
        assert_eq!(named.len(), feature_ids.len());
    }

    #[test]
    fn no_texture_compression_leaves_the_tile_texture_as_it_was() {
        let dir = std::env::temp_dir().join(format!("plain_tiles_{}", uuid::Uuid::new_v4()));
        let texture = make_test_texture(70, 50);
        let source_bytes = texture.image_data.clone();
        let mesh = textured_grid_mesh(2, texture);
        tile_meshes(&[mesh], &dir, &MeshTilingConfig::default()).expect("tile_meshes failed");

        let (json, views) = read_glb(&dir.join("tiles/root.glb"));
        assert_eq!(json["images"][0]["mimeType"], "image/png");
        assert!(json.get("extensionsRequired").is_none());
        assert!(json["textures"][0].get("extensions").is_none());
        assert_eq!(texture_bytes(&json, &views), source_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn packed_edge_values_match_the_extension_readme_examples() {
        // EXT_mesh_primitive_edge_visibility README, "Shared silhouette edge" and "Shared hard edge"
        assert_eq!(packed_edge_values(&[2, 0, 1, 0, 2, 0]), [18, 2]);
        assert_eq!(packed_edge_values(&[2, 0, 2, 3, 2, 0]), [226, 2]);
    }

    fn indexed_glb_mesh(
        positions: Vec<[f32; 3]>,
        indices: Vec<u32>,
        feature_ids: Vec<u32>,
    ) -> GlbMesh {
        GlbMesh {
            positions,
            normals: None,
            indices: Some(indices),
            colors: None,
            texcoords: None,
            metadata: None,
            feature_ids: Some(feature_ids),
            texture: None,
            base_color_factor: None,
            node_translation: None,
            instanced_meshes: Vec::new(),
            edge_visibility: None,
        }
    }

    #[test]
    fn a_shared_edge_between_two_elements_is_drawn_on_both_and_inside_one_is_hidden() {
        let corners = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        let indices: Vec<u32> = (0..6).collect();

        let one_element = indexed_glb_mesh(corners.clone(), indices.clone(), vec![0; 6]);
        let edges = edge_visibility(&one_element).expect("drawn edges");
        // [2, 2, 0, 0, 2, 2]: the diagonal both triangles share is hidden
        assert_eq!(edges.visibility, [10, 10]);
        assert!(edges.silhouette_normals.is_empty());

        let two_elements = indexed_glb_mesh(corners, indices, vec![0, 0, 0, 1, 1, 1]);
        let edges = edge_visibility(&two_elements).expect("drawn edges");
        // [2, 2, 2, 2, 2, 2]: each element outlines its own triangle
        assert_eq!(edges.visibility, [170, 10]);
    }

    #[test]
    fn a_shallow_fold_is_a_silhouette_carrying_both_face_normals() {
        let lift = 0.125;
        let mesh = indexed_glb_mesh(
            vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [1.0, 1.0, 0.0],
                [0.0, 1.0, lift],
            ],
            vec![0, 1, 2, 0, 2, 3],
            vec![0; 4],
        );
        let edges = edge_visibility(&mesh).expect("drawn edges");

        // [2, 2, 1, 0, 2, 2]: the fold of about 10 degrees is encoded once as a silhouette
        assert_eq!(edges.visibility, [26, 10]);
        let length = (1.0f32 + 2.0 * lift * lift).sqrt();
        let expected = [
            [0.0, 0.0, 1.0],
            [lift / length, -lift / length, 1.0 / length],
        ];
        assert_eq!(edges.silhouette_normals.len(), 2);
        for (normal, expected) in edges.silhouette_normals.iter().zip(expected) {
            for axis in 0..3 {
                assert!(
                    (normal[axis] - expected[axis]).abs() < 1e-6,
                    "{normal:?} should be {expected:?}"
                );
            }
        }
    }

    fn face_split_box(asset_id: &str) -> MeshData {
        let cube = cube_mesh();
        MeshData {
            positions: cube
                .indices
                .iter()
                .map(|&index| cube.positions[index as usize])
                .collect(),
            normals: Vec::new(),
            indices: (0..cube.indices.len() as u32).collect(),
            asset_id: Some(asset_id.to_string()),
            ..cube
        }
    }

    fn edge_visibility_extension(json: &serde_json::Value) -> &serde_json::Value {
        &json["meshes"][0]["primitives"][0]["extensions"]["EXT_mesh_primitive_edge_visibility"]
    }

    fn bin_slice<'a>(bin: &'a [u8], location: &serde_json::Value) -> &'a [u8] {
        let offset = location["byteOffset"].as_u64().unwrap_or(0) as usize;
        let length = location["byteLength"].as_u64().unwrap() as usize;
        &bin[offset..offset + length]
    }

    fn accessor_view<'a>(
        json: &'a serde_json::Value,
        accessor: &serde_json::Value,
    ) -> &'a serde_json::Value {
        let accessor = accessor.as_u64().expect("an accessor index") as usize;
        let view = json["accessors"][accessor]["bufferView"]
            .as_u64()
            .expect("an accessor with a buffer view");
        &json["bufferViews"][view as usize]
    }

    fn decoded_positions_and_indices(
        tile: &[u8],
        json: &serde_json::Value,
        bin: &[u8],
        compression: MeshCompression,
    ) -> (Vec<[f32; 3]>, Vec<u32>) {
        let primitive = &json["meshes"][0]["primitives"][0];
        match compression {
            MeshCompression::None => (
                bytemuck::pod_collect_to_vec(bin_slice(
                    bin,
                    accessor_view(json, &primitive["attributes"]["POSITION"]),
                )),
                bytemuck::pod_collect_to_vec(bin_slice(
                    bin,
                    accessor_view(json, &primitive["indices"]),
                )),
            ),
            MeshCompression::Meshopt => {
                let stream = |accessor: &serde_json::Value| {
                    &accessor_view(json, accessor)["extensions"]["EXT_meshopt_compression"]
                };
                let count = |stream: &serde_json::Value| stream["count"].as_u64().unwrap() as usize;
                let positions = stream(&primitive["attributes"]["POSITION"]);
                let indices = stream(&primitive["indices"]);
                assert_eq!(indices["mode"], "INDICES");
                let quantized: Vec<[u16; 4]> =
                    meshopt::decode_vertex_buffer(bin_slice(bin, positions), count(positions))
                        .expect("meshopt decodes the positions");
                let node = &json["nodes"][0];
                let translation = |axis: usize| node["translation"][axis].as_f64().unwrap();
                let scale = |axis: usize| node["scale"][axis].as_f64().unwrap();
                (
                    quantized
                        .iter()
                        .map(|position| {
                            [0, 1, 2].map(|axis| {
                                (translation(axis) + scale(axis) * f64::from(position[axis])) as f32
                            })
                        })
                        .collect(),
                    meshopt::decode_index_sequence(bin_slice(bin, indices), count(indices))
                        .expect("meshopt decodes the indices"),
                )
            }
            MeshCompression::Draco => {
                let geometry = draco_gltf::parse(tile, draco_gltf::ValidationProfile::Gltf20)
                    .expect("draco-gltf parses the tile")
                    .read_primitive(draco_gltf::PrimitiveIndex::new(
                        draco_gltf::MeshIndex::from(0usize),
                        0,
                    ))
                    .expect("draco-gltf decodes the primitive");
                let positions = geometry
                    .attributes()
                    .iter()
                    .find(|attribute| attribute.semantic() == "POSITION")
                    .expect("decoded positions");
                (
                    bytemuck::pod_collect_to_vec(positions.bytes()),
                    bytemuck::pod_collect_to_vec(
                        geometry.indices().expect("decoded indices").bytes(),
                    ),
                )
            }
        }
    }

    #[test]
    fn cad_edges_of_every_compression_draw_each_box_edge_once_and_hide_the_face_diagonals() {
        let welded_box = MeshData {
            asset_id: Some("box".to_string()),
            ..cube_mesh()
        };
        for mesh_compression in [
            MeshCompression::None,
            MeshCompression::Meshopt,
            MeshCompression::Draco,
        ] {
            for fixture in [face_split_box("box"), welded_box.clone()] {
                let config = MeshTilingConfig {
                    cad_edges: true,
                    mesh_compression,
                    ..Default::default()
                };
                let written = write_tileset(&[fixture], &config);
                let tile = std::fs::read(written.dir.join("tiles/root.glb")).unwrap();
                let glb = gltf::Glb::from_slice(&tile).expect("a parseable GLB");
                let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
                let bin = glb.bin.unwrap_or_default();

                let extensions = &json["meshes"][0]["primitives"][0]["extensions"];
                assert!(extensions["EXT_mesh_features"].is_object());
                let extension = edge_visibility_extension(&json);
                assert!(extension.get("silhouetteNormals").is_none());
                let visibility = &extension["visibility"];
                // 12 triangles hold 36 edges at 2 bits each
                assert_eq!(
                    json["accessors"][visibility.as_u64().unwrap() as usize]["count"],
                    9
                );
                let packed = bin_slice(&bin, accessor_view(&json, visibility));
                let (positions, indices) =
                    decoded_positions_and_indices(&tile, &json, &bin, mesh_compression);

                let corner = |vertex: u32| {
                    positions[vertex as usize].map(|component| (component * 2.0).round() as i32)
                };
                let mut values_by_edge: BTreeMap<[[i32; 3]; 2], Vec<u8>> = BTreeMap::new();
                let corner_pairs = indices.as_chunks::<3>().0.iter().flat_map(|corners| {
                    [
                        (corners[0], corners[1]),
                        (corners[1], corners[2]),
                        (corners[2], corners[0]),
                    ]
                });
                for (edge, (start, end)) in corner_pairs.enumerate() {
                    let value = (packed[edge / 4] >> ((edge % 4) * 2)) & 3;
                    let mut ends = [corner(start), corner(end)];
                    ends.sort_unstable();
                    values_by_edge.entry(ends).or_default().push(value);
                }

                let (box_edges, diagonals): (Vec<_>, Vec<_>) =
                    values_by_edge.into_iter().partition(|(ends, _)| {
                        (0..3)
                            .filter(|&axis| ends[0][axis] != ends[1][axis])
                            .count()
                            == 1
                    });
                assert_eq!(box_edges.len(), 12, "{mesh_compression:?}");
                assert_eq!(diagonals.len(), 6, "{mesh_compression:?}");
                for (ends, mut values) in box_edges {
                    values.sort_unstable();
                    assert_eq!(values, [2, 3], "{mesh_compression:?} box edge {ends:?}");
                }
                for (ends, values) in diagonals {
                    assert_eq!(
                        values,
                        [0, 0],
                        "{mesh_compression:?} face diagonal {ends:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn cad_edges_reach_tiles_of_every_compression_only_when_the_tileset_asks_for_them() {
        for (cad_edges, mesh_compression, expected) in [
            (false, MeshCompression::None, false),
            (false, MeshCompression::Meshopt, false),
            (false, MeshCompression::Draco, false),
            (true, MeshCompression::None, true),
            (true, MeshCompression::Meshopt, true),
            (true, MeshCompression::Draco, true),
        ] {
            let config = MeshTilingConfig {
                cad_edges,
                mesh_compression,
                ..Default::default()
            };
            let written = write_tileset(&[face_split_box("box")], &config);
            let bytes = std::fs::read(written.dir.join("tiles/root.glb")).unwrap();
            let glb = gltf::Glb::from_slice(&bytes).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();

            let declared = json["extensionsUsed"].as_array().is_some_and(|names| {
                names.contains(&serde_json::json!("EXT_mesh_primitive_edge_visibility"))
            });
            assert_eq!(declared, expected, "{cad_edges} {mesh_compression:?}");
            assert_eq!(
                edge_visibility_extension(&json).is_object(),
                expected,
                "{cad_edges} {mesh_compression:?}"
            );
        }
    }

    #[test]
    fn cad_edges_expand_repeated_elements_so_each_copy_keeps_its_edges() {
        let mut meshes = numbered_doors(6);
        meshes.push(shaded_by_its_normals(triangle_at(-20.0, Some("wall"))));
        let config = MeshTilingConfig {
            cad_edges: true,
            ..Default::default()
        };
        let written = write_tileset(&meshes, &config);
        let (json, views) = written.glb(&written.tileset.root);

        assert!(instanced_nodes(&json).is_empty());
        assert_eq!(json["meshes"].as_array().unwrap().len(), 1);
        let extension = edge_visibility_extension(&json);
        assert!(extension.get("silhouetteNormals").is_none());
        let visibility = &extension["visibility"];
        let indices = index_values(&json, &views);
        assert_eq!(
            json["accessors"][visibility.as_u64().unwrap() as usize]["count"],
            indices.len().div_ceil(4)
        );

        let positions: Vec<[f32; 3]> =
            attribute_values(&json, &views, "POSITION").expect("positions");
        let (min_x, max_x) = positions
            .iter()
            .fold((f32::MAX, f32::MIN), |(min_x, max_x), position| {
                (min_x.min(position[0]), max_x.max(position[0]))
            });
        assert!(
            max_x - min_x > 20.0,
            "the six doors stay apart, span {min_x} to {max_x}"
        );

        let rows = asset_ids(&json, &views);
        let mut reached: Vec<&str> = referenced_feature_ids(&json, &views)
            .iter()
            .map(|feature_id| rows[*feature_id as usize].as_str())
            .collect();
        reached.sort_unstable();
        assert_eq!(
            reached,
            [
                "door-0", "door-1", "door-2", "door-3", "door-4", "door-5", "wall",
            ]
        );
    }
}
