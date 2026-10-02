//! Zone-publish handler for the session server.
//!
//! This module contains `handle_zone_publish`, which processes a `ZonePublish`
//! message from the client and routes it through the scene-graph mutation path.
//!
//! Durable-zone publishes are transactional and receive a `ZonePublishResult` ack.
//! Ephemeral-zone publishes are fire-and-forget; no `ZonePublishResult` is sent.

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::element_store::ElementType;

use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;

use super::stream_session::StreamSession;
use super::{
    bytes_to_scene_id, capability_set_covers, now_ms, now_wall_us, persist_element_store,
    touch_element_store_entry_by_id, touch_element_store_entry_by_namespace,
};

/// Handle a ZonePublish from the client. The agent's allow list must cover
/// the zone.
///
/// Durable-zone publishes are transactional and receive a ZonePublishResult.
/// Ephemeral-zone publishes are fire-and-forget; no ZonePublishResult is sent.
///
/// Zone durability is determined by `ZoneDefinition.ephemeral`:
/// - `false` (default): durable → sends ZonePublishResult ack.
/// - `true`: ephemeral → fire-and-forget, no ZonePublishResult.
pub(super) async fn handle_zone_publish(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    request_sequence: u64,
    publish: ZonePublish,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) -> bool {
    // Apply the zone publish through the scene graph mutation path.
    // Also determine zone durability (ephemeral vs durable) for ack decision.
    let (accepted, error_code, error_message, is_ephemeral_zone, persist_request) = {
        let mut st = state.lock().await;
        let (resolved_zone_name, resolved_element_id, preflight_error) =
            if !publish.element_id.is_empty() {
                match bytes_to_scene_id(&publish.element_id) {
                    Ok(element_id) => match st.element_store.entries.get(&element_id) {
                        Some(entry) if entry.element_type == ElementType::Zone => {
                            (entry.namespace.clone(), Some(element_id), None)
                        }
                        _ => (
                            String::new(),
                            None,
                            Some((
                                false,
                                "ELEMENT_NOT_FOUND".to_string(),
                                "element_id does not reference a known zone".to_string(),
                                false,
                                None,
                            )),
                        ),
                    },
                    Err(_) => (
                        String::new(),
                        None,
                        Some((
                            false,
                            "INVALID_ARGUMENT".to_string(),
                            "invalid element_id: expected 16 bytes".to_string(),
                            false,
                            None,
                        )),
                    ),
                }
            } else {
                (publish.zone_name.clone(), None, None)
            };

        if let Some(preflight_error) = preflight_error {
            preflight_error
        } else {
            let mut scene = st.scene.lock().await;

            // Check zone durability before applying the mutation
            let zone_is_ephemeral = scene
                .zone_registry
                .get_by_name(&resolved_zone_name)
                .map(|def| def.ephemeral)
                .unwrap_or(false); // Unknown zones default to durable (will fail below)

            let content = publish
                .content
                .as_ref()
                .and_then(crate::convert::proto_zone_content_to_scene);

            let allowed = capability_set_covers(
                &session.capabilities,
                &format!("publish_zone:{resolved_zone_name}"),
            );

            if !allowed {
                (
                    false,
                    "CAPABILITY_MISSING".to_string(),
                    format!("agent allow list lacks zone:{resolved_zone_name}"),
                    zone_is_ephemeral,
                    None,
                )
            } else if let Some(content) = content {
                // Validate: breakpoints are only meaningful for StreamText content.
                if !publish.breakpoints.is_empty()
                    && !matches!(content, tze_hud_scene::types::ZoneContent::StreamText(_))
                {
                    (
                        false,
                        "INVALID_ARGUMENT".to_string(),
                        "breakpoints are only valid for StreamText content".to_string(),
                        zone_is_ephemeral,
                        None,
                    )
                } else if let Some(timing_error) = zone_timing_error(
                    &publish,
                    scene.now_wall_us(),
                    &resolved_zone_name,
                    scene
                        .zone_registry
                        .get_by_name(&resolved_zone_name)
                        .is_some(),
                ) {
                    let (code, message) = timing_error;
                    (false, code, message, zone_is_ephemeral, None)
                } else {
                    let merge_key = if publish.merge_key.is_empty() {
                        None
                    } else {
                        Some(publish.merge_key.clone())
                    };

                    let mutation = tze_hud_scene::mutation::SceneMutation::PublishToZone {
                        zone_name: resolved_zone_name.clone(),
                        content,
                        publish_token: tze_hud_scene::types::ZonePublishToken { token: Vec::new() },
                        merge_key,
                        // Content expiry (invariant 1): an absolute expires_at wins,
                        // else ttl_us counts from presentation; 0 for both means none.
                        expires_at_wall_us: zone_expires_at_wall_us(&publish, scene.now_wall_us()),
                        content_classification: None,
                        // Wire breakpoints from the ZonePublish proto for StreamText streaming reveal.
                        // Per spec §Subtitle Streaming Word-by-Word Reveal.
                        breakpoints: publish.breakpoints.clone(),
                    };

                    // Apply as a single-mutation batch.
                    let zone_publish_lease_id = session.lease_ids.first().copied();
                    let batch = tze_hud_scene::mutation::MutationBatch {
                        batch_id: tze_hud_scene::SceneId::new(),
                        agent_namespace: session.namespace.clone(),
                        mutations: vec![mutation],
                        timing_hints: None,
                        lease_id: zone_publish_lease_id,
                    };
                    // present_at in the future: hold the publish until due.
                    let present_at = publish.present_at_wall_us;
                    let result = if present_at > scene.now_wall_us() {
                        scene.schedule_batch(present_at, batch.clone());
                        tze_hud_scene::mutation::MutationResult {
                            batch_id: batch.batch_id,
                            applied: true,
                            created_ids: Vec::new(),
                            error: None,
                            rejection: None,
                            budget_warning: false,
                            sequence_number: None,
                        }
                    } else {
                        scene.apply_batch(&batch)
                    };
                    drop(scene);
                    if result.applied {
                        let now = now_ms();
                        let persist_request = if let Some(element_id) = resolved_element_id {
                            touch_element_store_entry_by_id(
                                &mut st,
                                element_id,
                                ElementType::Zone,
                                now,
                            )
                        } else {
                            touch_element_store_entry_by_namespace(
                                &mut st,
                                ElementType::Zone,
                                &resolved_zone_name,
                                now,
                            )
                        };
                        (
                            true,
                            String::new(),
                            String::new(),
                            zone_is_ephemeral,
                            persist_request,
                        )
                    } else {
                        let (code, msg) = match &result.error {
                            Some(tze_hud_scene::ValidationError::ZoneNotFound { name }) => (
                                "ZONE_NOT_FOUND".to_string(),
                                format!("Zone not found: {name}"),
                            ),
                            Some(tze_hud_scene::ValidationError::ZonePublishTokenInvalid {
                                zone,
                            }) => (
                                "TOKEN_INVALID".to_string(),
                                format!("Publish token invalid for zone '{zone}'"),
                            ),
                            Some(tze_hud_scene::ValidationError::BudgetExceeded { resource }) => (
                                "BUDGET_EXCEEDED".to_string(),
                                format!("Budget exceeded: {resource}"),
                            ),
                            Some(err) => ("ZONE_PUBLISH_FAILED".to_string(), err.to_string()),
                            None => (
                                "ZONE_PUBLISH_FAILED".to_string(),
                                "Zone publish failed".to_string(),
                            ),
                        };
                        (false, code, msg, zone_is_ephemeral, None)
                    }
                }
            } else {
                (
                    false,
                    "INVALID_CONTENT".to_string(),
                    "Missing or invalid zone content".to_string(),
                    zone_is_ephemeral,
                    None,
                )
            }
        }
    };

    if accepted {
        render_wake.notify();
    }
    persist_element_store(persist_request).await;

    // Durable zones: send ZonePublishResult (transactional ack).
    // Ephemeral zones: fire-and-forget — no ZonePublishResult sent, even on failure
    // (RFC 0005 §8.6: "Ephemeral-zone publishes SHALL be fire-and-forget").
    if !is_ephemeral_zone {
        let seq = session.next_server_seq();
        let _ = tx
            .send(Ok(ServerMessage {
                sequence: seq,
                timestamp_wall_us: now_wall_us(),
                payload: Some(ServerPayload::ZonePublishResult(ZonePublishResult {
                    request_sequence,
                    accepted,
                    error_code,
                    error_message,
                })),
            }))
            .await;
    }
    // Ephemeral zone: no ack sent (fire-and-forget per RFC 0005 §8.6), success or failure
    accepted
}

