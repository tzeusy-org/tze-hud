//! The five MCP verbs (`docs/api.md`): `hud_surfaces`, `hud_publish`,
//! `hud_hold`, `hud_clear`, `hud_input`.
//!
//! Every verb acts on a surface string handed out by `hud_surfaces`:
//! `zone:<name>`, `widget:<name>`, or `portal:<id>`. Identity comes from the
//! caller's PSK (the namespace is the agent id), so no call carries a
//! namespace or token. Times are milliseconds. Failures are
//! [`McpError::Tool`] with a stable code and a hint naming the next call.

use crate::{error::McpError, portal_op::PortalOp, types::McpResult};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::Mutex;
use tze_hud_projection::hub::PortalError;
use tze_hud_scene::{
    SceneId, ValidationError,
    config::AgentIdentity,
    graph::SceneGraph,
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
const POLL_INTERVAL_MS: u64 = 150;

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
    action: String,
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
    fn with<R>(&self, agent: &str, f: impl FnOnce(&mut AgentState) -> R) -> R {
        let mut agents = self.agents.lock().unwrap_or_else(|p| p.into_inner());
        f(agents.entry(agent.to_string()).or_default())
    }
}

/// Everything a verb needs for one call.
pub struct ToolCtx<'a> {
    pub scene: &'a Arc<Mutex<SceneGraph>>,
    pub portal_op_tx: Option<&'a tokio::sync::mpsc::UnboundedSender<PortalOp>>,
    pub portal_wake: &'a RenderWakeNotifier,
    pub state: &'a McpState,
    /// The runtime's safe-mode flag, shared with gRPC. While set, every
    /// mutating verb is refused.
    pub safe_mode: &'a AtomicBool,
    pub agent: &'a AgentIdentity,
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

fn remaining_ms(expires_at_us: Option<u64>, now_us: u64) -> Option<u64> {
    expires_at_us.map(|e| e.saturating_sub(now_us) / 1_000)
}

