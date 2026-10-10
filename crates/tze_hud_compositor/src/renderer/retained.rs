//! Narrow retained-render path for the canonical headless scene.
//!
//! It owns a private compositor snapshot and accepts only the bounded
//! fifty-tile headless scene: one tile's text changes and only that tile (plus
//! a z-higher translucent overlap, if any) is repainted inside a scissor. Every
//! other scene stays on the established full-frame renderer. Opt-in dev/test
//! observations record shaping, widget raster/upload work and repaint area.

use std::collections::BTreeSet;
use std::time::Instant;

use wgpu::util::DeviceExt;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{DragHandleElementKind, NodeData, Rect, SceneId, TextMarkdownNode};
#[cfg(any(test, feature = "dev-mode"))]
use tze_hud_telemetry::WorkCounts;

use crate::pipeline::rect_vertices;
use crate::surface::{CompositorSurface, HeadlessSurface};
use crate::text::TextItem;

use super::Compositor;

const CANONICAL_TILE_COUNT: usize = 50;

/// Integer pixel rectangle in the presentation viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PixelRect {
    #[cfg(any(test, feature = "dev-mode"))]
    fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Compositor-private retained state for the canonical headless lane.
#[derive(Default)]
pub(super) struct RetainedRenderState {
    snapshot: Option<CanonicalSceneSnapshot>,
    #[cfg(any(test, feature = "dev-mode"))]
    latest_work: Option<WorkCounts>,
    #[cfg(any(test, feature = "dev-mode"))]
    frame_start_work: WorkTotals,
}

/// Cumulative owner counts sampled only at actual headless render boundaries.
#[cfg(any(test, feature = "dev-mode"))]
#[derive(Clone, Copy, Default)]
struct WorkTotals {
    layout: u64,
    raster: u64,
    upload: u64,
}

#[derive(Clone, PartialEq)]
struct CanonicalSceneSnapshot {
    viewport: (u32, u32),
    scenario: CanonicalScenario,
    tiles: Vec<CanonicalTextTile>,
}

/// The only retained evidence layouts this private headless lane understands.
///
/// The transparent variant is deliberately a single dependency-expansion
/// vector, not a general overlap planner: it proves that a z-higher translucent
/// contributor is named and repainted without widening the product claim.
#[derive(Clone, PartialEq)]
enum CanonicalScenario {
    OpaqueNonOverlapping,
    TransparentOverlap {
        lower_tile_id: SceneId,
        upper_tile_id: SceneId,
        overlap_bounds: Rect,
    },
}

#[derive(Clone, PartialEq)]
struct CanonicalTextTile {
    tile_id: SceneId,
    node_id: SceneId,
    tile_bounds: Rect,
    z_order: u32,
    opacity: f32,
    text: TextMarkdownNode,
}

struct CanonicalTextChange {
    redraw_tiles: Vec<CanonicalTextTile>,
    damage: PixelRect,
    next_snapshot: CanonicalSceneSnapshot,
}

/// Plan a scoped repaint from the prior private snapshot and the live scene.
/// `None` means the caller must run the full-frame renderer.
fn plan_headless(
    state: &RetainedRenderState,
    scene: &SceneGraph,
    width: u32,
    height: u32,
) -> Option<Box<CanonicalTextChange>> {
    let previous = state.snapshot.as_ref()?;
    let next = canonical_snapshot(scene, width, height)?;
    if previous.viewport != next.viewport || previous == &next {
        return None;
    }
    let change = planned_canonical_text_change(previous, &next)?;
    Some(Box::new(CanonicalTextChange {
        next_snapshot: next,
        ..change
    }))
}

impl RetainedRenderState {
    fn remember_full_headless_scene(&mut self, scene: &SceneGraph, width: u32, height: u32) {
        self.snapshot = canonical_snapshot(scene, width, height);
    }

    fn forget_snapshot(&mut self) {
        self.snapshot = None;
    }

    /// Work counts are a single-frame drain, not a history: a new frame must
    /// never leave the previous frame's counts available to be misattributed.
    #[cfg(any(test, feature = "dev-mode"))]
    fn begin_observation_frame(&mut self) {
        self.latest_work = None;
    }

