//! Prometheus metrics: a small registry plus the text exposition format.
//!
//! The registry is a process-wide singleton reached through [`metrics()`], so any
//! module can record without threading a handle through call signatures. Metric
//! families are pre-declared in [`Metrics::new`]; label sets are created lazily on
//! first use and live for the process lifetime, so labels must stay low-cardinality
//! (source names, layer names, fixed result strings — never tile coordinates).

use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;
use tracing::info;

/// Bucket bounds (seconds) for sub-second work: tile reads, webhook deliveries.
const FAST_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];

/// Bucket bounds (seconds) for long-running work: full generation, batch updates, publishing.
const SLOW_BUCKETS: &[f64] = &[
    0.05, 0.25, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
];

/// Monotonically increasing count.
#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }

    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Value that can go up or down.
#[derive(Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }

    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Histogram with fixed bucket bounds.
///
/// Each observation touches exactly one bucket; the cumulative counts Prometheus
/// wants are summed at render time. That keeps the hot path to a single atomic
/// increment rather than one per bound.
pub struct Histogram {
    bounds: &'static [f64],
    /// Observations falling in each bound's slot, plus a final slot for `+Inf`.
    counts: Vec<AtomicU64>,
    count: AtomicU64,
    /// Sum of observed values, stored as `f64` bits.
    sum_bits: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            // One slot per bound, plus the +Inf overflow slot
            counts: (0..bounds.len() + 1).map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_bits: AtomicU64::new(0.0f64.to_bits()),
        }
    }

    pub fn observe(&self, value: f64) {
        let slot = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());
        self.counts[slot].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);

        // f64 has no atomic type; accumulate through a compare-exchange loop.
        let mut current = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(current) + value).to_bits();
            match self.sum_bits.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Observe a duration since `start`, in seconds.
    pub fn observe_since(&self, start: Instant) {
        self.observe(start.elapsed().as_secs_f64());
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
    }
}

/// A metric family: one name, one help string, and a child metric per label set.
pub struct Family<M> {
    name: &'static str,
    help: &'static str,
    label_names: &'static [&'static str],
    children: RwLock<HashMap<Vec<String>, Arc<M>>>,
    /// Builds a child when a label set is seen for the first time.
    factory: fn() -> M,
}

impl<M> Family<M> {
    fn new(
        name: &'static str,
        help: &'static str,
        label_names: &'static [&'static str],
        factory: fn() -> M,
    ) -> Self {
        Self {
            name,
            help,
            label_names,
            children: RwLock::new(HashMap::new()),
            factory,
        }
    }

    /// Child metric for a label set. `labels` must line up with `label_names`.
    pub fn with(&self, labels: &[&str]) -> Arc<M> {
        debug_assert_eq!(
            labels.len(),
            self.label_names.len(),
            "metric '{}' expects {} label(s)",
            self.name,
            self.label_names.len()
        );

        let key: Vec<String> = labels.iter().map(|l| l.to_string()).collect();

        // A map of counters is still perfectly usable after a panic elsewhere, so
        // recover through the poison rather than losing every metric for the rest
        // of the process lifetime.
        if let Some(child) = self
            .children
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return child.clone();
        }

        self.children
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert_with(|| Arc::new((self.factory)()))
            .clone()
    }

    /// Child metric of a family declared without labels.
    pub fn get(&self) -> Arc<M> {
        self.with(&[])
    }

    /// Label sets sorted by label values, so exposition output is deterministic.
    fn sorted_children(&self) -> Vec<(Vec<String>, Arc<M>)> {
        let mut entries: Vec<(Vec<String>, Arc<M>)> = self
            .children
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    fn write_header(&self, kind: &str, out: &mut String) {
        out.push_str(&format!(
            "# HELP {} {}\n",
            self.name,
            escape_help(self.help)
        ));
        out.push_str(&format!("# TYPE {} {}\n", self.name, kind));
    }

    /// Renders `{label="value",...}`, or an empty string for an unlabeled family.
    fn label_block(&self, values: &[String], extra: Option<(&str, String)>) -> String {
        let mut parts: Vec<String> = self
            .label_names
            .iter()
            .zip(values)
            .map(|(name, value)| format!("{}=\"{}\"", name, escape_label(value)))
            .collect();

        if let Some((name, value)) = extra {
            parts.push(format!("{}=\"{}\"", name, escape_label(&value)));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", parts.join(","))
        }
    }
}

