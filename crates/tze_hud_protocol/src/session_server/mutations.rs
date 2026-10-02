//! Mutation batch handler for the session server.
//!
//! This module contains:
//! - `convert_proto_mutations`: single canonical conversion path shared by the
//!   live and freeze-drain paths; it also checks the agent's allow list.
//! - `handle_mutation_batch`: live-path handler (called from the dispatcher).
//! - `apply_queued_batch_to_scene`: drain-path handler (called from the session
//!   loop when the scene is unfrozen).

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};

use crate::convert;
use crate::dedup::CachedResult;
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;

use super::MutationBudgetDecision;
use super::freeze_queue::FreezeEnqueueResult;
use super::stream_session::StreamSession;
use super::verbs::{batch_result, fail};
use super::{
    DEFAULT_MAX_FUTURE_SCHEDULE_US, bytes_to_scene_id, capability_set_covers, now_wall_us,
    scene_id_to_bytes, validate_timing_hints,
};

/// The permission a mutation needs, and the `allow` entry that grants it.
fn required_permission(mutation: &SceneMutation) -> (String, String) {
    match mutation {
        SceneMutation::CreateTile { .. } => ("create_tiles".to_string(), "tiles".to_string()),
        SceneMutation::PublishToZone { zone_name, .. }
        | SceneMutation::ClearZone { zone_name, .. } => (
            format!("publish_zone:{zone_name}"),
            format!("zone:{zone_name}"),
        ),
        SceneMutation::ClearWidget { widget_name, .. } => (
            format!("publish_widget:{widget_name}"),
            format!("widget:{widget_name}"),
        ),
        _ => ("modify_own_tiles".to_string(), "tiles".to_string()),
    }
}

/// Reject the batch unless the agent's allow list covers every mutation.
fn check_mutation_permissions(
    mutations: &[SceneMutation],
    permissions: &[String],
) -> Result<(), (String, String)> {
    for mutation in mutations {
        let (permission, allow_entry) = required_permission(mutation);
        if !capability_set_covers(permissions, &permission) {
            return Err((
                "NOT_ALLOWED".to_string(),
                format!("agent allow list lacks {allow_entry}"),
            ));
        }
    }
    Ok(())
}

