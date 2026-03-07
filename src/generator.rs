use anyhow::{bail, Context, Result};
use tracing::info;

use crate::config::SourceConfig;
use crate::postgis::PostgisReader;

/// Generate MBTiles for a single source using Tippecanoe
pub async fn generate_source(source: &SourceConfig, reader: &PostgisReader) -> Result<()> {
    let temp_dir = std::env::temp_dir().join(format!("postile-export-{}", source.name));
    tokio::fs::create_dir_all(&temp_dir).await?;

    let mut geojson_files = Vec::new();

    // Export each layer to GeoJSON
    for layer in &source.layers {
        let geojson_path = temp_dir
            .join(format!("{}.geojson", layer.name))
            .to_string_lossy()
            .to_string();

        reader.export_layer_geojson(layer, &geojson_path).await?;
        geojson_files.push((layer.clone(), geojson_path));
    }

    // Build Tippecanoe command
    let mut cmd = tokio::process::Command::new("tippecanoe");
    cmd.arg("-o").arg(&source.mbtiles_path);
    cmd.arg("--force"); // Overwrite existing
    cmd.arg("--minimum-zoom").arg(source.min_zoom.to_string());
    cmd.arg("--maximum-zoom").arg(source.max_zoom.to_string());
    cmd.arg("--no-tile-size-limit");

    for (layer, path) in &geojson_files {
        cmd.arg("--named-layer")
            .arg(format!("{}:{}", layer.name, path));
    }

    info!("Running Tippecanoe for source '{}': {:?}", source.name, cmd);

    let output = cmd
        .output()
        .await
        .context("Failed to run Tippecanoe. Is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Tippecanoe failed for source '{}': {}", source.name, stderr);
    }

    info!(
        "MBTiles generated for source '{}' at {}",
        source.name, source.mbtiles_path
    );

    // Clean up temp files
    for (_, path) in &geojson_files {
        let _ = tokio::fs::remove_file(path).await;
    }

    Ok(())
}
