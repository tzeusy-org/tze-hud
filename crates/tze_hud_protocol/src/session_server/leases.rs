//! Lease handlers for the session server (RFC 0005 §3.2, §5.3; lease-governance spec).
//!
//! This module contains the three lease lifecycle handlers:
//! - `handle_lease_request`: grant a new lease with priority + capability scope.
//! - `handle_lease_renew`: extend the TTL of an existing lease.
//! - `handle_lease_release`: revoke an existing lease.
//!
//! All three handlers implement the retransmit-dedup contract (RFC 0005 §5.3)
//! via `session.lease_correlation_cache`.

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;

use crate::auth::validate_canonical_capabilities;
use crate::lease::{CachedLeaseResponse, effective_priority};
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;
use tze_hud_scene::types::Capability;

use super::stream_session::StreamSession;
use super::{
    bytes_to_scene_id, canonical_name_to_capability, capability_set_covers, now_wall_us,
    scene_id_to_bytes,
};

pub(super) async fn handle_lease_request(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    client_sequence: u64,
    req: LeaseRequest,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> bool {
    // Retransmit dedup (RFC 0005 §5.3): if we have already processed this
    // client sequence, replay the cached response.
    if client_sequence > 0 {
        if let Some(cached) = session
            .lease_correlation_cache
            .get(client_sequence)
            .cloned()
        {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: cached.granted,
                        lease_id: cached.lease_id,
                        granted_ttl_ms: cached.granted_ttl_ms,
                        granted_priority: cached.granted_priority,
                        granted_capabilities: cached.granted_capabilities,
                        deny_reason: cached.deny_reason,
                        deny_code: cached.deny_code,
                        result: if cached.granted {
                            LeaseResult::Granted as i32
                        } else {
                            LeaseResult::Denied as i32
                        },
                    })),
                }))
                .await;
            return false;
        }
    }

    // Validate requested capabilities against the canonical v1 vocabulary.
    // Non-canonical names (including legacy names like create_tile, receive_input)
    // must be rejected with CONFIG_UNKNOWN_CAPABILITY and a hint.
    if let Err(unknown_caps) = validate_canonical_capabilities(&req.capabilities) {
        let hints: Vec<serde_json::Value> = unknown_caps
            .iter()
            .map(|e| serde_json::json!({"unknown": e.unknown, "hint": e.hint}))
            .collect();
        let hint_json = serde_json::to_string(&hints)
            .unwrap_or_else(|_| "see configuration/spec.md §Capability Vocabulary".to_string());
        let deny_reason = format!("{} unrecognized capability name(s)", unknown_caps.len());
        // Cache the denial so retransmits replay a stable response without
        // duplicating the RuntimeError advisory (RFC 0005 §5.3 dedup contract).
        if client_sequence > 0 {
            session.lease_correlation_cache.insert(
                client_sequence,
                CachedLeaseResponse {
                    granted: false,
                    lease_id: Vec::new(),
                    granted_ttl_ms: 0,
                    granted_priority: 0,
                    granted_capabilities: Vec::new(),
                    deny_reason: deny_reason.clone(),
                    deny_code: "CONFIG_UNKNOWN_CAPABILITY".to_string(),
                },
            );
        }
        let seq = session.next_server_seq();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                    granted: false,
                    deny_code: "CONFIG_UNKNOWN_CAPABILITY".to_string(),
                    deny_reason,
                    result: LeaseResult::Denied as i32,
                    ..Default::default()
                })),
            }))
            .await;
        // Send structured hints as a RuntimeError advisory.
        // LeaseResponse has no hint field; the advisory carries the JSON hint array
        // so agents can identify which names are non-canonical and what to use instead.
        let hint_seq = session.next_server_seq();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: hint_seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::RuntimeError(RuntimeError {
                    error_code: "CONFIG_UNKNOWN_CAPABILITY".to_string(),
                    message: format!(
                        "LeaseRequest contains {} unrecognized capability name(s)",
                        unknown_caps.len()
                    ),
                    hint: hint_json,
                    ..Default::default()
                })),
            }))
            .await;
        return false;
    }

    // Lease capability scope must stay within the session's currently granted
    // authority surface. Do not silently clamp to a subset: deny the full
    // request when any requested capability is out of scope.
    let unauthorized_caps: Vec<String> = req
        .capabilities
        .iter()
        .filter(|requested| !capability_set_covers(&session.capabilities, requested))
        .cloned()
        .collect();
    if !unauthorized_caps.is_empty() {
        let deny_reason = format!(
            "requested lease scope exceeds session-granted capabilities: {}",
            unauthorized_caps.join(", ")
        );
        let deny_code = "PERMISSION_DENIED".to_string();
        if client_sequence > 0 {
            session.lease_correlation_cache.insert(
                client_sequence,
                CachedLeaseResponse {
                    granted: false,
                    lease_id: Vec::new(),
                    granted_ttl_ms: 0,
                    granted_priority: 0,
                    granted_capabilities: Vec::new(),
                    deny_reason: deny_reason.clone(),
                    deny_code: deny_code.clone(),
                },
            );
        }
        let seq = session.next_server_seq();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                    granted: false,
                    deny_reason,
                    deny_code,
                    result: LeaseResult::Denied as i32,
                    ..Default::default()
                })),
            }))
            .await;
        return false;
    }

    let granted_capabilities: Vec<String> = req.capabilities.clone();
    let capabilities: Vec<Capability> = granted_capabilities
        .iter()
        .filter_map(|c| canonical_name_to_capability(c))
        .collect();

    let ttl = if req.ttl_ms > 0 { req.ttl_ms } else { 60_000 };

    // Enforce priority rules per lease-governance spec §Priority Assignment.
    let granted_priority = effective_priority(req.lease_priority, &session.capabilities);

    // Persist the effective priority in the scene graph lease record so that the
    // degradation ladder and arbitration engine can sort tiles by
    // (lease_priority ASC, z_order DESC) without consulting the session layer.
    // Spec §Requirement: Priority Sort Semantics (lease-governance/spec.md lines 62-69).
    // `effective_priority` returns u32 (wire type); priority values are 0-4 so the
    // conversion to u8 is always lossless.
    let priority_u8 = granted_priority as u8;
    let lease_result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        scene.try_grant_lease_for_session_with_budget(
            &session.namespace,
            session.scene_session_id,
            ttl,
            priority_u8,
            capabilities,
            session.resource_budget.clone(),
        )
    };
    let lease_id = match lease_result {
        Ok(lease_id) => lease_id,
        Err(error) => {
            let deny_reason = error.to_string();
            let deny_code = "RESOURCE_EXHAUSTED".to_string();
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: false,
                        lease_id: Vec::new(),
                        granted_ttl_ms: 0,
                        granted_priority: 0,
                        granted_capabilities: Vec::new(),
                        deny_reason: deny_reason.clone(),
                        deny_code: deny_code.clone(),
                    },
                );
            }
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: false,
                        deny_reason,
                        deny_code,
                        result: LeaseResult::Denied as i32,
                        ..Default::default()
                    })),
                }))
                .await;
            return false;
        }
    };
    render_wake.notify();
    session.lease_ids.push(lease_id);
    let lease_id_bytes = scene_id_to_bytes(lease_id);

    // Cache the response for retransmit handling (RFC 0005 §5.3).
    if client_sequence > 0 {
        session.lease_correlation_cache.insert(
            client_sequence,
            CachedLeaseResponse {
                granted: true,
                lease_id: lease_id_bytes.clone(),
                granted_ttl_ms: ttl,
                granted_priority,
                granted_capabilities: granted_capabilities.clone(),
                deny_reason: String::new(),
                deny_code: String::new(),
            },
        );
    }

    // Send LeaseResponse (transactional: never dropped, RFC 0005 §3.1).
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                granted: true,
                lease_id: lease_id_bytes.clone(),
                granted_ttl_ms: ttl,
                granted_priority,
                granted_capabilities,
                result: LeaseResult::Granted as i32,
                ..Default::default()
            })),
        }))
        .await;

    true
}

