#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tiletopia_server::{AppState, router};
    use tower::ServiceExt;

    async fn test_state() -> Arc<AppState> {
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
            elevation_store: tiletopia_server::elevation::DemStore::new(),
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
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/assets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
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
}
