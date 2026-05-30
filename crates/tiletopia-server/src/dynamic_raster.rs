//! Dynamic raster tiles — on-the-fly tile rendering from GeoTIFF/COG sources.
//!
//! TiTiler-like functionality: serve any GeoTIFF as styled XYZ/WMTS tiles
//! with dynamic rescaling, colormap application, and band math.

use serde::{Deserialize, Serialize};

/// Request for a dynamic raster tile.
#[derive(Debug, Clone, Deserialize)]
pub struct DynamicTileRequest {
    pub dataset_id: String,
    pub z: u32,
    pub x: u32,
    pub y: u32,
    pub bands: Option<Vec<u32>>,
    pub rescale: Option<Vec<RescaleRange>>,
    pub colormap: Option<ColormapName>,
    pub expression: Option<String>,
    pub tile_size: Option<u32>,
    pub resampling: Option<Resampling>,
    pub return_mask: Option<bool>,
}

/// Min/max rescale range per band.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct RescaleRange {
    pub min: f64,
    pub max: f64,
}

/// Supported colormaps.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum ColormapName {
    Viridis,
    Plasma,
    Inferno,
    Magma,
    Terrain,
    Spectral,
    RdYlGn,
    Blues,
    Greens,
    Reds,
    Greys,
    Hot,
    Cool,
    Jet,
    Rainbow,
    Custom,
}

/// Resampling method for tile rendering.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Resampling {
    #[default]
    Nearest,
    Bilinear,
    Cubic,
    Average,
    Lanczos,
}

/// Response containing rendered tile data.
#[derive(Debug, Clone)]
pub struct DynamicTileResponse {
    pub data: Vec<u8>,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
}

/// Statistics for a raster band (used for auto-rescaling).
#[derive(Debug, Clone, Serialize)]
pub struct BandStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub std_dev: f64,
    pub percentile_2: f64,
    pub percentile_98: f64,
    pub valid_pixels: u64,
    pub nodata_pixels: u64,
}

/// Compute statistics from a band of data.
pub fn compute_band_stats(data: &[f64], nodata: f64) -> BandStats {
    let valid: Vec<f64> = data
        .iter()
        .copied()
        .filter(|&v| v.is_finite() && (v - nodata).abs() > f64::EPSILON)
        .collect();

    let n = valid.len();
    let nodata_pixels = (data.len() - n) as u64;

    if n == 0 {
        return BandStats {
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            std_dev: 0.0,
            percentile_2: 0.0,
            percentile_98: 0.0,
            valid_pixels: 0,
            nodata_pixels,
        };
    }

    let mut sorted = valid.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let min = sorted[0];
    let max = sorted[n - 1];
    let mean = valid.iter().sum::<f64>() / n as f64;
    let variance = valid.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    let std_dev = variance.sqrt();
    let p2_idx = (n as f64 * 0.02) as usize;
    let p98_idx = ((n as f64 * 0.98) as usize).min(n - 1);

    BandStats {
        min,
        max,
        mean,
        std_dev,
        percentile_2: sorted[p2_idx],
        percentile_98: sorted[p98_idx],
        valid_pixels: n as u64,
        nodata_pixels,
    }
}

/// Rescale values from [src_min, src_max] to [0, 255] (u8 range).
pub fn rescale_to_u8(data: &[f64], range: &RescaleRange) -> Vec<u8> {
    let span = range.max - range.min;
    if span.abs() < f64::EPSILON {
        return vec![0u8; data.len()];
    }
    data.iter()
        .map(|&v| {
            let normalized = (v - range.min) / span;
            (normalized.clamp(0.0, 1.0) * 255.0) as u8
        })
        .collect()
}

/// Apply a colormap to single-band u8 data, producing RGBA pixels.
pub fn apply_colormap(data: &[u8], colormap: ColormapName) -> Vec<u8> {
    let lut = colormap_lut(colormap);
    let mut rgba = Vec::with_capacity(data.len() * 4);
    for &v in data {
        let idx = v as usize;
        rgba.push(lut[idx * 3]);
        rgba.push(lut[idx * 3 + 1]);
        rgba.push(lut[idx * 3 + 2]);
        rgba.push(255);
    }
    rgba
}

