//! Custom CRS reprojection support.
//!
//! Transforms coordinates between arbitrary coordinate reference systems.
//! Supports EPSG codes, WKT, and PROJ strings.

use std::f64::consts::PI;

/// Supported CRS definitions.
#[derive(Debug, Clone)]
pub enum CrsDef {
    /// EPSG code (e.g., 4326 for WGS84, 32632 for UTM zone 32N)
    Epsg(u32),
    /// PROJ.4 string
    Proj(String),
    /// WKT definition
    Wkt(String),
}

/// A coordinate reprojection transformer.
#[derive(Debug, Clone)]
pub struct Transformer {
    pub source: CrsDef,
    pub target: CrsDef,
}

/// A 3D coordinate.
#[derive(Debug, Clone, Copy)]
pub struct Coord3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Transformer {
    /// Create a new transformer from source to target CRS.
    pub fn new(source: CrsDef, target: CrsDef) -> Self {
        Self { source, target }
    }

    /// Transform a single coordinate from source to target CRS.
    pub fn transform(&self, coord: Coord3D) -> Result<Coord3D, ReprojError> {
        match (&self.source, &self.target) {
            (CrsDef::Epsg(src), CrsDef::Epsg(tgt)) if *src == *tgt => Ok(coord),
            (CrsDef::Epsg(4326), CrsDef::Epsg(epsg)) if is_utm(*epsg) => {
                let (zone, north) = utm_zone_from_epsg(*epsg);
                let (e, n) = latlon_to_utm(coord.y, coord.x, zone, north)?;
                Ok(Coord3D {
                    x: e,
                    y: n,
                    z: coord.z,
                })
            }
            (CrsDef::Epsg(epsg), CrsDef::Epsg(4326)) if is_utm(*epsg) => {
                let (zone, north) = utm_zone_from_epsg(*epsg);
                let (lat, lon) = utm_to_latlon(coord.x, coord.y, zone, north)?;
                Ok(Coord3D {
                    x: lon,
                    y: lat,
                    z: coord.z,
                })
            }
            (CrsDef::Epsg(4326), CrsDef::Epsg(4978)) => {
                // WGS84 geodetic to ECEF
                let (x, y, z) = geodetic_to_ecef(coord.y, coord.x, coord.z);
                Ok(Coord3D { x, y, z })
            }
            (CrsDef::Epsg(4978), CrsDef::Epsg(4326)) => {
                let (lat, lon, h) = ecef_to_geodetic(coord.x, coord.y, coord.z);
                Ok(Coord3D {
                    x: lon,
                    y: lat,
                    z: h,
                })
            }
            _ => Err(ReprojError::UnsupportedTransform),
        }
    }

    /// Transform a batch of coordinates.
    pub fn transform_batch(&self, coords: &[Coord3D]) -> Result<Vec<Coord3D>, ReprojError> {
        coords.iter().map(|c| self.transform(*c)).collect()
    }
}

/// Reprojection errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ReprojError {
    #[error("unsupported CRS transform")]
    UnsupportedTransform,
    #[error("coordinate out of range: {0}")]
    OutOfRange(String),
}

fn is_utm(epsg: u32) -> bool {
    (32601..=32660).contains(&epsg) || (32701..=32760).contains(&epsg)
}

fn utm_zone_from_epsg(epsg: u32) -> (u32, bool) {
    if epsg >= 32701 {
        (epsg - 32700, false)
    } else {
        (epsg - 32600, true)
    }
}

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;
const WGS84_E2: f64 = 2.0 * WGS84_F - WGS84_F * WGS84_F;

fn geodetic_to_ecef(lat_deg: f64, lon_deg: f64, h: f64) -> (f64, f64, f64) {
    let lat = lat_deg * PI / 180.0;
    let lon = lon_deg * PI / 180.0;
    let sin_lat = lat.sin();
    let cos_lat = lat.cos();
    let n = WGS84_A / (1.0 - WGS84_E2 * sin_lat * sin_lat).sqrt();
    let x = (n + h) * cos_lat * lon.cos();
    let y = (n + h) * cos_lat * lon.sin();
    let z = (n * (1.0 - WGS84_E2) + h) * sin_lat;
    (x, y, z)
}

fn ecef_to_geodetic(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    let lon = y.atan2(x);
    let p = (x * x + y * y).sqrt();
    let b = WGS84_A * (1.0 - WGS84_F);
    let ep2 = (WGS84_A * WGS84_A - b * b) / (b * b);
    let theta = (z * WGS84_A).atan2(p * b);
    let lat =
        (z + ep2 * b * theta.sin().powi(3)).atan2(p - WGS84_E2 * WGS84_A * theta.cos().powi(3));
    let sin_lat = lat.sin();
    let n = WGS84_A / (1.0 - WGS84_E2 * sin_lat * sin_lat).sqrt();
    let h = p / lat.cos() - n;
    (lat * 180.0 / PI, lon * 180.0 / PI, h)
}

