use anyhow::{Context, Result};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::config::{DatabaseConfig, DerivedGeomType, LayerConfig};

/// Build one GeoJSON feature for export. With `tippecanoe_zoom`, the feature
/// carries Tippecanoe's per-feature zoom object (`"tippecanoe": {"minzoom",
/// "maxzoom"}`), which Tippecanoe honours and other GeoJSON readers ignore.
fn geojson_feature(
    id: i64,
    geometry: JsonValue,
    properties: JsonValue,
    tippecanoe_zoom: Option<(u8, u8)>,
) -> JsonValue {
    let mut feature = serde_json::json!({
        "type": "Feature",
        "id": id,
        "geometry": geometry,
        "properties": properties,
    });
    if let Some((minzoom, maxzoom)) = tippecanoe_zoom {
        feature["tippecanoe"] = serde_json::json!({ "minzoom": minzoom, "maxzoom": maxzoom });
    }
    feature
}

#[derive(Clone)]
pub struct PostgisReader {
    pool: Pool,
}

impl PostgisReader {
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        let tls = crate::pgtls::make_tls_connector(config)?;
        let manager = Manager::from_config(
            config.pg_config(),
            tls,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );

        let pool_size = config.pool_size.unwrap_or(4);
        let pool = Pool::builder(manager)
            .max_size(pool_size)
            .runtime(Runtime::Tokio1)
            .build()
            .context("Failed to build connection pool")?;

        // Verify connectivity
        let _client = pool.get().await.with_context(|| {
            format!(
                "Failed to connect to PostGIS (sslmode={})",
                config.sslmode.as_str()
            )
        })?;
        info!(
            "Connected to PostGIS (pool_size={}, sslmode={})",
            pool_size,
            config.sslmode.as_str()
        );

