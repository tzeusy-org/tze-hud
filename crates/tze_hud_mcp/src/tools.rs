//! The five MCP verbs (`docs/api.md`): `hud_surfaces`, `hud_publish`,
//! `hud_hold`, `hud_clear`, `hud_input`.
//!
//! Every verb acts on a surface string handed out by `hud_surfaces`:
//! `zone:<name>`, `widget:<name>`, or `portal:<id>`. Identity comes from the
//! caller's PSK (the namespace is the agent id), so no call carries a
//! namespace or token. Times are milliseconds. Failures are
//! [`McpError::Tool`] with a stable code and a hint naming the next call.

use crate::publish_wait::{PublishReply, WaitClaim};
use crate::{
    error::McpError,
    server::{InputNotifiers, PortalHandle},
    types::McpResult,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::Mutex;
use tze_hud_portal::hub::{PortalError, PortalHub, PortalKey, PortalStatus, Publish};
use tze_hud_scene::{
    SceneId, ValidationError,
    config::AgentIdentity,
    graph::{NotificationOutcome, PendingActionKind, SceneGraph},
    mutation::{MutationBatch, SceneMutation},
    render_wake::RenderWakeNotifier,
    types::{
        LeaseState, NotificationAction, NotificationPayload, Rgba, StatusBarPayload,
        WidgetParamType, WidgetParameterValue, ZoneContent, ZoneMediaType, ZonePublishToken,
    },
};

/// Default content lifetime for a zone publish without `ttl_ms`.
pub const DEFAULT_ZONE_TTL_MS: u64 = 60_000;
/// Longest `delay_ms` accepted (matches the gRPC scheduling horizon).
pub const MAX_DELAY_MS: u64 = 300_000;
/// Lifetime of an MCP agent's lease; every publish and hold renews it. The
/// lease is bookkeeping (lease expiry clears the agent's publications); content
/// lifetime is the per-publication expiry.
pub const MCP_LEASE_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Longest `wait_ms` long-poll for `hud_input`.
pub const MAX_WAIT_MS: u64 = 30_000;

/// Build a tool error: a stable code plus a hint for the model.
pub fn tool_err(code: &'static str, hint: impl Into<String>) -> McpError {
    McpError::Tool {
        code,
        hint: hint.into(),
    }
}

fn invalid(hint: impl Into<String>) -> McpError {
    tool_err("INVALID_ARGUMENT", hint)
}

// ─── Per-agent MCP state ─────────────────────────────────────────────────────

/// A notification action press delivered to an agent and not yet acked.
/// Redelivered on every `hud_input` until acked. (Portal input is queued and
/// redelivered by the portal hub.)
#[derive(Clone, Debug)]
struct ActionItem {
    id: String,
    zone: String,
    kind: PendingActionKind,
    key: Option<String>,
}

#[derive(Default, Debug)]
struct AgentState {
    /// The agent's MCP lease (renewed by publish and hold).
    lease: Option<SceneId>,
    /// Delivered, unacked action presses, oldest first.
    delivered: Vec<ActionItem>,
}

/// Server-side MCP state, keyed by agent id.
#[derive(Default, Debug)]
pub struct McpState {
    agents: std::sync::Mutex<HashMap<String, AgentState>>,
}

impl McpState {
    fn try_with<R>(&self, agent: &str, f: impl FnOnce(&mut AgentState) -> R) -> McpResult<R> {
        let mut agents = match self.agents.try_lock() {
            Ok(agents) => agents,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(tool_err(
                    "INTERNAL",
                    "input state unavailable; retry hud_input",
                ));
            }
        };
        Ok(f(agents.entry(agent.to_string()).or_default()))
    }

    fn with<R>(&self, agent: &str, f: impl FnOnce(&mut AgentState) -> R) -> R {
        let mut agents = self.agents.lock().unwrap_or_else(|p| p.into_inner());
        f(agents.entry(agent.to_string()).or_default())
    }
}

/// Everything a verb needs for one call.
pub struct ToolCtx<'a> {
    pub scene: &'a Arc<Mutex<SceneGraph>>,
    pub portals: Option<&'a PortalHandle>,
    pub input_notifiers: &'a InputNotifiers,
    #[cfg(any(test, feature = "test-harness"))]
    pub input_wait_probe: Option<&'a tokio::sync::watch::Sender<bool>>,
    pub portal_wake: &'a RenderWakeNotifier,
    pub render_wake: &'a RenderWakeNotifier,
    pub state: &'a McpState,
    /// The runtime's safe-mode flag, shared with gRPC. While set, every
    /// mutating verb is refused.
    pub safe_mode: &'a AtomicBool,
    pub agent: &'a AgentIdentity,
    /// Server-owned implicit widget motion; never a model-visible parameter.
    pub widget_transition_ms: u32,
    pub publication_defaults: &'a tze_hud_scene::PublicationDefaults,
}

/// The one error every mutating verb returns while the human has paused agents.
fn safe_mode_active() -> McpError {
    tool_err(
        "SAFE_MODE_ACTIVE",
        "the human paused agents; retry after safe mode ends",
    )
}

/// Refuse a mutating verb during safe mode (all surfaces, including portals).
fn check_not_safe_mode(ctx: &ToolCtx<'_>) -> McpResult<()> {
    if ctx.safe_mode.load(Ordering::Acquire) {
        return Err(safe_mode_active());
    }
    Ok(())
}

// ─── Surfaces ────────────────────────────────────────────────────────────────

/// A parsed surface string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Surface {
    Zone(String),
    Widget(String),
    Portal(String),
}

impl Surface {
    pub fn parse(s: &str) -> McpResult<Self> {
        let bad =
            || invalid("surface is zone:<name>, widget:<name>, or portal:<id>; call hud_surfaces");
        let (kind, name) = s.split_once(':').ok_or_else(bad)?;
        if name.is_empty() {
            return Err(bad());
        }
        match kind {
            "zone" => Ok(Self::Zone(name.into())),
            "widget" => Ok(Self::Widget(name.into())),
            "portal" => Ok(Self::Portal(name.into())),
            _ => Err(bad()),
        }
    }

    /// The internal permission this surface needs, and the allow entry that grants it.
    pub fn permission(&self) -> (String, String) {
        match self {
            Self::Zone(n) => (format!("publish_zone:{n}"), format!("zone:{n}")),
            Self::Widget(n) => (format!("publish_widget:{n}"), format!("widget:{n}")),
            Self::Portal(_) => ("resident_mcp".into(), "portal".into()),
        }
    }
}

/// Reject a surface the agent's allow list doesn't cover.
fn check_allowed(ctx: &ToolCtx<'_>, surface: &Surface) -> McpResult<()> {
    let (permission, entry) = surface.permission();
    if ctx.agent.allows(&permission) {
        Ok(())
    } else {
        Err(not_allowed(&ctx.agent.agent_id, &entry))
    }
}

pub fn not_allowed(agent_id: &str, allow_entry: &str) -> McpError {
    tool_err(
        "NOT_ALLOWED",
        format!(
            "needs \"{allow_entry}\" in [agents.{agent_id}] allow; call hud_surfaces for what you may use"
        ),
    )
}

fn now_us(scene: &SceneGraph) -> u64 {
    scene.now_wall_us()
}

