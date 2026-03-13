use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::config::WebhookConfig;
use crate::events::{EventSender, TileEvent};

type HmacSha256 = Hmac<Sha256>;

pub struct WebhookNotifier {
    config: WebhookConfig,
    client: reqwest::Client,
}

impl WebhookNotifier {
    pub fn new(config: WebhookConfig) -> Self {
        let timeout = Duration::from_millis(config.timeout_ms.unwrap_or(5000));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { config, client }
    }

    /// Start a background task that listens for events and sends webhooks.
    /// When `cooldown_secs` is set, events are accumulated per source and
    /// only sent after the cooldown window expires (trailing-edge throttle).
    pub fn start(self, event_tx: &EventSender) {
        let cooldown = self
            .config
            .cooldown_secs
            .filter(|&s| s > 0)
            .map(Duration::from_secs);

        match cooldown {
            Some(cooldown) => self.start_throttled(event_tx, cooldown),
            None => self.start_immediate(event_tx),
        }
    }

    /// No cooldown — send every event immediately
    fn start_immediate(self, event_tx: &EventSender) {
        let mut rx = event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if self.should_send(&event) {
                            self.send_all(&event).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Webhook consumer lagged, dropped {} events", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    /// With cooldown — accumulate events per source and flush after the window
    fn start_throttled(self, event_tx: &EventSender, cooldown: Duration) {
        let mut rx = event_tx.subscribe();
        tokio::spawn(async move {
            // Per-source: accumulated event + when the cooldown window started
            let mut pending: HashMap<String, (TileEvent, Instant)> = HashMap::new();

            loop {
                // Calculate how long until the earliest pending source needs flushing
                let next_flush = pending
                    .values()
                    .map(|(_, started)| *started + cooldown)
                    .min();

                let sleep_until =
                    next_flush.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

                tokio::select! {
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                if !self.should_send(&event) {
                                    continue;
                                }
                                let source = event.source().to_string();
                                match pending.get_mut(&source) {
                                    Some((existing, _started)) => {
                                        // Merge into existing pending event
                                        existing.merge(&event);
                                    }
                                    None => {
                                        // First event for this source in this window
                                        pending.insert(source, (event, Instant::now()));
                                    }
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Webhook consumer lagged, dropped {} events", n);
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                // Flush all remaining pending events before exit
                                for (_, (event, _)) in pending.drain() {
                                    self.send_all(&event).await;
                                }
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(sleep_until) => {
                        // Flush sources whose cooldown has expired
                        let now = Instant::now();
                        let expired: Vec<String> = pending
                            .iter()
                            .filter(|(_, (_, started))| now >= *started + cooldown)
                            .map(|(source, _)| source.clone())
                            .collect();

                        for source in expired {
                            if let Some((event, _)) = pending.remove(&source) {
                                info!(
                                    "Cooldown expired for source '{}', sending aggregated webhook",
                                    source
                                );
                                self.send_all(&event).await;
                            }
                        }
                    }
                }
            }
        });
    }

    fn should_send(&self, event: &TileEvent) -> bool {
        match event {
            TileEvent::GenerateComplete { .. } => self.config.on_generate_enabled(),
            TileEvent::UpdateComplete { .. } => self.config.on_update_enabled(),
        }
    }

    async fn send_all(&self, event: &TileEvent) {
        let payload = match serde_json::to_string(event) {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to serialize webhook payload: {}", e);
                return;
            }
        };

        for url in &self.config.urls {
            let url = url.clone();
            let payload = payload.clone();
            let client = self.client.clone();
            let secret = self.config.secret.clone();
            let max_retries = self.config.retry_count.unwrap_or(2);

            tokio::spawn(async move {
                send_with_retries(&client, &url, &payload, secret.as_deref(), max_retries).await;
            });
        }
    }
}

async fn send_with_retries(
    client: &reqwest::Client,
    url: &str,
    payload: &str,
    secret: Option<&str>,
    max_retries: u32,
) {
    let event_type = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let backoff = Duration::from_millis(500 * 2u64.pow(attempt - 1));
            tokio::time::sleep(backoff).await;
        }

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("User-Agent", "tilefeed-webhook")
            .header("X-Tilefeed-Event", &event_type);

        if let Some(secret) = secret {
            let signature = compute_signature(secret, payload);
            req = req.header("X-Tilefeed-Signature", format!("sha256={}", signature));
        }

        match req.body(payload.to_string()).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!("Webhook delivered to {}", url);
                return;
            }
            Ok(resp) => {
                warn!(
                    "Webhook to {} returned status {} (attempt {}/{})",
                    url,
                    resp.status(),
                    attempt + 1,
                    max_retries + 1
                );
            }
            Err(e) => {
                warn!(
                    "Webhook to {} failed: {} (attempt {}/{})",
                    url,
                    e,
                    attempt + 1,
                    max_retries + 1
                );
            }
        }
    }
    error!(
        "Webhook delivery to {} failed after {} retries",
        url, max_retries
    );
}

