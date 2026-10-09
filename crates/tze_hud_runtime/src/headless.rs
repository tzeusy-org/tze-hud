//! Headless runtime — runs the full 8-stage frame pipeline without a display server.
//! Suitable for testing, CI, and artifact generation.
//!
//! The headless runtime runs all pipeline stages sequentially in the same task
//! (no cross-thread signalling). This is correct for testing because the pipeline
//! contract (stage order, per-stage telemetry, ArcSwap snapshot, overflow counter)
//! is identical to the windowed runtime; only the thread assignment differs.
//!
//! # Headless Mode Parity
//!
//! Per runtime-kernel/spec.md Requirement: Headless Mode (line 198):
//! "Headless mode MUST use the same process, code path, and pipeline as
//! windowed mode.  The only difference SHALL be the render surface."
//!
//! `HeadlessRuntime` wires the full pipeline:
//! - `Compositor` (GPU device + wgpu render pipeline)
//! - `HeadlessSurface` (offscreen render target, `present()` is a no-op)
//! - `InputProcessor` (hit-test, local feedback)
//! - `TelemetryCollector` (per-frame telemetry)
//! - `SharedState` (scene + session registry)
//! - gRPC server (HudSession streaming — optional)
//!
//! # Software GPU
//!
//! `HeadlessRuntime::new` respects the `HEADLESS_FORCE_SOFTWARE` environment
//! variable (spec line 409).  When set to `1`, wgpu adapter selection uses
//! `force_fallback_adapter = true` (llvmpipe on Linux, WARP on Windows).
//!
//! # Session Limits & Hot-Connect
//!
//! Per spec Requirement: Session Limits (line 355): headless mode still runs
//! the gRPC session server — agents connect normally, session limits are
//! enforced identically.
//!
//! Per spec Requirement: Hot-Connect (line 346): agents connecting to headless
//! runtime receive the full scene snapshot (handled by `HudSessionImpl`).
//!
//! # grpc_port = 0
//!
//! Setting `grpc_port = 0` in `HeadlessConfig` disables the gRPC server.
//! Tests that don't exercise the session layer use this to skip server startup.

use crate::degradation::{DegradationController, DegradationEnvelope};
use crate::element_store::bootstrap_scene_element_store;
use crate::input_latency::{
    PendingInputLatencySamples, drain_pending_input_latency, record_committed_input_response,
    record_pending_input_latency,
};
use crate::pipeline::{FramePipeline, HitTestSnapshot};
use crate::runtime_context::RuntimeContext;
use crate::scene_startup::run_scene_startup;
use crate::widget_runtime_registration::process_pending_widget_svgs;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tze_hud_compositor::{Compositor, HeadlessSurface};
use tze_hud_config::resolve_runtime_widget_asset_store;
use tze_hud_input::{AgentDispatch, InputProcessor, InputResult, PointerEvent, ScrollEvent};
use tze_hud_protocol::proto::FramePresented;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::session::SharedState;
use tze_hud_protocol::session_server::{DegradationNoticeSender, HudSessionImpl, SessionDeps};
use tze_hud_resource::{
    ResourceStore, ResourceStoreConfig, RuntimeWidgetStore, RuntimeWidgetStoreConfig,
};
use tze_hud_scene::config::{AgentDirectory, SharedAgents};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::mutation::{MutationBatch, MutationResult, SceneMutation};
use tze_hud_telemetry::{FrameTelemetry, TelemetryCollector};
use wgpu::TextureFormat;

/// Configuration for the headless runtime.
pub struct HeadlessConfig {
    /// Render target width in pixels.
    pub width: u32,
    /// Render target height in pixels.
    pub height: u32,
    /// gRPC server port.  Set to `0` to disable the gRPC server entirely
    /// (useful for tests that only need rendering, not session management).
    ///
    /// The headless gRPC server binds IPv6 loopback (`[::1]`) only.
    pub grpc_port: u16,
    /// The agents that may connect: paired agents (from `agents.toml`, see
    /// `tze_hud_config::AgentsFile`) and, for tests and dev, an
    /// [`AgentDirectory::unrestricted`] dev PSK.
    pub agents: AgentDirectory,
    /// Optional TOML config string to load.
    ///
    /// When `Some(toml)`, the runtime parses and validates it, building a
    /// `RuntimeContext` with the profile budgets. If parsing or validation
    /// fails, falls back to headless-default.
    ///
    /// When `None`:
    /// - Under `cfg(any(test, feature = "dev-mode"))`: a headless-profile
    ///   `RuntimeContext` is used.
    /// - In production builds (without `dev-mode` feature): startup **fails**
    ///   with an error.
    pub config_toml: Option<String>,
}

/// `HeadlessConfig::default()` is only available under `cfg(test)` or with the
/// `dev-mode` feature enabled, because the default sets `config_toml: None`
/// and an unrestricted dev PSK (`test-key`).
///
/// Production code must construct `HeadlessConfig` explicitly and supply a
/// `config_toml` value.
#[cfg(any(test, feature = "dev-mode"))]
impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            grpc_port: 50051,
            agents: AgentDirectory::unrestricted("test-key"),
            config_toml: None,
        }
    }
}

impl HeadlessConfig {
    /// Build a `RuntimeContext` from the optional TOML config.
    ///
    /// If `config_toml` is `None`:
    /// - Under `cfg(any(test, feature = "dev-mode"))`: returns a headless-default
    ///   context.
    /// - In production builds: returns `Err` — config is required.
    ///
    /// If `config_toml` is `Some(toml)`, see [`RuntimeContext::from_toml`]
    /// (a bad config falls back to the headless default).
    pub fn build_runtime_context(&self) -> Result<RuntimeContext, Box<dyn std::error::Error>> {
        match &self.config_toml {
            None => {
                #[cfg(any(test, feature = "dev-mode"))]
                {
                    Ok(RuntimeContext::headless_default())
                }
                #[cfg(not(any(test, feature = "dev-mode")))]
                {
                    Err(
                        "HeadlessConfig: config_toml is None but the `dev-mode` feature is not \
                         enabled. Production builds require an explicit config. Supply a TOML config string via \
                         `HeadlessConfig { config_toml: Some(toml), .. }` or enable the \
                         `dev-mode` feature for development use."
                            .into(),
                    )
                }
            }
            Some(toml) => Ok(RuntimeContext::from_toml(Some(toml))),
        }
    }
}

/// The headless runtime instance.
///
/// Owns all runtime state: GPU compositor, offscreen surface, input processor,
/// telemetry collector, and scene/session state.
/// Includes the frame pipeline orchestrator (ArcSwap hit-test snapshot,
/// overflow counter, per-stage telemetry).
pub struct HeadlessRuntime {
    pub compositor: Compositor,
    pub surface: HeadlessSurface,
    pub input_processor: InputProcessor,
    pub telemetry: TelemetryCollector,
    pub state: Arc<Mutex<SharedState>>,
    pub config: HeadlessConfig,
    /// The 8-stage frame pipeline orchestrator.
    pub pipeline: FramePipeline,
    /// Immutable runtime context built from validated config at startup.
    ///
    /// Holds profile budgets.
    pub runtime_context: Arc<RuntimeContext>,
    /// The live agent directory the gRPC server authenticates against; store
    /// a new directory to pair or remove agents without a restart.
    pub agents: SharedAgents,
    /// Keeps the durable runtime widget asset store alive for runtime lifetime.
    _runtime_widget_store: Option<RuntimeWidgetStore>,
    /// Actual input samples; response context exists only for applied batches.
    pending_input_latency: PendingInputLatencySamples,
    /// Broadcast sender for batch-correlated present acknowledgments (hud-91uu6).
    ///
    /// When set, `render_frame` drains the scene's present-ack queue after each
    /// presented frame and emits a [`FramePresented`] pairing the applied
    /// batch_ids with the frame number + present wall-clock. `None` (the default)
    /// disables emission — the drain still runs so the queue never grows unbounded.
    /// Wire this to `HudSessionImpl::frame_presented_tx` (via
    /// [`HeadlessRuntime::set_frame_presented_tx`]) to deliver to gRPC subscribers.
    frame_presented_tx: Option<tokio::sync::broadcast::Sender<FramePresented>>,
    degradation_controller: DegradationController,
    degradation_notices: DegradationNoticeSender,
    degradation_clock_start: Instant,
}

