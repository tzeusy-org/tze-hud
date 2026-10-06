use std::sync::Arc;
use std::time::Instant;

use winit::event::MouseScrollDelta;

use crate::channels::{INPUT_EVENT_CAPACITY, InputEvent};
use tze_hud_protocol::proto::{EventBatch, InputEnvelope};
use tze_hud_scene::MonoUs;
use tze_hud_scene::graph::SceneGraph;

pub(super) fn normalize_mouse_wheel_delta(delta: &MouseScrollDelta) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (-x * 40.0, -y * 40.0),
        MouseScrollDelta::PixelDelta(pos) => (-(pos.x as f32), -(pos.y as f32)),
    }
}

/// Dispatch a `ScrollOffsetChangedEvent` to the tile-owning agent.
///
/// Looks up the owning namespace from the scene graph, constructs an
/// `EventBatch` with a single `ScrollOffsetChangedEvent` envelope, and sends it
/// through the traffic-class-aware input-event bus. The session handler delivers the
/// batch only when the agent is subscribed to `INPUT_EVENTS` — the subscription
/// gate is enforced in `subscriptions::filter_event_batch`, not here.
///
/// This is a best-effort dispatch (non-blocking, try_send semantics): if no
/// receiver is connected (gRPC disabled, no agent subscribed) the event is
/// silently dropped, matching the ephemeral-realtime message class contract.
pub(super) fn dispatch_scroll_offset_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    scene: &SceneGraph,
    ev: tze_hud_input::ScrollOffsetChangedEvent,
) {
    // Look up the namespace that owns this tile so the session handler can
    // route the batch to the correct agent.
    let Some(namespace) = scene.tiles.get(&ev.tile_id).map(|t| t.namespace.clone()) else {
        return;
    };
    dispatch_scroll_offset_event_to_namespace(tx, namespace, ev);
}

/// Dispatch a scroll-offset notification when the owning namespace was
/// already resolved under the scene lock.
pub(super) fn dispatch_scroll_offset_event_to_namespace(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    namespace: String,
    ev: tze_hud_input::ScrollOffsetChangedEvent,
) {
    let Some(tx) = tx else { return };

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let timestamp_mono_us = monotonic_us_since_start();

    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
    let batch = EventBatch {
        frame_number: 0, // synthetic — not tied to a compositor frame
        batch_ts_us: now_us,
        events: vec![InputEnvelope {
            event: Some(InputEvent::ScrollOffsetChanged(
                tze_hud_protocol::proto::ScrollOffsetChangedEvent {
                    tile_id: ev.tile_id.as_uuid().as_bytes().to_vec(),
                    timestamp_mono_us: timestamp_mono_us.as_u64(),
                    offset_x: ev.offset_x,
                    offset_y: ev.offset_y,
                },
            )),
        }],
    };

    // Scroll is ephemeral, so the bus uses its bounded droppable lane.
    let _ = tx.send((namespace, batch));
}

/// Broadcast a [`tze_hud_input::KeyboardDispatch`] to the owning agent via the
/// `INPUT_EVENTS` gRPC channel.
///
/// Converts the `KeyboardDispatch` to the appropriate proto envelope
/// (`KeyDownEvent`, `KeyUpEvent`, or `CharacterEvent`), wraps it in an
/// `EventBatch`, and sends it through the input-event bus. The session handler
/// delivers the batch only when the agent is subscribed to `INPUT_EVENTS` —
/// the subscription gate is enforced in `subscriptions::filter_event_batch`.
///
/// Keyboard events are transactional and therefore use the durable lane. If
/// no receiver is connected (gRPC disabled or no live session), the event has
/// no destination and the send result is ignored.
pub(super) fn dispatch_keyboard_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    dispatch: tze_hud_input::KeyboardDispatch,
) {
    let Some(tx) = tx else { return };

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let tile_id_bytes = dispatch.tile_id.as_uuid().as_bytes().to_vec();
    let node_id_bytes = dispatch
        .node_id
        .map(|id| id.as_uuid().as_bytes().to_vec())
        .unwrap_or_default();

    use tze_hud_input::KeyboardDispatchKind;
    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

    let event = match dispatch.kind {
        KeyboardDispatchKind::KeyDown {
            key_code,
            key,
            modifiers,
            repeat,
            timestamp_mono_us,
        } => InputEvent::KeyDown(tze_hud_protocol::proto::KeyDownEvent {
            tile_id: tile_id_bytes,
            node_id: node_id_bytes,
            timestamp_mono_us: timestamp_mono_us.0,
            key_code,
            key,
            repeat,
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            meta: modifiers.meta,
        }),
        KeyboardDispatchKind::KeyUp {
            key_code,
            key,
            modifiers,
            timestamp_mono_us,
        } => InputEvent::KeyUp(tze_hud_protocol::proto::KeyUpEvent {
            tile_id: tile_id_bytes,
            node_id: node_id_bytes,
            timestamp_mono_us: timestamp_mono_us.0,
            key_code,
            key,
            ctrl: modifiers.ctrl,
            shift: modifiers.shift,
            alt: modifiers.alt,
            meta: modifiers.meta,
        }),
        KeyboardDispatchKind::Character {
            character,
            timestamp_mono_us,
        } => InputEvent::Character(tze_hud_protocol::proto::CharacterEvent {
            tile_id: tile_id_bytes,
            node_id: node_id_bytes,
            timestamp_mono_us: timestamp_mono_us.0,
            character,
        }),
    };

    let batch = EventBatch {
        frame_number: 0, // synthetic — not tied to a compositor frame
        batch_ts_us: now_us,
        events: vec![InputEnvelope { event: Some(event) }],
    };

    // Fan out to session handler tasks; the sender classifies keyboard input
    // as transactional, so a slow receiver cannot make this event lag/drop.
    let _ = tx.send((dispatch.namespace, batch));
}

