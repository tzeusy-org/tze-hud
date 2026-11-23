//! MCP JSON-RPC server.
//!
//! Standard MCP over JSON-RPC 2.0: `initialize`, `notifications/initialized`,
//! `tools/list`, and `tools/call` for the five `hud_*` tools in
//! [`crate::tools`], plus the adapter's internal `notifications/tze/session_state`.
//!
//! ## Authentication
//!
//! Every request carries a PSK as the HTTP `Authorization: Bearer` value. The
//! PSK resolves to a paired agent (`[agents.<id>]` in `agents.toml`): the
//! agent id is the namespace, and its `allow` list is the whole permission
//! model. The directory is shared live with gRPC and read per request, so a
//! newly paired agent works on its next call. With no agents every request is
//! rejected. The PSK is never echoed.
//!
//! ## Sessions
//!
//! `initialize` opens a session scope ([`crate::scopes`]); the transport
//! returns its id as `Mcp-Session-Id` and sends it back on later calls, which
//! then own content, portals and input per session. An optional label rides
//! in `initialize` `params._meta["tze-hud/label"]`, outside model context.
//! Calls without an id are the agent's unscoped caller, as before sessions.
//!
//! ## Results
//!
//! A tool result is one text content block holding compact JSON. A failed
//! call is a tool result with `isError: true` whose text is
//! `{"code":"...","hint":"..."}` (see [`crate::error`]). Only unusable
//! requests (bad JSON, unknown method or tool, bad auth) are JSON-RPC errors.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as HubMutex};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, warn};
use tze_hud_portal::hub::{PortalError, PortalHub, PortalKey};
use tze_hud_scene::config::{AgentDirectory, SharedAgents};
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::render_wake::RenderWakeNotifier;
use tze_hud_scene::{Clock, SystemClock};

use crate::{
    error::{JsonRpcError, McpError},
    tools::{self, McpState, ToolCtx},
    types::{McpRequest, McpResponse},
};

// ─── Caller context ──────────────────────────────────────────────────────────

/// Authentication material for one request, from the transport.
#[derive(Clone, Debug, Default)]
pub struct CallerContext {
    /// PSK from the HTTP `Authorization: Bearer` header.
    pub bearer_token: Option<String>,
    /// The HTTP `Mcp-Session-Id` header: the session scope to act within.
    pub session_id: Option<String>,
}

impl CallerContext {
    /// An unauthenticated context.
    pub fn guest() -> Self {
        Self::default()
    }

    /// A context with a bearer token.
    pub fn with_bearer(token: impl Into<String>) -> Self {
        Self {
            bearer_token: Some(token.into()),
            session_id: None,
        }
    }

    /// The same caller acting within session `id`.
    pub fn in_session(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }
}

// ─── Server configuration ────────────────────────────────────────────────────

/// Server configuration: the trusted agents.
#[derive(Clone, Debug, Default)]
pub struct McpConfig {
    /// A caller's PSK resolves to an agent id (its namespace) and its
    /// allow-list permissions. With no agents, every call is rejected.
    pub agents: SharedAgents,
    /// Implicit widget transition duration supplied by the HUD's resolved
    /// startup profile. Standalone unconfigured servers retain instant updates.
    pub widget_transition_ms: u32,
    pub publication_defaults: tze_hud_scene::PublicationDefaults,
}

impl McpConfig {
    /// Accept the given dev PSK with unrestricted permissions (dev/test).
    pub fn with_psk(key: impl Into<String>) -> Self {
        Self::with_agents(AgentDirectory::unrestricted(key).shared())
    }

    /// Use the live agent directory shared with the runtime.
    pub fn with_agents(agents: SharedAgents) -> Self {
        Self {
            agents,
            widget_transition_ms: 0,
            publication_defaults: Default::default(),
        }
    }

    /// Load a dev PSK from `MCP_TEST_PSK` (test harnesses). Unset means
    /// every call is rejected.
    pub fn from_env() -> Self {
        match std::env::var("MCP_TEST_PSK") {
            Ok(k) => Self::with_psk(k),
            Err(_) => Self::default(),
        }
    }
}

