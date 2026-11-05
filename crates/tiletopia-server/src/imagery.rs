use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path as FilePath, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
    routing::get,
};
use futures::future::BoxFuture;
use geoplumb::caps::{
    Caps, CapsPattern, CapsSet, Constraint, Crs, Dtype, RasterPattern, ResRange, SetField,
};
use geoplumb::chunk::{Chunk, RasterChunk};
use geoplumb::element::{Source, Transform};
use geoplumb::elements::Reproject;
use geoplumb::resample::resample_to_grid;
use geoplumb::tile::{TILE_PX, XyzTile};
use geoplumb::window::{Bbox, GridSpec, WindowReq};
use geoplumb::{Engine, Graph, NodeId};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use terrano_core::{BandedRaster, CogParams, CogReader, Raster, SampleFormat, write_cog_tiles};
use tiff::ColorType;
use tiff::decoder::{ChunkType, Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use tiletopia_core::crs::{Coord3D, CrsDef, Transformer};
use tiletopia_ingest::crs_detect::parse_epsg_code;
use uuid::Uuid;

use crate::analysis_tiles::{
    CHUNK_BUDGET_BYTES, MAX_ZOOM, MERCATOR_MAX_LAT, WEB_MERCATOR_EXTENT, mercator_x, mercator_y,
};
use crate::elevation::BUILD_BUDGET_BYTES;
use crate::http_cache::CachePolicy;
#[cfg(feature = "jp2")]
use crate::jp2;
use crate::terrain_api::Refusal;
use crate::users::to_hex;
use crate::{AppState, AssetType};

const MANIFEST_FILE: &str = "imagery/manifest.json";
const SOURCE_COG_FILE: &str = "imagery/source.tif";

const DECODED_SCRATCH_FILE: &str = "decoded.raw";
const COG_TILE_SIDE: usize = 256;

// 4096 squared f64 samples are 128 MiB per band, the read behind one far zoomed out tile
const MAX_DIRECT_READ_SIDE: usize = 4096;

// each open layer has a CHUNK_BUDGET_BYTES chunk cache and one open file
const MAX_OPEN_LAYERS: usize = 8;

// TIFF ExtraSamples values 1 and 2 are associated and unassociated alpha
const ALPHA_EXTRA_SAMPLES: [u16; 2] = [1, 2];

// GeoTIFF key 32767 means the CRS is defined in the file and has no EPSG code
const USER_DEFINED_GEO_KEY: u16 = 32767;

const TIFF_JPEG_COMPRESSION: u16 = 7;

// TIFF Photometric values terrano's samples mean as they stand: min-is-black and rgb
const DIRECT_PHOTOMETRICS: [u16; 2] = [1, 2];

const MODEL_TRANSFORMATION_TAG: u16 = 34264;

// the web mercator square's edge projects to 180.00000000000003 degrees
const GLOBE_EDGE_TOLERANCE_DEGREES: f64 = 1e-9;

pub fn imagery_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/assets/{id}/imagery/tilejson.json",
            get(imagery_tilejson),
        )
        .route("/api/v1/assets/{id}/imagery/{z}/{x}/{y}", get(imagery_tile))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PixelGrid {
    pub(crate) origin_x: f64,
    pub(crate) origin_y: f64,
    pub(crate) pixel_width: f64,
    pub(crate) pixel_height: f64,
}

impl PixelGrid {
    fn is_positive(&self) -> bool {
        [
            self.origin_x,
            self.origin_y,
            self.pixel_width,
            self.pixel_height,
        ]
        .iter()
        .all(|value| value.is_finite())
            && self.pixel_width > 0.0
            && self.pixel_height > 0.0
    }

    fn has_square_pixels(&self, cols: usize, rows: usize) -> bool {
        // geoplumb grids have one resolution for both axes
        let longest_side = cols.max(rows) as f64;
        (self.pixel_width - self.pixel_height).abs() * longest_side < 0.5 * self.pixel_width
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Georeference {
    grid: PixelGrid,
    epsg: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct BandLayout {
    color_bands: Vec<usize>,
    alpha_band: Option<usize>,
    opaque_alpha: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ImageryManifest {
    cog_file: String,
    epsg: u32,
    origin_x: f64,
    origin_y: f64,
    resolution: f64,
    bands: BandLayout,
    value_low: f64,
    value_high: f64,
    pub(crate) bounds_degrees: [f64; 4],
    mercator_resolution: f64,
    pub(crate) max_zoom: u8,
}

impl ImageryManifest {
    pub(crate) fn rectangle_radians(&self) -> serde_json::Value {
        let [west, south, east, north] = self.bounds_degrees.map(f64::to_radians);
        serde_json::json!({ "west": west, "south": south, "east": east, "north": north })
    }

    pub(crate) fn mercator_footprint(&self) -> Bbox {
        let [west, south, east, north] = self.bounds_degrees;
        Bbox::new(
            mercator_x(west),
            mercator_y(south.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT)),
            mercator_x(east),
            mercator_y(north.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT)),
        )
    }
}

pub(crate) fn tile_template(id: Uuid) -> String {
    format!("/api/v1/assets/{id}/imagery/{{z}}/{{x}}/{{y}}.png")
}

pub(crate) async fn read_manifest(
    data_dir: &FilePath,
    id: Uuid,
) -> Result<Option<ImageryManifest>, String> {
    let path = data_dir.join(id.to_string()).join(MANIFEST_FILE);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ImageryForm {
    pub(crate) world_file: Option<String>,
    pub(crate) bounds: Option<String>,
    pub(crate) crs: Option<String>,
    pub(crate) asset_type: Option<String>,
}

impl ImageryForm {
    pub(crate) fn places_a_raster(&self) -> bool {
        self.world_file.is_some() || self.bounds.is_some() || self.asset_type.is_some()
    }
}

pub(crate) struct PreparedUpload {
    pub(crate) asset_type: AssetType,
    pub(crate) tiles_ready: bool,
}

pub(crate) fn takes_imagery_form(extension: &str) -> bool {
    matches!(
        extension,
        "jpg" | "jpeg" | "png" | "tif" | "tiff" | "jp2" | "j2k"
    )
}

pub(crate) fn prepare_upload(
    input_path: &FilePath,
    asset_dir: &FilePath,
    detected_type: AssetType,
    form: &ImageryForm,
) -> Result<PreparedUpload, String> {
    let extension = input_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" | "png" | "jp2" | "j2k" => {
            if requested_asset_type(form)? == Some(AssetType::Terrain) {
                return Err(format!(
                    "asset_type terrain: a {extension} upload is imagery"
                ));
            }
            if matches!(extension.as_str(), "jp2" | "j2k") {
                prepare_jp2(input_path, asset_dir, form)?;
            } else {
                prepare_picture(input_path, asset_dir, form)?;
            }
            Ok(PreparedUpload {
                asset_type: AssetType::Imagery,
                tiles_ready: true,
            })
        }
        "tif" | "tiff" => prepare_tiff_upload(input_path, asset_dir, form),
        "czml" => crate::czml::check_document(input_path).map(|()| PreparedUpload {
            asset_type: AssetType::Czml,
            tiles_ready: true,
        }),
        _ => Ok(PreparedUpload {
            asset_type: detected_type,
            tiles_ready: false,
        }),
    }
}

fn requested_asset_type(form: &ImageryForm) -> Result<Option<AssetType>, String> {
    match form.asset_type.as_deref() {
        None => Ok(None),
        Some("imagery") => Ok(Some(AssetType::Imagery)),
        Some("terrain") => Ok(Some(AssetType::Terrain)),
        Some(other) => Err(format!("asset_type {other}: expected imagery or terrain")),
    }
}

fn terrain_upload(form: &ImageryForm) -> Result<PreparedUpload, String> {
    if form.world_file.is_some() || form.bounds.is_some() {
        return Err(
            "world_file and bounds apply to imagery: send asset_type imagery with them, \
             or drop them to upload terrain"
                .to_string(),
        );
    }
    Ok(PreparedUpload {
        asset_type: AssetType::Terrain,
        tiles_ready: false,
    })
}

fn prepare_tiff_upload(
    input_path: &FilePath,
    asset_dir: &FilePath,
    form: &ImageryForm,
) -> Result<PreparedUpload, String> {
    let requested = requested_asset_type(form)?;
    let tags = match read_tiff_tags(input_path) {
        Ok(tags) => tags,
        Err(reason) if requested == Some(AssetType::Imagery) => {
            return Err(format!("not a readable TIFF: {reason}"));
        }
        Err(_) => return terrain_upload(form),
    };
    let asset_type = requested.unwrap_or(if tags.samples >= 2 {
        AssetType::Imagery
    } else {
        AssetType::Terrain
    });
    if asset_type == AssetType::Terrain {
        return terrain_upload(form);
    }
    prepare_geotiff(input_path, asset_dir, form, &tags)?;
    Ok(PreparedUpload {
        asset_type,
        tiles_ready: true,
    })
}

fn form_georeference(
    form: &ImageryForm,
    cols: usize,
    rows: usize,
) -> Result<Option<Georeference>, String> {
    let grid = match (&form.bounds, &form.world_file) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err("bounds and world_file both place the image, send one of them".into());
        }
        (Some(bounds), None) => {
            let [west, south, east, north] = parse_bounds(bounds)?;
            PixelGrid {
                origin_x: west,
                origin_y: north,
                pixel_width: (east - west) / cols as f64,
                pixel_height: (north - south) / rows as f64,
            }
        }
        (None, Some(world_file)) => parse_world_file(world_file)?,
    };
    let crs = form
        .crs
        .as_deref()
        .ok_or("bounds and world files do not name a CRS, add a crs field such as EPSG:32632")?;
    Ok(Some(Georeference {
        grid,
        epsg: parse_crs(crs)?,
    }))
}

fn parse_crs(crs: &str) -> Result<u32, String> {
    parse_epsg_code(crs)
        .ok_or_else(|| format!("crs {crs}: expected an EPSG code such as EPSG:32632"))
}

