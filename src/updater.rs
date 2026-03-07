use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_postgres::{AsyncMessage, NoTls};
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::mbtiles::MbtilesStore;
use crate::mvt;
use crate::postgis::{Bounds, PostgisReader};
use crate::storage::StoragePublisher;
use crate::tiles::{tiles_for_bounds, TileCoord};

#[derive(Debug)]
struct UpdateEvent {
    layer_name: String,
    feature_id: i64,
    old_bounds: Option<Bounds>,
}

/// Start the LISTEN/NOTIFY listener for incremental tile updates
pub async fn start_listener(
    config: Arc<AppConfig>,
    mbtiles: Arc<Mutex<MbtilesStore>>,
    publisher: Option<Arc<StoragePublisher>>,
) -> Result<()> {
    let (client, mut connection) =
        tokio_postgres::connect(&config.database.connection_string(), NoTls)
            .await
            .context("Failed to connect for LISTEN/NOTIFY")?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<tokio_postgres::Notification>();

    tokio::spawn(async move {
        let mut stream = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));

        while let Some(msg) = stream.next().await {
            match msg {
                Ok(AsyncMessage::Notification(n)) => {
                    if tx.send(n).is_err() {
                        break;
                    }
                }
                Ok(AsyncMessage::Notice(notice)) => {
                    info!("PostgreSQL notice: {}", notice.message());
                }
                Ok(_) => {}
                Err(e) => {
                    error!("PostgreSQL connection error: {}", e);
                    break;
                }
            }
        }
    });

    client
        .execute("LISTEN tile_update", &[])
        .await
        .context("Failed to LISTEN")?;

    info!("Listening for tile_update notifications on PostgreSQL");

    let reader = PostgisReader::connect(&config.database).await?;
    let debounce_ms = config.updates.debounce_ms.unwrap_or(200);

    // Debounce loop: collect events over a window, then batch-process
    loop {
        // Wait for the first notification
        let first = match rx.recv().await {
            Some(n) => n,
            None => break,
        };

        let mut events = vec![first];

        // Collect more notifications within the debounce window
        let deadline = Instant::now() + Duration::from_millis(debounce_ms);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(n)) => events.push(n),
                _ => break,
            }
        }

        info!("Processing batch of {} notification(s)", events.len());

        let mut parsed_events = Vec::new();
        for notification in &events {
            let payload = notification.payload();
            match parse_notification(payload) {
                Ok(event) => parsed_events.push(event),
                Err(e) => warn!("Invalid notification payload '{}': {}", payload, e),
            }
        }

        if !parsed_events.is_empty() {
            if let Err(e) = handle_batch_update(
                &config,
                &reader,
                &mbtiles,
                publisher.as_ref(),
                &parsed_events,
            )
            .await
            {
                error!("Failed to handle update batch: {}", e);
            }
        }
    }

    Ok(())
}

fn parse_notification(payload: &str) -> Result<UpdateEvent> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) {
        let layer_name = json["layer"]
            .as_str()
            .context("Missing 'layer' in notification")?
            .to_string();
        let feature_id = json["id"]
            .as_i64()
            .context("Missing 'id' in notification")?;

        let old_bounds = json.get("old_bounds").map(|ob| Bounds {
            min_lon: ob["min_lon"].as_f64().unwrap_or(0.0),
            min_lat: ob["min_lat"].as_f64().unwrap_or(0.0),
            max_lon: ob["max_lon"].as_f64().unwrap_or(0.0),
            max_lat: ob["max_lat"].as_f64().unwrap_or(0.0),
        });

        return Ok(UpdateEvent {
            layer_name,
            feature_id,
            old_bounds,
        });
    }

    let parts: Vec<&str> = payload.splitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Expected format 'layer:id' or JSON");
    }

    Ok(UpdateEvent {
        layer_name: parts[0].to_string(),
        feature_id: parts[1].parse()?,
        old_bounds: None,
    })
}

