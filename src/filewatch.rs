//! Regenerating sources when the files their data comes from change.
//!
//! Data read through OGR_FDW (a GeoPackage, a shapefile, …) changes without
//! PostgreSQL knowing, so no trigger fires. A source lists those files in
//! `watch_files`; they are polled (which, unlike filesystem events, also works
//! on network shares and Docker bind mounts), and once a change has settled
//! the source is generated again into a scratch file and swapped in.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::{AppConfig, SourceConfig};
use crate::events::TileEvent;
use crate::mbtiles::MbtilesStore;
use crate::metrics::metrics;
use crate::postgis::PostgisReader;
use crate::updater::UpdateOutputs;

/// What a watched path looks like: per file, its name, size and mtime. A
/// directory contributes each file directly inside it (a shapefile's .shp,
/// .dbf, .shx… or a GeoPackage and its -wal), a missing path nothing.
type Signature = Vec<(String, u64, Option<SystemTime>)>;

fn signature(paths: &[String]) -> Signature {
    let mut sig = Signature::new();
    let mut add = |path: &Path| {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.is_file() {
                sig.push((
                    path.to_string_lossy().into_owned(),
                    meta.len(),
                    meta.modified().ok(),
                ));
            }
        }
    };
    for path in paths {
        let path = Path::new(path);
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    add(&entry.path());
                }
            }
        } else {
            add(path);
        }
    }
    sig.sort();
    sig
}

/// Whether a watched file changed after the MBTiles was last written: then
/// the tiles predate the data (e.g. `serve --skip-generate` after an edit made
/// while tilefeed was stopped).
fn changed_since(sig: &Signature, mbtiles_path: &str) -> bool {
    let Ok(written) = std::fs::metadata(mbtiles_path).and_then(|m| m.modified()) else {
        return false;
    };
    sig.iter()
        .any(|(_, _, mtime)| mtime.is_some_and(|m| m > written))
}

/// Poll state for one source.
struct Watch {
    last: Signature,
    /// A change seen on the previous poll, regenerated once the files have
    /// stayed the same for a whole interval (a copy may still be writing).
    pending: bool,
}

impl Watch {
    /// Feed one poll; true when it is time to regenerate.
    fn poll(&mut self, sig: Signature) -> bool {
        if sig != self.last {
            self.last = sig;
            self.pending = true;
            false
        } else {
            std::mem::take(&mut self.pending)
        }
    }
}

/// Watch every source with `watch_files` until the task is aborted. `None`
/// when no source watches files.
pub fn spawn(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    outputs: UpdateOutputs,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    if !config.watches_files() {
        return Ok(None);
    }
    // Lazy: the files are watched even while PostgreSQL is down, and a
    // regeneration that fails for it is retried on the next change.
    let reader = PostgisReader::lazy(&config.database)?;
    let interval = config.updates.file_poll_interval();

    let mut watches: Vec<(SourceConfig, Watch)> = Vec::new();
    for source in config.sources.iter().filter(|s| !s.watch_files.is_empty()) {
        let last = signature(&source.watch_files);
        if last.is_empty() {
            warn!(
                "Source '{}': none of watch_files exist yet ({})",
                source.name,
                source.watch_files.join(", ")
            );
        }
        let pending = changed_since(&last, &source.mbtiles_path);
        if pending {
            info!(
                "Source '{}': watched files changed after its MBTiles was written; regenerating",
                source.name
            );
        }
        info!(
            "Source '{}': watching {} for changes every {}s",
            source.name,
            source.watch_files.join(", "),
            interval.as_secs()
        );
        watches.push((source.clone(), Watch { last, pending }));
    }

    Ok(Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            for (source, watch) in &mut watches {
                let sig = {
                    let paths = source.watch_files.clone();
                    tokio::task::spawn_blocking(move || signature(&paths))
                        .await
                        .unwrap_or_default()
                };
                if !watch.poll(sig) {
                    continue;
                }
                let Some(store) = stores.get(&source.name) else {
                    continue;
                };
                info!(
                    "Source '{}': watched files changed, regenerating",
                    source.name
                );
                let started = std::time::Instant::now();
                let result = regenerate(&config, &reader, source, store).await;
                metrics()
                    .generate_duration
                    .with(&[&source.name])
                    .observe_since(started);
                metrics()
                    .generate_total
                    .with(&[&source.name, crate::metrics::result_label(&result)])
                    .inc();
                match result {
                    Ok(()) => finish(&config, source, &outputs, started).await,
                    // The old tiles stay; the next change tries again.
                    Err(e) => error!("Source '{}': regeneration failed: {:#}", source.name, e),
                }
            }
        }
    })))
}

/// How many times a regeneration is redone because an incremental update
/// committed while it ran (and would be lost by the swap).
const MAX_ATTEMPTS: u32 = 3;

