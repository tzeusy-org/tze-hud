//! Runtime-authored viewer reply echo for raw-tile portals (hud-nx7yq.3).
//!
//! On a raw-tile pilot portal the transcript is adapter-owned (the exemplar
//! publishes its own transcript node over `HudSession`), and the tile is not
//! attached to the [`ProjectionAuthority`], so the authority-path
//! `append_viewer_echo` never fires. Without this, an accepted composer
//! submission clears the composer and the viewer's words vanish from the surface
//! ("whenever I press Enter my text disappears").
//!
//! This module is the runtime-authored equivalent the spec permits
//! (§Pilot-Path Viewer History): the windowed runtime pushes a
//! [`ViewerEchoAppend`] onto a shared queue at submit time, and the compositor
//! drains it into a per-tile bounded store rendered as kind-distinct lines just
//! above the composer input strip. It is pure local presentation — it never
//! touches unread counts or attention/interruption state, and an adapter still
//! cannot forge it (the adapter output-publication path rejects the viewer kind,
//! unchanged).
//!
//! [`ProjectionAuthority`]: https://docs.rs/tze_hud_projection

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};

use tze_hud_scene::types::SceneId;

/// Maximum retained viewer echo entries per portal tile.
///
/// Bounds the runtime-authored viewer history window so an unbounded reply
/// stream cannot grow the overlay without limit (mirrors the Bounded Transcript
/// Viewport principle for the pilot slice).
pub(crate) const MAX_VIEWER_ECHO_ENTRIES: usize = 8;

/// A runtime-authored viewer reply, addressed to a specific portal tile, pushed
/// onto the shared queue at submit time by the windowed runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerEchoAppend {
    /// The portal tile whose transcript window receives the entry.
    pub tile_id: SceneId,
    /// The submitted reply text (already the accepted submission's content).
    pub text: String,
    /// Wall-clock submit time in microseconds (for ordering / display only).
    pub submitted_at_wall_us: u64,
}

/// Shared append queue between the runtime (writer) and compositor (drainer).
///
/// The runtime pushes [`ViewerEchoAppend`]s on accepted raw-tile submissions;
/// the compositor drains them into its [`ViewerEchoStore`] once per frame,
/// mirroring the single-slot `LocalComposerStateHandle` hand-off pattern but
/// accumulating (append) rather than latest-wins.
pub type PortalViewerEchoQueue = Arc<StdMutex<Vec<ViewerEchoAppend>>>;

/// One retained viewer echo entry within a tile's transcript window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ViewerEchoEntry {
    /// The viewer-authored reply text.
    pub(crate) text: String,
    /// Wall-clock submit time in microseconds.
    pub(crate) submitted_at_wall_us: u64,
}

/// Per-tile bounded store of runtime-authored viewer echo entries.
///
/// Entries are retained oldest-first; appends beyond [`MAX_VIEWER_ECHO_ENTRIES`]
/// evict the oldest so the window stays bounded.
#[derive(Debug, Default)]
pub(crate) struct ViewerEchoStore {
    by_tile: HashMap<SceneId, VecDeque<ViewerEchoEntry>>,
}

impl ViewerEchoStore {
    /// Construct an empty store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append a viewer entry for `tile_id`, evicting the oldest beyond the bound.
    pub(crate) fn append(&mut self, tile_id: SceneId, text: String, submitted_at_wall_us: u64) {
        let queue = self.by_tile.entry(tile_id).or_default();
        queue.push_back(ViewerEchoEntry {
            text,
            submitted_at_wall_us,
        });
        while queue.len() > MAX_VIEWER_ECHO_ENTRIES {
            queue.pop_front();
        }
    }

    /// Drain a shared append queue into this store (called once per frame).
    pub(crate) fn drain_queue(&mut self, queue: &PortalViewerEchoQueue) {
        let Ok(mut pending) = queue.lock() else {
            return;
        };
        for append in pending.drain(..) {
            self.append(append.tile_id, append.text, append.submitted_at_wall_us);
        }
    }

    /// Drop entries for tiles for which `is_alive` returns `false`.
    ///
    /// Called each frame with the live tile set so echoes for destroyed portals
    /// do not linger.
    pub(crate) fn retain_tiles(&mut self, is_alive: impl Fn(SceneId) -> bool) {
        self.by_tile.retain(|tile_id, _| is_alive(*tile_id));
    }

    /// Retained entries for a tile (oldest first), or `None` when the tile has
    /// no viewer echoes.
    pub(crate) fn entries_for(&self, tile_id: SceneId) -> Option<&VecDeque<ViewerEchoEntry>> {
        self.by_tile.get(&tile_id).filter(|q| !q.is_empty())
    }

    /// True when no tile has any retained entries.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_tile.values().all(VecDeque::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_retains_entries_oldest_first() {
        let mut store = ViewerEchoStore::new();
        let tile = SceneId::new();
        store.append(tile, "first".into(), 10);
        store.append(tile, "second".into(), 20);

        let entries = store.entries_for(tile).expect("tile has entries");
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["first", "second"],
            "entries retained oldest-first"
        );
    }

    #[test]
    fn append_evicts_oldest_beyond_bound() {
        let mut store = ViewerEchoStore::new();
        let tile = SceneId::new();
        for i in 0..(MAX_VIEWER_ECHO_ENTRIES + 3) {
            store.append(tile, format!("m{i}"), i as u64);
        }
        let entries = store.entries_for(tile).unwrap();
        assert_eq!(
            entries.len(),
            MAX_VIEWER_ECHO_ENTRIES,
            "store stays bounded"
        );
        // Oldest three (m0,m1,m2) evicted; newest survives.
        assert_eq!(entries.front().unwrap().text, "m3");
        assert_eq!(entries.back().unwrap().text, "m10");
    }

    #[test]
    fn drain_queue_moves_pending_appends_into_store() {
        let mut store = ViewerEchoStore::new();
        let tile = SceneId::new();
        let queue: PortalViewerEchoQueue = Arc::new(StdMutex::new(Vec::new()));
        queue.lock().unwrap().push(ViewerEchoAppend {
            tile_id: tile,
            text: "hi".into(),
            submitted_at_wall_us: 1,
        });

        store.drain_queue(&queue);

        assert_eq!(store.entries_for(tile).unwrap().back().unwrap().text, "hi");
        assert!(
            queue.lock().unwrap().is_empty(),
            "queue drained after draining into the store"
        );
    }

    #[test]
    fn retain_tiles_drops_dead_tiles() {
        let mut store = ViewerEchoStore::new();
        let live = SceneId::new();
        let dead = SceneId::new();
        store.append(live, "keep".into(), 1);
        store.append(dead, "drop".into(), 2);

        store.retain_tiles(|tid| tid == live);

        assert!(store.entries_for(live).is_some());
        assert!(store.entries_for(dead).is_none());
    }

    #[test]
    fn entries_for_absent_tile_is_none() {
        let store = ViewerEchoStore::new();
        assert!(store.entries_for(SceneId::new()).is_none());
        assert!(store.is_empty());
    }
}
