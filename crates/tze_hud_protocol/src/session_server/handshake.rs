//! Session handshake handlers — SS-6 submodule.
//!
//! Contains `authorization_scope_for_agent`, `handle_session_init`, and
//! `handle_session_resume`, extracted mechanically from `mod.rs`.
//! The dispatcher (`dispatch_message`) and session loop remain in `mod.rs`
//! and call these functions unchanged.

use crate::auth::{
    AuthResult, CapabilityPolicy, authenticate_session_init, negotiate_version,
    validate_canonical_capabilities,
};
use crate::dedup::DedupWindow;
use crate::lease::{DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY, LeaseCorrelationCache};
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;
use crate::subscriptions;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::events::emission::AgentEventRateLimiter;
use tze_hud_scene::types::ResourceBudget;

use super::freeze_queue::{FREEZE_QUEUE_CAPACITY, SessionFreezeQueue};
use super::lifecycle::SessionState;
use super::stream_session::StreamSession;
use super::upload::UploadByteRateLimiter;
use super::{DEFAULT_HEARTBEAT_INTERVAL_MS, now_ms, now_wall_us};

/// Resolve the per-agent authorization scope used for `CapabilityRequest`
/// evaluation.
///
/// Source of truth in v1:
/// - Registered agent entries (`agent_capabilities`) provide the full
///   allow-list.
/// - Unregistered agents receive unrestricted scope only when
///   `fallback_unrestricted=true` (dev/test mode).
/// - Otherwise unregistered agents are guest scope (empty allow-list).
pub(super) fn authorization_scope_for_agent(
    agent_id: &str,
    agent_capabilities: &HashMap<String, Vec<String>>,
    fallback_unrestricted: bool,
) -> Vec<String> {
    match agent_capabilities.get(agent_id) {
        Some(caps) => caps.clone(),
        None if fallback_unrestricted => vec!["*".to_string()],
        None => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_init(
    state: &Arc<Mutex<SharedState>>,
    psk: &str,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    init: &SessionInit,
    agent_capabilities: &HashMap<String, Vec<String>>,
    agent_resource_budgets: &HashMap<String, ResourceBudget>,
    fallback_resource_budget: &ResourceBudget,
    budget_enforcer: Option<&super::SharedMutationBudgetEnforcer>,
    fallback_unrestricted: bool,
    peer_ip: Option<std::net::IpAddr>,
) -> Option<StreamSession> {
    // ── Step 1: Version negotiation (RFC 0005 §4.1) ──────────────────────────
    // Do this before authentication so agents can learn about version
    // incompatibility even if they send a wrong key.
    let negotiated_version =
        match negotiate_version(init.min_protocol_version, init.max_protocol_version) {
            Ok(v) => v,
            Err(msg) => {
                let _ = tx
                    .send(Ok(ServerMessage {
                        sequence: 1,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(ServerPayload::SessionError(SessionError {
                            code: "UNSUPPORTED_PROTOCOL_VERSION".to_string(),
                            message: msg,
                            hint: format!(
                                "{{\"runtime_min\": {}, \"runtime_max\": {}}}",
                                crate::auth::RUNTIME_MIN_VERSION,
                                crate::auth::RUNTIME_MAX_VERSION
                            ),
                        })),
                    }))
                    .await;
                return None;
            }
        };

    // ── Step 2: Authentication (RFC 0005 §1.4) ───────────────────────────────
    // Authentication is evaluated synchronously before SessionEstablished is sent.
    // peer_ip is passed for LocalSocketCredential loopback gating (hud-1aswu.1).
    let auth_result = authenticate_session_init(
        init.auth_credential.as_ref(),
        &init.pre_shared_key,
        psk,
        peer_ip,
    );

    match auth_result {
        AuthResult::Accepted => {}
        AuthResult::Failed(reason) => {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: "AUTH_FAILED".to_string(),
                        message: reason,
                        hint: String::new(),
                    })),
                }))
                .await;
            return None;
        }
        AuthResult::Unimplemented(reason) => {
            // v1-reserved credential type — reject with AUTH_FAILED.
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: "AUTH_FAILED".to_string(),
                        message: reason,
                        hint: r#"{"supported_v1": ["PreSharedKeyCredential", "LocalSocketCredential"]}"#.to_string(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // ── Step 3: Capability vocabulary validation (configuration/spec.md §Capability Vocabulary) ──
    // All requested capability names MUST be from the canonical v1 vocabulary.
    // Legacy names (create_tile, receive_input, read_scene, zone_publish) and any
    // other non-canonical name MUST be rejected with CONFIG_UNKNOWN_CAPABILITY and a hint.
    if let Err(unknown_caps) = validate_canonical_capabilities(&init.requested_capabilities) {
        // Collect all errors before reporting (spec requires collecting all, not fail-fast).
        let hints: Vec<serde_json::Value> = unknown_caps
            .iter()
            .map(|e| serde_json::json!({"unknown": e.unknown, "hint": e.hint}))
            .collect();
        let hint_json = serde_json::to_string(&hints)
            .unwrap_or_else(|_| "see configuration/spec.md §Capability Vocabulary".to_string());
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: 1,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::SessionError(SessionError {
                    code: "CONFIG_UNKNOWN_CAPABILITY".to_string(),
                    message: format!(
                        "{} unrecognized capability name(s); canonical v1 names are required",
                        unknown_caps.len()
                    ),
                    hint: hint_json,
                })),
            }))
            .await;
        return None;
    }

    // ── Step 4: Capability negotiation (RFC 0005 §5.3) ───────────────────────
    // Capabilities are gated against the agent's authorization policy.
    //
    // Per configuration/spec.md §Requirement: Agent Registration (lines 136-147),
    // the configured authorization scope is the source of truth for both
    // handshake grants and future mid-session escalation checks.
    let authorization_scope =
        authorization_scope_for_agent(&init.agent_id, agent_capabilities, fallback_unrestricted);
    let policy = CapabilityPolicy::new(authorization_scope.clone());
    let (granted_capabilities, _denied_caps) =
        policy.partition_capabilities(&init.requested_capabilities);

    // ── Step 5: Subscription filtering (RFC 0005 §7.1) ──────────────────────
    // Initial subscriptions are filtered against the agent's explicitly granted
    // capabilities. Agents must include the required capability in their
    // `requested_capabilities` to subscribe to capability-gated categories
    // (e.g. `access_input_events` for INPUT_EVENTS, `read_scene_topology` for
    // SCENE_TOPOLOGY). Mandatory categories are always active.
    let policy_caps = if policy.is_unrestricted() {
        vec!["*".to_string()]
    } else {
        authorization_scope
    };
    let sub_result =
        subscriptions::filter_subscriptions(&init.initial_subscriptions, &granted_capabilities);

    let session_uuid = uuid::Uuid::now_v7();
    let session_id = session_uuid.to_string();
    let namespace = init.agent_id.clone();
    let resume_token = uuid::Uuid::now_v7().as_bytes().to_vec();
    let scene_session_id = tze_hud_scene::SceneId::from_uuid(session_uuid);
    let resource_budget = agent_resource_budgets
        .get(&init.agent_id)
        .cloned()
        .unwrap_or_else(|| fallback_resource_budget.clone());
    if let Some(enforcer) = budget_enforcer {
        if let super::MutationBudgetDecision::Reject {
            error_code,
            message,
        }
        | super::MutationBudgetDecision::Revoke {
            error_code,
            message,
        } = enforcer.register_session(
            scene_session_id,
            namespace.clone(),
            resource_budget.clone(),
            agent_resource_budgets.contains_key(&init.agent_id),
            super::MutationBudgetUsage::default(),
        ) {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: error_code.to_string(),
                        message,
                        hint: String::new(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // Register session in the session registry and capture upload rate config +
    // the runtime's resolved portal tokens for the handshake (hud-16um0).
    let (upload_rate_limit_bytes_per_sec, resolved_portal_tokens) = {
        let mut st = state.lock().await;
        let _ = st
            .sessions
            .authenticate(&init.agent_id, psk, &granted_capabilities);
        (
            st.resource_store.upload_rate_limit_bytes_per_sec(),
            st.resolved_portal_tokens.clone(),
        )
    };
    // Only carry the field when the runtime actually exposes tokens; an empty
    // map means "not exposed" (headless/tests) and clients fall back to their
    // local default mirror, matching the pre-field wire behaviour.
    let portal_part_tokens = if resolved_portal_tokens.is_empty() {
        None
    } else {
        Some(ResolvedPortalTokens {
            tokens: resolved_portal_tokens,
        })
    };

    let session_open_at = now_wall_us();
    let mut session = StreamSession {
        session_id: session_id.clone(),
        namespace: namespace.clone(),
        agent_name: init.agent_id.clone(),
        capabilities: granted_capabilities.clone(),
        policy_capabilities: policy_caps.clone(),
        lease_ids: Vec::new(),
        scene_session_id,
        resource_budget,
        budget_enforcer: budget_enforcer.cloned(),
        subscriptions: sub_result.active.clone(),
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: resume_token.clone(),
        last_heartbeat_ms: now_ms(),
        state: SessionState::Handshaking,
        last_client_sequence: 1, // SessionInit is sequence 1; start validation from next
        safe_mode_active: false,
        expect_resume: false,
        agent_event_rate_limiter: AgentEventRateLimiter::new(),
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: session_open_at,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            upload_rate_limit_bytes_per_sec,
        ),
    };

    // ── Step 5: Clock skew estimation (RFC 0003 §1.3) ────────────────────────
    let compositor_ts = now_wall_us();
    let estimated_skew = if init.agent_timestamp_wall_us > 0 {
        init.agent_timestamp_wall_us as i64 - compositor_ts as i64
    } else {
        0
    };

    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: compositor_ts,
            payload: Some(ServerPayload::SessionEstablished(SessionEstablished {
                // Reuse the already-created UUID bytes directly; no need to
                // re-parse the string we just formatted.
                session_id: session_uuid.as_bytes().to_vec(),
                namespace,
                granted_capabilities,
                resume_token,
                heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
                server_sequence: seq,
                compositor_timestamp_wall_us: compositor_ts,
                estimated_skew_us: estimated_skew,
                active_subscriptions: sub_result.active,
                denied_subscriptions: sub_result.denied,
                negotiated_protocol_version: negotiated_version,
                portal_part_tokens,
            })),
        }))
        .await;

    Some(session)
}