    /// Invalidate the retained baseline after a real production surface
    /// recovery. The following frame is a full repaint.
    pub(super) fn note_device_recovery(&mut self) {
        self.snapshot = None;
    }
}

impl Compositor {
    #[cfg(any(test, feature = "dev-mode"))]
    fn current_work_totals(&self) -> WorkTotals {
        let (raster, upload) = self
            .widget_renderer
            .as_ref()
            .map_or((0, 0), |renderer| renderer.work_totals());
        WorkTotals {
            layout: self.text_shape_call_count(),
            raster,
            upload,
        }
    }

    /// Start once at actual render entry, before retained preparation can fail
    /// and fall back. No frame invocation means no newly sampled observation.
    #[cfg(any(test, feature = "dev-mode"))]
    pub(super) fn begin_headless_work_observation(&mut self) {
        self.retained_render_state.begin_observation_frame();
        self.retained_render_state.frame_start_work = self.current_work_totals();
    }

    #[cfg(any(test, feature = "dev-mode"))]
    fn record_headless_work(&mut self, tiles: u32, damage_px: u64, full_frame: bool) {
        let before = self.retained_render_state.frame_start_work;
        let after = self.current_work_totals();
        self.retained_render_state.latest_work = Some(WorkCounts {
            layout: after.layout - before.layout,
            raster: after.raster - before.raster,
            upload: after.upload - before.upload,
            damage_px,
            tiles_redrawn: tiles,
            pixels_damaged: damage_px,
            full_frame,
        });
    }

    /// Try the retained canonical headless path before falling back to the
    /// ordinary full-frame renderer. `None` always means the caller must run
    /// the existing path.
    pub(super) fn try_render_retained_headless(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
    ) -> Option<tze_hud_telemetry::FrameTelemetry> {
        if !self.retained_headless_policy_is_supported() {
            // A full-frame degradation policy can alter visibility or raster
            // semantics. Its submitted pixels are not a retained baseline, so
            // invalidate the private snapshot rather than repainting against
            // divergent content.
            self.retained_render_state.forget_snapshot();
            return None;
        }
        let (width, height) = surface.size();
        let change = *plan_headless(&self.retained_render_state, scene, width, height)?;

        let Some(telemetry) = self.render_retained_text_change(scene, surface, &change) else {
            self.retained_render_state.forget_snapshot();
            return None;
        };

        #[cfg(any(test, feature = "dev-mode"))]
        self.record_headless_work(
            change.redraw_tiles.len() as u32,
            change.damage.area(),
            false,
        );
        self.retained_render_state.snapshot = Some(change.next_snapshot);
        Some(telemetry)
    }

    /// Record the result of an ordinary headless frame. This seeds (or
    /// refreshes) the private retained snapshot only after the real full frame
    /// was submitted.
    pub(super) fn observe_full_headless_frame(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
    ) {
        let (width, height) = surface.size();
        if self.retained_headless_policy_is_supported() {
            self.retained_render_state
                .remember_full_headless_scene(scene, width, height);
        } else {
            self.retained_render_state.forget_snapshot();
        }
        #[cfg(any(test, feature = "dev-mode"))]
        self.record_headless_work(
            scene.visible_tiles().len() as u32,
            u64::from(width) * u64::from(height),
            true,
        );
    }

    /// Drain actual shaping, widget raster/upload and repaint work from the
    /// most recent completed headless frame, including failed preparation
    /// before full fallback. A second drain returns `None`.
    ///
    /// Rust-only dev/test hook; it does not touch scene, gRPC, or protobuf
    /// contracts.
    #[cfg(any(test, feature = "dev-mode"))]
    pub fn take_work_counts(&mut self) -> Option<WorkCounts> {
        self.retained_render_state.latest_work.take()
    }

