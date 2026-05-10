//! SRTM HGT elevation reader.

use crate::{Heightmap, IngestError};
use std::io::Read;
use std::path::Path;

const NODATA: i16 = -32768;

/// Read an SRTM HGT file into a Heightmap.
///
/// The filename encodes the SW corner (e.g. `N47E011.hgt`).
/// SRTM1 files are 3601×3601, SRTM3 files are 1201×1201.
pub fn read(path: &Path) -> Result<Heightmap, IngestError> {
    let file_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| IngestError::ParseError("HGT: cannot determine filename".to_string()))?;

    let (lat, lon) = parse_hgt_filename(file_name)?;

    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len() as usize;

    let side = match file_len {
        25934402 => 3601, // SRTM1: 3601×3601 × 2
        2884802 => 1201,  // SRTM3: 1201×1201 × 2
        _ => {
            // Try to deduce from file size: file_len = side * side * 2
            let samples = file_len / 2;
            let side = (samples as f64).sqrt() as usize;
            if side * side * 2 != file_len {
                return Err(IngestError::ParseError(format!(
                    "HGT: unexpected file size {file_len} bytes"
                )));
            }
            side
        }
    };

    let mut raw = vec![0u8; file_len];
    file.read_exact(&mut raw)?;

    let mut elevations = Vec::with_capacity(side * side);
    for i in 0..(side * side) {
        let offset = i * 2;
        let raw_val = i16::from_be_bytes([raw[offset], raw[offset + 1]]);
        elevations.push(if raw_val == NODATA {
            f64::NAN
        } else {
            raw_val as f64
        });
    }

    // HGT data is stored north-to-south, west-to-east — already row-major.
    let bounds = [lon as f64, lat as f64, lon as f64 + 1.0, lat as f64 + 1.0];

    tracing::info!(
        "Read {}×{} HGT heightmap from {}",
        side,
        side,
        path.display(),
    );

    Ok(Heightmap {
        width: side,
        height: side,
        elevations,
        bounds,
        nodata: Some(f64::NAN),
    })
}

/// Parse latitude/longitude from HGT filename (e.g. "N47E011" → (47, 11)).
fn parse_hgt_filename(name: &str) -> Result<(i32, i32), IngestError> {
    let name = name.to_uppercase();
    if name.len() < 7 {
        return Err(IngestError::ParseError(format!(
            "HGT: filename too short: '{name}'"
        )));
    }

    let lat_hemi = &name[0..1];
    let lat_val: i32 = name[1..3]
        .parse()
        .map_err(|_| IngestError::ParseError(format!("HGT: invalid latitude in '{name}'")))?;
    let lon_hemi = &name[3..4];
    let lon_val: i32 = name[4..7]
        .parse()
        .map_err(|_| IngestError::ParseError(format!("HGT: invalid longitude in '{name}'")))?;

    let lat = if lat_hemi == "S" { -lat_val } else { lat_val };
    let lon = if lon_hemi == "W" { -lon_val } else { lon_val };

    Ok((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hgt_filename() {
        let (lat, lon) = parse_hgt_filename("N47E011").unwrap();
        assert_eq!(lat, 47);
        assert_eq!(lon, 11);

        let (lat, lon) = parse_hgt_filename("S33W070").unwrap();
        assert_eq!(lat, -33);
        assert_eq!(lon, -70);
    }

    #[test]
    fn test_read_synthetic_hgt() {
        let dir = std::env::temp_dir().join("tiletopia_hgt_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("N00E000.hgt");

        // Create a small 5×5 HGT file (5*5*2 = 50 bytes)
        let side = 5usize;
        let mut data = Vec::with_capacity(side * side * 2);
        for i in 0..(side * side) {
            let elev = (i as i16) * 10;
            data.extend_from_slice(&elev.to_be_bytes());
        }
        std::fs::write(&path, &data).unwrap();

        let hm = read(&path).unwrap();
        assert_eq!(hm.width, side);
        assert_eq!(hm.height, side);
        assert_eq!(hm.elevations.len(), side * side);
        assert!((hm.elevations[0] - 0.0).abs() < 1e-10);
        assert!((hm.elevations[1] - 10.0).abs() < 1e-10);
        assert!((hm.bounds[0] - 0.0).abs() < 1e-10);
        assert!((hm.bounds[3] - 1.0).abs() < 1e-10);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_nodata() {
        let dir = std::env::temp_dir().join("tiletopia_hgt_nodata_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("N01E001.hgt");

        let side = 3usize;
        let mut data = Vec::with_capacity(side * side * 2);
        for i in 0..(side * side) {
            let elev: i16 = if i == 4 { -32768 } else { 100 };
            data.extend_from_slice(&elev.to_be_bytes());
        }
        std::fs::write(&path, &data).unwrap();

        let hm = read(&path).unwrap();
        assert!(hm.elevations[4].is_nan());
        assert!((hm.elevations[0] - 100.0).abs() < 1e-10);

        std::fs::remove_dir_all(&dir).ok();
    }
}