fn compute_signature(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn test_compute_signature() {
        let sig = compute_signature("my-secret", r#"{"event":"update_complete"}"#);
        assert!(!sig.is_empty());
        assert_eq!(sig.len(), 64); // SHA256 = 32 bytes = 64 hex chars
    }

    #[test]
    fn test_compute_signature_deterministic() {
        let sig1 = compute_signature("key", "payload");
        let sig2 = compute_signature("key", "payload");
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_compute_signature_different_keys() {
        let sig1 = compute_signature("key1", "payload");
        let sig2 = compute_signature("key2", "payload");
        assert_ne!(sig1, sig2);
    }

    /// Spin up a real HTTP server, send an event through the bus,
    /// and verify the webhook is delivered with correct headers and payload.
    #[tokio::test]
    async fn test_webhook_delivers_to_http_server() {
        use axum::{routing::post, Router};

        // Shared state to capture the received request
        let received_body: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_event_header: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let body_clone = received_body.clone();
        let header_clone = received_event_header.clone();

        let app = Router::new().route(
            "/hook",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let body_clone = body_clone.clone();
                let header_clone = header_clone.clone();
                async move {
                    *body_clone.lock().await = Some(body);
                    *header_clone.lock().await = headers
                        .get("X-Tilefeed-Event")
                        .map(|v| v.to_str().unwrap_or("").to_string());
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = WebhookConfig {
            urls: vec![format!("http://{}/hook", addr)],
            timeout_ms: Some(5000),
            retry_count: Some(0),
            on_generate: Some(true),
            on_update: Some(true),
            secret: None,
            cooldown_secs: None,
        };

        let event_tx = crate::events::create_event_bus();
        let notifier = WebhookNotifier::new(config);
        notifier.start(&event_tx);

        event_tx
            .send(crate::events::TileEvent::GenerateComplete {
                source: "test_src".to_string(),
                duration_ms: 500,
            })
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let body = received_body.lock().await;
        assert!(body.is_some(), "Webhook was not received");
        let body_str = body.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_str(body_str).unwrap();
        assert_eq!(json["event"], "generate_complete");
        assert_eq!(json["source"], "test_src");
        assert_eq!(json["duration_ms"], 500);

        let header = received_event_header.lock().await;
        assert_eq!(header.as_deref(), Some("generate_complete"));
    }

    /// Test that HMAC signature is correctly sent and verifiable
    #[tokio::test]
    async fn test_webhook_with_hmac_signature() {
        use axum::{routing::post, Router};

        let received_sig: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let received_body: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sig_clone = received_sig.clone();
        let body_clone = received_body.clone();

        let app = Router::new().route(
            "/hook",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let sig_clone = sig_clone.clone();
                let body_clone = body_clone.clone();
                async move {
                    *sig_clone.lock().await = headers
                        .get("X-Tilefeed-Signature")
                        .map(|v| v.to_str().unwrap_or("").to_string());
                    *body_clone.lock().await = Some(body);
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let secret = "test-secret-key";
        let config = WebhookConfig {
            urls: vec![format!("http://{}/hook", addr)],
            timeout_ms: Some(5000),
            retry_count: Some(0),
            on_generate: Some(true),
            on_update: Some(true),
            secret: Some(secret.to_string()),
            cooldown_secs: None,
        };

        let event_tx = crate::events::create_event_bus();
        let notifier = WebhookNotifier::new(config);
        notifier.start(&event_tx);

        event_tx
            .send(crate::events::TileEvent::GenerateComplete {
                source: "signed_test".to_string(),
                duration_ms: 100,
            })
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let sig = received_sig.lock().await;
        assert!(sig.is_some(), "Signature header was not received");
        let sig_str = sig.as_ref().unwrap();
        assert!(
            sig_str.starts_with("sha256="),
            "Signature should start with sha256="
        );

        let body = received_body.lock().await;
        let expected_sig = compute_signature(secret, body.as_ref().unwrap());
        assert_eq!(sig_str, &format!("sha256={}", expected_sig));
    }

    /// Test that on_update=false skips update events
    #[tokio::test]
    async fn test_webhook_respects_on_update_false() {
        use axum::{routing::post, Router};

        let call_count: Arc<std::sync::atomic::AtomicU32> =
            Arc::new(std::sync::atomic::AtomicU32::new(0));
        let count_clone = call_count.clone();

        let app = Router::new().route(
            "/hook",
            post(move |_body: String| {
                let count_clone = count_clone.clone();
                async move {
                    count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = WebhookConfig {
            urls: vec![format!("http://{}/hook", addr)],
            timeout_ms: Some(5000),
            retry_count: Some(0),
            on_generate: Some(true),
            on_update: Some(false),
            secret: None,
            cooldown_secs: None,
        };

        let event_tx = crate::events::create_event_bus();
        let notifier = WebhookNotifier::new(config);
        notifier.start(&event_tx);

        let mut zooms = std::collections::HashSet::new();
        zooms.insert(5u8);
        event_tx
            .send(crate::events::TileEvent::update_complete(
                "src".to_string(),
                10,
                &zooms,
                14,
                vec!["layer".to_string()],
            ))
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Update event should not be sent when on_update=false"
        );

        event_tx
            .send(crate::events::TileEvent::GenerateComplete {
                source: "src".to_string(),
                duration_ms: 100,
            })
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Generate event should still be sent"
        );
    }

    /// Test cooldown: rapid events are batched and sent as one after the window
    #[tokio::test]
    async fn test_webhook_cooldown_batches_events() {
        use axum::{routing::post, Router};

        let received_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_clone = received_bodies.clone();

        let app = Router::new().route(
            "/hook",
            post(move |body: String| {
                let bodies_clone = bodies_clone.clone();
                async move {
                    bodies_clone.lock().await.push(body);
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = WebhookConfig {
            urls: vec![format!("http://{}/hook", addr)],
            timeout_ms: Some(5000),
            retry_count: Some(0),
            on_generate: Some(true),
            on_update: Some(true),
            secret: None,
            cooldown_secs: Some(2), // 2 second cooldown
        };

        let event_tx = crate::events::create_event_bus();
        let notifier = WebhookNotifier::new(config);
        notifier.start(&event_tx);

        // Send 3 rapid update events for the same source
        for i in 0..3 {
            let mut zooms = std::collections::HashSet::new();
            zooms.insert(10 + i as u8);
            event_tx
                .send(crate::events::TileEvent::update_complete(
                    "parks".to_string(),
                    10 + i,
                    &zooms,
                    14,
                    vec![format!("layer{}", i)],
                ))
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // After 500ms, nothing should have been sent yet (cooldown is 2s)
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            received_bodies.lock().await.len(),
            0,
            "No webhook should fire during cooldown"
        );

        // Wait for cooldown to expire (total ~2.3s from first event)
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let bodies = received_bodies.lock().await;
        assert_eq!(
            bodies.len(),
            1,
            "Should receive exactly 1 aggregated webhook"
        );

        let json: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(json["event"], "update_complete");
        assert_eq!(json["source"], "parks");
        // tiles_updated should be sum: 10 + 11 + 12 = 33
        assert_eq!(json["tiles_updated"], 33);
        // affected_zooms should include all three: 10, 11, 12
        let zooms = json["affected_zooms"].as_array().unwrap();
        assert_eq!(zooms.len(), 3);
    }

    /// Test that different sources have independent cooldowns
    #[tokio::test]
    async fn test_webhook_cooldown_independent_sources() {
        use axum::{routing::post, Router};

        let received_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_clone = received_bodies.clone();

        let app = Router::new().route(
            "/hook",
            post(move |body: String| {
                let bodies_clone = bodies_clone.clone();
                async move {
                    bodies_clone.lock().await.push(body);
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = WebhookConfig {
            urls: vec![format!("http://{}/hook", addr)],
            timeout_ms: Some(5000),
            retry_count: Some(0),
            on_generate: Some(true),
            on_update: Some(true),
            secret: None,
            cooldown_secs: Some(1),
        };

        let event_tx = crate::events::create_event_bus();
        let notifier = WebhookNotifier::new(config);
        notifier.start(&event_tx);

        // Send events for two different sources
        let mut zooms = std::collections::HashSet::new();
        zooms.insert(10u8);
        event_tx
            .send(crate::events::TileEvent::update_complete(
                "parks".to_string(),
                5,
                &zooms,
                14,
                vec!["parks".to_string()],
            ))
            .unwrap();
        event_tx
            .send(crate::events::TileEvent::update_complete(
                "basemap".to_string(),
                8,
                &zooms,
                14,
                vec!["roads".to_string()],
            ))
            .unwrap();

        // Wait for cooldown
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let bodies = received_bodies.lock().await;
        assert_eq!(
            bodies.len(),
            2,
            "Should receive 2 webhooks (one per source)"
        );

        let sources: Vec<String> = bodies
            .iter()
            .map(|b| {
                let json: serde_json::Value = serde_json::from_str(b).unwrap();
                json["source"].as_str().unwrap().to_string()
            })
            .collect();
        assert!(sources.contains(&"parks".to_string()));
        assert!(sources.contains(&"basemap".to_string()));
    }
}