/// Absolute content expiry for a zone publish: `expires_at_wall_us` if set,
/// else `ttl_us` counted from presentation (now, or `present_at` if later),
/// else none.
fn zone_expires_at_wall_us(publish: &ZonePublish, now_wall_us: u64) -> Option<u64> {
    if publish.expires_at_wall_us > 0 {
        Some(publish.expires_at_wall_us)
    } else if publish.ttl_us > 0 {
        let shown_at = publish.present_at_wall_us.max(now_wall_us);
        Some(shown_at.saturating_add(publish.ttl_us))
    } else {
        None
    }
}

/// Reject zone publish timing that can never display, and scheduled
/// publishes to unknown zones (they would otherwise fail silently when due).
fn zone_timing_error(
    publish: &ZonePublish,
    now_wall_us: u64,
    zone_name: &str,
    zone_exists: bool,
) -> Option<(String, String)> {
    let present = publish.present_at_wall_us;
    if present > now_wall_us.saturating_add(super::DEFAULT_MAX_FUTURE_SCHEDULE_US) {
        return Some((
            "TIMESTAMP_TOO_FUTURE".to_string(),
            format!("present_at_wall_us ({present}) is beyond the scheduling horizon"),
        ));
    }
    if let Some(expires) = zone_expires_at_wall_us(publish, now_wall_us)
        && expires <= present.max(now_wall_us)
    {
        return Some((
            "TIMESTAMP_EXPIRY_BEFORE_PRESENT".to_string(),
            format!("expires_at_wall_us ({expires}) must be after the presentation time"),
        ));
    }
    if present > now_wall_us && !zone_exists {
        return Some((
            "ZONE_NOT_FOUND".to_string(),
            format!("Zone not found: {zone_name}"),
        ));
    }
    None
}