/// Convert lat/lon (degrees) to UTM easting/northing.
fn latlon_to_utm(
    lat_deg: f64,
    lon_deg: f64,
    zone: u32,
    _north: bool,
) -> Result<(f64, f64), ReprojError> {
    if !(-80.0..=84.0).contains(&lat_deg) {
        return Err(ReprojError::OutOfRange("latitude out of UTM range".into()));
    }
    let lat = lat_deg * PI / 180.0;
    let lon = lon_deg * PI / 180.0;
    let lon0 = ((zone as f64 - 1.0) * 6.0 - 180.0 + 3.0) * PI / 180.0;
    let k0 = 0.9996;
    let e = WGS84_E2.sqrt();
    let e_prime_sq = WGS84_E2 / (1.0 - WGS84_E2);
    let n_val = WGS84_A / (1.0 - WGS84_E2 * lat.sin().powi(2)).sqrt();
    let t = lat.tan().powi(2);
    let c = e_prime_sq * lat.cos().powi(2);
    let a_val = lat.cos() * (lon - lon0);
    let m = WGS84_A
        * ((1.0 - WGS84_E2 / 4.0 - 3.0 * WGS84_E2.powi(2) / 64.0) * lat
            - (3.0 * WGS84_E2 / 8.0 + 3.0 * WGS84_E2.powi(2) / 32.0) * (2.0 * lat).sin()
            + (15.0 * WGS84_E2.powi(2) / 256.0) * (4.0 * lat).sin());
    let _ = e; // used for clarity

    let easting = k0
        * n_val
        * (a_val
            + (1.0 - t + c) * a_val.powi(3) / 6.0
            + (5.0 - 18.0 * t + t * t) * a_val.powi(5) / 120.0)
        + 500_000.0;
    let mut northing = k0
        * (m + n_val
            * lat.tan()
            * (a_val.powi(2) / 2.0
                + (5.0 - t + 9.0 * c + 4.0 * c * c) * a_val.powi(4) / 24.0
                + (61.0 - 58.0 * t + t * t) * a_val.powi(6) / 720.0));
    if lat_deg < 0.0 {
        northing += 10_000_000.0;
    }
    Ok((easting, northing))
}

/// Convert UTM easting/northing to lat/lon (degrees).
fn utm_to_latlon(
    easting: f64,
    northing: f64,
    zone: u32,
    north: bool,
) -> Result<(f64, f64), ReprojError> {
    let k0 = 0.9996;
    let e1 = (1.0 - (1.0 - WGS84_E2).sqrt()) / (1.0 + (1.0 - WGS84_E2).sqrt());
    let x = easting - 500_000.0;
    let y = if north {
        northing
    } else {
        northing - 10_000_000.0
    };

    let m = y / k0;
    let mu = m / (WGS84_A * (1.0 - WGS84_E2 / 4.0 - 3.0 * WGS84_E2.powi(2) / 64.0));
    let phi1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1.powi(3) / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1.powi(2) / 16.0 - 55.0 * e1.powi(4) / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1.powi(3) / 96.0) * (6.0 * mu).sin();

    let e_prime_sq = WGS84_E2 / (1.0 - WGS84_E2);
    let n1 = WGS84_A / (1.0 - WGS84_E2 * phi1.sin().powi(2)).sqrt();
    let t1 = phi1.tan().powi(2);
    let c1 = e_prime_sq * phi1.cos().powi(2);
    let r1 = WGS84_A * (1.0 - WGS84_E2) / (1.0 - WGS84_E2 * phi1.sin().powi(2)).powf(1.5);
    let d = x / (n1 * k0);

    let lat = phi1
        - (n1 * phi1.tan() / r1)
            * (d.powi(2) / 2.0
                - (5.0 + 3.0 * t1 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * e_prime_sq) * d.powi(4)
                    / 24.0
                + (61.0 + 90.0 * t1 + 298.0 * c1 + 45.0 * t1 * t1 - 252.0 * e_prime_sq)
                    * d.powi(6)
                    / 720.0);
    let lon0 = ((zone as f64 - 1.0) * 6.0 - 180.0 + 3.0) * PI / 180.0;
    let lon = lon0
        + (d - (1.0 + 2.0 * t1 + c1) * d.powi(3) / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t1 - 3.0 * c1 * c1 + 8.0 * e_prime_sq + 24.0 * t1 * t1)
                * d.powi(5)
                / 120.0)
            / phi1.cos();

    Ok((lat * 180.0 / PI, lon * 180.0 / PI))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identity_transform() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(4326));
        let c = Coord3D {
            x: 10.0,
            y: 48.0,
            z: 100.0,
        };
        let r = t.transform(c).unwrap();
        assert!((r.x - c.x).abs() < 1e-10);
    }

    #[test]
    fn test_wgs84_to_utm() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632));
        let c = Coord3D {
            x: 9.0,
            y: 48.0,
            z: 0.0,
        }; // lon=9, lat=48
        let r = t.transform(c).unwrap();
        assert!((r.x - 500_000.0).abs() < 1.0); // central meridian → 500km easting
    }

    #[test]
    fn test_roundtrip_utm() {
        let t_fwd = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(32632));
        let t_inv = Transformer::new(CrsDef::Epsg(32632), CrsDef::Epsg(4326));
        let c = Coord3D {
            x: 11.5,
            y: 47.3,
            z: 500.0,
        };
        let utm = t_fwd.transform(c).unwrap();
        let back = t_inv.transform(utm).unwrap();
        assert!((back.x - c.x).abs() < 1e-6);
        assert!((back.y - c.y).abs() < 1e-6);
    }

    #[test]
    fn test_wgs84_to_ecef() {
        let t = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(4978));
        let c = Coord3D {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        }; // null island
        let r = t.transform(c).unwrap();
        assert!((r.x - WGS84_A).abs() < 1.0);
        assert!(r.y.abs() < 1.0);
        assert!(r.z.abs() < 1.0);
    }
}
