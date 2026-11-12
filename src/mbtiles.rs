use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use std::path::Path;
use tracing::info;

use crate::tiles::TileCoord;

/// Build a `file:` URI with `immutable=1` for reading a database on read-only
/// media. Only the characters SQLite's URI parser treats specially are escaped.
fn immutable_uri(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    // file:///C:/... for a Windows absolute path
    if !normalized.starts_with('/') && normalized.chars().nth(1) == Some(':') {
        normalized.insert(0, '/');
    }

    let escaped: String = normalized
        .chars()
        .map(|c| match c {
            '%' => "%25".to_string(),
            '?' => "%3f".to_string(),
            '#' => "%23".to_string(),
            ' ' => "%20".to_string(),
            other => other.to_string(),
        })
        .collect();

    format!("file:{}?immutable=1", escaped)
}

pub struct MbtilesStore {
    conn: Connection,
    /// Transactions committed through this connection. An on-demand render
    /// notes it before querying PostGIS and stores its tile only if no update
    /// committed in the meantime, since that update may have dropped the very
    /// tile it rendered from older data.
    commits: std::cell::Cell<u64>,
    /// Times `replace_contents_from` swapped in a whole new file. An
    /// incremental update notes it before encoding and doesn't write tiles
    /// encoded before a swap over the regenerated ones.
    replacements: std::cell::Cell<u64>,
}

impl MbtilesStore {
    /// Open an existing MBTiles file, materializing the tiles view if needed.
    ///
    /// The file must already exist and hold tiles. SQLite would otherwise happily
    /// create an empty database here, and the caller would serve or inspect it as
    /// if it were real, failing later with "no such table: tiles".
    pub fn open(path: &str) -> Result<Self> {
        // Without SQLITE_OPEN_CREATE, SQLite itself refuses to conjure a file up.
        // A `Path::exists()` pre-check would race a concurrent `generate`, and
        // would report an unreadable directory as a missing file.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;

        let conn = Connection::open_with_flags(path, flags).map_err(|e| {
            if Path::new(path).exists() {
                anyhow!("Failed to open MBTiles at {}: {}", path, e)
            } else {
                anyhow!(
                    "MBTiles file not found: {}. Run `tilefeed generate` to build it first.",
                    path
                )
            }
        })?;

        // Distinguish "no tiles table" from "could not read the schema at all":
        // a corrupt or locked database must not be reported as a file to rebuild.
        let has_tiles: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master
                 WHERE name = 'tiles' AND type IN ('table', 'view')",
                [],
                |row| row.get(0),
            )
            .with_context(|| format!("Failed to read the MBTiles schema of {}", path))?;

        if !has_tiles {
            bail!(
                "{} is not a valid MBTiles file (no `tiles` table). \
                 Run `tilefeed generate` to rebuild it.",
                path
            );
        }

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        // Tippecanoe creates a `tiles` view over `map` + `images` tables.
        // Materialize it into a real table so we can INSERT/UPDATE/DELETE.
        let is_view: bool = conn
            .query_row(
                "SELECT type = 'view' FROM sqlite_master WHERE name = 'tiles'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if is_view {
            info!("Materializing tiles view into a writable table");
            conn.execute_batch(
                "
                CREATE TABLE tiles_real (
                    zoom_level INTEGER NOT NULL,
                    tile_column INTEGER NOT NULL,
                    tile_row INTEGER NOT NULL,
                    tile_data BLOB,
                    UNIQUE (zoom_level, tile_column, tile_row)
                );
                INSERT INTO tiles_real SELECT * FROM tiles;
                DROP VIEW tiles;
                ALTER TABLE tiles_real RENAME TO tiles;
                CREATE INDEX IF NOT EXISTS idx_tiles ON tiles (zoom_level, tile_column, tile_row);
                ",
            )?;
            info!("Tiles view materialized successfully");
        }

