//! Asset version control — git-like diffing for 3D geospatial assets.
//!
//! Track changes between temporal scans, visualize differences,
//! and maintain full revision history with branching.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A versioned asset (analogous to a git repository).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedAsset {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub name: String,
    pub current_version: u32,
    pub branch: String,
    pub versions: Vec<AssetVersion>,
    pub created_at: DateTime<Utc>,
}

/// A single version (analogous to a git commit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetVersion {
    pub version: u32,
    pub commit_hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: DateTime<Utc>,
    pub parent_version: Option<u32>,
    pub stats: VersionStats,
    pub tags: Vec<String>,
}

/// Statistics for a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionStats {
    pub total_points: u64,
    pub points_added: u64,
    pub points_removed: u64,
    pub points_modified: u64,
    pub bounding_box: Option<[[f64; 3]; 2]>,
    pub file_size_bytes: u64,
}

/// A diff between two versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDiff {
    pub from_version: u32,
    pub to_version: u32,
    pub summary: DiffSummary,
    pub change_regions: Vec<ChangeRegion>,
}

/// Summary of changes between versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub points_added: u64,
    pub points_removed: u64,
    pub points_moved: u64,
    pub max_displacement_m: f64,
    pub mean_displacement_m: f64,
    pub affected_area_m2: f64,
    pub change_detected: bool,
}

/// A spatial region with detected changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRegion {
    pub id: Uuid,
    pub center: [f64; 3],
    pub radius_m: f64,
    pub change_type: ChangeType,
    pub magnitude_m: f64,
    pub confidence: f32,
    pub description: String,
}

/// Type of detected change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChangeType {
    /// Material added (construction, accumulation)
    Addition,
    /// Material removed (excavation, erosion)
    Removal,
    /// Displacement/deformation (structural movement)
    Deformation,
    /// Vegetation growth or removal
    VegetationChange,
    /// New structure detected
    NewStructure,
    /// Structure demolished
    Demolition,
}

/// Version control engine.
pub struct VersioningEngine {
    assets: Arc<RwLock<Vec<VersionedAsset>>>,
}

impl Default for VersioningEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl VersioningEngine {
    pub fn new() -> Self {
        Self {
            assets: Arc::new(RwLock::new(Self::demo_data())),
        }
    }

    /// List versioned assets.
    pub async fn list_assets(&self) -> Vec<VersionedAsset> {
        self.assets.read().await.clone()
    }

    /// Get a versioned asset.
    pub async fn get_asset(&self, asset_id: Uuid) -> Option<VersionedAsset> {
        self.assets
            .read()
            .await
            .iter()
            .find(|a| a.asset_id == asset_id)
            .cloned()
    }

    /// Compute diff between two versions.
    pub async fn diff(&self, asset_id: Uuid, from: u32, to: u32) -> Option<VersionDiff> {
        let assets = self.assets.read().await;
        let asset = assets.iter().find(|a| a.asset_id == asset_id)?;

        // Verify both versions exist
        let _from_v = asset.versions.iter().find(|v| v.version == from)?;
        let _to_v = asset.versions.iter().find(|v| v.version == to)?;

        // Simulated diff (in production this would compute C2C distances)
        Some(VersionDiff {
            from_version: from,
            to_version: to,
            summary: DiffSummary {
                points_added: 45_230,
                points_removed: 12_100,
                points_moved: 892_000,
                max_displacement_m: 0.023,
                mean_displacement_m: 0.004,
                affected_area_m2: 156.7,
                change_detected: true,
            },
            change_regions: vec![
                ChangeRegion {
                    id: Uuid::new_v4(),
                    center: [-122.4192, 37.7750, 38.0],
                    radius_m: 2.5,
                    change_type: ChangeType::Deformation,
                    magnitude_m: 0.023,
                    confidence: 0.97,
                    description: "Lateral displacement of support beam B-7".into(),
                },
                ChangeRegion {
                    id: Uuid::new_v4(),
                    center: [-122.4188, 37.7752, 41.0],
                    radius_m: 5.0,
                    change_type: ChangeType::VegetationChange,
                    magnitude_m: 1.2,
                    confidence: 0.89,
                    description: "Vegetation growth near abutment".into(),
                },
            ],
        })
    }

