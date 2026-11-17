//! Pointer capture manager.
//!
//! — pointer capture semantics:
//!
//! - Only one node holds pointer capture at a time **globally across the entire
//!   scene**, keyed by pointer device (`device_id`).
//! - Capture can be acquired explicitly via `acquire()` or automatically by the
//!   InputProcessor when a `HitRegionNode` with `auto_capture=true` receives a
//!   `PointerDownEvent`.
//! - While capture is active, all pointer events from the captured device are
//!   routed to the capturing node, bypassing normal hit-testing.
//! - Capture is released by: explicit `release()`, `release_on_up` logic in
//!   the InputProcessor, or runtime theft via `InputProcessor::steal_capture()`.
//!
//! ## Semantics enforced here
//!
//! - `acquire()` returns `Err(CaptureError::AlreadyCaptured)` if another node
//!   already holds capture for the same `device_id`.  Only one node per device
//!   at a time.
//! - `get()` returns the current capture state for a device, or `None`.
//! - `release()` clears the capture entry (idempotent).

use std::collections::HashMap;
use tze_hud_scene::SceneId;

/// Snapshot of the current capture state for a single pointer device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureState {
    /// The tile whose node holds capture.
    pub tile_id: SceneId,
    /// The node that holds capture.
    pub node_id: SceneId,
    /// Whether capture is released automatically on PointerUpEvent.
    pub release_on_up: bool,
}

/// Error returned when an `acquire()` attempt is denied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// Another node already holds capture for this device.
    AlreadyCaptured,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::AlreadyCaptured => write!(
                f,
                "another node already holds pointer capture for this device"
            ),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Global pointer capture registry.
///
/// Keyed by `device_id` (u32).  At most one [`CaptureState`] per device at
/// any time.  This struct is owned by [`crate::InputProcessor`] and operated
/// only through the processor's public API — do not modify the map directly.
#[derive(Debug, Default)]
pub struct PointerCaptureManager {
    captures: HashMap<u32, CaptureState>,
}

impl PointerCaptureManager {
    /// Create a new, empty capture manager.
    pub fn new() -> Self {
        Self {
            captures: HashMap::new(),
        }
    }

    /// Attempt to acquire pointer capture for `device_id` on behalf of
    /// `(tile_id, node_id)`.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::AlreadyCaptured`] if `device_id` already has an
    /// active capture entry (regardless of which node holds it).
    pub fn acquire(
        &mut self,
        device_id: u32,
        tile_id: SceneId,
        node_id: SceneId,
        release_on_up: bool,
    ) -> Result<(), CaptureError> {
        if self.captures.contains_key(&device_id) {
            return Err(CaptureError::AlreadyCaptured);
        }
        self.captures.insert(
            device_id,
            CaptureState {
                tile_id,
                node_id,
                release_on_up,
            },
        );
        Ok(())
    }

    /// Return the current capture state for `device_id`, or `None` if no
    /// capture is active.
    pub fn get(&self, device_id: u32) -> Option<&CaptureState> {
        self.captures.get(&device_id)
    }

    /// Release capture for `device_id`.  Idempotent — safe to call even if no
    /// capture is active.
    pub fn release(&mut self, device_id: u32) {
        self.captures.remove(&device_id);
    }

    /// Return `true` if `device_id` has an active capture.
    pub fn is_captured(&self, device_id: u32) -> bool {
        self.captures.contains_key(&device_id)
    }

    /// Number of active captures across all devices.
    pub fn active_count(&self) -> usize {
        self.captures.len()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::SceneId;

    fn node_a() -> (SceneId, SceneId) {
        (SceneId::new(), SceneId::new())
    }

    #[test]
    fn acquire_succeeds_when_no_capture_active() {
        let mut mgr = PointerCaptureManager::new();
        let (tile, node) = node_a();
        assert!(mgr.acquire(0, tile, node, false).is_ok());
        assert!(mgr.is_captured(0));
    }

    #[test]
    fn acquire_denied_when_device_already_captured() {
        let mut mgr = PointerCaptureManager::new();
        let (tile1, node1) = node_a();
        let (tile2, node2) = node_a();
        mgr.acquire(0, tile1, node1, false).unwrap();
        // Second node tries to capture the same device
        let err = mgr.acquire(0, tile2, node2, false).unwrap_err();
        assert_eq!(err, CaptureError::AlreadyCaptured);
        // Original capture unchanged
        let state = mgr.get(0).unwrap();
        assert_eq!(state.tile_id, tile1);
        assert_eq!(state.node_id, node1);
    }

    #[test]
    fn different_devices_can_be_captured_independently() {
        let mut mgr = PointerCaptureManager::new();
        let (tile1, node1) = node_a();
        let (tile2, node2) = node_a();
        mgr.acquire(0, tile1, node1, false).unwrap();
        mgr.acquire(1, tile2, node2, true).unwrap();
        assert_eq!(mgr.active_count(), 2);
        assert_eq!(mgr.get(0).unwrap().node_id, node1);
        assert_eq!(mgr.get(1).unwrap().node_id, node2);
    }

    #[test]
    fn release_clears_capture() {
        let mut mgr = PointerCaptureManager::new();
        let (tile, node) = node_a();
        mgr.acquire(0, tile, node, false).unwrap();
        mgr.release(0);
        assert!(!mgr.is_captured(0));
        assert!(mgr.get(0).is_none());
    }

    #[test]
    fn release_is_idempotent() {
        let mut mgr = PointerCaptureManager::new();
        // Release with no capture active — should not panic
        mgr.release(0);
        mgr.release(0);
    }

    #[test]
    fn get_returns_none_when_not_captured() {
        let mgr = PointerCaptureManager::new();
        assert!(mgr.get(0).is_none());
    }

    #[test]
    fn release_on_up_flag_stored_correctly() {
        let mut mgr = PointerCaptureManager::new();
        let (tile, node) = node_a();
        mgr.acquire(0, tile, node, true).unwrap();
        assert!(mgr.get(0).unwrap().release_on_up);
    }

    #[test]
    fn acquire_again_after_release_succeeds() {
        let mut mgr = PointerCaptureManager::new();
        let (tile1, node1) = node_a();
        let (tile2, node2) = node_a();
        mgr.acquire(0, tile1, node1, false).unwrap();
        mgr.release(0);
        // After release, a new node may capture the same device
        assert!(mgr.acquire(0, tile2, node2, false).is_ok());
        assert_eq!(mgr.get(0).unwrap().node_id, node2);
    }
}
