//! Static map rendering: map images drawn server-side, without a browser.
//!
//! The base layer is a hillshade of the DEM this server holds, the same stores
//! every elevation and analysis route reads (see [`crate::elevation`]). Where no
//! DEM covers the requested box the image gets a flat background instead, and
//! the render says which of the two it is: there is no basemap here to fall back
//! to, and drawing invented streets or imagery would be a picture of nothing.
//!
//! Markers and overlays are drawn on top in pixel space. The projection is
//! plate carrée: longitude and latitude map linearly onto x and y, so a box
//! crossing the antimeridian is refused rather than wrapped.

use image::{ImageBuffer, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use serde::Deserialize;
use terrano_core::hillshade;

use crate::analysis::{dem_over_bbox, hillshade_color};
use crate::elevation::{ElevationField, ElevationGap, ElevationSources, on_the_globe};

/// Most pixels per side. One request holds the whole image in memory twice, as
/// an RGB buffer and as encoded bytes.
pub const MAX_IMAGE_SIDE: u32 = 4096;

/// Resolutions a PDF page may be laid out at. Only the PDF uses dpi: it sets
/// the page size in points, and the raster formats carry no resolution.
pub const ALLOWED_DPI: [u32; 3] = [72, 150, 300];

/// PDF user-space units per inch.
const POINTS_PER_INCH: f64 = 72.0;

/// Quality the JPEG answer and the JPEG the PDF embeds are encoded at.
const JPEG_QUALITY: u8 = 85;

/// Zoom used when a request gives a center and no zoom.
const DEFAULT_ZOOM: f64 = 12.0;

/// Zoom range a center-and-zoom request may ask for. Past 24 the box is
/// narrower than a float degree can express.
const ZOOM_RANGE: (f64, f64) = (0.0, 24.0);

/// Widest stroke an overlay may ask for, in pixels.
const MAX_STROKE_WIDTH: f32 = 64.0;

/// Background for a plain base layer, and for cells the DEM does not cover
/// under a hillshade.
const PLAIN_BACKGROUND: Rgb<u8> = Rgb([235, 235, 235]);

/// Most DEM samples per side the hillshade is computed on. The shading is
/// nearest-neighbour scaled onto the canvas from there, so a 4096-pixel image
/// costs the same DEM reads as a 1024-pixel one.
const MAX_SHADE_SAMPLES: usize = 1024;

/// Fewest DEM samples per side: terrano's 3x3 slope kernel needs a few cells to
/// read anything.
const MIN_SHADE_SAMPLES: usize = 8;

const HILLSHADE_AZIMUTH: f64 = 315.0;
const HILLSHADE_ALTITUDE: f64 = 45.0;

/// A static map render request.
#[derive(Debug, Clone, Deserialize)]
pub struct StaticMapRequest {
    /// `[longitude, latitude]` the image is centred on, with `zoom`. Ignored
    /// when `bbox` is given.
    #[serde(default)]
    pub center: Option<[f64; 2]>,
    #[serde(default)]
    pub zoom: Option<f64>,
    /// `[west, south, east, north]` in degrees.
    #[serde(default)]
    pub bbox: Option<[f64; 4]>,
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    #[serde(default)]
    pub markers: Vec<MapMarker>,
    #[serde(default)]
    pub overlays: Vec<MapOverlay>,
    /// Page resolution for the PDF format, one of [`ALLOWED_DPI`].
    #[serde(default = "default_dpi")]
    pub dpi: u32,
}

fn default_dpi() -> u32 {
    ALLOWED_DPI[0]
}

/// Output image format.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
    Svg,
    Pdf,
}

/// Every format this module encodes.
pub const FORMATS: [ImageFormat; 5] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Webp,
    ImageFormat::Svg,
    ImageFormat::Pdf,
];

