# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Tiles on demand.** `generation_backend = "on_demand"` generates nothing up front: `serve` renders each tile from PostGIS the first time it is requested and stores it, and updates drop the tiles they touch so the next request renders the new rows. Empty tiles aren't stored, concurrent requests for one tile render it once, and two new metrics (`tilefeed_tiles_rendered_total`, `tilefeed_tile_render_duration_seconds`) count the renders. See [docs/configuration.md](docs/configuration.md#on-demand).
- **Watching source files.** A source's `watch_files` lists the files (or directories) its data is read from, such as a GeoPackage behind an OGR_FDW foreign table, whose edits never notify. `watch`, `run` and `serve` poll them (`[updates] file_poll_secs`, default 5) and regenerate the source once a change settles, swapping the result into the open MBTiles in one transaction, then clearing the tile cache and sending `generate_complete`. A file changed while tilefeed was stopped is caught at startup, and a failed regeneration is retried with backoff. See [docs/configuration.md](docs/configuration.md#watching-source-files).

### Fixed

- **Publishing after incremental updates** copied the `.mbtiles` without its WAL, so the published file could miss the latest batch. The WAL is now checkpointed into the file before publishing.

## [0.13.1] - 2026-11-11

### Added

- **`[serve] public_url`** sets the address used in TileJSON's tile URLs, for servers behind a proxy or CDN.

### Fixed

- **TileJSON tile URLs** were built from `[serve] host` and `port`, so with the common `host = "0.0.0.0"` (and in Docker) they pointed at an address browsers can't fetch from, and behind a proxy at the internal address. They now use the request's `Host` (and `X-Forwarded-Proto: https`), or `public_url` when set; the response carries `Vary: Host, X-Forwarded-Proto` so shared caches keep them apart.

## [0.13.0] - 2026-11-10

### Added

- **`[updates] listen = false`** turns off LISTEN/NOTIFY: `serve` serves the MBTiles as they are, for sources rebuilt on a schedule instead. `watch` refuses to start with it.

## [0.12.0] - 2026-11-10

### Added

