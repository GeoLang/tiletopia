//! CRS detection and auto-reprojection for ingested geospatial data.
//!
//! Detects coordinate reference systems from LAS VLRs, GeoTIFF tags,
//! and .prj sidecar files, then reprojects to WGS84 when needed.

use std::path::Path;
use tiletopia_core::crs::{Coord3D, CrsDef, Transformer};

/// Detected coordinate reference system.
#[derive(Debug, Clone)]
pub enum DetectedCrs {
    /// EPSG:4326 (lon/lat/height)
    Wgs84,
    /// EPSG:4978 (X/Y/Z)
    Ecef,
    /// EPSG:326XX or 327XX
    Utm { zone: u32, north: bool },
    /// Any other EPSG code
    Epsg(u32),
    /// Could not determine CRS
    Unknown,
}

impl DetectedCrs {
    fn to_epsg(&self) -> Option<u32> {
        match self {
            Self::Wgs84 => Some(4326),
            Self::Ecef => Some(4978),
            Self::Utm { zone, north } => {
                if *north {
                    Some(32600 + zone)
                } else {
                    Some(32700 + zone)
                }
            }
            Self::Epsg(code) => Some(*code),
            Self::Unknown => None,
        }
    }
}

/// Classify an EPSG code into a DetectedCrs variant.
fn classify_epsg(code: u32) -> DetectedCrs {
    match code {
        4326 => DetectedCrs::Wgs84,
        4978 => DetectedCrs::Ecef,
        c if (32601..=32660).contains(&c) => DetectedCrs::Utm {
            zone: c - 32600,
            north: true,
        },
        c if (32701..=32760).contains(&c) => DetectedCrs::Utm {
            zone: c - 32700,
            north: false,
        },
        c => DetectedCrs::Epsg(c),
    }
}

/// Detect CRS from a LAS file's VLR records.
///
/// Looks for the GeoKeyDirectoryTag VLR (user_id "LASF_Projection",
/// record_id 34735) and parses GeoKeys to find EPSG codes.
pub fn detect_crs_from_las(path: &Path) -> DetectedCrs {
    let reader = match las::Reader::from_path(path) {
        Ok(r) => r,
        Err(_) => return DetectedCrs::Unknown,
    };

    for vlr in reader.header().vlrs() {
        if vlr.user_id == "LASF_Projection" && vlr.record_id == 34735 {
            return parse_geo_key_directory(&vlr.data);
        }
    }

    DetectedCrs::Unknown
}

/// Parse a GeoKeyDirectoryTag VLR payload to extract EPSG codes.
///
/// The directory is an array of u16 values:
///   [key_directory_version, key_revision, minor_revision, num_keys,
///    key_id_0, tiff_tag_location_0, count_0, value_offset_0,
///    key_id_1, ...]
///
/// We look for ProjectedCSTypeGeoKey (3072) and GeographicTypeGeoKey (2048).
fn parse_geo_key_directory(data: &[u8]) -> DetectedCrs {
    if data.len() < 8 {
        return DetectedCrs::Unknown;
    }

    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    if u16s.len() < 4 {
        return DetectedCrs::Unknown;
    }

    let num_keys = u16s[3] as usize;

    for i in 0..num_keys {
        let base = 4 + i * 4;
        if base + 3 >= u16s.len() {
            break;
        }
        let key_id = u16s[base];
        let tiff_tag_location = u16s[base + 1];
        let value_offset = u16s[base + 3];

        // Only handle inline values (tiff_tag_location == 0)
        if tiff_tag_location != 0 {
            continue;
        }

        match key_id {
            // ProjectedCSTypeGeoKey — preferred for projected CRS
            3072 if value_offset != 0 && value_offset != 32767 => {
                return classify_epsg(value_offset as u32);
            }
            // GeographicTypeGeoKey — geographic CRS
            2048 if value_offset != 0 && value_offset != 32767 => {
                return classify_epsg(value_offset as u32);
            }
            _ => {}
        }
    }

    DetectedCrs::Unknown
}

