//! SRTM DEM tile download and caching.
//!
//! Downloads SRTM 1-arcsecond (SRTM GL1) tiles from the AWS public dataset
//! and caches them locally for terrain generation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

/// DEM cache errors.
#[derive(Debug, Error)]
pub enum DemError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("decompression error: {0}")]
    Decompress(String),
    #[error("ingest error: {0}")]
    Ingest(#[from] tiletopia_ingest::IngestError),
}

/// Public AWS bucket prefix the SRTM HGT tiles come from.
pub const DEFAULT_SRTM_BASE_URL: &str = "https://elevation-tiles-prod.s3.amazonaws.com/skadi";

/// SRTM prefix from `TILETOPIA_SRTM_BASE_URL`, else the public bucket.
pub fn srtm_base_url_from_env() -> String {
    std::env::var("TILETOPIA_SRTM_BASE_URL").unwrap_or_else(|_| DEFAULT_SRTM_BASE_URL.to_string())
}

/// Cached SRTM tile downloader.
pub struct DemCache {
    cache_dir: PathBuf,
    client: reqwest::Client,
    base_url: String,
}

impl DemCache {
    pub fn new(cache_dir: PathBuf, base_url: String) -> Self {
        Self {
            cache_dir,
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Get or download an SRTM HGT tile.
    ///
    /// `lat`/`lon` are integer degrees of the tile's SW corner
    /// (e.g. lat=47, lon=8 → N47E008).
    pub async fn get_srtm_tile(&self, lat: i32, lon: i32) -> Result<PathBuf, DemError> {
        let tile_name = srtm_tile_name(lat, lon);
        let hgt_path = self.cache_dir.join(format!("{tile_name}.hgt"));

        if hgt_path.exists() {
            return Ok(hgt_path);
        }

        std::fs::create_dir_all(&self.cache_dir)?;

        let ns = if lat >= 0 { "N" } else { "S" };
        let lat_dir = format!("{ns}{:02}", lat.unsigned_abs());
        let url = format!("{}/{lat_dir}/{tile_name}.hgt.gz", self.base_url);

        tracing::info!("Downloading SRTM tile: {url}");
        let response = self.client.get(&url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(DemError::Upstream(format!(
                "HTTP {status} downloading {url}"
            )));
        }
        let compressed = response.bytes().await?;

        // Decompress gzip
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| DemError::Decompress(e.to_string()))?;

        write_atomic(&hgt_path, &decompressed)?;
        tracing::info!("Cached SRTM tile: {}", hgt_path.display());

        Ok(hgt_path)
    }

    /// Get a heightmap for a geographic region by mosaicing SRTM tiles.
    pub async fn get_heightmap(
        &self,
        west: f64,
        south: f64,
        east: f64,
        north: f64,
    ) -> Result<tiletopia_ingest::Heightmap, DemError> {
        let tiles = required_srtm_tiles(west, south, east, north);

        // Download all needed tiles
        let mut tile_data: Vec<(i32, i32, tiletopia_ingest::Heightmap)> = Vec::new();
        for (lat, lon) in &tiles {
            let path = self.get_srtm_tile(*lat, *lon).await?;
            let hm = tiletopia_ingest::hgt_reader::read(&path)?;
            tile_data.push((*lat, *lon, hm));
        }

        if tile_data.is_empty() {
            return Ok(tiletopia_ingest::Heightmap {
                width: 1,
                height: 1,
                elevations: vec![0.0],
                bounds: [west, south, east, north],
                nodata: Some(f64::NAN),
            });
        }

        // Single tile — crop to bounds
        if tile_data.len() == 1 {
            let (_, _, hm) = tile_data.into_iter().next().unwrap();
            return Ok(crop_heightmap(&hm, west, south, east, north));
        }

        // Mosaic multiple tiles
        Ok(mosaic_heightmaps(&tile_data, west, south, east, north))
    }
}

/// Write a cache file so no reader ever observes it half-written.
///
/// Concurrent terrain requests hit the same tile at once, and a plain write
/// leaves the destination truncated for as long as it takes to fill: readers
/// were picking up 0-byte DEMs. Filling a per-writer temp file next to the
/// destination and renaming makes the swap atomic.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    std::fs::write(&temp, bytes)?;
    if let Err(e) = std::fs::rename(&temp, path) {
        std::fs::remove_file(&temp).ok();
        return Err(e);
    }
    Ok(())
}

