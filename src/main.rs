mod config;
mod generator;
mod mbtiles;
mod mvt;
mod postgis;
mod storage;
mod tiles;
mod updater;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use updater::start_listener;

#[derive(Parser)]
#[command(
    name = "postile",
    about = "PostGIS vector tile generator with incremental MBTiles updates"
)]
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

    /// Watch PostgreSQL LISTEN/NOTIFY and update MBTiles incrementally
    Watch,

    /// Generate tiles, optionally publish, then watch for incremental updates
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
    let app_config = Arc::new(config::load_config(&cli.config)?);
    let publisher = storage::StoragePublisher::from_config(&app_config.publish)?.map(Arc::new);

    match cli.command {
        Commands::Generate => {
            let reader = postgis::PostgisReader::connect(&app_config.database).await?;
            generate_all_sources(&app_config, &reader).await?;
            publish_after_generate(&app_config, publisher.as_deref()).await?;
            info!("Tile generation complete");
        }
        Commands::Watch => {
            watch_updates(app_config, publisher).await?;
        }
        Commands::Run => {
            let reader = postgis::PostgisReader::connect(&app_config.database).await?;
            generate_all_sources(&app_config, &reader).await?;
            publish_after_generate(&app_config, publisher.as_deref()).await?;
            info!("Tile generation complete, starting incremental watcher...");
            watch_updates(app_config, publisher).await?;
        }
    }

    Ok(())
}

async fn generate_all_sources(
    config: &config::AppConfig,
    reader: &postgis::PostgisReader,
) -> Result<()> {
    for source in &config.sources {
        generator::generate_source(source, reader).await?;
    }
    Ok(())
}

async fn publish_after_generate(
    config: &config::AppConfig,
    publisher: Option<&storage::StoragePublisher>,
) -> Result<()> {
    if !config.publish.publish_on_generate_enabled() {
        return Ok(());
    }

    if let Some(publisher) = publisher {
        for source in &config.sources {
            publisher
                .publish_mbtiles(&source.mbtiles_path, "full-generate")
                .await?;
        }
    }

    Ok(())
}

async fn watch_updates(
    config: Arc<config::AppConfig>,
    publisher: Option<Arc<storage::StoragePublisher>>,
) -> Result<()> {
    // Open one MbtilesStore per source
    let mut stores: HashMap<String, Arc<Mutex<mbtiles::MbtilesStore>>> = HashMap::new();
    for source in &config.sources {
        let store = mbtiles::MbtilesStore::open(&source.mbtiles_path)?;
        stores.insert(source.name.clone(), Arc::new(Mutex::new(store)));
    }

    info!(
        "Watching PostgreSQL notifications for {} source(s)",
        stores.len()
    );

    let mut listener_task = tokio::spawn(start_listener(
        config.clone(),
        stores,
        publisher.clone(),
    ));

    tokio::select! {
        result = &mut listener_task => {
            result??;
        }
        _ = shutdown_signal() => {
            listener_task.abort();
            let _ = listener_task.await;
            info!("Incremental watcher shut down");
        }
    }

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
