mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{test_state, test_state_with_global_terrain, token};
use serde_json::{Value, json};
use tiletopia_server::global_terrain::{GlobalTerrain, GlobalTerrainSettings};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const GDAL_COG: &[u8] = include_bytes!("fixtures/imagery_rgb_gdal_cog.tif");
const CZML_DOCUMENT: &[u8] = br#"[{"id":"document","version":"1.0"}]"#;
const BUNDLE_LAYER_JSON: &str =
    r#"{"tilejson":"2.1.0","format":"quantized-mesh-1.0","version":"1.0.0","available":[]}"#;

const OWNER: &str = "owner-subject";
const OTHER: &str = "other-subject";
const ADMIN: &str = "admin-subject";

struct Answer {
    status: StatusCode,
    cache_control: Option<String>,
    vary: Option<String>,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap()
    }
}

async fn get(state: &Arc<AppState>, uri: &str, bearer: Option<&str>) -> Answer {
    let mut request = Request::builder().uri(uri);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let response = router(Arc::clone(state))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let header_text = |name: header::HeaderName| {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    let status = response.status();
    let cache_control = header_text(header::CACHE_CONTROL);
    let vary = header_text(header::VARY);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        cache_control,
        vary,
        bytes,
    }
}

fn owner_token() -> String {
    token(OWNER, "editor")
}

fn other_token() -> String {
    token(OTHER, "editor")
}

fn admin_token() -> String {
    token(ADMIN, "admin")
}

async fn seed_asset(
    state: &Arc<AppState>,
    owner: Option<&str>,
    asset_type: AssetType,
) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let ion_id = state
        .db
        .create_asset(&Asset {
            id,
            name: format!("named-by-{}", owner.unwrap_or("nobody")),
            asset_type,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: Vec::new(),
            owner_id: owner.map(str::to_string),
        })
        .await
        .unwrap();
    (id, ion_id)
}