/// How long until `ns`'s last publication on `widget` clears, counting the
/// owner's auto-clear limit (which a hold cannot extend).
fn widget_expires_in(
    scene: &tze_hud_scene::graph::SceneGraph,
    widget: &str,
    ns: &str,
    now: u64,
) -> Option<u64> {
    scene
        .widget_registry
        .active_publishes
        .get(widget)
        .into_iter()
        .flatten()
        .filter(|r| r.publisher_namespace == ns)
        .map(|r| scene.widget_expiry(r))
        .chain(scene.scheduled_widget_publication_expiries(widget, ns))
        .filter_map(|expiry| remaining_ms(expiry, now))
        .max()
}

/// How long until the publication `ns` just made on `widget` (under `key`)
/// clears, counting the owner's auto-clear limit. Other publications of
/// `ns` there (other keys, a stack) keep their own expiry.
fn published_expires_in(
    scene: &tze_hud_scene::graph::SceneGraph,
    widget: &str,
    ns: &str,
    key: Option<&str>,
    now: u64,
) -> Option<u64> {
    let mine: Vec<_> = scene
        .widget_registry
        .active_publishes
        .get(widget)
        .into_iter()
        .flatten()
        .filter(|r| r.publisher_namespace == ns)
        .collect();
    let keyed: Vec<_> = mine
        .iter()
        .filter(|r| r.merge_key.as_deref() == key)
        .collect();
    let candidates = if keyed.is_empty() {
        mine.iter().collect()
    } else {
        keyed
    };
    // The newest; on a tie the last stored, which is the one just made.
    let record = candidates
        .into_iter()
        .max_by_key(|r| r.published_at_wall_us)?;
    remaining_ms(scene.widget_expiry(record), now)
}

fn remaining_ms(expires_at_us: Option<u64>, now_us: u64) -> Option<u64> {
    expires_at_us.map(|e| e.saturating_sub(now_us) / 1_000)
}

fn accepts(types: &[ZoneMediaType]) -> String {
    types
        .iter()
        .filter_map(|t| match t {
            ZoneMediaType::StreamText => Some("text"),
            ZoneMediaType::ShortTextWithIcon => Some("notification"),
            ZoneMediaType::KeyValuePairs => Some("status_bar"),
            ZoneMediaType::StaticImage => None,
            ZoneMediaType::SolidColor => Some("solid_color"),
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn widget_params(scene: &SceneGraph, instance: &str) -> Value {
    let mut out = Map::new();
    let Some(def) = scene
        .widget_registry
        .instances
        .get(instance)
        .and_then(|i| scene.widget_registry.definitions.get(&i.widget_type_name))
    else {
        return Value::Object(out);
    };
    for d in &def.parameter_schema {
        if d.audience != tze_hud_scene::WidgetParamAudience::Model {
            continue;
        }
        let c = d.constraints.as_ref();
        let ty = match d.param_type {
            WidgetParamType::Deadline => "deadline<=24h".into(),
            WidgetParamType::F32 => match (c.and_then(|c| c.f32_min), c.and_then(|c| c.f32_max)) {
                (Some(lo), Some(hi)) => format!("number {lo}..{hi}"),
                _ => "number".into(),
            },
            WidgetParamType::String => {
                match c.and_then(|c| c.string_max_bytes).filter(|n| *n > 0) {
                    Some(n) => format!("string<={n}"),
                    None => "string".into(),
                }
            }
            WidgetParamType::Color => "color".into(),
            WidgetParamType::Enum => match c {
                Some(c) if !c.enum_allowed_values.is_empty() => {
                    format!("enum {}", c.enum_allowed_values.join("|"))
                }
                _ => "enum".into(),
            },
        };
        out.insert(d.name.clone(), json!(ty));
    }
    Value::Object(out)
}

// ─── hud_surfaces ────────────────────────────────────────────────────────────

/// List the surfaces this agent may use and what it holds now.
pub async fn hud_surfaces(ctx: &ToolCtx<'_>) -> McpResult<Value> {
    let ns = ctx.agent.agent_id.as_str();
    let mut surfaces = Vec::new();
    {
        let lock_started = tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG)
            .then(std::time::Instant::now);
        if lock_started.is_some() {
            tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "scene_lock_wait",
                operation = "surfaces"
            );
        }
        let scene = ctx.scene.lock().await;
        if let Some(started) = &lock_started {
            tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "scene_lock_acquired",
                operation = "surfaces",
                elapsed_us = started.elapsed().as_micros()
            );
        }
        let now = now_us(&scene);
        let mut zones: Vec<_> = scene.zone_registry.zones.values().collect();
        zones.sort_by(|a, b| a.name.cmp(&b.name));
        for z in zones {
            let surface = Surface::Zone(z.name.clone());
            if !ctx.agent.allows(&surface.permission().0) {
                continue;
            }
            let mut o = Map::new();
            o.insert("s".into(), json!(format!("zone:{}", z.name)));
            o.insert("accepts".into(), json!(accepts(&z.accepted_media_types)));
            match z.contention_policy {
                tze_hud_scene::ContentionPolicy::LatestWins
                | tze_hud_scene::ContentionPolicy::Replace => {
                    o.insert("one".into(), json!(true));
                    if scene
                        .zone_registry
                        .active_publishes
                        .get(&z.name)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.publisher_namespace != ns
                                    && item.expires_at_wall_us.is_none_or(|expiry| expiry > now)
                            })
                        })
                    {
                        o.insert("busy".into(), json!(true));
                    }
                }
                tze_hud_scene::ContentionPolicy::Stack { max_depth } => {
                    o.insert("max".into(), json!(max_depth));
                }
                _ => {}
            }
            let expiries: Vec<_> = scene
                .zone_registry
                .active_publishes
                .get(&z.name)
                .into_iter()
                .flatten()
                .filter(|r| r.publisher_namespace == ns)
                .map(|r| r.expires_at_wall_us)
                .chain(scene.scheduled_zone_publication_expiries(&z.name, ns))
                .collect();
            if !expiries.is_empty() {
                o.insert("held".into(), json!(true));
            }
            if let Some(ms) = expiries
                .iter()
                .filter_map(|expiry| remaining_ms(*expiry, now))
                .max()
            {
                o.insert("expires_in_ms".into(), json!(ms));
            }
            surfaces.push(Value::Object(o));
        }
        let mut widgets: Vec<_> = scene.widget_registry.instances.keys().cloned().collect();
        widgets.sort();
        for w in widgets {
            let surface = Surface::Widget(w.clone());
            if !ctx.agent.allows(&surface.permission().0) {
                continue;
            }
            let mut o = Map::new();
            o.insert("s".into(), json!(format!("widget:{w}")));
            o.insert("params".into(), widget_params(&scene, &w));
            let mine: Vec<_> = scene
                .widget_registry
                .active_publishes
                .get(&w)
                .into_iter()
                .flatten()
                .filter(|r| r.publisher_namespace == ns)
                .collect();
            if !mine.is_empty() {
                o.insert("held".into(), json!(true));
                if let Some(ms) = widget_expires_in(&scene, &w, ns, now) {
                    o.insert("expires_in_ms".into(), json!(ms));
                }
            }
            surfaces.push(Value::Object(o));
        }
    }
    if portals_enabled(ctx) {
        // Absent service stays undiscoverable; a present failed Hub refuses.
        let portals = with_portals(ctx, |hub, _| (Ok(hub.list(ns)), false))?;
        for p in portals {
            let mut o = Map::new();
            o.insert("s".into(), json!(format!("portal:{}", p.id)));
            o.insert("state".into(), json!(p.status.as_str()));
            if p.pending_input > 0 {
                o.insert("pending_input".into(), json!(p.pending_input));
            }
            surfaces.push(Value::Object(o));
        }
    }
    Ok(json!({ "surfaces": surfaces }))
}

