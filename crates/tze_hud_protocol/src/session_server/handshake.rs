//! Session handshake handlers — SS-6 submodule.
//!
//! Contains the authenticated `SessionInit` handshake.
//! The dispatcher (`dispatch_message`) and session loop remain in `mod.rs`
//! and call these functions unchanged.

use crate::auth::identify_session;
use crate::lease::{DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY, LeaseCorrelationCache};
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::{SessionCloser, SharedState};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::config::{AgentDirectory, AgentIdentity, AuthRejection, SharedAgents};
use tze_hud_scene::types::ResourceBudget;

use super::lifecycle::SessionState;
use super::stream_session::StreamSession;
use super::upload::UploadByteRateLimiter;
use super::{DEFAULT_HEARTBEAT_INTERVAL_MS, now_wall_us};

/// Owns the exact successful budget registration across handshake and handler awaits.
pub(super) struct BudgetRegistrationGuard {
    scene_session_id: tze_hud_scene::SceneId,
    enforcer: super::SharedMutationBudgetEnforcer,
}

impl BudgetRegistrationGuard {
    fn new(
        scene_session_id: tze_hud_scene::SceneId,
        enforcer: super::SharedMutationBudgetEnforcer,
    ) -> Self {
        Self {
            scene_session_id,
            enforcer,
        }
    }
}

impl Drop for BudgetRegistrationGuard {
    fn drop(&mut self) {
        self.enforcer.remove_session(self.scene_session_id);
    }
}

/// Dispatch the initial inbound read, including failures before identification.
pub(super) async fn handle_handshake_read(
    ctx: HandshakeCtx<'_>,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    read: Result<Option<ClientMessage>, Status>,
) -> Option<(
    StreamSession,
    Option<BudgetRegistrationGuard>,
    ServerMessage,
)> {
    let error = match read {
        Ok(Some(message)) => match message.payload {
            Some(client_message::Payload::SessionInit(init)) => {
                return handle_session_init(ctx, tx, &init).await;
            }
            _ => SessionError {
                code: "INVALID_HANDSHAKE".to_string(),
                message: "First message must be SessionInit".to_string(),
                hint: "Send SessionInit as the first message on a new stream".to_string(),
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

/// Register the session, with its close signal, unless its credential stopped
/// authenticating since the handshake read the directory.
///
/// The owner's Unpair swaps the live directory, then closes the agent's
/// registered sessions under the state lock this runs under. So a handshake
/// either registers first, and is closed, or sees the swapped directory here
/// and is refused: none slips between the two.
fn register_if_paired(
    st: &mut SharedState,
    namespace: &str,
    capabilities: &[String],
    authorization: &tze_hud_scene::config::agents::PairingAuthorization,
    closer: &SessionCloser,
    recheck: impl FnOnce() -> Result<AgentIdentity, AuthRejection>,
) -> Option<crate::session::AgentSession> {
    match recheck() {
        Ok(identity)
            if identity.agent_id == namespace
                && identity.authorization == *authorization
                && authorization.with_live_grants(|_| ()).is_some() =>
        {
            Some(
                st.sessions
                    .register(namespace, capabilities, closer.clone()),
            )
        }
        _ => {
            tracing::info!(
                namespace,
                "handshake refused: the agent was unpaired meanwhile"
            );
            None
        }
    }
}

async fn send_unpaired(tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>) {
    let _ = tx
        .send(Err(Status::unauthenticated(
            "this agent was unpaired; pair again to reconnect",
        )))
        .await;
}

/// What the handshake handlers read from the service, borrowed per stream.
#[derive(Clone, Copy)]
pub(super) struct HandshakeCtx<'a> {
    pub state: &'a Arc<Mutex<SharedState>>,
    /// The agent directory as of this handshake.
    pub agents: &'a AgentDirectory,
    /// The live directory, re-checked at registration (see [`register_if_paired`]).
    pub live_agents: &'a SharedAgents,
    /// Ends this session when the owner unpairs its agent.
    pub closer: &'a SessionCloser,
    pub resource_budget: &'a ResourceBudget,
    pub budget_enforcer: Option<&'a super::SharedMutationBudgetEnforcer>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_session_init(
    ctx: HandshakeCtx<'_>,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    init: &SessionInit,
) -> Option<(
    StreamSession,
    Option<BudgetRegistrationGuard>,
    ServerMessage,
)> {
    let HandshakeCtx {
        state,
        agents,
        live_agents,
        closer,
        resource_budget,
        budget_enforcer,
    } = ctx;
    let identity = match identify_session(agents, init.auth_credential.as_ref(), &init.agent_id) {
        Ok(identity) => identity,
        Err(rejection) => {
            send_auth_failed(tx, rejection).await;
            return None;
        }
    };
    let authorization = identity.authorization;
    let granted_capabilities = identity.permissions;

    let session_uuid = uuid::Uuid::now_v7();
    let namespace = identity.agent_id.clone();
    let scene_session_id = tze_hud_scene::SceneId::from_uuid(session_uuid);
    let resource_budget = resource_budget.clone();
    let registration_guard = if let Some(enforcer) = budget_enforcer {
        if let super::MutationBudgetDecision::Reject {
            error_code,
            message,
        } = enforcer.register_session(
            scene_session_id,
            &namespace,
            resource_budget,
            agents.contains(&namespace),
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
        Some(BudgetRegistrationGuard::new(
            scene_session_id,
            Arc::clone(enforcer),
        ))
    } else {
        None
    };

    // Register session in the session registry and capture upload rate config.
    let (session_id, upload_rate_limit_bytes_per_sec) = {
        let mut st = state.lock().await;
        let recheck = || {
            identify_session(
                &live_agents.load(),
                init.auth_credential.as_ref(),
                &init.agent_id,
            )
        };
        let Some(registered) = register_if_paired(
            &mut st,
            &namespace,
            &granted_capabilities,
            &authorization,
            closer,
            recheck,
        ) else {
            drop(st);
            send_unpaired(tx).await;
            return None;
        };
        (
            registered.session_id,
            st.resource_store.upload_rate_limit_bytes_per_sec(),
        )
    };
    let mut session = StreamSession {
        authorization,
        publication_defaults: Default::default(),
        session_id,
        namespace: namespace.clone(),
        agent_name: namespace.clone(),
        capabilities: granted_capabilities,
        publication_origin: scene_session_id,
        server_sequence: 0,
        state: SessionState::Handshaking,
        last_client_sequence: 1, // SessionInit is sequence 1; start validation from next
        safe_mode_active: false,
        lease_correlation_cache: LeaseCorrelationCache::new(
            DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY,
        ),
        resource_upload_rate_limiter: UploadByteRateLimiter::with_limit(
            upload_rate_limit_bytes_per_sec,
        ),
    };

    let compositor_ts = now_wall_us();

    let seq = session.next_server_seq();
    // No await follows successful registry insertion. The caller arms the
    // cleanup owner before sending this prepared establishment message.
    let established = ServerMessage {
        sequence: seq,
        timestamp_wall_us: compositor_ts,
        payload: Some(ServerPayload::SessionEstablished(SessionEstablished {
            // Reuse the already-created UUID bytes directly; no need to
            // re-parse the string we just formatted.
            session_id: session_uuid.as_bytes().to_vec(),
            namespace,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            server_sequence: seq,
            compositor_timestamp_wall_us: compositor_ts,
        })),
    };

    Some((session, registration_guard, established))
}