impl ImageFormat {
    /// The name this format is asked for by, in a query string or a request
    /// body.
    pub fn name(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Webp => "webp",
            ImageFormat::Svg => "svg",
            ImageFormat::Pdf => "pdf",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Svg => "image/svg+xml",
            ImageFormat::Pdf => "application/pdf",
        }
    }

    /// The format this name asks for, `None` for a name nothing encodes. `jpg`
    /// is taken as `jpeg`.
    pub fn from_name(name: &str) -> Option<ImageFormat> {
        if name == "jpg" {
            return Some(ImageFormat::Jpeg);
        }
        FORMATS.into_iter().find(|format| format.name() == name)
    }
}

/// A marker on the static map.
#[derive(Debug, Clone, Deserialize)]
pub struct MapMarker {
    pub longitude: f64,
    pub latitude: f64,
    /// `#rrggbb`, or `rrggbb`.
    pub color: String,
    pub size: MarkerSize,
}

/// Marker size.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MarkerSize {
    Small,
    Medium,
    Large,
}

impl MarkerSize {
    fn radius(self) -> i32 {
        match self {
            MarkerSize::Small => 4,
            MarkerSize::Medium => 8,
            MarkerSize::Large => 12,
        }
    }
}

/// An overlay geometry on the map.
#[derive(Debug, Clone, Deserialize)]
pub struct MapOverlay {
    pub overlay_type: OverlayType,
    /// `[longitude, latitude]` positions, in drawing order.
    pub coordinates: Vec<[f64; 2]>,
    pub stroke_color: String,
    pub stroke_width: f32,
    /// Fills a polygon. A polyline is never filled.
    #[serde(default)]
    pub fill_color: Option<String>,
    #[serde(default)]
    pub fill_opacity: Option<f32>,
}

/// Overlay geometry type.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OverlayType {
    Polyline,
    Polygon,
}

/// What the image is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseLayer {
    /// Hillshade of the DEM covering the requested box.
    Hillshade,
    /// Flat background, because no DEM covers the requested box.
    Plain,
}

/// Every base layer a render is drawn on.
pub const BASE_LAYERS: [BaseLayer; 2] = [BaseLayer::Hillshade, BaseLayer::Plain];

impl BaseLayer {
    pub fn name(self) -> &'static str {
        match self {
            BaseLayer::Hillshade => "hillshade",
            BaseLayer::Plain => "plain",
        }
    }

    /// Where the pixels under the markers came from.
    pub fn drawn_from(self) -> &'static str {
        match self {
            BaseLayer::Hillshade => "the DEM staged for the requested area",
            BaseLayer::Plain => "a flat background, where no DEM covers the area",
        }
    }
}

/// A checked request: a box the renderer can draw, dimensions inside the caps,
/// and every colour already parsed.
#[derive(Debug, Clone)]
pub struct StaticMapPlan {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub dpi: u32,
    pub bbox: [f64; 4],
    markers: Vec<PlannedMarker>,
    overlays: Vec<PlannedOverlay>,
}

#[derive(Debug, Clone)]
struct PlannedMarker {
    longitude: f64,
    latitude: f64,
    color: Rgb<u8>,
    radius: i32,
}

#[derive(Debug, Clone)]
struct PlannedOverlay {
    overlay_type: OverlayType,
    coordinates: Vec<[f64; 2]>,
    stroke: Rgb<u8>,
    stroke_width: u32,
    fill: Option<(Rgb<u8>, f32)>,
}

/// A rendered static map.
pub struct StaticMapRender {
    pub bytes: Vec<u8>,
    pub base_layer: BaseLayer,
}

impl StaticMapRequest {
    /// Check the request, refusing with the reason a caller can act on.
    pub fn plan(&self) -> Result<StaticMapPlan, String> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_IMAGE_SIDE
            || self.height > MAX_IMAGE_SIDE
        {
            return Err(format!(
                "width and height must be 1..={MAX_IMAGE_SIDE} pixels, got {}x{}",
                self.width, self.height
            ));
        }
        if !ALLOWED_DPI.contains(&self.dpi) {
            return Err(format!(
                "dpi must be one of {ALLOWED_DPI:?}, got {}",
                self.dpi
            ));
        }

