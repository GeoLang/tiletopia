//! Elevation lookups over the DEM data this server holds.
//!
//! Two stores, read in this order: grids loaded into [`DemStore`], then the
//! one-degree tiles staged under `<data-dir>/dem/`, and behind those the SRTM
//! cache. Every elevation route, the terrain meshes and the analysis endpoints
//! read the same two, so a point, a profile and a hillshade of the same ground
//! agree.
//!
//! Nothing is invented. A query no DEM covers answers
//! [`ElevationGap::NoCoverage`] naming the location, and a tile that should be
//! there but cannot be read answers [`ElevationGap::Unreadable`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tiletopia_terrain::global_dem::DemTile;

/// Metres per degree of latitude, and per degree of longitude at the equator.
pub const METERS_PER_DEG_LAT: f64 = 111_320.0;

/// Most one-degree SRTM tiles a single request may pull from upstream.
///
/// These reads are anonymous, and a low-zoom terrain tile covers tens of
/// thousands of one-degree cells, so an unbounded fetch turns one GET into a
/// multi-terabyte download loop. Above the bound the request renders from
/// staged DEM or answers no coverage instead, which is what a wide tile did in
/// practice anyway: it never finished.
const MAX_SRTM_TILES_PER_REQUEST: usize = 16;

/// Nodata value the staged `.bin` and HGT tiles carry.
const DEM_NODATA: f32 = -9999.0;

/// Elevation at a single point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationPoint {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation_m: f64,
    /// Sample spacing of the DEM the value came from, in metres.
    pub resolution_m: f64,
    pub source: ElevationSource,
}

/// Elevation profile along a path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevationProfile {
    pub points: Vec<ElevationPoint>,
    pub total_distance_m: f64,
    pub elevation_gain_m: f64,
    pub elevation_loss_m: f64,
    pub min_elevation_m: f64,
    pub max_elevation_m: f64,
}

/// Which store an elevation came from.
///
/// Only what the server can actually serve: a grid or staged file an operator
/// put on disk, or a tile downloaded from the SRTM bucket.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ElevationSource {
    Srtm30m,
    LocalDem,
}

/// Why a DEM query has no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ElevationGap {
    /// No loaded grid, no staged tile and no cached tile covers the query.
    NoCoverage(String),
    /// The DEM this query needs could not be read or fetched.
    Unreadable(String),
}

impl ElevationGap {
    pub fn message(&self) -> &str {
        match self {
            ElevationGap::NoCoverage(message) | ElevationGap::Unreadable(message) => message,
        }
    }
}

impl IntoResponse for ElevationGap {
    fn into_response(self) -> Response {
        match self {
            ElevationGap::NoCoverage(message) => (StatusCode::NOT_FOUND, message).into_response(),
            ElevationGap::Unreadable(message) => {
                tracing::warn!("elevation refused: {message}");
                (StatusCode::SERVICE_UNAVAILABLE, message).into_response()
            }
        }
    }
}

/// The one message for a query no DEM covers.
fn no_coverage(latitude: f64, longitude: f64) -> ElevationGap {
    ElevationGap::NoCoverage(format!(
        "no elevation data staged for this location ({latitude}, {longitude})"
    ))
}

/// A loaded DEM grid for elevation lookups.
pub struct DemGrid {
    pub bounds: [f64; 4], // [west, south, east, north]
    pub width: usize,
    pub height: usize,
    pub cell_size_x: f64,
    pub cell_size_y: f64,
    pub elevations: Vec<f64>, // row-major, [height][width]
    pub nodata: f64,
}

