//! Zone rendering and zone layout methods for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Zone rendering" and "Zone layout"
//! clusters, formerly ~L6381–7083 at plan date) by Step R-7 of the renderer
//! module split (hud-fgryk).  No logic was changed; only visibility modifiers
//! were added where Rust's module-privacy rules require them (listed in the
//! PR body).
//!
//! ## Methods in this file
//!
//! **Zone rendering**:
//! - `render_zone_content` — render backdrop quads + icon textures for all
//!   active zone publications, respecting `RenderingPolicy` and layer filter.
//!
//! **Zone layout**:
//! - `status_bar_card` — the content-hugging status-bar card rect and rows,
//!   shared by backdrop, SDF, text, icon and drag-handle paths.
//! - `stack_slot_height` — per-slot height for single-line Stack zones.
//! - `notification_slot_height` — per-slot height for notification publications
//!   (one-line or two-line depending on `payload.title`).
//! - `per_slot_heights` — per-slot height vector for a full publication slice.
//! - `zone_slot_layout` — authoritative slot-geometry computation (returns
//!   `ZoneSlotLayout`), shared by `collect_text_items`, `render_zone_content`,
//!   and `collect_all_rounded_rect_cmds`.
//! - `slot_offsets` — cumulative y-offsets from per-slot heights and the
//!   inter-card gap.
//! - `resolve_zone_geometry` — converts a `GeometryPolicy` to pixel bounds.

use std::collections::HashMap;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;

use super::Compositor;
use super::draw_cmds::TexturedDrawCmd;
use super::notification_style::NotificationStyle;
use super::token_colors::{
    ICON_SIZE_PX, NOTIFICATION_BODY_SCALE, NOTIFICATION_ICON_SIZE_PX, NOTIFICATION_INTER_LINE_GAP,
    STATIC_IMAGE_PLACEHOLDER_COLOR, is_alert_banner_zone, notification_inset,
    sort_alert_banner_indices, urgency_to_severity_color, zone_opacity_ceiling,
};
use crate::pipeline::RectVertex;
use crate::pipeline::rect_vertices;

/// A laid-out status-bar card (see [`Compositor::status_bar_card`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StatusBarCard {
    /// The drawn card: backdrop, border, rows and drag handle all use it.
    pub(crate) rect: Rect,
    /// Height of one row (a text line, or the icon if taller).
    pub(crate) row_h: f32,
    /// Height of one text line (`font_size_px × LINE_HEIGHT_MULTIPLIER`).
    pub(crate) line_h: f32,
    /// Vertical padding above the first row and below the last.
    pub(crate) pad_v: f32,
    /// Whether rows reserve space for `key_icon_map` icons.
    pub(crate) icon_layout: bool,
    /// The shown `(key, value)` rows, sorted by key.
    pub(crate) rows: Vec<(String, String)>,
}

impl StatusBarCard {
    /// Top of row `i`.
    pub(crate) fn row_y(&self, i: usize) -> f32 {
        self.rect.y + self.pad_v + i as f32 * self.row_h
    }
}

impl Compositor {
    /// Render zone content backdrop quads driven by `RenderingPolicy`.
    ///
    /// For each zone with at least one active publish:
    /// - Reads `backdrop` + `backdrop_opacity` from the zone's `RenderingPolicy`.
    /// - For `alert-banner` zones with `Notification` content, overrides the
    ///   backdrop color with the urgency-derived `color.severity.*` token color.
    /// - For non-alert-banner zones with `Notification` content, overrides the
    ///   backdrop color with its urgency tone from [`NotificationStyle`]
    ///   (card container roles at `opacity.notification`). Its hairline border is an
    ///   SDF command from `collect_all_rounded_rect_cmds`.
    /// - Applies the zone's current animation opacity (fade-in/fade-out).
    /// - Skips the backdrop quad when `rendering_policy.backdrop` is `None`.
    ///
    /// ## Layer filtering
    ///
    /// When `only_layer` is `Some(layer)`, only zones whose `layer_attachment`
    /// matches `layer` are rendered.  Pass `None` to render all layers (the
    /// legacy behaviour used by unit tests that call this method directly).
    ///
    /// Render-frame methods call this three times to enforce the canonical
    /// layer order: Background → (tiles) → Content → Chrome.
    ///
    /// No per-content-type color branching — all visual properties come from
    /// `RenderingPolicy` fields (spec §Refactoring note, §Default Zone Rendering).
    #[cfg(test)]
    pub(super) fn render_zone_content(
        &self,
        scene: &tze_hud_scene::graph::SceneGraph,
        vertices: &mut Vec<RectVertex>,
        textured_cmds: &mut Vec<TexturedDrawCmd>,
        sw: f32,
        sh: f32,
        only_layer: Option<LayerAttachment>,
    ) {
        self.render_zone_content_at(
            scene,
            vertices,
            textured_cmds,
            (sw, sh),
            only_layer,
            std::time::Instant::now(),
        );
    }

