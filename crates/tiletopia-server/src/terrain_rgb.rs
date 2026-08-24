//! Terrain-RGB raster tiles for raster-DEM clients (MapLibre, deck.gl).
//!
//! Web Mercator XYZ, unlike the geographic quantized-mesh endpoint next door in
//! [`crate::terrain_api`]. Both read the same DEM through
//! [`crate::elevation::ElevationSources::dem_tiles`]; only the tile indexing
//! differs.

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use tiletopia_terrain::global_dem::{DemTile, sample_dem_tiles};
use tiletopia_terrain::mercator::MercatorTileCoord;

use crate::AppState;
use crate::terrain_api::Refusal;

/// Tile edge in pixels, the raster-DEM standard.
const TILE_PX: u32 = 256;

/// Deepest zoom served. SRTM is ~30 m, so past this the tiles only upsample.
const MAX_RGB_ZOOM: u32 = 15;

/// Register the terrain-RGB route.
pub fn terrain_rgb_routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/terrain/rgb/{z}/{x}/{y}", get(serve_terrain_rgb))
}

/// Serve one terrain-RGB PNG tile.
async fn serve_terrain_rgb(
    State(state): State<Arc<AppState>>,
    Path((z, x, y)): Path<(u32, u32, String)>,
) -> Result<impl IntoResponse, Refusal> {
    let coord = parse_rgb_coord(z, x, &y).ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    let dem_tiles = state
        .elevation_sources()
        .dem_tiles(coord.bounds())
        .await?
        .tiles;
    let png = render_terrain_rgb(&coord, &dem_tiles);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/png".parse().unwrap());
    headers.insert(
        header::CACHE_CONTROL,
        "public, max-age=86400".parse().unwrap(),
    );
    Ok((headers, png))
}

/// Parse a tile coordinate from the path, where y arrives as `{y}.png`.
fn parse_rgb_coord(z: u32, x: u32, y: &str) -> Option<MercatorTileCoord> {
    let y: u32 = y.strip_suffix(".png").unwrap_or(y).parse().ok()?;
    if z > MAX_RGB_ZOOM {
        return None;
    }
    let tiles = MercatorTileCoord::tiles_at_zoom(z);
    if x >= tiles || y >= tiles {
        return None;
    }
    Some(MercatorTileCoord { zoom: z, x, y })
}

/// Render a tile as a Mapbox terrain-RGB PNG.
///
/// Row 0 is the tile's north edge, matching the raster tile convention.
fn render_terrain_rgb(coord: &MercatorTileCoord, dem_tiles: &[DemTile]) -> Vec<u8> {
    let mut image = image::RgbImage::new(TILE_PX, TILE_PX);
    for row in 0..TILE_PX {
        let lat = coord.lat_at((row as f64 + 0.5) / TILE_PX as f64);
        for col in 0..TILE_PX {
            let lon = coord.lon_at((col as f64 + 0.5) / TILE_PX as f64);
            // no coverage means ocean or a void, which encode as height 0
            let height = sample_dem_tiles(dem_tiles, lat, lon).unwrap_or(0.0);
            image.put_pixel(col, row, image::Rgb(encode_terrain_rgb(height)));
        }
    }

    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("writing PNG to a Vec cannot fail");
    png.into_inner()
}

