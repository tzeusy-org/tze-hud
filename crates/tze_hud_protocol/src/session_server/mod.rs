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

use crate::auth::CapabilityPolicy;
use crate::convert;
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
use crate::proto::{ElementInfo, ListElementsRequest, ListElementsResponse};
use crate::session::{SESSION_EVENT_CHANNEL_CAPACITY, SharedState};
use crate::subscriptions;
use crate::token::DEFAULT_GRACE_PERIOD_MS;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashMap;
use std::sync::Arc;
// Duration and Instant are used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use tze_hud_resource::{
    ResourceError as StoreResourceError, ResourceStored as StoreResourceStored,
    RuntimeWidgetStoreError, RuntimeWidgetStorePutOutcome as DurablePutOutcome,
};
use tze_hud_scene::element_store::{ElementStore, ElementStoreEntry, ElementType};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::types::*;
use tze_hud_widget::{RuntimeWidgetAssetError, register_runtime_widget_svg_asset};

// ─── Submodules (SS-1..SS-7h) ────────────────────────────────────────────────

pub mod budget_gate;
pub mod config;
pub mod degradation_notice_bus;
pub mod emit_scene_event;
pub mod freeze_queue;
pub mod handshake;
pub mod input;
pub mod input_event_bus;
pub mod lease_expiry_bus;
pub mod leases;
pub mod lifecycle;
pub mod mutations;
pub mod service;
pub mod stream_session;
pub mod subscriptions_cap;
pub mod traffic;
pub mod upload;
pub mod widgets;
pub mod zone_publish;

pub use budget_gate::{
    MutationBudgetDecision, MutationBudgetEnforcer, MutationBudgetUsage,
    SharedMutationBudgetEnforcer,
};
pub use config::SessionConfig;
pub use degradation_notice_bus::{DegradationNoticeReceiver, DegradationNoticeSender};
// FreezeEnqueueResult, FREEZE_QUEUE_CAPACITY, and SessionFreezeQueue are used
// transitively in `mod tests { use super::* }`.
use emit_scene_event::handle_emit_scene_event;
#[allow(unused_imports)]
use freeze_queue::{FREEZE_QUEUE_CAPACITY, FreezeEnqueueResult, SessionFreezeQueue};
use handshake::{handle_session_init, handle_session_resume};
use input::{
    handle_input_capture_release, handle_input_capture_request, handle_input_focus_request,
};
// scene_node_contains is used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use input::scene_node_contains;
pub use input_event_bus::{InputEventReceiver, InputEventRecvError, InputEventSender};
pub use lease_expiry_bus::{LeaseExpiryNotice, LeaseExpiryReceiver, LeaseExpirySender};
use leases::{handle_lease_release, handle_lease_renew, handle_lease_request};
pub use lifecycle::SessionState;
use mutations::{apply_queued_batch_to_scene, handle_mutation_batch};
pub use service::HudSessionImpl;
pub use stream_session::CapabilityRevocationEvent;
use stream_session::StreamSession;
use subscriptions_cap::{
    handle_capability_request, handle_capability_revocation, handle_list_elements_request,
    handle_subscription_change,
};
pub use traffic::{TrafficClass, classify_server_payload};
use upload::{UploadWorkerCommand, UploadWorkerEvent, run_upload_worker};
use widgets::{handle_widget_asset_register, handle_widget_publish};
use zone_publish::handle_zone_publish;
// UploadByteRateLimiter is used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use upload::UploadByteRateLimiter;

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

fn lease_state_wire_name(state: LeaseState) -> &'static str {
    match state {
        LeaseState::Requested => "REQUESTED",
        LeaseState::Active => "ACTIVE",
        LeaseState::Suspended => "SUSPENDED",
        LeaseState::Orphaned => "ORPHANED",
        LeaseState::Denied => "DENIED",
        LeaseState::Revoked => "REVOKED",
        LeaseState::Expired => "EXPIRED",
        LeaseState::Released => "RELEASED",
    }
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

/// Captures the data needed to persist the element store outside the shared-state lock.
pub(super) struct ElementStorePersistRequest {
    store: ElementStore,
    path: std::path::PathBuf,
}

