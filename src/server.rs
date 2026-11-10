use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
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
use tracing::{info, warn};

use crate::cache::{CachedTile, Lookup, TileCache};
use crate::config::{AppConfig, ServeConfig};
use crate::events::{EventSender, Subscription, TileEvent};
use crate::mbtiles::MbtilesStore;
use crate::metrics::{metrics, GaugeGuard};
use crate::viewport::{EventsQuery, Viewport};

type Stores = HashMap<String, Arc<Mutex<MbtilesStore>>>;

#[derive(Clone)]
struct AppState {
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    config: Arc<AppConfig>,
    event_tx: Option<EventSender>,
    cache: Option<Arc<TileCache>>,
    cache_control: header::HeaderValue,
}

/// Stand-in `source` label for requests naming a source that does not exist.
///
/// The requested name comes straight off the URL, so recording it verbatim would
/// let any client mint permanent label sets in the registry — a scanner walking
/// `/aaa/0/0/0.pbf`, `/aab/...` would grow it without bound.
const UNKNOWN_SOURCE: &str = "__unknown__";

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Build the tile server, ready to run.
///
/// Assembling the router is separated from running it so callers can do it on the
/// startup path: a bad route pattern panics inside `Router::route`, and a bad
/// `[metrics] path` returns an error — neither is any use surfacing from inside a
/// spawned task after the process has already reported itself started.
pub fn build_server(
    config: &Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    event_tx: Option<EventSender>,
    cache: Option<Arc<TileCache>>,
) -> Result<TileServer> {
    let watched = cache.clone().map(|cache| (cache, stores.clone()));
    let state = app_state(config, stores, event_tx, cache)?;

    Ok(TileServer {
        addr: crate::metrics::bind_addr(config.serve.host(), config.serve.port()),
        router: build_router(config, state)?,
        watched,
    })
}

fn app_state(
    config: &Arc<AppConfig>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    event_tx: Option<EventSender>,
    cache: Option<Arc<TileCache>>,
) -> Result<AppState> {
    let cache_control = header::HeaderValue::from_str(&config.serve.cache_control())
        .map_err(|e| anyhow::anyhow!("Invalid Cache-Control value: {}", e))?;
    Ok(AppState {
        stores,
        config: config.clone(),
        event_tx,
        cache,
        cache_control,
    })
}

/// A built tile server: routes assembled, nothing bound yet.
pub struct TileServer {
    addr: String,
    router: Router,
    /// The cache and the stores it caches, watched for outside writes.
    watched: Option<(Arc<TileCache>, Stores)>,
}

/// How often each MBTiles file is checked for commits by other processes.
const EXTERNAL_WRITE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

impl TileServer {
    pub async fn run(self) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(&self.addr).await?;
        info!("Tile server listening on http://{}", self.addr);

        if let Some((cache, stores)) = self.watched {
            tokio::spawn(clear_cache_on_external_writes(
                cache,
                stores,
                EXTERNAL_WRITE_POLL,
            ));
        }

        axum::serve(listener, self.router).await?;
        Ok(())
    }
}

/// Assemble the tile server's routes. Shared with the tests so they exercise the
/// same wiring the binary does, metrics mounting included.
fn build_router(config: &AppConfig, state: AppState) -> Result<Router> {
    let cors = build_cors_layer(&config.serve);

    // The extension is part of the path parameter, not the route pattern: axum
    // allows only one parameter per path segment, so "/{y}.pbf" is rejected at
    // startup. The handlers strip and validate the suffix instead, which keeps the
    // published URLs (`/{source}/{z}/{x}/{y}.pbf`, `/{source}.json`) unchanged.
    let app = Router::new()
        .route("/{source}/{z}/{x}/{y_pbf}", get(serve_tile))
        .route("/{source_json}", get(serve_tilejson))
        .route("/", get(serve_preview))
        .route("/health", get(health_check))
        .route("/events", get(sse_handler))
        .layer(cors);

    Ok(mount_metrics(app, config)?.with_state(state))
}

/// Clear a source's cached tiles when another process commits to its MBTiles:
/// a cron `generate`, a separate `watch`, a manual edit. Writes by this
/// process's updater share the connection, so they never trip this; the
/// updater clears exactly the tiles it wrote instead.
///
/// SQLite's `data_version` changes only when a *different* connection commits,
/// which is exactly the case to catch.
async fn clear_cache_on_external_writes(
    cache: Arc<TileCache>,
    stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
    poll: std::time::Duration,
) {
    let mut last: HashMap<String, i64> = HashMap::new();
    let mut interval = tokio::time::interval(poll);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        for (source, store) in &stores {
            let version = match store.lock().await.data_version() {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "Could not check MBTiles of source '{}' for changes: {}",
                        source, e
                    );
                    continue;
                }
            };
            match last.insert(source.clone(), version) {
                Some(previous) if previous != version => {
                    let cleared = cache.clear_source(source);
                    info!(
                        "MBTiles of source '{}' changed outside this process; cleared {} cached tile(s)",
                        source, cleared
                    );
                }
                _ => {}
            }
        }
    }
}

