//! Event structures for the dispatch pipeline.
//!
//! Implements:
//! - `RouteTarget`: event routing decision from the event router
//! - `SceneLocalPatch`: local-state updates from Stage 2 (compositor-applied)

use serde::{Deserialize, Serialize};
use tze_hud_scene::SceneId;

// ─── Route target ─────────────────────────────────────────────────────────────

/// Resolved routing target for an event after hit-test.
///
/// Produced by the event router.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RouteTarget {
    /// Route to the lease owner of the given tile.
    Agent {
        /// Namespace (agent name) of the lease owner.
        namespace: String,
        /// The tile being addressed.
        tile_id: SceneId,
    },
    /// Chrome event; handled locally, no agent routing.
    ChromeLocal,
    /// Pass through to the desktop (overlay mode) or discard (fullscreen).
    Passthrough,
}

// ─── SceneLocalPatch ──────────────────────────────────────────────────────────

/// A per-node local state update, produced by Stage 2 (Local Feedback).
///
/// Local Feedback Rendering via SceneLocalPatch.
/// Applied by the compositor in Stage 4 without lease validation or budget
/// checks.
///
/// The three runtime-owned boolean state bits:
/// - `pressed` — set on PointerDown, cleared on PointerUp (or rollback)
/// - `hovered` — set on PointerEnter, cleared on PointerLeave
/// - `focused` — set on focus acquisition, cleared on focus loss
///
/// `rollback=true` signals a 100ms reverse animation on agent explicit rejection
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalStateUpdate {
    /// The node whose local state changed.
    pub node_id: SceneId,
    /// New pressed state. `None` = unchanged.
    pub pressed: Option<bool>,
    /// New hovered state. `None` = unchanged.
    pub hovered: Option<bool>,
    /// New focused state. `None` = unchanged.
    pub focused: Option<bool>,
    /// If true, this update initiates a 100ms reverse rollback animation
    /// (agent explicitly rejected the interaction).
    #[serde(default)]
    pub rollback: bool,
}

impl LocalStateUpdate {
    /// Construct a simple state update with no rollback.
    pub fn new(node_id: SceneId) -> Self {
        Self {
            node_id,
            pressed: None,
            hovered: None,
            focused: None,
            rollback: false,
        }
    }

    /// Set pressed state and return self for chaining.
    pub fn with_pressed(mut self, pressed: bool) -> Self {
        self.pressed = Some(pressed);
        self
    }

    /// Set hovered state and return self for chaining.
    pub fn with_hovered(mut self, hovered: bool) -> Self {
        self.hovered = Some(hovered);
        self
    }

    /// Mark this update as a rollback (pressed → false with 100ms animation).
    pub fn with_rollback(mut self) -> Self {
        self.rollback = true;
        self
    }
}

/// A per-tile scroll offset update, produced by Stage 2.
///
/// Carries the **absolute** post-update scroll offset for the tile, per
/// Local Feedback Rendering via SceneLocalPatch:
/// `ScrollOffsetUpdate(tile_id, offset_x, offset_y)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollOffsetUpdate {
    /// The tile whose scroll offset changed.
    pub tile_id: SceneId,
    /// New absolute horizontal scroll offset (pixels from content origin).
    pub offset_x: f32,
    /// New absolute vertical scroll offset (pixels from content origin).
    pub offset_y: f32,
    /// Origin — `true` = user input, `false` = agent request.
    #[serde(default)]
    pub user_initiated: bool,
}

/// Batch of local state changes produced during Stage 2 Local Feedback.
///
/// Forwarded to the compositor on the local-feedback channel and applied
/// before render encoding.
///
/// ## Latency invariant
/// Must be produced within 1ms of the input event (combined Stage 1+2 budget).
/// The compositor must apply it before the next frame (< 33ms guarantee).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SceneLocalPatch {
    /// Per-node state updates (pressed, hovered, focused).
    pub node_updates: Vec<LocalStateUpdate>,
    /// Per-tile scroll offset updates.
    pub scroll_updates: Vec<ScrollOffsetUpdate>,
}

impl SceneLocalPatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if this patch contains no changes.
    pub fn is_empty(&self) -> bool {
        self.node_updates.is_empty() && self.scroll_updates.is_empty()
    }

    /// Add a node state update.
    pub fn push_state(&mut self, update: LocalStateUpdate) {
        self.node_updates.push(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_local_patch_empty_by_default() {
        let patch = SceneLocalPatch::new();
        assert!(patch.is_empty());
    }
}
