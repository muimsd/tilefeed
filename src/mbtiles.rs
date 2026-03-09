use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use tracing::info;

pub struct MbtilesStore {
    conn: Connection,
}

impl MbtilesStore {
    /// Open an existing MBTiles file, materializing the tiles view if needed
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open MBTiles at {}", path))?;

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

        Ok(Self { conn })
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
        Ok(Self { conn })
    }

    /// Set metadata value
    pub fn set_metadata(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata (name, value) VALUES (?1, ?2)",
            params![name, value],
        )?;
        Ok(())
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
        Ok(())
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
        let mut stmt = self.conn.prepare("SELECT name, value FROM metadata ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Count total tiles
    pub fn tile_count(&self) -> Result<u64> {
        let count: i64 = self.conn.query_row("SELECT COUNT(*) FROM tiles", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Count tiles per zoom level
    pub fn tile_count_by_zoom(&self) -> Result<Vec<(u8, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT zoom_level, COUNT(*) FROM tiles GROUP BY zoom_level ORDER BY zoom_level"
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
            store.put_tile(5, i, 0, format!("tile_{}", i).as_bytes()).unwrap();
        }
        store.commit_transaction().unwrap();

        assert_eq!(store.tile_count().unwrap(), 100);
        cleanup(&path);
    }
}