/// Add the metrics route if this server is the one serving metrics.
///
/// Call it *after* the CORS layer: the tile API is deliberately readable from any
/// origin, but a deployment's source names and traffic volumes should not be
/// readable by any page the operator's browser happens to visit.
fn mount_metrics(app: Router<AppState>, config: &AppConfig) -> Result<Router<AppState>> {
    if !config.metrics_on_tile_server() {
        return Ok(app);
    }

    let path = config.metrics.path();
    if crate::config::RESERVED_SERVER_PATHS.contains(&path.as_str()) {
        // Handing this path to axum would panic inside a spawned task, a long
        // way from the config line that caused it.
        anyhow::bail!(
            "[metrics] path = \"{}\" collides with a built-in endpoint; choose another path",
            path
        );
    }

    info!("Prometheus metrics exposed at {}", path);
    Ok(app.route(&path, get(crate::metrics::render_handler)))
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
    Path((source, z, x, y_pbf)): Path<(String, u8, u32, String)>,
    headers: HeaderMap,
) -> Response {
    let m = metrics();

    let y: u32 = match y_pbf.strip_suffix(".pbf").and_then(|y| y.parse().ok()) {
        Some(y) => y,
        None => {
            m.tile_requests.with(&[UNKNOWN_SOURCE, "bad_request"]).inc();
            return (StatusCode::NOT_FOUND, "Expected /{source}/{z}/{x}/{y}.pbf").into_response();
        }
    };

    let store = match state.stores.get(&source) {
        Some(s) => s,
        None => {
            m.tile_requests.with(&[UNKNOWN_SOURCE, "not_found"]).inc();
            return (StatusCode::NOT_FOUND, "Source not found").into_response();
        }
    };

    // x and y must address a tile at zoom z; past z30 the TMS flip overflows.
    if z > 30 || x >= (1u32 << z) || y >= (1u32 << z) {
        m.tile_requests.with(&[&source, "bad_request"]).inc();
        return (StatusCode::NOT_FOUND, "Tile coordinates out of range").into_response();
    }

    // Only zooms the source actually has are cached, so requests outside its
    // range (overzoom probes, crawlers) cannot push real tiles out.
    let cache = state.cache.as_ref().filter(|_| {
        state
            .config
            .sources
            .iter()
            .find(|s| s.name == source)
            .is_some_and(|s| (s.min_zoom..=s.max_zoom).contains(&z))
    });

    // On a miss the generation is taken before the store read, so an update
    // committed while we read makes the cache refuse what we read.
    let generation = match cache.map(|c| c.lookup(&source, z, x, y)) {
        Some(Lookup::Hit(tile)) => {
            return tile_response(&tile, &headers, &source, &state.cache_control);
        }
        Some(Lookup::Miss { generation }) => Some(generation),
        None => None,
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

    let tile = match tile_data {
        None => CachedTile::Empty,
        Some(data) => {
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let etag = format!("\"{}\"", to_hex(&hasher.finalize()[..8]));
            CachedTile::Tile {
                data: data.into(),
                etag,
            }
        }
    };

    if let (Some(cache), Some(generation)) = (cache, generation) {
        cache.insert(&source, z, x, y, tile.clone(), generation);
    }

    tile_response(&tile, &headers, &source, &state.cache_control)
}

/// Build the HTTP response for a tile, from the cache or freshly read.
fn tile_response(
    tile: &CachedTile,
    headers: &HeaderMap,
    source: &str,
    cache_control: &header::HeaderValue,
) -> Response {
    let m = metrics();
    let (data, etag) = match tile {
        CachedTile::Empty => {
            m.tile_requests.with(&[source, "empty"]).inc();
            return StatusCode::NO_CONTENT.into_response();
        }
        CachedTile::Tile { data, etag } => (data, etag),
    };

    // Check If-None-Match
    if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH) {
        if let Ok(val) = if_none_match.to_str() {
            if val == etag || val == "*" {
                m.tile_requests.with(&[source, "not_modified"]).inc();
                // A 304 carries the Cache-Control the 200 would have, so
                // caches refreshed by revalidation pick up the current policy.
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, header::HeaderValue::from_str(etag).ok()),
                        (header::CACHE_CONTROL, Some(cache_control.clone())),
                    ]
                    .into_iter()
                    .filter_map(|(k, v)| v.map(|v| (k, v)))
                    .collect::<HeaderMap>(),
                )
                    .into_response();
            }
        }
    }

    m.tile_requests.with(&[source, "hit"]).inc();
    m.tile_bytes.with(&[source]).add(data.len() as u64);

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-protobuf"),
    );
    response_headers.insert(header::CACHE_CONTROL, cache_control.clone());
    if let Ok(value) = header::HeaderValue::from_str(etag) {
        response_headers.insert(header::ETAG, value);
    }

    // Tiles are gzipped by the native encoder and by Tippecanoe's default,
    // but not when `no_tile_compression` is set, so advertise the encoding
    // the stored bytes actually have rather than assuming gzip.
    if crate::mvt::is_gzipped(data) {
        response_headers.insert(
            header::CONTENT_ENCODING,
            header::HeaderValue::from_static("gzip"),
        );
    }

    (response_headers, data.clone()).into_response()
}

