mod common;

use std::io::Cursor;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, token};
use terrano_core::{BandedRaster, CogParams, Raster, SampleFormat, write_cog_bands};
use tiff::encoder::colortype::{ColorType, Gray8, Gray16, RGB8, RGBA8};
use tiff::encoder::compression::Lzw;
use tiff::encoder::{TiffEncoder, TiffValue};
use tiff::tags::Tag;
use tiletopia_server::{AppState, router};
use tower::ServiceExt;

const GDAL_COG: &[u8] = include_bytes!("fixtures/imagery_rgb_gdal_cog.tif");
const GDAL_MASKED_COG: &[u8] = include_bytes!("fixtures/imagery_gray_mask_gdal_cog.tif");

const RED: [u8; 3] = [200, 10, 10];
const GREEN: [u8; 3] = [10, 200, 10];
const BLUE: [u8; 3] = [10, 10, 200];
const WHITE: [u8; 3] = [240, 240, 240];

const GEO_KEYS_4326: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
const GEO_KEYS_32632: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 32632];
const GEO_KEYS_3857: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 3857];

type Part<'a> = (&'a str, Option<&'a str>, Vec<u8>);

fn editor() -> String {
    token("imagery-owner", "editor")
}

async fn upload(state: &Arc<AppState>, parts: &[Part<'_>]) -> (StatusCode, Vec<u8>) {
    let boundary = "tiletopiaimageryboundary";
    let mut body: Vec<u8> = Vec::new();
    for (field, filename, value) in parts {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"").as_bytes(),
        );
        if let Some(filename) = filename {
            body.extend_from_slice(format!("; filename=\"{filename}\"").as_bytes());
        }
        body.extend_from_slice(b"\r\n\r\n");
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/assets")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::from(body))
        .unwrap();
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn upload_created(state: &Arc<AppState>, parts: &[Part<'_>]) -> serde_json::Value {
    let (status, bytes) = upload(state, parts).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

async fn upload_refused(state: &Arc<AppState>, parts: &[Part<'_>]) -> String {
    let (status, bytes) = upload(state, parts).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    String::from_utf8(bytes).unwrap()
}

async fn fetch(state: &Arc<AppState>, uri: &str) -> (StatusCode, Vec<u8>) {
    // map libraries send no Authorization header
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn fetch_as_owner(state: &Arc<AppState>, uri: &str) -> (StatusCode, Vec<u8>) {
    let request = Request::builder()
        .uri(uri)
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

fn tile_and_pixel_holding_point(lon: f64, lat: f64, z: u8) -> (u32, u32, u32, u32) {
    let world = f64::from(1u32 << z) * 256.0;
    let x = (lon + 180.0) / 360.0 * world;
    let y = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * world;
    (
        (x / 256.0) as u32,
        (y / 256.0) as u32,
        (x % 256.0) as u32,
        (y % 256.0) as u32,
    )
}

async fn pixel_at(state: &Arc<AppState>, id: &str, lon: f64, lat: f64, z: u8) -> [u8; 4] {
    let (x, y, col, row) = tile_and_pixel_holding_point(lon, lat, z);
    let (status, png) = fetch(
        state,
        &format!("/api/v1/assets/{id}/imagery/{z}/{x}/{y}.png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tile = image::load_from_memory(&png).unwrap().to_rgba8();
    assert_eq!(tile.dimensions(), (256, 256));
    tile.get_pixel(col, row).0
}

fn opaque([r, g, b]: [u8; 3]) -> [u8; 4] {
    [r, g, b, 255]
}

fn asset_id(asset: &serde_json::Value) -> String {
    asset["id"].as_str().unwrap().to_string()
}

fn quadrants<const N: usize>(side: usize, colors_nw_ne_sw_se: [[u8; N]; 4]) -> Vec<u8> {
    let mut data = Vec::with_capacity(side * side * N);
    for row in 0..side {
        for col in 0..side {
            let quadrant = usize::from(row >= side / 2) * 2 + usize::from(col >= side / 2);
            data.extend_from_slice(&colors_nw_ne_sw_se[quadrant]);
        }
    }
    data
}

struct TiffPlacement {
    origin_x: f64,
    origin_y: f64,
    pixel_size: f64,
    geo_keys: &'static [u16],
}

fn stripped_lzw_tiff<C: ColorType>(
    side: u32,
    data: &[C::Inner],
    placement: Option<&TiffPlacement>,
    extra_samples: Option<u16>,
    nodata: Option<&str>,
) -> Vec<u8>
where
    [C::Inner]: TiffValue,
{
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = TiffEncoder::new(&mut buffer).unwrap();
    let mut image = encoder
        .new_image_with_compression::<C, _>(side, side, Lzw)
        .unwrap();
    if let Some(placement) = placement {
        let scale = [placement.pixel_size, placement.pixel_size, 0.0];
        let tiepoint = [0.0, 0.0, 0.0, placement.origin_x, placement.origin_y, 0.0];
        let directory = image.encoder();
        directory
            .write_tag(Tag::ModelPixelScaleTag, &scale[..])
            .unwrap();
        directory
            .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
            .unwrap();
        directory
            .write_tag(Tag::GeoKeyDirectoryTag, placement.geo_keys)
            .unwrap();
    }
    if let Some(extra_samples) = extra_samples {
        image
            .encoder()
            .write_tag(Tag::ExtraSamples, extra_samples)
            .unwrap();
    }
    if let Some(nodata) = nodata {
        image.encoder().write_tag(Tag::GdalNodata, nodata).unwrap();
    }
    image.write_data(data).unwrap();
    buffer.into_inner()
}

const ALPS: TiffPlacement = TiffPlacement {
    origin_x: 7.0,
    origin_y: 47.0,
    pixel_size: 0.01,
    geo_keys: &GEO_KEYS_4326,
};

#[tokio::test]
async fn a_gdal_cog_upload_serves_correct_pixels_to_an_anonymous_client() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    assert_eq!(asset["asset_type"], "imagery");
    assert_eq!(asset["status"], "ready");
    let id = asset_id(&asset);

    // the image spans 7..7.512 east and 46.488..47 north, split into quadrants
    for z in [9, 12, 15] {
        assert_eq!(
            pixel_at(&state, &id, 7.1, 46.9, z).await,
            opaque(RED),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 7.4, 46.9, z).await,
            opaque(GREEN),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 7.1, 46.6, z).await,
            opaque(BLUE),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 7.4, 46.6, z).await,
            opaque(WHITE),
            "z{z}"
        );
    }
    // zoom 6 is past the file's overviews
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 6).await, opaque(RED));

    assert!(!state.data_dir.join(&id).join("imagery/source.tif").exists());
}

#[tokio::test]
async fn a_cog_with_an_internal_mask_is_served_in_place() {
    let state = test_state().await;
    let asset = upload_created(
        &state,
        &[
            ("file", Some("pan.tif"), GDAL_MASKED_COG.to_vec()),
            ("asset_type", None, b"imagery".to_vec()),
        ],
    )
    .await;
    assert_eq!(asset["asset_type"], "imagery");
    let id = asset_id(&asset);
    assert!(!state.data_dir.join(&id).join("imagery/source.tif").exists());
    for z in [9, 12] {
        assert_eq!(
            pixel_at(&state, &id, 7.1, 46.9, z).await,
            [200, 200, 200, 255],
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 7.4, 46.9, z).await,
            [10, 10, 10, 255],
            "z{z}"
        );
    }
}

fn stripped_lzw_rgb_tiff_with_internal_mask(
    side: u32,
    data: &[u8],
    placement: &TiffPlacement,
) -> Vec<u8> {
    const NEW_SUBFILE_TYPE_MASK: u32 = 4;
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = TiffEncoder::new(&mut buffer).unwrap();
    let mut image = encoder
        .new_image_with_compression::<RGB8, _>(side, side, Lzw)
        .unwrap();
    let scale = [placement.pixel_size, placement.pixel_size, 0.0];
    let tiepoint = [0.0, 0.0, 0.0, placement.origin_x, placement.origin_y, 0.0];
    let directory = image.encoder();
    directory
        .write_tag(Tag::ModelPixelScaleTag, &scale[..])
        .unwrap();
    directory
        .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
        .unwrap();
    directory
        .write_tag(Tag::GeoKeyDirectoryTag, placement.geo_keys)
        .unwrap();
    image.write_data(data).unwrap();

    let mask = vec![0u8; (side * side) as usize];
    let mut mask_image = encoder
        .new_image_with_compression::<Gray8, _>(side, side, Lzw)
        .unwrap();
    mask_image
        .encoder()
        .write_tag(Tag::NewSubfileType, NEW_SUBFILE_TYPE_MASK)
        .unwrap();
    mask_image.write_data(&mask).unwrap();
    buffer.into_inner()
}

#[tokio::test]
async fn a_stripped_lzw_tiff_with_an_internal_mask_is_refused_naming_the_mask() {
    let state = test_state().await;
    let data = quadrants(16, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_rgb_tiff_with_internal_mask(16, &data, &ALPS);

    let reason = upload_refused(&state, &[("file", Some("masked.tif"), file)]).await;

    assert!(reason.contains("transparency mask"), "{reason}");
    assert!(state.db.list_assets().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_image_of_the_whole_web_mercator_square_is_served() {
    let state = test_state().await;
    let extent = 20_037_508.342_789_244;
    let placement = TiffPlacement {
        origin_x: -extent,
        origin_y: extent,
        pixel_size: 2.0 * extent / 80.0,
        geo_keys: &GEO_KEYS_3857,
    };
    let data = quadrants(80, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_tiff::<RGB8>(80, &data, Some(&placement), None, None);
    let asset = upload_created(&state, &[("file", Some("world.tif"), file)]).await;
    let id = asset_id(&asset);
    for z in [0, 2] {
        assert_eq!(
            pixel_at(&state, &id, -90.0, 45.0, z).await,
            opaque(RED),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 90.0, -45.0, z).await,
            opaque(WHITE),
            "z{z}"
        );
    }
}

#[tokio::test]
async fn projected_images_land_where_gdal_puts_them_and_answer_every_zoomed_out_tile() {
    let state = test_state().await;
    let png = picture_bytes(image::ImageFormat::Png);
    // gdaltransform gave each longitude and latitude, the center of the red north west quadrant
    for (crs, bounds, lon, lat) in [
        (
            "EPSG:3035",
            "4300000,3200000,4340000,3240000",
            9.839188,
            52.179636,
        ),
        (
            "EPSG:27700",
            "400000,300000,440000,340000",
            -1.852891,
            52.867406,
        ),
        (
            "EPSG:2154",
            "700000,6600000,740000,6640000",
            3.131045,
            46.770053,
        ),
        (
            "EPSG:3413",
            "900000,-600000,1100000,-400000",
            19.653824,
            80.318342,
        ),
        (
            "EPSG:3031",
            "900000,900000,1100000,1100000",
            42.137595,
            -77.021340,
        ),
    ] {
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, bounds.as_bytes().to_vec()),
                ("crs", None, crs.as_bytes().to_vec()),
                ("file", Some("map.png"), png.clone()),
            ],
        )
        .await;
        let id = asset_id(&asset);
        assert_every_tile_answers(&state, &id, 3, crs).await;
        assert_eq!(
            pixel_at(&state, &id, lon, lat, 9).await,
            opaque(RED),
            "{crs}"
        );
    }
}

#[tokio::test]
async fn images_filling_their_projection_answer_every_zoomed_out_tile() {
    let state = test_state().await;
    let png = picture_bytes(image::ImageFormat::Png);
    for (crs, bounds) in [
        ("EPSG:27700", "0,0,700000,700000"),
        ("EPSG:3035", "2000000,1000000,6000000,5000000"),
        ("EPSG:32632", "166000,4000000,834000,4668000"),
        ("EPSG:3413", "-4000000,-4000000,4000000,4000000"),
    ] {
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, bounds.as_bytes().to_vec()),
                ("crs", None, crs.as_bytes().to_vec()),
                ("file", Some("map.png"), png.clone()),
            ],
        )
        .await;
        assert_every_tile_answers(&state, &asset_id(&asset), 4, crs).await;
    }
}

