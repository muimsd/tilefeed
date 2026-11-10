# Configuration Reference

tilefeed reads a TOML configuration file (default: `config.toml`). All fields can be overridden with environment variables using the `TILES_` prefix and `__` as section separator.

## Environment Variables

```bash
export TILES_DATABASE__HOST=db.example.com
export TILES_DATABASE__PORT=5432
export TILES_DATABASE__USER=myuser
export TILES_DATABASE__PASSWORD=secret
export TILES_DATABASE__DBNAME=geodata
```

You can also use a `.env` file in the project directory.

## Full Config Example

```toml
# Path to Tippecanoe binary (default: "tippecanoe", resolved via PATH)
# tippecanoe_bin = "/usr/local/bin/tippecanoe"

# Path to ogr2ogr binary (default: "ogr2ogr", resolved via PATH)
# ogr2ogr_bin = "/usr/local/bin/ogr2ogr"

[database]
host = "localhost"
port = 5432
user = "postgres"
password = "postgres"
dbname = "geodata"
pool_size = 4
sslmode = "prefer"  # disable | prefer | require | verify-ca | verify-full
# sslrootcert = "/etc/ssl/certs/db-ca.pem"  # required for verify-ca

[updates]
debounce_ms = 200
worker_concurrency = 8

[serve]
host = "0.0.0.0"
port = 3000
cors_origins = ["http://localhost:8080"]
max_age_secs = 300  # Cache-Control max-age for tiles; 0 sends no-cache
preview = true      # map of every source at / (default)

[serve.cache]
enabled = true
max_size_mb = 64
# ttl_secs = 3600

[metrics]
enabled = true
path = "/metrics"
# port = 9090  # dedicated metrics listener, required to scrape `watch` and `run`

[publish]
backend = "none" # none | local | s3 | mapbox | command
publish_on_generate = true
publish_on_update = true

# Source with multiple layers
[[sources]]
name = "basemap"
mbtiles_path = "./basemap.mbtiles"
min_zoom = 0
max_zoom = 14
generation_backend = "tippecanoe" # tippecanoe | gdal | native

[sources.tippecanoe]
drop_densest_as_needed = true
no_tile_size_limit = true

[[sources.layers]]
name = "buildings"
schema = "public"
table = "buildings"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type", "height"]
filter = "type != 'demolished'"
simplify_tolerance = 0.00001
generate_label_points = true
label_point_method = "point_on_surface"  # or "centroid"; label_point_column uses your own
generate_boundary_lines = true

# Exclude heavy properties at low zooms
[[sources.layers.property_rules]]
below_zoom = 8
exclude = ["description", "metadata"]

[[sources.layers.property_rules]]
below_zoom = 5
exclude = ["type", "height"]

[[sources.layers]]
name = "roads"
table = "roads"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "class"]
simplify_tolerance = 0.00001
min_zoom = 6   # only in tiles at zoom 6-14 (the source's max_zoom)
```

## Section Reference

### `[database]`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `host` | string | yes | PostgreSQL host |
| `port` | int | yes | PostgreSQL port |
| `user` | string | yes | Database user |
| `password` | string | yes | Database password |
| `dbname` | string | yes | Database name |
| `pool_size` | int | no | Connection pool size |
| `sslmode` | string | no | TLS mode, libpq semantics: `"disable"`, `"prefer"` (default), `"require"`, `"verify-ca"`, `"verify-full"` |
| `sslrootcert` | string | no | PEM file with the CA certificate(s) to verify the server against. Required for `verify-ca`; for `verify-full` defaults to the bundled Mozilla (webpki) roots |

`sslmode` applies to both the connection pool and the LISTEN/NOTIFY connection:

- `disable` — never use TLS.
- `prefer` — use TLS when the server offers it, otherwise fall back to plaintext. The certificate is not verified. This is the default, so configs written before `sslmode` existed keep working. Unlike libpq, a TLS handshake that *fails* (for example a server that only speaks TLS 1.0/1.1) is an error, not a plaintext retry; set `disable` for such servers.
- `require` — TLS is mandatory; the certificate is not verified (as in libpq, setting `sslrootcert` upgrades this to `verify-ca`).
- `verify-ca` — TLS is mandatory and the certificate chain must validate against `sslrootcert`; the hostname is not checked. `sslrootcert` is required: against the public roots, a chain-only check would accept any publicly trusted certificate.
- `verify-full` — the certificate chain must validate (against `sslrootcert`, or the bundled public roots when unset) and match `host`.

A `host` starting with `/` is a Unix-socket directory. PostgreSQL has no TLS over Unix sockets, so `sslmode` is ignored there, as in libpq.

