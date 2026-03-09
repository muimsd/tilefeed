use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::config::{AppConfig, ServeConfig};
use crate::mbtiles::MbtilesStore;

#[derive(Clone)]
struct AppState {
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    config: Arc<AppConfig>,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub async fn start_server(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
) -> Result<()> {
    let serve_config = &config.serve;
    let host = serve_config.host.as_deref().unwrap_or("127.0.0.1");
    let port = serve_config.port.unwrap_or(3000);

    let state = AppState {
        stores,
        config: config.clone(),
    };

    let cors = build_cors_layer(serve_config);

    let app = Router::new()
        .route("/{source}/{z}/{x}/{y}.pbf", get(serve_tile))
        .route("/{source}.json", get(serve_tilejson))
        .route("/health", get(health_check))
        .layer(cors)
        .with_state(state);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Tile server listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors_layer(config: &ServeConfig) -> CorsLayer {
    match &config.cors_origins {
        Some(origins) if !origins.is_empty() => {
            let origins: Vec<header::HeaderValue> = origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect();
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([axum::http::Method::GET])
                .allow_headers([header::CONTENT_TYPE, header::IF_NONE_MATCH])
        }
        _ => CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([axum::http::Method::GET])
            .allow_headers([header::CONTENT_TYPE, header::IF_NONE_MATCH]),
    }
}

async fn serve_tile(
    State(state): State<AppState>,
    Path((source, z, x, y)): Path<(String, u8, u32, u32)>,
    headers: HeaderMap,
) -> Response {
    let store = match state.stores.get(&source) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "Source not found").into_response(),
    };

    let tile_data = {
        let store = store.lock().await;
        match store.get_tile(z, x, y) {
            Ok(data) => data,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read tile").into_response(),
        }
    };

    match tile_data {
        None => StatusCode::NO_CONTENT.into_response(),
        Some(data) => {
            // Compute ETag
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let etag = format!("\"{}\"", to_hex(&hasher.finalize()[..8]));

            // Check If-None-Match
            if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
                if let Ok(val) = if_none_match.to_str() {
                    if val == etag || val == "*" {
                        return (
                            StatusCode::NOT_MODIFIED,
                            [(header::ETAG, etag)],
                        ).into_response();
                    }
                }
            }

            (
                [
                    (header::CONTENT_TYPE, "application/x-protobuf".to_string()),
                    (header::CONTENT_ENCODING, "gzip".to_string()),
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, "public, max-age=300".to_string()),
                ],
                data,
            ).into_response()
        }
    }
}

async fn serve_tilejson(
    State(state): State<AppState>,
    Path(source): Path<String>,
) -> Response {
    let source_config = match state.config.sources.iter().find(|s| s.name == source) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "Source not found").into_response(),
    };

    let host = state.config.serve.host.as_deref().unwrap_or("127.0.0.1");
    let port = state.config.serve.port.unwrap_or(3000);

    let layers: Vec<serde_json::Value> = source_config
        .layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "id": l.name,
                "fields": l.properties.as_ref().map(|p| {
                    p.iter().map(|k| (k.clone(), "".to_string())).collect::<HashMap<String, String>>()
                }).unwrap_or_default(),
            })
        })
        .collect();

    let tilejson = serde_json::json!({
        "tilejson": "3.0.0",
        "name": source_config.name,
        "tiles": [format!("http://{}:{}/{}/{{z}}/{{x}}/{{y}}.pbf", host, port, source_config.name)],
        "minzoom": source_config.min_zoom,
        "maxzoom": source_config.max_zoom,
        "vector_layers": layers,
    });

    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&tilejson).unwrap_or_default(),
    ).into_response()
}

async fn health_check() -> &'static str {
    "ok"
}