    pub(super) fn render_zone_content_at(
        &self,
        scene: &tze_hud_scene::graph::SceneGraph,
        vertices: &mut Vec<RectVertex>,
        textured_cmds: &mut Vec<TexturedDrawCmd>,
        viewport: (f32, f32),
        only_layer: Option<LayerAttachment>,
        now: std::time::Instant,
    ) {
        let (sw, sh) = viewport;
        if !scene.zones_shown() {
            return;
        }
        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            let zone_tokens = self.zone_tokens(zone_name);
            let scale = tze_hud_config::tokens::px_scale(zone_tokens);
            // Layer filter: skip zones that don't match the requested layer.
            if let Some(required_layer) = only_layer {
                if zone_def.layer_attachment != required_layer {
                    continue;
                }
            }
            let (x, y, w, h) =
                self.zone_geometry(zone_name, &scene.effective_zone_geometry(zone_def), sw, sh);

            let policy = &zone_def.rendering_policy;

            // Zones with backdrop_radius use the SDF rounded-rect pipeline instead.
            // Skip flat-rect backdrop emission for those zones; collect_all_rounded_rect_cmds
            // handles their backdrops separately in encode_rounded_rect_pass.
            let use_rounded_rect = self.degradation_policy.level
                < tze_hud_scene::DegradationLevel::Simplified
                && policy.backdrop_radius.is_some_and(|r| r > 0.0);

            // A zone's own fill/image never exceeds this (ambient-background).
            let ceiling = zone_opacity_ceiling(zone_name, zone_tokens);

            // Determine current animation opacity for this zone.
            let anim_opacity =
                if self.degradation_policy.level >= tze_hud_scene::DegradationLevel::Simplified {
                    1.0
                } else {
                    self.zone_animation_states
                        .get(zone_name)
                        .map(|s| s.current_opacity_at(now))
                        .unwrap_or(1.0)
                };

            // Resolve and emit backdrop quads based on the zone's contention policy.
            //
            // Stack zones: each publication gets its own vertically-stacked slot.
            //   For alert-banner: severity-descending (critical first), then
            //   recency-descending within the same severity.
            //   For other Stack zones: newest-first (slot 0 = newest, at top of zone).
            //   Per-slot heights are computed via Self::per_slot_heights() and may vary
            //   by item (two-line notifications occupy a taller slot).  Dynamic zone
            //   height for alert-banner: sum(slot_heights) — zero when empty.
            //
            // MergeByKey zones: one backdrop (entries are merged at the data
            //   level). Status-bar content draws as a card hugging its rows
            //   (`status_bar_card`), not the full zone.
            //
            // LatestWins / Replace: single backdrop from the most-recent publish only.
            match zone_def.contention_policy {
                ContentionPolicy::Stack { .. } => {
                    // Slot geometry is computed once by zone_slot_layout and shared
                    // with collect_text_items / collect_all_rounded_rect_cmds (hud-qlerb).
                    let layout = self.zone_slot_layout(zone_name, publishes, policy, h, w);

                    for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(y) {
                        let record = &publishes[pub_idx];

                        // Per-publication fade-out opacity (1.0 when no fade active).
                        let pub_opacity = self.pub_opacity_at(zone_name, record, now);
                        // Combined opacity: zone animation × per-publication fade.
                        let combined_opacity = (anim_opacity * pub_opacity).clamp(0.0, 1.0);

                        // Determine backdrop color.
                        // alert-banner: urgency → color.severity.* tokens
                        // non-alert-banner Notification: urgency → NotificationStyle tone
                        //   (opacity included); its hairline, dismiss and actions are SDF
                        //   commands (collect_all_rounded_rect_cmds)
                        // SolidColor: always its own color
                        // StaticImage: warm-gray placeholder quad (full GPU texture deferred)
                        // Other: policy.backdrop
                        let is_notification_content =
                            matches!(&record.content, ZoneContent::Notification(_));
                        let backdrop_rgba: Option<Rgba> = match &record.content {
                            ZoneContent::SolidColor(rgba) => Some(*rgba),
                            ZoneContent::StaticImage(resource_id) => {
                                // If a GPU texture is cached, emit a textured draw command
                                // filling this publication's slot.
                                if self.image_texture_cache.contains_key(resource_id) {
                                    let combined_opacity = (anim_opacity
                                        * self.pub_opacity_at(zone_name, record, now))
                                    .clamp(0.0, ceiling);
                                    textured_cmds.push(TexturedDrawCmd {
                                        resource_id: *resource_id,
                                        x,
                                        y: slot_y,
                                        w,
                                        h: effective_slot_h,
                                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                                        tint: [1.0, 1.0, 1.0, combined_opacity],
                                    });
                                    None // skip the color quad
                                } else {
                                    // Placeholder warm-gray backdrop.
                                    Some(STATIC_IMAGE_PLACEHOLDER_COLOR)
                                }
                            }
                            ZoneContent::Notification(n) if is_alert_banner_zone(zone_name) => {
                                // alert-banner: severity tokens (color.severity.*).
                                // Respect policy.backdrop contract: only override the color when
                                // a backdrop is enabled; skip entirely when backdrop is None.
                                if policy.backdrop.is_some() {
                                    let severity_color =
                                        urgency_to_severity_color(n.urgency, zone_tokens);
                                    Some(severity_color)
                                } else {
                                    None
                                }
                            }
                            ZoneContent::Notification(n) => {
                                // Non-alert-banner notification: the card style's
                                // urgency tone (semantic container roles).
                                // Per spec: urgency >3 clamped to 3 (critical).
                                // Respect policy.backdrop contract: only emit a backdrop (and
                                // border) when a backdrop is enabled; skip entirely when None.
                                policy.backdrop.map(|_| {
                                    NotificationStyle::resolve(zone_tokens, policy)
                                        .tone(n.urgency)
                                        .fill
                                })
                            }
                            _ => policy.backdrop,
                        };

                        if let Some(mut rgba) = backdrop_rgba {
                            // For non-notification content: apply policy backdrop_opacity.
                            // For Notification content in non-alert-banner zones: opacity is
                            // already set by the card style above; skip policy override.
                            if !is_notification_content || is_alert_banner_zone(zone_name) {
                                if let Some(opacity) = policy.backdrop_opacity {
                                    rgba.a = opacity.clamp(0.0, 1.0);
                                }
                            }
                            // Fill, placeholder or policy opacity: never above the
                            // zone's ceiling (ambient-background).
                            rgba.a = rgba.a.min(ceiling);
                            rgba.a *= combined_opacity;
                            // Skip flat-rect emission for rounded zones — handled by the SDF pass.
                            if !use_rounded_rect {
                                vertices.extend_from_slice(&rect_vertices(
                                    x,
                                    slot_y,
                                    w,
                                    effective_slot_h,
                                    sw,
                                    sh,
                                    self.gpu_color(rgba),
                                ));
                            }
                        }

                        // Action buttons are SDF commands drawn after the backdrop
                        // (`collect_all_rounded_rect_cmds`): a flat fill here would
                        // sit under a rounded Chrome-layer backdrop (hud-paslt).

                        // Notification icon: emit a textured draw command for the icon
                        // left-aligned inside the slot, vertically centred.
                        // Renders only when: icon is a valid hex ResourceId AND the
                        // texture is cached.  Falls back to text-only (no icon) otherwise.
                        if let ZoneContent::Notification(payload) = &record.content {
                            if let Some(icon_id) =
                                super::icon::parse_notification_icon(&payload.icon)
                            {
                                if self.image_texture_cache.contains_key(&icon_id) {
                                    // Alert banners keep their pre-restyle icon
                                    // (hud-h51u7.4.2); cards put it at the padding,
                                    // centred on the text block.
                                    let (icon_x, icon_y, size) = if is_alert_banner_zone(zone_name)
                                    {
                                        let inset_h = notification_inset(policy, scale);
                                        let text_slot_h = effective_slot_h
                                            - self
                                                .notification_action_layout(
                                                    zone_name, record, policy, w,
                                                )
                                                .reserve();
                                        let size = NOTIFICATION_ICON_SIZE_PX * scale;
                                        (
                                            x + inset_h,
                                            slot_y + (text_slot_h - size).max(0.0) * 0.5,
                                            size,
                                        )
                                    } else {
                                        let style = NotificationStyle::resolve(zone_tokens, policy);
                                        let text_h =
                                            style.text_block_height(!payload.title.is_empty());
                                        (
                                            x + style.padding_h,
                                            slot_y + (text_h - style.icon_size).max(0.0) * 0.5,
                                            style.icon_size,
                                        )
                                    };
                                    textured_cmds.push(TexturedDrawCmd {
                                        resource_id: icon_id,
                                        x: icon_x,
                                        y: icon_y,
                                        w: size,
                                        h: size,
                                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                                        tint: [1.0, 1.0, 1.0, combined_opacity],
                                    });
                                }
                            }
                        }
                    }
                }
                ContentionPolicy::MergeByKey { .. }
                | ContentionPolicy::LatestWins
                | ContentionPolicy::Replace => {
                    // For MergeByKey and single-publish policies: render a single backdrop
                    // for the zone using the latest publication's content type.
                    let latest = &publishes[publishes.len() - 1];
                    // Status-bar content draws as a card hugging its rows; with no
                    // entries nothing is drawn at all.
                    let card = Self::status_bar_card(
                        publishes,
                        zone_def.contention_policy,
                        policy,
                        scale,
                        Rect::new(x, y, w, h),
                    );
                    if card.is_none() && matches!(&latest.content, ZoneContent::StatusBar(_)) {
                        continue;
                    }
                    let (x, y, w, h) = card.as_ref().map_or((x, y, w, h), |c| {
                        (c.rect.x, c.rect.y, c.rect.width, c.rect.height)
                    });
                    let is_notification_content =
                        matches!(&latest.content, ZoneContent::Notification(_));
                    let backdrop_rgba: Option<Rgba> = match &latest.content {
                        ZoneContent::SolidColor(rgba) => Some(*rgba),
                        ZoneContent::StaticImage(resource_id) => {
                            // If a GPU texture is cached, emit a textured draw command
                            // filling the zone.
                            if self.image_texture_cache.contains_key(resource_id) {
                                textured_cmds.push(TexturedDrawCmd {
                                    resource_id: *resource_id,
                                    x,
                                    y,
                                    w,
                                    h,
                                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                                    tint: [1.0, 1.0, 1.0, anim_opacity.clamp(0.0, ceiling)],
                                });
                                None // skip the color quad
                            } else {
                                // Placeholder warm-gray backdrop.
                                Some(STATIC_IMAGE_PLACEHOLDER_COLOR)
                            }
                        }
                        ZoneContent::Notification(n) if is_alert_banner_zone(zone_name) => {
                            // alert-banner: map urgency to severity token color.
                            // Per spec §Notification Urgency-to-Severity Token Mapping:
                            //   urgency 0,1 → color.severity.info
                            //   urgency 2   → color.severity.warning
                            //   urgency 3   → color.severity.critical
                            // Token-resolved values take precedence over hardcoded constants;
                            // falls back to SEVERITY_* when the token map is empty or the
                            // key is absent.
                            // Respect policy.backdrop contract: only override when backdrop
                            // is enabled; skip entirely when backdrop is None.
                            if policy.backdrop.is_some() {
                                let severity_color =
                                    urgency_to_severity_color(n.urgency, zone_tokens);
                                Some(severity_color)
                            } else {
                                None
                            }
                        }
                        ZoneContent::Notification(n) => {
                            // Non-alert-banner notification: the card style's urgency
                            // tone (semantic container roles).
                            // Per spec: urgency >3 clamped to 3 (critical).
                            // Respect policy.backdrop contract: only emit a backdrop (and
                            // border) when a backdrop is enabled; skip entirely when None.
                            policy.backdrop.map(|_| {
                                NotificationStyle::resolve(zone_tokens, policy)
                                    .tone(n.urgency)
                                    .fill
                            })
                        }
                        _ => {
                            // All other content: use policy.backdrop.
                            policy.backdrop
                        }
                    };

                    if let Some(mut rgba) = backdrop_rgba {
                        // For non-notification content: apply policy backdrop_opacity.
                        // For Notification content in non-alert-banner zones: opacity is
                        // already set by the card style above; skip policy override.
                        if !is_notification_content || is_alert_banner_zone(zone_name) {
                            if let Some(opacity) = policy.backdrop_opacity {
                                rgba.a = opacity.clamp(0.0, 1.0);
                            }
                        }
                        // Fill, placeholder or policy opacity: never above the
                        // zone's ceiling (ambient-background).
                        rgba.a = rgba.a.min(ceiling);
                        // Apply zone animation opacity.
                        rgba.a *= anim_opacity.clamp(0.0, 1.0);

                        // Skip flat-rect emission for rounded zones — handled by the SDF pass.
                        if !use_rounded_rect {
                            vertices.extend_from_slice(&rect_vertices(
                                x,
                                y,
                                w,
                                h,
                                sw,
                                sh,
                                self.gpu_color(rgba),
                            ));
                        }
                    }

                    // Notification icon for LatestWins/MergeByKey/Replace zones.
                    if let ZoneContent::Notification(payload) = &latest.content {
                        if let Some(icon_id) = super::icon::parse_notification_icon(&payload.icon) {
                            if self.image_texture_cache.contains_key(&icon_id) {
                                let style = NotificationStyle::resolve(zone_tokens, policy);
                                let size = style.icon_size;
                                let eff_opacity = anim_opacity.clamp(0.0, 1.0);
                                textured_cmds.push(TexturedDrawCmd {
                                    resource_id: icon_id,
                                    x: x + style.padding_h,
                                    y: y + (h - size) * 0.5,
                                    w: size,
                                    h: size,
                                    uv_rect: [0.0, 0.0, 1.0, 1.0],
                                    tint: [1.0, 1.0, 1.0, eff_opacity],
                                });
                            }
                        }
                    }

                    // ── StatusBar icon rendering ──────────────────────────────
                    // One icon per shown row whose key maps to a cached icon
                    // texture, at the left inset, centred in the row.
                    if let Some(card) = card.as_ref().filter(|c| c.icon_layout) {
                        let margin_h = policy
                            .margin_horizontal
                            .or(policy.margin_px)
                            .unwrap_or(8.0 * scale);
                        let icon = ICON_SIZE_PX * scale;
                        for (row, (k, _v)) in card.rows.iter().enumerate() {
                            let Some(svg_path) = policy.key_icon_map.get(k.as_str()) else {
                                continue;
                            };
                            // Cached under the path-derived ResourceId by
                            // ensure_scene_icon_textures before this pass; a
                            // failed rasterization leaves it absent.
                            let resource_id = ResourceId::of(svg_path.as_bytes());
                            if !self.image_texture_cache.contains_key(&resource_id) {
                                continue;
                            }
                            textured_cmds.push(TexturedDrawCmd {
                                resource_id,
                                x: card.rect.x + margin_h,
                                y: card.row_y(row) + (card.row_h - icon) * 0.5,
                                w: icon,
                                h: icon,
                                uv_rect: [0.0, 0.0, 1.0, 1.0],
                                tint: [1.0, 1.0, 1.0, anim_opacity.clamp(0.0, 1.0)],
                            });
                        }
                    }
                }
            }
        }

