use std::sync::Arc;
use std::time::Instant;

use winit::event::MouseScrollDelta;

use crate::os_prefs::WheelLines;
use tze_hud_input::{WHEEL_LINE_PX, WheelStep};

use crate::channels::{INPUT_EVENT_CAPACITY, InputEvent};
use tze_hud_protocol::proto::{EventBatch, InputEnvelope};
use tze_hud_scene::MonoUs;
use tze_hud_scene::graph::SceneGraph;

/// A wheel event as `(delta_x px, delta_y, unit of delta_y)`, positive
/// scrolling down/right (hud-ljhku.13).
///
/// A notch (`LineDelta`) scrolls the Windows "lines per notch" setting
/// (`setting`, read per event) in text lines, or one page for
/// `WHEEL_PAGESCROLL`; the scroll target sizes the line or page. Off
/// Windows a notch stays [`WHEEL_LINE_PX`]. Horizontal notches and pixel
/// deltas (touchpads) are unchanged.
pub(super) fn normalize_mouse_wheel_delta(
    delta: &MouseScrollDelta,
    setting: WheelLines,
) -> (f32, f32, WheelStep) {
    match *delta {
        MouseScrollDelta::LineDelta(x, y) => {
            let dx = -x * WHEEL_LINE_PX;
            match setting {
                WheelLines::Fixed => (dx, -y * WHEEL_LINE_PX, WheelStep::Pixels),
                WheelLines::Lines(n) => (dx, -y * n as f32, WheelStep::Lines),
                WheelLines::Page => (dx, -y, WheelStep::Pages),
            }
        }
        MouseScrollDelta::PixelDelta(pos) => (-(pos.x as f32), -(pos.y as f32), WheelStep::Pixels),
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

/// The text a key press types, or `None` for non-text keys and chords.
///
/// Shared by the composer/agent character path and the control-panel search.
/// A key with a layout character types that character. Text with no layout
/// key, such as a Windows `VK_PACKET` from the touch keyboard or `SendInput`
/// Unicode events, reaches winit as `Key::Unidentified` with the character
/// only in `KeyEvent::text`, so that text is used instead. Control characters
/// (Enter, Tab, Backspace) are never text here, so a `"\r"` is not routed to
/// an agent as a character; their named keys handle them.
///
/// Characters outside the BMP sent as two `VK_PACKET` surrogates are not
/// covered: winit 0.30 finalizes each half as its own key event and drops the
/// unpaired surrogate before it reaches this function.
pub(super) fn key_press_text(
    key: &winit::keyboard::Key,
    text: Option<&str>,
    mods: winit::keyboard::ModifiersState,
) -> Option<String> {
    use winit::keyboard::Key;
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return None;
    }
    match key {
        Key::Character(ch) => Some(ch.to_string()),
        Key::Unidentified(_) => text
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
            .map(str::to_string),
        Key::Named(_) | Key::Dead(_) => None,
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
    fn key_press_text_types_layout_and_packet_text_but_not_chords() {
        use winit::keyboard::{Key, ModifiersState as M, NamedKey, NativeKey};
        let none = M::empty();
        let packet = Key::Unidentified(NativeKey::Windows(0xE7));
        // Windows VK_PACKET: no layout character, the event text carries it.
        assert_eq!(
            key_press_text(&packet, Some("é"), none).as_deref(),
            Some("é")
        );
        assert_eq!(
            key_press_text(&packet, Some(" "), none).as_deref(),
            Some(" ")
        );
        assert_eq!(key_press_text(&packet, Some("\r"), none), None);
        assert_eq!(key_press_text(&packet, None, none), None);
        assert_eq!(key_press_text(&packet, Some("é"), M::CONTROL), None);
        // Layout characters type with Shift, not with Ctrl/Alt/Super chords.
        let h = Key::Character("h".into());
        assert_eq!(key_press_text(&h, Some("h"), none).as_deref(), Some("h"));
        assert_eq!(
            key_press_text(&h, Some("H"), M::SHIFT).as_deref(),
            Some("h")
        );
        for chord in [M::CONTROL, M::ALT, M::SUPER] {
            assert_eq!(key_press_text(&h, None, chord), None);
        }
        // Named keys never type text; their key-down handling owns them.
        assert_eq!(
            key_press_text(&Key::Named(NamedKey::Enter), Some("\r"), none),
            None
        );
        assert_eq!(
            key_press_text(&Key::Named(NamedKey::Space), Some(" "), none),
            None
        );
    }

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

    // Non-Windows (`WheelLines::Fixed`) keeps today's 40 px per notch.
    #[test]
    fn mouse_wheel_delta_positive_line_scrolls_toward_top() {
        let (x, y, step) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, 1.0), WheelLines::Fixed);

        assert_eq!(x, 0.0);
        assert_eq!(y, -40.0);
        assert_eq!(step, WheelStep::Pixels);
    }

    #[test]
    fn mouse_wheel_delta_negative_line_scrolls_down_transcript() {
        let (_, y, _) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, -1.0), WheelLines::Fixed);

        assert_eq!(y, 40.0);
    }

    // hud-ljhku.13 R13.3 S1: a notch is the Windows lines-per-notch setting,
    // in text lines the scroll target sizes; WHEEL_PAGESCROLL is one page.
    #[test]
    fn mouse_wheel_notch_uses_windows_lines_per_notch() {
        let notch_down = MouseScrollDelta::LineDelta(0.0, -1.0);
        assert_eq!(
            normalize_mouse_wheel_delta(&notch_down, WheelLines::Lines(5)),
            (0.0, 5.0, WheelStep::Lines)
        );
        // The setting is read per event: the next notch uses the new value.
        assert_eq!(
            normalize_mouse_wheel_delta(&notch_down, WheelLines::Lines(2)),
            (0.0, 2.0, WheelStep::Lines)
        );
        assert_eq!(
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, 2.0), WheelLines::Page),
            (0.0, -2.0, WheelStep::Pages)
        );
        // Horizontal notches and touchpad pixels are unchanged.
        assert_eq!(
            normalize_mouse_wheel_delta(
                &MouseScrollDelta::LineDelta(1.0, 0.0),
                WheelLines::Lines(5)
            ),
            (-40.0, -0.0, WheelStep::Lines)
        );
        let pixels = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, -12.0));
        assert_eq!(
            normalize_mouse_wheel_delta(&pixels, WheelLines::Lines(5)),
            (-0.0, 12.0, WheelStep::Pixels)
        );
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

    /// hud-ljhku.13 R13.3 S1: five lines per notch scroll five OUTPUT text
    /// rows on a portal (its shaped line height), five 40 px lines on a
    /// surface without text, and WHEEL_PAGESCROLL one visible page.
    #[test]
    fn wheel_lines_follow_the_target_text_line_height() {
        use std::sync::Arc;
        use tze_hud_portal::hub::{PortalHub, PortalKey, Publish};
        use tze_hud_scene::{NodeData, PortalDocumentLayout, PortalPartKind, Rect};

        let clock = tze_hud_scene::TestClock::from_us(200);
        let mut driver = crate::portal::PortalDriver::new(tze_hud_mcp::PortalHandle::new(
            PortalHub::default(),
            Arc::new(clock.clone()),
        ));
        driver
            .hub_mut()
            .publish(
                &PortalKey::new("agent", "wheel"),
                Publish {
                    text: Some("hello".into()),
                    ..Default::default()
                },
                100,
            )
            .expect("publish accepted");
        let mut scene = SceneGraph::new_with_clock(1920.0, 1080.0, Arc::new(clock.clone()));
        let tab = scene.create_tab("Main", 0).unwrap();
        let mut processor = tze_hud_input::InputProcessor::new();
        driver.drain(&mut scene, &mut processor, Some(tab));
        let tile_id = *scene.tiles.keys().next().expect("portal tile");
        let (node_id, source) = scene
            .nodes
            .iter()
            .find_map(
                |(id, node)| match (scene.portal_document_kind(tile_id, *id), &node.data) {
                    (Some(PortalPartKind::Transcript), NodeData::TextMarkdown(t)) => {
                        Some((*id, t.clone()))
                    }
                    _ => None,
                },
            )
            .expect("transcript document");
        let pane = scene
            .portal_surface(tile_id)
            .unwrap()
            .parts
            .iter()
            .find(|p| p.kind == PortalPartKind::Transcript)
            .unwrap()
            .bounds;
        let content: String = (0..100).map(|i| format!("l{i}\n")).collect();
        let rows: Vec<(usize, f32)> = (0..100).map(|i| (i * 4, i as f32 * 22.0)).collect();
        let tile = scene.tiles[&tile_id].bounds;
        scene.overlay.portal_document_layouts.insert(
            (tile_id, PortalPartKind::Transcript),
            PortalDocumentLayout {
                node_id,
                signature: [3; 32],
                source,
                tile_size: (tile.width, tile.height),
                font_scale: scene.tile_font_scale(tile_id),
                content: content.into(),
                height: 2200.0,
                viewport: Rect::new(pane.x, pane.y, pane.width, 110.0),
                line_height: 22.0,
                rows: rows.into(),
            },
        );
        processor.reconcile_portal_document_layouts(&mut scene);
        let (x, y) = (tile.x + pane.x + pane.width / 2.0, tile.y + pane.y + 5.0);
        assert_eq!(
            scene.portal_document_at(tile_id, x, y),
            Some(PortalPartKind::Transcript)
        );
        // Scrolled back to the top first so downward notches have room.
        let top = scene.tile_scroll_offset_local(tile_id).1;
        processor.process_scroll_event(
            &tze_hud_input::ScrollEvent {
                x,
                y,
                delta_x: 0.0,
                delta_y: -top,
            },
            &mut scene,
        );
        let offset = |scene: &SceneGraph| scene.tile_scroll_offset_local(tile_id).1;
        let start = offset(&scene);
        let (_, dy, step) = normalize_mouse_wheel_delta(
            &MouseScrollDelta::LineDelta(0.0, -1.0),
            WheelLines::Lines(5),
        );
        let notch =
            |scene: &mut SceneGraph, processor: &mut tze_hud_input::InputProcessor, dy, step| {
                processor.process_wheel_event(
                    &tze_hud_input::ScrollEvent {
                        x,
                        y,
                        delta_x: 0.0,
                        delta_y: dy,
                    },
                    step,
                    scene,
                );
            };
        notch(&mut scene, &mut processor, dy, step);
        assert_eq!(offset(&scene) - start, 5.0 * 22.0, "five OUTPUT rows");
        let before = offset(&scene);
        let (_, dy, step) =
            normalize_mouse_wheel_delta(&MouseScrollDelta::LineDelta(0.0, -1.0), WheelLines::Page);
        notch(&mut scene, &mut processor, dy, step);
        assert_eq!(offset(&scene) - before, 110.0, "one visible page");

        // A surface without text: five 40 px lines.
        let lease = scene.grant_lease("plain", 60_000);
        let plain = scene
            .create_tile(
                tab,
                "plain",
                lease,
                Rect::new(1500.0, 600.0, 300.0, 300.0),
                50,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(plain, tze_hud_scene::TileScrollConfig::vertical())
            .unwrap();
        let (_, dy, step) = normalize_mouse_wheel_delta(
            &MouseScrollDelta::LineDelta(0.0, -1.0),
            WheelLines::Lines(5),
        );
        processor.process_wheel_event(
            &tze_hud_input::ScrollEvent {
                x: 1600.0,
                y: 700.0,
                delta_x: 0.0,
                delta_y: dy,
            },
            step,
            &mut scene,
        );
        assert_eq!(scene.tile_scroll_offset_local(plain).1, 5.0 * 40.0);
    }
}
