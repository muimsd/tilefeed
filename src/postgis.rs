use anyhow::{Context, Result};
use deadpool_postgres::{Config as PoolConfig, Pool, Runtime};
use serde_json::Value as JsonValue;
use tokio_postgres::NoTls;
use tracing::info;

use crate::config::{DatabaseConfig, LayerConfig};

#[derive(Clone)]
pub struct PostgisReader {
    pool: Pool,
}

impl PostgisReader {
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        let mut pool_cfg = PoolConfig::new();
        pool_cfg.host = Some(config.host.clone());
        pool_cfg.port = Some(config.port);
        pool_cfg.user = Some(config.user.clone());
        pool_cfg.password = Some(config.password.clone());
        pool_cfg.dbname = Some(config.dbname.clone());

        let pool_size = config.pool_size.unwrap_or(4);
        let pool = pool_cfg
            .builder(NoTls)
            .context("Failed to create pool builder")?
            .max_size(pool_size)
            .runtime(Runtime::Tokio1)
            .build()
            .context("Failed to build connection pool")?;

        // Verify connectivity
        let _client = pool.get().await.context("Failed to connect to PostGIS")?;
        info!("Connected to PostGIS (pool_size={})", pool_size);

        Ok(Self { pool })
    }

    /// Export an entire layer as GeoJSON FeatureCollection to a file
    pub async fn export_layer_geojson(&self, layer: &LayerConfig, output_path: &str) -> Result<()> {
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
                .unwrap_or(0);
            let geometry: JsonValue = row.get("geometry");
            let properties: JsonValue = row.get("properties");

            features.push(serde_json::json!({
                "type": "Feature",
                "id": feature_id,
                "geometry": geometry,
                "properties": properties,
            }));
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

        let mut query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON(ST_Transform("{geom_col}", 4326))::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties,
                ST_XMin(ST_Transform(ST_Envelope("{geom_col}"), 4326)) as xmin,
                ST_YMin(ST_Transform(ST_Envelope("{geom_col}"), 4326)) as ymin,
                ST_XMax(ST_Transform(ST_Envelope("{geom_col}"), 4326)) as xmax,
                ST_YMax(ST_Transform(ST_Envelope("{geom_col}"), 4326)) as ymax
            FROM "{schema}"."{table}"
            WHERE "{id_col}" = $1 AND "{geom_col}" IS NOT NULL"#,
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

    /// Connect with retry and exponential backoff
    pub async fn connect_with_retry(config: &DatabaseConfig, max_retries: u32) -> Result<Self> {
        let mut attempt = 0;
        loop {
            match Self::connect(config).await {
                Ok(reader) => return Ok(reader),
                Err(e) => {
                    attempt += 1;
                    if attempt >= max_retries {
                        return Err(e.context(format!("Failed to connect after {} attempts", max_retries)));
                    }
                    let delay = std::time::Duration::from_secs(2u64.pow(attempt.min(5)));
                    tracing::warn!(
                        "PostGIS connection attempt {} failed: {}. Retrying in {:?}...",
                        attempt, e, delay
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
            issues.push(format!("Table \"{}\".\"{}\" does not exist", schema, layer.table));
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
