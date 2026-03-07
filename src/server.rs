use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

use crate::cache::TileCache;
use crate::config::AppConfig;
use crate::mbtiles::MbtilesStore;

#[derive(Clone)]
pub struct AppState {
    pub mbtiles: Arc<Mutex<MbtilesStore>>,
    pub config: Arc<AppConfig>,
    pub cache: Arc<TileCache>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/tiles/:z/:x/:y_pbf", get(get_tile))
        .route("/tiles.json", get(tilejson))
        .route("/health", get(health))
        .route("/metadata", get(get_metadata))
        .route("/stats", get(get_stats))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn get_tile(
    Path((z, x, y_pbf)): Path<(u8, u32, String)>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let y: u32 = match y_pbf.strip_suffix(".pbf").unwrap_or(&y_pbf).parse() {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Check in-memory cache first
    if let Some(cached) = state.cache.get(z, x, y).await {
        // ETag conditional response
        if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
            if let Ok(val) = if_none_match.to_str() {
                if val == cached.etag {
                    return StatusCode::NOT_MODIFIED.into_response();
                }
            }
        }

        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-protobuf"),
                (header::CONTENT_ENCODING, "gzip"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
                (header::ETAG, &cached.etag),
            ],
            cached.data,
        )
            .into_response();
    }

    // Cache miss — read from MBTiles
    let store = state.mbtiles.lock().await;
    let result = store.get_tile(z, x, y);
    drop(store);

    match result {
        Ok(Some(data)) => {
            // Populate cache
            state.cache.put(z, x, y, data.clone()).await;

            // Compute ETag for this response
            let etag = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                data.hash(&mut h);
                format!("\"{}\"", h.finish())
            };

            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/x-protobuf"),
                    (header::CONTENT_ENCODING, "gzip"),
                    (header::CACHE_CONTROL, "public, max-age=3600"),
                    (header::ETAG, &*etag),
                ],
                data,
            )
                .into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!("Failed to get tile {}/{}/{}: {}", z, x, y, e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn tilejson(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    let host = &config.server.host;
    let port = config.server.port;

    let base_url = if host == "0.0.0.0" {
        format!("http://localhost:{}", port)
    } else {
        format!("http://{}:{}", host, port)
    };

    let tilejson = serde_json::json!({
        "tilejson": "3.0.0",
        "name": "postile",
        "scheme": "xyz",
        "tiles": [format!("{}/tiles/{{z}}/{{x}}/{{y}}.pbf", base_url)],
        "minzoom": config.tiles.min_zoom,
        "maxzoom": config.tiles.max_zoom,
        "format": "pbf",
        "vector_layers": config.tiles.layers.iter().map(|l| {
            serde_json::json!({
                "id": l.name,
                "fields": l.properties.as_ref().map(|props| {
                    props.iter().map(|p| (p.clone(), "".to_string())).collect::<std::collections::HashMap<_, _>>()
                }).unwrap_or_default(),
            })
        }).collect::<Vec<_>>(),
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&tilejson).unwrap_or_default(),
    )
}

async fn get_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let config = &state.config;
    let metadata = serde_json::json!({
        "name": "postile",
        "format": "pbf",
        "minzoom": config.tiles.min_zoom,
        "maxzoom": config.tiles.max_zoom,
        "layers": config.tiles.layers.iter().map(|l| {
            serde_json::json!({
                "name": l.name,
                "table": l.table,
                "schema": l.schema.as_deref().unwrap_or("public"),
            })
        }).collect::<Vec<_>>(),
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&metadata).unwrap_or_default(),
    )
}

async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    let (hits, misses) = state.cache.stats();
    let total = hits + misses;
    let hit_rate = if total > 0 {
        hits as f64 / total as f64 * 100.0
    } else {
        0.0
    };

    let stats = serde_json::json!({
        "cache": {
            "hits": hits,
            "misses": misses,
            "hit_rate_pct": (hit_rate * 100.0).round() / 100.0,
        }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&stats).unwrap_or_default(),
    )
}
