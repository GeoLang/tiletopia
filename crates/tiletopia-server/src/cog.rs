//! Cloud Optimized GeoTIFF (COG) serving.
//!
//! Supports range-request–friendly GeoTIFF files with internal tiling
//! and overview levels. Provides tile indexing and metadata extraction.

use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
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

/// Error type for COG operations.
#[derive(Debug, thiserror::Error)]
pub enum CogError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("TIFF decode error: {0}")]
    Tiff(String),
    #[error("Not a valid COG: {0}")]
    InvalidCog(String),
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

    /// Register a dataset at runtime.
    pub fn register_dataset(&mut self, dataset: CogDataset) {
        self.datasets.push(dataset);
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

// TIFF tag constants
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_BITS_PER_SAMPLE: u16 = 258;

/// Parse TIFF tile offsets and byte counts from a reader.
/// Returns a vector of TileIndex entries with real byte offsets.
pub fn parse_tile_offsets<R: Read + Seek>(reader: &mut R) -> Result<Vec<TileIndex>, CogError> {
    // Read byte order marker
    let mut endian_bytes = [0u8; 2];
    reader.read_exact(&mut endian_bytes)?;
    let little_endian = match &endian_bytes {
        b"II" => true,
        b"MM" => false,
        _ => return Err(CogError::InvalidCog("invalid byte order marker".into())),
    };

    // Read magic number (42)
    let magic = read_u16(reader, little_endian)?;
    if magic != 42 {
        return Err(CogError::InvalidCog(format!(
            "expected TIFF magic 42, got {magic}"
        )));
    }

    // Read IFD offset
    let ifd_offset = read_u32(reader, little_endian)?;
    reader.seek(SeekFrom::Start(ifd_offset as u64))?;

    // Read number of IFD entries
    let entry_count = read_u16(reader, little_endian)?;

    let mut tile_offsets: Vec<u64> = Vec::new();
    let mut tile_byte_counts: Vec<u32> = Vec::new();

    for _ in 0..entry_count {
        let tag = read_u16(reader, little_endian)?;
        let field_type = read_u16(reader, little_endian)?;
        let count = read_u32(reader, little_endian)?;
        let value_offset_bytes = read_u32(reader, little_endian)?;

        match tag {
            TAG_TILE_OFFSETS => {
                let saved_pos = reader.stream_position()?;
                tile_offsets =
                    read_long_array(reader, little_endian, field_type, count, value_offset_bytes)?;
                reader.seek(SeekFrom::Start(saved_pos))?;
            }
            TAG_TILE_BYTE_COUNTS => {
                let saved_pos = reader.stream_position()?;
                tile_byte_counts = read_short_or_long_array(
                    reader,
                    little_endian,
                    field_type,
                    count,
                    value_offset_bytes,
                )?;
                reader.seek(SeekFrom::Start(saved_pos))?;
            }
            _ => {}
        }
    }

    if tile_offsets.is_empty() {
        return Err(CogError::InvalidCog("no TileOffsets tag found".into()));
    }
    if tile_byte_counts.len() != tile_offsets.len() {
        return Err(CogError::InvalidCog(
            "TileOffsets and TileByteCounts count mismatch".into(),
        ));
    }

    let entries: Vec<TileIndex> = tile_offsets
        .iter()
        .zip(tile_byte_counts.iter())
        .map(|(&offset, &length)| TileIndex {
            offset,
            length,
            overview_level: 0,
        })
        .collect();

    Ok(entries)
}

/// Parse a COG file to extract metadata and tile index.
pub fn parse_cog_file(path: &std::path::Path) -> Result<CogDataset, CogError> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);

    // Read byte order
    let mut endian_bytes = [0u8; 2];
    reader.read_exact(&mut endian_bytes)?;
    let little_endian = match &endian_bytes {
        b"II" => true,
        b"MM" => false,
        _ => return Err(CogError::InvalidCog("invalid byte order marker".into())),
    };

    let magic = read_u16(&mut reader, little_endian)?;
    if magic != 42 {
        return Err(CogError::InvalidCog(format!(
            "expected TIFF magic 42, got {magic}"
        )));
    }

    let ifd_offset = read_u32(&mut reader, little_endian)?;
    reader.seek(SeekFrom::Start(ifd_offset as u64))?;

    let entry_count = read_u16(&mut reader, little_endian)?;

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut tile_width: u32 = 256;
    let mut tile_height: u32 = 256;
    let mut samples_per_pixel: u16 = 1;
    let mut bits_per_sample: u16 = 8;

    for _ in 0..entry_count {
        let tag = read_u16(&mut reader, little_endian)?;
        let _field_type = read_u16(&mut reader, little_endian)?;
        let _count = read_u32(&mut reader, little_endian)?;
        let value = read_u32(&mut reader, little_endian)?;

        match tag {
            TAG_IMAGE_WIDTH => width = value,
            TAG_IMAGE_LENGTH => height = value,
            TAG_TILE_WIDTH => tile_width = value,
            TAG_TILE_LENGTH => tile_height = value,
            TAG_SAMPLES_PER_PIXEL => samples_per_pixel = value as u16,
            TAG_BITS_PER_SAMPLE => bits_per_sample = value as u16,
            _ => {}
        }
    }

    let data_type = match bits_per_sample {
        8 => CogDataType::UInt8,
        16 => CogDataType::UInt16,
        32 => CogDataType::Float32,
        64 => CogDataType::Float64,
        _ => CogDataType::UInt8,
    };

    let bands: Vec<BandInfo> = (0..samples_per_pixel)
        .map(|i| BandInfo {
            index: i as u8 + 1,
            name: None,
            data_type: data_type.clone(),
            color_interp: None,
            min_value: None,
            max_value: None,
            statistics: None,
        })
        .collect();

    let file_size = std::fs::metadata(path)?.len();

    Ok(CogDataset {
        id: Uuid::new_v4(),
        name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string(),
        href: path.display().to_string(),
        file_size_bytes: file_size,
        width,
        height,
        bands,
        crs: "EPSG:4326".into(),
        bbox: [0.0, 0.0, 0.0, 0.0],
        pixel_size: [1.0, -1.0],
        overviews: vec![],
        tile_size: [tile_width, tile_height],
        compression: CogCompression::None,
        nodata_value: None,
    })
}

