use super::*;
use crate::surface::{HeadlessSurface, SurfaceAcquireFailure, SurfaceRecoveryOutcome};
use image_cache::{
    CARET_BLINK_HALF_PERIOD, ComposerLayout, caret_visible_at, composer_scroll_offset,
    composer_vertical_line_offset, composer_visible_line_count,
};
use tze_hud_input::{DRAG_OPACITY_BOOST, DRAG_Z_ORDER_BOOST};
use tze_hud_scene::graph::SceneGraph;

/// Mutex to serialize tests that mutate `HEADLESS_FORCE_SOFTWARE`, a
/// global environment variable.  Rust tests run in parallel by default,
/// so concurrent mutations would cause races.  This is an in-process lock;
/// it does not protect against separate test binary runs.
static ENV_VAR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes headless device creation: concurrent wgpu/Vulkan construction
/// from the parallel libtest harness can wedge the driver (see `just test-gpu`).
static GPU_INIT_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Returns `true` when the test should be skipped due to missing GPU.
///
/// Tests that require a wgpu adapter (GPU or software renderer) hang
/// indefinitely on `request_adapter` in environments without any GPU
/// or software fallback (e.g., minimal CI containers without llvmpipe).
///
/// Set `TZE_HUD_SKIP_GPU_TESTS=1` to opt out all GPU-dependent tests.
/// In CI, Mesa/llvmpipe is installed and `HEADLESS_FORCE_SOFTWARE=1` is
/// set instead, so GPU tests run via a software adapter.
fn should_skip_gpu_tests() -> bool {
    std::env::var("TZE_HUD_SKIP_GPU_TESTS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// Process-wide count of GPU-gated tests that hit `require_gpu!`'s early
/// return (no wgpu adapter available, or `TZE_HUD_SKIP_GPU_TESTS=1`).
///
/// Without this, a skipped test reports as an ordinary `test ... ok` in
/// `cargo test` output — indistinguishable from a test that actually
/// exercised the render path. PR #1148's RefCell double-borrow panic escaped
/// exactly this way: the render-path regression tests that would have caught
/// it (`markdown_primer_landing_converges_on_render_path_hud_u4lq2`,
/// `reused_compositor_across_scenes_lands_markdown_primer_hud_u4lq2`) silently
/// no-op'd in a no-GPU sandbox; only CI's GPU/llvmpipe lane actually ran them
/// (hud-7o3rw). `require_gpu!` now `eprintln!`s a loud, greppable
/// `"SKIPPED (no GPU)"` marker — carrying the exact call-site location and
/// this running count — on every skip, so `grep -c "SKIPPED (no GPU)"` over
/// captured test output tells a contributor exactly how many render-path
/// tests did NOT run for real in this environment.
static GPU_SKIP_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Skips a GPU-dependent test by returning early if no GPU is available.
///
/// Usage inside an `async fn` test:
/// ```ignore
/// let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
/// ```
///
/// Expands to a `match` that returns `()` when the helper returns `None` (no
/// adapter found or `TZE_HUD_SKIP_GPU_TESTS=1`) — by default `cargo test`
/// then reports the test as an ordinary `ok`, identical to a test that ran
/// the real render path. To make that gap visible instead of silent
/// (hud-7o3rw), the `None` arm first prints a `"SKIPPED (no GPU)"` marker
/// carrying the exact `require_gpu!` call site (`file!()`/`line!()` resolve
/// to the *invocation* site here, not this macro definition, so each call
/// site gets its own precise, independently-greppable location) and the
/// running `GPU_SKIP_COUNT`. This is deliberately NOT turned into a
/// panic/failure: no-GPU / no-llvmpipe environments (including some CI
/// lanes) are expected to skip these tests, and a hard failure there would
/// break every headless environment instead of just surfacing the coverage
/// gap.
macro_rules! require_gpu {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => {
                let skip_count =
                    GPU_SKIP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                eprintln!(
                    "SKIPPED (no GPU) [{skip_count} skipped so far] at {}:{} — this \
                     render-path test did NOT exercise the real render path in this \
                     environment; only a GPU- or llvmpipe-backed lane actually runs it",
                    file!(),
                    line!(),
                );
                return;
            }
        }
    };
}

