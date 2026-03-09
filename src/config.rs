use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub updates: UpdateConfig,
    #[serde(default)]
    pub publish: PublishConfig,
    /// Path to the Tippecanoe binary (default: "tippecanoe", resolved via PATH)
    pub tippecanoe_bin: Option<String>,
    #[serde(default)]
    pub serve: ServeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub dbname: String,
    pub pool_size: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GenerationBackend {
    /// Use Tippecanoe (default, requires tippecanoe binary)
    #[default]
    Tippecanoe,
    /// Use GDAL ogr2ogr (requires ogr2ogr binary)
    Gdal,
    /// Use the built-in native Rust MVT encoder (no external dependencies)
    Native,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub mbtiles_path: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Tile generation backend: "tippecanoe" (default), "gdal", or "native"
    #[serde(default)]
    pub generation_backend: GenerationBackend,
    pub layers: Vec<LayerConfig>,
    #[serde(default)]
    pub tippecanoe: TippecanoeConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TippecanoeConfig {
    // Feature dropping strategies (mutually exclusive in practice)
    /// Drop the densest features to keep tiles under size limit
    pub drop_densest_as_needed: Option<bool>,
    /// Drop a fraction of features to keep tiles under size limit
    pub drop_fraction_as_needed: Option<bool>,
    /// Drop the smallest features to keep tiles under size limit
    pub drop_smallest_as_needed: Option<bool>,
    /// Coalesce the densest features to keep tiles under size limit
    pub coalesce_densest_as_needed: Option<bool>,

    /// Continue tiling to higher zooms if features are still being dropped
    pub extend_zooms_if_still_dropping: Option<bool>,

    // Drop rate control
    /// Rate at which features are dropped at lower zooms (default: 2.5)
    pub drop_rate: Option<f64>,
    /// Base zoom level for the drop rate calculation
    pub base_zoom: Option<u8>,

    // Detail and simplification
    /// Simplification factor (in tile units, e.g. 10)
    pub simplification: Option<f64>,
    /// Detect shared borders between polygons and simplify them identically
    pub detect_shared_borders: Option<bool>,
    /// Don't combine very small polygons into pixels
    pub no_tiny_polygon_reduction: Option<bool>,

    // Tile limits
    /// Don't limit the number of features per tile (default: 200,000)
    pub no_feature_limit: Option<bool>,
    /// Don't limit tile size (currently hardcoded on; set false to use default 500KB limit)
    pub no_tile_size_limit: Option<bool>,
    /// Don't compress tile data (PBF) with gzip
    pub no_tile_compression: Option<bool>,

    // Geometry tuning
    /// Buffer size in pixels around each tile (default: 5)
    pub buffer: Option<u32>,
    /// Detail level at max zoom (default: 12, i.e. 4096 units)
    pub full_detail: Option<u32>,
    /// Detail level at lower zooms (default: 12)
    pub low_detail: Option<u32>,
    /// Detail level below which to drop features (default: equal to low_detail)
    pub minimum_detail: Option<u32>,

    /// Additional raw arguments passed directly to Tippecanoe
    pub extra_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayerConfig {
    pub name: String,
    pub schema: Option<String>,
    pub table: String,
    pub geometry_column: Option<String>,
    pub id_column: Option<String>,
    pub srid: Option<i32>,
    pub properties: Option<Vec<String>>,
    pub filter: Option<String>,
    pub geometry_columns: Option<Vec<String>>,
    /// Douglas-Peucker simplification tolerance in degrees (applied per zoom)
    /// Higher values = more simplification. Tolerance is scaled by 2^(max_zoom - current_zoom).
    pub simplify_tolerance: Option<f64>,
    /// Properties to exclude at zoom levels below this threshold
    /// e.g. `{ below_zoom = 10, exclude = ["description", "metadata"] }`
    pub property_rules: Option<Vec<PropertyRule>>,
    /// Automatically generate a centroid point layer for labels.
    /// Creates a companion layer named `{name}_labels` with Point geometry
    /// derived from ST_PointOnSurface of the polygon.
    #[serde(default)]
    pub generate_label_points: bool,
    /// Automatically generate a boundary polyline layer.
    /// Creates a companion layer named `{name}_boundary` with LineString geometry
    /// derived from ST_Boundary of the polygon.
    #[serde(default)]
    pub generate_boundary_lines: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PropertyRule {
    /// Zoom level below which properties are excluded
    pub below_zoom: u8,
    /// Properties to exclude below the threshold
    pub exclude: Vec<String>,
}

impl LayerConfig {
    /// Get all geometry columns (supports both single and multiple)
    pub fn geometry_columns(&self) -> Vec<String> {
        if let Some(cols) = &self.geometry_columns {
            cols.clone()
        } else {
            vec![self.geometry_column.as_deref().unwrap_or("geom").to_string()]
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConfig {
    pub debounce_ms: Option<u64>,
    pub worker_concurrency: Option<usize>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            debounce_ms: Some(200),
            worker_concurrency: Some(8),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PublishBackend {
    #[default]
    None,
    Local,
    S3,
    Mapbox,
    Command,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublishConfig {
    #[serde(default)]
    pub backend: PublishBackend,
    pub destination: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub mapbox_tileset_id: Option<String>,
    pub mapbox_token: Option<String>,
    pub publish_on_generate: Option<bool>,
    pub publish_on_update: Option<bool>,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            backend: PublishBackend::None,
            destination: None,
            command: None,
            args: None,
            mapbox_tileset_id: None,
            mapbox_token: None,
            publish_on_generate: Some(true),
            publish_on_update: Some(true),
        }
    }
}

impl PublishConfig {
    pub fn publish_on_generate_enabled(&self) -> bool {
        self.publish_on_generate.unwrap_or(true)
    }

    pub fn publish_on_update_enabled(&self) -> bool {
        self.publish_on_update.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServeConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub cors_origins: Option<Vec<String>>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            host: Some("127.0.0.1".to_string()),
            port: Some(3000),
            cors_origins: None,
        }
    }
}

impl DatabaseConfig {
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.host, self.port, self.user, self.password, self.dbname
        )
    }
}

impl AppConfig {
    /// Find which source owns a given layer name (including derived layers like _labels, _boundary)
    pub fn find_source_for_layer(&self, layer_name: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| {
            s.layers.iter().any(|l| {
                l.name == layer_name
                    || (l.generate_label_points && format!("{}_labels", l.name) == layer_name)
                    || (l.generate_boundary_lines && format!("{}_boundary", l.name) == layer_name)
            })
        })
    }
}

impl SourceConfig {
    /// Find a layer by name within this source
    pub fn find_layer(&self, name: &str) -> Option<&LayerConfig> {
        self.layers.iter().find(|l| l.name == name)
    }

    /// Find the parent layer that owns a derived layer (e.g. "parks" for "parks_labels")
    pub fn find_parent_layer_for_derived(&self, derived_name: &str) -> Option<&LayerConfig> {
        self.layers.iter().find(|l| {
            (l.generate_label_points && format!("{}_labels", l.name) == derived_name)
                || (l.generate_boundary_lines && format!("{}_boundary", l.name) == derived_name)
        })
    }

    /// Get all effective layer names including derived layers
    pub fn all_layer_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for layer in &self.layers {
            names.push(layer.name.clone());
            if layer.generate_label_points {
                names.push(format!("{}_labels", layer.name));
            }
            if layer.generate_boundary_lines {
                names.push(format!("{}_boundary", layer.name));
            }
        }
        names
    }
}

/// Type of derived geometry to generate from a polygon layer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedGeomType {
    /// ST_PointOnSurface — centroid point for label placement
    LabelPoint,
    /// ST_Boundary — polygon boundary as linestring
    BoundaryLine,
}

