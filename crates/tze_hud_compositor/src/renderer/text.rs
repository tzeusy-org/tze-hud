//! Text-collection methods for the compositor.
//!
//! Moved from `renderer/mod.rs` text-collection cluster (formerly ~L3546–4287
//! pre-split), plus `collect_ellipsis_text_items_from_node` from the
//! module-level helpers section, by Step R-4 of the renderer module split
//! (hud-fgryk).  No logic was changed; only visibility modifiers were added
//! where Rust's module-privacy rules require them (listed in the PR body).

use std::collections::HashMap;
use std::sync::Arc;

use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use crate::text::TextItem;

use super::icon::parse_notification_icon;
use super::tile_render::node_uses_display_tile_scroll;
use super::token_colors::{
    ICON_SIZE_PX, ICON_TEXT_GAP_PX, NOTIFICATION_BODY_SCALE, NOTIFICATION_DISMISS_BUTTON_SIZE_PX,
    NOTIFICATION_DISMISS_FONT_SIZE_PX, NOTIFICATION_DISMISS_FONT_WEIGHT,
    NOTIFICATION_DISMISS_GAP_PX, NOTIFICATION_ICON_GAP_PX, NOTIFICATION_ICON_SIZE_PX,
    NOTIFICATION_INTER_LINE_GAP, NOTIFICATION_TITLE_WEIGHT, is_alert_banner_zone,
    notification_dismiss_bounds, resolve_composer_overlay_tokens, resolve_jump_to_latest_tokens,
    resolve_scroll_indicator_tokens, resolve_transcript_max_measure_px, resolve_viewer_echo_tokens,
};

/// Default line-height multiplier (`font_size_px × 1.4 = line_height_px`).
///
/// Mirrors [`crate::markdown::MarkdownTokens::default().line_height_multiplier`]
/// (1.4) and is used by all notification/icon-inset `TextItem` builders below so
/// we avoid constructing a default `MarkdownTokens` struct six times per frame.
const DEFAULT_LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Below this whole-tile opacity a tile contributes no visible text, so the
/// text collector skips shaping/rasterizing its glyphs entirely rather than
/// emitting fully-transparent `TextItem`s.  This mirrors the quad/backdrop path
/// (which paints nothing at a zero-alpha tile) so a minimized tile — opacity
/// driven to 0 by `update_tile_opacity(0.0)` — leaves NO floating glyphs on
/// screen (hud-dat3x).  Chosen well below one 8-bit alpha step (1/255 ≈ 0.0039)
/// so any perceptible fraction still blends.
const TILE_TEXT_OPACITY_EPSILON: f32 = 1e-4;

// ─── Text-collection impl block ───────────────────────────────────────────────

impl super::Compositor {
    /// Build per-entry `TextItem`s for a StatusBar zone with icon mappings.
    ///
    /// Called when `RenderingPolicy::key_icon_map` is non-empty.  Each entry
    /// occupies one slot (height = `stack_slot_height(policy)`).  Entries whose
    /// key is mapped to an SVG icon have their text x-origin shifted right by
    /// `ICON_SIZE_PX + ICON_TEXT_GAP_PX` so the icon quad can be rendered to
    /// the left without overlap.  Entries without an icon mapping are rendered
    /// at the unshifted position.
    ///
    /// Returns a unicode emoji prefix for well-known status-bar keys.
    /// Used as a v1 fallback when no SVG icon mapping exists.
    ///
    /// # Parameters
    /// - `key` — the status-bar entry key.
    ///
    /// # Returns
    /// A unicode emoji string, or empty string if key is not recognized.
    fn status_bar_icon_prefix(key: &str) -> &str {
        match key {
            "battery" => "🔋",
            "wifi" | "network" => "📶",
            "cpu" => "🖥",
            "memory" | "mem" => "💾",
            "time" | "clock" => "🕐",
            "weather" => "☀",
            "temperature" | "temp" => "🌡",
            _ => "",
        }
    }

    /// Each entry is a single `TextItem` rendered by delegating to
    /// `TextItem::from_zone_policy` with adjusted `(x, y, w, h)` that place
    /// the text in its row and account for icon inset.
    ///
    /// # Parameters
    /// - `sorted` — entries sorted by key (deterministic row order).
    /// - `key_icon_map` — key → SVG path mapping from `RenderingPolicy`.
    /// - `zx`, `zy`, `zw` — zone pixel bounds (x, y, width).
    /// - `policy` — the zone's `RenderingPolicy`.
    /// - `opacity` — current animation opacity.
    fn status_bar_icon_text_items(
        sorted: &[(&String, &String)],
        key_icon_map: &HashMap<String, String>,
        zx: f32,
        zy: f32,
        zw: f32,
        policy: &RenderingPolicy,
        opacity: f32,
    ) -> Vec<TextItem> {
        const STATUS_BAR_MAX_ENTRIES: usize = 8;
        let slot_h = Self::stack_slot_height(policy);

        sorted
            .iter()
            .take(STATUS_BAR_MAX_ENTRIES)
            .enumerate()
            .map(|(i, (k, v))| {
                let entry_y = zy + i as f32 * slot_h;
                let has_icon = key_icon_map.contains_key(k.as_str());
                let icon_inset = if has_icon {
                    ICON_SIZE_PX + ICON_TEXT_GAP_PX
                } else {
                    0.0
                };
                // Pass adjusted bounds to from_zone_policy so it applies margins
                // on top of the per-entry position and icon inset.
                // Effective pixel_x = (zx + icon_inset) + margin_h
                // Effective bounds_width = (zw - icon_inset) - 2*margin_h
                let text = if has_icon {
                    format!("{k}: {v}")
                } else {
                    let prefix = Self::status_bar_icon_prefix(k.as_str());
                    if prefix.is_empty() {
                        format!("{k}: {v}")
                    } else {
                        format!("{prefix} {k}: {v}")
                    }
                };
                TextItem::from_zone_policy(
                    &text,
                    zx + icon_inset,
                    entry_y,
                    zw - icon_inset,
                    slot_h,
                    policy,
                    opacity,
                )
            })
            .collect()
    }

    /// Build a zone-derived [`TextItem`] with the shared boilerplate filled in.
    ///
    /// All notification and icon-inset `TextItem` literals in
    /// [`Self::collect_text_items`] share four constant tail fields
    /// (`color_runs`, `styled_runs`, `line_height_multiplier`,
    /// `viewport`) and always set `clip_*` to mirror `pixel_*` / `bounds_*`.
    /// This builder centralises those seven fields so each call site only
    /// supplies the twelve fields that actually vary.
    ///
    /// Call sites (all in `collect_text_items`):
    /// - Stack / single-line notification (text = body, `alignment = Start`)
    /// - Stack / two-line title line (`font_weight = notif_title_weight`)
    /// - Stack / two-line body line (`font_size_px = body_font_size`)
    /// - MergeByKey fallback icon-inset notification
    /// - LatestWins/Replace icon-inset notification (identical to MergeByKey)
    // Lint suppressed deliberately: this IS the consolidation the lint would
    // suggest — it already absorbs seven shared fields, leaving only the
    // twelve genuinely-varying `TextItem` fields, each a distinct primitive.
    // Bundling them into a struct would just rename the same flat argument list.
    #[allow(clippy::too_many_arguments)]
    fn make_zone_text_item(
        text: Arc<str>,
        pixel_x: f32,
        pixel_y: f32,
        bounds_width: f32,
        bounds_height: f32,
        font_size_px: f32,
        font_family: FontFamily,
        font_weight: u16,
        color: [u8; 4],
        alignment: TextAlign,
        overflow: TextOverflow,
        outline_color: Option<[u8; 4]>,
        outline_width: Option<f32>,
        opacity: f32,
    ) -> TextItem {
        TextItem {
            text,
            pixel_x,
            pixel_y,
            bounds_width,
            bounds_height,
            clip_pixel_x: pixel_x,
            clip_pixel_y: pixel_y,
            clip_bounds_width: bounds_width,
            clip_bounds_height: bounds_height,
            font_size_px,
            font_family,
            font_weight,
            color,
            alignment,
            overflow,
            outline_color,
            outline_width,
            opacity,
            color_runs: Box::default(),
            styled_runs: Box::default(),
            line_height_multiplier: DEFAULT_LINE_HEIGHT_MULTIPLIER,
            viewport: crate::overflow::TruncationViewport::HeadAnchored,
        }
    }

    /// Build a `TextItem` for `ZoneContent::StreamText`, honouring the zone's
    /// tail-anchor opt-in.
    ///
    /// Delegates geometry, color, and typography to
    /// [`TextItem::from_zone_policy`] (so all visual properties still come from
    /// `RenderingPolicy` — no hardcoded styling), then selects the truncation
    /// viewport:
    ///
    /// - `policy.stream_tail_anchored == Some(true)` →
    ///   [`TruncationViewport::TailAnchored`]: a streaming surface shows the
    ///   **newest** content (the tail), mirroring the transcript portal's
    ///   follow-tail behaviour (spec §3.2).
    /// - otherwise → [`TruncationViewport::HeadAnchored`] (the default;
    ///   preserves the pre-existing behaviour for all zones that have not opted
    ///   in).
    ///
    /// The viewport only changes which side is truncated when overflow resolves
    /// to [`TextOverflow::Ellipsis`]; for `Clip` it is inert.  This is the single
    /// place the zone-vs-portal tail-anchor decision is made for StreamText, so
    /// the per-frame item and any primed cache entry agree on `viewport` (the
    /// truncation key includes it).
    ///
    /// [`TruncationViewport`]: crate::overflow::TruncationViewport
    /// [`TextOverflow`]: tze_hud_scene::types::TextOverflow
    #[allow(clippy::too_many_arguments)]
    pub(super) fn zone_stream_text_item(
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        policy: &RenderingPolicy,
        opacity: f32,
    ) -> TextItem {
        let mut item = TextItem::from_zone_policy(text, x, y, w, h, policy, opacity);
        if policy.stream_tail_anchored == Some(true) {
            item.viewport = crate::overflow::TruncationViewport::TailAnchored;
        }
        item
    }

    /// Collect the `Ellipsis`-overflow `ZoneContent::StreamText` `TextItem`s in
    /// the scene, for off-frame truncation-cache priming.
    ///
    /// Zone StreamText items are not produced by the tile-node walk in
    /// [`Compositor::prime_truncation_cache`], so without this pass an
    /// overflowing streaming zone would hit the render path cold and fall back to
    /// raw clipping (never tail- or head-anchored truncation).  This method
    /// rebuilds the same StreamText items `collect_text_items` emits — identical
    /// geometry, policy, and `viewport` (via [`Self::zone_stream_text_item`]) —
    /// so the per-frame key matches the primed cache entry.
    ///
    /// Only `Ellipsis` items are emitted: `effective_truncation_key` returns
    /// `None` for any other overflow, so priming them would be wasted work.
    /// Opacity is irrelevant to the truncation key (which keys on text, geometry,
    /// font, and viewport only), so this pass uses `1.0`.
    ///
    /// # Word-by-word reveal interaction
    ///
    /// This pass primes the **full** StreamText content.  While a
    /// `LatestWins`/`Replace` zone is mid word-by-word reveal, the render path
    /// keys on the partially-revealed *prefix* (`visible_text`), so the primed
    /// full-text entry will not match until the reveal completes — those frames
    /// fall back to raw clipping (the doctrinal arrival≠presentation graceful
    /// path).  Once the reveal finishes (`visible_text == text`) the keys agree
    /// and the steady-state streaming surface renders correctly anchored.  Reveal
    /// prefixes are short and typically fit without overflow, so this is rarely
    /// observable in practice.
    pub(super) fn collect_zone_stream_text_ellipsis_items(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> Vec<TextItem> {
        let mut items: Vec<TextItem> = Vec::new();

        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            let policy = &zone_def.rendering_policy;
            // Truncation (and thus anchoring) only applies to Ellipsis overflow.
            if policy.overflow != Some(TextOverflow::Ellipsis) {
                continue;
            }

            let (zx, zy, zw, zh) = Self::resolve_zone_geometry(&zone_def.geometry_policy, sw, sh);

            match zone_def.contention_policy {
                ContentionPolicy::Stack { .. } => {
                    let layout = self.zone_slot_layout(zone_name, publishes, policy, zh);
                    for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(zy) {
                        if let ZoneContent::StreamText(text) = &publishes[pub_idx].content {
                            items.push(Self::zone_stream_text_item(
                                text,
                                zx,
                                slot_y,
                                zw,
                                effective_slot_h,
                                policy,
                                1.0,
                            ));
                        }
                    }
                }
                ContentionPolicy::MergeByKey { .. }
                | ContentionPolicy::LatestWins
                | ContentionPolicy::Replace => {
                    // Only the most-recent StreamText publish renders (and is primed).
                    for record in publishes.iter().rev() {
                        if let ZoneContent::StreamText(text) = &record.content {
                            items.push(Self::zone_stream_text_item(
                                text, zx, zy, zw, zh, policy, 1.0,
                            ));
                            break;
                        }
                    }
                }
            }
        }

        items
    }

