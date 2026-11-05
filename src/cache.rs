//! In-memory tile cache for the HTTP server.
//!
//! Tiles are cached per `(source, z, x, y)` with their precomputed ETag, so a
//! hit costs neither a SQLite read nor a SHA-256. Empty tiles (served as 204)
//! are cached too: most requests for a sparse source hit tiles that do not exist.
//!
//! The cache is bounded by total bytes and evicts least-recently-used tiles.
//! Old tiles are cleared as the data changes:
//! - the incremental updater calls [`TileCache::invalidate`] with exactly the
//!   tiles it rewrote or deleted, right after its MBTiles commit;
//! - a full generation clears the whole source ([`TileCache::clear_source`]);
//! - an optional TTL expires entries regardless, as a backstop for writes made
//!   outside this process.
//!
//! Every invalidation bumps a per-source generation counter. A request that read
//! the store before an invalidation carries the old generation and its insert is
//! dropped, so a tile read just before an update cannot be cached after it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;

use crate::metrics::metrics;

/// Rough per-entry overhead (key, maps, ETag) counted toward the byte budget,
/// so a cache full of empty tiles is still bounded.
const ENTRY_OVERHEAD: usize = 96;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    source: String,
    z: u8,
    x: u32,
    y: u32,
}

/// A cached tile response: the stored bytes and their ETag, or an empty tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachedTile {
    Empty,
    Tile { data: Bytes, etag: String },
}

impl CachedTile {
    fn size(&self) -> usize {
        ENTRY_OVERHEAD
            + match self {
                CachedTile::Empty => 0,
                CachedTile::Tile { data, etag } => data.len() + etag.len(),
            }
    }
}

struct Entry {
    tile: CachedTile,
    size: usize,
    inserted: Instant,
    tick: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Key, Entry>,
    /// Access order: tick → key. The first entry is the least recently used.
    order: BTreeMap<u64, Key>,
    next_tick: u64,
    bytes: usize,
    generations: HashMap<String, u64>,
}

impl Inner {
    fn touch(&mut self, key: &Key) {
        let tick = self.next_tick;
        self.next_tick += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            self.order.remove(&entry.tick);
            entry.tick = tick;
            self.order.insert(tick, key.clone());
        }
    }

    fn remove(&mut self, key: &Key) -> bool {
        match self.entries.remove(key) {
            Some(entry) => {
                self.order.remove(&entry.tick);
                self.bytes -= entry.size;
                true
            }
            None => false,
        }
    }

    fn bump_generation(&mut self, source: &str) {
        *self.generations.entry(source.to_string()).or_insert(0) += 1;
    }
}

pub struct TileCache {
    inner: Mutex<Inner>,
    max_bytes: usize,
    ttl: Option<Duration>,
}