pub(super) async fn handle_lease_renew(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    client_sequence: u64,
    renew: LeaseRenew,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> bool {
    // Retransmit dedup (RFC 0005 §5.3).
    if client_sequence > 0 {
        if let Some(cached) = session
            .lease_correlation_cache
            .get(client_sequence)
            .cloned()
        {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: cached.granted,
                        lease_id: cached.lease_id,
                        granted_ttl_ms: cached.granted_ttl_ms,
                        granted_priority: cached.granted_priority,
                        granted_capabilities: cached.granted_capabilities,
                        deny_reason: cached.deny_reason,
                        deny_code: cached.deny_code,
                        result: if cached.granted {
                            LeaseResult::Granted as i32
                        } else {
                            LeaseResult::Denied as i32
                        },
                    })),
                }))
                .await;
            return false;
        }
    }

    let lease_id = match bytes_to_scene_id(&renew.lease_id) {
        Ok(id) => id,
        Err(_) => {
            let seq = session.next_server_seq();
            let deny_reason = "Invalid lease_id bytes".to_string();
            let deny_code = "INVALID_ARGUMENT".to_string();
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: false,
                        lease_id: Vec::new(),
                        granted_ttl_ms: 0,
                        granted_priority: 0,
                        granted_capabilities: Vec::new(),
                        deny_reason: deny_reason.clone(),
                        deny_code: deny_code.clone(),
                    },
                );
            }
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: false,
                        deny_reason,
                        deny_code,
                        result: LeaseResult::Denied as i32,
                        ..Default::default()
                    })),
                }))
                .await;
            return false;
        }
    };

    let ttl = if renew.new_ttl_ms > 0 {
        renew.new_ttl_ms
    } else {
        60_000
    };
    let lease_id_bytes = scene_id_to_bytes(lease_id);

    let renew_result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let result = scene.renew_lease(lease_id, ttl);
        // Read the stored priority while we hold the scene lock.
        let stored_priority = scene
            .leases
            .get(&lease_id)
            .map(|l| l.priority as u32)
            .unwrap_or(2);
        result.map(|()| stored_priority)
    };
    if renew_result.is_ok() {
        render_wake.notify();
    }

    match renew_result {
        Ok(stored_priority) => {
            // Spec: "runtime SHALL respond with LeaseResponse" for lease operations.
            // For renewal success, return LeaseResponse(granted=true) with the updated TTL.
            // Read the stored priority from the scene graph so the renewal response reflects
            // the persisted value (lease-governance spec §Requirement: Priority Assignment,
            // lines 49-60: renewal preserves the priority set at grant time).
            let seq = session.next_server_seq();
            let lease_response = LeaseResponse {
                granted: true,
                lease_id: lease_id_bytes.clone(),
                granted_ttl_ms: ttl,
                granted_priority: stored_priority,
                result: LeaseResult::Granted as i32,
                ..Default::default()
            };
            // Cache exactly what we send, so retransmit replays the same response.
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: lease_response.granted,
                        lease_id: lease_response.lease_id.clone(),
                        granted_ttl_ms: lease_response.granted_ttl_ms,
                        granted_priority: lease_response.granted_priority,
                        granted_capabilities: lease_response.granted_capabilities.clone(),
                        deny_reason: lease_response.deny_reason.clone(),
                        deny_code: lease_response.deny_code.clone(),
                    },
                );
            }
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(lease_response)),
                }))
                .await;

            true
        }
        Err(e) => {
            let seq = session.next_server_seq();
            let deny_reason = e.to_string();
            let deny_code = "LEASE_NOT_FOUND".to_string();
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: false,
                        lease_id: Vec::new(),
                        granted_ttl_ms: 0,
                        granted_priority: 0,
                        granted_capabilities: Vec::new(),
                        deny_reason: deny_reason.clone(),
                        deny_code: deny_code.clone(),
                    },
                );
            }
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: false,
                        deny_reason,
                        deny_code,
                        result: LeaseResult::Denied as i32,
                        ..Default::default()
                    })),
                }))
                .await;
            false
        }
    }
}

