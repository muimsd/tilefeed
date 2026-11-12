use anyhow::{bail, Context, Result};
use futures_util::stream::{self, StreamExt};
use std::collections::HashMap;
use tracing::info;

use crate::config::{DerivedGeomType, GenerationBackend, LayerConfig, SourceConfig, TileLayer};
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
        GenerationBackend::OnDemand => prepare_on_demand(source, reader).await,
    }
}

/// "Generation" for an on-demand source: an empty MBTiles with its metadata,
/// `bounds` included. Tiles rendered by an earlier run are dropped with the old
/// file, since the data may have changed while nothing was listening.
async fn prepare_on_demand(source: &SourceConfig, reader: &PostgisReader) -> Result<()> {
    let store = MbtilesStore::create(&source.mbtiles_path)?;
    store.write_default_metadata(
        &source.name,
        &format!("Rendered on demand by tilefeed for {}", source.name),
    )?;
    if let Some(extent) = data_extent(source, reader).await? {
        write_bounds(&store, &extent)?;
    }
    info!(
        "Source '{}' renders tiles on demand (zoom {}-{}); prepared {}",
        source.name, source.min_zoom, source.max_zoom, source.mbtiles_path
    );
    Ok(())
}

/// Union of every layer's extent in EPSG:4326, or `None` when no layer has rows.
/// Derived geometries lie within their source geometry, except points from a
/// label_point_column, which get_layer_extent also covers.
async fn data_extent(
    source: &SourceConfig,
    reader: &PostgisReader,
) -> Result<Option<crate::postgis::Bounds>> {
    let mut extent: Option<crate::postgis::Bounds> = None;
    for layer in &source.layers {
        if let Some(b) = reader.get_layer_extent(layer).await? {
            extent = Some(match extent {
                Some(e) => union_bounds(&e, &b),
                None => b,
            });
        }
    }
    Ok(extent)
}

/// The MBTiles `bounds` metadata, as Tippecanoe writes it. TileJSON (and the
/// map preview) read it to know where the data is.
fn write_bounds(store: &MbtilesStore, extent: &crate::postgis::Bounds) -> Result<()> {
    store.set_metadata(
        "bounds",
        &format!(
            "{},{},{},{}",
            extent.min_lon, extent.min_lat, extent.max_lon, extent.max_lat
        ),
    )
}

