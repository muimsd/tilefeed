//! Standalone demo of webhook + SSE notifications, no database needed.
//!
//! Run with: cargo run --example webhook_sse_demo
//!
//! This starts:
//!   1. A webhook receiver on port 9000
//!   2. An SSE server on port 3000, with the same `/events` as `tilefeed
//!      serve`: viewport filter (`bbox`, `zoom`, `source`), event ids, replay
//!      with `since` / `Last-Event-ID`, and the `ready` / `resync` events
//!   3. Simulated edits every 3 seconds, around Paris, Tokyo and New York
//!
//! Test with:
//!   curl -N http://localhost:3000/events                         (everything)
//!   curl -N 'http://localhost:3000/events?bbox=2.2,48.8,2.4,48.9&zoom=12&source=parks'
//!                                                                (Paris parks only)
//!   open examples/webhook-sse/map.html                           (MapLibre live map)
//!
//! The event bus and viewport filter are tilefeed's own (`src/events.rs`,
//! `src/viewport.rs`), so the demo can't drift from the real server. The map
//! shows no features: the demo serves empty tiles, but the event log and
//! the tile refreshes work as they do against `tilefeed serve`.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::{get, post},
    Router,
};
use std::collections::HashSet;
use std::convert::Infallible;
use tokio::sync::broadcast;

// tilefeed is a binary crate, so its modules are included by path. They only
// need `Bounds` from the PostGIS reader.
#[allow(dead_code)]
#[path = "../../src/events.rs"]
mod events;
#[allow(dead_code)]
#[path = "../../src/tiles.rs"]
mod tiles;
#[allow(dead_code)]
#[path = "../../src/viewport.rs"]
mod viewport;
mod postgis {
    #[derive(Debug, Clone)]
    pub struct Bounds {
        pub min_lon: f64,
        pub min_lat: f64,
        pub max_lon: f64,
        pub max_lat: f64,
    }
}

use events::{create_event_bus, ChangedArea, EventSender, Subscription, TileEvent};
use postgis::Bounds;
use viewport::{EventsQuery, Viewport};

/// Simulated sources and their layers; every source covers zooms 0-14.
const SOURCES: [(&str, &[&str]); 3] = [
    ("parks", &["parks", "trails"]),
    ("basemap", &["buildings", "roads", "water"]),
    ("poi", &["restaurants", "shops"]),
];
const MAX_ZOOM: u8 = 14;
const PLACES: [(&str, f64, f64); 3] = [
    ("Paris", 2.35, 48.86),
    ("Tokyo", 139.70, 35.68),
    ("New York", -73.98, 40.75),
];

// ── SSE handler: the same stream `tilefeed serve` sends ─────────

