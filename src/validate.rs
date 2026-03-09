use anyhow::Result;
use tracing::info;

use crate::config::AppConfig;
use crate::postgis::PostgisReader;

/// Validate the configuration against the actual database
pub async fn validate_config(config: &AppConfig) -> Result<bool> {
    info!("Connecting to database for validation...");
    let reader = PostgisReader::connect(&config.database).await?;

    let mut all_valid = true;

    for source in &config.sources {
        println!("Source: {}", source.name);
        println!("  MBTiles path: {}", source.mbtiles_path);
        println!("  Zoom range: {} - {}", source.min_zoom, source.max_zoom);

        // Check MBTiles path is writable
        if let Some(parent) = std::path::Path::new(&source.mbtiles_path).parent() {
            if !parent.exists() {
                println!("  WARNING: MBTiles parent directory does not exist: {}", parent.display());
                all_valid = false;
            }
        }

        for layer in &source.layers {
            println!("  Layer: {} (table: {}.{})",
                layer.name,
                layer.schema.as_deref().unwrap_or("public"),
                layer.table
            );

            match reader.validate_layer(layer).await {
                Ok(issues) => {
                    if issues.is_empty() {
                        println!("    OK");
                    } else {
                        all_valid = false;
                        for issue in &issues {
                            println!("    ISSUE: {}", issue);
                        }
                    }
                }
                Err(e) => {
                    all_valid = false;
                    println!("    ERROR: Failed to validate: {}", e);
                }
            }

            if let Some(ref filter) = layer.filter {
                println!("    Filter: {}", filter);
            }
        }
        println!();
    }

    if all_valid {
        println!("All validations passed.");
    } else {
        println!("Some validations failed. See issues above.");
    }

    Ok(all_valid)
}
