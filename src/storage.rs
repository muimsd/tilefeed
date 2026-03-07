use anyhow::{bail, Context, Result};
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
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .env("POSTILE_MBTILES_PATH", source_path)
        .env("POSTILE_PUBLISH_REASON", reason)
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