/// Broadcast an abstract command produced by the production input adapter.
///
/// Command events are transactional RFC 0004 input events. They use the same
/// namespace-addressed `INPUT_EVENTS` channel as keyboard and pointer input;
/// the session handler applies the capability/subscription filter.
pub(super) fn dispatch_command_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    dispatch: tze_hud_input::CommandDispatch,
) {
    let Some(tx) = tx else { return };

    debug_assert!(
        dispatch.is_transactional,
        "CommandInputEvent must always be transactional"
    );

    let action = match dispatch.event.action {
        tze_hud_input::CommandAction::NavigateNext => {
            tze_hud_protocol::proto::CommandAction::NavigateNext
        }
        tze_hud_input::CommandAction::NavigatePrev => {
            tze_hud_protocol::proto::CommandAction::NavigatePrev
        }
        tze_hud_input::CommandAction::Activate => tze_hud_protocol::proto::CommandAction::Activate,
        tze_hud_input::CommandAction::Cancel => tze_hud_protocol::proto::CommandAction::Cancel,
        tze_hud_input::CommandAction::Context => tze_hud_protocol::proto::CommandAction::Context,
        tze_hud_input::CommandAction::ScrollUp => tze_hud_protocol::proto::CommandAction::ScrollUp,
        tze_hud_input::CommandAction::ScrollDown => {
            tze_hud_protocol::proto::CommandAction::ScrollDown
        }
    };
    let source = match dispatch.event.source {
        tze_hud_input::CommandSource::Keyboard => tze_hud_protocol::proto::CommandSource::Keyboard,
        tze_hud_input::CommandSource::Dpad => tze_hud_protocol::proto::CommandSource::Dpad,
        tze_hud_input::CommandSource::Voice => tze_hud_protocol::proto::CommandSource::Voice,
        tze_hud_input::CommandSource::RemoteClicker => {
            tze_hud_protocol::proto::CommandSource::RemoteClicker
        }
        tze_hud_input::CommandSource::RotaryDial => {
            tze_hud_protocol::proto::CommandSource::RotaryDial
        }
        tze_hud_input::CommandSource::Programmatic => {
            tze_hud_protocol::proto::CommandSource::Programmatic
        }
    };

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let event = tze_hud_protocol::proto::input_envelope::Event::CommandInput(
        tze_hud_protocol::proto::CommandInputEvent {
            tile_id: dispatch.event.tile_id.as_uuid().as_bytes().to_vec(),
            node_id: dispatch
                .event
                .node_id
                .map(|id| id.as_uuid().as_bytes().to_vec())
                .unwrap_or_default(),
            interaction_id: dispatch.event.interaction_id,
            timestamp_mono_us: dispatch.event.timestamp_mono_us.0,
            device_id: dispatch.event.device_id,
            action: action as i32,
            source: source as i32,
        },
    );
    let batch = EventBatch {
        frame_number: 0,
        batch_ts_us: now_us,
        events: vec![InputEnvelope { event: Some(event) }],
    };

    let _ = tx.send((dispatch.namespace, batch));
}

/// Deliver a [`tze_hud_input::DraftNotificationBatch`] to the owning adapter
/// via the `INPUT_EVENTS` gRPC event channel (hud-ygbcy).
///
/// # Proto mapping
///
/// Each component of the batch maps to one outbound proto message in
/// `InputEnvelope`:
///
/// | Batch field      | Class       | Proto variant              |
/// |------------------|-------------|----------------------------|
/// | `latest`         | state-stream | `ComposerDraftStateEvent`  |
/// | `submission`     | transactional | `ComposerDraftSubmitEvent` |
/// | `cancel`         | transactional | `ComposerDraftCancelEvent` |
///
/// # Ordering
///
/// Messages are emitted in the order required by the delivery contract
/// (spec §4.3):
///
/// 1. State-stream notification (`latest`) — if present.
/// 2. Submission (`submission`) — if present; sequence > any `latest`.
///    The post-submit clear (`text=""`, sequence = submission + 1) is the
///    `latest` field produced by `DraftScheduler::flush_submit`; it arrives
///    in the *next* batch after submission.  No special handling needed here.
/// 3. Cancel (`cancel`) — if present (submit and cancel are mutually exclusive).
///
/// # Delivery semantics
///
/// Fan out on the `INPUT_EVENTS` channel addressed to `namespace`. The
/// session handler delivers each `EventBatch` only when the agent is
/// subscribed to `INPUT_EVENTS` — the subscription gate is enforced in
/// `subscriptions::filter_event_batch`, not here. A batch containing submit or
/// cancel is classified transactional; state-only batches remain droppable.
///
/// # Parameters
///
/// - `tx`: the shared traffic-class-aware sender; `None` when gRPC is disabled.
/// - `namespace`: the agent namespace that owns the composer node.
/// - `node_id_bytes`: 16-byte UUIDv7 of the focused composer node.
/// - `tile_id_bytes`: 16-byte UUIDv7 of the owning portal tile.
/// - `batch`: the coalesced draft batch to deliver.
pub(super) fn deliver_composer_batch(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    namespace: String,
    node_id_bytes: &[u8],
    tile_id_bytes: &[u8],
    batch: tze_hud_input::DraftNotificationBatch,
) {
    let Some(tx) = tx else { return };

    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    // Collect events in the canonical delivery order (state-stream first, then
    // transactional), each wrapped in an InputEnvelope.
    let mut events: Vec<InputEnvelope> = Vec::new();

    // Destructure to take ownership of the text fields — avoids cloning.
    let tze_hud_input::DraftNotificationBatch {
        latest,
        submission,
        cancel,
    } = batch;

    // 1. State-stream notification (UpdateComposerDisplay).
    if let Some(notif) = latest {
        tracing::debug!(
            namespace = %namespace,
            text_len = notif.text.len(),
            cursor = notif.cursor,
            at_capacity = notif.at_capacity,
            sequence = notif.sequence,
            "composer: delivering draft state notification (state-stream)"
        );
        events.push(InputEnvelope {
            event: Some(InputEvent::ComposerDraftState(
                tze_hud_protocol::proto::ComposerDraftStateEvent {
                    node_id: node_id_bytes.to_vec(),
                    text: notif.text,
                    cursor: notif.cursor as u64,
                    at_capacity: notif.at_capacity,
                    sequence: notif.sequence,
                    tile_id: tile_id_bytes.to_vec(),
                },
            )),
        });
    }

    // 2. Transactional submission.
    if let Some(sub) = submission {
        tracing::debug!(
            namespace = %namespace,
            text_len = sub.text.len(),
            sequence = sub.sequence,
            "composer: delivering draft submission (transactional)"
        );
        events.push(InputEnvelope {
            event: Some(InputEvent::ComposerDraftSubmit(
                tze_hud_protocol::proto::ComposerDraftSubmitEvent {
                    node_id: node_id_bytes.to_vec(),
                    text: sub.text,
                    sequence: sub.sequence,
                    tile_id: tile_id_bytes.to_vec(),
                },
            )),
        });
    }

    // 3. Transactional cancel (mutually exclusive with submission per batch
    //    XOR semantics enforced by DraftScheduler).
    if let Some(cancel) = cancel {
        tracing::debug!(
            namespace = %namespace,
            sequence = cancel.sequence,
            "composer: delivering draft cancel (transactional)"
        );
        events.push(InputEnvelope {
            event: Some(InputEvent::ComposerDraftCancel(
                tze_hud_protocol::proto::ComposerDraftCancelEvent {
                    node_id: node_id_bytes.to_vec(),
                    sequence: cancel.sequence,
                    tile_id: tile_id_bytes.to_vec(),
                },
            )),
        });
    }

    if events.is_empty() {
        return;
    }

    let event_batch = EventBatch {
        frame_number: 0, // synthetic — not tied to a compositor frame
        batch_ts_us: now_us,
        events,
    };

    // Fan out on the transactional lane; each session handler still applies
    // namespace and subscription filtering.
    let _ = tx.send((namespace, event_batch));
}

