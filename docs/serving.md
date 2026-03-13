# Tile Serving

tilefeed includes a built-in HTTP tile server for development and production use.

## Built-in Server

Start the server with the `serve` command, which generates tiles, starts an HTTP server, and watches for incremental updates:

```bash
tilefeed serve
tilefeed -c myconfig.toml serve
```

### Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /{source}/{z}/{x}/{y}.pbf` | Serve a vector tile (MVT protobuf) |
| `GET /{source}.json` | TileJSON 3.0.0 metadata |
| `GET /events` | Server-Sent Events stream for live tile updates |
| `GET /health` | Health check (returns `ok`) |

### Features

- **ETags** — SHA-256 based content hashing with `If-None-Match` / 304 Not Modified support
- **CORS** — Configurable origins or wildcard (default)
- **Cache-Control** — `public, max-age=300` on tile responses
- **TileJSON 3.0.0** — Auto-generated from source config, includes derived layers (`_labels`, `_boundary`)

### Configuration

```toml
[serve]
host = "0.0.0.0"     # bind address (default: 127.0.0.1)
port = 3000           # port (default: 3000)
cors_origins = ["http://localhost:8080"]  # omit for wildcard
```

## Server-Sent Events (SSE)

The `GET /events` endpoint provides a live stream of tile update events using [Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events). Frontends can subscribe to this stream to refresh tiles automatically when data changes.

### Event Types

**`update_complete`** — Sent after incremental tile updates:

```json
{
  "event": "update_complete",
  "source": "basemap",
  "tiles_updated": 42,
  "affected_zooms": [10, 11, 12, 13, 14],
  "max_zoom": 14,
  "layers_affected": ["buildings", "roads"]
}
```

**`generate_complete`** — Sent after a full tile generation:

```json
{
  "event": "generate_complete",
  "source": "basemap",
  "duration_ms": 5000
}
```

The `max_zoom` field helps frontends handle overzooming — tiles at zoom levels beyond `max_zoom` are rendered by upscaling tiles from `max_zoom`, so those views should also be invalidated.

### MapLibre Example

```javascript
const es = new EventSource('http://localhost:3000/events');
es.addEventListener('update_complete', (e) => {
    const data = JSON.parse(e.data);
    const source = map.getSource(data.source);
    if (source) {
        source.setTiles([`http://localhost:3000/${data.source}/{z}/{x}/{y}.pbf?_t=${Date.now()}`]);
    }
});
```

A full MapLibre integration example is available at `examples/webhook-sse/map.html`.

### Cooldown / Throttle

If `cooldown_secs` is set in `[webhook]` config, SSE events are also throttled — events are accumulated per source during the cooldown window and sent as one aggregated notification when the window expires. This prevents flooding frontends during rapid database changes.

## Webhooks

tilefeed can send HTTP POST notifications to external URLs when tiles are updated. Configure under `[webhook]` in your config file (see [Configuration Reference](configuration.md#webhook)).

### Payload

The request body is the same JSON as SSE events (see above). The `Content-Type` header is `application/json`.

### HMAC Signing

When `secret` is configured, each request includes an `X-Tilefeed-Signature` header:

```
X-Tilefeed-Signature: sha256=<hex-encoded HMAC-SHA256 of request body>
```

Verify this server-side to authenticate that the webhook came from tilefeed.

### Cooldown

Set `cooldown_secs` to aggregate rapid-fire events. For example, `cooldown_secs = 300` batches all events per source over 5 minutes into a single webhook call with accumulated tile counts and zoom levels.

## External Tile Servers

You can also serve MBTiles files produced by tilefeed with external tools:

- **CDN (CloudFront, Cloudflare R2)** — upload via S3 or command backend and serve through CDN
- **Martin** — point [Martin](https://github.com/maplibre/martin) at the MBTiles file for hot-reload
- **tileserver-gl** — use [tileserver-gl](https://github.com/maptiler/tileserver-gl) for raster + vector serving
- **nginx** — use an nginx module or lightweight proxy to read tiles from SQLite directly
