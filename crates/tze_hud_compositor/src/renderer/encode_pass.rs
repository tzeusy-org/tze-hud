//! GPU encode-pass methods and rounded-rect helpers for the compositor.
//!
//! Moved from `renderer/mod.rs` (the "Color helpers + encode passes" and
//! "Rounded rect" clusters, formerly ~L5850–6380 at plan date) by Step R-6 of
//! the renderer module split (hud-fgryk).  No logic was changed; only
//! visibility modifiers were added where Rust's module-privacy rules require
//! them (listed in the PR body).
//!
//! ## Methods in this file
//!
//! **Color helpers** (used by many encode methods):
//! - `gpu_color` — converts `Rgba` to GPU-ready `[f32; 4]` (overlay-aware premultiply)
//! - `gpu_color_raw` — same as `gpu_color` but for raw `[f32; 4]` arrays
//! - `clear_color` — returns the wgpu clear color (transparent in overlay, dark in fullscreen)
//!
//! **Encode passes**:
//! - `encode_widget_pass` — composites widget textures into the frame
//! - `encode_drag_handle_pass` — chrome pass for drag-handle quads
//! - `encode_image_pass` — textured image quad render pass
//!
//! **Rounded rect**:
//! - `collect_all_rounded_rect_cmds` — partitions zone rounded-rect cmds by layer
//! - `collect_tile_rounded_rect_cmds` — collects rounded-rect cmds from tile scene nodes
//! - `collect_tile_rounded_rect_cmds_from_node` — recursive per-node helper
//! - `encode_rounded_rect_pass` — GPU pass for SDF rounded-rectangle rendering

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;
use wgpu::util::DeviceExt;

use crate::pipeline::{
    RoundedRectClip, RoundedRectDrawCmd, RoundedRectVertex, rounded_rect_vertices,
    rounded_rect_vertices_with_draw_bounds, textured_rect_vertices,
};

use super::LayerPartitionedRoundedRectCmds;
use super::draw_cmds::TexturedDrawCmd;
use super::tile_render::node_uses_display_tile_scroll;
use super::token_colors::{
    NOTIFICATION_BACKDROP_OPACITY, STATIC_IMAGE_PLACEHOLDER_COLOR, is_alert_banner_zone,
    linear_to_srgb, srgb_to_linear, urgency_to_notification_color, urgency_to_severity_color,
};

#[inline]
fn simplify_flat_alpha(mut color: [f32; 4], level: tze_hud_scene::DegradationLevel) -> [f32; 4] {
    if level >= tze_hud_scene::DegradationLevel::Simplified && color[3] > 0.0 {
        color[3] = 1.0;
    }
    color
}

// ─── Encode-pass impl block ───────────────────────────────────────────────────

impl super::Compositor {
    /// Convert an Rgba to GPU-ready `[f32; 4]`.
    ///
    /// In overlay mode the clear_pipeline writes RGBA directly (no GPU blending);
    /// DWM composites with premultiplied alpha **in sRGB space**.  The GPU
    /// auto-encodes fragment output from linear→sRGB (`Bgra8UnormSrgb`), so we
    /// must output the linear value that, after encoding, yields the correct
    /// sRGB-premultiplied result:
    ///
    ///   stored_R = sRGB(R) × A          (what DWM expects)
    ///   fragment_R = linear(sRGB(R) × A) (what we must output)
    ///
    /// In fullscreen mode the blend pipeline handles compositing, so straight
    /// alpha is correct.
    #[inline]
    pub(super) fn gpu_color(&self, rgba: Rgba) -> [f32; 4] {
        let rgba = simplify_flat_alpha(rgba.to_array(), self.degradation_policy.level);
        if self.overlay_mode {
            let a = rgba[3];
            [
                srgb_to_linear(linear_to_srgb(rgba[0]) * a),
                srgb_to_linear(linear_to_srgb(rgba[1]) * a),
                srgb_to_linear(linear_to_srgb(rgba[2]) * a),
                a,
            ]
        } else {
            rgba
        }
    }

