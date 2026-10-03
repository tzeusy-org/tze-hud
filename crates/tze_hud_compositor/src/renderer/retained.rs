//! Narrow retained-render path used by the canonical change-efficiency proof.
//!
//! This module deliberately does not consume any reconnect/WAL
//! protocol state.  It owns a private compositor snapshot and accepts only the
//! bounded fifty-tile headless scene used by the Layer 3 evidence lane.  Every
//! other scene stays on the established full-frame renderer.

use std::collections::BTreeSet;
use std::time::Instant;

use wgpu::util::DeviceExt;

use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{DragHandleElementKind, NodeData, Rect, SceneId, TextMarkdownNode};
use tze_hud_telemetry::{
    ActualWorkItem, ChangeEfficiencyArtifact, ChangeMeasurementProvenance, ChangeMeasurementStatus,
    ChangeRenderWorkObservation, ClosureWorkItem, EfficiencyPacingIdentity, EfficiencyPacingMode,
    EfficiencyRendererIdentity, EfficiencyRuntimeIdentity, EfficiencyScenarioIdentity,
    EfficiencyViewport, EfficiencyWindowMode, FullSurfaceInvalidation,
    FullSurfaceInvalidationReason, InvalidationCategory, InvalidationClosure,
    InvalidationDependencyReason, NodeWorkItemId, PartialPresentCapability, PixelRect,
    RenderPlanWorkItemId, TextureUploadCategory,
};

use crate::pipeline::rect_vertices;
use crate::surface::{CompositorSurface, HeadlessSurface};
use crate::text::TextItem;

use super::Compositor;

const CANONICAL_TILE_COUNT: usize = 50;

/// Compositor-private retained state for the canonical headless proof lane.
#[derive(Default)]
pub(super) struct RetainedRenderState {
    snapshot: Option<CanonicalSceneSnapshot>,
    pending_full_surface_diagnostic: Option<FullSurfaceInvalidation>,
    latest_capture: Option<RetainedChangeEfficiencyCapture>,
    latest_diagnostic: Option<ChangeEfficiencyArtifact>,
    device_recovery_pending: bool,
}

/// Opaque proof returned only by the compositor after the retained headless
/// renderer has encoded and submitted scoped work. A serialized
/// `ChangeEfficiencyArtifact` cannot manufacture this wrapper.
#[derive(Debug)]
#[must_use = "a retained runtime capture should be validated or inspected"]
pub struct RetainedChangeEfficiencyCapture {
    artifact: ChangeEfficiencyArtifact,
}

impl RetainedChangeEfficiencyCapture {
    fn from_observed_runtime(artifact: ChangeEfficiencyArtifact) -> Self {
        debug_assert_eq!(
            artifact.measurement_provenance,
            ChangeMeasurementProvenance::ObservedRetainedRuntime,
            "only a real retained runtime observation may create a capture"
        );
        Self { artifact }
    }

    /// Inspect the immutable observed-operation artifact recorded by the
    /// compositor. Calling `ChangeEfficiencyArtifact::validate` directly is
    /// intentionally non-certifying; use [`Self::validate`] for this opaque
    /// runtime capture.
    pub fn artifact(&self) -> &ChangeEfficiencyArtifact {
        &self.artifact
    }

    /// Validate the real retained capture and upgrade only a contract-valid,
    /// scoped observation to the certification status.
    pub fn validate(&self) -> tze_hud_telemetry::ChangeEfficiencyValidation {
        let mut validation = self.artifact.validate();
        if validation.contract_satisfied
            && validation.status
                == tze_hud_telemetry::ChangeEfficiencyValidationStatus::PendingRuntimeInstrumentation
            && self.artifact.measurement_provenance
                == ChangeMeasurementProvenance::ObservedRetainedRuntime
        {
            validation.passed = true;
            validation.status =
                tze_hud_telemetry::ChangeEfficiencyValidationStatus::CertifiedRetainedRuntime;
        }
        validation
    }
}

