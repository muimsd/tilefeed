use anyhow::Result;

use crate::mbtiles::MbtilesStore;

/// Inspect an MBTiles file and print its metadata and statistics
pub fn inspect_mbtiles(path: &str) -> Result<()> {
    let store = MbtilesStore::open(path)?;

    println!("MBTiles: {}", path);
    println!();

    // Metadata
    let metadata = store.get_all_metadata()?;
    if metadata.is_empty() {
        println!("Metadata: (none)");
    } else {
        println!("Metadata:");
        for (key, value) in &metadata {
            println!("  {}: {}", key, value);
        }
    }
    println!();

    // Tile counts
    let total = store.tile_count()?;
    println!("Total tiles: {}", total);

    if total > 0 {
        let total_size = store.total_tile_size()?;
        let avg_size = store.avg_tile_size()?;
        println!("Total tile data: {}", format_bytes(total_size));
        println!("Average tile size: {}", format_bytes(avg_size as u64));
        println!();

        // Per-zoom breakdown
        let by_zoom = store.tile_count_by_zoom()?;
        println!("Tiles by zoom level:");
        println!("  {:>5}  {:>10}", "Zoom", "Tiles");
        println!("  {:>5}  {:>10}", "-----", "----------");
        for (zoom, count) in &by_zoom {
            println!("  {:>5}  {:>10}", zoom, count);
        }
    }

    // File size on disk
    if let Ok(file_meta) = std::fs::metadata(path) {
        println!();
        println!("File size on disk: {}", format_bytes(file_meta.len()));
    }

    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn test_format_bytes_one_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn test_format_bytes_kb_range() {
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn test_format_bytes_one_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn test_format_bytes_mb_range() {
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    #[test]
    fn test_format_bytes_one_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn test_format_bytes_boundary_kb() {
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn test_inspect_mbtiles_with_tiles() {
        let path = std::env::temp_dir()
            .join(format!("tilefeed_inspect_test_{}.mbtiles", std::process::id()))
            .to_string_lossy()
            .to_string();

        let store = crate::mbtiles::MbtilesStore::create(&path).unwrap();
        store.write_default_metadata("test", "A test tileset").unwrap();
        store.put_tile(0, 0, 0, b"tile_data_here").unwrap();
        store.put_tile(1, 0, 0, b"more_data").unwrap();
        drop(store);

        // Should not panic or error
        let result = inspect_mbtiles(&path);
        assert!(result.is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_inspect_mbtiles_empty() {
        let path = std::env::temp_dir()
            .join(format!("tilefeed_inspect_empty_{}.mbtiles", std::process::id()))
            .to_string_lossy()
            .to_string();

        let store = crate::mbtiles::MbtilesStore::create(&path).unwrap();
        drop(store);

        let result = inspect_mbtiles(&path);
        assert!(result.is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_inspect_mbtiles_nonexistent() {
        let result = inspect_mbtiles("/tmp/nonexistent_tilefeed_test.mbtiles");
        assert!(result.is_err());
    }
}