        let bbox = self.resolved_bbox()?;
        let mut markers = Vec::with_capacity(self.markers.len());
        for marker in &self.markers {
            if !on_the_globe(marker.longitude, marker.latitude) {
                return Err(format!(
                    "marker ({}, {}) is not a point on the globe",
                    marker.longitude, marker.latitude
                ));
            }
            markers.push(PlannedMarker {
                longitude: marker.longitude,
                latitude: marker.latitude,
                color: parse_color(&marker.color)?,
                radius: marker.size.radius(),
            });
        }

        let mut overlays = Vec::with_capacity(self.overlays.len());
        for overlay in &self.overlays {
            overlays.push(overlay.planned()?);
        }

        Ok(StaticMapPlan {
            width: self.width,
            height: self.height,
            format: self.format,
            dpi: self.dpi,
            bbox,
            markers,
            overlays,
        })
    }

    /// The box to draw: the one given, or the one a center and zoom name.
    fn resolved_bbox(&self) -> Result<[f64; 4], String> {
        if let Some(bbox) = self.bbox {
            return checked_bbox(bbox);
        }
        let Some([longitude, latitude]) = self.center else {
            return Err("give a bbox, or a center to draw around".into());
        };
        if !on_the_globe(longitude, latitude) {
            return Err(format!(
                "center ({longitude}, {latitude}) is not a point on the globe"
            ));
        }
        let zoom = self.zoom.unwrap_or(DEFAULT_ZOOM);
        let (min_zoom, max_zoom) = ZOOM_RANGE;
        if !(min_zoom..=max_zoom).contains(&zoom) {
            return Err(format!(
                "zoom must be within {min_zoom}..={max_zoom}, got {zoom}"
            ));
        }
        let span = 360.0 / 2.0_f64.powf(zoom);
        let aspect = self.width as f64 / self.height as f64;
        checked_bbox([
            longitude - span * aspect / 2.0,
            (latitude - span / 2.0).max(-90.0),
            longitude + span * aspect / 2.0,
            (latitude + span / 2.0).min(90.0),
        ])
    }
}

impl MapOverlay {
    fn planned(&self) -> Result<PlannedOverlay, String> {
        if self.coordinates.len() < 2 {
            return Err("an overlay needs at least two [longitude, latitude] positions".into());
        }
        if self.overlay_type == OverlayType::Polygon && self.coordinates.len() < 3 {
            return Err("a polygon overlay needs at least three positions".into());
        }
        for &[longitude, latitude] in &self.coordinates {
            if !on_the_globe(longitude, latitude) {
                return Err(format!(
                    "overlay position ({longitude}, {latitude}) is not a point on the globe"
                ));
            }
        }
        if !(1.0..=MAX_STROKE_WIDTH).contains(&self.stroke_width) {
            return Err(format!(
                "stroke_width must be within 1..={MAX_STROKE_WIDTH} pixels, got {}",
                self.stroke_width
            ));
        }
        let opacity = self.fill_opacity.unwrap_or(1.0);
        if !(0.0..=1.0).contains(&opacity) {
            return Err(format!("fill_opacity must be within 0..=1, got {opacity}"));
        }
        let fill = match &self.fill_color {
            Some(color) if self.overlay_type == OverlayType::Polygon => {
                Some((parse_color(color)?, opacity))
            }
            Some(_) => return Err("only a polygon overlay can be filled".into()),
            None => None,
        };
        Ok(PlannedOverlay {
            overlay_type: self.overlay_type,
            coordinates: self.coordinates.clone(),
            stroke: parse_color(&self.stroke_color)?,
            stroke_width: self.stroke_width.round() as u32,
            fill,
        })
    }
}

/// A box the renderer can draw: on the globe, covering ground, and not crossing
/// the antimeridian, which the linear pixel mapping cannot express.
fn checked_bbox(bbox: [f64; 4]) -> Result<[f64; 4], String> {
    let [west, south, east, north] = bbox;
    if !on_the_globe(west, south) || !on_the_globe(east, north) || west >= east || south >= north {
        return Err(format!(
            "bbox {bbox:?} must be west,south,east,north in degrees, on the globe \
             and covering ground"
        ));
    }
    Ok(bbox)
}

