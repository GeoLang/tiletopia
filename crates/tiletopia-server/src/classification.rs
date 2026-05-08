//! LiDAR auto-classification — ML-powered point cloud segmentation.
//!
//! Classifies point clouds into semantic categories:
//! - Ground, Building, Vegetation (high/low), Water, Road
//! - Power line, Pole, Vehicle, Noise
//!
//! Uses ASPRS LAS classification codes (0–255).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// ASPRS classification codes (LAS 1.4 standard).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PointClass {
    Unclassified = 0,
    Ground = 2,
    LowVegetation = 3,
    MediumVegetation = 4,
    HighVegetation = 5,
    Building = 6,
    Noise = 7,
    Water = 9,
    Rail = 10,
    RoadSurface = 11,
    BridgeDeck = 17,
    HighNoise = 18,
    PowerLine = 14,
    TransmissionTower = 15,
    Pole = 19,
    Vehicle = 64,
    Terrain = 65,
}

impl PointClass {
    pub fn label(&self) -> &str {
        match self {
            Self::Unclassified => "Unclassified",
            Self::Ground => "Ground",
            Self::LowVegetation => "Low Vegetation",
            Self::MediumVegetation => "Medium Vegetation",
            Self::HighVegetation => "High Vegetation",
            Self::Building => "Building",
            Self::Noise => "Noise (Low)",
            Self::Water => "Water",
            Self::Rail => "Rail",
            Self::RoadSurface => "Road Surface",
            Self::BridgeDeck => "Bridge Deck",
            Self::HighNoise => "Noise (High)",
            Self::PowerLine => "Power Line",
            Self::TransmissionTower => "Transmission Tower",
            Self::Pole => "Pole/Lamppost",
            Self::Vehicle => "Vehicle",
            Self::Terrain => "Terrain (other)",
        }
    }

    pub fn color_rgb(&self) -> [u8; 3] {
        match self {
            Self::Unclassified => [200, 200, 200],
            Self::Ground => [139, 90, 43],
            Self::LowVegetation => [144, 238, 144],
            Self::MediumVegetation => [34, 139, 34],
            Self::HighVegetation => [0, 100, 0],
            Self::Building => [255, 69, 0],
            Self::Noise => [255, 0, 255],
            Self::Water => [0, 100, 255],
            Self::Rail => [128, 128, 128],
            Self::RoadSurface => [64, 64, 64],
            Self::BridgeDeck => [160, 82, 45],
            Self::HighNoise => [255, 0, 255],
            Self::PowerLine => [255, 255, 0],
            Self::TransmissionTower => [255, 165, 0],
            Self::Pole => [0, 255, 255],
            Self::Vehicle => [255, 20, 147],
            Self::Terrain => [210, 180, 140],
        }
    }
}

/// Classification model type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelType {
    /// Progressive Morphological Filter (fast, ground-only)
    MorphologicalFilter,
    /// Random Forest classifier (multi-class)
    RandomForest,
    /// PointNet++ deep learning model
    PointNetPP,
    /// RandLA-Net (large-scale point clouds)
    RandLANet,
    /// Custom user-trained model
    Custom(String),
}

/// Classification job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationJob {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub tenant_id: Uuid,
    pub model: ModelType,
    pub status: ClassificationStatus,
    pub progress_percent: u8,
    pub total_points: u64,
    pub classified_points: u64,
    pub class_distribution: Vec<ClassCount>,
    pub confidence_threshold: f32,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub accuracy_metrics: Option<AccuracyMetrics>,
}

/// Count of points per class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassCount {
    pub class: PointClass,
    pub count: u64,
    pub percentage: f32,
}

/// Classification accuracy metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccuracyMetrics {
    pub overall_accuracy: f32,
    pub kappa: f32,
    pub per_class_f1: Vec<(PointClass, f32)>,
    pub confusion_matrix_size: usize,
}

/// Classification status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClassificationStatus {
    Queued,
    Preprocessing,
    Classifying,
    PostProcessing,
    Completed,
    Failed(String),
}

/// Classification engine.
pub struct ClassificationEngine {
    jobs: Arc<RwLock<Vec<ClassificationJob>>>,
}

impl Default for ClassificationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ClassificationEngine {
    pub fn new() -> Self {
        Self {
            jobs: Arc::new(RwLock::new(Self::demo_jobs())),
        }
    }

    /// Create a new classification job.
    pub async fn create_job(
        &self,
        asset_id: Uuid,
        tenant_id: Uuid,
        model: ModelType,
        confidence_threshold: f32,
    ) -> ClassificationJob {
        let job = ClassificationJob {
            id: Uuid::new_v4(),
            asset_id,
            tenant_id,
            model,
            status: ClassificationStatus::Queued,
            progress_percent: 0,
            total_points: 0,
            classified_points: 0,
            class_distribution: Vec::new(),
            confidence_threshold,
            created_at: Utc::now(),
            completed_at: None,
            accuracy_metrics: None,
        };
        self.jobs.write().await.push(job.clone());
        job
    }

    /// List all classification jobs.
    pub async fn list_jobs(&self, tenant_id: Option<Uuid>) -> Vec<ClassificationJob> {
        let jobs = self.jobs.read().await;
        match tenant_id {
            Some(id) => jobs.iter().filter(|j| j.tenant_id == id).cloned().collect(),
            None => jobs.clone(),
        }
    }

