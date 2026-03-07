# postile

A fast, self-contained PostGIS vector tile server with **live incremental updates**.

postile serves [MVT](https://github.com/mapbox/vector-tile-spec) tiles from an MBTiles cache, and automatically regenerates affected tiles when data changes in PostgreSQL — no manual rebuilds needed.

## Features

- **Incremental tile updates** — PostgreSQL LISTEN/NOTIFY triggers detect INSERT/UPDATE/DELETE and regenerate only affected tiles
- **In-memory LRU cache** — configurable tile cache with automatic invalidation on updates
- **ETag support** — conditional `304 Not Modified` responses save bandwidth
- **TileJSON 3.0** — auto-discovery endpoint for MapLibre GL JS / Mapbox GL JS clients
- **Connection pooling** — deadpool-postgres for concurrent PostGIS queries
- **Debounced batch processing** — rapid-fire database changes are batched within a configurable window before regeneration
- **Concurrent tile regeneration** — affected tiles are rebuilt in parallel
- **Graceful shutdown** — handles SIGTERM and Ctrl+C cleanly
- **Native MVT encoding** — built-in protobuf encoder, no runtime dependencies beyond PostgreSQL

## Requirements

- Rust 1.70+
- PostgreSQL with [PostGIS](https://postgis.net/) extension
- [Tippecanoe](https://github.com/felt/tippecanoe) (only for initial full tile generation)

## Quick Start

### 1. Set up the database

```bash
createdb geodata
psql -d geodata -c "CREATE EXTENSION IF NOT EXISTS postgis"
```

### 2. Install the notification trigger

```bash
psql -d geodata < sql/setup_notify.sql
```

This creates the `notify_tile_update()` trigger function. Attach it to each table you want to serve:

```sql
CREATE TRIGGER tile_update_trigger
    AFTER INSERT OR UPDATE OR DELETE ON your_table
    FOR EACH ROW
    EXECUTE FUNCTION notify_tile_update('your_layer_name');
```

### 3. Configure

Edit `config.toml`:

```toml
[database]
host = "localhost"
port = 5432
user = "postgres"
password = "postgres"
dbname = "geodata"
pool_size = 4

[server]
host = "0.0.0.0"
port = 3000

[tiles]
mbtiles_path = "./tiles.mbtiles"
min_zoom = 0
max_zoom = 14

[cache]
max_tiles = 10000
debounce_ms = 200

[[tiles.layers]]
name = "buildings"
table = "buildings"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type", "height"]
```

The layer `name` must match the argument passed to `notify_tile_update()` in your trigger.

### 4. Generate and serve

```bash
# Generate tiles from PostGIS via Tippecanoe, then start serving
cargo run --release -- run

# Or just serve an existing MBTiles file
cargo run --release -- serve

# Or generate without serving
cargo run --release -- generate
```

Configuration can also be set via environment variables with the `TILES_` prefix (e.g., `TILES_DATABASE__HOST=localhost`).

## API Endpoints

| Endpoint | Description |
|---|---|
| `GET /tiles/{z}/{x}/{y}.pbf` | Fetch a vector tile |
| `GET /tiles.json` | TileJSON 3.0 metadata |
| `GET /metadata` | Layer configuration |
| `GET /health` | Health check |
| `GET /stats` | Cache hit/miss statistics |

### Using with MapLibre GL JS

```javascript
const map = new maplibregl.Map({
  container: 'map',
  style: {
    version: 8,
    sources: {
      postile: {
        type: 'vector',
        url: 'http://localhost:3000/tiles.json'
      }
    },
    layers: [{
      id: 'buildings',
      type: 'fill',
      source: 'postile',
      'source-layer': 'buildings',
      paint: { 'fill-color': '#888', 'fill-opacity': 0.5 }
    }]
  }
});
```

## How Incremental Updates Work

1. A row is inserted/updated/deleted in PostgreSQL
2. The trigger fires `pg_notify('tile_update', ...)` with the layer name, feature ID, and old bounds (for moves/deletes)
3. postile receives the notification, waits for the debounce window to collect more changes
4. Affected tile coordinates are computed from feature bounding boxes across all zoom levels
5. Tiles are regenerated concurrently from PostGIS and written to MBTiles
6. The LRU cache is invalidated then repopulated with fresh tiles

## License

MIT
