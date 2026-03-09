use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::info;

use crate::config::{PublishBackend, PublishConfig};

#[derive(Debug, Clone)]
pub enum StoragePublisher {
    Local {
        destination: PathBuf,
    },
    S3 {
        destination: String,
        args: Vec<String>,
    },
    Mapbox {
        tileset_id: String,
        token: String,
    },
    Command {
        command: String,
    },
}

impl StoragePublisher {
    pub fn from_config(config: &PublishConfig) -> Result<Option<Self>> {
        match config.backend {
            PublishBackend::None => Ok(None),
            PublishBackend::Local => {
                let destination = config
                    .destination
                    .as_ref()
                    .context("publish.destination is required when publish.backend=local")?;
                Ok(Some(Self::Local {
                    destination: PathBuf::from(destination),
                }))
            }
            PublishBackend::S3 => {
                let destination = config
                    .destination
                    .as_ref()
                    .context("publish.destination is required when publish.backend=s3")?;
                Ok(Some(Self::S3 {
                    destination: destination.clone(),
                    args: config.args.clone().unwrap_or_default(),
                }))
            }
            PublishBackend::Mapbox => {
                let tileset_id = config
                    .mapbox_tileset_id
                    .as_ref()
                    .context("publish.mapbox_tileset_id is required when publish.backend=mapbox")?;
                let token = config
                    .mapbox_token
                    .as_ref()
                    .context("publish.mapbox_token is required when publish.backend=mapbox")?;
                Ok(Some(Self::Mapbox {
                    tileset_id: tileset_id.clone(),
                    token: token.clone(),
                }))
            }
            PublishBackend::Command => {
                let command = config
                    .command
                    .as_ref()
                    .context("publish.command is required when publish.backend=command")?;
                Ok(Some(Self::Command {
                    command: command.clone(),
                }))
            }
        }
    }

    pub async fn publish_mbtiles(&self, source_path: &str, reason: &str) -> Result<()> {
        match self {
            Self::Local { destination } => publish_to_local(source_path, destination).await,
            Self::S3 { destination, args } => publish_to_s3(source_path, destination, args).await,
            Self::Mapbox { tileset_id, token } => {
                publish_to_mapbox(source_path, tileset_id, token).await
            }
            Self::Command { command } => run_publish_command(command, source_path, reason).await,
        }
    }
}

async fn publish_to_local(source_path: &str, destination: &Path) -> Result<()> {
    let source_file = Path::new(source_path);
    let target = if destination.is_dir() {
        let filename = source_file
            .file_name()
            .context("MBTiles source path has no filename")?;
        destination.join(filename)
    } else {
        destination.to_path_buf()
    };

    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create publish directory {}", parent.display()))?;
    }

    tokio::fs::copy(source_path, &target)
        .await
        .with_context(|| format!("Failed to copy MBTiles to {}", target.display()))?;

    info!(
        "Published MBTiles artifact to local storage: {}",
        target.display()
    );
    Ok(())
}

async fn publish_to_s3(source_path: &str, destination: &str, args: &[String]) -> Result<()> {
    let mut cmd = Command::new("aws");
    cmd.arg("s3")
        .arg("cp")
        .arg(source_path)
        .arg(destination)
        .args(args);

    let output = cmd
        .output()
        .await
        .context("Failed to run aws CLI for S3 publish. Is aws installed and configured?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("S3 publish failed: {}", stderr.trim());
    }

    info!("Published MBTiles artifact to {}", destination);
    Ok(())
}

