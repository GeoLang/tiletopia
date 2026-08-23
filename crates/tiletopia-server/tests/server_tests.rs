mod common;

#[cfg(test)]
mod tests {
    use crate::common::build_state;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tiletopia_server::{AppState, router};
    use tower::ServiceExt;

    async fn test_state() -> Arc<AppState> {
        state_with_engines(tiletopia_server::analysis_tiles::AnalysisEngines::new()).await
    }

    async fn state_with_engines(
        analysis_engines: tiletopia_server::analysis_tiles::AnalysisEngines,
    ) -> Arc<AppState> {
        build_state(analysis_engines, None).await
    }

    async fn state_with_external_tiler(
        external_tiler_jar: Option<std::path::PathBuf>,
    ) -> Arc<AppState> {
        build_state(
            tiletopia_server::analysis_tiles::AnalysisEngines::new(),
            external_tiler_jar,
        )
        .await
    }

    #[tokio::test]
    async fn health_endpoint() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn list_assets_empty() {
        let state = test_state().await;
        let (token, _uid) = signup(&state, "list-empty@example.com").await;
        let (status, assets) = list_assets(&state, Some(&token), "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(assets.is_empty());
    }

    #[tokio::test]
    async fn get_asset_not_found() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assets/00000000-0000-0000-0000-000000000000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_tileset_not_found() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assets/00000000-0000-0000-0000-000000000000/tileset.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // -- terrain reads --
    //
    // the exemption rule itself is unit-tested in auth::tests (the middleware
    // only enforces when TILETOPIA_JWT_SECRET is set, which a test in this
    // binary cannot do without racing every other test in the process: a test
    // that needs it set gets its own binary, see
    // martin_routes_refuse_a_tokenless_request). these two cover that the
    // routes exist and answer a tokenless GET.

    #[tokio::test]
    async fn terrain_layer_json_anonymous_ok() {
        let state = test_state().await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/terrain/layer.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // what CesiumTerrainProvider's layer.json parser insists on
        assert_eq!(v["format"], "quantized-mesh-1.0");
        assert_eq!(v["projection"], "EPSG:4326");
        assert_eq!(v["scheme"], "tms");
        // relative, so it resolves against layer.json behind any proxy prefix
        assert_eq!(v["tiles"][0], "{z}/{x}/{y}.terrain?v={version}");
        assert_eq!(v["available"][0][0]["endX"], 1);
        assert_eq!(v["available"].as_array().unwrap().len() as u64, 16);
    }

    #[tokio::test]
    async fn terrain_root_tiles_serve_cesiums_request() {
        let state = test_state().await;

        for uri in [
            "/api/v1/terrain/0/0/0.terrain?v=1.0.0",
            "/api/v1/terrain/0/1/0.terrain?v=1.0.0",
        ] {
            let resp = router(Arc::clone(&state))
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(
                            "accept",
                            "application/vnd.quantized-mesh,application/octet-stream;q=0.9,*/*;q=0.01",
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                resp.headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok()),
                Some("application/vnd.quantized-mesh")
            );
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(bytes.len() > 88, "{uri} returned only a header");
        }
    }

    /// Write a flat local DEM under every one-degree cell these bounds touch,
    /// so a terrain request over them never reaches for SRTM.
    fn seed_local_dem(data_dir: &std::path::Path, bounds: [f64; 4]) {
        let dem_dir = data_dir.join("dem");
        std::fs::create_dir_all(&dem_dir).unwrap();
        let elevations: Vec<u8> = (0..16u32 * 16)
            .flat_map(|i| (i as f32).to_le_bytes())
            .collect();
        for (lat, lon) in tiletopia_terrain::global_dem::required_dem_tiles(bounds) {
            std::fs::write(dem_dir.join(format!("{lat}_{lon}.bin")), &elevations).unwrap();
        }
    }

    #[tokio::test]
    async fn terrain_rgb_tile_anonymous_ok() {
        let state = test_state().await;
        seed_local_dem(
            &state.data_dir,
            tiletopia_terrain::mercator::MercatorTileCoord {
                zoom: 9,
                x: 266,
                y: 186,
            }
            .bounds(),
        );

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/terrain/rgb/9/266/186.png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    }

