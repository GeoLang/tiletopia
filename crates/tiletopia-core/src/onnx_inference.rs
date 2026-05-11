//! ONNX Runtime inference for point cloud classification.
//!
//! Runs ONNX models directly in Rust — no Python sidecar needed.
//! Supports any ONNX model exported from PyTorch, TensorFlow, or scikit-learn.
//!
//! Enable with `--features tiletopia-core/onnx`.

#[cfg(feature = "onnx")]
pub mod runtime {
    use ndarray::{Array2, Axis};
    use ort::{session::Session, value::Value};
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
            &self,
            features: &[PointFeatures],
        ) -> Result<Vec<(Classification, f32)>, ort::Error> {
            let n = features.len();
            if n == 0 {
                return Ok(vec![]);
            }

            // Build input array (n, 8)
            let mut input = Array2::<f32>::zeros((n, 8));
            for (i, f) in features.iter().enumerate() {
                input[[i, 0]] = f.height_above_ground as f32;
                input[[i, 1]] = f.planarity as f32;
                input[[i, 2]] = f.linearity as f32;
                input[[i, 3]] = f.scatter as f32;
                input[[i, 4]] = f.density as f32;
                input[[i, 5]] = f.elevation as f32;
                input[[i, 6]] = f.return_number as f32;
                input[[i, 7]] = f.normal_z as f32;
            }

            let input_value = Value::from_array(input.view())?;
            let outputs = self.session.run(ort::inputs![input_value]?)?;

            // Extract output logits
            let output = outputs[0].try_extract_tensor::<f32>()?;
            let output_view = output.view();

            let mut results = Vec::with_capacity(n);
            for i in 0..n {
                let row = output_view.index_axis(Axis(0), i);
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
            &self,
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
