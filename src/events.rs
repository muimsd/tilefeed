use serde::Serialize;
use std::collections::HashSet;
use tokio::sync::broadcast;

/// Events emitted by the tile pipeline
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TileEvent {
    /// Full generation completed for a source
    GenerateComplete {
        source: String,
        duration_ms: u64,
    },
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
        }
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
                    ..
                },
                TileEvent::UpdateComplete {
                    tiles_updated: other_tiles,
                    affected_zooms: other_zooms,
                    layers_affected: other_layers,
                    ..
                },
            ) => {
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
            (this @ TileEvent::UpdateComplete { .. }, other @ TileEvent::GenerateComplete { .. }) => {
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

/// Shared event bus for SSE and webhook consumers
pub type EventSender = broadcast::Sender<TileEvent>;

pub fn create_event_bus() -> EventSender {
    let (tx, _) = broadcast::channel(256);
    tx
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

        let event = rx.try_recv().unwrap();
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

        let e2 = TileEvent::update_complete(
            "basemap".to_string(),
            5,
            &HashSet::new(),
            14,
            vec![],
        );
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
