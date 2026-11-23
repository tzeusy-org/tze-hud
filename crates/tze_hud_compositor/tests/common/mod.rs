//! Shared helpers for the compositor GPU integration tests.
//!
//! The external test harness includes this module once for all GPU suites.
mod gpu;
mod pixels;

pub(crate) use gpu::{gpu_required, new_gpu};
pub(crate) use pixels::pixel_at;

use std::collections::HashMap;
use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{
    GeometryPolicy, NotificationPayload, SceneId, WidgetInstance, WidgetParameterValue, ZoneContent,
};
use tze_hud_telemetry::FrameTelemetry;
use tze_hud_widget::loader::BundleScanResult;

macro_rules! bundle {
    ($name:literal, [$($file:literal),*]) => {
        tze_hud_widget::loader::load_bundle_from_files(
            $name,
            &[
                ("widget.toml", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/widget_bundles/", $name, "/widget.toml")) as &[u8]),
                $(($file, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/widget_bundles/", $name, "/", $file)) as &[u8])),*
            ],
            &$crate::common::tokens(),
        )
    };
}

pub(crate) fn tokens() -> HashMap<String, String> {
    tze_hud_config::tokens::resolve_tokens(&Default::default(), &Default::default())
}

/// A headless compositor plus the surface it renders into.
pub(crate) struct Gpu {
    pub compositor: Compositor,
    surface: HeadlessSurface,
    width: u32,
}

/// The sRGB bytes of one rendered frame.
pub(crate) struct Frame {
    pixels: Vec<u8>,
    width: u32,
}

impl Gpu {
    /// Create a compositor and surface, or `None` (skip) when
    /// `TZE_HUD_SKIP_GPU_TESTS=1` or no adapter exists.
    pub(crate) async fn new(width: u32, height: u32) -> Option<Self> {
        let (compositor, surface) = new_gpu(width, height).await?;
        Some(Self {
            compositor,
            surface,
            width,
        })
    }

    pub(crate) async fn with_widgets(width: u32, height: u32) -> Option<Self> {
        let mut gpu = Self::new(width, height).await?;
        gpu.compositor
            .init_widget_renderer(wgpu::TextureFormat::Rgba8UnormSrgb);
        Some(gpu)
    }

    pub(crate) fn draw(&mut self, scene: &mut SceneGraph) -> FrameTelemetry {
        self.compositor.prime_markdown_cache(scene);
        self.compositor.render_frame_headless(scene, &self.surface)
    }

    pub(crate) fn pixels(&self) -> Vec<u8> {
        self.surface.read_pixels(&self.compositor.device)
    }

    pub(crate) fn render_pixels(&mut self, scene: &mut SceneGraph) -> Vec<u8> {
        self.draw(scene);
        self.pixels()
    }

    pub(crate) fn render(&mut self, scene: &mut SceneGraph) -> Frame {
        Frame {
            pixels: self.render_pixels(scene),
            width: self.width,
        }
    }
}

impl Frame {
    pub(crate) fn at(&self, x: u32, y: u32) -> [u8; 4] {
        pixels::pixel_at(&self.pixels, self.width, x, y)
    }

    /// Panic unless the pixel at (x, y) is within `tol` of `expected` on every channel.
    #[track_caller]
    pub(crate) fn expect(&self, x: u32, y: u32, expected: [u8; 4], tol: u8, what: &str) {
        pixels::assert_pixel_color(&self.pixels, self.width, x, y, expected, tol, what)
            .unwrap_or_else(|e| panic!("{what} at ({x},{y}): {e}"));
    }

    /// Whether any pixel in the half-open rect is brighter than 180 on all channels
    /// (white text over a dark backdrop).
    pub(crate) fn has_bright_pixel(
        &self,
        xs: std::ops::Range<u32>,
        ys: std::ops::Range<u32>,
    ) -> bool {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .any(|(x, y)| self.at(x, y)[..3].iter().all(|&c| c > 180))
    }
}

/// Publish a notification payload to `zone` as `publisher`.
pub(crate) fn publish_notification(
    scene: &mut SceneGraph,
    zone: &str,
    urgency: u32,
    icon: String,
    publisher: &str,
) {
    scene
        .publish_to_zone(
            zone,
            ZoneContent::Notification(NotificationPayload {
                text: format!("urgency {urgency}"),
                icon,
                urgency,
                ttl_ms: None,
                title: String::new(),
                actions: Vec::new(),
            }),
            publisher,
            None,
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("publish to {zone} failed: {e:?}"));
}

pub(crate) fn register_bundle(
    compositor: &mut Compositor,
    scene: &mut SceneGraph,
    loaded: BundleScanResult,
) -> String {
    let BundleScanResult::Ok(bundle) = loaded else {
        panic!("shipped bundle: {loaded:?}")
    };
    let name = bundle.definition.id.clone();
    let renderer = compositor.widget_renderer_mut().unwrap();
    for (file, bytes) in bundle.svg_contents {
        renderer.register_svg(&name, &file, bytes);
    }
    scene.widget_registry.register_definition(bundle.definition);
    name
}

pub(crate) fn widget_defaults(
    widget: &tze_hud_scene::WidgetDefinition,
) -> HashMap<String, WidgetParameterValue> {
    widget
        .parameter_schema
        .iter()
        .map(|p| (p.name.clone(), p.default_value.clone()))
        .collect()
}

pub(crate) const fn relative(
    x_pct: f32,
    y_pct: f32,
    width_pct: f32,
    height_pct: f32,
) -> GeometryPolicy {
    GeometryPolicy::Relative {
        x_pct,
        y_pct,
        width_pct,
        height_pct,
    }
}

pub(crate) fn register_instance(
    scene: &mut SceneGraph,
    tab: SceneId,
    widget_type: &str,
    name: &str,
    geometry: Option<GeometryPolicy>,
    use_defaults: bool,
) {
    let current_params = if use_defaults {
        widget_defaults(&scene.widget_registry.definitions[widget_type])
    } else {
        HashMap::new()
    };
    scene.widget_registry.register_instance(WidgetInstance {
        id: SceneId::new(),
        widget_type_name: widget_type.into(),
        tab_id: tab,
        geometry_override: geometry,
        contention_override: None,
        instance_name: name.into(),
        current_params,
        auto_clear_ms: None,
    });
}

pub(crate) fn publish_params<'a>(
    scene: &mut SceneGraph,
    instance: &str,
    publisher: &str,
    transition_ms: u32,
    params: impl IntoIterator<Item = (&'a str, WidgetParameterValue)>,
) {
    let params = params
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
    scene
        .publish_to_widget(instance, params, publisher, None, transition_ms, None)
        .unwrap();
}
