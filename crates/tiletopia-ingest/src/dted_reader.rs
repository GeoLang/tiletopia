//! DTED (Digital Terrain Elevation Data) reader.

use crate::{Heightmap, IngestError};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const UHL_LEN: usize = 80;
const DSI_LEN: usize = 648;
const ACC_LEN: usize = 2700;
const HEADER_TOTAL: usize = UHL_LEN + DSI_LEN + ACC_LEN;

/// Read a DTED file (DT0/DT1/DT2) into a Heightmap.
pub fn read(path: &Path) -> Result<Heightmap, IngestError> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len() as usize;

    if file_len < HEADER_TOTAL {
        return Err(IngestError::ParseError(
            "DTED file too small for headers".to_string(),
        ));
    }

    // Read UHL (User Header Label)
    let mut uhl = [0u8; UHL_LEN];
    file.read_exact(&mut uhl)?;

    if &uhl[0..3] != b"UHL" {
        return Err(IngestError::ParseError(
            "DTED: missing UHL sentinel".to_string(),
        ));
    }

    // Parse origin longitude/latitude from UHL
    let origin_lon = parse_dted_lon(&uhl[4..12])?;
    let origin_lat = parse_dted_lat(&uhl[12..20])?;

    // Longitude/latitude intervals in tenths of arc-seconds
    let lon_interval = parse_uhl_int(&uhl[20..24])? as f64 / 36000.0; // degrees
    let lat_interval = parse_uhl_int(&uhl[24..28])? as f64 / 36000.0; // degrees

    // Number of longitude and latitude lines
    let num_lon_lines = parse_uhl_int(&uhl[47..51])?;
    let num_lat_points = parse_uhl_int(&uhl[51..55])?;

    if num_lon_lines == 0 || num_lat_points == 0 {
        return Err(IngestError::ParseError("DTED: zero dimensions".to_string()));
    }

    let width = num_lon_lines;
    let height = num_lat_points;

    // Skip DSI and ACC headers
    file.seek(SeekFrom::Start(HEADER_TOTAL as u64))?;

    // Read elevation data column by column
    // Each column: 8-byte prefix + num_lat_points * 2 bytes elevation + 4 bytes checksum
    let col_data_len = 8 + height * 2 + 4;
    let mut elevations = vec![0.0f64; width * height];

    for col in 0..width {
        let mut col_buf = vec![0u8; col_data_len];
        file.read_exact(&mut col_buf).map_err(|e| {
            IngestError::ParseError(format!("DTED: failed to read column {col}: {e}"))
        })?;

        // First byte should be data record sentinel (0xAA)
        if col_buf[0] != 0xAA {
            return Err(IngestError::ParseError(format!(
                "DTED: missing sentinel at column {col}"
            )));
        }

        // Elevation samples start at offset 8 (after 12-byte prefix, but
        // the prefix is: sentinel(1) + data_block_count(3) + lon_count(2) + lat_count(2) + padding(4)
        // Actually the DTED spec says: sentinel(1) + sequential_count(3) + lon_count(2) + lat_count(2) = 8 bytes
        // then elevation data follows
        for row in 0..height {
            let offset = 8 + row * 2;
            let raw = i16::from_be_bytes([col_buf[offset], col_buf[offset + 1]]);
            // Row order: south to north. Store as row-major (north to south).
            let out_row = height - 1 - row;
            elevations[out_row * width + col] = raw as f64;
        }
    }

    let bounds = [
        origin_lon,
        origin_lat,
        origin_lon + (width as f64 - 1.0) * lon_interval,
        origin_lat + (height as f64 - 1.0) * lat_interval,
    ];

    tracing::info!(
        "Read {}×{} DTED heightmap from {}",
        width,
        height,
        path.display(),
    );

    Ok(Heightmap {
        width,
        height,
        elevations,
        bounds,
        nodata: Some(-32767.0),
    })
}

