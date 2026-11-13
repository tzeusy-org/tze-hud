//! Widget renderer methods for the compositor.

use tze_hud_scene::DegradationLevel;
use tze_hud_scene::WidgetParameterValue;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::widget_deadline::widget_params_at;

use crate::widget::WidgetRenderer;

impl super::Compositor {
    /// Countdowns are a one-shot display deadline, never a frame-rate animation.
    pub fn next_widget_deadline_wall_us(&self, scene: &SceneGraph) -> Option<u64> {
        if self.safe_mode_overlay {
            None
        } else {
            scene.next_widget_deadline_wall_us()
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
        let now = scene.now_wall_us();
        scene.widget_registry.instances.values().any(|instance| {
            scene.widget_visible(instance)
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

    /// Ensure widget instances have up-to-date cached textures for all widget
    /// instances in the registry.
    ///
    /// For each widget instance:
    /// - If no texture entry exists (first frame), rasterizes with default params.
    /// - If the instance has a `dirty` flag set, re-rasterizes with current params.
    /// - If an animation is active, resolves interpolated params and re-rasterizes.
    ///
    /// Under degradation level [`DegradationLevel::Simplified`] or higher, active
    /// transitions are snapped to their final values immediately, reducing
    /// re-rasterization to at most once per parameter change during transitions.
    ///
    /// This should be called once per frame before `render_frame`.
    pub(crate) fn sync_widget_textures(
        &mut self,
        scene: &SceneGraph,
        degradation_level: DegradationLevel,
    ) {
        let countdowns_paused = self.safe_mode_overlay || scene.widget_time_paused();
        let wr = match &mut self.widget_renderer {
            Some(r) => r,
            None => return,
        };
        wr.begin_sync();
        // Hidden by the owner: no raster or upload. Cached textures stay for
        // when they are shown again; transitions stop rather than tick frames
        // nobody sees, and the first shown sync snaps to the newest values.
        if !scene.widgets_shown() {
            wr.hide();
            return;
        }
        let snap = wr.take_snap_on_show();

        let registry = &scene.widget_registry;
        let now_us = scene.now_monotonic_us();

        // Collect instances that need texture updates. Widgets without active
        // publications are not visible; clear their cached texture so clear/TTL
        // removal takes effect on the next frame instead of rendering defaults.
        let instance_names: Vec<String> = registry.instances.keys().cloned().collect();

        // Reclaim every safely inactive texture before admitting any new or
        // replacement raster for this frame. Active publications form the
        // current-frame guard set and are never evicted by this pass.
        for instance_name in &instance_names {
            let has_active_publication = registry
                .active_publishes
                .get(instance_name)
                .is_some_and(|publishes| !publishes.is_empty());
            if !has_active_publication {
                wr.remove_texture(instance_name);
            }
        }

        for instance_name in instance_names {
            let has_active_publication = registry
                .active_publishes
                .get(&instance_name)
                .is_some_and(|publishes| !publishes.is_empty());
            if !has_active_publication {
                continue;
            }

            let instance = match registry.instances.get(&instance_name) {
                Some(i) => i.clone(),
                None => continue,
            };
            let def = match registry.definitions.get(&instance.widget_type_name) {
                Some(d) => d.clone(),
                None => continue,
            };

            // Raster at the size the widget is drawn: its box at the zoom.
            let Some(geometry) = registry.drawn_geometry(&instance) else {
                continue;
            };
            let (pw, ph) = super::resolve_widget_pixel_size(&geometry, self.width, self.height);
            if pw == 0 || ph == 0 {
                continue;
            }

            // Check if this instance needs an initial texture (no entry yet).
            let needs_initial = wr.texture_entry(&instance_name).is_none();

            // Resolve animated or static params, applying degradation-aware snapping.
            let has_deadline = instance
                .current_params
                .values()
                .any(|v| matches!(v, WidgetParameterValue::Deadline(_)));
            if has_deadline && !scene.widget_visible(&instance) {
                wr.remove_texture(&instance_name);
                continue;
            }
            if has_deadline && countdowns_paused {
                // Leave the last pixels in place. Resume is an existing render
                // event; no timer or repeated overdue repaint is needed.
                if let Some(entry) = wr.texture_entry_mut(&instance_name) {
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
                .get(&instance_name)
                .and_then(|p| p.iter().max_by_key(|r| r.published_at_wall_us))
                .map_or(0, |r| r.transition_ms);
            if transition_ms == 0 || snap {
                if let Some(entry) = wr.texture_entry_mut(&instance_name) {
                    entry.animation = None;
                }
            } else if let Some(entry) = wr.texture_entry(&instance_name) {
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
                        &instance_name,
                        from,
                        current_params.clone(),
                        transition_ms,
                        now_us,
                    );
                } else if let Some(animation) = wr
                    .texture_entry_mut(&instance_name)
                    .and_then(|e| e.animation.as_mut())
                {
                    // Text and enums snap even during a running numeric
                    // transition; changing countdown text never restarts it.
                    animation.to_params.clone_from(current_params);
                }
            }
            let (effective_params, still_animating) = wr.resolve_animated_params(
                &instance_name,
                current_params,
                degradation_level,
                now_us,
            );

            let params_changed = wr
                .texture_entry(&instance_name)
                .map(|e| e.last_rendered_params != effective_params)
                .unwrap_or(false);

            // The owner moved or resized it (control panel layout).
            let resized = wr
                .texture_entry(&instance_name)
                .is_some_and(|e| (e.width, e.height) != (pw, ph));

            let dirty = needs_initial
                || still_animating
                || params_changed
                || resized
                || wr.restyled(&instance.widget_type_name)
                || wr
                    .texture_entry(&instance_name)
                    .map(|e| e.dirty)
                    .unwrap_or(false);

            if dirty {
                wr.rasterize_and_upload(
                    &self.device,
                    &self.queue,
                    &instance_name,
                    &def,
                    &effective_params,
                    pw,
                    ph,
                );
                // Record what params were rendered so we can detect future changes.
                if let Some(entry) = wr.texture_entry_mut(&instance_name) {
                    entry.last_rendered_params = effective_params;
                    entry.dirty = false;
                }
            }
        }
        wr.end_sync();
    }
}
