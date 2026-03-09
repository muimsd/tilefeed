# Example: Serve with Simplification

Demonstrates the HTTP tile server with geometry simplification, feature filtering, and per-zoom property rules.

## Features shown

- `tilefeed serve` — generate + watch + HTTP server
- `filter` — exclude private parks via SQL expression
- `simplify_tolerance` — Douglas-Peucker simplification at lower zooms
- `property_rules` — drop heavy properties (description, area) below zoom 8
- `[serve]` — CORS and port configuration

## Setup

```bash
createdb tilefeed_example
psql -d tilefeed_example < examples/serve-with-simplification/setup.sql
```

## Run

```bash
cargo run --release -- -c examples/serve-with-simplification/config.toml serve
```

Tiles available at:
- `http://localhost:3000/parks/{z}/{x}/{y}.pbf`
- `http://localhost:3000/parks.json` (TileJSON)
- `http://localhost:3000/health`

## Verify

```bash
# TileJSON
curl -s http://localhost:3000/parks.json | jq .

# Fetch a tile (SF area at zoom 10)
curl -s -o /dev/null -w "%{http_code}" http://localhost:3000/parks/10/163/395.pbf

# ETag support (304 on second request)
ETAG=$(curl -sI http://localhost:3000/parks/10/163/395.pbf | grep -i etag | tr -d '\r' | awk '{print $2}')
curl -s -o /dev/null -w "%{http_code}" -H "If-None-Match: $ETAG" http://localhost:3000/parks/10/163/395.pbf
```

## Inspect & Diff

```bash
# Inspect the generated MBTiles
cargo run -- inspect examples/serve-with-simplification/parks.mbtiles

# Make a change, re-generate, and diff
cp examples/serve-with-simplification/parks.mbtiles /tmp/before.mbtiles
psql -d tilefeed_example -c "INSERT INTO parks (name, type, geom) VALUES ('New Park', 'city_park', ST_GeomFromText('POLYGON((-122.42 37.76, -122.41 37.76, -122.41 37.77, -122.42 37.77, -122.42 37.76))', 4326));"
# Wait for incremental update, then:
cargo run -- diff /tmp/before.mbtiles examples/serve-with-simplification/parks.mbtiles
```

## Cleanup

```bash
dropdb tilefeed_example
rm -f examples/serve-with-simplification/parks.mbtiles
```