fn parse_bounds(raw: &str) -> Result<[f64; 4], String> {
    let parsed: Vec<f64> = raw
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("bounds {raw}: expected west,south,east,north in crs units"))?;
    let [west, south, east, north] = parsed[..] else {
        return Err(format!(
            "bounds {raw}: expected west,south,east,north in crs units"
        ));
    };
    let is_valid = parsed.iter().all(|value| value.is_finite()) && west < east && south < north;
    if !is_valid {
        return Err(format!(
            "bounds {raw}: west must be below east and south below north"
        ));
    }
    Ok([west, south, east, north])
}

fn parse_world_file(text: &str) -> Result<PixelGrid, String> {
    let values: Vec<f64> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .map_err(|_| "world_file: every line must be a number".to_string())?;
    let [x_scale, y_rotation, x_rotation, y_scale, center_x, center_y] = values[..] else {
        return Err(format!(
            "world_file: expected 6 lines, found {}",
            values.len()
        ));
    };
    if x_rotation != 0.0 || y_rotation != 0.0 {
        return Err("world_file: rotated images are not served, north must be up".into());
    }
    if x_scale <= 0.0 || y_scale >= 0.0 {
        return Err(
            "world_file: expected a positive x pixel size and a negative y pixel size".into(),
        );
    }
    // world file coordinates are the center of the top left pixel
    Ok(PixelGrid {
        origin_x: center_x - x_scale / 2.0,
        origin_y: center_y - y_scale / 2.0,
        pixel_width: x_scale,
        pixel_height: -y_scale,
    })
}

struct TiffTags {
    cols: usize,
    rows: usize,
    samples: usize,
    bits: u16,
    sample_format: u16,
    compression: u16,
    photometric: u16,
    planar: u16,
    alpha_declared: bool,
    nodata: Option<f64>,
    grid: Option<PixelGrid>,
    epsg: Option<u32>,
}

fn first_short_tag<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
    tag: Tag,
    default: u16,
) -> u16 {
    // tiff's u16 tag getter errors on a one-value SHORT
    decoder
        .find_tag_unsigned_vec::<u16>(tag)
        .ok()
        .flatten()
        .and_then(|values| values.first().copied())
        .unwrap_or(default)
}

fn read_tiff_tags(path: &FilePath) -> Result<TiffTags, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    tiff_tags_from(BufReader::new(file))
}

fn tiff_tags_from<R: std::io::Read + std::io::Seek>(reader: R) -> Result<TiffTags, String> {
    let mut decoder = Decoder::new(reader).map_err(|e| e.to_string())?;
    let (cols, rows) = decoder.dimensions().map_err(|e| e.to_string())?;
    let samples = first_short_tag(&mut decoder, Tag::SamplesPerPixel, 1);
    let bits = first_short_tag(&mut decoder, Tag::BitsPerSample, 1);
    let sample_format = first_short_tag(&mut decoder, Tag::SampleFormat, 1);
    let compression = first_short_tag(&mut decoder, Tag::Compression, 1);
    let photometric = first_short_tag(&mut decoder, Tag::PhotometricInterpretation, 1);
    let planar = first_short_tag(&mut decoder, Tag::PlanarConfiguration, 1);
    let extra_sample = first_short_tag(&mut decoder, Tag::ExtraSamples, 0);
    let nodata = decoder
        .get_tag_ascii_string(Tag::GdalNodata)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    let geo_keys: Vec<u16> = decoder
        .find_tag_unsigned_vec(Tag::GeoKeyDirectoryTag)
        .ok()
        .flatten()
        .unwrap_or_default();
    let pixel_is_point = geo_key(&geo_keys, 1025) == Some(2);
    Ok(TiffTags {
        cols: cols as usize,
        rows: rows as usize,
        samples: usize::from(samples),
        bits,
        sample_format,
        compression,
        photometric,
        planar,
        alpha_declared: ALPHA_EXTRA_SAMPLES.contains(&extra_sample),
        nodata,
        grid: tag_pixel_grid(&mut decoder, pixel_is_point),
        epsg: geo_keys_epsg(&geo_keys),
    })
}

fn tag_pixel_grid<R: std::io::Read + std::io::Seek>(
    decoder: &mut Decoder<R>,
    pixel_is_point: bool,
) -> Option<PixelGrid> {
    let scale = decoder.get_tag_f64_vec(Tag::ModelPixelScaleTag).ok();
    let tiepoint = decoder.get_tag_f64_vec(Tag::ModelTiepointTag).ok();
    let grid = match (scale, tiepoint) {
        (Some(scale), Some(tiepoint)) if scale.len() >= 2 && tiepoint.len() >= 6 => PixelGrid {
            origin_x: tiepoint[3] - tiepoint[0] * scale[0],
            origin_y: tiepoint[4] + tiepoint[1] * scale[1],
            pixel_width: scale[0],
            pixel_height: scale[1],
        },
        _ => {
            let matrix = decoder
                .get_tag_f64_vec(Tag::Unknown(MODEL_TRANSFORMATION_TAG))
                .ok()?;
            let is_north_up = matrix.len() >= 16 && matrix[1] == 0.0 && matrix[4] == 0.0;
            if !is_north_up {
                return None;
            }
            PixelGrid {
                origin_x: matrix[3],
                origin_y: matrix[7],
                pixel_width: matrix[0],
                pixel_height: -matrix[5],
            }
        }
    };
    if !pixel_is_point {
        return Some(grid);
    }
    // PixelIsPoint tiepoints are the center of the top left pixel
    Some(PixelGrid {
        origin_x: grid.origin_x - grid.pixel_width / 2.0,
        origin_y: grid.origin_y + grid.pixel_height / 2.0,
        ..grid
    })
}

fn geo_key(keys: &[u16], id: u16) -> Option<u16> {
    let (entries, _) = keys.get(4..)?.as_chunks::<4>();
    entries
        .iter()
        .find(|entry| entry[0] == id && entry[1] == 0)
        .map(|entry| entry[3])
}

fn geo_keys_epsg(keys: &[u16]) -> Option<u32> {
    let projected = geo_key(keys, 3072);
    let geographic = geo_key(keys, 2048);
    let code = match geo_key(keys, 1024) {
        Some(2) => geographic,
        _ => projected.or(geographic),
    }?;
    (code != 0 && code != USER_DEFINED_GEO_KEY).then_some(u32::from(code))
}

fn color_band_count(samples: usize) -> usize {
    if samples >= 3 { 3 } else { 1 }
}

fn band_layout(samples: usize, alpha_declared: bool, format: SampleFormat) -> BandLayout {
    let first_extra = color_band_count(samples);
    let color_bands: Vec<usize> = (0..first_extra).collect();
    BandLayout {
        color_bands,
        alpha_band: (alpha_declared && samples > first_extra).then_some(first_extra),
        opaque_alpha: format.integer_range().map_or(1.0, |(_, high)| high),
    }
}

struct EmbeddedPlacement {
    grid: Option<PixelGrid>,
    epsg: Option<u32>,
    source: &'static str,
}

fn form_or_embedded_georeference(
    form: &ImageryForm,
    cols: usize,
    rows: usize,
    embedded: EmbeddedPlacement,
) -> Result<Georeference, String> {
    if let Some(georeference) = form_georeference(form, cols, rows)? {
        return Ok(georeference);
    }
    let source = embedded.source;
    let grid = embedded.grid.ok_or_else(|| {
        format!(
            "no north-up georeferencing in {source}: add a world_file or bounds field with a crs"
        )
    })?;
    let epsg = match (&form.crs, embedded.epsg) {
        (Some(crs), _) => parse_crs(crs)?,
        (None, Some(epsg)) => epsg,
        (None, None) => {
            return Err(format!(
                "{source} name no EPSG code: add a crs field such as EPSG:32632"
            ));
        }
    };
    Ok(Georeference { grid, epsg })
}

fn tiff_sample_format(tags: &TiffTags) -> Result<SampleFormat, String> {
    match (
        SampleFormat::from_tiff(tags.bits, tags.sample_format),
        tags.bits,
    ) {
        (Some(format), _) => Ok(format),
        // terrano has no 64 bit integer samples
        (None, 64) => Ok(SampleFormat::F64),
        (None, bits) => Err(format!(
            "{bits} bit samples of TIFF format {} are not served",
            tags.sample_format
        )),
    }
}

// tiff 6.0 sets bit 2 of NewSubfileType on a transparency mask
const NEW_SUBFILE_TYPE_MASK_BIT: u32 = 4;
// tiff 6.0 gives a transparency mask PhotometricInterpretation 4
const TRANSPARENCY_MASK_PHOTOMETRIC: u16 = 4;

fn has_internal_mask(path: &FilePath) -> Result<bool, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut decoder = Decoder::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    loop {
        let subfile_type = decoder
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        let photometric = first_short_tag(&mut decoder, Tag::PhotometricInterpretation, 0);
        if subfile_type & NEW_SUBFILE_TYPE_MASK_BIT != 0
            || photometric == TRANSPARENCY_MASK_PHOTOMETRIC
        {
            return Ok(true);
        }
        if !decoder.more_images() {
            return Ok(false);
        }
        decoder.next_image().map_err(|e| {
            format!("a later image in this TIFF cannot be read, and it can be a mask: {e}")
        })?;
    }
}

fn prepare_geotiff(
    input_path: &FilePath,
    asset_dir: &FilePath,
    form: &ImageryForm,
    tags: &TiffTags,
) -> Result<(), String> {
    let embedded = EmbeddedPlacement {
        grid: tags.grid,
        epsg: tags.epsg,
        source: "the TIFF tags",
    };
    let georeference = form_or_embedded_georeference(form, tags.cols, tags.rows, embedded)?;
    if !georeference.grid.is_positive() {
        return Err("the georeferencing has a pixel size that is not positive".into());
    }
    let format = tiff_sample_format(tags)?;
    let bands = band_layout(tags.samples, tags.alpha_declared, format);

    if serves_as_is(input_path, tags, &georeference) {
        let cog_file = input_path
            .strip_prefix(asset_dir)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        return write_manifest(asset_dir, cog_file, &georeference, format, bands);
    }

    if has_internal_mask(input_path)? {
        return Err(
            "this TIFF has an internal transparency mask, which is applied only to a tiled, deflate \
             or uncompressed COG with overviews: make it one with \
             gdal_translate -of COG -co COMPRESS=DEFLATE, or make the mask an alpha band with \
             -b 1 -b 2 -b 3 -b mask -colorinterp red,green,blue,alpha"
                .into(),
        );
    }
    let decoded = decode_tiff(input_path, asset_dir, tags, format)?;
    write_source_cog(asset_dir, decoded, georeference, bands)
}