fn accepts(types: &[ZoneMediaType]) -> String {
    types
        .iter()
        .map(|t| match t {
            ZoneMediaType::StreamText => "text",
            ZoneMediaType::ShortTextWithIcon => "notification",
            ZoneMediaType::KeyValuePairs => "status_bar",
            ZoneMediaType::StaticImage => "static_image",
            ZoneMediaType::SolidColor => "solid_color",
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
        let c = d.constraints.as_ref();
        let ty = match d.param_type {
            WidgetParamType::F32 => match (c.and_then(|c| c.f32_min), c.and_then(|c| c.f32_max)) {
                (Some(lo), Some(hi)) => format!("f32 {lo}..{hi}"),
                _ => "f32".into(),
            },
            WidgetParamType::String => "string".into(),
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
        let scene = ctx.scene.lock().await;
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
            let mine: Vec<_> = scene
                .zone_registry
                .active_publishes
                .get(&z.name)
                .into_iter()
                .flatten()
                .filter(|r| r.publisher_namespace == ns)
                .collect();
            if !mine.is_empty() {
                o.insert("held".into(), json!(true));
            }
            if let Some(ms) = mine
                .iter()
                .filter_map(|r| remaining_ms(r.expires_at_wall_us, now))
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
                if let Some(ms) = mine
                    .iter()
                    .filter_map(|r| remaining_ms(r.expires_at_wall_us, now))
                    .max()
                {
                    o.insert("expires_in_ms".into(), json!(ms));
                }
            }
            surfaces.push(Value::Object(o));
        }
    }
    if portals_enabled(ctx) {
        // Discovery still answers if the portal service is down.
        let agent = ns.to_string();
        let portals = portal_call(ctx, |reply| PortalOp::List { agent, reply })
            .await
            .unwrap_or_default();
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
pub async fn hud_publish(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: PublishParams = parse_args(args)?;
    let surface = Surface::parse(&p.surface)?;
    check_allowed(ctx, &surface)?;
    check_not_safe_mode(ctx)?;
    check_fields(&surface, &p)?;
    match surface {
        Surface::Zone(zone) => publish_zone(ctx, &zone, p).await,
        Surface::Widget(widget) => publish_widget(ctx, &widget, p).await,
        Surface::Portal(pid) => publish_portal(ctx, &pid, p).await,
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
            ("content", p.content.is_some()),
            ("delay_ms", p.delay_ms.is_some()),
            ("status", p.status.is_some()),
            ("expects_reply", p.expects_reply.is_some()),
            ("display_name", p.display_name.is_some()),
        ]
        .into_iter()
        .find(|(_, set)| *set),
        Surface::Portal(_) => [
            ("params", p.params.is_some()),
            ("ttl_ms", p.ttl_ms.is_some()),
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
        V::WidgetParameterTypeMismatch { widget, param }
        | V::WidgetParameterInvalidValue { widget, param, .. } => tool_err(
            "WIDGET_PARAMETER_INVALID",
            format!("bad value for {widget}.{param}; see params in hud_surfaces"),
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

async fn publish_zone(ctx: &ToolCtx<'_>, zone: &str, p: PublishParams) -> McpResult<Value> {
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
    let ttl_ms = p.ttl_ms.unwrap_or(DEFAULT_ZONE_TTL_MS);
    let mut scene = ctx.scene.lock().await;
    let Some(def) = scene.zone_registry.zones.get(zone) else {
        return Err(tool_err(
            "ZONE_NOT_FOUND",
            format!("no zone {zone}; known: {}", zone_names(&scene)),
        ));
    };
    let content = parse_zone_content(&with_inferred_type(raw, &def.accepted_media_types))?;
    let lease_id = ensure_lease(ctx, &mut scene)?;
    let ttl_us = (ttl_ms > 0).then(|| ttl_ms.saturating_mul(1_000));
    if delay_ms > 0 {
        // Arrival is not presentation (invariant 1): hold until due.
        let present_at = now_us(&scene).saturating_add(delay_ms * 1_000);
        let batch = MutationBatch {
            batch_id: SceneId::new(),
            agent_namespace: ns,
            mutations: vec![SceneMutation::PublishToZone {
                zone_name: zone.to_string(),
                content,
                publish_token: ZonePublishToken { token: Vec::new() },
                merge_key: p.key,
                expires_at_wall_us: ttl_us.map(|t| present_at.saturating_add(t)),
                content_classification: None,
                breakpoints: Vec::new(),
                held: ttl_us.is_none(),
            }],
            timing_hints: None,
            lease_id: Some(lease_id),
        };
        scene.schedule_batch(present_at, batch);
        return Ok(ok_with_expiry((ttl_ms > 0).then_some(delay_ms + ttl_ms)));
    }
    match ttl_us {
        Some(_) => scene.publish_to_zone_with_lease(zone, content, &ns, lease_id, p.key, ttl_us),
        // Held until cleared: no urgency-derived expiry either.
        None => scene.publish_held_to_zone_with_lease(zone, content, &ns, lease_id, p.key),
    }
    .map_err(|e| scene_error(&e))?;
    Ok(ok_with_expiry((ttl_ms > 0).then_some(ttl_ms)))
}

async fn publish_widget(ctx: &ToolCtx<'_>, widget: &str, p: PublishParams) -> McpResult<Value> {
    let ns = ctx.agent.agent_id.clone();
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
    let lease_id = ensure_lease(ctx, &mut scene)?;
    let ttl_ms = p.ttl_ms.unwrap_or(0);
    let expires = (ttl_ms > 0).then(|| now_us(&scene).saturating_add(ttl_ms * 1_000));
    scene
        .publish_to_widget_for_lease(widget, typed, &ns, p.key, 0, expires, Some(lease_id))
        .map_err(|e| scene_error(&e))?;
    Ok(ok_with_expiry((ttl_ms > 0).then_some(ttl_ms)))
}

/// Whether this agent may hold portals and the portal driver is wired, so
/// discovery and input polling ask it.
fn portals_enabled(ctx: &ToolCtx<'_>) -> bool {
    ctx.portal_op_tx.is_some()
        && ctx
            .agent
            .allows(&Surface::Portal(String::new()).permission().0)
}

/// Send one operation to the portal driver and await its reply.
async fn portal_call<T>(
    ctx: &ToolCtx<'_>,
    build: impl FnOnce(tokio::sync::oneshot::Sender<T>) -> PortalOp,
) -> McpResult<T> {
    let unavailable = || {
        tool_err(
            "UNAVAILABLE",
            "the portal service isn't running; retry later",
        )
    };
    let tx = ctx.portal_op_tx.ok_or_else(unavailable)?;
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(build(reply)).map_err(|_| unavailable())?;
    ctx.portal_wake.notify();
    rx.await.map_err(|_| unavailable())
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
    portal_call(ctx, |reply| PortalOp::Publish {
        agent: ctx.agent.agent_id.clone(),
        portal: pid.to_string(),
        display_name: p.display_name,
        text,
        key: p.key,
        expects_reply: p.expects_reply.unwrap_or(false),
        status: p.status,
        reply,
    })
    .await?
    .map_err(|e| portal_err(e, &p.surface))?;
    Ok(json!({ "ok": true }))
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
    let result = ok_with_expiry((p.ttl_ms > 0).then_some(p.ttl_ms));
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
        }
        Surface::Portal(pid) => {
            // The runtime keeps a held portal (and its transcript) past the
            // idle reclaim until the hold lapses or hud_clear.
            portal_call(ctx, |reply| PortalOp::Hold {
                agent: ns,
                portal: pid,
                ttl_ms: p.ttl_ms,
                reply,
            })
            .await?
            .map_err(|e| portal_err(e, &p.surface))?;
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
    pub reason: Option<String>,
}

/// Release a zone publication, a widget publication, or a portal (detach).
pub async fn hud_clear(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: ClearParams = parse_args(args)?;
    let surface = Surface::parse(&p.surface)?;
    check_allowed(ctx, &surface)?;
    let ns = ctx.agent.agent_id.clone();
    match surface {
        Surface::Zone(zone) => {
            let mut scene = ctx.scene.lock().await;
            scene
                .clear_zone_and_cancel_pending(&zone, &ns)
                .map_err(|e| scene_error(&e))?;
        }
        Surface::Widget(widget) => {
            let mut scene = ctx.scene.lock().await;
            scene
                .clear_widget_for_publisher(&widget, &ns)
                .map_err(|e| scene_error(&e))?;
        }
        Surface::Portal(pid) => {
            portal_call(ctx, |reply| PortalOp::Clear {
                agent: ns,
                portal: pid,
                reply,
            })
            .await?
            .map_err(|e| portal_err(e, &p.surface))?;
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

/// Ack earlier items, then return input from every held surface, oldest
/// first. Unacked items are redelivered.
pub async fn hud_input(ctx: &ToolCtx<'_>, args: Value) -> McpResult<Value> {
    let p: InputParams = parse_args(args)?;
    let ns = ctx.agent.agent_id.clone();

    // Acks first, so a poll+ack is one round trip.
    ctx.state
        .with(&ns, |s| s.delivered.retain(|i| !p.ack.contains(&i.id)));
    let mut ack = p.ack;

    let wait_ms = p.wait_ms.unwrap_or(0).min(MAX_WAIT_MS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    let max_items = p.max_items.unwrap_or(usize::MAX);
    loop {
        let actions = ctx.scene.lock().await.take_pending_actions(&ns);
        let mut items: Vec<Value> = ctx.state.with(&ns, |s| {
            for a in actions {
                s.delivered.push(ActionItem {
                    id: format!("a{}", NEXT_ACTION_ID.fetch_add(1, Ordering::Relaxed)),
                    zone: a.zone_name,
                    action: a.callback_id,
                });
            }
            s.delivered
                .iter()
                .take(max_items)
                .map(|a| json!({ "id": a.id, "s": format!("zone:{}", a.zone), "action": a.action }))
                .collect()
        });
        let mut backlog = ctx.state.with(&ns, |s| s.delivered.len()) - items.len();
        if portals_enabled(ctx) {
            let batch = portal_call(ctx, |reply| PortalOp::Input {
                agent: ns.clone(),
                ack: std::mem::take(&mut ack),
                max_items: Some(max_items - items.len()),
                reply,
            })
            .await?;
            backlog += batch.remaining;
            items.extend(batch.items.into_iter().map(
                |i| json!({ "id": i.id, "s": format!("portal:{}", i.portal), "text": i.text }),
            ));
        }
        let now = tokio::time::Instant::now();
        if !items.is_empty() || backlog > 0 || now >= deadline {
            return Ok(json!({ "items": items, "remaining": backlog }));
        }
        tokio::time::sleep(
            (deadline - now).min(std::time::Duration::from_millis(POLL_INTERVAL_MS)),
        )
        .await;
    }
}

// ─── Content parsing ─────────────────────────────────────────────────────────

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
            match type_str {
                "stream_text" => {
                    let text = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
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
                    let urgency = obj.get("urgency").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
                    let ttl_ms = obj.get("ttl_ms").and_then(|v| v.as_u64());
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
                                let callback_id = o
                                    .get("callback_id")
                                    .and_then(|v| v.as_str())
                                    .filter(|v| !v.is_empty())
                                    .ok_or_else(|| {
                                        invalid(format!(
                                            "notification.actions[{index}].callback_id must be a non-empty string"
                                        ))
                                    })?
                                    .to_string();
                                Ok(NotificationAction { label, callback_id })
                            })
                            .collect::<Result<Vec<_>, McpError>>()?,
                        Some(_) => {
                            return Err(invalid(
                                "notification.actions must be an array".to_string(),
                            ))
                        }
                    };
                    Ok(ZoneContent::Notification(NotificationPayload {
                        text,
                        icon,
                        urgency,
                        ttl_ms,
                        title,
                        actions,
                    }))
                }
                "status_bar" => {
                    let entries: HashMap<String, String> = obj
                        .get("entries")
                        .and_then(|v| v.as_object())
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    if entries.is_empty() {
                        return Err(invalid(
                            "status_bar content must have a non-empty \"entries\" object"
                                .to_string(),
                        ));
                    }
                    Ok(ZoneContent::StatusBar(StatusBarPayload { entries }))
                }
                "solid_color" => {
                    let r = obj.get("r").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let g = obj.get("g").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                    let b = obj.get("b").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
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
    Ok(match decl.param_type {
        WidgetParamType::F32 => {
            WidgetParameterValue::F32(v.as_f64().ok_or_else(|| bad("a number"))? as f32)
        }
        WidgetParamType::String => {
            WidgetParameterValue::String(v.as_str().ok_or_else(|| bad("a string"))?.to_string())
        }
        WidgetParamType::Color => {
            let o = v
                .as_object()
                .ok_or_else(|| bad("a color {r,g,b,a} in 0..1"))?;
            let c = |k: &str, d: f64| o.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
            WidgetParameterValue::Color(Rgba {
                r: c("r", 0.0),
                g: c("g", 0.0),
                b: c("b", 0.0),
                a: c("a", 1.0),
            })
        }
        WidgetParamType::Enum => WidgetParameterValue::Enum(
            v.as_str()
                .ok_or_else(|| bad("one of its enum values"))?
                .to_string(),
        ),
    })
}