#[derive(Clone, PartialEq)]
struct CanonicalSceneSnapshot {
    viewport: EfficiencyViewport,
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
    changed: CanonicalTextTile,
    redraw_tiles: Vec<CanonicalTextTile>,
    damage: PixelRect,
    scenario: CanonicalScenario,
    next_snapshot: CanonicalSceneSnapshot,
}

enum RetainedPlan {
    Partial(Box<CanonicalTextChange>),
    FullFrame {
        diagnostic: Option<FullSurfaceInvalidation>,
    },
}

/// Typed, compositor-private planner for the exact retained evidence envelope.
///
/// It has no WAL input: planning is derived solely from the prior
/// private snapshot and the live scene about to be rendered.
struct InvalidationPlanner;

impl InvalidationPlanner {
    fn plan_headless(
        state: &RetainedRenderState,
        scene: &SceneGraph,
        width: u32,
        height: u32,
    ) -> RetainedPlan {
        if state.device_recovery_pending {
            return RetainedPlan::FullFrame {
                diagnostic: Some(full_surface_invalidation(
                    FullSurfaceInvalidationReason::DeviceRecovery,
                    PartialPresentCapability::Supported,
                )),
            };
        }

        match (&state.snapshot, canonical_snapshot(scene, width, height)) {
            (None, Some(_)) => RetainedPlan::FullFrame {
                diagnostic: Some(full_surface_invalidation(
                    FullSurfaceInvalidationReason::SurfaceCreation,
                    PartialPresentCapability::Supported,
                )),
            },
            // This lane intentionally does not instrument arbitrary scenes
            // before it has established a canonical baseline. hud-f670c.1 owns
            // broader retained-planner coverage.
            (None, None) => RetainedPlan::FullFrame { diagnostic: None },
            (Some(_), None) => RetainedPlan::FullFrame {
                diagnostic: Some(full_surface_invalidation(
                    FullSurfaceInvalidationReason::UnsupportedRetainedSceneChange,
                    PartialPresentCapability::Supported,
                )),
            },
            (Some(previous_snapshot), Some(next_snapshot)) => {
                if previous_snapshot.viewport != next_snapshot.viewport {
                    return RetainedPlan::FullFrame {
                        diagnostic: Some(full_surface_invalidation(
                            FullSurfaceInvalidationReason::Resize,
                            PartialPresentCapability::Supported,
                        )),
                    };
                }
                if previous_snapshot == &next_snapshot {
                    return RetainedPlan::FullFrame { diagnostic: None };
                }
                let Some(change) = planned_canonical_text_change(previous_snapshot, &next_snapshot)
                else {
                    return RetainedPlan::FullFrame {
                        diagnostic: Some(full_surface_invalidation(
                            FullSurfaceInvalidationReason::UnsupportedRetainedSceneChange,
                            PartialPresentCapability::Supported,
                        )),
                    };
                };
                RetainedPlan::Partial(Box::new(CanonicalTextChange {
                    next_snapshot,
                    ..change
                }))
            }
        }
    }
}

impl RetainedRenderState {
    fn plan_headless(&self, scene: &SceneGraph, width: u32, height: u32) -> RetainedPlan {
        InvalidationPlanner::plan_headless(self, scene, width, height)
    }

    fn remember_full_headless_scene(&mut self, scene: &SceneGraph, width: u32, height: u32) {
        self.snapshot = canonical_snapshot(scene, width, height);
        self.device_recovery_pending = false;
    }

    fn forget_snapshot(&mut self) {
        self.snapshot = None;
    }

    fn complete_partial(&mut self, next_snapshot: CanonicalSceneSnapshot) {
        self.snapshot = Some(next_snapshot);
    }

    fn set_pending_full_surface_diagnostic(&mut self, diagnostic: Option<FullSurfaceInvalidation>) {
        self.pending_full_surface_diagnostic = diagnostic;
    }

