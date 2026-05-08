//! Temporal versioning for time-series 3D data.
//!
//! Stores multiple versions of a tileset over time, enabling playback
//! and comparison of changes (e.g., construction progress, environmental monitoring).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A temporal version of a tileset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilesetVersion {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub version: u32,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub label: Option<String>,
    /// Path to the tileset.json for this version.
    pub tileset_path: String,
    /// Number of tiles in this version.
    pub tile_count: u64,
    /// Optional diff summary from previous version.
    pub diff_summary: Option<DiffSummary>,
}

/// Summary of changes between versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub tiles_added: u64,
    pub tiles_removed: u64,
    pub tiles_modified: u64,
    pub points_added: u64,
    pub points_removed: u64,
}

/// Temporal version store for an asset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionHistory {
    pub asset_id: Uuid,
    pub versions: Vec<TilesetVersion>,
}

impl VersionHistory {
    pub fn new(asset_id: Uuid) -> Self {
        Self {
            asset_id,
            versions: Vec::new(),
        }
    }

    /// Add a new version.
    pub fn add_version(
        &mut self,
        tileset_path: String,
        tile_count: u64,
        label: Option<String>,
    ) -> &TilesetVersion {
        let version = self.versions.len() as u32 + 1;
        let v = TilesetVersion {
            id: Uuid::new_v4(),
            asset_id: self.asset_id,
            version,
            timestamp: chrono::Utc::now(),
            label,
            tileset_path,
            tile_count,
            diff_summary: None,
        };
        self.versions.push(v);
        self.versions.last().unwrap()
    }

    /// Get the latest version.
    pub fn latest(&self) -> Option<&TilesetVersion> {
        self.versions.last()
    }

    /// Get a specific version by number.
    pub fn get_version(&self, version: u32) -> Option<&TilesetVersion> {
        self.versions.iter().find(|v| v.version == version)
    }

    /// Get version at a specific timestamp (closest before or equal).
    pub fn at_time(&self, time: chrono::DateTime<chrono::Utc>) -> Option<&TilesetVersion> {
        self.versions.iter().rev().find(|v| v.timestamp <= time)
    }

    /// Get all versions in a time range.
    pub fn in_range(
        &self,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Vec<&TilesetVersion> {
        self.versions
            .iter()
            .filter(|v| v.timestamp >= start && v.timestamp <= end)
            .collect()
    }

    /// Get version count.
    pub fn count(&self) -> usize {
        self.versions.len()
    }

    /// Save version history to JSON.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Load version history from JSON.
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_history() {
        let asset_id = Uuid::new_v4();
        let mut history = VersionHistory::new(asset_id);
        history.add_version("v1/tileset.json".into(), 100, Some("Initial".into()));
        history.add_version("v2/tileset.json".into(), 150, Some("Update".into()));
        assert_eq!(history.count(), 2);
        assert_eq!(history.latest().unwrap().version, 2);
        assert_eq!(history.get_version(1).unwrap().tile_count, 100);
    }

    #[test]
    fn test_at_time() {
        let asset_id = Uuid::new_v4();
        let mut history = VersionHistory::new(asset_id);
        history.add_version("v1/tileset.json".into(), 10, None);
        let v = history.at_time(chrono::Utc::now());
        assert!(v.is_some());
        // Before any version
        let old = chrono::Utc::now() - chrono::Duration::days(365);
        let history2 = VersionHistory::new(asset_id);
        assert!(history2.at_time(old).is_none());
    }
}