    fn render_retained_text_change(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
        change: &CanonicalTextChange,
    ) -> Option<tze_hud_telemetry::FrameTelemetry> {
        let frame_start = Instant::now();
        let (width, height) = surface.size();
        let damage = change.damage;
        let mut background_vertices = Vec::with_capacity(change.redraw_tiles.len() * 6);
        let mut background_ranges = Vec::with_capacity(change.redraw_tiles.len());
        let text_items = {
            // The normal render pipeline commit-primes this cache before reaching
            // the retained path. Decline to the full renderer if that invariant is
            // unavailable; this proof lane must not reintroduce lossy markdown or
            // an unbounded parse on its evidence frame.
            let markdown_cache = self.markdown_cache();
            let mut text_items = Vec::with_capacity(change.redraw_tiles.len());
            for tile in &change.redraw_tiles {
                let current_tile = scene.tiles.get(&tile.tile_id)?;
                let current_node = scene.nodes.get(&tile.node_id)?;
                let NodeData::TextMarkdown(current_text) = &current_node.data else {
                    return None;
                };
                if current_text != &tile.text
                    || !current_node.children.is_empty()
                    || current_tile.bounds != tile.tile_bounds
                    || current_tile.z_order != tile.z_order
                    || current_tile.opacity != tile.opacity
                {
                    return None;
                }

                let background_color = self.tile_background_color(current_tile, scene)?;
                let start = background_vertices.len() as u32;
                background_vertices.extend_from_slice(&rect_vertices(
                    tile.tile_bounds.x,
                    tile.tile_bounds.y,
                    tile.tile_bounds.width,
                    tile.tile_bounds.height,
                    width as f32,
                    height as f32,
                    self.gpu_color_raw(background_color),
                ));
                background_ranges.push(start..background_vertices.len() as u32);

                let content_key = self.node_key_cache.get(&tile.node_id).copied()?;
                let parsed = markdown_cache.get_by_key(&content_key)?;
                let mut text_item = TextItem::from_text_markdown_cached(
                    current_text,
                    tile.tile_bounds.x,
                    tile.tile_bounds.y,
                    parsed,
                );
                // `collect_text_items` applies whole-tile opacity to glyphs;
                // mirror that production rule so the translucent z-higher
                // contributor is repainted exactly like the full frame.
                text_item.opacity *= self.tile_effective_opacity(current_tile, scene);
                text_items.push(text_item);
            }
            text_items
        };
        let background_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("retained_change_background"),
                contents: bytemuck::cast_slice(&background_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let inline_backdrops = {
            let text_rasterizer = self.text_rasterizer.as_mut()?;
            text_rasterizer.update_viewport(&self.queue, width, height);
            text_rasterizer
                .prepare_text_items(&self.device, &self.queue, &text_items)
                .ok()?
        };
        // The canonical lane admits plain text only; inline markdown backdrops
        // would require an extra scoped pass, so fail closed to the full path.
        if !inline_backdrops.is_empty() {
            return None;
        }

        // A normal frame renders the passive drag grips in its top chrome pass.
        // The canonical scene has neither portals nor active widget/zone content,
        // so selecting only these already-declared closure tiles is equivalent to
        // that pass for the damaged pixels. Dynamic grip state is rejected by
        // `canonical_snapshot`; do not generalize this to arbitrary chrome.
        let all_drag_handles = self.collect_drag_handle_entries(scene, width as f32, height as f32);
        // A grip straddles its tile's top edge, so a tile body outside the
        // closure can still have chrome inside the scoped damage. This narrow
        // lane repairs chrome only for declared closure tiles; decline rather
        // than silently overwrite a non-closure grip fragment.
        if all_drag_handles.iter().any(|entry| {
            !change
                .redraw_tiles
                .iter()
                .any(|tile| tile.tile_id == entry.element_id)
                && pixel_rect_for_bounds(entry.bounds, width, height)
                    .is_none_or(|bounds| pixel_rects_intersect(bounds, damage))
        }) {
            return None;
        }
        let drag_handles: Vec<_> = all_drag_handles
            .into_iter()
            .filter(|entry| {
                change
                    .redraw_tiles
                    .iter()
                    .any(|tile| tile.tile_id == entry.element_id)
            })
            .collect();
        if drag_handles.len() != change.redraw_tiles.len()
            || drag_handles
                .iter()
                .zip(&change.redraw_tiles)
                .any(|(entry, tile)| {
                    entry.element_id != tile.tile_id
                        || entry.element_kind != DragHandleElementKind::Tile
                        || entry.is_header_band
                })
        {
            return None;
        }
        // An active-drag highlight needs the SDF pass this lane does not run.
        if !self.drag_highlight_cmds(scene, &drag_handles).is_empty() {
            return None;
        }
        let mut drag_handle_vertices = Vec::new();
        self.append_drag_handle_vertices(
            scene,
            &drag_handles,
            &mut drag_handle_vertices,
            width as f32,
            height as f32,
        );
        if drag_handle_vertices.is_empty() {
            return None;
        }
        let drag_handle_buffer =
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("retained_change_drag_handle"),
                    contents: bytemuck::cast_slice(&drag_handle_vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });

        let frame = surface.acquire_frame()?;
        let encode_start = Instant::now();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("retained_change_encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("retained_change_background_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Retain every unaffected pixel from the submitted baseline.
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_scissor_rect(damage.x, damage.y, damage.width, damage.height);
            if self.use_opaque_rect_pipeline() {
                pass.set_pipeline(&self.clear_pipeline);
            } else {
                pass.set_pipeline(&self.pipeline);
            }
            pass.set_vertex_buffer(0, background_buffer.slice(..));
            for range in background_ranges {
                pass.draw(range, 0..1);
            }
        }
        {
            let text_rasterizer = self.text_rasterizer.as_ref()?;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("retained_change_text_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
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
            pass.set_scissor_rect(damage.x, damage.y, damage.width, damage.height);
            text_rasterizer.render_text_pass(&mut pass).ok()?;
        }
        if let Some(text_rasterizer) = self.text_rasterizer.as_mut() {
            text_rasterizer.trim_atlas();
        }
        {
            // Repaint only the passive grips belonging to closure members. The
            // scissor preserves every unaffected grip pixel; it repairs precisely
            // the fragments overwritten by the retained tile backgrounds.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("retained_change_drag_handle_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
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
            pass.set_scissor_rect(damage.x, damage.y, damage.width, damage.height);
            if self.use_opaque_rect_pipeline() {
                pass.set_pipeline(&self.clear_pipeline);
            } else {
                pass.set_pipeline(&self.pipeline);
            }
            pass.set_vertex_buffer(0, drag_handle_buffer.slice(..));
            pass.draw(0..drag_handle_vertices.len() as u32, 0..1);
        }

        // The readback copy preserves HeadlessRuntime's normal contract.  It is
        // not a render encode or texture upload and does not expand the damage.
        surface.copy_to_buffer(&mut encoder);
        let stage6_render_encode_us = encode_start.elapsed().as_micros().max(1) as u64;
        let submit_start = Instant::now();
        self.queue.submit(std::iter::once(encoder.finish()));
        surface.present();
        drop(frame);
        self.device.poll(wgpu::Maintain::Wait);
        let stage7_gpu_submit_us = submit_start.elapsed().as_micros().max(1) as u64;

        let mut telemetry = tze_hud_telemetry::FrameTelemetry::new(self.frame_number);
        telemetry.tile_count = CANONICAL_TILE_COUNT as u32;
        telemetry.node_count = CANONICAL_TILE_COUNT as u32;
        telemetry.active_leases = scene.leases.len() as u32;
        telemetry.tiles_layout_recomputed = change.redraw_tiles.len() as u32;
        telemetry.stage6_render_encode_us = stage6_render_encode_us;
        telemetry.stage7_gpu_submit_us = stage7_gpu_submit_us;
        telemetry.frame_time_us = frame_start.elapsed().as_micros().max(1) as u64;
        Some(telemetry)
    }

    fn retained_headless_policy_is_supported(&self) -> bool {
        self.degradation_policy.level == tze_hud_scene::DegradationLevel::Nominal
            // The retained proof can reproduce only the deterministic idle
            // drag-grip layer below. All other compositor-owned chrome remains
            // a full-frame concern until it has its own bounded evidence lane.
            && self.focus_ring_owner.is_none()
            && self.resize_grip_hover.is_none()
            && self.tile_close_hover.is_none()
            && self.local_composer.is_none()
            && self.system_card.is_none()
    }
}

fn canonical_snapshot(
    scene: &SceneGraph,
    width: u32,
    height: u32,
) -> Option<CanonicalSceneSnapshot> {
    if width == 0
        || height == 0
        || !scene.zone_registry.active_publishes.is_empty()
        || !scene.widget_registry.active_publishes.is_empty()
        || !scene.overlay.tile_scroll_configs.is_empty()
        || !scene.overlay.tile_scroll_offsets.is_empty()
        || !scene.overlay.displayed_tile_scroll_offsets.is_empty()
        || !scene.overlay.tile_follow_tail_at_tail.is_empty()
        || !scene.overlay.tile_unread_counts.is_empty()
        || !scene.overlay.tile_lifecycle_accents.is_empty()
        || !scene.overlay.tile_composer_interactions.is_empty()
        // Idle hit regions are an output of the full baseline frame and do not
        // affect pixels. A local hover/press state does, so retained rendering
        // must decline rather than silently repaint it as idle chrome.
        || !scene.overlay.drag_handle_states.is_empty()
        || !scene.overlay.drag_active_elements.is_empty()
        || scene.overlay.drag_handle_context_menu.is_some()
        || !scene.overlay.portal_surfaces.is_empty()
        || !scene.overlay.tile_font_scale.is_empty()
    {
        return None;
    }

    let visible_tiles = scene.visible_tiles();
    if visible_tiles.len() != CANONICAL_TILE_COUNT || scene.nodes.len() != CANONICAL_TILE_COUNT {
        return None;
    }

    let mut tiles = Vec::with_capacity(CANONICAL_TILE_COUNT);
    for tile in visible_tiles {
        // This narrow pass cannot repaint the full renderer's disconnect badge.
        if tile.visual_hint == tze_hud_scene::lease::TileVisualHint::DisconnectionBadge
            || !tile.opacity.is_finite()
            || !(0.0..=1.0).contains(&tile.opacity)
            || !rect_is_inside_viewport(tile.bounds, width, height)
        {
            return None;
        }
        let root_id = tile.root_node?;
        let node = scene.nodes.get(&root_id)?;
        if !node.children.is_empty() {
            return None;
        }
        let NodeData::TextMarkdown(text) = &node.data else {
            return None;
        };
        if !is_canonical_plain_ascii_content(&text.content)
            || text.background.is_some()
            || !text.color_runs.is_empty()
            || !matches!(text.overflow, tze_hud_scene::types::TextOverflow::Clip)
            || !rect_is_inside_tile(text.bounds, tile.bounds)
        {
            return None;
        }
        tiles.push(CanonicalTextTile {
            tile_id: tile.id,
            node_id: root_id,
            tile_bounds: tile.bounds,
            z_order: tile.z_order,
            opacity: tile.opacity,
            text: text.clone(),
        });
    }
    tiles.sort_by_key(|tile| tile.tile_id);
    let scenario = classify_canonical_scenario(&tiles)?;
    Some(CanonicalSceneSnapshot {
        viewport: (width, height),
        scenario,
        tiles,
    })
}

fn classify_canonical_scenario(tiles: &[CanonicalTextTile]) -> Option<CanonicalScenario> {
    let intersections: Vec<_> = tiles
        .iter()
        .enumerate()
        .flat_map(|(index, tile)| {
            tiles[index + 1..]
                .iter()
                .filter(move |other| tile.tile_bounds.intersects(&other.tile_bounds))
                .map(move |other| (tile, other))
        })
        .collect();

    if intersections.is_empty() {
        return tiles
            .iter()
            .all(|tile| tile.opacity == 1.0)
            .then_some(CanonicalScenario::OpaqueNonOverlapping);
    }

    let [(first, second)] = intersections.as_slice() else {
        return None;
    };
    let (lower, upper) = if first.z_order < second.z_order {
        (*first, *second)
    } else if second.z_order < first.z_order {
        (*second, *first)
    } else {
        return None;
    };
    if lower.opacity != 1.0
        || !(upper.opacity > 0.0 && upper.opacity < 1.0)
        || tiles.iter().any(|tile| {
            tile.tile_id != lower.tile_id && tile.tile_id != upper.tile_id && tile.opacity != 1.0
        })
    {
        return None;
    }
    let overlap_bounds = intersection_rect(lower.tile_bounds, upper.tile_bounds)?;
    Some(CanonicalScenario::TransparentOverlap {
        lower_tile_id: lower.tile_id,
        upper_tile_id: upper.tile_id,
        overlap_bounds,
    })
}

/// The canonical proof deliberately accepts only text that the Markdown parser
/// cannot reinterpret into additional glyphs or styles. Broader Markdown
/// retained rendering needs parsed-style inventory accounting rather than the
/// raw-byte glyph check below.
fn is_canonical_plain_ascii_content(content: &str) -> bool {
    !content.is_empty()
        && content
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b' ')
}

