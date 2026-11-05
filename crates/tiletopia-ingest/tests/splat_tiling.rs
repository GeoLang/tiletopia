use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use tiletopia_core::splat_tiler::{MAXIMUM_SPLATS_PER_TILE, tile_splats};
use tiletopia_ingest::splat_ply_reader;

const GRID: [usize; 3] = [40, 40, 25];
const SPACING_METRES: f64 = 0.25;
const ORIGIN: [f64; 3] = [100.0, -50.0, 20.0];
const READ_BUDGET_BYTES: u64 = 1 << 30;
// KHR_gaussian_splatting: diffuse colour is the dc coefficient times this plus 0.5
const SPHERICAL_HARMONIC_DEGREE_0: f64 = 0.282_094_791_773_878;
// spz stores 0.15 times the dc coefficient
const SPZ_COLOR_SCALE: f64 = 0.15;
const SPZ_MAGIC: u32 = 0x5053_474e;

// spz keeps positions to 12 fractional bits and f32 input adds a little more
const POSITION_TOLERANCE_METRES: f64 = 5e-4;
// spz stores log scale in steps of one sixteenth, so linear scale is within e^(1/32) - 1
const RELATIVE_SCALE_TOLERANCE: f64 = 0.032;
// spz rotations keep 9 bits over 0 to one over root two, and the largest component adds up their error
const ROTATION_TOLERANCE: f64 = 3e-3;
const OPACITY_TOLERANCE: f64 = 0.5 / 255.0 + 1e-6;
const COLOR_TOLERANCE: f64 = SPHERICAL_HARMONIC_DEGREE_0 * 0.5 / (SPZ_COLOR_SCALE * 255.0) + 1e-6;
const HIGHER_DEGREE_TOLERANCE: f64 = 0.5 / 128.0 + 1e-6;
const LOG_OPACITY_TIMES_AREA_TOLERANCE: f64 = 1e-4;
const BOX_TOLERANCE_METRES: f64 = 1e-9;
// KHR_gaussian_splatting cuts the ellipse kernel off at a Mahalanobis distance of 3
const KERNEL_CUTOFF_SIGMAS: f64 = 3.0;

const PLY_PROPERTIES: [&str; 26] = [
    "x", "y", "z", "nx", "ny", "nz", "f_dc_0", "f_dc_1", "f_dc_2", "f_rest_0", "f_rest_1",
    "f_rest_2", "f_rest_3", "f_rest_4", "f_rest_5", "f_rest_6", "f_rest_7", "f_rest_8", "opacity",
    "scale_0", "scale_1", "scale_2", "rot_0", "rot_1", "rot_2", "rot_3",
];

struct InputSplat {
    position: [f32; 3],
    color_dc: [f32; 3],
    // channel major, as 3dgs writes f_rest
    higher_degree: [f32; 9],
    opacity_logit: f32,
    log_scale: [f32; 3],
    rotation_wxyz: [f32; 4],
}

impl InputSplat {
    fn ply_row(&self) -> Vec<f32> {
        let mut row = self.position.to_vec();
        row.extend([0.0; 3]);
        row.extend(self.color_dc);
        row.extend(self.higher_degree);
        row.push(self.opacity_logit);
        row.extend(self.log_scale);
        row.extend(self.rotation_wxyz);
        row
    }
}

struct NumberSequence(u64);

impl NumberSequence {
    fn between(&mut self, low: f32, high: f32) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let unit = (self.0 >> 40) as f32 / (1u64 << 24) as f32;
        low + (high - low) * unit
    }
}

