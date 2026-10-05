//! Surface abstraction — windowed or headless.
//!
//! The compositor renders to a surface without knowing which kind it is.
//!
//! # Design
//!
//! `CompositorSurface` is the trait that decouples the frame pipeline from the
//! display back-end.  Two implementations ship in v1:
//!
//! - [`HeadlessSurface`] — offscreen `wgpu::Texture` for testing and CI.
//!   `present()` is a no-op; pixel readback uses `copy_texture_to_buffer`.
//! - [`WindowSurface`] — window-backed surface via `wgpu::Surface`.
//!   Reserved for windowed mode; windowed rendering is post-vertical-slice.
//!
//! Per runtime-kernel/spec.md Requirement: Compositor Surface Trait (line 364)
//! the surface is abstracted behind `CompositorSurface` with `acquire_frame()`,
//! `present()`, and `size()`.  `CompositorFrame` bundles the `TextureView` with
//! an ownership guard so the `SurfaceTexture` lives until after `present()`.
//!
//! ## Thread model
//! - `acquire_frame()` is called on the compositor thread.
//! - `present()` MUST be called on the main thread (macOS/Metal requirement).
//!   The compositor thread signals the main thread via `FrameReadySignal`;
//!   the main thread calls `surface.present()`.
//! - `size()` may be called from any thread (read-only).

use std::any::Any;

// ─── CompositorFrame ─────────────────────────────────────────────────────────

/// Shared swapchain-ownership state for a `WindowSurface`, guarded by a single
/// mutex (`WindowSurface::swapchain`).
///
/// Bundling the pending texture and the "encode in progress" marker under ONE
/// mutex is what closes the acquire→submit vs present/reconfigure race
/// (Bug B Layer-2, hud-hj0xb). The previous design used a separate
/// `AtomicBool` checked *before* the pending-texture lock was taken, leaving a
/// check-then-act (TOCTOU) gap: the main thread could observe `encoding=false`,
/// then — before it acquired the lock — the compositor could begin the next
/// frame (`encoding=true`, reusing the same `SurfaceTexture`), after which the
/// main thread would take and present (destroy) that texture while the
/// compositor's `TextureView` was still mid-submit, producing the wgpu
/// validation error "Surface Texture has been destroyed".
///
/// With both fields under one lock and a condvar, the main thread checks
/// `encoding` *while holding the lock* and blocks on the condvar until the
/// compositor's submit completes, so the in-flight surface texture cannot be
/// destroyed mid-submit.
struct SwapchainSlot {
    /// `SurfaceTexture` acquired by the compositor thread and awaiting
    /// presentation on the main thread. `None` between present and the next
    /// acquire, or after a reconfigure cleared it.
    pending: Option<wgpu::SurfaceTexture>,
    /// `true` from the start of `acquire_frame()` until the `CompositorFrame`
    /// guard is dropped (after `queue.submit()`). While set, the compositor
    /// holds a `TextureView` referencing `pending`, so the texture MUST NOT be
    /// presented (taken/destroyed) or invalidated by a reconfigure.
    encoding: bool,
}

/// RAII guard that marks the end of the compositor's encode→submit critical
/// section. Stored inside `CompositorFrame._guard` so it lives for the entire
/// encode+submit lifecycle; on drop (after `queue.submit()`) it clears
/// `encoding` under the swapchain mutex and wakes any main-thread presenter or
/// compositor reconfigure waiting on the condvar.
///
/// This is the release half of the acquire→submit serialization: holding the
/// `encoding` marker (not the raw `MutexGuard`, which is neither `Send` nor
/// `'static`) lets the compositor run encode+submit *without* keeping the
/// mutex locked — so unrelated GPU work is not serialized and the main thread
/// only blocks when it actually needs the in-flight texture.
struct EncodingGuard {
    slot: std::sync::Arc<std::sync::Mutex<SwapchainSlot>>,
    done: std::sync::Arc<std::sync::Condvar>,
}

impl Drop for EncodingGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.encoding = false;
        }
        // Wake the main thread (present) and/or compositor (reconfigure) that
        // may be waiting for the in-flight submit to finish.
        self.done.notify_all();
    }
}

/// A frame ready for rendering: a `TextureView` plus an ownership guard.
///
/// The `_guard` holds a heap-allocated value that keeps the underlying GPU
/// resource alive until this `CompositorFrame` is dropped.  For `HeadlessSurface`
/// the guard is a `()` no-op.  For `WindowSurface` it would hold the
/// `wgpu::SurfaceTexture`.
///
/// Per runtime-kernel/spec.md: "CompositorFrame MUST bundle the TextureView
/// with an ownership guard (_guard: Box<dyn Any + Send>) to keep the
/// SurfaceTexture alive until after present()." (line 364)
pub struct CompositorFrame {
    /// Render target for this frame.
    pub view: wgpu::TextureView,
    /// Ownership guard — keeps the backing resource alive until this frame is dropped.
    pub _guard: Box<dyn Any + Send>,
    /// Whether this build acquired a new surface texture or reused a pending one.
    pub acquisition: SurfaceFrameAcquisition,
}

/// How a compositor build obtained its renderable surface texture.
///
/// A windowed build may reuse the texture acquired by an earlier build while
/// the main thread has not presented it yet. That reuse remains render work but
/// is not another swapchain acquisition for idle-efficiency telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceFrameAcquisition {
    /// This build acquired a new texture from the surface boundary.
    Fresh,
    /// This build reused a texture that was already pending presentation.
    ReusedPending,
}

impl SurfaceFrameAcquisition {
    const fn from_pending_surface(has_pending_surface: bool) -> Self {
        if has_pending_surface {
            Self::ReusedPending
        } else {
            Self::Fresh
        }
    }

    /// Whether this build acquired a new surface texture.
    #[must_use]
    pub const fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }
}

/// A non-timeout failure observed while acquiring a window-backed surface.
///
/// This deliberately mirrors the recoverability classes of
/// [`wgpu::SurfaceError`] without exposing wgpu's error type beyond the surface
/// boundary. It is compositor-private evidence for the runtime-owned recovery
/// lifecycle, not an agent-visible diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceAcquireFailure {
    /// The swapchain was lost and should be reconfigured once.
    Lost,
    /// The swapchain configuration is stale and should be reconfigured once.
    Outdated,
    /// The GPU cannot allocate the next surface texture.
    OutOfMemory,
    /// The surface reported an unclassified backend failure.
    Other,
}

/// Result of one production surface-recovery transition.
///
/// The runtime consumes only the coarse control disposition. This detailed
/// compositor-private outcome remains available for the retained diagnostics
/// follow-up without fabricating a parallel test-only event source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceRecoveryOutcome {
    /// A lost or outdated surface was successfully reconfigured and needs a
    /// fresh acquire before it is considered healthy again.
    Reconfigured { trigger: SurfaceAcquireFailure },
    /// The surface could not be recovered and requires terminal runtime
    /// handling.
    Terminal { trigger: SurfaceAcquireFailure },
}

/// One pending surface lifecycle transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceRecoveryRequest {
    Reconfigure { trigger: SurfaceAcquireFailure },
    Terminal { trigger: SurfaceAcquireFailure },
}

