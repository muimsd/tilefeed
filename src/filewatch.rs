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
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::config::{AppConfig, SourceConfig};
use crate::events::TileEvent;
use crate::mbtiles::MbtilesStore;
use crate::postgis::PostgisReader;
use crate::updater::UpdateOutputs;

/// What a watched path looks like: per file, its name, size and mtime. A
/// directory contributes each file directly inside it (a shapefile's .shp,
/// .dbf, .shx…), a file its SQLite `-wal`/`-journal` too (a GeoPackage edited
/// in WAL mode only touches its `-wal` until a checkpoint), a missing path
/// nothing.
type Signature = Vec<(String, u64, Option<SystemTime>)>;

/// Each watched source's signature, taken before startup generation so a
/// change made while it runs is still noticed.
pub struct Baseline(HashMap<String, Signature>);

impl Baseline {
    pub fn capture(config: &AppConfig) -> Self {
        Self(
            config
                .sources
                .iter()
                .filter(|s| !s.watch_files.is_empty())
                .map(|s| (s.name.clone(), signature(&s.watch_files)))
                .collect(),
        )
    }
}

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
            let path = path.to_string_lossy();
            add(Path::new(&format!("{path}-wal")));
            add(Path::new(&format!("{path}-journal")));
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

/// Polls a file may keep changing before the source is regenerated anyway,
/// so a file written to constantly (an open editor's `-wal`) can't keep its
/// tiles stale forever.
const MAX_UNSETTLED_POLLS: u32 = 12;
/// Waits before retrying a failed regeneration: doubling from the first up
/// to the last, until one succeeds or the files change again.
const FIRST_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(15 * 60);

/// Poll state for one source.
struct Watch {
    last: Signature,
    /// Consecutive polls that saw a change. The source is regenerated once
    /// the files stay the same for a whole poll (a copy may still be
    /// writing), or after `MAX_UNSETTLED_POLLS`.
    unsettled: Option<u32>,
    /// After a failed regeneration: when to try again, and the wait used.
    retry: Option<(Instant, Duration)>,
}

impl Watch {
    fn new(last: Signature, pending: bool) -> Self {
        Self {
            last,
            unsettled: pending.then_some(0),
            retry: None,
        }
    }

    /// Feed one poll; true when it is time to regenerate.
    fn poll(&mut self, sig: Signature, now: Instant) -> bool {
        if sig != self.last {
            self.last = sig;
            let polls = self.unsettled.map_or(1, |n| n + 1);
            if polls >= MAX_UNSETTLED_POLLS {
                self.unsettled = None;
                return true;
            }
            self.unsettled = Some(polls);
            return false;
        }
        if self.unsettled.take().is_some() {
            return true;
        }
        self.retry.is_some_and(|(at, _)| now >= at)
    }

    fn succeeded(&mut self) {
        self.retry = None;
    }

    fn failed(&mut self, now: Instant) -> Duration {
        let wait = self
            .retry
            .map_or(FIRST_RETRY, |(_, w)| (w * 2).min(MAX_RETRY));
        self.retry = Some((now + wait, wait));
        wait
    }
}