/// One MCP request's transport-level outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionReply {
    /// The JSON-RPC response body (empty for a notification), and the id of
    /// the session an `initialize` just opened.
    Body {
        body: String,
        opened: Option<String>,
    },
    /// The request named a session that is not live (ended, released by the
    /// table bound, or lost to a restart); nothing was dispatched.
    UnknownSession,
}

/// What ending a session did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    Released,
    Unknown,
    Unauthenticated,
}

/// The `initialize` metadata key carrying the session label.
pub const LABEL_META_KEY: &str = "tze-hud/label";

/// A successful tool result: one text block of compact JSON.
fn tool_result(value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": value.to_string() }],
    })
}

/// A failed tool result: `isError` plus one text block of `{code, hint}`.
fn tool_error_result(err: &McpError) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": err.to_json().to_string() }],
        "isError": true,
    })
}

const TOOL_NAMES: &[&str] = &[
    "hud_surfaces",
    "hud_publish",
    "hud_hold",
    "hud_clear",
    "hud_input",
];

// ─── Server ──────────────────────────────────────────────────────────────────

/// Per-agent input readiness shared by MCP callers and the runtime's producers.
/// Notifications carry no input; each consumer must inspect its retained queues.
#[derive(Clone, Default)]
pub struct InputNotifiers {
    agents: Arc<HubMutex<HashMap<String, Arc<Notify>>>>,
    #[cfg(any(test, feature = "test-harness"))]
    wait_probes: Arc<HubMutex<InputWaitProbes>>,
}

#[cfg(any(test, feature = "test-harness"))]
#[derive(Default)]
struct InputWaitProbes {
    next_id: u64,
    pending: HashMap<(String, String), tokio::sync::watch::Sender<bool>>,
}

/// One authenticated request's current armed queue wait, available to fixtures.
/// Dropping the observer does not cancel the server's dispatch future.
#[cfg(any(test, feature = "test-harness"))]
pub struct InputWaitProbe {
    namespace: String,
    request_id: String,
    waiting: tokio::sync::watch::Receiver<bool>,
    registry: Arc<HubMutex<InputWaitProbes>>,
}

#[cfg(any(test, feature = "test-harness"))]
impl InputWaitProbe {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// True only while the matched dispatch is inside its armed wait.
    pub fn is_waiting(&self) -> bool {
        self.waiting.has_changed().is_ok() && *self.waiting.borrow()
    }

    /// False when dispatch finishes before reaching a currently armed wait.
    /// The caller must bound this observation with its existing test patience.
    pub async fn wait_until_waiting(&mut self) -> bool {
        loop {
            if self.is_waiting() {
                return true;
            }
            if self.waiting.changed().await.is_err() {
                return false;
            }
        }
    }
}

#[cfg(any(test, feature = "test-harness"))]
impl Drop for InputWaitProbe {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap()
            .pending
            .remove(&(self.namespace.clone(), self.request_id.clone()));
    }
}

#[cfg(any(test, feature = "test-harness"))]
pub(crate) struct InputWaitGuard<'a>(&'a tokio::sync::watch::Sender<bool>);

#[cfg(any(test, feature = "test-harness"))]
impl<'a> InputWaitGuard<'a> {
    pub(crate) fn arm(sender: &'a tokio::sync::watch::Sender<bool>) -> Self {
        sender.send_replace(true);
        Self(sender)
    }
}

#[cfg(any(test, feature = "test-harness"))]
impl Drop for InputWaitGuard<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

impl InputNotifiers {
    #[cfg(any(test, feature = "test-harness"))]
    fn observe_input_wait(&self, namespace: &str) -> InputWaitProbe {
        let mut registry = self.wait_probes.lock().unwrap();
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .expect("input probe id space");
        let request_id = format!("input-wait-probe-{}", registry.next_id);
        let (sender, waiting) = tokio::sync::watch::channel(false);
        let replaced = registry
            .pending
            .insert((namespace.to_owned(), request_id.clone()), sender);
        assert!(replaced.is_none());
        InputWaitProbe {
            namespace: namespace.to_owned(),
            request_id,
            waiting,
            registry: Arc::clone(&self.wait_probes),
        }
    }