impl Family<Counter> {
    fn encode(&self, out: &mut String) {
        let children = self.sorted_children();
        if children.is_empty() {
            return;
        }
        self.write_header("counter", out);
        for (labels, counter) in children {
            out.push_str(&format!(
                "{}{} {}\n",
                self.name,
                self.label_block(&labels, None),
                counter.get()
            ));
        }
    }
}

impl Family<Gauge> {
    fn encode(&self, out: &mut String) {
        let children = self.sorted_children();
        if children.is_empty() {
            return;
        }
        self.write_header("gauge", out);
        for (labels, gauge) in children {
            out.push_str(&format!(
                "{}{} {}\n",
                self.name,
                self.label_block(&labels, None),
                gauge.get()
            ));
        }
    }
}

impl Family<Histogram> {
    fn encode(&self, out: &mut String) {
        let children = self.sorted_children();
        if children.is_empty() {
            return;
        }
        self.write_header("histogram", out);
        for (labels, histogram) in children {
            // Read the total first, then clamp every bucket to it. Observations
            // landing mid-render would otherwise push a finite bucket above
            // `+Inf`, which Prometheus reads as a corrupt histogram; clamping
            // defers at most one observation to the next scrape instead.
            let total = histogram.count();
            let mut cumulative = 0u64;
            for (i, bound) in histogram.bounds.iter().enumerate() {
                cumulative = (cumulative + histogram.counts[i].load(Ordering::Relaxed)).min(total);
                out.push_str(&format!(
                    "{}_bucket{} {}\n",
                    self.name,
                    self.label_block(&labels, Some(("le", format_float(*bound)))),
                    cumulative
                ));
            }
            out.push_str(&format!(
                "{}_bucket{} {}\n",
                self.name,
                self.label_block(&labels, Some(("le", "+Inf".to_string()))),
                total
            ));
            out.push_str(&format!(
                "{}_sum{} {}\n",
                self.name,
                self.label_block(&labels, None),
                format_float(histogram.sum())
            ));
            out.push_str(&format!(
                "{}_count{} {}\n",
                self.name,
                self.label_block(&labels, None),
                total
            ));
        }
    }
}

/// Every metric tilefeed exposes.
pub struct Metrics {
    started: Instant,

    // --- Process ---
    /// Always 1; carries the version as a label for dashboards to join on.
    pub build_info: Family<Gauge>,

    // --- HTTP tile serving ---
    /// `result` is one of: hit, empty, not_modified, source_not_found, error.
    pub tile_requests: Family<Counter>,
    pub tile_bytes: Family<Counter>,
    pub tile_read_duration: Family<Histogram>,
    /// `result` is one of: ok, source_not_found.
    pub tilejson_requests: Family<Counter>,
    pub sse_clients: Family<Gauge>,
    pub sse_connections: Family<Counter>,

    // --- Full generation ---
    /// `result` is `success` or `failure`.
    pub generate_total: Family<Counter>,
    pub generate_duration: Family<Histogram>,

    // --- Incremental updates ---
    /// `result` is one of: routed, unknown_layer, invalid_payload.
    pub notifications: Family<Counter>,
    pub update_batches: Family<Counter>,
    pub tiles_written: Family<Counter>,
    pub tiles_deleted: Family<Counter>,
    pub tile_encode_errors: Family<Counter>,
    pub update_errors: Family<Counter>,
    pub update_duration: Family<Histogram>,
    pub listener_reconnects: Family<Counter>,

    // --- Webhooks ---
    /// `result` is one of: success, http_error, transport_error.
    pub webhook_requests: Family<Counter>,
    pub webhook_retries: Family<Counter>,
    pub webhook_failures: Family<Counter>,
    pub webhook_duration: Family<Histogram>,

    // --- Publishing ---
    /// `result` is `success` or `failure`.
    pub publish_total: Family<Counter>,
    pub publish_duration: Family<Histogram>,
}

