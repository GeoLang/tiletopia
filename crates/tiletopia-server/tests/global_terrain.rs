mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use common::decode_tile;
use terrano_core::{CogParams, Raster, SampleFormat, write_cog};
use tiletopia_server::global_terrain::{GlobalTerrain, GlobalTerrainSettings};
use tiletopia_server::{AppState, router};
use tiletopia_terrain::global_dem::TerrainTileCoord;
use tower::ServiceExt;

const CELL: &str = "Copernicus_DSM_COG_10_N46_00_E007_00_DEM";
const CELL_PIXELS: usize = 480;
const QUANTIZED_MAX: f64 = 32767.0;
// quantizing truncates, so a decoded vertex sits up to one step off the position its height was sampled at
const POSITION_SLACK_METRES: f64 = 0.01;

fn cell_path() -> String {
    format!("/{CELL}/{CELL}.tif")
}

fn dem_height(latitude: f64, longitude: f64) -> f64 {
    500.0 + 1000.0 * (latitude - 46.0) + 300.0 * (longitude - 7.0)
}

fn geoid_undulation(latitude: f64) -> f64 {
    20.0 + 0.5 * latitude
}

fn float_cog(
    width: usize,
    height: usize,
    origin: (f64, f64),
    pixel: f64,
    overview_levels: u32,
    value_at: impl Fn(usize, usize) -> f64,
) -> Vec<u8> {
    let data = (0..height)
        .flat_map(|row| (0..width).map(move |column| (row, column)))
        .map(|(row, column)| value_at(row, column))
        .collect();
    let raster = Raster::from_vec(width, height, data, pixel, f64::NAN).unwrap();
    let params = CogParams {
        tile_width: 128,
        tile_height: 128,
        overview_levels,
        epsg: 4326,
        origin_x: origin.0,
        origin_y: origin.1,
        pixel_width: pixel,
        pixel_height: pixel,
        deflate: true,
        nodata: None,
        format: SampleFormat::F32,
    };
    let mut bytes = Vec::new();
    write_cog(&raster, &params, &mut bytes).unwrap();
    bytes
}

fn dem_samples(origin: (f64, f64)) -> Vec<u8> {
    let pixel = 1.0 / CELL_PIXELS as f64;
    float_cog(CELL_PIXELS, CELL_PIXELS, origin, pixel, 2, |row, column| {
        dem_height(47.0 - row as f64 * pixel, 7.0 + column as f64 * pixel)
    })
}

fn geoid_samples(origin: (f64, f64)) -> Vec<u8> {
    float_cog(360, 181, origin, 1.0, 0, |row, _| {
        geoid_undulation(90.0 - row as f64)
    })
}

fn dem_cell() -> Vec<u8> {
    let half_pixel = 0.5 / CELL_PIXELS as f64;
    // terrano's write_cog tags PixelIsArea, whose origin is the corner of the first pixel
    dem_samples((7.0 - half_pixel, 47.0 + half_pixel))
}

fn geoid_grid() -> Vec<u8> {
    // terrano's write_cog tags PixelIsArea, whose origin is the corner of the first pixel
    geoid_samples((-180.5, 90.5))
}

fn coarse_dem(value_at: impl Fn(f64, f64) -> f64) -> Vec<u8> {
    let pixel = 1.0;
    float_cog(360, 181, (-180.5, 90.5), pixel, 0, |row, column| {
        let latitude = 90.5 - (row as f64 + 0.5) * pixel;
        let longitude = -180.5 + (column as f64 + 0.5) * pixel;
        value_at(latitude, longitude)
    })
}

const RASTER_TYPE_GEO_KEY: u16 = 1025;
const GEO_KEY_DIRECTORY_TAG: u16 = 34735;

fn marked_pixel_is_point(mut cog: Vec<u8>) -> Vec<u8> {
    let u16_at = |at: usize| usize::from(u16::from_le_bytes([cog[at], cog[at + 1]]));
    let u32_at = |at: usize| u32::from_le_bytes(cog[at..at + 4].try_into().unwrap()) as usize;
    let first_ifd = u32_at(4);
    let directory_entry = (0..u16_at(first_ifd))
        .map(|entry| first_ifd + 2 + entry * 12)
        .find(|&at| u16_at(at) == usize::from(GEO_KEY_DIRECTORY_TAG))
        .unwrap();
    let keys = u32_at(directory_entry + 8);
    let raster_type_key = (0..u16_at(keys + 6))
        .map(|key| keys + 8 + key * 8)
        .find(|&at| u16_at(at) == usize::from(RASTER_TYPE_GEO_KEY))
        .unwrap();
    // geotiff RasterPixelIsPoint is value 2 of GTRasterTypeGeoKey
    cog[raster_type_key + 6..raster_type_key + 8].copy_from_slice(&2u16.to_le_bytes());
    cog
}

