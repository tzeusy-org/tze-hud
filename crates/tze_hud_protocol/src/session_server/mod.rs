//! Bidirectional streaming session server implementing.
//!
//! This module provides `HudSessionImpl`, the server-side implementation of the
//! `HudSession` gRPC service. It manages the bidirectional streaming session
//! lifecycle: handshake, publication management, resource uploads, heartbeats,
//! degradation notices, and termination.
//!
//! # Session Lifecycle State Machine
//!
//! ```text
//! Connecting → Handshaking → Active → Disconnecting → Closed
//! ```
//!
//! Valid transitions:
//! - Connecting → Handshaking (stream opened, SessionInit received)
//! - Handshaking → Active (valid auth → SessionEstablished)
//! - Handshaking → Closed (auth failure → SessionError(AUTH_FAILED))
//! - Active → Disconnecting (SessionClose received)
//! - Active → Closed (ungraceful: heartbeat timeout or stream EOF/RST)
//! - Disconnecting → Closed (stream termination complete)

// LeaseCorrelationCache and DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY are used
// transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use crate::lease::{DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY, LeaseCorrelationCache};
use crate::proto::session::client_message::Payload as ClientPayload;
use crate::proto::session::hud_session_server::HudSession;
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::{SESSION_EVENT_CHANNEL_CAPACITY, SharedState};
use std::sync::Arc;
// Duration and Instant are used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tze_hud_resource::{
    ResourceError as StoreResourceError, ResourceStored as StoreResourceStored,
};

// ─── Submodules (SS-1..SS-7h) ────────────────────────────────────────────────

pub mod budget_gate;
pub mod degradation_notice_bus;
mod element_persist;
pub mod handshake;
pub mod lifecycle;
pub mod service;
pub mod stream_session;
pub mod upload;
pub mod verbs;

pub use budget_gate::{
    MutationBudgetDecision, MutationBudgetEnforcer, SharedMutationBudgetEnforcer,
};
pub use degradation_notice_bus::{DegradationNoticeReceiver, DegradationNoticeSender};
use element_persist::{persist_element_store, touch_element_store_entry_by_namespace};
#[cfg(test)]
use handshake::handle_session_init;
use handshake::{HandshakeCtx, handle_handshake_read};
pub use lifecycle::SessionState;
pub use service::{HudSessionImpl, SessionDeps};
use stream_session::StreamSession;
use upload::{UploadWorkerCommand, UploadWorkerEvent, run_upload_worker};
use verbs::{handle_clear, handle_hold, handle_publish};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Default heartbeat interval in milliseconds.
pub(super) const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 5000;

/// Default heartbeat missed threshold (number of missed heartbeats before disconnect).
const HEARTBEAT_MISSED_THRESHOLD: u64 = 3;

/// Default heartbeat timeout: threshold * interval.
const DEFAULT_HEARTBEAT_TIMEOUT_MS: u64 =
    DEFAULT_HEARTBEAT_INTERVAL_MS * HEARTBEAT_MISSED_THRESHOLD;

/// Default maximum sequence gap before SEQUENCE_GAP_EXCEEDED.
const DEFAULT_MAX_SEQUENCE_GAP: u64 = 100;

// ─── Helper ─────────────────────────────────────────────────────────────────

pub(super) fn now_wall_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // tonic::Status is large by design; boxing it would add indirection on every call
pub(super) fn bytes_to_scene_id(bytes: &[u8]) -> Result<tze_hud_scene::SceneId, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "invalid scene ID: expected 16 bytes, got {}",
            bytes.len()
        )));
    }
    // Length is checked to be exactly 16 above; the conversion cannot fail.
    let arr: [u8; 16] = bytes
        .try_into()
        .expect("bytes length is exactly 16, checked above");
    let uuid = uuid::Uuid::from_bytes(arr);
    Ok(tze_hud_scene::SceneId::from_uuid(uuid))
}

