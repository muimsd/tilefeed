use anyhow::{bail, Context, Result};
use futures_util::stream::{self, StreamExt};
use std::collections::HashMap;
use tracing::info;

use crate::config::{DerivedGeomType, GenerationBackend, SourceConfig};
use crate::mbtiles::MbtilesStore;
use crate::mvt;
use crate::postgis::PostgisReader;
use crate::tiles::{children_within, expand_bounds, tiles_for_bounds, union_bounds, TileCoord};

/// Generate MBTiles for a single source using the configured backend
pub async fn generate_source(
    source: &SourceConfig,
    reader: &PostgisReader,
    tippecanoe_bin: Option<&str>,
    ogr2ogr_bin: Option<&str>,
) -> Result<()> {
    match source.generation_backend {
        GenerationBackend::Tippecanoe => {
            generate_with_tippecanoe(source, reader, tippecanoe_bin).await
        }
        GenerationBackend::Gdal => generate_with_gdal(source, reader, ogr2ogr_bin).await,
        GenerationBackend::Native => generate_with_native(source, reader).await,
    }
}

/// Check if a required external binary is available on PATH or at the given path
pub fn check_binary(name: &str, custom_path: Option<&str>) -> Result<()> {
    let bin = custom_path.unwrap_or(name);
    match std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Ok(()), // some tools return non-zero for --version but still exist
        Err(_) => bail!("'{}' not found. Is {} installed and in PATH?", bin, name),
    }
}

/// Validate that all required external tools are available for the configured backends
pub fn check_required_tools(
    sources: &[SourceConfig],
    tippecanoe_bin: Option<&str>,
    ogr2ogr_bin: Option<&str>,
) -> Result<()> {
    let needs_tippecanoe = sources
        .iter()
        .any(|s| matches!(s.generation_backend, GenerationBackend::Tippecanoe));
    let needs_gdal = sources
        .iter()
        .any(|s| matches!(s.generation_backend, GenerationBackend::Gdal));

    if needs_tippecanoe {
        check_binary("tippecanoe", tippecanoe_bin)?;
    }
    if needs_gdal {
        check_binary("ogr2ogr", ogr2ogr_bin)?;
    }
    Ok(())
}

/// Export all layers (including derived) to GeoJSON temp files.
/// Returns Vec<(layer_name, geojson_path)>.
async fn export_all_layers(
    source: &SourceConfig,
    reader: &PostgisReader,
    temp_dir: &std::path::Path,
) -> Result<Vec<(String, String)>> {
    let mut geojson_files: Vec<(String, String)> = Vec::new();

    for layer in &source.layers {
        // Original layer
        let geojson_path = temp_dir
            .join(format!("{}.geojson", layer.name))
            .to_string_lossy()
            .to_string();

        reader.export_layer_geojson(layer, &geojson_path).await?;
        geojson_files.push((layer.name.clone(), geojson_path));

        // Derived label points layer
        if layer.generate_label_points {
            let label_name = format!("{}_labels", layer.name);
            let label_path = temp_dir
                .join(format!("{}.geojson", label_name))
                .to_string_lossy()
                .to_string();

            reader
                .export_derived_layer_geojson(layer, DerivedGeomType::LabelPoint, &label_path)
                .await?;
            geojson_files.push((label_name, label_path));
        }

        // Derived boundary lines layer
        if layer.generate_boundary_lines {
            let boundary_name = format!("{}_boundary", layer.name);
            let boundary_path = temp_dir
                .join(format!("{}.geojson", boundary_name))
                .to_string_lossy()
                .to_string();

            reader
                .export_derived_layer_geojson(layer, DerivedGeomType::BoundaryLine, &boundary_path)
                .await?;
            geojson_files.push((boundary_name, boundary_path));
        }
    }

    Ok(geojson_files)
}

