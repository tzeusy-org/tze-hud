//! The compositor — renders the scene graph to a surface.
//!
//! For the vertical slice, this renders tiles as colored rectangles
//! with hit-region highlighting for local feedback.
//!
//! The frame pipeline is **surface-agnostic**: it renders through the
//! `CompositorSurface` trait, which is implemented by both `HeadlessSurface`
//! (offscreen) and `WindowSurface` (display-connected).  No conditional
//! compilation exists in the render path — only the surface implementation
//! differs between modes.
//!
//! Per runtime-kernel/spec.md Requirement: Headless Mode (line 198):
//! "No conditional compilation for the render path."
//!
//! ## Two-pass rendering (content → chrome)
//!
//! [`Compositor::render_frame_with_chrome`] implements the three-layer ordering required by
//! the chrome sovereignty contract:
//!   1. Background + content pass (`LoadOp::Clear` — clears and draws agent tiles)
//!   2. Chrome pass (`LoadOp::Load` — draws chrome on top of content, preserving pixels)
//!
//! This separation is the architectural foundation for future render-skip redaction
//! (capture-safe architecture): the content and chrome passes are structurally independent.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use crate::markdown::MarkdownScope;
use crate::pipeline::{
    ChromeDrawCmd, ROUNDED_RECT_OVERLAY_SHADER, ROUNDED_RECT_SHADER, RectVertex,
    RoundedRectDrawCmd, RoundedRectVertex, create_texture_rect_bind_group_layout,
    create_texture_rect_pipeline, rect_vertices,
};
use crate::surface::{CompositorSurface, HeadlessSurface, SurfaceRecoveryOutcome, WindowSurface};
use crate::text::{TextItem, TextRasterizer};
use crate::widget::WidgetRenderer;
use tze_hud_scene::DegradationLevel;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;
use tze_hud_telemetry::FrameTelemetry;

/// Runtime-selected render policy for one frame.
///
/// The compositor never derives or advances degradation state independently.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositorDegradationPolicy {
    pub level: DegradationLevel,
    pub texture_quality_threshold_px: u32,
    pub texture_scale_factor: f32,
}

impl Default for CompositorDegradationPolicy {
    fn default() -> Self {
        Self {
            level: DegradationLevel::Nominal,
            texture_quality_threshold_px: 512,
            texture_scale_factor: 0.5,
        }
    }
}

pub mod animation;
pub mod draw_cmds;
pub mod easing;
pub mod encode_pass;
pub mod focus_ring;
pub mod frame;
pub mod hit_regions;
pub mod icon;
pub mod image_cache;
mod retained;
pub mod text;
pub mod tile_render;
pub mod token_colors;
pub mod viewer_echo;
pub mod widget_geometry;
pub mod widgets;
pub mod zone_render;

pub use retained::RetainedChangeEfficiencyCapture;

// Re-export submodule items into this module's namespace so all existing code
// in mod.rs can reference them without path changes (move-only, zero refactor).
use draw_cmds::*;
use icon::*;
// pub use re-exports the public items (ImageTextureEntry, LocalComposerState,
// LocalComposerStateHandle) so callers via `tze_hud_compositor::renderer::*`
// see unchanged paths.  The pub(crate) helpers are brought in separately.
pub use focus_ring::{FocusRingOwner, FocusRingOwnerHandle};

/// Shared single-slot handle carrying the tile whose resize-grip affordance the
/// pointer is currently over (hud-wgiys). The runtime's windowed hit-test writes
/// the focused portal's `SceneId` when the pointer is over its bottom-right
/// resize corner (or `None` otherwise); the compositor drains it and swaps that
/// tile's grip to `portal.window.resize_grip.hover_color`. Latest-wins — a plain
/// overwrite-and-read, mirroring [`FocusRingOwnerHandle`].
pub type ResizeGripHoverHandle = Arc<StdMutex<Option<SceneId>>>;
use image_cache::apply_composer_slot;
pub use image_cache::{
    ComposerVisualLayoutHandle, ImageTextureEntry, LocalComposerState, LocalComposerStateHandle,
};
use text::*;
use token_colors::*;
pub use viewer_echo::{
    MAX_VIEWER_ECHO_ENTRIES, PortalViewerEchoQueue, ViewerEchoAppend, ViewerEchoEntry,
    ViewerEchoStore,
};

/// Stable, dependency-free identity for the adapter selected by wgpu.
///
/// Benchmark and validation crates use this instead of depending directly on
/// wgpu merely to prove which renderer executed a constrained lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompositorAdapterInfo {
    /// Adapter name reported by wgpu (for example, `llvmpipe`).
    pub name: String,
    /// Debug-stable wgpu backend name (for example, `Vulkan`).
    pub backend: String,
    /// Debug-stable wgpu device type (for example, `Cpu`).
    pub device_type: String,
    /// Adapter driver name reported by wgpu.
    pub driver: String,
    /// Adapter driver version/details reported by wgpu.
    pub driver_info: String,
    /// PCI vendor identifier reported by the adapter when available.
    pub vendor: u32,
    /// PCI device identifier reported by the adapter when available.
    pub device: u32,
}

impl From<wgpu::AdapterInfo> for CompositorAdapterInfo {
    fn from(info: wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name,
            backend: format!("{:?}", info.backend),
            device_type: format!("{:?}", info.device_type),
            driver: info.driver,
            driver_info: info.driver_info,
            vendor: info.vendor,
            device: info.device,
        }
    }
}

/// Coarse runtime control result for one queued window-surface recovery.
///
/// Detailed trigger evidence remains compositor-private in
/// [`SurfaceRecoveryOutcome`]. The windowed runtime uses this narrow result to
/// decide whether to repaint after a successful recovery or initiate its
/// existing terminal GPU-loss shutdown path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowSurfaceRecoveryStatus {
    /// No acquire failure had queued a recovery transition.
    #[default]
    NoPending,
    /// The surface was configured and needs one repaint.
    Reconfigured,
    /// The surface cannot resume normal presentation.
    Terminal,
}

impl WindowSurfaceRecoveryStatus {
    /// Whether a real reconfigure completed and requires one repaint.
    pub const fn reconfigured_surface(self) -> bool {
        matches!(self, Self::Reconfigured)
    }

    /// Whether the surface failure is terminal for this compositor generation.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }
}

