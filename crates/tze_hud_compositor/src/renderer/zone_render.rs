//! Zone rendering and zone layout methods for the compositor.
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
    STATIC_IMAGE_PLACEHOLDER_COLOR, is_alert_banner_zone, sort_alert_banner_indices,
    zone_opacity_ceiling,
};
use crate::pipeline::RectVertex;
use crate::pipeline::rect_vertices;

impl Compositor {
    /// Render zone content backdrop quads driven by `RenderingPolicy`.
    ///
    /// For each zone with at least one active publish:
    /// - Reads `backdrop` + `backdrop_opacity` from the zone's `RenderingPolicy`.
    /// - For `alert-banner` zones with `Notification` content, overrides the
    ///   backdrop color with its urgency tone from [`NotificationStyle`]
    ///   (`color.alert.*` banner roles at `opacity.alert`).
    /// - For other zones with `Notification` content, overrides the
    ///   backdrop color with its urgency tone from [`NotificationStyle`]
    ///   (card container roles at `opacity.notification`). Hairline borders,
    ///   severity marks and controls are SDF commands from
    ///   `collect_all_rounded_rect_cmds`.
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
    /// `RenderingPolicy` fields.
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
        for stack in self.zone_stacks(scene) {
            let _tokens = self.tokens_for_stack(&stack);
            let (zone_name, publishes) = (stack.name, &*stack.publishes);
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };
            let zone_tokens = self.zone_tokens(zone_name);
            // Layer filter: skip zones that don't match the requested layer.
            if let Some(required_layer) = only_layer {
                if zone_def.layer_attachment != required_layer {
                    continue;
                }
            }
            let (x, y, w, h) =
                self.stack_geometry(&stack, &scene.effective_zone_geometry(zone_def), sw, sh);

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
            //   Other Stack zones: unanswered questions first, newest-first
            //   within the question and non-question groups.
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
                    // with collect_text_items / collect_all_rounded_rect_cmds.
                    let layout = self.zone_slot_layout(scene, zone_name, publishes, policy, h, w);