/// Broadcast `FocusGainedEvent` and/or `FocusLostEvent` to the owning agents
/// via the `FOCUS_EVENTS` gRPC channel.
///
/// Converts a [`tze_hud_input::FocusTransition`] into proto envelopes and sends
/// each event as a single-event `EventBatch` on the durable lane. The
/// session handler delivers each batch only when the agent is subscribed to
/// `FOCUS_EVENTS` — the subscription gate is enforced in
/// `subscriptions::filter_event_batch`, not here.
///
/// Focus lost is dispatched first (if present) so the agent that relinquished
/// focus receives its event before the newly-focused agent receives its gained
/// event, preserving the ordering guarantee in RFC 0004 §8.4.
///
/// Focus events use the durable transactional lane. A send with no connected
/// session has no destination and is ignored.
pub(super) fn dispatch_focus_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    transition: tze_hud_input::FocusTransition,
) {
    let Some(tx) = tx else { return };

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let timestamp_mono_us = monotonic_us_since_start();

    use tze_hud_input::{FocusLostReason, FocusSource};
    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

    // ── FocusLostEvent (dispatched first per RFC 0004 §8.4) ─────────────────
    if let Some((lost_ev, namespace)) = transition.lost {
        let tile_id_bytes = lost_ev.tile_id.as_uuid().as_bytes().to_vec();
        let node_id_bytes = lost_ev
            .node_id
            .map(|id| id.as_uuid().as_bytes().to_vec())
            .unwrap_or_default();

        let proto_reason = match lost_ev.reason {
            FocusLostReason::ClickElsewhere => {
                tze_hud_protocol::proto::FocusLostReason::ClickElsewhere
            }
            FocusLostReason::TabKey => tze_hud_protocol::proto::FocusLostReason::TabKey,
            FocusLostReason::Programmatic => tze_hud_protocol::proto::FocusLostReason::Programmatic,
            FocusLostReason::TileDestroyed => {
                tze_hud_protocol::proto::FocusLostReason::TileDestroyed
            }
            FocusLostReason::TabSwitched => tze_hud_protocol::proto::FocusLostReason::TabSwitched,
            FocusLostReason::LeaseRevoked => tze_hud_protocol::proto::FocusLostReason::LeaseRevoked,
            FocusLostReason::AgentDisconnected => {
                tze_hud_protocol::proto::FocusLostReason::AgentDisconnected
            }
            FocusLostReason::CommandInput => tze_hud_protocol::proto::FocusLostReason::CommandInput,
        };

        let batch = EventBatch {
            frame_number: 0,
            batch_ts_us: now_us,
            events: vec![InputEnvelope {
                event: Some(InputEvent::FocusLost(
                    tze_hud_protocol::proto::FocusLostEvent {
                        tile_id: tile_id_bytes,
                        node_id: node_id_bytes,
                        timestamp_mono_us: timestamp_mono_us.as_u64(),
                        reason: proto_reason as i32,
                    },
                )),
            }],
        };

        let _ = tx.send((namespace, batch));
    }

    // ── FocusGainedEvent ─────────────────────────────────────────────────────
    if let Some((gained_ev, namespace)) = transition.gained {
        let tile_id_bytes = gained_ev.tile_id.as_uuid().as_bytes().to_vec();
        let node_id_bytes = gained_ev
            .node_id
            .map(|id| id.as_uuid().as_bytes().to_vec())
            .unwrap_or_default();

        let proto_source = match gained_ev.source {
            FocusSource::Click => tze_hud_protocol::proto::FocusSource::Click,
            FocusSource::TabKey => tze_hud_protocol::proto::FocusSource::TabKey,
            FocusSource::Programmatic => tze_hud_protocol::proto::FocusSource::Programmatic,
            FocusSource::CommandInput => tze_hud_protocol::proto::FocusSource::CommandInput,
        };

        let batch = EventBatch {
            frame_number: 0,
            batch_ts_us: now_us,
            events: vec![InputEnvelope {
                event: Some(InputEvent::FocusGained(
                    tze_hud_protocol::proto::FocusGainedEvent {
                        tile_id: tile_id_bytes,
                        node_id: node_id_bytes,
                        timestamp_mono_us: timestamp_mono_us.as_u64(),
                        source: proto_source as i32,
                    },
                )),
            }],
        };

        let _ = tx.send((namespace, batch));
    }
}

