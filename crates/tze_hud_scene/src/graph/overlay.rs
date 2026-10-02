use crate::types::*;
use crate::validation::ValidationError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Transient per-frame state owned by the runtime and compositor layers.
///
/// These fields are all `#[serde(skip)]` — they are never serialized and carry
/// no durable scene-model semantics.  They are grouped here so that
/// [`SceneGraph`]'s public surface is not polluted with render-scratch details.
///
/// Owned by the [`SceneGraph`] as `pub overlay: RuntimeOverlayState`.  The
/// compositor populates hit regions each frame; the input processor reads and
/// clears them; the runtime drains pending SVG assets before the render loop.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeOverlayState {
    /// Runtime-managed zone interaction hit regions.
    ///
    /// Populated by the compositor each frame during `render_zone_content`.
    /// Contains the display-space bounds of dismiss (×) buttons and action
    /// buttons for visible notification slots.
    ///
    /// The hit-test pipeline checks this list after tile testing when the
    /// result would otherwise be `Passthrough`, producing a
    /// [`HitResult::ZoneInteraction`] result for zone-owned affordances.
    ///
    /// Ephemeral: skipped during serialization.  Cleared by the compositor
    /// at the start of each frame before zone geometry is recomputed.
    #[serde(skip, default)]
    pub zone_hit_regions: Vec<ZoneHitRegion>,
    /// Runtime-managed chrome drag-handle hit regions.
    ///
    /// Populated by the compositor each frame from currently visible tiles,
    /// active zones, and active widgets. These are runtime-internal affordances
    /// and are never serialized or exposed over agent-facing scene mutations.
    #[serde(skip, default)]
    pub drag_handle_hit_regions: Vec<DragHandleHitRegion>,
    /// Local-first hover/press state keyed by drag-handle interaction id.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub drag_handle_states: HashMap<String, DragHandleLocalState>,
    /// Set of element_ids that are currently being actively dragged.
    ///
    /// Written by the input processor on DragActivated and cleared on
    /// DragReleased / DragCancelled. Read by the compositor to apply v1-
    /// compatible visual feedback (z-order boost, opacity, 2px highlight border).
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub drag_active_elements: HashSet<SceneId>,
    /// Chrome-layer context menu shown on right-click / short-tap of a drag handle.
    ///
    /// `Some` while the menu is visible; `None` otherwise.  The compositor
    /// renders this overlay and populates `reset_button_rect` for hit-testing.
    /// Auto-dismissed by the windowed runtime after 3 s, or on click-outside.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub drag_handle_context_menu: Option<crate::types::DragHandleContextMenuState>,
    /// Runtime-registered widget SVG assets awaiting compositor registration.
    ///
    /// Producers (session/MCP runtime registration paths) enqueue validated SVGs
    /// here. Consumers (windowed/headless render loops) drain and register them
    /// with the compositor-side widget renderer before rendering.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub pending_widget_svg_assets: Vec<(String, String, Vec<u8>)>,
    /// Runtime-owned scroll configs for tiles that opt into local-first scroll.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_scroll_configs: HashMap<SceneId, TileScrollConfig>,
    /// Runtime-owned local scroll offsets per tile.
    ///
    /// Offsets are absolute from the tile content origin and are applied by
    /// the compositor and hit-testing paths for immediate local feedback.
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_scroll_offsets: HashMap<SceneId, (f32, f32)>,
    /// Displayed (smoothed/lagged) scroll offsets per tile, published by the
    /// compositor each frame while windowed scroll smoothing is active.
    ///
    /// During an in-flight smoothed scroll the renderer draws tile content at
    /// the *displayed* offset from its `ScrollSmoother`, which lags the
    /// authoritative target in [`tile_scroll_offsets`](Self::tile_scroll_offsets).
    /// The hit-test path consults this map (via
    /// [`SceneGraph::effective_tile_scroll_offset_local`]) so pointer queries
    /// map against the same rows the operator actually sees mid-animation
    /// (hud-3lynp).
    ///
    /// Empty in headless/snap mode (smoothing disabled): hit-testing then falls
    /// back to the authoritative offset, leaving deterministic tests unchanged.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub displayed_tile_scroll_offsets: HashMap<SceneId, (f32, f32)>,
    /// Follow-tail anchor state per tile (used by truncation-mode selection).
    ///
    /// `true` = tile viewport is at the tail of its content stream (use
    /// tail-anchored truncation); `false` = scrolled-back (use head-anchored
    /// truncation, spec task 3.3 append-stability guarantee).
    ///
    /// Defaults to `true` for newly registered tiles (new tiles start at tail).
    /// Set by the runtime layer when `FollowTailAnchor` transitions; read by
    /// the compositor's truncation-mode selection in `prime_truncation_cache`.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_follow_tail_at_tail: HashMap<SceneId, bool>,
    /// Runtime-owned ambient unread-output count per tile (hud-g1ena.3).
    ///
    /// The aggregate `ProjectedPortalState::unread_output_count` a text-stream
    /// portal already tracks, plumbed onto the transcript tile by the portal
    /// projection driver each drain. Read by the compositor to render the
    /// ambient unread badge the jump-to-latest pill MAY carry
    /// (portal-chat-grade-affordances §Jump-to-Latest Affordance) — the pill is
    /// only shown while `tile_follow_tail_at_tail == false`, so the badge clears
    /// with it when the viewer returns to the tail.
    ///
    /// `0` (or an absent entry) means nothing unread; the driver stores `0` when
    /// the count is redacted or empty so no badge renders. Ephemeral: skipped
    /// during serialization.
    #[serde(skip, default)]
    pub tile_unread_counts: HashMap<SceneId, usize>,
    /// Runtime-owned lifecycle-affordance accents per tile.
    ///
    /// Set by the [`SceneMutation::SetTileLifecycleAccent`] apply path (driven by
    /// the coalescible StateStream `SetTileLifecycleAccent` wire mutation) and
    /// read by the compositor, which paints a token-colored left-edge bar.
    ///
    /// Lives here, keyed by tile id, rather than as a scene node so it survives
    /// `SetTileRoot`/`PublishToTile` content republishes (which replace the whole
    /// node tree). This is what keeps lifecycle-visible portals coalescible —
    /// the accent is never re-added as a per-republish `AddNode` (hud-m48i0).
    ///
    /// [`SceneMutation::SetTileLifecycleAccent`]: crate::mutation::SceneMutation::SetTileLifecycleAccent
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_lifecycle_accents: HashMap<SceneId, LifecycleAccent>,
    /// Runtime-owned composer interaction hit-region specs per tile (hud-iofav).
    ///
    /// Set by the [`SceneMutation::SetTileComposerInteraction`] apply path (driven
    /// by the coalescible StateStream `SetTileComposerInteraction` wire mutation).
    /// The scene derives a [`crate::types::HitRegionNode`] child of the tile root
    /// from this spec and RE-ATTACHES it after every `SetTileRoot`/`PublishToTile`
    /// content republish — which replaces the whole node tree and would otherwise
    /// wipe a child hit-region node.
    ///
    /// Lives here, keyed by tile id, rather than surviving only as a scene node so
    /// that an interaction-enabled portal's per-render transcript republish never
    /// has to re-send the composer as a per-republish `AddNode` (which flips the
    /// batch Transactional and defeats StateStream coalescing on the hottest path —
    /// a streaming transcript with an interactive composer, hud-mzk74 / hud-iofav).
    /// The derived hit region stays a real scene node so `hit_test`, focus, and the
    /// `ComposerDraftManager` operate unchanged — it is synthesized server-side as a
    /// derived consequence of this overlay, not as an inbound `AddNode`.
    ///
    /// [`SceneMutation::SetTileComposerInteraction`]: crate::mutation::SceneMutation::SetTileComposerInteraction
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_composer_interactions: HashMap<SceneId, HitRegionNode>,
    /// The scene node id currently synthesized for each tile's composer interaction
    /// (hud-iofav). Maps host tile id → the derived hit-region node's id.
    ///
    /// Written by the composer-interaction reattach path
    /// ([`SceneGraph::ensure_tile_composer_node`]) so a stale derived node can be
    /// detached before a fresh one is attached (on spec change or root replacement).
    /// A mapped node that no longer exists in `nodes` (e.g. removed with the old
    /// root subtree on republish) is treated as absent and rebuilt.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_composer_nodes: HashMap<SceneId, SceneId>,
    /// First-class text-stream portal surface descriptors, keyed by host tile id
    /// (hud-tc153).
    ///
    /// Declared by the Transactional [`SceneMutation::SetPortalSurface`] apply
    /// path and patched by the coalescible StateStream
    /// [`SceneMutation::UpdatePortalSurfaceState`] path. Lives here, keyed by tile
    /// id, rather than as a scene node so it survives `SetTileRoot`/`PublishToTile`
    /// transcript republishes that replace the tile's node tree — the same
    /// coalescing rationale as [`tile_lifecycle_accents`](Self::tile_lifecycle_accents).
    ///
    /// The descriptor is adapter-driven metadata (like the lifecycle accent). It is
    /// `#[serde(skip)]` here, but is included in the full scene snapshot as
    /// [`SceneGraphSnapshot::portal_surfaces`](crate::types::SceneGraphSnapshot::portal_surfaces)
    /// so a reconnecting session recovers its declared surfaces from the snapshot
    /// rather than re-declaring blindly (hud-ruynm reconnect parity).
    ///
    /// [`SceneMutation::SetPortalSurface`]: crate::mutation::SceneMutation::SetPortalSurface
    /// [`SceneMutation::UpdatePortalSurfaceState`]: crate::mutation::SceneMutation::UpdatePortalSurfaceState
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub portal_surfaces: HashMap<SceneId, PortalSurface>,
    /// Tile IDs removed since the last runtime drain.
    ///
    /// Populated by [`SceneGraph::remove_tile_and_nodes`] on every tile
    /// deletion; drained by the windowed runtime in `about_to_wait` via
    /// [`SceneGraph::drain_removed_tile_ids`].  Allows the runtime to eagerly
    /// prune per-tile state that cannot live inside the scene graph due to
    /// crate-layer constraints (e.g. `portal_resize_states` in `windowed.rs`,
    /// which holds a `tze_hud_input` type and would create a circular
    /// dependency if moved here).
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub recently_removed_tile_ids: Vec<SceneId>,
    /// Accepted MutationBatch ids applied since the last present drain (hud-91uu6).
    ///
    /// Populated by [`SceneGraph::apply_batch`] on every accepted (applied) batch,
    /// in application order. Drained by the render loop at frame present via
    /// [`SceneGraph::drain_present_ack_batch_ids`], which stamps them with the
    /// presented frame number + present wall-clock and emits a `FramePresented`
    /// event to TELEMETRY_FRAMES subscribers. This is the batch → on-screen-present
    /// correlation primitive: a batch whose commit changed the scene is reflected
    /// by the next composited frame, so recording batch_ids here and draining at
    /// present pairs each batch with the frame that carried it.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub present_ack_pending_batch_ids: Vec<SceneId>,
    /// Tile IDs whose geometry the viewer has taken control of via a whole-portal
    /// move or resize gesture (hud-lyqun).
    ///
    /// While a tile is in this set the runtime holds geometry authority over it:
    /// the owning adapter's [`SceneMutation::UpdateTileBounds`] republishes become
    /// a bounds no-op (see [`SceneGraph::update_tile_bounds`]), so an adapter that
    /// re-emits its stale client-side layout on the next content publish or drag
    /// can no longer stomp individual members and fracture the portal. Content
    /// mutations (`SetTileRoot`/`PublishToTile`, `AddNode`, accents, input mode)
    /// are unaffected — they apply within the viewer-defined geometry, per the
    /// text-stream-portals resize contract ("the owning adapter … MUST NOT veto
    /// or reposition the surface"; adapter content "SHALL apply within the
    /// gesture-defined geometry").
    ///
    /// Viewer-driven resize/drag write `tile.bounds` directly and therefore
    /// bypass the lock; only adapter-originated `update_tile_bounds` calls are
    /// gated. Cleared per tile by the reset-geometry affordance and on tile
    /// removal.
    ///
    /// [`SceneMutation::UpdateTileBounds`]: crate::mutation::SceneMutation::UpdateTileBounds
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub viewer_geometry_locked: HashSet<SceneId>,

    /// Viewer-local per-tile font-scale multiplier for whole-portal resize text
    /// scaling (hud-ovjxu.1, spec §Portal Resize Text Scaling).
    ///
    /// Accumulates the portal's WIDTH ratio across resize steps (grow ⇒ >1,
    /// shrink ⇒ <1). The compositor multiplies each text node's published
    /// `font_size_px` by this factor at text-collection time and clamps the
    /// result to the token-defined legible min/max — so the text grows and
    /// shrinks with the portal without ever mutating the adapter-published node
    /// content. This is the durable, viewer-local design required by the spec
    /// ("font scaling ... MUST NOT alter the adapter-published content"): a
    /// content republish carries the base font, and the multiplier re-applies at
    /// render, so an adapter update can never reset the viewer's zoom (contrast a
    /// stored-font mutation, which a republish would stomp — the reason the
    /// [`Self::viewer_geometry_locked`] lock exists for bounds).
    ///
    /// Absent entry ⇒ 1.0 (no scaling). Cleared on tile removal and by the
    /// reset-geometry affordance alongside the geometry lock.
    ///
    /// Ephemeral: skipped during serialization.
    #[serde(skip, default)]
    pub tile_font_scale: HashMap<SceneId, f32>,
}

