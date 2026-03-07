use anyhow::{bail, Context, Result};
use tracing::info;

use crate::config::AppConfig;
use crate::postgis::PostgisReader;

/// Generate MBTiles from PostGIS using Tippecanoe
pub async fn generate_full(config: &AppConfig, reader: &PostgisReader) -> Result<()> {
    let temp_dir = std::env::temp_dir().join("postile-export");
    tokio::fs::create_dir_all(&temp_dir).await?;

    let mut geojson_files = Vec::new();

    // Export each layer to GeoJSON
    for layer in &config.tiles.layers {
        let geojson_path = temp_dir
            .join(format!("{}.geojson", layer.name))
            .to_string_lossy()
            .to_string();

        reader.export_layer_geojson(layer, &geojson_path).await?;
        geojson_files.push((layer.clone(), geojson_path));
    }

    // Build Tippecanoe command
    let mbtiles_path = &config.tiles.mbtiles_path;

    let mut cmd = tokio::process::Command::new("tippecanoe");
    cmd.arg("-o").arg(mbtiles_path);
    cmd.arg("--force"); // Overwrite existing
    cmd.arg("--minimum-zoom")
        .arg(config.tiles.min_zoom.to_string());
    cmd.arg("--maximum-zoom")
        .arg(config.tiles.max_zoom.to_string());
    cmd.arg("--no-tile-size-limit");

    for (layer, path) in &geojson_files {
        cmd.arg("--named-layer")
            .arg(format!("{}:{}", layer.name, path));
    }

    info!("Running Tippecanoe: {:?}", cmd);

    let output = cmd
        .output()
        .await
        .context("Failed to run Tippecanoe. Is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Tippecanoe failed: {}", stderr);
    }

    info!("MBTiles generated at {}", mbtiles_path);

    // Clean up temp files
    for (_, path) in &geojson_files {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok(())
}
