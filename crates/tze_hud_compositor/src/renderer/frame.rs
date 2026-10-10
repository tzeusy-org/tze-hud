use std::borrow::Cow;

use super::*;
use crate::display::FrameTarget;
use crate::pipeline::{RoundedRectBorder, RoundedRectClip};

/// One display window's view of a [`WindowedFrameBuild`], in that window's
/// pixel / NDC space (see [`Compositor::target_view`]).
struct TargetView<'a> {
    width: u32,
    height: u32,
    vertices: Cow<'a, [RectVertex]>,
    textured_cmds: Cow<'a, [TexturedDrawCmd]>,
    rr_background: Cow<'a, [RoundedRectDrawCmd]>,
    rr_content: Cow<'a, [RoundedRectDrawCmd]>,
    rr_post: Cow<'a, [RoundedRectDrawCmd]>,
    text_items: Cow<'a, [TextItem]>,
    card_items: &'a [TextItem],
    drag_handle_vertices: Cow<'a, [RectVertex]>,
    drag_highlight_cmds: Cow<'a, [RoundedRectDrawCmd]>,
    focus_ring_vertices: Cow<'a, [RectVertex]>,
    context_menu_vertices: Cow<'a, [RectVertex]>,
    safe_mode_vertices: Cow<'a, [RectVertex]>,
    system_card_vertices: &'a [RectVertex],
    widget_quads: &'a [crate::widget::WidgetDrawQuad],
}

fn hash_f32s(hasher: &mut impl std::hash::Hasher, values: &[f32]) {
    for v in values {
        hasher.write_u32(v.to_bits());
    }
}

/// Hash the triangles (3 vertices each) that touch the NDC viewport. A
/// triangle's bounding box is tested against `[-1, 1]²`; anything wholly off
/// this window contributes nothing.
fn hash_visible_triangles(hasher: &mut impl std::hash::Hasher, vertices: &[RectVertex]) {
    for tri in vertices.chunks(3) {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for v in tri {
            x0 = x0.min(v.position[0]);
            x1 = x1.max(v.position[0]);
            y0 = y0.min(v.position[1]);
            y1 = y1.max(v.position[1]);
        }
        if x1 < -1.0 || x0 > 1.0 || y1 < -1.0 || y0 > 1.0 {
            continue;
        }
        for v in tri {
            hash_f32s(hasher, &v.position);
            hash_f32s(hasher, &v.color);
        }
    }
}

/// All CPU-side, scene-free frame data produced under the scene lock by
/// [`Compositor::build_windowed_frame`] and consumed lock-free by
/// [`Compositor::present_windowed_frame`] (hud-uyhpn).
///
/// The windowed frame loop builds this value while holding the scene lock, then
/// DROPS the lock before calling `present_windowed_frame`, which performs the
/// vsync-blocking `acquire_frame()` + encode + submit + `device.poll(Wait)`.
/// Because every field here is owned (no `&SceneGraph` borrow survives), the
/// entire GPU present tail runs without the scene lock held — collapsing the
/// former ~full-refresh-interval lock hold (which starved the main-thread
/// interaction path's `spin_acquire` and dropped drag-move samples) down to the
/// cheap scene-read build phase.
pub struct WindowedFrameBuild {
    /// Frame telemetry accumulated during the build (tile/node/lease counts,
    /// frame number). `present_windowed_frame` fills in the encode/submit/total
    /// timings and returns it.
    telemetry: FrameTelemetry,
    /// Surface dimensions this frame was built for.
    surf_w: u32,
    surf_h: u32,
    /// Flat-rect geometry (Background → tiles → Content → Chrome) and the vertex
    /// offsets just past Background and at the start of Chrome flat geometry.
    vertices: Vec<RectVertex>,
    bg_vertex_count: usize,
    chrome_vertex_start: usize,
    /// Textured image draw commands (composited above the color geometry).
    textured_cmds: Vec<TexturedDrawCmd>,
    /// Scene-free encode inputs (rounded-rect cmds + unprepared text). Each
    /// display window maps and glyphon-prepares its own view at present time.
    encode_sources: EncodeSources,
    /// Precomputed drag-handle chrome vertices.
    drag_handle_vertices: Vec<RectVertex>,
    /// Active-drag highlight borders, drawn just before the drag-handle pass.
    drag_highlight_cmds: Vec<RoundedRectDrawCmd>,
    /// Precomputed keyboard focus-ring chrome vertices.
    focus_ring_vertices: Vec<RectVertex>,
    /// Precomputed drag-handle reset context-menu chrome vertices.
    context_menu_vertices: Vec<RectVertex>,
    /// Safe-mode overlay quads (empty unless safe mode is active).
    safe_mode_vertices: Vec<RectVertex>,
    /// System card backdrop quads (empty unless a card is set); the very last
    /// pass, above the safe-mode overlay.
    system_card_vertices: Vec<RectVertex>,
    /// Precomputed per-instance widget draw quads.
    widget_quads: Vec<crate::widget::WidgetDrawQuad>,
    /// Wall-clock start of the frame, for the total frame-time telemetry.
    frame_start: std::time::Instant,
}

/// Exact GPU-operation outcome for one windowed present attempt.
///
/// These booleans are independent of the aggregate stage-success sentinel in
/// [`FrameTelemetry`]: an acquisition or queue submission remains an actual
/// operation even when a later operation in the stage fails.
pub struct WindowedPresentOutcome {
    pub telemetry: FrameTelemetry,
    /// True only when this build acquired a new surface texture. Reusing a
    /// pending texture before main-thread presentation is render work, not a
    /// second surface acquisition.
    pub surface_acquired: bool,
    pub gpu_submitted: bool,
}

impl WindowedFrameBuild {
    /// Surface dimensions this frame was built for.
    pub fn size(&self) -> (u32, u32) {
        (self.surf_w, self.surf_h)
    }
}

#[cfg(test)]
impl WindowedFrameBuild {
    /// Total flat-rect vertices built this frame (test / diagnostic accessor).
    ///
    /// Exposed so a test can assert that [`Compositor::build_windowed_frame`]
    /// produces scene geometry WITHOUT ever acquiring the surface — the structural
    /// property that lets the windowed loop drop the scene lock before present.
    pub(crate) fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    /// Flat-rect geometry built for this frame (test / diagnostic accessor).
    ///
    /// Kept behind `cfg(test)` because callers must not reuse a frame build's
    /// private staging buffer in production.  Renderer regressions use this to
    /// verify that the first frame after a local reflow derives chrome from the
    /// same layout as its text pass.
    pub(crate) fn flat_rect_vertices(&self) -> &[RectVertex] {
        &self.vertices
    }

    /// Precomputed drag-handle chrome vertices this frame (test accessor).
    pub(crate) fn drag_handle_vertex_count(&self) -> usize {
        self.drag_handle_vertices.len()
    }

    /// Tile count recorded in this frame's telemetry (test accessor).
    pub(crate) fn tile_count(&self) -> u32 {
        self.telemetry.tile_count
    }
}