        Ok(Self {
            conn,
            commits: Default::default(),
            replacements: Default::default(),
        })
    }

    /// Open an MBTiles file without writing to it.
    ///
    /// [`open`](Self::open) flips the journal to WAL and materializes a
    /// Tippecanoe `tiles` view into a table — both writes. A read-only command
    /// pointed at a published or archived MBTiles, possibly on read-only media,
    /// must not do either. Views read fine; only writing through one needs the
    /// materialization.
    pub fn open_read_only(path: &str) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;

        let opened = Connection::open_with_flags(path, flags).and_then(|conn| {
            // Opening succeeds lazily; a WAL database only tries to create its
            // -shm sidecar on the first read, so probe before deciding.
            conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
                row.get::<_, i64>(0)
            })?;
            Ok(conn)
        });

        let conn = match opened {
            Ok(conn) => conn,
            Err(first_error) => {
                // A WAL database on read-only media cannot create its -shm file.
                // `immutable=1` tells SQLite the file will not change while open,
                // which is exactly true of the published artifacts this command
                // exists to read; it is only a fallback, so a writable database
                // still takes the normal path above.
                Connection::open_with_flags(immutable_uri(path), flags).map_err(|_| {
                    if Path::new(path).exists() {
                        anyhow!("Failed to open MBTiles at {}: {}", path, first_error)
                    } else {
                        anyhow!("MBTiles file not found: {}", path)
                    }
                })?
            }
        };

        let has_tiles: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master
                 WHERE name = 'tiles' AND type IN ('table', 'view')",
                [],
                |row| row.get(0),
            )
            .with_context(|| format!("Failed to read the MBTiles schema of {}", path))?;

        if !has_tiles {
            bail!("{} is not a valid MBTiles file (no `tiles` table)", path);
        }

        Ok(Self {
            conn,
            commits: Default::default(),
            replacements: Default::default(),
        })
    }

    /// Stream every tile coordinate, as (zoom, column, TMS row).
    ///
    /// Coordinates only: a caller that has to reorder tiles — PMTiles export
    /// sorts them onto a Hilbert curve — would otherwise have to hold every
    /// tile's bytes in memory to do it.
    pub fn for_each_tile_coord<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(u8, u32, u32) -> Result<()>,
    {
        let mut stmt = self.conn.prepare(
            "SELECT zoom_level, tile_column, tile_row FROM tiles
             WHERE tile_data IS NOT NULL",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            f(
                row.get::<_, u8>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
            )?;
        }
        Ok(())
    }

    /// Create a new MBTiles file with the required schema
    pub fn create(path: &str) -> Result<Self> {
        if Path::new(path).exists() {
            std::fs::remove_file(path)?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("Failed to create MBTiles at {}", path))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        conn.execute_batch(
            "
            CREATE TABLE metadata (
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                UNIQUE (name)
            );

            CREATE TABLE tiles (
                zoom_level INTEGER NOT NULL,
                tile_column INTEGER NOT NULL,
                tile_row INTEGER NOT NULL,
                tile_data BLOB,
                UNIQUE (zoom_level, tile_column, tile_row)
            );

            CREATE INDEX idx_tiles ON tiles (zoom_level, tile_column, tile_row);
            ",
        )?;

        info!("Created new MBTiles file at {}", path);
        Ok(Self {
            conn,
            commits: Default::default(),
            replacements: Default::default(),
        })
    }

    /// Set metadata value
    pub fn set_metadata(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)",
            params![name, value],
        )?;
        Ok(())
    }

    /// The `bounds` metadata, if present and well-formed.
    pub fn metadata_bounds(&self) -> Result<Option<[f64; 4]>> {
        let Some(meta) = self.get_metadata("bounds")? else {
            return Ok(None);
        };
        let parts: Vec<f64> = meta
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        Ok(match parts[..] {
            [w, s, e, n] if [w, s, e, n].iter().all(|v| v.is_finite()) && w <= e && s <= n => {
                Some([w, s, e, n])
            }
            _ => None,
        })
    }

    /// Grow the `bounds` metadata to cover `boxes` (`[w, s, e, n]`), so it
    /// keeps up with incremental updates. Never shrinks it: a deleted feature
    /// leaves the box as it was, as a regeneration would only fix later. A
    /// store without `bounds` metadata is left without (`bounds()` then reads
    /// the tiles, which are already current), unless `create` is set: an
    /// on-demand source that started out empty only renders inside its
    /// `bounds`, so its first features have to create them.
    pub fn widen_bounds(
        &self,
        boxes: impl IntoIterator<Item = [f64; 4]>,
        create: bool,
    ) -> Result<()> {
        let mut boxes = boxes.into_iter();
        let old = match self.metadata_bounds()? {
            Some(old) => old,
            None if create => match boxes.next() {
                Some(first) => {
                    self.set_metadata(
                        "bounds",
                        &format!("{},{},{},{}", first[0], first[1], first[2], first[3]),
                    )?;
                    first
                }
                None => return Ok(()),
            },
            None => return Ok(()),
        };
        let mut new = old;
        for [w, s, e, n] in boxes {
            new = [new[0].min(w), new[1].min(s), new[2].max(e), new[3].max(n)];
        }
        if new != old {
            self.set_metadata(
                "bounds",
                &format!("{},{},{},{}", new[0], new[1], new[2], new[3]),
            )?;
        }
        Ok(())
    }

    /// The area the tiles cover, `[west, south, east, north]` in degrees.
    /// Tippecanoe and native generation record it as `bounds` metadata.
    /// Without it (files from older versions) the extent of the tiles at the
    /// deepest stored zoom up to 10 is used: close enough to frame a map, and
    /// a much smaller scan than the deepest zoom. `None` when empty.
    pub fn bounds(&self) -> Result<Option<[f64; 4]>> {
        if let Some(bounds) = self.metadata_bounds()? {
            return Ok(Some(bounds));
        }
        let row: Option<(u8, u32, u32, u32, u32)> = self.conn.query_row(
            "SELECT zoom_level, MIN(tile_column), MAX(tile_column), MIN(tile_row), MAX(tile_row)
                 FROM tiles
                 WHERE zoom_level = COALESCE(
                     (SELECT MAX(zoom_level) FROM tiles WHERE zoom_level <= 10),
                     (SELECT MIN(zoom_level) FROM tiles))",
            [],
            |r| {
                let z: Option<u8> = r.get(0)?;
                Ok(match z {
                    Some(z) => Some((z, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    None => None,
                })
            },
        )?;
        let Some((z, x_min, x_max, tms_min, tms_max)) = row else {
            return Ok(None);
        };
        // TMS rows count from the south: the highest row is the northmost.
        let flip = |tms: u32| (1u32 << z) - 1 - tms;
        let north_west = TileCoord {
            z,
            x: x_min,
            y: flip(tms_max),
        }
        .bounds();
        let south_east = TileCoord {
            z,
            x: x_max,
            y: flip(tms_min),
        }
        .bounds();
        Ok(Some([
            north_west.min_lon,
            south_east.min_lat,
            south_east.max_lon,
            north_west.max_lat,
        ]))
    }

    /// Get a tile's data
    pub fn get_tile(&self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        // MBTiles uses TMS y-coordinate (flipped)
        let tms_y = (1u32 << z) - 1 - y;

        let mut stmt = self.conn.prepare(
            "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        )?;

        let result = stmt.query_row(params![z as i32, x as i32, tms_y as i32], |row| {
            row.get::<_, Vec<u8>>(0)
        });

        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Insert or replace a tile
    pub fn put_tile(&self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        // MBTiles uses TMS y-coordinate (flipped)
        let tms_y = (1u32 << z) - 1 - y;

        self.conn.execute(
            "INSERT OR REPLACE INTO tiles (zoom_level, tile_column, tile_row, tile_data) VALUES (?1, ?2, ?3, ?4)",
            params![z as i32, x as i32, tms_y as i32, data],
        )?;

        Ok(())
    }

    /// Delete a tile
    pub fn delete_tile(&self, z: u8, x: u32, y: u32) -> Result<()> {
        let tms_y = (1u32 << z) - 1 - y;

        self.conn.execute(
            "DELETE FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
            params![z as i32, x as i32, tms_y as i32],
        )?;

        Ok(())
    }

    /// Write default metadata for vector tiles
    /// Delete every stored tile in `range` (XYZ), returning how many there were.
    pub fn delete_tile_range(&self, range: &crate::tiles::TileRange) -> Result<usize> {
        // MBTiles rows are TMS: the flip reverses the y range
        let flip = |y: u32| (1u32 << range.z) - 1 - y;
        Ok(self.conn.execute(
            "DELETE FROM tiles WHERE zoom_level = ?1
                 AND tile_column BETWEEN ?2 AND ?3 AND tile_row BETWEEN ?4 AND ?5",
            params![
                range.z as i32,
                range.x_min as i32,
                range.x_max as i32,
                flip(range.y_max) as i32,
                flip(range.y_min) as i32
            ],
        )?)
    }

    pub fn write_default_metadata(&self, name: &str, description: &str) -> Result<()> {
        self.set_metadata("name", name)?;
        self.set_metadata("format", "pbf")?;
        self.set_metadata("type", "overlay")?;
        self.set_metadata("version", "2")?;
        self.set_metadata("description", description)?;
        self.set_metadata("scheme", "tms")?;
        Ok(())
    }

    /// Begin a transaction for batch operations
    pub fn begin_transaction(&self) -> Result<()> {
        self.conn.execute("BEGIN TRANSACTION", [])?;
        Ok(())
    }

    /// Commit the current transaction
    pub fn commit_transaction(&self) -> Result<()> {
        self.conn.execute("COMMIT", [])?;
        self.commits.set(self.commits.get() + 1);
        Ok(())
    }

    /// Replace every tile and metadata row with those of the MBTiles at
    /// `path`, in one transaction through this connection.
    ///
    /// Used to swap in a regenerated file while `serve` holds this one open:
    /// readers see the old tiles until the commit and the new ones after, no
    /// file is renamed under an open handle (which Windows refuses), and the
    /// commit counts in `commit_count`, so an on-demand render that started
    /// before it is not stored over the new contents.
    pub fn replace_contents_from(&self, path: &str) -> Result<()> {
        self.conn
            .execute("ATTACH DATABASE ?1 AS regen", params![path])
            .with_context(|| format!("Failed to attach {}", path))?;
        let copied = (|| -> Result<()> {
            self.begin_transaction()?;
            let result = self.conn.execute_batch(
                "DELETE FROM main.tiles;
                 INSERT INTO main.tiles (zoom_level, tile_column, tile_row, tile_data)
                     SELECT zoom_level, tile_column, tile_row, tile_data FROM regen.tiles;
                 DELETE FROM main.metadata;
                 INSERT INTO main.metadata (name, value) SELECT name, value FROM regen.metadata;",
            );
            match result {
                Ok(()) => self.commit_transaction(),
                Err(e) => {
                    let _ = self.rollback_transaction();
                    Err(e.into())
                }
            }
        })();
        let detached = self.conn.execute("DETACH DATABASE regen", []);
        copied?;
        self.replacements.set(self.replacements.get() + 1);
        detached?;
        Ok(())
    }

    /// How many times `replace_contents_from` has swapped in a new file.
    pub fn replacement_count(&self) -> u64 {
        self.replacements.get()
    }

    /// Fold the WAL into the database file, so copying the `.mbtiles` alone
    /// (as publishing does) gets every committed write.
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// How many transactions this connection has committed. See `commits`.
    pub fn commit_count(&self) -> u64 {
        self.commits.get()
    }

    /// Rollback the current transaction
    pub fn rollback_transaction(&self) -> Result<()> {
        self.conn.execute("ROLLBACK", [])?;
        Ok(())
    }

    /// Read a metadata value by name
    pub fn get_metadata(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM metadata WHERE name = ?1")?;
        let result = stmt.query_row(params![name], |row| row.get::<_, String>(0));
        match result {
            Ok(val) => Ok(Some(val)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all metadata key-value pairs
    pub fn get_all_metadata(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, value FROM metadata ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// SQLite's `data_version`: changes whenever *another* connection commits
    /// to this database file, never for this connection's own writes. The tile
    /// server polls it to notice MBTiles rewritten by a different process.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |row| row.get(0))?)
    }

    /// Count total tiles
    pub fn tile_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tiles", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Count tiles per zoom level
    pub fn tile_count_by_zoom(&self) -> Result<Vec<(u8, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT zoom_level, COUNT(*) FROM tiles GROUP BY zoom_level ORDER BY zoom_level",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, u8>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get total size of all tile data in bytes
    pub fn total_tile_size(&self) -> Result<u64> {
        let size: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(tile_data)), 0) FROM tiles",
            [],
            |row| row.get(0),
        )?;
        Ok(size as u64)
    }

    /// Get average tile size in bytes
    pub fn avg_tile_size(&self) -> Result<f64> {
        let avg: f64 = self.conn.query_row(
            "SELECT COALESCE(AVG(LENGTH(tile_data)), 0) FROM tiles",
            [],
            |row| row.get(0),
        )?;
        Ok(avg)
    }

    /// Get all tile coordinates (for diff)
    pub fn all_tile_coords(&self) -> Result<Vec<(u8, u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT zoom_level, tile_column, tile_row FROM tiles ORDER BY zoom_level, tile_column, tile_row"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, u8>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get raw tile data by TMS coordinates (no y-flip)
    pub fn get_tile_raw_tms(&self, z: u8, x: u32, tms_y: u32) -> Result<Option<Vec<u8>>> {
        // Cached: an export calls this once per tile, and recompiling the SQL
        // each time would dominate the lookup itself.
        let mut stmt = self.conn.prepare_cached(
            "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        )?;
        let result = stmt.query_row(params![z as i32, x as i32, tms_y as i32], |row| {
            row.get::<_, Vec<u8>>(0)
        });
        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Read the raw tile_row stored in SQLite for a given z/x/tms_y (no flip)
    #[cfg(test)]
    fn get_raw_tile_row(&self, z: u8, x: u32, tms_y: u32) -> Result<Option<Vec<u8>>> {
        let mut stmt = self.conn.prepare(
            "SELECT tile_data FROM tiles WHERE zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3",
        )?;
        let result = stmt.query_row(params![z as i32, x as i32, tms_y as i32], |row| {
            row.get::<_, Vec<u8>>(0)
        });
        match result {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_mbtiles_path() -> String {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("tilefeed_test_{}_{}_{}.mbtiles", pid, ts, id));
        path.to_string_lossy().to_string()
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_bounds_empty_store_is_none() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        assert_eq!(store.bounds().unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn test_bounds_prefers_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(0, 0, 0, b"t").unwrap();
        store
            .set_metadata("bounds", "2.25,48.8, 2.45,48.9")
            .unwrap();
        assert_eq!(store.bounds().unwrap(), Some([2.25, 48.8, 2.45, 48.9]));
        // Malformed or inverted metadata falls back to the tiles
        store.set_metadata("bounds", "5,5,1,1").unwrap();
        assert_eq!(
            store.bounds().unwrap(),
            Some([-180.0, -85.0511287798066, 180.0, 85.0511287798066])
        );
        cleanup(&path);
    }

    #[test]
    fn test_widen_bounds_only_grows_existing_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        // Without metadata there is nothing to widen
        store.widen_bounds([[0.0, 0.0, 1.0, 1.0]], false).unwrap();
        assert_eq!(store.get_metadata("bounds").unwrap(), None);

        store.set_metadata("bounds", "2,48,3,49").unwrap();
        store.widen_bounds([[2.5, 48.5, 2.6, 48.6]], false).unwrap();
        assert_eq!(store.bounds().unwrap(), Some([2.0, 48.0, 3.0, 49.0]));
        store
            .widen_bounds([[1.5, 48.2, 2.2, 48.3], [2.9, 48.9, 3.5, 49.5]], false)
            .unwrap();
        assert_eq!(store.bounds().unwrap(), Some([1.5, 48.0, 3.5, 49.5]));
        cleanup(&path);
    }

    #[test]
    fn test_widen_bounds_can_create_missing_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.widen_bounds([], true).unwrap();
        assert_eq!(store.get_metadata("bounds").unwrap(), None);

        store
            .widen_bounds([[2.5, 48.5, 2.6, 48.6], [2.0, 48.9, 2.1, 49.0]], true)
            .unwrap();
        assert_eq!(
            store.metadata_bounds().unwrap(),
            Some([2.0, 48.5, 2.6, 49.0])
        );
        cleanup(&path);
    }

    #[test]
    fn test_delete_tile_range_takes_xyz_rows() {
        use crate::tiles::TileRange;
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        for x in 0..4 {
            for y in 0..4 {
                store.put_tile(2, x, y, b"t").unwrap();
            }
        }
        store.put_tile(3, 1, 1, b"t").unwrap();
        let range = TileRange {
            z: 2,
            x_min: 1,
            x_max: 2,
            y_min: 0,
            y_max: 1,
        };
        assert_eq!(store.delete_tile_range(&range).unwrap(), 4);
        for x in 0..4 {
            for y in 0..4 {
                let gone = range.contains(2, x, y);
                assert_eq!(store.get_tile(2, x, y).unwrap().is_none(), gone, "{x},{y}");
            }
        }
        assert!(store.get_tile(3, 1, 1).unwrap().is_some());
        assert_eq!(store.delete_tile_range(&range).unwrap(), 0);
        cleanup(&path);
    }

    #[test]
    fn test_replace_contents_from_swaps_tiles_and_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.write_default_metadata("old", "old").unwrap();
        store.set_metadata("bounds", "0,0,1,1").unwrap();
        store.put_tile(1, 0, 0, b"old").unwrap();
        store.put_tile(2, 1, 1, b"gone").unwrap();

        let regen = temp_mbtiles_path();
        let new = MbtilesStore::create(&regen).unwrap();
        new.write_default_metadata("new", "new").unwrap();
        new.put_tile(1, 0, 0, b"new").unwrap();
        new.put_tile(3, 2, 2, b"added").unwrap();
        drop(new);

        store.replace_contents_from(&regen).unwrap();
        assert_eq!(
            store.get_tile(1, 0, 0).unwrap().as_deref(),
            Some(&b"new"[..])
        );
        assert_eq!(store.get_tile(2, 1, 1).unwrap(), None);
        assert_eq!(
            store.get_tile(3, 2, 2).unwrap().as_deref(),
            Some(&b"added"[..])
        );
        assert_eq!(store.get_metadata("name").unwrap().as_deref(), Some("new"));
        // Metadata the new file lacks is gone too
        assert_eq!(store.get_metadata("bounds").unwrap(), None);
        assert_eq!(store.commit_count(), 1);
        assert_eq!(store.replacement_count(), 1);
        // Detached again: a second swap can attach the same name
        store.replace_contents_from(&regen).unwrap();
        cleanup(&path);
        cleanup(&regen);
    }

    #[test]
    fn test_replace_contents_from_a_bad_file_keeps_the_tiles() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(1, 0, 0, b"kept").unwrap();
        let not_mbtiles = temp_mbtiles_path();
        rusqlite::Connection::open(&not_mbtiles)
            .unwrap()
            .execute_batch("CREATE TABLE other (x)")
            .unwrap();

        assert!(store.replace_contents_from(&not_mbtiles).is_err());
        assert_eq!(
            store.get_tile(1, 0, 0).unwrap().as_deref(),
            Some(&b"kept"[..])
        );
        assert_eq!(store.commit_count(), 0);
        assert_eq!(store.replacement_count(), 0);
        cleanup(&path);
        cleanup(&not_mbtiles);
    }

    #[test]
    fn test_checkpoint_folds_the_wal_into_the_file() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.begin_transaction().unwrap();
        store.put_tile(0, 0, 0, b"committed").unwrap();
        store.commit_transaction().unwrap();
        store.checkpoint().unwrap();
        // A copy of the main file alone has the tile
        let copy = temp_mbtiles_path();
        std::fs::copy(&path, &copy).unwrap();
        let copied = MbtilesStore::open(&copy).unwrap();
        assert_eq!(
            copied.get_tile(0, 0, 0).unwrap().as_deref(),
            Some(&b"committed"[..])
        );
        cleanup(&path);
        cleanup(&copy);
    }

    #[test]
    fn test_commit_count_counts_committed_transactions_only() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(0, 0, 0, b"t").unwrap();
        store.begin_transaction().unwrap();
        store.rollback_transaction().unwrap();
        assert_eq!(store.commit_count(), 0);
        store.begin_transaction().unwrap();
        store.commit_transaction().unwrap();
        assert_eq!(store.commit_count(), 1);
        cleanup(&path);
    }

    #[test]
    fn test_bounds_from_tiles_uses_deepest_zoom_up_to_10() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        // z1 north-west quarter, and at z2 the two tiles x=0..1, y=1 (XYZ)
        store.put_tile(1, 0, 0, b"t").unwrap();
        store.put_tile(2, 0, 1, b"t").unwrap();
        store.put_tile(2, 1, 1, b"t").unwrap();
        // A deeper zoom past 10 is ignored for the scan
        store.put_tile(12, 0, 0, b"t").unwrap();
        let [w, s, e, n] = store.bounds().unwrap().unwrap();
        assert_eq!((w, e), (-180.0, 0.0));
        let row1 = TileCoord { z: 2, x: 0, y: 1 }.bounds();
        assert!(
            (s - row1.min_lat).abs() < 1e-9 && (n - row1.max_lat).abs() < 1e-9,
            "{s} {n}"
        );
        cleanup(&path);
    }

    #[test]
    fn test_create_and_put_get_roundtrip() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let data = b"hello tile";
        store.put_tile(5, 10, 12, data).unwrap();

        let retrieved = store.get_tile(5, 10, 12).unwrap();
        assert_eq!(retrieved, Some(data.to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_get_tile_not_found() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let result = store.get_tile(0, 0, 0).unwrap();
        assert_eq!(result, None);

        cleanup(&path);
    }

    #[test]
    fn test_put_tile_overwrite() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(3, 4, 5, b"first").unwrap();
        store.put_tile(3, 4, 5, b"second").unwrap();

        let retrieved = store.get_tile(3, 4, 5).unwrap();
        assert_eq!(retrieved, Some(b"second".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_delete_tile() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(2, 1, 1, b"data").unwrap();
        assert!(store.get_tile(2, 1, 1).unwrap().is_some());

        store.delete_tile(2, 1, 1).unwrap();
        assert_eq!(store.get_tile(2, 1, 1).unwrap(), None);

        cleanup(&path);
    }

    #[test]
    fn test_delete_tile_nonexistent() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        // Deleting a tile that doesn't exist should not error
        store.delete_tile(0, 0, 0).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_write_default_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store
            .write_default_metadata("my_tiles", "A test tileset")
            .unwrap();

        assert_eq!(
            store.get_metadata("name").unwrap(),
            Some("my_tiles".to_string())
        );
        assert_eq!(
            store.get_metadata("format").unwrap(),
            Some("pbf".to_string())
        );
        assert_eq!(
            store.get_metadata("type").unwrap(),
            Some("overlay".to_string())
        );
        assert_eq!(
            store.get_metadata("version").unwrap(),
            Some("2".to_string())
        );
        assert_eq!(
            store.get_metadata("description").unwrap(),
            Some("A test tileset".to_string())
        );
        assert_eq!(
            store.get_metadata("scheme").unwrap(),
            Some("tms".to_string())
        );

        cleanup(&path);
    }

    #[test]
    fn test_set_metadata_overwrite() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.set_metadata("name", "first").unwrap();
        assert_eq!(
            store.get_metadata("name").unwrap(),
            Some("first".to_string())
        );

        store.set_metadata("name", "second").unwrap();
        assert_eq!(
            store.get_metadata("name").unwrap(),
            Some("second".to_string())
        );

        cleanup(&path);
    }

    #[test]
    fn test_tms_y_flip() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        // At zoom 2, there are 4 rows (0..3).
        // XYZ y=0 should map to TMS y = (1<<2) - 1 - 0 = 3
        // XYZ y=3 should map to TMS y = (1<<2) - 1 - 3 = 0
        let z: u8 = 2;
        let x: u32 = 1;
        let xyz_y: u32 = 0;

        store.put_tile(z, x, xyz_y, b"top_tile").unwrap();

        // The raw SQLite row should have tms_y = 3
        let expected_tms_y: u32 = (1u32 << z) - 1 - xyz_y;
        assert_eq!(expected_tms_y, 3);

        let raw = store.get_raw_tile_row(z, x, expected_tms_y).unwrap();
        assert_eq!(raw, Some(b"top_tile".to_vec()));

        // Conversely, reading via get_tile with the same xyz_y should work
        let retrieved = store.get_tile(z, x, xyz_y).unwrap();
        assert_eq!(retrieved, Some(b"top_tile".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_tms_y_flip_bottom() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        // At zoom 3, XYZ y=7 -> TMS y = (1<<3) - 1 - 7 = 0
        let z: u8 = 3;
        let x: u32 = 2;
        let xyz_y: u32 = 7;

        store.put_tile(z, x, xyz_y, b"bottom_tile").unwrap();

        let expected_tms_y: u32 = 0;
        let raw = store.get_raw_tile_row(z, x, expected_tms_y).unwrap();
        assert_eq!(raw, Some(b"bottom_tile".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_transaction_commit() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.begin_transaction().unwrap();
        store.put_tile(0, 0, 0, b"txn_data").unwrap();
        store.commit_transaction().unwrap();

        assert_eq!(store.get_tile(0, 0, 0).unwrap(), Some(b"txn_data".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_transaction_rollback() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        // Insert one tile outside a transaction
        store.put_tile(0, 0, 0, b"existing").unwrap();

        store.begin_transaction().unwrap();
        store.put_tile(1, 0, 0, b"will_rollback").unwrap();
        store.rollback_transaction().unwrap();

        // The rolled-back tile should not exist
        assert_eq!(store.get_tile(1, 0, 0).unwrap(), None);
        // The previously committed tile should still exist
        assert_eq!(store.get_tile(0, 0, 0).unwrap(), Some(b"existing".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_open_read_only_does_not_write_to_the_source() {
        let path = temp_mbtiles_path();
        {
            let store = MbtilesStore::create(&path).unwrap();
            store.put_tile(1, 0, 0, b"data").unwrap();
        }

        let before = std::fs::metadata(&path).unwrap().len();
        {
            let store = MbtilesStore::open_read_only(&path).unwrap();
            assert_eq!(store.tile_count().unwrap(), 1);
            assert_eq!(
                store.get_tile(1, 0, 0).unwrap().as_deref(),
                Some(&b"data"[..])
            );
            // A read-only handle must refuse writes rather than silently work
            assert!(store.put_tile(2, 0, 0, b"nope").is_err());
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            before,
            "a read-only open must not change the file"
        );

        cleanup(&path);
    }

    #[test]
    fn test_open_read_only_rejects_a_missing_file() {
        let path = temp_mbtiles_path();
        let err = match MbtilesStore::open_read_only(&path) {
            Err(e) => e,
            Ok(_) => panic!("opening a missing file read-only must fail"),
        };
        assert!(err.to_string().contains("not found"), "got: {}", err);
    }

    #[test]
    fn test_immutable_uri_escaping() {
        assert_eq!(
            immutable_uri("/tmp/a.mbtiles"),
            "file:/tmp/a.mbtiles?immutable=1"
        );
        assert_eq!(
            immutable_uri("/tmp/my tiles.mbtiles"),
            "file:/tmp/my%20tiles.mbtiles?immutable=1"
        );
        // Windows paths become file:///C:/...
        assert_eq!(
            immutable_uri("C:\\data\\a.mbtiles"),
            "file:/C:/data/a.mbtiles?immutable=1"
        );
    }

    #[test]
    fn test_for_each_tile_coord_streams_every_row() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(1, 0, 0, b"a").unwrap();
        store.put_tile(1, 1, 1, b"b").unwrap();

        let mut seen = Vec::new();
        store
            .for_each_tile_coord(|z, x, tms_y| {
                seen.push((z, x, tms_y));
                Ok(())
            })
            .unwrap();

        assert_eq!(seen.len(), 2);
        // Rows come back in TMS space, as stored: XYZ 1/0/0 is TMS row 1
        assert!(seen.contains(&(1, 0, 1)));

        cleanup(&path);
    }

    #[test]
    fn test_open_missing_file_errors() {
        // SQLite would create an empty database here; serving it would look fine
        // at startup and fail on every tile request instead.
        let path = temp_mbtiles_path();
        let err = match MbtilesStore::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("opening a missing MBTiles file must fail"),
        };
        assert!(err.to_string().contains("not found"), "got: {}", err);
        assert!(
            !std::path::Path::new(&path).exists(),
            "a failed open must not leave a file behind"
        );
    }

    #[test]
    fn test_open_file_without_tiles_table_errors() {
        let path = temp_mbtiles_path();
        // A real file, but not an MBTiles one
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE something_else (id INTEGER);")
            .unwrap();

        let err = match MbtilesStore::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("opening a non-MBTiles file must fail"),
        };
        assert!(
            err.to_string().contains("not a valid MBTiles"),
            "got: {}",
            err
        );

        cleanup(&path);
    }

    #[test]
    fn test_open_corrupt_file_reports_the_real_error() {
        // Telling someone to rebuild a 40GB planet file because a read failed is
        // worse than useless, so corruption must not be reported as "no tiles table".
        let path = temp_mbtiles_path();
        std::fs::write(&path, b"this is definitely not a sqlite database").unwrap();

        let err = match MbtilesStore::open(&path) {
            Err(e) => e,
            Ok(_) => panic!("opening a corrupt file must fail"),
        };
        let message = format!("{:#}", err);
        assert!(
            !message.contains("not a valid MBTiles"),
            "corruption should not be reported as a missing tiles table: {}",
            message
        );

        cleanup(&path);
    }

    #[test]
    fn test_open_existing_mbtiles() {
        let path = temp_mbtiles_path();

        // Create and populate
        {
            let store = MbtilesStore::create(&path).unwrap();
            store.put_tile(5, 10, 10, b"persist").unwrap();
        }

        // Re-open
        let store = MbtilesStore::open(&path).unwrap();
        let retrieved = store.get_tile(5, 10, 10).unwrap();
        assert_eq!(retrieved, Some(b"persist".to_vec()));

        cleanup(&path);
    }

    #[test]
    fn test_tile_count() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        assert_eq!(store.tile_count().unwrap(), 0);

        store.put_tile(0, 0, 0, b"a").unwrap();
        store.put_tile(1, 0, 0, b"b").unwrap();
        store.put_tile(1, 1, 0, b"c").unwrap();

        assert_eq!(store.tile_count().unwrap(), 3);
        cleanup(&path);
    }

    #[test]
    fn test_tile_count_by_zoom() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(0, 0, 0, b"a").unwrap();
        store.put_tile(2, 0, 0, b"b").unwrap();
        store.put_tile(2, 1, 0, b"c").unwrap();
        store.put_tile(2, 1, 1, b"d").unwrap();

        let counts = store.tile_count_by_zoom().unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0], (0, 1));
        assert_eq!(counts[1], (2, 3));
        cleanup(&path);
    }

    #[test]
    fn test_total_tile_size() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(0, 0, 0, b"hello").unwrap(); // 5 bytes
        store.put_tile(1, 0, 0, b"world!!").unwrap(); // 7 bytes

        assert_eq!(store.total_tile_size().unwrap(), 12);
        cleanup(&path);
    }

    #[test]
    fn test_get_all_metadata() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.set_metadata("name", "test").unwrap();
        store.set_metadata("format", "pbf").unwrap();

        let metadata = store.get_all_metadata().unwrap();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0], ("format".to_string(), "pbf".to_string()));
        assert_eq!(metadata[1], ("name".to_string(), "test".to_string()));
        cleanup(&path);
    }

    #[test]
    fn test_all_tile_coords() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(0, 0, 0, b"a").unwrap(); // TMS y = 0
        store.put_tile(1, 0, 0, b"b").unwrap(); // TMS y = 1

        let coords = store.all_tile_coords().unwrap();
        assert_eq!(coords.len(), 2);
        cleanup(&path);
    }

    #[test]
    fn test_avg_tile_size() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(0, 0, 0, b"hello").unwrap(); // 5 bytes
        store.put_tile(1, 0, 0, b"world!!").unwrap(); // 7 bytes

        let avg = store.avg_tile_size().unwrap();
        assert!((avg - 6.0).abs() < 0.01);
        cleanup(&path);
    }

    #[test]
    fn test_avg_tile_size_empty() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let avg = store.avg_tile_size().unwrap();
        assert!((avg - 0.0).abs() < 0.01);
        cleanup(&path);
    }

    #[test]
    fn test_get_tile_raw_tms() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        // put_tile uses XYZ y=0 at zoom 2 -> TMS y=3
        store.put_tile(2, 1, 0, b"data").unwrap();

        // get_tile_raw_tms uses raw TMS y, so tms_y=3 should find it
        let result = store.get_tile_raw_tms(2, 1, 3).unwrap();
        assert_eq!(result, Some(b"data".to_vec()));

        // tms_y=0 should not find it
        let result = store.get_tile_raw_tms(2, 1, 0).unwrap();
        assert_eq!(result, None);

        cleanup(&path);
    }

    #[test]
    fn test_get_tile_raw_tms_not_found() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let result = store.get_tile_raw_tms(5, 10, 20).unwrap();
        assert_eq!(result, None);
        cleanup(&path);
    }

    #[test]
    fn test_get_metadata_not_found() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let result = store.get_metadata("nonexistent").unwrap();
        assert_eq!(result, None);
        cleanup(&path);
    }

    #[test]
    fn test_tile_count_empty() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        assert_eq!(store.tile_count().unwrap(), 0);
        cleanup(&path);
    }

    #[test]
    fn test_tile_count_by_zoom_empty() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let counts = store.tile_count_by_zoom().unwrap();
        assert!(counts.is_empty());
        cleanup(&path);
    }

    #[test]
    fn test_total_tile_size_empty() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        assert_eq!(store.total_tile_size().unwrap(), 0);
        cleanup(&path);
    }

    #[test]
    fn test_all_tile_coords_empty() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        let coords = store.all_tile_coords().unwrap();
        assert!(coords.is_empty());
        cleanup(&path);
    }

    #[test]
    fn test_multiple_tiles_same_zoom() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.put_tile(3, 0, 0, b"a").unwrap();
        store.put_tile(3, 1, 0, b"b").unwrap();
        store.put_tile(3, 0, 1, b"c").unwrap();
        store.put_tile(3, 1, 1, b"d").unwrap();

        assert_eq!(store.tile_count().unwrap(), 4);

        let by_zoom = store.tile_count_by_zoom().unwrap();
        assert_eq!(by_zoom.len(), 1);
        assert_eq!(by_zoom[0], (3, 4));

        cleanup(&path);
    }

    #[test]
    fn test_create_overwrites_existing() {
        let path = temp_mbtiles_path();

        // Create and add data
        {
            let store = MbtilesStore::create(&path).unwrap();
            store.put_tile(0, 0, 0, b"old").unwrap();
        }

        // Create again should start fresh
        let store = MbtilesStore::create(&path).unwrap();
        assert_eq!(store.tile_count().unwrap(), 0);

        cleanup(&path);
    }

    #[test]
    fn test_batch_write_in_transaction() {
        let path = temp_mbtiles_path();
        let store = MbtilesStore::create(&path).unwrap();

        store.begin_transaction().unwrap();
        for i in 0..100u32 {
            store
                .put_tile(5, i, 0, format!("tile_{}", i).as_bytes())
                .unwrap();
        }
        store.commit_transaction().unwrap();

        assert_eq!(store.tile_count().unwrap(), 100);
        cleanup(&path);
    }
}
