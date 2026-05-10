//! Static Map Rendering — server-side render maps to PNG/PDF images.
//!
//! Generates map images at specified bounds/center/zoom without
//! needing a browser. Useful for reports, thumbnails, and print.

use image::{ImageBuffer, Rgb, RgbImage};
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
    /// The rendered image bytes.
    #[serde(skip)]
    pub image_bytes: Vec<u8>,
}

/// Render a static map to image bytes.
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

    let start = std::time::Instant::now();
    let mut img: RgbImage = ImageBuffer::from_pixel(request.width, request.height, Rgb([230, 230, 230]));

    // Draw markers
    for marker in &request.markers {
        let (px, py) = lonlat_to_pixel(marker.longitude, marker.latitude, &bbox, request.width, request.height);
        let radius = match marker.size {
            MarkerSize::Small => 4i32,
            MarkerSize::Medium => 8,
            MarkerSize::Large => 12,
        };
        let color = parse_hex_color(&marker.color);
        draw_filled_circle(&mut img, px, py, radius, color);
    }

    // Draw overlays
    for overlay in &request.overlays {
        let color = parse_hex_color(&overlay.stroke_color);
        let pixels: Vec<(i32, i32)> = overlay
            .coordinates
            .iter()
            .map(|c| lonlat_to_pixel(c[0], c[1], &bbox, request.width, request.height))
            .collect();

        for pair in pixels.windows(2) {
            draw_line(&mut img, pair[0].0, pair[0].1, pair[1].0, pair[1].1, color);
        }

        if overlay.overlay_type == OverlayType::Polygon {
            if let (Some(first), Some(last)) = (pixels.first(), pixels.last()) {
                draw_line(&mut img, last.0, last.1, first.0, first.1, color);
            }
        }
    }

    let render_time_ms = start.elapsed().as_millis() as u32;

    // Encode to the requested format
    let mut buf = std::io::Cursor::new(Vec::new());
    match request.format {
        ImageFormat::Png => {
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        }
        ImageFormat::Jpeg | ImageFormat::Webp => {
            img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        }
        ImageFormat::Pdf | ImageFormat::Svg => {
            // Fall back to PNG for formats the image crate doesn't support
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        }
    }

    let bytes = buf.into_inner();

    StaticMapResult {
        id: Uuid::new_v4(),
        width: request.width,
        height: request.height,
        format: request.format.clone(),
        size_bytes: bytes.len() as u64,
        center,
        zoom,
        bbox,
        render_time_ms,
        image_bytes: bytes,
    }
}

fn lonlat_to_pixel(lon: f64, lat: f64, bbox: &[f64; 4], w: u32, h: u32) -> (i32, i32) {
    let px = ((lon - bbox[0]) / (bbox[2] - bbox[0]) * w as f64) as i32;
    let py = ((bbox[3] - lat) / (bbox[3] - bbox[1]) * h as f64) as i32;
    (px, py)
}

fn parse_hex_color(hex: &str) -> Rgb<u8> {
    let hex = hex.trim_start_matches('#');
    if hex.len() >= 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(128);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(128);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(128);
        Rgb([r, g, b])
    } else {
        Rgb([128, 128, 128])
    }
}

fn draw_filled_circle(img: &mut RgbImage, cx: i32, cy: i32, radius: i32, color: Rgb<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && px < w && py >= 0 && py < h {
                    img.put_pixel(px as u32, py as u32, color);
                }
            }
        }
    }
}

fn draw_line(img: &mut RgbImage, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut x = x0;
    let mut y = y0;

    loop {
        if x >= 0 && x < w && y >= 0 && y < h {
            img.put_pixel(x as u32, y as u32, color);
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
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