    /// Collect `TextItem`s for all TextMarkdownNode tiles and zone StreamText
    /// and ShortTextWithIcon/Notification content in the scene.
    ///
    /// All zone `TextItem`s are constructed from `RenderingPolicy` fields —
    /// no hardcoded colors or font choices.  Animation opacity is applied to
    /// the color channels so text fades with the backdrop.
    ///
    /// Returns a flat `Vec<TextItem>` ready for `TextRasterizer::prepare_text_items`.
    pub(super) fn collect_text_items(&self, scene: &SceneGraph, sw: f32, sh: f32) -> Vec<TextItem> {
        let mut items: Vec<TextItem> = Vec::new();

        // Transcript optimal-measure cap (hud-rivcy): resolve once per frame; a
        // positive value narrows portal-surface transcript wrapping, `0` leaves
        // it unbounded. Gated to portal (scrollable) tiles per-tile below, the
        // same surface signal that selects the portal markdown token scope.
        let transcript_max_measure_px = resolve_transcript_max_measure_px(&self.token_map);

        // ── TextMarkdownNode tiles ────────────────────────────────────────────
        for tile in &Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene) {
            if let Some(root_id) = tile.root_node {
                // Whole-tile opacity, split into its two components:
                //
                //   • `base_opacity` — the DURABLE scene-level `tile.opacity`
                //     (× drag boost). The exemplar minimize path drives this to 0
                //     via `update_tile_opacity(0.0)`; it is a persistent state, not
                //     a per-frame animation.
                //   • the §6.3 portal-transition animation opacity — a TRANSIENT
                //     per-frame fade that a portal passes through while collapsing
                //     or expanding.
                //
                // Skip shaping ONLY on durable invisibility. A tile fading IN still
                // has `base_opacity == 1` while its animation opacity ramps up from
                // 0; shaping it now (and blending the item alpha down) warms the
                // shaped-buffer cache so the fade reuses one shape instead of
                // re-shaping when it crosses the visibility threshold (hud-991cj).
                // Gating the skip on the *combined* value would defer that shape
                // into the middle of the animation — a per-frame re-shape hitch.
                let base_opacity = self.rendered_tile_opacity(tile, scene);
                if base_opacity <= TILE_TEXT_OPACITY_EPSILON {
                    // Durably invisible (minimized / dragged to zero): collect,
                    // shape, truncate, and draw nothing — no floating glyphs
                    // (hud-dat3x). Matches the quad/backdrop path, which paints
                    // nothing at a zero-alpha tile.
                    continue;
                }
                // The value the quad/backdrop path applies via
                // `tile_effective_opacity` (== base × portal anim). Text fades in
                // lockstep with the tile's solid-color backdrop.
                let tile_opacity = self.tile_effective_opacity(tile, scene);

                // Compute scroll offset once per tile and pass it down so text
                // glyph positions track the scrolled content (Bounded Transcript
                // Viewport requirement — hud-w5ih).
                let (scroll_x, scroll_y) = self.display_tile_scroll_offset(scene, tile.id);
                // Determine follow-tail state so Ellipsis TextItems receive the
                // correct TruncationViewport (TailAnchored vs HeadAnchored).
                // Uses the shared helper that prime_truncation_cache /
                // collect_ellipsis_text_items_from_node also calls, so the
                // per-frame key always matches the primed cache entry (hud-lu50e,
                // hud-plz8q).
                let at_tail = super::tile_at_tail_for_ellipsis(tile.id, scene);

                // Track item count before so the tile's whole-tile opacity can be
                // folded into exactly the glyphs this tile just contributed.
                let items_before = items.len();
                // Only portal (scrollable) surfaces are transcripts; other
                // markdown tiles wrap to full width unchanged. Pass 0.0 (no
                // clamp) for non-portal tiles so the collector is a no-op there.
                let transcript_measure_px = if scene.tile_scroll_config(tile.id).is_some() {
                    transcript_max_measure_px
                } else {
                    0.0
                };
                // Per-part portal-surface consumption (hud-s4lrw): when the tile
                // declares a `PortalSurface`, resolve each node's part so the
                // recursion applies transcript scope + a per-part clip band only
                // where the schema says. `None` preserves legacy tile-level
                // overflow/clip behavior; geometry-only resident surfaces still
                // resolve their chrome-vs-document translation per node below.
                let part_index = portal_part_index(scene, tile.id);
                self.collect_text_items_from_node(
                    root_id,
                    tile,
                    scene,
                    scroll_x,
                    scroll_y,
                    at_tail,
                    transcript_measure_px,
                    part_index,
                    None,
                    &mut items,
                );

                // Fractional opacity: blend every glyph (transcript, cached
                // markdown, plain, and ellipsis/truncation items alike) the tile
                // just contributed, proportionally — matching the backdrop fade.
                // This includes the §6.3 portal animation opacity, since
                // `tile_effective_opacity` already folds it in.
                if tile_opacity < 1.0 {
                    for item in &mut items[items_before..] {
                        item.opacity *= tile_opacity;
                    }
                }

                // The per-segment streaming-reveal fade (hud-bl7yi) is applied
                // inline in `collect_text_items_from_node`, routed by
                // `(tile, node)` identity (hud-g8xpg), so it never cross-fades a
                // settled node onto a same-text revealing sibling. Nothing to do
                // here.
            }
        }