pub(super) async fn handle_lease_release(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    client_sequence: u64,
    release: LeaseRelease,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> bool {
    // Retransmit dedup (RFC 0005 §5.3).
    // Replay the cached LeaseResponse for both success and denial paths so the
    // client always receives a LeaseResponse on retransmit (consistent with the
    // original send).
    if client_sequence > 0 {
        if let Some(cached) = session
            .lease_correlation_cache
            .get(client_sequence)
            .cloned()
        {
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: cached.granted,
                        lease_id: cached.lease_id,
                        granted_ttl_ms: cached.granted_ttl_ms,
                        granted_priority: cached.granted_priority,
                        granted_capabilities: cached.granted_capabilities,
                        deny_reason: cached.deny_reason,
                        deny_code: cached.deny_code,
                        result: if cached.granted {
                            LeaseResult::Released as i32
                        } else {
                            LeaseResult::Denied as i32
                        },
                    })),
                }))
                .await;
            return false;
        }
    }

    let lease_id = match bytes_to_scene_id(&release.lease_id) {
        Ok(id) => id,
        Err(_) => {
            let seq = session.next_server_seq();
            let deny_reason = "Invalid lease_id bytes".to_string();
            let deny_code = "INVALID_ARGUMENT".to_string();
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: false,
                        lease_id: Vec::new(),
                        granted_ttl_ms: 0,
                        granted_priority: 0,
                        granted_capabilities: Vec::new(),
                        deny_reason: deny_reason.clone(),
                        deny_code: deny_code.clone(),
                    },
                );
            }
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: false,
                        deny_reason,
                        deny_code,
                        result: LeaseResult::Denied as i32,
                        ..Default::default()
                    })),
                }))
                .await;
            return false;
        }
    };

    let lease_id_bytes = scene_id_to_bytes(lease_id);

    let revoke_result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        scene.revoke_lease(lease_id)
    };

    match revoke_result {
        Ok(()) => {
            render_wake.notify();
            // Remove from session's tracked leases
            session.lease_ids.retain(|&id| id != lease_id);

            // Every lease operation is answered with exactly one LeaseResponse.
            let release_response = LeaseResponse {
                granted: true,
                lease_id: lease_id_bytes.clone(),
                result: LeaseResult::Released as i32,
                ..Default::default()
            };
            // Cache the LeaseResponse so retransmits replay it.
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: release_response.granted,
                        lease_id: release_response.lease_id.clone(),
                        granted_ttl_ms: release_response.granted_ttl_ms,
                        granted_priority: release_response.granted_priority,
                        granted_capabilities: release_response.granted_capabilities.clone(),
                        deny_reason: release_response.deny_reason.clone(),
                        deny_code: release_response.deny_code.clone(),
                    },
                );
            }
            let seq = session.next_server_seq();
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(release_response)),
                }))
                .await;

            true
        }
        Err(e) => {
            let seq = session.next_server_seq();
            let deny_reason = e.to_string();
            let deny_code = "LEASE_NOT_FOUND".to_string();
            if client_sequence > 0 {
                session.lease_correlation_cache.insert(
                    client_sequence,
                    CachedLeaseResponse {
                        granted: false,
                        lease_id: Vec::new(),
                        granted_ttl_ms: 0,
                        granted_priority: 0,
                        granted_capabilities: Vec::new(),
                        deny_reason: deny_reason.clone(),
                        deny_code: deny_code.clone(),
                    },
                );
            }
            let _ = tx
                .send(Ok(ServerMessage {
                    sequence: seq,
                    timestamp_wall_us: now_wall_us(),
                    payload: Some(ServerPayload::LeaseResponse(LeaseResponse {
                        granted: false,
                        deny_reason,
                        deny_code,
                        result: LeaseResult::Denied as i32,
                        ..Default::default()
                    })),
                }))
                .await;
            false
        }
    }
}
