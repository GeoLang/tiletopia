use crate::bounding_volume::Aabb;
use crate::glb_writer::{self, COMPONENT_FLOAT, COMPONENT_UNSIGNED_BYTE};
use crate::lod::geometric_error_for;
use crate::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub const MAXIMUM_SPLATS_PER_TILE: usize = 32_768;
const MAXIMUM_DEPTH: u32 = 20;

pub const HIGHER_DEGREE_COEFFICIENT_COUNTS: [usize; 4] = [0, 9, 24, 45];
const HIGHER_DEGREE_COEFFICIENTS_PER_DEGREE: [(u8, usize); 3] = [(1, 3), (2, 5), (3, 7)];

const KHR_GAUSSIAN_SPLATTING: &str = "KHR_gaussian_splatting";
const KHR_GAUSSIAN_SPLATTING_COMPRESSION_SPZ_2: &str = "KHR_gaussian_splatting_compression_spz_2";

// "NGSP" read as a little endian u32
const SPZ_MAGIC: u32 = 0x5053_474e;
// spz 2.0.0 writes version 3, which stores the smallest three quaternion components
const SPZ_VERSION: u32 = 3;
const SPZ_HEADER_BYTES: usize = 16;
const SPZ_POSITION_BYTES: usize = 9;
// spz 2.0.0 writes 12 fractional bits, about a quarter millimetre
const MAXIMUM_FRACTIONAL_BITS: u32 = 12;
// spz positions are 24 bit signed fixed point numbers
const LARGEST_FIXED_POINT: f64 = ((1 << 23) - 1) as f64;
// spz stores 0.15 times the dc coefficient, not the 0.282 times that gives colour
const SPZ_COLOR_SCALE: f32 = 0.15;
const ROTATION_COMPONENT_BITS: u32 = 9;
// spz stores a log scale as (log scale + 10) * 16 in one byte
const SPZ_LOG_SCALE_OFFSET: f32 = 10.0;
const SPZ_LOG_SCALE_STEPS: f32 = 16.0;
// KHR_gaussian_splatting cuts the ellipse kernel off at a Mahalanobis distance of 3
const KERNEL_CUTOFF_SIGMAS: f64 = 3.0;

const ALPHA_OFFSET: usize = 0;
const COLOR_OFFSET: usize = 1;
const SCALE_OFFSET: usize = 4;
const ROTATION_OFFSET: usize = 7;
const HIGHER_DEGREE_OFFSET: usize = 11;

const POSITION_BYTES: u64 = 12;
const INDEX_BYTES: u64 = 4;

#[derive(Debug, Clone)]
pub struct GaussianSplat {
    pub position: [f32; 3],
    pub log_scale: [f32; 3],
    pub rotation_xyzw: [f32; 4],
    pub opacity_logit: f32,
    pub color_dc: [f32; 3],
    // spz order: coefficient major, colour channel inner
    pub higher_degree_coefficients: [f32; 45],
}

pub struct SplatCloud {
    spherical_harmonics_degree: u8,
    positions: Vec<[f32; 3]>,
    packed_records: Vec<u8>,
}

impl SplatCloud {
    pub fn with_capacity(
        splat_count: usize,
        spherical_harmonics_degree: u8,
        max_bytes_held: u64,
    ) -> Result<Self, String> {
        let Some(&higher_degree_count) =
            HIGHER_DEGREE_COEFFICIENT_COUNTS.get(usize::from(spherical_harmonics_degree))
        else {
            return Err(format!(
                "spherical harmonics degree {spherical_harmonics_degree}: spz holds degrees 0 to 3"
            ));
        };
        let record_bytes = HIGHER_DEGREE_OFFSET + higher_degree_count;
        let needed = splat_tiling_bytes(splat_count as u64, record_bytes as u64);
        if needed > max_bytes_held {
            return Err(format!(
                "the capture holds {splat_count} splats with degree {spherical_harmonics_degree} \
                 spherical harmonics, which take {needed} bytes to tile, over the {max_bytes_held} \
                 byte job budget"
            ));
        }
        Ok(Self {
            spherical_harmonics_degree,
            positions: Vec::with_capacity(splat_count),
            packed_records: Vec::with_capacity(splat_count * record_bytes),
        })
    }

