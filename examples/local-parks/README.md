# Example: Local Parks

End-to-end example using sample park and trail data in San Francisco. Demonstrates full MBTiles generation from PostGIS via Tippecanoe, then incremental tile updates via LISTEN/NOTIFY.

## Prerequisites

- PostgreSQL 17 with PostGIS extension
- [Tippecanoe](https://github.com/felt/tippecanoe)
- Rust toolchain

## Setup

```bash
# Create the database and load sample data (5 parks, 4 trails)
createdb tilefeed_example
psql -d tilefeed_example < examples/local-parks/setup.sql
```

This creates two tables (`parks` polygons, `trails` linestrings) with sample SF geometries, spatial indexes, and LISTEN/NOTIFY triggers.

## Configure

Edit `examples/local-parks/config.toml` to match your PostgreSQL credentials, or override via environment variables:

```bash
export TILES_DATABASE__USER=myuser
export TILES_DATABASE__PASSWORD=mypassword
```

## Complete Flow

All commands should be run from the project root.

### Step 1: Generate MBTiles

```bash
cargo run --release -- -c examples/local-parks/config.toml generate
```

This exports all layers to GeoJSON, runs Tippecanoe to build MBTiles, and publishes the artifact to `examples/local-parks/output/parks.mbtiles`.

Verify the output:

```bash
sqlite3 examples/local-parks/parks.mbtiles "SELECT COUNT(*) FROM tiles;"
# => 50 tiles
```

### Step 2: Start the watcher

In a terminal, start the incremental update watcher:

```bash
cargo run --release -- -c examples/local-parks/config.toml watch
```

You should see:

```
INFO tilefeed::updater: Listening for tile_update notifications on PostgreSQL
```

### Step 3: Make changes in another terminal

While the watcher is running, open another terminal and modify data:

```bash
# Insert a new park
psql -d tilefeed_example -c "
INSERT INTO parks (name, type, geom) VALUES (
    'Twin Peaks',
    'scenic_overlook',
    ST_GeomFromText('POLYGON((-122.4490 37.7525, -122.4440 37.7525, -122.4440 37.7560, -122.4490 37.7560, -122.4490 37.7525))', 4326)
);
"

# Update an existing trail
psql -d tilefeed_example -c "
UPDATE trails SET difficulty = 'moderate', length_km = 2.0 WHERE name = 'Glen Park Loop';
"

# Delete a park
psql -d tilefeed_example -c "
DELETE FROM parks WHERE name = 'Buena Vista Park';
"
```

### Step 4: Observe incremental updates

The watcher terminal should show tiles being regenerated for each change:

```
INFO tilefeed::updater: Processing batch of 1 notification(s)
INFO tilefeed::updater: Regenerating 17 unique tiles from batch
INFO tilefeed::updater: Batch update complete (17 tiles)
INFO tilefeed::updater: Processing batch of 1 notification(s)
INFO tilefeed::updater: Regenerating 15 unique tiles from batch
INFO tilefeed::updater: Batch update complete (15 tiles)
INFO tilefeed::updater: Processing batch of 1 notification(s)
INFO tilefeed::updater: Regenerating 16 unique tiles from batch
INFO tilefeed::updater: Batch update complete (16 tiles)
```

Each INSERT/UPDATE/DELETE fires a PostgreSQL NOTIFY event. The watcher debounces them, computes affected tiles from feature bounds, and regenerates only those tiles in the MBTiles file.

### Alternative: generate + watch in one command

Instead of running steps 1 and 2 separately, use `run` to do both:

```bash
cargo run --release -- -c examples/local-parks/config.toml run
```

## Cleanup

```bash
dropdb tilefeed_example
rm -f examples/local-parks/parks.mbtiles
rm -rf examples/local-parks/output/
```