        // ── Zone StreamText, Notification, and StatusBar content ─────────────
        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };

            // Resolve zone geometry to pixel bounds.
            let (zx, zy, zw, zh) = Self::resolve_zone_geometry(&zone_def.geometry_policy, sw, sh);

            let policy = &zone_def.rendering_policy;

            // Current animation opacity for this zone.
            let anim_opacity = if self.degradation_policy.level >= DegradationLevel::Simplified {
                1.0
            } else {
                self.zone_animation_states
                    .get(zone_name)
                    .map(|s| s.current_opacity())
                    .unwrap_or(1.0)
            };

            // Emit TextItems based on contention policy.
            //
            // Stack: each publication occupies a vertically-stacked slot.
            //   For generic Stack zones: newest at top (slot 0 = newest).
            //   For alert-banner zones: severity-descending (critical first),
            //   then recency-descending (newer first) within the same severity.
            //   Per-slot heights are computed via Self::per_slot_heights() and may
            //   vary by item (two-line notifications occupy a taller slot).
            //   Dynamic height: alert-banner zone height = sum(slot_heights).
            //
            // MergeByKey: collect ALL StatusBar publications, merge their entries
            //   (last write wins per key), render the merged set as one text item.
            //
            // LatestWins / Replace: render only the most-recent publication.
            match zone_def.contention_policy {
                ContentionPolicy::Stack { .. } => {
                    // Slot geometry is computed once by zone_slot_layout and shared
                    // with render_zone_content / collect_all_rounded_rect_cmds (hud-qlerb).
                    let layout = self.zone_slot_layout(zone_name, publishes, policy, zh);

                    // Resolve notification typography tokens needed for text rendering
                    // (not slot geometry — those live in zone_slot_layout).
                    let notif_body_scale = self
                        .token_map
                        .get("typography.notification.body.scale")
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|v| v.is_finite())
                        .unwrap_or(NOTIFICATION_BODY_SCALE)
                        .clamp(0.5, 1.0);
                    let notif_title_weight = self
                        .token_map
                        .get("typography.notification.title.weight")
                        .and_then(|v| v.parse::<u16>().ok())
                        .unwrap_or(NOTIFICATION_TITLE_WEIGHT);
                    let notif_dismiss_font_size_px = self
                        .token_map
                        .get("typography.notification.dismiss.font_size_px")
                        .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
                        .unwrap_or(NOTIFICATION_DISMISS_FONT_SIZE_PX)
                        .clamp(6.0, 200.0);
                    let notif_dismiss_font_weight = self
                        .token_map
                        .get("typography.notification.dismiss.font_weight")
                        .and_then(|v| v.parse::<u16>().ok())
                        .unwrap_or(NOTIFICATION_DISMISS_FONT_WEIGHT);

                    for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(zy) {
                        let record = &publishes[pub_idx];

                        // Per-publication fade-out opacity (1.0 when no fade active).
                        let pub_opacity = self.pub_opacity(zone_name, record);
                        // Combined opacity: zone animation × per-publication fade.
                        let effective_opacity = anim_opacity * pub_opacity;

                        match &record.content {
                            ZoneContent::StreamText(text) => {
                                items.push(Self::zone_stream_text_item(
                                    text,
                                    zx,
                                    slot_y,
                                    zw,
                                    effective_slot_h,
                                    policy,
                                    effective_opacity,
                                ));
                            }
                            ZoneContent::Notification(payload) => {
                                // Notification text rendering priority:
                                // 1. RenderingPolicy explicit values (set by zone configuration)
                                // 2. Token-resolved values (typography.body.size, color.text.primary)
                                // 3. Hardcoded defaults (16px, near-white)
                                //
                                // This allows alert-banner and other configured zones to override
                                // the defaults via RenderingPolicy while notification-area falls
                                // through to the token/default path.
                                //
                                // Layout: left-aligned with 9px inset (8px padding + 1px border),
                                // clips at content area boundary (no wrapping in v1).
                                // When a valid icon texture is cached, the text is additionally
                                // inset to the right of the icon (icon_size + gap).
                                // Spec-defined inset for notification-area zones:
                                // 1px border + 8px padding = 9px from backdrop edges.
                                // When the zone's RenderingPolicy explicitly sets
                                // margin_horizontal/margin_vertical, those values take
                                // precedence (e.g. alert-banner uses margin_horizontal=8).
                                const NOTIFICATION_BORDER_PX: f32 = 1.0;
                                const NOTIFICATION_PADDING_PX: f32 = 8.0;
                                const NOTIFICATION_INSET: f32 =
                                    NOTIFICATION_BORDER_PX + NOTIFICATION_PADDING_PX;
                                // Horizontal inset: policy margin_horizontal > margin_px > 9px
                                let inset_h = policy
                                    .margin_horizontal
                                    .or(policy.margin_px)
                                    .unwrap_or(NOTIFICATION_INSET);
                                // Vertical inset: policy margin_vertical > margin_px > 9px
                                let inset_v = policy
                                    .margin_vertical
                                    .or(policy.margin_px)
                                    .unwrap_or(NOTIFICATION_INSET);
                                // If a cached icon texture exists, inset text to the right of it.
                                let icon_width_reserved = parse_notification_icon(&payload.icon)
                                    .filter(|id| self.image_texture_cache.contains_key(id))
                                    .map(|_| NOTIFICATION_ICON_SIZE_PX + NOTIFICATION_ICON_GAP_PX)
                                    .unwrap_or(0.0);
                                // Font size: policy explicit > typography.body.size token > 16px
                                let font_size_px = policy.font_size_px.unwrap_or_else(|| {
                                    Self::resolve_body_font_size(&self.token_map)
                                });
                                // Font family: policy explicit > SystemSansSerif default
                                let font_family = policy
                                    .font_family
                                    .unwrap_or(tze_hud_scene::types::FontFamily::SystemSansSerif);
                                // Text color: policy explicit > color.text.primary token > near-white
                                let base_color = policy
                                    .text_color
                                    .map(crate::text::rgba_to_srgb_u8)
                                    .unwrap_or_else(|| {
                                        Self::resolve_text_primary_color(&self.token_map)
                                    });
                                let color = crate::text::apply_opacity_to_color(
                                    base_color,
                                    effective_opacity,
                                );
                                // Outline: use policy outline if set, otherwise
                                // derive from policy (ensures legibility on
                                // light-colored backdrops like warning/amber).
                                let (oc, ow) = match (policy.outline_color, policy.outline_width) {
                                    (Some(oc), Some(ow)) if ow > 0.0 => {
                                        let oc_srgb = crate::text::apply_opacity_to_color(
                                            crate::text::rgba_to_srgb_u8(oc),
                                            effective_opacity,
                                        );
                                        (Some(oc_srgb), Some(ow))
                                    }
                                    _ => (None, None),
                                };

                                let dismiss_reserved_w = if is_alert_banner_zone(zone_name) {
                                    0.0
                                } else {
                                    NOTIFICATION_DISMISS_BUTTON_SIZE_PX
                                        + NOTIFICATION_DISMISS_GAP_PX
                                };
                                // Text x-offset respects icon reservation (from icon texture pipeline).
                                let text_x = zx + inset_h + icon_width_reserved;
                                let text_w = (zw
                                    - inset_h
                                    - icon_width_reserved
                                    - inset_h
                                    - dismiss_reserved_w)
                                    .max(1.0);

                                if payload.title.is_empty() {
                                    // ── Single-line rendering (backward-compatible) ──
                                    // Font weight: policy explicit > 400 default
                                    let font_weight = policy.font_weight.unwrap_or(400);
                                    items.push(Self::make_zone_text_item(
                                        Arc::from(payload.text.as_str()),
                                        text_x,
                                        slot_y + inset_v,
                                        text_w,
                                        (effective_slot_h - inset_v * 2.0).max(1.0),
                                        font_size_px,
                                        font_family,
                                        font_weight,
                                        color,
                                        tze_hud_scene::types::TextAlign::Start,
                                        tze_hud_scene::types::TextOverflow::Clip,
                                        oc,
                                        ow,
                                        effective_opacity,
                                    ));
                                } else {
                                    // ── Two-line rendering: bold title + regular body ──
                                    //
                                    // Use pre-resolved token values (notif_title_weight,
                                    // notif_body_scale) to keep rendering consistent with
                                    // the slot-height calculation above.
                                    let body_font_size = font_size_px * notif_body_scale;
                                    let title_line_h = font_size_px * 1.4;
                                    let content_top = slot_y + inset_v;

                                    // Title line (bold)
                                    items.push(Self::make_zone_text_item(
                                        Arc::from(payload.title.as_str()),
                                        text_x,
                                        content_top,
                                        text_w,
                                        title_line_h.max(1.0),
                                        font_size_px,
                                        font_family,
                                        notif_title_weight,
                                        color,
                                        tze_hud_scene::types::TextAlign::Start,
                                        tze_hud_scene::types::TextOverflow::Clip,
                                        oc,
                                        ow,
                                        effective_opacity,
                                    ));
                                    // Body line (regular weight, 0.85× size)
                                    let body_top =
                                        content_top + title_line_h + NOTIFICATION_INTER_LINE_GAP;
                                    // Remaining slot height available for body (down to inset bottom)
                                    let body_bounds_h =
                                        ((slot_y + effective_slot_h - inset_v) - body_top).max(1.0);
                                    items.push(Self::make_zone_text_item(
                                        Arc::from(payload.text.as_str()),
                                        text_x,
                                        body_top,
                                        text_w,
                                        body_bounds_h,
                                        body_font_size,
                                        font_family,
                                        400,
                                        color,
                                        tze_hud_scene::types::TextAlign::Start,
                                        tze_hud_scene::types::TextOverflow::Clip,
                                        oc,
                                        ow,
                                        effective_opacity,
                                    ));
                                }

                                if !is_alert_banner_zone(zone_name)
                                    && self.text_rasterizer.is_some()
                                {
                                    let dismiss_bounds = notification_dismiss_bounds(
                                        zx,
                                        slot_y,
                                        zw,
                                        effective_slot_h,
                                    );
                                    let dismiss_color = crate::text::apply_opacity_to_color(
                                        base_color,
                                        effective_opacity,
                                    );
                                    static DISMISS_LABEL: std::sync::OnceLock<Arc<str>> =
                                        std::sync::OnceLock::new();
                                    items.push(Self::make_zone_text_item(
                                        Arc::clone(DISMISS_LABEL.get_or_init(|| Arc::from("X"))),
                                        dismiss_bounds.x,
                                        dismiss_bounds.y + 1.0,
                                        dismiss_bounds.width.max(1.0),
                                        dismiss_bounds.height.max(1.0),
                                        notif_dismiss_font_size_px,
                                        font_family,
                                        notif_dismiss_font_weight,
                                        dismiss_color,
                                        tze_hud_scene::types::TextAlign::Center,
                                        tze_hud_scene::types::TextOverflow::Clip,
                                        None,
                                        None,
                                        effective_opacity,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ContentionPolicy::MergeByKey { max_keys } => {
                    // Collect all StatusBar publications and merge their entries.
                    // For each key, the last publish wins (latest value).
                    let mut merged: HashMap<String, String> =
                        HashMap::with_capacity(max_keys as usize);
                    for record in publishes.iter() {
                        if let ZoneContent::StatusBar(payload) = &record.content {
                            for (k, v) in &payload.entries {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    if !merged.is_empty() {
                        let mut sorted: Vec<(&String, &String)> = merged.iter().collect();
                        sorted.sort_by_key(|(k, _)| k.as_str());
                        // Use per-entry layout only when key_icon_map is non-empty AND
                        // at least one current entry key has an icon mapping.  This
                        // avoids switching layout when the map is configured but none
                        // of the currently displayed entries are mapped.
                        let use_icon_layout = !policy.key_icon_map.is_empty()
                            && sorted
                                .iter()
                                .any(|(k, _)| policy.key_icon_map.contains_key(k.as_str()));
                        if !use_icon_layout {
                            // No icons for current entries: render all as a single
                            // newline-joined TextItem (existing behavior).
                            let text = sorted
                                .iter()
                                .map(|(k, v)| format!("{k}: {v}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            items.push(TextItem::from_zone_policy(
                                &text,
                                zx,
                                zy,
                                zw,
                                zh,
                                policy,
                                anim_opacity,
                            ));
                        } else {
                            // Icons configured for current entries: render each as an
                            // individual TextItem, with text x-inset when an icon is mapped.
                            items.extend(Self::status_bar_icon_text_items(
                                &sorted,
                                &policy.key_icon_map,
                                zx,
                                zy,
                                zw,
                                policy,
                                anim_opacity,
                            ));
                        }
                    } else {
                        // Fallback: render whatever the latest publication contains.
                        for record in publishes.iter().rev() {
                            match &record.content {
                                ZoneContent::StreamText(text) => {
                                    items.push(Self::zone_stream_text_item(
                                        text,
                                        zx,
                                        zy,
                                        zw,
                                        zh,
                                        policy,
                                        anim_opacity,
                                    ));
                                    break;
                                }
                                ZoneContent::Notification(payload) => {
                                    // MergeByKey fallback: icon-aware inset, same logic
                                    // as the LatestWins/Replace path.
                                    let icon_width_reserved =
                                        parse_notification_icon(&payload.icon)
                                            .filter(|id| self.image_texture_cache.contains_key(id))
                                            .map(|_| {
                                                NOTIFICATION_ICON_SIZE_PX + NOTIFICATION_ICON_GAP_PX
                                            })
                                            .unwrap_or(0.0);
                                    if icon_width_reserved > 0.0 {
                                        // Use the same default inset as the icon draw path
                                        // (NOTIFICATION_INSET = 9px) so text and icon stay
                                        // aligned when policy margins are not explicitly set.
                                        const NOTIFICATION_INSET: f32 = 9.0;
                                        let margin_h = policy
                                            .margin_horizontal
                                            .or(policy.margin_px)
                                            .unwrap_or(NOTIFICATION_INSET);
                                        let margin_v = policy
                                            .margin_vertical
                                            .or(policy.margin_px)
                                            .unwrap_or(NOTIFICATION_INSET);
                                        let font_size_px =
                                            policy.font_size_px.unwrap_or(16.0).clamp(6.0, 200.0);
                                        let font_family = policy.font_family.unwrap_or(
                                            tze_hud_scene::types::FontFamily::SystemSansSerif,
                                        );
                                        let font_weight =
                                            policy.font_weight.unwrap_or(400).clamp(100, 900);
                                        let base_color = policy
                                            .text_color
                                            .map(crate::text::rgba_to_srgb_u8)
                                            .unwrap_or([255, 255, 255, 220]);
                                        let color = crate::text::apply_opacity_to_color(
                                            base_color,
                                            anim_opacity,
                                        );
                                        let (oc, ow) =
                                            match (policy.outline_color, policy.outline_width) {
                                                (Some(oc), Some(ow)) if ow > 0.0 => {
                                                    let oc_srgb =
                                                        crate::text::apply_opacity_to_color(
                                                            crate::text::rgba_to_srgb_u8(oc),
                                                            anim_opacity,
                                                        );
                                                    (Some(oc_srgb), Some(ow))
                                                }
                                                _ => (None, None),
                                            };
                                        items.push(Self::make_zone_text_item(
                                            Arc::from(payload.text.as_str()),
                                            zx + margin_h + icon_width_reserved,
                                            zy + margin_v,
                                            (zw - margin_h - icon_width_reserved - margin_h)
                                                .max(1.0),
                                            (zh - margin_v * 2.0).max(1.0),
                                            font_size_px,
                                            font_family,
                                            font_weight,
                                            color,
                                            policy
                                                .text_align
                                                .unwrap_or(tze_hud_scene::types::TextAlign::Start),
                                            policy.overflow.unwrap_or(
                                                tze_hud_scene::types::TextOverflow::Clip,
                                            ),
                                            oc,
                                            ow,
                                            anim_opacity,
                                        ));
                                    } else {
                                        items.push(TextItem::from_zone_policy(
                                            &payload.text,
                                            zx,
                                            zy,
                                            zw,
                                            zh,
                                            policy,
                                            anim_opacity,
                                        ));
                                    }
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ContentionPolicy::LatestWins | ContentionPolicy::Replace => {
                    // Use the most-recent publish only.
                    for record in publishes.iter().rev() {
                        match &record.content {
                            ZoneContent::StreamText(text) => {
                                // Apply streaming word-by-word reveal if active.
                                // The reveal state truncates the visible text to a
                                // byte offset (breakpoint boundary).  When no reveal
                                // state exists, the full text is rendered immediately.
                                let visible_text: &str = if let Some(state) =
                                    self.stream_reveal_states.get(zone_name)
                                {
                                    let offset = state.visible_byte_offset();
                                    if offset == usize::MAX {
                                        text.as_str()
                                    } else {
                                        // Clamp to string length, then walk backward to
                                        // a valid UTF-8 character boundary.  Breakpoints
                                        // come from external input and may not be on a
                                        // char boundary; slicing at a non-boundary panics.
                                        let mut safe_offset = offset.min(text.len());
                                        while safe_offset > 0 && !text.is_char_boundary(safe_offset)
                                        {
                                            safe_offset -= 1;
                                        }
                                        &text[..safe_offset]
                                    }
                                } else {
                                    text.as_str()
                                };
                                items.push(Self::zone_stream_text_item(
                                    visible_text,
                                    zx,
                                    zy,
                                    zw,
                                    zh,
                                    policy,
                                    anim_opacity,
                                ));
                                // Only render the most-recent StreamText publish.
                                break;
                            }
                            ZoneContent::Notification(payload) => {
                                // Render the notification text.
                                // When a valid icon texture is cached, inset the text
                                // to the right of the icon (icon_size + gap px).
                                let icon_width_reserved = parse_notification_icon(&payload.icon)
                                    .filter(|id| self.image_texture_cache.contains_key(id))
                                    .map(|_| NOTIFICATION_ICON_SIZE_PX + NOTIFICATION_ICON_GAP_PX)
                                    .unwrap_or(0.0);
                                if icon_width_reserved > 0.0 {
                                    // Build TextItem manually to apply icon offset.
                                    // Use the same default inset as the icon draw path
                                    // (NOTIFICATION_INSET = 9px) so text and icon stay
                                    // aligned when policy margins are not explicitly set.
                                    const NOTIFICATION_INSET: f32 = 9.0;
                                    let margin_h = policy
                                        .margin_horizontal
                                        .or(policy.margin_px)
                                        .unwrap_or(NOTIFICATION_INSET);
                                    let margin_v = policy
                                        .margin_vertical
                                        .or(policy.margin_px)
                                        .unwrap_or(NOTIFICATION_INSET);
                                    let font_size_px =
                                        policy.font_size_px.unwrap_or(16.0).clamp(6.0, 200.0);
                                    let font_family = policy.font_family.unwrap_or(
                                        tze_hud_scene::types::FontFamily::SystemSansSerif,
                                    );
                                    let font_weight =
                                        policy.font_weight.unwrap_or(400).clamp(100, 900);
                                    let base_color = policy
                                        .text_color
                                        .map(crate::text::rgba_to_srgb_u8)
                                        .unwrap_or([255, 255, 255, 220]);
                                    let color = crate::text::apply_opacity_to_color(
                                        base_color,
                                        anim_opacity,
                                    );
                                    let (oc, ow) =
                                        match (policy.outline_color, policy.outline_width) {
                                            (Some(oc), Some(ow)) if ow > 0.0 => {
                                                let oc_srgb = crate::text::apply_opacity_to_color(
                                                    crate::text::rgba_to_srgb_u8(oc),
                                                    anim_opacity,
                                                );
                                                (Some(oc_srgb), Some(ow))
                                            }
                                            _ => (None, None),
                                        };
                                    items.push(Self::make_zone_text_item(
                                        Arc::from(payload.text.as_str()),
                                        zx + margin_h + icon_width_reserved,
                                        zy + margin_v,
                                        (zw - margin_h - icon_width_reserved - margin_h).max(1.0),
                                        (zh - margin_v * 2.0).max(1.0),
                                        font_size_px,
                                        font_family,
                                        font_weight,
                                        color,
                                        policy
                                            .text_align
                                            .unwrap_or(tze_hud_scene::types::TextAlign::Start),
                                        policy
                                            .overflow
                                            .unwrap_or(tze_hud_scene::types::TextOverflow::Clip),
                                        oc,
                                        ow,
                                        anim_opacity,
                                    ));
                                } else {
                                    items.push(TextItem::from_zone_policy(
                                        &payload.text,
                                        zx,
                                        zy,
                                        zw,
                                        zh,
                                        policy,
                                        anim_opacity,
                                    ));
                                }
                                // Only render the most-recent publish.
                                break;
                            }
                            ZoneContent::StatusBar(payload) => {
                                // Format key-value pairs as "key: value" lines, sorted by key
                                // for deterministic output.
                                let mut sorted: Vec<(&String, &String)> =
                                    payload.entries.iter().collect();
                                sorted.sort_by_key(|(k, _)| k.as_str());
                                // Use per-entry layout only when key_icon_map is non-empty AND
                                // at least one current entry key has an icon mapping.
                                let use_icon_layout = !policy.key_icon_map.is_empty()
                                    && sorted
                                        .iter()
                                        .any(|(k, _)| policy.key_icon_map.contains_key(k.as_str()));
                                if !use_icon_layout {
                                    // No icons for current entries: single newline-joined TextItem.
                                    let text = sorted
                                        .iter()
                                        .map(|(k, v)| format!("{k}: {v}"))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    items.push(TextItem::from_zone_policy(
                                        &text,
                                        zx,
                                        zy,
                                        zw,
                                        zh,
                                        policy,
                                        anim_opacity,
                                    ));
                                } else {
                                    // Icons configured for current entries: per-entry TextItems.
                                    items.extend(Self::status_bar_icon_text_items(
                                        &sorted,
                                        &policy.key_icon_map,
                                        zx,
                                        zy,
                                        zw,
                                        policy,
                                        anim_opacity,
                                    ));
                                }
                                // Only render the most-recent StatusBar publish.
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // ── Composer echo text (hud-r3ax6) ───────────────────────────────────
        // Inject a TextItem for the local composer draft + caret glyph on top of
        // the composer-active tile.  The background geometry is handled in
        // render_composer_overlay / render_frame tile loop.
        if self.local_composer.is_some() {
            let composer_tokens = resolve_composer_overlay_tokens(&self.token_map);
            for tile in &Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene) {
                if let Some(mut text_item) =
                    self.collect_composer_text_item(tile, scene, sw, sh, &composer_tokens)
                {
                    // Fold the whole-tile opacity into the composer draft so it
                    // fades with (and vanishes alongside) the tile backdrop
                    // rather than floating over a minimized tile (hud-dat3x).
                    // Skip only on DURABLE invisibility (scene tile.opacity); a
                    // transient portal fade still shapes and blends.
                    if self.rendered_tile_opacity(tile, scene) > TILE_TEXT_OPACITY_EPSILON {
                        text_item.opacity *= self.tile_effective_opacity(tile, scene);
                        items.push(text_item);
                    }
                    // Only the first matching tile owns the composer (focus is
                    // exclusive), so stop regardless of whether it was visible.
                    break;
                }
            }
        }

        // ── Jump-to-latest unread-count badge (hud-g1ena.3) ──────────────────
        // The ambient unread count the jump-to-latest pill MAY carry
        // (portal-chat-grade-affordances §Jump-to-Latest Affordance). The pill
        // rect itself is drawn in `render_frame`'s tile loop; this injects the
        // count TEXT centered on top of it. `collect_jump_to_latest_badge_item`
        // reproduces the pill's exact visibility gates (overflow + scrolled-away)
        // and only yields a `TextItem` when the tile carries a nonzero,
        // non-redacted unread count — so it renders on any scrolled-back
        // scrollable tile with unread content and clears with the pill at the tail.
        let jump_to_latest_tokens = resolve_jump_to_latest_tokens(&self.token_map);
        let scroll_indicator_tokens = resolve_scroll_indicator_tokens(&self.token_map);
        for tile in self.policy_visible_tiles(scene) {
            if let Some(mut badge) = self.collect_jump_to_latest_badge_item(
                tile,
                scene,
                &jump_to_latest_tokens,
                &scroll_indicator_tokens,
            ) {
                // Fade the badge with the tile (matches the pill / composer echo).
                if self.rendered_tile_opacity(tile, scene) > TILE_TEXT_OPACITY_EPSILON {
                    badge.opacity *= self.tile_effective_opacity(tile, scene);
                    items.push(badge);
                }
            }
        }

        // ── Viewer reply echo (hud-nx7yq.3) ──────────────────────────────────
        // Inject kind-distinct viewer history lines above the composer strip for
        // any portal tile with retained runtime-authored echoes. Unlike the
        // composer (single focused tile), echoes may exist on multiple tiles, so
        // every visible tile is checked. No-op when the store is empty.
        if !self.viewer_echoes.is_empty() {
            let viewer_tokens = resolve_viewer_echo_tokens(&self.token_map);
            for tile in &Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene) {
                // Skip minimized tiles entirely and blend the rest, so viewer
                // echo history fades with the tile backdrop instead of floating
                // over a hidden tile (hud-dat3x). Gate the skip on DURABLE
                // invisibility (scene tile.opacity) — a transient portal fade
                // still shapes and blends.
                if self.rendered_tile_opacity(tile, scene) <= TILE_TEXT_OPACITY_EPSILON {
                    continue;
                }
                let tile_opacity = self.tile_effective_opacity(tile, scene);
                let items_before = items.len();
                items.extend(self.collect_viewer_echo_text_items(
                    tile,
                    scene,
                    sw,
                    sh,
                    &viewer_tokens,
                ));
                if tile_opacity < 1.0 {
                    for item in &mut items[items_before..] {
                        item.opacity *= tile_opacity;
                    }
                }
            }
        }

        items
    }

    /// Resolve `color.text.primary` from the token map as a sRGB u8 color.
    ///
    /// Falls back to near-white (R=255, G=255, B=255, A=223) when the token
    /// is absent — matching the canonical default for text on dark backgrounds.
    pub(super) fn resolve_text_primary_color(token_map: &HashMap<String, String>) -> [u8; 4] {
        super::token_colors::resolve_token_color(token_map, "color.text.primary")
            .map(crate::text::rgba_to_srgb_u8)
            .unwrap_or([255, 255, 255, 223]) // near-white, alpha ≈ 87.5%
    }

    /// Resolve `typography.body.size` from the token map as a pixel value.
    ///
    /// Falls back to 16.0 px (canonical body text default) when the token is
    /// absent or cannot be parsed as a number.
    pub(super) fn resolve_body_font_size(token_map: &HashMap<String, String>) -> f32 {
        token_map
            .get("typography.body.size")
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .unwrap_or(16.0)
            .clamp(6.0, 200.0)
    }

    /// Recursively collect `TextItem`s from a node and its children.
    ///
    /// `at_tail`: whether the owning tile is currently in follow-tail/at-tail
    /// mode.  When `true` and the node uses `TextOverflow::Ellipsis`, the
    /// resulting `TextItem`'s viewport is set to `TailAnchored` so the
    /// per-frame truncation key matches the primed cache entry (hud-lu50e).
    /// Apply the viewer-local whole-portal resize font scaling (hud-ovjxu.1) to a
    /// base `font_size_px`.
    ///
    /// When the tile carries a font-scale multiplier (from a whole-portal
    /// resize), the effective font is `base * scale` clamped to the token-defined
    /// legible min/max — so text grows and shrinks with the portal, and further
    /// shrink past the minimum holds the font at the floor while only the content
    /// window keeps shrinking (spec §Portal Resize Text Scaling). At the default
    /// scale (1.0 — no resize) the adapter-published font is returned untouched,
    /// so non-resized portals and all other text are unaffected.
    ///
    /// Both the render collector and the ellipsis/truncation collector call this
    /// with the same inputs, so the truncation cache key (which includes
    /// `font_size_px`) matches what the render path shapes.
    pub(super) fn scaled_portal_font(
        &self,
        base_font_px: f32,
        tile_id: SceneId,
        scene: &SceneGraph,
    ) -> f32 {
        let scale = scene.tile_font_scale(tile_id);
        if (scale - 1.0).abs() < f32::EPSILON {
            return base_font_px;
        }
        let (min, max) = self.portal_font_clamp_range();
        scale_portal_font_px(base_font_px, scale, min, max)
    }

    /// Resolve the legible font clamp range `(min, max)` from the display token
    /// map, falling back to the tze_hud_scene defaults (hud-ovjxu.1). Shared by
    /// the render path and the truncation-prime path so both clamp identically.
    pub(super) fn portal_font_clamp_range(&self) -> (f32, f32) {
        let min = self
            .token_map
            .get(tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_TOKEN)
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(tze_hud_scene::types::PORTAL_TEXT_MIN_FONT_PX_DEFAULT);
        let max = self
            .token_map
            .get(tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_TOKEN)
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(tze_hud_scene::types::PORTAL_TEXT_MAX_FONT_PX_DEFAULT);
        (min, max)
    }

    #[allow(clippy::only_used_in_recursion)]
    // All arguments are required: node identity, tile geometry, scene reference,
    // scroll offsets, tail-follow state, and the output accumulator.
    // This mirrors the parameter shape of the free-function twin
    // `collect_ellipsis_text_items_from_node` which also carries the same lint.
    #[allow(clippy::too_many_arguments)]
    fn collect_text_items_from_node(
        &self,
        node_id: SceneId,
        tile: &Tile,
        scene: &SceneGraph,
        scroll_x: f32,
        scroll_y: f32,
        at_tail: bool,
        transcript_measure_px: f32,
        part_index: Option<&[PortalPart]>,
        active_part: Option<&PortalPart>,
        items: &mut Vec<TextItem>,
    ) {
        let node = match scene.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };

        // The part governing this node: its own declared part, else the one
        // inherited from an ancestor container part (hud-s4lrw, PR #1099 Codex
        // P2). Threaded into children below so a container part scopes its whole
        // subtree.
        let effective_part = resolve_effective_part(part_index, node_id, active_part);
        let (node_scroll_x, node_scroll_y) = if node_uses_display_tile_scroll(scene, tile, node_id)
        {
            (scroll_x, scroll_y)
        } else {
            (0.0, 0.0)
        };

        if let NodeData::TextMarkdown(tm) = &node.data {
            // hud-pd9bp: when this node is a `NodeLayout::VerticalFlow` child, its
            // glyph origin moves to the runtime-resolved stacked y. The
            // `from_text_markdown_*` constructors add `node.bounds.y` internally,
            // so fold the delta `(resolved_y - tm.bounds.y)` into the `tile_y`
            // argument below — the constructor then nets exactly `resolved_y`.
            // Zero for Absolute nodes (absent from the map) → byte-identical, and
            // it matches the `effective_y` substitution in `render_node`'s
            // geometry pass so the background quad and glyphs stay aligned. The
            // truncation cache is y-independent (TruncationKey excludes y), so the
            // ellipsis-prime walk needs no matching change.
            let flow_dy = self
                .tile_flow_offsets
                .get(&node_id)
                .map(|resolved_y| resolved_y - tm.bounds.y)
                .unwrap_or(0.0);
            // Subtract scroll offset only for document content. Geometry-only
            // resident portal surfaces keep their frame/header/pane labels fixed;
            // this mirrors `render_node` and the rounded-rect draw-list.
            //
            // Phase-1 (hud-5jbra.2): try the markdown cache first.  Use
            // `get_by_key` with a precomputed key (O(1) — no re-hash on the
            // frame path).  The cache is primed by `prime_markdown_cache` on
            // every scene-version change (commit-time prime, hud-380dl).
            //
            // color_runs bypass: `from_text_markdown_cached` uses markdown
            // plain_text as the text base and discards `node.color_runs`.
            // `color_runs` byte offsets are against the *raw* content (not the
            // stripped plain_text), so the cache path is incompatible.  Skip
            // it when the node carries inline color_runs and fall through to
            // `from_text_markdown_node` which preserves them correctly.
            //
            // Key lookup: the per-node key cache (node_key_cache) is populated
            // by prime_markdown_cache at content-commit time, so the frame path
            // never calls MarkdownCache::compute_key (which hashes the full
            // content string).  The lookup is a 32-byte HashMap read — O(1).
            // If the key is absent (first frame before any prime, or a node
            // added mid-frame) the fallback re-computes the key once and
            // consults the markdown_cache directly.  (hud-gpqde)
            //
            // Cache-miss fallback (hud-xcp9b): if the markdown cache is cold
            // (first frame before any commit-time prime, or a node added
            // mid-frame after prime_markdown_cache ran), we parse the content
            // inline on the spot and use from_text_markdown_cached so markdown
            // structure and styling are preserved.  This honors the 'never
            // dropped' contract (spec task 2.2).  A tracing::warn! fires so the
            // miss is observable in production logs.  No debug_assert: the
            // first-frame miss is normal operation, not a bug. [hud-rbf91]
            // In steady-state (commit-time-primed) frames this branch is never taken.
            //
            // Gate on *pixel-bearing* runs, not `is_empty()`: zero-length
            // sentinel runs (lifecycle/stale/at-capacity markers) reference no
            // content bytes, so a node carrying only sentinels stays on the
            // cached/styled markdown path instead of dropping to the lossy
            // raw-content constructor.  Only genuine pixel runs (start < end)
            // force the legacy path. (hud-9v3t6)
            // Per-tile token scope (hud-3ryie): a portal surface (scrollable
            // tile) resolves the portal token set; any other markdown surface
            // resolves the generic set.  Used for the cache-miss key/inline
            // parse so it agrees with the scope `prime_markdown_cache` used.
            let md_tokens = if scene.tile_scroll_config(tile.id).is_some() {
                &self.markdown_tokens
            } else {
                &self.markdown_tokens_generic
            };
            let mut item = if !crate::text::markdown_node_has_pixel_runs(tm) {
                let content_key = self
                    .node_key_cache
                    .get(&node_id)
                    .copied()
                    .unwrap_or_else(|| {
                        crate::markdown::MarkdownCache::compute_key(&tm.content, md_tokens)
                    });
                // Load the current snapshot lock-free (hud-33qo7).  Pinned by the
                // returned Arc for the duration of this lookup.
                let markdown_cache = self.markdown_cache();
                if let Some(parsed) = markdown_cache.get_by_key(&content_key) {
                    TextItem::from_text_markdown_cached(
                        tm,
                        tile.bounds.x - node_scroll_x,
                        tile.bounds.y - node_scroll_y + flow_dy,
                        parsed,
                    )
                } else {
                    // Cache miss: parse inline (non-lossy) so styling is
                    // preserved.  This is the expected path on the first frame
                    // before any commit-time prime and for nodes added mid-frame
                    // after prime_markdown_cache ran — both are normal operation.
                    // In steady-state (commit-time-primed) frames this branch is
                    // never taken.  The warn! makes the miss observable in
                    // production logs without panicking. [hud-rbf91]
                    //
                    // Counted (hud-u4lq2): a growing count past the first
                    // post-prime frame means the primer's background result
                    // never landed and this "rare" fallback became the
                    // permanent per-frame path — see `markdown_cache_miss_count`.
                    self.markdown_cache_miss_count
                        .set(self.markdown_cache_miss_count.get() + 1);
                    tracing::warn!(
                        node_id = ?node_id,
                        content_len = tm.content.len(),
                        "markdown cache miss on render path — expected commit-time prime \
                         (hud-xcp9b); parsing inline to preserve styling [hud-380dl]"
                    );
                    // Self-healing fallback cache (hud-u4lq2 defense-in-depth):
                    // check the small side cache before paying a fresh parse.
                    // Whatever caused the primer's authoritative snapshot to
                    // miss this key, we must not re-pay the full parse cost
                    // every single frame — see `Compositor::markdown_fallback_cache`.
                    // Scope the immutable `Ref` to this block so it is
                    // dropped before the `else` branch below might need
                    // `borrow_mut()`.  The scrutinee of `if let ... else`
                    // has its temporary lifetime extended to the end of the
                    // WHOLE if/else (not just the `then` arm) — leaving the
                    // `.borrow()` call directly in the `if let` condition
                    // would keep the `Ref` alive into the `else` arm and
                    // panic with `already borrowed: BorrowMutError` on
                    // every genuine fallback-cache miss. Confirmed via a
                    // standalone repro during hud-u4lq2 review.
                    let cache_hit = {
                        self.markdown_fallback_cache
                            .borrow()
                            .get(&content_key)
                            .map(|parsed| {
                                TextItem::from_text_markdown_cached(
                                    tm,
                                    tile.bounds.x - node_scroll_x,
                                    tile.bounds.y - node_scroll_y + flow_dy,
                                    parsed,
                                )
                            })
                    };
                    if let Some(item) = cache_hit {
                        item
                    } else {
                        let parsed = crate::markdown::parse_markdown_subset(&tm.content, md_tokens);
                        let item = TextItem::from_text_markdown_cached(
                            tm,
                            tile.bounds.x - node_scroll_x,
                            tile.bounds.y - node_scroll_y + flow_dy,
                            &parsed,
                        );
                        self.markdown_fallback_cache
                            .borrow_mut()
                            .insert(content_key, parsed);
                        item
                    }
                }
            } else {
                // Pixel-bearing color_runs present: use the legacy path that
                // preserves raw content byte offsets.  The markdown cache is
                // intentionally bypassed here.
                TextItem::from_text_markdown_node(
                    tm,
                    tile.bounds.x - node_scroll_x,
                    tile.bounds.y - node_scroll_y + flow_dy,
                )
            };
            // Viewer-local whole-portal resize font scaling (hud-ovjxu.1): scale
            // the shaped font by the tile's resize multiplier, clamped to the
            // token legible range. No-op at the default scale. Must mirror the
            // ellipsis collector so truncation keys match this shaped font.
            item.font_size_px = self.scaled_portal_font(item.font_size_px, tile.id, scene);
            // Per-part transcript scope (hud-s4lrw): under the multi-node model
            // only the declared `Transcript` part receives the optimal-measure
            // clamp + tail-anchored ellipsis; composer/header/frame part nodes
            // wrap to their own width and stay head-anchored. `None` (legacy
            // single-node portal / non-portal markdown) → every node inherits
            // the caller's tile-level gate, byte-identical to pre-promotion.
            let node_is_transcript = node_gets_transcript_treatment(part_index, effective_part);
            let node_measure_px = if node_is_transcript {
                transcript_measure_px
            } else {
                0.0
            };
            // Optimal-measure clamp (hud-rivcy): cap the transcript's effective
            // wrap width to `portal.transcript.max_measure_px` (0 = unbounded).
            // Applied BEFORE the clip computation below so `clip_bounds_width`
            // (derived from `item.bounds_width`) tracks the clamped measure, and
            // BEFORE the item is shaped so the wrap width and truncation key both
            // reflect the cap — matching the prime path in
            // `collect_ellipsis_text_items_from_node`.
            item.bounds_width = clamp_transcript_measure(item.bounds_width, node_measure_px);
            // Override viewport for at-tail Ellipsis tiles (hud-lu50e).
            // The prime path (collect_ellipsis_text_items_from_node) already
            // primes TailAnchored keys for these tiles; the per-frame key must
            // match or `prepare_text_items` will miss the primed entry and fall
            // back to inline head-anchored truncation — always showing the head
            // of the content instead of the newest lines. Gated to the
            // transcript part so a bounded composer never tail-follows.
            if node_is_transcript && at_tail && tm.overflow == TextOverflow::Ellipsis {
                item.viewport = crate::overflow::TruncationViewport::TailAnchored;
            }
            // Clip envelope (hud-s4lrw): the drawn content is clamped to the
            // tile viewport and — for a declared portal part — additionally to
            // that part's band, so one part's overflow can never paint over a
            // sibling part's region (e.g. a long transcript over the composer
            // strip). The band is a *clip* only: it does not alter
            // `bounds_width`, so the shaped-buffer / truncation caches stay
            // keyed exactly as before. `None` reproduces the legacy content∩tile
            // clip.
            let unscrolled_x = item.pixel_x + node_scroll_x;
            let unscrolled_y = item.pixel_y + node_scroll_y;
            let part_band = portal_part_clip_band(effective_part, tile);
            let (clip_x, clip_y, clip_w, clip_h) = crate::text::portal_part_clip_rect(
                (
                    unscrolled_x,
                    unscrolled_y,
                    item.bounds_width,
                    item.bounds_height,
                ),
                (
                    tile.bounds.x,
                    tile.bounds.y,
                    tile.bounds.width,
                    tile.bounds.height,
                ),
                part_band,
            );
            item.clip_pixel_x = clip_x;
            item.clip_pixel_y = clip_y;
            item.clip_bounds_width = clip_w;
            item.clip_bounds_height = clip_h;

            // Per-segment streaming-reveal fade (hud-bl7yi), routed by node
            // identity (hud-g8xpg). Reveal state is keyed per `(tile, node)`, so
            // look up *this* node's reveal directly instead of matching by
            // plain-text in a tile-wide post-pass. Text-only matching cross-faded
            // a settled node onto a revealing sibling that laid out the same
            // string (e.g. two transcript lines both "ok"), and picked
            // non-deterministically between two same-text reveals. The O(1)
            // lookup here fades each glyph under at most its own node's reveal.
            // Steady nodes (`is_revealing() == false`) are untouched, so their
            // draw output stays byte-identical to the no-reveal path.
            if let Some(reveal) = self.portal_tile_reveal_states.get(&(tile.id, node_id)) {
                if reveal.is_revealing() {
                    let ramp = super::easing::StreamFadeRamp::default();
                    apply_portal_reveal_fade(&mut item, reveal, ramp);
                }
            }

            // Transcript-tail streaming cursor (hud-zlq2v): when the node carries
            // the streaming-cursor tail marker, paint the trailing cursor glyph in
            // its token-resolved accent at the precise, layout-measured tail of the
            // latest agent turn. Derived per-render from the marker (which the
            // adapter emits only while `agent_activity_active`), so it advances with
            // each streaming publish and clears on quiesce (kbm80) with no separate
            // per-frame timer. Applied AFTER the reveal fade so a freshly-revealed
            // tail still shows the cursor accent; a no-op unless the marker is
            // present, keeping steady tiles byte-identical to the no-cursor path.
            if let Some(color) = crate::text::markdown_node_tail_cursor_color(tm) {
                apply_tail_streaming_cursor(&mut item, color);
            }

            items.push(item);
        }

        for child_id in &node.children {
            self.collect_text_items_from_node(
                *child_id,
                tile,
                scene,
                scroll_x,
                scroll_y,
                at_tail,
                transcript_measure_px,
                part_index,
                effective_part,
                items,
            );
        }
    }
}

// ─── Module-level text helpers ────────────────────────────────────────────────

/// Core math for viewer-local resize font scaling (hud-ovjxu.1): `base * scale`
/// clamped to the legible `[min, max]` range. A `scale` of 1.0 returns `base`
/// unchanged (adapter font untouched). Guards an inverted `(min, max)` pair so
/// `clamp` never panics. Shared by the render path (`scaled_portal_font`) and the
/// truncation-prime path so both produce identical shaped font sizes — the
/// truncation key includes `font_size_px`, so any divergence would miss the cache.
pub(super) fn scale_portal_font_px(base_font_px: f32, scale: f32, min: f32, max: f32) -> f32 {
    if (scale - 1.0).abs() < f32::EPSILON {
        return base_font_px;
    }
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    (base_font_px * scale).clamp(lo, hi)
}

/// Clamp a transcript node's wrapping measure to the optimal-measure cap
/// (`portal.transcript.max_measure_px`, hud-rivcy).
///
/// A portal transcript wraps its body text to `bounds_width` (the node width).
/// On a wide surface that yields overlong, hard-to-read lines; this cap holds a
/// comfortable line length by shrinking the effective wrap measure.
///
/// `max_measure_px == 0.0` means **unbounded** — returns `bounds_width`
/// unchanged, preserving today's behavior (no regression on the default
/// profile). A positive `max_measure_px` returns `min(bounds_width,
/// max_measure_px)`: the cap only ever *narrows* the measure, never widens a
/// node that is already narrower than the cap.
///
/// The same clamp is applied on BOTH the frame render path
/// (`collect_text_items_from_node`) and the truncation-prime path
/// (`collect_ellipsis_text_items_from_node`) so the primed truncation key
/// (which includes `bounds_width`) matches what the render path shapes.
pub(super) fn clamp_transcript_measure(bounds_width: f32, max_measure_px: f32) -> f32 {
    if max_measure_px > 0.0 {
        bounds_width.min(max_measure_px)
    } else {
        bounds_width
    }
}

/// Borrow a tile's declared [`PortalSurface`](tze_hud_scene::types::PortalSurface)
/// part list for per-part render consumption, if any (hud-s4lrw).
///
/// Returns the surface's `&[PortalPart]` so the text collectors can resolve
/// **per-part** overflow scope (transcript vs. composer/header/…) and the
/// per-part clip envelope by a linear scan — the part count is bounded at
/// [`PORTAL_MAX_PARTS`](tze_hud_scene::types::PORTAL_MAX_PARTS) (8), so a slice
/// scan is both allocation-free on the per-frame hot path and faster than a
/// `HashMap` for such small N (PR #1099 review).
///
/// Returns `None` when the tile carries no surface **or** the surface has no
/// materialized part nodes yet. In both cases, text overflow/clip treatment
/// keeps the pre-promotion tile-level fallback (the whole scrollable tile is
/// treated as one transcript). Geometry-only resident surfaces still use their
/// declared bounds to keep chrome fixed while document text scrolls; that
/// translation policy lives in `tile_render::node_uses_display_tile_scroll`.
pub(super) fn portal_part_index(scene: &SceneGraph, tile_id: SceneId) -> Option<&[PortalPart]> {
    let surface = scene.portal_surface(tile_id)?;
    if surface.parts.iter().any(|p| p.node.is_some()) {
        Some(&surface.parts)
    } else {
        None
    }
}

/// Resolve the [`PortalPart`] whose scope governs `node_id`: the part that
/// directly backs it, or — so a part whose `node` is a *container* (a
/// `SolidColor`/`HitRegion` root with `TextMarkdown` descendants, which the
/// scene contract permits) governs its whole subtree — the `inherited` part
/// carried down from the nearest ancestor part (PR #1099 review, Codex P2).
///
/// A node that directly declares a part overrides any inherited scope. Returns
/// `None` for a node under a surface that is neither a declared part nor a
/// descendant of one, and (trivially) for the legacy no-surface case where
/// `part_index` is `None`.
pub(super) fn resolve_effective_part<'a>(
    part_index: Option<&'a [PortalPart]>,
    node_id: SceneId,
    inherited: Option<&'a PortalPart>,
) -> Option<&'a PortalPart> {
    part_index
        .and_then(|parts| parts.iter().find(|p| p.node == Some(node_id)))
        .or(inherited)
}

/// Resolve whether a node should receive **transcript** overflow treatment
/// (optimal-measure wrap clamp + tail-anchored ellipsis) under the multi-node
/// portal-part model (hud-s4lrw).
///
/// - `part_index == None` (legacy single-node portal, or a non-portal markdown
///   surface): returns `true` so every node inherits the caller's tile-level
///   gate unchanged (the caller passes a `0.0` measure for non-portal tiles, so
///   this is a no-op there and identical to pre-promotion behavior).
/// - `part_index == Some(_)`: only the declared `Transcript` part (or a
///   descendant of it — see [`resolve_effective_part`]) is a transcript;
///   composer/header/frame/… parts and any undeclared node wrap to their own
///   width and stay head-anchored, so a bounded composer never inherits the
///   transcript's tail-follow or measure clamp.
pub(super) fn node_gets_transcript_treatment(
    part_index: Option<&[PortalPart]>,
    effective_part: Option<&PortalPart>,
) -> bool {
    match part_index {
        None => true,
        Some(_) => matches!(
            effective_part.map(|p| p.kind),
            Some(PortalPartKind::Transcript)
        ),
    }
}

/// Absolute-pixel clip band for a node's effective portal part, if any.
///
/// `PortalPart::bounds` is tile-local (origin relative to the host tile), so the
/// band is offset by the tile origin into the same absolute space as the drawn
/// glyphs. Returns `None` for the legacy/no-surface case (and for a node with no
/// effective part) so the caller keeps the plain content∩tile clip.
fn portal_part_clip_band(
    effective_part: Option<&PortalPart>,
    tile: &Tile,
) -> Option<crate::text::ClipBox> {
    let part = effective_part?;
    Some((
        tile.bounds.x + part.bounds.x,
        tile.bounds.y + part.bounds.y,
        part.bounds.width,
        part.bounds.height,
    ))
}

/// Apply the per-segment streaming-reveal fade to a portal-tile markdown
/// [`TextItem`], in place (hud-bl7yi).
///
/// Rewrites `item.styled_runs` into a *full-coverage* run list so every laid-out
/// byte carries an explicit alpha, driven by `reveal.alpha_for_byte`:
/// - pre-existing / already-revealed bytes keep their original style at full
///   alpha,
/// - the leading (currently-fading) segment is dimmed by the [`StreamFadeRamp`],
/// - not-yet-revealed segments are driven to alpha `0` (laid out, invisible).
///
/// Full coverage matters because unstyled gaps would otherwise render at the
/// item's default color (full alpha) and refuse to fade. Style attributes
/// (weight/italic/monospace/color/size/background) are inherited from the
/// original run covering each slice (last-writer-wins, matching the renderer's
/// run precedence); gaps fall back to the item's base color.
///
/// No-op unless the item lays out exactly the snapshot the reveal was anchored
/// to (guards against truncation/mismatch) and is a cached/styled markdown item
/// (empty `color_runs`). Slices never straddle a breakpoint, so `alpha_for_byte`
/// at the slice start is the alpha for the whole slice.
///
/// [`StreamFadeRamp`]: super::easing::StreamFadeRamp
fn apply_portal_reveal_fade(
    item: &mut TextItem,
    reveal: &super::draw_cmds::PortalTileStreamReveal,
    ramp: super::easing::StreamFadeRamp,
) {
    use crate::text::{StyledRunItem, apply_opacity_to_color};

    if !reveal.is_revealing() {
        return;
    }
    // The reveal's offsets index the plain-text snapshot it was built from; only
    // apply when this item lays out that exact text.
    if item.text.as_ref() != reveal.plain_text.as_ref() {
        return;
    }
    // Pixel-bearing color-run items use raw-content offsets (never reached for
    // portal cached markdown, but stay safe).
    if !item.color_runs.is_empty() {
        return;
    }
    let n = item.text.len();
    if n == 0 {
        return;
    }

    // Slice boundaries: existing run edges ∪ breakpoints ∪ reveal_start ∪
    // endpoints. Every breakpoint is a boundary, so no slice crosses a segment
    // and the alpha is constant across each slice.
    let mut bounds: Vec<usize> =
        Vec::with_capacity(item.styled_runs.len() * 2 + reveal.breakpoints.len() + 3);
    bounds.push(0);
    bounds.push(n);
    let push_bound = |bounds: &mut Vec<usize>, off: usize| {
        let off = off.min(n);
        if item.text.is_char_boundary(off) {
            bounds.push(off);
        }
    };
    push_bound(&mut bounds, reveal.reveal_start);
    for &b in &reveal.breakpoints {
        push_bound(&mut bounds, b);
    }
    for run in item.styled_runs.iter() {
        push_bound(&mut bounds, run.start_byte);
        push_bound(&mut bounds, run.end_byte);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let base = item.color;
    let mut faded: Vec<StyledRunItem> = Vec::with_capacity(bounds.len());
    for win in bounds.windows(2) {
        let (s, e) = (win[0], win[1]);
        if s >= e {
            continue;
        }
        let alpha = reveal.alpha_for_byte(s, ramp);
        // Last original run covering `s` supplies the style (last-writer-wins).
        let style = item
            .styled_runs
            .iter()
            .rev()
            .find(|r| r.start_byte <= s && s < r.end_byte);
        let src_color = style.and_then(|r| r.color).unwrap_or(base);
        faded.push(StyledRunItem {
            start_byte: s,
            end_byte: e,
            weight: style.and_then(|r| r.weight),
            italic: style.map(|r| r.italic).unwrap_or(false),
            monospace: style.map(|r| r.monospace).unwrap_or(false),
            color: Some(apply_opacity_to_color(src_color, alpha)),
            background_color: style
                .and_then(|r| r.background_color)
                .map(|c| apply_opacity_to_color(c, alpha)),
            size_scale: style.and_then(|r| r.size_scale),
            fill_line_width: false,
        });
    }
    item.styled_runs = faded.into_boxed_slice();
}

/// The transcript-tail streaming-cursor glyph (U+258D LEFT THREE EIGHTHS BLOCK).
///
/// Mirrors the trailing glyph in `PORTAL_STREAMING_CURSOR_GLYPH` (`" ▍"`) that
/// `tze_hud_projection::resident_grpc` appends to the streaming transcript body.
/// Kept in sync with that constant: if the adapter's cursor glyph changes, change
/// this too (there is no shared crate for the literal).
const STREAMING_CURSOR_GLYPH: char = '\u{258D}';

/// Byte range of the streaming-cursor glyph at the tail of the latest agent turn,
/// located in the LAID-OUT (post-markdown-strip) plain text (hud-zlq2v).
///
/// Measured against the rendered text — the last occurrence of the cursor glyph —
/// never against raw-markdown byte arithmetic, so an emphasized or multibyte tail
/// stays intact. `rfind` returns a char boundary and `len_utf8()` yields the exact
/// glyph end, so the range is always a valid UTF-8 slice. `None` when the glyph is
/// absent (cue quiesced, or the tail was truncated away).
fn tail_cursor_glyph_range(text: &str) -> Option<(usize, usize)> {
    let start = text.rfind(STREAMING_CURSOR_GLYPH)?;
    Some((start, start + STREAMING_CURSOR_GLYPH.len_utf8()))
}

/// Paint the transcript-tail streaming-cursor glyph in its token accent (hud-zlq2v).
///
/// Overlays a color-only styled run on exactly the trailing cursor glyph's byte
/// range (located via [`tail_cursor_glyph_range`] on the laid-out text), so the
/// cursor renders in `portal.streaming_cursor.color` rather than the transcript
/// text color. This is the promotion-era replacement for the old byte-0
/// zero-length sentinel, which the cached-markdown path discarded — the cursor
/// never actually took its accent before.
///
/// Cached-markdown only: like [`apply_portal_reveal_fade`], bail if the item
/// carries pixel-bearing `color_runs` (the legacy raw-content path uses different
/// offsets). Existing styled runs (markdown weight/italic/color) are preserved
/// exactly; only the cursor slice's color is overridden. A no-op when the glyph
/// is absent, keeping non-streaming tiles byte-identical.
fn apply_tail_streaming_cursor(item: &mut TextItem, color: [u8; 4]) {
    use crate::text::StyledRunItem;

    if !item.color_runs.is_empty() {
        return;
    }
    let n = item.text.len();
    let Some((cur_start, cur_end)) = tail_cursor_glyph_range(&item.text) else {
        return;
    };

    // Rebuild styled_runs, splitting at the cursor edges so the accent overlays
    // exactly the glyph. Slices outside the cursor preserve their original run
    // (or stay a gap → base color); slices inside get the accent color while
    // inheriting any weight/italic/monospace so the caret matches its context.
    let mut bounds: Vec<usize> = Vec::with_capacity(item.styled_runs.len() * 2 + 4);
    bounds.push(0);
    bounds.push(n);
    let mut push_bound = |off: usize| {
        if off <= n && item.text.is_char_boundary(off) {
            bounds.push(off);
        }
    };
    push_bound(cur_start);
    push_bound(cur_end);
    for run in item.styled_runs.iter() {
        push_bound(run.start_byte);
        push_bound(run.end_byte);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut rebuilt: Vec<StyledRunItem> = Vec::with_capacity(bounds.len());
    for win in bounds.windows(2) {
        let (s, e) = (win[0], win[1]);
        if s >= e {
            continue;
        }
        // Original run covering `s` (last-writer-wins), if any.
        let style = item
            .styled_runs
            .iter()
            .rev()
            .find(|r| r.start_byte <= s && s < r.end_byte);
        let in_cursor = s >= cur_start && e <= cur_end;
        if !in_cursor && style.is_none() {
            // Preserve gaps (base color) — do not synthesize a run.
            continue;
        }
        rebuilt.push(StyledRunItem {
            start_byte: s,
            end_byte: e,
            weight: style.and_then(|r| r.weight),
            italic: style.map(|r| r.italic).unwrap_or(false),
            monospace: style.map(|r| r.monospace).unwrap_or(false),
            color: if in_cursor {
                Some(color)
            } else {
                style.and_then(|r| r.color)
            },
            background_color: style.and_then(|r| r.background_color),
            size_scale: style.and_then(|r| r.size_scale),
            fill_line_width: false,
        });
    }
    item.styled_runs = rebuilt.into_boxed_slice();
}

/// Collect [`TextItem`]s for all `TextOverflow::Ellipsis` nodes reachable from
/// `node_id`, without scroll offset (prime-time geometry).
///
/// This is a free function (not a method) to avoid a split-borrow conflict in
/// [`super::Compositor::prime_truncation_cache`], where `self.text_rasterizer` is
/// borrowed mutably while the markdown snapshot (loaded from the primer) and
/// `self.node_key_cache` are read immutably.
///
/// The geometry produced here is identical to what `collect_text_items_from_node`
/// produces at scroll_x=0, scroll_y=0 (valid because truncation is geometry-
/// dependent only on `bounds_width` / `bounds_height`, which are scroll-invariant).
///
/// `at_tail`: whether the tile owning these nodes is currently in follow-tail/at-tail
/// mode.  Callers must obtain this value via [`super::tile_at_tail_for_ellipsis`] to
/// guarantee alignment with the per-frame key built by `collect_text_items_from_node`.
/// `true` → `TailAnchored` truncation (spec §3.2 — newest lines visible);
/// `false` → `HeadAnchored` (spec §3.3 — viewport stability after user scroll-back).
///
/// `markdown_tokens`: used for the cache-miss non-lossy inline parse fallback
/// (hud-xcp9b); in steady-state this argument is never consumed.
// All arguments are required: node identity, scene reference, tile position,
// tail-follow state, markdown cache, node key cache, token store, and the output
// accumulator.  The parameter set mirrors the sibling method
// `collect_text_items_from_node`; no subset can be omitted.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_ellipsis_text_items_from_node(
    node_id: SceneId,
    scene: &SceneGraph,
    tile_id: SceneId,
    tile_x: f32,
    tile_y: f32,
    at_tail: bool,
    markdown_cache: &crate::markdown::MarkdownCache,
    node_key_cache: &HashMap<SceneId, [u8; 32]>,
    markdown_tokens: &crate::markdown::MarkdownTokens,
    font_clamp: (f32, f32),
    transcript_measure_px: f32,
    part_index: Option<&[PortalPart]>,
    active_part: Option<&PortalPart>,
    items: &mut Vec<TextItem>,
) {
    let node = match scene.nodes.get(&node_id) {
        Some(n) => n,
        None => return,
    };

    // Effective part for this node (own declared part, else inherited from an
    // ancestor container part) — must mirror the render path exactly so the
    // primed truncation keys match (hud-s4lrw, PR #1099 Codex P2).
    let effective_part = resolve_effective_part(part_index, node_id, active_part);

    if let NodeData::TextMarkdown(tm) = &node.data {
        if tm.overflow == tze_hud_scene::types::TextOverflow::Ellipsis {
            // color_runs bypass: same as in collect_text_items_from_node —
            // the markdown cache path drops color_runs, so skip it when the
            // node carries inline color_runs.
            //
            // Key lookup: use the per-node key cache (populated at prime time)
            // to avoid re-hashing content on the frame path.  Falls back to
            // compute_key only if the entry is absent (pre-prime first frame).
            // (hud-gpqde)
            //
            // Cache-miss fallback (hud-xcp9b): same non-lossy inline-parse
            // strategy as collect_text_items_from_node.  See that site for the
            // full rationale.  In steady-state (commit-time-primed) frames this
            // branch is never taken.
            //
            // Gate on *pixel-bearing* runs, not `is_empty()`: zero-length
            // sentinel runs carry metadata only and must not force the lossy
            // raw-content path. (hud-9v3t6)
            let mut item = if !crate::text::markdown_node_has_pixel_runs(tm) {
                let content_key = node_key_cache.get(&node_id).copied().unwrap_or_else(|| {
                    // Key with the tile-scoped tokens the caller selected
                    // (hud-3ryie) so it agrees with prime_markdown_cache.
                    crate::markdown::MarkdownCache::compute_key(&tm.content, markdown_tokens)
                });
                if let Some(parsed) = markdown_cache.get_by_key(&content_key) {
                    TextItem::from_text_markdown_cached(tm, tile_x, tile_y, parsed)
                } else {
                    // Cache miss: same non-lossy inline-parse strategy as
                    // collect_text_items_from_node — normal on first frame /
                    // mid-frame node add.  warn! provides observability. [hud-rbf91]
                    tracing::warn!(
                        node_id = ?node_id,
                        content_len = tm.content.len(),
                        "markdown cache miss on ellipsis render path — expected commit-time prime \
                         (hud-xcp9b); parsing inline to preserve styling [hud-380dl]"
                    );
                    let parsed =
                        crate::markdown::parse_markdown_subset(&tm.content, markdown_tokens);
                    TextItem::from_text_markdown_cached(tm, tile_x, tile_y, &parsed)
                }
            } else {
                TextItem::from_text_markdown_node(tm, tile_x, tile_y)
            };
            // Apply the SAME viewer-local resize font scaling as the render path
            // (hud-ovjxu.1) so the primed truncation key (which includes
            // font_size_px) matches what collect_text_items_from_node shapes.
            item.font_size_px = scale_portal_font_px(
                item.font_size_px,
                scene.tile_font_scale(tile_id),
                font_clamp.0,
                font_clamp.1,
            );
            // Per-part transcript scope (hud-s4lrw): mirror the render path's
            // `node_gets_transcript_treatment` gate so the primed key (measure +
            // viewport) matches exactly. Under a multi-node surface only the
            // `Transcript` part is clamped/tail-anchored; `None` → legacy
            // tile-level behavior.
            let node_is_transcript = node_gets_transcript_treatment(part_index, effective_part);
            let node_measure_px = if node_is_transcript {
                transcript_measure_px
            } else {
                0.0
            };
            // Optimal-measure clamp (hud-rivcy): apply the SAME transcript wrap
            // cap as the render path so this primed truncation key (which
            // includes `bounds_width`) matches what `collect_text_items_from_node`
            // shapes.
            item.bounds_width = clamp_transcript_measure(item.bounds_width, node_measure_px);
            // Override viewport based on the tile's follow-tail state (transcript
            // part only, matching the render path).
            if node_is_transcript && at_tail {
                item.viewport = crate::overflow::TruncationViewport::TailAnchored;
            }
            items.push(item);
        }
    }

    for child_id in &node.children {
        collect_ellipsis_text_items_from_node(
            *child_id,
            scene,
            tile_id,
            tile_x,
            tile_y,
            at_tail,
            markdown_cache,
            node_key_cache,
            markdown_tokens,
            font_clamp,
            transcript_measure_px,
            part_index,
            effective_part,
            items,
        );
    }
}

#[cfg(test)]
mod portal_reveal_render_tests {
    use super::super::draw_cmds::{PortalTileStreamReveal, derive_word_breakpoints};
    use super::super::easing::{Easing, StreamFadeRamp};
    use super::apply_portal_reveal_fade;
    use crate::markdown::{ParsedMarkdown, StyleAttr, StyledSpan};
    use crate::text::TextItem;
    use std::sync::Arc;

    // hud-ovjxu.1: resize font-scale clamp math (pure — no GPU).
    #[test]
    fn scale_portal_font_px_scales_and_clamps() {
        use super::scale_portal_font_px;
        // Grow: base * scale, within the legible range.
        assert!((scale_portal_font_px(16.0, 1.5, 9.0, 48.0) - 24.0).abs() < 1e-4);
        // Grow past the max clamps to max.
        assert!((scale_portal_font_px(16.0, 10.0, 9.0, 48.0) - 48.0).abs() < 1e-4);
        // Shrink past the min clamps to min (spec: font holds at the floor).
        assert!((scale_portal_font_px(16.0, 0.1, 9.0, 48.0) - 9.0).abs() < 1e-4);
        // Scale exactly 1.0 returns the base untouched (adapter font).
        assert!((scale_portal_font_px(16.0, 1.0, 9.0, 48.0) - 16.0).abs() < 1e-4);
        // Inverted (min, max) pair is tolerated (no clamp panic).
        assert!((scale_portal_font_px(16.0, 2.0, 48.0, 9.0) - 32.0).abs() < 1e-4);
    }
    use tze_hud_scene::types::{FontFamily, Rect, Rgba, TextAlign, TextMarkdownNode, TextOverflow};

    fn plain_attr() -> StyleAttr {
        StyleAttr {
            weight: None,
            italic: false,
            monospace: false,
            color: None,
            background_color: None,
            size_scale: None,
        }
    }

    /// Build a cached markdown `TextItem` whose laid-out text == `plain`, with a
    /// single bold span over the leading `bold_prefix` bytes.
    fn markdown_item(plain: &str, bold_prefix: usize) -> TextItem {
        let parsed = ParsedMarkdown {
            plain_text: Arc::from(plain),
            spans: vec![StyledSpan {
                start_byte: 0,
                end_byte: bold_prefix,
                attr: StyleAttr {
                    weight: Some(700),
                    ..plain_attr()
                },
            }],
            code_panels: vec![],
            list_items: vec![],
            thematic_breaks: vec![],
            line_height_multiplier: 1.4,
        };
        let node = TextMarkdownNode {
            content: plain.to_owned(),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            color_runs: Box::default(),
        };
        TextItem::from_text_markdown_cached(&node, 0.0, 0.0, &parsed)
    }

    /// Alpha (0..=255) of the styled run covering `byte`, or `None` if uncovered.
    fn alpha_at(item: &TextItem, byte: usize) -> Option<u8> {
        item.styled_runs
            .iter()
            .rev()
            .find(|r| r.start_byte <= byte && byte < r.end_byte)
            .and_then(|r| r.color)
            .map(|c| c[3])
    }

    #[test]
    fn fade_dims_leading_segment_and_hides_unrevealed_tail() {
        let plain = "old new1 new2";
        let item = markdown_item(plain, 3); // "old" bold
        let start = super::super::draw_cmds::common_prefix_len("old ", plain);
        let bps = derive_word_breakpoints(plain, start);
        let reveal = PortalTileStreamReveal::new(plain.into(), start, bps);
        let ramp = StreamFadeRamp::new(Easing::Linear);

        let mut faded = item.clone();
        apply_portal_reveal_fade(&mut faded, &reveal, ramp);

        // Pre-existing prefix stays fully opaque AND keeps its bold weight.
        assert_eq!(alpha_at(&faded, 0), Some(255), "prefix must stay opaque");
        let prefix_run = faded
            .styled_runs
            .iter()
            .find(|r| r.start_byte == 0 && r.end_byte > 0)
            .unwrap();
        assert_eq!(prefix_run.weight, Some(700), "prefix style must survive");

        // Leading segment (first appended word) is dimmed at t=0 (alpha 0).
        assert_eq!(
            alpha_at(&faded, start),
            Some(0),
            "leading starts transparent"
        );
        // Not-yet-revealed tail word is fully hidden.
        assert_eq!(alpha_at(&faded, plain.len() - 1), Some(0), "tail hidden");
    }

    #[test]
    fn fade_alpha_increases_with_reveal_progress() {
        let plain = "x ab cd";
        let item = markdown_item(plain, 0);
        let bps = derive_word_breakpoints(plain, 0);
        let mut reveal = PortalTileStreamReveal::new(plain.into(), 0, bps);
        let ramp = StreamFadeRamp::new(Easing::Linear);

        let mut a = item.clone();
        apply_portal_reveal_fade(&mut a, &reveal, ramp);
        let alpha0 = alpha_at(&a, 0).unwrap();

        reveal.advance();
        reveal.advance();
        let mut b = item.clone();
        apply_portal_reveal_fade(&mut b, &reveal, ramp);
        let alpha1 = alpha_at(&b, 0).unwrap();

        assert!(
            alpha1 > alpha0,
            "leading-segment draw alpha must rise with reveal progress: {alpha1} > {alpha0}"
        );
    }

    #[test]
    fn settled_reveal_leaves_item_untouched() {
        let plain = "old new1 new2";
        let item = markdown_item(plain, 3);
        // A settled (fully-revealed) reveal must be a no-op: steady tiles render
        // identically to the no-reveal path (deliverable #3).
        let reveal = PortalTileStreamReveal::settled(plain.into());
        let ramp = StreamFadeRamp::default();

        let mut after = item.clone();
        apply_portal_reveal_fade(&mut after, &reveal, ramp);
        assert_eq!(
            format!("{:?}", item.styled_runs),
            format!("{:?}", after.styled_runs),
            "settled reveal must not alter styled runs"
        );
    }

    // ── Transcript-tail streaming cursor (hud-zlq2v) ──────────────────────────

    use super::{apply_tail_streaming_cursor, tail_cursor_glyph_range};
    use crate::text::markdown_node_tail_cursor_color;

    /// sRGB color of the styled run covering `byte`, or `None` if uncovered.
    fn color_at(item: &TextItem, byte: usize) -> Option<[u8; 4]> {
        item.styled_runs
            .iter()
            .rev()
            .find(|r| r.start_byte <= byte && byte < r.end_byte)
            .and_then(|r| r.color)
    }

    /// The cursor byte range is located on the LAID-OUT text and is multibyte /
    /// grapheme safe: it targets the LAST `▍`, spans exactly its 3 UTF-8 bytes,
    /// and never splits an adjacent multibyte glyph.
    #[test]
    fn tail_cursor_glyph_range_is_multibyte_safe() {
        // Multibyte content before the cursor; a trailing space then the glyph.
        let text = "héllo 世界 ▍";
        let (s, e) = tail_cursor_glyph_range(text).expect("glyph present");
        assert_eq!(&text[s..e], "▍", "range covers exactly the cursor glyph");
        assert_eq!(e - s, "▍".len(), "range is the glyph's 3 UTF-8 bytes");
        assert!(text.is_char_boundary(s) && text.is_char_boundary(e));
        // Absent glyph → None (quiesced tail).
        assert_eq!(tail_cursor_glyph_range("no cursor here"), None);
        // The LAST occurrence wins if the agent text itself contains a `▍`.
        let two = "a ▍ mid b ▍";
        let (s2, _) = tail_cursor_glyph_range(two).unwrap();
        assert_eq!(s2, two.rfind('▍').unwrap(), "targets the tail-most glyph");
    }

    /// `markdown_node_tail_cursor_color` recognizes ONLY a content-end zero-length
    /// marker (never the byte-0 sentinels) and returns its token color.
    #[test]
    fn tail_cursor_color_reads_content_end_marker_only() {
        use tze_hud_scene::types::TextColorRun;
        let content = "some transcript ▍".to_string();
        let end = content.len() as u32;
        let accent = Rgba {
            r: 0.2,
            g: 0.4,
            b: 0.8,
            a: 1.0,
        };
        let mut node = TextMarkdownNode {
            content: content.clone(),
            bounds: Rect::new(0.0, 0.0, 400.0, 300.0),
            font_size_px: 16.0,
            font_family: FontFamily::SystemSansSerif,
            color: Rgba::WHITE,
            background: None,
            alignment: TextAlign::Start,
            overflow: TextOverflow::Clip,
            // A byte-0 sentinel (e.g. stale/lifecycle) plus the content-end cursor
            // marker — only the latter must be picked up.
            color_runs: Box::from([
                TextColorRun {
                    start_byte: 0,
                    end_byte: 0,
                    color: Rgba::WHITE,
                },
                TextColorRun {
                    start_byte: end,
                    end_byte: end,
                    color: accent,
                },
            ]),
        };
        assert_eq!(
            markdown_node_tail_cursor_color(&node),
            Some(crate::text::rgba_to_srgb_u8(accent)),
            "reads the content-end marker's token color"
        );
        // Without the content-end marker (only a byte-0 sentinel) → None.
        node.color_runs = Box::from([TextColorRun {
            start_byte: 0,
            end_byte: 0,
            color: accent,
        }]);
        assert_eq!(
            markdown_node_tail_cursor_color(&node),
            None,
            "a byte-0 sentinel is not the tail cursor"
        );
    }

    /// `apply_tail_streaming_cursor` recolors exactly the trailing cursor glyph in
    /// the accent, leaves surrounding text and pre-existing markdown styling
    /// untouched, and is a no-op when the glyph is absent.
    #[test]
    fn tail_cursor_recolors_only_the_glyph_and_preserves_styling() {
        let plain = "hello ▍"; // "hello" bold, then space + cursor glyph
        let item = markdown_item(plain, 5);
        let accent = [10u8, 20, 30, 255];

        let mut painted = item.clone();
        apply_tail_streaming_cursor(&mut painted, accent);

        let (cur_s, _) = tail_cursor_glyph_range(plain).unwrap();
        assert_eq!(
            color_at(&painted, cur_s),
            Some(accent),
            "cursor glyph takes the accent color"
        );
        // The bold prefix keeps its weight and does NOT take the accent.
        let prefix_run = painted
            .styled_runs
            .iter()
            .find(|r| r.start_byte == 0 && r.end_byte > 0)
            .unwrap();
        assert_eq!(prefix_run.weight, Some(700), "bold prefix weight survives");
        assert_ne!(color_at(&painted, 0), Some(accent), "prefix not recolored");

        // No glyph → untouched.
        let no_cursor = markdown_item("plain tail", 0);
        let mut same = no_cursor.clone();
        apply_tail_streaming_cursor(&mut same, accent);
        assert_eq!(
            format!("{:?}", no_cursor.styled_runs),
            format!("{:?}", same.styled_runs),
            "absent cursor is a no-op"
        );
    }
}

#[cfg(test)]
mod transcript_measure_tests {
    use super::clamp_transcript_measure;
    use crate::renderer::token_colors::resolve_transcript_max_measure_px;
    use std::collections::HashMap;

    // ── clamp_transcript_measure: pure logic (no GPU, hud-rivcy) ──────────────

    #[test]
    fn clamp_unbounded_returns_full_width() {
        // 0 = unbounded: the measure is left at the full node width so the
        // default profile (default token value "0") is unchanged.
        assert!((clamp_transcript_measure(400.0, 0.0) - 400.0).abs() < 1e-4);
    }

    #[test]
    fn clamp_narrows_when_cap_below_width() {
        // Cap < node width: the effective measure shrinks to the cap.
        assert!((clamp_transcript_measure(400.0, 120.0) - 120.0).abs() < 1e-4);
    }

    #[test]
    fn clamp_never_widens_when_cap_above_width() {
        // Cap >= node width: a node already narrower than the cap is left
        // untouched (the clamp only ever narrows, never widens).
        assert!((clamp_transcript_measure(200.0, 640.0) - 200.0).abs() < 1e-4);
        // Exactly equal is a no-op.
        assert!((clamp_transcript_measure(300.0, 300.0) - 300.0).abs() < 1e-4);
    }

    // ── resolve_transcript_max_measure_px: token resolution (hud-rivcy) ───────

    #[test]
    fn resolve_defaults_to_unbounded_when_absent() {
        let map = HashMap::new();
        assert_eq!(resolve_transcript_max_measure_px(&map), 0.0);
    }

    #[test]
    fn resolve_parses_positive_measure() {
        let mut map = HashMap::new();
        map.insert(
            "portal.transcript.max_measure_px".to_string(),
            "640".to_string(),
        );
        assert!((resolve_transcript_max_measure_px(&map) - 640.0).abs() < 1e-4);
    }

    #[test]
    fn resolve_rejects_negative_and_nonfinite() {
        for bad in ["-1", "nan", "inf", "not-a-number", ""] {
            let mut map = HashMap::new();
            map.insert(
                "portal.transcript.max_measure_px".to_string(),
                bad.to_string(),
            );
            assert_eq!(
                resolve_transcript_max_measure_px(&map),
                0.0,
                "value {bad:?} must fall back to unbounded (0.0)"
            );
        }
    }

    /// End-to-end wrap behavior (CPU shaping, no GPU): a transcript line shaped
    /// at the clamped measure wraps into MORE visual lines than at the full node
    /// width, while the unbounded (0) case shapes to the exact same line count as
    /// the full width — proving the clamp is what drives the tighter wrap and
    /// that `0` is a genuine no-regression pass-through.
    #[test]
    fn clamped_measure_wraps_tighter_than_full_width() {
        let mut fs = crate::fonts::bundled_font_system();
        let font_size_px = 16.0;
        let lhm = 1.4;
        let node_width = 400.0;
        let cap = 120.0;
        let line = "The quick brown fox jumps over the lazy dog near the \
                    riverbank at dawn every single morning without fail.";

        let full_measure = clamp_transcript_measure(node_width, 0.0);
        let capped_measure = clamp_transcript_measure(node_width, cap);
        assert!((full_measure - node_width).abs() < 1e-4);
        assert!((capped_measure - cap).abs() < 1e-4);

        let full_lines =
            crate::text::composer_wrap_line_widths(&mut fs, line, full_measure, font_size_px, lhm)
                .len();
        let capped_lines = crate::text::composer_wrap_line_widths(
            &mut fs,
            line,
            capped_measure,
            font_size_px,
            lhm,
        )
        .len();

        assert!(
            capped_lines > full_lines,
            "clamped measure ({cap}px) must wrap into more lines than full width \
             ({node_width}px); got capped={capped_lines} full={full_lines}"
        );

        // Unbounded (0) must reproduce the full-width wrap exactly — no regression.
        let unbounded_measure = clamp_transcript_measure(node_width, 0.0);
        let unbounded_lines = crate::text::composer_wrap_line_widths(
            &mut fs,
            line,
            unbounded_measure,
            font_size_px,
            lhm,
        )
        .len();
        assert_eq!(
            unbounded_lines, full_lines,
            "unbounded (0) measure must match full-width wrapping exactly"
        );
    }
}