/// Broadcast a `PointerDownEvent`, `PointerMoveEvent`, or `PointerUpEvent` to
/// the owning agent via the `INPUT_EVENTS` gRPC channel.
///
/// Converts an [`tze_hud_input::AgentDispatch`] with `kind` in
/// {`PointerDown`, `PointerMove`, `PointerUp`} to the corresponding proto
/// envelope, wraps it in an `EventBatch`, and sends it through the input-event
/// bus. The session handler delivers the batch only when the agent is
/// subscribed to `INPUT_EVENTS` — the subscription gate is enforced in
/// `subscriptions::filter_event_batch`, not here.
///
/// **Throttling**: every `PointerMove` is forwarded as-is to all opted-in
/// subscribers.  Subscribers that cannot tolerate the full rate should
/// throttle on the receive side.  This matches the "ephemeral realtime"
/// message class contract (RFC CLAUDE.md §Four Message Classes) and avoids
/// imposing a specific rate budget on the dispatch path, which is on the
/// Stage 2 hot path (< 500 µs p99 per engineering-bar.md §2).
///
/// All other `AgentDispatchKind` values are silently ignored — only
/// `PointerDown`, `PointerMove`, and `PointerUp` are dispatched here.
/// `PointerEnter`, `PointerLeave`, `Activated` are not yet wired.
/// `CaptureReleased` is routed to `dispatch_capture_released_event` by the
/// caller (it belongs to `FOCUS_EVENTS`, not `INPUT_EVENTS`).
///
/// Transactional pointer variants use the durable lane; move/hover variants
/// retain bounded ephemeral delivery.
pub(super) fn dispatch_pointer_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    dispatch: tze_hud_input::AgentDispatch,
) {
    let Some(tx) = tx else { return };

    use tze_hud_input::AgentDispatchKind;
    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

    let tile_id_bytes = dispatch.tile_id.as_uuid().as_bytes().to_vec();
    // Send empty bytes when no specific node was hit (tile-level pointer event).
    // This matches the proto field-presence convention used by FocusLostEvent,
    // FocusGainedEvent, and CaptureReleasedEvent: absent field = empty Vec,
    // not 16 zero bytes.
    let node_id_bytes = if dispatch.node_id.is_nil() {
        Vec::new()
    } else {
        dispatch.node_id.as_uuid().as_bytes().to_vec()
    };

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;

    let timestamp_mono_us = monotonic_us_since_start();

    let event = match dispatch.kind {
        AgentDispatchKind::PointerDown => {
            InputEvent::PointerDown(tze_hud_protocol::proto::PointerDownEvent {
                tile_id: tile_id_bytes,
                node_id: node_id_bytes,
                interaction_id: dispatch.interaction_id,
                timestamp_mono_us: timestamp_mono_us.as_u64(),
                device_id: dispatch.device_id.to_string(),
                local_x: dispatch.local_x,
                local_y: dispatch.local_y,
                display_x: dispatch.display_x,
                display_y: dispatch.display_y,
                button: 0, // primary button; multi-button not yet tracked in AgentDispatch
            })
        }
        AgentDispatchKind::PointerMove => {
            InputEvent::PointerMove(tze_hud_protocol::proto::PointerMoveEvent {
                tile_id: tile_id_bytes,
                node_id: node_id_bytes,
                interaction_id: dispatch.interaction_id,
                timestamp_mono_us: timestamp_mono_us.as_u64(),
                device_id: dispatch.device_id.to_string(),
                local_x: dispatch.local_x,
                local_y: dispatch.local_y,
                display_x: dispatch.display_x,
                display_y: dispatch.display_y,
            })
        }
        AgentDispatchKind::PointerUp => {
            InputEvent::PointerUp(tze_hud_protocol::proto::PointerUpEvent {
                tile_id: tile_id_bytes,
                node_id: node_id_bytes,
                interaction_id: dispatch.interaction_id,
                timestamp_mono_us: timestamp_mono_us.as_u64(),
                device_id: dispatch.device_id.to_string(),
                local_x: dispatch.local_x,
                local_y: dispatch.local_y,
                display_x: dispatch.display_x,
                display_y: dispatch.display_y,
                button: 0, // primary button; multi-button not yet tracked in AgentDispatch
            })
        }
        // All other variants (PointerEnter, PointerLeave, Activated, PointerCancel)
        // are not yet wired.  CaptureReleased is pre-filtered by the caller and
        // routed to dispatch_capture_released_event instead.
        _ => return,
    };

    let batch = EventBatch {
        frame_number: 0, // synthetic — not tied to a compositor frame
        batch_ts_us: now_us,
        events: vec![InputEnvelope { event: Some(event) }],
    };

    // The bus routes PointerDown/Up through the durable lane and PointerMove
    // through the bounded ephemeral lane.
    let _ = tx.send((dispatch.namespace, batch));
}

/// Broadcast a `CaptureReleasedEvent` to the owning agent via the `FOCUS_EVENTS`
/// gRPC channel.
///
/// Called when `InputProcessor` produces a `CaptureReleased` dispatch in
/// `extra_dispatches` (e.g. after `PointerUp` with `release_on_up=true`).
///
/// `CaptureReleased` is a focus/lease lifecycle event, not a pointer event, so
/// it belongs on the `FOCUS_EVENTS` channel (RFC 0004 §8.3, subscriptions.rs
/// §`is_focus_variant`).  Agents that subscribe to `FOCUS_EVENTS` with the
/// `access_input_events` capability will receive it.
///
/// Capture release uses the durable transactional lane.
pub(super) fn dispatch_capture_released_event(
    tx: &Option<tze_hud_protocol::session_server::InputEventSender>,
    dispatch: tze_hud_input::AgentDispatch,
) {
    let Some(tx) = tx else { return };

    use tze_hud_input::{AgentDispatchKind, CaptureReleasedReason};
    use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

    debug_assert_eq!(
        dispatch.kind,
        AgentDispatchKind::CaptureReleased,
        "dispatch_capture_released_event called with non-CaptureReleased kind"
    );

    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let timestamp_mono_us = monotonic_us_since_start();

    let proto_reason = match dispatch.capture_released_reason {
        Some(CaptureReleasedReason::AgentReleased) => {
            tze_hud_protocol::proto::CaptureReleasedReason::AgentReleased
        }
        Some(CaptureReleasedReason::PointerUp) => {
            tze_hud_protocol::proto::CaptureReleasedReason::PointerUp
        }
        Some(CaptureReleasedReason::RuntimeRevoked) => {
            tze_hud_protocol::proto::CaptureReleasedReason::RuntimeRevoked
        }
        Some(CaptureReleasedReason::LeaseRevoked) => {
            tze_hud_protocol::proto::CaptureReleasedReason::LeaseRevoked
        }
        None => tze_hud_protocol::proto::CaptureReleasedReason::Unspecified,
    };

    let tile_id_bytes = dispatch.tile_id.as_uuid().as_bytes().to_vec();
    // Send empty bytes when no specific node was captured (tile-level capture).
    // This matches the proto field-presence convention used by FocusLostEvent
    // and FocusGainedEvent: absent field = empty Vec, not 16 zero bytes.
    let node_id_bytes = if dispatch.node_id.is_nil() {
        Vec::new()
    } else {
        dispatch.node_id.as_uuid().as_bytes().to_vec()
    };

    let batch = EventBatch {
        frame_number: 0, // synthetic — not tied to a compositor frame
        batch_ts_us: now_us,
        events: vec![InputEnvelope {
            event: Some(InputEvent::CaptureReleased(
                tze_hud_protocol::proto::CaptureReleasedEvent {
                    tile_id: tile_id_bytes,
                    node_id: node_id_bytes,
                    timestamp_mono_us: timestamp_mono_us.as_u64(),
                    device_id: dispatch.device_id.to_string(),
                    reason: proto_reason as i32,
                },
            )),
        }],
    };

    // Broadcast to FOCUS_EVENTS subscribers.  Errors are silently ignored.
    let _ = tx.send((dispatch.namespace, batch));
}

