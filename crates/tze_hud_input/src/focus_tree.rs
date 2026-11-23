//! Per-tab focus ownership and bounded history.
//!
//! Each tab maintains an independent focus tree. At most one focus owner exists
//! per tab. This module owns the data structure; `FocusManager` owns the
//! lifecycle / state machine.
//!
//! # Focus owner variants
//! - `FocusOwner::None` — no element focused on this tab
//! - `FocusOwner::ChromeElement(id)` — a chrome UI element holds focus
//! - `FocusOwner::Tile(tile_id)` — a tile holds focus at tile level (no focusable node)
//! - `FocusOwner::Node { tile_id, node_id }` — a specific HitRegionNode holds focus
//!
//! # Invariants
//! - Exactly one `FocusTree` exists per tab.
//! - `FocusTree::current` encodes the single current owner.
//! - `FocusTree::history` is a bounded stack (max depth: `HISTORY_DEPTH`) of
//!   previously focused elements, used for fallback on destruction.
//!

use std::collections::VecDeque;
use tze_hud_scene::SceneId;

/// Maximum depth of per-tab focus history for fallback-on-destruction.
const HISTORY_DEPTH: usize = 8;

/// The current owner of focus in a tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusOwner {
    /// No element is focused.
    None,
    /// A chrome element holds focus (e.g. tab bar, chrome button).
    ChromeElement(SceneId),
    /// A tile holds focus at tile level (no focusable HitRegionNode).
    Tile(SceneId),
    /// A specific HitRegionNode within a tile holds focus.
    Node { tile_id: SceneId, node_id: SceneId },
}

impl FocusOwner {
    /// Returns the tile_id of the current focus owner, if any.
    pub fn tile_id(&self) -> Option<SceneId> {
        match self {
            FocusOwner::Tile(id) => Some(*id),
            FocusOwner::Node { tile_id, .. } => Some(*tile_id),
            _ => None,
        }
    }