    pub fn len(&self) -> usize {
        self.positions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    fn record_bytes(&self) -> usize {
        HIGHER_DEGREE_OFFSET
            + HIGHER_DEGREE_COEFFICIENT_COUNTS[usize::from(self.spherical_harmonics_degree)]
    }

    fn record(&self, index: u32) -> &[u8] {
        let record_bytes = self.record_bytes();
        let start = index as usize * record_bytes;
        &self.packed_records[start..start + record_bytes]
    }

    pub fn push(&mut self, splat: &GaussianSplat) -> Result<(), String> {
        let rotation = pack_rotation(splat.rotation_xyzw).ok_or_else(|| {
            format!(
                "splat {}: its rotation quaternion has no length",
                self.len()
            )
        })?;
        let higher_degree_count =
            HIGHER_DEGREE_COEFFICIENT_COUNTS[usize::from(self.spherical_harmonics_degree)];

        self.positions.push(splat.position);
        let alpha = sigmoid(splat.opacity_logit) * 255.0;
        self.packed_records.push(to_byte(alpha));
        for dc in splat.color_dc {
            self.packed_records
                .push(to_byte(dc * (SPZ_COLOR_SCALE * 255.0) + 0.5 * 255.0));
        }
        for log_scale in splat.log_scale {
            self.packed_records.push(to_byte(
                (log_scale + SPZ_LOG_SCALE_OFFSET) * SPZ_LOG_SCALE_STEPS,
            ));
        }
        self.packed_records.extend_from_slice(&rotation);
        for coefficient in &splat.higher_degree_coefficients[..higher_degree_count] {
            self.packed_records
                .push(to_byte(coefficient * 128.0 + 128.0));
        }
        Ok(())
    }

    fn attribute_blocks(&self) -> [(usize, usize); 5] {
        [
            (ALPHA_OFFSET, 1),
            (COLOR_OFFSET, 3),
            (SCALE_OFFSET, 3),
            (ROTATION_OFFSET, 4),
            (
                HIGHER_DEGREE_OFFSET,
                self.record_bytes() - HIGHER_DEGREE_OFFSET,
            ),
        ]
    }

    fn log_opacity_times_area(&self, index: u32) -> f32 {
        let record = self.record(index);
        let mut scales = [
            record[SCALE_OFFSET],
            record[SCALE_OFFSET + 1],
            record[SCALE_OFFSET + 2],
        ];
        scales.sort_unstable();
        f32::from(record[ALPHA_OFFSET]).ln()
            + f32::from(u16::from(scales[1]) + u16::from(scales[2])) / SPZ_LOG_SCALE_STEPS
    }

    fn extent_radius(&self, index: u32) -> f64 {
        let scales = &self.record(index)[SCALE_OFFSET..SCALE_OFFSET + 3];
        let largest = scales.iter().copied().max().expect("three scales");
        let log_scale = f32::from(largest) / SPZ_LOG_SCALE_STEPS - SPZ_LOG_SCALE_OFFSET;
        KERNEL_CUTOFF_SIGMAS * f64::from(log_scale).exp()
    }

    fn splat_extents(&self, indices: &[u32]) -> Aabb {
        let mut extents = Aabb::empty();
        for &index in indices {
            let center = self.positions[index as usize].map(f64::from);
            let radius = self.extent_radius(index);
            extents.expand_point(center.map(|coordinate| coordinate - radius));
            extents.expand_point(center.map(|coordinate| coordinate + radius));
        }
        extents
    }
}

fn splat_tiling_bytes(splat_count: u64, record_bytes: u64) -> u64 {
    let held_bytes = splat_count.saturating_mul(POSITION_BYTES + record_bytes + INDEX_BYTES);
    let tile_limit = MAXIMUM_SPLATS_PER_TILE as u64;
    let tree_walk_bytes = u64::from(MAXIMUM_DEPTH + 1) * 2 * tile_limit * INDEX_BYTES;
    let tile_write_bytes = 3 * tile_limit * (SPZ_POSITION_BYTES as u64 + record_bytes);
    held_bytes.saturating_add(tree_walk_bytes + tile_write_bytes)
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value).exp())
}

