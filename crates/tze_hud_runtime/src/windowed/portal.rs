use tze_hud_input::{
    DragEventOutcome, InputProcessor, PointerEvent, PointerEventKind, PortalRect,
    PortalResizeState, PortalWindowTokens, ResizeBounds, ResizeEdge, ResizeOutcome, hit_affordance,
};
use tze_hud_scene::types::{DragHandleElementKind, ZoneInteractionKind};
use tze_hud_scene::{HitResult, Rect, SceneGraph, SceneId};

use super::WinitApp;
use super::input_dispatch::dispatch_portal_geometry_event;
use super::lifecycle::{INTERACTION_LOCK_BUDGET, spin_acquire};

/// Captured definition identity; a zone is a slot, never an agent tile.
#[derive(Clone, Debug)]
pub(super) enum DragTarget {
    Tile(SceneId),
    Zone(String),
}

impl DragTarget {
    pub(super) fn capture(
        scene: &SceneGraph,
        id: SceneId,
        kind: DragHandleElementKind,
    ) -> Option<Self> {
        match kind {
            DragHandleElementKind::Tile => scene.tiles.get(&id).map(|t| Self::Tile(t.lease_id)),
            DragHandleElementKind::Zone if scene.zones_shown() => {
                scene.zone_by_id(id).map(|z| Self::Zone(z.name.clone()))
            }
            _ => None,
        }
    }

    fn bounds(&self, scene: &SceneGraph, id: SceneId, sw: f32, sh: f32) -> Option<Rect> {
        let zone = match self {
            Self::Tile(_) => return scene.tiles.get(&id).map(|t| t.bounds),
            Self::Zone(name) => scene.zone_registry.zones.get(name).filter(|z| z.id == id)?,
        };
        let (x, y, w, h) =
            tze_hud_scene::resolve_zone_geometry(&scene.effective_zone_geometry(zone), sw, sh);
        Some(Rect::new(x, y, w, h))
    }
}

/// Identity of the primary drag that actually entered the recognizer under lock.
/// A missed Up retains only this gesture's terminal event, never generic input.
#[derive(Clone, Debug)]
pub(super) struct PrimaryDragGesture {
    pub(super) element_id: tze_hud_scene::SceneId,
    pub(super) target: DragTarget,
    pub(super) feedback_target: tze_hud_scene::SceneId,
    pub(super) interaction_id: String,
    pub(super) press_start: std::time::Instant,
    pub(super) window_id: Option<winit::window::WindowId>,
    pub(super) window_allocation: Option<std::sync::Weak<winit::window::Window>>,
    pub(super) terminal: Option<PointerEvent>,
}

impl PrimaryDragGesture {
    pub(super) fn matches(&self, drag: &tze_hud_input::drag::DeviceDragState) -> bool {
        self.element_id == drag.element_id
            && self.interaction_id == drag.interaction_id
            && self.press_start == drag.press_start
    }

    pub(super) fn target_is_live(&self, scene: &tze_hud_scene::SceneGraph) -> bool {
        match &self.target {
            DragTarget::Tile(lease) => [self.element_id, self.feedback_target].iter().all(|id| {
                scene
                    .tiles
                    .get(id)
                    .is_some_and(|tile| tile.lease_id == *lease)
            }),
            DragTarget::Zone(name) => {
                scene.zones_shown()
                    && scene
                        .zone_registry
                        .zones
                        .get(name)
                        .is_some_and(|zone| zone.id == self.element_id)
            }
        }
    }
}

pub(super) fn drag_feedback_target(
    scene: &tze_hud_scene::SceneGraph,
    element_id: tze_hud_scene::SceneId,
) -> tze_hud_scene::SceneId {
    let Some(tile) = scene.tiles.get(&element_id) else {
        return element_id;
    };
    if scene
        .overlay
        .portal_surfaces
        .get(&element_id)
        .is_some_and(|surface| {
            surface
                .parts
                .iter()
                .any(|part| part.kind == tze_hud_scene::PortalPartKind::Header)
        })
    {
        return element_id;
    }
    if let Some(anchor_id) = scene.portal_band_anchor_tile(element_id)
        && let Some(frame) = scene.tiles.get(&anchor_id)
        && rect_contains(&frame.bounds, &tile.bounds, 1.0)
        && scene.overlay.tile_scroll_configs.keys().any(|id| {
            scene.tiles.get(id).is_some_and(|member| {
                member.lease_id == tile.lease_id
                    && rect_contains(&frame.bounds, &member.bounds, 1.0)
            })
        })
    {
        anchor_id
    } else {
        element_id
    }
}

/// Reconcile shared feedback from all remaining activated device gestures.
/// Releasing one device must not clear another device's border on the same frame.
pub(super) fn reconcile_drag_feedback(
    input: &InputProcessor,
    scene: &mut tze_hud_scene::SceneGraph,
) {
    let active: std::collections::HashSet<_> = input
        .drag_states
        .values()
        .filter(|drag| {
            drag.phase == tze_hud_input::drag::DragPhase::Activated
                && DragTarget::capture(scene, drag.element_id, drag.element_kind).is_some()
        })
        .map(|drag| drag_feedback_target(scene, drag.element_id))
        .collect();
    let obsolete: Vec<_> = scene
        .overlay
        .drag_active_elements
        .difference(&active)
        .copied()
        .collect();
    for id in obsolete {
        scene.clear_drag_active(id);
    }
    for id in active {
        scene.set_drag_active(id);
    }
}

/// Result of one best-effort portal projection drain.
///
/// A busy scene is distinct from a completed no-op: the windowed scheduler
/// must install its coalesced availability wake only for the former. Treating
/// both as `false` loses the information needed to avoid retrying a past
/// portal deadline on a frame cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PortalProjectionDrain {
    Deferred,
    Completed { scene_changed: bool },
}

impl PortalProjectionDrain {
    pub(super) const fn scene_changed(self) -> bool {
        match self {
            Self::Deferred => false,
            Self::Completed { scene_changed } => scene_changed,
        }
    }

    pub(super) const fn is_deferred(self) -> bool {
        matches!(self, Self::Deferred)
    }
}

// ── Drag-to-move: data carried out of the scene-lock for post-lock work ──────

/// Payload returned by [`apply_drag_handle_pointer_event`] when a drag is
/// completed and the geometry must be persisted outside the scene lock.
pub(super) enum DragRelease {
    Tile(DragReleasedData),
    Zone,
}

pub(super) struct DragReleasedData {
    /// Scene-level ID of the element that was dragged.
    element_id: tze_hud_scene::SceneId,
    /// Final snapped/clamped bounds; dragging preserves the original dimensions.
    bounds: Rect,
    /// Primary canvas dimensions for relative geometry normalization.
    display: (f32, f32),
    /// Agent namespace that owns the tile, used for `ElementRepositionedEvent` routing.
    namespace: String,
    /// Final whole-portal member bounds, including the grabbed tile; empty
    /// for single tiles. [`write_member_geometry_overrides`] owns the id-keyed
    /// persistence contract for shared namespaces (hud-8vejp).
    group_members: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)>,
}

// ── Portal-resize pointer: geometry carried out of scene lock ────────────────

/// Per-member geometry update produced by a whole-portal resize step.
///
/// A portal is composed of several constituent surfaces; a single resize step
/// scales all of them, so the outcome carries one snapshot per member for the
/// caller to broadcast.
pub(super) struct PortalMemberGeometry {
    /// Constituent-surface tile whose bounds were updated.
    pub(super) tile_id: tze_hud_scene::SceneId,
    /// Geometry snapshot carrying this member's scaled rect.
    pub(super) snapshot: tze_hud_input::GeometrySnapshot,
}

/// Outcome of a pointer-driven portal resize step, carried out of the scene
/// lock so that [`dispatch_portal_geometry_event`] can be called without
/// holding locks (fire-and-forget gRPC send).
pub(super) struct PortalResizePointerOutcome {
    /// Per-member geometry updates to broadcast (includes the anchor/frame,
    /// whose snapshot carries the whole-portal rect).
    pub(super) members: Vec<PortalMemberGeometry>,
    /// Display width at the time of the event (for geometry normalisation).
    pub(super) display_w: f32,
    /// Display height at the time of the event.
    pub(super) display_h: f32,
    /// Whether this step should durably persist each member's geometry override
    /// to the element store (hud-8vejp). Set only on the terminal PointerUp so
    /// the disk write fires once per gesture; intermediate down/move steps keep
    /// the group coherent via the in-session lock without touching disk.
    pub(super) persist: bool,
}

// The input processor, pointer event, hit result, scene, and display dimensions
// are all separate concerns that cannot be bundled into a context struct without
// creating an artificial abstraction; the argument count is genuinely necessary.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_drag_handle_pointer_event(
    input_processor: &mut InputProcessor,
    pointer_event: &PointerEvent,
    result_hit: &HitResult,
    scene: &mut tze_hud_scene::graph::SceneGraph,
    display_width: f32,
    display_height: f32,
) -> Option<DragRelease> {
    let device_id = pointer_event.device_id;

    // Determine which drag handle (if any) was hit on this event.
    let hit_drag_info: Option<(&str, tze_hud_scene::SceneId, DragHandleElementKind, bool)> =
        match result_hit {
            HitResult::ZoneInteraction {
                interaction_id,
                kind:
                    ZoneInteractionKind::DragHandle {
                        element_id,
                        element_kind,
                        is_header_band,
                    },
                ..
            } => Some((
                interaction_id.as_str(),
                *element_id,
                *element_kind,
                *is_header_band,
            )),
            _ => None,
        };

    // On PointerDown on a drag handle, start accumulating.
    if pointer_event.kind == PointerEventKind::Down {
        input_processor.drag_states.remove(&device_id);
        if let Some((interaction_id, element_id, element_kind, is_header_band)) = hit_drag_info {
            let target = DragTarget::capture(scene, element_id, element_kind);
            let live_handle = !matches!(target, Some(DragTarget::Zone(_)))
                || scene.overlay.drag_handle_hit_regions.iter().any(|h| {
                    h.element_id == element_id
                        && h.element_kind == element_kind
                        && h.interaction_id == interaction_id
                        && scene.shown_zone_publishes().any(|(name, p)| {
                            !p.is_empty() && scene.zone_registry.zones[name].id == element_id
                        })
                });
            let Some(element_bounds) = target
                .filter(|_| live_handle)
                .and_then(|t| t.bounds(scene, element_id, display_width, display_height))
            else {
                reconcile_drag_feedback(input_processor, scene);
                return None;
            };
            let outcome = input_processor.process_drag_handle_pointer(
                pointer_event,
                interaction_id,
                element_id,
                element_kind,
                element_bounds,
                display_width,
                display_height,
                is_header_band,
            );
            tracing::trace!(
                element_id = %element_id,
                x = pointer_event.x,
                y = pointer_event.y,
                ?outcome,
                "drag-handle: PointerDown accumulating"
            );
        }
        reconcile_drag_feedback(input_processor, scene);
        return None;
    }

    // On PointerMove or PointerUp, check for an in-flight drag on this device.
    let drag_info = input_processor
        .drag_states
        .get(&device_id)
        .map(|s| (s.interaction_id.clone(), s.element_id, s.element_kind));

    let Some((interaction_id, element_id, element_kind)) = drag_info else {
        // No drag in progress for this device — nothing to do.
        return None;
    };

    let target = DragTarget::capture(scene, element_id, element_kind);
    let Some(element_bounds) = target
        .as_ref()
        .and_then(|t| t.bounds(scene, element_id, display_width, display_height))
    else {
        input_processor.drag_states.remove(&device_id);
        reconcile_drag_feedback(input_processor, scene);
        return None;
    };

    // The drag is already in flight for this device; its DeviceDragState already
    // carries the immediate/band flag, so the value passed here is unused on
    // Move/Up (it only seeds a new state on Down).
    let outcome = input_processor.process_drag_handle_pointer(
        pointer_event,
        &interaction_id,
        element_id,
        element_kind,
        element_bounds,
        display_width,
        display_height,
        false,
    );

    if let Some(DragTarget::Zone(_)) = target {
        let position = match outcome {
            DragEventOutcome::Moved { new_x, new_y, .. } => Some((new_x, new_y)),
            DragEventOutcome::Released {
                final_x, final_y, ..
            } => Some((final_x, final_y)),
            _ => None,
        };
        if let Some((x, y)) = position {
            scene.set_zone_placement(
                element_id,
                tze_hud_input::drag::final_position_to_geometry(
                    x,
                    y,
                    element_bounds.width,
                    element_bounds.height,
                    display_width,
                    display_height,
                ),
            );
        }
        reconcile_drag_feedback(input_processor, scene);
        return matches!(outcome, DragEventOutcome::Released { .. }).then_some(DragRelease::Zone);
    }

    let released = match outcome {
        DragEventOutcome::Idle | DragEventOutcome::Accumulating { .. } => {
            // Nothing to do locally.
            None
        }
        DragEventOutcome::Activated { element_id, .. } => {
            tracing::debug!(
                element_id = %element_id,
                "drag-handle: drag activated — element follows pointer"
            );
            None
        }
        DragEventOutcome::Cancelled => {
            tracing::trace!(
                element_id = %element_id,
                "drag-handle: drag cancelled (tap or moved beyond tolerance)"
            );
            None
        }
        DragEventOutcome::Moved {
            element_id: eid,
            new_x,
            new_y,
            ..
        } => {
            move_drag_tile(scene, eid, new_x, new_y);
            None
        }
        DragEventOutcome::Released {
            element_id: eid,
            final_x,
            final_y,
            element_kind: _,
        } => {
            let (width, height) = (element_bounds.width, element_bounds.height);
            let group_members = if move_drag_tile(scene, eid, final_x, final_y) {
                resolve_portal_group(scene, eid)
                    .expect("translated portal group remains live under the scene lock")
                    .member_ids
                    .into_iter()
                    .map(|id| (id, scene.tiles[&id].bounds))
                    .collect()
            } else {
                Vec::new()
            };

            let namespace = scene.tiles[&eid].namespace.clone();

            tracing::debug!(
                element_id = %eid,
                final_x,
                final_y,
                width,
                height,
                "drag-handle: drag released — persisting geometry"
            );

            // Return data the caller will use to persist after releasing locks.
            Some(DragRelease::Tile(DragReleasedData {
                element_id: eid,
                bounds: Rect::new(final_x, final_y, width, height),
                display: (display_width, display_height),
                namespace,
                group_members,
            }))
        }
    };
    reconcile_drag_feedback(input_processor, scene);
    released
}

/// Move the existing tile/portal owner once; return whether its group moved.
/// Release alone snapshots group members for durable post-lock persistence.
fn move_drag_tile(scene: &mut SceneGraph, id: SceneId, x: f32, y: f32) -> bool {
    let Some(old) = scene.tiles.get(&id).map(|t| t.bounds) else {
        return false;
    };
    if translate_portal_group_on_drag(scene, id, x - old.x, y - old.y) {
        return true;
    }
    let tile = scene.tiles.get_mut(&id).expect("live single tile");
    tile.bounds.x = x;
    tile.bounds.y = y;
    scene.bump_geometry_epoch();
    false
}

/// Compute the maximum resize dimensions for a portal tile, combining the
/// display boundary clamp with the tile's lease-granted spatial budget.
///
/// The display boundary (`display_w`, `display_h`) is always the hard outer
/// clamp.  When the tile's lease carries a non-zero `max_tile_width_px` /
/// `max_tile_height_px`, whichever is smaller wins (most-restrictive policy).
///
/// `lease_max_width_px` and `lease_max_height_px` should be sourced from
/// the `Lease.resource_budget` in `scene.leases` (the authoritative location
/// for spatial budget limits).  Pass `0.0` for each to indicate unconstrained.
///
/// A lease budget value of `0.0` means unconstrained: only the display boundary
/// applies.
///
/// `min_*` values are from the portal tokens and serve as the floor (a portal
/// can always be grown to at least the minimum size even if the lease limit is
/// somehow smaller — the lease limit is not allowed to shrink a portal below the
/// token minimum, per §6b design intent).
///
/// ## Why `display_w`, not `display_w − tile.x`
///
/// `PortalRect::clamped` enforces that the portal fits on-screen by adjusting
/// both the size **and** the origin together: `x.clamp(0, display_w − w)`.
/// If the portal is at x=500 on a 1000px display, it can still grow to
/// 1000px wide — `clamped` will shift the origin left to x=0 so the right
/// edge lands exactly at the screen edge.  Using `display_w − tile.x` as the
/// max would prevent this shift and cap the portal at half the display width,
/// which is incorrect.
fn compute_portal_max_dims(
    lease_max_width_px: f32,
    lease_max_height_px: f32,
    display_w: f32,
    display_h: f32,
    min_width_px: f32,
    min_height_px: f32,
) -> (f32, f32) {
    // Display boundary: the portal cannot be wider/taller than the display.
    // The origin is clamped separately by PortalRect::clamped, so we do not
    // subtract tile.x / tile.y here (that was the pre-fix incorrect bound).
    let display_max_w = display_w.max(min_width_px);
    let display_max_h = display_h.max(min_height_px);

    // Lease budget: intersect with display boundary (most restrictive wins).
    // A lease value of 0.0 means unconstrained; skip the intersection.
    let max_w = if lease_max_width_px > 0.0 {
        display_max_w.min(lease_max_width_px).max(min_width_px)
    } else {
        display_max_w
    };
    let max_h = if lease_max_height_px > 0.0 {
        display_max_h.min(lease_max_height_px).max(min_height_px)
    } else {
        display_max_h
    };
    (max_w, max_h)
}

// ── Portal-group resolution (hud-fb3en) ──────────────────────────────────────

/// A resolved portal group: the constituent scene tiles that move and resize as
/// one coherent unit.
///
/// A text-stream portal is composed of several independent scene tiles that
/// share a single lease (frame, transcript/output pane, composer/input pane,
/// minimized icon, capture backstop, drag shield). The runtime has no explicit
/// portal-group field on [`tze_hud_scene::types::Tile`], so the group is resolved
/// structurally: the frame is the largest-area lease member (the portal-sized
/// frame / capture backstop), and its top-left is the fixed anchor for
/// grow/shrink (matching the top-left anchor semantics from PR #981). Members
/// are the lease's tiles whose bounds lie within the frame rect.
///
/// This deliberately EXCLUDES the drag shield, which the client parks in a far
/// display corner and which must not scale with the portal — mirroring the
/// client-side `portal_bounds_mutations` layout, which omits the drag shield
/// from the visible-portal geometry.
///
/// A single-tile lease resolves to a one-member group (the tile is its own
/// anchor), preserving the pre-fix single-surface behavior.
pub(super) struct PortalGroup {
    /// The frame/anchor tile — its bounds are the whole-portal rect.
    pub(super) anchor_tile_id: tze_hud_scene::SceneId,
    /// Whole-portal rect (anchor frame bounds) at resolution time.
    pub(super) portal_rect: PortalRect,
    /// All member tile ids that scale with the portal (includes the anchor).
    pub(super) member_ids: Vec<tze_hud_scene::SceneId>,
    /// Stable per-portal hash (from the anchor tile) for `PortalResizeState`.
    pub(super) portal_id_hash: u64,
}

/// Return true when `inner` lies within `outer`, allowing a small epsilon so
/// sub-pixel rounding at pane edges does not drop a legitimate member.
pub(super) fn rect_contains(
    outer: &tze_hud_scene::Rect,
    inner: &tze_hud_scene::Rect,
    eps: f32,
) -> bool {
    inner.x >= outer.x - eps
        && inner.y >= outer.y - eps
        && inner.x + inner.width <= outer.x + outer.width + eps
        && inner.y + inner.height <= outer.y + outer.height + eps
}