    #[tokio::test]
    async fn terrain_rgb_rejects_malformed_coords() {
        let state = test_state().await;

        for uri in [
            "/api/v1/terrain/rgb/0/1/0.png",  // only one tile at zoom 0
            "/api/v1/terrain/rgb/16/0/0.png", // past the zoom cap
            "/api/v1/terrain/rgb/9/266/abc.png",
            "/api/v1/terrain/rgb/9/266/186.jpg",
        ] {
            let resp = router(Arc::clone(&state))
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    /// The tile every terrain test renders, narrow enough that a missing local
    /// DEM sends the handler to SRTM.
    const TERRAIN_TILE: tiletopia_terrain::global_dem::TerrainTileCoord =
        tiletopia_terrain::global_dem::TerrainTileCoord {
            zoom: 12,
            x: 2200,
            y: 1400,
        };

    #[tokio::test]
    async fn terrain_tile_anonymous_ok() {
        let state = test_state().await;

        // seed a local DEM so the handler never reaches for SRTM over the network
        seed_local_dem(&state.data_dir, TERRAIN_TILE.bounds());

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/terrain/12/2200/1400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/vnd.quantized-mesh")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(bytes.len() > 88, "quantized mesh header plus vertex data");
    }

    #[tokio::test]
    async fn terrain_tile_refuses_when_srtm_fetch_fails() {
        // no local DEM here, so the handler goes upstream, and upstream is broken
        let mut state = test_state().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let failing =
                axum::Router::new().fallback(|| async { StatusCode::INTERNAL_SERVER_ERROR });
            axum::serve(listener, failing).await.unwrap();
        });
        Arc::get_mut(&mut state).unwrap().srtm_base_url = format!("http://{addr}");

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/terrain/12/2200/1400")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // a flat 200 here is the bug: terrain looks enabled and perfectly level
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        let bounds = TERRAIN_TILE.bounds();
        let (lat, lon) = tiletopia_terrain::dem_cache::required_srtm_tiles(
            bounds[0], bounds[1], bounds[2], bounds[3],
        )[0];
        let name = tiletopia_terrain::dem_cache::srtm_tile_name(lat, lon);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(&name), "{body} should name the failed tile");
    }

    // -- prebuilt terrain bundles --

    /// Bytes a tiler would have written into one `.terrain` file. Only the
    /// server's own handling is under test here, so the payload just has to
    /// survive the round trip byte for byte.
    const BUNDLE_TILE_BODY: &[u8] = b"quantized-mesh payload";

    fn gzipped(data: &[u8]) -> Vec<u8> {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn write_bundle_tile(dir: &std::path::Path, z: u32, x: u32, y: u32, body: Vec<u8>) {
        let column = dir.join(z.to_string()).join(x.to_string());
        std::fs::create_dir_all(&column).unwrap();
        std::fs::write(column.join(format!("{y}.terrain")), body).unwrap();
    }

    /// Lay a bundle out the way an external tiler does: a layer.json beside a
    /// {z}/{x}/{y}.terrain tree, gzipped at 0/0/0 and plain at 0/1/0.
    fn seed_terrain_bundle(
        data_dir: &std::path::Path,
        name: &str,
        layer_json: serde_json::Value,
    ) -> std::path::PathBuf {
        let dir = data_dir.join("terrain_bundles").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("layer.json"),
            serde_json::to_vec(&layer_json).unwrap(),
        )
        .unwrap();
        write_bundle_tile(&dir, 0, 0, 0, gzipped(BUNDLE_TILE_BODY));
        write_bundle_tile(&dir, 0, 1, 0, BUNDLE_TILE_BODY.to_vec());
        write_bundle_tile(&dir, 1, 2, 1, BUNDLE_TILE_BODY.to_vec());
        write_bundle_tile(&dir, 1, 3, 1, BUNDLE_TILE_BODY.to_vec());
        dir
    }

    fn bundle_layer_json() -> serde_json::Value {
        serde_json::json!({
            "tilejson": "2.1.0",
            "name": "alps",
            "version": "1.1.0",
            "format": "quantized-mesh-1.0",
            "scheme": "tms",
            "projection": "EPSG:4326",
            "bounds": [-180.0, -90.0, 180.0, 90.0],
            "tiles": ["https://assets.example.com/{z}/{x}/{y}.terrain?v={version}"],
            "available": [
                [{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }],
                [{ "startX": 2, "startY": 1, "endX": 3, "endY": 1 }]
            ]
        })
    }

    async fn get(state: &Arc<AppState>, uri: &str) -> axum::response::Response {
        router(Arc::clone(state))
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn bundle_layer_json_is_served_and_repointed_at_this_server() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());

        let resp = get(&state, "/api/v1/terrain/bundles/alps/layer.json").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cache = resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_string();
        assert!(cache.contains("public"), "{cache}");

        let layer: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        // what CesiumTerrainProvider's layer.json parser insists on, passed
        // through from the bundle
        assert_eq!(layer["format"], "quantized-mesh-1.0");
        assert_eq!(layer["scheme"], "tms");
        assert_eq!(layer["projection"], "EPSG:4326");
        assert_eq!(layer["version"], "1.1.0");
        // the bundle pointed at someone else's host, which would have defeated
        // the whole point of hosting it here
        assert_eq!(layer["tiles"][0], "{z}/{x}/{y}.terrain?v={version}");
        // availability the bundle carries is left exactly as it was
        assert_eq!(layer["available"][0][0]["endX"], 1);
        assert_eq!(layer["available"][1][0]["startX"], 2);
        assert_eq!(layer["available"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bundle_availability_is_read_off_the_tile_tree_when_absent() {
        let state = test_state().await;
        let mut layer_json = bundle_layer_json();
        layer_json.as_object_mut().unwrap().remove("available");
        seed_terrain_bundle(&state.data_dir, "derived", layer_json);

        let resp = get(&state, "/api/v1/terrain/bundles/derived/layer.json").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let layer: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();

        let available = layer["available"].as_array().unwrap();
        // both levels the fixture wrote, and nothing past the deepest one
        assert_eq!(available.len(), 2);
        assert_eq!(available[0][0]["startX"], 0);
        assert_eq!(available[0][0]["endX"], 1);
        assert_eq!(available[0][0]["startY"], 0);
        assert_eq!(available[0][0]["endY"], 0);
        assert_eq!(available[1][0]["startX"], 2);
        assert_eq!(available[1][0]["endX"], 3);
        assert_eq!(available[1][0]["startY"], 1);
        assert_eq!(available[1][0]["endY"], 1);
    }

    #[tokio::test]
    async fn bundle_gzipped_tile_says_so_and_arrives_intact() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());

        let resp = get(&state, "/api/v1/terrain/bundles/alps/0/0/0.terrain?v=1.1.0").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/vnd.quantized-mesh")
        );
        // without this the browser hands Cesium the gzip container as a mesh
        assert_eq!(
            resp.headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );

        let body = body_bytes(resp).await;
        let mut decoded = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(body.as_slice()),
            &mut decoded,
        )
        .unwrap();
        assert_eq!(decoded, BUNDLE_TILE_BODY);
    }

    #[tokio::test]
    async fn bundle_plain_tile_is_not_labelled_gzip() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());

        let resp = get(&state, "/api/v1/terrain/bundles/alps/0/1/0.terrain").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("content-encoding").is_none());
        assert_eq!(body_bytes(resp).await, BUNDLE_TILE_BODY);
    }

    #[tokio::test]
    async fn bundle_misses_are_404_not_empty_tiles() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());

        for uri in [
            "/api/v1/terrain/bundles/alps/0/0/9.terrain", // tile the bundle has no file for
            "/api/v1/terrain/bundles/alps/7/1/1.terrain", // level the bundle has no directory for
            "/api/v1/terrain/bundles/nosuch/0/0/0.terrain",
            "/api/v1/terrain/bundles/nosuch/layer.json",
        ] {
            let resp = get(&state, uri).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[tokio::test]
    async fn bundle_names_and_coords_that_are_not_tiles_are_refused() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());

        for uri in [
            "/api/v1/terrain/bundles/%2E%2E/layer.json",
            "/api/v1/terrain/bundles/%2E%2E/0/0/0.terrain",
            "/api/v1/terrain/bundles/alps/0/0/abc.terrain",
            "/api/v1/terrain/bundles/alps/0/0/0.png",
        ] {
            let resp = get(&state, uri).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    #[tokio::test]
    async fn bundle_in_another_format_is_refused_rather_than_mislabelled() {
        let state = test_state().await;
        let mut layer_json = bundle_layer_json();
        layer_json["format"] = serde_json::json!("heightmap-1.0");
        seed_terrain_bundle(&state.data_dir, "heights", layer_json);

        let resp = get(&state, "/api/v1/terrain/bundles/heights/layer.json").await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn bundle_listing_names_what_is_hosted() {
        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());
        seed_terrain_bundle(&state.data_dir, "iceland", bundle_layer_json());
        // a directory with no layer.json is not a bundle
        std::fs::create_dir_all(state.data_dir.join("terrain_bundles/half-copied")).unwrap();

        let resp = get(&state, "/api/v1/terrain/bundles").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let names: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(names, serde_json::json!(["alps", "iceland"]));
    }

    #[tokio::test]
    async fn bundle_listing_is_empty_when_none_are_hosted() {
        let state = test_state().await;

        let resp = get(&state, "/api/v1/terrain/bundles").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let names: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(names, serde_json::json!([]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bundle_listing_that_cannot_be_read_is_an_error_not_an_empty_list() {
        use std::os::unix::fs::PermissionsExt;

        let state = test_state().await;
        seed_terrain_bundle(&state.data_dir, "alps", bundle_layer_json());
        let root = state.data_dir.join("terrain_bundles");
        let readable = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();

        // root reads a directory whatever its mode says, so there is no
        // unreadable directory to answer for
        if std::fs::read_dir(&root).is_ok() {
            std::fs::set_permissions(&root, readable).unwrap();
            return;
        }

        let resp = get(&state, "/api/v1/terrain/bundles").await;
        std::fs::set_permissions(&root, readable).unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn bundles_do_not_shadow_the_on_demand_terrain_routes() {
        let state = test_state().await;
        seed_local_dem(&state.data_dir, TERRAIN_TILE.bounds());

        let resp = get(&state, "/api/v1/terrain/12/2200/1400").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // signs up a user and returns (bearer_token, user_id).
    async fn signup(state: &Arc<AppState>, email: &str) -> (String, String) {
        let body =
            serde_json::json!({ "email": email, "password": "pw123456", "name": "Test User" })
                .to_string();
        let resp = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/signup")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        (
            v["token"].as_str().unwrap().to_string(),
            v["user"]["id"].as_str().unwrap().to_string(),
        )
    }

    async fn create_portal_item(
        state: &Arc<AppState>,
        token: &str,
        title: &str,
        sharing: &str,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "title": title,
            "type": "map",
            "description": "a test item",
            "tags": ["alpha", "beta"],
            "sharing": sharing,
        })
        .to_string();
        let resp = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/portal/items")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn list_portal_items(state: &Arc<AppState>, token: &str) -> Vec<serde_json::Value> {
        let resp = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/portal/items")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn portal_requires_auth() {
        let state = test_state().await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/portal/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn portal_crud_roundtrip() {
        let state = test_state().await;
        let (token, _uid) = signup(&state, "owner@example.com").await;

        let created = create_portal_item(&state, &token, "My Map", "private").await;
        assert_eq!(created["title"], "My Map");
        assert_eq!(created["type"], "map");
        assert_eq!(created["owner"], "Test User");
        assert!(created.get("owner_id").is_none(), "owner_id must not leak");
        let id = created["id"].as_str().unwrap();

        let items = list_portal_items(&state, &token).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], id);

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/portal/items/{id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let items = list_portal_items(&state, &token).await;
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn portal_non_owner_cannot_delete() {
        let state = test_state().await;
        let (owner_token, _) = signup(&state, "owner2@example.com").await;
        let (other_token, _) = signup(&state, "other2@example.com").await;

        let created = create_portal_item(&state, &owner_token, "Owned", "public").await;
        let id = created["id"].as_str().unwrap();

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/portal/items/{id}"))
                    .header("authorization", format!("Bearer {other_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // item survives the rejected delete
        let items = list_portal_items(&state, &owner_token).await;
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn portal_private_hidden_from_others() {
        let state = test_state().await;
        let (owner_token, _) = signup(&state, "owner3@example.com").await;
        let (other_token, _) = signup(&state, "other3@example.com").await;

        create_portal_item(&state, &owner_token, "Secret", "private").await;
        let shared = create_portal_item(&state, &owner_token, "Shared", "public").await;

        // owner sees both
        let owner_items = list_portal_items(&state, &owner_token).await;
        assert_eq!(owner_items.len(), 2);

        // other user sees only the public one
        let other_items = list_portal_items(&state, &other_token).await;
        assert_eq!(other_items.len(), 1);
        assert_eq!(other_items[0]["id"], shared["id"]);
    }

    #[tokio::test]
    async fn tile_path_traversal_blocked() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assets/00000000-0000-0000-0000-000000000000/tiles/../../etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Axum normalizes paths, so ".." gets handled at the routing level
        // Either BAD_REQUEST (our check) or NOT_FOUND (Axum path normalization) is acceptable
        assert!(
            response.status() == StatusCode::BAD_REQUEST
                || response.status() == StatusCode::NOT_FOUND
        );
    }

    async fn post_json(
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let app = router(test_state().await);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, ct, bytes)
    }

    #[tokio::test]
    async fn analysis_viewshed_returns_polygon() {
        let (status, ct, bytes) = post_json(
            "/api/v1/analysis/viewshed",
            serde_json::json!({ "observer": [7.42, 43.73], "height_m": 2.0, "radius_m": 1000.0 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.unwrap().contains("json"));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "FeatureCollection");
        let geom = &v["features"][0]["geometry"];
        assert_eq!(geom["type"], "Polygon");
        assert!(geom["coordinates"][0].as_array().unwrap().len() > 8);
    }

    #[tokio::test]
    async fn analysis_flood_grows_with_level() {
        let bbox = [7.40, 43.72, 7.45, 43.75];
        let count_at = |level: f64| async move {
            let (status, _, bytes) = post_json(
                "/api/v1/analysis/flood",
                serde_json::json!({ "level_m": level, "bbox": bbox, "resolution": 48 }),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            v["features"][0]["properties"]["flooded_cells"]
                .as_u64()
                .unwrap_or(0)
        };
        let low = count_at(40.0).await;
        let high = count_at(80.0).await;
        assert!(high >= low, "high {high} should be >= low {low}");
    }

    #[tokio::test]
    async fn analysis_terrain_hillshade_png_decodes() {
        let (status, ct, bytes) = post_json(
            "/api/v1/analysis/terrain",
            serde_json::json!({ "op": "hillshade", "bbox": [7.40, 43.72, 7.45, 43.75], "resolution": 48 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.unwrap(), "image/png");
        let img = image::load_from_memory(&bytes).expect("valid png");
        assert_eq!(img.width(), 48);
    }

    #[tokio::test]
    async fn analysis_terrain_contours_returns_lines() {
        let (status, ct, bytes) = post_json(
            "/api/v1/analysis/terrain",
            serde_json::json!({ "op": "contours", "bbox": [7.40, 43.72, 7.45, 43.75], "resolution": 64 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.unwrap().contains("json"));
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "FeatureCollection");
        let feats = v["features"].as_array().unwrap();
        assert!(!feats.is_empty());
        assert_eq!(feats[0]["geometry"]["type"], "LineString");
    }

    #[tokio::test]
    async fn analysis_solar_returns_png() {
        let (status, ct, bytes) = post_json(
            "/api/v1/analysis/solar",
            serde_json::json!({ "bbox": [7.40, 43.72, 7.45, 43.75], "date": "2026-06-21", "resolution": 48 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct.unwrap(), "image/png");
        image::load_from_memory(&bytes).expect("valid png");
    }

    // -- analysis xyz tiles --

    /// A tile over the shared state, so repeat calls hit the same engine.
    async fn get_tile_bytes(state: &Arc<AppState>, uri: &str) -> (StatusCode, Vec<u8>) {
        let resp = router(Arc::clone(state))
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, bytes)
    }

    fn distinct_pixels(img: &image::DynamicImage) -> usize {
        let mut seen: Vec<[u8; 4]> = img.to_rgba8().pixels().map(|p| p.0).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    #[tokio::test]
    async fn analysis_xyz_hillshade_tile_is_a_256_png() {
        let state = test_state().await;
        let (status, bytes) =
            get_tile_bytes(&state, "/api/v1/analysis/xyz/hillshade/12/2132/1493.png").await;
        assert_eq!(status, StatusCode::OK);
        let img = image::load_from_memory(&bytes).expect("valid png");
        assert_eq!((img.width(), img.height()), (256, 256));
        // a flat tile would mean the pull never reached the terrain
        assert!(distinct_pixels(&img) > 8);
    }

    #[tokio::test]
    async fn analysis_xyz_slope_tile_renders() {
        let state = test_state().await;
        let (status, bytes) =
            get_tile_bytes(&state, "/api/v1/analysis/xyz/slope/12/2132/1493.png").await;
        assert_eq!(status, StatusCode::OK);
        let img = image::load_from_memory(&bytes).expect("valid png");
        assert_eq!(img.width(), 256);
        assert!(distinct_pixels(&img) > 8);
    }

    /// The second call comes off the engine's chunk cache, so it has to render
    /// the same tile, not merely another valid one.
    #[tokio::test]
    async fn analysis_xyz_repeat_tile_is_identical() {
        let state = test_state().await;
        let uri = "/api/v1/analysis/xyz/hillshade/12/2132/1493.png";
        let (first_status, first) = get_tile_bytes(&state, uri).await;
        let (second_status, second) = get_tile_bytes(&state, uri).await;
        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::OK);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn analysis_xyz_azimuth_changes_the_tile() {
        let state = test_state().await;
        let (_, east) = get_tile_bytes(
            &state,
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=90",
        )
        .await;
        let (_, west) = get_tile_bytes(
            &state,
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=270",
        )
        .await;
        assert!(!east.is_empty());
        assert_ne!(east, west);
    }

    #[tokio::test]
    async fn analysis_xyz_unknown_op_is_rejected() {
        let state = test_state().await;
        let (status, _) =
            get_tile_bytes(&state, "/api/v1/analysis/xyz/viewshed/12/2132/1493.png").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn analysis_xyz_out_of_range_tile_is_not_found() {
        let state = test_state().await;
        let (status, _) = get_tile_bytes(&state, "/api/v1/analysis/xyz/slope/2/9/1.png").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A turn of azimuth is the same sun, so the second call has to come off the
    /// first one's engine rather than build a second one.
    #[tokio::test]
    async fn analysis_xyz_wrapped_azimuth_is_the_same_tile() {
        let state = test_state().await;
        let (plain_status, plain) = get_tile_bytes(
            &state,
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=15",
        )
        .await;
        let (wrapped_status, wrapped) = get_tile_bytes(
            &state,
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=375",
        )
        .await;
        assert_eq!(plain_status, StatusCode::OK);
        assert_eq!(wrapped_status, StatusCode::OK);
        assert_eq!(plain, wrapped);
    }

    #[tokio::test]
    async fn analysis_xyz_non_finite_params_are_rejected() {
        let state = test_state().await;
        for uri in [
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=nan",
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?azimuth=inf",
            "/api/v1/analysis/xyz/hillshade/12/2132/1493.png?altitude=-inf",
            "/api/v1/analysis/xyz/slope/12/2132/1493.png?azimuth=NaN",
        ] {
            let (status, _) = get_tile_bytes(&state, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
    }

    // -- analysis export --

    /// The export of the same terrain the tile tests render, verified by
    /// reading the bytes back as a COG: web mercator, whole-pixel dims
    /// anchored north-west, real values everywhere on the synthetic source.
    #[tokio::test]
    async fn analysis_export_answers_a_web_mercator_cog() {
        let state = test_state().await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/analysis/export/hillshade?bbox=7,45,7.02,45.01&resolution=200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/tiff")
        );
        assert_eq!(
            resp.headers()
                .get("content-disposition")
                .and_then(|v| v.to_str().ok()),
            Some("attachment; filename=\"hillshade.tif\"")
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();

        let mut reader = terrano_core::CogReader::open(&bytes[..]).expect("valid cog");
        assert_eq!(reader.meta().epsg, 3857);
        assert_eq!(reader.meta().pixel_width, 200.0);
        assert_eq!(reader.meta().pixel_height, 200.0);
        // 0.02 x 0.01 degrees at 45N is 2226 x 1574 mercator meters, so 200
        // m/px snaps up to 12 x 8 whole pixels
        let level0 = &reader.levels()[0];
        assert_eq!((level0.width, level0.height), (12, 8));
        assert_eq!(level0.samples, 1);
        let banded = reader.read_window_bands(0, 0, 0, 12, 8).expect("window");
        let band = banded.band(0).expect("one band");
        assert!(band.data().iter().all(|v| v.is_finite()));
        // a flat export would mean the pull never reached the terrain
        let (min, max) = band
            .data()
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(*v), hi.max(*v))
            });
        assert!(max > min);
    }

    #[tokio::test]
    async fn analysis_export_refuses_bad_requests() {
        let state = test_state().await;
        for (uri, needle) in [
            (
                "/api/v1/analysis/export/hillshade?bbox=-180,-85,180,85&resolution=1",
                "export cap",
            ),
            (
                "/api/v1/analysis/export/hillshade?bbox=8,46,7,47&resolution=100",
                "bbox",
            ),
            (
                "/api/v1/analysis/export/hillshade?bbox=7,45,8,46&resolution=0",
                "resolution",
            ),
            (
                "/api/v1/analysis/export/hillshade?bbox=7,45,8,46&resolution=100&azimuth=nan",
                "finite",
            ),
            (
                "/api/v1/analysis/export/contour?bbox=7,45,8,46&resolution=100",
                "unknown op",
            ),
        ] {
            let (status, bytes) = get_tile_bytes(&state, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            let body = String::from_utf8_lossy(&bytes);
            assert!(body.contains(needle), "{uri}: {body}");
        }
    }

    /// A tile waits for a render slot, and is refused once the wait runs out.
    /// Shedding past that is what keeps an anonymous caller from pinning every
    /// core. The wait is short here, the served one is two seconds.
    #[tokio::test]
    async fn analysis_xyz_refuses_a_tile_when_renders_stay_saturated() {
        let wait = std::time::Duration::from_millis(100);
        let state = state_with_engines(
            tiletopia_server::analysis_tiles::AnalysisEngines::with_render_limits(0, wait),
        )
        .await;
        let started = std::time::Instant::now();
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/analysis/xyz/hillshade/12/2132/1493.png")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "1");
        // it queued for the slot rather than refusing on the spot
        assert!(started.elapsed() >= wait);
    }

    // -- role management --

    async fn login(
        state: &Arc<AppState>,
        email: &str,
        password: &str,
    ) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "email": email, "password": password }).to_string();
        let resp = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    // signup a user, promote it to admin straight in the db (the CLI's offline
    // bootstrap path), then log in for a token carrying the admin role.
    async fn bootstrap_admin(state: &Arc<AppState>, email: &str) -> String {
        let (_token, uid) = signup(state, email).await;
        let id = uuid::Uuid::parse_str(&uid).unwrap();
        let mut user = state.db.get_user(id).await.unwrap().unwrap();
        user.role = tiletopia_server::users::UserRole::Admin;
        state.db.update_user(&user).await.unwrap();
        let (status, body) = login(state, email, "pw123456").await;
        assert_eq!(status, StatusCode::OK);
        body["token"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn non_admin_cannot_set_role() {
        let state = test_state().await;
        let (viewer_token, _) = signup(&state, "viewer-a@example.com").await;
        let (_t, target_id) = signup(&state, "viewer-b@example.com").await;

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/admin/users/{target_id}/role"))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {viewer_token}"))
                    .body(Body::from(
                        serde_json::json!({ "role": "editor" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_can_set_role_and_token_carries_it() {
        let state = test_state().await;
        let admin_token = bootstrap_admin(&state, "root@example.com").await;
        let (_t, target_id) = signup(&state, "promote-me@example.com").await;

        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/admin/users/{target_id}/role"))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {admin_token}"))
                    .body(Body::from(
                        serde_json::json!({ "role": "admin" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["role"], "admin");

        // a freshly minted token for the promoted user must carry the new role;
        // prove it by using that token on an admin-only route.
        let (status, body) = login(&state, "promote-me@example.com", "pw123456").await;
        assert_eq!(status, StatusCode::OK);
        let target_token = body["token"].as_str().unwrap().to_string();
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/users")
                    .header("authorization", format!("Bearer {target_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn update_me_cannot_change_role() {
        let state = test_state().await;
        let (token, uid) = signup(&state, "self@example.com").await;

        // try to sneak a role escalation through the self-service profile update
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/users/me")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::from(
                        serde_json::json!({ "name": "New Name", "role": "admin" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["name"], "New Name");
        assert_eq!(v["role"], "viewer");

        // and it must not have been persisted either
        let id = uuid::Uuid::parse_str(&uid).unwrap();
        let stored = state.db.get_user(id).await.unwrap().unwrap();
        assert_eq!(stored.role, tiletopia_server::users::UserRole::Viewer);
    }

    fn legacy_hash(password: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let salt = [7u8; 16];
        let mut mac = Hmac::<Sha256>::new_from_slice(&salt).unwrap();
        mac.update(password.as_bytes());
        let result = mac.finalize().into_bytes();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        format!("{}:{}", hex(&salt), hex(&result))
    }

    #[tokio::test]
    async fn legacy_password_hash_logs_in_and_is_rehashed() {
        use tiletopia_server::users::{User, UserRole};
        let state = test_state().await;

        let email = "legacy@example.com";
        let user = User {
            id: uuid::Uuid::new_v4(),
            email: email.to_string(),
            name: "Legacy".into(),
            role: UserRole::Viewer,
            org_id: None,
            created_at: chrono::Utc::now(),
            last_login: None,
        };
        state
            .db
            .create_user(&user, &legacy_hash("pw123456"))
            .await
            .unwrap();

        // old-format hash still authenticates
        let (status, body) = login(&state, email, "pw123456").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["token"].as_str().is_some());

        // and it was transparently upgraded to argon2id
        let (_u, stored) = state.db.get_user_by_email(email).await.unwrap().unwrap();
        assert!(
            stored.starts_with("$argon2"),
            "hash should be argon2id, got {stored}"
        );

        // wrong password still fails after migration
        let (status, _) = login(&state, email, "wrong-password").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // -- ion-compat auth --

    // signup a user, promote to editor in the db, then log in for a token
    // carrying the editor role.
    async fn bootstrap_editor(state: &Arc<AppState>, email: &str) -> String {
        bootstrap_editor_with_id(state, email).await.0
    }

    async fn bootstrap_editor_with_id(state: &Arc<AppState>, email: &str) -> (String, String) {
        let (_token, uid) = signup(state, email).await;
        let id = uuid::Uuid::parse_str(&uid).unwrap();
        let mut user = state.db.get_user(id).await.unwrap().unwrap();
        user.role = tiletopia_server::users::UserRole::Editor;
        state.db.update_user(&user).await.unwrap();
        let (status, body) = login(state, email, "pw123456").await;
        assert_eq!(status, StatusCode::OK);
        (body["token"].as_str().unwrap().to_string(), uid)
    }

    async fn post_ion(
        state: &Arc<AppState>,
        uri: &str,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> StatusCode {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        router(Arc::clone(state))
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn ion_create_asset_anonymous_rejected() {
        let state = test_state().await;
        let status = post_ion(
            &state,
            "/v1/assets",
            None,
            serde_json::json!({ "name": "anon", "type": "3DTILES" }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ion_create_asset_viewer_forbidden() {
        let state = test_state().await;
        let (viewer_token, _) = signup(&state, "ion-viewer@example.com").await;
        let status = post_ion(
            &state,
            "/v1/assets",
            Some(&viewer_token),
            serde_json::json!({ "name": "nope", "type": "3DTILES" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn ion_create_asset_editor_succeeds() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "ion-editor@example.com").await;
        let status = post_ion(
            &state,
            "/v1/assets",
            Some(&editor_token),
            serde_json::json!({ "name": "yes", "type": "3DTILES" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn ion_list_assets_anonymous_ok() {
        // a legitimate anonymous tile-data GET (the Ion read layer) still works
        let state = test_state().await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/v1/assets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ion_create_token_requires_auth() {
        let state = test_state().await;
        // anonymous is rejected
        let status = post_ion(
            &state,
            "/v1/tokens",
            None,
            serde_json::json!({ "name": "t", "scopes": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // editor is not enough (admin-only credential mint)
        let editor_token = bootstrap_editor(&state, "ion-tok-editor@example.com").await;
        let status = post_ion(
            &state,
            "/v1/tokens",
            Some(&editor_token),
            serde_json::json!({ "name": "t", "scopes": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // admin can mint
        let admin_token = bootstrap_admin(&state, "ion-tok-admin@example.com").await;
        let status = post_ion(
            &state,
            "/v1/tokens",
            Some(&admin_token),
            serde_json::json!({ "name": "t", "scopes": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // -- ion endpoint resolution --

    async fn seed_asset(
        state: &Arc<AppState>,
        asset_type: tiletopia_server::AssetType,
    ) -> uuid::Uuid {
        use tiletopia_server::{Asset, AssetStatus};

        let id = uuid::Uuid::new_v4();
        state
            .db
            .create_asset(&Asset {
                id,
                name: "seeded".into(),
                asset_type,
                status: AssetStatus::Ready,
                created_at: chrono::Utc::now(),
                tile_count: 0,
                size_bytes: 0,
                description: String::new(),
                tags: vec![],
                owner_id: None,
            })
            .await
            .unwrap();
        id
    }

    async fn ion_endpoint(
        state: &Arc<AppState>,
        id: impl std::fmt::Display,
    ) -> (StatusCode, serde_json::Value) {
        let resp = get(state, &format!("/v1/assets/{id}/endpoint")).await;
        let status = resp.status();
        let body = serde_json::from_slice(&body_bytes(resp).await).unwrap_or_default();
        (status, body)
    }

    /// Path of an absolute url, so a test can follow the endpoint back into the
    /// router without depending on TILETOPIA_ION_BASE_URL.
    fn url_path(url: &str) -> String {
        let after_scheme = url.split_once("://").unwrap().1;
        format!("/{}", after_scheme.split_once('/').unwrap().1)
    }

    #[tokio::test]
    async fn ion_endpoint_for_terrain_points_at_a_bundle_cesium_can_load() {
        let state = test_state().await;
        let id = seed_asset(&state, tiletopia_server::AssetType::Terrain).await;
        seed_terrain_bundle(&state.data_dir, &id.to_string(), bundle_layer_json());

        let (status, body) = ion_endpoint(&state, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["type"], "TERRAIN");
        // CesiumJS maps attributions unguarded, so a missing field throws
        // before any tile is asked for
        assert_eq!(body["attributions"], serde_json::json!([]));

        let url = body["url"].as_str().unwrap();
        assert!(
            url.ends_with(&format!("/api/v1/terrain/bundles/{id}/")),
            "{url} is not a terrain directory"
        );

        // what CesiumTerrainProvider.fromUrl does with that url: append a
        // forward slash and fetch layer.json off it
        let path = url_path(url);
        let resp = get(&state, &format!("{path}layer.json")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let layer: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(layer["format"], "quantized-mesh-1.0");

        // and the tile template that layer.json advertises, resolved against it
        let tile = get(&state, &format!("{path}0/0/0.terrain?v=1.1.0")).await;
        assert_eq!(tile.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ion_endpoint_for_terrain_without_a_bundle_says_so() {
        let state = test_state().await;
        let id = seed_asset(&state, tiletopia_server::AssetType::Terrain).await;

        let (status, body) = ion_endpoint(&state, id).await;
        // a tileset.json url here would not even fail loudly: CesiumJS reads
        // the 404 on layer.json as a legacy heightmap layer and 404s every tile
        assert_eq!(status, StatusCode::NOT_FOUND);
        let message = body["message"].as_str().unwrap();
        assert!(
            message.contains("terrain_bundles") && message.contains(&id.to_string()),
            "{message} does not name the bundle the operator has to put there"
        );
    }

    #[tokio::test]
    async fn ion_endpoint_for_a_tileset_asset_is_unchanged() {
        let state = test_state().await;
        let id = seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;

        let (status, body) = ion_endpoint(&state, id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["type"], "3DTILES");
        assert_eq!(body["attributions"], serde_json::json!([]));
        assert!(
            body["url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/api/v1/assets/{id}/tileset.json")),
            "{}",
            body["url"]
        );
    }

    #[tokio::test]
    async fn ion_endpoint_for_imagery_refuses_instead_of_naming_a_tileset() {
        let state = test_state().await;
        let id = seed_asset(&state, tiletopia_server::AssetType::Imagery).await;

        let (status, body) = ion_endpoint(&state, id).await;
        // a tileset.json url typed IMAGERY sends cesium's TMS provider looking
        // for tilemapresource.xml next to a 3D Tiles tileset
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert!(body["url"].is_null());
        let message = body["message"].as_str().unwrap();
        assert!(
            message.contains("imagery"),
            "{message} does not say why there is no endpoint"
        );
    }

    async fn ion_assets(state: &Arc<AppState>) -> Vec<serde_json::Value> {
        let resp = get(state, "/v1/assets").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        body["items"].as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn ion_list_id_is_what_the_id_routes_take() {
        let state = test_state().await;
        let uuid = seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;

        let items = ion_assets(&state).await;
        let ion_id = items[0]["id"].as_i64().unwrap();

        // the number off the list is all an Ion client has, and
        // IonImageryProvider.fromAssetId refuses an id that is not one
        let resp = get(&state, &format!("/v1/assets/{ion_id}")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let asset: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
        assert_eq!(asset["id"].as_i64().unwrap(), ion_id);

        let (status, body) = ion_endpoint(&state, ion_id).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["url"]
                .as_str()
                .unwrap()
                .ends_with(&format!("/api/v1/assets/{uuid}/tileset.json")),
            "{} is not the asset the number stands for",
            body["url"]
        );
    }

    #[tokio::test]
    async fn ion_ids_are_never_shared_or_reused() {
        let state = test_state().await;
        seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;
        let second = seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;

        let ids: Vec<i64> = ion_assets(&state)
            .await
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);

        // deleting the newest asset must not put its number back in circulation
        state.db.delete_asset(second).await.unwrap();
        seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;
        let after: Vec<i64> = ion_assets(&state)
            .await
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect();
        assert_eq!(after.len(), 2);
        assert_ne!(after[0], after[1]);
        assert!(!ids.contains(&after.iter().copied().max().unwrap()));
    }

    #[tokio::test]
    async fn ion_ids_are_backfilled_for_rows_that_predate_them() {
        let state = test_state().await;
        seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;
        seed_asset(&state, tiletopia_server::AssetType::PointCloud).await;

        // what a database written before ion_id existed looks like
        sqlx::query("UPDATE assets SET ion_id = NULL")
            .execute(&state.db.pool)
            .await
            .unwrap();
        state.db.migrate().await.unwrap();

        let ids: Vec<i64> = ion_assets(&state)
            .await
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(ids.iter().all(|id| *id > 0));

        // and the routes take the numbers the backfill handed out
        let (status, _) = ion_endpoint(&state, ids[0]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn ion_endpoint_for_a_missing_asset_is_not_a_terrain_answer() {
        let state = test_state().await;
        let (status, body) = ion_endpoint(&state, uuid::Uuid::new_v4()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["url"].is_null());
    }

    // -- native asset write auth --

    // send a bodyless native asset write and return just the status.
    async fn asset_write(
        state: &Arc<AppState>,
        method: &str,
        uri: &str,
        token: Option<&str>,
    ) -> StatusCode {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        router(Arc::clone(state))
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    // multipart upload of a tiny .glb, which detects as Model.
    async fn upload_glb(
        state: &Arc<AppState>,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        upload_named(state, token, "t.glb").await
    }

    async fn upload_named(
        state: &Arc<AppState>,
        token: Option<&str>,
        filename: &str,
    ) -> (StatusCode, serde_json::Value) {
        let boundary = "tiletopiatestboundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\n\r\nglTF-bytes\r\n--{boundary}--\r\n"
        );
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/v1/assets")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            );
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    #[tokio::test]
    async fn native_create_asset_anonymous_rejected() {
        let state = test_state().await;
        let (status, _) = upload_glb(&state, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn native_create_asset_viewer_forbidden() {
        let state = test_state().await;
        let (viewer_token, _) = signup(&state, "native-viewer@example.com").await;
        let (status, _) = upload_glb(&state, Some(&viewer_token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn native_create_asset_editor_succeeds() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "native-editor@example.com").await;
        let (status, asset) = upload_glb(&state, Some(&editor_token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(asset["id"].as_str().is_some());
    }

    #[tokio::test]
    async fn upload_returns_tiling_job_id() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "upload-job-editor@example.com").await;

        let (status, asset) = upload_named(&state, Some(&editor_token), "cloud.las").await;
        assert_eq!(status, StatusCode::CREATED);
        let job_id = uuid::Uuid::parse_str(asset["job_id"].as_str().expect("job_id in response"))
            .expect("job_id is a uuid");
        let job = state.db.get_job(job_id).await.unwrap().expect("job exists");
        assert_eq!(
            job.asset_id,
            uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap()
        );

        // a model goes to the external tiler on upload the same way
        let (status, model) = upload_glb(&state, Some(&editor_token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(model["job_id"].as_str().is_some());

        // terrain has no 3D Tiles path at all, so it reports no job
        let (status, terrain) = upload_named(&state, Some(&editor_token), "dem.tif").await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(terrain.get("job_id").is_none());
    }

    #[tokio::test]
    async fn asset_jobs_route_finds_the_tiling_job_without_the_upload_response() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "asset-jobs-editor@example.com").await;

        let (status, asset) = upload_named(&state, Some(&editor_token), "cloud.las").await;
        assert_eq!(status, StatusCode::CREATED);
        let uri = format!("/api/v1/assets/{}/jobs", asset["id"].as_str().unwrap());

        // the route a client that only listed the asset has to use
        let req = Request::builder()
            .method("GET")
            .uri(&uri)
            .header("authorization", format!("Bearer {editor_token}"));
        let resp = router(Arc::clone(&state))
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let jobs: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(jobs.as_array().expect("a list of jobs").len(), 1);
        assert_eq!(jobs[0]["id"], asset["job_id"]);
        assert_eq!(jobs[0]["asset_id"], asset["id"]);
    }

    #[tokio::test]
    async fn native_delete_asset_requires_editor() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "native-del-editor@example.com").await;
        let (status, asset) = upload_glb(&state, Some(&editor_token)).await;
        assert_eq!(status, StatusCode::CREATED);
        let uri = format!("/api/v1/assets/{}", asset["id"].as_str().unwrap());

        assert_eq!(
            asset_write(&state, "DELETE", &uri, None).await,
            StatusCode::UNAUTHORIZED
        );

        let (viewer_token, _) = signup(&state, "native-del-viewer@example.com").await;
        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&viewer_token)).await,
            StatusCode::FORBIDDEN
        );

        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&editor_token)).await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn native_start_tiling_requires_editor() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "native-tile-editor@example.com").await;
        let (status, asset) = upload_glb(&state, Some(&editor_token)).await;
        assert_eq!(status, StatusCode::CREATED);
        let uri = format!("/api/v1/assets/{}/tile", asset["id"].as_str().unwrap());

        assert_eq!(
            asset_write(&state, "POST", &uri, None).await,
            StatusCode::UNAUTHORIZED
        );

        let (viewer_token, _) = signup(&state, "native-tile-viewer@example.com").await;
        assert_eq!(
            asset_write(&state, "POST", &uri, Some(&viewer_token)).await,
            StatusCode::FORBIDDEN
        );

        // the editor gets past authz; the job itself may still fail on content
        let status = asset_write(&state, "POST", &uri, Some(&editor_token)).await;
        assert!(status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN);
    }

    // -- catalog dataset add auth --

    async fn catalog_add(state: &Arc<AppState>, token: Option<&str>) -> StatusCode {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/v1/catalog/copernicus-dem-30/add")
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = serde_json::json!({
            "name": "dem tile",
            "bounds": { "west": 0.0, "south": 0.0, "east": 1.0, "north": 1.0 },
        });
        router(Arc::clone(state))
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn catalog_add_anonymous_rejected() {
        let state = test_state().await;
        assert_eq!(catalog_add(&state, None).await, StatusCode::UNAUTHORIZED);
        assert!(state.db.list_assets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn catalog_add_viewer_forbidden() {
        let state = test_state().await;
        let (viewer_token, _) = signup(&state, "catalog-viewer@example.com").await;
        assert_eq!(
            catalog_add(&state, Some(&viewer_token)).await,
            StatusCode::FORBIDDEN
        );
        assert!(state.db.list_assets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn catalog_add_editor_records_owner() {
        let state = test_state().await;
        let (editor_token, uid) =
            bootstrap_editor_with_id(&state, "catalog-editor@example.com").await;
        assert_eq!(
            catalog_add(&state, Some(&editor_token)).await,
            StatusCode::CREATED
        );

        let assets = state.db.list_assets().await.unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].owner_id.as_deref(), Some(uid.as_str()));
    }

    // -- realtime collaboration websocket --

    // serve the router on a loopback port so a real websocket handshake can run.
    async fn serve(state: &Arc<AppState>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(Arc::clone(state));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn realtime_route_is_mounted_and_rejects_anonymous() {
        let state = test_state().await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/realtime/room-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 401, not 404: the route exists and the join gate is what refused
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // a handshake request. into_client_request fills in the five required ws
    // headers; `subprotocol` is the raw Sec-WebSocket-Protocol offer so a test
    // can send a malformed one.
    fn ws_request(
        addr: std::net::SocketAddr,
        path: &str,
        subprotocol: Option<&str>,
        bearer: Option<&str>,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderName};

        let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
        if let Some(p) = subprotocol {
            req.headers_mut().insert(
                HeaderName::from_static("sec-websocket-protocol"),
                p.parse().unwrap(),
            );
        }
        if let Some(t) = bearer {
            req.headers_mut()
                .insert(AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
        }
        req
    }

    fn room_request(
        addr: std::net::SocketAddr,
        subprotocol: Option<&str>,
    ) -> tokio_tungstenite::tungstenite::http::Request<()> {
        ws_request(addr, "/api/v1/realtime/room-1", subprotocol, None)
    }

    // the handshake a browser sends: new WebSocket(url, ["bearer", jwt])
    fn bearer_offer(token: &str) -> String {
        format!("bearer, {token}")
    }

    type ClientWs = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect_room(addr: std::net::SocketAddr, room: &str, token: &str) -> ClientWs {
        let req = ws_request(
            addr,
            &format!("/api/v1/realtime/{room}"),
            Some(&bearer_offer(token)),
            None,
        );
        tokio_tungstenite::connect_async(req).await.unwrap().0
    }

    async fn send_join(ws: &mut ClientWs, room: &str, user_name: &str) {
        use futures::SinkExt;
        let msg = serde_json::json!({
            "type": "Join",
            "user_id": "self-assigned-id",
            "asset_id": room,
            "user_name": user_name,
        })
        .to_string();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
            .await
            .unwrap();
    }

    // sorted user ids of the next Presence broadcast this connection receives
    async fn next_roster(ws: &mut ClientWs) -> Vec<String> {
        use futures::StreamExt;
        let msg = ws.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["type"], "Presence");
        let mut ids: Vec<String> = v["users"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["user_id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    async fn expect_handshake_rejected(
        addr: std::net::SocketAddr,
        protocol: Option<&str>,
        what: &str,
    ) {
        match tokio_tungstenite::connect_async(room_request(addr, protocol)).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status().as_u16(), 401, "{what}");
            }
            Err(e) => panic!("{what}: expected an http rejection, got {e:?}"),
            Ok(_) => panic!("{what}: must not upgrade"),
        }
    }

    #[tokio::test]
    async fn realtime_handshake_rejects_missing_and_bad_credentials() {
        let state = test_state().await;
        let (token, _uid) = signup(&state, "collab-reject@example.com").await;
        let addr = serve(&state).await;

        expect_handshake_rejected(addr, None, "no credential").await;
        expect_handshake_rejected(addr, Some("bearer, not-a-jwt"), "forged token").await;
        // the marker alone carries no token
        expect_handshake_rejected(addr, Some("bearer"), "marker only").await;
        // a bare token with no marker is not the contract
        expect_handshake_rejected(addr, Some(&token), "token without marker").await;
        // an unrelated subprotocol is not a credential
        expect_handshake_rejected(addr, Some("graphql-ws"), "wrong subprotocol").await;
    }

    #[tokio::test]
    async fn realtime_query_string_token_is_rejected() {
        let state = test_state().await;
        let (token, _uid) = signup(&state, "collab-query@example.com").await;
        let addr = serve(&state).await;

        // ?token= used to authenticate here; it must not any more
        let req = ws_request(
            addr,
            &format!("/api/v1/realtime/room-1?token={token}"),
            None,
            None,
        );
        match tokio_tungstenite::connect_async(req).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status().as_u16(), 401);
            }
            Err(e) => panic!("expected an http rejection, got {e:?}"),
            Ok(_) => panic!("a query-string token must not upgrade"),
        }
    }

    #[tokio::test]
    async fn realtime_join_round_trip_with_viewer_token() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let state = test_state().await;
        // a plain signup is a viewer, which is enough to join a room
        let (token, uid) = signup(&state, "collab@example.com").await;
        let addr = serve(&state).await;

        let (mut ws, resp) =
            tokio_tungstenite::connect_async(room_request(addr, Some(&bearer_offer(&token))))
                .await
                .unwrap();
        assert_eq!(resp.status().as_u16(), 101);
        // the marker is echoed so a browser accepts the upgrade, the token is not
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("bearer")
        );

        ws.send(Message::Text(
            serde_json::json!({
                "type": "Join",
                "user_id": "self-assigned-id",
                "asset_id": "room-1",
                "user_name": "Ann",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        // the server answers a join by broadcasting the room presence list, with
        // the id taken from the token rather than the one the client picked
        let msg = ws.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["type"], "Presence");
        assert_eq!(v["users"][0]["user_id"], uid);
        assert_eq!(v["users"][0]["user_name"], "Ann");
    }

    #[tokio::test]
    async fn realtime_authorization_header_also_works() {
        let state = test_state().await;
        let (token, _uid) = signup(&state, "collab-hdr@example.com").await;
        let addr = serve(&state).await;

        // non-browser clients can use the header and offer no subprotocol
        let req = ws_request(addr, "/api/v1/realtime/room-1", None, Some(&token));
        let (_ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 101);
        // nothing was offered, so nothing is selected
        assert!(resp.headers().get("sec-websocket-protocol").is_none());
    }

    #[tokio::test]
    async fn realtime_sender_cannot_impersonate_another_member() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let state = test_state().await;
        let (token_a, uid_a) = signup(&state, "collab-a@example.com").await;
        let (token_b, uid_b) = signup(&state, "collab-b@example.com").await;
        assert_ne!(uid_a, uid_b);
        let addr = serve(&state).await;

        let (mut a, _) =
            tokio_tungstenite::connect_async(room_request(addr, Some(&bearer_offer(&token_a))))
                .await
                .unwrap();
        let (mut b, _) =
            tokio_tungstenite::connect_async(room_request(addr, Some(&bearer_offer(&token_b))))
                .await
                .unwrap();

        // A joins claiming to be B
        a.send(Message::Text(
            serde_json::json!({
                "type": "Join",
                "user_id": uid_b,
                "asset_id": "room-1",
                "user_name": "Impostor",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        // B sees the presence entry attributed to A, not to itself
        let msg = b.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["type"], "Presence");
        assert_eq!(v["users"].as_array().unwrap().len(), 1);
        assert_eq!(v["users"][0]["user_id"], uid_a);

        // and a chat claiming B's id is rebroadcast under A's real sub
        a.send(Message::Text(
            serde_json::json!({
                "type": "Chat",
                "user_id": uid_b,
                "user_name": "Impostor",
                "message": "transfer the assets",
                "timestamp": "2026-07-26T00:00:00Z",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        let msg = b.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["type"], "Chat");
        assert_eq!(v["user_id"], uid_a, "sender id must come from the token");
        assert_eq!(v["message"], "transfer the assets");
        // the display name is the client's to choose, only the id is stamped
        assert_eq!(v["user_name"], "Impostor");
    }

    #[tokio::test]
    async fn realtime_presence_is_per_connection_not_per_account() {
        let state = test_state().await;
        let (token_a, uid_a) = signup(&state, "collab-tabs@example.com").await;
        let (token_b, uid_b) = signup(&state, "collab-watcher@example.com").await;
        let addr = serve(&state).await;

        // the watcher stays connected, so every roster change is observable
        let mut watcher = connect_room(addr, "room-1", &token_b).await;
        send_join(&mut watcher, "room-1", "Watcher").await;
        assert_eq!(
            next_roster(&mut watcher).await,
            std::slice::from_ref(&uid_b)
        );

        // two tabs of one account
        let mut expected = vec![uid_a, uid_b.clone()];
        expected.sort();

        let mut tab1 = connect_room(addr, "room-1", &token_a).await;
        send_join(&mut tab1, "room-1", "Ann").await;
        assert_eq!(next_roster(&mut watcher).await, expected);

        let mut tab2 = connect_room(addr, "room-1", &token_a).await;
        send_join(&mut tab2, "room-1", "Ann").await;
        assert_eq!(next_roster(&mut watcher).await, expected);

        // one tab closes: the account is still present on the other one
        tab1.close(None).await.unwrap();
        assert_eq!(
            next_roster(&mut watcher).await,
            expected,
            "one closed tab must not remove an account that is still connected"
        );

        // the last tab closes: now the departure is announced
        tab2.close(None).await.unwrap();
        assert_eq!(next_roster(&mut watcher).await, [uid_b]);
    }

    #[tokio::test]
    async fn realtime_rejects_rooms_past_the_per_user_cap() {
        use futures::StreamExt;
        use tiletopia_server::realtime::{MAX_ROOMS_PER_USER, ROOM_LIMIT_CLOSE_CODE};
        use tokio_tungstenite::tungstenite::Message;

        let state = test_state().await;
        let (token, _uid) = signup(&state, "collab-cap@example.com").await;
        let addr = serve(&state).await;

        // rooms live only while a connection holds them, so hold them all open
        let mut held = Vec::new();
        for i in 0..MAX_ROOMS_PER_USER {
            held.push(connect_room(addr, &format!("room-{i}"), &token).await);
        }

        // the next room is refused with a code the client can branch on
        let mut over = connect_room(addr, "over-the-cap", &token).await;
        match over.next().await {
            Some(Ok(Message::Close(Some(frame)))) => {
                assert_eq!(u16::from(frame.code), ROOM_LIMIT_CLOSE_CODE);
            }
            other => panic!("expected a room-limit close, got {other:?}"),
        }

        // freeing a room frees the slot. the server's cleanup runs after our
        // close is delivered, so retry until it has.
        held.pop().unwrap().close(None).await.unwrap();
        let mut accepted = false;
        for _ in 0..40 {
            let mut ws = connect_room(addr, "after-freeing-one", &token).await;
            send_join(&mut ws, "after-freeing-one", "Ann").await;
            match ws.next().await {
                Some(Ok(Message::Close(_))) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Some(Ok(Message::Text(text))) => {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(v["type"], "Presence");
                    accepted = true;
                    break;
                }
                other => panic!("unexpected reply: {other:?}"),
            }
        }
        assert!(accepted, "a freed room slot must let a new room through");
    }

    // -- per-asset ownership --

    #[test]
    fn may_modify_asset_policy() {
        use tiletopia_server::{auth::Claims, may_modify_asset};
        let claims = |sub: &str, role: &str| Claims {
            sub: sub.into(),
            exp: 0,
            role: role.into(),
        };
        let owner = claims("user-a", "editor");
        let other = claims("user-b", "editor");
        let admin = claims("user-c", "admin");

        assert!(may_modify_asset(&owner, Some("user-a")));
        assert!(!may_modify_asset(&other, Some("user-a")));
        assert!(may_modify_asset(&admin, Some("user-a")));
        // legacy rows have no owner and stay writable for any editor
        assert!(may_modify_asset(&other, None));
    }

    #[tokio::test]
    async fn upload_records_owner_id() {
        let state = test_state().await;
        let (editor_token, uid) = bootstrap_editor_with_id(&state, "own-create@example.com").await;
        let (status, asset) = upload_glb(&state, Some(&editor_token)).await;
        assert_eq!(status, StatusCode::CREATED);

        // owner_id is authz-internal and must not be exposed in the response
        assert!(asset.get("owner_id").is_none(), "owner_id must not leak");

        let id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();
        let stored = state.db.get_asset(id).await.unwrap().unwrap();
        assert_eq!(stored.owner_id.as_deref(), Some(uid.as_str()));
    }

    #[tokio::test]
    async fn ion_create_records_owner_id() {
        let state = test_state().await;
        let (editor_token, uid) =
            bootstrap_editor_with_id(&state, "own-ion-editor@example.com").await;
        let status = post_ion(
            &state,
            "/v1/assets",
            Some(&editor_token),
            serde_json::json!({ "name": "owned", "type": "3DTILES" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let assets = state.db.list_assets().await.unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].owner_id.as_deref(), Some(uid.as_str()));
    }

    #[tokio::test]
    async fn asset_delete_is_owner_or_admin_only() {
        let state = test_state().await;
        let owner_token = bootstrap_editor(&state, "own-del-owner@example.com").await;
        let other_token = bootstrap_editor(&state, "own-del-other@example.com").await;

        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let uri = format!("/api/v1/assets/{}", asset["id"].as_str().unwrap());

        // a second editor is not trusted with someone else's asset
        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&other_token)).await,
            StatusCode::FORBIDDEN
        );
        let id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();
        assert!(state.db.get_asset(id).await.unwrap().is_some());

        // the owner can
        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&owner_token)).await,
            StatusCode::NO_CONTENT
        );
        assert!(state.db.get_asset(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn admin_can_delete_another_users_asset() {
        let state = test_state().await;
        let owner_token = bootstrap_editor(&state, "own-del-admin-owner@example.com").await;
        let admin_token = bootstrap_admin(&state, "own-del-admin@example.com").await;

        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let uri = format!("/api/v1/assets/{}", asset["id"].as_str().unwrap());
        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&admin_token)).await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn legacy_ownerless_asset_accepts_any_editor() {
        use tiletopia_server::{Asset, AssetStatus, AssetType};
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "own-legacy-editor@example.com").await;

        // a row from before owner_id existed
        let asset = Asset {
            id: uuid::Uuid::new_v4(),
            name: "legacy.glb".into(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: vec![],
            owner_id: None,
        };
        state.db.create_asset(&asset).await.unwrap();

        let uri = format!("/api/v1/assets/{}", asset.id);
        assert_eq!(
            asset_write(&state, "DELETE", &uri, Some(&editor_token)).await,
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn retile_is_owner_or_admin_only() {
        let state = test_state().await;
        let owner_token = bootstrap_editor(&state, "own-tile-owner@example.com").await;
        let other_token = bootstrap_editor(&state, "own-tile-other@example.com").await;
        let admin_token = bootstrap_admin(&state, "own-tile-admin@example.com").await;

        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let uri = format!("/api/v1/assets/{}/tile", asset["id"].as_str().unwrap());

        assert_eq!(
            asset_write(&state, "POST", &uri, Some(&other_token)).await,
            StatusCode::FORBIDDEN
        );
        // owner and admin get past authz; the job itself may still fail on content
        for token in [&owner_token, &admin_token] {
            let status = asset_write(&state, "POST", &uri, Some(token)).await;
            assert!(
                status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                "unexpected {status}"
            );
        }
    }

    // -- asset list visibility --

    async fn list_assets(
        state: &Arc<AppState>,
        token: Option<&str>,
        query: &str,
    ) -> (StatusCode, Vec<serde_json::Value>) {
        let mut req = Request::builder().uri(format!("/api/v1/assets{query}"));
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_default();
        (status, v)
    }

    fn asset_ids(assets: &[serde_json::Value]) -> Vec<String> {
        let mut ids: Vec<String> = assets
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn asset_list_requires_a_token() {
        let state = test_state().await;
        let (status, _) = list_assets(&state, None, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // and on the search path too, which is a different branch of the handler
        let (status, _) = list_assets(&state, None, "?q=t").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn asset_list_shows_only_your_own_and_legacy_rows() {
        use tiletopia_server::{Asset, AssetStatus, AssetType};
        let state = test_state().await;
        let a_token = bootstrap_editor(&state, "list-a@example.com").await;
        let b_token = bootstrap_editor(&state, "list-b@example.com").await;
        let admin_token = bootstrap_admin(&state, "list-admin@example.com").await;

        let (_s, a_asset) = upload_glb(&state, Some(&a_token)).await;
        let (_s, b_asset) = upload_glb(&state, Some(&b_token)).await;
        let a_id = a_asset["id"].as_str().unwrap().to_string();
        let b_id = b_asset["id"].as_str().unwrap().to_string();

        // a row from before owner_id existed stays visible to everyone
        let legacy = Asset {
            id: uuid::Uuid::new_v4(),
            name: "legacy.glb".into(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: vec![],
            owner_id: None,
        };
        state.db.create_asset(&legacy).await.unwrap();
        let legacy_id = legacy.id.to_string();

        let (status, seen) = list_assets(&state, Some(&a_token), "").await;
        assert_eq!(status, StatusCode::OK);
        let mut expected = vec![a_id.clone(), legacy_id.clone()];
        expected.sort();
        assert_eq!(asset_ids(&seen), expected, "A must not see B's asset");

        let (_s, seen) = list_assets(&state, Some(&b_token), "").await;
        let mut expected = vec![b_id.clone(), legacy_id.clone()];
        expected.sort();
        assert_eq!(asset_ids(&seen), expected);

        // an admin sees every asset
        let (_s, seen) = list_assets(&state, Some(&admin_token), "").await;
        let mut all = vec![a_id.clone(), b_id.clone(), legacy_id];
        all.sort();
        assert_eq!(asset_ids(&seen), all);
    }

    #[tokio::test]
    async fn asset_search_is_filtered_by_owner_too() {
        let state = test_state().await;
        let a_token = bootstrap_editor(&state, "search-a@example.com").await;
        let b_token = bootstrap_editor(&state, "search-b@example.com").await;

        let (_s, a_asset) = upload_glb(&state, Some(&a_token)).await;
        upload_glb(&state, Some(&b_token)).await;

        // both assets are named t.glb, so an unfiltered search would return both
        let (status, seen) = list_assets(&state, Some(&a_token), "?q=t.glb").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(asset_ids(&seen), vec![a_asset["id"].as_str().unwrap()]);
    }

    // -- annotation write authz --

    fn asset_uuid(asset: &serde_json::Value) -> uuid::Uuid {
        uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap()
    }

    async fn post_annotation(
        state: &Arc<AppState>,
        asset_id: &str,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/assets/{asset_id}/annotations"))
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = serde_json::json!({
            "text": "a note",
            "longitude": 7.42,
            "latitude": 43.73,
        });
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, v)
    }

    async fn delete_annotation(
        state: &Arc<AppState>,
        asset_id: &str,
        annotation_id: &str,
        token: Option<&str>,
    ) -> StatusCode {
        asset_write(
            state,
            "DELETE",
            &format!("/api/v1/assets/{asset_id}/annotations/{annotation_id}"),
            token,
        )
        .await
    }

    #[tokio::test]
    async fn annotation_create_is_editor_and_owner_only() {
        let state = test_state().await;
        let (owner_token, owner_id) =
            bootstrap_editor_with_id(&state, "ann-owner@example.com").await;
        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let asset_id = asset["id"].as_str().unwrap();

        assert_eq!(
            post_annotation(&state, asset_id, None).await.0,
            StatusCode::UNAUTHORIZED
        );

        let (viewer_token, _) = signup(&state, "ann-viewer@example.com").await;
        assert_eq!(
            post_annotation(&state, asset_id, Some(&viewer_token))
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // the Edit tier is not enough on someone else's asset
        let other_token = bootstrap_editor(&state, "ann-other@example.com").await;
        assert_eq!(
            post_annotation(&state, asset_id, Some(&other_token))
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        assert!(
            state
                .db
                .list_annotations(asset_uuid(&asset))
                .await
                .unwrap()
                .is_empty()
        );

        // the owner can, and the note is attributed to them
        let (status, created) = post_annotation(&state, asset_id, Some(&owner_token)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["created_by"], owner_id);

        // and so can an admin
        let admin_token = bootstrap_admin(&state, "ann-admin@example.com").await;
        assert_eq!(
            post_annotation(&state, asset_id, Some(&admin_token))
                .await
                .0,
            StatusCode::CREATED
        );

        assert_eq!(
            state
                .db
                .list_annotations(asset_uuid(&asset))
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn annotation_create_on_a_missing_asset_is_not_found() {
        let state = test_state().await;
        let editor_token = bootstrap_editor(&state, "ann-missing@example.com").await;
        let (status, _) = post_annotation(
            &state,
            "00000000-0000-0000-0000-000000000000",
            Some(&editor_token),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn annotation_delete_is_editor_and_owner_only() {
        let state = test_state().await;
        let owner_token = bootstrap_editor(&state, "anndel-owner@example.com").await;
        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let asset_id = asset["id"].as_str().unwrap();

        let (_s, created) = post_annotation(&state, asset_id, Some(&owner_token)).await;
        let ann_id = created["id"].as_str().unwrap();

        assert_eq!(
            delete_annotation(&state, asset_id, ann_id, None).await,
            StatusCode::UNAUTHORIZED
        );

        let (viewer_token, _) = signup(&state, "anndel-viewer@example.com").await;
        assert_eq!(
            delete_annotation(&state, asset_id, ann_id, Some(&viewer_token)).await,
            StatusCode::FORBIDDEN
        );

        let other_token = bootstrap_editor(&state, "anndel-other@example.com").await;
        assert_eq!(
            delete_annotation(&state, asset_id, ann_id, Some(&other_token)).await,
            StatusCode::FORBIDDEN
        );

        // the rejected deletes left it alone
        assert_eq!(
            state
                .db
                .list_annotations(asset_uuid(&asset))
                .await
                .unwrap()
                .len(),
            1
        );

        assert_eq!(
            delete_annotation(&state, asset_id, ann_id, Some(&owner_token)).await,
            StatusCode::NO_CONTENT
        );
        assert!(
            state
                .db
                .list_annotations(asset_uuid(&asset))
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Owning one asset must not be a way to delete an annotation hanging off
    /// another one: the delete is scoped to the asset in the path.
    #[tokio::test]
    async fn annotation_delete_is_scoped_to_its_asset() {
        let state = test_state().await;
        let victim_token = bootstrap_editor(&state, "annscope-victim@example.com").await;
        let attacker_token = bootstrap_editor(&state, "annscope-attacker@example.com").await;

        let (_s, victim_asset) = upload_glb(&state, Some(&victim_token)).await;
        let victim_id = victim_asset["id"].as_str().unwrap();
        let (_s, note) = post_annotation(&state, victim_id, Some(&victim_token)).await;
        let note_id = note["id"].as_str().unwrap();

        let (_s, attacker_asset) = upload_glb(&state, Some(&attacker_token)).await;
        let attacker_id = attacker_asset["id"].as_str().unwrap();

        // authorized against their own asset, aiming at someone else's annotation
        assert_eq!(
            delete_annotation(&state, attacker_id, note_id, Some(&attacker_token)).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            state
                .db
                .list_annotations(asset_uuid(&victim_asset))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn annotation_list_stays_readable_with_any_token() {
        let state = test_state().await;
        let owner_token = bootstrap_editor(&state, "annlist-owner@example.com").await;
        let (_s, asset) = upload_glb(&state, Some(&owner_token)).await;
        let asset_id = asset["id"].as_str().unwrap();
        post_annotation(&state, asset_id, Some(&owner_token)).await;

        let (viewer_token, _) = signup(&state, "annlist-viewer@example.com").await;
        let resp = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/assets/{asset_id}/annotations"))
                    .header("authorization", format!("Bearer {viewer_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.len(), 1);
    }

    // -- plugin registry authz --

    fn plugin_body(id: &str) -> serde_json::Value {
        serde_json::json!({
            "manifest": {
                "id": id,
                "name": "Test Plugin",
                "version": "1.0.0",
                "description": "a test plugin",
                "author": "tests",
                "license": "AGPL-3.0-or-later",
                "entry_point": "main.wasm",
                "capabilities": ["transform"],
                "config_schema": null,
            },
            "config": {},
        })
    }

    async fn plugin_request(
        state: &Arc<AppState>,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> StatusCode {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        router(Arc::clone(state))
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn plugin_install_is_admin_only() {
        let state = test_state().await;
        let uri = "/api/v1/plugins/registry";

        assert_eq!(
            plugin_request(&state, "POST", uri, None, Some(plugin_body("p1"))).await,
            StatusCode::UNAUTHORIZED
        );

        let (viewer_token, _) = signup(&state, "plug-viewer@example.com").await;
        assert_eq!(
            plugin_request(
                &state,
                "POST",
                uri,
                Some(&viewer_token),
                Some(plugin_body("p1"))
            )
            .await,
            StatusCode::FORBIDDEN
        );

        // a plugin runs for the whole server, so the Edit tier is not enough
        let editor_token = bootstrap_editor(&state, "plug-editor@example.com").await;
        assert_eq!(
            plugin_request(
                &state,
                "POST",
                uri,
                Some(&editor_token),
                Some(plugin_body("p1"))
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert!(state.db.list_plugins().await.unwrap().is_empty());

        let admin_token = bootstrap_admin(&state, "plug-admin@example.com").await;
        assert_eq!(
            plugin_request(
                &state,
                "POST",
                uri,
                Some(&admin_token),
                Some(plugin_body("p1"))
            )
            .await,
            StatusCode::CREATED
        );
        assert_eq!(state.db.list_plugins().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn plugin_mutations_are_admin_only() {
        let state = test_state().await;
        let admin_token = bootstrap_admin(&state, "plugmut-admin@example.com").await;
        let editor_token = bootstrap_editor(&state, "plugmut-editor@example.com").await;
        assert_eq!(
            plugin_request(
                &state,
                "POST",
                "/api/v1/plugins/registry",
                Some(&admin_token),
                Some(plugin_body("p2"))
            )
            .await,
            StatusCode::CREATED
        );

        let config = serde_json::json!({ "config": { "k": "v" } });
        let mutations: [(&str, &str, Option<serde_json::Value>); 4] = [
            (
                "PUT",
                "/api/v1/plugins/registry/p2/config",
                Some(config.clone()),
            ),
            ("POST", "/api/v1/plugins/registry/p2/disable", None),
            ("POST", "/api/v1/plugins/registry/p2/enable", None),
            ("DELETE", "/api/v1/plugins/registry/p2", None),
        ];

        for (method, uri, body) in &mutations {
            assert_eq!(
                plugin_request(&state, method, uri, None, body.clone()).await,
                StatusCode::UNAUTHORIZED,
                "anonymous {method} {uri}"
            );
            assert_eq!(
                plugin_request(&state, method, uri, Some(&editor_token), body.clone()).await,
                StatusCode::FORBIDDEN,
                "editor {method} {uri}"
            );
        }
        // nothing an editor tried went through
        let plugins = state.db.list_plugins().await.unwrap();
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].enabled);
        assert_eq!(plugins[0].config, serde_json::json!({}));

        for (method, uri, body) in &mutations {
            let status =
                plugin_request(&state, method, uri, Some(&admin_token), body.clone()).await;
            assert!(status.is_success(), "admin {method} {uri} got {status}");
        }
        assert!(state.db.list_plugins().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn plugin_reads_stay_open_to_any_token() {
        let state = test_state().await;
        let admin_token = bootstrap_admin(&state, "plugread-admin@example.com").await;
        plugin_request(
            &state,
            "POST",
            "/api/v1/plugins/registry",
            Some(&admin_token),
            Some(plugin_body("p3")),
        )
        .await;

        let (viewer_token, _) = signup(&state, "plugread-viewer@example.com").await;
        assert_eq!(
            plugin_request(
                &state,
                "GET",
                "/api/v1/plugins/registry",
                Some(&viewer_token),
                None
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            plugin_request(
                &state,
                "GET",
                "/api/v1/plugins/registry/p3",
                Some(&viewer_token),
                None
            )
            .await,
            StatusCode::OK
        );
    }

    // -- role parsing --

    /// A token carrying a role we don't know must land in no tier at all. This
    /// is the decision every route gate reads, so a forged or foreign role
    /// cannot fall through to editor or admin.
    #[test]
    fn unknown_role_in_a_token_grants_nothing() {
        use tiletopia_server::auth::Claims;
        let with_role = |role: &str| Claims {
            sub: "user-a".into(),
            exp: 0,
            role: role.into(),
        };

        for role in [
            "",
            "root",
            "superuser",
            "owner",
            "Admin",
            "ADMIN",
            "admin ",
            " admin",
            "admin,viewer",
        ] {
            let claims = with_role(role);
            assert!(!claims.can_admin(), "can_admin on '{role}'");
            assert!(!claims.can_write(), "can_write on '{role}'");
            assert!(claims.parsed_role().is_none(), "parsed_role on '{role}'");
        }

        assert!(with_role("admin").can_admin());
        assert!(with_role("admin").can_write());
        assert!(!with_role("editor").can_admin());
        assert!(with_role("editor").can_write());
        assert!(!with_role("viewer").can_write());
    }

    /// An unknown role is not an admin, so it cannot reach another user's asset
    /// through the ownership rule either.
    #[test]
    fn unknown_role_is_not_an_owner_override() {
        use tiletopia_server::{auth::Claims, may_modify_asset, may_view_asset};
        let claims = Claims {
            sub: "user-a".into(),
            exp: 0,
            role: "superuser".into(),
        };
        assert!(!may_modify_asset(&claims, Some("user-b")));
        assert!(!may_view_asset(&claims, Some("user-b")));
        // its own assets stay its own: that is identity, not role
        assert!(may_modify_asset(&claims, Some("user-a")));
        assert!(may_view_asset(&claims, Some("user-a")));
    }

    // -- persistence and the job worker --

    const PLY_FIXTURE: &str = "\
ply\nformat ascii 1.0\nelement vertex 4\nproperty float x\nproperty float y\n\
property float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\n\
end_header\n0.0 0.0 0.0 255 0 0\n1.0 0.0 0.0 0 255 0\n0.0 1.0 0.0 0 0 255\n\
1.0 1.0 1.0 255 255 255\n";

    /// Rows written through one `Database` are still there after the handle is
    /// dropped and the same file is reopened, which is the whole point of
    /// keeping assets and jobs in SQLite rather than in process memory.
    #[tokio::test]
    async fn assets_and_jobs_persist_across_reopen() {
        use tiletopia_server::db::{Database, JobRecord, JobStatus};
        use tiletopia_server::{Asset, AssetStatus, AssetType};

        let dir = tempfile::tempdir().unwrap();
        let db_url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("tiletopia.db").display()
        );

        let asset = Asset {
            id: uuid::Uuid::new_v4(),
            name: "survey.ply".into(),
            asset_type: AssetType::PointCloud,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 7,
            size_bytes: 4096,
            description: "reopen me".into(),
            tags: vec!["survey".into(), "2026".into()],
            owner_id: Some("user-a".into()),
        };
        let job = JobRecord {
            id: uuid::Uuid::new_v4(),
            asset_id: asset.id,
            status: JobStatus::Done,
            progress: 1.0,
            input_path: "/data/survey.ply".into(),
            output_format: "3dtiles".into(),
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            completed_at: Some(chrono::Utc::now()),
            error: None,
            points_processed: 42,
            tiles_written: 7,
            placement: tiletopia_server::db::ModelPlacement::default(),
        };

        {
            let db = Database::new(&db_url).await.unwrap();
            db.migrate().await.unwrap();
            db.create_asset(&asset).await.unwrap();
            db.create_job(&job).await.unwrap();
            db.pool.close().await;
        }

        // migrate again on reopen, the way a restarted server does
        let db = Database::new(&db_url).await.unwrap();
        db.migrate().await.unwrap();

        let stored = db
            .get_asset(asset.id)
            .await
            .unwrap()
            .expect("asset lost on reopen");
        assert_eq!(stored.name, asset.name);
        assert_eq!(stored.asset_type, AssetType::PointCloud);
        assert!(matches!(stored.status, AssetStatus::Ready));
        assert_eq!(stored.tile_count, 7);
        assert_eq!(stored.size_bytes, 4096);
        assert_eq!(stored.description, "reopen me");
        assert_eq!(stored.tags, vec!["survey".to_string(), "2026".to_string()]);
        assert_eq!(stored.owner_id.as_deref(), Some("user-a"));

        let stored_job = db
            .get_job(job.id)
            .await
            .unwrap()
            .expect("job lost on reopen");
        assert_eq!(stored_job.asset_id, asset.id);
        assert_eq!(stored_job.status, JobStatus::Done);
        assert!((stored_job.progress - 1.0).abs() < 1e-9);
        assert_eq!(stored_job.input_path, "/data/survey.ply");
        assert_eq!(stored_job.points_processed, 42);
        assert_eq!(stored_job.tiles_written, 7);
        assert!(stored_job.started_at.is_some());
        assert!(stored_job.completed_at.is_some());

        assert_eq!(db.list_assets().await.unwrap().len(), 1);
        assert_eq!(db.list_jobs_for_asset(asset.id).await.unwrap().len(), 1);
    }

    /// Submitting returns immediately with a queued job, the background worker
    /// picks it up and drives it to done. `started_at` is the durable witness of
    /// the running transition: tiling four points takes milliseconds, so polling
    /// for `status == Running` would race the worker.
    #[tokio::test]
    async fn job_lifecycle_queued_to_running_to_done() {
        use tiletopia_server::db::JobStatus;
        use tiletopia_server::{Asset, AssetStatus, AssetType};

        let state = test_state().await;

        let id = uuid::Uuid::new_v4();
        let input_dir = state.data_dir.join(id.to_string()).join("input");
        std::fs::create_dir_all(&input_dir).unwrap();
        let input_path = input_dir.join("cloud.ply");
        std::fs::write(&input_path, PLY_FIXTURE).unwrap();

        let asset = Asset {
            id,
            name: "cloud.ply".into(),
            asset_type: AssetType::PointCloud,
            status: AssetStatus::Uploading,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: PLY_FIXTURE.len() as u64,
            description: String::new(),
            tags: vec![],
            owner_id: Some("user-a".into()),
        };
        state.db.create_asset(&asset).await.unwrap();

        let job = state
            .job_queue
            .submit(
                id,
                input_path.to_string_lossy().into_owned(),
                tiletopia_server::db::ModelPlacement::default(),
            )
            .await
            .unwrap();
        assert_eq!(job.status, JobStatus::Queued);

        // queued state is in the database before any worker exists, so a client
        // can poll it straight after submit
        let queued = state.db.get_job(job.id).await.unwrap().unwrap();
        assert_eq!(queued.status, JobStatus::Queued);
        assert!(queued.progress.abs() < 1e-9);
        assert!(queued.started_at.is_none());
        assert_eq!(
            state.db.next_queued_job().await.unwrap().unwrap().id,
            job.id
        );

        let worker = Arc::clone(&state.job_queue).start().await;

        // spun rather than slept between reads, so the asset is read in the
        // instant the job settles. A worker that announced Done before the
        // asset status write landed would be caught here rather than whenever
        // the machine happened to be slow enough.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut settled = None;
        while std::time::Instant::now() < deadline {
            let current = state.db.get_job(job.id).await.unwrap().unwrap();
            if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
                settled = Some(current);
                break;
            }
            tokio::task::yield_now().await;
        }
        let tiled = state.db.get_asset(id).await.unwrap().unwrap();
        worker.abort();

        let settled = settled.expect("job never left the queue");
        assert_eq!(
            settled.status,
            JobStatus::Done,
            "error: {:?}",
            settled.error
        );
        assert!((settled.progress - 1.0).abs() < 1e-9);
        assert!(
            settled.started_at.is_some(),
            "running transition not stored"
        );
        assert!(settled.completed_at.is_some());
        assert!(settled.tiles_written > 0);
        assert!(settled.error.is_none());

        // the worker owns the asset status too, and a client that sees Done
        // stops polling, so the asset has to already say Ready by then
        assert!(
            matches!(tiled.status, AssetStatus::Ready),
            "job was Done while the asset still said {:?}",
            tiled.status
        );
        assert_eq!(tiled.tile_count, settled.tiles_written);

        // and no queued work is left behind
        assert!(state.db.next_queued_job().await.unwrap().is_none());
    }

    // -- the external tiler --

    const MAGO_JAR_VAR: &str = "TILETOPIA_MAGO_JAR";

    const CUBE_OBJ: &str = "\
v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
f 1 2 3 4\nf 5 6 7 8\nf 1 2 6 5\nf 2 3 7 6\nf 3 4 8 7\nf 4 1 5 8\n";

    const POINT_GEOJSON: &str = r#"{"type":"FeatureCollection","features":[
{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[10,20]}}]}"#;

    /// Multipart upload of `contents` under `filename`, with any extra text
    /// fields the upload takes beside the file.
    async fn upload_with_fields(
        state: &Arc<AppState>,
        token: &str,
        filename: &str,
        contents: &str,
        fields: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let boundary = "tiletopiatestboundary";
        let mut body = String::new();
        for (name, value) in fields {
            body.push_str(&format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            ));
        }
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\n\r\n{contents}\r\n--{boundary}--\r\n"
        ));

        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/assets")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header("authorization", format!("Bearer {token}"));
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    async fn get_with_token(
        state: &Arc<AppState>,
        uri: &str,
        token: &str,
    ) -> (StatusCode, Vec<u8>) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// Drive the queue until the asset's only job settles, then report it.
    async fn settle_only_job(
        state: &Arc<AppState>,
        asset_id: uuid::Uuid,
    ) -> tiletopia_server::db::JobRecord {
        use tiletopia_server::db::JobStatus;

        let worker = Arc::clone(&state.job_queue).start().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut settled = None;
        while std::time::Instant::now() < deadline {
            let jobs = state.db.list_jobs_for_asset(asset_id).await.unwrap();
            let current = jobs.into_iter().next().expect("a job for the asset");
            if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
                settled = Some(current);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        worker.abort();
        settled.expect("job never settled")
    }

    /// Every `uri` anywhere in a tileset tree, root first.
    fn content_uris(node: &serde_json::Value, found: &mut Vec<String>) {
        if let Some(uri) = node.pointer("/content/uri").and_then(|u| u.as_str()) {
            found.push(uri.to_string());
        }
        if let Some(children) = node["children"].as_array() {
            for child in children {
                content_uris(child, found);
            }
        }
    }

    /// An OBJ upload comes back as a 3D Tiles tileset whose content the `data`
    /// route serves. Needs the jar, so it is skipped when the variable is unset.
    #[tokio::test]
    async fn obj_upload_is_tiled_by_the_external_tiler() {
        use tiletopia_server::db::JobStatus;

        let Ok(jar) = std::env::var(MAGO_JAR_VAR) else {
            eprintln!("skipped: {MAGO_JAR_VAR} is not set");
            return;
        };

        let state = state_with_external_tiler(Some(jar.into())).await;
        let token = bootstrap_editor(&state, "mago-obj-editor@example.com").await;

        let (status, body) = upload_with_fields(
            &state,
            &token,
            "cube.obj",
            CUBE_OBJ,
            &[("longitude", "10"), ("latitude", "20"), ("crs", "3857")],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(
            settled.status,
            JobStatus::Done,
            "error: {:?}",
            settled.error
        );
        assert!(settled.tiles_written > 0, "no tile files were counted");

        let (status, body) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tileset.json"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tileset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(tileset["asset"]["version"], "1.1");

        let mut uris = Vec::new();
        content_uris(&tileset["root"], &mut uris);
        let uri = uris
            .iter()
            .find(|u| u.starts_with("data/"))
            .unwrap_or_else(|| panic!("no data/ content uri in {uris:?}"));

        let (status, tile) =
            get_with_token(&state, &format!("/api/v1/assets/{asset_id}/{uri}"), &token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&tile[..4], b"glTF", "tile content is not a glb");
    }

    /// A vector upload has no native tiler behind it, so with no jar configured
    /// the job fails naming the variable rather than leaving the asset at
    /// Uploading forever.
    #[tokio::test]
    async fn a_vector_upload_without_the_jar_fails_naming_the_variable() {
        use tiletopia_server::AssetStatus;
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "mago-nojar-editor@example.com").await;

        let (status, body) = upload_with_fields(
            &state,
            &token,
            "places.geojson",
            POINT_GEOJSON,
            &[("longitude", "10"), ("latitude", "20"), ("crs", "3857")],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(settled.status, JobStatus::Failed);
        let error = settled.error.expect("a failed job says why");
        assert!(error.contains(MAGO_JAR_VAR), "{error}");

        let stored = state.db.get_asset(asset_id).await.unwrap().unwrap();
        assert!(
            matches!(stored.status, AssetStatus::Error),
            "asset says {:?}",
            stored.status
        );
    }

    // -- the native mesh tiler --

    /// One extruded wall, no site coordinates.
    const WALL_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('extrusion.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#40=IFCWALL('0002',$,'TestWall',$,$,#12,#31,$);
ENDSEC;
END-ISO-10303-21;
";

    /// The same wall under a site at 51°30'N, 0°7'40\"W, 12.5 m up.
    const WALL_ON_A_SITE_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('site.ifc','2024-01-01',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('0001',$,'TestProject',$,$,$,$,$,$);
#2=IFCSITE('0003',$,'TestSite',$,$,$,$,$,.ELEMENT.,(51,30,0),(-0,-7,-40),12.5,$,$);
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#40=IFCWALL('0002',$,'TestWall',$,$,#12,#31,$);
ENDSEC;
END-ISO-10303-21;
";

    /// How far the tileset's root transform may sit from the expected origin.
    const ORIGIN_TOLERANCE_METRES: f64 = 1.0;

    /// The root tile's translation, read out of a served tileset.json.
    fn root_translation(tileset: &serde_json::Value) -> [f64; 3] {
        let transform = tileset["root"]["transform"]
            .as_array()
            .expect("the root tile carries a transform");
        assert_eq!(transform.len(), 16, "{transform:?}");
        [
            transform[12].as_f64().unwrap(),
            transform[13].as_f64().unwrap(),
            transform[14].as_f64().unwrap(),
        ]
    }

    fn assert_within_a_metre(written: [f64; 3], expected: [f64; 3]) {
        for axis in 0..3 {
            assert!(
                (written[axis] - expected[axis]).abs() < ORIGIN_TOLERANCE_METRES,
                "axis {axis}: {written:?} is not {expected:?}"
            );
        }
    }

    /// An IFC upload is read and tiled by this repository, with no jar in
    /// sight, and lands where the upload's longitude and latitude say.
    #[tokio::test]
    async fn ifc_upload_is_tiled_natively_and_placed_from_the_upload() {
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "native-ifc-editor@example.com").await;

        let (status, body) = upload_with_fields(
            &state,
            &token,
            "wall.ifc",
            WALL_IFC,
            &[("longitude", "10"), ("latitude", "20")],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(
            settled.status,
            JobStatus::Done,
            "error: {:?}",
            settled.error
        );
        assert!(settled.tiles_written > 0, "no tiles were counted");

        let (status, body) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tileset.json"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tileset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(tileset["asset"]["version"], "1.1");

        assert_within_a_metre(
            root_translation(&tileset),
            tiletopia_core::spatial::geodetic_to_ecef(20f64.to_radians(), 10f64.to_radians(), 0.0),
        );

        let (status, tile) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tiles/root.glb"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&tile[..4], b"glTF", "tile content is not a glb");
    }

    /// A model at the ECEF origin is not a success, so an IFC with neither an
    /// upload placement nor site coordinates fails saying what to send.
    #[tokio::test]
    async fn an_ifc_with_no_coordinates_anywhere_fails() {
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "native-ifc-nocoords@example.com").await;

        let (status, body) = upload_with_fields(&state, &token, "wall.ifc", WALL_IFC, &[]).await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(settled.status, JobStatus::Failed);
        let error = settled.error.expect("a failed job says why");
        assert!(error.contains("no site coordinates"), "{error}");
        assert!(error.contains("longitude and latitude"), "{error}");
    }

    /// With nothing on the upload the IfcSite's reference coordinates place it.
    #[tokio::test]
    async fn an_ifc_without_an_upload_placement_falls_back_to_its_site() {
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "native-ifc-site@example.com").await;

        let (status, body) =
            upload_with_fields(&state, &token, "site.ifc", WALL_ON_A_SITE_IFC, &[]).await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(
            settled.status,
            JobStatus::Done,
            "error: {:?}",
            settled.error
        );

        let (status, body) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tileset.json"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tileset: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let longitude: f64 = -(7.0 / 60.0 + 40.0 / 3600.0);
        assert_within_a_metre(
            root_translation(&tileset),
            tiletopia_core::spatial::geodetic_to_ecef(
                51.5f64.to_radians(),
                longitude.to_radians(),
                12.5,
            ),
        );
    }

    /// With no jar configured an OBJ falls back to the native mesh tiler and
    /// lands where the upload's longitude and latitude say.
    #[tokio::test]
    async fn obj_upload_without_the_jar_is_tiled_natively() {
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "native-obj-editor@example.com").await;

        let (status, body) = upload_with_fields(
            &state,
            &token,
            "cube.obj",
            CUBE_OBJ,
            &[("longitude", "10"), ("latitude", "20")],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(
            settled.status,
            JobStatus::Done,
            "error: {:?}",
            settled.error
        );
        assert!(settled.tiles_written > 0, "no tiles were counted");

        let (status, body) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tileset.json"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tileset: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_within_a_metre(
            root_translation(&tileset),
            tiletopia_core::spatial::geodetic_to_ecef(20f64.to_radians(), 10f64.to_radians(), 0.0),
        );

        let (status, tile) = get_with_token(
            &state,
            &format!("/api/v1/assets/{asset_id}/tiles/root.glb"),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&tile[..4], b"glTF", "tile content is not a glb");
    }

    /// An OBJ carries no coordinates of its own, so with neither an upload
    /// placement nor a jar the job fails naming both ways out.
    #[tokio::test]
    async fn an_obj_with_no_placement_and_no_jar_fails_naming_both_options() {
        use tiletopia_server::db::JobStatus;

        let state = state_with_external_tiler(None).await;
        let token = bootstrap_editor(&state, "native-obj-nocoords@example.com").await;

        let (status, body) = upload_with_fields(&state, &token, "cube.obj", CUBE_OBJ, &[]).await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(settled.status, JobStatus::Failed);
        let error = settled.error.expect("a failed job says why");
        assert!(error.contains("longitude and latitude"), "{error}");
        assert!(error.contains(MAGO_JAR_VAR), "{error}");
    }

    /// DAE has no reader here and no mago input type, so it still fails naming
    /// the format. The check runs before the jar lookup.
    #[tokio::test]
    async fn a_dae_upload_fails_naming_the_format() {
        use tiletopia_server::db::JobStatus;

        let jar = std::env::var(MAGO_JAR_VAR)
            .ok()
            .map(std::path::PathBuf::from);
        let state = state_with_external_tiler(jar).await;
        let token = bootstrap_editor(&state, "dae-editor@example.com").await;

        let (status, body) =
            upload_with_fields(&state, &token, "model.dae", "not really collada", &[]).await;
        assert_eq!(status, StatusCode::CREATED);
        let asset: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let asset_id = uuid::Uuid::parse_str(asset["id"].as_str().unwrap()).unwrap();

        let settled = settle_only_job(&state, asset_id).await;
        assert_eq!(settled.status, JobStatus::Failed);
        let error = settled.error.expect("a failed job says why");
        assert!(error.contains("dae"), "{error}");
        assert!(error.contains("native tiler"), "{error}");
        assert!(error.contains("external one"), "{error}");
    }

    /// The catch-all that used to call every unknown extension a point cloud is
    /// gone, so an extension with no tiler behind it is refused at the door.
    #[tokio::test]
    async fn an_unknown_extension_is_refused_with_the_accepted_list() {
        let state = test_state().await;
        let token = bootstrap_editor(&state, "unknown-ext-editor@example.com").await;

        let (status, body) = upload_with_fields(&state, &token, "notes.txt", "hello", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = String::from_utf8_lossy(&body);
        assert!(message.contains("notes.txt"), "{message}");
        assert!(message.contains("glb"), "{message}");
    }

    /// mago takes longitude and latitude together or not at all, so half a
    /// placement is refused before anything is stored.
    #[tokio::test]
    async fn longitude_without_latitude_is_refused() {
        let state = test_state().await;
        let token = bootstrap_editor(&state, "half-placement-editor@example.com").await;

        let (status, body) =
            upload_with_fields(&state, &token, "cube.obj", CUBE_OBJ, &[("longitude", "10")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = String::from_utf8_lossy(&body);
        assert!(message.contains("latitude"), "{message}");
        assert_eq!(state.db.list_assets().await.unwrap().len(), 0);
    }

    // -- asset exports --

    const EXPORT_ASSET: &str = "11111111-1111-1111-1111-111111111111";

    fn geojson_export_body() -> serde_json::Value {
        serde_json::json!({ "asset_id": EXPORT_ASSET, "format": "geojson" })
    }

    async fn post_export(
        state: &Arc<AppState>,
        token: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/v1/exports")
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = router(Arc::clone(state))
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    /// A tokened GET, returning the content-disposition so download tests can
    /// assert the filename the browser will save under.
    async fn get_export(
        state: &Arc<AppState>,
        uri: &str,
        token: &str,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let resp = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let disposition = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, disposition, bytes)
    }

    /// The whole loop the viewer drives: start a job, poll it to Ready, then
    /// pull the encoded file back down.
    #[tokio::test]
    async fn export_create_poll_and_download() {
        let state = test_state().await;
        let token = bootstrap_editor(&state, "export-editor@example.com").await;

        let (status, job) = post_export(&state, Some(&token), geojson_export_body()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(job["status"], "Queued");
        assert_eq!(job["format"], "GeoJson");
        let id = job["id"].as_str().expect("job id").to_string();

        let mut settled = None;
        for _ in 0..200 {
            let (status, _, bytes) =
                get_export(&state, &format!("/api/v1/exports/{id}"), &token).await;
            assert_eq!(status, StatusCode::OK);
            let current: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if current["status"] != "Queued" && current["status"] != "Processing" {
                settled = Some(current);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let settled = settled.expect("export never settled");
        assert_eq!(settled["status"], "Ready", "job: {settled}");
        assert_eq!(
            settled["download_url"],
            format!("/api/v1/exports/download/{id}")
        );
        assert!(settled["file_size_bytes"].as_u64().unwrap() > 0);

        let (status, disposition, bytes) =
            get_export(&state, &format!("/api/v1/exports/download/{id}"), &token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            disposition.as_deref(),
            Some("attachment; filename=\"export.geojson\"")
        );
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["type"], "FeatureCollection");
        assert_eq!(doc["metadata"]["asset_id"], EXPORT_ASSET);
    }

    /// Downloading a job that has not encoded anything yet is a 404, never an
    /// empty file the caller would mistake for the export.
    #[tokio::test]
    async fn export_download_before_ready_is_not_found() {
        let state = test_state().await;
        let (token, uid) = bootstrap_editor_with_id(&state, "export-pending@example.com").await;

        let job = state
            .export_engine
            .create_export(
                uuid::Uuid::parse_str(&uid).unwrap(),
                uuid::Uuid::new_v4(),
                tiletopia_server::export::ExportFormat::GeoJson,
                None,
            )
            .await;

        let (status, _, _) = get_export(
            &state,
            &format!("/api/v1/exports/download/{}", job.id),
            &token,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // the job itself stays readable while it waits
        let (status, _, _) =
            get_export(&state, &format!("/api/v1/exports/{}", job.id), &token).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn export_create_anonymous_rejected() {
        let state = test_state().await;
        let (status, _) = post_export(&state, None, geojson_export_body()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn export_create_viewer_forbidden() {
        let state = test_state().await;
        let (token, _) = signup(&state, "export-viewer@example.com").await;
        let (status, _) = post_export(&state, Some(&token), geojson_export_body()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn export_rejects_an_unadvertised_format() {
        let state = test_state().await;
        let token = bootstrap_editor(&state, "export-format@example.com").await;
        let (status, _) = post_export(
            &state,
            Some(&token),
            serde_json::json!({ "asset_id": EXPORT_ASSET, "format": "dwg" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// Another tenant's job id answers 404, so ids leak nothing.
    #[tokio::test]
    async fn export_of_another_tenant_is_invisible() {
        let state = test_state().await;
        let owner = bootstrap_editor(&state, "export-owner@example.com").await;
        let other = bootstrap_editor(&state, "export-other@example.com").await;

        let (status, job) = post_export(&state, Some(&owner), geojson_export_body()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = job["id"].as_str().unwrap().to_string();

        let (status, _, _) = get_export(&state, &format!("/api/v1/exports/{id}"), &other).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) =
            get_export(&state, &format!("/api/v1/exports/download/{id}"), &other).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn export_listing_is_tenant_scoped() {
        let state = test_state().await;
        let owner = bootstrap_editor(&state, "list-owner@example.com").await;
        let other = bootstrap_editor(&state, "list-other@example.com").await;

        let (status, job) = post_export(&state, Some(&owner), geojson_export_body()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = job["id"].as_str().unwrap().to_string();

        let (status, _, body) = get_export(&state, "/api/v1/exports", &owner).await;
        assert_eq!(status, StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let owner_ids: Vec<&str> = listed["exports"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap())
            .collect();
        assert!(owner_ids.contains(&id.as_str()));

        let (status, _, body) = get_export(&state, "/api/v1/exports", &other).await;
        assert_eq!(status, StatusCode::OK);
        let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(listed["exports"].as_array().unwrap().len(), 0);
    }
}
