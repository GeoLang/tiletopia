use std::sync::Arc;
use tiletopia_server::AppState;

/// Delay before a failed webhook delivery's second attempt in the test state.
pub const WEBHOOK_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(20);

/// The one-degree cell the terrain and analysis cases query, off Monaco.
const FIXTURE_TILE: (i32, i32) = (43, 7);

/// Samples per axis in the staged tile. 121 across a degree is about 900 m,
/// coarse for terrain but enough structure for the analysis cases to read.
const FIXTURE_SAMPLES: usize = 121;

/// Elevation of the staged fixture tile at a point inside it: ground climbing a
/// kilometre from south to north, with four north-south ridges 400 m tall
/// across it. Steep enough for slope, hillshade and a viewshed to have
/// something to read, and monotone northwards so a profile up a meridian only
/// climbs.
pub fn fixture_elevation_m(lat: f64, lon: f64) -> f64 {
    let (tile_lat, tile_lon) = FIXTURE_TILE;
    let north = lat - tile_lat as f64;
    let east = lon - tile_lon as f64;
    1000.0 * north + 400.0 * (8.0 * std::f64::consts::PI * east).sin()
}

/// Stage one degree of DEM the way an operator would: south-up f32 samples in
/// `<data-dir>/dem/{lat}_{lon}.bin`, which is where every elevation query looks
/// first.
///
/// Staging it here keeps the cases off the network: with nothing on disk the
/// elevation lookups would reach for the SRTM bucket.
fn stage_fixture_dem(data_dir: &std::path::Path) {
    let (lat, lon) = FIXTURE_TILE;
    let dir = data_dir.join("dem");
    std::fs::create_dir_all(&dir).ok();

    let last = (FIXTURE_SAMPLES - 1) as f64;
    let mut bytes = Vec::with_capacity(FIXTURE_SAMPLES * FIXTURE_SAMPLES * 4);
    for row in 0..FIXTURE_SAMPLES {
        for col in 0..FIXTURE_SAMPLES {
            let elevation = fixture_elevation_m(
                lat as f64 + row as f64 / last,
                lon as f64 + col as f64 / last,
            );
            bytes.extend_from_slice(&(elevation as f32).to_le_bytes());
        }
    }
    std::fs::write(dir.join(format!("{lat}_{lon}.bin")), bytes).unwrap();
}

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
    stage_fixture_dem(&dir);

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

    // per-test, so a case that builds an archive cannot see another's, and so
    // TILETOPIA_TILESET_DIR is left out of it
    #[cfg(feature = "martin")]
    let tileset_dir = dir.join("tilesets");
    #[cfg(feature = "martin")]
    std::fs::create_dir_all(&tileset_dir).ok();

    // a short retry base, so a case that exhausts the delivery retries takes
    // milliseconds rather than minutes
    let webhooks = Arc::new(tiletopia_server::webhooks::WebhookQueue::with_retry_base(
        Arc::clone(&db),
        WEBHOOK_RETRY_BASE,
    ));

    let job_queue = Arc::new(tiletopia_server::job_queue::JobQueue::new(
        Arc::clone(&db),
        dir.clone(),
        Arc::clone(&store),
        external_tiler_jar,
        Arc::clone(&webhooks),
    ));

    Arc::new(AppState {
        db,
        store,
        data_dir: dir,
        // empty turns the SRTM download fallback off: a case reads the staged
        // fixture or gets an explicit gap, never the network
        srtm_base_url: String::new(),
        job_queue,
        realtime: tiletopia_server::realtime::RealtimeState::new(),
        demo: tiletopia_server::demo::DemoState::new(),
        catalog: tiletopia_server::catalog::OpenDataCatalog::new(),
        started_at: std::time::Instant::now(),
        api_key_rate_limiter: tiletopia_server::api_keys::RateLimiter::new(),
        metering_store: tiletopia_server::metering::MeteringStore::new(),
        webhooks,
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
        #[cfg(feature = "martin")]
        tileset_dir: tileset_dir.clone(),
        #[cfg(feature = "martin")]
        current_tileset_build: tiletopia_server::tilesets::CurrentBuild::new(),
    })
}
