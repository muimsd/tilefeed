//! Per-connection SSE filter: a client subscribes to `/events` with the part of
//! the map it is showing (`bbox`, `zoom`, `source`) and only receives updates
//! that change something it can see.

use std::collections::HashSet;

use crate::events::{ChangedArea, TileEvent};
use crate::postgis::Bounds;
use crate::tiles::{tile_range_for_bounds, TileCoord};

/// Highest zoom a client may report; anything above is overzoom anyway.
const MAX_CLIENT_ZOOM: f64 = 30.0;
/// Web Mercator's latitude limit — tiles stop here.
const MERCATOR_MAX_LAT: f64 = 85.051_128_779_806_59;

/// Longitude ranges and the latitude range a view covers.
type Covered = (Vec<(f64, f64)>, (f64, f64));

/// Raw `/events` query parameters.
#[derive(Debug, Default, serde::Deserialize)]
pub struct EventsQuery {
    /// `west,south,east,north` in degrees. Longitudes may be unwrapped (as
    /// MapLibre/Mapbox `getBounds()` returns them past ±180) or have
    /// `west > east` for a view across the antimeridian.
    pub bbox: Option<String>,
    /// The map's current (fractional) zoom.
    pub zoom: Option<f64>,
    /// Comma-separated source names.
    pub source: Option<String>,
}

/// What one SSE client is looking at. Every part is optional; an empty filter
/// passes every event, as `/events` always did.
#[derive(Debug, Default)]
pub struct Viewport {
    /// Longitude ranges within [-180, 180] (two when the view crosses the
    /// antimeridian) and the latitude range.
    lon_ranges: Vec<(f64, f64)>,
    lat_range: Option<(f64, f64)>,
    zoom: Option<f64>,
    sources: Option<HashSet<String>>,
}

impl Viewport {
    /// Validate the query. `known_sources` are the configured source names.
    pub fn parse<'a>(
        query: &EventsQuery,
        known_sources: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let mut viewport = Viewport::default();

        if let Some(bbox) = &query.bbox {
            let parts: Vec<f64> = bbox
                .split(',')
                .map(|p| p.trim().parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    format!("bbox must be four numbers west,south,east,north: '{bbox}'")
                })?;
            let [west, south, east, north] = parts[..] else {
                return Err(format!(
                    "bbox must be four numbers west,south,east,north: '{bbox}'"
                ));
            };
            if parts.iter().any(|v| !v.is_finite()) {
                return Err("bbox values must be finite".into());
            }
            if !(-90.0..=90.0).contains(&south) || !(-90.0..=90.0).contains(&north) {
                return Err("bbox latitudes must be within -90..90".into());
            }
            if south > north {
                return Err("bbox south must not be greater than north".into());
            }
            viewport.lon_ranges = normalize_lon_range(west, east);
            viewport.lat_range = Some((south, north));
        }

        if let Some(zoom) = query.zoom {
            if !zoom.is_finite() || !(0.0..=MAX_CLIENT_ZOOM).contains(&zoom) {
                return Err(format!("zoom must be between 0 and {MAX_CLIENT_ZOOM}"));
            }
            viewport.zoom = Some(zoom);
        }

        if let Some(list) = &query.source {
            let known: HashSet<&str> = known_sources.into_iter().collect();
            let mut sources = HashSet::new();
            for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if !known.contains(name) {
                    return Err(format!("unknown source '{name}'"));
                }
                sources.insert(name.to_string());
            }
            if !sources.is_empty() {
                viewport.sources = Some(sources);
            }
        }

        Ok(viewport)
    }

    /// Whether `event` changes anything this client can see.
    pub fn wants(&self, event: &TileEvent) -> bool {
        if let Some(sources) = &self.sources {
            if !sources.contains(event.source()) {
                return false;
            }
        }
        match event {
            // A full rebuild may have changed anything.
            TileEvent::GenerateComplete { .. } => true,
            TileEvent::UpdateComplete {
                areas, max_zoom, ..
            } => {
                if areas.is_empty() {
                    return true; // extent unknown
                }
                // The tile zoom the map shows: beyond the source's max zoom it
                // overzooms the max-zoom tiles.
                let tile_zoom = self.zoom.map(|z| (z.floor() as u8).min(*max_zoom));
                let view = self.covered_ranges(tile_zoom);
                areas.iter().any(|a| self.sees(a, tile_zoom, &view))
            }
        }
    }

    fn sees(&self, area: &ChangedArea, tile_zoom: Option<u8>, view: &Option<Covered>) -> bool {
        if let Some(z) = tile_zoom {
            if z < area.min_zoom || z > area.max_zoom {
                return false;
            }
        }
        let Some((lon_ranges, (south, north))) = view else {
            return true;
        };
        let [w, s, e, n] = area.bbox;
        s <= *north
            && n >= *south
            && lon_ranges
                .iter()
                .any(|(west, east)| w <= *east && e >= *west)
    }

    /// The viewport grown to the edges of the tiles that cover it at
    /// `tile_zoom`: the map renders whole tiles, and a tile's buffer can draw
    /// a feature that sits just outside the visible area. Without a zoom, the
    /// viewport as given.
    fn covered_ranges(&self, tile_zoom: Option<u8>) -> Option<Covered> {
        let (south, north) = self.lat_range?;
        let Some(z) = tile_zoom else {
            return Some((self.lon_ranges.clone(), (south, north)));
        };
        let mut lat = (south, north);
        let lons = self
            .lon_ranges
            .iter()
            .map(|&(west, east)| {
                let bounds = Bounds {
                    min_lon: west,
                    min_lat: south.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT),
                    max_lon: east,
                    max_lat: north.clamp(-MERCATOR_MAX_LAT, MERCATOR_MAX_LAT),
                };
                let (x_min, x_max, y_min, y_max) = tile_range_for_bounds(&bounds, z);
                let top_left = TileCoord {
                    z,
                    x: x_min,
                    y: y_min,
                }
                .bounds();
                let bottom_right = TileCoord {
                    z,
                    x: x_max,
                    y: y_max,
                }
                .bounds();
                // Keep the poles reachable: a view clamped to the Mercator
                // limit still covers features beyond it.
                lat = (
                    if south <= -MERCATOR_MAX_LAT {
                        south
                    } else {
                        bottom_right.min_lat
                    },
                    if north >= MERCATOR_MAX_LAT {
                        north
                    } else {
                        top_left.max_lat
                    },
                );
                (top_left.min_lon, bottom_right.max_lon)
            })
            .collect();
        Some((lons, lat))
    }
}