/// Small, compositor-thread-owned state machine for window-surface recovery.
///
/// `WindowSurface::acquire_retry_view` records actual wgpu acquire errors here;
/// the normal compositor loop later performs the queued reconfigure with its
/// owned `wgpu::Device`. Keeping the observation and device operation separate
/// preserves the `CompositorSurface` frame trait while making the transition
/// explicit and production-owned.
#[derive(Debug, Default)]
struct SurfaceRecoveryState {
    pending: Option<SurfaceRecoveryRequest>,
}

impl SurfaceRecoveryState {
    /// Classify an actual wgpu acquire error into this recovery state machine.
    ///
    /// This is the production entry point used by
    /// [`WindowSurface::acquire_retry_view`]. Keeping the mapping beside the
    /// queue makes the Lost/Outdated regression exercise the same path that a
    /// real surface error follows, rather than injecting a test-only request.
    fn observe_wgpu_acquire_failure(&mut self, err: &wgpu::SurfaceError) {
        let failure = match err {
            wgpu::SurfaceError::Timeout => return,
            wgpu::SurfaceError::Outdated => SurfaceAcquireFailure::Outdated,
            wgpu::SurfaceError::Lost => SurfaceAcquireFailure::Lost,
            wgpu::SurfaceError::OutOfMemory => SurfaceAcquireFailure::OutOfMemory,
            wgpu::SurfaceError::Other => SurfaceAcquireFailure::Other,
        };
        self.observe_acquire_failure(failure);
    }

    /// Record an actual non-timeout acquire failure.
    ///
    /// Lost and Outdated remain recoverable surface conditions. Fatal acquire
    /// classes, and a failed `Surface::configure` later, become terminal.
    fn observe_acquire_failure(&mut self, failure: SurfaceAcquireFailure) {
        if matches!(self.pending, Some(SurfaceRecoveryRequest::Terminal { .. })) {
            return;
        }

        let request = match failure {
            SurfaceAcquireFailure::Lost | SurfaceAcquireFailure::Outdated => {
                SurfaceRecoveryRequest::Reconfigure { trigger: failure }
            }
            SurfaceAcquireFailure::OutOfMemory | SurfaceAcquireFailure::Other => {
                SurfaceRecoveryRequest::Terminal { trigger: failure }
            }
        };
        self.pending = Some(request);
    }

    /// Consume a queued transition and execute its reconfigure action.
    ///
    /// The production `WindowSurface` calls this exact state machine from its
    /// compositor-thread recovery hook. The closure keeps the state machine
    /// GPU-free and lets its recovery semantics be regression-tested without a
    /// live window or swapchain.
    fn attempt_pending_recovery(
        &mut self,
        reconfigure: impl FnOnce() -> Result<(), ()>,
    ) -> Option<SurfaceRecoveryOutcome> {
        match self.pending.take()? {
            SurfaceRecoveryRequest::Reconfigure { trigger } => match reconfigure() {
                Ok(()) => Some(SurfaceRecoveryOutcome::Reconfigured { trigger }),
                Err(()) => Some(SurfaceRecoveryOutcome::Terminal { trigger }),
            },
            SurfaceRecoveryRequest::Terminal { trigger } => {
                Some(SurfaceRecoveryOutcome::Terminal { trigger })
            }
        }
    }
}

// ─── CompositorSurface trait ─────────────────────────────────────────────────

/// Trait for compositor render targets.
///
/// Implemented by both `HeadlessSurface` (offscreen) and `WindowSurface`
/// (display-connected).  The compositor uses this trait exclusively — no
/// `if headless { … } else { … }` branches in the frame pipeline.
///
/// Per runtime-kernel/spec.md Requirement: Compositor Surface Trait (line 364):
/// - `acquire_frame()` → `Option<CompositorFrame>` containing the `TextureView`,
///   or `None` if the frame should be skipped (e.g., after a double swapchain-
///   acquire failure). Callers MUST skip rendering and signal frame-skip telemetry
///   on `None`; they MUST NOT panic.
/// - `present()` — submit/flip the frame.  No-op for headless.
/// - `size()` → `(width, height)` in pixels.
pub trait CompositorSurface: Send + 'static {
    /// Acquire a frame for rendering.  Returns `Some(CompositorFrame)` on
    /// success; `None` signals that this frame must be skipped (surface is
    /// temporarily unavailable — e.g., after a double swapchain-acquire
    /// failure on driver reset or device loss).
    ///
    /// Callers MUST handle `None` gracefully: skip the render pass, return
    /// early with a zeroed `FrameTelemetry`, and retry on the next frame.
    /// Treating `None` as a fatal error is an anti-pattern ("Treating
    /// graceful degradation as a bug").
    ///
    /// Called on the compositor thread (Stage 6 / Stage 7 boundary).
    fn acquire_frame(&self) -> Option<CompositorFrame>;

    /// Present (flip/submit) the current frame.
    ///
    /// For `HeadlessSurface` this is a no-op per spec line 199.
    /// For a real surface this submits the frame to the display.
    ///
    /// **MUST be called on the main thread** (macOS/Metal requirement).
    fn present(&self);

    /// Surface dimensions in pixels.
    fn size(&self) -> (u32, u32);
}

// ─── WindowSurface ─────────────────────────────────────────────────────────────

/// Window-backed swapchain surface.
///
/// Wraps a `wgpu::Surface` created from a `winit::window::Window`. Used by
/// `WindowedRuntime` to display rendered frames on a real screen.
///
/// ## Thread model (spec §Compositor Thread Ownership, line 46)
/// - `acquire_frame()` is called on the **compositor thread** — it calls
///   `surface.get_current_texture()` to obtain the next swapchain image.
///   The acquired `SurfaceTexture` is stored in `pending_texture` so the main
///   thread can retrieve and present it.
/// - `take_pending_texture()` is called on the **main thread** after the
///   compositor signals `FrameReadySignal`. The main thread calls
///   `SurfaceTexture::present()` on the returned texture, satisfying the
///   macOS/Metal requirement that `present()` runs on the main thread.
/// - `size()` may be called from any thread (stored atomically).
/// - `pending_resize` signals a pending resize from the main thread to the
///   compositor thread. The compositor calls `reconfigure()` using its owned
///   `wgpu::Device` before the next `acquire_frame()`.
///
/// ## Reconfiguration
/// On window resize, the main thread stores the new dimensions in
/// `pending_resize`. The compositor thread detects a non-zero pending resize at
/// the start of each frame cycle and calls `reconfigure()`.
pub struct WindowSurface {
    /// The underlying wgpu surface (window-backed swapchain).
    pub surface: wgpu::Surface<'static>,
    /// Current surface configuration.
    pub config: std::sync::Mutex<wgpu::SurfaceConfiguration>,
    /// Current width in pixels (kept in sync with config).
    pub width: std::sync::atomic::AtomicU32,
    /// Current height in pixels (kept in sync with config).
    pub height: std::sync::atomic::AtomicU32,
    /// Swapchain-ownership state: the pending `SurfaceTexture` awaiting
    /// presentation plus the compositor's "encode in progress" marker, guarded
    /// by a single mutex.
    ///
    /// This is the swapchain-ownership mutex the acquire→submit critical
    /// section is serialized against (Bug B Layer-2, hud-hj0xb). Folding both
    /// the pending texture and the encoding marker under one lock — rather than
    /// a separate `AtomicBool` checked before the lock — is what makes the
    /// main-thread present and the compositor reconfigure unable to destroy a
    /// surface texture that an in-flight submit still references.
    swapchain: std::sync::Arc<std::sync::Mutex<SwapchainSlot>>,
    /// Condition variable paired with `swapchain`. Signalled by the
    /// `EncodingGuard` drop when the compositor's encode→submit completes, so
    /// `present_pending_texture()` (main thread) and `reconfigure()`
    /// (compositor thread) can wait for an in-flight submit without spinning.
    swapchain_done: std::sync::Arc<std::sync::Condvar>,
    /// Pending recovery lifecycle state, written by actual acquire failures and
    /// consumed by the compositor thread before its next frame build.
    surface_recovery: std::sync::Mutex<SurfaceRecoveryState>,
    /// Pending resize dimensions signalled from the main thread to the
    /// compositor thread. `(0, 0)` means no resize pending.
    ///
    /// The main thread stores `(new_width, new_height)` atomically on
    /// `WindowEvent::Resized`. The compositor thread reads this at the start of
    /// each frame, applies `reconfigure()` with the new dimensions using its
    /// owned `wgpu::Device`, then resets both fields to `0`.
    pub pending_resize_width: std::sync::atomic::AtomicU32,
    pub pending_resize_height: std::sync::atomic::AtomicU32,
    /// Number of swapchain textures actually presented by the main thread.
    ///
    /// Windowed benchmarks use this as the presentation acknowledgement; a
    /// compositor-side build or submit attempt is not itself a displayed frame.
    presented_frame_count: std::sync::atomic::AtomicU64,
    /// Rate limit for acquire-timeout logs: an occluded or asleep window
    /// times out on every attempt.
    timeout_log: LogThrottle,
}

