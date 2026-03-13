# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[0.1.0]: https://github.com/muimsd/tilefeed/releases/tag/v0.1.0
