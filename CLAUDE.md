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
cargo run -- serve            # start tile server (requires existing tiles.mbtiles)
cargo run -- generate         # full tile generation from PostGIS via Tippecanoe
cargo run -- run              # generate then serve
cargo run -- -c other.toml serve  # use alternate config file
```

Requires PostgreSQL with PostGIS extension. Tippecanoe is needed only for the `generate`/`run` commands.

## Architecture

**postile** is a PostGIS vector tile server that serves MVT tiles from MBTiles (SQLite) and incrementally updates them via PostgreSQL LISTEN/NOTIFY.

### Data flow

1. **Full generation** (`generator.rs`): PostGIS → GeoJSON export → Tippecanoe → MBTiles file
2. **Serving** (`server.rs`): HTTP request → LRU cache → MBTiles SQLite → gzipped PBF response
3. **Incremental updates** (`updater.rs`): PostgreSQL NOTIFY → debounce window → query affected features → re-encode MVT → write MBTiles + invalidate/repopulate cache

### Key modules

- **`main.rs`** — CLI (clap), wires up all components, graceful shutdown (SIGTERM/Ctrl+C)
- **`server.rs`** — Axum HTTP server. Routes: `/tiles/:z/:x/:y_pbf`, `/tiles.json` (TileJSON 3.0), `/health`, `/metadata`, `/stats`. ETag conditional responses (304).
- **`postgis.rs`** — PostGIS reader using `deadpool-postgres` connection pool. Exports GeoJSON, queries features by bounds or ID.
- **`mbtiles.rs`** — SQLite MBTiles store. Auto-materializes Tippecanoe's `tiles` view into a writable table on open.
- **`mvt.rs`** — Native MVT/protobuf encoder. Converts GeoJSON geometries to MVT commands (MoveTo/LineTo/ClosePath with zigzag encoding). Uses `prost` with generated code from `vector_tile.proto`.
- **`updater.rs`** — LISTEN/NOTIFY consumer with debounced batching. Collects notifications within a configurable window, deduplicates affected tiles, regenerates concurrently (semaphore-bounded to 8).
- **`tiles.rs`** — Tile math: XYZ coordinate ↔ lon/lat bounds conversion, tiles-for-bounds enumeration.
- **`cache.rs`** — LRU tile cache with atomic hit/miss counters and ETag computation.
- **`config.rs`** — Config deserialization from TOML + env vars (prefix `TILES_`).

### Important patterns

- **MBTiles uses TMS y-coordinates** (flipped from XYZ). All `get_tile`/`put_tile` methods handle the conversion: `tms_y = (1 << z) - 1 - y`.
- **Axum route syntax**: This project uses `:param` style (not `{param}`) for path parameters due to matchit 0.7 compatibility. The `.pbf` extension is parsed from a String parameter, not baked into the route.
- **No `.await` while holding MBTiles lock**: The updater pre-computes all tile data (PostGIS queries + MVT encoding) before acquiring the `Arc<Mutex<MbtilesStore>>` lock for batch writes.
- **Tippecanoe creates views, not tables**: The MBTiles `open()` method detects and materializes the `tiles` view into a real table so incremental writes work.

### Config (`config.toml`)

Layers are defined under `[[tiles.layers]]` with: `name`, `table`, `schema`, `geometry_column`, `id_column`, `srid`, `properties`. The `name` field must match the trigger argument in `sql/setup_notify.sql`.

### Database setup

`sql/setup_notify.sql` installs a PostgreSQL trigger function (`notify_tile_update`) that sends JSON payloads on INSERT/UPDATE/DELETE. Each table needs its own trigger with the layer name as argument.
