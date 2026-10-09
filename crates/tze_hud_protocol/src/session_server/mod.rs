//! Bidirectional streaming session server implementing RFC 0005.
//!
//! This module provides `HudSessionImpl`, the server-side implementation of the
//! `HudSession` gRPC service. It manages the bidirectional streaming session
//! lifecycle: handshake, mutation processing, lease management, heartbeats,
//! event dispatch, and reconnection.
//!
//! # Session Lifecycle State Machine (RFC 0005 §1.1)
//!
//! ```text
//! Connecting → Handshaking → Active → Disconnecting → Closed → Resuming
//! ```
//!
//! Valid transitions:
//! - Connecting → Handshaking (stream opened, SessionInit received)
//! - Handshaking → Active (valid auth → SessionEstablished)
//! - Handshaking → Closed (auth failure → SessionError(AUTH_FAILED))
//! - Active → Disconnecting (SessionClose received)
//! - Active → Closed (ungraceful: heartbeat timeout or stream EOF/RST)
//! - Disconnecting → Closed (stream termination complete)
//! - Closed → Resuming (SessionResume within grace period)
//! - Resuming → Active (valid resume token)
//! - Resuming → Closed (expired/invalid token)

// DedupWindow is used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use crate::dedup::{CachedResult, DedupWindow};
// LeaseCorrelationCache and DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY are used
// transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use crate::lease::{DEFAULT_LEASE_CORRELATION_CACHE_CAPACITY, LeaseCorrelationCache};
use crate::proto::session::client_message::Payload as ClientPayload;
use crate::proto::session::hud_session_server::HudSession;
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::{SESSION_EVENT_CHANNEL_CAPACITY, SharedState};
use crate::token::DEFAULT_GRACE_PERIOD_MS;
use std::sync::Arc;
// Duration and Instant are used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tze_hud_resource::{
    ResourceError as StoreResourceError, ResourceStored as StoreResourceStored,
};
use tze_hud_scene::types::*;

// ─── Submodules (SS-1..SS-7h) ────────────────────────────────────────────────

pub mod budget_gate;
pub mod degradation_notice_bus;
mod element_persist;
pub mod freeze_queue;
pub mod handshake;
pub mod input_event_bus;
pub mod lease_expiry_bus;
pub mod lifecycle;
pub mod mutations;
pub mod service;
pub mod stream_session;
pub mod traffic;
pub mod upload;
pub mod verbs;

pub use budget_gate::{
    MutationBudgetDecision, MutationBudgetEnforcer, MutationBudgetUsage,
    SharedMutationBudgetEnforcer,
};
pub use degradation_notice_bus::{DegradationNoticeReceiver, DegradationNoticeSender};
// FreezeEnqueueResult, FREEZE_QUEUE_CAPACITY, and SessionFreezeQueue are used
// transitively in `mod tests { use super::* }`.
use element_persist::{
    ElementStorePersistRequest, persist_created_tile_entries, persist_element_store,
    touch_element_store_entry_by_namespace,
};
#[allow(unused_imports)]
use freeze_queue::{FREEZE_QUEUE_CAPACITY, FreezeEnqueueResult, SessionFreezeQueue};
use handshake::{HandshakeCtx, handle_handshake_read};
#[cfg(test)]
use handshake::{handle_session_init, handle_session_resume};
pub use input_event_bus::{InputEventReceiver, InputEventRecvError, InputEventSender};
pub use lease_expiry_bus::{LeaseExpiryNotice, LeaseExpiryReceiver, LeaseExpirySender};
pub use lifecycle::SessionState;
use mutations::{apply_queued_batch_to_scene, handle_mutation_batch};
pub use service::{HudSessionImpl, SessionDeps};
use stream_session::StreamSession;
pub use traffic::TrafficClass;
use upload::{UploadWorkerCommand, UploadWorkerEvent, run_upload_worker};
use verbs::{handle_claim_tile, handle_clear, handle_hold, handle_publish};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Default heartbeat interval in milliseconds.
pub(super) const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 5000;

