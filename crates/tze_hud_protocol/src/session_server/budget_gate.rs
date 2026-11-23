//! Dependency-safe bridge for runtime-owned session admission.

use std::sync::Arc;

use tze_hud_scene::types::{ResourceBudget, SceneId};

/// Stable outcome returned by runtime admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationBudgetDecision {
    Allow,
    Reject {
        error_code: &'static str,
        message: String,
    },
}

/// Protocol-facing contract implemented by the runtime's admission gate.
pub trait MutationBudgetEnforcer: Send + Sync {
    fn register_session(
        &self,
        session_id: SceneId,
        namespace: &str,
        budget: ResourceBudget,
        resident: bool,
    ) -> MutationBudgetDecision;
    fn remove_session(&self, session_id: SceneId);
}

pub type SharedMutationBudgetEnforcer = Arc<dyn MutationBudgetEnforcer>;