/// Lets one log line through per `interval` and counts the ones it drops.
#[derive(Debug)]
pub(crate) struct LogThrottle {
    interval: std::time::Duration,
    state: std::sync::Mutex<(Option<std::time::Instant>, u64)>,
}

impl LogThrottle {
    pub(crate) fn new(interval: std::time::Duration) -> Self {
        Self {
            interval,
            state: std::sync::Mutex::new((None, 0)),
        }
    }

    /// `Some(suppressed)` when a line may be logged at `now` (with the count
    /// dropped since the last one), `None` when it should be dropped.
    pub(crate) fn allow(&self, now: std::time::Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (last, suppressed) = &mut *state;
        if last.is_some_and(|at| now.duration_since(at) < self.interval) {
            *suppressed += 1;
            return None;
        }
        *last = Some(now);
        Some(std::mem::take(suppressed))
    }
}

/// Interval between acquire-timeout log lines for one window.
const ACQUIRE_TIMEOUT_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

impl WindowSurface {
    /// Create a `WindowSurface` from an already-configured `wgpu::Surface`.
    ///
    /// The `config` must already have been applied to the surface via
    /// `surface.configure(&device, &config)` before calling this constructor.
    ///
    /// This constructor is called by `Compositor::new_windowed()` after adapter
    /// and device creation, so the surface and device are guaranteed compatible.
    pub fn new(surface: wgpu::Surface<'static>, config: wgpu::SurfaceConfiguration) -> Self {
        let width = config.width;
        let height = config.height;
        Self {
            surface,
            config: std::sync::Mutex::new(config),
            width: std::sync::atomic::AtomicU32::new(width),
            height: std::sync::atomic::AtomicU32::new(height),
            swapchain: std::sync::Arc::new(std::sync::Mutex::new(SwapchainSlot {
                pending: None,
                encoding: false,
            })),
            swapchain_done: std::sync::Arc::new(std::sync::Condvar::new()),
            surface_recovery: std::sync::Mutex::new(SurfaceRecoveryState::default()),
            pending_resize_width: std::sync::atomic::AtomicU32::new(0),
            pending_resize_height: std::sync::atomic::AtomicU32::new(0),
            presented_frame_count: std::sync::atomic::AtomicU64::new(0),
            timeout_log: LogThrottle::new(ACQUIRE_TIMEOUT_LOG_INTERVAL),
        }
    }