/// Watch every source with `watch_files` until the task is aborted, from
/// `baseline` (taken before any startup generation). `None` when no source
/// watches files.
pub fn spawn(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    outputs: UpdateOutputs,
    baseline: Baseline,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    if !config.watches_files() {
        return Ok(None);
    }
    // Lazy: the files are watched even while PostgreSQL is down, and a
    // regeneration that fails for it is retried with backoff.
    let reader = PostgisReader::lazy(&config.database)?;
    let interval = config.updates.file_poll_interval();

    let mut watches: Vec<(SourceConfig, Watch)> = Vec::new();
    for source in config.sources.iter().filter(|s| !s.watch_files.is_empty()) {
        let current = signature(&source.watch_files);
        let last = baseline
            .0
            .get(&source.name)
            .cloned()
            .unwrap_or(current.clone());
        if current.is_empty() {
            warn!(
                "Source '{}': none of watch_files exist yet ({})",
                source.name,
                source.watch_files.join(", ")
            );
        }
        // Changed while tilefeed was stopped (--skip-generate) or while
        // startup generation ran: regenerate on the first polls.
        let pending = last != current || changed_since(&current, &source.mbtiles_path);
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
        watches.push((source.clone(), Watch::new(last, pending)));
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
                if !watch.poll(sig, Instant::now()) {
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
                crate::generator::record_generation(&source.name, started, &result);
                match result {
                    Ok(()) => {
                        watch.succeeded();
                        finish(&config, source, store, &outputs, started).await;
                    }
                    // The old tiles stay until a retry or a new change succeeds.
                    Err(e) => {
                        let wait = watch.failed(Instant::now());
                        error!(
                            "Source '{}': regeneration failed, retrying in {}s: {:#}",
                            source.name,
                            wait.as_secs(),
                            e
                        );
                    }
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
        // Copying a large file takes a while; keep it off the async workers
        // (tile reads wait on this store's lock either way).
        break blocking(|| store.replace_contents_from(&scratch))
            .with_context(|| format!("Failed to swap in {}", scratch));
    };

    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{scratch}{suffix}"));
    }
    result
}

/// Run blocking work in place, letting the runtime move other tasks off
/// this worker when it can (a multi-threaded runtime, as the binary uses).
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Tell everyone the source was rebuilt, as a full generation does.
async fn finish(
    config: &AppConfig,
    source: &SourceConfig,
    store: &Mutex<MbtilesStore>,
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
            // The swap committed through a WAL connection: fold it into the
            // file first, or a copy of the .mbtiles alone would miss it.
            if let Err(e) = store.lock().await.checkpoint() {
                warn!("Source '{}': WAL checkpoint failed: {:#}", source.name, e);
            }
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

    fn sig(n: u64) -> Signature {
        vec![("a".to_string(), n, None)]
    }

    #[test]
    fn test_poll_waits_for_the_files_to_settle() {
        let now = Instant::now();
        let mut watch = Watch::new(sig(1), false);
        assert!(!watch.poll(sig(1), now), "unchanged");
        assert!(!watch.poll(sig(2), now), "changed: wait a poll");
        assert!(!watch.poll(sig(3), now), "still changing: keep waiting");
        assert!(watch.poll(sig(3), now), "settled: regenerate");
        assert!(!watch.poll(sig(3), now), "once");
    }

    #[test]
    fn test_poll_regenerates_a_file_that_never_settles() {
        let now = Instant::now();
        let mut watch = Watch::new(sig(0), false);
        let fired: Vec<bool> = (1..=MAX_UNSETTLED_POLLS as u64)
            .map(|n| watch.poll(sig(n), now))
            .collect();
        assert_eq!(fired.iter().filter(|f| **f).count(), 1);
        assert!(fired[MAX_UNSETTLED_POLLS as usize - 1]);
    }

    #[test]
    fn test_pending_at_start_regenerates_on_the_first_quiet_poll() {
        let mut watch = Watch::new(sig(1), true);
        assert!(watch.poll(sig(1), Instant::now()));
    }

    #[test]
    fn test_failed_regeneration_is_retried_with_backoff() {
        let start = Instant::now();
        let mut watch = Watch::new(sig(1), false);
        assert_eq!(watch.failed(start), FIRST_RETRY);
        assert!(!watch.poll(sig(1), start), "not yet");
        let due = start + FIRST_RETRY;
        assert!(watch.poll(sig(1), due), "retry due");
        assert_eq!(watch.failed(due), FIRST_RETRY * 2, "doubles");
        let mut wait = FIRST_RETRY * 2;
        for _ in 0..10 {
            wait = watch.failed(due);
        }
        assert_eq!(wait, MAX_RETRY, "capped");
        watch.succeeded();
        assert!(
            !watch.poll(sig(1), due + MAX_RETRY * 2),
            "no retry after success"
        );
    }

    #[test]
    fn test_signature_covers_a_files_sqlite_sidecars() {
        let dir = temp_dir("wal");
        let gpkg = dir.join("p.gpkg");
        std::fs::write(&gpkg, b"db").unwrap();
        let paths = [gpkg.to_string_lossy().into_owned()];
        let before = signature(&paths);
        std::fs::write(dir.join("p.gpkg-wal"), b"edit").unwrap();
        assert_ne!(signature(&paths), before, "a WAL-mode edit");
        // -shm changes on every read; it must not count
        let with_wal = signature(&paths);
        std::fs::write(dir.join("p.gpkg-shm"), b"x").unwrap();
        assert_eq!(signature(&paths), with_wal);
        let _ = std::fs::remove_dir_all(&dir);
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
