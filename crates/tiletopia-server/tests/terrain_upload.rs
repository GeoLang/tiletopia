mod common;

use std::io::Cursor;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{DecodedTile, decode_tile};
use tiff::encoder::{TiffEncoder, colortype};
use tiff::tags::Tag;
use tiletopia_core::crs::{Coord3D, CrsDef, Transformer};
use tiletopia_server::db::{JobRecord, JobStatus, ModelPlacement};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tiletopia_terrain::global_dem::TerrainTileCoord;
use tower::ServiceExt;
use uuid::Uuid;

const JOB_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
const GEOGRAPHIC_TYPE_GEO_KEY: u16 = 2048;
const PROJECTED_TYPE_GEO_KEY: u16 = 3072;
// the fixture ramps are exact under bilinear sampling
const HEIGHT_TOLERANCE_METRES: f64 = 0.5;
// decoded vertices move by a quantization step, which can cross the edge of the DEM
const DEM_EDGE_MARGIN_DEGREES: f64 = 1e-3;

async fn started_state() -> Arc<AppState> {
    let state = common::test_state().await;
    Arc::clone(&state.job_queue).start().await;
    state
}

fn editor() -> String {
    common::token("terrain-editor", "editor")
}

async fn upload_dem(
    state: &Arc<AppState>,
    file_name: &str,
    contents: &[u8],
    crs: Option<&str>,
) -> serde_json::Value {
    let boundary = "tiletopiaterrainboundary";
    let mut body = Vec::new();
    if let Some(crs) = crs {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"crs\"\r\n\r\n{crs}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(contents);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {}", editor()))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

async fn response_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

