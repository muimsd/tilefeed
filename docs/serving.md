# Tile Serving

tilefeed includes a built-in HTTP tile server for development and production use.

## Built-in Server

Start the server with the `serve` command, which generates tiles, starts an HTTP server, and watches for incremental updates:

```bash
tilefeed serve
tilefeed -c myconfig.toml serve
```

### Serving without rebuilding

By default `serve` runs a full generation before it serves anything, so a restart
re-runs Tippecanoe over every source. Pass `--skip-generate` to serve the MBTiles
already on disk:

```bash
tilefeed serve --skip-generate
```

This changes three things:

- **Startup is immediate** — no generation, however large the sources are. (One
  exception: the first open of a file Tippecanoe just produced materializes its
  `tiles` view into a writable table, which copies every tile once. Subsequent
  starts skip that.)
- **No external tools are required.** Tippecanoe and GDAL are only checked for when
  a run will actually generate, so a serving-only container needs neither installed.
- **The database is not needed to start.** Tiles are served straight away; the
  LISTEN/NOTIFY watcher connects in the background and retries with backoff, so
  incremental updates resume by themselves once PostgreSQL is reachable.

The MBTiles file has to exist — `tilefeed generate` builds it. Starting with a
missing or non-MBTiles file fails immediately with a message naming the source,
rather than serving an empty database.

Two metrics describe what a process did at startup:

```
tilefeed_startup_info{command="serve",generated="false"} 1
tilefeed_mbtiles_tiles_at_open{source="basemap"} 41532
```

The tile count is a snapshot taken in the background just after startup, so it
never delays the first request — and it does not move as incremental updates write
new tiles.

`run --skip-generate` does the same for the watch-only pipeline; it is equivalent
to `watch`, and exists so a deployment already running `run` can switch by adding a
flag rather than changing its command.

### Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /{source}/{z}/{x}/{y}.pbf` | Serve a vector tile (MVT protobuf) |
| `GET /{source}.json` | TileJSON 3.0.0 metadata |
| `GET /events` | Server-Sent Events stream for live tile updates |
| `GET /health` | Health check (returns `ok`) |
| `GET /metrics` | [Prometheus metrics](metrics.md) (enabled by default, unless `[metrics] port` moves them to their own listener) |

### Features

