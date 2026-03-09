use anyhow::Result;
use flate2::write::GzEncoder;
use flate2::Compression;
use geo::{LineString, Polygon, Simplify};
use prost::Message;
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use crate::config::LayerConfig;
use crate::postgis::FeatureData;
use crate::tiles::TileCoord;

pub mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

use vector_tile::tile::{Feature, GeomType, Layer, Value};
use vector_tile::Tile;

const EXTENT: u32 = 4096;

/// Encode a set of features into a gzipped MVT tile
pub fn encode_tile(
    tile_coord: &TileCoord,
    features_by_layer: &HashMap<String, Vec<FeatureData>>,
) -> Result<Vec<u8>> {
    encode_tile_with_config(tile_coord, features_by_layer, &HashMap::new())
}

/// Encode a set of features into a gzipped MVT tile with layer-specific config
/// for geometry simplification and property filtering
pub fn encode_tile_with_config(
    tile_coord: &TileCoord,
    features_by_layer: &HashMap<String, Vec<FeatureData>>,
    layer_configs: &HashMap<String, &LayerConfig>,
) -> Result<Vec<u8>> {
    let mut layers = Vec::new();

    for (layer_name, features) in features_by_layer {
        let layer_cfg = layer_configs.get(layer_name.as_str()).copied();

        // Compute excluded properties for this zoom level
        let excluded_props = compute_excluded_properties(layer_cfg, tile_coord.z);

        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        let mut key_index: HashMap<String, u32> = HashMap::new();
        let mut value_index: HashMap<String, u32> = HashMap::new();
        let mut mvt_features: Vec<Feature> = Vec::new();

        for feature in features {
            // Apply geometry simplification if configured
            let geometry = if let Some(tolerance) = simplification_tolerance(layer_cfg, tile_coord.z) {
                let simplified = simplify_geometry(&feature.geometry, tolerance);
                encode_geometry(&simplified, tile_coord)?
            } else {
                encode_geometry(&feature.geometry, tile_coord)?
            };

            let geom_type = detect_geom_type(&feature.geometry);

            if geometry.is_empty() {
                continue;
            }

            let mut tags = Vec::new();
            if let Some(props) = feature.properties.as_object() {
                for (k, v) in props {
                    // Skip excluded properties
                    if excluded_props.contains(k.as_str()) {
                        continue;
                    }

                    let key_idx = *key_index.entry(k.clone()).or_insert_with(|| {
                        keys.push(k.clone());
                        (keys.len() - 1) as u32
                    });

                    let value_str = value_to_string(v);
                    let val_idx = *value_index.entry(value_str.clone()).or_insert_with(|| {
                        values.push(json_to_mvt_value(v));
                        (values.len() - 1) as u32
                    });

                    tags.push(key_idx);
                    tags.push(val_idx);
                }
            }

            mvt_features.push(Feature {
                id: Some(feature.id as u64),
                tags,
                r#type: Some(geom_type as i32),
                geometry,
            });
        }

        if !mvt_features.is_empty() {
            layers.push(Layer {
                version: 2,
                name: layer_name.clone(),
                features: mvt_features,
                keys,
                values,
                extent: Some(EXTENT),
            });
        }
    }

    let tile = Tile { layers };

    // Encode to protobuf
    let mut buf = Vec::new();
    tile.encode(&mut buf)?;

    // Gzip compress
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&buf)?;
    let compressed = encoder.finish()?;

    Ok(compressed)
}

/// Compute the simplification tolerance for a given zoom level
fn simplification_tolerance(layer_cfg: Option<&LayerConfig>, zoom: u8) -> Option<f64> {
    let cfg = layer_cfg?;
    let base_tolerance = cfg.simplify_tolerance?;
    // Scale tolerance: more simplification at lower zooms
    // At max_zoom, tolerance is the base value; at lower zooms it's larger
    Some(base_tolerance * (1 << (18u8.saturating_sub(zoom))) as f64)
}

/// Compute the set of property names to exclude at the given zoom level
fn compute_excluded_properties<'a>(layer_cfg: Option<&'a LayerConfig>, zoom: u8) -> HashSet<&'a str> {
    let mut excluded = HashSet::new();
    if let Some(cfg) = layer_cfg {
        if let Some(rules) = &cfg.property_rules {
            for rule in rules {
                if zoom < rule.below_zoom {
                    for prop in &rule.exclude {
                        excluded.insert(prop.as_str());
                    }
                }
            }
        }
    }
    excluded
}

