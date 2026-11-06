use serde::Serialize;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// A region an incremental update changed: the bounding box of a feature's new or
/// old geometry, and the zooms at which its layer is encoded. Clients use it (and
/// the server's `/events?bbox=…&zoom=…` filter does) to tell whether a change is
/// visible in a given viewport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ChangedArea {
    /// `[west, south, east, north]` in degrees (EPSG:4326)
    pub bbox: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
}

impl ChangedArea {
    pub fn new(bounds: &crate::postgis::Bounds, min_zoom: u8, max_zoom: u8) -> Self {
        ChangedArea {
            bbox: [
                bounds.min_lon,
                bounds.min_lat,
                bounds.max_lon,
                bounds.max_lat,
            ],
            min_zoom,
            max_zoom,
        }
    }

    fn union(a: &ChangedArea, b: &ChangedArea) -> ChangedArea {
        ChangedArea {
            bbox: [
                a.bbox[0].min(b.bbox[0]),
                a.bbox[1].min(b.bbox[1]),
                a.bbox[2].max(b.bbox[2]),
                a.bbox[3].max(b.bbox[3]),
            ],
            min_zoom: a.min_zoom.min(b.min_zoom),
            max_zoom: a.max_zoom.max(b.max_zoom),
        }
    }
}

/// Most areas one event carries. Past this they are merged into one box per zoom
/// range (then into a single box), so a large batch or a long cooldown can't grow
/// the payload without bound — at the cost of a coarser, never narrower, area.
pub const MAX_CHANGED_AREAS: usize = 64;

/// Shrink `areas` to at most [`MAX_CHANGED_AREAS`], only ever growing the region
/// they cover.
fn compact_areas(areas: &mut Vec<ChangedArea>) {
    // A feature edited in several merged batches repeats its box; drop the
    // repeats wherever they are before deciding whether to coarsen.
    areas.sort_by(|a, b| {
        a.bbox
            .iter()
            .zip(&b.bbox)
            .map(|(x, y)| x.total_cmp(y))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.min_zoom.cmp(&b.min_zoom))
            .then(a.max_zoom.cmp(&b.max_zoom))
    });
    areas.dedup();
    if areas.len() <= MAX_CHANGED_AREAS {
        return;
    }
    let mut by_zoom: Vec<ChangedArea> = Vec::new();
    for area in areas.iter() {
        match by_zoom
            .iter_mut()
            .find(|a| a.min_zoom == area.min_zoom && a.max_zoom == area.max_zoom)
        {
            Some(existing) => *existing = ChangedArea::union(existing, area),
            None => by_zoom.push(*area),
        }
    }
    if by_zoom.len() > MAX_CHANGED_AREAS {
        let all = by_zoom
            .iter()
            .skip(1)
            .fold(by_zoom[0], |acc, a| ChangedArea::union(&acc, a));
        by_zoom = vec![all];
    }
    *areas = by_zoom;
}

/// Events emitted by the tile pipeline
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TileEvent {
    /// Full generation completed for a source
    GenerateComplete { source: String, duration_ms: u64 },
    /// Incremental update completed for a source
    UpdateComplete {
        source: String,
        tiles_updated: usize,
        affected_zooms: Vec<u8>,
        /// The source's configured max zoom — tiles at this level are overzoomed
        /// by clients at higher zoom levels (e.g. z14 tiles shown at z15-z22),
        /// so the frontend should also invalidate overzoomed views.
        max_zoom: u8,
        layers_affected: Vec<String>,
        /// Where the change happened. Empty means unknown — treat it as everywhere.
        areas: Vec<ChangedArea>,
    },
}

impl TileEvent {
    /// The source name this event belongs to
    pub fn source(&self) -> &str {
        match self {
            TileEvent::GenerateComplete { source, .. } => source,
            TileEvent::UpdateComplete { source, .. } => source,
        }
    }

    /// Create an UpdateComplete event from tile update results
    pub fn update_complete(
        source: String,
        tiles_updated: usize,
        zooms: &HashSet<u8>,
        max_zoom: u8,
        layers: Vec<String>,
    ) -> Self {
        let mut affected_zooms: Vec<u8> = zooms.iter().copied().collect();
        affected_zooms.sort();
        Self::UpdateComplete {
            source,
            tiles_updated,
            affected_zooms,
            max_zoom,
            layers_affected: layers,
            areas: Vec::new(),
        }
    }

    /// Attach the changed areas to an `UpdateComplete` event (no-op otherwise).
    pub fn with_areas(mut self, new_areas: Vec<ChangedArea>) -> Self {
        if let TileEvent::UpdateComplete { areas, .. } = &mut self {
            *areas = new_areas;
            compact_areas(areas);
        }
        self
    }