fn planned_canonical_text_change(
    previous: &CanonicalSceneSnapshot,
    current: &CanonicalSceneSnapshot,
) -> Option<CanonicalTextChange> {
    if previous.scenario != current.scenario {
        return None;
    }
    let changed = changed_canonical_tile(previous, current)?;
    match &current.scenario {
        CanonicalScenario::OpaqueNonOverlapping => {
            let damage =
                pixel_rect_for_bounds(changed.tile_bounds, current.viewport.0, current.viewport.1)?;
            Some(CanonicalTextChange {
                redraw_tiles: vec![changed],
                damage,
                next_snapshot: current.clone(),
            })
        }
        CanonicalScenario::TransparentOverlap {
            lower_tile_id,
            upper_tile_id,
            overlap_bounds,
        } => {
            if changed.tile_id != *lower_tile_id {
                return None;
            }
            let upper = current
                .tiles
                .iter()
                .find(|tile| tile.tile_id == *upper_tile_id)?
                .clone();
            let mut redraw_tiles = vec![changed.clone(), upper];
            redraw_tiles.sort_by_key(|tile| tile.z_order);
            if redraw_tiles
                .first()
                .is_none_or(|tile| tile.tile_id != changed.tile_id)
            {
                return None;
            }
            let damage = pixel_rect_for_bounds(
                union_rect(changed.tile_bounds, *overlap_bounds),
                current.viewport.0,
                current.viewport.1,
            )?;
            Some(CanonicalTextChange {
                redraw_tiles,
                damage,
                next_snapshot: current.clone(),
            })
        }
    }
}

