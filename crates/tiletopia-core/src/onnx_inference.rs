//! ONNX Runtime inference for point cloud classification.
//!
//! Runs ONNX models directly in Rust — no Python sidecar needed.
//! Supports any ONNX model exported from PyTorch, TensorFlow, or scikit-learn.
//!
//! Enable with `--features tiletopia-core/onnx`.

#[cfg(feature = "onnx")]
pub mod runtime {
    use ort::session::Session;
    use ort::value::Tensor;
    use std::path::Path;

    use crate::classify::{Classification, PointFeatures};

    /// ONNX model session wrapper.
    pub struct OnnxClassifier {
        session: Session,
        class_map: Vec<Classification>,
    }

    impl OnnxClassifier {
        /// Load an ONNX model from disk.
        ///
        /// The model should accept input shape `(batch, 8)` with features:
        /// `[height_above_ground, planarity, linearity, scatter, density, elevation, return_number, normal_z]`
        ///
        /// Output shape should be `(batch, num_classes)` with logits or probabilities.
        pub fn load(model_path: impl AsRef<Path>) -> Result<Self, ort::Error> {
            let session = Session::builder()?
                .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
                .commit_from_file(model_path)?;

            // Default ASPRS class mapping (index → Classification)
            let class_map = vec![
                Classification::Unclassified,
                Classification::Ground,
                Classification::LowVegetation,
                Classification::MediumVegetation,
                Classification::HighVegetation,
                Classification::Building,
                Classification::Noise,
                Classification::Water,
                Classification::Road,
                Classification::PowerLine,
                Classification::Bridge,
            ];

            Ok(Self { session, class_map })
        }

        /// Classify a batch of points.
        ///
        /// Returns `(classification, confidence)` for each point.
        pub fn classify_batch(
            &mut self,
            features: &[PointFeatures],
        ) -> Result<Vec<(Classification, f32)>, ort::Error> {
            let n = features.len();
            if n == 0 {
                return Ok(vec![]);
            }

            // Build input as a flat row-major (n, 8) buffer
            let mut input = vec![0f32; n * 8];
            for (i, f) in features.iter().enumerate() {
                let row = &mut input[i * 8..(i + 1) * 8];
                row[0] = f.height_above_ground as f32;
                row[1] = f.planarity as f32;
                row[2] = f.linearity as f32;
                row[3] = f.scatter as f32;
                row[4] = f.density as f32;
                row[5] = f.elevation as f32;
                row[6] = f.return_number as f32;
                row[7] = f.normal_z as f32;
            }

            let input_value = Tensor::from_array(([n, 8], input))?;
            let outputs = self.session.run(ort::inputs![input_value])?;

            // Extract output logits as (shape, flat data)
            let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
            let num_classes = shape.get(1).copied().unwrap_or(1) as usize;

            let mut results = Vec::with_capacity(n);
            for i in 0..n {
                let row = &data[i * num_classes..(i + 1) * num_classes];
                let (max_idx, max_val) = row
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
                    .unwrap_or((0, &0.0));

                // Softmax-style confidence
                let sum_exp: f32 = row.iter().map(|v| v.exp()).sum();
                let confidence = max_val.exp() / sum_exp;

                let class = self
                    .class_map
                    .get(max_idx)
                    .copied()
                    .unwrap_or(Classification::Unclassified);

                results.push((class, confidence));
            }

            Ok(results)
        }

        /// Classify a single point.
        pub fn classify_one(
            &mut self,
            features: &PointFeatures,
        ) -> Result<(Classification, f32), ort::Error> {
            let results = self.classify_batch(std::slice::from_ref(features))?;
            Ok(results
                .into_iter()
                .next()
                .unwrap_or((Classification::Unclassified, 0.0)))
        }
    }
}

#[cfg(feature = "onnx")]
pub use runtime::OnnxClassifier;

#[cfg(test)]
#[cfg(feature = "onnx")]
mod tests {
    use super::*;

    #[test]
    fn test_onnx_module_compiles() {
        // Just verify the module compiles — actual model tests require an ONNX file
        let _ = std::mem::size_of::<OnnxClassifier>();
    }
}
