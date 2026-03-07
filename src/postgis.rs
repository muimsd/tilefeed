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

        let query = format!(
            r#"SELECT
                "{id_col}" as feature_id,
                ST_AsGeoJSON(ST_Transform("{geom_col}", 4326))::json as geometry,
                row_to_json((SELECT r FROM (SELECT {props_select}) r)) as properties
            FROM "{schema}"."{table}"
            WHERE "{geom_col}" IS NOT NULL"#,
            table = layer.table,
        );

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

        let query = format!(
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

        let query = format!(
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
