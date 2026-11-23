//! # Frame pipeline state
//!
//! The lock-free [`HitTestSnapshot`] the compositor thread publishes after each
//! scene commit, the production frame turn shared by all three drivers, and
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

// ─── Production frame turn ───────────────────────────────────────────────────

/// Explicit API readback needs a fresh frame; event-driven turns obey idle.
#[derive(Clone, Copy, Default)]
pub(crate) enum FrameDemand {
    #[default]
    EventDriven,
    Readback,
}

#[derive(Default)]
pub(crate) struct FrameTurnRequest {
    pub demand: FrameDemand,
    pub repaint: bool,
    pub degradation: Option<tze_hud_compositor::CompositorDegradationPolicy>,
}

/// The ordinary frame ledger advances only after an actual successful submit.
pub(crate) struct FrameTurnState {
    last_rendered: (u64, u64),
    widget_repaint_pending: bool,
    fallback_repaint_pending: bool,
    local_repaint_pending: bool,
}

impl Default for FrameTurnState {
    fn default() -> Self {
        Self {
            last_rendered: (u64::MAX, u64::MAX),
            widget_repaint_pending: false,
            fallback_repaint_pending: false,
            local_repaint_pending: false,
        }
    }
}

/// Backend-specific work. Windowed work is prepared under the lock and submitted
/// later; offscreen work includes the real copy-to-buffer and queue submission.
pub(crate) struct FrameTurnBuild<T> {
    pub work: T,
    pub submitted: bool,
}

pub(crate) struct FrameTurnOutcome<C, T> {
    pub committed: C,
    /// None explicitly means no renderer was attached, never a successful frame.
    pub work: Option<T>,
    pub submitted: bool,
    pub markdown_prime_us: u64,
    pub stage4_us: u64,
    pub next_wall_us: Option<u64>,
    pub next_animation_at: Option<std::time::Instant>,
}