// This observer is private to the owning service's retained lifecycle fixture.
// The handle is the real session task, not the Tonic transport server task.
#[cfg(test)]
struct ObservedSessionHandler {
    task: tokio::task::JoinHandle<()>,
    panic: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
fn session_handler_observers() -> &'static std::sync::Mutex<
    std::collections::HashMap<usize, tokio::sync::mpsc::UnboundedSender<ObservedSessionHandler>>,
> {
    static OBSERVERS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<
                usize,
                tokio::sync::mpsc::UnboundedSender<ObservedSessionHandler>,
            >,
        >,
    > = std::sync::OnceLock::new();
    OBSERVERS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
struct SessionHandlerObserver {
    state: Arc<Mutex<SharedState>>,
}

#[cfg(test)]
impl Drop for SessionHandlerObserver {
    fn drop(&mut self) {
        session_handler_observers()
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&self.state) as usize));
    }
}

#[cfg(test)]
fn observe_session_handlers(
    state: &Arc<Mutex<SharedState>>,
) -> (
    SessionHandlerObserver,
    tokio::sync::mpsc::UnboundedReceiver<ObservedSessionHandler>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(
        session_handler_observers()
            .lock()
            .unwrap()
            .insert(Arc::as_ptr(state) as usize, tx)
            .is_none(),
        "only one observer may own this exact test service"
    );
    (
        SessionHandlerObserver {
            state: Arc::clone(state),
        },
        rx,
    )
}

#[cfg(test)]
fn spawn_observed_session_handler(
    state: &Arc<Mutex<SharedState>>,
    handler: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let observer = session_handler_observers()
        .lock()
        .unwrap()
        .get(&(Arc::as_ptr(state) as usize))
        .cloned();
    if let Some(observer) = observer {
        let (panic, panic_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::pin!(handler);
            tokio::select! {
                () = &mut handler => {}
                requested = panic_rx => {
                    if requested.is_ok() {
                        panic!("retained fixture requested panic in its actual session handler");
                    }
                    handler.await;
                }
            }
        });
        // Dropping an observer does not cancel an unrelated or unobserved task.
        let _ = observer.send(ObservedSessionHandler { task, panic });
    } else {
        tokio::spawn(handler);
    }
}

// ─── Service implementation (SS-5) ──────────────────────────────────────────
//
// `HudSessionImpl` struct, constructors, and non-session runtime helpers live in
// `service.rs`. The `async fn session` dispatch loop (the `HudSession` trait impl)
// stays here as a split `impl HudSession for HudSessionImpl` block.