type ServedRequests = Arc<Mutex<Vec<(String, u64)>>>;

fn requested_range(headers: &HeaderMap) -> (u64, u64) {
    let value = headers[header::RANGE].to_str().unwrap();
    let (first, last) = value
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    (first.parse().unwrap(), last.parse().unwrap())
}

async fn serve_files(files: HashMap<String, Vec<u8>>) -> (String, ServedRequests) {
    let requests: ServedRequests = Arc::default();
    let recorder = Arc::clone(&requests);
    let files = Arc::new(files);
    let app = axum::Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
        let files = Arc::clone(&files);
        let recorder = Arc::clone(&recorder);
        async move {
            let path = uri.path().to_string();
            let (first, last) = requested_range(&headers);
            let Some(bytes) = files.get(&path) else {
                recorder.lock().unwrap().push((path, 0));
                return (StatusCode::NOT_FOUND, HeaderMap::new(), Vec::new());
            };
            let last = last.min(bytes.len() as u64 - 1);
            recorder.lock().unwrap().push((path, last - first + 1));
            let mut answer = HeaderMap::new();
            answer.insert(
                header::CONTENT_RANGE,
                format!("bytes {first}-{last}/{}", bytes.len())
                    .parse()
                    .unwrap(),
            );
            (
                StatusCode::PARTIAL_CONTENT,
                answer,
                bytes[first as usize..=last as usize].to_vec(),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), requests)
}

struct Harness {
    state: Arc<AppState>,
    requests: ServedRequests,
    dem_cell_bytes: usize,
    cache: tempfile::TempDir,
}

impl Harness {
    async fn new(cache_bytes: u64, ion_asset_id: Option<i64>) -> Harness {
        Harness::serving(dem_cell(), geoid_grid(), cache_bytes, ion_asset_id).await
    }

    async fn serving(
        dem_cell: Vec<u8>,
        geoid_grid: Vec<u8>,
        cache_bytes: u64,
        ion_asset_id: Option<i64>,
    ) -> Harness {
        Self::open(dem_cell, geoid_grid, None, cache_bytes, ion_asset_id).await
    }

    async fn open(
        dem_cell: Vec<u8>,
        geoid_grid: Vec<u8>,
        coarse: Option<Vec<u8>>,
        cache_bytes: u64,
        ion_asset_id: Option<i64>,
    ) -> Harness {
        let dem_cell_bytes = dem_cell.len();
        let mut files = HashMap::from([
            (cell_path(), dem_cell),
            ("/geoid.tif".to_string(), geoid_grid),
        ]);
        let coarse_is_set = coarse.is_some();
        if let Some(bytes) = coarse {
            files.insert("/coarse.tif".to_string(), bytes);
        }
        let (base, requests) = serve_files(files).await;
        let cache = tempfile::tempdir().unwrap();
        let global_terrain = GlobalTerrain::new(GlobalTerrainSettings {
            dem_url: Some(base.clone()),
            geoid_url: Some(format!("{base}/geoid.tif")),
            coarse_url: coarse_is_set.then(|| format!("{base}/coarse.tif")),
            cache_dir: Some(cache.path().to_path_buf()),
            cache_bytes,
            ion_asset_id,
        });
        Harness {
            state: common::test_state_with_global_terrain(global_terrain).await,
            requests,
            dem_cell_bytes,
            cache,
        }
    }

    fn requests_for(&self, path: &str) -> Vec<u64> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(requested, _)| requested == path)
            .map(|(_, length)| *length)
            .collect()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    async fn get(&self, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = router(Arc::clone(&self.state))
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body.to_vec())
    }

    async fn tile(&self, coord: &TerrainTileCoord) -> Vec<u8> {
        let uri = format!(
            "/api/v1/terrain/global/{}/{}/{}.terrain",
            coord.zoom, coord.x, coord.y
        );
        let (status, body) = self.get(&uri).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{uri}: {}",
            String::from_utf8_lossy(&body)
        );
        body
    }
}

fn tile_containing(zoom: u32, longitude: f64, latitude: f64) -> TerrainTileCoord {
    let span = 180.0 / 2f64.powi(zoom as i32);
    TerrainTileCoord {
        zoom,
        x: ((longitude + 180.0) / span) as u32,
        y: ((latitude + 90.0) / span) as u32,
    }
}