use super::SceneGraph;

impl SceneGraph {
    /// Queue a runtime-registered widget SVG asset for compositor registration.
    pub fn enqueue_widget_svg_asset(
        &mut self,
        widget_type_id: &str,
        svg_filename: &str,
        svg_bytes: Vec<u8>,
    ) {
        self.overlay.pending_widget_svg_assets.push((
            widget_type_id.to_string(),
            svg_filename.to_string(),
            svg_bytes,
        ));
    }

    /// Drain pending runtime widget SVG assets.
    pub fn drain_pending_widget_svg_assets(&mut self) -> Vec<(String, String, Vec<u8>)> {
        self.overlay.pending_widget_svg_assets.drain(..).collect()
    }

    /// Drain the list of tile IDs removed since the last call.
    ///
    /// Populated by [`SceneGraph::remove_tile_and_nodes`].  The windowed
    /// runtime calls this in `about_to_wait` (inside
    /// `prune_portal_resize_states`) to eagerly prune per-tile state held
    /// outside the scene graph (e.g. `portal_resize_states`).
    pub fn drain_removed_tile_ids(&mut self) -> Vec<SceneId> {
        self.overlay.recently_removed_tile_ids.drain(..).collect()
    }

    /// Record an accepted MutationBatch id as pending on-screen present (hud-91uu6).
    ///
    /// Called by [`SceneGraph::apply_batch`] for every batch that applied. The id
    /// is drained at the next frame present by [`Self::drain_present_ack_batch_ids`]
    /// and paired with the presented frame number + present wall-clock to form a
    /// `FramePresented` acknowledgment.
    pub fn record_present_ack_batch(&mut self, batch_id: SceneId) {
        self.overlay.present_ack_pending_batch_ids.push(batch_id);
    }

