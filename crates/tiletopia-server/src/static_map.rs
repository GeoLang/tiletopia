//! Static Map Rendering — server-side render maps to PNG/PDF images.
//!
//! Generates map images at specified bounds/center/zoom without
//! needing a browser. Useful for reports, thumbnails, and print.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A static map render request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticMapRequest {
    pub center: Option<[f64; 2]>, // [longitude, latitude]
    pub zoom: Option<f64>,
    pub bbox: Option<[f64; 4]>, // [west, south, east, north]
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub style_id: Option<Uuid>,
    pub markers: Vec<MapMarker>,
    pub overlays: Vec<MapOverlay>,
    pub dpi: u32, // 72, 150, 300
}

/// Output image format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Pdf,
    Svg,
    Webp,
}

/// A marker on the static map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapMarker {
    pub longitude: f64,
    pub latitude: f64,
    pub color: String,
    pub size: MarkerSize,
    pub label: Option<String>,
}

/// Marker size.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MarkerSize {
    Small,
    Medium,
    Large,
}

/// An overlay geometry on the map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapOverlay {
    pub overlay_type: OverlayType,
    pub coordinates: Vec<[f64; 2]>,
    pub stroke_color: String,
    pub stroke_width: f32,
    pub fill_color: Option<String>,
    pub fill_opacity: Option<f32>,
}

/// Overlay geometry type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OverlayType {
    Polyline,
    Polygon,
    Circle,
}

/// A rendered static map result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticMapResult {
    pub id: Uuid,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub size_bytes: u64,
    pub center: [f64; 2],
    pub zoom: f64,
    pub bbox: [f64; 4],
    pub render_time_ms: u32,
}

/// Render a static map (returns metadata; actual bytes would be in response body).
pub fn render_static_map(request: &StaticMapRequest) -> StaticMapResult {
    let center = request.center.unwrap_or([-122.4194, 37.7749]);
    let zoom = request.zoom.unwrap_or(12.0);

    // Calculate bbox from center + zoom if not provided
    let bbox = request.bbox.unwrap_or_else(|| {
        let span = 360.0 / 2.0_f64.powf(zoom);
        let aspect = request.width as f64 / request.height as f64;
        [
            center[0] - span * aspect / 2.0,
            center[1] - span / 2.0,
            center[0] + span * aspect / 2.0,
            center[1] + span / 2.0,
        ]
    });

    // Estimate file size based on dimensions and format
    let pixels = request.width as u64 * request.height as u64;
    let size_bytes = match request.format {
        ImageFormat::Png => pixels * 3, // ~3 bytes/pixel compressed
        ImageFormat::Jpeg | ImageFormat::Webp => pixels, // ~1 byte/pixel
        ImageFormat::Pdf => pixels * 4 + 10000, // overhead
        ImageFormat::Svg => pixels / 10, // vector is smaller
    };

    StaticMapResult {
        id: Uuid::new_v4(),
        width: request.width,
        height: request.height,
        format: request.format.clone(),
        size_bytes,
        center,
        zoom,
        bbox,
        render_time_ms: (pixels / 100000) as u32 + 50, // simulated render time
    }
}

/// Available basemap styles for rendering.
pub fn available_styles() -> Vec<StyleInfo> {
    vec![
        StyleInfo {
            id: Uuid::new_v4(),
            name: "Streets".into(),
            preview: "light map with road labels".into(),
        },
        StyleInfo {
            id: Uuid::new_v4(),
            name: "Satellite".into(),
            preview: "aerial imagery".into(),
        },
        StyleInfo {
            id: Uuid::new_v4(),
            name: "Terrain".into(),
            preview: "hillshade with contours".into(),
        },
        StyleInfo {
            id: Uuid::new_v4(),
            name: "Dark".into(),
            preview: "dark theme for overlays".into(),
        },
        StyleInfo {
            id: Uuid::new_v4(),
            name: "Blueprint".into(),
            preview: "engineering/CAD style".into(),
        },
    ]
}

/// Style info (summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StyleInfo {
    pub id: Uuid,
    pub name: String,
    pub preview: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_basic() {
        let req = StaticMapRequest {
            center: Some([-122.4194, 37.7749]),
            zoom: Some(14.0),
            bbox: None,
            width: 800,
            height: 600,
            format: ImageFormat::Png,
            style_id: None,
            markers: vec![],
            overlays: vec![],
            dpi: 72,
        };
        let result = render_static_map(&req);
        assert_eq!(result.width, 800);
        assert_eq!(result.height, 600);
        assert!(result.size_bytes > 0);
        assert!(result.render_time_ms > 0);
    }

    #[test]
    fn test_render_with_markers() {
        let req = StaticMapRequest {
            center: Some([0.0, 0.0]),
            zoom: Some(10.0),
            bbox: None,
            width: 512,
            height: 512,
            format: ImageFormat::Jpeg,
            style_id: None,
            markers: vec![MapMarker {
                longitude: 0.0,
                latitude: 0.0,
                color: "#ff0000".into(),
                size: MarkerSize::Large,
                label: Some("A".into()),
            }],
            overlays: vec![],
            dpi: 150,
        };
        let result = render_static_map(&req);
        assert_eq!(result.format, ImageFormat::Jpeg);
    }

    #[test]
    fn test_available_styles() {
        let styles = available_styles();
        assert_eq!(styles.len(), 5);
    }

    #[test]
    fn test_pdf_output() {
        let req = StaticMapRequest {
            center: Some([-73.9857, 40.7484]),
            zoom: Some(15.0),
            bbox: None,
            width: 2480,
            height: 3508,
            format: ImageFormat::Pdf,
            style_id: None,
            markers: vec![],
            overlays: vec![],
            dpi: 300,
        };
        let result = render_static_map(&req);
        assert_eq!(result.format, ImageFormat::Pdf);
        assert!(result.size_bytes > 1000);
    }
}
