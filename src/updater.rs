use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_postgres::{AsyncMessage, NoTls};
use tracing::{error, info, warn};

use crate::config::{AppConfig, DerivedGeomType};
use crate::events::EventSender;
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

/// Start the LISTEN/NOTIFY listener for incremental tile updates with auto-reconnect
pub async fn start_listener(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    publisher: Option<Arc<StoragePublisher>>,
    event_tx: Option<EventSender>,
) -> Result<()> {
    let mut retry_count: u32 = 0;
    let max_retry_delay_secs: u64 = 60;

    loop {
        match run_listener(&config, &stores, publisher.as_ref(), event_tx.as_ref()).await {
            Ok(()) => {
                // Clean exit (channel closed)
                info!("Listener exited cleanly");
                return Ok(());
            }
            Err(e) => {
                retry_count += 1;
                let delay_secs = (2u64.pow(retry_count.min(6))).min(max_retry_delay_secs);
                error!(
                    "Listener connection lost: {}. Reconnecting in {}s (attempt {})...",
                    e, delay_secs, retry_count
                );
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            }
        }
    }
}

async fn run_listener(
    config: &AppConfig,
    stores: &HashMap<String, Arc<Mutex<MbtilesStore>>>,
    publisher: Option<&Arc<StoragePublisher>>,
    event_tx: Option<&EventSender>,
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
            None => return Ok(()), // channel closed, will trigger reconnect in outer loop
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
            if let Err(e) =
                handle_batch_update(config, &reader, stores, publisher, event_tx, &parsed_events)
                    .await
            {
                error!("Failed to handle update batch: {}", e);
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_notification tests ---

    #[test]
    fn test_parse_notification_json_minimal() {
        let payload = r#"{"layer": "buildings", "id": 42}"#;
        let event = parse_notification(payload).unwrap();
        assert_eq!(event.layer_name, "buildings");
        assert_eq!(event.feature_id, 42);
        assert!(event.old_bounds.is_none());
    }

    #[test]
    fn test_parse_notification_json_with_old_bounds() {
        let payload = r#"{"layer": "roads", "id": 7, "old_bounds": {"min_lon": -0.5, "min_lat": 51.0, "max_lon": 0.5, "max_lat": 52.0}}"#;
        let event = parse_notification(payload).unwrap();
        assert_eq!(event.layer_name, "roads");
        assert_eq!(event.feature_id, 7);
        let bounds = event.old_bounds.unwrap();
        assert!((bounds.min_lon - (-0.5)).abs() < 1e-6);
        assert!((bounds.min_lat - 51.0).abs() < 1e-6);
        assert!((bounds.max_lon - 0.5).abs() < 1e-6);
        assert!((bounds.max_lat - 52.0).abs() < 1e-6);
    }

    #[test]
    fn test_parse_notification_simple_format() {
        let payload = "water:123";
        let event = parse_notification(payload).unwrap();
        assert_eq!(event.layer_name, "water");
        assert_eq!(event.feature_id, 123);
        assert!(event.old_bounds.is_none());
    }

    #[test]
    fn test_parse_notification_simple_negative_id() {
        let event = parse_notification("layer:-1").unwrap();
        assert_eq!(event.feature_id, -1);
    }

    #[test]
    fn test_parse_notification_invalid_format() {
        let result = parse_notification("no_colon_here");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_notification_invalid_id() {
        let result = parse_notification("layer:not_a_number");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_notification_json_missing_layer() {
        let payload = r#"{"id": 42}"#;
        let result = parse_notification(payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_notification_json_missing_id() {
        let payload = r#"{"layer": "test"}"#;
        let result = parse_notification(payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_notification_empty_string() {
        let result = parse_notification("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_notification_json_large_id() {
        let payload = r#"{"layer": "test", "id": 9999999999}"#;
        let event = parse_notification(payload).unwrap();
        assert_eq!(event.feature_id, 9999999999);
    }

    #[test]
    fn test_parse_notification_json_old_bounds_partial() {
        // old_bounds with missing fields should default to 0.0
        let payload = r#"{"layer": "test", "id": 1, "old_bounds": {"min_lon": 5.0}}"#;
        let event = parse_notification(payload).unwrap();
        let bounds = event.old_bounds.unwrap();
        assert!((bounds.min_lon - 5.0).abs() < 1e-6);
        assert!((bounds.min_lat - 0.0).abs() < 1e-6);
    }
}

/// Handle a batch of update events: group by source, deduplicate affected tiles, regenerate
async fn handle_batch_update(
    config: &AppConfig,
    reader: &PostgisReader,
    stores: &HashMap<String, Arc<Mutex<MbtilesStore>>>,
    publisher: Option<&Arc<StoragePublisher>>,
    event_tx: Option<&EventSender>,
    events: &[UpdateEvent],
) -> Result<()> {
    // Group events by source name
    let mut events_by_source: HashMap<String, Vec<&UpdateEvent>> = HashMap::new();

    for event in events {
        match config.find_source_for_layer(&event.layer_name) {
            Some(source) => {
                events_by_source
                    .entry(source.name.clone())
                    .or_default()
                    .push(event);
            }
            None => {
                warn!("Unknown layer '{}', not in any source", event.layer_name);
            }
        }
    }

    // Process each affected source independently
    for (source_name, source_events) in &events_by_source {
        let source = config
            .sources
            .iter()
            .find(|s| &s.name == source_name)
            .unwrap();

        let store = match stores.get(source_name) {
            Some(s) => s,
            None => {
                error!("No MBTiles store for source '{}'", source_name);
                continue;
            }
        };

        update_source(
            config,
            reader,
            source,
            store,
            publisher,
            event_tx,
            source_events,
        )
        .await?;
    }

    Ok(())
}

/// Update a single source's MBTiles with affected tiles
async fn update_source(
    config: &AppConfig,
    reader: &PostgisReader,
    source: &crate::config::SourceConfig,
    mbtiles: &Arc<Mutex<MbtilesStore>>,
    publisher: Option<&Arc<StoragePublisher>>,
    event_tx: Option<&EventSender>,
    events: &[&UpdateEvent],
) -> Result<()> {
    let mut all_affected: Vec<TileCoord> = Vec::new();

    for event in events {
        let layer = match source.find_layer(&event.layer_name) {
            Some(l) => l,
            None => continue,
        };

        if let Some(feat) = reader.get_feature(layer, event.feature_id).await? {
            all_affected.extend(tiles_for_bounds(
                &feat.bounds,
                source.min_zoom,
                source.max_zoom,
            ));
        }

        if let Some(ref old_bounds) = event.old_bounds {
            all_affected.extend(tiles_for_bounds(
                old_bounds,
                source.min_zoom,
                source.max_zoom,
            ));
        }
    }

    // Deduplicate
    all_affected.sort_by(|a, b| (a.z, a.x, a.y).cmp(&(b.z, b.x, b.y)));
    all_affected.dedup();

    if all_affected.is_empty() {
        info!("No affected tiles for source '{}'", source.name);
        return Ok(());
    }

    info!(
        "Regenerating {} unique tiles for source '{}'",
        all_affected.len(),
        source.name
    );

    // Regenerate tiles concurrently (bounded concurrency)
    let worker_count = config.updates.worker_concurrency.unwrap_or(8).max(1);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(worker_count));
    let mut handles = Vec::new();

    for tile_coord in all_affected.clone() {
        let sem = semaphore.clone();
        let src = source.clone();
        let rdr = reader.clone();

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            regenerate_single_tile(&src, &rdr, &tile_coord).await
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
                .publish_mbtiles(&source.mbtiles_path, "incremental-update")
                .await?;
        }
    }

    info!(
        "Batch update complete for source '{}' ({} tiles)",
        source.name,
        all_affected.len()
    );

    // Emit event for webhooks and SSE
    if let Some(event_tx) = event_tx {
        let zooms: std::collections::HashSet<u8> = all_affected.iter().map(|t| t.z).collect();
        let layers: Vec<String> = events
            .iter()
            .map(|e| e.layer_name.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let _ = event_tx.send(crate::events::TileEvent::update_complete(
            source.name.clone(),
            all_affected.len(),
            &zooms,
            source.max_zoom,
            layers,
        ));
    }

    Ok(())
}

async fn regenerate_single_tile(
    source: &crate::config::SourceConfig,
    reader: &PostgisReader,
    tile_coord: &TileCoord,
) -> Result<Option<Vec<u8>>> {
    let bounds = tile_coord.bounds();
    let mut features_by_layer: HashMap<String, Vec<crate::postgis::FeatureData>> = HashMap::new();
    let mut layer_configs: HashMap<String, &crate::config::LayerConfig> = HashMap::new();

    for layer in &source.layers {
        // Original polygon/geometry layer
        let features = reader.get_features_in_bounds(layer, &bounds).await?;
        if !features.is_empty() {
            features_by_layer.insert(layer.name.clone(), features);
        }
        layer_configs.insert(layer.name.clone(), layer);

        // Derived label points layer (centroid of polygons)
        if layer.generate_label_points {
            let label_name = format!("{}_labels", layer.name);
            let label_features = reader
                .get_derived_features_in_bounds(layer, DerivedGeomType::LabelPoint, &bounds)
                .await?;
            if !label_features.is_empty() {
                features_by_layer.insert(label_name, label_features);
            }
        }

        // Derived boundary lines layer (polygon outline as linestring)
        if layer.generate_boundary_lines {
            let boundary_name = format!("{}_boundary", layer.name);
            let boundary_features = reader
                .get_derived_features_in_bounds(layer, DerivedGeomType::BoundaryLine, &bounds)
                .await?;
            if !boundary_features.is_empty() {
                features_by_layer.insert(boundary_name, boundary_features);
            }
        }
    }

    if features_by_layer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(mvt::encode_tile_with_config(
            tile_coord,
            &features_by_layer,
            &layer_configs,
        )?))
    }
}