fn serves_as_is(input_path: &FilePath, tags: &TiffTags, georeference: &Georeference) -> bool {
    if !DIRECT_PHOTOMETRICS.contains(&tags.photometric)
        || !georeference.grid.has_square_pixels(tags.cols, tags.rows)
    {
        return false;
    }
    let Ok(file) = File::open(input_path) else {
        return false;
    };
    let Ok(reader) = CogReader::open(file) else {
        return false;
    };
    let levels = reader.levels();
    let tiles_are_small = levels
        .iter()
        .all(|level| level.tile_width.max(level.tile_height) <= MAX_DIRECT_READ_SIDE);
    let has_a_small_level = levels.iter().any(|level| {
        level.samples == tags.samples && level.width.max(level.height) <= MAX_DIRECT_READ_SIDE
    });
    tiles_are_small && has_a_small_level
}

// terrano's cog writer stores the image width and height as 16 bit tiff shorts
const MAX_SIDE: usize = u16::MAX as usize;

// terrano's cog has 32 bit offsets, and its overviews add a third to the decoded bytes
const MAX_DECODED_BYTES: u64 = 2 << 30;

fn check_decoded_size(cols: usize, rows: usize, pixel_bytes: usize) -> Result<(), String> {
    let decoded_bytes = (cols as u64)
        .saturating_mul(rows as u64)
        .saturating_mul(pixel_bytes as u64);
    if cols.max(rows) > MAX_SIDE || decoded_bytes > MAX_DECODED_BYTES {
        return Err(format!(
            "{cols} x {rows} pixels of {pixel_bytes} bytes is past the limit of {MAX_SIDE} pixels \
             a side and {MAX_DECODED_BYTES} decoded bytes for a file that is not a tiled, deflate \
             or uncompressed COG with overviews: convert it with \
             gdal_translate -of COG -co COMPRESS=DEFLATE"
        ));
    }
    Ok(())
}

struct ScratchRaster {
    path: PathBuf,
    file: File,
    cols: usize,
    rows: usize,
    pixel_bytes: usize,
}

impl ScratchRaster {
    fn create(
        asset_dir: &FilePath,
        name: &str,
        cols: usize,
        rows: usize,
        pixel_bytes: usize,
    ) -> Result<Self, String> {
        let directory = asset_dir.join("imagery");
        std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let path = directory.join(name);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(ScratchRaster {
            path,
            file,
            cols,
            rows,
            pixel_bytes,
        })
    }

    fn offset(&self, row: usize, col: usize) -> u64 {
        (row as u64 * self.cols as u64 + col as u64) * self.pixel_bytes as u64
    }