/// Generate a 256-entry colormap lookup table (768 bytes: R,G,B for each of 256 values).
fn colormap_lut(name: ColormapName) -> Vec<u8> {
    let mut lut = vec![0u8; 768];
    match name {
        ColormapName::Viridis => {
            for i in 0..256 {
                let t = i as f64 / 255.0;
                lut[i * 3] = (68.0 + t * (253.0 - 68.0) * (1.0 - t) + t * t * 231.0) as u8;
                lut[i * 3 + 1] = (1.0 + t * 215.0) as u8;
                lut[i * 3 + 2] = (84.0 + t * (37.0 - 84.0) * t + (1.0 - t) * t * 170.0) as u8;
            }
        }
        ColormapName::Terrain => {
            for i in 0..256 {
                let t = i as f64 / 255.0;
                if t < 0.25 {
                    let s = t / 0.25;
                    lut[i * 3] = (0.0 + s * 0.0) as u8;
                    lut[i * 3 + 1] = (100.0 + s * 155.0) as u8;
                    lut[i * 3 + 2] = (200.0 - s * 200.0) as u8;
                } else if t < 0.5 {
                    let s = (t - 0.25) / 0.25;
                    lut[i * 3] = (s * 200.0) as u8;
                    lut[i * 3 + 1] = (255.0 - s * 55.0) as u8;
                    lut[i * 3 + 2] = 0;
                } else if t < 0.75 {
                    let s = (t - 0.5) / 0.25;
                    lut[i * 3] = (200.0 + s * 55.0) as u8;
                    lut[i * 3 + 1] = (200.0 - s * 100.0) as u8;
                    lut[i * 3 + 2] = (s * 100.0) as u8;
                } else {
                    let s = (t - 0.75) / 0.25;
                    lut[i * 3] = 255;
                    lut[i * 3 + 1] = (100.0 + s * 155.0) as u8;
                    lut[i * 3 + 2] = (100.0 + s * 155.0) as u8;
                }
            }
        }
        ColormapName::Greys => {
            for i in 0..256 {
                lut[i * 3] = i as u8;
                lut[i * 3 + 1] = i as u8;
                lut[i * 3 + 2] = i as u8;
            }
        }
        _ => {
            // Default: greyscale fallback for unimplemented colormaps
            for i in 0..256 {
                lut[i * 3] = i as u8;
                lut[i * 3 + 1] = i as u8;
                lut[i * 3 + 2] = i as u8;
            }
        }
    }
    lut
}

/// Evaluate a simple band math expression on pixel values.
/// Supports: b1, b2, b3, ..., +, -, *, /, (, )
/// Example: "(b4 - b3) / (b4 + b3)" for NDVI.
pub fn evaluate_band_expression(
    expression: &str,
    bands: &[&[f64]],
    num_pixels: usize,
) -> Result<Vec<f64>, String> {
    // Simple expression evaluator for common patterns
    let expr = expression.trim();

    // Parse band references
    let band_refs: Vec<(usize, String)> = (1..=bands.len())
        .map(|i| (i - 1, format!("b{i}")))
        .filter(|(_, name)| expr.contains(name.as_str()))
        .collect();

    if band_refs.is_empty() {
        return Err("no band references found in expression".to_string());
    }

    // Common patterns (fast path)
    if let Some(result) = try_ndvi_pattern(expr, bands, num_pixels) {
        return Ok(result);
    }

    // Fallback: evaluate per-pixel
    let mut result = Vec::with_capacity(num_pixels);
    for pixel_idx in 0..num_pixels {
        let mut eval_expr = expr.to_string();
        for i in (0..bands.len()).rev() {
            let name = format!("b{}", i + 1);
            let val = bands[i].get(pixel_idx).copied().unwrap_or(0.0);
            eval_expr = eval_expr.replace(&name, &val.to_string());
        }
        let val = simple_eval(&eval_expr).unwrap_or(0.0);
        result.push(val);
    }
    Ok(result)
}

/// Fast path for (bX - bY) / (bX + bY) pattern (NDVI, NDWI, NBR, etc.)
fn try_ndvi_pattern(expr: &str, bands: &[&[f64]], num_pixels: usize) -> Option<Vec<f64>> {
    // Match pattern: (bX - bY) / (bX + bY)
    let expr = expr.replace(' ', "");
    if !expr.contains(")/(") {
        return None;
    }
    let parts: Vec<&str> = expr.split(")/(").collect();
    if parts.len() != 2 {
        return None;
    }
    let left = parts[0].trim_start_matches('(');
    let right = parts[1].trim_end_matches(')');

    // Parse "bX-bY" from left and "bX+bY" from right
    let (l_a, l_b) = parse_band_op(left, '-')?;
    let (r_a, r_b) = parse_band_op(right, '+')?;

    if l_a != r_a || l_b != r_b {
        return None;
    }

    let band_a = bands.get(l_a)?;
    let band_b = bands.get(l_b)?;

    let result: Vec<f64> = (0..num_pixels)
        .map(|i| {
            let a = band_a.get(i).copied().unwrap_or(0.0);
            let b = band_b.get(i).copied().unwrap_or(0.0);
            let sum = a + b;
            if sum.abs() < f64::EPSILON {
                0.0
            } else {
                (a - b) / sum
            }
        })
        .collect();

    Some(result)
}

