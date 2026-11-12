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

/// Inclusive XYZ tile range `(x_min, x_max, y_min, y_max)` covering `bounds` at `zoom`.
/// Coordinates outside the Web Mercator square are clamped to the edge tiles.
pub fn tile_range_for_bounds(bounds: &Bounds, zoom: u8) -> (u32, u32, u32, u32) {
    let max_tile = (1u32 << zoom) - 1;
    let x_min = lon_to_tile_x(bounds.min_lon, zoom).min(max_tile);
    let x_max = lon_to_tile_x(bounds.max_lon, zoom).min(max_tile);
    // Note: lat_to_tile_y is inverted (higher lat = lower y)
    let y_min = lat_to_tile_y(bounds.max_lat, zoom).min(max_tile);
    let y_max = lat_to_tile_y(bounds.min_lat, zoom).min(max_tile);
    (x_min, x_max, y_min, y_max)
}

/// An inclusive block of XYZ tiles at one zoom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRange {
    pub z: u8,
    pub x_min: u32,
    pub x_max: u32,
    pub y_min: u32,
    pub y_max: u32,
}

impl TileRange {
    pub fn contains(&self, z: u8, x: u32, y: u32) -> bool {
        z == self.z
            && (self.x_min..=self.x_max).contains(&x)
            && (self.y_min..=self.y_max).contains(&y)
    }

    /// How many tiles the range holds.
    pub fn tile_count(&self) -> u64 {
        (self.x_max - self.x_min + 1) as u64 * (self.y_max - self.y_min + 1) as u64
    }
}

/// The tiles intersecting `bounds` at each zoom, as one range per zoom:
/// `tiles_for_bounds` without listing them, for areas too big to enumerate.
pub fn tile_ranges_for_bounds(bounds: &Bounds, min_zoom: u8, max_zoom: u8) -> Vec<TileRange> {
    (min_zoom..=max_zoom)
        .map(|z| {
            let (x_min, x_max, y_min, y_max) = tile_range_for_bounds(bounds, z);
            TileRange {
                z,
                x_min,
                x_max,
                y_min,
                y_max,
            }
        })
        .collect()
}

/// Get all tiles that intersect the given bounding box at the specified zoom levels
pub fn tiles_for_bounds(bounds: &Bounds, min_zoom: u8, max_zoom: u8) -> Vec<TileCoord> {
    let mut tiles = Vec::new();

    for z in min_zoom..=max_zoom {
        let (x_min, x_max, y_min, y_max) = tile_range_for_bounds(bounds, z);
        for x in x_min..=x_max {
            for y in y_min..=y_max {
                tiles.push(TileCoord { z, x, y });
            }
        }
    }

    tiles
}

/// Smallest bounds containing both `a` and `b`.
pub fn union_bounds(a: &Bounds, b: &Bounds) -> Bounds {
    Bounds {
        min_lon: a.min_lon.min(b.min_lon),
        min_lat: a.min_lat.min(b.min_lat),
        max_lon: a.max_lon.max(b.max_lon),
        max_lat: a.max_lat.max(b.max_lat),
    }
}

/// Grow `bounds` by `margin` degrees on every side.
pub fn expand_bounds(bounds: &Bounds, margin: f64) -> Bounds {
    Bounds {
        min_lon: bounds.min_lon - margin,
        min_lat: bounds.min_lat - margin,
        max_lon: bounds.max_lon + margin,
        max_lat: bounds.max_lat + margin,
    }
}