    fn read_pixels(&self, row: usize, col: usize, pixels: &mut [u8]) -> std::io::Result<()> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(self.offset(row, col)))?;
        file.read_exact(pixels)
    }

    fn read_row(&self, row: usize) -> std::io::Result<Vec<u8>> {
        let mut pixels = vec![0; self.cols * self.pixel_bytes];
        self.read_pixels(row, 0, &mut pixels)?;
        Ok(pixels)
    }

    fn write_pixels(&self, row: usize, col: usize, pixels: &[u8]) -> std::io::Result<()> {
        let mut file = &self.file;
        file.seek(SeekFrom::Start(self.offset(row, col)))?;
        file.write_all(pixels)
    }

    fn remove(self) -> Result<(), String> {
        drop(self.file);
        std::fs::remove_file(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

struct DecodedRaster {
    scratch: ScratchRaster,
    samples: usize,
    format: SampleFormat,
    nodata: Option<f64>,
}

fn encoded_samples<T: Copy>(
    values: &[T],
    to_f64: impl Fn(T) -> f64,
    format: SampleFormat,
    float_nodata: Option<f64>,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * format.bytes() as usize);
    for &value in values {
        let value = to_f64(value);
        let value = if Some(value) == float_nodata {
            f64::NAN
        } else {
            value
        };
        format.encode(value, &mut bytes);
    }
    bytes
}

// tiff decodes one strip or tile whole, and a striped file can be a single strip
const MAX_TIFF_CHUNK_BYTES: usize = 64 << 20;

fn decode_tiff(
    input_path: &FilePath,
    asset_dir: &FilePath,
    tags: &TiffTags,
    format: SampleFormat,
) -> Result<DecodedRaster, String> {
    let pixel_bytes = tags.samples * format.bytes() as usize;
    check_decoded_size(tags.cols, tags.rows, pixel_bytes)?;
    if tags.planar != 1 {
        return Err("TIFF band planes stored separately are not served".into());
    }
    let file = File::open(input_path).map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.decoding_buffer_size = MAX_TIFF_CHUNK_BYTES;
    limits.intermediate_buffer_size = MAX_TIFF_CHUNK_BYTES;
    let mut decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| e.to_string())?
        .with_limits(limits);
    let color = decoder
        .colortype()
        .map_err(|e| format!("this TIFF layout cannot be decoded: {e}"))?;
    let decodes_to_bands = match color {
        ColorType::Gray(_) | ColorType::RGB(_) | ColorType::RGBA(_) => true,
        // tiff's jpeg decoder returns rgb, and raw YCbCr samples stay YCbCr
        ColorType::YCbCr(_) => tags.compression == TIFF_JPEG_COMPRESSION,
        _ => false,
    };
    if !decodes_to_bands {
        return Err(format!("TIFF color type {color:?} is not served"));
    }
    let (chunk_width, chunk_height) = decoder.chunk_dimensions();
    let (chunk_width, chunk_height) = (chunk_width as usize, chunk_height as usize);
    if chunk_width == 0 || chunk_height == 0 {
        return Err("TIFF strips or tiles of no pixels are not served".into());
    }
    let chunk_bytes = chunk_width.min(tags.cols) * chunk_height.min(tags.rows) * pixel_bytes;
    if chunk_bytes > MAX_TIFF_CHUNK_BYTES {
        return Err(format!(
            "TIFF strips or tiles of {chunk_bytes} bytes are past the {MAX_TIFF_CHUNK_BYTES} byte \
             limit: convert it with gdal_translate -of COG -co COMPRESS=DEFLATE"
        ));
    }
    let chunk_count = match decoder.get_chunk_type() {
        ChunkType::Strip => decoder.strip_count(),
        ChunkType::Tile => decoder.tile_count(),
    }
    .map_err(|e| format!("this TIFF cannot be decoded: {e}"))?;
    let chunks_across = tags.cols.div_ceil(chunk_width);
    let chunks_expected = chunks_across * tags.rows.div_ceil(chunk_height);
    if chunk_count as usize != chunks_expected {
        return Err(format!(
            "this TIFF has {chunk_count} strips or tiles where its size needs {chunks_expected}"
        ));
    }
    let float_nodata = tags.nodata.filter(|_| format.integer_range().is_none());
    let scratch = ScratchRaster::create(
        asset_dir,
        DECODED_SCRATCH_FILE,
        tags.cols,
        tags.rows,
        pixel_bytes,
    )?;
    for chunk in 0..chunk_count {
        let decoded = decoder
            .read_chunk(chunk)
            .map_err(|e| format!("this TIFF cannot be decoded: {e}"))?;
        let bytes = match decoded {
            DecodingResult::U8(values) => encoded_samples(&values, f64::from, format, float_nodata),
            DecodingResult::U16(values) => {
                encoded_samples(&values, f64::from, format, float_nodata)
            }
            DecodingResult::U32(values) => {
                encoded_samples(&values, f64::from, format, float_nodata)
            }
            DecodingResult::U64(values) => {
                encoded_samples(&values, |v| v as f64, format, float_nodata)
            }
            DecodingResult::I8(values) => encoded_samples(&values, f64::from, format, float_nodata),
            DecodingResult::I16(values) => {
                encoded_samples(&values, f64::from, format, float_nodata)
            }
            DecodingResult::I32(values) => {
                encoded_samples(&values, f64::from, format, float_nodata)
            }
            DecodingResult::I64(values) => {
                encoded_samples(&values, |v| v as f64, format, float_nodata)
            }
            DecodingResult::F32(values) => {
                encoded_samples(&values, f64::from, format, float_nodata)
            }
            DecodingResult::F64(values) => encoded_samples(&values, |v| v, format, float_nodata),
        };
        let (data_width, data_height) = decoder.chunk_data_dimensions(chunk);
        let row_bytes = data_width as usize * pixel_bytes;
        if bytes.len() != row_bytes * data_height as usize {
            return Err(format!(
                "TIFF strip or tile {chunk} decoded to {} bytes, expected {}",
                bytes.len(),
                row_bytes * data_height as usize
            ));
        }
        let chunk = chunk as usize;
        let first_col = chunk % chunks_across * chunk_width;
        let first_row = chunk / chunks_across * chunk_height;
        for (row, pixels) in bytes.chunks_exact(row_bytes).enumerate() {
            scratch
                .write_pixels(first_row + row, first_col, pixels)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(DecodedRaster {
        scratch,
        samples: tags.samples,
        format,
        nodata: cog_nodata(format, tags.nodata),
    })
}

fn cog_nodata(format: SampleFormat, declared: Option<f64>) -> Option<f64> {
    match format.integer_range() {
        None => Some(f64::NAN),
        Some((low, high)) => declared.filter(|v| v.fract() == 0.0 && (low..=high).contains(v)),
    }
}

fn prepare_picture(
    input_path: &FilePath,
    asset_dir: &FilePath,
    form: &ImageryForm,
) -> Result<(), String> {
    let open_reader = || {
        ImageReader::open(input_path)
            .map_err(|e| e.to_string())?
            .with_guessed_format()
            .map_err(|e| e.to_string())
    };
    let reader = open_reader()?;
    let is_png = reader.format() == Some(ImageFormat::Png);
    let (cols, rows) = reader
        .into_dimensions()
        .map_err(|e| format!("this image cannot be read: {e}"))?;
    let (cols, rows) = (cols as usize, rows as usize);

    let georeference = form_georeference(form, cols, rows)?.ok_or(
        "JPEG and PNG carry no georeferencing: add a world_file or bounds field with a crs",
    )?;
    if !georeference.grid.is_positive() {
        return Err("the georeferencing has a pixel size that is not positive".into());
    }

    let png_rows = if is_png {
        non_interlaced_png_rows(input_path)?
    } else {
        None
    };
    let (decoded, alpha_declared) = match png_rows {
        Some(png_rows) => decode_png_rows(png_rows, asset_dir, cols, rows)?,
        None => decode_whole_picture(open_reader()?, asset_dir, cols, rows)?,
    };
    let bands = band_layout(decoded.samples, alpha_declared, decoded.format);
    write_source_cog(asset_dir, decoded, georeference, bands)
}

fn non_interlaced_png_rows(
    input_path: &FilePath,
) -> Result<Option<png::Reader<BufReader<File>>>, String> {
    let file = File::open(input_path).map_err(|e| e.to_string())?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let reader = decoder
        .read_info()
        .map_err(|e| format!("this image cannot be read: {e}"))?;
    Ok((!reader.info().interlaced).then_some(reader))
}

fn decode_png_rows(
    mut reader: png::Reader<BufReader<File>>,
    asset_dir: &FilePath,
    cols: usize,
    rows: usize,
) -> Result<(DecodedRaster, bool), String> {
    let (color, _) = reader.output_color_type();
    let samples = color.samples();
    check_decoded_size(cols, rows, samples)?;
    let scratch = ScratchRaster::create(asset_dir, DECODED_SCRATCH_FILE, cols, rows, samples)?;
    for row in 0..rows {
        let pixels = reader
            .next_row()
            .map_err(|e| format!("this image cannot be decoded: {e}"))?
            .ok_or("this image ends before its last row")?;
        scratch
            .write_pixels(row, 0, pixels.data())
            .map_err(|e| e.to_string())?;
    }
    let decoded = DecodedRaster {
        scratch,
        samples,
        format: SampleFormat::U8,
        nodata: None,
    };
    let alpha_declared = matches!(color, png::ColorType::GrayscaleAlpha | png::ColorType::Rgba);
    Ok((decoded, alpha_declared))
}

// zune-jpeg holds two bytes of coefficients per sample of a progressive jpeg beside the decoded image
const WHOLE_PICTURE_BYTES_PER_DECODED_BYTE: u64 = 3;

fn decode_whole_picture(
    mut reader: ImageReader<BufReader<File>>,
    asset_dir: &FilePath,
    cols: usize,
    rows: usize,
) -> Result<(DecodedRaster, bool), String> {
    reader.no_limits();
    let decoder = reader
        .into_decoder()
        .map_err(|e| format!("this image cannot be read: {e}"))?;
    let decoded_bytes = ImageDecoder::total_bytes(&decoder);
    let max_decoded_bytes = BUILD_BUDGET_BYTES / WHOLE_PICTURE_BYTES_PER_DECODED_BYTE;
    if decoded_bytes > max_decoded_bytes {
        return Err(format!(
            "{cols} x {rows} pixels decode to {decoded_bytes} bytes, past the {max_decoded_bytes} \
             byte limit for pictures that decode whole, such as JPEG and interlaced PNG: send a \
             PNG that is not interlaced, or a GeoTIFF"
        ));
    }
    let picture = DynamicImage::from_decoder(decoder)
        .map_err(|e| format!("this image cannot be decoded: {e}"))?;
    let color = picture.color();
    let (pixels, samples) = if color.has_alpha() {
        (picture.into_rgba8().into_raw(), 4)
    } else if color.has_color() {
        (picture.into_rgb8().into_raw(), 3)
    } else {
        (picture.into_luma8().into_raw(), 1)
    };
    check_decoded_size(cols, rows, samples)?;
    let scratch = ScratchRaster::create(asset_dir, DECODED_SCRATCH_FILE, cols, rows, samples)?;
    scratch
        .write_pixels(0, 0, &pixels)
        .map_err(|e| e.to_string())?;
    let decoded = DecodedRaster {
        scratch,
        samples,
        format: SampleFormat::U8,
        nodata: None,
    };
    Ok((decoded, color.has_alpha()))
}

#[cfg(not(feature = "jp2"))]
fn prepare_jp2(_: &FilePath, _: &FilePath, _: &ImageryForm) -> Result<(), String> {
    Err(
        "this server was built without the jp2 feature, so it cannot decode JPEG 2000: \
         convert the file to a GeoTIFF"
            .into(),
    )
}

#[cfg(feature = "jp2")]
fn prepare_jp2(
    input_path: &FilePath,
    asset_dir: &FilePath,
    form: &ImageryForm,
) -> Result<(), String> {
    let layout = jp2::read_layout(input_path)?;
    let geotiff_tags = layout
        .geotiff
        .as_deref()
        .and_then(|bytes| tiff_tags_from(Cursor::new(bytes)).ok());
    let gml = layout
        .gml_documents
        .iter()
        .find_map(|document| jp2::gml_placement(document));
    let declared_nodata = geotiff_tags.as_ref().and_then(|tags| tags.nodata);
    let embedded = EmbeddedPlacement {
        grid: geotiff_tags
            .as_ref()
            .and_then(|tags| tags.grid)
            .or(gml.as_ref().map(|placement| placement.grid)),
        epsg: geotiff_tags
            .and_then(|tags| tags.epsg)
            .or(gml.and_then(|placement| placement.epsg)),
        source: "the GeoJP2 and GMLJP2 boxes",
    };
    let georeference = form_or_embedded_georeference(form, layout.cols, layout.rows, embedded)?;
    if !georeference.grid.is_positive() {
        return Err("the georeferencing has a pixel size that is not positive".into());
    }

    let format = jp2::sample_format(layout.precision, layout.signed)?;
    let alpha_declared = layout.first_opacity_band == Some(color_band_count(layout.samples));
    let mut bands = band_layout(layout.samples, alpha_declared, format);
    if !layout.signed {
        bands.opaque_alpha = f64::from((1u32 << layout.precision) - 1);
    }
    let decoded = decode_jp2(input_path, asset_dir, &layout, format, declared_nodata)?;
    write_source_cog(asset_dir, decoded, georeference, bands)
}

#[cfg(feature = "jp2")]
fn decode_jp2(
    input_path: &FilePath,
    asset_dir: &FilePath,
    layout: &jp2::Jp2Layout,
    format: SampleFormat,
    declared_nodata: Option<f64>,
) -> Result<DecodedRaster, String> {
    let pixel_bytes = layout.samples * format.bytes() as usize;
    check_decoded_size(layout.cols, layout.rows, pixel_bytes)?;
    let rows_per_window = jp2::rows_per_window(layout.cols, layout.samples, layout.tile_height)?;
    let scratch = ScratchRaster::create(
        asset_dir,
        DECODED_SCRATCH_FILE,
        layout.cols,
        layout.rows,
        pixel_bytes,
    )?;
    for first_row in (0..layout.rows).step_by(rows_per_window) {
        let last_row = (first_row + rows_per_window).min(layout.rows);
        let pixels = jp2::decode_rows(input_path, layout, first_row..last_row, format)?;
        scratch
            .write_pixels(first_row, 0, &pixels)
            .map_err(|e| e.to_string())?;
    }
    Ok(DecodedRaster {
        scratch,
        samples: layout.samples,
        format,
        nodata: cog_nodata(format, declared_nodata),
    })
}

fn onto_square_pixels(
    asset_dir: &FilePath,
    decoded: DecodedRaster,
    grid: PixelGrid,
) -> Result<(DecodedRaster, PixelGrid), String> {
    let (cols, rows) = (decoded.scratch.cols, decoded.scratch.rows);
    if grid.has_square_pixels(cols, rows) {
        return Ok((decoded, grid));
    }
    let pixel_bytes = decoded.scratch.pixel_bytes;
    let resolution = grid.pixel_width.min(grid.pixel_height);
    let square_cols = (cols as f64 * grid.pixel_width / resolution).round() as usize;
    let square_rows = (rows as f64 * grid.pixel_height / resolution).round() as usize;
    check_decoded_size(square_cols, square_rows, pixel_bytes)?;
    let source_col =
        |col: usize| (((col as f64 + 0.5) * resolution / grid.pixel_width) as usize).min(cols - 1);
    let source_cols: Vec<usize> = (0..square_cols).map(source_col).collect();
    let square = ScratchRaster::create(
        asset_dir,
        "square.raw",
        square_cols,
        square_rows,
        pixel_bytes,
    )?;
    let mut square_row = Vec::with_capacity(square_cols * pixel_bytes);
    for row in 0..square_rows {
        let source_row =
            (((row as f64 + 0.5) * resolution / grid.pixel_height) as usize).min(rows - 1);
        let source = decoded
            .scratch
            .read_row(source_row)
            .map_err(|e| e.to_string())?;
        square_row.clear();
        for &col in &source_cols {
            square_row.extend_from_slice(&source[col * pixel_bytes..(col + 1) * pixel_bytes]);
        }
        square
            .write_pixels(row, 0, &square_row)
            .map_err(|e| e.to_string())?;
    }
    decoded.scratch.remove()?;
    Ok((
        DecodedRaster {
            scratch: square,
            ..decoded
        },
        PixelGrid {
            pixel_width: resolution,
            pixel_height: resolution,
            ..grid
        },
    ))
}

fn overview_level(
    asset_dir: &FilePath,
    finer: &ScratchRaster,
    level: u32,
    format: SampleFormat,
    nodata: Option<f64>,
) -> Result<ScratchRaster, String> {
    let pixel_bytes = finer.pixel_bytes;
    let sample_bytes = format.bytes() as usize;
    let cols = finer.cols.div_ceil(2);
    let rows = finer.rows.div_ceil(2);
    let coarser = ScratchRaster::create(
        asset_dir,
        &format!("overview-{level}.raw"),
        cols,
        rows,
        pixel_bytes,
    )?;
    let absent = nodata.unwrap_or(f64::NAN);
    let mut averaged = Vec::with_capacity(cols * pixel_bytes);
    for row in 0..rows {
        let mut source_rows = vec![finer.read_row(2 * row).map_err(|e| e.to_string())?];
        if 2 * row + 1 < finer.rows {
            source_rows.push(finer.read_row(2 * row + 1).map_err(|e| e.to_string())?);
        }
        averaged.clear();
        for col in 0..cols {
            let source_cols = 2 * col..(2 * col + 2).min(finer.cols);
            for sample in 0..pixel_bytes / sample_bytes {
                let mut sum = 0.0;
                let mut count = 0usize;
                for source in &source_rows {
                    for source_col in source_cols.clone() {
                        let at = source_col * pixel_bytes + sample * sample_bytes;
                        let value = format.decode(&source[at..]);
                        if !value.is_nan() && Some(value) != nodata {
                            sum += value;
                            count += 1;
                        }
                    }
                }
                let value = if count > 0 {
                    sum / count as f64
                } else {
                    absent
                };
                format.encode(value, &mut averaged);
            }
        }
        coarser
            .write_pixels(row, 0, &averaged)
            .map_err(|e| e.to_string())?;
    }
    Ok(coarser)
}

fn cog_tile(
    level: &ScratchRaster,
    tile_col: usize,
    tile_row: usize,
    padding: &[u8],
) -> std::io::Result<Vec<u8>> {
    let mut tile = padding.repeat(COG_TILE_SIDE * COG_TILE_SIDE);
    let first_col = tile_col * COG_TILE_SIDE;
    let first_row = tile_row * COG_TILE_SIDE;
    let row_bytes = COG_TILE_SIDE.min(level.cols - first_col) * level.pixel_bytes;
    let rows_inside = COG_TILE_SIDE.min(level.rows - first_row);
    let tile_rows = tile.chunks_exact_mut(COG_TILE_SIDE * level.pixel_bytes);
    for (row, pixels) in tile_rows.take(rows_inside).enumerate() {
        level.read_pixels(first_row + row, first_col, &mut pixels[..row_bytes])?;
    }
    Ok(tile)
}

const COARSEST_OVERVIEW_SIDE: f64 = 512.0;

fn write_source_cog(
    asset_dir: &FilePath,
    decoded: DecodedRaster,
    georeference: Georeference,
    bands: BandLayout,
) -> Result<(), String> {
    let (decoded, grid) = onto_square_pixels(asset_dir, decoded, georeference.grid)?;
    let epsg = u16::try_from(georeference.epsg)
        .map_err(|_| format!("EPSG:{} does not fit a GeoTIFF key", georeference.epsg))?;
    let DecodedRaster {
        scratch,
        samples,
        format,
        nodata,
    } = decoded;
    let (cols, rows) = (scratch.cols, scratch.rows);
    let overview_levels = (cols.max(rows) as f64 / COARSEST_OVERVIEW_SIDE)
        .log2()
        .ceil()
        .max(0.0) as u32;
    let mut levels = vec![scratch];
    for level in 1..=overview_levels {
        let finer = levels.last().expect("levels start with the full image");
        let coarser = overview_level(asset_dir, finer, level, format, nodata)?;
        levels.push(coarser);
    }

    let params = CogParams {
        tile_width: COG_TILE_SIDE as u32,
        tile_height: COG_TILE_SIDE as u32,
        overview_levels,
        epsg,
        origin_x: grid.origin_x,
        origin_y: grid.origin_y,
        pixel_width: grid.pixel_width,
        pixel_height: grid.pixel_height,
        deflate: true,
        nodata,
        format,
    };
    // terrano pads tiles past the image edge with nodata, or zero for integers and NaN for floats
    let padding_value = nodata.unwrap_or(if format.integer_range().is_some() {
        0.0
    } else {
        f64::NAN
    });
    let mut padding = Vec::new();
    for _ in 0..samples {
        format.encode(padding_value, &mut padding);
    }
    let path = asset_dir.join(SOURCE_COG_FILE);
    let mut writer = BufWriter::new(File::create(path).map_err(|e| e.to_string())?);
    write_cog_tiles(
        cols,
        rows,
        samples,
        &params,
        |level, tile_col, tile_row| {
            cog_tile(&levels[level], tile_col, tile_row, &padding)
                .map_err(terrano_core::Error::from)
        },
        &mut writer,
    )
    .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    drop(writer);
    for level in levels {
        level.remove()?;
    }
    write_manifest(
        asset_dir,
        SOURCE_COG_FILE.to_string(),
        &Georeference {
            grid,
            epsg: georeference.epsg,
        },
        format,
        bands,
    )
}

fn write_manifest(
    asset_dir: &FilePath,
    cog_file: String,
    georeference: &Georeference,
    format: SampleFormat,
    bands: BandLayout,
) -> Result<(), String> {
    let file = File::open(asset_dir.join(&cog_file)).map_err(|e| e.to_string())?;
    let reader = CogReader::open(file).map_err(|e| e.to_string())?;
    let (cols, rows) = {
        let base = &reader.levels()[0];
        (base.width, base.height)
    };
    let resolution = georeference.grid.pixel_width;
    let (value_low, value_high) = value_range(&reader, format, &bands.color_bands)?;
    let extent = mercator_extent(georeference, cols, rows)?;
    let manifest = ImageryManifest {
        cog_file,
        epsg: georeference.epsg,
        origin_x: georeference.grid.origin_x,
        origin_y: georeference.grid.origin_y,
        resolution,
        bands,
        value_low,
        value_high,
        bounds_degrees: extent.bounds_degrees,
        mercator_resolution: extent.resolution,
        max_zoom: max_zoom(extent.resolution),
    };
    open_layer(asset_dir, manifest.clone())
        .map_err(|e| format!("EPSG:{} cannot be tiled: {e}", georeference.epsg))?;
    std::fs::create_dir_all(asset_dir.join("imagery")).map_err(|e| e.to_string())?;
    let json = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(asset_dir.join(MANIFEST_FILE), json).map_err(|e| e.to_string())
}

fn value_range(
    reader: &CogReader<File>,
    format: SampleFormat,
    color_bands: &[usize],
) -> Result<(f64, f64), String> {
    if format == SampleFormat::U8 {
        return Ok((0.0, 255.0));
    }
    let levels = level_scales(reader, 1.0);
    let coarsest = levels.last().expect("the first level always qualifies");
    let level = &reader.levels()[coarsest.index];
    let window = reader
        .read_window_bands(coarsest.index, 0, 0, level.width, level.height)
        .map_err(|e| e.to_string())?;
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for &band in color_bands {
        for &value in window.bands()[band].data() {
            if value.is_finite() {
                low = low.min(value);
                high = high.max(value);
            }
        }
    }
    if low > high {
        return Err("every sample is nodata".into());
    }
    Ok((low, high))
}

struct MercatorExtent {
    bounds_degrees: [f64; 4],
    resolution: f64,
}

fn mercator_extent(
    georeference: &Georeference,
    cols: usize,
    rows: usize,
) -> Result<MercatorExtent, String> {
    const EDGE_STEPS: usize = 32;
    const PERIMETER_POINTS_PER_STEP: usize = 4;
    const POLE_ROUND_TRIP_DEGREES: f64 = 1e-6;
    let grid = &georeference.grid;
    let width = cols as f64 * grid.pixel_width;
    let height = rows as f64 * grid.pixel_height;
    let center_x = grid.origin_x + width / 2.0;
    let mut perimeter = Vec::with_capacity(PERIMETER_POINTS_PER_STEP * (EDGE_STEPS + 1));
    let mut pixel_neighbors = Vec::with_capacity(PERIMETER_POINTS_PER_STEP * (EDGE_STEPS + 1));
    for step in 0..=EDGE_STEPS {
        let t = step as f64 / EDGE_STEPS as f64;
        for (x, y) in [
            (grid.origin_x + t * width, grid.origin_y),
            (grid.origin_x + t * width, grid.origin_y - height),
            (grid.origin_x, grid.origin_y - t * height),
            (grid.origin_x + width, grid.origin_y - t * height),
        ] {
            perimeter.push(Coord3D { x, y, z: 0.0 });
            let toward_center = if x > center_x {
                -grid.pixel_width
            } else {
                grid.pixel_width
            };
            pixel_neighbors.push(Coord3D {
                x: x + toward_center,
                y,
                z: 0.0,
            });
        }
    }
    let transformer = Transformer::new(CrsDef::Epsg(georeference.epsg), CrsDef::Epsg(4326));
    let cannot_project = |e| format!("EPSG:{} cannot be projected: {e}", georeference.epsg);
    let projected = transformer
        .transform_batch(&perimeter)
        .map_err(cannot_project)?;
    let projected_neighbors = transformer
        .transform_batch(&pixel_neighbors)
        .map_err(cannot_project)?;
    let finest_pixel = projected
        .iter()
        .zip(&projected_neighbors)
        .filter(|(a, b)| a.y.abs() <= MERCATOR_MAX_LAT && b.y.abs() <= MERCATOR_MAX_LAT)
        .map(|(a, b)| (mercator_x(a.x) - mercator_x(b.x)).hypot(mercator_y(a.y) - mercator_y(b.y)))
        .fold(f64::INFINITY, f64::min);
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for point in &projected {
        bounds[0] = bounds[0].min(point.x);
        bounds[1] = bounds[1].min(point.y);
        bounds[2] = bounds[2].max(point.x);
        bounds[3] = bounds[3].max(point.y);
    }
    let [west, south, east, north] = bounds;
    let past_west = west < -180.0 - GLOBE_EDGE_TOLERANCE_DEGREES;
    let past_east = east > 180.0 + GLOBE_EDGE_TOLERANCE_DEGREES;
    let on_the_latitudes = south >= -90.0 - GLOBE_EDGE_TOLERANCE_DEGREES
        && north <= 90.0 + GLOBE_EDGE_TOLERANCE_DEGREES;
    let across_the_antimeridian = (past_west && east >= -180.0) || (past_east && west <= 180.0);
    if on_the_latitudes && across_the_antimeridian {
        return Err(format!(
            "the image lands at {bounds:?} in degrees, across the antimeridian: split it in two at 180 degrees"
        ));
    }
    if past_west || past_east || !on_the_latitudes {
        return Err(format!(
            "the image lands at {bounds:?} in degrees, off the globe: check the CRS, EPSG:{} was used",
            georeference.epsg
        ));
    }
    let mut bounds_degrees = [
        west.max(-180.0),
        south.max(-90.0),
        east.min(180.0),
        north.min(90.0),
    ];
    // perimeters of images around a pole reach neither the pole nor every longitude
    let degrees_to_source = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(georeference.epsg));
    let mut holds_a_pole = false;
    for pole_latitude in [90.0, -90.0] {
        let pole = Coord3D {
            x: 0.0,
            y: pole_latitude,
            z: 0.0,
        };
        // projections with no point for a pole fail, and their images contain no pole
        let Ok(in_source) = degrees_to_source.transform_batch(&[pole]) else {
            continue;
        };
        // stereographic projections send the far pole to the near pole's point
        let Ok(back) = transformer.transform_batch(&in_source) else {
            continue;
        };
        let holds_this_pole = (back[0].y - pole_latitude).abs() < POLE_ROUND_TRIP_DEGREES
            && (grid.origin_x..=grid.origin_x + width).contains(&in_source[0].x)
            && (grid.origin_y - height..=grid.origin_y).contains(&in_source[0].y);
        if holds_this_pole {
            holds_a_pole = true;
            bounds_degrees[0] = -180.0;
            bounds_degrees[2] = 180.0;
            let latitude_slot = if pole_latitude > 0.0 { 3 } else { 1 };
            bounds_degrees[latitude_slot] = pole_latitude;
        }
    }
    // some CRSs wrap longitude to -180..180
    let perimeter_jumps = projected
        .windows(PERIMETER_POINTS_PER_STEP + 1)
        .any(|points| (points[PERIMETER_POINTS_PER_STEP].x - points[0].x).abs() > 180.0);
    if perimeter_jumps && !holds_a_pole {
        return Err(format!(
            "the image lands at {bounds:?} in degrees, across the antimeridian: split it in two at 180 degrees"
        ));
    }
    // images wholly past web mercator's latitude edge have no pixel to measure
    let resolution = if finest_pixel.is_finite() {
        finest_pixel
    } else {
        (mercator_x(bounds_degrees[2]) - mercator_x(bounds_degrees[0])) / cols as f64
    };
    Ok(MercatorExtent {
        bounds_degrees,
        resolution,
    })
}

fn max_zoom(mercator_resolution: f64) -> u8 {
    let zoom = (2.0 * WEB_MERCATOR_EXTENT / (TILE_PX as f64 * mercator_resolution))
        .log2()
        .ceil();
    zoom.clamp(0.0, f64::from(MAX_ZOOM)) as u8
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LevelScale {
    index: usize,
    resolution: f64,
}

fn level_scales(reader: &CogReader<File>, base_resolution: f64) -> Vec<LevelScale> {
    let base = &reader.levels()[0];
    let mut scales: Vec<LevelScale> = reader
        .levels()
        .iter()
        .enumerate()
        .filter(|(_, level)| level.samples == base.samples)
        .map(|(index, level)| LevelScale {
            index,
            resolution: base_resolution * base.width as f64 / level.width as f64,
        })
        .collect();
    scales.sort_by(|a, b| a.resolution.total_cmp(&b.resolution));
    scales
}

fn level_for(levels: &[LevelScale], resolution: f64) -> LevelScale {
    levels
        .iter()
        .rev()
        .find(|level| level.resolution <= resolution * (1.0 + 1e-9))
        .copied()
        .unwrap_or(levels[0])
}

struct ImageryCogSource {
    reader: Arc<CogReader<File>>,
    levels: Vec<LevelScale>,
    bands: u16,
    origin_x: f64,
    origin_y: f64,
    resolution: f64,
    crs: Crs,
}

fn read_cog_chunk(
    reader: &CogReader<File>,
    level: LevelScale,
    origin: (f64, f64),
    req: &WindowReq,
    crs: Crs,
) -> geoplumb::Result<RasterChunk> {
    let cols = (req.bbox.width() / req.resolution).round() as usize;
    let rows = (req.bbox.height() / req.resolution).round() as usize;
    let cog_level = &reader.levels()[level.index];
    let step = req.resolution / level.resolution;
    let left = (req.bbox.min_x - origin.0) / level.resolution;
    let top = (origin.1 - req.bbox.max_y) / level.resolution;
    let col_start = left.round().max(0.0) as usize;
    let row_start = top.round().max(0.0) as usize;
    let col_end = (left + cols as f64 * step).round().max(0.0) as usize;
    let row_end = (top + rows as f64 * step).round().max(0.0) as usize;
    let col_end = col_end.min(cog_level.width);
    let row_end = row_end.min(cog_level.height);

    let mut bands: Vec<Vec<f64>> = vec![vec![f64::NAN; cols * rows]; cog_level.samples];
    // geoplumb's CogSrc allocates the whole chunk at level resolution, however little the file covers
    if col_start < col_end && row_start < row_end {
        let window = reader.read_window_bands(
            level.index,
            col_start,
            row_start,
            col_end - col_start,
            row_end - row_start,
        )?;
        let span = |start: f64, index: usize| -> (i64, i64) {
            let first = (start + index as f64 * step).round() as i64;
            let last = (start + (index + 1) as f64 * step).round() as i64;
            (first, last.max(first + 1))
        };
        let window_cols = (col_end - col_start) as i64;
        let window_rows = (row_end - row_start) as i64;
        for (plane, band) in bands.iter_mut().zip(window.bands()) {
            let data = band.data();
            for row in 0..rows {
                let (first_row, last_row) = span(top, row);
                let first_row = (first_row - row_start as i64).max(0);
                let last_row = (last_row - row_start as i64).min(window_rows);
                for col in 0..cols {
                    let (first_col, last_col) = span(left, col);
                    let first_col = (first_col - col_start as i64).max(0);
                    let last_col = (last_col - col_start as i64).min(window_cols);
                    let mut sum = 0.0;
                    let mut count = 0usize;
                    for source_row in first_row..last_row {
                        for source_col in first_col..last_col {
                            let value = data[(source_row * window_cols + source_col) as usize];
                            if value.is_finite() {
                                sum += value;
                                count += 1;
                            }
                        }
                    }
                    if count > 0 {
                        plane[row * cols + col] = sum / count as f64;
                    }
                }
            }
        }
    }
    let rasters = bands
        .into_iter()
        .map(|plane| Raster::from_vec(cols, rows, plane, req.resolution, f64::NAN))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RasterChunk {
        bands: BandedRaster::new(rasters)?,
        bbox: req.bbox,
        resolution: req.resolution,
        crs,
    })
}

impl Source for ImageryCogSource {
    fn identity(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(format!("{:?}", self.reader.levels()).as_bytes());
        format!(
            "ImageryCogSource crs={} origin_x={:?} origin_y={:?} resolution={:?} layout={}",
            self.crs,
            self.origin_x,
            self.origin_y,
            self.resolution,
            to_hex(&digest.finalize())
        )
    }

    fn constraint(&self) -> Constraint {
        Constraint::Produces(CapsSet::one(CapsPattern::Raster(RasterPattern {
            dtype: SetField::one(Dtype::F64),
            bands: SetField::one(self.bands),
            crs: SetField::one(self.crs),
            resolution: ResRange::at_least(self.resolution),
            chunk_px: SetField::Any,
        })))
    }

    fn grid(&self) -> GridSpec {
        GridSpec {
            origin_x: self.origin_x,
            origin_y: self.origin_y,
            base_resolution: self.resolution,
            chunk_px: 256,
        }
    }

    fn read<'a>(&'a self, req: &'a WindowReq) -> BoxFuture<'a, geoplumb::Result<Chunk>> {
        let reader = Arc::clone(&self.reader);
        let level = level_for(&self.levels, req.resolution);
        let origin = (self.origin_x, self.origin_y);
        let crs = self.crs;
        let req = *req;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                read_cog_chunk(&reader, level, origin, &req, crs).map(Chunk::Raster)
            })
            .await
            .expect("imagery read task panicked")
        })
    }
}