/// Default heartbeat missed threshold (number of missed heartbeats before disconnect).
const HEARTBEAT_MISSED_THRESHOLD: u64 = 3;

/// Default heartbeat timeout: threshold * interval.
const DEFAULT_HEARTBEAT_TIMEOUT_MS: u64 =
    DEFAULT_HEARTBEAT_INTERVAL_MS * HEARTBEAT_MISSED_THRESHOLD;

/// Default maximum sequence gap before SEQUENCE_GAP_EXCEEDED (RFC 0005 §2.3).
const DEFAULT_MAX_SEQUENCE_GAP: u64 = 100;

// ─── Helper ─────────────────────────────────────────────────────────────────

/// Process-start instant used as the base for monotonic timestamps.
///
/// Initialized on first access. All `_mono_us` timestamps are microseconds
/// elapsed since this point, giving true monotonic semantics independent of
/// wall-clock adjustments.
static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Returns the process-start `Instant`, initializing it on first call.
fn process_start() -> std::time::Instant {
    *PROCESS_START.get_or_init(std::time::Instant::now)
}

/// Returns monotonic microseconds elapsed since process start.
///
/// Uses `std::time::Instant` so the value is immune to wall-clock adjustments
/// (NTP steps, leap seconds, user clock changes). Suitable for `_mono_us` fields.
fn now_mono_us() -> u64 {
    process_start().elapsed().as_micros() as u64
}

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

pub(super) fn scene_id_to_bytes(id: tze_hud_scene::SceneId) -> Vec<u8> {
    id.as_uuid().as_bytes().to_vec()
}

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

