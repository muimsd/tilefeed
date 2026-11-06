use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_postgres::AsyncMessage;
use tracing::{error, info, warn};

use crate::cache::TileCache;
use crate::config::AppConfig;
use crate::events::EventSender;
use crate::mbtiles::MbtilesStore;
use crate::metrics::metrics;
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

/// Where an applied update is reported: publishing, webhooks/SSE, tile cache.
#[derive(Clone, Default)]
pub struct UpdateOutputs {
    pub publisher: Option<Arc<StoragePublisher>>,
    pub event_tx: Option<EventSender>,
    pub cache: Option<Arc<TileCache>>,
}

/// Start the LISTEN/NOTIFY listener for incremental tile updates with auto-reconnect
pub async fn start_listener(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    outputs: UpdateOutputs,
) -> Result<()> {
    let mut retry_count: u32 = 0;
    let max_retry_delay_secs: u64 = 60;

    loop {
        // The listener only stops when the connection is gone. A real shutdown
        // aborts this task instead, so neither outcome here means "we are done":
        // returning would end the process, taking the HTTP tile server with it.
        let reason = match run_listener(&config, &stores, &outputs).await {
            Ok(()) => "listener stopped without an error".to_string(),
            Err(e) => format!("{:#}", e),
        };

        retry_count += 1;
        metrics().listener_reconnects.get().inc();
        let delay_secs = (2u64.pow(retry_count.min(6))).min(max_retry_delay_secs);
        error!(
            "Listener connection lost: {}. Reconnecting in {}s (attempt {})...",
            reason, delay_secs, retry_count
        );
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
    }
}

/// Collect a debounced batch of notifications.
///
/// Returns `Err` when the channel closes, which only happens when the PostgreSQL
/// connection task has exited — the caller must reconnect rather than treat it as
/// a clean stop.
async fn next_batch<T>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>,
    debounce_ms: u64,
) -> Result<Vec<T>> {
    let first = rx
        .recv()
        .await
        .context("PostgreSQL notification channel closed")?;
    let mut batch = vec![first];

    let deadline = Instant::now() + Duration::from_millis(debounce_ms);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(n)) => batch.push(n),
            // Timed out, or the channel closed mid-window: hand back what we have
            // and let the next call surface the closure.
            _ => break,
        }
    }

    Ok(batch)
}

