use super::*;

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
    /// offset just past the Background zones (for the split flat-rect pass).
    vertices: Vec<RectVertex>,
    bg_vertex_count: usize,
    /// Textured image draw commands (composited above the color geometry).
    textured_cmds: Vec<TexturedDrawCmd>,
    /// Scene-free encode inputs (rounded-rect cmds + prepared text).
    encode_inputs: EncodeInputs,
    /// Precomputed drag-handle chrome vertices.
    drag_handle_vertices: Vec<RectVertex>,
    /// Precomputed keyboard focus-ring chrome vertices.
    focus_ring_vertices: Vec<RectVertex>,
    /// Precomputed drag-handle reset context-menu chrome vertices.
    context_menu_vertices: Vec<RectVertex>,
    /// Safe-mode overlay quads (empty unless safe mode is active); drawn last.
    safe_mode_vertices: Vec<RectVertex>,
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
    /// Single source of truth for the scene→geometry stage that was previously
    /// triplicated across `render_frame`, `render_frame_headless`, and
    /// `render_frame_with_chrome` (hud-8uafa). It covers the canonical layer
    /// ordering (Background zones → tiles → Content zones → Chrome zones), the
    /// per-tile background / content / scroll-indicator / composer-overlay
    /// geometry, and the overlay-mode alpha-zeroing quad. The three public render
    /// entry points keep their distinct encode tails (windowed present, headless
    /// readback + hit regions, chrome two-pass) and consume this shared body.
    ///
    /// All colors flow through [`gpu_color_raw`], which is identity outside
    /// overlay mode and applies the sRGB-premultiply transform inside it. This is
    /// correct for every call site: only the windowed path is ever in overlay
    /// mode, so the headless and chrome paths see the raw color unchanged — which
    /// is exactly what their hand-written predecessors did. (Unifying here also
    /// removes the previously-latent divergence where the windowed tile
    /// background went through `gpu_color_raw` but the headless/chrome copies did
    /// not — benign only because headless never set overlay mode.)
    ///
    /// Returns `(vertices, textured_cmds, bg_vertex_count)`, where
    /// `bg_vertex_count` is the vertex offset just after the Background-layer
    /// flat-rect zones — used by the caller to split the flat-rect pass so the
    /// Background SDF pass can be interleaved. Also populates the `tile_count`,
    /// `node_count`, and `active_leases` telemetry fields.
    fn build_frame_vertices(
        &mut self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
        telemetry: &mut FrameTelemetry,
    ) -> (Vec<RectVertex>, Vec<TexturedDrawCmd>, usize) {
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
        self.update_zone_animations(scene);
        // §6.3 portal transition: advance per-portal-tile fade animations
        // alongside zone animations (hud-58rg1). Folded in here so all three
        // render entry points share the single update site.
        self.update_portal_tile_animations(scene);
        // Smooth scroll / animated follow-tail (hud-bq0gl.10): advance the
        // per-portal-tile scroll smoothers once per frame, BEFORE the tile loop
        // and the later text/encode passes read displayed offsets via
        // `display_tile_scroll_offset`. No-op (snap) in headless mode.
        self.update_scroll_smoothing(scene);

        // ── Layer ordering: Background → Tiles → Content zones → Chrome zones ─
        // Background zones render first so agent tiles occlude them.
        self.render_zone_content(
            scene,
            &mut vertices,
            &mut textured_cmds,
            sw,
            sh,
            Some(LayerAttachment::Background),
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
        self.update_zone_animations(scene);
        // §6.3 portal transition: advance per-portal-tile fade animations
        // alongside zone animations (hud-58rg1).
        self.update_portal_tile_animations(scene);

        // Update streaming word-by-word reveal state.
        self.update_stream_reveals(scene);

        // Update per-portal-tile streaming-reveal fade state (hud-bl7yi): fades
        // newly-appended portal-tile content in segment-by-segment instead of
        // snapping. Mirrors the zone reveal above for the portal-tile path.
        self.update_portal_tile_reveals(scene);

        // Content zones render as a batch after all tiles (above background, below chrome).
        self.render_zone_content(
            scene,
            &mut vertices,
            &mut textured_cmds,
            sw,
            sh,
            Some(LayerAttachment::Content),
        );
        // Chrome zones render last, above everything.
        self.render_zone_content(
            scene,
            &mut vertices,
            &mut textured_cmds,
            sw,
            sh,
            Some(LayerAttachment::Chrome),
        );

        (vertices, textured_cmds, bg_vertex_count)
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
        let (vertices, textured_cmds, bg_vertex_count) =
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
        let encode_inputs = self.collect_encode_inputs(scene, surf_w, surf_h);

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
            textured_cmds,
            encode_inputs,
            drag_handle_vertices,
            focus_ring_vertices,
            context_menu_vertices,
            safe_mode_vertices,
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
    pub fn present_windowed_frame(
        &mut self,
        build: WindowedFrameBuild,
        surface: &dyn CompositorSurface,
    ) -> FrameTelemetry {
        self.present_windowed_frame_with_outcome(build, surface)
            .telemetry
    }

    /// Present a frame and report each completed GPU-facing operation
    /// independently for runtime efficiency accounting.
    pub fn present_windowed_frame_with_outcome(
        &mut self,
        build: WindowedFrameBuild,
        surface: &dyn CompositorSurface,
    ) -> WindowedPresentOutcome {
        let WindowedFrameBuild {
            mut telemetry,
            surf_w,
            surf_h,
            vertices,
            bg_vertex_count,
            textured_cmds,
            encode_inputs,
            drag_handle_vertices,
            focus_ring_vertices,
            context_menu_vertices,
            safe_mode_vertices,
            widget_quads,
            frame_start,
        } = build;

        let sw = surf_w as f32;
        let sh = surf_h as f32;

        // Acquire frame through the surface trait (surface-agnostic).
        // The CompositorFrame._guard keeps the backing resource alive until drop.
        // Returns None when the swapchain is temporarily unavailable (double
        // failure) — skip this frame gracefully rather than panicking.
        let frame = match surface.acquire_frame() {
            Some(f) => f,
            None => {
                // Surface unavailable: skip render pass, return zeroed telemetry.
                // The runtime will retry on the next frame cycle.
                telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;
                return WindowedPresentOutcome {
                    telemetry,
                    surface_acquired: false,
                    gpu_submitted: false,
                };
            }
        };
        let surface_acquired = frame.acquisition.is_fresh();

        let (mut encoder, encode_us) = self.encode_from_inputs(
            &vertices,
            &frame.view,
            &encode_inputs,
            surf_w,
            surf_h,
            self.overlay_mode,
            bg_vertex_count,
        );
        telemetry.stage6_render_encode_us = encode_us;

        // ── Image pass: draw textured quads on top of color geometry ─────────
        self.encode_image_pass(&mut encoder, &frame.view, &textured_cmds, sw, sh);

        // ── Widget pass: composite pre-synced textures above zone content ────
        self.encode_widget_pass_prepared(&mut encoder, &frame.view, &widget_quads, sw, sh);
        self.encode_drag_handle_pass(&mut encoder, &frame.view, &drag_handle_vertices);

        // ── Keyboard focus ring (chrome layer, hud-k6yvb) ───────────────────
        // Drawn above all agent content (input-model §416) for the current focus
        // owner — node OR tile-level, any tile — via the same LoadOp::Load chrome
        // pass the drag handles use.
        if !focus_ring_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, &frame.view, &focus_ring_vertices);
        }

        // ── Chrome context menu (hud-zc7f) ─────────────────────────────────
        // Render the drag-handle reset context menu on top of everything.
        if !context_menu_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, &frame.view, &context_menu_vertices);
        }

        // ── Safe-mode overlay (hud-jm8nq.10): above everything, including chrome.
        if !safe_mode_vertices.is_empty() {
            self.encode_drag_handle_pass(&mut encoder, &frame.view, &safe_mode_vertices);
        }

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
            return retained_telemetry;
        }

        // Build the shared per-frame geometry (Background → tiles → Content →
        // Chrome zones) via the single source of truth shared with the windowed
        // and chrome render paths. Uses surface.size() — not self.width/height —
        // so vertex normalization is correct even if the HeadlessSurface was
        // created with different dimensions than the compositor's stored size.
        let (surf_w, surf_h) = surface.size();
        let sw = surf_w as f32;
        let sh = surf_h as f32;
        let (vertices, textured_cmds, bg_vertex_count) =
            self.build_frame_vertices(scene, sw, sh, &mut telemetry);

        // Collect drag handle entries once and reuse for both rendering and hit-region
        // population, avoiding a redundant second traversal at the end of the frame.
        let drag_handles = self.collect_drag_handle_entries(scene, sw, sh);
        let mut drag_handle_vertices: Vec<RectVertex> = Vec::new();
        self.append_drag_handle_vertices(scene, &drag_handles, &mut drag_handle_vertices, sw, sh);

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
                return telemetry;
            }
        };

        // Headless never uses overlay mode — pass false for the pipeline selector.
        let (mut encoder, encode_us) = self.encode_frame(
            &vertices,
            &frame.view,
            scene,
            surf_w,
            surf_h,
            false,
            bg_vertex_count,
        );
        telemetry.stage6_render_encode_us = encode_us;

        // ── Image pass: draw textured quads on top of color geometry ─────────
        self.encode_image_pass(&mut encoder, &frame.view, &textured_cmds, sw, sh);

        // ── Widget pass: composite pre-synced textures above zone content ────
        self.encode_widget_pass(&mut encoder, &frame.view, &scene.widget_registry, sw, sh);
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

        telemetry
    }

    /// Render one frame with a separate chrome pass after the content pass.
    ///
    /// # Layer ordering (back to front)
    ///
    /// 1. **Content pass** — background clear + all agent tiles. Uses `LoadOp::Clear`.
    /// 2. **Chrome pass** — draws chrome draw commands on top. Uses `LoadOp::Load` to
    ///    preserve the content pass pixels, ensuring chrome is always on top.
    ///
    /// Chrome draw commands are produced by the caller (from `ChromeRenderer::render_chrome`)
    /// before calling this function. The compositor does not access `ChromeState` directly —
    /// chrome state is fully decoupled from scene/agent state.
    ///
    /// # Separable passes
    ///
    /// The content pass and chrome pass are encoded as two separate `RenderPass` blocks
    /// within the same `CommandEncoder`. This architectural separation is the foundation
    /// for future render-skip redaction (capture-safe architecture): the content pass can
    /// be suppressed independently of the chrome pass.
    ///
    /// # Chrome layer sovereignty
    ///
    /// Because the chrome pass uses `LoadOp::Load` and runs after the content pass,
    /// chrome pixels are always written last — no agent tile can occlude chrome regardless
    /// of its z-order value.
    pub fn render_frame_with_chrome(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
        chrome_cmds: &[ChromeDrawCmd],
    ) -> FrameTelemetry {
        let frame_start = std::time::Instant::now();
        self.frame_number += 1;

        // ── Drain local composer echo state (hud-r3ax6) ───────────────────
        self.drain_local_composer_state();

        let mut telemetry = FrameTelemetry::new(self.frame_number);

        // ── Phase-1 markdown cache prime (hud-380dl) ──────────────────────────
        // Safety fallback — mirrors render_frame / render_frame_headless.
        // In steady state the cache is already primed at commit time and this
        // block is a no-op.
        if scene.version != self.markdown_cache_scene_version {
            debug_assert!(
                false,
                "render_frame_with_chrome: markdown cache was not commit-primed for scene \
                 version {} (cache version {}); falling back to in-render prime [hud-380dl]",
                scene.version, self.markdown_cache_scene_version,
            );
            self.prime_markdown_cache(scene);
        }

        // ── Phase-1 truncation cache prime (hud-wgq7j / hud-v2z6u) ─────────────
        // Safety fallback — mirrors render_frame / render_frame_headless.
        // In steady state the cache is primed at commit time; this is a no-op.
        // A version mismatch is an EXPECTED state because prime_truncation_cache's
        // mid-drag cadence gate (hud-ghhxa) defers re-primes during fast geometry
        // changes — so we trace rather than debug_assert [hud-v2z6u].
        if scene.version != self.truncation_cache_scene_version {
            tracing::trace!(
                scene_version = scene.version,
                cache_version = self.truncation_cache_scene_version,
                "render_frame_with_chrome: truncation cache lags scene; applying \
                 cadence-gated in-render prime"
            );
            self.prime_truncation_cache(scene);
        }

        // ── Widget texture sync before encoding (avoids surface-texture race).
        self.sync_widget_textures(scene, self.degradation_level);
        telemetry.widget_rasterized = self
            .widget_renderer
            .as_ref()
            .map(|wr| wr.rasterized_last_sync().to_vec())
            .unwrap_or_default();

        // Build the shared per-frame content geometry (Background → tiles →
        // Content → Chrome zones). `build_frame_vertices` is the single source of
        // truth shared with the windowed and headless render paths; here its
        // output becomes the *content* pass, with chrome composited as a separate
        // GPU pass below for capture-safe layer sovereignty. (sync_widget_textures
        // and image-texture upload already ran above, before encoding begins.)
        let (surf_w, surf_h) = surface.size();
        let sw = surf_w as f32;
        let sh = surf_h as f32;
        let (content_vertices, textured_cmds, bg_vertex_count) =
            self.build_frame_vertices(scene, sw, sh, &mut telemetry);

        let encode_start = std::time::Instant::now();

        let content_buffer = if content_vertices.is_empty() {
            None
        } else {
            let buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("content_vertex_buffer"),
                    contents: bytemuck::cast_slice(&content_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            Some(buf)
        };

        // ── Pass 2: Chrome ───────────────────────────────────────────────────
        let mut chrome_vertices: Vec<RectVertex> = Vec::new();
        for cmd in chrome_cmds {
            let verts = rect_vertices(cmd.x, cmd.y, cmd.width, cmd.height, sw, sh, cmd.color);
            chrome_vertices.extend_from_slice(&verts);
        }
        let drag_handles = self.collect_drag_handle_entries(scene, sw, sh);
        self.append_drag_handle_vertices(scene, &drag_handles, &mut chrome_vertices, sw, sh);
        // Keyboard focus ring in the chrome pass, above all content (hud-k6yvb).
        self.append_focus_ring_vertices(scene, &mut chrome_vertices, sw, sh);
        // Composer caret quad, same chrome pass (hud-hxhnt).
        self.append_composer_caret_vertices(scene, &mut chrome_vertices, sw, sh);

        let chrome_buffer = if chrome_vertices.is_empty() {
            None
        } else {
            let buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("chrome_vertex_buffer"),
                    contents: bytemuck::cast_slice(&chrome_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            Some(buf)
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame_encoder_with_chrome"),
            });

        // ── Content pass 1: Background flat-rect zones ────────────────────────
        // Clears the surface and draws Background-layer zone backdrops (those
        // without backdrop_radius).  Tile/content/chrome vertices are deferred
        // to pass 2 so that the Background SDF pass can be interleaved.
        {
            let mut content_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("content_pass_bg"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if self.use_opaque_rect_pipeline() {
                content_pass.set_pipeline(&self.clear_pipeline);
            } else {
                content_pass.set_pipeline(&self.pipeline);
            }
            if let Some(ref buf) = content_buffer {
                let bg_end = bg_vertex_count.min(content_vertices.len());
                if bg_end > 0 {
                    content_pass.set_vertex_buffer(0, buf.slice(..));
                    content_pass.draw(0..bg_end as u32, 0..1);
                }
            }
        }

        // ── Single-pass partition of all rounded-rect commands ────────────────
        // Collect once; reuse the partitioned results in both SDF passes below.
        let rr_all = self.collect_all_rounded_rect_cmds(scene, sw, sh);

        // ── Background SDF rounded-rect pass ─────────────────────────────────
        // Runs after the Clear pass so Background backdrops are below tiles.
        self.encode_rounded_rect_pass(&mut encoder, &surface.view, &rr_all.background, sw, sh);

        // ── Content pass 2: Tiles + Content + Chrome flat-rect zones ──────────
        // Uses LoadOp::Load to preserve Background geometry drawn above.
        {
            let mut content_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("content_pass_tiles_content_chrome"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if self.use_opaque_rect_pipeline() {
                content_pass.set_pipeline(&self.clear_pipeline);
            } else {
                content_pass.set_pipeline(&self.pipeline);
            }
            if let Some(ref buf) = content_buffer {
                let bg_end = bg_vertex_count.min(content_vertices.len());
                let rest_count = content_vertices.len().saturating_sub(bg_end);
                if rest_count > 0 {
                    content_pass.set_vertex_buffer(0, buf.slice(..));
                    content_pass.draw(bg_end as u32..content_vertices.len() as u32, 0..1);
                }
            }
        }

        // ── Content + Chrome SDF rounded-rect pass ────────────────────────────
        // Runs after tiles so Content/Chrome backdrops composite above tiles.
        {
            let mut rr_post: Vec<crate::pipeline::RoundedRectDrawCmd> = Vec::new();
            rr_post.extend(rr_all.content);
            rr_post.extend(rr_all.chrome);
            rr_post.extend(self.collect_tile_rounded_rect_cmds(scene));
            self.encode_rounded_rect_pass(&mut encoder, &surface.view, &rr_post, sw, sh);
        }

        // ── Stage 6: Text pass (between content and chrome) ──────────────────
        // Composer, viewer-echo, and vertical-flow layout were resolved by the
        // shared frame builder before it emitted flat geometry, so this text pass
        // consumes the exact same same-frame state as the chrome above.
        // Text is content — rendered above geometry rectangles but below chrome.
        let text_items_chrome: Vec<TextItem> = if self.text_rasterizer.is_some() {
            self.collect_text_items(scene, sw, sh)
        } else {
            vec![]
        };
        // Phase A: prepare glyphon buffers (requires mutable tr borrow).
        // Drop the borrow immediately so Phase B can call self.gpu_color_raw.
        let chrome_prepare_result: Option<Result<Vec<crate::text::InlineBackdropQuad>, String>> = {
            let (chrome_surf_w, chrome_surf_h) = (self.width, self.height);
            if let Some(ref mut tr) = self.text_rasterizer {
                tr.update_viewport(&self.queue, chrome_surf_w, chrome_surf_h);
                // When text_items_chrome is empty we return None — not Some(Ok([])) —
                // so Phase C skips render_text_pass entirely.  Calling render_text_pass
                // without a preceding prepare would replay glyphon's previously prepared
                // TextAreas from the last non-empty frame, causing stale text to linger.
                if text_items_chrome.is_empty() {
                    None
                } else {
                    Some(tr.prepare_text_items(&self.device, &self.queue, &text_items_chrome))
                }
            } else {
                None
            }
        };
        // Phase B: convert inline backdrop quads to GPU vertices.
        // `self` is no longer mutably borrowed here.
        let chrome_inline_verts: Vec<crate::pipeline::RectVertex> =
            if let Some(Ok(ref inline_quads)) = chrome_prepare_result {
                inline_quads
                    .iter()
                    .flat_map(|q| {
                        let color = self.gpu_color_raw([
                            q.color[0] as f32 / 255.0,
                            q.color[1] as f32 / 255.0,
                            q.color[2] as f32 / 255.0,
                            q.color[3] as f32 / 255.0,
                        ]);
                        rect_vertices(q.x, q.y, q.w, q.h, sw, sh, color)
                    })
                    .collect()
            } else {
                vec![]
            };
        // Phase C: render passes (re-takes tr mutable borrow).
        let use_opaque_rect_pipeline = self.use_opaque_rect_pipeline();
        if let Some(ref mut tr) = self.text_rasterizer {
            if let Some(ref result) = chrome_prepare_result {
                match result {
                    Err(e) => {
                        tracing::warn!(error = %e, "text prepare failed in chrome path");
                    }
                    Ok(_) => {
                        // ── Inline backdrop pass (Phase 2, hud-9ieev) ────────────
                        if !chrome_inline_verts.is_empty() {
                            let inline_buf =
                                self.device
                                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                        label: Some("inline_backdrop_verts"),
                                        contents: bytemuck::cast_slice(&chrome_inline_verts),
                                        usage: wgpu::BufferUsages::VERTEX,
                                    });
                            let mut inline_pass =
                                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                    label: Some("inline_backdrop_pass"),
                                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                        view: &surface.view,
                                        resolve_target: None,
                                        ops: wgpu::Operations {
                                            load: wgpu::LoadOp::Load,
                                            store: wgpu::StoreOp::Store,
                                        },
                                    })],
                                    depth_stencil_attachment: None,
                                    timestamp_writes: None,
                                    occlusion_query_set: None,
                                });
                            if use_opaque_rect_pipeline {
                                inline_pass.set_pipeline(&self.clear_pipeline);
                            } else {
                                inline_pass.set_pipeline(&self.pipeline);
                            }
                            inline_pass.set_vertex_buffer(0, inline_buf.slice(..));
                            inline_pass.draw(0..chrome_inline_verts.len() as u32, 0..1);
                        }

                        // ── Glyphon text pass ─────────────────────────────────────
                        let mut text_pass =
                            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("text_pass"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &surface.view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Load,
                                        store: wgpu::StoreOp::Store,
                                    },
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                            });
                        if let Err(e) = tr.render_text_pass(&mut text_pass) {
                            tracing::warn!(error = %e, "text render failed in chrome path");
                        }
                    }
                }
            }
            // Trim every frame regardless of item count — glyphs from prior frames
            // must be evicted even when the current frame has no text.
            tr.trim_atlas();
        }

        // ── Image pass: draw textured quads on top of color geometry ─────────
        self.encode_image_pass(&mut encoder, &surface.view, &textured_cmds, sw, sh);

        // ── Widget pass: composite pre-synced textures above content + text ──
        // sync_widget_textures is called earlier, before frame encoding begins.
        self.encode_widget_pass(&mut encoder, &surface.view, &scene.widget_registry, sw, sh);

        // Chrome render pass — uses LoadOp::Load to preserve content pixels.
        // Chrome commands are drawn ON TOP of content by construction.
        // No agent tile can occlude chrome regardless of z-order.
        {
            let mut chrome_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("chrome_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // LoadOp::Load: preserve content pixels — chrome draws on top.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if self.use_opaque_rect_pipeline() {
                chrome_pass.set_pipeline(&self.clear_pipeline);
            } else {
                chrome_pass.set_pipeline(&self.pipeline);
            }
            if let Some(ref buf) = chrome_buffer {
                chrome_pass.set_vertex_buffer(0, buf.slice(..));
                chrome_pass.draw(0..chrome_vertices.len() as u32, 0..1);
            }
        }

        telemetry.stage6_render_encode_us = encode_start.elapsed().as_micros() as u64;

        surface.copy_to_buffer(&mut encoder);

        let submit_start = std::time::Instant::now();
        self.queue.submit(std::iter::once(encoder.finish()));
        self.device.poll(wgpu::Maintain::Wait);
        telemetry.stage7_gpu_submit_us = submit_start.elapsed().as_micros() as u64;

        telemetry.frame_time_us = frame_start.elapsed().as_micros() as u64;
        telemetry
    }
}