    #[cfg(any(test, feature = "test-harness"))]
    fn claim_input_wait(
        &self,
        namespace: &str,
        id: Option<&serde_json::Value>,
    ) -> Option<tokio::sync::watch::Sender<bool>> {
        let id = id?.as_str()?;
        self.wait_probes
            .lock()
            .unwrap()
            .pending
            .remove(&(namespace.to_owned(), id.to_owned()))
    }

    pub fn for_agent(&self, namespace: &str) -> Result<Arc<Notify>, McpError> {
        let mut agents = self.agents.lock().map_err(|_| McpError::Tool {
            code: "INTERNAL",
            hint: "input readiness unavailable; retry hud_input".to_string(),
        })?;
        Ok(Arc::clone(agents.entry(namespace.to_string()).or_default()))
    }

    /// Wake every waiter of a released scope and drop its entry.
    fn release(&self, namespace: &str) {
        let removed = self
            .agents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(namespace);
        if let Some(ready) = removed {
            ready.notify_waiters();
        }
    }

    /// Wait-path acquisition must not block the request's absolute deadline.
    /// Producers retain for_agent's admission/routing behavior above.
    pub(crate) fn try_for_agent(&self, namespace: &str) -> Result<Arc<Notify>, McpError> {
        let mut agents = self.agents.try_lock().map_err(|_| McpError::Tool {
            code: "INTERNAL",
            hint: "input readiness unavailable; retry hud_input".to_string(),
        })?;
        Ok(Arc::clone(agents.entry(namespace.to_string()).or_default()))
    }
}

/// Admission result and queue counts captured together under the portal hub lock.
pub struct PortalSubmission {
    pub result: Result<String, PortalError>,
    pub pending_input_count: usize,
    pub pending_input_bytes: usize,
}

/// One hub, wall clock and per-agent input readiness shared by MCP and the driver.
#[derive(Clone)]
pub struct PortalHandle {
    pub hub: Arc<HubMutex<PortalHub>>,
    pub clock: Arc<dyn Clock>,
    input_notifiers: InputNotifiers,
}

impl PortalHandle {
    pub fn new(hub: PortalHub, clock: Arc<dyn Clock>) -> Self {
        Self {
            hub: Arc::new(HubMutex::new(hub)),
            clock,
            input_notifiers: InputNotifiers::default(),
        }
    }

    pub fn input_notify(&self, namespace: &str) -> Result<Arc<Notify>, McpError> {
        self.input_notifiers.for_agent(namespace)
    }

    /// Reserve a unique JSON-RPC id for an authenticated namespace's hud_input.
    #[cfg(any(test, feature = "test-harness"))]
    pub fn observe_input_wait(&self, namespace: &str) -> InputWaitProbe {
        self.input_notifiers.observe_input_wait(namespace)
    }

    /// Commit the same input/history admission used by the runtime composer.
    /// A refusal never signals accepted input; a poisoned owner refuses the turn.
    pub fn submit_reply(
        &self,
        key: &PortalKey,
        text: String,
        now_us: u64,
    ) -> Option<PortalSubmission> {
        let ready = match self.input_notify(&key.agent) {
            Ok(ready) => ready,
            Err(_) => {
                tracing::error!("input readiness poisoned; refusing portal submission");
                return None;
            }
        };
        let mut hub = match self.hub.lock() {
            Ok(hub) => hub,
            Err(_) => {
                tracing::error!("portal hub poisoned; refusing portal submission");
                return None;
            }
        };
        let result = hub.submit_reply(key, text, now_us);
        let (pending_input_count, pending_input_bytes) = hub
            .get(key)
            .map(|portal| (portal.inputs.len(), portal.input_bytes()))
            .unwrap_or_default();
        drop(hub);
        if result.is_ok() {
            ready.notify_waiters();
        }
        Some(PortalSubmission {
            result,
            pending_input_count,
            pending_input_bytes,
        })
    }
}