#[tokio::test]
async fn an_image_holding_the_pole_is_drawn_around_every_longitude() {
    let state = test_state().await;
    let png = picture_bytes(image::ImageFormat::Png);
    // 8000 km across at 125 km a pixel
    let asset = upload_created(
        &state,
        &[
            (
                "bounds",
                None,
                b"-4000000,-4000000,4000000,4000000".to_vec(),
            ),
            ("crs", None, b"EPSG:3413".to_vec()),
            ("file", Some("arctic.png"), png),
        ],
    )
    .await;
    let id = asset_id(&asset);
    let (_, body) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/tilejson.json"),
    )
    .await;
    let tilejson: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tilejson["bounds"][0], -180.0, "{tilejson}");
    assert_eq!(tilejson["bounds"][2], 180.0, "{tilejson}");
    assert_eq!(tilejson["bounds"][3], 90.0, "{tilejson}");
    // the corners of the image, which do not reach the far pole
    let south = tilejson["bounds"][1].as_f64().unwrap();
    assert!((south - 40.9).abs() < 0.1, "{tilejson}");
    // finest pixel is 137 km at the lowest latitude, not the 626 km of mercator width over columns
    assert_eq!(tilejson["maxzoom"], 1, "{tilejson}");

    // gdaltransform gave each longitude and latitude, the center of one quadrant
    for (lon, color) in [(-179.9, RED), (90.0, GREEN), (-90.0, BLUE), (0.0, WHITE)] {
        assert_eq!(
            pixel_at(&state, &id, lon, 64.311742, 1).await,
            opaque(color),
            "{lon}"
        );
    }
}

#[tokio::test]
async fn an_image_across_the_antimeridian_is_refused_by_name() {
    let state = test_state().await;
    for (crs, bounds) in [
        ("EPSG:32601", "100000,0,300000,200000"),
        ("EPSG:2193", "2100000,5400000,2500000,5800000"),
    ] {
        let reason = upload_refused(
            &state,
            &[
                ("bounds", None, bounds.as_bytes().to_vec()),
                ("crs", None, crs.as_bytes().to_vec()),
                (
                    "file",
                    Some("map.png"),
                    picture_bytes(image::ImageFormat::Png),
                ),
            ],
        )
        .await;
        assert!(reason.contains("antimeridian"), "{crs}: {reason}");
    }
}

async fn assert_every_tile_answers(state: &Arc<AppState>, id: &str, max_zoom: u32, label: &str) {
    for z in 0..=max_zoom {
        for x in 0..1u32 << z {
            for y in 0..1u32 << z {
                let uri = format!("/api/v1/assets/{id}/imagery/{z}/{x}/{y}.png");
                let (status, _) = fetch(state, &uri).await;
                assert_eq!(status, StatusCode::OK, "{label} {z}/{x}/{y}");
            }
        }
    }
}

#[tokio::test]
async fn an_image_on_the_pole_is_past_the_mercator_square_and_answers_transparent() {
    let state = test_state().await;
    let asset = upload_created(
        &state,
        &[
            ("bounds", None, b"-100000,-100000,100000,100000".to_vec()),
            ("crs", None, b"EPSG:3413".to_vec()),
            (
                "file",
                Some("pole.png"),
                picture_bytes(image::ImageFormat::Png),
            ),
        ],
    )
    .await;
    let id = asset_id(&asset);
    assert_every_tile_answers(&state, &id, 3, "pole").await;
    assert_eq!(pixel_at(&state, &id, 0.0, 85.0, 3).await[3], 0);
}

#[tokio::test]
async fn tiles_past_the_image_are_transparent_and_cheap() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, -120.0, 40.0, 10).await, [0, 0, 0, 0]);
    // beside the image inside the same tile
    assert_eq!(pixel_at(&state, &id, 6.95, 46.9, 9).await, [0, 0, 0, 0]);
    assert_eq!(pixel_at(&state, &id, 7.1, 47.02, 12).await, [0, 0, 0, 0]);
}

#[tokio::test]
async fn tile_addresses_are_checked() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    for (uri, expected) in [
        (
            format!("/api/v1/assets/{id}/imagery/23/0/0.png"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("/api/v1/assets/{id}/imagery/1/2/0.png"),
            StatusCode::NOT_FOUND,
        ),
        (
            format!("/api/v1/assets/{id}/imagery/1/0/0.jpg"),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("/api/v1/assets/{id}/imagery/1/0/abc.png"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(fetch(&state, &uri).await.0, expected, "{uri}");
    }
}

#[tokio::test]
async fn a_utm_tiff_reprojects_onto_web_mercator() {
    let state = test_state().await;
    // 40 km square split at easting 500000, the 9 degree meridian, and northing 4980000
    let placement = TiffPlacement {
        origin_x: 480_000.0,
        origin_y: 5_000_000.0,
        pixel_size: 500.0,
        geo_keys: &GEO_KEYS_32632,
    };
    let data = quadrants(80, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_tiff::<RGB8>(80, &data, Some(&placement), None, None);
    let asset = upload_created(&state, &[("file", Some("utm.tif"), file)]).await;
    assert_eq!(asset["asset_type"], "imagery");
    let id = asset_id(&asset);

    assert!(state.data_dir.join(&id).join("imagery/source.tif").exists());
    for z in [8, 11] {
        assert_eq!(
            pixel_at(&state, &id, 8.9, 45.1, z).await,
            opaque(RED),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 9.1, 45.1, z).await,
            opaque(GREEN),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 8.9, 44.85, z).await,
            opaque(BLUE),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 9.1, 44.85, z).await,
            opaque(WHITE),
            "z{z}"
        );
    }
    // whole world tiles span past the UTM zone's domain, where the image must not repeat
    for z in 0..=5 {
        assert_eq!(
            pixel_at(&state, &id, -171.0, -45.0, z).await,
            [0, 0, 0, 0],
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 100.0, 45.0, z).await,
            [0, 0, 0, 0],
            "z{z}"
        );
    }
    assert_eq!(pixel_at(&state, &id, 8.9, 45.1, 6).await[3], 255);
    assert_eq!(pixel_at(&state, &id, 9.1, 44.85, 5).await[3], 255);
}

