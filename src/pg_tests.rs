//! Tests against a real PostGIS database.
//!
//! Every SQL statement tilefeed builds only fails at runtime, so these run the
//! real generation and `watch` paths and decode the tiles they write.
//!
//! Set `TILEFEED_TEST_PG` to libpq-style `key=value` pairs to run them:
//!
//! ```text
//! TILEFEED_TEST_PG="host=localhost port=5432 user=postgres dbname=postgres" cargo test pg_tests
//! ```
//!
//! `password` may be left out (a throwaway container with trust auth).
//! Without it they pass as skipped, unless `TILEFEED_REQUIRE_PG` is set (as in
//! CI), which turns a missing database into a failure (and
//! `TILEFEED_REQUIRE_TIPPECANOE` a missing `tippecanoe`). Each test works in its
//! own schema and layer names, so they run in parallel against one database.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tokio::sync::Mutex;

use crate::config::{load_config, AppConfig};
use crate::events::{create_event_bus, TileEvent};
use crate::generator::generate_source;
use crate::mbtiles::MbtilesStore;
use crate::mvt::{is_gzipped, vector_tile::tile::Value, vector_tile::Tile};
use crate::postgis::{Bounds, PostgisReader};
use crate::tiles::{tiles_for_bounds, TileCoord};
use crate::updater::{start_listener, UpdateOutputs};

/// The `[database]` table for the test database, or `None` to skip.
fn database_toml() -> Option<String> {
    let Ok(spec) = std::env::var("TILEFEED_TEST_PG") else {
        if std::env::var_os("TILEFEED_REQUIRE_PG").is_some() {
            panic!("TILEFEED_REQUIRE_PG is set but TILEFEED_TEST_PG is not");
        }
        eprintln!("skipped: set TILEFEED_TEST_PG to run PostGIS tests");
        return None;
    };
    let mut toml = String::from("[database]\n");
    for pair in spec.split_whitespace() {
        let (key, value) = pair
            .split_once('=')
            .unwrap_or_else(|| panic!("TILEFEED_TEST_PG: expected key=value, got {pair:?}"));
        match key {
            "port" | "pool_size" => toml.push_str(&format!("{key} = {value}\n")),
            _ => toml.push_str(&format!("{key} = {value:?}\n")),
        }
    }
    if !spec.split_whitespace().any(|p| p.starts_with("password=")) {
        toml.push_str("password = \"\"\n");
    }
    Some(toml)
}

/// A schema of its own, with the production notify trigger function installed.
struct TestDb {
    config: AppConfig,
    client: tokio_postgres::Client,
    dir: PathBuf,
}