/// Update tile entries in the element store and return an optional persistence request.
pub(super) async fn persist_created_tile_entries(
    st: &mut SharedState,
    created_ids: &[SceneId],
) -> Option<ElementStorePersistRequest> {
    if created_ids.is_empty() {
        return None;
    }

    // `(id, namespace, z_order)` for each just-created tile, plus the ids of
    // every tile currently live in the scene (needed to tell a recreated portal
    // member's orphaned entry from a still-live sibling's — hud-08nls).
    let (created_tiles, live_ids): (
        Vec<(SceneId, String, u32)>,
        std::collections::HashSet<SceneId>,
    ) = {
        let scene = st.scene.lock().await;
        let created = created_ids
            .iter()
            .filter_map(|id| {
                scene
                    .tiles
                    .get(id)
                    .map(|tile| (*id, tile.namespace.clone(), tile.z_order))
            })
            .collect();
        let live = scene.tiles.keys().copied().collect();
        (created, live)
    };

    if created_tiles.is_empty() {
        return None;
    }

    let now = now_ms();
    let mut changed = false;
    let recreated: Vec<tze_hud_scene::element_store::RecreatedTile> = created_tiles
        .iter()
        .map(
            |(id, namespace, z_order)| tze_hud_scene::element_store::RecreatedTile {
                id: *id,
                namespace: namespace.clone(),
                z_order: *z_order,
            },
        )
        .collect();
    for (id, namespace, z_order) in created_tiles {
        match st.element_store.entries.get_mut(&id) {
            Some(entry) => {
                if entry.element_type != ElementType::Tile {
                    entry.element_type = ElementType::Tile;
                    changed = true;
                }
                if entry.namespace != namespace {
                    entry.namespace = namespace;
                    changed = true;
                }
                if entry.z_order != z_order {
                    entry.z_order = z_order;
                    changed = true;
                }
                if entry.created_at == 0 {
                    entry.created_at = now;
                    changed = true;
                }
                if entry.last_published_at != now {
                    entry.last_published_at = now;
                    changed = true;
                }
                // A just-published tile is live, so it starts a fresh retention
                // window; clear any accumulated unseen-restart count (hud-fwgv7).
                if entry.unseen_restarts != 0 {
                    entry.unseen_restarts = 0;
                    changed = true;
                }
                if entry.geometry_override.is_some() {
                    entry.geometry_override = None;
                    changed = true;
                }
            }
            None => {
                st.element_store.entries.insert(
                    id,
                    ElementStoreEntry {
                        element_type: ElementType::Tile,
                        namespace,
                        created_at: now,
                        last_published_at: now,
                        z_order,
                        unseen_restarts: 0,
                        geometry_override: None,
                    },
                );
                changed = true;
            }
        }
    }

    // Re-home any durable override whose portal member tile was recreated with a
    // fresh SceneId (the entries were just inserted above with no override; a
    // matching orphan hands its override over here). Re-lock viewer geometry for
    // each adopter so a subsequent adapter `UpdateTileBounds` republish cannot
    // reposition it before the viewer touches it again (mirrors the bootstrap
    // re-lock in `tze_hud_runtime::element_store`).
    let adopted = st
        .element_store
        .adopt_orphaned_tile_overrides(&recreated, &live_ids);
    if !adopted.is_empty() {
        changed = true;
        let mut scene = st.scene.lock().await;
        for id in &adopted {
            scene.lock_viewer_geometry(*id);
        }
    }

    if !changed {
        return None;
    }

    st.element_store_path
        .clone()
        .map(|path| ElementStorePersistRequest {
            store: st.element_store.clone(),
            path,
        })
}