/// Generate `source` into a scratch file next to its MBTiles and swap the
/// result in through the open store.
async fn regenerate(
    config: &AppConfig,
    reader: &PostgisReader,
    source: &SourceConfig,
    store: &Mutex<MbtilesStore>,
) -> Result<()> {
    let scratch = format!("{}.regen", source.mbtiles_path);
    let mut target = source.clone();
    target.mbtiles_path = scratch.clone();

    let mut attempt = 1;
    let result = loop {
        let before = store.lock().await.commit_count();
        if let Err(e) = crate::generator::generate_source(
            &target,
            reader,
            config.tippecanoe_bin.as_deref(),
            config.ogr2ogr_bin.as_deref(),
        )
        .await
        {
            break Err(e);
        }
        let store = store.lock().await;
        if store.commit_count() != before {
            if attempt < MAX_ATTEMPTS {
                attempt += 1;
                info!(
                    "Source '{}': an update landed during regeneration, regenerating again",
                    source.name
                );
                continue;
            }
            warn!(
                "Source '{}': updates kept landing during regeneration; \
                 swapping in the last one, which may miss the most recent",
                source.name
            );
        }
        break store
            .replace_contents_from(&scratch)
            .with_context(|| format!("Failed to swap in {}", scratch));
    };

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{scratch}{suffix}"));
    }
    result
}

/// Tell everyone the source was rebuilt, as a full generation does.
async fn finish(
    config: &AppConfig,
    source: &SourceConfig,
    outputs: &UpdateOutputs,
    started: std::time::Instant,
) {
    if let Some(cache) = &outputs.cache {
        cache.clear_source(&source.name);
    }
    info!(
        "Source '{}': regenerated in {:.1}s",
        source.name,
        started.elapsed().as_secs_f64()
    );
    if let Some(publisher) = &outputs.publisher {
        if config.publish.publish_on_generate_enabled() {
            if let Err(e) = publisher
                .publish_mbtiles(&source.mbtiles_path, "full-generate")
                .await
            {
                error!("Source '{}': publishing failed: {:#}", source.name, e);
            }
        }
    }
    if let Some(event_tx) = &outputs.event_tx {
        let _ = event_tx.send(TileEvent::GenerateComplete {
            source: source.name.clone(),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tilefeed-fw-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_signature_covers_files_and_directory_entries() {
        let dir = temp_dir("sig");
        let shp = dir.join("shp");
        std::fs::create_dir(&shp).unwrap();
        std::fs::write(shp.join("a.shp"), b"1").unwrap();
        std::fs::write(shp.join("a.dbf"), b"22").unwrap();
        let gpkg = dir.join("b.gpkg");
        std::fs::write(&gpkg, b"333").unwrap();
        let missing = dir.join("missing.gpkg");

        let paths = [&shp, &gpkg, &missing].map(|p| p.to_string_lossy().into_owned());
        let sig = signature(&paths);
        let sizes: Vec<u64> = sig.iter().map(|(_, len, _)| *len).collect();
        assert_eq!(sig.len(), 3, "{sig:?}");
        assert_eq!(sizes.iter().sum::<u64>(), 6);

        // A new file inside the directory, or a missing one appearing, changes it
        std::fs::write(shp.join("a.shx"), b"4").unwrap();
        assert_ne!(signature(&paths), sig);
        let sig = signature(&paths);
        std::fs::write(&missing, b"5").unwrap();
        assert_ne!(signature(&paths), sig);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_poll_waits_for_the_files_to_settle() {
        let a = vec![("a".to_string(), 1, None)];
        let b = vec![("a".to_string(), 2, None)];
        let c = vec![("a".to_string(), 3, None)];
        let mut watch = Watch {
            last: a.clone(),
            pending: false,
        };
        assert!(!watch.poll(a.clone()), "unchanged");
        assert!(!watch.poll(b.clone()), "changed: wait a poll");
        assert!(!watch.poll(c.clone()), "still changing: keep waiting");
        assert!(watch.poll(c.clone()), "settled: regenerate");
        assert!(!watch.poll(c), "once");
    }

    #[test]
    fn test_changed_since_compares_with_the_mbtiles_mtime() {
        let dir = temp_dir("since");
        let data = dir.join("data.gpkg");
        std::fs::write(&data, b"x").unwrap();
        let paths = [data.to_string_lossy().into_owned()];
        let sig = signature(&paths);
        let mbtiles = dir.join("out.mbtiles");

        // No MBTiles yet: nothing to compare (startup generation makes it)
        assert!(!changed_since(&sig, mbtiles.to_str().unwrap()));

        let file = std::fs::File::create(&mbtiles).unwrap();
        let data_mtime = sig[0].2.unwrap();
        file.set_modified(data_mtime + std::time::Duration::from_secs(10))
            .unwrap();
        assert!(!changed_since(&sig, mbtiles.to_str().unwrap()));
        file.set_modified(data_mtime - std::time::Duration::from_secs(10))
            .unwrap();
        assert!(changed_since(&sig, mbtiles.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