fn input_splats() -> Vec<InputSplat> {
    let mut numbers = NumberSequence(7);
    let mut splats = Vec::new();
    for i in 0..GRID[0] {
        for j in 0..GRID[1] {
            for k in 0..GRID[2] {
                let jitter = SPACING_METRES as f32 / 5.0;
                let cell = [i, j, k];
                let position = [0, 1, 2].map(|axis| {
                    (ORIGIN[axis] + cell[axis] as f64 * SPACING_METRES) as f32
                        + numbers.between(-jitter, jitter)
                });
                splats.push(InputSplat {
                    position,
                    color_dc: [0; 3].map(|_| numbers.between(-1.5, 1.5)),
                    higher_degree: [0; 9].map(|_| numbers.between(-0.9, 0.9)),
                    opacity_logit: numbers.between(-5.0, 5.0),
                    log_scale: [0; 3].map(|_| numbers.between(-6.0, -1.0)),
                    rotation_wxyz: [0; 4].map(|_| numbers.between(-2.0, 2.0)),
                });
            }
        }
    }
    splats
}

fn grid_cell(position: [f64; 3]) -> [i64; 3] {
    [0, 1, 2].map(|axis| ((position[axis] - ORIGIN[axis]) / SPACING_METRES).round() as i64)
}

fn write_binary_ply(path: &Path, properties: &[&str], rows: &[Vec<f32>]) {
    let mut bytes = format!(
        "ply\nformat binary_little_endian 1.0\nelement vertex {}\n",
        rows.len()
    )
    .into_bytes();
    for property in properties {
        bytes.extend_from_slice(format!("property float {property}\n").as_bytes());
    }
    bytes.extend_from_slice(b"end_header\n");
    for row in rows {
        for value in row {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).unwrap();
}

struct DecodedSplat {
    position: [f64; 3],
    linear_scale: [f64; 3],
    rotation_xyzw: [f64; 4],
    opacity: f64,
    color: [f64; 3],
    higher_degree: Vec<f64>,
}

fn sign_extended(bytes: &[u8]) -> i32 {
    let fill = if bytes[2] & 0x80 == 0 { 0 } else { 0xff };
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], fill])
}

fn smallest_three_rotation(bytes: &[u8]) -> [f64; 4] {
    let mut packed = u32::from_le_bytes(bytes.try_into().unwrap());
    let largest = (packed >> 30) as usize;
    let mut rotation = [0.0; 4];
    let mut sum_of_squares = 0.0;
    for component in (0..4).rev() {
        if component == largest {
            continue;
        }
        let magnitude = f64::from(packed & 511) / 511.0 * std::f64::consts::FRAC_1_SQRT_2;
        let negative = (packed >> 9) & 1 == 1;
        packed >>= 10;
        rotation[component] = if negative { -magnitude } else { magnitude };
        sum_of_squares += rotation[component] * rotation[component];
    }
    rotation[largest] = (1.0 - sum_of_squares).max(0.0).sqrt();
    rotation
}

struct DecodedTile {
    gltf: serde_json::Value,
    splats: Vec<DecodedSplat>,
}

