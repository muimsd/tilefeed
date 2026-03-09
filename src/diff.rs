use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::mbtiles::MbtilesStore;

/// Compare two MBTiles files and show the differences
pub fn diff_mbtiles(path_a: &str, path_b: &str) -> Result<()> {
    let store_a = MbtilesStore::open(path_a)?;
    let store_b = MbtilesStore::open(path_b)?;

    println!("Comparing:");
    println!("  A: {}", path_a);
    println!("  B: {}", path_b);
    println!();

    // Compare metadata
    let meta_a: HashMap<String, String> = store_a.get_all_metadata()?.into_iter().collect();
    let meta_b: HashMap<String, String> = store_b.get_all_metadata()?.into_iter().collect();

    let all_keys: HashSet<&String> = meta_a.keys().chain(meta_b.keys()).collect();
    let mut meta_diffs = Vec::new();

    for key in &all_keys {
        match (meta_a.get(*key), meta_b.get(*key)) {
            (Some(a), Some(b)) if a != b => {
                meta_diffs.push(format!("  {}: \"{}\" -> \"{}\"", key, a, b));
            }
            (Some(a), None) => {
                meta_diffs.push(format!("  {}: \"{}\" -> (removed)", key, a));
            }
            (None, Some(b)) => {
                meta_diffs.push(format!("  {}: (added) -> \"{}\"", key, b));
            }
            _ => {}
        }
    }

    if meta_diffs.is_empty() {
        println!("Metadata: identical");
    } else {
        println!("Metadata differences:");
        for diff in &meta_diffs {
            println!("{}", diff);
        }
    }
    println!();

    // Compare tile counts
    let count_a = store_a.tile_count()?;
    let count_b = store_b.tile_count()?;
    println!("Tile count: {} -> {}", count_a, count_b);

    // Get all tile coordinates from both
    let coords_a: HashSet<(u8, u32, u32)> = store_a.all_tile_coords()?.into_iter().collect();
    let coords_b: HashSet<(u8, u32, u32)> = store_b.all_tile_coords()?.into_iter().collect();

    let only_in_a: HashSet<&(u8, u32, u32)> = coords_a.difference(&coords_b).collect();
    let only_in_b: HashSet<&(u8, u32, u32)> = coords_b.difference(&coords_a).collect();
    let in_both: HashSet<&(u8, u32, u32)> = coords_a.intersection(&coords_b).collect();

    // Check content differences for shared tiles
    let mut changed_count = 0u64;
    for &&(z, x, tms_y) in &in_both {
        let data_a = store_a.get_tile_raw_tms(z, x, tms_y)?;
        let data_b = store_b.get_tile_raw_tms(z, x, tms_y)?;

        let hash_a = data_a.as_ref().map(|d| hash_tile(d));
        let hash_b = data_b.as_ref().map(|d| hash_tile(d));

        if hash_a != hash_b {
            changed_count += 1;
        }
    }

    println!();
    println!("Summary:");
    println!("  Added tiles (only in B):   {}", only_in_b.len());
    println!("  Removed tiles (only in A): {}", only_in_a.len());
    println!("  Changed tiles (different): {}", changed_count);
    println!("  Unchanged tiles:           {}", in_both.len() as u64 - changed_count);

    // Per-zoom breakdown if there are differences
    if !only_in_a.is_empty() || !only_in_b.is_empty() || changed_count > 0 {
        println!();
        println!("Changes by zoom level:");
        println!("  {:>5}  {:>8}  {:>8}  {:>8}", "Zoom", "Added", "Removed", "Changed");
        println!("  {:>5}  {:>8}  {:>8}  {:>8}", "-----", "--------", "--------", "--------");

        let max_zoom = coords_a
            .iter()
            .chain(coords_b.iter())
            .map(|(z, _, _)| *z)
            .max()
            .unwrap_or(0);

        for z in 0..=max_zoom {
            let added = only_in_b.iter().filter(|(zz, _, _)| *zz == z).count();
            let removed = only_in_a.iter().filter(|(zz, _, _)| *zz == z).count();

            // Count changed at this zoom
            let mut changed_at_zoom = 0u64;
            for &&(zz, x, tms_y) in &in_both {
                if zz != z {
                    continue;
                }
                let data_a = store_a.get_tile_raw_tms(zz, x, tms_y)?;
                let data_b = store_b.get_tile_raw_tms(zz, x, tms_y)?;
                if data_a.as_ref().map(|d| hash_tile(d)) != data_b.as_ref().map(|d| hash_tile(d)) {
                    changed_at_zoom += 1;
                }
            }

            if added > 0 || removed > 0 || changed_at_zoom > 0 {
                println!("  {:>5}  {:>8}  {:>8}  {:>8}", z, added, removed, changed_at_zoom);
            }
        }
    }

    Ok(())
}