pub struct Compositor {
    pub(crate) resident_ledger: Option<tze_hud_resource::ResidentLedger>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    adapter_info: CompositorAdapterInfo,
    pipeline: wgpu::RenderPipeline,
    /// Pipeline with no blending — writes RGBA directly. Used to clear
    /// the framebuffer to transparent in overlay mode (LoadOp::Clear
    /// doesn't write alpha correctly on some GPUs).
    clear_pipeline: wgpu::RenderPipeline,
    /// Render pipeline for textured rectangles (static image rendering).
    texture_rect_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for the texture rect pipeline (shared by all images).
    texture_rect_bind_group_layout: wgpu::BindGroupLayout,
    /// Shared linear-filtering sampler for all image textures (created once).
    image_sampler: wgpu::Sampler,
    /// SDF rounded-rectangle pipeline (fullscreen / straight-alpha mode).
    ///
    /// Used to render zone backdrops whose `RenderingPolicy` has
    /// `backdrop_radius` set.  Encoded in a separate pass after the main
    /// rect pass so rounded corners composite cleanly over the background.
    ///
    /// Uses `BlendState::ALPHA_BLENDING`; vertex colors are non-premultiplied.
    /// In overlay mode `rounded_rect_overlay_pipeline` is selected instead.
    rounded_rect_pipeline: wgpu::RenderPipeline,
    /// SDF rounded-rectangle pipeline for overlay / premultiplied-alpha mode.
    ///
    /// Identical geometry to `rounded_rect_pipeline` but uses
    /// `BlendState::PREMULTIPLIED_ALPHA_BLENDING` and
    /// `ROUNDED_RECT_OVERLAY_SHADER` (which scales all four channels by
    /// coverage).  Selected by `encode_rounded_rect_pass` when
    /// `self.overlay_mode` is true.
    rounded_rect_overlay_pipeline: wgpu::RenderPipeline,
    pub width: u32,
    pub height: u32,
    frame_number: u64,
    /// Retained snapshot and evidence slot for the narrowly-scoped canonical
    /// headless change-efficiency validation path.
    retained_render_state: retained::RetainedRenderState,
    /// Latest real window-surface lifecycle outcome. Successful recovery also
    /// invalidates the compositor-private retained proof lane at this same
    /// production fan-out point.
    latest_surface_recovery: Option<SurfaceRecoveryOutcome>,
    /// When true, the clear color uses alpha=0 for transparent overlay mode.
    pub overlay_mode: bool,
    /// When true, render all zone boundaries with colored tints even when
    /// zones have no active content. Controlled by `TZE_HUD_DEBUG_ZONES=1`.
    pub debug_zone_tints: bool,
    /// Current degradation level, set by the runtime before each frame.
    ///
    /// At [`DegradationLevel::Simplified`] or higher, widget transition
    /// interpolation is skipped and final parameter values are applied
    /// immediately to reduce re-rasterization under load.
    pub degradation_level: DegradationLevel,
    degradation_policy: CompositorDegradationPolicy,
    /// Optional text rasterizer (glyphon). Absent until `init_text_renderer`
    /// is called. When `None`, TextMarkdownNode and zone StreamText content
    /// renders as solid-color rectangles only (no glyph output).
    pub(crate) text_rasterizer: Option<TextRasterizer>,
    /// Optional widget renderer. Absent until `init_widget_renderer` is called.
    /// When `None`, widget instances in the scene graph are not composited.
    pub(crate) widget_renderer: Option<WidgetRenderer>,
    /// Per-zone fade-in / fade-out animation state.
    ///
    /// Keyed by zone name. An entry is inserted on the first publish to a zone
    /// that has `transition_in_ms > 0`, and on every zone clear when
    /// `transition_out_ms > 0`. Completed transitions are pruned each frame.
    pub(crate) zone_animation_states: HashMap<String, ZoneAnimationState>,
    /// Track which zones had active publishes in the previous frame so we can
    /// detect publish → clear transitions and start fade-out animations.
    prev_active_zones: HashMap<String, bool>,
    /// Per-portal-tile fade-in / fade-out animation state (§6.3 transition tokens).
    ///
    /// Keyed by tile `SceneId`. An entry is inserted when a scrollable tile
    /// (i.e. a portal tile, identified by a registered `TileScrollConfig`)
    /// gains its first root node (`transition_in_ms` fade-in) and when it
    /// loses its root node (`transition_out_ms` fade-out).
    ///
    /// Durations are sourced from the `portal.transition.in_ms` and
    /// `portal.transition.out_ms` design tokens so no literal durations are
    /// present in the compositor.  Completed transitions are pruned each frame.
    pub(crate) portal_tile_anim_states: HashMap<SceneId, ZoneAnimationState>,
    /// Track which scrollable tiles had a root node in the previous frame so
    /// we can detect appear/disappear transitions.
    prev_portal_tile_has_content: HashMap<SceneId, bool>,
    /// Per-publication TTL fade-out animation state.
    ///
    /// Outer key: zone name.  Inner key: `PubKey = (published_at_wall_us, publisher_namespace)`.
    ///
    /// Created when the compositor first encounters a publication in a Stack zone.
    /// Entries for publications no longer in `active_publishes` are pruned by
    /// [`Compositor::prune_faded_publications`].
    pub(crate) pub_animation_states: HashMap<String, HashMap<PubKey, PublicationAnimationState>>,
    /// Per-zone streaming word-by-word reveal state.
    ///
    /// Keyed by zone name.  Present only when the latest publication in that zone
    /// has non-empty breakpoints.  Absent (or None) means reveal all at once.
    ///
    /// Per spec §Subtitle Streaming Word-by-Word Reveal: when a new publication
    /// replaces the old one (latest-wins), the old reveal state is discarded and
    /// a new one starts from the beginning of the new breakpoints.
    pub(crate) stream_reveal_states: HashMap<String, StreamRevealState>,
    /// Per-portal-tile streaming-reveal state (hud-bl7yi).
    ///
    /// Keyed by `(tile SceneId, markdown-node SceneId)` — one entry **per
    /// eligible markdown node** under a scrollable (portal) tile, NOT one per
    /// tile (hud-tbdfx). Per-node keying makes the tracker robust to the *first
    /// eligible node* changing between frames: a portal input tile carries both
    /// a settled history markdown node and a composer draft node whose
    /// pixel-bearing color runs toggle its eligibility, so a tile-keyed tracker
    /// would flip between the two nodes' plain-texts and spuriously word-reveal
    /// the entire history on every composer submit. With per-node keying each
    /// node diffs only against its own prior snapshot, so a genuine append to
    /// one node fades only that node's changed suffix and a node appearing /
    /// disappearing never mimics growth of a different node.
    ///
    /// Fades **newly-appended** content segment-by-segment via [`StreamFadeRamp`]
    /// instead of snapping. An entry is held for every eligible markdown node —
    /// settled (non-animating) once its current content is fully revealed — so
    /// the next frame can diff against the prior plain-text snapshot and start a
    /// fresh reveal when that node's content grows. Pruned when the node (or its
    /// tile) disappears.
    ///
    /// [`StreamFadeRamp`]: crate::renderer::easing::StreamFadeRamp
    pub(crate) portal_tile_reveal_states: HashMap<(SceneId, SceneId), PortalTileStreamReveal>,
    /// Per-portal-tile smoothed scroll offset (smooth scroll / animated
    /// follow-tail, hud-bq0gl.10).
    ///
    /// Keyed by tile `SceneId`. The adapter/input layer owns the authoritative
    /// scroll *target* (`SceneGraph::tile_scroll_offset_local`); this eases the
    /// *displayed* offset toward it each frame. Advanced once per frame by
    /// [`Compositor::update_scroll_smoothing`]; read via
    /// [`Compositor::display_tile_scroll_offset`].
    pub(crate) scroll_smoothers: HashMap<SceneId, easing::ScrollSmoother>,
    /// Whether scroll smoothing animates (windowed/live) or snaps (headless).
    ///
    /// Headless render paths back the deterministic golden tests that assert
    /// exact scrolled glyph positions, so smoothing is disabled there: the
    /// displayed offset always equals the scene target.
    pub(crate) scroll_smoothing_enabled: bool,
    /// Wall-clock instant of the last [`Compositor::update_scroll_smoothing`]
    /// call, used to derive the per-frame `dt` for frame-rate-independent
    /// smoothing. `None` until the first frame.
    last_scroll_smooth_at: Option<std::time::Instant>,
    /// Resolved design token map, set at startup via `set_token_map`.
    ///
    /// Used to resolve `color.severity.{info,warning,critical}` tokens for
    /// alert-banner backdrop colors. When empty (the default), all severity
    /// lookups fall back to the hardcoded `SEVERITY_*` constants.
    ///
    /// Populated by calling `set_token_map` with the global token map from
    /// scene startup.
    pub token_map: HashMap<String, String>,
    /// Decoded RGBA image bytes indexed by `ResourceId`.
    ///
    /// Populated by calling [`Compositor::register_image_bytes`] after an image
    /// resource is uploaded. The compositor decodes/uploads to GPU texture on
    /// first reference via [`Compositor::ensure_image_texture`].
    ///
    /// The raw bytes are kept until the ResourceId is no longer referenced by
    /// any scene node or zone publication (evicted by `evict_unused_image_textures`).
    image_bytes: HashMap<ResourceId, Arc<[u8]>>,
    /// Explicit pixel dimensions for registered image resources, keyed by `ResourceId`.
    ///
    /// Populated alongside `image_bytes` by [`Compositor::register_image_bytes`].
    /// Stores `(width, height)` so that `ensure_scene_image_textures` can pass
    /// exact dimensions to [`Compositor::ensure_image_texture`] without resorting
    /// to the square-root heuristic that fails for non-square images.
    image_dims: HashMap<ResourceId, (u32, u32)>,
    /// GPU texture cache for static images, keyed by `ResourceId`.
    ///
    /// Entries are created on-demand by `ensure_image_texture` and evicted
    /// when the resource is no longer referenced in the scene.
    ///
    /// Also stores status-bar icon textures via `ensure_icon_texture`, which
    /// uses `ResourceId::of(svg_path.as_bytes())` as the key.  Icon entries
    /// are kept alive by including their ResourceIds in the eviction-guard set
    /// (see `ensure_scene_icon_textures`).
    image_texture_cache: HashMap<ResourceId, ImageTextureEntry>,
    /// Negative cache for SVG paths that failed to load/parse.
    ///
    /// Paths are inserted on the first failed `ensure_icon_texture` call.
    /// Subsequent calls skip the filesystem read entirely, avoiding per-frame
    /// I/O and log spam for invalid icon paths. Entries persist for the
    /// lifetime of the compositor (SVG paths are static config; runtime reload
    /// of icon paths is not currently supported).
    failed_icon_paths: HashSet<ResourceId>,
    /// Phase-1 markdown parse cache, fronted by the async primer (hud-5jbra.2,
    /// hud-380dl, hud-33qo7).
    ///
    /// Maps BLAKE3(content) → [`ParsedMarkdown`].  Populated at content-commit
    /// time by [`Compositor::prime_markdown_cache`]; consumed at render time by
    /// [`Compositor::collect_text_items_from_node`] with zero parse cost on
    /// cache hits.  The cache is evicted in sync with node removal.
    ///
    /// hud-33qo7: the primer owns an `Arc<ArcSwap<MarkdownCache>>`.  Large
    /// payloads are parsed on a background thread and swapped in atomically; the
    /// render path `load()`s the current snapshot lock-free.  Read it via
    /// [`Compositor::markdown_cache`].
    ///
    /// [`ParsedMarkdown`]: crate::markdown::ParsedMarkdown
    pub(crate) markdown_primer: crate::markdown::MarkdownPrimer,
    /// Per-node content-key cache (hud-gpqde).
    ///
    /// Maps `SceneId` → precomputed BLAKE3 content key, populated at prime time
    /// alongside `markdown_cache`.  Eliminates the per-frame BLAKE3 re-hash in
    /// `collect_text_items_from_node` — the frame path does a 32-byte HashMap
    /// lookup rather than hashing the full content string every frame.
    ///
    /// Invalidated and rebuilt on every `prime_markdown_cache` call when the
    /// scene version changes.  Evicted in sync with `markdown_cache.evict_except`.
    pub(crate) node_key_cache: HashMap<SceneId, [u8; 32]>,
    /// Cumulative count of `collect_text_items_from_node` markdown-cache misses
    /// on the render path (hud-u4lq2).
    ///
    /// A miss here means the primer's published snapshot did not contain the
    /// entry for a node's content key, so the render path fell back to
    /// [`Self::markdown_fallback_cache`] (or a fresh inline parse on that
    /// cache's own miss). In steady state (commit-time-primed, background
    /// parse landed) this must stay at 0 after the first prime; a nonzero,
    /// growing count means the primer's background result never landed and
    /// the render path is silently paying re-parse cost every frame. Kept as
    /// defense-in-depth observability (surfaced by
    /// [`Self::markdown_cache_miss_count`]), not just a test hook.
    ///
    /// `Cell`, not a plain `u64`: incremented from `collect_text_items_from_node`,
    /// which takes `&self` (the collect phase is read-only by design), so a
    /// shared-reference-compatible counter is needed. Single-threaded (render
    /// path only) — no atomic required.
    pub(crate) markdown_cache_miss_count: std::cell::Cell<u64>,
    /// Self-healing fallback cache for markdown-cache misses on the render
    /// path (hud-u4lq2 defense-in-depth).
    ///
    /// A miss in [`Self::markdown_primer`]'s authoritative snapshot is
    /// expected only transiently (first frame before any commit-time prime,
    /// or a node added mid-frame after `prime_markdown_cache` already ran).
    /// Whatever the cause, `collect_text_items_from_node` inserts the
    /// inline-parsed result here so the SAME miss is never paid twice: the
    /// next frame's lookup checks this cache before falling back to a fresh
    /// `parse_markdown_subset` call. This bounds the cost of ANY cache-miss
    /// condition — including ones not yet understood — to one parse per
    /// distinct missing key, rather than one parse per frame forever.
    ///
    /// Deliberately a small side cache (O(1) insert), not a clone-and-replace
    /// of the primer's authoritative `MarkdownCache`: the miss path must never
    /// pay an O(cache size) cost just to self-heal one entry. Cleared at the
    /// start of every successful `prime_markdown_cache` call — a fresh
    /// commit-time prime supersedes any entries parked here, so nothing here
    /// can shadow updated content or grow unbounded across scene mutations.
    ///
    /// `RefCell`, not `Cell`, because the value type (`ParsedMarkdown`) is not
    /// `Copy`; `collect_text_items_from_node` still only needs `&self`.
    pub(crate) markdown_fallback_cache:
        std::cell::RefCell<HashMap<[u8; 32], crate::markdown::ParsedMarkdown>>,
    /// Design-token–resolved markdown styling for the **portal** transcript
    /// surface (hud-5jbra.2) — prefers `portal.*`-namespaced keys.
    ///
    /// Rebuilt from `token_map` whenever `set_token_map` is called.
    /// Consumed by `prime_markdown_cache` and the render path for markdown nodes
    /// that belong to a governed portal surface (a tile with a scroll config).
    pub(crate) markdown_tokens: crate::markdown::MarkdownTokens,
    /// Design-token–resolved markdown styling for any **non-portal** markdown
    /// surface (hud-3ryie) — resolves only the generic keys, ignoring every
    /// `portal.*`-namespaced token so a portal preference cannot leak onto it.
    ///
    /// Rebuilt alongside [`Self::markdown_tokens`] in `set_token_map`.  In v1
    /// there is no non-portal markdown surface, so this is never selected; it
    /// exists so per-tile scoping is correct the moment one is introduced.
    pub(crate) markdown_tokens_generic: crate::markdown::MarkdownTokens,
    /// The `SceneGraph::version` value at the last `prime_markdown_cache` call.
    ///
    /// `prime_markdown_cache` is gated on this value so it only runs when the
    /// scene has actually changed.  Initialized to `u64::MAX` so the first
    /// frame always primes.
    ///
    /// hud-u4lq2: `version` alone cannot distinguish "the same scene,
    /// unchanged" from "a DIFFERENT `SceneGraph` instance that happens to have
    /// reached the same version number" — two independently-constructed
    /// `SceneGraph`s commonly do (e.g. two benchmark scenarios sharing one
    /// `Compositor`). See [`Self::markdown_cache_scene_instance`], checked
    /// alongside this field, for the guard against that case.
    markdown_cache_scene_version: u64,
    /// The process-unique `SceneGraph::instance_id` last seen by
    /// `prime_markdown_cache`, checked alongside
    /// [`Self::markdown_cache_scene_version`] (hud-u4lq2). `None` means "never
    /// primed". A mismatch here forces a re-prime even when `scene.version`
    /// happens to equal the stored sentinel, so a `Compositor` reused across a
    /// freshly-constructed `SceneGraph` can never have its priming silently
    /// skipped by version-number coincidence.
    markdown_cache_scene_instance: Option<u64>,
    /// The `SceneGraph::version` value at the last `prime_truncation_cache` call.
    ///
    /// `prime_truncation_cache` is gated on this value so it only runs when the
    /// scene has actually changed.  Initialized to `u64::MAX` so the first
    /// frame always primes.
    ///
    /// hud-u4lq2: see [`Self::markdown_cache_scene_version`]'s doc for why
    /// [`Self::truncation_cache_scene_instance`] is checked alongside it.
    truncation_cache_scene_version: u64,
    /// The process-unique `SceneGraph::instance_id` last seen by
    /// `prime_truncation_cache`. Same rationale as
    /// [`Self::markdown_cache_scene_instance`] (hud-u4lq2).
    truncation_cache_scene_instance: Option<u64>,
    /// Operator-configured per-surface bound (bytes) for the truncation cache's
    /// viewport-adjacent-window fallback, sourced from the resolved
    /// `DisplayProfile` via [`Compositor::set_max_truncation_input_bytes`].
    ///
    /// `None` until the runtime applies config, in which case the
    /// [`crate::text::TruncationCache`] keeps its own default
    /// (`overflow::DEFAULT_MAX_TRUNCATION_INPUT_BYTES`, 4096). Retained on the
    /// compositor so it is re-applied if [`Compositor::init_text_renderer`] is
    /// called again (surface resize / format change replaces the rasterizer,
    /// resetting its cache to the default bound).
    configured_max_truncation_input_bytes: Option<usize>,
    /// Instant of the last completed `prime_truncation_cache` run.
    ///
    /// Used by the adaptive mid-drag re-truncation cadence gate (hud-ghhxa,
    /// hud-3to8i): when the scene version changes rapidly (e.g. per-frame
    /// bounds updates during a resize drag), we cap the re-prime rate to at
    /// most once per the adaptive interval so we never re-prime every frame.
    ///
    /// `None` means no prime has run yet (the first frame always primes).
    /// When `Some`, a new prime is only allowed when at least the adaptive
    /// interval (derived from [`resize_reprime_content_bytes`]) has elapsed.
    resize_reprime_last_at: Option<std::time::Instant>,
    /// Total byte count of Ellipsis text content across all visible tiles at
    /// the time of the last successful `prime_truncation_cache` run.
    ///
    /// Used as the input to [`adaptive_reprime_interval_ms`] on the *next*
    /// cadence-gate check.  Using the last-prime byte count (rather than
    /// recomputing from the scene on every deferred frame) keeps the gate
    /// decision O(1) with no scene traversal on deferred frames.
    ///
    /// Initialized to 0 so the first prime always uses the short interval
    /// (which is effectively bypassed because `resize_reprime_last_at` is
    /// `None` and the gate never defers on the first call).
    resize_reprime_content_bytes: usize,
    /// Shared handle for receiving local composer echo state from the
    /// input-event thread (main thread) without blocking on the scene lock.
    ///
    /// Drained at the start of each frame loop iteration in the compositor
    /// thread.  `None` when no composer is active or no draft has arrived
    /// for the current frame.
    pub local_composer_state: LocalComposerStateHandle,
    /// Shared queue for runtime-authored viewer reply echoes on raw-tile portals
    /// (hud-nx7yq.3).  The windowed runtime pushes a [`viewer_echo::ViewerEchoAppend`]
    /// on each accepted raw-tile composer submission; the compositor drains it
    /// into `viewer_echoes` at frame start.
    pub viewer_echo_queue: viewer_echo::PortalViewerEchoQueue,
    /// Per-tile bounded store of runtime-authored viewer echo entries, rendered
    /// as kind-distinct lines above the composer input strip.  Fed by draining
    /// `viewer_echo_queue` and pruned to the live tile set each frame.
    pub(crate) viewer_echoes: viewer_echo::ViewerEchoStore,
    /// Reverse channel: the active composer's wrapped-line layout, published each
    /// frame for the input thread's soft-wrap vertical caret movement (hud-21o6x).
    ///
    /// Written at the tail of [`Compositor::prime_composer_scroll_offset`] (this
    /// thread) with the multi-line profile's freshly measured layout, or `None`
    /// otherwise. The runtime clones this `Arc` at init and reads it on the main
    /// thread before dispatching ArrowUp/ArrowDown.
    pub composer_visual_layout: image_cache::ComposerVisualLayoutHandle,
    /// Per-tile total WRAPPED visual-line count of the joined viewer-echo history,
    /// measured once per frame in `prime_viewer_echo_layout` (hud-pncm3).
    ///
    /// Wrapping needs the `&mut` text rasterizer (font metrics), so it is measured
    /// in the per-frame prime and read by the `&self` `collect_viewer_echo_text_items`
    /// to bottom-align the history block above the live composer box. Absent =>
    /// the collect path falls back to the logical (`\n`-split) line count.
    pub(crate) viewer_echo_line_counts: HashMap<SceneId, usize>,
    /// Per-tile PER-ENTRY wrapped visual-line counts of the viewer-echo history,
    /// oldest→newest, measured alongside [`Self::viewer_echo_line_counts`] in
    /// `prime_viewer_echo_layout` (hud-hsc1t).
    ///
    /// Feeds `viewer_echo_divider_rects`, which places a token-styled divider on
    /// the cumulative-line boundary between each adjacent pair of entries so the
    /// pilot-path history reads as discrete turns. Absent => no per-entry divider
    /// geometry that frame (the joined text still renders).
    pub(crate) viewer_echo_entry_line_counts: HashMap<SceneId, Vec<usize>>,
    /// Per-frame resolved vertical-flow y-offsets (hud-pd9bp): for each direct
    /// child of a `NodeLayout::VerticalFlow` node, its runtime-stacked tile-local
    /// `y` keyed by the child's own `SceneId`. Populated once per frame by
    /// [`Self::prime_vertical_flow_layout`] and read at the `&self` geometry sites
    /// (`render_node` and the text-collect walks), which substitute the resolved
    /// `y` for the child's own `bounds.y`.
    ///
    /// EMPTY for any scene with no `VerticalFlow` node (the overwhelmingly common
    /// case) — every geometry site then falls back to `bounds.y` and rendering is
    /// byte-identical to before this capability existed.
    pub(crate) tile_flow_offsets: HashMap<SceneId, f32>,
    /// Shared handle carrying the current keyboard-focus owner from the runtime
    /// `FocusManager` (hud-k6yvb). Drained at frame start into `focus_ring_owner`;
    /// the chrome-layer ring pass draws the ring for whatever owner it names.
    pub focus_ring_owner_state: focus_ring::FocusRingOwnerHandle,
    /// Most-recently drained focus-ring owner for this frame. `None` when focus is
    /// cleared / chrome / on another tab.
    pub(crate) focus_ring_owner: Option<focus_ring::FocusRingOwner>,
    /// Shared handle carrying the portal tile whose resize-grip corner the pointer
    /// is over (hud-wgiys). Drained at frame start into `resize_grip_hover`; the
    /// grip pass paints that tile's mark in `hover_color`.
    pub resize_grip_hover_state: ResizeGripHoverHandle,
    /// Most-recently drained resize-grip hover target for this frame. `Some(tile)`
    /// when the pointer is over that portal's bottom-right resize corner; `None`
    /// otherwise (every grip renders resting).
    pub(crate) resize_grip_hover: Option<SceneId>,
    /// Most-recently drained local composer state for this frame.
    ///
    /// Populated by draining `local_composer_state` at frame start.
    /// Consumed by `collect_text_items` (text pass) and the geometry pass
    /// (background rect + caret rect).  Cleared when the handle delivers
    /// `None` (composer deactivated).
    pub(crate) local_composer: Option<LocalComposerState>,
    /// Reference instant for the composer caret blink phase.
    ///
    /// The caret toggles solid/hidden every [`image_cache::CARET_BLINK_HALF_PERIOD`]
    /// measured from this instant.  Reset to "now" whenever a new draft state is
    /// applied (keystroke / caret move) so the caret is solid immediately after
    /// typing.  Computed per-frame in `collect_composer_text_item`; the model is
    /// never in this loop.
    pub(crate) composer_caret_blink_start: std::time::Instant,
    /// Caret phase the idle gate last reported (`None` = no composer active).
    pub(crate) composer_caret_rendered_phase: Option<bool>,
    /// Per-frame composer layout state (hud-zlfi4 single-line caret-follow +
    /// hud-nx7yq.1 multi-line wrap / upward growth / vertical scroll).
    ///
    /// Recomputed once per frame by [`Compositor::prime_composer_scroll_offset`]
    /// (which measures the draft against the composer font — a mutable rasterizer
    /// borrow the immutable collect path lacks) BEFORE `collect_text_items`, and
    /// consumed by `render_composer_overlay` + `collect_composer_text_item`.
    /// Resets to [`ComposerLayout::default`] when no composer is active.  This is
    /// local presentation state — no adapter round trip — subject to the same
    /// redaction / safe-mode / focus rules as the rest of the draft (it is only
    /// ever applied while the composer overlay itself renders).
    pub(crate) composer_layout: image_cache::ComposerLayout,
}