/// Convert a slice of proto [`MutationProto`] into scene mutations.
///
/// This is the single authoritative conversion path used by both the live path
/// ([`handle_mutation_batch`]) and the freeze-drain path
/// ([`apply_queued_batch_to_scene`]). The only intentional behavioural
/// difference between those two call sites is the log-line suffix; pass
/// `log_suffix = " (queued)"` for the drain path and `""` for the live path.
///
/// Returns `Err((error_code, message))` if any mutation cannot be converted
/// and the batch should be rejected. In that case the caller is responsible for
/// deciding what to do with the error (send a `RequestResult` on the live
/// path; log-and-skip on the drain path).
fn convert_proto_mutations(
    mutations: &[crate::proto::MutationProto],
    session: &StreamSession,
    log_suffix: &str,
) -> Result<Vec<SceneMutation>, (String, String)> {
    let mut scene_mutations = Vec::new();

    for m in mutations {
        match &m.mutation {
            Some(crate::proto::mutation_proto::Mutation::CreateTile(_)) => {
                return Err((
                    "INVALID_ARGUMENT".to_string(),
                    "create_tile is runtime-internal; claim tiles with ClaimTile".to_string(),
                ));
            }
            Some(crate::proto::mutation_proto::Mutation::SetTileRoot(str_)) => {
                // tile_id is encoded as uuid::Uuid::as_bytes() (big-endian RFC 4122 bytes),
                // matching scene_id_to_bytes / bytes_to_scene_id wire contract.
                match bytes_to_scene_id(&str_.tile_id) {
                    Ok(tile_id) => {
                        // Materialize any inline `children` subtree atomically
                        // (hud-ga4md): flat root-first list → root + descendants,
                        // carried on the single coalescible SetTileRoot mutation.
                        // Empty children = one-element list = flat root (unchanged).
                        if let Some(ref node_proto) = str_.node
                            && let Some(mut nodes) = convert::proto_node_tree_to_scene(node_proto)
                        {
                            let node = nodes.remove(0);
                            scene_mutations.push(SceneMutation::SetTileRoot {
                                tile_id,
                                node,
                                descendants: nodes,
                            });
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = str_.tile_id.len(),
                            "SetTileRoot{log_suffix}: invalid tile_id length (expected 16 bytes); \
                             mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::PublishToTile(_)) => {
                return Err((
                    "INVALID_ARGUMENT".to_string(),
                    "publish_to_tile is runtime-internal; use set_tile_root on a tile from ClaimTile".to_string(),
                ));
            }
            Some(crate::proto::mutation_proto::Mutation::UpdateNodeContent(unc)) => {
                match (
                    bytes_to_scene_id(&unc.tile_id),
                    bytes_to_scene_id(&unc.node_id),
                ) {
                    (Ok(tile_id), Ok(node_id)) => {
                        if let Some(ref d) = unc.data
                            && let Some(data) = convert::proto_update_node_content_data_to_scene(d)
                        {
                            scene_mutations.push(SceneMutation::UpdateNodeContent {
                                tile_id,
                                node_id,
                                data,
                            });
                        } else {
                            tracing::warn!(
                                "UpdateNodeContent{log_suffix}: missing or unrecognised data \
                                 variant; mutation skipped"
                            );
                        }
                    }
                    _ => {
                        tracing::warn!(
                            tile_id_len = unc.tile_id.len(),
                            node_id_len = unc.node_id.len(),
                            "UpdateNodeContent{log_suffix}: invalid tile_id or node_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::AddNode(an)) => {
                match bytes_to_scene_id(&an.tile_id) {
                    Ok(tile_id) => {
                        let parent_id_result = if an.parent_id.is_empty() {
                            Ok(None)
                        } else {
                            bytes_to_scene_id(&an.parent_id).map(Some)
                        };
                        match parent_id_result {
                            Ok(parent_id) => {
                                if let Some(ref node_proto) = an.node
                                    && let Some(node) = convert::proto_node_to_scene(node_proto)
                                {
                                    scene_mutations.push(SceneMutation::AddNode {
                                        tile_id,
                                        parent_id,
                                        node,
                                    });
                                }
                            }
                            Err(_) => {
                                tracing::warn!(
                                    parent_id_len = an.parent_id.len(),
                                    "AddNode{log_suffix}: invalid parent_id length (expected 16 \
                                     bytes); mutation skipped — SDK bug or wire corruption"
                                );
                            }
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = an.tile_id.len(),
                            "AddNode{log_suffix}: invalid tile_id length (expected 16 bytes); \
                             mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::UpdateTileOpacity(uto)) => {
                match bytes_to_scene_id(&uto.tile_id) {
                    Ok(tile_id) => {
                        scene_mutations.push(SceneMutation::UpdateTileOpacity {
                            tile_id,
                            opacity: uto.opacity,
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = uto.tile_id.len(),
                            "UpdateTileOpacity{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::UpdateTileInputMode(utim)) => {
                match bytes_to_scene_id(&utim.tile_id) {
                    Ok(tile_id) => {
                        let input_mode = convert::proto_input_mode_to_scene(
                            crate::proto::TileInputModeProto::try_from(utim.input_mode).unwrap_or(
                                crate::proto::TileInputModeProto::TileInputModeUnspecified,
                            ),
                        );
                        scene_mutations.push(SceneMutation::UpdateTileInputMode {
                            tile_id,
                            input_mode,
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = utim.tile_id.len(),
                            "UpdateTileInputMode{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::RegisterTileScroll(rts)) => {
                match bytes_to_scene_id(&rts.tile_id) {
                    Ok(tile_id) => {
                        // -1.0 sentinel = unset (no clamp); >= 0.0 = clamp limit.
                        let content_width = if rts.content_width >= 0.0 {
                            Some(rts.content_width)
                        } else {
                            None
                        };
                        let content_height = if rts.content_height >= 0.0 {
                            Some(rts.content_height)
                        } else {
                            None
                        };
                        scene_mutations.push(SceneMutation::RegisterTileScroll {
                            tile_id,
                            scrollable_x: rts.scrollable_x,
                            scrollable_y: rts.scrollable_y,
                            content_width,
                            content_height,
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = rts.tile_id.len(),
                            "RegisterTileScroll{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::SetScrollOffset(sso)) => {
                match bytes_to_scene_id(&sso.tile_id) {
                    Ok(tile_id) => {
                        scene_mutations.push(SceneMutation::SetScrollOffset {
                            tile_id,
                            offset_x: sso.offset_x,
                            offset_y: sso.offset_y,
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = sso.tile_id.len(),
                            "SetScrollOffset{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(
                crate::proto::mutation_proto::Mutation::SetTileLifecycleAccent(_)
                | crate::proto::mutation_proto::Mutation::SetTileUnreadCount(_)
                | crate::proto::mutation_proto::Mutation::SetTileComposerInteraction(_)
                | crate::proto::mutation_proto::Mutation::SetPortalSurface(_)
                | crate::proto::mutation_proto::Mutation::UpdatePortalSurfaceState(_),
            ) => {
                return Err((
                    "INVALID_ARGUMENT".to_string(),
                    "portal mutations are runtime-internal; drive a portal with hud_publish"
                        .to_string(),
                ));
            }

            None => {}
        }
    }

    check_mutation_permissions(&scene_mutations, &session.capabilities)?;

    Ok(scene_mutations)
}

pub(super) async fn handle_mutation_batch(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    client_sequence: u64,
    batch: MutationBatch,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    // ── Step 1: Safe mode check (RFC 0005 §3.7) ─────────────────────────────
    // Reject MutationBatch when safe mode is active.
    // Session-local flag tracks per-session suspension (from SessionSuspended delivery).
    // Shared state flag tracks global suspension (from the runtime side).
    // Both are checked; shared state takes precedence.
    // Per the spec invariant: safe_mode=true implies freeze_active=false,
    // so this check runs before the freeze check.
    {
        let st = state.lock().await;
        let safe_mode = session.safe_mode_active
            || st
                .safe_mode_atomic
                .load(std::sync::atomic::Ordering::Acquire);
        if safe_mode {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::RequestResult(fail(
                        client_sequence,
                        "SAFE_MODE_ACTIVE",
                        "the human paused agents; retry after SessionResumed",
                    ))),
                }))
                .await;
            return;
        }
    }

    // ── Step 2: Freeze check (system-shell/spec.md §Freeze Scene) ────────────
    // When the scene is frozen, mutations are QUEUED (not rejected).
    // Agents are NEVER informed that the scene is frozen — signals are generic
    // queue-pressure signals to avoid leaking viewer state.
    //
    // FIFO-drain invariant: also enqueue when freeze_active has just been cleared
    // but the freeze drain loop has NOT yet emptied the queue. A new mutation that
    // bypasses the queue in this window would be applied BEFORE still-queued ones,
    // violating submission order. Checking `!session.freeze_queue.is_empty()` here
    // closes that race: the mutation is kept behind the existing entries until the
    // drain loop fully empties the queue and steady-state (no freeze) resumes.
    {
        let st = state.lock().await;
        if st.freeze_active || !session.freeze_queue.is_empty() {
            // ── Deduplication on the freeze path (RFC 0005 §5.2) ─────────────
            //
            // A Transactional batch retransmitted while the scene is frozen must
            // be suppressed here, before it enters the freeze queue.  Without
            // this check the retransmit is enqueued as a second entry and applied
            // twice after drain — a duplicate-application bug that does not occur
            // on the non-frozen path (where the dedup window is consulted before
            // the batch reaches the scene).
            //
            // Symmetry with the non-frozen path:
            //   non-frozen: check dedup_window → apply to scene → insert dedup_window
            //   frozen:     check dedup_window → enqueue       → insert dedup_window
            //
            // We cache accepted=true (empty created_ids) here because that is the
            // response the client receives for a queued batch; drain does not send
            // a second RequestResult.
            if !batch.batch_id.is_empty() {
                if let Some(cached) = session.dedup_window.lookup(&batch.batch_id) {
                    let seq = session.next_server_seq();
                    drop(st);
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                batch.batch_id,
                                cached.accepted,
                                cached.created_ids,
                                cached.error_code,
                                cached.error_message,
                            )),
                        }))
                        .await;
                    return;
                }
            }

            // Determine traffic class and enqueue.
            let namespace = session.namespace.clone();
            let result = session.freeze_queue.enqueue(batch.clone(), &namespace);
            drop(st);

            match result {
                FreezeEnqueueResult::Queued { pressure_warning } => {
                    // Cache accepted=true in the dedup window so a retransmit of
                    // the same batch_id while still frozen is suppressed above.
                    // created_ids is empty because the queued path sends no
                    // created-element IDs at enqueue time (they are not known yet).
                    if !batch.batch_id.is_empty() {
                        session.dedup_window.insert(
                            batch.batch_id.clone(),
                            CachedResult {
                                accepted: true,
                                created_ids: Vec::new(),
                                error_code: String::new(),
                                error_message: String::new(),
                            },
                        );
                    }
                    if pressure_warning {
                        // Send MUTATION_QUEUE_PRESSURE — generic, not freeze-specific.
                        let seq = session.next_server_seq();
                        let _ = tx
                            .send(Ok(ServerMessage {
                                sequence: seq,
                                timestamp_wall_us: now_wall_us(),
                                payload: Some(batch_result(
                                    client_sequence,
                                    batch.batch_id,
                                    true,
                                    Vec::new(),
                                    "UNAVAILABLE".to_string(),
                                    "Mutation queue is under pressure (>= 80% capacity)."
                                        .to_string(),
                                )),
                            }))
                            .await;
                    } else {
                        // Send accepted=true (queued — not yet applied, but accepted).
                        let seq = session.next_server_seq();
                        let _ = tx
                            .send(Ok(ServerMessage {
                                sequence: seq,
                                timestamp_wall_us: now_wall_us(),
                                payload: Some(batch_result(
                                    client_sequence,
                                    batch.batch_id,
                                    true,
                                    Vec::new(),
                                    String::new(),
                                    String::new(),
                                )),
                            }))
                            .await;
                    }
                }
                FreezeEnqueueResult::Coalesced => {
                    // Coalesced with an existing entry — accepted. Cache so retransmits
                    // while frozen do not re-coalesce or create duplicate queue entries.
                    if !batch.batch_id.is_empty() {
                        session.dedup_window.insert(
                            batch.batch_id.clone(),
                            CachedResult {
                                accepted: true,
                                created_ids: Vec::new(),
                                error_code: String::new(),
                                error_message: String::new(),
                            },
                        );
                    }
                    let seq = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                batch.batch_id,
                                true,
                                Vec::new(),
                                String::new(),
                                String::new(),
                            )),
                        }))
                        .await;
                }
                FreezeEnqueueResult::Evicted { evicted_batch_id } => {
                    // An older non-transactional entry was evicted; new one queued.
                    // Cache the new batch as accepted so retransmits while frozen
                    // are suppressed.
                    if !batch.batch_id.is_empty() {
                        session.dedup_window.insert(
                            batch.batch_id.clone(),
                            CachedResult {
                                accepted: true,
                                created_ids: Vec::new(),
                                error_code: String::new(),
                                error_message: String::new(),
                            },
                        );
                    }
                    // Invalidate any stale accepted=true entry for the evicted batch.
                    // Without this, a client that retransmits the evicted batch_id while
                    // still frozen would hit the old cache entry and receive accepted=true
                    // even though the mutation was dropped.  Overwrite with the actual
                    // outcome so the dedup window reflects reality.
                    if !evicted_batch_id.is_empty() {
                        session.dedup_window.insert(
                            evicted_batch_id.clone(),
                            CachedResult {
                                accepted: false,
                                created_ids: Vec::new(),
                                error_code: "UNAVAILABLE".to_string(),
                                error_message:
                                    "Mutation evicted from queue due to capacity pressure."
                                        .to_string(),
                            },
                        );
                    }
                    // Send MUTATION_DROPPED for the evicted batch (generic signal).
                    let seq_evicted = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq_evicted,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                evicted_batch_id,
                                false,
                                Vec::new(),
                                "UNAVAILABLE".to_string(),
                                "Mutation evicted from queue due to capacity pressure.".to_string(),
                            )),
                        }))
                        .await;
                    // New batch was queued — send accepted.
                    let seq_new = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq_new,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                batch.batch_id,
                                true,
                                Vec::new(),
                                String::new(),
                                String::new(),
                            )),
                        }))
                        .await;
                }
                FreezeEnqueueResult::BackpressureRequired => {
                    // Transactional mutation: queue full — apply gRPC backpressure.
                    // Do NOT cache in dedup_window: the client must retry and we want
                    // that retry to enter the queue once capacity frees up.
                    // Send MUTATION_QUEUE_PRESSURE signal.
                    let seq = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                batch.batch_id,
                                false,
                                Vec::new(),
                                "UNAVAILABLE".to_string(),
                                "Mutation queue full; backpressure applied.".to_string(),
                            )),
                        }))
                        .await;
                }
                FreezeEnqueueResult::Dropped => {
                    // Ephemeral mutation dropped. Do NOT cache: ephemeral retransmits
                    // are not expected (ephemeral = drop-on-overflow is fine semantics).
                    let seq = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(batch_result(
                                client_sequence,
                                batch.batch_id,
                                false,
                                Vec::new(),
                                "UNAVAILABLE".to_string(),
                                "Ephemeral mutation dropped; queue at capacity.".to_string(),
                            )),
                        }))
                        .await;
                }
            }
            return;
        }
    }

    // ── Deduplication (RFC 0005 §5.2) ────────────────────────────────────────
    //
    // If this batch_id is already in the dedup window, return the cached result
    // without re-applying mutations. This covers retransmission scenarios where
    // the agent resends with the same batch_id and a new sequence number.
    if !batch.batch_id.is_empty() {
        if let Some(cached) = session.dedup_window.lookup(&batch.batch_id) {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(batch_result(
                        client_sequence,
                        batch.batch_id,
                        cached.accepted,
                        cached.created_ids,
                        cached.error_code,
                        cached.error_message,
                    )),
                }))
                .await;
            return;
        }
    }

    // ── TimingHints validation (RFC 0003 §3.5, RFC 0005 §3.3) ────────────────
    if let Some(ref hints) = batch.timing {
        if let Err((error_code, message)) = validate_timing_hints(
            hints,
            session.session_open_at_wall_us,
            DEFAULT_MAX_FUTURE_SCHEDULE_US,
        ) {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::RequestResult(fail(
                        client_sequence,
                        error_code,
                        message,
                    ))),
                }))
                .await;
            return;
        }
    }

    let st = state.lock().await;

    let lease_id = match bytes_to_scene_id(&batch.lease_id) {
        Ok(id) => id,
        Err(_) => {
            let cached = CachedResult {
                accepted: false,
                created_ids: Vec::new(),
                error_code: "INVALID_ARGUMENT".to_string(),
                error_message: "Invalid lease_id bytes".to_string(),
            };
            if !batch.batch_id.is_empty() {
                session
                    .dedup_window
                    .insert(batch.batch_id.clone(), cached.clone());
            }
            let seq = session.next_server_seq();
            // Drop lock before awaiting send to avoid holding mutex across await point.
            drop(st);
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(batch_result(
                        client_sequence,
                        batch.batch_id,
                        false,
                        Vec::new(),
                        cached.error_code,
                        cached.error_message,
                    )),
                }))
                .await;
            return;
        }
    };

    // Convert proto mutations to scene mutations (single canonical path shared
    // with the freeze-drain path; only the log suffix differs between the two).
    let converted = match convert_proto_mutations(&batch.mutations, session, "") {
        Ok(c) => c,
        Err((error_code, error_message)) => {
            let cached = CachedResult {
                accepted: false,
                created_ids: Vec::new(),
                error_code: error_code.clone(),
                error_message: error_message.clone(),
            };
            if !batch.batch_id.is_empty() {
                session
                    .dedup_window
                    .insert(batch.batch_id.clone(), cached.clone());
            }
            let seq = session.next_server_seq();
            drop(st);
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(batch_result(
                        client_sequence,
                        batch.batch_id,
                        false,
                        Vec::new(),
                        cached.error_code,
                        cached.error_message,
                    )),
                }))
                .await;
            return;
        }
    };
    let scene_mutations = converted;

    let scene_batch_id = proto_batch_id_to_scene_id(&batch.batch_id);
    let timing_hints = scene_timing_hints(batch.timing.as_ref());
    let present_at_wall_us = timing_hints
        .as_ref()
        .and_then(|h| h.present_at_wall_us)
        .map(|t| t.0)
        .filter(|&t| t > 0);
    let scene_batch = SceneMutationBatch {
        batch_id: scene_batch_id,
        agent_namespace: session.namespace.clone(),
        mutations: scene_mutations,
        timing_hints,
        lease_id: Some(lease_id),
    };

    // present_at in the future: hold the batch until due (invariant 1).
    let scheduled_at = {
        let now_us = st.scene.lock().await.now_wall_us();
        present_at_wall_us.filter(|&t| t > now_us)
    };
    let budget_delta = st
        .scene
        .lock()
        .await
        .mutation_budget_delta(&lease_id, &scene_batch);
    if let Some(enforcer) = &session.budget_enforcer {
        match enforcer.reserve_mutation(
            session.scene_session_id,
            budget_delta.delta_tiles,
            budget_delta.delta_texture_bytes,
            budget_delta.max_nodes_in_batch,
        ) {
            MutationBudgetDecision::Allow => {}
            MutationBudgetDecision::Reject {
                error_code,
                message,
            } => {
                let cached = CachedResult {
                    accepted: false,
                    created_ids: Vec::new(),
                    error_code: "BUDGET_EXCEEDED".to_string(),
                    error_message: format!("{error_code}: {message}"),
                };
                if !batch.batch_id.is_empty() {
                    session
                        .dedup_window
                        .insert(batch.batch_id.clone(), cached.clone());
                }
                let seq = session.next_server_seq();
                drop(st);
                let _ = tx
                    .send(Ok(ServerMessage {
                        sequence: seq,
                        timestamp_wall_us: now_wall_us(),
                        payload: Some(batch_result(
                            client_sequence,
                            batch.batch_id,
                            false,
                            Vec::new(),
                            cached.error_code,
                            cached.error_message,
                        )),
                    }))
                    .await;
                return;
            }
        }
    }

    if let Some(present_at) = scheduled_at {
        st.scene
            .lock()
            .await
            .schedule_batch(present_at, scene_batch);
        if !batch.batch_id.is_empty() {
            session.dedup_window.insert(
                batch.batch_id.clone(),
                CachedResult {
                    accepted: true,
                    created_ids: Vec::new(),
                    error_code: String::new(),
                    error_message: String::new(),
                },
            );
        }
        let seq = session.next_server_seq();
        drop(st);
        // Wake the compositor so it arms a wake for the new deadline.
        render_wake.notify();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(batch_result(
                    client_sequence,
                    batch.batch_id,
                    true,
                    Vec::new(),
                    String::new(),
                    String::new(),
                )),
            }))
            .await;
        return;
    }

    let result = {
        let mut scene = st.scene.lock().await;
        let r = scene.apply_batch(&scene_batch);
        // A batch may switch the active tab (SwitchTab mutation) or auto-activate
        // the first tab on initial tile creation; keep the lock-free
        // keyboard-dispatch mirror in sync so composer echo routes correctly
        // without ever touching the scene mutex (hud-dwcr7).
        st.refresh_active_tab_mirror(&scene);
        r
    };

    let seq = session.next_server_seq();
    if !result.applied
        && let Some(enforcer) = &session.budget_enforcer
    {
        enforcer.rollback_mutation(
            session.scene_session_id,
            budget_delta.delta_tiles,
            budget_delta.delta_texture_bytes,
        );
    }
    if result.applied {
        let created_ids: Vec<Vec<u8>> = result
            .created_ids
            .iter()
            .map(|id| scene_id_to_bytes(*id))
            .collect();

        // Cache result before sending.
        if !batch.batch_id.is_empty() {
            session.dedup_window.insert(
                batch.batch_id.clone(),
                CachedResult {
                    accepted: true,
                    created_ids: created_ids.clone(),
                    error_code: String::new(),
                    error_message: String::new(),
                },
            );
        }

        // Drop lock before awaiting send to avoid holding mutex across await point.
        drop(st);
        render_wake.notify();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(batch_result(
                    client_sequence,
                    batch.batch_id,
                    true,
                    created_ids,
                    String::new(),
                    String::new(),
                )),
            }))
            .await;
    } else {
        // The scene's own reason reaches the agent as the hint.
        let (error_code, error_message) = match result.error {
            Some(e) => (
                tze_hud_scene::error_codes::validation_error_code(&e),
                e.to_string(),
            ),
            None => ("INTERNAL", "batch was not applied".to_string()),
        };

        // Cache rejection result before sending.
        if !batch.batch_id.is_empty() {
            session.dedup_window.insert(
                batch.batch_id.clone(),
                CachedResult {
                    accepted: false,
                    created_ids: Vec::new(),
                    error_code: error_code.to_string(),
                    error_message: error_message.clone(),
                },
            );
        }

        // Drop lock before awaiting send to avoid holding mutex across await point.
        drop(st);
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(batch_result(
                    client_sequence,
                    batch.batch_id,
                    false,
                    Vec::new(),
                    error_code.to_string(),
                    error_message,
                )),
            }))
            .await;
    }
}

