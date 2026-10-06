//! Safe mode — suspend and resume every agent's leases.
//!
//! Safe mode is a human override: it works without any agent's cooperation and
//! cannot be vetoed. [`enter_safe_mode`] and [`exit_safe_mode`] are the sole
//! writers of the safe-mode state.
//!
//! # Entry
//!
//! 1. Suspend all ACTIVE leases (NOT revoke — identity is preserved).
//! 2. Set `SharedState.safe_mode_atomic = true` so mutation intake rejects
//!    batches and the input thread captures input lock-free.
//! 3. Broadcast `SessionSuspended` to all connected sessions.
//! 4. Set `ChromeState.safe_mode_active = true` so the windowed frame draws the
//!    overlay.
//!
//! # Exit
//!
//! 1. Clear `ChromeState.safe_mode_active`.
//! 2. Resume all SUSPENDED leases to ACTIVE (TTL excludes the suspension).
//! 3. Clear `SharedState.safe_mode_atomic` so mutations are accepted again.
//! 4. Broadcast `SessionResumed`.
//!
//! Both operations are idempotent. `SharedState.safe_mode_atomic` is the single
//! source of truth, so any caller sharing the same state can exit safe mode.
//! Lease TTL accounting and session notification timestamps use the scene's
//! injected clock.

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

use tze_hud_protocol::proto::session::{
    ServerMessage, SessionResumed, SessionSuspended, server_message::Payload as ServerPayload,
};
use tze_hud_protocol::session::SharedState;
use tze_hud_scene::types::LeaseState;

use super::chrome::ChromeState;

/// Result of [`enter_safe_mode`].
#[derive(Debug)]
pub struct SafeModeEntryResult {
    /// Number of leases that were suspended.
    pub leases_suspended: usize,
    /// Number of sessions that received `SessionSuspended`.
    pub sessions_notified: usize,
}

/// Result of [`exit_safe_mode`].
#[derive(Debug)]
pub struct SafeModeExitResult {
    /// Whether this call transitioned safe mode from active to inactive.
    ///
    /// An idempotent exit has no compositor-visible effect, so callers use
    /// this to avoid scheduling render work for a no-op signal.
    pub exited: bool,
    /// Number of leases that were resumed.
    pub leases_resumed: usize,
    /// Number of sessions that received `SessionResumed`.
    pub sessions_notified: usize,
}

/// Enter safe mode. A no-op result is returned if safe mode is already active.
pub async fn enter_safe_mode(
    shared_state: &Arc<Mutex<SharedState>>,
    chrome_state: &Arc<RwLock<ChromeState>>,
) -> SafeModeEntryResult {
    let (leases_suspended, sessions_notified) = {
        let st = shared_state.lock().await;

        // Guard: idempotent. `safe_mode_atomic` is the single source of truth.
        if st.safe_mode_atomic.load(Ordering::Acquire) {
            return SafeModeEntryResult {
                leases_suspended: 0,
                sessions_notified: 0,
            };
        }

        // Suspend all ACTIVE leases (NOT revoke).
        let (leases_suspended, now_us) = {
            let mut scene = st.scene.lock().await;
            let now_ms = scene.now_millis();
            let now_us = scene.now_wall_us();
            scene.suspend_all_leases(now_ms);
            let suspended = scene
                .leases
                .values()
                .filter(|l| l.state == LeaseState::Suspended)
                .count();
            (suspended, now_us)
        };

        // The event thread reads this lock-free and mutation intake reads it
        // under the SharedState lock; Release pairs with their Acquire loads.
        st.safe_mode_atomic.store(true, Ordering::Release);

        // sequence = 0: the session handler stamps the per-session sequence
        // before delivery; a shared broadcast cannot.
        let suspended_msg = ServerMessage {
            sequence: 0,
            timestamp_wall_us: now_us,
            payload: Some(ServerPayload::SessionSuspended(SessionSuspended {
                reason: "safe_mode_entered".to_string(),
                timestamp_wall_us: now_us,
            })),
        };
        let sessions_notified = st.sessions.broadcast_server_message(suspended_msg);

        (leases_suspended, sessions_notified)
    };

    // Overlay becomes visible on the next frame. Recover from a poisoned lock:
    // safe mode is a failure-recovery path and must stay resilient.
    chrome_state
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .safe_mode_active = true;

    SafeModeEntryResult {
        leases_suspended,
        sessions_notified,
    }
}