    /// Drain the batch_ids applied since the last present (hud-91uu6).
    ///
    /// Returns them in application order. The render loop calls this once per
    /// presented frame; a non-empty result is stamped with the frame number and
    /// present wall-clock and emitted as a `FramePresented` event to
    /// TELEMETRY_FRAMES subscribers.
    pub fn drain_present_ack_batch_ids(&mut self) -> Vec<SceneId> {
        self.overlay
            .present_ack_pending_batch_ids
            .drain(..)
            .collect()
    }

    /// Take viewer geometry authority over a tile (hud-lyqun).
    ///
    /// Called for every member of a portal group when the viewer moves or
    /// resizes the whole portal, so a subsequent adapter `UpdateTileBounds`
    /// republish cannot reposition the member and fracture the group.
    pub fn lock_viewer_geometry(&mut self, tile_id: SceneId) {
        self.overlay.viewer_geometry_locked.insert(tile_id);
    }

    /// Release viewer geometry authority over a tile, restoring adapter control
    /// of its bounds. Called by the reset-geometry affordance and on tile
    /// removal.
    pub fn unlock_viewer_geometry(&mut self, tile_id: SceneId) {
        self.overlay.viewer_geometry_locked.remove(&tile_id);
    }

    /// The viewer-local font-scale multiplier for `tile_id` (hud-ovjxu.1).
    /// Returns `1.0` when no scaling has been applied (the common case).
    pub fn tile_font_scale(&self, tile_id: SceneId) -> f32 {
        self.overlay
            .tile_font_scale
            .get(&tile_id)
            .copied()
            .unwrap_or(1.0)
    }