/// Partitioned rounded-rectangle draw commands organized by layer.
pub(crate) struct LayerPartitionedRoundedRectCmds {
    pub(crate) background: Vec<RoundedRectDrawCmd>,
    pub(crate) content: Vec<RoundedRectDrawCmd>,
    pub(crate) chrome: Vec<RoundedRectDrawCmd>,
}

/// Owned, scene-free inputs for the GPU encode stage (hud-uyhpn).
///
/// Produced by [`Compositor::collect_encode_inputs`] — which performs ALL of the
/// scene reads the encode stage previously did inline (rounded-rect command
/// collection, composer/echo layout priming, text-item collection + glyphon
/// prepare) — and consumed by [`Compositor::encode_from_inputs`], which touches
/// only the GPU surface view and these precomputed values.
///
/// Splitting the scene-read half out of the encode half is what lets the
/// windowed frame loop DROP the scene lock before the vsync-blocking
/// `acquire_frame()` + submit + `device.poll(Wait)`: the whole encode tail now
/// runs lock-free against this owned struct rather than borrowing `&SceneGraph`.
pub(crate) struct EncodeInputs {
    /// Background-layer SDF rounded-rect commands (drawn between the Clear pass
    /// and the tile/content/chrome flat-rect pass).
    rr_background: Vec<RoundedRectDrawCmd>,
    /// Content + Chrome + per-tile SDF rounded-rect commands (drawn above tiles).
    rr_post: Vec<RoundedRectDrawCmd>,
    /// Inline code-span backdrop vertices, already color-transformed through
    /// `gpu_color_raw` (so the encode stage needs no `&self` method borrow).
    inline_verts: Vec<RectVertex>,
    /// Whether glyphon text was prepared this frame and the text pass should be
    /// replayed. `false` when there is no rasterizer, no text items, or prepare
    /// failed — in which case the text pass is skipped (but the atlas is still
    /// trimmed) exactly as before the split.
    render_text: bool,
}

/// Per-zone Stack slot layout, computed once per zone per frame by
/// [`Compositor::zone_slot_layout`] and consumed by all zone-rendering call
/// sites that need slot geometry: `collect_text_items`, `render_zone_content`,
/// and `collect_all_rounded_rect_cmds`.
///
/// Centralising this computation eliminates the hand-mirrored duplication that
/// was previously guarded by a "must-mirror" comment (hud-lu50e / hud-qlerb).
#[derive(Debug)]
pub(crate) struct ZoneSlotLayout {
    /// Publication indices sorted by the zone's ordering rule (alert-banner:
    /// severity-descending, then recency; others: newest-first / reverse order).
    pub(crate) ordered_indices: Vec<usize>,
    /// Per-slot heights in the same order as `ordered_indices`.
    /// Two-line notifications produce a taller slot; all others use
    /// `stack_slot_height`.
    pub(crate) slot_heights: Vec<f32>,
    /// Cumulative y-offsets from the zone origin, one per slot.
    /// `slot_offsets[i]` is the y-start of slot `i` relative to `zy`.
    pub(crate) slot_offsets: Vec<f32>,
    /// Effective zone height in pixels.
    /// For alert-banner zones this equals `sum(slot_heights)` (dynamic growth).
    /// For all other Stack zones this equals the configured zone height `zh`.
    pub(crate) effective_h: f32,
}

impl ZoneSlotLayout {
    /// Iterate `(pub_idx, slot_offset, slot_h)` triples for the slots that
    /// fall inside the zone.  Slots whose top edge is at or beyond `zy +
    /// effective_h` are excluded (the iterator stops early).
    pub(crate) fn iter_visible(&self, zy: f32) -> impl Iterator<Item = (usize, f32, f32)> + '_ {
        let zone_bottom = zy + self.effective_h;
        self.ordered_indices
            .iter()
            .copied()
            .zip(
                self.slot_offsets
                    .iter()
                    .copied()
                    .zip(self.slot_heights.iter().copied()),
            )
            .take_while(move |(_, (offset, _))| zy + offset < zone_bottom)
            .map(move |(pub_idx, (offset, slot_h))| {
                let slot_y = zy + offset;
                let effective_slot_h = slot_h.min(zone_bottom - slot_y);
                (pub_idx, slot_y, effective_slot_h)
            })
    }
}

/// Backends the windowed compositor requests: Vulkan on Linux, D3D12 and
/// Vulkan on Windows, Metal on macOS. Headless construction uses
/// `Backends::all()` so software renderers (llvmpipe, WARP) still work.
fn platform_backends() -> wgpu::Backends {
    if cfg!(target_os = "linux") {
        wgpu::Backends::VULKAN
    } else if cfg!(target_os = "windows") {
        wgpu::Backends::DX12 | wgpu::Backends::VULKAN
    } else if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::all()
    }
}