/// Resolve the whole-portal group that owns `member_tile_id`.
///
/// Works from ANY member tile — a focused pane on the hotkey / pointer-down
/// path, or the stored anchor tile on the pointer-move / pointer-up path: it
/// looks up the shared lease, picks the largest-area lease member as the
/// frame/anchor, then collects the lease members spatially contained within the
/// frame rect (plus the seed tile itself, defensively).
///
/// Returns `None` if the tile does not exist.
pub(super) fn resolve_portal_group(
    scene: &tze_hud_scene::graph::SceneGraph,
    member_tile_id: tze_hud_scene::SceneId,
) -> Option<PortalGroup> {
    let seed = scene.tiles.get(&member_tile_id)?;
    let lease_id = seed.lease_id;

    // Largest-area lease member is the frame/anchor (ties → lowest id). Delegated
    // to the scene-crate resolver so the frame pick is a single source of truth
    // shared with the compositor's header-band drag handle (hud-643dv).
    let anchor_id = scene.portal_anchor_tile(member_tile_id)?;
    let anchor_bounds = scene.tiles.get(&anchor_id)?.bounds;

    // Members = lease tiles spatially within the frame rect. The far-corner
    // drag shield falls outside the frame and is excluded. The seed and anchor
    // are always included.
    let eps = 1.0_f32;
    let mut member_ids: Vec<tze_hud_scene::SceneId> = scene
        .tiles
        .iter()
        .filter(|(id, tile)| {
            tile.lease_id == lease_id
                && (**id == member_tile_id
                    || **id == anchor_id
                    || rect_contains(&anchor_bounds, &tile.bounds, eps))
        })
        .map(|(id, _)| *id)
        .collect();
    member_ids.sort();

    let portal_rect = PortalRect {
        x: anchor_bounds.x,
        y: anchor_bounds.y,
        width: anchor_bounds.width,
        height: anchor_bounds.height,
    };
    let portal_id_hash = anchor_id.as_uuid().as_u128() as u64;

    Some(PortalGroup {
        anchor_tile_id: anchor_id,
        portal_rect,
        member_ids,
        portal_id_hash,
    })
}

/// Compute new bounds for every group member under a top-left-anchored uniform
/// scale from `old_rect` → `new_rect`.
///
/// The portal's top-left (`new_rect` origin) is the fixed anchor. Each member's
/// offset from the anchor and its size scale by the per-axis ratio, preserving
/// relative layout. Returns `(tile_id, new_rect)` pairs for members that still
/// exist in the scene.
fn scale_portal_members(
    scene: &tze_hud_scene::graph::SceneGraph,
    group: &PortalGroup,
    old_rect: PortalRect,
    new_rect: PortalRect,
) -> Vec<(tze_hud_scene::SceneId, tze_hud_scene::Rect)> {
    let r_w = if old_rect.width > 0.0 {
        new_rect.width / old_rect.width
    } else {
        1.0
    };
    let r_h = if old_rect.height > 0.0 {
        new_rect.height / old_rect.height
    } else {
        1.0
    };
    let mut updates = Vec::with_capacity(group.member_ids.len());
    for &tile_id in &group.member_ids {
        let Some(tile) = scene.tiles.get(&tile_id) else {
            continue;
        };
        // The anchor/frame IS the whole-portal rect: assign it exactly rather
        // than round-tripping through the scale ratio. This keeps a single-tile
        // portal bit-identical to the new rect (no float drift), which matters
        // for the `scene.version` size-change guard driving the compositor's
        // truncation-cache re-prime cadence.
        let new_bounds = if tile_id == group.anchor_tile_id {
            tze_hud_scene::Rect::new(new_rect.x, new_rect.y, new_rect.width, new_rect.height)
        } else {
            let b = tile.bounds;
            tze_hud_scene::Rect::new(
                new_rect.x + (b.x - old_rect.x) * r_w,
                new_rect.y + (b.y - old_rect.y) * r_h,
                b.width * r_w,
                b.height * r_h,
            )
        };
        updates.push((tile_id, new_bounds));
    }
    updates
}

/// Scale a tile's node tree in place by the per-axis ratio `(r_w, r_h)`.
///
/// Node bounds are **tile-local** (origin + extent relative to the tile's
/// top-left) and independent of `Tile::bounds` — nothing derives them from the
/// tile size. So when the viewer resizes a tile, the node tree must scale by the
/// same ratio or content keeps laying out to the old geometry. In particular the
/// compositor wraps `TextMarkdownNode` text to `node.bounds.width` (the layout
/// column in `TextItem::from_text_markdown_cached` / `from_text_markdown_node`),
/// so without this the transcript/composer text keeps its attach-time wrap width
/// and does not re-flow to the resized pane (hud-rpmwt). Font size is untouched —
/// text reflows to the new width, it does not zoom.
fn scale_tile_node_tree(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    tile_id: tze_hud_scene::SceneId,
    r_w: f32,
    r_h: f32,
) {
    let Some(root) = scene.tiles.get(&tile_id).and_then(|t| t.root_node) else {
        return;
    };
    // Collect the subtree ids with an immutable walk first, then mutate — a
    // tile's node tree is a small DAG-free tree (≤ MAX_NODES_PER_TILE) so this
    // avoids aliasing the node store while descending `children`.
    let mut stack = vec![root];
    let mut ids = Vec::new();
    while let Some(id) = stack.pop() {
        let Some(node) = scene.nodes.get(&id) else {
            continue;
        };
        ids.push(id);
        stack.extend(node.children.iter().copied());
    }
    for id in ids {
        if let Some(node) = scene.nodes.get_mut(&id) {
            let b = node.data.bounds_mut();
            b.x *= r_w;
            b.y *= r_h;
            b.width *= r_w;
            b.height *= r_h;
        }
    }
}

/// Scale a tile's first-class [`PortalSurface`] part bounds in lock-step with a
/// whole-portal resize (hud-s4lrw).
///
/// `PortalPart::bounds` is tile-local surface geometry — the per-part clip band
/// the compositor uses to keep one part's content (e.g. an overlong transcript)
/// from painting over a sibling part's region. When the viewer resizes the
/// portal, [`scale_tile_node_tree`] re-flows the part *nodes* to the new pane;
/// the surface descriptor's declared part bounds must scale by the same per-axis
/// ratio or the render-side clip band drifts off the content and the per-part
/// overflow invariant no longer holds at the new geometry.
///
/// Part bounds are tile-local (origin relative to the tile top-left), so — like
/// the node tree — they scale by the raw ratio; a whole-portal *drag* leaves
/// them untouched (only `tile.bounds` moves). No-op when the tile carries no
/// surface. Mutates the public `overlay.portal_surfaces` state directly (no
/// lease/capability gate): this is viewer-driven geometry authority, the same
/// path that writes `tile.bounds` above.
fn scale_portal_surface_parts(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    tile_id: tze_hud_scene::SceneId,
    r_w: f32,
    r_h: f32,
) {
    let Some(surface) = scene.overlay.portal_surfaces.get_mut(&tile_id) else {
        return;
    };
    for part in &mut surface.parts {
        part.bounds.x *= r_w;
        part.bounds.y *= r_h;
        part.bounds.width *= r_w;
        part.bounds.height *= r_h;
    }
}

/// Write a durable `Relative` geometry override into the element store for each
/// `(SceneId, rect)` member, keyed by its stable scene id (hud-8vejp).
///
/// Shared by the whole-portal resize persist path
/// ([`WinitApp::persist_portal_member_overrides`]) and the whole-portal drag
/// persist path ([`WinitApp::persist_drag_release`]). Keying by id — not by the
/// shared portal namespace — is what gives *every* member its own durable
/// override, so `list_elements` reports `has_user_override=true` for all members
/// (not just the drag-release/namespace one) and an adapter republish can never
/// reposition a member post-restart (the stored override is authoritative at the
/// publish ingress). `rect` is absolute display-pixel bounds, normalised to a
/// `Relative` policy against `display_w`/`display_h` before storage.
///
/// Members with no matching store entry are skipped (the id-keyed setter is a
/// no-op) — a member tile must be registered in the store before it can carry a
/// durable override.
fn write_member_geometry_overrides(
    store: &mut tze_hud_scene::element_store::ElementStore,
    members: &[(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)],
    display_w: f32,
    display_h: f32,
) {
    for (id, bounds) in members {
        let policy =
            tze_hud_scene::types::rect_to_relative_geometry_policy(*bounds, display_w, display_h);
        store.set_geometry_override(*id, policy);
    }
}

/// Apply a resolved whole-portal resize to the scene: write each member's scaled
/// bounds, bump the scene version once if any geometry changed, and build the
/// per-member geometry snapshots to broadcast.
///
/// `primary` is the whole-portal geometry snapshot produced by the resize state
/// machine (its `rect` is the new anchor/frame rect). Each returned member
/// snapshot carries the same sequence / gesture flag but with that member's own
/// scaled rect, so per-member `ElementRepositionedEvent`s report correct
/// geometry.
fn commit_portal_group_resize(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    group: &PortalGroup,
    old_rect: PortalRect,
    primary: tze_hud_input::GeometrySnapshot,
) -> Vec<PortalMemberGeometry> {
    let new_rect = primary.rect;
    let updates = scale_portal_members(scene, group, old_rect, new_rect);
    let mut any_changed = false;
    // Whole-portal WIDTH ratio — the single deterministic ratio the spec mandates
    // for text scaling (never anisotropic): the viewer-local per-tile font-scale
    // multiplier accumulates this factor across resize steps so transcript and
    // composer text grow/shrink with the portal (hud-ovjxu.1). Applied per member
    // below (gated on a real change); the compositor clamps the effective font to
    // the token-defined legible min/max at render, never mutating the
    // adapter-published `font_size_px`.
    let font_ratio = if old_rect.width > 0.0 {
        new_rect.width / old_rect.width
    } else {
        1.0
    };
    let mut members = Vec::with_capacity(updates.len());
    for (tile_id, new_bounds) in updates {
        let old_tile_bounds = scene.tiles.get(&tile_id).map(|t| t.bounds);
        if let Some(tile) = scene.tiles.get_mut(&tile_id) {
            if tile.bounds.width != new_bounds.width || tile.bounds.height != new_bounds.height {
                any_changed = true;
            }
            tile.bounds = new_bounds;
        }
        // Scale the tile's node tree in lock-step with the tile so tile-local
        // node geometry — and the text wrap width the compositor reads from
        // `TextMarkdownNode::bounds.width` — re-resolves to the new pane. Use
        // each tile's OWN size ratio (not the whole-portal ratio) so the nodes
        // track exactly the tile they live in. Without this the frame scales but
        // the transcript/composer text stays wrapped at the old width (hud-rpmwt).
        if let Some(old) = old_tile_bounds {
            let node_r_w = if old.width > 0.0 {
                new_bounds.width / old.width
            } else {
                1.0
            };
            let node_r_h = if old.height > 0.0 {
                new_bounds.height / old.height
            } else {
                1.0
            };
            if node_r_w != 1.0 || node_r_h != 1.0 {
                scale_tile_node_tree(scene, tile_id, node_r_w, node_r_h);
                // Keep any first-class portal-surface part bounds in lock-step
                // with the re-flowed node tree so the compositor's per-part clip
                // band stays aligned at the new geometry (hud-s4lrw). No-op for
                // legacy raw-tile portals with no surface descriptor.
                scale_portal_surface_parts(scene, tile_id, node_r_w, node_r_h);
            }
        }
        // The viewer now owns this member's geometry: take authority so an
        // adapter republishing its stale client-side layout on the next content
        // publish or drag cannot stomp the member back and fracture the portal
        // group (hud-lyqun). Viewer-driven resize writes `tile.bounds` directly
        // (above), so this only gates adapter-originated `UpdateTileBounds`.
        scene.lock_viewer_geometry(tile_id);
        // Accumulate the whole-portal width ratio into this member's viewer-local
        // font-scale (hud-ovjxu.1). Gated on a real ratio so a clamped-at-boundary
        // press does not drift the scale. Uniform across members (all get the same
        // portal ratio) so text stays consistent portal-wide.
        if (font_ratio - 1.0).abs() > f32::EPSILON {
            let next = scene.tile_font_scale(tile_id) * font_ratio;
            scene.set_tile_font_scale(tile_id, next);
        }
        let snapshot = tze_hud_input::GeometrySnapshot {
            rect: PortalRect {
                x: new_bounds.x,
                y: new_bounds.y,
                width: new_bounds.width,
                height: new_bounds.height,
            },
            ..primary
        };
        members.push(PortalMemberGeometry { tile_id, snapshot });
    }
    // The scene version drives the compositor's truncation-cache re-prime at the
    // new (intermediate) geometry; bump once per whole-portal step, guarded on a
    // real size change so a clamped-at-boundary press does not churn the cache.
    if any_changed {
        scene.version += 1;
    }
    members
}

/// Translate a whole portal group by `(dx, dy)` when the viewer drags one of its
/// constituent surfaces, preserving the group's relative layout and taking
/// viewer geometry authority over every member (hud-lyqun).
///
/// A text-stream portal is N independent tiles (frame + scrollable panes + drag
/// shield). Before this, the chrome drag handler moved only the single grabbed
/// tile, so dragging a portal fractured it (and, after a prior whole-portal
/// resize, the grabbed surface floated away from the rest). Here the dragged
/// tile's motion delta is applied to every group member so the portal moves as
/// one coherent unit — the completion of the whole-unit gesture work started for
/// resize in PR #984.
///
/// Gated to real portals: the resolved group must have more than one member and
/// contain at least one scrollable constituent surface. A plain single tile /
/// widget / zone drag resolves to a lone or non-portal group and is left to the
/// single-element move path (returns `false`). The far-corner drag shield is
/// excluded by `resolve_portal_group` and stays parked.
///
/// Returns `true` when a whole-portal translate was applied.
fn translate_portal_group_on_drag(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    dragged_tile_id: tze_hud_scene::SceneId,
    dx: f32,
    dy: f32,
) -> bool {
    let Some(group) = resolve_portal_group(scene, dragged_tile_id) else {
        return false;
    };
    let is_portal = group.member_ids.len() > 1
        && group
            .member_ids
            .iter()
            .any(|id| scene.tile_scroll_config(*id).is_some());
    if !is_portal {
        return false;
    }

    for &tile_id in &group.member_ids {
        if let Some(tile) = scene.tiles.get_mut(&tile_id) {
            tile.bounds.x += dx;
            tile.bounds.y += dy;
        }
        // Viewer geometry authority — same as the resize path — so an adapter
        // republish cannot pull a member back to its stale layout.
        scene.lock_viewer_geometry(tile_id);
    }
    // Position-only mutation: bump the geometry epoch (re-arms the present-gate
    // so every member paints at its new position this frame) but NOT
    // scene.version — a translation changes no content and no size, so the
    // compositor's version-gated markdown/truncation caches must NOT re-prime.
    // Bumping version here forced a full per-frame re-hash/re-shape and made the
    // live drag low-fps / flickery (hud-uyhpn). #986 group coherence and #989
    // resize reflow both key off size and are unaffected.
    scene.bump_geometry_epoch();
    true
}

/// Pointer-driven portal resize state machine step.
///
/// Called from [`WinitApp::enqueue_pointer_event`] while the scene lock is held.
/// Drives `PortalResizeState` through the pointer-down / pointer-move / pointer-up
/// lifecycle for resize affordances (§6b.1 pointer resize scenario).
///
/// On **PointerDown**: resolves the portal from the tile UNDER THE POINTER
/// (falling back to the focused tile only when the pointer is over no tile) and
/// hit-tests its resize affordances.  Resolving from the pointer rather than the
/// focused tile is deliberate: the initiating click-to-focus moves focus onto
/// the non-scrollable frame under the affordance corner, so a focus-gated resize
/// never starts under real OS pointer input (hud-yno2r).  If the pointer lands on
/// an affordance, starts the gesture and returns a [`PortalResizePointerOutcome`]
/// with the initial snapshot so the caller can apply local bounds and broadcast
/// the geometry event.
///
/// On **PointerMove**: if a gesture is active for `device_id`, computes the new
/// intermediate rect and applies it to the scene immediately (local-first).
///
/// On **PointerUp**: ends the gesture, applies the final clamped rect, and
/// returns an outcome the caller must broadcast.
///
/// Returns `None` when there is nothing to do (no portal under the pointer,
/// pointer outside affordances, no gesture active, or lock contention).
///
/// ## Gesture authority
///
/// The gesture epoch is advanced inside `PortalResizeState::on_pointer_down` (to
/// odd, blocking adapter publishes) and `on_pointer_up` (back to even, releasing
/// the block after the gesture ends).  The caller MUST propagate snapshots with
/// `gesture_active = true` to prevent adapter geometry from stomping the in-flight
/// resize.
// The scene, pointer, focus, resize-state, display-dims, and token arguments
// are all necessary and unrelated — merging them into a struct would create an
// ad-hoc context object with no benefit.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_portal_resize_pointer_event(
    pointer_event: &PointerEvent,
    portal_resize_states: &mut std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState>,
    active_tab: Option<tze_hud_scene::SceneId>,
    focus_manager: &tze_hud_input::FocusManager,
    scene: &mut tze_hud_scene::graph::SceneGraph,
    display_w: f32,
    display_h: f32,
    tokens: PortalWindowTokens,
) -> Option<PortalResizePointerOutcome> {
    let device_id = pointer_event.device_id;
    let x = pointer_event.x;
    let y = pointer_event.y;

    // Build the clamping bounds for a whole-portal rect owned by `anchor_lease`.
    let resize_bounds_for_lease =
        |scene: &tze_hud_scene::graph::SceneGraph, anchor_lease: tze_hud_scene::SceneId| {
            let (lease_max_w, lease_max_h) = scene
                .leases
                .get(&anchor_lease)
                .map(|l| {
                    (
                        l.spatial_budget.max_tile_width_px,
                        l.spatial_budget.max_tile_height_px,
                    )
                })
                .unwrap_or((0.0, 0.0));
            let (max_width_px, max_height_px) = compute_portal_max_dims(
                lease_max_w,
                lease_max_h,
                display_w,
                display_h,
                tokens.min_width_px,
                tokens.min_height_px,
            );
            ResizeBounds {
                tokens,
                max_width_px,
                max_height_px,
                display_w,
                display_h,
            }
        };

    match pointer_event.kind {
        PointerEventKind::Down => {
            // Resolve the portal from the tile UNDER THE POINTER, not the
            // *focused* tile. The resize affordance sits on the frame's
            // bottom-right corner — a non-scrollable tile — and the very same
            // pointer-down first runs click-to-focus (`process_with_focus` in
            // `enqueue_pointer_event`), which moves keyboard focus onto that
            // frame tile before this handler runs. Gating the gesture on the
            // *focused* tile being scrollable therefore made whole-portal
            // resize inert under real OS pointer input: the focus the gate
            // required had already been pulled off the scrollable pane by the
            // initiating click (hud-yno2r). Mirror the whole-portal move path
            // (`translate_portal_group_on_drag`), which resolves the group from
            // the tile under the pointer and gates portal-ness across the whole
            // group rather than the focused tile. Fall back to the focused tile
            // only when the pointer is not over any tile (e.g. injected events
            // in tests that pre-seed focus without a hittable frame).
            let seed_tile_id = match scene.hit_test(x, y) {
                HitResult::NodeHit { tile_id, .. } | HitResult::TileHit { tile_id } => {
                    Some(tile_id)
                }
                _ => active_tab.and_then(|tab| focus_manager.current_owner(tab).tile_id()),
            }?;
            // Resolve the whole portal; the affordance strip lives on the frame.
            let group = resolve_portal_group(scene, seed_tile_id)?;
            // Portal-ness gate: the resolved group must contain at least one
            // scrollable constituent surface (same check as the move path).
            // This keeps a plain widget / zone from acting as a resize target
            // without requiring the *focused* tile to be the scrollable one.
            if !group
                .member_ids
                .iter()
                .any(|id| scene.tile_scroll_config(*id).is_some())
            {
                return None;
            }
            let old_rect = group.portal_rect;
            let edge: ResizeEdge = hit_affordance(x, y, &old_rect, tokens.affordance_px)?;

            // Spatial budget from the anchor/frame's lease (the whole-portal
            // rect clamps against the portal's lease budget).
            let anchor_lease = scene.tiles.get(&group.anchor_tile_id)?.lease_id;
            let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);

            let resize_state = portal_resize_states
                .entry(group.anchor_tile_id)
                .or_insert_with(|| PortalResizeState::new(group.portal_id_hash));

            let outcome =
                resize_state.on_pointer_down(device_id, edge, x, y, old_rect, &resize_bounds);
            let snapshot = match outcome {
                ResizeOutcome::GestureStarted { snapshot } => snapshot,
                _ => return None,
            };
            let gesture_epoch = resize_state.current_gesture_epoch();

            // Apply initial (clamped) rect to the whole portal — local-first.
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::debug!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                ?edge,
                x,
                y,
                gesture_epoch,
                "portal resize: pointer-down on affordance — whole-portal gesture started"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: false,
            })
        }

        PointerEventKind::Move => {
            let mut active_gesture = None;
            for (&anchor_id, resize_state) in portal_resize_states.iter_mut() {
                if !resize_state.gesture_active() {
                    continue;
                }
                let Some(anchor_lease) = scene.tiles.get(&anchor_id).map(|t| t.lease_id) else {
                    continue;
                };
                let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);
                if let ResizeOutcome::GestureUpdate { snapshot } =
                    resize_state.on_pointer_move(device_id, x, y, &resize_bounds)
                {
                    active_gesture = Some((anchor_id, snapshot));
                    break;
                }
            }

            let (anchor_id, snapshot) = active_gesture?;
            let group = resolve_portal_group(scene, anchor_id)?;
            let old_rect = group.portal_rect;

            // Apply the updated whole-portal rect immediately (local-first).
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::trace!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                x,
                y,
                new_w = snapshot.rect.width,
                new_h = snapshot.rect.height,
                "portal resize: pointer-move — whole-portal bounds updated locally"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: false,
            })
        }

        PointerEventKind::Up => {
            let mut active_gesture = None;
            for (&anchor_id, resize_state) in portal_resize_states.iter_mut() {
                if !resize_state.gesture_active() {
                    continue;
                }
                let Some(anchor_lease) = scene.tiles.get(&anchor_id).map(|t| t.lease_id) else {
                    continue;
                };
                let resize_bounds = resize_bounds_for_lease(scene, anchor_lease);
                if let ResizeOutcome::GestureEnded { snapshot } =
                    resize_state.on_pointer_up(device_id, x, y, &resize_bounds)
                {
                    active_gesture =
                        Some((anchor_id, snapshot, resize_state.current_gesture_epoch()));
                    break;
                }
            }

            let (anchor_id, snapshot, gesture_epoch) = active_gesture?;
            let group = resolve_portal_group(scene, anchor_id)?;
            let old_rect = group.portal_rect;

            // Apply final clamped whole-portal rect (local-first).
            let members = commit_portal_group_resize(scene, &group, old_rect, snapshot);

            tracing::debug!(
                anchor_tile_id = ?group.anchor_tile_id,
                members = members.len(),
                x,
                y,
                final_w = snapshot.rect.width,
                final_h = snapshot.rect.height,
                gesture_epoch,
                "portal resize: pointer-up — whole-portal gesture ended, final bounds applied"
            );

            Some(PortalResizePointerOutcome {
                members,
                display_w,
                display_h,
                persist: true,
            })
        }
    }
}

