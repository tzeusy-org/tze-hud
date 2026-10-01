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
    resident: bool,
    budget: ResourceBudget,
    tiles: u32,
    texture_bytes: u64,
    recent_updates: VecDeque<Instant>,
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
        let rate_hz = self.recent_updates.len() as f32;
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
        _namespace: String,
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
        let mut session = SessionBudgetState {
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
            Instant::now(),
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
        let Some(session) = state.sessions.get_mut(&session_id) else {
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
        if let Some(message) = session.check(
            session_tiles,
            session_texture,
            max_nodes_in_batch,
            Instant::now(),
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
