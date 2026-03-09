-- Integration test: create test table with PostGIS data
CREATE EXTENSION IF NOT EXISTS postgis;

CREATE TABLE public.test_points (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    geom geometry(Point, 4326)
);

INSERT INTO test_points (name, description, geom) VALUES
    ('Point A', 'First test point', ST_SetSRID(ST_MakePoint(-122.4194, 37.7749), 4326)),
    ('Point B', 'Second test point', ST_SetSRID(ST_MakePoint(-73.9857, 40.7484), 4326)),
    ('Point C', 'Third test point', ST_SetSRID(ST_MakePoint(0.1278, 51.5074), 4326));

CREATE TABLE public.test_lines (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    geom geometry(LineString, 4326)
);

INSERT INTO test_lines (name, geom) VALUES
    ('Line A', ST_SetSRID(ST_MakeLine(ST_MakePoint(-122.4, 37.7), ST_MakePoint(-122.3, 37.8)), 4326));

-- Install notify trigger
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

CREATE TRIGGER test_points_notify
    AFTER INSERT OR UPDATE OR DELETE ON test_points
    FOR EACH ROW EXECUTE FUNCTION notify_tile_update('points');

CREATE TRIGGER test_lines_notify
    AFTER INSERT OR UPDATE OR DELETE ON test_lines
    FOR EACH ROW EXECUTE FUNCTION notify_tile_update('lines');