/// Simplify a GeoJSON geometry using Douglas-Peucker algorithm
fn simplify_geometry(geometry: &JsonValue, tolerance: f64) -> JsonValue {
    let geom_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let coords = &geometry["coordinates"];

    match geom_type {
        "LineString" => {
            if let Some(ls) = parse_linestring(coords) {
                let simplified = ls.simplify(&tolerance);
                let new_coords: Vec<JsonValue> = simplified
                    .into_inner()
                    .into_iter()
                    .map(|c| serde_json::json!([c.x, c.y]))
                    .collect();
                serde_json::json!({"type": "LineString", "coordinates": new_coords})
            } else {
                geometry.clone()
            }
        }
        "MultiLineString" => {
            if let Some(lines) = coords.as_array() {
                let new_lines: Vec<JsonValue> = lines
                    .iter()
                    .filter_map(|l| parse_linestring(l))
                    .map(|ls| {
                        let simplified = ls.simplify(&tolerance);
                        simplified
                            .into_inner()
                            .into_iter()
                            .map(|c| serde_json::json!([c.x, c.y]))
                            .collect::<Vec<_>>()
                    })
                    .map(|coords| JsonValue::Array(coords))
                    .collect();
                serde_json::json!({"type": "MultiLineString", "coordinates": new_lines})
            } else {
                geometry.clone()
            }
        }
        "Polygon" => {
            if let Some(poly) = parse_polygon(coords) {
                let simplified = poly.simplify(&tolerance);
                let new_coords = polygon_to_json(&simplified);
                serde_json::json!({"type": "Polygon", "coordinates": new_coords})
            } else {
                geometry.clone()
            }
        }
        "MultiPolygon" => {
            if let Some(polygons) = coords.as_array() {
                let new_polys: Vec<JsonValue> = polygons
                    .iter()
                    .filter_map(|p| parse_polygon(p))
                    .map(|poly| {
                        let simplified = poly.simplify(&tolerance);
                        JsonValue::Array(polygon_to_json(&simplified))
                    })
                    .collect();
                serde_json::json!({"type": "MultiPolygon", "coordinates": new_polys})
            } else {
                geometry.clone()
            }
        }
        // Points don't need simplification
        _ => geometry.clone(),
    }
}

fn parse_linestring(coords: &JsonValue) -> Option<LineString<f64>> {
    let points = coords.as_array()?;
    let coords: Vec<geo_types::Coord<f64>> = points
        .iter()
        .filter_map(|p| {
            Some(geo_types::Coord {
                x: p[0].as_f64()?,
                y: p[1].as_f64()?,
            })
        })
        .collect();
    if coords.len() >= 2 {
        Some(LineString::new(coords))
    } else {
        None
    }
}

fn parse_polygon(coords: &JsonValue) -> Option<Polygon<f64>> {
    let rings = coords.as_array()?;
    if rings.is_empty() {
        return None;
    }

    let exterior = parse_linestring(&rings[0])?;
    let interiors: Vec<LineString<f64>> = rings[1..]
        .iter()
        .filter_map(|r| parse_linestring(r))
        .collect();

    Some(Polygon::new(exterior, interiors))
}

fn polygon_to_json(poly: &Polygon<f64>) -> Vec<JsonValue> {
    let mut rings = Vec::new();

    let exterior: Vec<JsonValue> = poly
        .exterior()
        .0
        .iter()
        .map(|c| serde_json::json!([c.x, c.y]))
        .collect();
    rings.push(JsonValue::Array(exterior));

    for interior in poly.interiors() {
        let ring: Vec<JsonValue> = interior
            .0
            .iter()
            .map(|c| serde_json::json!([c.x, c.y]))
            .collect();
        rings.push(JsonValue::Array(ring));
    }

    rings
}

fn detect_geom_type(geometry: &JsonValue) -> GeomType {
    match geometry.get("type").and_then(|t| t.as_str()) {
        Some("Point") | Some("MultiPoint") => GeomType::Point,
        Some("LineString") | Some("MultiLineString") => GeomType::Linestring,
        Some("Polygon") | Some("MultiPolygon") => GeomType::Polygon,
        _ => GeomType::Unknown,
    }
}

