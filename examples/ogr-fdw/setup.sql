-- Example: OGR_FDW with Esri FeatureServer
-- Demonstrates using PostgreSQL Foreign Data Wrapper to generate
-- vector tiles from an external Esri FeatureServer endpoint.
--
-- Prerequisites:
--   - PostgreSQL with PostGIS and ogr_fdw extensions
--   - Network access to the Esri FeatureServer endpoint
--
-- Install ogr_fdw:
--   - Debian/Ubuntu: apt install postgresql-17-ogr-fdw
--   - macOS (Homebrew): brew install pgsql-ogr-fdw
--   - From source: https://github.com/pramsey/pgsql-ogr-fdw

CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS ogr_fdw;

-- Connect to an Esri FeatureServer.
-- Replace the datasource URL with your own endpoint.
CREATE SERVER esri_buildings
    FOREIGN DATA WRAPPER ogr_fdw
    OPTIONS (
        datasource 'https://services.arcgis.com/YOUR_ORG/arcgis/rest/services/Buildings/FeatureServer/0',
        format 'ESRIJSON'
    );

-- Import all layers from the server into the public schema.
-- This creates foreign tables that mirror the remote layer structure.
IMPORT FOREIGN SCHEMA ogr_all
    FROM SERVER esri_buildings
    INTO public;

-- Alternatively, define the foreign table manually for more control:
--
-- CREATE FOREIGN TABLE buildings (
--     ogc_fid integer,
--     name text,
--     type text,
--     height double precision,
--     geom geometry(Polygon, 4326)
-- )
-- SERVER esri_buildings
-- OPTIONS (layer 'Buildings');

-- Optional: create a materialized view for better performance.
-- This caches the remote data locally and allows LISTEN/NOTIFY triggers.
--
-- CREATE MATERIALIZED VIEW buildings_local AS
--     SELECT * FROM buildings;
--
-- CREATE INDEX idx_buildings_local_geom ON buildings_local USING GIST (geom);
--
-- -- Refresh on a schedule (e.g. via pg_cron):
-- -- SELECT cron.schedule('refresh-buildings', '*/15 * * * *',
-- --     'REFRESH MATERIALIZED VIEW CONCURRENTLY buildings_local');

-- Example: SQL Server via ODBC
-- Requires: ODBC driver for SQL Server
--
-- CREATE SERVER mssql_parcels
--     FOREIGN DATA WRAPPER ogr_fdw
--     OPTIONS (
--         datasource 'MSSQL:server=db.example.com;database=geodata;uid=user;pwd=pass',
--         format 'MSSQLSpatial'
--     );
--
-- IMPORT FOREIGN SCHEMA ogr_all
--     FROM SERVER mssql_parcels
--     INTO public;

-- Example: GeoPackage file
--
-- CREATE SERVER gpkg_data
--     FOREIGN DATA WRAPPER ogr_fdw
--     OPTIONS (
--         datasource '/data/parcels.gpkg',
--         format 'GPKG'
--     );
--
-- IMPORT FOREIGN SCHEMA ogr_all
--     FROM SERVER gpkg_data
--     INTO public;