/// Persist a detached snapshot after the caller has released shared/scene guards.
fn persist_element_store_in_background(
    store: tze_hud_scene::element_store::ElementStore,
    path: Option<std::path::PathBuf>,
) {
    if let Some(path) = path {
        std::thread::spawn(move || {
            if let Err(error) = crate::element_store::persist_element_store_to_path(&store, &path) {
                tracing::warn!(%error, "element store persist failed");
            }
        });
    }
}

impl WinitApp {
    /// Route a local submission through the complete native portal binding.
    pub(super) fn submit_composer_reply(&mut self, tile_id: tze_hud_scene::SceneId, text: String) {
        let submitted_at_wall_us = self.state.portal_projection_driver.now_wall_us();
        match self.state.portal_projection_driver.submit_reply_for_tile(
            tile_id,
            text,
            submitted_at_wall_us,
        ) {
            crate::portal::PortalReplyOutcome::NotPortal => {}
            crate::portal::PortalReplyOutcome::Handled(feedback) => {
                tracing::debug!(?tile_id, ?feedback, "composer: portal submission handled");
            }
        }
        self.reset_input_history_scroll_to_tail(tile_id);
    }

    /// Apply a retained Up only to the original live gesture. Generic input,
    /// capture dispatch and latency recording remain solely at ingress; only
    /// the ring enqueue certainly ran on a lock miss. None is replayed here.
    pub(super) fn drain_pending_drag_terminal(&mut self) -> bool {
        let Some(gesture) = self.state.primary_drag_gesture.clone() else {
            return false;
        };
        let Some(terminal) = gesture.terminal.as_ref() else {
            return false;
        };
        let window_is_live = self.primary_drag_window_is_live();
        let (released, scene_changed) = {
            let Ok(state) = self.state.shared_state.try_lock() else {
                return false;
            };
            let Ok(mut scene) = state.scene.try_lock() else {
                return false;
            };
            let epoch_before = scene.geometry_epoch;
            let same_gesture = self
                .state
                .input_processor
                .drag_states
                .get(&0)
                .is_some_and(|drag| gesture.matches(drag));
            let feedback_is_live =
                self.state
                    .input_processor
                    .drag_states
                    .get(&0)
                    .is_some_and(|drag| {
                        drag.phase != tze_hud_input::DragPhase::Activated
                            || scene.is_drag_active(gesture.feedback_target)
                    });
            let released = if same_gesture
                && window_is_live
                && feedback_is_live
                && gesture.target_is_live(&scene)
            {
                apply_drag_handle_pointer_event(
                    &mut self.state.input_processor,
                    terminal,
                    &HitResult::Passthrough,
                    &mut scene,
                    self.state.config.window.width as f32,
                    self.state.config.window.height as f32,
                )
            } else {
                // A late terminal cannot remove a newer drag, resurrect a removed
                // tile, or persist geometry from a different window allocation.
                if same_gesture {
                    self.state.input_processor.drag_states.remove(&0);
                }
                reconcile_drag_feedback(&self.state.input_processor, &mut scene);
                None
            };
            self.state.primary_drag_gesture = None;
            (released, scene.geometry_epoch != epoch_before)
        };
        if let Some(released) = released {
            self.persist_drag_release(released);
        }
        scene_changed
    }

    pub(super) fn primary_drag_window_is_live(&self) -> bool {
        let Some(gesture) = self.state.primary_drag_gesture.as_ref() else {
            return true;
        };
        match gesture.window_id {
            Some(id) => {
                let Some(original) = gesture
                    .window_allocation
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                else {
                    return false;
                };
                self.state.window.as_ref().is_some_and(|window| {
                    window.id() == id && std::sync::Arc::ptr_eq(window, &original)
                }) || self.state.secondaries.iter().any(|display| {
                    display.window.id() == id && std::sync::Arc::ptr_eq(&display.window, &original)
                })
            }
            None => {
                gesture.window_allocation.is_none()
                    && self.state.window.is_none()
                    && self.state.secondaries.is_empty()
            }
        }
    }

    /// Weak allocation identity distinguishes recreated windows even if the OS
    /// recycles their numeric WindowId, without keeping a retired window alive.
    pub(super) fn primary_drag_window_allocation(
        &self,
        id: Option<winit::window::WindowId>,
    ) -> Option<std::sync::Weak<winit::window::Window>> {
        let id = id?;
        self.state
            .window
            .as_ref()
            .filter(|window| window.id() == id)
            .or_else(|| {
                self.state
                    .secondaries
                    .iter()
                    .find(|display| display.window.id() == id)
                    .map(|display| &display.window)
            })
            .map(std::sync::Arc::downgrade)
    }

    /// Persist the geometry override for a completed drag and broadcast an
    /// `ElementRepositionedEvent`.
    ///
    /// Called after all scene locks are released to avoid holding locks during
    /// disk I/O (element store atomic write + fsync).
    pub(super) fn persist_drag_release(&mut self, released: DragRelease) {
        let DragRelease::Tile(released) = released else {
            return;
        };
        use tze_hud_scene::element_store::ElementType;

        let (store_snapshot, persist_path, new_geometry) = {
            let Ok(mut state) = self.state.shared_state.try_lock() else {
                tracing::warn!("persist_drag_release: could not acquire shared_state lock");
                return;
            };

            let new_geometry = tze_hud_input::drag::final_position_to_geometry(
                released.bounds.x,
                released.bounds.y,
                released.bounds.width,
                released.bounds.height,
                released.display.0,
                released.display.1,
            );

            if released.group_members.is_empty() {
                // Plain single-tile move: match the store entry by namespace and
                // write its geometry override (the pre-existing path).
                tze_hud_input::drag::persist_geometry_override(
                    &mut state.element_store,
                    ElementType::Tile,
                    &released.namespace,
                    new_geometry,
                );
            } else {
                // Shared namespaces need the canonical per-member writer.
                write_member_geometry_overrides(
                    &mut state.element_store,
                    &released.group_members,
                    released.display.0,
                    released.display.1,
                );
            }

            let store_snapshot = state.element_store.clone();
            let persist_path = state.element_store_path.clone();
            (store_snapshot, persist_path, new_geometry)
        };

        // Persist element store on a background thread (avoids blocking the
        // winit event loop with sync disk I/O).
        persist_element_store_in_background(store_snapshot, persist_path);

        // Broadcast ElementRepositionedEvent so gRPC subscribers are notified.
        if let Some(ref tx) = self.state.element_repositioned_tx {
            let event = tze_hud_protocol::proto::ElementRepositionedEvent {
                element_id: released.element_id.as_uuid().as_bytes().to_vec(),
                new_geometry: Some(tze_hud_protocol::convert::geometry_policy_to_proto(
                    &new_geometry,
                )),
                previous_geometry: None,
            };
            // Errors (no receivers, channel lagged) are silently ignored —
            // ElementRepositionedEvent is an ephemeral-realtime notification;
            // subscribers that are not present at the moment of a drag release
            // will learn the final geometry on next subscription refresh.
            tx.send(event).unwrap_or_default();
            tracing::debug!(
                element_id = %released.element_id,
                final_x = released.bounds.x,
                final_y = released.bounds.y,
                "ElementRepositionedEvent broadcast after drag release"
            );
        }
    }

    /// Complete resize notifications and optional terminal persistence after unlock.
    pub(super) fn finish_portal_resize_members(
        &mut self,
        members: &[PortalMemberGeometry],
        display_w: f32,
        display_h: f32,
        persist: bool,
    ) {
        let mut bounds = Vec::with_capacity(members.len());
        for member in members {
            dispatch_portal_geometry_event(
                &self.state.element_repositioned_tx,
                member.tile_id,
                &member.snapshot,
                display_w,
                display_h,
            );
            let r = member.snapshot.rect;
            bounds.push((
                member.tile_id,
                tze_hud_scene::Rect::new(r.x, r.y, r.width, r.height),
            ));
        }
        if persist {
            self.persist_portal_member_overrides(&bounds, display_w, display_h);
        }
    }

    /// Persist a durable, id-keyed geometry override for each `(SceneId, rect)`
    /// pair and flush the element store to disk on a background thread.
    ///
    /// Used by the whole-portal resize path (hud-8vejp) to give every group
    /// member its own durable override after a viewer resize — the drag path
    /// persists inline in [`Self::persist_drag_release`]. `rect` is absolute
    /// display-pixel bounds; it is normalised to a `Relative` policy against the
    /// supplied display size before being stored, matching how drag-release and
    /// adapter republishes resolve geometry.
    ///
    /// A no-op (no lock, no disk write) when `members` is empty. Lock contention
    /// on `shared_state` is logged and skipped: the transient in-session
    /// `viewer_geometry_locked` set already keeps the group coherent this
    /// session, and the next viewer gesture will retry the durable write.
    pub(super) fn persist_portal_member_overrides(
        &mut self,
        members: &[(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)],
        display_w: f32,
        display_h: f32,
    ) {
        if members.is_empty() {
            return;
        }

        let (store_snapshot, persist_path) = {
            let Ok(mut state) = self.state.shared_state.try_lock() else {
                tracing::warn!(
                    "persist_portal_member_overrides: could not acquire shared_state lock"
                );
                return;
            };
            write_member_geometry_overrides(
                &mut state.element_store,
                members,
                display_w,
                display_h,
            );
            (
                state.element_store.clone(),
                state.element_store_path.clone(),
            )
        };

        persist_element_store_in_background(store_snapshot, persist_path);
    }

    /// Called from `about_to_wait` after composer-draft flush.  Drives
    /// `PortalDriver::drain` which calls
    /// `InputProcessor::notify_tile_content_appended` for every `RenderPortal`
    /// drain record that carries append geometry (spec §3.2 / §3.3).
    ///
    /// Uses `try_lock` on the shared scene to avoid blocking the main thread.
    /// If the scene lock is busy, returns [`PortalProjectionDrain::Deferred`]
    /// so `about_to_wait` can arrange one release-driven availability wake.
    pub(super) fn drain_portal_projection(&mut self) -> PortalProjectionDrain {
        let Ok(state) = self.state.shared_state.try_lock() else {
            tracing::trace!("portal drain deferred: shared_state lock busy");
            return PortalProjectionDrain::Deferred;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            tracing::trace!("portal drain deferred: scene lock busy");
            return PortalProjectionDrain::Deferred;
        };
        let tab_id = scene.active_tab;
        let scene_version_before = scene.version;
        let geometry_epoch_before = scene.geometry_epoch;
        self.state.portal_projection_driver.drain(
            &mut scene,
            &mut self.state.input_processor,
            tab_id,
        );
        let scene_changed =
            scene.version != scene_version_before || scene.geometry_epoch != geometry_epoch_before;
        // The driver activates a tab when a cooperative portal needs to render
        // and none was active (hud-obw3q). Keep the lock-free keyboard-dispatch
        // mirror in sync so keyboard routing targets the newly active tab.
        if scene.active_tab != tab_id {
            state.refresh_active_tab_mirror(&scene);
        }
        PortalProjectionDrain::Completed { scene_changed }
    }

    /// Remove stale resize, history and drag state. Returns whether drag feedback
    /// changed, so a main-only availability wake can admit its cleared frame.
    ///
    /// Called once per `about_to_wait` iteration.  Uses a two-phase approach:
    ///
    /// 1. **Eager drain** — drains `SceneGraph::recently_removed_tile_ids`
    ///    (populated by `remove_tile_and_nodes` on every `DeleteTile` mutation)
    ///    and removes each returned ID from both maps immediately. This is
    ///    O(removed) and requires only the scene lock.
    ///
    /// 2. **Fallback sweep** — if `portal_resize_states` is still non-empty
    ///    after the drain (e.g. entries that predated the drain queue, or
    ///    entries whose tiles were removed while the lock was busy), a full
    ///    `retain` sweep validates all remaining entries against the live tile
    ///    set.  Uses `try_lock`; deferred to next iteration if lock is busy.
    ///
    /// The eager path handles the common case (each `DeleteTile` generates
    /// exactly one removal notification).  The fallback prevents unbounded
    /// accumulation in pathological cases.
    pub(super) fn prune_portal_resize_states(&mut self) -> bool {
        let original_window_is_live = self.primary_drag_window_is_live();
        let Ok(state) = self.state.shared_state.try_lock() else {
            tracing::trace!("portal resize prune deferred: shared_state lock busy");
            return false;
        };
        let Ok(mut scene) = state.scene.try_lock() else {
            tracing::trace!("portal resize prune deferred: scene lock busy");
            return false;
        };
        let epoch_before = scene.geometry_epoch;

        self.state.input_processor.drag_states.retain(|_, drag| {
            DragTarget::capture(&scene, drag.element_id, drag.element_kind).is_some()
        });
        if self
            .state
            .primary_drag_gesture
            .as_ref()
            .is_some_and(|gesture| !original_window_is_live || !gesture.target_is_live(&scene))
            && let Some(gesture) = self.state.primary_drag_gesture.take()
            && self
                .state
                .input_processor
                .drag_states
                .get(&0)
                .is_some_and(|drag| gesture.matches(drag))
        {
            self.state.input_processor.drag_states.remove(&0);
        }
        reconcile_drag_feedback(&self.state.input_processor, &mut scene);

        // Phase 1: eager drain — O(removed), handles the common `DeleteTile` path.
        let removed_ids = scene.drain_removed_tile_ids();
        let mut eagerly_removed = 0usize;
        for tile_id in removed_ids {
            let resize_removed = self.state.portal_resize_states.remove(&tile_id).is_some();
            if resize_removed {
                eagerly_removed += 1;
            }
        }
        if eagerly_removed > 0 {
            tracing::debug!(
                removed = eagerly_removed,
                resize_remaining = self.state.portal_resize_states.len(),
                "portal resize: eagerly pruned stale per-tile state for removed tiles"
            );
        }

        // Phase 2: fallback sweep — catches any entries that slipped through
        // (e.g. tiles removed before the drain queue existed, or during a
        // prior lock-busy deferral).
        if self.state.portal_resize_states.is_empty() {
            return scene.geometry_epoch != epoch_before;
        }
        let resize_before = self.state.portal_resize_states.len();
        self.state
            .portal_resize_states
            .retain(|tile_id, _| scene.tiles.contains_key(tile_id));
        let swept = resize_before - self.state.portal_resize_states.len();
        if swept > 0 {
            tracing::debug!(
                removed = swept,
                resize_remaining = self.state.portal_resize_states.len(),
                "portal resize: sweep-pruned stale per-tile state for removed tiles"
            );
        }
        scene.geometry_epoch != epoch_before
    }
    /// Apply a Ctrl-gated portal resize hotkey to the focused portal tile.
    ///
    /// Looks up the currently focused tile in `tab_id`. If the focused tile is
    /// a portal tile (has a registered scroll config), applies the resize step
    /// locally (local-first per §6b.2), updates the scene tile bounds, and
    /// broadcasts an `ElementRepositionedEvent` on the `SCENE_TOPOLOGY` channel
    /// via `element_repositioned_tx` so gRPC subscribers receive the updated
    /// portal geometry (relative %).
    ///
    /// Returns `true` when the hotkey was consumed (applied to a focused portal
    /// tile) so the caller knows to stop propagating the key event.
    /// Returns `false` when no focused portal tile was found (the key must not
    /// be consumed and should fall through to the normal dispatch path).
    ///
    /// `axis` selects which axis/axes the step grows/shrinks:
    /// [`HotkeyResizeAxis::Both`](tze_hud_input::HotkeyResizeAxis::Both) for the
    /// symmetric Ctrl+`+`/`-` chord, or
    /// [`Width`](tze_hud_input::HotkeyResizeAxis::Width) /
    /// [`Height`](tze_hud_input::HotkeyResizeAxis::Height) for the directional
    /// Ctrl+Shift+Arrow chord (hud-csrmf). A width step reflows the transcript
    /// text (it changes the wrap column) and takes the viewer-geometry-lock, so
    /// it drives the dynamic hud-rpmwt reconcile-on-republish path without pointer
    /// injection.
    pub(super) fn apply_portal_resize_hotkey(
        &mut self,
        tab_id: tze_hud_scene::SceneId,
        dir: tze_hud_input::HotkeyResizeDir,
        axis: tze_hud_input::HotkeyResizeAxis,
    ) -> bool {
        // Resolve the focused tile from the focus manager.
        let focused_tile_id = match self.state.focus_manager.current_owner(tab_id).tile_id() {
            Some(id) => id,
            None => return false,
        };

        let display_w = self.state.config.window.width as f32;
        let display_h = self.state.config.window.height as f32;

        // Acquire scene + check if the focused tile is a portal surface (has a
        // scroll config), resolve the whole-portal group, and build the bounds
        // for clamping the whole-portal rect.
        let (group, old_rect, bounds) = {
            // A resize hotkey is a deliberate user action that must produce
            // visible feedback this frame; acquire with a bounded spin rather
            // than a single try_lock so a contended scene lock (compositor /
            // streaming publish) cannot silently swallow the resize.
            let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET)
            else {
                return false;
            };
            let Some(scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) else {
                return false;
            };
            // Only scrollable portal tiles accept resize hotkeys.
            if scene.tile_scroll_config(focused_tile_id).is_none() {
                return false;
            }
            // Resolve the whole portal (frame anchor + constituent surfaces)
            // that owns the focused surface, so the step scales the unit rather
            // than the single focused tile (hud-fb3en).
            let Some(group) = resolve_portal_group(&scene, focused_tile_id) else {
                return false;
            };
            let Some(anchor_lease) = scene.tiles.get(&group.anchor_tile_id).map(|t| t.lease_id)
            else {
                return false;
            };
            let old_rect = group.portal_rect;
            let portal_part = tze_hud_config::resolve_portal_tokens(&self.state.global_tokens);
            let tokens = PortalWindowTokens {
                min_width_px: portal_part.window_min_width_px,
                min_height_px: portal_part.window_min_height_px,
                resize_step_px: portal_part.window_resize_step_px,
                affordance_px: portal_part.window_resize_affordance_px,
            };
            // Resolve spatial budget from the anchor/frame's lease (the
            // whole-portal rect clamps against the portal's lease budget).
            let (lease_max_w, lease_max_h) = scene
                .leases
                .get(&anchor_lease)
                .map(|l| {
                    (
                        l.spatial_budget.max_tile_width_px,
                        l.spatial_budget.max_tile_height_px,
                    )
                })
                .unwrap_or((0.0, 0.0));
            let (max_width_px, max_height_px) = compute_portal_max_dims(
                lease_max_w,
                lease_max_h,
                display_w,
                display_h,
                tokens.min_width_px,
                tokens.min_height_px,
            );
            let resize_bounds = ResizeBounds {
                tokens,
                max_width_px,
                max_height_px,
                display_w,
                display_h,
            };
            (group, old_rect, resize_bounds)
        };

