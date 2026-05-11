//! Diff-based incremental tiling.
//!
//! Detects which regions of a point cloud have changed and only re-tiles those octree nodes.

use crate::bounding_volume::Aabb;
use crate::octree::OctreePoint;
use std::collections::HashMap;
use std::path::Path;

/// Hash of a spatial cell's content for change detection.
pub type CellHash = u64;

/// A spatial grid used for change detection.
#[derive(Debug, Clone)]
pub struct SpatialGrid {
    pub cell_size: f64,
    pub cells: HashMap<(i64, i64, i64), CellHash>,
}

/// Result of comparing two datasets.
#[derive(Debug, Clone)]
pub struct DiffResult {
    /// Cells that are new or have changed.
    pub changed_cells: Vec<(i64, i64, i64)>,
    /// Cells that were removed.
    pub removed_cells: Vec<(i64, i64, i64)>,
    /// Cells that are unchanged.
    pub unchanged_count: usize,
}

impl SpatialGrid {
    /// Build a spatial grid from points.
    pub fn from_points(points: &[OctreePoint], cell_size: f64) -> Self {
        let mut cells: HashMap<(i64, i64, i64), Vec<u64>> = HashMap::new();
        for p in points {
            let cx = (p.position[0] / cell_size).floor() as i64;
            let cy = (p.position[1] / cell_size).floor() as i64;
            let cz = (p.position[2] / cell_size).floor() as i64;
            let key = (cx, cy, cz);
            // Simple hash: combine position bits
            let hash = hash_point(p);
            cells.entry(key).or_default().push(hash);
        }
        let hashed: HashMap<(i64, i64, i64), CellHash> =
            cells.into_iter().map(|(k, v)| (k, hash_vec(&v))).collect();
        Self {
            cell_size,
            cells: hashed,
        }
    }

    /// Compare this grid with a previous snapshot.
    pub fn diff(&self, previous: &SpatialGrid) -> DiffResult {
        let mut changed_cells = Vec::new();
        let mut unchanged_count = 0;

        for (key, hash) in &self.cells {
            match previous.cells.get(key) {
                Some(prev_hash) if *prev_hash == *hash => unchanged_count += 1,
                _ => changed_cells.push(*key),
            }
        }

        let removed_cells: Vec<_> = previous
            .cells
            .keys()
            .filter(|k| !self.cells.contains_key(k))
            .copied()
            .collect();

        DiffResult {
            changed_cells,
            removed_cells,
            unchanged_count,
        }
    }

    /// Get the bounding box of changed cells.
    pub fn changed_bounds(&self, diff: &DiffResult) -> Option<Aabb> {
        if diff.changed_cells.is_empty() {
            return None;
        }
        let mut aabb = Aabb {
            min: [f64::MAX; 3],
            max: [f64::MIN; 3],
        };
        for (cx, cy, cz) in &diff.changed_cells {
            let x = *cx as f64 * self.cell_size;
            let y = *cy as f64 * self.cell_size;
            let z = *cz as f64 * self.cell_size;
            aabb.expand_point([x, y, z]);
            aabb.expand_point([x + self.cell_size, y + self.cell_size, z + self.cell_size]);
        }
        Some(aabb)
    }

    /// Save grid snapshot to disk.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let data: Vec<u8> = self
            .cells
            .iter()
            .flat_map(|((x, y, z), h)| {
                let mut bytes = Vec::with_capacity(32);
                bytes.extend_from_slice(&x.to_le_bytes());
                bytes.extend_from_slice(&y.to_le_bytes());
                bytes.extend_from_slice(&z.to_le_bytes());
                bytes.extend_from_slice(&h.to_le_bytes());
                bytes
            })
            .collect();
        let mut header = (self.cell_size).to_le_bytes().to_vec();
        header.extend_from_slice(&(self.cells.len() as u64).to_le_bytes());
        header.extend(data);
        std::fs::write(path, header)
    }

    /// Load grid snapshot from disk.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let data = std::fs::read(path)?;
        if data.len() < 16 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "too short",
            ));
        }
        let cell_size = f64::from_le_bytes(data[0..8].try_into().unwrap());
        let count = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
        let mut cells = HashMap::with_capacity(count);
        let mut offset = 16;
        for _ in 0..count {
            if offset + 32 > data.len() {
                break;
            }
            let x = i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
            let y = i64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap());
            let z = i64::from_le_bytes(data[offset + 16..offset + 24].try_into().unwrap());
            let h = u64::from_le_bytes(data[offset + 24..offset + 32].try_into().unwrap());
            cells.insert((x, y, z), h);
            offset += 32;
        }
        Ok(Self { cell_size, cells })
    }
}

/// Result of partial re-tiling: only the octree nodes affected by changes.
#[derive(Debug, Clone)]
pub struct PartialRetileResult {
    /// The bounding boxes of octree nodes that need re-tiling.
    pub affected_nodes: Vec<Aabb>,
    /// Points that fall within changed cells (input for octree rebuild).
    pub changed_points: Vec<OctreePoint>,
    /// Number of cells skipped (unchanged).
    pub skipped_cells: usize,
}