fn changed_canonical_tile(
    previous: &CanonicalSceneSnapshot,
    current: &CanonicalSceneSnapshot,
) -> Option<CanonicalTextTile> {
    if previous.tiles.len() != current.tiles.len() {
        return None;
    }
    let mut changed = None;
    for (before, after) in previous.tiles.iter().zip(&current.tiles) {
        if before.tile_id != after.tile_id
            || before.node_id != after.node_id
            || before.tile_bounds != after.tile_bounds
            || before.z_order != after.z_order
            || before.opacity != after.opacity
        {
            return None;
        }
        if before.text.content == after.text.content {
            if before.text != after.text {
                return None;
            }
            continue;
        }
        let mut before_with_new_content = before.text.clone();
        before_with_new_content.content = after.text.content.clone();
        if before_with_new_content != after.text
            || !same_ascii_glyph_inventory(&before.text.content, &after.text.content)
            || changed.replace(after.clone()).is_some()
        {
            return None;
        }
    }
    changed
}

fn intersection_rect(first: Rect, second: Rect) -> Option<Rect> {
    if !first.intersects(&second) {
        return None;
    }
    let left = first.x.max(second.x);
    let top = first.y.max(second.y);
    let right = (first.x + first.width).min(second.x + second.width);
    let bottom = (first.y + first.height).min(second.y + second.height);
    (right > left && bottom > top).then_some(Rect::new(left, top, right - left, bottom - top))
}

