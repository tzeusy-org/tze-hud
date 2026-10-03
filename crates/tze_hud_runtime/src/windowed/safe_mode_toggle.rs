#![cfg_attr(not(target_os = "windows"), allow(dead_code))]
//! Human safe-mode toggle (hud-jm8nq.10): the global-hotkey bridge.
//!
//! A global hotkey fires on its own OS thread; the unfocused, click-through
//! overlay never sees the keystroke. The hotkey thread sends `()` on an
//! unbounded channel and a task on the network runtime flips safe mode, so the
//! human override never waits on the winit loop or on an agent. Direction is
//! decided *inside* the task from the authoritative `safe_mode_atomic`, which
//! serialises rapid presses.

use std::sync::{Arc, RwLock, atomic::Ordering};

use tokio::sync::{Mutex, mpsc};

use crate::shell::{ChromeState, SafeModeController};
use crate::threads::ShutdownToken;
use tze_hud_protocol::session::SharedState;
use tze_hud_scene::render_wake::RenderWakeNotifier;

/// What a toggle request does given the current safe-mode state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SafeModeAction {
    Enter,
    Exit,
}

/// The enter/exit state machine: a toggle exits when active, enters otherwise.
pub(super) fn action_for_toggle(safe_mode_active: bool) -> SafeModeAction {
    if safe_mode_active {
        SafeModeAction::Exit
    } else {
        SafeModeAction::Enter
    }
}

/// Spawn the bridge task and return the sender the hotkey thread signals.
///
/// Each signal that changes state wakes the compositor once so the overlay is
/// drawn or cleared; a no-op signal costs nothing.
pub(super) fn spawn_safe_mode_toggle_bridge(
    handle: &tokio::runtime::Handle,
    shared: Arc<Mutex<SharedState>>,
    chrome: Arc<RwLock<ChromeState>>,
    render_wake: RenderWakeNotifier,
    shutdown: ShutdownToken,
) -> mpsc::UnboundedSender<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    handle.spawn(async move {
        let mut shutdown_rx = shutdown.subscribe();
        loop {
            tokio::select! {
                signal = rx.recv() => {
                    if signal.is_none() {
                        break;
                    }
                    toggle_safe_mode(&shared, &chrome, &render_wake).await;
                }
                _ = shutdown_rx.recv() => break,
            }
        }
    });
    tx
}

async fn toggle_safe_mode(
    shared: &Arc<Mutex<SharedState>>,
    chrome: &Arc<RwLock<ChromeState>>,
    render_wake: &RenderWakeNotifier,
) {
    let active = shared.lock().await.safe_mode_atomic.load(Ordering::Acquire);
    let mut ctrl = SafeModeController::new_headless(Arc::clone(shared), Arc::clone(chrome));
    match action_for_toggle(active) {
        SafeModeAction::Enter => {
            let result = ctrl.enter_safe_mode_viewer_action().await;
            tracing::info!(
                leases_suspended = result.leases_suspended,
                sessions_notified = result.sessions_notified,
                "safe-mode hotkey: entered safe mode"
            );
            render_wake.notify();
        }
        SafeModeAction::Exit => {
            let result = ctrl.exit_safe_mode().await;
            tracing::info!(
                exited = result.exited,
                leases_resumed = result.leases_resumed,
                sessions_notified = result.sessions_notified,
                "safe-mode hotkey: exited safe mode"
            );
            if result.exited {
                render_wake.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windowed::WindowedRuntimeState;
    use tze_hud_scene::types::LeaseState;

    #[test]
    fn toggle_enters_when_inactive_and_exits_when_active() {
        assert_eq!(action_for_toggle(false), SafeModeAction::Enter);
        assert_eq!(action_for_toggle(true), SafeModeAction::Exit);
    }

    /// A hotkey signal enters safe mode and suspends leases; a second exits and
    /// resumes them. Drives the production bridge against the headless runtime
    /// state's real shared scene, chrome state and safe-mode atomic.
    #[tokio::test(flavor = "multi_thread")]
    async fn hotkey_event_enters_safe_mode_and_suspends_leases() {
        let state = WindowedRuntimeState::new_headless();
        let lease_id = state
            .shared_state
            .lock()
            .await
            .scene
            .lock()
            .await
            .grant_lease("agent", 60_000);
        let tx = spawn_safe_mode_toggle_bridge(
            &tokio::runtime::Handle::current(),
            Arc::clone(&state.shared_state),
            Arc::clone(&state.chrome_state),
            state.wake.render_notifier(),
            state.shutdown.clone(),
        );

        let lease_state = || async {
            let st = state.shared_state.lock().await;
            let scene = st.scene.lock().await;
            scene.leases[&lease_id].state
        };
        let atomic = Arc::clone(&state.safe_mode_atomic);
        let wait_for_active = |want: bool| {
            let atomic = Arc::clone(&atomic);
            async move {
                for _ in 0..200 {
                    if atomic.load(Ordering::Acquire) == want {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                panic!("safe mode never became {want}");
            }
        };

        tx.send(()).unwrap();
        wait_for_active(true).await;
        assert_eq!(lease_state().await, LeaseState::Suspended);
        assert!(state.chrome_state.read().unwrap().safe_mode_active);

        tx.send(()).unwrap();
        wait_for_active(false).await;
        assert_eq!(lease_state().await, LeaseState::Active);
        assert!(!state.chrome_state.read().unwrap().safe_mode_active);
    }
}