async fn sse_handler(
    State(bus): State<EventSender>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>>, (StatusCode, String)>
{
    let mut viewport = Viewport::parse(&query, SOURCES.iter().map(|(name, _)| *name))
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
    } = bus.subscribe_since(since);

    let stream = async_stream::stream! {
        if !complete {
            yield Ok(resync_sse());
        }
        for missed in replay {
            if viewport.wants(&missed.event) {
                yield Ok(event_to_sse(&missed.event, missed.id));
            }
        }
        yield Ok(SseEvent::default()
            .event("ready")
            .id(latest_id.to_string())
            .data(format!(r#"{{"last_event_id":{latest_id}}}"#)));

        loop {
            match rx.recv().await {
                Ok(e) if !viewport.wants(&e.event) => continue,
                Ok(e) => yield Ok(event_to_sse(&e.event, e.id)),
                Err(broadcast::error::RecvError::Lagged(_)) => yield Ok(resync_sse()),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

fn event_name(event: &TileEvent) -> &'static str {
    match event {
        TileEvent::GenerateComplete { .. } => "generate_complete",
        TileEvent::UpdateComplete { .. } => "update_complete",
    }
}

fn event_to_sse(event: &TileEvent, id: u64) -> SseEvent {
    SseEvent::default()
        .event(event_name(event))
        .id(id.to_string())
        .data(serde_json::to_string(event).unwrap())
}

fn resync_sse() -> SseEvent {
    SseEvent::default()
        .event("resync")
        .data(r#"{"reason":"missed_events"}"#)
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("Tilefeed: webhook + SSE demo");
    println!();

    let bus = create_event_bus();

    // ── 1. Webhook receiver on port 9000 ────────────────────────
    let webhook_app = Router::new().route(
        "/hooks/tile-update",
        post(|headers: HeaderMap, body: String| async move {
            let header = |name| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string()
            };
            println!(
                "  📨 Webhook {} (signature {})",
                header("X-Tilefeed-Event"),
                header("X-Tilefeed-Signature")
            );
            println!("     {body}");
            "ok"
        }),
    );
    let webhook_listener = tokio::net::TcpListener::bind("127.0.0.1:9000")
        .await
        .expect("Failed to bind webhook receiver on port 9000");
    tokio::spawn(async move { axum::serve(webhook_listener, webhook_app).await.unwrap() });

    // ── 2. SSE server on port 3000 ──────────────────────────────
    // The map page is usually opened from disk or another port, so allow
    // any origin. Tiles are empty: there is no database behind the demo.
    let sse_app = Router::new()
        .route("/events", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .route(
            "/{source}/{z}/{x}/{y}",
            get(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(bus.clone());
    let sse_listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("Failed to bind SSE server on port 3000");
    tokio::spawn(async move { axum::serve(sse_listener, sse_app).await.unwrap() });

    println!("  ✓ Webhook receiver  http://127.0.0.1:9000/hooks/tile-update");
    println!("  ✓ SSE endpoint      http://127.0.0.1:3000/events");
    println!();
    println!("  Try:");
    println!("    curl -N http://localhost:3000/events");
    println!(
        "    curl -N 'http://localhost:3000/events?bbox=2.2,48.8,2.4,48.9&zoom=12&source=parks'"
    );
    println!("    open examples/webhook-sse/map.html   (zoom to Paris, Tokyo or New York)");
    println!();

    // ── 3. Webhook notifier: posts every event to port 9000 ─────
    let mut webhook_rx = bus.subscribe();
    tokio::spawn(async move {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        loop {
            let event = match webhook_rx.recv().await {
                Ok(e) => e.event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            // Webhook payloads are the event JSON; ids exist only on SSE.
            let payload = serde_json::to_string(&event).unwrap();
            let mut mac = Hmac::<Sha256>::new_from_slice(b"demo-secret").unwrap();
            mac.update(payload.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            let _ = client
                .post("http://127.0.0.1:9000/hooks/tile-update")
                .header("Content-Type", "application/json")
                .header("X-Tilefeed-Event", event_name(&event))
                .header("X-Tilefeed-Signature", format!("sha256={sig}"))
                .body(payload)
                .send()
                .await;
        }
    });

    // ── 4. Simulated edits ──────────────────────────────────────
    println!("Simulating an edit every 3 seconds...");
    println!();
    for cycle in 0usize.. {
        let (source, layers) = SOURCES[cycle % SOURCES.len()];

        let event = if cycle % 7 == 6 {
            // Now and then a full rebuild, which every client receives.
            println!("  🔄 generate_complete for \"{source}\"");
            TileEvent::GenerateComplete {
                source: source.to_string(),
                duration_ms: 800 + (cycle as u64 * 137) % 2000,
            }
        } else {
            let (place, lon, lat) = PLACES[(cycle / SOURCES.len()) % PLACES.len()];
            let layer = layers[cycle % layers.len()];
            // A feature about 1 km across, drawn from zoom 10 to the source's max
            let d = 0.005;
            let bounds = Bounds {
                min_lon: lon - d,
                min_lat: lat - d,
                max_lon: lon + d,
                max_lat: lat + d,
            };
            let zooms: HashSet<u8> = (10..=MAX_ZOOM).collect();
            println!("  📝 update_complete for \"{source}\" ({layer} near {place})");
            TileEvent::update_complete(
                source.to_string(),
                zooms.len(),
                &zooms,
                MAX_ZOOM,
                vec![layer.to_string()],
            )
            .with_areas(vec![ChangedArea::new(&bounds, 10, MAX_ZOOM)])
        };
        let _ = bus.send(event);

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
