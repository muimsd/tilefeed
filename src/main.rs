mod config;
mod diff;
mod events;
mod generator;
mod inspect;
mod mbtiles;
mod metrics;
mod mvt;
mod postgis;
mod server;
mod storage;
mod tiles;
mod updater;
mod validate;
mod webhook;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info, warn};
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
    Run {
        /// Start from the existing MBTiles instead of rebuilding it first
        #[arg(long)]
        skip_generate: bool,
    },

    /// Generate, then serve tiles over HTTP and watch for updates
    Serve {
        /// Serve the existing MBTiles immediately instead of rebuilding it first.
        /// Skips Tippecanoe/GDAL entirely, so restarts are instant and the server
        /// comes up even while PostgreSQL is unreachable.
        #[arg(long)]
        skip_generate: bool,
    },

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

impl Commands {
    /// Short name used for logging and the startup metric.
    fn name(&self) -> &'static str {
        match self {
            Commands::Generate => "generate",
            Commands::Watch => "watch",
            Commands::Run { .. } => "run",
            Commands::Serve { .. } => "serve",
            Commands::Inspect { .. } => "inspect",
            Commands::Validate => "validate",
            Commands::Diff { .. } => "diff",
        }
    }

    /// Whether this invocation will run a full tile generation.
    fn generates(&self) -> bool {
        match self {
            Commands::Generate => true,
            Commands::Run { skip_generate } | Commands::Serve { skip_generate } => !skip_generate,
            _ => false,
        }
    }
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

    // Before anything else: uptime is measured from here.
    metrics::init();

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

            // Tippecanoe and GDAL are only needed to generate; a serving-only
            // process should not require them to be installed at all.
            if cli.command.generates() {
                generator::check_required_tools(
                    &app_config.sources,
                    app_config.tippecanoe_bin.as_deref(),
                    app_config.ogr2ogr_bin.as_deref(),
                )?;
            }
            metrics::record_startup(cli.command.name(), cli.command.generates());

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
                Commands::Run { skip_generate } => {
                    if skip_generate {
                        info!("Skipping generation, watching existing MBTiles for updates...");
                    } else {
                        let reader = postgis::PostgisReader::connect(&app_config.database).await?;
                        generate_all_sources(&app_config, &reader, &event_tx).await?;
                        publish_after_generate(&app_config, publisher.as_deref()).await?;
                        info!("Tile generation complete, starting incremental watcher...");
                    }
                    watch_updates(app_config, publisher, event_tx).await?;
                }
                Commands::Serve { skip_generate } => {
                    if skip_generate {
                        info!("Skipping generation, serving the existing MBTiles...");
                    } else {
                        let reader = postgis::PostgisReader::connect(&app_config.database).await?;
                        generate_all_sources(&app_config, &reader, &event_tx).await?;
                        publish_after_generate(&app_config, publisher.as_deref()).await?;
                        info!("Tile generation complete, starting server and watcher...");
                    }
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
        let result = generator::generate_source(
            source,
            reader,
            config.tippecanoe_bin.as_deref(),
            config.ogr2ogr_bin.as_deref(),
        )
        .await;

        metrics::metrics()
            .generate_duration
            .with(&[&source.name])
            .observe_since(start);
        metrics::metrics()
            .generate_total
            .with(&[&source.name, metrics::result_label(&result)])
            .inc();
        result?;

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
        let store = mbtiles::MbtilesStore::open(&source.mbtiles_path)
            .with_context(|| format!("Cannot open source '{}'", source.name))?;

        // Worth stating plainly: without a generation step, this line is the only
        // confirmation that the file being served actually holds tiles.
        match store.tile_count() {
            Ok(count) => {
                info!(
                    "Source '{}': {} tiles from {}",
                    source.name, count, source.mbtiles_path
                );
                metrics::metrics()
                    .mbtiles_tiles
                    .with(&[&source.name])
                    .set(count as i64);
            }
            Err(e) => warn!("Could not count tiles for source '{}': {}", source.name, e),
        }

        stores.insert(source.name.clone(), Arc::new(Mutex::new(store)));
    }
    Ok(stores)
}

