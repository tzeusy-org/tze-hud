//! Mutation batch handler for the session server (RFC 0005 §3.3, §3.7, §5.2).
//!
//! This module contains:
//! - `ConvertedBatch`: output type of the proto→scene conversion.
//! - `convert_proto_mutations`: single canonical conversion path shared by the
//!   live and freeze-drain paths.
//! - `handle_mutation_batch`: live-path handler (called from the dispatcher).
//! - `apply_queued_batch_to_scene`: drain-path handler (called from the session
//!   loop when the scene is unfrozen).

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::element_store::{ElementStoreEntry, ElementType};
use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};
use tze_hud_scene::types::*;

use crate::convert;
use crate::dedup::CachedResult;
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;

use super::MutationBudgetDecision;
use super::freeze_queue::FreezeEnqueueResult;
use super::stream_session::StreamSession;
use super::{
    DEFAULT_MAX_FUTURE_SCHEDULE_US, ElementStorePersistRequest, bytes_to_scene_id, now_ms,
    now_wall_us, persist_created_tile_entries, persist_element_store, scene_id_to_bytes,
    validate_timing_hints,
};

/// Output of [`convert_proto_mutations`]: the converted scene mutations and the
/// element-store bookkeeping side-effects needed after a successful apply.
struct ConvertedBatch {
    scene_mutations: Vec<SceneMutation>,
    /// Elements that should have `last_published_at` updated, keyed by ID.
    pending_touch_ids: Vec<(SceneId, ElementType)>,
    /// Elements that should have `last_published_at` updated, keyed by namespace string.
    pending_touch_names: Vec<(ElementType, String)>,
}

