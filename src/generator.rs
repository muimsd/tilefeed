use anyhow::{bail, Context, Result};
use tracing::info;

use crate::config::SourceConfig;
use crate::postgis::PostgisReader;

/// Generate MBTiles for a single source using Tippecanoe
pub async fn generate_source(
    source: &SourceConfig,
    reader: &PostgisReader,
    tippecanoe_bin: Option<&str>,
) -> Result<()> {
    let temp_dir = std::env::temp_dir().join(format!("tilefeed-export-{}", source.name));
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
    let bin = tippecanoe_bin.unwrap_or("tippecanoe");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("-o").arg(&source.mbtiles_path);
    cmd.arg("--force"); // Overwrite existing
    cmd.arg("--minimum-zoom").arg(source.min_zoom.to_string());
    cmd.arg("--maximum-zoom").arg(source.max_zoom.to_string());

    let tc = &source.tippecanoe;

    // Tile size limit: default on unless explicitly disabled
    if tc.no_tile_size_limit.unwrap_or(true) {
        cmd.arg("--no-tile-size-limit");
    }

    // Feature dropping strategies
    if tc.drop_densest_as_needed == Some(true) {
        cmd.arg("--drop-densest-as-needed");
    }
    if tc.drop_fraction_as_needed == Some(true) {
        cmd.arg("--drop-fraction-as-needed");
    }
    if tc.drop_smallest_as_needed == Some(true) {
        cmd.arg("--drop-smallest-as-needed");
    }
    if tc.coalesce_densest_as_needed == Some(true) {
        cmd.arg("--coalesce-densest-as-needed");
    }
    if tc.extend_zooms_if_still_dropping == Some(true) {
        cmd.arg("--extend-zooms-if-still-dropping");
    }

    // Drop rate control
    if let Some(rate) = tc.drop_rate {
        cmd.arg("--drop-rate").arg(rate.to_string());
    }
    if let Some(bz) = tc.base_zoom {
        cmd.arg("--base-zoom").arg(bz.to_string());
    }

    // Detail and simplification
    if let Some(s) = tc.simplification {
        cmd.arg("--simplification").arg(s.to_string());
    }
    if tc.detect_shared_borders == Some(true) {
        cmd.arg("--detect-shared-borders");
    }
    if tc.no_tiny_polygon_reduction == Some(true) {
        cmd.arg("--no-tiny-polygon-reduction");
    }

    // Tile limits
    if tc.no_feature_limit == Some(true) {
        cmd.arg("--no-feature-limit");
    }
    if tc.no_tile_compression == Some(true) {
        cmd.arg("--no-tile-compression");
    }

    // Geometry tuning
    if let Some(b) = tc.buffer {
        cmd.arg("--buffer").arg(b.to_string());
    }
    if let Some(d) = tc.full_detail {
        cmd.arg("--full-detail").arg(d.to_string());
    }
    if let Some(d) = tc.low_detail {
        cmd.arg("--low-detail").arg(d.to_string());
    }
    if let Some(d) = tc.minimum_detail {
        cmd.arg("--minimum-detail").arg(d.to_string());
    }

    // Extra raw arguments
    if let Some(extra) = &tc.extra_args {
        for arg in extra {
            cmd.arg(arg);
        }
    }

    for (layer, path) in &geojson_files {
        cmd.arg("--named-layer")
            .arg(format!("{}:{}", layer.name, path));
    }

    info!("Running Tippecanoe for source '{}': {:?}", source.name, cmd);

    let output = cmd
        .output()
        .await
        .context(format!(
            "Failed to run '{}'. Is it installed and in PATH?",
            bin
        ))?;

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
