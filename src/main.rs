mod cache;
mod config;
mod generator;
mod mbtiles;
mod mvt;
mod postgis;
mod server;
mod tiles;
mod updater;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Parser)]
#[command(name = "postile", about = "PostGIS vector tile server with incremental updates")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate MBTiles from PostGIS using Tippecanoe (full rebuild)
    Generate,

    /// Start the tile server with incremental update listener
    Serve,

    /// Generate tiles then start serving
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    let app_config = config::load_config(&cli.config)?;

    match cli.command {
        Commands::Generate => {
            let reader = postgis::PostgisReader::connect(&app_config.database).await?;
            generator::generate_full(&app_config, &reader).await?;
            info!("Tile generation complete");
        }
        Commands::Serve => {
            start_server(app_config).await?;
        }
        Commands::Run => {
            let reader = postgis::PostgisReader::connect(&app_config.database).await?;
            generator::generate_full(&app_config, &reader).await?;
            info!("Tile generation complete, starting server...");
            start_server(app_config).await?;
        }
    }

    Ok(())
}

async fn start_server(app_config: config::AppConfig) -> Result<()> {
    let config = Arc::new(app_config);
    let mbtiles_store = mbtiles::MbtilesStore::open(&config.tiles.mbtiles_path)?;
    let mbtiles = Arc::new(Mutex::new(mbtiles_store));

    let cache_size = config.cache.max_tiles.unwrap_or(10_000);
    let tile_cache = Arc::new(cache::TileCache::new(cache_size));
    info!("Tile cache initialized (capacity={})", cache_size);

    // Start the LISTEN/NOTIFY updater in the background
    let updater_config = config.clone();
    let updater_mbtiles = mbtiles.clone();
    let updater_cache = tile_cache.clone();
    tokio::spawn(async move {
        if let Err(e) =
            updater::start_listener(updater_config, updater_mbtiles, updater_cache).await
        {
            tracing::error!("Updater listener failed: {}", e);
        }
    });

    // Start HTTP server with graceful shutdown
    let state = server::AppState {
        mbtiles,
        config: config.clone(),
        cache: tile_cache,
    };

    let router = server::create_router(state);

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    info!("Tile server listening on http://{}", bind_addr);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shut down gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("Received Ctrl+C, shutting down..."),
        _ = terminate => info!("Received SIGTERM, shutting down..."),
    }
}