    /// Set the viewer-local font-scale multiplier for `tile_id` (hud-ovjxu.1).
    ///
    /// A factor of exactly `1.0` removes the entry (back to the default) so the
    /// map only holds tiles the viewer has actually zoomed. Non-finite or
    /// non-positive factors are ignored (defensive — a resize ratio is always a
    /// positive finite number).
    pub fn set_tile_font_scale(&mut self, tile_id: SceneId, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        if (factor - 1.0).abs() < f32::EPSILON {
            self.overlay.tile_font_scale.remove(&tile_id);
        } else {
            self.overlay.tile_font_scale.insert(tile_id, factor);
        }
    }

    /// Clear any viewer-local font scaling for `tile_id`, restoring the
    /// adapter-published font size. Called by the reset-geometry affordance and
    /// on tile removal, alongside [`Self::unlock_viewer_geometry`].
    pub fn clear_tile_font_scale(&mut self, tile_id: SceneId) {
        self.overlay.tile_font_scale.remove(&tile_id);
    }

    /// Whether the viewer holds geometry authority over `tile_id` — i.e. adapter
    /// bounds republishes are currently suppressed for it.
    pub fn is_viewer_geometry_locked(&self, tile_id: SceneId) -> bool {
        self.overlay.viewer_geometry_locked.contains(&tile_id)
    }

