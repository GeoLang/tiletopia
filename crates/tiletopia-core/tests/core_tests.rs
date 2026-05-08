#[cfg(test)]
mod tests {
    use tiletopia_core::bounding_volume::Aabb;
    use tiletopia_core::lod::generate_tileset;
    use tiletopia_core::octree::{
        OctreeConfig, OctreeNode, OctreePoint, build_octree, collect_stats,
    };
    use tiletopia_core::tile::write_pnts;
    use tiletopia_core::{BoundingVolume, Refine, Tile, TileContent, Tileset, TilesetAsset};

    fn make_point(x: f64, y: f64, z: f64) -> OctreePoint {
        OctreePoint {
            position: [x, y, z],
            color: [255, 128, 0],
            intensity: 100,
            classification: 2,
        }
    }

    #[test]
    fn aabb_expand_and_center() {
        let mut aabb = Aabb::empty();
        aabb.expand_point([0.0, 0.0, 0.0]);
        aabb.expand_point([10.0, 20.0, 30.0]);
        assert_eq!(aabb.center(), [5.0, 10.0, 15.0]);
        assert_eq!(aabb.half_extents(), [5.0, 10.0, 15.0]);
    }

    #[test]
    fn aabb_contains_point() {
        let aabb = Aabb::new([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]);
        assert!(aabb.contains_point([5.0, 5.0, 5.0]));
        assert!(!aabb.contains_point([11.0, 5.0, 5.0]));
    }

    #[test]
    fn aabb_octants_count() {
        let aabb = Aabb::new([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]);
        let octants = aabb.octants();
        assert_eq!(octants.len(), 8);
        // Each octant should be 5×5×5
        for o in &octants {
            let h = o.half_extents();
            assert!((h[0] - 2.5).abs() < 1e-10);
        }
    }

    #[test]
    fn aabb_to_3dtiles_box() {
        let aabb = Aabb::new([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]);
        let b = aabb.to_3dtiles_box();
        assert_eq!(b[0], 5.0); // center x
        assert_eq!(b[3], 5.0); // half-extent x
        assert_eq!(b[7], 5.0); // half-extent y
    }

    #[test]
    fn octree_leaf_small_input() {
        let points: Vec<OctreePoint> = (0..100).map(|i| make_point(i as f64, 0.0, 0.0)).collect();
        let config = OctreeConfig {
            max_points_per_node: 200,
            ..Default::default()
        };
        let root = build_octree(points, &config);
        assert!(matches!(root, OctreeNode::Leaf { .. }));
        assert_eq!(root.point_count(), 100);
    }

    #[test]
    fn octree_subdivides() {
        let points: Vec<OctreePoint> = (0..10_000)
            .map(|i| {
                let x = (i % 100) as f64;
                let y = (i / 100) as f64;
                make_point(x, y, 0.0)
            })
            .collect();
        let config = OctreeConfig {
            max_points_per_node: 1_000,
            max_depth: 10,
            min_extent: 0.01,
        };
        let root = build_octree(points, &config);
        let stats = collect_stats(&root);
        assert!(stats.internal_nodes > 0);
        assert!(stats.leaf_nodes > 1);
        assert_eq!(stats.total_nodes, stats.internal_nodes + stats.leaf_nodes);
    }

    #[test]
    fn tileset_json_serialization() {
        let tileset = Tileset {
            asset: TilesetAsset::default(),
            geometric_error: 100.0,
            root: Tile {
                bounding_volume: BoundingVolume::Sphere {
                    sphere: [0.0, 0.0, 0.0, 1000.0],
                },
                geometric_error: 50.0,
                content: Some(TileContent {
                    uri: "tiles/root.pnts".into(),
                }),
                children: vec![],
                refine: Some(Refine::Add),
                transform: None,
            },
        };
        let json = serde_json::to_string(&tileset).unwrap();
        assert!(json.contains("\"geometricError\""));
        assert!(json.contains("\"version\":\"1.1\""));
    }

    #[test]
    fn tileset_json_deserialization() {
        let json = r#"{
            "asset": {"version": "1.1"},
            "geometricError": 100.0,
            "root": {
                "boundingVolume": {"type": "Sphere", "sphere": [0,0,0,1000]},
                "geometricError": 50.0,
                "children": []
            }
        }"#;
        let tileset: Tileset = serde_json::from_str(json).unwrap();
        assert_eq!(tileset.asset.version, "1.1");
        assert_eq!(tileset.geometric_error, 100.0);
    }

    #[test]
    fn generate_tileset_from_octree() {
        let points: Vec<OctreePoint> = (0..500)
            .map(|i| make_point(i as f64 * 0.1, 0.0, 0.0))
            .collect();
        let config = OctreeConfig {
            max_points_per_node: 100,
            ..Default::default()
        };
        let root = build_octree(points, &config);
        let tileset = generate_tileset(&root, "tiles/");
        assert_eq!(tileset.asset.version, "1.1");
        assert!(tileset.geometric_error > 0.0);
        assert!(!tileset.root.children.is_empty());
    }

    #[test]
    fn write_pnts_format() {
        let points = vec![make_point(1.0, 2.0, 3.0), make_point(4.0, 5.0, 6.0)];
        let mut buf = Vec::new();
        write_pnts(&points, &mut buf).unwrap();
        // Check magic bytes
        assert_eq!(&buf[0..4], b"pnts");
        // Check version
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 1);
        // Check total size matches buffer
        let total_size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        assert_eq!(total_size as usize, buf.len());
    }

    #[test]
    fn spatial_deg_to_rad() {
        use tiletopia_core::spatial::{deg_to_rad, rad_to_deg};
        let rad = deg_to_rad(180.0);
        assert!((rad - std::f64::consts::PI).abs() < 1e-10);
        let deg = rad_to_deg(std::f64::consts::PI);
        assert!((deg - 180.0).abs() < 1e-10);
    }

    #[test]
    fn geodetic_to_ecef_equator() {
        use tiletopia_core::spatial::geodetic_to_ecef;
        let [x, y, z] = geodetic_to_ecef(0.0, 0.0, 0.0);
        // At equator, prime meridian, height 0: x ≈ WGS84_A, y ≈ 0, z ≈ 0
        assert!((x - 6_378_137.0).abs() < 1.0);
        assert!(y.abs() < 1.0);
        assert!(z.abs() < 1.0);
    }

    #[test]
    fn tileset_write_to_dir() {
        let points: Vec<OctreePoint> = (0..200)
            .map(|i| make_point(i as f64, (i % 10) as f64, 0.0))
            .collect();
        let config = tiletopia_core::tileset::TilingConfig {
            octree: OctreeConfig {
                max_points_per_node: 50,
                ..Default::default()
            },
            ..Default::default()
        };
        let dir = std::env::temp_dir().join(format!("tiletopia_test_{}", std::process::id()));
        let stats = tiletopia_core::tileset::tile_point_cloud(points, &dir, &config).unwrap();
        assert!(stats.total_nodes > 0);
        assert!(dir.join("tileset.json").exists());
        assert!(dir.join("tiles").exists());
        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
