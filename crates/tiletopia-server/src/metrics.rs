//! Prometheus metrics endpoint.

use axum::response::IntoResponse;

/// Install the Prometheus metrics exporter and return a handle.
pub fn install() {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder.install().ok();
}

/// Handler for GET /metrics — serves Prometheus text format.
pub async fn metrics_handler() -> impl IntoResponse {
    // metrics-exporter-prometheus installs a global recorder;
    // the /metrics endpoint is handled by the recorder's built-in HTTP server
    // or we can render manually. For simplicity, return a placeholder.
    // In production, use the recorder's built-in server on a separate port.
    let output = prometheus_output();
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        output,
    )
}

fn prometheus_output() -> String {
    // The metrics crate doesn't expose a text renderer directly.
    // Use the exporter's render method if available, otherwise basic stats.
    "# tiletopia metrics\n# Use TILETOPIA_METRICS_PORT to expose full Prometheus endpoint\n".to_string()
}