#[tokio::test]
async fn a_web_mercator_tiff_lands_where_its_coordinates_say() {
    let state = test_state().await;
    // 40 km square of mercator metres split at x 820000 and y 5880000
    let placement = TiffPlacement {
        origin_x: 800_000.0,
        origin_y: 5_900_000.0,
        pixel_size: 500.0,
        geo_keys: &GEO_KEYS_3857,
    };
    let data = quadrants(80, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_tiff::<RGB8>(80, &data, Some(&placement), None, None);
    let asset = upload_created(&state, &[("file", Some("merc.tif"), file)]).await;
    assert_eq!(asset["asset_type"], "imagery");
    let id = asset_id(&asset);

    let degrees = |x: f64, y: f64| {
        let radius = 6_378_137.0;
        (
            (x / radius).to_degrees(),
            (y / radius).sinh().atan().to_degrees(),
        )
    };
    for ((x, y), color) in [
        ((810_000.0, 5_890_000.0), RED),
        ((830_000.0, 5_890_000.0), GREEN),
        ((810_000.0, 5_870_000.0), BLUE),
        ((830_000.0, 5_870_000.0), WHITE),
    ] {
        let (lon, lat) = degrees(x, y);
        assert_eq!(pixel_at(&state, &id, lon, lat, 11).await, opaque(color));
    }
    let (lon, lat) = degrees(790_000.0, 5_890_000.0);
    assert_eq!(pixel_at(&state, &id, lon, lat, 11).await[3], 0);
}

#[tokio::test]
async fn a_utm_image_paints_nothing_outside_its_footprint_on_wide_tiles() {
    let state = test_state().await;
    let placement = TiffPlacement {
        origin_x: 480_000.0,
        origin_y: 5_000_000.0,
        pixel_size: 500.0,
        geo_keys: &GEO_KEYS_32632,
    };
    let data = quadrants(80, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_tiff::<RGB8>(80, &data, Some(&placement), None, None);
    let asset = upload_created(&state, &[("file", Some("utm.tif"), file)]).await;
    let id = asset_id(&asset);

    for z in 0..=3u8 {
        let side = 1u32 << z;
        let world_pixels = f64::from(side) * 256.0;
        let pixel_degrees = 360.0 / world_pixels;
        for x in 0..side {
            for y in 0..side {
                let (status, png) = fetch(
                    &state,
                    &format!("/api/v1/assets/{id}/imagery/{z}/{x}/{y}.png"),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{z}/{x}/{y}");
                let tile = image::load_from_memory(&png).unwrap().to_rgba8();
                for (col, row, pixel) in tile.enumerate_pixels() {
                    if pixel.0[3] == 0 {
                        continue;
                    }
                    let lon = (f64::from(x) * 256.0 + f64::from(col) + 0.5) / world_pixels * 360.0
                        - 180.0;
                    let mercator =
                        1.0 - 2.0 * (f64::from(y) * 256.0 + f64::from(row) + 0.5) / world_pixels;
                    let lat = (mercator * std::f64::consts::PI).sinh().atan().to_degrees();
                    // the image spans 8.74..9.26 east and 44.78..45.16 north
                    let margin = 2.0 * pixel_degrees;
                    assert!(
                        (8.74 - margin..=9.26 + margin).contains(&lon)
                            && (44.78 - margin..=45.16 + margin).contains(&lat),
                        "z{z} tile {x}/{y} paints {lon},{lat}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn an_image_of_the_whole_world_fills_the_web_mercator_square() {
    let state = test_state().await;
    let asset = upload_created(
        &state,
        &[
            ("bounds", None, b"-180,-90,180,90".to_vec()),
            ("crs", None, b"EPSG:4326".to_vec()),
            (
                "file",
                Some("world.png"),
                picture_bytes(image::ImageFormat::Png),
            ),
        ],
    )
    .await;
    let id = asset_id(&asset);
    for z in [0, 2] {
        assert_eq!(
            pixel_at(&state, &id, -90.0, 45.0, z).await,
            opaque(RED),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 90.0, 80.0, z).await,
            opaque(GREEN),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, -90.0, -80.0, z).await,
            opaque(BLUE),
            "z{z}"
        );
        assert_eq!(
            pixel_at(&state, &id, 90.0, -45.0, z).await,
            opaque(WHITE),
            "z{z}"
        );
    }
}

#[tokio::test]
async fn an_alpha_band_is_carried_through() {
    let state = test_state().await;
    let data = quadrants(
        60,
        [
            [200, 10, 10, 255],
            [10, 200, 10, 0],
            [10, 10, 200, 128],
            [240, 240, 240, 255],
        ],
    );
    let file = stripped_lzw_tiff::<RGBA8>(60, &data, Some(&ALPS), Some(2), None);
    let asset = upload_created(&state, &[("file", Some("rgba.tif"), file)]).await;
    assert_eq!(asset["asset_type"], "imagery");
    let id = asset_id(&asset);
    // quadrants split at 7.3 east and 46.7 north
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, opaque(RED));
    assert_eq!(pixel_at(&state, &id, 7.5, 46.9, 12).await[3], 0);
    assert_eq!(
        pixel_at(&state, &id, 7.1, 46.5, 12).await,
        [10, 10, 200, 128]
    );
}

#[tokio::test]
async fn a_fourth_band_with_no_alpha_tag_is_ignored() {
    let state = test_state().await;
    let data = quadrants(
        60,
        [
            [200, 10, 10, 0],
            [10, 200, 10, 0],
            [10, 10, 200, 0],
            [240, 240, 240, 0],
        ],
    );
    let file = stripped_lzw_tiff::<RGBA8>(60, &data, Some(&ALPS), None, None);
    let asset = upload_created(&state, &[("file", Some("rgbn.tif"), file)]).await;
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, opaque(RED));
}

fn gray16_quadrants(values: [u16; 4]) -> Vec<u16> {
    let mut data = Vec::with_capacity(3600);
    for row in 0..60 {
        for col in 0..60 {
            data.push(values[usize::from(row >= 30) * 2 + usize::from(col >= 30)]);
        }
    }
    data
}

#[tokio::test]
async fn single_band_samples_stretch_and_nodata_is_transparent() {
    let state = test_state().await;
    let data = gray16_quadrants([100, 1100, 600, 0]);
    let file = stripped_lzw_tiff::<Gray16>(60, &data, Some(&ALPS), None, Some("0"));
    let asset = upload_created(
        &state,
        &[
            ("file", Some("pan.tif"), file),
            ("asset_type", None, b"imagery".to_vec()),
        ],
    )
    .await;
    assert_eq!(asset["asset_type"], "imagery");
    assert_eq!(asset["status"], "ready");
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, [0, 0, 0, 255]);
    assert_eq!(
        pixel_at(&state, &id, 7.5, 46.9, 12).await,
        [255, 255, 255, 255]
    );
    assert_eq!(
        pixel_at(&state, &id, 7.1, 46.5, 12).await,
        [128, 128, 128, 255]
    );
    assert_eq!(pixel_at(&state, &id, 7.5, 46.5, 12).await[3], 0);
}

#[tokio::test]
async fn one_band_of_elevation_is_terrain_with_no_imagery_tiles() {
    let state = test_state().await;
    let data = gray16_quadrants([100, 1100, 600, 0]);
    let file = stripped_lzw_tiff::<Gray16>(60, &data, Some(&ALPS), None, Some("0"));
    let asset = upload_created(&state, &[("file", Some("dem.tif"), file.clone())]).await;
    assert_eq!(asset["asset_type"], "terrain");
    assert!(asset["job_id"].as_str().is_some(), "{asset}");
    let id = asset_id(&asset);
    let (status, _) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/tilejson.json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for asset_type in [Some("terrain"), None] {
        let mut parts = vec![
            ("bounds", None, b"7,46,8,47".to_vec()),
            ("crs", None, b"4326".to_vec()),
            ("file", Some("dem.tif"), file.clone()),
        ];
        if let Some(asset_type) = asset_type {
            parts.push(("asset_type", None, asset_type.as_bytes().to_vec()));
        }
        let reason = upload_refused(&state, &parts).await;
        assert!(reason.contains("apply to imagery"), "{reason}");
    }
}

#[tokio::test]
async fn one_band_of_8_bit_samples_is_terrain_unless_asset_type_is_imagery() {
    let state = test_state().await;
    let data = quadrants(60, [[10], [200], [100], [50]]);
    let file = stripped_lzw_tiff::<Gray8>(60, &data, Some(&ALPS), None, None);
    let asset = upload_created(&state, &[("file", Some("pan.tif"), file.clone())]).await;
    assert_eq!(asset["asset_type"], "terrain");
    assert!(asset["job_id"].as_str().is_some(), "{asset}");

    let asset = upload_created(
        &state,
        &[
            ("file", Some("pan.tif"), file),
            ("asset_type", None, b"imagery".to_vec()),
        ],
    )
    .await;
    assert_eq!(asset["asset_type"], "imagery");
    assert_eq!(asset["status"], "ready");
}

#[tokio::test]
async fn a_float_cog_with_nan_nodata_is_served_as_is() {
    let state = test_state().await;
    let (cols, rows) = (512usize, 512usize);
    let band: Vec<f64> = (0..cols * rows)
        .map(|index| {
            let (row, col) = (index / cols, index % cols);
            match (row >= 256, col >= 256) {
                (false, false) => 10.0,
                (false, true) => 20.0,
                (true, false) => f64::NAN,
                (true, true) => 30.0,
            }
        })
        .collect();
    let raster = Raster::from_vec(cols, rows, band, 0.001, f64::NAN).unwrap();
    let params = CogParams {
        tile_width: 128,
        tile_height: 128,
        overview_levels: 2,
        epsg: 4326,
        origin_x: 7.0,
        origin_y: 47.0,
        pixel_width: 0.001,
        pixel_height: 0.001,
        deflate: true,
        nodata: Some(f64::NAN),
        format: SampleFormat::F32,
    };
    let mut file = Cursor::new(Vec::new());
    write_cog_bands(
        &BandedRaster::new(vec![raster]).unwrap(),
        &params,
        &mut file,
    )
    .unwrap();
    let asset = upload_created(
        &state,
        &[
            ("file", Some("float.tif"), file.into_inner()),
            ("asset_type", None, b"imagery".to_vec()),
        ],
    )
    .await;
    let id = asset_id(&asset);
    assert!(!state.data_dir.join(&id).join("imagery/source.tif").exists());
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, [0, 0, 0, 255]);
    assert_eq!(
        pixel_at(&state, &id, 7.4, 46.6, 12).await,
        [255, 255, 255, 255]
    );
    assert_eq!(pixel_at(&state, &id, 7.1, 46.6, 12).await[3], 0);
}

fn picture_bytes(format: image::ImageFormat) -> Vec<u8> {
    let data = quadrants(64, [RED, GREEN, BLUE, WHITE]);
    let picture = image::RgbImage::from_raw(64, 64, data).unwrap();
    let mut bytes = Cursor::new(Vec::new());
    picture.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

#[tokio::test]
async fn a_png_with_bounds_and_crs_serves_correct_pixels() {
    let state = test_state().await;
    // 0.64 by 0.32 degrees over 64 square pixels, so these pixels are not square
    let asset = upload_created(
        &state,
        &[
            ("bounds", None, b"7,46.68,7.64,47".to_vec()),
            ("crs", None, b"EPSG:4326".to_vec()),
            (
                "file",
                Some("map.png"),
                picture_bytes(image::ImageFormat::Png),
            ),
        ],
    )
    .await;
    assert_eq!(asset["asset_type"], "imagery");
    assert_eq!(asset["status"], "ready");
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, 7.1, 46.95, 12).await, opaque(RED));
    assert_eq!(pixel_at(&state, &id, 7.5, 46.95, 12).await, opaque(GREEN));
    assert_eq!(pixel_at(&state, &id, 7.1, 46.72, 12).await, opaque(BLUE));
    assert_eq!(pixel_at(&state, &id, 7.5, 46.72, 12).await, opaque(WHITE));
}

#[tokio::test]
async fn a_png_takes_only_asset_type_imagery() {
    let state = test_state().await;
    let png = picture_bytes(image::ImageFormat::Png);
    for (asset_type, expected) in [
        ("terrain", "asset_type terrain"),
        ("junk", "asset_type junk"),
    ] {
        let reason = upload_refused(
            &state,
            &[
                ("bounds", None, b"7,46.68,7.64,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("asset_type", None, asset_type.as_bytes().to_vec()),
                ("file", Some("map.png"), png.clone()),
            ],
        )
        .await;
        assert!(reason.contains(expected), "{reason}");
    }

    let asset = upload_created(
        &state,
        &[
            ("bounds", None, b"7,46.68,7.64,47".to_vec()),
            ("crs", None, b"EPSG:4326".to_vec()),
            ("asset_type", None, b"imagery".to_vec()),
            ("file", Some("map.png"), png),
        ],
    )
    .await;
    assert_eq!(asset["asset_type"], "imagery");
}

#[tokio::test]
async fn a_jpeg_with_a_world_file_serves_close_pixels() {
    let state = test_state().await;
    let world_file = b"0.01\n0\n0\n-0.01\n7.005\n46.995\n".to_vec();
    let asset = upload_created(
        &state,
        &[
            ("world_file", None, world_file),
            ("crs", None, b"EPSG:4326".to_vec()),
            (
                "file",
                Some("map.jpg"),
                picture_bytes(image::ImageFormat::Jpeg),
            ),
        ],
    )
    .await;
    let id = asset_id(&asset);
    let [r, g, b, a] = pixel_at(&state, &id, 7.1, 46.9, 12).await;
    assert!(r > 150 && g < 60 && b < 60 && a == 255, "{r} {g} {b} {a}");
    let [r, g, b, _] = pixel_at(&state, &id, 7.5, 46.5, 12).await;
    assert!(r > 200 && g > 200 && b > 200, "{r} {g} {b}");
}

#[tokio::test]
async fn pictures_without_georeferencing_are_refused_by_name() {
    let state = test_state().await;
    let png = picture_bytes(image::ImageFormat::Png);

    let reason = upload_refused(&state, &[("file", Some("a.png"), png.clone())]).await;
    assert!(
        reason.contains("world_file") && reason.contains("bounds"),
        "{reason}"
    );

    let reason = upload_refused(
        &state,
        &[
            ("world_file", None, b"1\n0\n0\n-1\n0\n0".to_vec()),
            ("file", Some("a.png"), png.clone()),
        ],
    )
    .await;
    assert!(reason.contains("crs"), "{reason}");

    let reason = upload_refused(
        &state,
        &[
            ("bounds", None, b"7,46,8,47".to_vec()),
            ("crs", None, b"not a crs".to_vec()),
            ("file", Some("a.png"), png.clone()),
        ],
    )
    .await;
    assert!(reason.contains("EPSG"), "{reason}");

    // a wrong CRS puts the image off the globe
    let reason = upload_refused(
        &state,
        &[
            ("bounds", None, b"480000,4960000,520000,5000000".to_vec()),
            ("crs", None, b"EPSG:4326".to_vec()),
            ("file", Some("a.png"), png),
        ],
    )
    .await;
    assert!(reason.contains("off the globe"), "{reason}");

    let leftovers = std::fs::read_dir(&state.data_dir)
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            uuid::Uuid::parse_str(&name.to_string_lossy()).is_ok()
        })
        .count();
    assert_eq!(leftovers, 0, "a refusal leaves no asset directory behind");
}

