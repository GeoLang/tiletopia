//! Cloud Optimized GeoTIFF (COG) serving.
//!
//! Supports range-request–friendly GeoTIFF files with internal tiling
//! and overview levels. Provides tile indexing and metadata extraction.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A registered COG dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CogDataset {
    pub id: Uuid,
    pub name: String,
    pub href: String, // URL or path to the COG file
    pub file_size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub bands: Vec<BandInfo>,
    pub crs: String,          // e.g., "EPSG:4326"
    pub bbox: [f64; 4],       // [west, south, east, north]
    pub pixel_size: [f64; 2], // [x_resolution, y_resolution] in CRS units
    pub overviews: Vec<OverviewLevel>,
    pub tile_size: [u32; 2], // [width, height] of internal tiles
    pub compression: CogCompression,
    pub nodata_value: Option<f64>,
}

/// Band metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandInfo {
    pub index: u8,
    pub name: Option<String>,
    pub data_type: CogDataType,
    pub color_interp: Option<ColorInterpretation>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub statistics: Option<BandStatistics>,
}

/// Data type for a band.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CogDataType {
    UInt8,
    UInt16,
    Int16,
    UInt32,
    Int32,
    Float32,
    Float64,
}

/// Color interpretation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ColorInterpretation {
    Red,
    Green,
    Blue,
    Alpha,
    Gray,
    Palette,
    Undefined,
}

/// Band statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandStatistics {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub stddev: f64,
}

/// Compression method used in the COG.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CogCompression {
    None,
    Deflate,
    Lzw,
    Zstd,
    Jpeg,
    Webp,
    Lerc,
}

/// Overview (reduced resolution) level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewLevel {
    pub level: u8,
    pub width: u32,
    pub height: u32,
    pub scale_factor: u32, // 2, 4, 8, etc.
}

/// A tile request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileRequest {
    pub dataset_id: Uuid,
    pub z: u8,  // zoom level
    pub x: u32, // tile column
    pub y: u32, // tile row
    pub format: TileFormat,
}

/// Output tile format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TileFormat {
    Png,
    Jpeg,
    Webp,
    Tiff,
}

/// Tile metadata (byte range info for HTTP range requests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileIndex {
    pub offset: u64,
    pub length: u32,
    pub overview_level: u8,
}

/// COG serving engine.
pub struct CogEngine {
    datasets: Vec<CogDataset>,
}

impl CogEngine {
    /// Create engine with demo data.
    pub fn new() -> Self {
        Self {
            datasets: demo_datasets(),
        }
    }

    /// List all registered COG datasets.
    pub fn list_datasets(&self) -> &[CogDataset] {
        &self.datasets
    }

    /// Get dataset by ID.
    pub fn get_dataset(&self, id: Uuid) -> Option<&CogDataset> {
        self.datasets.iter().find(|d| d.id == id)
    }

    /// Compute the byte range for a specific tile (simulated).
    pub fn get_tile_index(&self, req: &TileRequest) -> Option<TileIndex> {
        let ds = self.get_dataset(req.dataset_id)?;
        let tiles_per_row = ds.width / ds.tile_size[0];
        let tile_offset = (req.y as u64 * tiles_per_row as u64 + req.x as u64) * 65536;
        Some(TileIndex {
            offset: 8 + tile_offset, // 8 bytes for TIFF header
            length: 65536,
            overview_level: req.z,
        })
    }

    /// Get available zoom levels for a dataset.
    pub fn available_zooms(&self, id: Uuid) -> Option<Vec<u8>> {
        let ds = self.get_dataset(id)?;
        let mut zooms: Vec<u8> = ds.overviews.iter().map(|o| o.level).collect();
        zooms.push(0); // full resolution
        zooms.sort();
        Some(zooms)
    }
}