fn parse_band_op(s: &str, op: char) -> Option<(usize, usize)> {
    let parts: Vec<&str> = s.split(op).collect();
    if parts.len() != 2 {
        return None;
    }
    let a = parts[0].trim_start_matches('b').parse::<usize>().ok()? - 1;
    let b = parts[1].trim_start_matches('b').parse::<usize>().ok()? - 1;
    Some((a, b))
}

/// Minimal arithmetic expression evaluator.
fn simple_eval(expr: &str) -> Option<f64> {
    let expr = expr.trim();
    // Try parsing as number
    if let Ok(v) = expr.parse::<f64>() {
        return Some(v);
    }
    // Find last + or - not inside parens
    let mut depth = 0i32;
    let mut last_add = None;
    let bytes = expr.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'+' | b'-' if depth == 0 && i > 0 => last_add = Some(i),
            _ => {}
        }
    }
    if let Some(pos) = last_add {
        let left = simple_eval(&expr[..pos])?;
        let op = bytes[pos];
        let right = simple_eval(&expr[pos + 1..])?;
        return Some(if op == b'+' {
            left + right
        } else {
            left - right
        });
    }
    // Find last * or / not inside parens
    let mut last_mul = None;
    depth = 0;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'*' | b'/' if depth == 0 => last_mul = Some(i),
            _ => {}
        }
    }
    if let Some(pos) = last_mul {
        let left = simple_eval(&expr[..pos])?;
        let op = bytes[pos];
        let right = simple_eval(&expr[pos + 1..])?;
        return Some(if op == b'*' {
            left * right
        } else if right.abs() > f64::EPSILON {
            left / right
        } else {
            0.0
        });
    }
    // Strip parens
    if expr.starts_with('(') && expr.ends_with(')') {
        return simple_eval(&expr[1..expr.len() - 1]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_band_stats() {
        let data = vec![1.0, 2.0, 3.0, 4.0, 5.0, -9999.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let stats = compute_band_stats(&data, -9999.0);
        assert_eq!(stats.valid_pixels, 10);
        assert_eq!(stats.nodata_pixels, 1);
        assert!((stats.min - 1.0).abs() < 0.01);
        assert!((stats.max - 10.0).abs() < 0.01);
        assert!((stats.mean - 5.5).abs() < 0.01);
    }

    #[test]
    fn test_rescale_to_u8() {
        let data = vec![0.0, 0.5, 1.0, 2.0, -1.0];
        let range = RescaleRange { min: 0.0, max: 1.0 };
        let result = rescale_to_u8(&data, &range);
        assert_eq!(result[0], 0);
        assert_eq!(result[1], 127); // ~0.5 * 255
        assert_eq!(result[2], 255);
        assert_eq!(result[3], 255); // clamped
        assert_eq!(result[4], 0); // clamped
    }

    #[test]
    fn test_colormap_greyscale() {
        let data = vec![0, 128, 255];
        let rgba = apply_colormap(&data, ColormapName::Greys);
        assert_eq!(rgba.len(), 12); // 3 pixels * 4 bytes
        assert_eq!(rgba[0], 0); // R
        assert_eq!(rgba[4], 128); // R of second pixel
        assert_eq!(rgba[8], 255); // R of third pixel
    }

    #[test]
    fn test_ndvi_expression() {
        let nir = vec![0.5, 0.8, 0.3];
        let red = vec![0.1, 0.2, 0.3];
        let result = evaluate_band_expression("(b1 - b2) / (b1 + b2)", &[&nir, &red], 3).unwrap();
        assert!((result[0] - 0.6667).abs() < 0.01);
        assert!((result[1] - 0.6).abs() < 0.01);
        assert!((result[2] - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_simple_eval() {
        assert!((simple_eval("3+4").unwrap() - 7.0).abs() < 0.01);
        assert!((simple_eval("10/2").unwrap() - 5.0).abs() < 0.01);
        assert!((simple_eval("(2+3)*4").unwrap() - 20.0).abs() < 0.01);
    }
}