impl Compositor {
    /// Build the shared per-frame vertex / textured-command lists from the scene.
    ///
    /// Single source of truth for the scene→geometry stage shared by
    /// `render_frame` and `render_frame_headless` (hud-8uafa). It covers the
    /// canonical layer ordering (Background zones → tiles → Content zones →
    /// Chrome zones), the per-tile background / content / scroll-indicator /
    /// composer-overlay geometry, and the overlay-mode alpha-zeroing quad. The
    /// public render entry points keep their distinct encode tails (windowed
    /// present, headless readback + hit regions) and consume this shared body.
    ///
    /// All colors flow through [`gpu_color_raw`], which is identity outside
    /// overlay mode and applies the sRGB-premultiply transform inside it. This is
    /// correct for every call site: only the windowed path is ever in overlay
    /// mode, so the headless path sees the raw color unchanged — which
    /// is exactly what their hand-written predecessors did. (Unifying here also
    /// removes the previously-latent divergence where the windowed tile
    /// background went through `gpu_color_raw` but the headless copy did
    /// not — benign only because headless never set overlay mode.)
    ///
    /// Returns geometry and the Background-end/Chrome-start flat vertex offsets,
    /// so each layer's SDF can be interleaved by the shared GPU encoder.
    /// Also populates the `tile_count`,
    /// `node_count`, and `active_leases` telemetry fields.
    pub(super) fn build_frame_vertices(
        &mut self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
        telemetry: &mut FrameTelemetry,
    ) -> (Vec<RectVertex>, Vec<TexturedDrawCmd>, usize, usize) {
        let animation_now = std::time::Instant::now();
        // Collect visible tiles, re-sorted with drag-z-order boost applied.
        let tiles = Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene);
        telemetry.tile_count = tiles.len() as u32;
        telemetry.node_count = scene.node_count() as u32;
        telemetry.active_leases = scene.leases.len() as u32;

        // Runtime-authored viewer reply echoes (hud-nx7yq.3): drain any pending
        // submit-time appends into the per-tile store and prune echoes for tiles
        // that no longer exist, before the text pass reads them.
        let queue = std::sync::Arc::clone(&self.viewer_echo_queue);
        self.viewer_echoes.drain_queue(&queue);
        self.viewer_echoes
            .retain_tiles(|tile_id| scene.tiles.contains_key(&tile_id));

        // Resolve every resize-sensitive local layout BEFORE any geometry reads
        // it.  The composer fill and viewer-echo dividers below, plus the later
        // text pass, must share one same-frame wrapped layout: reflowing only at
        // text collection would leave chrome one frame behind after a resize.
        // These are deliberately once-per-frame measurements, off the transcript
        // hot path.
        self.prime_composer_scroll_offset(scene);
        // Measure the viewer-echo history wrap (total + per-entry) before the
        // tile loop so the turn-divider pass can place a token-styled rule on
        // each entry boundary from the same fresh composer anchor (hud-hsc1t).
        self.prime_viewer_echo_layout(scene);
        // hud-pd9bp: resolve vertical-flow child offsets once per frame,
        // before the geometry + text passes read them. No-op for Absolute scenes.
        self.prime_vertical_flow_layout(scene);

        let mut vertices: Vec<RectVertex> = Vec::new();
        let mut textured_cmds: Vec<TexturedDrawCmd> = Vec::new();

        // In overlay mode, prepend a full-screen quad to zero out alpha.
        // No-op for the headless/chrome paths, which are never in overlay mode.
        if self.overlay_mode {
            vertices.extend_from_slice(&rect_vertices(
                0.0,
                0.0,
                sw,
                sh,
                sw,
                sh,
                [0.0, 0.0, 0.0, 0.0],
            ));
        }

        // ── Reclaim stale image textures before admitting this frame ─────────
        // Collect the complete current-frame guard set first, then evict only
        // allocations outside it. This creates headroom before cache admission
        // while ensuring a resource referenced by this frame is never freed.
        let mut image_refs = Self::scene_image_resource_ids(scene);
        image_refs.extend(Self::scene_icon_resource_ids(scene));
        self.evict_unused_image_textures(&image_refs);

        // Ensure current-frame image/icon textures only after the safe eviction
        // pass, so class/aggregate reserve sees all reclaimable headroom.
        self.ensure_scene_image_textures(scene);
        self.ensure_scene_icon_textures(scene);

        // Update zone animation states (fade-in/fade-out) before rendering.
        // Must run before any render_zone_content call below.
        self.update_zone_animations_at(scene, animation_now);
        // §6.3 portal transition: advance per-portal-tile fade animations
        // alongside zone animations (hud-58rg1). Folded in here so all three
        // render entry points share the single update site.
        self.update_portal_tile_animations_at(scene, animation_now);
        // Smooth scroll / animated follow-tail (hud-bq0gl.10): advance the
        // per-portal-tile scroll smoothers once per frame, BEFORE the tile loop
        // and the later text/encode passes read displayed offsets via
        // `display_tile_scroll_offset`. No-op (snap) in headless mode.
        self.update_scroll_smoothing(scene);

        // ── Layer ordering: Background → Tiles → Content zones → Chrome zones ─
        // Background zones render first so agent tiles occlude them.
        self.render_zone_content_at(
            scene,
            &mut vertices,
            &mut textured_cmds,
            (sw, sh),
            Some(LayerAttachment::Background),
            animation_now,
        );

        // Capture the vertex count after Background zones so the caller can split
        // the flat-rect pass and interleave the Background SDF pass.
        let bg_vertex_count = vertices.len();

        // Resolve scroll-indicator tokens once per frame (not per tile) since
        // the token map does not change during the tile loop.
        let scroll_indicator_tokens = resolve_scroll_indicator_tokens(&self.token_map);
        // Resolve "jump to latest" pill tokens once per frame (hud-9ci61).
        let jump_to_latest_tokens = resolve_jump_to_latest_tokens(&self.token_map);
        // Resolve composer overlay tokens once per frame (hud-r3ax6).
        let composer_overlay_tokens = resolve_composer_overlay_tokens(&self.token_map);