impl Metrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),

            build_info: Family::new(
                "tilefeed_build_info",
                "Build information; always 1, the version is carried as a label.",
                &["version"],
                Gauge::default,
            ),

            tile_requests: Family::new(
                "tilefeed_tile_requests_total",
                "Tile requests served, by source and result.",
                &["source", "result"],
                Counter::default,
            ),
            tile_bytes: Family::new(
                "tilefeed_tile_bytes_total",
                "Total bytes of tile data served, by source.",
                &["source"],
                Counter::default,
            ),
            tile_read_duration: Family::new(
                "tilefeed_tile_read_duration_seconds",
                "Time spent reading a tile out of MBTiles, by source.",
                &["source"],
                || Histogram::new(FAST_BUCKETS),
            ),
            tilejson_requests: Family::new(
                "tilefeed_tilejson_requests_total",
                "TileJSON requests served, by source and result.",
                &["source", "result"],
                Counter::default,
            ),
            sse_clients: Family::new(
                "tilefeed_sse_clients",
                "SSE clients currently connected to /events.",
                &[],
                Gauge::default,
            ),
            sse_connections: Family::new(
                "tilefeed_sse_connections_total",
                "SSE connections opened since start.",
                &[],
                Counter::default,
            ),

            generate_total: Family::new(
                "tilefeed_generate_total",
                "Full tile generation runs, by source and result.",
                &["source", "result"],
                Counter::default,
            ),
            generate_duration: Family::new(
                "tilefeed_generate_duration_seconds",
                "Duration of full tile generation, by source.",
                &["source"],
                || Histogram::new(SLOW_BUCKETS),
            ),

            notifications: Family::new(
                "tilefeed_notifications_total",
                "LISTEN/NOTIFY payloads received, by routing result.",
                &["result"],
                Counter::default,
            ),
            update_batches: Family::new(
                "tilefeed_update_batches_total",
                "Debounced update batches applied, by source.",
                &["source"],
                Counter::default,
            ),
            tiles_written: Family::new(
                "tilefeed_tiles_written_total",
                "Tiles written into MBTiles by incremental updates, by source.",
                &["source"],
                Counter::default,
            ),
            tiles_deleted: Family::new(
                "tilefeed_tiles_deleted_total",
                "Tiles deleted from MBTiles by incremental updates (emptied tiles), by source.",
                &["source"],
                Counter::default,
            ),
            tile_encode_errors: Family::new(
                "tilefeed_tile_encode_errors_total",
                "Tiles that failed to regenerate during an incremental update, by source.",
                &["source"],
                Counter::default,
            ),
            update_errors: Family::new(
                "tilefeed_update_errors_total",
                "Incremental update batches that failed, by source.",
                &["source"],
                Counter::default,
            ),
            update_duration: Family::new(
                "tilefeed_update_duration_seconds",
                "Duration of an incremental update batch, by source.",
                &["source"],
                || Histogram::new(SLOW_BUCKETS),
            ),
            listener_reconnects: Family::new(
                "tilefeed_listener_reconnects_total",
                "PostgreSQL LISTEN/NOTIFY reconnection attempts.",
                &[],
                Counter::default,
            ),

            webhook_requests: Family::new(
                "tilefeed_webhook_requests_total",
                "Webhook HTTP attempts, by result.",
                &["result"],
                Counter::default,
            ),
            webhook_retries: Family::new(
                "tilefeed_webhook_retries_total",
                "Webhook delivery retries.",
                &[],
                Counter::default,
            ),
            webhook_failures: Family::new(
                "tilefeed_webhook_failures_total",
                "Webhook deliveries abandoned after exhausting retries.",
                &[],
                Counter::default,
            ),
            webhook_duration: Family::new(
                "tilefeed_webhook_duration_seconds",
                "Duration of a single webhook HTTP attempt.",
                &[],
                || Histogram::new(FAST_BUCKETS),
            ),

            publish_total: Family::new(
                "tilefeed_publish_total",
                "MBTiles artifact publishes, by backend and result.",
                &["backend", "result"],
                Counter::default,
            ),
            publish_duration: Family::new(
                "tilefeed_publish_duration_seconds",
                "Duration of an MBTiles artifact publish, by backend.",
                &["backend"],
                || Histogram::new(SLOW_BUCKETS),
            ),
        }
    }

    /// Materialize the series whose label sets are known up front, so they read 0
    /// instead of being absent. A counter that first appears at 1 gives `rate()`
    /// nothing to compare against, which would keep an alert written for a rare
    /// failure silent on the very first occurrence it was meant to catch.
    fn register_known_series(&self) {
        self.sse_clients.get().set(0);
        self.sse_connections.get().add(0);
        self.listener_reconnects.get().add(0);
        self.webhook_retries.get().add(0);
        self.webhook_failures.get().add(0);

        for result in ["routed", "unknown_layer", "invalid_payload"] {
            self.notifications.with(&[result]).add(0);
        }
        for result in ["success", "http_error", "transport_error"] {
            self.webhook_requests.with(&[result]).add(0);
        }
    }

    /// Seconds since the registry was created (process start, in practice).
    pub fn uptime_seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// Render every metric in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(4096);

        self.build_info.encode(&mut out);

        out.push_str("# HELP tilefeed_uptime_seconds Seconds since the process started.\n");
        out.push_str("# TYPE tilefeed_uptime_seconds gauge\n");
        out.push_str(&format!(
            "tilefeed_uptime_seconds {}\n",
            format_float(self.uptime_seconds())
        ));

        self.tile_requests.encode(&mut out);
        self.tile_bytes.encode(&mut out);
        self.tile_read_duration.encode(&mut out);
        self.tilejson_requests.encode(&mut out);
        self.sse_clients.encode(&mut out);
        self.sse_connections.encode(&mut out);

        self.generate_total.encode(&mut out);
        self.generate_duration.encode(&mut out);

        self.notifications.encode(&mut out);
        self.update_batches.encode(&mut out);
        self.tiles_written.encode(&mut out);
        self.tiles_deleted.encode(&mut out);
        self.tile_encode_errors.encode(&mut out);
        self.update_errors.encode(&mut out);
        self.update_duration.encode(&mut out);
        self.listener_reconnects.encode(&mut out);

        self.webhook_requests.encode(&mut out);
        self.webhook_retries.encode(&mut out);
        self.webhook_failures.encode(&mut out);
        self.webhook_duration.encode(&mut out);

        self.publish_total.encode(&mut out);
        self.publish_duration.encode(&mut out);

        out
    }
}