    /// Register local-first scroll config for a tile.
    pub fn register_tile_scroll_config(
        &mut self,
        tile_id: SceneId,
        config: TileScrollConfig,
    ) -> Result<(), ValidationError> {
        if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }
        self.overlay.tile_scroll_configs.insert(tile_id, config);
        self.overlay
            .tile_scroll_offsets
            .entry(tile_id)
            .or_insert((0.0, 0.0));
        Ok(())
    }

    /// Remove local-first scroll config and offset state for a tile.
    pub fn clear_tile_scroll_config(&mut self, tile_id: SceneId) {
        self.overlay.tile_scroll_configs.remove(&tile_id);
        self.overlay.tile_scroll_offsets.remove(&tile_id);
        self.overlay.displayed_tile_scroll_offsets.remove(&tile_id);
    }

    /// Get the registered local-first scroll config for a tile.
    pub fn tile_scroll_config(&self, tile_id: SceneId) -> Option<TileScrollConfig> {
        self.overlay.tile_scroll_configs.get(&tile_id).copied()
    }

    /// Set the lifecycle-affordance accent for a tile (latest-wins; coalescible).
    ///
    /// Bumps `scene.version` only when the stored accent actually changes, so an
    /// accent-only transition re-arms the #943 idle present-gate (which repaints
    /// on `scene.version != last_rendered`) and the new color paints even when no
    /// content mutation co-travels in the batch. A redundant re-publish of the
    /// same accent leaves the version untouched, keeping a steady-state portal
    /// idle.
    pub fn set_tile_lifecycle_accent(
        &mut self,
        tile_id: SceneId,
        accent: LifecycleAccent,
    ) -> Result<(), ValidationError> {
        if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }
        if self.overlay.tile_lifecycle_accents.get(&tile_id) != Some(&accent) {
            self.overlay.tile_lifecycle_accents.insert(tile_id, accent);
            self.version += 1;
        }
        Ok(())
    }

    /// Set the lifecycle accent with a full lease gate (checked path).
    ///
    /// Mirrors the checked tile-content mutations
    /// ([`set_tile_root_checked`](Self::set_tile_root_checked),
    /// [`update_tile_input_mode`](Self::update_tile_input_mode),
    /// [`set_portal_surface`](Self::set_portal_surface)): namespace isolation, a
    /// live `require_active_lease`, and `ModifyOwnTiles`. The unchecked
    /// [`set_tile_lifecycle_accent`](Self::set_tile_lifecycle_accent) only checks
    /// tile existence, so the in-process portal render-batch path
    /// (`apply_portal_render_batch_to_scene`) — which bypasses the `apply_batch`
    /// Stage-1 lease check — could otherwise mutate the accent overlay and bump
    /// `scene.version` under a safe-mode-suspended (or orphaned/expired) lease,
    /// escaping lease suspension (hud-a745w). `require_active_lease` accepts only
    /// the `Active` state, so the lease-grace degraded repaint — which reconnects
    /// the driver lease to `Active` before rendering (hud-i429x) — still applies,
    /// exactly matching the sibling `set_tile_root_checked` content paint in the
    /// same batch.
    pub fn set_tile_lifecycle_accent_checked(
        &mut self,
        tile_id: SceneId,
        accent: LifecycleAccent,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        self.set_tile_lifecycle_accent(tile_id, accent)
    }

    /// Clear the lifecycle-affordance accent for a tile (no-op if unset).
    ///
    /// Bumps `scene.version` only when an accent was actually present, so a
    /// redaction-CLEAR transition re-arms the present-gate and the stale accent
    /// is removed on screen; a clear with nothing stored stays idle.
    pub fn clear_tile_lifecycle_accent(&mut self, tile_id: SceneId) {
        if self
            .overlay
            .tile_lifecycle_accents
            .remove(&tile_id)
            .is_some()
        {
            self.version += 1;
        }
    }

    /// Clear the lifecycle accent with a full lease gate (checked path).
    ///
    /// The clear-side counterpart to
    /// [`set_tile_lifecycle_accent_checked`](Self::set_tile_lifecycle_accent_checked):
    /// a suspended/orphaned/expired lease must not be able to mutate the accent
    /// overlay (or bump `scene.version`) through the in-process portal render-batch
    /// path, even to clear it (hud-a745w).
    pub fn clear_tile_lifecycle_accent_checked(
        &mut self,
        tile_id: SceneId,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        self.clear_tile_lifecycle_accent(tile_id);
        Ok(())
    }

    /// Get the lifecycle-affordance accent for a tile, if any.
    pub fn tile_lifecycle_accent(&self, tile_id: SceneId) -> Option<LifecycleAccent> {
        self.overlay.tile_lifecycle_accents.get(&tile_id).copied()
    }

    // ── Composer interaction (hud-iofav) ─────────────────────────────────────

    /// Set (or replace) the composer interaction hit-region spec for a tile
    /// (latest-wins; coalescible), then reconcile the derived scene node.
    ///
    /// The spec is stored as overlay state keyed by `tile_id`; the scene derives a
    /// [`HitRegionNode`] child of the tile root from it and re-attaches that node
    /// after every `SetTileRoot`/`PublishToTile` republish (see
    /// [`ensure_tile_composer_node`](Self::ensure_tile_composer_node)). This keeps
    /// an interaction-enabled portal's transcript republish coalescible: the
    /// composer never rides a per-republish `AddNode` (which would flip the batch
    /// Transactional, hud-mzk74 / hud-iofav).
    ///
    /// Bumps `scene.version` only when the stored spec actually changes, so a
    /// redundant re-publish of the same composer keeps a steady-state portal idle
    /// (mirroring [`set_tile_lifecycle_accent`](Self::set_tile_lifecycle_accent)).
    pub fn set_tile_composer_interaction(
        &mut self,
        tile_id: SceneId,
        region: HitRegionNode,
    ) -> Result<(), ValidationError> {
        if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }
        let changed = self.overlay.tile_composer_interactions.get(&tile_id) != Some(&region);
        if changed {
            self.overlay
                .tile_composer_interactions
                .insert(tile_id, region);
            // Force a rebuild so the new spec's bounds/flags take effect: the
            // current derived node (if any) may have been attached from the prior
            // spec earlier in this same batch (the `PublishToTile` reattach runs
            // before this mutation).
            self.detach_tile_composer_node(tile_id);
        }
        self.ensure_tile_composer_node(tile_id);
        if changed {
            self.version += 1;
        }
        Ok(())
    }

    /// Set the composer interaction with a full lease gate (checked
    /// path).
    ///
    /// Mirrors the checked tile-content mutations
    /// ([`set_tile_root_checked`](Self::set_tile_root_checked),
    /// [`set_tile_lifecycle_accent_checked`](Self::set_tile_lifecycle_accent_checked)):
    /// namespace isolation, a live `require_active_lease`, and `ModifyOwnTiles`. The
    /// unchecked [`set_tile_composer_interaction`](Self::set_tile_composer_interaction)
    /// only checks tile existence, so the in-process portal render-batch path — which
    /// bypasses the `apply_batch` Stage-1 lease check — could otherwise attach the
    /// composer node and bump `scene.version` under a suspended/orphaned/expired
    /// lease, escaping lease suspension (hud-a745w).
    pub fn set_tile_composer_interaction_checked(
        &mut self,
        tile_id: SceneId,
        region: HitRegionNode,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        self.set_tile_composer_interaction(tile_id, region)
    }

    /// Clear the composer interaction for a tile, detaching the derived hit-region
    /// node (no-op if unset).
    ///
    /// Bumps `scene.version` only when a spec was actually present, so a
    /// disable-transition removes the region on screen while a clear with nothing
    /// stored stays idle (mirroring
    /// [`clear_tile_lifecycle_accent`](Self::clear_tile_lifecycle_accent)).
    pub fn clear_tile_composer_interaction(&mut self, tile_id: SceneId) {
        if self
            .overlay
            .tile_composer_interactions
            .remove(&tile_id)
            .is_some()
        {
            self.detach_tile_composer_node(tile_id);
            self.version += 1;
        }
    }

    /// Clear the composer interaction with a full lease gate (checked
    /// path) — the clear-side counterpart to
    /// [`set_tile_composer_interaction_checked`](Self::set_tile_composer_interaction_checked)
    /// (hud-a745w).
    pub fn clear_tile_composer_interaction_checked(
        &mut self,
        tile_id: SceneId,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        self.clear_tile_composer_interaction(tile_id);
        Ok(())
    }

    /// Get the composer interaction hit-region spec for a tile, if any.
    pub fn tile_composer_interaction(&self, tile_id: SceneId) -> Option<&HitRegionNode> {
        self.overlay.tile_composer_interactions.get(&tile_id)
    }

    // ── Portal surface (hud-tc153) ──────────────────

    /// Declare or replace the first-class portal surface descriptor over a tile.
    ///
    /// Enforces namespace isolation plus a live lease check (mirroring the
    /// checked tile-content mutation paths such as
    /// [`set_tile_root_checked`](Self::set_tile_root_checked)): a portal
    /// surface is a content-layer, lease-governed object.
    ///
    /// Validates the surface's structural invariants
    /// ([`PortalSurface::validate_structure`]) and that every part's referenced
    /// backing node exists and belongs to `tile_id`, so a portal surface cannot
    /// reference another agent's nodes. Bumps `scene.version` only when the stored
    /// descriptor actually changes, keeping a steady-state portal idle.
    pub fn set_portal_surface(
        &mut self,
        tile_id: SceneId,
        surface: PortalSurface,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        if let Err(reason) = surface.validate_structure() {
            return Err(ValidationError::InvalidPortalSurface { tile_id, reason });
        }
        // Every materialized part node must exist within this tile's node tree.
        for part in &surface.parts {
            if let Some(node_id) = part.node {
                if !self.node_belongs_to_tile(node_id, tile_id) {
                    return Err(ValidationError::InvalidPortalSurface {
                        tile_id,
                        reason: format!(
                            "part {:?} references node {node_id} not reachable from tile {tile_id}",
                            part.kind
                        ),
                    });
                }
            }
        }
        if self.overlay.portal_surfaces.get(&tile_id) != Some(&surface) {
            self.overlay.portal_surfaces.insert(tile_id, surface);
            self.version += 1;
        }
        Ok(())
    }

    /// Patch the lifecycle and/or display state of an existing portal surface.
    ///
    /// This is the coalescible StateStream update path: a `None` field is left
    /// unchanged. Errors if no portal surface has been declared on `tile_id`.
    /// Bumps `scene.version` only when a field actually changes.
    ///
    /// Enforces the same namespace + live lease gate as
    /// [`set_portal_surface`](Self::set_portal_surface) before touching state.
    pub fn update_portal_surface_state(
        &mut self,
        tile_id: SceneId,
        lifecycle: Option<PortalLifecycleState>,
        display_state: Option<PortalDisplayState>,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        let surface = self
            .overlay
            .portal_surfaces
            .get_mut(&tile_id)
            .ok_or(ValidationError::PortalSurfaceNotFound { tile_id })?;
        let mut changed = false;
        if let Some(next) = lifecycle {
            if surface.lifecycle != next {
                surface.lifecycle = next;
                changed = true;
            }
        }
        if let Some(next) = display_state {
            if surface.display_state != next {
                surface.display_state = next;
                changed = true;
            }
        }
        if changed {
            self.version += 1;
        }
        Ok(())
    }

    /// Get the portal surface descriptor for a tile, if any.
    pub fn portal_surface(&self, tile_id: SceneId) -> Option<&PortalSurface> {
        self.overlay.portal_surfaces.get(&tile_id)
    }

    /// Resolve the lease backing `tile_id`, enforcing namespace isolation.
    ///
    /// Mirrors the tile-content `get_tile_lease_checked` gate (which lives in the
    /// `tiles` submodule and is not visible here): the tile must exist and belong
    /// to `agent_namespace`. Returns the tile's `lease_id` so the caller can layer
    /// on `require_active_lease`.
    fn portal_tile_lease_checked(
        &self,
        tile_id: SceneId,
        agent_namespace: &str,
    ) -> Result<SceneId, ValidationError> {
        let tile = self
            .tiles
            .get(&tile_id)
            .ok_or(ValidationError::TileNotFound { id: tile_id })?;
        if tile.namespace != agent_namespace {
            return Err(ValidationError::NamespaceMismatch {
                tile_id,
                tile_namespace: tile.namespace.clone(),
                agent_namespace: agent_namespace.to_string(),
            });
        }
        Ok(tile.lease_id)
    }

    /// Drop any portal-surface part `node` references for `tile_id` that no longer
    /// resolve within the tile's current node tree.
    ///
    /// The descriptor is deliberately kept across `SetTileRoot`/`PublishToTile`
    /// content republishes (identity / lifecycle / geometry survive), but those
    /// paths replace the tile's node subtree, so any `PortalPart.node` pointing at
    /// a removed node would dangle. Rather than leave stale `SceneId`s that
    /// consumers cannot resolve, this nulls them back to "not yet materialized"
    /// (`node = None`); the owning adapter re-binds them on its next
    /// `SetPortalSurface`. Bumps `scene.version` only when a reference is actually
    /// pruned. Called by the tile-root replacement path (hud-tc153 review P2).
    pub(crate) fn revalidate_portal_surface_part_nodes(&mut self, tile_id: SceneId) {
        // Collect the stale node refs first to avoid borrowing `self` mutably and
        // immutably at once (node_belongs_to_tile needs &self).
        let Some(surface) = self.overlay.portal_surfaces.get(&tile_id) else {
            return;
        };
        let stale: Vec<SceneId> = surface
            .parts
            .iter()
            .filter_map(|p| p.node)
            .filter(|&node_id| !self.node_belongs_to_tile(node_id, tile_id))
            .collect();
        if stale.is_empty() {
            return;
        }
        let surface = self
            .overlay
            .portal_surfaces
            .get_mut(&tile_id)
            .expect("portal surface presence verified immediately above");
        let mut changed = false;
        for part in &mut surface.parts {
            if part.node.is_some_and(|n| stale.contains(&n)) {
                part.node = None;
                changed = true;
            }
        }
        if changed {
            self.version += 1;
        }
    }

    /// Whether `node_id` exists and is reachable from `tile_id`'s root node tree.
    fn node_belongs_to_tile(&self, node_id: SceneId, tile_id: SceneId) -> bool {
        let Some(tile) = self.tiles.get(&tile_id) else {
            return false;
        };
        let Some(root) = tile.root_node else {
            return false;
        };
        // The node itself must exist in the graph.
        if !self.nodes.contains_key(&node_id) {
            return false;
        }
        // Walk the tree from the root looking for node_id.
        let mut stack = vec![root];
        while let Some(cur) = stack.pop() {
            if cur == node_id {
                return true;
            }
            if let Some(n) = self.nodes.get(&cur) {
                stack.extend(n.children.iter().copied());
            }
        }
        false
    }

    /// Set the tile-local scroll offset used by runtime local feedback.
    pub fn set_tile_scroll_offset_local(
        &mut self,
        tile_id: SceneId,
        offset_x: f32,
        offset_y: f32,
    ) -> Result<(), ValidationError> {
        if !self.tiles.contains_key(&tile_id) {
            return Err(ValidationError::TileNotFound { id: tile_id });
        }
        self.overlay
            .tile_scroll_offsets
            .insert(tile_id, (offset_x, offset_y));
        Ok(())
    }

    /// Get the current runtime-local scroll offset for a tile.
    ///
    /// This is the *authoritative* offset — the scroll target the runtime
    /// committed. During a smoothed scroll animation the renderer draws at the
    /// lagged displayed offset instead; use
    /// [`effective_tile_scroll_offset_local`](Self::effective_tile_scroll_offset_local)
    /// for pointer mapping so input and render agree.
    pub fn tile_scroll_offset_local(&self, tile_id: SceneId) -> (f32, f32) {
        self.overlay
            .tile_scroll_offsets
            .get(&tile_id)
            .copied()
            .unwrap_or((0.0, 0.0))
    }

    /// Publish the displayed (smoothed/lagged) scroll offset for a tile.
    ///
    /// Called by the compositor once per frame, after advancing the per-tile
    /// `ScrollSmoother`s, so the hit-test path can map pointer coordinates
    /// against the same offset the renderer drew with (hud-3lynp). Only invoked
    /// while windowed scroll smoothing is active.
    pub fn set_displayed_tile_scroll_offset(
        &mut self,
        tile_id: SceneId,
        offset_x: f32,
        offset_y: f32,
    ) {
        self.overlay
            .displayed_tile_scroll_offsets
            .insert(tile_id, (offset_x, offset_y));
    }

    /// Drop all published displayed scroll offsets.
    ///
    /// Called by the compositor when scroll smoothing is disabled (headless /
    /// snap) so hit-testing falls back to the authoritative offset.
    pub fn clear_displayed_tile_scroll_offsets(&mut self) {
        self.overlay.displayed_tile_scroll_offsets.clear();
    }

    /// Retain only the displayed scroll offsets whose tile satisfies `keep`.
    ///
    /// Lets the compositor prune overrides for tiles that no longer have an
    /// active smoother (no longer scrollable / removed) each frame.
    pub fn retain_displayed_tile_scroll_offsets<F: FnMut(SceneId) -> bool>(&mut self, mut keep: F) {
        self.overlay
            .displayed_tile_scroll_offsets
            .retain(|id, _| keep(*id));
    }

    /// Get the *effective* tile-local scroll offset for pointer mapping.
    ///
    /// Returns the displayed (smoothed/lagged) offset when the compositor has
    /// published one for this tile (i.e. a smoothed scroll is in flight);
    /// otherwise falls back to the authoritative
    /// [`tile_scroll_offset_local`](Self::tile_scroll_offset_local). This is the
    /// offset the hit-test path uses so input and render agree during animation
    /// (hud-3lynp).
    pub fn effective_tile_scroll_offset_local(&self, tile_id: SceneId) -> (f32, f32) {
        self.overlay
            .displayed_tile_scroll_offsets
            .get(&tile_id)
            .copied()
            .unwrap_or_else(|| self.tile_scroll_offset_local(tile_id))
    }

    // ─── Follow-tail anchor ───────────────────────────────────────────────

    /// Record whether a tile's viewport is at the tail of its content stream.
    ///
    /// Called by the runtime input layer whenever `FollowTailAnchor` transitions
    /// (typically after `ScrollState::notify_content_appended` or
    /// `ScrollState::apply_user_scroll`).
    ///
    /// `at_tail = true` means the tile uses tail-anchored truncation (newest
    /// content always visible).  `at_tail = false` means the tile uses
    /// head-anchored truncation (scrolled-back viewport, spec task 3.3
    /// append-stability guarantee).
    ///
    /// No-op if the tile does not exist (tiles without scroll state always
    /// behave as head-anchored by default).
    pub fn set_tile_follow_tail_at_tail(&mut self, tile_id: SceneId, at_tail: bool) {
        if self.tiles.contains_key(&tile_id) {
            self.overlay
                .tile_follow_tail_at_tail
                .insert(tile_id, at_tail);
        }
    }

    /// Return whether a tile's viewport is currently at the tail.
    ///
    /// Returns `true` (at-tail, tail-anchored mode) when the tile has been
    /// explicitly registered via [`set_tile_follow_tail_at_tail`] with `true`.
    ///
    /// Returns `false` (head-anchored mode) when:
    /// - the tile has been scrolled back, or
    /// - the tile has never been registered (non-scrollable or newly created
    ///   tiles that have not yet received a content-append or scroll event).
    ///
    /// Non-scrollable static-text tiles must default to `false` so that
    /// `TextOverflow::Ellipsis` shows the beginning of the text, not the end.
    pub fn tile_follow_tail_at_tail(&self, tile_id: SceneId) -> bool {
        self.overlay
            .tile_follow_tail_at_tail
            .get(&tile_id)
            .copied()
            .unwrap_or(false)
    }

    // ─── Ambient unread count (hud-g1ena.3) ───────────────────────────────

    /// Record a tile's ambient unread-output count (the aggregate
    /// `ProjectedPortalState::unread_output_count`).
    ///
    /// Called by the portal projection driver each drain, and by the wire apply
    /// paths for a bridged portal (hud-hwk2m). `count = 0` clears the badge; the
    /// producer passes `0` for both a genuine empty count and a redacted (`None`)
    /// count so no unread badge renders in either case.
    ///
    /// Bumps `scene.version` only when the stored count actually changes, so a
    /// count-only update (no co-travelling content mutation) still re-arms the
    /// #943 idle present-gate and the badge repaints — mirroring
    /// [`set_tile_lifecycle_accent`](Self::set_tile_lifecycle_accent). A redundant
    /// re-write of the same count leaves the version untouched, keeping a
    /// steady-state portal idle.
    ///
    /// No-op if the tile does not exist.
    pub fn set_tile_unread_count(&mut self, tile_id: SceneId, count: usize) {
        if !self.tiles.contains_key(&tile_id) {
            return;
        }
        if self.overlay.tile_unread_counts.get(&tile_id) != Some(&count) {
            self.overlay.tile_unread_counts.insert(tile_id, count);
            self.version += 1;
        }
    }

    /// Set the unread-output count with a full lease gate (checked
    /// path), mirroring
    /// [`set_tile_lifecycle_accent_checked`](Self::set_tile_lifecycle_accent_checked):
    /// namespace isolation, a live `require_active_lease`, and `ModifyOwnTiles`.
    ///
    /// The unchecked [`set_tile_unread_count`](Self::set_tile_unread_count) only
    /// checks tile existence, so the wire apply paths that bypass the `apply_batch`
    /// Stage-1 lease check (`apply_portal_render_batch_to_scene`, and the
    /// session-server batch that reaches `apply_single_mutation`) could otherwise
    /// mutate the badge overlay under a `ModifyOwnTiles`-revoked, safe-mode-
    /// suspended, orphaned, or expired lease — escaping tile-modification authority
    /// exactly like the accent overlay did before hud-a745w. `require_active_lease`
    /// accepts only `Active`, so the lease-grace degraded repaint (which reconnects
    /// the driver lease to `Active` before rendering, hud-i429x) still applies.
    pub fn set_tile_unread_count_checked(
        &mut self,
        tile_id: SceneId,
        count: usize,
        agent_namespace: &str,
    ) -> Result<(), ValidationError> {
        let lease_id = self.portal_tile_lease_checked(tile_id, agent_namespace)?;
        self.require_active_lease(lease_id)?;
        self.require_active_lease(lease_id)?;
        self.set_tile_unread_count(tile_id, count);
        Ok(())
    }

    /// Return a tile's ambient unread-output count (`0` when unset/cleared).
    pub fn tile_unread_count(&self, tile_id: SceneId) -> usize {
        self.overlay
            .tile_unread_counts
            .get(&tile_id)
            .copied()
            .unwrap_or(0)
    }
}
