//! A textured OBJ from disk, through the readers, to 3D Tiles GLB content.

use tiletopia_core::mesh_tiler::{MeshData, MeshTilingConfig, tile_meshes};

/// Side of the grid the test OBJ holds, in quads.
const GRID: usize = 16;
const TEXTURE_SIZE: u32 = 64;

fn write_textured_obj(dir: &std::path::Path) -> std::path::PathBuf {
    let image = image::RgbaImage::from_fn(TEXTURE_SIZE, TEXTURE_SIZE, |x, y| {
        image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
    });
    image.save(dir.join("ground.png")).unwrap();
    std::fs::write(
        dir.join("ground.mtl"),
        "newmtl ground\nKd 0.9 0.9 0.9\nmap_Kd ground.png\n",
    )
    .unwrap();

    let mut obj = String::from("mtllib ground.mtl\nusemtl ground\n");
    for y in 0..=GRID {
        for x in 0..=GRID {
            obj.push_str(&format!("v {x} {y} 0\n"));
            obj.push_str(&format!(
                "vt {} {}\n",
                x as f32 / GRID as f32,
                y as f32 / GRID as f32
            ));
        }
    }
    let stride = GRID + 1;
    for y in 0..GRID {
        for x in 0..GRID {
            // OBJ indices count from 1
            let bottom_left = y * stride + x + 1;
            let bottom_right = bottom_left + 1;
            let top_left = bottom_left + stride;
            let top_right = top_left + 1;
            obj.push_str(&format!(
                "f {bottom_left}/{bottom_left} {bottom_right}/{bottom_right} {top_left}/{top_left}\n"
            ));
            obj.push_str(&format!(
                "f {bottom_right}/{bottom_right} {top_right}/{top_right} {top_left}/{top_left}\n"
            ));
        }
    }

    let path = dir.join("ground.obj");
    std::fs::write(&path, obj).unwrap();
    path
}

/// The image bytes the GLB's material points at, and its UV accessor count.
fn material_texture(path: &std::path::Path) -> (Vec<u8>, u64) {
    let bytes = std::fs::read(path).expect("a written tile");
    let glb = gltf::Glb::from_slice(&bytes).expect("a parseable GLB");
    let json: serde_json::Value = serde_json::from_slice(&glb.json).unwrap();
    let bin = glb.bin.expect("a binary chunk");

    assert_eq!(
        json["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["index"],
        0,
        "{} should paint itself with the texture",
        path.display()
    );
    let source = json["textures"][0]["source"].as_u64().unwrap() as usize;
    let view =
        &json["bufferViews"][json["images"][source]["bufferView"].as_u64().unwrap() as usize];
    let offset = view["byteOffset"].as_u64().unwrap_or(0) as usize;
    let length = view["byteLength"].as_u64().unwrap() as usize;

    let uv_accessor = json["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_0"]
        .as_u64()
        .expect("a TEXCOORD_0 attribute");
    let uv_count = json["accessors"][uv_accessor as usize]["count"]
        .as_u64()
        .unwrap();

    (bin[offset..offset + length].to_vec(), uv_count)
}

#[test]
fn a_textured_obj_tiles_into_textured_glbs() {
    let dir = tempfile::tempdir().unwrap();
    let obj = write_textured_obj(dir.path());

    let meshes: Vec<MeshData> = tiletopia_ingest::read_mesh(&obj)
        .expect("the OBJ should read")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(meshes.len(), 1);
    assert!(
        meshes[0].texture.is_some(),
        "the mesh should carry its texture"
    );

    let output = dir.path().join("tiles_out");
    let config = MeshTilingConfig {
        max_triangles_per_tile: 64,
        ..Default::default()
    };
    let stats = tile_meshes(&meshes, &output, &config).expect("tiling should succeed");
    assert!(stats.tile_count > 1, "the mesh should have been split");

    let tiles: Vec<std::path::PathBuf> = std::fs::read_dir(output.join("tiles"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(tiles.len() > 1);

    let mut cropped = 0;
    for tile in &tiles {
        let (image_bytes, uv_count) = material_texture(tile);
        let image = image::load_from_memory(&image_bytes).expect("a decodable tile texture");
        assert!(image.width() <= TEXTURE_SIZE && image.height() <= TEXTURE_SIZE);
        if image.width() < TEXTURE_SIZE || image.height() < TEXTURE_SIZE {
            cropped += 1;
        }
        assert_eq!(
            uv_count as usize,
            meshes[0].positions.len(),
            "every vertex keeps a UV"
        );
    }
    assert!(cropped > 0, "split tiles should carry a cropped texture");
}
