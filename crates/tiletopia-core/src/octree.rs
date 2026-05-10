//! Octree spatial partitioning for point cloud tiling.
//!
//! Recursively subdivides 3D space into octants until each leaf node
//! contains fewer than `max_points_per_node` points, or the maximum
//! depth is reached.

use crate::bounding_volume::Aabb;
use rayon::prelude::*;

/// A point with position and attributes, used during octree construction.
#[derive(Debug, Clone, Copy)]
pub struct OctreePoint {
    pub position: [f64; 3],
    pub color: [u8; 3],
    pub intensity: u16,
    pub classification: u8,
}

/// Configuration for octree construction.
#[derive(Debug, Clone)]
pub struct OctreeConfig {
    /// Maximum points in a leaf node before subdivision.
    pub max_points_per_node: usize,
    /// Maximum octree depth (prevents infinite recursion on coincident points).
    pub max_depth: u32,
    /// Minimum node extent (meters) — stop subdividing below this size.
    pub min_extent: f64,
}

impl Default for OctreeConfig {
    fn default() -> Self {
        Self {
            max_points_per_node: 20_000,
            max_depth: 20,
            min_extent: 0.01,
        }
    }
}

/// A node in the octree. Either a leaf containing points, or an internal node with children.
#[derive(Debug)]
pub enum OctreeNode {
    Leaf {
        bounds: Aabb,
        points: Vec<OctreePoint>,
        depth: u32,
    },
    Internal {
        bounds: Aabb,
        children: [Option<Box<OctreeNode>>; 8],
        /// Representative points kept at this level for LOD (decimated subset).
        lod_points: Vec<OctreePoint>,
        depth: u32,
    },
}

impl OctreeNode {
    pub fn bounds(&self) -> &Aabb {
        match self {
            OctreeNode::Leaf { bounds, .. } => bounds,
            OctreeNode::Internal { bounds, .. } => bounds,
        }
    }

    pub fn depth(&self) -> u32 {
        match self {
            OctreeNode::Leaf { depth, .. } => *depth,
            OctreeNode::Internal { depth, .. } => *depth,
        }
    }

    pub fn point_count(&self) -> usize {
        match self {
            OctreeNode::Leaf { points, .. } => points.len(),
            OctreeNode::Internal {
                children,
                lod_points,
                ..
            } => {
                lod_points.len()
                    + children
                        .iter()
                        .filter_map(|c| c.as_ref())
                        .map(|c| c.point_count())
                        .sum::<usize>()
            }
        }
    }
}

/// Build an octree from a set of points.
pub fn build_octree(points: Vec<OctreePoint>, config: &OctreeConfig) -> OctreeNode {
    let mut bounds = Aabb::empty();
    for p in &points {
        bounds.expand_point(p.position);
    }
    build_node(points, bounds, 0, config)
}

fn build_node(
    points: Vec<OctreePoint>,
    bounds: Aabb,
    depth: u32,
    config: &OctreeConfig,
) -> OctreeNode {
    // Leaf conditions: few enough points, max depth, or tiny extent.
    if points.len() <= config.max_points_per_node
        || depth >= config.max_depth
        || bounds.max_extent() < config.min_extent
    {
        return OctreeNode::Leaf {
            bounds,
            points,
            depth,
        };
    }

    let octants = bounds.octants();
    let mut buckets: [Vec<OctreePoint>; 8] = Default::default();

    // Keep every Nth point at this level for LOD.
    let stride = 8.max(points.len() / config.max_points_per_node).min(64);
    let mut lod_points = Vec::with_capacity(points.len() / stride + 1);

    for (i, p) in points.into_iter().enumerate() {
        if i % stride == 0 {
            lod_points.push(p);
        }
        // Find which octant this point belongs to.
        let c = bounds.center();
        let idx = ((p.position[0] >= c[0]) as usize)
            | (((p.position[1] >= c[1]) as usize) << 1)
            | (((p.position[2] >= c[2]) as usize) << 2);
        buckets[idx].push(p);
    }

    // Build children in parallel using rayon.
    let children_vec: Vec<Option<Box<OctreeNode>>> = buckets
        .into_par_iter()
        .enumerate()
        .map(|(i, bucket)| {
            if bucket.is_empty() {
                None
            } else {
                Some(Box::new(build_node(bucket, octants[i], depth + 1, config)))
            }
        })
        .collect();

    let children: [Option<Box<OctreeNode>>; 8] =
        children_vec.try_into().expect("always exactly 8 elements");

    OctreeNode::Internal {
        bounds,
        children,
        lod_points,
        depth,
    }
}

/// Walk the octree and collect statistics.
#[derive(Debug, Default)]
pub struct OctreeStats {
    pub total_nodes: usize,
    pub leaf_nodes: usize,
    pub internal_nodes: usize,
    pub max_depth: u32,
    pub total_points: usize,
}

pub fn collect_stats(node: &OctreeNode) -> OctreeStats {
    let mut stats = OctreeStats::default();
    collect_stats_recursive(node, &mut stats);
    stats
}

fn collect_stats_recursive(node: &OctreeNode, stats: &mut OctreeStats) {
    stats.total_nodes += 1;
    stats.max_depth = stats.max_depth.max(node.depth());
    match node {
        OctreeNode::Leaf { points, .. } => {
            stats.leaf_nodes += 1;
            stats.total_points += points.len();
        }
        OctreeNode::Internal {
            children,
            lod_points,
            ..
        } => {
            stats.internal_nodes += 1;
            stats.total_points += lod_points.len();
            for child in children.iter().flatten() {
                collect_stats_recursive(child, stats);
            }
        }
    }
}