impl Compositor {
    /// Create a new headless compositor.
    ///
    /// Checks the `HEADLESS_FORCE_SOFTWARE` environment variable.  When set to
    /// `1`, wgpu adapter selection uses `force_fallback_adapter = true`, which
    /// selects llvmpipe on Linux or WARP on Windows.
    ///
    /// Per runtime-kernel/spec.md Requirement: Headless Software GPU (line 211):
    /// "When set, the wgpu adapter selection MUST request a software fallback
    /// (force_fallback_adapter = true)."
    pub async fn new_headless(width: u32, height: u32) -> Result<Self, CompositorError> {
        // Check HEADLESS_FORCE_SOFTWARE env var (spec line 409: "conventionally
        // named HEADLESS_FORCE_SOFTWARE").
        let force_software = std::env::var("HEADLESS_FORCE_SOFTWARE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: force_software,
            })
            .await
            .ok_or(CompositorError::NoAdapter)?;
        let adapter_info = CompositorAdapterInfo::from(adapter.get_info());

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("tze_hud_compositor"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await
            .map_err(|e| CompositorError::DeviceCreation(e.to_string()))?;

        let pipeline = Self::create_pipeline(&device);
        let clear_pipeline =
            Self::create_clear_pipeline(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let rounded_rect_pipeline =
            Self::create_rounded_rect_pipeline(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let rounded_rect_overlay_pipeline = Self::create_rounded_rect_overlay_pipeline(
            &device,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );

        let texture_rect_bind_group_layout = create_texture_rect_bind_group_layout(&device);
        let texture_rect_pipeline = create_texture_rect_pipeline(
            &device,
            &texture_rect_bind_group_layout,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_linear_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        Ok(Self {
            device,
            queue,
            adapter_info,
            pipeline,
            clear_pipeline,
            texture_rect_pipeline,
            texture_rect_bind_group_layout,
            image_sampler,
            rounded_rect_pipeline,
            rounded_rect_overlay_pipeline,
            width,
            height,
            frame_number: 0,
            retained_render_state: retained::RetainedRenderState::default(),
            latest_surface_recovery: None,
            overlay_mode: false,
            debug_zone_tints: std::env::var("TZE_HUD_DEBUG_ZONES").is_ok_and(|v| v == "1"),
            degradation_level: DegradationLevel::Nominal,
            degradation_policy: CompositorDegradationPolicy::default(),
            text_rasterizer: None,
            widget_renderer: None,
            resident_ledger: None,
            zone_animation_states: HashMap::new(),
            prev_active_zones: HashMap::new(),
            portal_tile_anim_states: HashMap::new(),
            prev_portal_tile_has_content: HashMap::new(),
            pub_animation_states: HashMap::new(),
            stream_reveal_states: HashMap::new(),
            portal_tile_reveal_states: HashMap::new(),
            // Headless paths snap scroll (deterministic golden tests).
            scroll_smoothers: HashMap::new(),
            scroll_smoothing_enabled: false,
            last_scroll_smooth_at: None,
            token_map: HashMap::new(),
            markdown_primer: crate::markdown::MarkdownPrimer::new(),
            node_key_cache: HashMap::new(),
            markdown_cache_miss_count: std::cell::Cell::new(0),
            markdown_fallback_cache: std::cell::RefCell::new(HashMap::new()),
            markdown_tokens: crate::markdown::MarkdownTokens::default(),
            markdown_tokens_generic: crate::markdown::MarkdownTokens::default(),
            markdown_cache_scene_version: u64::MAX,
            markdown_cache_scene_instance: None,
            truncation_cache_scene_version: u64::MAX,
            truncation_cache_scene_instance: None,
            configured_max_truncation_input_bytes: None,
            resize_reprime_last_at: None,
            resize_reprime_content_bytes: 0,
            image_bytes: HashMap::new(),
            image_dims: HashMap::new(),
            image_texture_cache: HashMap::new(),
            failed_icon_paths: HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            viewer_echo_queue: Arc::new(StdMutex::new(Vec::new())),
            viewer_echoes: viewer_echo::ViewerEchoStore::new(),
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            viewer_echo_line_counts: HashMap::new(),
            viewer_echo_entry_line_counts: HashMap::new(),
            tile_flow_offsets: HashMap::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            focus_ring_owner: None,
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            resize_grip_hover: None,
            local_composer: None,
            composer_caret_blink_start: std::time::Instant::now(),
            composer_caret_rendered_phase: None,
            composer_layout: image_cache::ComposerLayout::default(),
        })
    }

    /// Create a windowed compositor backed by a real `winit::window::Window`.
    ///
    /// This is the factory method for production windowed rendering. It:
    /// 1. Requests an adapter with the platform backends (Vulkan on Linux, D3D12/Vulkan on Windows, Metal on macOS).
    /// 2. Creates a `wgpu::Surface` from the window via `instance.create_surface`.
    /// 3. Negotiates the surface format (sRGB preferred).
    /// 4. Configures the surface with the window's physical dimensions.
    /// 5. Creates the `wgpu::Device` and `wgpu::Queue`.
    ///
    /// Returns the `(Compositor, WindowSurface)` pair. The `WindowSurface` must
    /// be kept alive for the duration of the runtime.
    ///
    /// Per spec §Compositor Thread Ownership (line 46): the returned `Compositor`
    /// (and thus `Device` + `Queue`) MUST be transferred to the compositor thread
    /// immediately after creation. The `WindowSurface` is owned by the main thread.
    ///
    /// Per spec §Platform GPU Backends (line 189): this path uses the platform-
    /// mandated backends — unlike `new_headless` which uses `Backends::all()`.
    pub async fn new_windowed(
        window: std::sync::Arc<winit::window::Window>,
        width: u32,
        height: u32,
    ) -> Result<(Self, crate::surface::WindowSurface), CompositorError> {
        Self::new_windowed_inner(window, width, height, false, false).await
    }

    /// Create a windowed compositor, optionally forcing Vulkan for overlay
    /// transparency (DX12 only supports Opaque swapchain alpha mode).
    pub async fn new_windowed_overlay(
        window: std::sync::Arc<winit::window::Window>,
        width: u32,
        height: u32,
    ) -> Result<(Self, crate::surface::WindowSurface), CompositorError> {
        Self::new_windowed_inner(window, width, height, true, false).await
    }

    /// Create a windowed compositor for the bounded quiescent-efficiency lane.
    ///
    /// This is deliberately separate from normal windowed startup: CI needs a
    /// real WARP/llvmpipe surface, while an operator's overlay must retain the
    /// platform-preferred hardware adapter and transparent-swapchain policy.
    pub async fn new_windowed_constrained(
        window: std::sync::Arc<winit::window::Window>,
        width: u32,
        height: u32,
        overlay: bool,
    ) -> Result<(Self, crate::surface::WindowSurface), CompositorError> {
        Self::new_windowed_inner(window, width, height, overlay, true).await
    }

    async fn new_windowed_inner(
        window: std::sync::Arc<winit::window::Window>,
        width: u32,
        height: u32,
        overlay: bool,
        force_fallback_adapter: bool,
    ) -> Result<(Self, crate::surface::WindowSurface), CompositorError> {
        use crate::surface::WindowSurface;

        // ── Step 1: Create instance with platform-mandated backends ──────────
        // We need the surface before adapter selection so we can pass it as
        // `compatible_surface`. Create a temporary instance first, create the
        // surface, then select the adapter with that surface constraint.
        // On Windows in overlay mode, force Vulkan — DX12 only supports Opaque
        // swapchain alpha mode, which prevents per-pixel transparency.
        let backends = if force_fallback_adapter {
            // The quiescent CI lane proves the software renderer actually used
            // by WARP/llvmpipe, so it must not inherit the production overlay's
            // Vulkan-only transparency workaround.
            wgpu::Backends::all()
        } else if overlay && cfg!(target_os = "windows") {
            tracing::info!("overlay mode: forcing Vulkan backend for transparent swapchain");
            wgpu::Backends::VULKAN
        } else {
            platform_backends()
        };
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });

        // ── Step 2: Create wgpu::Surface from the winit window ───────────────
        // SAFETY: `window` is wrapped in Arc — it outlives the surface because
        // we pass 'static lifetime via Arc<Window>.
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| CompositorError::DeviceCreation(format!("create_surface: {e}")))?;

        // ── Step 3: Select adapter compatible with the surface ────────────────
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: if force_fallback_adapter {
                    wgpu::PowerPreference::LowPower
                } else {
                    wgpu::PowerPreference::HighPerformance
                },
                compatible_surface: Some(&surface),
                force_fallback_adapter,
            })
            .await
            .ok_or(CompositorError::NoAdapter)?;

        let adapter_info = adapter.get_info();
        tracing::info!(
            backend = ?adapter_info.backend,
            device_name = %adapter_info.name,
            vendor = adapter_info.vendor,
            "windowed: GPU adapter selected"
        );

        // ── Step 4: Request device ────────────────────────────────────────────
        // Use downlevel_defaults() for broad compatibility but override
        // max_texture_dimension_2d with the adapter's actual capability.
        // downlevel_defaults() caps this at 2048 which is smaller than
        // common display resolutions (e.g. 2560x1440).  The adapter knows
        // the GPU's true limit; requesting it ensures the surface can be
        // configured at the monitor's native resolution.
        let mut required_limits = wgpu::Limits::downlevel_defaults();
        required_limits.max_texture_dimension_2d = adapter.limits().max_texture_dimension_2d;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("tze_hud_compositor_windowed"),
                    required_features: wgpu::Features::empty(),
                    required_limits,
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await
            .map_err(|e| CompositorError::DeviceCreation(e.to_string()))?;

        // ── Step 5: Configure the surface ────────────────────────────────────
        let surface_caps = surface.get_capabilities(&adapter);

        // Guard: wgpu surface capabilities must be non-empty on a valid
        // adapter/surface combination. Return a structured error instead of
        // panicking via index [0] so the caller can diagnose driver issues.
        if surface_caps.formats.is_empty() {
            return Err(CompositorError::DeviceCreation(
                "surface reports no supported texture formats — driver or backend issue"
                    .to_string(),
            ));
        }
        if surface_caps.present_modes.is_empty() {
            return Err(CompositorError::DeviceCreation(
                "surface reports no supported present modes — driver or backend issue".to_string(),
            ));
        }
        if surface_caps.alpha_modes.is_empty() {
            return Err(CompositorError::DeviceCreation(
                "surface reports no supported alpha modes — driver or backend issue".to_string(),
            ));
        }

        // Prefer sRGB surface format; fall back to the first available format.
        let surface_format = surface_caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(surface_caps.formats[0]);

        let present_mode = if surface_caps
            .present_modes
            .contains(&wgpu::PresentMode::Fifo)
        {
            wgpu::PresentMode::Fifo // vsync — latency-stable
        } else {
            surface_caps.present_modes[0]
        };

        // Clamp dimensions to the device's maximum supported texture size.
        // Use device.limits() (not adapter.limits()) because the device is
        // created with required_limits=downlevel_defaults(); the actual device
        // limits reflect what the adapter provides subject to those requirements.
        // Some GPUs (e.g. certain Intel/Mesa drivers) report a max of 2048,
        // which is smaller than common display resolutions like 2560x1440.
        //
        // Also guard against zero-size dimensions: wgpu panics if width or
        // height is 0 in surface.configure().  This can happen if inner_size()
        // returned (0,0) on a minimized or not-yet-shown window.
        let max_dim = device.limits().max_texture_dimension_2d;
        let clamped_width = width.min(max_dim).max(1);
        let clamped_height = height.min(max_dim).max(1);
        if clamped_width != width || clamped_height != height {
            tracing::warn!(
                requested_width = width,
                requested_height = height,
                clamped_width,
                clamped_height,
                max_texture_dimension_2d = max_dim,
                "windowed: surface dimensions clamped to device limit"
            );
        }

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: clamped_width,
            height: clamped_height,
            present_mode,
            alpha_mode: surface_caps
                .alpha_modes
                .iter()
                .find(|m| **m == wgpu::CompositeAlphaMode::PreMultiplied)
                .or_else(|| {
                    surface_caps
                        .alpha_modes
                        .iter()
                        .find(|m| **m == wgpu::CompositeAlphaMode::PostMultiplied)
                })
                .copied()
                .unwrap_or(surface_caps.alpha_modes[0]),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        tracing::info!(
            available_alpha_modes = ?surface_caps.alpha_modes,
            selected_alpha_mode = ?config.alpha_mode,
            "windowed: alpha mode selection"
        );
        surface.configure(&device, &config);
        tracing::info!(
            format = ?surface_format,
            present_mode = ?present_mode,
            alpha_mode = ?config.alpha_mode,
            width = clamped_width,
            height = clamped_height,
            "windowed: surface configured"
        );

        // ── Step 6: Create render pipelines (format-aware) ────────────────────
        let pipeline = Self::create_pipeline_with_format(&device, surface_format);
        let clear_pipeline = Self::create_clear_pipeline(&device, surface_format);
        let rounded_rect_pipeline = Self::create_rounded_rect_pipeline(&device, surface_format);
        let rounded_rect_overlay_pipeline =
            Self::create_rounded_rect_overlay_pipeline(&device, surface_format);

        let texture_rect_bind_group_layout = create_texture_rect_bind_group_layout(&device);
        let texture_rect_pipeline =
            create_texture_rect_pipeline(&device, &texture_rect_bind_group_layout, surface_format);
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_linear_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let compositor = Self {
            device,
            queue,
            adapter_info: CompositorAdapterInfo::from(adapter_info),
            pipeline,
            clear_pipeline,
            texture_rect_pipeline,
            texture_rect_bind_group_layout,
            image_sampler,
            rounded_rect_pipeline,
            rounded_rect_overlay_pipeline,
            width: clamped_width,
            height: clamped_height,
            frame_number: 0,
            retained_render_state: retained::RetainedRenderState::default(),
            latest_surface_recovery: None,
            overlay_mode: false,
            debug_zone_tints: std::env::var("TZE_HUD_DEBUG_ZONES").is_ok_and(|v| v == "1"),
            degradation_level: DegradationLevel::Nominal,
            degradation_policy: CompositorDegradationPolicy::default(),
            text_rasterizer: None,
            widget_renderer: None,
            resident_ledger: None,
            zone_animation_states: HashMap::new(),
            prev_active_zones: HashMap::new(),
            portal_tile_anim_states: HashMap::new(),
            prev_portal_tile_has_content: HashMap::new(),
            pub_animation_states: HashMap::new(),
            stream_reveal_states: HashMap::new(),
            portal_tile_reveal_states: HashMap::new(),
            // Windowed/live path animates scroll catch-up.
            scroll_smoothers: HashMap::new(),
            scroll_smoothing_enabled: true,
            last_scroll_smooth_at: None,
            token_map: HashMap::new(),
            markdown_primer: crate::markdown::MarkdownPrimer::new(),
            node_key_cache: HashMap::new(),
            markdown_cache_miss_count: std::cell::Cell::new(0),
            markdown_fallback_cache: std::cell::RefCell::new(HashMap::new()),
            markdown_tokens: crate::markdown::MarkdownTokens::default(),
            markdown_tokens_generic: crate::markdown::MarkdownTokens::default(),
            markdown_cache_scene_version: u64::MAX,
            markdown_cache_scene_instance: None,
            truncation_cache_scene_version: u64::MAX,
            truncation_cache_scene_instance: None,
            configured_max_truncation_input_bytes: None,
            resize_reprime_last_at: None,
            resize_reprime_content_bytes: 0,
            image_bytes: HashMap::new(),
            image_dims: HashMap::new(),
            image_texture_cache: HashMap::new(),
            failed_icon_paths: HashSet::new(),
            local_composer_state: Arc::new(StdMutex::new(None)),
            viewer_echo_queue: Arc::new(StdMutex::new(Vec::new())),
            viewer_echoes: viewer_echo::ViewerEchoStore::new(),
            composer_visual_layout: Arc::new(StdMutex::new(None)),
            viewer_echo_line_counts: HashMap::new(),
            viewer_echo_entry_line_counts: HashMap::new(),
            tile_flow_offsets: HashMap::new(),
            focus_ring_owner_state: Arc::new(StdMutex::new(None)),
            focus_ring_owner: None,
            resize_grip_hover_state: Arc::new(StdMutex::new(None)),
            resize_grip_hover: None,
            local_composer: None,
            composer_caret_blink_start: std::time::Instant::now(),
            composer_caret_rendered_phase: None,
            composer_layout: image_cache::ComposerLayout::default(),
        };

        let window_surface = WindowSurface::new(surface, config);
        Ok((compositor, window_surface))
    }

    /// Identity of the adapter selected when this compositor was created.
    pub fn adapter_info(&self) -> &CompositorAdapterInfo {
        &self.adapter_info
    }

    /// Fan out an actual window-surface lifecycle outcome without widening any
    /// scene, protocol, or agent-visible contract.
    ///
    /// A successfully reconfigured surface no longer has a trustworthy
    /// retained contents baseline. The next canonical headless observation is
    /// therefore forced through its structured full-surface diagnostic path;
    /// terminal outcomes retain the existing shutdown behavior and only remain
    /// available as compositor-private lifecycle evidence.
    pub(super) fn record_surface_recovery_outcome(&mut self, outcome: SurfaceRecoveryOutcome) {
        if let SurfaceRecoveryOutcome::Reconfigured { trigger } = outcome {
            tracing::debug!(
                ?trigger,
                "reconfigured window surface invalidated retained validation state"
            );
            self.retained_render_state.note_device_recovery();
        }
        self.latest_surface_recovery = Some(outcome);
    }

    /// Attempt a recovery transition queued by a real window-surface acquire
    /// failure.
    ///
    /// The compositor owns the `wgpu::Device`, so this is the only production
    /// seam that may turn a `Lost`/`Outdated` acquire result into a configured
    /// surface. The detailed outcome remains private to this crate; callers
    /// receive only the control result needed to repaint or terminate safely.
    pub fn attempt_pending_surface_recovery(
        &mut self,
        surface: &WindowSurface,
    ) -> WindowSurfaceRecoveryStatus {
        let Some(outcome) = surface.attempt_pending_recovery(&self.device) else {
            return WindowSurfaceRecoveryStatus::default();
        };

        let status = match outcome {
            SurfaceRecoveryOutcome::Reconfigured { trigger } => {
                let (width, height) = surface.size();
                self.width = width;
                self.height = height;
                tracing::info!(
                    ?trigger,
                    width,
                    height,
                    "window surface recovery reconfigured successfully"
                );
                WindowSurfaceRecoveryStatus::Reconfigured
            }
            SurfaceRecoveryOutcome::Terminal { trigger } => {
                tracing::error!(
                    ?trigger,
                    "window surface recovery reached a terminal failure"
                );
                WindowSurfaceRecoveryStatus::Terminal
            }
        };
        self.record_surface_recovery_outcome(outcome);
        status
    }

    /// Drain the detailed outcome from a real surface recovery transition.
    ///
    /// Retained diagnostics receive successful reconfiguration at the same
    /// private fan-out point. Draining this lifecycle evidence still avoids
    /// widening WAL, protobuf, or agent-visible APIs.
    #[allow(dead_code)] // Available for compositor-private lifecycle inspection.
    pub(crate) fn take_latest_surface_recovery(&mut self) -> Option<SurfaceRecoveryOutcome> {
        self.latest_surface_recovery.take()
    }

    /// Create a render pipeline targeting a specific texture format.
    ///
    /// This is the canonical pipeline constructor. Both `create_pipeline`
    /// (headless, fixed format) and `create_pipeline_with_format` (windowed,
    /// dynamic swapchain format) delegate here to avoid duplicating the
    /// pipeline descriptor.
    ///
    /// `label_prefix` is prepended to debug labels so GPU profilers can
    /// distinguish headless vs windowed pipelines.
    fn create_pipeline_inner(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        label_prefix: &str,
    ) -> wgpu::RenderPipeline {
        use crate::pipeline::{RECT_SHADER, RectVertex};

        let shader_label = format!("{label_prefix}rect_shader");
        let layout_label = format!("{label_prefix}rect_pipeline_layout");
        let pipeline_label = format!("{label_prefix}rect_pipeline");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&shader_label),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&layout_label),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(&pipeline_label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[RectVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    }

    /// Create a render pipeline targeting a dynamic swapchain format.
    ///
    /// Called by `new_windowed` so the pipeline matches the negotiated
    /// surface format (which varies by platform/driver).
    fn create_pipeline_with_format(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        Self::create_pipeline_inner(device, format, "windowed_")
    }

    /// Create a pipeline with no blending — writes RGBA directly.
    /// Used to clear the framebuffer to transparent in overlay mode.
    fn create_clear_pipeline(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        use crate::pipeline::{RECT_SHADER, RectVertex};
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("clear_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("clear_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("clear_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[RectVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None, // No blending — direct RGBA write
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    }

    /// Create a render pipeline for headless mode (`Rgba8UnormSrgb`).
    fn create_pipeline(device: &wgpu::Device) -> wgpu::RenderPipeline {
        Self::create_pipeline_inner(device, wgpu::TextureFormat::Rgba8UnormSrgb, "")
    }

    /// Create the SDF rounded-rectangle pipeline for the given output format.
    ///
    /// Uses `ROUNDED_RECT_SHADER` and `RoundedRectVertex::desc()`.
    /// Alpha blending is enabled (standard source-over) so rounded corners
    /// composite correctly over any background.
    ///
    /// This pipeline is used in `encode_rounded_rect_pass` for zones whose
    /// `RenderingPolicy` has `backdrop_radius` set.
    fn create_rounded_rect_pipeline(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rounded_rect_shader"),
            source: wgpu::ShaderSource::Wgsl(ROUNDED_RECT_SHADER.into()),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rounded_rect_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rounded_rect_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[RoundedRectVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    }

    /// Create the SDF rounded-rectangle pipeline for overlay / premultiplied-alpha mode.
    ///
    /// Uses `ROUNDED_RECT_OVERLAY_SHADER` (which scales all four RGBA channels by
    /// SDF coverage) and `BlendState::PREMULTIPLIED_ALPHA_BLENDING`.  Selected by
    /// `encode_rounded_rect_pass` when `self.overlay_mode` is true.
    ///
    /// In overlay mode vertex colors are premultiplied by `gpu_color`; the shader
    /// must therefore output `(rgb * cov, a * cov)` so the premultiplied blend
    /// equation applies coverage exactly once without double-multiplying alpha.
    fn create_rounded_rect_overlay_pipeline(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rounded_rect_overlay_shader"),
            source: wgpu::ShaderSource::Wgsl(ROUNDED_RECT_OVERLAY_SHADER.into()),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rounded_rect_overlay_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rounded_rect_overlay_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[RoundedRectVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        })
    }

    /// Replace the compositor's resolved design token map.
    ///
    /// Should be called once at startup with the global token map from scene
    /// startup.  The map is keyed by canonical
    /// token names (e.g. `"color.severity.warning"`) with hex-color string values
    /// (e.g. `"#FFB800"`).
    ///
    /// At render time the compositor looks up `color.severity.{info,warning,critical}`
    /// to derive alert-banner backdrop colors, falling back to hardcoded constants
    /// when a key is absent or unparseable.
    pub fn set_token_map(&mut self, map: HashMap<String, String>) {
        // Rebuild markdown tokens from the new map so heading weights, link
        // colors, and code family are in sync with the current theme.
        // Clear the markdown cache so existing entries are re-parsed with the
        // new token values on the next prime() call.
        self.markdown_tokens =
            crate::markdown::MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Portal);
        // Resolve the non-portal (generic) scope too so per-tile token selection
        // is correct the moment a non-portal markdown surface is introduced
        // (hud-3ryie).  In v1 this set is never selected (the portal transcript
        // is the sole governed markdown surface).
        self.markdown_tokens_generic =
            crate::markdown::MarkdownTokens::from_token_map_scoped(&map, MarkdownScope::Generic);
        self.markdown_primer.reset();
        // Clear the per-node key cache: it will be repopulated by the next
        // prime_markdown_cache call once the version sentinel fires.
        self.node_key_cache.clear();
        // Reset the version sentinel so the next prime_markdown_cache call
        // re-parses all nodes with the updated tokens, even if scene.version
        // has not changed since the last prime.
        self.markdown_cache_scene_version = u64::MAX;
        self.markdown_cache_scene_instance = None;

        // Clear the truncation cache: a token-map change can alter font metrics
        // (monospace vs. sans-serif substitution, heading-scale changes) which
        // affects the shaped width and thus the truncation point.  Re-prime
        // unconditionally on the next frame.
        if let Some(rasterizer) = &mut self.text_rasterizer {
            rasterizer.truncation_cache.clear();
        }
        self.truncation_cache_scene_version = u64::MAX;
        self.truncation_cache_scene_instance = None;

        self.token_map = map;
        // Token-substitution failures in ensure_icon_texture are tied to the
        // token map state, not to the SVG file on disk.  Clearing the negative
        // cache here lets icons that previously failed due to a missing token
        // be retried with the updated map (e.g. on hot-reload or late init).
        self.failed_icon_paths.clear();
    }

    /// Drain any pending [`LocalComposerState`] update from the shared handle.
    ///
    /// Called once per frame at the top of the compositor render loop.
    ///
    /// - `Some(Some(state))` in the slot → update `self.local_composer`.
    /// - `Some(None)` in the slot → clear `self.local_composer` (composer
    ///   deactivated by blur / submit / cancel).
    /// - `None` in the slot → no update; keep the current frame state so the
    ///   overlay persists between keystrokes.
    ///
    /// Taking the value (`.take()`) resets the slot to `None` so the
    /// same update is not applied twice.
    ///
    /// Returns `true` when a new draft (keystroke / caret move) was applied.
    pub fn drain_local_composer_state(&mut self) -> bool {
        let new_draft = apply_composer_slot(&self.local_composer_state, &mut self.local_composer);
        if new_draft {
            // Reset the blink phase so the caret is solid right after typing or
            // moving the caret (standard editor behavior).
            self.composer_caret_blink_start = std::time::Instant::now();
        }
        // Latest-wins read of the focus-ring owner (hud-k6yvb): a plain overwrite,
        // so a momentary lock contention simply keeps the prior owner this frame.
        if let Ok(slot) = self.focus_ring_owner_state.lock() {
            self.focus_ring_owner = *slot;
        }
        // Latest-wins read of the resize-grip hover target (hud-wgiys): the same
        // plain overwrite as the focus-ring owner, so momentary lock contention
        // simply keeps the prior hover state for this frame.
        if let Ok(slot) = self.resize_grip_hover_state.lock() {
            self.resize_grip_hover = *slot;
        }
        new_draft
    }

    /// Drain the local composer echo slot and report whether the idle render
    /// gate (hud-ilivg) must render this frame on the composer's behalf.
    ///
    /// The windowed frame loop calls this **before** the gate decides whether to
    /// skip the build/encode/present pass. Two composer effects are driven purely
    /// off out-of-band state that never bumps `scene.version`, so the gate would
    /// otherwise freeze them (hud-r3ax6, "Local feedback first"):
    ///
    /// 1. **Local draft echo.** A keystroke writes the `local_composer_state`
    ///    slot; the draft is only applied by draining it (the `local_composer`
    ///    field is populated *here*, not by the scene diff). Draining before the
    ///    gate is what makes a pending echo observable to the gate — otherwise it
    ///    would only ever be drained inside `render_frame`, which the gate may
    ///    skip (chicken-and-egg).
    /// 2. **Caret blink.** While a composer is focused the caret toggles off the
    ///    wall clock ([`caret_visible_at`]). The gate is dirty only when the
    ///    phase differs from the last one it reported, so an idle focused
    ///    composer renders once per toggle (see
    ///    [`Compositor::next_animation_deadline`], which schedules the wake), not
    ///    every frame.
    ///
    /// Returns `true` for a pending echo, a caret-phase toggle, a composer
    /// activating, and the single frame on which it deactivates (so the overlay
    /// is cleared from the screen); `false` otherwise, including an idle focused
    /// composer between toggles.
    ///
    /// [`caret_visible_at`]: image_cache::caret_visible_at
    pub fn drain_local_composer_and_needs_render(&mut self) -> bool {
        let was_active = self.local_composer.is_some();
        let previous_focus_ring_owner = self.focus_ring_owner;
        let previous_resize_grip_hover = self.resize_grip_hover;
        let draft_applied = self.drain_local_composer_state();
        let is_active = self.local_composer.is_some();
        // The caret only changes pixels when its phase flips; between toggles an
        // idle focused composer costs nothing (`next_animation_deadline` wakes us
        // at the next toggle).
        let caret_visible = is_active
            .then(|| image_cache::caret_visible_at(self.composer_caret_blink_start.elapsed()));
        let caret_toggled = caret_visible != self.composer_caret_rendered_phase;
        self.composer_caret_rendered_phase = caret_visible;
        // `draft_applied` → pending echo / caret move.
        // `was_active != is_active` → activation, and the deactivation frame that
        //                clears the overlay drawn last frame.
        draft_applied
            || was_active != is_active
            || caret_toggled
            || focus_or_grip_changed(
                previous_focus_ring_owner,
                self.focus_ring_owner,
                previous_resize_grip_hover,
                self.resize_grip_hover,
            )
    }

    /// Prime the markdown parse cache for all [`TextMarkdownNode`] nodes in the scene.
    ///
    /// **Call site**: this must be called by the **runtime at Stage 4 (scene
    /// commit time)**, before `render_frame` or `render_frame_headless` is
    /// invoked.  Callers are responsible for measuring the cost and recording it
    /// as `FrameTelemetry::markdown_prime_us` (hud-380dl, Option A).
    ///
    /// The method is gated internally on `scene.version` so repeated calls on
    /// unchanged scenes are no-ops (constant-time, no allocation).
    ///
    /// After this call returns, the frame pipeline can consume cached results via
    /// [`MarkdownCache::get_by_key`] with zero parse cost — satisfying the
    /// "parse-on-commit, zero per-frame parse cost" contract (hud-5jbra.2,
    /// hud-380dl).
    ///
    /// The method also evicts cache entries for content no longer referenced by
    /// any scene node, keeping the cache bounded to the live node set.
    ///
    /// [`TextMarkdownNode`]: tze_hud_scene::types::TextMarkdownNode
    /// Cumulative per-frame text `shape_until_scroll` count (hud-991cj).
    ///
    /// Zero when no text renderer is initialised. Used by the scroll-reshape
    /// benchmark/regression to prove that a pure scroll performs no re-shapes.
    #[allow(dead_code)] // used by the hud-991cj scroll-reshape benchmark
    pub(crate) fn text_shape_call_count(&self) -> u64 {
        self.text_rasterizer
            .as_ref()
            .map(|r| r.shape_call_count())
            .unwrap_or(0)
    }

    /// Cumulative markdown-cache-miss count on the render path (hud-u4lq2). See
    /// [`Self::markdown_cache_miss_count`]'s field doc for what a nonzero,
    /// growing count means. Currently consumed only by
    /// `renderer::tests::markdown_primer::reused_compositor_across_scenes_lands_markdown_primer_hud_u4lq2`
    /// and its sibling baseline test; `#[allow(dead_code)]` covers the non-test
    /// build (the field itself keeps incrementing regardless, for log-visible
    /// `tracing::warn!` observability at the call site in `text.rs`).
    #[allow(dead_code)]
    pub(crate) fn markdown_cache_miss_count(&self) -> u64 {
        self.markdown_cache_miss_count.get()
    }

    pub fn prime_markdown_cache(&mut self, scene: &SceneGraph) {
        use tze_hud_scene::types::NodeData;

        // Skip if the scene has not changed since we last primed. Checks the
        // scene INSTANCE identity alongside the version number (hud-u4lq2):
        // `version` restarts at 0 for every newly constructed `SceneGraph`, so
        // comparing it alone cannot tell "the same scene, unchanged" apart from
        // "a different scene instance that happens to have reached the same
        // version number" (a `Compositor` reused across independently
        // constructed `SceneGraph`s — e.g. scenario-style benchmarks/tests —
        // hits this). A mismatched instance always forces a re-prime.
        if self.markdown_cache_scene_instance == Some(scene.instance_id)
            && scene.version == self.markdown_cache_scene_version
        {
            return;
        }
        self.markdown_cache_scene_version = scene.version;
        self.markdown_cache_scene_instance = Some(scene.instance_id);
        // A fresh commit-time prime is about to rebuild the authoritative
        // snapshot for every live node — any entries parked in the fallback
        // self-heal cache (hud-u4lq2) are superseded now, so drop them rather
        // than let them shadow stale content or grow unbounded.
        self.markdown_fallback_cache.borrow_mut().clear();

        // Snapshot the current cache once so we can decide, per node, whether
        // its content still needs parsing.  Already-cached entries are carried
        // forward by the primer without re-cloning their source string
        // (hud-33qo7); only cache misses ship their `Arc<str>` content.
        let current = self.markdown_primer.load();

        // Build the complete live job set and repopulate the per-node key cache
        // (hud-gpqde) in the same pass — eliminating per-frame re-hashes in
        // collect_text_items_from_node.
        let mut jobs: Vec<crate::markdown::PrimeJob> = Vec::new();

        // Rebuild the node key cache for this scene version.
        self.node_key_cache.clear();

        // Classify markdown nodes by scope (hud-3ryie): a node under a governed
        // portal surface (a tile with a scroll config) resolves the portal token
        // set; every other markdown node resolves the generic set.  The chosen
        // token identity is folded into the cache key, so the two scopes never
        // collide on identical content.  In v1 every markdown node lives under a
        // portal tile, so `generic` is unused and behavior is identical to before
        // per-tile scoping.
        let portal_node_ids = portal_markdown_node_ids(scene);
        // One `Arc` per scope, cloned by refcount bump onto each job.
        let portal_tokens = std::sync::Arc::new(self.markdown_tokens.clone());
        let generic_tokens = std::sync::Arc::new(self.markdown_tokens_generic.clone());

        for (node_id, node) in scene.nodes.iter() {
            if let NodeData::TextMarkdown(tm) = &node.data {
                let tokens = if portal_node_ids.contains(node_id) {
                    &portal_tokens
                } else {
                    &generic_tokens
                };
                let key = crate::markdown::MarkdownCache::compute_key(&tm.content, tokens);
                // Store the precomputed key so the frame path can look it up
                // in O(1) without re-hashing content.
                self.node_key_cache.insert(*node_id, key);
                // Attach content only for cache misses; hits carry forward by
                // key with no source clone.
                let content = if current.get_by_key(&key).is_none() {
                    Some(std::sync::Arc::<str>::from(tm.content.as_str()))
                } else {
                    None
                };
                jobs.push(crate::markdown::PrimeJob {
                    key,
                    content,
                    tokens: std::sync::Arc::clone(tokens),
                });
            }
        }

        // Hand the complete live set to the primer.  It parses any missing
        // content (inline for small payloads, on the background thread for large
        // ones) with each job's own scoped tokens, atomically swaps in the new
        // snapshot, and evicts dead entries implicitly (the new snapshot is
        // exactly the live set).  Ordering against a late background result is
        // handled by the primer's own internal epoch (hud-u4lq2) — it no longer
        // takes `scene.version`, which is only monotonic within one `SceneGraph`
        // instance's lifetime and previously caused legitimate results to be
        // silently dropped when a `Compositor` was reused across scenes.
        self.markdown_primer.prime(jobs);
    }

    /// Load the current markdown parse-cache snapshot lock-free (hud-33qo7).
    ///
    /// Render-path callers use this to read [`ParsedMarkdown`] via
    /// [`MarkdownCache::get_by_key`].  The returned [`Arc`] pins the snapshot for
    /// the duration of the borrow, so a concurrent background swap cannot free it
    /// mid-read.  Hold it across a traversal (it is a cheap atomic load) rather
    /// than re-loading per node.
    ///
    /// [`MarkdownCache::get_by_key`]: crate::markdown::MarkdownCache::get_by_key
    /// [`ParsedMarkdown`]: crate::markdown::ParsedMarkdown
    #[inline]
    pub(crate) fn markdown_cache(&self) -> std::sync::Arc<crate::markdown::MarkdownCache> {
        self.markdown_primer.load()
    }

    /// Prime the ellipsis-truncation cache for all `TextOverflow::Ellipsis` items
    /// currently in the scene (hud-wgq7j, hud-ghhxa).
    ///
    /// Must be called **at content-commit time** (when `scene.version` changes),
    /// never on the per-frame render path.  The method is gated internally on
    /// `scene.version` so repeated calls on unchanged scenes are no-ops.
    ///
    /// After this call, `prepare_text_items` resolves each Ellipsis item in O(1)
    /// via `TruncationCache::get_by_key` — satisfying the "stable-shape-caching"
    /// contract from RFC 0013 §3.4 and §4.2, Phase-1 design §3.
    ///
    /// # Mid-drag cadence (hud-ghhxa — spec §6b.3)
    ///
    /// When a portal tile is resized (hotkey or pointer drag), `scene.version`
    /// increments on every geometry change.  Because `truncate_line_to_ellipsis`
    /// is O(n) in content length, re-priming every frame during a fast drag
    /// would blow the Stage 5 / Stage 6 frame budget on large content.
    ///
    /// The adaptive cadence gate caps re-primes to at most once per the
    /// content-length-derived interval (see [`adaptive_reprime_interval_ms`])
    /// while guaranteeing that every distinct intermediate geometry is
    /// *eventually* reflected — not only at drag-end.  When the geometry
    /// settles (scene.version stops changing), the last geometry is primed on
    /// the next frame after the interval elapses.
    pub fn prime_truncation_cache(&mut self, scene: &SceneGraph) {
        // Skip if the scene has not changed since we last primed. Checks the
        // scene INSTANCE identity alongside the version number (hud-u4lq2) —
        // see `Compositor::markdown_cache_scene_instance`'s doc for why
        // `scene.version` alone is not a reliable "unchanged" signal across
        // independently-constructed `SceneGraph`s sharing one `Compositor`.
        let same_instance = self.truncation_cache_scene_instance == Some(scene.instance_id);
        if same_instance && scene.version == self.truncation_cache_scene_version {
            return;
        }

        // ── Adaptive mid-drag cadence gate (hud-ghhxa, hud-3to8i) ───────────
        // When geometry changes rapidly (bounds updates from resize), cap the
        // re-prime rate to an adaptive interval derived from the total byte
        // count of Ellipsis text content visible in the scene.  This avoids
        // per-frame O(n) shaping cost on large content while keeping truncation
        // visually responsive for short/cheap content.
        //
        // Strategy: if a prime ran within the adaptive interval, defer — leave
        // the sentinel unchanged so the next frame will retry.  When the
        // interval has elapsed (or this is the very first prime), proceed.
        //
        // The adaptive interval is derived from `resize_reprime_content_bytes`,
        // which holds the total content bytes from the *last* successful prime.
        // Using the cached value keeps this check O(1) with no scene traversal
        // on deferred frames (frame-loop contract: the adaptive decision itself
        // must be cheap).
        //
        // This guarantees:
        //  a) The first geometry change in each interval triggers an immediate
        //     re-prime (the TruncationKey includes bounds, so the new geometry
        //     will be primed on first visit).
        //  b) Intermediate geometries *between* interval ticks are not orphaned:
        //     the sentinel is not updated on a defer, so the most-recent
        //     scene.version is always picked up on the next allowed tick.
        //  c) At drag-end (geometry settles), the final geometry is primed on the
        //     next tick after the interval elapses — completing the re-resolution
        //     required by spec §6b.3.
        //
        // The cadence gate is bypassed when the sentinel is `u64::MAX` (a forced
        // re-prime requested by `set_token_map` or initialisation) so that token/
        // font-metric changes are always reflected immediately regardless of resize
        // cadence. Also bypassed on a scene-instance mismatch (hud-u4lq2): a
        // brand-new `SceneGraph` handed to a reused `Compositor` must never be
        // treated as "mid-drag on the same scene" just because its version
        // number happens to fall inside the debounce window.
        let forced_prime = self.truncation_cache_scene_version == u64::MAX || !same_instance;
        let interval_ms = adaptive_reprime_interval_ms(self.resize_reprime_content_bytes);
        if !forced_prime && should_defer_reprime(self.resize_reprime_last_at, interval_ms) {
            // Within the debounce window: defer, do not update the sentinel.
            tracing::trace!(
                scene_version = scene.version,
                interval_ms,
                content_bytes = self.resize_reprime_content_bytes,
                "prime_truncation_cache: mid-drag cadence defer (within adaptive interval)"
            );
            return;
        }

        // Build the set of TextItems with Ellipsis overflow that are currently
        // reachable in the scene.  We need TextItem geometry, which is the same
        // geometry produced by `collect_text_items_from_node`.  Rather than
        // duplicating that traversal here, we ask each tile's node tree for its
        // Ellipsis TextMarkdownNodes and reconstruct the same geometry that
        // `collect_text_items_from_node` would compute.
        //
        // NOTE: the scene-version sentinel is updated only after confirming the
        // rasterizer is available.  If we returned early here (rasterizer = None)
        // while recording the version, subsequent frames would skip priming even
        // once the rasterizer is initialized — until the scene changes again.
        if self.text_rasterizer.is_none() {
            return; // text renderer not yet initialized; retry next version change
        }

        // Zone StreamText items are not produced by the tile-node walk below, so
        // prime them separately.  Without this, an overflowing streaming zone
        // would hit the render path with no cache entry and fall back to raw
        // clipping (head-anchored), never showing the tail (hud-gxz0x).  Zone
        // geometry resolves against the scene's display area — kept in sync with
        // the surface size (windowed.rs::sync_scene_display_area), so the primed
        // geometry matches what `collect_text_items` produces on the frame path.
        //
        // Computed here (before the &mut self.text_rasterizer borrow below) so
        // the &self collector does not conflict with that borrow.
        let zone_stream_items = self.collect_zone_stream_text_ellipsis_items(
            scene,
            scene.display_area.width,
            scene.display_area.height,
        );

        // Record the version and timestamp now that priming will proceed.
        self.truncation_cache_scene_version = scene.version;
        self.truncation_cache_scene_instance = Some(scene.instance_id);
        self.resize_reprime_last_at = Some(std::time::Instant::now());

        // Resolve the resize font clamp range BEFORE the &mut text_rasterizer
        // borrow below (hud-ovjxu.1) — the ellipsis collector applies the same
        // viewer-local font scaling as the render path so truncation keys match.
        let font_clamp = self.portal_font_clamp_range();

        // Transcript optimal-measure cap (hud-rivcy): resolve once and apply the
        // SAME clamp the render path (`collect_text_items_from_node`) applies, so
        // the primed truncation key (which includes `bounds_width`) matches what
        // gets shaped. Gated per-tile to portal surfaces below.
        let transcript_max_measure_px = resolve_transcript_max_measure_px(&self.token_map);

        let policy_visible_tiles = self.policy_visible_tiles(scene);

        // Safe: checked is_none() above and returned; rasterizer stays Some
        // (this method holds &mut self exclusively).
        let rasterizer = self
            .text_rasterizer
            .as_mut()
            .expect("text rasterizer presence checked above");

        // Load the markdown snapshot once (lock-free atomic load) and pin it for
        // the whole traversal — a concurrent background swap cannot free it
        // mid-read (hud-33qo7).
        let markdown_cache = self.markdown_primer.load();
        let mut live_items: Vec<crate::text::TextItem> = Vec::new();
        for tile in policy_visible_tiles {
            let tile_x = tile.bounds.x;
            let tile_y = tile.bounds.y;
            // Determine whether this tile is currently following its tail.
            // Use the shared helper so this computation is identical to
            // collect_text_items_from_node (hud-plz8q / hud-lu50e).
            let at_tail = tile_at_tail_for_ellipsis(tile.id, scene);
            // Per-tile token scope (hud-3ryie): a portal surface (scrollable
            // tile) resolves the portal token set; any other markdown surface
            // resolves the generic set.  The whole subtree shares the tile's
            // scope, so select once and thread it down the recursion.
            let is_portal_surface = scene.tile_scroll_config(tile.id).is_some();
            let md_tokens = if is_portal_surface {
                &self.markdown_tokens
            } else {
                &self.markdown_tokens_generic
            };
            // Only portal surfaces are transcripts; pass 0.0 (no clamp) for
            // other markdown tiles so the collector is a no-op there. Mirrors
            // the render-path gate in `collect_text_items`.
            let transcript_measure_px = if is_portal_surface {
                transcript_max_measure_px
            } else {
                0.0
            };
            if let Some(root_id) = tile.root_node {
                // Per-part portal-surface consumption (hud-s4lrw): mirror the
                // render path so the primed truncation keys (measure + viewport)
                // match. `None` → legacy tile-level behavior.
                let part_index = portal_part_index(scene, tile.id);
                collect_ellipsis_text_items_from_node(
                    root_id,
                    scene,
                    tile.id,
                    tile_x,
                    tile_y,
                    at_tail,
                    &markdown_cache,
                    &self.node_key_cache,
                    md_tokens,
                    font_clamp,
                    transcript_measure_px,
                    part_index,
                    None,
                    &mut live_items,
                );
            }
        }

        // Append the zone StreamText Ellipsis items computed before the
        // rasterizer borrow (see comment above) so they are primed alongside the
        // tile-node items.
        live_items.extend(zone_stream_items);

        // Update the cached content byte count for the adaptive cadence gate on
        // the *next* call.  Summing item lengths is O(n) in item count here, but
        // this only runs when a prime is allowed (not on every deferred frame),
        // so it stays within the prime-time O(n) budget.
        self.resize_reprime_content_bytes = live_items.iter().map(|item| item.text.len()).sum();

        let mut live_keys = rasterizer.prime_truncation_cache(&live_items);

        // Evict stale truncation entries for nodes/geometry no longer in the scene.
        //
        // Deduplicate live_keys before the length comparison: duplicate scene
        // items (same content + geometry) produce the same TruncationKey.
        // Without dedup, `live_keys.len()` may exceed the number of distinct
        // cache entries, causing the `len > live_keys.len()` condition to be
        // false even when stale entries exist — a memory leak.
        live_keys.sort_unstable();
        live_keys.dedup();
        if rasterizer.truncation_cache.len() > live_keys.len() {
            rasterizer.truncation_cache.evict_except(&live_keys);
        }
    }

    /// Scan the scene graph for all StaticImage resources, ensure their GPU
    /// textures are uploaded, and return the set of referenced `ResourceId`s.
    ///
    /// This method uploads textures for tile-backed static images directly,
    /// and for zone-publication images when dimensions can be inferred from
    /// registered RGBA bytes.
    ///
    /// The returned `HashSet<ResourceId>` should be passed to
    /// [`Compositor::evict_unused_image_textures`] once per frame so that
    /// stale cache entries are reclaimed and the cache does not grow without
    /// bound across frames.
    fn scene_image_resource_ids(scene: &SceneGraph) -> HashSet<ResourceId> {
        let mut referenced = HashSet::new();
        for node in scene.nodes.values() {
            if let NodeData::StaticImage(img) = &node.data {
                referenced.insert(img.resource_id);
            }
        }
        for publishes in scene.zone_registry.active_publishes.values() {
            for record in publishes {
                match &record.content {
                    ZoneContent::StaticImage(resource_id) => {
                        referenced.insert(*resource_id);
                    }
                    ZoneContent::Notification(payload) if !payload.icon.is_empty() => {
                        if let Some(resource_id) = parse_notification_icon(&payload.icon) {
                            referenced.insert(resource_id);
                        }
                    }
                    _ => {}
                }
            }
        }
        referenced
    }

    fn ensure_scene_image_textures(&mut self, scene: &SceneGraph) -> HashSet<ResourceId> {
        let mut referenced: HashSet<ResourceId> = HashSet::new();

        // Collect all referenced StaticImage resource IDs from tiles.
        for node in scene.nodes.values() {
            if let NodeData::StaticImage(img) = &node.data {
                self.ensure_image_texture(img.resource_id, img.width, img.height);
                referenced.insert(img.resource_id);
            }
        }

        // Collect from zone publications.
        for publishes in scene.zone_registry.active_publishes.values() {
            for record in publishes {
                match &record.content {
                    ZoneContent::StaticImage(resource_id) => {
                        referenced.insert(*resource_id);
                        // Use the explicit dimensions stored by `register_image_bytes`.
                        // This correctly handles non-square images (e.g. 640×360) that
                        // the old square-root heuristic would mis-detect.
                        if !self.image_texture_cache.contains_key(resource_id) {
                            if let Some(&(w, h)) = self.image_dims.get(resource_id) {
                                if w > 0 && h > 0 {
                                    self.ensure_image_texture(*resource_id, w, h);
                                }
                            }
                        }
                    }
                    ZoneContent::Notification(payload) if !payload.icon.is_empty() => {
                        // Parse the icon field via the shared helper (handles empty string
                        // and invalid hex with graceful None). Non-parseable icons are
                        // silently skipped (render notification text-only).
                        if let Some(resource_id) = parse_notification_icon(&payload.icon) {
                            referenced.insert(resource_id);
                            // Use the explicit dimensions stored by `register_image_bytes`.
                            if !self.image_texture_cache.contains_key(&resource_id) {
                                if let Some(&(w, h)) = self.image_dims.get(&resource_id) {
                                    if w > 0 && h > 0 {
                                        self.ensure_image_texture(resource_id, w, h);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        referenced
    }

    /// Scan the scene's zone registry for all `key_icon_map` SVG paths,
    /// ensure their GPU textures are cached, and return the set of their
    /// path-derived `ResourceId`s.
    ///
    /// The returned set must be merged into the `referenced_ids` passed to
    /// `evict_unused_image_textures` so that icon textures are not evicted
    /// on frames where no matching StaticImage publication is active.
    ///
    /// Called once per frame before `render_zone_content` so that the
    /// immutable render path can look up `image_texture_cache` by ResourceId.
    ///
    /// SVG rasterization only occurs on cache miss (init-time / first-seen
    /// path).  Subsequent calls are O(1) per path due to the cache hit-check
    /// guard in `ensure_icon_texture`.
    fn scene_icon_resource_ids(scene: &SceneGraph) -> HashSet<ResourceId> {
        scene
            .zone_registry
            .zones
            .values()
            .flat_map(|zone| zone.rendering_policy.key_icon_map.values())
            .map(|svg_path| ResourceId::of(svg_path.as_bytes()))
            .collect()
    }

    fn ensure_scene_icon_textures(&mut self, scene: &SceneGraph) -> HashSet<ResourceId> {
        let mut icon_ids: HashSet<ResourceId> = HashSet::new();
        for zone_def in scene.zone_registry.zones.values() {
            let icon_map = &zone_def.rendering_policy.key_icon_map;
            if icon_map.is_empty() {
                continue;
            }
            for svg_path in icon_map.values() {
                let id = ResourceId::of(svg_path.as_bytes());
                icon_ids.insert(id);
                // ensure_icon_texture is a no-op on cache hit.
                self.ensure_icon_texture(svg_path);
            }
        }
        icon_ids
    }

    /// Shared encode pipeline used by `render_frame` and `render_frame_headless`.
    ///
    /// Encodes the geometry pass (clear + vertex draw) and the text pass into a
    /// single `CommandEncoder`.  The encoder is returned to the caller **before**
    /// `queue.submit` so that headless callers can append a `copy_to_buffer`
    /// command (which must precede submit).
    ///
    /// # Parameters
    ///
    /// - `vertices` — pre-built vertex list (caller is responsible for overlay
    ///   quads, zone content, etc.)
    /// - `frame_view` — render target view for this frame
    /// - `scene` — used only for the text pass (`collect_text_items`)
    /// - `surf_w` / `surf_h` — surface dimensions used for the text viewport
    /// - `use_overlay_pipeline` — when `true`, selects `clear_pipeline` (no
    ///   blending) instead of the standard `pipeline`
    ///
    /// # Returns
    ///
    /// `(encoder, encode_us)` — the ready-to-submit encoder and the wall-clock
    /// microseconds spent encoding (for `FrameTelemetry::stage6_render_encode_us`).
    /// `bg_vertex_count`: number of vertices at the start of `vertices` that
    /// make up the initial Background slice.  This includes any overlay-mode
    /// clear quad prepended before Background zone vertices, so in overlay
    /// mode callers must include those 6 clear-quad vertices in the count
    /// even when Background zones emit no flat-rect vertices.  The
    /// Background SDF rounded-rect pass is inserted between this initial
    /// slice and the remaining tile/content/chrome vertices so that
    /// Background backdrops are correctly drawn BELOW agent tiles.  Pass `0`
    /// only when the initial Background slice is empty (i.e. no Background
    /// flat-rect vertices and no overlay-mode clear quad); the Background
    /// SDF pass still runs before the tile/content/chrome pass.
    // All arguments serve distinct roles in the GPU encode pass: vertex data,
    // frame target, scene snapshot, surface dimensions, pipeline selector, and
    // the Background vertex split index.  No natural grouping exists that would
    // reduce the count without creating an ad-hoc context struct.
    #[allow(clippy::too_many_arguments)]
    /// Perform every scene read the GPU encode stage needs, returning an owned
    /// [`EncodeInputs`] (hud-uyhpn).
    ///
    /// This is the scene-read half of the former monolithic `encode_frame`:
    /// rounded-rect command collection, composer/viewer-echo layout priming,
    /// text-item collection, and the glyphon prepare (Phase A/B). It borrows
    /// `&SceneGraph` and mutates `self` (rasterizer / caches) but never touches
    /// the swapchain surface, so it can run entirely under the scene lock and
    /// BEFORE the vsync-blocking `acquire_frame()`.
    ///
    /// The matching encode half is [`Compositor::encode_from_inputs`], which is
    /// scene-free and runs after the lock is dropped.
    fn collect_encode_inputs(
        &mut self,
        scene: &SceneGraph,
        surf_w: u32,
        surf_h: u32,
    ) -> EncodeInputs {
        let sw = surf_w as f32;
        let sh = surf_h as f32;

        // ── Single-pass partition of all rounded-rect commands ────────────────
        let rr_all = self.collect_all_rounded_rect_cmds(scene, sw, sh);
        let rr_background = rr_all.background;
        let mut rr_post: Vec<crate::pipeline::RoundedRectDrawCmd> = Vec::new();
        rr_post.extend(rr_all.content);
        rr_post.extend(rr_all.chrome);
        rr_post.extend(self.collect_tile_rounded_rect_cmds(scene));

        // ── Text pass inputs (Stage 6) ────────────────────────────────────────
        // The shared frame builder primes composer / viewer-echo layout before
        // emitting any flat geometry, so text consumes the same resolved layout
        // rather than reflowing after the composer chrome has been staged.

        let has_text_rasterizer = self.text_rasterizer.is_some();
        let text_items: Vec<TextItem> = if has_text_rasterizer {
            self.collect_text_items(scene, sw, sh)
        } else {
            vec![]
        };
        // Phase A: prepare glyphon buffers (requires mutable tr borrow). Drop the
        // borrow immediately after so Phase B can call self.gpu_color_raw.
        //
        // When text_items is empty we return None — not Some(Ok([])) — so the
        // encode stage skips render_text_pass entirely. Replaying glyphon's
        // previously-prepared TextAreas from the last non-empty frame would leave
        // stale text on screen.
        let prepare_result: Option<Result<Vec<crate::text::InlineBackdropQuad>, String>> =
            if let Some(ref mut tr) = self.text_rasterizer {
                tr.update_viewport(&self.queue, surf_w, surf_h);
                if text_items.is_empty() {
                    None
                } else {
                    Some(tr.prepare_text_items(&self.device, &self.queue, &text_items))
                }
            } else {
                None
            };
        // Phase B: convert inline backdrop quads to GPU vertices. `self` is no
        // longer mutably borrowed here, so gpu_color_raw is available.
        let inline_verts: Vec<crate::pipeline::RectVertex> =
            if let Some(Ok(ref inline_quads)) = prepare_result {
                inline_quads
                    .iter()
                    .flat_map(|q| {
                        let color = self.gpu_color_raw([
                            q.color[0] as f32 / 255.0,
                            q.color[1] as f32 / 255.0,
                            q.color[2] as f32 / 255.0,
                            q.color[3] as f32 / 255.0,
                        ]);
                        rect_vertices(q.x, q.y, q.w, q.h, sw, sh, color)
                    })
                    .collect()
            } else {
                vec![]
            };
        // Determine whether the encode stage should replay the text pass. On a
        // prepare error we warn HERE (once) and skip — matching the pre-split
        // warn site's effect exactly.
        let render_text = match &prepare_result {
            Some(Ok(_)) => true,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "text prepare failed — frame continues without text");
                false
            }
            None => false,
        };

        EncodeInputs {
            rr_background,
            rr_post,
            inline_verts,
            render_text,
        }
    }

    /// Encode the GPU command buffer from precomputed, scene-free
    /// [`EncodeInputs`] (hud-uyhpn).
    ///
    /// This is the surface-touching half of the former monolithic `encode_frame`.
    /// It reads only the acquired surface `frame_view` and the owned `inputs`
    /// (plus GPU-owned state like the text-rasterizer atlas) — never `&SceneGraph`
    /// — so the windowed frame loop runs it AFTER dropping the scene lock.
    ///
    /// `render_text` gating and the always-run `trim_atlas()` preserve the exact
    /// glyph-eviction semantics of the pre-split code.
    #[allow(clippy::too_many_arguments)]
    fn encode_from_inputs(
        &mut self,
        vertices: &[RectVertex],
        frame_view: &wgpu::TextureView,
        inputs: &EncodeInputs,
        surf_w: u32,
        surf_h: u32,
        use_overlay_pipeline: bool,
        bg_vertex_count: usize,
    ) -> (wgpu::CommandEncoder, u64) {
        let encode_start = std::time::Instant::now();

        // Upload the full vertex buffer once — individual passes render
        // different vertex sub-ranges from it using draw ranges.
        let vertex_buffer = if vertices.is_empty() {
            None
        } else {
            let buffer = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex_buffer"),
                    contents: bytemuck::cast_slice(vertices),
                    usage: wgpu::BufferUsages::VERTEX,
                });
            Some(buffer)
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame_encoder"),
            });

        let sw = surf_w as f32;
        let sh = surf_h as f32;

        // Both flat-rect geometry sub-passes use the same pipeline selector:
        // in overlay mode, the clear_pipeline (no blending) is used for ALL
        // flat-rect rendering. The first 6 vertices are a full-screen
        // transparent quad that zeros out every pixel's alpha. Subsequent
        // content overwrites specific regions with their own alpha.

        // ── Geometry pass 1: Background flat-rect zones ───────────────────────
        // Clears the surface and draws Background-layer zone backdrops (those
        // without backdrop_radius; zones with backdrop_radius emit no vertices
        // here).  Tile/content/chrome vertices are deferred to pass 2 so that
        // the Background SDF pass can be interleaved between the two.
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame_pass_bg"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear_color()),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if use_overlay_pipeline || self.use_opaque_rect_pipeline() {
                render_pass.set_pipeline(&self.clear_pipeline);
            } else {
                render_pass.set_pipeline(&self.pipeline);
            }

            if let Some(ref buffer) = vertex_buffer {
                let bg_end = bg_vertex_count.min(vertices.len());
                if bg_end > 0 {
                    render_pass.set_vertex_buffer(0, buffer.slice(..));
                    render_pass.draw(0..bg_end as u32, 0..1);
                }
            }
        }

        // ── Background SDF rounded-rect pass ──────────────────────────────────
        // Runs after the Clear pass (so it composites over the cleared surface)
        // but BEFORE tile/content/chrome geometry — this is what ensures
        // Background backdrops are correctly occluded by agent tiles. Commands
        // were collected scene-side in `collect_encode_inputs`.
        self.encode_rounded_rect_pass(&mut encoder, frame_view, &inputs.rr_background, sw, sh);

        // ── Geometry pass 2: Tiles + Content + Chrome flat-rect zones ─────────
        // Uses LoadOp::Load to preserve the Background geometry drawn above.
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame_pass_tiles_content_chrome"),
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

            if use_overlay_pipeline || self.use_opaque_rect_pipeline() {
                render_pass.set_pipeline(&self.clear_pipeline);
            } else {
                render_pass.set_pipeline(&self.pipeline);
            }

            if let Some(ref buffer) = vertex_buffer {
                let bg_end = bg_vertex_count.min(vertices.len());
                let rest_count = vertices.len().saturating_sub(bg_end);
                if rest_count > 0 {
                    render_pass.set_vertex_buffer(0, buffer.slice(..));
                    render_pass.draw(bg_end as u32..vertices.len() as u32, 0..1);
                }
            }
        }

        // ── Content + Chrome SDF rounded-rect pass ────────────────────────────
        // Runs after tiles so Content/Chrome backdrops composite above tiles.
        // `rr_post` (content + chrome + per-tile) was collected scene-side.
        self.encode_rounded_rect_pass(&mut encoder, frame_view, &inputs.rr_post, sw, sh);

        // ── Text pass (Stage 6) ───────────────────────────────────────────────
        // The text items were collected and glyphon-prepared scene-side in
        // `collect_encode_inputs` (Phase A/B). Here we only replay the prepared
        // TextAreas (Phase C) and always trim the atlas — no scene access.
        let inline_verts = &inputs.inline_verts;
        let use_opaque_rect_pipeline = self.use_opaque_rect_pipeline();
        if let Some(ref mut tr) = self.text_rasterizer {
            if inputs.render_text {
                // ── Inline backdrop pass (Phase 2, hud-9ieev) ────────────────
                // Render pixel-exact backdrop quads for inline code spans BEFORE
                // the glyphon glyph pass so backdrops appear behind the glyphs.
                // Uses LoadOp::Load to preserve geometry pixels.
                if !inline_verts.is_empty() {
                    let inline_buf =
                        self.device
                            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("inline_backdrop_verts"),
                                contents: bytemuck::cast_slice(inline_verts),
                                usage: wgpu::BufferUsages::VERTEX,
                            });
                    let mut inline_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("inline_backdrop_pass"),
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
                    if use_overlay_pipeline || use_opaque_rect_pipeline {
                        inline_pass.set_pipeline(&self.clear_pipeline);
                    } else {
                        inline_pass.set_pipeline(&self.pipeline);
                    }
                    inline_pass.set_vertex_buffer(0, inline_buf.slice(..));
                    inline_pass.draw(0..inline_verts.len() as u32, 0..1);
                }

                // ── Glyphon text pass ────────────────────────────────────────
                let mut text_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("text_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: frame_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // LoadOp::Load: preserve geometry pixels under the text.
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                if let Err(e) = tr.render_text_pass(&mut text_pass) {
                    tracing::warn!(error = %e, "text render failed — frame continues without text");
                }
            }
            // Trim every frame regardless of item count — glyphs from prior frames
            // must be evicted even when the current frame has no text.
            tr.trim_atlas();
        }

        let encode_us = encode_start.elapsed().as_micros() as u64;
        (encoder, encode_us)
    }

    /// Encode one frame's GPU command buffer directly from the scene (the
    /// headless / chrome-render convenience wrapper).
    ///
    /// Behaviourally identical to the pre-split monolith: it simply threads the
    /// scene through [`Compositor::collect_encode_inputs`] (scene reads) and then
    /// [`Compositor::encode_from_inputs`] (GPU encode). The windowed present path
    /// does NOT use this wrapper — it calls the two halves separately so it can
    /// drop the scene lock between them (hud-uyhpn).
    #[allow(clippy::too_many_arguments)]
    fn encode_frame(
        &mut self,
        vertices: &[RectVertex],
        frame_view: &wgpu::TextureView,
        scene: &SceneGraph,
        surf_w: u32,
        surf_h: u32,
        use_overlay_pipeline: bool,
        bg_vertex_count: usize,
    ) -> (wgpu::CommandEncoder, u64) {
        let inputs = self.collect_encode_inputs(scene, surf_w, surf_h);
        self.encode_from_inputs(
            vertices,
            frame_view,
            &inputs,
            surf_w,
            surf_h,
            use_overlay_pipeline,
            bg_vertex_count,
        )
    }
}

