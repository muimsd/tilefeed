use crate::postgis::Bounds;

/// A tile coordinate (z, x, y) in TMS or XYZ scheme
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct TileCoord {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileCoord {
    /// Get the bounding box of this tile in EPSG:4326 (lon/lat)
    pub fn bounds(&self) -> Bounds {
        let n = (1u64 << self.z) as f64;
        let min_lon = (self.x as f64 / n) * 360.0 - 180.0;
        let max_lon = ((self.x + 1) as f64 / n) * 360.0 - 180.0;

        let min_lat_rad = std::f64::consts::PI * (1.0 - 2.0 * (self.y + 1) as f64 / n);
        let max_lat_rad = std::f64::consts::PI * (1.0 - 2.0 * self.y as f64 / n);

        let min_lat = min_lat_rad.sinh().atan().to_degrees();
        let max_lat = max_lat_rad.sinh().atan().to_degrees();

        Bounds {
            min_lon,
            min_lat,
            max_lon,
            max_lat,
        }
    }
}

/// Convert a longitude to tile X coordinate at a given zoom
fn lon_to_tile_x(lon: f64, zoom: u8) -> u32 {
    let n = (1u64 << zoom) as f64;
    ((lon + 180.0) / 360.0 * n).floor() as u32
}

/// Convert a latitude to tile Y coordinate at a given zoom (XYZ scheme)
fn lat_to_tile_y(lat: f64, zoom: u8) -> u32 {
    let n = (1u64 << zoom) as f64;
    let lat_rad = lat.to_radians();
    ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor() as u32
}

/// Get all tiles that intersect the given bounding box at the specified zoom levels
pub fn tiles_for_bounds(bounds: &Bounds, min_zoom: u8, max_zoom: u8) -> Vec<TileCoord> {
    let mut tiles = Vec::new();

    for z in min_zoom..=max_zoom {
        let max_tile = (1u32 << z) - 1;

        let x_min = lon_to_tile_x(bounds.min_lon, z).min(max_tile);
        let x_max = lon_to_tile_x(bounds.max_lon, z).min(max_tile);
        // Note: lat_to_tile_y is inverted (higher lat = lower y)
        let y_min = lat_to_tile_y(bounds.max_lat, z).min(max_tile);
        let y_max = lat_to_tile_y(bounds.min_lat, z).min(max_tile);

        for x in x_min..=x_max {
            for y in y_min..=y_max {
                tiles.push(TileCoord { z, x, y });
            }
        }
    }

    tiles
}

/// Convert tile-relative coordinates (0..extent) to pixel within tile
pub fn world_to_tile_coords(lon: f64, lat: f64, tile: &TileCoord, extent: u32) -> (i32, i32) {
    let bounds = tile.bounds();
    let x = ((lon - bounds.min_lon) / (bounds.max_lon - bounds.min_lon) * extent as f64) as i32;
    let y = ((bounds.max_lat - lat) / (bounds.max_lat - bounds.min_lat) * extent as f64) as i32;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_bounds_roundtrip() {
        let tile = TileCoord { z: 0, x: 0, y: 0 };
        let bounds = tile.bounds();
        assert!((bounds.min_lon - (-180.0)).abs() < 1e-6);
        assert!((bounds.max_lon - 180.0).abs() < 1e-6);
    }

    #[test]
    fn test_tiles_for_bounds() {
        // A small area around London
        let bounds = Bounds {
            min_lon: -0.2,
            min_lat: 51.4,
            max_lon: 0.0,
            max_lat: 51.6,
        };
        let tiles = tiles_for_bounds(&bounds, 10, 10);
        assert!(!tiles.is_empty());
        for t in &tiles {
            assert_eq!(t.z, 10);
        }
    }
}