/// Detect CRS from a GeoTIFF's tags.
///
/// Reads tag 34735 (GeoKeyDirectoryTag) for EPSG codes, and
/// tag 34737 (GeoAsciiParamsTag) for WKT-based identification.
pub fn detect_crs_from_geotiff(path: &Path) -> DetectedCrs {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return DetectedCrs::Unknown,
    };
    let reader = std::io::BufReader::new(file);

    let mut decoder = match tiff::decoder::Decoder::new(reader) {
        Ok(d) => d,
        Err(_) => return DetectedCrs::Unknown,
    };

    // Try GeoKeyDirectoryTag (34735)
    if let Ok(values) = decoder.get_tag_u16_vec(tiff::tags::Tag::Unknown(34735)) {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let crs = parse_geo_key_directory(&bytes);
        if !matches!(crs, DetectedCrs::Unknown) {
            return crs;
        }
    }

    // Try GeoAsciiParamsTag (34737) for WKT strings
    if let Ok(ascii) = decoder.get_tag_ascii_string(tiff::tags::Tag::Unknown(34737)) {
        return detect_crs_from_wkt(&ascii);
    }

    DetectedCrs::Unknown
}

/// Detect CRS from a .prj sidecar file (ESRI WKT format).
pub fn detect_crs_from_prj(path: &Path) -> DetectedCrs {
    let prj_path = path.with_extension("prj");
    let content = match std::fs::read_to_string(&prj_path) {
        Ok(s) => s,
        Err(_) => return DetectedCrs::Unknown,
    };
    detect_crs_from_wkt(&content)
}

/// Basic WKT CRS detection.
fn detect_crs_from_wkt(wkt: &str) -> DetectedCrs {
    let upper = wkt.to_uppercase();

    // Check for UTM zones (underscore format)
    if let Some(crs) = parse_utm_from_wkt(&upper, "UTM_ZONE_") {
        return crs;
    }

    // Check for UTM in "Zone XX" format
    if let Some(crs) = parse_utm_from_wkt(&upper, "UTM ZONE ") {
        return crs;
    }

    // WGS 84
    if (upper.contains("GCS_WGS_1984")
        || upper.contains("\"WGS 84\"")
        || upper.contains("\"WGS_1984\""))
        && !upper.starts_with("PROJCS")
    {
        return DetectedCrs::Wgs84;
    }

    // Try to find EPSG code in AUTHORITY tag
    if let Some(pos) = upper.find("AUTHORITY[\"EPSG\",\"") {
        let after = &upper[pos + 18..];
        if let Some(code) = after
            .find('"')
            .and_then(|end| after[..end].parse::<u32>().ok())
        {
            return classify_epsg(code);
        }
    }

    DetectedCrs::Unknown
}

fn parse_utm_from_wkt(upper: &str, pattern: &str) -> Option<DetectedCrs> {
    let pos = upper.find(pattern)?;
    let after = &upper[pos + pattern.len()..];
    let zone_str = after.split(|c: char| !c.is_ascii_digit()).next()?;
    let zone: u32 = zone_str.parse().ok()?;
    if !(1..=60).contains(&zone) {
        return None;
    }
    let north = !upper.contains("SOUTH") && !after.starts_with('S');
    Some(DetectedCrs::Utm { zone, north })
}

/// Attempt to detect CRS for any file (tries all methods).
pub fn detect_crs(path: &Path) -> DetectedCrs {
    match path.extension().and_then(|e| e.to_str()) {
        Some("las" | "laz") => {
            let crs = detect_crs_from_las(path);
            if matches!(crs, DetectedCrs::Unknown) {
                detect_crs_from_prj(path)
            } else {
                crs
            }
        }
        Some("tif" | "tiff") => {
            let crs = detect_crs_from_geotiff(path);
            if matches!(crs, DetectedCrs::Unknown) {
                detect_crs_from_prj(path)
            } else {
                crs
            }
        }
        _ => detect_crs_from_prj(path),
    }
}