// no token, because CesiumTerrainProvider sends none
async fn anonymous_get(state: &Arc<AppState>, uri: &str) -> axum::response::Response {
    router(Arc::clone(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn editor_request(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
) -> axum::response::Response {
    router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {}", editor()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn settled_job(state: &Arc<AppState>, job_id: Uuid) -> JobRecord {
    let deadline = std::time::Instant::now() + JOB_DEADLINE;
    loop {
        let job = state.db.get_job(job_id).await.unwrap().unwrap();
        if matches!(job.status, JobStatus::Done | JobStatus::Failed) {
            return job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "job {job_id} never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn job_id_of(uploaded: &serde_json::Value) -> Uuid {
    Uuid::parse_str(uploaded["job_id"].as_str().expect("job_id in the response")).unwrap()
}

fn asset_id_of(uploaded: &serde_json::Value) -> Uuid {
    Uuid::parse_str(uploaded["id"].as_str().unwrap()).unwrap()
}

async fn uploaded_and_tiled(
    state: &Arc<AppState>,
    file_name: &str,
    contents: &[u8],
    crs: Option<&str>,
) -> Uuid {
    let uploaded = upload_dem(state, file_name, contents, crs).await;
    let job = settled_job(state, job_id_of(&uploaded)).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    asset_id_of(&uploaded)
}

async fn terrain_root(state: &Arc<AppState>, asset_id: Uuid) -> String {
    let response = editor_request(state, "GET", &format!("/v1/assets/{asset_id}/endpoint")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let endpoint: serde_json::Value =
        serde_json::from_slice(&response_bytes(response).await).unwrap();
    assert_eq!(endpoint["type"], "TERRAIN");
    let url = endpoint["url"].as_str().unwrap();
    assert!(
        url.ends_with(&format!("/api/v1/terrain/bundles/{asset_id}/")),
        "{url}"
    );
    format!(
        "/{}",
        url.split_once("://").unwrap().1.split_once('/').unwrap().1
    )
}

async fn layer_json(state: &Arc<AppState>, root: &str) -> serde_json::Value {
    let response = anonymous_get(state, &format!("{root}layer.json")).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

fn deepest_tile_at(layer: &serde_json::Value, longitude: f64, latitude: f64) -> TerrainTileCoord {
    let zoom = layer["maxzoom"].as_u64().unwrap() as u32;
    let span = 180.0 / 2f64.powi(zoom as i32);
    TerrainTileCoord {
        zoom,
        x: ((longitude + 180.0) / span).floor() as u32,
        y: ((latitude + 90.0) / span).floor() as u32,
    }
}

async fn fetch_tile(state: &Arc<AppState>, root: &str, coord: &TerrainTileCoord) -> DecodedTile {
    let uri = format!(
        "{root}{}/{}/{}.terrain?v=1.0.0",
        coord.zoom, coord.x, coord.y
    );
    let response = anonymous_get(state, &uri).await;
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.quantized-mesh"
    );
    decode_tile(&response_bytes(response).await)
}

// 33 samples across one degree: 100 m climb per row northward and 10 m per column eastward
const HGT_SIDE: usize = 33;

fn hgt_ramp_elevation(longitude: f64, latitude: f64) -> f64 {
    100.0 + 3200.0 * (latitude - 47.0) + 320.0 * (longitude - 11.0)
}

fn hgt_ramp() -> Vec<u8> {
    let last = (HGT_SIDE - 1) as f64;
    let mut bytes = Vec::with_capacity(HGT_SIDE * HGT_SIDE * 2);
    for row in 0..HGT_SIDE {
        for column in 0..HGT_SIDE {
            let elevation =
                hgt_ramp_elevation(11.0 + column as f64 / last, 48.0 - row as f64 / last);
            bytes.extend_from_slice(&(elevation as i16).to_be_bytes());
        }
    }
    bytes
}

fn assert_tile_matches(
    tile: &DecodedTile,
    coord: &TerrainTileCoord,
    dem_bounds: [f64; 4],
    expected_elevation: impl Fn(f64, f64) -> f64,
) -> usize {
    let [west, south, east, north] = dem_bounds;
    let mut inside = 0;
    for [longitude, latitude, height] in tile.vertices(coord) {
        let distance_inside = (longitude - west)
            .min(east - longitude)
            .min(latitude - south)
            .min(north - latitude);
        if distance_inside.abs() < DEM_EDGE_MARGIN_DEGREES {
            continue;
        }
        let expected = if distance_inside > 0.0 {
            inside += 1;
            expected_elevation(longitude, latitude)
        } else {
            0.0
        };
        assert!(
            (height - expected).abs() < HEIGHT_TOLERANCE_METRES,
            "height {height} at ({longitude}, {latitude}), expected {expected}"
        );
    }
    inside
}

#[tokio::test]
async fn an_hgt_upload_serves_cesium_terrain_with_no_extra_step() {
    let state = started_state().await;
    let uploaded = upload_dem(&state, "N47E011.hgt", &hgt_ramp(), None).await;
    assert_eq!(uploaded["asset_type"], "terrain");

    let job = settled_job(&state, job_id_of(&uploaded)).await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    assert_eq!(job.output_format, "quantized-mesh");
    assert!(job.tiles_written > 0);

    let asset_id = asset_id_of(&uploaded);
    let asset = state.db.get_asset(asset_id).await.unwrap().unwrap();
    assert!(matches!(asset.status, AssetStatus::Ready));
    assert_eq!(asset.tile_count, job.tiles_written);

    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    assert_eq!(layer["format"], "quantized-mesh-1.0");
    assert_eq!(layer["scheme"], "tms");
    assert_eq!(layer["projection"], "EPSG:4326");
    assert_eq!(layer["tiles"][0], "{z}/{x}/{y}.terrain?v={version}");
    assert_eq!(layer["bounds"], serde_json::json!([11.0, 47.0, 12.0, 48.0]));
    // cesium fails on a root tile it cannot load
    assert_eq!(
        layer["available"][0],
        serde_json::json!([{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }])
    );
    let available = layer["available"].as_array().unwrap();
    assert_eq!(
        available.len() as u64,
        layer["maxzoom"].as_u64().unwrap() + 1
    );

    // wholly inside the DEM, and the north and east of it high as the file says
    let coord = deepest_tile_at(&layer, 11.5, 47.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    assert!(tile.edge_vertex_counts.iter().all(|&count| count > 0));
    let inside = assert_tile_matches(&tile, &coord, [11.0, 47.0, 12.0, 48.0], hgt_ramp_elevation);
    assert_eq!(inside, tile.u.len());

    // straddling the east edge: the DEM to the west of it, flat ground past it
    let coord = deepest_tile_at(&layer, 11.999, 47.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    let inside = assert_tile_matches(&tile, &coord, [11.0, 47.0, 12.0, 48.0], hgt_ramp_elevation);
    assert!(
        inside > 0 && inside < tile.u.len(),
        "{inside} of {}",
        tile.u.len()
    );

    for root_x in [0, 1] {
        let coord = TerrainTileCoord {
            zoom: 0,
            x: root_x,
            y: 0,
        };
        fetch_tile(&state, &root, &coord).await;
    }

    let listing = editor_request(&state, "GET", "/api/v1/terrain/bundles").await;
    let names: Vec<String> = serde_json::from_slice(&response_bytes(listing).await).unwrap();
    assert_eq!(names, vec![asset_id.to_string()]);

    let listing = anonymous_get(&state, "/api/v1/terrain/bundles").await;
    let names: Vec<String> = serde_json::from_slice(&response_bytes(listing).await).unwrap();
    assert!(names.is_empty(), "{names:?}");
}

#[tokio::test]
async fn a_void_inside_an_hgt_takes_nearby_heights_and_flat_ground_stays_outside() {
    const HGT_VOID: i16 = i16::MIN;
    let state = started_state().await;
    let mut hgt = hgt_ramp();
    let void_sample = (HGT_SIDE / 2) * HGT_SIDE + HGT_SIDE / 2;
    hgt[void_sample * 2..void_sample * 2 + 2].copy_from_slice(&HGT_VOID.to_be_bytes());
    let asset_id = uploaded_and_tiled(&state, "N47E011.hgt", &hgt, None).await;
    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    let dem_bounds = [11.0, 47.0, 12.0, 48.0];

    // the mean of a sample's four neighbours on a ramp is the sample's own height
    let coord = deepest_tile_at(&layer, 11.5, 47.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    let inside = assert_tile_matches(&tile, &coord, dem_bounds, hgt_ramp_elevation);
    assert_eq!(inside, tile.u.len());

    let coord = deepest_tile_at(&layer, 11.999, 47.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    let inside = assert_tile_matches(&tile, &coord, dem_bounds, hgt_ramp_elevation);
    assert!(inside < tile.u.len(), "{inside} of {}", tile.u.len());
}

#[tokio::test]
async fn a_dted_upload_lands_at_its_header_origin() {
    const UHL_BYTES: usize = 80;
    const DSI_AND_ACC_BYTES: usize = 648 + 2700;
    const COLUMN_PREFIX_BYTES: usize = 8;
    const COLUMN_CHECKSUM_BYTES: usize = 4;
    const DTED_DATA_SENTINEL: u8 = 0xAA;

    let last = (HGT_SIDE - 1) as f64;
    let mut uhl = [b' '; UHL_BYTES];
    uhl[..3].copy_from_slice(b"UHL");
    uhl[4..12].copy_from_slice(b"0110000E");
    uhl[12..20].copy_from_slice(b"0470000N");
    // 1125 tenths of an arc-second is 1/32 of a degree
    uhl[20..24].copy_from_slice(b"1125");
    uhl[24..28].copy_from_slice(b"1125");
    uhl[47..51].copy_from_slice(format!("{HGT_SIDE:04}").as_bytes());
    uhl[51..55].copy_from_slice(format!("{HGT_SIDE:04}").as_bytes());

    let mut dted = uhl.to_vec();
    dted.resize(UHL_BYTES + DSI_AND_ACC_BYTES, b' ');
    for column in 0..HGT_SIDE {
        dted.push(DTED_DATA_SENTINEL);
        dted.resize(dted.len() + COLUMN_PREFIX_BYTES - 1, 0);
        // each column runs south to north
        for row in 0..HGT_SIDE {
            let elevation =
                hgt_ramp_elevation(11.0 + column as f64 / last, 47.0 + row as f64 / last);
            dted.extend_from_slice(&(elevation as i16).to_be_bytes());
        }
        dted.resize(dted.len() + COLUMN_CHECKSUM_BYTES, 0);
    }

    let state = started_state().await;
    let asset_id = uploaded_and_tiled(&state, "alps.dt2", &dted, None).await;
    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    assert_eq!(layer["bounds"], serde_json::json!([11.0, 47.0, 12.0, 48.0]));

    let coord = deepest_tile_at(&layer, 11.5, 47.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    let inside = assert_tile_matches(&tile, &coord, [11.0, 47.0, 12.0, 48.0], hgt_ramp_elevation);
    assert_eq!(inside, tile.u.len());
}

fn geotiff(
    side: u32,
    tiepoint: [f64; 2],
    pixel_scale: f64,
    geo_key: Option<(u16, u16)>,
    elevation: impl Fn(u32, u32) -> f32,
) -> Vec<u8> {
    let mut samples = Vec::with_capacity((side * side) as usize);
    for row in 0..side {
        for column in 0..side {
            samples.push(elevation(row, column));
        }
    }

    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder
            .new_image::<colortype::Gray32Float>(side, side)
            .unwrap();
        let tags = image.encoder();
        tags.write_tag(
            Tag::ModelPixelScaleTag,
            &[pixel_scale, pixel_scale, 0.0][..],
        )
        .unwrap();
        tags.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, tiepoint[0], tiepoint[1], 0.0][..],
        )
        .unwrap();
        if let Some((key, code)) = geo_key {
            tags.write_tag(
                Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 1, key, 0, 1, code][..],
            )
            .unwrap();
        }
        image.write_data(&samples).unwrap();
    }
    bytes.into_inner()
}

const GEOGRAPHIC_SIDE: u32 = 41;
const GEOGRAPHIC_SCALE: f64 = 0.025;
const GEOGRAPHIC_WEST: f64 = 7.0;
const GEOGRAPHIC_NORTH: f64 = 44.0;

fn geographic_ramp_elevation(longitude: f64, latitude: f64) -> f64 {
    50.0 + 800.0 * (longitude - GEOGRAPHIC_WEST) + 1600.0 * (GEOGRAPHIC_NORTH - latitude)
}

fn geographic_tiff(geo_key: Option<(u16, u16)>) -> Vec<u8> {
    geotiff(
        GEOGRAPHIC_SIDE,
        [GEOGRAPHIC_WEST, GEOGRAPHIC_NORTH],
        GEOGRAPHIC_SCALE,
        geo_key,
        |row, column| {
            // pixel-is-area sample centres sit half a pixel in from the tiepoint
            geographic_ramp_elevation(
                GEOGRAPHIC_WEST + (column as f64 + 0.5) * GEOGRAPHIC_SCALE,
                GEOGRAPHIC_NORTH - (row as f64 + 0.5) * GEOGRAPHIC_SCALE,
            ) as f32
        },
    )
}

#[tokio::test]
async fn a_wgs84_geotiff_lands_where_its_tags_say() {
    let state = started_state().await;
    let tiff = geographic_tiff(Some((GEOGRAPHIC_TYPE_GEO_KEY, 4326)));
    let asset_id = uploaded_and_tiled(&state, "alps.tif", &tiff, None).await;

    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    let half_pixel = GEOGRAPHIC_SCALE / 2.0;
    let span = GEOGRAPHIC_SCALE * GEOGRAPHIC_SIDE as f64;
    let dem_bounds = [
        GEOGRAPHIC_WEST + half_pixel,
        GEOGRAPHIC_NORTH - span + half_pixel,
        GEOGRAPHIC_WEST + span - half_pixel,
        GEOGRAPHIC_NORTH - half_pixel,
    ];
    for (axis, expected) in dem_bounds.iter().enumerate() {
        let bound = layer["bounds"][axis].as_f64().unwrap();
        assert!((bound - expected).abs() < 1e-9, "{bound} vs {expected}");
    }

    let coord = deepest_tile_at(&layer, 7.5, 43.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    let inside = assert_tile_matches(&tile, &coord, dem_bounds, geographic_ramp_elevation);
    assert!(inside > 0);
}

#[tokio::test]
async fn a_projected_geotiff_is_reprojected_through_projicio() {
    const SIDE: u32 = 51;
    const METRES_PER_SAMPLE: f64 = 100.0;
    const WEST_EASTING: f64 = 500_000.0;
    const NORTH_NORTHING: f64 = 5_300_000.0;
    const UTM_32_NORTH: u16 = 32632;
    const REPROJECTION_TOLERANCE_METRES: f64 = 1.0;
    let elevation_at_easting = |easting: f64| 100.0 + 0.1 * (easting - WEST_EASTING);

    let state = started_state().await;
    let tiff = geotiff(
        SIDE,
        [WEST_EASTING, NORTH_NORTHING],
        METRES_PER_SAMPLE,
        Some((PROJECTED_TYPE_GEO_KEY, UTM_32_NORTH)),
        |_, column| {
            elevation_at_easting(WEST_EASTING + (column as f64 + 0.5) * METRES_PER_SAMPLE) as f32
        },
    );
    let asset_id = uploaded_and_tiled(&state, "utm.tif", &tiff, None).await;

    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    let [west, south, east, north] =
        [0, 1, 2, 3].map(|axis| layer["bounds"][axis].as_f64().unwrap());
    // the central meridian of zone 32 is 9 degrees east, where this grid starts
    assert!(
        (8.9..9.2).contains(&west) && (8.9..9.2).contains(&east),
        "{west} {east}"
    );
    assert!(
        (47.0..48.5).contains(&south) && (47.0..48.5).contains(&north),
        "{south} {north}"
    );

    let coord = deepest_tile_at(&layer, (west + east) / 2.0, (south + north) / 2.0);
    let tile = fetch_tile(&state, &root, &coord).await;
    let to_utm = Transformer::new(CrsDef::Epsg(4326), CrsDef::Epsg(UTM_32_NORTH as u32));
    let mut compared = 0;
    for [longitude, latitude, height] in tile.vertices(&coord) {
        let utm = to_utm
            .transform(Coord3D {
                x: longitude,
                y: latitude,
                z: 0.0,
            })
            .unwrap();
        // one sample in from the source grid on every side, clear of the fill around it
        let inset = 1.5 * METRES_PER_SAMPLE;
        let width = SIDE as f64 * METRES_PER_SAMPLE;
        let in_grid = (WEST_EASTING + inset..WEST_EASTING + width - inset).contains(&utm.x)
            && (NORTH_NORTHING - width + inset..NORTH_NORTHING - inset).contains(&utm.y);
        if !in_grid {
            continue;
        }
        compared += 1;
        let expected = elevation_at_easting(utm.x);
        assert!(
            (height - expected).abs() < REPROJECTION_TOLERANCE_METRES,
            "height {height} at ({longitude}, {latitude}), expected {expected}"
        );
    }
    assert!(compared > 0, "no vertex of {coord:?} fell inside the grid");
}

#[tokio::test]
async fn a_geotiff_with_no_crs_is_refused_until_the_upload_names_one() {
    let state = started_state().await;
    let tiff = geographic_tiff(None);

    let uploaded = upload_dem(&state, "bare.tif", &tiff, None).await;
    let job = settled_job(&state, job_id_of(&uploaded)).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.starts_with("dem_no_crs"), "{error}");

    let asset_id = asset_id_of(&uploaded);
    let asset = state.db.get_asset(asset_id).await.unwrap().unwrap();
    assert!(matches!(asset.status, AssetStatus::Error));

    let response = editor_request(&state, "GET", &format!("/v1/assets/{asset_id}/endpoint")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_slice(&response_bytes(response).await).unwrap();
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("/api/v1/assets/{asset_id}/jobs")),
        "{message}"
    );

    let asset_id = uploaded_and_tiled(&state, "bare.tif", &tiff, Some("EPSG:4326")).await;
    let root = terrain_root(&state, asset_id).await;
    let layer = layer_json(&state, &root).await;
    let coord = deepest_tile_at(&layer, 7.5, 43.5);
    let tile = fetch_tile(&state, &root, &coord).await;
    assert!(tile.max_height > tile.min_height);
}

#[tokio::test]
async fn a_crs_that_is_no_epsg_code_is_refused_naming_it() {
    let state = started_state().await;
    let uploaded = upload_dem(&state, "N47E011.hgt", &hgt_ramp(), Some("utm32")).await;
    let job = settled_job(&state, job_id_of(&uploaded)).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.starts_with("dem_crs_unsupported"), "{error}");
    assert!(error.contains("utm32"), "{error}");
}

#[tokio::test]
async fn a_dem_over_the_sample_cap_fails_its_job_by_name() {
    const OVERSIZED_SIDE: u64 = 4801;
    let state = started_state().await;
    let asset_id = Uuid::new_v4();
    let input_dir = state.data_dir.join(asset_id.to_string()).join("input");
    std::fs::create_dir_all(&input_dir).unwrap();
    let input = input_dir.join("N47E011.hgt");
    std::fs::File::create(&input)
        .unwrap()
        .set_len(2 * OVERSIZED_SIDE * OVERSIZED_SIDE)
        .unwrap();
    state
        .db
        .create_asset(&Asset {
            id: asset_id,
            name: "N47E011.hgt".into(),
            asset_type: AssetType::Terrain,
            status: AssetStatus::Uploading,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: vec![],
            owner_id: None,
        })
        .await
        .unwrap();
    let job = state
        .job_queue
        .submit(
            asset_id,
            input.to_string_lossy().into_owned(),
            ModelPlacement::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();

    let job = settled_job(&state, job.id).await;
    assert_eq!(job.status, JobStatus::Failed);
    let error = job.error.unwrap();
    assert!(error.starts_with("dem_too_large"), "{error}");
    assert!(
        !state
            .data_dir
            .join("terrain_bundles")
            .join(asset_id.to_string())
            .exists()
    );
}

#[tokio::test]
async fn a_retile_rebuilds_the_bundle_and_leaves_no_staging_behind() {
    let state = started_state().await;
    let asset_id = uploaded_and_tiled(&state, "N47E011.hgt", &hgt_ramp(), None).await;
    let bundle_dir = state
        .data_dir
        .join("terrain_bundles")
        .join(asset_id.to_string());
    let stray = bundle_dir.join("stray-file");
    std::fs::write(&stray, b"from the earlier bundle").unwrap();

    let response = editor_request(&state, "POST", &format!("/api/v1/assets/{asset_id}/tile")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: serde_json::Value = serde_json::from_slice(&response_bytes(response).await).unwrap();
    let job = settled_job(
        &state,
        Uuid::parse_str(job["id"].as_str().unwrap()).unwrap(),
    )
    .await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);

    assert!(!stray.exists());
    assert!(bundle_dir.join("layer.json").is_file());
    assert!(
        !state
            .data_dir
            .join(asset_id.to_string())
            .join("terrain_staging")
            .exists()
    );
}

#[tokio::test]
async fn a_rebuild_reads_the_crs_the_upload_named() {
    let state = started_state().await;
    let tiff = geographic_tiff(None);
    let asset_id = uploaded_and_tiled(&state, "bare.tif", &tiff, Some("EPSG:4326")).await;

    let response = editor_request(&state, "POST", &format!("/api/v1/assets/{asset_id}/tile")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: serde_json::Value = serde_json::from_slice(&response_bytes(response).await).unwrap();
    let job = settled_job(
        &state,
        Uuid::parse_str(job["id"].as_str().unwrap()).unwrap(),
    )
    .await;
    assert_eq!(job.status, JobStatus::Done, "{:?}", job.error);
    assert_eq!(job.placement.crs.as_deref(), Some("EPSG:4326"));
}

#[tokio::test]
async fn deleting_the_asset_takes_its_bundle_with_it() {
    let state = started_state().await;
    let asset_id = uploaded_and_tiled(&state, "N47E011.hgt", &hgt_ramp(), None).await;
    let root = terrain_root(&state, asset_id).await;

    let response = editor_request(&state, "DELETE", &format!("/api/v1/assets/{asset_id}")).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let layer = anonymous_get(&state, &format!("{root}layer.json")).await;
    assert_eq!(layer.status(), StatusCode::NOT_FOUND);
    assert!(
        !state
            .data_dir
            .join("terrain_bundles")
            .join(asset_id.to_string())
            .exists()
    );
}
