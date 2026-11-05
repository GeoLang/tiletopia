//! An IFC from disk, through the readers, to 3D Tiles GLB content carrying
//! each element's GlobalId as the `asset_id` feature property.

use std::collections::BTreeSet;
use tiletopia_core::mesh_tiler::{MeshData, MeshTilingConfig, tile_meshes};

/// The twelve GlobalIds `twin_boxes.ifc` gives its IfcBuildingElementProxy
/// boxes, one per box, laid out on a 4 by 3 grid five metres apart. They are
/// real 22-character IFC GlobalIds: the alphabet is `0-9A-Za-z_$` and the
/// first character carries two bits, so it has to be `0` to `3`.
const BOX_GLOBAL_IDS: [&str; 12] = [
    "0TwinBox01000000000000",
    "0TwinBox02000000000000",
    "0TwinBox03000000000000",
    "0TwinBox04000000000000",
    "0TwinBox05000000000000",
    "0TwinBox06000000000000",
    "0TwinBox07000000000000",
    "0TwinBox08000000000000",
    "0TwinBox09000000000000",
    "0TwinBox10000000000000",
    "0TwinBox11000000000000",
    "0TwinBox12000000000000",
];

/// Small enough to split the fixture's 144 triangles over several tiles.
const TRIANGLES_PER_TILE: usize = 32;

struct Glb {
    json: serde_json::Value,
    bin: Vec<u8>,
}

fn read_glb(path: &std::path::Path) -> Glb {
    let bytes = std::fs::read(path).expect("a written tile");
    let glb = gltf::Glb::from_slice(&bytes).expect("a parseable GLB");
    Glb {
        json: serde_json::from_slice(&glb.json).unwrap(),
        bin: glb.bin.map(|bin| bin.to_vec()).unwrap_or_default(),
    }
}

impl Glb {
    fn buffer_view(&self, index: usize) -> &[u8] {
        let view = &self.json["bufferViews"][index];
        let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
        let length = view["byteLength"].as_u64().unwrap() as usize;
        &self.bin[offset..offset + length]
    }

    /// The `asset_id` column of the tile's property table.
    fn asset_ids(&self) -> Vec<String> {
        let table = &self.json["extensions"]["EXT_structural_metadata"]["propertyTables"][0];
        assert_eq!(table["class"], "asset");
        let property = &table["properties"]["asset_id"];
        let values = self.buffer_view(property["values"].as_u64().unwrap() as usize);
        let offsets_bytes = self.buffer_view(property["stringOffsets"].as_u64().unwrap() as usize);

        let offsets: Vec<usize> = offsets_bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk) as usize)
            .collect();
        assert_eq!(
            offsets.len() as u64 - 1,
            table["count"].as_u64().unwrap(),
            "one string offset per feature, plus the end"
        );

        offsets
            .windows(2)
            .map(|pair| String::from_utf8(values[pair[0]..pair[1]].to_vec()).unwrap())
            .collect()
    }

    fn attribute_bytes(&self, attribute: &str) -> &[u8] {
        let accessor = self.json["meshes"][0]["primitives"][0]["attributes"][attribute]
            .as_u64()
            .unwrap() as usize;
        let view = self.json["accessors"][accessor]["bufferView"]
            .as_u64()
            .unwrap() as usize;
        self.buffer_view(view)
    }

    fn feature_ids(&self) -> Vec<usize> {
        self.attribute_bytes("_FEATURE_ID_0")
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk) as usize)
            .collect()
    }

    fn x_and_y_of_vertices(&self) -> Vec<[f64; 2]> {
        let translation = &self.json["nodes"][0]["translation"];
        let translated = |axis: usize, local: &[u8; 4]| {
            translation[axis].as_f64().unwrap() + f64::from(f32::from_le_bytes(*local))
        };
        self.attribute_bytes("POSITION")
            .as_chunks::<12>()
            .0
            .iter()
            .map(|vertex| {
                let (coordinates, _) = vertex.as_chunks::<4>();
                [
                    translated(0, &coordinates[0]),
                    translated(1, &coordinates[1]),
                ]
            })
            .collect()
    }
}

fn box_center(box_index: usize) -> [f64; 2] {
    // twin_boxes.ifc places box n at x = 5 * (n % 4), y = 5 * (n / 4), counting from 0
    [(5 * (box_index % 4)) as f64, (5 * (box_index / 4)) as f64]
}