- **Map preview at `/`.** `tilefeed serve` now serves a MapLibre map of every source: layers styled by geometry type with toggles, feature properties on click, framed on the data, the position kept in the URL, and live updates for the current view over `/events`. Turn it off with `[serve] preview = false`. See [docs/serving.md](docs/serving.md#map-preview).
- **TileJSON `bounds` and `center`.** Native generation now writes the MBTiles `bounds` metadata (Tippecanoe already did), incremental updates widen it to cover changed features, and TileJSON reports it. MBTiles files without the metadata get bounds from their stored tiles.

### Changed

- **Big update batches are much faster.** An update used to look up each changed row with its own query before finding the tiles to redraw, so one statement that touched 300 rows meant 300 round trips (about 20 s against a database in another region). It now fetches the bounds of all of a layer's changed rows in one query.

## [0.11.1] - 2026-11-10

### Changed

- **The webhook + SSE demo** (`cargo run --example webhook_sse_demo`) now runs tilefeed's own event bus and viewport filter, so its `/events` matches `tilefeed serve`: `bbox`/`zoom`/`source`, event ids, `since` replay, `ready` and `resync`. It simulates edits in three cities, sends CORS headers so `map.html` opened from disk can connect, and answers tile requests with empty tiles instead of 404s.

### Fixed

- **`srid` was ignored.** Layers whose geometries have no SRID (0) failed everywhere with `ST_Transform: Input geometry has unknown (0) SRID`, and so did every UPDATE or DELETE on their table, because the notify trigger hit the same error. `srid` now sets the SRID of geometries stored without one; geometries with an SRID are still transformed from their own, so an `srid = 4326` copied from the examples changes nothing for them. The trigger takes the SRID as an optional second argument (`notify_tile_update('layer', '3857')`); without it, an SRID-0 row's write succeeds and the notification just carries no old bounds. Re-run `sql/setup_notify.sql` to update the function. `tilefeed validate` reports SRID-0 geometries on a layer without `srid`.

## [0.11.0] - 2026-11-06

### Added

- **Viewport-filtered SSE.** `GET /events?bbox=w,s,e,n&zoom=z&source=name` sends a client only the updates that change tiles in its view: outside the box, or at zooms where the changed layer isn't drawn, they are skipped. Handles antimeridian and unwrapped longitudes from MapLibre/Mapbox `getBounds()`, and overzoom past the source's `max_zoom`. `update_complete` events (SSE and webhooks) now include `areas`, the bounding box and zoom range of each change. Every SSE event now has an `id`. The server keeps the last 256 events, so a client reconnecting with `since=<id>` (or `Last-Event-ID`) gets the updates it missed. A `ready` event marks the end of the replay, and a `resync` event tells the client to reload when the missed events are no longer available. The MapLibre example re-subscribes as the map moves, without losing updates. See [docs/serving.md](docs/serving.md#only-the-changes-in-view).

### Fixed

- **Layers without `properties` failed** in native generation, incremental updates and Tippecanoe export (`SELECT * with no tables specified is not valid`). They now include every column except geometry and geography columns.
- A notification whose old geometry was NULL (or whose `old_bounds` was incomplete) was read as a box at 0,0, so tiles at Null Island were regenerated for nothing.

## [0.10.0] - 2026-11-06

### Added

- **Tile cache.** `serve` keeps tiles in memory with their ETag, in an LRU bounded by `[serve.cache] max_size_mb` (default 64) with an optional `ttl_secs`. Incremental updates clear exactly the tiles they rewrite, right after the MBTiles commit, and commits by other processes (a cron `generate`, a separate `watch`) clear the source within about 2 seconds, detected via SQLite's `data_version`. Only zooms within the source's range are cached. New metrics: `tilefeed_tile_cache_*`.
- **Out-of-range tile coordinates return 404.** `x` or `y` ≥ 2^z (or z > 30) used to wrap around in the TMS flip.
- **Configurable `Cache-Control`.** `[serve] max_age_secs` (default 300, the previous fixed value). `0` sends `no-cache`, so browsers revalidate with the ETag and see updates immediately. 304 responses now carry `Cache-Control` too.
- **Label point method.** Layers with `generate_label_points = true` accept `label_point_method` (`point_on_surface`, the default and previous behaviour, or `centroid` for the center of mass) and `label_point_column` (use an existing point column instead of computing one).
- **TLS to PostgreSQL.** New `[database]` keys `sslmode` (`disable` | `prefer` | `require` | `verify-ca` | `verify-full`, libpq semantics, default `prefer`) and `sslrootcert`. Uses rustls (ring) for both the connection pool and the LISTEN/NOTIFY connection, so managed databases that enforce TLS (Supabase, Neon, RDS, Cloud SQL, Crunchy Bridge) can now be reached. `verify-ca` requires `sslrootcert`; `verify-full` verifies against `sslrootcert` or the bundled webpki roots. Unix-socket hosts never use TLS, as in libpq.
- **Per-layer zoom range**: `min_zoom` / `max_zoom` on `[[sources.layers]]` restrict a layer to part of its source's zoom range (e.g. buildings only from zoom 13). Either may be set alone; derived `_labels` / `_boundary` layers follow their parent. All three backends honour it — Tippecanoe through per-feature `"tippecanoe": {"minzoom", "maxzoom"}` objects, GDAL through the `MINZOOM`/`MAXZOOM` layer creation options, and the native encoder by skipping the layer outside its range. Incremental updates only re-encode tiles inside the changed layer's range and never write a layer into a tile outside it, and TileJSON reports each layer's `minzoom`/`maxzoom`. See [docs/configuration.md](docs/configuration.md#per-layer-zoom-range).

### Fixed

- Incremental updates failed for layers whose `id_column` is `int2`/`int4` (`serial`, `integer`): the feature lookup bound an `i64` parameter against a 32-bit column. The id is now cast to `bigint` and decoded from any integer width.
- Per-layer `min_zoom` / `max_zoom` keys were accepted and silently ignored, so a layer meant for "zoom 10–14" appeared at every zoom of its source. Config loading now also rejects a range with `min_zoom > max_zoom` or one outside the source's range, naming the layer.
- **The native generator no longer walks the whole world.** `generation_backend = "native"` iterated every `x,y` at every zoom and issued a query per tile per layer, so even a 900-row table at z10–15 never finished. It now restricts tiles to the data extent and descends the quadtree only below non-empty tiles, running tile queries concurrently. Output for tiles with data is unchanged.
- Listener reconnect logs now include the underlying cause (e.g. `server does not support TLS`) instead of only the outermost error context.
- Database connections are configured field by field rather than through a `key=value` string, so passwords containing spaces or quotes work for the LISTEN connection.

## [0.9.0] - 2026-09-12

### Added

- **PMTiles export**: `tilefeed export <in.mbtiles> <out.pmtiles>` writes a [PMTiles v3](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md) archive for static hosting on S3, R2, or any CDN — no tile server required. Deduplicates identical tiles, collapses consecutive runs, orders tiles along a Hilbert curve, and builds leaf directories when the root would exceed 16 KiB. Archives are opened read-only (so an immutable or read-only artifact exports fine), tile bytes are staged through a temporary file so memory scales with tile *count* rather than size, and every archive is read back before the command reports success — structurally, and by comparing up to 64 sampled tiles byte-for-byte against the source. See [docs/pmtiles.md](docs/pmtiles.md).

### Fixed

- `tilefeed --version` / `-V` now report the version instead of failing with "unexpected argument"
- **Package manifests are all updated at release time.** The AUR, winget and Chocolatey manifests were frozen at 0.1.0 with literal `__CHECKSUM__` placeholders — only Homebrew and Scoop were maintained. All six are now generated by `scripts/update-manifests.sh`, which the release workflow calls and which can be run by hand to repair a manifest.
- The generated Homebrew formula escaped its Ruby interpolation (`"\#{bin}/tilefeed"`), so `brew test tilefeed` ran a command literally named `#{bin}/tilefeed` and failed. The generated Scoop manifest had its indentation mangled by a dedent `sed`. Both came from embedding heredocs in the workflow YAML.
- Release asset downloads use `curl -f`, retry transient CDN failures, and check each archive's type, so a missing or truncated asset fails the job instead of publishing the checksum of GitHub's HTML error page.
- **`brew install` on Linux arm64 installed an x86_64 binary.** The generated formula had no `on_arm` branch under `on_linux`, so Graviton/Ampere hosts and Docker on Apple Silicon downloaded the x86_64 tarball and got `Exec format error`. The aarch64 Linux asset was being built and published all along, just never referenced.
- A prerelease tag (`v1.2.3-rc1`) would have overwritten the stable manifests with release-candidate URLs and written a `pkgver` that makepkg rejects. Both the release job and the script now refuse anything that is not a plain `MAJOR.MINOR.PATCH`.
- The release job no longer interpolates the tag name into a shell command, and fails loudly if a release changes no manifest at all.

## [0.8.1] - 2026-09-11

This is the first published build of the 0.8 line: 0.8.0 was prepared but never
tagged, so everything under it below ships here too. Anyone on 0.7.x should
upgrade — the HTTP tile server could not start at all in those releases.

### Added

- **`--skip-generate` for `serve` and `run`**: start from the MBTiles already on disk instead of rebuilding it first. Restarts are immediate, Tippecanoe/GDAL are no longer required for a serving-only process, and the server comes up while PostgreSQL is unreachable — the LISTEN/NOTIFY watcher reconnects on its own.
- `tilefeed_startup_info{command,generated}` and `tilefeed_mbtiles_tiles_at_open{source}` metrics, so a scrape shows whether a process regenerated at startup and how many tiles it opened with

### Fixed

- **The HTTP tile server could not start.** `serve` panicked immediately with `Invalid route "/{source}/{z}/{x}/{y}.pbf": Only one parameter is allowed per path segment` — axum does not allow a literal next to a parameter in one path segment. The published URLs are unchanged; the handlers now parse the `.pbf`/`.json` suffix. The tests had declared their own route patterns, which is why this went unnoticed; they now build the same router the binary does.
- Opening a missing or invalid MBTiles file now fails with a clear message naming the file. SQLite was creating an empty database instead, which `serve`, `watch`, `inspect`, and `diff` then treated as real.
- **A dropped PostgreSQL connection killed the process.** Only the *initial* connect retried; a connection lost mid-run closed the notification channel, which the listener reported as a clean exit — ending `serve` and taking the HTTP tile server down with it. Both cases now reconnect with backoff.
- `tilefeed validate` no longer reports success when Tippecanoe or GDAL is missing.

## [0.8.0] - unreleased

Prepared but never tagged; released as part of 0.8.1.

### Added

- **Prometheus metrics**: `GET /metrics` on the tile server, exposing counters, gauges, and histograms for tile serving, full generation, incremental updates, webhook delivery, and artifact publishing
- **Dedicated metrics listener**: `[metrics] port` serves metrics on their own listener instead of the tile port, and makes `watch` and `run` scrapeable — they have no tile server of their own
- `[metrics]` config section (`enabled`, `path`, `host`, `port`); metrics are on by default at `/metrics`
- Metrics documentation with a full metric reference, PromQL examples, and alerting starters (`docs/metrics.md`)

### Fixed

- **Tile `Content-Encoding`**: the server advertised `gzip` on every tile regardless of how it was stored, so tiles generated with Tippecanoe's `no_tile_compression` were undecodable in the browser. The header is now set from the stored bytes.

## [0.7.0] - 2026-03-13

### Added

- **Webhook notifications**: HTTP POST to external URLs on tile generation and incremental updates, with HMAC-SHA256 signing (`X-Tilefeed-Signature` header) and retry with exponential backoff
- **Server-Sent Events (SSE)**: `GET /events` endpoint for live tile refresh in frontends (MapLibre, Leaflet, etc.)
- **Cooldown / throttle**: `cooldown_secs` config to aggregate rapid-fire events per source into a single notification, preventing frontend flooding during bulk database changes
- **Overzoom awareness**: `max_zoom` field in `update_complete` events so frontends can invalidate overzoomed tile views (tiles rendered beyond the source's configured max zoom)
- **Event merging**: accumulated tile counts, zoom levels, and layers during cooldown windows
- MapLibre integration example (`examples/webhook-sse/map.html`)

## [0.1.0] - 2026-03-08

Initial release.

### Added

- Full MBTiles generation from PostGIS via Tippecanoe
- Incremental tile updates using PostgreSQL LISTEN/NOTIFY with debounced batching
- Multiple sources: independent MBTiles outputs with separate layers and zoom ranges
- Native MVT/protobuf encoder with zigzag encoding for all geometry types (Point, LineString, Polygon, Multi*)
- Storage publish backends: local file copy, S3 upload (`aws s3 cp`), custom shell command
- Cross-platform support: Linux, macOS (Intel + Apple Silicon), Windows
- CLI with `generate`, `watch`, and `run` subcommands
- Configuration via TOML file and environment variables (`TILES_` prefix)
- Graceful shutdown on SIGTERM/Ctrl+C
- CI with tests on all platforms, clippy, formatting checks, and release builds
- Local-parks example with end-to-end walkthrough

[Unreleased]: https://github.com/muimsd/tilefeed/compare/v0.13.1...HEAD
[0.13.1]: https://github.com/muimsd/tilefeed/releases/tag/v0.13.1
[0.13.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.13.0
[0.12.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.12.0
[0.11.1]: https://github.com/muimsd/tilefeed/releases/tag/v0.11.1
[0.11.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.11.0
[0.10.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.10.0
[0.9.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.9.0
[0.8.1]: https://github.com/muimsd/tilefeed/releases/tag/v0.8.1
[0.7.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.7.0
[0.1.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.1.0
