//! Stress tests for the tiling pipeline.
//!
//! These tests exercise the octree builder, tile writer, and tileset generation
//! with progressively larger synthetic point clouds to validate correctness and
//! measure throughput at scale.
//!
//! Run with: cargo test --release -p tiletopia-core stress -- --nocapture

use std::time::Instant;
use tiletopia_core::octree::{OctreeConfig, OctreePoint, build_octree, collect_stats};
use tiletopia_core::tile::write_tileset_to_dir;
use tiletopia_core::tileset::TilingConfig;

fn make_random_points(n: usize, spread: f64) -> Vec<OctreePoint> {
    // Deterministic pseudo-random using simple LCG for reproducibility
    let mut rng_state: u64 = 42;
    let mut next = || -> f64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((rng_state >> 33) as f64) / (u32::MAX as f64) * spread
    };

    (0..n)
        .map(|_| OctreePoint {
            position: [next(), next(), next()],
            color: [(next() * 255.0 / spread) as u8, 128, 64],
            intensity: (next() * 1000.0 / spread) as u16,
            classification: ((next() * 10.0 / spread) as u8).min(9),
        })
        .collect()
}

fn make_clustered_points(n: usize, num_clusters: usize, spread: f64) -> Vec<OctreePoint> {
    let mut rng_state: u64 = 123;
    let mut next = || -> f64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((rng_state >> 33) as f64) / (u32::MAX as f64)
    };

    // Generate cluster centers
    let centers: Vec<[f64; 3]> = (0..num_clusters)
        .map(|_| [next() * spread, next() * spread, next() * spread * 0.1])
        .collect();

    (0..n)
        .map(|i| {
            let c = &centers[i % num_clusters];
            let jitter = 5.0; // cluster radius
            OctreePoint {
                position: [
                    c[0] + (next() - 0.5) * jitter,
                    c[1] + (next() - 0.5) * jitter,
                    c[2] + (next() - 0.5) * jitter,
                ],
                color: [200, 180, 160],
                intensity: 500,
                classification: 2,
            }
        })
        .collect()
}

#[test]
fn stress_100k_uniform() {
    let n = 100_000;
    let points = make_random_points(n, 1000.0);
    let config = OctreeConfig::default();

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 100K uniform points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);
    println!("  Internal nodes: {}", stats.internal_nodes);
    println!("  Max depth: {}", stats.max_depth);
    println!("  Points preserved: {}", stats.total_points);

    // total_points includes LOD copies at internal nodes, so it exceeds input count
    assert!(
        stats.total_points >= n,
        "all points must be preserved in octree"
    );
    assert!(
        stats.total_points < n * 2,
        "LOD overhead should be < 2x: got {}",
        stats.total_points
    );
    assert!(stats.leaf_nodes > 1, "should have multiple leaf nodes");
    assert!(
        elapsed.as_secs() < 10,
        "100K points should tile in under 10s"
    );
}

#[test]
fn stress_1m_uniform() {
    let n = 1_000_000;
    let points = make_random_points(n, 5000.0);
    let config = OctreeConfig {
        max_points_per_node: 20_000,
        max_depth: 20,
        min_extent: 0.01,
    };

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 1M uniform points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);
    println!("  Internal nodes: {}", stats.internal_nodes);
    println!("  Max depth: {}", stats.max_depth);
    println!("  Points preserved: {}", stats.total_points);
    println!(
        "  Throughput: {:.1}M pts/sec",
        n as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    assert!(stats.total_points >= n);
    assert!(stats.total_points < n * 2, "LOD overhead < 2x");
    assert!(elapsed.as_secs() < 30, "1M points should tile in under 30s");
}

#[test]
fn stress_5m_uniform() {
    let n = 5_000_000;
    let points = make_random_points(n, 10000.0);
    let config = OctreeConfig::default();

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 5M uniform points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);
    println!("  Max depth: {}", stats.max_depth);
    println!(
        "  Throughput: {:.1}M pts/sec",
        n as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    assert!(stats.total_points >= n);
    assert!(stats.total_points < n * 2, "LOD overhead < 2x");
    assert!(
        elapsed.as_secs() < 120,
        "5M points should tile in under 2min"
    );
}

#[test]
fn stress_10m_uniform() {
    let n = 10_000_000;
    let points = make_random_points(n, 20000.0);
    let config = OctreeConfig::default();

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 10M uniform points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);
    println!("  Max depth: {}", stats.max_depth);
    println!(
        "  Throughput: {:.1}M pts/sec",
        n as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    assert!(stats.total_points >= n);
    assert!(stats.total_points < n * 2, "LOD overhead < 2x");
    assert!(
        elapsed.as_secs() < 300,
        "10M points should tile in under 5min"
    );
}

