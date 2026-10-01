//! Dependency-safe bridge for runtime mutation-intake budget enforcement.

use std::sync::Arc;

use tze_hud_scene::types::{ResourceBudget, SceneId};

/// Stable outcome returned by the runtime-owned mutation budget enforcer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationBudgetDecision {
    Allow,
    Reject {
        error_code: &'static str,
        message: String,
    },
}

/// Existing logical scene usage restored when a session resumes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MutationBudgetUsage {
    pub tiles: u32,
    pub texture_bytes: u64,
}

/// Protocol-facing contract implemented by the runtime's budget enforcer.
pub trait MutationBudgetEnforcer: Send + Sync {
    fn register_session(
        &self,
        session_id: SceneId,
        namespace: String,
        budget: ResourceBudget,
        resident: bool,
        initial_usage: MutationBudgetUsage,
    ) -> MutationBudgetDecision;
    fn remove_session(&self, session_id: SceneId);
    /// Atomically admit and reserve an aggregate/per-agent mutation delta.
    fn reserve_mutation(
        &self,
        session_id: SceneId,
        delta_tiles: i32,
        delta_texture_bytes: i64,
        max_nodes_in_batch: u32,
    ) -> MutationBudgetDecision;
    /// Roll back a reservation when scene commit rejects the batch.
    fn rollback_mutation(&self, session_id: SceneId, delta_tiles: i32, delta_texture_bytes: i64);
}

pub type SharedMutationBudgetEnforcer = Arc<dyn MutationBudgetEnforcer>;
