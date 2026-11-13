mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tiletopia_core::spatial::geodetic_to_ecef;
use tiletopia_server::global_terrain::{GlobalTerrain, GlobalTerrainSettings};
use tiletopia_server::osm_buildings::{OsmBuildings, OsmBuildingsSettings, TILESET_PATH};
use tiletopia_server::{AppState, router};
use tower::ServiceExt;

const LEAF_LEVEL: u32 = 15;
// f32 vertices within a kilometre of the tile centre round to well under a millimetre
const POSITION_SLACK_METRES: f64 = 0.01;

struct Served {
    state: Arc<AppState>,
    queries: common::OverpassQueries,
    _cache: tempfile::TempDir,
}

async fn serve(
    failures_first: Vec<StatusCode>,
    answer: Value,
    global_terrain: GlobalTerrain,
    ion_asset_id: Option<i64>,
) -> Served {
    let (overpass_url, queries) = common::serve_overpass(failures_first, answer).await;
    let cache = tempfile::tempdir().unwrap();
    let osm_buildings = OsmBuildings::new(
        OsmBuildingsSettings {
            overpass_url: Some(overpass_url),
            cache_dir: Some(cache.path().to_path_buf()),
            cache_bytes: 1 << 30,
            ion_asset_id,
        },
        &global_terrain,
    );
    Served {
        state: common::test_state_with_global_layers(global_terrain, osm_buildings).await,
        queries,
        _cache: cache,
    }
}

async fn get(state: &Arc<AppState>, uri: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = router(Arc::clone(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_string());
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, content_type, body.to_vec())
}

struct Leaf {
    x: u32,
    y: u32,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
}

fn leaf_containing(longitude: f64, latitude: f64) -> Leaf {
    let tiles = f64::from(1u32 << LEAF_LEVEL);
    let (width, height) = (360.0 / tiles, 180.0 / tiles);
    let x = ((longitude + 180.0) / width).floor() as u32;
    let y = ((latitude + 90.0) / height).floor() as u32;
    let west = -180.0 + f64::from(x) * width;
    let south = -90.0 + f64::from(y) * height;
    Leaf {
        x,
        y,
        west,
        south,
        east: west + width,
        north: south + height,
    }
}

fn square_around(longitude: f64, latitude: f64) -> Vec<[f64; 2]> {
    let half = 0.0001;
    vec![
        [longitude - half, latitude - half],
        [longitude - half, latitude + half],
        [longitude + half, latitude + half],
        [longitude + half, latitude - half],
    ]
}

fn assert_has_corner(positions: &[[f64; 3]], [longitude, latitude]: [f64; 2], height: f64) {
    let expected = geodetic_to_ecef(latitude.to_radians(), longitude.to_radians(), height);
    let nearest = positions
        .iter()
        .map(|position| {
            ((position[0] - expected[0]).powi(2)
                + (position[1] - expected[1]).powi(2)
                + (position[2] - expected[2]).powi(2))
            .sqrt()
        })
        .fold(f64::INFINITY, f64::min);
    assert!(
        nearest < POSITION_SLACK_METRES,
        "no vertex at {longitude}, {latitude}, {height} m: nearest is {nearest} m away"
    );
}