    /// Returns the focused node id, or `None` when focus is tile-level, chrome,
    /// or absent.
    pub fn node_id(&self) -> Option<SceneId> {
        match self {
            FocusOwner::Node { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }

    /// Returns true if the focus owner is for the given tile or a node in it.
    pub(crate) fn is_on_tile(&self, tile_id: SceneId) -> bool {
        self.tile_id() == Some(tile_id)
    }
}

/// Per-tab focus state.
///
/// Maintained entirely in memory. Not persisted; survives tab switches by
/// virtue of each tab having its own `FocusTree` in `FocusManager`.
#[derive(Clone, Debug)]
pub(crate) struct FocusTree {
    /// The current focus owner on this tab.
    pub(crate) current: FocusOwner,
    /// Bounded history of prior focus owners (newest at back).
    ///
    /// Used for fallback when the focused tile/node is destroyed.
    /// Implemented as a `VecDeque` for O(1) front-eviction when the bound is hit.
    history: VecDeque<FocusOwner>,
}

impl FocusTree {
    /// Create a new focus tree for a tab, with no initial focus.
    fn new() -> Self {
        Self {
            current: FocusOwner::None,
            history: VecDeque::with_capacity(HISTORY_DEPTH),
        }
    }

    /// Returns the current focus owner (read-only reference).
    pub(crate) fn current(&self) -> &FocusOwner {
        &self.current
    }

    /// Transitions focus to `next`.
    ///
    /// - Records the previous owner in history (bounded by `HISTORY_DEPTH`).
    /// - Does NOT emit events — callers handle event dispatch.
    pub(crate) fn set_focus(&mut self, next: FocusOwner) {
        let prev = std::mem::replace(&mut self.current, next);
        if prev != FocusOwner::None {
            if self.history.len() == HISTORY_DEPTH {
                self.history.pop_front();
            }
            self.history.push_back(prev);
        }
    }

    /// Returns the most-recent prior focus owner (if any).
    ///
    /// Used when the current focus owner is destroyed to fall back to the
    /// previous element.
    #[cfg(test)]
    fn previous(&self) -> Option<&FocusOwner> {
        self.history.back()
    }

    /// Remove `owner` from history (called when a tile/node is destroyed so
    /// we don't fall back to a dead element).
    pub(crate) fn remove_from_history(&mut self, tile_id: SceneId) {
        self.history.retain(|owner| match owner {
            FocusOwner::Tile(id) => *id != tile_id,
            FocusOwner::Node { tile_id: t, .. } => *t != tile_id,
            _ => true,
        });
    }

    /// Pop the most-recent valid history entry that still refers to a live
    /// owner (excluding any entries for `destroyed_tile_id`).
    ///
    /// Returns `FocusOwner::None` if no valid history entry exists.
    pub(crate) fn pop_fallback(&mut self, destroyed_tile_id: SceneId) -> FocusOwner {
        // Walk history from newest to oldest, skipping the destroyed tile.
        while let Some(prev) = self.history.pop_back() {
            match &prev {
                FocusOwner::Tile(id) if *id == destroyed_tile_id => continue,
                FocusOwner::Node { tile_id: t, .. } if *t == destroyed_tile_id => continue,
                _ => return prev,
            }
        }
        FocusOwner::None
    }
}

impl Default for FocusTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id() -> SceneId {
        SceneId::new()
    }

    #[test]
    fn test_initial_state_is_none() {
        let tree = FocusTree::new();
        assert_eq!(*tree.current(), FocusOwner::None);
        assert!(tree.previous().is_none());
    }

    #[test]
    fn test_set_focus_records_history() {
        let mut tree = FocusTree::new();
        let tile_id = make_id();
        let node_id = make_id();

        tree.set_focus(FocusOwner::Node { tile_id, node_id });
        assert_eq!(*tree.current(), FocusOwner::Node { tile_id, node_id });
        // None is not pushed to history.
        assert!(tree.previous().is_none());

        let tile_id2 = make_id();
        tree.set_focus(FocusOwner::Tile(tile_id2));
        assert_eq!(*tree.current(), FocusOwner::Tile(tile_id2));
        assert_eq!(
            tree.previous(),
            Some(&FocusOwner::Node { tile_id, node_id })
        );
    }

    #[test]
    fn test_history_bounded_by_history_depth() {
        let mut tree = FocusTree::new();
        // Push more entries than HISTORY_DEPTH.
        for _ in 0..(HISTORY_DEPTH + 3) {
            let t = make_id();
            tree.set_focus(FocusOwner::Tile(t));
        }
        // History should be capped at HISTORY_DEPTH.
        assert!(tree.history.len() <= HISTORY_DEPTH);
    }

    #[test]
    fn test_pop_fallback_skips_destroyed_tile() {
        let mut tree = FocusTree::new();
        let t1 = make_id();
        let t2 = make_id();
        let n1 = make_id();
        let n2 = make_id();

        tree.set_focus(FocusOwner::Node {
            tile_id: t1,
            node_id: n1,
        });
        tree.set_focus(FocusOwner::Node {
            tile_id: t2,
            node_id: n2,
        });
        tree.set_focus(FocusOwner::Tile(t2)); // second t2 entry

        // Destroy t2; fallback should skip both t2 entries and return t1/n1.
        let fallback = tree.pop_fallback(t2);
        assert_eq!(
            fallback,
            FocusOwner::Node {
                tile_id: t1,
                node_id: n1
            }
        );
    }

    #[test]
    fn test_pop_fallback_returns_none_when_all_history_destroyed() {
        let mut tree = FocusTree::new();
        let t1 = make_id();
        let n1 = make_id();

        tree.set_focus(FocusOwner::Node {
            tile_id: t1,
            node_id: n1,
        });
        tree.set_focus(FocusOwner::Tile(t1));

        let fallback = tree.pop_fallback(t1);
        assert_eq!(fallback, FocusOwner::None);
    }

    #[test]
    fn test_remove_from_history() {
        let mut tree = FocusTree::new();
        let t1 = make_id();
        let t2 = make_id();
        let n1 = make_id();

        tree.set_focus(FocusOwner::Node {
            tile_id: t1,
            node_id: n1,
        });
        tree.set_focus(FocusOwner::Tile(t2));

        tree.remove_from_history(t1);
        // t1 entries should be gone.
        assert!(tree.history.iter().all(|o| !o.is_on_tile(t1)));
    }

    #[test]
    fn test_focus_owner_tile_id() {
        let t = make_id();
        let n = make_id();
        assert_eq!(FocusOwner::None.tile_id(), None);
        assert_eq!(FocusOwner::Tile(t).tile_id(), Some(t));
        assert_eq!(
            FocusOwner::Node {
                tile_id: t,
                node_id: n
            }
            .tile_id(),
            Some(t)
        );
    }
}
