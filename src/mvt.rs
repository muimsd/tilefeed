use anyhow::Result;
use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::io::Write;

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
    let mut layers = Vec::new();

    for (layer_name, features) in features_by_layer {
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        let mut key_index: HashMap<String, u32> = HashMap::new();
        let mut value_index: HashMap<String, u32> = HashMap::new();
        let mut mvt_features: Vec<Feature> = Vec::new();

        for feature in features {
            let geom_type = detect_geom_type(&feature.geometry);
            let geometry = encode_geometry(&feature.geometry, tile_coord)?;

            if geometry.is_empty() {
                continue;
            }

            let mut tags = Vec::new();
            if let Some(props) = feature.properties.as_object() {
                for (k, v) in props {
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

    let mut cmds = Vec::new();
    cmds.push(command(1, 1)); // MoveTo, count=1
    cmds.push(zigzag(x));
    cmds.push(zigzag(y));
    Ok(cmds)
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