    /// Same as [`gpu_color`] but for raw `[f32; 4]` arrays (assumed linear).
    #[inline]
    pub(super) fn gpu_color_raw(&self, color: [f32; 4]) -> [f32; 4] {
        let color = simplify_flat_alpha(color, self.degradation_policy.level);
        if self.overlay_mode {
            let a = color[3];
            [
                srgb_to_linear(linear_to_srgb(color[0]) * a),
                srgb_to_linear(linear_to_srgb(color[1]) * a),
                srgb_to_linear(linear_to_srgb(color[2]) * a),
                a,
            ]
        } else {
            color
        }
    }

    /// Return the clear color for render passes. Transparent in overlay mode,
    /// dark background in fullscreen mode.
    pub(super) fn clear_color(&self) -> wgpu::Color {
        if self.overlay_mode {
            wgpu::Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }
        } else {
            wgpu::Color {
                r: 0.05,
                g: 0.05,
                b: 0.1,
                a: 1.0,
            }
        }
    }

    /// Encode a widget render pass that composites all widget textures into the frame.
    ///
    /// Widget tiles use z_order >= WIDGET_TILE_Z_MIN (0x9000_0000), placing them
    /// above zone tiles but below chrome (spec §Requirement: Widget Contention and
    /// Governance, §Requirement: Widget Input Mode).
    ///
    /// This is a no-op when the widget renderer is not initialized or the registry
    /// has no instances with cached textures.
    pub(super) fn encode_widget_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        registry: &WidgetRegistry,
        surf_w: f32,
        surf_h: f32,
    ) {
        let wr = match &self.widget_renderer {
            Some(r) => r,
            None => return,
        };

        // Check if there are any active widget instances with cached textures.
        if registry.active_publishes.is_empty() {
            return;
        }

        let any_textured = registry
            .active_publishes
            .iter()
            .any(|(name, publishes)| !publishes.is_empty() && wr.texture_entry(name).is_some());
        if !any_textured {
            return;
        }

        // Begin a LoadOp::Load render pass — widgets composite on top of scene content.
        let mut widget_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("widget_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load, // preserve content pixels under widgets
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        wr.composite_widgets(&mut widget_pass, registry, surf_w, surf_h, &self.device);
    }

    /// Collect the scene-side widget draw geometry (hud-uyhpn).
    ///
    /// Thin Compositor-level wrapper over
    /// [`crate::widget::WidgetRenderer::collect_widget_draw_quads`] that also
    /// handles the "no widget renderer" case. Called in the windowed build phase
    /// (under the scene lock) so the widget pass can be encoded lock-free from the
    /// returned owned quads.
    pub(super) fn collect_widget_draw_geometry(
        &self,
        scene: &SceneGraph,
        surf_w: f32,
        surf_h: f32,
    ) -> Vec<crate::widget::WidgetDrawQuad> {
        match &self.widget_renderer {
            Some(wr) => wr.collect_widget_draw_quads(&scene.widget_registry, surf_w, surf_h),
            None => Vec::new(),
        }
    }

    /// Encode the widget pass from precomputed, scene-free quads (hud-uyhpn).
    ///
    /// The scene-free counterpart to [`Self::encode_widget_pass`]. Consumes the
    /// owned [`crate::widget::WidgetDrawQuad`]s produced by
    /// [`Self::collect_widget_draw_geometry`] so the windowed present path never
    /// touches `&scene.widget_registry` after the lock is dropped.
    pub(super) fn encode_widget_pass_prepared(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        quads: &[crate::widget::WidgetDrawQuad],
        surf_w: f32,
        surf_h: f32,
    ) {
        let wr = match &self.widget_renderer {
            Some(r) => r,
            None => return,
        };
        if quads.is_empty() {
            return;
        }

        // Begin a LoadOp::Load render pass — widgets composite on top of scene content.
        let mut widget_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("widget_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
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

        wr.composite_prepared(&mut widget_pass, quads, surf_w, surf_h, &self.device);
    }

    /// Encode a top-most chrome pass for drag-handle quads.
    pub(super) fn encode_drag_handle_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        vertices: &[crate::pipeline::RectVertex],
    ) {
        if vertices.is_empty() {
            return;
        }

        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("drag_handle_vertex_buffer"),
                contents: bytemuck::cast_slice(vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("drag_handle_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
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
            pass.set_pipeline(&self.clear_pipeline);
        } else {
            pass.set_pipeline(&self.pipeline);
        }
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(0..vertices.len() as u32, 0..1);
    }

    /// Encode a render pass for textured image quads.
    ///
    /// Uses `LoadOp::Load` to composite textured images on top of the color
    /// geometry already written to the frame. Each unique `ResourceId` in
    /// `cmds` switches the bind group to the corresponding cached texture.
    pub(super) fn encode_image_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        cmds: &[TexturedDrawCmd],
        sw: f32,
        sh: f32,
    ) {
        if cmds.is_empty() {
            return;
        }

        use wgpu::util::DeviceExt;

        let mut image_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("image_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
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

        image_pass.set_pipeline(&self.texture_rect_pipeline);

        for cmd in cmds {
            let entry = match self.image_texture_cache.get(&cmd.resource_id) {
                Some(e) => e,
                None => continue, // shouldn't happen if ensure was called
            };

            let verts =
                textured_rect_vertices(cmd.x, cmd.y, cmd.w, cmd.h, sw, sh, cmd.uv_rect, cmd.tint);
            let vertex_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("image_quad_buf"),
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                });

            image_pass.set_bind_group(0, &entry.bind_group, &[]);
            image_pass.set_vertex_buffer(0, vertex_buf.slice(..));
            image_pass.draw(0..6, 0..1);
        }
    }

    /// Collect all rounded-rectangle draw commands in a single pass, partitioned by layer.
    ///
    /// Zones with `backdrop_radius` in their `RenderingPolicy` are collected and
    /// partitioned into separate vectors for Background, Content, and Chrome layers.
    /// This replaces three separate calls to `collect_rounded_rect_cmds` with one
    /// efficient pass through the zone registry.
    ///
    /// These zones are excluded from the flat-rect backdrop pass
    /// (`render_zone_content`) and rendered instead by `encode_rounded_rect_pass`
    /// using the SDF pipeline.
    ///
    /// Mirrors the backdrop-resolution logic in `render_zone_content` so color
    /// derivation (severity tokens, urgency colors, opacity) is consistent.
    pub(super) fn collect_all_rounded_rect_cmds(
        &self,
        scene: &SceneGraph,
        sw: f32,
        sh: f32,
    ) -> LayerPartitionedRoundedRectCmds {
        let mut result = LayerPartitionedRoundedRectCmds {
            background: Vec::new(),
            content: Vec::new(),
            chrome: Vec::new(),
        };
        if self.degradation_policy.level >= tze_hud_scene::DegradationLevel::Simplified {
            return result;
        }

        for (zone_name, publishes) in &scene.zone_registry.active_publishes {
            if publishes.is_empty() {
                continue;
            }
            let zone_def = match scene.zone_registry.zones.get(zone_name) {
                Some(z) => z,
                None => continue,
            };

            // Only collect zones with a backdrop_radius — others use the flat rect path.
            let radius = match zone_def.rendering_policy.backdrop_radius {
                Some(r) if r > 0.0 => r,
                _ => continue,
            };

            let policy = &zone_def.rendering_policy;
            let (x, y, w, h) = Self::resolve_zone_geometry(&zone_def.geometry_policy, sw, sh);
            // Clamp radius against the zone's full dimensions as a first-pass
            // upper bound. For Stack zones, each slot height (effective_slot_h)
            // may be smaller than h, so per-slot clamping is applied below.
            let max_r_zone = (w * 0.5).min(h * 0.5).max(0.0);
            let radius = radius.min(max_r_zone);

            let anim_opacity = self
                .zone_animation_states
                .get(zone_name)
                .map(|s| s.current_opacity())
                .unwrap_or(1.0);

            // Resolve backdrop color using the same logic as render_zone_content.
            match zone_def.contention_policy {
                ContentionPolicy::Stack { .. } => {
                    // Slot geometry is computed once by zone_slot_layout and shared
                    // with collect_text_items / render_zone_content (hud-qlerb).
                    let layout = self.zone_slot_layout(zone_name, publishes, policy, h);

                    for (pub_idx, slot_y, effective_slot_h) in layout.iter_visible(y) {
                        let record = &publishes[pub_idx];

                        let pub_opacity = self.pub_opacity(zone_name, record);
                        let combined_opacity = (anim_opacity * pub_opacity).clamp(0.0, 1.0);

                        let is_notification_content =
                            matches!(&record.content, ZoneContent::Notification(_));
                        let backdrop_rgba: Option<Rgba> = match &record.content {
                            ZoneContent::SolidColor(rgba) => Some(*rgba),
                            ZoneContent::StaticImage(_) => Some(STATIC_IMAGE_PLACEHOLDER_COLOR),
                            ZoneContent::Notification(n) if is_alert_banner_zone(zone_name) => {
                                if policy.backdrop.is_some() {
                                    Some(urgency_to_severity_color(n.urgency, &self.token_map))
                                } else {
                                    None
                                }
                            }
                            ZoneContent::Notification(n) => {
                                if policy.backdrop.is_some() {
                                    let mut color =
                                        urgency_to_notification_color(n.urgency, &self.token_map);
                                    color.a = NOTIFICATION_BACKDROP_OPACITY;
                                    Some(color)
                                } else {
                                    None
                                }
                            }
                            _ => policy.backdrop,
                        };

                        if let Some(mut rgba) = backdrop_rgba {
                            if !is_notification_content || is_alert_banner_zone(zone_name) {
                                if let Some(opacity) = policy.backdrop_opacity {
                                    rgba.a = opacity.clamp(0.0, 1.0);
                                }
                            }
                            rgba.a *= combined_opacity;
                            // Re-clamp radius per slot: effective_slot_h may be
                            // smaller than the zone height (e.g., last slot in a
                            // stack), so the radius must not exceed half the slot.
                            let slot_radius =
                                radius.min((w * 0.5).min(effective_slot_h * 0.5).max(0.0));
                            let cmd = RoundedRectDrawCmd {
                                x,
                                y: slot_y,
                                width: w,
                                height: effective_slot_h,
                                radius: slot_radius,
                                color: self.gpu_color(rgba),
                                clip: None,
                            };
                            // Partition by layer
                            match zone_def.layer_attachment {
                                LayerAttachment::Background => result.background.push(cmd),
                                LayerAttachment::Content => result.content.push(cmd),
                                LayerAttachment::Chrome => result.chrome.push(cmd),
                            }
                        }
                    }
                }
                ContentionPolicy::MergeByKey { .. }
                | ContentionPolicy::LatestWins
                | ContentionPolicy::Replace => {
                    let latest = &publishes[publishes.len() - 1];
                    let is_notification_content =
                        matches!(&latest.content, ZoneContent::Notification(_));
                    let backdrop_rgba: Option<Rgba> = match &latest.content {
                        ZoneContent::SolidColor(rgba) => Some(*rgba),
                        ZoneContent::StaticImage(_) => Some(STATIC_IMAGE_PLACEHOLDER_COLOR),
                        ZoneContent::Notification(n) if is_alert_banner_zone(zone_name) => {
                            if policy.backdrop.is_some() {
                                Some(urgency_to_severity_color(n.urgency, &self.token_map))
                            } else {
                                None
                            }
                        }
                        ZoneContent::Notification(n) => {
                            if policy.backdrop.is_some() {
                                let mut color =
                                    urgency_to_notification_color(n.urgency, &self.token_map);
                                color.a = NOTIFICATION_BACKDROP_OPACITY;
                                Some(color)
                            } else {
                                None
                            }
                        }
                        _ => policy.backdrop,
                    };

                    if let Some(mut rgba) = backdrop_rgba {
                        if !is_notification_content || is_alert_banner_zone(zone_name) {
                            if let Some(opacity) = policy.backdrop_opacity {
                                rgba.a = opacity.clamp(0.0, 1.0);
                            }
                        }
                        rgba.a *= anim_opacity.clamp(0.0, 1.0);
                        let cmd = RoundedRectDrawCmd {
                            x,
                            y,
                            width: w,
                            height: h,
                            radius,
                            color: self.gpu_color(rgba),
                            clip: None,
                        };
                        // Partition by layer
                        match zone_def.layer_attachment {
                            LayerAttachment::Background => result.background.push(cmd),
                            LayerAttachment::Content => result.content.push(cmd),
                            LayerAttachment::Chrome => result.chrome.push(cmd),
                        }
                    }
                }
            }
        }

        result
    }

    pub(super) fn collect_tile_rounded_rect_cmds(
        &self,
        scene: &SceneGraph,
    ) -> Vec<crate::pipeline::RoundedRectDrawCmd> {
        if self.degradation_policy.level >= tze_hud_scene::DegradationLevel::Simplified {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        for tile in &Self::sort_tiles_with_drag_boost(self.policy_visible_tiles(scene), scene) {
            if let Some(root_id) = tile.root_node {
                // Compute scroll offset once per tile rather than on every
                // recursive node visit — it is constant across all nodes in
                // the same tile.
                let (scroll_x, scroll_y) = self.display_tile_scroll_offset(scene, tile.id);
                // Compute the whole-tile fade once per tile (drag + §6.3 portal
                // transition) so a rounded backdrop fades with the rest of the tile
                // instead of staying opaque while the flat backdrop/text fade
                // (hud-b0x0m; same class as the hud-w41ef flat-backdrop fix).
                let tile_opacity = self.tile_effective_opacity(tile, scene);
                self.collect_tile_rounded_rect_cmds_from_node(
                    root_id,
                    tile,
                    scene,
                    scroll_x,
                    scroll_y,
                    tile_opacity,
                    &mut cmds,
                );
            }
        }
        cmds
    }

    // `tile_opacity` is the whole-tile fade computed once per tile by the caller
    // and threaded unchanged through the recursion (like `scroll_x`/`scroll_y`),
    // so every rounded backdrop in the subtree fades with the tile (hud-b0x0m).
    #[allow(clippy::too_many_arguments)]
    fn collect_tile_rounded_rect_cmds_from_node(
        &self,
        node_id: SceneId,
        tile: &Tile,
        scene: &SceneGraph,
        scroll_x: f32,
        scroll_y: f32,
        tile_opacity: f32,
        cmds: &mut Vec<crate::pipeline::RoundedRectDrawCmd>,
    ) {
        let node = match scene.nodes.get(&node_id) {
            Some(n) => n,
            None => return,
        };

        if let NodeData::SolidColor(sc) = &node.data {
            if let Some(radius) = sc.radius.filter(|r| *r > 0.0) {
                // hud-xe37d (review finding on hud-pd9bp, confirmed real): a
                // rounded SolidColor is emitted here, not by render_node's flat-
                // quad path (which explicitly skips radius > 0), so it needs the
                // SAME `effective_y` vertical-flow substitution `render_node`
                // applies — otherwise a rounded backdrop stays at its own
                // `bounds.y` while its siblings (and, for a shared node, its own
                // glyphs) move to the resolved stack position. Absent from the
                // map (every node in an Absolute scene) falls back to `sc.bounds.y`,
                // matching the old behavior exactly.
                let effective_y = self
                    .tile_flow_offsets
                    .get(&node_id)
                    .copied()
                    .unwrap_or(sc.bounds.y);
                let (node_scroll_x, node_scroll_y) =
                    if node_uses_display_tile_scroll(scene, tile, node_id) {
                        (scroll_x, scroll_y)
                    } else {
                        (0.0, 0.0)
                    };
                let rect = Rect::new(
                    tile.bounds.x + sc.bounds.x - node_scroll_x,
                    tile.bounds.y + effective_y - node_scroll_y,
                    sc.bounds.width,
                    sc.bounds.height,
                );
                if let Some(clipped) = Self::clip_rect_to_tile(tile, rect) {
                    let max_r = (rect.width * 0.5).min(rect.height * 0.5).max(0.0);
                    cmds.push(crate::pipeline::RoundedRectDrawCmd {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                        radius: radius.min(max_r),
                        color: self.gpu_color(Rgba {
                            a: sc.color.a * tile_opacity,
                            ..sc.color
                        }),
                        clip: Some(RoundedRectClip {
                            x: clipped.x,
                            y: clipped.y,
                            width: clipped.width,
                            height: clipped.height,
                        }),
                    });
                }
            }
        }

        for child_id in &node.children {
            self.collect_tile_rounded_rect_cmds_from_node(
                *child_id,
                tile,
                scene,
                scroll_x,
                scroll_y,
                tile_opacity,
                cmds,
            );
        }
    }

    /// Encode a GPU pass that renders the given rounded-rectangle commands
    /// using the SDF pipeline.
    ///
    /// Uses `LoadOp::Load` so existing scene pixels are preserved beneath
    /// the rounded corners.  Skips the pass entirely when `cmds` is empty.
    pub(super) fn encode_rounded_rect_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        frame_view: &wgpu::TextureView,
        cmds: &[crate::pipeline::RoundedRectDrawCmd],
        sw: f32,
        sh: f32,
    ) {
        if cmds.is_empty() {
            return;
        }

        // Build vertex buffer from all commands.
        let mut vertices: Vec<RoundedRectVertex> = Vec::with_capacity(cmds.len() * 6);
        for cmd in cmds {
            if cmd.width <= 0.0 || cmd.height <= 0.0 {
                continue;
            }
            if let Some(clip) = cmd.clip {
                if clip.width <= 0.0 || clip.height <= 0.0 {
                    continue;
                }
                vertices.extend_from_slice(&rounded_rect_vertices_with_draw_bounds(
                    clip.x,
                    clip.y,
                    clip.width,
                    clip.height,
                    cmd.x,
                    cmd.y,
                    cmd.width,
                    cmd.height,
                    sw,
                    sh,
                    cmd.radius,
                    cmd.color,
                ));
            } else {
                vertices.extend_from_slice(&rounded_rect_vertices(
                    cmd.x, cmd.y, cmd.width, cmd.height, sw, sh, cmd.radius, cmd.color,
                ));
            }
        }
        if vertices.is_empty() {
            return;
        }

        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("rounded_rect_vertex_buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rounded_rect_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load, // preserve background pixels
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        // In overlay mode select the premultiplied pipeline; otherwise use the
        // straight-alpha pipeline.  Vertex colors are premultiplied by gpu_color
        // in overlay mode, so the blend equation and shader must match.
        let pipeline = if self.overlay_mode {
            &self.rounded_rect_overlay_pipeline
        } else {
            &self.rounded_rect_pipeline
        };
        pass.set_pipeline(pipeline);
        pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        pass.draw(0..vertices.len() as u32, 0..1);
    }
}

#[cfg(test)]
mod degradation_flat_alpha_tests {
    use super::simplify_flat_alpha;
    use tze_hud_scene::DegradationLevel;

    #[test]
    fn significant_policy_forces_visible_flat_geometry_opaque_but_preserves_clear() {
        assert_eq!(
            simplify_flat_alpha([0.2, 0.3, 0.4, 0.25], DegradationLevel::Nominal),
            [0.2, 0.3, 0.4, 0.25]
        );
        assert_eq!(
            simplify_flat_alpha([0.2, 0.3, 0.4, 0.25], DegradationLevel::Simplified,),
            [0.2, 0.3, 0.4, 1.0]
        );
        assert_eq!(
            simplify_flat_alpha([0.0, 0.0, 0.0, 0.0], DegradationLevel::Simplified),
            [0.0, 0.0, 0.0, 0.0]
        );
    }
}