/// Turn a `west..east` longitude span — possibly unwrapped past ±180 or with
/// `west > east` across the antimeridian — into one or two ranges in [-180, 180].
fn normalize_lon_range(west: f64, east: f64) -> Vec<(f64, f64)> {
    let (mut west, mut east) = (west, east);
    if west > east {
        east += 360.0;
    }
    if east - west >= 360.0 {
        return vec![(-180.0, 180.0)];
    }
    let shift = ((west + 180.0) / 360.0).floor() * 360.0;
    west -= shift;
    east -= shift;
    if east <= 180.0 {
        vec![(west, east)]
    } else {
        vec![(west, 180.0), (-180.0, east - 360.0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(bbox: Option<&str>, zoom: Option<f64>, source: Option<&str>) -> EventsQuery {
        EventsQuery {
            bbox: bbox.map(String::from),
            zoom,
            source: source.map(String::from),
        }
    }

    fn viewport(bbox: Option<&str>, zoom: Option<f64>, source: Option<&str>) -> Viewport {
        Viewport::parse(&query(bbox, zoom, source), ["basemap", "parks"]).unwrap()
    }

    fn update(source: &str, areas: Vec<ChangedArea>) -> TileEvent {
        TileEvent::update_complete(source.into(), 1, &HashSet::new(), 14, vec![]).with_areas(areas)
    }

    fn area(bbox: [f64; 4], min_zoom: u8, max_zoom: u8) -> ChangedArea {
        ChangedArea {
            bbox,
            min_zoom,
            max_zoom,
        }
    }

    // A change in central Paris, visible at every zoom of the source.
    fn paris() -> ChangedArea {
        area([2.34, 48.85, 2.35, 48.86], 0, 14)
    }

    #[test]
    fn empty_filter_passes_everything() {
        let v = viewport(None, None, None);
        assert!(v.wants(&update("basemap", vec![paris()])));
        assert!(v.wants(&TileEvent::GenerateComplete {
            source: "parks".into(),
            duration_ms: 1
        }));
    }

    #[test]
    fn bbox_selects_changes_in_view() {
        let over_paris = viewport(Some("2.2,48.8,2.5,48.9"), Some(12.0), None);
        let over_tokyo = viewport(Some("139.6,35.6,139.8,35.7"), Some(12.0), None);
        let event = update("basemap", vec![paris()]);
        assert!(over_paris.wants(&event));
        assert!(!over_tokyo.wants(&event));
    }

    #[test]
    fn change_just_outside_view_but_in_a_visible_tile_is_sent() {
        // The z10 tile holding this view spans roughly 2.11..2.46 lon; the
        // change at 2.40 is outside the viewport but drawn in that tile.
        let v = viewport(Some("2.30,48.84,2.36,48.87"), Some(10.5), None);
        assert!(v.wants(&update(
            "basemap",
            vec![area([2.40, 48.85, 2.41, 48.86], 0, 14)]
        )));
        // Without a zoom there are no tiles to grow to, so only the bbox counts.
        let v = viewport(Some("2.30,48.84,2.36,48.87"), None, None);
        assert!(!v.wants(&update(
            "basemap",
            vec![area([2.40, 48.85, 2.41, 48.86], 0, 14)]
        )));
    }

    #[test]
    fn zoom_outside_layer_range_is_skipped() {
        // Buildings only from z13: a client at z8 can't see them.
        let buildings = update("basemap", vec![area([2.34, 48.85, 2.35, 48.86], 13, 14)]);
        assert!(!viewport(Some("2.2,48.8,2.5,48.9"), Some(8.0), None).wants(&buildings));
        assert!(viewport(Some("2.2,48.8,2.5,48.9"), Some(13.2), None).wants(&buildings));
        // Overzoomed past the source max zoom (14): the z14 tiles are shown.
        assert!(viewport(Some("2.34,48.85,2.35,48.86"), Some(18.0), None).wants(&buildings));
    }

    #[test]
    fn source_filter() {
        let v = viewport(None, None, Some("parks"));
        assert!(!v.wants(&update("basemap", vec![paris()])));
        assert!(v.wants(&update("parks", vec![paris()])));
        assert!(!v.wants(&TileEvent::GenerateComplete {
            source: "basemap".into(),
            duration_ms: 1
        }));
    }

    #[test]
    fn generate_and_unknown_extent_always_pass_the_viewport() {
        let v = viewport(Some("139.6,35.6,139.8,35.7"), Some(12.0), None);
        assert!(v.wants(&TileEvent::GenerateComplete {
            source: "basemap".into(),
            duration_ms: 1
        }));
        assert!(v.wants(&update("basemap", vec![])));
    }

    #[test]
    fn antimeridian_views() {
        let fiji = update("basemap", vec![area([178.0, -18.0, 178.5, -17.5], 0, 14)]);
        let samoa = update("basemap", vec![area([-172.0, -14.0, -171.5, -13.5], 0, 14)]);
        // west > east
        let crossing = viewport(Some("175,-20,-170,-10"), Some(5.0), None);
        assert!(crossing.wants(&fiji) && crossing.wants(&samoa));
        // Unwrapped, as getBounds() returns after panning east past 180
        let unwrapped = viewport(Some("175,-20,190,-10"), Some(5.0), None);
        assert!(unwrapped.wants(&fiji) && unwrapped.wants(&samoa));
        // Panned a full turn west
        let shifted = viewport(Some("-185,-20,-170,-10"), Some(5.0), None);
        assert!(shifted.wants(&fiji) && shifted.wants(&samoa));
        // Zoomed out past a whole world
        let world = viewport(Some("-400,-80,400,80"), Some(0.0), None);
        assert!(world.wants(&fiji));
    }

    #[test]
    fn any_area_in_a_merged_event_is_enough() {
        let v = viewport(Some("2.2,48.8,2.5,48.9"), Some(12.0), None);
        let tokyo = area([139.7, 35.6, 139.71, 35.61], 0, 14);
        assert!(!v.wants(&update("basemap", vec![tokyo])));
        assert!(v.wants(&update("basemap", vec![tokyo, paris()])));
    }

    #[test]
    fn rejects_bad_queries() {
        let known = ["basemap"];
        for (bbox, zoom, source) in [
            (Some("1,2,3"), None, None),
            (Some("a,b,c,d"), None, None),
            (Some("0,10,1,5"), None, None),
            (Some("0,-91,1,5"), None, None),
            (Some("0,0,inf,1"), None, None),
            (None, Some(-1.0), None),
            (None, Some(31.0), None),
            (None, None, Some("ghost")),
        ] {
            assert!(
                Viewport::parse(&query(bbox, zoom, source), known).is_err(),
                "{bbox:?} {zoom:?} {source:?} should be rejected"
            );
        }
    }

    #[test]
    fn normalizes_longitudes() {
        assert_eq!(normalize_lon_range(10.0, 20.0), vec![(10.0, 20.0)]);
        assert_eq!(normalize_lon_range(370.0, 380.0), vec![(10.0, 20.0)]);
        assert_eq!(
            normalize_lon_range(170.0, -170.0),
            vec![(170.0, 180.0), (-180.0, -170.0)]
        );
        assert_eq!(normalize_lon_range(-200.0, 200.0), vec![(-180.0, 180.0)]);
    }
}
