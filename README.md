# postile

A PostGIS vector tile pipeline for **MBTiles generation + incremental updates + storage publish**.

`postile` no longer serves HTTP tiles directly. It builds and maintains an MBTiles file, then pushes that artifact to your target storage (local path, S3, or a custom command backend).

## Features

- Full MBTiles generation from PostGIS via Tippecanoe
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

The trigger layer name must match `[[tiles.layers]].name` in config.

### 2. Configure

```toml
[database]
host = "localhost"
port = 5432
user = "postgres"
password = "postgres"
dbname = "geodata"
pool_size = 4

[tiles]
mbtiles_path = "./tiles.mbtiles"
min_zoom = 0
max_zoom = 14

[updates]
debounce_ms = 200
worker_concurrency = 8

[publish]
backend = "none" # none | local | s3 | command
publish_on_generate = true
publish_on_update = true

[[tiles.layers]]
name = "buildings"
table = "buildings"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type", "height"]
```

Backend-specific publish fields:

- `local`: set `publish.destination` to a file path (or an existing directory).
- `s3`: set `publish.destination = "s3://bucket/path/tiles.mbtiles"`.
- `command`: set `publish.command`, and use env vars:
  - `POSTILE_MBTILES_PATH`
  - `POSTILE_PUBLISH_REASON`

### 3. Run commands

```bash
# Full rebuild only
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
3. Affected tiles are derived from new/old feature bounds
4. Tiles are regenerated and written into MBTiles
5. MBTiles artifact is published if `publish_on_update = true`

## License

MIT