/// Reproject a slice of 3D points from source CRS to WGS84 (lon, lat, height).
pub fn reproject_to_wgs84(points: &mut [[f64; 3]], from: &DetectedCrs) {
    let src_epsg = match from.to_epsg() {
        Some(epsg) => epsg,
        None => return,
    };

    if src_epsg == 4326 {
        return;
    }

    let transformer = Transformer::new(CrsDef::Epsg(src_epsg), CrsDef::Epsg(4326));
    let coords: Vec<Coord3D> = points
        .iter()
        .map(|pt| Coord3D {
            x: pt[0],
            y: pt[1],
            z: pt[2],
        })
        .collect();

    let Ok(transformed) = transformer.transform_batch(&coords) else {
        return;
    };

    for (pt, out) in points.iter_mut().zip(transformed) {
        pt[0] = out.x;
        pt[1] = out.y;
        pt[2] = out.z;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_wgs84() {
        assert!(matches!(classify_epsg(4326), DetectedCrs::Wgs84));
    }

    #[test]
    fn classify_ecef() {
        assert!(matches!(classify_epsg(4978), DetectedCrs::Ecef));
    }

    #[test]
    fn classify_utm_north() {
        match classify_epsg(32632) {
            DetectedCrs::Utm { zone, north } => {
                assert_eq!(zone, 32);
                assert!(north);
            }
            other => panic!("expected UTM, got {other:?}"),
        }
    }

    #[test]
    fn classify_utm_south() {
        match classify_epsg(32755) {
            DetectedCrs::Utm { zone, north } => {
                assert_eq!(zone, 55);
                assert!(!north);
            }
            other => panic!("expected UTM, got {other:?}"),
        }
    }

    #[test]
    fn parse_geo_key_directory_projected() {
        // Simulate a GeoKeyDirectoryTag VLR with ProjectedCSTypeGeoKey = 32632
        let mut data = Vec::new();
        let push_u16 = |v: &mut Vec<u8>, val: u16| v.extend_from_slice(&val.to_le_bytes());

        // Header: version=1, revision=1, minor=0, numKeys=1
        push_u16(&mut data, 1);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);

        // Key: ProjectedCSTypeGeoKey(3072), tiffTagLocation=0, count=1, value=32632
        push_u16(&mut data, 3072);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 32632);

        match parse_geo_key_directory(&data) {
            DetectedCrs::Utm { zone, north } => {
                assert_eq!(zone, 32);
                assert!(north);
            }
            other => panic!("expected UTM zone 32N, got {other:?}"),
        }
    }

    #[test]
    fn parse_geo_key_directory_geographic() {
        let mut data = Vec::new();
        let push_u16 = |v: &mut Vec<u8>, val: u16| v.extend_from_slice(&val.to_le_bytes());

        push_u16(&mut data, 1);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);

        // GeographicTypeGeoKey(2048) = 4326
        push_u16(&mut data, 2048);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 4326);

        assert!(matches!(parse_geo_key_directory(&data), DetectedCrs::Wgs84));
    }

    #[test]
    fn wkt_utm_detection() {
        let wkt = r#"PROJCS["WGS 84 / UTM_Zone_32N",GEOGCS["GCS_WGS_1984"]]"#;
        match detect_crs_from_wkt(wkt) {
            DetectedCrs::Utm { zone, north } => {
                assert_eq!(zone, 32);
                assert!(north);
            }
            other => panic!("expected UTM zone 32N, got {other:?}"),
        }
    }

    #[test]
    fn wkt_wgs84_detection() {
        let wkt = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984"]]"#;
        assert!(matches!(detect_crs_from_wkt(wkt), DetectedCrs::Wgs84));
    }

    #[test]
    fn wkt_authority_detection() {
        let wkt = r#"GEOGCS["foo",AUTHORITY["EPSG","4978"]]"#;
        assert!(matches!(detect_crs_from_wkt(wkt), DetectedCrs::Ecef));
    }

    #[test]
    fn reproject_utm_to_wgs84() {
        // Zurich is roughly at UTM 32N: easting=465000, northing=5248000
        let mut points = [[465_000.0, 5_248_000.0, 400.0]];
        reproject_to_wgs84(
            &mut points,
            &DetectedCrs::Utm {
                zone: 32,
                north: true,
            },
        );

        // Should be roughly lon=8.5, lat=47.4
        assert!((points[0][0] - 8.5).abs() < 0.5, "lon={}", points[0][0]);
        assert!((points[0][1] - 47.4).abs() < 0.5, "lat={}", points[0][1]);
    }

    #[test]
    fn reproject_ecef_to_wgs84() {
        // ECEF for roughly (lat=0, lon=0, h=0) → (WGS84_A, 0, 0)
        let a = 6_378_137.0;
        let mut points = [[a, 0.0, 0.0]];
        reproject_to_wgs84(&mut points, &DetectedCrs::Ecef);

        assert!((points[0][0]).abs() < 0.001, "lon={}", points[0][0]);
        assert!((points[0][1]).abs() < 0.001, "lat={}", points[0][1]);
    }

    #[test]
    fn detect_unknown_for_missing_file() {
        let crs = detect_crs(Path::new("/nonexistent/file.las"));
        assert!(matches!(crs, DetectedCrs::Unknown));
    }
}