async fn asset_read_routes(state: &Arc<AppState>, id: Uuid, ion_id: i64) -> Vec<String> {
    let job = state
        .job_queue
        .submit(
            id,
            "cloud.ply".to_string(),
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .unwrap();
    vec![
        format!("/v1/assets/{ion_id}"),
        format!("/v1/assets/{id}"),
        format!("/v1/assets/{ion_id}/endpoint"),
        format!("/v1/assets/{id}/endpoint"),
        format!("/api/v1/assets/{id}"),
        format!("/api/v1/assets/{id}/jobs"),
        format!("/api/v1/assets/{id}/annotations"),
        format!("/api/v1/jobs/{}", job.id),
    ]
}

#[tokio::test]
async fn anonymous_is_refused_on_every_asset_read() {
    let state = test_state().await;
    let (id, ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    for uri in asset_read_routes(&state, id, ion_id).await {
        assert_eq!(
            get(&state, &uri, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    for uri in ["/v1/assets", "/api/v1/assets"] {
        assert_eq!(
            get(&state, uri, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn another_owner_gets_a_404_on_every_asset_read() {
    let state = test_state().await;
    let (id, ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    for uri in asset_read_routes(&state, id, ion_id).await {
        assert_eq!(
            get(&state, &uri, Some(&other_token())).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn the_owner_and_an_admin_read_every_asset_route() {
    let state = test_state().await;
    let (id, ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    for uri in asset_read_routes(&state, id, ion_id).await {
        for bearer in [owner_token(), admin_token()] {
            let answer = get(&state, &uri, Some(&bearer)).await;
            assert_eq!(
                answer.status,
                StatusCode::OK,
                "{uri}: {}",
                String::from_utf8_lossy(&answer.bytes)
            );
        }
    }
}

#[tokio::test]
async fn another_owners_asset_answers_like_a_missing_one() {
    let state = test_state().await;
    let (_, ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    let missing_ion_id = ion_id + 1000;

    let theirs = get(
        &state,
        &format!("/v1/assets/{ion_id}/endpoint"),
        Some(&other_token()),
    )
    .await;
    let missing = get(
        &state,
        &format!("/v1/assets/{missing_ion_id}/endpoint"),
        Some(&other_token()),
    )
    .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        theirs.json(),
        json!({ "message": format!("no asset {ion_id}") })
    );
    assert_eq!(
        missing.json(),
        json!({ "message": format!("no asset {missing_ion_id}") })
    );

    let theirs = get(
        &state,
        &format!("/v1/assets/{ion_id}"),
        Some(&other_token()),
    )
    .await;
    let missing = get(
        &state,
        &format!("/v1/assets/{missing_ion_id}"),
        Some(&other_token()),
    )
    .await;
    assert_eq!(theirs.status, missing.status);
    assert_eq!(theirs.bytes, missing.bytes);
}

#[tokio::test]
async fn the_ion_list_holds_only_what_the_caller_may_view() {
    let state = test_state().await;
    let (_, owner_ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    let (_, other_ion_id) = seed_asset(&state, Some(OTHER), AssetType::PointCloud).await;
    let (_, ownerless_ion_id) = seed_asset(&state, None, AssetType::PointCloud).await;

    let listed_ids = |answer: Answer| -> Vec<i64> {
        assert_eq!(answer.status, StatusCode::OK);
        let mut ids: Vec<i64> = answer.json()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_i64().unwrap())
            .collect();
        ids.sort();
        ids
    };

    assert_eq!(
        listed_ids(get(&state, "/v1/assets", Some(&owner_token())).await),
        vec![owner_ion_id]
    );
    assert_eq!(
        listed_ids(get(&state, "/v1/assets", Some(&other_token())).await),
        vec![other_ion_id]
    );
    let mut every_id = vec![owner_ion_id, other_ion_id, ownerless_ion_id];
    every_id.sort();
    assert_eq!(
        listed_ids(get(&state, "/v1/assets", Some(&admin_token())).await),
        every_id
    );
}

#[tokio::test]
async fn an_ownerless_asset_is_for_admins_alone() {
    let state = test_state().await;
    let (id, ion_id) = seed_asset(&state, None, AssetType::PointCloud).await;
    for uri in asset_read_routes(&state, id, ion_id).await {
        assert_eq!(
            get(&state, &uri, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
        assert_eq!(
            get(&state, &uri, Some(&token("a-viewer", "viewer")))
                .await
                .status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
        assert_eq!(
            get(&state, &uri, Some(&admin_token())).await.status,
            StatusCode::OK,
            "{uri}"
        );
    }
    let listed = get(&state, "/api/v1/assets", Some(&owner_token())).await;
    assert_eq!(listed.json(), json!([]));
}

async fn upload(state: &Arc<AppState>, file_name: &str, contents: &[u8]) -> Uuid {
    let boundary = "assetaccessboundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(contents);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(header::AUTHORIZATION, format!("Bearer {}", owner_token()))
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let asset: Value = serde_json::from_slice(&bytes).unwrap();
    Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap()
}

fn write_file(path: std::path::PathBuf, contents: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn seed_bundle(state: &Arc<AppState>, name: &str) {
    let dir = state.data_dir.join("terrain_bundles").join(name);
    write_file(dir.join("layer.json"), BUNDLE_LAYER_JSON.as_bytes());
    write_file(dir.join("0/0/0.terrain"), b"tile bytes");
}

#[tokio::test]
async fn tile_routes_answer_with_no_header() {
    let state = test_state().await;
    let (tiled, _) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    let asset_dir = state.data_dir.join(tiled.to_string());
    write_file(
        asset_dir.join("tileset.json"),
        br#"{"asset":{"version":"1.1"}}"#,
    );
    write_file(asset_dir.join("tiles/0.pnts"), b"tile bytes");
    write_file(asset_dir.join("data/RC0000.glb"), b"tile bytes");

    let imagery = upload(&state, "ortho.tif", GDAL_COG).await;
    let czml = upload(&state, "scene.czml", CZML_DOCUMENT).await;

    let (terrain, _) = seed_asset(&state, Some(OWNER), AssetType::Terrain).await;
    seed_bundle(&state, &terrain.to_string());

    for uri in [
        format!("/api/v1/assets/{tiled}/tileset.json"),
        format!("/api/v1/assets/{tiled}/tiles/0.pnts"),
        format!("/api/v1/assets/{tiled}/data/RC0000.glb"),
        format!("/api/v1/assets/{imagery}/imagery/tilejson.json"),
        format!("/api/v1/assets/{imagery}/imagery/0/0/0.png"),
        format!("/api/v1/assets/{imagery}/imagery/wmts?SERVICE=WMTS&REQUEST=GetCapabilities"),
        format!("/api/v1/assets/{imagery}/imagery/wmts/1.0.0/WMTSCapabilities.xml"),
        format!("/api/v1/assets/{imagery}/imagery/tms/tilemapresource.xml"),
        format!("/api/v1/assets/{imagery}/imagery/tms/0/0/0.png"),
        format!("/api/v1/assets/{czml}/czml"),
        format!("/api/v1/terrain/bundles/{terrain}/layer.json"),
        format!("/api/v1/terrain/bundles/{terrain}/0/0/0.terrain"),
        "/api/v1/terrain/layer.json".to_string(),
    ] {
        let answer = get(&state, &uri, None).await;
        assert_eq!(
            answer.status,
            StatusCode::OK,
            "{uri}: {}",
            String::from_utf8_lossy(&answer.bytes)
        );
    }
}

fn bundle_names(answer: &Answer) -> Vec<String> {
    assert_eq!(
        answer.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&answer.bytes)
    );
    serde_json::from_slice(&answer.bytes).unwrap()
}

#[tokio::test]
async fn the_bundle_list_is_scoped_by_owner_and_keeps_operator_bundles() {
    let state = test_state().await;
    let (owner_bundle, _) = seed_asset(&state, Some(OWNER), AssetType::Terrain).await;
    let (other_bundle, _) = seed_asset(&state, Some(OTHER), AssetType::Terrain).await;
    let (ownerless_bundle, _) = seed_asset(&state, None, AssetType::Terrain).await;
    let no_row_bundle = Uuid::new_v4();
    for name in [
        "alps".to_string(),
        owner_bundle.to_string(),
        other_bundle.to_string(),
        ownerless_bundle.to_string(),
        no_row_bundle.to_string(),
    ] {
        seed_bundle(&state, &name);
    }
    let sorted = |ids: &[Uuid]| {
        let mut names: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        names.push("alps".to_string());
        names.sort();
        names
    };
    let uri = "/api/v1/terrain/bundles";

    assert_eq!(
        bundle_names(&get(&state, uri, None).await),
        vec!["alps".to_string()]
    );
    assert_eq!(
        bundle_names(&get(&state, uri, Some(&owner_token())).await),
        sorted(&[owner_bundle])
    );
    assert_eq!(
        bundle_names(&get(&state, uri, Some(&other_token())).await),
        sorted(&[other_bundle])
    );
    assert_eq!(
        bundle_names(&get(&state, uri, Some(&admin_token())).await),
        sorted(&[owner_bundle, other_bundle, ownerless_bundle])
    );

    assert_eq!(
        get(&state, uri, Some("not-a-token")).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn only_the_anonymous_bundle_list_may_be_cached_publicly() {
    let state = test_state().await;
    let uri = "/api/v1/terrain/bundles";

    let anonymous = get(&state, uri, None).await;
    assert!(
        anonymous
            .cache_control
            .as_deref()
            .unwrap()
            .contains("public"),
        "{:?}",
        anonymous.cache_control
    );
    assert_eq!(anonymous.vary.as_deref(), Some("authorization"));

    let tokened = get(&state, uri, Some(&owner_token())).await;
    assert!(
        tokened
            .cache_control
            .as_deref()
            .unwrap()
            .contains("no-store"),
        "{:?}",
        tokened.cache_control
    );
    assert_eq!(tokened.vary.as_deref(), Some("authorization"));
}

const GLOBAL_TERRAIN_ION_ID: i64 = 1;

async fn state_with_global_terrain() -> Arc<AppState> {
    test_state_with_global_terrain(GlobalTerrain::new(GlobalTerrainSettings {
        dem_url: Some("http://dem.test".to_string()),
        geoid_url: Some("http://geoid.test/geoid.tif".to_string()),
        cache_dir: Some(std::env::temp_dir().join(format!("asset_access_{}", Uuid::new_v4()))),
        cache_bytes: 1 << 20,
        ion_asset_id: Some(GLOBAL_TERRAIN_ION_ID),
    }))
    .await
}

#[tokio::test]
async fn the_configured_global_terrain_endpoint_alone_stays_anonymous() {
    let state = state_with_global_terrain().await;

    let endpoint = get(&state, "/v1/assets/1/endpoint", None).await;
    assert_eq!(endpoint.status, StatusCode::OK);
    assert_eq!(endpoint.json()["type"], "TERRAIN");

    for uri in ["/v1/assets/2/endpoint", "/v1/assets/1", "/v1/assets"] {
        assert_eq!(
            get(&state, uri, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn a_server_with_no_global_terrain_id_gates_every_endpoint() {
    let state = test_state().await;
    assert_eq!(
        get(&state, "/v1/assets/1/endpoint", None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

async fn post_json(state: &Arc<AppState>, uri: &str, bearer: &str, body: Value) -> Answer {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Answer {
        status,
        cache_control: None,
        vary: None,
        bytes,
    }
}

#[tokio::test]
async fn an_export_needs_a_view_of_the_asset() {
    let state = test_state().await;
    // the export routes read the tenant off the subject, which has to be a uuid
    let owner_id = Uuid::new_v4().to_string();
    let (asset_id, _) = seed_asset(&state, Some(&owner_id), AssetType::PointCloud).await;
    let body = json!({ "asset_id": asset_id, "format": "geojson" });

    let other = token(&Uuid::new_v4().to_string(), "editor");
    assert_eq!(
        post_json(&state, "/api/v1/exports", &other, body.clone())
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let owner = token(&owner_id, "editor");
    assert_eq!(
        post_json(&state, "/api/v1/exports", &owner, body.clone())
            .await
            .status,
        StatusCode::ACCEPTED
    );

    let admin = token(&Uuid::new_v4().to_string(), "admin");
    assert_eq!(
        post_json(&state, "/api/v1/exports", &admin, body)
            .await
            .status,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn a_private_story_is_for_its_author_and_admins() {
    let state = test_state().await;
    let story = post_json(
        &state,
        "/api/v1/stories",
        &owner_token(),
        json!({ "title": "private walk", "is_public": false }),
    )
    .await;
    assert_eq!(story.status, StatusCode::CREATED);
    let uri = format!("/api/v1/stories/{}", story.json()["id"].as_str().unwrap());

    assert_eq!(
        get(&state, &uri, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&state, &uri, Some(&other_token())).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&state, &uri, Some(&owner_token())).await.status,
        StatusCode::OK
    );
    assert_eq!(
        get(&state, &uri, Some(&admin_token())).await.status,
        StatusCode::OK
    );

    let listed_by_other = get(&state, "/api/v1/stories", Some(&other_token())).await;
    assert_eq!(listed_by_other.json(), json!([]));
    let listed_by_author = get(&state, "/api/v1/stories", Some(&owner_token())).await;
    assert_eq!(listed_by_author.json().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn upload_complete_answers_another_owner_like_a_missing_asset() {
    let state = test_state().await;
    let (_, ion_id) = seed_asset(&state, Some(OWNER), AssetType::PointCloud).await;
    let missing_ion_id = ion_id + 1000;
    let upload_complete = |id: i64, bearer: String| {
        let state = Arc::clone(&state);
        async move {
            let uri = format!("/v1/assets/{id}/uploadComplete");
            post_json(&state, &uri, &bearer, json!({})).await
        }
    };

    let theirs = upload_complete(ion_id, other_token()).await;
    let missing = upload_complete(missing_ion_id, other_token()).await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        theirs.json(),
        json!({ "message": format!("no asset {ion_id}") })
    );
    assert_eq!(
        missing.json(),
        json!({ "message": format!("no asset {missing_ion_id}") })
    );

    // the owner gets past the check to the state of the upload
    assert_eq!(
        upload_complete(ion_id, owner_token()).await.status,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn an_ownerless_asset_is_modified_by_admins_alone() {
    let state = test_state().await;
    let (id, ion_id) = seed_asset(&state, None, AssetType::Model).await;
    let editor = owner_token();

    for (uri, body) in [
        (format!("/api/v1/assets/{id}/tile"), json!({})),
        (
            format!("/api/v1/assets/{id}/annotations"),
            json!({ "text": "a note", "longitude": 7.4, "latitude": 43.7 }),
        ),
    ] {
        assert_eq!(
            post_json(&state, &uri, &editor, body).await.status,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    assert_eq!(
        post_json(
            &state,
            &format!("/v1/assets/{ion_id}/uploadComplete"),
            &editor,
            json!({})
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    let delete_as = |bearer: String| {
        let state = Arc::clone(&state);
        async move {
            router(state)
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!("/api/v1/assets/{id}"))
                        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(delete_as(editor).await, StatusCode::FORBIDDEN);
    assert!(state.db.get_asset(id).await.unwrap().is_some());
    assert_eq!(delete_as(admin_token()).await, StatusCode::NO_CONTENT);
    assert!(state.db.get_asset(id).await.unwrap().is_none());
}

#[tokio::test]
async fn a_public_story_is_readable_with_any_token() {
    let state = test_state().await;
    let story = post_json(
        &state,
        "/api/v1/stories",
        &owner_token(),
        json!({ "title": "public walk", "is_public": true }),
    )
    .await;
    assert_eq!(story.status, StatusCode::CREATED);
    let uri = format!("/api/v1/stories/{}", story.json()["id"].as_str().unwrap());

    assert_eq!(
        get(&state, &uri, Some(&other_token())).await.status,
        StatusCode::OK
    );
    let listed_by_other = get(&state, "/api/v1/stories", Some(&other_token())).await;
    assert_eq!(listed_by_other.json().as_array().unwrap().len(), 1);
}