impl Default for PortalHandle {
    fn default() -> Self {
        Self::new(PortalHub::default(), Arc::new(SystemClock::new()))
    }
}

pub struct McpServer {
    scene: Arc<Mutex<SceneGraph>>,
    render_wake: RenderWakeNotifier,
    portal_ingress_wake: RenderWakeNotifier,
    config: McpConfig,
    /// Shared portal service. `None` means
    /// portal surfaces answer `UNAVAILABLE`.
    portals: Option<PortalHandle>,
    input_notifiers: InputNotifiers,
    /// Per-agent leases and unacked action presses.
    state: McpState,
    /// Live MCP session scopes.
    scopes: Arc<crate::scopes::Scopes>,
    /// Runtime safe-mode flag (shared with gRPC); false when standalone.
    safe_mode: Arc<AtomicBool>,
    /// The renderer's admission-time text measurement; `None` reports no
    /// `clipped` (no renderer, e.g. a bare test server).
    text_fit: Option<Arc<dyn tze_hud_scene::TextFit>>,
}

impl McpServer {
    /// A server over its own scene. Rejects every call until
    /// [`Self::with_config`] supplies PSKs.
    pub fn new(scene: SceneGraph) -> Self {
        Self::with_shared_scene(Arc::new(Mutex::new(scene)))
    }

    /// A server sharing a scene with the runtime and the gRPC plane.
    pub fn with_shared_scene(scene: Arc<Mutex<SceneGraph>>) -> Self {
        Self {
            scene,
            render_wake: RenderWakeNotifier::default(),
            portal_ingress_wake: RenderWakeNotifier::default(),
            config: McpConfig::default(),
            portals: None,
            input_notifiers: InputNotifiers::default(),
            state: McpState::default(),
            scopes: Default::default(),
            safe_mode: Arc::new(AtomicBool::new(false)),
            text_fit: None,
        }
    }

    /// Attach the agent directory.
    pub fn with_config(mut self, config: McpConfig) -> Self {
        self.config = config;
        self
    }

    /// Bind the runtime's render-work wake callback.
    pub fn with_render_wake_notifier(mut self, notifier: RenderWakeNotifier) -> Self {
        self.render_wake = notifier;
        self
    }

    /// Bind the main-thread portal ingress wake separately from render work.
    pub fn with_portal_ingress_wake_notifier(mut self, notifier: RenderWakeNotifier) -> Self {
        self.portal_ingress_wake = notifier;
        self
    }

    /// Share the runtime's portal hub; no main-thread round trip is needed.
    pub fn with_portals(mut self, portals: PortalHandle) -> Self {
        self.input_notifiers = portals.input_notifiers.clone();
        if let Some(scopes) = Arc::get_mut(&mut self.scopes) {
            scopes.set_clock(Arc::clone(&portals.clock));
        }
        self.portals = Some(portals);
        self
    }

    /// Share the runtime's safe-mode flag so mutating verbs honor it.
    pub fn with_safe_mode(mut self, flag: Arc<AtomicBool>) -> Self {
        self.safe_mode = flag;
        self
    }

    /// Measure published text with the renderer's shaper and tokens, so
    /// publish replies can report `clipped`.
    pub fn with_text_fit(mut self, fit: Arc<dyn tze_hud_scene::TextFit>) -> Self {
        self.text_fit = Some(fit);
        self
    }

    /// Borrow the shared scene handle.
    pub fn scene_handle(&self) -> Arc<Mutex<SceneGraph>> {
        Arc::clone(&self.scene)
    }

    /// Per-agent MCP state (for tests).
    pub fn state(&self) -> &McpState {
        &self.state
    }