fn pixels_inside(out: &WindowReq, footprint: &Bbox) -> Option<WindowReq> {
    let res = out.resolution;
    let last_col = (out.bbox.width() / res).round() - 1.0;
    let last_row = (out.bbox.height() / res).round() - 1.0;
    let first_inside_col = ((footprint.min_x - out.bbox.min_x) / res - 0.5)
        .ceil()
        .max(0.0);
    let last_inside_col = ((footprint.max_x - out.bbox.min_x) / res - 0.5)
        .floor()
        .min(last_col);
    let first_inside_row = ((out.bbox.max_y - footprint.max_y) / res - 0.5)
        .ceil()
        .max(0.0);
    let last_inside_row = ((out.bbox.max_y - footprint.min_y) / res - 0.5)
        .floor()
        .min(last_row);
    if first_inside_col > last_inside_col || first_inside_row > last_inside_row {
        return None;
    }
    let bbox = Bbox::new(
        out.bbox.min_x + first_inside_col * res,
        out.bbox.max_y - (last_inside_row + 1.0) * res,
        out.bbox.min_x + (last_inside_col + 1.0) * res,
        out.bbox.max_y - first_inside_row * res,
    );
    Some(out.with_window(bbox, res))
}

struct FootprintReproject {
    reproject: Reproject,
    footprint: Bbox,
    mercator_resolution: f64,
}

