//! Widget renderer methods for the compositor.

use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;

use crate::widget::WidgetRenderer;

impl super::Compositor {
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
    pub fn sync_widget_textures(
        &mut self,
        scene: &SceneGraph,
        degradation_level: DegradationLevel,
    ) {
        let wr = match &mut self.widget_renderer {
            Some(r) => r,
            None => return,
        };

        let registry = &scene.widget_registry;

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

            // Determine pixel geometry from the instance's geometry policy.
            // Fall back to a sensible default if not set.
            let (pw, ph) =
                super::resolve_widget_pixel_size(&instance, &def, self.width, self.height);
            if pw == 0 || ph == 0 {
                continue;
            }

            // Check if this instance needs an initial texture (no entry yet).
            let needs_initial = wr.texture_entry(&instance_name).is_none();

            // Resolve animated or static params, applying degradation-aware snapping.
            let current_params = &instance.current_params;
            let (effective_params, still_animating) =
                wr.resolve_animated_params(&instance_name, current_params, degradation_level);

            let params_changed = wr
                .texture_entry(&instance_name)
                .map(|e| e.last_rendered_params != effective_params)
                .unwrap_or(false);

            let dirty = needs_initial
                || still_animating
                || params_changed
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
    }
}
