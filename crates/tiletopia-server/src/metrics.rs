//! Prometheus metrics endpoint.
//!
//! Uses metrics-exporter-prometheus to record and serve real counters/gauges.

use axum::response::IntoResponse;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;

static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus metrics recorder. Call once at startup.
pub fn install() {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
    PROMETHEUS_HANDLE.set(handle).ok();

    // Register metrics
    metrics::describe_counter!("tiletopia_requests_total", "Total HTTP requests");
    metrics::describe_counter!("tiletopia_tiling_jobs_started", "Tiling jobs started");
    metrics::describe_counter!("tiletopia_tiling_jobs_completed", "Tiling jobs completed");
    metrics::describe_counter!("tiletopia_tiling_jobs_failed", "Tiling jobs failed");
    metrics::describe_counter!("tiletopia_uploads_total", "Total file uploads");
    metrics::describe_gauge!("tiletopia_assets_total", "Total assets managed");
    metrics::describe_gauge!("tiletopia_tiles_served", "Tiles served");
    metrics::describe_histogram!(
        "tiletopia_tiling_duration_seconds",
        "Tiling job duration in seconds"
    );
}

/// Increment a counter metric.
pub fn inc_counter(name: &'static str) {
    metrics::counter!(name).increment(1);
}

/// Set a gauge value.
pub fn set_gauge(name: &'static str, value: f64) {
    metrics::gauge!(name).set(value);
}

/// Record a histogram observation.
pub fn record_histogram(name: &'static str, value: f64) {
    metrics::histogram!(name).record(value);
}

/// Handler for GET /metrics — serves Prometheus text format.
pub async fn metrics_handler() -> impl IntoResponse {
    let output = match PROMETHEUS_HANDLE.get() {
        Some(handle) => handle.render(),
        None => "# HELP tiletopia_up Server is running\ntiletopia_up 1\n".to_string(),
    };
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        output,
    )
}