fn union_rect(first: Rect, second: Rect) -> Rect {
    let left = first.x.min(second.x);
    let top = first.y.min(second.y);
    let right = (first.x + first.width).max(second.x + second.width);
    let bottom = (first.y + first.height).max(second.y + second.height);
    Rect::new(left, top, right - left, bottom - top)
}

fn same_ascii_glyph_inventory(before: &str, after: &str) -> bool {
    let before_glyphs: BTreeSet<_> = before.bytes().collect();
    let after_glyphs: BTreeSet<_> = after.bytes().collect();
    before_glyphs == after_glyphs
}

fn rect_is_inside_viewport(rect: Rect, width: u32, height: u32) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width > 0.0
        && rect.height > 0.0
        && rect.x >= 0.0
        && rect.y >= 0.0
        && rect.x + rect.width <= width as f32
        && rect.y + rect.height <= height as f32
}

fn rect_is_inside_tile(node_bounds: Rect, tile_bounds: Rect) -> bool {
    node_bounds.x.is_finite()
        && node_bounds.y.is_finite()
        && node_bounds.width.is_finite()
        && node_bounds.height.is_finite()
        && node_bounds.width > 0.0
        && node_bounds.height > 0.0
        && node_bounds.x >= 0.0
        && node_bounds.y >= 0.0
        && node_bounds.x + node_bounds.width <= tile_bounds.width
        && node_bounds.y + node_bounds.height <= tile_bounds.height
}

