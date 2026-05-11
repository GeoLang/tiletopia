//! ML Model Registry — register, version, and manage classification models.
//!
//! Supports multiple model formats (ONNX, PyTorch, scikit-learn) with
//! metadata, accuracy metrics, and A/B testing between model versions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, put},
};

use crate::AppState;

/// Supported model formats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelFormat {
    /// ONNX Runtime (runs in Rust, no Python needed)
    Onnx,
    /// PyTorch model (requires Python sidecar)
    PyTorch,
    /// Scikit-learn pickle (requires Python sidecar)
    ScikitLearn,
    /// TileTopia built-in decision tree ensemble
    BuiltIn,
    /// Custom HTTP endpoint
    HttpEndpoint(String),
}

/// Model task type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ModelTask {
    /// Per-point semantic segmentation (ASPRS classes)
    PointClassification,
    /// Object detection (buildings, vehicles, etc.)
    ObjectDetection,
    /// Change detection between epochs
    ChangeDetection,
    /// Anomaly / outlier detection
    AnomalyDetection,
    /// Terrain feature extraction
    TerrainAnalysis,
    /// Custom task
    Custom(String),
}

/// Accuracy metrics for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetrics {
    pub overall_accuracy: f64,
    pub mean_iou: f64,
    pub per_class_accuracy: HashMap<String, f64>,
    pub per_class_iou: HashMap<String, f64>,
    pub confusion_matrix: Option<Vec<Vec<u64>>>,
    pub eval_dataset: Option<String>,
    pub eval_point_count: u64,
}

/// A registered ML model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredModel {
    pub id: Uuid,
    pub name: String,
    pub version: String,
    pub format: ModelFormat,
    pub task: ModelTask,
    pub description: String,
    /// Path to model file (ONNX, .pt, .pkl) or HTTP URL
    pub artifact_path: String,
    /// Number of input features expected
    pub num_features: usize,
    /// Class labels this model predicts
    pub class_labels: Vec<String>,
    pub metrics: Option<ModelMetrics>,
    pub created_at: DateTime<Utc>,
    pub is_default: bool,
    pub tags: HashMap<String, String>,
}

/// A/B test configuration between two models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbTest {
    pub id: Uuid,
    pub name: String,
    pub model_a: Uuid,
    pub model_b: Uuid,
    /// Fraction of requests routed to model B (0.0–1.0)
    pub traffic_split: f64,
    pub results_a: AbTestResults,
    pub results_b: AbTestResults,
    pub created_at: DateTime<Utc>,
    pub active: bool,
}

/// Accumulated A/B test results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AbTestResults {
    pub total_points: u64,
    pub total_latency_ms: f64,
    pub correct_predictions: u64,
}

/// Thread-safe model registry.
#[derive(Clone)]
pub struct ModelRegistry {
    models: Arc<RwLock<HashMap<Uuid, RegisteredModel>>>,
    ab_tests: Arc<RwLock<HashMap<Uuid, AbTest>>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        let mut models = HashMap::new();

        // Register built-in model
        let builtin = RegisteredModel {
            id: Uuid::new_v4(),
            name: "TileTopia Built-in".into(),
            version: "1.0.0".into(),
            format: ModelFormat::BuiltIn,
            task: ModelTask::PointClassification,
            description: "Expert-tuned decision tree ensemble for aerial LiDAR".into(),
            artifact_path: "builtin".into(),
            num_features: 8,
            class_labels: vec![
                "ground".into(),
                "low_vegetation".into(),
                "medium_vegetation".into(),
                "high_vegetation".into(),
                "building".into(),
                "water".into(),
                "road".into(),
                "power_line".into(),
                "bridge".into(),
                "noise".into(),
            ],
            metrics: None,
            created_at: Utc::now(),
            is_default: true,
            tags: HashMap::new(),
        };
        models.insert(builtin.id, builtin);