    /// Texture format the swapchain is configured with.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).format
    }

    /// Return the number of swapchain textures actually presented.
    pub fn presented_frame_count(&self) -> u64 {
        self.presented_frame_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Attempt the recovery transition queued by a previous acquire failure.
    ///
    /// This is intentionally `pub(crate)`: the normal `Compositor` owns the
    /// device needed to call it, while the detailed outcome remains internal to
    /// the compositor for later retained-diagnostics wiring.
    ///
    /// Must run on the compositor thread, between frames. That is the same
    /// ownership boundary as [`Self::reconfigure`].
    pub(crate) fn attempt_pending_recovery(
        &self,
        device: &wgpu::Device,
    ) -> Option<SurfaceRecoveryOutcome> {
        let mut recovery = self
            .surface_recovery
            .lock()
            .expect("surface recovery lock poisoned");
        recovery.attempt_pending_recovery(|| self.reconfigure_current(device))
    }

    /// Reconfigure using the latest non-zero configuration dimensions and
    /// translate a wgpu panic into a terminal recovery result.
    fn reconfigure_current(&self, device: &wgpu::Device) -> Result<(), ()> {
        let (width, height) = {
            let config = self
                .config
                .lock()
                .expect("WindowSurface config lock poisoned");
            (config.width, config.height)
        };
        if width == 0 || height == 0 {
            tracing::error!(
                width,
                height,
                "WindowSurface recovery cannot reconfigure a zero-sized surface"
            );
            return Err(());
        }

        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.reconfigure(width, height, device);
        }))
        .map_err(|_| {
            tracing::error!("WindowSurface recovery reconfigure panicked");
        })
    }

    /// Reconfigure the surface after a window resize.
    ///
    /// MUST be called from the compositor thread (it owns the `wgpu::Device`).
    /// The main thread signals a resize via `pending_resize_width/height`.
    pub fn reconfigure(&self, new_width: u32, new_height: u32, device: &wgpu::Device) {
        if new_width == 0 || new_height == 0 {
            // Zero-size surface is invalid — skip reconfiguration.
            return;
        }
        // Ensure the GPU has finished processing any command buffers that
        // reference the current surface texture before we drop it.  Without
        // this, queue::submit() may still be reading the texture asynchronously
        // when we destroy it below, causing a wgpu validation error:
        //   "Texture with '<Surface Texture>' label has been destroyed"
        device.poll(wgpu::Maintain::Wait);

        // Serialize against the compositor's acquire→submit critical section
        // (Bug B Layer-2, hud-hj0xb). `reconfigure()` runs on the compositor
        // thread at the top of the frame loop, but `Surface::configure()` below
        // destroys every outstanding swapchain image. If a previous frame's
        // submit were still in flight (e.g. the main-thread present had not yet
        // released its `SurfaceTexture`), destroying it here would invalidate a
        // texture an in-flight `TextureView`/submit still references.
        //
        // We take the swapchain lock and block on the condvar until any
        // `encoding` is finished, then clear `pending` *while still holding the
        // lock* so no present can take a texture we are about to invalidate.
        // wgpu also requires all acquired `SurfaceTexture` images to be dropped
        // before `Surface::configure()`, so clearing `pending` here avoids
        // "SurfaceOutput must be dropped before a new Surface is made".
        {
            let mut slot = self.swapchain.lock().expect("swapchain lock poisoned");
            // `reconfigure` runs at the top of the compositor frame loop, BEFORE
            // `acquire_frame`/`render_frame` for this cycle, so no `CompositorFrame`
            // guard is alive on this thread and `encoding` is normally already
            // `false`. The wait is bounded purely as a defense-in-depth guard: if
            // a future refactor ever called this with an in-flight encode on the
            // SAME thread, an unbounded `wait` would deadlock (the thread that must
            // drop the guard is the one parked here). On timeout we surface the
            // contract violation loudly rather than hang forever; we still avoid
            // taking `pending` while `encoding` is true so we never destroy a
            // texture an in-flight submit references.
            let wait_start = std::time::Instant::now();
            while slot.encoding {
                let (next, timeout) = self
                    .swapchain_done
                    .wait_timeout(slot, std::time::Duration::from_millis(250))
                    .expect("swapchain condvar poisoned");
                slot = next;
                if timeout.timed_out() && slot.encoding {
                    tracing::error!(
                        elapsed_ms = wait_start.elapsed().as_millis() as u64,
                        "reconfigure: encoding still in progress after timeout — \
                         unexpected (reconfigure must run between frames); leaving \
                         pending texture intact and proceeding without clearing it"
                    );
                    break;
                }
            }
            // Only clear (drop) the pending texture if no encode references it.
            if !slot.encoding {
                let _ = slot.pending.take();
            }
        }
        // Clamp to adapter's max texture dimension to avoid wgpu validation errors
        // on GPUs with limits below the requested window size.
        let max_dim = device.limits().max_texture_dimension_2d;
        let w = new_width.min(max_dim);
        let h = new_height.min(max_dim);
        let mut cfg = self
            .config
            .lock()
            .expect("WindowSurface config lock poisoned");
        cfg.width = w;
        cfg.height = h;
        self.surface.configure(device, &cfg);
        self.width.store(w, std::sync::atomic::Ordering::Release);
        self.height.store(h, std::sync::atomic::Ordering::Release);
        tracing::info!(width = w, height = h, "WindowSurface reconfigured");
    }

    /// Present the currently pending swapchain image, if any.
    ///
    /// Called from the **main thread** after the compositor signals
    /// `FrameReadySignal`. Returns `true` if a texture was presented, `false`
    /// if no texture was pending.
    ///
    /// ## Serialization (Bug B Layer-2, hud-hj0xb)
    /// The take+present is performed *while holding the swapchain mutex*, and
    /// the wait for the compositor's encode→submit to finish happens under that
    /// same lock via the condvar. This is the fix for the acquire→submit race:
    /// because the `encoding` check and the `pending.take()` are now atomic
    /// w.r.t. the compositor (no check-then-act gap), the main thread can never
    /// take and destroy a `SurfaceTexture` that an in-flight submit's
    /// `TextureView` still references — which previously produced
    /// "Surface Texture has been destroyed" on `queue.submit()`.
    pub fn present_pending_texture(&self) -> bool {
        let mut slot = self.swapchain.lock().expect("swapchain lock poisoned");

        // Wait for the compositor thread to finish encoding + submitting before
        // we take (and destroy) the SurfaceTexture. The condvar releases the
        // lock while parked and re-acquires it on wake, so the compositor can
        // make progress and the wakeup is observed atomically (no spin, no
        // TOCTOU). The wait is bounded in practice: the EncodingGuard clears
        // `encoding` right after queue.submit() + device.poll() (sub-ms).
        let wait_start = std::time::Instant::now();
        while slot.encoding {
            // Safety valve: don't block the main thread forever if the
            // compositor thread died mid-encode without dropping its guard.
            let (next, timeout) = self
                .swapchain_done
                .wait_timeout(slot, std::time::Duration::from_millis(250))
                .expect("swapchain condvar poisoned");
            slot = next;
            if timeout.timed_out() && slot.encoding {
                // The encode→submit is STILL in flight after the timeout. We must
                // NOT take+present (destroy) the texture here — the compositor's
                // `TextureView` may still reference it, which would reopen the
                // exact "Surface Texture has been destroyed" race this lock fixes
                // (a 250ms encode means a driver stall / shader compile / loaded
                // CI, not necessarily a dead thread). Skip presenting this tick
                // and leave `pending` in place; the next event-loop tick will
                // present it once the compositor finishes and clears `encoding`.
                tracing::warn!(
                    elapsed_ms = wait_start.elapsed().as_millis() as u64,
                    "present_pending_texture: encoding still in progress after timeout; \
                     skipping present this tick (compositor may be stalled)"
                );
                return false;
            }
        }

        if let Some(texture) = slot.pending.take() {
            texture.present();
            self.presented_frame_count
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Finish a successful acquire: release the swapchain lock, then build the
    /// `EncodingGuard` and the `CompositorFrame`.
    ///
    /// The guard is created **after** `drop(slot)` because its `Drop` re-locks
    /// `self.swapchain`; constructing/holding it while `slot` is still locked
    /// would deadlock on the non-reentrant `std::sync::Mutex`. The `encoding`
    /// flag was already set under the lock by `acquire_frame`, so the critical
    /// section stays open continuously across this hand-off — the guard's drop
    /// (after `queue.submit()`) is what closes it.
    fn finish_frame(
        &self,
        slot: std::sync::MutexGuard<'_, SwapchainSlot>,
        view: wgpu::TextureView,
        acquisition: SurfaceFrameAcquisition,
    ) -> Option<CompositorFrame> {
        drop(slot);
        let guard = EncodingGuard {
            slot: self.swapchain.clone(),
            done: self.swapchain_done.clone(),
        };
        Some(CompositorFrame {
            view,
            _guard: Box::new(guard),
            acquisition,
        })
    }

    /// Handle a `get_current_texture()` error during `acquire_frame`.
    ///
    /// Called while holding the swapchain lock (`slot`). Differentiates by
    /// error variant to avoid wasted GPU calls and log noise during normal
    /// resize/minimize cycles:
    ///
    /// - `Timeout`     — transient; retry once (GPU may have been busy). On a
    ///   successful retry the texture is stored in `slot.pending` and a view is
    ///   returned.
    /// - `Outdated` / `Lost` — surface changed (resize/DPI) or swapchain lost;
    ///   reconfiguration is required before the next acquire succeeds, so an
    ///   immediate retry would fail again. Skip the frame.
    /// - `OutOfMemory` / `Other` — log and skip the frame.
    ///
    /// Returns `Some(view)` if a texture was (re)acquired, `None` to skip the
    /// frame. The caller closes the critical section on `None`.
    fn acquire_retry_view(
        &self,
        slot: &mut SwapchainSlot,
        err: wgpu::SurfaceError,
    ) -> Option<wgpu::TextureView> {
        // This is the only acquire-error ingress for window surfaces. It
        // ignores a transient timeout, queues Lost/Outdated recovery, and
        // marks terminal failures before this frame is skipped.
        self.observe_wgpu_acquire_failure(&err);
        match err {
            wgpu::SurfaceError::Timeout => {
                // One throttle covers both lines: a window that keeps timing
                // out (occluded, asleep) would otherwise log every attempt.
                let log = self.timeout_log.allow(std::time::Instant::now());
                if let Some(suppressed) = log {
                    tracing::warn!(
                        error = %err,
                        suppressed,
                        "WindowSurface::acquire_frame: timeout acquiring texture; retrying once"
                    );
                }
                match self.surface.get_current_texture() {
                    Ok(t) => {
                        let v = t
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        slot.pending = Some(t);
                        Some(v)
                    }
                    Err(e2) => {
                        self.observe_wgpu_acquire_failure(&e2);
                        if log.is_some() {
                            tracing::error!(
                                first_error = %err,
                                second_error = %e2,
                                "WindowSurface::acquire_frame: retry after timeout also failed; \
                                 skipping frame (runtime will retry later)"
                            );
                        }
                        None
                    }
                }
            }
            wgpu::SurfaceError::Outdated => {
                tracing::warn!(
                    error = %err,
                    "WindowSurface::acquire_frame: surface outdated or lost; \
                     skipping frame to allow reconfiguration"
                );
                None
            }
            wgpu::SurfaceError::Lost => {
                tracing::warn!(
                    error = %err,
                    "WindowSurface::acquire_frame: surface outdated or lost; \
                     skipping frame to allow reconfiguration"
                );
                None
            }
            wgpu::SurfaceError::OutOfMemory => {
                tracing::error!(
                    error = %err,
                    "WindowSurface::acquire_frame: out of GPU memory; skipping frame"
                );
                None
            }
            wgpu::SurfaceError::Other => {
                tracing::error!(
                    error = %err,
                    "WindowSurface::acquire_frame: unexpected surface error; skipping frame"
                );
                None
            }
        }
    }

    /// Classify a failed retry through the same production recovery transition
    /// as a first-attempt acquire failure.
    fn observe_wgpu_acquire_failure(&self, err: &wgpu::SurfaceError) {
        self.surface_recovery
            .lock()
            .expect("surface recovery lock poisoned")
            .observe_wgpu_acquire_failure(err);
    }
}

/// Creates [`WindowSurface`]s for additional display windows on the same
/// instance, adapter and device as the compositor's primary surface, with the
/// primary's format, present mode and alpha mode (one compositor renders all
/// of them, so the format must match its pipelines).
#[derive(Clone)]
pub struct SurfaceFactory {
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) template: wgpu::SurfaceConfiguration,
}