/// Start the dedicated metrics listener configured by `[metrics] port`.
///
/// `skip_if_serve_addr` is set by commands that run a tile server: that server
/// serves the metrics path itself when the two addresses coincide, so starting a
/// second listener on the same socket would just fail to bind.
fn spawn_metrics_exporter(
    config: &config::AppConfig,
    skip_if_serve_addr: bool,
) -> Option<tokio::task::JoinHandle<()>> {
    if skip_if_serve_addr && config.metrics_addr_is_serve_addr() {
        return None;
    }

    let (host, port) = config.metrics_addr()?;
    let path = config.metrics.path();

    Some(tokio::spawn(async move {
        // A metrics endpoint that can't bind must not take the pipeline down with
        // it, but it also must not fail silently — nothing else would notice.
        if let Err(e) = metrics::start_metrics_server(&host, port, &path).await {
            error!("Metrics server on port {} stopped: {}", port, e);
        }
    }))
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

    let metrics_task = spawn_metrics_exporter(&config, false);

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

    if let Some(task) = metrics_task {
        task.abort();
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

    let metrics_task = spawn_metrics_exporter(&config, true);

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

    if let Some(task) = metrics_task {
        task.abort();
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn test_serve_generates_by_default() {
        let cli = parse(&["tilefeed", "serve"]);
        assert!(matches!(
            cli.command,
            Commands::Serve {
                skip_generate: false
            }
        ));
        assert!(cli.command.generates());
        assert_eq!(cli.command.name(), "serve");
    }

    #[test]
    fn test_serve_skip_generate() {
        let cli = parse(&["tilefeed", "serve", "--skip-generate"]);
        assert!(matches!(
            cli.command,
            Commands::Serve {
                skip_generate: true
            }
        ));
        // Drives both the Tippecanoe/GDAL precondition check and the startup metric
        assert!(!cli.command.generates());
    }

    #[test]
    fn test_run_skip_generate() {
        let cli = parse(&["tilefeed", "run", "--skip-generate"]);
        assert!(matches!(
            cli.command,
            Commands::Run {
                skip_generate: true
            }
        ));
        assert!(!cli.command.generates());
        assert_eq!(cli.command.name(), "run");
    }

    #[test]
    fn test_only_generating_commands_report_generates() {
        assert!(parse(&["tilefeed", "generate"]).command.generates());
        assert!(!parse(&["tilefeed", "watch"]).command.generates());
        assert!(!parse(&["tilefeed", "validate"]).command.generates());
        assert!(!parse(&["tilefeed", "inspect", "a.mbtiles"])
            .command
            .generates());
        assert!(!parse(&["tilefeed", "diff", "a.mbtiles", "b.mbtiles"])
            .command
            .generates());
    }

    #[test]
    fn test_command_names_are_stable_metric_labels() {
        for (args, name) in [
            (vec!["tilefeed", "generate"], "generate"),
            (vec!["tilefeed", "watch"], "watch"),
            (vec!["tilefeed", "run"], "run"),
            (vec!["tilefeed", "serve"], "serve"),
            (vec!["tilefeed", "validate"], "validate"),
            (vec!["tilefeed", "inspect", "a.mbtiles"], "inspect"),
            (vec!["tilefeed", "diff", "a.mbtiles", "b.mbtiles"], "diff"),
        ] {
            assert_eq!(parse(&args).command.name(), name);
        }
    }

    #[test]
    fn test_skip_generate_is_rejected_on_non_generating_commands() {
        // `watch` never generated in the first place, so the flag would be a lie
        assert!(Cli::try_parse_from(["tilefeed", "watch", "--skip-generate"]).is_err());
        assert!(Cli::try_parse_from(["tilefeed", "generate", "--skip-generate"]).is_err());
    }

    #[test]
    fn test_record_startup_exports_mode() {
        metrics::record_startup("serve", false);
        let out = metrics::metrics().render();
        assert!(out.contains("tilefeed_startup_info{command=\"serve\",generated=\"false\"} 1"));
    }
}