    /// Merge another event into this one (accumulates tiles, zooms, layers).
    /// GenerateComplete always wins over UpdateComplete. If both are GenerateComplete,
    /// the latest duration is kept.
    pub fn merge(&mut self, other: &TileEvent) {
        match (self, other) {
            // Accumulate update events
            (
                TileEvent::UpdateComplete {
                    tiles_updated,
                    affected_zooms,
                    layers_affected,
                    areas,
                    ..
                },
                TileEvent::UpdateComplete {
                    tiles_updated: other_tiles,
                    affected_zooms: other_zooms,
                    layers_affected: other_layers,
                    areas: other_areas,
                    ..
                },
            ) => {
                // An event with no areas means "anywhere"; merging keeps that.
                if areas.is_empty() || other_areas.is_empty() {
                    areas.clear();
                } else {
                    areas.extend(other_areas.iter().copied());
                    compact_areas(areas);
                }
                *tiles_updated += other_tiles;
                let mut zoom_set: HashSet<u8> = affected_zooms.iter().copied().collect();
                zoom_set.extend(other_zooms);
                *affected_zooms = zoom_set.into_iter().collect();
                affected_zooms.sort();
                let mut layer_set: HashSet<String> = layers_affected.drain(..).collect();
                layer_set.extend(other_layers.iter().cloned());
                *layers_affected = layer_set.into_iter().collect();
                layers_affected.sort();
            }
            // Generate replaces update
            (
                this @ TileEvent::UpdateComplete { .. },
                other @ TileEvent::GenerateComplete { .. },
            ) => {
                *this = other.clone();
            }
            // Latest generate wins
            (
                TileEvent::GenerateComplete { duration_ms, .. },
                TileEvent::GenerateComplete {
                    duration_ms: other_dur,
                    ..
                },
            ) => {
                *duration_ms = *other_dur;
            }
            // Generate stays over update (already the stronger event)
            (TileEvent::GenerateComplete { .. }, TileEvent::UpdateComplete { .. }) => {}
        }
    }
}

/// An event as broadcast on the bus, numbered in send order (from 1).
#[derive(Debug, Clone)]
pub struct Sequenced {
    pub id: u64,
    pub event: TileEvent,
}

/// How many recent events the bus keeps for SSE clients that reconnect.
pub const EVENT_HISTORY: usize = 256;

/// What a new subscriber gets: the retained events after the id it asked
/// for, a receiver for everything after those, and whether the history could
/// cover the request.
pub struct Subscription {
    pub replay: Vec<Sequenced>,
    pub rx: broadcast::Receiver<Sequenced>,
    /// Id of the newest event sent so far (0 before the first).
    pub latest_id: u64,
    /// `false` when the requested id is older than the retained history or
    /// newer than any id sent (the server restarted): events may be missing.
    pub complete: bool,
}

struct History {
    latest_id: u64,
    recent: VecDeque<Sequenced>,
}

/// Broadcast bus for SSE and webhook consumers. Every event gets an id, and
/// the last [`EVENT_HISTORY`] are kept so an SSE client that reconnects (a map
/// re-subscribing for a new viewport) can catch up on what it missed.
pub struct EventBus {
    tx: broadcast::Sender<Sequenced>,
    history: Mutex<History>,
}