impl TestDb {
    /// `sources` is the `[[sources]]` part of the config; `{dir}` in it is
    /// replaced by a fresh scratch directory. `sql` runs inside `schema`.
    async fn new(schema: &str, sql: &str, sources: &str) -> Option<Self> {
        let database = database_toml()?;
        let dir = std::env::temp_dir().join(format!("tilefeed-pg-{schema}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let toml = format!(
            "{database}\n[updates]\ndebounce_ms = 50\n\n{}",
            sources.replace("{dir}", dir.to_str().unwrap())
        );
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        let config = load_config(path.to_str().unwrap()).expect("test config");

        let tls = crate::pgtls::make_tls_connector(&config.database).unwrap();
        let (client, connection) = config
            .database
            .pg_config()
            .connect(tls)
            .await
            .expect("connect to TILEFEED_TEST_PG");
        tokio::spawn(connection);

        // The function is shared by every test schema; installing it
        // concurrently fails with "tuple concurrently updated" without a lock.
        let notify_sql = include_str!("../sql/setup_notify.sql");
        let function = notify_sql
            .split("-- Example")
            .next()
            .expect("notify function");
        client
            .batch_execute(&format!(
                "BEGIN;
                 SELECT pg_advisory_xact_lock(7417);
                 CREATE EXTENSION IF NOT EXISTS postgis;
                 SET LOCAL search_path = public;
                 {function}
                 COMMIT;
                 DROP SCHEMA IF EXISTS {schema} CASCADE;
                 CREATE SCHEMA {schema};
                 SET search_path = {schema}, public;
                 {sql}"
            ))
            .await
            .expect("set up test schema");

        Some(Self {
            config,
            client,
            dir,
        })
    }

    async fn reader(&self) -> PostgisReader {
        PostgisReader::connect(&self.config.database).await.unwrap()
    }

    async fn generate(&self) {
        let reader = self.reader().await;
        for source in &self.config.sources {
            generate_source(source, &reader, None, None)
                .await
                .unwrap_or_else(|e| panic!("generate '{}': {e:#}", source.name));
        }
    }

    fn store(&self, source: usize) -> MbtilesStore {
        MbtilesStore::open(&self.config.sources[source].mbtiles_path).unwrap()
    }

    async fn exec(&self, sql: &str) {
        self.client.batch_execute(sql).await.unwrap();
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Props = BTreeMap<String, String>;

/// Decode a tile into `layer name -> features' properties`.
fn decode(data: &[u8]) -> BTreeMap<String, Vec<Props>> {
    let mut raw = Vec::new();
    if is_gzipped(data) {
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut raw)
            .unwrap();
    } else {
        raw.extend_from_slice(data);
    }
    let tile = Tile::decode(raw.as_slice()).expect("valid MVT");
    tile.layers
        .into_iter()
        .map(|layer| {
            let features = layer
                .features
                .iter()
                .map(|f| {
                    f.tags
                        .chunks(2)
                        .map(|kv| {
                            let key = layer.keys[kv[0] as usize].clone();
                            (key, value_string(&layer.values[kv[1] as usize]))
                        })
                        .collect()
                })
                .collect();
            (layer.name, features)
        })
        .collect()
}

fn value_string(v: &Value) -> String {
    if let Some(s) = &v.string_value {
        return s.clone();
    }
    v.int_value
        .map(|i| i.to_string())
        .or(v.sint_value.map(|i| i.to_string()))
        .or(v.uint_value.map(|i| i.to_string()))
        .or(v.double_value.map(|d| d.to_string()))
        .or(v.float_value.map(|d| d.to_string()))
        .or(v.bool_value.map(|b| b.to_string()))
        .unwrap_or_default()
}

/// The tile containing `(lon, lat)` at zoom `z`.
fn tile_at(lon: f64, lat: f64, z: u8) -> TileCoord {
    let point = Bounds {
        min_lon: lon,
        min_lat: lat,
        max_lon: lon,
        max_lat: lat,
    };
    tiles_for_bounds(&point, z, z).remove(0)
}

fn layers_at(store: &MbtilesStore, lon: f64, lat: f64, z: u8) -> BTreeMap<String, Vec<Props>> {
    let t = tile_at(lon, lat, z);
    match store.get_tile(t.z, t.x, t.y).unwrap() {
        Some(data) => decode(&data),
        None => BTreeMap::new(),
    }
}

fn keys(features: &[Props]) -> BTreeSet<&str> {
    features
        .iter()
        .flat_map(|p| p.keys().map(String::as_str))
        .collect()
}

const PARIS: (f64, f64) = (2.305, 48.855);
const TOKYO: (f64, f64) = (139.705, 35.655);

/// A polygon around `(lon, lat)`, `d` degrees on each side.
fn square(lon: f64, lat: f64, d: f64) -> String {
    format!(
        "ST_MakeEnvelope({}, {}, {}, {}, 4326)",
        lon - d,
        lat - d,
        lon + d,
        lat + d
    )
}

#[tokio::test]
async fn native_without_properties_takes_every_non_spatial_column() {
    let Some(db) = TestDb::new(
        "it_native_all",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, parks text, area int,
                geom geometry(Polygon, 4326), centre geometry(Point, 4326), g2 geography);
             INSERT INTO parks (parks, area, geom, centre)
             VALUES ('Luxembourg', 5, {}, ST_Point(2.305, 48.855, 4326));",
            square(PARIS.0, PARIS.1, 0.005)
        ),
        r#"
[[sources]]
name = "it_native_all"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 10
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_na_parks"
schema = "it_native_all"
table = "parks"
generate_label_points = true
generate_boundary_lines = true
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    let store = db.store(0);
    for z in 10..=12 {
        let layers = layers_at(&store, PARIS.0, PARIS.1, z);
        for name in ["it_na_parks", "it_na_parks_labels", "it_na_parks_boundary"] {
            let features = layers
                .get(name)
                .unwrap_or_else(|| panic!("z{z}: no {name} layer in {layers:?}"));
            assert_eq!(features.len(), 1, "z{z} {name}");
            // A column named like its table must not replace the whole row,
            // and spatial columns must not leak in as hex WKB.
            assert_eq!(
                keys(features),
                BTreeSet::from(["area", "id", "parks"]),
                "z{z} {name}"
            );
            assert_eq!(features[0]["parks"], "Luxembourg");
        }
    }
}

#[tokio::test]
async fn native_with_properties_takes_only_those_and_honours_filter() {
    let Some(db) = TestDb::new(
        "it_native_list",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, name text, area int, secret text,
                geom geometry(Polygon, 4326));
             INSERT INTO parks (name, area, secret, geom) VALUES
                ('kept', 5, 's', {}), ('filtered', 1, 's', {});",
            square(PARIS.0, PARIS.1, 0.002),
            square(PARIS.0 + 0.003, PARIS.1, 0.001)
        ),
        r#"
[[sources]]
name = "it_native_list"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 12
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_nl_parks"
schema = "it_native_list"
table = "parks"
properties = ["name", "area"]
filter = "area > 2"
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    // The filtered extent is recorded as MBTiles `bounds`, as Tippecanoe does.
    let [w, s, e, n] = db.store(0).bounds().unwrap().expect("bounds metadata");
    let expect = [
        PARIS.0 - 0.002,
        PARIS.1 - 0.002,
        PARIS.0 + 0.002,
        PARIS.1 + 0.002,
    ];
    for (got, want) in [w, s, e, n].iter().zip(expect) {
        assert!((got - want).abs() < 1e-9, "bounds {:?}", [w, s, e, n]);
    }
    let layers = layers_at(&db.store(0), PARIS.0, PARIS.1, 12);
    let features = &layers["it_nl_parks"];
    assert_eq!(features.len(), 1, "{features:?}");
    assert_eq!(keys(features), BTreeSet::from(["area", "name"]));
    assert_eq!(features[0]["name"], "kept");
}

