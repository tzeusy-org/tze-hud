//! Widget renderer methods for the compositor.

use tze_hud_scene::WidgetParameterValue;
use tze_hud_scene::dock::DockWidgetSlot;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{FontFamily, RenderingPolicy, TextOverflow};
use tze_hud_scene::widget_deadline::widget_params_at;
use tze_hud_scene::{DegradationLevel, Rect};

use crate::display::{DisplayRect, FrameTarget};
use crate::text::TextItem;
use crate::widget::{
    WidgetDrawGeometry, WidgetDrawQuad, WidgetRenderer, same_dock_presentation, snap_composite_rect,
};

use super::token_colors::{canonical_color, canonical_number, resolve_token_color};

#[derive(Default)]
pub(super) struct WidgetDraws {
    primary: Vec<WidgetDrawQuad>,
    dock: Option<DockDraws>,
}

struct DockDraws {
    display: DisplayRect,
    quads: Vec<WidgetDrawQuad>,
    captions: Vec<TextItem>,
}

fn target_rect(target: &FrameTarget) -> Rect {
    Rect::new(
        target.x,
        target.y,
        target.width as f32,
        target.height as f32,
    )
}

impl WidgetDraws {
    pub(super) fn for_target(
        &self,
        target: &FrameTarget,
        display: Option<&str>,
    ) -> ([&[WidgetDrawQuad]; 2], &[TextItem]) {
        let primary = if target.primary {
            self.primary.as_slice()
        } else {
            &[]
        };
        let dock = self.dock.as_ref().filter(|dock| {
            dock.display.rect == target_rect(target)
                && dock.display.primary == target.primary
                && match display {
                    Some(name) => name == dock.display.name,
                    None => target.primary,
                }
        });
        match dock {
            Some(dock) => ([primary, &dock.quads], &dock.captions),
            None => ([primary, &[]], &[]),
        }
    }

    pub(super) fn publish_geometry(
        &self,
        wr: Option<&WidgetRenderer>,
        scene: &mut SceneGraph,
        windowed_primary: Option<FrameTarget>,
    ) {
        if let Some(dock) = &self.dock
            && windowed_primary.is_some()
        {
            scene.certify_dock_passive_column(&dock.display.name, dock.display.rect);
        }
        let geometry = &mut scene.overlay.widget_geometry;
        geometry.clear();
        let Some(wr) = wr else { return };
        let mut publish = |quads: &[WidgetDrawQuad], display: Rect| {
            geometry.extend(quads.iter().filter_map(|quad| {
                let (_, mut bounds) = wr.drawable_widget(quad, display.width, display.height)?;
                bounds.x += display.x;
                bounds.y += display.y;
                Some(tze_hud_scene::graph::overlay::WidgetGeometry {
                    instance_name: quad.instance_name.clone(),
                    bounds,
                })
            }));
        };
        if let Some(primary) = windowed_primary {
            publish(&self.primary, target_rect(&primary));
        }
        // Direct headless only submits its primary surface.
        if let Some(dock) = &self.dock
            && (windowed_primary.is_some() || dock.display.primary)
        {
            publish(&dock.quads, dock.display.rect);
        }
    }
}

impl super::Compositor {
    /// Settle dock geometry before the ordinary frame gate, using the existing
    /// exit motion. Headless has no pointer and can always settle.
    pub fn advance_dock_compaction(&self, scene: &mut SceneGraph, windowed: bool) -> bool {
        use super::easing::Easing;
        let ease: fn(f32) -> f32 = match self.zone_motion.exit {
            Easing::Linear => |t| Easing::Linear.apply(t),
            Easing::EaseInOut => |t| Easing::EaseInOut.apply(t),
            Easing::EaseOutQuad => |t| Easing::EaseOutQuad.apply(t),
            Easing::EaseInQuad => |t| Easing::EaseInQuad.apply(t),
        };
        let duration = if tze_hud_config::tokens::os_reduce_motion(&self.token_map) {
            0
        } else {
            u64::from(self.zone_motion.exit_ms) * 1_000
        };
        scene.advance_dock_compaction(windowed, duration, ease)
    }