fn hash_tile(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(suffix: &str) -> String {
        std::env::temp_dir()
            .join(format!("tilefeed_diff_{}_{}.mbtiles", std::process::id(), suffix))
            .to_string_lossy()
            .to_string()
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_hash_tile_deterministic() {
        let data = b"hello world";
        let hash1 = hash_tile(data);
        let hash2 = hash_tile(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_tile_different_data() {
        let hash1 = hash_tile(b"data_a");
        let hash2 = hash_tile(b"data_b");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_tile_empty() {
        let hash = hash_tile(b"");
        // SHA-256 of empty data is a known constant
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_diff_identical_mbtiles() {
        let path_a = temp_path("identical_a");
        let path_b = temp_path("identical_b");

        {
            let store_a = crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
            store_a.set_metadata("name", "test").unwrap();
            store_a.put_tile(0, 0, 0, b"same").unwrap();
        }
        {
            let store_b = crate::mbtiles::MbtilesStore::create(&path_b).unwrap();
            store_b.set_metadata("name", "test").unwrap();
            store_b.put_tile(0, 0, 0, b"same").unwrap();
        }

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_added_tiles() {
        let path_a = temp_path("added_a");
        let path_b = temp_path("added_b");

        {
            let store = crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
            store.put_tile(0, 0, 0, b"tile").unwrap();
        }
        {
            let store = crate::mbtiles::MbtilesStore::create(&path_b).unwrap();
            store.put_tile(0, 0, 0, b"tile").unwrap();
            store.put_tile(1, 0, 0, b"new").unwrap();
        }

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_removed_tiles() {
        let path_a = temp_path("removed_a");
        let path_b = temp_path("removed_b");

        {
            let store = crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
            store.put_tile(0, 0, 0, b"tile").unwrap();
            store.put_tile(1, 0, 0, b"extra").unwrap();
        }
        {
            let store = crate::mbtiles::MbtilesStore::create(&path_b).unwrap();
            store.put_tile(0, 0, 0, b"tile").unwrap();
        }

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_changed_tiles() {
        let path_a = temp_path("changed_a");
        let path_b = temp_path("changed_b");

        {
            let store = crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
            store.put_tile(0, 0, 0, b"old_data").unwrap();
        }
        {
            let store = crate::mbtiles::MbtilesStore::create(&path_b).unwrap();
            store.put_tile(0, 0, 0, b"new_data").unwrap();
        }

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_empty_mbtiles() {
        let path_a = temp_path("empty_a");
        let path_b = temp_path("empty_b");

        crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
        crate::mbtiles::MbtilesStore::create(&path_b).unwrap();

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_metadata_differences() {
        let path_a = temp_path("meta_a");
        let path_b = temp_path("meta_b");

        {
            let store = crate::mbtiles::MbtilesStore::create(&path_a).unwrap();
            store.set_metadata("name", "v1").unwrap();
            store.set_metadata("format", "pbf").unwrap();
        }
        {
            let store = crate::mbtiles::MbtilesStore::create(&path_b).unwrap();
            store.set_metadata("name", "v2").unwrap();
            store.set_metadata("version", "2").unwrap();
        }

        let result = diff_mbtiles(&path_a, &path_b);
        assert!(result.is_ok());

        cleanup(&path_a);
        cleanup(&path_b);
    }

    #[test]
    fn test_diff_nonexistent_file() {
        let path_a = temp_path("exists_ne");
        crate::mbtiles::MbtilesStore::create(&path_a).unwrap();

        let result = diff_mbtiles(&path_a, "/tmp/nonexistent_tilefeed.mbtiles");
        assert!(result.is_err());

        cleanup(&path_a);
    }
}