/// Broadcast channel capacity for transactional server-push messages.
///
/// Runtime-injected input events use this channel as well as degradation
/// notices. Keep enough headroom for short key/pointer bursts while
/// a session handler is also processing mutation responses.
const BROADCAST_CHANNEL_CAPACITY: usize = 1024;

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
        // Extract peer address BEFORE consuming the request via into_inner().
        // This is needed for LocalSocketCredential loopback gating (hud-1aswu.1).
        let peer_ip: Option<std::net::IpAddr> = request.remote_addr().map(|addr| addr.ip());

        let mut inbound = request.into_inner();
        let state = self.state.clone();
        let agents = self.agents.clone();
        let resource_budget = self.resource_budget.clone();
        let budget_enforcer = self.budget_enforcer.clone();
        let render_wake = self.render_wake.clone();
        let degradation_notices = self.degradation_notices.clone();
        // Runtime → session feeds, subscribed before the handler task starts
        // so a terminal lease transition cannot race the session's setup.
        let mut feeds = self.subscribe_feeds();

        // Create outbound channel
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(
            SESSION_EVENT_CHANNEL_CAPACITY,
        );

        // Spawn the session handler task
        tokio::spawn(async move {
            // Wait for the first message (must be SessionInit or SessionResume)
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
                resource_budget: &resource_budget,
                budget_enforcer: budget_enforcer.as_ref(),
                peer_ip,
            };
            let mut session = handle_handshake_read(handshake_ctx, &tx, first_read).await;

            let Some(ref mut session) = session else {
                return; // Handshake failed, error already sent
            };

            if session.state == SessionState::Resuming {
                render_wake.notify();
            }

            // The handshake registered this session; from here on, every exit
            // (early `break 'active`, panic, task cancellation) must unregister
            // it. Normal exits run the full cleanup below and disarm the guard.
            let mut registry_guard = RegistryGuard::new(state.clone(), session.session_id.clone());

            // Everything between registration and cleanup runs inside this
            // labeled block so no early exit can skip the cleanup below.
            let mut upload_worker = None;
            'active: {
                // Transition: Handshaking/Resuming → Active (RFC 0005 §1.1)
                session.transition(SessionState::Active);

                // Safe mode reaches this stream through the registry: it needs
                // the outbound sender to send `SessionSuspended`/`SessionResumed`.
                // `remove_session` at cleanup drops it.
                state
                    .lock()
                    .await
                    .sessions
                    .register_server_message_tx(&session.session_id, tx.clone());

                // Register the durable input lane only after the session has an
                // authenticated namespace. This prevents unrelated or incomplete
                // sessions from accumulating transactional input for other agents.
                let mut input_event_rx = feeds.input_events.subscribe(session.namespace.clone());

                // Send SceneSnapshot after successful handshake (RFC 0005 §1.3, §6.4)
                {
                    let st = state.lock().await;
                    let wall_us = now_wall_us();
                    let mono_us: u64 = now_mono_us();
                    let (snap_json, checksum, sequence_number) = {
                        let scene = st.scene.lock().await;
                        let graph_snap = scene.take_snapshot(wall_us, mono_us);
                        let snap_json = graph_snap
                            .to_json()
                            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"));
                        let checksum = graph_snap.checksum.clone();
                        let sequence_number = scene.sequence_number;
                        (snap_json, checksum, sequence_number)
                    };
                    let seq = session.next_server_seq();
                    drop(st);
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::SceneSnapshot(SceneSnapshot {
                                snapshot_json: snap_json,
                                sequence: sequence_number,
                                snapshot_wall_us: wall_us,
                                snapshot_mono_us: mono_us,
                                blake3_checksum: checksum,
                            })),
                        }))
                        .await;
                }

                // Atomically subscribe after the coherent scene snapshot, then send
                // the captured current policy before any later transition or
                // incremental event. RFC 0005 requires this for both new sessions
                // and resumes, including when the current level is Normal.
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
                upload_worker = Some(tokio::spawn(run_upload_worker(
                    state.clone(),
                    session.namespace.clone(),
                    upload_command_rx,
                    upload_event_tx,
                    upload_rate_limit_bytes_per_sec,
                    render_wake.clone(),
                )));

                // Main message loop
                //
                // The loop exits for one of three reasons:
                //   1. Stream EOF (graceful): agent closed the stream.
                //   2. Stream error: transport-level error.
                //   3. Heartbeat timeout: no message for heartbeat_missed_threshold × interval.
                //
                // In cases (2) and (3) the disconnect is ungraceful; leases become orphaned.
                // In case (1) the disconnect may be graceful (SessionClose was sent) or
                // ungraceful (agent dropped the connection without sending SessionClose).
                //
                // The loop also listens on `degradation_rx` for transactional DegradationNotice
                // broadcasts (RFC 0005 §3.4). These are delivered unconditionally to all active
                // sessions regardless of subscription config and are never dropped.
                loop {
                    // Use heartbeat timeout for receive (RFC 0005 §1.6, §3.6)
                    let timeout_duration =
                        tokio::time::Duration::from_millis(DEFAULT_HEARTBEAT_TIMEOUT_MS);

                    // ── Unfreeze drain: apply queued mutations if freeze just cleared ──
                    // When the shell sets SharedState.freeze_active = false, queued
                    // mutations are applied at the start of the next loop iteration
                    // so they are delivered in the next available frame batch
                    // (system-shell/spec.md §Freeze Scene: "Unfreeze applies queued
                    //  mutations in submission order in the next available frame batch").
                    //
                    // IMPORTANT: Use `apply_queued_batch_to_scene` (not
                    // `handle_mutation_batch`) here. Each queued batch has already
                    // received an immediate `MutationResult(accepted=true)` when it
                    // was enqueued. Re-using `handle_mutation_batch` would send a
                    // second result for the same batch_id, violating RFC 0005 §2.1.
                    {
                        let freeze_active = state.lock().await.freeze_active;
                        if !freeze_active && !session.freeze_queue.is_empty() {
                            let queued = session.freeze_queue.drain();
                            let mut applied_render_work = false;
                            for queued_batch in queued {
                                applied_render_work |=
                                    apply_queued_batch_to_scene(&state, session, queued_batch)
                                        .await;
                            }
                            if applied_render_work {
                                render_wake.notify();
                            }
                        }
                    }

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

                        // ── DegradationNotice broadcast (RFC 0005 §3.4, §7.1) ────
                        //
                        // Transactional — delivered unconditionally to all active sessions
                        // regardless of subscription config. Never dropped.
                        degradation_notice = degradation_rx.recv() => {
                            if let LoopAction::Break = session.on_degradation(degradation_notice, &tx).await {
                                break;
                            }
                        }

                        // ── Terminal lease transition from the compositor ─────────
                        //
                        // `SceneGraph::expire_leases()` owns the transition and
                        // resource cleanup. This per-session durable lane owns the
                        // corresponding wire notification, filtered by lease id.
                        lease_expiry = feeds.lease_expiry.recv() => {
                            if let LoopAction::Break = session.on_lease_expiry(lease_expiry, &tx).await {
                                break;
                            }
                        }

                        // ── Runtime-injected input EventBatch (hud-i6yd.6) ───────────
                        //
                        // The compositor input pipeline (Stage 2) assembles ClickEvent /
                        // CommandInputEvent batches for the owning agent and injects them
                        // through `HudSessionImpl::input_event_tx`. Only batches
                        // addressed to this session's namespace are forwarded; others are
                        // silently discarded.
                        //
                        // Delivery is gated on subscription: the batch is filtered through
                        // `subscriptions::filter_event_batch` before sending. If the agent
                        // is not subscribed to INPUT_EVENTS / FOCUS_EVENTS the batch is
                        // dropped silently (no error response).
                        input_event_result = input_event_rx.recv() => {
                            if let LoopAction::Break = session.on_input_event(input_event_result, &tx).await {
                                break;
                            }
                        }

                        // ── ElementRepositionedEvent broadcast (hud-bs2q.6) ──────────
                        //
                        // Emitted after drag completion or reset-to-default. Delivered to
                        // agents subscribed to SCENE_TOPOLOGY (requires read_scene_topology).
                        // Transactional — never coalesced or dropped. Agent cannot reject.
                        element_repositioned_result = feeds.element_repositioned.recv() => {
                            if let LoopAction::Break = session.on_element_repositioned(element_repositioned_result, &tx).await {
                                break;
                            }
                        }

                        // ── FramePresented broadcast (hud-91uu6) ─────────────────────
                        //
                        // Batch-correlated present acknowledgment: pairs the accepted
                        // MutationBatch.batch_ids composited into a presented frame with
                        // that frame's present wall-clock. Delivered to agents subscribed
                        // to TELEMETRY_FRAMES (requires read_telemetry). State-stream —
                        // coalesced/droppable under backpressure. Agent cannot reject.
                        frame_presented_result = feeds.frame_presented.recv() => {
                            if let LoopAction::Break = session.on_frame_presented(
                                frame_presented_result,
                                &degradation_notices,
                                &tx,
                            ).await {
                                break;
                            }
                        }
                    }
                }
            }

            // The command sender and event receiver were dropped with the active
            // loop. Join outside all shared-state locks; the closed event lane
            // wakes blocked replies/backpressure and the worker aborts only its
            // owned pending IDs before the cleanup witness can complete.
            if let Some(worker) = upload_worker {
                if let Err(error) = worker.await {
                    tracing::error!(%error, "resource upload worker failed during session cleanup");
                }
            }

            // Cleanup: disconnect is not release (invariant 4).
            //
            // The session's active leases become ORPHANED (badge shown, content
            // kept) and the resume token is saved so the agent can reconnect
            // within the grace period using SessionResume. When the grace period
            // ends, the compositor's `expire_leases()` sweep reclaims the leases
            // and their content with no agent help. Token and lease grace are
            // measured on the scene clock so they expire together. Tokens are
            // not persisted across process restarts.
            {
                let mut st = state.lock().await;
                st.sessions.remove_session(&session.session_id);
                registry_guard.disarm();

                // Only sessions that completed the handshake get a grace period.
                if !session.resume_token.is_empty() {
                    let now = {
                        let mut scene = st.scene.lock().await;
                        let now = scene.now_millis();
                        for lease_id in &session.lease_ids {
                            let active = scene
                                .leases
                                .get(lease_id)
                                .is_some_and(|l| l.state == LeaseState::Active);
                            if active {
                                let _ = scene.disconnect_lease(lease_id, now);
                            }
                        }
                        scene.orphan_publications(
                            session.publication_origin,
                            now,
                            DEFAULT_GRACE_PERIOD_MS,
                        );
                        now
                    };
                    st.token_store.insert_with_publication_origin(
                        session.resume_token.clone(),
                        session.agent_name.clone(),
                        session.capabilities.clone(),
                        session.subscriptions.clone(),
                        session.lease_ids.clone(),
                        DEFAULT_GRACE_PERIOD_MS,
                        now,
                        Some(session.publication_origin),
                    );
                }
            }
            render_wake.notify();
            if let Some(enforcer) = &session.budget_enforcer {
                enforcer.remove_session(session.scene_session_id);
            }
            #[cfg(test)]
            state
                .lock()
                .await
                .sessions
                .finish_cleanup(&session.scene_session_id);
        });

        // Return the receiver stream as the response
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Unregisters a session (and its outbound sender) if the handler task dies
/// before the normal cleanup runs, e.g. on panic or runtime shutdown. Normal
/// exits call [`RegistryGuard::disarm`] after removing the entry themselves.
/// Removal only: lease orphaning stays in the normal cleanup path.
struct RegistryGuard {
    state: Arc<Mutex<SharedState>>,
    session_id: String,
    armed: bool,
}