impl TileCache {
    pub fn new(max_bytes: usize, ttl: Option<Duration>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_bytes,
            ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves at worst a stale accounting
        // entry; serving must not stop over it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The source's current generation. Read it *before* reading the store and
    /// pass it to [`TileCache::insert`].
    pub fn generation(&self, source: &str) -> u64 {
        self.lock().generations.get(source).copied().unwrap_or(0)
    }

    pub fn get(&self, source: &str, z: u8, x: u32, y: u32) -> Option<CachedTile> {
        let key = Key {
            source: source.to_string(),
            z,
            x,
            y,
        };
        let mut inner = self.lock();
        let expired = match inner.entries.get(&key) {
            None => {
                drop(inner);
                metrics().tile_cache_requests.with(&[source, "miss"]).inc();
                return None;
            }
            Some(entry) => self.ttl.is_some_and(|ttl| entry.inserted.elapsed() >= ttl),
        };
        if expired {
            inner.remove(&key);
            metrics().tile_cache_bytes.get().set(inner.bytes as i64);
            drop(inner);
            metrics()
                .tile_cache_requests
                .with(&[source, "expired"])
                .inc();
            return None;
        }
        inner.touch(&key);
        let tile = inner.entries.get(&key).map(|e| e.tile.clone());
        drop(inner);
        metrics().tile_cache_requests.with(&[source, "hit"]).inc();
        tile
    }

    /// Cache a tile read from the store at `generation`. Dropped if the source
    /// was invalidated since (the read may predate the write), or if the tile
    /// alone exceeds the budget. Returns whether it was stored.
    pub fn insert(
        &self,
        source: &str,
        z: u8,
        x: u32,
        y: u32,
        tile: CachedTile,
        generation: u64,
    ) -> bool {
        let size = tile.size();
        if size > self.max_bytes {
            return false;
        }
        let key = Key {
            source: source.to_string(),
            z,
            x,
            y,
        };
        let mut inner = self.lock();
        if inner.generations.get(source).copied().unwrap_or(0) != generation {
            return false;
        }
        inner.remove(&key);

        let mut evicted = 0u64;
        while inner.bytes + size > self.max_bytes {
            let Some((_, lru)) = inner.order.pop_first() else {
                break;
            };
            if let Some(entry) = inner.entries.remove(&lru) {
                inner.bytes -= entry.size;
                evicted += 1;
            }
        }

        let tick = inner.next_tick;
        inner.next_tick += 1;
        inner.order.insert(tick, key.clone());
        inner.entries.insert(
            key,
            Entry {
                tile,
                size,
                inserted: Instant::now(),
                tick,
            },
        );
        inner.bytes += size;
        let bytes = inner.bytes;
        drop(inner);

        let m = metrics();
        m.tile_cache_bytes.get().set(bytes as i64);
        if evicted > 0 {
            m.tile_cache_evictions.get().add(evicted);
        }
        true
    }

    /// Drop the given tiles of a source (XYZ coordinates). Call after the new
    /// tiles are committed to the store. Returns how many were cached.
    pub fn invalidate<I>(&self, source: &str, coords: I) -> usize
    where
        I: IntoIterator<Item = (u8, u32, u32)>,
    {
        let mut inner = self.lock();
        inner.bump_generation(source);
        let mut removed = 0;
        for (z, x, y) in coords {
            let key = Key {
                source: source.to_string(),
                z,
                x,
                y,
            };
            if inner.remove(&key) {
                removed += 1;
            }
        }
        let bytes = inner.bytes;
        drop(inner);
        let m = metrics();
        m.tile_cache_bytes.get().set(bytes as i64);
        m.tile_cache_invalidations
            .with(&[source])
            .add(removed as u64);
        removed
    }

    /// Drop every cached tile of a source, e.g. after a full generation.
    pub fn clear_source(&self, source: &str) -> usize {
        let mut inner = self.lock();
        inner.bump_generation(source);
        let keys: Vec<Key> = inner
            .entries
            .keys()
            .filter(|k| k.source == source)
            .cloned()
            .collect();
        for key in &keys {
            inner.remove(key);
        }
        let bytes = inner.bytes;
        drop(inner);
        let m = metrics();
        m.tile_cache_bytes.get().set(bytes as i64);
        m.tile_cache_invalidations
            .with(&[source])
            .add(keys.len() as u64);
        keys.len()
    }

    /// Sources that currently have cached tiles.
    pub fn sources(&self) -> Vec<String> {
        let inner = self.lock();
        let mut sources: Vec<String> = inner.entries.keys().map(|k| k.source.clone()).collect();
        sources.sort();
        sources.dedup();
        sources
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(n: usize) -> CachedTile {
        CachedTile::Tile {
            data: Bytes::from(vec![7u8; n]),
            etag: "\"e\"".to_string(),
        }
    }

    #[test]
    fn test_get_returns_inserted_tile() {
        let cache = TileCache::new(1 << 20, None);
        assert_eq!(cache.get("s", 1, 0, 0), None);
        assert!(cache.insert("s", 1, 0, 0, tile(10), cache.generation("s")));
        assert_eq!(cache.get("s", 1, 0, 0), Some(tile(10)));
        assert_eq!(cache.get("other", 1, 0, 0), None);
        assert_eq!(cache.get("s", 1, 0, 1), None);
    }

    #[test]
    fn test_empty_tiles_are_cached() {
        let cache = TileCache::new(1 << 20, None);
        cache.insert("s", 3, 1, 2, CachedTile::Empty, 0);
        assert_eq!(cache.get("s", 3, 1, 2), Some(CachedTile::Empty));
    }

    #[test]
    fn test_evicts_least_recently_used_within_budget() {
        let each = tile(100).size();
        let cache = TileCache::new(each * 2, None);
        cache.insert("s", 0, 0, 0, tile(100), 0);
        cache.insert("s", 1, 0, 0, tile(100), 0);
        // Touch the first so the second is now least recently used.
        assert!(cache.get("s", 0, 0, 0).is_some());
        cache.insert("s", 2, 0, 0, tile(100), 0);
        assert!(cache.get("s", 0, 0, 0).is_some());
        assert_eq!(cache.get("s", 1, 0, 0), None);
        assert!(cache.get("s", 2, 0, 0).is_some());
        assert!(cache.bytes() <= each * 2);
    }

    #[test]
    fn test_tile_larger_than_budget_is_not_cached() {
        let cache = TileCache::new(50, None);
        assert!(!cache.insert("s", 0, 0, 0, tile(1000), 0));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_reinsert_replaces_without_leaking_bytes() {
        let cache = TileCache::new(1 << 20, None);
        cache.insert("s", 0, 0, 0, tile(100), 0);
        cache.insert("s", 0, 0, 0, tile(10), 0);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.bytes(), tile(10).size());
    }

    #[test]
    fn test_invalidate_clears_only_given_tiles() {
        let cache = TileCache::new(1 << 20, None);
        cache.insert("s", 5, 1, 1, tile(10), 0);
        cache.insert("s", 5, 1, 2, tile(10), 0);
        cache.insert("t", 5, 1, 1, tile(10), 0);
        assert_eq!(cache.invalidate("s", [(5, 1, 1), (5, 9, 9)]), 1);
        assert_eq!(cache.get("s", 5, 1, 1), None);
        assert!(cache.get("s", 5, 1, 2).is_some());
        assert!(cache.get("t", 5, 1, 1).is_some());
        assert_eq!(cache.bytes(), 2 * tile(10).size());
    }

    #[test]
    fn test_read_before_invalidation_is_not_cached() {
        // A request reads the old tile, the updater commits and invalidates,
        // then the request tries to cache what it read: that must be refused.
        let cache = TileCache::new(1 << 20, None);
        let gen_before_read = cache.generation("s");
        cache.invalidate("s", [(4, 2, 2)]);
        assert!(!cache.insert("s", 4, 2, 2, tile(10), gen_before_read));
        assert_eq!(cache.get("s", 4, 2, 2), None);
        // A fresh read after the invalidation caches normally.
        assert!(cache.insert("s", 4, 2, 2, tile(10), cache.generation("s")));
    }

    #[test]
    fn test_invalidation_does_not_block_other_sources() {
        let cache = TileCache::new(1 << 20, None);
        let gen_t = cache.generation("t");
        cache.invalidate("s", [(0, 0, 0)]);
        assert!(cache.insert("t", 0, 0, 0, tile(10), gen_t));
    }

    #[test]
    fn test_clear_source() {
        let cache = TileCache::new(1 << 20, None);
        cache.insert("s", 0, 0, 0, tile(10), 0);
        cache.insert("s", 1, 1, 1, CachedTile::Empty, 0);
        cache.insert("t", 0, 0, 0, tile(10), 0);
        assert_eq!(cache.clear_source("s"), 2);
        assert_eq!(cache.len(), 1);
        assert!(cache.get("t", 0, 0, 0).is_some());
    }

    #[test]
    fn test_ttl_expires_entries() {
        let cache = TileCache::new(1 << 20, Some(Duration::from_millis(20)));
        cache.insert("s", 0, 0, 0, tile(10), 0);
        assert!(cache.get("s", 0, 0, 0).is_some());
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(cache.get("s", 0, 0, 0), None);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.bytes(), 0);
    }
}