/// Apply a previously-queued mutation batch to the scene without sending a
/// `RequestResult` response.
///
/// This is called during the unfreeze drain. The initial `RequestResult`
/// (with `accepted = true`) was already sent when the batch was enqueued;
/// sending a second one would violate the "one response per request" contract
/// (RFC 0005 §2.1).
///
/// Safe mode and freeze checks are intentionally skipped here: the spec
/// invariant (`safe_mode = true → freeze_active = false`) guarantees that
/// safe mode cannot activate between freeze deactivation and the drain.
pub(super) async fn apply_queued_batch_to_scene(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    batch: MutationBatch,
) -> bool {
    let st = state.lock().await;

    let lease_id = match bytes_to_scene_id(&batch.lease_id) {
        Ok(id) => id,
        Err(_) => return false, // invalid lease_id — silently skip (already acked)
    };

    // Convert proto mutations to scene mutations (single canonical path shared
    // with the live path; the " (queued)" suffix distinguishes drain-path logs).
    let converted = match convert_proto_mutations(&batch.mutations, session, " (queued)") {
        Ok(c) => c,
        Err((error_code, error_message)) => {
            tracing::warn!(
                error_code,
                error_message,
                "queued mutation batch skipped due to conversion error after enqueue"
            );
            return false;
        }
    };
    let scene_mutations = converted;
    let scene_batch_id = proto_batch_id_to_scene_id(&batch.batch_id);
    let timing_hints = scene_timing_hints(batch.timing.as_ref());
    let present_at_wall_us = timing_hints
        .as_ref()
        .and_then(|h| h.present_at_wall_us)
        .map(|t| t.0)
        .filter(|&t| t > 0);
    let scene_batch = SceneMutationBatch {
        batch_id: scene_batch_id,
        agent_namespace: session.namespace.clone(),
        mutations: scene_mutations,
        timing_hints,
        lease_id: Some(lease_id),
    };

    // present_at in the future: hold the batch until due (invariant 1).
    let scheduled_at = {
        let now_us = st.scene.lock().await.now_wall_us();
        present_at_wall_us.filter(|&t| t > now_us)
    };
    if scheduled_at.is_some()
        && scene_batch
            .mutations
            .iter()
            .any(|m| matches!(m, SceneMutation::CreateTile { .. }))
    {
        tracing::warn!(
            namespace = session.namespace,
            "queued mutation batch skipped: future present_at with CreateTile"
        );
        return false;
    }
    let budget_delta = st
        .scene
        .lock()
        .await
        .mutation_budget_delta(&lease_id, &scene_batch);
    if let Some(enforcer) = &session.budget_enforcer {
        if !matches!(
            enforcer.reserve_mutation(
                session.scene_session_id,
                budget_delta.delta_tiles,
                budget_delta.delta_texture_bytes,
                budget_delta.max_nodes_in_batch,
            ),
            MutationBudgetDecision::Allow
        ) {
            tracing::warn!(
                namespace = session.namespace,
                "queued mutation batch skipped by runtime budget admission"
            );
            return false;
        }
    }

    // Apply to scene; response was already sent when the batch was queued.
    if let Some(present_at) = scheduled_at {
        st.scene
            .lock()
            .await
            .schedule_batch(present_at, scene_batch);
        return true;
    }

    let result = {
        let mut scene = st.scene.lock().await;
        let r = scene.apply_batch(&scene_batch);
        // Keep the lock-free keyboard-dispatch mirror in sync with any
        // active-tab change in this drained batch (hud-dwcr7).
        st.refresh_active_tab_mirror(&scene);
        r
    };
    if !result.applied {
        if let Some(enforcer) = &session.budget_enforcer {
            enforcer.rollback_mutation(
                session.scene_session_id,
                budget_delta.delta_tiles,
                budget_delta.delta_texture_bytes,
            );
        }
    }
    if result.applied {
        drop(st);
        true
    } else {
        false
    }
}