#[tokio::test]
async fn a_colour_tiff_without_georeferencing_is_refused_by_name() {
    let state = test_state().await;
    let data = quadrants(16, [RED, GREEN, BLUE, WHITE]);
    let file = stripped_lzw_tiff::<RGB8>(16, &data, None, None, None);
    let reason = upload_refused(&state, &[("file", Some("scan.tif"), file.clone())]).await;
    assert!(reason.contains("georeferencing"), "{reason}");

    let asset = upload_created(
        &state,
        &[
            (
                "world_file",
                None,
                b"0.1\n0\n0\n-0.1\n7.05\n46.95\n".to_vec(),
            ),
            ("crs", None, b"4326".to_vec()),
            ("file", Some("scan.tif"), file),
        ],
    )
    .await;
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, 7.2, 46.8, 10).await, opaque(RED));
}

#[tokio::test]
async fn a_file_that_is_not_a_tiff_stays_a_stored_terrain_asset() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("dem.tif"), b"not a tiff".to_vec())]).await;
    assert_eq!(asset["asset_type"], "terrain");
    assert_eq!(asset["status"], "uploading");
    let id = asset_id(&asset);
    let (status, body) = fetch(&state, &format!("/api/v1/assets/{id}/imagery/1/0/0.png")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("no imagery tiles")
    );

    let reason = upload_refused(
        &state,
        &[
            ("file", Some("dem.tif"), b"not a tiff".to_vec()),
            ("asset_type", None, b"imagery".to_vec()),
        ],
    )
    .await;
    assert!(reason.contains("not a readable TIFF"), "{reason}");
}