#[tonic::async_trait]
impl HudSession for HudSessionImpl {
    type SessionStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn session(
        &self,
        request: Request<tonic::Streaming<ClientMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let mut inbound = request.into_inner();
        let state = self.state.clone();
        let agents = self.agents.clone();
        let resource_budget = self.resource_budget.clone();
        let budget_enforcer = self.budget_enforcer.clone();
        let render_wake = self.render_wake.clone();
        let publication_defaults = self.publication_defaults.clone();
        let degradation_notices = self.degradation_notices.clone();

        // Create outbound channel
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(
            SESSION_EVENT_CHANNEL_CAPACITY,
        );

        // The owner's unpair ends the session from outside (see below).
        let (closer, close) = crate::session::SessionCloser::pair();
        let closing_tx = tx.clone();
        let live_agents = self.agents.clone();

        // Cleanup outlives a cancelled handler and owns the completion signal.
        let cleanup_guard =
            SessionCleanupGuard::new(state.clone(), render_wake.clone(), close.clone());
        let cleanup = Arc::clone(&cleanup_guard.shared);

        // Spawn the session handler task
        #[cfg(test)]
        let handler_state = Arc::clone(&state);
        let session_handler = async move {
            // Wait for the first message (must be SessionInit)
            let first_read = match tokio::time::timeout(
                tokio::time::Duration::from_millis(5000),
                inbound.message(),
            )
            .await
            {
                Ok(read) => read,
                Err(_) => {
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: 1,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::SessionError(SessionError {
                                code: "HANDSHAKE_TIMEOUT".to_string(),
                                message: "Handshake timed out (5000ms)".to_string(),
                                hint: "Send SessionInit as the first message".to_string(),
                            })),
                        }))
                        .await;
                    return;
                }
            };

            // Process handshake against the agents paired as of now.
            let agents = agents.load_full();
            let handshake_ctx = HandshakeCtx {
                state: &state,
                agents: &agents,
                live_agents: &live_agents,
                closer: &closer,
                resource_budget: &resource_budget,
                budget_enforcer: budget_enforcer.as_ref(),
            };
            let Some((mut session, budget_registration, established)) =
                handle_handshake_read(handshake_ctx, &tx, first_read).await
            else {
                return; // Handshake failed, error already sent
            };

            session.publication_defaults = publication_defaults;

            {
                let mut cleanup = cleanup.lock().unwrap_or_else(|p| p.into_inner());
                let cleanup = cleanup.as_mut().expect("session cleanup is armed");
                cleanup.session = Some(RegisteredResident {
                    session_id: session.session_id.clone(),
                    publication_origin: session.publication_origin,
                    budget_registration,
                });
            }

            // Registry registration and cleanup arming have no intervening await.
            if tx.send(Ok(established)).await.is_err() {
                return;
            }

            // Dropping this block closes upload channels before cleanup joins it.
            'active: {
                // Transition: Handshaking → Active
                session.transition(SessionState::Active);

                // Safe mode reaches this stream through the registry: it needs
                // the outbound sender to send `SessionSuspended`/`SessionResumed`.
                // `remove_session` at cleanup drops it.
                {
                    let mut st = state.lock().await;
                    st.sessions
                        .register_server_message_tx(&session.session_id, tx.clone());
                }

                // Atomically subscribe and send the current policy before later events.
                let (mut degradation_rx, current_degradation) =
                    degradation_notices.subscribe_with_current();
                let seq = session.next_server_seq();
                if tx
                    .send(Ok(ServerMessage {
                        sequence: seq,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(ServerPayload::DegradationNotice(current_degradation)),
                    }))
                    .await
                    .is_err()
                {
                    break 'active;
                }

                let upload_rate_limit_bytes_per_sec =
                    session.resource_upload_rate_limiter.limit_bytes_per_second;
                let (upload_command_tx, upload_command_rx) =
                    tokio::sync::mpsc::channel::<UploadWorkerCommand>(64);
                let (upload_event_tx, mut upload_event_rx) =
                    tokio::sync::mpsc::channel::<UploadWorkerEvent>(64);
                let upload_worker = tokio::spawn(run_upload_worker(
                    state.clone(),
                    session.namespace.clone(),
                    session.authorization.clone(),
                    upload_command_rx,
                    upload_event_tx,
                    upload_rate_limit_bytes_per_sec,
                    render_wake.clone(),
                ));
                cleanup
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .expect("session cleanup is armed")
                    .upload_worker = Some(upload_worker);

                // Main message loop
                //
                // EOF, transport errors, explicit close and the existing receive
                // timeout all end this physical session and release its content.
                //
                // The loop also listens on `degradation_rx` for transactional DegradationNotice
                // broadcasts. These are delivered unconditionally to all active
                // sessions and are never dropped.
                loop {
                    // Use heartbeat timeout for receive
                    let timeout_duration =
                        tokio::time::Duration::from_millis(DEFAULT_HEARTBEAT_TIMEOUT_MS);

                    tokio::select! {
                        // ── Inbound client message ────────────────────────────────
                        msg_result = tokio::time::timeout(timeout_duration, inbound.message()) => {
                            match session.on_client_message(
                                msg_result,
                                &state,
                                &tx,
                                &upload_command_tx,
                                &render_wake,
                            ).await {
                                LoopAction::Continue => continue,
                                LoopAction::Break => break,
                            }
                        }

                        upload_event = upload_event_rx.recv() => {
                            if let LoopAction::Break = session.on_upload_event(upload_event, &tx).await {
                                break;
                            }
                        }

                        // ── DegradationNotice broadcast ────
                        //
                        // Transactional — delivered unconditionally to all active sessions
                        // Never dropped.
                        degradation_notice = degradation_rx.recv() => {
                            if let LoopAction::Break = session.on_degradation(degradation_notice, &tx).await {
                                break;
                            }
                        }


                    }
                }
            }
        };
        // Unpair drops the handler wherever it waits, even on a send to a hung
        // agent; cleanup then releases its publications, uploads and slot. The
        // close arm is polled first, so a closed handler makes no more progress.
        let session_handler = async move {
            tokio::select! {
                biased;
                () = close.closed() => {
                    tracing::info!("session closed: the owner unpaired this agent");
                    let _ = closing_tx.try_send(Err(Status::unauthenticated(
                        "this agent was unpaired; pair again to reconnect",
                    )));
                }
                () = session_handler => {}
            }
            cleanup_guard.finish().await;
            // Dropping `close` now tells the unpairing owner cleanup has finished.
        };
        #[cfg(not(test))]
        tokio::spawn(session_handler);
        #[cfg(test)]
        spawn_observed_session_handler(&handler_state, session_handler);

        // Return the receiver stream as the response
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Cleanup for one physical resident, including paths where the active future
/// is dropped by unpair, task cancellation or panic. The upload channels belong
/// to that future; dropping them wakes the worker so it aborts its pending IDs.
struct RegisteredResident {
    session_id: String,
    publication_origin: tze_hud_scene::SceneId,
    budget_registration: Option<handshake::BudgetRegistrationGuard>,
}

