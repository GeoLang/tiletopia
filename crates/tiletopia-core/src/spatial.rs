//! Spatial utilities — coordinate transforms and helpers.

/// Convert degrees to radians.
pub fn deg_to_rad(deg: f64) -> f64 {
    deg * std::f64::consts::PI / 180.0
}

/// Convert radians to degrees.
pub fn rad_to_deg(rad: f64) -> f64 {
    rad * 180.0 / std::f64::consts::PI
}

/// WGS84 semi-major axis (meters).
pub const WGS84_A: f64 = 6_378_137.0;

/// WGS84 flattening.
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Convert geodetic (lat, lon, height) to ECEF (x, y, z).
/// lat/lon in radians, height in meters.
pub fn geodetic_to_ecef(lat: f64, lon: f64, height: f64) -> [f64; 3] {
    let e2 = 2.0 * WGS84_F - WGS84_F * WGS84_F;
    let sin_lat = lat.sin();
    let cos_lat = lat.cos();
    let n = WGS84_A / (1.0 - e2 * sin_lat * sin_lat).sqrt();

    [
        (n + height) * cos_lat * lon.cos(),
        (n + height) * cos_lat * lon.sin(),
        (n * (1.0 - e2) + height) * sin_lat,
    ]
}

/// Compute a 4x4 East-North-Up (ENU) to ECEF transform matrix at a given origin.
/// Returns column-major [f64; 16] suitable for 3D Tiles `transform`.
pub fn enu_to_ecef_matrix(lat: f64, lon: f64, height: f64) -> [f64; 16] {
    let origin = geodetic_to_ecef(lat, lon, height);
    let sin_lat = lat.sin();
    let cos_lat = lat.cos();
    let sin_lon = lon.sin();
    let cos_lon = lon.cos();

    // East, North, Up basis vectors
    let east = [-sin_lon, cos_lon, 0.0];
    let north = [-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat];
    let up = [cos_lat * cos_lon, cos_lat * sin_lon, sin_lat];

    // Column-major 4x4
    [
        east[0], east[1], east[2], 0.0, north[0], north[1], north[2], 0.0, up[0], up[1], up[2],
        0.0, origin[0], origin[1], origin[2], 1.0,
    ]
}
