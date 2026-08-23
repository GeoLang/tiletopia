use std::sync::Arc;
use tiletopia_server::AppState;

/// An `AppState` backed by a per-test temp directory and a private in-memory
/// database, so cases running in parallel cannot see each other's rows.
pub async fn build_state(
    analysis_engines: tiletopia_server::analysis_tiles::AnalysisEngines,
    external_tiler_jar: Option<std::path::PathBuf>,
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
        external_tiler_jar,
    ));

    Arc::new(AppState {
        db,
        store,
        data_dir: dir,
        srtm_base_url: tiletopia_terrain::dem_cache::DEFAULT_SRTM_BASE_URL.to_string(),
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
        #[cfg(feature = "martin")]
        martin_backend: tiletopia_server::map_tiles::martin_backend::MartinTileBackend::new(),
    })
}
