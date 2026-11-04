//! Lifecycle verbs on the resident session stream (`docs/api.md`):
//! `Publish`, `Clear`, and `Hold`, plus the one reply shape,
//! `RequestResult`.
//!
//! Surfaces are the strings MCP uses (`zone:<name>`, `widget:<name>`).
//! Failures carry a code from the shared closed set (`tze_hud_scene::error_codes::ERROR_CODES`) and a hint.
//!
//! `Hold` and `Clear` replies are cached by client sequence so a
//! retransmit replays the reply without applying the operation twice.

use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::Status;
use tze_hud_scene::element_store::ElementType;
use tze_hud_scene::error_codes::{ERROR_CODES, validation_error_code};
use tze_hud_scene::mutation::{MutationBatch as SceneMutationBatch, SceneMutation};
use tze_hud_scene::types::SceneId;

use crate::proto::session::server_message::Payload as ServerPayload;
use crate::proto::session::*;
use crate::session::SharedState;

use super::stream_session::StreamSession;
use super::{
    capability_set_covers, now_ms, now_wall_us, persist_element_store,
    touch_element_store_entry_by_namespace,
};

// ─── RequestResult ───────────────────────────────────────────────────────────

pub(super) fn ok(seq: u64) -> RequestResult {
    RequestResult {
        seq,
        ok: true,
        ..Default::default()
    }
}

/// A failed request. `code` must be in the shared closed set.
pub(super) fn fail(seq: u64, code: &str, hint: impl Into<String>) -> RequestResult {
    debug_assert!(ERROR_CODES.contains(&code), "{code} not in ERROR_CODES");
    RequestResult {
        seq,
        ok: false,
        code: code.to_string(),
        hint: rejection_hint(code, hint.into()),
        ..Default::default()
    }
}

/// Keep the rejection reason and add the next action for its existing category.
/// Request replays pass the same cached reason through this deterministic map.
fn rejection_hint(code: &str, reason: String) -> String {
    let action = match code {
        "INVALID_ARGUMENT" => "Correct the rejected request fields and resend as a new request",
        "NOT_ALLOWED" => {
            "Ask the operator to grant the required allow entry, then reconnect with a fresh SessionInit or valid SessionResume, then retry"
        }
        "NOT_HELD" => "Publish to obtain a holding before retrying",
        "SAFE_MODE_ACTIVE" => "Wait for SessionResumed before retrying",
        "ZONE_NOT_FOUND" => "Use a configured zone name, then resend Publish",
        "WIDGET_NOT_FOUND" => "Use a registered widget name, then resend Publish",
        "WIDGET_PARAMETER_INVALID" => {
            "Use parameter names, types and values from the widget definition, then resend Publish"
        }
        "CONTENT_REJECTED" => {
            "Use allowed content and a valid publication token; reduce publication/key counts or reuse an existing key before retrying"
        }
        "LEASE_NOT_ACTIVE" => "Wait for an active session before retrying",
        "BUDGET_EXCEEDED" => {
            "Reduce the request or clear unused holdings, then retry as a new request when capacity is available"
        }
        "TIMESTAMP_TOO_OLD" => {
            "Use a timestamp within the current session and resend as a new request"
        }
        "TIMESTAMP_TOO_FUTURE" => {
            "Move the presentation timestamp within the scheduling horizon and resend as a new request"
        }
        "TIMESTAMP_EXPIRY_BEFORE_PRESENT" => {
            "Set expiry after presentation or remove expiry, then resend as a new request"
        }
        "UNAVAILABLE" => "Wait for capacity before retrying with a new request sequence",
        _ => "Retry as a new request; if the failure persists, report it to the operator",
    };
    format!("{reason}; {action}")
}

pub(super) async fn send_result(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    result: RequestResult,
) {
    let seq = session.next_server_seq();
    let _ = tx
        .send(Ok(ServerMessage {
            sequence: seq,
            timestamp_wall_us: now_wall_us(),
            payload: Some(ServerPayload::RequestResult(result)),
        }))
        .await;
}

/// Send `result` and cache it under `seq` for retransmit replay.
async fn send_cached(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    result: RequestResult,
) {
    if result.seq > 0 {
        session
            .lease_correlation_cache
            .insert(result.seq, result.clone());
    }
    send_result(session, tx, result).await;
}

/// Replay a cached reply for a retransmitted request. Returns whether it did.
async fn replay_cached(
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
) -> bool {
    if seq == 0 {
        return false;
    }
    let Some(cached) = session.lease_correlation_cache.get(seq).cloned() else {
        return false;
    };
    send_result(session, tx, cached).await;
    true
}

// ─── Surfaces ────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Surface {
    Zone(String),
    Widget(String),
}