    /// Countdowns are a one-shot display deadline, never a frame-rate animation.
    pub fn next_widget_deadline_wall_us(&self, scene: &SceneGraph) -> Option<u64> {
        if self.safe_mode_overlay {
            None
        } else {
            let dock_missing = self.named_dock_target_missing(scene);
            scene.next_widget_deadline_wall_us_where(|instance| {
                !dock_missing || !scene.is_dock_widget(&instance.instance_name)
            })
        }
    }

    /// The idle gate runs before texture sync. Compare the text that would be
    /// drawn now against the last submitted widget parameters before parking.
    pub fn widget_deadline_needs_render(&self, scene: &SceneGraph) -> bool {
        if self.safe_mode_overlay || scene.widget_time_paused() || !scene.widgets_shown() {
            return false;
        }
        let Some(renderer) = self.widget_renderer.as_ref() else {
            return false;
        };
        let dock_missing = self.named_dock_target_missing(scene);
        let now = scene.now_wall_us();
        scene.widget_registry.instances.values().any(|instance| {
            (!dock_missing || !scene.is_dock_widget(&instance.instance_name))
                && scene.widget_visible(instance)
                && instance.current_params.iter().any(|(name, value)| {
                    let WidgetParameterValue::Deadline(deadline) = value else {
                        return false;
                    };
                    let displayed = WidgetParameterValue::String(deadline.display_at(now));
                    renderer
                        .texture_entry(&instance.instance_name)
                        .and_then(|e| e.last_rendered_params.get(name))
                        != Some(&displayed)
                })
        })
    }

    /// Initialize (or re-initialize) the widget renderer for the given surface format.
    ///
    /// Must be called once before widget textures can be composited. For headless
    /// compositors, `format` should be `Rgba8UnormSrgb`. For windowed compositors,
    /// use the negotiated swapchain format.
    ///
    /// Calling this multiple times replaces the existing renderer (e.g. on surface
    /// reconfiguration or format change). Any cached textures are discarded.
    pub fn init_widget_renderer(&mut self, format: wgpu::TextureFormat) {
        let mut renderer = WidgetRenderer::new(&self.device, format);
        if let Some(ledger) = &self.resident_ledger {
            renderer.set_resident_ledger(ledger.clone());
        }
        if let Some(fit) = &self.text_fit {
            renderer.set_text_fit(std::sync::Arc::clone(fit));
        }
        self.widget_renderer = Some(renderer);
        tracing::debug!(format = ?format, "widget renderer initialized");
    }

    pub fn set_resident_ledger(&mut self, ledger: tze_hud_resource::ResidentLedger) {
        if let Some(renderer) = &mut self.widget_renderer {
            renderer.set_resident_ledger(ledger.clone());
        }
        self.resident_ledger = Some(ledger);
    }

    /// Get a mutable reference to the widget renderer, if initialized.
    pub fn widget_renderer_mut(&mut self) -> Option<&mut WidgetRenderer> {
        self.widget_renderer.as_mut()
    }

    /// Get a reference to the widget renderer, if initialized.
    pub fn widget_renderer(&self) -> Option<&WidgetRenderer> {
        self.widget_renderer.as_ref()
    }

    pub(super) fn dock_draw_display(
        &self,
        scene: &SceneGraph,
        primary: &FrameTarget,
    ) -> Option<&DisplayRect> {
        let projection = scene.dock_projection()?;
        let target = projection.target.as_deref()?;
        self.display_layout.displays().iter().find(|display| {
            display.name == target
                && display.rect == projection.layout.display
                && (!display.primary || display.rect == target_rect(primary))
        })
    }