fn glb_json(bytes: &[u8]) -> Value {
    let json_length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    serde_json::from_slice(&bytes[20..20 + json_length]).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leaf_extrudes_its_buildings_on_the_ellipsoid_and_reads_the_cache_after() {
    let leaf = leaf_containing(7.45, 46.95);
    let centre = [
        (leaf.west + leaf.east) * 0.5,
        (leaf.south + leaf.north) * 0.5,
    ];
    let ring = square_around(centre[0], centre[1]);
    let answer = common::overpass_buildings(&[(
        &ring,
        json!({ "building": "office", "height": "12 m", "building:colour": "#A0B0C0", "name": "Tower" }),
    )]);
    let served = serve(
        Vec::new(),
        answer,
        common::unconfigured_global_terrain(),
        None,
    )
    .await;
    let uri = format!(
        "/api/v1/buildings/osm/content/{LEAF_LEVEL}/{}/{}.glb",
        leaf.x, leaf.y
    );

    let (status, content_type, first) = get(&served.state, &uri).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&first)
    );
    assert_eq!(content_type.as_deref(), Some("model/gltf-binary"));
    let positions = common::glb_earth_centred_positions(&first);
    for &corner in &ring {
        assert_has_corner(&positions, corner, 0.0);
        assert_has_corner(&positions, corner, 12.0);
    }
    let metadata = &glb_json(&first)["extensions"]["EXT_structural_metadata"];
    assert_eq!(metadata["propertyTables"][0]["count"], 1);
    assert!(metadata["propertyTables"][0]["properties"]["cesium#color"].is_object());

    let queries = served.queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 1);
    assert!(
        queries[0].contains(&format!(
            "way[\"building\"]({},{},{},{})",
            leaf.south, leaf.west, leaf.north, leaf.east
        )),
        "{}",
        queries[0]
    );

    let (status, _, second) = get(&served.state, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second, first);
    assert_eq!(served.queries.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_leaf_with_no_buildings_is_a_gltf_with_no_meshes() {
    let served = serve(
        Vec::new(),
        common::overpass_buildings(&[]),
        common::unconfigured_global_terrain(),
        None,
    )
    .await;
    let (status, _, body) = get(
        &served.state,
        &format!("/api/v1/buildings/osm/content/{LEAF_LEVEL}/100/200.glb"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let gltf = gltf::Gltf::from_slice(&body).unwrap();
    assert_eq!(gltf.meshes().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tileset_is_one_implicit_quadtree_over_the_globe() {
    let served = serve(
        Vec::new(),
        common::overpass_buildings(&[]),
        common::unconfigured_global_terrain(),
        None,
    )
    .await;
    let (status, _, body) = get(&served.state, TILESET_PATH).await;
    assert_eq!(status, StatusCode::OK);
    let tileset: Value = serde_json::from_slice(&body).unwrap();
    let root = &tileset["root"];
    let region: Vec<f64> =
        serde_json::from_value(root["boundingVolume"]["region"].clone()).unwrap();
    assert_eq!(
        region[..4],
        [
            -std::f64::consts::PI,
            -std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
            std::f64::consts::FRAC_PI_2
        ]
    );
    assert_eq!(root["implicitTiling"]["subdivisionScheme"], "QUADTREE");
    assert_eq!(root["implicitTiling"]["availableLevels"], LEAF_LEVEL + 1);
    assert_eq!(root["content"]["uri"], "content/{level}/{x}/{y}.glb");
    assert!(
        tileset["extras"]["ground"]
            .as_str()
            .unwrap()
            .contains("ellipsoid")
    );

    let subtree_levels = root["implicitTiling"]["subtreeLevels"].as_u64().unwrap() as u32;
    let mut level = 0;
    while level < LEAF_LEVEL {
        let (status, _, bytes) = get(
            &served.state,
            &format!("/api/v1/buildings/osm/subtrees/{level}/0/0.subtree"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "subtree at level {level}");
        assert_eq!(&bytes[..4], b"subt");
        level += subtree_levels;
    }
    for uri in [
        "/api/v1/buildings/osm/subtrees/3/0/0.subtree",
        "/api/v1/buildings/osm/subtrees/16/0/0.subtree",
        "/api/v1/buildings/osm/subtrees/4/16/0.subtree",
        "/api/v1/buildings/osm/content/14/0/0.glb",
    ] {
        assert_eq!(
            get(&served.state, uri).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unconfigured_layer_names_what_is_missing() {
    let state = common::test_state().await;
    let (status, _, body) = get(&state, TILESET_PATH).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let message = String::from_utf8(body).unwrap();
    assert!(
        message.contains("TILETOPIA_OSM_BUILDINGS_OVERPASS_URL"),
        "{message}"
    );
    assert!(
        message.contains("TILETOPIA_OSM_BUILDINGS_CACHE_DIR"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_buildings_ion_id_resolves_without_a_token_and_new_assets_skip_both_reserved_ids() {
    let global_terrain = GlobalTerrain::new(GlobalTerrainSettings {
        dem_url: None,
        geoid_url: None,
        coarse_url: None,
        cache_dir: None,
        cache_bytes: 1 << 30,
        ion_asset_id: Some(1),
    });
    let served = serve(
        Vec::new(),
        common::overpass_buildings(&[]),
        global_terrain,
        Some(2),
    )
    .await;

    let (status, _, body) = get(&served.state, "/v1/assets/2/endpoint").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let endpoint: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(endpoint["type"], "3DTILES");
    assert!(endpoint["url"].as_str().unwrap().ends_with(TILESET_PATH));
    assert!(
        endpoint["attributions"][0]["html"]
            .as_str()
            .unwrap()
            .contains("OpenStreetMap")
    );

    assert_eq!(common::uploaded_ion_id(&served.state).await, 3);
    assert_eq!(
        get(&served.state, "/v1/assets/3/endpoint").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_id_counter_steps_over_a_run_of_reserved_ids() {
    let state = common::test_state().await;
    tiletopia_server::ion_compat::reserve_ion_ids(
        &state.db,
        &[("FIRST", Some(2)), ("SECOND", Some(3))],
    )
    .await
    .unwrap();
    assert_eq!(common::uploaded_ion_id(&state).await, 1);
    assert_eq!(common::uploaded_ion_id(&state).await, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_refuses_one_ion_id_for_both_layers() {
    let state = common::test_state().await;
    let refusal = tiletopia_server::ion_compat::reserve_ion_ids(
        &state.db,
        &[
            (tiletopia_server::global_terrain::ION_ASSET_ID_VAR, Some(7)),
            (tiletopia_server::osm_buildings::ION_ASSET_ID_VAR, Some(7)),
        ],
    )
    .await
    .unwrap_err();
    assert!(
        refusal.contains("TILETOPIA_GLOBAL_TERRAIN_ION_ASSET_ID"),
        "{refusal}"
    );
    assert!(
        refusal.contains("TILETOPIA_OSM_BUILDINGS_ION_ASSET_ID"),
        "{refusal}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_overpass_is_asked_once_more() {
    let leaf = leaf_containing(7.45, 46.95);
    let ring = square_around(
        (leaf.west + leaf.east) * 0.5,
        (leaf.south + leaf.north) * 0.5,
    );
    let served = serve(
        vec![StatusCode::BAD_GATEWAY],
        common::overpass_buildings(&[(&ring, json!({ "building": "yes" }))]),
        common::unconfigured_global_terrain(),
        None,
    )
    .await;
    let uri = format!(
        "/api/v1/buildings/osm/content/{LEAF_LEVEL}/{}/{}.glb",
        leaf.x, leaf.y
    );

    let (status, _, body) = get(&served.state, &uri).await;

    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(served.queries.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn overpass_busy_twice_answers_503_and_a_server_error_is_not_retried() {
    let leaf = leaf_containing(7.45, 46.95);
    let uri = format!(
        "/api/v1/buildings/osm/content/{LEAF_LEVEL}/{}/{}.glb",
        leaf.x, leaf.y
    );
    let busy = serve(
        vec![StatusCode::GATEWAY_TIMEOUT, StatusCode::TOO_MANY_REQUESTS],
        common::overpass_buildings(&[]),
        common::unconfigured_global_terrain(),
        None,
    )
    .await;
    let broken = serve(
        vec![StatusCode::INTERNAL_SERVER_ERROR],
        common::overpass_buildings(&[]),
        common::unconfigured_global_terrain(),
        None,
    )
    .await;

    let (status, _, body) = get(&busy.state, &uri).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&body).contains("429"));
    assert_eq!(busy.queries.lock().unwrap().len(), 2);

    let (status, _, _) = get(&broken.state, &uri).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(broken.queries.lock().unwrap().len(), 1);
}
