# tilefeed

![CI](https://github.com/muimsd/tilefeed/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)

Dynamic vector tiles from PostGIS and GDAL files: **MBTiles generation + incremental updates + HTTP serving + storage publish**.

## Architecture

```
PostGIS ───┬──┬── Tippecanoe ──┐
OGR_FDW ───┤  ├── GDAL ────────┤── MBTiles ─────────────┬── Built-in HTTP server
           │  └── Native Rust ─┘                        ├── Local copy
           │                                            ├── S3 upload
           └── LISTEN/NOTIFY ── Debounce ── MVT encode ─├── Mapbox Studio
                                                        ├── Custom command
                                                        ├── Webhooks (HTTP POST)
                                                        ├── SSE (Server-Sent Events)
                                                        └── PMTiles export (static hosting)

Every stage ──────────────────────────────────────────────── Prometheus /metrics
```

Full generation exports PostGIS layers through one of three backends (Tippecanoe, GDAL, or native Rust) to produce MBTiles. Incremental updates listen for PostgreSQL notifications, debounce and deduplicate affected tiles, re-encode them as MVT protobuf, and write them into the existing MBTiles file. The built-in HTTP server serves tiles directly from MBTiles with ETag caching and TileJSON metadata.

## Why tilefeed?

| Tool | Approach | How tilefeed differs |
|------|----------|---------------------|
| **pg_tileserv** | Serves tiles on-the-fly from PostGIS, no caching or pre-generation. | tilefeed pre-generates tiles into MBTiles for predictable latency and CDN-friendly serving. |
| **Martin** | Full-featured tile server with many backends, but more complex to deploy. | tilefeed is a focused pipeline that generates, serves, and incrementally updates MBTiles. |
| **t-rex** | Similar pre-generation approach but tightly coupled to its own built-in HTTP server. | tilefeed decouples generation from serving — use the built-in server or bring your own. |
| **Tippecanoe cron** | Periodic full re-runs via cron or CI. Misses real-time updates and wastes work. | tilefeed adds incremental updates via PostgreSQL LISTEN/NOTIFY, regenerating only affected tiles. |

## Features

- Cross-platform: Linux, macOS, and Windows
- Three generation backends: [Tippecanoe](docs/tippecanoe.md), GDAL (ogr2ogr), and native Rust MVT encoder
- Multiple sources: separate MBTiles outputs with independent layers and zoom ranges
- [Per-layer zoom ranges](docs/configuration.md#per-layer-zoom-range) (`min_zoom`/`max_zoom`) within a source, honoured by every backend and by incremental updates
- Incremental tile regeneration using PostgreSQL LISTEN/NOTIFY with debounced batching
- [TLS to PostgreSQL](docs/configuration.md#database) (`sslmode`, `sslrootcert`) for managed databases such as Supabase, Neon, RDS and Cloud SQL
- Built-in HTTP tile server with ETag caching, CORS, configurable `Cache-Control`, and [TileJSON 3.0.0](docs/serving.md)
- [Map preview](docs/serving.md#map-preview) at `/`: every source on a MapLibre map, framed on the data, with feature popups and live updates as rows change
- [In-memory tile cache](docs/serving.md#tile-cache) for `serve`, cleared tile by tile as updates land, and per source when another process writes the MBTiles
- [Derived geometry layers](docs/derived-layers.md): auto-generated label points (point on surface, centroid, or your own point column) and boundary lines from polygons
- Native generator that only visits tiles inside the data's extent, descending the quadtree below non-empty tiles
- Geometry simplification (Douglas-Peucker) with per-zoom scaling
- Per-zoom property filtering to reduce tile sizes at low zooms
- Storage backends: local copy, S3, [Mapbox Studio](https://docs.mapbox.com/api/maps/uploads/), custom command
- [Webhook notifications](docs/serving.md#webhooks) with HMAC-SHA256 signing and configurable cooldown
- [Server-Sent Events](docs/serving.md#server-sent-events-sse) (`GET /events`) for live tile refresh in frontends, optionally [filtered to the map's viewport](docs/serving.md#only-the-changes-in-view) so a client only hears about changes it can see
- [Prometheus metrics](docs/metrics.md) (`GET /metrics`) for tile serving, generation, updates, webhooks, and publishing
- [PMTiles export](docs/pmtiles.md) for static hosting on S3/R2/any CDN — no tile server needed
- CLI tools: `inspect`, `validate`, `diff` for MBTiles diagnostics
- Docker support with multi-stage build
- `--skip-generate` for fast restarts: serve an existing MBTiles without rebuilding, PostGIS, or Tippecanoe
- Auto-reconnect on PostgreSQL connection loss with exponential backoff
- WAL mode for concurrent MBTiles reads during writes

## Installation

### Prerequisites

- PostgreSQL with [PostGIS](https://postgis.net/) extension
- [Tippecanoe](https://github.com/felt/tippecanoe) — only if using `generation_backend = "tippecanoe"` (default)
- [GDAL](https://gdal.org/) — only if using `generation_backend = "gdal"`
- Neither needed for `generation_backend = "native"`

### Homebrew (macOS / Linux)

```bash
brew tap muimsd/tilefeed
brew install tilefeed
```

### Scoop (Windows)

```powershell
scoop bucket add tilefeed https://github.com/muimsd/tilefeed
scoop install tilefeed
```

### Debian / Ubuntu (.deb)

```bash
curl -LO https://github.com/muimsd/tilefeed/releases/latest/download/tilefeed_amd64.deb
sudo dpkg -i tilefeed_amd64.deb
```

### Fedora / RHEL (.rpm)

```bash
curl -LO https://github.com/muimsd/tilefeed/releases/latest/download/tilefeed-x86_64.rpm
sudo rpm -i tilefeed-x86_64.rpm
```

### Cargo

```bash
cargo install tilefeed
```

### Docker

```bash
docker build -t tilefeed .
docker run -v ./config.toml:/data/config.toml tilefeed serve
```

### From source

Requires [Rust 1.70+](https://rustup.rs/) and `protoc` (protobuf compiler).

```bash
# Install protoc
# Linux: sudo apt-get install -y protobuf-compiler
# macOS: brew install protobuf
# Windows: choco install protoc

git clone https://github.com/muimsd/tilefeed.git
cd tilefeed
cargo build --release
# Binary: target/release/tilefeed
```

### Pre-built binaries

Download from [GitHub Releases](https://github.com/muimsd/tilefeed/releases):

| Platform | Binary |
|----------|--------|
| Linux x86_64 | `tilefeed-x86_64-unknown-linux-gnu` |
| Linux ARM64 | `tilefeed-aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `tilefeed-aarch64-apple-darwin` |
| macOS Intel | `tilefeed-x86_64-apple-darwin` |
| Windows x86_64 | `tilefeed-x86_64-pc-windows-msvc` |

## Usage

### Commands

```bash
tilefeed generate            # full tile generation from PostGIS
tilefeed watch               # watch LISTEN/NOTIFY for incremental updates
tilefeed run                 # generate then watch
tilefeed serve               # generate, start HTTP server, and watch
tilefeed serve --skip-generate  # serve the existing MBTiles immediately (no rebuild)
tilefeed inspect <file>      # inspect MBTiles metadata and statistics
tilefeed export <in> <out>   # export MBTiles to a PMTiles archive
tilefeed validate            # validate config against the database
tilefeed diff <a> <b>        # compare two MBTiles files
tilefeed -c other.toml serve # use alternate config file
```

If running from source: `cargo run --release -- serve`

## Quick Start

### 1. Set up database + triggers

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

```toml
[database]
host = "localhost"
port = 5432
user = "postgres"
password = "postgres"
dbname = "geodata"
# sslmode = "require"   # managed databases; verify-full also checks the certificate

[serve]
host = "0.0.0.0"
port = 3000
# max_age_secs = 0      # browsers revalidate every tile (ETag), so edits show at once

# [serve.cache]
# max_size_mb = 64      # in-memory tile cache, on by default

# [metrics]
# port = 9090   # dedicated metrics listener; omit to serve /metrics on the tile port

# [webhook]
# urls = ["https://example.com/hooks/tilefeed"]
# secret = "my-signing-secret"
# cooldown_secs = 300

[[sources]]
name = "basemap"
mbtiles_path = "./basemap.mbtiles"
min_zoom = 0
max_zoom = 14
# generation_backend = "native"  # no external tools needed

[[sources.layers]]
name = "buildings"
table = "buildings"
geometry_column = "geom"
id_column = "id"
srid = 4326
properties = ["name", "type", "height"]
simplify_tolerance = 0.00001
generate_label_points = true

[[sources.layers.property_rules]]
below_zoom = 8
exclude = ["height"]
```

See [full configuration reference](docs/configuration.md) for all options.

### 3. Run

```bash
# Full rebuild + serve + watch, then open http://localhost:3000/ for the map preview
tilefeed serve

# Or step by step:
tilefeed generate   # build MBTiles
tilefeed watch      # incremental updates only

# Restart without rebuilding — serves the existing MBTiles right away,
# needs neither Tippecanoe nor a reachable database to start
tilefeed serve --skip-generate
```

## Documentation

| Doc | Description |
|-----|-------------|
| [Configuration Reference](docs/configuration.md) | All config fields, sections, and generation backends |
| [Tippecanoe Settings](docs/tippecanoe.md) | Fine-tuning Tippecanoe tile generation |
| [Tile Serving](docs/serving.md) | Built-in HTTP server, SSE, webhooks, and external alternatives |
| [Metrics](docs/metrics.md) | Prometheus metrics, scrape setup, and example queries |
| [PMTiles Export](docs/pmtiles.md) | Single-file archives for static hosting |
| [Derived Layers](docs/derived-layers.md) | Auto-generated label points and boundary lines |
| [OGR_FDW Integration](docs/ogr-fdw.md) | Using external data sources via PostgreSQL FDW |

## Incremental Flow

1. PostgreSQL trigger emits `pg_notify('tile_update', ...)`
2. tilefeed debounces notifications into a batch
3. Events are routed to the correct source based on layer name
4. Affected tiles are derived from new/old feature bounds
5. Tiles are regenerated and written into the source's MBTiles, and exactly those tiles are dropped from the server's tile cache
6. MBTiles artifact is published if `publish_on_update = true`
7. Webhook and SSE consumers are notified with affected zooms, tile counts, `max_zoom` for overzoom awareness, and the changed `areas`; SSE clients that subscribed with a viewport only hear about changes inside it

## Support

<a href="https://www.buymeacoffee.com/muimsd" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-green.png" alt="Buy Me A Coffee" style="height: 60px !important;width: 217px !important;" ></a>

## License

MIT
