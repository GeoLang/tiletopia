//! Model Zoo — pre-trained model catalog with download support.
//!
//! Provides a registry of ready-to-use models for common point cloud tasks.
//! Models can be downloaded on demand and registered with the model registry.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A model available in the zoo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZooModel {
    pub id: String,
    pub name: String,
    pub description: String,
    pub task: String,
    pub architecture: String,
    pub format: String,
    pub download_url: String,
    pub file_size_mb: f64,
    pub accuracy: f64,
    pub mean_iou: f64,
    pub training_dataset: String,
    pub num_classes: usize,
    pub class_labels: Vec<String>,
    pub citation: Option<String>,
}

/// Pre-defined model zoo catalog.
pub fn catalog() -> Vec<ZooModel> {
    vec![
        ZooModel {
            id: "pointnet-urban-v1".into(),
            name: "PointNet Urban Classifier".into(),
            description: "General-purpose urban LiDAR classifier trained on DALES dataset. \
                Classifies ground, buildings, vegetation, vehicles, poles, power lines, and fences."
                .into(),
            task: "point_classification".into(),
            architecture: "PointNet".into(),
            format: "onnx".into(),
            download_url: "https://models.tiletopia.dev/zoo/pointnet-urban-v1.onnx".into(),
            file_size_mb: 12.4,
            accuracy: 0.89,
            mean_iou: 0.72,
            training_dataset: "DALES (aerial LiDAR, urban)".into(),
            num_classes: 11,
            class_labels: vec![
                "ground".into(), "vegetation".into(), "car".into(), "truck".into(),
                "power_line".into(), "pole".into(), "fence".into(), "building".into(),
                "unclassified".into(), "water".into(), "road".into(),
            ],
            citation: Some("Varney et al. DALES: A Large-scale Aerial LiDAR Data Set for Semantic Segmentation (2020)".into()),
        },
        ZooModel {
            id: "pointnet-forest-v1".into(),
            name: "PointNet Forest Classifier".into(),
            description: "Forestry LiDAR classifier for tree species, canopy, understory, and ground separation."
                .into(),
            task: "point_classification".into(),
            architecture: "PointNet".into(),
            format: "onnx".into(),
            download_url: "https://models.tiletopia.dev/zoo/pointnet-forest-v1.onnx".into(),
            file_size_mb: 11.8,
            accuracy: 0.91,
            mean_iou: 0.76,
            training_dataset: "FOR-instance (forestry ALS)".into(),
            num_classes: 6,
            class_labels: vec![
                "ground".into(), "low_vegetation".into(), "medium_vegetation".into(),
                "high_vegetation".into(), "trunk".into(), "noise".into(),
            ],
            citation: None,
        },
        ZooModel {
            id: "pointnet-coastal-v1".into(),
            name: "PointNet Coastal Classifier".into(),
            description: "Coastal/bathymetric LiDAR classifier for water, sand, rock, vegetation, and structures."
                .into(),
            task: "point_classification".into(),
            architecture: "PointNet".into(),
            format: "onnx".into(),
            download_url: "https://models.tiletopia.dev/zoo/pointnet-coastal-v1.onnx".into(),
            file_size_mb: 10.2,
            accuracy: 0.87,
            mean_iou: 0.69,
            training_dataset: "Coastal survey dataset".into(),
            num_classes: 7,
            class_labels: vec![
                "ground".into(), "water".into(), "sand".into(), "rock".into(),
                "vegetation".into(), "building".into(), "noise".into(),
            ],
            citation: None,
        },
        ZooModel {
            id: "randlanet-large-v1".into(),
            name: "RandLA-Net Large Scale".into(),
            description: "Efficient large-scale point cloud segmentation using random sampling \
                and local feature aggregation. Best for datasets >100M points."
                .into(),
            task: "point_classification".into(),
            architecture: "RandLA-Net".into(),
            format: "onnx".into(),
            download_url: "https://models.tiletopia.dev/zoo/randlanet-large-v1.onnx".into(),
            file_size_mb: 45.6,
            accuracy: 0.93,
            mean_iou: 0.81,
            training_dataset: "Semantic3D + S3DIS".into(),
            num_classes: 11,
            class_labels: vec![
                "ground".into(), "building".into(), "low_vegetation".into(),
                "medium_vegetation".into(), "high_vegetation".into(), "vehicle".into(),
                "hardscape".into(), "artifact".into(), "water".into(), "road".into(),
                "noise".into(),
            ],
            citation: Some("Hu et al. RandLA-Net: Efficient Semantic Segmentation of Large-Scale Point Clouds (2020)".into()),
        },
        ZooModel {
            id: "anomaly-deformation-v1".into(),
            name: "Deformation Detector".into(),
            description: "Detects structural deformation and subsidence by comparing temporal point cloud epochs."
                .into(),
            task: "anomaly_detection".into(),
            architecture: "PointNet+ICP".into(),
            format: "onnx".into(),
            download_url: "https://models.tiletopia.dev/zoo/anomaly-deformation-v1.onnx".into(),
            file_size_mb: 8.3,
            accuracy: 0.94,
            mean_iou: 0.0,
            training_dataset: "Synthetic deformation dataset".into(),
            num_classes: 3,
            class_labels: vec!["stable".into(), "subsidence".into(), "uplift".into()],
            citation: None,
        },
    ]
}

/// Download a model from the zoo to local storage.
///
/// Returns the local file path.
pub async fn download_model(
    model: &ZooModel,
    models_dir: impl AsRef<Path>,
) -> Result<PathBuf, String> {
    let dir = models_dir.as_ref();
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create models dir: {e}"))?;

    let filename = format!("{}.onnx", model.id);
    let dest = dir.join(&filename);

    if dest.exists() {
        return Ok(dest);
    }

    #[cfg(feature = "ml")]
    {
        let response = reqwest::get(&model.download_url)
            .await
            .map_err(|e| format!("Download failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!("Download failed: HTTP {}", response.status()));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response: {e}"))?;

        std::fs::write(&dest, &bytes).map_err(|e| format!("Failed to write model: {e}"))?;
        Ok(dest)
    }

    #[cfg(not(feature = "ml"))]
    {
        Err("Enable the `ml` feature to download models (requires reqwest)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_not_empty() {
        let models = catalog();
        assert!(!models.is_empty());
        assert!(models.len() >= 5);
    }

    #[test]
    fn test_catalog_models_have_required_fields() {
        for model in catalog() {
            assert!(!model.id.is_empty());
            assert!(!model.name.is_empty());
            assert!(!model.download_url.is_empty());
            assert!(model.num_classes > 0);
            assert_eq!(model.class_labels.len(), model.num_classes);
            assert!(model.accuracy > 0.0 && model.accuracy <= 1.0);
        }
    }

    #[test]
    fn test_catalog_unique_ids() {
        let models = catalog();
        let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), models.len());
    }
}