#[tokio::test]
async fn raster_fields_on_other_uploads_are_refused() {
    let state = test_state().await;
    let reason = upload_refused(
        &state,
        &[
            ("bounds", None, b"7,46,8,47".to_vec()),
            ("file", Some("cloud.las"), b"bytes".to_vec()),
        ],
    )
    .await;
    assert!(
        reason.contains("world_file, bounds and asset_type"),
        "{reason}"
    );

    let reason = upload_refused(
        &state,
        &[
            ("asset_type", None, b"model".to_vec()),
            ("file", Some("ortho.tif"), GDAL_COG.to_vec()),
        ],
    )
    .await;
    assert!(reason.contains("asset_type model"), "{reason}");
}

#[tokio::test]
async fn tilejson_describes_the_layer() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    let (status, body) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/tilejson.json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tilejson: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(tilejson["tilejson"], "3.0.0");
    assert_eq!(tilejson["name"], "ortho.tif");
    assert_eq!(
        tilejson["tiles"][0],
        format!("/api/v1/assets/{id}/imagery/{{z}}/{{x}}/{{y}}.png")
    );
    assert_eq!(tilejson["minzoom"], 0);
    // a 0.001 degree pixel is 111 m, and z11 has 76 m pixels at the equator
    assert_eq!(tilejson["maxzoom"], 11);
    let bounds: Vec<f64> = serde_json::from_value(tilejson["bounds"].clone()).unwrap();
    let expected = [7.0, 46.488, 7.512, 47.0];
    for (got, want) in bounds.iter().zip(expected) {
        assert!((got - want).abs() < 1e-9, "{bounds:?}");
    }

    let unknown = uuid::Uuid::new_v4();
    let (status, _) = fetch(
        &state,
        &format!("/api/v1/assets/{unknown}/imagery/tilejson.json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_ion_endpoint_answers_the_way_cesium_reads_a_url_template() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    let (status, body) = fetch_as_owner(&state, &format!("/v1/assets/{id}/endpoint")).await;
    assert_eq!(status, StatusCode::OK);
    let endpoint: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(endpoint["type"], "IMAGERY");
    assert_eq!(endpoint["externalType"], "URL_TEMPLATE");
    assert_eq!(endpoint["attributions"], serde_json::json!([]));
    let url = endpoint["options"]["url"].as_str().unwrap();
    assert!(
        url.ends_with(&format!(
            "/api/v1/assets/{id}/imagery/{{z}}/{{x}}/{{y}}.png"
        )),
        "{url}"
    );
    assert_eq!(endpoint["options"]["maximumLevel"], 11);
    let west = endpoint["options"]["rectangle"]["west"].as_f64().unwrap();
    assert!((west - 7.0_f64.to_radians()).abs() < 1e-9);
    let north = endpoint["options"]["rectangle"]["north"].as_f64().unwrap();
    assert!((north - 47.0_f64.to_radians()).abs() < 1e-9);

    let (status, body) = fetch_as_owner(&state, &format!("/v1/assets/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    let ion_asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ion_asset["type"], "IMAGERY");
    assert_eq!(ion_asset["status"], "COMPLETE");
    assert_eq!(ion_asset["percentComplete"], 100);

    // IonImageryProvider.fromAssetId takes only the number the asset list hands out
    let ion_id = ion_asset["id"].as_i64().unwrap();
    let (status, body) = fetch_as_owner(&state, &format!("/v1/assets/{ion_id}/endpoint")).await;
    assert_eq!(status, StatusCode::OK);
    let by_number: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(by_number, endpoint);
}

#[cfg(not(feature = "jp2"))]
#[tokio::test]
async fn a_jp2_upload_without_the_jp2_feature_is_refused_and_leaves_no_asset() {
    let state = test_state().await;
    for name in ["scan.jp2", "scan.j2k"] {
        let reason = upload_refused(&state, &[("file", Some(name), b"jp2-bytes".to_vec())]).await;
        assert!(reason.contains("jp2 feature"), "{reason}");
    }
    assert!(state.db.list_assets().await.unwrap().is_empty());
}

#[cfg(feature = "jp2")]
mod jp2 {
    use std::ffi::c_char;
    use std::path::Path;
    use std::ptr;

    use super::*;
    use grokj2k_sys::{
        _GRK_CHANNEL_ASSOC_GRK_CHANNEL_ASSOC_WHOLE_IMAGE,
        _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_OPACITY, _GRK_COLOR_SPACE_GRK_CLRSPC_GRAY,
        _GRK_COLOR_SPACE_GRK_CLRSPC_SRGB, GRK_COLOR_SPACE, GRK_FMT_J2K, GRK_FMT_JP2, grk_asoc,
        grk_compress, grk_compress_init, grk_compress_set_default_params, grk_cparameters,
        grk_image_comp, grk_image_meta, grk_image_meta_new, grk_image_meta_set_asocs,
        grk_image_meta_set_field, grk_image_new, grk_initialize, grk_object_unref,
        grk_stream_params,
    };

    struct Jp2Picture<'a> {
        cols: u32,
        rows: u32,
        bands: u16,
        precision: u8,
        opacity_band: Option<u16>,
        sample: &'a dyn Fn(u32, u32, u16) -> i32,
    }

    #[derive(Default)]
    struct Jp2Boxes<'a> {
        geotiff: Option<&'a [u8]>,
        gml: Option<&'a str>,
    }

    fn attached_meta(boxes: &Jp2Boxes) -> *mut grk_image_meta {
        unsafe {
            let meta = grk_image_meta_new();
            assert!(!meta.is_null());
            if let Some(geotiff) = boxes.geotiff {
                let field = c"geotiff".as_ptr();
                assert!(grk_image_meta_set_field(
                    meta,
                    field,
                    geotiff.as_ptr(),
                    geotiff.len()
                ));
            }
            if let Some(gml) = boxes.gml {
                let mut asocs: [grk_asoc; 2] = std::mem::zeroed();
                asocs[0].level = 0;
                asocs[0].label = c"gml.data".as_ptr();
                asocs[1].level = 1;
                asocs[1].label = c"gml.root-instance".as_ptr();
                asocs[1].xml = gml.as_ptr().cast_mut();
                asocs[1].xml_len = gml.len() as u32;
                assert!(grk_image_meta_set_asocs(meta, asocs.as_ptr(), 2));
            }
            meta
        }
    }

    fn write_codestream(
        path: &Path,
        picture: &Jp2Picture,
        parameters: &mut grk_cparameters,
        meta: *mut grk_image_meta,
    ) {
        let colour_space: GRK_COLOR_SPACE = if picture.bands >= 3 {
            _GRK_COLOR_SPACE_GRK_CLRSPC_SRGB
        } else {
            _GRK_COLOR_SPACE_GRK_CLRSPC_GRAY
        };
        unsafe {
            grk_initialize(ptr::null(), 0, ptr::null_mut());
            let mut component: grk_image_comp = std::mem::zeroed();
            component.dx = 1;
            component.dy = 1;
            component.w = picture.cols;
            component.h = picture.rows;
            component.prec = picture.precision;
            let mut components = vec![component; usize::from(picture.bands)];
            let image = grk_image_new(picture.bands, components.as_mut_ptr(), colour_space, true);
            assert!(!image.is_null());
            if let Some(band) = picture.opacity_band {
                let component = &mut *(*image).comps.add(usize::from(band));
                component.type_ = _GRK_CHANNEL_TYPE_GRK_CHANNEL_TYPE_OPACITY;
                component.association = _GRK_CHANNEL_ASSOC_GRK_CHANNEL_ASSOC_WHOLE_IMAGE;
            }
            for band in 0..picture.bands {
                let component = &*(*image).comps.add(usize::from(band));
                let samples = component.data.cast::<i32>();
                for row in 0..picture.rows {
                    for col in 0..picture.cols {
                        let at = row as usize * component.stride as usize + col as usize;
                        *samples.add(at) = (picture.sample)(col, row, band);
                    }
                }
            }
            (*image).meta = meta;

            let mut stream: grk_stream_params = std::mem::zeroed();
            let path = path.to_str().unwrap();
            for (slot, byte) in stream.file.iter_mut().zip(path.bytes()) {
                *slot = byte as c_char;
            }
            let codec = grk_compress_init(&mut stream, parameters, image);
            assert!(!codec.is_null());
            assert!(grk_compress(codec, ptr::null_mut()) > 0);
            (*image).meta = ptr::null_mut();
            grk_object_unref(codec);
            grk_object_unref(&mut (*image).obj);
        }
    }

    fn lossless_parameters() -> grk_cparameters {
        unsafe {
            let mut parameters: grk_cparameters = std::mem::zeroed();
            grk_compress_set_default_params(&mut parameters);
            parameters.irreversible = false;
            parameters.numlayers = 1;
            parameters.layer_rate[0] = 0.0;
            parameters
        }
    }

    fn encoded(
        picture: &Jp2Picture,
        boxes: &Jp2Boxes,
        tile_side: Option<u32>,
        is_jp2: bool,
    ) -> Vec<u8> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture");
        let mut parameters = lossless_parameters();
        parameters.cod_format = if is_jp2 { GRK_FMT_JP2 } else { GRK_FMT_J2K };
        parameters.jpx_branding = boxes.gml.is_some();
        if let Some(tile_side) = tile_side {
            parameters.tile_size_on = true;
            parameters.t_width = tile_side;
            parameters.t_height = tile_side;
        }
        let meta = if is_jp2 {
            attached_meta(boxes)
        } else {
            ptr::null_mut()
        };
        write_codestream(&path, picture, &mut parameters, meta);
        if !meta.is_null() {
            unsafe { grk_object_unref(&mut (*meta).obj) };
        }
        std::fs::read(&path).unwrap()
    }

    fn quadrant_sample(col: u32, row: u32, band: u16) -> i32 {
        let quadrant = usize::from(row >= 32) * 2 + usize::from(col >= 32);
        i32::from([RED, GREEN, BLUE, WHITE][quadrant][usize::from(band)])
    }

    const RGB_QUADRANTS: Jp2Picture = Jp2Picture {
        cols: 64,
        rows: 64,
        bands: 3,
        precision: 8,
        opacity_band: None,
        sample: &quadrant_sample,
    };

    async fn assert_alps_quadrants(state: &Arc<AppState>, asset: &serde_json::Value) {
        assert_eq!(asset["asset_type"], "imagery");
        assert_eq!(asset["status"], "ready");
        let id = asset_id(asset);
        assert_eq!(pixel_at(state, &id, 7.1, 46.9, 12).await, opaque(RED));
        assert_eq!(pixel_at(state, &id, 7.5, 46.9, 12).await, opaque(GREEN));
        assert_eq!(pixel_at(state, &id, 7.1, 46.5, 12).await, opaque(BLUE));
        assert_eq!(pixel_at(state, &id, 7.5, 46.5, 12).await, opaque(WHITE));
    }

    #[tokio::test]
    async fn a_j2k_with_bounds_and_crs_serves_correct_pixels() {
        let state = test_state().await;
        let j2k = encoded(&RGB_QUADRANTS, &Jp2Boxes::default(), None, false);
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, b"7,46.36,7.64,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("file", Some("map.j2k"), j2k),
            ],
        )
        .await;
        assert_alps_quadrants(&state, &asset).await;
    }

    #[tokio::test]
    async fn a_tiled_jp2_is_placed_by_its_geotiff_box() {
        let state = test_state().await;
        let stub = stripped_lzw_tiff::<Gray8>(1, &[0], Some(&ALPS), None, None);
        let boxes = Jp2Boxes {
            geotiff: Some(&stub),
            gml: None,
        };
        let jp2 = encoded(&RGB_QUADRANTS, &boxes, Some(24), true);
        let asset = upload_created(&state, &[("file", Some("map.jp2"), jp2)]).await;
        assert_alps_quadrants(&state, &asset).await;
    }

    #[tokio::test]
    async fn a_jp2_is_placed_by_its_gml_box_in_latitude_longitude_order() {
        let state = test_state().await;
        let gml = r#"<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml">
          <gml:RectifiedGrid dimension="2">
            <gml:origin>
              <gml:Point srsName="urn:ogc:def:crs:EPSG::4326"><gml:pos>46.995 7.005</gml:pos></gml:Point>
            </gml:origin>
            <gml:offsetVector>0 0.01</gml:offsetVector>
            <gml:offsetVector>-0.01 0</gml:offsetVector>
          </gml:RectifiedGrid>
        </gml:FeatureCollection>"#;
        let boxes = Jp2Boxes {
            geotiff: None,
            gml: Some(gml),
        };
        let jp2 = encoded(&RGB_QUADRANTS, &boxes, None, true);
        let asset = upload_created(&state, &[("file", Some("map.jp2"), jp2)]).await;
        assert_alps_quadrants(&state, &asset).await;
    }

    #[tokio::test]
    async fn form_fields_override_the_boxes_of_a_jp2() {
        let state = test_state().await;
        let stub = stripped_lzw_tiff::<Gray8>(1, &[0], Some(&ALPS), None, None);
        let boxes = Jp2Boxes {
            geotiff: Some(&stub),
            gml: None,
        };
        let jp2 = encoded(&RGB_QUADRANTS, &boxes, None, true);
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, b"20,46.36,20.64,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("file", Some("map.jp2"), jp2),
            ],
        )
        .await;
        let id = asset_id(&asset);
        assert_eq!(pixel_at(&state, &id, 20.1, 46.9, 12).await, opaque(RED));
    }

    #[tokio::test]
    async fn jpeg_2000_without_georeferencing_is_refused_by_name_and_leaves_no_asset() {
        let state = test_state().await;
        let j2k = encoded(&RGB_QUADRANTS, &Jp2Boxes::default(), None, false);
        let reason = upload_refused(&state, &[("file", Some("map.j2k"), j2k)]).await;
        assert!(
            reason.contains("world_file") && reason.contains("bounds"),
            "{reason}"
        );
        let jp2 = encoded(&RGB_QUADRANTS, &Jp2Boxes::default(), None, true);
        let reason = upload_refused(&state, &[("file", Some("map.jp2"), jp2)]).await;
        assert!(reason.contains("GeoJP2"), "{reason}");
        assert!(state.db.list_assets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn bytes_that_are_not_jpeg_2000_are_refused() {
        let state = test_state().await;
        let reason = upload_refused(
            &state,
            &[
                ("bounds", None, b"7,46.36,7.64,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("file", Some("scan.jp2"), b"jp2-bytes".to_vec()),
            ],
        )
        .await;
        assert!(reason.contains("not a readable JPEG 2000 file"), "{reason}");
    }

    #[tokio::test]
    async fn rows_on_both_sides_of_a_decode_window_land_where_they_belong() {
        let state = test_state().await;
        // 8192 cols of one band are a 2048 row decode window
        let picture = Jp2Picture {
            cols: 8192,
            rows: 2100,
            bands: 1,
            precision: 8,
            opacity_band: None,
            sample: &|_, row, _| if row < 2048 { 40 } else { 200 },
        };
        let j2k = encoded(&picture, &Jp2Boxes::default(), None, false);
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, b"7,44.9,15.192,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("file", Some("long.j2k"), j2k),
            ],
        )
        .await;
        let id = asset_id(&asset);
        assert_eq!(
            pixel_at(&state, &id, 9.0, 44.98, 11).await,
            opaque([40, 40, 40])
        );
        assert_eq!(
            pixel_at(&state, &id, 9.0, 44.92, 11).await,
            opaque([200, 200, 200])
        );
    }

    #[tokio::test]
    async fn a_jp2_channel_definition_marks_the_alpha_band() {
        let state = test_state().await;
        let stub = stripped_lzw_tiff::<Gray8>(1, &[0], Some(&ALPS), None, None);
        let boxes = Jp2Boxes {
            geotiff: Some(&stub),
            gml: None,
        };
        let picture = Jp2Picture {
            cols: 64,
            rows: 64,
            bands: 4,
            precision: 8,
            opacity_band: Some(3),
            sample: &|col, _, band| match band {
                0 => i32::from(RED[0]),
                1 => i32::from(RED[1]),
                2 => i32::from(RED[2]),
                _ if col < 32 => 255,
                _ => 0,
            },
        };
        let jp2 = encoded(&picture, &boxes, None, true);
        let asset = upload_created(&state, &[("file", Some("alpha.jp2"), jp2)]).await;
        let id = asset_id(&asset);
        assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, opaque(RED));
        assert_eq!(pixel_at(&state, &id, 7.5, 46.9, 12).await[3], 0);
    }

    #[tokio::test]
    async fn twelve_bit_samples_stretch_from_their_lowest_to_their_highest_value() {
        let state = test_state().await;
        let picture = Jp2Picture {
            cols: 64,
            rows: 64,
            bands: 1,
            precision: 12,
            opacity_band: None,
            sample: &|col, _, _| if col < 32 { 100 } else { 4000 },
        };
        let j2k = encoded(&picture, &Jp2Boxes::default(), None, false);
        let asset = upload_created(
            &state,
            &[
                ("bounds", None, b"7,46.36,7.64,47".to_vec()),
                ("crs", None, b"EPSG:4326".to_vec()),
                ("file", Some("deep.j2k"), j2k),
            ],
        )
        .await;
        let id = asset_id(&asset);
        assert_eq!(
            pixel_at(&state, &id, 7.1, 46.9, 12).await,
            opaque([0, 0, 0])
        );
        assert_eq!(
            pixel_at(&state, &id, 7.5, 46.9, 12).await,
            opaque([255, 255, 255])
        );
    }
}

