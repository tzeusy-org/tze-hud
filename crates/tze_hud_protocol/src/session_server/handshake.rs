//! Session handshake handlers — SS-6 submodule.
//!
//! Contains `handle_session_init` and `handle_session_resume`, extracted
//! mechanically from `mod.rs`.
//! The dispatcher (`dispatch_message`) and session loop remain in `mod.rs`
//! and call these functions unchanged.

use crate::auth::{identify_session, negotiate_version};
use crate::dedup::DedupWindow;
use crate::lease::{DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY, LeaseCorrelationCache};
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;
use crate::subscriptions;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::config::{AgentDirectory, AuthRejection};
use tze_hud_scene::types::ResourceBudget;

use super::freeze_queue::{FREEZE_QUEUE_CAPACITY, SessionFreezeQueue};
use super::lifecycle::SessionState;
use super::stream_session::StreamSession;
use super::upload::UploadByteRateLimiter;
use super::{DEFAULT_HEARTBEAT_INTERVAL_MS, now_wall_us};

/// Dispatch the initial inbound read, including failures before identification.
pub(super) async fn handle_handshake_read(
    ctx: HandshakeCtx<'_>,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    read: Result<Option<ClientMessage>, Status>,
) -> Option<StreamSession> {
    let error = match read {
        Ok(Some(message)) => match message.payload {
            Some(client_message::Payload::SessionInit(init)) => {
                return handle_session_init(ctx, tx, &init).await;
            }
            Some(client_message::Payload::SessionResume(resume)) => {
                return handle_session_resume(ctx, tx, &resume).await;
            }
            _ => SessionError {
                code: "INVALID_HANDSHAKE".to_string(),
                message: "First message must be SessionInit or SessionResume".to_string(),
                hint: "Send SessionInit or SessionResume as the first message on a new stream"
                    .to_string(),
            },
        },
        Ok(None) => SessionError {
            code: "HANDSHAKE_TIMEOUT".to_string(),
            message: "Stream closed before handshake".to_string(),
            hint: "Open a new stream and send SessionInit as the first message".to_string(),
        },
        Err(error) => SessionError {
            code: "HANDSHAKE_ERROR".to_string(),
            message: format!("Error receiving handshake: {error}"),
            hint: "Open a new stream and send SessionInit as the first message".to_string(),
        },
    };
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::SessionError(error)),
        }))
        .await;
    None
}

async fn send_auth_failed(
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    rejection: AuthRejection,
) {
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: 1,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::SessionError(SessionError {
                code: rejection.code.to_string(),
                message: rejection.message,
                hint: rejection.hint,
            })),
        }))
        .await;
}