#[tokio::test]
async fn native_skips_a_layer_above_its_min_zoom() {
    let Some(db) = TestDb::new(
        "it_native_zoom",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, geom geometry(Polygon, 4326));
             CREATE TABLE trees (id serial PRIMARY KEY, geom geometry(Point, 4326));
             INSERT INTO parks (geom) VALUES ({});
             INSERT INTO trees (geom) VALUES (ST_Point(139.705, 35.655, 4326));",
            square(PARIS.0, PARIS.1, 0.002)
        ),
        r#"
[[sources]]
name = "it_native_zoom"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 8
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_nz_parks"
schema = "it_native_zoom"
table = "parks"

[[sources.layers]]
name = "it_nz_trees"
schema = "it_native_zoom"
table = "trees"
min_zoom = 11
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    let store = db.store(0);
    // Trees are far from every park, so their tiles at z11+ are only found
    // through the EXISTS probe at the zooms above the layer's min_zoom.
    for z in 8..=10 {
        assert!(
            layers_at(&store, TOKYO.0, TOKYO.1, z).is_empty(),
            "z{z} trees drawn above the layer's min_zoom"
        );
    }
    for z in 11..=12 {
        let layers = layers_at(&store, TOKYO.0, TOKYO.1, z);
        assert_eq!(layers["it_nz_trees"].len(), 1, "z{z}: {layers:?}");
    }
    assert_eq!(
        layers_at(&store, PARIS.0, PARIS.1, 8)["it_nz_parks"].len(),
        1
    );
}

#[tokio::test]
async fn tippecanoe_export_works_with_and_without_properties() {
    if crate::generator::check_binary("tippecanoe", None).is_err() {
        if std::env::var_os("TILEFEED_REQUIRE_TIPPECANOE").is_some() {
            panic!("TILEFEED_REQUIRE_TIPPECANOE is set but tippecanoe is not on PATH");
        }
        eprintln!("skipped: tippecanoe not on PATH");
        return;
    }
    let Some(db) = TestDb::new(
        "it_tippecanoe",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, name text, area int,
                geom geometry(Polygon, 4326), centre geometry(Point, 4326));
             INSERT INTO parks (name, area, geom) VALUES ('Luxembourg', 5, {});",
            square(PARIS.0, PARIS.1, 0.005)
        ),
        r#"
[[sources]]
name = "it_tippecanoe"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 10
max_zoom = 12

[[sources.layers]]
name = "it_tc_all"
schema = "it_tippecanoe"
table = "parks"
generate_label_points = true