/// Compute the SRTM tile name for given integer lat/lon.
pub fn srtm_tile_name(lat: i32, lon: i32) -> String {
    let ns = if lat >= 0 { "N" } else { "S" };
    let ew = if lon >= 0 { "E" } else { "W" };
    format!("{ns}{:02}{ew}{:03}", lat.unsigned_abs(), lon.unsigned_abs())
}

/// Compute which SRTM 1°×1° tiles are needed to cover a bounding box.
pub fn required_srtm_tiles(west: f64, south: f64, east: f64, north: f64) -> Vec<(i32, i32)> {
    let lat_min = south.floor() as i32;
    let lat_max = (north - 1e-9).floor() as i32;
    let lon_min = west.floor() as i32;
    let lon_max = (east - 1e-9).floor() as i32;

    let mut tiles = Vec::new();
    for lat in lat_min..=lat_max {
        for lon in lon_min..=lon_max {
            tiles.push((lat, lon));
        }
    }
    tiles
}

/// Crop a heightmap to the given bounds via bilinear resampling.
fn crop_heightmap(
    hm: &tiletopia_ingest::Heightmap,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) -> tiletopia_ingest::Heightmap {
    let lon_range = hm.bounds[2] - hm.bounds[0];
    let lat_range = hm.bounds[3] - hm.bounds[1];

    // Output resolution: maintain ~same density as source
    let out_width = ((east - west) / lon_range * hm.width as f64)
        .ceil()
        .max(2.0) as usize;
    let out_height = ((north - south) / lat_range * hm.height as f64)
        .ceil()
        .max(2.0) as usize;

    let mut elevations = Vec::with_capacity(out_width * out_height);
    for row in 0..out_height {
        let lat = north - (row as f64 / (out_height - 1).max(1) as f64) * (north - south);
        for col in 0..out_width {
            let lon = west + (col as f64 / (out_width - 1).max(1) as f64) * (east - west);

            // Map to source fractional pixel
            let u = (lon - hm.bounds[0]) / lon_range;
            let v = 1.0 - (lat - hm.bounds[1]) / lat_range; // HGT is north-to-south
            let fx = u * (hm.width - 1) as f64;
            let fy = v * (hm.height - 1) as f64;

            let x0 = (fx.floor() as usize).min(hm.width - 1);
            let y0 = (fy.floor() as usize).min(hm.height - 1);
            let x1 = (x0 + 1).min(hm.width - 1);
            let y1 = (y0 + 1).min(hm.height - 1);
            let dx = fx - x0 as f64;
            let dy = fy - y0 as f64;

            let v00 = hm.elevations[y0 * hm.width + x0];
            let v10 = hm.elevations[y0 * hm.width + x1];
            let v01 = hm.elevations[y1 * hm.width + x0];
            let v11 = hm.elevations[y1 * hm.width + x1];

            let val = v00 * (1.0 - dx) * (1.0 - dy)
                + v10 * dx * (1.0 - dy)
                + v01 * (1.0 - dx) * dy
                + v11 * dx * dy;
            elevations.push(val);
        }
    }

    tiletopia_ingest::Heightmap {
        width: out_width,
        height: out_height,
        elevations,
        bounds: [west, south, east, north],
        nodata: hm.nodata,
    }
}

