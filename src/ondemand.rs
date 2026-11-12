//! Rendering tiles from PostGIS when they are requested, for sources with
//! `generation_backend = "on_demand"`.
//!
//! A rendered tile is stored in the source's MBTiles, so it is rendered once;
//! incremental updates drop the tiles a change touches and the next request
//! renders them again. Empty tiles are not stored (that would let any client
//! grow the file without bound by walking empty tiles at deep zooms): tiles
//! outside the source's `bounds` are answered without a query, and the tile
//! cache, when enabled, absorbs repeated requests for empty ones inside them.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

use crate::config::{AppConfig, SourceConfig};
use crate::mbtiles::MbtilesStore;
use crate::metrics::metrics;
use crate::postgis::PostgisReader;
use crate::tiles::TileCoord;

type TileKey = (String, u8, u32, u32);

pub struct Renderer {
    reader: PostgisReader,
    /// Bounds concurrent renders by the pool size, so a burst of requests
    /// queues here instead of starving the updater of connections.
    permits: Semaphore,
    /// One lock per tile being rendered: concurrent requests for the same
    /// tile wait for the first one and then find its result in the store.
    in_flight: std::sync::Mutex<HashMap<TileKey, Arc<Mutex<()>>>>,
}

impl Renderer {
    /// A renderer, if any source renders on demand.
    pub fn for_config(config: &AppConfig, reader: PostgisReader) -> Option<Self> {
        if !config.sources.iter().any(|s| s.renders_on_demand()) {
            return None;
        }
        let permits = Semaphore::new(reader.max_connections().max(1));
        Some(Self {
            reader,
            permits,
            in_flight: Default::default(),
        })
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
        let lock = self
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        let result = {
            let _rendering = lock.lock().await;
            self.render_unless_stored(source, store, tile).await
        };
        // The last request out removes the entry; one that arrives after this
        // gets a fresh lock, and finds the stored tile when it checks.
        {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            if Arc::strong_count(&lock) == 2 {
                in_flight.remove(&key);
            }
        }
        result
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