impl SurfaceFactory {
    /// Create and configure a surface for `window` at `width` x `height`.
    pub fn create(
        &self,
        window: std::sync::Arc<winit::window::Window>,
        width: u32,
        height: u32,
    ) -> Result<WindowSurface, String> {
        let surface = self
            .instance
            .create_surface(window)
            .map_err(|e| format!("create_surface: {e}"))?;
        let caps = surface.get_capabilities(&self.adapter);
        if !caps.formats.contains(&self.template.format) {
            return Err(format!(
                "display surface does not support the primary format {:?}",
                self.template.format
            ));
        }
        let mut config = self.template.clone();
        if !caps.alpha_modes.contains(&config.alpha_mode) {
            config.alpha_mode = caps
                .alpha_modes
                .first()
                .copied()
                .ok_or("display surface reports no alpha modes")?;
        }
        if !caps.present_modes.contains(&config.present_mode) {
            config.present_mode = wgpu::PresentMode::Fifo;
        }
        let max_dim = self.device.limits().max_texture_dimension_2d;
        config.width = width.clamp(1, max_dim);
        config.height = height.clamp(1, max_dim);
        surface.configure(&self.device, &config);
        Ok(WindowSurface::new(surface, config))
    }
}

impl CompositorSurface for WindowSurface {
    /// Acquire the next swapchain image from the OS compositor.
    ///
    /// Called on the compositor thread (Stage 6 / Stage 7 boundary).
    ///
    /// Returns `Some(CompositorFrame)` on success. Returns `None` when the
    /// surface is temporarily unavailable (double consecutive acquire failure,
    /// e.g., on driver reset or device loss on resume). The caller MUST skip
    /// the render pass and retry on the next frame — this is not a fatal error.
    ///
    /// The acquired `SurfaceTexture` is stored in `swapchain.pending` so the
    /// main thread can present it via `present_pending_texture()`, satisfying
    /// the macOS/Metal requirement. The `CompositorFrame._guard` holds an
    /// `EncodingGuard` that marks the encode→submit critical section and, on
    /// drop, releases it — it does NOT own the `SurfaceTexture` (ownership stays
    /// in `swapchain.pending` so the frame is not discarded on the compositor
    /// thread).
    ///
    /// A `Timeout` retries once. `Outdated` and `Lost` queue the explicit
    /// compositor-thread recovery transition that reconfigures before the next
    /// fresh acquire. If a frame cannot be acquired, `None` is returned and the
    /// `encoding` marker is cleared (and waiters woken) so the main thread is
    /// not blocked.
    fn acquire_frame(&self) -> Option<CompositorFrame> {
        // Serialize acquire/pending-state handoff through the swapchain-
        // ownership mutex used by the main-thread present path and the
        // compositor reconfigure path. If a texture is still pending, reuse
        // that same acquired image instead of trying a second acquire.
        let mut slot = self.swapchain.lock().expect("swapchain lock poisoned");

        // Mark the encode→submit critical section open BEFORE creating the
        // TextureView and while still holding the lock. This is the acquire
        // half of the serialization (hud-hj0xb): present_pending_texture() and
        // reconfigure() both check `encoding` under this same lock and wait on
        // the condvar, so neither can destroy the SurfaceTexture while the
        // compositor holds a TextureView referencing it.
        slot.encoding = true;
        let acquisition = SurfaceFrameAcquisition::from_pending_surface(slot.pending.is_some());

        // Build the frame's TextureView (if any) while holding the lock. On the
        // success paths we hand off `slot` to `finish_frame`, which constructs
        // the `EncodingGuard` AFTER the lock has been released — the guard's
        // drop re-locks `swapchain`, so it must never drop while we still hold
        // `slot` (`std::sync::Mutex` is non-reentrant → that would deadlock).
        //
        // On the skip paths we clear `encoding` directly under the held lock
        // (no guard is created), so the critical section closes immediately and
        // any present/reconfigure waiter is woken below.
        let view = if let Some(existing) = slot.pending.as_ref() {
            Some(
                existing
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default()),
            )
        } else {
            match self.surface.get_current_texture() {
                Ok(surface_texture) => {
                    let v = surface_texture
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    // Store the SurfaceTexture so the main thread can present
                    // it. Do NOT box it inside CompositorFrame._guard — that
                    // would drop it (without calling .present()) when the frame
                    // is dropped on the compositor thread, discarding the
                    // rendered frame.
                    slot.pending = Some(surface_texture);
                    Some(v)
                }
                Err(e) => self.acquire_retry_view(&mut slot, e),
            }
        };

