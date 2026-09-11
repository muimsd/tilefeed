# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-09-11

### Added

- **Prometheus metrics**: `GET /metrics` on the tile server, exposing counters, gauges, and histograms for tile serving, full generation, incremental updates, webhook delivery, and artifact publishing
- **Standalone metrics exporter**: `[metrics] port` starts a metrics-only HTTP listener, making `watch` and `run` scrapeable — they have no tile server of their own
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

[0.8.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.8.0
[0.7.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.7.0
[0.1.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.1.0
