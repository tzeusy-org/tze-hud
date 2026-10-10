//! Runtime implementation of the protocol's mutation budget gate.
//!
//! Budgets are hard caps: a session registers with a [`ResourceBudget`], and
//! each mutation batch is admitted only if the session's tiles, texture bytes,
//! update rate, and nodes-per-tile stay within that budget and the runtime-wide
//! aggregate limits. Over-budget batches are rejected; nothing escalates.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tze_hud_protocol::session_server::{
    MutationBudgetDecision, MutationBudgetEnforcer as MutationBudgetEnforcerContract,
    MutationBudgetUsage,
};
use tze_hud_scene::types::{ResourceBudget, SceneId};

/// Default cap on concurrent guest (non-resident) sessions.
pub const DEFAULT_MAX_GUEST_SESSIONS: u32 = 64;

/// Window over which `max_update_rate_hz` is measured.
const UPDATE_RATE_WINDOW: Duration = Duration::from_secs(1);

/// Shared enforcement object used by production gRPC session handlers.
pub struct RuntimeMutationBudgetEnforcer {
    inner: Mutex<AggregateBudgetState>,
    #[cfg(test)]
    clock: Option<std::sync::Arc<dyn Fn() -> Instant + Send + Sync>>,
}

struct AggregateBudgetState {
    sessions: HashMap<SceneId, SessionBudgetState>,
    resident_sessions: u32,
    guest_sessions: u32,
    leased_tiles: u32,
    leased_texture_bytes: u64,
    max_resident_sessions: u32,
    max_guest_sessions: u32,
    max_leased_tiles: u32,
    max_leased_texture_bytes: u64,
}

struct SessionBudgetState {
    namespace: String,
    resident: bool,
    budget: ResourceBudget,
    tiles: u32,
    texture_bytes: u64,
    recent_updates: VecDeque<Instant>,
}

impl AggregateBudgetState {
    /// Count recent attempts by the requesting agent's other live sessions.
    fn namespace_peer_updates(
        &self,
        namespace: &str,
        replaced_session: Option<SceneId>,
        now: Instant,
    ) -> usize {
        self.sessions
            .iter()
            .filter(|(id, session)| {
                Some(**id) != replaced_session && session.namespace == namespace
            })
            .map(|(_, session)| {
                let updates = &session.recent_updates;
                let expired =
                    updates.partition_point(|t| now.duration_since(*t) >= UPDATE_RATE_WINDOW);
                updates.len() - expired
            })
            .sum()
    }

    /// Check the proposed session usage together with its same-agent peers.
    /// Replacing one session's usage preserves every peer during rollback/resume.
    fn check_namespace_usage(
        &self,
        namespace: &str,
        replaced_session: Option<SceneId>,
        proposed: MutationBudgetUsage,
        budget: &ResourceBudget,
    ) -> Option<String> {
        let (tiles, texture_bytes) = self
            .sessions
            .iter()
            .filter(|(id, session)| {
                Some(**id) != replaced_session && session.namespace == namespace
            })
            .fold(
                (
                    u128::from(proposed.tiles),
                    u128::from(proposed.texture_bytes),
                ),
                |(tiles, texture_bytes), (_, session)| {
                    (
                        tiles + u128::from(session.tiles),
                        texture_bytes + u128::from(session.texture_bytes),
                    )
                },
            );
        if tiles > u128::from(budget.max_tiles) {
            return Some(format!(
                "agent tiles proposed={tiles} limit={}",
                budget.max_tiles
            ));
        }
        if texture_bytes > u128::from(budget.max_texture_bytes) {
            return Some(format!(
                "agent texture_bytes proposed={texture_bytes} limit={}",
                budget.max_texture_bytes
            ));
        }
        None
    }
}

impl SessionBudgetState {
    /// Check a batch against this session's budget, recording it for rate
    /// tracking. Returns the rejection message, if any.
    fn check(
        &mut self,
        proposed_tiles: u32,
        proposed_texture_bytes: u64,
        max_nodes_in_batch: u32,
        now: Instant,
        peer_updates: usize,
    ) -> Option<String> {
        if proposed_tiles > self.budget.max_tiles {
            return Some(format!(
                "tiles proposed={proposed_tiles} limit={}",
                self.budget.max_tiles
            ));
        }
        if proposed_texture_bytes > self.budget.max_texture_bytes {
            return Some(format!(
                "texture_bytes proposed={proposed_texture_bytes} limit={}",
                self.budget.max_texture_bytes
            ));
        }
        if max_nodes_in_batch > self.budget.max_nodes_per_tile {
            return Some(format!(
                "nodes_per_tile proposed={max_nodes_in_batch} limit={}",
                self.budget.max_nodes_per_tile
            ));
        }
        while self
            .recent_updates
            .front()
            .is_some_and(|t| now.duration_since(*t) >= UPDATE_RATE_WINDOW)
        {
            self.recent_updates.pop_front();
        }
        self.recent_updates.push_back(now);
        let rate_hz = (self.recent_updates.len() + peer_updates) as f32;
        if rate_hz > self.budget.max_update_rate_hz {
            return Some(format!(
                "update_rate_hz current={rate_hz} limit={}",
                self.budget.max_update_rate_hz
            ));
        }
        None
    }
}