/// Pack a height into Mapbox terrain-RGB, the inverse of
/// `height = -10000 + (R * 65536 + G * 256 + B) * 0.1`.
fn encode_terrain_rgb(height: f32) -> [u8; 3] {
    let value = ((height as f64 + 10000.0) / 0.1)
        .round()
        .clamp(0.0, 16_777_215.0) as u32;
    [(value >> 16) as u8, (value >> 8) as u8, value as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decode every raster-DEM client applies.
    fn decode_terrain_rgb(pixel: [u8; 3]) -> f64 {
        -10000.0 + (pixel[0] as f64 * 65536.0 + pixel[1] as f64 * 256.0 + pixel[2] as f64) * 0.1
    }

    #[test]
    fn encoding_round_trips_the_heights_that_matter() {
        for height in [0.0, -430.0, 8.8, 1658.0, 8848.0] {
            let decoded = decode_terrain_rgb(encode_terrain_rgb(height));
            assert!(
                (decoded - height as f64).abs() <= 0.05,
                "{height} decoded as {decoded}"
            );
        }
        // ocean is exactly zero, not a near miss
        assert_eq!(encode_terrain_rgb(0.0), [1, 134, 160]);
        assert_eq!(decode_terrain_rgb([1, 134, 160]), 0.0);
    }

    #[test]
    fn coordinates_out_of_range_are_rejected() {
        assert!(parse_rgb_coord(0, 0, "0.png").is_some());
        assert!(parse_rgb_coord(12, 2132, "1489.png").is_some());
        assert!(parse_rgb_coord(12, 2132, "1489").is_some());

        assert!(parse_rgb_coord(0, 1, "0.png").is_none()); // one tile at zoom 0
        assert!(parse_rgb_coord(0, 0, "1.png").is_none());
        assert!(parse_rgb_coord(MAX_RGB_ZOOM + 1, 0, "0.png").is_none());
        assert!(parse_rgb_coord(1, 0, "0.jpg").is_none());
    }

    /// A DEM cell that is coast in the south and mountain in the north, the
    /// real asymmetry of N43E007 (Mediterranean at 43.0, Maritime Alps at 44.0).
    fn coast_to_mountain_tile() -> DemTile {
        let samples = 64usize;
        // north-up rows, exactly how an HGT file is laid out
        let mut north_up = Vec::with_capacity(samples * samples);
        for row in 0..samples {
            // row 0 = north = 1658 m, last row = south = sea level
            let t = 1.0 - row as f32 / (samples - 1) as f32;
            north_up.extend(std::iter::repeat_n(t * 1658.0, samples));
        }
        DemTile::from_north_up(43, 7, north_up, samples as u32, -9999.0).unwrap()
    }

    /// Smallest zoom tile that still contains all of 43..44N at 7.4E.
    const N43E007_TILE: MercatorTileCoord = MercatorTileCoord {
        zoom: 6,
        x: 33,
        y: 23,
    };

    #[test]
    fn rendered_tile_puts_the_mountain_north_and_the_coast_south() {
        let coord = N43E007_TILE;
        let png = render_terrain_rgb(&coord, &[coast_to_mountain_tile()]);
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (TILE_PX, TILE_PX));

        let nearest = |target: f64, of: &dyn Fn(u32) -> f64| {
            (0..TILE_PX)
                .min_by(|&a, &b| {
                    (of(a) - target)
                        .abs()
                        .partial_cmp(&(of(b) - target).abs())
                        .unwrap()
                })
                .unwrap()
        };
        let row_lat = |row: u32| coord.lat_at((row as f64 + 0.5) / TILE_PX as f64);
        let col_lon = |col: u32| coord.lon_at((col as f64 + 0.5) / TILE_PX as f64);

        let col = nearest(7.4, &col_lon);
        let coast_row = nearest(43.03, &row_lat);
        let mountain_row = nearest(43.97, &row_lat);
        assert!(
            mountain_row < coast_row,
            "north must be the smaller row index"
        );

        let coast = decode_terrain_rgb(decoded.get_pixel(col, coast_row).0);
        let mountain = decode_terrain_rgb(decoded.get_pixel(col, mountain_row).0);

        // the flip bug swapped exactly these two readings
        assert!(coast < 80.0, "south edge should be coast, decoded {coast}");
        assert!(
            mountain > 1500.0,
            "north edge should be mountain, decoded {mountain}"
        );
    }

    #[test]
    fn tiles_with_no_dem_coverage_are_all_sea_level() {
        let coord = N43E007_TILE;
        let png = render_terrain_rgb(&coord, &[]);
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        for pixel in decoded.pixels() {
            assert_eq!(decode_terrain_rgb(pixel.0), 0.0);
        }
    }
}