// ─── hud_publish ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishParams {
    pub surface: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub params: Option<Map<String, Value>>,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    #[serde(default)]
    pub delay_ms: Option<u64>,
    #[serde(default)]
    pub wait_ms: Option<u64>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub expects_reply: Option<bool>,
    #[serde(default)]
    pub display_name: Option<String>,
}

pub fn parse_args<T: for<'de> Deserialize<'de>>(args: Value) -> McpResult<T> {
    serde_json::from_value(args).map_err(|e| invalid(e.to_string()))
}

/// Publish to a surface: zone content, widget params, or portal output.
pub(crate) async fn hud_publish<'a>(ctx: &ToolCtx<'a>, args: Value) -> McpResult<PublishReply<'a>> {
    let started = tokio::time::Instant::now();
    let p: PublishParams = parse_args(args)?;
    let surface = Surface::parse(&p.surface)?;
    check_allowed(ctx, &surface)?;
    check_not_safe_mode(ctx)?;
    check_fields(&surface, &p)?;
    match surface {
        Surface::Zone(zone) => publish_zone(ctx, &zone, p, started).await,
        Surface::Widget(widget) => publish_widget(ctx, &widget, p)
            .await
            .map(PublishReply::plain),
        Surface::Portal(pid) => publish_portal(ctx, &pid, p).await.map(PublishReply::plain),
    }
}

/// Reject fields that don't apply to the surface kind, so a mistaken call
/// fails loudly instead of silently dropping intent.
fn check_fields(surface: &Surface, p: &PublishParams) -> McpResult<()> {
    let misplaced = match surface {
        Surface::Zone(_) => [
            ("params", p.params.is_some()),
            ("status", p.status.is_some()),
            ("expects_reply", p.expects_reply.is_some()),
            ("display_name", p.display_name.is_some()),
        ]
        .into_iter()
        .find(|(_, set)| *set),
        Surface::Widget(_) => [
            ("wait_ms", p.wait_ms.is_some()),
            ("content", p.content.is_some()),
            ("status", p.status.is_some()),
            ("expects_reply", p.expects_reply.is_some()),
            ("display_name", p.display_name.is_some()),
        ]
        .into_iter()
        .find(|(_, set)| *set),
        Surface::Portal(_) => [
            ("wait_ms", p.wait_ms.is_some()),
            ("params", p.params.is_some()),
            ("delay_ms", p.delay_ms.is_some()),
        ]
        .into_iter()
        .find(|(_, set)| *set),
    };
    match misplaced {
        Some((field, _)) => Err(invalid(format!("{field} doesn't apply to {}", p.surface))),
        None => Ok(()),
    }
}

/// The agent's MCP lease: reuse (and renew) it while active, else grant one.
/// A Suspended lease means safe mode: never grant around it.
fn ensure_lease(ctx: &ToolCtx<'_>, scene: &mut SceneGraph) -> McpResult<SceneId> {
    let ns = ctx.agent.agent_id.as_str();
    let existing = ctx.state.with(ns, |s| s.lease);
    if let Some(id) = existing {
        match scene.leases.get(&id).map(|l| l.state) {
            Some(LeaseState::Suspended) => return Err(safe_mode_active()),
            Some(LeaseState::Active) if scene.renew_lease(id, MCP_LEASE_TTL_MS).is_ok() => {
                return Ok(id);
            }
            _ => {}
        }
    }
    let id = scene.grant_lease(ns, MCP_LEASE_TTL_MS);
    ctx.state.with(ns, |s| s.lease = Some(id));
    Ok(id)
}

fn ok_with_expiry(expires_in_ms: Option<u64>) -> Value {
    match expires_in_ms {
        Some(ms) => json!({ "ok": true, "expires_in_ms": ms }),
        None => json!({ "ok": true }),
    }
}

fn zone_names(scene: &SceneGraph) -> String {
    let mut names: Vec<_> = scene.zone_registry.zones.keys().cloned().collect();
    names.sort();
    names.join(", ")
}

/// Map a scene rejection to a stable code and hint.
pub fn scene_error(e: &ValidationError) -> McpError {
    use ValidationError as V;
    match e {
        V::ZoneNotFound { name } => tool_err(
            "ZONE_NOT_FOUND",
            format!("no zone {name}; call hud_surfaces"),
        ),
        V::ZoneMediaTypeMismatch { zone } => tool_err(
            "CONTENT_REJECTED",
            format!("zone {zone} doesn't accept this content type; see accepts in hud_surfaces"),
        ),
        V::ZoneMaxPublishersReached { zone, .. } | V::ZoneMaxKeysReached { zone, .. } => tool_err(
            "CONTENT_REJECTED",
            format!("zone {zone} is full; hud_clear your older publication or reuse its key"),
        ),
        V::ZonePublishSafeModeActive { .. } => tool_err(
            "SAFE_MODE_ACTIVE",
            "the human paused agents; retry after safe mode ends",
        ),
        V::ZonePublishLeaseOrphaned { .. }
        | V::ZonePublishLeaseNotActive { .. }
        | V::ZonePublishLeaseNotFound { .. } => tool_err("LEASE_NOT_ACTIVE", "retry hud_publish"),
        V::WidgetNotFound { name } => tool_err(
            "WIDGET_NOT_FOUND",
            format!("no widget {name}; call hud_surfaces"),
        ),
        V::WidgetUnknownParameter { widget, param } => tool_err(
            "WIDGET_PARAMETER_INVALID",
            format!("{widget} has no param {param}; see params in hud_surfaces"),
        ),
        V::WidgetParameterTypeMismatch {
            widget,
            param,
            expected,
        } => tool_err(
            "WIDGET_PARAMETER_INVALID",
            format!("{widget}.{param} must be {expected}"),
        ),
        V::WidgetParameterInvalidValue {
            widget,
            param,
            reason,
        } => tool_err(
            "WIDGET_PARAMETER_INVALID",
            format!("{widget}.{param}: {reason}"),
        ),
        V::WidgetMaxPublishersReached { widget, .. } => tool_err(
            "CONTENT_REJECTED",
            format!("widget {widget} is full; retry later"),
        ),
        other => tool_err(
            tze_hud_scene::error_codes::validation_error_code(other),
            other.to_string(),
        ),
    }
}

/// Infer the content `type` from what the zone accepts when an object omits it.
fn with_inferred_type(content: Value, accepted: &[ZoneMediaType]) -> Value {
    match content {
        Value::Object(mut o) if !o.contains_key("type") => {
            let ty = match accepted.first() {
                Some(ZoneMediaType::ShortTextWithIcon) => "notification",
                Some(ZoneMediaType::KeyValuePairs) => "status_bar",
                Some(ZoneMediaType::SolidColor) => "solid_color",
                Some(ZoneMediaType::StaticImage) => "static_image",
                _ => "stream_text",
            };
            o.insert("type".into(), json!(ty));
            Value::Object(o)
        }
        other => other,
    }
}

