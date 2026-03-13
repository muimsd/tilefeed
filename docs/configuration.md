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

[updates]
debounce_ms = 200
worker_concurrency = 8

[serve]
host = "0.0.0.0"
port = 3000
cors_origins = ["http://localhost:8080"]

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

### `[updates]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `debounce_ms` | int | 200 | Debounce window for batching notifications |
| `worker_concurrency` | int | 8 | Max concurrent tile regeneration workers |

### `[serve]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | `"127.0.0.1"` | HTTP server bind address |
| `port` | int | 3000 | HTTP server port |
| `cors_origins` | string[] | `["*"]` | Allowed CORS origins. Omit or empty for wildcard. |

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
| `srid` | int | no | SRID (default: 4326) |
| `properties` | string[] | no | Properties to include in tiles |
| `filter` | string | no | SQL WHERE clause to filter features |
| `simplify_tolerance` | float | no | Douglas-Peucker tolerance in degrees (scaled per zoom) |
| `generate_label_points` | bool | no | Generate `{name}_labels` centroid layer |
| `generate_boundary_lines` | bool | no | Generate `{name}_boundary` polyline layer |

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

```toml
[[sources]]
name = "parks"
mbtiles_path = "./parks.mbtiles"
min_zoom = 0
max_zoom = 8
generation_backend = "native"
```