/// Identify only the octree nodes that need re-tiling after a point cloud update.
///
/// Compares `new_points` against a previous `snapshot`, using the given
/// `cell_size` for spatial hashing, and returns the affected regions plus
/// only the points in those regions — so the worker can skip unchanged data.
pub fn partial_retile(
    snapshot: &SpatialGrid,
    new_points: &[OctreePoint],
    config: &crate::octree::OctreeConfig,
) -> PartialRetileResult {
    let new_grid = SpatialGrid::from_points(new_points, snapshot.cell_size);
    let diff = new_grid.diff(snapshot);

    let changed_points = filter_changed_points(new_points, &diff, snapshot.cell_size);

    // Build bounding boxes for affected octree nodes by grouping changed cells
    // into regions aligned to the octree's min_extent.
    let node_size = config.min_extent.max(snapshot.cell_size);
    let mut node_set = std::collections::HashSet::new();
    for &(cx, cy, cz) in diff.changed_cells.iter().chain(diff.removed_cells.iter()) {
        let world_x = cx as f64 * snapshot.cell_size;
        let world_y = cy as f64 * snapshot.cell_size;
        let world_z = cz as f64 * snapshot.cell_size;
        let nx = (world_x / node_size).floor() as i64;
        let ny = (world_y / node_size).floor() as i64;
        let nz = (world_z / node_size).floor() as i64;
        node_set.insert((nx, ny, nz));
    }

    let affected_nodes: Vec<Aabb> = node_set
        .into_iter()
        .map(|(nx, ny, nz)| {
            let x = nx as f64 * node_size;
            let y = ny as f64 * node_size;
            let z = nz as f64 * node_size;
            Aabb {
                min: [x, y, z],
                max: [x + node_size, y + node_size, z + node_size],
            }
        })
        .collect();

    PartialRetileResult {
        affected_nodes,
        changed_points,
        skipped_cells: diff.unchanged_count,
    }
}

/// Filter points to only those in changed cells.
pub fn filter_changed_points(
    points: &[OctreePoint],
    diff: &DiffResult,
    cell_size: f64,
) -> Vec<OctreePoint> {
    let changed_set: std::collections::HashSet<_> = diff.changed_cells.iter().copied().collect();
    points
        .iter()
        .filter(|p| {
            let cx = (p.position[0] / cell_size).floor() as i64;
            let cy = (p.position[1] / cell_size).floor() as i64;
            let cz = (p.position[2] / cell_size).floor() as i64;
            changed_set.contains(&(cx, cy, cz))
        })
        .cloned()
        .collect()
}

fn hash_point(p: &OctreePoint) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &f in &p.position {
        h ^= f.to_bits();
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn hash_vec(v: &[u64]) -> CellHash {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &val in v {
        h ^= val;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_changes() {
        let points: Vec<OctreePoint> = (0..100)
            .map(|i| OctreePoint {
                position: [i as f64, 0.0, 0.0],
                color: [0, 0, 0],
                intensity: 0,
                classification: 0,
            })
            .collect();
        let grid = SpatialGrid::from_points(&points, 10.0);
        let diff = grid.diff(&grid);
        assert!(diff.changed_cells.is_empty());
        assert_eq!(diff.unchanged_count, grid.cells.len());
    }

    #[test]
    fn test_detect_changes() {
        let points1: Vec<OctreePoint> = (0..10)
            .map(|i| OctreePoint {
                position: [i as f64, 0.0, 0.0],
                color: [0, 0, 0],
                intensity: 0,
                classification: 0,
            })
            .collect();
        let mut points2 = points1.clone();
        points2.push(OctreePoint {
            position: [50.0, 50.0, 50.0],
            color: [255, 0, 0],
            intensity: 100,
            classification: 2,
        });
        let grid1 = SpatialGrid::from_points(&points1, 10.0);
        let grid2 = SpatialGrid::from_points(&points2, 10.0);
        let diff = grid2.diff(&grid1);
        assert!(!diff.changed_cells.is_empty());
    }

    #[test]
    fn test_save_load_roundtrip() {
        let points: Vec<OctreePoint> = (0..50)
            .map(|i| OctreePoint {
                position: [i as f64, (i * 2) as f64, 0.0],
                color: [0, 0, 0],
                intensity: 0,
                classification: 0,
            })
            .collect();
        let grid = SpatialGrid::from_points(&points, 5.0);
        let tmp = std::env::temp_dir().join("tiletopia_test_grid.bin");
        grid.save(&tmp).unwrap();
        let loaded = SpatialGrid::load(&tmp).unwrap();
        assert_eq!(loaded.cells.len(), grid.cells.len());
        let _ = std::fs::remove_file(&tmp);
    }
}
