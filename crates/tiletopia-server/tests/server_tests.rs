#[cfg(test)]
mod tests {
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);

        let dir = std::env::temp_dir().join(format!(
            "tiletopia_server_test_{}_{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).ok();

        // named shared-cache memory db so all pooled connections see one database,
        // unique per test so cases stay isolated
        let db_url = format!(
            "sqlite:file:tiletopia_test_{}_{}?mode=memory&cache=shared",
            std::process::id(),
            n
        );
        let db = Arc::new(tiletopia_server::db::Database::new(&db_url).await.unwrap());
        db.migrate().await.unwrap();

        let store: Arc<dyn tiletopia_store::TileStore> =
            Arc::new(tiletopia_store::LocalStore::new(dir.clone()));

        let job_queue = Arc::new(tiletopia_server::job_queue::JobQueue::new(
            Arc::clone(&db),
            dir.clone(),
            Arc::clone(&store),
        ));

        Arc::new(AppState {
            db,
            store,
            data_dir: dir,
            job_queue,
            realtime: tiletopia_server::realtime::RealtimeState::new(),
            demo: tiletopia_server::demo::DemoState::new(),
            catalog: tiletopia_server::catalog::OpenDataCatalog::new(),
            started_at: std::time::Instant::now(),
            api_key_store: tiletopia_server::api_keys::ApiKeyStore::new(),
            metering_store: tiletopia_server::metering::MeteringStore::new(),
            webhook_engine: tiletopia_server::webhooks::WebhookEngine::new(),
            workspace_store: tiletopia_server::workspaces::WorkspaceStore::new(),
            export_engine: tiletopia_server::export::ExportEngine::new(),
            scheduler: tiletopia_server::scheduler::Scheduler::new(),
            plugin_registry: tiletopia_server::plugins::PluginRegistry::new(),
            photogrammetry_engine: tiletopia_server::photogrammetry::PhotogrammetryEngine::new(),
            classification_engine: tiletopia_server::classification::ClassificationEngine::new(),
            model_registry: tiletopia_server::model_registry::ModelRegistry::new(),
            collaboration_engine: tiletopia_server::collaboration::CollaborationEngine::new(),
            versioning_engine: tiletopia_server::versioning::VersioningEngine::new(),
            bim4d_engine: tiletopia_server::bim4d::Bim4DEngine::new(),
            cog_engine: tiletopia_server::cog::CogEngine::new(),
            routing_engine: tiletopia_server::routing::RoutingEngine::new(),
            map_tile_engine: tiletopia_server::map_tiles::MapTileEngine::new(),
            feature_service_engine: tiletopia_server::feature_service::FeatureServiceEngine::new(),
            issue_tracker: tiletopia_server::issue_tracking::IssueTracker::new(),
            elevation_store: Arc::new(tiletopia_server::elevation::DemStore::new()),
            analysis_engines,
            entity_link_store: tiletopia_server::entity_linking::EntityLinkStore::new(),
        })
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
    // only enforces when TILETOPIA_JWT_SECRET is set, which a test cannot do
    // without racing every other test in the process). these two cover that the
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

    #[tokio::test]
    async fn terrain_rgb_tile_anonymous_ok() {
        let state = test_state().await;

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

    #[tokio::test]
    async fn terrain_tile_anonymous_ok() {
        use tiletopia_terrain::global_dem::{TerrainTileCoord, required_dem_tiles};

        let state = test_state().await;

        // seed a local DEM so the handler never reaches for SRTM over the network
        let coord = TerrainTileCoord {
            zoom: 12,
            x: 2200,
            y: 1400,
        };
        let dem_dir = state.data_dir.join("dem");
        std::fs::create_dir_all(&dem_dir).unwrap();
        let elevations: Vec<u8> = (0..16u32 * 16)
            .flat_map(|i| (i as f32).to_le_bytes())
            .collect();
        for (lat, lon) in required_dem_tiles(coord.bounds()) {
            std::fs::write(dem_dir.join(format!("{lat}_{lon}.bin")), &elevations).unwrap();
        }

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

    /// With every render slot taken the route sheds load instead of queueing,
    /// which is what keeps an anonymous caller from pinning every core.
    #[tokio::test]
    async fn analysis_xyz_refuses_a_tile_when_renders_are_saturated() {
        let state = state_with_engines(
            tiletopia_server::analysis_tiles::AnalysisEngines::with_render_slots(0),
        )
        .await;
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

    // multipart upload of a tiny .glb, which detects as Model so no tiling job
    // is queued by the upload itself.
    async fn upload_glb(
        state: &Arc<AppState>,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let boundary = "tiletopiatestboundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"t.glb\"\r\n\r\nglTF-bytes\r\n--{boundary}--\r\n"
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
            .submit(id, input_path.to_string_lossy().into_owned())
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

        let mut settled = None;
        for _ in 0..200 {
            let current = state.db.get_job(job.id).await.unwrap().unwrap();
            if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
                settled = Some(current);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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

        // the worker owns the asset status too
        let tiled = state.db.get_asset(id).await.unwrap().unwrap();
        assert!(matches!(tiled.status, AssetStatus::Ready));
        assert_eq!(tiled.tile_count, settled.tiles_written);

        // and no queued work is left behind
        assert!(state.db.next_queued_job().await.unwrap().is_none());
    }
}
