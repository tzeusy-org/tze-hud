//! Per-session state types for the streaming session server.
//!
//! This module contains `StreamSession` (all per-session fields) and
//! `CapabilityRevocationEvent` (runtime-broadcast revocation command).
//! Moved from `session_server/mod.rs` as Step SS-4 of the module split
//! (docs/design/session-server-renderer-module-split-plan.md §3.4).

use super::freeze_queue::SessionFreezeQueue;
use super::lifecycle::SessionState;
use super::upload::UploadByteRateLimiter;
use crate::dedup::DedupWindow;
use crate::lease::LeaseCorrelationCache;
use tze_hud_scene::types::{ResourceBudget, SceneId};

use super::budget_gate::SharedMutationBudgetEnforcer;

// ─── Session state ──────────────────────────────────────────────────────────

/// Per-session state tracked by the streaming server.
pub(super) struct StreamSession {
    pub(super) session_id: String,
    pub(super) namespace: String,
    pub(super) agent_name: String,
    /// Capabilities explicitly granted at handshake (from `requested_capabilities`).
    pub(super) capabilities: Vec<String>,
    /// Authorization scope for subscription and capability-request checks.
    /// For unrestricted PSK sessions this is `vec!["*"]`; for restricted agents
    /// it mirrors `capabilities`. Used for gating subscriptions and mid-session
    /// CapabilityRequest evaluation.
    pub(super) policy_capabilities: Vec<String>,
    pub(super) lease_ids: Vec<tze_hud_scene::SceneId>,
    pub(super) scene_session_id: SceneId,
    pub(super) resource_budget: ResourceBudget,
    pub(super) budget_enforcer: Option<SharedMutationBudgetEnforcer>,
    pub(super) subscriptions: Vec<String>,
    /// Fine-grained event type prefix filters per subscription category (RFC 0010 §7.2).
    ///
    /// When an agent subscribes with a `filter_prefix` (via `SubscriptionChange.subscribe_filter`),
    /// the filter is stored here keyed by category name. Categories not present in this map
    /// use the category's default prefix. Filters are removed when the category is unsubscribed.
    pub(super) subscription_filters: std::collections::HashMap<String, String>,
    pub(super) server_sequence: u64,
    pub(super) resume_token: Vec<u8>,
    pub(super) last_heartbeat_ms: u64,

    /// Current lifecycle state (RFC 0005 §1.1).
    pub(super) state: SessionState,

    /// Last validated client-side sequence number (RFC 0005 §2.3).
    /// Initialized to 1 during session init/resume (treating the handshake message as
    /// sequence 1). Each subsequent validated message must carry a strictly greater
    /// sequence number within `max_sequence_gap` of the previous.
    pub(super) last_client_sequence: u64,

    /// Whether safe mode is active for this session (RFC 0005 §3.7).
    /// When true, MutationBatch messages are rejected with SAFE_MODE_ACTIVE.
    pub(super) safe_mode_active: bool,

    /// Per-session mutation queue for freeze semantics (system-shell/spec.md §Freeze Scene).
    ///
    /// When `SharedState.freeze_active` is true, incoming MutationBatch messages
    /// are enqueued here rather than applied to the scene. On unfreeze, all queued
    /// mutations are applied in submission order.
    ///
    /// The shell owns freeze state transitions; the session server owns the queue.
    pub(super) freeze_queue: SessionFreezeQueue,

    /// Wall-clock time (UTC µs since epoch) at which this session was opened.
    /// Used for TIMESTAMP_TOO_OLD validation of TimingHints (RFC 0003 §3.5).
    pub(super) session_open_at_wall_us: u64,

    /// Per-session MutationBatch deduplication window (RFC 0005 §5.2).
    pub(super) dedup_window: DedupWindow,

    /// Per-session lease-operation correlation cache (RFC 0005 §5.3).
    ///
    /// Maps client sequence number → cached `LeaseResponse` payload.  On
    /// retransmit the server replays the cached response without re-applying
    /// the lease operation, preserving at-least-once semantics with
    /// idempotent handling.
    pub(super) lease_correlation_cache: LeaseCorrelationCache,

    /// Per-session upload-byte limiter for resident resource transport.
    pub(super) resource_upload_rate_limiter: UploadByteRateLimiter,
}

impl StreamSession {
    pub(super) fn next_server_seq(&mut self) -> u64 {
        self.server_sequence += 1;
        self.server_sequence
    }

    /// Transition to a new state. Returns the previous state.
    pub(super) fn transition(&mut self, new_state: SessionState) -> SessionState {
        let prev = self.state.clone();
        self.state = new_state;
        prev
    }

    /// Validate an inbound client sequence number per RFC 0005 §2.3.
    ///
    /// Returns `Ok(())` if valid, or an error string with the appropriate
    /// SessionError code if the sequence is regressed or the gap is too large.
    pub(super) fn validate_client_sequence(
        &mut self,
        seq: u64,
        max_gap: u64,
    ) -> Result<(), (&'static str, String)> {
        if seq <= self.last_client_sequence {
            return Err((
                "SEQUENCE_REGRESSION",
                format!(
                    "sequence regression: received {seq}, last was {}",
                    self.last_client_sequence
                ),
            ));
        }
        // "gap" per spec (RFC 0005 §2.3) = seq − last_seq.
        // Reject if gap > max_sequence_gap (default 100).
        // Example: last=5, seq=105 → gap=100 = max_gap → accepted (not strictly greater).
        //          last=5, seq=106 → gap=101 > max_gap=100 → rejected.
        //          last=5, seq=150 (spec example) → gap=145 > 100 → rejected.
        let gap = seq - self.last_client_sequence;
        if gap > max_gap {
            return Err((
                "SEQUENCE_GAP_EXCEEDED",
                format!(
                    "sequence gap {gap} exceeds max {max_gap}: received {seq}, last was {}",
                    self.last_client_sequence
                ),
            ));
        }
        self.last_client_sequence = seq;
        Ok(())
    }
}

// ─── Capability Revocation Event ─────────────────────────────────────────────

/// A runtime-initiated capability revocation command broadcast to all session handlers.
///
/// When the runtime calls [`super::HudSessionImpl::revoke_capability_on_lease`], it broadcasts
/// this event. Each session handler checks whether any of its leases match `lease_id`
/// and, if so, applies the revocation to the scene graph and notifies the agent via
/// `CapabilityNotice(revoked=[capability_name])`.
///
/// RFC 0001 §3.3: capability checks are enforced at mutation time against the live scope,
/// not merely at grant time.
#[derive(Clone, Debug)]
pub struct CapabilityRevocationEvent {
    /// The lease to narrow, or null for an explicit runtime-global session-capability revocation.
    pub lease_id: tze_hud_scene::SceneId,
    /// Canonical name of the capability to remove (e.g. `"create_tiles"`, `"publish_zone:subtitle"`).
    pub capability_name: String,
}