impl HeadlessRuntime {
    /// Create a new headless runtime.
    ///
    /// Respects `HEADLESS_FORCE_SOFTWARE=1` — when set, the wgpu adapter
    /// selection uses `force_fallback_adapter = true` (spec line 211).
    ///
    /// If `config.config_toml` is provided, the runtime builds an immutable
    /// `RuntimeContext` from the loaded config (profile budgets). If config is
    /// present but fails to parse/validate, falls back to headless-default.
    ///
    /// If `config.config_toml` is `None`:
    /// - Under `cfg(any(test, feature = "dev-mode"))`: uses headless-default.
    /// - In production builds (without `dev-mode`): returns `Err` immediately —
    ///   the runtime refuses to start without an explicit config.
    pub async fn new(config: HeadlessConfig) -> Result<Self, Box<dyn std::error::Error>> {
        // Build RuntimeContext at startup from loaded config (or default).
        // Returns Err if config_toml is None in a production build (dev-mode not enabled).
        let runtime_context = Arc::new(config.build_runtime_context()?);
        let agents = config.agents.clone().shared();

        let mut compositor = Compositor::new_headless(config.width, config.height).await?;
        let surface = HeadlessSurface::new(&compositor.device, config.width, config.height);

        // ── Initialize text renderer ──────────────────────────────────────
        // HeadlessSurface always uses Rgba8UnormSrgb. Must be called here so
        // glyphon text rendering is active for all runtime paths (not just tests).
        compositor.init_text_renderer(TextureFormat::Rgba8UnormSrgb);
        compositor.init_widget_renderer(TextureFormat::Rgba8UnormSrgb);
        compositor.set_resident_ledger(runtime_context.resident_ledger.clone());
        // Apply the resolved per-surface truncation-input bound so the
        // viewport-adjacent-window fallback engages at the operator-configured
        // threshold rather than the compositor's built-in default (hud-59p2z).
        compositor.set_max_truncation_input_bytes(
            runtime_context.profile.max_truncation_input_bytes as usize,
        );
        tracing::debug!(
            max_truncation_input_bytes = runtime_context.profile.max_truncation_input_bytes,
            "headless: text + widget renderers initialized"
        );

        // ── Scene startup: design tokens + zone registry ──────────────────
        // When config_toml is provided, run scene startup so design tokens reach
        // the compositor and the zone registry receives token-derived rendering
        // policies.  Mirrors what the windowed runtime does in its initializer.
        let mut scene = SceneGraph::new(config.width as f32, config.height as f32);
        let (runtime_widget_store, compositor_token_map): (
            Option<RuntimeWidgetStore>,
            std::collections::HashMap<String, String>,
        ) = if let Some(toml_str) = &config.config_toml {
            match toml::from_str::<tze_hud_config::raw::RawConfig>(toml_str) {
                Ok(raw) => {
                    let resolved = resolve_runtime_widget_asset_store(&raw, None).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("runtime widget asset store config invalid: {}", e.hint),
                        )
                    })?;
                    let runtime_widget_store =
                        RuntimeWidgetStore::open(RuntimeWidgetStoreConfig {
                            store_path: resolved.store_path,
                            max_total_bytes: resolved.max_total_bytes,
                            max_agent_bytes: resolved.max_agent_bytes,
                        })?;
                    let mut startup_result = run_scene_startup(&raw, None, &mut scene);
                    // Register widget SVG assets with the headless widget renderer,
                    // mirroring the windowed runtime so bundled SVG-based widgets render correctly.
                    process_pending_widget_svgs(
                        compositor.widget_renderer_mut(),
                        startup_result.widget_svg_assets.drain(..),
                    );
                    tracing::debug!(
                        token_count = startup_result.global_tokens.len(),
                        "headless: scene startup complete — design tokens and zone registry applied"
                    );
                    (Some(runtime_widget_store), startup_result.global_tokens)
                }
                Err(e) => {
                    // Even when a RuntimeContext has been constructed (potentially via
                    // fallbacks in build_runtime_context), raw deserialization into
                    // RawConfig may still fail. Fall back to canonical zone defaults
                    // with no token derivation.
                    tracing::warn!(
                        error = %e,
                        "headless: component startup skipped — raw config parse failed; \
                         zone registry will use defaults, no design tokens applied"
                    );
                    scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
                    (None, std::collections::HashMap::new())
                }
            }
        } else {
            // No config provided — bootstrap with canonical zone defaults.
            scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
            (None, std::collections::HashMap::new())
        };

        // Apply the resolved design tokens to the
        // compositor so token-driven properties are resolved at render time.
        compositor.set_token_map(compositor_token_map);
        tracing::debug!("headless: compositor token map applied");

        let element_store_bootstrap = bootstrap_scene_element_store(&mut scene);
        let scene = Arc::new(Mutex::new(scene));
        let sessions = tze_hud_protocol::session::SessionRegistry::new();
        let resident_limits = runtime_context.resident_store_limits();
        let state = Arc::new(Mutex::new(SharedState {
            scene,
            sessions,
            resource_store: ResourceStore::new_with_resident_ledger(
                ResourceStoreConfig {
                    max_total_texture_bytes: resident_limits.resource_bytes,
                    ..ResourceStoreConfig::default()
                },
                runtime_context.resident_ledger.clone(),
            ),
            runtime_widget_store: runtime_widget_store.clone(),
            element_store: element_store_bootstrap.store,
            element_store_path: Some(element_store_bootstrap.path),
            safe_mode_atomic: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
            token_store: tze_hud_protocol::token::TokenStore::new(),
            freeze_active: false,
            input_capture_tx: None,
            input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tile_placement: Default::default(),
        }));

        tracing::info!(
            target: "tze_hud::resident_accounting",
            snapshot = %runtime_context.resident_accounting_snapshot(),
            "headless runtime resident accounting initialised"
        );

        let degradation_notices = DegradationNoticeSender::default();
        Ok(Self {
            compositor,
            surface,
            input_processor: InputProcessor::new(),
            telemetry: TelemetryCollector::new(),
            state,
            config,
            pipeline: FramePipeline::new(),
            runtime_context,
            agents,
            _runtime_widget_store: runtime_widget_store,
            pending_input_latency: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            frame_presented_tx: None,
            degradation_controller: DegradationController::with_envelope(
                crate::degradation::DegradationConfig::default(),
                DegradationEnvelope::from_effective_fps(60).expect("60 Hz is valid"),
            ),
            degradation_notices,
            degradation_clock_start: Instant::now(),
        })
    }

    /// Wire a `FramePresented` broadcast sender so the render loop emits
    /// batch-correlated present acknowledgments (hud-91uu6).
    ///
    /// Pass `HudSessionImpl::frame_presented_tx.clone()` to deliver present acks
    /// to gRPC agents subscribed to TELEMETRY_FRAMES, or a standalone sender in
    /// tests to observe the correlation directly.
    pub fn set_frame_presented_tx(&mut self, tx: tokio::sync::broadcast::Sender<FramePresented>) {
        self.frame_presented_tx = Some(tx);
    }

    /// Get a reference to the shared state (scene + sessions).
    pub fn shared_state(&self) -> &Arc<Mutex<SharedState>> {
        &self.state
    }

    /// Run one frame through the full 8-stage pipeline.
    ///
    /// Stages 1-8 run sequentially in the calling task (no cross-thread signalling
    /// in headless mode). Per-stage telemetry is recorded in the returned
    /// `FrameTelemetry`.
    /// Same-frame widget raster observations from the compositor are preserved
    /// in both this return value and the telemetry collector.
    ///
    /// The compositor renders the current scene to the headless surface via
    /// `render_frame_headless()`, which includes the `copy_to_buffer` step so
    /// that `read_pixels()` returns actual rendered pixel data after this call.
    pub async fn render_frame(&mut self) -> FrameTelemetry {
        self.render_frame_inner::<
            fn(&AgentDispatch, &mut SceneGraph) -> Option<(MutationBatch, MutationResult)>,
        >(None)
        .await
        .0
    }

    /// Process actual pointer input, a controlled in-process response, and its frame.
    ///
    /// The handler receives the real dispatch and applies its response through
    /// `SceneGraph::apply_batch`, returning that batch and result. Only a newly
    /// applied content/opacity response for this dispatch's owner and tile is
    /// associated. Cached replies, unrelated work and rejected responses are not
    /// input-to-response measurements. The scene lock covers processing through
    /// submit, so this frame contains the accepted response without an intervening
    /// agent mutation. This is a local submit boundary, not a network roundtrip or
    /// Windows surface present/scanout; it is not an authentication boundary.
    pub async fn render_frame_with_input_response<F>(
        &mut self,
        event: &PointerEvent,
        respond: F,
    ) -> (FrameTelemetry, InputResult, Option<MutationResult>)
    where
        F: FnOnce(&AgentDispatch, &mut SceneGraph) -> Option<(MutationBatch, MutationResult)>,
    {
        let (telemetry, input) = self.render_frame_inner(Some((event, respond))).await;
        let (input, response) = input.expect("the supplied pointer event was processed");
        (telemetry, input, response)
    }

    async fn render_frame_inner<F>(
        &mut self,
        input: Option<(&PointerEvent, F)>,
    ) -> (
        FrameTelemetry,
        Option<(InputResult, Option<MutationResult>)>,
    )
    where
        F: FnOnce(&AgentDispatch, &mut SceneGraph) -> Option<(MutationBatch, MutationResult)>,
    {
        // Stamp the actual input entry, including scene-lock wait. Ordinary frames
        // have no input timestamp and cannot become input measurements.
        let input_started_at = input.as_ref().map(|_| Instant::now());
        let frame_start = Instant::now();
        // Include all active scene/compositor work performed for this frame;
        // this boundary precedes expiry, animation, and Stage 3 work and ends
        // only after Stage 7 completes.
        let degradation_work_start = Instant::now();
        let state = self.state.lock().await;
        // Clone the Arc so we can release the SharedState lock before rendering.
        let scene_arc = state.scene.clone();
        drop(state);
        let mut scene_guard = scene_arc.lock().await;
        // Register runtime-uploaded widget SVG assets before rendering so new
        // registrations are visible on the next frame.
        process_pending_widget_svgs(
            self.compositor.widget_renderer_mut(),
            scene_guard.drain_pending_widget_svg_assets(),
        );

        // Timed content, expired publications, and elapsed leases
        // (invariants 1 and 4), all cleared before the next frame. Headless
        // keeps no session fan-out for the returned expiries; the scene-side
        // reclaim is what matters here.
        let _ = crate::pipeline::sweep_timed_scene_state(&mut scene_guard);

        // Drain the removal notification queue populated by remove_tile_and_nodes.
        // Without this, the queue grows unboundedly in headless / server runtimes
        // that delete tiles but have no windowed event loop to consume it.
        // The windowed path drains this in prune_portal_resize_states; headless
        // has no portal_resize_states consumer, so we simply discard here.
        let _ = scene_guard.drain_removed_tile_ids();

        // Per-publication TTL fade-out sweep: seed/tick animation states and
        // prune publications whose 150ms fade-out has completed.
        self.compositor.update_publication_animations(&scene_guard);
        self.compositor.prune_faded_publications(&mut scene_guard);

        // ── Capture compositor render work upfront ────────────────────────────
        // The headless pipeline runs all stages in-process. The compositor's
        // render_frame() handles stages 6 (encode) and 7 (submit) internally.
        // We split timing manually below.

        // Stage 1: Input Drain (headless — no OS event queue to drain)
        let s1_start = Instant::now();
        let stage1_us = s1_start.elapsed().as_micros() as u64;

        // Stage 2: Local Feedback (load ArcSwap snapshot — no mutex)
        // Per spec §3.2: Stage 2 reads the snapshot published after Stage 4 of the
        // *previous* frame. It must not publish a new snapshot (that is Stage 4's job).
        let s2_start = Instant::now();
        let _snap_ref = self.pipeline.hit_test_snapshot.load();
        let input_result = input
            .as_ref()
            .map(|(event, _)| self.input_processor.process(event, &mut scene_guard));
        let stage2_us = s2_start.elapsed().as_micros() as u64;

        // Stage 3: Mutation Intake (ordinary headless mutations arrive before render;
        // a controlled input handler applies its actual response at this boundary).
        let s3_start = Instant::now();
        let mut associated_response = false;
        let response = input.and_then(|(_, respond)| {
            let result = input_result.as_ref()?;
            let dispatch = result.dispatch.as_ref()?;
            let sequence_before = scene_guard.sequence_number;
            let (batch, response) = respond(dispatch, &mut scene_guard)?;
            let matches_target = batch.agent_namespace == dispatch.namespace
                && !batch.mutations.is_empty()
                && batch.mutations.iter().all(|mutation| match mutation {
                    SceneMutation::UpdateNodeContent { tile_id, .. }
                    | SceneMutation::UpdateTileOpacity { tile_id, .. } => {
                        *tile_id == dispatch.tile_id
                    }
                    _ => false,
                });
            let newly_applied = response.applied
                && response.batch_id == batch.batch_id
                && response.sequence_number.is_some_and(|sequence| {
                    sequence > sequence_before && sequence == scene_guard.sequence_number
                });
            if matches_target && newly_applied {
                record_committed_input_response(
                    &self.pending_input_latency,
                    input_started_at.expect("input entry has a timestamp"),
                    result.local_ack_us,
                    batch.batch_id,
                    Instant::now(),
                );
                associated_response = true;
            }
            Some(response)
        });
        if let Some(result) = &input_result
            && !associated_response
        {
            record_pending_input_latency(
                &self.pending_input_latency,
                input_started_at.expect("input entry has a timestamp"),
                result.local_ack_us,
            );
        }
        let stage3_us = s3_start.elapsed().as_micros() as u64;

        // Stage 4: Scene Commit (headless — scene already committed)
        let s4_start = Instant::now();
        let new_snap = HitTestSnapshot::from_scene(&scene_guard);
        self.pipeline.hit_test_snapshot.store(Arc::new(new_snap));

        // ── Commit-time markdown cache prime (hud-380dl) ──────────────────────
        // Prime the markdown parse cache here, at the end of Stage 4 (scene
        // commit), before any render stage executes.  This moves parsing off the
        // render thread entirely: render_frame_headless finds the cache already
        // populated and does no parse work.  The prime is gated internally on
        // scene.version so it is a no-op when the scene has not changed.
        let markdown_prime_start = Instant::now();
        self.compositor.prime_markdown_cache(&scene_guard);
        let markdown_prime_us = markdown_prime_start.elapsed().as_micros() as u64;

        // ── Commit-time truncation cache prime (hud-v2z6u) ───────────────────
        // Prime the truncation cache after markdown, at the end of Stage 4,
        // so render_frame_headless never does shaping work in the frame loop.
        // Gated internally on scene.version; no-op when unchanged.
        self.compositor.prime_truncation_cache(&scene_guard);

        let applied_degradation_level = self.degradation_controller.level();
        self.compositor
            .set_degradation_policy(self.degradation_controller.compositor_policy());

        let stage4_us = s4_start.elapsed().as_micros() as u64;

        // Stage 5: Layout Resolve
        let s5_start = Instant::now();
        let stage5_us = s5_start.elapsed().as_micros() as u64;

        // Stages 6 + 7: Render Encode + GPU Submit (handled by Compositor::render_frame_headless)
        // Must use render_frame_headless (not render_frame) so copy_to_buffer is called
        // before queue.submit(), making read_pixels() return actual rendered pixel data.
        // render_frame_headless takes &mut SceneGraph to populate zone_hit_regions after
        // rendering so that interactive zone affordances are ready for the next frame's
        // hit-testing.
        let (compositor_telemetry, frame_submitted) = self
            .compositor
            .render_frame_headless_with_submission(&mut scene_guard, &self.surface);
        let submitted_at = frame_submitted.then(Instant::now);
        // Total frame time from Compositor covers encode + submit
        let stage6_us = compositor_telemetry.stage6_render_encode_us;
        let stage7_us = compositor_telemetry.stage7_gpu_submit_us;

        // Total frame time: stage 1 start → stage 7 end
        let frame_time_us = frame_start.elapsed().as_micros() as u64;
        let degradation_work_time_us = degradation_work_start.elapsed().as_micros() as u64;

        // ── Batch-correlated present acknowledgment (hud-91uu6) ───────────────
        // Stage 7 (GPU submit) is complete, so any batches applied to the scene
        // since the last present are now on screen. Drain them and, if a sender
        // is wired, emit a FramePresented pairing those batch_ids with this
        // frame's number + present wall-clock. The drain runs unconditionally so
        // the queue never grows unbounded even when no subscriber is attached.
        // A skipped submission leaves both batch identity and timing context
        // pending for a frame that actually carries the response.
        let present_ack_batch_ids = if frame_submitted {
            scene_guard.drain_present_ack_batch_ids()
        } else {
            Vec::new()
        };
        let input_latencies = drain_pending_input_latency(
            &self.pending_input_latency,
            &present_ack_batch_ids,
            submitted_at,
        );
        if let Some(tx) = &self.frame_presented_tx
            && !present_ack_batch_ids.is_empty()
        {
            let present_wall_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            let event = FramePresented {
                frame_number: compositor_telemetry.frame_number,
                present_wall_us,
                // 16-byte big-endian UUID, matching the scene_id_to_bytes /
                // bytes_to_scene_id wire contract used for MutationBatch.batch_id.
                batch_ids: present_ack_batch_ids
                    .iter()
                    .map(|id| id.as_uuid().as_bytes().to_vec())
                    .collect(),
            };
            // Broadcast send errors only when there are no subscribers — not an
            // error condition for a droppable state-stream present ack.
            let _ = tx.send(event);
        }
        drop(scene_guard);

        // Build the per-stage telemetry record (outside the Stage 8 timed region)
        let mut telemetry = FrameTelemetry::new(compositor_telemetry.frame_number);
        telemetry.stage1_input_drain_us = stage1_us;
        telemetry.stage2_local_feedback_us = stage2_us;
        telemetry.stage3_mutation_intake_us = stage3_us;
        telemetry.stage4_scene_commit_us = stage4_us;
        telemetry.stage5_layout_resolve_us = stage5_us;
        telemetry.stage6_render_encode_us = stage6_us;
        telemetry.stage7_gpu_submit_us = stage7_us;
        telemetry.frame_time_us = frame_time_us;
        telemetry.degradation_work_time_us = degradation_work_time_us;
        telemetry.degradation_level = applied_degradation_level.as_u8();
        // ── Split input latency fields ─────────────────────────────────────
        // Only actual input and its newly applied associated response populate
        // these fields. Ordinary/no-response frames retain constructor-zero;
        // frame_start and unrelated scene work are never input-arrival proxies.
        if let Some((local_ack, scene_commit, next_submit)) = input_latencies {
            telemetry.input_to_local_ack_us = local_ack;
            telemetry.input_to_scene_commit_us = scene_commit;
            telemetry.input_to_next_present_us = next_submit;
        }
        telemetry.tile_count = compositor_telemetry.tile_count;
        telemetry.node_count = compositor_telemetry.node_count;
        telemetry.active_leases = compositor_telemetry.active_leases;
        telemetry.mutations_applied = compositor_telemetry.mutations_applied;
        telemetry.hit_region_updates = compositor_telemetry.hit_region_updates;
        telemetry.widget_rasterized = compositor_telemetry.widget_rasterized;
        // Propagate commit-time markdown prime cost (hud-380dl).
        // Non-zero only when scene.version changed this frame (new/changed content
        // required a parse pass); zero on steady-state frames (cache hit, no work).
        telemetry.markdown_prime_us = markdown_prime_us;

        let completed_at_us = self.degradation_clock_start.elapsed().as_micros() as u64;
        if let Some(event) = self
            .degradation_controller
            .record_frame_at(degradation_work_time_us, completed_at_us)
        {
            tracing::warn!(
                previous_level = event.previous_level,
                new_level = event.new_level,
                direction = ?event.direction,
                p95_us = event.frame_time_p95_us,
                sample_count = event.sample_count,
                window_duration_us = event.window_duration_us,
                effective_cadence_hz = event.effective_cadence_hz,
                entry_threshold_us = event.entry_threshold_us,
                recovery_threshold_us = event.recovery_threshold_us,
                recovery_source = ?event.recovery_source,
                "runtime degradation transition"
            );
            let notice = self.degradation_controller.protocol_notice(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_micros() as u64)
                    .unwrap_or(0),
            );
            self.degradation_notices.publish(notice).await;
        }

        // Stage 8: Telemetry Emit — non-blocking record into collector.
        // Timer wraps the actual emit so the measurement reflects its cost.
        let s8_start = Instant::now();
        self.telemetry.record(telemetry.clone());
        telemetry.stage8_telemetry_emit_us = s8_start.elapsed().as_micros() as u64;

        (telemetry, input_result.map(|input| (input, response)))
    }

    /// Read back pixels from the last rendered frame.
    ///
    /// Returns RGBA8 data (width × height × 4 bytes).  Blocks until GPU is idle.
    ///
    /// Per spec line 208: "pixel readback MUST be on-demand via copy_texture_to_buffer."
    pub fn read_pixels(&self) -> Vec<u8> {
        self.surface.read_pixels(&self.compositor.device)
    }

    /// Start the gRPC server in the background, serving the HudSession streaming service.
    ///
    /// Per spec Requirement: Session Limits (line 355): headless runtime still
    /// runs the gRPC server — agents connect normally.
    ///
    /// Per spec Requirement: Hot-Connect (line 346): the `HudSessionImpl` sends
    /// a full scene snapshot to each agent on connect.
    ///
    /// Returns the server task handle.  The caller must retain it to keep the
    /// server running.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `config.grpc_port == 0` (gRPC server disabled) or if
    /// the gRPC server fails to bind.
    pub async fn start_grpc_server(
        &self,
    ) -> Result<tokio::task::JoinHandle<()>, Box<dyn std::error::Error>> {
        if self.config.grpc_port == 0 {
            return Err("start_grpc_server: grpc_port = 0 (gRPC server disabled)".into());
        }

        // Loopback only. [::1] (IPv6) rather than 127.0.0.1 because the
        // integration tests connect via http://[::1]:{port}.
        // Binding before spawning eliminates the race that required a sleep:
        // the port is ready before this function returns.
        let bind_addr = format!("[::1]:{}", self.config.grpc_port);
        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| format!("gRPC server: failed to bind {bind_addr}: {e}"))?;
        tracing::info!(addr = %bind_addr, "gRPC server listener bound");

        Ok(self.serve_grpc_listener(listener))
    }

    /// Adopt a continuously owned fixture listener without changing the public
    /// configured-port checks or binding behavior.
    #[cfg(test)]
    fn start_grpc_server_on_listener(
        &self,
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        self.serve_grpc_listener(listener)
    }

    fn serve_grpc_listener(
        &self,
        listener: tokio::net::TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        let service = HudSessionImpl::from_deps(SessionDeps {
            resource_budget: self.runtime_context.resource_budget(),
            budget_enforcer: Some(std::sync::Arc::new(
                crate::mutation_budget_bridge::RuntimeMutationBudgetEnforcer::with_limits(
                    self.runtime_context
                        .operational_envelope
                        .max_resident_sessions,
                    self.runtime_context.operational_envelope.max_leased_tiles,
                    self.runtime_context
                        .operational_envelope
                        .max_agent_leased_texture_bytes,
                ),
            )),
            degradation_notices: self.degradation_notices.clone(),
            ..SessionDeps::new(self.state.clone(), self.agents.clone())
        });

        tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(HudSessionServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("gRPC server failed");
        })
    }

    /// Process a wheel/trackpad scroll event through the local-first scroll path.
    pub fn process_scroll_event(
        &mut self,
        event: &ScrollEvent,
        scene: &mut SceneGraph,
    ) -> Option<tze_hud_input::ScrollOffsetChangedEvent> {
        self.input_processor.process_scroll_event(event, scene)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_scene::types::{
        FontFamily, Node, NodeData, Rect, ResourceBudget, Rgba, SceneId, TextAlign,
        TextMarkdownNode, TextOverflow,
    };

    #[tokio::test]
    async fn headless_initialization_is_serial_and_releases_after_failure() {
        use crate::test_support::serialized_headless_init;
        use std::future::{Future, pending, poll_fn};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Poll, Waker};

        struct ActiveInit(Arc<AtomicUsize>);

        impl ActiveInit {
            fn enter(active: &Arc<AtomicUsize>, peak: &AtomicUsize) -> Self {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                Self(active.clone())
            }
        }

        impl Drop for ActiveInit {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let active = Arc::new(AtomicUsize::new(0));
        let peak = AtomicUsize::new(0);
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let mut first = Box::pin(serialized_headless_init(async {
            let _active = ActiveInit::enter(&active, &peak);
            finish_rx.await.expect("first initialization released");
        }));

        // Other runtime-lib tests may hold the process gate initially. Wait for
        // our first initializer to enter, without completing it or using sleep.
        poll_fn(|cx| {
            assert!(first.as_mut().poll(cx).is_pending());
            if active.load(Ordering::SeqCst) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        let mut second = Box::pin(serialized_headless_init(async {
            let _active = ActiveInit::enter(&active, &peak);
        }));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            second.as_mut().poll(&mut cx).is_pending(),
            "a second initializer must wait for admission"
        );
        let mut queued = Box::pin(serialized_headless_init(async {
            panic!("a cancelled queued initializer must never enter");
        }));
        assert!(queued.as_mut().poll(&mut cx).is_pending());
        drop(queued);

        finish_tx.send(()).expect("first initializer still alive");
        first.await;
        second.await;
        assert_eq!(peak.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);

        assert_eq!(
            serialized_headless_init(async { Err::<(), _>("initialization failed") }).await,
            Err("initialization failed"),
            "initialization errors must propagate unchanged"
        );
        let panic = tokio::spawn(serialized_headless_init(async {
            panic!("initializer unwind probe");
        }))
        .await
        .expect_err("initializer panic must propagate");
        assert!(panic.is_panic());

        let mut cancelled = Box::pin(serialized_headless_init(async {
            let _active = ActiveInit::enter(&active, &peak);
            pending::<()>().await;
        }));
        poll_fn(|cx| {
            assert!(cancelled.as_mut().poll(cx).is_pending());
            if active.load(Ordering::SeqCst) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        drop(cancelled);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            serialized_headless_init(async { "initialized again" }).await,
            "initialized again",
            "error, unwind and cancellation must leave admission available"
        );
    }
    use tze_hud_scene::{MutationBatch, SceneMutation};

    fn retained_plain_text_node(content: &str, width: f32, height: f32) -> Node {
        Node {
            id: SceneId::new(),
            children: vec![],
            layout: Default::default(),
            data: NodeData::TextMarkdown(TextMarkdownNode {
                content: content.into(),
                bounds: Rect::new(0.0, 0.0, width, height),
                font_size_px: 18.0,
                font_family: FontFamily::SystemSansSerif,
                color: Rgba::WHITE,
                background: None,
                alignment: TextAlign::Start,
                overflow: TextOverflow::Clip,
                color_runs: Box::default(),
            }),
        }
    }

    async fn install_transparent_overlap_scene(
        runtime: &HeadlessRuntime,
        lower_content: &str,
        with_intruding_idle_grip: bool,
    ) -> (
        Arc<Mutex<SceneGraph>>,
        SceneId,
        SceneId,
        SceneId,
        SceneId,
        Vec<SceneId>,
    ) {
        let shared_state = runtime.shared_state().lock().await;
        let scene_arc = Arc::clone(&shared_state.scene);
        drop(shared_state);
        let mut scene = scene_arc.lock().await;
        let tab_id = scene
            .create_tab("Transparent overlap", 0)
            .expect("transparent overlap tab creation");
        let lease_id = scene
            .try_grant_lease_for_session_with_budget(
                "transparent-overlap-agent",
                SceneId::nil(),
                60_000,
                ResourceBudget {
                    max_tiles: 50,
                    ..ResourceBudget::default()
                },
            )
            .expect("transparent overlap lease creation");

        let mut control_tile_ids = Vec::with_capacity(48);
        for index in 0..48 {
            let column = index % 8;
            let row = index / 8;
            let bounds = if with_intruding_idle_grip && index == 0 {
                // This tile only touches the lower tile at its bottom edge, so
                // it is not a visual overlap contributor. Its idle grip extends
                // four pixels upward into the lower tile's retained damage.
                Rect::new(100.0, 340.0, 40.0, 50.0)
            } else {
                Rect::new(
                    600.0 + (column * 48) as f32,
                    10.0 + (row * 60) as f32,
                    40.0,
                    50.0,
                )
            };
            let tile_id = scene
                .create_tile(
                    tab_id,
                    "transparent-overlap-agent",
                    lease_id,
                    bounds,
                    index as u32,
                )
                .expect("unrelated control tile creation");
            let control_label = format!("C{index:02}");
            scene
                .set_tile_root(
                    tile_id,
                    retained_plain_text_node(&control_label, 40.0, 50.0),
                )
                .expect("unrelated control text root");
            control_tile_ids.push(tile_id);
        }

        let lower_tile_id = scene
            .create_tile(
                tab_id,
                "transparent-overlap-agent",
                lease_id,
                Rect::new(40.0, 100.0, 400.0, 240.0),
                48,
            )
            .expect("opaque lower tile creation");
        let lower_node = retained_plain_text_node(lower_content, 400.0, 240.0);
        let lower_node_id = lower_node.id;
        scene
            .set_tile_root(lower_tile_id, lower_node)
            .expect("opaque lower text root");

        let upper_tile_id = scene
            .create_tile(
                tab_id,
                "transparent-overlap-agent",
                lease_id,
                Rect::new(240.0, 160.0, 250.0, 150.0),
                49,
            )
            .expect("transparent upper tile creation");
        scene
            .set_tile_root(upper_tile_id, retained_plain_text_node("OV", 250.0, 150.0))
            .expect("transparent upper text root");
        scene
            .update_tile_opacity(upper_tile_id, 0.5, "transparent-overlap-agent")
            .expect("transparent upper opacity");

        drop(scene);
        (
            scene_arc,
            lease_id,
            lower_tile_id,
            lower_node_id,
            upper_tile_id,
            control_tile_ids,
        )
    }

    #[tokio::test]
    async fn transparent_overlap_update_repaints_only_the_changed_tile_and_its_overlap() {
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let config = HeadlessConfig {
            width: 1_000,
            height: 500,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("transparent-overlap-efficiency-test"),
            config_toml: None,
        };
        let pixel_width = config.width as usize;
        let expected_pixel_bytes = pixel_width * config.height as usize * 4;
        let mut runtime = HeadlessRuntime::new(HeadlessConfig {
            width: config.width,
            height: config.height,
            grpc_port: config.grpc_port,
            agents: config.agents.clone(),
            config_toml: config.config_toml.clone(),
        })
        .await
        .expect("primary runtime init");
        let (scene_arc, lease_id, lower_tile_id, lower_node_id, _upper_tile_id, _control_tile_ids) =
            install_transparent_overlap_scene(&runtime, "AB", false).await;

        // Seed the retained snapshot and glyph atlas from a real full frame.
        runtime.render_frame().await;

        {
            let mut scene = scene_arc.lock().await;
            let update = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "transparent-overlap-agent".into(),
                mutations: vec![SceneMutation::UpdateNodeContent {
                    tile_id: lower_tile_id,
                    node_id: lower_node_id,
                    // Keep the update inside the retained glyph-inventory envelope.
                    // WorkCounts does not observe glyphon atlas uploads.
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "BA".into(),
                        bounds: Rect::new(0.0, 0.0, 400.0, 240.0),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            let update_result = scene.apply_batch(&update);
            assert!(
                update_result.applied,
                "transparent-overlap update applies: {update_result:#?}"
            );
        }

        // This independent runtime begins without a retained snapshot, so its
        // post-update render is a fresh full-frame correctness reference.
        let mut reference = HeadlessRuntime::new(config)
            .await
            .expect("reference runtime init");
        let _ = install_transparent_overlap_scene(&reference, "BA", false).await;
        reference.render_frame().await;
        let reference_work = reference
            .compositor
            .take_work_counts()
            .expect("reference frame records work");
        assert!(
            reference_work.full_frame,
            "fresh reference must use the established full renderer"
        );
        assert!(reference_work.layout > 0, "{reference_work:?}");
        assert_eq!((reference_work.raster, reference_work.upload), (0, 0));
        assert_eq!(reference_work.damage_px, 1_000 * 500);
        let reference_pixels = reference.read_pixels();
        assert_eq!(
            reference_pixels.len(),
            expected_pixel_bytes,
            "fresh full-frame reference must expose one RGBA value for every configured pixel"
        );
        drop(reference);

        runtime.render_frame().await;
        let retained_pixels = runtime.read_pixels();
        assert_eq!(
            retained_pixels.len(),
            expected_pixel_bytes,
            "retained path must expose one RGBA value for every configured pixel"
        );
        assert_eq!(
            retained_pixels.len(),
            reference_pixels.len(),
            "the retained/reference pixel oracle must not silently truncate either readback"
        );
        let work = runtime
            .compositor
            .take_work_counts()
            .expect("work counts after lower text update");
        assert!(!work.full_frame, "{work:?}");
        assert_eq!(
            work.tiles_redrawn, 2,
            "only the changed lower tile and its z-higher overlap are repainted"
        );
        assert_eq!(
            work.pixels_damaged,
            400 * 240,
            "damage is the lower tile, not the 1000x500 surface"
        );
        assert_eq!(work.damage_px, work.pixels_damaged);
        assert_eq!(work.layout, 1, "only changed text needs shaping: {work:?}");
        assert_eq!((work.raster, work.upload), (0, 0), "no widget work");
        assert!(runtime.compositor.take_work_counts().is_none());

        let mut differing_pixels = 0usize;
        let mut first_difference = None;
        let mut difference_bounds: Option<(usize, usize, usize, usize)> = None;
        let mut difference_rows = std::collections::BTreeMap::<usize, (usize, usize, usize)>::new();
        for (pixel_index, (retained, reference)) in retained_pixels
            .chunks_exact(4)
            .zip(reference_pixels.chunks_exact(4))
            .enumerate()
        {
            if retained != reference {
                differing_pixels += 1;
                let x = pixel_index % pixel_width;
                let y = pixel_index / pixel_width;
                difference_bounds = Some(match difference_bounds {
                    Some((min_x, min_y, max_x, max_y)) => {
                        (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                    }
                    None => (x, y, x, y),
                });
                difference_rows
                    .entry(y)
                    .and_modify(|(min_x, max_x, count)| {
                        *min_x = (*min_x).min(x);
                        *max_x = (*max_x).max(x);
                        *count += 1;
                    })
                    .or_insert((x, x, 1));
                first_difference.get_or_insert((
                    pixel_index,
                    [retained[0], retained[1], retained[2], retained[3]],
                    [reference[0], reference[1], reference[2], reference[3]],
                ));
            }
        }
        assert!(
            first_difference.is_none(),
            "the retained z-ordered overlap repair must equal a fresh full-frame post-update render; \
             first difference at pixel {first_difference:?}, bounds {difference_bounds:?}, \
             rows {difference_rows:?}, of {differing_pixels} pixels"
        );
    }

    #[tokio::test]
    async fn transparent_overlap_rejects_nonclosure_idle_grip_that_crosses_damage() {
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(HeadlessConfig {
            width: 1_000,
            height: 500,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("transparent-overlap-intruding-grip-test"),
            config_toml: None,
        })
        .await
        .expect("runtime init");
        let (scene_arc, lease_id, lower_tile_id, lower_node_id, _upper_tile_id, _control_tile_ids) =
            install_transparent_overlap_scene(&runtime, "AB", true).await;

        // Seed the private snapshot from the real full frame; the adjacent tile
        // is allowed in the scene because its body does not overlap the lower
        // tile. Only its chrome grip crosses the retained damage boundary.
        runtime.render_frame().await;

        {
            let mut scene = scene_arc.lock().await;
            let update = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "transparent-overlap-agent".into(),
                mutations: vec![SceneMutation::UpdateNodeContent {
                    tile_id: lower_tile_id,
                    node_id: lower_node_id,
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "BA".into(),
                        bounds: Rect::new(0.0, 0.0, 400.0, 240.0),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            assert!(
                scene.apply_batch(&update).applied,
                "intruding-grip update applies"
            );
        }

        runtime.render_frame().await;

        let work = runtime
            .compositor
            .take_work_counts()
            .expect("fallback frame records work");
        assert!(
            work.full_frame,
            "a non-closure grip crossing retained damage must fall back to a full frame"
        );
        // Retained preparation can shape before discovering the intruding grip.
        // The observation includes that work and the full-frame retry.
        assert!((1..=51).contains(&work.layout), "{work:?}");
        assert_eq!((work.raster, work.upload), (0, 0));
        assert_eq!(work.damage_px, 1_000 * 500);
        assert_eq!(work.damage_px, work.pixels_damaged);
    }

    #[tokio::test]
    async fn canonical_one_node_update_repaints_only_the_changed_tile() {
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(HeadlessConfig {
            width: 1_000,
            height: 500,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("change-efficiency-test"),
            config_toml: None,
        })
        .await
        .expect("runtime init");

        let (scene_arc, lease_id, changed_tile_id, changed_node_id) = {
            let shared_state = runtime.shared_state().lock().await;
            let scene_arc = Arc::clone(&shared_state.scene);
            drop(shared_state);
            let mut scene = scene_arc.lock().await;
            let tab_id = scene.create_tab("Canonical", 0).expect("tab creation");
            let lease_id = scene
                .try_grant_lease_for_session_with_budget(
                    "change-efficiency-agent",
                    SceneId::nil(),
                    60_000,
                    ResourceBudget {
                        max_tiles: 50,
                        ..ResourceBudget::default()
                    },
                )
                .expect("canonical lease creation");
            let mut changed = None;
            for row in 0..5 {
                for column in 0..10 {
                    let index = row * 10 + column;
                    // Leave enough vertical clearance that the next row's
                    // idle grip cannot enter this tile's scoped damage. The
                    // retained proof is intentionally closure-only for chrome.
                    let tile_width = 96.0;
                    let tile_height = 92.0;
                    let tile_id = scene
                        .create_tile(
                            tab_id,
                            "change-efficiency-agent",
                            lease_id,
                            Rect::new(
                                (column * 100) as f32,
                                (row * 100) as f32,
                                tile_width,
                                tile_height,
                            ),
                            index,
                        )
                        .expect("canonical tile creation");
                    let node = Node {
                        id: SceneId::new(),
                        children: vec![],
                        layout: Default::default(),
                        data: NodeData::TextMarkdown(TextMarkdownNode {
                            // The update below preserves this glyph inventory,
                            // so an observed zero-upload capture is meaningful.
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
                    };
                    if index == 0 {
                        changed = Some((tile_id, node.id));
                    }
                    scene
                        .set_tile_root(tile_id, node)
                        .expect("canonical text root");
                }
            }
            let (changed_tile_id, changed_node_id) = changed.expect("first tile exists");
            drop(scene);
            (scene_arc, lease_id, changed_tile_id, changed_node_id)
        };

        // Baseline: real full headless render that seeds the private retained
        // snapshot and glyph atlas. It is deliberately not the evidence frame.
        runtime.render_frame().await;
        let baseline_pixels = runtime.read_pixels();

        {
            let mut scene = scene_arc.lock().await;
            let update = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "change-efficiency-agent".into(),
                mutations: vec![SceneMutation::UpdateNodeContent {
                    tile_id: changed_tile_id,
                    node_id: changed_node_id,
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "BA".into(),
                        bounds: Rect::new(0.0, 0.0, 96.0, 92.0),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            let update_result = scene.apply_batch(&update);
            assert!(
                update_result.applied,
                "UpdateNodeContent applies: {update_result:#?}"
            );
        }

        // Evidence frame: the normal runtime API must select the retained
        // compositor path; this test never manufactures an artifact.
        runtime.render_frame().await;
        let updated_pixels = runtime.read_pixels();
        let work = runtime
            .compositor
            .take_work_counts()
            .expect("work counts after UpdateNodeContent");
        assert!(!work.full_frame, "{work:?}");
        assert_eq!(work.tiles_redrawn, 1);
        assert_eq!(work.pixels_damaged, 96 * 92, "damage is the changed tile");
        assert_eq!(work.damage_px, work.pixels_damaged);
        assert_eq!(work.layout, 1, "one changed text shape: {work:?}");
        assert_eq!((work.raster, work.upload), (0, 0), "no widget work");
        assert!(runtime.compositor.take_work_counts().is_none());

        // Read back both submitted surfaces: LoadOp::Load plus the scoped
        // scissors must preserve every pixel outside the changed tile and must
        // visibly update at least one pixel inside it.
        // The changed tile is the first tile at the origin.
        let damage = (0_u32, 0_u32, 96_u32, 92_u32);
        let mut changed_pixels_inside_damage = 0_u64;
        for y in 0..500 {
            for x in 0..1_000 {
                let offset = ((y * 1_000 + x) * 4) as usize;
                let before = &baseline_pixels[offset..offset + 4];
                let after = &updated_pixels[offset..offset + 4];
                let inside_damage = x >= damage.0
                    && x < damage.0 + damage.2
                    && y >= damage.1
                    && y < damage.1 + damage.3;
                if inside_damage {
                    changed_pixels_inside_damage += u64::from(before != after);
                } else {
                    assert_eq!(
                        before, after,
                        "retained update changed pixel outside damage at ({x}, {y})"
                    );
                }
            }
        }
        assert!(
            changed_pixels_inside_damage > 0,
            "retained update did not visibly change its declared damage region"
        );

        // A full-frame baseline rendered under degradation draws differently.
        // The retained path must decline and invalidate its snapshot rather than
        // paint a nominal-quality update onto that baseline.
        for _ in 0..40 {
            runtime.degradation_controller.record_frame(20_000);
        }
        assert_eq!(
            runtime.degradation_controller.level(),
            crate::degradation::DegradationLevel::Simplified,
            "the real runtime controller must produce the simplified policy"
        );
        {
            let mut scene = scene_arc.lock().await;
            let suppressed_update = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "change-efficiency-agent".into(),
                mutations: vec![SceneMutation::UpdateNodeContent {
                    tile_id: changed_tile_id,
                    node_id: changed_node_id,
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "AB".into(),
                        bounds: Rect::new(0.0, 0.0, 96.0, 92.0),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            assert!(
                scene.apply_batch(&suppressed_update).applied,
                "suppressed canonical update still applies to the scene"
            );
        }
        runtime.render_frame().await;
        let degraded_work = runtime
            .compositor
            .take_work_counts()
            .expect("degraded frame records work");
        assert!(
            degraded_work.full_frame,
            "degraded frames must never take the retained path"
        );
        assert!(degraded_work.layout > 0, "{degraded_work:?}");
        assert_eq!((degraded_work.raster, degraded_work.upload), (0, 0));
        assert_eq!(degraded_work.damage_px, 1_000 * 500);
        runtime.degradation_controller = DegradationController::with_defaults();
        runtime.render_frame().await;
        let rebaseline_work = runtime
            .compositor
            .take_work_counts()
            .expect("rebaseline frame records work");
        assert!(
            rebaseline_work.full_frame,
            "returning to the canonical policy requires a fresh full-frame baseline"
        );
        assert_eq!((rebaseline_work.raster, rebaseline_work.upload), (0, 0));
        assert_eq!(rebaseline_work.damage_px, 1_000 * 500);

        // A canonical scene change that leaves the deliberately narrow proof
        // envelope (new glyph inventory) must be a structured, non-passing
        // full-frame diagnostic instead of a silent proportionality claim.
        {
            let mut scene = scene_arc.lock().await;
            let unsupported_update = MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "change-efficiency-agent".into(),
                mutations: vec![SceneMutation::UpdateNodeContent {
                    tile_id: changed_tile_id,
                    node_id: changed_node_id,
                    data: NodeData::TextMarkdown(TextMarkdownNode {
                        content: "AC".into(),
                        bounds: Rect::new(0.0, 0.0, 96.0, 92.0),
                        font_size_px: 18.0,
                        font_family: FontFamily::SystemSansSerif,
                        color: Rgba::WHITE,
                        background: None,
                        alignment: TextAlign::Start,
                        overflow: TextOverflow::Clip,
                        color_runs: Box::default(),
                    }),
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            };
            assert!(
                scene.apply_batch(&unsupported_update).applied,
                "unsupported canonical update still applies to the scene"
            );
        }
        runtime.render_frame().await;
        let unsupported_work = runtime
            .compositor
            .take_work_counts()
            .expect("unsupported change records work");
        assert!(
            unsupported_work.full_frame,
            "a change outside the retained envelope falls back to a full frame"
        );
        assert!(unsupported_work.layout > 0, "{unsupported_work:?}");
        assert_eq!((unsupported_work.raster, unsupported_work.upload), (0, 0));
        assert_eq!(unsupported_work.damage_px, 1_000 * 500);
    }

    /// Verify that grpc_port = 0 does not start a server by default.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_headless_runtime_no_grpc() {
        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");
        // render_frame should succeed without a gRPC server
        let telemetry = runtime.render_frame().await;
        assert!(telemetry.frame_time_us > 0, "frame time must be non-zero");
    }

    /// An orphaned lease is reclaimed by the frame sweep once its grace ends,
    /// with no agent help (invariant 4) — headless runs the same sweep as the
    /// windowed compositor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn render_frame_reclaims_orphaned_lease_after_grace() {
        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");
        let clock = tze_hud_scene::TestClock::new(1_000);
        let publication_origin = SceneId::new();
        let (lease_id, tile_id) = {
            let state = runtime.shared_state().lock().await;
            let mut scene = state.scene.lock().await;
            *scene = SceneGraph::new_with_clock(64.0, 64.0, Arc::new(clock.clone()));
            let tab_id = scene.create_tab("Main", 0).expect("tab");
            let lease_id = scene.grant_lease("agent", 600_000);
            let tile_id = scene
                .create_tile(
                    tab_id,
                    "agent",
                    lease_id,
                    Rect::new(0.0, 0.0, 32.0, 32.0),
                    1,
                )
                .expect("tile");
            let now_ms = scene.now_millis();
            scene
                .disconnect_lease(&lease_id, now_ms)
                .expect("orphan the lease");
            scene.zone_registry = tze_hud_scene::ZoneRegistry::with_defaults();
            let batch = tze_hud_scene::mutation::MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: "publication-only".into(),
                mutations: vec![tze_hud_scene::mutation::SceneMutation::PublishToZone {
                    zone_name: "subtitle".into(),
                    content: tze_hud_scene::ZoneContent::StreamText("held".into()),
                    publish_token: tze_hud_scene::ZonePublishToken { token: Vec::new() },
                    merge_key: None,
                    expires_at_wall_us: None,
                    content_classification: None,
                    breakpoints: Vec::new(),
                    held: false,
                }],
                timing_hints: None,
                lease_id: None,
            };
            assert!(
                scene
                    .apply_batch_for_origin(&batch, Some(publication_origin))
                    .applied
            );
            scene.orphan_publications(
                publication_origin,
                now_ms,
                SceneGraph::DEFAULT_GRACE_PERIOD_MS,
            );
            (lease_id, tile_id)
        };

        clock.advance(SceneGraph::DEFAULT_GRACE_PERIOD_MS - 1);
        runtime.render_frame().await;
        {
            let state = runtime.shared_state().lock().await;
            let scene = state.scene.lock().await;
            assert!(scene.tiles.contains_key(&tile_id), "kept within grace");
            assert_eq!(scene.zone_registry.active_for_zone("subtitle").len(), 1);
        }

        clock.advance(1);
        runtime.render_frame().await;
        let state = runtime.shared_state().lock().await;
        let scene = state.scene.lock().await;
        assert!(!scene.tiles.contains_key(&tile_id), "tile reclaimed");
        assert!(scene.zone_registry.active_for_zone("subtitle").is_empty());
        assert_eq!(
            scene.next_lease_deadline_ms(SceneGraph::DEFAULT_MAX_SUSPENSION_MS),
            None
        );
        assert!(
            scene
                .leases
                .get(&lease_id)
                .is_none_or(|lease| lease.state.is_terminal()),
            "lease reclaimed"
        );
    }

    /// Verify that read_pixels returns actual rendered content after render_frame().
    ///
    /// Regression test for the bug where render_frame() called compositor.render_frame()
    /// instead of render_frame_headless(), causing copy_to_buffer to be skipped and
    /// read_pixels() to return stale/zero data.
    ///
    /// The clear color in render_frame_headless is (r:0.05, g:0.05, b:0.1, a:1.0) in
    /// linear space, which in Rgba8UnormSrgb encoding is approximately (48, 48, 80, 255).
    /// The alpha channel MUST be 255 (fully opaque). Any all-zero pixel data indicates
    /// the copy_to_buffer step was skipped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_headless_read_pixels_content_after_render() {
        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");
        runtime.render_frame().await;
        let pixels = runtime.read_pixels();

        assert_eq!(
            pixels.len(),
            64 * 64 * 4,
            "pixel buffer must be width * height * 4 bytes"
        );

        // Verify pixels are not all zero — all-zero means copy_to_buffer was skipped
        // (the bug this test guards against: using render_frame instead of render_frame_headless).
        let all_zero = pixels.iter().all(|&b| b == 0);
        assert!(
            !all_zero,
            "read_pixels() returned all-zero data: copy_to_buffer was likely skipped. \
             render_frame() must call render_frame_headless() for correct pixel readback."
        );

        // Verify alpha channel is 255 (fully opaque clear color).
        // RGBA8 layout: [R, G, B, A, R, G, B, A, ...]
        let alpha_nonopaque = pixels.chunks(4).any(|px| px[3] != 255);
        assert!(
            !alpha_nonopaque,
            "expected fully-opaque pixels (alpha=255) from clear color render, got non-255 alpha"
        );
    }

    /// Verify that the gRPC server starts and the runtime remains functional
    /// after startup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_headless_grpc_server_starts() {
        // Keep the actual IPv6 loopback listener owned through construction
        // and move it directly into the shared real gRPC serving path.
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: addr.port(),
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let mut runtime = HeadlessRuntime::new(config).await.expect("runtime init");
        assert!(
            runtime.start_grpc_server().await.is_err(),
            "the public wrapper must reject the occupied configured port"
        );
        runtime.config.grpc_port = 0;
        assert!(
            runtime.start_grpc_server().await.is_err(),
            "the public wrapper must keep port zero disabled"
        );
        runtime.config.grpc_port = addr.port();
        let _server = runtime.start_grpc_server_on_listener(listener);
        tze_hud_protocol::proto::session::hud_session_client::HudSessionClient::connect(format!(
            "http://{addr}"
        ))
        .await
        .expect("the real IPv6 gRPC server must accept a transport connection");

        // Render a frame while the server is running
        let telemetry = runtime.render_frame().await;
        assert!(telemetry.frame_time_us > 0);

        // Server task is still running (not panicked)
        assert!(
            !_server.is_finished(),
            "gRPC server task should still be running"
        );
        _server.abort();
        assert!(_server.await.unwrap_err().is_cancelled());
    }

    /// Verify that config_toml = None produces a headless-default RuntimeContext.
    ///
    /// This path is only reachable under cfg(test) or the `dev-mode` feature;
    /// in production builds, `build_runtime_context()` returns `Err` when
    /// `config_toml` is `None`.
    #[test]
    fn test_build_runtime_context_none_is_headless_default() {
        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let ctx = config
            .build_runtime_context()
            .expect("build_runtime_context with None should succeed under cfg(test)");
        assert_eq!(ctx.profile.name, "headless");
    }

    /// Verify that design tokens from config_toml are applied to the compositor
    /// when HeadlessRuntime is initialized.
    ///
    /// When config_toml contains a [design_tokens] section, the HeadlessRuntime
    /// must call run_scene_startup and compositor.set_token_map so that
    /// token-driven properties (e.g. severity colors for alert-banner) are
    /// resolved at render time rather than falling back to hardcoded constants.
    ///
    /// Regression test for hud-kz2l: HeadlessRuntime was not calling
    /// run_scene_startup, so design tokens were never applied even when
    /// config_toml with [design_tokens] was supplied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_design_tokens_applied_when_config_toml_provided() {
        let toml = r##"
[runtime]
profile = "headless"

[[tabs]]
name = "Main"
default_tab = true

[design_tokens]
"color.text.primary" = "#FF0000"
"color.severity.warning" = "#00FF00"
"color.backdrop.default" = "#0000FF"
"opacity.backdrop.default" = "0.5"
"stroke.outline.width" = "2.0"
"color.outline.default" = "#FFFF00"
"typography.subtitle.size" = "18"
"typography.subtitle.weight" = "600"
"typography.subtitle.family" = "system-ui"
"typography.body.size" = "14"
"typography.body.weight" = "400"
"typography.body.family" = "system-ui"
"spacing.padding.medium" = "8"
"spacing.padding.large" = "16"
"typography.status.size" = "12"
"typography.status.weight" = "400"
"typography.status.family" = "system-ui"
"typography.alert.size" = "16"
"typography.alert.weight" = "700"
"typography.alert.family" = "system-ui"
"typography.notification.size" = "14"
"typography.notification.weight" = "500"
"typography.notification.family" = "system-ui"
"color.text.muted" = "#AAAAAA"
"opacity.text.muted" = "0.7"
"color.accent.primary" = "#3399FF"
"color.accent.secondary" = "#33FF99"
"color.surface.primary" = "#1A1A2E"
"color.surface.secondary" = "#16213E"
"color.border.default" = "#444466"
"##;

        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: Some(toml.to_string()),
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let runtime = HeadlessRuntime::new(config).await.expect("runtime init");

        // The compositor's token_map must contain the tokens from config_toml.
        // color.text.primary was set to "#FF0000" in the config.
        assert_eq!(
            runtime
                .compositor
                .token_map
                .get("color.text.primary")
                .map(String::as_str),
            Some("#FF0000"),
            "compositor token_map should contain color.text.primary from config_toml"
        );
        // color.severity.warning was set to "#00FF00".
        assert_eq!(
            runtime
                .compositor
                .token_map
                .get("color.severity.warning")
                .map(String::as_str),
            Some("#00FF00"),
            "compositor token_map should contain color.severity.warning from config_toml"
        );

        // The scene's zone_registry must contain token-derived rendering policies.
        // Verify that the subtitle zone got a text_color derived from color.text.primary (#FF0000).
        let state = runtime.state.lock().await;
        let scene = state.scene.lock().await;
        let subtitle_zone = scene
            .zone_registry
            .zones
            .get("subtitle")
            .expect("subtitle zone should be registered");
        let text_color = subtitle_zone
            .rendering_policy
            .text_color
            .expect("subtitle zone should have token-derived text_color after component startup");
        // #FF0000 → R=1.0, G=0.0, B=0.0
        assert!(
            (text_color.r - 1.0).abs() < 1e-3,
            "subtitle text_color.r should be 1.0 for #FF0000, got {}",
            text_color.r
        );
        assert!(
            text_color.g < 1e-3,
            "subtitle text_color.g should be 0.0 for #FF0000, got {}",
            text_color.g
        );
        assert!(
            text_color.b < 1e-3,
            "subtitle text_color.b should be 0.0 for #FF0000, got {}",
            text_color.b
        );
    }

    /// Verify that when config_toml is None, the compositor token_map is empty
    /// (no design tokens, canonical zone defaults used).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_no_design_tokens_when_no_config_toml() {
        let config = HeadlessConfig {
            width: 64,
            height: 64,
            grpc_port: 0,
            agents: AgentDirectory::unrestricted("test"),
            config_toml: None,
        };
        let _runtime_guard = crate::test_support::lock_headless_runtime().await;
        let runtime = HeadlessRuntime::new(config).await.expect("runtime init");

        // Without config_toml, the compositor token_map should be empty.
        assert!(
            runtime.compositor.token_map.is_empty(),
            "compositor token_map should be empty when config_toml is None"
        );
    }
}