fn assert_heights(bytes: &[u8], coord: &TerrainTileCoord, expected: impl Fn(f64, f64) -> f64) {
    let tile = decode_tile(bytes);
    let tolerance = (tile.max_height - tile.min_height) / QUANTIZED_MAX + POSITION_SLACK_METRES;
    for [longitude, latitude, height] in tile.vertices(coord) {
        let want = expected(latitude, longitude);
        assert!(
            (height - want).abs() <= tolerance,
            "height {height} at ({longitude}, {latitude}), expected {want} within {tolerance}"
        );
    }
}

// rows 31 to 117 and columns 15 to 100 of the cell, inside its first 128-pixel tile
const INSIDE_CELL: TerrainTileCoord = TerrainTileCoord {
    zoom: 10,
    x: 1064,
    y: 778,
};

#[tokio::test(flavor = "multi_thread")]
async fn a_tile_reads_one_window_and_lands_on_the_ellipsoid() {
    let harness = Harness::new(1 << 30, None).await;
    let bytes = harness.tile(&INSIDE_CELL).await;

    let cell_reads = harness.requests_for(&cell_path());
    assert_eq!(
        cell_reads.len(),
        2,
        "a header read and one window read: {cell_reads:?}"
    );
    assert!(
        (cell_reads[1] as usize) < harness.dem_cell_bytes / 4,
        "the window read {} of {} bytes",
        cell_reads[1],
        harness.dem_cell_bytes
    );
    assert!(harness.requests_for("/geoid.tif").len() <= 2);

    assert_heights(&bytes, &INSIDE_CELL, |latitude, longitude| {
        dem_height(latitude, longitude) + geoid_undulation(latitude)
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn pixel_is_point_files_keep_their_samples_on_the_tiepoint() {
    let harness = Harness::serving(
        marked_pixel_is_point(dem_samples((7.0, 47.0))),
        marked_pixel_is_point(geoid_samples((-180.0, 90.0))),
        1 << 30,
        None,
    )
    .await;
    let bytes = harness.tile(&INSIDE_CELL).await;
    assert_heights(&bytes, &INSIDE_CELL, |latitude, longitude| {
        dem_height(latitude, longitude) + geoid_undulation(latitude)
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_request_is_served_from_the_disk_cache() {
    let harness = Harness::new(1 << 30, None).await;
    let first = harness.tile(&INSIDE_CELL).await;
    let reads_after_first = harness.request_count();

    let second = harness.tile(&INSIDE_CELL).await;
    assert_eq!(harness.request_count(), reads_after_first);
    assert_eq!(first, second);
    assert!(harness.cache.path().join("10/1064/778.terrain").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn ground_with_no_cell_is_sea_level_on_the_geoid() {
    let harness = Harness::new(1 << 30, None).await;
    let ocean =
        "/Copernicus_DSM_COG_10_N46_00_E010_00_DEM/Copernicus_DSM_COG_10_N46_00_E010_00_DEM.tif";

    let coord = tile_containing(10, 10.1, 46.8);
    let bytes = harness.tile(&coord).await;
    assert_heights(&bytes, &coord, |latitude, _| geoid_undulation(latitude));
    assert_eq!(harness.requests_for(ocean).len(), 1);

    let neighbour = TerrainTileCoord {
        x: coord.x + 1,
        ..coord
    };
    harness.tile(&neighbour).await;
    assert_eq!(
        harness.requests_for(ocean).len(),
        1,
        "a missing cell is asked for once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn layer_json_advertises_the_levels_served_and_the_copernicus_notice() {
    let harness = Harness::new(1 << 30, None).await;
    let (status, body) = harness.get("/api/v1/terrain/global/layer.json").await;
    assert_eq!(status, StatusCode::OK);
    let layer: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        layer["attribution"]
            .as_str()
            .unwrap()
            .contains("produced using Copernicus WorldDEM-30")
    );
    let available = layer["available"].as_array().unwrap();
    for (level, ranges) in available.iter().enumerate() {
        let (x_tiles, y_tiles) = TerrainTileCoord::grid_at_zoom(level as u32);
        assert_eq!(ranges[0]["endX"], x_tiles - 1);
        assert_eq!(ranges[0]["endY"], y_tiles - 1);
    }

    let deepest = (available.len() - 1) as u32;
    assert_eq!(layer["maxzoom"], deepest);
    harness.tile(&tile_containing(deepest, 7.1, 46.85)).await;
    let past = tile_containing(deepest + 1, 7.1, 46.85);
    let (status, _) = harness
        .get(&format!(
            "/api/v1/terrain/global/{}/{}/{}.terrain",
            past.zoom, past.x, past.y
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn levels_below_seven_carry_the_geoid_alone() {
    let harness = Harness::new(1 << 30, None).await;
    let coord = tile_containing(3, 7.5, 46.5);
    let bytes = harness.tile(&coord).await;
    assert_heights(&bytes, &coord, |latitude, _| geoid_undulation(latitude));
    assert!(harness.requests_for(&cell_path()).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn levels_below_seven_read_the_coarse_dem() {
    let harness = Harness::open(
        dem_cell(),
        geoid_grid(),
        Some(coarse_dem(dem_height)),
        1 << 30,
        None,
    )
    .await;
    let coord = tile_containing(3, 7.5, 46.5);
    let bytes = harness.tile(&coord).await;
    assert_heights(&bytes, &coord, |latitude, longitude| {
        dem_height(latitude, longitude) + geoid_undulation(latitude)
    });
    assert!(
        harness.requests_for(&cell_path()).is_empty(),
        "a coarse tile does not open one-degree cells"
    );
    assert!(!harness.requests_for("/coarse.tif").is_empty());

    let (status, body) = harness.get("/api/v1/terrain/global/layer.json").await;
    assert_eq!(status, StatusCode::OK);
    let layer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        layer["description"]
            .as_str()
            .unwrap()
            .contains("configured coarse DEM"),
        "{}",
        layer["description"]
    );

    let detailed = harness.tile(&INSIDE_CELL).await;
    assert_heights(&detailed, &INSIDE_CELL, |latitude, longitude| {
        dem_height(latitude, longitude) + geoid_undulation(latitude)
    });
    assert_eq!(harness.requests_for(&cell_path()).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_in_the_coarse_dem_is_sea_level_on_the_geoid() {
    let harness = Harness::open(
        dem_cell(),
        geoid_grid(),
        Some(coarse_dem(|_, _| f64::NAN)),
        1 << 30,
        None,
    )
    .await;
    let coord = tile_containing(3, 7.5, 46.5);
    let bytes = harness.tile(&coord).await;
    assert_heights(&bytes, &coord, |latitude, _| geoid_undulation(latitude));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_coarse_dem_that_does_not_cover_the_globe_names_the_file() {
    let local = float_cog(8, 8, (7.0, 47.0), 0.01, 0, |_, _| 10.0);
    let harness = Harness::open(dem_cell(), geoid_grid(), Some(local), 1 << 30, None).await;
    let coord = tile_containing(3, 7.5, 46.5);
    let (status, body) = harness
        .get(&format!(
            "/api/v1/terrain/global/{}/{}/{}.terrain",
            coord.zoom, coord.x, coord.y
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let message = String::from_utf8_lossy(&body);
    assert!(message.contains("/coarse.tif"), "{message}");
    assert!(message.contains("does not cover the globe"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cache_drops_the_oldest_tile_past_its_cap() {
    let first = TerrainTileCoord {
        zoom: 2,
        x: 0,
        y: 0,
    };
    let second = TerrainTileCoord {
        zoom: 2,
        x: 1,
        y: 0,
    };
    let tile_bytes = Harness::new(1 << 30, None).await.tile(&first).await.len() as u64;

    let harness = Harness::new(tile_bytes + tile_bytes / 2, None).await;
    harness.tile(&first).await;
    harness.tile(&second).await;
    assert!(!harness.cache.path().join("2/0/0.terrain").exists());
    assert!(harness.cache.path().join("2/1/0.terrain").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconfigured_layer_names_what_is_missing() {
    const DEM_URL: &str = "TILETOPIA_GLOBAL_TERRAIN_DEM_URL";
    const GEOID_URL: &str = "TILETOPIA_GLOBAL_TERRAIN_GEOID_URL";
    const CACHE_DIR: &str = "TILETOPIA_GLOBAL_TERRAIN_CACHE_DIR";

    let layer_refusal = |state: Arc<AppState>| async move {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/terrain/global/layer.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).into_owned()
    };

    let message = layer_refusal(common::test_state().await).await;
    for variable in [DEM_URL, GEOID_URL, CACHE_DIR] {
        assert!(message.contains(variable), "{variable} in {message}");
    }

    let partly_configured = GlobalTerrain::new(GlobalTerrainSettings {
        dem_url: Some("http://dem.test".to_string()),
        geoid_url: None,
        coarse_url: None,
        cache_dir: None,
        cache_bytes: 1 << 30,
        ion_asset_id: None,
    });
    let message =
        layer_refusal(common::test_state_with_global_terrain(partly_configured).await).await;
    assert!(!message.contains(DEM_URL), "{message}");
    assert!(message.contains(GEOID_URL), "{message}");
    assert!(message.contains(CACHE_DIR), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_configured_ion_id_resolves_to_the_global_layer() {
    let harness = Harness::new(1 << 30, Some(4242)).await;
    let (status, body) = harness.get("/v1/assets/4242/endpoint").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let endpoint: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(endpoint["type"], "TERRAIN");
    assert!(
        endpoint["url"]
            .as_str()
            .unwrap()
            .ends_with("/api/v1/terrain/global/")
    );
    assert!(
        endpoint["url"]
            .as_str()
            .unwrap()
            .starts_with(common::TEST_ION_BASE_URL)
    );
    let attribution = &endpoint["attributions"][0];
    assert!(
        attribution["html"]
            .as_str()
            .unwrap()
            .contains("Copernicus WorldDEM-30")
    );
    assert_eq!(attribution["collapsible"], false);

    // cesium appends layer.json to the endpoint url
    let url = endpoint["url"].as_str().unwrap();
    let root = url.strip_prefix(common::TEST_ION_BASE_URL).unwrap();
    let (status, body) = harness.get(&format!("{root}layer.json")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let layer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(layer["format"], "quantized-mesh-1.0");
    let tile_template = layer["tiles"][0].as_str().unwrap();
    let tile_path = tile_template
        .replace("{z}", &INSIDE_CELL.zoom.to_string())
        .replace("{x}", &INSIDE_CELL.x.to_string())
        .replace("{y}", &INSIDE_CELL.y.to_string())
        .replace("{version}", layer["version"].as_str().unwrap());
    let (status, tile) = harness.get(&format!("{root}{tile_path}")).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&tile));
    assert_heights(&tile, &INSIDE_CELL, |latitude, longitude| {
        dem_height(latitude, longitude) + geoid_undulation(latitude)
    });

    let (status, _) = harness.get("/v1/assets/4243/endpoint").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn uploaded_ion_id(state: &Arc<AppState>) -> i64 {
    let boundary = "tiletopiaglobalterrainboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"cloud.ply\"\r\n\r\n{}\r\n--{boundary}--\r\n",
        common::PLY_FIXTURE
    );
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(
                    "authorization",
                    format!("Bearer {}", common::token("terrain-uploader", "editor")),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let uploaded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{}", uploaded["id"].as_str().unwrap()))
                .header(
                    "authorization",
                    format!("Bearer {}", common::token("terrain-uploader", "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let ion_asset: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    ion_asset["id"].as_i64().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn ion_id_one_stays_the_global_layer_after_the_first_upload() {
    let harness = Harness::new(1 << 30, Some(1)).await;

    assert_eq!(uploaded_ion_id(&harness.state).await, 2);
    assert_eq!(uploaded_ion_id(&harness.state).await, 3);

    let (status, body) = harness.get("/v1/assets/1/endpoint").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let endpoint: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(endpoint["type"], "TERRAIN");
    assert!(
        endpoint["url"]
            .as_str()
            .unwrap()
            .ends_with("/api/v1/terrain/global/")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_id_counter_steps_over_a_reserved_id_it_reaches_later() {
    let harness = Harness::new(1 << 30, Some(2)).await;

    assert_eq!(uploaded_ion_id(&harness.state).await, 1);
    assert_eq!(uploaded_ion_id(&harness.state).await, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_an_ion_id_an_asset_already_holds() {
    let state = common::test_state().await;
    assert_eq!(uploaded_ion_id(&state).await, 1);
    let holder = state.db.get_asset_by_ion_id(1).await.unwrap().unwrap().0;

    let global_terrain = GlobalTerrain::new(GlobalTerrainSettings {
        dem_url: None,
        geoid_url: None,
        coarse_url: None,
        cache_dir: None,
        cache_bytes: 1 << 30,
        ion_asset_id: Some(1),
    });
    let refusal = global_terrain
        .reserve_ion_asset_id(&state.db)
        .await
        .unwrap_err();

    assert!(
        refusal.contains("TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID"),
        "{refusal}"
    );
    assert!(refusal.contains(&holder.id.to_string()), "{refusal}");
    assert!(refusal.contains("cloud.ply"), "{refusal}");
}