fn decode_tile(path: &Path) -> DecodedTile {
    let glb = std::fs::read(path).unwrap();
    let json_length = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
    let gltf: serde_json::Value = serde_json::from_slice(&glb[20..20 + json_length]).unwrap();
    let bin = &glb[20 + json_length + 8..];
    let primitive = &gltf["meshes"][0]["primitives"][0];
    let view_index = primitive["extensions"]["KHR_gaussian_splatting"]["extensions"]
        ["KHR_gaussian_splatting_compression_spz_2"]["bufferView"]
        .as_u64()
        .unwrap() as usize;
    let view_length = gltf["bufferViews"][view_index]["byteLength"]
        .as_u64()
        .unwrap() as usize;
    let mut spz = Vec::new();
    flate2::read::GzDecoder::new(&bin[..view_length])
        .read_to_end(&mut spz)
        .unwrap();

    let word = |offset: usize| u32::from_le_bytes(spz[offset..offset + 4].try_into().unwrap());
    assert_eq!(word(0), SPZ_MAGIC);
    assert_eq!(word(4), 3);
    let count = word(8) as usize;
    let degree = spz[12] as usize;
    let fixed_point_scale = f64::from(1u32 << spz[13]);
    let higher_degree_count = [0, 9, 24, 45][degree];
    let positions = 16;
    let alphas = positions + count * 9;
    let colors = alphas + count;
    let scales = colors + count * 3;
    let rotations = scales + count * 3;
    let higher_degrees = rotations + count * 4;
    assert_eq!(spz.len(), higher_degrees + count * higher_degree_count);

    let matrix: Vec<f64> = serde_json::from_value(gltf["nodes"][0]["matrix"].clone()).unwrap();
    let splats = (0..count)
        .map(|splat| {
            let local = [0, 1, 2].map(|axis| {
                let start = positions + splat * 9 + axis * 3;
                f64::from(sign_extended(&spz[start..start + 3])) / fixed_point_scale
            });
            let y_up = [0, 1, 2].map(|row| {
                matrix[row] * local[0]
                    + matrix[4 + row] * local[1]
                    + matrix[8 + row] * local[2]
                    + matrix[12 + row]
            });
            DecodedSplat {
                position: [y_up[0], -y_up[2], y_up[1]],
                linear_scale: [0, 1, 2]
                    .map(|axis| (f64::from(spz[scales + splat * 3 + axis]) / 16.0 - 10.0).exp()),
                rotation_xyzw: smallest_three_rotation(
                    &spz[rotations + splat * 4..rotations + splat * 4 + 4],
                ),
                opacity: f64::from(spz[alphas + splat]) / 255.0,
                color: [0, 1, 2].map(|channel| {
                    let dc = (f64::from(spz[colors + splat * 3 + channel]) / 255.0 - 0.5)
                        / SPZ_COLOR_SCALE;
                    0.5 + SPHERICAL_HARMONIC_DEGREE_0 * dc
                }),
                higher_degree: (0..higher_degree_count)
                    .map(|value| {
                        (f64::from(spz[higher_degrees + splat * higher_degree_count + value])
                            - 128.0)
                            / 128.0
                    })
                    .collect(),
            }
        })
        .collect();
    DecodedTile { gltf, splats }
}

struct TileVisit {
    box_values: [f64; 12],
    geometric_error: f64,
    uri: String,
}

fn collect_tiles(tile: &serde_json::Value, visits: &mut Vec<TileVisit>) {
    visits.push(TileVisit {
        box_values: serde_json::from_value(tile["boundingVolume"]["box"].clone()).unwrap(),
        geometric_error: tile["geometricError"].as_f64().unwrap(),
        uri: tile["content"]["uri"].as_str().unwrap().to_string(),
    });
    let parent_box = visits.last().unwrap().box_values;
    if let Some(children) = tile["children"].as_array() {
        for child in children {
            let child_box: [f64; 12] =
                serde_json::from_value(child["boundingVolume"]["box"].clone()).unwrap();
            assert!(
                (0..3).all(|axis| {
                    (child_box[axis] - parent_box[axis]).abs() + child_box[3 + axis * 4]
                        <= parent_box[3 + axis * 4] + BOX_TOLERANCE_METRES
                }),
                "{child_box:?} reaches outside its parent {parent_box:?}"
            );
            collect_tiles(child, visits);
        }
    }
}

fn box_contains_splat(
    box_values: &[f64; 12],
    center: [f64; 3],
    radius: f64,
    tolerance: f64,
) -> bool {
    (0..3).all(|axis| {
        let half_extent = box_values[3 + axis * 4];
        (center[axis] - box_values[axis]).abs() + radius <= half_extent + tolerance
    })
}

fn extent_radius(splat: &DecodedSplat) -> f64 {
    KERNEL_CUTOFF_SIGMAS * splat.linear_scale.into_iter().fold(0.0, f64::max)
}