        // Get or lazily create the per-portal resize state, keyed by the anchor
        // (frame) tile so one gesture state tracks the whole portal.
        let resize_state = self
            .state
            .portal_resize_states
            .entry(group.anchor_tile_id)
            .or_insert_with(|| PortalResizeState::new(group.portal_id_hash));

        // Apply the hotkey resize to the whole-portal rect (O(1) on the hot path).
        let outcome = tze_hud_input::apply_hotkey_resize_axis(
            true, // portal is focused (checked above)
            dir,
            axis,
            old_rect,
            &bounds,
            resize_state,
        );

        let snapshot = match outcome {
            tze_hud_input::HotkeyResizeOutcome::Applied { snapshot } => snapshot,
            tze_hud_input::HotkeyResizeOutcome::NotFocused => return false,
        };

        // Local-first feedback: scale every constituent surface immediately in
        // the scene (same frame, no adapter roundtrip) per §6b.2 /
        // local-feedback-first, preserving relative layout around the top-left
        // anchor. `commit_portal_group_resize` bumps `scene.version` once when
        // the geometry actually changes, so the compositor re-primes the
        // truncation cache at the new geometry (hud-ghhxa — spec §6b.3) without
        // churning at a clamped boundary.
        let members = {
            let Some(state) = spin_acquire(&self.state.shared_state, INTERACTION_LOCK_BUDGET)
            else {
                return true; // hotkey consumed even if local update fails
            };
            let Some(mut scene) = spin_acquire(&state.scene, INTERACTION_LOCK_BUDGET) else {
                return true;
            };
            commit_portal_group_resize(&mut scene, &group, old_rect, snapshot)
        };

        // A hotkey is a discrete committed step; pointer completion passes its
        // terminal flag to the same post-lock broadcast/persistence seam.
        self.finish_portal_resize_members(&members, display_w, display_h, true);

        tracing::debug!(
            anchor_tile_id = ?group.anchor_tile_id,
            members = members.len(),
            new_width = snapshot.rect.width,
            new_height = snapshot.rect.height,
            sequence = snapshot.sequence,
            "portal resize: hotkey applied — whole-portal bounds updated locally"
        );

