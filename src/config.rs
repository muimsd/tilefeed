use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub tiles: TilesConfig,
    #[serde(default)]
    pub updates: UpdateConfig,
    #[serde(default)]
    pub publish: PublishConfig,
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

#[derive(Debug, Clone, Deserialize)]
pub struct TilesConfig {
    pub mbtiles_path: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub layers: Vec<LayerConfig>,
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
    Command,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublishConfig {
    #[serde(default)]
    pub backend: PublishBackend,
    pub destination: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
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

impl DatabaseConfig {
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.host, self.port, self.user, self.password, self.dbname
        )
    }
}

pub fn load_config(path: &str) -> anyhow::Result<AppConfig> {
    let settings = config::Config::builder()
        .add_source(config::File::with_name(path))
        .add_source(config::Environment::with_prefix("TILES"))
        .build()?;

    let cfg: AppConfig = settings.try_deserialize()?;
    Ok(cfg)
}