/// Encode one tile from PostGIS: every layer (and derived layer) whose zoom
/// range covers it. `None` when the tile has no features.
///
/// Shared by incremental updates and on-demand rendering, so a tile looks the
/// same however it was produced.
pub async fn render_tile(
    source: &SourceConfig,
    reader: &PostgisReader,
    tile: &TileCoord,
) -> Result<Option<Vec<u8>>> {
    let features = fetch_tile_features(reader, tile, &source.tile_layers_at_zoom(tile.z)).await?;
    if features.is_empty() {
        return Ok(None);
    }
    let layer_configs: HashMap<String, &LayerConfig> =
        source.layers.iter().map(|l| (l.name.clone(), l)).collect();
    Ok(Some(mvt::encode_tile_with_config(
        tile,
        &features,
        &layer_configs,
    )?))
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

/// One layer (original or derived) exported to a GeoJSON temp file.
struct ExportedLayer {
    name: String,
    path: String,
    /// The layer's zoom range when narrower than the source's. Derived layers
    /// inherit their parent's.
    zoom_override: Option<(u8, u8)>,
}

/// Export all layers (including derived) to GeoJSON temp files.
///
/// With `stamp_tippecanoe_zoom`, features of a layer with its own zoom range carry
/// Tippecanoe's per-feature `"tippecanoe": {"minzoom", "maxzoom"}` object.
async fn export_all_layers(
    source: &SourceConfig,
    reader: &PostgisReader,
    temp_dir: &std::path::Path,
    stamp_tippecanoe_zoom: bool,
) -> Result<Vec<ExportedLayer>> {
    let mut geojson_files: Vec<ExportedLayer> = Vec::new();

    for layer in &source.layers {
        let zoom_override = source.layer_zoom_override(layer);
        // Only the bounds the layer sets are stamped: a layer with just
        // min_zoom keeps following the tileset upward (e.g. with
        // --extend-zooms-if-still-dropping).
        let stamp = zoom_override
            .filter(|_| stamp_tippecanoe_zoom)
            .map(|_| (layer.min_zoom, layer.max_zoom));
        // Original layer
        let geojson_path = temp_dir
            .join(format!("{}.geojson", layer.name))
            .to_string_lossy()
            .to_string();

        reader
            .export_layer_geojson(layer, &geojson_path, stamp)
            .await?;
        geojson_files.push(ExportedLayer {
            name: layer.name.clone(),
            path: geojson_path,
            zoom_override,
        });

        // Derived label points layer
        if layer.generate_label_points {
            let label_name = format!("{}_labels", layer.name);
            let label_path = temp_dir
                .join(format!("{}.geojson", label_name))
                .to_string_lossy()
                .to_string();

            reader
                .export_derived_layer_geojson(
                    layer,
                    DerivedGeomType::LabelPoint,
                    &label_path,
                    stamp,
                )
                .await?;
            geojson_files.push(ExportedLayer {
                name: label_name,
                path: label_path,
                zoom_override,
            });
        }

        // Derived boundary lines layer
        if layer.generate_boundary_lines {
            let boundary_name = format!("{}_boundary", layer.name);
            let boundary_path = temp_dir
                .join(format!("{}.geojson", boundary_name))
                .to_string_lossy()
                .to_string();

            reader
                .export_derived_layer_geojson(
                    layer,
                    DerivedGeomType::BoundaryLine,
                    &boundary_path,
                    stamp,
                )
                .await?;
            geojson_files.push(ExportedLayer {
                name: boundary_name,
                path: boundary_path,
                zoom_override,
            });
        }
    }

    Ok(geojson_files)
}

/// Clean up temp files
async fn cleanup_temp(geojson_files: &[ExportedLayer], temp_dir: &std::path::Path) {
    for layer in geojson_files {
        let _ = tokio::fs::remove_file(&layer.path).await;
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

    let geojson_files = export_all_layers(source, reader, &temp_dir, true).await?;

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

    for layer in &geojson_files {
        cmd.arg("--named-layer")
            .arg(format!("{}:{}", layer.name, layer.path));
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

    let geojson_files = export_all_layers(source, reader, &temp_dir, false).await?;

    // GDAL's ogr2ogr can write MBTiles directly from GeoJSON inputs
    // We merge all layers into a single MBTiles using ogr2ogr
    let mbtiles_path = &source.mbtiles_path;

    // Remove existing file so ogr2ogr creates fresh
    let _ = tokio::fs::remove_file(mbtiles_path).await;

    for (i, exported) in geojson_files.iter().enumerate() {
        let layer_name = &exported.name;
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
        cmd.arg(&exported.path);

        // Layer naming
        cmd.arg("-nln").arg(layer_name);

        // Zoom range
        cmd.arg("-dsco").arg(format!("MINZOOM={}", source.min_zoom));
        cmd.arg("-dsco").arg(format!("MAXZOOM={}", source.max_zoom));

        // Per-layer zoom range, narrower than the dataset's
        cmd.args(gdal_layer_zoom_args(exported.zoom_override));

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

/// `-lco MINZOOM=… -lco MAXZOOM=…` for a layer with its own zoom range. These are
/// the MBTiles/MVT drivers' layer creation options; without them the layer spans
/// the dataset's `MINZOOM`/`MAXZOOM`.
fn gdal_layer_zoom_args(zoom_override: Option<(u8, u8)>) -> Vec<String> {
    match zoom_override {
        Some((min, max)) => vec![
            "-lco".to_string(),
            format!("MINZOOM={}", min),
            "-lco".to_string(),
            format!("MAXZOOM={}", max),
        ],
        None => Vec::new(),
    }
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

    // Only tiles intersecting the data extent can hold features.
    let Some(extent) = data_extent(source, reader).await? else {
        info!(
            "Native generation complete for source '{}': no features, 0 tiles at {}",
            source.name, source.mbtiles_path
        );
        return Ok(());
    };
    write_bounds(&store, &extent)?;
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
    // parent's, so an empty parent implies empty children, for every layer.
    // A layer that starts deeper than the current zoom is not encoded yet, but
    // a cheap existence check for it still decides where to descend, so the
    // walk never has to fall back to scanning a whole extent.
    let mut level = tiles_for_bounds(&extent, source.min_zoom, source.min_zoom);
    for z in source.min_zoom..=source.max_zoom {
        if level.is_empty() {
            break;
        }
        let candidates = level.len();
        let layer_configs = &layer_configs;
        // Both depend only on z: computed once per zoom, not per tile.
        let active = source.tile_layers_at_zoom(z);
        let pending: Vec<&LayerConfig> = source
            .layers
            .iter()
            .filter(|l| source.layer_zoom_range(l).0 > z)
            .collect();
        let (active, pending) = (&active, &pending);
        let mut results = stream::iter(level.into_iter().map(|tile| async move {
            let features = fetch_tile_features(reader, &tile, active).await?;
            let encoded = if features.is_empty() {
                None
            } else {
                Some(mvt::encode_tile_with_config(
                    &tile,
                    &features,
                    layer_configs,
                )?)
            };
            let expand =
                encoded.is_some() || any_layer_has_features(reader, pending, &tile).await?;
            Ok::<_, anyhow::Error>((tile, encoded, expand))
        }))
        .buffer_unordered(concurrency);

        let mut zoom_tiles = 0u64;
        let mut expand = Vec::new();
        while let Some(result) = results.next().await {
            let (tile, encoded, descend) = result?;
            if let Some(tile_data) = encoded {
                store.put_tile(tile.z, tile.x, tile.y, &tile_data)?;
                zoom_tiles += 1;
            }
            if descend {
                expand.push(tile);
            }
        }
        drop(results);

        info!(
            "Zoom {}: generated {} tiles ({} candidates) for source '{}'",
            z, zoom_tiles, candidates, source.name
        );
        total_tiles += zoom_tiles;

        level = if z < source.max_zoom {
            children_within(&expand, &extent)
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
pub const EXTENT_MARGIN_DEG: f64 = 1e-6;

/// Query the given tile layers (those active at the tile's zoom) for one tile.
async fn fetch_tile_features(
    reader: &PostgisReader,
    tile: &TileCoord,
    tile_layers: &[TileLayer<'_>],
) -> Result<HashMap<String, Vec<crate::postgis::FeatureData>>> {
    let bounds = tile.bounds();
    let mut features_by_layer: HashMap<String, Vec<crate::postgis::FeatureData>> = HashMap::new();

    for tl in tile_layers {
        let features = match tl.derived {
            None => reader.get_features_in_bounds(tl.layer, &bounds).await?,
            Some(derived) => {
                reader
                    .get_derived_features_in_bounds(tl.layer, derived, &bounds)
                    .await?
            }
        };
        if !features.is_empty() {
            features_by_layer.insert(tl.name.clone(), features);
        }
    }

    Ok(features_by_layer)
}

/// Whether any of `layers` has a feature (or label point) in the tile. Used for
/// layers that start deeper, to decide which tiles the walk descends into.
async fn any_layer_has_features(
    reader: &PostgisReader,
    layers: &[&LayerConfig],
    tile: &TileCoord,
) -> Result<bool> {
    let bounds = tile.bounds();
    for layer in layers {
        if reader.layer_has_features_in_bounds(layer, &bounds).await? {
            return Ok(true);
        }
    }
    Ok(false)
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
    fn test_gdal_layer_zoom_args_none_without_override() {
        assert!(gdal_layer_zoom_args(None).is_empty());
    }

    #[test]
    fn test_gdal_layer_zoom_args_with_override() {
        assert_eq!(
            gdal_layer_zoom_args(Some((10, 14))),
            vec!["-lco", "MINZOOM=10", "-lco", "MAXZOOM=14"]
        );
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
