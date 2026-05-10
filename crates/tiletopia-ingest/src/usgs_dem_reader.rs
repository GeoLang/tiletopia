//! USGS native DEM (ASCII) elevation reader.
//!
//! Parses the legacy USGS 1-degree DEM format, which uses fixed-width
//! ASCII records: a 1024-char Record A (header) followed by one Record B
//! per elevation profile (column).

use crate::{Heightmap, IngestError};
use std::path::Path;

/// Read a USGS native DEM file into a Heightmap.
///
/// The format is an ASCII fixed-width layout from the 1990s:
/// - Record A (1024 chars): metadata (name, corners, units, resolution, dimensions)
/// - Record B (1024 chars each): one elevation profile per column
pub fn read(path: &Path) -> Result<Heightmap, IngestError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        IngestError::ParseError(format!("USGS DEM: cannot read file: {e}"))
    })?;

    if content.len() < 1024 {
        return Err(IngestError::ParseError(
            "USGS DEM: file too short for Record A".to_string(),
        ));
    }

    let record_a = &content[..1024];

    // Parse ground planimetric reference system (col 529, 1-indexed → byte 528)
    let plan_ref = parse_int_field(record_a, 528, 534).unwrap_or(0);
    // Parse elevation units: 1=feet, 2=meters
    let elev_unit_code = parse_int_field(record_a, 534, 540).unwrap_or(2);

    // Number of columns and rows (cols 469–474, 475–480 in older spec variants)
    // Try the most common layout: dims at 858–864 (cols) and 864–870 (rows)
    let (num_cols, num_rows) = parse_dimensions(record_a)?;

    // X/Y ground resolution
    let x_res = parse_float_field(record_a, 816, 828).unwrap_or(30.0);
    let y_res = parse_float_field(record_a, 828, 840).unwrap_or(30.0);

    // Corner coordinates: SW, NW, NE, SE as (easting, northing) pairs
    let sw_x = parse_float_field(record_a, 546, 570).unwrap_or(0.0);
    let sw_y = parse_float_field(record_a, 570, 594).unwrap_or(0.0);
    let _nw_x = parse_float_field(record_a, 594, 618).unwrap_or(0.0);
    let nw_y = parse_float_field(record_a, 618, 642).unwrap_or(0.0);
    let ne_x = parse_float_field(record_a, 642, 666).unwrap_or(0.0);
    let _ne_y = parse_float_field(record_a, 666, 690).unwrap_or(0.0);

    let _ = plan_ref;
    let elev_scale = if elev_unit_code == 1 { 0.3048 } else { 1.0 };

    let bounds = [sw_x, sw_y, ne_x, nw_y];

    // Parse elevation profiles from Record B blocks
    let mut elevations = vec![0.0f64; num_cols * num_rows];
    let remaining = &content[1024..];

    let mut offset = 0;
    for col in 0..num_cols {
        if offset >= remaining.len() {
            return Err(IngestError::ParseError(format!(
                "USGS DEM: unexpected end of data at column {col}"
            )));
        }

        // Each Record B starts with a 6-int header: row_id, col_id, num_rows_in_profile,
        // num_cols_in_profile(1), x_gp, y_gp, elev_local_datum, min_elev, max_elev
        let record_b = &remaining[offset..];
        let (profile_rows, values, consumed) = parse_record_b(record_b, num_rows)?;

        let actual_rows = profile_rows.min(num_rows);
        for row in 0..actual_rows {
            // Store north-to-south row-major
            let out_row = num_rows - 1 - row;
            let val = if row < values.len() {
                values[row] * elev_scale
            } else {
                0.0
            };
            elevations[out_row * num_cols + col] = val;
        }

        offset += consumed;
    }

    tracing::info!(
        "Read {}×{} USGS DEM heightmap from {} (res {x_res}×{y_res})",
        num_cols,
        num_rows,
        path.display(),
    );

    Ok(Heightmap {
        width: num_cols,
        height: num_rows,
        elevations,
        bounds,
        nodata: None,
    })
}

/// Parse an integer from a fixed-width field.
fn parse_int_field(record: &str, start: usize, end: usize) -> Option<i64> {
    let end = end.min(record.len());
    if start >= end {
        return None;
    }
    record[start..end].trim().parse().ok()
}

/// Parse a float from a fixed-width field (handles Fortran-style `D` exponent).
fn parse_float_field(record: &str, start: usize, end: usize) -> Option<f64> {
    let end = end.min(record.len());
    if start >= end {
        return None;
    }
    let s = record[start..end].trim().replace('D', "E").replace('d', "e");
    s.parse().ok()
}

/// Try to extract dimensions from Record A.
fn parse_dimensions(record_a: &str) -> Result<(usize, usize), IngestError> {
    // Several USGS DEM spec variants place dimensions differently.
    // Try columns 858–864 and 864–870 first (most common 1-degree DEM layout).
    if let (Some(cols), Some(rows)) = (
        parse_int_field(record_a, 858, 864),
        parse_int_field(record_a, 864, 870),
    ) {
        if cols > 0 && rows > 0 {
            return Ok((cols as usize, rows as usize));
        }
    }

    // Fallback: try 468–474 and 474–480
    if let (Some(cols), Some(rows)) = (
        parse_int_field(record_a, 468, 474),
        parse_int_field(record_a, 474, 480),
    ) {
        if cols > 0 && rows > 0 {
            return Ok((cols as usize, rows as usize));
        }
    }

    Err(IngestError::ParseError(
        "USGS DEM: cannot determine dimensions from Record A".to_string(),
    ))
}

