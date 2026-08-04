use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "tiletopia",
    version,
    about = "Fast open-source 3D Tiles server"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tile a geospatial dataset into 3D Tiles
    Tile {
        /// Input file (LAS, LAZ, GeoTIFF, glTF, CityGML, etc.)
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory for tileset
        #[arg(short, long)]
        output: PathBuf,

        /// Maximum geometric error for LOD
        #[arg(long, default_value = "1.0")]
        max_error: f64,
    },

    /// Start the tile server
    Serve {
        /// Data directory containing tilesets
        #[arg(short, long, default_value = "./data")]
        data_dir: PathBuf,

        /// Listen address
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Listen port
        #[arg(short, long, default_value = "3000")]
        port: u16,
    },

    /// Set a user's role (viewer/editor/admin). Works offline against the
    /// sqlite database, so the first admin can be promoted before any admin
    /// HTTP route is reachable.
    SetRole {
        /// User email or id
        user: String,

        /// New role: viewer, editor, or admin
        role: String,

        /// Data directory containing tiletopia.db
        #[arg(short, long, default_value = "./data")]
        data_dir: PathBuf,
    },

    /// Show information about a tileset or source file
    Info {
        /// Path to tileset.json or source file
        path: PathBuf,
    },

    /// Validate a tileset.json file
    Validate {
        /// Path to tileset.json
        path: PathBuf,
    },

    /// Build for edge/embedded deployment (ARM, minimal binary)
    Edge {
        /// Target triple (e.g. aarch64-unknown-linux-musl)
        #[arg(long, default_value = "aarch64-unknown-linux-musl")]
        target: String,

        /// Strip debug symbols
        #[arg(long, default_value = "true")]
        strip: bool,

        /// Output directory
        #[arg(short, long, default_value = "./dist")]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Tile {
            input,
            output,
            max_error,
        } => {
            tracing::info!("Tiling {} → {}", input.display(), output.display());
            tracing::info!("Max geometric error: {}", max_error);

            // Read source data
            let points = tiletopia_ingest::read_point_cloud(&input)?;
            tracing::info!("Read {} points", points.len());

            // Convert ingest points to octree points
            let octree_points: Vec<tiletopia_core::octree::OctreePoint> = points
                .into_iter()
                .map(|p| tiletopia_core::octree::OctreePoint {
                    position: [p.x, p.y, p.z],
                    color: [p.r, p.g, p.b],
                    intensity: p.intensity,
                    classification: p.classification,
                })
                .collect();

            // Run tiling pipeline
            let config = tiletopia_core::tileset::TilingConfig {
                octree: tiletopia_core::octree::OctreeConfig {
                    max_points_per_node: 20_000,
                    ..Default::default()
                },
                max_geometric_error: max_error,
            };

            let stats = tiletopia_core::tileset::tile_point_cloud(octree_points, &output, &config)?;

            tracing::info!(
                "Done! {} nodes ({} leaf, {} internal), max depth {}",
                stats.total_nodes,
                stats.leaf_nodes,
                stats.internal_nodes,
                stats.max_depth,
            );
        }

        Commands::Serve {
            data_dir,
            host,
            port,
        } => {
            // checked before anything else: serving with no JWT secret would
            // leave every endpoint open, so refuse to start instead
            tiletopia_server::auth::startup_check().map_err(anyhow::Error::msg)?;
            // a bbox typo would otherwise only show up as failing analysis tiles
            tiletopia_server::analysis_tiles::startup_check().map_err(anyhow::Error::msg)?;

            std::fs::create_dir_all(&data_dir)?;

            let db_url = format!(
                "sqlite://{}?mode=rwc",
                data_dir.join("tiletopia.db").display()
            );
            let db = Arc::new(
                tiletopia_server::db::Database::new(&db_url)
                    .await
                    .expect("failed to open database"),
            );
            db.migrate().await.expect("failed to run migrations");

            let store: Arc<dyn tiletopia_store::TileStore> =
                Arc::new(tiletopia_store::LocalStore::new(data_dir.clone()));

            let job_queue = Arc::new(tiletopia_server::job_queue::JobQueue::new(
                Arc::clone(&db),
                data_dir.clone(),
                Arc::clone(&store),
            ));
            Arc::clone(&job_queue).start().await;

            let state = Arc::new(tiletopia_server::AppState {
                db,
                store,
                data_dir,
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
                photogrammetry_engine: tiletopia_server::photogrammetry::PhotogrammetryEngine::new(
                ),
                classification_engine: tiletopia_server::classification::ClassificationEngine::new(
                ),
                model_registry: tiletopia_server::model_registry::ModelRegistry::new(),
                collaboration_engine: tiletopia_server::collaboration::CollaborationEngine::new(),
                versioning_engine: tiletopia_server::versioning::VersioningEngine::new(),
                bim4d_engine: tiletopia_server::bim4d::Bim4DEngine::new(),
                cog_engine: tiletopia_server::cog::CogEngine::new(),
                routing_engine: tiletopia_server::routing::RoutingEngine::new(),
                map_tile_engine: tiletopia_server::map_tiles::MapTileEngine::new(),
                feature_service_engine:
                    tiletopia_server::feature_service::FeatureServiceEngine::new(),
                issue_tracker: tiletopia_server::issue_tracking::IssueTracker::new(),
                elevation_store: std::sync::Arc::new(tiletopia_server::elevation::DemStore::new()),
                analysis_engines: tiletopia_server::analysis_tiles::AnalysisEngines::new(),
                entity_link_store: tiletopia_server::entity_linking::EntityLinkStore::new(),
            });

            let app = tiletopia_server::router(state);
            let addr = format!("{}:{}", host, port);
            tracing::info!("tiletopia server listening on http://{}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::SetRole {
            user,
            role,
            data_dir,
        } => {
            use tiletopia_server::users::UserRole;

            let role: UserRole = role.parse().map_err(anyhow::Error::msg)?;

            let db_url = format!(
                "sqlite://{}?mode=rwc",
                data_dir.join("tiletopia.db").display()
            );
            let db = tiletopia_server::db::Database::new(&db_url).await?;
            db.migrate().await?;

            // accept either an email or a uuid
            let mut record = match uuid::Uuid::parse_str(&user) {
                Ok(id) => db.get_user(id).await?,
                Err(_) => db.get_user_by_email(&user).await?.map(|(u, _)| u),
            };
            let Some(u) = record.as_mut() else {
                anyhow::bail!("no user found matching '{user}'");
            };

            u.role = role;
            db.update_user(u).await?;
            println!("set {} ({}) to role {:?}", u.email, u.id, u.role);
        }

        Commands::Info { path } => {
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let data = std::fs::read_to_string(&path)?;
                let tileset: tiletopia_core::Tileset = serde_json::from_str(&data)?;
                println!("Tileset version: {}", tileset.asset.version);
                println!(
                    "Generator: {}",
                    tileset.asset.generator.as_deref().unwrap_or("unknown")
                );
                println!("Root geometric error: {}", tileset.geometric_error);
            } else {
                let points = tiletopia_ingest::read_point_cloud(&path)?;
                println!("Points: {}", points.len());
            }
        }

        Commands::Validate { path } => {
            let data = std::fs::read_to_string(&path)?;
            let errors = tiletopia_server::cicd::validate_tileset(&data);
            if errors.is_empty() {
                println!("✓ {} is valid", path.display());
            } else {
                println!("✗ {} has {} issue(s):", path.display(), errors.len());
                for err in &errors {
                    println!("  [{:?}] {}: {}", err.severity, err.check, err.message);
                }
                std::process::exit(1);
            }
        }

        Commands::Edge {
            target,
            strip,
            output,
        } => {
            println!("Building TileTopia for edge deployment");
            println!("  Target: {}", target);
            println!("  Strip: {}", strip);
            println!("  Output: {}", output.display());
            println!();
            println!("Run the following commands:");
            println!("  rustup target add {}", target);
            let mut cmd = format!(
                "  cargo build --release --target {} --no-default-features",
                target
            );
            if strip {
                cmd.push_str(" && strip target/{}/release/tiletopia-cli");
            }
            println!("{}", cmd);
            println!(
                "  cp target/{}/release/tiletopia-cli {}/tiletopia",
                target,
                output.display()
            );
            println!();
            println!("Supported edge targets:");
            println!("  aarch64-unknown-linux-musl   (ARM64 - Raspberry Pi, Jetson, drones)");
            println!("  armv7-unknown-linux-musleabihf (ARMv7 - older embedded)");
            println!("  x86_64-unknown-linux-musl    (x86 static binary)");

            // Create output dir
            std::fs::create_dir_all(&output)?;

            // Write edge deployment readme
            let readme = format!(
                "# TileTopia Edge Deployment\n\n\
                 Binary built for: {}\n\n\
                 ## Usage\n\n\
                 ```sh\n\
                 # Tile locally on device\n\
                 ./tiletopia tile --input scan.las --output ./tiles\n\n\
                 # Serve tiles on local network\n\
                 ./tiletopia serve --data-dir ./tiles --port 3000\n\
                 ```\n\n\
                 ## Resource Requirements\n\n\
                 - RAM: 256MB minimum, 1GB recommended\n\
                 - Storage: depends on dataset size\n\
                 - No runtime dependencies (statically linked)\n",
                target
            );
            std::fs::write(output.join("README.md"), readme)?;
            println!("✓ Edge deployment package prepared at {}", output.display());
        }
    }

    Ok(())
}