/// Next quadtree level for a top-down walk: the children of `parents` (all at
/// the same zoom) that intersect `extent`, sorted and de-duplicated.
///
/// A child tile's envelope is contained in its parent's, so a child can only
/// hold features if its parent does — expanding only non-empty parents never
/// skips a tile with data.
pub fn children_within(parents: &[TileCoord], extent: &Bounds) -> Vec<TileCoord> {
    let mut out = Vec::with_capacity(parents.len() * 4);
    let Some(first) = parents.first() else {
        return out;
    };
    let z = first.z + 1;
    let (x_min, x_max, y_min, y_max) = tile_range_for_bounds(extent, z);
    for p in parents {
        debug_assert_eq!(p.z + 1, z, "parents must share one zoom level");
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (x, y) = (p.x * 2 + dx, p.y * 2 + dy);
            if (x_min..=x_max).contains(&x) && (y_min..=y_max).contains(&y) {
                out.push(TileCoord { z, x, y });
            }
        }
    }
    out.sort_by_key(|t| (t.x, t.y));
    out.dedup();
    out
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

    fn lisbon() -> Bounds {
        // Extent of the demo "parcels" table
        Bounds {
            min_lon: -9.16,
            min_lat: 38.71,
            max_lon: -9.1301,
            max_lat: 38.7339,
        }
    }

    #[test]
    fn test_tile_range_for_small_bbox_is_tiny() {
        let b = lisbon();
        let (x0, x1, y0, y1) = tile_range_for_bounds(&b, 10);
        // The bbox straddles the x=485/486 boundary at z10
        assert_eq!((x0, x1), (485, 486));
        assert_eq!(y0, y1);
        let (x0, x1, y0, y1) = tile_range_for_bounds(&b, 15);
        // ~0.03deg wide at z15 is a handful of tiles, not 2^15 squared
        assert!(x1 - x0 < 4, "x range {}..{}", x0, x1);
        assert!(y1 - y0 < 4, "y range {}..{}", y0, y1);
        // Every tile in the range intersects the bbox
        for t in tiles_for_bounds(&b, 15, 15) {
            let tb = t.bounds();
            assert!(tb.min_lon <= b.max_lon && tb.max_lon >= b.min_lon);
            assert!(tb.min_lat <= b.max_lat && tb.max_lat >= b.min_lat);
        }
    }

    #[test]
    fn test_tile_ranges_match_tiles_for_bounds() {
        let b = lisbon();
        let ranges = tile_ranges_for_bounds(&b, 8, 14);
        let tiles = tiles_for_bounds(&b, 8, 14);
        assert_eq!(ranges.len(), 7);
        assert_eq!(
            ranges.iter().map(TileRange::tile_count).sum::<u64>(),
            tiles.len() as u64
        );
        for t in &tiles {
            assert!(ranges.iter().any(|r| r.contains(t.z, t.x, t.y)), "{t:?}");
        }
        let r = ranges[0];
        assert!(!r.contains(r.z + 1, r.x_min, r.y_min));
        assert!(!r.contains(r.z, r.x_max + 1, r.y_min));
    }

    #[test]
    fn test_tile_range_clamps_out_of_range_coords() {
        let b = Bounds {
            min_lon: -200.0,
            min_lat: -90.0,
            max_lon: 200.0,
            max_lat: 90.0,
        };
        assert_eq!(tile_range_for_bounds(&b, 3), (0, 7, 0, 7));
    }

    #[test]
    fn test_union_and_expand_bounds() {
        let a = Bounds {
            min_lon: 0.0,
            min_lat: 0.0,
            max_lon: 1.0,
            max_lat: 1.0,
        };
        let b = Bounds {
            min_lon: -2.0,
            min_lat: 0.5,
            max_lon: 0.5,
            max_lat: 3.0,
        };
        let u = union_bounds(&a, &b);
        assert_eq!(
            (u.min_lon, u.min_lat, u.max_lon, u.max_lat),
            (-2.0, 0.0, 1.0, 3.0)
        );
        let e = expand_bounds(&a, 0.5);
        assert_eq!(
            (e.min_lon, e.min_lat, e.max_lon, e.max_lat),
            (-0.5, -0.5, 1.5, 1.5)
        );
    }

    #[test]
    fn test_children_within_matches_full_range_scan() {
        // Walking the quadtree from every tile at z10 must reach exactly the
        // tiles a flat scan of the extent finds at each deeper zoom.
        let b = lisbon();
        let mut level = tiles_for_bounds(&b, 10, 10);
        for z in 11..=15u8 {
            level = children_within(&level, &b);
            let mut flat = tiles_for_bounds(&b, z, z);
            flat.sort_by_key(|t| (t.x, t.y));
            assert_eq!(level, flat, "zoom {}", z);
        }
    }

    #[test]
    fn test_children_within_only_expands_given_parents() {
        let world = Bounds {
            min_lon: -180.0,
            min_lat: -85.0,
            max_lon: 180.0,
            max_lat: 85.0,
        };
        let parents = [TileCoord { z: 1, x: 1, y: 0 }];
        let kids = children_within(&parents, &world);
        assert_eq!(
            kids,
            vec![
                TileCoord { z: 2, x: 2, y: 0 },
                TileCoord { z: 2, x: 2, y: 1 },
                TileCoord { z: 2, x: 3, y: 0 },
                TileCoord { z: 2, x: 3, y: 1 },
            ]
        );
        // Children are geometrically inside the parent
        let pb = parents[0].bounds();
        for k in &kids {
            let kb = k.bounds();
            assert!(kb.min_lon >= pb.min_lon && kb.max_lon <= pb.max_lon);
            assert!(kb.min_lat >= pb.min_lat && kb.max_lat <= pb.max_lat);
        }
        assert!(children_within(&[], &world).is_empty());
    }

    #[test]
    fn test_children_within_filters_by_extent() {
        let b = lisbon();
        let parent = tiles_for_bounds(&b, 10, 10);
        assert_eq!(parent.len(), 2);
        // Only the first parent: its children never spill into the second's
        let kids = children_within(&parent[..1], &b);
        assert!(!kids.is_empty() && kids.len() <= 4);
        for k in &kids {
            assert_eq!(k.x / 2, parent[0].x);
        }
        let (x0, x1, y0, y1) = tile_range_for_bounds(&b, 11);
        for k in &kids {
            assert!((x0..=x1).contains(&k.x) && (y0..=y1).contains(&k.y));
        }
    }

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

    #[test]
    fn test_tiles_for_bounds_zoom_0() {
        // At zoom 0, the entire world is one tile
        let bounds = Bounds {
            min_lon: -10.0,
            min_lat: -10.0,
            max_lon: 10.0,
            max_lat: 10.0,
        };
        let tiles = tiles_for_bounds(&bounds, 0, 0);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].z, 0);
        assert_eq!(tiles[0].x, 0);
        assert_eq!(tiles[0].y, 0);
    }

    #[test]
    fn test_tiles_for_bounds_multiple_zooms() {
        let bounds = Bounds {
            min_lon: 0.0,
            min_lat: 0.0,
            max_lon: 1.0,
            max_lat: 1.0,
        };
        let tiles = tiles_for_bounds(&bounds, 0, 2);
        // Should contain tiles at zoom 0, 1, and 2
        assert!(tiles.iter().any(|t| t.z == 0));
        assert!(tiles.iter().any(|t| t.z == 1));
        assert!(tiles.iter().any(|t| t.z == 2));
    }

    #[test]
    fn test_world_to_tile_coords_center() {
        // At zoom 0, tile (0,0) covers the whole world.
        // The center of the world (lon=0, lat=0) should map to approximately (extent/2, extent/2)
        let tile = TileCoord { z: 0, x: 0, y: 0 };
        let extent = 4096u32;
        let (x, y) = world_to_tile_coords(0.0, 0.0, &tile, extent);
        // lon=0 is the center horizontally -> x ~ extent/2
        assert!((x - (extent as i32 / 2)).abs() < 2);
        // lat=0 is roughly the center vertically (Mercator) -> y ~ extent/2
        assert!((y - (extent as i32 / 2)).abs() < 100); // Mercator distortion means this is approximate
    }

    #[test]
    fn test_world_to_tile_coords_top_left() {
        // The top-left corner of the world tile should map to (0, 0)
        let tile = TileCoord { z: 0, x: 0, y: 0 };
        let bounds = tile.bounds();
        let extent = 4096u32;
        let (x, y) = world_to_tile_coords(bounds.min_lon, bounds.max_lat, &tile, extent);
        assert_eq!(x, 0);
        assert_eq!(y, 0);
    }

    #[test]
    fn test_world_to_tile_coords_bottom_right() {
        // The bottom-right corner should map to approximately (extent, extent)
        let tile = TileCoord { z: 0, x: 0, y: 0 };
        let bounds = tile.bounds();
        let extent = 4096u32;
        let (x, y) = world_to_tile_coords(bounds.max_lon, bounds.min_lat, &tile, extent);
        assert!((x - extent as i32).abs() <= 1);
        assert!((y - extent as i32).abs() <= 1);
    }

    #[test]
    fn test_tile_bounds_zoom_1() {
        // At zoom 1 there are 4 tiles; tile (0,0) covers the top-left quadrant
        let tile = TileCoord { z: 1, x: 0, y: 0 };
        let bounds = tile.bounds();
        assert!((bounds.min_lon - (-180.0)).abs() < 1e-6);
        assert!((bounds.max_lon - 0.0).abs() < 1e-6);
        // max_lat should be ~85.05 (Mercator limit)
        assert!(bounds.max_lat > 80.0);
    }

    #[test]
    fn test_lon_to_tile_x_boundaries() {
        // At zoom 1, lon=-180 -> x=0, lon=0 -> x=1, lon=179.9 -> x=1
        assert_eq!(lon_to_tile_x(-180.0, 1), 0);
        assert_eq!(lon_to_tile_x(0.0, 1), 1);
        assert_eq!(lon_to_tile_x(179.9, 1), 1);
    }

    #[test]
    fn test_lat_to_tile_y_equator() {
        // At zoom 1, equator (lat=0) is the boundary between tile y=0 and y=1
        let y = lat_to_tile_y(0.0, 1);
        assert_eq!(y, 1);
    }

    #[test]
    fn test_tiles_for_bounds_whole_world() {
        let bounds = Bounds {
            min_lon: -180.0,
            min_lat: -85.0,
            max_lon: 180.0,
            max_lat: 85.0,
        };
        // At zoom 1, the whole world should return 4 tiles
        let tiles = tiles_for_bounds(&bounds, 1, 1);
        assert_eq!(tiles.len(), 4);
    }

    #[test]
    fn test_tiles_for_bounds_antimeridian() {
        // Bounds near the antimeridian (but not crossing it, since min_lon < max_lon)
        let bounds = Bounds {
            min_lon: 179.0,
            min_lat: 0.0,
            max_lon: 179.9,
            max_lat: 1.0,
        };
        let tiles = tiles_for_bounds(&bounds, 2, 2);
        assert!(!tiles.is_empty());
        // All tiles should be valid at zoom 2 (x in 0..3, y in 0..3)
        for t in &tiles {
            assert!(t.x < 4);
            assert!(t.y < 4);
        }
    }

    #[test]
    fn test_tiles_for_bounds_near_poles() {
        // Near the north pole
        let bounds = Bounds {
            min_lon: -10.0,
            min_lat: 80.0,
            max_lon: 10.0,
            max_lat: 84.0,
        };
        let tiles = tiles_for_bounds(&bounds, 3, 3);
        assert!(!tiles.is_empty());
        for t in &tiles {
            assert_eq!(t.z, 3);
            // Near the north pole, y should be small (top of the map)
            assert!(t.y < 4); // 2^3 / 2 = 4 is the equator
        }
    }

    // --- additional TileCoord::bounds tests ---

    #[test]
    fn test_tile_bounds_zoom_0_covers_world() {
        let tile = TileCoord { z: 0, x: 0, y: 0 };
        let bounds = tile.bounds();
        assert!((bounds.min_lon - (-180.0)).abs() < 1e-6);
        assert!((bounds.max_lon - 180.0).abs() < 1e-6);
        assert!(bounds.max_lat > 85.0);
        assert!(bounds.min_lat < -85.0);
    }

    #[test]
    fn test_tile_bounds_zoom_1_quadrants() {
        // Bottom-right tile at zoom 1
        let tile = TileCoord { z: 1, x: 1, y: 1 };
        let bounds = tile.bounds();
        assert!((bounds.min_lon - 0.0).abs() < 1e-6);
        assert!((bounds.max_lon - 180.0).abs() < 1e-6);
        assert!(bounds.max_lat < 1.0); // around equator
        assert!(bounds.min_lat < -80.0);
    }

    #[test]
    fn test_tile_bounds_high_zoom() {
        // At zoom 20, tiles are very small
        let tile = TileCoord {
            z: 20,
            x: 524288,
            y: 524288,
        };
        let bounds = tile.bounds();
        let width = bounds.max_lon - bounds.min_lon;
        let height = bounds.max_lat - bounds.min_lat;
        // At zoom 20, lon span per tile = 360/2^20 ≈ 0.000343
        assert!(width < 0.001);
        assert!(height < 0.001);
    }

    // --- lon_to_tile_x / lat_to_tile_y ---

    #[test]
    fn test_lon_to_tile_x_zoom_0() {
        // At zoom 0, everything maps to x=0
        assert_eq!(lon_to_tile_x(-180.0, 0), 0);
        assert_eq!(lon_to_tile_x(0.0, 0), 0);
        assert_eq!(lon_to_tile_x(179.9, 0), 0);
    }

    #[test]
    fn test_lat_to_tile_y_zoom_0() {
        // At zoom 0, everything maps to y=0
        assert_eq!(lat_to_tile_y(80.0, 0), 0);
        assert_eq!(lat_to_tile_y(0.0, 0), 0);
        assert_eq!(lat_to_tile_y(-80.0, 0), 0);
    }

    #[test]
    fn test_lon_to_tile_x_high_zoom() {
        // At zoom 10, there are 1024 tiles horizontally
        let x = lon_to_tile_x(0.0, 10);
        assert_eq!(x, 512); // lon=0 is at the center
    }

    #[test]
    fn test_lat_to_tile_y_north_pole() {
        // Very high latitude should map to y=0
        let y = lat_to_tile_y(85.0, 5);
        assert_eq!(y, 0);
    }

    // --- tiles_for_bounds edge cases ---

    #[test]
    fn test_tiles_for_bounds_point() {
        // A very tiny bounds (essentially a point)
        let bounds = Bounds {
            min_lon: 0.0,
            min_lat: 0.0,
            max_lon: 0.0001,
            max_lat: 0.0001,
        };
        let tiles = tiles_for_bounds(&bounds, 5, 5);
        // Should return at least one tile
        assert!(!tiles.is_empty());
    }

    #[test]
    fn test_tiles_for_bounds_single_zoom() {
        let bounds = Bounds {
            min_lon: -1.0,
            min_lat: 51.0,
            max_lon: 0.0,
            max_lat: 52.0,
        };
        let tiles = tiles_for_bounds(&bounds, 8, 8);
        for t in &tiles {
            assert_eq!(t.z, 8);
            assert!(t.x < 256); // 2^8 = 256
            assert!(t.y < 256);
        }
    }

    #[test]
    fn test_tiles_for_bounds_increasing_with_zoom() {
        let bounds = Bounds {
            min_lon: -1.0,
            min_lat: 51.0,
            max_lon: 1.0,
            max_lat: 52.0,
        };
        let tiles_z5 = tiles_for_bounds(&bounds, 5, 5);
        let tiles_z8 = tiles_for_bounds(&bounds, 8, 8);
        // Higher zoom should produce more tiles for the same bounds
        assert!(tiles_z8.len() >= tiles_z5.len());
    }

    // --- world_to_tile_coords edge cases ---

    #[test]
    fn test_world_to_tile_coords_higher_zoom() {
        let tile = TileCoord {
            z: 10,
            x: 512,
            y: 340,
        };
        let bounds = tile.bounds();
        let extent = 4096u32;

        // Center of this tile
        let center_lon = (bounds.min_lon + bounds.max_lon) / 2.0;
        let center_lat = (bounds.min_lat + bounds.max_lat) / 2.0;
        let (x, y) = world_to_tile_coords(center_lon, center_lat, &tile, extent);
        // Should be approximately in the middle
        assert!((x - 2048).abs() < 50);
        assert!((y - 2048).abs() < 50);
    }

    // --- TileCoord derives ---

    #[test]
    fn test_tile_coord_equality() {
        let a = TileCoord { z: 5, x: 10, y: 20 };
        let b = TileCoord { z: 5, x: 10, y: 20 };
        let c = TileCoord { z: 5, x: 10, y: 21 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_tile_coord_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TileCoord { z: 1, x: 0, y: 0 });
        set.insert(TileCoord { z: 1, x: 0, y: 0 }); // duplicate
        set.insert(TileCoord { z: 1, x: 1, y: 0 });
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_tile_coord_clone() {
        let a = TileCoord { z: 3, x: 4, y: 5 };
        let b = a;
        assert_eq!(a, b);
    }
}