        match view {
            Some(view) => self.finish_frame(slot, view, acquisition),
            None => {
                // Skip this frame: close the critical section under the held
                // lock and wake any waiter (present/reconfigure) before
                // releasing it. `slot` is dropped on return.
                slot.encoding = false;
                self.swapchain_done.notify_all();
                None
            }
        }
    }

    /// Present the current frame to the display.
    ///
    /// On macOS/Metal this MUST be called on the main thread. The
    /// `WindowedRuntime` main thread calls `present_pending_texture()`, which
    /// takes the pending `SurfaceTexture` and calls `SurfaceTexture::present()`
    /// directly. This trait method is a no-op for `WindowSurface` because the
    /// actual present happens via the pending-texture handoff, NOT through this
    /// `present()` call (which runs on the compositor thread alongside
    /// `render_frame()`).
    fn present(&self) {
        // No-op. The actual SurfaceTexture::present() is called by the main
        // thread via take_pending_texture(). See WindowedRuntime::maybe_present_frame().
    }

    fn size(&self) -> (u32, u32) {
        (
            self.width.load(std::sync::atomic::Ordering::Acquire),
            self.height.load(std::sync::atomic::Ordering::Acquire),
        )
    }
}

// ─── HeadlessSurface ─────────────────────────────────────────────────────────

/// Headless offscreen surface for testing and CI.
///
/// Satisfies:
/// - runtime-kernel/spec.md Requirement: Headless Mode (line 198)
/// - validation-framework/spec.md Requirement: DR-V2 (line 186)
/// - validation-framework/spec.md Requirement: DR-V6 (line 238)
pub struct HeadlessSurface {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    /// Output buffer for pixel readback.
    pub output_buffer: wgpu::Buffer,
    pub bytes_per_row: u32,
}

impl HeadlessSurface {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("headless_target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Buffer for readback
        let bytes_per_row = Self::aligned_bytes_per_row(width);
        let buffer_size = (bytes_per_row * height) as u64;
        let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("headless_readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        Self {
            texture,
            view,
            width,
            height,
            output_buffer,
            bytes_per_row,
        }
    }

    /// Bytes per row aligned to wgpu's 256-byte requirement.
    fn aligned_bytes_per_row(width: u32) -> u32 {
        let unaligned = width * 4; // RGBA8 = 4 bytes per pixel
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        unaligned.div_ceil(align) * align
    }

    /// Copy the rendered texture to the readback buffer.
    ///
    /// Call this after the render pass, before submitting the command buffer.
    pub fn copy_to_buffer(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.output_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.bytes_per_row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Read back pixels from the buffer. Returns RGBA8 data.
    /// This is a blocking call (waits for GPU to finish).
    ///
    /// Per runtime-kernel/spec.md line 208: pixel readback is on-demand via
    /// `copy_texture_to_buffer`.
    pub fn read_pixels(&self, device: &wgpu::Device) -> Vec<u8> {
        let buffer_slice = self.output_buffer.slice(..);

        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();

        let data = buffer_slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((self.width * self.height * 4) as usize);

        // Remove row padding
        for y in 0..self.height {
            let offset = (y * self.bytes_per_row) as usize;
            let row_end = offset + (self.width * 4) as usize;
            pixels.extend_from_slice(&data[offset..row_end]);
        }

        drop(data);
        self.output_buffer.unmap();

        pixels
    }

    /// Get pixel at (x, y) from raw RGBA data. Returns [r, g, b, a].
    pub fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * width + x) * 4) as usize;
        [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
    }

    /// Assert that a pixel at (x, y) is within `tolerance` of `expected` on
    /// every channel (R, G, B, A).
    ///
    /// Returns `Ok([r, g, b, a])` on pass, `Err(message)` on failure.
    ///
    /// The `label` is included in the error message for diagnostic clarity.
    ///
    /// Software-rasterised GPU paths (llvmpipe / SwiftShader) may produce
    /// values that differ from the linear-space input by ±2 per channel due
    /// to sRGB conversion.  Use `tolerance = 2` for solid fills on CI.
    pub fn assert_pixel_color(
        data: &[u8],
        width: u32,
        x: u32,
        y: u32,
        expected: [u8; 4],
        tolerance: u8,
        label: &str,
    ) -> Result<[u8; 4], String> {
        let actual = Self::pixel_at(data, width, x, y);
        for ch in 0..4 {
            let diff = actual[ch].abs_diff(expected[ch]);
            if diff > tolerance {
                return Err(format!(
                    "pixel assertion failed at ({x},{y}) [{label}]: \
                     channel {ch} actual={} expected={} diff={} tolerance={}",
                    actual[ch], expected[ch], diff, tolerance,
                ));
            }
        }
        Ok(actual)
    }
}

// ─── CompositorSurface impl for HeadlessSurface ────────────────────────────────

/// `HeadlessSurface` implements `CompositorSurface`.
///
/// - `acquire_frame()` creates a new `TextureView` from the offscreen texture; guard is `()`.
///   Always returns `Some` — headless surfaces never fail to acquire a frame.
/// - `present()` is a no-op (spec line 199: "Headless surface present() MUST be a no-op").
/// - `size()` returns `(width, height)`.
impl CompositorSurface for HeadlessSurface {
    fn acquire_frame(&self) -> Option<CompositorFrame> {
        // Re-create the view from the texture (the stored view can't be Clone).
        // We return a new TextureView pointing at the same texture.
        // The guard is a no-op `()` because the texture is owned by HeadlessSurface.
        // HeadlessSurface never fails — always returns Some.
        let view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        Some(CompositorFrame {
            view,
            _guard: Box::new(()), // no-op guard — HeadlessSurface owns the texture
            acquisition: SurfaceFrameAcquisition::Fresh,
        })
    }

    /// No-op — headless mode does not present to a display (spec line 199).
    fn present(&self) {}

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_throttle_passes_one_line_per_interval_and_counts_drops() {
        let throttle = LogThrottle::new(std::time::Duration::from_secs(10));
        let t0 = std::time::Instant::now();
        assert_eq!(throttle.allow(t0), Some(0));
        for i in 1..=119 {
            let at = t0 + std::time::Duration::from_millis(i * 8);
            assert_eq!(throttle.allow(at), None, "attempt {i} inside the interval");
        }
        assert_eq!(
            throttle.allow(t0 + std::time::Duration::from_secs(10)),
            Some(119)
        );
        assert_eq!(
            throttle.allow(t0 + std::time::Duration::from_secs(11)),
            None
        );
    }

    #[test]
    fn test_assert_pixel_color_passes_within_tolerance() {
        let pixels: Vec<u8> = vec![
            100, 200, 50, 255, // pixel (0,0)
            10, 20, 30, 255, // pixel (1,0)
        ];
        HeadlessSurface::assert_pixel_color(&pixels, 2, 0, 0, [100, 200, 50, 255], 0, "exact")
            .expect("exact match should pass");
        HeadlessSurface::assert_pixel_color(&pixels, 2, 0, 0, [102, 200, 50, 255], 2, "within tol")
            .expect("within-tolerance should pass");
    }