#[test]
fn stress_1m_clustered() {
    let n = 1_000_000;
    let points = make_clustered_points(n, 50, 5000.0);
    let config = OctreeConfig::default();

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 1M clustered points (50 clusters) ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);
    println!("  Max depth: {}", stats.max_depth);
    println!(
        "  Throughput: {:.1}M pts/sec",
        n as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    assert!(stats.total_points >= n);
}

#[test]
fn stress_full_pipeline_1m() {
    let n = 1_000_000;
    let points = make_random_points(n, 5000.0);
    let config = TilingConfig {
        octree: OctreeConfig {
            max_points_per_node: 20_000,
            ..Default::default()
        },
        max_geometric_error: 200.0,
    };

    let dir = std::env::temp_dir().join(format!("tiletopia_stress_{}", std::process::id()));

    let t0 = Instant::now();
    let stats = tiletopia_core::tileset::tile_point_cloud(points, &dir, &config).unwrap();
    let elapsed = t0.elapsed();

    println!("--- Full pipeline: 1M points → tileset.json + .pnts tiles ---");
    println!("  Total time: {:?}", elapsed);
    println!("  Nodes written: {}", stats.total_nodes);
    println!("  Leaf tiles: {}", stats.leaf_nodes);
    println!(
        "  Throughput: {:.1}M pts/sec",
        n as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );

    // Verify output
    let tileset_path = dir.join("tileset.json");
    assert!(tileset_path.exists(), "tileset.json must exist");

    let tileset_json = std::fs::read_to_string(&tileset_path).unwrap();
    let tileset: tiletopia_core::Tileset = serde_json::from_str(&tileset_json).unwrap();
    assert_eq!(tileset.asset.version, "1.1");
    assert!(tileset.geometric_error > 0.0);

    // Count .pnts files
    let tiles_dir = dir.join("tiles");
    let pnts_count = std::fs::read_dir(&tiles_dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .map(|e| e.path().extension().is_some_and(|ext| ext == "pnts"))
                .unwrap_or(false)
        })
        .count();
    println!("  .pnts files written: {}", pnts_count);
    assert!(pnts_count > 0, "must write at least one .pnts tile");

    // Calculate total output size
    let total_bytes: u64 = walkdir(&dir);
    println!(
        "  Total output size: {:.1} MB",
        total_bytes as f64 / 1_048_576.0
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

fn walkdir(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += walkdir(&entry.path());
        }
    }
    total
}

#[test]
fn stress_degenerate_coincident_points() {
    // All points at the same position — tests max_depth safeguard
    let n = 50_000;
    let points: Vec<OctreePoint> = (0..n)
        .map(|_| OctreePoint {
            position: [100.0, 200.0, 50.0],
            color: [255, 0, 0],
            intensity: 500,
            classification: 6,
        })
        .collect();

    let config = OctreeConfig {
        max_points_per_node: 1_000,
        max_depth: 10,
        min_extent: 0.001,
    };

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 50K coincident points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Max depth: {}", stats.max_depth);
    println!("  Total points: {}", stats.total_points);

    // Should not stack overflow or hang
    assert!(stats.total_points >= n);
    assert!(
        stats.max_depth <= config.max_depth,
        "depth must respect max_depth"
    );
    assert!(elapsed.as_secs() < 10, "coincident points should not hang");
}

#[test]
fn stress_thin_line_distribution() {
    // Points along a single line — worst case for octree balance
    let n = 500_000;
    let points: Vec<OctreePoint> = (0..n)
        .map(|i| OctreePoint {
            position: [i as f64 * 0.01, 0.0, 0.0],
            color: [0, 255, 0],
            intensity: 100,
            classification: 2,
        })
        .collect();

    let config = OctreeConfig::default();

    let t0 = Instant::now();
    let root = build_octree(points, &config);
    let elapsed = t0.elapsed();

    let stats = collect_stats(&root);
    println!("--- 500K linear points ---");
    println!("  Build time: {:?}", elapsed);
    println!("  Max depth: {}", stats.max_depth);
    println!("  Total nodes: {}", stats.total_nodes);
    println!("  Leaf nodes: {}", stats.leaf_nodes);

    assert!(stats.total_points >= n);
    assert!(elapsed.as_secs() < 30);
}
