//! Pause and resume every agent: the human override behind the pause chord,
//! the pause tile and closing the panel. A thin, logged wrapper over
//! [`enter_safe_mode`] / [`exit_safe_mode`] that wakes the compositor once per
//! real transition.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;
use tze_hud_protocol::session::SharedState;
use tze_hud_scene::render_wake::RenderWakeNotifier;

use crate::shell::ChromeState;
use crate::shell::safe_mode::{enter_safe_mode, exit_safe_mode};

#[derive(Clone)]
pub(crate) struct PauseControl {
    shared: Arc<Mutex<SharedState>>,
    chrome: Arc<RwLock<ChromeState>>,
    /// The authoritative safe-mode flag (`SharedState::safe_mode_atomic`).
    paused: Arc<AtomicBool>,
    render_wake: RenderWakeNotifier,
}

impl PauseControl {
    pub(crate) fn new(
        shared: Arc<Mutex<SharedState>>,
        chrome: Arc<RwLock<ChromeState>>,
        paused: Arc<AtomicBool>,
        render_wake: RenderWakeNotifier,
    ) -> Self {
        Self {
            shared,
            chrome,
            paused,
            render_wake,
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Pause (`true`) or resume (`false`) agents; a no-op when already there.
    /// Never waits on an agent: hung sessions are notified best-effort.
    pub(crate) async fn set(&self, paused: bool) {
        if paused == self.is_paused() {
            return;
        }
        if paused {
            let result = enter_safe_mode(&self.shared, &self.chrome).await;
            tracing::info!(
                leases_suspended = result.leases_suspended,
                sessions_notified = result.sessions_notified,
                "control panel: agents paused (safe mode entered)"
            );
            self.render_wake.notify();
        } else {
            let result = exit_safe_mode(&self.shared, &self.chrome).await;
            tracing::info!(
                exited = result.exited,
                leases_resumed = result.leases_resumed,
                sessions_notified = result.sessions_notified,
                "control panel: agents resumed (safe mode exited)"
            );
            if result.exited {
                self.render_wake.notify();
            }
        }
    }
}