    /// Available classification models.
    pub fn available_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                model_type: ModelType::MorphologicalFilter,
                name: "Progressive Morphological Filter".into(),
                description: "Fast ground classification using iterative morphological filtering. Best for terrain extraction.".into(),
                classes: vec![PointClass::Ground, PointClass::Unclassified],
                speed: "Fast",
                accuracy: "Good (ground only)",
            },
            ModelInfo {
                model_type: ModelType::RandomForest,
                name: "Random Forest Multi-Class".into(),
                description: "Traditional ML classifier using geometric features (eigenvalues, planarity, linearity).".into(),
                classes: vec![
                    PointClass::Ground, PointClass::Building, PointClass::HighVegetation,
                    PointClass::LowVegetation, PointClass::Water, PointClass::Noise,
                ],
                speed: "Medium",
                accuracy: "Good (85-90% OA)",
            },
            ModelInfo {
                model_type: ModelType::PointNetPP,
                name: "PointNet++ Deep Learning".into(),
                description: "State-of-the-art deep learning on raw 3D point sets with hierarchical feature learning.".into(),
                classes: vec![
                    PointClass::Ground, PointClass::Building, PointClass::HighVegetation,
                    PointClass::LowVegetation, PointClass::Water, PointClass::PowerLine,
                    PointClass::Pole, PointClass::Vehicle, PointClass::Noise,
                ],
                speed: "Slow (GPU required)",
                accuracy: "Excellent (92-96% OA)",
            },
            ModelInfo {
                model_type: ModelType::RandLANet,
                name: "RandLA-Net (Large-Scale)".into(),
                description: "Efficient deep learning for billion-point clouds using random sampling and local feature aggregation.".into(),
                classes: vec![
                    PointClass::Ground, PointClass::Building, PointClass::HighVegetation,
                    PointClass::LowVegetation, PointClass::Water, PointClass::PowerLine,
                    PointClass::Pole, PointClass::Vehicle, PointClass::RoadSurface,
                    PointClass::Rail, PointClass::Noise,
                ],
                speed: "Medium (GPU recommended)",
                accuracy: "Excellent (93-97% OA)",
            },
        ]
    }

    fn demo_jobs() -> Vec<ClassificationJob> {
        let tenant = Uuid::new_v4();
        vec![
            ClassificationJob {
                id: Uuid::new_v4(),
                asset_id: Uuid::new_v4(),
                tenant_id: tenant,
                model: ModelType::RandLANet,
                status: ClassificationStatus::Completed,
                progress_percent: 100,
                total_points: 187_000_000,
                classified_points: 187_000_000,
                class_distribution: vec![
                    ClassCount {
                        class: PointClass::Ground,
                        count: 78_540_000,
                        percentage: 42.0,
                    },
                    ClassCount {
                        class: PointClass::Building,
                        count: 52_360_000,
                        percentage: 28.0,
                    },
                    ClassCount {
                        class: PointClass::HighVegetation,
                        count: 33_660_000,
                        percentage: 18.0,
                    },
                    ClassCount {
                        class: PointClass::LowVegetation,
                        count: 11_220_000,
                        percentage: 6.0,
                    },
                    ClassCount {
                        class: PointClass::Vehicle,
                        count: 5_610_000,
                        percentage: 3.0,
                    },
                    ClassCount {
                        class: PointClass::PowerLine,
                        count: 3_740_000,
                        percentage: 2.0,
                    },
                    ClassCount {
                        class: PointClass::Noise,
                        count: 1_870_000,
                        percentage: 1.0,
                    },
                ],
                confidence_threshold: 0.85,
                created_at: Utc::now() - chrono::Duration::hours(4),
                completed_at: Some(Utc::now() - chrono::Duration::hours(2)),
                accuracy_metrics: Some(AccuracyMetrics {
                    overall_accuracy: 0.947,
                    kappa: 0.932,
                    per_class_f1: vec![
                        (PointClass::Ground, 0.97),
                        (PointClass::Building, 0.95),
                        (PointClass::HighVegetation, 0.94),
                        (PointClass::Vehicle, 0.88),
                        (PointClass::PowerLine, 0.91),
                    ],
                    confusion_matrix_size: 7,
                }),
            },
            ClassificationJob {
                id: Uuid::new_v4(),
                asset_id: Uuid::new_v4(),
                tenant_id: tenant,
                model: ModelType::MorphologicalFilter,
                status: ClassificationStatus::Classifying,
                progress_percent: 73,
                total_points: 45_000_000,
                classified_points: 32_850_000,
                class_distribution: vec![
                    ClassCount {
                        class: PointClass::Ground,
                        count: 19_710_000,
                        percentage: 60.0,
                    },
                    ClassCount {
                        class: PointClass::Unclassified,
                        count: 13_140_000,
                        percentage: 40.0,
                    },
                ],
                confidence_threshold: 0.9,
                created_at: Utc::now() - chrono::Duration::minutes(30),
                completed_at: None,
                accuracy_metrics: None,
            },
        ]
    }
}

/// Information about an available model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_type: ModelType,
    pub name: String,
    pub description: String,
    pub classes: Vec<PointClass>,
    pub speed: &'static str,
    pub accuracy: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_classification_job() {
        let engine = ClassificationEngine::new();
        let job = engine
            .create_job(Uuid::new_v4(), Uuid::new_v4(), ModelType::PointNetPP, 0.85)
            .await;
        assert_eq!(job.status, ClassificationStatus::Queued);
    }

    #[tokio::test]
    async fn test_demo_jobs() {
        let engine = ClassificationEngine::new();
        let jobs = engine.list_jobs(None).await;
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn test_available_models() {
        let models = ClassificationEngine::available_models();
        assert_eq!(models.len(), 4);
    }

    #[test]
    fn test_point_class_colors() {
        assert_eq!(PointClass::Ground.color_rgb(), [139, 90, 43]);
        assert_eq!(PointClass::Building.color_rgb(), [255, 69, 0]);
    }
}
