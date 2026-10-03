//! # Frame pipeline state
//!
//! The lock-free [`HitTestSnapshot`] the compositor thread publishes after each
//! scene commit, the shared timed-content sweep every frame driver runs, and
//! the latency budgets the portal tests assert against.
//!
//! ## ArcSwap Hit-Test Snapshot
//!
//! Local feedback must read tile bounds without taking a mutex. The compositor
//! thread publishes a new [`HitTestSnapshot`] after the scene commit via an
//! [`arc_swap::ArcSwap`]. The main thread loads the snapshot with a
//! pointer-width atomic load (no mutex, no blocking).

use std::sync::Arc;

use arc_swap::ArcSwap;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::Rect;

// ─── Budget constants (microseconds) ──────────────────────────────────────────

/// Stage 3 (Mutation Intake) p99 budget — 1ms.
pub const STAGE3_BUDGET_US: u64 = 1_000;
/// Stage 4 (Scene Commit) p99 budget — 1ms.
pub const STAGE4_BUDGET_US: u64 = 1_000;
/// Stage 5 (Layout Resolve) p99 budget — 1ms.
pub const STAGE5_BUDGET_US: u64 = 1_000;
/// Input-to-next-present p99 budget — 33ms (two frames at 60fps).
pub const INPUT_TO_NEXT_PRESENT_BUDGET_US: u64 = 33_000;

// ─── Hit-Test Snapshot ────────────────────────────────────────────────────────

/// A snapshot of tile bounds used for lock-free hit-testing in Stage 2.
///
/// Published by the compositor thread after Stage 4 (Scene Commit) via
/// [`ArcSwap`]. The main thread loads this atomically (pointer-width load,
/// no mutex).
#[derive(Clone, Debug)]
pub struct HitTestSnapshot {
    /// Sorted (by z-order descending) list of (tile_id_bytes, bounds) pairs.
    /// Using raw bytes avoids a SceneId dependency in snapshot loading.
    pub tiles: Vec<TileBoundsEntry>,
    /// Runtime chrome drag-handle bounds used by the main-thread input path.
    pub drag_handles: Vec<DragHandleBoundsEntry>,
}

/// One entry in the hit-test snapshot.
#[derive(Clone, Debug)]
pub struct TileBoundsEntry {
    /// Tile UUID bytes (128-bit).
    pub tile_id_bytes: [u8; 16],
    /// Tile bounds in display-space pixels.
    pub bounds: Rect,
    /// Z-order (higher = drawn on top / hit first).
    pub z_order: u32,
    /// Owner namespace (for dispatch routing).
    pub namespace: String,
    /// Whether this tile has scroll configuration and therefore accepts portal
    /// resize affordance pointer gestures.
    pub has_scroll_config: bool,
    /// Whether the viewer close button may show on this tile: an agent tile on
    /// the active tab that captures pointer input.
    pub dismissible: bool,
}

/// One runtime chrome drag-handle entry in the hit-test snapshot.
#[derive(Clone, Debug)]
pub struct DragHandleBoundsEntry {
    /// Display-space drag-handle bounds.
    pub bounds: Rect,
}

impl HitTestSnapshot {
    /// Create an empty snapshot.
    pub fn empty() -> Self {
        Self {
            tiles: Vec::new(),
            drag_handles: Vec::new(),
        }
    }

    /// Build a snapshot from the current scene graph.
    pub fn from_scene(scene: &SceneGraph) -> Self {
        let mut tiles: Vec<TileBoundsEntry> = scene
            .tiles
            .values()
            .map(|t| TileBoundsEntry {
                tile_id_bytes: t.id.to_bytes_le(),
                bounds: t.bounds,
                z_order: t.z_order,
                namespace: t.namespace.clone(),
                has_scroll_config: scene.tile_scroll_config(t.id).is_some(),
                dismissible: scene.active_tab == Some(t.tab_id)
                    && t.input_mode != tze_hud_scene::types::InputMode::Passthrough
                    && t.z_order < tze_hud_scene::types::ZONE_TILE_Z_MIN,
            })
            .collect();
        // Sort descending by z_order for hit-testing (highest z tested first)
        tiles.sort_unstable_by(|a, b| b.z_order.cmp(&a.z_order));
        let drag_handles = scene
            .overlay
            .drag_handle_hit_regions
            .iter()
            .filter(|region| region.hit_region.accepts_pointer)
            .map(|region| DragHandleBoundsEntry {
                bounds: region.bounds,
            })
            .collect();
        Self {
            tiles,
            drag_handles,
        }
    }