/// What the handshake handlers read from the service, borrowed per stream.
#[derive(Clone, Copy)]
pub(super) struct HandshakeCtx<'a> {
    pub state: &'a Arc<Mutex<SharedState>>,
    /// The agent directory as of this handshake.
    pub agents: &'a AgentDirectory,
    pub resource_budget: &'a ResourceBudget,
    pub budget_enforcer: Option<&'a super::SharedMutationBudgetEnforcer>,
    /// Peer address, for loopback gating of local-socket credentials.
    pub peer_ip: Option<std::net::IpAddr>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_init(
    ctx: HandshakeCtx<'_>,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    init: &SessionInit,
) -> Option<StreamSession> {
    let HandshakeCtx {
        state,
        agents,
        resource_budget,
        budget_enforcer,
        peer_ip,
    } = ctx;
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

    // ── Step 2: Identity (PSK → agent, allow → permissions) ──────────────────
    // peer_ip is passed for LocalSocketCredential loopback gating (hud-1aswu.1).
    let identity = match identify_session(
        agents,
        init.auth_credential.as_ref(),
        "",
        &init.agent_id,
        peer_ip,
    ) {
        Ok(identity) => identity,
        Err(rejection) => {
            send_auth_failed(tx, rejection).await;
            return None;
        }
    };
    let granted_capabilities = identity.permissions;

    // ── Step 3: Subscription filtering (RFC 0005 §7.1) ──────────────────────
    // Mandatory categories are always active; gated categories need the
    // matching permission from the agent's `allow` list.
    let sub_result =
        subscriptions::filter_subscriptions(&init.initial_subscriptions, &granted_capabilities);

    let session_uuid = uuid::Uuid::now_v7();
    let namespace = identity.agent_id.clone();
    let resume_token = uuid::Uuid::now_v7().as_bytes().to_vec();
    let scene_session_id = tze_hud_scene::SceneId::from_uuid(session_uuid);
    let resource_budget = resource_budget.clone();
    if let Some(enforcer) = budget_enforcer {
        if let super::MutationBudgetDecision::Reject {
            error_code,
            message,
        } = enforcer.register_session(
            scene_session_id,
            namespace.clone(),
            resource_budget.clone(),
            agents.contains(&namespace),
            super::MutationBudgetUsage::default(),
        ) {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: error_code.to_string(),
                        message,
                        hint: "Wait for session capacity to become available, then open a new stream with SessionInit"
                            .to_string(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // Register session in the session registry and capture upload rate config.
    let (session_id, upload_rate_limit_bytes_per_sec) = {
        let mut st = state.lock().await;
        let registered = st.sessions.register(&namespace, &granted_capabilities);
        #[cfg(test)]
        st.sessions
            .bind_cleanup_session(&registered.session_id, scene_session_id);
        (
            registered.session_id,
            st.resource_store.upload_rate_limit_bytes_per_sec(),
        )
    };
    let scene_clock = Arc::clone(&state.lock().await.scene);
    let session_open_at = scene_clock.lock().await.now_wall_us();
    let mut session = StreamSession {
        session_id,
        namespace: namespace.clone(),
        agent_name: namespace.clone(),
        capabilities: granted_capabilities,
        lease_ids: Vec::new(),
        scene_session_id,
        resource_budget,
        budget_enforcer: budget_enforcer.cloned(),
        subscriptions: sub_result.active.clone(),
        server_sequence: 0,
        resume_token: resume_token.clone(),
        state: SessionState::Handshaking,
        last_client_sequence: 1, // SessionInit is sequence 1; start validation from next
        safe_mode_active: false,
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
            payload: Some(ServerPayload::SessionEstablished(SessionEstablished {
                // Reuse the already-created UUID bytes directly; no need to
                // re-parse the string we just formatted.
                session_id: session_uuid.as_bytes().to_vec(),
                namespace,
                resume_token,
                heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
                server_sequence: seq,
                compositor_timestamp_wall_us: compositor_ts,
                active_subscriptions: sub_result.active,
                denied_subscriptions: sub_result.denied,
                negotiated_protocol_version: negotiated_version,
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
    ctx: HandshakeCtx<'_>,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    resume: &SessionResume,
) -> Option<StreamSession> {
    let HandshakeCtx {
        state,
        agents,
        resource_budget,
        budget_enforcer,
        peer_ip,
    } = ctx;
    // Re-authentication is required on resume (RFC 0005 §6.2).
    // peer_ip is passed for LocalSocketCredential loopback gating (hud-1aswu.1).
    let identity = match identify_session(
        agents,
        resume.auth_credential.as_ref(),
        &resume.pre_shared_key,
        &resume.agent_id,
        peer_ip,
    ) {
        Ok(identity) => identity,
        Err(rejection) => {
            send_auth_failed(tx, rejection).await;
            return None;
        }
    };

    // Step 2: Validate the resume token.
    // Token expiry is measured on the scene clock, the same clock that
    // drives orphan grace expiry, so both end together.
    let resume_result = {
        let mut st = state.lock().await;
        let current_ms = st.scene.lock().await.now_millis();
        st.token_store
            .consume(&resume.resume_token, &identity.agent_id, current_ms)
    };

    let mut prior_entry = match resume_result {
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

    // Reconnect the orphaned leases (ORPHANED → ACTIVE, badge cleared). Leases
    // the runtime already reclaimed are dropped from the restored set.
    {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let now = scene.now_millis();
        prior_entry.orphaned_lease_ids.retain(|lease_id| {
            match scene.leases.get(lease_id).map(|l| l.state) {
                Some(tze_hud_scene::LeaseState::Orphaned) => {
                    scene.reconnect_lease(lease_id, now).is_ok()
                }
                Some(state) => !state.is_terminal(),
                None => false,
            }
        });
    }

    // Step 3: Build restored session.
    let session_uuid = uuid::Uuid::now_v7();
    let namespace = identity.agent_id.clone();
    // Issue a fresh single-use token for the resumed session (RFC 0005 §6.3).
    let new_resume_token = uuid::Uuid::now_v7().as_bytes().to_vec();
    let scene_session_id = tze_hud_scene::SceneId::from_uuid(session_uuid);
    let resource_budget = resource_budget.clone();
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
        } = enforcer.register_session(
            scene_session_id,
            namespace.clone(),
            resource_budget.clone(),
            agents.contains(&namespace),
            restored_usage,
        ) {
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: 1,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::SessionError(SessionError {
                        code: error_code.to_string(),
                        message,
                        hint: "Wait for resource capacity to become available, then open a new stream with SessionInit; this resume token has been consumed"
                            .to_string(),
                    })),
                }))
                .await;
            return None;
        }
    }

    // Register the resumed agent in the session registry so shared-state
    // operations (e.g. lease grant, broadcast) can find it, and capture the
    // current upload-rate configuration for this session.
    let (session_id, upload_rate_limit_bytes_per_sec) = {
        let mut st = state.lock().await;
        let registered = st.sessions.register(&namespace, &identity.permissions);
        #[cfg(test)]
        st.sessions
            .bind_cleanup_session(&registered.session_id, scene_session_id);
        (
            registered.session_id,
            st.resource_store.upload_rate_limit_bytes_per_sec(),
        )
    };

    let scene_clock = Arc::clone(&state.lock().await.scene);
    let session_open_at = scene_clock.lock().await.now_wall_us();
    let mut session = StreamSession {
        session_id,
        namespace: namespace.clone(),
        agent_name: namespace.clone(),
        // Permissions come from the current config, not the pre-disconnect set.
        capabilities: identity.permissions,
        // Restore orphaned leases so the agent can continue using them.
        lease_ids: prior_entry.orphaned_lease_ids.clone(),
        scene_session_id,
        resource_budget,
        budget_enforcer: budget_enforcer.cloned(),
        // Restore subscription set from before the disconnect.
        subscriptions: prior_entry.subscriptions.clone(),
        server_sequence: 0,
        resume_token: new_resume_token.clone(),
        state: SessionState::Resuming,
        last_client_sequence: 1, // SessionResume is sequence 1; start validation from next
        safe_mode_active: false,
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
                active_subscriptions: prior_entry.subscriptions,
                denied_subscriptions: Vec::new(),
            })),
        }))
        .await;

    Some(session)
}