[[sources.layers]]
name = "it_tc_listed"
schema = "it_tippecanoe"
table = "parks"
properties = ["name"]
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    let layers = layers_at(&db.store(0), PARIS.0, PARIS.1, 12);
    assert_eq!(
        keys(&layers["it_tc_all"]),
        BTreeSet::from(["area", "id", "name"])
    );
    assert_eq!(
        keys(&layers["it_tc_all_labels"]),
        BTreeSet::from(["area", "id", "name"])
    );
    assert_eq!(keys(&layers["it_tc_listed"]), BTreeSet::from(["name"]));
}

type EventRx = tokio::sync::broadcast::Receiver<crate::events::Sequenced>;

/// Run the production listener on the test config's first source.
async fn start_watch(db: &TestDb) -> (EventRx, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let mut stores = HashMap::new();
    stores.insert(
        db.config.sources[0].name.clone(),
        Arc::new(Mutex::new(db.store(0))),
    );
    let bus = create_event_bus();
    let rx = bus.subscribe();
    let listener = tokio::spawn(start_listener(
        Arc::new(db.config.clone()),
        stores,
        UpdateOutputs {
            event_tx: Some(bus),
            ..Default::default()
        },
    ));
    (rx, listener)
}

/// LISTEN is set up asynchronously: repeat `touch` (a write that fires the
/// trigger) until the listener is there to see it.
async fn first_update(
    db: &TestDb,
    rx: &mut EventRx,
    touch: &str,
) -> Vec<crate::events::ChangedArea> {
    let source = db.config.sources[0].name.clone();
    loop {
        db.exec(touch).await;
        if let Ok(areas) =
            tokio::time::timeout(Duration::from_secs(2), next_update(rx, &source)).await
        {
            return areas;
        }
    }
}

/// The `areas` of the next update to `want`. Other tests' notifications
/// reach every listener too, but name layers it doesn't know.
async fn next_update(rx: &mut EventRx, want: &str) -> Vec<crate::events::ChangedArea> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("no update within 20s")
            .expect("event bus closed")
            .event;
        if let TileEvent::UpdateComplete { source, areas, .. } = event {
            if source == want {
                return areas;
            }
        }
    }
}

#[tokio::test]
async fn watch_applies_insert_update_and_delete() {
    let Some(db) = TestDb::new(
        "it_watch",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, name text,
                geom geometry(Polygon, 4326));
             INSERT INTO parks (name, geom) VALUES ('Luxembourg', {});
             CREATE TRIGGER tile_update_trigger AFTER INSERT OR UPDATE OR DELETE ON parks
                FOR EACH ROW EXECUTE FUNCTION public.notify_tile_update('it_w_parks');",
            square(PARIS.0, PARIS.1, 0.002)
        ),
        r#"
[[sources]]
name = "it_watch"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 10
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_w_parks"
schema = "it_watch"
table = "parks"
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    let (mut rx, listener) = start_watch(&db).await;
    let areas = first_update(
        &db,
        &mut rx,
        "UPDATE parks SET name = 'Luxembourg' WHERE id = 1",
    )
    .await;
    assert!(!areas.is_empty());

    // Move the park to Tokyo and rename it: both places are redrawn.
    db.exec(&format!(
        "UPDATE parks SET name = 'Yoyogi', geom = {} WHERE id = 1",
        square(TOKYO.0, TOKYO.1, 0.002)
    ))
    .await;
    let areas = next_update(&mut rx, "it_watch").await;
    let covers = |(lon, lat): (f64, f64)| {
        areas
            .iter()
            .any(|a| a.bbox[0] <= lon && lon <= a.bbox[2] && a.bbox[1] <= lat && lat <= a.bbox[3])
    };
    assert!(covers(PARIS) && covers(TOKYO), "areas {areas:?}");
    let store = db.store(0);
    // `bounds` grew from Paris to cover Tokyo as well
    let [w, s, e, n] = store.bounds().unwrap().unwrap();
    assert!(
        w <= PARIS.0 && e >= TOKYO.0 && s <= TOKYO.1 && n >= PARIS.1,
        "bounds {:?}",
        [w, s, e, n]
    );
    for z in 10..=12 {
        let tokyo = layers_at(&store, TOKYO.0, TOKYO.1, z);
        assert_eq!(tokyo["it_w_parks"][0]["name"], "Yoyogi", "z{z}");
        let paris = layers_at(&store, PARIS.0, PARIS.1, z);
        assert!(
            paris.get("it_w_parks").is_none_or(|f| f.is_empty()),
            "z{z}: park left behind in Paris: {paris:?}"
        );
    }

    db.exec(&format!(
        "INSERT INTO parks (name, geom) VALUES ('Tuileries', {})",
        square(PARIS.0, PARIS.1, 0.001)
    ))
    .await;
    next_update(&mut rx, "it_watch").await;
    assert_eq!(
        layers_at(&store, PARIS.0, PARIS.1, 12)["it_w_parks"][0]["name"],
        "Tuileries"
    );

    db.exec("DELETE FROM parks WHERE id = 1").await;
    next_update(&mut rx, "it_watch").await;
    let tokyo = layers_at(&store, TOKYO.0, TOKYO.1, 12);
    assert!(
        tokyo.get("it_w_parks").is_none_or(|f| f.is_empty()),
        "deleted park still drawn: {tokyo:?}"
    );

    listener.abort();
}

