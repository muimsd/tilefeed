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
psql -d tilefeed_ogr_example -c "\dt+ buildings"
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

For large remote data sources, consider materializing the foreign table locally:

```sql
-- Create a local materialized view
CREATE MATERIALIZED VIEW buildings_local AS SELECT * FROM buildings;
CREATE INDEX idx_buildings_local_geom ON buildings_local USING GIST (geom);

-- Refresh periodically (or via pg_cron)
REFRESH MATERIALIZED VIEW CONCURRENTLY buildings_local;
```

Then update `config.toml` to point at `buildings_local` instead of `buildings`. You can also attach LISTEN/NOTIFY triggers to the materialized view refresh to enable `tilefeed watch`.

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