/// Broadcast an `ElementRepositionedEvent` after a hotkey-driven portal resize.
///
/// The scene tile bounds have already been updated locally (local-first feedback).
/// This function notifies gRPC subscribers subscribed to `SCENE_TOPOLOGY` that
/// the portal geometry has changed, using the same `ElementRepositionedEvent`
/// that drag-reposition uses (§6b.4).
///
/// Delivery is best-effort (fire-and-forget): errors (no receivers, channel
/// lagged) are silently ignored.
pub(super) fn dispatch_portal_geometry_event(
    tx: &Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::ElementRepositionedEvent>>,
    tile_id: tze_hud_scene::SceneId,
    snapshot: &tze_hud_input::GeometrySnapshot,
    display_w: f32,
    display_h: f32,
) {
    let Some(tx) = tx else { return };

    // Convert the absolute pixel rect to a relative (percentage) geometry policy
    // so subscribers see the same wire format as drag-reposition events.
    let (x_pct, y_pct, w_pct, h_pct) = if display_w > 0.0 && display_h > 0.0 {
        (
            snapshot.rect.x / display_w,
            snapshot.rect.y / display_h,
            snapshot.rect.width / display_w,
            snapshot.rect.height / display_h,
        )
    } else {
        (0.0, 0.0, 0.0, 0.0)
    };

    let new_geometry = tze_hud_protocol::proto::GeometryPolicyProto {
        policy: Some(
            tze_hud_protocol::proto::geometry_policy_proto::Policy::Relative(
                tze_hud_protocol::proto::RelativeGeometryPolicy {
                    x_pct,
                    y_pct,
                    width_pct: w_pct,
                    height_pct: h_pct,
                },
            ),
        ),
    };

    let event = tze_hud_protocol::proto::ElementRepositionedEvent {
        element_id: tile_id.as_uuid().as_bytes().to_vec(),
        new_geometry: Some(new_geometry),
        previous_geometry: None,
    };

    let _ = tx.send(event);
}

/// Push an `InputEvent` into the ring buffer, dropping the oldest if full.
pub(super) fn enqueue_input(
    ring: &Arc<std::sync::Mutex<std::collections::VecDeque<InputEvent>>>,
    event: InputEvent,
) {
    if let Ok(mut q) = ring.lock() {
        if q.len() >= INPUT_EVENT_CAPACITY {
            q.pop_front(); // Drop oldest to make room.
        }
        q.push_back(event);
    }
}