/// Convert a slice of proto [`MutationProto`] into a [`ConvertedBatch`].
///
/// This is the single authoritative conversion path used by both the live path
/// ([`handle_mutation_batch`]) and the freeze-drain path
/// ([`apply_queued_batch_to_scene`]). The only intentional behavioural
/// difference between those two call sites is the log-line suffix; pass
/// `log_suffix = " (queued)"` for the drain path and `""` for the live path.
///
/// Returns `Err((error_code, message))` if any mutation cannot be converted
/// and the batch should be rejected. In that case the caller is responsible for
/// deciding what to do with the error (send a `MutationResult` on the live
/// path; log-and-skip on the drain path).
fn convert_proto_mutations(
    mutations: &[crate::proto::MutationProto],
    element_store: &tze_hud_scene::element_store::ElementStore,
    tab_id: SceneId,
    lease_id: SceneId,
    display_area: tze_hud_scene::Rect,
    namespace: &str,
    log_suffix: &str,
) -> Result<ConvertedBatch, (String, String)> {
    let mut scene_mutations = Vec::new();
    let mut pending_touch_ids: Vec<(SceneId, ElementType)> = Vec::new();
    let mut pending_touch_names: Vec<(ElementType, String)> = Vec::new();

    for m in mutations {
        match &m.mutation {
            Some(crate::proto::mutation_proto::Mutation::CreateTile(ct)) => {
                let requested_bounds = ct
                    .bounds
                    .as_ref()
                    .map(convert::proto_rect_to_scene)
                    .unwrap_or(tze_hud_scene::Rect::new(0.0, 0.0, 200.0, 150.0));
                let bounds =
                    resolve_tile_bounds_with_override(None, Some(requested_bounds), display_area)
                        .unwrap_or(requested_bounds);
                scene_mutations.push(SceneMutation::CreateTile {
                    tab_id,
                    namespace: namespace.to_string(),
                    lease_id,
                    bounds,
                    z_order: ct.z_order,
                });
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
            Some(crate::proto::mutation_proto::Mutation::PublishToZone(pz)) => {
                let content = pz
                    .content
                    .as_ref()
                    .and_then(convert::proto_zone_content_to_scene);
                if let Some(content) = content {
                    let resolved_zone_name = if !pz.element_id.is_empty() {
                        match bytes_to_scene_id(&pz.element_id) {
                            Ok(element_id) => match element_store.entries.get(&element_id) {
                                Some(entry) if entry.element_type == ElementType::Zone => {
                                    pending_touch_ids.push((element_id, ElementType::Zone));
                                    entry.namespace.clone()
                                }
                                _ => {
                                    return Err((
                                        "ELEMENT_NOT_FOUND".to_string(),
                                        "publish_to_zone element_id does not reference a known zone"
                                            .to_string(),
                                    ));
                                }
                            },
                            Err(_) => {
                                return Err((
                                    "INVALID_ARGUMENT".to_string(),
                                    "publish_to_zone element_id must be 16 bytes".to_string(),
                                ));
                            }
                        }
                    } else {
                        pending_touch_names.push((ElementType::Zone, pz.zone_name.clone()));
                        pz.zone_name.clone()
                    };
                    let token = tze_hud_scene::types::ZonePublishToken {
                        token: pz
                            .publish_token
                            .as_ref()
                            .map(|t| t.token.clone())
                            .unwrap_or_default(),
                    };
                    let merge_key = if pz.merge_key.is_empty() {
                        None
                    } else {
                        Some(pz.merge_key.clone())
                    };
                    scene_mutations.push(SceneMutation::PublishToZone {
                        zone_name: resolved_zone_name,
                        content,
                        publish_token: token,
                        merge_key,
                        // expires_at_wall_us and content_classification are not yet present
                        // in the PublishToZoneMutation proto (post-v1 wire extensions).
                        expires_at_wall_us: None,
                        content_classification: None,
                        // breakpoints are not in the MutationBatch PublishToZoneMutation proto
                        // (post-v1 wire extension); use ZonePublish path for streaming.
                        breakpoints: Vec::new(),
                    });
                }
            }
            Some(crate::proto::mutation_proto::Mutation::PublishToTile(pt)) => {
                let element_id = match bytes_to_scene_id(&pt.element_id) {
                    Ok(id) => id,
                    Err(_) => {
                        return Err((
                            "INVALID_ARGUMENT".to_string(),
                            "publish_to_tile element_id must be 16 bytes".to_string(),
                        ));
                    }
                };

                let entry = match element_store.entries.get(&element_id) {
                    Some(entry) if entry.element_type == ElementType::Tile => entry.clone(),
                    _ => {
                        return Err((
                            "ELEMENT_NOT_FOUND".to_string(),
                            "publish_to_tile element_id does not reference a known tile"
                                .to_string(),
                        ));
                    }
                };

                let requested_bounds = pt.bounds.as_ref().map(convert::proto_rect_to_scene);
                if let Some(resolved_bounds) =
                    resolve_tile_bounds_with_override(Some(&entry), requested_bounds, display_area)
                {
                    scene_mutations.push(SceneMutation::UpdateTileBounds {
                        tile_id: element_id,
                        bounds: resolved_bounds,
                    });
                }

                let mut had_content = false;
                if let Some(ref node_proto) = pt.node {
                    // Inline-subtree materialization (hud-ga4md): see the
                    // SetTileRoot arm above — flat list split into root +
                    // descendants on one coalescible mutation.
                    if let Some(mut nodes) = convert::proto_node_tree_to_scene(node_proto) {
                        let node = nodes.remove(0);
                        scene_mutations.push(SceneMutation::SetTileRoot {
                            tile_id: element_id,
                            node,
                            descendants: nodes,
                        });
                        had_content = true;
                    } else {
                        return Err((
                            "INVALID_ARGUMENT".to_string(),
                            "publish_to_tile node content is invalid or missing data".to_string(),
                        ));
                    }
                }

                if !had_content && requested_bounds.is_none() {
                    return Err((
                        "INVALID_ARGUMENT".to_string(),
                        "publish_to_tile requires at least one of bounds or node".to_string(),
                    ));
                }

                pending_touch_ids.push((element_id, ElementType::Tile));
            }
            Some(crate::proto::mutation_proto::Mutation::ClearZone(cz)) => {
                let token = tze_hud_scene::types::ZonePublishToken {
                    token: cz
                        .publish_token
                        .as_ref()
                        .map(|t| t.token.clone())
                        .unwrap_or_default(),
                };
                scene_mutations.push(SceneMutation::ClearZone {
                    zone_name: cz.zone_name.clone(),
                    publish_token: token,
                });
            }
            Some(crate::proto::mutation_proto::Mutation::ClearWidget(cw)) => {
                let instance_id = (!cw.instance_id.is_empty()).then_some(cw.instance_id.clone());
                scene_mutations.push(SceneMutation::ClearWidget {
                    widget_name: cw.widget_name.clone(),
                    instance_id,
                });
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
            Some(crate::proto::mutation_proto::Mutation::SetTileLifecycleAccent(sla)) => {
                match bytes_to_scene_id(&sla.tile_id) {
                    Ok(tile_id) => {
                        // Absent/zero-alpha color or non-positive width = clear.
                        let accent = sla.color.as_ref().and_then(|c| {
                            if sla.width_px > 0.0 && c.a > 0.0 {
                                Some(LifecycleAccent {
                                    color: convert::proto_rgba_to_scene(c),
                                    width_px: sla.width_px,
                                })
                            } else {
                                None
                            }
                        });
                        scene_mutations
                            .push(SceneMutation::SetTileLifecycleAccent { tile_id, accent });
                        // No `pending_touch_ids` entry: the accent is runtime
                        // overlay state, not a published element, so it must not
                        // bump the tile's element-store `last_published_at`. The
                        // #943 present-gate redraw is armed by the version bump in
                        // `set/clear_tile_lifecycle_accent` (overlay.rs), which
                        // fires for any accent transition independent of whether a
                        // content mutation co-travels.
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = sla.tile_id.len(),
                            "SetTileLifecycleAccent{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::SetTileUnreadCount(stuc)) => {
                match bytes_to_scene_id(&stuc.tile_id) {
                    Ok(tile_id) => {
                        scene_mutations.push(SceneMutation::SetTileUnreadCount {
                            tile_id,
                            count: stuc.count as usize,
                        });
                        // No `pending_touch_ids` entry: the badge count is runtime
                        // overlay state, not a published element, so it must not
                        // bump the tile's element-store `last_published_at`
                        // (mirrors SetTileLifecycleAccent). The count always rides
                        // alongside a co-travelling `PublishToTile` content mutation
                        // in the portal render batch, whose repaint carries it.
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = stuc.tile_id.len(),
                            "SetTileUnreadCount{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::SetTileComposerInteraction(stci)) => {
                match bytes_to_scene_id(&stci.tile_id) {
                    Ok(tile_id) => {
                        // Absent composer = clear (interaction disabled), mirroring
                        // the accent's absent-color clear.
                        let region = stci
                            .composer
                            .as_ref()
                            .map(convert::proto_hit_region_to_scene);
                        scene_mutations
                            .push(SceneMutation::SetTileComposerInteraction { tile_id, region });
                        // No `pending_touch_ids` entry: the composer hit region is
                        // runtime overlay state, not a published element, so it must
                        // not bump the tile's element-store `last_published_at`
                        // (mirrors SetTileLifecycleAccent). The present-gate redraw is
                        // armed by the version bump in
                        // `set/clear_tile_composer_interaction` (overlay.rs).
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = stci.tile_id.len(),
                            "SetTileComposerInteraction{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::SetPortalSurface(sps)) => {
                match bytes_to_scene_id(&sps.tile_id) {
                    Ok(tile_id) => {
                        if let Some(ref surface_proto) = sps.surface {
                            match convert::proto_portal_surface_to_scene(surface_proto) {
                                Ok(surface) => {
                                    scene_mutations
                                        .push(SceneMutation::SetPortalSurface { tile_id, surface });
                                    // No `pending_touch_ids`: the portal surface is
                                    // runtime overlay state, not a published element
                                    // (mirrors SetTileLifecycleAccent). The present-gate
                                    // redraw is armed by the version bump in
                                    // `set_portal_surface` (overlay.rs).
                                }
                                Err(reason) => {
                                    tracing::warn!(
                                        "SetPortalSurface{log_suffix}: invalid surface \
                                         ({reason}); mutation skipped"
                                    );
                                }
                            }
                        } else {
                            tracing::warn!(
                                "SetPortalSurface{log_suffix}: missing surface; mutation skipped"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = sps.tile_id.len(),
                            "SetPortalSurface{log_suffix}: invalid tile_id length (expected 16 \
                             bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            Some(crate::proto::mutation_proto::Mutation::UpdatePortalSurfaceState(ups)) => {
                match bytes_to_scene_id(&ups.tile_id) {
                    Ok(tile_id) => {
                        scene_mutations.push(SceneMutation::UpdatePortalSurfaceState {
                            tile_id,
                            lifecycle: convert::proto_portal_lifecycle_to_scene(ups.lifecycle),
                            display_state: convert::proto_portal_display_state_to_scene(
                                ups.display_state,
                            ),
                        });
                    }
                    Err(_) => {
                        tracing::warn!(
                            tile_id_len = ups.tile_id.len(),
                            "UpdatePortalSurfaceState{log_suffix}: invalid tile_id length \
                             (expected 16 bytes); mutation skipped — SDK bug or wire corruption"
                        );
                    }
                }
            }
            None => {}
        }
    }

    Ok(ConvertedBatch {
        scene_mutations,
        pending_touch_ids,
        pending_touch_names,
    })
}

pub(super) async fn handle_mutation_batch(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
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
                    payload: Some(ServerPayload::RuntimeError(RuntimeError {
                        error_code: "SAFE_MODE_ACTIVE".to_string(),
                        message: "Mutations are not accepted while the runtime is in safe mode."
                            .to_string(),
                        context: String::new(),
                        hint: r#"{"wait_for": "SessionResumed"}"#.to_string(),
                        error_code_enum: ErrorCode::SafeModeActive as i32,
                    })),
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
            // a second MutationResult.
            if !batch.batch_id.is_empty() {
                if let Some(cached) = session.dedup_window.lookup(&batch.batch_id) {
                    let seq = session.next_server_seq();
                    drop(st);
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: batch.batch_id,
                                accepted: cached.accepted,
                                created_ids: cached.created_ids,
                                error_code: cached.error_code,
                                error_message: cached.error_message,
                            })),
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
                                payload: Some(ServerPayload::MutationResult(MutationResult {
                                    batch_id: batch.batch_id,
                                    accepted: true,
                                    created_ids: Vec::new(),
                                    error_code: "MUTATION_QUEUE_PRESSURE".to_string(),
                                    error_message:
                                        "Mutation queue is under pressure (>= 80% capacity)."
                                            .to_string(),
                                })),
                            }))
                            .await;
                    } else {
                        // Send accepted=true (queued — not yet applied, but accepted).
                        let seq = session.next_server_seq();
                        let _ = tx
                            .send(Ok(ServerMessage {
                                sequence: seq,
                                timestamp_wall_us: now_wall_us(),
                                payload: Some(ServerPayload::MutationResult(MutationResult {
                                    batch_id: batch.batch_id,
                                    accepted: true,
                                    created_ids: Vec::new(),
                                    error_code: String::new(),
                                    error_message: String::new(),
                                })),
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
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: batch.batch_id,
                                accepted: true,
                                created_ids: Vec::new(),
                                error_code: String::new(),
                                error_message: String::new(),
                            })),
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
                                error_code: "MUTATION_DROPPED".to_string(),
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
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: evicted_batch_id,
                                accepted: false,
                                created_ids: Vec::new(),
                                error_code: "MUTATION_DROPPED".to_string(),
                                error_message:
                                    "Mutation evicted from queue due to capacity pressure."
                                        .to_string(),
                            })),
                        }))
                        .await;
                    // New batch was queued — send accepted.
                    let seq_new = session.next_server_seq();
                    let _ = tx
                        .send(Ok(ServerMessage {
                            sequence: seq_new,
                            timestamp_wall_us: now_wall_us(),
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: batch.batch_id,
                                accepted: true,
                                created_ids: Vec::new(),
                                error_code: String::new(),
                                error_message: String::new(),
                            })),
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
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: batch.batch_id,
                                accepted: false,
                                created_ids: Vec::new(),
                                error_code: "MUTATION_QUEUE_PRESSURE".to_string(),
                                error_message: "Mutation queue full; backpressure applied."
                                    .to_string(),
                            })),
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
                            payload: Some(ServerPayload::MutationResult(MutationResult {
                                batch_id: batch.batch_id,
                                accepted: false,
                                created_ids: Vec::new(),
                                error_code: "MUTATION_DROPPED".to_string(),
                                error_message: "Ephemeral mutation dropped; queue at capacity."
                                    .to_string(),
                            })),
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
                    payload: Some(ServerPayload::MutationResult(MutationResult {
                        batch_id: batch.batch_id,
                        accepted: cached.accepted,
                        created_ids: cached.created_ids,
                        error_code: cached.error_code,
                        error_message: cached.error_message,
                    })),
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
            let error_code_enum = match error_code {
                "TIMESTAMP_TOO_OLD" => ErrorCode::TimestampTooOld as i32,
                "TIMESTAMP_TOO_FUTURE" => ErrorCode::TimestampTooFuture as i32,
                "TIMESTAMP_EXPIRY_BEFORE_PRESENT" => ErrorCode::TimestampExpiryBeforePresent as i32,
                _ => ErrorCode::InvalidArgument as i32,
            };
            // context points at the specific field that caused the rejection.
            let context = match error_code {
                "TIMESTAMP_EXPIRY_BEFORE_PRESENT" => "timing.expires_at_wall_us",
                _ => "timing.present_at_wall_us",
            };
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::RuntimeError(RuntimeError {
                        error_code: error_code.to_string(),
                        message,
                        context: context.to_string(),
                        hint: r#"{"check_field": "timing"}"#.to_string(),
                        error_code_enum,
                    })),
                }))
                .await;
            return;
        }
    }

    let mut st = state.lock().await;

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
                    payload: Some(ServerPayload::MutationResult(MutationResult {
                        batch_id: batch.batch_id,
                        accepted: false,
                        created_ids: Vec::new(),
                        error_code: cached.error_code,
                        error_message: cached.error_message,
                    })),
                }))
                .await;
            return;
        }
    };

    // Read both active_tab and display_area from the scene in a single lock
    // acquisition to avoid acquiring the scene lock twice in succession.
    let (active_tab_opt, display_area) = {
        let scene = st.scene.lock().await;
        (scene.active_tab, scene.display_area)
    };
    let tab_id = match active_tab_opt {
        Some(id) => id,
        None => {
            let cached = CachedResult {
                accepted: false,
                created_ids: Vec::new(),
                error_code: "PRECONDITION_FAILED".to_string(),
                error_message: "No active tab".to_string(),
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
                    payload: Some(ServerPayload::MutationResult(MutationResult {
                        batch_id: batch.batch_id,
                        accepted: false,
                        created_ids: Vec::new(),
                        error_code: cached.error_code,
                        error_message: cached.error_message,
                    })),
                }))
                .await;
            return;
        }
    };

    // Convert proto mutations to scene mutations (single canonical path shared
    // with the freeze-drain path; only the log suffix differs between the two).
    let converted = match convert_proto_mutations(
        &batch.mutations,
        &st.element_store,
        tab_id,
        lease_id,
        display_area,
        &session.namespace,
        "",
    ) {
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
                    payload: Some(ServerPayload::MutationResult(MutationResult {
                        batch_id: batch.batch_id,
                        accepted: false,
                        created_ids: Vec::new(),
                        error_code: cached.error_code,
                        error_message: cached.error_message,
                    })),
                }))
                .await;
            return;
        }
    };
    let ConvertedBatch {
        scene_mutations,
        pending_touch_ids,
        pending_touch_names,
    } = converted;

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

    // present_at in the future: hold the batch until due (invariant 1). Created
    // ids must come back synchronously, so a scheduled batch cannot create tiles.
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
        let seq = session.next_server_seq();
        drop(st);
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::RuntimeError(RuntimeError {
                    error_code: "PRESENT_AT_WITH_CREATE".to_string(),
                    message: "a batch with a future present_at cannot create tiles".to_string(),
                    context: "timing.present_at_wall_us".to_string(),
                    hint: "create the tile in an immediate batch, then send its content \
                           with present_at"
                        .to_string(),
                    error_code_enum: ErrorCode::InvalidArgument as i32,
                })),
            }))
            .await;
        return;
    }
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
                    error_code: error_code.to_string(),
                    error_message: message,
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
                        payload: Some(ServerPayload::MutationResult(MutationResult {
                            batch_id: batch.batch_id,
                            accepted: false,
                            created_ids: Vec::new(),
                            error_code: cached.error_code,
                            error_message: cached.error_message,
                        })),
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
                payload: Some(ServerPayload::MutationResult(MutationResult {
                    batch_id: batch.batch_id,
                    accepted: true,
                    created_ids: Vec::new(),
                    error_code: String::new(),
                    error_message: String::new(),
                })),
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
        let mut persist_request = persist_created_tile_entries(&mut st, &result.created_ids).await;
        let now = now_ms();
        let mut touched = false;
        for (element_id, element_type) in pending_touch_ids {
            if let Some(entry) = st.element_store.entries.get_mut(&element_id) {
                if entry.element_type == element_type {
                    entry.last_published_at = now;
                    touched = true;
                }
            }
        }
        for (element_type, namespace) in pending_touch_names {
            if let Some(id) = st
                .element_store
                .find_id_by_type_namespace(element_type, namespace.as_str())
            {
                if let Some(entry) = st.element_store.entries.get_mut(&id) {
                    if entry.element_type == element_type {
                        entry.last_published_at = now;
                        touched = true;
                    }
                }
            }
        }
        if touched {
            persist_request =
                st.element_store_path
                    .clone()
                    .map(|path| ElementStorePersistRequest {
                        store: st.element_store.clone(),
                        path,
                    });
        }

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
        persist_element_store(persist_request).await;
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::MutationResult(MutationResult {
                    batch_id: batch.batch_id,
                    accepted: true,
                    created_ids,
                    error_code: String::new(),
                    error_message: String::new(),
                })),
            }))
            .await;
    } else {
        let error_message = result
            .error
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string());

        // Cache rejection result before sending.
        if !batch.batch_id.is_empty() {
            session.dedup_window.insert(
                batch.batch_id.clone(),
                CachedResult {
                    accepted: false,
                    created_ids: Vec::new(),
                    error_code: "MUTATION_REJECTED".to_string(),
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
                payload: Some(ServerPayload::MutationResult(MutationResult {
                    batch_id: batch.batch_id,
                    accepted: false,
                    created_ids: Vec::new(),
                    error_code: "MUTATION_REJECTED".to_string(),
                    error_message,
                })),
            }))
            .await;
    }
}

