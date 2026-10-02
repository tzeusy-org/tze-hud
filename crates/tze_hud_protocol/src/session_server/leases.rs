//! Lease handlers for the session server (RFC 0005 §3.2, §5.3; lease-governance spec).
//!
//! This module contains the three lease lifecycle handlers:
//! - `handle_lease_request`: grant a new lease scoped to the session's permissions.
//! - `handle_lease_renew`: extend the TTL of an existing lease.
//! - `handle_lease_release`: revoke an existing lease.
//!
//! All three handlers implement the retransmit-dedup contract (RFC 0005 §5.3)
//! via `session.lease_correlation_cache`.

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;

use crate::lease::CachedLeaseResponse;
use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;
use tze_hud_scene::types::Capability;

use super::stream_session::StreamSession;
use super::{bytes_to_scene_id, canonical_name_to_capability, now_wall_us, scene_id_to_bytes};

/// Default internal lease priority. Agents no longer choose a priority; chrome
/// is structurally above agent content and ties go to claim order.
const AGENT_LEASE_PRIORITY: u8 = 2;

/// Expand the session's permission strings to scene lease capabilities.
fn lease_capabilities(permissions: &[String]) -> Vec<Capability> {
    if permissions.iter().any(|p| p == "*") {
        return vec![
            Capability::CreateTiles,
            Capability::ModifyOwnTiles,
            Capability::ManageTabs,
            Capability::UploadResource,
            Capability::ReadSceneTopology,
            Capability::AccessInputEvents,
            Capability::ReadTelemetry,
            Capability::ResidentMcp,
            Capability::PublishZone("*".to_string()),
            Capability::PublishWidget("*".to_string()),
        ];
    }
    permissions
        .iter()
        .filter_map(|p| canonical_name_to_capability(p))
        .collect()
}

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

    let capabilities = lease_capabilities(&session.capabilities);

    let ttl = if req.ttl_ms > 0 { req.ttl_ms } else { 60_000 };

    let lease_result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        scene.try_grant_lease_for_session_with_budget(
            &session.namespace,
            session.scene_session_id,
            ttl,
            AGENT_LEASE_PRIORITY,
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
        scene.renew_lease(lease_id, ttl)
    };
    if renew_result.is_ok() {
        render_wake.notify();
    }

    match renew_result {
        Ok(()) => {
            // Spec: "runtime SHALL respond with LeaseResponse" for lease operations.
            // For renewal success, return LeaseResponse(granted=true) with the updated TTL.
            let seq = session.next_server_seq();
            let lease_response = LeaseResponse {
                granted: true,
                lease_id: lease_id_bytes.clone(),
                granted_ttl_ms: ttl,
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