async fn publish_zone<'a>(
    ctx: &ToolCtx<'a>,
    zone: &str,
    p: PublishParams,
    started: tokio::time::Instant,
) -> McpResult<PublishReply<'a>> {
    let ns = ctx.agent.agent_id.clone();
    let raw = p
        .content
        .filter(|c| !c.is_null() && c.as_str() != Some(""))
        .ok_or_else(|| {
            invalid("zone publish needs content (a string, or an object for structured zones)")
        })?;
    let delay_ms = p.delay_ms.unwrap_or(0);
    if delay_ms > MAX_DELAY_MS {
        return Err(tool_err(
            "TIMESTAMP_TOO_FUTURE",
            format!("delay_ms is at most {MAX_DELAY_MS}"),
        ));
    }
    let lock_started = tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG)
        .then(std::time::Instant::now);
    if lock_started.is_some() {
        tracing::debug!(
            target: "tze_hud_mcp_phases",
            phase = "scene_lock_wait",
            operation = "publish_zone"
        );
    }
    let mut scene = ctx.scene.lock().await;
    if let Some(started) = &lock_started {
        tracing::debug!(
            target: "tze_hud_mcp_phases",
            phase = "scene_lock_acquired",
            operation = "publish_zone",
            elapsed_us = started.elapsed().as_micros()
        );
    }
    let Some(def) = scene.zone_registry.zones.get(zone) else {
        return Err(tool_err(
            "ZONE_NOT_FOUND",
            format!("no zone {zone}; known: {}", zone_names(&scene)),
        ));
    };
    let content = parse_zone_content(&with_inferred_type(raw, &def.accepted_media_types))?;
    if p.wait_ms.is_some()
        && !matches!(&content, ZoneContent::Notification(n) if !n.actions.is_empty())
    {
        return Err(invalid(
            "wait_ms needs a notification with actions; omit it for other content",
        ));
    }
    if matches!(&content, ZoneContent::Notification(_)) {
        SceneGraph::validate_notification_key(p.key.as_deref()).map_err(|e| scene_error(&e))?;
    }
    // Read the owner default exactly once, after resolving the content. Explicit
    // zero remains held; ordinary zone content keeps its historical default.
    let ttl_ms = p.ttl_ms.unwrap_or_else(|| {
        if matches!(&content, ZoneContent::Notification(n) if !n.actions.is_empty()) {
            u64::from(ctx.publication_defaults.answer_window_ms())
        } else {
            DEFAULT_ZONE_TTL_MS
        }
    });
    let lease_id = ensure_lease(ctx, &mut scene)?;
    let deadline =
        started + std::time::Duration::from_millis(p.wait_ms.unwrap_or(0).min(MAX_WAIT_MS));
    let claim = if p.wait_ms.is_some_and(|ms| ms > 0) {
        let ready = ctx.input_notifiers.try_for_agent(&ns)?;
        let active = Arc::new(AtomicBool::new(true));
        let id = SceneId::new();
        let signal = RenderWakeNotifier::new(move || ready.notify_waiters());
        scene.register_action_notifier(&ns, signal.clone());
        // The scene classifies acceptance independently of this task being
        // polled. Preserve the original call window, including admission time.
        let remaining_us = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .as_micros() as u64; // bounded by MAX_WAIT_MS
        let deadline_mono_us = scene.now_monotonic_us().saturating_add(remaining_us);
        scene.arm_notification_wait(
            id,
            &ns,
            Arc::clone(&active),
            deadline_mono_us,
            signal.clone(),
        );
        Some(WaitClaim {
            scene: Arc::clone(ctx.scene),
            id,
            namespace: ns.clone(),
            active,
            ready: signal,
        })
    } else {
        None
    };
    let now = now_us(&scene);
    let present_at = now.saturating_add(delay_ms.saturating_mul(1_000));
    let expires_at = (ttl_ms > 0).then(|| present_at.saturating_add(ttl_ms.saturating_mul(1_000)));
    if delay_ms > 0 {
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns.clone(),
            mutations: vec![SceneMutation::PublishToZone {
                zone_name: zone.to_string(),
                content,
                publish_token: ZonePublishToken { token: Vec::new() },
                merge_key: p.key,
                expires_at_wall_us: expires_at,
                content_classification: None,
                breakpoints: Vec::new(),
                held: expires_at.is_none(),
            }],
            timing_hints: None,
            lease_id: Some(lease_id),
        };
        let batch_id = batch.batch_id;
        if let Some(claim) = &claim {
            scene.schedule_notification_batch(present_at, batch, claim.id);
        } else {
            scene.schedule_batch(present_at, batch);
        }
        if zone == "subtitle"
            && tracing::enabled!(target: "tze_hud_delay_diagnostic", tracing::Level::DEBUG)
        {
            let span = tracing::Span::current();
            if span
                .metadata()
                .is_some_and(|m| m.name() == "mcp_request" && m.target() == "tze_hud_mcp_phases")
                && let Some(span_id) = span.id()
            {
                let (batch_hi, batch_lo) = batch_id.as_uuid().as_u64_pair();
                tracing::debug!(target: "tze_hud_delay_diagnostic",
                    event = "delayed_enqueued", request_span_id = span_id.into_u64(),
                    batch_hi, batch_lo, scheduled_at_wall_us = now, present_at_wall_us = present_at,
                    expires_at_wall_us = expires_at, is_subtitle = true);
            }
        }
    } else if let Some(claim) = &claim {
        scene
            .publish_notification_until_with_id(
                zone, content, &ns, lease_id, p.key, expires_at, claim.id,
            )
            .map_err(|error| scene_error(&error))?;
    } else {
        scene
            .publish_to_zone_until_with_lease(zone, content, &ns, lease_id, p.key, expires_at)
            .map_err(|error| scene_error(&error))?;
    }
    let wakes = scene.take_notification_wait_notifiers();
    drop(scene);
    for wake in wakes {
        wake.notify();
    }
    // A quiescent renderer must present/schedule this question before its call waits.
    ctx.render_wake.notify();
    let Some(claim) = claim else {
        return Ok(PublishReply::plain(ok_with_expiry(
            expires_at.map(|at| at.saturating_sub(now) / 1_000),
        )));
    };
    let activity = InputPollLease::start(ctx, &ns, deadline)?;
    let ready = ctx.input_notifiers.try_for_agent(&ns)?;
    loop {
        let mut notified = std::pin::pin!(ready.notified());
        notified.as_mut().enable();
        let mut scene = match ctx.scene.try_lock() {
            Ok(scene) => scene,
            Err(_) => tokio::time::timeout_at(deadline, ctx.scene.lock())
                .await
                .map_err(|_| tool_err("INTERNAL", "input state busy; retry hud_input"))?,
        };
        match scene.notification_wait_outcome(claim.id) {
            Some(NotificationOutcome::Answered) => {
                let action = scene
                    .notification_wait_answer(claim.id, &ns)
                    .expect("accepted claimed answer stays in FIFO");
                let mut reply =
                    ok_with_expiry(expires_at.map(|at| at.saturating_sub(now_us(&scene)) / 1_000));
                match &action.kind {
                    PendingActionKind::Callback(callback) => {
                        reply["action"] = json!(callback);
                    }
                    PendingActionKind::Dismiss => {
                        reply["action"] = json!("dismiss");
                        reply["kind"] = json!("dismiss");
                    }
                }
                return Ok(PublishReply::answered(reply, scene, claim, activity));
            }
            Some(NotificationOutcome::Gone(reason)) => {
                let reply = PublishReply::plain(json!({"ok":true,"gone":reason.as_str()}));
                scene.release_notification_wait(claim.id);
                drop(scene);
                return Ok(reply);
            }
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            // Read a live retimed holding, including a still-scheduled admission.
            let expiry = scene
                .zone_registry
                .active_for_zone(zone)
                .iter()
                .find(|r| r.publication_id == claim.id)
                .map(|r| r.expires_at_wall_us)
                .or_else(|| {
                    scene
                        .scheduled_batches
                        .iter()
                        .find(|b| b.notification_publication == Some(claim.id))
                        .and_then(|b| b.batch.mutations.first())
                        .and_then(|m| match m {
                            SceneMutation::PublishToZone {
                                expires_at_wall_us, ..
                            } => Some(*expires_at_wall_us),
                            _ => None,
                        })
                })
                .unwrap_or(expires_at);
            scene.release_notification_wait(claim.id);
            let reply = ok_with_expiry(expiry.map(|at| at.saturating_sub(now_us(&scene)) / 1_000));
            drop(scene);
            return Ok(PublishReply::plain(reply));
        }
        drop(scene);
        #[cfg(any(test, feature = "test-harness"))]
        let _waiting = ctx.input_wait_probe.map(crate::server::InputWaitGuard::arm);
        tokio::select! {
            _ = notified.as_mut() => {},
            _ = tokio::time::sleep_until(deadline) => {},
        }
    }
}