/// Exit safe mode. A no-op result is returned if safe mode is not active.
///
/// Agents do not re-request leases: identity, capability scope, and resource
/// budget are preserved across the ACTIVE -> SUSPENDED -> ACTIVE cycle.
pub async fn exit_safe_mode(
    shared_state: &Arc<Mutex<SharedState>>,
    chrome_state: &Arc<RwLock<ChromeState>>,
) -> SafeModeExitResult {
    let st = shared_state.lock().await;
    if !st.safe_mode_atomic.load(Ordering::Acquire) {
        return SafeModeExitResult {
            exited: false,
            leases_resumed: 0,
            sessions_notified: 0,
        };
    }

    // Dismiss the overlay first so the next frame has none.
    chrome_state
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .safe_mode_active = false;

    let (leases_resumed, now_us) = {
        let mut scene = st.scene.lock().await;
        let now_ms = scene.now_millis();
        let now_us = scene.now_wall_us();
        let suspended = scene
            .leases
            .values()
            .filter(|l| l.state == LeaseState::Suspended)
            .count();
        scene.resume_all_leases(now_ms);
        (suspended, now_us)
    };

    // Mutation intake accepts new batches again.
    st.safe_mode_atomic.store(false, Ordering::Release);

    let resumed_msg = ServerMessage {
        sequence: 0,
        timestamp_wall_us: now_us,
        payload: Some(ServerPayload::SessionResumed(SessionResumed {
            timestamp_wall_us: now_us,
        })),
    };
    let sessions_notified = st.sessions.broadcast_server_message(resumed_msg);

    SafeModeExitResult {
        exited: true,
        leases_resumed,
        sessions_notified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use tokio::sync::Mutex;
    use tze_hud_protocol::session::{SessionRegistry, SharedState};
    use tze_hud_protocol::token::TokenStore;
    use tze_hud_scene::graph::SceneGraph;
    use tze_hud_scene::types::{LeaseState, SceneId};

    fn make_shared_state() -> Arc<Mutex<SharedState>> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        Arc::new(Mutex::new(SharedState {
            scene: Arc::new(Mutex::new(SceneGraph::new(1920.0, 1080.0))),
            sessions: SessionRegistry::new(),
            resource_store: tze_hud_resource::ResourceStore::new(
                tze_hud_resource::ResourceStoreConfig::default(),
            ),
            runtime_widget_store: None,
            element_store: tze_hud_scene::element_store::ElementStore::default(),
            element_store_path: None,
            safe_mode_atomic: Arc::new(AtomicBool::new(false)),
            active_tab_mirror: Arc::new(std::sync::Mutex::new(None)),
            freeze_active: false,
            token_store: TokenStore::new(),
            input_capture_tx: None,
            input_capture_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tile_placement: Default::default(),
        }))
    }

    struct Fixture {
        shared: Arc<Mutex<SharedState>>,
        chrome: Arc<RwLock<ChromeState>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                shared: make_shared_state(),
                chrome: Arc::new(RwLock::new(ChromeState::new())),
            }
        }

        async fn enter(&self) -> SafeModeEntryResult {
            enter_safe_mode(&self.shared, &self.chrome).await
        }

        async fn exit(&self) -> SafeModeExitResult {
            exit_safe_mode(&self.shared, &self.chrome).await
        }

        async fn grant(&self, namespace: &str) -> SceneId {
            let st = self.shared.lock().await;
            st.scene.lock().await.grant_lease(namespace, 60_000)
        }

        async fn lease_state(&self, id: SceneId) -> LeaseState {
            let st = self.shared.lock().await;
            st.scene.lock().await.leases[&id].state
        }

        async fn atomic(&self) -> bool {
            self.shared
                .lock()
                .await
                .safe_mode_atomic
                .load(Ordering::Acquire)
        }

        fn overlay(&self) -> bool {
            self.chrome.read().unwrap().safe_mode_active
        }
    }

    /// WHEN the viewer enters safe mode THEN every agent's active leases are
    /// suspended (not revoked), mutation intake is gated, and the overlay flag
    /// is set.
    #[tokio::test]
    async fn test_enter_safe_mode_suspends_active_leases() {
        let fx = Fixture::new();
        let lease_a = fx.grant("agent.alpha").await;
        let lease_b = fx.grant("agent.beta").await;
        assert_eq!(fx.lease_state(lease_a).await, LeaseState::Active);
        assert!(!fx.overlay());

        let result = fx.enter().await;

        assert_eq!(result.leases_suspended, 2);
        assert_eq!(
            fx.lease_state(lease_a).await,
            LeaseState::Suspended,
            "lease must be SUSPENDED not REVOKED"
        );
        assert_eq!(fx.lease_state(lease_b).await, LeaseState::Suspended);
        assert!(fx.atomic().await);
        assert!(fx.overlay(), "ChromeState drives the overlay");
    }

    /// Resume returns every lease to ACTIVE without a re-request and dismisses
    /// the overlay.
    #[tokio::test]
    async fn test_exit_safe_mode_resumes_suspended_leases() {
        let fx = Fixture::new();
        let lease_id = fx.grant("agent.alpha").await;
        fx.enter().await;

        let result = fx.exit().await;

        assert!(result.exited);
        assert_eq!(result.leases_resumed, 1);
        assert_eq!(fx.lease_state(lease_id).await, LeaseState::Active);
        assert!(!fx.atomic().await);
        assert!(!fx.overlay());
    }

    /// Agents do NOT re-request leases: identity is preserved.
    #[tokio::test]
    async fn test_lease_identity_preserved_across_suspend_resume() {
        let fx = Fixture::new();
        let lease_id = fx.grant("agent.alpha").await;
        let (ns_before, session_before) = {
            let st = fx.shared.lock().await;
            let scene = st.scene.lock().await;
            let l = &scene.leases[&lease_id];
            (l.namespace.clone(), l.session_id)
        };

        fx.enter().await;
        fx.exit().await;

        let st = fx.shared.lock().await;
        let scene = st.scene.lock().await;
        let l = &scene.leases[&lease_id];
        assert_eq!(l.namespace, ns_before);
        assert_eq!(l.session_id, session_before);
    }

    /// TTL pause: suspension time is excluded from TTL accounting.
    #[tokio::test]
    async fn test_ttl_excluded_during_suspension() {
        use tze_hud_scene::clock::TestClock;

        let fx = Fixture::new();
        let clock = TestClock::new(1_000);
        {
            let st = fx.shared.lock().await;
            *st.scene.lock().await =
                SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        }
        let lease_id = fx.grant("agent.alpha").await;
        let original_ttl = {
            let st = fx.shared.lock().await;
            st.scene.lock().await.leases[&lease_id].ttl_ms
        };
        let elapsed_before_suspension = 12_000;
        clock.advance(elapsed_before_suspension);

        fx.enter().await;
        let remaining_ttl = original_ttl - elapsed_before_suspension;
        {
            let st = fx.shared.lock().await;
            let scene = st.scene.lock().await;
            let lease = &scene.leases[&lease_id];
            assert_eq!(lease.state, LeaseState::Suspended);
            assert_eq!(lease.suspended_at_ms, Some(scene.now_millis()));
            assert_eq!(lease.ttl_remaining_at_suspend_ms, Some(remaining_ttl));
        }

        // Longer than the original TTL, but within the suspension timeout.
        clock.advance(120_000);
        {
            let st = fx.shared.lock().await;
            let mut scene = st.scene.lock().await;
            assert!(scene.expire_leases().is_empty());
            assert_eq!(scene.leases[&lease_id].state, LeaseState::Suspended);
        }
        fx.exit().await;

        let post_resume_ttl = {
            let st = fx.shared.lock().await;
            let scene = st.scene.lock().await;
            let lease = &scene.leases[&lease_id];
            assert_eq!(lease.state, LeaseState::Active);
            assert_eq!(lease.granted_at_ms, scene.now_millis());
            lease.ttl_ms
        };
        assert_eq!(post_resume_ttl, remaining_ttl);

        clock.advance(remaining_ttl - 1);
        {
            let st = fx.shared.lock().await;
            let mut scene = st.scene.lock().await;
            assert!(scene.expire_leases().is_empty());
            assert_eq!(scene.leases[&lease_id].state, LeaseState::Active);
        }
        clock.advance(1);
        let st = fx.shared.lock().await;
        let mut scene = st.scene.lock().await;
        let expiries = scene.expire_leases();
        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].lease_id, lease_id);
        assert_eq!(scene.leases[&lease_id].state, LeaseState::Expired);
    }

    /// While safe mode is active `SharedState.safe_mode_atomic` is set, which
    /// makes the session server reject MutationBatch with SAFE_MODE_ACTIVE.
    #[tokio::test]
    async fn test_mutations_rejected_via_shared_state_flag() {
        let fx = Fixture::new();

        fx.enter().await;
        assert!(fx.atomic().await, "session server gates on this flag");

        fx.exit().await;
        assert!(!fx.atomic().await, "flag clears after exit");
    }

    /// Entering twice, or exiting while inactive, does nothing.
    #[tokio::test]
    async fn test_enter_and_exit_are_idempotent() {
        let fx = Fixture::new();
        let lease_id = fx.grant("agent.alpha").await;

        let noop_exit = fx.exit().await;
        assert!(!noop_exit.exited);
        assert_eq!(noop_exit.leases_resumed, 0);

        fx.enter().await;
        let second = fx.enter().await;
        assert_eq!(second.leases_suspended, 0, "second entry is a no-op");
        assert_eq!(fx.lease_state(lease_id).await, LeaseState::Suspended);
    }

    /// Sessions receive SessionSuspended on entry and SessionResumed on exit.
    #[tokio::test]
    async fn test_session_notification_broadcast() {
        use tokio::sync::mpsc;

        let fx = Fixture::new();
        let (tx, mut rx) = mpsc::channel(8);
        {
            let mut st = fx.shared.lock().await;
            let session = st.sessions.register("agent.notify_test", &[]);
            assert!(
                st.sessions
                    .register_server_message_tx(&session.session_id, tx)
            );
        }

        let entry = fx.enter().await;
        assert_eq!(entry.sessions_notified, 1);
        let msg = rx.try_recv().unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(ServerPayload::SessionSuspended(_))
        ));

        let exit = fx.exit().await;
        assert_eq!(exit.sessions_notified, 1);
        let msg = rx.try_recv().unwrap().unwrap();
        assert!(matches!(
            msg.payload,
            Some(ServerPayload::SessionResumed(_))
        ));
    }
}