    fn named_dock_target_missing(&self, scene: &SceneGraph) -> bool {
        scene.dock_projection().is_some_and(|p| p.target.is_some())
            && self
                .dock_draw_display(scene, &FrameTarget::primary(self.width, self.height))
                .is_none()
    }

    pub(super) fn dock_caption_items(
        &self,
        scene: &SceneGraph,
        display: &DisplayRect,
    ) -> Vec<TextItem> {
        self.dock_caption_items_at(scene, display, |index| scene.dock_caption_rect(index))
    }

    pub(super) fn dock_caption_items_for_cache(
        &self,
        scene: &SceneGraph,
        display: &DisplayRect,
    ) -> Vec<TextItem> {
        let Some(projection) = scene.dock_projection() else {
            return Vec::new();
        };
        let layout = &projection.layout;
        // A live row can enter the aperture at terminal compaction without
        // changing Scene.version. Prime its width/height key before that turn.
        self.dock_caption_items_at(scene, display, |index| {
            (index < layout.capacity).then(|| layout.caption(index))
        })
    }

    fn dock_caption_items_at(
        &self,
        scene: &SceneGraph,
        display: &DisplayRect,
        bounds: impl Fn(usize) -> Option<Rect>,
    ) -> Vec<TextItem> {
        let Some(projection) = scene.dock_projection() else {
            return Vec::new();
        };
        if !scene.widgets_shown()
            || scene.dock_rows().is_empty()
            || self.widget_renderer.is_none()
            || self.text_rasterizer.is_none()
        {
            return Vec::new();
        }
        let tokens = &self.token_map;
        let number = |key| {
            tokens
                .get(key)
                .and_then(|value| tze_hud_config::tokens::parse_numeric(value))
                .filter(|value| *value > 0.0)
                .unwrap_or_else(|| canonical_number(key))
        };
        let scale = projection.pixels_per_dp * projection.layout.scale;
        let size = number("type.label.m.size") * scale;
        let line = number("type.label.m.line_height") * scale;
        if !size.is_finite() || !line.is_finite() || size <= 0.0 || line < 1.0 {
            return Vec::new();
        }
        let font = match tokens.get("type.label.m.family").map(String::as_str) {
            Some("monospace") => FontFamily::SystemMonospace,
            Some("serif") => FontFamily::SystemSerif,
            _ => FontFamily::SystemSansSerif,
        };
        let color = "color.on_surface.variant";
        let policy = RenderingPolicy {
            font_family: Some(font),
            font_size_px: Some(size),
            font_weight: Some(number("type.label.m.weight") as u16),
            text_color: resolve_token_color(tokens, color).or_else(|| canonical_color(color)),
            overflow: Some(TextOverflow::Ellipsis),
            margin_px: Some(0.0),
            ..Default::default()
        };
        scene
            .dock_rows()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let caption = scene.dock_caption(&row.namespace)?;
                let bounds = bounds(index)?;
                if bounds.width < 1.0 {
                    return None;
                }
                let x = bounds.x - display.rect.x;
                let top = bounds.y - display.rect.y;
                let y = top + (bounds.height - line) * 0.5;
                let mut item = TextItem::from_zone_policy(
                    caption,
                    x,
                    y,
                    bounds.width,
                    line,
                    &policy,
                    1.0,
                    1.0,
                );
                item.line_height_multiplier = line / item.font_size_px;
                item.clip_pixel_y = top.max(y);
                item.clip_bounds_height = (top + bounds.height - item.clip_pixel_y).min(line);
                Some(item)
            })
            .collect()
    }

    /// Update visible textures and prepare scene-free draw geometry in the
    /// same pass, using the accepted dock slots for both work and placement.
    pub(super) fn prepare_widget_draws(
        &mut self,
        scene: &SceneGraph,
        degradation_level: DegradationLevel,
        width: u32,
        height: u32,
    ) -> WidgetDraws {
        let primary_target = FrameTarget::primary(width, height);
        let dock = self
            .dock_draw_display(scene, &primary_target)
            .map(|display| DockDraws {
                display: display.clone(),
                quads: Vec::new(),
                captions: self.dock_caption_items(scene, display),
            });
        let countdowns_paused = self.safe_mode_overlay || scene.widget_time_paused();
        let wr = match &mut self.widget_renderer {
            Some(r) => r,
            None => return WidgetDraws::default(),
        };
        wr.begin_sync();
        let registry = &scene.widget_registry;
        wr.retire_absent_dock_instances(registry);
        // Hidden by the owner: no raster or upload. Cached textures stay for
        // when they are shown again; transitions stop rather than tick frames
        // nobody sees, and the first shown sync snaps to the newest values.
        if !scene.widgets_shown() {
            wr.hide();
            return WidgetDraws::default();
        }
        let snap = wr.take_snap_on_show();

        let now_us = scene.now_monotonic_us();

        // Retire inactive and out-of-capacity textures before admitting
        // replacement rasters. Overflow keeps the scene publication alive.
        let mut visible = Vec::with_capacity(registry.instances.len());
        for (name, instance) in &registry.instances {
            if registry
                .active_publishes
                .get(name)
                .is_none_or(Vec::is_empty)
            {
                wr.remove_texture(name);
                continue;
            }
            let dock_bounds = match (scene.dock_widget_slot(name), dock.as_ref()) {
                (DockWidgetSlot::Registry, _) => None,
                (DockWidgetSlot::Placed(bounds), Some(dock)) => {
                    let display = &dock.display;
                    let (x, y, w, h) = snap_composite_rect(
                        bounds.x - display.rect.x,
                        bounds.y - display.rect.y,
                        bounds.width,
                        bounds.height,
                        display.rect.width,
                        display.rect.height,
                    );
                    Some(Rect::new(x, y, w, h))
                }
                _ => {
                    wr.remove_texture(name);
                    continue;
                }
            };
            visible.push((instance, dock_bounds));
        }

        let mut draws = WidgetDraws {
            dock,
            ..Default::default()
        };
        let mut registry_names = Vec::new();
        for (instance, dock_bounds) in visible {
            let instance_name = &instance.instance_name;
            if let Some(bounds) = dock_bounds {
                draws
                    .dock
                    .as_mut()
                    .expect("placed dock display")
                    .quads
                    .push(WidgetDrawQuad {
                        instance_name: instance_name.clone(),
                        geometry: WidgetDrawGeometry::Dock(bounds),
                    });
            } else {
                registry_names.push(instance_name.as_str());
            }
            let Some(def) = registry.definitions.get(&instance.widget_type_name) else {
                continue;
            };
            let dock = scene.widget_dock_presentation(instance);
            if wr.texture_entry(instance_name).is_some_and(|entry| {
                !same_dock_presentation(entry.dock_presentation.as_ref(), dock)
            }) {
                // A denied source or texture admission must not draw the old
                // presentation in place of the selected one.
                wr.remove_texture(instance_name);
            }

            // Dock slots are already scaled and snapped to physical pixels.
            let (pw, ph) = if let Some(bounds) = dock_bounds {
                (bounds.width as u32, bounds.height as u32)
            } else {
                let Some(geometry) = registry.drawn_geometry(instance) else {
                    continue;
                };
                super::resolve_widget_pixel_size(&geometry, self.width, self.height)
            };
            if pw == 0 || ph == 0 {
                continue;
            }

            // Check if this instance needs an initial texture (no entry yet).
            let needs_initial = wr.texture_entry(instance_name).is_none();

            // Resolve animated or static params, applying degradation-aware snapping.
            let has_deadline = instance
                .current_params
                .values()
                .any(|v| matches!(v, WidgetParameterValue::Deadline(_)));
            if has_deadline && dock_bounds.is_none() && !scene.widget_visible(instance) {
                wr.remove_texture(instance_name);
                continue;
            }
            if has_deadline && countdowns_paused {
                // Leave the last pixels in place. Resume is an existing render
                // event; no timer or repeated overdue repaint is needed.
                if let Some(entry) = wr.texture_entry_mut(instance_name) {
                    entry.animation = None;
                }
                continue;
            }
            let projected = has_deadline
                .then(|| widget_params_at(&instance.current_params, scene.now_wall_us()));
            let current_params = projected.as_ref().unwrap_or(&instance.current_params);

            // A published change on an already-visible widget starts a
            // transition of the newest publication's `transition_ms`, from
            // what is on screen now (so a retarget mid-flight is continuous).
            // First appearance snaps. An instant publication cancels any older
            // transition before resolving params, even if its target is unchanged.
            let transition_ms = registry
                .active_publishes
                .get(instance_name)
                .and_then(|p| p.iter().max_by_key(|r| r.published_at_wall_us))
                .map_or(0, |r| r.transition_ms);
            if transition_ms == 0 || snap {
                if let Some(entry) = wr.texture_entry_mut(instance_name) {
                    entry.animation = None;
                }
            } else if let Some(entry) = wr.texture_entry(instance_name) {
                let target = entry
                    .animation
                    .as_ref()
                    .map_or(&entry.last_rendered_params, |a| &a.to_params);
                let interpolated_change = current_params.iter().any(|(name, value)| {
                    matches!(
                        value,
                        WidgetParameterValue::F32(_) | WidgetParameterValue::Color(_)
                    ) && target.get(name).is_some_and(|old| old != value)
                });
                if interpolated_change {
                    let from = entry.last_rendered_params.clone();
                    wr.start_transition(
                        instance_name,
                        from,
                        current_params.clone(),
                        transition_ms,
                        now_us,
                    );
                } else if let Some(animation) = wr
                    .texture_entry_mut(instance_name)
                    .and_then(|e| e.animation.as_mut())
                {
                    // Text and enums snap even during a running numeric
                    // transition; changing countdown text never restarts it.
                    animation.to_params.clone_from(current_params);
                }
            }
            let (effective_params, still_animating) = wr.resolve_animated_params(
                instance_name,
                current_params,
                degradation_level,
                now_us,
            );

            let params_changed = wr
                .texture_entry(instance_name)
                .map(|e| e.last_rendered_params != effective_params)
                .unwrap_or(false);

            // The owner moved or resized it (control panel layout).
            let resized = wr
                .texture_entry(instance_name)
                .is_some_and(|e| (e.width, e.height) != (pw, ph));

            let dirty = needs_initial
                || still_animating
                || params_changed
                || resized
                || wr.restyled(
                    &def.id,
                    dock.map_or(def.layers.as_slice(), |dock| &dock.layers),
                )
                || wr
                    .texture_entry(instance_name)
                    .map(|e| e.dirty)
                    .unwrap_or(false);

            if dirty {
                let uploaded = wr.rasterize_and_upload(
                    &self.device,
                    &self.queue,
                    instance_name,
                    def,
                    dock_bounds.is_some() || dock.is_some() || scene.is_dock_widget(instance_name),
                    dock,
                    &effective_params,
                    pw,
                    ph,
                );
                // Record what params were rendered so we can detect future changes.
                if let Some(entry) = wr.texture_entry_mut(instance_name) {
                    if uploaded {
                        entry.dock_presentation = dock.cloned();
                    }
                    entry.last_rendered_params = effective_params;
                    entry.dirty = false;
                }
            }
        }
        wr.end_sync();
        draws.primary =
            wr.collect_registry_widget_quads(registry, width as f32, height as f32, registry_names);
        if let Some(dock) = &mut draws.dock {
            dock.quads.retain(|quad| {
                wr.drawable_widget(quad, dock.display.rect.width, dock.display.rect.height)
                    .is_some()
            });
            dock.quads
                .sort_unstable_by(|a, b| a.instance_name.cmp(&b.instance_name));
        }
        draws
    }
}
