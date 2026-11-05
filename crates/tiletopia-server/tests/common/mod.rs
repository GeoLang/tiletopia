// every test binary compiles this module on its own, and none of them use all of it
#![allow(dead_code)]

use std::sync::Arc;
use tiletopia_server::AppState;

const JWT_SECRET_ENV: &str = "TILETOPIA_JWT_SECRET";
const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";
pub const TEST_ION_BASE_URL: &str = "http://tiletopia.test:3000";

/// Put the auth middleware into its enforcing state, once for the binary.
pub fn signing_secret() -> &'static str {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // safe: the only writer in this test binary, before any request runs
        unsafe {
            std::env::set_var(JWT_SECRET_ENV, TEST_SECRET);
            std::env::remove_var("TILETOPIA_AUTH_DISABLED");
        }
    });
    TEST_SECRET
}

pub fn token(subject: &str, role: &str) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let claims = serde_json::json!({
        "sub": subject,
        "exp": chrono::Utc::now().timestamp() + 300,
        "role": role,
    });
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(signing_secret().as_bytes()),
    )
    .unwrap()
}

pub type ReceivedJobEvents = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

pub async fn subscribe_to_job_events(state: &Arc<AppState>) -> ReceivedJobEvents {
    use tiletopia_server::webhooks::{WebhookEvent, WebhookSubscription, generate_secret};

    let subscriber = stored_admin(state).await;

    let received: ReceivedJobEvents = Arc::default();
    let recorder = Arc::clone(&received);
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |body: axum::body::Bytes| {
            let recorder = Arc::clone(&recorder);
            async move {
                let event = serde_json::from_slice(&body).unwrap();
                recorder.lock().unwrap().push(event);
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    state
        .db
        .create_webhook_subscription(&WebhookSubscription {
            id: uuid::Uuid::new_v4(),
            url: format!("http://{address}/hook"),
            events: vec![WebhookEvent::JobCompleted, WebhookEvent::JobFailed],
            secret: generate_secret(),
            created_by: subscriber.to_string(),
            active: true,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    Arc::clone(&state.webhooks).start();
    received
}

pub async fn first_job_event(received: &ReceivedJobEvents) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(event) = received.lock().unwrap().first() {
            return event.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no job event was delivered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub async fn stored_admin(state: &Arc<AppState>) -> uuid::Uuid {
    use tiletopia_server::users::{User, UserRole};
    let id = uuid::Uuid::new_v4();
    let user = User {
        id,
        email: format!("{id}@example.com"),
        name: "stored admin".to_string(),
        role: UserRole::Admin,
        org_id: None,
        created_at: chrono::Utc::now(),
        last_login: None,
    };
    state.db.create_user(&user, "not-a-hash").await.unwrap();
    id
}

pub async fn stored_admin_token(state: &Arc<AppState>) -> String {
    token(&stored_admin(state).await.to_string(), "admin")
}

pub const PLY_FIXTURE: &str = "\
ply\nformat ascii 1.0\nelement vertex 4\nproperty float x\nproperty float y\n\
property float z\nproperty uchar red\nproperty uchar green\nproperty uchar blue\n\
end_header\n0.0 0.0 0.0 255 0 0\n1.0 0.0 0.0 0 255 0\n0.0 1.0 0.0 0 0 255\n\
1.0 1.0 1.0 255 255 255\n";

pub fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        writer.start_file(*name, options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

pub fn texture_png() -> Vec<u8> {
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(8, 8, image::Rgb([90, 140, 60]))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    png.into_inner()
}

pub async fn test_state() -> Arc<AppState> {
    signing_secret();
    build_state(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
    )
    .await
}

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
    build_state_with_account_limits(
        analysis_engines,
        external_tiler_jar,
        None,
        tiletopia_server::users::AccountLimits::default(),
        unconfigured_global_terrain(),
        None,
    )
    .await
}

pub async fn state_with_node_odm(
    node_odm: tiletopia_server::photogrammetry::NodeOdm,
) -> Arc<AppState> {
    signing_secret();
    build_state_with_account_limits(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
        Some(node_odm),
        tiletopia_server::users::AccountLimits::default(),
        unconfigured_global_terrain(),
        None,
    )
    .await
}

pub async fn test_state_with_account_limits(
    account_limits: tiletopia_server::users::AccountLimits,
) -> Arc<AppState> {
    build_state_with_account_limits(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
        None,
        account_limits,
        unconfigured_global_terrain(),
        None,
    )
    .await
}

pub async fn test_state_with_global_terrain(
    global_terrain: tiletopia_server::global_terrain::GlobalTerrain,
) -> Arc<AppState> {
    signing_secret();
    build_state_with_account_limits(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
        None,
        tiletopia_server::users::AccountLimits::default(),
        global_terrain,
        None,
    )
    .await
}

pub async fn test_state_with_ids_check(
    sidecar: tiletopia_server::ids_check::IdsCheckSidecar,
) -> Arc<AppState> {
    signing_secret();
    build_state_with_account_limits(
        tiletopia_server::analysis_tiles::AnalysisEngines::new(),
        None,
        None,
        tiletopia_server::users::AccountLimits::default(),
        unconfigured_global_terrain(),
        Some(sidecar),
    )
    .await
}

fn unconfigured_global_terrain() -> tiletopia_server::global_terrain::GlobalTerrain {
    tiletopia_server::global_terrain::GlobalTerrain::new(
        tiletopia_server::global_terrain::GlobalTerrainSettings {
            dem_url: None,
            geoid_url: None,
            cache_dir: None,
            cache_bytes: 1 << 30,
            ion_asset_id: None,
        },
    )
}

async fn build_state_with_account_limits(
    analysis_engines: tiletopia_server::analysis_tiles::AnalysisEngines,
    external_tiler_jar: Option<std::path::PathBuf>,
    node_odm: Option<tiletopia_server::photogrammetry::NodeOdm>,
    account_limits: tiletopia_server::users::AccountLimits,
    global_terrain: tiletopia_server::global_terrain::GlobalTerrain,
    ids_check: Option<tiletopia_server::ids_check::IdsCheckSidecar>,
) -> Arc<AppState> {
    let dir = tempfile::Builder::new()
        .prefix("tiletopia_server_test_")
        .tempdir()
        .unwrap()
        .keep();
    stage_fixture_dem(&dir);

    // sqlite shared-cache memory databases answer concurrent writers with table locks
    let db_url = format!("sqlite://{}?mode=rwc", dir.join("tiletopia.db").display());
    let db = Arc::new(tiletopia_server::db::Database::new(&db_url).await.unwrap());
    db.migrate().await.unwrap();
    global_terrain.reserve_ion_asset_id(&db).await.unwrap();

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
        platform_webhook::PrivateHosts::AllowedForTestReceiver,
    ));

    let elevation_store = Arc::new(tiletopia_server::elevation::DemStore::new());

    let job_queue = Arc::new(tiletopia_server::job_queue::JobQueue::new(
        Arc::clone(&db),
        dir.clone(),
        Arc::clone(&store),
        external_tiler_jar,
        node_odm,
        Arc::clone(&webhooks),
        Arc::clone(&elevation_store),
    ));

    let export_engine = Arc::new(tiletopia_server::export::ExportEngine::new());

    let scheduler = Arc::new(tiletopia_server::scheduler::Scheduler::new(
        Arc::clone(&db),
        dir.clone(),
        Arc::clone(&job_queue),
        Arc::clone(&export_engine),
    ));

    // no sweep task: a case that wants one runs it itself, so nothing deletes
    // rows under a case that is still reading them
    let audit_log = Arc::new(tiletopia_server::audit::AuditLog::new(Arc::clone(&db)));

    Arc::new(AppState {
        db,
        store,
        data_dir: dir,
        // empty turns the SRTM download fallback off: a case reads the staged
        // fixture or gets an explicit gap, never the network
        srtm_base_url: String::new(),
        ion_base_url: TEST_ION_BASE_URL.to_string(),
        staged_upload_ledgers: Default::default(),
        job_queue,
        realtime: tiletopia_server::realtime::RealtimeState::new(),
        audit_log,
        catalog: tiletopia_server::catalog::OpenDataCatalog::new(),
        started_at: std::time::Instant::now(),
        api_key_rate_limiter: tiletopia_server::api_keys::RateLimiter::new(),
        account_limits,
        webhooks,
        export_engine,
        scheduler,
        model_registry: tiletopia_server::model_registry::ModelRegistry::new(),
        cog_engine: tiletopia_server::cog::CogEngine::new(),
        elevation_store,
        analysis_engines,
        imagery_layers: tiletopia_server::imagery::ImageryLayers::new(),
        global_terrain,
        entity_link_store: tiletopia_server::entity_linking::EntityLinkStore::new(),
        ids_check,
        #[cfg(feature = "martin")]
        martin_backend: tiletopia_server::map_tiles::martin_backend::MartinTileBackend::new(),
        #[cfg(feature = "martin")]
        tileset_dir: tileset_dir.clone(),
        #[cfg(feature = "martin")]
        current_tileset_build: tiletopia_server::tilesets::CurrentBuild::new(),
    })
}

const QUANTIZED_MAX: f64 = 32767.0;

pub struct DecodedTile {
    pub min_height: f64,
    pub max_height: f64,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
    pub heights: Vec<u16>,
    pub edge_vertex_counts: Vec<usize>,
}

impl DecodedTile {
    pub fn vertices(
        &self,
        coord: &tiletopia_terrain::global_dem::TerrainTileCoord,
    ) -> Vec<[f64; 3]> {
        let [west, south, east, north] = coord.bounds();
        (0..self.u.len())
            .map(|index| {
                [
                    west + self.u[index] as f64 / QUANTIZED_MAX * (east - west),
                    south + self.v[index] as f64 / QUANTIZED_MAX * (north - south),
                    self.min_height
                        + self.heights[index] as f64 / QUANTIZED_MAX
                            * (self.max_height - self.min_height),
                ]
            })
            .collect()
    }
}

pub fn decode_tile(bytes: &[u8]) -> DecodedTile {
    let u32_at = |pos: usize| u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
    let u16_at = |pos: usize| u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
    let f32_at = |pos: usize| f32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as f64;

    let min_height = f32_at(24);
    let max_height = f32_at(28);
    let mut pos = 88;
    let vertex_count = u32_at(pos);
    pos += 4;

    let mut channels = Vec::new();
    for _ in 0..3 {
        let mut channel = Vec::with_capacity(vertex_count);
        let mut running: i32 = 0;
        for _ in 0..vertex_count {
            let code = u16_at(pos) as i32;
            running += (code >> 1) ^ -(code & 1);
            channel.push(running as u16);
            pos += 2;
        }
        channels.push(channel);
    }

    let triangle_count = u32_at(pos);
    pos += 4;
    let mut highest = 0usize;
    for _ in 0..triangle_count * 3 {
        let code = u16_at(pos) as usize;
        assert!(
            code <= highest,
            "high water mark code {code} over {highest}"
        );
        assert!(highest - code < vertex_count);
        if code == 0 {
            highest += 1;
        }
        pos += 2;
    }

    let mut edge_vertex_counts = Vec::new();
    for _ in 0..4 {
        let count = u32_at(pos);
        pos += 4 + count * 2;
        edge_vertex_counts.push(count);
    }
    assert_eq!(pos, bytes.len(), "the tile has bytes past its edge lists");

    let heights = channels.pop().unwrap();
    let v = channels.pop().unwrap();
    let u = channels.pop().unwrap();
    DecodedTile {
        min_height,
        max_height,
        u,
        v,
        heights,
        edge_vertex_counts,
    }
}