pub(crate) fn focus_or_grip_changed(
    previous_focus: Option<focus_ring::FocusRingOwner>,
    current_focus: Option<focus_ring::FocusRingOwner>,
    previous_grip: Option<tze_hud_scene::SceneId>,
    current_grip: Option<tze_hud_scene::SceneId>,
) -> bool {
    previous_focus != current_focus || previous_grip != current_grip
}

/// Collect [`TextItem`]s for all `TextOverflow::Ellipsis` nodes reachable from
/// `node_id`, without scroll offset (prime-time geometry).
///
/// This is a free function (not a method) to avoid a split-borrow conflict in
/// [`Compositor::prime_truncation_cache`], where `self.text_rasterizer` is
/// borrowed mutably while the markdown snapshot (loaded from the primer) and
/// `self.node_key_cache` are read immutably.
///
/// The geometry produced here is identical to what `collect_text_items_from_node`
/// produces at scroll_x=0, scroll_y=0 (valid because truncation is geometry-
/// Returns `true` if the mid-drag cadence gate should defer a truncation cache
/// re-prime call.
///
/// Called by [`Compositor::prime_truncation_cache`] to rate-limit re-primes
/// during rapid geometry changes (e.g. hotkey resize).  The gate is bypassed
/// for forced primes (`truncation_cache_scene_version == u64::MAX`) — callers
/// are responsible for that check.
///
/// # Parameters
/// - `last_at`: timestamp of the last successful truncation cache prime, or
///   `None` if no prime has ever run.
/// - `interval_ms`: minimum interval between primes in milliseconds
///   (derived from content length via [`adaptive_reprime_interval_ms`]).
///
/// Returns `true` (defer) only when `last_at` is `Some` and the elapsed time
/// is less than `interval_ms`.  Returns `false` (allow) when `last_at` is
/// `None` (first call ever) or the interval has elapsed.
pub(crate) fn should_defer_reprime(last_at: Option<std::time::Instant>, interval_ms: u64) -> bool {
    match last_at {
        None => false, // first call: never defer
        Some(last) => last.elapsed() < std::time::Duration::from_millis(interval_ms),
    }
}