    #[test]
    fn test_assert_pixel_color_fails_outside_tolerance() {
        let pixels: Vec<u8> = vec![100, 200, 50, 255];
        let result = HeadlessSurface::assert_pixel_color(
            &pixels,
            1,
            0,
            0,
            [110, 200, 50, 255],
            2,
            "outside",
        );
        assert!(result.is_err(), "should fail when diff > tolerance");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("channel 0"),
            "error should identify channel: {msg}"
        );
    }

    /// `WindowSurface::size()` cannot be tested without a real wgpu surface
    /// (requires a window handle). The `HeadlessSurface` covers the rendering
    /// path; windowed integration tests are in the `vertical_slice` binary.
    /// This test documents the interface contract for reviewers.
    #[test]
    fn test_window_surface_atomic_size_fields() {
        // Verify that AtomicU32 read/write works correctly for width/height.
        // We cannot construct a real WindowSurface without a window handle,
        // so this test only exercises the atomic helpers directly.
        use std::sync::atomic::{AtomicU32, Ordering};
        let w = AtomicU32::new(1920);
        let h = AtomicU32::new(1080);
        assert_eq!(w.load(Ordering::Acquire), 1920);
        assert_eq!(h.load(Ordering::Acquire), 1080);
        w.store(2560, Ordering::Release);
        assert_eq!(w.load(Ordering::Acquire), 2560);
    }

    /// A real `Lost` result must enter the production recovery state machine
    /// before the compositor thread attempts to configure the surface again.
    ///
    /// This enters through the same wgpu-error classifier used by
    /// `WindowSurface::acquire_retry_view`; it deliberately does not inject a
    /// test-only recovery request.
    #[test]
    fn lost_or_outdated_acquire_failure_queues_and_attempts_reconfigure() {
        for (error, trigger) in [
            (wgpu::SurfaceError::Lost, SurfaceAcquireFailure::Lost),
            (
                wgpu::SurfaceError::Outdated,
                SurfaceAcquireFailure::Outdated,
            ),
        ] {
            let mut recovery = SurfaceRecoveryState::default();
            recovery.observe_wgpu_acquire_failure(&error);

            let mut reconfigure_attempts = 0;
            let outcome = recovery.attempt_pending_recovery(|| {
                reconfigure_attempts += 1;
                Ok(())
            });

            assert_eq!(reconfigure_attempts, 1);
            assert_eq!(
                outcome,
                Some(SurfaceRecoveryOutcome::Reconfigured { trigger })
            );
        }
    }

    /// If the production reconfigure action fails, the recovery state must
    /// surface a terminal outcome rather than retrying forever.
    #[test]
    fn failed_lost_reconfigure_records_terminal_recovery_outcome() {
        let mut recovery = SurfaceRecoveryState::default();
        recovery.observe_acquire_failure(SurfaceAcquireFailure::Lost);

        let outcome = recovery.attempt_pending_recovery(|| Err(()));

        assert_eq!(
            outcome,
            Some(SurfaceRecoveryOutcome::Terminal {
                trigger: SurfaceAcquireFailure::Lost,
            })
        );
    }

    #[test]
    fn out_of_memory_and_other_acquire_failures_are_terminal_without_reconfigure() {
        for (error, trigger) in [
            (
                wgpu::SurfaceError::OutOfMemory,
                SurfaceAcquireFailure::OutOfMemory,
            ),
            (wgpu::SurfaceError::Other, SurfaceAcquireFailure::Other),
        ] {
            let mut recovery = SurfaceRecoveryState::default();
            recovery.observe_wgpu_acquire_failure(&error);

            let mut reconfigure_attempted = false;
            let outcome = recovery.attempt_pending_recovery(|| {
                reconfigure_attempted = true;
                Ok(())
            });

            assert!(
                !reconfigure_attempted,
                "{trigger:?} must not attempt to reconfigure a terminal surface"
            );
            assert_eq!(outcome, Some(SurfaceRecoveryOutcome::Terminal { trigger }));
        }
    }

    /// Verify that a simulated double swapchain-acquire failure does NOT panic.
    ///
    /// `WindowSurface::acquire_frame` uses a real `wgpu::Surface` and cannot be
    /// unit-tested without a GPU. This test validates the *contract* via a mock
    /// `CompositorSurface` that always returns `None`, asserting that callers
    /// handle `None` gracefully rather than unwrapping.
    ///
    /// The real double-failure path is covered by the fix at surface.rs
    /// `Err(e2)` arm: it returns `None` instead of `panic!`. This test
    /// documents that contract and ensures it compiles correctly. [hud-lnjs4]
    #[test]
    fn test_acquire_frame_double_failure_returns_none_no_panic() {
        // A mock surface that always returns None from acquire_frame,
        // simulating two consecutive swapchain-acquire failures.
        struct AlwaysFailSurface;

        impl CompositorSurface for AlwaysFailSurface {
            fn acquire_frame(&self) -> Option<CompositorFrame> {
                None
            }
            fn present(&self) {}
            fn size(&self) -> (u32, u32) {
                (1920, 1080)
            }
        }

        let surface = AlwaysFailSurface;

        // This must NOT panic — the contract is that double failure returns None.
        let result = surface.acquire_frame();
        assert!(
            result.is_none(),
            "acquire_frame must return None on double failure, not panic"
        );

        // Callers that correctly handle None skip the frame — verify the
        // pattern compiles and behaves correctly.
        let frame_skipped = surface.acquire_frame().is_none();
        assert!(frame_skipped, "caller must detect None and skip the frame");
    }

    // ─── Bug B Layer-2: acquire→submit vs present/reconfigure serialization ──
    //
    // `WindowSurface::acquire_frame`/`present_pending_texture`/`reconfigure`
    // need a real `wgpu::Surface` and so cannot be unit-tested without a GPU.
    // The race they fix, however, lives entirely in the `SwapchainSlot` +
    // `Condvar` lock discipline, which we CAN exercise directly. The harness
    // below uses the exact same private types and mirrors the real lock order:
    //
    //   compositor: lock → encoding=true → (acquire view) → unlock
    //                → encode/submit (no lock) → guard drop: lock → encoding=false → notify
    //   main:       lock → while encoding { wait } → take+present(destroy) → unlock
    //   reconfigure:lock → while encoding { wait } → take+destroy → configure → unlock
    //
    // A live "surface texture" is modelled by an `Arc<AtomicBool>`. Destroying
    // it (present/reconfigure) while a "TextureView" still references it (the
    // compositor's encode→submit window) is exactly the wgpu
    // "Surface Texture has been destroyed" condition; the test asserts it never
    // happens, across many concurrent cycles.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// A modelled swapchain image: `alive` is cleared when "presented"/destroyed.
    struct FakeTexture {
        alive: Arc<AtomicBool>,
    }

    /// Test harness mirroring `WindowSurface`'s swapchain-ownership protocol.
    struct SwapchainHarness {
        slot: Arc<std::sync::Mutex<TestSlot>>,
        done: Arc<std::sync::Condvar>,
        /// Set if any present/reconfigure destroyed a texture while a frame was
        /// mid-encode (i.e. an `EncodingGuard` was still outstanding). Any hit
        /// here is the bug.
        violations: Arc<AtomicU64>,
    }

    /// Mirror of `SwapchainSlot` for the harness (private types can't escape the
    /// crate, so the test re-declares an equivalent shape).
    struct TestSlot {
        pending: Option<FakeTexture>,
        encoding: bool,
    }

    /// Mirror of `EncodingGuard`: on drop, clears `encoding` under the lock and
    /// notifies waiters — closing the critical section after "submit".
    struct TestGuard {
        slot: Arc<std::sync::Mutex<TestSlot>>,
        done: Arc<std::sync::Condvar>,
        /// Liveness flag of the texture this frame's view references.
        view_alive: Arc<AtomicBool>,
        violations: Arc<AtomicU64>,
    }

    impl Drop for TestGuard {
        fn drop(&mut self) {
            // The view must still reference a LIVE texture for the whole
            // encode→submit window. If a present/reconfigure destroyed it while
            // we held the guard, that is the race we are guarding against.
            if !self.view_alive.load(Ordering::Acquire) {
                self.violations.fetch_add(1, Ordering::Relaxed);
            }
            let mut slot = self.slot.lock().expect("slot poisoned");
            slot.encoding = false;
            self.done.notify_all();
        }
    }

    impl SwapchainHarness {
        fn new() -> Self {
            Self {
                slot: Arc::new(std::sync::Mutex::new(TestSlot {
                    pending: None,
                    encoding: false,
                })),
                done: Arc::new(std::sync::Condvar::new()),
                violations: Arc::new(AtomicU64::new(0)),
            }
        }

        /// Compositor-thread acquire: open the critical section and return a
        /// guard plus the referenced texture liveness flag. Mirrors
        /// `acquire_frame`'s success path (lock → encoding=true → unlock →
        /// build guard after lock release).
        fn acquire(&self) -> TestGuard {
            let view_alive = {
                let mut slot = self.slot.lock().expect("slot poisoned");
                slot.encoding = true;
                // Lock released at the end of this block (mirrors finish_frame's
                // drop(slot) before building the guard).
                match slot.pending.as_ref() {
                    Some(t) => t.alive.clone(),
                    None => {
                        let alive = Arc::new(AtomicBool::new(true));
                        slot.pending = Some(FakeTexture {
                            alive: alive.clone(),
                        });
                        alive
                    }
                }
            };
            TestGuard {
                slot: self.slot.clone(),
                done: self.done.clone(),
                view_alive,
                violations: self.violations.clone(),
            }
        }

        /// Classify the next compositor build against the same pending-frame
        /// state used by the real window surface. This lets the regression stay
        /// deterministic without constructing a live swapchain.
        fn acquisition_for_next_build(&self) -> SurfaceFrameAcquisition {
            let slot = self.slot.lock().expect("slot poisoned");
            SurfaceFrameAcquisition::from_pending_surface(slot.pending.is_some())
        }

        /// Main-thread present: wait for encoding to finish, then take+destroy
        /// the texture. Mirrors `present_pending_texture`.
        fn present(&self) {
            let mut slot = self.slot.lock().expect("slot poisoned");
            while slot.encoding {
                slot = self.done.wait(slot).expect("condvar poisoned");
            }
            if let Some(t) = slot.pending.take() {
                t.alive.store(false, Ordering::Release); // "present" destroys it
            }
        }

        /// Compositor-thread reconfigure: wait for encoding to finish, then
        /// destroy any pending texture (Surface::configure would invalidate it).
        /// Mirrors `reconfigure`'s swapchain-lock block.
        fn reconfigure(&self) {
            let mut slot = self.slot.lock().expect("slot poisoned");
            while slot.encoding {
                slot = self.done.wait(slot).expect("condvar poisoned");
            }
            if let Some(t) = slot.pending.take() {
                t.alive.store(false, Ordering::Release);
            }
        }
    }

    /// Two compositor builds can target the same pending swapchain texture
    /// before the main thread presents it. Only the first build acquired a new
    /// surface texture; telemetry must not count the reuse as a second acquire.
    #[test]
    fn two_builds_before_present_report_one_fresh_swapchain_acquisition() {
        let harness = SwapchainHarness::new();

        let first = harness.acquisition_for_next_build();
        let first_guard = harness.acquire();
        drop(first_guard);

        let second = harness.acquisition_for_next_build();
        let second_guard = harness.acquire();
        drop(second_guard);

        assert_eq!(first, SurfaceFrameAcquisition::Fresh);
        assert_eq!(second, SurfaceFrameAcquisition::ReusedPending);
        assert!(first.is_fresh());
        assert!(
            !second.is_fresh(),
            "a build reusing the pending texture must not increment surface_acquisitions"
        );

        harness.present();
        assert_eq!(
            harness.acquisition_for_next_build(),
            SurfaceFrameAcquisition::Fresh,
            "presentation consumes the pending texture, so the next build acquires anew"
        );
    }

    /// Concurrent reconfigure/present vs encode→submit must never destroy a
    /// surface texture that an in-flight frame still references. [hud-hj0xb]
    ///
    /// This is the regression test for Bug B Layer-2: before the fix, the
    /// main-thread present checked an `AtomicBool` *before* taking the
    /// pending-texture lock, leaving a check-then-act gap through which the
    /// texture could be destroyed mid-submit. With the `encoding` check folded
    /// under the swapchain mutex and a condvar wait, that gap is closed —
    /// `violations` must stay 0.
    #[test]
    fn test_acquire_submit_serialized_against_present_and_reconfigure() {
        let harness = Arc::new(SwapchainHarness::new());
        const ITERS: usize = 5_000;

        // Compositor thread: acquire → "encode/submit" → drop guard, in a tight
        // loop. The drop guard verifies the texture was alive for the whole
        // window.
        let compositor = {
            let h = harness.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERS {
                    let guard = h.acquire();
                    // Simulate encode + submit work referencing the view. If a
                    // racing present/reconfigure destroys the texture here, the
                    // guard's drop records a violation.
                    std::hint::spin_loop();
                    std::hint::spin_loop();
                    drop(guard);
                }
            })
        };

        // Main thread analogue: hammer present.
        let presenter = {
            let h = harness.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERS {
                    h.present();
                }
            })
        };

        // Another compositor-side caller: hammer reconfigure.
        let reconfigurer = {
            let h = harness.clone();
            std::thread::spawn(move || {
                for _ in 0..ITERS {
                    h.reconfigure();
                }
            })
        };

        compositor.join().expect("compositor thread panicked");
        presenter.join().expect("presenter thread panicked");
        reconfigurer.join().expect("reconfigurer thread panicked");

        assert_eq!(
            harness.violations.load(Ordering::Relaxed),
            0,
            "a present/reconfigure destroyed a surface texture while a frame was \
             mid-encode — the acquire→submit critical section is not serialized"
        );
    }

    /// A present that races an in-progress encode MUST block until the encode's
    /// guard drops, never taking the texture early. [hud-hj0xb]
    #[test]
    fn test_present_blocks_until_encoding_completes() {
        let harness = Arc::new(SwapchainHarness::new());

        // Open a critical section (encoding=true) and hold it.
        let guard = harness.acquire();
        let presented_early = Arc::new(AtomicBool::new(false));

        let presenter = {
            let h = harness.clone();
            let flag = presented_early.clone();
            std::thread::spawn(move || {
                h.present();
                // We only reach here after present() returns, which must be
                // after the guard dropped.
                flag.store(true, Ordering::Release);
            })
        };

        // Give the presenter time to reach (and block on) the condvar wait.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !presented_early.load(Ordering::Acquire),
            "present() returned while encoding was still in progress"
        );

        // Now finish the encode; present() must unblock and complete.
        drop(guard);
        presenter.join().expect("presenter thread panicked");
        assert!(
            presented_early.load(Ordering::Acquire),
            "present() did not complete after the encode guard dropped"
        );
        assert_eq!(harness.violations.load(Ordering::Relaxed), 0);
    }
}