/// Process-wide metrics registry.
pub fn metrics() -> &'static Metrics {
    static REGISTRY: OnceLock<Metrics> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let m = Metrics::new();
        m.build_info.with(&[env!("CARGO_PKG_VERSION")]).set(1);
        m.register_known_series();
        m
    })
}

/// Create the registry now rather than on the first recorded sample. Call this at
/// startup: `uptime_seconds` counts from the registry's creation, so without it a
/// `watch` process that sits idle for days would report an uptime of milliseconds
/// on its first scrape.
pub fn init() {
    let _ = metrics();
}

/// Content type for the Prometheus text exposition format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Join a configured host and port into a bindable address. A bare IPv6 host has
/// to be bracketed, or `::1` + 9090 would parse as the address `::1:9090`.
pub fn bind_addr(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Increments a gauge and decrements it again when dropped.
pub struct GaugeGuard(Arc<Gauge>);

impl GaugeGuard {
    pub fn new(gauge: Arc<Gauge>) -> Self {
        gauge.inc();
        Self(gauge)
    }
}

impl Drop for GaugeGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// `success` / `failure` label for a `Result`, so call sites stay one-liners.
pub fn result_label<T, E>(result: &Result<T, E>) -> &'static str {
    if result.is_ok() {
        "success"
    } else {
        "failure"
    }
}