impl DemGrid {
    /// Bilinear interpolation at (lat, lon).
    pub fn sample(&self, lat: f64, lon: f64) -> Option<f64> {
        let col_f = (lon - self.bounds[0]) / self.cell_size_x;
        let row_f = (self.bounds[3] - lat) / self.cell_size_y; // north-down

        if col_f < 0.0 || row_f < 0.0 {
            return None;
        }

        let col0 = col_f.floor() as usize;
        let row0 = row_f.floor() as usize;
        if col0 + 1 >= self.width || row0 + 1 >= self.height {
            return None;
        }

        let fx = col_f - col0 as f64;
        let fy = row_f - row0 as f64;

        let v00 = self.elevations[row0 * self.width + col0];
        let v10 = self.elevations[row0 * self.width + col0 + 1];
        let v01 = self.elevations[(row0 + 1) * self.width + col0];
        let v11 = self.elevations[(row0 + 1) * self.width + col0 + 1];

        // Skip nodata cells
        for v in [v00, v10, v01, v11] {
            if (v - self.nodata).abs() < 1e-10 {
                return None;
            }
        }

        let val = v00 * (1.0 - fx) * (1.0 - fy)
            + v10 * fx * (1.0 - fy)
            + v01 * (1.0 - fx) * fy
            + v11 * fx * fy;
        Some(val)
    }

    /// Sample spacing in metres, the coarser of the two axes.
    fn resolution_m(&self) -> f64 {
        self.cell_size_x.max(self.cell_size_y) * METERS_PER_DEG_LAT
    }
}

/// Store of loaded DEM grids for elevation lookup.
pub struct DemStore {
    grids: Vec<DemGrid>,
}

impl Default for DemStore {
    fn default() -> Self {
        Self::new()
    }
}

impl DemStore {
    pub fn new() -> Self {
        Self { grids: Vec::new() }
    }

    pub fn add_grid(&mut self, grid: DemGrid) {
        self.grids.push(grid);
    }

    /// The best-resolution grid loaded, whatever it covers. Callers that need a
    /// grid to anchor a pixel ladder on use this; point lookups go through
    /// `find_grid`, which also checks coverage.
    pub fn finest_grid(&self) -> Option<&DemGrid> {
        self.grids.iter().min_by(|a, b| {
            let res_a = a.cell_size_x * a.cell_size_y;
            let res_b = b.cell_size_x * b.cell_size_y;
            res_a.partial_cmp(&res_b).unwrap()
        })
    }

    /// Find the grid containing this point, preferring the best (smallest cell) resolution.
    fn find_grid(&self, lat: f64, lon: f64) -> Option<&DemGrid> {
        self.grids
            .iter()
            .filter(|g| {
                lon >= g.bounds[0] && lon <= g.bounds[2] && lat >= g.bounds[1] && lat <= g.bounds[3]
            })
            .min_by(|a, b| {
                let res_a = a.cell_size_x * a.cell_size_y;
                let res_b = b.cell_size_x * b.cell_size_y;
                res_a.partial_cmp(&res_b).unwrap()
            })
    }
}

/// The DEM stores the server reads, in the order it reads them.
///
/// Cheap to clone, so an analysis engine can hold one for as long as it lives
/// rather than borrowing the application state.
#[derive(Clone)]
pub struct ElevationSources {
    grids: Arc<DemStore>,
    data_dir: PathBuf,
    /// Where the SRTM fallback downloads from. Empty turns the fallback off, so
    /// an air-gapped server answers no coverage instead of a fetch failure.
    srtm_base_url: String,
}

impl ElevationSources {
    pub fn new(grids: Arc<DemStore>, data_dir: PathBuf, srtm_base_url: String) -> Self {
        ElevationSources {
            grids,
            data_dir,
            srtm_base_url,
        }
    }

    /// The loaded grids, for callers that anchor a resolution ladder on one.
    pub fn grids(&self) -> &Arc<DemStore> {
        &self.grids
    }

    /// Elevation over `bounds`, resolved once so a whole raster can then be
    /// sampled from memory.
    pub async fn field(&self, bounds: [f64; 4]) -> Result<ElevationField, ElevationGap> {
        Ok(ElevationField {
            grids: Arc::clone(&self.grids),
            coverage: self.dem_tiles(bounds).await?,
        })
    }

