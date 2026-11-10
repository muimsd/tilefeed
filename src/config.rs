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
    /// Path to the ogr2ogr binary (default: "ogr2ogr", resolved via PATH)
    pub ogr2ogr_bin: Option<String>,
    #[serde(default)]
    pub serve: ServeConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub dbname: String,
    pub pool_size: Option<usize>,
    /// TLS mode for PostgreSQL connections, with libpq semantics:
    /// "disable" | "prefer" (default) | "require" | "verify-ca" | "verify-full".
    #[serde(default)]
    pub sslmode: SslMode,
    /// PEM file with the CA certificate(s) used to verify the server in
    /// "verify-ca"/"verify-full" mode (and, as in libpq, in "require" mode
    /// when set). Required for "verify-ca"; for "verify-full" the bundled
    /// Mozilla (webpki) roots are used when unset.
    #[serde(default)]
    pub sslrootcert: Option<String>,
}

/// PostgreSQL TLS mode. Mirrors libpq's `sslmode` values (except "allow").
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SslMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server supports it, otherwise fall back to plaintext.
    /// The certificate is not verified. This is libpq's default and keeps
    /// configs written before `sslmode` existed working against non-TLS servers.
    /// Unlike libpq, a failed TLS handshake is an error, not a plaintext retry.
    #[default]
    Prefer,
    /// Always use TLS; fail if the server refuses it. The certificate is not
    /// verified unless `sslrootcert` is set (then behaves like "verify-ca").
    Require,
    /// Always use TLS and verify the certificate chain, but not the hostname.
    VerifyCa,
    /// Always use TLS, verify the certificate chain and that the hostname matches.
    VerifyFull,
}

impl SslMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }

    /// The handshake mode tokio-postgres should use. Verification is handled
    /// separately by the rustls connector.
    pub fn to_postgres(self) -> tokio_postgres::config::SslMode {
        match self {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
                tokio_postgres::config::SslMode::Require
            }
        }
    }
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

/// How a polygon's label point is derived.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LabelPointMethod {
    /// ST_PointOnSurface: always inside the polygon, the right choice for labels (default).
    #[default]
    PointOnSurface,
    /// ST_Centroid: the true center of mass. Can fall outside concave or multi-part polygons.
    Centroid,
}

impl LayerConfig {
    /// SQL expression for this layer's label point, in EPSG:4326.
    /// The column a derived layer's rows are drawn from: `label_point_column`
    /// for label points when set, the geometry column otherwise. Derived
    /// queries filter on this column (non-null, intersecting the tile), so a
    /// hand-placed label point lands in the tile that contains it.
    pub fn derived_source_column<'a>(
        &'a self,
        derived_type: DerivedGeomType,
        geom_col: &'a str,
    ) -> &'a str {
        match (derived_type, &self.label_point_column) {
            (DerivedGeomType::LabelPoint, Some(col)) if self.generate_label_points => col,
            _ => geom_col,
        }
    }

    /// The label point column, when label points are generated from one.
    pub fn active_label_column(&self) -> Option<&str> {
        if self.generate_label_points {
            self.label_point_column.as_deref()
        } else {
            None
        }
    }

    pub fn label_point_sql(&self, geom_col: &str) -> String {
        if let Some(col) = &self.label_point_column {
            return self.col_4326(col);
        }
        let geom = self.col_4326(geom_col);
        match self.label_point_method {
            LabelPointMethod::PointOnSurface => format!("ST_PointOnSurface({geom})"),
            LabelPointMethod::Centroid => format!("ST_Centroid({geom})"),
        }
    }

    /// The geometry expression `expr` in EPSG:4326. Geometries stored with
    /// an SRID are transformed from it; ones stored without (SRID 0, as
    /// OGR_FDW and `shp2pgsql` without `-s` leave them) are read as `srid`
    /// when the layer sets one. Without `srid` PostGIS rejects those
    /// ("unknown (0) SRID"). `srid` never overrides a stored SRID, so a
    /// copied `srid = 4326` can't misread a table in another projection.
    pub fn to_4326(&self, expr: &str) -> String {
        match self.srid {
            Some(srid) => format!(
                "ST_Transform(CASE WHEN ST_SRID({expr}) = 0 THEN ST_SetSRID({expr}, {srid}) ELSE {expr} END, 4326)"
            ),
            None => format!("ST_Transform({expr}, 4326)"),
        }
    }

    /// `to_4326` of the column `col`.
    pub fn col_4326(&self, col: &str) -> String {
        self.to_4326(&format!("\"{col}\""))
    }
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
    /// Automatically generate a point layer for labels.
    /// Creates a companion layer named `{name}_labels` with one Point per feature,
    /// computed with `label_point_method` or read from `label_point_column`.
    #[serde(default)]
    pub generate_label_points: bool,
    /// How label points are computed (default `point_on_surface`).
    #[serde(default)]
    pub label_point_method: LabelPointMethod,
    /// Use this existing point column of the table as the label point instead of computing one
    /// (e.g. a hand-placed `label_geom`). Overrides `label_point_method`.
    #[serde(default)]
    pub label_point_column: Option<String>,
    /// Automatically generate a boundary polyline layer.
    /// Creates a companion layer named `{name}_boundary` with LineString geometry
    /// derived from ST_Boundary of the polygon.
    #[serde(default)]
    pub generate_boundary_lines: bool,
    /// Lowest zoom this layer (and its derived layers) appears at. Must lie within
    /// the source's zoom range; defaults to the source's `min_zoom`.
    pub min_zoom: Option<u8>,
    /// Highest zoom this layer (and its derived layers) appears at. Must lie within
    /// the source's zoom range; defaults to the source's `max_zoom`.
    pub max_zoom: Option<u8>,
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
            vec![self
                .geometry_column
                .as_deref()
                .unwrap_or("geom")
                .to_string()]
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
    /// `Cache-Control: max-age` sent with tiles, in seconds (default: 300).
    /// `0` sends `no-cache`: browsers revalidate every tile with its ETag, so
    /// updated tiles show up immediately and unchanged ones cost a 304.
    pub max_age_secs: Option<u64>,
    /// In-memory tile cache (`[serve.cache]`).
    #[serde(default)]
    pub cache: TileCacheConfig,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            host: Some("127.0.0.1".to_string()),
            port: Some(3000),
            cors_origins: None,
            max_age_secs: None,
            cache: TileCacheConfig::default(),
        }
    }
}

