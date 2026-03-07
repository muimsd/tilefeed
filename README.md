# postile

A PostGIS vector tile pipeline for **MBTiles generation + incremental updates + storage publish**.

## Features

- Full MBTiles generation from PostGIS via Tippecanoe
- Multiple sources: separate MBTiles outputs with independent layers and zoom ranges
- Incremental tile regeneration using PostgreSQL LISTEN/NOTIFY
- Debounced update batching and concurrent tile rebuild workers
- Optional publish after full generation and/or incremental updates
- Storage backends:
  - Local file copy
  - S3 upload via `aws s3 cp`
  - Custom command runner (for any storage workflow)

## Requirements

- Rust 1.70+
- PostgreSQL with [PostGIS](https://postgis.net/)
- [Tippecanoe](https://github.com/felt/tippecanoe) for full generation
- `aws` CLI only if using `publish.backend = "s3"`

## Quick Start

### 1. Set up database + trigger function

```bash
createdb geodata
psql -d geodata -c "CREATE EXTENSION IF NOT EXISTS postgis"
psql -d geodata < sql/setup_notify.sql
```

Attach the trigger to each source table:

```sql
CREATE TRIGGER tile_update_trigger
    AFTER INSERT OR UPDATE OR DELETE ON your_table
    FOR EACH ROW
    EXECUTE FUNCTION notify_tile_update('your_layer_name');
```

The trigger layer name must match a `[[sources.layers]].name` in config.

### 2. Configure

Each `[[sources]]` block defines an independent MBTiles output with its own layers and zoom range. Notifications are automatically routed to the correct source based on layer name.

```toml
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

[publish]
backend = "none" # none | local | s3 | command
publish_on_generate = true
publish_on_update = true

# Source 1: basemap with multiple layers
[[sources]]
name = "basemap"
mbtiles_path = "./basemap.mbtiles"
min_zoom = 0
max_zoom = 14

[[sources.layers]]
name = "buildings"
table = "buildings"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type", "height"]

[[sources.layers]]
name = "roads"
table = "roads"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "class"]

# Source 2: points of interest at higher zoom
[[sources]]
name = "poi"
mbtiles_path = "./poi.mbtiles"
min_zoom = 10
max_zoom = 16

[[sources.layers]]
name = "pois"
table = "points_of_interest"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "category"]
```

Backend-specific publish fields:

- `local`: set `publish.destination` to a file path.
- `s3`: set `publish.destination = "s3://bucket/path/tiles.mbtiles"`.
- `command`: set `publish.command`, and use env vars:
  - `POSTILE_MBTILES_PATH`
  - `POSTILE_PUBLISH_REASON`

### 3. Run commands

```bash
# Full rebuild all sources
cargo run --release -- generate

# Incremental watcher only (requires existing MBTiles)
cargo run --release -- watch

# Full rebuild, then keep watching updates
cargo run --release -- run
```

Configuration can also be set via environment variables with the `TILES_` prefix.

## Incremental Flow

1. PostgreSQL trigger emits `pg_notify('tile_update', ...)`
2. `postile` debounces notifications into a batch
3. Events are routed to the correct source based on layer name
4. Affected tiles are derived from new/old feature bounds
5. Tiles are regenerated and written into the source's MBTiles
6. MBTiles artifact is published if `publish_on_update = true`

## License

MIT
