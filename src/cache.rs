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
//! - the server watches each MBTiles file for commits made by *other*
//!   processes (a cron `generate`, a separate `watch`) and calls
//!   [`TileCache::clear_source`] when one lands;
//! - an optional TTL expires entries regardless.
//!
//! Every invalidation bumps a per-source generation counter. A request that read
//! the store before an invalidation carries the old generation and its insert is
//! dropped, so a tile read just before an update cannot be cached after it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;

use crate::metrics::metrics;

/// Per-entry memory beyond the tile bytes and ETag: the key in both maps, the
/// entry itself, and map/tree node overhead. Counted toward the byte budget so
/// a cache full of empty tiles is still bounded.
const ENTRY_OVERHEAD: usize = 160;

/// Source names are interned: keys share one `Arc<str>` per source, so neither
/// a lookup nor an entry allocates a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    source: Arc<str>,
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

/// Result of a cache lookup. A miss carries the source's generation, to be
/// passed back to [`TileCache::insert`] after reading the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Hit(CachedTile),
    Miss { generation: u64 },
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
    /// Interned source name → generation.
    sources: HashMap<Arc<str>, u64>,
}

impl Inner {
    fn intern(&mut self, source: &str) -> Arc<str> {
        if let Some((name, _)) = self.sources.get_key_value(source) {
            return name.clone();
        }
        let name: Arc<str> = Arc::from(source);
        self.sources.insert(name.clone(), 0);
        name
    }

    fn key(&mut self, source: &str, z: u8, x: u32, y: u32) -> Key {
        Key {
            source: self.intern(source),
            z,
            x,
            y,
        }
    }

    fn generation(&self, source: &str) -> u64 {
        self.sources.get(source).copied().unwrap_or(0)
    }

    fn bump_generation(&mut self, source: &str) {
        let name = self.intern(source);
        *self.sources.entry(name).or_insert(0) += 1;
    }

