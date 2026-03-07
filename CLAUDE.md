# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                  # dev build
cargo build --release        # release build
cargo test                   # run all tests
cargo test tiles::tests      # run specific test module
cargo check                  # fast type-check without codegen
```

The build step compiles `proto/vector_tile.proto` via `prost-build` (see `build.rs`).

## Running

```bash
cargo run -- generate              # full tile generation from PostGIS via Tippecanoe
cargo run -- watch                 # watch LISTEN/NOTIFY and apply incremental updates
cargo run -- run                   # generate then watch
cargo run -- -c other.toml watch   # use alternate config file
```

Requires PostgreSQL with PostGIS extension. Tippecanoe is needed for `generate` and `run`.

## Architecture

**postile** is a PostGIS vector tile pipeline that builds MBTiles and incrementally updates them via PostgreSQL LISTEN/NOTIFY. It does not serve HTTP tiles.

### Multi-source model

The config defines one or more `[[sources]]`, each producing an independent MBTiles file with its own layers and zoom range. Notifications are routed to the correct source by matching the layer name.

### Data flow

1. **Full generation** (`generator.rs`): For each source: PostGIS → GeoJSON export → Tippecanoe → MBTiles file
2. **Incremental updates** (`updater.rs`): PostgreSQL NOTIFY → debounce window → route to source → query affected features → re-encode MVT → write source's MBTiles
3. **Publishing** (`storage.rs`): copy/upload each source's MBTiles artifact to local path, S3, or custom command backend

### Key modules

- **`main.rs`** — CLI (clap), wires up all components, graceful shutdown (SIGTERM/Ctrl+C)
- **`postgis.rs`** — PostGIS reader using `deadpool-postgres` connection pool. Exports GeoJSON, queries features by bounds or ID.
- **`mbtiles.rs`** — SQLite MBTiles store. Auto-materializes Tippecanoe's `tiles` view into a writable table on open.
- **`mvt.rs`** — Native MVT/protobuf encoder. Converts GeoJSON geometries to MVT commands (MoveTo/LineTo/ClosePath with zigzag encoding). Uses `prost` with generated code from `vector_tile.proto`.
- **`updater.rs`** — LISTEN/NOTIFY consumer with debounced batching. Groups events by source, deduplicates affected tiles, regenerates concurrently (semaphore-bounded).
- **`storage.rs`** — Publishing abstraction for MBTiles artifact sync to local filesystem, S3 (`aws s3 cp`), or custom shell command.
- **`tiles.rs`** — Tile math: XYZ coordinate ↔ lon/lat bounds conversion, tiles-for-bounds enumeration.
- **`config.rs`** — Config deserialization from TOML + env vars (prefix `TILES_`).

### Important patterns

- **MBTiles uses TMS y-coordinates** (flipped from XYZ). All `get_tile`/`put_tile` methods handle the conversion: `tms_y = (1 << z) - 1 - y`.
- **No `.await` while holding MBTiles lock**: The updater pre-computes all tile data (PostGIS queries + MVT encoding) before acquiring the `Arc<Mutex<MbtilesStore>>` lock for batch writes.
- **Tippecanoe creates views, not tables**: The MBTiles `open()` method detects and materializes the `tiles` view into a real table so incremental writes work.
- **Layer→source routing**: `AppConfig::find_source_for_layer()` maps a notification's layer name to the owning source. Each source maintains its own MBTiles store.

### Config (`config.toml`)

Sources are defined under `[[sources]]` with: `name`, `mbtiles_path`, `min_zoom`, `max_zoom`.
Layers within each source are defined under `[[sources.layers]]` with: `name`, `table`, `schema`, `geometry_column`, `id_column`, `srid`, `properties`. The `name` field must match the trigger argument in `sql/setup_notify.sql`.
Incremental settings live under `[updates]` (`debounce_ms`, `worker_concurrency`).
Publishing settings live under `[publish]` (`backend`, `destination`, `command`, `args`, `publish_on_generate`, `publish_on_update`).

### Database setup

`sql/setup_notify.sql` installs a PostgreSQL trigger function (`notify_tile_update`) that sends JSON payloads on INSERT/UPDATE/DELETE. Each table needs its own trigger with the layer name as argument.