    /// One-degree DEM tiles covering `bounds`: staged files first, then the
    /// SRTM cache. Empty when nothing is staged and the area is too wide to
    /// fetch, or when the SRTM fallback is off.
    ///
    /// A tile the fetch reaches for but cannot get is an error, not a gap:
    /// skadi covers the whole globe, ocean and poles included, so an
    /// unreachable tile means upstream trouble rather than no data there.
    /// Dropping it would render as sea level, which looks like terrain that is
    /// simply flat.
    pub async fn dem_tiles(&self, bounds: [f64; 4]) -> Result<DemCoverage, ElevationGap> {
        let bounds = whole_degrees(bounds);
        let staged = self.staged_tiles(bounds);
        if !staged.is_empty() {
            return Ok(DemCoverage {
                tiles: staged,
                source: ElevationSource::LocalDem,
            });
        }
        if self.srtm_base_url.is_empty() {
            return Ok(DemCoverage::none());
        }

        let cache = tiletopia_terrain::dem_cache::DemCache::new(
            self.data_dir.join("dem_cache"),
            self.srtm_base_url.clone(),
        );
        let mut tiles = Vec::new();
        for (lat, lon) in srtm_tiles_to_fetch(bounds) {
            let name = tiletopia_terrain::dem_cache::srtm_tile_name(lat, lon);
            let hgt_path = cache.get_srtm_tile(lat, lon).await.map_err(|e| {
                ElevationGap::Unreadable(format!("SRTM tile {name} could not be fetched: {e}"))
            })?;
            let tile = dem_tile_from_hgt(&hgt_path, lat, lon).map_err(|e| {
                ElevationGap::Unreadable(format!("SRTM tile {name} is unusable: {e}"))
            })?;
            tiles.push(tile);
        }
        Ok(DemCoverage {
            tiles,
            source: ElevationSource::Srtm30m,
        })
    }

    /// DEM tiles for these bounds that are already on disk under `dem/`.
    fn staged_tiles(&self, bounds: [f64; 4]) -> Vec<DemTile> {
        let mut tiles = Vec::new();
        for (lat, lon) in tiletopia_terrain::global_dem::required_dem_tiles(bounds) {
            let path = self.data_dir.join(format!("dem/{lat}_{lon}.bin"));
            if path.exists()
                && let Ok(tile) = staged_tile_from_file(&path, lat, lon)
            {
                tiles.push(tile);
            }
        }
        tiles
    }
}

/// DEM tiles covering an area, and which store they came from.
pub struct DemCoverage {
    pub tiles: Vec<DemTile>,
    source: ElevationSource,
}

impl DemCoverage {
    /// No tile covers the area. The source label is unread: every caller checks
    /// for a sample first.
    fn none() -> Self {
        DemCoverage {
            tiles: Vec::new(),
            source: ElevationSource::LocalDem,
        }
    }
}

/// Elevation over one area, resolved once and then sampled per point.
pub struct ElevationField {
    grids: Arc<DemStore>,
    coverage: DemCoverage,
}

impl ElevationField {
    /// Elevation at one point, `None` where no DEM covers it.
    pub fn sample(&self, latitude: f64, longitude: f64) -> Option<ElevationPoint> {
        if let Some(grid) = self.grids.find_grid(latitude, longitude)
            && let Some(elevation_m) = grid.sample(latitude, longitude)
        {
            return Some(ElevationPoint {
                latitude,
                longitude,
                elevation_m,
                resolution_m: grid.resolution_m(),
                source: ElevationSource::LocalDem,
            });
        }
        self.coverage.tiles.iter().find_map(|tile| {
            let elevation_m = tile.sample(latitude, longitude)? as f64;
            Some(ElevationPoint {
                latitude,
                longitude,
                elevation_m,
                resolution_m: tile.resolution_deg() * METERS_PER_DEG_LAT,
                source: self.coverage.source,
            })
        })
    }

    /// Elevation in metres at one point, `None` where no DEM covers it.
    pub fn elevation_at(&self, latitude: f64, longitude: f64) -> Option<f64> {
        self.sample(latitude, longitude).map(|p| p.elevation_m)
    }