pub fn load_config(path: &str) -> anyhow::Result<AppConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name(path))
        .add_source(config::Environment::with_prefix("TILES"))
        .build()?;

    let cfg: AppConfig = settings.try_deserialize()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layer(name: &str) -> LayerConfig {
        LayerConfig {
            name: name.to_string(),
            schema: None,
            table: format!("{}_table", name),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: None,
            generate_label_points: false,
            generate_boundary_lines: false,
        }
    }

    fn sample_source(name: &str, layer_names: &[&str]) -> SourceConfig {
        SourceConfig {
            name: name.to_string(),
            mbtiles_path: format!("/tmp/{}.mbtiles", name),
            min_zoom: 0,
            max_zoom: 14,
            generation_backend: GenerationBackend::default(),
            layers: layer_names.iter().map(|l| sample_layer(l)).collect(),
            tippecanoe: TippecanoeConfig::default(),
        }
    }

    fn sample_config(sources: Vec<SourceConfig>) -> AppConfig {
        AppConfig {
            database: DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                user: "postgres".to_string(),
                password: "secret".to_string(),
                dbname: "testdb".to_string(),
                pool_size: None,
            },
            sources,
            updates: UpdateConfig::default(),
            publish: PublishConfig::default(),
            tippecanoe_bin: None,
            serve: ServeConfig::default(),
        }
    }

    // --- find_source_for_layer tests ---

    #[test]
    fn test_find_source_for_layer_found() {
        let config = sample_config(vec![
            sample_source("source_a", &["buildings", "roads"]),
            sample_source("source_b", &["water", "land"]),
        ]);

        let result = config.find_source_for_layer("water");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "source_b");
    }

    #[test]
    fn test_find_source_for_layer_first_source() {
        let config = sample_config(vec![
            sample_source("source_a", &["buildings", "roads"]),
            sample_source("source_b", &["water"]),
        ]);

        let result = config.find_source_for_layer("buildings");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "source_a");
    }

    #[test]
    fn test_find_source_for_layer_not_found() {
        let config = sample_config(vec![sample_source("source_a", &["buildings"])]);

        let result = config.find_source_for_layer("nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_source_for_layer_empty_sources() {
        let config = sample_config(vec![]);

        let result = config.find_source_for_layer("anything");
        assert!(result.is_none());
    }

    // --- SourceConfig::find_layer tests ---

    #[test]
    fn test_find_layer_found() {
        let source = sample_source("src", &["buildings", "roads", "water"]);

        let result = source.find_layer("roads");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "roads");
    }

    #[test]
    fn test_find_layer_not_found() {
        let source = sample_source("src", &["buildings"]);

        let result = source.find_layer("missing");
        assert!(result.is_none());
    }

    #[test]
    fn test_find_layer_empty_layers() {
        let source = sample_source("src", &[]);

        let result = source.find_layer("anything");
        assert!(result.is_none());
    }

    // --- PublishConfig defaults ---

    #[test]
    fn test_publish_config_defaults() {
        let config = PublishConfig::default();

        assert!(matches!(config.backend, PublishBackend::None));
        assert_eq!(config.destination, None);
        assert_eq!(config.command, None);
        assert_eq!(config.args, None);
        assert_eq!(config.publish_on_generate, Some(true));
        assert_eq!(config.publish_on_update, Some(true));
    }

    #[test]
    fn test_publish_on_generate_enabled_default() {
        let config = PublishConfig::default();
        assert!(config.publish_on_generate_enabled());
    }

    #[test]
    fn test_publish_on_generate_enabled_explicit_false() {
        let config = PublishConfig {
            publish_on_generate: Some(false),
            ..PublishConfig::default()
        };
        assert!(!config.publish_on_generate_enabled());
    }

    #[test]
    fn test_publish_on_update_enabled_default() {
        let config = PublishConfig::default();
        assert!(config.publish_on_update_enabled());
    }

    #[test]
    fn test_publish_on_update_enabled_none() {
        let config = PublishConfig {
            publish_on_update: None,
            ..PublishConfig::default()
        };
        // None defaults to true
        assert!(config.publish_on_update_enabled());
    }

    // --- UpdateConfig defaults ---

    #[test]
    fn test_update_config_defaults() {
        let config = UpdateConfig::default();
        assert_eq!(config.debounce_ms, Some(200));
        assert_eq!(config.worker_concurrency, Some(8));
    }

    // --- DatabaseConfig::connection_string tests ---

    #[test]
    fn test_connection_string_format() {
        let db = DatabaseConfig {
            host: "db.example.com".to_string(),
            port: 5433,
            user: "admin".to_string(),
            password: "p@ss".to_string(),
            dbname: "mydb".to_string(),
            pool_size: Some(10),
        };

        let conn_str = db.connection_string();
        assert_eq!(
            conn_str,
            "host=db.example.com port=5433 user=admin password=p@ss dbname=mydb"
        );
    }

    #[test]
    fn test_connection_string_default_port() {
        let db = DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "".to_string(),
            dbname: "tiles".to_string(),
            pool_size: None,
        };

        let conn_str = db.connection_string();
        assert!(conn_str.contains("host=localhost"));
        assert!(conn_str.contains("port=5432"));
        assert!(conn_str.contains("user=postgres"));
        assert!(conn_str.contains("dbname=tiles"));
    }

    // --- ServeConfig defaults ---

    #[test]
    fn test_serve_config_defaults() {
        let config = ServeConfig::default();
        assert_eq!(config.host, Some("127.0.0.1".to_string()));
        assert_eq!(config.port, Some(3000));
        assert_eq!(config.cors_origins, None);
    }

    // --- GenerationBackend defaults ---

    #[test]
    fn test_generation_backend_default_is_tippecanoe() {
        let backend = GenerationBackend::default();
        assert!(matches!(backend, GenerationBackend::Tippecanoe));
    }

    // --- find_source_for_layer with derived layers ---

    #[test]
    fn test_find_source_for_label_layer() {
        let mut source = sample_source("src", &["parks"]);
        source.layers[0].generate_label_points = true;
        let config = sample_config(vec![source]);

        let result = config.find_source_for_layer("parks_labels");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "src");
    }

    #[test]
    fn test_find_source_for_boundary_layer() {
        let mut source = sample_source("src", &["parks"]);
        source.layers[0].generate_boundary_lines = true;
        let config = sample_config(vec![source]);

        let result = config.find_source_for_layer("parks_boundary");
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "src");
    }

    #[test]
    fn test_find_source_for_derived_layer_not_enabled() {
        let source = sample_source("src", &["parks"]);
        let config = sample_config(vec![source]);

        assert!(config.find_source_for_layer("parks_labels").is_none());
        assert!(config.find_source_for_layer("parks_boundary").is_none());
    }

    // --- SourceConfig::find_parent_layer_for_derived ---

    #[test]
    fn test_find_parent_layer_for_labels() {
        let mut source = sample_source("src", &["parks", "water"]);
        source.layers[0].generate_label_points = true;

        let parent = source.find_parent_layer_for_derived("parks_labels");
        assert!(parent.is_some());
        assert_eq!(parent.unwrap().name, "parks");
    }

    #[test]
    fn test_find_parent_layer_for_boundary() {
        let mut source = sample_source("src", &["parks"]);
        source.layers[0].generate_boundary_lines = true;

        let parent = source.find_parent_layer_for_derived("parks_boundary");
        assert!(parent.is_some());
        assert_eq!(parent.unwrap().name, "parks");
    }

    #[test]
    fn test_find_parent_layer_for_derived_not_found() {
        let source = sample_source("src", &["parks"]);
        assert!(source.find_parent_layer_for_derived("parks_labels").is_none());
        assert!(source.find_parent_layer_for_derived("nonexistent_labels").is_none());
    }

    // --- SourceConfig::all_layer_names ---

    #[test]
    fn test_all_layer_names_no_derived() {
        let source = sample_source("src", &["buildings", "roads"]);
        let names = source.all_layer_names();
        assert_eq!(names, vec!["buildings", "roads"]);
    }

    #[test]
    fn test_all_layer_names_with_derived() {
        let mut source = sample_source("src", &["parks", "water"]);
        source.layers[0].generate_label_points = true;
        source.layers[0].generate_boundary_lines = true;
        source.layers[1].generate_label_points = true;

        let names = source.all_layer_names();
        assert_eq!(
            names,
            vec!["parks", "parks_labels", "parks_boundary", "water", "water_labels"]
        );
    }

    #[test]
    fn test_all_layer_names_empty() {
        let source = sample_source("src", &[]);
        let names = source.all_layer_names();
        assert!(names.is_empty());
    }

    // --- LayerConfig::geometry_columns ---

    #[test]
    fn test_geometry_columns_default() {
        let layer = sample_layer("test");
        let cols = layer.geometry_columns();
        assert_eq!(cols, vec!["geom".to_string()]);
    }

    #[test]
    fn test_geometry_columns_single_override() {
        let mut layer = sample_layer("test");
        layer.geometry_column = Some("the_geom".to_string());
        let cols = layer.geometry_columns();
        assert_eq!(cols, vec!["the_geom".to_string()]);
    }

    #[test]
    fn test_geometry_columns_multiple() {
        let mut layer = sample_layer("test");
        layer.geometry_columns = Some(vec!["geom".to_string(), "geom_simplified".to_string()]);
        let cols = layer.geometry_columns();
        assert_eq!(cols, vec!["geom".to_string(), "geom_simplified".to_string()]);
    }

    // --- PublishBackend ---

    #[test]
    fn test_publish_backend_default_is_none() {
        let backend = PublishBackend::default();
        assert!(matches!(backend, PublishBackend::None));
    }

    // --- SourceConfig::find_layer edge cases ---

    #[test]
    fn test_find_layer_does_not_match_derived_names() {
        let mut source = sample_source("src", &["parks"]);
        source.layers[0].generate_label_points = true;

        // find_layer only matches actual layer names, not derived
        assert!(source.find_layer("parks_labels").is_none());
        assert!(source.find_layer("parks").is_some());
    }
}