/// Monotonic nanosecond timestamp for `InputEvent.timestamp_ns`.
///
/// Uses process-relative time so values are comparable within a session.
pub(super) fn nanoseconds_since_start() -> u64 {
    // Use std::time::Instant for monotonic timing.
    // We store the process start time lazily and subtract.
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

/// Monotonic microseconds since process start for `_mono_us` input payloads.
///
/// `MonoUs(0)` is the protocol's "not set" sentinel, so production dispatches
/// clamp the first sub-microsecond sample to one.
pub(super) fn monotonic_us_since_start() -> MonoUs {
    MonoUs((nanoseconds_since_start() / 1_000).max(1))
}

/// Map a winit `PhysicalKey` to a compact u32 key code.
///
/// This is a best-effort mapping for the `InputEventKind::KeyPress/KeyRelease`
/// channel type. The full keyboard pipeline uses `tze_hud_input::KeyboardProcessor`
/// for richer key event data.
pub(super) fn physical_key_to_u32(key: &winit::keyboard::PhysicalKey) -> u32 {
    use winit::keyboard::PhysicalKey;
    match key {
        PhysicalKey::Code(code) => *code as u32,
        PhysicalKey::Unidentified(_) => 0,
    }
}

/// Map a winit `PhysicalKey` to the DOM `KeyboardEvent.code`-style string
/// used by `RawKeyDownEvent.key_code` (RFC 0004 §7.4).
///
/// Returns the `KeyCode` variant name (e.g. `"KeyA"`, `"ShiftLeft"`,
/// `"ArrowDown"`) for identified keys, and `"Unidentified"` for unknown ones.
pub(super) fn physical_key_to_key_code_str(key: &winit::keyboard::PhysicalKey) -> String {
    use winit::keyboard::PhysicalKey;
    match key {
        PhysicalKey::Code(code) => format!("{code:?}"),
        PhysicalKey::Unidentified(_) => "Unidentified".to_string(),
    }
}

/// Map a winit logical `Key` to the DOM `KeyboardEvent.key`-style string
/// used by `RawKeyDownEvent.key` and `RawKeyUpEvent.key` (RFC 0004 §7.4).
///
/// For character keys this is the character itself (e.g. `"a"`, `"A"`, `"1"`).
/// For named keys this is the `NamedKey` variant name (e.g. `"Enter"`,
/// `"Backspace"`, `"ArrowDown"`). Unknown keys map to `"Unidentified"`.
pub(super) fn logical_key_to_str(key: &winit::keyboard::Key) -> String {
    use winit::keyboard::Key;
    match key {
        Key::Character(s) => s.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified(_) => "Unidentified".to_string(),
        Key::Dead(Some(c)) => format!("Dead({c})"),
        Key::Dead(None) => "Dead".to_string(),
    }
}

/// Convert winit's `ModifiersState` to `KeyboardModifiers` (RFC 0004 §7.4).
///
/// CapsLock and NumLock toggle states are not exposed by winit's
/// `ModifiersState`; they default to `false` here. Full toggle-key tracking
/// can be added via `WindowEvent::KeyboardInput` state tracking if needed.
pub(super) fn winit_mods_to_keyboard_modifiers(
    mods: winit::keyboard::ModifiersState,
) -> tze_hud_input::KeyboardModifiers {
    tze_hud_input::KeyboardModifiers {
        shift: mods.shift_key(),
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        meta: mods.super_key(),
        caps_lock: false, // winit ModifiersState does not expose CapsLock state
        num_lock: false,  // winit ModifiersState does not expose NumLock state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_input_event_channel() -> (
        tze_hud_protocol::session_server::InputEventSender,
        tze_hud_protocol::session_server::InputEventReceiver,
    ) {
        let tx = tze_hud_protocol::session_server::InputEventSender::new(8);
        let rx = tx.subscribe_all();
        (tx, rx)
    }
    use crate::channels::{INPUT_EVENT_CAPACITY, InputEventKind};

    #[test]
    fn enqueue_input_drops_oldest_when_full() {
        let ring = Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::with_capacity(INPUT_EVENT_CAPACITY),
        ));
        // Fill beyond capacity.
        for i in 0..INPUT_EVENT_CAPACITY + 10 {
            let event = InputEvent {
                timestamp_ns: i as u64,
                kind: InputEventKind::KeyPress { key: 0 },
            };
            enqueue_input(&ring, event);
        }
        let q = ring.lock().unwrap();
        assert_eq!(
            q.len(),
            INPUT_EVENT_CAPACITY,
            "ring buffer should never exceed capacity"
        );
        // The oldest entry was dropped; the newest should have timestamp
        // INPUT_EVENT_CAPACITY + 9.
        let last = q.back().unwrap();
        assert_eq!(
            last.timestamp_ns,
            (INPUT_EVENT_CAPACITY + 9) as u64,
            "most recent event should be at the back"
        );
    }

    #[test]
    fn nanoseconds_since_start_is_monotonic() {
        let t1 = nanoseconds_since_start();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let t2 = nanoseconds_since_start();
        assert!(t2 > t1, "timestamps must be monotonically increasing");
    }

    #[test]
    fn mouse_wheel_delta_positive_line_scrolls_toward_top() {
        let (x, y) = normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, 1.0));

        assert_eq!(x, 0.0);
        assert_eq!(y, -40.0);
    }

    #[test]
    fn mouse_wheel_delta_negative_line_scrolls_down_transcript() {
        let (_, y) = normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, -1.0));

        assert_eq!(y, 40.0);
    }

    /// Build a minimal [`AgentDispatch`] for the given kind.
    fn make_agent_dispatch(kind: tze_hud_input::AgentDispatchKind) -> tze_hud_input::AgentDispatch {
        use tze_hud_scene::SceneId;
        tze_hud_input::AgentDispatch {
            namespace: "test-agent".to_string(),
            tile_id: SceneId::new(),
            node_id: SceneId::new(),
            interaction_id: "test-interaction".to_string(),
            local_x: 1.0,
            local_y: 2.0,
            display_x: 10.0,
            display_y: 20.0,
            device_id: 0,
            kind,
            capture_released_reason: None,
        }
    }

    /// Extract `timestamp_mono_us` from a received `EventBatch` containing one
    /// pointer event.  Panics with a descriptive message if the batch or event
    /// is not what was expected.
    fn extract_pointer_timestamp(batch: &tze_hud_protocol::proto::EventBatch) -> u64 {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;
        assert_eq!(
            batch.events.len(),
            1,
            "batch must contain exactly one event"
        );
        match &batch.events[0].event {
            Some(InputEvent::PointerDown(ev)) => ev.timestamp_mono_us,
            Some(InputEvent::PointerMove(ev)) => ev.timestamp_mono_us,
            Some(InputEvent::PointerUp(ev)) => ev.timestamp_mono_us,
            other => panic!("expected pointer event, got: {other:?}"),
        }
    }

    fn assert_set_mono_timestamp(timestamp_mono_us: u64, event_name: &str) {
        assert!(
            tze_hud_scene::MonoUs(timestamp_mono_us).is_set(),
            "{event_name} must carry a set timestamp_mono_us for latency measurement"
        );
    }

    #[test]
    fn scroll_offset_dispatch_propagates_set_mono_timestamp() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        dispatch_scroll_offset_event_to_namespace(
            &tx_opt,
            "scroll-agent".to_string(),
            tze_hud_input::ScrollOffsetChangedEvent {
                tile_id: tze_hud_scene::SceneId::new(),
                offset_x: 12.0,
                offset_y: 34.0,
            },
        );

        let (namespace, batch) = rx.try_recv().expect("scroll event must be sent");
        assert_eq!(namespace, "scroll-agent");
        let Some(InputEvent::ScrollOffsetChanged(event)) = &batch.events[0].event else {
            panic!("expected ScrollOffsetChanged event");
        };
        assert_set_mono_timestamp(event.timestamp_mono_us, "ScrollOffsetChangedEvent");
    }

    #[test]
    fn focus_dispatch_propagates_set_mono_timestamps() {
        use tze_hud_input::{FocusGainedEvent, FocusLostEvent, FocusLostReason, FocusSource};
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let transition = tze_hud_input::FocusTransition {
            lost: Some((
                FocusLostEvent {
                    tile_id: tze_hud_scene::SceneId::new(),
                    node_id: Some(tze_hud_scene::SceneId::new()),
                    reason: FocusLostReason::ClickElsewhere,
                },
                "lost-agent".to_string(),
            )),
            gained: Some((
                FocusGainedEvent {
                    tile_id: tze_hud_scene::SceneId::new(),
                    node_id: Some(tze_hud_scene::SceneId::new()),
                    source: FocusSource::Click,
                },
                "gained-agent".to_string(),
            )),
            ring_update: None,
        };

        dispatch_focus_event(&tx_opt, transition);

        let (lost_namespace, lost_batch) = rx.try_recv().expect("focus-lost event must be sent");
        assert_eq!(lost_namespace, "lost-agent");
        let Some(InputEvent::FocusLost(event)) = &lost_batch.events[0].event else {
            panic!("expected FocusLost event");
        };
        assert_set_mono_timestamp(event.timestamp_mono_us, "FocusLostEvent");

        let (gained_namespace, gained_batch) =
            rx.try_recv().expect("focus-gained event must be sent");
        assert_eq!(gained_namespace, "gained-agent");
        let Some(InputEvent::FocusGained(event)) = &gained_batch.events[0].event else {
            panic!("expected FocusGained event");
        };
        assert_set_mono_timestamp(event.timestamp_mono_us, "FocusGainedEvent");
    }

    #[test]
    fn capture_release_dispatch_propagates_set_mono_timestamp() {
        use tze_hud_input::{AgentDispatchKind, CaptureReleasedReason};
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let mut dispatch = make_agent_dispatch(AgentDispatchKind::CaptureReleased);
        dispatch.capture_released_reason = Some(CaptureReleasedReason::PointerUp);

        dispatch_capture_released_event(&tx_opt, dispatch);

        let (namespace, batch) = rx.try_recv().expect("capture-release event must be sent");
        assert_eq!(namespace, "test-agent");
        let Some(InputEvent::CaptureReleased(event)) = &batch.events[0].event else {
            panic!("expected CaptureReleased event");
        };
        assert_set_mono_timestamp(event.timestamp_mono_us, "CaptureReleasedEvent");
    }

    /// `dispatch_pointer_event` must set `timestamp_mono_us > 0` for PointerDown,
    /// PointerMove, and PointerUp (gap from hud-zffvp now closed).
    #[test]
    fn dispatch_pointer_event_timestamp_mono_us_is_non_zero() {
        use tze_hud_input::AgentDispatchKind;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);

        for kind in [
            AgentDispatchKind::PointerDown,
            AgentDispatchKind::PointerMove,
            AgentDispatchKind::PointerUp,
        ] {
            let label = format!("{kind:?}");
            dispatch_pointer_event(&tx_opt, make_agent_dispatch(kind));
            let (_ns, batch) = rx.try_recv().expect("event must be sent on the channel");
            let ts = extract_pointer_timestamp(&batch);
            assert!(
                ts > 0,
                "{label}: timestamp_mono_us must be non-zero (monotonic clock wired)"
            );
        }
    }

    /// Two consecutive `dispatch_pointer_event` calls must produce monotonically
    /// increasing `timestamp_mono_us` values, confirming the clock source is
    /// truly monotonic and not stuck at zero.
    #[test]
    fn dispatch_pointer_event_timestamp_mono_us_is_monotonic() {
        use tze_hud_input::AgentDispatchKind;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);

        dispatch_pointer_event(&tx_opt, make_agent_dispatch(AgentDispatchKind::PointerDown));
        let (_ns, batch1) = rx.try_recv().expect("first event must be sent");
        let ts1 = extract_pointer_timestamp(&batch1);

        // A small sleep ensures the monotonic clock advances between calls.
        std::thread::sleep(std::time::Duration::from_millis(1));

        dispatch_pointer_event(&tx_opt, make_agent_dispatch(AgentDispatchKind::PointerMove));
        let (_ns, batch2) = rx.try_recv().expect("second event must be sent");
        let ts2 = extract_pointer_timestamp(&batch2);

        assert!(
            ts2 > ts1,
            "timestamp_mono_us must be strictly increasing across consecutive dispatches \
             (ts1={ts1}, ts2={ts2})"
        );
    }

    /// Extract proto variant from an `InputEnvelope`.
    fn envelope_variant(
        env: &tze_hud_protocol::proto::InputEnvelope,
    ) -> &tze_hud_protocol::proto::input_envelope::Event {
        env.event
            .as_ref()
            .expect("InputEnvelope must have an event")
    }

    /// Build a `DraftNotificationBatch` with only a state-stream notification.
    fn make_latest_batch(
        text: &str,
        cursor: usize,
        sequence: u64,
    ) -> tze_hud_input::DraftNotificationBatch {
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.coalesce_state(tze_hud_input::DraftStateNotification {
            text: text.to_string(),
            cursor,
            selection_anchor: cursor,
            at_capacity: false,
            sequence,
        });
        batch
    }

    /// Build a `DraftNotificationBatch` with a submission (the post-submit
    /// clear is emitted by the scheduler as a follow-on `latest` in the
    /// next batch; here we test the submission-only path first).
    fn make_submit_batch(text: &str, sequence: u64) -> tze_hud_input::DraftNotificationBatch {
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_submission(tze_hud_input::DraftSubmission {
            text: text.to_string(),
            sequence,
        });
        batch
    }

    /// Build a cancel batch.
    fn make_cancel_batch(sequence: u64) -> tze_hud_input::DraftNotificationBatch {
        let mut batch = tze_hud_input::DraftNotificationBatch::new();
        batch.record_cancel(tze_hud_input::DraftCancel { sequence });
        batch
    }

    /// Spec §4.3: a state-stream notification produces a single
    /// `ComposerDraftStateEvent` in the outbound EventBatch.
    #[test]
    fn deliver_composer_batch_latest_emits_draft_state_event() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let node_id_bytes = vec![0u8; 16];
        let tile_id_bytes = vec![9u8; 16];

        let batch = make_latest_batch("hello", 5, 1);
        deliver_composer_batch(
            &tx_opt,
            "test-agent".to_string(),
            &node_id_bytes,
            &tile_id_bytes,
            batch,
        );

        let (ns, ev_batch) = rx.try_recv().expect("event must be sent");
        assert_eq!(ns, "test-agent");
        assert_eq!(ev_batch.events.len(), 1);

        let ev = envelope_variant(&ev_batch.events[0]);
        let InputEvent::ComposerDraftState(state) = ev else {
            panic!("expected ComposerDraftState, got {ev:?}");
        };
        assert_eq!(state.text, "hello");
        assert_eq!(state.cursor, 5);
        assert_eq!(state.sequence, 1);
        assert!(!state.at_capacity);
        assert_eq!(state.node_id, node_id_bytes);
        assert_eq!(
            state.tile_id, tile_id_bytes,
            "tile_id must be carried on the wire so a bridge can attribute input (hud-25g5i)"
        );
    }

    /// Spec §4.3: a transactional submission produces a single
    /// `ComposerDraftSubmitEvent`.
    #[test]
    fn deliver_composer_batch_submit_emits_draft_submit_event() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let node_id_bytes = vec![1u8; 16];
        let tile_id_bytes = vec![8u8; 16];

        let batch = make_submit_batch("send this", 42);
        deliver_composer_batch(
            &tx_opt,
            "portal-agent".to_string(),
            &node_id_bytes,
            &tile_id_bytes,
            batch,
        );

        let (ns, ev_batch) = rx.try_recv().expect("event must be sent");
        assert_eq!(ns, "portal-agent");
        assert_eq!(ev_batch.events.len(), 1);

        let ev = envelope_variant(&ev_batch.events[0]);
        let InputEvent::ComposerDraftSubmit(sub) = ev else {
            panic!("expected ComposerDraftSubmit, got {ev:?}");
        };
        assert_eq!(sub.text, "send this");
        assert_eq!(sub.sequence, 42);
        assert_eq!(sub.node_id, node_id_bytes);
        assert_eq!(sub.tile_id, tile_id_bytes);
    }

    /// Spec §4.3: a cancel produces a single `ComposerDraftCancelEvent`.
    #[test]
    fn deliver_composer_batch_cancel_emits_draft_cancel_event() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let node_id_bytes = vec![2u8; 16];
        let tile_id_bytes = vec![7u8; 16];

        let batch = make_cancel_batch(7);
        deliver_composer_batch(
            &tx_opt,
            "test-agent".to_string(),
            &node_id_bytes,
            &tile_id_bytes,
            batch,
        );

        let (ns, ev_batch) = rx.try_recv().expect("event must be sent");
        assert_eq!(ns, "test-agent");
        assert_eq!(ev_batch.events.len(), 1);

        let ev = envelope_variant(&ev_batch.events[0]);
        let InputEvent::ComposerDraftCancel(cancel) = ev else {
            panic!("expected ComposerDraftCancel, got {ev:?}");
        };
        assert_eq!(cancel.sequence, 7);
        assert_eq!(cancel.node_id, node_id_bytes);
        assert_eq!(cancel.tile_id, tile_id_bytes);
    }

    /// Spec §4.3 / hud-qwqxy: a full submit cycle produces events in the
    /// correct order and with correct sequences:
    ///
    ///   Batch 1: latest (text="hello", seq=3) + submission (text="hello", seq=4)
    ///     → [ComposerDraftState(seq=3), ComposerDraftSubmit(seq=4)]
    ///
    ///   Batch 2 (post-submit clear from DraftScheduler::flush_submit):
    ///     latest (text="", seq=5)
    ///     → [ComposerDraftState(seq=5, text="")]
    ///
    /// This verifies ordering and that the post-submit clear (seq > submission)
    /// flows through correctly.
    #[test]
    fn deliver_composer_batch_submit_cycle_correct_order_and_sequences() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let node_id_bytes = vec![3u8; 16];
        let tile_id_bytes = vec![6u8; 16];

        // Batch 1: latest(seq=3) + submission(seq=4)
        let mut batch1 = tze_hud_input::DraftNotificationBatch::new();
        batch1.coalesce_state(tze_hud_input::DraftStateNotification {
            text: "hello".to_string(),
            cursor: 5,
            selection_anchor: 5,
            at_capacity: false,
            sequence: 3,
        });
        batch1.record_submission(tze_hud_input::DraftSubmission {
            text: "hello".to_string(),
            sequence: 4,
        });

        deliver_composer_batch(
            &tx_opt,
            "test-agent".to_string(),
            &node_id_bytes,
            &tile_id_bytes,
            batch1,
        );

        let (_ns, batch_out1) = rx.try_recv().expect("batch 1 must be sent");
        assert_eq!(
            batch_out1.events.len(),
            2,
            "batch 1 must contain exactly 2 events (state + submit)"
        );

        // State event comes first (state-stream before transactional per delivery order)
        let ev0 = envelope_variant(&batch_out1.events[0]);
        let InputEvent::ComposerDraftState(state) = ev0 else {
            panic!("expected ComposerDraftState first, got {ev0:?}");
        };
        assert_eq!(state.sequence, 3, "state event sequence must be 3");
        assert_eq!(state.text, "hello");

        // Submission event second
        let ev1 = envelope_variant(&batch_out1.events[1]);
        let InputEvent::ComposerDraftSubmit(sub) = ev1 else {
            panic!("expected ComposerDraftSubmit second, got {ev1:?}");
        };
        assert_eq!(sub.sequence, 4, "submit sequence must be 4 (> state seq)");
        assert_eq!(sub.text, "hello");

        // Batch 2: post-submit clear (text="", seq=5 > submission seq=4)
        let mut batch2 = tze_hud_input::DraftNotificationBatch::new();
        batch2.coalesce_state(tze_hud_input::DraftStateNotification {
            text: String::new(),
            cursor: 0,
            selection_anchor: 0,
            at_capacity: false,
            sequence: 5,
        });

        deliver_composer_batch(
            &tx_opt,
            "test-agent".to_string(),
            &node_id_bytes,
            &tile_id_bytes,
            batch2,
        );

        let (_ns, batch_out2) = rx
            .try_recv()
            .expect("batch 2 (post-submit clear) must be sent");
        assert_eq!(
            batch_out2.events.len(),
            1,
            "post-submit clear batch must contain exactly 1 event"
        );

        let ev2 = envelope_variant(&batch_out2.events[0]);
        let InputEvent::ComposerDraftState(clear) = ev2 else {
            panic!("expected ComposerDraftState for post-submit clear, got {ev2:?}");
        };
        assert_eq!(clear.text, "", "post-submit clear must have empty text");
        assert_eq!(
            clear.sequence, 5,
            "post-submit clear sequence must be 5 (> submission seq=4)"
        );
    }

    /// `deliver_composer_batch` is silent when `tx` is `None` (gRPC disabled).
    #[test]
    fn deliver_composer_batch_no_op_when_tx_is_none() {
        let batch = make_latest_batch("text", 4, 1);
        // Should not panic
        deliver_composer_batch(
            &None,
            "test-agent".to_string(),
            &[0u8; 16],
            &[0u8; 16],
            batch,
        );
    }

    /// An empty batch produces no event.
    #[test]
    fn deliver_composer_batch_empty_batch_sends_nothing() {
        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);

        deliver_composer_batch(
            &tx_opt,
            "test-agent".to_string(),
            &[0u8; 16],
            &[0u8; 16],
            tze_hud_input::DraftNotificationBatch::new(),
        );

        assert!(
            rx.try_recv().is_err(),
            "empty batch must not produce an event"
        );
    }

    /// Sequence ordering: the post-submit clear's sequence is strictly greater
    /// than the submission sequence, ensuring the adapter can safely skip
    /// state-stream notifications with sequence ≤ last seen.
    #[test]
    fn deliver_composer_batch_post_submit_clear_sequence_exceeds_submission() {
        use tze_hud_protocol::proto::input_envelope::Event as InputEvent;

        let (tx, mut rx) = test_input_event_channel();
        let tx_opt = Some(tx);
        let node_id_bytes = vec![0u8; 16];

        // Simulate the DraftScheduler::flush_submit output:
        // submission at seq=10, post-submit clear at seq=11.
        let mut submit_batch = tze_hud_input::DraftNotificationBatch::new();
        submit_batch.record_submission(tze_hud_input::DraftSubmission {
            text: "msg".to_string(),
            sequence: 10,
        });
        deliver_composer_batch(
            &tx_opt,
            "agent".to_string(),
            &node_id_bytes,
            &[0u8; 16],
            submit_batch,
        );

        let mut clear_batch = tze_hud_input::DraftNotificationBatch::new();
        clear_batch.coalesce_state(tze_hud_input::DraftStateNotification {
            text: String::new(),
            cursor: 0,
            selection_anchor: 0,
            at_capacity: false,
            sequence: 11,
        });
        deliver_composer_batch(
            &tx_opt,
            "agent".to_string(),
            &node_id_bytes,
            &[0u8; 16],
            clear_batch,
        );

        let (_ns, sub_out) = rx.try_recv().expect("submission event");
        let (_ns, clear_out) = rx.try_recv().expect("clear event");

        let sub_ev = envelope_variant(&sub_out.events[0]);
        let InputEvent::ComposerDraftSubmit(sub) = sub_ev else {
            panic!("expected ComposerDraftSubmit");
        };
        let clear_ev = envelope_variant(&clear_out.events[0]);
        let InputEvent::ComposerDraftState(clr) = clear_ev else {
            panic!("expected ComposerDraftState for clear");
        };

        assert!(
            clr.sequence > sub.sequence,
            "post-submit clear seq={} must exceed submission seq={}",
            clr.sequence,
            sub.sequence
        );
    }
}