impl Surface {
    pub(super) fn parse(s: &str) -> Result<Self, &'static str> {
        const BAD: &str = "surface is zone:<name> or widget:<name>";
        let (kind, name) = s.split_once(':').ok_or(BAD)?;
        if name.is_empty() {
            return Err(BAD);
        }
        match kind {
            "zone" => Ok(Self::Zone(name.to_string())),
            "widget" => Ok(Self::Widget(name.to_string())),
            _ => Err(BAD),
        }
    }

    /// The internal permission this surface needs, and the allow entry that grants it.
    fn permission(&self) -> (String, String) {
        match self {
            Self::Zone(n) => (format!("publish_zone:{n}"), format!("zone:{n}")),
            Self::Widget(n) => (format!("publish_widget:{n}"), format!("widget:{n}")),
        }
    }
}

/// Parse `surface` and check the agent's allow list covers it.
fn allowed_surface(
    session: &StreamSession,
    seq: u64,
    s: &str,
) -> Result<Surface, Box<RequestResult>> {
    let surface =
        Surface::parse(s).map_err(|hint| Box::new(fail(seq, "INVALID_ARGUMENT", hint)))?;
    let (permission, entry) = surface.permission();
    if capability_set_covers(&session.capabilities, &permission) {
        Ok(surface)
    } else {
        Err(Box::new(not_allowed(seq, &session.agent_name, &entry)))
    }
}

fn not_allowed(seq: u64, agent: &str, entry: &str) -> RequestResult {
    fail(
        seq,
        "NOT_ALLOWED",
        format!("needs \"{entry}\" in [agents.{agent}] allow"),
    )
}

fn safe_mode(seq: u64) -> RequestResult {
    fail(
        seq,
        "SAFE_MODE_ACTIVE",
        "the human paused agents; retry after SessionResumed",
    )
}

fn not_held(seq: u64, surface: &str) -> RequestResult {
    fail(
        seq,
        "NOT_HELD",
        format!("you hold nothing on {surface}; Publish first"),
    )
}

// ─── Publish ─────────────────────────────────────────────────────────────────