struct SessionCleanup {
    state: Arc<Mutex<SharedState>>,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    session: Option<RegisteredResident>,
    upload_worker: Option<tokio::task::JoinHandle<()>>,
    // The owner waits for this signal, including cleanup after cancellation.
    _close: crate::session::SessionCloseSignal,
}

impl SessionCleanup {
    async fn run(mut self) {
        // Never hold SharedState while joining: the worker needs it to finish.
        if let Some(worker) = self.upload_worker.take() {
            if let Err(error) = worker.await {
                tracing::error!(%error, "resource upload worker failed during session cleanup");
            }
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        let (changed, question_wakes) = {
            let mut st = self.state.lock().await;
            st.sessions.remove_session(&session.session_id);
            let mut scene = st.scene.lock().await;
            let changed = scene.clear_publications_for_origin(session.publication_origin);
            (changed, scene.take_notification_wait_notifiers())
        };
        for wake in question_wakes {
            wake.notify();
        }
        if changed {
            self.render_wake.notify();
        }
        // Release admission only after uploads and retained publications end.
        drop(session.budget_registration.take());
        #[cfg(test)]
        self.state
            .lock()
            .await
            .sessions
            .finish_cleanup(&session.publication_origin);
    }
}

struct SessionCleanupGuard {
    shared: Arc<std::sync::Mutex<Option<SessionCleanup>>>,
}

impl SessionCleanupGuard {
    fn new(
        state: Arc<Mutex<SharedState>>,
        render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
        close: crate::session::SessionCloseSignal,
    ) -> Self {
        Self {
            shared: Arc::new(std::sync::Mutex::new(Some(SessionCleanup {
                state,
                render_wake,
                session: None,
                upload_worker: None,
                _close: close,
            }))),
        }
    }

    fn start(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let cleanup = self
            .shared
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()?;
        // Without a runtime, the scene and upload store are shutting down too.
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(runtime.spawn(cleanup.run()))
    }