// ─── Helpers used only by this module (migrated from mod.rs, SS-9) ──────────

/// Map proto `batch_id` bytes to a `SceneId` for rejection-correlation semantics.
///
/// If the client supplied a valid 16-byte UUID, use it directly so that any
/// `BatchRejected` or `RequestResult` echoes the client's own `batch_id`.
/// Note: `bytes_to_scene_id` validates only the byte length (16 bytes); UUID
/// version/variant are not checked because the spec (RFC 0005 §3.2) requires
/// only that `batch_id` is a 16-byte RFC 4122 UUID (big-endian, matching
/// `scene_id_to_bytes` / `bytes_to_scene_id`) — version bits are the client's
/// responsibility.
///
/// Falls back to a fresh `SceneId` only when the field is absent or malformed
/// (wrong length); logs a debug warning so SDK regressions are diagnosable.
fn proto_batch_id_to_scene_id(batch_id: &[u8]) -> tze_hud_scene::SceneId {
    match bytes_to_scene_id(batch_id) {
        Ok(id) => id,
        Err(_) => {
            tracing::debug!(
                batch_id_len = batch_id.len(),
                "proto batch_id is absent or malformed (expected 16 bytes); \
                 generating a fresh SceneId — client cannot correlate this batch"
            );
            tze_hud_scene::SceneId::new()
        }
    }
}

/// Convert wire timing hints to scene hints; zero fields mean "not set".
fn scene_timing_hints(
    hints: Option<&TimingHints>,
) -> Option<tze_hud_scene::mutation::BatchTimingHints> {
    use tze_hud_scene::timing::domains::WallUs;
    let hints = hints?;
    let set = |us: u64| (us > 0).then_some(WallUs(us));
    let present_at_wall_us = set(hints.present_at_wall_us);
    let expires_at_wall_us = set(hints.expires_at_wall_us);
    (present_at_wall_us.is_some() || expires_at_wall_us.is_some()).then_some(
        tze_hud_scene::mutation::BatchTimingHints {
            present_at_wall_us,
            expires_at_wall_us,
        },
    )
}