    /// Build the per-call [`CallerContext`] from a transport bearer token.
    pub fn caller_context(&self, bearer_token: Option<String>) -> CallerContext {
        match bearer_token {
            Some(token) => CallerContext::with_bearer(token),
            None => CallerContext::guest(),
        }
    }

    /// Live session scopes (for `/admin/status` and tests).
    pub fn scopes(&self) -> &Arc<crate::scopes::Scopes> {
        &self.scopes
    }

    fn identity(&self, ctx: &CallerContext) -> Option<tze_hud_scene::config::AgentIdentity> {
        ctx.bearer_token
            .as_deref()
            .and_then(|key| self.config.agents.load().resolve(key, "").ok())
    }

    /// End session `ctx.session_id` (HTTP `DELETE`): an explicit release.
    pub async fn end_session(&self, ctx: &CallerContext) -> SessionEnd {
        let Some(identity) = self.identity(ctx) else {
            return SessionEnd::Unauthenticated;
        };
        let closed = ctx
            .session_id
            .as_deref()
            .and_then(|id| self.scopes.close(&identity.agent_id, id));
        match closed {
            Some(scope) => {
                self.release_scope(&scope, "ended").await;
                SessionEnd::Released
            }
            None => SessionEnd::Unknown,
        }
    }

    /// Clear everything a closed scope owns: zone and widget content,
    /// delayed publishes, portals and un-ACKed input; end its armed waits.
    async fn release_scope(&self, scope: &crate::scopes::Scope, reason: &'static str) {
        let ns = scope.namespace.as_str();
        let (cleared, wakes) = {
            let mut scene = self.scene.lock().await;
            let cleared = scene.release_scope(ns).cleared;
            (cleared, scene.take_notification_wait_notifiers())
        };
        for wake in wakes {
            wake.notify();
        }
        let portals = match &self.portals {
            Some(portals) => {
                let mut hub = portals.hub.lock().unwrap_or_else(|p| p.into_inner());
                let ids: Vec<String> = hub.list(ns).into_iter().map(|p| p.id).collect();
                for id in &ids {
                    let _ = hub.clear(&PortalKey::new(ns, id.as_str()));
                }
                ids.len()
            }
            None => 0,
        };
        self.state.forget(ns);
        self.input_notifiers.release(ns);
        if portals > 0 {
            self.portal_ingress_wake.notify();
        }
        self.render_wake.notify();
        tracing::info!(
            agent = %scope.agent,
            label = scope.label().as_deref().unwrap_or(""),
            reason,
            zones = cleared.zones,
            widgets = cleared.widgets,
            portals,
            "MCP session ended"
        );
    }

    /// Open a session for `agent`, releasing one first at the table bound.
    async fn open_scope(&self, agent: &str, label: Option<&str>) -> String {
        let mut scene = self.scene.lock().await;
        let mut held = scene.holding_namespaces();
        if let Some(portals) = &self.portals {
            let hub = portals.hub.lock().unwrap_or_else(|p| p.into_inner());
            hub.visit_holdings(|key, _, _| {
                held.insert(key.agent.clone());
            });
        }
        let (scope, evicted) = self
            .scopes
            .open(agent, label, |s| !held.contains(&s.namespace));
        // Its cards carry its label as a caption.
        scene.set_scoped_session_label(&scope.namespace, scope.session_label());
        drop(scene);
        if let Some(evicted) = evicted {
            self.release_scope(&evicted, "session limit").await;
        }
        tracing::info!(
            agent,
            label = scope.label().as_deref().unwrap_or(""),
            "MCP session started"
        );
        scope.id.clone()
    }