    fn next_tick(&mut self) -> u64 {
        let tick = self.next_tick;
        self.next_tick += 1;
        tick
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

    /// Look a tile up. On a miss, read the store and pass the returned
    /// generation to [`TileCache::insert`]; it is taken under the same lock,
    /// before the store read, which is what makes the race check sound.
    pub fn lookup(&self, source: &str, z: u8, x: u32, y: u32) -> Lookup {
        let mut inner = self.lock();
        let key = inner.key(source, z, x, y);
        let (result, expired) = match inner.entries.get(&key) {
            None => ("miss", false),
            Some(entry) if self.ttl.is_some_and(|ttl| entry.inserted.elapsed() >= ttl) => {
                ("expired", true)
            }
            Some(_) => ("hit", false),
        };

        let lookup = if result == "hit" {
            let tick = inner.next_tick();
            let entry = inner.entries.get_mut(&key).expect("entry checked above");
            let old_tick = std::mem::replace(&mut entry.tick, tick);
            let tile = entry.tile.clone();
            inner.order.remove(&old_tick);
            inner.order.insert(tick, key);
            Lookup::Hit(tile)
        } else {
            if expired {
                inner.remove(&key);
            }
            Lookup::Miss {
                generation: inner.generation(source),
            }
        };
        let bytes = inner.bytes;
        drop(inner);

        let m = metrics();
        m.tile_cache_requests.with(&[source, result]).inc();
        if expired {
            m.tile_cache_bytes.get().set(bytes as i64);
        }
        lookup
    }

    /// Cache a tile read from the store after a [`Lookup::Miss`] at
    /// `generation`. Dropped if the source was invalidated since (the read may
    /// predate the write), or if the tile alone exceeds the budget. Returns
    /// whether it was stored.
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
        let mut inner = self.lock();
        if inner.generation(source) != generation {
            return false;
        }
        let key = inner.key(source, z, x, y);
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

        let tick = inner.next_tick();
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
            let key = inner.key(source, z, x, y);
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

    /// Drop the cached tiles of a source inside any of `ranges` (XYZ): for
    /// areas too large to list tile by tile. Returns how many were cached.
    pub fn invalidate_ranges(&self, source: &str, ranges: &[crate::tiles::TileRange]) -> usize {
        let mut inner = self.lock();
        inner.bump_generation(source);
        let keys: Vec<Key> = inner
            .entries
            .keys()
            .filter(|k| &*k.source == source && ranges.iter().any(|r| r.contains(k.z, k.x, k.y)))
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

    /// Drop every cached tile of a source, e.g. after another process rewrote
    /// its MBTiles. Returns how many were cached.
    pub fn clear_source(&self, source: &str) -> usize {
        let mut inner = self.lock();
        inner.bump_generation(source);
        let keys: Vec<Key> = inner
            .entries
            .keys()
            .filter(|k| &*k.source == source)
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

    /// Number of cached entries (for tests outside this module).
    #[cfg(test)]
    pub fn sources_cached(&self) -> usize {
        self.lock().entries.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().entries.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.lock().bytes
    }

    #[cfg(test)]
    fn get(&self, source: &str, z: u8, x: u32, y: u32) -> Option<CachedTile> {
        match self.lookup(source, z, x, y) {
            Lookup::Hit(tile) => Some(tile),
            Lookup::Miss { .. } => None,
        }
    }

    #[cfg(test)]
    fn generation(&self, source: &str) -> u64 {
        self.lock().generation(source)
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
        let Lookup::Miss { generation } = cache.lookup("s", 1, 0, 0) else {
            panic!("expected a miss");
        };
        assert!(cache.insert("s", 1, 0, 0, tile(10), generation));
        assert_eq!(cache.lookup("s", 1, 0, 0), Lookup::Hit(tile(10)));
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
        // A request misses, reads the old tile, the updater commits and
        // invalidates, then the request tries to cache what it read: refused.
        let cache = TileCache::new(1 << 20, None);
        let Lookup::Miss { generation } = cache.lookup("s", 4, 2, 2) else {
            panic!("expected a miss");
        };
        cache.invalidate("s", [(4, 2, 2)]);
        assert!(!cache.insert("s", 4, 2, 2, tile(10), generation));
        assert_eq!(cache.get("s", 4, 2, 2), None);
        // A fresh read after the invalidation caches normally.
        assert!(cache.insert("s", 4, 2, 2, tile(10), cache.generation("s")));
    }

    #[test]
    fn test_clear_of_uncached_source_still_blocks_inflight_reads() {
        // The source has nothing cached when it is cleared; a read already in
        // flight must still be refused.
        let cache = TileCache::new(1 << 20, None);
        let Lookup::Miss { generation } = cache.lookup("s", 0, 0, 0) else {
            panic!("expected a miss");
        };
        assert_eq!(cache.clear_source("s"), 0);
        assert!(!cache.insert("s", 0, 0, 0, tile(10), generation));
    }

    #[test]
    fn test_invalidation_does_not_block_other_sources() {
        let cache = TileCache::new(1 << 20, None);
        let gen_t = cache.generation("t");
        cache.invalidate("s", [(0, 0, 0)]);
        assert!(cache.insert("t", 0, 0, 0, tile(10), gen_t));
    }

    #[test]
    fn test_invalidate_ranges_clears_tiles_inside_them() {
        use crate::tiles::TileRange;
        let cache = TileCache::new(1 << 20, None);
        cache.insert("s", 5, 1, 1, tile(10), 0);
        cache.insert("s", 5, 3, 3, CachedTile::Empty, 0);
        cache.insert("s", 6, 1, 1, tile(10), 0);
        cache.insert("t", 5, 1, 1, tile(10), 0);
        let Lookup::Miss { generation } = cache.lookup("s", 5, 2, 2) else {
            panic!("expected a miss");
        };
        let range = TileRange {
            z: 5,
            x_min: 0,
            x_max: 3,
            y_min: 0,
            y_max: 3,
        };
        assert_eq!(cache.invalidate_ranges("s", &[range]), 2);
        assert_eq!(cache.get("s", 5, 1, 1), None);
        assert_eq!(cache.get("s", 5, 3, 3), None, "empty tiles too");
        assert!(cache.get("s", 6, 1, 1).is_some());
        assert!(cache.get("t", 5, 1, 1).is_some());
        // A read in flight across the invalidation is refused
        assert!(!cache.insert("s", 5, 2, 2, tile(10), generation));
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
