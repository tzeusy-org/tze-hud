//! MCP JSON-RPC server.
//!
//! Standard MCP over JSON-RPC 2.0: `initialize`, `notifications/initialized`,
//! `tools/list`, and `tools/call` for the five `hud_*` tools in
//! [`crate::tools`]. There is no other method.
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
//! ## Results
//!
//! A tool result is one text content block holding compact JSON. A failed
//! call is a tool result with `isError: true` whose text is
//! `{"code":"...","hint":"..."}` (see [`crate::error`]). Only unusable
//! requests (bad JSON, unknown method or tool, bad auth) are JSON-RPC errors.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as HubMutex};
use tokio::sync::Mutex;
use tracing::{debug, warn};
use tze_hud_projection::hub::PortalHub;
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
        }
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

/// The single portal state and wall clock shared by MCP and the winit driver.
#[derive(Clone)]
pub struct PortalHandle {
    pub hub: Arc<HubMutex<PortalHub>>,
    pub clock: Arc<dyn Clock>,
}

impl PortalHandle {
    pub fn new(hub: PortalHub, clock: Arc<dyn Clock>) -> Self {
        Self {
            hub: Arc::new(HubMutex::new(hub)),
            clock,
        }
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
    /// Per-agent leases and unacked action presses.
    state: McpState,
    /// Runtime safe-mode flag (shared with gRPC); false when standalone.
    safe_mode: Arc<AtomicBool>,
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
            state: McpState::default(),
            safe_mode: Arc::new(AtomicBool::new(false)),
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
        self.portals = Some(portals);
        self
    }

    /// Share the runtime's safe-mode flag so mutating verbs honor it.
    pub fn with_safe_mode(mut self, flag: Arc<AtomicBool>) -> Self {
        self.safe_mode = flag;
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

    /// Dispatch one JSON-RPC 2.0 request body. Returns the response body, or
    /// an empty string for a notification.
    pub async fn dispatch(&self, body: &str, ctx: &CallerContext) -> String {
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

        let identity = ctx
            .bearer_token
            .as_deref()
            .and_then(|key| self.config.agents.load().resolve(key, "").ok());
        let Some(identity) = identity else {
            warn!(method = %request.method, "MCP: authentication failed");
            return respond(McpResponse::err(id, JsonRpcError::unauthenticated()));
        };

        match request.method.as_str() {
            "initialize" => respond(McpResponse::ok(id, crate::schema::initialize_result())),
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
                let tool_ctx = ToolCtx {
                    scene: &self.scene,
                    portals: self.portals.as_ref(),
                    portal_wake: &self.portal_ingress_wake,
                    state: &self.state,
                    safe_mode: &self.safe_mode,
                    agent: &identity,
                    widget_transition_ms: self.config.widget_transition_ms,
                };
                let portal_target = args
                    .get("surface")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|surface| tools::Surface::parse(surface).ok())
                    .is_some_and(|surface| matches!(surface, tools::Surface::Portal(_)));
                let result = match name {
                    "hud_surfaces" => tools::hud_surfaces(&tool_ctx).await,
                    "hud_publish" => tools::hud_publish(&tool_ctx, args).await,
                    "hud_hold" => tools::hud_hold(&tool_ctx, args).await,
                    "hud_clear" => tools::hud_clear(&tool_ctx, args).await,
                    _ => tools::hud_input(&tool_ctx, args).await,
                };
                let body = match &result {
                    Ok(value) => {
                        if !portal_target
                            && matches!(name, "hud_publish" | "hud_hold" | "hud_clear")
                        {
                            self.render_wake.notify();
                        }
                        tool_result(value)
                    }
                    Err(e) => {
                        debug!(tool = name, error = %e, "MCP: tool error");
                        tool_error_result(e)
                    }
                };
                respond(McpResponse::ok(id, body))
            }
            other => respond(McpResponse::err(id, JsonRpcError::method_not_found(other))),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
