//! Input and interaction message handlers (RFC 0005 §3.8, RFC 0004 §8.3.1).
//!
//! Extracted from `session_server/mod.rs` in SS-7f.
//! Contains focus-request and pointer-capture handlers, plus the
//! private helpers that are exclusively consumed by those handlers.

use super::*;

// ─── Private helpers ─────────────────────────────────────────────────────────

fn parse_input_device_id(device_id: &str) -> Result<u32, String> {
    if device_id.trim().is_empty() {
        return Ok(0);
    }
    device_id
        .parse::<u32>()
        .map_err(|_| format!("invalid pointer device_id '{device_id}'"))
}

pub(super) fn scene_node_contains(
    scene: &tze_hud_scene::SceneGraph,
    root: tze_hud_scene::SceneId,
    target: tze_hud_scene::SceneId,
) -> bool {
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        if current == target {
            return true;
        }
        if let Some(node) = scene.nodes.get(&current) {
            stack.extend(node.children.iter().copied());
        }
    }
    false
}

async fn send_input_capture_invalid_argument(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    message: String,
    context: &'static str,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::RequestResult(super::verbs::fail(
                0,
                "INVALID_ARGUMENT",
                format!("{context}: {message}"),
            ))),
        }))
        .await;
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// Handle an InputFocusRequest from the client (RFC 0005 §3.8, RFC 0004 §8.3.1).
///
/// Synchronous: runtime responds with InputFocusResponse correlated by sequence.
/// v1 grants focus unconditionally (focus arbitration deferred to post-v1).
pub(super) async fn handle_input_focus_request(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    req: InputFocusRequest,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::InputFocusResponse(InputFocusResponse {
                tile_id: req.tile_id.clone(),
                granted: true,
                reason: String::new(),
            })),
        }))
        .await;
}

/// Handle an InputCaptureRequest from the client (RFC 0005 §3.8, RFC 0004 §8.3.1).
///
/// Synchronous: runtime responds with InputCaptureResponse correlated by sequence.
/// Windowed runtime queues pointer capture into the local input processor; test
/// and headless services without a capture bridge retain legacy no-op grants.
pub(super) async fn handle_input_capture_request(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    req: InputCaptureRequest,
) {
    let mut granted = true;
    let mut reason = String::new();

    let (input_capture_tx, input_capture_wake, scene) = {
        let st = state.lock().await;
        (
            st.input_capture_tx.clone(),
            st.input_capture_wake.clone(),
            st.scene.clone(),
        )
    };

    if let Some(input_capture_tx) = input_capture_tx {
        if req.device_kind != "pointer" && req.device_kind != "touch" {
            granted = false;
            reason = format!("unsupported capture device_kind '{}'", req.device_kind);
        } else if req.node_id.is_empty() {
            granted = false;
            reason = "node_id is required for runtime pointer capture".to_string();
        } else {
            let tile_id = bytes_to_scene_id(&req.tile_id);
            let node_id = bytes_to_scene_id(&req.node_id);
            let device_id = parse_input_device_id(&req.device_id);
            match (tile_id, node_id, device_id) {
                (Ok(tile_id), Ok(node_id), Ok(device_id)) => {
                    let scene_guard = scene.lock().await;
                    let target_valid = scene_guard
                        .tiles
                        .get(&tile_id)
                        .map(|tile| {
                            tile.namespace == session.namespace
                                && tile
                                    .root_node
                                    .map(|root| scene_node_contains(&scene_guard, root, node_id))
                                    .unwrap_or(false)
                        })
                        .unwrap_or(false)
                        && matches!(
                            scene_guard.nodes.get(&node_id).map(|n| &n.data),
                            Some(tze_hud_scene::NodeData::HitRegion(_))
                        );
                    drop(scene_guard);

                    if !target_valid {
                        granted = false;
                        reason = "capture target tile/node was not found".to_string();
                    } else if input_capture_tx
                        .send(crate::session::InputCaptureCommand::Request {
                            tile_id,
                            node_id,
                            device_id,
                            release_on_up: req.release_on_up,
                        })
                        .is_err()
                    {
                        granted = false;
                        reason = "runtime input capture bridge is unavailable".to_string();
                    } else {
                        input_capture_wake.notify();
                    }
                }
                (Err(e), _, _) | (_, Err(e), _) => {
                    granted = false;
                    reason = e.message().to_string();
                }
                (_, _, Err(e)) => {
                    granted = false;
                    reason = e;
                }
            }
        }
    }

    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::InputCaptureResponse(InputCaptureResponse {
                tile_id: req.tile_id.clone(),
                granted,
                device_kind: req.device_kind.clone(),
                reason,
            })),
        }))
        .await;
}

/// Handle an InputCaptureRelease from the client (RFC 0005 §3.8, RFC 0004 §8.3.1).
///
/// Asynchronous: confirmed by CaptureReleasedEvent in the next EventBatch (field 34).
/// No synchronous response is sent. The event is delivered with reason=AGENT_RELEASED.
/// Windowed runtime releases capture through the local input processor; services
/// without a capture bridge retain the legacy synthetic confirmation.
pub(super) async fn handle_input_capture_release(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    rel: InputCaptureRelease,
) {
    use crate::proto::CaptureReleasedReason;
    use crate::proto::input_envelope::Event as InputEvent;
    use crate::proto::{CaptureReleasedEvent, EventBatch, InputEnvelope};

    let (input_capture_tx, input_capture_wake) = {
        let st = state.lock().await;
        (st.input_capture_tx.clone(), st.input_capture_wake.clone())
    };

    if let Some(input_capture_tx) = input_capture_tx {
        let device_id = match parse_input_device_id(&rel.device_id) {
            Ok(device_id) => device_id,
            Err(e) => {
                send_input_capture_invalid_argument(
                    session,
                    tx,
                    e,
                    "input_capture_release.device_id",
                )
                .await;
                return;
            }
        };
        if input_capture_tx
            .send(crate::session::InputCaptureCommand::Release { device_id })
            .is_ok()
        {
            input_capture_wake.notify();
        }
        return;
    }

    // Only deliver the CaptureReleasedEvent if the agent is subscribed to FOCUS_EVENTS.
    // CaptureReleasedEvent is a focus variant (RFC 0005 §7.1).
    if !session
        .subscriptions
        .iter()
        .any(|s| s == subscriptions::category::FOCUS_EVENTS)
    {
        // Agent not subscribed to FOCUS_EVENTS; do not deliver CaptureReleasedEvent.
        // The release is still processed (capture is released from the runtime side).
        return;
    }

    let now_us = now_wall_us();
    let timestamp_mono_us = now_mono_us().max(1);
    let seq = session.next_server_seq();

    let capture_released = CaptureReleasedEvent {
        tile_id: rel.tile_id.clone(),
        node_id: Vec::new(),
        timestamp_mono_us,
        device_id: rel.device_kind.clone(),
        reason: CaptureReleasedReason::AgentReleased as i32,
    };

    let batch = EventBatch {
        frame_number: 0, // synthetic batch (not tied to compositor frame)
        batch_ts_us: now_us,
        events: vec![InputEnvelope {
            event: Some(InputEvent::CaptureReleased(capture_released)),
        }],
    };

    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_us,
            payload: Some(ServerPayload::EventBatch(batch)),
        }))
        .await;
}