impl RegistryGuard {
    fn new(state: Arc<Mutex<SharedState>>, session_id: String) -> Self {
        Self {
            state,
            session_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The registry lock is async; hand removal to the runtime if one is
        // still alive (if it is not, the whole registry is going away anyway).
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.state.clone();
            let session_id = std::mem::take(&mut self.session_id);
            runtime.spawn(async move {
                state.lock().await.sessions.remove_session(&session_id);
            });
        }
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
        return;
    };

    match payload {
        ClientPayload::MutationBatch(batch) => {
            handle_mutation_batch(state, session, tx, client_sequence, batch, render_wake).await;
        }
        ClientPayload::ClaimTile(claim) => {
            handle_claim_tile(state, session, tx, client_sequence, claim, render_wake).await;
        }
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
        // SessionInit/SessionResume should not appear after handshake
        ClientPayload::SessionInit(_) | ClientPayload::SessionResume(_) => {
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
                // Retransmit fast-path (RFC 0005 §5.3).
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
                    Some(ClientPayload::ClaimTile(_))
                        | Some(ClientPayload::Hold(_))
                        | Some(ClientPayload::Clear(_))
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

                // Validate client sequence number (RFC 0005 §2.3).
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
                                            "Open a new stream with SessionInit or SessionResume; send increasing sequence numbers starting at 2 with gaps no larger than {DEFAULT_MAX_SEQUENCE_GAP}"
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
                // Heartbeat timeout (RFC 0005 §1.6, §3.6)
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

    /// Handle a `DegradationNotice` broadcast result (RFC 0005 §3.4, §7.1).
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

    /// Deliver one terminal scene-lease transition to its owning session.
    ///
    /// The compositor has already applied cleanup by the time it publishes the
    /// notice. Removing the id before sending makes duplicate runtime notices
    /// harmless and guarantees at most one terminal response for a
    /// connected session.
    async fn on_lease_expiry(
        &mut self,
        lease_expiry: Option<LeaseExpiryNotice>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        let Some(notice) = lease_expiry else {
            self.transition(SessionState::Closed);
            return LoopAction::Break;
        };
        if !self.lease_ids.contains(&notice.lease_id) {
            return LoopAction::Continue;
        }

        self.lease_ids
            .retain(|lease_id| *lease_id != notice.lease_id);
        let lease_id = scene_id_to_bytes(notice.lease_id);
        let why = match notice.terminal_state {
            LeaseState::Expired => ReclaimReason::Expired,
            _ => ReclaimReason::Override,
        } as i32;
        let surfaces: Vec<String> = if notice.removed_tiles.is_empty() {
            vec![String::new()]
        } else {
            notice
                .removed_tiles
                .iter()
                .map(|t| verbs::tile_surface(*t))
                .collect()
        };
        for surface in surfaces {
            let seq = self.next_server_seq();
            if tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::Reclaimed(Reclaimed {
                        surface,
                        why,
                        lease_id: lease_id.clone(),
                    })),
                }))
                .await
                .is_err()
            {
                self.transition(SessionState::Closed);
                return LoopAction::Break;
            }
        }

        LoopAction::Continue
    }

    /// Handle a runtime-injected input `EventBatch` broadcast result (hud-i6yd.6).
    ///
    /// The compositor input pipeline assembles ClickEvent / CommandInputEvent batches
    /// for the owning agent. Only batches addressed to this session's namespace are
    /// forwarded; others are silently discarded. Delivery is gated on subscription.
    async fn on_input_event(
        &mut self,
        input_event_result: Result<(String, crate::proto::EventBatch), InputEventRecvError>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match input_event_result {
            Ok((target_namespace, batch)) => {
                // Namespace filter: only deliver to the owning session.
                if target_namespace == self.namespace {
                    // Subscription filter: gate on INPUT_EVENTS / FOCUS_EVENTS.
                    if let Some(filtered) =
                        crate::subscriptions::filter_event_batch(batch, &self.subscriptions)
                    {
                        let seq = self.next_server_seq();
                        let _ = tx
                            .send(Ok(ServerMessage {
                                sequence: seq,
                                timestamp_wall_us: now_wall_us(),
                                payload: Some(ServerPayload::EventBatch(filtered)),
                            }))
                            .await;
                    }
                }
                LoopAction::Continue
            }
            Err(InputEventRecvError::Lagged(_)) => {
                // Only ephemeral/state-stream input uses the bounded lane.
                LoopAction::Continue
            }
            Err(InputEventRecvError::Closed) => {
                // Runtime shutting down — treat as ungraceful disconnect.
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }

    /// Handle an `ElementRepositionedEvent` broadcast result (hud-bs2q.6).
    ///
    /// Emitted after drag completion or reset-to-default. Delivered to
    /// agents subscribed to SCENE_TOPOLOGY (requires read_scene_topology).
    /// Transactional — never coalesced or dropped. Agent cannot reject.
    async fn on_element_repositioned(
        &mut self,
        element_repositioned_result: Result<
            crate::proto::ElementRepositionedEvent,
            tokio::sync::broadcast::error::RecvError,
        >,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match element_repositioned_result {
            Ok(event) => {
                // Gate on SCENE_TOPOLOGY subscription.
                if self
                    .subscriptions
                    .contains(&crate::subscriptions::category::SCENE_TOPOLOGY.to_string())
                {
                    let seq = self.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::ElementRepositioned(event)),
                        }))
                        .await;
                }
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                // Missed notifications. Log and continue — the element
                // store state is persistent so a future snapshot or
                // The element store will reflect the current position.
                let _ = n; // suppress unused warning; production: tracing::warn!
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                // Runtime shutting down — treat as ungraceful disconnect.
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }

    /// Handle a `FramePresented` broadcast result (hud-91uu6).
    ///
    /// Batch-correlated present acknowledgment. Delivered to agents subscribed to
    /// TELEMETRY_FRAMES (requires the read_telemetry capability, enforced at
    /// subscribe time — so checking the active subscription here is sufficient).
    /// State-stream class:
    /// coalesced/droppable under backpressure. Agent cannot reject.
    async fn on_frame_presented(
        &mut self,
        frame_presented_result: Result<
            crate::proto::FramePresented,
            tokio::sync::broadcast::error::RecvError,
        >,
        degradation_notices: &DegradationNoticeSender,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match frame_presented_result {
            Ok(event) => {
                // Gate on TELEMETRY_FRAMES subscription (read_telemetry capability
                // was already enforced when the subscription was granted).
                if degradation_notices.should_emit_state_stream(event.frame_number)
                    && self
                        .subscriptions
                        .contains(&crate::subscriptions::category::TELEMETRY_FRAMES.to_string())
                {
                    let seq = self.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::FramePresented(event)),
                        }))
                        .await;
                }
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Missed present acks under backpressure. State-stream class —
                // droppable; the latency probe samples, so a gap is acceptable.
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                // Runtime shutting down — treat as ungraceful disconnect.
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }
}

