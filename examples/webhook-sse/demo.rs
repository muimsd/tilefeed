//! Standalone demo of webhook + SSE notifications.
//!
//! Run with: cargo run --example webhook_sse_demo
//!
//! This starts:
//!   1. A webhook receiver on port 9000
//!   2. A tile server with SSE on port 3000
//!   3. Simulates tile events every 3 seconds
//!
//! Test with:
//!   curl -N http://localhost:3000/events     (SSE stream)
//!   open examples/webhook-sse/map.html       (MapLibre live map)

use axum::{
    http::HeaderMap,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::{get, post},
    Router,
};
use std::collections::HashSet;
use std::convert::Infallible;
use tokio::sync::broadcast;

// ── Event types (mirrors src/events.rs) ─────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum TileEvent {
    GenerateComplete {
        source: String,
        duration_ms: u64,
    },
    UpdateComplete {
        source: String,
        tiles_updated: usize,
        affected_zooms: Vec<u8>,
        layers_affected: Vec<String>,
    },
}

type EventSender = broadcast::Sender<TileEvent>;

// ── SSE handler ─────────────────────────────────────────────────

async fn sse_handler(
    axum::extract::State(tx): axum::extract::State<EventSender>,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
    let mut rx = tx.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let event_type = match &event {
                        TileEvent::GenerateComplete { .. } => "generate_complete",
                        TileEvent::UpdateComplete { .. } => "update_complete",
                    };
                    if let Ok(data) = serde_json::to_string(&event) {
                        yield Ok(SseEvent::default().event(event_type).data(data));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║        Tilefeed — Webhook + SSE Demo                    ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    let (event_tx, _) = broadcast::channel::<TileEvent>(256);

    // ── 1. Start webhook receiver on port 9000 ──────────────────
    let webhook_app = Router::new().route(
        "/hooks/tile-update",
        post(|headers: HeaderMap, body: String| async move {
            let event = headers
                .get("X-Tilefeed-Event")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown");
            let sig = headers
                .get("X-Tilefeed-Signature")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none");

            println!("  📨 Webhook received!");
            println!("     Event:     {}", event);
            println!("     Signature: {}", sig);

            // Pretty-print the JSON
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                println!(
                    "     Payload:   {}",
                    serde_json::to_string_pretty(&json).unwrap_or(body)
                );
            }
            println!();

            "ok"
        }),
    );

    let webhook_listener = tokio::net::TcpListener::bind("127.0.0.1:9000")
        .await
        .expect("Failed to bind webhook receiver on port 9000");
    println!("  ✓ Webhook receiver listening on http://127.0.0.1:9000/hooks/tile-update");

    tokio::spawn(async move {
        axum::serve(webhook_listener, webhook_app).await.unwrap();
    });

    // ── 2. Start tile server with SSE on port 3000 ──────────────
    let sse_app = Router::new()
        .route("/events", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(event_tx.clone());

    let sse_listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .expect("Failed to bind SSE server on port 3000");
    println!("  ✓ SSE endpoint available at    http://127.0.0.1:3000/events");
    println!();
    println!("────────────────────────────────────────────────────────────");
    println!("  Try these:");
    println!("    curl -N http://localhost:3000/events");
    println!("    open examples/webhook-sse/map.html");
    println!("────────────────────────────────────────────────────────────");
    println!();

    tokio::spawn(async move {
        axum::serve(sse_listener, sse_app).await.unwrap();
    });

    // ── 3. Start webhook notifier (sends to port 9000) ──────────
    let webhook_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut rx = webhook_tx.subscribe();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        loop {
            match rx.recv().await {
                Ok(event) => {
                    let payload = serde_json::to_string(&event).unwrap();
                    let event_type = match &event {
                        TileEvent::GenerateComplete { .. } => "generate_complete",
                        TileEvent::UpdateComplete { .. } => "update_complete",
                    };

                    // Compute HMAC signature
                    use hmac::{Hmac, Mac};
                    use sha2::Sha256;
                    let secret = "demo-secret";
                    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
                    mac.update(payload.as_bytes());
                    let sig = hex::encode(mac.finalize().into_bytes());

                    let _ = client
                        .post("http://127.0.0.1:9000/hooks/tile-update")
                        .header("Content-Type", "application/json")
                        .header("X-Tilefeed-Event", event_type)
                        .header("X-Tilefeed-Signature", format!("sha256={}", sig))
                        .body(payload)
                        .send()
                        .await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // ── 4. Simulate tile events ─────────────────────────────────
    println!("Simulating tile events every 3 seconds...");
    println!();

    let sources = ["parks", "basemap", "poi"];
    let layers_map: std::collections::HashMap<&str, Vec<&str>> = [
        ("parks", vec!["parks", "trails"]),
        ("basemap", vec!["buildings", "roads", "water"]),
        ("poi", vec!["restaurants", "shops"]),
    ]
    .into_iter()
    .collect();

    let mut cycle = 0u64;
    loop {
        let source = sources[cycle as usize % sources.len()];
        let layers = &layers_map[source];

        if cycle % 3 == 0 {
            // Every 3rd cycle: simulate a full generation
            let duration = 800 + (cycle * 137) % 2000;
            println!(
                "  🔄 Simulating generate_complete for \"{}\" ({}ms)",
                source, duration
            );
            let _ = event_tx.send(TileEvent::GenerateComplete {
                source: source.to_string(),
                duration_ms: duration,
            });
        } else {
            // Otherwise: simulate incremental update
            let tiles = 1 + (cycle * 31) % 100;
            let mut zooms = HashSet::new();
            zooms.insert((8 + cycle % 7) as u8);
            zooms.insert((10 + cycle % 5) as u8);
            let mut sorted_zooms: Vec<u8> = zooms.into_iter().collect();
            sorted_zooms.sort();

            let affected_layer = layers[cycle as usize % layers.len()];
            println!(
                "  📝 Simulating update_complete for \"{}\" ({} tiles, zooms {:?}, layer: {})",
                source, tiles, sorted_zooms, affected_layer
            );
            let _ = event_tx.send(TileEvent::UpdateComplete {
                source: source.to_string(),
                tiles_updated: tiles as usize,
                affected_zooms: sorted_zooms,
                layers_affected: vec![affected_layer.to_string()],
            });
        }

        cycle += 1;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}