/// Clean up temp files
async fn cleanup_temp(geojson_files: &[(String, String)], temp_dir: &std::path::Path) {
    for (_, path) in geojson_files {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_dir(temp_dir).await;
}

// ---------------------------------------------------------------------------
// Tippecanoe backend
// ---------------------------------------------------------------------------

async fn generate_with_tippecanoe(
    source: &SourceConfig,
    reader: &PostgisReader,
    tippecanoe_bin: Option<&str>,
) -> Result<()> {
    let temp_dir = std::env::temp_dir().join(format!("tilefeed-export-{}", source.name));
    tokio::fs::create_dir_all(&temp_dir).await?;

    let geojson_files = export_all_layers(source, reader, &temp_dir).await?;

    let bin = tippecanoe_bin.unwrap_or("tippecanoe");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("-o").arg(&source.mbtiles_path);
    cmd.arg("--force");
    cmd.arg("--minimum-zoom").arg(source.min_zoom.to_string());
    cmd.arg("--maximum-zoom").arg(source.max_zoom.to_string());

    let tc = &source.tippecanoe;

    if tc.no_tile_size_limit.unwrap_or(true) {
        cmd.arg("--no-tile-size-limit");
    }
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
    if let Some(rate) = tc.drop_rate {
        cmd.arg("--drop-rate").arg(rate.to_string());
    }
    if let Some(bz) = tc.base_zoom {
        cmd.arg("--base-zoom").arg(bz.to_string());
    }
    if let Some(s) = tc.simplification {
        cmd.arg("--simplification").arg(s.to_string());
    }
    if tc.detect_shared_borders == Some(true) {
        cmd.arg("--detect-shared-borders");
    }
    if tc.no_tiny_polygon_reduction == Some(true) {
        cmd.arg("--no-tiny-polygon-reduction");
    }
    if tc.no_feature_limit == Some(true) {
        cmd.arg("--no-feature-limit");
    }
    if tc.no_tile_compression == Some(true) {
        cmd.arg("--no-tile-compression");
    }
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
    if let Some(extra) = &tc.extra_args {
        for arg in extra {
            cmd.arg(arg);
        }
    }

    for (layer_name, path) in &geojson_files {
        cmd.arg("--named-layer")
            .arg(format!("{}:{}", layer_name, path));
    }

    info!("Running Tippecanoe for source '{}': {:?}", source.name, cmd);

    let output = cmd.output().await.context(format!(
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

    cleanup_temp(&geojson_files, &temp_dir).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// GDAL (ogr2ogr) backend
// ---------------------------------------------------------------------------

async fn generate_with_gdal(
    source: &SourceConfig,
    reader: &PostgisReader,
    ogr2ogr_bin: Option<&str>,
) -> Result<()> {
    let temp_dir = std::env::temp_dir().join(format!("tilefeed-export-{}", source.name));
    tokio::fs::create_dir_all(&temp_dir).await?;

    let geojson_files = export_all_layers(source, reader, &temp_dir).await?;

    // GDAL's ogr2ogr can write MBTiles directly from GeoJSON inputs
    // We merge all layers into a single MBTiles using ogr2ogr
    let mbtiles_path = &source.mbtiles_path;

    // Remove existing file so ogr2ogr creates fresh
    let _ = tokio::fs::remove_file(mbtiles_path).await;

    for (i, (layer_name, geojson_path)) in geojson_files.iter().enumerate() {
        let bin = ogr2ogr_bin.unwrap_or("ogr2ogr");
        let mut cmd = tokio::process::Command::new(bin);

        if i == 0 {
            // First layer: create the MBTiles
            cmd.arg("-f").arg("MBTiles");
        } else {
            // Subsequent layers: append
            cmd.arg("-f").arg("MBTiles").arg("-update").arg("-append");
        }

        cmd.arg(mbtiles_path);
        cmd.arg(geojson_path);

        // Layer naming
        cmd.arg("-nln").arg(layer_name);

        // Zoom range
        cmd.arg("-dsco").arg(format!("MINZOOM={}", source.min_zoom));
        cmd.arg("-dsco").arg(format!("MAXZOOM={}", source.max_zoom));

        // MVT format
        cmd.arg("-dsco").arg("TYPE=overlay");

        info!(
            "Running ogr2ogr for layer '{}' in source '{}': {:?}",
            layer_name, source.name, cmd
        );

        let output = cmd.output().await.context(format!(
            "Failed to run '{}'. Is GDAL installed and in PATH?",
            bin
        ))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "ogr2ogr failed for layer '{}' in source '{}': {}",
                layer_name,
                source.name,
                stderr
            );
        }
    }

    info!(
        "MBTiles generated via GDAL for source '{}' at {}",
        source.name, source.mbtiles_path
    );

    cleanup_temp(&geojson_files, &temp_dir).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Native Rust backend (no external dependencies)
// ---------------------------------------------------------------------------

async fn generate_with_native(source: &SourceConfig, reader: &PostgisReader) -> Result<()> {
    info!(
        "Generating MBTiles natively for source '{}' (zoom {}-{})",
        source.name, source.min_zoom, source.max_zoom
    );

    let store = MbtilesStore::create(&source.mbtiles_path)?;
    store.write_default_metadata(
        &source.name,
        &format!("Generated by tilefeed for {}", source.name),
    )?;

    // Build layer configs map for encode_tile_with_config
    let layer_configs: HashMap<String, &crate::config::LayerConfig> =
        source.layers.iter().map(|l| (l.name.clone(), l)).collect();

    // Only tiles intersecting the data extent (union over layers, EPSG:4326)
    // can hold features. Derived geometries lie within their source geometry,
    // except points from a label_point_column, which get_layer_extent also
    // covers.
    let mut extent: Option<crate::postgis::Bounds> = None;
    for layer in &source.layers {
        if let Some(b) = reader.get_layer_extent(layer).await? {
            extent = Some(match extent {
                Some(e) => union_bounds(&e, &b),
                None => b,
            });
        }
    }
    let Some(extent) = extent else {
        info!(
            "Native generation complete for source '{}': no features, 0 tiles at {}",
            source.name, source.mbtiles_path
        );
        return Ok(());
    };
    // A small margin so features touching the extent edge still reach the
    // neighbouring tile whose border they touch, as a full scan would.
    let extent = expand_bounds(&extent, EXTENT_MARGIN_DEG);
    info!(
        "Data extent for source '{}': [{:.6}, {:.6}, {:.6}, {:.6}]",
        source.name, extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat
    );

    let concurrency = reader.max_connections().max(1);
    let mut total_tiles = 0u64;

    // Walk the quadtree top-down: every tile in the extent at min_zoom, then
    // only children of tiles that had data. A child's envelope lies inside its
    // parent's, so an empty parent implies empty children.
    let mut level = tiles_for_bounds(&extent, source.min_zoom, source.min_zoom);
    for z in source.min_zoom..=source.max_zoom {
        if level.is_empty() {
            break;
        }
        let candidates = level.len();
        let layer_configs = &layer_configs;
        let mut results = stream::iter(level.into_iter().map(|tile| async move {
            let features = fetch_tile_features(source, reader, &tile).await?;
            let encoded = if features.is_empty() {
                None
            } else {
                Some(mvt::encode_tile_with_config(
                    &tile,
                    &features,
                    layer_configs,
                )?)
            };
            Ok::<_, anyhow::Error>((tile, encoded))
        }))
        .buffer_unordered(concurrency);

        let mut non_empty = Vec::new();
        while let Some(result) = results.next().await {
            let (tile, encoded) = result?;
            if let Some(tile_data) = encoded {
                store.put_tile(tile.z, tile.x, tile.y, &tile_data)?;
                non_empty.push(tile);
            }
        }
        drop(results);

        let zoom_tiles = non_empty.len() as u64;
        info!(
            "Zoom {}: generated {} tiles ({} candidates) for source '{}'",
            z, zoom_tiles, candidates, source.name
        );
        total_tiles += zoom_tiles;

        level = if z < source.max_zoom {
            children_within(&non_empty, &extent)
        } else {
            Vec::new()
        };
    }

    info!(
        "Native generation complete for source '{}': {} tiles at {}",
        source.name, total_tiles, source.mbtiles_path
    );

    Ok(())
}

/// Margin (degrees, ~10 cm) added around the data extent before tiling.
const EXTENT_MARGIN_DEG: f64 = 1e-6;

/// Query every layer (and derived layer) of `source` for one tile.
async fn fetch_tile_features(
    source: &SourceConfig,
    reader: &PostgisReader,
    tile: &TileCoord,
) -> Result<HashMap<String, Vec<crate::postgis::FeatureData>>> {
    let bounds = tile.bounds();
    let mut features_by_layer: HashMap<String, Vec<crate::postgis::FeatureData>> = HashMap::new();

    for layer in &source.layers {
        let features = reader.get_features_in_bounds(layer, &bounds).await?;
        if !features.is_empty() {
            features_by_layer.insert(layer.name.clone(), features);
        }

        // Derived label points
        if layer.generate_label_points {
            let label_features = reader
                .get_derived_features_in_bounds(layer, DerivedGeomType::LabelPoint, &bounds)
                .await?;
            if !label_features.is_empty() {
                features_by_layer.insert(format!("{}_labels", layer.name), label_features);
            }
        }

        // Derived boundary lines
        if layer.generate_boundary_lines {
            let boundary_features = reader
                .get_derived_features_in_bounds(layer, DerivedGeomType::BoundaryLine, &bounds)
                .await?;
            if !boundary_features.is_empty() {
                features_by_layer.insert(format!("{}_boundary", layer.name), boundary_features);
            }
        }
    }

    Ok(features_by_layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    fn sample_source(backend: GenerationBackend) -> SourceConfig {
        SourceConfig {
            name: "test".to_string(),
            mbtiles_path: "/tmp/test.mbtiles".to_string(),
            min_zoom: 0,
            max_zoom: 4,
            generation_backend: backend,
            layers: vec![],
            tippecanoe: TippecanoeConfig::default(),
        }
    }

    #[test]
    fn test_check_binary_found() {
        assert!(check_binary("echo", None).is_ok());
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_check_binary_custom_path() {
        assert!(check_binary("echo", Some("/bin/echo")).is_ok());
    }

    #[test]
    fn test_check_binary_not_found() {
        let result = check_binary("nonexistent_tool_xyz_12345", None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("nonexistent_tool_xyz_12345"));
    }

    #[test]
    fn test_check_required_tools_native_needs_nothing() {
        let sources = vec![sample_source(GenerationBackend::Native)];
        assert!(check_required_tools(&sources, None, None).is_ok());
    }

    #[test]
    fn test_check_required_tools_tippecanoe_missing() {
        let sources = vec![sample_source(GenerationBackend::Tippecanoe)];
        let result = check_required_tools(&sources, Some("nonexistent_xyz_12345"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_required_tools_gdal_missing() {
        let sources = vec![sample_source(GenerationBackend::Gdal)];
        let result = check_required_tools(&sources, None, Some("nonexistent_xyz_12345"));
        assert!(result.is_err());
    }

    #[test]
    fn test_check_required_tools_skips_unneeded() {
        // Native backend should not check for tippecanoe or ogr2ogr
        let sources = vec![sample_source(GenerationBackend::Native)];
        assert!(
            check_required_tools(&sources, Some("nonexistent_xyz"), Some("nonexistent_xyz"))
                .is_ok()
        );
    }
}