/// In-memory tile cache for the HTTP server. Tiles changed by incremental
/// updates or a full generation are cleared from it as they are written.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TileCacheConfig {
    /// Cache tiles in memory (default: true)
    pub enabled: Option<bool>,
    /// Size budget in megabytes; least recently used tiles are evicted beyond it
    /// (default: 64)
    pub max_size_mb: Option<u64>,
    /// Expire cached tiles after this many seconds even if unchanged, for MBTiles
    /// written by another process (default: unset, entries live until changed
    /// or evicted)
    pub ttl_secs: Option<u64>,
}

impl TileCacheConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true) && self.max_bytes() > 0
    }

    pub fn max_bytes(&self) -> usize {
        (self.max_size_mb.unwrap_or(64) as usize).saturating_mul(1024 * 1024)
    }

    pub fn ttl(&self) -> Option<std::time::Duration> {
        match self.ttl_secs {
            Some(0) | None => None,
            Some(s) => Some(std::time::Duration::from_secs(s)),
        }
    }
}

impl ServeConfig {
    pub fn host(&self) -> &str {
        self.host.as_deref().unwrap_or("127.0.0.1")
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(3000)
    }

    pub fn cache_control(&self) -> String {
        match self.max_age_secs.unwrap_or(300) {
            0 => "no-cache".to_string(),
            secs => format!("public, max-age={}", secs),
        }
    }
}

/// Paths the tile server already owns; mounting metrics on one of them would
/// panic axum at startup, so the config is rejected instead.
pub const RESERVED_SERVER_PATHS: &[&str] = &["/health", "/events"];

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetricsConfig {
    /// Expose Prometheus metrics (default: true)
    pub enabled: Option<bool>,
    /// Path the metrics are served under (default: "/metrics")
    pub path: Option<String>,
    /// Host for the dedicated metrics listener (default: the `[serve]` host)
    pub host: Option<String>,
    /// Port for a dedicated metrics listener. Metrics move off the tile server
    /// onto it, and it is the only way to expose metrics from `watch` and `run`,
    /// which have no tile server of their own.
    pub port: Option<u16>,
}

impl MetricsConfig {
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Metrics path, normalized to always start with `/`.
    pub fn path(&self) -> String {
        match self
            .path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(p) if p.starts_with('/') => p.to_string(),
            Some(p) => format!("/{}", p),
            None => "/metrics".to_string(),
        }
    }
}

impl AppConfig {
    /// Address of the dedicated metrics listener, when `[metrics] port` names one.
    /// The host falls back to the `[serve]` host.
    pub fn metrics_addr(&self) -> Option<(String, u16)> {
        if !self.metrics.enabled() {
            return None;
        }
        let port = self.metrics.port?;
        let host = self
            .metrics
            .host
            .as_deref()
            .unwrap_or_else(|| self.serve.host())
            .to_string();
        Some((host, port))
    }

    /// Whether that dedicated address is the socket the tile server already binds,
    /// in which case the two would collide and the tile server serves metrics.
    pub fn metrics_addr_is_serve_addr(&self) -> bool {
        self.metrics_addr().is_some_and(|(host, port)| {
            (host.as_str(), port) == (self.serve.host(), self.serve.port())
        })
    }

    /// Whether the tile server itself should serve the metrics path. Metrics live
    /// in exactly one place: on a separate listener when one is configured, on the
    /// tile server otherwise — so setting `[metrics] port` really does take the
    /// scrape endpoint off the tile port.
    pub fn metrics_on_tile_server(&self) -> bool {
        self.metrics.enabled()
            && (self.metrics_addr().is_none() || self.metrics_addr_is_serve_addr())
    }
}