async fn publish_widget(ctx: &ToolCtx<'_>, widget: &str, p: PublishParams) -> McpResult<Value> {
    let ns = ctx.agent.agent_id.clone();
    let delay_ms = p.delay_ms.unwrap_or(0);
    if delay_ms > MAX_DELAY_MS {
        return Err(tool_err(
            "TIMESTAMP_TOO_FUTURE",
            format!("delay_ms is at most {MAX_DELAY_MS}"),
        ));
    }
    let params = p
        .params
        .ok_or_else(|| invalid("widget publish needs params; see params in hud_surfaces"))?;
    let mut scene = ctx.scene.lock().await;
    if !scene.widget_registry.instances.contains_key(widget) {
        return Err(tool_err(
            "WIDGET_NOT_FOUND",
            format!("no widget {widget}; call hud_surfaces"),
        ));
    }
    let mut typed = HashMap::new();
    for (name, value) in &params {
        typed.insert(
            name.clone(),
            json_to_widget_param_value(value, name, &scene, widget)?,
        );
    }
    let typed = scene
        .validate_widget_params(widget, &typed)
        .map_err(|e| scene_error(&e))?;
    let lease_id = ensure_lease(ctx, &mut scene)?;
    let ttl_ms = p.ttl_ms.unwrap_or(0);
    let now = now_us(&scene);
    let ttl_us = (ttl_ms > 0).then(|| ttl_ms.saturating_mul(1_000));
    if delay_ms > 0 {
        let present_at = now.saturating_add(delay_ms.saturating_mul(1_000));
        let owner_ttl = scene
            .widget_registry
            .instances
            .get(widget)
            .and_then(|instance| instance.auto_clear_ms)
            .map(|ms| ms.saturating_mul(1_000));
        let expiry = match (ttl_us, owner_ttl) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|ttl| present_at.saturating_add(ttl));
        scene.schedule_batch(
            present_at,
            MutationBatch {
                batch_id: SceneId::new(),
                agent_namespace: ns,
                mutations: vec![SceneMutation::PublishToWidget {
                    widget_name: widget.into(),
                    params: typed,
                    merge_key: p.key,
                    transition_ms: ctx.widget_transition_ms,
                    ttl_us,
                    expires_at_wall_us: None,
                }],
                timing_hints: None,
                lease_id: Some(lease_id),
            },
        );
        return Ok(ok_with_expiry(remaining_ms(expiry, now)));
    }
    let expires = ttl_us.map(|ttl| now.saturating_add(ttl));
    let key = p.key.clone();
    scene
        .publish_to_widget_for_lease(
            widget,
            typed,
            &ns,
            p.key,
            ctx.widget_transition_ms,
            expires,
            Some(lease_id),
        )
        .map_err(|e| scene_error(&e))?;
    let now = now_us(&scene);
    Ok(ok_with_expiry(published_expires_in(
        &scene,
        widget,
        &ns,
        key.as_deref(),
        now,
    )))
}

/// Whether this agent may hold portals and the portal driver is wired, so
/// discovery and input polling ask it.
fn portals_enabled(ctx: &ToolCtx<'_>) -> bool {
    ctx.portals.is_some()
        && ctx
            .agent
            .allows(&Surface::Portal(String::new()).permission().0)
}

/// Complete one synchronous Hub operation, then wake eligible work unlocked.
fn with_portals<T>(
    ctx: &ToolCtx<'_>,
    apply: impl FnOnce(&mut PortalHub, u64) -> (McpResult<T>, bool),
) -> McpResult<T> {
    let unavailable = || {
        tool_err(
            "UNAVAILABLE",
            "the portal service isn't running; retry later",
        )
    };
    let portals = ctx.portals.ok_or_else(unavailable)?;
    let portal_started = tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG)
        .then(std::time::Instant::now);
    if portal_started.is_some() {
        tracing::debug!(target: "tze_hud_mcp_phases", phase = "portal_lock_wait");
    }
    let (result, changed) = {
        let mut hub = portals.hub.lock().map_err(|_| {
            if let Some(started) = &portal_started {
                tracing::debug!(
                    target: "tze_hud_mcp_phases",
                    phase = "portal_lock_error",
                    code = "INTERNAL",
                    elapsed_us = started.elapsed().as_micros()
                );
            }
            tool_err(
                "INTERNAL",
                "the portal service failed; retry after it restarts",
            )
        })?;
        if let Some(started) = &portal_started {
            tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "portal_lock_acquired",
                elapsed_us = started.elapsed().as_micros()
            );
        }
        apply(&mut hub, portals.clock.now_us())
    };
    if let Some(started) = &portal_started {
        // The current Hub returns synchronously; this is not a remote acknowledgement.
        tracing::debug!(
            target: "tze_hud_mcp_phases",
            phase = "portal_reply",
            code = match &result {
                Ok(_) => "OK",
                Err(error) => error.code(),
            },
            elapsed_us = started.elapsed().as_micros()
        );
    }
    if result.is_ok() && changed {
        ctx.portal_wake.notify();
    }
    result
}

/// A portal refusal in the shared error set; `NotHeld` names the surface.
fn portal_err(e: PortalError, surface: &str) -> McpError {
    match e {
        PortalError::NotHeld => not_held(surface),
        e => {
            let (code, hint) = crate::error::map_portal(e);
            tool_err(code, hint)
        }
    }
}

async fn publish_portal(ctx: &ToolCtx<'_>, pid: &str, p: PublishParams) -> McpResult<Value> {
    let text = match p.content {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err(invalid("portal content is the output text (a string)")),
    };
    let status = p
        .status
        .as_deref()
        .map(PortalStatus::parse)
        .transpose()
        .map_err(|_| invalid("portal status must be active or degraded; hud_clear detaches"))?;
    let outcome = with_portals(ctx, |hub, now| {
        if let Err(error) = check_not_safe_mode(ctx) {
            return (Err(error), false);
        }
        (
            hub.publish_with_hold(
                &PortalKey::new(ctx.agent.agent_id.clone(), pid),
                Publish {
                    display_name: p.display_name,
                    text,
                    key: p.key,
                    expects_reply: p.expects_reply.unwrap_or(false),
                    status,
                },
                p.ttl_ms,
                now,
            )
            .map_err(|e| portal_err(e, &p.surface)),
            true,
        )
    })?;
    let mut result = ok_with_expiry(p.ttl_ms.filter(|ms| *ms > 0));
    result["reattached"] = json!(outcome.reattached);
    result["pending_input"] = json!(outcome.pending_input);
    Ok(result)
}