    /// Test whether a display-space point (x, y) hits any tile.
    /// Returns the first (highest z) tile entry that contains the point.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<&TileBoundsEntry> {
        self.tiles.iter().find(|t| {
            x >= t.bounds.x
                && x < t.bounds.x + t.bounds.width
                && y >= t.bounds.y
                && y < t.bounds.y + t.bounds.height
        })
    }

    /// The tile a pointer at (x, y) is over that may show a viewer close
    /// button: the highest-z dismissible tile containing the point.
    pub fn close_hover_target(&self, x: f32, y: f32) -> Option<tze_hud_scene::SceneId> {
        let entry = self
            .tiles
            .iter()
            .find(|t| t.dismissible && t.bounds.contains_point(x, y))?;
        tze_hud_scene::SceneId::from_bytes_le(&entry.tile_id_bytes)
    }

    /// Test whether a display-space point hits any runtime drag handle.
    pub fn hit_test_drag_handle(&self, x: f32, y: f32) -> bool {
        self.drag_handles
            .iter()
            .any(|handle| handle.bounds.contains_point(x, y))
    }
}

// ─── Frame Pipeline ───────────────────────────────────────────────────────────

/// Holds the lock-free hit-test snapshot the compositor publishes after each
/// scene commit and the main thread reads without a mutex.
pub struct FramePipeline {
    /// Shared hit-test snapshot, published after the scene commit.
    pub hit_test_snapshot: Arc<ArcSwap<HitTestSnapshot>>,
}

impl FramePipeline {
    /// Create a new pipeline with an empty hit-test snapshot.
    pub fn new() -> Self {
        Self {
            hit_test_snapshot: Arc::new(ArcSwap::from_pointee(HitTestSnapshot::empty())),
        }
    }
}

impl Default for FramePipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Stage 4 timed-content sweep ──────────────────────────────────────────────

/// Stage 4 sweep of everything with a deadline, shared by every frame driver
/// (windowed compositor, headless runtime, GPU-free harness) so none of them
/// can skip a step.
///
/// Applies batches whose `present_at` has arrived (invariant 1), removes
/// expired tiles and zone and widget publications, and expires leases whose
/// TTL or orphan grace has elapsed (invariant 4). Returns the terminal lease
/// expiries; the caller forwards them to sessions after releasing the scene
/// lock.
pub fn sweep_timed_scene_state(scene: &mut SceneGraph) -> Vec<tze_hud_scene::types::LeaseExpiry> {
    for late in scene.apply_due_batches() {
        if let Some(err) = late.error {
            tracing::warn!(
                batch_id = %late.batch_id,
                error = %err,
                "scheduled batch rejected at present_at"
            );
        }
    }
    scene.drain_expired_tiles();
    scene.drain_expired_zone_publications();
    scene.drain_expired_widget_publications();
    scene.expire_leases()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the HitTestSnapshot correctly builds from a scene and performs hit-testing.
    #[test]
    fn test_hit_test_snapshot_from_scene() {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab = scene.create_tab("Main", 0).unwrap();
        let lease = scene.grant_lease("agent", 60_000);
        scene
            .create_tile(
                tab,
                "agent",
                lease,
                Rect::new(100.0, 100.0, 300.0, 200.0),
                1,
            )
            .unwrap();
        scene
            .create_tile(
                tab,
                "agent",
                lease,
                Rect::new(200.0, 150.0, 100.0, 100.0),
                2,
            )
            .unwrap();

        let snap = HitTestSnapshot::from_scene(&scene);
        assert_eq!(snap.tiles.len(), 2);
        // Tiles sorted by z_order descending (z=2 first)
        assert_eq!(snap.tiles[0].z_order, 2);
        assert_eq!(snap.tiles[1].z_order, 1);

        // Hit the higher-z tile
        let hit = snap.hit_test(250.0, 175.0);
        assert!(hit.is_some(), "should hit a tile at (250, 175)");
        assert_eq!(hit.unwrap().z_order, 2, "should hit the higher-z tile");

        // Hit outside all tiles
        assert!(
            snap.hit_test(0.0, 0.0).is_none(),
            "should not hit anything at (0,0)"
        );
    }
}