fn read_u16<R: Read>(reader: &mut R, little_endian: bool) -> Result<u16, std::io::Error> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(if little_endian {
        u16::from_le_bytes(buf)
    } else {
        u16::from_be_bytes(buf)
    })
}

fn read_u32<R: Read>(reader: &mut R, little_endian: bool) -> Result<u32, std::io::Error> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(if little_endian {
        u32::from_le_bytes(buf)
    } else {
        u32::from_be_bytes(buf)
    })
}

/// Read an array of LONG (u32) values, returned as u64 for offset compatibility.
fn read_long_array<R: Read + Seek>(
    reader: &mut R,
    little_endian: bool,
    field_type: u16,
    count: u32,
    value_offset: u32,
) -> Result<Vec<u64>, CogError> {
    let _ = field_type;
    if count == 1 {
        return Ok(vec![value_offset as u64]);
    }
    reader.seek(SeekFrom::Start(value_offset as u64))?;
    let mut values = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let v = read_u32(reader, little_endian).map_err(CogError::Io)?;
        values.push(v as u64);
    }
    Ok(values)
}

/// Read an array of SHORT or LONG values as u32.
fn read_short_or_long_array<R: Read + Seek>(
    reader: &mut R,
    little_endian: bool,
    field_type: u16,
    count: u32,
    value_offset: u32,
) -> Result<Vec<u32>, CogError> {
    if count == 1 {
        // Value inline for SHORT (type 3) — only lower 16 bits, or full u32 for LONG (type 4)
        return Ok(vec![if field_type == 3 {
            value_offset & 0xFFFF
        } else {
            value_offset
        }]);
    }
    reader.seek(SeekFrom::Start(value_offset as u64))?;
    let mut values = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if field_type == 3 {
            let v = read_u16(reader, little_endian).map_err(CogError::Io)?;
            values.push(v as u32);
        } else {
            let v = read_u32(reader, little_endian).map_err(CogError::Io)?;
            values.push(v);
        }
    }
    Ok(values)
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

    #[test]
    fn test_register_dataset() {
        let mut engine = CogEngine::new();
        let ds = CogDataset {
            id: Uuid::new_v4(),
            name: "Test".into(),
            href: "/tmp/test.tif".into(),
            file_size_bytes: 1024,
            width: 256,
            height: 256,
            bands: vec![],
            crs: "EPSG:4326".into(),
            bbox: [0.0, 0.0, 1.0, 1.0],
            pixel_size: [1.0, -1.0],
            overviews: vec![],
            tile_size: [256, 256],
            compression: CogCompression::None,
            nodata_value: None,
        };
        let ds_id = ds.id;
        engine.register_dataset(ds);
        assert_eq!(engine.list_datasets().len(), 3);
        assert!(engine.get_dataset(ds_id).is_some());
    }

    #[test]
    fn test_parse_tile_offsets_from_synthetic_tiff() {
        // Build a minimal TIFF in memory: little-endian, 1 tile
        let mut buf: Vec<u8> = Vec::new();

        // Byte order (II = little-endian)
        buf.extend_from_slice(b"II");
        // Magic number 42
        buf.extend_from_slice(&42u16.to_le_bytes());
        // IFD offset (immediately after header = byte 8)
        buf.extend_from_slice(&8u32.to_le_bytes());

        // IFD: 2 entries (TileOffsets and TileByteCounts)
        buf.extend_from_slice(&2u16.to_le_bytes());

        // Entry 1: TileOffsets (tag 324, type LONG=4, count 1, value = 200)
        buf.extend_from_slice(&324u16.to_le_bytes());
        buf.extend_from_slice(&4u16.to_le_bytes()); // LONG
        buf.extend_from_slice(&1u32.to_le_bytes()); // count
        buf.extend_from_slice(&200u32.to_le_bytes()); // offset value

        // Entry 2: TileByteCounts (tag 325, type LONG=4, count 1, value = 512)
        buf.extend_from_slice(&325u16.to_le_bytes());
        buf.extend_from_slice(&4u16.to_le_bytes()); // LONG
        buf.extend_from_slice(&1u32.to_le_bytes()); // count
        buf.extend_from_slice(&512u32.to_le_bytes()); // byte count value

        let mut cursor = std::io::Cursor::new(buf);
        let tiles = parse_tile_offsets(&mut cursor).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].offset, 200);
        assert_eq!(tiles[0].length, 512);
    }

    #[test]
    fn test_parse_invalid_tiff() {
        let buf = vec![0u8; 10]; // garbage data
        let mut cursor = std::io::Cursor::new(buf);
        assert!(parse_tile_offsets(&mut cursor).is_err());
    }
}