impl Transform for FootprintReproject {
    fn constraint(&self) -> Constraint {
        self.reproject.constraint()
    }

    fn identity(&self) -> String {
        format!(
            "FootprintReproject {} footprint={:?}",
            self.reproject.identity(),
            self.footprint
        )
    }

    fn configure(&mut self, input: &Caps, output: &Caps) -> geoplumb::Result<()> {
        self.reproject.configure(input, output)
    }

    fn output_grid(&self, input: &GridSpec) -> GridSpec {
        GridSpec {
            // geoplumb's Reproject measures its scale 1000 pixels from the corner, past a small image's CRS domain
            base_resolution: self.mercator_resolution,
            ..self.reproject.output_grid(input)
        }
    }

    fn plan(&self, out: &WindowReq) -> WindowReq {
        const CELLS_PER_SIDE: usize = 4;
        let inside = pixels_inside(out, &self.footprint).unwrap_or_else(|| {
            let (x, y) = (self.footprint.min_x, self.footprint.max_y);
            let corner_pixel = Bbox::new(x, y - out.resolution, x + out.resolution, y);
            out.with_window(corner_pixel, out.resolution)
        });
        let whole = self.reproject.plan(&inside);
        let cell_width = inside.bbox.width() / CELLS_PER_SIDE as f64;
        let cell_height = inside.bbox.height() / CELLS_PER_SIDE as f64;
        // geoplumb's Reproject projects eight points of a window's edge, which misses most of a ring around a pole
        let mut covered = whole.bbox;
        for row in 0..CELLS_PER_SIDE {
            for col in 0..CELLS_PER_SIDE {
                let min_x = inside.bbox.min_x + col as f64 * cell_width;
                let min_y = inside.bbox.min_y + row as f64 * cell_height;
                let cell = Bbox::new(min_x, min_y, min_x + cell_width, min_y + cell_height);
                let planned = self
                    .reproject
                    .plan(&inside.with_window(cell, inside.resolution))
                    .bbox;
                covered = Bbox::new(
                    covered.min_x.min(planned.min_x),
                    covered.min_y.min(planned.min_y),
                    covered.max_x.max(planned.max_x),
                    covered.max_y.max(planned.max_y),
                );
            }
        }
        whole.with_window(covered, whole.resolution)
    }