/// One statement that changes many rows arrives as one batch; every row's new
/// place is found (in one query per layer) and redrawn.
#[tokio::test]
async fn watch_redraws_every_row_of_a_multi_row_update() {
    let Some(db) = TestDb::new(
        "it_batch",
        &format!(
            "CREATE TABLE parks (id serial PRIMARY KEY, name text, open boolean NOT NULL DEFAULT true,
                geom geometry(Polygon, 4326));
             INSERT INTO parks (name, geom)
                SELECT 'park ' || i, ST_Translate({}, (i % 10) * 0.001, (i / 10) * 0.001)
                FROM generate_series(0, 49) AS i;
             CREATE TRIGGER tile_update_trigger AFTER INSERT OR UPDATE OR DELETE ON parks
                FOR EACH ROW EXECUTE FUNCTION public.notify_tile_update('it_b_parks');",
            square(PARIS.0, PARIS.1, 0.0005)
        ),
        r#"
[[sources]]
name = "it_batch"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 12
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_b_parks"
schema = "it_batch"
table = "parks"
filter = "open"
"#,
    )
    .await
    else {
        return;
    };

    db.generate().await;
    let (mut rx, listener) = start_watch(&db).await;
    first_update(&db, &mut rx, "UPDATE parks SET name = name WHERE id = 1").await;

    // Move all 50 to Tokyo in one statement; close one of them on the way.
    db.exec(&format!(
        "UPDATE parks SET geom = ST_Translate(geom, {dx}, {dy}), open = (id <> 7)",
        dx = TOKYO.0 - PARIS.0,
        dy = TOKYO.1 - PARIS.1
    ))
    .await;
    next_update(&mut rx, "it_batch").await;
    let store = db.store(0);
    let tokyo = layers_at(&store, TOKYO.0, TOKYO.1, 12);
    assert_eq!(
        tokyo["it_b_parks"].len(),
        49,
        "the closed park is filtered out"
    );
    let paris = layers_at(&store, PARIS.0, PARIS.1, 12);
    assert!(
        paris.get("it_b_parks").is_none_or(|f| f.is_empty()),
        "parks left behind in Paris: {paris:?}"
    );

    listener.abort();
}

#[tokio::test]
async fn validate_reports_missing_columns_and_trigger() {
    let Some(db) = TestDb::new(
        "it_validate",
        "CREATE TABLE parks (id serial PRIMARY KEY, name text, geom geometry(Polygon, 4326));",
        r#"
[[sources]]
name = "it_validate"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 0
max_zoom = 4

[[sources.layers]]
name = "it_v_parks"
schema = "it_validate"
table = "parks"
properties = ["name", "missing"]
"#,
    )
    .await
    else {
        return;
    };

    let reader = db.reader().await;
    let layer = &db.config.sources[0].layers[0];
    let issues = reader.validate_layer(layer).await.unwrap();
    assert!(
        issues.iter().any(|i| i.contains("\"missing\"")),
        "{issues:?}"
    );
    assert!(issues.iter().any(|i| i.contains("trigger")), "{issues:?}");
    assert!(!issues.iter().any(|i| i.contains("\"name\"")), "{issues:?}");

    db.exec(
        "CREATE TRIGGER tile_update_trigger AFTER INSERT OR UPDATE OR DELETE ON parks
            FOR EACH ROW EXECUTE FUNCTION public.notify_tile_update('it_v_parks');",
    )
    .await;
    let issues = reader.validate_layer(layer).await.unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
}

/// Paris in EPSG:3857 metres, stored with SRID 0 the way OGR_FDW and
/// `shp2pgsql` without `-s` leave geometries.
fn srid0_square(lon: f64, lat: f64, d: f64) -> String {
    format!("ST_SetSRID(ST_Transform({}, 3857), 0)", square(lon, lat, d))
}