// ─── hud_hold ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldParams {
    pub surface: String,
    pub ttl_ms: u64,
}

fn not_held(surface: &str) -> McpError {
    tool_err(
        "NOT_HELD",
        format!("you hold nothing on {surface}; hud_publish first"),
    )
}

/// Extend a holding without resending content. `ttl_ms` 0 holds until cleared.
pub async fn hud_hold(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: HoldParams = parse_args(args)?;
    let surface = Surface::parse(&p.surface)?;
    check_allowed(ctx, &surface)?;
    check_not_safe_mode(ctx)?;
    let ns = ctx.agent.agent_id.clone();
    let expiry = |now: u64| (p.ttl_ms > 0).then(|| now.saturating_add(p.ttl_ms * 1_000));
    let mut result = ok_with_expiry((p.ttl_ms > 0).then_some(p.ttl_ms));
    match surface {
        Surface::Zone(zone) => {
            let mut scene = ctx.scene.lock().await;
            let ttl_us = (p.ttl_ms > 0).then(|| p.ttl_ms.saturating_mul(1_000));
            if !scene.hold_zone_publications(&zone, &ns, ttl_us) {
                return Err(not_held(&p.surface));
            }
            ensure_lease(ctx, &mut scene)?;
        }
        Surface::Widget(widget) => {
            let mut scene = ctx.scene.lock().await;
            let expires = expiry(now_us(&scene));
            if !scene.hold_widget_publications(&widget, &ns, expires) {
                return Err(not_held(&p.surface));
            }
            ensure_lease(ctx, &mut scene)?;
            // The owner's auto-clear limit may end it sooner.
            let now = now_us(&scene);
            result = ok_with_expiry(widget_expires_in(&scene, &widget, &ns, now));
        }
        Surface::Portal(pid) => {
            // The runtime keeps a held portal (and its transcript) past the
            // idle reclaim until the hold lapses or hud_clear.
            with_portals(ctx, |hub, now| {
                if let Err(error) = check_not_safe_mode(ctx) {
                    return (Err(error), false);
                }
                (
                    hub.hold(&PortalKey::new(ns, pid), p.ttl_ms, now)
                        .map_err(|e| portal_err(e, &p.surface)),
                    true,
                )
            })?;
        }
    }
    Ok(result)
}

// ─── hud_clear ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClearParams {
    pub surface: String,
    #[serde(default)]
    pub key: Option<String>,
}

/// Release a zone publication, a widget publication, or a portal (detach).
pub async fn hud_clear(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: ClearParams = parse_args(args)?;
    let surface = Surface::parse(&p.surface)?;
    check_allowed(ctx, &surface)?;
    let key = p.key.as_deref().filter(|key| !key.is_empty());
    if key.is_some() && !matches!(&surface, Surface::Zone(_)) {
        return Err(invalid("keyed clear requires a notification zone"));
    }
    let ns = ctx.agent.agent_id.clone();
    match surface {
        Surface::Zone(zone) => {
            let mut scene = ctx.scene.lock().await;
            match key {
                Some(key) => scene.clear_notification_key(&zone, &ns, key),
                None => scene.clear_zone_and_cancel_pending(&zone, &ns),
            }
            .map_err(|e| scene_error(&e))?;
        }
        Surface::Widget(widget) => {
            let mut scene = ctx.scene.lock().await;
            scene
                .clear_widget_for_publisher(&widget, &ns)
                .map_err(|e| scene_error(&e))?;
        }
        Surface::Portal(pid) => {
            with_portals(ctx, |hub, _| {
                (
                    hub.clear(&PortalKey::new(ns, pid))
                        .map_err(|e| portal_err(e, &p.surface)),
                    true,
                )
            })?;
        }
    }
    Ok(json!({ "ok": true }))
}

// ─── hud_input ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputParams {
    #[serde(default)]
    pub ack: Vec<String>,
    #[serde(default)]
    pub wait_ms: Option<u64>,
    #[serde(default)]
    pub max_items: Option<usize>,
}

static NEXT_ACTION_ID: AtomicU64 = AtomicU64::new(1);

/// The runtime shares a synchronous Hub mutex. Cooperatively wait only while
/// it is contended, without blocking an executor or leaving a detached owner
/// after cancellation. Ordinary empty input waits still use readiness below.
async fn with_input_hub<T>(
    portals: &PortalHandle,
    deadline: tokio::time::Instant,
    apply: impl FnOnce(&mut PortalHub) -> McpResult<T>,
) -> McpResult<T> {
    loop {
        match portals.hub.try_lock() {
            Ok(mut hub) => return apply(&mut hub),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(tool_err(
                    "INTERNAL",
                    "input state unavailable; retry hud_input",
                ));
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(tool_err("INTERNAL", "input state busy; retry hud_input"));
        }
        tokio::time::sleep_until(deadline.min(now + std::time::Duration::from_millis(1))).await;
    }
}

/// Own one finite active poll. Drop releases ownership without inventing
/// activity, including cancellation before the first portal attachment.
pub(crate) struct InputPollLease {
    portals: PortalHandle,
    namespace: String,
    request: u64,
    active: Arc<AtomicBool>,
    wake: RenderWakeNotifier,
}

impl InputPollLease {
    async fn start(
        ctx: &ToolCtx<'_>,
        namespace: &str,
        deadline: tokio::time::Instant,
    ) -> McpResult<Option<Self>> {
        if !portals_enabled(ctx) || tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        let portals = ctx.portals.expect("enabled portal handle");
        let active = Arc::new(AtomicBool::new(true));
        let (request, changed) = with_input_hub(portals, deadline, |hub| {
            let before = hub.next_deadline();
            let remaining_us = deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_micros()
                .min(u64::MAX as u128) as u64;
            let request = hub
                .begin_input_poll(
                    namespace,
                    remaining_us,
                    portals.clock.now_us(),
                    Arc::clone(&active),
                )
                .map_err(|_| {
                    tool_err("INTERNAL", "input ownership unavailable; retry hud_input")
                })?;
            let changed = !hub.list(namespace).is_empty() && before != hub.next_deadline();
            Ok((request, changed))
        })
        .await?;
        if changed {
            ctx.portal_wake.notify();
        }
        Ok(request.map(|request| Self {
            portals: portals.clone(),
            namespace: namespace.to_string(),
            request,
            active,
            wake: ctx.portal_wake.clone(),
        }))
    }
}

impl Drop for InputPollLease {
    fn drop(&mut self) {
        let changed = match self.portals.hub.try_lock() {
            Ok(mut hub) => {
                let before = hub.next_deadline();
                self.active.store(false, Ordering::Release);
                hub.end_input_poll(&self.namespace, self.request);
                !hub.list(&self.namespace).is_empty() && before != hub.next_deadline()
            }
            Err(_) => {
                // Logical cancellation never waits for the Hub. Its next
                // deadline/sweep ignores this entry; a single availability
                // wake re-evaluates any previously deferred idle deadline.
                self.active.store(false, Ordering::Release);
                true
            }
        };
        if changed {
            self.wake.notify();
        }
    }
}

