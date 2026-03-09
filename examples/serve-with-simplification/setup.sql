-- Example: Parks with extended schema for simplification + filtering demo
CREATE EXTENSION IF NOT EXISTS postgis;

CREATE TABLE IF NOT EXISTS parks (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    description TEXT,
    area_sqm DOUBLE PRECISION,
    geom geometry(Polygon, 4326) NOT NULL
);

CREATE TABLE IF NOT EXISTS trails (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    difficulty TEXT DEFAULT 'easy',
    length_km DOUBLE PRECISION,
    geom geometry(LineString, 4326) NOT NULL
);

-- Sample parks (SF area)
INSERT INTO parks (name, type, description, area_sqm, geom) VALUES
    ('Golden Gate Park', 'city_park',
     'Large urban park spanning 1,017 acres in western San Francisco.',
     4117000,
     ST_GeomFromText('POLYGON((-122.5108 37.7694, -122.4534 37.7694, -122.4534 37.7750, -122.5108 37.7750, -122.5108 37.7694))', 4326)),
    ('Dolores Park', 'city_park',
     'Popular park in the Mission District with city views.',
     64000,
     ST_GeomFromText('POLYGON((-122.4280 37.7596, -122.4250 37.7596, -122.4250 37.7620, -122.4280 37.7620, -122.4280 37.7596))', 4326)),
    ('Buena Vista Park', 'city_park',
     'Oldest official park in San Francisco, steep and wooded.',
     148000,
     ST_GeomFromText('POLYGON((-122.4420 37.7680, -122.4380 37.7680, -122.4380 37.7710, -122.4420 37.7710, -122.4420 37.7680))', 4326)),
    ('Private Garden', 'private',
     'This should be filtered out by the filter expression.',
     500,
     ST_GeomFromText('POLYGON((-122.4100 37.7800, -122.4090 37.7800, -122.4090 37.7810, -122.4100 37.7810, -122.4100 37.7800))', 4326));

INSERT INTO trails (name, difficulty, length_km, geom) VALUES
    ('Coastal Trail', 'moderate', 5.2,
     ST_GeomFromText('LINESTRING(-122.5100 37.7900, -122.5000 37.7880, -122.4900 37.7850, -122.4800 37.7820)', 4326)),
    ('Glen Park Loop', 'easy', 1.5,
     ST_GeomFromText('LINESTRING(-122.4350 37.7380, -122.4300 37.7370, -122.4280 37.7360, -122.4350 37.7380)', 4326));

-- Spatial indexes
CREATE INDEX IF NOT EXISTS idx_parks_geom ON parks USING GIST (geom);
CREATE INDEX IF NOT EXISTS idx_trails_geom ON trails USING GIST (geom);

-- LISTEN/NOTIFY triggers
CREATE OR REPLACE FUNCTION notify_tile_update() RETURNS trigger AS $$
DECLARE
    layer_name TEXT;
    payload JSON;
BEGIN
    layer_name := TG_ARGV[0];
    IF TG_OP = 'INSERT' OR TG_OP = 'UPDATE' THEN
        payload := json_build_object('layer', layer_name, 'id', NEW.id, 'op', TG_OP);
    ELSE
        payload := json_build_object('layer', layer_name, 'id', OLD.id, 'op', TG_OP);
    END IF;
    PERFORM pg_notify('tile_update', payload::text);
    RETURN COALESCE(NEW, OLD);
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER parks_notify AFTER INSERT OR UPDATE OR DELETE ON parks
    FOR EACH ROW EXECUTE FUNCTION notify_tile_update('parks');

CREATE TRIGGER trails_notify AFTER INSERT OR UPDATE OR DELETE ON trails
    FOR EACH ROW EXECUTE FUNCTION notify_tile_update('trails');