- **ETags** — SHA-256 based content hashing with `If-None-Match` / 304 Not Modified support
- **CORS** — Configurable origins or wildcard (default)
- **Tile cache** — tiles are kept in memory with their ETag, so repeat requests skip SQLite entirely. Tiles are cleared from the cache as soon as an update rewrites them (see [Tile cache](#tile-cache))
- **Cache-Control** — `public, max-age=300` on tile responses by default; set `max_age_secs` (`0` sends `no-cache`)
- **TileJSON 3.0.0** — Auto-generated from source config, includes derived layers (`_labels`, `_boundary`)
- **Content-Encoding** — `gzip` is advertised only when the stored tile bytes are actually gzipped
- **[Prometheus metrics](metrics.md)** — Request counts, bytes served, and read latency per source

### Configuration

```toml
[serve]
host = "0.0.0.0"     # bind address (default: 127.0.0.1)
port = 3000           # port (default: 3000)
cors_origins = ["http://localhost:8080"]  # omit for wildcard
max_age_secs = 300    # Cache-Control max-age for tiles; 0 sends no-cache (default: 300)

[serve.cache]
enabled = true        # in-memory tile cache (default: true)
max_size_mb = 64      # size budget; least recently used tiles are evicted (default: 64)
# ttl_secs = 3600     # expire entries even if unchanged (default: unset)

[metrics]
enabled = true        # expose /metrics (default: true)
path = "/metrics"     # metrics path (default: /metrics)
# port = 9090         # serve metrics on their own listener instead of the tile port
```

### Tile cache

`serve` keeps recently requested tiles in memory, including empty (204) tiles, together with their ETag. A cache hit costs neither a SQLite read nor a hash.

Old tiles are cleared from the cache as the data changes:

- **Incremental updates.** After the updater commits a batch to MBTiles, it clears exactly the tiles it rewrote or deleted, so the next request reads the new tile. A request that read the store just before the commit is not allowed to put the old tile back in the cache.
- **Writes by other processes.** If something else commits to a source's MBTiles file (a cron `generate`, a separate `watch`, a manual edit), the server notices within about 2 seconds and clears that source's cached tiles. It checks SQLite's `data_version`, which changes only when another connection commits. This does not cover a file that is *replaced* (renamed over) rather than written to: the server keeps reading the file it opened, with or without the cache, so restart `serve` after swapping files.
- **TTL.** `ttl_secs` expires entries even when nothing changed, as an extra safety net.

Only tiles at zooms the source has (`min_zoom`..`max_zoom`) are cached, so requests outside that range can't push real tiles out. Coordinates that don't exist at their zoom (`x` or `y` ≥ 2^z) get a 404.

The cache only clears tiles held by this server. Browsers and CDNs keep their own copies for `max_age_secs`. For live-updating maps, set `max_age_secs = 0` (`Cache-Control: no-cache`): clients revalidate each tile with its ETag, unchanged tiles cost a 304 served from memory, and changed tiles show up immediately.

Metrics: `tilefeed_tile_cache_requests_total{result="hit|miss|expired"}`, `tilefeed_tile_cache_bytes`, `tilefeed_tile_cache_evictions_total`, `tilefeed_tile_cache_invalidations_total`.

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
  "layers_affected": ["buildings", "roads"],
  "areas": [
    { "bbox": [2.3412, 48.8530, 2.3431, 48.8544], "min_zoom": 13, "max_zoom": 14 },
    { "bbox": [2.2950, 48.8580, 2.2990, 48.8600], "min_zoom": 0, "max_zoom": 14 }
  ]
}
```

`areas` lists where the change happened: the bounding box (`[west, south, east, north]`, EPSG:4326) of each changed feature's new and old geometry, with the zoom range its layer is encoded at. An event carries at most 64 areas; beyond that they are merged into larger boxes, which can only widen the region, never narrow it. An empty `areas` means the extent is unknown and should be treated as "anywhere".

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

### Only the changes in view

By default every client receives every event. A client can instead subscribe with what it is showing, and the server then sends only the updates that change tiles in that view:

```
GET /events?bbox=<west>,<south>,<east>,<north>&zoom=<zoom>&source=<name>[,<name>]
```

| Parameter | Meaning |
|---|---|
| `bbox` | The visible area in degrees. Longitudes may be unwrapped past ±180 (as `getBounds()` returns them after panning across the antimeridian), or `west > east` for a view that crosses it. |
| `zoom` | The map's zoom, fractional is fine. Updates to layers not drawn at that zoom are skipped (a `min_zoom = 13` buildings layer doesn't wake a client at zoom 8). Above the source's `max_zoom` the client is overzooming, so changes at `max_zoom` count. |
| `source` | Only these sources. Unknown names are rejected. |

Every parameter is optional and they combine. With `zoom`, the view is widened to the edges of the tiles that cover it, because the map draws whole tiles. Changes at `floor(zoom)` and one level deeper count, since 256px sources and zoom transitions show tiles a level below the integer zoom. `generate_complete` and updates without `areas` always pass the viewport check, since they may have changed anything. Invalid values return `400` with the reason.

The view is fixed for the life of the connection, so re-subscribe when the map moves. Pass the id of the last event you received as `since`, and the server first replays the updates you missed while reconnecting that touch the new view. The same code works in MapLibre GL JS and Mapbox GL JS:

```javascript
let es, lastId = null;
function subscribe() {
    if (es) es.close();
    const b = map.getBounds();
    const bbox = [b.getWest(), b.getSouth(), b.getEast(), b.getNorth()].join(',');
    const since = lastId ? `&since=${lastId}` : '';
    es = new EventSource(`${server}/events?bbox=${bbox}&zoom=${map.getZoom()}&source=basemap${since}`);
    es.addEventListener('ready', (e) => { lastId = e.lastEventId; });
    es.addEventListener('resync', () => { lastId = null; refresh('basemap'); });
    for (const type of ['update_complete', 'generate_complete']) {
        es.addEventListener(type, (e) => { lastId = e.lastEventId; refresh(JSON.parse(e.data).source); });
    }
}
let timer;
map.on('moveend', () => { clearTimeout(timer); timer = setTimeout(subscribe, 300); });
map.on('load', subscribe);
```

### Event ids, replay and `resync`

Every event carries an SSE `id`, numbered in order. The server keeps the last 256 events. A connection with `since=<id>`, or the `Last-Event-ID` header that `EventSource` sends when it reconnects on its own, first receives the retained events after that id, filtered by its viewport. Two extra event types frame this:

- **`ready`**: sent once the replay is done, with `id` set to the newest event so far (`{"last_event_id": 42}`). Record its `lastEventId`, so a later reconnect knows where to resume even if no update has matched your view yet.
- **`resync`**: some events can't be replayed, because they have aged out of the history, the server restarted, or the client fell too far behind. Reload the tiles of every source you show.

Viewport filtering happens per connection, before the cooldown below, so an aggregated event only accumulates changes that client can see.

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