/// Return whether a tile is currently in follow-tail/at-tail mode for the
/// purpose of ellipsis truncation geometry.
///
/// This is the single authoritative computation shared by
/// [`Compositor::prime_truncation_cache`] (via [`collect_ellipsis_text_items_from_node`])
/// and [`Compositor::collect_text_items`] (via [`Compositor::collect_text_items_from_node`]).
/// Both callers must resolve `at_tail` identically so the per-frame
/// [`crate::text::TruncationKey`] matches the primed cache entry (hud-lu50e).
///
/// Returns `true` (TailAnchored — spec §3.2: newest lines visible) when the
/// tile has been registered at-tail by the follow-tail driver.  Returns `false`
/// (HeadAnchored — spec §3.3: viewport stability) when the tile has been
/// scrolled back or has never been registered (e.g., non-scrollable tiles).
///
/// Extracting this as a named helper eliminates the hand-mirrored
/// `must-mirror` comment that previously guarded the two call sites (hud-plz8q).
pub(crate) fn tile_at_tail_for_ellipsis(tile_id: SceneId, scene: &SceneGraph) -> bool {
    scene.tile_follow_tail_at_tail(tile_id)
}

/// Collect the ids of every node that belongs to a **governed portal surface**
/// — a tile whose scroll config is set (hud-3ryie).
///
/// A node is portal-scoped iff it lies in the subtree of a scrollable tile.
/// This mirrors the "portal = tile has `TileScrollConfig`" gate used elsewhere
/// (e.g. `SceneGraph::portal_header_band_anchors`).  The returned set is
/// consulted by `prime_markdown_cache` (and must agree with the render path's
/// per-tile token selection) to decide whether a markdown node resolves the
/// portal or the generic token set.
///
/// In v1 every markdown node is under a portal tile, so this returns all of
/// them and per-tile scoping is a no-op (identical to pre-hud-3ryie behavior).
fn portal_markdown_node_ids(scene: &SceneGraph) -> HashSet<SceneId> {
    let mut ids = HashSet::new();
    for tile in scene.tiles.values() {
        if scene.tile_scroll_config(tile.id).is_none() {
            continue;
        }
        if let Some(root) = tile.root_node {
            collect_subtree_node_ids(scene, root, &mut ids);
        }
    }
    ids
}