    /// Start a new observed frame. Evidence slots are single-frame drains, not
    /// a history: a later fallback must never leave a prior passing capture
    /// available to be misattributed to the new frame.
    fn begin_observation_frame(&mut self) {
        self.clear_observed_evidence();
        // A previous planner decline that never reached the full-frame observer
        // must not leak its diagnostic into the next observed frame.
        self.pending_full_surface_diagnostic = None;
    }

    fn clear_observed_evidence(&mut self) {
        self.latest_capture = None;
        self.latest_diagnostic = None;
    }

    fn take_pending_full_surface_diagnostic(&mut self) -> Option<FullSurfaceInvalidation> {
        self.pending_full_surface_diagnostic.take()
    }

    fn set_latest_capture(&mut self, capture: RetainedChangeEfficiencyCapture) {
        self.latest_capture = Some(capture);
    }

    fn take_latest_capture(&mut self) -> Option<RetainedChangeEfficiencyCapture> {
        self.latest_capture.take()
    }

    fn set_latest_diagnostic(&mut self, artifact: ChangeEfficiencyArtifact) {
        self.latest_diagnostic = Some(artifact);
    }

    fn take_latest_diagnostic(&mut self) -> Option<ChangeEfficiencyArtifact> {
        self.latest_diagnostic.take()
    }

    /// Invalidate the retained proof baseline after a real production surface
    /// recovery. The following observed frame must be a full-surface
    /// diagnostic, never a scoped retained certification.
    pub(super) fn note_device_recovery(&mut self) {
        self.device_recovery_pending = true;
        self.snapshot = None;
    }
}

impl Compositor {
    /// Try the retained canonical headless path before falling back to the
    /// ordinary full-frame renderer.  `None` always means the caller must run
    /// the existing path; it never treats a proxy counter as evidence.
    pub(super) fn try_render_retained_headless(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
    ) -> Option<tze_hud_telemetry::FrameTelemetry> {
        self.retained_render_state.begin_observation_frame();
        if !self.retained_headless_policy_is_supported() {
            // A full-frame degradation policy can alter visibility or raster
            // semantics. Its submitted pixels are not a retained baseline, so
            // invalidate the private snapshot and surface a non-certifying
            // fallback rather than repainting against divergent content.
            self.retained_render_state.forget_snapshot();
            self.retained_render_state
                .set_pending_full_surface_diagnostic(Some(full_surface_invalidation(
                    FullSurfaceInvalidationReason::UnsupportedRetainedSceneChange,
                    PartialPresentCapability::Supported,
                )));
            return None;
        }
        let (width, height) = surface.size();
        let plan = self
            .retained_render_state
            .plan_headless(scene, width, height);
        let RetainedPlan::Partial(change) = plan else {
            let RetainedPlan::FullFrame { diagnostic } = plan else {
                unreachable!("retained plan has only partial or full-frame variants");
            };
            self.retained_render_state
                .set_pending_full_surface_diagnostic(diagnostic);
            return None;
        };
        let change = *change;

        let (telemetry, interval_duration_ms) =
            match self.render_retained_text_change(scene, surface, &change) {
                Some(result) => result,
                None => {
                    self.retained_render_state.forget_snapshot();
                    self.retained_render_state
                        .set_pending_full_surface_diagnostic(Some(full_surface_invalidation(
                            FullSurfaceInvalidationReason::UnsupportedRetainedSceneChange,
                            PartialPresentCapability::Supported,
                        )));
                    return None;
                }
            };

        let capture = RetainedChangeEfficiencyCapture::from_observed_runtime(
            self.observed_change_artifact(&change, width, height, interval_duration_ms),
        );
        self.retained_render_state
            .complete_partial(change.next_snapshot);
        self.retained_render_state.set_latest_capture(capture);
        Some(telemetry)
    }