        Self {
            models: Arc::new(RwLock::new(models)),
            ab_tests: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new model.
    pub async fn register(&self, model: RegisteredModel) -> Uuid {
        let id = model.id;
        self.models.write().await.insert(id, model);
        id
    }

    /// List all registered models.
    pub async fn list(&self) -> Vec<RegisteredModel> {
        self.models.read().await.values().cloned().collect()
    }

    /// Get a model by ID.
    pub async fn get(&self, id: Uuid) -> Option<RegisteredModel> {
        self.models.read().await.get(&id).cloned()
    }

    /// Get the default model for a task.
    pub async fn get_default(&self, task: &ModelTask) -> Option<RegisteredModel> {
        self.models
            .read()
            .await
            .values()
            .find(|m| &m.task == task && m.is_default)
            .cloned()
    }

    /// Set a model as the default for its task.
    pub async fn set_default(&self, id: Uuid) -> bool {
        let mut models = self.models.write().await;
        let task = match models.get(&id) {
            Some(m) => m.task.clone(),
            None => return false,
        };
        // Unset previous defaults for this task
        for m in models.values_mut() {
            if m.task == task {
                m.is_default = false;
            }
        }
        if let Some(m) = models.get_mut(&id) {
            m.is_default = true;
            true
        } else {
            false
        }
    }

    /// Delete a model.
    pub async fn delete(&self, id: Uuid) -> bool {
        self.models.write().await.remove(&id).is_some()
    }

    /// Update model metrics.
    pub async fn update_metrics(&self, id: Uuid, metrics: ModelMetrics) -> bool {
        let mut models = self.models.write().await;
        if let Some(m) = models.get_mut(&id) {
            m.metrics = Some(metrics);
            true
        } else {
            false
        }
    }

    /// Create an A/B test between two models.
    pub async fn create_ab_test(
        &self,
        name: String,
        model_a: Uuid,
        model_b: Uuid,
        split: f64,
    ) -> Uuid {
        let test = AbTest {
            id: Uuid::new_v4(),
            name,
            model_a,
            model_b,
            traffic_split: split.clamp(0.0, 1.0),
            results_a: AbTestResults::default(),
            results_b: AbTestResults::default(),
            created_at: Utc::now(),
            active: true,
        };
        let id = test.id;
        self.ab_tests.write().await.insert(id, test);
        id
    }

    /// List active A/B tests.
    pub async fn list_ab_tests(&self) -> Vec<AbTest> {
        self.ab_tests.read().await.values().cloned().collect()
    }

    /// Select which model to use for an A/B test (based on traffic split).
    pub async fn select_ab_model(&self, test_id: Uuid) -> Option<Uuid> {
        let tests = self.ab_tests.read().await;
        let test = tests.get(&test_id)?;
        if !test.active {
            return None;
        }
        let r: f64 = rand_simple();
        Some(if r < test.traffic_split {
            test.model_b
        } else {
            test.model_a
        })
    }

    /// Record A/B test result.
    pub async fn record_ab_result(
        &self,
        test_id: Uuid,
        used_model_b: bool,
        points: u64,
        latency_ms: f64,
        correct: u64,
    ) {
        let mut tests = self.ab_tests.write().await;
        if let Some(test) = tests.get_mut(&test_id) {
            let results = if used_model_b {
                &mut test.results_b
            } else {
                &mut test.results_a
            };
            results.total_points += points;
            results.total_latency_ms += latency_ms;
            results.correct_predictions += correct;
        }
    }
}

/// Simple pseudo-random in [0, 1) without external crate.
fn rand_simple() -> f64 {
    use std::time::SystemTime;
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seed = t.subsec_nanos() as u64 ^ t.as_secs();
    ((seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407))
        >> 33) as f64
        / (1u64 << 31) as f64
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── HTTP Routes ─────────────────────────────────────────────────────────────

pub fn model_registry_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/models", get(list_models).post(register_model))
        .route("/api/v1/models/{id}", get(get_model).delete(delete_model))
        .route("/api/v1/models/{id}/default", put(set_default_model))
        .route("/api/v1/models/{id}/metrics", put(update_model_metrics))
        .route(
            "/api/v1/models/ab-tests",
            get(list_ab_tests).post(create_ab_test_handler),
        )
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let models = state.model_registry.list().await;
    Json(serde_json::json!(models))
}

async fn get_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state.model_registry.get(id).await {
        Some(m) => Json(serde_json::json!(m)),
        None => Json(serde_json::json!({"error": "model not found"})),
    }
}

async fn register_model(
    State(state): State<Arc<AppState>>,
    Json(model): Json<RegisteredModel>,
) -> Json<serde_json::Value> {
    let id = state.model_registry.register(model).await;
    Json(serde_json::json!({"id": id}))
}

async fn delete_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Json<serde_json::Value> {
    let ok = state.model_registry.delete(id).await;
    Json(serde_json::json!({"deleted": ok}))
}