async fn serve_tilejson(
    State(state): State<AppState>,
    Path(source_json): Path<String>,
) -> Response {
    // This route is a single-segment catch-all, so it also collects /favicon.ico
    // and anything else a browser or scraper tries; count those rather than
    // 404-ing them invisibly.
    let source = match source_json.strip_suffix(".json") {
        Some(source) => source.to_string(),
        None => {
            metrics()
                .tilejson_requests
                .with(&[UNKNOWN_SOURCE, "bad_request"])
                .inc();
            return (StatusCode::NOT_FOUND, "Expected /{source}.json").into_response();
        }
    };

    let source_config = match state.config.sources.iter().find(|s| s.name == source) {
        Some(s) => s,
        None => {
            metrics()
                .tilejson_requests
                .with(&[UNKNOWN_SOURCE, "not_found"])
                .inc();
            return (StatusCode::NOT_FOUND, "Source not found").into_response();
        }
    };
    metrics().tilejson_requests.with(&[&source, "ok"]).inc();

    let host = state.config.serve.host.as_deref().unwrap_or("127.0.0.1");
    let port = state.config.serve.port.unwrap_or(3000);

    let mut layers: Vec<serde_json::Value> = Vec::new();
    for l in &source_config.layers {
        // TileJSON 3.0 per-layer zoom range; derived layers share their parent's
        let (layer_min, layer_max) = source_config.layer_zoom_range(l);
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
            "minzoom": layer_min,
            "maxzoom": layer_max,
        }));

        // Include derived layers in TileJSON
        if l.generate_label_points {
            layers.push(serde_json::json!({
                "id": format!("{}_labels", l.name),
                "fields": fields,
                "minzoom": layer_min,
                "maxzoom": layer_max,
            }));
        }
        if l.generate_boundary_lines {
            layers.push(serde_json::json!({
                "id": format!("{}_boundary", l.name),
                "fields": fields,
                "minzoom": layer_min,
                "maxzoom": layer_max,
            }));
        }
    }

    let mut tilejson = serde_json::json!({
        "tilejson": "3.0.0",
        "name": source_config.name,
        "tiles": [format!("http://{}:{}/{}/{{z}}/{{x}}/{{y}}.pbf", host, port, source_config.name)],
        "minzoom": source_config.min_zoom,
        "maxzoom": source_config.max_zoom,
        "vector_layers": layers,
    });
    // Where the data is, so a client (the preview at `/`) can frame it.
    let bounds = match state.stores.get(&source) {
        Some(store) => store.lock().await.bounds().unwrap_or_else(|e| {
            warn!("Cannot read bounds of source '{}': {:#}", source, e);
            None
        }),
        None => None,
    };
    if let Some([w, s, e, n]) = bounds {
        tilejson["bounds"] = serde_json::json!([w, s, e, n]);
        tilejson["center"] =
            serde_json::json!([(w + e) / 2.0, (s + n) / 2.0, source_config.min_zoom]);
    }

    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string_pretty(&tilejson).unwrap_or_default(),
    )
        .into_response()
}

/// The map preview: every source on a MapLibre map, refreshed live over
/// SSE. The page is static except for the source list spliced in here.
async fn serve_preview(State(state): State<AppState>) -> Response {
    if !state.config.serve.preview_enabled() {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    let sources: Vec<serde_json::Value> = state
        .config
        .sources
        .iter()
        .map(
            |s| serde_json::json!({ "name": s.name, "minzoom": s.min_zoom, "maxzoom": s.max_zoom }),
        )
        .collect();
    // Inside a <script>, "</" could close the tag early; JSON allows "<\/".
    let sources = serde_json::to_string(&sources)
        .unwrap_or_else(|_| "[]".to_string())
        .replace("</", "<\\/");
    let page = PREVIEW_HTML.replace("__TILEFEED_SOURCES__", &sources);
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        page,
    )
        .into_response()
}

const PREVIEW_HTML: &str = include_str!("preview.html");

async fn health_check() -> &'static str {
    "ok"
}