fn pixel_rect_for_bounds(bounds: Rect, width: u32, height: u32) -> Option<PixelRect> {
    if !rect_is_inside_viewport(bounds, width, height) {
        return None;
    }
    let left = bounds.x.floor() as u32;
    let top = bounds.y.floor() as u32;
    let right = (bounds.x + bounds.width).ceil() as u32;
    let bottom = (bounds.y + bounds.height).ceil() as u32;
    let pixel_rect = PixelRect {
        x: left,
        y: top,
        width: right.checked_sub(left)?,
        height: bottom.checked_sub(top)?,
    };
    (pixel_rect.width > 0 && pixel_rect.height > 0).then_some(pixel_rect)
}

/// Whether two half-open pixel rectangles share any rasterized pixels.
///
/// Overflow is impossible for valid viewport rectangles, but returning `true`
/// on malformed bounds keeps this proof-only retained path fail-closed.
fn pixel_rects_intersect(first: PixelRect, second: PixelRect) -> bool {
    let (Some(first_right), Some(first_bottom), Some(second_right), Some(second_bottom)) = (
        first.x.checked_add(first.width),
        first.y.checked_add(first.height),
        second.x.checked_add(second.width),
        second.y.checked_add(second.height),
    ) else {
        return true;
    };

    first.x < second_right
        && first_right > second.x
        && first.y < second_bottom
        && first_bottom > second.y
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::types::{FontFamily, Node, Rgba, TextAlign, TextOverflow};

    fn canonical_scene_with_first_content(first_content: &str) -> SceneGraph {
        let mut scene = SceneGraph::new(1_000.0, 500.0);
        let tab_id = scene.create_tab("canonical", 0).expect("canonical tab");
        let lease_id = scene.grant_lease("canonical-agent", 60_000);

        for index in 0..CANONICAL_TILE_COUNT {
            let column = index % 10;
            let row = index / 10;
            let tile_id = scene
                .create_tile(
                    tab_id,
                    "canonical-agent",
                    lease_id,
                    Rect::new((column * 100) as f32, (row * 100) as f32, 100.0, 100.0),
                    index as u32,
                )
                .expect("canonical tile");
            let content = if index == 0 { first_content } else { "AB" };
            scene
                .set_tile_root(
                    tile_id,
                    Node {
                        id: SceneId::new(),
                        children: vec![],
                        layout: Default::default(),
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            content: content.into(),
                            bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                            font_size_px: 18.0,
                            font_family: FontFamily::SystemSansSerif,
                            color: Rgba::WHITE,
                            background: None,
                            alignment: TextAlign::Start,
                            overflow: TextOverflow::Clip,
                            color_runs: Box::default(),
                        }),
                    },
                )
                .expect("canonical text root");
        }

        scene
    }

    #[test]
    fn markdown_bearing_text_fails_closed_before_retained_planning() {
        let markdown_scene = canonical_scene_with_first_content("*AB*");
        let plain_scene = canonical_scene_with_first_content("AB");

        assert!(
            canonical_snapshot(&markdown_scene, 1_000, 500).is_none(),
            "raw Markdown markers must not be admitted to a glyph-inventory-only retained proof"
        );
        assert!(
            canonical_snapshot(&plain_scene, 1_000, 500).is_some(),
            "the plain-text canonical control scene must remain eligible"
        );
    }

    #[test]
    fn dynamic_drag_handle_state_fails_closed_before_retained_planning() {
        let mut scene = canonical_scene_with_first_content("AB");
        scene
            .overlay
            .drag_handle_states
            .insert("drag-handle:canonical".into(), Default::default());

        assert!(
            canonical_snapshot(&scene, 1_000, 500).is_none(),
            "hover or press chrome must remain on the established full-frame path"
        );

        let mut lifecycle_scene = canonical_scene_with_first_content("AB");
        let previous = canonical_snapshot(&lifecycle_scene, 1_000, 500)
            .expect("the live plain-text control is retained-eligible");
        let tile_id = previous.tiles[0].tile_id;
        let node_id = previous.tiles[0].node_id;
        let lease_id = lifecycle_scene.tiles[&tile_id].lease_id;
        let retained = RetainedRenderState {
            snapshot: Some(previous),
            ..Default::default()
        };
        let live_version = lifecycle_scene.version;

        lifecycle_scene.disconnect_lease(&lease_id, 1_000).unwrap();
        assert!(lifecycle_scene.version > live_version);
        assert!(lifecycle_scene.tiles.values().all(|tile| {
            tile.visual_hint == tze_hud_scene::lease::TileVisualHint::DisconnectionBadge
        }));
        assert!(
            canonical_snapshot(&lifecycle_scene, 1_000, 500).is_none(),
            "orphan badge chrome must remain on the full-frame path"
        );
        let NodeData::TextMarkdown(text) =
            &mut lifecycle_scene.nodes.get_mut(&node_id).unwrap().data
        else {
            panic!("canonical tile must keep its text node on disconnect");
        };
        assert_eq!(text.content, "AB", "disconnect keeps the tile's content");
        // A same-glyph content permutation would otherwise select a scoped
        // repaint. This is a CPU planner input, not an orphan-agent mutation.
        text.content = "BA".into();
        assert!(
            plan_headless(&retained, &lifecycle_scene, 1_000, 500).is_none(),
            "a text change must not bypass the orphan-badge fallback"
        );
        let orphan_version = lifecycle_scene.version;

        lifecycle_scene.reconnect_lease(&lease_id, 1_001).unwrap();
        assert!(lifecycle_scene.version > orphan_version);
        assert!(lifecycle_scene
            .tiles
            .values()
            .all(|tile| tile.visual_hint == tze_hud_scene::lease::TileVisualHint::None));
        assert!(
            canonical_snapshot(&lifecycle_scene, 1_000, 500).is_some(),
            "the same resumed surfaces become retained-eligible again"
        );
        assert!(
            plan_headless(&retained, &lifecycle_scene, 1_000, 500).is_some(),
            "the identical text permutation is a valid live retained control"
        );
    }

    #[test]
    fn new_observation_clears_prior_frame_work_counts() {
        let mut state = RetainedRenderState {
            latest_work: Some(WorkCounts {
                layout: 1,
                raster: 2,
                upload: 1,
                damage_px: 100,
                tiles_redrawn: 1,
                pixels_damaged: 100,
                full_frame: false,
            }),
            ..RetainedRenderState::default()
        };

        state.begin_observation_frame();

        assert!(state.latest_work.is_none());
    }

    #[test]
    fn device_recovery_invalidates_the_private_snapshot() {
        let mut state = RetainedRenderState {
            snapshot: Some(CanonicalSceneSnapshot {
                viewport: (1_000, 500),
                scenario: CanonicalScenario::OpaqueNonOverlapping,
                tiles: vec![],
            }),
            ..RetainedRenderState::default()
        };

        state.note_device_recovery();

        assert!(state.snapshot.is_none());
    }
}