fn to_byte(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn pack_rotation(rotation_xyzw: [f32; 4]) -> Option<[u8; 4]> {
    let length = rotation_xyzw.iter().map(|c| c * c).sum::<f32>().sqrt();
    if !length.is_normal() {
        return None;
    }
    let quaternion = rotation_xyzw.map(|c| c / length);
    let largest = (0..4)
        .max_by(|&a, &b| quaternion[a].abs().total_cmp(&quaternion[b].abs()))
        .expect("four components");
    // q and -q are the same rotation
    let negate = quaternion[largest] < 0.0;
    let largest_magnitude = (1u32 << ROTATION_COMPONENT_BITS) - 1;
    let mut packed = largest as u32;
    for (component, value) in quaternion.iter().enumerate() {
        if component == largest {
            continue;
        }
        let sign = u32::from((*value < 0.0) != negate);
        // the three smaller components of a unit quaternion are at most one over root two
        let magnitude = ((largest_magnitude as f32)
            * (value.abs() / std::f32::consts::FRAC_1_SQRT_2)
            + 0.5) as u32;
        packed = (packed << (ROTATION_COMPONENT_BITS + 1))
            | (sign << ROTATION_COMPONENT_BITS)
            | magnitude.min(largest_magnitude);
    }
    Some(packed.to_le_bytes())
}

struct WrittenNode {
    tile: Tile,
    extents: Aabb,
    kept: Vec<u32>,
}

pub fn tile_splats(
    cloud: &SplatCloud,
    output_dir: &Path,
    root_transform: [f64; 16],
) -> io::Result<usize> {
    if cloud.is_empty() {
        return Err(io::Error::other("the capture holds no splats"));
    }
    let tiles_dir = output_dir.join("tiles");
    std::fs::create_dir_all(&tiles_dir)?;

    let mut bounds = Aabb::empty();
    for position in &cloud.positions {
        bounds.expand_point(position.map(f64::from));
    }
    let mut indices: Vec<u32> = (0..cloud.len() as u32).collect();
    let mut root = write_node(
        cloud,
        &mut indices,
        bounds,
        0,
        &mut String::new(),
        &tiles_dir,
    )?
    .tile;
    root.transform = Some(root_transform);
    let tile_count = count_tiles(&root);

    let tileset = Tileset {
        asset: TilesetAsset {
            version: "1.1".to_string(),
            tileset_version: Some("1.0.0".to_string()),
            generator: Some("tiletopia".to_string()),
        },
        geometric_error: geometric_error_for(&bounds),
        root,
    };
    let mut json = serde_json::to_value(&tileset).map_err(io::Error::other)?;
    let splat_extensions = [
        KHR_GAUSSIAN_SPLATTING,
        KHR_GAUSSIAN_SPLATTING_COMPRESSION_SPZ_2,
    ];
    // cesiumjs 1.147 draws glb content as splats only when the tileset requires both extensions here
    json["extensionsUsed"] = serde_json::json!(["3DTILES_content_gltf"]);
    json["extensions"] = serde_json::json!({
        "3DTILES_content_gltf": {
            "extensionsUsed": splat_extensions,
            "extensionsRequired": splat_extensions,
        }
    });
    let text = serde_json::to_string_pretty(&json).map_err(io::Error::other)?;
    std::fs::write(output_dir.join("tileset.json"), text)?;
    Ok(tile_count)
}

fn count_tiles(tile: &Tile) -> usize {
    usize::from(tile.content.is_some()) + tile.children.iter().map(count_tiles).sum::<usize>()
}

fn write_node(
    cloud: &SplatCloud,
    indices: &mut [u32],
    bounds: Aabb,
    depth: u32,
    path: &mut String,
    tiles_dir: &Path,
) -> io::Result<WrittenNode> {
    let file_name = if path.is_empty() {
        "root.glb".to_string()
    } else {
        format!("{path}.glb")
    };
    let content = Some(TileContent {
        uri: format!("tiles/{file_name}"),
    });
    let tile_path = tiles_dir.join(file_name);

    if indices.len() <= MAXIMUM_SPLATS_PER_TILE {
        write_tile(cloud, indices, &bounds, &tile_path)?;
        let extents = cloud.splat_extents(indices);
        return Ok(WrittenNode {
            tile: Tile {
                bounding_volume: BoundingVolume::Box {
                    r#box: extents.to_3dtiles_box(),
                },
                geometric_error: 0.0,
                content,
                children: Vec::new(),
                refine: Some(Refine::Replace),
                transform: None,
            },
            extents,
            kept: indices.to_vec(),
        });
    }
    if depth == MAXIMUM_DEPTH {
        return Err(io::Error::other(format!(
            "{} splats sit in one cell {} m across, more than the {MAXIMUM_SPLATS_PER_TILE} a tile holds",
            indices.len(),
            bounds.max_extent()
        )));
    }

    let octant_bounds = bounds.octants();
    let octant_lengths = partition_into_octants(cloud, indices, bounds.center());
    let mut remaining = indices;
    let mut children = Vec::new();
    let mut kept = Vec::new();
    let mut extents = Aabb::empty();
    for (octant, length) in octant_lengths.into_iter().enumerate() {
        let (octant_indices, rest) = std::mem::take(&mut remaining).split_at_mut(length);
        remaining = rest;
        if octant_indices.is_empty() {
            continue;
        }
        path.push(char::from(b'0' + octant as u8));
        let child = write_node(
            cloud,
            octant_indices,
            octant_bounds[octant],
            depth + 1,
            path,
            tiles_dir,
        )?;
        path.pop();
        children.push(child.tile);
        extents.expand_point(child.extents.min);
        extents.expand_point(child.extents.max);
        kept.extend(child.kept);
        keep_largest_opacity_times_area(cloud, &mut kept);
    }

    write_tile(cloud, &kept, &bounds, &tile_path)?;
    Ok(WrittenNode {
        tile: Tile {
            bounding_volume: BoundingVolume::Box {
                r#box: extents.to_3dtiles_box(),
            },
            geometric_error: geometric_error_for(&bounds),
            content,
            children,
            refine: Some(Refine::Replace),
            transform: None,
        },
        extents,
        kept,
    })
}

fn keep_largest_opacity_times_area(cloud: &SplatCloud, kept: &mut Vec<u32>) {
    if kept.len() <= MAXIMUM_SPLATS_PER_TILE {
        return;
    }
    kept.select_nth_unstable_by(MAXIMUM_SPLATS_PER_TILE - 1, |a, b| {
        cloud
            .log_opacity_times_area(*b)
            .total_cmp(&cloud.log_opacity_times_area(*a))
    });
    kept.truncate(MAXIMUM_SPLATS_PER_TILE);
}

fn partition_into_octants(cloud: &SplatCloud, indices: &mut [u32], center: [f64; 3]) -> [usize; 8] {
    let is_low = move |axis: usize| {
        move |index: u32| f64::from(cloud.positions[index as usize][axis]) < center[axis]
    };
    let mut lengths = [0; 8];
    let z_low_count = partition(indices, is_low(2));
    let (z_low, z_high) = indices.split_at_mut(z_low_count);
    for (z, z_half) in [z_low, z_high].into_iter().enumerate() {
        let y_low_count = partition(z_half, is_low(1));
        let (y_low, y_high) = z_half.split_at_mut(y_low_count);
        for (y, y_half) in [y_low, y_high].into_iter().enumerate() {
            let x_low_count = partition(y_half, is_low(0));
            lengths[z * 4 + y * 2] = x_low_count;
            lengths[z * 4 + y * 2 + 1] = y_half.len() - x_low_count;
        }
    }
    lengths
}

fn partition(indices: &mut [u32], goes_first: impl Fn(u32) -> bool) -> usize {
    let mut first_count = 0;
    for position in 0..indices.len() {
        if goes_first(indices[position]) {
            indices.swap(first_count, position);
            first_count += 1;
        }
    }
    first_count
}

fn fractional_bits_for(largest_offset: f64) -> io::Result<u32> {
    (0..=MAXIMUM_FRACTIONAL_BITS)
        .rev()
        .find(|bits| largest_offset * f64::from(1u32 << bits) <= LARGEST_FIXED_POINT)
        .ok_or_else(|| {
            io::Error::other(format!(
                "a tile reaches {largest_offset} m from its centre, past the \
                 {LARGEST_FIXED_POINT} m spz positions reach"
            ))
        })
}

fn write_tile(cloud: &SplatCloud, indices: &[u32], bounds: &Aabb, path: &Path) -> io::Result<()> {
    let center = bounds.center();
    let half_extents = bounds.half_extents();
    let fractional_bits =
        fractional_bits_for(half_extents[0].max(half_extents[1]).max(half_extents[2]))?;
    let fixed_point_scale = f64::from(1u32 << fractional_bits);
    let splat_count = indices.len();

    let mut spz = Vec::with_capacity(
        SPZ_HEADER_BYTES + splat_count * (SPZ_POSITION_BYTES + cloud.record_bytes()),
    );
    spz.extend_from_slice(&SPZ_MAGIC.to_le_bytes());
    spz.extend_from_slice(&SPZ_VERSION.to_le_bytes());
    spz.extend_from_slice(&(splat_count as u32).to_le_bytes());
    spz.extend_from_slice(&[
        cloud.spherical_harmonics_degree,
        fractional_bits as u8,
        0,
        0,
    ]);

    let mut stored_bounds = Aabb::empty();
    for &index in indices {
        let position = cloud.positions[index as usize];
        let mut stored = [0.0; 3];
        for (axis, coordinate) in position.into_iter().enumerate() {
            let fixed = ((f64::from(coordinate) - center[axis]) * fixed_point_scale).round() as i32;
            spz.extend_from_slice(&fixed.to_le_bytes()[..3]);
            stored[axis] = f64::from(fixed) / fixed_point_scale;
        }
        stored_bounds.expand_point(stored);
    }
    for (offset, length) in cloud.attribute_blocks() {
        for &index in indices {
            spz.extend_from_slice(&cloud.record(index)[offset..offset + length]);
        }
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&spz)?;
    drop(spz);
    let mut bin = encoder.finish()?;
    let spz_length = bin.len();
    bin.resize(glb_writer::align4(spz_length), 0);

    let document = splat_document(cloud, splat_count, center, &stored_bounds, spz_length);
    let mut writer = BufWriter::new(std::fs::File::create(path)?);
    glb_writer::write_container(&document, &bin, &mut writer)?;
    writer.flush()
}

fn splat_document(
    cloud: &SplatCloud,
    splat_count: usize,
    center: [f64; 3],
    stored_bounds: &Aabb,
    spz_length: usize,
) -> serde_json::Value {
    let float_accessor = |accessor_type: &str| {
        serde_json::json!({
            "componentType": COMPONENT_FLOAT,
            "count": splat_count,
            "type": accessor_type,
        })
    };
    let mut accessors = vec![
        serde_json::json!({
            "componentType": COMPONENT_FLOAT,
            "count": splat_count,
            "type": "VEC3",
            "min": stored_bounds.min,
            "max": stored_bounds.max,
        }),
        serde_json::json!({
            "componentType": COMPONENT_UNSIGNED_BYTE,
            "normalized": true,
            "count": splat_count,
            "type": "VEC4",
        }),
        float_accessor("VEC3"),
        float_accessor("VEC4"),
    ];
    let mut attributes = serde_json::json!({
        "POSITION": 0,
        "COLOR_0": 1,
        "KHR_gaussian_splatting:SCALE": 2,
        "KHR_gaussian_splatting:ROTATION": 3,
    });
    for (degree, coefficients) in HIGHER_DEGREE_COEFFICIENTS_PER_DEGREE {
        if degree > cloud.spherical_harmonics_degree {
            break;
        }
        for coefficient in 0..coefficients {
            attributes[format!("KHR_gaussian_splatting:SH_DEGREE_{degree}_COEF_{coefficient}")] =
                serde_json::json!(accessors.len());
            accessors.push(float_accessor("VEC3"));
        }
    }

    // 3d tiles turns y up gltf content to z up
    let matrix = [
        1.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 1.0, 0.0, 0.0, center[0], center[2],
        -center[1], 1.0,
    ];
    let splat_extensions = [
        KHR_GAUSSIAN_SPLATTING,
        KHR_GAUSSIAN_SPLATTING_COMPRESSION_SPZ_2,
    ];
    serde_json::json!({
        "asset": { "version": "2.0", "generator": "tiletopia" },
        "extensionsUsed": splat_extensions,
        "extensionsRequired": splat_extensions,
        "scene": 0,
        "scenes": [{ "nodes": [0] }],
        "nodes": [{ "mesh": 0, "matrix": matrix }],
        "meshes": [{
            "primitives": [{
                "mode": 0,
                "attributes": attributes,
                "extensions": {
                    KHR_GAUSSIAN_SPLATTING: {
                        "kernel": "ellipse",
                        "colorSpace": "srgb_rec709_display",
                        "extensions": {
                            KHR_GAUSSIAN_SPLATTING_COMPRESSION_SPZ_2: { "bufferView": 0 }
                        }
                    }
                }
            }]
        }],
        "accessors": accessors,
        "bufferViews": [{ "buffer": 0, "byteLength": spz_length }],
        "buffers": [{ "byteLength": glb_writer::align4(spz_length) }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_over_the_budget_is_refused_naming_its_size() {
        let error = match SplatCloud::with_capacity(5_000_000, 3, 1024 * 1024 * 1024 / 3) {
            Ok(_) => panic!("five million degree 3 splats fit the budget"),
            Err(error) => error,
        };
        assert!(error.contains("5000000 splats"), "{error}");
        assert!(error.contains("degree 3"), "{error}");
    }

    #[test]
    fn a_splat_without_a_rotation_is_refused() {
        let mut cloud = SplatCloud::with_capacity(1, 0, u64::MAX).unwrap();
        let error = cloud
            .push(&GaussianSplat {
                position: [0.0; 3],
                log_scale: [0.0; 3],
                rotation_xyzw: [0.0; 4],
                opacity_logit: 0.0,
                color_dc: [0.0; 3],
                higher_degree_coefficients: [0.0; 45],
            })
            .unwrap_err();
        assert!(error.contains("splat 0"), "{error}");
        assert!(error.contains("rotation"), "{error}");
    }
}