        Ok(Self { pool })
    }

    /// Export an entire layer as GeoJSON FeatureCollection to a file.
    ///
    /// `tippecanoe_zoom` stamps every feature with a Tippecanoe
    /// `{"minzoom", "maxzoom"}` object, restricting the layer to that range.
    pub async fn export_layer_geojson(
        &self,
        layer: &LayerConfig,
        output_path: &str,
        tippecanoe_zoom: Option<(u8, u8)>,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");
        let id_col = layer.id_column.as_deref().unwrap_or("id");

        let props_select = match &layer.properties {
            Some(props) => props
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", "),
            None => "*".to_string(),
        };

        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON(ST_Transform("{geom_col}", 4326))::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties
            FROM "{schema}"."{table}"
            WHERE "{geom_col}" IS NOT NULL"#,
            table = layer.table,
        );

        if let Some(ref filter) = layer.filter {
            query.push_str(&format!(" AND ({})", filter));
        }

        let rows = client
            .query(&query, &[])
            .await
            .with_context(|| format!("Failed to query layer {}.{}", schema, layer.table))?;

        let mut features = Vec::with_capacity(rows.len());
        for row in &rows {
            let feature_id: i64 = row
                .try_get::<_, i64>("feature_id")
                .or_else(|_| row.try_get::<_, i32>("feature_id").map(|v| v as i64))
                .or_else(|_| row.try_get::<_, i16>("feature_id").map(|v| v as i64))
                .unwrap_or(0);
            let geometry: JsonValue = row.get("geometry");
            let properties: JsonValue = row.get("properties");

            features.push(geojson_feature(
                feature_id,
                geometry,
                properties,
                tippecanoe_zoom,
            ));
        }

        let feature_collection = serde_json::json!({
            "type": "FeatureCollection",
            "features": features,
        });

        tokio::fs::write(output_path, serde_json::to_string(&feature_collection)?)
            .await
            .with_context(|| format!("Failed to write GeoJSON to {}", output_path))?;

        info!(
            "Exported {} features from {}.{} to {}",
            features.len(),
            schema,
            layer.table,
            output_path
        );

        Ok(())
    }

    /// Get a single feature's geometry as GeoJSON and its bounding box
    pub async fn get_feature(
        &self,
        layer: &LayerConfig,
        feature_id: i64,
    ) -> Result<Option<FeatureData>> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");
        let id_col = layer.id_column.as_deref().unwrap_or("id");

        let props_select = match &layer.properties {
            Some(props) => props
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", "),
            None => "*".to_string(),
        };

        // Affected tiles must cover the label point too, which a
        // label_point_column may place outside the geometry.
        let bounds_geom = match layer.active_label_column() {
            Some(label_col) => format!(
                r#"ST_Envelope(ST_Collect(ST_Transform("{geom_col}", 4326), ST_Transform(COALESCE("{label_col}", "{geom_col}"), 4326)))"#
            ),
            None => format!(r#"ST_Transform(ST_Envelope("{geom_col}"), 4326)"#),
        };
        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON(ST_Transform("{geom_col}", 4326))::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties,
                ST_XMin({bounds_geom}) as xmin,
                ST_YMin({bounds_geom}) as ymin,
                ST_XMax({bounds_geom}) as xmax,
                ST_YMax({bounds_geom}) as ymax
            FROM "{schema}"."{table}"
            WHERE "{id_col}" = $1::bigint AND "{geom_col}" IS NOT NULL"#,
            table = layer.table,
        );

        if let Some(ref filter) = layer.filter {
            query.push_str(&format!(" AND ({})", filter));
        }

        let row = client.query_opt(&query, &[&feature_id]).await?;

        match row {
            Some(row) => {
                let geometry: JsonValue = row.get("geometry");
                let properties: JsonValue = row.get("properties");
                let bounds = Bounds {
                    min_lon: row.get("xmin"),
                    min_lat: row.get("ymin"),
                    max_lon: row.get("xmax"),
                    max_lat: row.get("ymax"),
                };

                Ok(Some(FeatureData {
                    id: feature_id,
                    geometry,
                    properties,
                    bounds,
                    layer_name: layer.name.clone(),
                }))
            }
            None => Ok(None),
        }
    }

    /// Maximum number of pooled connections (upper bound for useful query concurrency).
    pub fn max_connections(&self) -> usize {
        self.pool.status().max_size
    }

    /// Extent of a layer's (filtered) geometries in EPSG:4326, or `None` if the
    /// layer has no rows. Geometries are transformed before aggregating so the
    /// box is exact rather than a reprojected source-SRID box.
    pub async fn get_layer_extent(&self, layer: &LayerConfig) -> Result<Option<Bounds>> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");

        // A label_point_column may place points outside the polygons'
        // extent, so it widens the extent too.
        let extent_geom = match layer.active_label_column() {
            Some(label_col) => format!(
                r#"ST_Collect(ST_Transform("{geom_col}", 4326), ST_Transform(COALESCE("{label_col}", "{geom_col}"), 4326))"#
            ),
            None => format!(r#"ST_Transform("{geom_col}", 4326)"#),
        };
        let mut inner = format!(
            r#"SELECT ST_Extent({extent_geom}) AS e
            FROM "{schema}"."{table}"
            WHERE "{geom_col}" IS NOT NULL"#,
            table = layer.table,
        );
        if let Some(ref filter) = layer.filter {
            inner.push_str(&format!(" AND ({})", filter));
        }
        let query = format!(
            "SELECT ST_XMin(e), ST_YMin(e), ST_XMax(e), ST_YMax(e) FROM ({}) s",
            inner
        );

        let row = client
            .query_one(&query, &[])
            .await
            .with_context(|| format!("Failed to compute extent of layer '{}'", layer.name))?;
        let min_lon: Option<f64> = row.get(0);
        let min_lat: Option<f64> = row.get(1);
        let max_lon: Option<f64> = row.get(2);
        let max_lat: Option<f64> = row.get(3);
        Ok(match (min_lon, min_lat, max_lon, max_lat) {
            (Some(min_lon), Some(min_lat), Some(max_lon), Some(max_lat)) => Some(Bounds {
                min_lon,
                min_lat,
                max_lon,
                max_lat,
            }),
            _ => None,
        })
    }

    /// Get all features intersecting a given tile bounding box
    pub async fn get_features_in_bounds(
        &self,
        layer: &LayerConfig,
        bounds: &Bounds,
    ) -> Result<Vec<FeatureData>> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");
        let id_col = layer.id_column.as_deref().unwrap_or("id");

        let props_select = match &layer.properties {
            Some(props) => props
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", "),
            None => "*".to_string(),
        };

        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON(ST_Transform("{geom_col}", 4326))::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties
            FROM "{schema}"."{table}"
            WHERE "{geom_col}" IS NOT NULL
              AND ST_Intersects(
                  ST_Transform("{geom_col}", 4326),
                  ST_MakeEnvelope($1, $2, $3, $4, 4326)
              )"#,
            table = layer.table,
        );

        if let Some(ref filter) = layer.filter {
            query.push_str(&format!(" AND ({})", filter));
        }

        let rows = client
            .query(
                &query,
                &[
                    &bounds.min_lon,
                    &bounds.min_lat,
                    &bounds.max_lon,
                    &bounds.max_lat,
                ],
            )
            .await?;

        let mut features = Vec::with_capacity(rows.len());
        for row in &rows {
            let feature_id: i64 = row
                .try_get::<_, i64>("feature_id")
                .or_else(|_| row.try_get::<_, i32>("feature_id").map(|v| v as i64))
                .or_else(|_| row.try_get::<_, i16>("feature_id").map(|v| v as i64))
                .unwrap_or(0);
            let geometry: JsonValue = row.get("geometry");
            let properties: JsonValue = row.get("properties");

            features.push(FeatureData {
                id: feature_id,
                geometry,
                properties,
                bounds: bounds.clone(),
                layer_name: layer.name.clone(),
            });
        }

        Ok(features)
    }

    /// Export a derived geometry layer (labels/boundary) as GeoJSON FeatureCollection.
    /// `tippecanoe_zoom` works as in [`Self::export_layer_geojson`].
    pub async fn export_derived_layer_geojson(
        &self,
        layer: &LayerConfig,
        derived_type: DerivedGeomType,
        output_path: &str,
        tippecanoe_zoom: Option<(u8, u8)>,
    ) -> Result<()> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");
        let id_col = layer.id_column.as_deref().unwrap_or("id");

        let props_select = match &layer.properties {
            Some(props) => props
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", "),
            None => "*".to_string(),
        };

        let geom_transform = match derived_type {
            DerivedGeomType::LabelPoint => layer.label_point_sql(geom_col),
            DerivedGeomType::BoundaryLine => {
                format!("ST_Boundary(ST_Transform(\"{geom_col}\", 4326))")
            }
        };
        let src_col = layer.derived_source_column(derived_type, geom_col);

        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON({geom_transform})::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties
            FROM "{schema}"."{table}"
            WHERE "{src_col}" IS NOT NULL"#,
            table = layer.table,
        );

        if let Some(ref filter) = layer.filter {
            query.push_str(&format!(" AND ({})", filter));
        }

        let rows = client.query(&query, &[]).await.with_context(|| {
            format!(
                "Failed to query derived {:?} layer {}.{}",
                derived_type, schema, layer.table
            )
        })?;

        let mut features = Vec::with_capacity(rows.len());
        for row in &rows {
            let feature_id: i64 = row
                .try_get::<_, i64>("feature_id")
                .or_else(|_| row.try_get::<_, i32>("feature_id").map(|v| v as i64))
                .or_else(|_| row.try_get::<_, i16>("feature_id").map(|v| v as i64))
                .unwrap_or(0);
            let geometry: JsonValue = row.get("geometry");
            let properties: JsonValue = row.get("properties");

            features.push(geojson_feature(
                feature_id,
                geometry,
                properties,
                tippecanoe_zoom,
            ));
        }

        let feature_collection = serde_json::json!({
            "type": "FeatureCollection",
            "features": features,
        });

        tokio::fs::write(output_path, serde_json::to_string(&feature_collection)?)
            .await
            .with_context(|| format!("Failed to write derived GeoJSON to {}", output_path))?;

        let type_name = match derived_type {
            DerivedGeomType::LabelPoint => "label points",
            DerivedGeomType::BoundaryLine => "boundary lines",
        };
        info!(
            "Exported {} {} from {}.{} to {}",
            features.len(),
            type_name,
            schema,
            layer.table,
            output_path
        );

        Ok(())
    }

    /// Get derived geometry features (labels/boundary) intersecting a tile bounding box
    pub async fn get_derived_features_in_bounds(
        &self,
        layer: &LayerConfig,
        derived_type: DerivedGeomType,
        bounds: &Bounds,
    ) -> Result<Vec<FeatureData>> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let geom_col = layer.geometry_column.as_deref().unwrap_or("geom");
        let id_col = layer.id_column.as_deref().unwrap_or("id");

        let props_select = match &layer.properties {
            Some(props) => props
                .iter()
                .map(|p| format!("\"{}\"", p))
                .collect::<Vec<_>>()
                .join(", "),
            None => "*".to_string(),
        };

        let geom_transform = match derived_type {
            DerivedGeomType::LabelPoint => layer.label_point_sql(geom_col),
            DerivedGeomType::BoundaryLine => {
                format!("ST_Boundary(ST_Transform(\"{geom_col}\", 4326))")
            }
        };
        let src_col = layer.derived_source_column(derived_type, geom_col);

        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON({geom_transform})::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties
            FROM "{schema}"."{table}"
            WHERE "{src_col}" IS NOT NULL
              AND ST_Intersects(
                  ST_Transform("{src_col}", 4326),
                  ST_MakeEnvelope($1, $2, $3, $4, 4326)
              )"#,
            table = layer.table,
        );

        if let Some(ref filter) = layer.filter {
            query.push_str(&format!(" AND ({})", filter));
        }

        let rows = client
            .query(
                &query,
                &[
                    &bounds.min_lon,
                    &bounds.min_lat,
                    &bounds.max_lon,
                    &bounds.max_lat,
                ],
            )
            .await?;

        let derived_layer_name = match derived_type {
            DerivedGeomType::LabelPoint => format!("{}_labels", layer.name),
            DerivedGeomType::BoundaryLine => format!("{}_boundary", layer.name),
        };

        let mut features = Vec::with_capacity(rows.len());
        for row in &rows {
            let feature_id: i64 = row
                .try_get::<_, i64>("feature_id")
                .or_else(|_| row.try_get::<_, i32>("feature_id").map(|v| v as i64))
                .or_else(|_| row.try_get::<_, i16>("feature_id").map(|v| v as i64))
                .unwrap_or(0);
            let geometry: JsonValue = row.get("geometry");
            let properties: JsonValue = row.get("properties");

            features.push(FeatureData {
                id: feature_id,
                geometry,
                properties,
                bounds: bounds.clone(),
                layer_name: derived_layer_name.clone(),
            });
        }

        Ok(features)
    }

    /// Connect with retry and exponential backoff
    pub async fn connect_with_retry(config: &DatabaseConfig, max_retries: u32) -> Result<Self> {
        let mut attempt = 0;
        loop {
            match Self::connect(config).await {
                Ok(reader) => return Ok(reader),
                Err(e) => {
                    attempt += 1;
                    if attempt >= max_retries {
                        return Err(
                            e.context(format!("Failed to connect after {} attempts", max_retries))
                        );
                    }
                    let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(5)));
                    tracing::warn!(
                        "PostGIS connection attempt {} failed: {}. Retrying in {:?}...",
                        attempt,
                        e,
                        delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Check if a layer's table and columns exist in the database
    pub async fn validate_layer(&self, layer: &LayerConfig) -> Result<Vec<String>> {
        let client = self.pool.get().await?;
        let schema = layer.schema.as_deref().unwrap_or("public");
        let mut issues = Vec::new();

        // Check table exists
        let table_exists: bool = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2)",
                &[&schema, &layer.table],
            )
            .await?
            .get(0);

        if !table_exists {
            issues.push(format!(
                "Table \"{}\".\"{}\" does not exist",
                schema, layer.table
            ));
            return Ok(issues);
        }

        // Check geometry column(s) exist
        for geom_col in layer.geometry_columns() {
            let col_exists: bool = client
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 AND column_name = $3)",
                    &[&schema, &layer.table, &geom_col],
                )
                .await?
                .get(0);
            if !col_exists {
                issues.push(format!(
                    "Geometry column \"{}\" does not exist in \"{}\".\"{}\"",
                    geom_col, schema, layer.table
                ));
            }
        }

        // Check id column exists
        let id_col = layer.id_column.as_deref().unwrap_or("id");
        let id_exists: bool = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 AND column_name = $3)",
                &[&schema, &layer.table, &id_col],
            )
            .await?
            .get(0);
        if !id_exists {
            issues.push(format!(
                "ID column \"{}\" does not exist in \"{}\".\"{}\"",
                id_col, schema, layer.table
            ));
        }

        // Check property columns exist
        if let Some(ref props) = layer.properties {
            for prop in props {
                let prop_exists: bool = client
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 AND column_name = $3)",
                        &[&schema, &layer.table, &prop],
                    )
                    .await?
                    .get(0);
                if !prop_exists {
                    issues.push(format!(
                        "Property column \"{}\" does not exist in \"{}\".\"{}\"",
                        prop, schema, layer.table
                    ));
                }
            }
        }

        // Check if NOTIFY trigger is installed
        let trigger_exists: bool = client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM information_schema.triggers WHERE event_object_schema = $1 AND event_object_table = $2 AND trigger_name LIKE '%tile_update%')",
                &[&schema, &layer.table],
            )
            .await?
            .get(0);
        if !trigger_exists {
            issues.push(format!(
                "No tile_update trigger found on \"{}\".\"{}\"",
                schema, layer.table
            ));
        }

        Ok(issues)
    }
}