                    for (pub_idx, card) in layout.iter_visible(Rect::new(x, y, w, h)) {
                        let record = &publishes[pub_idx];
                        let (x, slot_y, w, effective_slot_h) =
                            (card.x, card.y, card.width, card.height);

                        // Per-publication fade-out opacity (1.0 when no fade active).
                        let pub_opacity = self.pub_opacity_at(zone_name, record, now);
                        // Combined opacity: zone animation × per-publication fade.
                        let combined_opacity = (anim_opacity * pub_opacity).clamp(0.0, 1.0);

                        // Determine backdrop color.
                        // Notification (card or alert banner): urgency → NotificationStyle tone
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
                                let source = if record.resident_raster.is_some() {
                                    super::draw_cmds::ImageSource::Resident
                                } else {
                                    super::draw_cmds::ImageSource::Generic
                                };
                                let cache = if record.resident_raster.is_some() {
                                    &self.resident_image_texture_cache
                                } else {
                                    &self.image_texture_cache
                                };
                                if cache.contains_key(resource_id) {
                                    let combined_opacity = (anim_opacity
                                        * self.pub_opacity_at(zone_name, record, now))
                                    .clamp(0.0, ceiling);
                                    textured_cmds.push(TexturedDrawCmd {
                                        source,
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
                            ZoneContent::Notification(n) => {
                                // The card or banner style's urgency tone
                                // (semantic container roles).
                                // urgency >3 clamped to 3 (critical).
                                // Respect policy.backdrop contract: only emit a backdrop (and
                                // border) when a backdrop is enabled; skip entirely when None.
                                policy.backdrop.map(|_| {
                                    NotificationStyle::for_zone(zone_name, zone_tokens, policy)
                                        .tone(n.urgency)
                                        .fill
                                })
                            }
                            _ => policy.backdrop,
                        };

                        if let Some(mut rgba) = backdrop_rgba {
                            // For non-notification content: apply policy backdrop_opacity.
                            // For Notification content opacity is already set by the
                            // card style above; skip policy override.
                            if !is_notification_content {
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
                        // sit under a rounded Chrome-layer backdrop.

                        // Notification icon: emit a textured draw command for the icon
                        // left-aligned inside the slot, vertically centred.
                        // Renders only when: icon is a valid hex ResourceId AND the
                        // texture is cached.  Falls back to text-only (no icon) otherwise.
                        if let ZoneContent::Notification(payload) = &record.content {
                            if let Some(icon_id) =
                                super::icon::parse_notification_icon(&payload.icon)
                            {
                                if self.image_texture_cache.contains_key(&icon_id) {
                                    // At the padding, centred on the text block
                                    // (cards and banners alike).
                                    let icon =
                                        NotificationStyle::for_zone(zone_name, zone_tokens, policy)
                                            .icon_bounds(
                                                x,
                                                slot_y,
                                                !payload.title.is_empty(),
                                                record.caption.as_deref(),
                                            );
                                    let (icon_x, icon_y, size) = (icon.x, icon.y, icon.width);
                                    textured_cmds.push(TexturedDrawCmd {
                                        source: super::draw_cmds::ImageSource::Generic,
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
                    let subtitle_pill = if zone_name == "subtitle"
                        && let ZoneContent::StreamText(text) = &latest.content
                    {
                        let Some(pill) =
                            self.subtitle_layout(zone_name, text, policy, Rect::new(x, y, w, h))
                        else {
                            continue;
                        };
                        Some(pill)
                    } else {
                        None
                    };
                    // Status-bar content draws as a pill hugging its entries; with no
                    // entries nothing is drawn at all.
                    let card = self.status_bar_card(
                        zone_name,
                        publishes,
                        zone_def.contention_policy,
                        policy,
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
                            let source = if latest.resident_raster.is_some() {
                                super::draw_cmds::ImageSource::Resident
                            } else {
                                super::draw_cmds::ImageSource::Generic
                            };
                            let cache = if latest.resident_raster.is_some() {
                                &self.resident_image_texture_cache
                            } else {
                                &self.image_texture_cache
                            };
                            if cache.contains_key(resource_id) {
                                textured_cmds.push(TexturedDrawCmd {
                                    source,
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
                        ZoneContent::StatusBar(_) => card.as_ref().map(|c| c.style.fill),
                        ZoneContent::Notification(n) => {
                            // The card or banner style's urgency tone
                            // (semantic container roles).
                            // urgency >3 clamped to 3 (critical).
                            // Respect policy.backdrop contract: only emit a backdrop (and
                            // border) when a backdrop is enabled; skip entirely when None.
                            policy.backdrop.map(|_| {
                                NotificationStyle::for_zone(zone_name, zone_tokens, policy)
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
                        // For Notification content opacity is already set by the
                        // card style above; skip policy override.
                        if !is_notification_content && card.is_none() {
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
                        if !card.as_ref().map_or(use_rounded_rect, |c| {
                            c.style.radius > 0.0
                                && self.degradation_policy.level
                                    < tze_hud_scene::DegradationLevel::Simplified
                        }) {
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

                    // Match the normal SDF composition: optional whole-zone
                    // contrast first, content-hugging pill on top. Simplified
                    // rendering drops corners/hairline and makes fills opaque,
                    // while preserving the layer order.
                    if self.degradation_policy.level >= tze_hud_scene::DegradationLevel::Simplified
                        && let Some(pill) = subtitle_pill
                        && pill.style.fill.a > 0.0
                    {
                        let mut fill = pill.style.fill;
                        fill.a *= anim_opacity;
                        vertices.extend_from_slice(&rect_vertices(
                            pill.rect.x,
                            pill.rect.y,
                            pill.rect.width,
                            pill.rect.height,
                            sw,
                            sh,
                            self.gpu_color(fill),
                        ));
                    }

                    // Notification icon for LatestWins/MergeByKey/Replace zones.
                    if let ZoneContent::Notification(payload) = &latest.content {
                        if let Some(icon_id) = super::icon::parse_notification_icon(&payload.icon) {
                            if self.image_texture_cache.contains_key(&icon_id) {
                                let style =
                                    NotificationStyle::for_zone(zone_name, zone_tokens, policy);
                                let size = style.icon_size;
                                let eff_opacity = anim_opacity.clamp(0.0, 1.0);
                                textured_cmds.push(TexturedDrawCmd {
                                    source: super::draw_cmds::ImageSource::Generic,
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

                    // Explicit owner icons share the measured horizontal entry positions.
                    if let Some(card) = card.as_ref() {
                        let icon = card.style.icon;
                        for entry in &card.entries {
                            let Some(svg_path) = &entry.icon else {
                                continue;
                            };
                            let resource_id = ResourceId::of(svg_path.as_bytes());
                            if !self.image_texture_cache.contains_key(&resource_id) {
                                continue;
                            }
                            textured_cmds.push(TexturedDrawCmd {
                                source: super::draw_cmds::ImageSource::Generic,
                                resource_id,
                                x: card.rect.x + entry.x,
                                y: card.rect.y + (card.rect.height - icon) * 0.5,
                                w: icon,
                                h: icon,
                                uv_rect: [0.0, 0.0, 1.0, 1.0],
                                tint: [1.0, 1.0, 1.0, anim_opacity.clamp(0.0, 1.0)],
                            });
                        }
                        let mut divider = card.style.outline;
                        divider.a *= anim_opacity;
                        for rect in card.dividers() {
                            vertices.extend_from_slice(&rect_vertices(
                                rect.x,
                                rect.y,
                                rect.width,
                                rect.height,
                                sw,
                                sh,
                                self.gpu_color(divider),
                            ));
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

    /// Resolve changed publications before the first slot/animation/draw read,
    /// using the rasterizer's actual font database (including uploaded fonts).
    pub(super) fn prime_notification_layouts(&mut self, scene: &SceneGraph, sw: f32, sh: f32) {
        let Some(rasterizer) = self.text_rasterizer.as_mut() else {
            return;
        };
        let mut cache = self.notification_layouts.borrow_mut();
        cache.retain(scene);
        for stack in super::zone_stacks_in(&self.display_layout, scene) {
            let (name, records) = (stack.name, &*stack.publishes);
            let Some(zone) = scene.zone_registry.zones.get(name) else {
                continue;
            };
            let tokens = tze_hud_config::tokens::for_zone(&self.token_map, &stack.token_key());
            let (_, _, width, _) = super::stack_geometry_in(
                &self.display_layout,
                &stack,
                &scene.effective_zone_geometry(zone),
                sw,
                sh,
            );
            // Banner actions occupy the inset card, matching every slot and
            // draw consumer. Priming at zone width would immediately discard
            // this live-font descriptor for a narrower fallback layout.
            let width = if is_alert_banner_zone(name) {
                let style = NotificationStyle::for_zone(name, &tokens, &zone.rendering_policy);
                (width - 2.0 * style.margin).max(1.0)
            } else {
                width
            };
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
    /// the slot-height/offset/ordering logic inline.
    ///
    /// # Parameters
    /// - `zone_name` — used to distinguish alert-banner zones (severity sort
    ///   + dynamic height) from regular Stack zones (question-first, fixed height).
    /// - `publishes` — the ordered publish list for this zone.
    /// - `policy` — the zone's `RenderingPolicy` (font metrics, margins).
    /// - `zh` — the configured zone height in pixels, used as `effective_h` for
    ///   non-alert-banner zones.
    pub(super) fn zone_slot_layout(
        &self,
        scene: &SceneGraph,
        zone_name: &str,
        publishes: &[ZonePublishRecord],
        policy: &RenderingPolicy,
        zh: f32,
        zw: f32,
    ) -> super::ZoneSlotLayout {
        Self::zone_slot_layout_with_tokens(
            self.zone_tokens(zone_name),
            scene,
            zone_name,
            publishes,
            policy,
            (zw, zh),
            &self.notification_layouts,
        )
    }

    /// The order a stack draws (and tabs through) its publications.
    pub(super) fn stack_order(
        scene: &SceneGraph,
        zone_name: &str,
        publishes: &[ZonePublishRecord],
    ) -> Vec<usize> {
        if is_alert_banner_zone(zone_name) {
            sort_alert_banner_indices(publishes)
        } else {
            // A spared question must stay reachable when the bounded zone
            // cannot draw every retained card. Keep newest-first within each
            // group and use the same answered/expiry predicate as admission.
            let mut indices: Vec<_> = (0..publishes.len()).rev().collect();
            // Evaluate expiry once per card so its key cannot change mid-sort.
            indices.sort_by_cached_key(|&index| !scene.is_unanswered_question(&publishes[index]));
            indices
        }
    }

    /// [`Self::zone_slot_layout`] without a compositor, so GPU-free hit-region
    /// population lays slots out with the exact same function as drawing.
    pub(super) fn zone_slot_layout_with_tokens(
        token_map: &HashMap<String, String>,
        scene: &SceneGraph,
        zone_name: &str,
        publishes: &[ZonePublishRecord],
        policy: &RenderingPolicy,
        size: (f32, f32),
        layouts: &std::cell::RefCell<super::notification_layout::NotificationLayoutCache>,
    ) -> super::ZoneSlotLayout {
        let (zw, zh) = size;
        let ordered_indices = Self::stack_order(scene, zone_name, publishes);

        let ordered_refs: Vec<&ZonePublishRecord> =
            ordered_indices.iter().map(|&i| &publishes[i]).collect();
        // Every notification (card or banner) sizes from its style; other
        // content takes a single-line stack slot.
        let style = NotificationStyle::for_zone(zone_name, token_map, policy);
        let scale = tze_hud_config::tokens::px_scale(token_map);
        let card_w = (zw - 2.0 * style.margin).max(1.0);
        let slot_heights: Vec<f32> = ordered_refs
            .iter()
            .map(|record| match &record.content {
                ZoneContent::Notification(n) => {
                    style.text_block_height(!n.title.is_empty())
                        + style.caption_height(record.caption.as_deref())
                        + style.progress_reserve(n)
                        + layouts
                            .borrow_mut()
                            .resolve(token_map, zone_name, record, policy, card_w)
                            .reserve()
                }
                _ => Self::stack_slot_height(policy, scale),
            })
            .collect();
        // Cards are separated by a token gap so consecutive cards never
        // touch; alert banners start and stack `space.alert.margin` apart and
        // the zone grows to the last one.
        let mut slot_offsets = Self::slot_offsets(&slot_heights, style.stack_gap);
        for offset in &mut slot_offsets {
            *offset += style.margin;
        }
        let effective_h = if is_alert_banner_zone(zone_name) {
            slot_offsets
                .last()
                .zip(slot_heights.last())
                .map_or(0.0, |(offset, h)| offset + h)
        } else {
            zh
        };

        super::ZoneSlotLayout {
            ordered_indices,
            slot_heights,
            slot_offsets,
            inset: style.margin,
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