    async fn finish(mut self) {
        // A separate task keeps cleanup alive if the supervisor is cancelled
        // while awaiting it. Taking the data makes normal/Drop cleanup exclusive.
        if let Some(task) = self.start() {
            if let Err(error) = task.await {
                tracing::error!(%error, "resident session cleanup failed");
            }
        }
    }
}

impl Drop for SessionCleanupGuard {
    fn drop(&mut self) {
        drop(self.start());
    }
}

// ─── Message handlers ───────────────────────────────────────────────────────

async fn handle_client_message(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    upload_command_tx: &tokio::sync::mpsc::Sender<UploadWorkerCommand>,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
    msg: ClientMessage,
) {
    let client_sequence = msg.sequence;
    let Some(payload) = msg.payload else {
        verbs::send_result(
            session,
            tx,
            verbs::fail(
                client_sequence,
                "INVALID_ARGUMENT",
                "ClientMessage needs a supported request payload; use the current session.proto schema and send a recognized request",
            ),
        )
        .await;
        return;
    };

    match payload {
        ClientPayload::Publish(publish) => {
            handle_publish(state, session, tx, client_sequence, publish, render_wake).await;
        }
        ClientPayload::Clear(clear) => {
            handle_clear(state, session, tx, client_sequence, clear, render_wake).await;
        }
        ClientPayload::Hold(hold) => {
            handle_hold(state, session, tx, client_sequence, hold, render_wake).await;
        }
        ClientPayload::Heartbeat(hb) => {
            handle_heartbeat(session, tx, hb).await;
        }
        ClientPayload::SessionClose(_close) => {
            // Graceful disconnect: the main loop ends the stream after this returns.
        }
        ClientPayload::ResourceUploadStart(start) => {
            let _ = upload_command_tx
                .send(UploadWorkerCommand::Start {
                    request_sequence: client_sequence,
                    capabilities: session.capabilities.clone(),
                    start,
                })
                .await;
        }
        ClientPayload::ResourceUploadChunk(chunk) => {
            let _ = upload_command_tx
                .send(UploadWorkerCommand::Chunk {
                    request_sequence: client_sequence,
                    chunk,
                })
                .await;
        }
        ClientPayload::ResourceUploadComplete(complete) => {
            let _ = upload_command_tx
                .send(UploadWorkerCommand::Complete {
                    request_sequence: client_sequence,
                    capabilities: session.capabilities.clone(),
                    complete,
                })
                .await;
        }
        // SessionInit should not appear after handshake
        ClientPayload::SessionInit(_) => {
            // Protocol violation: ignore (or could send RuntimeError)
        }
    }
}

/// Signal returned by each `on_*` select-arm handler.
///
/// `Continue` — proceed to the next loop iteration.
/// `Break`    — exit the session loop (stream closed or fatal error).
enum LoopAction {
    Continue,
    Break,
}

// ─── Per-session select-arm handlers ────────────────────────────────────────
//
// Each `on_*` method below is the extracted body of one arm of the main
// `tokio::select!` in `async fn session`. The select! arms are now thin
// wrappers that call these helpers and match on `LoopAction`.

impl StreamSession {
    /// Handle an inbound client message (or timeout/error) from the stream.
    ///
    /// Encompasses: heartbeat update, retransmit fast-path, sequence validation,
    /// graceful-close detection, and dispatch to `handle_client_message`.
    async fn on_client_message(
        &mut self,
        msg_result: Result<
            Result<Option<ClientMessage>, tonic::Status>,
            tokio::time::error::Elapsed,
        >,
        state: &Arc<Mutex<SharedState>>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
        upload_command_tx: &tokio::sync::mpsc::Sender<UploadWorkerCommand>,
        render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> LoopAction {
        match msg_result {
            Ok(Ok(Some(msg))) => {
                // Retransmit fast-path.
                //
                // For lease operations there is no batch_id correlation key;
                // the client-side sequence number serves as the correlation key.
                // When the server sees a sequence it has already processed for
                // a lease operation, it replays the cached response without
                // re-applying the operation and WITHOUT running sequence
                // validation (which would reject the same sequence as a
                // regression).
                let is_lease_op = matches!(
                    &msg.payload,
                    Some(ClientPayload::Hold(_)) | Some(ClientPayload::Clear(_))
                );
                if is_lease_op
                    && msg.sequence > 0
                    && self.lease_correlation_cache.get(msg.sequence).is_some()
                {
                    // This is a retransmit: dispatch to the lease handler which
                    // will replay the cached response.  Skip sequence validation
                    // so the duplicate sequence does not terminate the session.
                    handle_client_message(state, self, tx, upload_command_tx, render_wake, msg)
                        .await;
                    return LoopAction::Continue;
                }

                // Validate client sequence number.
                // Skip validation for sequence 0 (unset) to allow legacy callers
                // that don't set sequences. Sequence must be monotonically increasing
                // starting at 2 (since 1 is the handshake message).
                if msg.sequence != 0 {
                    match self.validate_client_sequence(msg.sequence, DEFAULT_MAX_SEQUENCE_GAP) {
                        Ok(()) => {}
                        Err((code, message)) => {
                            // Close stream with sequence error
                            let seq = self.next_server_seq();
                            let _ = tx
                                .send(Ok(ServerMessage {
                                    sequence: seq,
                                    timestamp_wall_us: now_wall_us(),
                                    payload: Some(ServerPayload::SessionError(SessionError {
                                        code: code.to_string(),
                                        message,
                                        hint: format!(
                                            "Open a new stream with SessionInit; send increasing sequence numbers starting at 2 with gaps no larger than {DEFAULT_MAX_SEQUENCE_GAP}"
                                        ),
                                    })),
                                }))
                                .await;
                            self.transition(SessionState::Closed);
                            return LoopAction::Break;
                        }
                    }
                }

                // Check if this is a graceful close message
                let is_close = matches!(&msg.payload, Some(ClientPayload::SessionClose(_)));

                handle_client_message(state, self, tx, upload_command_tx, render_wake, msg).await;

                // After handling SessionClose, transition to Disconnecting then Closed
                if is_close {
                    self.transition(SessionState::Disconnecting);
                    self.transition(SessionState::Closed);
                    return LoopAction::Break;
                }

                LoopAction::Continue
            }
            Ok(Ok(None)) => {
                // Stream EOF
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
            Ok(Err(_e)) => {
                // Stream transport error — ungraceful disconnect
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
            Err(_) => {
                // Heartbeat timeout
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }

    /// Handle an event from the upload worker.
    ///
    /// Encompasses: UploadAccepted, Stored, Error, and channel-closed (→ Break).
    async fn on_upload_event(
        &mut self,
        upload_event: Option<UploadWorkerEvent>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match upload_event {
            Some(UploadWorkerEvent::UploadAccepted {
                request_sequence,
                upload_id,
            }) => {
                let seq = self.next_server_seq();
                let _ = tx
                    .send(Ok(ServerMessage {
                        sequence: seq,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(ServerPayload::ResourceUploadAccepted(
                            ResourceUploadAccepted {
                                request_sequence,
                                upload_id: upload_id.to_vec(),
                            },
                        )),
                    }))
                    .await;
                LoopAction::Continue
            }
            Some(UploadWorkerEvent::Stored {
                request_sequence,
                stored,
                stored_bytes,
                metadata,
                upload_id,
            }) => {
                send_resource_stored(
                    self,
                    tx,
                    request_sequence,
                    &stored,
                    stored_bytes,
                    metadata,
                    upload_id.as_ref(),
                )
                .await;
                LoopAction::Continue
            }
            Some(UploadWorkerEvent::Error {
                request_sequence,
                upload_id,
                err,
            }) => {
                send_resource_error_response(
                    self,
                    tx,
                    request_sequence,
                    upload_id.as_deref(),
                    &err,
                )
                .await;
                LoopAction::Continue
            }
            None => {
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }

    /// Handle a `DegradationNotice` broadcast result.
    ///
    /// Transactional — delivered unconditionally to all active sessions.
    async fn on_degradation(
        &mut self,
        degradation_notice: Option<DegradationNotice>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match degradation_notice {
            Some(notice) => {
                let seq = self.next_server_seq();
                let _ = tx
                    .send(Ok(ServerMessage {
                        sequence: seq,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(ServerPayload::DegradationNotice(notice)),
                    }))
                    .await;
                LoopAction::Continue
            }
            None => {
                // Treat as ungraceful disconnect.
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }
}

/// Maximum future schedule horizon in microseconds (default 5 minutes).
pub(super) const DEFAULT_MAX_FUTURE_SCHEDULE_US: u64 = 300_000_000;

pub(super) fn capability_grant_covers(granted: &str, requested: &str) -> bool {
    if granted == "*" || granted == requested {
        return true;
    }

    (granted == "publish_zone:*" && requested.starts_with("publish_zone:"))
        || (granted == "publish_widget:*" && requested.starts_with("publish_widget:"))
}

pub(super) fn capability_set_covers(granted: &[String], requested: &str) -> bool {
    granted
        .iter()
        .any(|grant| capability_grant_covers(grant, requested))
}

fn resource_error_code_i32(err: &StoreResourceError) -> i32 {
    match err {
        StoreResourceError::CapabilityDenied => 1,
        StoreResourceError::BudgetExceeded { .. } => 2,
        StoreResourceError::SizeExceeded { .. } => 3,
        StoreResourceError::UnsupportedType(_) => 4,
        StoreResourceError::DecodeError(_) => 5,
        StoreResourceError::HashMismatch { .. } => 6,
        StoreResourceError::UploadNotFound(_) => 9,
        StoreResourceError::InvalidChunk(_) => 7,
        StoreResourceError::TooManyUploads => 8,
        StoreResourceError::UploadAborted(_) => 9,
        StoreResourceError::Internal(_) => 7,
    }
}

async fn send_resource_stored(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    request_sequence: u64,
    stored: &StoreResourceStored,
    stored_bytes: u64,
    metadata: ResourceMetadata,
    upload_id: Option<&[u8; 16]>,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::ResourceStored(ResourceStored {
                request_sequence,
                resource_id: Some(crate::proto::ResourceIdProto {
                    bytes: stored.resource_id.as_bytes().to_vec(),
                }),
                was_deduplicated: stored.was_deduplicated,
                stored_bytes,
                decoded_bytes: stored.decoded_bytes as u64,
                metadata: Some(metadata),
                upload_id: upload_id.map(|u| u.to_vec()).unwrap_or_default(),
            })),
        }))
        .await;
}

async fn send_resource_error_response(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    request_sequence: u64,
    upload_id: Option<&[u8]>,
    err: &StoreResourceError,
) {
    let context = serde_json::json!({
        "domain": "resource_upload",
        "wire_code": err.wire_code(),
    })
    .to_string();
    let next_action = match err {
        StoreResourceError::CapabilityDenied => {
            "Ask the operator to grant upload_resource in the allow entries, then reconnect with a fresh SessionInit, then retry ResourceUploadStart"
        }
        StoreResourceError::BudgetExceeded { .. } => {
            "Use a smaller resource or wait for resource capacity, then retry ResourceUploadStart"
        }
        StoreResourceError::SizeExceeded { .. } => {
            "Declare a positive total_size_bytes and keep raw size, decoded size and dimensions within the reported limits, then retry ResourceUploadStart; use chunks above the inline limit"
        }
        StoreResourceError::UnsupportedType(_) => {
            "Choose a supported resource_type matching the bytes, then retry ResourceUploadStart"
        }
        StoreResourceError::DecodeError(_) => {
            "Repair the resource bytes and metadata for the declared resource_type, recompute expected_hash, then retry ResourceUploadStart"
        }
        StoreResourceError::HashMismatch { .. } => {
            "Compute expected_hash as the 32-byte BLAKE3 hash of the exact upload bytes, then retry ResourceUploadStart"
        }
        StoreResourceError::TooManyUploads => {
            "Complete an in-flight upload before retrying ResourceUploadStart; if none can complete, report stale upload capacity to the operator"
        }
        StoreResourceError::UploadNotFound(_) => {
            "Restart with ResourceUploadStart and wait for ResourceUploadAccepted before sending chunks"
        }
        StoreResourceError::InvalidChunk(_) => {
            "Restart with ResourceUploadStart, then send consecutive chunk_index values from 0 using the returned upload_id"
        }
        StoreResourceError::UploadAborted(_) => {
            "Restart with ResourceUploadStart and use the new returned upload_id"
        }
        StoreResourceError::Internal(_) => {
            "Retry ResourceUploadStart; if the failure persists, report it to the operator"
        }
    };
    let hint = serde_json::json!({
        "expected_flow": "ResourceUploadStart -> [ResourceUploadAccepted] -> ResourceUploadChunk* -> ResourceUploadComplete",
        "next_action": next_action,
    })
    .to_string();
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::ResourceErrorResponse(
                ResourceErrorResponse {
                    request_sequence,
                    error_code: resource_error_code_i32(err),
                    message: err.to_string(),
                    context,
                    hint,
                    upload_id: upload_id.map(|u| u.to_vec()).unwrap_or_default(),
                },
            )),
        }))
        .await;
}

async fn handle_heartbeat(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    hb: Heartbeat,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::Heartbeat(Heartbeat {
                // Echo the client's monotonic timestamp for RTT calculation
                timestamp_mono_us: hb.timestamp_mono_us,
            })),
        }))
        .await;
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
