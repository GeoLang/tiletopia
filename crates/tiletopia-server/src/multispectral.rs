//! Multispectral Imagery — NDVI, thermal overlays, band math, spectral indices.
//!
//! Process multi-band drone/satellite imagery for agriculture, environmental
//! monitoring, and construction thermal analysis.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A multispectral image dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultispectralImage {
    pub id: Uuid,
    pub name: String,
    pub bands: Vec<Band>,
    pub width: u32,
    pub height: u32,
    pub gsd_m: f64,       // ground sample distance
    pub bounds: [f64; 4], // [min_x, min_y, max_x, max_y]
    pub capture_date: String,
    pub sensor: String,
}

/// A spectral band.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Band {
    pub index: u8,
    pub name: String,
    pub wavelength_nm: f64,
    pub bandwidth_nm: f64,
    pub radiometric_bits: u8,
}

/// Spectral index type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SpectralIndex {
    /// Normalized Difference Vegetation Index (NIR-Red)/(NIR+Red)
    Ndvi,
    /// Normalized Difference Water Index (Green-NIR)/(Green+NIR)
    Ndwi,
    /// Enhanced Vegetation Index
    Evi,
    /// Soil Adjusted Vegetation Index
    Savi { l_factor: f64 },
    /// Normalized Difference Red Edge (RedEdge-Red)/(RedEdge+Red)
    Ndre,
    /// Green Normalized Difference Vegetation Index
    Gndvi,
    /// Custom band math expression
    Custom { expression: String },
}

/// Index computation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResult {
    pub id: Uuid,
    pub index_type: SpectralIndex,
    pub width: u32,
    pub height: u32,
    pub statistics: IndexStats,
    pub histogram: Vec<HistogramBin>,
    pub classification: Option<IndexClassification>,
}

/// Statistics for computed index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub min_value: f64,
    pub max_value: f64,
    pub mean_value: f64,
    pub std_dev: f64,
    pub valid_pixel_count: u64,
    pub nodata_pixel_count: u64,
}

/// Histogram bin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramBin {
    pub range_start: f64,
    pub range_end: f64,
    pub count: u64,
    pub percentage: f64,
}

/// Classification of index values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexClassification {
    pub classes: Vec<IndexClass>,
}

/// A classification class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexClass {
    pub name: String,
    pub min_value: f64,
    pub max_value: f64,
    pub color: String,
    pub area_m2: f64,
    pub percentage: f64,
}

/// Thermal analysis result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThermalResult {
    pub id: Uuid,
    pub min_temp_c: f64,
    pub max_temp_c: f64,
    pub mean_temp_c: f64,
    pub hotspots: Vec<Hotspot>,
    pub cold_spots: Vec<Hotspot>,
}

/// A thermal anomaly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hotspot {
    pub position: [f64; 2], // [x, y]
    pub temperature_c: f64,
    pub area_m2: f64,
    pub severity: String,
}

/// Compute NDVI from red and NIR bands.
pub fn compute_ndvi(red_band: &[f64], nir_band: &[f64]) -> Vec<f64> {
    red_band
        .iter()
        .zip(nir_band.iter())
        .map(|(r, n)| {
            let sum = n + r;
            if sum.abs() < 1e-10 {
                0.0
            } else {
                (n - r) / sum
            }
        })
        .collect()
}

/// Compute any normalized difference index: (band_a - band_b) / (band_a + band_b).
pub fn normalized_difference(band_a: &[f64], band_b: &[f64]) -> Vec<f64> {
    band_a
        .iter()
        .zip(band_b.iter())
        .map(|(a, b)| {
            let sum = a + b;
            if sum.abs() < 1e-10 {
                0.0
            } else {
                (a - b) / sum
            }
        })
        .collect()
}

