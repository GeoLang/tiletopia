use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

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

    /// Show information about a tileset or source file
    Info {
        /// Path to tileset.json or source file
        path: PathBuf,
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
            let state = Arc::new(tiletopia_server::AppState {
                assets: RwLock::new(vec![]),
                data_dir,
                realtime: tiletopia_server::realtime::RealtimeState::new(),
            });

            let app = tiletopia_server::router(state);
            let addr = format!("{}:{}", host, port);
            tracing::info!("tiletopia server listening on http://{}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
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
    }

    Ok(())
}
