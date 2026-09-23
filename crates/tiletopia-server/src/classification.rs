//! LiDAR auto-classification — ML-powered point cloud segmentation.
//!
//! Classifies point clouds into semantic categories:
//! - Ground, Building, Vegetation (high/low), Water, Road
//! - Power line, Pole, Vehicle, Noise
//!
//! Uses ASPRS LAS classification codes (0–255).

use serde::{Deserialize, Serialize};

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

    #[test]
    fn test_available_models() {
        let models = available_models();
        assert_eq!(models.len(), 4);
    }

    #[test]
    fn test_point_class_colors() {
        assert_eq!(PointClass::Ground.color_rgb(), [139, 90, 43]);
        assert_eq!(PointClass::Building.color_rgb(), [255, 69, 0]);
    }
}