impl DatabaseConfig {
    /// Build a tokio-postgres config. Values are set field by field, so
    /// passwords containing spaces or quotes need no escaping.
    pub fn pg_config(&self) -> tokio_postgres::Config {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.host)
            .port(self.port)
            .user(&self.user)
            .password(&self.password)
            .dbname(&self.dbname)
            .ssl_mode(self.effective_ssl_mode());
        cfg
    }

    /// A host starting with `/` is a Unix-socket directory. PostgreSQL does
    /// not offer TLS over Unix sockets and libpq ignores `sslmode` for them,
    /// so TLS is disabled there whatever `sslmode` says.
    pub fn is_unix_socket(&self) -> bool {
        self.host.starts_with('/')
    }

    fn effective_ssl_mode(&self) -> tokio_postgres::config::SslMode {
        if self.is_unix_socket() {
            tokio_postgres::config::SslMode::Disable
        } else {
            self.sslmode.to_postgres()
        }
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
    /// Effective zoom range of a layer: its own `min_zoom`/`max_zoom`, falling back
    /// to the source's. Derived layers (`_labels`, `_boundary`) share their
    /// parent's range.
    pub fn layer_zoom_range(&self, layer: &LayerConfig) -> (u8, u8) {
        (
            layer.min_zoom.unwrap_or(self.min_zoom),
            layer.max_zoom.unwrap_or(self.max_zoom),
        )
    }

    /// The layer's zoom range when it is narrower than the source's, i.e. when a
    /// generator has to restrict it explicitly. `None` means "every source zoom".
    pub fn layer_zoom_override(&self, layer: &LayerConfig) -> Option<(u8, u8)> {
        let range = self.layer_zoom_range(layer);
        (range != (self.min_zoom, self.max_zoom)).then_some(range)
    }

    /// Whether a layer (and its derived layers) is encoded into tiles at zoom `z`.
    pub fn layer_applies_at_zoom(&self, layer: &LayerConfig, z: u8) -> bool {
        let (min, max) = self.layer_zoom_range(layer);
        (min..=max).contains(&z)
    }

    /// Layers to encode into tiles at zoom `z`.
    pub fn layers_at_zoom(&self, z: u8) -> impl Iterator<Item = &LayerConfig> {
        self.layers
            .iter()
            .filter(move |l| self.layer_applies_at_zoom(l, z))
    }

    /// Every tile layer (original and derived) to query and encode for a tile at
    /// zoom `z`, in encoding order. Derived layers follow their parent's range.
    pub fn tile_layers_at_zoom(&self, z: u8) -> Vec<TileLayer<'_>> {
        let mut out = Vec::new();
        for layer in self.layers_at_zoom(z) {
            out.push(TileLayer {
                name: layer.name.clone(),
                layer,
                derived: None,
            });
            if layer.generate_label_points {
                out.push(TileLayer {
                    name: format!("{}_labels", layer.name),
                    layer,
                    derived: Some(DerivedGeomType::LabelPoint),
                });
            }
            if layer.generate_boundary_lines {
                out.push(TileLayer {
                    name: format!("{}_boundary", layer.name),
                    layer,
                    derived: Some(DerivedGeomType::BoundaryLine),
                });
            }
        }
        out
    }

    /// Check the per-layer zoom ranges against each other and the source's range.
    pub fn validate_layer_zooms(&self) -> anyhow::Result<()> {
        for layer in &self.layers {
            let (min, max) = self.layer_zoom_range(layer);
            if let Some(z) = layer.min_zoom {
                if z < self.min_zoom || z > self.max_zoom {
                    anyhow::bail!(
                        "layer '{}' in source '{}': min_zoom {} is outside the source's zoom range {}..={}",
                        layer.name, self.name, z, self.min_zoom, self.max_zoom
                    );
                }
            }
            if let Some(z) = layer.max_zoom {
                if z < self.min_zoom || z > self.max_zoom {
                    anyhow::bail!(
                        "layer '{}' in source '{}': max_zoom {} is outside the source's zoom range {}..={}",
                        layer.name, self.name, z, self.min_zoom, self.max_zoom
                    );
                }
            }
            if min > max {
                anyhow::bail!(
                    "layer '{}' in source '{}': min_zoom {} is greater than max_zoom {}",
                    layer.name,
                    self.name,
                    min,
                    max
                );
            }
        }
        Ok(())
    }

    /// Find a layer by name within this source
    pub fn find_layer(&self, name: &str) -> Option<&LayerConfig> {
        self.layers.iter().find(|l| l.name == name)
    }

    /// Find the parent layer that owns a derived layer (e.g. "parks" for "parks_labels")
    #[cfg(test)]
    pub fn find_parent_layer_for_derived(&self, derived_name: &str) -> Option<&LayerConfig> {
        self.layers.iter().find(|l| {
            (l.generate_label_points && format!("{}_labels", l.name) == derived_name)
                || (l.generate_boundary_lines && format!("{}_boundary", l.name) == derived_name)
        })
    }

    /// Get all effective layer names including derived layers
    #[cfg(test)]
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

/// One layer of a tile: an original layer, or a layer derived from one.
#[derive(Debug, Clone)]
pub struct TileLayer<'a> {
    /// Name of the layer inside the tile (`parks`, `parks_labels`, ...)
    pub name: String,
    /// The configured layer the features come from
    pub layer: &'a LayerConfig,
    /// `None` for the layer itself, otherwise the derived geometry
    pub derived: Option<DerivedGeomType>,
}

/// Type of derived geometry to generate from a polygon layer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedGeomType {
    /// ST_PointOnSurface — centroid point for label placement
    LabelPoint,
    /// ST_Boundary — polygon boundary as linestring
    BoundaryLine,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookConfig {
    /// URLs to send POST notifications to
    #[serde(default)]
    pub urls: Vec<String>,
    /// HTTP timeout in milliseconds (default: 5000)
    pub timeout_ms: Option<u64>,
    /// Number of retries on failure (default: 2)
    pub retry_count: Option<u32>,
    /// Send webhook on full tile generation completion
    pub on_generate: Option<bool>,
    /// Send webhook on incremental tile update completion
    pub on_update: Option<bool>,
    /// HMAC-SHA256 secret for signing payloads
    pub secret: Option<String>,
    /// Minimum seconds between notifications per source (default: 0 = no cooldown).
    /// During rapid updates, events are accumulated and a single aggregated
    /// notification is sent after the cooldown expires.
    pub cooldown_secs: Option<u64>,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            timeout_ms: Some(5000),
            retry_count: Some(2),
            on_generate: Some(true),
            on_update: Some(true),
            secret: None,
            cooldown_secs: None,
        }
    }
}

impl WebhookConfig {
    pub fn on_generate_enabled(&self) -> bool {
        self.on_generate.unwrap_or(true)
    }