        true // hotkey consumed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tze_hud_input::{
        FocusManager, FocusRequest, InputProcessor, PointerEvent, PointerEventKind,
        PortalResizeState, PortalWindowTokens,
    };

    use super::super::WinitApp;
    use super::super::test_support::{
        make_windowed_keyboard_test_app, multi_surface_portal_scene, portal_scene_with_focus,
        scene_with_drag_handle_tile,
    };
    use super::*;

    // ── Drag-to-move: long-press drag moves a text stream portal tile [hud-9yfce] ──

    /// A long-press drag on a tile's drag handle must move the tile's bounds and
    /// return a `DragReleasedData` payload when the pointer is released.
    ///
    /// Acceptance criteria for hud-9yfce:
    /// - `Moved` outcome during pointer-move updates `tile.bounds` immediately.
    /// - `Released` outcome on pointer-up produces persist data.
    /// - Click-focus is unaffected: a short tap (no long-press) produces no move.
    #[test]
    fn drag_to_move_long_press_moves_tile_bounds() {
        use std::time::Duration;
        use tze_hud_input::{InputProcessor, PointerEvent};

        let (mut scene, tile_id, element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);

        // The drag handle was placed at:
        //   x = 400 + 600/2 - 20 = 680, y = 300 - 10 = 290, w=40, h=20
        // So the handle spans x: 680..720, y: 290..310.
        let handle_cx = 700.0_f32; // centre of the handle
        let handle_cy = 300.0_f32;

        let mut processor = InputProcessor::new();

        // ── Step 1: PointerDown on the drag handle ────────────────────────────
        let down = PointerEvent {
            x: handle_cx,
            y: handle_cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        // process() produces the HitResult for the drag handle.
        let result_down = processor.process(&down, &mut scene);
        assert!(
            result_down.hit.is_zone_interaction(),
            "pointer-down on drag handle must produce ZoneInteraction hit"
        );

        // Drive the drag state machine — should start accumulating.
        let released_on_down = super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_down.is_none(),
            "PointerDown must not trigger release"
        );
        assert!(
            processor.drag_states.contains_key(&0),
            "drag state must be created for device 0 after PointerDown on handle"
        );

        assert!(!scene.is_drag_active(tile_id), "accumulation has no border");
        // Age the existing recognizer's press; no wall-clock wait is needed.
        processor.drag_states.get_mut(&0).unwrap().press_start -= Duration::from_millis(250);

        // ── Step 3: PointerMove — first move activates the drag, second moves tile ──
        //
        // The state machine on the first PointerMove after the threshold transitions
        // Accumulating → Activated (returns `Activated`, not `Moved` yet).  The
        // grab offset is recorded at activation.  Subsequent PointerMove events
        // return `Moved` and update the tile bounds.
        let move1 = PointerEvent {
            x: handle_cx + 5.0, // small nudge triggers Activated
            kind: PointerEventKind::Move,
            ..down
        };
        let result_move1 = processor.process(&move1, &mut scene);
        let released_on_move1 = super::apply_drag_handle_pointer_event(
            &mut processor,
            &move1,
            &result_move1.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_move1.is_none(),
            "first PointerMove (Activated) must not trigger release"
        );
        assert!(
            scene.is_drag_active(tile_id),
            "activation must publish local feedback"
        );

        // Second PointerMove — now in Activated phase, returns Moved.
        let move2 = PointerEvent {
            x: handle_cx + 100.0,
            y: handle_cy + 50.0,
            kind: PointerEventKind::Move,
            device_id: 0,
            timestamp: None,
        };
        let result_move2 = processor.process(&move2, &mut scene);
        let released_on_move2 = super::apply_drag_handle_pointer_event(
            &mut processor,
            &move2,
            &result_move2.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            released_on_move2.is_none(),
            "PointerMove must not trigger release"
        );

        // The tile must have moved from its original position.
        let tile_after_move = scene.tiles.get(&tile_id).expect("tile must exist");
        assert_ne!(
            tile_after_move.bounds.x, 400.0,
            "tile X must change after drag move"
        );
        assert_ne!(
            tile_after_move.bounds.y, 300.0,
            "tile Y must change after drag move"
        );

        // ── Step 4: PointerUp — should release and return persist data ────────
        // Release at the same position as the last move.
        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..move2
        };
        let result_up = processor.process(&up, &mut scene);
        // A second activated device owns the same feedback target independently.
        processor
            .drag_states
            .insert(1, processor.drag_states.get(&0).unwrap().clone());
        let (tab, lease) = {
            let tile = &scene.tiles[&tile_id];
            (tile.tab_id, tile.lease_id)
        };
        let unrelated_id = scene
            .create_tile(
                tab,
                "portal-agent",
                lease,
                tze_hud_scene::Rect::new(20.0, 20.0, 100.0, 80.0),
                0,
            )
            .unwrap();
        let mut unrelated_drag = processor.drag_states[&0].clone();
        unrelated_drag.element_id = unrelated_id;
        unrelated_drag.interaction_id = "unrelated-device-drag".to_string();
        processor.drag_states.insert(2, unrelated_drag);
        let released_on_up = super::apply_drag_handle_pointer_event(
            &mut processor,
            &up,
            &result_up.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        let Some(DragRelease::Tile(released)) = released_on_up else {
            panic!("PointerUp after activated tile drag must return tile persistence data");
        };
        assert_eq!(
            released.element_id, element_id,
            "released element_id must match the dragged tile"
        );
        assert!(
            released.bounds.x >= 0.0 && released.bounds.x + released.bounds.width <= 1920.0,
            "final X must be within display bounds"
        );
        assert!(
            released.bounds.y >= 0.0 && released.bounds.y + released.bounds.height <= 1080.0,
            "final Y must be within display bounds"
        );

        // Drag state must be cleaned up after release.
        assert!(
            !processor.drag_states.contains_key(&0),
            "drag state must be removed after PointerUp"
        );
        assert!(
            scene.is_drag_active(tile_id),
            "the second device still owns the border"
        );
        let other_up = PointerEvent { device_id: 1, ..up };
        assert!(
            super::apply_drag_handle_pointer_event(
                &mut processor,
                &other_up,
                &HitResult::Passthrough,
                &mut scene,
                1920.0,
                1080.0,
            )
            .is_some()
        );
        assert!(
            !scene.is_drag_active(tile_id),
            "the last release clears the border"
        );
        assert!(
            scene.is_drag_active(unrelated_id),
            "an unrelated device's target remains active"
        );
        let unrelated_up = PointerEvent {
            device_id: 2,
            ..other_up.clone()
        };
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &unrelated_up,
            &HitResult::Passthrough,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(!scene.is_drag_active(unrelated_id));
        let result_down = processor.process(&down, &mut scene);
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        processor.drag_states.get_mut(&0).unwrap().press_start -= Duration::from_millis(250);
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &move1,
            &HitResult::Passthrough,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(scene.is_drag_active(tile_id));
        scene.delete_tile(tile_id, "portal-agent").unwrap();
        assert!(
            super::apply_drag_handle_pointer_event(
                &mut processor,
                &other_up,
                &HitResult::Passthrough,
                &mut scene,
                1920.0,
                1080.0
            )
            .is_none()
        );
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &PointerEvent {
                device_id: 0,
                ..other_up
            },
            &HitResult::Passthrough,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            !processor.drag_states.contains_key(&0),
            "removed targets cannot resume a stale drag"
        );
        assert!(!scene.is_drag_active(tile_id));
    }

    /// A quick tap (PointerDown immediately followed by PointerUp, no long-press)
    /// on a drag handle must NOT move the tile — the click-to-focus path must be
    /// unaffected.
    ///
    /// A tap (down then up, no move) is a click, not a drag.
    #[test]
    fn drag_to_move_quick_tap_does_not_move_tile() {
        use tze_hud_input::{InputProcessor, PointerEvent};

        let (mut scene, tile_id, _element_id, _interaction_id) =
            scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);

        // Same drag handle position as drag_to_move_long_press_moves_tile_bounds:
        //   x: 680..720, y: 290..310.
        let handle_cx = 700.0_f32; // centre of the handle
        let handle_cy = 300.0_f32;

        let mut processor = InputProcessor::new();

        // PointerDown.
        let down = PointerEvent {
            x: handle_cx,
            y: handle_cy,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let result_down = processor.process(&down, &mut scene);
        let _ = super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        // PointerUp immediately — no long-press threshold met.
        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..down
        };
        let result_up = processor.process(&up, &mut scene);
        let released_on_up = super::apply_drag_handle_pointer_event(
            &mut processor,
            &up,
            &result_up.hit,
            &mut scene,
            1920.0,
            1080.0,
        );

        // Must NOT return release data — this was a tap, not a drag.
        assert!(
            released_on_up.is_none(),
            "quick tap must not trigger drag release [click-focus coexistence]"
        );

        // Tile bounds must be unchanged.
        let tile = scene.tiles.get(&tile_id).expect("tile must exist");
        assert_eq!(
            tile.bounds.x, 400.0,
            "tile X must not change after a tap on the drag handle"
        );
        assert_eq!(
            tile.bounds.y, 300.0,
            "tile Y must not change after a tap on the drag handle"
        );
        // The handle's own hover/press feedback repaints (hud-paslt), but a
        // quick tap never enters drag feedback.
        assert!(!scene.is_drag_active(tile_id));
        let result_down = processor.process(&down, &mut scene);
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &down,
            &result_down.hit,
            &mut scene,
            1920.0,
            1080.0,
        );
        // A mouse drags a grip on its first move, like a window titlebar: no
        // hold needed (the owner found the long-press grip read as dead).
        let fast_move = PointerEvent {
            x: handle_cx + 30.0,
            kind: PointerEventKind::Move,
            ..down
        };
        super::apply_drag_handle_pointer_event(
            &mut processor,
            &fast_move,
            &HitResult::Passthrough,
            &mut scene,
            1920.0,
            1080.0,
        );
        assert!(
            scene.is_drag_active(tile_id),
            "a mouse drag starts on the first move"
        );
    }

    // ── Portal control keyboard recovery / activation (hud-2v8br) ──────────
    //
    // Build a portal tile whose root composer (accepts_composer_input) has a
    // non-composer minimize control as a focusable child, land Tab focus on the
    // control, and assert the runtime never strands the keyboard user.

    // ── Pointer-affordance portal resize ─────────────────────────────────────

    /// A pointer-down on a resize affordance starts the gesture:
    ///   - `gesture_active()` becomes true.
    ///   - An outcome (snapshot) is returned.
    ///   - The snapshot's `gesture_active` flag is true.
    ///   - Tile bounds are unchanged (clamped initial = current).
    #[test]
    fn pointer_down_on_affordance_starts_gesture() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Tile is at (100, 100, 400, 300). Affordance strip is 8px.
        // Pointer at (496, 250) is on the right edge (100+400-8 = 492 ≤ 496 ≤ 500).
        let event = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };

        let outcome = apply_portal_resize_pointer_event(
            &event,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-down on affordance must return an outcome"
        );
        let outcome = outcome.unwrap();
        assert_eq!(
            outcome.members[0].tile_id, tile_id,
            "outcome must reference the focused portal tile"
        );
        assert!(
            outcome.members[0].snapshot.gesture_active,
            "snapshot must have gesture_active=true on pointer-down"
        );

        // Resize state map must now have an entry with gesture_active=true.
        let resize_state = resize_states
            .get(&tile_id)
            .expect("resize state must be created on pointer-down");
        assert!(
            resize_state.gesture_active(),
            "gesture_active() must be true after pointer-down on affordance"
        );
    }

    /// A pointer-move during an active gesture applies local bounds immediately
    /// (local-first feedback), and the tile width grows as the pointer moves right.
    #[test]
    fn pointer_move_during_gesture_updates_tile_bounds_locally() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on the right edge.
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        let width_before = scene.tiles[&tile_id].bounds.width;

        // Move pointer 20px to the right → right edge should grow by 20px.
        let mv = PointerEvent {
            x: 516.0,
            kind: PointerEventKind::Move,
            ..down
        };
        let outcome = apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-move during gesture must return an outcome"
        );
        let new_width = scene.tiles[&tile_id].bounds.width;
        assert!(
            new_width > width_before,
            "tile width must grow when pointer moves right on right edge: before={width_before}, after={new_width}"
        );
    }

    /// A pointer-up ends the gesture and broadcasts the final geometry snapshot.
    ///   - After pointer-up, `gesture_active()` becomes false.
    ///   - The final snapshot's `gesture_active` is false.
    ///   - Tile bounds reflect the final clamped position.
    ///   - A geometry event outcome is returned (for broadcasting).
    #[test]
    fn pointer_up_ends_gesture_and_returns_geometry_event() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on the right edge.
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Move to establish a drag delta.
        let mv = PointerEvent {
            x: 530.0,
            kind: PointerEventKind::Move,
            ..down
        };
        apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Release pointer.
        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..mv
        };
        let outcome = apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_some(),
            "pointer-up must return a geometry event outcome"
        );
        let outcome = outcome.unwrap();
        assert!(
            !outcome.members[0].snapshot.gesture_active,
            "snapshot gesture_active must be false after pointer-up"
        );
        assert_eq!(
            outcome.members[0].tile_id, tile_id,
            "outcome tile_id must match the resized portal"
        );

        // gesture_active() must be false after the last device lifts.
        let resize_state = resize_states
            .get(&tile_id)
            .expect("resize state must exist");
        assert!(
            !resize_state.gesture_active(),
            "gesture_active() must be false after pointer-up"
        );

        // Final tile width should reflect the drag delta (496→530 = +34px on right edge).
        let final_width = scene.tiles[&tile_id].bounds.width;
        assert!(
            final_width > 400.0,
            "tile width must be larger than initial 400px after rightward drag: {final_width}"
        );
    }

    /// Move/up routing must find the active resize gesture for the current
    /// `device_id`, not merely the first portal that has any active gesture.
    #[test]
    fn multi_device_resize_move_and_up_route_to_matching_device_gesture() {
        use tze_hud_scene::Rect;
        use tze_hud_scene::types::TileScrollConfig;

        let (mut scene, tab_id, tile_a, mut fm) = portal_scene_with_focus();
        let lease_b = scene.grant_lease("portal-agent-b", 60_000);
        let tile_b = scene
            .create_tile(
                tab_id,
                "portal-agent-b",
                lease_b,
                Rect::new(700.0, 100.0, 400.0, 300.0),
                2,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(tile_b, TileScrollConfig::vertical())
            .unwrap();

        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;
        let tokens = PortalWindowTokens::default();

        let down_a = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        let outcome_a = apply_portal_resize_pointer_event(
            &down_a,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("device 1 down must start a resize on tile A");
        assert_eq!(outcome_a.members[0].tile_id, tile_a);

        let (focus_result, _) = fm.request_focus(
            FocusRequest {
                tile_id: tile_b,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent-b".to_string(),
            },
            tab_id,
            &scene,
        );
        assert_eq!(
            focus_result,
            tze_hud_input::FocusResult::Granted,
            "test setup must focus the second portal tile"
        );

        let down_b = PointerEvent {
            x: 1096.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 2,
            timestamp: None,
        };
        let outcome_b = apply_portal_resize_pointer_event(
            &down_b,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("device 2 down must start a resize on tile B");
        assert_eq!(outcome_b.members[0].tile_id, tile_b);

        let active_order = resize_states
            .iter()
            .filter(|(_, state)| state.gesture_active())
            .map(|(&tile_id, _)| tile_id)
            .collect::<Vec<_>>();
        assert_eq!(
            active_order.len(),
            2,
            "test setup must have two concurrently active portal resize gestures"
        );

        let first_iterated_tile = active_order[0];
        let target_tile = active_order[1];
        let (target_device_id, target_left_x, other_tile) = if target_tile == tile_a {
            (1, 100.0, tile_b)
        } else {
            (2, 700.0, tile_a)
        };

        let target_width_before = scene.tiles[&target_tile].bounds.width;
        let other_width_before = scene.tiles[&other_tile].bounds.width;
        let mv = PointerEvent {
            x: target_left_x + 430.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: target_device_id,
            timestamp: None,
        };
        let move_outcome = apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("move must find the portal whose gesture belongs to this device");

        assert_eq!(
            move_outcome.members[0].tile_id, target_tile,
            "move must update the active portal for the current device"
        );
        assert!(
            scene.tiles[&target_tile].bounds.width > target_width_before,
            "target portal width must grow after moving its right-edge gesture"
        );
        assert_eq!(
            scene.tiles[&other_tile].bounds.width, other_width_before,
            "move for one device must not mutate the other active portal"
        );

        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..mv
        };
        let up_outcome = apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("up must end the portal gesture for the current device");

        assert_eq!(
            up_outcome.members[0].tile_id, target_tile,
            "up must end the active portal for the current device"
        );
        assert!(
            !resize_states[&target_tile].gesture_active(),
            "target portal gesture must end after pointer-up"
        );
        assert!(
            resize_states[&first_iterated_tile].gesture_active(),
            "the other portal gesture must remain active"
        );
    }

    /// gesture_active() is true during the drag, making adapter publishes
    /// rejectable — the primary reason this code path must exist in production
    /// (hud-o0st9 acceptance criterion: gesture_active becomes reachable).
    #[test]
    fn gesture_active_is_true_during_pointer_drag() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Before any gesture, no entry exists — gesture_active is implicitly false.
        assert!(
            resize_states
                .get(&tile_id)
                .is_none_or(|s: &PortalResizeState| !s.gesture_active()),
            "gesture must not be active before pointer-down"
        );

        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            resize_states[&tile_id].gesture_active(),
            "gesture_active() must be true between pointer-down and pointer-up"
        );

        let up = PointerEvent {
            x: 510.0,
            kind: PointerEventKind::Up,
            ..down
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            !resize_states[&tile_id].gesture_active(),
            "gesture_active() must be false after pointer-up"
        );
    }

    /// A pointer-down outside any affordance (in the content area) must NOT
    /// start a gesture.
    #[test]
    fn pointer_down_in_content_area_does_not_start_gesture() {
        let (mut scene, tab_id, tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Tile is at (100, 100, 400, 300). Content area center is at (300, 250).
        let event = PointerEvent {
            x: 300.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };

        let outcome = apply_portal_resize_pointer_event(
            &event,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        assert!(
            outcome.is_none(),
            "pointer-down in content area must not start a gesture"
        );
        assert!(
            resize_states
                .get(&tile_id)
                .is_none_or(|s| !s.gesture_active()),
            "no gesture must be active after content-area pointer-down"
        );
    }

    // ── Lease-bound maxima (hud-kgu8u / hud-zleu2) ───────────────────────────

    /// Portal resize gesture respects the lease-bound maximum:
    /// dragging the right edge beyond the lease limit is clamped to the limit.
    #[test]
    fn pointer_drag_respects_lease_bound_maximum() {
        use tze_hud_input::FocusManager;
        use tze_hud_scene::{Rect, SceneGraph};

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        // Tile at (100, 100, 400, 300) — starts at 400×300.
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(100.0, 100.0, 400.0, 300.0),
                1,
            )
            .unwrap();
        // Set a tight lease budget on the authoritative lease entry:
        // max 450 wide, 350 tall.
        if let Some(lease) = scene.leases.get_mut(&lease_id) {
            lease.spatial_budget.max_tile_width_px = 450.0;
            lease.spatial_budget.max_tile_height_px = 350.0;
        }
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::types::TileScrollConfig::vertical(),
            )
            .unwrap();

        let mut fm = FocusManager::new();
        fm.request_focus(
            tze_hud_input::FocusRequest {
                tile_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );

        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Start gesture on right edge (100+400-8=492 ≤ 496 ≤ 500).
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        // Drag far to the right — would grow the tile to >> 450px without a lease clamp.
        let mv = PointerEvent {
            x: 800.0,
            kind: PointerEventKind::Move,
            ..down
        };
        apply_portal_resize_pointer_event(
            &mv,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );

        let width_after_drag = scene.tiles[&tile_id].bounds.width;
        assert!(
            width_after_drag <= 450.0,
            "tile width must be clamped to lease budget 450.0, got {width_after_drag}"
        );
    }

    // ── portal_resize_states map pruning (hud-kgu8u) ─────────────────────────

    // ── scene.version increment on pointer-drag resize (hud-g1hij) ───────────

    /// A pointer-drag resize (on_pointer_move → GestureUpdate) increments
    /// `scene.version` exactly once when the tile bounds change, matching the
    /// hotkey resize path (hud-ghhxa / hud-g1hij).
    ///
    /// Regression test — the 20Hz re-prime cadence gate in
    /// `prime_truncation_cache` is keyed on `scene.version`; if the drag path
    /// did NOT bump it, mid-drag re-truncation would never fire.
    ///
    /// Also verifies that a pointer-move at the identical position (no size
    /// delta) does NOT bump the version a second time, preventing spurious
    /// cache invalidations when the gesture is clamped at a boundary.
    #[test]
    fn drag_resize_pointer_move_bumps_scene_version_on_size_change() {
        let (mut scene, tab_id, _tile_id, fm) = portal_scene_with_focus();
        let mut resize_states = std::collections::HashMap::new();
        let display_w = 1920.0_f32;
        let display_h = 1080.0_f32;

        // Pointer-down on the right-edge affordance (x=496 hits the 8px strip
        // of a tile whose right edge is at 100+400=500).  No version bump expected
        // on gesture start (clamped initial rect is identical to current rect).
        let version_before_down = scene.version;
        let down = PointerEvent {
            x: 496.0,
            y: 250.0,
            kind: PointerEventKind::Down,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &down,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version, version_before_down,
            "scene.version must NOT change on gesture start (bounds unchanged at down)"
        );

        // Pointer-move 20px right → right edge grows, size changes.
        // scene.version must increment exactly once.
        let version_before_move = scene.version;
        let mv_grow = PointerEvent {
            x: 516.0,
            kind: PointerEventKind::Move,
            ..down
        };
        apply_portal_resize_pointer_event(
            &mv_grow,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version,
            version_before_move + 1,
            "scene.version must increment exactly once when drag resize changes tile size"
        );

        // Pointer-move at the exact same position (no delta) — the tile size
        // is already at the value from the previous move, so `size_changed` is
        // false and the version must NOT advance again.
        let version_before_noop = scene.version;
        let mv_noop = PointerEvent {
            x: 516.0,
            y: 250.0,
            kind: PointerEventKind::Move,
            device_id: 1,
            timestamp: None,
        };
        apply_portal_resize_pointer_event(
            &mv_noop,
            &mut resize_states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            PortalWindowTokens::default(),
        );
        assert_eq!(
            scene.version, version_before_noop,
            "scene.version must NOT change when pointer-move produces no size delta"
        );
    }

    // ── Whole-portal resize (hud-fb3en) ──────────────────────────────────────

    /// Relative geometry of a surface `b` expressed as fractions of the frame
    /// `f`: (x offset, y offset, width, height) all divided by the frame rect.
    /// Two surfaces share a portal layout iff these tuples are (approx) equal
    /// before and after a resize.
    fn rel_to_frame(b: tze_hud_scene::Rect, f: tze_hud_scene::Rect) -> (f32, f32, f32, f32) {
        (
            (b.x - f.x) / f.width,
            (b.y - f.y) / f.height,
            b.width / f.width,
            b.height / f.height,
        )
    }

    fn approx_tuple(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3
            && (a.1 - b.1).abs() < 1e-3
            && (a.2 - b.2).abs() < 1e-3
            && (a.3 - b.3).abs() < 1e-3
    }

    /// Core hud-fb3en regression: focusing the composer and pressing Ctrl+= must
    /// grow the WHOLE portal — every constituent surface scales together,
    /// preserving relative layout, anchored top-left — not just the focused
    /// pane. The far-corner drag shield must NOT move.
    #[test]
    fn ctrl_resize_hotkey_scales_whole_portal_not_just_focused_surface() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };

        let frame_before = read(&app, frame_id);
        let composer_before = read(&app, composer_id);
        let transcript_before = read(&app, transcript_id);
        let shield_before = read(&app, shield_id);

        let consumed = app.apply_portal_resize_hotkey(
            tab_id,
            HotkeyResizeDir::Grow,
            tze_hud_input::HotkeyResizeAxis::Both,
        );
        assert!(
            consumed,
            "Ctrl resize hotkey must be consumed for a focused portal surface"
        );

        let frame_after = read(&app, frame_id);
        let composer_after = read(&app, composer_id);
        let transcript_after = read(&app, transcript_id);
        let shield_after = read(&app, shield_id);

        // Frame (anchor) grew, top-left anchored (origin fixed).
        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "the portal frame must grow"
        );
        assert_eq!(
            (frame_after.x, frame_after.y),
            (frame_before.x, frame_before.y),
            "grow must be anchored at the frame's top-left corner"
        );

        // Both panes scaled with the portal — not left in place, not resized in
        // isolation.
        assert!(
            composer_after.width > composer_before.width
                && composer_after.height > composer_before.height,
            "the focused composer pane must scale WITH the whole portal"
        );
        assert!(
            transcript_after.width > transcript_before.width
                && transcript_after.height > transcript_before.height,
            "the transcript pane must scale WITH the whole portal"
        );

        // Relative layout preserved for every constituent surface.
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "composer must keep its relative position/size within the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(transcript_before, frame_before),
                rel_to_frame(transcript_after, frame_after)
            ),
            "transcript must keep its relative position/size within the portal"
        );

        // The spatially-detached drag shield is not a spatial member of the
        // portal frame and must not scale or move.
        assert_eq!(
            shield_after, shield_before,
            "the far-corner drag shield must not scale/move with a portal resize"
        );
    }

    // ── Directional (Ctrl+Shift+Arrow) whole-portal WIDTH resize (hud-csrmf) ──

    /// Attach a tile-filling `TextMarkdown` root node to `tile_id` at
    /// `local_bounds`, so a node-tree scale (the compositor's wrap-width source)
    /// is observable after a resize. Returns the node id.
    fn attach_text_node(
        scene: &mut tze_hud_scene::graph::SceneGraph,
        tile_id: tze_hud_scene::SceneId,
        local_bounds: tze_hud_scene::Rect,
    ) -> tze_hud_scene::SceneId {
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };
        let id = tze_hud_scene::SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    layout: Default::default(),
                    id,
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "the quick brown fox jumps over the lazy dog".to_owned(),
                        bounds: local_bounds,
                        font_size_px: 14.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
            .unwrap();
        id
    }

    fn node_width(app: &WinitApp, node_id: tze_hud_scene::SceneId) -> f32 {
        use tze_hud_scene::types::NodeData;
        let shared = app.state.shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        match &scene.nodes.get(&node_id).unwrap().data {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        }
    }

    /// (a) Ctrl+Shift+ArrowRight width resize scales the whole-portal tile bounds
    /// AND the node tree along WIDTH ONLY, takes viewer geometry authority over
    /// every member, and leaves height untouched. Width is the text-wrap axis, so
    /// this is exactly what drives the dynamic hud-rpmwt re-wrap without pointer
    /// injection.
    #[test]
    fn ctrl_shift_arrow_width_resize_scales_whole_portal_and_locks() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (mut scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        // Transcript pane is 180×280 (tile-local node fills it).
        let node_id = attach_text_node(
            &mut scene,
            transcript_id,
            tze_hud_scene::Rect::new(0.0, 0.0, 180.0, 280.0),
        );
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };

        let frame_before = read(&app, frame_id);
        let transcript_before = read(&app, transcript_id);
        let node_w_before = node_width(&app, node_id);

        let consumed =
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        assert!(
            consumed,
            "width hotkey must be consumed for a focused portal"
        );

        let frame_after = read(&app, frame_id);
        let transcript_after = read(&app, transcript_id);
        let node_w_after = node_width(&app, node_id);

        // Frame grew in WIDTH only, top-left anchored.
        assert!(
            frame_after.width > frame_before.width,
            "frame width must grow"
        );
        assert_eq!(
            frame_after.height, frame_before.height,
            "width-axis resize must not change the frame height"
        );
        assert_eq!(
            (frame_after.x, frame_after.y),
            (frame_before.x, frame_before.y),
            "grow must stay anchored at the frame top-left"
        );

        // Transcript pane scaled width, kept height.
        assert!(
            transcript_after.width > transcript_before.width,
            "transcript pane width must scale with the whole portal"
        );
        assert_eq!(
            transcript_after.height, transcript_before.height,
            "transcript pane height must not change on a width-axis resize"
        );

        // Node tree scaled by the same WIDTH ratio → the compositor's wrap column
        // tracks the resized pane.
        assert!(
            node_w_after > node_w_before,
            "the tile's text node width must scale with the pane (wrap-column authority)"
        );

        // Viewer geometry authority taken over every member so a republish cannot
        // stomp them back (hud-lyqun) — the precondition the reconcile fix needs.
        for id in [frame_id, transcript_id, composer_id] {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(
                scene.is_viewer_geometry_locked(id),
                "every portal member must be viewer-geometry-locked after a width resize"
            );
        }
    }

    /// (b) After a Ctrl+Shift+ArrowLeft width shrink, an adapter content republish
    /// with STALE attach-time node bounds must be reconciled to the narrower pane
    /// — the exact hud-rpmwt reconcile-on-republish path, now reached via the
    /// keyboard width chord instead of a pointer drag.
    #[test]
    fn ctrl_shift_arrow_width_resize_then_republish_reconciles_to_narrower_pane() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (mut scene, tab_id, _frame_id, transcript_id, _composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let attach = tze_hud_scene::Rect::new(0.0, 0.0, 180.0, 280.0);
        let _first = attach_text_node(&mut scene, transcript_id, attach);
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        // Width SHRINK: one 32px step narrows the 400px frame → the 180px
        // transcript pane drops below its attach-time node width.
        assert!(
            app.apply_portal_resize_hotkey(
                tab_id,
                HotkeyResizeDir::Shrink,
                HotkeyResizeAxis::Width
            ),
            "width shrink hotkey must be consumed"
        );

        let transcript_after = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&transcript_id).unwrap().bounds
        };
        assert!(
            transcript_after.width < 180.0,
            "precondition: the transcript pane must have shrunk below the attach width (got {})",
            transcript_after.width
        );

        // Adapter republishes its stale (wide) attach-time node tree. The tile is
        // viewer-geometry-locked, so `set_tile_root` reconciles the republished
        // root to the resized pane instead of re-homing it to the stale column.
        let second = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            attach_text_node(&mut scene, transcript_id, attach)
        };
        let republished_w = node_width(&app, second);
        assert!(
            (republished_w - transcript_after.width).abs() < 1e-2,
            "republished node width must be reconciled to the resized pane: expected ~{}, got {}",
            transcript_after.width,
            republished_w
        );
        assert!(
            republished_w < 180.0,
            "republished node must not keep the stale attach-time wrap width"
        );
    }

    /// (c) With no focused portal the width chord is a no-op: not consumed, no
    /// bounds change, and no resize state is created.
    #[test]
    fn ctrl_shift_arrow_width_resize_noop_when_unfocused() {
        use tze_hud_input::{FocusManager, HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();
        // Fresh FocusManager with the tab but NO focus owner.
        let mut fm = FocusManager::new();
        fm.add_tab(tab_id);
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let before = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };

        let consumed =
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        assert!(
            !consumed,
            "width chord must not be consumed without a focused portal"
        );

        let after = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };
        assert_eq!(
            after, before,
            "unfocused width chord must not change any bounds"
        );
        assert!(
            app.state.portal_resize_states.is_empty(),
            "unfocused width chord must not create resize state"
        );
    }

    /// (d) A width grow is clamped so the portal never leaves the display: after
    /// many steps the right edge stays within the display bound.
    #[test]
    fn ctrl_shift_arrow_width_resize_clamps_at_display_edge() {
        use tze_hud_input::{HotkeyResizeAxis, HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());
        let display_w = app.state.config.window.width as f32;

        // Grow far past the display bound; each step clamps.
        for _ in 0..200 {
            app.apply_portal_resize_hotkey(tab_id, HotkeyResizeDir::Grow, HotkeyResizeAxis::Width);
        }

        let frame = {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&frame_id).unwrap().bounds
        };
        assert!(
            frame.x + frame.width <= display_w + 1e-3,
            "width grow must clamp the right edge to the display bound (x={} w={} display_w={})",
            frame.x,
            frame.width,
            display_w
        );
    }

    /// Register an `ElementType::Tile` entry (no override) in a shared state's
    /// element store for each id, so an id-keyed override write has a target.
    /// Mirrors the runtime tile-creation reconcile that seeds these entries.
    fn seed_tile_entries(app: &WinitApp, ids: &[tze_hud_scene::SceneId], namespace: &str) {
        use tze_hud_scene::element_store::{ElementStoreEntry, ElementType};
        let mut state = app.state.shared_state.try_lock().unwrap();
        for id in ids {
            state.element_store.entries.insert(
                *id,
                ElementStoreEntry {
                    element_type: ElementType::Tile,
                    namespace: namespace.to_string(),
                    created_at: 1,
                    last_published_at: 1,
                    z_order: 0,
                    unseen_restarts: 0,
                    geometry_override: None,
                },
            );
        }
    }

    fn has_override(app: &WinitApp, id: tze_hud_scene::SceneId) -> bool {
        let state = app.state.shared_state.try_lock().unwrap();
        state
            .element_store
            .entries
            .get(&id)
            .map(|e| e.geometry_override.is_some())
            .unwrap_or(false)
    }

    /// hud-8vejp: a whole-portal resize must write a durable per-member geometry
    /// override for EVERY constituent surface — not just one namespace/drag
    /// member — so `list_elements` reports `has_user_override=true` for all of
    /// them (the override key is `has_user_override = geometry_override.is_some()`).
    #[test]
    fn whole_portal_resize_writes_durable_override_for_every_member() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        // Seed store entries for the members (as the runtime does on tile
        // creation) so the id-keyed override write has a target.
        seed_tile_entries(
            &app,
            &[frame_id, transcript_id, composer_id],
            "portal-agent",
        );

        // Pre-condition: no member carries a user override.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                !has_override(&app, id),
                "no member should have an override before the resize"
            );
        }

        assert!(
            app.apply_portal_resize_hotkey(
                tab_id,
                HotkeyResizeDir::Grow,
                tze_hud_input::HotkeyResizeAxis::Both
            ),
            "hotkey must be consumed for a focused portal surface"
        );

        // Every member — frame, transcript, composer — now has a durable
        // override, not just the anchor/one namespace member.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                has_override(&app, id),
                "every portal member must have a durable geometry override after a whole-portal resize"
            );
        }
    }

    /// hud-8vejp: a whole-portal drag-move must write a durable per-member
    /// override for EVERY member (a portal's tiles share one namespace, so the
    /// legacy namespace-matched persist reached only one). Drives the real
    /// capture (`translate_portal_group_on_drag` + `resolve_portal_group`) and
    /// the real persist (`persist_drag_release`).
    #[test]
    fn whole_portal_drag_writes_durable_override_for_every_member() {
        use tze_hud_input::InputProcessor;

        let (scene, _tab, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());
        seed_tile_entries(
            &app,
            &[frame_id, transcript_id, composer_id, shield_id],
            "portal-agent",
        );

        // Reproduce exactly what the `Released` arm does: translate the whole
        // group, then collect each member's final bounds by SceneId.
        let (frame_final, group_members) = {
            let state = app.state.shared_state.try_lock().unwrap();
            let mut scene = state.scene.try_lock().unwrap();
            let (dx, dy) = (60.0_f32, -30.0_f32);
            assert!(
                translate_portal_group_on_drag(&mut scene, frame_id, dx, dy),
                "a multi-surface portal must engage the whole-portal translate"
            );
            let members: Vec<(tze_hud_scene::SceneId, tze_hud_scene::types::Rect)> =
                resolve_portal_group(&scene, frame_id)
                    .unwrap()
                    .member_ids
                    .iter()
                    .filter_map(|id| scene.tiles.get(id).map(|t| (*id, t.bounds)))
                    .collect();
            let frame_final = scene.tiles.get(&frame_id).unwrap().bounds;
            (frame_final, members)
        };

        let released = DragReleasedData {
            element_id: frame_id,
            bounds: frame_final,
            display: (1920.0, 1080.0),
            namespace: "portal-agent".to_string(),
            group_members,
        };
        app.persist_drag_release(DragRelease::Tile(released));

        // Every spatial portal member has a durable override.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                has_override(&app, id),
                "every portal member must have a durable geometry override after a whole-portal drag"
            );
        }
        // The far-corner drag shield is not a spatial portal member, so it is not
        // translated and gets no override.
        assert!(
            !has_override(&app, shield_id),
            "the detached drag shield is not a portal member and must not get an override"
        );
    }

    /// A non-portal (non-scrollable) focused surface must not consume the resize
    /// hotkey nor change any geometry — the whole-portal path only engages for a
    /// focused portal surface.
    #[test]
    fn ctrl_resize_hotkey_ignored_when_focused_surface_is_not_a_portal() {
        use tze_hud_input::{HotkeyResizeDir, InputProcessor};

        let (scene, tab_id, frame_id, _transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Focus the frame, which is NOT a scrollable portal surface.
        let mut fm = FocusManager::new();
        fm.request_focus(
            FocusRequest {
                tile_id: frame_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );

        let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());

        let read = |app: &WinitApp, id: tze_hud_scene::SceneId| {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&app, frame_id);

        let consumed = app.apply_portal_resize_hotkey(
            tab_id,
            HotkeyResizeDir::Grow,
            tze_hud_input::HotkeyResizeAxis::Both,
        );
        assert!(
            !consumed,
            "resize hotkey must not be consumed when the focused surface is not a portal"
        );
        assert_eq!(
            read(&app, frame_id),
            frame_before,
            "geometry must not change when the focused surface is not a portal"
        );
    }

    /// Pointer-affordance resize (dragging the frame's bottom-right corner) must
    /// also scale the WHOLE portal: the returned member set covers the frame and
    /// both panes but excludes the far-corner drag shield, and every member
    /// scales together while the shield stays put.
    #[test]
    fn pointer_affordance_resize_scales_whole_portal_from_frame() {
        use tze_hud_input::{
            PointerEvent, PointerEventKind, PortalResizeState, PortalWindowTokens,
        };

        let (mut scene, tab_id, frame_id, transcript_id, composer_id, shield_id, fm) =
            multi_surface_portal_scene();
        let mut states: std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState> =
            std::collections::HashMap::new();
        let tokens = PortalWindowTokens::default();
        let (display_w, display_h) = (1920.0_f32, 1080.0_f32);

        let read = |scene: &tze_hud_scene::graph::SceneGraph, id: tze_hud_scene::SceneId| {
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&scene, frame_id);
        let composer_before = read(&scene, composer_id);
        let transcript_before = read(&scene, transcript_id);
        let shield_before = read(&scene, shield_id);

        // PointerDown on the frame's bottom-right corner affordance (500, 400).
        let down = PointerEvent {
            x: 498.0,
            y: 398.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let out_down = apply_portal_resize_pointer_event(
            &down,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
        assert!(
            out_down.is_some(),
            "pointer-down on the frame resize affordance must start a whole-portal gesture even with the composer focused"
        );

        // PointerMove outward to grow the portal.
        let mv = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Move,
            ..down
        };
        let out_move = apply_portal_resize_pointer_event(
            &mv,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("pointer-move must update the whole portal");

        let member_ids: Vec<_> = out_move.members.iter().map(|m| m.tile_id).collect();
        assert!(
            member_ids.contains(&frame_id)
                && member_ids.contains(&composer_id)
                && member_ids.contains(&transcript_id),
            "whole-portal resize members must include the frame and both panes"
        );
        assert!(
            !member_ids.contains(&shield_id),
            "the far-corner drag shield must be excluded from resize members"
        );

        let frame_after = read(&scene, frame_id);
        let composer_after = read(&scene, composer_id);
        let transcript_after = read(&scene, transcript_id);
        let shield_after = read(&scene, shield_id);

        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "pointer resize must grow the frame"
        );
        assert!(
            composer_after.width > composer_before.width,
            "pointer resize must scale the composer pane with the portal"
        );
        assert!(
            transcript_after.width > transcript_before.width,
            "pointer resize must scale the transcript pane with the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "pointer resize must preserve the composer's relative layout"
        );
        assert_eq!(
            shield_after, shield_before,
            "pointer resize must not move the far-corner drag shield"
        );

        // End the gesture cleanly.
        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..mv
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
    }

    /// hud-yno2r regression (deterministic): whole-portal pointer resize must
    /// start from the frame's bottom-right affordance even when keyboard focus
    /// is NOT on a scrollable pane.
    ///
    /// On the live OS-pointer path the initiating click-to-focus moves focus
    /// onto the (non-scrollable) frame tile under the resize corner *before* the
    /// resize handler runs. The old gate required the *focused* tile to be
    /// scrollable (`scene.tile_scroll_config(focused_tile_id)?`), so the very
    /// click that started the gesture also disqualified it — resize was inert
    /// (Δw=Δh=0) while whole-portal move (which resolves the group from the tile
    /// under the pointer) kept working. This test pins focus to the frame — the
    /// exact post-click-to-focus state — and asserts the gesture still starts and
    /// grows every member. The prior test
    /// (`pointer_affordance_resize_scales_whole_portal_from_frame`) pre-focused
    /// the scrollable composer and so never exercised this focus state.
    #[test]
    fn pointer_resize_starts_from_frame_corner_regardless_of_focused_pane() {
        let (mut scene, tab_id, frame_id, transcript_id, composer_id, _shield_id, mut fm) =
            multi_surface_portal_scene();

        // Emulate the live post-click-to-focus state: the pointer-down on the
        // bottom-right resize corner lands on the frame tile, so click-to-focus
        // has already moved keyboard focus OFF the scrollable composer and ONTO
        // the non-scrollable frame. The old focus-gated resize returned early
        // here (frame has no scroll config).
        fm.request_focus(
            FocusRequest {
                tile_id: frame_id,
                node_id: None,
                steal: true,
                requesting_namespace: "portal-agent".to_string(),
            },
            tab_id,
            &scene,
        );
        assert_eq!(
            fm.current_owner(tab_id).tile_id(),
            Some(frame_id),
            "focus must be on the non-scrollable frame for this regression"
        );
        assert!(
            scene.tile_scroll_config(frame_id).is_none(),
            "the frame must be non-scrollable — the condition the old gate failed on"
        );

        let mut states: std::collections::HashMap<tze_hud_scene::SceneId, PortalResizeState> =
            std::collections::HashMap::new();
        let tokens = PortalWindowTokens::default();
        let (display_w, display_h) = (1920.0_f32, 1080.0_f32);

        let read = |scene: &tze_hud_scene::graph::SceneGraph, id: tze_hud_scene::SceneId| {
            scene.tiles.get(&id).unwrap().bounds
        };
        let frame_before = read(&scene, frame_id);
        let composer_before = read(&scene, composer_id);
        let transcript_before = read(&scene, transcript_id);

        // PointerDown on the frame's bottom-right corner affordance. The frame is
        // (100,100,400,300) → corner (500,400); (498,398) sits within the band.
        let down = PointerEvent {
            x: 498.0,
            y: 398.0,
            kind: PointerEventKind::Down,
            device_id: 0,
            timestamp: None,
        };
        let out_down = apply_portal_resize_pointer_event(
            &down,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
        assert!(
            out_down.is_some(),
            "pointer-down on the frame resize affordance must start a whole-portal \
             gesture even when focus is on the non-scrollable frame (hud-yno2r)"
        );

        // PointerMove outward to grow the portal.
        let mv = PointerEvent {
            x: 570.0,
            y: 470.0,
            kind: PointerEventKind::Move,
            ..down
        };
        let out_move = apply_portal_resize_pointer_event(
            &mv,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        )
        .expect("pointer-move must update the whole portal");

        let member_ids: Vec<_> = out_move.members.iter().map(|m| m.tile_id).collect();
        assert!(
            member_ids.contains(&frame_id)
                && member_ids.contains(&composer_id)
                && member_ids.contains(&transcript_id),
            "whole-portal resize members must include the frame and both panes"
        );

        let frame_after = read(&scene, frame_id);
        let composer_after = read(&scene, composer_id);
        let transcript_after = read(&scene, transcript_id);
        assert!(
            frame_after.width > frame_before.width && frame_after.height > frame_before.height,
            "pointer resize must grow the frame"
        );
        assert!(
            composer_after.width > composer_before.width,
            "the composer pane must scale with the portal"
        );
        assert!(
            transcript_after.width > transcript_before.width,
            "the transcript pane must scale with the portal"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_before, frame_before),
                rel_to_frame(composer_after, frame_after)
            ),
            "the composer must keep its relative layout within the portal"
        );

        // End the gesture cleanly.
        let up = PointerEvent {
            kind: PointerEventKind::Up,
            ..mv
        };
        apply_portal_resize_pointer_event(
            &up,
            &mut states,
            Some(tab_id),
            &fm,
            &mut scene,
            display_w,
            display_h,
            tokens,
        );
    }

    /// Grow the whole portal group that owns `frame_id` to `new_rect`, mirroring
    /// what a viewer resize gesture commits. Returns after the members have been
    /// scaled and viewer-geometry-locked.
    fn resize_group_to(
        scene: &mut tze_hud_scene::graph::SceneGraph,
        frame_id: tze_hud_scene::SceneId,
        new_rect: tze_hud_input::PortalRect,
    ) {
        let group = resolve_portal_group(scene, frame_id).expect("group must resolve");
        let old_rect = group.portal_rect;
        let snapshot = tze_hud_input::GeometrySnapshot {
            portal_id_hash: group.portal_id_hash,
            rect: new_rect,
            gesture_active: false,
            sequence: 1,
        };
        commit_portal_group_resize(scene, &group, old_rect, snapshot);
    }

    /// hud-s4lrw unit: `scale_portal_surface_parts` scales every part's
    /// tile-local bounds by the per-axis ratio, and is a no-op when the tile has
    /// no first-class surface.
    #[test]
    fn scale_portal_surface_parts_scales_tile_local_bounds() {
        use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, Rect};

        let mut scene = tze_hud_scene::graph::SceneGraph::new(800.0, 600.0);
        let tab = scene.create_tab("t", 0).unwrap();
        let lease = scene.grant_lease("ns", 120_000);
        let tile = scene
            .create_tile(tab, "ns", lease, Rect::new(0.0, 0.0, 300.0, 200.0), 1)
            .unwrap();

        // No-op before any surface is declared.
        scale_portal_surface_parts(&mut scene, tile, 2.0, 0.5);
        assert!(!scene.overlay.portal_surfaces.contains_key(&tile));

        scene.overlay.portal_surfaces.insert(
            tile,
            PortalSurface {
                parts: vec![
                    PortalPart {
                        kind: PortalPartKind::Transcript,
                        bounds: Rect::new(10.0, 20.0, 100.0, 40.0),
                        node: Some(tze_hud_scene::SceneId::new()),
                    },
                    PortalPart {
                        kind: PortalPartKind::Composer,
                        bounds: Rect::new(0.0, 150.0, 300.0, 50.0),
                        node: Some(tze_hud_scene::SceneId::new()),
                    },
                ],
                ..Default::default()
            },
        );

        scale_portal_surface_parts(&mut scene, tile, 2.0, 0.5);
        let parts = &scene.overlay.portal_surfaces.get(&tile).unwrap().parts;
        // Transcript part: x,w ×2.0 ; y,h ×0.5.
        assert_eq!(parts[0].bounds.x, 20.0);
        assert_eq!(parts[0].bounds.y, 10.0);
        assert_eq!(parts[0].bounds.width, 200.0);
        assert_eq!(parts[0].bounds.height, 20.0);
        // Composer part scales the same way.
        assert_eq!(parts[1].bounds.y, 75.0);
        assert_eq!(parts[1].bounds.width, 600.0);
        assert_eq!(parts[1].bounds.height, 25.0);
    }

    /// hud-s4lrw integration: a whole-portal resize scales the host tile's
    /// first-class `PortalSurface` part bounds in lock-step with the re-flowed
    /// node tree, so the compositor's per-part clip band stays aligned at the new
    /// geometry ("overflow invariants hold at all geometries").
    #[test]
    fn whole_portal_resize_scales_surface_part_bounds() {
        use tze_hud_scene::types::{PortalPart, PortalPartKind, PortalSurface, Rect};

        let (mut scene, _tab, frame_id, transcript_id, _composer_id, _shield, _fm) =
            multi_surface_portal_scene();

        // Attach a first-class surface to the transcript (host) tile.
        let part_node = tze_hud_scene::SceneId::new();
        scene.overlay.portal_surfaces.insert(
            transcript_id,
            PortalSurface {
                parts: vec![PortalPart {
                    kind: PortalPartKind::Transcript,
                    bounds: Rect::new(10.0, 20.0, 100.0, 40.0),
                    node: Some(part_node),
                }],
                ..Default::default()
            },
        );

        let old_tile = scene.tiles.get(&transcript_id).unwrap().bounds;
        let old_frame = scene.tiles.get(&frame_id).unwrap().bounds;
        // Grow the whole portal (anisotropic to exercise both axes).
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: old_frame.x,
                y: old_frame.y,
                width: old_frame.width * 1.4,
                height: old_frame.height * 1.2,
            },
        );
        let new_tile = scene.tiles.get(&transcript_id).unwrap().bounds;
        let r_w = new_tile.width / old_tile.width;
        let r_h = new_tile.height / old_tile.height;
        assert!(r_w > 1.0 && r_h > 1.0, "resize must grow the host tile");

        let part = &scene
            .overlay
            .portal_surfaces
            .get(&transcript_id)
            .unwrap()
            .parts[0];
        assert!((part.bounds.x - 10.0 * r_w).abs() < 0.5, "x scaled by r_w");
        assert!((part.bounds.y - 20.0 * r_h).abs() < 0.5, "y scaled by r_h");
        assert!(
            (part.bounds.width - 100.0 * r_w).abs() < 0.5,
            "width scaled by r_w"
        );
        assert!(
            (part.bounds.height - 40.0 * r_h).abs() < 0.5,
            "height scaled by r_h"
        );
    }

    /// hud-rpmwt core: after a whole-portal resize the transcript/composer text
    /// must re-resolve to the NEW pane geometry, not stay wrapped at the
    /// attach-time width. The compositor wraps `TextMarkdownNode` text to the
    /// node's own `bounds.width` (tile-local, see
    /// `TextItem::from_text_markdown_node` / `from_text_markdown_cached`), so a
    /// resize that scales only `tile.bounds` — leaving the node tree stale —
    /// leaves the text wrapped at the old column: "resize works but the text
    /// isn't being resized". Assert both the node bounds AND the resulting
    /// `TextItem` layout width track the resized pane, at the draw-item seam.
    #[test]
    fn whole_portal_resize_accumulates_font_scale_by_width_ratio() {
        // hud-ovjxu.1: whole-portal resize records a viewer-local per-tile font
        // scale = the portal WIDTH ratio, uniform across every member, and
        // accumulates across steps. Bumps scene.version exactly once per step so
        // text re-shapes at most once per commit.
        let (mut scene, _tab, frame_id, transcript_id, composer_id, _shield, _fm) =
            multi_surface_portal_scene();
        let members = [frame_id, transcript_id, composer_id];
        let old_w = scene.tiles.get(&frame_id).unwrap().bounds.width; // 400 (fixture)

        // Default: no scaling before any resize.
        for id in members {
            assert_eq!(scene.tile_font_scale(id), 1.0);
        }

        // ── Grow to width 560 → ratio 1.4, applied to every member. ──────────
        let v0 = scene.version;
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );
        let grow_ratio = 560.0 / old_w;
        for id in members {
            assert!(
                (scene.tile_font_scale(id) - grow_ratio).abs() < 1e-3,
                "every member's font scale must equal the portal width ratio after grow"
            );
        }
        assert_eq!(
            scene.version,
            v0 + 1,
            "a resize step must bump scene.version exactly once (single re-shape)"
        );

        // ── Shrink to width 280 → step ratio 0.5, ACCUMULATES to 1.4*0.5=0.7. ─
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 280.0,
                height: 210.0,
            },
        );
        let accumulated = grow_ratio * (280.0 / 560.0);
        for id in members {
            assert!(
                (scene.tile_font_scale(id) - accumulated).abs() < 1e-3,
                "font scale must accumulate multiplicatively across resize steps \
                 (got {}, want {accumulated})",
                scene.tile_font_scale(id)
            );
        }
    }

    #[test]
    fn whole_portal_resize_reflows_transcript_text_to_new_pane_width() {
        use tze_hud_compositor::TextItem;
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };

        let (mut scene, _tab_id, frame_id, transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Attach a wrapping TextMarkdown node to the transcript pane. Node bounds
        // are tile-local: fill the transcript tile (180 x 280 from the fixture).
        let text_id = SceneId::new();
        scene.nodes.insert(
            text_id,
            Node {
                layout: Default::default(),
                id: text_id,
                children: vec![],
                data: NodeData::TextMarkdown(TextMarkdownNode {
                    content: "the quick brown fox jumps over the lazy dog again and again"
                        .to_owned(),
                    bounds: Rect::new(0.0, 0.0, 180.0, 280.0),
                    font_size_px: 14.0,
                    font_family: FontFamily::SystemSansSerif,
                    color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                    background: None,
                    alignment: TextAlign::Start,
                    overflow: TextOverflow::Clip,
                    color_runs: Box::default(),
                }),
            },
        );
        scene.tiles.get_mut(&transcript_id).unwrap().root_node = Some(text_id);

        let node_width = |scene: &tze_hud_scene::graph::SceneGraph| match &scene
            .nodes
            .get(&text_id)
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        // Wrap column the compositor would use, before the resize.
        let layout_width_before = {
            let NodeData::TextMarkdown(tm) = &scene.nodes.get(&text_id).unwrap().data else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        let node_width_before = node_width(&scene);
        let transcript_before = scene.tiles.get(&transcript_id).unwrap().bounds;

        // Grow the whole portal ~1.5x in width, ~1.3x in height.
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 600.0,
                height: 390.0,
            },
        );

        let transcript_after = scene.tiles.get(&transcript_id).unwrap().bounds;
        assert!(
            transcript_after.width > transcript_before.width,
            "precondition: the transcript tile must have grown"
        );

        // 1) The node tree re-resolved: node width scaled with its tile.
        let node_width_after = node_width(&scene);
        let tile_ratio = transcript_after.width / transcript_before.width;
        let expected_node_width = node_width_before * tile_ratio;
        assert!(
            (node_width_after - expected_node_width).abs() < 1e-2,
            "transcript text node width must scale with the pane: expected \
             ~{expected_node_width}, got {node_width_after}"
        );

        // 2) The draw-item wrap column tracks the new pane — the seam the
        //    compositor shapes/wraps against. Before the fix this stayed pinned
        //    to the attach-time width.
        let layout_width_after = {
            let NodeData::TextMarkdown(tm) = &scene.nodes.get(&text_id).unwrap().data else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        assert!(
            layout_width_after > layout_width_before + 1.0,
            "TextItem layout/wrap width must grow with the resized pane: \
             before={layout_width_before}, after={layout_width_after}"
        );

        // 3) The wrap column stays within the resized pane — no layout to a
        //    width wider than the tile (overflow contract: no partially clipped
        //    glyphs from a stale-wide layout).
        assert!(
            layout_width_after <= transcript_after.width + 1e-3,
            "wrap width {layout_width_after} must not exceed the resized pane \
             width {}",
            transcript_after.width
        );
    }

    /// hud-rpmwt live-repro core: after a whole-portal resize, an ADAPTER
    /// content republish must NOT snap the transcript back to its stale
    /// attach-time wrap width. The resize scales `tile.bounds` + the node tree
    /// and takes viewer geometry authority (hud-lyqun). But a republish replaces
    /// the whole node tree via `set_tile_root`, and the compositor wraps
    /// `TextMarkdownNode` text to `node.bounds.width` and clips to `tile.bounds`.
    /// An adapter that republishes its stale (config-width) node bounds would
    /// re-home the transcript to the old column — the fresh repro: text rendered
    /// at the stale ~1720px wrap, overflowing the resized frame UNCLIPPED. The
    /// node-bounds-authority fix reconciles the republished root to the tile's
    /// viewer-defined bounds. Assert both the node width AND the draw-item wrap
    /// width re-resolve to the resized pane (and never exceed it — the clip
    /// contract) at the same `TextItem` seam the sibling reflow test uses.
    #[test]
    fn adapter_republish_after_resize_keeps_transcript_wrapped_to_resized_pane() {
        use tze_hud_compositor::TextItem;
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            FontFamily, Node, NodeData, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow,
        };

        let (mut scene, _tab_id, frame_id, transcript_id, _composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // The adapter's attach-time node geometry: tile-local, filling the
        // transcript pane at its ORIGINAL (config) size (180 x 280 fixture).
        let attach_bounds = Rect::new(0.0, 0.0, 180.0, 280.0);
        let make_node = |bounds: Rect| {
            let id = SceneId::new();
            (
                id,
                Node {
                    layout: Default::default(),
                    id,
                    children: vec![],
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "the quick brown fox jumps over the lazy dog again and again"
                            .to_owned(),
                        bounds,
                        font_size_px: 14.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::new(1.0, 1.0, 1.0, 1.0),
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                },
            )
        };

        // Initial attach: publish the transcript node at the attach-time width.
        let (first_id, first_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, first_node).unwrap();
        let _ = first_id;

        // Whole-portal SHRINK: 400→260 wide frame. The transcript tile shrinks
        // with it, the resize scales the node tree, and the tile takes viewer
        // geometry authority (lock).
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 260.0,
                height: 260.0,
            },
        );
        let transcript_after = scene.tiles.get(&transcript_id).unwrap().bounds;
        assert!(
            transcript_after.width < 180.0,
            "precondition: the transcript pane must have shrunk below the \
             attach-time node width (got {})",
            transcript_after.width
        );
        assert!(
            scene.is_viewer_geometry_locked(transcript_id),
            "precondition: a whole-portal resize must take viewer geometry authority"
        );

        // ── ADAPTER REPUBLISH with STALE attach-time bounds ──────────────────
        // Simulate the exemplar re-publishing its content at the config width
        // (the live failure mode): a fresh node tree carrying the wide
        // attach-time bounds, which would otherwise re-home the transcript to the
        // stale column and overflow the resized pane unclipped.
        let (_second_id, second_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, second_node).unwrap();

        let node_width_after = match &scene
            .nodes
            .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };

        // 1) The republished node's width was reconciled to the resized pane, NOT
        //    left at the stale attach-time width.
        assert!(
            (node_width_after - transcript_after.width).abs() < 1e-2,
            "republished transcript node width must track the resized pane: \
             expected ~{}, got {node_width_after} (stale attach width was 180)",
            transcript_after.width
        );
        assert!(
            node_width_after < 180.0,
            "republished node must not keep the stale attach-time wrap width"
        );

        // 2) The draw-item wrap column (the seam the compositor shapes against)
        //    stays within the resized pane — no wrap-to-stale-width, and the clip
        //    can never be exceeded because the node no longer overflows the tile.
        let layout_width_after = {
            let NodeData::TextMarkdown(tm) = &scene
                .nodes
                .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
                .unwrap()
                .data
            else {
                unreachable!()
            };
            TextItem::from_text_markdown_node(tm, 0.0, 0.0).bounds_width
        };
        assert!(
            layout_width_after <= transcript_after.width + 1e-3,
            "wrap width {layout_width_after} must not exceed the resized pane \
             width {} (stale-wide layout would overflow the frame unclipped)",
            transcript_after.width
        );

        // 2b) A child node added AFTER the root within the same republish (a
        //     portal batch attaches the composer as a separate `AddNode` after
        //     `SetTileRoot`) is reconciled too — otherwise only the root scales
        //     and the child keeps stale attach-time geometry. Add a child at the
        //     stale attach width and assert it tracks the resized pane.
        let root_id_for_child = scene.tiles.get(&transcript_id).unwrap().root_node.unwrap();
        let (child_id, child_node) = make_node(attach_bounds);
        scene
            .add_node_to_tile(transcript_id, Some(root_id_for_child), child_node)
            .unwrap();
        let child_width = match &scene.nodes.get(&child_id).unwrap().data {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        assert!(
            (child_width - transcript_after.width).abs() < 1e-2,
            "a child added after the root must also be reconciled to the resized \
             pane: expected ~{}, got {child_width} (stale attach width was 180)",
            transcript_after.width
        );

        // 3) Scoping guard: once the viewer releases geometry authority, a
        //    republish is NOT reconciled — the adapter regains node-bounds
        //    control, proving the fix is gated on the lock and cannot distort
        //    ordinary (non-viewer-resized) tiles.
        scene.unlock_viewer_geometry(transcript_id);
        let (_third_id, third_node) = make_node(attach_bounds);
        scene.set_tile_root(transcript_id, third_node).unwrap();
        let node_width_unlocked = match &scene
            .nodes
            .get(&scene.tiles.get(&transcript_id).unwrap().root_node.unwrap())
            .unwrap()
            .data
        {
            NodeData::TextMarkdown(tm) => tm.bounds.width,
            other => panic!("expected TextMarkdown, got {other:?}"),
        };
        assert!(
            (node_width_unlocked - 180.0).abs() < 1e-2,
            "an unlocked tile must apply the adapter's published bounds verbatim \
             (got {node_width_unlocked}, want 180)"
        );
    }

    /// hud-lyqun core: dragging one constituent surface of a text-stream portal
    /// must translate the WHOLE portal by the same delta — every member moves
    /// together preserving relative layout, the far-corner drag shield stays
    /// parked, and every moved member takes viewer geometry authority.
    #[test]
    fn drag_move_translates_whole_portal_group_coherently() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, shield_id, _fm) =
            multi_surface_portal_scene();

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_before = read(&scene, frame_id);
        let transcript_before = read(&scene, transcript_id);
        let composer_before = read(&scene, composer_id);
        let shield_before = read(&scene, shield_id);

        let (dx, dy) = (140.0_f32, -35.0_f32);
        let translated = translate_portal_group_on_drag(&mut scene, frame_id, dx, dy);
        assert!(
            translated,
            "dragging a portal surface must engage the whole-portal translate path"
        );

        let frame_after = read(&scene, frame_id);
        let transcript_after = read(&scene, transcript_id);
        let composer_after = read(&scene, composer_id);
        let shield_after = read(&scene, shield_id);

        // Every constituent surface moved by exactly the drag delta.
        for (before, after, name) in [
            (frame_before, frame_after, "frame"),
            (transcript_before, transcript_after, "transcript"),
            (composer_before, composer_after, "composer"),
        ] {
            assert!(
                (after.x - (before.x + dx)).abs() < 1e-3
                    && (after.y - (before.y + dy)).abs() < 1e-3,
                "{name} must translate by the drag delta"
            );
            assert!(
                (after.width - before.width).abs() < 1e-3
                    && (after.height - before.height).abs() < 1e-3,
                "{name} size must not change on a move"
            );
        }

        // Relative layout preserved for every member.
        assert!(approx_tuple(
            rel_to_frame(transcript_before, frame_before),
            rel_to_frame(transcript_after, frame_after)
        ));
        assert!(approx_tuple(
            rel_to_frame(composer_before, frame_before),
            rel_to_frame(composer_after, frame_after)
        ));

        // The far-corner drag shield is not a spatial member and stays put.
        assert_eq!(
            shield_after, shield_before,
            "the far-corner drag shield must not move with a portal drag"
        );

        // Every moved member now holds viewer geometry authority.
        for id in [frame_id, transcript_id, composer_id] {
            assert!(
                scene.is_viewer_geometry_locked(id),
                "each dragged portal member must take viewer geometry authority"
            );
        }
        assert!(
            !scene.is_viewer_geometry_locked(shield_id),
            "the untouched drag shield must not be locked"
        );
    }

    /// hud-uyhpn core: a multi-frame drag-move is a POSITION-ONLY mutation. It
    /// must NOT bump `scene.version` (the sentinel the compositor's markdown /
    /// truncation caches gate on) — otherwise every pointer delta invalidates the
    /// content-shaped caches and forces a full re-hash / re-shape per frame, the
    /// low-fps / flickery drag observed live. Instead each frame advances
    /// `scene.geometry_epoch` so the present-gate still repaints the moved portal.
    ///
    /// Baseline (pre-fix) over a K-frame drag: `scene.version` advanced K times →
    /// K content-cache re-primes. Fixed: `scene.version` advances 0 times → ZERO
    /// re-primes, while position updates every frame (`geometry_epoch` +K, bounds
    /// move each frame).
    #[test]
    fn drag_move_is_position_only_no_version_bump_repaints_each_frame() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;

        // Settle to a known baseline as if content had just been committed and the
        // compositor primed its caches at this version.
        let version_at_drag_start = scene.version;
        let epoch_at_drag_start = scene.geometry_epoch;

        // Simulate a K-frame pointer drag: many small deltas, one per frame.
        const FRAMES: usize = 30;
        let (dx, dy) = (3.0_f32, -2.0_f32);
        let members = [frame_id, transcript_id, composer_id];
        let mut prev: Vec<_> = members.iter().map(|&id| read(&scene, id)).collect();

        for frame in 0..FRAMES {
            let translated = translate_portal_group_on_drag(&mut scene, frame_id, dx, dy);
            assert!(translated, "frame {frame}: portal translate must engage");

            // Content-cache sentinel must NOT move: version is frozen across the
            // whole drag, so the version-gated markdown/truncation caches skip
            // every frame — ZERO re-primes / re-shapes.
            assert_eq!(
                scene.version, version_at_drag_start,
                "frame {frame}: a position-only drag must NOT bump scene.version \
                 (would re-prime content caches every frame — hud-uyhpn)"
            );

            // The present-gate must still see the frame as dirty: geometry_epoch
            // advances exactly once per applied translate.
            assert_eq!(
                scene.geometry_epoch,
                epoch_at_drag_start + (frame as u64 + 1),
                "frame {frame}: geometry_epoch must advance once per drag frame so \
                 the moved portal repaints"
            );

            // Every member actually moved by the delta this frame.
            for (i, &id) in members.iter().enumerate() {
                let now = read(&scene, id);
                assert!(
                    (now.x - (prev[i].x + dx)).abs() < 1e-3
                        && (now.y - (prev[i].y + dy)).abs() < 1e-3,
                    "frame {frame}: member {i} must translate by the per-frame delta"
                );
                // Size never changes on a move — so truncation keys (size-based)
                // stay identical and the cache stays valid the whole drag.
                assert!(
                    (now.width - prev[i].width).abs() < 1e-3
                        && (now.height - prev[i].height).abs() < 1e-3,
                    "frame {frame}: member {i} size must not change on a move"
                );
                prev[i] = now;
            }
        }

        // Over the whole drag: version never moved (0 re-primes), geometry_epoch
        // moved once per frame (FRAMES repaints).
        assert_eq!(
            scene.version, version_at_drag_start,
            "scene.version must be unchanged across the entire drag"
        );
        assert_eq!(
            scene.geometry_epoch,
            epoch_at_drag_start + FRAMES as u64,
            "geometry_epoch must have advanced once per drag frame"
        );
    }

    /// hud-uyhpn: the single-tile (non-portal) move fallback is also position-only
    /// — it must advance `geometry_epoch`, not `scene.version`.
    #[test]
    fn single_tile_move_fallback_is_position_only() {
        use tze_hud_scene::{Rect, SceneGraph};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(10.0, 10.0, 100.0, 80.0),
                1,
            )
            .unwrap();

        let version_before = scene.version;
        let epoch_before = scene.geometry_epoch;

        // A plain tile resolves to the single-tile fallback (translate returns
        // false); the caller then applies bounds + geometry epoch. Reproduce that
        // position-only contract exactly.
        assert!(
            !translate_portal_group_on_drag(&mut scene, tile_id, 20.0, 15.0),
            "a plain tile must fall through to the single-tile move path"
        );
        if scene.tiles.contains_key(&tile_id) {
            if let Some(tile) = scene.tiles.get_mut(&tile_id) {
                tile.bounds.x += 20.0;
                tile.bounds.y += 15.0;
            }
            scene.bump_geometry_epoch();
        }

        assert_eq!(
            scene.version, version_before,
            "single-tile move must not bump scene.version (position-only)"
        );
        assert_eq!(
            scene.geometry_epoch,
            epoch_before + 1,
            "single-tile move must advance geometry_epoch so it repaints"
        );
    }

    /// hud-643dv: the runtime header-band drag handle targets the portal FRAME
    /// (the largest-area lease member), and a drag originating from that anchor —
    /// exactly what the band produces — moves the whole group position-only
    /// (leverages the #991 geometry-epoch path: no `scene.version` bump).
    #[test]
    fn header_band_anchor_is_the_frame_and_band_drag_is_position_only() {
        fn release_and_wait<T>(app: &WinitApp, held: T) {
            let generation = app.state.wake.main_work_generation();
            drop(held);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while app.state.wake.main_work_generation() == generation {
                assert!(
                    std::time::Instant::now() < deadline,
                    "release availability must precede settlement"
                );
                std::thread::yield_now();
            }
        }

        let (mut scene, _tab, frame_id, transcript_id, composer_id, _shield, _fm) =
            multi_surface_portal_scene();

        // The band handle the compositor emits targets the frame/anchor tile.
        let anchors = scene.portal_header_band_anchors(52.0);
        assert!(
            anchors.iter().any(|(a, _)| *a == frame_id),
            "the header-band drag handle must target the portal frame anchor"
        );
        assert!(
            !anchors
                .iter()
                .any(|(a, _)| *a == transcript_id || *a == composer_id),
            "panes must NOT get their own header band"
        );

        // Dragging from the anchor (as a band drag does) moves the whole group
        // position-only: no scene.version bump, geometry_epoch advances once.
        let read = |s: &tze_hud_scene::graph::SceneGraph, id| s.tiles.get(&id).unwrap().bounds;
        let (fb, tb, cb) = (
            read(&scene, frame_id),
            read(&scene, transcript_id),
            read(&scene, composer_id),
        );
        let version_before = scene.version;
        let epoch_before = scene.geometry_epoch;
        let (dx, dy) = (40.0_f32, -25.0_f32);
        assert!(translate_portal_group_on_drag(&mut scene, frame_id, dx, dy));

        assert_eq!(
            scene.version, version_before,
            "a band drag must be position-only — no content-cache re-prime (hud-uyhpn)"
        );
        assert_eq!(scene.geometry_epoch, epoch_before + 1);
        for (before, id) in [(fb, frame_id), (tb, transcript_id), (cb, composer_id)] {
            let after = read(&scene, id);
            assert!(
                (after.x - (before.x + dx)).abs() < 1e-3
                    && (after.y - (before.y + dy)).abs() < 1e-3,
                "every member must translate by the band-drag delta"
            );
        }

        // The visible frame, not an equal-bounds passthrough backstop, owns
        // the real runtime gesture and its one whole-portal highlight.
        let frame = scene.tiles.get(&frame_id).unwrap().clone();
        let backstop = scene
            .create_tile(
                frame.tab_id,
                "portal-agent",
                frame.lease_id,
                frame.bounds,
                0,
            )
            .unwrap();
        scene.tiles.get_mut(&backstop).unwrap().input_mode = tze_hud_scene::InputMode::Passthrough;
        let (_, band) = scene
            .portal_header_band_anchors(52.0)
            .into_iter()
            .find(|(id, _)| *id == frame_id)
            .unwrap();
        let interaction_id = format!("drag-handle:{frame_id}");
        scene
            .overlay
            .drag_handle_hit_regions
            .push(tze_hud_scene::DragHandleHitRegion {
                element_id: frame_id,
                element_kind: tze_hud_scene::DragHandleElementKind::Tile,
                bounds: band,
                interaction_id: interaction_id.clone(),
                hit_region: tze_hud_scene::HitRegionNode {
                    bounds: band,
                    interaction_id,
                    accepts_pointer: true,
                    ..Default::default()
                },
                tab_order: 0,
                is_header_band: true,
            });
        assert_eq!(
            drag_feedback_target(&scene, transcript_id),
            frame_id,
            "a contained surface uses the visible frame"
        );
        assert_eq!(
            drag_feedback_target(&scene, _shield),
            _shield,
            "an outside seed is not a portal feedback member"
        );
        let outside_grip = tze_hud_scene::Rect::new(1880.0, 1060.0, 40.0, 20.0);
        let mut outside_handle = scene.overlay.drag_handle_hit_regions[0].clone();
        outside_handle.element_id = _shield;
        outside_handle.bounds = outside_grip;
        outside_handle.hit_region.bounds = outside_grip;
        outside_handle.interaction_id = format!("drag-handle:{_shield}");
        outside_handle.hit_region.interaction_id = outside_handle.interaction_id.clone();
        outside_handle.is_header_band = false;
        scene.overlay.drag_handle_hit_regions.push(outside_handle);
        let (mut app, _rx) =
            make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
        seed_tile_entries(
            &app,
            &[frame_id, transcript_id, composer_id, backstop, _shield],
            "portal-agent",
        );
        app.state.cursor_x = 1900.0;
        app.state.cursor_y = 1070.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        app.state
            .input_processor
            .drag_states
            .get_mut(&0)
            .unwrap()
            .press_start -= std::time::Duration::from_millis(250);
        // This private identity follows the same press seed in this fixture.
        app.state.primary_drag_gesture.as_mut().unwrap().press_start =
            app.state.input_processor.drag_states[&0].press_start;
        app.state.cursor_x += 1.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(scene.is_drag_active(_shield));
            assert!(
                !scene.is_drag_active(frame_id),
                "the real outside gesture cannot mark the portal frame"
            );
            let gesture = app.state.primary_drag_gesture.as_ref().unwrap();
            assert_eq!(gesture.feedback_target, _shield);
        }
        app.state.cursor_x = band.x + 50.0;
        app.state.cursor_y = band.y + 12.0;
        app.enqueue_pointer_event(PointerEventKind::Down);
        assert!(
            app.state.primary_drag_gesture.is_some(),
            "the actual Down seeds the gesture"
        );
        app.state.cursor_x += 5.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        {
            let shared = app.state.shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(scene.is_drag_active(frame_id));
            assert_eq!(scene.overlay.drag_active_elements.len(), 1);
            assert!(!scene.is_drag_active(backstop));
        }
        app.state.cursor_x += 35.0;
        app.state.cursor_y += 20.0;
        app.enqueue_pointer_event(PointerEventKind::Move);

        // Exhaust the real SharedState acquisition budget on Up. The actual
        // event is retained once and a newer Down cannot steal its identity.
        let shared_state = Arc::clone(&app.state.shared_state);
        let held = shared_state.try_lock().unwrap();
        let original_press = app.state.primary_drag_gesture.as_ref().unwrap().press_start;
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(
            app.state
                .primary_drag_gesture
                .as_ref()
                .unwrap()
                .terminal
                .is_some()
        );
        let ring_after_up = app.state.input_ring.lock().unwrap().len();
        app.enqueue_pointer_event(PointerEventKind::Down);
        assert_eq!(
            app.state.primary_drag_gesture.as_ref().unwrap().press_start,
            original_press
        );
        assert_eq!(app.state.input_ring.lock().unwrap().len(), ring_after_up);
        // The real availability waiter briefly reacquires both scene locks.
        // Settle only after its completion, as the event loop does on its wake.
        release_and_wait(&app, held);
        let checkpoint = app.state.wake.main_work_checkpoint();
        let settled = app.settle_scene_work();
        assert!(
            settled.scene_changed(),
            "release must admit the cleared-border frame"
        );
        assert!(
            app.state
                .wake
                .finish_main_work_after_settle(checkpoint, settled.scene_changed())
        );
        assert!(app.state.primary_drag_gesture.is_none());
        assert!(!app.state.input_processor.drag_states.contains_key(&0));
        assert_eq!(
            app.state.input_ring.lock().unwrap().len(),
            ring_after_up,
            "retry emits no second input"
        );
        for id in [frame_id, transcript_id, composer_id, backstop] {
            assert!(
                has_override(&app, id),
                "the original release persists every member"
            );
        }
        assert!(
            !has_override(&app, _shield),
            "the distant shield is not a portal member"
        );

        // Exercise the independent scene-lock failure, then admit a new Down
        // only after the retained Up has ended the original gesture.
        {
            let shared = shared_state.try_lock().unwrap();
            let mut scene = shared.scene.try_lock().unwrap();
            assert!(!scene.is_drag_active(frame_id));
            let (_, band) = scene
                .portal_header_band_anchors(52.0)
                .into_iter()
                .find(|(id, _)| *id == frame_id)
                .unwrap();
            scene.overlay.drag_handle_hit_regions[0].bounds = band;
            scene.overlay.drag_handle_hit_regions[0].hit_region.bounds = band;
            app.state.cursor_x = band.x + 50.0;
            app.state.cursor_y = band.y + 12.0;
        }
        app.enqueue_pointer_event(PointerEventKind::Down);
        app.state.cursor_x += 5.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        let original_press = app.state.primary_drag_gesture.as_ref().unwrap().press_start;
        let scene_arc = Arc::clone(&shared_state.try_lock().unwrap().scene);
        let held = scene_arc.try_lock().unwrap();
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(
            app.state
                .primary_drag_gesture
                .as_ref()
                .unwrap()
                .terminal
                .is_some()
        );
        release_and_wait(&app, held);
        app.enqueue_pointer_event(PointerEventKind::Down);
        let replacement = app.state.primary_drag_gesture.as_ref().unwrap();
        assert_ne!(replacement.press_start, original_press);
        assert!(replacement.terminal.is_none());
        assert_eq!(
            app.state.input_processor.drag_states[&0].phase,
            tze_hud_input::DragPhase::Accumulating
        );
        app.enqueue_pointer_event(PointerEventKind::Up);
        assert!(app.state.primary_drag_gesture.is_none());

        // Headless identity checks prove rejection of an absent original window
        // allocation and a changed lease, without claiming native capture coverage.
        for invalid_window in [true, false] {
            app.enqueue_pointer_event(PointerEventKind::Down);
            app.state.cursor_x += 5.0;
            app.enqueue_pointer_event(PointerEventKind::Move);
            let held = shared_state.try_lock().unwrap();
            app.enqueue_pointer_event(PointerEventKind::Up);
            let saved_override = held.element_store.entries[&frame_id].geometry_override;
            if invalid_window {
                let gesture = app.state.primary_drag_gesture.as_mut().unwrap();
                gesture.window_id = Some(winit::window::WindowId::dummy());
                gesture.window_allocation = Some(std::sync::Weak::new());
            } else {
                let mut scene = held.scene.try_lock().unwrap();
                let different_lease = scene.grant_lease("portal-agent", 60_000);
                scene.tiles.get_mut(&frame_id).unwrap().lease_id = different_lease;
            }
            release_and_wait(&app, held);
            app.settle_scene_work();
            assert!(app.state.primary_drag_gesture.is_none());
            assert!(!app.state.input_processor.drag_states.contains_key(&0));
            let held = shared_state.try_lock().unwrap();
            let mut scene = held.scene.try_lock().unwrap();
            assert!(!scene.is_drag_active(frame_id));
            assert_eq!(
                held.element_store.entries[&frame_id].geometry_override,
                saved_override
            );
            scene.tiles.get_mut(&frame_id).unwrap().lease_id = frame.lease_id;
        }

        // A removed target cancels the retained terminal instead of applying
        // its stale final coordinates or persisting a phantom tile.
        app.enqueue_pointer_event(PointerEventKind::Down);
        app.state.cursor_x += 5.0;
        app.enqueue_pointer_event(PointerEventKind::Move);
        let held = shared_state.try_lock().unwrap();
        app.enqueue_pointer_event(PointerEventKind::Up);
        let saved_override = held.element_store.entries[&frame_id].geometry_override;
        held.scene
            .try_lock()
            .unwrap()
            .delete_tile(frame_id, "portal-agent")
            .unwrap();
        release_and_wait(&app, held);
        app.settle_scene_work();
        assert!(app.state.primary_drag_gesture.is_none());
        assert!(!app.state.input_processor.drag_states.contains_key(&0));
        let shared = shared_state.try_lock().unwrap();
        let scene = shared.scene.try_lock().unwrap();
        assert!(!scene.tiles.contains_key(&frame_id));
        assert!(!scene.is_drag_active(frame_id));
        assert_eq!(
            shared.element_store.entries[&frame_id].geometry_override,
            saved_override
        );
        drop(scene);
        drop(shared);
        // Zone identity is the slot, even across clear or publication replacement.
        // This CPU fixture models its emitted handle; the render parent proves
        // admission using the actual compositor-produced handle.
        for terminal in [
            "clear",
            "publication",
            "removed",
            "definition",
            "window",
            "hidden",
        ] {
            let (mut scene, ..) =
                super::super::test_support::scene_with_drag_handle_tile(400.0, 300.0, 600.0, 200.0);
            let zone = super::super::test_support::make_test_zone("slot");
            let id = zone.id;
            scene.register_zone(zone.clone());
            let content = tze_hud_scene::ZoneContent::Notification(Default::default());
            scene
                .publish_to_zone("slot", content.clone(), "owner", None, None, None)
                .unwrap();
            let handle = &mut scene.overlay.drag_handle_hit_regions[0];
            handle.element_id = id;
            handle.element_kind = DragHandleElementKind::Zone;
            handle.interaction_id = format!("drag-handle:{id}");
            handle.hit_region.interaction_id = handle.interaction_id.clone();
            let (x, y) = (
                handle.bounds.x + handle.bounds.width / 2.0,
                handle.bounds.y + handle.bounds.height / 2.0,
            );
            let (mut app, _rx) =
                make_windowed_keyboard_test_app(scene, FocusManager::new(), InputProcessor::new());
            let (tx, mut events) = tokio::sync::broadcast::channel(4);
            app.state.element_repositioned_tx = Some(tx);
            app.state.cursor_x = x;
            app.state.cursor_y = y;
            app.enqueue_pointer_event(PointerEventKind::Down);
            app.state.cursor_y += 1.0;
            app.enqueue_pointer_event(PointerEventKind::Move);
            app.state.cursor_y += 43.2;
            app.enqueue_pointer_event(PointerEventKind::Move);
            let shared_state = Arc::clone(&app.state.shared_state);
            let held = shared_state.try_lock().unwrap();
            app.enqueue_pointer_event(PointerEventKind::Up);
            assert!(
                app.state
                    .primary_drag_gesture
                    .as_ref()
                    .unwrap()
                    .terminal
                    .is_some()
            );
            let ring = app.state.input_ring.lock().unwrap().len();
            app.enqueue_pointer_event(PointerEventKind::Down);
            assert_eq!(app.state.input_ring.lock().unwrap().len(), ring);
            {
                let mut scene = held.scene.try_lock().unwrap();
                match terminal {
                    "clear" => scene.clear_zone("slot").unwrap(),
                    "publication" => {
                        scene.clear_zone("slot").unwrap();
                        scene
                            .publish_to_zone("slot", content, "other", None, None, None)
                            .unwrap();
                    }
                    "removed" => {
                        scene.unregister_zone("slot");
                    }
                    "definition" => {
                        let mut replacement = zone;
                        replacement.id = SceneId::new();
                        scene.register_zone(replacement);
                    }
                    "hidden" => {
                        let mut hidden = scene.owner_hidden();
                        hidden.zones = true;
                        scene.set_owner_hidden(hidden);
                    }
                    "window" => {
                        let gesture = app.state.primary_drag_gesture.as_mut().unwrap();
                        gesture.window_id = Some(winit::window::WindowId::dummy());
                        gesture.window_allocation = Some(std::sync::Weak::new());
                    }
                    _ => unreachable!(),
                }
            }
            release_and_wait(&app, held);
            app.settle_scene_work();
            assert!(app.state.primary_drag_gesture.is_none());
            assert!(app.state.input_processor.drag_states.is_empty());
            assert_eq!(app.state.input_ring.lock().unwrap().len(), ring);
            let shared = shared_state.try_lock().unwrap();
            let scene = shared.scene.try_lock().unwrap();
            assert!(!scene.is_drag_active(id));
            assert!(shared.element_store.entries.is_empty());
            assert!(events.try_recv().is_err(), "zone completion is owner-local");
            assert_eq!(
                scene.overlay.zone_placements.contains_key(&id),
                !matches!(terminal, "removed" | "definition")
            );
            if terminal == "clear" {
                assert!(scene.zone_registry.active_for_zone("slot").is_empty());
            }
        }
    }

    /// A single non-portal tile drag must NOT engage the whole-portal translate
    /// path (no scrollable constituent), so behavior is unchanged for plain tiles.
    #[test]
    fn drag_move_single_non_portal_tile_is_not_group_translated() {
        use tze_hud_scene::{Rect, SceneGraph};
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "agent",
                lease_id,
                Rect::new(10.0, 10.0, 100.0, 80.0),
                1,
            )
            .unwrap();

        let translated = translate_portal_group_on_drag(&mut scene, tile_id, 20.0, 20.0);
        assert!(
            !translated,
            "a plain non-portal tile must be left to the single-tile move path"
        );
        assert!(
            !scene.is_viewer_geometry_locked(tile_id),
            "a non-portal single-tile drag must not take geometry authority"
        );
    }

    /// hud-lyqun regression: resize the whole portal, THEN drag it — the group
    /// must stay coherent (relative layout preserved) rather than fracturing.
    #[test]
    fn resize_then_drag_keeps_portal_group_coherent() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        // Grow the portal (top-left anchored) to a larger rect.
        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_mid = read(&scene, frame_id);
        let transcript_mid = read(&scene, transcript_id);
        let composer_mid = read(&scene, composer_id);

        // Now drag the resized portal.
        let (dx, dy) = (-60.0_f32, 90.0_f32);
        assert!(translate_portal_group_on_drag(&mut scene, frame_id, dx, dy));

        let frame_after = read(&scene, frame_id);
        let transcript_after = read(&scene, transcript_id);
        let composer_after = read(&scene, composer_id);

        // Relative layout is preserved through resize AND the subsequent drag.
        assert!(
            approx_tuple(
                rel_to_frame(transcript_mid, frame_mid),
                rel_to_frame(transcript_after, frame_after)
            ),
            "transcript must keep relative layout after resize+drag"
        );
        assert!(
            approx_tuple(
                rel_to_frame(composer_mid, frame_mid),
                rel_to_frame(composer_after, frame_after)
            ),
            "composer must keep relative layout after resize+drag"
        );
        // Frame translated by the drag delta.
        assert!(
            (frame_after.x - (frame_mid.x + dx)).abs() < 1e-3
                && (frame_after.y - (frame_mid.y + dy)).abs() < 1e-3,
            "frame must translate by the drag delta after a resize"
        );
    }

    /// hud-lyqun proof: after a whole-portal resize, an adapter republishing its
    /// stale client-side member layout (via `update_tile_bounds`) CANNOT move any
    /// member — the group cannot be fractured.
    #[test]
    fn adapter_republish_cannot_fracture_resized_portal_group() {
        let (mut scene, _tab_id, frame_id, transcript_id, composer_id, _shield_id, _fm) =
            multi_surface_portal_scene();

        resize_group_to(
            &mut scene,
            frame_id,
            tze_hud_input::PortalRect {
                x: 100.0,
                y: 100.0,
                width: 560.0,
                height: 420.0,
            },
        );

        let read =
            |scene: &tze_hud_scene::graph::SceneGraph, id| scene.tiles.get(&id).unwrap().bounds;
        let frame_scaled = read(&scene, frame_id);
        let transcript_scaled = read(&scene, transcript_id);
        let composer_scaled = read(&scene, composer_id);

        // The adapter re-emits its OLD pre-resize client-side layout for a subset
        // of members (exactly the live-observed fracture: some members stomped to
        // stale bounds while others keep runtime-scaled bounds).
        let _ = scene.update_tile_bounds(
            transcript_id,
            tze_hud_scene::Rect::new(110.0, 110.0, 180.0, 280.0),
            "portal-agent",
        );
        let _ = scene.update_tile_bounds(
            composer_id,
            tze_hud_scene::Rect::new(300.0, 110.0, 190.0, 280.0),
            "portal-agent",
        );

        // Nothing moved: the runtime-owned scaled geometry held for every member.
        assert_eq!(
            read(&scene, transcript_id),
            transcript_scaled,
            "adapter republish must not stomp the transcript pane after a resize"
        );
        assert_eq!(
            read(&scene, composer_id),
            composer_scaled,
            "adapter republish must not stomp the composer pane after a resize"
        );
        assert_eq!(
            read(&scene, frame_id),
            frame_scaled,
            "the frame must keep its resized geometry"
        );

        // The group is still internally coherent (relative layout intact).
        assert!(approx_tuple(
            rel_to_frame(transcript_scaled, frame_scaled),
            rel_to_frame(read(&scene, transcript_id), read(&scene, frame_id))
        ));
    }

    // ── Composer pointer caret hit-test + drag-select (hud-etrs0) ───────────

    /// Includes the full-display-vs-origin regression and both clamp directions.
    #[test]
    fn compute_portal_max_dims_cases() {
        for (case, [lease_w, lease_h, display_w, display_h, min_w, min_h], expected) in [
            (
                "compute_portal_max_dims_unconstrained_uses_display_boundary",
                [0.0, 0.0, 1920.0, 1080.0, 50.0, 50.0],
                (1920.0, 1080.0),
            ),
            (
                "compute_portal_max_dims_uses_display_w_not_display_w_minus_tile_x",
                [0.0, 0.0, 1000.0, 800.0, 50.0, 50.0],
                (1000.0, 800.0),
            ),
            (
                "compute_portal_max_dims_lease_budget_is_more_restrictive",
                [500.0, 400.0, 1920.0, 1080.0, 50.0, 50.0],
                (500.0, 400.0),
            ),
            (
                "compute_portal_max_dims_display_boundary_is_more_restrictive",
                [5000.0, 5000.0, 1920.0, 1080.0, 50.0, 50.0],
                (1920.0, 1080.0),
            ),
            (
                "compute_portal_max_dims_lease_budget_floored_to_token_minimum",
                [10.0, 10.0, 1920.0, 1080.0, 50.0, 50.0],
                (50.0, 50.0),
            ),
        ] {
            assert_eq!(
                compute_portal_max_dims(lease_w, lease_h, display_w, display_h, min_w, min_h),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn portal_resize_state_pruning_preserves_live_and_cleans_removed_tiles() {
        for eager in [false, true] {
            let (mut scene, _tab, live, fm) = portal_scene_with_focus();
            let tile = scene.tiles[&live].clone();
            let stale = scene
                .create_tile(
                    tile.tab_id,
                    "portal-agent",
                    tile.lease_id,
                    tze_hud_scene::Rect::new(600.0, 100.0, 300.0, 200.0),
                    2,
                )
                .unwrap();
            let (mut app, _rx) = make_windowed_keyboard_test_app(scene, fm, InputProcessor::new());
            for id in [live, stale] {
                app.state
                    .portal_resize_states
                    .insert(id, PortalResizeState::new(id.as_uuid().as_u128() as u64));
            }
            let shared = Arc::clone(&app.state.shared_state);
            let scene_handle = {
                let state = shared.try_lock().unwrap();
                let mut scene = state.scene.try_lock().unwrap();
                scene.delete_tile(stale, "portal-agent").unwrap();
                if !eager {
                    assert_eq!(scene.drain_removed_tile_ids(), vec![stale]);
                }
                Arc::clone(&state.scene)
            };
            // Both busy owners defer without draining removal or changing maps.
            let state_guard = shared.try_lock().unwrap();
            assert!(!app.prune_portal_resize_states());
            assert_eq!(app.state.portal_resize_states.len(), 2);
            drop(state_guard);
            let scene_guard = scene_handle.try_lock().unwrap();
            assert!(!app.prune_portal_resize_states());
            assert_eq!(app.state.portal_resize_states.len(), 2);
            drop(scene_guard);
            app.prune_portal_resize_states();
            for id in [live, stale] {
                assert_eq!(app.state.portal_resize_states.contains_key(&id), id == live);
            }
            assert!(
                scene_handle
                    .try_lock()
                    .unwrap()
                    .drain_removed_tile_ids()
                    .is_empty()
            );
            assert!(
                !app.prune_portal_resize_states(),
                "second prune is harmless"
            );
            assert_eq!(app.state.portal_resize_states.len(), 1);
            assert!(
                scene_handle
                    .try_lock()
                    .unwrap()
                    .drain_removed_tile_ids()
                    .is_empty()
            );
        }
    }
}