/// Maximum future schedule horizon in microseconds (RFC 0003 §3.5, default 5 minutes).
pub(super) const DEFAULT_MAX_FUTURE_SCHEDULE_US: u64 = 300_000_000;

/// Validate TimingHints for a MutationBatch (RFC 0003 §3.5, RFC 0005 §3.3).
///
/// Returns `Ok(())` if valid, or `Err((error_code, message))` for each
/// invalid condition.
///
/// Validation rules:
/// - `present_at_wall_us < session_open_at_wall_us - 60_000_000` → TIMESTAMP_TOO_OLD
/// - `present_at_wall_us > current_wall_us + max_future_schedule_us` → TIMESTAMP_TOO_FUTURE
/// - `expires_at_wall_us > 0 && expires_at_wall_us <= present_at_wall_us` → TIMESTAMP_EXPIRY_BEFORE_PRESENT
///
/// A value of 0 in either field means "no constraint".
/// `now` comes from the same injected scene wall clock as the session stamp.
pub(super) fn validate_timing_hints(
    hints: &TimingHints,
    session_open_at_wall_us: u64,
    max_future_schedule_us: u64,
    now: u64,
) -> Result<(), (&'static str, String)> {
    let present = hints.present_at_wall_us;
    let expires = hints.expires_at_wall_us;

    if present > 0 {
        // TIMESTAMP_TOO_OLD: present_at_wall_us more than 60 seconds before session open
        // (RFC 0003 §3.5; 60s = 60_000_000 µs)
        let too_old_threshold = session_open_at_wall_us.saturating_sub(60_000_000);
        if present < too_old_threshold {
            return Err((
                "TIMESTAMP_TOO_OLD",
                format!(
                    "present_at_wall_us ({present}) is more than 60s before session open \
                     ({session_open_at_wall_us})"
                ),
            ));
        }

        // TIMESTAMP_TOO_FUTURE: present_at_wall_us exceeds max_future_schedule_us horizon
        if present > now.saturating_add(max_future_schedule_us) {
            return Err((
                "TIMESTAMP_TOO_FUTURE",
                format!(
                    "present_at_wall_us ({present}) exceeds max future schedule \
                     ({max_future_schedule_us} µs from now={now})"
                ),
            ));
        }

        // TIMESTAMP_EXPIRY_BEFORE_PRESENT: non-zero expiry at or before present
        if expires > 0 && expires <= present {
            return Err((
                "TIMESTAMP_EXPIRY_BEFORE_PRESENT",
                format!(
                    "expires_at_wall_us ({expires}) must be strictly after \
                     present_at_wall_us ({present})"
                ),
            ));
        }
    }

    Ok(())
}

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
        StoreResourceError::InvalidChunk(detail)
            if detail.contains("unknown upload_id")
                || detail.contains("not in-flight")
                || detail.contains("no uploads in flight") =>
        {
            9
        }
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
            "Ask the operator to grant upload_resource in the allow entries, then reconnect with a fresh SessionInit or valid SessionResume, then retry ResourceUploadStart"
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
        StoreResourceError::InvalidChunk(detail)
            if detail.contains("unknown upload_id")
                || detail.contains("not in-flight")
                || detail.contains("no uploads in flight") =>
        {
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