    /// Record the result of an ordinary headless frame.  This seeds (or
    /// refreshes) the private retained snapshot only after the real full frame
    /// was submitted, and preserves structured diagnostics for resize and the
    /// modeled device-recovery planner path rather than certifying them.
    pub(super) fn observe_full_headless_frame(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
    ) {
        // `render_frame_headless` normally calls the retained planner first,
        // but keep this observer independently fail-closed for callers that
        // invoke a full frame directly.
        self.retained_render_state.clear_observed_evidence();
        let (width, height) = surface.size();
        let diagnostic = self
            .retained_render_state
            .take_pending_full_surface_diagnostic();
        if self.retained_headless_policy_is_supported() {
            self.retained_render_state
                .remember_full_headless_scene(scene, width, height);
        } else {
            self.retained_render_state.forget_snapshot();
        }
        if let Some(diagnostic) = diagnostic {
            self.retained_render_state
                .set_latest_diagnostic(full_surface_artifact(
                    self.renderer_identity(),
                    scene.visible_tiles().len() as u32,
                    width,
                    height,
                    diagnostic,
                ));
        }
    }

    /// Drain the most recent opaque capture from the canonical fifty-tile,
    /// headless evidence lane.
    ///
    /// This is a Rust-only Layer 3 validation hook.  It does not alter scene,
    /// gRPC, protobuf, reconnect-diff, or WAL contracts. `None` means this
    /// deliberately narrow producer did not observe its canonical scenario;
    /// it does not make a claim about arbitrary scene changes.
    pub fn take_change_efficiency_capture(&mut self) -> Option<RetainedChangeEfficiencyCapture> {
        self.retained_render_state.take_latest_capture()
    }

    /// Drain the most recent structured full-frame diagnostic from the
    /// canonical retained evidence lane. Diagnostics are deliberately raw
    /// artifacts and can never certify proportional rendering.
    pub fn take_change_efficiency_diagnostic(&mut self) -> Option<ChangeEfficiencyArtifact> {
        self.retained_render_state.take_latest_diagnostic()
    }

    fn render_retained_text_change(
        &mut self,
        scene: &SceneGraph,
        surface: &HeadlessSurface,
        change: &CanonicalTextChange,
    ) -> Option<(tze_hud_telemetry::FrameTelemetry, u64)> {
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
        let interval_duration_ms = frame_start.elapsed().as_millis().max(1) as u64;
        Some((telemetry, interval_duration_ms))
    }