/// Serialize and atomically write an [`ElementStore`] to disk.
///
/// This is the protocol-layer counterpart of
/// `tze_hud_runtime::element_store::persist_element_store_to_path`.  It is
/// intentionally a local copy so that `tze_hud_protocol` does not need to
/// depend on `tze_hud_runtime` (which would create a circular dependency since
/// `tze_hud_runtime` already depends on `tze_hud_protocol`).
fn write_element_store_to_path(
    store: &ElementStore,
    path: &std::path::Path,
) -> std::io::Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    let toml_text = toml::to_string_pretty(store).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to serialize element_store TOML: {err}"),
        )
    })?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("element_store.toml");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(
        ".{stem}.tmp.{}.{}.{}",
        std::process::id(),
        now_ns,
        tze_hud_scene::types::SceneId::new()
    ));

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)?;
    file.write_all(toml_text.as_bytes())?;
    file.sync_all()?;
    drop(file);

    if let Err(err) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    // On Unix, sync the parent directory so the rename is durable.
    // On Windows, the rename itself is sufficient.
    #[cfg(not(target_os = "windows"))]
    {
        OpenOptions::new().read(true).open(parent)?.sync_all()?;
    }

    Ok(())
}

/// Persist the element store without blocking the async executor worker thread.
pub(super) async fn persist_element_store(request: Option<ElementStorePersistRequest>) {
    let Some(request) = request else {
        return;
    };

    let path_for_log = request.path.clone();
    match tokio::task::spawn_blocking(move || {
        write_element_store_to_path(&request.store, &request.path)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %err,
                "element_store: failed to persist tile IDs"
            );
        }
        Err(err) => {
            tracing::warn!(
                path = %path_for_log.display(),
                error = %err,
                "element_store: failed to join tile ID persistence task"
            );
        }
    }
}

pub(super) fn touch_element_store_entry_by_id(
    st: &mut SharedState,
    element_id: SceneId,
    element_type: ElementType,
    now: u64,
) -> Option<ElementStorePersistRequest> {
    let entry = st.element_store.entries.get_mut(&element_id)?;
    if entry.element_type != element_type {
        return None;
    }
    entry.last_published_at = now;
    st.element_store_path
        .clone()
        .map(|path| ElementStorePersistRequest {
            store: st.element_store.clone(),
            path,
        })
}

pub(super) fn touch_element_store_entry_by_namespace(
    st: &mut SharedState,
    element_type: ElementType,
    namespace: &str,
    now: u64,
) -> Option<ElementStorePersistRequest> {
    let id = st
        .element_store
        .find_id_by_type_namespace(element_type, namespace)?;
    touch_element_store_entry_by_id(st, id, element_type, now)
}

// ─── Shared agent event emission types ───────────────────────────────────────

// MAX_PAYLOAD_BYTES and DEFAULT_MAX_EVENTS_PER_SECOND live in
// tze_hud_scene::events::emission and are shared with tze_hud_runtime.
use tze_hud_scene::events::emission::{DEFAULT_MAX_EVENTS_PER_SECOND, MAX_PAYLOAD_BYTES};
// AgentEventRateLimiter is used transitively in `mod tests { use super::* }`.
#[allow(unused_imports)]
use tze_hud_scene::events::emission::AgentEventRateLimiter;

