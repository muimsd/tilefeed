use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Router,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::config::{AppConfig, ServeConfig};
use crate::events::{EventSender, TileEvent};
use crate::mbtiles::MbtilesStore;
use crate::metrics::{metrics, GaugeGuard};

#[derive(Clone)]
struct AppState {
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    config: Arc<AppConfig>,
    event_tx: Option<EventSender>,
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub async fn start_server(
    config: Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    event_tx: Option<EventSender>,
) -> Result<()> {
    let serve_config = &config.serve;
    let host = serve_config.host.as_deref().unwrap_or("127.0.0.1");
    let port = serve_config.port.unwrap_or(3000);

    let state = AppState {
        stores,
        config: config.clone(),
        event_tx,
    };

    let cors = build_cors_layer(serve_config);

    let mut app = Router::new()
        .route("/{source}/{z}/{x}/{y}.pbf", get(serve_tile))
        .route("/{source}.json", get(serve_tilejson))
        .route("/health", get(health_check))
        .route("/events", get(sse_handler));

    if config.metrics.enabled() {
        let path = config.metrics.path();
        info!("Prometheus metrics exposed at {}", path);
        app = app.route(&path, get(crate::metrics::render_handler));
    }

    let app = app.layer(cors).with_state(state);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("Tile server listening on http://{}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

fn build_cors_layer(config: &ServeConfig) -> CorsLayer {
    match &config.cors_origins {
        Some(origins) if !origins.is_empty() => {
            let origins: Vec<header::HeaderValue> =
                origins.iter().filter_map(|o| o.parse().ok()).collect();
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
    let m = metrics();

    let store = match state.stores.get(&source) {
        Some(s) => s,
        None => {
            m.tile_requests.with(&[&source, "source_not_found"]).inc();
            return (StatusCode::NOT_FOUND, "Source not found").into_response();
        }
    };

    let read_started = std::time::Instant::now();
    let tile_data = {
        let store = store.lock().await;
        match store.get_tile(z, x, y) {
            Ok(data) => data,
            Err(_) => {
                m.tile_requests.with(&[&source, "error"]).inc();
                return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read tile").into_response();
            }
        }
    };
    m.tile_read_duration
        .with(&[&source])
        .observe_since(read_started);

    match tile_data {
        None => {
            m.tile_requests.with(&[&source, "empty"]).inc();
            StatusCode::NO_CONTENT.into_response()
        }
        Some(data) => {
            // Compute ETag
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let etag = format!("\"{}\"", to_hex(&hasher.finalize()[..8]));

            // Check If-None-Match
            if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
                if let Ok(val) = if_none_match.to_str() {
                    if val == etag || val == "*" {
                        m.tile_requests.with(&[&source, "not_modified"]).inc();
                        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
                    }
                }
            }

            m.tile_requests.with(&[&source, "hit"]).inc();
            m.tile_bytes.with(&[&source]).add(data.len() as u64);

            let mut response_headers = HeaderMap::new();
            response_headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/x-protobuf"),
            );
            response_headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("public, max-age=300"),
            );
            if let Ok(value) = header::HeaderValue::from_str(&etag) {
                response_headers.insert(header::ETAG, value);
            }

            // Tiles are gzipped by the native encoder and by Tippecanoe's default,
            // but not when `no_tile_compression` is set, so advertise the encoding
            // the stored bytes actually have rather than assuming gzip.
            if is_gzipped(&data) {
                response_headers.insert(
                    header::CONTENT_ENCODING,
                    header::HeaderValue::from_static("gzip"),
                );
            }

            (response_headers, data).into_response()
        }
    }
}

/// Gzip magic number (RFC 1952): 0x1f 0x8b, followed by deflate method 0x08.
fn is_gzipped(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0x1f && data[1] == 0x8b && data[2] == 0x08
}

async fn serve_tilejson(State(state): State<AppState>, Path(source): Path<String>) -> Response {
    let source_config = match state.config.sources.iter().find(|s| s.name == source) {
        Some(s) => s,
        None => {
            metrics()
                .tilejson_requests
                .with(&[&source, "source_not_found"])
                .inc();
            return (StatusCode::NOT_FOUND, "Source not found").into_response();
        }
    };
    metrics().tilejson_requests.with(&[&source, "ok"]).inc();

    let host = state.config.serve.host.as_deref().unwrap_or("127.0.0.1");
    let port = state.config.serve.port.unwrap_or(3000);

    let mut layers: Vec<serde_json::Value> = Vec::new();
    for l in &source_config.layers {
        let fields = l
            .properties
            .as_ref()
            .map(|p| {
                p.iter()
                    .map(|k| (k.clone(), "".to_string()))
                    .collect::<HashMap<String, String>>()
            })
            .unwrap_or_default();

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
    )
        .into_response()
}

async fn health_check() -> &'static str {
    "ok"
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<SseEvent, std::convert::Infallible>>>,
    StatusCode,
> {
    let event_tx = state.event_tx.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let mut rx = event_tx.subscribe();

    metrics().sse_connections.get().inc();
    // Held by the stream below, so the gauge drops when the client disconnects.
    let _client_guard = GaugeGuard::new(metrics().sse_clients.get());

    let cooldown = state
        .config
        .webhook
        .cooldown_secs
        .filter(|&s| s > 0)
        .map(std::time::Duration::from_secs);

    let stream = async_stream::stream! {
        // Move the gauge guard into the stream so it lives as long as the connection.
        let _client_guard = _client_guard;

        match cooldown {
            None => {
                // No cooldown — forward every event immediately
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            if let Some(sse) = event_to_sse(&event) {
                                yield Ok(sse);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            Some(cooldown) => {
                // Trailing-edge throttle per source
                let mut pending: HashMap<String, (TileEvent, tokio::time::Instant)> = HashMap::new();

                loop {
                    let next_flush = pending
                        .values()
                        .map(|(_, started)| *started + cooldown)
                        .min();
                    let sleep_until = next_flush
                        .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));

                    tokio::select! {
                        result = rx.recv() => {
                            match result {
                                Ok(event) => {
                                    let source = event.source().to_string();
                                    match pending.get_mut(&source) {
                                        Some((existing, _)) => {
                                            existing.merge(&event);
                                        }
                                        None => {
                                            pending.insert(source, (event, tokio::time::Instant::now()));
                                        }
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => {
                                    for (_, (event, _)) in pending.drain() {
                                        if let Some(sse) = event_to_sse(&event) {
                                            yield Ok(sse);
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep_until(sleep_until) => {
                            let now = tokio::time::Instant::now();
                            let expired: Vec<String> = pending
                                .iter()
                                .filter(|(_, (_, started))| now >= *started + cooldown)
                                .map(|(source, _)| source.clone())
                                .collect();

                            for source in expired {
                                if let Some((event, _)) = pending.remove(&source) {
                                    if let Some(sse) = event_to_sse(&event) {
                                        yield Ok(sse);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn event_to_sse(event: &TileEvent) -> Option<SseEvent> {
    let event_type = match event {
        TileEvent::GenerateComplete { .. } => "generate_complete",
        TileEvent::UpdateComplete { .. } => "update_complete",
    };
    serde_json::to_string(event)
        .ok()
        .map(|data| SseEvent::default().event(event_type).data(data))
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
            ogr2ogr_bin: None,
            serve: ServeConfig::default(),
            webhook: WebhookConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    static SERVER_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_test_store() -> (String, MbtilesStore) {
        let id = SERVER_TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir()
            .join(format!(
                "tilefeed_server_test_{}_{}.mbtiles",
                std::process::id(),
                id
            ))
            .to_string_lossy()
            .to_string();
        let store = MbtilesStore::create(&path).unwrap();
        store.put_tile(0, 0, 0, b"tile_data").unwrap();
        (path, store)
    }

    fn make_app(config: AppConfig, stores: HashMap<String, Arc<Mutex<MbtilesStore>>>) -> Router {
        make_app_with_events(config, stores, None)
    }

    fn make_app_with_events(
        config: AppConfig,
        stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
        event_tx: Option<EventSender>,
    ) -> Router {
        let state = AppState {
            stores,
            config: Arc::new(config),
            event_tx,
        };
        let cors = build_cors_layer(&ServeConfig::default());
        Router::new()
            .route("/{source}/{z}/{x}/{y_pbf}", get(serve_tile_test))
            .route("/{source_json}", get(serve_tilejson_test))
            .route("/health", get(health_check))
            .route("/events", get(sse_handler))
            .route("/metrics", get(crate::metrics::render_handler))
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
    fn test_is_gzipped() {
        // gzip magic + deflate method
        assert!(is_gzipped(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(!is_gzipped(b"raw protobuf bytes"));
        assert!(!is_gzipped(&[0x1f, 0x8b]));
        assert!(!is_gzipped(&[]));
    }

    #[tokio::test]
    async fn test_uncompressed_tile_has_no_content_encoding() {
        // Tippecanoe with `no_tile_compression` stores raw protobuf; advertising
        // gzip on those bytes makes them undecodable in the browser.
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
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn test_gzipped_tile_advertises_content_encoding() {
        let config = make_test_config();
        let (path, store) = make_test_store();
        // Minimal gzip stream header, enough for the sniff
        store
            .put_tile(1, 0, 0, &[0x1f, 0x8b, 0x08, 0x00, 0x00])
            .unwrap();
        let _ = path;
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));

        let app = make_app(config, stores);
        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source/1/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[tokio::test]
    async fn test_metrics_endpoint_exposes_tile_counters() {
        let config = make_test_config();
        let (_path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));

        let app = make_app(config, stores);

        let tile_response = send_request(
            app.clone(),
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(tile_response.status(), StatusCode::OK);

        let response = send_request(
            app,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(crate::metrics::CONTENT_TYPE)
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains("tilefeed_build_info"));
        assert!(
            body.contains("tilefeed_tile_requests_total{source=\"test_source\",result=\"hit\"}")
        );
        assert!(body.contains("tilefeed_tile_bytes_total{source=\"test_source\"}"));
        assert!(body.contains("tilefeed_tile_read_duration_seconds_count"));
    }

    #[tokio::test]
    async fn test_metrics_records_missing_source() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app.clone(),
            Request::builder()
                .uri("/ghost_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = send_request(
            app,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(body.contains(
            "tilefeed_tile_requests_total{source=\"ghost_source\",result=\"source_not_found\"}"
        ));
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
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

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
        )
        .await;

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
        )
        .await;

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
        )
        .await;

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
        )
        .await;
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // Second request with If-None-Match
        let app2 = make_app(config, stores);
        let response = send_request(
            app2,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

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
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
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
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // --- SSE endpoint tests ---

    #[tokio::test]
    async fn test_sse_returns_404_without_event_bus() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_sse_returns_200_with_event_bus() {
        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        let app = make_app_with_events(config, HashMap::new(), Some(event_tx));

        let response = send_request(
            app,
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[tokio::test]
    async fn test_sse_client_gauge_tracks_open_connection() {
        // The gauge guard has to be moved into the stream, otherwise the count
        // drops as soon as the handler returns rather than when the client leaves.
        let gauge = crate::metrics::metrics().sse_clients.get();
        let connections_before = crate::metrics::metrics().sse_connections.get().get();

        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        let app = make_app_with_events(config, HashMap::new(), Some(event_tx));

        let response = send_request(
            app,
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        // Other tests share the process-wide registry, so assert on the floor:
        // while this response body is alive, at least one client is counted.
        assert!(gauge.get() >= 1);
        assert!(crate::metrics::metrics().sse_connections.get().get() > connections_before);
    }

    #[tokio::test]
    async fn test_sse_receives_generate_event() {
        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        let app = make_app_with_events(config, HashMap::new(), Some(event_tx.clone()));

        // Connect to SSE endpoint
        let response = send_request(
            app,
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        // Send an event through the bus
        event_tx
            .send(TileEvent::GenerateComplete {
                source: "test_source".to_string(),
                duration_ms: 1234,
            })
            .unwrap();

        // Drop sender to close the stream so we can read it
        drop(event_tx);

        // Read the SSE body
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        assert!(body_str.contains("event: generate_complete"));
        assert!(body_str.contains("test_source"));
        assert!(body_str.contains("1234"));
    }

    #[tokio::test]
    async fn test_sse_receives_update_event() {
        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        let app = make_app_with_events(config, HashMap::new(), Some(event_tx.clone()));

        let response = send_request(
            app,
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let mut zooms = std::collections::HashSet::new();
        zooms.insert(10u8);
        zooms.insert(11u8);

        event_tx
            .send(TileEvent::update_complete(
                "parks".to_string(),
                42,
                &zooms,
                14,
                vec!["parks".to_string()],
            ))
            .unwrap();

        drop(event_tx);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();

        assert!(body_str.contains("event: update_complete"));
        assert!(body_str.contains("\"tiles_updated\":42"));
        assert!(body_str.contains("parks"));
        assert!(body_str.contains("10"));
        assert!(body_str.contains("11"));
    }
}