    fn observed_change_artifact(
        &self,
        change: &CanonicalTextChange,
        width: u32,
        height: u32,
        interval_duration_ms: u64,
    ) -> ChangeEfficiencyArtifact {
        let members: Vec<_> = change
            .redraw_tiles
            .iter()
            .map(|tile| {
                let reason = if tile.tile_id == change.changed.tile_id {
                    InvalidationDependencyReason::DirectChange
                } else {
                    InvalidationDependencyReason::VisualOverlap
                };
                (tile, reason)
            })
            .collect();
        let node_members: Vec<_> = members
            .iter()
            .map(|(tile, reason)| {
                (
                    NodeWorkItemId {
                        tile_id: tile.tile_id.to_string(),
                        node_id: tile.node_id.to_string(),
                    },
                    reason.clone(),
                )
            })
            .collect();
        let render_members: Vec<_> = members
            .iter()
            .map(|(tile, reason)| {
                (
                    RenderPlanWorkItemId {
                        tile_id: tile.tile_id.to_string(),
                        plan_id: format!("{}:retained-text", tile.node_id),
                    },
                    reason.clone(),
                )
            })
            .collect();
        let direct_damage = tze_hud_telemetry::DamageWorkItemId {
            tile_id: change.changed.tile_id.to_string(),
            region_id: format!("{}:bounds", change.changed.tile_id),
            bounds: pixel_rect_for_bounds(change.changed.tile_bounds, width, height)
                .expect("canonical planner already validated changed tile damage"),
        };
        let damage_members = match &change.scenario {
            CanonicalScenario::OpaqueNonOverlapping => {
                vec![(direct_damage, InvalidationDependencyReason::DirectChange)]
            }
            CanonicalScenario::TransparentOverlap {
                upper_tile_id,
                overlap_bounds,
                ..
            } => {
                let upper = change
                    .redraw_tiles
                    .iter()
                    .find(|tile| tile.tile_id == *upper_tile_id)
                    .expect("transparent-overlap planner supplies its upper tile");
                vec![
                    (direct_damage, InvalidationDependencyReason::DirectChange),
                    (
                        tze_hud_telemetry::DamageWorkItemId {
                            tile_id: upper.tile_id.to_string(),
                            region_id: format!("{}:overlap", upper.tile_id),
                            bounds: pixel_rect_for_bounds(*overlap_bounds, width, height)
                                .expect("transparent-overlap planner supplies viewport bounds"),
                        },
                        InvalidationDependencyReason::VisualOverlap,
                    ),
                ]
            }
        };
        let (scenario_name, scenario_version) = match &change.scenario {
            CanonicalScenario::OpaqueNonOverlapping => (
                tze_hud_telemetry::ONE_NODE_FIFTY_TILE_SCENARIO_NAME,
                tze_hud_telemetry::ONE_NODE_FIFTY_TILE_SCENARIO_VERSION,
            ),
            CanonicalScenario::TransparentOverlap { .. } => (
                tze_hud_telemetry::TRANSPARENT_OVERLAP_FIFTY_TILE_SCENARIO_NAME,
                tze_hud_telemetry::TRANSPARENT_OVERLAP_FIFTY_TILE_SCENARIO_VERSION,
            ),
        };
        let scoped_encode_count = members.len() as u64;
        ChangeEfficiencyArtifact {
            schema_version: tze_hud_telemetry::CHANGE_EFFICIENCY_SCHEMA_VERSION,
            scenario: EfficiencyScenarioIdentity {
                name: scenario_name.into(),
                version: scenario_version,
            },
            runtime: EfficiencyRuntimeIdentity {
                build: format!("tze_hud_compositor-{}", env!("CARGO_PKG_VERSION")),
                window_mode: EfficiencyWindowMode::Headless,
            },
            pacing: EfficiencyPacingIdentity {
                mode: EfficiencyPacingMode::EventDriven,
                requested_cadence_hz: None,
            },
            renderer: self.renderer_identity(),
            viewport: EfficiencyViewport { width, height },
            constrained_profile: None,
            settling_duration_ms: 0,
            interval_duration_ms,
            status: ChangeMeasurementStatus::Complete,
            measurement_provenance: ChangeMeasurementProvenance::ObservedRetainedRuntime,
            scene_tile_count: CANONICAL_TILE_COUNT as u32,
            closure: InvalidationClosure {
                layout: observed_category(node_members.clone()),
                raster: observed_category(node_members),
                texture_upload: TextureUploadCategory {
                    closure_items: vec![],
                    actual_work: vec![],
                },
                render_encoding: observed_category(render_members),
                composition_damage: observed_category(damage_members),
            },
            render_observation: ChangeRenderWorkObservation {
                full_surface_clear_operations: 0,
                full_frame_encode_operations: 0,
                scoped_render_encode_operations: scoped_encode_count,
            },
            // Each closure tile contributes scoped background, prepared-text,
            // and deterministic idle-grip work. This logical count remains
            // distinct from render-plan membership and full-frame encodes.
            encoded_draw_calls: scoped_encode_count * 3,
            full_surface_invalidation: None,
        }
    }

    fn renderer_identity(&self) -> EfficiencyRendererIdentity {
        EfficiencyRendererIdentity {
            backend: nonempty_identity(&self.adapter_info.backend, "unknown-backend"),
            adapter: nonempty_identity(&self.adapter_info.name, "unknown-adapter"),
            software: self.adapter_info.device_type.eq_ignore_ascii_case("cpu"),
        }
    }