    /// Elevation at one point, refusing the query when no DEM covers it.
    pub fn point(&self, latitude: f64, longitude: f64) -> Result<ElevationPoint, ElevationGap> {
        self.sample(latitude, longitude)
            .ok_or_else(|| no_coverage(latitude, longitude))
    }

    /// Elevation along a path of `[longitude, latitude]` points, refusing the
    /// query at the first point no DEM covers.
    pub fn profile(&self, path: &[[f64; 2]]) -> Result<ElevationProfile, ElevationGap> {
        let mut points: Vec<ElevationPoint> = Vec::with_capacity(path.len());
        let mut total_distance_m = 0.0;
        let mut elevation_gain_m = 0.0;
        let mut elevation_loss_m = 0.0;

        for &[longitude, latitude] in path {
            let point = self.point(latitude, longitude)?;
            if let Some(previous) = points.last() {
                total_distance_m += haversine_distance(previous, &point);
                let climb = point.elevation_m - previous.elevation_m;
                if climb > 0.0 {
                    elevation_gain_m += climb;
                } else {
                    elevation_loss_m += climb.abs();
                }
            }
            points.push(point);
        }

        Ok(ElevationProfile {
            min_elevation_m: points
                .iter()
                .map(|p| p.elevation_m)
                .fold(f64::INFINITY, f64::min),
            max_elevation_m: points
                .iter()
                .map(|p| p.elevation_m)
                .fold(f64::NEG_INFINITY, f64::max),
            points,
            total_distance_m,
            elevation_gain_m,
            elevation_loss_m,
        })
    }
}

/// Whether a coordinate names a point on the globe.
pub fn on_the_globe(longitude: f64, latitude: f64) -> bool {
    (-180.0..=180.0).contains(&longitude) && (-90.0..=90.0).contains(&latitude)
}

/// The box holding these `[longitude, latitude]` points, `None` for no points.
pub fn bounds_of(points: &[[f64; 2]]) -> Option<[f64; 4]> {
    let [first_lon, first_lat] = *points.first()?;
    let mut bounds = [first_lon, first_lat, first_lon, first_lat];
    for &[lon, lat] in points {
        bounds[0] = bounds[0].min(lon);
        bounds[1] = bounds[1].min(lat);
        bounds[2] = bounds[2].max(lon);
        bounds[3] = bounds[3].max(lat);
    }
    Some(bounds)
}

/// Snap a box out to whole degrees, at least one degree per axis: the DEM
/// stores hold one-degree tiles, and a box of no width names none of them.
fn whole_degrees(bounds: [f64; 4]) -> [f64; 4] {
    let [west, south, east, north] = bounds;
    let (west, south) = (west.floor(), south.floor());
    [
        west,
        south,
        east.ceil().max(west + 1.0),
        north.ceil().max(south + 1.0),
    ]
}

/// SRTM tiles to fetch for these bounds, empty when the area is too wide to
/// serve from upstream downloads.
pub(crate) fn srtm_tiles_to_fetch(bounds: [f64; 4]) -> Vec<(i32, i32)> {
    let required = tiletopia_terrain::dem_cache::required_srtm_tiles(
        bounds[0], bounds[1], bounds[2], bounds[3],
    );
    if required.len() > MAX_SRTM_TILES_PER_REQUEST {
        tracing::debug!(
            "area spans {} SRTM tiles, over the {MAX_SRTM_TILES_PER_REQUEST} fetch bound",
            required.len()
        );
        return Vec::new();
    }
    required
}

/// Read a cached HGT file into a DEM tile.
///
/// HGT rows run north-to-south, the opposite of the order [`DemTile`] samples,
/// so this must go through [`DemTile::from_north_up`]. Getting it wrong mirrors
/// every elevation about the tile's mid-latitude, which reads as plausible
/// terrain in the wrong place rather than as an error.
fn dem_tile_from_hgt(path: &Path, lat: i32, lon: i32) -> Result<DemTile, std::io::Error> {
    let hm = tiletopia_ingest::hgt_reader::read(path)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let elevations = hm.elevations.iter().map(|&e| e as f32).collect();
    DemTile::from_north_up(lat, lon, elevations, hm.width as u32, DEM_NODATA).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}×{} is not a usable DEM grid", hm.width, hm.height),
        )
    })
}