#[tokio::test]
async fn srid_zero_geometries_are_read_as_the_layer_srid() {
    let Some(db) = TestDb::new(
        "it_srid0",
        &format!(
            "CREATE TABLE parcels (id serial PRIMARY KEY, name text, geom geometry);
             INSERT INTO parcels (name, geom) VALUES ('Luxembourg', {});
             CREATE TRIGGER tile_update_trigger AFTER INSERT OR UPDATE OR DELETE ON parcels
                FOR EACH ROW EXECUTE FUNCTION public.notify_tile_update('it_s0_parcels', '3857');",
            srid0_square(PARIS.0, PARIS.1, 0.002)
        ),
        r#"
[[sources]]
name = "it_srid0"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 10
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_s0_parcels"
schema = "it_srid0"
table = "parcels"
srid = 3857
generate_label_points = true
"#,
    )
    .await
    else {
        return;
    };

    // The metres were reprojected: the parcel lands in Paris's tiles.
    db.generate().await;
    let store = db.store(0);
    for z in 10..=12 {
        let layers = layers_at(&store, PARIS.0, PARIS.1, z);
        assert_eq!(layers["it_s0_parcels"][0]["name"], "Luxembourg", "z{z}");
        assert_eq!(layers["it_s0_parcels_labels"].len(), 1, "z{z}");
    }

    // The trigger's second argument gives the old position, so a move
    // clears the old tiles as well as drawing the new ones.
    let (mut rx, listener) = start_watch(&db).await;
    first_update(
        &db,
        &mut rx,
        "UPDATE parcels SET name = 'Luxembourg' WHERE id = 1",
    )
    .await;
    db.exec(&format!(
        "UPDATE parcels SET name = 'Yoyogi', geom = {} WHERE id = 1",
        srid0_square(TOKYO.0, TOKYO.1, 0.002)
    ))
    .await;
    next_update(&mut rx, "it_srid0").await;
    let tokyo = layers_at(&store, TOKYO.0, TOKYO.1, 12);
    assert_eq!(tokyo["it_s0_parcels"][0]["name"], "Yoyogi");
    let paris = layers_at(&store, PARIS.0, PARIS.1, 12);
    assert!(
        paris.get("it_s0_parcels").is_none_or(|f| f.is_empty()),
        "parcel left behind in Paris: {paris:?}"
    );
    listener.abort();

    // `srid` only applies to geometries without one: a row stored with
    // EPSG:4326 next to the SRID-0 ones is still read as 4326.
    db.exec(&format!(
        "INSERT INTO parcels (name, geom) VALUES ('Tuileries', {})",
        square(PARIS.0, PARIS.1, 0.001)
    ))
    .await;
    db.generate().await;
    let paris = layers_at(&db.store(0), PARIS.0, PARIS.1, 12);
    assert_eq!(paris["it_s0_parcels"][0]["name"], "Tuileries");
}

#[tokio::test]
async fn srid_zero_without_layer_srid_is_reported_and_writes_still_succeed() {
    let Some(db) = TestDb::new(
        "it_srid0_unset",
        &format!(
            "CREATE TABLE parcels (id serial PRIMARY KEY, geom geometry);
             INSERT INTO parcels (geom) VALUES ({});
             CREATE TRIGGER tile_update_trigger AFTER INSERT OR UPDATE OR DELETE ON parcels
                FOR EACH ROW EXECUTE FUNCTION public.notify_tile_update('it_s0u_parcels');",
            srid0_square(PARIS.0, PARIS.1, 0.002)
        ),
        r#"
[[sources]]
name = "it_srid0_unset"
mbtiles_path = "{dir}/out.mbtiles"
min_zoom = 10
max_zoom = 12
generation_backend = "native"

[[sources.layers]]
name = "it_s0u_parcels"
schema = "it_srid0_unset"
table = "parcels"
"#,
    )
    .await
    else {
        return;
    };

    let issues = db
        .reader()
        .await
        .validate_layer(&db.config.sources[0].layers[0])
        .await
        .unwrap();
    assert!(
        issues
            .iter()
            .any(|i| i.contains("without an SRID") && i.contains("`srid`")),
        "{issues:?}"
    );
    // The trigger can't place the old geometry, but must not fail the write.
    db.exec("UPDATE parcels SET geom = geom WHERE id = 1; DELETE FROM parcels WHERE id = 1")
        .await;
}