impl Default for CogEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Demo COG datasets.
fn demo_datasets() -> Vec<CogDataset> {
    vec![
        CogDataset {
            id: Uuid::new_v4(),
            name: "San Francisco Orthophoto 2024".into(),
            href: "s3://tiletopia-data/cog/sf_ortho_2024.tif".into(),
            file_size_bytes: 4_800_000_000,
            width: 120000,
            height: 90000,
            bands: vec![
                BandInfo {
                    index: 1,
                    name: Some("Red".into()),
                    data_type: CogDataType::UInt8,
                    color_interp: Some(ColorInterpretation::Red),
                    min_value: Some(0.0),
                    max_value: Some(255.0),
                    statistics: Some(BandStatistics {
                        min: 0.0,
                        max: 255.0,
                        mean: 128.5,
                        stddev: 54.2,
                    }),
                },
                BandInfo {
                    index: 2,
                    name: Some("Green".into()),
                    data_type: CogDataType::UInt8,
                    color_interp: Some(ColorInterpretation::Green),
                    min_value: Some(0.0),
                    max_value: Some(255.0),
                    statistics: Some(BandStatistics {
                        min: 0.0,
                        max: 255.0,
                        mean: 135.2,
                        stddev: 48.7,
                    }),
                },
                BandInfo {
                    index: 3,
                    name: Some("Blue".into()),
                    data_type: CogDataType::UInt8,
                    color_interp: Some(ColorInterpretation::Blue),
                    min_value: Some(0.0),
                    max_value: Some(255.0),
                    statistics: Some(BandStatistics {
                        min: 0.0,
                        max: 255.0,
                        mean: 121.8,
                        stddev: 51.3,
                    }),
                },
            ],
            crs: "EPSG:32610".into(),
            bbox: [-122.52, 37.70, -122.35, 37.82],
            pixel_size: [0.1, -0.1],
            overviews: vec![
                OverviewLevel {
                    level: 1,
                    width: 60000,
                    height: 45000,
                    scale_factor: 2,
                },
                OverviewLevel {
                    level: 2,
                    width: 30000,
                    height: 22500,
                    scale_factor: 4,
                },
                OverviewLevel {
                    level: 3,
                    width: 15000,
                    height: 11250,
                    scale_factor: 8,
                },
                OverviewLevel {
                    level: 4,
                    width: 7500,
                    height: 5625,
                    scale_factor: 16,
                },
            ],
            tile_size: [512, 512],
            compression: CogCompression::Deflate,
            nodata_value: None,
        },
        CogDataset {
            id: Uuid::new_v4(),
            name: "California DEM 10m".into(),
            href: "s3://tiletopia-data/cog/ca_dem_10m.tif".into(),
            file_size_bytes: 2_100_000_000,
            width: 50000,
            height: 60000,
            bands: vec![BandInfo {
                index: 1,
                name: Some("Elevation".into()),
                data_type: CogDataType::Float32,
                color_interp: Some(ColorInterpretation::Gray),
                min_value: Some(-85.0),
                max_value: Some(4421.0),
                statistics: Some(BandStatistics {
                    min: -85.0,
                    max: 4421.0,
                    mean: 842.3,
                    stddev: 612.7,
                }),
            }],
            crs: "EPSG:4326".into(),
            bbox: [-124.48, 32.53, -114.13, 42.01],
            pixel_size: [0.0001, -0.0001],
            overviews: vec![
                OverviewLevel {
                    level: 1,
                    width: 25000,
                    height: 30000,
                    scale_factor: 2,
                },
                OverviewLevel {
                    level: 2,
                    width: 12500,
                    height: 15000,
                    scale_factor: 4,
                },
                OverviewLevel {
                    level: 3,
                    width: 6250,
                    height: 7500,
                    scale_factor: 8,
                },
            ],
            tile_size: [256, 256],
            compression: CogCompression::Zstd,
            nodata_value: Some(-9999.0),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cog_engine_list() {
        let engine = CogEngine::new();
        let datasets = engine.list_datasets();
        assert_eq!(datasets.len(), 2);
    }

    #[test]
    fn test_cog_tile_index() {
        let engine = CogEngine::new();
        let ds_id = engine.list_datasets()[0].id;
        let req = TileRequest {
            dataset_id: ds_id,
            z: 0,
            x: 1,
            y: 0,
            format: TileFormat::Png,
        };
        let idx = engine.get_tile_index(&req).unwrap();
        assert_eq!(idx.length, 65536);
        assert!(idx.offset > 0);
    }

    #[test]
    fn test_available_zooms() {
        let engine = CogEngine::new();
        let ds_id = engine.list_datasets()[1].id;
        let zooms = engine.available_zooms(ds_id).unwrap();
        assert_eq!(zooms.len(), 4); // 3 overviews + full res
    }
}