fn parse_color(color: &str) -> Result<Rgb<u8>, String> {
    let digits = color.strip_prefix('#').unwrap_or(color);
    let bad = || format!("colour {color:?} must be six hex digits, as #rrggbb");
    if digits.len() != 6 {
        return Err(bad());
    }
    let mut channels = [0u8; 3];
    for (channel, pair) in channels.iter_mut().zip(digits.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
        *channel = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
    }
    Ok(Rgb(channels))
}

/// Draw the plan, over the hillshade of whatever DEM covers it.
pub async fn render(
    plan: &StaticMapPlan,
    sources: &ElevationSources,
) -> Result<StaticMapRender, ElevationGap> {
    let field = sources.field(plan.bbox).await?;
    let (base, base_layer) = base_canvas(plan, &field)?;

    // an SVG keeps its markers and overlays as vector elements, so only the
    // base layer is rasterized into it
    if plan.format == ImageFormat::Svg {
        let bytes = svg_document(plan, &png_bytes(&base));
        return Ok(StaticMapRender { bytes, base_layer });
    }

    let mut canvas = base;
    for overlay in &plan.overlays {
        draw_overlay(&mut canvas, plan, overlay);
    }
    for marker in &plan.markers {
        let (x, y) = pixel_of(marker.longitude, marker.latitude, plan);
        draw_disc(&mut canvas, x, y, marker.radius, marker.color);
    }

    let bytes = match plan.format {
        ImageFormat::Png => png_bytes(&canvas),
        ImageFormat::Jpeg => jpeg_bytes(&canvas),
        ImageFormat::Webp => webp_bytes(&canvas),
        ImageFormat::Pdf => pdf_page(&jpeg_bytes(&canvas), plan),
        ImageFormat::Svg => unreachable!("answered above"),
    };
    Ok(StaticMapRender { bytes, base_layer })
}

/// The image before markers and overlays: a hillshade of the DEM over the box,
/// or a flat background where no DEM covers it.
fn base_canvas(
    plan: &StaticMapPlan,
    field: &ElevationField,
) -> Result<(RgbImage, BaseLayer), ElevationGap> {
    let flat = || ImageBuffer::from_pixel(plan.width, plan.height, PLAIN_BACKGROUND);
    let (columns, rows) = shade_grid(plan.width, plan.height);
    let dem = match dem_over_bbox(field, plan.bbox, columns, rows) {
        Ok(dem) => dem,
        Err(ElevationGap::NoCoverage(_)) => return Ok((flat(), BaseLayer::Plain)),
        Err(gap) => return Err(gap),
    };

    let shade = hillshade(&dem, HILLSHADE_AZIMUTH, HILLSHADE_ALTITUDE);
    let mut canvas = flat();
    for (x, y, pixel) in canvas.enumerate_pixels_mut() {
        let row = (y as usize * rows / plan.height as usize).min(rows - 1);
        let column = (x as usize * columns / plan.width as usize).min(columns - 1);
        let value = shade.get(row, column).unwrap();
        if !shade.is_nodata(value) && value.is_finite() {
            let [red, green, blue, _] = hillshade_color(value);
            *pixel = Rgb([red, green, blue]);
        }
    }
    Ok((canvas, BaseLayer::Hillshade))
}

/// DEM samples per axis the shading is computed on, capped so a large image
/// does not turn into a large DEM read.
fn shade_grid(width: u32, height: u32) -> (usize, usize) {
    let longest = width.max(height) as f64;
    let scale = (MAX_SHADE_SAMPLES as f64 / longest).min(1.0);
    let samples = |side: u32| ((side as f64 * scale).round() as usize).max(MIN_SHADE_SAMPLES);
    (samples(width), samples(height))
}

// ── drawing ─────────────────────────────────────────────────────────────────

