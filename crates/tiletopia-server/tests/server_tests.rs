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
        assert_eq!(v["tiles"][0], "/api/v1/terrain/{z}/{x}/{y}");
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

    #[tokio::test]
    async fn realtime_anonymous_handshake_never_upgrades() {
        let state = test_state().await;
        let addr = serve(&state).await;

        let attempt =
            tokio_tungstenite::connect_async(format!("ws://{addr}/api/v1/realtime/room-1")).await;
        match attempt {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status().as_u16(), 401);
            }
            Err(e) => panic!("expected an http rejection, got {e:?}"),
            Ok(_) => panic!("anonymous handshake must not upgrade"),
        }
    }

    #[tokio::test]
    async fn realtime_join_round_trip_with_viewer_token() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let state = test_state().await;
        // a plain signup is a viewer, which is enough to join a room
        let (token, _uid) = signup(&state, "collab@example.com").await;
        let addr = serve(&state).await;

        // a browser cannot set headers on a ws handshake, so the token rides the
        // query string
        let (mut ws, resp) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/api/v1/realtime/room-1?token={token}"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 101);

        ws.send(Message::Text(
            serde_json::json!({
                "type": "Join",
                "user_id": "u1",
                "asset_id": "room-1",
                "user_name": "Ann",
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

        // the server answers a join by broadcasting the room presence list
        let msg = ws.next().await.unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["type"], "Presence");
        assert_eq!(v["users"][0]["user_id"], "u1");
        assert_eq!(v["users"][0]["user_name"], "Ann");
    }

    #[tokio::test]
    async fn realtime_bad_token_rejected() {
        let state = test_state().await;
        let addr = serve(&state).await;

        let attempt = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/api/v1/realtime/room-1?token=not-a-jwt"
        ))
        .await;
        match attempt {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                assert_eq!(resp.status().as_u16(), 401);
            }
            Err(e) => panic!("expected an http rejection, got {e:?}"),
            Ok(_) => panic!("a forged token must not upgrade"),
        }
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
}