/// Trailing zeros are noise in exposition output, but integers still need to
/// parse as floats, so whole numbers are rendered with a single decimal place.
fn format_float(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    if value == value.trunc() && value.abs() < 1e15 {
        format!("{:.1}", value)
    } else {
        format!("{}", value)
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn escape_help(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Run a metrics-only HTTP server. Used by commands that have no tile server of
/// their own (`watch`, `run`) so long-lived processes stay observable.
pub async fn start_metrics_server(host: &str, port: u16, path: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind_addr(host, port)).await?;
    info!(
        "Metrics server listening on http://{}{}",
        listener.local_addr()?,
        path
    );
    serve_metrics(listener, path).await
}

async fn serve_metrics(listener: tokio::net::TcpListener, path: &str) -> Result<()> {
    use axum::{routing::get, Router};

    let app = Router::new().route(path, get(render_handler));
    axum::serve(listener, app).await?;
    Ok(())
}

/// Axum handler for the metrics endpoint, shared by the tile server and the
/// dedicated metrics listener.
pub async fn render_handler() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, CONTENT_TYPE)],
        metrics().render(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter_add_and_inc() {
        let c = Counter::default();
        assert_eq!(c.get(), 0);
        c.inc();
        c.add(41);
        assert_eq!(c.get(), 42);
    }

    #[test]
    fn test_gauge_up_and_down() {
        let g = Gauge::default();
        g.inc();
        g.inc();
        g.dec();
        assert_eq!(g.get(), 1);
        g.set(10);
        assert_eq!(g.get(), 10);
    }

    #[test]
    fn test_gauge_guard_restores_value() {
        let g = Arc::new(Gauge::default());
        {
            let _guard = GaugeGuard::new(g.clone());
            assert_eq!(g.get(), 1);
        }
        assert_eq!(g.get(), 0);
    }

    #[test]
    fn test_histogram_counts_and_sum() {
        let h = Histogram::new(FAST_BUCKETS);
        h.observe(0.002);
        h.observe(0.2);
        h.observe(100.0); // above every bound, so it lands in the +Inf slot

        assert_eq!(h.count(), 3);
        assert!((h.sum() - 100.202).abs() < 1e-9);

        // Each observation lands in exactly one slot; cumulative counts are
        // summed at render time rather than stored.
        assert_eq!(h.counts[1].load(Ordering::Relaxed), 1); // 0.002 -> le=0.005
        assert_eq!(h.counts[6].load(Ordering::Relaxed), 1); // 0.2   -> le=0.25
        assert_eq!(h.counts[FAST_BUCKETS.len()].load(Ordering::Relaxed), 1); // 100.0 -> +Inf
    }

    #[test]
    fn test_histogram_observe_touches_one_slot() {
        let h = Histogram::new(FAST_BUCKETS);
        h.observe(0.002);
        let touched = h
            .counts
            .iter()
            .filter(|c| c.load(Ordering::Relaxed) > 0)
            .count();
        assert_eq!(touched, 1);
    }

    #[test]
    fn test_bind_addr_brackets_ipv6() {
        assert_eq!(bind_addr("127.0.0.1", 3000), "127.0.0.1:3000");
        assert_eq!(bind_addr("0.0.0.0", 9090), "0.0.0.0:9090");
        assert_eq!(bind_addr("::1", 9090), "[::1]:9090");
        assert_eq!(bind_addr("::", 9090), "[::]:9090");
        // Already bracketed hosts are left alone
        assert_eq!(bind_addr("[::1]", 9090), "[::1]:9090");
    }

    #[tokio::test]
    async fn test_ipv6_host_binds() {
        // `::1` + port must not be pasted together as `::1:9090`
        let addr = bind_addr("::1", 0);
        let listener = tokio::net::TcpListener::bind(&addr).await;
        assert!(
            listener.is_ok(),
            "failed to bind {}: {:?}",
            addr,
            listener.err()
        );
    }

    #[test]
    fn test_known_series_are_registered_at_zero() {
        // A counter that first appears at 1 gives rate() nothing to compare
        // against, so the alerts documented for rare failures would stay silent.
        let m = Metrics::new();
        m.register_known_series();
        let out = m.render();

        assert!(out.contains("tilefeed_listener_reconnects_total 0"));
        assert!(out.contains("tilefeed_webhook_failures_total 0"));
        assert!(out.contains("tilefeed_webhook_retries_total 0"));
        assert!(out.contains("tilefeed_notifications_total{result=\"unknown_layer\"} 0"));
        assert!(out.contains("tilefeed_webhook_requests_total{result=\"transport_error\"} 0"));
    }

    #[test]
    fn test_histogram_render_is_cumulative() {
        let family = Family::new("test_hist", "Test histogram.", &["source"], || {
            Histogram::new(FAST_BUCKETS)
        });
        family.with(&["parks"]).observe(0.002);
        family.with(&["parks"]).observe(2.0);

        let mut out = String::new();
        family.encode(&mut out);

        assert!(out.contains("# TYPE test_hist histogram"));
        assert!(out.contains("test_hist_bucket{source=\"parks\",le=\"0.005\"} 1"));
        assert!(out.contains("test_hist_bucket{source=\"parks\",le=\"2.5\"} 2"));
        assert!(out.contains("test_hist_bucket{source=\"parks\",le=\"+Inf\"} 2"));
        assert!(out.contains("test_hist_count{source=\"parks\"} 2"));
        assert!(out.contains("test_hist_sum{source=\"parks\"} 2.002"));
    }

    #[test]
    fn test_family_reuses_child_per_label_set() {
        let family = Family::new("test_total", "Test counter.", &["source"], Counter::default);
        family.with(&["a"]).inc();
        family.with(&["a"]).inc();
        family.with(&["b"]).inc();

        assert_eq!(family.with(&["a"]).get(), 2);
        assert_eq!(family.with(&["b"]).get(), 1);
    }

    #[test]
    fn test_counter_family_render_sorted() {
        let family = Family::new(
            "test_total",
            "Test counter.",
            &["source", "result"],
            Counter::default,
        );
        family.with(&["zebra", "hit"]).inc();
        family.with(&["alpha", "hit"]).add(3);

        let mut out = String::new();
        family.encode(&mut out);

        let alpha = out.find("source=\"alpha\"").unwrap();
        let zebra = out.find("source=\"zebra\"").unwrap();
        assert!(alpha < zebra, "label sets should render sorted");
        assert!(out.contains("test_total{source=\"alpha\",result=\"hit\"} 3"));
        assert!(out.contains("# HELP test_total Test counter."));
    }

    #[test]
    fn test_empty_family_renders_nothing() {
        let family = Family::new("test_total", "Test counter.", &["source"], Counter::default);
        let mut out = String::new();
        family.encode(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn test_unlabeled_family_has_no_label_block() {
        let family = Family::new("test_total", "Test counter.", &[], Counter::default);
        family.get().inc();

        let mut out = String::new();
        family.encode(&mut out);
        assert!(out.contains("test_total 1"));
        assert!(!out.contains('{'));
    }

    #[test]
    fn test_label_values_are_escaped() {
        let family = Family::new("test_total", "Test counter.", &["source"], Counter::default);
        family.with(&["we\"ird\\path"]).inc();

        let mut out = String::new();
        family.encode(&mut out);
        assert!(out.contains("source=\"we\\\"ird\\\\path\""));
    }

    #[test]
    fn test_format_float() {
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(0.005), "0.005");
        assert_eq!(format_float(f64::INFINITY), "+Inf");
    }

    #[test]
    fn test_result_label() {
        let ok: Result<(), anyhow::Error> = Ok(());
        let err: Result<(), anyhow::Error> = Err(anyhow::anyhow!("boom"));
        assert_eq!(result_label(&ok), "success");
        assert_eq!(result_label(&err), "failure");
    }

    #[tokio::test]
    async fn test_standalone_exporter_serves_metrics() {
        // The exporter is the only way `watch` and `run` are observable, so check
        // it end-to-end over a real socket rather than through a Router in-process.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_metrics(listener, "/internal/metrics").await;
        });

        let url = format!("http://{}/internal/metrics", addr);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(CONTENT_TYPE)
        );

        let body = response.text().await.unwrap();
        assert!(body.contains("tilefeed_build_info"));
        assert!(body.contains("tilefeed_uptime_seconds"));

        // The configured path is the only one served
        let missing = reqwest::get(format!("http://{}/metrics", addr))
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_registry_render_includes_build_info_and_uptime() {
        let out = metrics().render();
        assert!(out.contains("tilefeed_build_info{version=\""));
        assert!(out.contains("# TYPE tilefeed_uptime_seconds gauge"));
    }

    #[test]
    fn test_render_has_no_duplicate_help_lines() {
        let m = Metrics::new();
        m.tile_requests.with(&["parks", "hit"]).inc();
        m.tile_requests.with(&["parks", "empty"]).inc();

        let out = m.render();
        let help_lines = out
            .lines()
            .filter(|l| l.starts_with("# HELP tilefeed_tile_requests_total"))
            .count();
        assert_eq!(help_lines, 1);
    }
}