async fn set_default_model(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Json<serde_json::Value> {
    let ok = state.model_registry.set_default(id).await;
    Json(serde_json::json!({"set_default": ok}))
}

async fn update_model_metrics(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(metrics): Json<ModelMetrics>,
) -> Json<serde_json::Value> {
    let ok = state.model_registry.update_metrics(id, metrics).await;
    Json(serde_json::json!({"updated": ok}))
}

#[derive(Deserialize)]
struct CreateAbTest {
    name: String,
    model_a: Uuid,
    model_b: Uuid,
    traffic_split: f64,
}

async fn list_ab_tests(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tests = state.model_registry.list_ab_tests().await;
    Json(serde_json::json!(tests))
}

async fn create_ab_test_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateAbTest>,
) -> Json<serde_json::Value> {
    let id = state
        .model_registry
        .create_ab_test(req.name, req.model_a, req.model_b, req.traffic_split)
        .await;
    Json(serde_json::json!({"id": id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_list() {
        let reg = ModelRegistry::new();
        assert_eq!(reg.list().await.len(), 1); // built-in

        let model = RegisteredModel {
            id: Uuid::new_v4(),
            name: "Test ONNX".into(),
            version: "0.1.0".into(),
            format: ModelFormat::Onnx,
            task: ModelTask::PointClassification,
            description: "Test".into(),
            artifact_path: "/models/test.onnx".into(),
            num_features: 8,
            class_labels: vec!["ground".into(), "building".into()],
            metrics: None,
            created_at: Utc::now(),
            is_default: false,
            tags: HashMap::new(),
        };
        let id = reg.register(model).await;
        assert_eq!(reg.list().await.len(), 2);
        assert!(reg.get(id).await.is_some());
    }

    #[tokio::test]
    async fn test_set_default() {
        let reg = ModelRegistry::new();
        let model = RegisteredModel {
            id: Uuid::new_v4(),
            name: "New Default".into(),
            version: "1.0.0".into(),
            format: ModelFormat::Onnx,
            task: ModelTask::PointClassification,
            description: "".into(),
            artifact_path: "/models/new.onnx".into(),
            num_features: 8,
            class_labels: vec![],
            metrics: None,
            created_at: Utc::now(),
            is_default: false,
            tags: HashMap::new(),
        };
        let id = reg.register(model).await;
        reg.set_default(id).await;
        let default = reg
            .get_default(&ModelTask::PointClassification)
            .await
            .unwrap();
        assert_eq!(default.id, id);
    }

    #[tokio::test]
    async fn test_delete() {
        let reg = ModelRegistry::new();
        let models = reg.list().await;
        let id = models[0].id;
        assert!(reg.delete(id).await);
        assert!(reg.get(id).await.is_none());
    }

    #[tokio::test]
    async fn test_update_metrics() {
        let reg = ModelRegistry::new();
        let models = reg.list().await;
        let id = models[0].id;
        let metrics = ModelMetrics {
            overall_accuracy: 0.92,
            mean_iou: 0.78,
            per_class_accuracy: HashMap::new(),
            per_class_iou: HashMap::new(),
            confusion_matrix: None,
            eval_dataset: Some("test_v1".into()),
            eval_point_count: 50000,
        };
        assert!(reg.update_metrics(id, metrics).await);
        let m = reg.get(id).await.unwrap();
        assert_eq!(m.metrics.unwrap().overall_accuracy, 0.92);
    }

    #[tokio::test]
    async fn test_ab_test() {
        let reg = ModelRegistry::new();
        let models = reg.list().await;
        let model_a = models[0].id;
        let model_b_reg = RegisteredModel {
            id: Uuid::new_v4(),
            name: "Model B".into(),
            version: "1.0.0".into(),
            format: ModelFormat::PyTorch,
            task: ModelTask::PointClassification,
            description: "".into(),
            artifact_path: "/models/b.pt".into(),
            num_features: 8,
            class_labels: vec![],
            metrics: None,
            created_at: Utc::now(),
            is_default: false,
            tags: HashMap::new(),
        };
        let model_b = reg.register(model_b_reg).await;
        let test_id = reg
            .create_ab_test("test".into(), model_a, model_b, 0.5)
            .await;
        let selected = reg.select_ab_model(test_id).await;
        assert!(selected.is_some());
        let tests = reg.list_ab_tests().await;
        assert_eq!(tests.len(), 1);
    }
}