#[tokio::test]
async fn deleting_the_asset_ends_its_tiles() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    assert_eq!(pixel_at(&state, &id, 7.1, 46.9, 12).await, opaque(RED));

    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/assets/{id}"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let response = router(Arc::clone(&state)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let (status, _) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/12/2132/1450.png"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn failed_tiling_job_error(state: &Arc<AppState>, id: &str) -> String {
    let worker = Arc::clone(&state.job_queue).start().await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/assets/{id}/tile"))
        .header("authorization", format!("Bearer {}", editor()))
        .body(Body::empty())
        .unwrap();
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let job_id: uuid::Uuid = job["id"].as_str().unwrap().parse().unwrap();

    let mut settled = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let record = state.db.get_job(job_id).await.unwrap().unwrap();
        if record.completed_at.is_some() {
            settled = Some(record);
            break;
        }
    }
    worker.abort();
    let record = settled.expect("the job settles");
    record.error.expect("a tiling job on imagery fails")
}

#[tokio::test]
async fn a_tiling_request_leaves_ready_imagery_ready() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);

    let error = failed_tiling_job_error(&state, &id).await;
    assert!(error.contains("not tiled"), "{error}");

    let (_, body) = fetch_as_owner(&state, &format!("/v1/assets/{id}")).await;
    let ion_asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(ion_asset["status"], "COMPLETE");
}