    fn spread(&self, dirty: &Bbox, resolution: f64) -> Bbox {
        self.reproject.spread(dirty, resolution)
    }

    fn compute(&self, out: &WindowReq, input: &Chunk) -> geoplumb::Result<Chunk> {
        let res = out.resolution;
        let cols = (out.bbox.width() / res).round() as usize;
        let rows = (out.bbox.height() / res).round() as usize;
        let band_count = input.raster()?.bands.bands().len();
        let mut planes = vec![vec![f64::NAN; cols * rows]; band_count];
        // geoplumb's Reproject fails the whole chunk when one pixel center is outside the source CRS's domain
        if let Some(inside) = pixels_inside(out, &self.footprint) {
            let warped = self.reproject.compute(&inside, input)?;
            let warped = warped.raster()?;
            let first_col = ((inside.bbox.min_x - out.bbox.min_x) / res).round() as usize;
            let first_row = ((out.bbox.max_y - inside.bbox.max_y) / res).round() as usize;
            for (plane, band) in planes.iter_mut().zip(warped.bands.bands()) {
                let width = band.width();
                for (row, values) in band.data().chunks_exact(width).enumerate() {
                    let start = (first_row + row) * cols + first_col;
                    plane[start..start + width].copy_from_slice(values);
                }
            }
        }
        let rasters = planes
            .into_iter()
            .map(|plane| Raster::from_vec(cols, rows, plane, res, f64::NAN))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Chunk::Raster(RasterChunk {
            bands: BandedRaster::new(rasters)?,
            bbox: out.bbox,
            resolution: res,
            crs: Crs::WEB_MERCATOR,
        }))
    }
}

struct ImageryLayer {
    engine: Engine,
    node: NodeId,
    manifest: ImageryManifest,
}

fn open_layer(asset_dir: &FilePath, manifest: ImageryManifest) -> geoplumb::Result<ImageryLayer> {
    let path = asset_dir.join(&manifest.cog_file);
    let file = File::open(&path)
        .map_err(|e| geoplumb::Error::Source(format!("{}: {e}", path.display())))?;
    let reader = CogReader::open(file)?;
    let levels = level_scales(&reader, manifest.resolution);
    let samples = reader.levels()[0].samples;
    let bands = u16::try_from(samples)
        .map_err(|_| geoplumb::Error::Source(format!("{samples} bands is too many")))?;
    let source = ImageryCogSource {
        reader: Arc::new(reader),
        levels,
        bands,
        origin_x: manifest.origin_x,
        origin_y: manifest.origin_y,
        resolution: manifest.resolution,
        crs: Crs(manifest.epsg),
    };
    let mut graph = Graph::new();
    let cog = graph.add_source(Box::new(source));
    let node = graph.add_transform(
        cog,
        Box::new(FootprintReproject {
            reproject: Reproject::new(Crs::WEB_MERCATOR),
            footprint: manifest.mercator_footprint(),
            mercator_resolution: manifest.mercator_resolution,
        }),
    );
    Ok(ImageryLayer {
        engine: Engine::new(graph, CHUNK_BUDGET_BYTES)?,
        node,
        manifest,
    })
}

#[derive(Default)]
pub struct ImageryLayers {
    open: Mutex<Vec<(Uuid, Arc<ImageryLayer>)>>,
}

impl ImageryLayers {
    pub fn new() -> Self {
        ImageryLayers::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<(Uuid, Arc<ImageryLayer>)>> {
        // poisoned lists still contain only whole Arcs
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn forget(&self, id: Uuid) {
        self.lock().retain(|(open_id, _)| *open_id != id);
    }

    async fn get_or_open(
        &self,
        id: Uuid,
        data_dir: &FilePath,
    ) -> Result<Option<Arc<ImageryLayer>>, String> {
        if let Some(layer) = self.find(id) {
            return Ok(Some(layer));
        }
        let Some(manifest) = read_manifest(data_dir, id).await? else {
            return Ok(None);
        };
        let asset_dir: PathBuf = data_dir.join(id.to_string());
        let layer = tokio::task::spawn_blocking(move || open_layer(&asset_dir, manifest))
            .await
            .expect("imagery layer task panicked")
            .map_err(|e| e.to_string())?;
        let layer = Arc::new(layer);
        let mut open = self.lock();
        if let Some((_, existing)) = open.iter().find(|(open_id, _)| *open_id == id) {
            return Ok(Some(Arc::clone(existing)));
        }
        if open.len() >= MAX_OPEN_LAYERS {
            open.remove(0);
        }
        open.push((id, Arc::clone(&layer)));
        Ok(Some(layer))
    }

    fn find(&self, id: Uuid) -> Option<Arc<ImageryLayer>> {
        self.lock()
            .iter()
            .find(|(open_id, _)| *open_id == id)
            .map(|(_, layer)| Arc::clone(layer))
    }
}

fn to_channel(value: f64, low: f64, high: f64) -> u8 {
    let span = (high - low).max(f64::EPSILON);
    (((value - low) / span).clamp(0.0, 1.0) * 255.0).round() as u8
}

fn encode_png(image: &RgbaImage) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, image::ImageFormat::Png)
        .expect("png encode");
    buffer.into_inner()
}

fn paint_tile(chunk: &RasterChunk, manifest: &ImageryManifest) -> Option<Vec<u8>> {
    let bands = &manifest.bands;
    let color: Vec<&[f64]> = bands
        .color_bands
        .iter()
        .map(|&band| chunk.bands.band(band).map(|raster| raster.data()))
        .collect::<Option<_>>()?;
    let alpha = match bands.alpha_band {
        Some(band) => Some(chunk.bands.band(band)?.data()),
        None => None,
    };
    let channel = |value: f64| to_channel(value, manifest.value_low, manifest.value_high);
    let side = TILE_PX as u32;
    let mut image = RgbaImage::new(side, side);
    for (index, pixel) in image.pixels_mut().enumerate() {
        // gdal nodata applies per band
        if color.iter().all(|band| band[index].is_nan()) {
            continue;
        }
        let opacity = alpha.map_or(255, |band| to_channel(band[index], 0.0, bands.opaque_alpha));
        let first = channel(color[0][index]);
        *pixel = match color.as_slice() {
            [_, green, blue] => Rgba([first, channel(green[index]), channel(blue[index]), opacity]),
            _ => Rgba([first, first, first, opacity]),
        };
    }
    Some(encode_png(&image))
}

async fn render_tile_png(layer: &ImageryLayer, tile: XyzTile) -> geoplumb::Result<Option<Vec<u8>>> {
    let tile_bbox = tile.bbox();
    let resolution = tile.resolution();
    // bilinear resampling reads one pixel past the footprint edge
    let footprint = layer.manifest.mercator_footprint().expand(resolution);
    if !tile_bbox.intersects(&footprint) {
        return Ok(Some(encode_png(&RgbaImage::new(
            TILE_PX as u32,
            TILE_PX as u32,
        ))));
    }
    let clipped = Bbox::new(
        tile_bbox.min_x.max(footprint.min_x),
        tile_bbox.min_y.max(footprint.min_y),
        tile_bbox.max_x.min(footprint.max_x),
        tile_bbox.max_y.min(footprint.max_y),
    );
    let pulled = layer
        .engine
        .pull(
            layer.node,
            WindowReq {
                bbox: clipped,
                resolution,
                time: None,
            },
        )
        .await?;
    let chunk = resample_to_grid(pulled.raster()?, &tile_bbox, TILE_PX, TILE_PX);
    Ok(paint_tile(&chunk, &layer.manifest))
}

pub(crate) fn no_imagery(id: Uuid) -> Response {
    (
        StatusCode::NOT_FOUND,
        format!("asset {id} has no imagery tiles: upload a georeferenced GeoTIFF, JPEG, PNG or JPEG 2000"),
    )
        .into_response()
}

pub(crate) fn server_error(context: &str, error: impl std::fmt::Display) -> Response {
    tracing::warn!("{context}: {error}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn imagery_tile(
    State(state): State<Arc<AppState>>,
    Path((id, z, x, y)): Path<(Uuid, u8, u32, String)>,
) -> Result<Response, Refusal> {
    let y: u32 = y
        .strip_suffix(".png")
        .and_then(|digits| digits.parse().ok())
        .ok_or(StatusCode::BAD_REQUEST.into_response())?;
    if z > MAX_ZOOM {
        return Err(StatusCode::BAD_REQUEST.into_response().into());
    }
    let side = 1u32 << z;
    if x >= side || y >= side {
        return Err(StatusCode::NOT_FOUND.into_response().into());
    }
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|e| server_error("imagery asset lookup failed", e))?;
    if asset.is_none() {
        return Err(no_imagery(id).into());
    }
    tile_png_response(&state, id, XyzTile { z, x, y }).await
}

