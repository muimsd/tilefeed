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

    let mut layers: Vec<serde_json::Value> = Vec::new();
    for l in &source_config.layers {
        let fields = l.properties.as_ref().map(|p| {
            p.iter().map(|k| (k.clone(), "".to_string())).collect::<HashMap<String, String>>()
        }).unwrap_or_default();

        layers.push(serde_json::json!({
            "id": l.name,
            "fields": fields,
        }));

        // Include derived layers in TileJSON
        if l.generate_label_points {
            layers.push(serde_json::json!({
                "id": format!("{}_labels", l.name),
                "fields": fields,
            }));
        }
        if l.generate_boundary_lines {
            layers.push(serde_json::json!({
                "id": format!("{}_boundary", l.name),
                "fields": fields,
            }));
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;
    use tower::util::ServiceExt;

    fn make_test_config() -> AppConfig {
        use crate::config::*;
        AppConfig {
            database: DatabaseConfig {
                host: "localhost".to_string(),
                port: 5432,
                user: "postgres".to_string(),
                password: "secret".to_string(),
                dbname: "test".to_string(),
                pool_size: None,
            },
            sources: vec![SourceConfig {
                name: "test_source".to_string(),
                mbtiles_path: "/tmp/test.mbtiles".to_string(),
                min_zoom: 0,
                max_zoom: 14,
                generation_backend: GenerationBackend::default(),
                layers: vec![LayerConfig {
                    name: "buildings".to_string(),
                    schema: None,
                    table: "buildings".to_string(),
                    geometry_column: None,
                    id_column: None,
                    srid: None,
                    properties: Some(vec!["name".to_string(), "height".to_string()]),
                    filter: None,
                    geometry_columns: None,
                    simplify_tolerance: None,
                    property_rules: None,
                    generate_label_points: true,
                    generate_boundary_lines: false,
                }],
                tippecanoe: TippecanoeConfig::default(),
            }],
            updates: UpdateConfig::default(),
            publish: PublishConfig::default(),
            tippecanoe_bin: None,
            serve: ServeConfig::default(),
        }
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    static SERVER_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_test_store() -> (String, MbtilesStore) {
        let id = SERVER_TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join(format!("tilefeed_server_test_{}_{}.mbtiles", std::process::id(), id))
            .to_string_lossy()
            .to_string();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(0, 0, 0, b"tile_data").unwrap();
        (path, store)
    }

    fn make_app(config: AppConfig, stores: HashMap<String, Arc<Mutex<MbtilesStore>>>) -> Router {
        let state = AppState {
            stores,
            config: Arc::new(config),
        };
        let cors = build_cors_layer(&ServeConfig::default());
        Router::new()
            .route("/{source}/{z}/{x}/{y_pbf}", get(serve_tile_test))
            .route("/{source_json}", get(serve_tilejson_test))
            .route("/health", get(health_check))
            .layer(cors)
            .with_state(state)
    }

    /// Test-only handler that parses z/x/y.pbf from path segments
    async fn serve_tile_test(
        State(state): State<AppState>,
        Path((source, z, x, y_pbf)): Path<(String, u8, u32, String)>,
        headers: HeaderMap,
    ) -> Response {
        let y: u32 = y_pbf.trim_end_matches(".pbf").parse().unwrap_or(0);
        serve_tile(State(state), Path((source, z, x, y)), headers).await
    }

    /// Test-only handler that strips .json suffix
    async fn serve_tilejson_test(
        State(state): State<AppState>,
        Path(source_json): Path<String>,
    ) -> Response {
        let source = source_json.trim_end_matches(".json").to_string();
        serve_tilejson(State(state), Path(source)).await
    }

    async fn send_request(app: Router, request: Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    #[test]
    fn test_to_hex() {
        assert_eq!(to_hex(&[0x00, 0xff, 0xab]), "00ffab");
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x01]), "01");
    }

    #[test]
    fn test_build_cors_layer_default() {
        let config = ServeConfig::default();
        // Should not panic
        let _cors = build_cors_layer(&config);
    }

    #[test]
    fn test_build_cors_layer_with_origins() {
        let config = ServeConfig {
            host: None,
            port: None,
            cors_origins: Some(vec!["http://localhost:3000".to_string()]),
        };
        let _cors = build_cors_layer(&config);
    }

    #[test]
    fn test_build_cors_layer_empty_origins() {
        let config = ServeConfig {
            host: None,
            port: None,
            cors_origins: Some(vec![]),
        };
        // Empty origins should fall through to Any
        let _cors = build_cors_layer(&config);
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder().uri("/health").body(Body::empty()).unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_tile_source_not_found() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/nonexistent/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_tile_found() {
        let config = make_test_config();
        let (_path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));

        let app = make_app(config, stores);

        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/x-protobuf"
        );
        assert!(response.headers().get("etag").is_some());
        assert!(response.headers().get("cache-control").is_some());

        let _ = std::fs::remove_file(&_path);
    }

    #[tokio::test]
    async fn test_tile_not_found_returns_no_content() {
        let config = make_test_config();
        let (_path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));

        let app = make_app(config, stores);

        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source/5/10/10.pbf")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let _ = std::fs::remove_file(&_path);
    }

    #[tokio::test]
    async fn test_tile_etag_304() {
        let config = make_test_config();
        let (_path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));

        // First request to get the ETag
        let app1 = make_app(config.clone(), stores.clone());
        let response = send_request(
            app1,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        ).await;
        let etag = response.headers().get("etag").unwrap().to_str().unwrap().to_string();

        // Second request with If-None-Match
        let app2 = make_app(config, stores);
        let response = send_request(
            app2,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

        let _ = std::fs::remove_file(&_path);
    }

    #[tokio::test]
    async fn test_tilejson_endpoint() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source.json")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["tilejson"], "3.0.0");
        assert_eq!(json["name"], "test_source");
        assert_eq!(json["minzoom"], 0);
        assert_eq!(json["maxzoom"], 14);

        let layers = json["vector_layers"].as_array().unwrap();
        // buildings + buildings_labels (generate_label_points=true)
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0]["id"], "buildings");
        assert_eq!(layers[1]["id"], "buildings_labels");
    }

    #[tokio::test]
    async fn test_tilejson_source_not_found() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/nonexistent.json")
                .body(Body::empty())
                .unwrap(),
        ).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