async fn run_publish_command(command: &str, source_path: &str, reason: &str) -> Result<()> {
    #[cfg(unix)]
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("TILEFEED_MBTILES_PATH", source_path)
        .env("TILEFEED_PUBLISH_REASON", reason)
        .output()
        .await
        .with_context(|| format!("Failed to run publish command '{}'", command))?;

    #[cfg(windows)]
    let output = Command::new("cmd")
        .arg("/C")
        .arg(command)
        .env("TILEFEED_MBTILES_PATH", source_path)
        .env("TILEFEED_PUBLISH_REASON", reason)
        .output()
        .await
        .with_context(|| format!("Failed to run publish command '{}'", command))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Publish command failed: {}", stderr.trim());
    }

    info!("Published MBTiles artifact via custom command");
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MapboxCredentials {
    access_key_id: String,
    bucket: String,
    key: String,
    secret_access_key: String,
    session_token: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct MapboxUploadResponse {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PublishBackend, PublishConfig};

    fn make_config(backend: PublishBackend) -> PublishConfig {
        PublishConfig {
            backend,
            destination: Some("/tmp/output".to_string()),
            command: Some("echo done".to_string()),
            args: Some(vec!["--extra".to_string()]),
            mapbox_tileset_id: Some("user.tileset".to_string()),
            mapbox_token: Some("pk.test123".to_string()),
            publish_on_generate: Some(true),
            publish_on_update: Some(true),
        }
    }

    #[test]
    fn test_from_config_none() {
        let config = make_config(PublishBackend::None);
        let result = StoragePublisher::from_config(&config).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_from_config_local() {
        let config = make_config(PublishBackend::Local);
        let result = StoragePublisher::from_config(&config).unwrap();
        assert!(result.is_some());
        match result.unwrap() {
            StoragePublisher::Local { destination } => {
                assert_eq!(destination, std::path::PathBuf::from("/tmp/output"));
            }
            _ => panic!("Expected Local variant"),
        }
    }

    #[test]
    fn test_from_config_s3() {
        let config = make_config(PublishBackend::S3);
        let result = StoragePublisher::from_config(&config).unwrap();
        match result.unwrap() {
            StoragePublisher::S3 { destination, args } => {
                assert_eq!(destination, "/tmp/output");
                assert_eq!(args, vec!["--extra".to_string()]);
            }
            _ => panic!("Expected S3 variant"),
        }
    }

    #[test]
    fn test_from_config_mapbox() {
        let config = make_config(PublishBackend::Mapbox);
        let result = StoragePublisher::from_config(&config).unwrap();
        match result.unwrap() {
            StoragePublisher::Mapbox { tileset_id, token } => {
                assert_eq!(tileset_id, "user.tileset");
                assert_eq!(token, "pk.test123");
            }
            _ => panic!("Expected Mapbox variant"),
        }
    }

    #[test]
    fn test_from_config_command() {
        let config = make_config(PublishBackend::Command);
        let result = StoragePublisher::from_config(&config).unwrap();
        match result.unwrap() {
            StoragePublisher::Command { command } => {
                assert_eq!(command, "echo done");
            }
            _ => panic!("Expected Command variant"),
        }
    }

    #[test]
    fn test_from_config_local_missing_destination() {
        let config = PublishConfig {
            backend: PublishBackend::Local,
            destination: None,
            ..PublishConfig::default()
        };
        let result = StoragePublisher::from_config(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_from_config_s3_missing_destination() {
        let config = PublishConfig {
            backend: PublishBackend::S3,
            destination: None,
            ..PublishConfig::default()
        };
        assert!(StoragePublisher::from_config(&config).is_err());
    }

    #[test]
    fn test_from_config_mapbox_missing_tileset() {
        let config = PublishConfig {
            backend: PublishBackend::Mapbox,
            mapbox_tileset_id: None,
            mapbox_token: Some("token".to_string()),
            ..PublishConfig::default()
        };
        assert!(StoragePublisher::from_config(&config).is_err());
    }

    #[test]
    fn test_from_config_mapbox_missing_token() {
        let config = PublishConfig {
            backend: PublishBackend::Mapbox,
            mapbox_tileset_id: Some("user.tileset".to_string()),
            mapbox_token: None,
            ..PublishConfig::default()
        };
        assert!(StoragePublisher::from_config(&config).is_err());
    }

    #[test]
    fn test_from_config_command_missing_command() {
        let config = PublishConfig {
            backend: PublishBackend::Command,
            command: None,
            ..PublishConfig::default()
        };
        assert!(StoragePublisher::from_config(&config).is_err());
    }

    #[test]
    fn test_from_config_s3_no_extra_args() {
        let config = PublishConfig {
            backend: PublishBackend::S3,
            destination: Some("s3://bucket/path".to_string()),
            args: None,
            ..PublishConfig::default()
        };
        let result = StoragePublisher::from_config(&config).unwrap();
        match result.unwrap() {
            StoragePublisher::S3 { args, .. } => {
                assert!(args.is_empty());
            }
            _ => panic!("Expected S3 variant"),
        }
    }
}

async fn publish_to_mapbox(source_path: &str, tileset_id: &str, token: &str) -> Result<()> {
    // The tileset_id format is "username.tileset-name"
    let username = tileset_id
        .split('.')
        .next()
        .context("mapbox_tileset_id must be in the format 'username.tileset-name'")?;

    let client = reqwest::Client::new();

    // Step 1: Get temporary S3 credentials from Mapbox
    let creds: MapboxCredentials = client
        .post(format!(
            "https://api.mapbox.com/uploads/v1/{}/credentials?access_token={}",
            username, token
        ))
        .send()
        .await
        .context("Failed to request Mapbox upload credentials")?
        .error_for_status()
        .context("Mapbox credentials request returned an error")?
        .json()
        .await
        .context("Failed to parse Mapbox credentials response")?;

    info!(
        "Obtained Mapbox staging credentials for bucket: {}",
        creds.bucket
    );

    // Step 2: Upload MBTiles to Mapbox's staging S3 bucket using AWS CLI
    let staging_url = format!("s3://{}/{}", creds.bucket, creds.key);
    let mut cmd = Command::new("aws");
    cmd.arg("s3")
        .arg("cp")
        .arg(source_path)
        .arg(&staging_url)
        .env("AWS_ACCESS_KEY_ID", &creds.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &creds.secret_access_key)
        .env("AWS_SESSION_TOKEN", &creds.session_token);

    let output = cmd
        .output()
        .await
        .context("Failed to run aws CLI for Mapbox staging upload. Is aws installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Mapbox staging upload failed: {}", stderr.trim());
    }

    info!("Uploaded MBTiles to Mapbox staging: {}", staging_url);

    // Step 3: Create the upload on Mapbox
    let upload_resp: MapboxUploadResponse = client
        .post(format!(
            "https://api.mapbox.com/uploads/v1/{}?access_token={}",
            username, token
        ))
        .json(&serde_json::json!({
            "url": creds.url,
            "tileset": tileset_id,
            "name": tileset_id,
        }))
        .send()
        .await
        .context("Failed to create Mapbox upload")?
        .error_for_status()
        .context("Mapbox upload creation returned an error")?
        .json()
        .await
        .context("Failed to parse Mapbox upload response")?;

    info!(
        "Created Mapbox upload (id: {}) for tileset {}",
        upload_resp.id, tileset_id
    );

    Ok(())
}
