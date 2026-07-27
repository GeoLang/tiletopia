//! Web Mercator (slippy XYZ) tiling for the terrain-RGB raster endpoint.
//!
//! Deliberately a different scheme from the quantized-mesh endpoint, which is
//! geographic ([`crate::global_dem::TerrainTileCoord`]): raster-DEM clients
//! want mercator, Cesium wants geographic. The two coordinate types are
//! separate so a tile index cannot be handed to the wrong one.

/// A Web Mercator tile: one tile at zoom 0, y counting south from the top.
#[derive(Debug, Clone, Copy)]
pub struct MercatorTileCoord {
    pub zoom: u32,
    pub x: u32,
    pub y: u32,
}

impl MercatorTileCoord {
    /// Tiles per axis at a zoom level.
    pub fn tiles_at_zoom(zoom: u32) -> u32 {
        1u32.checked_shl(zoom).unwrap_or(u32::MAX)
    }

    /// Geographic bounds as [west, south, east, north] in degrees.
    pub fn bounds(&self) -> [f64; 4] {
        [
            self.lon_at(0.0),
            self.lat_at(1.0),
            self.lon_at(1.0),
            self.lat_at(0.0),
        ]
    }

    /// Longitude at a fraction across the tile, 0 at the west edge.
    pub fn lon_at(&self, u: f64) -> f64 {
        let n = Self::tiles_at_zoom(self.zoom) as f64;
        (self.x as f64 + u) / n * 360.0 - 180.0
    }

    /// Latitude at a fraction down the tile, 0 at the north edge.
    ///
    /// Nonlinear in mercator, so per-pixel latitude has to come through here
    /// rather than from interpolating the tile's north and south bounds.
    pub fn lat_at(&self, v: f64) -> f64 {
        let n = Self::tiles_at_zoom(self.zoom) as f64;
        let y = (self.y as f64 + v) / n;
        let angle = std::f64::consts::PI * (1.0 - 2.0 * y);
        angle.sinh().atan().to_degrees()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_zero_is_one_tile_spanning_the_mercator_world() {
        let world = MercatorTileCoord {
            zoom: 0,
            x: 0,
            y: 0,
        };
        let [west, south, east, north] = world.bounds();
        assert_eq!(west, -180.0);
        assert_eq!(east, 180.0);
        assert!((north - 85.0511).abs() < 0.001);
        assert!((south + 85.0511).abs() < 0.001);
    }

    #[test]
    fn latitude_runs_north_to_south_and_is_not_linear() {
        // zoom 1 top-left: north edge at the mercator limit, south edge at 0
        let tile = MercatorTileCoord {
            zoom: 1,
            x: 0,
            y: 0,
        };
        assert!((tile.lat_at(0.0) - 85.0511).abs() < 0.001);
        assert!(tile.lat_at(1.0).abs() < 1e-9);
        // the midpoint sits well north of the linear halfway mark
        assert!(tile.lat_at(0.5) > 60.0, "got {}", tile.lat_at(0.5));
    }

    #[test]
    fn known_tiles_cover_their_landmarks() {
        // z12 slippy tiles for the Monaco coast and, four tiles north, the
        // Col de Turini ridge
        for (y, lat) in [(1493, 43.735), (1489, 43.970)] {
            let tile = MercatorTileCoord {
                zoom: 12,
                x: 2132,
                y,
            };
            let [west, south, east, north] = tile.bounds();
            assert!(west < 7.40 && 7.40 < east, "lon {west}..{east}");
            assert!(south < lat && lat < north, "y{y}: lat {south}..{north}");
        }
    }
}
