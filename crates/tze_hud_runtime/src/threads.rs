//! # threads
//!
//! Compositor thread spawning, main-thread priority elevation, and the
//! shutdown token shared by the runtime's threads.
//!
//! Priority elevation is best-effort: Linux uses `SCHED_RR`, Windows uses
//! `THREAD_PRIORITY_TIME_CRITICAL`. Failure to elevate never fails startup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::runtime::Runtime;
use tokio::sync::{broadcast, oneshot};

// ─── Shutdown token ───────────────────────────────────────────────────────────

/// Broadcast a shutdown signal to all threads.
///
/// Every long-running loop should listen on a `broadcast::Receiver` obtained
/// from `token.subscribe()` and exit cleanly when the signal arrives.
#[derive(Clone)]
pub struct ShutdownToken {
    tx: broadcast::Sender<ShutdownReason>,
    triggered: Arc<AtomicBool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownReason {
    /// Normal exit requested (SIGTERM, user close, etc.).
    Clean,
    /// GPU device was lost — flush telemetry and exit with non-zero code.
    GpuDeviceLost,
    /// An unrecoverable error occurred.
    Fatal(String),
}

impl ShutdownToken {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(8);
        Self {
            tx,
            triggered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Trigger shutdown. Idempotent — safe to call from any thread.
    pub fn trigger(&self, reason: ShutdownReason) {
        if self
            .triggered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            let _ = self.tx.send(reason);
        }
    }

    /// Subscribe a new receiver. Use this to listen for shutdown in loops.
    pub fn subscribe(&self) -> broadcast::Receiver<ShutdownReason> {
        self.tx.subscribe()
    }

    /// Returns `true` if shutdown has been triggered.
    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }
}

impl Default for ShutdownToken {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Main thread priority elevation ──────────────────────────────────────────

/// Attempt to elevate the calling thread to real-time / high priority.
///
/// This MUST be called from the main thread immediately after the winit
/// event loop starts.
///
/// Failure is non-fatal — the function logs a warning and returns `false`.
pub fn elevate_main_thread_priority() -> bool {
    #[cfg(target_os = "linux")]
    {
        elevate_linux()
    }
    #[cfg(target_os = "windows")]
    {
        elevate_windows()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        tracing::warn!("thread priority elevation not implemented for this platform");
        false
    }
}

#[cfg(target_os = "linux")]
fn elevate_linux() -> bool {
    // SCHED_RR with a modest priority (10 out of 99).
    // Requires CAP_SYS_NICE or a permissive rlimit.

    // Safety: sched_param is a plain C struct; all fields zero-initialized.
    let param = libc::sched_param { sched_priority: 10 };
    // SAFETY: syscall with valid args; failure handled below.
    let ret = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_RR, &param) };
    if ret == 0 {
        tracing::info!("main thread elevated to SCHED_RR priority 10");
        true
    } else {
        tracing::warn!(
            errno = ret,
            "failed to elevate main thread to SCHED_RR (errno {}); continuing at normal priority",
            ret
        );
        false
    }
}

#[cfg(target_os = "windows")]
fn elevate_windows() -> bool {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for the calling thread.
    let result = unsafe {
        let handle = GetCurrentThread();
        SetThreadPriority(handle, THREAD_PRIORITY_TIME_CRITICAL)
    };
    if result.is_ok() {
        tracing::info!("main thread elevated to THREAD_PRIORITY_TIME_CRITICAL");
        true
    } else {
        tracing::warn!(
            "failed to elevate main thread priority on Windows; continuing at normal priority"
        );
        false
    }
}

// ─── Compositor thread ────────────────────────────────────────────────────────

/// Signal sent by the compositor thread when it has finished initialising.
pub struct CompositorReady {
    /// The compositor is ready to accept work.
    pub ok: bool,
}

/// Spawn the compositor thread.
///
/// The closure `f` receives a `ShutdownToken` receiver and should run the
/// compositor event loop. It is responsible for owning `wgpu::Device` and
/// `wgpu::Queue` exclusively.
pub fn spawn_compositor_thread<F>(
    shutdown: ShutdownToken,
    ready_tx: oneshot::Sender<CompositorReady>,
    f: F,
) -> std::thread::JoinHandle<()>
where
    F: FnOnce(ShutdownToken, oneshot::Sender<CompositorReady>) + Send + 'static,
{
    std::thread::Builder::new()
        .name("tze-compositor".to_string())
        .spawn(move || {
            tracing::info!("compositor thread started");
            f(shutdown, ready_tx);
            tracing::info!("compositor thread exiting");
        })
        .expect("failed to spawn compositor thread")
}

// ─── Network runtime ──────────────────────────────────────────────────────────

/// Tokio multi-thread runtime used for network threads (gRPC, MCP, sessions).
///
/// Created once at startup. All network async tasks are spawned onto this runtime.
pub struct NetworkRuntime {
    pub rt: Runtime,
}

impl NetworkRuntime {
    /// Build a multi-thread Tokio runtime for network tasks.
    pub fn new() -> Result<Self, std::io::Error> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .thread_name("tze-network")
            .enable_all()
            .build()?;
        Ok(Self { rt })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ShutdownToken ────────────────────────────────────────────────────────

    #[test]
    fn shutdown_token_starts_untriggered() {
        let token = ShutdownToken::new();
        assert!(!token.is_triggered());
    }

    #[test]
    fn shutdown_token_trigger_sets_flag() {
        let token = ShutdownToken::new();
        token.trigger(ShutdownReason::Clean);
        assert!(token.is_triggered());
    }

    #[test]
    fn shutdown_token_trigger_is_idempotent() {
        let token = ShutdownToken::new();
        token.trigger(ShutdownReason::Clean);
        token.trigger(ShutdownReason::GpuDeviceLost); // second trigger ignored
        assert!(token.is_triggered());
    }

    #[tokio::test]
    async fn shutdown_token_receiver_gets_reason() {
        let token = ShutdownToken::new();
        let mut rx = token.subscribe();
        token.trigger(ShutdownReason::Clean);
        let reason = rx.recv().await.unwrap();
        assert_eq!(reason, ShutdownReason::Clean);
    }

    #[tokio::test]
    async fn shutdown_token_clone_shares_state() {
        let token = ShutdownToken::new();
        let token2 = token.clone();
        token2.trigger(ShutdownReason::Clean);
        assert!(token.is_triggered());
        assert!(token2.is_triggered());
    }

    // ── Thread spawning ──────────────────────────────────────────────────────

    #[test]
    fn spawn_compositor_thread_and_join() {
        let token = ShutdownToken::new();
        let (ready_tx, ready_rx) = oneshot::channel();
        let handle = spawn_compositor_thread(token, ready_tx, |_shutdown, ready| {
            // Signal ready immediately.
            let _ = ready.send(CompositorReady { ok: true });
            // No actual work — just a smoke test.
        });
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(ready_rx)
            .unwrap();
        assert!(result.ok);
        handle.join().expect("compositor thread panicked");
    }
}