    pub fn on_update_enabled(&self) -> bool {
        self.on_update.unwrap_or(true)
    }

    pub fn is_configured(&self) -> bool {
        !self.urls.is_empty()
    }
}

pub fn load_config(path: &str) -> anyhow::Result<AppConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name(path))
        .add_source(config::Environment::with_prefix("TILES"))
        .build()?;

    let cfg: AppConfig = settings.try_deserialize()?;
    cfg.validate()?;
    Ok(cfg)
}

impl AppConfig {
    /// Semantic checks serde cannot express. Run by `load_config`.
    pub fn validate(&self) -> anyhow::Result<()> {
        for source in &self.sources {
            source.validate_layer_zooms()?;
        }
        check_label_point_columns(self)?;
        Ok(())
    }
}

/// `label_point_column` is spliced into SQL as a quoted identifier, so only plain identifiers are allowed.
fn check_label_point_columns(cfg: &AppConfig) -> anyhow::Result<()> {
    for source in &cfg.sources {
        for layer in &source.layers {
            if let Some(col) = &layer.label_point_column {
                let ok =
                    !col.is_empty() && col.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !ok {
                    anyhow::bail!(
                        "layer \"{}\": label_point_column \"{}\" must be a plain column name (letters, digits, underscore)",
                        layer.name,
                        col
                    );
                }
            }
        }
    }
    Ok(())
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
            min_zoom: None,
            max_zoom: None,
            label_point_method: Default::default(),
            label_point_column: None,
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
                sslmode: SslMode::default(),
                sslrootcert: None,
            },
            sources,
            updates: UpdateConfig::default(),
            publish: PublishConfig::default(),
            tippecanoe_bin: None,
            ogr2ogr_bin: None,
            serve: ServeConfig::default(),
            webhook: WebhookConfig::default(),
            metrics: MetricsConfig::default(),
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

    // --- [serve.cache] ---

    #[test]
    fn test_tile_cache_config_defaults_and_parse() {
        let defaults = TileCacheConfig::default();
        assert!(defaults.enabled());
        assert_eq!(defaults.max_bytes(), 64 * 1024 * 1024);
        assert_eq!(defaults.ttl(), None);

        let serve: ServeConfig =
            toml_value("max_age_secs = 0\n[cache]\nmax_size_mb = 8\nttl_secs = 30");
        assert_eq!(serve.cache_control(), "no-cache");
        assert_eq!(serve.cache.max_bytes(), 8 * 1024 * 1024);
        assert_eq!(serve.cache.ttl(), Some(std::time::Duration::from_secs(30)));

        let off: ServeConfig = toml_value("[cache]\nenabled = false");
        assert!(!off.cache.enabled());
        let zero: ServeConfig = toml_value("[cache]\nmax_size_mb = 0");
        assert!(!zero.cache.enabled());
    }

    fn toml_value<T: serde::de::DeserializeOwned>(toml: &str) -> T {
        config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    // --- DatabaseConfig::pg_config ---

    #[test]
    fn test_unix_socket_host_disables_tls() {
        let mut db = parse_database("sslmode = \"require\"").unwrap();
        db.host = "/var/run/postgresql".to_string();
        assert!(db.is_unix_socket());
        assert_eq!(
            db.pg_config().get_ssl_mode(),
            tokio_postgres::config::SslMode::Disable
        );
        db.host = "db.example.com".to_string();
        assert!(!db.is_unix_socket());
        assert_eq!(
            db.pg_config().get_ssl_mode(),
            tokio_postgres::config::SslMode::Require
        );
    }

    // --- label point method / column ---

    fn parse_layer(extra: &str) -> LayerConfig {
        let toml = format!(
            "name = \"parcels\"\ntable = \"parcels\"\ngenerate_label_points = true\n{}",
            extra
        );
        config::Config::builder()
            .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    #[test]
    fn test_label_point_method_defaults_to_point_on_surface() {
        let l = parse_layer("");
        assert_eq!(l.label_point_method, LabelPointMethod::PointOnSurface);
        assert_eq!(
            l.label_point_sql("geom"),
            "ST_PointOnSurface(ST_Transform(\"geom\", 4326))"
        );
    }

    #[test]
    fn test_to_4326_without_srid_transforms_the_stored_srid() {
        let layer = parse_layer("");
        assert_eq!(layer.col_4326("geom"), r#"ST_Transform("geom", 4326)"#);
    }

    #[test]
    fn test_to_4326_with_srid_only_assigns_it_to_srid_zero() {
        let layer = parse_layer("srid = 3857");
        assert_eq!(
            layer.col_4326("geom"),
            r#"ST_Transform(CASE WHEN ST_SRID("geom") = 0 THEN ST_SetSRID("geom", 3857) ELSE "geom" END, 4326)"#
        );
        // Label points go through the same conversion
        assert!(layer
            .label_point_sql("geom")
            .contains("ST_SetSRID(\"geom\", 3857)"));
    }

    #[test]
    fn test_label_point_method_centroid() {
        let l = parse_layer("label_point_method = \"centroid\"");
        assert_eq!(
            l.label_point_sql("geom"),
            "ST_Centroid(ST_Transform(\"geom\", 4326))"
        );
    }

    #[test]
    fn test_label_point_column_overrides_method() {
        let l =
            parse_layer("label_point_method = \"centroid\"\nlabel_point_column = \"label_geom\"");
        assert_eq!(
            l.label_point_sql("geom"),
            "ST_Transform(\"label_geom\", 4326)"
        );
    }

    #[test]
    fn test_label_point_method_rejects_unknown_values() {
        let toml = "name = \"p\"\ntable = \"p\"\nlabel_point_method = \"middle\"";
        let r: Result<LayerConfig, _> = config::Config::builder()
            .add_source(config::File::from_str(toml, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize();
        assert!(r.is_err());
    }

    #[test]
    fn test_label_point_column_must_be_a_plain_identifier() {
        let mut source = sample_source("s", &["parcels"]);
        source.layers[0].label_point_column = Some("geom\"; DROP TABLE x; --".to_string());
        let cfg = sample_config(vec![source]);
        assert!(check_label_point_columns(&cfg).is_err());
        let mut ok = sample_source("s", &["parcels"]);
        ok.layers[0].label_point_column = Some("label_geom".to_string());
        assert!(check_label_point_columns(&sample_config(vec![ok])).is_ok());
    }

    // --- [database] sslmode / sslrootcert parsing ---

    fn parse_database(extra: &str) -> anyhow::Result<DatabaseConfig> {
        let toml = format!(
            "host = \"db\"\nport = 5432\nuser = \"u\"\npassword = \"p\"\ndbname = \"d\"\n{}",
            extra
        );
        let settings = config::Config::builder()
            .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
            .build()?;
        Ok(settings.try_deserialize()?)
    }

    #[test]
    fn test_sslmode_defaults_to_prefer_for_existing_configs() {
        let db = parse_database("").unwrap();
        assert_eq!(db.sslmode, SslMode::Prefer);
        assert_eq!(db.sslrootcert, None);
    }

    #[test]
    fn test_sslmode_parses_all_libpq_values() {
        for (raw, expected) in [
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            let db = parse_database(&format!("sslmode = \"{}\"", raw)).unwrap();
            assert_eq!(db.sslmode, expected, "sslmode = {}", raw);
            assert_eq!(db.sslmode.as_str(), raw);
        }
    }

    #[test]
    fn test_sslmode_rejects_unknown_value() {
        assert!(parse_database("sslmode = \"verify_full\"").is_err());
        assert!(parse_database("sslmode = \"on\"").is_err());
    }

    #[test]
    fn test_sslrootcert_parses() {
        let db = parse_database("sslmode = \"verify-full\"\nsslrootcert = \"/etc/ssl/pg-ca.pem\"")
            .unwrap();
        assert_eq!(db.sslmode, SslMode::VerifyFull);
        assert_eq!(db.sslrootcert.as_deref(), Some("/etc/ssl/pg-ca.pem"));
    }

    #[test]
    fn test_sslmode_maps_to_tokio_postgres_handshake_mode() {
        use tokio_postgres::config::SslMode as Pg;
        assert_eq!(SslMode::Disable.to_postgres(), Pg::Disable);
        assert_eq!(SslMode::Prefer.to_postgres(), Pg::Prefer);
        assert_eq!(SslMode::Require.to_postgres(), Pg::Require);
        assert_eq!(SslMode::VerifyCa.to_postgres(), Pg::Require);
        assert_eq!(SslMode::VerifyFull.to_postgres(), Pg::Require);
    }

    #[test]
    fn test_pg_config_carries_sslmode_and_unescaped_password() {
        let db = DatabaseConfig {
            host: "db".to_string(),
            port: 6543,
            user: "u".to_string(),
            password: "has space'quote".to_string(),
            dbname: "d".to_string(),
            pool_size: None,
            sslmode: SslMode::Require,
            sslrootcert: None,
        };
        let cfg = db.pg_config();
        assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Require);
        assert_eq!(cfg.get_password(), Some(&b"has space'quote"[..]));
        assert_eq!(cfg.get_ports(), &[6543]);
        assert_eq!(cfg.get_dbname(), Some("d"));
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
        assert!(source
            .find_parent_layer_for_derived("parks_labels")
            .is_none());
        assert!(source
            .find_parent_layer_for_derived("nonexistent_labels")
            .is_none());
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
            vec![
                "parks",
                "parks_labels",
                "parks_boundary",
                "water",
                "water_labels"
            ]
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
        assert_eq!(
            cols,
            vec!["geom".to_string(), "geom_simplified".to_string()]
        );
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

    // --- per-layer zoom range ---

    fn parse_config(toml: &str) -> anyhow::Result<AppConfig> {
        let dir = std::env::temp_dir().join(format!(
            "tilefeed-cfg-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        // Same parse and validation as load_config, without the TILES_*
        // environment layer, so the test can't pick up the runner's env.
        let result = config::Config::builder()
            .add_source(config::File::with_name(path.to_str().unwrap()))
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|s| s.try_deserialize::<AppConfig>().map_err(Into::into))
            .and_then(|cfg| cfg.validate().map(|_| cfg));
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn config_with_layer_zooms(layer_zooms: &str) -> String {
        format!(
            r#"
[database]
host = "localhost"
port = 5432
user = "postgres"
password = ""
dbname = "tiles"

[[sources]]
name = "basemap"
mbtiles_path = "/tmp/basemap.mbtiles"
min_zoom = 2
max_zoom = 14

[[sources.layers]]
name = "roads"
table = "roads"
{layer_zooms}
"#
        )
    }

    #[test]
    fn test_layer_zoom_range_parses() {
        let cfg = parse_config(&config_with_layer_zooms("min_zoom = 10\nmax_zoom = 12")).unwrap();
        let source = &cfg.sources[0];
        let layer = &source.layers[0];
        assert_eq!(layer.min_zoom, Some(10));
        assert_eq!(layer.max_zoom, Some(12));
        assert_eq!(source.layer_zoom_range(layer), (10, 12));
        assert_eq!(source.layer_zoom_override(layer), Some((10, 12)));
    }

    #[test]
    fn test_layer_zoom_range_defaults_to_source_range() {
        let cfg = parse_config(&config_with_layer_zooms("")).unwrap();
        let source = &cfg.sources[0];
        let layer = &source.layers[0];
        assert_eq!(layer.min_zoom, None);
        assert_eq!(layer.max_zoom, None);
        assert_eq!(source.layer_zoom_range(layer), (2, 14));
        assert_eq!(source.layer_zoom_override(layer), None);
        assert!(source.layer_applies_at_zoom(layer, 2));
        assert!(source.layer_applies_at_zoom(layer, 14));
    }

    #[test]
    fn test_layer_zoom_range_one_sided() {
        let cfg = parse_config(&config_with_layer_zooms("min_zoom = 10")).unwrap();
        let source = &cfg.sources[0];
        assert_eq!(source.layer_zoom_range(&source.layers[0]), (10, 14));

        let cfg = parse_config(&config_with_layer_zooms("max_zoom = 6")).unwrap();
        let source = &cfg.sources[0];
        assert_eq!(source.layer_zoom_range(&source.layers[0]), (2, 6));
    }

    #[test]
    fn test_layer_zoom_range_equal_to_source_range_is_no_override() {
        let cfg = parse_config(&config_with_layer_zooms("min_zoom = 2\nmax_zoom = 14")).unwrap();
        let source = &cfg.sources[0];
        assert_eq!(source.layer_zoom_override(&source.layers[0]), None);
    }

    #[test]
    fn test_layer_zoom_range_rejects_min_above_max() {
        let err = parse_config(&config_with_layer_zooms("min_zoom = 12\nmax_zoom = 10"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'roads'"), "{err}");
        assert!(
            err.contains("min_zoom 12 is greater than max_zoom 10"),
            "{err}"
        );
    }

    #[test]
    fn test_layer_zoom_range_rejects_min_above_implied_max() {
        // min_zoom alone, above the source's max_zoom
        let err = parse_config(&config_with_layer_zooms("min_zoom = 15"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'roads'"), "{err}");
        assert!(err.contains("min_zoom 15 is outside"), "{err}");
    }

    #[test]
    fn test_layer_zoom_range_rejects_below_source_min() {
        let err = parse_config(&config_with_layer_zooms("min_zoom = 0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("layer 'roads' in source 'basemap'"), "{err}");
        assert!(
            err.contains("min_zoom 0 is outside the source's zoom range 2..=14"),
            "{err}"
        );
    }

    #[test]
    fn test_layer_zoom_range_rejects_above_source_max() {
        let err = parse_config(&config_with_layer_zooms("max_zoom = 16"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_zoom 16 is outside"), "{err}");
    }

    #[test]
    fn test_layer_zoom_range_rejects_max_below_source_min() {
        let err = parse_config(&config_with_layer_zooms("max_zoom = 1"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_zoom 1 is outside"), "{err}");
    }

    #[test]
    fn test_layers_at_zoom_filters_by_range() {
        let mut source = sample_source("src", &["base", "detail", "overview"]);
        source.layers[1].min_zoom = Some(10);
        source.layers[2].max_zoom = Some(5);
        source.validate_layer_zooms().unwrap();

        let names =
            |z: u8| -> Vec<&str> { source.layers_at_zoom(z).map(|l| l.name.as_str()).collect() };
        assert_eq!(names(0), vec!["base", "overview"]);
        assert_eq!(names(5), vec!["base", "overview"]);
        assert_eq!(names(6), vec!["base"]);
        assert_eq!(names(10), vec!["base", "detail"]);
        assert_eq!(names(14), vec!["base", "detail"]);
    }

    #[test]
    fn test_tile_layers_at_zoom_derived_layers_inherit_parent_range() {
        let mut source = sample_source("src", &["parks", "roads"]);
        source.layers[0].generate_label_points = true;
        source.layers[0].generate_boundary_lines = true;
        source.layers[0].min_zoom = Some(8);
        source.layers[0].max_zoom = Some(12);

        let names = |z: u8| -> Vec<String> {
            source
                .tile_layers_at_zoom(z)
                .into_iter()
                .map(|tl| tl.name)
                .collect()
        };
        assert_eq!(names(7), vec!["roads"]);
        assert_eq!(
            names(8),
            vec!["parks", "parks_labels", "parks_boundary", "roads"]
        );
        assert_eq!(
            names(12),
            vec!["parks", "parks_labels", "parks_boundary", "roads"]
        );
        assert_eq!(names(13), vec!["roads"]);

        let derived: Vec<Option<DerivedGeomType>> = source
            .tile_layers_at_zoom(8)
            .into_iter()
            .map(|tl| tl.derived)
            .collect();
        assert_eq!(
            derived,
            vec![
                None,
                Some(DerivedGeomType::LabelPoint),
                Some(DerivedGeomType::BoundaryLine),
                None
            ]
        );
    }

    #[test]
    fn test_tile_layers_at_zoom_empty_when_no_layer_covers_zoom() {
        let mut source = sample_source("src", &["detail"]);
        source.layers[0].min_zoom = Some(10);
        assert!(source.tile_layers_at_zoom(9).is_empty());
        assert_eq!(source.tile_layers_at_zoom(10).len(), 1);
    }

    // --- Example config loading tests ---

    #[test]
    fn test_load_local_parks_config() {
        let cfg = load_config("examples/local-parks/config").unwrap();
        assert_eq!(cfg.sources.len(), 1);
        assert_eq!(cfg.sources[0].name, "parks");
        assert_eq!(cfg.sources[0].layers.len(), 2);
        assert_eq!(cfg.sources[0].layers[0].name, "parks");
        assert_eq!(cfg.sources[0].layers[1].name, "trails");
        assert!(matches!(
            cfg.sources[0].generation_backend,
            GenerationBackend::Tippecanoe
        ));
        assert!(matches!(cfg.publish.backend, PublishBackend::Local));
    }

    #[test]
    fn test_load_serve_simplification_config() {
        let cfg = load_config("examples/serve-with-simplification/config").unwrap();
        assert_eq!(cfg.sources[0].layers[0].name, "parks");
        assert_eq!(
            cfg.sources[0].layers[0].filter,
            Some("type != 'private'".to_string())
        );
        assert!(cfg.sources[0].layers[0].simplify_tolerance.is_some());
        assert!(cfg.sources[0].layers[0].property_rules.is_some());
        assert!(cfg.sources[0].layers[0].generate_label_points);
        assert!(cfg.sources[0].layers[0].generate_boundary_lines);
        assert_eq!(cfg.serve.port, Some(3000));
        assert_eq!(
            cfg.serve.cors_origins,
            Some(vec!["http://localhost:8080".to_string()])
        );
    }

    #[test]
    fn test_load_ogr_fdw_config() {
        let cfg = load_config("examples/ogr-fdw/config").unwrap();
        assert_eq!(cfg.sources[0].name, "buildings");
        assert_eq!(
            cfg.sources[0].layers[0].id_column,
            Some("ogc_fid".to_string())
        );
        assert!(cfg.sources[0].tippecanoe.drop_densest_as_needed == Some(true));
        assert!(cfg.sources[0].tippecanoe.no_tile_size_limit == Some(true));
    }

    #[test]
    fn test_load_native_generation_config() {
        let cfg = load_config("examples/native-generation/config").unwrap();
        assert_eq!(cfg.sources[0].name, "parks");
        assert!(matches!(
            cfg.sources[0].generation_backend,
            GenerationBackend::Native
        ));
        assert_eq!(cfg.sources[0].max_zoom, 8);
        assert!(cfg.sources[0].layers[0].generate_label_points);
    }

    #[test]
    fn test_load_gdal_generation_config() {
        let cfg = load_config("examples/gdal-generation/config").unwrap();
        assert_eq!(cfg.sources[0].name, "districts");
        assert!(matches!(
            cfg.sources[0].generation_backend,
            GenerationBackend::Gdal
        ));
        assert_eq!(cfg.sources[0].max_zoom, 12);
        assert!(cfg.sources[0].layers[0].generate_label_points);
        assert_eq!(
            cfg.sources[0].layers[0].properties,
            Some(vec![
                "name".to_string(),
                "population".to_string(),
                "area_sqkm".to_string()
            ])
        );
    }

    #[test]
    fn test_load_multi_source_config() {
        let cfg = load_config("examples/multi-source/config").unwrap();
        assert_eq!(cfg.sources.len(), 2);
        // Source 1: basemap with tippecanoe
        assert_eq!(cfg.sources[0].name, "basemap");
        assert!(matches!(
            cfg.sources[0].generation_backend,
            GenerationBackend::Tippecanoe
        ));
        assert_eq!(cfg.sources[0].layers.len(), 2);
        assert!(cfg.sources[0].layers[0].generate_label_points);
        assert!(cfg.sources[0].layers[0].generate_boundary_lines);
        assert!(cfg.sources[0].layers[0].simplify_tolerance.is_some());
        assert!(cfg.sources[0].layers[0].property_rules.is_some());
        assert!(cfg.sources[0].layers[1].property_rules.is_some());
        // Source 2: POI with native
        assert_eq!(cfg.sources[1].name, "poi");
        assert!(matches!(
            cfg.sources[1].generation_backend,
            GenerationBackend::Native
        ));
        assert_eq!(cfg.sources[1].min_zoom, 10);
        assert_eq!(cfg.sources[1].max_zoom, 16);
        // Serve config
        assert_eq!(cfg.serve.host, Some("0.0.0.0".to_string()));
        assert_eq!(cfg.serve.cors_origins.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_load_s3_publish_config() {
        let cfg = load_config("examples/s3-publish/config").unwrap();
        assert!(matches!(cfg.publish.backend, PublishBackend::S3));
        assert!(cfg
            .publish
            .destination
            .as_ref()
            .unwrap()
            .starts_with("s3://"));
        assert!(cfg.publish.publish_on_generate_enabled());
        assert!(cfg.publish.publish_on_update_enabled());
    }

    #[test]
    fn test_load_mapbox_publish_config() {
        let cfg = load_config("examples/mapbox-publish/config").unwrap();
        assert!(matches!(cfg.publish.backend, PublishBackend::Mapbox));
        assert_eq!(
            cfg.publish.mapbox_tileset_id,
            Some("username.basemap".to_string())
        );
        assert!(cfg.publish.mapbox_token.is_some());
        assert!(cfg.publish.publish_on_generate_enabled());
        assert!(!cfg.publish.publish_on_update_enabled());
        assert!(cfg.sources[0].layers[0].generate_label_points);
    }

    #[test]
    fn test_load_webhook_sse_config() {
        let cfg = load_config("examples/webhook-sse/config").unwrap();
        assert!(cfg.webhook.is_configured());
        assert_eq!(cfg.webhook.urls.len(), 1);
        assert_eq!(
            cfg.webhook.urls[0],
            "http://localhost:9000/hooks/tile-update"
        );
        assert_eq!(cfg.webhook.timeout_ms, Some(5000));
        assert_eq!(cfg.webhook.retry_count, Some(2));
        assert!(cfg.webhook.on_generate_enabled());
        assert!(cfg.webhook.on_update_enabled());
        assert!(cfg.webhook.secret.is_none());
        assert_eq!(cfg.webhook.cooldown_secs, Some(300));
    }

    #[test]
    fn test_webhook_config_defaults() {
        let config = WebhookConfig::default();
        assert!(config.urls.is_empty());
        assert!(!config.is_configured());
        assert!(config.on_generate_enabled());
        assert!(config.on_update_enabled());
        assert_eq!(config.timeout_ms, Some(5000));
        assert_eq!(config.retry_count, Some(2));
        assert!(config.secret.is_none());
        assert!(config.cooldown_secs.is_none());
    }

    // --- metrics config ---

    #[test]
    fn test_metrics_config_defaults() {
        let config = MetricsConfig::default();
        assert!(config.enabled());
        assert_eq!(config.path(), "/metrics");
        assert!(config.port.is_none());
    }

    #[test]
    fn test_metrics_enabled_by_default_for_existing_configs() {
        // Configs written before [metrics] existed should still expose metrics
        let cfg = load_config("examples/local-parks/config").unwrap();
        assert!(cfg.metrics.enabled());
        assert_eq!(cfg.metrics.path(), "/metrics");
    }

    #[test]
    fn test_metrics_path_normalizes_leading_slash() {
        let config = MetricsConfig {
            enabled: Some(true),
            path: Some("internal/metrics".to_string()),
            host: None,
            port: None,
        };
        assert_eq!(config.path(), "/internal/metrics");
    }

    #[test]
    fn test_metrics_path_falls_back_when_blank() {
        let config = MetricsConfig {
            enabled: None,
            path: Some("   ".to_string()),
            host: None,
            port: None,
        };
        assert_eq!(config.path(), "/metrics");
        // enabled defaults to true when unset
        assert!(config.enabled());
    }

    #[test]
    fn test_metrics_can_be_disabled() {
        let config = MetricsConfig {
            enabled: Some(false),
            path: None,
            host: None,
            port: None,
        };
        assert!(!config.enabled());
    }

    #[test]
    fn test_metrics_addr_defaults_host_to_serve_host() {
        let mut cfg = sample_config(vec![sample_source("a", &["l"])]);
        cfg.serve.host = Some("0.0.0.0".to_string());
        cfg.metrics.port = Some(9090);

        assert_eq!(cfg.metrics_addr(), Some(("0.0.0.0".to_string(), 9090)));
    }

    #[test]
    fn test_separate_metrics_port_takes_endpoint_off_the_tile_server() {
        let mut cfg = sample_config(vec![sample_source("a", &["l"])]);
        cfg.serve.port = Some(3000);
        cfg.metrics.port = Some(9090);

        assert!(!cfg.metrics_addr_is_serve_addr());
        assert!(!cfg.metrics_on_tile_server());
    }

    #[test]
    fn test_metrics_port_matching_serve_port_stays_on_the_tile_server() {
        // Both would bind the same socket; the tile server wins and the
        // standalone listener is skipped rather than failing to bind.
        let mut cfg = sample_config(vec![sample_source("a", &["l"])]);
        cfg.serve.host = Some("127.0.0.1".to_string());
        cfg.serve.port = Some(3000);
        cfg.metrics.port = Some(3000);

        assert!(cfg.metrics_addr_is_serve_addr());
        assert!(cfg.metrics_on_tile_server());
    }

    #[test]
    fn test_metrics_on_tile_server_by_default() {
        let cfg = sample_config(vec![sample_source("a", &["l"])]);
        assert!(cfg.metrics_addr().is_none());
        assert!(cfg.metrics_on_tile_server());
    }

    #[test]
    fn test_disabled_metrics_have_no_address_and_no_route() {
        let mut cfg = sample_config(vec![sample_source("a", &["l"])]);
        cfg.metrics.enabled = Some(false);
        cfg.metrics.port = Some(9090);

        assert!(cfg.metrics_addr().is_none());
        assert!(!cfg.metrics_on_tile_server());
    }

    #[test]
    fn test_serve_config_accessor_defaults() {
        let cfg = ServeConfig {
            host: None,
            port: None,
            cors_origins: None,
            ..Default::default()
        };
        assert_eq!(cfg.host(), "127.0.0.1");
        assert_eq!(cfg.port(), 3000);
    }

    #[test]
    fn test_load_metrics_config() {
        let cfg = load_config("examples/metrics/config").unwrap();
        assert!(cfg.metrics.enabled());
        assert_eq!(cfg.metrics.port, Some(9090));
        assert_eq!(cfg.metrics.host, Some("0.0.0.0".to_string()));
        assert_eq!(cfg.metrics.path(), "/metrics");
    }

    #[test]
    fn test_webhook_not_configured_by_default() {
        // Existing configs without [webhook] should still load fine
        let cfg = load_config("examples/local-parks/config").unwrap();
        assert!(!cfg.webhook.is_configured());
    }

    #[test]
    fn test_load_custom_command_config() {
        let cfg = load_config("examples/custom-command/config").unwrap();
        assert!(matches!(cfg.publish.backend, PublishBackend::Command));
        assert_eq!(cfg.publish.command, Some("rsync".to_string()));
        assert_eq!(
            cfg.publish.args,
            Some(vec!["-avz".to_string(), "--progress".to_string()])
        );
        assert!(matches!(
            cfg.sources[0].generation_backend,
            GenerationBackend::Native
        ));
        assert_eq!(
            cfg.sources[0].layers[0].filter,
            Some("type IN ('national', 'state')".to_string())
        );
    }
}
