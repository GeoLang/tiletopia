use crate::crs_detect::{WGS84_GEODETIC_EPSG, detect_crs_from_geotiff};
use crate::tiff_reader::{self, GridLayout};
use crate::{Heightmap, IngestError, dted_reader, read_heightmap};
use std::path::Path;
use tiletopia_core::crs::{Coord3D, CrsDef, ReprojError, Transformer};

fn lowercase_extension(path: &Path) -> String {
    path.extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_geotiff(path: &Path) -> bool {
    matches!(lowercase_extension(path).as_str(), "tif" | "tiff")
}

fn sample_count(path: &Path) -> Result<u64, IngestError> {
    if is_geotiff(path) {
        let layout = tiff_reader::grid_layout(path)?;
        return Ok(layout.width as u64 * layout.height as u64);
    }
    match lowercase_extension(path).as_str() {
        "hgt" => Ok(std::fs::metadata(path)?.len() / 2),
        "dt0" | "dt1" | "dt2" => {
            let (width, height) = dted_reader::dimensions(path)?;
            Ok(width as u64 * height as u64)
        }
        other => Err(IngestError::UnsupportedFormat(other.to_string())),
    }
}

pub fn read_dem_wgs84(
    path: &Path,
    fallback_source_epsg: Option<u32>,
    max_samples: u64,
) -> Result<Heightmap, IngestError> {
    let samples = sample_count(path)?;
    if samples > max_samples {
        return Err(IngestError::DemRefused(format!(
            "dem_too_large: {} has {samples} samples and the limit is {max_samples}",
            file_name(path)
        )));
    }

    let geotiff = if is_geotiff(path) {
        Some(checked_geotiff(path, fallback_source_epsg)?)
    } else {
        None
    };

    let mut dem = read_heightmap(path)?;
    if dem.width < 2 || dem.height < 2 {
        return Err(IngestError::DemRefused(format!(
            "dem_too_small: {} is {} x {} samples, and a grid needs at least 2 x 2",
            file_name(path),
            dem.width,
            dem.height
        )));
    }
    mark_voids(&mut dem);
    if dem.elevations.iter().all(|elevation| elevation.is_nan()) {
        return Err(IngestError::DemRefused(format!(
            "dem_no_elevations: every sample in {} is a void",
            file_name(path)
        )));
    }
    let filled_void_count = fill_voids_from_nearest_samples(&mut dem);
    if filled_void_count > 0 {
        tracing::warn!(
            "{}: filled {filled_void_count} void samples from their nearest valid samples",
            file_name(path)
        );
    }

    let source_epsg = match geotiff {
        Some((layout, source_epsg)) => {
            dem.bounds = outer_sample_bounds(dem.bounds, &layout);
            source_epsg
        }
        // HGT and DTED are geographic by format, with their outer samples on the bounds
        None => WGS84_GEODETIC_EPSG,
    };

    let [west, south, east, north] = dem.bounds;
    if !(west < east && south < north) {
        return Err(bad_bounds(path, dem.bounds));
    }
    if source_epsg != WGS84_GEODETIC_EPSG {
        dem = reproject(dem, source_epsg)?;
    }

    let [west, south, east, north] = dem.bounds;
    if !(-180.0 <= west && west < east && east <= 180.0)
        || !(-90.0 <= south && south < north && north <= 90.0)
    {
        return Err(bad_bounds(path, dem.bounds));
    }
    Ok(dem)
}

fn bad_bounds(path: &Path, bounds: [f64; 4]) -> IngestError {
    IngestError::DemRefused(format!(
        "dem_bounds_invalid: {} resolves to bounds {bounds:?}, which are not west, south, east, north in degrees",
        file_name(path)
    ))
}

fn checked_geotiff(
    path: &Path,
    fallback_source_epsg: Option<u32>,
) -> Result<(GridLayout, u32), IngestError> {
    let layout = tiff_reader::grid_layout(path)?;
    if !layout.georeferenced {
        return Err(IngestError::DemRefused(format!(
            "dem_not_georeferenced: {} has no ModelTransformation or ModelTiepoint tag, so its location is unknown",
            file_name(path)
        )));
    }
    if layout.rotated {
        return Err(IngestError::DemRefused(format!(
            "dem_rotated: {} has a rotated or sheared ModelTransformation, and only north-up grids are read",
            file_name(path)
        )));
    }
    let source_epsg = detect_crs_from_geotiff(path)
        .to_epsg()
        .or(fallback_source_epsg)
        .ok_or_else(|| {
            IngestError::DemRefused(format!(
                "dem_no_crs: {} names no coordinate reference system: upload it with crs, such as EPSG:32632",
                file_name(path)
            ))
        })?;
    Ok((layout, source_epsg))
}

fn mark_voids(dem: &mut Heightmap) {
    let Some(nodata) = dem.nodata else {
        return;
    };
    // float32 samples only match their text nodata tag at float32 precision
    let nodata = nodata as f32;
    for elevation in &mut dem.elevations {
        if *elevation as f32 == nodata {
            *elevation = f64::NAN;
        }
    }
}

// gdal_fillnodata searches 100 pixels out by default
const MOST_VOID_FILL_RINGS: usize = 100;

fn fill_voids_from_nearest_samples(dem: &mut Heightmap) -> usize {
    let (width, height) = (dem.width, dem.height);
    let neighbours = |index: usize| {
        [
            (index >= width).then(|| index - width),
            (index + width < width * height).then(|| index + width),
            (!index.is_multiple_of(width)).then(|| index - 1),
            (index % width + 1 < width).then(|| index + 1),
        ]
        .into_iter()
        .flatten()
    };

    let mut ring: Vec<usize> = (0..dem.elevations.len())
        .filter(|&index| {
            dem.elevations[index].is_nan()
                && neighbours(index).any(|neighbour| !dem.elevations[neighbour].is_nan())
        })
        .collect();
    let mut filled_count = 0;
    for _ in 0..MOST_VOID_FILL_RINGS {
        let means: Vec<f64> = ring
            .iter()
            .map(|&index| {
                let values: Vec<f64> = neighbours(index)
                    .map(|neighbour| dem.elevations[neighbour])
                    .filter(|value| !value.is_nan())
                    .collect();
                values.iter().sum::<f64>() / values.len() as f64
            })
            .collect();
        for (&index, mean) in ring.iter().zip(means) {
            dem.elevations[index] = mean;
        }
        filled_count += ring.len();

        let mut next_ring: Vec<usize> = ring
            .iter()
            .flat_map(|&index| neighbours(index))
            .filter(|&neighbour| dem.elevations[neighbour].is_nan())
            .collect();
        next_ring.sort_unstable();
        next_ring.dedup();
        ring = next_ring;
    }
    filled_count
}

fn outer_sample_bounds(bounds: [f64; 4], layout: &GridLayout) -> [f64; 4] {
    let [west, south, east, north] = bounds;
    let x_step = (east - west) / layout.width as f64;
    let y_step = (north - south) / layout.height as f64;
    if layout.pixel_is_point {
        [west, south + y_step, east - x_step, north]
    } else {
        [
            west + x_step / 2.0,
            south + y_step / 2.0,
            east - x_step / 2.0,
            north - y_step / 2.0,
        ]
    }
}

fn axis_positions(start: f64, end: f64, count: usize) -> impl Iterator<Item = f64> {
    let step = (end - start) / (count - 1) as f64;
    (0..count).map(move |index| start + index as f64 * step)
}

fn reproject(source: Heightmap, source_epsg: u32) -> Result<Heightmap, IngestError> {
    let refuse = |error: ReprojError| {
        IngestError::DemRefused(format!(
            "dem_crs_unsupported: EPSG:{source_epsg} cannot be reprojected to EPSG:{WGS84_GEODETIC_EPSG}: {error}"
        ))
    };
    let source_crs = CrsDef::Epsg(source_epsg);
    let wgs84 = CrsDef::Epsg(WGS84_GEODETIC_EPSG);
    let [west, south, east, north] = source.bounds;

    let edge = |x: f64, y: f64| Coord3D { x, y, z: 0.0 };
    let mut perimeter: Vec<Coord3D> = Vec::with_capacity(2 * (source.width + source.height));
    perimeter.extend(axis_positions(west, east, source.width).map(|x| edge(x, south)));
    perimeter.extend(axis_positions(south, north, source.height).map(|y| edge(east, y)));
    perimeter.extend(axis_positions(east, west, source.width).map(|x| edge(x, north)));
    perimeter.extend(axis_positions(north, south, source.height).map(|y| edge(west, y)));
    let outline = Transformer::new(source_crs.clone(), wgs84.clone())
        .transform_batch(&perimeter)
        .map_err(refuse)?;
    // a grid over the antimeridian or a pole wraps from +180 to -180 between two neighbouring points
    let longitude_wraps = outline
        .windows(2)
        .any(|pair| (pair[1].x - pair[0].x).abs() > 180.0);
    // projicio answers -180.07 for a point just past the antimeridian in UTM zone 1, with no wrap
    let longitude_runs_past_180 = outline.iter().any(|point| point.x.abs() > 180.0);
    if longitude_wraps || longitude_runs_past_180 {
        return Err(IngestError::DemRefused(format!(
            "dem_crosses_antimeridian: the EPSG:{source_epsg} grid crosses longitude 180 or contains a pole, and bounds run west to east within -180 to 180"
        )));
    }
    let bounds = outline.iter().fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |[min_x, min_y, max_x, max_y], point| {
            [
                min_x.min(point.x),
                min_y.min(point.y),
                max_x.max(point.x),
                max_y.max(point.y),
            ]
        },
    );

    // a whole grid of Coord3D is three times the size of the elevations
    let inverse = Transformer::new(wgs84, source_crs);
    let mut elevations = Vec::with_capacity(source.elevations.len());
    for latitude in axis_positions(bounds[3], bounds[1], source.height) {
        let row: Vec<Coord3D> = axis_positions(bounds[0], bounds[2], source.width)
            .map(|longitude| edge(longitude, latitude))
            .collect();
        for projected in inverse.transform_batch(&row).map_err(refuse)? {
            elevations.push(
                source
                    .elevation_at(projected.x, projected.y)
                    .unwrap_or(f64::NAN),
            );
        }
    }

    Ok(Heightmap {
        width: source.width,
        height: source.height,
        elevations,
        bounds,
        nodata: source.nodata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff::encoder::{TiffEncoder, colortype};
    use tiff::tags::Tag;

    const GEOGRAPHIC_TYPE_GEO_KEY: u16 = 2048;
    const RASTER_TYPE_GEO_KEY: u16 = 1025;
    const PIXEL_IS_AREA: u16 = 1;
    const PIXEL_IS_POINT: u16 = 2;

    const NO_SAMPLE_LIMIT: u64 = u64::MAX;

    fn write_wgs84_geotiff(
        path: &Path,
        elevations: &[f32],
        side: u32,
        raster_type: u16,
        west: f64,
    ) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = TiffEncoder::new(file).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray32Float>(side, side)
            .unwrap();
        let tags = image.encoder();
        tags.write_tag(Tag::ModelPixelScaleTag, &[0.5f64, 0.5, 0.0][..])
            .unwrap();
        tags.write_tag(
            Tag::ModelTiepointTag,
            &[0.0f64, 0.0, 0.0, west, 50.0, 0.0][..],
        )
        .unwrap();
        tags.write_tag(
            Tag::GeoKeyDirectoryTag,
            &[
                1u16,
                1,
                0,
                2,
                RASTER_TYPE_GEO_KEY,
                0,
                1,
                raster_type,
                GEOGRAPHIC_TYPE_GEO_KEY,
                0,
                1,
                4326,
            ][..],
        )
        .unwrap();
        tags.write_tag(Tag::GdalNodata, "-9999").unwrap();
        image.write_data(elevations).unwrap();
    }

    #[test]
    fn pixel_is_area_bounds_move_half_a_pixel_onto_the_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[1.0, 2.0, 3.0, 4.0], 2, PIXEL_IS_AREA, 10.0);

        let dem = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT).unwrap();
        assert_eq!(dem.bounds, [10.25, 49.25, 10.75, 49.75]);
        assert_eq!(dem.elevation_at(10.25, 49.75), Some(1.0));
        assert_eq!(dem.elevation_at(10.75, 49.25), Some(4.0));
    }

    #[test]
    fn a_sample_beside_a_void_keeps_its_value_and_the_void_takes_the_mean_of_its_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[1.0, -9999.0, 3.0, 4.0], 2, PIXEL_IS_AREA, 10.0);

        let dem = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT).unwrap();
        assert_eq!(dem.elevation_at(10.25, 49.75), Some(1.0));
        assert_eq!(dem.elevation_at(10.75, 49.75), Some(2.5));
        assert_eq!(dem.elevation_at(10.5, 49.75), Some(1.75));
    }

    #[test]
    fn ground_outside_the_dem_has_no_elevation_after_a_void_is_filled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[1.0, -9999.0, 3.0, 4.0], 2, PIXEL_IS_AREA, 10.0);

        let dem = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT).unwrap();
        assert_eq!(dem.elevation_at(11.0, 49.75), None);
    }

    fn void_grid(width: usize, height: usize, elevations: Vec<f64>) -> Heightmap {
        Heightmap {
            width,
            height,
            elevations,
            bounds: [0.0, 0.0, 1.0, 1.0],
            nodata: None,
        }
    }

    #[test]
    fn voids_fill_one_ring_at_a_time_from_the_nearest_samples() {
        let void = f64::NAN;
        let mut dem = void_grid(
            3,
            3,
            vec![1.0, void, void, void, void, void, void, void, 9.0],
        );

        assert_eq!(fill_voids_from_nearest_samples(&mut dem), 7);

        assert_eq!(
            dem.elevations,
            [1.0, 1.0, 5.0, 1.0, 5.0, 9.0, 5.0, 9.0, 9.0]
        );
    }

    #[test]
    fn voids_past_the_ring_limit_stay_voids() {
        let width = MOST_VOID_FILL_RINGS + 3;
        let mut elevations = vec![f64::NAN; width];
        elevations[0] = 7.0;
        let mut dem = void_grid(width, 1, elevations);

        assert_eq!(
            fill_voids_from_nearest_samples(&mut dem),
            MOST_VOID_FILL_RINGS
        );

        assert_eq!(dem.elevations[MOST_VOID_FILL_RINGS], 7.0);
        assert!(dem.elevations[MOST_VOID_FILL_RINGS + 1].is_nan());
    }

    #[test]
    fn pixel_is_point_bounds_start_on_the_tiepoint_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[1.0, 2.0, 3.0, 4.0], 2, PIXEL_IS_POINT, 10.0);

        let dem = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT).unwrap();
        assert_eq!(dem.bounds, [10.0, 49.5, 10.5, 50.0]);
        assert_eq!(dem.elevation_at(10.0, 50.0), Some(1.0));
    }

    #[test]
    fn nodata_samples_are_filled_from_their_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[-9999.0, 2.0, 3.0, 4.0], 2, PIXEL_IS_AREA, 10.0);

        let dem = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT).unwrap();
        assert_eq!(dem.elevations[0], 2.5);
        assert_eq!(dem.elevation_at(10.25, 49.75), Some(2.5));
        assert_eq!(dem.elevation_at(10.75, 49.25), Some(4.0));
    }

    #[test]
    fn a_dem_of_only_voids_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[-9999.0; 4], 2, PIXEL_IS_AREA, 10.0);

        let error = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("dem_no_elevations"), "{error}");
    }

    #[test]
    fn a_tiff_with_no_georeferencing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.tif");
        TiffEncoder::new(std::fs::File::create(&path).unwrap())
            .unwrap()
            .write_image::<colortype::Gray8>(2, 2, &[1, 2, 3, 4])
            .unwrap();

        let error = read_dem_wgs84(&path, Some(4326), NO_SAMPLE_LIMIT)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("dem_not_georeferenced"), "{error}");
    }

    #[test]
    fn longitudes_past_180_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dem.tif");
        write_wgs84_geotiff(&path, &[1.0, 2.0, 3.0, 4.0], 2, PIXEL_IS_AREA, 190.0);

        let error = read_dem_wgs84(&path, None, NO_SAMPLE_LIMIT)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("dem_bounds_invalid"), "{error}");
    }

    fn utm_grid(west_easting: f64, utm_epsg: u32) -> Result<Heightmap, IngestError> {
        const SOUTH_NORTHING: f64 = 5_540_000.0;
        const SPAN_METRES: f64 = 10_000.0;
        let grid = Heightmap {
            width: 3,
            height: 3,
            elevations: vec![100.0; 9],
            bounds: [
                west_easting,
                SOUTH_NORTHING,
                west_easting + SPAN_METRES,
                SOUTH_NORTHING + SPAN_METRES,
            ],
            nodata: None,
        };
        reproject(grid, utm_epsg)
    }

    #[test]
    fn a_projected_grid_over_the_antimeridian_is_refused() {
        // zone 1 is centred on 177 west, so longitude 180 falls near easting 285000 at 50 north
        const UTM_1_NORTH: u32 = 32601;
        let error = utm_grid(280_000.0, UTM_1_NORTH).unwrap_err().to_string();
        assert!(error.starts_with("dem_crosses_antimeridian"), "{error}");

        let beside_it = utm_grid(500_000.0, UTM_1_NORTH).unwrap();
        assert!(beside_it.bounds[2] - beside_it.bounds[0] < 1.0);
    }

    #[test]
    fn a_dem_over_the_sample_limit_is_refused_before_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("N47E011.hgt");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(2 * 5000 * 5000)
            .unwrap();

        let error = read_dem_wgs84(&path, None, 24_999_999)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("dem_too_large"), "{error}");
        assert!(error.contains("25000000"), "{error}");
    }

    #[test]
    fn sample_count_reads_headers_without_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let hgt = dir.path().join("N47E011.hgt");
        std::fs::File::create(&hgt)
            .unwrap()
            .set_len(2 * 5000 * 5000)
            .unwrap();
        assert_eq!(sample_count(&hgt).unwrap(), 25_000_000);

        let tif = dir.path().join("dem.TIF");
        write_wgs84_geotiff(&tif, &[1.0; 9], 3, PIXEL_IS_AREA, 10.0);
        assert_eq!(sample_count(&tif).unwrap(), 9);

        let error = sample_count(Path::new("cloud.las")).unwrap_err();
        assert!(matches!(error, IngestError::UnsupportedFormat(_)));
    }
}
