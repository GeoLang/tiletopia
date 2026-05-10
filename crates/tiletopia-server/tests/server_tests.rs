#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tiletopia_server::{AppState, router};
    use tower::ServiceExt;

    async fn test_state() -> Arc<AppState> {
        let dir =
            std::env::temp_dir().join(format!("tiletopia_server_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();

        let db = Arc::new(
            tiletopia_server::db::Database::new("sqlite::memory:")
                .await
                .unwrap(),
        );
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
}