fn encode_geometry(geometry: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let geom_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let coords = &geometry["coordinates"];

    match geom_type {
        "Point" => encode_point(coords, tile_coord),
        "MultiPoint" => encode_multi_point(coords, tile_coord),
        "LineString" => encode_linestring(coords, tile_coord),
        "MultiLineString" => encode_multi_linestring(coords, tile_coord),
        "Polygon" => encode_polygon(coords, tile_coord),
        "MultiPolygon" => encode_multi_polygon(coords, tile_coord),
        _ => Ok(Vec::new()),
    }
}

fn to_tile_xy(lon: f64, lat: f64, tile_coord: &TileCoord) -> (i32, i32) {
    crate::tiles::world_to_tile_coords(lon, lat, tile_coord, EXTENT)
}

fn encode_point(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let lon = coords[0].as_f64().unwrap_or(0.0);
    let lat = coords[1].as_f64().unwrap_or(0.0);
    let (x, y) = to_tile_xy(lon, lat, tile_coord);

    Ok(vec![command(1, 1), zigzag(x), zigzag(y)])
}

fn encode_multi_point(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let points = coords.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    if points.is_empty() {
        return Ok(Vec::new());
    }

    let mut cmds = Vec::new();
    cmds.push(command(1, points.len() as u32)); // MoveTo, count=N

    let mut prev_x = 0i32;
    let mut prev_y = 0i32;

    for pt in points {
        let lon = pt[0].as_f64().unwrap_or(0.0);
        let lat = pt[1].as_f64().unwrap_or(0.0);
        let (x, y) = to_tile_xy(lon, lat, tile_coord);
        cmds.push(zigzag(x - prev_x));
        cmds.push(zigzag(y - prev_y));
        prev_x = x;
        prev_y = y;
    }

    Ok(cmds)
}

fn encode_linestring(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    encode_ring(coords, tile_coord, false)
}

fn encode_multi_linestring(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let lines = coords.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let mut cmds = Vec::new();
    for line in lines {
        cmds.extend(encode_ring(line, tile_coord, false)?);
    }
    Ok(cmds)
}

fn encode_polygon(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let rings = coords.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let mut cmds = Vec::new();
    for ring in rings {
        cmds.extend(encode_ring(ring, tile_coord, true)?);
    }
    Ok(cmds)
}

fn encode_multi_polygon(coords: &JsonValue, tile_coord: &TileCoord) -> Result<Vec<u32>> {
    let polygons = coords.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let mut cmds = Vec::new();
    for polygon in polygons {
        let rings = polygon.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        for ring in rings {
            cmds.extend(encode_ring(ring, tile_coord, true)?);
        }
    }
    Ok(cmds)
}

fn encode_ring(coords: &JsonValue, tile_coord: &TileCoord, close: bool) -> Result<Vec<u32>> {
    let points = coords.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    if points.len() < 2 {
        return Ok(Vec::new());
    }

    let mut cmds = Vec::new();

    // MoveTo first point
    let lon = points[0][0].as_f64().unwrap_or(0.0);
    let lat = points[0][1].as_f64().unwrap_or(0.0);
    let (x, y) = to_tile_xy(lon, lat, tile_coord);

    cmds.push(command(1, 1)); // MoveTo
    cmds.push(zigzag(x));
    cmds.push(zigzag(y));

    let mut prev_x = x;
    let mut prev_y = y;

    // LineTo remaining points (skip last if closing, since ClosePath handles it)
    let end = if close {
        points.len() - 1
    } else {
        points.len()
    };
    let line_to_count = end - 1;

    if line_to_count > 0 {
        cmds.push(command(2, line_to_count as u32)); // LineTo

        for pt in &points[1..end] {
            let lon = pt[0].as_f64().unwrap_or(0.0);
            let lat = pt[1].as_f64().unwrap_or(0.0);
            let (x, y) = to_tile_xy(lon, lat, tile_coord);
            cmds.push(zigzag(x - prev_x));
            cmds.push(zigzag(y - prev_y));
            prev_x = x;
            prev_y = y;
        }
    }

    if close {
        cmds.push(command(7, 1)); // ClosePath
    }

    Ok(cmds)
}

/// MVT command encoding: id | count
fn command(id: u32, count: u32) -> u32 {
    (id & 0x7) | (count << 3)
}

