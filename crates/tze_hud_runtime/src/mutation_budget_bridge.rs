//! Runtime-owned admission limits for resident and guest gRPC sessions.

use std::collections::HashSet;
use std::sync::Mutex;

use tze_hud_protocol::session_server::{MutationBudgetDecision, MutationBudgetEnforcer};
use tze_hud_scene::types::{ResourceBudget, SceneId};

/// Default cap on concurrent guest (non-resident) sessions.
pub(crate) const DEFAULT_MAX_GUEST_SESSIONS: u32 = 64;

pub(crate) struct RuntimeMutationBudgetEnforcer {
    sessions: Mutex<SessionPools>,
    max_resident_sessions: u32,
    max_guest_sessions: u32,
}

#[derive(Default)]
struct SessionPools {
    residents: HashSet<SceneId>,
    guests: HashSet<SceneId>,
}

impl RuntimeMutationBudgetEnforcer {
    pub(crate) fn with_limits(max_resident_sessions: u32) -> Self {
        Self::with_session_limits(max_resident_sessions, DEFAULT_MAX_GUEST_SESSIONS)
    }

    fn with_session_limits(max_resident_sessions: u32, max_guest_sessions: u32) -> Self {
        Self {
            sessions: Mutex::new(SessionPools::default()),
            max_resident_sessions,
            max_guest_sessions,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SessionPools> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn exhausted(message: String) -> MutationBudgetDecision {
    MutationBudgetDecision::Reject {
        error_code: "RESOURCE_EXHAUSTED",
        message,
    }
}

impl MutationBudgetEnforcer for RuntimeMutationBudgetEnforcer {
    fn register_session(
        &self,
        session_id: SceneId,
        _namespace: &str,
        budget: ResourceBudget,
        resident: bool,
    ) -> MutationBudgetDecision {
        let mut sessions = self.lock();
        if sessions.residents.contains(&session_id) || sessions.guests.contains(&session_id) {
            return exhausted(format!("session_id {session_id} is already registered"));
        }
        let (pool, limit, label) = if resident {
            (
                &mut sessions.residents,
                self.max_resident_sessions,
                "resident_sessions",
            )
        } else {
            (
                &mut sessions.guests,
                self.max_guest_sessions,
                "guest_sessions",
            )
        };
        if pool.len() >= limit as usize {
            return exhausted(format!("{label} current={} limit={limit}", pool.len()));
        }

        // Registration counted as one update in the former mutation budget.
        // Keep its rejection for programmatic profiles that allow fewer than one.
        if 1.0 > budget.max_update_rate_hz {
            return MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                message: format!(
                    "restored session usage rejected: update_rate_hz current=1 limit={}",
                    budget.max_update_rate_hz,
                ),
            };
        }

        pool.insert(session_id);
        MutationBudgetDecision::Allow
    }

    fn remove_session(&self, session_id: SceneId) {
        let mut sessions = self.lock();
        if !sessions.residents.remove(&session_id) {
            sessions.guests.remove(&session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_sessions_use_a_separate_enforced_pool() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_session_limits(1, 1);
        let register = |id, resident| {
            enforcer.register_session(id, "agent", ResourceBudget::default(), resident)
        };
        let resident = SceneId::new();
        let guest = SceneId::new();

        assert_eq!(register(resident, true), MutationBudgetDecision::Allow);
        assert!(matches!(
            register(resident, false),
            MutationBudgetDecision::Reject { error_code: "RESOURCE_EXHAUSTED", message }
                if message.contains("already registered")
        ));
        assert_eq!(register(guest, false), MutationBudgetDecision::Allow);
        assert!(matches!(
            register(SceneId::new(), true),
            MutationBudgetDecision::Reject { error_code: "RESOURCE_EXHAUSTED", message }
                if message.contains("resident_sessions")
        ));
        assert!(matches!(
            register(SceneId::new(), false),
            MutationBudgetDecision::Reject { error_code: "RESOURCE_EXHAUSTED", message }
                if message.contains("guest_sessions")
        ));

        enforcer.remove_session(guest);
        enforcer.remove_session(guest);
        assert_eq!(
            register(SceneId::new(), false),
            MutationBudgetDecision::Allow
        );
        assert!(matches!(
            register(SceneId::new(), true),
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_EXHAUSTED",
                ..
            }
        ));
        enforcer.remove_session(resident);
        assert_eq!(
            register(SceneId::new(), true),
            MutationBudgetDecision::Allow
        );
    }

    #[test]
    fn registration_rejects_an_update_budget_below_one_without_consuming_capacity() {
        let enforcer = RuntimeMutationBudgetEnforcer::with_session_limits(1, 0);
        let id = SceneId::new();
        let budget = ResourceBudget {
            max_update_rate_hz: 0.0,
            ..ResourceBudget::default()
        };
        assert!(matches!(
            enforcer.register_session(id, "agent", budget, true),
            MutationBudgetDecision::Reject {
                error_code: "RESOURCE_BUDGET_EXCEEDED",
                ..
            }
        ));
        assert_eq!(
            enforcer.register_session(id, "agent", ResourceBudget::default(), true),
            MutationBudgetDecision::Allow
        );
    }
}