fn pixel_of(longitude: f64, latitude: f64, plan: &StaticMapPlan) -> (i32, i32) {
    let [west, south, east, north] = plan.bbox;
    let x = (longitude - west) / (east - west) * plan.width as f64;
    let y = (north - latitude) / (north - south) * plan.height as f64;
    (x as i32, y as i32)
}

fn draw_overlay(canvas: &mut RgbImage, plan: &StaticMapPlan, overlay: &PlannedOverlay) {
    let points: Vec<(i32, i32)> = overlay
        .coordinates
        .iter()
        .map(|&[longitude, latitude]| pixel_of(longitude, latitude, plan))
        .collect();

    if let Some((color, opacity)) = overlay.fill {
        fill_polygon(canvas, &points, color, opacity);
    }

    // a stroke wider than one pixel is drawn by stamping a disc along the line
    let radius = (overlay.stroke_width as i32 - 1) / 2;
    for pair in points.windows(2) {
        draw_line(canvas, pair[0], pair[1], radius, overlay.stroke);
    }
    if overlay.overlay_type == OverlayType::Polygon
        && let (Some(first), Some(last)) = (points.first(), points.last())
    {
        draw_line(canvas, *last, *first, radius, overlay.stroke);
    }
}

fn draw_disc(canvas: &mut RgbImage, cx: i32, cy: i32, radius: i32, color: Rgb<u8>) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                put_pixel(canvas, cx + dx, cy + dy, color);
            }
        }
    }
}

fn draw_line(canvas: &mut RgbImage, from: (i32, i32), to: (i32, i32), radius: i32, color: Rgb<u8>) {
    let (x1, y1) = to;
    let (mut x, mut y) = from;
    let dx = (x1 - x).abs();
    let dy = -(y1 - y).abs();
    let step_x = if x < x1 { 1 } else { -1 };
    let step_y = if y < y1 { 1 } else { -1 };
    let mut error = dx + dy;

    loop {
        draw_disc(canvas, x, y, radius, color);
        if x == x1 && y == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x += step_x;
        }
        if doubled <= dx {
            error += dx;
            y += step_y;
        }
    }
}

/// Even-odd scanline fill: the pixels a ray from the left crosses an odd number
/// of edges to reach are inside.
fn fill_polygon(canvas: &mut RgbImage, points: &[(i32, i32)], color: Rgb<u8>, opacity: f32) {
    if points.len() < 3 {
        return;
    }
    let last_row = canvas.height() as i32 - 1;
    let last_column = canvas.width() as i32 - 1;
    let top = points.iter().map(|p| p.1).min().unwrap_or(0).max(0);
    let bottom = points.iter().map(|p| p.1).max().unwrap_or(0).min(last_row);

    let mut crossings: Vec<f64> = Vec::with_capacity(points.len());
    for row in top..=bottom {
        // the scanline runs through the middle of the pixel row, so a vertex
        // sitting exactly on a row boundary is not counted twice
        let scan = row as f64 + 0.5;
        crossings.clear();
        for edge in 0..points.len() {
            let (x0, y0) = points[edge];
            let (x1, y1) = points[(edge + 1) % points.len()];
            let (y0, y1) = (y0 as f64, y1 as f64);
            if (y0 <= scan) == (y1 <= scan) {
                continue;
            }
            let along = (scan - y0) / (y1 - y0);
            crossings.push(x0 as f64 + along * (x1 - x0) as f64);
        }
        crossings.sort_unstable_by(f64::total_cmp);
        for span in crossings.chunks(2) {
            let [left, right] = span else { continue };
            let from = (left.round() as i32).max(0);
            let to = (right.round() as i32).min(last_column);
            for column in from..=to {
                blend_pixel(canvas, column, row, color, opacity);
            }
        }
    }
}

fn put_pixel(canvas: &mut RgbImage, x: i32, y: i32, color: Rgb<u8>) {
    if x >= 0 && y >= 0 && x < canvas.width() as i32 && y < canvas.height() as i32 {
        canvas.put_pixel(x as u32, y as u32, color);
    }
}