/// Broadcast channel capacity for transactional server-push messages.
///
/// Runtime-injected input events use this channel as well as degradation and
/// revocation notices. Keep enough headroom for short key/pointer bursts while
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
        let psk = self.psk.clone();
        // Clone the capability registry for use inside the session task.
        let agent_capabilities = self.agent_capabilities.clone();
        let agent_resource_budgets = self.agent_resource_budgets.clone();
        let fallback_resource_budget = self.fallback_resource_budget.clone();
        let budget_enforcer = self.budget_enforcer.clone();
        let fallback_unrestricted = self.fallback_unrestricted;
        let render_wake = self.render_wake.clone();
        let degradation_notices = self.degradation_notices.clone();
        // This durable lane is subscribed before the handler task starts so a
        // terminal transition cannot race a newly-connected session's setup.
        let mut lease_expiry_rx = self.lease_expirations.subscribe();
        // Subscribe to the capability revocation broadcast channel.
        // Subscribing here ensures the session handler receives revocations issued
        // immediately after it is spawned (before the task subscribes itself).
        let mut capability_revocation_rx = self.capability_revocation_tx.subscribe();

        // Clone the input-event sender into the task. The durable subscription
        // is created only after authentication establishes the namespace.
        let input_event_tx = self.input_event_tx.clone();

        // Subscribe to the element-repositioned broadcast channel (hud-bs2q.6).
        // Delivery is gated on SCENE_TOPOLOGY subscription in the session loop.
        let mut element_repositioned_rx = self.element_repositioned_tx.subscribe();

        // Subscribe to the frame-presented broadcast channel (hud-91uu6).
        // Delivery is gated on TELEMETRY_FRAMES subscription in the session loop.
        let mut frame_presented_rx = self.frame_presented_tx.subscribe();

        // Create outbound channel
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ServerMessage, Status>>(
            SESSION_EVENT_CHANNEL_CAPACITY,
        );

        // Spawn the session handler task
        tokio::spawn(async move {
            // Wait for the first message (must be SessionInit or SessionResume)
            let first_msg = match tokio::time::timeout(
                tokio::time::Duration::from_millis(5000),
                inbound.message(),
            )
            .await
            {
                Ok(Ok(Some(msg))) => msg,
                Ok(Ok(None)) => {
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: 1,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::SessionError(SessionError {
                                code: "HANDSHAKE_TIMEOUT".to_string(),
                                message: "Stream closed before handshake".to_string(),
                                hint: String::new(),
                            })),
                        }))
                        .await;
                    return;
                }
                Ok(Err(e)) => {
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: 1,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::SessionError(SessionError {
                                code: "HANDSHAKE_ERROR".to_string(),
                                message: format!("Error receiving handshake: {e}"),
                                hint: String::new(),
                            })),
                        }))
                        .await;
                    return;
                }
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

            // Process handshake
            let mut session = match first_msg.payload {
                Some(ClientPayload::SessionInit(init)) => {
                    handle_session_init(
                        &state,
                        &psk,
                        &tx,
                        &init,
                        &agent_capabilities,
                        &agent_resource_budgets,
                        &fallback_resource_budget,
                        budget_enforcer.as_ref(),
                        fallback_unrestricted,
                        peer_ip,
                    )
                    .await
                }
                Some(ClientPayload::SessionResume(resume)) => {
                    handle_session_resume(
                        &state,
                        &psk,
                        &tx,
                        &resume,
                        &agent_capabilities,
                        &agent_resource_budgets,
                        &fallback_resource_budget,
                        budget_enforcer.as_ref(),
                        fallback_unrestricted,
                        peer_ip,
                    )
                    .await
                }
                _ => {
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: 1,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::SessionError(SessionError {
                                code: "INVALID_HANDSHAKE".to_string(),
                                message: "First message must be SessionInit or SessionResume"
                                    .to_string(),
                                hint: String::new(),
                            })),
                        }))
                        .await;
                    return;
                }
            };

            let Some(ref mut session) = session else {
                return; // Handshake failed, error already sent
            };

            // Transition: Handshaking/Resuming → Active (RFC 0005 §1.1)
            session.transition(SessionState::Active);

            // Register the durable input lane only after the session has an
            // authenticated namespace. This prevents unrelated or incomplete
            // sessions from accumulating transactional input for other agents.
            let mut input_event_rx = input_event_tx.subscribe(session.namespace.clone());

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
                return;
            }

            let upload_rate_limit_bytes_per_sec =
                session.resource_upload_rate_limiter.limit_bytes_per_second;
            let (upload_command_tx, upload_command_rx) =
                tokio::sync::mpsc::channel::<UploadWorkerCommand>(64);
            let (upload_event_tx, mut upload_event_rx) =
                tokio::sync::mpsc::channel::<UploadWorkerEvent>(64);
            tokio::spawn(run_upload_worker(
                state.clone(),
                session.namespace.clone(),
                upload_command_rx,
                upload_event_tx,
                upload_rate_limit_bytes_per_sec,
                render_wake.clone(),
            ));

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
                                apply_queued_batch_to_scene(&state, session, queued_batch).await;
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

                    // ── Capability revocation broadcast (RFC 0001 §3.3, GAP-G3-4) ────
                    //
                    // The runtime can narrow an active lease's capability scope without
                    // revoking the lease itself. The session handler applies the change
                    // to the scene graph and notifies the agent with CapabilityNotice
                    // + LeaseStateChange (both transactional — never dropped).
                    revocation_result = capability_revocation_rx.recv() => {
                        if let LoopAction::Break = session.on_capability_revocation(
                            revocation_result,
                            &state,
                            &tx,
                        ).await {
                            break;
                        }
                    }

                    // ── Terminal lease transition from the compositor ─────────
                    //
                    // `SceneGraph::expire_leases()` owns the transition and
                    // resource cleanup. This per-session durable lane owns the
                    // corresponding wire notification, filtered by lease id.
                    lease_expiry = lease_expiry_rx.recv() => {
                        if let LoopAction::Break = session.on_lease_expiry(lease_expiry, &tx).await {
                            break;
                        }
                    }

                    // ── Runtime-injected input EventBatch (hud-i6yd.6) ───────────
                    //
                    // The compositor input pipeline (Stage 2) assembles ClickEvent /
                    // CommandInputEvent batches for the owning agent and injects them
                    // here via `HudSessionImpl::inject_input_event`. Only batches
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
                    element_repositioned_result = element_repositioned_rx.recv() => {
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
                    frame_presented_result = frame_presented_rx.recv() => {
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

            // Cleanup: remove session from registry and store resume token.
            //
            // The resume token issued at handshake time is saved to the TokenStore so
            // the agent can reconnect within the grace period using SessionResume.
            // Token is not persisted across process restarts (RFC 0005 §6.6).
            let (resource_store, namespace_for_cleanup) = {
                let mut st = state.lock().await;
                st.sessions.remove_session(&session.session_id);

                // Only register a resume token if the session was ever Active
                // (i.e. handshake succeeded). Sessions that fail auth do not
                // get an orphaned-lease grace period.
                if !session.resume_token.is_empty() {
                    st.token_store.insert(
                        session.resume_token.clone(),
                        session.agent_name.clone(),
                        session.capabilities.clone(),
                        session.subscriptions.clone(),
                        session.lease_ids.clone(),
                        DEFAULT_GRACE_PERIOD_MS,
                        now_ms(),
                    );
                }
                (st.resource_store.clone(), session.namespace.clone())
            };
            resource_store
                .abort_all_uploads(&namespace_for_cleanup)
                .await;
            if let Some(enforcer) = &session.budget_enforcer {
                enforcer.remove_session(session.scene_session_id);
            }
        });

        // Return the receiver stream as the response
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
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
            handle_mutation_batch(state, session, tx, batch, render_wake).await;
        }
        ClientPayload::LeaseRequest(req) => {
            let _ =
                handle_lease_request(state, session, tx, client_sequence, req, render_wake).await;
        }
        ClientPayload::LeaseRenew(renew) => {
            let _ =
                handle_lease_renew(state, session, tx, client_sequence, renew, render_wake).await;
        }
        ClientPayload::LeaseRelease(release) => {
            let _ = handle_lease_release(state, session, tx, client_sequence, release, render_wake)
                .await;
        }
        ClientPayload::SubscriptionChange(change) => {
            handle_subscription_change(session, tx, change).await;
        }
        ClientPayload::ListElementsRequest(request) => {
            handle_list_elements_request(state, session, tx, request).await;
        }
        ClientPayload::ZonePublish(publish) => {
            let _ = handle_zone_publish(state, session, tx, client_sequence, publish, render_wake)
                .await;
        }
        ClientPayload::Heartbeat(hb) => {
            handle_heartbeat(session, tx, hb).await;
        }
        ClientPayload::TelemetryFrame(_tf) => {
            // Accept agent-side telemetry frames silently (logging/storage deferred to post-v1)
        }
        ClientPayload::InputFocusRequest(req) => {
            // Synchronous focus request (RFC 0005 §3.8).
            // v1 grants focus unconditionally (arbitration deferred to post-v1).
            handle_input_focus_request(session, tx, req).await;
        }
        ClientPayload::InputCaptureRequest(req) => {
            // Synchronous capture request (RFC 0005 §3.8).
            handle_input_capture_request(state, session, tx, req).await;
        }
        ClientPayload::InputCaptureRelease(rel) => {
            // Asynchronous capture release (RFC 0005 §3.8).
            // Confirmed by CaptureReleasedEvent in EventBatch (field 34).
            handle_input_capture_release(state, session, tx, rel).await;
        }
        ClientPayload::SetImePosition(_pos) => {
            // IME position hint (RFC 0005 §3.8): fire-and-forget, no response sent.
        }
        ClientPayload::SessionClose(close) => {
            // Graceful disconnect (RFC 0005 §1.5).
            // Record the expect_resume hint; the main loop transitions state after this returns.
            session.expect_resume = close.expect_resume;
        }
        ClientPayload::CapabilityRequest(req) => {
            handle_capability_request(session, tx, req).await;
        }
        // Agent scene event emission (scene-events/spec.md §5.1, §5.2).
        ClientPayload::EmitSceneEvent(emit) => {
            handle_emit_scene_event(state, session, tx, client_sequence, emit).await;
        }
        // Widget publishing (widget-system spec §Requirement: Widget Publishing via gRPC).
        // Durable-widget publishes receive WidgetPublishResult (ServerMessage field 47).
        // Ephemeral-widget publishes are fire-and-forget (no result).
        ClientPayload::WidgetPublish(publish) => {
            let _ =
                handle_widget_publish(state, session, tx, client_sequence, publish, render_wake)
                    .await;
        }
        // Widget asset register/upload (session-protocol spec §Requirement: Widget Asset Registration via Session Stream).
        // Always transactional; every request receives WidgetAssetRegisterResult.
        ClientPayload::WidgetAssetRegister(register) => {
            handle_widget_asset_register(
                state,
                session,
                tx,
                client_sequence,
                register,
                render_wake,
            )
            .await;
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
                // Update heartbeat timestamp on any received message
                self.last_heartbeat_ms = now_ms();

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
                    Some(ClientPayload::LeaseRequest(_))
                        | Some(ClientPayload::LeaseRenew(_))
                        | Some(ClientPayload::LeaseRelease(_))
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
                                        hint: String::new(),
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

    /// Handle a capability revocation broadcast result (RFC 0001 §3.3, GAP-G3-4).
    ///
    /// The runtime can narrow an active lease's capability scope without
    /// revoking the lease itself. The session handler applies the change
    /// to the scene graph and notifies the agent with CapabilityNotice
    /// + LeaseStateChange (both transactional — never dropped).
    async fn on_capability_revocation(
        &mut self,
        revocation_result: Result<
            CapabilityRevocationEvent,
            tokio::sync::broadcast::error::RecvError,
        >,
        state: &Arc<Mutex<SharedState>>,
        tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    ) -> LoopAction {
        match revocation_result {
            Ok(event) => {
                // Only this session's leases are affected.
                if self.lease_ids.contains(&event.lease_id) {
                    handle_capability_revocation(state, self, tx, event).await;
                }
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Missed revocation events. Log and continue; the capability
                // scope may be stale for those dropped events.
                // In production: emit a metric and re-query the live scope.
                LoopAction::Continue
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                // Runtime shutting down — treat as ungraceful disconnect.
                self.transition(SessionState::Closed);
                LoopAction::Break
            }
        }
    }

    /// Deliver one terminal scene-lease transition to its owning session.
    ///
    /// The compositor has already applied cleanup by the time it publishes the
    /// notice. Removing the id before sending makes duplicate runtime notices
    /// harmless and guarantees at most one terminal response/state pair for a
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
        let (result, reason, deny_code) = match notice.terminal_state {
            LeaseState::Expired => (
                LeaseResult::Expired as i32,
                "Lease TTL or reconnect grace period elapsed".to_string(),
                "LEASE_EXPIRED".to_string(),
            ),
            LeaseState::Revoked => (
                LeaseResult::Revoked as i32,
                "Lease suspension timeout elapsed".to_string(),
                "LEASE_REVOKED".to_string(),
            ),
            state => (
                LeaseResult::Unspecified as i32,
                format!(
                    "Lease reached terminal state {}",
                    lease_state_wire_name(state)
                ),
                "LEASE_TERMINAL".to_string(),
            ),
        };

        let response_seq = self.next_server_seq();
        if tx
            .send(Ok(ServerMessage {
                sequence: response_seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                    granted: false,
                    lease_id: lease_id.clone(),
                    deny_reason: reason.clone(),
                    deny_code,
                    result,
                    ..Default::default()
                })),
            }))
            .await
            .is_err()
        {
            self.transition(SessionState::Closed);
            return LoopAction::Break;
        }

        let state_change_seq = self.next_server_seq();
        if tx
            .send(Ok(ServerMessage {
                sequence: state_change_seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::LeaseStateChange(LeaseStateChange {
                    lease_id,
                    previous_state: lease_state_wire_name(notice.previous_state).to_string(),
                    new_state: lease_state_wire_name(notice.terminal_state).to_string(),
                    reason,
                    timestamp_wall_us: now_wall_us(),
                })),
            }))
            .await
            .is_err()
        {
            self.transition(SessionState::Closed);
            return LoopAction::Break;
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
                // ListElementsRequest will reflect the current position.
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
    /// subscribe time — so checking the active subscription here is sufficient
    /// and matches the RuntimeTelemetryFrame gate). State-stream class:
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
pub(super) fn validate_timing_hints(
    hints: &TimingHints,
    session_open_at_wall_us: u64,
    max_future_schedule_us: u64,
) -> Result<(), (&'static str, String)> {
    let present = hints.present_at_wall_us;
    let expires = hints.expires_at_wall_us;

    if present > 0 {
        let now = now_wall_us();

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

/// Map a canonical v1 capability wire name to the `Capability` enum variant.
///
/// Only canonical names (post-validation) reach this function.
/// Returns `None` for names that have no corresponding enum variant at this
/// layer (e.g., informational capabilities not enforced by the scene graph).
pub(super) fn canonical_name_to_capability(name: &str) -> Option<Capability> {
    match name {
        "create_tiles" => Some(Capability::CreateTiles),
        "modify_own_tiles" => Some(Capability::ModifyOwnTiles),
        "manage_tabs" => Some(Capability::ManageTabs),
        "manage_sync_groups" => Some(Capability::ManageSyncGroups),
        "upload_resource" => Some(Capability::UploadResource),
        "read_scene_topology" => Some(Capability::ReadSceneTopology),
        "subscribe_scene_events" => Some(Capability::SubscribeSceneEvents),
        "overlay_privileges" => Some(Capability::OverlayPrivileges),
        "access_input_events" => Some(Capability::AccessInputEvents),
        "high_priority_z_order" => Some(Capability::HighPriorityZOrder),
        "exceed_default_budgets" => Some(Capability::ExceedDefaultBudgets),
        "read_telemetry" => Some(Capability::ReadTelemetry),
        "resident_mcp" => Some(Capability::ResidentMcp),
        "lease:priority:1" => Some(Capability::LeasePriority1),
        _ if name.starts_with("publish_zone:") => {
            let zone = name.strip_prefix("publish_zone:").unwrap_or("*");
            Some(Capability::PublishZone(zone.to_string()))
        }
        _ if name.starts_with("publish_widget:") => {
            let widget = name.strip_prefix("publish_widget:").unwrap_or("*");
            Some(Capability::PublishWidget(widget.to_string()))
        }
        _ if name.starts_with("emit_scene_event:") => {
            let event = name.strip_prefix("emit_scene_event:").unwrap_or("");
            Some(Capability::EmitSceneEvent(event.to_string()))
        }
        // Higher-priority lease variants beyond priority 1 are not yet represented
        // in the enum; skip them without error (forward compat).
        _ => None,
    }
}

pub(super) fn capability_grant_covers(granted: &str, requested: &str) -> bool {
    if granted == "*" || granted == requested {
        return true;
    }

    (granted == "publish_zone:*" && requested.starts_with("publish_zone:"))
        || (granted == "publish_widget:*" && requested.starts_with("publish_widget:"))
        || (granted == "emit_scene_event:*" && requested.starts_with("emit_scene_event:"))
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
    let hint = serde_json::json!({
        "expected_flow": "ResourceUploadStart -> [ResourceUploadAccepted] -> ResourceUploadChunk* -> ResourceUploadComplete",
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
    session.last_heartbeat_ms = now_ms();

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