/// Depth-first collect `node_id` and all its descendants into `out`.
///
/// The `insert` return value doubles as a visited-guard, so a malformed graph
/// with a shared or cyclic child reference terminates instead of recursing
/// forever.
fn collect_subtree_node_ids(scene: &SceneGraph, node_id: SceneId, out: &mut HashSet<SceneId>) {
    if !out.insert(node_id) {
        return;
    }
    if let Some(node) = scene.nodes.get(&node_id) {
        for child in &node.children {
            collect_subtree_node_ids(scene, *child, out);
        }
    }
}

/// Resolve the pixel dimensions for a widget instance.
///
/// Returns (width, height) in pixels. Returns (0, 0) if the geometry cannot be
/// resolved (e.g., zero-sized surface or unrecognized policy).
pub(super) fn resolve_widget_pixel_size(
    instance: &WidgetInstance,
    def: &WidgetDefinition,
    surf_w: u32,
    surf_h: u32,
) -> (u32, u32) {
    let geo = instance
        .geometry_override
        .as_ref()
        .unwrap_or(&def.default_geometry_policy);
    let sw = surf_w as f32;
    let sh = surf_h as f32;
    let (w, h) = match geo {
        GeometryPolicy::Relative {
            width_pct,
            height_pct,
            ..
        } => (sw * width_pct, sh * height_pct),
        GeometryPolicy::EdgeAnchored {
            width_pct,
            height_pct,
            ..
        } => (sw * width_pct, sh * height_pct),
    };
    let w = (w.max(1.0) as u32).min(surf_w.max(1));
    let h = (h.max(1.0) as u32).min(surf_h.max(1));
    (w, h)
}

#[derive(Debug, thiserror::Error)]
pub enum CompositorError {
    #[error("no suitable GPU adapter found")]
    NoAdapter,
    #[error("failed to create device: {0}")]
    DeviceCreation(String),
}

// Make buffer init descriptor available
use wgpu::util::DeviceExt;

#[cfg(test)]
mod tests;