Managed providers (Supabase, Neon, RDS with `rds.force_ssl`, Cloud SQL, Crunchy Bridge) need at least `require`; prefer `verify-full` with the provider's CA bundle in `sslrootcert`.

```toml
[database]
host = "db.example.com"
port = 5432
user = "tiles"
password = "secret"
dbname = "geodata"
sslmode = "verify-full"
sslrootcert = "/etc/ssl/certs/provider-ca.pem"
```

### `[updates]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `debounce_ms` | int | 200 | Debounce window for batching notifications |
| `worker_concurrency` | int | 8 | Max concurrent tile regeneration workers |
| `listen` | bool | true | `false`: don't LISTEN for notifications. `serve` serves the MBTiles as they are and something else (a scheduled `generate`) keeps them fresh. `watch` refuses to start. |

### `[serve]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | `"127.0.0.1"` | HTTP server bind address |
| `port` | int | 3000 | HTTP server port |
| `cors_origins` | string[] | `["*"]` | Allowed CORS origins. Omit or empty for wildcard. |
| `max_age_secs` | int | 300 | `Cache-Control: max-age` for tiles. `0` sends `no-cache` (clients revalidate with the ETag). |
| `preview` | bool | true | Serve a [map preview](serving.md#map-preview) of every source at `/`. `false` makes `/` return 404. |

### `[serve.cache]`

In-memory tile cache for `serve`. Tiles are cleared from it as incremental updates and full generations rewrite them. See [Tile cache](serving.md#tile-cache).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | true | Cache tiles in memory |
| `max_size_mb` | int | 64 | Size budget; least recently used tiles are evicted beyond it. `0` disables the cache. |
| `ttl_secs` | int | unset | Expire cached tiles after this many seconds even if unchanged (for MBTiles written by another process) |

### `[metrics]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | true | Expose Prometheus metrics |
| `path` | string | `"/metrics"` | Path the metrics are served under |
| `host` | string | `[serve]` host | Bind address for the dedicated metrics listener |
| `port` | int | — | Port for a dedicated metrics listener. Required to scrape `watch` and `run`, which have no tile server; when set for `serve`, metrics move off the tile port to this one. |

See [Metrics](metrics.md) for the full metric reference.

### `[publish]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `backend` | string | `"none"` | `none`, `local`, `s3`, `mapbox`, `command` |
| `destination` | string | — | File path (local) or S3 URI (s3) |
| `command` | string | — | Shell command (command backend) |
| `args` | string[] | — | Extra args for command backend |
| `mapbox_tileset_id` | string | — | `username.tileset` for Mapbox uploads |
| `mapbox_token` | string | — | Mapbox secret token with `uploads:write` |
| `publish_on_generate` | bool | true | Publish after full generation |
| `publish_on_update` | bool | true | Publish after incremental updates |

### `[[sources]]`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Source identifier |
| `mbtiles_path` | string | yes | Output MBTiles file path |
| `min_zoom` | int | yes | Minimum zoom level |
| `max_zoom` | int | yes | Maximum zoom level |
| `generation_backend` | string | no | `"tippecanoe"` (default), `"gdal"`, or `"native"` |

### `[[sources.layers]]`

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Layer name (must match trigger argument) |
| `schema` | string | no | Database schema (default: `"public"`) |
| `table` | string | yes | Source table name |
| `geometry_column` | string | no | Geometry column (default: `"geom"`) |
| `id_column` | string | no | Feature ID column |
| `srid` | int | no | SRID of geometries stored without one (SRID 0, as `shp2pgsql` without `-s` or an OGR_FDW source with no recognized projection leaves them). Geometries with an SRID are always transformed from their own, whatever this says. Pass the same value as the trigger's second argument (see `sql/setup_notify.sql`). `tilefeed validate` reports SRID-0 geometries on a layer without it |
| `properties` | string[] | no | Properties to include in tiles. Omit it to include every column except geometry and geography columns |
| `filter` | string | no | SQL WHERE clause to filter features |
| `simplify_tolerance` | float | no | Douglas-Peucker tolerance in degrees (scaled per zoom) |
| `generate_label_points` | bool | no | Generate `{name}_labels` point layer (see derived-layers.md) |
| `label_point_method` | string | no | `point_on_surface` (default) or `centroid` |
| `label_point_column` | string | no | Use this existing point column as the label point; overrides `label_point_method` |
| `generate_boundary_lines` | bool | no | Generate `{name}_boundary` polyline layer |
| `min_zoom` | int | no | Lowest zoom the layer appears at (default: the source's `min_zoom`) |
| `max_zoom` | int | no | Highest zoom the layer appears at (default: the source's `max_zoom`) |

#### Per-layer zoom range

`min_zoom` and `max_zoom` restrict a layer to part of its source's zoom range —
for example, building footprints only from zoom 13, or a country-outline layer only
up to zoom 6. Either may be set alone; the other defaults to the source's bound.
Derived layers (`{name}_labels`, `{name}_boundary`) follow their parent's range.

```toml
[[sources]]
name = "basemap"
mbtiles_path = "./basemap.mbtiles"
min_zoom = 0
max_zoom = 14

[[sources.layers]]
name = "countries"
table = "countries"
max_zoom = 6          # zoom 0-6

[[sources.layers]]
name = "buildings"
table = "buildings"
min_zoom = 13         # zoom 13-14
generate_label_points = true   # buildings_labels: also zoom 13-14
```

The range must lie inside the source's: `tilefeed` refuses to start when
`min_zoom > max_zoom` or when either is outside the source's `min_zoom..=max_zoom`,
naming the layer.

Like any change to a layer's config, changing its zoom range needs a full
`generate`. Incremental updates only rewrite tiles inside the layer's current
range, so tiles built under the old range keep their old content at zooms the
layer no longer covers.

All generation backends honour it:

| Backend | How |
|---------|-----|
| `tippecanoe` | Each exported feature carries Tippecanoe's `"tippecanoe": {"minzoom": …, "maxzoom": …}` object, with only the bounds the layer sets (so `min_zoom` alone doesn't cap the layer under `--extend-zooms-if-still-dropping`) |
| `gdal` | `ogr2ogr` is given the layer creation options `-lco MINZOOM=… -lco MAXZOOM=…` |
| `native` | The layer is not queried or encoded for tiles outside its range |

Incremental updates only re-encode tiles inside the changed layer's range, and a
re-encoded tile only contains the layers whose range covers its zoom. The TileJSON
endpoint reports each layer's range as `minzoom`/`maxzoom` in `vector_layers`.

### `[[sources.layers.property_rules]]`

| Field | Type | Description |
|-------|------|-------------|
| `below_zoom` | int | Zoom threshold |
| `exclude` | string[] | Properties to exclude below this zoom |

### `[webhook]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `urls` | string[] | `[]` | Webhook endpoint URLs to receive HTTP POST notifications |
| `secret` | string | — | HMAC-SHA256 signing secret. When set, requests include `X-Tilefeed-Signature: sha256=...` header |
| `cooldown_secs` | int | — | Trailing-edge throttle window in seconds. Events are accumulated per source and sent as one aggregated notification when the window expires. Also applies to SSE. |
| `timeout_ms` | int | 5000 | HTTP request timeout per webhook call |
| `retry_count` | int | 2 | Number of retries with exponential backoff on failure |
| `on_generate` | bool | true | Send webhook after full generation completes |
| `on_update` | bool | true | Send webhook after incremental tile updates |

Example:

```toml
[webhook]
urls = ["https://example.com/hooks/tilefeed"]
secret = "my-signing-secret"
cooldown_secs = 300  # aggregate events for 5 minutes
on_generate = true
on_update = true
```

The webhook payload is a JSON object with an `event` field (`"generate_complete"` or `"update_complete"`). The `update_complete` payload includes `max_zoom` so frontends can invalidate overzoomed tile views (tiles rendered beyond the source's max zoom level).

### `[sources.tippecanoe]`

See [Tippecanoe Settings](tippecanoe.md).

## Generation Backends

### Tippecanoe (default)

Requires the [Tippecanoe](https://github.com/felt/tippecanoe) binary. Exports PostGIS layers as GeoJSON, pipes through Tippecanoe, and produces optimized MBTiles. Best for production with large datasets.

### GDAL

Requires `ogr2ogr` from [GDAL](https://gdal.org/). Exports via OGR and converts to MBTiles. Useful when Tippecanoe isn't available.

### Native

No external dependencies. Uses tilefeed's built-in Rust MVT encoder to generate tiles directly from PostGIS queries. Supports geometry simplification and derived layers. Best for development or environments where installing external tools is impractical.

Generation only visits tiles that can hold data: it computes the data extent (`ST_Extent` of every layer, in EPSG:4326), queries every tile inside it at `min_zoom`, and then descends only into the children of tiles that had features. Tile queries run concurrently, up to `pool_size`. A layer whose `min_zoom` is deeper than the source's isn't encoded above it, so at those zooms a cheap `EXISTS` check decides whether a tile still needs descending into.

```toml
[[sources]]
name = "parks"
mbtiles_path = "./parks.mbtiles"
min_zoom = 0
max_zoom = 8
generation_backend = "native"
```