#[derive(Debug, Clone)]
pub struct Bounds {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

#[derive(Debug, Clone)]
pub struct FeatureData {
    pub id: i64,
    pub geometry: JsonValue,
    pub properties: JsonValue,
    pub bounds: Bounds,
    pub layer_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geojson_feature_without_zoom_range_has_no_tippecanoe_member() {
        let f = geojson_feature(
            7,
            serde_json::json!({"type": "Point", "coordinates": [0.0, 0.0]}),
            serde_json::json!({"name": "a"}),
            None,
        );
        assert_eq!(f["type"], "Feature");
        assert_eq!(f["id"], 7);
        assert_eq!(f["properties"]["name"], "a");
        assert!(f.get("tippecanoe").is_none());
    }

    #[test]
    fn test_geojson_feature_with_zoom_range_carries_tippecanoe_object() {
        let f = geojson_feature(
            1,
            serde_json::json!({"type": "Point", "coordinates": [0.0, 0.0]}),
            serde_json::json!({}),
            Some((10, 14)),
        );
        assert_eq!(f["tippecanoe"]["minzoom"], 10);
        assert_eq!(f["tippecanoe"]["maxzoom"], 14);
        // The zoom object is a feature member, never a property
        assert!(f["properties"].get("tippecanoe").is_none());
    }
}