/// `GET /events[?bbox=w,s,e,n][&zoom=z][&source=a,b][&since=id]`.
///
/// With a viewport, the client only gets updates that change tiles it can see
/// (see `viewport.rs`). Every event carries an SSE `id`; a client reconnecting
/// with `since` (or the `Last-Event-ID` header `EventSource` sends on its own
/// retries) first gets the retained events it missed that match its new
/// viewport, or a `resync` event when they are no longer available. A `ready`
/// event marks the end of the replay and carries the current id.
async fn sse_handler(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<SseEvent, std::convert::Infallible>>>,
    (StatusCode, String),
> {
    let event_tx = state
        .event_tx
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "Not Found".to_string()))?;
    let mut viewport =
        Viewport::parse(&query, state.config.sources.iter().map(|s| s.name.as_str()))
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let since = query.since.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    });
    let Subscription {
        replay,
        mut rx,
        latest_id,
        complete,
    } = event_tx.subscribe_since(since);

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

        if !complete {
            yield Ok(resync_sse());
        }
        for missed in replay {
            if viewport.wants(&missed.event) {
                if let Some(sse) = event_to_sse(&missed.event, missed.id) {
                    yield Ok(sse);
                }
            }
        }
        yield Ok(ready_sse(latest_id));

        match cooldown {
            None => {
                // No cooldown — forward every event immediately
                loop {
                    match rx.recv().await {
                        Ok(e) if !viewport.wants(&e.event) => continue,
                        Ok(e) => {
                            if let Some(sse) = event_to_sse(&e.event, e.id) {
                                yield Ok(sse);
                            }
                        }
                        // Events were dropped for this slow client; tell it
                        // to refresh rather than leave tiles stale.
                        Err(broadcast::error::RecvError::Lagged(_)) => yield Ok(resync_sse()),
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
            Some(cooldown) => {
                // Trailing-edge throttle per source
                let mut pending: HashMap<String, PendingEvent> = HashMap::new();

                loop {
                    let next_flush = pending
                        .values()
                        .map(|p| p.started + cooldown)
                        .min();
                    let sleep_until = next_flush
                        .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(3600));

                    tokio::select! {
                        result = rx.recv() => {
                            match result {
                                // Filter before merging, so an aggregated event
                                // only accumulates changes this client can see.
                                Ok(e) if !viewport.wants(&e.event) => {}
                                Ok(e) => {
                                    let source = e.event.source().to_string();
                                    match pending.get_mut(&source) {
                                        Some(p) => {
                                            p.event.merge(&e.event);
                                            p.last_id = e.id;
                                        }
                                        None => {
                                            pending.insert(source, PendingEvent {
                                                first_id: e.id,
                                                last_id: e.id,
                                                event: e.event,
                                                started: tokio::time::Instant::now(),
                                            });
                                        }
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(_)) => yield Ok(resync_sse()),
                                Err(broadcast::error::RecvError::Closed) => {
                                    for (id, event) in flush_pending(&mut pending, |_| true) {
                                        if let Some(sse) = event_to_sse(&event, id) { yield Ok(sse); }
                                    }
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep_until(sleep_until) => {
                            let now = tokio::time::Instant::now();
                            for (id, event) in flush_pending(&mut pending, |p| now >= p.started + cooldown) {
                                if let Some(sse) = event_to_sse(&event, id) { yield Ok(sse); }
                            }
                        }
                    }
                }
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// A source's events accumulated during an SSE cooldown window.
struct PendingEvent {
    event: TileEvent,
    started: tokio::time::Instant,
    first_id: u64,
    last_id: u64,
}

/// Take the pending events `due` selects, each with the SSE id to send it with.
///
/// The SSE id a client resumes from must not pass an event it hasn't been
/// sent: another source's events may still be waiting out their cooldown, so
/// each flushed event's id is held below the oldest one still pending.
fn flush_pending(
    pending: &mut HashMap<String, PendingEvent>,
    due: impl Fn(&PendingEvent) -> bool,
) -> Vec<(u64, TileEvent)> {
    let due_sources: Vec<String> = pending
        .iter()
        .filter(|(_, p)| due(p))
        .map(|(source, _)| source.clone())
        .collect();
    let mut flushed: Vec<PendingEvent> = due_sources
        .iter()
        .filter_map(|source| pending.remove(source))
        .collect();
    flushed.sort_by_key(|p| p.last_id);
    let still_waiting = pending.values().map(|p| p.first_id).min();
    flushed
        .into_iter()
        .map(|p| {
            let id = match still_waiting {
                Some(first) => p.last_id.min(first.saturating_sub(1)),
                None => p.last_id,
            };
            (id, p.event)
        })
        .collect()
}

fn event_to_sse(event: &TileEvent, id: u64) -> Option<SseEvent> {
    let event_type = match event {
        TileEvent::GenerateComplete { .. } => "generate_complete",
        TileEvent::UpdateComplete { .. } => "update_complete",
    };
    serde_json::to_string(event).ok().map(|data| {
        SseEvent::default()
            .event(event_type)
            .id(id.to_string())
            .data(data)
    })
}

/// End of the replay; its id is where a reconnect resumes from.
fn ready_sse(latest_id: u64) -> SseEvent {
    SseEvent::default()
        .event("ready")
        .id(latest_id.to_string())
        .data(format!(r#"{{"last_event_id":{latest_id}}}"#))
}

/// Updates were missed and can't be replayed: reload every source shown.
fn resync_sse() -> SseEvent {
    SseEvent::default()
        .event("resync")
        .data(r#"{"reason":"missed_events"}"#)
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
                sslmode: SslMode::default(),
                sslrootcert: None,
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
                    min_zoom: None,
                    max_zoom: None,
                    label_point_method: Default::default(),
                    label_point_column: None,
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
        try_make_app_with_events(config, stores, event_tx).unwrap()
    }

    /// Builds exactly what the binary serves. Nothing about the routing is
    /// re-stated here: a test harness that declared its own routes is what let a
    /// startup panic in the real ones go unnoticed.
    fn try_make_app_with_events(
        config: AppConfig,
        stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
        event_tx: Option<EventSender>,
    ) -> Result<Router> {
        try_make_app_with_cache(config, stores, event_tx, None)
    }

    fn try_make_app_with_cache(
        config: AppConfig,
        stores: HashMap<String, Arc<Mutex<MbtilesStore>>>,
        event_tx: Option<EventSender>,
        cache: Option<Arc<TileCache>>,
    ) -> Result<Router> {
        let config = Arc::new(config);
        let state = app_state(&config, stores, event_tx, cache)?;
        build_router(&config, state)
    }

    async fn send_request(app: Router, request: Request<Body>) -> Response {
        app.oneshot(request).await.unwrap()
    }

    #[test]
    fn test_is_gzipped() {
        // gzip magic + deflate method
        assert!(crate::mvt::is_gzipped(&[0x1f, 0x8b, 0x08, 0x00]));
        assert!(!crate::mvt::is_gzipped(b"raw protobuf bytes"));
        assert!(!crate::mvt::is_gzipped(&[0x1f, 0x8b]));
        assert!(!crate::mvt::is_gzipped(&[]));
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

        // The requested name must not become a label — it comes from the URL, so
        // recording it would let any client grow the registry without bound.
        assert!(body
            .contains("tilefeed_tile_requests_total{source=\"__unknown__\",result=\"not_found\"}"));
        assert!(!body.contains("ghost_source"));
    }

    #[tokio::test]
    async fn test_unknown_sources_share_one_label_set() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        for name in ["scan_a", "scan_b", "scan_c"] {
            let response = send_request(
                app.clone(),
                Request::builder()
                    .uri(format!("/{}/0/0/0.pbf", name))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);

            let response = send_request(
                app.clone(),
                Request::builder()
                    .uri(format!("/{}.json", name))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

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

        for name in ["scan_a", "scan_b", "scan_c"] {
            assert!(
                !body.contains(name),
                "'{}' leaked into a metric label",
                name
            );
        }
    }

    #[tokio::test]
    async fn test_metrics_route_not_mounted_when_disabled() {
        let mut config = make_test_config();
        config.metrics.enabled = Some(false);
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        // Falls through to the `/{source_json}` catch-all rather than serving metrics
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(crate::metrics::CONTENT_TYPE)
        );
    }

    #[tokio::test]
    async fn test_metrics_served_on_custom_path() {
        let mut config = make_test_config();
        config.metrics.path = Some("internal/metrics".to_string());
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/internal/metrics")
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
    }

    #[tokio::test]
    async fn test_metrics_not_mounted_when_a_separate_listener_serves_them() {
        // Setting a distinct [metrics] port is documented as the way to keep the
        // scrape endpoint off the tile port, so the tile server must not mount it.
        let mut config = make_test_config();
        config.serve.port = Some(3000);
        config.metrics.port = Some(9090);
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(crate::metrics::CONTENT_TYPE)
        );
    }

    #[test]
    fn test_metrics_path_colliding_with_builtin_is_rejected() {
        let mut config = make_test_config();
        config.metrics.path = Some("/health".to_string());

        let err = try_make_app_with_events(config, HashMap::new(), None)
            .expect_err("a colliding metrics path must be reported, not panic inside axum");
        assert!(err.to_string().contains("collides"));
    }

    #[tokio::test]
    async fn test_metrics_route_is_not_cors_exposed() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app.clone(),
            Request::builder()
                .uri("/metrics")
                .header("Origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "metrics must not be readable cross-origin"
        );

        // ...while tiles still are
        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .header("Origin", "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(response
            .headers()
            .get("access-control-allow-origin")
            .is_some());
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
            ..Default::default()
        };
        let _cors = build_cors_layer(&config);
    }

    #[test]
    fn test_build_cors_layer_empty_origins() {
        let config = ServeConfig {
            host: None,
            port: None,
            cors_origins: Some(vec![]),
            ..Default::default()
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

    #[test]
    fn test_production_routes_are_valid() {
        // axum panics when a route pattern puts a literal next to a parameter in
        // one segment ("/{y}.pbf"), and it panics at startup, inside the spawned
        // server task. Building the real router here is what catches that.
        let config = make_test_config();
        let _router = try_make_app_with_events(config, HashMap::new(), None)
            .expect("the production router must build");
    }

    #[tokio::test]
    async fn test_tile_requires_pbf_extension() {
        let config = make_test_config();
        let (_path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
        let app = make_app(config, stores);

        for uri in [
            "/test_source/0/0/0.png",
            "/test_source/0/0/0",
            "/test_source/0/0/abc.pbf",
        ] {
            let response = send_request(
                app.clone(),
                Request::builder().uri(uri).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "for {}", uri);
        }
    }

    #[tokio::test]
    async fn test_tilejson_requires_json_extension() {
        let config = make_test_config();
        let app = make_app(config, HashMap::new());

        let response = send_request(
            app,
            Request::builder()
                .uri("/test_source")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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

    async fn get_tile_body(app: Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = send_request(
            app,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn test_cache_serves_cached_tile_until_invalidated() {
        let (path, store) = make_test_store();
        let store = Arc::new(Mutex::new(store));
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), store.clone());
        let cache = Arc::new(TileCache::new(1 << 20, None));
        let app =
            try_make_app_with_cache(make_test_config(), stores, None, Some(cache.clone())).unwrap();

        let (status, body) = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"tile_data");

        // Written behind the cache's back: the cached tile is still served.
        store.lock().await.put_tile(0, 0, 0, b"new_data").unwrap();
        let (_, body) = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        assert_eq!(body, b"tile_data");

        // The updater clears the tile after writing it: the new one is served.
        assert_eq!(cache.invalidate("test_source", [(0, 0, 0)]), 1);
        let (_, body) = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        assert_eq!(body, b"new_data");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_cache_clears_emptied_tile_and_caches_empty() {
        let (path, store) = make_test_store();
        let store = Arc::new(Mutex::new(store));
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), store.clone());
        let cache = Arc::new(TileCache::new(1 << 20, None));
        let app =
            try_make_app_with_cache(make_test_config(), stores, None, Some(cache.clone())).unwrap();

        // An empty tile is cached as empty...
        let (status, _) = get_tile_body(app.clone(), "/test_source/1/0/0.pbf").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        store.lock().await.put_tile(1, 0, 0, b"filled").unwrap();
        let (status, _) = get_tile_body(app.clone(), "/test_source/1/0/0.pbf").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        cache.invalidate("test_source", [(1, 0, 0)]);
        let (status, body) = get_tile_body(app.clone(), "/test_source/1/0/0.pbf").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"filled");

        // ...and a deleted tile stops being served once cleared.
        get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        store.lock().await.delete_tile(0, 0, 0).unwrap();
        cache.invalidate("test_source", [(0, 0, 0)]);
        let (status, _) = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_cached_tile_keeps_etag_and_304() {
        let (path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
        let cache = Arc::new(TileCache::new(1 << 20, None));
        let app = try_make_app_with_cache(make_test_config(), stores, None, Some(cache)).unwrap();

        let first = send_request(
            app.clone(),
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let etag = first.headers().get("etag").unwrap().clone();
        // Served from the cache now; same ETag, and revalidation still 304s.
        let second = send_request(
            app.clone(),
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .header("if-none-match", etag.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(second.headers().get("etag").unwrap(), &etag);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_external_write_clears_source_cache() {
        let (path, store) = make_test_store();
        let store = Arc::new(Mutex::new(store));
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), store.clone());
        let cache = Arc::new(TileCache::new(1 << 20, None));
        let app = try_make_app_with_cache(
            make_test_config(),
            stores.clone(),
            None,
            Some(cache.clone()),
        )
        .unwrap();
        let task = tokio::spawn(clear_cache_on_external_writes(
            cache.clone(),
            stores,
            std::time::Duration::from_millis(10),
        ));

        let (_, body) = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await;
        assert_eq!(body, b"tile_data");
        // Let the watcher record the current version before the outside write.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Another process (here: another connection) rewrites the tile.
        let other = MbtilesStore::open(&path).unwrap();
        other.put_tile(0, 0, 0, b"outside").unwrap();
        drop(other);

        let mut body = Vec::new();
        for _ in 0..100 {
            body = get_tile_body(app.clone(), "/test_source/0/0/0.pbf").await.1;
            if body == b"outside" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(body, b"outside");
        task.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_out_of_range_coordinates_are_rejected_and_not_cached() {
        let (path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
        let cache = Arc::new(TileCache::new(1 << 20, None));
        let app =
            try_make_app_with_cache(make_test_config(), stores, None, Some(cache.clone())).unwrap();

        for uri in [
            "/test_source/3/8/0.pbf",
            "/test_source/3/0/8.pbf",
            "/test_source/31/0/0.pbf",
            "/test_source/200/0/0.pbf",
        ] {
            let (status, _) = get_tile_body(app.clone(), uri).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{}", uri);
        }
        // Valid but outside the source's zoom range: served, not cached.
        let max = make_test_config().sources[0].max_zoom;
        let uri = format!("/test_source/{}/0/0.pbf", max + 1);
        let (status, _) = get_tile_body(app.clone(), &uri).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(cache.sources_cached(), 0);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_not_modified_carries_cache_control() {
        let (path, store) = make_test_store();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
        let mut config = make_test_config();
        config.serve.max_age_secs = Some(0);
        let app = make_app(config, stores);
        let first = send_request(
            app.clone(),
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let etag = first.headers().get("etag").unwrap().clone();
        let second = send_request(
            app,
            Request::builder()
                .uri("/test_source/0/0/0.pbf")
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(second.headers().get("cache-control").unwrap(), "no-cache");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_cache_control_is_configurable() {
        for (max_age, expected) in [
            (None, "public, max-age=300"),
            (Some(0), "no-cache"),
            (Some(60), "public, max-age=60"),
        ] {
            let (path, store) = make_test_store();
            let mut stores = HashMap::new();
            stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
            let mut config = make_test_config();
            config.serve.max_age_secs = max_age;
            let response = send_request(
                make_app(config, stores),
                Request::builder()
                    .uri("/test_source/0/0/0.pbf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(response.headers().get("cache-control").unwrap(), expected);
            let _ = std::fs::remove_file(&path);
        }
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
        // Without a per-layer range, layers span the source's zooms
        assert_eq!(layers[0]["minzoom"], 0);
        assert_eq!(layers[0]["maxzoom"], 14);
    }

    #[tokio::test]
    async fn test_tilejson_reports_per_layer_zoom_range() {
        let mut config = make_test_config();
        config.sources[0].layers[0].min_zoom = Some(10);
        config.sources[0].layers[0].max_zoom = Some(12);
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
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        // The tileset keeps the source range; the layer and its derived layer narrow it
        assert_eq!(json["minzoom"], 0);
        assert_eq!(json["maxzoom"], 14);
        let layers = json["vector_layers"].as_array().unwrap();
        for layer in layers {
            assert_eq!(layer["minzoom"], 10, "{}", layer["id"]);
            assert_eq!(layer["maxzoom"], 12, "{}", layer["id"]);
        }
    }

    async fn body_string(response: Response) -> String {
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn test_tilejson_reports_bounds_and_center_of_the_store() {
        let config = make_test_config();
        let (path, store) = make_test_store();
        store.set_metadata("bounds", "2.25,48.8,2.45,48.9").unwrap();
        let mut stores = HashMap::new();
        stores.insert("test_source".to_string(), Arc::new(Mutex::new(store)));
        let app = make_app(config, stores);

        let response = send_request(app, get("/test_source.json")).await;
        let json: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(json["bounds"], serde_json::json!([2.25, 48.8, 2.45, 48.9]));
        let center = json["center"].as_array().unwrap();
        assert!((center[0].as_f64().unwrap() - 2.35).abs() < 1e-9);
        assert!((center[1].as_f64().unwrap() - 48.85).abs() < 1e-9);
        assert_eq!(center[2], 0);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn test_tilejson_without_tiles_has_no_bounds() {
        let app = make_app(make_test_config(), HashMap::new());
        let response = send_request(app, get("/test_source.json")).await;
        let json: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert!(json.get("bounds").is_none());
        assert!(json.get("center").is_none());
    }

    #[tokio::test]
    async fn test_preview_page_lists_the_sources() {
        let app = make_app(make_test_config(), HashMap::new());
        let response = send_request(app, get("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let page = body_string(response).await;
        let line = page
            .lines()
            .find(|l| l.contains("const SOURCES = "))
            .expect("source list missing");
        let json = line
            .trim()
            .trim_start_matches("const SOURCES = ")
            .trim_end_matches(';');
        let sources: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            sources,
            serde_json::json!([{"name": "test_source", "minzoom": 0, "maxzoom": 14}])
        );
        assert!(!page.contains("__TILEFEED_SOURCES__"));
        assert!(page.contains("maplibre-gl@4.7.1"));
    }

    #[tokio::test]
    async fn test_preview_escapes_a_source_name_that_closes_the_script() {
        let mut config = make_test_config();
        config.sources[0].name = "x</script><script>alert(1)//".to_string();
        let app = make_app(config, HashMap::new());
        let page = body_string(send_request(app, get("/")).await).await;
        assert!(!page.contains("x</script>"), "unescaped source name");
        assert!(page.contains(r#"x<\/script>"#));
    }

    #[tokio::test]
    async fn test_preview_can_be_turned_off() {
        let mut config = make_test_config();
        config.serve.preview = Some(false);
        let app = make_app(config, HashMap::new());
        let response = send_request(app, get("/")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
    async fn test_sse_viewport_filter_drops_changes_out_of_view() {
        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        let app = make_app_with_events(config, HashMap::new(), Some(event_tx.clone()));

        // A client looking at Paris at z12, through the production router.
        let response = send_request(
            app,
            Request::builder()
                .uri("/events?bbox=2.2,48.8,2.5,48.9&zoom=12&source=test_source")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let at = |tiles: usize, bbox: [f64; 4]| {
            TileEvent::update_complete(
                "test_source".to_string(),
                tiles,
                &std::collections::HashSet::new(),
                14,
                vec![],
            )
            .with_areas(vec![crate::events::ChangedArea {
                bbox,
                min_zoom: 0,
                max_zoom: 14,
            }])
        };
        event_tx
            .send(at(111, [139.7, 35.6, 139.71, 35.61]))
            .unwrap(); // Tokyo
        event_tx.send(at(222, [2.34, 48.85, 2.35, 48.86])).unwrap(); // Paris
        drop(event_tx);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("\"tiles_updated\":222"));
        assert!(!body_str.contains("\"tiles_updated\":111"));
    }

    async fn sse_body(app: Router, request: Request<Body>, event_tx: EventSender) -> String {
        let response = send_request(app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(event_tx);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    fn generated(source: &str, duration_ms: u64) -> TileEvent {
        TileEvent::GenerateComplete {
            source: source.to_string(),
            duration_ms,
        }
    }

    /// SSE body for `request` against a bus that has already sent `sent`.
    /// Every handle to the bus is dropped before reading, so the stream ends.
    async fn sse_after(sent: &[TileEvent], request: Request<Body>) -> String {
        let event_tx = crate::events::create_event_bus();
        let _keep = event_tx.subscribe();
        for event in sent {
            event_tx.send(event.clone()).unwrap();
        }
        let app = make_app_with_events(make_test_config(), HashMap::new(), Some(event_tx.clone()));
        sse_body(app, request, event_tx).await
    }

    #[tokio::test]
    async fn test_sse_replays_missed_events_since_id() {
        let sent: Vec<TileEvent> = [101, 102, 103]
            .into_iter()
            .map(|ms| generated("test_source", ms))
            .collect();

        // A map re-subscribing after having seen event 1.
        let request = Request::builder()
            .uri("/events?since=1")
            .body(Body::empty())
            .unwrap();
        let body = sse_after(&sent, request).await;
        assert!(!body.contains("101") && body.contains("102") && body.contains("103"));
        assert!(body.contains("id: 3"));
        assert!(body.contains("event: ready"));
        assert!(!body.contains("event: resync"));
        // Replayed events come before `ready`.
        assert!(body.find("103").unwrap() < body.find("event: ready").unwrap());

        // EventSource's own retry sends Last-Event-ID instead.
        let request = Request::builder()
            .uri("/events")
            .header("last-event-id", "2")
            .body(Body::empty())
            .unwrap();
        let body = sse_after(&sent, request).await;
        assert!(!body.contains("102") && body.contains("103"));

        // The replay is filtered by the new viewport's sources too.
        let request = Request::builder()
            .uri("/events?since=0&source=test_source")
            .body(Body::empty())
            .unwrap();
        let mut other = sent.clone();
        other.push(generated("test_source_2", 999));
        let body = sse_after(&other, request).await;
        assert!(body.contains("101") && !body.contains("999"));
    }

    #[tokio::test]
    async fn test_sse_asks_for_resync_when_history_is_gone() {
        // An id this server never issued (it restarted since).
        let request = Request::builder()
            .uri("/events?since=500")
            .body(Body::empty())
            .unwrap();
        let body = sse_after(&[generated("test_source", 1)], request).await;
        assert!(body.contains("event: resync"));
        assert!(body.contains("event: ready"));
    }

    #[test]
    fn test_cooldown_flush_holds_id_below_pending_events() {
        let now = tokio::time::Instant::now();
        let mut pending = HashMap::new();
        pending.insert(
            "a".to_string(),
            PendingEvent {
                event: generated("a", 1),
                started: now,
                first_id: 7,
                last_id: 9,
            },
        );
        pending.insert(
            "b".to_string(),
            PendingEvent {
                event: generated("b", 1),
                started: now,
                first_id: 5,
                last_id: 8,
            },
        );
        // Flush only "a": "b" (from id 5) is still waiting, so resuming from
        // "a"'s id must not skip it.
        let sent = flush_pending(&mut pending, |p| p.first_id == 7);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 4);
        assert_eq!(sent[0].1.source(), "a");
        assert_eq!(pending.len(), 1);
        // With nothing else pending, the event's own last id is used.
        let sent = flush_pending(&mut pending, |_| true);
        assert_eq!(sent[0].0, 8);
    }

    #[tokio::test]
    async fn test_sse_rejects_bad_viewport() {
        let config = make_test_config();
        let event_tx = crate::events::create_event_bus();
        for uri in [
            "/events?bbox=1,2,3",
            "/events?zoom=99",
            "/events?zoom=abc",
            "/events?source=ghost",
        ] {
            let app = make_app_with_events(config.clone(), HashMap::new(), Some(event_tx.clone()));
            let response = send_request(
                app,
                Request::builder().uri(uri).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        }
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