/// Publish to a zone or widget. Durable surfaces reply with `RequestResult`;
/// ephemeral ones are fire-and-forget, success or failure (invariant 2).
pub(super) async fn handle_publish(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    publish: Publish,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    let surface = match allowed_surface(session, seq, &publish.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let (result, ephemeral, persist) = {
        let mut st = state.lock().await;
        let paused = session.safe_mode_active
            || st
                .safe_mode_atomic
                .load(std::sync::atomic::Ordering::Acquire);
        let mut scene = st.scene.lock().await;
        let (result, ephemeral, touched) = match &surface {
            Surface::Zone(zone) => {
                let ephemeral = scene
                    .zone_registry
                    .get_by_name(zone)
                    .is_some_and(|d| d.ephemeral);
                let r = if paused {
                    safe_mode(seq)
                } else {
                    publish_zone(&mut scene, session, seq, zone, &publish)
                };
                (r, ephemeral, Some((ElementType::Zone, zone.clone())))
            }
            Surface::Widget(widget) => {
                let ephemeral = scene
                    .widget_registry
                    .instances
                    .get(widget)
                    .and_then(|i| scene.widget_registry.definitions.get(&i.widget_type_name))
                    .is_some_and(|d| d.ephemeral);
                let r = if paused {
                    safe_mode(seq)
                } else {
                    publish_widget(&mut scene, session, seq, widget, &publish)
                };
                (r, ephemeral, Some((ElementType::Widget, widget.clone())))
            }
        };
        drop(scene);
        let persist = match touched {
            Some((ty, name)) if result.ok => {
                touch_element_store_entry_by_namespace(&mut st, ty, &name, now_ms())
            }
            _ => None,
        };
        (result, ephemeral, persist)
    };
    if result.ok {
        render_wake.notify();
    }
    persist_element_store(persist).await;
    if !ephemeral {
        send_result(session, tx, result).await;
    }
}

fn publish_zone(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    seq: u64,
    zone: &str,
    publish: &Publish,
) -> RequestResult {
    let Some(content) = publish
        .content
        .as_ref()
        .and_then(crate::convert::proto_zone_content_to_scene)
    else {
        return fail(seq, "INVALID_ARGUMENT", "zone publish needs content");
    };
    if let tze_hud_scene::types::ZoneContent::Notification(n) = &content
        && let Err(hint) = n.validate()
    {
        return fail(seq, "INVALID_ARGUMENT", hint);
    }
    if !publish.breakpoints.is_empty()
        && !matches!(content, tze_hud_scene::types::ZoneContent::StreamText(_))
    {
        return fail(
            seq,
            "INVALID_ARGUMENT",
            "breakpoints are only valid for stream_text content",
        );
    }
    let now = scene.now_wall_us();
    let zone_exists = scene.zone_registry.get_by_name(zone).is_some();
    if let Some((code, hint)) = timing_error(publish, now, zone, zone_exists) {
        return fail(seq, code, hint);
    }
    let expiry = expires_at_wall_us(publish, now).or_else(|| {
        matches!(&content, tze_hud_scene::ZoneContent::Notification(n) if !n.actions.is_empty())
            .then(|| {
                publish.present_at_us.max(now).saturating_add(
                    u64::from(session.publication_defaults.answer_window_ms()) * 1_000,
                )
            })
    });
    let batch = SceneMutationBatch {
        batch_id: SceneId::new(),
        agent_namespace: session.namespace.clone(),
        mutations: vec![SceneMutation::PublishToZone {
            zone_name: zone.to_string(),
            content,
            publish_token: tze_hud_scene::types::ZonePublishToken { token: Vec::new() },
            merge_key: (!publish.key.is_empty()).then(|| publish.key.clone()),
            // Content expiry (invariant 1): expires_at wins, else ttl_ms
            // counts from presentation.
            expires_at_wall_us: expiry,
            content_classification: None,
            breakpoints: publish.breakpoints.clone(),
            held: false,
        }],
        timing_hints: None,
        lease_id: None,
    };
    // present_at in the future: hold the publish until due (invariant 1).
    if publish.present_at_us > now {
        scene.schedule_batch_for_origin(
            publish.present_at_us,
            batch,
            Some(session.publication_origin),
        );
        return ok(seq);
    }
    let result = scene.apply_batch_for_origin(&batch, Some(session.publication_origin));
    match result.error {
        None if result.applied => ok(seq),
        Some(e) => fail(seq, validation_error_code(&e), e.to_string()),
        None => fail(seq, "INTERNAL", "zone publish was not applied"),
    }
}

fn publish_widget(
    scene: &mut tze_hud_scene::graph::SceneGraph,
    session: &StreamSession,
    seq: u64,
    widget: &str,
    publish: &Publish,
) -> RequestResult {
    let params = publish
        .params
        .iter()
        .filter_map(crate::convert::proto_to_widget_param_value)
        .collect();
    let now = scene.now_wall_us();
    match scene.publish_to_widget_for_origin(
        widget,
        params,
        &session.namespace,
        (!publish.key.is_empty()).then(|| publish.key.clone()),
        publish.transition_ms,
        expires_at_wall_us(publish, now),
        None,
        Some(session.publication_origin),
    ) {
        Ok(_) => ok(seq),
        Err(e) => fail(seq, validation_error_code(&e), e.to_string()),
    }
}

/// Absolute content expiry: `expires_at_us` if set, else `ttl_ms` counted
/// from presentation (now, or `present_at_us` if later), else none.
fn expires_at_wall_us(publish: &Publish, now_wall_us: u64) -> Option<u64> {
    if publish.expires_at_us > 0 {
        Some(publish.expires_at_us)
    } else if publish.ttl_ms > 0 {
        let shown_at = publish.present_at_us.max(now_wall_us);
        Some(shown_at.saturating_add(publish.ttl_ms.saturating_mul(1_000)))
    } else {
        None
    }
}

/// Reject timing that can never display, and scheduled publishes to unknown
/// zones (they would otherwise fail silently when due).
fn timing_error(
    publish: &Publish,
    now_wall_us: u64,
    zone: &str,
    zone_exists: bool,
) -> Option<(&'static str, String)> {
    let present = publish.present_at_us;
    if present > now_wall_us.saturating_add(super::DEFAULT_MAX_FUTURE_SCHEDULE_US) {
        return Some((
            "TIMESTAMP_TOO_FUTURE",
            format!("present_at_us ({present}) is beyond the scheduling horizon"),
        ));
    }
    if let Some(expires) = expires_at_wall_us(publish, now_wall_us)
        && expires <= present.max(now_wall_us)
    {
        return Some((
            "TIMESTAMP_EXPIRY_BEFORE_PRESENT",
            format!("expiry ({expires}) must be after the presentation time"),
        ));
    }
    if present > now_wall_us && !zone_exists {
        return Some(("ZONE_NOT_FOUND", format!("no zone {zone}")));
    }
    None
}

// ─── Clear ───────────────────────────────────────────────────────────────────

/// Release a zone or widget publication.
pub(super) async fn handle_clear(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    clear: Clear,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    if replay_cached(session, tx, seq).await {
        return;
    }
    let surface = match allowed_surface(session, seq, &clear.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let r = match &surface {
            Surface::Zone(zone) => scene.clear_zone_and_cancel_pending(zone, &session.namespace),
            Surface::Widget(widget) => scene.clear_widget_for_publisher(widget, &session.namespace),
        };
        match r {
            Ok(()) => ok(seq),
            Err(e) => fail(seq, validation_error_code(&e), e.to_string()),
        }
    };
    if result.ok {
        render_wake.notify();
    }
    send_cached(session, tx, result).await;
}

// ─── Hold ────────────────────────────────────────────────────────────────────

/// Extend a holding without resending content.
pub(super) async fn handle_hold(
    state: &Arc<Mutex<SharedState>>,
    session: &mut StreamSession,
    tx: &tokio::sync::mpsc::Sender<Result<ServerMessage, Status>>,
    seq: u64,
    hold: Hold,
    render_wake: &tze_hud_scene::render_wake::RenderWakeNotifier,
) {
    if replay_cached(session, tx, seq).await {
        return;
    }
    let surface = match allowed_surface(session, seq, &hold.surface) {
        Ok(s) => s,
        Err(r) => return send_result(session, tx, *r).await,
    };
    let result = {
        let st = state.lock().await;
        let mut scene = st.scene.lock().await;
        let expires =
            (hold.ttl_ms > 0).then(|| scene.now_wall_us().saturating_add(hold.ttl_ms * 1_000));
        let held_ttl = RequestResult {
            ttl_ms: hold.ttl_ms,
            ..ok(seq)
        };
        match &surface {
            Surface::Zone(zone) => {
                let ttl_us = (hold.ttl_ms > 0).then(|| hold.ttl_ms.saturating_mul(1_000));
                if scene.hold_zone_publications(zone, &session.namespace, ttl_us) {
                    held_ttl
                } else {
                    not_held(seq, &hold.surface)
                }
            }
            Surface::Widget(widget) => {
                if scene.hold_widget_publications(widget, &session.namespace, expires) {
                    held_ttl
                } else {
                    not_held(seq, &hold.surface)
                }
            }
        }
    };
    if result.ok {
        render_wake.notify();
    }
    send_cached(session, tx, result).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_parse_like_mcp() {
        assert_eq!(
            Surface::parse("zone:subtitle"),
            Ok(Surface::Zone("subtitle".into()))
        );
        assert_eq!(
            Surface::parse("widget:gauge"),
            Ok(Surface::Widget("gauge".into()))
        );
        let id = SceneId::new();
        assert!(Surface::parse(&format!("tile:{}", id.as_uuid())).is_err());
        assert!(Surface::parse("tile:nope").is_err());
        assert!(Surface::parse("zone:").is_err());
        assert!(Surface::parse("portal:x").is_err());
    }

    /// Every code the session server can put in a `RequestResult` is in the
    /// shared closed set (invariant 8).
    #[test]
    fn grpc_codes_are_in_the_shared_set() {
        let sources = [include_str!("verbs.rs")];
        let re = regex_lite_codes();
        for src in sources {
            for code in re(src) {
                assert!(
                    ERROR_CODES.contains(&code.as_str()),
                    "{code} not in ERROR_CODES"
                );
            }
        }
    }

    /// Quoted SHOUTY_SNAKE strings passed as codes: `fail(seq, "X"`, a
    /// `("X", …)` error tuple, or `"X".to_string()` in a code position.
    fn regex_lite_codes() -> impl Fn(&str) -> Vec<String> {
        |src: &str| {
            let mut out = Vec::new();
            for line in src.lines() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with("assert") {
                    continue;
                }
                let mut rest = line;
                while let Some(i) = rest.find('"') {
                    let after = &rest[i + 1..];
                    let Some(j) = after.find('"') else { break };
                    let lit = &after[..j];
                    let is_code = lit.len() > 3
                        && lit.contains('_')
                        && lit.chars().all(|c| c.is_ascii_uppercase() || c == '_');
                    let next = &after[j + 1..];
                    if is_code
                        && (next.starts_with(".to_string()")
                            || next.starts_with(',')
                            || next.starts_with(')'))
                        && !KNOWN_NON_CODES.contains(&lit)
                    {
                        out.push(lit.to_string());
                    }
                    rest = next;
                }
            }
            out
        }
    }

    /// Subscription category names, which share the SHOUTY_SNAKE style.
    const KNOWN_NON_CODES: &[&str] = &[
        "SCENE_TOPOLOGY",
        "INPUT_EVENTS",
        "FOCUS_EVENTS",
        "DEGRADATION_NOTICES",
        "TELEMETRY_FRAMES",
    ];
}