/// Convenience: build a minimal scene with one tile containing the given node.
fn scene_with_node(node: Node) -> SceneGraph {
    let mut scene = SceneGraph::new(256.0, 256.0);
    let tab_id = scene.create_tab("test", 0).unwrap();
    let lease_id = scene.grant_lease("test", 60_000);
    let tile_id = scene
        .create_tile(
            tab_id,
            "test",
            lease_id,
            Rect::new(0.0, 0.0, 256.0, 256.0),
            1,
        )
        .unwrap();
    scene.set_tile_root(tile_id, node).unwrap();
    scene
}

/// Build the exact canonical scene accepted by the retained evidence lane and
/// return the first tile/root pair for a controlled text-only mutation.
fn canonical_retained_scene() -> (SceneGraph, SceneId, SceneId) {
    let mut scene = SceneGraph::new(1_000.0, 500.0);
    let tab_id = scene.create_tab("canonical", 0).expect("canonical tab");
    let lease_id = scene.grant_lease("canonical-agent", 60_000);
    let mut first_tile_and_root = None;

    for index in 0..50 {
        let column = index % 10;
        let row = index / 10;
        // Leave gutters so an adjacent tile's idle drag grip cannot cross the
        // first tile's retained damage boundary.
        let tile_width = 96.0;
        let tile_height = 92.0;
        let tile_id = scene
            .create_tile(
                tab_id,
                "canonical-agent",
                lease_id,
                Rect::new(
                    (column * 100) as f32,
                    (row * 100) as f32,
                    tile_width,
                    tile_height,
                ),
                index as u32,
            )
            .expect("canonical tile");
        let root_id = SceneId::new();
        scene
            .set_tile_root(
                tile_id,
                Node {
                    id: root_id,
                    children: vec![],
                    layout: Default::default(),
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "AB".into(),
                        bounds: Rect::new(0.0, 0.0, tile_width, tile_height),
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
        if index == 0 {
            first_tile_and_root = Some((tile_id, root_id));
        }
    }

    let (first_tile_id, first_root_id) = first_tile_and_root.expect("first canonical tile");
    (scene, first_tile_id, first_root_id)
}

/// Create a headless compositor and surface pair for testing.
///
/// Returns `None` (and prints a skip notice) when:
/// - `TZE_HUD_SKIP_GPU_TESTS=1` is set, or
/// - no wgpu adapter is available in the current environment.
///
/// Use the `require_gpu!` macro at the call site to early-return from the
/// test when `None` is returned:
/// ```ignore
/// let (mut compositor, surface) = require_gpu!(make_compositor_and_surface(256, 256).await);
/// ```
async fn make_compositor_and_surface(w: u32, h: u32) -> Option<(Compositor, HeadlessSurface)> {
    if should_skip_gpu_tests() {
        eprintln!("SKIPPED (no GPU) reason: TZE_HUD_SKIP_GPU_TESTS=1 is set");
        return None;
    }
    let built = {
        let _guard = GPU_INIT_MUTEX.lock().await;
        Compositor::new_headless(w, h).await
    };
    match built {
        Ok(compositor) => {
            let surface = HeadlessSurface::new(&compositor.device, w, h);
            Some((compositor, surface))
        }
        Err(CompositorError::NoAdapter) => {
            eprintln!("SKIPPED (no GPU) reason: no wgpu adapter available in this environment");
            None
        }
        Err(e) => panic!("unexpected compositor error: {e}"),
    }
}
const VIEWER_ECHO_COLOR: [u8; 4] = [0x8A, 0xB4, 0xF8, 0xFF];

mod alert_banner;
mod composer;
mod focus_chrome;
mod hit_drag;
mod idle_gate;
mod markdown_cache;
mod markdown_primer;
mod notification_layout;
mod portal_animation;
mod portal_reveal;
mod retained_image;
mod scroll;
mod surface;
mod text_render;
mod tile_opacity;
mod tokens;
mod viewer_echo;
mod zone_layers;
mod zone_notification;
mod zone_stack;
