//! Keyboard focus-ring owner plumbing for the chrome-layer ring pass.
//!
//! The runtime's `FocusManager` owns keyboard focus; the compositor draws the
//! ring. Node-level focus is visible to the compositor via
//! `hit_region_states.focused`, but a **tile-level** focus stop (a non-passthrough
//! tile with no focusable nodes) has no scene representation, so the compositor
//! cannot see it. Rather than special-case only what the scene exposes, the
//! runtime plumbs the current focus owner here and the compositor draws the ring
//! for ANY owner — tile-level or node, in any tile — in a single chrome-layer
//! pass above all agent content (input-model ).

use std::sync::{Arc, Mutex as StdMutex};

use tze_hud_scene::types::SceneId;

/// The current keyboard-focus owner, plumbed from the runtime once per frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusRingOwner {
    /// The tab the focus lives on; the ring renders only when this is the scene's
    /// active tab.
    pub tab_id: SceneId,
    /// The owning tile (the ring is clipped to this tile's bounds).
    pub tile_id: SceneId,
    /// `Some` for node-level focus (ring around the node); `None` for a
    /// tile-level stop (ring around the whole tile).
    pub node_id: Option<SceneId>,
}

/// Single-slot handle: the runtime writes the current owner (or `None` when focus
/// is cleared) each frame; the compositor reads it to place the ring. Latest-wins
/// — no accumulation, so a simple overwrite-and-read is correct.
pub type FocusRingOwnerHandle = Arc<StdMutex<Option<FocusRingOwner>>>;