        // ── Debug zone tints (--debug-zones) ─────────────────────────────────
        // Render ALL zone boundaries with colored tints ON TOP of content
        // backgrounds so developers can see zone geometry even when zones have
        // opaque content.  The overall background gets a subtle black tint;
        // each zone gets a rainbow-sampled color at low opacity.
        //
        // Guard: only emit debug tints when there is no layer filter (legacy
        // single-pass test usage) or on the final Chrome pass.  The three-pass
        // frame-render methods call this function three times; without this
        // guard the debug overlay would be drawn 3× per frame, tripling its
        // effective opacity.
        let emit_debug_tints =
            self.debug_zone_tints && matches!(only_layer, None | Some(LayerAttachment::Chrome));
        if emit_debug_tints {
            // 0.5% opacity black background tint over the full window.
            // In overlay mode, the clear_pipeline writes RGBA directly (no
            // GPU blending). DWM composites with premultiplied alpha, so RGB
            // values must be pre-multiplied by alpha.
            const A: f32 = 0.001;
            vertices.extend_from_slice(&rect_vertices(
                0.0,
                0.0,
                sw,
                sh,
                sw,
                sh,
                [0.0, 0.0, 0.0, A],
            ));

            // Rainbow palette for zone tints (1% opacity, premultiplied).
            const ZA: f32 = 0.005;
            let palette: &[[f32; 4]] = &[
                [1.0 * ZA, 0.2 * ZA, 0.2 * ZA, ZA], // red
                [1.0 * ZA, 0.6 * ZA, 0.1 * ZA, ZA], // orange
                [1.0 * ZA, 1.0 * ZA, 0.2 * ZA, ZA], // yellow
                [0.2 * ZA, 0.9 * ZA, 0.2 * ZA, ZA], // green
                [0.2 * ZA, 0.6 * ZA, 1.0 * ZA, ZA], // blue
                [0.7 * ZA, 0.3 * ZA, 1.0 * ZA, ZA], // violet
            ];
            for (idx, (zone_name, zone_def)) in scene.zone_registry.zones.iter().enumerate() {
                let color = palette[idx % palette.len()];
                let (x, y, w, h) =
                    self.zone_geometry(zone_name, &scene.effective_zone_geometry(zone_def), sw, sh);
                vertices.extend_from_slice(&rect_vertices(x, y, w, h, sw, sh, color));
            }
        }
    }

    /// Lay out the status-bar card for a zone: the one rect that its backdrop,
    /// border, text rows, icons and drag handle all use, plus its visible rows.
    ///
    /// Returns `None` when the zone shows no status-bar entries — nothing is
    /// drawn, not even an empty backdrop.
    ///
    /// - Entries: `MergeByKey` merges every active StatusBar publication (the
    ///   latest value per key wins); other policies use the most-recent
    ///   publication only, and only while it is a StatusBar. Rows are sorted
    ///   by key (`StatusBarPayload.entries` carries no publish order).
    /// - Rows: one line each, `font_size_px × LINE_HEIGHT_MULTIPLIER` tall (at
    ///   least the icon height while any shown key has a `key_icon_map` icon).
    ///   As many rows as fit the zone are shown, never fewer than one.
    /// - Rect: the zone's full width, `rows × row_h + 2 × margin_v` tall,
    ///   anchored to the zone's bottom edge (the zone is the most it may grow
    ///   to, not the drawn size).
    pub(crate) fn status_bar_card(
        publishes: &[ZonePublishRecord],
        contention_policy: ContentionPolicy,
        policy: &RenderingPolicy,
        scale: f32,
        zone: Rect,
    ) -> Option<StatusBarCard> {
        let mut entries: Vec<(String, String)> = match contention_policy {
            ContentionPolicy::MergeByKey { max_keys } => {
                let mut merged: HashMap<&str, &str> = HashMap::with_capacity(max_keys as usize);
                for record in publishes {
                    if let ZoneContent::StatusBar(payload) = &record.content {
                        for (k, v) in &payload.entries {
                            merged.insert(k, v);
                        }
                    }
                }
                merged
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect()
            }
            _ => match publishes.last().map(|r| &r.content) {
                Some(ZoneContent::StatusBar(p)) => p
                    .entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                _ => Vec::new(),
            },
        };
        if entries.is_empty() {
            return None;
        }
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Per-entry icons only while a shown key is mapped, so configuring
        // the map for absent keys never changes the layout.
        let icon_layout = !policy.key_icon_map.is_empty()
            && entries
                .iter()
                .any(|(k, _)| policy.key_icon_map.contains_key(k.as_str()));
        let font_size_px = policy
            .font_size_px
            .unwrap_or(16.0 * scale)
            .clamp(6.0, 200.0);
        let line_h = font_size_px * crate::text::LINE_HEIGHT_MULTIPLIER;
        let row_h = if icon_layout {
            line_h.max(ICON_SIZE_PX * scale)
        } else {
            line_h
        };
        let pad_v = policy
            .margin_vertical
            .or(policy.margin_px)
            .unwrap_or(8.0 * scale);
        let fit = ((zone.height - 2.0 * pad_v) / row_h).floor().max(1.0) as usize;
        entries.truncate(fit);
        let height = (entries.len() as f32 * row_h + 2.0 * pad_v).min(zone.height);
        Some(StatusBarCard {
            rect: Rect::new(zone.x, zone.y + zone.height - height, zone.width, height),
            row_h,
            line_h,
            pad_v,
            icon_layout,
            rows: entries,
        })
    }

    /// Compute the per-slot height for a Stack zone (single-line content).
    ///
    /// `slot_h = line_height + 2 * margin_v + SLOT_BASELINE_GAP`
    /// where `line_height = font_size_px * 1.4`
    ///
    /// - `font_size_px` — from `RenderingPolicy`; defaults to 16.
    /// - `margin_v` — from `margin_vertical` → `margin_px` → 8 px fallback chain.
    /// - `SLOT_BASELINE_GAP` — a small constant gap (4 px) between successive slot
    ///   backdrops so they don't bleed into each other visually. This is not a
    ///   configurable policy field; it is a structural layout constant.
    ///
    /// Used by both `collect_text_items` and `render_zone_content` so both code
    /// paths stay consistent.
    pub(crate) fn stack_slot_height(policy: &RenderingPolicy, scale: f32) -> f32 {
        let slot_baseline_gap = 4.0 * scale;
        let font_size_px = policy
            .font_size_px
            .unwrap_or(16.0 * scale)
            .clamp(6.0, 200.0);
        // Use line_height (font_size × 1.4) to match cosmic-text layout in
        // prepare_text_items. Previously this used font_size_px directly, which
        // produced bounds too small for one line of text to render.
        let line_height = font_size_px * 1.4;
        let margin_v = policy
            .margin_vertical
            .or(policy.margin_px)
            .unwrap_or(8.0 * scale);
        (line_height + 2.0 * margin_v + slot_baseline_gap).max(1.0)
    }

    /// Compute the per-slot height for a single notification publication.
    ///
    /// When `payload.title` is non-empty, the slot must accommodate two text lines:
    ///   - Title line: `font_size_px * 1.4` (line height, same as single-line slot)
    ///   - Body line: `title_line_h * notification_body_scale`
    ///   - Inter-line gap: `notification_inter_line_gap` px between the two lines
    ///
    /// Two-line height formula:
    ///   `slot_h = title_line_h + inter_line_gap + body_line_h + 2*margin_v + SLOT_BASELINE_GAP`
    ///   where `title_line_h = font_size_px * 1.4`, `body_line_h = title_line_h * notification_body_scale`
    ///
    /// The caller is responsible for resolving `notification_body_scale` and
    /// `notification_inter_line_gap` from the token map (with appropriate fallbacks) so
    /// that height calculation and rendering use the exact same values.
    ///
    /// When `payload.title` is empty, the text height is [`Self::stack_slot_height`]
    /// (single line).
    ///
    /// Slot height for a notification's text alone (title and body).
    pub(crate) fn notification_slot_height(
        payload: &NotificationPayload,
        policy: &RenderingPolicy,
        notification_body_scale: f32,
        notification_inter_line_gap: f32,
        scale: f32,
    ) -> f32 {
        if payload.title.is_empty() {
            return Self::stack_slot_height(policy, scale);
        }
        let slot_baseline_gap = 4.0 * scale;
        let font_size_px = policy
            .font_size_px
            .unwrap_or(16.0 * scale)
            .clamp(6.0, 200.0);
        let title_line_h = font_size_px * 1.4;
        let body_line_h = title_line_h * notification_body_scale;
        let margin_v = policy
            .margin_vertical
            .or(policy.margin_px)
            .unwrap_or(8.0 * scale);
        (title_line_h
            + notification_inter_line_gap
            + body_line_h
            + 2.0 * margin_v
            + slot_baseline_gap)
            .max(1.0)
    }

    /// Compute per-slot heights for an ordered slice of publications in a Stack zone.
    ///
    /// For non-`Notification` content and single-line notifications, returns
    /// `Self::stack_slot_height(policy)` for every slot.  For two-line
    /// notifications (`payload.title` non-empty), returns the taller two-line height
    /// computed from the same resolved notification typography values used by rendering.
    ///
    /// Returns a `Vec<f32>` of length `ordered.len()`, one height per slot.
    fn per_slot_heights(
        ordered: &[&ZonePublishRecord],
        policy: &RenderingPolicy,
        notification_body_scale: f32,
        notification_inter_line_gap: f32,
        scale: f32,
    ) -> Vec<f32> {
        ordered
            .iter()
            .map(|rec| match &rec.content {
                ZoneContent::Notification(n) => Self::notification_slot_height(
                    n,
                    policy,
                    notification_body_scale,
                    notification_inter_line_gap,
                    scale,
                ),
                _ => Self::stack_slot_height(policy, scale),
            })
            .collect()
    }

    /// Resolve changed publications before the first slot/animation/draw read,
    /// using the rasterizer's actual font database (including uploaded fonts).
    pub(super) fn prime_notification_layouts(&mut self, scene: &SceneGraph, sw: f32, sh: f32) {
        let Some(rasterizer) = self.text_rasterizer.as_mut() else {
            return;
        };
        let mut cache = self.notification_layouts.borrow_mut();
        cache.retain(scene);
        for (name, records) in scene.shown_zone_publishes() {
            let Some(zone) = scene.zone_registry.zones.get(name) else {
                continue;
            };
            let tokens = tze_hud_config::tokens::for_zone(&self.token_map, name);
            let (_, _, width, _) = super::zone_geometry_in(
                &self.display_layout,
                name,
                &scene.effective_zone_geometry(zone),
                sw,
                sh,
            );
            for record in records {
                cache.resolve_with_fonts(
                    &tokens,
                    name,
                    record,
                    &zone.rendering_policy,
                    width,
                    Some(rasterizer.font_system_mut()),
                );
            }
        }
    }

    pub(super) fn notification_action_layout(
        &self,
        zone: &str,
        record: &ZonePublishRecord,
        policy: &RenderingPolicy,
        width: f32,
    ) -> super::notification_layout::NotificationActionLayout {
        self.notification_layouts.borrow_mut().resolve(
            self.zone_tokens(zone),
            zone,
            record,
            policy,
            width,
        )
    }

    /// Compute the [`ZoneSlotLayout`] for a Stack zone.
    ///
    /// This is the single authoritative computation for zone slot geometry.
    /// All frame-path consumers (`collect_text_items`, `render_zone_content`,
    /// `collect_all_rounded_rect_cmds`) call this method instead of duplicating
    /// the slot-height/offset/ordering logic inline (hud-qlerb).
    ///
    /// # Parameters
    /// - `zone_name` — used to distinguish alert-banner zones (severity sort
    ///   + dynamic height) from regular Stack zones (newest-first, fixed height).
    /// - `publishes` — the ordered publish list for this zone.
    /// - `policy` — the zone's `RenderingPolicy` (font metrics, margins).
    /// - `zh` — the configured zone height in pixels, used as `effective_h` for
    ///   non-alert-banner zones.
    pub(super) fn zone_slot_layout(
        &self,
        zone_name: &str,
        publishes: &[ZonePublishRecord],
        policy: &RenderingPolicy,
        zh: f32,
        zw: f32,
    ) -> super::ZoneSlotLayout {
        Self::zone_slot_layout_with_tokens(
            self.zone_tokens(zone_name),
            zone_name,
            publishes,
            policy,
            zh,
            zw,
            &self.notification_layouts,
        )
    }

    /// [`Self::zone_slot_layout`] without a compositor, so GPU-free hit-region
    /// population lays slots out with the exact same function as drawing.
    pub(super) fn zone_slot_layout_with_tokens(
        token_map: &HashMap<String, String>,
        zone_name: &str,
        publishes: &[ZonePublishRecord],
        policy: &RenderingPolicy,
        zh: f32,
        zw: f32,
        layouts: &std::cell::RefCell<super::notification_layout::NotificationLayoutCache>,
    ) -> super::ZoneSlotLayout {
        let ordered_indices: Vec<usize> = if is_alert_banner_zone(zone_name) {
            sort_alert_banner_indices(publishes)
        } else {
            (0..publishes.len()).rev().collect()
        };

        let notif_body_scale = token_map
            .get("typography.notification.body.scale")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(NOTIFICATION_BODY_SCALE)
            .clamp(0.5, 1.0);

        let ordered_refs: Vec<&ZonePublishRecord> =
            ordered_indices.iter().map(|&i| &publishes[i]).collect();
        let mut slot_heights = Self::per_slot_heights(
            &ordered_refs,
            policy,
            notif_body_scale,
            NOTIFICATION_INTER_LINE_GAP * tze_hud_config::tokens::px_scale(token_map),
            tze_hud_config::tokens::px_scale(token_map),
        );
        let alert_banner = is_alert_banner_zone(zone_name);
        let card_style = (!alert_banner).then(|| NotificationStyle::resolve(token_map, policy));
        for (height, record) in slot_heights.iter_mut().zip(&ordered_refs) {
            if let ZoneContent::Notification(n) = &record.content {
                if let Some(style) = &card_style {
                    *height = style.text_block_height(!n.title.is_empty());
                }
                *height += layouts
                    .borrow_mut()
                    .resolve(token_map, zone_name, record, policy, zw)
                    .reserve();
            }
        }
        // Alert banners stack flush as one strip; notification cards are
        // separated by a token gap so consecutive cards never touch.
        let (slot_offsets, effective_h) = match &card_style {
            None => (
                Self::slot_offsets(&slot_heights, 0.0),
                slot_heights.iter().sum(),
            ),
            Some(style) => (Self::slot_offsets(&slot_heights, style.stack_gap), zh),
        };

        super::ZoneSlotLayout {
            ordered_indices,
            slot_heights,
            slot_offsets,
            effective_h,
        }
    }

    /// Compute the cumulative slot y-offsets from a list of per-slot heights.
    ///
    /// Returns a `Vec<f32>` of length `heights.len()`, where `offsets[i]` is the
    /// sum of all `heights[0..i]` plus `gap` between each pair (i.e. the
    /// y-start of slot `i` relative to the zone origin).
    fn slot_offsets(heights: &[f32], gap: f32) -> Vec<f32> {
        let mut offsets = Vec::with_capacity(heights.len());
        let mut acc = 0.0_f32;
        for &h in heights {
            offsets.push(acc);
            acc += h + gap;
        }
        offsets
    }

    /// Resolve a zone's geometry policy to pixel bounds (x, y, w, h).
    pub(super) fn resolve_zone_geometry(
        policy: &GeometryPolicy,
        sw: f32,
        sh: f32,
    ) -> (f32, f32, f32, f32) {
        tze_hud_scene::resolve_zone_geometry(policy, sw, sh)
    }
}