/// Handle a `SessionResume` message — the first message on a reconnecting stream
/// within the grace period (RFC 0005 §6.2–6.4).
///
/// # Protocol contract
///
/// 1. Re-authenticate via `pre_shared_key` (RFC 0005 §6.2).
/// 2. Look up and consume the resume token from the [`TokenStore`].
///    - If missing or expired → `SessionError(SESSION_GRACE_EXPIRED)`.
///    - If valid → restore session state and issue new token.
/// 3. Send [`SessionResumeResult`] with `accepted=true` and the confirmed
///    subscription/capability state.
/// 4. The caller (main session loop) sends a [`SceneSnapshot`] immediately
///    after this function returns (same mechanism as new connections).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_resume(
    state: &Arc<Mutex<SharedState>>,
    psk: &str,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    resume: &SessionResume,
    agent_capabilities: &HashMap<String, Vec<String>>,
    agent_resource_budgets: &HashMap<String, ResourceBudget>,
    fallback_resource_budget: &ResourceBudget,
    budget_enforcer: Option<&super::SharedMutationBudgetEnforcer>,
    fallback_unrestricted: bool,
    peer_ip: Option<std::net::IpAddr>,
) -> Option<StreamSession> {
    // Re-authentication is required on resume (RFC 0005 §6.2).
    // peer_ip is passed for LocalSocketCredential loopback gating (hud-1aswu.1).
    let auth_result = authenticate_session_init(
        resume.auth_credential.as_ref(),
        &resume.pre_shared_key,
        psk,
        peer_ip,
    );
    match auth_result {
        AuthResult::Accepted => {}
        AuthResult::Failed(reason) | AuthResult::Unimplemented(reason) => {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: "AUTH_FAILED".to_string(),
                        message: reason,
                        hint: String::new(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // Step 2: Validate the resume token.
    let current_ms = now_ms();
    let resume_result = {
        let mut st = state.lock().await;
        st.token_store
            .consume(&resume.resume_token, &resume.agent_id, current_ms)
    };

    let prior_entry = match resume_result {
        Ok(entry) => entry,
        Err(err) => {
            // Token invalid or expired — agent must perform a full SessionInit.
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: err.error_code().to_string(),
                        message: err.message().to_string(),
                        hint: err.hint().to_string(),
                    })),
                }))
                .await;
            return None;
        }
    };

    // Step 3: Build restored session.
    let session_uuid = uuid::Uuid::now_v7();
    let session_id = session_uuid.to_string();
    let namespace = resume.agent_id.clone();
    // Issue a fresh single-use token for the resumed session (RFC 0005 §6.3).
    let new_resume_token = uuid::Uuid::now_v7().as_bytes().to_vec();
    let scene_session_id = tze_hud_scene::SceneId::from_uuid(session_uuid);
    let resource_budget = agent_resource_budgets
        .get(&resume.agent_id)
        .cloned()
        .unwrap_or_else(|| fallback_resource_budget.clone());
    let restored_usage = {
        let st = state.lock().await;
        let scene = st.scene.lock().await;
        prior_entry.orphaned_lease_ids.iter().fold(
            super::MutationBudgetUsage::default(),
            |mut total, lease_id| {
                let usage = scene.lease_resource_usage(lease_id);
                total.tiles = total.tiles.saturating_add(usage.tiles);
                total.texture_bytes = total.texture_bytes.saturating_add(usage.texture_bytes);
                total
            },
        )
    };
    if let Some(enforcer) = budget_enforcer {
        if let super::MutationBudgetDecision::Reject {
            error_code,
            message,
        }
        | super::MutationBudgetDecision::Revoke {
            error_code,
            message,
        } = enforcer.register_session(
            scene_session_id,
            namespace.clone(),
            resource_budget.clone(),
            agent_resource_budgets.contains_key(&resume.agent_id),
            restored_usage,
        ) {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: error_code.to_string(),
                        message,
                        hint: String::new(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // Register the resumed agent in the session registry so shared-state
    // operations (e.g. lease grant, broadcast) can find it, and capture the
    // current upload-rate configuration for this session.
    let upload_rate_limit_bytes_per_sec = {
        let mut st = state.lock().await;
        let _ = st
            .sessions
            .authenticate(&resume.agent_id, psk, &prior_entry.capabilities);
        st.resource_store.upload_rate_limit_bytes_per_sec()
    };

    // Reconstruct policy_caps for the resumed session using the same config-driven
    // lookup as new sessions.  `capabilities` (restored from TokenStore) holds the
    // grants the agent actually held before disconnect.  `policy_capabilities` governs
    // mid-session CapabilityRequest escalation and must reflect the agent's full
    // *authorization* scope (not just the already-granted subset), so that
    // post-resume escalation requests stay within the registered allow-list.
    let resume_policy_caps =
        authorization_scope_for_agent(&resume.agent_id, agent_capabilities, fallback_unrestricted);
    let session_open_at = now_wall_us();
    let mut session = StreamSession {
        session_id: session_id.clone(),
        namespace: namespace.clone(),
        agent_name: resume.agent_id.clone(),
        capabilities: prior_entry.capabilities.clone(),
        policy_capabilities: resume_policy_caps,
        // Restore orphaned leases so the agent can continue using them.
        lease_ids: prior_entry.orphaned_lease_ids.clone(),
        scene_session_id,
        resource_budget,
        budget_enforcer: budget_enforcer.cloned(),
        // Restore subscription set from before the disconnect.
        subscriptions: prior_entry.subscriptions.clone(),
        // Subscription filters are not persisted across reconnects; agents must re-send
        // subscribe_filter entries after resuming if they still need prefix filtering.
        subscription_filters: std::collections::HashMap::new(),
        server_sequence: 0,
        resume_token: new_resume_token.clone(),
        last_heartbeat_ms: now_ms(),
        state: SessionState::Resuming,
        last_client_sequence: 1, // SessionResume is sequence 1; start validation from next
        safe_mode_active: false,
        expect_resume: false,
        agent_event_rate_limiter: AgentEventRateLimiter::new(),
        freeze_queue: SessionFreezeQueue::new(FREEZE_QUEUE_CAPACITY),
        session_open_at_wall_us: session_open_at,
        dedup_window: DedupWindow::new(1000, 60),
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            upload_rate_limit_bytes_per_sec,
        ),
    };

    let compositor_ts = now_wall_us();
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: compositor_ts,
            payload: Some(ServerPayload::SessionResumeResult(SessionResumeResult {
                accepted: true,
                new_session_token: new_resume_token.clone(),
                new_server_sequence: seq,
                // Resume always runs at the highest runtime-supported version.
                // version = major * 1000 + minor; v1.1 = 1001.
                negotiated_protocol_version: crate::auth::RUNTIME_MAX_VERSION,
                // RFC 0005 §6.3: agents MUST use confirmed state, not assume pre-disconnect set.
                granted_capabilities: prior_entry.capabilities,
                active_subscriptions: prior_entry.subscriptions,
                denied_subscriptions: Vec::new(),
                error: String::new(),
            })),
        }))
        .await;

    Some(session)
}