async fn run_listener(
    config: &AppConfig,
    stores: &HashMap<String, Arc<Mutex<MbtilesStore>>>,
    outputs: &UpdateOutputs,
) -> Result<()> {
    let tls = crate::pgtls::make_tls_connector(&config.database)?;
    let (client, mut connection) = config
        .database
        .pg_config()
        .connect(tls)
        .await
        .with_context(|| {
            format!(
                "Failed to connect for LISTEN/NOTIFY (sslmode={})",
                config.database.sslmode.as_str()
            )
        })?;

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
        let events = next_batch(&mut rx, debounce_ms).await?;

        info!("Processing batch of {} notification(s)", events.len());

        let mut parsed_events = Vec::new();
        for notification in &events {
            let payload = notification.payload();
            match parse_notification(payload) {
                Ok(event) => parsed_events.push(event),
                Err(e) => {
                    metrics().notifications.with(&["invalid_payload"]).inc();
                    warn!("Invalid notification payload '{}': {}", payload, e);
                }
            }
        }

        if !parsed_events.is_empty() {
            if let Err(e) =
                handle_batch_update(config, &reader, stores, outputs, &parsed_events).await
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

        // The trigger sends nulls when the old geometry was NULL. Anything but
        // four numbers forming a box is no old location at all — defaulting
        // the gaps to 0 would regenerate (and announce) tiles at 0,0.
        let old_bounds = json.get("old_bounds").and_then(|ob| {
            let field = |k: &str| ob[k].as_f64().filter(|v| v.is_finite());
            let bounds = Bounds {
                min_lon: field("min_lon")?,
                min_lat: field("min_lat")?,
                max_lon: field("max_lon")?,
                max_lat: field("max_lat")?,
            };
            (bounds.min_lon <= bounds.max_lon && bounds.min_lat <= bounds.max_lat).then_some(bounds)
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

    // --- next_batch tests ---

    #[tokio::test]
    async fn test_next_batch_errors_when_channel_closes() {
        // A closed channel means the PostgreSQL connection task died. Reporting
        // that as a clean stop ends `start_listener`, which in `serve` ends the
        // process and takes the HTTP tile server down with it.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        drop(tx);

        let result = next_batch(&mut rx, 10).await;
        assert!(
            result.is_err(),
            "a closed channel must not look like a clean stop"
        );
    }

    #[tokio::test]
    async fn test_next_batch_collects_within_debounce_window() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tx.send("a".to_string()).unwrap();
        tx.send("b".to_string()).unwrap();
        tx.send("c".to_string()).unwrap();

        let batch = next_batch(&mut rx, 50).await.unwrap();
        assert_eq!(batch, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn test_next_batch_returns_pending_items_when_channel_closes_mid_window() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tx.send("only".to_string()).unwrap();
        drop(tx);

        // What arrived before the closure is still processed...
        let batch = next_batch(&mut rx, 50).await.unwrap();
        assert_eq!(batch, vec!["only"]);
        // ...and the closure surfaces on the next call
        assert!(next_batch(&mut rx, 50).await.is_err());
    }

    // --- per-layer zoom range ---

    fn zoom_source() -> crate::config::SourceConfig {
        let cfg: crate::config::SourceConfig = toml_source(
            r#"
            name = "s"
            mbtiles_path = "/tmp/s.mbtiles"
            min_zoom = 0
            max_zoom = 14

            [[layers]]
            name = "everywhere"
            table = "a"

            [[layers]]
            name = "detail"
            table = "b"
            min_zoom = 10
            generate_label_points = true
            "#,
        );
        cfg.validate_layer_zooms().unwrap();
        cfg
    }

    fn toml_source(src: &str) -> crate::config::SourceConfig {
        config::Config::builder()
            .add_source(config::File::from_str(src, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    fn small_bounds() -> Bounds {
        Bounds {
            min_lon: 10.0,
            min_lat: 50.0,
            max_lon: 10.001,
            max_lat: 50.001,
        }
    }

    #[test]
    fn test_affected_tiles_stay_within_layer_zoom_range() {
        let source = zoom_source();
        let detail = source.find_layer("detail").unwrap();

        let tiles = affected_tiles(&source, detail, &small_bounds());
        let zooms: std::collections::BTreeSet<u8> = tiles.iter().map(|t| t.z).collect();
        assert_eq!(zooms, (10..=14).collect());
    }

    #[test]
    fn test_affected_tiles_without_layer_range_cover_source_range() {
        let source = zoom_source();
        let everywhere = source.find_layer("everywhere").unwrap();

        let tiles = affected_tiles(&source, everywhere, &small_bounds());
        let zooms: std::collections::BTreeSet<u8> = tiles.iter().map(|t| t.z).collect();
        assert_eq!(zooms, (0..=14).collect());
    }

    #[test]
    fn test_regenerated_tiles_only_encode_layers_in_range() {
        // regenerate_single_tile queries and encodes exactly `tile_layers_at_zoom(z)`,
        // so a tile below a layer's min_zoom never gains it or its derived layers.
        let source = zoom_source();
        let names = |z: u8| -> Vec<String> {
            source
                .tile_layers_at_zoom(z)
                .into_iter()
                .map(|tl| tl.name)
                .collect()
        };
        assert_eq!(names(9), vec!["everywhere"]);
        assert_eq!(names(10), vec!["everywhere", "detail", "detail_labels"]);
        assert_eq!(names(14), vec!["everywhere", "detail", "detail_labels"]);
    }

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
        // A partial or null box (old geometry was NULL) is no old location,
        // not a box at 0,0.
        for payload in [
            r#"{"layer": "test", "id": 1, "old_bounds": {"min_lon": 5.0}}"#,
            r#"{"layer": "test", "id": 1, "old_bounds": {"min_lon": null, "min_lat": null, "max_lon": null, "max_lat": null}}"#,
            r#"{"layer": "test", "id": 1, "old_bounds": {"min_lon": 5, "min_lat": 0, "max_lon": 1, "max_lat": 1}}"#,
        ] {
            assert!(
                parse_notification(payload).unwrap().old_bounds.is_none(),
                "{payload}"
            );
        }
    }
}

/// Handle a batch of update events: group by source, deduplicate affected tiles, regenerate
async fn handle_batch_update(
    config: &AppConfig,
    reader: &PostgisReader,
    stores: &HashMap<String, Arc<Mutex<MbtilesStore>>>,
    outputs: &UpdateOutputs,
    events: &[UpdateEvent],
) -> Result<()> {
    // Group events by source name
    let mut events_by_source: HashMap<String, Vec<&UpdateEvent>> = HashMap::new();

    for event in events {
        match config.find_source_for_layer(&event.layer_name) {
            Some(source) => {
                metrics().notifications.with(&["routed"]).inc();
                events_by_source
                    .entry(source.name.clone())
                    .or_default()
                    .push(event);
            }
            None => {
                metrics().notifications.with(&["unknown_layer"]).inc();
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

        let started = std::time::Instant::now();
        let result = update_source(config, reader, source, store, outputs, source_events).await;

        metrics()
            .update_duration
            .with(&[source_name])
            .observe_since(started);
        metrics().update_batches.with(&[source_name]).inc();
        if result.is_err() {
            metrics().update_errors.with(&[source_name]).inc();
        }
        result?;
    }

    Ok(())
}

/// Update a single source's MBTiles with affected tiles
async fn update_source(
    config: &AppConfig,
    reader: &PostgisReader,
    source: &crate::config::SourceConfig,
    mbtiles: &Arc<Mutex<MbtilesStore>>,
    outputs: &UpdateOutputs,
    events: &[&UpdateEvent],
) -> Result<()> {
    let publisher = outputs.publisher.as_ref();
    let event_tx = outputs.event_tx.as_ref();
    let cache = outputs.cache.as_ref();
    let mut all_affected: Vec<TileCoord> = Vec::new();
    let mut areas: Vec<crate::events::ChangedArea> = Vec::new();

    for event in events {
        let layer = match source.find_layer(&event.layer_name) {
            Some(l) => l,
            None => continue,
        };

        let (min_zoom, max_zoom) = source.layer_zoom_range(layer);
        let mut record = |bounds: &Bounds| {
            all_affected.extend(affected_tiles(source, layer, bounds));
            areas.push(crate::events::ChangedArea {
                bbox: [
                    bounds.min_lon,
                    bounds.min_lat,
                    bounds.max_lon,
                    bounds.max_lat,
                ],
                min_zoom,
                max_zoom,
            });
        };

        if let Some(feat) = reader.get_feature(layer, event.feature_id).await? {
            record(&feat.bounds);
        }

        if let Some(ref old_bounds) = event.old_bounds {
            record(old_bounds);
        }
    }

    // Deduplicate
    all_affected.sort_by_key(|t| (t.z, t.x, t.y));
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
            Ok(Err(e)) => {
                metrics().tile_encode_errors.with(&[&source.name]).inc();
                error!("Failed to regenerate tile {:?}: {}", coord, e);
            }
            Err(e) => {
                metrics().tile_encode_errors.with(&[&source.name]).inc();
                error!("Task panicked for tile {:?}: {}", coord, e);
            }
        }
    }

    let written = encoded.iter().filter(|(_, d)| d.is_some()).count();
    let deleted = encoded.len() - written;

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

    // Clear the old versions of exactly these tiles from the server's cache.
    // After the commit, so a request that misses reads the new tile; the
    // cache's generation check drops any read that started before it.
    if let Some(cache) = cache {
        let cleared = cache.invalidate(&source.name, encoded.iter().map(|(c, _)| (c.z, c.x, c.y)));
        if cleared > 0 {
            info!(
                "Cleared {} cached tile(s) for source '{}'",
                cleared, source.name
            );
        }
    }

    metrics()
        .tiles_written
        .with(&[&source.name])
        .add(written as u64);
    metrics()
        .tiles_deleted
        .with(&[&source.name])
        .add(deleted as u64);

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
        let _ = event_tx.send(
            crate::events::TileEvent::update_complete(
                source.name.clone(),
                all_affected.len(),
                &zooms,
                source.max_zoom,
                layers,
            )
            .with_areas(areas),
        );
    }

    Ok(())
}

/// Tiles a change to `layer` within `bounds` can alter: only those at the zooms
/// the layer (and its derived layers) is encoded at.
fn affected_tiles(
    source: &crate::config::SourceConfig,
    layer: &crate::config::LayerConfig,
    bounds: &Bounds,
) -> Vec<TileCoord> {
    let (min_zoom, max_zoom) = source.layer_zoom_range(layer);
    tiles_for_bounds(bounds, min_zoom, max_zoom)
}

async fn regenerate_single_tile(
    source: &crate::config::SourceConfig,
    reader: &PostgisReader,
    tile_coord: &TileCoord,
) -> Result<Option<Vec<u8>>> {
    let bounds = tile_coord.bounds();
    let mut features_by_layer: HashMap<String, Vec<crate::postgis::FeatureData>> = HashMap::new();
    let mut layer_configs: HashMap<String, &crate::config::LayerConfig> = HashMap::new();

    // Only layers whose zoom range covers this tile; the rest stay out of it.
    for tl in source.tile_layers_at_zoom(tile_coord.z) {
        let features = match tl.derived {
            None => {
                layer_configs.insert(tl.layer.name.clone(), tl.layer);
                reader.get_features_in_bounds(tl.layer, &bounds).await?
            }
            Some(derived) => {
                reader
                    .get_derived_features_in_bounds(tl.layer, derived, &bounds)
                    .await?
            }
        };
        if !features.is_empty() {
            features_by_layer.insert(tl.name, features);
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
