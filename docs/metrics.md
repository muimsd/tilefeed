# Metrics

tilefeed exposes [Prometheus](https://prometheus.io/) metrics in the text exposition
format (version 0.0.4) covering tile serving, full generation, incremental updates,
webhook delivery, and artifact publishing.

Metrics are **enabled by default**.

## Where to scrape

| Command | Endpoint |
|---------|----------|
| `serve` | On the tile server itself, e.g. `http://127.0.0.1:3000/metrics` |
| `watch`, `run` | Only on the standalone exporter — set `[metrics] port` |
| `generate`, `inspect`, `validate`, `diff` | Not exposed (these commands exit immediately) |

`watch` and `run` have no tile server of their own, so they need `[metrics] port` set
to expose anything. `serve` always serves the metrics path on the tile server; setting
`[metrics] port` to something other than the `[serve] port` *additionally* starts a
metrics-only listener, which is the usual way to keep the scrape endpoint off a
public tile port.

## Configuration

```toml
[metrics]
enabled = true       # expose metrics at all (default: true)
path = "/metrics"    # path to serve them under (default: "/metrics")
host = "127.0.0.1"   # standalone exporter bind address (default: the [serve] host)
port = 9090          # standalone exporter port — required for `watch` and `run`
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | true | Expose metrics. Set `false` to turn the endpoint off entirely. |
| `path` | string | `"/metrics"` | Path the metrics are served under. A leading `/` is added if missing. |
| `host` | string | `[serve] host` | Bind address for the standalone exporter. |
| `port` | int | — | Port for a standalone metrics-only listener. Unset means no standalone exporter. |

Prometheus scrape config:

```yaml
scrape_configs:
  - job_name: tilefeed
    static_configs:
      - targets: ["localhost:9090"]
```

## Metrics reference

### Process

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_build_info` | gauge | `version` | Always 1; carries the running version as a label |
| `tilefeed_uptime_seconds` | gauge | — | Seconds since the process started |

### Tile serving

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_tile_requests_total` | counter | `source`, `result` | Tile requests. `result`: `hit`, `empty`, `not_modified`, `source_not_found`, `error` |
| `tilefeed_tile_bytes_total` | counter | `source` | Bytes of tile data served |
| `tilefeed_tile_read_duration_seconds` | histogram | `source` | Time spent reading a tile out of MBTiles |
| `tilefeed_tilejson_requests_total` | counter | `source`, `result` | TileJSON requests. `result`: `ok`, `source_not_found` |
| `tilefeed_sse_clients` | gauge | — | SSE clients currently connected to `/events` |
| `tilefeed_sse_connections_total` | counter | — | SSE connections opened since start |

`empty` is a 204: the tile is genuinely absent from the MBTiles (no features there),
which is normal and not an error.

### Full generation

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_generate_total` | counter | `source`, `result` | Generation runs. `result`: `success`, `failure` |
| `tilefeed_generate_duration_seconds` | histogram | `source` | Duration of a full generation |

### Incremental updates

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_notifications_total` | counter | `result` | LISTEN/NOTIFY payloads. `result`: `routed`, `unknown_layer`, `invalid_payload` |
| `tilefeed_update_batches_total` | counter | `source` | Debounced batches applied |
| `tilefeed_tiles_written_total` | counter | `source` | Tiles written into MBTiles |
| `tilefeed_tiles_deleted_total` | counter | `source` | Tiles deleted (regenerated to empty) |
| `tilefeed_tile_encode_errors_total` | counter | `source` | Tiles that failed to regenerate |
| `tilefeed_update_errors_total` | counter | `source` | Update batches that failed |
| `tilefeed_update_duration_seconds` | histogram | `source` | Duration of an update batch |
| `tilefeed_listener_reconnects_total` | counter | — | PostgreSQL LISTEN/NOTIFY reconnection attempts |

A rising `unknown_layer` count means a database trigger is passing a layer name that
no `[[sources.layers]]` declares — the notification is dropped and those tiles go
stale. `tilefeed_listener_reconnects_total` climbing means the database connection
keeps dropping, during which updates are missed entirely.

### Webhooks

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_webhook_requests_total` | counter | `result` | HTTP attempts. `result`: `success`, `http_error`, `transport_error` |
| `tilefeed_webhook_retries_total` | counter | — | Delivery retries |
| `tilefeed_webhook_failures_total` | counter | — | Deliveries abandoned after exhausting retries |
| `tilefeed_webhook_duration_seconds` | histogram | — | Duration of a single HTTP attempt |

### Publishing

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_publish_total` | counter | `backend`, `result` | Artifact publishes. `backend`: `local`, `s3`, `mapbox`, `command` |
| `tilefeed_publish_duration_seconds` | histogram | `backend` | Duration of a publish |

## Example queries

Tile request rate by source:

```promql
sum by (source) (rate(tilefeed_tile_requests_total[5m]))
```

Share of requests served from cache validation (304s):

```promql
sum(rate(tilefeed_tile_requests_total{result="not_modified"}[5m]))
  / sum(rate(tilefeed_tile_requests_total[5m]))
```

95th percentile tile read latency:

```promql
histogram_quantile(0.95,
  sum by (le, source) (rate(tilefeed_tile_read_duration_seconds_bucket[5m])))
```

Incremental update throughput, in tiles per second:

```promql
sum by (source) (rate(tilefeed_tiles_written_total[5m]))
```

Notifications that never reached a source — usually a trigger/config name mismatch:

```promql
rate(tilefeed_notifications_total{result="unknown_layer"}[15m]) > 0
```

Bytes served per second:

```promql
sum by (source) (rate(tilefeed_tile_bytes_total[5m]))
```

## Alerting starters

```yaml
groups:
  - name: tilefeed
    rules:
      - alert: TilefeedUpdateFailures
        expr: rate(tilefeed_update_errors_total[10m]) > 0
        for: 10m
        annotations:
          summary: "Incremental tile updates are failing for {{ $labels.source }}"

      - alert: TilefeedListenerFlapping
        expr: rate(tilefeed_listener_reconnects_total[15m]) > 0
        for: 15m
        annotations:
          summary: "PostgreSQL LISTEN/NOTIFY connection keeps dropping — updates are being missed"

      - alert: TilefeedWebhookDeliveryFailing
        expr: rate(tilefeed_webhook_failures_total[15m]) > 0
        for: 15m
        annotations:
          summary: "Webhook deliveries are exhausting their retries"
```

## Cardinality

Label values are bounded by the config: source names, publish backends, and a fixed
set of result strings. Nothing is labelled by tile coordinate, zoom, or client, so the
series count stays proportional to the number of configured sources.