    fn retained_headless_policy_is_supported(&self) -> bool {
        self.degradation_policy.level == tze_hud_scene::DegradationLevel::Nominal
            // The retained proof can reproduce only the deterministic idle
            // drag-grip layer below. All other compositor-owned chrome remains
            // a full-frame concern until it has its own bounded evidence lane.
            && self.focus_ring_owner.is_none()
            && self.resize_grip_hover.is_none()
            && self.local_composer.is_none()
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
        if !tile.opacity.is_finite()
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
        viewport: EfficiencyViewport { width, height },
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
            let damage = pixel_rect_for_bounds(
                changed.tile_bounds,
                current.viewport.width,
                current.viewport.height,
            )?;
            Some(CanonicalTextChange {
                changed: changed.clone(),
                redraw_tiles: vec![changed],
                damage,
                scenario: current.scenario.clone(),
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
                current.viewport.width,
                current.viewport.height,
            )?;
            Some(CanonicalTextChange {
                changed,
                redraw_tiles,
                damage,
                scenario: current.scenario.clone(),
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

fn one_item_category<T>(identity: T) -> InvalidationCategory<T>
where
    T: Clone,
{
    InvalidationCategory {
        closure_items: vec![ClosureWorkItem {
            identity: identity.clone(),
            dependency_reason: InvalidationDependencyReason::DirectChange,
        }],
        actual_work: vec![ActualWorkItem {
            identity,
            operations: 1,
        }],
    }
}

fn observed_category<T>(members: Vec<(T, InvalidationDependencyReason)>) -> InvalidationCategory<T>
where
    T: Clone,
{
    InvalidationCategory {
        closure_items: members
            .iter()
            .map(|(identity, dependency_reason)| ClosureWorkItem {
                identity: identity.clone(),
                dependency_reason: dependency_reason.clone(),
            })
            .collect(),
        actual_work: members
            .into_iter()
            .map(|(identity, _)| ActualWorkItem {
                identity,
                operations: 1,
            })
            .collect(),
    }
}

fn full_surface_invalidation(
    reason: FullSurfaceInvalidationReason,
    partial_present_capability: PartialPresentCapability,
) -> FullSurfaceInvalidation {
    FullSurfaceInvalidation {
        reason,
        partial_present_capability,
    }
}

fn full_surface_artifact(
    renderer: EfficiencyRendererIdentity,
    scene_tile_count: u32,
    width: u32,
    height: u32,
    full_surface_invalidation: FullSurfaceInvalidation,
) -> ChangeEfficiencyArtifact {
    let surface_damage = tze_hud_telemetry::DamageWorkItemId {
        tile_id: "runtime-surface".into(),
        region_id: "full-surface".into(),
        bounds: PixelRect {
            x: 0,
            y: 0,
            width,
            height,
        },
    };
    ChangeEfficiencyArtifact {
        schema_version: tze_hud_telemetry::CHANGE_EFFICIENCY_SCHEMA_VERSION,
        scenario: EfficiencyScenarioIdentity {
            name: tze_hud_telemetry::ONE_NODE_FIFTY_TILE_SCENARIO_NAME.into(),
            version: tze_hud_telemetry::ONE_NODE_FIFTY_TILE_SCENARIO_VERSION,
        },
        runtime: EfficiencyRuntimeIdentity {
            build: format!("tze_hud_compositor-{}", env!("CARGO_PKG_VERSION")),
            window_mode: EfficiencyWindowMode::Headless,
        },
        pacing: EfficiencyPacingIdentity {
            mode: EfficiencyPacingMode::EventDriven,
            requested_cadence_hz: None,
        },
        renderer,
        viewport: EfficiencyViewport { width, height },
        constrained_profile: None,
        settling_duration_ms: 0,
        interval_duration_ms: 1,
        status: ChangeMeasurementStatus::Complete,
        measurement_provenance: ChangeMeasurementProvenance::ObservedFullFrameRuntime,
        scene_tile_count,
        closure: InvalidationClosure {
            layout: InvalidationCategory {
                closure_items: vec![],
                actual_work: vec![],
            },
            raster: InvalidationCategory {
                closure_items: vec![],
                actual_work: vec![],
            },
            texture_upload: TextureUploadCategory {
                closure_items: vec![],
                actual_work: vec![],
            },
            render_encoding: InvalidationCategory {
                closure_items: vec![],
                actual_work: vec![],
            },
            composition_damage: one_item_category(surface_damage),
        },
        render_observation: ChangeRenderWorkObservation {
            full_surface_clear_operations: 1,
            full_frame_encode_operations: 1,
            scoped_render_encode_operations: 0,
        },
        encoded_draw_calls: 0,
        full_surface_invalidation: Some(full_surface_invalidation),
    }
}

fn nonempty_identity(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.into()
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::types::{FontFamily, Node, Rgba, TextAlign, TextOverflow};

    fn renderer() -> EfficiencyRendererIdentity {
        EfficiencyRendererIdentity {
            backend: "test-backend".into(),
            adapter: "test-adapter".into(),
            software: true,
        }
    }

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
    }

    #[test]
    fn full_surface_reasons_are_structured_non_passing_diagnostics() {
        for (reason, capability) in [
            (
                FullSurfaceInvalidationReason::Resize,
                PartialPresentCapability::Supported,
            ),
            (
                FullSurfaceInvalidationReason::DeviceRecovery,
                PartialPresentCapability::Supported,
            ),
            (
                FullSurfaceInvalidationReason::UnsupportedPartialPresentBackend,
                PartialPresentCapability::Unsupported,
            ),
            (
                FullSurfaceInvalidationReason::UnsupportedRetainedSceneChange,
                PartialPresentCapability::Supported,
            ),
        ] {
            let artifact = full_surface_artifact(
                renderer(),
                CANONICAL_TILE_COUNT as u32,
                1_000,
                500,
                full_surface_invalidation(reason.clone(), capability),
            );
            let report = artifact.validate();
            assert!(!report.passed, "{report:#?}");
            assert_eq!(
                report.status,
                tze_hud_telemetry::ChangeEfficiencyValidationStatus::DiagnosticFullSurface,
                "{report:#?}"
            );
            assert_eq!(artifact.full_surface_invalidation.unwrap().reason, reason);
        }
    }

    #[test]
    fn new_observation_clears_prior_frame_capture_and_diagnostic() {
        let mut state = RetainedRenderState::default();
        let mut prior_capture_artifact = full_surface_artifact(
            renderer(),
            CANONICAL_TILE_COUNT as u32,
            1_000,
            500,
            full_surface_invalidation(
                FullSurfaceInvalidationReason::SurfaceCreation,
                PartialPresentCapability::Supported,
            ),
        );
        prior_capture_artifact.measurement_provenance =
            ChangeMeasurementProvenance::ObservedRetainedRuntime;
        state.set_latest_capture(RetainedChangeEfficiencyCapture::from_observed_runtime(
            prior_capture_artifact,
        ));
        state.set_latest_diagnostic(full_surface_artifact(
            renderer(),
            CANONICAL_TILE_COUNT as u32,
            1_000,
            500,
            full_surface_invalidation(
                FullSurfaceInvalidationReason::Resize,
                PartialPresentCapability::Supported,
            ),
        ));
        state.set_pending_full_surface_diagnostic(Some(full_surface_invalidation(
            FullSurfaceInvalidationReason::DeviceRecovery,
            PartialPresentCapability::Supported,
        )));

        state.begin_observation_frame();

        assert!(state.take_latest_capture().is_none());
        assert!(state.take_latest_diagnostic().is_none());
        assert!(state.take_pending_full_surface_diagnostic().is_none());
    }

    #[test]
    fn device_recovery_invalidates_the_private_snapshot() {
        let mut state = RetainedRenderState {
            snapshot: Some(CanonicalSceneSnapshot {
                viewport: EfficiencyViewport {
                    width: 1_000,
                    height: 500,
                },
                scenario: CanonicalScenario::OpaqueNonOverlapping,
                tiles: vec![],
            }),
            ..RetainedRenderState::default()
        };

        state.note_device_recovery();

        assert!(state.snapshot.is_none());
        assert!(state.device_recovery_pending);
    }
}