    fn tool_context<'a>(
        &'a self,
        identity: &'a tze_hud_scene::config::AgentIdentity,
        scope: Option<&'a crate::scopes::Scope>,
        #[cfg(any(test, feature = "test-harness"))] input_wait_probe: Option<
            &'a tokio::sync::watch::Sender<bool>,
        >,
    ) -> ToolCtx<'a> {
        ToolCtx {
            scene: &self.scene,
            portals: self.portals.as_ref(),
            input_notifiers: &self.input_notifiers,
            #[cfg(any(test, feature = "test-harness"))]
            input_wait_probe,
            portal_wake: &self.portal_ingress_wake,
            render_wake: &self.render_wake,
            state: &self.state,
            safe_mode: &self.safe_mode,
            agent: identity,
            owner: scope.map_or(identity.agent_id.as_str(), |s| s.namespace.as_str()),
            scope,
            scopes: &self.scopes,
            widget_transition_ms: self.config.widget_transition_ms,
            publication_defaults: &self.config.publication_defaults,
            text_fit: self.text_fit.as_deref(),
        }
    }

    /// Dispatch one JSON-RPC 2.0 request body. Returns the response body, or
    /// an empty string for a notification or an unknown session.
    pub async fn dispatch(&self, body: &str, ctx: &CallerContext) -> String {
        match self.dispatch_session(body, ctx).await {
            SessionReply::Body { body, .. } => body,
            SessionReply::UnknownSession => String::new(),
        }
    }

    /// Dispatch one request within its session. A named session that is not
    /// live is refused before anything runs.
    pub async fn dispatch_session(&self, body: &str, ctx: &CallerContext) -> SessionReply {
        let identity = self.identity(ctx);
        let scope = match (&identity, ctx.session_id.as_deref()) {
            (Some(identity), Some(id)) => match self.scopes.touch(&identity.agent_id, id) {
                Some(scope) => Some(scope),
                None => return SessionReply::UnknownSession,
            },
            _ => None,
        };
        let mut opened = None;
        let body = self
            .dispatch_inner(body, identity, scope.as_deref(), &mut opened)
            .await;
        SessionReply::Body { body, opened }
    }

    async fn dispatch_inner(
        &self,
        body: &str,
        identity: Option<tze_hud_scene::config::AgentIdentity>,
        scope: Option<&crate::scopes::Scope>,
        opened: &mut Option<String>,
    ) -> String {
        let respond = |resp: McpResponse| serde_json::to_string(&resp).unwrap_or_default();
        let request: McpRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "MCP: failed to parse JSON-RPC request");
                return respond(McpResponse::err(None, JsonRpcError::parse_error()));
            }
        };
        let id = request.id.clone();
        if request.jsonrpc != "2.0" {
            return respond(McpResponse::err(id, JsonRpcError::invalid_request()));
        }

        let Some(identity) = identity else {
            warn!(method = %request.method, "MCP: authentication failed");
            return respond(McpResponse::err(id, JsonRpcError::unauthenticated()));
        };

        match request.method.as_str() {
            "initialize" => {
                // A session's own re-initialize keeps its scope.
                if scope.is_none() {
                    let label = request
                        .params
                        .pointer("/_meta")
                        .and_then(|meta| meta.get(LABEL_META_KEY))
                        .and_then(serde_json::Value::as_str);
                    *opened = Some(self.open_scope(&identity.agent_id, label).await);
                }
                respond(McpResponse::ok(id, crate::schema::initialize_result()))
            }
            "notifications/tze/session_state" => {
                if id.is_none()
                    && let Some(scope) = scope
                {
                    self.scopes.report_presence(scope, &request.params);
                }
                String::new()
            }
            // Notifications get no response.
            m if m.starts_with("notifications/") => String::new(),
            "ping" => respond(McpResponse::ok(id, serde_json::json!({}))),
            "tools/list" => respond(McpResponse::ok(id, crate::schema::tools_list_result())),
            "tools/call" => {
                let params = request.params.as_object();
                let Some(name) = params.and_then(|p| p.get("name")).and_then(|n| n.as_str()) else {
                    return respond(McpResponse::err(
                        id,
                        JsonRpcError::invalid_params("tools/call requires a string `name`"),
                    ));
                };
                if !TOOL_NAMES.contains(&name) {
                    return respond(McpResponse::err(
                        id,
                        JsonRpcError::invalid_params(format!(
                            "unknown tool {name}; see tools/list"
                        )),
                    ));
                }
                let args = params
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .filter(|a| !a.is_null())
                    .unwrap_or_else(|| serde_json::json!({}));
                debug!(tool = name, agent = %identity.agent_id, "MCP: tools/call");
                if name == "hud_publish"
                    && args.get("surface").and_then(serde_json::Value::as_str)
                        == Some("zone:subtitle")
                    && args
                        .get("delay_ms")
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|ms| ms > 0)
                    && tracing::enabled!(target: "tze_hud_delay_diagnostic", tracing::Level::DEBUG)
                {
                    let span = tracing::Span::current();
                    if span.metadata().is_some_and(|m| {
                        m.name() == "mcp_request" && m.target() == "tze_hud_mcp_phases"
                    }) && let (Some(request_id), Some(span_id)) =
                        (id.as_ref().and_then(serde_json::Value::as_u64), span.id())
                    {
                        debug!(target: "tze_hud_delay_diagnostic",
                            event = "delayed_request", request_id,
                            request_span_id = span_id.into_u64());
                    }
                }
                #[cfg(any(test, feature = "test-harness"))]
                let input_wait_probe = if matches!(name, "hud_input" | "hud_publish") {
                    self.input_notifiers.claim_input_wait(
                        scope.map_or(identity.agent_id.as_str(), |s| s.namespace.as_str()),
                        id.as_ref(),
                    )
                } else {
                    None
                };
                let tool_ctx = self.tool_context(
                    &identity,
                    scope,
                    #[cfg(any(test, feature = "test-harness"))]
                    input_wait_probe.as_ref(),
                );
                let args_zone_target = args
                    .get("surface")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| s.starts_with("zone:"));
                let portal_target = args
                    .get("surface")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|surface| tools::Surface::parse(surface).ok())
                    .is_some_and(|surface| matches!(surface, tools::Surface::Portal(_)));
                let tool_started =
                    tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG)
                        .then(std::time::Instant::now);
                if tool_started.is_some() {
                    // `name` has passed the five-name allowlist above.
                    debug!(target: "tze_hud_mcp_phases", phase = "tool_begin", tool = name);
                }
                let result = match name {
                    "hud_surfaces" => tools::hud_surfaces(&tool_ctx)
                        .await
                        .map(crate::publish_wait::PublishReply::plain),
                    "hud_publish" => tools::hud_publish(&tool_ctx, args).await,
                    "hud_hold" => tools::hud_hold(&tool_ctx, args)
                        .await
                        .map(crate::publish_wait::PublishReply::plain),
                    "hud_clear" => tools::hud_clear(&tool_ctx, args)
                        .await
                        .map(crate::publish_wait::PublishReply::plain),
                    _ => tools::hud_input(&tool_ctx, args)
                        .await
                        .map(crate::publish_wait::PublishReply::plain),
                };
                if let Some(started) = &tool_started {
                    debug!(
                        target: "tze_hud_mcp_phases",
                        phase = "tool_return",
                        tool = name,
                        code = match &result {
                            Ok(_) => "OK",
                            Err(error) => error.code(),
                        },
                        elapsed_us = started.elapsed().as_micros()
                    );
                }
                if matches!(name, "hud_clear" | "hud_hold") {
                    let wakes = self.scene.lock().await.take_notification_wait_notifiers();
                    for wake in wakes {
                        wake.notify();
                    }
                }
                let body = match &result {
                    Ok(value) => {
                        if !portal_target
                            && (matches!(name, "hud_hold" | "hud_clear")
                                || (name == "hud_publish" && !args_zone_target))
                        {
                            self.render_wake.notify();
                        }
                        tool_result(&value.value)
                    }
                    Err(e) => {
                        debug!(tool = name, error = %e, "MCP: tool error");
                        tool_error_result(e)
                    }
                };
                let encoded = respond(McpResponse::ok(id, body));
                if !encoded.is_empty()
                    && let Ok(reply) = result
                {
                    reply.encoded();
                }
                encoded
            }
            other => respond(McpResponse::err(id, JsonRpcError::method_not_found(other))),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
