# Example: OGR_FDW External Data Sources

Generate vector tiles from external data sources using PostgreSQL's [OGR_FDW](https://github.com/pramsey/pgsql-ogr-fdw) Foreign Data Wrapper. This example shows how to connect to an Esri FeatureServer, but the same approach works for SQL Server, GeoPackage, shapefiles, WFS, and any other OGR-supported format.

## Prerequisites

- PostgreSQL 14+ with PostGIS extension
- [ogr_fdw](https://github.com/pramsey/pgsql-ogr-fdw) extension
- [Tippecanoe](https://github.com/felt/tippecanoe)

### Installing ogr_fdw

```bash
# Debian/Ubuntu
sudo apt install postgresql-17-ogr-fdw

# macOS (Homebrew)
brew install pgsql-ogr-fdw

# From source
git clone https://github.com/pramsey/pgsql-ogr-fdw.git
cd pgsql-ogr-fdw
make && sudo make install
```

## Setup

1. Edit `setup.sql` and replace the `datasource` URL with your Esri FeatureServer endpoint (or other OGR source).

2. Create the database and run the setup:

```bash
createdb tilefeed_ogr_example
psql -d tilefeed_ogr_example < examples/ogr-fdw/setup.sql
```

3. Verify the foreign table was created:

```bash
psql -d tilefeed_ogr_example -c "\det+"            # lists foreign tables
psql -d tilefeed_ogr_example -c "\d buildings"      # columns of this one
```

## Discover available layers

Use `ogr_fdw_info` to inspect what layers and columns are available from a remote source before importing:

```bash
# Esri FeatureServer
ogr_fdw_info -s 'https://services.arcgis.com/YOUR_ORG/arcgis/rest/services/Buildings/FeatureServer/0'

# SQL Server
ogr_fdw_info -s 'MSSQL:server=db.example.com;database=geodata;uid=user;pwd=pass'

# GeoPackage
ogr_fdw_info -s '/data/parcels.gpkg'
```

## Generate tiles

```bash
cargo run --release -- -c examples/ogr-fdw/config.toml generate
```

This queries the remote data source through OGR_FDW, exports to GeoJSON, and builds MBTiles via Tippecanoe.

## Scheduling rebuilds

Since foreign tables don't support PostgreSQL LISTEN/NOTIFY, use cron or a scheduler for periodic rebuilds:

```bash
# Rebuild tiles every 15 minutes
*/15 * * * * cd /path/to/tilefeed && ./tilefeed -c examples/ogr-fdw/config.toml generate
```

## Performance tips

For large remote data sources, keep a local copy and point tilefeed at it. A materialized view is the simplest:

```sql
CREATE MATERIALIZED VIEW buildings_local AS SELECT * FROM buildings;
CREATE UNIQUE INDEX ON buildings_local (id);   -- required for CONCURRENTLY
CREATE INDEX ON buildings_local USING GIST (geom);

-- Refresh periodically (or via pg_cron), then run `tilefeed generate`
REFRESH MATERIALIZED VIEW CONCURRENTLY buildings_local;
```

PostgreSQL can't put triggers on a materialized view, so this copy works with scheduled `tilefeed generate` runs, not `tilefeed watch`. To keep incremental updates, sync into a regular table instead. Only rows that actually changed are written, so only they fire the notify trigger:

```sql
CREATE TABLE buildings_local AS SELECT * FROM buildings;
ALTER TABLE buildings_local ADD PRIMARY KEY (id);
CREATE INDEX ON buildings_local USING GIST (geom);
-- then attach notify_tile_update('buildings') to buildings_local, as in sql/setup_notify.sql

-- Sync (pg_cron or any scheduler). List your own columns in SET and WHERE.
INSERT INTO buildings_local SELECT * FROM buildings
ON CONFLICT (id) DO UPDATE
  SET name = EXCLUDED.name, geom = EXCLUDED.geom
  WHERE (buildings_local.name, buildings_local.geom)
        IS DISTINCT FROM (EXCLUDED.name, EXCLUDED.geom);
DELETE FROM buildings_local l
WHERE NOT EXISTS (SELECT 1 FROM buildings b WHERE b.id = l.id);
```

## Supported data sources

OGR_FDW supports 80+ formats. Common ones for vector tiles:

| Source | OGR format | Example datasource |
|--------|-----------|-------------------|
| Esri FeatureServer | `ESRIJSON` | `https://services.arcgis.com/.../FeatureServer/0` |
| SQL Server | `MSSQLSpatial` | `MSSQL:server=host;database=db;uid=u;pwd=p` |
| GeoPackage | `GPKG` | `/data/file.gpkg` |
| Shapefile | `ESRI Shapefile` | `/data/parcels.shp` |
| WFS | `WFS` | `https://example.com/wfs?service=WFS` |
| GeoJSON | `GeoJSON` | `/data/features.geojson` |
| Oracle Spatial | `OCI` | `OCI:user/pass@host:port/db` |
| MySQL | `MySQL` | `MYSQL:db,host=h,user=u,password=p` |
| CSV (with coords) | `CSV` | `/data/points.csv` |

## Cleanup

```bash
dropdb tilefeed_ogr_example
rm -f examples/ogr-fdw/buildings.mbtiles
rm -rf examples/ogr-fdw/output/
```