/// Ack earlier items, then return input from every held surface, oldest
/// first. Unacked items are redelivered.
pub async fn hud_input(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: InputParams = parse_args(args)?;
    let ns = ctx.agent.agent_id.clone();

    // Acks first, so a poll+ack is one round trip.
    ctx.state
        .try_with(&ns, |s| s.delivered.retain(|i| !p.ack.contains(&i.id)))?;
    let mut ack = p.ack;

    let wait_ms = p.wait_ms.unwrap_or(0).min(MAX_WAIT_MS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    let max_items = p.max_items.unwrap_or(usize::MAX);
    let input_ready = ctx.input_notifiers.try_for_agent(&ns)?;
    let _active_poll = InputPollLease::start(ctx, &ns, deadline).await?;
    let action_ready = Arc::clone(&input_ready);
    {
        let mut scene = match ctx.scene.try_lock() {
            Ok(scene) => scene,
            Err(_) => tokio::time::timeout_at(deadline, ctx.scene.lock())
                .await
                .map_err(|_| McpError::Tool {
                    code: "INTERNAL",
                    hint: "input state busy; retry hud_input".to_string(),
                })?,
        };
        scene.register_action_notifier(
            &ns,
            RenderWakeNotifier::new(move || action_ready.notify_waiters()),
        );
    }
    loop {
        // Register before both authoritative predicates: a producer between
        // the check and await then leaves this waiter ready, not a lost wake.
        let mut notified = std::pin::pin!(input_ready.notified());
        notified.as_mut().enable();
        let (mut items, mut backlog) = {
            let mut scene = match ctx.scene.try_lock() {
                Ok(scene) => scene,
                Err(_) => tokio::time::timeout_at(deadline, ctx.scene.lock())
                    .await
                    .map_err(|_| tool_err("INTERNAL", "input state busy; retry hud_input"))?,
            };
            ctx.state.try_with(&ns, |s| {
                // Both guards exist before removal. No await occurs between
                // destructive take and retained-until-ack state commit.
                for a in scene.take_pending_actions(&ns) {
                    s.delivered.push(ActionItem {
                        id: format!("a{}", NEXT_ACTION_ID.fetch_add(1, Ordering::Relaxed)),
                        zone: a.zone_name,
                        kind: a.kind,
                        key: a.key,
                    });
                }
                let items: Vec<Value> = s.delivered.iter().take(max_items)
                    .map(|a| {
                        let mut item = match &a.kind {
                        PendingActionKind::Callback(callback) => {
                            json!({"id":a.id,"s":format!("zone:{}",a.zone),"action":callback})
                        }
                        PendingActionKind::Dismiss => {
                            json!({"id":a.id,"s":format!("zone:{}",a.zone),"action":"dismiss","kind":"dismiss"})
                        }
                        };
                        if let Some(key) = &a.key {
                            item["key"] = json!(key);
                        }
                        item
                    })
                    .collect();
                let backlog = s.delivered.len() - items.len();
                (items, backlog)
            })?
        };
        if portals_enabled(ctx) {
            let portals = ctx.portals.expect("enabled portal handle");
            let (batch, changed) = with_input_hub(portals, deadline, |hub| {
                Ok(hub.poll_input_with_changes(
                    &ns,
                    &std::mem::take(&mut ack),
                    Some(max_items - items.len()),
                    portals.clock.now_us(),
                ))
            })
            .await?;
            if changed {
                ctx.portal_wake.notify();
            }
            backlog += batch.remaining;
            items.extend(batch.items.into_iter().map(
                |i| json!({ "id": i.id, "s": format!("portal:{}", i.portal), "text": i.text }),
            ));
        }
        let now = tokio::time::Instant::now();
        if !items.is_empty() || backlog > 0 || now >= deadline {
            return Ok(json!({ "items": items, "remaining": backlog }));
        }
        {
            // Observe only the armed wait after both authoritative predicates.
            // Every wake or dispatch cancellation clears it before any recheck.
            #[cfg(any(test, feature = "test-harness"))]
            let _waiting = ctx.input_wait_probe.map(crate::server::InputWaitGuard::arm);
            tokio::select! {
                _ = notified.as_mut() => {},
                _ = tokio::time::sleep_until(deadline) => {},
            }
        }
        // A deadline wake takes one final predicate pass; it never resets the
        // request deadline or repeatedly touches the Hub while idle.
    }
}

// ─── Content parsing ─────────────────────────────────────────────────────────

/// Keys each typed content object may carry, with the shape a nested wrapper
/// (`{"notification":{..}}`) should have been sent as.
const CONTENT_KEYS: &[(&str, &[&str], &str)] = &[
    ("stream_text", &["type", "text"], r#"{"text":..}"#),
    (
        "notification",
        &[
            "type", "title", "body", "text", "icon", "urgency", "ttl_ms", "actions",
        ],
        r#"{"title":..,"body":..}"#,
    ),
    ("status_bar", &["type", "entries"], r#"{"entries":{..}}"#),
    (
        "solid_color",
        &["type", "r", "g", "b", "a"],
        r#"{"r":..,"g":..,"b":..}"#,
    ),
    (
        "static_image",
        &["type", "resource_id"],
        r#"{"resource_id":..}"#,
    ),
];

/// Reject any key `ty` content does not read, so a wrong shape fails loudly
/// instead of rendering defaults.
fn reject_unknown_keys(obj: &serde_json::Map<String, Value>, ty: &str) -> Result<(), McpError> {
    let Some((_, allowed, _)) = CONTENT_KEYS.iter().find(|(t, ..)| *t == ty) else {
        return Ok(());
    };
    let Some(key) = obj.keys().find(|k| !allowed.contains(&k.as_str())) else {
        return Ok(());
    };
    if let Some((_, _, shape)) = CONTENT_KEYS.iter().find(|(t, ..)| t == key) {
        return Err(invalid(format!(
            "content is not nested; send {key} fields directly: {shape}"
        )));
    }
    Err(invalid(format!(
        "unknown field `{key}` in {ty} content; allowed: {}",
        allowed.join(", ")
    )))
}

/// Parse the polymorphic `content` field into a `ZoneContent`.
///
/// - Plain string → `StreamText`
/// - Object with `"type"` → dispatched by variant name
fn parse_zone_content(content: &Value) -> Result<ZoneContent, McpError> {
    match content {
        Value::String(s) => Ok(ZoneContent::StreamText(s.clone())),
        Value::Object(obj) => {
            let type_str = obj
                .get("type")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    invalid(
                        "object content must have a \"type\" field (one of: stream_text, notification, status_bar, solid_color, static_image)".to_string(),
                    )
                })?;
            reject_unknown_keys(obj, type_str)?;
            match type_str {
                "stream_text" => {
                    let text = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .filter(|t| !t.is_empty())
                        .ok_or_else(|| invalid("stream_text content needs a non-empty \"text\""))?
                        .to_string();
                    Ok(ZoneContent::StreamText(text))
                }
                "notification" => {
                    let text = obj
                        .get("text")
                        .or_else(|| obj.get("body"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let icon = obj
                        .get("icon")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let urgency = match obj.get("urgency") {
                        None => 1,
                        Some(value) => value.as_u64().filter(|v| *v <= 3).ok_or_else(|| {
                            invalid("notification.urgency must be an integer in 0..3")
                        })? as u32,
                    };
                    if obj.contains_key("ttl_ms") {
                        return Err(invalid(
                            "notification.content.ttl_ms is not supported; use top-level hud_publish ttl_ms",
                        ));
                    }
                    let title = obj
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let actions: Vec<NotificationAction> = match obj.get("actions") {
                        None => Vec::new(),
                        Some(Value::Array(arr)) => arr
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                let o = item.as_object().ok_or_else(|| {
                                    invalid(format!(
                                        "notification.actions[{index}] must be an object"
                                    ))
                                })?;
                                let label = o
                                    .get("label")
                                    .and_then(|v| v.as_str())
                                    .filter(|v| !v.is_empty())
                                    .ok_or_else(|| {
                                        invalid(format!(
                                            "notification.actions[{index}].label must be a non-empty string"
                                        ))
                                    })?
                                    .to_string();
                                let target = |name: &str| -> Result<Option<String>, McpError> {
                                    o.get(name).map(|value| value.as_str().map(str::to_owned)
                                        .ok_or_else(|| invalid(format!(
                                            "notification.actions[{index}].{name} must be a string"))))
                                        .transpose()
                                };
                                let action = NotificationAction {
                                    label,
                                    callback_id: target("callback_id")?,
                                    url: target("url")?,
                                };
                                // Preserve the existing MCP non-empty callback rule,
                                // after exclusivity so both supplied members never alias.
                                if action.url.is_none() && action.callback_id.as_deref() == Some("") {
                                    return Err(invalid(format!(
                                        "notification.actions[{index}].callback_id must be a non-empty string")));
                                }
                                Ok(action)
                            })
                            .collect::<Result<Vec<_>, McpError>>()?,
                        Some(_) => {
                            return Err(invalid(
                                "notification.actions must be an array".to_string(),
                            ))
                        }
                    };
                    let n = NotificationPayload {
                        text,
                        icon,
                        urgency,
                        ttl_ms: None,
                        title,
                        actions,
                    };
                    n.validate().map_err(invalid)?;
                    Ok(ZoneContent::Notification(n))
                }
                "status_bar" => {
                    let values = obj.get("entries").and_then(Value::as_object)
                        .filter(|m| !m.is_empty())
                        .ok_or_else(|| invalid("status_bar content must have a non-empty entries object with string values"))?;
                    let entries = values
                        .iter()
                        .map(|(key, value)| {
                            value
                                .as_str()
                                .map(|v| (key.clone(), v.to_string()))
                                .ok_or_else(|| {
                                    invalid(format!("status_bar.entries.{key} must be a string"))
                                })
                        })
                        .collect::<McpResult<HashMap<_, _>>>()?;
                    Ok(ZoneContent::StatusBar(StatusBarPayload { entries }))
                }
                "solid_color" => {
                    let channel = |k: &str| {
                        obj.get(k)
                            .and_then(Value::as_f64)
                            .map(|v| v as f32)
                            .ok_or_else(|| {
                                invalid("solid_color content needs numeric r, g and b (a optional)")
                            })
                    };
                    let (r, g, b) = (channel("r")?, channel("g")?, channel("b")?);
                    let a = obj.get("a").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32;
                    Ok(ZoneContent::SolidColor(Rgba { r, g, b, a }))
                }
                "static_image" => {
                    use tze_hud_scene::types::ResourceId;
                    let hex = obj
                        .get("resource_id")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            invalid(
                                "static_image content must have a \"resource_id\" field (hex-encoded 32-byte BLAKE3 hash)".to_string(),
                            )
                        })?;
                    // Decode hex without an external crate: parse pairs of chars as u8.
                    if hex.len() != 64 {
                        return Err(invalid(format!(
                            "static_image \"resource_id\" must be 64 hex chars (32 bytes), got {}",
                            hex.len()
                        )));
                    }
                    let mut raw = [0u8; 32];
                    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
                        let hi = char::from(chunk[0]).to_digit(16);
                        let lo = char::from(chunk[1]).to_digit(16);
                        if let (Some(hi), Some(lo)) = (hi, lo) {
                            raw[i] = (hi * 16 + lo) as u8;
                        } else {
                            return Err(invalid(format!(
                                "static_image \"resource_id\" is not valid hex: \"{hex}\""
                            )));
                        }
                    }
                    Ok(ZoneContent::StaticImage(ResourceId::from_bytes(raw)))
                }
                other => Err(invalid(format!(
                    "unknown content type \"{other}\"; expected one of: stream_text, notification, status_bar, solid_color, static_image"
                ))),
            }
        }
        _ => Err(invalid(
            "content must be a string or an object with a \"type\" field".to_string(),
        )),
    }
}