fn blend_pixel(canvas: &mut RgbImage, x: i32, y: i32, color: Rgb<u8>, opacity: f32) {
    if x < 0 || y < 0 || x >= canvas.width() as i32 || y >= canvas.height() as i32 {
        return;
    }
    let under = canvas.get_pixel(x as u32, y as u32).0;
    let mixed: [u8; 3] = std::array::from_fn(|channel| {
        let blended = under[channel] as f32 * (1.0 - opacity) + color.0[channel] as f32 * opacity;
        blended.round() as u8
    });
    canvas.put_pixel(x as u32, y as u32, Rgb(mixed));
}

// ── encoders ────────────────────────────────────────────────────────────────

fn png_bytes(canvas: &RgbImage) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    canvas
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("png encode of an rgb buffer");
    bytes.into_inner()
}

fn jpeg_bytes(canvas: &RgbImage) -> Vec<u8> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(canvas)
        .expect("jpeg encode of an rgb buffer");
    bytes
}

/// Lossless WebP: the image crate's encoder writes no lossy WebP.
fn webp_bytes(canvas: &RgbImage) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    canvas
        .write_to(&mut bytes, image::ImageFormat::WebP)
        .expect("webp encode of an rgb buffer");
    bytes.into_inner()
}

/// A one-page PDF holding the render as a DCTDecode image, sized in points from
/// the pixel dimensions and the dpi.
///
/// Five objects and an xref table, written out here rather than through the
/// report generator's printpdf: this page is one image placed over the whole
/// MediaBox, and printpdf decodes and re-encodes the pixels to get there.
fn pdf_page(jpeg: &[u8], plan: &StaticMapPlan) -> Vec<u8> {
    let scale = POINTS_PER_INCH / plan.dpi as f64;
    let page_width = plan.width as f64 * scale;
    let page_height = plan.height as f64 * scale;

    let mut pdf: Vec<u8> = Vec::with_capacity(jpeg.len() + 1024);
    pdf.extend_from_slice(b"%PDF-1.4\n");
    let mut offsets: Vec<usize> = Vec::with_capacity(5);

    let content = format!("q {page_width:.2} 0 0 {page_height:.2} 0 0 cm /Im0 Do Q\n");
    let dictionaries = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_width:.2} {page_height:.2}] \
             /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
        ),
    ];
    for (index, dictionary) in dictionaries.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{dictionary}\nendobj\n", index + 1).as_bytes());
    }

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        format!(
            "4 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} \
             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            plan.width,
            plan.height,
            jpeg.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(jpeg);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        format!(
            "5 0 obj\n<< /Length {} >>\nstream\n{content}endstream\nendobj\n",
            content.len()
        )
        .as_bytes(),
    );

    let xref_at = pdf.len();
    let object_count = offsets.len() + 1;
    pdf.extend_from_slice(format!("xref\n0 {object_count}\n0000000000 65535 f \n").as_bytes());
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {object_count} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
            .as_bytes(),
    );
    pdf
}