/// Apply a previously-queued mutation batch to the scene without sending a
/// `MutationResult` response.
///
/// This is called during the unfreeze drain. The initial `MutationResult`
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
    let mut st = state.lock().await;

    let lease_id = match bytes_to_scene_id(&batch.lease_id) {
        Ok(id) => id,
        Err(_) => return false, // invalid lease_id — silently skip (already acked)
    };

    // Read both active_tab and display_area from the scene in a single lock
    // acquisition to avoid acquiring the scene lock twice in succession.
    let (active_tab_opt, display_area) = {
        let scene = st.scene.lock().await;
        (scene.active_tab, scene.display_area)
    };
    let tab_id = match active_tab_opt {
        Some(id) => id,
        None => return false, // no active tab — skip silently
    };

    // Convert proto mutations to scene mutations (single canonical path shared
    // with the live path; the " (queued)" suffix distinguishes drain-path logs).
    let converted = match convert_proto_mutations(
        &batch.mutations,
        &st.element_store,
        tab_id,
        lease_id,
        display_area,
        &session.namespace,
        " (queued)",
    ) {
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
    let ConvertedBatch {
        scene_mutations,
        pending_touch_ids,
        pending_touch_names,
    } = converted;
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

    // present_at in the future: hold the batch until due (invariant 1). Created
    // ids must come back synchronously, so a scheduled batch cannot create tiles.
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
        let mut persist_request = persist_created_tile_entries(&mut st, &result.created_ids).await;
        let now = now_ms();
        let mut touched = false;
        for (element_id, element_type) in pending_touch_ids {
            if let Some(entry) = st.element_store.entries.get_mut(&element_id) {
                if entry.element_type == element_type {
                    entry.last_published_at = now;
                    touched = true;
                }
            }
        }
        for (element_type, namespace) in pending_touch_names {
            if let Some(id) = st
                .element_store
                .find_id_by_type_namespace(element_type, namespace.as_str())
            {
                if let Some(entry) = st.element_store.entries.get_mut(&id) {
                    if entry.element_type == element_type {
                        entry.last_published_at = now;
                        touched = true;
                    }
                }
            }
        }
        if touched {
            persist_request =
                st.element_store_path
                    .clone()
                    .map(|path| ElementStorePersistRequest {
                        store: st.element_store.clone(),
                        path,
                    });
        }
        drop(st);
        persist_element_store(persist_request).await;
        true
    } else {
        false
    }
}

// ─── Helpers used only by this module (migrated from mod.rs, SS-9) ──────────

/// Map proto `batch_id` bytes to a `SceneId` for rejection-correlation semantics.
///
/// If the client supplied a valid 16-byte UUID, use it directly so that any
/// `BatchRejected` or `MutationResult` echoes the client's own `batch_id`.
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

fn resolve_tile_bounds_with_override(
    entry: Option<&ElementStoreEntry>,
    agent_bounds: Option<Rect>,
    display_area: Rect,
) -> Option<Rect> {
    let user_override = entry.and_then(|e| e.geometry_override);
    let agent_requested = agent_bounds.map(|bounds| {
        rect_to_relative_geometry_policy(bounds, display_area.width, display_area.height)
    });
    resolve_geometry_override_chain(user_override, agent_requested, None, None).map(|policy| {
        geometry_policy_to_absolute_rect(policy, display_area.width, display_area.height)
    })
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
