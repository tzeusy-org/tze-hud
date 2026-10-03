//! Shared helpers for the compositor GPU integration tests.
//!
//! Each test binary compiles this module separately and uses a subset of it.
#![allow(dead_code)]

use tokio::sync::Mutex;
use tze_hud_compositor::{Compositor, CompositorError, surface::HeadlessSurface};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::{NotificationPayload, ZoneContent};

/// Serializes headless device creation within one test binary.
///
/// Concurrent wgpu/Vulkan device construction from the default parallel libtest
/// harness can wedge the driver. Every multi-test binary under `tests/` must
/// create its device through this helper; single-test binaries (`gpu_canary`,
/// `widget_raster_count`) need no guard. Run GPU suites with `just test-gpu`,
/// which also pins the Vulkan loader to Mesa llvmpipe.
static GPU_INIT_MUTEX: Mutex<()> = Mutex::const_new(());

pub async fn new_headless_serialized(w: u32, h: u32) -> Result<Compositor, CompositorError> {
    let _guard = GPU_INIT_MUTEX.lock().await;
    Compositor::new_headless(w, h).await
}

/// A headless compositor plus the surface it renders into.
pub struct Gpu {
    pub compositor: Compositor,
    surface: HeadlessSurface,
    width: u32,
}

/// The sRGB bytes of one rendered frame.
pub struct Frame {
    pixels: Vec<u8>,
    width: u32,
}

impl Gpu {
    /// Create a compositor and surface, or `None` (skip) when
    /// `TZE_HUD_SKIP_GPU_TESTS=1` or no adapter exists.
    pub async fn new(width: u32, height: u32) -> Option<Self> {
        if std::env::var("TZE_HUD_SKIP_GPU_TESTS").is_ok_and(|v| v.trim() == "1") {
            eprintln!("skipping GPU test: TZE_HUD_SKIP_GPU_TESTS=1");
            return None;
        }
        match new_headless_serialized(width, height).await {
            Ok(compositor) => {
                let surface = HeadlessSurface::new(&compositor.device, width, height);
                Some(Self {
                    compositor,
                    surface,
                    width,
                })
            }
            Err(CompositorError::NoAdapter) => {
                eprintln!("skipping GPU test: no wgpu adapter available");
                None
            }
            Err(e) => panic!("unexpected compositor error: {e}"),
        }
    }

    /// Render `scene` and read the frame back. Reusable across scenes.
    pub fn render(&mut self, scene: &mut SceneGraph) -> Frame {
        self.compositor.prime_markdown_cache(scene);
        self.compositor.render_frame_headless(scene, &self.surface);
        Frame {
            pixels: self.surface.read_pixels(&self.compositor.device),
            width: self.width,
        }
    }
}

impl Frame {
    pub fn at(&self, x: u32, y: u32) -> [u8; 4] {
        HeadlessSurface::pixel_at(&self.pixels, self.width, x, y)
    }

    /// Panic unless the pixel at (x, y) is within `tol` of `expected` on every channel.
    #[track_caller]
    pub fn expect(&self, x: u32, y: u32, expected: [u8; 4], tol: u8, what: &str) {
        HeadlessSurface::assert_pixel_color(&self.pixels, self.width, x, y, expected, tol, what)
            .unwrap_or_else(|e| panic!("{what} at ({x},{y}): {e}"));
    }

    /// Whether any pixel in the half-open rect is brighter than 180 on all channels
    /// (white text over a dark backdrop).
    pub fn has_bright_pixel(&self, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>) -> bool {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .any(|(x, y)| self.at(x, y)[..3].iter().all(|&c| c > 180))
    }
}

/// Publish a notification payload to `zone` as `publisher`.
pub fn publish_notification(
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
