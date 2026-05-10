//! GeoPackage vector reader.

use crate::{IngestError, VectorFeature, VectorGeometry};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

/// Read vector features from a GeoPackage file.
pub fn read(path: &Path) -> Result<Vec<VectorFeature>, IngestError> {
    let conn = Connection::open(path)
        .map_err(|e| IngestError::ParseError(format!("GeoPackage open error: {e}")))?;

    // Find feature tables from gpkg_contents
    let mut tables_stmt = conn
        .prepare("SELECT table_name, data_type FROM gpkg_contents WHERE data_type = 'features'")
        .map_err(|e| IngestError::ParseError(format!("GeoPackage query error: {e}")))?;

    let tables: Vec<String> = tables_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| IngestError::ParseError(format!("GeoPackage query error: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

    if tables.is_empty() {
        return Err(IngestError::ParseError(
            "GeoPackage: no feature tables found".to_string(),
        ));
    }

    let mut features = Vec::new();

    for table_name in &tables {
        // Find the geometry column name from gpkg_geometry_columns
        let geom_col: String = conn
            .query_row(
                "SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?1",
                [table_name],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "geom".to_string());

        // Get column names for the table
        let col_info_sql = format!("PRAGMA table_info('{}')", table_name.replace('\'', "''"));
        let mut col_stmt = conn
            .prepare(&col_info_sql)
            .map_err(|e| IngestError::ParseError(format!("GeoPackage PRAGMA error: {e}")))?;

        let columns: Vec<String> = col_stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| IngestError::ParseError(format!("GeoPackage column query error: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        let attr_cols: Vec<&str> = columns
            .iter()
            .filter(|c| *c != &geom_col && *c != "fid" && *c != "id")
            .map(|c| c.as_str())
            .collect();

        let select_cols = std::iter::once(geom_col.as_str())
            .chain(attr_cols.iter().copied())
            .collect::<Vec<&str>>()
            .join(", ");

        let sql = format!(
            "SELECT {} FROM \"{}\"",
            select_cols,
            table_name.replace('"', "\"\"")
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| IngestError::ParseError(format!("GeoPackage select error: {e}")))?;

        let rows = stmt
            .query_map([], |row| {
                let geom_blob: Vec<u8> = row.get(0)?;
                let mut props = HashMap::new();
                for (i, col) in attr_cols.iter().enumerate() {
                    if let Ok(val) = row.get::<_, String>(i + 1) {
                        props.insert(col.to_string(), val);
                    } else if let Ok(val) = row.get::<_, f64>(i + 1) {
                        props.insert(col.to_string(), val.to_string());
                    } else if let Ok(val) = row.get::<_, i64>(i + 1) {
                        props.insert(col.to_string(), val.to_string());
                    }
                }
                Ok((geom_blob, props))
            })
            .map_err(|e| IngestError::ParseError(format!("GeoPackage row query error: {e}")))?;

        for row_result in rows {
            let (geom_blob, props) = row_result
                .map_err(|e| IngestError::ParseError(format!("GeoPackage row error: {e}")))?;

            if let Some(geometry) = parse_gpkg_geometry(&geom_blob) {
                features.push(VectorFeature {
                    geometry,
                    properties: props,
                });
            }
        }
    }

    tracing::info!(
        "Read {} features from {}",
        features.len(),
        path.display(),
    );

    Ok(features)
}

/// Parse a GeoPackage geometry blob (GeoPackage Binary header + WKB).
fn parse_gpkg_geometry(data: &[u8]) -> Option<VectorGeometry> {
    if data.len() < 8 {
        return None;
    }

    // GeoPackage Binary header:
    // bytes 0-1: magic 'GP' (0x47, 0x50)
    // byte 2: version
    // byte 3: flags (bits 1-3 = envelope type, bit 0 = byte order)
    // bytes 4-7: srs_id (i32)
    // then: envelope (variable size depending on flags), then WKB

    if data[0] != 0x47 || data[1] != 0x50 {
        // Not a GeoPackage binary — try plain WKB
        return parse_wkb(data);
    }

    let flags = data[3];
    let envelope_indicator = (flags >> 1) & 0x07;
    let envelope_size = match envelope_indicator {
        0 => 0,       // no envelope
        1 => 32,      // [minx, maxx, miny, maxy]
        2 => 48,      // + [minz, maxz]
        3 => 48,      // + [minm, maxm]
        4 => 64,      // + [minz, maxz, minm, maxm]
        _ => return None,
    };

    let wkb_offset = 8 + envelope_size;
    if data.len() <= wkb_offset {
        return None;
    }

    parse_wkb(&data[wkb_offset..])
}

/// Parse standard WKB (Well-Known Binary) geometry.
fn parse_wkb(data: &[u8]) -> Option<VectorGeometry> {
    if data.len() < 5 {
        return None;
    }

    let byte_order = data[0]; // 0 = big-endian, 1 = little-endian
    let le = byte_order == 1;

    let geom_type = read_u32(&data[1..5], le);
    // Mask out Z/M/SRID flags
    let base_type = geom_type & 0xFF;

    let body = &data[5..];

    match base_type {
        1 => parse_wkb_point(body, le),
        2 => parse_wkb_linestring(body, le),
        3 => parse_wkb_polygon(body, le),
        4 => parse_wkb_multipoint(body, le),
        5 => parse_wkb_multilinestring(body, le),
        6 => parse_wkb_multipolygon(body, le),
        _ => None,
    }
}

fn parse_wkb_point(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 16 {
        return None;
    }
    let x = read_f64(&data[0..8], le);
    let y = read_f64(&data[8..16], le);
    Some(VectorGeometry::Point(x, y))
}

fn parse_wkb_linestring(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 4 {
        return None;
    }
    let num_points = read_u32(&data[0..4], le) as usize;
    let mut coords = Vec::with_capacity(num_points);
    let mut offset = 4;
    for _ in 0..num_points {
        if offset + 16 > data.len() {
            break;
        }
        let x = read_f64(&data[offset..offset + 8], le);
        let y = read_f64(&data[offset + 8..offset + 16], le);
        coords.push((x, y));
        offset += 16;
    }
    Some(VectorGeometry::LineString(coords))
}

fn parse_wkb_polygon(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 4 {
        return None;
    }
    let num_rings = read_u32(&data[0..4], le) as usize;
    let mut rings = Vec::with_capacity(num_rings);
    let mut offset = 4;
    for _ in 0..num_rings {
        if offset + 4 > data.len() {
            break;
        }
        let num_points = read_u32(&data[offset..offset + 4], le) as usize;
        offset += 4;
        let mut ring = Vec::with_capacity(num_points);
        for _ in 0..num_points {
            if offset + 16 > data.len() {
                break;
            }
            let x = read_f64(&data[offset..offset + 8], le);
            let y = read_f64(&data[offset + 8..offset + 16], le);
            ring.push((x, y));
            offset += 16;
        }
        rings.push(ring);
    }
    Some(VectorGeometry::Polygon(rings))
}

fn parse_wkb_multipoint(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 4 {
        return None;
    }
    let count = read_u32(&data[0..4], le) as usize;
    let mut points = Vec::with_capacity(count);
    let mut offset = 4;
    for _ in 0..count {
        if offset + 21 > data.len() {
            break;
        }
        // Each point has its own WKB header (5 bytes) + 16 bytes data
        let pt_le = data[offset] == 1;
        let x = read_f64(&data[offset + 5..offset + 13], pt_le);
        let y = read_f64(&data[offset + 13..offset + 21], pt_le);
        points.push((x, y));
        offset += 21;
    }
    Some(VectorGeometry::MultiPoint(points))
}

fn parse_wkb_multilinestring(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 4 {
        return None;
    }
    let count = read_u32(&data[0..4], le) as usize;
    let mut lines = Vec::with_capacity(count);
    let mut offset = 4;
    for _ in 0..count {
        if offset + 9 > data.len() {
            break;
        }
        // Skip WKB header for each linestring
        let ls_le = data[offset] == 1;
        offset += 5;
        if offset + 4 > data.len() {
            break;
        }
        let num_pts = read_u32(&data[offset..offset + 4], ls_le) as usize;
        offset += 4;
        let mut line = Vec::with_capacity(num_pts);
        for _ in 0..num_pts {
            if offset + 16 > data.len() {
                break;
            }
            let x = read_f64(&data[offset..offset + 8], ls_le);
            let y = read_f64(&data[offset + 8..offset + 16], ls_le);
            line.push((x, y));
            offset += 16;
        }
        lines.push(line);
    }
    Some(VectorGeometry::MultiLineString(lines))
}

fn parse_wkb_multipolygon(data: &[u8], le: bool) -> Option<VectorGeometry> {
    if data.len() < 4 {
        return None;
    }
    let count = read_u32(&data[0..4], le) as usize;
    let mut polygons = Vec::with_capacity(count);
    let mut offset = 4;
    for _ in 0..count {
        if offset + 9 > data.len() {
            break;
        }
        let poly_le = data[offset] == 1;
        offset += 5;
        if offset + 4 > data.len() {
            break;
        }
        let num_rings = read_u32(&data[offset..offset + 4], poly_le) as usize;
        offset += 4;
        let mut rings = Vec::with_capacity(num_rings);
        for _ in 0..num_rings {
            if offset + 4 > data.len() {
                break;
            }
            let num_pts = read_u32(&data[offset..offset + 4], poly_le) as usize;
            offset += 4;
            let mut ring = Vec::with_capacity(num_pts);
            for _ in 0..num_pts {
                if offset + 16 > data.len() {
                    break;
                }
                let x = read_f64(&data[offset..offset + 8], poly_le);
                let y = read_f64(&data[offset + 8..offset + 16], poly_le);
                ring.push((x, y));
                offset += 16;
            }
            rings.push(ring);
        }
        polygons.push(rings);
    }
    Some(VectorGeometry::MultiPolygon(polygons))
}

fn read_u32(data: &[u8], le: bool) -> u32 {
    let bytes: [u8; 4] = data[..4].try_into().unwrap();
    if le {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    }
}

fn read_f64(data: &[u8], le: bool) -> f64 {
    let bytes: [u8; 8] = data[..8].try_into().unwrap();
    if le {
        f64::from_le_bytes(bytes)
    } else {
        f64::from_be_bytes(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_minimal_gpkg() {
        let dir = std::env::temp_dir().join("tiletopia_gpkg_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.gpkg");

        // Create a minimal GeoPackage SQLite database
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE gpkg_contents (
                table_name TEXT NOT NULL,
                data_type TEXT NOT NULL,
                identifier TEXT,
                description TEXT DEFAULT '',
                last_change TEXT,
                min_x DOUBLE,
                min_y DOUBLE,
                max_x DOUBLE,
                max_y DOUBLE,
                srs_id INTEGER
            );
            CREATE TABLE gpkg_geometry_columns (
                table_name TEXT NOT NULL,
                column_name TEXT NOT NULL,
                geometry_type_name TEXT NOT NULL,
                srs_id INTEGER NOT NULL,
                z INTEGER NOT NULL,
                m INTEGER NOT NULL
            );
            INSERT INTO gpkg_contents (table_name, data_type) VALUES ('test_features', 'features');
            INSERT INTO gpkg_geometry_columns (table_name, column_name, geometry_type_name, srs_id, z, m)
                VALUES ('test_features', 'geom', 'POINT', 4326, 0, 0);
            CREATE TABLE test_features (
                fid INTEGER PRIMARY KEY,
                geom BLOB,
                name TEXT
            );
            ",
        )
        .unwrap();

        // Insert a point using GeoPackage Binary format (GP header + WKB)
        let mut geom_blob = Vec::new();
        // GP header
        geom_blob.push(0x47); // 'G'
        geom_blob.push(0x50); // 'P'
        geom_blob.push(0x00); // version
        geom_blob.push(0x01); // flags: little-endian, no envelope
        geom_blob.extend_from_slice(&4326i32.to_le_bytes()); // srs_id
        // WKB Point (little-endian)
        geom_blob.push(0x01); // byte order = LE
        geom_blob.extend_from_slice(&1u32.to_le_bytes()); // wkbPoint
        geom_blob.extend_from_slice(&10.0f64.to_le_bytes()); // x
        geom_blob.extend_from_slice(&20.0f64.to_le_bytes()); // y

        conn.execute(
            "INSERT INTO test_features (fid, geom, name) VALUES (1, ?1, 'TestPoint')",
            [&geom_blob],
        )
        .unwrap();

        drop(conn);

        let features = read(&path).unwrap();
        assert_eq!(features.len(), 1);
        match &features[0].geometry {
            VectorGeometry::Point(x, y) => {
                assert!((x - 10.0).abs() < 1e-10);
                assert!((y - 20.0).abs() < 1e-10);
            }
            other => panic!("expected Point, got {:?}", other),
        }
        assert_eq!(features[0].properties.get("name").unwrap(), "TestPoint");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_parse_wkb_polygon() {
        // Little-endian WKB Polygon
        let mut data = Vec::new();
        data.push(0x01); // LE
        data.extend_from_slice(&3u32.to_le_bytes()); // wkbPolygon
        data.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
        data.extend_from_slice(&4u32.to_le_bytes()); // 4 points
        for &(x, y) in &[(0.0f64, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)] {
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&(y as f64).to_le_bytes());
        }

        let geom = parse_wkb(&data).unwrap();
        match geom {
            VectorGeometry::Polygon(rings) => {
                assert_eq!(rings.len(), 1);
                assert_eq!(rings[0].len(), 4);
            }
            other => panic!("expected Polygon, got {:?}", other),
        }
    }
}