const BOX_HALF_WIDTH: f64 = 1.0;

fn tile_paths(output: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(output.join("tiles"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    paths
}

/// A flat two-triangle OBJ, a format with no element identity to carry.
fn write_plain_obj(dir: &std::path::Path) -> std::path::PathBuf {
    let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf 1 3 4\n";
    let path = dir.join("plain.obj");
    std::fs::write(&path, obj).unwrap();
    path
}

fn read_and_tile(source: &std::path::Path, output: &std::path::Path) -> Vec<std::path::PathBuf> {
    let meshes: Vec<MeshData> = tiletopia_ingest::read_mesh(source, u64::MAX)
        .expect("the fixture should read")
        .into_iter()
        .map(Into::into)
        .collect();
    let config = MeshTilingConfig {
        max_triangles_per_tile: TRIANGLES_PER_TILE,
        ..Default::default()
    };
    tile_meshes(&meshes, output, &config).expect("tiling should succeed");
    tile_paths(output)
}

#[test]
fn every_ifc_global_id_reaches_a_tile_whose_table_lists_only_its_own_elements() {
    let dir = tempfile::tempdir().unwrap();
    let fixture =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/twin_boxes.ifc");

    let tiles = read_and_tile(&fixture, &dir.path().join("tiles_out"));
    assert!(
        tiles.len() > 1,
        "the fixture should split over several tiles"
    );

    let mut seen = BTreeSet::new();
    for tile in &tiles {
        let glb = read_glb(tile);
        let primitive = &glb.json["meshes"][0]["primitives"][0];
        assert!(
            primitive["attributes"]["_FEATURE_ID_0"].is_number(),
            "{} should carry per-vertex feature ids",
            tile.display()
        );
        let feature_ids = &primitive["extensions"]["EXT_mesh_features"]["featureIds"][0];
        assert_eq!(
            feature_ids["propertyTable"],
            0,
            "{} should point its feature ids at the property table",
            tile.display()
        );

        let used = glb.json["extensionsUsed"].as_array().unwrap();
        assert!(used.iter().any(|name| name == "EXT_mesh_features"));
        assert!(used.iter().any(|name| name == "EXT_structural_metadata"));

        let rows = glb.asset_ids();
        assert_eq!(feature_ids["featureCount"], rows.len());
        let vertex_features = glb.feature_ids();
        let distinct: BTreeSet<usize> = vertex_features.iter().copied().collect();
        assert_eq!(
            distinct,
            (0..rows.len()).collect::<BTreeSet<usize>>(),
            "{} should list exactly the elements its vertices belong to",
            tile.display()
        );
        for (vertex, feature) in glb.x_and_y_of_vertices().iter().zip(&vertex_features) {
            let box_index = BOX_GLOBAL_IDS
                .iter()
                .position(|id| *id == rows[*feature])
                .expect("a GlobalId from the fixture");
            let center = box_center(box_index);
            assert!(
                (vertex[0] - center[0]).abs() < BOX_HALF_WIDTH + 0.01
                    && (vertex[1] - center[1]).abs() < BOX_HALF_WIDTH + 0.01,
                "{}: {vertex:?} is not on {}",
                tile.display(),
                rows[*feature]
            );
        }

        seen.extend(rows);
    }

    let expected: BTreeSet<String> = BOX_GLOBAL_IDS.iter().map(|id| id.to_string()).collect();
    assert_eq!(seen, expected);
}

#[test]
fn an_obj_tileset_carries_neither_extension() {
    let dir = tempfile::tempdir().unwrap();
    let obj = write_plain_obj(dir.path());

    let tiles = read_and_tile(&obj, &dir.path().join("tiles_out"));
    assert!(!tiles.is_empty());

    for tile in &tiles {
        let glb = read_glb(tile);
        assert!(
            glb.json["extensions"]["EXT_structural_metadata"].is_null(),
            "{} should have no metadata extension",
            tile.display()
        );
        assert!(
            glb.json["meshes"][0]["primitives"][0]["extensions"].is_null(),
            "{} should have no primitive extension",
            tile.display()
        );
        assert!(
            glb.json["meshes"][0]["primitives"][0]["attributes"]["_FEATURE_ID_0"].is_null(),
            "{} should have no feature id attribute",
            tile.display()
        );
    }
}