/// Convert a JSON value to a typed widget parameter per the instance's schema.
fn json_to_widget_param_value(
    v: &Value,
    param_name: &str,
    scene: &SceneGraph,
    widget_name: &str,
) -> McpResult<WidgetParameterValue> {
    let bad = |want: &str| {
        tool_err(
            "WIDGET_PARAMETER_INVALID",
            format!("{widget_name}.{param_name} must be {want}"),
        )
    };
    let decl = scene
        .widget_registry
        .instances
        .get(widget_name)
        .and_then(|i| scene.widget_registry.definitions.get(&i.widget_type_name))
        .and_then(|d| d.parameter_schema.iter().find(|d| d.name == param_name))
        .ok_or_else(|| {
            tool_err(
                "WIDGET_PARAMETER_INVALID",
                format!("{widget_name} has no param {param_name}; see params in hud_surfaces"),
            )
        })?;
    SceneGraph::validate_widget_param_audience(widget_name, decl).map_err(|e| scene_error(&e))?;
    Ok(match decl.param_type {
        WidgetParamType::Deadline => {
            WidgetParameterValue::Deadline(tze_hud_scene::WidgetDeadline::relative(
                v.as_i64().ok_or_else(|| bad(&decl.value_hint()))?,
            ))
        }
        WidgetParamType::F32 => {
            let number = v.as_f64().ok_or_else(|| bad(&decl.value_hint()))?;
            WidgetParameterValue::F32(
                SceneGraph::validate_widget_f32_value(widget_name, param_name, decl, number)
                    .map_err(|e| scene_error(&e))?,
            )
        }
        WidgetParamType::String => WidgetParameterValue::String(
            v.as_str()
                .ok_or_else(|| bad(&decl.value_hint()))?
                .to_string(),
        ),
        WidgetParamType::Color => {
            let shape = "#RRGGBB or {r,g,b,a?} with finite numeric channels in 0..1";
            if let Some(hex) = v.as_str() {
                let bytes = hex.as_bytes();
                if bytes.len() != 7
                    || bytes[0] != b'#'
                    || !bytes[1..].iter().all(u8::is_ascii_hexdigit)
                {
                    return Err(bad(shape));
                }
                let channel =
                    |start| u8::from_str_radix(&hex[start..start + 2], 16).unwrap() as f32 / 255.0;
                return Ok(WidgetParameterValue::Color(Rgba::new(
                    channel(1),
                    channel(3),
                    channel(5),
                    1.0,
                )));
            }
            let o = v.as_object().ok_or_else(|| bad(shape))?;
            let c = |k: &str| {
                let value = match o.get(k) {
                    None if k == "a" => return Ok(1.0),
                    Some(value) => value.as_f64(),
                    None => None,
                }
                .ok_or_else(|| bad(&format!("{shape}; {k} must be numeric")))?;
                if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                    return Err(bad(&format!("{shape}; {k} is outside 0..1")));
                }
                Ok(value as f32)
            };
            WidgetParameterValue::Color(Rgba {
                r: c("r")?,
                g: c("g")?,
                b: c("b")?,
                a: c("a")?,
            })
        }
        WidgetParamType::Enum => WidgetParameterValue::Enum(
            v.as_str()
                .ok_or_else(|| bad(&decl.value_hint()))?
                .to_string(),
        ),
    })
}