/// Mosaic multiple 1°×1° tiles into a single heightmap covering the given bounds.
fn mosaic_heightmaps(
    tiles: &[(i32, i32, tiletopia_ingest::Heightmap)],
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) -> tiletopia_ingest::Heightmap {
    // Use the resolution of the first tile as reference
    let ref_tile = &tiles[0].2;
    let pixel_size_lon = (ref_tile.bounds[2] - ref_tile.bounds[0]) / ref_tile.width as f64;
    let pixel_size_lat = (ref_tile.bounds[3] - ref_tile.bounds[1]) / ref_tile.height as f64;

    let out_width = ((east - west) / pixel_size_lon).ceil().max(2.0) as usize;
    let out_height = ((north - south) / pixel_size_lat).ceil().max(2.0) as usize;

    let mut elevations = vec![f64::NAN; out_width * out_height];

    for row in 0..out_height {
        let lat = north - (row as f64 / (out_height - 1).max(1) as f64) * (north - south);
        for col in 0..out_width {
            let lon = west + (col as f64 / (out_width - 1).max(1) as f64) * (east - west);

            // Find the tile that covers this point
            for (tlat, tlon, hm) in tiles {
                let tile_west = *tlon as f64;
                let tile_south = *tlat as f64;
                let tile_east = tile_west + 1.0;
                let tile_north = tile_south + 1.0;

                if lon >= tile_west && lon <= tile_east && lat >= tile_south && lat <= tile_north {
                    let lon_range = hm.bounds[2] - hm.bounds[0];
                    let lat_range = hm.bounds[3] - hm.bounds[1];
                    let u = (lon - hm.bounds[0]) / lon_range;
                    let v = 1.0 - (lat - hm.bounds[1]) / lat_range;
                    let fx = u * (hm.width - 1) as f64;
                    let fy = v * (hm.height - 1) as f64;

                    let x0 = (fx.floor() as usize).min(hm.width - 1);
                    let y0 = (fy.floor() as usize).min(hm.height - 1);
                    let x1 = (x0 + 1).min(hm.width - 1);
                    let y1 = (y0 + 1).min(hm.height - 1);
                    let dx = fx - x0 as f64;
                    let dy = fy - y0 as f64;

                    let v00 = hm.elevations[y0 * hm.width + x0];
                    let v10 = hm.elevations[y0 * hm.width + x1];
                    let v01 = hm.elevations[y1 * hm.width + x0];
                    let v11 = hm.elevations[y1 * hm.width + x1];

                    let val = v00 * (1.0 - dx) * (1.0 - dy)
                        + v10 * dx * (1.0 - dy)
                        + v01 * (1.0 - dx) * dy
                        + v11 * dx * dy;

                    if !val.is_nan() {
                        elevations[row * out_width + col] = val;
                    }
                    break;
                }
            }
        }
    }

    tiletopia_ingest::Heightmap {
        width: out_width,
        height: out_height,
        elevations,
        bounds: [west, south, east, north],
        nodata: Some(f64::NAN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_name_north_east() {
        assert_eq!(srtm_tile_name(47, 8), "N47E008");
    }

    #[test]
    fn tile_name_south_west() {
        assert_eq!(srtm_tile_name(-33, -70), "S33W070");
    }

    #[test]
    fn tile_name_zero() {
        assert_eq!(srtm_tile_name(0, 0), "N00E000");
    }

    #[test]
    fn required_tiles_single() {
        let tiles = required_srtm_tiles(8.0, 47.0, 9.0, 48.0);
        assert_eq!(tiles, vec![(47, 8)]);
    }

    #[test]
    fn required_tiles_multiple() {
        let tiles = required_srtm_tiles(8.0, 46.5, 9.5, 48.0);
        // Should need: (46,8), (46,9), (47,8), (47,9)
        assert_eq!(tiles.len(), 4);
        assert!(tiles.contains(&(46, 8)));
        assert!(tiles.contains(&(46, 9)));
        assert!(tiles.contains(&(47, 8)));
        assert!(tiles.contains(&(47, 9)));
    }

    #[test]
    fn required_tiles_exact_boundary() {
        // When east/north fall exactly on integer boundaries
        let tiles = required_srtm_tiles(8.0, 47.0, 9.0, 48.0);
        assert_eq!(tiles, vec![(47, 8)]);
    }

    #[test]
    fn readers_never_see_a_half_written_cache_file() {
        // one writer replacing the file while a reader hammers it: with a plain
        // write the reader catches the truncated window and gets a short file
        let dir = std::env::temp_dir().join("tiletopia_atomic_write_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("N43E007.hgt");
        const LEN: usize = 512 * 1024;

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            for round in 0..40u8 {
                write_atomic(&writer_path, &vec![round; LEN]).unwrap();
            }
        });

        let mut reads = 0;
        while !writer.is_finished() {
            if let Ok(bytes) = std::fs::read(&path) {
                assert_eq!(bytes.len(), LEN, "reader saw a partially written file");
                reads += 1;
            }
        }
        writer.join().unwrap();
        assert!(reads > 0, "the reader never observed the file");

        // and no temp files are left behind
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left in the cache dir");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn crop_synthetic_heightmap() {
        let hm = tiletopia_ingest::Heightmap {
            width: 5,
            height: 5,
            elevations: (0..25).map(|i| i as f64 * 10.0).collect(),
            bounds: [8.0, 47.0, 9.0, 48.0],
            nodata: None,
        };

        let cropped = crop_heightmap(&hm, 8.25, 47.25, 8.75, 47.75);
        assert!(cropped.width >= 2);
        assert!(cropped.height >= 2);
        assert_eq!(cropped.bounds, [8.25, 47.25, 8.75, 47.75]);
    }
}