impl FrameTurnState {
    /// One stage order and idle decision for all runtime frame drivers.
    ///
    /// Adapters own scene acquisition. `commit` preserves the headless Stage 2/3
    /// input boundary, and `build` implements only the surface-specific tail.
    /// Windowed builds leave submission deferred until after releasing the lock.
    pub(crate) fn run_frame_turn<C, T>(
        &mut self,
        scene: &mut SceneGraph,
        mut compositor: Option<&mut tze_hud_compositor::Compositor>,
        request: FrameTurnRequest,
        commit: impl FnOnce(&mut SceneGraph) -> C,
        build: impl FnOnce(
            &mut tze_hud_compositor::Compositor,
            &mut SceneGraph,
            bool,
        ) -> FrameTurnBuild<T>,
    ) -> FrameTurnOutcome<C, T> {
        let turn_started_at = std::time::Instant::now();
        if let Some(renderer) = compositor.as_mut() {
            crate::widget_runtime_registration::process_pending_widget_svgs(
                renderer.widget_renderer_mut(),
                scene.drain_pending_widget_svg_assets(),
            );
        }
        sweep_timed_scene_state(scene);
        if let Some(renderer) = compositor.as_mut() {
            if let Some(policy) = request.degradation {
                renderer.set_degradation_policy(policy);
            }
            renderer.update_publication_animations(scene);
            renderer.prune_faded_publications(scene);
        }
        let commit_started_at = std::time::Instant::now();
        let committed = commit(scene);
        let commit_elapsed = commit_started_at.elapsed();
        let mut stage4_us = turn_started_at
            .elapsed()
            .saturating_sub(commit_elapsed)
            .as_micros() as u64;
        let mut markdown_prime_us = 0;
        let mut submitted = false;
        let mut needs_render = false;
        let work = compositor.as_mut().map(|renderer| {
            self.fallback_repaint_pending |= renderer.poll_font_fallback();
            let start = std::time::Instant::now();
            renderer.prime_markdown_cache(scene);
            markdown_prime_us = start.elapsed().as_micros() as u64;
            renderer.prime_truncation_cache(scene);
            // The drain consumes one-shot echo/blur/hover observations. Keep
            // their repaint owed across capture, lock misses and failed submits.
            self.local_repaint_pending |= renderer.drain_local_composer_and_needs_render();
            self.widget_repaint_pending &= scene.widgets_shown() && !scene.widget_time_paused();
            self.widget_repaint_pending |= renderer.widget_deadline_needs_render(scene);
            needs_render = frame_needs_render(
                scene.version != self.last_rendered.0,
                scene.geometry_epoch != self.last_rendered.1,
                renderer.has_inflight_animation(scene),
                self.local_repaint_pending,
                request.repaint || self.widget_repaint_pending || self.fallback_repaint_pending,
            ) || matches!(request.demand, FrameDemand::Readback);
            stage4_us = turn_started_at
                .elapsed()
                .saturating_sub(commit_elapsed)
                .as_micros() as u64;
            let result = build(renderer, scene, needs_render);
            submitted = result.submitted;
            result.work
        });
        let versions = (scene.version, scene.geometry_epoch);
        self.record_submission(submitted, versions);
        // A full windowed frame already schedules the following cadence turn.
        // Avoid scanning all lease/content deadlines on every active frame.
        let (next_wall_us, next_animation_at) = if needs_render {
            (None, None)
        } else {
            let wall = scene
                .next_publication_expiry_wall_us()
                .into_iter()
                .chain(
                    compositor
                        .as_ref()
                        .and_then(|renderer| renderer.next_widget_deadline_wall_us(scene)),
                )
                .chain(scene.next_timed_content_wall_us())
                .chain(
                    scene
                        .next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS)
                        .map(|ms| ms.saturating_mul(1_000)),
                )
                .min();
            let animation = compositor
                .as_ref()
                .and_then(|renderer| renderer.next_animation_deadline());
            (wall, animation)
        };
        FrameTurnOutcome {
            committed,
            work,
            submitted,
            markdown_prime_us,
            stage4_us,
            next_wall_us,
            next_animation_at,
        }
    }

    pub(crate) fn record_submission(&mut self, submitted: bool, versions: (u64, u64)) {
        if submitted {
            self.last_rendered = versions;
            self.widget_repaint_pending = false;
            self.fallback_repaint_pending = false;
            self.local_repaint_pending = false;
        }
    }

    #[cfg(test)]
    pub(crate) fn last_rendered(&self) -> (u64, u64) {
        self.last_rendered
    }
}

pub(crate) fn frame_needs_render(
    scene_changed: bool,
    geometry_changed: bool,
    animation_inflight: bool,
    composer_needs_render: bool,
    surface_repaint_pending: bool,
) -> bool {
    scene_changed
        || geometry_changed
        || animation_inflight
        || composer_needs_render
        || surface_repaint_pending
}

/// Recovery debt survives lock misses and failed acquisitions.
pub(crate) const fn update_surface_repaint_pending(
    was_pending: bool,
    recovered: bool,
    submitted: bool,
) -> bool {
    (was_pending || recovered) && !submitted
}

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
    /// Current painted Reset action; chrome only, never agent hit testing.
    pub context_menu: Option<Rect>,
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
    /// Draggable owner and its hover envelope, including the gap to its grip.
    /// This does not expand pointer capture or the recognizer's movement bounds.
    pub element_id: tze_hud_scene::SceneId,
    pub hover_bounds: Rect,
    /// Display-space drag-handle bounds.
    pub bounds: Rect,
    /// For the pip resize grip, the diagonal resize cursor of its corner.
    pub resize_cursor: Option<tze_hud_input::PortalCursor>,
}

fn enclosing_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    Rect::new(
        x,
        y,
        (a.x + a.width).max(b.x + b.width) - x,
        (a.y + a.height).max(b.y + b.height) - y,
    )
}

impl HitTestSnapshot {
    /// Create an empty snapshot.
    pub fn empty() -> Self {
        Self {
            tiles: Vec::new(),
            drag_handles: Vec::new(),
            context_menu: None,
        }
    }