struct XmlElement {
    path: String,
    attributes: Vec<(String, String)>,
    text: String,
}

impl XmlElement {
    fn attribute(&self, name: &str) -> &str {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("{} has no {name}", self.path))
    }
}

fn opened_element(
    elements: &[XmlElement],
    open: &[usize],
    start: &quick_xml::events::BytesStart,
) -> XmlElement {
    let name = String::from_utf8(start.name().as_ref().to_vec()).unwrap();
    let path = match open.last() {
        Some(&parent) => format!("{}/{name}", elements[parent].path),
        None => name,
    };
    let attributes = start
        .attributes()
        .map(|attribute| {
            let attribute = attribute.unwrap();
            (
                String::from_utf8(attribute.key.as_ref().to_vec()).unwrap(),
                attribute.unescape_value().unwrap().into_owned(),
            )
        })
        .collect();
    XmlElement {
        path,
        attributes,
        text: String::new(),
    }
}

fn parse_xml(xml: &[u8]) -> Vec<XmlElement> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(std::str::from_utf8(xml).unwrap());
    reader.config_mut().trim_text(true);
    let mut elements = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    loop {
        match reader.read_event().unwrap() {
            Event::Start(start) => {
                let element = opened_element(&elements, &open, &start);
                elements.push(element);
                open.push(elements.len() - 1);
            }
            Event::Empty(start) => {
                let element = opened_element(&elements, &open, &start);
                elements.push(element);
            }
            Event::Text(text) => {
                let parent = *open.last().unwrap();
                elements[parent].text.push_str(&text.decode().unwrap());
            }
            Event::End(_) => {
                open.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert!(open.is_empty());
    elements
}

fn only<'a>(elements: &'a [XmlElement], path: &str) -> &'a XmlElement {
    let found: Vec<&XmlElement> = elements.iter().filter(|e| e.path == path).collect();
    assert_eq!(found.len(), 1, "{path}");
    found[0]
}

fn texts<'a>(elements: &'a [XmlElement], path: &str) -> Vec<&'a str> {
    elements
        .iter()
        .filter(|e| e.path == path)
        .map(|e| e.text.as_str())
        .collect()
}

fn numbers(text: &str) -> Vec<f64> {
    text.split_whitespace()
        .map(|number| number.parse().unwrap())
        .collect()
}

fn assert_close(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (got_value, want_value) in got.iter().zip(want) {
        assert!(
            (got_value - want_value).abs() <= 1e-9 * want_value.abs().max(1.0),
            "{got:?} against {want:?}"
        );
    }
}

fn wmts_get_tile(id: &str, tile_matrix: u8, row: u32, col: u32) -> String {
    format!(
        "/api/v1/assets/{id}/imagery/wmts?service=WMTS&request=GetTile&version=1.0.0\
         &layer={id}&style=default&format=image/png&TileMatrixSet=GoogleMapsCompatible\
         &TileMatrix={tile_matrix}&TileRow={row}&TileCol={col}"
    )
}

fn pixel_of(png: &[u8], col: u32, row: u32) -> [u8; 4] {
    image::load_from_memory(png)
        .unwrap()
        .to_rgba8()
        .get_pixel(col, row)
        .0
}

#[tokio::test]
async fn wmts_capabilities_describe_the_layer_its_bounds_and_tile_limits() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    let (status, kvp) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/wmts?SERVICE=WMTS&REQUEST=GetCapabilities"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&kvp));
    let (status, restful) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/wmts/1.0.0/WMTSCapabilities.xml"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(kvp, restful);

    let elements = parse_xml(&kvp);
    let root = only(&elements, "Capabilities");
    assert_eq!(root.attribute("xmlns"), "http://www.opengis.net/wmts/1.0");
    assert_eq!(
        root.attribute("xmlns:ows"),
        "http://www.opengis.net/ows/1.1"
    );
    assert_eq!(root.attribute("version"), "1.0.0");

    let layer = "Capabilities/Contents/Layer";
    assert_eq!(only(&elements, &format!("{layer}/ows:Identifier")).text, id);
    assert_eq!(
        only(&elements, &format!("{layer}/ows:Title")).text,
        "ortho.tif"
    );
    assert_eq!(
        only(&elements, &format!("{layer}/Style")).attribute("isDefault"),
        "true"
    );
    assert_eq!(
        only(&elements, &format!("{layer}/Style/ows:Identifier")).text,
        "default"
    );
    assert_eq!(
        only(&elements, &format!("{layer}/Format")).text,
        "image/png"
    );
    let bounding_box = format!("{layer}/ows:WGS84BoundingBox");
    assert_close(
        &numbers(&only(&elements, &format!("{bounding_box}/ows:LowerCorner")).text),
        &[7.0, 46.488],
    );
    assert_close(
        &numbers(&only(&elements, &format!("{bounding_box}/ows:UpperCorner")).text),
        &[7.512, 47.0],
    );
    assert_eq!(
        only(
            &elements,
            &format!("{layer}/TileMatrixSetLink/TileMatrixSet")
        )
        .text,
        "GoogleMapsCompatible"
    );
    let resource = only(&elements, &format!("{layer}/ResourceURL"));
    assert_eq!(resource.attribute("resourceType"), "tile");
    assert_eq!(resource.attribute("format"), "image/png");
    let template = resource.attribute("template");
    assert!(
        template.ends_with(&format!(
            "/api/v1/assets/{id}/imagery/{{TileMatrix}}/{{TileCol}}/{{TileRow}}.png"
        )),
        "{template}"
    );
    assert!(
        template.starts_with(common::TEST_ION_BASE_URL),
        "{template}"
    );

    // the tilejson maxzoom of this image is 11
    let levels: Vec<u8> = (0..=11).collect();
    let limits = format!("{layer}/TileMatrixSetLink/TileMatrixSetLimits/TileMatrixLimits");
    let limit_column = |name: &str| -> Vec<u32> {
        texts(&elements, &format!("{limits}/{name}"))
            .iter()
            .map(|text| text.parse().unwrap())
            .collect()
    };
    let level_names: Vec<u32> = levels.iter().map(|&z| u32::from(z)).collect();
    assert_eq!(limit_column("TileMatrix"), level_names);
    let mut expected_limits = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for &z in &levels {
        let (west_col, north_row, _, _) = tile_and_pixel_holding_point(7.0, 47.0, z);
        let (east_col, south_row, _, _) = tile_and_pixel_holding_point(7.512, 46.488, z);
        for (column, value) in expected_limits
            .iter_mut()
            .zip([north_row, south_row, west_col, east_col])
        {
            column.push(value);
        }
    }
    let [min_rows, max_rows, min_cols, max_cols] = expected_limits;
    assert_eq!(limit_column("MinTileRow"), min_rows);
    assert_eq!(limit_column("MaxTileRow"), max_rows);
    assert_eq!(limit_column("MinTileCol"), min_cols);
    assert_eq!(limit_column("MaxTileCol"), max_cols);

    let matrix_set = "Capabilities/Contents/TileMatrixSet";
    assert_eq!(
        only(&elements, &format!("{matrix_set}/ows:Identifier")).text,
        "GoogleMapsCompatible"
    );
    assert_eq!(
        only(&elements, &format!("{matrix_set}/ows:SupportedCRS")).text,
        "urn:ogc:def:crs:EPSG::3857"
    );
    assert_eq!(
        only(&elements, &format!("{matrix_set}/WellKnownScaleSet")).text,
        "urn:ogc:def:wkss:OGC:1.0:GoogleMapsCompatible"
    );
    let matrix = format!("{matrix_set}/TileMatrix");
    assert_eq!(
        texts(&elements, &format!("{matrix}/ows:Identifier")),
        level_names.iter().map(u32::to_string).collect::<Vec<_>>()
    );
    let scales: Vec<f64> = texts(&elements, &format!("{matrix}/ScaleDenominator"))
        .iter()
        .map(|text| text.parse().unwrap())
        .collect();
    // scale denominators of zooms 0, 1 and 11 from table E.4 of ogc 07-057r7
    assert_close(
        &[scales[0], scales[1], scales[11]],
        &[559082264.0287178, 279541132.0143589, 272989.3867327723],
    );
    for corner in texts(&elements, &format!("{matrix}/TopLeftCorner")) {
        assert_close(&numbers(corner), &[-20037508.342789244, 20037508.342789244]);
    }
    let widths: Vec<u32> = texts(&elements, &format!("{matrix}/MatrixWidth"))
        .iter()
        .map(|text| text.parse().unwrap())
        .collect();
    let tiles_across: Vec<u32> = levels.iter().map(|&z| 1 << z).collect();
    assert_eq!(widths, tiles_across);

    let (x, y, col, row) = tile_and_pixel_holding_point(7.1, 46.9, 11);
    let tile_path = template
        .strip_prefix(common::TEST_ION_BASE_URL)
        .unwrap()
        .replace("{TileMatrix}", "11")
        .replace("{TileCol}", &x.to_string())
        .replace("{TileRow}", &y.to_string());
    let (status, tile) = fetch(&state, &tile_path).await;
    assert_eq!(status, StatusCode::OK, "{tile_path}");
    assert_eq!(pixel_of(&tile, col, row), opaque(RED), "{tile_path}");
}