/// Parse a Record B elevation profile, returning (num_rows, elevation_values, bytes_consumed).
fn parse_record_b(data: &str, expected_rows: usize) -> Result<(usize, Vec<f64>, usize), IngestError> {
    // Record B values are space-separated integers.
    // First 6 values: row_id, col_id, num_m, num_n, x_gp, y_gp
    // Then min_elev, max_elev, followed by elevation values.
    let mut values = Vec::new();
    let mut chars = data.char_indices().peekable();
    let mut token_count = 0;
    let mut num_rows_in_profile = expected_rows;
    let mut last_end = 0;

    // We need header (6 ints) + 2 (min/max) + num_rows elevation values
    let header_ints = 6;
    let target_count = header_ints + 2 + expected_rows;

    loop {
        // Skip whitespace
        while let Some(&(_, c)) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        let start = match chars.peek() {
            Some(&(i, _)) => i,
            None => break,
        };

        // Read token
        while let Some(&(i, c)) = chars.peek() {
            if c.is_whitespace() {
                last_end = i;
                break;
            }
            last_end = i + c.len_utf8();
            chars.next();
        }

        let token = &data[start..last_end];
        if token.is_empty() {
            break;
        }

        if token_count == 2 {
            // This is num_m (rows in this profile)
            if let Ok(v) = token.trim().parse::<i64>() {
                num_rows_in_profile = v.max(0) as usize;
            }
        }

        if token_count >= header_ints + 2 {
            // Elevation value
            let val: f64 = token
                .trim()
                .replace('D', "E")
                .replace('d', "e")
                .parse()
                .unwrap_or(0.0);
            values.push(val);
        }

        token_count += 1;
        if token_count >= target_count {
            break;
        }
    }

    Ok((num_rows_in_profile, values, last_end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_test_dem(cols: usize, rows: usize) -> String {
        // Build a minimal Record A (1024 chars)
        let mut record_a = vec![b' '; 1024];

        // Name at cols 1–40
        let name = b"TEST DEM";
        record_a[0..name.len()].copy_from_slice(name);

        // Elevation units = 2 (meters) at 534–540
        let unit_str = format!("{:>6}", 2);
        record_a[534..540].copy_from_slice(unit_str.as_bytes());

        // Corner coordinates (SW, NW, NE, SE)
        let sw_x = format!("{:>24}", "10.0");
        let sw_y = format!("{:>24}", "20.0");
        let nw_x = format!("{:>24}", "10.0");
        let nw_y = format!("{:>24}", "21.0");
        let ne_x = format!("{:>24}", "11.0");
        let ne_y = format!("{:>24}", "21.0");
        record_a[546..570].copy_from_slice(sw_x.as_bytes());
        record_a[570..594].copy_from_slice(sw_y.as_bytes());
        record_a[594..618].copy_from_slice(nw_x.as_bytes());
        record_a[618..642].copy_from_slice(nw_y.as_bytes());
        record_a[642..666].copy_from_slice(ne_x.as_bytes());
        record_a[666..690].copy_from_slice(ne_y.as_bytes());

        // Resolution at 816–840
        let x_res = format!("{:>12}", "30.0");
        let y_res = format!("{:>12}", "30.0");
        record_a[816..828].copy_from_slice(x_res.as_bytes());
        record_a[828..840].copy_from_slice(y_res.as_bytes());

        // Dimensions at 858–870
        let cols_str = format!("{:>6}", cols);
        let rows_str = format!("{:>6}", rows);
        record_a[858..864].copy_from_slice(cols_str.as_bytes());
        record_a[864..870].copy_from_slice(rows_str.as_bytes());

        let mut dem = String::from_utf8(record_a).unwrap();

        // Build Record B blocks (one per column)
        for col in 0..cols {
            // Header: row_id col_id num_m num_n x_gp y_gp min_elev max_elev
            let mut profile = format!("1 {} {} 1 0 0 0 100", col + 1, rows);
            for row in 0..rows {
                let elev = (col * rows + row) as i32 * 10;
                profile.push_str(&format!(" {}", elev));
            }
            profile.push(' ');
            dem.push_str(&profile);
        }

        dem
    }

    #[test]
    fn test_read_synthetic_dem() {
        let dem_content = make_test_dem(3, 4);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.dem");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(dem_content.as_bytes()).unwrap();

        let hm = read(&path).unwrap();
        assert_eq!(hm.width, 3);
        assert_eq!(hm.height, 4);
        assert_eq!(hm.elevations.len(), 12);
        assert!((hm.bounds[0] - 10.0).abs() < 0.01);
        assert!((hm.bounds[1] - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_read_file_too_short() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.dem");
        std::fs::write(&path, "too short").unwrap();
        assert!(read(&path).is_err());
    }

    #[test]
    fn test_parse_float_field_fortran() {
        assert!((parse_float_field("  1.5D+02  ", 0, 11).unwrap() - 150.0).abs() < 0.01);
    }
}