    /// Build a snapshot from the current scene graph.
    pub fn from_scene(scene: &SceneGraph) -> Self {
        let mut tiles: Vec<TileBoundsEntry> = scene
            .tiles
            .values()
            .filter(|t| scene.tile_shown(t))
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
            .map(|region| {
                // Notifications occupy admitted card slots, not the entire
                // logical zone. Reuse the actual painted rectangles without
                // changing the drag recognizer's existing movement bounds.
                let notification_zone = scene.zone_by_id(region.element_id).filter(|z| {
                    scene
                        .zone_registry
                        .active_publishes
                        .get(&z.name)
                        .is_some_and(|records| {
                            records.iter().any(|r| {
                                matches!(r.content, tze_hud_scene::ZoneContent::Notification(_))
                            })
                        })
                });
                let element_bounds = notification_zone
                    .map(|zone| {
                        scene
                            .overlay
                            .notification_geometry
                            .iter()
                            .filter(|card| card.zone_name == zone.name)
                            .map(|card| card.bounds)
                            .reduce(enclosing_rect)
                            .unwrap_or(region.bounds)
                    })
                    .unwrap_or_else(|| region.element_bounds.unwrap_or(region.bounds));
                DragHandleBoundsEntry {
                    element_id: region.element_id,
                    hover_bounds: enclosing_rect(element_bounds, region.bounds),
                    bounds: region.bounds,
                    resize_cursor: region
                        .resize_grip
                        .map(tze_hud_input::PortalCursor::for_grip_corner),
                }
            })
            .collect();
        Self {
            tiles,
            drag_handles,
            context_menu: scene
                .overlay
                .drag_handle_context_menu
                .as_ref()
                .and_then(|m| m.reset_button_rect),
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

    /// Draggable element under the pointer, including the gap to its grip. Resize-only
    /// entries do not independently expose a move pill. The content stays
    /// passthrough: capture still uses only `hit_test_drag_handle`.
    pub fn drag_handle_hover_target(&self, x: f32, y: f32) -> Option<tze_hud_scene::SceneId> {
        // Chrome entries follow paint order; only body hover traverses them
        // front to back. Grip capture retains its existing ordering.
        self.drag_handles
            .iter()
            .rev()
            .find(|h| h.resize_cursor.is_none() && h.hover_bounds.contains_point(x, y))
            .map(|h| h.element_id)
    }

    /// Test whether a display-space point hits any runtime drag handle.
    pub fn hit_test_drag_handle(&self, x: f32, y: f32) -> bool {
        self.drag_handles
            .iter()
            .any(|handle| handle.bounds.contains_point(x, y))
    }

    /// The resize cursor when (x, y) is over the pip resize grip.
    pub fn resize_grip_cursor(&self, x: f32, y: f32) -> Option<tze_hud_input::PortalCursor> {
        self.drag_handles
            .iter()
            .filter(|handle| handle.bounds.contains_point(x, y))
            .find_map(|handle| handle.resize_cursor)
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
pub(crate) fn sweep_timed_scene_state(
    scene: &mut SceneGraph,
) -> Vec<tze_hud_scene::types::LeaseExpiry> {
    // Diagnostic work is opt-in; normal sweeps allocate no extra metadata.
    let scheduled = tracing::enabled!(target: "tze_hud_delay_diagnostic", tracing::Level::DEBUG)
        .then(|| {
            scene
                .scheduled_publications
                .iter()
                .map(|s| {
                    let publication = match &s.publication {
                        tze_hud_scene::Publication::Zone {
                            zone_name,
                            expires_at_wall_us,
                            ..
                        } if zone_name == "subtitle" => {
                            Some((s.publisher.namespace.clone(), *expires_at_wall_us))
                        }
                        _ => None,
                    };
                    (s.id, (s.present_at_wall_us, publication))
                })
                .collect::<std::collections::HashMap<_, _>>()
        });
    let applied = scene.apply_due_publications();
    for late in &applied {
        if let Some((present_at, publication)) = scheduled.as_ref().and_then(|s| s.get(&late.id)) {
            let records = scene.zone_registry.active_publishes.get("subtitle");
            let record = records.and_then(|records| match records.as_slice() {
                [record]
                    if applied.len() == 1
                        && late.result.is_ok()
                        && publication.as_ref().is_some_and(|(namespace, expiry)| {
                            record.publisher_namespace == *namespace
                                && record.expires_at_wall_us == *expiry
                                && record.published_at_wall_us >= *present_at
                        }) =>
                {
                    Some(record)
                }
                _ => None,
            });
            let (batch_hi, batch_lo) = late.id.as_uuid().as_u64_pair();
            tracing::debug!(
                target: "tze_hud_delay_diagnostic",
                event = "scheduled_activation", batch_hi, batch_lo,
                present_at_wall_us = present_at,
                applied_at_wall_us = scene.now_wall_us(),
                scene_version = scene.version,
                accepted = late.result.is_ok(),
                identity_valid = record.is_some(),
                active_publications = records.map_or(0, Vec::len),
                published_at_wall_us = record.map(|r| r.published_at_wall_us),
                expires_at_wall_us = record.and_then(|r| r.expires_at_wall_us),
            );
        }
        if let Err(err) = &late.result {
            tracing::warn!(
                publication_id = %late.id,
                error = %err,
                "scheduled publication rejected at present_at"
            );
        }
    }
    scene.drain_expired_zone_publications();
    scene.drain_expired_widget_publications();
    scene.expire_leases()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resize grip's hover cursor is the diagonal of its corner: `╲` for
    /// top-left/bottom-right, `╱` for top-right/bottom-left; a move handle
    /// contributes none.
    #[test]
    fn resize_grip_cursor_maps_each_corner_to_its_diagonal() {
        use tze_hud_input::PortalCursor::{NeswResize, NwseResize};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let corners = [
            ((true, true), NwseResize),
            ((false, false), NwseResize),
            ((false, true), NeswResize),
            ((true, false), NeswResize),
        ];
        for (i, (corner, _)) in corners.iter().enumerate() {
            let bounds = Rect::new(100.0 * i as f32, 0.0, 24.0, 24.0);
            scene
                .overlay
                .drag_handle_hit_regions
                .push(tze_hud_scene::DragHandleHitRegion {
                    element_id: tze_hud_scene::SceneId::new(),
                    element_kind: tze_hud_scene::DragHandleElementKind::Zone,
                    bounds,
                    element_bounds: None,
                    interaction_id: format!("resize-grip:{i}"),
                    hit_region: tze_hud_scene::HitRegionNode {
                        bounds,
                        accepts_pointer: true,
                        ..Default::default()
                    },
                    tab_order: 0,
                    is_header_band: false,
                    resize_grip: Some(*corner),
                });
        }
        let handle = Rect::new(1000.0, 0.0, 24.0, 8.0);
        scene
            .overlay
            .drag_handle_hit_regions
            .push(tze_hud_scene::DragHandleHitRegion {
                element_id: tze_hud_scene::SceneId::new(),
                element_kind: tze_hud_scene::DragHandleElementKind::Zone,
                bounds: handle,
                element_bounds: None,
                interaction_id: "drag-handle:move".to_string(),
                hit_region: tze_hud_scene::HitRegionNode {
                    bounds: handle,
                    accepts_pointer: true,
                    ..Default::default()
                },
                tab_order: 0,
                is_header_band: false,
                resize_grip: None,
            });
        let snap = HitTestSnapshot::from_scene(&scene);
        for (i, (_, cursor)) in corners.iter().enumerate() {
            assert_eq!(
                snap.resize_grip_cursor(100.0 * i as f32 + 12.0, 12.0),
                Some(*cursor)
            );
        }
        assert_eq!(snap.resize_grip_cursor(1012.0, 4.0), None, "move handle");
        assert!(snap.hit_test_drag_handle(1012.0, 4.0));
        assert_eq!(
            snap.resize_grip_cursor(1500.0, 500.0),
            None,
            "off any handle"
        );
    }

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