#[tokio::test]
async fn wmts_get_tile_answers_the_bytes_of_the_xyz_tile() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    let (x, y, col, row) = tile_and_pixel_holding_point(7.1, 46.9, 11);
    let (status, xyz) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/11/{x}/{y}.png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, kvp) = fetch(&state, &wmts_get_tile(&id, 11, y, x)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&kvp));
    assert_eq!(kvp, xyz);
    assert_eq!(pixel_of(&kvp, col, row), opaque(RED));
}

#[tokio::test]
async fn tms_counts_rows_from_the_south() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);

    let (status, xml) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/tms/tilemapresource.xml"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let elements = parse_xml(&xml);
    assert_eq!(only(&elements, "TileMap").attribute("version"), "1.0.0");
    assert_eq!(only(&elements, "TileMap/SRS").text, "EPSG:3857");
    let format = only(&elements, "TileMap/TileFormat");
    assert_eq!(format.attribute("mime-type"), "image/png");
    assert_eq!(format.attribute("extension"), "png");
    let origin = only(&elements, "TileMap/Origin");
    assert_close(
        &[origin.attribute("x"), origin.attribute("y")].map(|value| value.parse::<f64>().unwrap()),
        &[-20037508.342789244, -20037508.342789244],
    );
    let bounding_box = only(&elements, "TileMap/BoundingBox");
    let mercator_x = |lon: f64| lon / 180.0 * 20037508.342789244;
    let mercator_y =
        |lat: f64| lat.to_radians().tan().asinh() / std::f64::consts::PI * 20037508.342789244;
    assert_close(
        &["minx", "miny", "maxx", "maxy"]
            .map(|name| bounding_box.attribute(name).parse::<f64>().unwrap()),
        &[
            mercator_x(7.0),
            mercator_y(46.488),
            mercator_x(7.512),
            mercator_y(47.0),
        ],
    );
    assert_eq!(
        only(&elements, "TileMap/TileSets").attribute("profile"),
        "global-mercator"
    );
    let tile_sets: Vec<&XmlElement> = elements
        .iter()
        .filter(|e| e.path == "TileMap/TileSets/TileSet")
        .collect();
    assert_eq!(tile_sets.len(), 12);
    for (z, tile_set) in tile_sets.iter().enumerate() {
        assert_eq!(tile_set.attribute("order"), z.to_string());
        let href = tile_set.attribute("href");
        assert!(
            href.ends_with(&format!("/api/v1/assets/{id}/imagery/tms/{z}")),
            "{href}"
        );
        assert!(href.starts_with(common::TEST_ION_BASE_URL), "{href}");
        let units_per_pixel: f64 = tile_set.attribute("units-per-pixel").parse().unwrap();
        assert_close(
            &[units_per_pixel],
            &[156543.03392804097 / f64::from(1u32 << z)],
        );
    }

    let (x, y, col, row) = tile_and_pixel_holding_point(7.1, 46.9, 11);
    let tms_y = (1 << 11) - 1 - y;
    let (status, tms) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/tms/11/{x}/{tms_y}.png"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, xyz) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/11/{x}/{y}.png"),
    )
    .await;
    assert_eq!(tms, xyz);
    assert_eq!(pixel_of(&tms, col, row), opaque(RED));
}

#[tokio::test]
async fn tiles_past_the_limits_are_refused_the_way_each_spec_says() {
    let state = test_state().await;
    let asset = upload_created(&state, &[("file", Some("ortho.tif"), GDAL_COG.to_vec())]).await;
    let id = asset_id(&asset);
    let (east_col, south_row, _, _) = tile_and_pixel_holding_point(7.512, 46.488, 11);

    let exception = |body: &[u8]| -> (String, String) {
        let elements = parse_xml(body);
        assert_eq!(
            only(&elements, "ExceptionReport").attribute("xmlns"),
            "http://www.opengis.net/ows/1.1"
        );
        let exception = only(&elements, "ExceptionReport/Exception");
        (
            exception.attribute("exceptionCode").to_string(),
            exception.attribute("locator").to_string(),
        )
    };
    let (status, body) = fetch(&state, &wmts_get_tile(&id, 11, south_row + 1, east_col)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        exception(&body),
        ("TileOutOfRange".to_string(), "TileRow".to_string())
    );
    let (status, body) = fetch(&state, &wmts_get_tile(&id, 11, south_row, east_col + 1)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        exception(&body),
        ("TileOutOfRange".to_string(), "TileCol".to_string())
    );
    let (status, body) = fetch(&state, &wmts_get_tile(&id, 12, 0, 0)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        exception(&body),
        (
            "InvalidParameterValue".to_string(),
            "TileMatrix".to_string()
        )
    );
    let without_column =
        wmts_get_tile(&id, 11, south_row, east_col).replace(&format!("&TileCol={east_col}"), "");
    let (status, body) = fetch(&state, &without_column).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        exception(&body),
        ("MissingParameterValue".to_string(), "TileCol".to_string())
    );
    let (status, body) = fetch(
        &state,
        &format!("/api/v1/assets/{id}/imagery/wmts?service=WMTS&request=GetFeatureInfo"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        exception(&body),
        ("OperationNotSupported".to_string(), "Request".to_string())
    );

    let past_south = (1 << 11) - 1 - (south_row + 1);
    for path in [
        format!("11/{east_col}/{past_south}.png"),
        format!("11/{}/{}.png", east_col + 1, (1 << 11) - 1 - south_row),
        "12/0/0.png".to_string(),
    ] {
        let (status, body) =
            fetch(&state, &format!("/api/v1/assets/{id}/imagery/tms/{path}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(parse_xml(&body)[0].path, "TileMapServerError");
    }

    let unknown = uuid::Uuid::new_v4();
    let (status, _) = fetch(
        &state,
        &format!("/api/v1/assets/{unknown}/imagery/wmts/1.0.0/WMTSCapabilities.xml"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
