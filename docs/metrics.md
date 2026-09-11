# Metrics

tilefeed exposes [Prometheus](https://prometheus.io/) metrics in the text exposition
format (version 0.0.4) covering tile serving, full generation, incremental updates,
webhook delivery, and artifact publishing.

Metrics are **enabled by default**.

## Where to scrape

Metrics are served in exactly one place: a dedicated listener when `[metrics] port`
configures one, the tile server otherwise.

| Command | `[metrics] port` unset | `[metrics] port` set |
|---------|------------------------|----------------------|
| `serve` | On the tile server, e.g. `http://127.0.0.1:3000/metrics` | Only on that port — **not** on the tile port |
| `watch`, `run` | Not exposed (no HTTP server runs) | On that port |
| `generate`, `inspect`, `validate`, `diff` | Not exposed (these commands exit immediately) | |

So setting a separate port genuinely takes the scrape endpoint off a public tile
port, rather than adding a second copy of it. If `[metrics] port` names the same
address the tile server already binds, the tile server serves it and no second
listener is started.

`watch` and `run` have no tile server of their own, so they need `[metrics] port`
set to expose anything.

## Configuration

```toml
[metrics]
enabled = true       # expose metrics at all (default: true)
path = "/metrics"    # path to serve them under (default: "/metrics")
host = "127.0.0.1"   # dedicated listener bind address (default: the [serve] host)
port = 9090          # dedicated listener port — required for `watch` and `run`
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | true | Expose metrics. Set `false` to turn the endpoint off entirely. |
| `path` | string | `"/metrics"` | Path the metrics are served under. A leading `/` is added if missing. |
| `host` | string | `[serve] host` | Bind address for the dedicated metrics listener. |
| `port` | int | — | Port for a dedicated metrics listener. Unset means metrics are served on the tile server instead. |

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
| `tilefeed_startup_info` | gauge | `command`, `generated` | Always 1; which command is running and whether it generated tiles at startup |
| `tilefeed_mbtiles_tiles` | gauge | `source` | Tiles present in the source's MBTiles when it was opened |

### Tile serving

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tilefeed_tile_requests_total` | counter | `source`, `result` | Tile requests. `result`: `hit`, `empty`, `not_modified`, `not_found`, `error` |
| `tilefeed_tile_bytes_total` | counter | `source` | Bytes of tile data served |
| `tilefeed_tile_read_duration_seconds` | histogram | `source` | Time spent reading a tile out of MBTiles |
| `tilefeed_tilejson_requests_total` | counter | `source`, `result` | TileJSON requests. `result`: `ok`, `not_found` |
| `tilefeed_sse_clients` | gauge | — | SSE clients currently connected to `/events` |
| `tilefeed_sse_connections_total` | counter | — | SSE connections opened since start |

`empty` is a 204: the tile is genuinely absent from the MBTiles (no features there),
which is normal and not an error.

Requests for a source that isn't configured are counted under the fixed label
`source="__unknown__"` rather than the name from the URL — otherwise a scanner
walking `/aaa/0/0/0.pbf`, `/aab/0/0/0.pbf`, … could grow the registry without bound.

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

## Notes

**Cardinality.** Label values are bounded by the config: source names, publish
backends, and a fixed set of result strings. Nothing is labelled by tile coordinate,
zoom, or client, and names taken from a URL are never used as labels, so the series
count stays proportional to the number of configured sources.

**Series that exist before anything happens.** Counters with known label sets —
reconnects, webhook results, notification routing results — are exported at `0` from
startup. A counter that first appears with the value 1 gives `rate()` nothing to
compare against, which would keep the alerts above silent on the very first failure
they were written to catch.

**Exposure.** The metrics route is deliberately left out of the tile server's CORS
layer, so a page in the operator's browser can't read it cross-origin even though the
tile API allows any origin by default. On a publicly bound tile server, prefer a
separate `[metrics] port` on an internal interface — or `enabled = false`.

**Reserved paths.** `[metrics] path` may not be `/health` or `/events`; the server
reports a config error at startup rather than letting the route collide.