fn log_opacity_times_area(splat: &DecodedSplat) -> f64 {
    let mut scales = splat.linear_scale;
    scales.sort_by(f64::total_cmp);
    (splat.opacity * scales[1] * scales[2]).ln()
}

#[test]
fn a_splat_ply_round_trips_through_spz_tiles() {
    let dir = tempfile::tempdir().unwrap();
    let ply = dir.path().join("capture.ply");
    let inputs = input_splats();
    let rows: Vec<Vec<f32>> = inputs.iter().map(InputSplat::ply_row).collect();
    write_binary_ply(&ply, &PLY_PROPERTIES, &rows);
    let input_by_cell: HashMap<[i64; 3], &InputSplat> = inputs
        .iter()
        .map(|splat| (grid_cell(splat.position.map(f64::from)), splat))
        .collect();
    assert_eq!(input_by_cell.len(), inputs.len());

    assert!(splat_ply_reader::is_splat_ply(&ply).unwrap());
    let cloud = splat_ply_reader::read(&ply, READ_BUDGET_BYTES).unwrap();
    let output = dir.path().join("tiles");
    let root_transform = tiletopia_core::spatial::enu_to_ecef_matrix(0.5, 0.2, 0.0);
    tile_splats(&cloud, &output, root_transform).unwrap();

    let tileset: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(output.join("tileset.json")).unwrap())
            .unwrap();
    assert_eq!(
        tileset["extensionsUsed"],
        serde_json::json!(["3DTILES_content_gltf"])
    );
    let mut visits = Vec::new();
    collect_tiles(&tileset["root"], &mut visits);

    let mut found_cells: HashMap<[i64; 3], usize> = HashMap::new();
    let mut root_ranks = Vec::new();
    let mut root_cells = HashSet::new();
    let mut leaf_ranks: HashMap<[i64; 3], f64> = HashMap::new();
    for (tile_index, visit) in visits.iter().enumerate() {
        let tile = decode_tile(&output.join(&visit.uri));
        let gltf = &tile.gltf;
        let splat_extensions = serde_json::json!([
            "KHR_gaussian_splatting",
            "KHR_gaussian_splatting_compression_spz_2"
        ]);
        assert_eq!(gltf["extensionsUsed"], splat_extensions);
        assert_eq!(gltf["extensionsRequired"], splat_extensions);
        let primitive = &gltf["meshes"][0]["primitives"][0];
        assert_eq!(primitive["mode"], 0);
        for semantic in [
            "POSITION",
            "COLOR_0",
            "KHR_gaussian_splatting:SCALE",
            "KHR_gaussian_splatting:ROTATION",
            "KHR_gaussian_splatting:SH_DEGREE_1_COEF_0",
            "KHR_gaussian_splatting:SH_DEGREE_1_COEF_1",
            "KHR_gaussian_splatting:SH_DEGREE_1_COEF_2",
        ] {
            let accessor = primitive["attributes"][semantic].as_u64().unwrap() as usize;
            assert_eq!(
                gltf["accessors"][accessor]["count"].as_u64().unwrap() as usize,
                tile.splats.len(),
                "{semantic}"
            );
        }
        assert!(primitive["attributes"]["KHR_gaussian_splatting:SH_DEGREE_2_COEF_0"].is_null());

        for splat in &tile.splats {
            assert!(
                box_contains_splat(
                    &visit.box_values,
                    splat.position,
                    extent_radius(splat),
                    POSITION_TOLERANCE_METRES
                ),
                "{:?} sits outside tile {}",
                splat.position,
                visit.uri
            );
            let cell = grid_cell(splat.position);
            let input = input_by_cell[&cell];
            let input_position = input.position.map(f64::from);
            let radius = extent_radius(splat);
            assert!(box_contains_splat(
                &visit.box_values,
                input_position,
                radius,
                BOX_TOLERANCE_METRES
            ));
            assert!(box_contains_splat(
                &visits[0].box_values,
                input_position,
                radius,
                BOX_TOLERANCE_METRES
            ));
            if tile_index == 0 {
                root_ranks.push(log_opacity_times_area(splat));
                root_cells.insert(cell);
            }
            if visit.geometric_error > 0.0 {
                continue;
            }
            *found_cells.entry(cell).or_default() += 1;
            leaf_ranks.insert(cell, log_opacity_times_area(splat));

            for (axis, input_coordinate) in input_position.iter().enumerate() {
                assert!(
                    (splat.position[axis] - input_coordinate).abs() < POSITION_TOLERANCE_METRES
                );
                let expected_scale = f64::from(input.log_scale[axis]).exp();
                assert!(
                    (splat.linear_scale[axis] / expected_scale - 1.0).abs()
                        < RELATIVE_SCALE_TOLERANCE
                );
                let expected_color =
                    0.5 + SPHERICAL_HARMONIC_DEGREE_0 * f64::from(input.color_dc[axis]);
                assert!((splat.color[axis] - expected_color).abs() < COLOR_TOLERANCE);
            }
            let expected_opacity = 1.0 / (1.0 + (-f64::from(input.opacity_logit)).exp());
            assert!((splat.opacity - expected_opacity).abs() < OPACITY_TOLERANCE);

            let [w, x, y, z] = input.rotation_wxyz.map(f64::from);
            let length = (w * w + x * x + y * y + z * z).sqrt();
            let expected_rotation = [x / length, y / length, z / length, w / length];
            let alignment: f64 = (0..4)
                .map(|c| expected_rotation[c] * splat.rotation_xyzw[c])
                .sum();
            for (component, expected) in expected_rotation.iter().enumerate() {
                let decoded = splat.rotation_xyzw[component] * alignment.signum();
                assert!((decoded - expected).abs() < ROTATION_TOLERANCE);
            }

            for coefficient in 0..3 {
                for channel in 0..3 {
                    let expected = f64::from(input.higher_degree[channel * 3 + coefficient]);
                    let decoded = splat.higher_degree[coefficient * 3 + channel];
                    assert!((decoded - expected).abs() < HIGHER_DEGREE_TOLERANCE);
                }
            }
        }
    }

    assert_eq!(found_cells.len(), inputs.len());
    assert!(found_cells.values().all(|&count| count == 1));

    assert!(
        visits[0].geometric_error > 0.0,
        "the root is not a coarser level"
    );
    assert_eq!(root_ranks.len(), MAXIMUM_SPLATS_PER_TILE);
    let lowest_kept_rank = root_ranks.iter().copied().fold(f64::INFINITY, f64::min);
    let highest_dropped_rank = leaf_ranks
        .iter()
        .filter(|(cell, _)| !root_cells.contains(*cell))
        .map(|(_, rank)| *rank)
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(lowest_kept_rank + LOG_OPACITY_TIMES_AREA_TOLERANCE >= highest_dropped_rank);
}

#[test]
fn an_ordinary_ply_is_no_splat_capture() {
    let dir = tempfile::tempdir().unwrap();
    let ply = dir.path().join("scan.ply");
    write_binary_ply(&ply, &["x", "y", "z", "nx", "ny", "nz"], &[vec![1.0; 6]]);
    assert!(!splat_ply_reader::is_splat_ply(&ply).unwrap());
}

#[test]
fn a_splat_ply_missing_a_property_fails_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let ply = dir.path().join("capture.ply");
    let properties: Vec<&str> = PLY_PROPERTIES
        .into_iter()
        .filter(|name| *name != "rot_2")
        .collect();
    write_binary_ply(&ply, &properties, &[vec![0.5; properties.len()]]);

    assert!(splat_ply_reader::is_splat_ply(&ply).unwrap());
    let error = match splat_ply_reader::read(&ply, READ_BUDGET_BYTES) {
        Ok(_) => panic!("a splat PLY without rot_2 was read"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("rot_2"), "{error}");
}