/// Vector markup: the base layer as an embedded PNG, and every marker and
/// overlay as an SVG element. Nothing is referenced from outside the document.
fn svg_document(plan: &StaticMapPlan, base_png: &[u8]) -> Vec<u8> {
    let (width, height) = (plan.width, plan.height);
    let mut svg = String::with_capacity(base_png.len() * 2);
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\">\n"
    ));
    svg.push_str(&format!(
        "<image x=\"0\" y=\"0\" width=\"{width}\" height=\"{height}\" \
         href=\"data:image/png;base64,{}\"/>\n",
        base64(base_png)
    ));

    for overlay in &plan.overlays {
        let points = overlay
            .coordinates
            .iter()
            .map(|&[longitude, latitude]| {
                let (x, y) = pixel_of(longitude, latitude, plan);
                format!("{x},{y}")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let stroke = format!(
            "stroke=\"{}\" stroke-width=\"{}\"",
            hex(overlay.stroke),
            overlay.stroke_width
        );
        match overlay.overlay_type {
            OverlayType::Polyline => svg.push_str(&format!(
                "<polyline points=\"{points}\" fill=\"none\" {stroke}/>\n"
            )),
            OverlayType::Polygon => {
                let fill = match overlay.fill {
                    Some((color, opacity)) => {
                        format!("fill=\"{}\" fill-opacity=\"{opacity}\"", hex(color))
                    }
                    None => "fill=\"none\"".to_string(),
                };
                svg.push_str(&format!("<polygon points=\"{points}\" {fill} {stroke}/>\n"));
            }
        }
    }

    for marker in &plan.markers {
        let (x, y) = pixel_of(marker.longitude, marker.latitude, plan);
        svg.push_str(&format!(
            "<circle cx=\"{x}\" cy=\"{y}\" r=\"{}\" fill=\"{}\"/>\n",
            marker.radius,
            hex(marker.color)
        ));
    }

    svg.push_str("</svg>\n");
    svg.into_bytes()
}

fn hex(color: Rgb<u8>) -> String {
    let [red, green, blue] = color.0;
    format!("#{red:02x}{green:02x}{blue:02x}")
}

/// Standard base64, for the data URI the SVG embeds its base layer with.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let mut packed = 0u32;
        for (index, byte) in group.iter().enumerate() {
            packed |= (*byte as u32) << (16 - 8 * index);
        }
        for index in 0..4 {
            if index <= group.len() {
                let sextet = (packed >> (18 - 6 * index)) & 0x3f;
                encoded.push(ALPHABET[sextet as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(format: ImageFormat) -> StaticMapRequest {
        StaticMapRequest {
            center: None,
            zoom: None,
            bbox: Some([7.0, 43.0, 8.0, 44.0]),
            width: 64,
            height: 64,
            format,
            markers: Vec::new(),
            overlays: Vec::new(),
            dpi: 72,
        }
    }

    #[test]
    fn a_center_and_zoom_name_a_box_of_the_image_aspect() {
        let mut request = request(ImageFormat::Png);
        request.bbox = None;
        request.center = Some([7.5, 43.5]);
        request.zoom = Some(10.0);
        request.width = 200;
        request.height = 100;

        let [west, south, east, north] = request.plan().unwrap().bbox;
        assert!((east - west) / (north - south) - 2.0 < 1e-9);
        assert!(west < 7.5 && east > 7.5);
        assert!(south < 43.5 && north > 43.5);
    }

    #[test]
    fn a_request_with_neither_box_nor_center_is_refused() {
        let mut request = request(ImageFormat::Png);
        request.bbox = None;
        assert!(request.plan().unwrap_err().contains("bbox"));
    }

    #[test]
    fn dimensions_outside_the_cap_are_refused() {
        for (width, height) in [(0, 64), (64, 0), (MAX_IMAGE_SIDE + 1, 64)] {
            let mut request = request(ImageFormat::Png);
            request.width = width;
            request.height = height;
            assert!(
                request.plan().unwrap_err().contains("width and height"),
                "{width}x{height} was accepted"
            );
        }
        let mut widest = request(ImageFormat::Png);
        widest.width = MAX_IMAGE_SIDE;
        widest.height = MAX_IMAGE_SIDE;
        assert!(widest.plan().is_ok());
    }

    #[test]
    fn a_box_that_covers_no_ground_is_refused() {
        for bbox in [
            [8.0, 43.0, 7.0, 44.0],
            [7.0, 44.0, 8.0, 44.0],
            [7.0, 43.0, 181.0, 44.0],
            [f64::NAN, 43.0, 8.0, 44.0],
        ] {
            let mut request = request(ImageFormat::Png);
            request.bbox = Some(bbox);
            assert!(request.plan().is_err(), "{bbox:?} was accepted");
        }
    }

    #[test]
    fn an_unusable_colour_is_refused_rather_than_drawn_grey() {
        let mut request = request(ImageFormat::Png);
        request.markers = vec![MapMarker {
            longitude: 7.5,
            latitude: 43.5,
            color: "puce".into(),
            size: MarkerSize::Small,
        }];
        assert!(request.plan().unwrap_err().contains("hex digits"));
        assert_eq!(parse_color("#ff8000").unwrap(), Rgb([255, 128, 0]));
        assert_eq!(parse_color("ff8000").unwrap(), Rgb([255, 128, 0]));
    }

    #[test]
    fn a_dpi_outside_the_listed_set_is_refused() {
        let mut request = request(ImageFormat::Pdf);
        request.dpi = 1200;
        assert!(request.plan().unwrap_err().contains("dpi"));
        for dpi in ALLOWED_DPI {
            let mut request = request.clone();
            request.dpi = dpi;
            assert!(request.plan().is_ok(), "dpi {dpi} was refused");
        }
    }

    #[test]
    fn only_a_polygon_takes_a_fill() {
        let overlay = MapOverlay {
            overlay_type: OverlayType::Polyline,
            coordinates: vec![[7.1, 43.1], [7.2, 43.2]],
            stroke_color: "#000000".into(),
            stroke_width: 2.0,
            fill_color: Some("#ff0000".into()),
            fill_opacity: None,
        };
        assert!(overlay.planned().unwrap_err().contains("polygon"));
    }

    #[test]
    fn every_format_has_one_name_and_one_content_type() {
        assert_eq!(FORMATS.len(), 5);
        for format in FORMATS {
            assert_eq!(ImageFormat::from_name(format.name()), Some(format));
            assert!(format.content_type().contains('/'));
        }
        assert_eq!(ImageFormat::from_name("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_name("tiff"), None);
    }

    #[test]
    fn a_polygon_fill_covers_its_inside_and_nothing_else() {
        let mut canvas: RgbImage = ImageBuffer::from_pixel(20, 20, Rgb([255, 255, 255]));
        let square = [(5, 5), (15, 5), (15, 15), (5, 15)];
        fill_polygon(&mut canvas, &square, Rgb([255, 0, 0]), 1.0);

        assert_eq!(*canvas.get_pixel(10, 10), Rgb([255, 0, 0]));
        assert_eq!(*canvas.get_pixel(1, 1), Rgb([255, 255, 255]));
        assert_eq!(*canvas.get_pixel(18, 10), Rgb([255, 255, 255]));
    }

    #[test]
    fn a_half_opaque_fill_blends_with_what_is_under_it() {
        let mut canvas: RgbImage = ImageBuffer::from_pixel(20, 20, Rgb([0, 0, 0]));
        fill_polygon(
            &mut canvas,
            &[(5, 5), (15, 5), (15, 15), (5, 15)],
            Rgb([255, 255, 255]),
            0.5,
        );
        assert_eq!(*canvas.get_pixel(10, 10), Rgb([128, 128, 128]));
    }

    #[test]
    fn base64_matches_the_padding_of_each_group_length() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_pdf_page_is_sized_in_points_from_the_dpi() {
        let mut request = request(ImageFormat::Pdf);
        request.width = 300;
        request.height = 150;
        request.dpi = 150;
        let plan = request.plan().unwrap();
        let pdf = pdf_page(b"not-really-a-jpeg", &plan);
        let text = String::from_utf8_lossy(&pdf);

        assert!(text.starts_with("%PDF-"));
        assert!(text.contains("/MediaBox [0 0 144.00 72.00]"), "{text}");
        assert!(text.contains("/Filter /DCTDecode"));
        assert!(text.contains("/Length 17"));
        assert!(text.contains("startxref"));
        assert!(text.trim_end().ends_with("%%EOF"));
    }

    #[test]
    fn the_shade_grid_never_exceeds_the_sample_cap() {
        assert_eq!(shade_grid(64, 64), (64, 64));
        assert_eq!(shade_grid(4096, 2048), (1024, 512));
        assert_eq!(shade_grid(4, 4), (MIN_SHADE_SAMPLES, MIN_SHADE_SAMPLES));
    }
}