pub(crate) async fn tile_png_response(
    state: &AppState,
    id: Uuid,
    tile: XyzTile,
) -> Result<Response, Refusal> {
    let Some(_slot) = state.analysis_engines.render_slot().await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
        )
            .into_response()
            .into());
    };
    let layer = state
        .imagery_layers
        .get_or_open(id, &state.data_dir)
        .await
        .map_err(|e| server_error("imagery layer open failed", e))?
        .ok_or_else(|| no_imagery(id))?;
    let png = render_tile_png(&layer, tile)
        .await
        .map_err(|e| server_error("imagery tile render failed", e))?
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let headers = [
        (
            header::CACHE_CONTROL,
            CachePolicy::tile().cache_control_header(),
        ),
        (header::CONTENT_TYPE, "image/png".to_string()),
    ];
    Ok((headers, png).into_response())
}

async fn imagery_tilejson(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, Refusal> {
    let asset = state
        .db
        .get_asset(id)
        .await
        .map_err(|e| server_error("imagery asset lookup failed", e))?
        .ok_or_else(|| no_imagery(id))?;
    let manifest = read_manifest(&state.data_dir, id)
        .await
        .map_err(|e| server_error("imagery manifest read failed", e))?
        .ok_or_else(|| no_imagery(id))?;
    let tilejson = serde_json::json!({
        "tilejson": "3.0.0",
        "name": asset.name,
        "scheme": "xyz",
        "format": "png",
        "tiles": [tile_template(id)],
        "minzoom": 0,
        "maxzoom": manifest.max_zoom,
        "bounds": manifest.bounds_degrees,
    });
    let cache_control = [(
        header::CACHE_CONTROL,
        CachePolicy::metadata().cache_control_header(),
    )];
    Ok((cache_control, Json(tilejson)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gdal_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/imagery_rgb_gdal_cog.tif")
    }

    #[test]
    fn world_files_name_the_center_of_the_top_left_pixel() {
        let grid = parse_world_file("0.5\n0\n0\n-0.25\n10.25\n50.125\n").unwrap();
        assert_eq!(
            grid,
            PixelGrid {
                origin_x: 10.0,
                origin_y: 50.25,
                pixel_width: 0.5,
                pixel_height: 0.25,
            }
        );
    }

    #[test]
    fn a_rotated_world_file_is_refused() {
        let error = parse_world_file("1\n0.1\n0\n-1\n0\n0").unwrap_err();
        assert!(error.contains("rotated"), "{error}");
    }

    #[test]
    fn a_short_world_file_is_refused() {
        let error = parse_world_file("1\n0\n0\n-1").unwrap_err();
        assert!(error.contains("6 lines"), "{error}");
    }

    #[test]
    fn a_south_up_world_file_is_refused() {
        let error = parse_world_file("1\n0\n0\n1\n0\n0").unwrap_err();
        assert!(error.contains("negative y"), "{error}");
    }

    #[test]
    fn bounds_must_cover_ground() {
        for raw in ["1,2,3", "3,2,1,4", "1,4,3,2", "a,b,c,d", "1,2,inf,4"] {
            assert!(parse_bounds(raw).is_err(), "{raw}");
        }
        assert_eq!(parse_bounds("1, 2, 3, 4"), Ok([1.0, 2.0, 3.0, 4.0]));
    }

    #[test]
    fn a_world_file_without_a_crs_is_refused() {
        let form = ImageryForm {
            world_file: Some("1\n0\n0\n-1\n0\n0".into()),
            ..ImageryForm::default()
        };
        let error = form_georeference(&form, 4, 4).unwrap_err();
        assert!(error.contains("crs"), "{error}");
    }

    #[test]
    fn user_defined_geo_keys_have_no_epsg_code() {
        let projected_user_defined = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, USER_DEFINED_GEO_KEY];
        assert_eq!(geo_keys_epsg(&projected_user_defined), None);
        let utm = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 32632];
        assert_eq!(geo_keys_epsg(&utm), Some(32632));
        let geographic = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
        assert_eq!(geo_keys_epsg(&geographic), Some(4326));
    }

    #[test]
    fn four_bands_are_alpha_only_when_the_file_says_so() {
        let declared = band_layout(4, true, SampleFormat::U8);
        assert_eq!(declared.alpha_band, Some(3));
        assert_eq!(declared.opaque_alpha, 255.0);
        assert_eq!(band_layout(4, false, SampleFormat::U8).alpha_band, None);
        assert_eq!(band_layout(2, true, SampleFormat::U16).alpha_band, Some(1));
        assert_eq!(
            band_layout(1, false, SampleFormat::F32).color_bands,
            vec![0]
        );
    }

    #[test]
    fn decoding_is_limited_by_side_and_decoded_bytes_not_by_memory() {
        assert!(check_decoded_size(8192, 8192, 4).is_ok());
        assert!(check_decoded_size(23_170, 23_170, 4).is_ok());
        let error = check_decoded_size(23_171, 23_171, 4).unwrap_err();
        assert!(error.contains("2147483648 decoded bytes"), "{error}");
        assert!(check_decoded_size(MAX_SIDE, 1, 4).is_ok());
        assert!(check_decoded_size(MAX_SIDE + 1, 1, 4).is_err());
        assert!(check_decoded_size(usize::MAX, usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn pixels_past_half_a_pixel_of_drift_are_not_square() {
        let grid = PixelGrid {
            origin_x: 0.0,
            origin_y: 0.0,
            pixel_width: 1.0,
            pixel_height: 1.001,
        };
        assert!(grid.has_square_pixels(100, 100));
        assert!(!grid.has_square_pixels(1000, 100));
    }

    #[test]
    fn non_square_pixels_resample_onto_the_finer_axis() {
        let asset_dir = tempfile::tempdir().unwrap();
        let scratch =
            ScratchRaster::create(asset_dir.path(), DECODED_SCRATCH_FILE, 2, 2, 1).unwrap();
        scratch.write_pixels(0, 0, &[1, 2, 3, 4]).unwrap();
        let decoded = DecodedRaster {
            scratch,
            samples: 1,
            format: SampleFormat::U8,
            nodata: None,
        };
        let grid = PixelGrid {
            origin_x: 0.0,
            origin_y: 0.0,
            pixel_width: 2.0,
            pixel_height: 1.0,
        };
        let (square, square_grid) = onto_square_pixels(asset_dir.path(), decoded, grid).unwrap();
        assert_eq!((square.scratch.cols, square.scratch.rows), (4, 2));
        assert_eq!(square_grid.pixel_width, 1.0);
        assert_eq!(square_grid.pixel_height, 1.0);
        let mut pixels = [0; 8];
        square.scratch.read_pixels(0, 0, &mut pixels).unwrap();
        assert_eq!(pixels, [1, 1, 2, 2, 3, 3, 4, 4]);
        assert!(
            !asset_dir
                .path()
                .join("imagery")
                .join(DECODED_SCRATCH_FILE)
                .exists()
        );
    }

    #[test]
    fn overviews_round_the_average_of_the_data_and_leave_nodata_blocks_nodata() {
        let asset_dir = tempfile::tempdir().unwrap();
        let finer = ScratchRaster::create(asset_dir.path(), DECODED_SCRATCH_FILE, 3, 3, 1).unwrap();
        finer
            .write_pixels(0, 0, &[10, 0, 7, 21, 0, 0, 5, 6, 0])
            .unwrap();
        let coarser =
            overview_level(asset_dir.path(), &finer, 1, SampleFormat::U8, Some(0.0)).unwrap();
        assert_eq!((coarser.cols, coarser.rows), (2, 2));
        let mut pixels = [0; 4];
        coarser.read_pixels(0, 0, &mut pixels).unwrap();
        assert_eq!(pixels, [16, 7, 6, 0]);
    }

    #[test]
    fn float_overviews_skip_nan_and_keep_every_band() {
        let asset_dir = tempfile::tempdir().unwrap();
        let mut samples = Vec::new();
        for value in [1.0, 100.0, f64::NAN, 200.0, 3.0, f64::NAN, f64::NAN, 600.0] {
            SampleFormat::F32.encode(value, &mut samples);
        }
        let finer = ScratchRaster::create(asset_dir.path(), DECODED_SCRATCH_FILE, 2, 2, 8).unwrap();
        finer.write_pixels(0, 0, &samples).unwrap();
        let coarser = overview_level(
            asset_dir.path(),
            &finer,
            1,
            SampleFormat::F32,
            Some(f64::NAN),
        )
        .unwrap();
        let mut pixel = [0; 8];
        coarser.read_pixels(0, 0, &mut pixel).unwrap();
        assert_eq!(SampleFormat::F32.decode(&pixel[..4]), 2.0);
        assert_eq!(SampleFormat::F32.decode(&pixel[4..]), 300.0);
    }

    #[test]
    fn gdal_overviews_are_scaled_by_width_not_by_their_missing_tags() {
        let file = File::open(gdal_fixture()).unwrap();
        let reader = CogReader::open(file).unwrap();
        let levels = level_scales(&reader, 0.25);
        assert_eq!(levels[0].resolution, 0.25);
        assert_eq!(levels[1].resolution, 0.5);
        assert_eq!(level_for(&levels, 0.25).index, 0);
        assert_eq!(level_for(&levels, 0.6).index, 1);
        assert_eq!(level_for(&levels, 1000.0).index, 1);
        assert_eq!(level_for(&levels, 0.1).index, 0);
    }

    #[test]
    fn channels_stretch_low_to_high_and_clamp() {
        assert_eq!(to_channel(0.0, 0.0, 255.0), 0);
        assert_eq!(to_channel(128.0, 0.0, 255.0), 128);
        assert_eq!(to_channel(300.0, 100.0, 200.0), 255);
        assert_eq!(to_channel(5.0, 5.0, 5.0), 0);
    }
}