    fn demo_data() -> Vec<VersionedAsset> {
        let asset_id = Uuid::new_v4();
        vec![VersionedAsset {
            id: Uuid::new_v4(),
            asset_id,
            name: "Highway 101 Bridge — Structural Monitoring".into(),
            current_version: 4,
            branch: "main".into(),
            versions: vec![
                AssetVersion {
                    version: 1,
                    commit_hash: "a1b2c3d".into(),
                    message: "Initial baseline scan".into(),
                    author: "Alice Chen".into(),
                    timestamp: Utc::now() - chrono::Duration::days(90),
                    parent_version: None,
                    stats: VersionStats {
                        total_points: 187_000_000,
                        points_added: 187_000_000,
                        points_removed: 0,
                        points_modified: 0,
                        bounding_box: Some([[-122.42, 37.77, 0.0], [-122.41, 37.78, 80.0]]),
                        file_size_bytes: 3_740_000_000,
                    },
                    tags: vec!["baseline".into()],
                },
                AssetVersion {
                    version: 2,
                    commit_hash: "e5f6g7h".into(),
                    message: "Month 1 monitoring scan".into(),
                    author: "Bob Martinez".into(),
                    timestamp: Utc::now() - chrono::Duration::days(60),
                    parent_version: Some(1),
                    stats: VersionStats {
                        total_points: 189_500_000,
                        points_added: 3_200_000,
                        points_removed: 700_000,
                        points_modified: 1_050_000,
                        bounding_box: Some([[-122.42, 37.77, 0.0], [-122.41, 37.78, 80.0]]),
                        file_size_bytes: 3_790_000_000,
                    },
                    tags: vec![],
                },
                AssetVersion {
                    version: 3,
                    commit_hash: "i9j0k1l".into(),
                    message: "Month 2 — detected lateral movement in beam B-7".into(),
                    author: "Bob Martinez".into(),
                    timestamp: Utc::now() - chrono::Duration::days(30),
                    parent_version: Some(2),
                    stats: VersionStats {
                        total_points: 191_200_000,
                        points_added: 2_400_000,
                        points_removed: 500_000,
                        points_modified: 892_000,
                        bounding_box: Some([[-122.42, 37.77, 0.0], [-122.41, 37.78, 80.0]]),
                        file_size_bytes: 3_824_000_000,
                    },
                    tags: vec!["anomaly-detected".into()],
                },
                AssetVersion {
                    version: 4,
                    commit_hash: "m2n3o4p".into(),
                    message: "Month 3 — post-repair verification scan".into(),
                    author: "Carol Park".into(),
                    timestamp: Utc::now() - chrono::Duration::days(2),
                    parent_version: Some(3),
                    stats: VersionStats {
                        total_points: 192_800_000,
                        points_added: 1_900_000,
                        points_removed: 300_000,
                        points_modified: 0,
                        bounding_box: Some([[-122.42, 37.77, 0.0], [-122.41, 37.78, 80.0]]),
                        file_size_bytes: 3_856_000_000,
                    },
                    tags: vec!["repair-verified".into()],
                },
            ],
            created_at: Utc::now() - chrono::Duration::days(90),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_versioned_assets() {
        let engine = VersioningEngine::new();
        let assets = engine.list_assets().await;
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].current_version, 4);
        assert_eq!(assets[0].versions.len(), 4);
    }

    #[tokio::test]
    async fn test_diff_versions() {
        let engine = VersioningEngine::new();
        let assets = engine.list_assets().await;
        let diff = engine.diff(assets[0].asset_id, 2, 3).await.unwrap();
        assert!(diff.summary.change_detected);
        assert_eq!(diff.change_regions.len(), 2);
    }

    #[tokio::test]
    async fn test_diff_nonexistent() {
        let engine = VersioningEngine::new();
        let diff = engine.diff(Uuid::new_v4(), 1, 2).await;
        assert!(diff.is_none());
    }
}