/// Handle a batch of update events: deduplicate affected tiles, regenerate concurrently
async fn handle_batch_update(
    config: &AppConfig,
    reader: &PostgisReader,
    mbtiles: &Arc<Mutex<MbtilesStore>>,
    publisher: Option<&Arc<StoragePublisher>>,
    events: &[UpdateEvent],
) -> Result<()> {
    // Collect all affected tiles across all events
    let mut all_affected: Vec<TileCoord> = Vec::new();

    for event in events {
        let layer = match config
            .tiles
            .layers
            .iter()
            .find(|l| l.name == event.layer_name)
        {
            Some(l) => l,
            None => {
                warn!("Unknown layer: {}", event.layer_name);
                continue;
            }
        };

        if let Some(feat) = reader.get_feature(layer, event.feature_id).await? {
            all_affected.extend(tiles_for_bounds(
                &feat.bounds,
                config.tiles.min_zoom,
                config.tiles.max_zoom,
            ));
        }

        if let Some(ref old_bounds) = event.old_bounds {
            all_affected.extend(tiles_for_bounds(
                old_bounds,
                config.tiles.min_zoom,
                config.tiles.max_zoom,
            ));
        }
    }

    // Deduplicate
    all_affected.sort_by(|a, b| (a.z, a.x, a.y).cmp(&(b.z, b.x, b.y)));
    all_affected.dedup();

    if all_affected.is_empty() {
        info!("No affected tiles in batch");
        return Ok(());
    }

    info!(
        "Regenerating {} unique tiles from batch",
        all_affected.len()
    );

    // Regenerate tiles concurrently (bounded concurrency)
    let worker_count = config.updates.worker_concurrency.unwrap_or(8).max(1);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(worker_count));
    let mut handles = Vec::new();

    for tile_coord in all_affected.clone() {
        let sem = semaphore.clone();
        let cfg = config.clone();
        let rdr = reader.clone();

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            regenerate_single_tile(&cfg, &rdr, &tile_coord).await
        });
        handles.push((tile_coord, handle));
    }

    // Collect results and write to MBTiles
    let mut encoded: Vec<(TileCoord, Option<Vec<u8>>)> = Vec::new();
    for (coord, handle) in handles {
        match handle.await {
            Ok(Ok(data)) => encoded.push((coord, data)),
            Ok(Err(e)) => error!("Failed to regenerate tile {:?}: {}", coord, e),
            Err(e) => error!("Task panicked for tile {:?}: {}", coord, e),
        }
    }

    // Write to MBTiles under lock (no .await while holding)
    let store = mbtiles.lock().await;
    store.begin_transaction()?;

    let write_result: Result<()> = (|| {
        for (coord, data) in &encoded {
            match data {
                None => store.delete_tile(coord.z, coord.x, coord.y)?,
                Some(tile_data) => store.put_tile(coord.z, coord.x, coord.y, tile_data)?,
            }
        }
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = store.rollback_transaction();
        return Err(e);
    }

    store.commit_transaction()?;
    drop(store);

    if let Some(publisher) = publisher {
        if config.publish.publish_on_update_enabled() {
            publisher
                .publish_mbtiles(&config.tiles.mbtiles_path, "incremental-update")
                .await?;
        }
    }

    info!("Batch update complete ({} tiles)", all_affected.len());
    Ok(())
}

async fn regenerate_single_tile(
    config: &AppConfig,
    reader: &PostgisReader,
    tile_coord: &TileCoord,
) -> Result<Option<Vec<u8>>> {
    let bounds = tile_coord.bounds();
    let mut features_by_layer: HashMap<String, Vec<crate::postgis::FeatureData>> = HashMap::new();

    for layer in &config.tiles.layers {
        let features = reader.get_features_in_bounds(layer, &bounds).await?;
        if !features.is_empty() {
            features_by_layer.insert(layer.name.clone(), features);
        }
    }

    if features_by_layer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(mvt::encode_tile(tile_coord, &features_by_layer)?))
    }
}