/// Read a staged DEM tile from a binary file (simple format: f32 elevation array).
fn staged_tile_from_file(path: &Path, lat: i32, lon: i32) -> std::io::Result<DemTile> {
    let data = std::fs::read(path)?;
    let samples = ((data.len() / 4) as f64).sqrt() as u32;

    let elevations: Vec<f32> = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();

    DemTile::from_south_up(lat, lon, elevations, samples, DEM_NODATA).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: not a square f32 DEM grid", path.display()),
        )
    })
}

/// Haversine distance between two elevation points (in meters).
fn haversine_distance(a: &ElevationPoint, b: &ElevationPoint) -> f64 {
    let r = 6_371_000.0; // Earth radius in meters
    let dlat = (b.latitude - a.latitude).to_radians();
    let dlon = (b.longitude - a.longitude).to_radians();
    let lat1 = a.latitude.to_radians();
    let lat2 = b.latitude.to_radians();
    let a_val = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a_val.sqrt().asin();
    r * c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DEM tile store rooted in a temp directory, cleaned up on drop.
    struct StagedDir {
        path: PathBuf,
    }

    impl StagedDir {
        fn new(name: &str) -> StagedDir {
            let path = std::env::temp_dir().join(format!("tiletopia_elevation_{name}"));
            std::fs::create_dir_all(path.join("dem")).unwrap();
            StagedDir { path }
        }

        /// Stage a one-degree tile whose elevation is `100 * (row + col)`, so a
        /// sample says which corner of the tile it came from.
        fn stage(&self, lat: i32, lon: i32, samples: usize) {
            let mut bytes = Vec::with_capacity(samples * samples * 4);
            for row in 0..samples {
                for col in 0..samples {
                    let elevation = 100.0 * (row + col) as f32;
                    bytes.extend_from_slice(&elevation.to_le_bytes());
                }
            }
            let path = self.path.join(format!("dem/{lat}_{lon}.bin"));
            std::fs::write(path, bytes).unwrap();
        }

        /// Sources over this directory with the SRTM fallback off, so a test
        /// never reaches the network.
        fn sources(&self) -> ElevationSources {
            ElevationSources::new(Arc::new(DemStore::new()), self.path.clone(), String::new())
        }
    }

    impl Drop for StagedDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    #[tokio::test]
    async fn a_staged_tile_answers_with_its_own_source_and_resolution() {
        let dir = StagedDir::new("staged_point");
        dir.stage(43, 7, 3);
        let field = dir.sources().field([7.4, 43.7, 7.4, 43.7]).await.unwrap();

        // south-up rows: the tile's south-west corner is row 0, col 0
        let south_west = field.point(43.0, 7.0).unwrap();
        assert_eq!(south_west.elevation_m, 0.0);
        assert_eq!(field.point(44.0, 8.0).unwrap().elevation_m, 400.0);
        assert_eq!(south_west.source, ElevationSource::LocalDem);
        // 3 samples across a degree is a half-degree step
        assert!((south_west.resolution_m - METERS_PER_DEG_LAT / 2.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn nothing_staged_is_an_explicit_gap_not_a_guess() {
        let dir = StagedDir::new("no_coverage");
        let field = dir.sources().field([7.4, 43.7, 7.4, 43.7]).await.unwrap();
        let Err(gap) = field.point(43.7, 7.4) else {
            panic!("an unstaged location has no elevation");
        };
        assert!(
            gap.message().contains("no elevation data staged"),
            "{gap:?}"
        );
        assert!(gap.message().contains("43.7"), "{gap:?}");
        assert!(field.sample(43.7, 7.4).is_none());
    }

    /// A point outside every staged tile is a gap even when the field holds
    /// tiles for its neighbours.
    #[tokio::test]
    async fn a_point_off_the_staged_tile_is_a_gap() {
        let dir = StagedDir::new("off_tile");
        dir.stage(43, 7, 3);
        let field = dir.sources().field([7.0, 43.0, 9.0, 44.0]).await.unwrap();
        assert!(field.sample(43.5, 7.5).is_some());
        assert!(field.sample(43.5, 8.5).is_none());
    }

    #[tokio::test]
    async fn a_profile_walks_the_path_and_measures_the_climb() {
        let dir = StagedDir::new("profile");
        dir.stage(43, 7, 3);
        let field = dir.sources().field([7.0, 43.0, 8.0, 44.0]).await.unwrap();
        // north-east along the tile, so every step climbs
        let profile = field
            .profile(&[[7.0, 43.0], [7.5, 43.5], [8.0, 44.0]])
            .unwrap();
        assert_eq!(profile.points.len(), 3);
        assert_eq!(profile.min_elevation_m, 0.0);
        assert_eq!(profile.max_elevation_m, 400.0);
        assert_eq!(profile.elevation_gain_m, 400.0);
        assert_eq!(profile.elevation_loss_m, 0.0);
        assert!(profile.total_distance_m > 100_000.0);

        // one point off the tile refuses the whole profile
        let gap = field.profile(&[[7.5, 43.5], [8.5, 43.5]]).unwrap_err();
        assert!(
            gap.message().contains("no elevation data staged"),
            "{gap:?}"
        );
    }

    #[tokio::test]
    async fn a_loaded_grid_wins_over_the_staged_tile() {
        let dir = StagedDir::new("grid_first");
        dir.stage(43, 7, 3);
        let mut store = DemStore::new();
        store.add_grid(DemGrid {
            bounds: [7.0, 43.0, 8.0, 44.0],
            width: 2,
            height: 2,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            elevations: vec![777.0; 4],
            nodata: -9999.0,
        });
        let sources = ElevationSources::new(Arc::new(store), dir.path.clone(), String::new());
        let field = sources.field([7.0, 43.0, 8.0, 44.0]).await.unwrap();
        assert_eq!(field.point(43.5, 7.5).unwrap().elevation_m, 777.0);
    }

    #[test]
    fn a_zero_width_box_still_names_its_degree_cell() {
        assert_eq!(
            whole_degrees([7.4, 43.7, 7.4, 43.7]),
            [7.0, 43.0, 8.0, 44.0]
        );
        assert_eq!(
            whole_degrees([7.0, 43.0, 7.0, 43.0]),
            [7.0, 43.0, 8.0, 44.0]
        );
        // a real span is only rounded outwards
        assert_eq!(
            whole_degrees([7.4, 43.7, 9.2, 45.1]),
            [7.0, 43.0, 10.0, 46.0]
        );
        assert_eq!(
            whole_degrees([-7.4, -43.7, -7.1, -43.2]),
            [-8.0, -44.0, -7.0, -43.0]
        );
    }

    #[test]
    fn wide_areas_do_not_fetch_srtm() {
        // a degree cell is one tile, a continent is thousands
        assert_eq!(srtm_tiles_to_fetch([7.0, 43.0, 8.0, 44.0]), vec![(43, 7)]);
        assert!(srtm_tiles_to_fetch([-180.0, -90.0, 180.0, 90.0]).is_empty());
        assert!(srtm_tiles_to_fetch([0.0, 0.0, 10.0, 10.0]).is_empty());
    }

    #[test]
    fn truncated_dem_files_are_skipped_not_sampled() {
        let dir = StagedDir::new("truncated");

        // empty file: the shape that crashed the sampler
        let empty = dir.path.join("dem/43_7.bin");
        std::fs::write(&empty, []).unwrap();
        assert!(staged_tile_from_file(&empty, 43, 7).is_err());

        // half a grid written so far
        let partial = dir.path.join("dem/43_8.bin");
        std::fs::write(&partial, vec![0u8; 4 * 10]).unwrap();
        assert!(staged_tile_from_file(&partial, 43, 8).is_err());

        // the bounds scan drops both rather than propagating a failure
        assert!(
            dir.sources()
                .staged_tiles([7.0, 43.0, 9.0, 44.0])
                .is_empty()
        );
    }

    #[test]
    fn hgt_rows_keep_their_latitude_through_the_reader() {
        // guards the seam the flip lived in: hgt_reader hands back north-up
        // rows, DemTile samples south-up
        let dir = StagedDir::new("hgt_orientation");
        let path = dir.path.join("N43E007.hgt");

        // 61×61, alpine along the north edge and sea level along the south
        let side = 61usize;
        let mut raw = Vec::with_capacity(side * side * 2);
        for row in 0..side {
            let height = (1658.0 * (1.0 - row as f64 / (side - 1) as f64)) as i16;
            for _ in 0..side {
                raw.extend_from_slice(&height.to_be_bytes());
            }
        }
        std::fs::write(&path, &raw).unwrap();

        let tile = dem_tile_from_hgt(&path, 43, 7).unwrap();
        assert!(
            tile.sample(43.97, 7.4).unwrap() > 1500.0,
            "north edge must stay alpine"
        );
        assert!(
            tile.sample(43.03, 7.4).unwrap() < 100.0,
            "south edge must stay coast"
        );
    }

    /// Build a 3x3 DEM grid and verify bilinear interpolation.
    #[test]
    fn dem_grid_interpolates_bilinearly() {
        // 3x3 grid covering [0,0] to [2,2] in lon/lat
        // Elevations:
        //   row0 (north=2): 100  200  300
        //   row1 (lat=1):   400  500  600
        //   row2 (south=0): 700  800  900
        let grid = DemGrid {
            bounds: [0.0, 0.0, 2.0, 2.0], // [west, south, east, north]
            width: 3,
            height: 3,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            elevations: vec![
                100.0, 200.0, 300.0, // row 0 (north)
                400.0, 500.0, 600.0, // row 1
                700.0, 800.0, 900.0, // row 2 (south)
            ],
            nodata: -9999.0,
        };

        // Centre of grid at lat=1.0, lon=1.0 → row_f=1.0, col_f=1.0 → exactly cell [1][1]=500
        let v = grid.sample(1.0, 1.0).unwrap();
        assert!((v - 500.0).abs() < 1e-9);

        // Midpoint between cells (0,0)=100 and (0,1)=200 at lat=2.0 (top row), lon=0.5
        // row_f = (2-2)/1 = 0.0, col_f = 0.5, fx=0.5, fy=0.0
        // = 100*0.5*1 + 200*0.5*1 + 400*0.5*0 + 500*0.5*0 = 150
        let v = grid.sample(2.0, 0.5).unwrap();
        assert!((v - 150.0).abs() < 1e-9);

        // Out-of-bounds should return None
        assert!(grid.sample(3.0, 1.0).is_none());
    }

    #[test]
    fn dem_grid_nodata_returns_none() {
        let grid = DemGrid {
            bounds: [0.0, 0.0, 2.0, 2.0],
            width: 3,
            height: 3,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            elevations: vec![
                100.0, -9999.0, 300.0, 400.0, 500.0, 600.0, 700.0, 800.0, 900.0,
            ],
            nodata: -9999.0,
        };
        // Interpolation touching the nodata cell should return None
        assert!(grid.sample(2.0, 0.5).is_none());
    }

    #[test]
    fn a_box_holds_every_point_of_a_path() {
        assert_eq!(bounds_of(&[]), None);
        assert_eq!(bounds_of(&[[7.4, 43.7]]), Some([7.4, 43.7, 7.4, 43.7]));
        assert_eq!(
            bounds_of(&[[7.4, 43.7], [7.1, 44.2], [8.0, 43.1]]),
            Some([7.1, 43.1, 8.0, 44.2])
        );
    }
}
