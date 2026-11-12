//! Rendering tiles from PostGIS when they are requested, for sources with
//! `generation_backend = "on_demand"`.
//!
//! A rendered tile is stored in the source's MBTiles, so it is rendered once;
//! incremental updates drop the tiles a change touches and the next request
//! renders them again. Empty tiles are not stored (that would let any client
//! grow the file without bound by walking empty tiles at deep zooms): tiles
//! outside the source's `bounds` are answered without a query, and the tile
//! cache, when enabled, absorbs repeated requests for empty ones inside them.
//!
//! A rendered tile is written without `TileCache::invalidate`: it only gets
//! here on a cache miss, and the handler caches it under the generation it
//! looked up before reading, which any update since then has moved on.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, Semaphore};

use crate::config::SourceConfig;
use crate::mbtiles::MbtilesStore;
use crate::metrics::metrics;
use crate::postgis::PostgisReader;
use crate::tiles::TileCoord;

type TileKey = (String, u8, u32, u32);
/// The outcome of one render, shared by every request that waited for it.
type Shared = Arc<OnceCell<Option<Vec<u8>>>>;

pub struct Renderer {
    reader: PostgisReader,
    /// Bounds concurrent renders by the pool size. The renderer has a pool of
    /// its own (`pool_size` connections, opened as needed), so renders can't
    /// take the connections incremental updates use.
    permits: Semaphore,
    /// The tiles being rendered: concurrent requests for one tile share one
    /// render, empty or not, instead of each querying PostGIS in turn.
    in_flight: std::sync::Mutex<HashMap<TileKey, Shared>>,
}

/// A request's claim on an in-flight render; the last one out removes the
/// entry, also when a client disconnects and its request is dropped mid-wait.
struct Claim<'a> {
    in_flight: &'a std::sync::Mutex<HashMap<TileKey, Shared>>,
    key: TileKey,
    shared: Shared,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        // Held only by the map and this claim: nobody else is waiting.
        if Arc::strong_count(&self.shared) == 2
            && in_flight
                .get(&self.key)
                .is_some_and(|s| Arc::ptr_eq(s, &self.shared))
        {
            in_flight.remove(&self.key);
        }
    }
}

impl Renderer {
    pub fn new(reader: PostgisReader) -> Self {
        let permits = Semaphore::new(reader.max_connections().max(1));
        Self {
            reader,
            permits,
            in_flight: Default::default(),
        }
    }

    /// The tile at `tile`, rendering and storing it if the store lacks it.
    /// `None` means the tile is empty.
    pub async fn get_or_render(
        &self,
        source: &SourceConfig,
        store: &Mutex<MbtilesStore>,
        tile: TileCoord,
    ) -> Result<Option<Vec<u8>>> {
        let key = (source.name.clone(), tile.z, tile.x, tile.y);
        let shared = self
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        let claim = Claim {
            in_flight: &self.in_flight,
            key,
            shared,
        };
        // A failed render leaves the cell empty: the next waiter tries again.
        let data = claim
            .shared
            .get_or_try_init(|| self.render_unless_stored(source, store, tile))
            .await?;
        Ok(data.clone())
    }

    async fn render_unless_stored(
        &self,
        source: &SourceConfig,
        store: &Mutex<MbtilesStore>,
        tile: TileCoord,
    ) -> Result<Option<Vec<u8>>> {
        let (bounds, commits) = {
            let store = store.lock().await;
            if let Some(data) = store.get_tile(tile.z, tile.x, tile.y)? {
                return Ok(Some(data));
            }
            (store.metadata_bounds()?, store.commit_count())
        };

        if !bounds.is_some_and(|b| intersects(&tile, b)) {
            metrics()
                .tiles_rendered
                .with(&[&source.name, "empty"])
                .inc();
            return Ok(None);
        }

        let started = std::time::Instant::now();
        let rendered = {
            let _permit = self.permits.acquire().await?;
            crate::generator::render_tile(source, &self.reader, &tile).await
        };
        metrics()
            .tile_render_duration
            .with(&[&source.name])
            .observe_since(started);
        let data = match rendered {
            Ok(data) => data,
            Err(e) => {
                metrics()
                    .tiles_rendered
                    .with(&[&source.name, "failure"])
                    .inc();
                return Err(e);
            }
        };

        let Some(data) = data else {
            metrics()
                .tiles_rendered
                .with(&[&source.name, "empty"])
                .inc();
            return Ok(None);
        };

        // An update committed while we queried may have dropped this tile
        // after we read older rows: serve what we have, but don't keep it.
        let result = {
            let store = store.lock().await;
            if store.commit_count() == commits {
                store.put_tile(tile.z, tile.x, tile.y, &data)?;
                "stored"
            } else {
                "discarded"
            }
        };
        metrics().tiles_rendered.with(&[&source.name, result]).inc();
        Ok(Some(data))
    }
}

/// Whether a tile overlaps the data `bounds` (`[w, s, e, n]`), with the same
/// small margin native generation tiles the extent with.
fn intersects(tile: &TileCoord, [w, s, e, n]: [f64; 4]) -> bool {
    let t = tile.bounds();
    let m = crate::generator::EXTENT_MARGIN_DEG;
    t.min_lon <= e + m && t.max_lon >= w - m && t.min_lat <= n + m && t.max_lat >= s - m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intersects_tiles_overlapping_the_bounds() {
        let paris = [2.2, 48.8, 2.5, 48.9];
        // z0 covers everything
        assert!(intersects(&TileCoord { z: 0, x: 0, y: 0 }, paris));
        // z1: Paris is in the north-east quarter (x=1, y=0)
        assert!(intersects(&TileCoord { z: 1, x: 1, y: 0 }, paris));
        assert!(!intersects(&TileCoord { z: 1, x: 0, y: 0 }, paris));
        assert!(!intersects(&TileCoord { z: 1, x: 1, y: 1 }, paris));
    }

    #[test]
    fn test_intersects_a_tile_touching_the_bounds_edge() {
        // z1 x=1 spans lon 0..180; bounds ending exactly at lon 0 touch it
        let west = [-10.0, 10.0, 0.0, 20.0];
        assert!(intersects(&TileCoord { z: 1, x: 1, y: 0 }, west));
    }
}