/// Compute Enhanced Vegetation Index.
pub fn compute_evi(nir: &[f64], red: &[f64], blue: &[f64]) -> Vec<f64> {
    let g = 2.5;
    let c1 = 6.0;
    let c2 = 7.5;
    let l = 1.0;

    nir.iter()
        .zip(red.iter())
        .zip(blue.iter())
        .map(|((n, r), b)| {
            let denom = n + c1 * r - c2 * b + l;
            if denom.abs() < 1e-10 {
                0.0
            } else {
                g * (n - r) / denom
            }
        })
        .collect()
}

/// Compute SAVI (Soil Adjusted Vegetation Index).
pub fn compute_savi(nir: &[f64], red: &[f64], l_factor: f64) -> Vec<f64> {
    nir.iter()
        .zip(red.iter())
        .map(|(n, r)| {
            let denom = n + r + l_factor;
            if denom.abs() < 1e-10 {
                0.0
            } else {
                ((n - r) / denom) * (1.0 + l_factor)
            }
        })
        .collect()
}

/// Classify NDVI values into vegetation health categories.
pub fn classify_ndvi(ndvi_values: &[f64], pixel_area_m2: f64) -> IndexClassification {
    let classes_def = [
        ("Water/Shadow", -1.0, -0.1, "#1a237e"),
        ("Bare Soil", -0.1, 0.1, "#8d6e63"),
        ("Sparse Vegetation", 0.1, 0.3, "#c8e6c9"),
        ("Moderate Vegetation", 0.3, 0.6, "#4caf50"),
        ("Dense Vegetation", 0.6, 1.0, "#1b5e20"),
    ];

    let total = ndvi_values.len() as f64;
    let classes = classes_def
        .iter()
        .map(|(name, min, max, color)| {
            let count = ndvi_values
                .iter()
                .filter(|v| **v >= *min && **v < *max)
                .count() as f64;
            IndexClass {
                name: name.to_string(),
                min_value: *min,
                max_value: *max,
                color: color.to_string(),
                area_m2: count * pixel_area_m2,
                percentage: if total > 0.0 {
                    count / total * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect();

    IndexClassification { classes }
}

/// Detect thermal anomalies (hotspots/cold spots).
pub fn detect_thermal_anomalies(
    thermal_band: &[f64],
    width: u32,
    pixel_size_m: f64,
    threshold_std: f64,
) -> ThermalResult {
    let n = thermal_band.len();
    if n == 0 {
        return ThermalResult {
            id: Uuid::new_v4(),
            min_temp_c: 0.0,
            max_temp_c: 0.0,
            mean_temp_c: 0.0,
            hotspots: vec![],
            cold_spots: vec![],
        };
    }

    let mean = thermal_band.iter().sum::<f64>() / n as f64;
    let std_dev = (thermal_band.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let min_temp = thermal_band.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_temp = thermal_band
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);

    let hot_threshold = mean + threshold_std * std_dev;
    let cold_threshold = mean - threshold_std * std_dev;

    let mut hotspots = Vec::new();
    let mut cold_spots = Vec::new();

    for (i, &temp) in thermal_band.iter().enumerate() {
        let x = (i as u32 % width) as f64 * pixel_size_m;
        let y = (i as u32 / width) as f64 * pixel_size_m;

        if temp > hot_threshold {
            let severity = if temp > mean + 3.0 * std_dev {
                "critical"
            } else if temp > mean + 2.0 * std_dev {
                "high"
            } else {
                "moderate"
            };
            hotspots.push(Hotspot {
                position: [x, y],
                temperature_c: temp,
                area_m2: pixel_size_m * pixel_size_m,
                severity: severity.into(),
            });
        } else if temp < cold_threshold {
            cold_spots.push(Hotspot {
                position: [x, y],
                temperature_c: temp,
                area_m2: pixel_size_m * pixel_size_m,
                severity: "low".into(),
            });
        }
    }

    ThermalResult {
        id: Uuid::new_v4(),
        min_temp_c: min_temp,
        max_temp_c: max_temp,
        mean_temp_c: mean,
        hotspots,
        cold_spots,
    }
}

/// Simple band math (addition, subtraction, ratios between two bands).
pub fn band_math(band_a: &[f64], band_b: &[f64], operation: &str) -> Vec<f64> {
    band_a
        .iter()
        .zip(band_b.iter())
        .map(|(a, b)| match operation {
            "add" => a + b,
            "subtract" => a - b,
            "multiply" => a * b,
            "divide" => {
                if b.abs() < 1e-10 {
                    0.0
                } else {
                    a / b
                }
            }
            "ratio" => {
                if b.abs() < 1e-10 {
                    0.0
                } else {
                    a / b
                }
            }
            _ => 0.0,
        })
        .collect()
}

/// List supported spectral indices.
pub fn supported_indices() -> Vec<&'static str> {
    vec!["NDVI", "NDWI", "EVI", "SAVI", "NDRE", "GNDVI", "Custom"]
}

/// List common multispectral sensors.
pub fn supported_sensors() -> Vec<&'static str> {
    vec![
        "DJI Phantom 4 Multispectral",
        "MicaSense RedEdge-MX",
        "MicaSense Altum",
        "Parrot Sequoia+",
        "Sentera 6X",
        "FLIR Vue Pro R",
        "DJI Zenmuse H20T",
        "Sentinel-2 MSI",
        "Landsat 8 OLI",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_ndvi() {
        let red = vec![0.1, 0.2, 0.3, 0.05];
        let nir = vec![0.5, 0.4, 0.3, 0.8];
        let ndvi = compute_ndvi(&red, &nir);
        assert_eq!(ndvi.len(), 4);
        // NDVI = (NIR-Red)/(NIR+Red)
        assert!((ndvi[0] - (0.5 - 0.1) / (0.5 + 0.1)).abs() < 0.001);
        assert!(ndvi[0] > 0.0); // vegetation
        assert!((ndvi[2]).abs() < 0.01); // equal = bare soil
    }

    #[test]
    fn test_normalized_difference() {
        let a = vec![10.0, 5.0];
        let b = vec![5.0, 10.0];
        let nd = normalized_difference(&a, &b);
        assert!((nd[0] - (10.0 - 5.0) / (10.0 + 5.0)).abs() < 0.001);
        assert!((nd[1] - (5.0 - 10.0) / (5.0 + 10.0)).abs() < 0.001);
    }

    #[test]
    fn test_compute_evi() {
        let nir = vec![0.5, 0.6];
        let red = vec![0.1, 0.15];
        let blue = vec![0.05, 0.08];
        let evi = compute_evi(&nir, &red, &blue);
        assert_eq!(evi.len(), 2);
        assert!(evi[0] > 0.0);
    }

    #[test]
    fn test_classify_ndvi() {
        let ndvi = vec![-0.5, 0.0, 0.2, 0.4, 0.7];
        let classification = classify_ndvi(&ndvi, 0.25);
        assert_eq!(classification.classes.len(), 5);
        let total_pct: f64 = classification.classes.iter().map(|c| c.percentage).sum();
        assert!((total_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn test_detect_thermal_anomalies() {
        // Uniform with one hot pixel
        let mut thermal = vec![25.0; 25];
        thermal[12] = 45.0; // hot spot in center
        let result = detect_thermal_anomalies(&thermal, 5, 0.1, 1.5);
        assert!(!result.hotspots.is_empty());
        assert!(result.max_temp_c > 40.0);
    }

    #[test]
    fn test_band_math() {
        let a = vec![10.0, 20.0, 30.0];
        let b = vec![2.0, 4.0, 5.0];
        let ratio = band_math(&a, &b, "divide");
        assert!((ratio[0] - 5.0).abs() < 0.01);
        assert!((ratio[1] - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_supported_indices() {
        assert_eq!(supported_indices().len(), 7);
    }
}
