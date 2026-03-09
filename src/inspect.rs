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