/// Parse a DTED longitude string (e.g. "0100000E" → 10.0).
fn parse_dted_lon(bytes: &[u8]) -> Result<f64, IngestError> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| IngestError::ParseError("DTED: invalid longitude bytes".to_string()))?
        .trim();
    if s.is_empty() {
        return Ok(0.0);
    }
    let (num_part, hemi) = s.split_at(s.len() - 1);
    let ddd = num_part
        .get(0..3)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let mm = num_part
        .get(3..5)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let ss = num_part
        .get(5..)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let mut deg = ddd + mm / 60.0 + ss / 3600.0;
    if hemi == "W" || hemi == "w" {
        deg = -deg;
    }
    Ok(deg)
}

/// Parse a DTED latitude string (e.g. "0470000N" → 47.0).
fn parse_dted_lat(bytes: &[u8]) -> Result<f64, IngestError> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| IngestError::ParseError("DTED: invalid latitude bytes".to_string()))?
        .trim();
    if s.is_empty() {
        return Ok(0.0);
    }
    let (num_part, hemi) = s.split_at(s.len() - 1);
    let dd = num_part
        .get(0..2)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let mm = num_part
        .get(2..4)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let ss = num_part
        .get(4..)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let mut deg = dd + mm / 60.0 + ss / 3600.0;
    if hemi == "S" || hemi == "s" {
        deg = -deg;
    }
    Ok(deg)
}

/// Parse a plain ASCII integer from a UHL field.
fn parse_uhl_int(bytes: &[u8]) -> Result<usize, IngestError> {
    let s = std::str::from_utf8(bytes)
        .map_err(|_| IngestError::ParseError("DTED: invalid UHL integer".to_string()))?
        .trim();
    s.parse::<usize>()
        .map_err(|_| IngestError::ParseError(format!("DTED: cannot parse UHL integer '{s}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_synthetic_dted() {
        let dir = std::env::temp_dir().join("tiletopia_dted_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.dt2");

        let width: usize = 3;
        let height: usize = 3;

        // Build UHL header (80 bytes)
        let mut uhl = [b' '; UHL_LEN];
        uhl[0..3].copy_from_slice(b"UHL");
        uhl[3] = b'1';
        // Longitude: "0110000E"
        uhl[4..12].copy_from_slice(b"0110000E");
        // Latitude: "0470000N"
        uhl[12..20].copy_from_slice(b"0470000N");
        // Lon interval: 0030 (3 arc-seconds = 30 tenths)
        uhl[20..24].copy_from_slice(b"0030");
        // Lat interval: 0030
        uhl[24..28].copy_from_slice(b"0030");
        // Number of longitude lines
        let width_str = format!("{:04}", width);
        uhl[47..51].copy_from_slice(width_str.as_bytes());
        // Number of latitude points
        let height_str = format!("{:04}", height);
        uhl[51..55].copy_from_slice(height_str.as_bytes());

        // DSI header (648 bytes) — filled with spaces
        let dsi = [b' '; DSI_LEN];
        // ACC header (2700 bytes) — filled with spaces
        let acc = [b' '; ACC_LEN];

        // Column data: each column = 8 prefix + height*2 elevation + 4 checksum
        let col_data_len = 8 + height * 2 + 4;
        let mut file_data = Vec::new();
        file_data.extend_from_slice(&uhl);
        file_data.extend_from_slice(&dsi);
        file_data.extend_from_slice(&acc);

        for col in 0..width {
            let mut col_buf = vec![0u8; col_data_len];
            col_buf[0] = 0xAA; // sentinel
            // Sequential count (3 bytes) + lon_count (2 bytes) + lat_count (2 bytes) = 7 bytes after sentinel = 8 prefix total
            for row in 0..height {
                let elev = (col * height + row) as i16 * 100;
                let offset = 8 + row * 2;
                let bytes = elev.to_be_bytes();
                col_buf[offset] = bytes[0];
                col_buf[offset + 1] = bytes[1];
            }
            file_data.extend_from_slice(&col_buf);
        }

        std::fs::write(&path, &file_data).unwrap();

        let hm = read(&path).unwrap();
        assert_eq!(hm.width, width);
        assert_eq!(hm.height, height);
        assert_eq!(hm.elevations.len(), width * height);

        std::fs::remove_dir_all(&dir).ok();
    }
}
