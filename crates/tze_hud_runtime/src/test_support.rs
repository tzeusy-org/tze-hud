//! Test-only headless GPU helpers, also included by path in external test binaries.
//! Include this module once per binary so all initialization paths share one gate.
//! Canonical-app production boot uses this gate too; its separate benchmark-config
//! binary has only one GPU-creating test and needs no initialization guard.

use std::future::Future;
use tokio::sync::{Mutex, MutexGuard};

/// Serializes GPU-backed headless work within one test process.
///
/// Runtime-lib scenarios retain their existing outer guards. External binaries
/// use `serialized_headless_init` to guard initialization without serializing
/// rendering or retaining the guard for the runtime's lifetime.
static HEADLESS_RUNTIME_MUTEX: Mutex<()> = Mutex::const_new(());

pub(crate) async fn lock_headless_runtime() -> MutexGuard<'static, ()> {
    HEADLESS_RUNTIME_MUTEX.lock().await
}

/// Poll initialization only after admission, preserving its exact output.
///
/// The async guard is released on return, error, unwind or cancellation. Do not
/// call this while already holding `lock_headless_runtime`: the gate is not
/// reentrant. Production builds never compile this module.
pub(crate) async fn serialized_headless_init<F: Future>(init: F) -> F::Output {
    let diagnostic_started = std::time::Instant::now();
    eprintln!(
        "[DEBUG-hud-vlvu6] pid={} admission begin",
        std::process::id()
    );
    let _guard = lock_headless_runtime().await;
    eprintln!(
        "[DEBUG-hud-vlvu6] pid={} admission acquired {:.6}s",
        std::process::id(),
        diagnostic_started.elapsed().as_secs_f64()
    );
    init.await
}