        for tile in &tiles {
            if let Some(bg_color) = self.tile_background_color(tile, scene) {
                let verts = rect_vertices(
                    tile.bounds.x,
                    tile.bounds.y,
                    tile.bounds.width,
                    tile.bounds.height,
                    sw,
                    sh,
                    self.gpu_color_raw(bg_color),
                );
                vertices.extend_from_slice(&verts);
            }

            // Render nodes within the tile.
            if let Some(root_id) = tile.root_node {
                self.render_node(
                    root_id,
                    tile,
                    scene,
                    &mut vertices,
                    &mut textured_cmds,
                    sw,
                    sh,
                );
            }

            // ── Lifecycle affordance accent (hud-m48i0) ────────────────────
            // A token-colored bar along the tile's left edge signalling the
            // portal's lifecycle state. Painted from runtime overlay state
            // (`tile_lifecycle_accents`) — set via the coalescible StateStream
            // `SetTileLifecycleAccent` mutation — so it survives the transcript's
            // `PublishToTile` content republishes and never rides a per-republish
            // `AddNode`. Geometry-only; carries no transcript content (redaction
            // is enforced at the producer: a redacted viewer gets no accent). The
            // color is token-resolved upstream — no literal visual value here.
            if let Some(accent) = scene.tile_lifecycle_accent(tile.id) {
                // Fold the tile's effective + portal-transition opacity so the
                // accent fades with the tile (matches the tile background).
                let opacity = self.tile_effective_opacity(tile, scene);
                if let Some((bar_w, color)) =
                    Self::lifecycle_accent_bar_geom(tile.bounds, accent, opacity)
                {
                    let accent_verts = rect_vertices(
                        tile.bounds.x,
                        tile.bounds.y,
                        bar_w,
                        tile.bounds.height,
                        sw,
                        sh,
                        self.gpu_color_raw(color),
                    );
                    vertices.extend_from_slice(&accent_verts);
                }
            }

            // ── Scroll indicator (§6b.5) ───────────────────────────────────
            // Rendered on top of the tile content. Geometry-only; carries no
            // transcript text. Redaction-safe: the indicator reveals only that
            // content overflows and approximately where the viewport sits.
            //
            // Only emitted for tiles that have a registered scroll config with
            // a known content_height (set by the portal adapter via
            // `register_tile_scroll_config`). Indicator is not shown when
            // content fits within the viewport (no overflow).
            if let Some(scroll_cfg) = scene.tile_scroll_config(tile.id) {
                if let Some(content_height) = scroll_cfg.content_height {
                    let viewport_px = tile.bounds.height;
                    let (_, scroll_offset_y) = self.display_tile_scroll_offset(scene, tile.id);
                    if let Some(geom) = tze_hud_input::compute_scroll_indicator(
                        viewport_px,
                        content_height,
                        scroll_offset_y,
                        &scroll_indicator_tokens,
                    ) {
                        // Clamp indicator width to tile width so an out-of-range
                        // token value can never push the thumb outside the tile.
                        let indicator_w = geom.width_px.min(tile.bounds.width);
                        // Track rect: right edge of the tile, full height.
                        // Thumb rect: inset within the track at thumb_y_px.
                        let track_x = tile.bounds.x + tile.bounds.width - indicator_w;
                        let thumb_color = self.gpu_color_raw([
                            scroll_indicator_tokens.color_r,
                            scroll_indicator_tokens.color_g,
                            scroll_indicator_tokens.color_b,
                            scroll_indicator_tokens.color_a,
                        ]);
                        let thumb_verts = rect_vertices(
                            track_x,
                            tile.bounds.y + geom.thumb_y_px,
                            indicator_w,
                            geom.thumb_height_px,
                            sw,
                            sh,
                            thumb_color,
                        );
                        vertices.extend_from_slice(&thumb_verts);

                        // ── Jump-to-latest pill (hud-9ci61) ──────────────────
                        // Rendered only while the tile is scrolled away from
                        // the tail (auto follow-tail already exists but had no
                        // click affordance to resume it). Geometry mirrors the
                        // hit region populated in `populate_zone_hit_regions`,
                        // computed from the same pure function so render and
                        // hit-test never disagree.
                        let scrolled_back = !scene.tile_follow_tail_at_tail(tile.id);
                        if let Some(pill) = tze_hud_input::compute_jump_to_latest_pill(
                            tile.bounds.width,
                            viewport_px,
                            scrolled_back,
                            &jump_to_latest_tokens,
                        ) {
                            let pill_color = self.gpu_color_raw([
                                jump_to_latest_tokens.color_r,
                                jump_to_latest_tokens.color_g,
                                jump_to_latest_tokens.color_b,
                                jump_to_latest_tokens.color_a,
                            ]);
                            let pill_verts = rect_vertices(
                                tile.bounds.x + pill.x_px,
                                tile.bounds.y + pill.y_px,
                                pill.width_px,
                                pill.height_px,
                                sw,
                                sh,
                                pill_color,
                            );
                            vertices.extend_from_slice(&pill_verts);
                        }
                    }
                }
            }

            // ── Composer echo overlay (hud-r3ax6) ─────────────────────────
            // Renders the local draft text background strip on top of the
            // tile.  The text itself is injected in collect_text_items via
            // collect_composer_text_item.  NO adapter round-trip.
            self.render_composer_overlay(
                tile,
                scene,
                &mut vertices,
                sw,
                sh,
                &composer_overlay_tokens,
            );

            // ── Viewer-echo turn dividers (hud-hsc1t) ─────────────────────
            // A token-styled rule between adjacent runtime-authored viewer
            // history entries so the pilot-path echo reads as discrete turns
            // (§Transcript Turn Separators). Shares the portal.divider.* tokens
            // with the markdown transcript separators; content-free geometry,
            // so nothing is revealed under redaction. No-op when the store is
            // empty or the divider token is unset.
            if !self.viewer_echoes.is_empty() {
                if let Some(sep_color) = self.markdown_tokens.separator_color {
                    let tile_opacity = self.tile_effective_opacity(tile, scene);
                    let divider_color = self.gpu_color(Rgba {
                        a: sep_color.a * tile_opacity,
                        ..sep_color
                    });
                    for rect in self.collect_viewer_echo_divider_rects(tile, scene) {
                        Self::append_clipped_rect_vertices(
                            tile,
                            rect,
                            sw,
                            sh,
                            divider_color,
                            &mut vertices,
                        );
                    }
                }
            }
        }

        // ── Resize-grip affordance (vd-crude-resize-handle-grip) ───────────────
        // A token-colored dot-grid mark at each portal (scrollable) tile's
        // bottom-right resize corner, drawn above the tile content it decorates.
        // Geometry-only; carries no transcript content (redaction-safe). Sized
        // and colored from `portal.window.resize_grip.*`; no literal visual
        // value at the call site.
        self.append_resize_grip_vertices(scene, &mut vertices, sw, sh);

        // ── Disconnection badge on orphaned tiles (invariant 4) ────────────────
        // Colored/sized from `tile.disconnect_badge.*`; reads `Tile::visual_hint`.
        self.append_disconnect_badge_vertices(scene, &mut vertices, sw, sh);

        // ── Viewer close button on the hovered tile (hud-jm8nq.11) ─────────────
        // Drawn only for the runtime-plumbed hover target; tokens come from
        // `tile.close_button.*`.
        self.append_tile_close_button_vertices(scene, &mut vertices, sw, sh);

        // Update zone animation states (fade-in/fade-out) before rendering.
        self.update_zone_animations_at(scene, animation_now);
        // §6.3 portal transition: advance per-portal-tile fade animations
        // alongside zone animations (hud-58rg1).
        self.update_portal_tile_animations_at(scene, animation_now);

        // Update streaming word-by-word reveal state.
        self.update_stream_reveals(scene);

        // Update per-portal-tile streaming-reveal fade state (hud-bl7yi): fades
        // newly-appended portal-tile content in segment-by-segment instead of
        // snapping. Mirrors the zone reveal above for the portal-tile path.
        self.update_portal_tile_reveals(scene);

        // Content zones render as a batch after all tiles (above background, below chrome).
        self.render_zone_content_at(
            scene,
            &mut vertices,
            &mut textured_cmds,
            (sw, sh),
            Some(LayerAttachment::Content),
            animation_now,
        );
        let chrome_vertex_start = vertices.len();
        // Chrome zones render last, above tiles and content zones.
        self.render_zone_content_at(
            scene,
            &mut vertices,
            &mut textured_cmds,
            (sw, sh),
            Some(LayerAttachment::Chrome),
            animation_now,
        );
        // The system card is not part of this vertex list: it is drawn by the
        // final `encode_system_card_pass` above every other pass (hud-w5zon).