/// ZigZag encoding for signed integers
fn zigzag(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

fn value_to_string(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => format!("s:{}", s),
        JsonValue::Number(n) => format!("n:{}", n),
        JsonValue::Bool(b) => format!("b:{}", b),
        _ => format!("o:{}", v),
    }
}

fn json_to_mvt_value(v: &JsonValue) -> Value {
    match v {
        JsonValue::String(s) => Value {
            string_value: Some(s.clone()),
            ..Default::default()
        },
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value {
                    int_value: Some(i),
                    ..Default::default()
                }
            } else if let Some(f) = n.as_f64() {
                Value {
                    double_value: Some(f),
                    ..Default::default()
                }
            } else {
                Value {
                    string_value: Some(n.to_string()),
                    ..Default::default()
                }
            }
        }
        JsonValue::Bool(b) => Value {
            bool_value: Some(*b),
            ..Default::default()
        },
        _ => Value {
            string_value: Some(v.to_string()),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- zigzag tests ---

    #[test]
    fn test_zigzag_zero() {
        assert_eq!(zigzag(0), 0);
    }

    #[test]
    fn test_zigzag_positive() {
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(2), 4);
        assert_eq!(zigzag(100), 200);
    }

    #[test]
    fn test_zigzag_negative() {
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(-2), 3);
        assert_eq!(zigzag(-100), 199);
    }

    #[test]
    fn test_zigzag_large_values() {
        assert_eq!(zigzag(i32::MAX), u32::MAX - 1);
        assert_eq!(zigzag(i32::MIN), u32::MAX);
    }

    // --- command tests ---

    #[test]
    fn test_command_moveto() {
        // MoveTo (id=1), count=1
        let cmd = command(1, 1);
        assert_eq!(cmd & 0x7, 1); // id
        assert_eq!(cmd >> 3, 1); // count
    }

    #[test]
    fn test_command_lineto() {
        // LineTo (id=2), count=5
        let cmd = command(2, 5);
        assert_eq!(cmd & 0x7, 2);
        assert_eq!(cmd >> 3, 5);
    }

    #[test]
    fn test_command_closepath() {
        // ClosePath (id=7), count=1
        let cmd = command(7, 1);
        assert_eq!(cmd & 0x7, 7);
        assert_eq!(cmd >> 3, 1);
    }

    // --- detect_geom_type tests ---

    #[test]
    fn test_detect_geom_type_point() {
        let geom = json!({"type": "Point", "coordinates": [0.0, 0.0]});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Point as i32);
    }

    #[test]
    fn test_detect_geom_type_multipoint() {
        let geom = json!({"type": "MultiPoint", "coordinates": [[0.0, 0.0]]});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Point as i32);
    }

    #[test]
    fn test_detect_geom_type_linestring() {
        let geom = json!({"type": "LineString", "coordinates": [[0.0, 0.0], [1.0, 1.0]]});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Linestring as i32);
    }

    #[test]
    fn test_detect_geom_type_polygon() {
        let geom = json!({"type": "Polygon", "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Polygon as i32);
    }

    #[test]
    fn test_detect_geom_type_unknown() {
        let geom = json!({"type": "GeometryCollection"});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Unknown as i32);
    }

    #[test]
    fn test_detect_geom_type_missing_type() {
        let geom = json!({"coordinates": [0.0, 0.0]});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Unknown as i32);
    }

    // --- json_to_mvt_value tests ---

    #[test]
    fn test_json_to_mvt_value_string() {
        let val = json_to_mvt_value(&json!("hello"));
        assert_eq!(val.string_value, Some("hello".to_string()));
        assert_eq!(val.int_value, None);
        assert_eq!(val.double_value, None);
        assert_eq!(val.bool_value, None);
    }

    #[test]
    fn test_json_to_mvt_value_integer() {
        let val = json_to_mvt_value(&json!(42));
        assert_eq!(val.int_value, Some(42));
        assert_eq!(val.string_value, None);
        assert_eq!(val.double_value, None);
    }

    #[test]
    fn test_json_to_mvt_value_float() {
        let val = json_to_mvt_value(&json!(3.14));
        assert_eq!(val.double_value, Some(3.14));
        assert_eq!(val.int_value, None);
        assert_eq!(val.string_value, None);
    }

    #[test]
    fn test_json_to_mvt_value_bool_true() {
        let val = json_to_mvt_value(&json!(true));
        assert_eq!(val.bool_value, Some(true));
    }

    #[test]
    fn test_json_to_mvt_value_bool_false() {
        let val = json_to_mvt_value(&json!(false));
        assert_eq!(val.bool_value, Some(false));
    }

    #[test]
    fn test_json_to_mvt_value_null() {
        let val = json_to_mvt_value(&json!(null));
        // Null falls through to the catch-all, stored as string
        assert!(val.string_value.is_some());
    }

    // --- encode_tile tests ---

    #[test]
    fn test_encode_tile_point_feature() {
        let tile_coord = TileCoord { z: 0, x: 0, y: 0 };
        let feature = FeatureData {
            id: 1,
            geometry: json!({
                "type": "Point",
                "coordinates": [0.0, 0.0]
            }),
            properties: json!({"name": "test"}),
            bounds: crate::postgis::Bounds {
                min_lon: -180.0,
                min_lat: -85.0,
                max_lon: 180.0,
                max_lat: 85.0,
            },
            layer_name: "test_layer".to_string(),
        };

        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("test_layer".to_string(), vec![feature]);

        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        // Should produce non-empty gzipped output
        assert!(!result.is_empty());
        // Gzip magic number check
        assert_eq!(result[0], 0x1f);
        assert_eq!(result[1], 0x8b);
    }

    #[test]
    fn test_encode_tile_empty_features() {
        let tile_coord = TileCoord { z: 0, x: 0, y: 0 };
        let features_by_layer: HashMap<String, Vec<FeatureData>> = HashMap::new();

        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        // Even with no layers, should still produce valid gzipped output (empty tile)
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_multiple_properties() {
        let tile_coord = TileCoord {
            z: 10,
            x: 512,
            y: 340,
        };
        let feature = FeatureData {
            id: 42,
            geometry: json!({
                "type": "Point",
                "coordinates": [0.0, 51.5]
            }),
            properties: json!({"name": "London", "population": 9000000, "capital": true}),
            bounds: crate::postgis::Bounds {
                min_lon: -1.0,
                min_lat: 51.0,
                max_lon: 1.0,
                max_lat: 52.0,
            },
            layer_name: "cities".to_string(),
        };

        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("cities".to_string(), vec![feature]);

        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        assert!(!result.is_empty());
        // Verify gzip header
        assert_eq!(result[0], 0x1f);
        assert_eq!(result[1], 0x8b);
    }

    // --- simplification tests ---

    #[test]
    fn test_simplify_geometry_point_unchanged() {
        let geom = json!({"type": "Point", "coordinates": [1.0, 2.0]});
        let result = simplify_geometry(&geom, 0.1);
        assert_eq!(result, geom);
    }

    #[test]
    fn test_simplify_geometry_linestring() {
        // A line with a slight deviation that should be simplified away
        let geom = json!({
            "type": "LineString",
            "coordinates": [[0.0, 0.0], [0.5, 0.001], [1.0, 0.0]]
        });
        let result = simplify_geometry(&geom, 0.01);
        let coords = result["coordinates"].as_array().unwrap();
        // With tolerance 0.01, the middle point (deviation 0.001) should be removed
        assert_eq!(coords.len(), 2);
    }

    #[test]
    fn test_simplify_geometry_polygon() {
        let geom = json!({
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 0.001], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]]
        });
        let result = simplify_geometry(&geom, 0.01);
        let exterior = result["coordinates"][0].as_array().unwrap();
        // The near-collinear point should be simplified away
        assert!(exterior.len() < 6);
    }

    // --- property filtering tests ---

    #[test]
    fn test_compute_excluded_properties_none() {
        let excluded = compute_excluded_properties(None, 5);
        assert!(excluded.is_empty());
    }

    #[test]
    fn test_compute_excluded_properties_below_zoom() {
        use crate::config::PropertyRule;

        let cfg = LayerConfig {
            name: "test".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: Some(vec![PropertyRule {
                below_zoom: 10,
                exclude: vec!["description".to_string(), "metadata".to_string()],
            }]),
            generate_label_points: false,
            generate_boundary_lines: false,
        };

        // At zoom 5 (below 10), properties should be excluded
        let excluded = compute_excluded_properties(Some(&cfg), 5);
        assert!(excluded.contains("description"));
        assert!(excluded.contains("metadata"));

        // At zoom 12 (above 10), nothing excluded
        let excluded = compute_excluded_properties(Some(&cfg), 12);
        assert!(excluded.is_empty());
    }

    #[test]
    fn test_encode_tile_with_property_filtering() {
        use crate::config::PropertyRule;

        let tile_coord = TileCoord { z: 5, x: 16, y: 16 };
        let feature = FeatureData {
            id: 1,
            geometry: json!({"type": "Point", "coordinates": [0.0, 0.0]}),
            properties: json!({"name": "test", "description": "long text", "metadata": "extra"}),
            bounds: crate::postgis::Bounds {
                min_lon: -1.0, min_lat: -1.0, max_lon: 1.0, max_lat: 1.0,
            },
            layer_name: "layer".to_string(),
        };

        let cfg = LayerConfig {
            name: "layer".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: Some(vec![PropertyRule {
                below_zoom: 10,
                exclude: vec!["description".to_string(), "metadata".to_string()],
            }]),
            generate_label_points: false,
            generate_boundary_lines: false,
        };

        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("layer".to_string(), vec![feature]);

        let mut layer_configs: HashMap<String, &LayerConfig> = HashMap::new();
        layer_configs.insert("layer".to_string(), &cfg);

        // At zoom 5, description and metadata should be excluded
        let result = encode_tile_with_config(&tile_coord, &features_by_layer, &layer_configs).unwrap();
        assert!(!result.is_empty());
        // Verify it's valid gzip
        assert_eq!(result[0], 0x1f);
        assert_eq!(result[1], 0x8b);
    }

    // --- geometry encoding tests ---

    fn make_feature(geom: JsonValue) -> FeatureData {
        FeatureData {
            id: 1,
            geometry: geom,
            properties: json!({}),
            bounds: crate::postgis::Bounds {
                min_lon: -180.0, min_lat: -85.0, max_lon: 180.0, max_lat: 85.0,
            },
            layer_name: "test".to_string(),
        }
    }

    fn encode_feature_tile(geom: JsonValue) -> Vec<u8> {
        let tile_coord = TileCoord { z: 0, x: 0, y: 0 };
        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("test".to_string(), vec![make_feature(geom)]);
        encode_tile(&tile_coord, &features_by_layer).unwrap()
    }

    #[test]
    fn test_encode_tile_linestring() {
        let geom = json!({
            "type": "LineString",
            "coordinates": [[0.0, 0.0], [10.0, 10.0], [20.0, 0.0]]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
        assert_eq!(result[0], 0x1f);
    }

    #[test]
    fn test_encode_tile_polygon() {
        let geom = json!({
            "type": "Polygon",
            "coordinates": [[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0], [0.0, 0.0]]]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
        assert_eq!(result[0], 0x1f);
    }

    #[test]
    fn test_encode_tile_multipoint() {
        let geom = json!({
            "type": "MultiPoint",
            "coordinates": [[0.0, 0.0], [10.0, 10.0]]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_multilinestring() {
        let geom = json!({
            "type": "MultiLineString",
            "coordinates": [
                [[0.0, 0.0], [10.0, 10.0]],
                [[20.0, 20.0], [30.0, 30.0]]
            ]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_multipolygon() {
        let geom = json!({
            "type": "MultiPolygon",
            "coordinates": [
                [[[0.0, 0.0], [5.0, 0.0], [5.0, 5.0], [0.0, 0.0]]],
                [[[10.0, 10.0], [15.0, 10.0], [15.0, 15.0], [10.0, 10.0]]]
            ]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_polygon_with_hole() {
        let geom = json!({
            "type": "Polygon",
            "coordinates": [
                [[0.0, 0.0], [20.0, 0.0], [20.0, 20.0], [0.0, 20.0], [0.0, 0.0]],
                [[5.0, 5.0], [15.0, 5.0], [15.0, 15.0], [5.0, 15.0], [5.0, 5.0]]
            ]
        });
        let result = encode_feature_tile(geom);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_unknown_geom_type() {
        let geom = json!({
            "type": "GeometryCollection",
            "geometries": []
        });
        // Should produce a tile but with no features (unknown geometry type gives empty commands)
        let tile_coord = TileCoord { z: 0, x: 0, y: 0 };
        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("test".to_string(), vec![make_feature(geom)]);
        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        // Should still be valid gzip even if empty
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_empty_coordinates() {
        // Linestring with too few points should produce empty geometry
        let geom = json!({
            "type": "LineString",
            "coordinates": [[0.0, 0.0]]
        });
        let tile_coord = TileCoord { z: 0, x: 0, y: 0 };
        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("test".to_string(), vec![make_feature(geom)]);
        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_tile_multiple_layers() {
        let tile_coord = TileCoord { z: 5, x: 16, y: 16 };
        let feature_a = FeatureData {
            id: 1,
            geometry: json!({"type": "Point", "coordinates": [0.0, 0.0]}),
            properties: json!({"name": "a"}),
            bounds: crate::postgis::Bounds {
                min_lon: -1.0, min_lat: -1.0, max_lon: 1.0, max_lat: 1.0,
            },
            layer_name: "layer_a".to_string(),
        };
        let feature_b = FeatureData {
            id: 2,
            geometry: json!({"type": "Point", "coordinates": [1.0, 1.0]}),
            properties: json!({"type": "b"}),
            bounds: crate::postgis::Bounds {
                min_lon: 0.0, min_lat: 0.0, max_lon: 2.0, max_lat: 2.0,
            },
            layer_name: "layer_b".to_string(),
        };

        let mut features_by_layer = HashMap::new();
        features_by_layer.insert("layer_a".to_string(), vec![feature_a]);
        features_by_layer.insert("layer_b".to_string(), vec![feature_b]);

        let result = encode_tile(&tile_coord, &features_by_layer).unwrap();
        assert!(!result.is_empty());
    }

    // --- value_to_string tests ---

    #[test]
    fn test_value_to_string_variants() {
        assert_eq!(value_to_string(&json!("hello")), "s:hello");
        assert_eq!(value_to_string(&json!(42)), "n:42");
        assert_eq!(value_to_string(&json!(true)), "b:true");
        assert_eq!(value_to_string(&json!(null)), "o:null");
    }

    // --- parse_linestring tests ---

    #[test]
    fn test_parse_linestring_valid() {
        let coords = json!([[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]);
        let ls = parse_linestring(&coords);
        assert!(ls.is_some());
        assert_eq!(ls.unwrap().0.len(), 3);
    }

    #[test]
    fn test_parse_linestring_too_few_points() {
        let coords = json!([[0.0, 0.0]]);
        assert!(parse_linestring(&coords).is_none());
    }

    #[test]
    fn test_parse_linestring_not_array() {
        let coords = json!("not an array");
        assert!(parse_linestring(&coords).is_none());
    }

    #[test]
    fn test_parse_linestring_empty() {
        let coords = json!([]);
        assert!(parse_linestring(&coords).is_none());
    }

    // --- parse_polygon tests ---

    #[test]
    fn test_parse_polygon_valid() {
        let coords = json!([[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]);
        let poly = parse_polygon(&coords);
        assert!(poly.is_some());
    }

    #[test]
    fn test_parse_polygon_with_hole() {
        let coords = json!([
            [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 0.0]],
            [[2.0, 2.0], [8.0, 2.0], [8.0, 8.0], [2.0, 2.0]]
        ]);
        let poly = parse_polygon(&coords);
        assert!(poly.is_some());
        assert_eq!(poly.unwrap().interiors().len(), 1);
    }

    #[test]
    fn test_parse_polygon_empty_rings() {
        let coords = json!([]);
        assert!(parse_polygon(&coords).is_none());
    }

    #[test]
    fn test_parse_polygon_not_array() {
        let coords = json!(42);
        assert!(parse_polygon(&coords).is_none());
    }

    // --- simplification tolerance ---

    #[test]
    fn test_simplification_tolerance_none_config() {
        assert_eq!(simplification_tolerance(None, 10), None);
    }

    #[test]
    fn test_simplification_tolerance_no_tolerance_set() {
        let cfg = LayerConfig {
            name: "test".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: None,
            generate_label_points: false,
            generate_boundary_lines: false,
        };
        assert_eq!(simplification_tolerance(Some(&cfg), 10), None);
    }

    #[test]
    fn test_simplification_tolerance_scales_by_zoom() {
        let cfg = LayerConfig {
            name: "test".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: Some(0.001),
            property_rules: None,
            generate_label_points: false,
            generate_boundary_lines: false,
        };

        let low_zoom = simplification_tolerance(Some(&cfg), 5).unwrap();
        let high_zoom = simplification_tolerance(Some(&cfg), 15).unwrap();
        // Lower zoom should have higher tolerance (more simplification)
        assert!(low_zoom > high_zoom);
    }

    // --- simplify_geometry edge cases ---

    #[test]
    fn test_simplify_multilinestring() {
        let geom = json!({
            "type": "MultiLineString",
            "coordinates": [
                [[0.0, 0.0], [0.5, 0.001], [1.0, 0.0]],
                [[2.0, 2.0], [2.5, 2.001], [3.0, 2.0]]
            ]
        });
        let result = simplify_geometry(&geom, 0.01);
        assert_eq!(result["type"], "MultiLineString");
        let lines = result["coordinates"].as_array().unwrap();
        // Each line should be simplified
        for line in lines {
            assert_eq!(line.as_array().unwrap().len(), 2);
        }
    }

    #[test]
    fn test_simplify_multipolygon() {
        let geom = json!({
            "type": "MultiPolygon",
            "coordinates": [
                [[[0.0, 0.0], [1.0, 0.0], [1.0, 0.001], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]]
            ]
        });
        let result = simplify_geometry(&geom, 0.01);
        assert_eq!(result["type"], "MultiPolygon");
    }

    // --- compute_excluded_properties with multiple rules ---

    #[test]
    fn test_compute_excluded_properties_multiple_rules() {
        use crate::config::PropertyRule;

        let cfg = LayerConfig {
            name: "test".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: Some(vec![
                PropertyRule {
                    below_zoom: 5,
                    exclude: vec!["metadata".to_string()],
                },
                PropertyRule {
                    below_zoom: 10,
                    exclude: vec!["description".to_string()],
                },
            ]),
            generate_label_points: false,
            generate_boundary_lines: false,
        };

        // At zoom 3 (below both thresholds): both excluded
        let excluded = compute_excluded_properties(Some(&cfg), 3);
        assert!(excluded.contains("metadata"));
        assert!(excluded.contains("description"));

        // At zoom 7 (above 5, below 10): only description excluded
        let excluded = compute_excluded_properties(Some(&cfg), 7);
        assert!(!excluded.contains("metadata"));
        assert!(excluded.contains("description"));

        // At zoom 12 (above both): none excluded
        let excluded = compute_excluded_properties(Some(&cfg), 12);
        assert!(excluded.is_empty());
    }

    #[test]
    fn test_compute_excluded_properties_no_rules() {
        let cfg = LayerConfig {
            name: "test".to_string(),
            schema: None,
            table: "test".to_string(),
            geometry_column: None,
            id_column: None,
            srid: None,
            properties: None,
            filter: None,
            geometry_columns: None,
            simplify_tolerance: None,
            property_rules: None,
            generate_label_points: false,
            generate_boundary_lines: false,
        };

        let excluded = compute_excluded_properties(Some(&cfg), 0);
        assert!(excluded.is_empty());
    }

    // --- json_to_mvt_value edge cases ---

    #[test]
    fn test_json_to_mvt_value_array() {
        let val = json_to_mvt_value(&json!([1, 2, 3]));
        // Arrays fall through to string representation
        assert!(val.string_value.is_some());
    }

    #[test]
    fn test_json_to_mvt_value_object() {
        let val = json_to_mvt_value(&json!({"key": "value"}));
        assert!(val.string_value.is_some());
    }

    #[test]
    fn test_json_to_mvt_value_negative_integer() {
        let val = json_to_mvt_value(&json!(-42));
        assert_eq!(val.int_value, Some(-42));
    }

    #[test]
    fn test_json_to_mvt_value_zero() {
        let val = json_to_mvt_value(&json!(0));
        assert_eq!(val.int_value, Some(0));
    }

    // --- detect_geom_type additional ---

    #[test]
    fn test_detect_geom_type_multilinestring() {
        let geom = json!({"type": "MultiLineString"});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Linestring as i32);
    }

    #[test]
    fn test_detect_geom_type_multipolygon() {
        let geom = json!({"type": "MultiPolygon"});
        assert_eq!(detect_geom_type(&geom) as i32, GeomType::Polygon as i32);
    }
}
