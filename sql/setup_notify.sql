-- Run this SQL in your PostGIS database to enable LISTEN/NOTIFY for tile updates.
-- Adjust the table name and columns to match your schema.

-- Generic trigger function that sends notifications on INSERT/UPDATE/DELETE.
--
-- Arguments: the layer name (must match `name` in config.toml) and,
-- optionally, the SRID of geometries stored without one (SRID 0, as
-- OGR_FDW and shp2pgsql without -s leave them); pass the layer's `srid`.
-- Without it, an SRID-0 row's old position is unknown: the notification
-- carries no old_bounds, so tiles at a moved or deleted feature's old place
-- are not cleared, but the write itself still succeeds.
CREATE OR REPLACE FUNCTION notify_tile_update()
RETURNS trigger AS $$
DECLARE
    payload TEXT;
    layer_name TEXT;
    feature_id BIGINT;
    old_geom geometry;
    old_box geometry;
BEGIN
    -- The layer name is passed via TG_ARGV[0]
    layer_name := TG_ARGV[0];

    IF TG_OP = 'INSERT' THEN
        feature_id := NEW.id;
        payload := json_build_object(
            'layer', layer_name,
            'id', feature_id,
            'op', 'insert'
        )::TEXT;
    ELSE
        -- UPDATE and DELETE include the old bounds, so tiles at the old
        -- location are refreshed (or cleared) too
        IF TG_OP = 'DELETE' THEN
            feature_id := OLD.id;
        ELSE
            feature_id := NEW.id;
        END IF;

        old_geom := OLD.geom;
        IF old_geom IS NOT NULL AND ST_SRID(old_geom) = 0 THEN
            IF TG_NARGS > 1 THEN
                old_geom := ST_SetSRID(old_geom, TG_ARGV[1]::int);
            ELSE
                old_geom := NULL;
            END IF;
        END IF;
        old_box := ST_Envelope(ST_Transform(old_geom, 4326));

        payload := json_build_object(
            'layer', layer_name,
            'id', feature_id,
            'op', lower(TG_OP),
            'old_bounds', json_build_object(
                'min_lon', ST_XMin(old_box),
                'min_lat', ST_YMin(old_box),
                'max_lon', ST_XMax(old_box),
                'max_lat', ST_YMax(old_box)
            )
        )::TEXT;
    END IF;

    PERFORM pg_notify('tile_update', payload);

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Example: attach the trigger to a 'buildings' table
-- Change 'buildings' to your actual table name
-- The argument 'buildings' must match the layer name in config.toml
DROP TRIGGER IF EXISTS tile_update_trigger ON buildings;
CREATE TRIGGER tile_update_trigger
    AFTER INSERT OR UPDATE OR DELETE ON buildings
    FOR EACH ROW
    EXECUTE FUNCTION notify_tile_update('buildings');

-- A table whose geometries have no SRID (0) passes the SRID as a second
-- argument, matching the layer's `srid`:
--     EXECUTE FUNCTION notify_tile_update('parcels', '2263');

-- To add more tables, just create additional triggers:
-- DROP TRIGGER IF EXISTS tile_update_trigger ON roads;
-- CREATE TRIGGER tile_update_trigger
--     AFTER INSERT OR UPDATE OR DELETE ON roads
--     FOR EACH ROW
--     EXECUTE FUNCTION notify_tile_update('roads');