        (
            vertices,
            textured_cmds,
            bg_vertex_count,
            chrome_vertex_start,
        )
    }

    /// Build all CPU-side, scene-free frame data under the scene lock (hud-uyhpn).
    ///
    /// This is the scene-reading half of the windowed present path. It performs
    /// EVERY read of (and the few writes back into) `scene` that a frame needs —
    /// vertex/geometry build, scroll-offset publish, encode-input collection
    /// (rounded rects + text prepare), drag-handle / focus-ring / context-menu /
    /// widget geometry, and drag-handle hit-region population — and returns an
    /// owned [`WindowedFrameBuild`]. It does NOT touch the swapchain surface.
    ///
    /// The caller (the windowed frame loop) drops the scene lock immediately
    /// after this returns and then calls [`Compositor::present_windowed_frame`],
    /// so the vsync-blocking acquire/submit/poll never runs while the scene lock
    /// is held. This is the core of the drag-input-starvation fix: the lock hold
    /// collapses to this cheap build phase instead of spanning a full refresh
    /// interval.
    ///
    /// Note (behaviour delta, intentional): drag-handle hit regions are now
    /// populated here — from the geometry we are about to present — rather than
    /// only on a successful present. The regions describe where handles ARE
    /// (a pure function of the scene geometry), independent of whether this
    /// particular frame reaches the surface, so refreshing them unconditionally
    /// keeps hit-testing correct even on a skipped-present frame.
    pub fn build_windowed_frame(
        &mut self,
        scene: &mut SceneGraph,
        surf_w: u32,
        surf_h: u32,
    ) -> WindowedFrameBuild {
        let frame_start = std::time::Instant::now();
        self.frame_number += 1;

        // ── Drain local composer echo state (hud-r3ax6) ───────────────────
        // Must happen before any render work so the overlay is current for
        // this frame.  Lock contention is negligible: the shared slot is
        // written ≤ once per keystroke and drained once per frame (60 Hz).
        self.drain_local_composer_state();

        let mut telemetry = FrameTelemetry::new(self.frame_number);

        // ── Phase-1 markdown cache prime (hud-380dl: commit-time prime) ─────
        // The markdown cache MUST be primed at commit time (before this build
        // runs) by an explicit `prime_markdown_cache` call at the scene-commit
        // site (Stage 3/4 of the pipeline).  By the time the build executes,
        // the cache is already populated and this block is a no-op.
        //
        // Safety fallback: if the render path somehow reaches a frame where the
        // cache has not been primed for the current scene version (e.g., the first
        // frame after compositor creation before any commit-time prime has run),
        // we prime here to preserve correctness.  In steady state this path is
        // never taken.  `markdown_prime_us` stays 0 on all normal (commit-primed)
        // frames, matching the "zero per-frame parse cost" contract.
        //
        // A debug assertion fires in test/dev builds if we ever reach this path
        // in steady state, catching regressions where a call site forgot to call
        // prime_markdown_cache before the build.
        if scene.version != self.markdown_cache_scene_version {
            debug_assert!(
                false,
                "build_windowed_frame: markdown cache was not commit-primed for scene version {} \
                 (cache version {}); falling back to in-render prime [hud-380dl]",
                scene.version, self.markdown_cache_scene_version,
            );
            self.prime_markdown_cache(scene);
            // Note: markdown_prime_us stays 0 here (the cost is absorbed into Stage 6
            // as a correctness fallback, not the normal commit-time path).
        }

        // ── Phase-1 truncation cache prime (hud-wgq7j / hud-v2z6u) ─────────────
        // The truncation cache MUST be primed at commit time (before this build
        // runs) by an explicit `prime_truncation_cache` call at the scene-commit
        // site, mirroring the markdown cache contract.  By the time the build
        // executes, the cache is already populated and this block is a no-op.
        //
        // Safety fallback: if we reach this path with a stale cache (e.g. the
        // very first frame before any commit-time prime, or a call site that
        // omitted the prime), we fall back here to preserve correctness.  In
        // steady state this path is never taken; the cost is absorbed into Stage 6.
        //
        // Unlike the markdown cache, a version mismatch here is NOT necessarily a
        // contract violation: `prime_truncation_cache` carries a mid-drag cadence
        // gate (hud-ghhxa) that intentionally DEFERS a re-prime — leaving the
        // sentinel behind `scene.version` — when geometry changes faster than
        // RESIZE_REPRIME_INTERVAL_MS.  During a fast resize drag (or the headless
        // benchmark's tight 180-frame loop), the cache legitimately lags the scene
        // for one or more frames.  We therefore trace rather than debug_assert; the
        // call to prime_truncation_cache below is itself cadence-gated and will be
        // a no-op defer when appropriate, preserving the per-frame budget [hud-v2z6u].
        if scene.version != self.truncation_cache_scene_version {
            tracing::trace!(
                scene_version = scene.version,
                cache_version = self.truncation_cache_scene_version,
                "build_windowed_frame: truncation cache lags scene (commit-prime not yet \
                 applied or cadence-deferred); applying cadence-gated in-render prime"
            );
            self.prime_truncation_cache(scene);
        }

        // Build the shared per-frame geometry (Background → tiles → Content →
        // Chrome zones). `build_frame_vertices` is the single source of truth for
        // this scene→vertex stage across all three render entry points; it also
        // populates the tile/node/lease telemetry counts and, in overlay mode,
        // the alpha-zeroing full-screen quad.
        let sw = surf_w as f32;
        let sh = surf_h as f32;
        let (vertices, textured_cmds, bg_vertex_count, chrome_vertex_start) =
            self.build_frame_vertices(scene, sw, sh, &mut telemetry);

        // Publish this frame's displayed (smoothed/lagged) scroll offsets into
        // the scene so the live hit-test path maps pointer coordinates against
        // the same offset we just drew with (hud-3lynp). build_frame_vertices
        // advanced the smoothers above; this records their displayed state.
        // No-op clear in headless/snap mode.
        //
        // Deliberately published here, AFTER the smoothers advanced and BEFORE
        // present (hud-96f3h). The override is read from the *live* scene by the
        // async input path (`SceneGraph::hit_test` → `effective_tile_scroll_offset_local`;
        // `HitTestSnapshot` does NOT carry the offset), so between frames it must
        // equal the offset currently on screen. For a presented frame that holds:
        // this frame's offset is on screen for the whole inter-frame interval, so
        // recording it post-advance keeps the override aligned. On a *skipped*
        // present (double `acquire_frame()` failure on resize/device-loss, or a
        // contained submit/present panic) the override briefly leads the on-screen
        // pixels by one frame until the next frame re-publishes — a sub-perceptual,
        // self-correcting skew, strictly better than the pre-#942 baseline.
        // Publishing only for actually-presented frames would require re-writing
        // the override AFTER present confirms; but present runs lock-free and never
        // touches the scene (the hud-uyhpn lock split, which collapsed the lock
        // hold to this build phase to stop drag-input starvation), so that would
        // reintroduce a post-present scene-lock re-acquire. Not worth it for this
        // precision case — the skew is transient and self-heals (hud-96f3h).
        self.publish_displayed_scroll_offsets(scene);

        let drag_handles = self.collect_drag_handle_entries(scene, sw, sh);
        let mut drag_handle_vertices: Vec<RectVertex> = Vec::new();
        self.append_drag_handle_vertices(scene, &drag_handles, &mut drag_handle_vertices, sw, sh);
        let drag_highlight_cmds = self.drag_highlight_cmds(scene, &drag_handles);

        // ── Widget texture sync: rasterize dirty SVGs BEFORE frame acquisition.
        // SVG rasterization can be slow; if a resize event arrives while we hold
        // the surface texture, the texture is destroyed and queue.submit panics.
        // Kept in the build phase (under the lock) since it reads scene state.
        self.sync_widget_textures(scene, self.degradation_level);
        telemetry.widget_rasterized = self
            .widget_renderer
            .as_ref()
            .map(|wr| wr.rasterized_last_sync().to_vec())
            .unwrap_or_default();

        // ── Scene-free encode inputs (rounded-rect cmds + prepared text) ─────
        // This is the second big scene read; collecting it here (rather than
        // inside the former post-acquire `encode_frame`) is what lets the encode
        // stage run lock-free.
        let encode_sources = self.collect_encode_sources(scene, surf_w, surf_h);

        // ── Widget draw geometry (precomputed from the registry) ─────────────
        let widget_quads = self.collect_widget_draw_geometry(scene, sw, sh);

        // ── Keyboard focus ring (chrome layer, hud-k6yvb) ───────────────────
        let mut focus_ring_vertices: Vec<RectVertex> = Vec::new();
        self.append_focus_ring_vertices(scene, &mut focus_ring_vertices, sw, sh);
        // ── Composer caret quad (chrome layer, same pass, hud-hxhnt) ────────
        self.append_composer_caret_vertices(scene, &mut focus_ring_vertices, sw, sh);

        // ── Chrome context menu (hud-zc7f) ─────────────────────────────────
        let context_menu_vertices = self.collect_context_menu_vertices(scene, sw, sh);
        let safe_mode_vertices = self.safe_mode_overlay_vertices(sw, sh);
        let system_card_vertices = self.system_card_vertices(sw, sh);

        // Populate drag-handle hit regions from the geometry we are about to
        // present so the next input snapshot matches this frame (see the method
        // doc-comment for why this is unconditional now). Consumes `drag_handles`.
        self.populate_drag_handle_hit_regions_from(scene, drag_handles);

        WindowedFrameBuild {
            telemetry,
            surf_w,
            surf_h,
            vertices,
            bg_vertex_count,
            chrome_vertex_start,
            textured_cmds,
            encode_sources,
            drag_handle_vertices,
            drag_highlight_cmds,
            focus_ring_vertices,
            context_menu_vertices,
            safe_mode_vertices,
            system_card_vertices,
            widget_quads,
            frame_start,
        }
    }

    /// Present a previously-built frame to the surface, lock-free (hud-uyhpn).
    ///
    /// This is the GPU half of the windowed present path. It acquires the
    /// swapchain frame, encodes every pass from the owned [`WindowedFrameBuild`]
    /// (never touching `&SceneGraph`), submits, presents, and waits — all with
    /// the scene lock already released by the caller. Returns the completed
    /// per-frame telemetry.
    ///
    /// Skips the frame gracefully (returning early with total-time telemetry) if
    /// the surface is unavailable, and contains the hud-pi5wx submit/present
    /// panic so the compositor thread survives a mid-frame swapchain reconfigure.
    pub(crate) fn present_windowed_frame(
        &mut self,
        build: WindowedFrameBuild,
        surface: &dyn CompositorSurface,
    ) -> FrameTelemetry {
        self.present_windowed_frame_with_outcome(build, surface)
            .telemetry
    }

    /// Present a frame and report each completed GPU-facing operation
    /// independently for runtime efficiency accounting.
    pub(crate) fn present_windowed_frame_with_outcome(
        &mut self,
        build: WindowedFrameBuild,
        surface: &dyn CompositorSurface,
    ) -> WindowedPresentOutcome {
        let target = FrameTarget::primary(build.surf_w, build.surf_h);
        self.present_windowed_frame_to(&build, &target, surface)
    }

    /// Present one display window's view (`target`) of a built frame. The
    /// same build can be presented to several windows.
    pub fn present_windowed_frame_to(
        &mut self,
        build: &WindowedFrameBuild,
        target: &FrameTarget,
        surface: &dyn CompositorSurface,
    ) -> WindowedPresentOutcome {
        // Acquire frame through the surface trait (surface-agnostic).
        // The CompositorFrame._guard keeps the backing resource alive until drop.
        // Returns None when the swapchain is temporarily unavailable (double
        // failure) — skip this frame gracefully rather than panicking.
        let frame = match surface.acquire_frame() {
            Some(f) => f,
            None => {
                // Surface unavailable: skip render pass, return zeroed telemetry.
                // The runtime will retry on the next frame cycle.
                let mut telemetry = build.telemetry.clone();
                telemetry.frame_time_us = build.frame_start.elapsed().as_micros() as u64;
                return WindowedPresentOutcome {
                    telemetry,
                    surface_acquired: false,
                    gpu_submitted: false,
                };
            }
        };
        let surface_acquired = frame.acquisition.is_fresh();

        let (encoder, encode_us) = self.encode_windowed_passes(build, target, &frame.view);
        let frame_start = build.frame_start;
        let mut telemetry = build.telemetry.clone();
        telemetry.stage6_render_encode_us = encode_us;

        let submit_start = std::time::Instant::now();
        let cmd = encoder.finish();
        // hud-pi5wx Layer-1 resilience: a swapchain reconfigure (e.g. a resize) can
        // destroy the surface texture between acquire_frame() and queue.submit(), so
        // submit raises a wgpu validation error ("<Surface Texture> has been
        // destroyed") whose default uncaptured-error handler PANICS. On the compositor
        // thread that panic would kill the thread and freeze the whole HUD permanently
        // (FrameReadySignal never fires again). Contain it here so the thread survives;
        // the next acquire_frame() reacquires/reconfigures the surface. The underlying
        // acquire->submit-vs-reconfigure race is the separate Layer-2 fix.
        let submit_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.queue.submit(std::iter::once(cmd));
        }));
        let gpu_submitted = submit_result.is_ok();
        let present_result = submit_result.and_then(|_| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // present(): no-op for headless, swap-chain flip for windowed. `frame`
                // (the surface-texture guard) is still alive here; dropped just below.
                surface.present();
                self.device.poll(wgpu::Maintain::Wait);
            }))
        });
        drop(frame);
        telemetry.stage7_gpu_submit_us = submit_start.elapsed().as_micros().max(1) as u64;

        if present_result.is_err() {
            tracing::error!(
                "compositor: queue.submit/present panicked (surface texture likely \
                 destroyed mid-frame) — frame skipped, compositor thread preserved \
                 (hud-pi5wx)"
            );
            // Zero is the established skipped-stage sentinel. The windowed
            // runtime must not signal or benchmark-count a failed submission.
            telemetry.stage7_gpu_submit_us = 0;
            telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;
            return WindowedPresentOutcome {
                telemetry,
                surface_acquired,
                gpu_submitted,
            };
        }

        telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;
        WindowedPresentOutcome {
            telemetry,
            surface_acquired,
            gpu_submitted,
        }
    }

    /// Encode every pass of a built frame into `view`, in draw order, and
    /// return the encoder plus the encode time. Shared by the swapchain present
    /// path and the offscreen admin capture so both draw identical pixels.
    pub(super) fn encode_windowed_passes(
        &mut self,
        build: &WindowedFrameBuild,
        target: &FrameTarget,
        view: &wgpu::TextureView,
    ) -> (wgpu::CommandEncoder, u64) {
        let tv = self.target_view(build, target);
        let sw = tv.width as f32;
        let sh = tv.height as f32;

        // Glyphon prepare for this target (its viewport and pixel space), right
        // before the encode that replays it.
        let inputs = self.prepare_encode_inputs(
            tv.rr_background.into_owned(),
            tv.rr_content.into_owned(),
            tv.rr_post.into_owned(),
            &tv.text_items,
            tv.card_items,
            (tv.width, tv.height),
        );

        let (mut encoder, encode_us) = self.encode_from_inputs(
            &tv.vertices,
            view,
            &inputs,
            tv.width,
            tv.height,
            self.overlay_mode,
            build.bg_vertex_count,
            build.chrome_vertex_start,
        );

        // ── Image pass: draw textured quads on top of color geometry ─────────
        self.encode_image_pass(&mut encoder, view, &tv.textured_cmds, sw, sh);

        // ── Widget pass: composite pre-synced textures above zone content ────
        self.encode_widget_pass_prepared(&mut encoder, view, tv.widget_quads, sw, sh);
        self.encode_rounded_rect_pass(&mut encoder, view, &tv.drag_highlight_cmds, sw, sh);
        self.encode_drag_handle_pass(&mut encoder, view, &tv.drag_handle_vertices);

        // ── Keyboard focus ring (chrome layer, hud-k6yvb) ───────────────────
        // Drawn above all agent content (input-model §416) for the current focus
        // owner — node OR tile-level, any tile — via the same LoadOp::Load chrome
        // pass the drag handles use.
        if !tv.focus_ring_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, view, &tv.focus_ring_vertices);
        }

        // ── Chrome context menu (hud-zc7f) ─────────────────────────────────
        // Render the drag-handle reset context menu on top of everything.
        if !tv.context_menu_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, view, &tv.context_menu_vertices);
        }

        // ── Safe-mode overlay (hud-jm8nq.10): above all scene content and chrome.
        if !tv.safe_mode_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, view, &tv.safe_mode_vertices);
        }

        // ── System card (hud-w5zon): the last pass, above the safe-mode overlay,
        // so the pairing code can never be covered.
        self.encode_system_card_pass(&mut encoder, view, tv.system_card_vertices, &inputs);

        (encoder, encode_us)
    }

    /// Map a canvas-space build onto one display window (see
    /// [`crate::display`]). The primary window borrows the build unchanged;
    /// any other window gets translated copies, its own safe-mode overlay, and
    /// none of the primary-only chrome (widgets clamp to the canvas; the
    /// system card belongs to the primary display).
    fn target_view<'a>(
        &self,
        build: &'a WindowedFrameBuild,
        target: &FrameTarget,
    ) -> TargetView<'a> {
        if target.primary && target.is_canvas(build.surf_w, build.surf_h) {
            return TargetView {
                width: build.surf_w,
                height: build.surf_h,
                vertices: Cow::Borrowed(&build.vertices),
                textured_cmds: Cow::Borrowed(&build.textured_cmds),
                rr_background: Cow::Borrowed(&build.encode_sources.rr_background),
                rr_content: Cow::Borrowed(&build.encode_sources.rr_content),
                rr_post: Cow::Borrowed(&build.encode_sources.rr_post),
                text_items: Cow::Borrowed(&build.encode_sources.text_items),
                card_items: &build.encode_sources.card_items,
                drag_handle_vertices: Cow::Borrowed(&build.drag_handle_vertices),
                drag_highlight_cmds: Cow::Borrowed(&build.drag_highlight_cmds),
                focus_ring_vertices: Cow::Borrowed(&build.focus_ring_vertices),
                context_menu_vertices: Cow::Borrowed(&build.context_menu_vertices),
                safe_mode_vertices: Cow::Borrowed(&build.safe_mode_vertices),
                system_card_vertices: &build.system_card_vertices,
                widget_quads: &build.widget_quads,
            };
        }
        let (tw, th) = (target.width.max(1), target.height.max(1));
        let affine = target.ndc_affine(build.surf_w, build.surf_h);
        let map = |vertices: &[RectVertex]| -> Vec<RectVertex> {
            vertices
                .iter()
                .map(|v| RectVertex {
                    position: affine.apply(v.position),
                    color: v.color,
                })
                .collect()
        };
        let mut vertices = map(&build.vertices);
        if self.overlay_mode && vertices.len() >= 6 {
            // The leading alpha-zeroing quad must cover this whole window, not
            // the canvas rect mapped into it.
            let clear = rect_vertices(
                0.0, 0.0, tw as f32, th as f32, tw as f32, th as f32, [0.0; 4],
            );
            vertices[..6].copy_from_slice(&clear);
        }
        let (dx, dy) = (-target.x, -target.y);
        let shift_rr = |cmds: &[RoundedRectDrawCmd]| -> Vec<RoundedRectDrawCmd> {
            cmds.iter()
                .map(|cmd| {
                    let mut cmd = cmd.clone();
                    cmd.x += dx;
                    cmd.y += dy;
                    if let Some(clip) = cmd.clip.as_mut() {
                        clip.x += dx;
                        clip.y += dy;
                    }
                    cmd
                })
                .collect()
        };
        let text_items = build
            .encode_sources
            .text_items
            .iter()
            .map(|item| {
                let mut item = item.clone();
                item.pixel_x += dx;
                item.pixel_y += dy;
                item.clip_pixel_x += dx;
                item.clip_pixel_y += dy;
                item
            })
            .collect();
        let textured_cmds = build
            .textured_cmds
            .iter()
            .map(|cmd| {
                let mut cmd = cmd.clone();
                cmd.x += dx;
                cmd.y += dy;
                cmd
            })
            .collect();
        let primary_only = |s: &'a [RectVertex]| if target.primary { s } else { &[] };
        TargetView {
            width: tw,
            height: th,
            vertices: Cow::Owned(vertices),
            textured_cmds: Cow::Owned(textured_cmds),
            rr_background: Cow::Owned(shift_rr(&build.encode_sources.rr_background)),
            rr_content: Cow::Owned(shift_rr(&build.encode_sources.rr_content)),
            rr_post: Cow::Owned(shift_rr(&build.encode_sources.rr_post)),
            text_items: Cow::Owned(text_items),
            card_items: if target.primary {
                &build.encode_sources.card_items
            } else {
                &[]
            },
            drag_handle_vertices: Cow::Owned(map(&build.drag_handle_vertices)),
            drag_highlight_cmds: Cow::Owned(shift_rr(&build.drag_highlight_cmds)),
            focus_ring_vertices: Cow::Owned(map(&build.focus_ring_vertices)),
            context_menu_vertices: Cow::Owned(map(&build.context_menu_vertices)),
            safe_mode_vertices: Cow::Owned(self.safe_mode_overlay_vertices(tw as f32, th as f32)),
            system_card_vertices: primary_only(&build.system_card_vertices),
            widget_quads: if target.primary {
                &build.widget_quads
            } else {
                &[]
            },
        }
    }

    /// Fingerprint of what `target` would show for `build`: only content that
    /// lands inside the window counts, so a change elsewhere in the scene (a
    /// portal on another display) leaves it unchanged and that window need not
    /// present. Used for non-primary windows; the primary presents every
    /// rendered frame.
    pub fn frame_signature(&self, build: &WindowedFrameBuild, target: &FrameTarget) -> u64 {
        use std::hash::{Hash, Hasher};
        let tv = self.target_view(build, target);
        let (w, h) = (tv.width as f32, tv.height as f32);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (tv.width, tv.height, self.overlay_mode).hash(&mut hasher);
        (self.degradation_level as u8).hash(&mut hasher);
        let bg = build.bg_vertex_count.min(tv.vertices.len());
        hash_visible_triangles(&mut hasher, &tv.vertices[..bg]);
        0xB6u8.hash(&mut hasher); // background / rest split
        let chrome = build.chrome_vertex_start.clamp(bg, tv.vertices.len());
        hash_visible_triangles(&mut hasher, &tv.vertices[bg..chrome]);
        0xC6u8.hash(&mut hasher); // Content SDF / Chrome flat split
        hash_visible_triangles(&mut hasher, &tv.vertices[chrome..]);
        for list in [
            &tv.drag_handle_vertices,
            &tv.focus_ring_vertices,
            &tv.context_menu_vertices,
            &tv.safe_mode_vertices,
        ] {
            0xC7u8.hash(&mut hasher);
            hash_visible_triangles(&mut hasher, list);
        }
        let visible =
            |x: f32, y: f32, cw: f32, ch: f32| x < w && y < h && x + cw > 0.0 && y + ch > 0.0;
        // Destructure exhaustively: a new draw-command field must be hashed
        // (or explicitly ignored) here, or a window would keep stale pixels.
        for cmd in tv.textured_cmds.iter() {
            let TexturedDrawCmd {
                resource_id,
                x,
                y,
                w: cw,
                h: ch,
                uv_rect,
                tint,
            } = cmd;
            if !visible(*x, *y, *cw, *ch) {
                continue;
            }
            resource_id.hash(&mut hasher);
            hash_f32s(&mut hasher, &[*x, *y, *cw, *ch]);
            hash_f32s(&mut hasher, uv_rect);
            hash_f32s(&mut hasher, tint);
        }
        for list in [
            &tv.rr_background,
            &tv.rr_content,
            &tv.rr_post,
            &tv.drag_highlight_cmds,
        ] {
            0xD8u8.hash(&mut hasher);
            for cmd in list.iter() {
                let RoundedRectDrawCmd {
                    x,
                    y,
                    width,
                    height,
                    radius,
                    color,
                    border,
                    clip,
                } = cmd;
                if !visible(*x, *y, *width, *height) {
                    continue;
                }
                hash_f32s(&mut hasher, &[*x, *y, *width, *height, *radius]);
                hash_f32s(&mut hasher, color);
                border.is_some().hash(&mut hasher);
                if let Some(RoundedRectBorder { width, color }) = border {
                    hash_f32s(&mut hasher, &[*width]);
                    hash_f32s(&mut hasher, color);
                }
                clip.is_some().hash(&mut hasher);
                if let Some(RoundedRectClip {
                    x,
                    y,
                    width,
                    height,
                }) = clip
                {
                    hash_f32s(&mut hasher, &[*x, *y, *width, *height]);
                }
            }
        }
        for item in tv.text_items.iter().filter(|i| {
            visible(
                i.clip_pixel_x,
                i.clip_pixel_y,
                i.clip_bounds_width,
                i.clip_bounds_height,
            )
        }) {
            // Every TextItem field affects pixels; Debug covers them all.
            format!("{item:?}").hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Render one frame of the scene to the surface (single-lock convenience).
    ///
    /// This is a thin wrapper that runs [`Compositor::build_windowed_frame`]
    /// immediately followed by [`Compositor::present_windowed_frame`]. The
    /// production windowed loop does NOT use this wrapper — it calls the two
    /// halves separately so it can drop the scene lock in between (hud-uyhpn).
    /// Retained for tests and any caller that holds the scene throughout.
    ///
    /// This method is surface-agnostic: it works with any type implementing
    /// `CompositorSurface`.  The same code path executes in headless and windowed
    /// modes — only the surface implementation differs.
    ///
    /// Per runtime-kernel/spec.md Requirement: Headless Mode (line 198):
    /// "No conditional compilation for the render path."
    ///
    /// For headless pixel readback, use `render_frame_headless()` instead,
    /// which includes the `copy_to_buffer` step internally so that
    /// `surface.read_pixels()` returns the current frame's data.
    /// `render_frame()` does NOT copy pixels to the readback buffer — the
    /// encoder is created and consumed internally and is not exposed.
    ///
    /// Returns telemetry for this frame.
    pub fn render_frame(
        &mut self,
        scene: &mut SceneGraph,
        surface: &dyn CompositorSurface,
    ) -> FrameTelemetry {
        let (surf_w, surf_h) = surface.size();
        let build = self.build_windowed_frame(scene, surf_w, surf_h);
        self.present_windowed_frame(build, surface)
    }

    /// Render one frame and copy pixel data into the headless readback buffer.
    ///
    /// This is a convenience method for testing/CI that handles the extra
    /// `copy_to_buffer` step required for headless pixel readback.
    ///
    /// `copy_to_buffer` is appended to the encoder before `queue.submit()` via
    /// the shared `encode_frame` helper, which returns the encoder prior to
    /// submission so that this headless-specific step can be inserted cleanly.
    ///
    /// Returns telemetry for this frame.
    pub fn render_frame_headless(
        &mut self,
        scene: &mut SceneGraph,
        surface: &HeadlessSurface,
    ) -> FrameTelemetry {
        self.render_frame_headless_with_submission(scene, surface).0
    }

    /// Render through the same headless pipeline and report whether it submitted.
    ///
    /// This is an actual queue outcome, independent of microsecond rounding.
    /// A local headless submit is not a windowed surface present or scanout.
    pub fn render_frame_headless_with_submission(
        &mut self,
        scene: &mut SceneGraph,
        surface: &HeadlessSurface,
    ) -> (FrameTelemetry, bool) {
        #[cfg(any(test, feature = "dev-mode"))]
        self.begin_headless_work_observation();
        let frame_start = std::time::Instant::now();
        self.frame_number += 1;

        // ── Drain local composer echo state (hud-r3ax6) ───────────────────
        self.drain_local_composer_state();

        let mut telemetry = FrameTelemetry::new(self.frame_number);

        // ── Phase-1 markdown cache prime (hud-380dl: commit-time prime) ─────
        // The markdown cache MUST be primed at commit time (before
        // render_frame_headless is called) by an explicit `prime_markdown_cache`
        // call at the scene-commit site (Stage 3/4).  By the time this path
        // executes, the cache is already populated and this block is a no-op.
        //
        // Safety fallback: identical to render_frame — see that method for the
        // full correctness rationale.  In steady state this path is never taken.
        // `markdown_prime_us` stays 0 on all commit-primed frames.
        if scene.version != self.markdown_cache_scene_version {
            debug_assert!(
                false,
                "render_frame_headless: markdown cache was not commit-primed for scene \
                 version {} (cache version {}); falling back to in-render prime [hud-380dl]",
                scene.version, self.markdown_cache_scene_version,
            );
            self.prime_markdown_cache(scene);
            // Note: markdown_prime_us stays 0 (correctness fallback, not normal path).
        }

        // ── Phase-1 truncation cache prime (hud-wgq7j / hud-v2z6u) ─────────────
        // Same commit-time prime contract as the markdown cache — see render_frame
        // for the full correctness rationale.  In steady state this block is a
        // no-op because the caller already primed at commit time.
        if scene.version != self.truncation_cache_scene_version {
            // A version mismatch is an EXPECTED state: prime_truncation_cache's
            // mid-drag cadence gate (hud-ghhxa) defers re-primes during fast
            // geometry changes, so the sentinel legitimately lags scene.version
            // for one or more frames.  Trace (not debug_assert) and apply the
            // cadence-gated in-render prime to preserve correctness [hud-v2z6u].
            tracing::trace!(
                scene_version = scene.version,
                cache_version = self.truncation_cache_scene_version,
                "render_frame_headless: truncation cache lags scene; applying \
                 cadence-gated in-render prime"
            );
            self.prime_truncation_cache(scene);
        }

        // The retained validation lane is intentionally narrower than normal
        // rendering: only its exact fifty-tile, direct-text scenario may retain
        // the previously submitted surface. Every other scene falls through to
        // the established full-frame path below.
        if let Some(retained_telemetry) = self.try_render_retained_headless(scene, surface) {
            // The retained delegate returns Some only after its real queue submission.
            return (retained_telemetry, true);
        }

        // Build the shared per-frame geometry (Background → tiles → Content →
        // Chrome zones) via the single source of truth shared with the windowed
        // and chrome render paths. Uses surface.size() — not self.width/height —
        // so vertex normalization is correct even if the HeadlessSurface was
        // created with different dimensions than the compositor's stored size.
        let (surf_w, surf_h) = surface.size();
        let sw = surf_w as f32;
        let sh = surf_h as f32;
        let (vertices, textured_cmds, bg_vertex_count, chrome_vertex_start) =
            self.build_frame_vertices(scene, sw, sh, &mut telemetry);

        // Collect drag handle entries once and reuse for both rendering and hit-region
        // population, avoiding a redundant second traversal at the end of the frame.
        let drag_handles = self.collect_drag_handle_entries(scene, sw, sh);
        let mut drag_handle_vertices: Vec<RectVertex> = Vec::new();
        self.append_drag_handle_vertices(scene, &drag_handles, &mut drag_handle_vertices, sw, sh);
        let drag_highlight_cmds = self.drag_highlight_cmds(scene, &drag_handles);

        // ── Widget texture sync before frame acquisition (same as windowed path).
        self.sync_widget_textures(scene, self.degradation_level);
        telemetry.widget_rasterized = self
            .widget_renderer
            .as_ref()
            .map(|wr| wr.rasterized_last_sync().to_vec())
            .unwrap_or_default();

        // Acquire frame via trait — same code path as render_frame().
        // HeadlessSurface never returns None, but we handle it for API
        // consistency and future-proofing. See WindowSurface for the case
        // where None signals a double swapchain-acquire failure.
        let frame = match surface.acquire_frame() {
            Some(f) => f,
            None => {
                // Surface unavailable: skip render pass, return zeroed telemetry.
                telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;
                return (telemetry, false);
            }
        };

        // Headless never uses overlay mode — pass false for the pipeline selector.
        let (mut encoder, encode_us, encode_inputs) = self.encode_frame(
            &vertices,
            &frame.view,
            scene,
            surf_w,
            surf_h,
            false,
            bg_vertex_count,
            chrome_vertex_start,
        );
        telemetry.stage6_render_encode_us = encode_us;

        // ── Image pass: draw textured quads on top of color geometry ─────────
        self.encode_image_pass(&mut encoder, &frame.view, &textured_cmds, sw, sh);

        // ── Widget pass: composite pre-synced textures above zone content ────
        self.encode_widget_pass(&mut encoder, &frame.view, &scene.widget_registry, sw, sh);
        self.encode_rounded_rect_pass(&mut encoder, &frame.view, &drag_highlight_cmds, sw, sh);
        self.encode_drag_handle_pass(&mut encoder, &frame.view, &drag_handle_vertices);

        // ── Keyboard focus ring (chrome layer, hud-k6yvb) ───────────────────
        let mut focus_ring_vertices: Vec<RectVertex> = Vec::new();
        self.append_focus_ring_vertices(scene, &mut focus_ring_vertices, sw, sh);
        // ── Composer caret quad (chrome layer, same pass, hud-hxhnt) ────────
        self.append_composer_caret_vertices(scene, &mut focus_ring_vertices, sw, sh);
        if !focus_ring_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, &frame.view, &focus_ring_vertices);
        }

        // ── Chrome context menu (hud-zc7f) ─────────────────────────────────
        let context_menu_vertices = self.collect_context_menu_vertices(scene, sw, sh);
        if !context_menu_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, &frame.view, &context_menu_vertices);
        }

        // System card last, above everything (hud-w5zon).
        let system_card_vertices = self.system_card_vertices(sw, sh);
        self.encode_system_card_pass(
            &mut encoder,
            &frame.view,
            &system_card_vertices,
            &encode_inputs,
        );

        // Headless-specific: copy rendered texture to readback buffer.
        // Must happen after all render passes and before submit.
        surface.copy_to_buffer(&mut encoder);

        let submit_start = std::time::Instant::now();
        self.queue.submit(std::iter::once(encoder.finish()));
        surface.present(); // no-op for headless
        drop(frame);
        self.device.poll(wgpu::Maintain::Wait);
        telemetry.stage7_gpu_submit_us = submit_start.elapsed().as_micros() as u64;

        telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;

        // Populate zone interaction hit regions for the next frame's hit-testing.
        // Must run after rendering so the region geometry is consistent with what
        // was just displayed.  This follows the snapshot-based design: regions
        // computed from the rendered geometry are used for hit-testing on the
        // next input event.
        self.populate_zone_hit_regions(scene, sw, sh);
        // Reuse the pre-computed drag_handles list rather than collecting again.
        self.populate_drag_handle_hit_regions_from(scene, drag_handles);

        // Seed the private retained snapshot only after this real full frame
        // completed its submit/readback tail. Structured fallback diagnostics
        // (resize and modeled device recovery) are captured here as non-passing
        // evidence.
        self.observe_full_headless_frame(scene, surface);

        (telemetry, true)
    }
}
