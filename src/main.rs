mod config;
mod diff;
mod events;
mod generator;
mod inspect;
mod mbtiles;
mod mvt;
mod postgis;
mod server;
mod storage;
mod tiles;
mod updater;
mod validate;
mod webhook;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use updater::start_listener;

#[derive(Parser)]
#[command(
    name = "tilefeed",
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

    /// Generate, then serve tiles over HTTP and watch for updates
    Serve,

    /// Inspect an MBTiles file (metadata, tile counts, sizes)
    Inspect {
        /// Path to the MBTiles file to inspect
        path: String,
    },

    /// Validate config against the actual database
    Validate,

    /// Compare two MBTiles files and show differences
    Diff {
        /// Path to the first MBTiles file
        path_a: String,
        /// Path to the second MBTiles file
        path_b: String,
    },
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

    match cli.command {
        Commands::Inspect { path } => {
            inspect::inspect_mbtiles(&path)?;
        }
        Commands::Diff { path_a, path_b } => {
            diff::diff_mbtiles(&path_a, &path_b)?;
        }
        _ => {
            // Commands that need the full config
            let app_config = Arc::new(config::load_config(&cli.config)?);
            let publisher =
                storage::StoragePublisher::from_config(&app_config.publish)?.map(Arc::new);

            // Create event bus for webhooks and SSE
            let event_tx = events::create_event_bus();

            // Start webhook notifier if configured
            if app_config.webhook.is_configured() {
                let notifier = webhook::WebhookNotifier::new(app_config.webhook.clone());
                notifier.start(&event_tx);
                info!(
                    "Webhook notifier started for {} URL(s)",
                    app_config.webhook.urls.len()
                );
            }

            // Check required external tools before starting
            generator::check_required_tools(
                &app_config.sources,
                app_config.tippecanoe_bin.as_deref(),
                app_config.ogr2ogr_bin.as_deref(),
            )?;

            match cli.command {
                Commands::Generate => {
                    let reader = postgis::PostgisReader::connect(&app_config.database).await?;
                    generate_all_sources(&app_config, &reader, &event_tx).await?;
                    publish_after_generate(&app_config, publisher.as_deref()).await?;
                    info!("Tile generation complete");
                }
                Commands::Watch => {
                    watch_updates(app_config, publisher, event_tx).await?;
                }
                Commands::Run => {
                    let reader = postgis::PostgisReader::connect(&app_config.database).await?;
                    generate_all_sources(&app_config, &reader, &event_tx).await?;
                    publish_after_generate(&app_config, publisher.as_deref()).await?;
                    info!("Tile generation complete, starting incremental watcher...");
                    watch_updates(app_config, publisher, event_tx).await?;
                }
                Commands::Serve => {
                    let reader = postgis::PostgisReader::connect(&app_config.database).await?;
                    generate_all_sources(&app_config, &reader, &event_tx).await?;
                    publish_after_generate(&app_config, publisher.as_deref()).await?;
                    info!("Tile generation complete, starting server and watcher...");
                    serve_and_watch(app_config, publisher, event_tx).await?;
                }
                Commands::Validate => {
                    let valid = validate::validate_config(&app_config).await?;
                    if !valid {
                        std::process::exit(1);
                    }
                }
                // Already handled above
                Commands::Inspect { .. } | Commands::Diff { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

async fn generate_all_sources(
    config: &config::AppConfig,
    reader: &postgis::PostgisReader,
    event_tx: &events::EventSender,
) -> Result<()> {
    for source in &config.sources {
        let start = std::time::Instant::now();
        generator::generate_source(
            source,
            reader,
            config.tippecanoe_bin.as_deref(),
            config.ogr2ogr_bin.as_deref(),
        )
        .await?;
        let duration_ms = start.elapsed().as_millis() as u64;
        let _ = event_tx.send(events::TileEvent::GenerateComplete {
            source: source.name.clone(),
            duration_ms,
        });
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

fn open_stores(
    config: &config::AppConfig,
) -> Result<HashMap<String, Arc<Mutex<mbtiles::MbtilesStore>>>> {
    let mut stores = HashMap::new();
    for source in &config.sources {
        let store = mbtiles::MbtilesStore::open(&source.mbtiles_path)?;
        stores.insert(source.name.clone(), Arc::new(Mutex::new(store)));
    }
    Ok(stores)
}

async fn watch_updates(
    config: Arc<config::AppConfig>,
    publisher: Option<Arc<storage::StoragePublisher>>,
    event_tx: events::EventSender,
) -> Result<()> {
    let stores = open_stores(&config)?;

    info!(
        "Watching PostgreSQL notifications for {} source(s)",
        stores.len()
    );

    let mut listener_task = tokio::spawn(start_listener(
        config.clone(),
        stores,
        publisher.clone(),
        Some(event_tx),
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

async fn serve_and_watch(
    config: Arc<config::AppConfig>,
    publisher: Option<Arc<storage::StoragePublisher>>,
    event_tx: events::EventSender,
) -> Result<()> {
    let stores = open_stores(&config)?;

    info!("Starting server and watcher for {} source(s)", stores.len());

    let mut listener_task = tokio::spawn(start_listener(
        config.clone(),
        stores.clone(),
        publisher.clone(),
        Some(event_tx.clone()),
    ));
    let mut server_task =
        tokio::spawn(server::start_server(config.clone(), stores, Some(event_tx)));

    tokio::select! {
        result = &mut listener_task => {
            result??;
        }
        result = &mut server_task => {
            result??;
        }
        _ = shutdown_signal() => {
            listener_task.abort();
            server_task.abort();
            let _ = listener_task.await;
            let _ = server_task.await;
            info!("Server and watcher shut down");
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