impl EventBus {
    pub fn send(&self, event: TileEvent) -> Result<usize, broadcast::error::SendError<Sequenced>> {
        // Numbering, recording and broadcasting happen under one lock, so a
        // concurrent `subscribe_since` sees each event in exactly one of its
        // replay or its receiver.
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        history.latest_id += 1;
        let sequenced = Sequenced {
            id: history.latest_id,
            event,
        };
        if history.recent.len() == EVENT_HISTORY {
            history.recent.pop_front();
        }
        history.recent.push_back(sequenced.clone());
        self.tx.send(sequenced)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Sequenced> {
        self.tx.subscribe()
    }

    /// Subscribe, replaying the retained events with an id above `since`.
    pub fn subscribe_since(&self, since: Option<u64>) -> Subscription {
        let history = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        let latest_id = history.latest_id;
        let Some(since) = since else {
            return Subscription {
                replay: Vec::new(),
                rx,
                latest_id,
                complete: true,
            };
        };
        let oldest_retained = history.recent.front().map_or(latest_id + 1, |e| e.id);
        let complete = since <= latest_id && since + 1 >= oldest_retained;
        let replay = history
            .recent
            .iter()
            .filter(|e| e.id > since)
            .cloned()
            .collect();
        Subscription {
            replay,
            rx,
            latest_id,
            complete,
        }
    }
}

/// Shared handle to the event bus.
pub type EventSender = Arc<EventBus>;

pub fn create_event_bus() -> EventSender {
    let (tx, _) = broadcast::channel(EVENT_HISTORY);
    Arc::new(EventBus {
        tx,
        history: Mutex::new(History {
            latest_id: 0,
            recent: VecDeque::with_capacity(EVENT_HISTORY),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_bus_send_receive() {
        let tx = create_event_bus();
        let mut rx = tx.subscribe();

        tx.send(TileEvent::GenerateComplete {
            source: "test".to_string(),
            duration_ms: 100,
        })
        .unwrap();

        let event = rx.try_recv().unwrap().event;
        match event {
            TileEvent::GenerateComplete {
                source,
                duration_ms,
            } => {
                assert_eq!(source, "test");
                assert_eq!(duration_ms, 100);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_update_complete_sorts_zooms() {
        let mut zooms = HashSet::new();
        zooms.insert(14u8);
        zooms.insert(2);
        zooms.insert(8);

        let event = TileEvent::update_complete(
            "src".to_string(),
            42,
            &zooms,
            14,
            vec!["layer1".to_string()],
        );

        match event {
            TileEvent::UpdateComplete {
                affected_zooms,
                tiles_updated,
                ..
            } => {
                assert_eq!(affected_zooms, vec![2, 8, 14]);
                assert_eq!(tiles_updated, 42);
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_event_serialization_generate() {
        let event = TileEvent::GenerateComplete {
            source: "parks".to_string(),
            duration_ms: 5000,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["event"], "generate_complete");
        assert_eq!(parsed["source"], "parks");
        assert_eq!(parsed["duration_ms"], 5000);
    }

    #[test]
    fn test_event_serialization_update() {
        let mut zooms = HashSet::new();
        zooms.insert(10u8);

        let event = TileEvent::update_complete(
            "basemap".to_string(),
            7,
            &zooms,
            14,
            vec!["buildings".to_string(), "roads".to_string()],
        );
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["event"], "update_complete");
        assert_eq!(parsed["source"], "basemap");
        assert_eq!(parsed["tiles_updated"], 7);
        assert_eq!(parsed["affected_zooms"], serde_json::json!([10]));
    }

    fn generated(source: &str) -> TileEvent {
        TileEvent::GenerateComplete {
            source: source.into(),
            duration_ms: 1,
        }
    }

    #[test]
    fn test_subscribe_since_replays_missed_events() {
        let bus = create_event_bus();
        let _keep_open = bus.subscribe();
        for name in ["a", "b", "c"] {
            bus.send(generated(name)).unwrap();
        }

        let sub = bus.subscribe_since(Some(1));
        assert!(sub.complete);
        assert_eq!(sub.latest_id, 3);
        let ids: Vec<u64> = sub.replay.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 3]);

        // Nothing replayed twice: the receiver starts after the replay.
        let mut rx = sub.rx;
        bus.send(generated("d")).unwrap();
        assert_eq!(rx.try_recv().unwrap().id, 4);
        assert!(rx.try_recv().is_err());

        // Up to date, and no `since` at all.
        let sub = bus.subscribe_since(Some(4));
        assert!(sub.complete && sub.replay.is_empty());
        let sub = bus.subscribe_since(None);
        assert!(sub.complete && sub.replay.is_empty());
    }

    #[test]
    fn test_subscribe_since_reports_gaps() {
        let bus = create_event_bus();
        let _keep_open = bus.subscribe();
        for _ in 0..EVENT_HISTORY + 10 {
            bus.send(generated("a")).unwrap();
        }
        // Expired from the history
        assert!(!bus.subscribe_since(Some(5)).complete);
        // Oldest retained is 11, so asking from 10 is still complete
        assert!(bus.subscribe_since(Some(10)).complete);
        // An id this bus never issued: the server restarted
        assert!(!bus.subscribe_since(Some(10_000)).complete);
    }

    #[test]
    fn test_multiple_subscribers() {
        let tx = create_event_bus();
        let mut rx1 = tx.subscribe();
        let mut rx2 = tx.subscribe();

        tx.send(TileEvent::GenerateComplete {
            source: "test".to_string(),
            duration_ms: 50,
        })
        .unwrap();

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn test_source_accessor() {
        let e1 = TileEvent::GenerateComplete {
            source: "parks".to_string(),
            duration_ms: 100,
        };
        assert_eq!(e1.source(), "parks");

        let e2 = TileEvent::update_complete("basemap".to_string(), 5, &HashSet::new(), 14, vec![]);
        assert_eq!(e2.source(), "basemap");
    }

    #[test]
    fn test_merge_update_events() {
        let mut e1 = TileEvent::update_complete(
            "src".to_string(),
            10,
            &[5u8, 6].into_iter().collect(),
            14,
            vec!["roads".to_string()],
        );
        let e2 = TileEvent::update_complete(
            "src".to_string(),
            20,
            &[6u8, 7].into_iter().collect(),
            14,
            vec!["buildings".to_string()],
        );
        e1.merge(&e2);

        match e1 {
            TileEvent::UpdateComplete {
                tiles_updated,
                affected_zooms,
                layers_affected,
                ..
            } => {
                assert_eq!(tiles_updated, 30);
                assert_eq!(affected_zooms, vec![5, 6, 7]);
                assert!(layers_affected.contains(&"roads".to_string()));
                assert!(layers_affected.contains(&"buildings".to_string()));
            }
            _ => panic!("Wrong type"),
        }
    }

    #[test]
    fn test_merge_generate_replaces_update() {
        let mut e1 = TileEvent::update_complete(
            "src".to_string(),
            10,
            &[5u8].into_iter().collect(),
            14,
            vec!["roads".to_string()],
        );
        let e2 = TileEvent::GenerateComplete {
            source: "src".to_string(),
            duration_ms: 500,
        };
        e1.merge(&e2);

        match e1 {
            TileEvent::GenerateComplete { duration_ms, .. } => {
                assert_eq!(duration_ms, 500);
            }
            _ => panic!("Should be GenerateComplete after merge"),
        }
    }

    #[test]
    fn test_merge_generate_keeps_over_update() {
        let mut e1 = TileEvent::GenerateComplete {
            source: "src".to_string(),
            duration_ms: 500,
        };
        let e2 = TileEvent::update_complete(
            "src".to_string(),
            10,
            &[5u8].into_iter().collect(),
            14,
            vec!["roads".to_string()],
        );
        e1.merge(&e2);

        // Generate should remain
        match e1 {
            TileEvent::GenerateComplete { duration_ms, .. } => {
                assert_eq!(duration_ms, 500);
            }
            _ => panic!("Should still be GenerateComplete"),
        }
    }

    fn area(w: f64, s: f64, e: f64, n: f64, min_zoom: u8, max_zoom: u8) -> ChangedArea {
        ChangedArea {
            bbox: [w, s, e, n],
            min_zoom,
            max_zoom,
        }
    }

    #[test]
    fn test_areas_serialize_and_merge() {
        let mut e1 = TileEvent::update_complete("src".into(), 1, &HashSet::new(), 14, vec![])
            .with_areas(vec![area(1.0, 2.0, 3.0, 4.0, 0, 14)]);
        let json: serde_json::Value = serde_json::to_value(&e1).unwrap();
        assert_eq!(
            json["areas"],
            serde_json::json!([{"bbox": [1.0, 2.0, 3.0, 4.0], "min_zoom": 0, "max_zoom": 14}])
        );

        let e2 = TileEvent::update_complete("src".into(), 1, &HashSet::new(), 14, vec![])
            .with_areas(vec![area(10.0, 10.0, 11.0, 11.0, 12, 14)]);
        e1.merge(&e2);
        match &e1 {
            TileEvent::UpdateComplete { areas, .. } => assert_eq!(areas.len(), 2),
            _ => panic!("Wrong type"),
        }

        // Merging an event of unknown extent widens the result to "anywhere".
        let unknown = TileEvent::update_complete("src".into(), 1, &HashSet::new(), 14, vec![]);
        e1.merge(&unknown);
        match &e1 {
            TileEvent::UpdateComplete { areas, .. } => assert!(areas.is_empty()),
            _ => panic!("Wrong type"),
        }
    }

    #[test]
    fn test_areas_are_compacted_without_shrinking() {
        let many: Vec<ChangedArea> = (0..200)
            .map(|i| {
                let x = i as f64 * 0.1;
                area(x, x, x + 0.05, x + 0.05, (i % 3) as u8, 14)
            })
            .collect();
        let e = TileEvent::update_complete("src".into(), 1, &HashSet::new(), 14, vec![])
            .with_areas(many.clone());
        let TileEvent::UpdateComplete { areas, .. } = e else {
            panic!("Wrong type")
        };
        assert!(areas.len() <= MAX_CHANGED_AREAS);
        // Every original area is still covered, at its zooms.
        for a in &many {
            assert!(areas.iter().any(|c| c.bbox[0] <= a.bbox[0]
                && c.bbox[1] <= a.bbox[1]
                && c.bbox[2] >= a.bbox[2]
                && c.bbox[3] >= a.bbox[3]
                && c.min_zoom <= a.min_zoom
                && c.max_zoom >= a.max_zoom));
        }
    }

    #[test]
    fn test_merge_two_generates() {
        let mut e1 = TileEvent::GenerateComplete {
            source: "src".to_string(),
            duration_ms: 100,
        };
        let e2 = TileEvent::GenerateComplete {
            source: "src".to_string(),
            duration_ms: 999,
        };
        e1.merge(&e2);

        match e1 {
            TileEvent::GenerateComplete { duration_ms, .. } => {
                assert_eq!(duration_ms, 999);
            }
            _ => panic!("Wrong type"),
        }
    }
}