fn apply_delta_u32(value: u32, delta: i32) -> u32 {
    if delta >= 0 {
        value.saturating_add(delta as u32)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn apply_delta_u64(value: u64, delta: i64) -> u64 {
    if delta >= 0 {
        value.saturating_add(delta as u64)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

impl RuntimeMutationBudgetEnforcer {
    pub fn new() -> Self {
        Self::with_limits(u32::MAX, u32::MAX, u64::MAX)
    }

    pub fn with_limits(
        max_resident_sessions: u32,
        max_leased_tiles: u32,
        max_leased_texture_bytes: u64,
    ) -> Self {
        Self::with_session_limits(
            max_resident_sessions,
            DEFAULT_MAX_GUEST_SESSIONS,
            max_leased_tiles,
            max_leased_texture_bytes,
        )
    }

    pub fn with_session_limits(
        max_resident_sessions: u32,
        max_guest_sessions: u32,
        max_leased_tiles: u32,
        max_leased_texture_bytes: u64,
    ) -> Self {
        Self {
            #[cfg(test)]
            clock: None,
            inner: Mutex::new(AggregateBudgetState {
                sessions: HashMap::new(),
                resident_sessions: 0,
                guest_sessions: 0,
                leased_tiles: 0,
                leased_texture_bytes: 0,
                max_resident_sessions,
                max_guest_sessions,
                max_leased_tiles,
                max_leased_texture_bytes,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AggregateBudgetState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.clock {
            return clock();
        }
        Instant::now()
    }
}

impl Default for RuntimeMutationBudgetEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

fn exhausted(message: String) -> MutationBudgetDecision {
    MutationBudgetDecision::Reject {
        error_code: "RESOURCE_EXHAUSTED",
        message,
    }
}

impl MutationBudgetEnforcerContract for RuntimeMutationBudgetEnforcer {
    fn register_session(
        &self,
        session_id: SceneId,
        namespace: String,
        budget: ResourceBudget,
        resident: bool,
        initial_usage: MutationBudgetUsage,
    ) -> MutationBudgetDecision {
        let mut state = self.lock();
        if state.sessions.contains_key(&session_id) {
            return exhausted(format!("session_id {session_id} is already registered"));
        }
        if resident && state.resident_sessions >= state.max_resident_sessions {
            return exhausted(format!(
                "resident_sessions current={} limit={}",
                state.resident_sessions, state.max_resident_sessions
            ));
        }
        if !resident && state.guest_sessions >= state.max_guest_sessions {
            return exhausted(format!(
                "guest_sessions current={} limit={}",
                state.guest_sessions, state.max_guest_sessions
            ));
        }
        let proposed_tiles = state.leased_tiles.saturating_add(initial_usage.tiles);
        if proposed_tiles > state.max_leased_tiles {
            return exhausted(format!(
                "leased_tiles current={} restored={} limit={}",
                state.leased_tiles, initial_usage.tiles, state.max_leased_tiles
            ));
        }
        let proposed_texture = state
            .leased_texture_bytes
            .saturating_add(initial_usage.texture_bytes);
        if proposed_texture > state.max_leased_texture_bytes {
            return exhausted(format!(
                "agent_leased_texture_bytes current={} restored={} limit={}",
                state.leased_texture_bytes,
                initial_usage.texture_bytes,
                state.max_leased_texture_bytes
            ));
        }
        if let Some(message) = state.check_namespace_usage(&namespace, None, initial_usage, &budget)
        {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                message: format!("restored agent usage rejected: {message}"),
            };
        }
        let now = self.now();
        let peer_updates = state.namespace_peer_updates(&namespace, None, now);
        let mut session = SessionBudgetState {
            namespace,
            resident,
            budget,
            tiles: 0,
            texture_bytes: 0,
            recent_updates: VecDeque::new(),
        };
        if let Some(message) = session.check(
            initial_usage.tiles,
            initial_usage.texture_bytes,
            0,
            now,
            peer_updates,
        ) {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                message: format!("restored session usage rejected: {message}"),
            };
        }
        session.tiles = initial_usage.tiles;
        session.texture_bytes = initial_usage.texture_bytes;
        state.sessions.insert(session_id, session);
        if resident {
            state.resident_sessions = state.resident_sessions.saturating_add(1);
        } else {
            state.guest_sessions = state.guest_sessions.saturating_add(1);
        }
        state.leased_tiles = proposed_tiles;
        state.leased_texture_bytes = proposed_texture;
        MutationBudgetDecision::Allow
    }

    fn remove_session(&self, session_id: SceneId) {
        let mut state = self.lock();
        let Some(session) = state.sessions.remove(&session_id) else {
            return;
        };
        state.leased_tiles = state.leased_tiles.saturating_sub(session.tiles);
        state.leased_texture_bytes = state
            .leased_texture_bytes
            .saturating_sub(session.texture_bytes);
        if session.resident {
            state.resident_sessions = state.resident_sessions.saturating_sub(1);
        } else {
            state.guest_sessions = state.guest_sessions.saturating_sub(1);
        }
    }

    fn reserve_mutation(
        &self,
        session_id: SceneId,
        delta_tiles: i32,
        delta_texture_bytes: i64,
        max_nodes_in_batch: u32,
    ) -> MutationBudgetDecision {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(session) = state.sessions.get(&session_id) else {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_SESSION_UNKNOWN",
                message: format!("session_id {session_id} is not registered"),
            };
        };
        let proposed_tiles = apply_delta_u32(state.leased_tiles, delta_tiles);
        if proposed_tiles > state.max_leased_tiles {
            return exhausted(format!(
                "leased_tiles current={} requested_delta={} limit={}",
                state.leased_tiles, delta_tiles, state.max_leased_tiles
            ));
        }
        let proposed_texture = apply_delta_u64(state.leased_texture_bytes, delta_texture_bytes);
        if proposed_texture > state.max_leased_texture_bytes {
            return exhausted(format!(
                "agent_leased_texture_bytes current={} requested_delta={} limit={}",
                state.leased_texture_bytes, delta_texture_bytes, state.max_leased_texture_bytes
            ));
        }
        let session_tiles = apply_delta_u32(session.tiles, delta_tiles);
        let session_texture = apply_delta_u64(session.texture_bytes, delta_texture_bytes);
        if let Some(message) = state.check_namespace_usage(
            &session.namespace,
            Some(session_id),
            MutationBudgetUsage {
                tiles: session_tiles,
                texture_bytes: session_texture,
            },
            &session.budget,
        ) {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                message,
            };
        }
        let now = self.now();
        let peer_updates = state.namespace_peer_updates(&session.namespace, Some(session_id), now);
        let session = state
            .sessions
            .get_mut(&session_id)
            .expect("session remains registered under the state lock");
        if let Some(message) = session.check(
            session_tiles,
            session_texture,
            max_nodes_in_batch,
            now,
            peer_updates,
        ) {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                message,
            };
        }
        session.tiles = session_tiles;
        session.texture_bytes = session_texture;
        state.leased_tiles = proposed_tiles;
        state.leased_texture_bytes = proposed_texture;
        MutationBudgetDecision::Allow
    }

    fn rollback_mutation(&self, session_id: SceneId, delta_tiles: i32, delta_texture_bytes: i64) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let Some(session) = state.sessions.get_mut(&session_id) else {
            return;
        };
        session.tiles = apply_delta_u32(session.tiles, delta_tiles.saturating_neg());
        session.texture_bytes =
            apply_delta_u64(session.texture_bytes, delta_texture_bytes.saturating_neg());
        state.leased_tiles = apply_delta_u32(state.leased_tiles, delta_tiles.saturating_neg());
        state.leased_texture_bytes = apply_delta_u64(
            state.leased_texture_bytes,
            delta_texture_bytes.saturating_neg(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_budget_rejects_mutation_above_tile_limit() {
        let enforcer = RuntimeMutationBudgetEnforcer::new();
        let session_id = SceneId::new();
        assert_eq!(
            enforcer.register_session(
                session_id,
                "agent-a".to_string(),
                ResourceBudget {
                    max_tiles: 1,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            enforcer.reserve_mutation(session_id, 1, 0, 1),
            MutationBudgetDecision::Allow
        );

        assert!(matches!(
            enforcer.reserve_mutation(session_id, 1, 0, 1),
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                ..
            }
        ));

        // Drive the real registration/reservation carrier without wall sleeps.
        // Successful registrations and rate-reaching rejected attempts are
        // existing samples; rollback and disconnect retain their existing fate.
        let t0 = Instant::now();
        let clock = std::sync::Arc::new(Mutex::new(t0));
        let mut rate = RuntimeMutationBudgetEnforcer::new();
        let read_clock = clock.clone();
        rate.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let a = SceneId::new();
        let b = SceneId::new();
        let rate_budget = ResourceBudget {
            max_update_rate_hz: 3.0,
            ..ResourceBudget::default()
        };
        for id in [a, b] {
            assert_eq!(
                rate.register_session(
                    id,
                    "same-agent".into(),
                    rate_budget.clone(),
                    true,
                    MutationBudgetUsage::default(),
                ),
                MutationBudgetDecision::Allow
            );
        }
        let mut rows = Vec::new();
        let first = rate.reserve_mutation(a, 1, 10, 1);
        assert_eq!(first, MutationBudgetDecision::Allow);
        let before = {
            let state = rate.lock();
            (
                state.leased_tiles,
                state.leased_texture_bytes,
                state.sessions[&b].tiles,
                state.sessions[&b].texture_bytes,
            )
        };
        let fourth = rate.reserve_mutation(b, 1, 10, 1);
        let after = {
            let state = rate.lock();
            assert_eq!(state.sessions[&a].recent_updates.len(), 2);
            assert_eq!(state.sessions[&b].recent_updates.len(), 2);
            (
                state.leased_tiles,
                state.leased_texture_bytes,
                state.sessions[&b].tiles,
                state.sessions[&b].texture_bytes,
            )
        };
        rows.push((
            "same-agent fourth counted attempt rejects",
            matches!(
                fourth,
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));
        rows.push((
            "rate rejection leaves resource usage atomic",
            before == after,
        ));

        let other = SceneId::new();
        assert_eq!(
            rate.register_session(
                other,
                "distinct-agent".into(),
                rate_budget.clone(),
                false,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        for _ in 0..2 {
            assert_eq!(
                rate.reserve_mutation(other, 0, 0, 1),
                MutationBudgetDecision::Allow
            );
        }
        assert!(matches!(
            rate.reserve_mutation(other, 0, 0, 1),
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                ..
            }
        ));
        assert_eq!(rate.lock().sessions[&other].recent_updates.len(), 4);

        *clock.lock().unwrap() = t0 + Duration::from_millis(999);
        rows.push((
            "same-agent history remains at 999ms",
            matches!(
                rate.reserve_mutation(a, 0, 0, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));
        *clock.lock().unwrap() = t0 + UPDATE_RATE_WINDOW;
        assert_eq!(
            rate.reserve_mutation(a, 0, 0, 1),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            rate.reserve_mutation(b, 0, 0, 1),
            MutationBudgetDecision::Allow
        );
        rows.push((
            "one-second expiry keeps the 999ms attempt",
            matches!(
                rate.reserve_mutation(b, 0, 0, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));
        assert_eq!(rate.lock().sessions[&a].recent_updates.len(), 2);
        assert_eq!(rate.lock().sessions[&b].recent_updates.len(), 2);

        // Early resource/node/unknown-session failures do not reach rate intake.
        let mut early = RuntimeMutationBudgetEnforcer::with_limits(4, 100, 1000);
        let read_clock = clock.clone();
        early.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let early_id = SceneId::new();
        assert_eq!(
            early.register_session(
                early_id,
                "early".into(),
                ResourceBudget {
                    max_tiles: 1,
                    max_texture_bytes: 10,
                    max_nodes_per_tile: 1,
                    max_update_rate_hz: 3.0,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        for (tiles, bytes, nodes) in [(2, 0, 1), (0, 11, 1), (0, 0, 2)] {
            assert!(matches!(
                early.reserve_mutation(early_id, tiles, bytes, nodes),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ));
            assert_eq!(early.lock().sessions[&early_id].recent_updates.len(), 1);
        }
        assert!(matches!(
            early.reserve_mutation(SceneId::new(), 0, 0, 1),
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_SESSION_UNKNOWN",
                ..
            }
        ));
        assert_eq!(
            early.reserve_mutation(early_id, 1, 10, 1),
            MutationBudgetDecision::Allow
        );
        early.rollback_mutation(early_id, 1, 10);
        {
            let state = early.lock();
            assert_eq!((state.leased_tiles, state.leased_texture_bytes), (0, 0));
            assert_eq!(state.sessions[&early_id].recent_updates.len(), 2);
        }
        let mut global = RuntimeMutationBudgetEnforcer::with_limits(1, 0, 0);
        let read_clock = clock.clone();
        global.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let global_id = SceneId::new();
        assert_eq!(
            global.register_session(
                global_id,
                "global".into(),
                ResourceBudget::default(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        for (tiles, bytes) in [(1, 0), (0, 1)] {
            assert!(matches!(
                global.reserve_mutation(global_id, tiles, bytes, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_EXHAUSTED",
                    ..
                }
            ));
            assert_eq!(global.lock().sessions[&global_id].recent_updates.len(), 1);
        }

        // Removal discards only that queue; resume registration is a fresh sample.
        let mut lifecycle = RuntimeMutationBudgetEnforcer::new();
        let read_clock = clock.clone();
        lifecycle.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let old = SceneId::new();
        let peer = SceneId::new();
        for id in [old, peer] {
            assert_eq!(
                lifecycle.register_session(
                    id,
                    "lifecycle".into(),
                    ResourceBudget {
                        max_update_rate_hz: 4.0,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default(),
                ),
                MutationBudgetDecision::Allow
            );
        }
        assert_eq!(
            lifecycle.reserve_mutation(old, 1, 10, 1),
            MutationBudgetDecision::Allow
        );
        lifecycle.remove_session(old);
        assert!(!lifecycle.lock().sessions.contains_key(&old));
        assert_eq!(lifecycle.lock().sessions[&peer].recent_updates.len(), 1);
        let resumed = SceneId::new();
        assert_eq!(
            lifecycle.register_session(
                resumed,
                "lifecycle".into(),
                ResourceBudget {
                    max_update_rate_hz: 4.0,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage {
                    tiles: 1,
                    texture_bytes: 10
                },
            ),
            MutationBudgetDecision::Allow
        );
        assert_eq!(lifecycle.lock().sessions[&resumed].recent_updates.len(), 1);
        assert_eq!(
            lifecycle.reserve_mutation(peer, 0, 0, 1),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            lifecycle.reserve_mutation(resumed, 0, 0, 1),
            MutationBudgetDecision::Allow
        );
        rows.push((
            "resume preserves live peer rate history",
            matches!(
                lifecycle.reserve_mutation(peer, 0, 0, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));

        // Heterogeneous direct trait callers retain the requester-budget rule.
        let mut mixed = RuntimeMutationBudgetEnforcer::new();
        let read_clock = clock.clone();
        mixed.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let low = SceneId::new();
        let high = SceneId::new();
        for (id, limit) in [(low, 3.0), (high, 5.0)] {
            assert_eq!(
                mixed.register_session(
                    id,
                    "mixed".into(),
                    ResourceBudget {
                        max_update_rate_hz: limit,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default()
                ),
                MutationBudgetDecision::Allow
            );
        }
        assert_eq!(
            mixed.reserve_mutation(low, 0, 0, 1),
            MutationBudgetDecision::Allow
        );
        for _ in 0..2 {
            assert_eq!(
                mixed.reserve_mutation(high, 0, 0, 1),
                MutationBudgetDecision::Allow
            );
        }
        rows.push((
            "heterogeneous requester limit remains its own",
            matches!(
                mixed.reserve_mutation(low, 0, 0, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));
        let mut fractional = RuntimeMutationBudgetEnforcer::new();
        let read_clock = clock.clone();
        fractional.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let ids = [SceneId::new(), SceneId::new()];
        for id in ids {
            assert_eq!(
                fractional.register_session(
                    id,
                    "fractional".into(),
                    ResourceBudget {
                        max_update_rate_hz: 2.5,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default()
                ),
                MutationBudgetDecision::Allow
            );
        }
        rows.push((
            "fractional rate keeps existing count comparison",
            matches!(
                fractional.reserve_mutation(ids[0], 0, 0, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ),
        ));

        for order in ["forward", "reverse", "parallel"] {
            let mut ordered = RuntimeMutationBudgetEnforcer::new();
            let read_clock = clock.clone();
            ordered.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
            let ids = [SceneId::new(), SceneId::new()];
            for id in ids {
                assert_eq!(
                    ordered.register_session(
                        id,
                        "ordered".into(),
                        rate_budget.clone(),
                        true,
                        MutationBudgetUsage::default()
                    ),
                    MutationBudgetDecision::Allow
                );
            }
            let attempts = if order == "reverse" {
                [ids[1], ids[0]]
            } else {
                ids
            };
            let outcomes = if order == "parallel" {
                std::thread::scope(|scope| {
                    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
                    let workers: Vec<_> = attempts
                        .into_iter()
                        .map(|id| {
                            let start = start.clone();
                            let ordered = &ordered;
                            scope.spawn(move || {
                                start.wait();
                                ordered.reserve_mutation(id, 0, 0, 1)
                            })
                        })
                        .collect();
                    workers
                        .into_iter()
                        .map(|worker| worker.join().expect("rate reserve worker joined"))
                        .collect::<Vec<_>>()
                })
            } else {
                attempts
                    .into_iter()
                    .map(|id| ordered.reserve_mutation(id, 0, 0, 1))
                    .collect()
            };
            for decision in &outcomes {
                assert!(matches!(
                    decision,
                    MutationBudgetDecision::Allow
                        | MutationBudgetDecision::Reject {
                            error_code: "RESOURCE_BUDGET_EXCEEDED",
                            ..
                        }
                ));
            }
            rows.push((
                order,
                outcomes
                    .iter()
                    .filter(|decision| **decision == MutationBudgetDecision::Allow)
                    .count()
                    == 1,
            ));
        }
        println!(
            "same-agent rate boundary and compatibility rows: {rows:?}; fourth={fourth:?}; before={before:?}; after={after:?}"
        );
        for (label, satisfied) in rows {
            assert!(satisfied, "per-agent rate contract: {label}");
        }

        // A saturated peer must not change fresh registration admission.
        // Its successful registration sample still counts for later mutations.
        let mut saturated = RuntimeMutationBudgetEnforcer::new();
        let read_clock = clock.clone();
        saturated.clock = Some(std::sync::Arc::new(move || *read_clock.lock().unwrap()));
        let peer = SceneId::new();
        assert_eq!(
            saturated.register_session(
                peer,
                "saturated-agent".into(),
                rate_budget.clone(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        for _ in 0..2 {
            assert_eq!(
                saturated.reserve_mutation(peer, 1, 10, 1),
                MutationBudgetDecision::Allow
            );
        }
        let before_registration = {
            let state = saturated.lock();
            assert_eq!(state.sessions[&peer].recent_updates.len(), 3);
            assert!(
                state.sessions[&peer]
                    .recent_updates
                    .iter()
                    .all(|sample| *sample == t0 + UPDATE_RATE_WINDOW)
            );
            (
                state.sessions.len(),
                state.leased_tiles,
                state.leased_texture_bytes,
            )
        };
        let fresh = SceneId::new();
        let fresh_registration = saturated.register_session(
            fresh,
            "saturated-agent".into(),
            rate_budget,
            true,
            MutationBudgetUsage {
                tiles: 1,
                texture_bytes: 10,
            },
        );
        let after_registration = {
            let state = saturated.lock();
            assert_eq!(state.sessions[&peer].recent_updates.len(), 3);
            (
                state.sessions.len(),
                state.leased_tiles,
                state.leased_texture_bytes,
            )
        };
        println!(
            "saturated-peer fresh registration: decision={fresh_registration:?}; before={before_registration:?}; after={after_registration:?}"
        );
        assert_eq!(before_registration, (1, 2, 20));
        assert_eq!(
            fresh_registration,
            MutationBudgetDecision::Allow,
            "saturated peer history preserves fresh registration admission"
        );
        assert_eq!(after_registration, (2, 3, 30));
        assert_eq!(saturated.lock().sessions[&fresh].recent_updates.len(), 1);
        let first_after_registration = saturated.reserve_mutation(fresh, 1, 10, 1);
        let after_rejection = {
            let state = saturated.lock();
            assert_eq!(state.sessions[&peer].recent_updates.len(), 3);
            assert_eq!(state.sessions[&fresh].recent_updates.len(), 2);
            assert_eq!(
                (
                    state.sessions[&fresh].tiles,
                    state.sessions[&fresh].texture_bytes
                ),
                (1, 10)
            );
            (
                state.sessions.len(),
                state.leased_tiles,
                state.leased_texture_bytes,
            )
        };
        println!(
            "saturated-peer mutation after registration: decision={first_after_registration:?}; before={after_registration:?}; after={after_rejection:?}"
        );
        assert!(matches!(
            first_after_registration,
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                ..
            }
        ));
        assert_eq!(after_rejection, after_registration);
    }

    #[test]
    fn aggregate_limits_are_atomic_across_agents() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_limits(2, 2, 100);
        let agent_a = SceneId::new();
        let agent_b = SceneId::new();
        for (session_id, name) in [(agent_a, "agent-a"), (agent_b, "agent-b")] {
            assert_eq!(
                enforcer.register_session(
                    session_id,
                    name.to_string(),
                    ResourceBudget {
                        max_tiles: 8,
                        max_texture_bytes: 100,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default(),
                ),
                MutationBudgetDecision::Allow
            );
        }
        assert!(matches!(
            enforcer.register_session(
                SceneId::new(),
                "agent-c".to_string(),
                ResourceBudget::default(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Reject { message, .. } if message.contains("resident_sessions")
        ));

        assert_eq!(
            enforcer.reserve_mutation(agent_a, 1, 60, 1),
            MutationBudgetDecision::Allow
        );
        assert!(matches!(
            enforcer.reserve_mutation(agent_b, 2, 0, 1),
            MutationBudgetDecision::Reject { message, .. } if message.contains("leased_tiles")
        ));
        assert!(matches!(
            enforcer.reserve_mutation(agent_b, 1, 50, 1),
            MutationBudgetDecision::Reject { message, .. } if message.contains("agent_leased_texture_bytes")
        ));
        assert_eq!(
            enforcer.reserve_mutation(agent_b, 1, 40, 1),
            MutationBudgetDecision::Allow
        );
    }

    #[test]
    fn removing_session_releases_its_aggregate_tile_and_texture_usage() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_limits(2, 2, 100);
        let agent_a = SceneId::new();
        assert_eq!(
            enforcer.register_session(
                agent_a,
                "agent-a".to_string(),
                ResourceBudget {
                    max_tiles: 2,
                    max_texture_bytes: 100,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            enforcer.reserve_mutation(agent_a, 2, 100, 1),
            MutationBudgetDecision::Allow
        );

        enforcer.remove_session(agent_a);

        let agent_b = SceneId::new();
        assert_eq!(
            enforcer.register_session(
                agent_b,
                "agent-b".to_string(),
                ResourceBudget {
                    max_tiles: 2,
                    max_texture_bytes: 100,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            enforcer.reserve_mutation(agent_b, 2, 100, 1),
            MutationBudgetDecision::Allow,
            "disconnect cleanup must not leak aggregate usage"
        );
    }

    #[test]
    fn resumed_session_restores_usage_before_accepting_new_mutations() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_limits(1, 2, 100);
        let resumed = SceneId::new();
        assert_eq!(
            enforcer.register_session(
                resumed,
                "agent-a".to_string(),
                ResourceBudget {
                    max_tiles: 2,
                    max_texture_bytes: 100,
                    ..ResourceBudget::default()
                },
                true,
                MutationBudgetUsage {
                    tiles: 1,
                    texture_bytes: 60,
                },
            ),
            MutationBudgetDecision::Allow
        );
        assert!(matches!(
            enforcer.reserve_mutation(resumed, 1, 50, 1),
            MutationBudgetDecision::Reject { message, .. }
                if message.contains("agent_leased_texture_bytes")
        ));
        assert_eq!(
            enforcer.reserve_mutation(resumed, 1, 40, 1),
            MutationBudgetDecision::Allow
        );
    }

    #[test]
    fn removing_one_of_two_same_namespace_sessions_preserves_the_other() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_limits(2, 2, 100);
        let first = SceneId::new();
        let second = SceneId::new();
        for session_id in [first, second] {
            assert_eq!(
                enforcer.register_session(
                    session_id,
                    "same-agent".to_string(),
                    ResourceBudget {
                        max_tiles: 2,
                        max_texture_bytes: 100,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default(),
                ),
                MutationBudgetDecision::Allow
            );
        }
        assert_eq!(
            enforcer.reserve_mutation(second, 1, 20, 1),
            MutationBudgetDecision::Allow
        );

        enforcer.remove_session(first);

        assert_eq!(
            enforcer.reserve_mutation(second, 1, 20, 1),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            enforcer.register_session(
                SceneId::new(),
                "third".to_string(),
                ResourceBudget::default(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert!(matches!(
            enforcer.register_session(
                SceneId::new(),
                "fourth".to_string(),
                ResourceBudget::default(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Reject { message, .. }
                if message.contains("resident_sessions")
        ));

        // Local caps are lower than the global ceiling, so neither can mask
        // the other. Rollback must release only the successful own delta.
        for (label, tiles, bytes) in [("tiles", 1, 0), ("texture", 0, 12)] {
            let local = RuntimeMutationBudgetEnforcer::with_limits(2, 8, 100);
            let session = SceneId::new();
            assert_eq!(
                local.register_session(
                    session,
                    "local".to_string(),
                    ResourceBudget {
                        max_tiles: 1,
                        max_texture_bytes: 20,
                        ..ResourceBudget::default()
                    },
                    true,
                    MutationBudgetUsage::default()
                ),
                MutationBudgetDecision::Allow
            );
            assert_eq!(
                local.reserve_mutation(session, tiles, bytes, 1),
                MutationBudgetDecision::Allow
            );
            let rejected = local.reserve_mutation(session, tiles, bytes, 1);
            println!("local {label} session={session} rejected={rejected:?}");
            assert!(matches!(
                rejected,
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_BUDGET_EXCEEDED",
                    ..
                }
            ));
            local.rollback_mutation(session, tiles, bytes);
            assert_eq!(
                local.reserve_mutation(session, tiles, bytes, 1),
                MutationBudgetDecision::Allow
            );
            local.remove_session(session);
        }

        // Joined execution threads contend on the existing global mutex.
        // A rejected delta reserves nothing; actual rollback/removal frees
        // capacity for the other session without releasing its peer usage.
        for (label, tiles, bytes) in [("tiles", 1, 0), ("texture", 0, 12)] {
            let global = RuntimeMutationBudgetEnforcer::with_limits(2, 1, 20);
            let sessions = [SceneId::new(), SceneId::new()];
            for (session, namespace) in sessions.into_iter().zip(["agent-a", "agent-b"]) {
                assert_eq!(
                    global.register_session(
                        session,
                        namespace.to_string(),
                        ResourceBudget {
                            max_tiles: 2,
                            max_texture_bytes: 100,
                            ..ResourceBudget::default()
                        },
                        true,
                        MutationBudgetUsage::default()
                    ),
                    MutationBudgetDecision::Allow
                );
            }
            let outcomes = std::thread::scope(|scope| {
                let start = std::sync::Arc::new(std::sync::Barrier::new(2));
                let workers: Vec<_> = sessions
                    .into_iter()
                    .map(|session| {
                        let start = start.clone();
                        let global = &global;
                        scope.spawn(move || {
                            start.wait();
                            (session, global.reserve_mutation(session, tiles, bytes, 1))
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().expect("global reserve thread completed"))
                    .collect::<Vec<_>>()
            });
            println!("global {label} joined={outcomes:?}");
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|(_, decision)| *decision == MutationBudgetDecision::Allow)
                    .count(),
                1
            );
            let winner = outcomes
                .iter()
                .find(|(_, decision)| *decision == MutationBudgetDecision::Allow)
                .unwrap()
                .0;
            let loser = outcomes
                .iter()
                .find(|(_, decision)| {
                    matches!(
                        decision,
                        MutationBudgetDecision::Reject {
                            error_code: "RESOURCE_EXHAUSTED",
                            ..
                        }
                    )
                })
                .unwrap()
                .0;
            global.rollback_mutation(winner, tiles, bytes);
            assert_eq!(
                global.reserve_mutation(loser, tiles, bytes, 1),
                MutationBudgetDecision::Allow
            );
            assert!(matches!(
                global.reserve_mutation(winner, tiles, bytes, 1),
                MutationBudgetDecision::Reject {
                    error_code: "RESOURCE_EXHAUSTED",
                    ..
                }
            ));
            global.remove_session(loser);
            assert_eq!(
                global.reserve_mutation(winner, tiles, bytes, 1),
                MutationBudgetDecision::Allow
            );
        }

        // I7 and ResourceBudget/trait say per-agent. API session wording does
        // not explicitly permit aggregate excess across the same identity.
        // These are injected-namespace unit rows, not an authenticated wire
        // witness. Record all joined outcomes before the normative assertion;
        // preserve a failure instead of changing policy to match the code.
        let mut aggregate_rows = Vec::new();
        for (label, tiles, bytes) in [("tiles", 1, 0), ("texture", 0, 12)] {
            for namespaces in [["same-agent", "same-agent"], ["agent-a", "agent-b"]] {
                for order in ["forward", "reverse", "parallel"] {
                    let aggregate = RuntimeMutationBudgetEnforcer::with_limits(2, 8, 100);
                    let sessions = [SceneId::new(), SceneId::new()];
                    for (session, namespace) in sessions.into_iter().zip(namespaces) {
                        assert_eq!(
                            aggregate.register_session(
                                session,
                                namespace.to_string(),
                                ResourceBudget {
                                    max_tiles: 1,
                                    max_texture_bytes: 20,
                                    ..ResourceBudget::default()
                                },
                                true,
                                MutationBudgetUsage::default()
                            ),
                            MutationBudgetDecision::Allow
                        );
                    }
                    let attempts = if order == "reverse" {
                        [sessions[1], sessions[0]]
                    } else {
                        sessions
                    };
                    let outcomes = if order == "parallel" {
                        std::thread::scope(|scope| {
                            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
                            let workers: Vec<_> = attempts
                                .into_iter()
                                .map(|session| {
                                    let start = start.clone();
                                    let aggregate = &aggregate;
                                    scope.spawn(move || {
                                        start.wait();
                                        (
                                            session,
                                            aggregate.reserve_mutation(session, tiles, bytes, 1),
                                        )
                                    })
                                })
                                .collect();
                            workers
                                .into_iter()
                                .map(|worker| {
                                    worker.join().expect("same-agent reserve thread completed")
                                })
                                .collect::<Vec<_>>()
                        })
                    } else {
                        attempts
                            .into_iter()
                            .map(|session| {
                                (
                                    session,
                                    aggregate.reserve_mutation(session, tiles, bytes, 1),
                                )
                            })
                            .collect()
                    };
                    println!(
                        "aggregate {label} namespaces={namespaces:?} order={order} joined={outcomes:?}"
                    );
                    aggregate_rows.push((label, namespaces, order, outcomes));
                }
            }
        }
        for (label, namespaces, order, outcomes) in aggregate_rows {
            let expected_admitted = if namespaces[0] == namespaces[1] { 1 } else { 2 };
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|(_, decision)| *decision == MutationBudgetDecision::Allow)
                    .count(),
                expected_admitted,
                "I7 per-agent {label} cap; namespaces={namespaces:?} order={order}: {outcomes:?}"
            );
            for (_, decision) in outcomes {
                assert!(matches!(
                    decision,
                    MutationBudgetDecision::Allow
                        | MutationBudgetDecision::Reject {
                            error_code: "RESOURCE_BUDGET_EXCEEDED",
                            ..
                        }
                ));
            }
        }
    }

    #[test]
    fn guest_sessions_use_a_separate_enforced_pool() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_session_limits(1, 1, 2, 100);
        let resident = SceneId::new();
        let guest = SceneId::new();
        assert_eq!(
            enforcer.register_session(
                resident,
                "resident".to_string(),
                ResourceBudget::default(),
                true,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert_eq!(
            enforcer.register_session(
                guest,
                "guest".to_string(),
                ResourceBudget::default(),
                false,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
        assert!(matches!(
            enforcer.register_session(
                SceneId::new(),
                "guest-2".to_string(),
                ResourceBudget::default(),
                false,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Reject { message, .. }
                if message.contains("guest_sessions")
        ));

        enforcer.remove_session(guest);
        assert_eq!(
            enforcer.register_session(
                SceneId::new(),
                "guest-2".to_string(),
                ResourceBudget::default(),
                false,
                MutationBudgetUsage::default(),
            ),
            MutationBudgetDecision::Allow
        );
    }
}
