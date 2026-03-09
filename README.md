# tilefeed

![CI](https://github.com/muimsd/tilefeed/actions/workflows/ci.yml/badge.svg)
![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)

A PostGIS vector tile pipeline for **MBTiles generation + incremental updates + storage publish**.

## Architecture

```
PostGIS --> GeoJSON --> Tippecanoe --> MBTiles --+--> Local copy
                                                +--> S3 upload
LISTEN/NOTIFY --> Debounce --> MVT encode ------+--> Custom cmd
```

Full generation exports PostGIS layers as GeoJSON, pipes them through Tippecanoe to produce MBTiles, then publishes the artifact. Incremental updates listen for PostgreSQL notifications, debounce and deduplicate affected tiles, re-encode them as MVT protobuf directly, write them into the existing MBTiles file, and publish again.

## Why tilefeed?

| Tool | Approach | How tilefeed differs |
|------|----------|---------------------|
| **pg_tileserv** | Serves tiles on-the-fly from PostGIS, no caching or pre-generation. | tilefeed pre-generates tiles into MBTiles for predictable latency and CDN-friendly serving. |
| **Martin** | Full-featured tile server with many backends, but more complex to deploy. | tilefeed is headless -- it produces an MBTiles artifact and gets out of the way. Bring your own serving layer (CDN, nginx, tileserver-gl). |
| **t-rex** | Similar pre-generation approach but tightly coupled to its own built-in HTTP server. | tilefeed decouples generation from serving, so you can choose the best serving strategy for your infrastructure. |
| **Tippecanoe cron** | Periodic full re-runs via cron or CI. Misses real-time updates and wastes work regenerating unchanged tiles. | tilefeed adds incremental updates via PostgreSQL LISTEN/NOTIFY, regenerating only the tiles affected by each change. |

## Features

- Cross-platform: runs on Linux, macOS, and Windows
- Full MBTiles generation from PostGIS via Tippecanoe with per-source fine-tuning
- Multiple sources: separate MBTiles outputs with independent layers and zoom ranges
- Incremental tile regeneration using PostgreSQL LISTEN/NOTIFY
- Debounced update batching and concurrent tile rebuild workers
- Optional publish after full generation and/or incremental updates
- Storage backends:
  - Local file copy
  - S3 upload via `aws s3 cp`
  - Custom command runner (for any storage workflow)

## Installation

### Prerequisites

- PostgreSQL with [PostGIS](https://postgis.net/) extension
- [Tippecanoe](https://github.com/felt/tippecanoe) for full generation
- `aws` CLI only if using `publish.backend = "s3"`

### Homebrew (macOS / Linux)

```bash
brew install --formula https://raw.githubusercontent.com/muimsd/tilefeed/main/Formula/tilefeed.rb
```

### Scoop (Windows)

```powershell
scoop bucket add tilefeed https://github.com/muimsd/tilefeed
scoop install tilefeed
```

### Chocolatey (Windows)

```powershell
choco install tilefeed
```

### winget (Windows)

```powershell
winget install muimsd.tilefeed
```

### Debian / Ubuntu (.deb)

Download the `.deb` from [GitHub Releases](https://github.com/muimsd/tilefeed/releases):

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

### From source (all platforms)

Requires [Rust 1.70+](https://rustup.rs/) and `protoc` (protobuf compiler).

```bash
# Install protoc
# Linux (Debian/Ubuntu):
sudo apt-get install -y protobuf-compiler
# macOS:
brew install protobuf
# Windows:
choco install protoc

# Clone and build
git clone https://github.com/muimsd/tilefeed.git
cd tilefeed
cargo build --release

# Binary is at target/release/tilefeed (or tilefeed.exe on Windows)
```

### From pre-built binaries

Download the latest binary for your platform from [GitHub Releases](https://github.com/muimsd/tilefeed/releases):

| Platform | Binary |
|----------|--------|
| Linux x86_64 | `tilefeed-x86_64-unknown-linux-gnu` |
| Linux ARM64 | `tilefeed-aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `tilefeed-aarch64-apple-darwin` |
| macOS Intel | `tilefeed-x86_64-apple-darwin` |
| Windows x86_64 | `tilefeed-x86_64-pc-windows-msvc` |

```bash
# Example: download and install on Linux
curl -L -o tilefeed https://github.com/muimsd/tilefeed/releases/latest/download/tilefeed-x86_64-unknown-linux-gnu
chmod +x tilefeed
sudo mv tilefeed /usr/local/bin/
```

### Install Tippecanoe

Tippecanoe is required for the `generate` and `run` commands.

```bash
# macOS
brew install tippecanoe

# Linux (build from source)
git clone https://github.com/felt/tippecanoe.git
cd tippecanoe
make -j && sudo make install

# Windows
# Use WSL or build with MSYS2. See https://github.com/felt/tippecanoe#windows
```

## Usage

### Commands

```bash
tilefeed generate              # full tile generation from PostGIS via Tippecanoe
tilefeed watch                 # watch LISTEN/NOTIFY and apply incremental updates
tilefeed run                   # generate then watch
tilefeed -c other.toml watch   # use alternate config file
tilefeed --help                # show all options
```

If running from source instead of an installed binary, prefix with `cargo run --release --`:

```bash
cargo run --release -- generate
cargo run --release -- -c myconfig.toml run
```

### Environment variables

All config fields can be overridden with environment variables using the `TILES_` prefix and `__` as a section separator:

```bash
export TILES_DATABASE__HOST=db.example.com
export TILES_DATABASE__PORT=5432
export TILES_DATABASE__USER=myuser
export TILES_DATABASE__PASSWORD=secret
export TILES_DATABASE__DBNAME=geodata
```

You can also use a `.env` file in the project directory.

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

[sources.tippecanoe]
drop_densest_as_needed = true
no_tile_size_limit = true

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

### Tippecanoe settings

Each source can include a `[sources.tippecanoe]` section to fine-tune how Tippecanoe generates tiles. All settings are optional — sensible defaults are applied when omitted.

```toml
[[sources]]
name = "basemap"
mbtiles_path = "./basemap.mbtiles"
min_zoom = 0
max_zoom = 14

[sources.tippecanoe]
# Feature dropping strategies — control how features are pruned at lower zooms
# to keep tile sizes manageable. Pick one or combine as needed.
drop_densest_as_needed = true       # drop features in densest areas first
# drop_fraction_as_needed = true    # drop a random fraction of features
# drop_smallest_as_needed = true    # drop the smallest features first
# coalesce_densest_as_needed = true # merge nearby features in dense areas
# extend_zooms_if_still_dropping = true  # keep zooming if features still overflow

# Drop rate control
# drop_rate = 2.5     # rate features are dropped at lower zooms (default: 2.5)
# base_zoom = 14      # base zoom for drop rate calculation

# Simplification
# simplification = 10.0          # simplification factor in tile coordinate units
# detect_shared_borders = true   # simplify shared polygon borders identically
# no_tiny_polygon_reduction = true  # don't collapse tiny polygons into pixels

# Tile limits
no_tile_size_limit = true        # no max tile size (default: true)
no_feature_limit = true          # no max features per tile (default: 200,000)
# no_tile_compression = true     # skip gzip compression of PBF output

# Geometry detail
# buffer = 5           # pixel buffer around each tile edge (default: 5)
# full_detail = 12     # detail at max zoom, 2^n coordinate units (default: 12 = 4096)
# low_detail = 12      # detail at lower zooms (default: 12)
# minimum_detail = 7   # detail below which features are dropped

# Escape hatch: pass any extra Tippecanoe flags not modeled above
# extra_args = ["--cluster-distance=10", "--accumulate-attribute=count:sum"]
```

| Setting | Tippecanoe flag | Description |
|---------|----------------|-------------|
| `drop_densest_as_needed` | `--drop-densest-as-needed` | Drop features in the densest areas to stay under tile size limits |
| `drop_fraction_as_needed` | `--drop-fraction-as-needed` | Drop a fraction of features at random |
| `drop_smallest_as_needed` | `--drop-smallest-as-needed` | Drop the smallest features first |
| `coalesce_densest_as_needed` | `--coalesce-densest-as-needed` | Merge nearby features in dense areas |
| `extend_zooms_if_still_dropping` | `--extend-zooms-if-still-dropping` | Continue to higher zooms if features are still being dropped |
| `drop_rate` | `--drop-rate` | Rate at which features are dropped at lower zooms (default: 2.5) |
| `base_zoom` | `--base-zoom` | Base zoom level for drop rate calculation |
| `simplification` | `--simplification` | Simplification factor in tile coordinate units |
| `detect_shared_borders` | `--detect-shared-borders` | Detect and simplify shared polygon borders identically |
| `no_tiny_polygon_reduction` | `--no-tiny-polygon-reduction` | Don't collapse very small polygons into single pixels |
| `no_feature_limit` | `--no-feature-limit` | Remove the default 200,000 feature-per-tile limit |
| `no_tile_size_limit` | `--no-tile-size-limit` | Remove the default 500KB tile size limit (default: true) |
| `no_tile_compression` | `--no-tile-compression` | Don't gzip-compress PBF tile data |
| `buffer` | `--buffer` | Pixel buffer around each tile edge |
| `full_detail` | `--full-detail` | Detail level at max zoom (2^n coordinate units) |
| `low_detail` | `--low-detail` | Detail level at lower zoom levels |
| `minimum_detail` | `--minimum-detail` | Minimum detail level below which features are dropped |
| `extra_args` | *(any)* | Array of additional raw Tippecanoe arguments |

The `extra_args` field is an escape hatch for any Tippecanoe option not explicitly modeled. Each array element is passed as a separate argument to the Tippecanoe command.

Backend-specific publish fields:

- `local`: set `publish.destination` to a file path.
- `s3`: set `publish.destination = "s3://bucket/path/tiles.mbtiles"`.
- `command`: set `publish.command`, and use env vars:
  - `TILEFEED_MBTILES_PATH`
  - `TILEFEED_PUBLISH_REASON`

### 3. Run

```bash
# Full rebuild all sources
tilefeed generate

# Incremental watcher only (requires existing MBTiles)
tilefeed watch

# Full rebuild, then keep watching updates
tilefeed run
```

## Serving tiles

tilefeed does not include an HTTP server. It produces MBTiles files and optionally publishes them to a storage backend. To serve tiles to clients, pair it with one of:

- **CDN (CloudFront, Cloudflare R2, etc.)** -- upload the MBTiles to object storage via the S3 or command backend and serve tiles through a CDN edge layer.
- **Martin** -- point [Martin](https://github.com/maplibre/martin) at the MBTiles file for a production-grade tile server with automatic hot-reload.
- **tileserver-gl** -- use [tileserver-gl](https://github.com/maptiler/tileserver-gl) to serve raster and vector tiles from MBTiles with built-in style rendering.
- **nginx with mbtiles module** -- for minimal setups, use an nginx module or a lightweight proxy that reads tiles directly from the SQLite MBTiles file.

This separation lets you choose the serving strategy that fits your infrastructure without being locked into a specific server runtime.

## Using OGR_FDW for external data sources

[OGR_FDW](https://github.com/pramsey/pgsql-ogr-fdw) is a PostgreSQL Foreign Data Wrapper that exposes any OGR-supported data source as a regular table. This lets tilefeed generate vector tiles from Esri FeatureServer, SQL Server, GeoPackage, shapefiles, WFS, and dozens of other formats -- without any code changes.

### How it works

```
Esri FeatureServer ──┐
SQL Server (via TDS) ─┤── OGR_FDW ── PostgreSQL foreign table ── tilefeed ── MBTiles
GeoPackage / SHP ─────┘
```

OGR_FDW creates foreign tables that look and query like regular PostGIS tables. Since tilefeed reads from PostGIS, it works transparently against these tables.

### Setup

```sql
-- Install the extension
CREATE EXTENSION ogr_fdw;

-- Example 1: Esri FeatureServer
CREATE SERVER esri_server
    FOREIGN DATA WRAPPER ogr_fdw
    OPTIONS (
        datasource 'https://services.arcgis.com/ORG_ID/arcgis/rest/services/MyService/FeatureServer/0',
        format 'ESRIJSON'
    );

IMPORT FOREIGN SCHEMA ogr_all
    FROM SERVER esri_server
    INTO public;

-- Example 2: SQL Server via ODBC
CREATE SERVER mssql_server
    FOREIGN DATA WRAPPER ogr_fdw
    OPTIONS (
        datasource 'MSSQL:server=db.example.com;database=geodata;uid=user;pwd=pass',
        format 'MSSQLSpatial'
    );

IMPORT FOREIGN SCHEMA ogr_all
    FROM SERVER mssql_server
    INTO public;

-- Example 3: GeoPackage file
CREATE SERVER gpkg_server
    FOREIGN DATA WRAPPER ogr_fdw
    OPTIONS (
        datasource '/data/parcels.gpkg',
        format 'GPKG'
    );

IMPORT FOREIGN SCHEMA ogr_all
    FROM SERVER gpkg_server
    INTO public;
```

Use `ogr_fdw_info` to discover available layers and columns before importing:

```bash
ogr_fdw_info -s 'https://services.arcgis.com/.../FeatureServer/0'
```

### tilefeed config

Point tilefeed layers at the foreign tables just like any other table:

```toml
[[sources]]
name = "external"
mbtiles_path = "./external.mbtiles"
min_zoom = 0
max_zoom = 14

[[sources.layers]]
name = "parcels"
table = "parcels"          # the foreign table name
geometry_column = "geom"
id_column = "ogc_fid"
srid = 4326
properties = ["owner", "area_sqm", "land_use"]
```

### Considerations

- **No LISTEN/NOTIFY for foreign tables.** Changes happen on the remote side, so PostgreSQL triggers won't fire. Use `tilefeed generate` on a schedule (cron) instead of `tilefeed watch` for these sources.
- **Performance depends on the remote source.** OGR_FDW can push down simple filters, but complex spatial queries may pull entire datasets over the network. For large remote sources, consider materializing the foreign table periodically:
  ```sql
  CREATE MATERIALIZED VIEW parcels_local AS SELECT * FROM parcels;
  REFRESH MATERIALIZED VIEW CONCURRENTLY parcels_local;
  ```
  Then point tilefeed at the materialized view and attach a trigger to refresh + notify.
- **Mixed sources work well.** You can have some sources backed by local PostGIS tables (with LISTEN/NOTIFY for real-time updates) and others backed by OGR_FDW foreign tables (with scheduled `generate` runs). Each `[[sources]]` block operates independently.

## Incremental Flow

1. PostgreSQL trigger emits `pg_notify('tile_update', ...)`
2. `tilefeed` debounces notifications into a batch
3. Events are routed to the correct source based on layer name
4. Affected tiles are derived from new/old feature bounds
5. Tiles are regenerated and written into the source's MBTiles
6. MBTiles artifact is published if `publish_on_update = true`

## License

MIT
