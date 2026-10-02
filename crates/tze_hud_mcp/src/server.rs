//! MCP JSON-RPC server.
//!
//! The server wraps the shared [`SceneGraph`] and dispatches incoming
//! JSON-RPC 2.0 requests to the appropriate tool handler.
//!
//! ## Authentication
//!
//! Authentication is **always enforced** — there is no bypass mode.  Every
//! call must carry a valid pre-shared key (PSK), even for guest tools.  The
//! PSK may be supplied as:
//!
//! 1. A JSON-RPC `params` top-level field `"_auth"` (string): preferred for
//!    stdio/NDJSON transports where custom headers are unavailable.
//! 2. An HTTP `Authorization: Bearer <key>` header value passed by the caller
//!    via [`CallerContext::with_bearer`].
//!
//! When no PSK is configured (`McpConfig::pre_shared_key` is `None`), every
//! call is rejected.  This is intentional — sovereignty must be enforced by
//! mechanism, not convention (heart-and-soul/security.md).
//!
//! Each call is authenticated independently — there is no persistent session
//! state.  The PSK is never echoed in responses.
//!
//! ## Guest vs Resident tools
//!
//! **Guest tools** (do not require `resident_mcp`; individual tools may enforce
//! additional capability checks):
//! - `publish_to_zone`
//! - `list_zones`
//! - `list_scene`
//! - `list_elements`
//! - `publish_to_widget`
//! - `list_widgets`
//! - `clear_widget`
//! - `register_widget_asset`
//! - `publish_to_element`
//!
//! **Resident tools** (require the `resident_mcp` capability):
//! - `create_tab`
//! - `create_tile`
//! - `set_content`
//! - `dismiss`
//! - `portal_projection_*`
//!
//! Calling a resident tool without the capability returns a structured
//! JSON-RPC 2.0 error: code -32603, `data.error_code="CAPABILITY_REQUIRED"`.
//!
//! ## Transport
//!
//! The server is transport-agnostic at this layer: [`McpServer::dispatch`]
//! accepts a raw `&str` (the JSON-RPC request body) plus a [`CallerContext`]
//! and returns a `String` (the JSON-RPC response body). Callers can wire this
//! to HTTP, stdio, or any other transport.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, warn};
use tze_hud_scene::config::AgentDirectory;
use tze_hud_scene::graph::SceneGraph;

use crate::{
    error::{JsonRpcError, McpError},
    tools,
    types::{McpRequest, McpResponse},
};

// ─── Caller context ──────────────────────────────────────────────────────────

/// Authentication and capability context for a single MCP call.
///
/// This is constructed by the transport layer and passed to
/// [`McpServer::dispatch`].  It carries:
/// - The pre-shared key (if available from the transport).
/// - The set of capabilities granted to this caller (e.g. `resident_mcp`).
///
/// Per spec §8.4: each call is authenticated independently — no persistent
/// session state is maintained at this layer.
#[derive(Clone, Debug, Default)]
pub struct CallerContext {
    /// Pre-shared key extracted from the transport (e.g. Bearer token from an
    /// HTTP header).  If `None`, only in-params `_auth` is attempted.
    pub bearer_token: Option<String>,
}

impl CallerContext {
    /// Create an unauthenticated context.
    pub fn guest() -> Self {
        Self::default()
    }

    /// Create a context with a bearer token (from HTTP `Authorization` header).
    pub fn with_bearer(token: impl Into<String>) -> Self {
        Self {
            bearer_token: Some(token.into()),
        }
    }
}

// ─── Server configuration ────────────────────────────────────────────────────

/// Server-level configuration.
///
/// Authentication is **always enforced** for every well-formed JSON-RPC 2.0
/// tool call dispatched via [`McpServer::dispatch`].  When `pre_shared_key`
/// is `None`, every such call is rejected with an `Unauthenticated` JSON-RPC
/// error.  There is no bypass mode.  Note that requests that fail JSON
/// parsing or JSON-RPC version validation are rejected before auth is
/// evaluated (returning `Parse error` or `Invalid Request` respectively).
///
/// Production deployments must supply a PSK via [`McpConfig::with_psk`];
/// test harnesses should use [`McpConfig::from_env`] or supply an explicit
/// test key.
///
/// Per spec §8.4, an MCP runtime is expected to evaluate authentication
/// during session establishment and, on failure, send `SessionError` and
/// close the stream.  This crate does **not** implement the session handshake
/// or manage stream lifecycles; it only enforces the configured policy on
/// each JSON-RPC request dispatched via [`McpServer::dispatch`].  Any
/// handshake-time authentication and mapping of failures to `SessionError` /
/// stream closure must be implemented by the higher-level transport/runtime
/// that integrates this server.
#[derive(Clone, Debug, Default)]
pub struct McpConfig {
    /// The trusted agents. A caller's PSK resolves to an agent id (its
    /// namespace) and its allow-list permissions. With no runtime PSK and no
    /// agent PSKs, every call is rejected (no bypass).
    pub agents: AgentDirectory,
}

impl McpConfig {
    /// Accept the given runtime PSK with unrestricted permissions (dev/test).
    pub fn with_psk(key: impl Into<String>) -> Self {
        Self {
            agents: AgentDirectory::unrestricted(key),
        }
    }

    /// Use a configured agent directory.
    pub fn with_agents(agents: AgentDirectory) -> Self {
        Self { agents }
    }

    /// Load the runtime PSK from the `MCP_TEST_PSK` environment variable.
    ///
    /// Intended for test harnesses. If the variable is unset, every call is
    /// rejected.
    pub fn from_env() -> Self {
        match std::env::var("MCP_TEST_PSK") {
            Ok(k) => Self::with_psk(k),
            Err(_) => Self::default(),
        }
    }

    fn has_credentials(&self) -> bool {
        !self.agents.runtime_psk.is_empty() || !self.agents.agent_psks.is_empty()
    }
}

// ─── Guest / Resident tool classification ────────────────────────────────────

/// Wrap a tool's successful raw JSON result in the MCP `tools/call` result shape
/// (hud-09emd): a single text `content` block carrying the JSON serialization
/// (so text-only clients get the full result) with `isError: false`. The bare
/// tool JSON stays available verbatim to legacy method==tool-name callers.
fn tools_call_success_result(value: serde_json::Value) -> serde_json::Value {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

/// Wrap a tool EXECUTION error in the MCP `tools/call` `isError` result shape
/// (hud-09emd): a text `content` block with the error message and
/// `isError: true`, so the calling model sees and can react to the failure —
/// rather than a JSON-RPC protocol error, which MCP reserves for unknown
/// tool / missing capability / malformed params.
fn tools_call_error_result(err: &crate::McpError) -> serde_json::Value {
    if let McpError::ProjectionRejected {
        error_code,
        operation,
    } = err
    {
        let wire = JsonRpcError::projection_rejected(*error_code, operation);
        let structured = wire.data.unwrap_or_else(|| {
            serde_json::json!({
                "error_code": error_code.as_str(),
                "message": wire.message,
            })
        });
        let text = serde_json::to_string(&structured).unwrap_or_else(|_| "{}".to_string());
        return serde_json::json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
            "isError": true,
        });
    }
    serde_json::json!({
        "content": [{ "type": "text", "text": err.to_string() }],
        "isError": true,
    })
}

/// Tools whose params carry a server-set `namespace` (the caller's agent id).
const NAMESPACED_TOOLS: &[&str] = &[
    "publish_to_zone",
    "publish_to_widget",
    "clear_widget",
    "publish_to_element",
    "create_tile",
];

fn is_known_tool(method: &str) -> bool {
    matches!(
        method,
        "publish_to_zone"
            | "list_zones"
            | "list_scene"
            | "list_elements"
            | "publish_to_widget"
            | "list_widgets"
            | "clear_widget"
            | "register_widget_asset"
            | "publish_to_element"
            | "create_tab"
            | "create_tile"
            | "set_content"
            | "dismiss"
            | "inject_composer_paste"
    ) || method.starts_with("portal_projection_")
        && matches!(
            method,
            "portal_projection_list"
                | "portal_projection_attach"
                | "portal_projection_publish"
                | "portal_projection_publish_status"
                | "portal_projection_get_pending_input"
                | "portal_projection_acknowledge_input"
                | "portal_projection_detach"
                | "portal_projection_cleanup"
        )
}

/// The permission a call needs, and the allow entry that grants it.
/// `None` means any authenticated agent may call it.
fn required_permission(method: &str, params: &serde_json::Value) -> Option<(String, String)> {
    let str_param = |k: &str| {
        params
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match method {
        "publish_to_zone" => {
            let zone = str_param("zone_name");
            Some((format!("publish_zone:{zone}"), format!("zone:{zone}")))
        }
        "publish_to_widget" | "clear_widget" => {
            let widget = str_param("widget_name");
            Some((
                format!("publish_widget:{widget}"),
                format!("widget:{widget}"),
            ))
        }
        "register_widget_asset" => Some(("register_widget_asset".into(), "widget:*".into())),
        "create_tab" | "create_tile" | "set_content" | "dismiss" => {
            Some(("create_tiles".into(), "tiles".into()))
        }
        m if m == "inject_composer_paste" || m.starts_with("portal_projection_") => {
            Some(("resident_mcp".into(), "portal".into()))
        }
        _ => None,
    }
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// Shared server state — wraps the scene graph behind an async mutex so that
/// concurrent requests serialize scene mutations safely.
pub struct McpServer {
    scene: Arc<Mutex<SceneGraph>>,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    widget_asset_registry: Arc<Mutex<tools::WidgetAssetRegistry>>,
    config: McpConfig,
    paste_inject_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Channel to the in-process portal projection authority (hud-bq0gl.2).
    ///
    /// When `Some`, `portal_projection_*` tools are reachable — they forward
    /// operations through this channel to the winit event-loop thread which owns
    /// the `InProcessPortalDriver`.  When `None`, those tools return an
    /// `Internal` error (authority not wired).
    portal_op_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>>,
    /// Portal owner tokens bound to (agent id, projection id). The model never
    /// sees them: the server injects them into portal calls by identity.
    portal_owner_tokens: Arc<std::sync::Mutex<HashMap<(String, String), String>>>,
}

impl McpServer {
    /// Create a new server backed by the given scene graph.
    ///
    /// The server is created with no PSK configured.  **All calls will be
    /// rejected** until a PSK is attached via [`.with_config`].  This is
    /// intentional: authentication is mandatory with no bypass mode.
    ///
    /// For tests, use `McpServer::new(scene).with_config(McpConfig::with_psk("test-key"))`.
    pub fn new(scene: SceneGraph) -> Self {
        Self {
            scene: Arc::new(Mutex::new(scene)),
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            widget_asset_registry: Arc::new(Mutex::new(tools::WidgetAssetRegistry::default())),
            config: McpConfig::default(),
            paste_inject_tx: None,
            portal_op_tx: None,
            portal_owner_tokens: Arc::default(),
        }
    }

    /// Create a new server sharing an existing arc-wrapped scene graph.
    ///
    /// Use this when the scene is also shared with the gRPC control plane.
    /// As with [`Self::new`], all calls are rejected until a PSK is configured
    /// via [`.with_config`].
    pub fn with_shared_scene(scene: Arc<Mutex<SceneGraph>>) -> Self {
        Self {
            scene,
            render_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            widget_asset_registry: Arc::new(Mutex::new(tools::WidgetAssetRegistry::default())),
            config: McpConfig::default(),
            paste_inject_tx: None,
            portal_op_tx: None,
            portal_owner_tokens: Arc::default(),
        }
    }

    /// Attach server configuration (auth settings, etc.).
    pub fn with_config(mut self, config: McpConfig) -> Self {
        self.config = config;
        self
    }

    /// Bind the runtime's platform-neutral render-work wake callback.
    pub fn with_render_wake_notifier(
        mut self,
        notifier: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> Self {
        self.render_wake = notifier;
        self
    }

    /// Bind the main-thread portal ingress wake separately from render work.
    pub fn with_portal_ingress_wake_notifier(
        mut self,
        notifier: tze_hud_scene::render_wake::RenderWakeNotifier,
    ) -> Self {
        self.portal_ingress_wake = notifier;
        self
    }

    /// Attach a paste-inject channel sender for `inject_composer_paste` tool.
    pub fn with_paste_inject_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        self.paste_inject_tx = Some(tx);
        self
    }

    /// Attach the portal-operation channel sender (hud-bq0gl.2).
    ///
    /// When set, `portal_projection_*` MCP tools forward operations to the
    /// in-process `InProcessPortalDriver` on the winit event-loop thread via
    /// this channel.  Without this, those tools return an `Internal` error.
    pub fn with_portal_op_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::portal_op::PortalOp>,
    ) -> Self {
        self.portal_op_tx = Some(tx);
        self
    }

    /// Borrow the underlying shared scene handle.
    pub fn scene_handle(&self) -> Arc<Mutex<SceneGraph>> {
        Arc::clone(&self.scene)
    }

    /// Build the per-call [`CallerContext`] from a transport-supplied bearer
    /// token. Identity and permissions are resolved in [`Self::dispatch`].
    pub fn caller_context(&self, bearer_token: Option<String>) -> CallerContext {
        match bearer_token {
            Some(token) => CallerContext::with_bearer(token),
            None => CallerContext::guest(),
        }
    }

    /// Dispatch a raw JSON-RPC 2.0 request body and return the response body.
    ///
    /// `ctx` carries authentication material and capabilities for this call.
    /// Per spec §8.4 each call is authenticated independently.
    ///
    /// This is the single entry point for all transports.
    pub async fn dispatch(&self, body: &str, ctx: &CallerContext) -> String {
        // Parse the request
        let mut request: McpRequest = match serde_json::from_str(body) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "MCP: failed to parse JSON-RPC request");
                let resp = McpResponse::err(None, JsonRpcError::parse_error());
                return serde_json::to_string(&resp).unwrap_or_default();
            }
        };

        // Validate JSON-RPC version
        if request.jsonrpc != "2.0" {
            let resp = McpResponse::err(request.id.clone(), JsonRpcError::invalid_request());
            return serde_json::to_string(&resp).unwrap_or_default();
        }

        // ── Per-call authentication (spec §8.4) ──────────────────────────────
        //
        // Authentication is ALWAYS evaluated — there is no bypass mode.
        // When no PSK is configured (`pre_shared_key` is `None`), all calls
        // are rejected.  This enforces the spec requirement: "sovereignty must
        // be enforced by mechanism, not convention" (heart-and-soul/security.md).
        //
        // Attempt auth from two independent sources (spec §8.4: either is valid):
        // 1. CallerContext bearer token (from HTTP Authorization header).
        // 2. `_auth` param field in the JSON-RPC params object.
        //
        // Both are checked independently — if a bearer token is present but
        // wrong, the `_auth` param can still authenticate the call.  This
        // prevents a rogue/stale transport header from blocking valid in-params auth.
        //
        // Constant-time comparison (via `subtle`) prevents timing side-channels.
        // When no PSK is configured, reject immediately with a single warning
        // (avoids emitting two warn entries for the same event).
        if !self.config.has_credentials() {
            warn!(method = %request.method, "MCP: authentication rejected (no PSK configured)");
            let resp = McpResponse::err(
                request.id.clone(),
                JsonRpcError::from(McpError::Unauthenticated),
            );
            return serde_json::to_string(&resp).unwrap_or_default();
        }

        let param_key = request
            .params
            .as_object()
            .and_then(|o| o.get("_auth"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let identity = [ctx.bearer_token.clone(), param_key]
            .into_iter()
            .flatten()
            .find_map(|key| self.config.agents.resolve(&key, "").ok());
        let Some(identity) = identity else {
            warn!(method = %request.method, "MCP: authentication failed");
            let resp = McpResponse::err(
                request.id.clone(),
                JsonRpcError::from(McpError::Unauthenticated),
            );
            return serde_json::to_string(&resp).unwrap_or_default();
        };
        // Strip _auth so handlers never see it (typed params reject unknown fields).
        if let Some(obj) = request.params.as_object_mut() {
            obj.remove("_auth");
        }

        // ── MCP lifecycle + introspection methods ────────────────────────────
        //
        // `initialize` (handshake) and `tools/list` (schema discovery) are MCP
        // protocol methods, not scene tools. They require valid PSK auth (above)
        // but carry no per-tool capability, so any authenticated caller can
        // negotiate the protocol and introspect the tool surface. Handle them
        // before the capability gate and tool router: their `name/` style method
        // names are not registered tools and would otherwise fall through to
        // MethodNotFound.
        // When true, the request arrived via the MCP-standard `tools/call`
        // envelope and its response MUST be spec-shaped (a `content`/`isError`
        // result), not the bare tool JSON the legacy method==tool-name path
        // returns. Set by the `tools/call` arm below, which rewrites the request
        // to the bare form so the SAME classify → capability-gate → invoke_tool
        // pipeline runs unchanged (hud-09emd).
        let mut is_tools_call = false;

        match request.method.as_str() {
            "initialize" => {
                let resp = McpResponse::ok(request.id.clone(), crate::schema::initialize_result());
                return serde_json::to_string(&resp).unwrap_or_default();
            }
            "tools/list" => {
                let resp = McpResponse::ok(request.id.clone(), crate::schema::tools_list_result());
                return serde_json::to_string(&resp).unwrap_or_default();
            }
            "tools/call" => {
                // Spec-standard tool invocation:
                //   params = { "name": "<tool>", "arguments": { ... } }
                // Delegate to the exact same dispatch table the bare-method path
                // uses by unwrapping the envelope into the bare form (method =
                // tool name, params = arguments) and flagging the response for
                // spec-shaping below. No forked tool registry — `invoke_tool`
                // stays the single source of truth (hud-09emd).
                let params_obj = request.params.as_object();
                let Some(name) = params_obj
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
                else {
                    let resp = McpResponse::err(
                        request.id.clone(),
                        JsonRpcError::invalid_params(
                            "tools/call requires a string `name` parameter",
                        ),
                    );
                    return serde_json::to_string(&resp).unwrap_or_default();
                };
                // `arguments` is optional per MCP (a no-arg tool may omit it);
                // default to an empty object so param deserialization behaves
                // exactly as a bare-method call with `{}`.
                let arguments = params_obj
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                request.method = name;
                request.params = arguments;
                is_tools_call = true;
            }
            _ => {}
        }

        debug!(method = %request.method, agent = %identity.agent_id, "MCP: dispatching tool call");

        if !is_known_tool(&request.method) {
            // Via `tools/call` the METHOD (`tools/call`) exists — it is the tool
            // NAME that is unknown, so return Invalid Params, not Method Not Found
            // (hud-09emd). The bare-method path keeps its -32601.
            let error = if is_tools_call {
                JsonRpcError::invalid_params(format!("Unknown tool: {}", request.method))
            } else {
                JsonRpcError::method_not_found(&request.method)
            };
            let resp = McpResponse::err(request.id.clone(), error);
            return serde_json::to_string(&resp).unwrap_or_default();
        }

        // ── Allow-list gate: one check per call, at the boundary ────────────
        let mut capabilities = identity.permissions.clone();
        if let Some((permission, allow_entry)) =
            required_permission(&request.method, &request.params)
        {
            if !identity.allows(&permission) {
                warn!(method = %request.method, agent = %identity.agent_id, "MCP: not allowed");
                let resp = McpResponse::err(
                    request.id.clone(),
                    JsonRpcError::not_allowed(&request.method, &identity.agent_id, &allow_entry),
                );
                return serde_json::to_string(&resp).unwrap_or_default();
            }
            // Tool handlers match exact permission strings.
            capabilities.push(permission);
        }
        if request.method == "publish_to_element" {
            let scene = self.scene.lock().await;
            for instance_name in scene.widget_registry.instances.keys() {
                let permission = format!("publish_widget:{instance_name}");
                if identity.allows(&permission) {
                    capabilities.push(permission);
                }
            }
        }

        // ── Identity-bound params: namespace and portal owner token ─────────
        if let Some(obj) = request.params.as_object_mut() {
            if NAMESPACED_TOOLS.contains(&request.method.as_str()) {
                obj.insert(
                    "namespace".into(),
                    serde_json::Value::String(identity.agent_id.clone()),
                );
            }
            if request.method.starts_with("portal_projection_")
                && request.method != "portal_projection_attach"
                && !obj.contains_key("owner_token")
                && let Some(pid) = obj.get("projection_id").and_then(|v| v.as_str())
                && let Some(token) = self.portal_owner_tokens.lock().ok().and_then(|m| {
                    m.get(&(identity.agent_id.clone(), pid.to_string()))
                        .cloned()
                })
            {
                obj.insert("owner_token".into(), serde_json::Value::String(token));
            }
        }
        let projection_id = request
            .params
            .get("projection_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let id = request.id.clone();
        let mut result = self
            .invoke_tool(&request.method, request.params, &capabilities)
            .await;
        if let (Ok(value), Some(pid)) = (&mut result, projection_id) {
            self.bind_portal_owner_token(&request.method, &identity.agent_id, &pid, value);
        }
        if let Ok(value) = &result {
            if request.method == "inject_composer_paste"
                && value.get("injected").and_then(serde_json::Value::as_bool) == Some(true)
            {
                // Paste is queued for a main-thread-owned composer.  Do not
                // wake the compositor before that drain has established that
                // the focused composer actually changed.
                self.portal_ingress_wake.notify();
            } else if tool_creates_render_work(&request.method, value) {
                self.render_wake.notify();
            }
        }

        let response = match result {
            Ok(value) => {
                // Bare-method path returns the tool's raw JSON verbatim (back-
                // compat). `tools/call` wraps it in the spec result shape.
                let result = if is_tools_call {
                    tools_call_success_result(value)
                } else {
                    value
                };
                McpResponse::ok(id, result)
            }
            Err(e) => {
                error!(error = %e, method = %request.method, "MCP: tool error");
                // MCP (2025-06-18 tools §Error handling) draws a line the bare
                // path does not: a tool that RAN but failed (an execution/
                // business error) is an `isError: true` result so the calling
                // model can observe and react to it; but INVALID ARGUMENTS — a
                // param schema violation (`InvalidParams`) or undeserializable
                // params (`ParseError`) surfaced by the handler's `parse_params`
                // — are PROTOCOL errors and MUST stay JSON-RPC errors (-32602 /
                // -32700), so a spec client can distinguish a malformed call from
                // a recoverable failure. (The unknown-tool / missing-capability
                // protocol errors were already returned above; these argument
                // errors only surface here, inside the handler.) The bare path is
                // unchanged: every error becomes a JSON-RPC error (hud-btveq
                // review: gemini + codex).
                let is_argument_error =
                    matches!(e, McpError::InvalidParams(_) | McpError::ParseError(_));
                if is_tools_call && !is_argument_error {
                    McpResponse::ok(id, tools_call_error_result(&e))
                } else {
                    McpResponse::err(id, JsonRpcError::from(e))
                }
            }
        };

        serde_json::to_string(&response).unwrap_or_else(|e| {
            error!(error = %e, "MCP: failed to serialize response");
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"Internal serialization error"},"id":null}"#.to_string()
        })
    }

    /// Keep portal owner tokens server-side: remember the token an attach
    /// returns (and strip it from the reply), forget it on detach/cleanup.
    fn bind_portal_owner_token(
        &self,
        method: &str,
        agent_id: &str,
        projection_id: &str,
        value: &mut serde_json::Value,
    ) {
        let Ok(mut tokens) = self.portal_owner_tokens.lock() else {
            return;
        };
        let key = (agent_id.to_string(), projection_id.to_string());
        match method {
            "portal_projection_attach" => {
                if let Some(obj) = value.as_object_mut()
                    && let Some(serde_json::Value::String(token)) = obj.remove("owner_token")
                {
                    tokens.insert(key, token);
                }
            }
            "portal_projection_detach" | "portal_projection_cleanup"
                if value.get("accepted").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                tokens.remove(&key);
            }
            _ => {}
        }
    }

    /// Invoke the named tool with the given parameters.
    ///
    /// `caller_capabilities` is the set of capability strings from the
    /// [`CallerContext`] (e.g. `["publish_widget:gauge"]`).  Tools that
    /// require specific capabilities (such as `publish_to_widget`) check this
    /// slice before accessing the scene graph.
    async fn invoke_tool(
        &self,
        method: &str,
        params: serde_json::Value,
        caller_capabilities: &[String],
    ) -> Result<serde_json::Value, crate::McpError> {
        match method {
            "create_tab" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_create_tab(params, &mut scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "create_tile" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_create_tile(params, &mut scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "set_content" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_set_content(params, &mut scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "dismiss" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_dismiss(params, &mut scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "publish_to_zone" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_publish_to_zone(params, &mut scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "list_zones" => {
                let scene = self.scene.lock().await;
                let r = tools::handle_list_zones(params, &scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "list_scene" => {
                let scene = self.scene.lock().await;
                let r = tools::handle_list_scene(params, &scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "list_elements" => {
                let scene = self.scene.lock().await;
                let r = tools::handle_list_elements(params, &scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "publish_to_widget" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_publish_to_widget(params, &mut scene, caller_capabilities)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "list_widgets" => {
                let scene = self.scene.lock().await;
                let r = tools::handle_list_widgets(params, &scene)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "clear_widget" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_clear_widget(params, &mut scene, caller_capabilities)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "register_widget_asset" => {
                let mut registry = self.widget_asset_registry.lock().await;
                let r = tools::handle_register_widget_asset(
                    params,
                    &mut registry,
                    caller_capabilities,
                )?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "publish_to_element" => {
                let mut scene = self.scene.lock().await;
                let r = tools::handle_publish_to_element(params, &mut scene, caller_capabilities)?;
                Ok(
                    serde_json::to_value(r)
                        .map_err(|e| crate::McpError::Internal(e.to_string()))?,
                )
            }
            "inject_composer_paste" => {
                let result =
                    tools::handle_inject_composer_paste(params, self.paste_inject_tx.as_ref())?;
                serde_json::to_value(result).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            // Portal projection tools (hud-bq0gl.2): wire authority/published-content
            // through the coalescer + InProcessPortalDriver drain path onto the live scene.
            "portal_projection_list" => {
                let r = tools::handle_portal_projection_list(params, self.portal_op_tx.as_ref())
                    .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_attach" => {
                let r = tools::handle_portal_projection_attach_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_publish" => {
                let r = tools::handle_portal_projection_publish_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_publish_status" => {
                let r = tools::handle_portal_projection_publish_status_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_get_pending_input" => {
                let r = tools::handle_portal_projection_get_pending_input_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_acknowledge_input" => {
                let r = tools::handle_portal_projection_acknowledge_input_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_detach" => {
                let r = tools::handle_portal_projection_detach_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            "portal_projection_cleanup" => {
                let r = tools::handle_portal_projection_cleanup_with_render_wake(
                    params,
                    self.portal_op_tx.as_ref(),
                    &self.portal_ingress_wake,
                )
                .await?;
                serde_json::to_value(r).map_err(|e| crate::McpError::Internal(e.to_string()))
            }
            unknown => Err(crate::McpError::MethodNotFound(unknown.to_string())),
        }
    }

    /// Run a minimal HTTP server on the given address, dispatching all POST `/`
    /// requests to [`Self::dispatch`].
    ///
    /// This is a lightweight reference implementation for integration tests and
    /// development. It is intentionally simple: one-request-at-a-time parsing
    /// on a single TCP stream, no TLS, no keep-alive.
    ///
    /// For production use, wire [`Self::dispatch`] into your HTTP framework of
    /// choice (axum, actix-web, hyper, etc.).
    #[cfg(feature = "http")]
    pub async fn run_http(self: Arc<Self>, addr: &str) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(addr).await?;
        tracing::info!(addr = %addr, "MCP HTTP server listening");

        loop {
            let (mut stream, peer) = listener.accept().await?;
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let n = match stream.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) => {
                        error!(peer = %peer, error = %e, "MCP: read error");
                        return;
                    }
                };

                // Extract headers and body from the raw HTTP request
                let raw = &buf[..n];
                let (header_section, body) =
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&raw[..pos]).unwrap_or("");
                        let body = std::str::from_utf8(&raw[pos + 4..]).unwrap_or("");
                        (headers, body)
                    } else {
                        ("", std::str::from_utf8(raw).unwrap_or(""))
                    };

                // Extract Bearer token from Authorization header.
                // Header name matching is case-insensitive (HTTP/1.1 §3.2).
                // Scheme matching is also case-insensitive per RFC 7235 §2.1
                // ("Bearer" is the registered scheme but clients may lowercase it).
                let bearer_token = header_section
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("authorization:"))
                    .and_then(|l| l.split_once(':').map(|x| x.1))
                    .map(|v| v.trim())
                    .and_then(|v| {
                        // Split into scheme + credentials; accept any case of "Bearer".
                        let mut parts = v.splitn(2, ' ');
                        match (parts.next(), parts.next()) {
                            (Some(scheme), Some(credentials))
                                if scheme.eq_ignore_ascii_case("bearer") =>
                            {
                                Some(credentials.trim().to_owned())
                            }
                            _ => None,
                        }
                    });

                let ctx = server.caller_context(bearer_token);

                let response_body = server.dispatch(body, &ctx).await;

                let http_response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );

                if let Err(e) = stream.write_all(http_response.as_bytes()).await {
                    error!(peer = %peer, error = %e, "MCP: write error");
                }
            });
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Channel closure is presentation-relevant: the winit owner marks all
        // still-attached projections disconnected when it observes this sender
        // disappear. Close the channel before notifying so the awakened drain
        // cannot race ahead and observe an empty-but-still-connected receiver.
        if self.portal_op_tx.take().is_some() {
            self.portal_ingress_wake.notify();
        }
    }
}

fn tool_creates_render_work(method: &str, result: &serde_json::Value) -> bool {
    match method {
        // ClearWidget is intentionally successful when the caller has no
        // publication to remove. Only a changed scene needs compositor work.
        "clear_widget" => result.get("changed").and_then(serde_json::Value::as_bool) == Some(true),
        _ => matches!(
            method,
            "create_tab"
                | "create_tile"
                | "set_content"
                | "dismiss"
                | "publish_to_zone"
                | "publish_to_widget"
                | "publish_to_element"
        ),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tze_hud_scene::{
        SceneId,
        graph::SceneGraph,
        types::{
            Capability, ContentionPolicy, GeometryPolicy, LayerAttachment, NodeData, Rect,
            RenderingPolicy, ZoneDefinition, ZoneMediaType,
        },
    };

    /// PSK used across all tests.  Tests set this via `MCP_TEST_PSK` env var or
    /// fall back to this compile-time constant.  Either way, auth is always
    /// exercised — there is no bypass.
    const TEST_PSK: &str = "test-psk-do-not-use-in-production";

    /// Build a test server with the test PSK configured.
    fn test_server(scene: SceneGraph) -> McpServer {
        let psk = std::env::var("MCP_TEST_PSK").unwrap_or_else(|_| TEST_PSK.to_string());
        McpServer::new(scene).with_config(McpConfig::with_psk(psk))
    }

    async fn server_with_tab() -> (McpServer, SceneId) {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).expect("create tab");
        let server = test_server(scene);
        (server, tab_id)
    }

    fn parse_response(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("valid JSON response")
    }

    /// Authenticated guest context (no resident_mcp capability).
    fn guest() -> CallerContext {
        let psk = std::env::var("MCP_TEST_PSK").unwrap_or_else(|_| TEST_PSK.to_string());
        CallerContext::with_bearer(psk)
    }

    /// Authenticated context for portal/tile tools (unrestricted test PSK).
    fn resident() -> CallerContext {
        guest()
    }

    // ── JSON-RPC protocol compliance ─────────────────────────────────────────

    #[tokio::test]
    async fn test_malformed_json_returns_parse_error() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server.dispatch("{not valid json", &guest()).await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["code"], -32700);
    }

    #[tokio::test]
    async fn test_wrong_jsonrpc_version_returns_invalid_request() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"1.0","method":"list_zones","id":1}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn test_unknown_method_returns_method_not_found() {
        let (server, _) = server_with_tab().await;
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"does_not_exist","id":1}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        // JSON-RPC 2.0: unknown method must return -32601 Method not found
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn test_request_id_echoed_in_response() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":42}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["id"], 42);
    }

    // ── create_tab (resident) ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_create_tab() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tab","params":{"name":"Alerts"},"id":1}"#,
                &resident(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null());
        assert!(!resp["result"]["tab_id"].as_str().unwrap().is_empty());
        assert_eq!(resp["result"]["name"], "Alerts");
    }

    // ── create_tile (resident) ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_create_tile() {
        let (server, _) = server_with_tab().await;
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tile","params":{"namespace":"a","bounds":{"x":0,"y":0,"width":200,"height":200}},"id":2}"#,
                &resident(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "unexpected error: {}",
            resp["error"]
        );
        assert!(!resp["result"]["tile_id"].as_str().unwrap().is_empty());
    }

    // ── set_content (resident) ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_set_content() {
        let (server, _) = server_with_tab().await;

        // First create a tile (resident)
        let tile_raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tile","params":{"namespace":"a","bounds":{"x":0,"y":0,"width":200,"height":200}},"id":1}"#,
                &resident(),
            )
            .await;
        let tile_resp = parse_response(&tile_raw);
        let tile_id = tile_resp["result"]["tile_id"].as_str().unwrap();

        let content_req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "set_content",
            "params": {"tile_id": tile_id, "content": "# Hello MCP"},
            "id": 2
        });
        let raw = server.dispatch(&content_req.to_string(), &resident()).await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "unexpected error: {}",
            resp["error"]
        );
        assert_eq!(resp["result"]["content_len"], 11);
    }

    // ── publish_to_zone (guest) ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_publish_to_zone() {
        let (server, _) = server_with_tab().await;

        // Register a zone directly on the shared scene
        {
            let mut scene = server.scene.lock().await;
            scene.zone_registry.zones.insert(
                "status".to_string(),
                ZoneDefinition {
                    id: SceneId::new(),
                    name: "status".to_string(),
                    description: "Status zone".to_string(),
                    geometry_policy: GeometryPolicy::Relative {
                        x_pct: 0.0,
                        y_pct: 0.0,
                        width_pct: 1.0,
                        height_pct: 0.05,
                    },
                    accepted_media_types: vec![ZoneMediaType::StreamText],
                    rendering_policy: RenderingPolicy::default(),
                    contention_policy: ContentionPolicy::LatestWins,
                    max_publishers: 4,
                    transport_constraint: None,
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Content,
                },
            );
        }

        // Guest caller can publish to zone without any capabilities
        let req = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_zone",
            "params": {"zone_name": "status", "content": "All systems go"},
            "id": 3
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "unexpected error: {}",
            resp["error"]
        );
        assert_eq!(resp["result"]["zone_name"], "status");
    }

    // ── list_zones (guest) ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_dispatch_list_zones_empty() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":4}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null());
        assert_eq!(resp["result"]["count"], 0);
    }

    #[tokio::test]
    async fn test_dispatch_list_zones_populated() {
        let (server, _) = server_with_tab().await;

        {
            let mut scene = server.scene.lock().await;
            scene.zone_registry.zones.insert(
                "hud".to_string(),
                ZoneDefinition {
                    id: SceneId::new(),
                    name: "hud".to_string(),
                    description: "HUD zone".to_string(),
                    geometry_policy: GeometryPolicy::Relative {
                        x_pct: 0.0,
                        y_pct: 0.0,
                        width_pct: 1.0,
                        height_pct: 0.05,
                    },
                    accepted_media_types: vec![ZoneMediaType::StreamText],
                    rendering_policy: RenderingPolicy::default(),
                    contention_policy: ContentionPolicy::LatestWins,
                    max_publishers: 4,
                    transport_constraint: None,
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Content,
                },
            );
        }

        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":{},"id":5}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null());
        assert_eq!(resp["result"]["count"], 1);
        assert_eq!(resp["result"]["zones"][0]["name"], "hud");
    }

    #[tokio::test]
    async fn test_dispatch_list_elements_tile_filter() {
        let (server, tab_id) = server_with_tab().await;
        let tile_id = {
            let mut scene = server.scene.lock().await;
            let lease_id = scene.grant_lease(
                "agent.list-elements",
                60_000,
                vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
            );
            scene
                .create_tile(
                    tab_id,
                    "agent.list-elements",
                    lease_id,
                    Rect::new(0.0, 0.0, 200.0, 100.0),
                    1,
                )
                .expect("create tile")
        };

        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_elements","params":{"element_type":"tile"},"id":6}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "unexpected error: {}",
            resp["error"]
        );
        assert_eq!(resp["result"]["count"], 1);
        assert_eq!(resp["result"]["elements"][0]["element_type"], "tile");
        assert_eq!(
            resp["result"]["elements"][0]["element_id"],
            tile_id.to_string()
        );
    }

    #[tokio::test]
    async fn test_dispatch_publish_to_element_tile_id() {
        let (server, tab_id) = server_with_tab().await;
        let tile_id = {
            let mut scene = server.scene.lock().await;
            let lease_id = scene.grant_lease(
                "agent.publish-element",
                60_000,
                vec![Capability::CreateTiles, Capability::ModifyOwnTiles],
            );
            scene
                .create_tile(
                    tab_id,
                    "agent.publish-element",
                    lease_id,
                    Rect::new(10.0, 10.0, 180.0, 90.0),
                    1,
                )
                .expect("create tile")
        };

        let req = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_element",
            "params": {"element_id": tile_id.to_string(), "content": "hello tile"},
            "id": 7
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "unexpected error: {}",
            resp["error"]
        );
        assert_eq!(resp["result"]["element_type"], "tile");

        let scene = server.scene.lock().await;
        let tile = scene.tiles.get(&tile_id).expect("tile should exist");
        let root_id = tile.root_node.expect("tile root should be set");
        let root = scene.nodes.get(&root_id).expect("root node should exist");
        match &root.data {
            NodeData::TextMarkdown(text) => assert_eq!(text.content, "hello tile"),
            other => panic!("expected markdown node, got {other:?}"),
        }
    }

    // ── Guest / Resident access control (spec §8.1, §8.3) ───────────────────

    #[tokio::test]
    async fn test_resident_portal_projection_cleanup_dispatches_to_op_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let server = test_server(SceneGraph::new(1920.0, 1080.0)).with_portal_op_tx(tx);
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("cleanup op must be forwarded") {
                crate::portal_op::PortalOp::Cleanup {
                    projection_id,
                    cleanup_authority,
                    operator_authority,
                    reply,
                    ..
                } => {
                    assert_eq!(projection_id, "p1");
                    assert_eq!(cleanup_authority, "operator");
                    assert_eq!(operator_authority.as_deref(), Some("op"));
                    reply.send(Ok(())).expect("cleanup reply must send");
                }
                other => panic!("unexpected portal op: {other:?}"),
            }
        });

        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_cleanup","params":{"projection_id":"p1","cleanup_authority":"operator","operator_authority":"op","reason":"override"},"id":16}"#,
                &resident(),
            )
            .await;
        responder.await.expect("responder must complete");
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null(), "{resp:#}");
        assert_eq!(resp["result"]["accepted"], true);
    }

    #[tokio::test]
    async fn test_resident_portal_projection_list_dispatches_content_free_summaries() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let server = test_server(SceneGraph::new(1920.0, 1080.0)).with_portal_op_tx(tx);
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("list op must be forwarded") {
                crate::portal_op::PortalOp::List { reply } => reply
                    .send(Ok(crate::portal_op::ProjectionListBatch {
                        projections: vec![crate::portal_op::ProjectionListEntry {
                            projection_id: "p1".to_string(),
                            display_name: "Projection".to_string(),
                            lifecycle_state: "active".to_string(),
                            unread_output_count: 2,
                            pending_input_count: 1,
                        }],
                    }))
                    .expect("list reply must send"),
                other => panic!("unexpected portal op: {other:?}"),
            }
        });

        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_list","params":{},"id":16}"#,
                &resident(),
            )
            .await;
        responder.await.expect("responder must complete");
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null(), "{resp:#}");
        assert_eq!(
            resp["result"],
            serde_json::json!({
                "projections": [{
                    "projection_id": "p1",
                    "display_name": "Projection",
                    "lifecycle_state": "active",
                    "unread_output_count": 2,
                    "pending_input_count": 1,
                }]
            }),
            "MCP list must not add content or credential fields"
        );
    }

    #[tokio::test]
    async fn test_bare_projection_token_expiry_is_actionable_and_redacted() {
        use tze_hud_projection::ProjectionErrorCode;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let server = test_server(SceneGraph::new(1920.0, 1080.0)).with_portal_op_tx(tx);
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("publish op must be forwarded") {
                crate::portal_op::PortalOp::PublishOutput { reply, .. } => {
                    reply
                        .send(Err(crate::portal_op::PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionTokenExpired,
                            "owner token secret-token expired beside private text TOP-SECRET",
                        )))
                        .expect("publish rejection must send");
                }
                other => panic!("unexpected portal op: {other:?}"),
            }
        });

        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_publish","params":{"projection_id":"p1","owner_token":"secret-token","output_text":"private"},"id":17}"#,
                &resident(),
            )
            .await;
        responder.await.expect("responder must complete");

        assert!(!raw.contains("secret-token"), "owner token leaked: {raw}");
        assert!(!raw.contains("TOP-SECRET"), "private detail leaked: {raw}");
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["code"], -32103, "{resp:#}");
        assert_eq!(
            resp["error"]["data"]["error_code"],
            "PROJECTION_TOKEN_EXPIRED"
        );
        assert_eq!(
            resp["error"]["data"]["context"]["operation"],
            "portal_projection_publish"
        );
        assert_eq!(
            resp["error"]["data"]["hint"]["recovery_operation"],
            "portal_projection_attach"
        );
        let resolution = resp["error"]["data"]["hint"]["resolution"]
            .as_str()
            .expect("resolution is text");
        assert!(resolution.contains("authenticated"), "{resolution}");
        assert!(
            resolution.contains("original idempotency_key"),
            "{resolution}"
        );
        assert!(
            resolution.contains("rotate the owner token"),
            "{resolution}"
        );
    }

    #[tokio::test]
    async fn portal_owner_token_is_stripped_from_attach_and_injected_later() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let server = test_server(SceneGraph::new(1920.0, 1080.0)).with_portal_op_tx(tx);
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("attach op") {
                crate::portal_op::PortalOp::Attach { reply, .. } => {
                    reply.send(Ok("server-held-token".to_string())).unwrap();
                }
                other => panic!("unexpected portal op: {other:?}"),
            }
            match rx.recv().await.expect("poll op") {
                crate::portal_op::PortalOp::GetPendingInput {
                    owner_token, reply, ..
                } => {
                    assert_eq!(owner_token, "server-held-token");
                    reply
                        .send(Err(crate::portal_op::PortalOpRejection::new(
                            tze_hud_projection::ProjectionErrorCode::ProjectionUnauthorized,
                            "stop here",
                        )))
                        .unwrap();
                }
                other => panic!("unexpected portal op: {other:?}"),
            }
        });

        let attach = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_attach","params":{"projection_id":"p1","display_name":"P1"},"id":1}"#,
                &resident(),
            )
            .await;
        assert!(
            !attach.contains("server-held-token"),
            "attach response must not carry the owner token: {attach}"
        );
        server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_get_pending_input","params":{"projection_id":"p1","wait_ms":0},"id":2}"#,
                &resident(),
            )
            .await;
        responder.await.expect("server injected the bound token");
    }

    #[tokio::test]
    async fn test_tools_call_projection_unauthorized_keeps_structured_recovery() {
        use tze_hud_projection::ProjectionErrorCode;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::portal_op::PortalOp>();
        let server = test_server(SceneGraph::new(1920.0, 1080.0)).with_portal_op_tx(tx);
        let responder = tokio::spawn(async move {
            match rx.recv().await.expect("poll op must be forwarded") {
                crate::portal_op::PortalOp::GetPendingInput { reply, .. } => {
                    reply
                        .send(Err(crate::portal_op::PortalOpRejection::new(
                            ProjectionErrorCode::ProjectionUnauthorized,
                            "wrong owner token secret-token for private projection",
                        )))
                        .expect("poll rejection must send");
                }
                other => panic!("unexpected portal op: {other:?}"),
            }
        });

        let request = json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {
                "name": "portal_projection_get_pending_input",
                "arguments": {
                    "projection_id": "p1",
                    "owner_token": "secret-token",
                    "wait_ms": 0
                }
            },
            "id": 18
        });
        let raw = server.dispatch(&request.to_string(), &resident()).await;
        responder.await.expect("responder must complete");

        assert!(!raw.contains("secret-token"), "owner token leaked: {raw}");
        assert!(
            parse_response(&raw)["error"].is_null(),
            "tools/call execution failures remain MCP results: {raw}"
        );
        let resp = parse_response(&raw);
        assert_eq!(resp["result"]["isError"], true);
        let structured = &resp["result"]["structuredContent"];
        assert_eq!(structured["error_code"], "PROJECTION_UNAUTHORIZED");
        assert_eq!(
            structured["context"]["operation"],
            "portal_projection_get_pending_input"
        );
        assert_eq!(
            structured["hint"]["recovery_operation"],
            "portal_projection_attach"
        );
        let text: serde_json::Value = serde_json::from_str(
            resp["result"]["content"][0]["text"]
                .as_str()
                .expect("projection error text block is JSON"),
        )
        .expect("projection error text is machine-readable JSON");
        assert_eq!(text, *structured);
    }

    #[tokio::test]
    async fn test_projection_authority_not_wired_remains_internal_runtime_fault() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_publish","params":{"projection_id":"p1","owner_token":"token","output_text":"hello"},"id":19}"#,
                &resident(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["code"], -32603, "{resp:#}");
        assert!(resp["error"]["data"]["error_code"].is_null());
        assert!(
            resp["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("authority not wired"))
        );
    }

    #[tokio::test]
    async fn test_structured_error_has_hint_field() {
        let server = restricted_server();
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tile","params":{"bounds":{"x":0,"y":0,"width":200,"height":200}},"id":14}"#,
                &CallerContext::with_bearer("bot-key"),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["data"]["error_code"], "NOT_ALLOWED");
        let hint = resp["error"]["data"]["hint"]
            .as_str()
            .expect("hint is a string");
        assert!(hint.contains("\"tiles\""), "{hint}");
    }

    #[tokio::test]
    async fn test_guest_can_call_list_zones() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":20}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        // Should succeed with no error — authenticated guest can use guest tools
        assert!(
            resp["error"].is_null(),
            "authenticated guest should be able to call list_zones"
        );
    }

    #[tokio::test]
    async fn test_guest_can_call_list_scene() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_scene","params":null,"id":21}"#,
                &guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "authenticated guest should be able to call list_scene"
        );
    }

    #[tokio::test]
    async fn test_resident_can_call_resident_tools() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tab","params":{"name":"T"},"id":22}"#,
                &resident(),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "authenticated resident should be able to call create_tab"
        );
    }

    // ── Per-call authentication (spec §8.4) ──────────────────────────────────

    #[tokio::test]
    async fn test_auth_accepted_via_bearer_token() {
        let server = McpServer::new(SceneGraph::new(1920.0, 1080.0))
            .with_config(McpConfig::with_psk("secret-key"));
        let ctx = CallerContext::with_bearer("secret-key");
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":30}"#,
                &ctx,
            )
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "valid bearer token should authenticate"
        );
    }

    #[tokio::test]
    async fn test_auth_accepted_via_params_field() {
        let server = McpServer::new(SceneGraph::new(1920.0, 1080.0))
            .with_config(McpConfig::with_psk("secret-key"));
        let req = json!({
            "jsonrpc": "2.0",
            "method": "list_zones",
            "params": {"_auth": "secret-key"},
            "id": 31
        });
        let raw = server
            .dispatch(&req.to_string(), &CallerContext::guest())
            .await;
        let resp = parse_response(&raw);
        assert!(
            resp["error"].is_null(),
            "valid _auth param should authenticate"
        );
    }

    #[tokio::test]
    async fn test_auth_rejected_with_wrong_key() {
        let server = McpServer::new(SceneGraph::new(1920.0, 1080.0))
            .with_config(McpConfig::with_psk("secret-key"));
        let ctx = CallerContext::with_bearer("wrong-key");
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":32}"#,
                &ctx,
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(
            resp["error"]["code"], -32004,
            "wrong key should be rejected"
        );
    }

    #[tokio::test]
    async fn test_auth_rejected_with_no_key() {
        let server = McpServer::new(SceneGraph::new(1920.0, 1080.0))
            .with_config(McpConfig::with_psk("secret-key"));
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":33}"#,
                &CallerContext::guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(
            resp["error"]["code"], -32004,
            "missing key should be rejected"
        );
    }

    #[tokio::test]
    async fn test_each_call_authenticated_independently() {
        // Spec §8.4: two consecutive calls, each must carry auth independently.
        let server = Arc::new(
            McpServer::new(SceneGraph::new(1920.0, 1080.0)).with_config(McpConfig::with_psk("k")),
        );
        let good = CallerContext::with_bearer("k");
        let bad = CallerContext::guest();

        let r1 = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":40}"#,
                &good,
            )
            .await;
        let r2 = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":41}"#,
                &bad,
            )
            .await;

        let resp1 = parse_response(&r1);
        let resp2 = parse_response(&r2);

        // First call: success (authenticated)
        assert!(
            resp1["error"].is_null(),
            "first call with valid key should succeed"
        );
        // Second call: rejected (no key — per-call auth, no persistent session)
        assert_eq!(
            resp2["error"]["code"], -32004,
            "second call without key should fail"
        );
    }

    #[tokio::test]
    async fn test_no_psk_config_rejects_all_calls() {
        // Security: when no PSK is configured, ALL calls are rejected.
        // There is no bypass mode — sovereignty enforced by mechanism, not convention.
        // (Spec: heart-and-soul/security.md, session-protocol/spec.md §Auth)
        let server = McpServer::new(SceneGraph::new(1920.0, 1080.0));
        // Even with no PSK configured, calls must be rejected
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":50}"#,
                &CallerContext::guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(
            resp["error"]["code"], -32004,
            "call must be rejected when no PSK is configured (no bypass mode)"
        );
    }

    #[tokio::test]
    async fn test_unauthenticated_call_rejected_even_for_guest_tools() {
        // Spec §8.4: guest tools are unconditionally accessible but STILL require
        // authentication.  An unauthenticated caller cannot reach any tool.
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        // CallerContext::guest() has no bearer token — unauthenticated
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":null,"id":51}"#,
                &CallerContext::guest(),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(
            resp["error"]["code"], -32004,
            "unauthenticated caller must be rejected even for guest tools"
        );
    }

    // ── Identity and allow lists ─────────────────────────────────────────────

    fn restricted_server() -> McpServer {
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        scene.zone_registry = tze_hud_scene::types::ZoneRegistry::with_defaults();
        let agents = AgentDirectory {
            runtime_psk: "runtime".into(),
            agent_psks: [("bot".to_string(), "bot-key".to_string())].into(),
            permissions: [("bot".to_string(), vec!["publish_zone:subtitle".to_string()])].into(),
            fallback_permissions: vec![],
        };
        McpServer::new(scene).with_config(McpConfig::with_agents(agents))
    }

    #[tokio::test]
    async fn allowed_zone_publish_succeeds_and_uses_agent_namespace() {
        let server = restricted_server();
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"publish_to_zone","params":{"zone_name":"subtitle","content":"hi","namespace":"spoofed"},"id":70}"#,
                &CallerContext::with_bearer("bot-key"),
            )
            .await;
        let resp = parse_response(&raw);
        assert!(resp["error"].is_null(), "{resp:#}");
        let scene = server.scene.lock().await;
        let publisher = scene.zone_registry.active_publishes["subtitle"][0]
            .publisher_namespace
            .clone();
        assert_eq!(
            publisher, "bot",
            "namespace comes from identity, not params"
        );
    }

    #[tokio::test]
    async fn disallowed_tool_returns_not_allowed_with_hint() {
        let server = restricted_server();
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tab","params":{"name":"X"},"id":71}"#,
                &CallerContext::with_bearer("bot-key"),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["data"]["error_code"], "NOT_ALLOWED");
        let hint = resp["error"]["data"]["hint"].as_str().unwrap();
        assert!(
            hint.contains("\"tiles\"") && hint.contains("[agents.bot]"),
            "{hint}"
        );
    }

    #[tokio::test]
    async fn runtime_psk_without_agent_table_gets_no_permissions() {
        let server = restricted_server();
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"publish_to_zone","params":{"zone_name":"subtitle","content":"hi"},"id":72}"#,
                &CallerContext::with_bearer("runtime"),
            )
            .await;
        let resp = parse_response(&raw);
        assert_eq!(resp["error"]["data"]["error_code"], "NOT_ALLOWED");
    }

    #[tokio::test]
    async fn unknown_psk_is_unauthenticated() {
        let server = restricted_server();
        let raw = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_zones","params":{},"id":73}"#,
                &CallerContext::with_bearer("nope"),
            )
            .await;
        assert_eq!(parse_response(&raw)["error"]["code"], -32004);
    }

    #[tokio::test]
    async fn test_dispatch_register_widget_asset_preflight_dedup_hit() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let payload =
            r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="2" height="2"/></svg>"#;
        let hash_hex = {
            let mut s = String::with_capacity(64);
            for b in blake3::hash(payload.as_bytes()).as_bytes() {
                use std::fmt::Write;
                let _ = write!(&mut s, "{b:02x}");
            }
            s
        };

        let psk = std::env::var("MCP_TEST_PSK").unwrap_or_else(|_| TEST_PSK.to_string());
        let ctx = CallerContext::with_bearer(psk);

        let upload_req = json!({
            "jsonrpc": "2.0",
            "method": "register_widget_asset",
            "params": {
                "widget_type_id": "gauge",
                "svg_filename": "cell.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "payload": payload
            },
            "id": 60
        });
        let upload_raw = server.dispatch(&upload_req.to_string(), &ctx).await;
        let upload_resp = parse_response(&upload_raw);
        assert!(upload_resp["error"].is_null(), "{upload_resp:#}");
        assert_eq!(upload_resp["result"]["accepted"], true);
        assert_eq!(upload_resp["result"]["was_deduplicated"], false);

        let preflight_req = json!({
            "jsonrpc": "2.0",
            "method": "register_widget_asset",
            "params": {
                "widget_type_id": "gauge",
                "svg_filename": "cell.svg",
                "content_hash_blake3": hash_hex,
                "total_size_bytes": payload.len(),
                "metadata_only_preflight": true
            },
            "id": 61
        });
        let preflight_raw = server.dispatch(&preflight_req.to_string(), &ctx).await;
        let preflight_resp = parse_response(&preflight_raw);
        assert!(preflight_resp["error"].is_null(), "{preflight_resp:#}");
        assert_eq!(preflight_resp["result"]["accepted"], true);
        assert_eq!(preflight_resp["result"]["was_deduplicated"], true);
    }

    // ── Gauge widget MCP integration tests (hud-qc0c, openspec task 10) ────────
    //
    // These tests exercise publish_to_widget and list_widgets through the actual
    // McpServer::dispatch path — the same code path an LLM would take.  They
    // use a full 4-parameter gauge definition mirroring the production schema
    // (level f32, label string, fill_color color, severity enum) so the
    // assertions cover all four parameter types.

    /// Build a server pre-seeded with the full production-schema gauge widget.
    ///
    /// The gauge definition mirrors `assets/widgets/gauge/widget.toml`:
    /// - level: f32, [0.0, 1.0], default 0.0
    /// - label: string, default ""
    /// - fill_color: color, default (74,158,255,255) → Rgba f32 components
    /// - severity: enum, allowed [info, warning, error], default "info"
    async fn server_with_gauge() -> McpServer {
        use std::collections::HashMap;
        use tze_hud_scene::types::{
            ContentionPolicy as CP, GeometryPolicy, RenderingPolicy, Rgba, WidgetDefinition,
            WidgetInstance, WidgetParamConstraints, WidgetParamType, WidgetParameterDeclaration,
            WidgetParameterValue,
        };

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).expect("create tab");

        scene.widget_registry.register_definition(WidgetDefinition {
            id: "gauge".to_string(),
            name: "Gauge".to_string(),
            description: "Vertical fill gauge — level, label, fill color, severity indicator"
                .to_string(),
            parameter_schema: vec![
                WidgetParameterDeclaration {
                    name: "level".to_string(),
                    param_type: WidgetParamType::F32,
                    default_value: WidgetParameterValue::F32(0.0),
                    constraints: Some(WidgetParamConstraints {
                        f32_min: Some(0.0),
                        f32_max: Some(1.0),
                        string_max_bytes: None,
                        enum_allowed_values: vec![],
                    }),
                },
                WidgetParameterDeclaration {
                    name: "label".to_string(),
                    param_type: WidgetParamType::String,
                    default_value: WidgetParameterValue::String(String::new()),
                    constraints: None,
                },
                WidgetParameterDeclaration {
                    name: "fill_color".to_string(),
                    param_type: WidgetParamType::Color,
                    default_value: WidgetParameterValue::Color(Rgba {
                        r: 74.0 / 255.0,
                        g: 158.0 / 255.0,
                        b: 1.0,
                        a: 1.0,
                    }),
                    constraints: None,
                },
                WidgetParameterDeclaration {
                    name: "severity".to_string(),
                    param_type: WidgetParamType::Enum,
                    default_value: WidgetParameterValue::Enum("info".to_string()),
                    constraints: Some(WidgetParamConstraints {
                        f32_min: None,
                        f32_max: None,
                        string_max_bytes: None,
                        enum_allowed_values: vec![
                            "info".to_string(),
                            "warning".to_string(),
                            "error".to_string(),
                        ],
                    }),
                },
            ],
            layers: vec![],
            default_geometry_policy: GeometryPolicy::Relative {
                x_pct: 0.0,
                y_pct: 0.0,
                width_pct: 0.2,
                height_pct: 0.5,
            },
            default_rendering_policy: RenderingPolicy::default(),
            default_contention_policy: CP::LatestWins,
            max_publishers: WidgetDefinition::default_max_publishers(),
            ephemeral: false,
            hover_behavior: None,
        });

        let default_params: HashMap<String, WidgetParameterValue> = HashMap::from([
            ("level".to_string(), WidgetParameterValue::F32(0.0)),
            (
                "label".to_string(),
                WidgetParameterValue::String(String::new()),
            ),
            (
                "fill_color".to_string(),
                WidgetParameterValue::Color(Rgba {
                    r: 74.0 / 255.0,
                    g: 158.0 / 255.0,
                    b: 1.0,
                    a: 1.0,
                }),
            ),
            (
                "severity".to_string(),
                WidgetParameterValue::Enum("info".to_string()),
            ),
        ]);

        scene.widget_registry.register_instance(WidgetInstance {
            id: SceneId::new(),
            widget_type_name: "gauge".to_string(),
            tab_id,
            geometry_override: None,
            contention_override: None,
            instance_name: "gauge".to_string(),
            current_params: default_params,
        });

        test_server(scene)
    }

    // 10.1 — Set level via MCP.
    //
    // Publish level=0.75 and verify the response confirms the param was applied
    // and the widget name echoes back.  Verifies the basic publish path works
    // end-to-end through dispatch().
    #[tokio::test]
    async fn test_gauge_publish_level_via_dispatch() {
        let server = server_with_gauge().await;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_widget",
            "params": {
                "widget_name": "gauge",
                "params": {"level": 0.75}
            },
            "id": 100
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);

        assert!(
            resp["error"].is_null(),
            "publish level=0.75 should succeed, got: {}",
            resp["error"]
        );
        assert_eq!(
            resp["result"]["widget_name"], "gauge",
            "response must echo widget_name"
        );
        let applied: Vec<String> = serde_json::from_value(resp["result"]["applied_params"].clone())
            .expect("applied_params must be an array");
        assert!(
            applied.contains(&"level".to_string()),
            "applied_params must contain 'level', got: {applied:?}"
        );

        // Verify the scene recorded the updated level via list_widgets.
        let list_req = json!({
            "jsonrpc": "2.0",
            "method": "list_widgets",
            "params": {},
            "id": 101
        });
        let list_raw = server.dispatch(&list_req.to_string(), &guest()).await;
        let list_resp = parse_response(&list_raw);
        assert!(list_resp["error"].is_null());
        let instances = &list_resp["result"]["widget_instances"];
        let gauge_inst = instances
            .as_array()
            .expect("widget_instances is array")
            .iter()
            .find(|i| i["instance_name"] == "gauge")
            .expect("gauge instance must be present");
        let level_val = gauge_inst["current_params"]["level"]
            .as_f64()
            .expect("level must be a number");
        assert!(
            (level_val - 0.75).abs() < 1e-4,
            "current level in scene should be ~0.75, got {level_val}"
        );
    }

    // 10.2 — Animate level with transition_ms.
    //
    // Publish level=0.9 with transition_ms=300.  The response must succeed and
    // echo back the widget name with the param applied.  The transition_ms field
    // is forwarded to the scene graph; this test confirms it is not rejected.
    #[tokio::test]
    async fn test_gauge_publish_level_with_transition_via_dispatch() {
        let server = server_with_gauge().await;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_widget",
            "params": {
                "widget_name": "gauge",
                "params": {"level": 0.9},
                "transition_ms": 300
            },
            "id": 110
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);

        assert!(
            resp["error"].is_null(),
            "publish level=0.9 transition_ms=300 should succeed, got: {}",
            resp["error"]
        );
        let applied: Vec<String> = serde_json::from_value(resp["result"]["applied_params"].clone())
            .expect("applied_params must be an array");
        assert!(
            applied.contains(&"level".to_string()),
            "applied_params must contain 'level' for transition publish, got: {applied:?}"
        );
    }

    // 10.3 — Set all four parameters in one call.
    //
    // Publish level=0.65, label="CPU", fill_color={orange}, severity="warning"
    // in a single publish_to_widget call.  Verifies all four param types are
    // accepted together and all appear in applied_params.
    #[tokio::test]
    async fn test_gauge_publish_all_four_params_via_dispatch() {
        let server = server_with_gauge().await;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_widget",
            "params": {
                "widget_name": "gauge",
                "params": {
                    "level": 0.65,
                    "label": "CPU",
                    "fill_color": {"r": 1.0, "g": 0.647, "b": 0.0, "a": 1.0},
                    "severity": "warning"
                }
            },
            "id": 120
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);

        assert!(
            resp["error"].is_null(),
            "all-four-params publish should succeed, got: {}",
            resp["error"]
        );
        let applied: Vec<String> = serde_json::from_value(resp["result"]["applied_params"].clone())
            .expect("applied_params must be an array");
        for param in &["level", "label", "fill_color", "severity"] {
            assert!(
                applied.contains(&param.to_string()),
                "applied_params must contain '{param}', got: {applied:?}"
            );
        }
        assert_eq!(applied.len(), 4, "exactly 4 params should be applied");
    }

    // 10.4 — Severity transition sequence: info → warning → error.
    //
    // Each severity change is a separate publish with only severity in params.
    // Verifies that each snap-to-color transition succeeds and that list_widgets
    // reflects the latest severity after each call.
    #[tokio::test]
    async fn test_gauge_severity_sequence_via_dispatch() {
        let server = server_with_gauge().await;

        for (id, severity) in [(130u32, "info"), (131, "warning"), (132, "error")] {
            let req = json!({
                "jsonrpc": "2.0",
                "method": "publish_to_widget",
                "params": {
                    "widget_name": "gauge",
                    "params": {"severity": severity}
                },
                "id": id
            });
            let raw = server.dispatch(&req.to_string(), &guest()).await;
            let resp = parse_response(&raw);
            assert!(
                resp["error"].is_null(),
                "publish severity={severity} should succeed, got: {}",
                resp["error"]
            );
            let applied: Vec<String> =
                serde_json::from_value(resp["result"]["applied_params"].clone())
                    .expect("applied_params must be an array");
            assert!(
                applied.contains(&"severity".to_string()),
                "applied_params must contain 'severity' for {severity} publish, got: {applied:?}"
            );

            // Verify scene reflects the latest severity.
            let list_req = json!({
                "jsonrpc": "2.0",
                "method": "list_widgets",
                "params": {},
                "id": id + 100
            });
            let list_raw = server.dispatch(&list_req.to_string(), &guest()).await;
            let list_resp = parse_response(&list_raw);
            let instances = &list_resp["result"]["widget_instances"];
            let gauge = instances
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["instance_name"] == "gauge")
                .expect("gauge instance");
            assert_eq!(
                gauge["current_params"]["severity"], severity,
                "scene severity should be '{severity}' after publish"
            );
        }
    }

    // 10.5 — Rapid successive publishes: level=0.3 then level=0.8 transition_ms=300.
    //
    // The second publish must succeed and its level must win in the scene state
    // (LatestWins contention policy).  Verifies that back-to-back publishes do
    // not interfere with each other at the MCP dispatch layer.
    #[tokio::test]
    async fn test_gauge_rapid_successive_publishes_via_dispatch() {
        let server = server_with_gauge().await;

        // First publish: level=0.3 (instant).
        let req1 = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_widget",
            "params": {
                "widget_name": "gauge",
                "params": {"level": 0.3}
            },
            "id": 140
        });
        let raw1 = server.dispatch(&req1.to_string(), &guest()).await;
        let resp1 = parse_response(&raw1);
        assert!(
            resp1["error"].is_null(),
            "first rapid publish should succeed"
        );

        // Second publish: level=0.8 with 300ms transition — immediately follows.
        let req2 = json!({
            "jsonrpc": "2.0",
            "method": "publish_to_widget",
            "params": {
                "widget_name": "gauge",
                "params": {"level": 0.8},
                "transition_ms": 300
            },
            "id": 141
        });
        let raw2 = server.dispatch(&req2.to_string(), &guest()).await;
        let resp2 = parse_response(&raw2);
        assert!(
            resp2["error"].is_null(),
            "second rapid publish should succeed (LatestWins interrupts first)"
        );

        // Scene state: the second publish target value (0.8) must be the latest.
        let list_req = json!({
            "jsonrpc": "2.0",
            "method": "list_widgets",
            "params": {},
            "id": 142
        });
        let list_raw = server.dispatch(&list_req.to_string(), &guest()).await;
        let list_resp = parse_response(&list_raw);
        let instances = &list_resp["result"]["widget_instances"];
        let gauge = instances
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["instance_name"] == "gauge")
            .expect("gauge instance");
        let level = gauge["current_params"]["level"]
            .as_f64()
            .expect("level must be a number");
        assert!(
            (level - 0.8).abs() < 1e-4,
            "after rapid publishes, scene level should be ~0.8 (second publish wins), got {level}"
        );
    }

    // 10.6 — list_widgets schema discovery.
    //
    // Verifies that list_widgets returns the gauge type with widget_type="gauge",
    // and that the parameter schema includes all four params with correct types,
    // defaults, and constraints.  This is how an LLM discovers the gauge API.
    #[tokio::test]
    async fn test_gauge_list_widgets_schema_discovery_via_dispatch() {
        let server = server_with_gauge().await;

        let req = json!({
            "jsonrpc": "2.0",
            "method": "list_widgets",
            "params": {},
            "id": 150
        });
        let raw = server.dispatch(&req.to_string(), &guest()).await;
        let resp = parse_response(&raw);

        assert!(
            resp["error"].is_null(),
            "list_widgets should succeed, got: {}",
            resp["error"]
        );

        // Type count and instance count.
        assert_eq!(
            resp["result"]["type_count"], 1,
            "one widget type registered"
        );
        assert_eq!(
            resp["result"]["instance_count"], 1,
            "one widget instance registered"
        );

        // Widget type entry.
        let types = resp["result"]["widget_types"]
            .as_array()
            .expect("widget_types must be an array");
        assert_eq!(types.len(), 1);
        let ty = &types[0];
        assert_eq!(ty["id"], "gauge", "widget type id must be 'gauge'");
        assert_eq!(
            ty["ephemeral"], false,
            "gauge must be durable (not ephemeral)"
        );

        // Parameter schema: all four params must be present with correct types.
        let schema = ty["parameter_schema"]
            .as_array()
            .expect("parameter_schema must be an array");
        assert_eq!(
            schema.len(),
            4,
            "gauge schema must have exactly 4 parameters"
        );

        let find_param = |name: &str| {
            schema
                .iter()
                .find(|p| p["name"] == name)
                .unwrap_or_else(|| panic!("parameter '{name}' must be present in gauge schema"))
        };

        let level = find_param("level");
        assert_eq!(level["param_type"], "f32", "level must be f32");
        assert!(
            (level["f32_min"].as_f64().expect("f32_min") - 0.0).abs() < 1e-6,
            "level f32_min must be 0.0"
        );
        assert!(
            (level["f32_max"].as_f64().expect("f32_max") - 1.0).abs() < 1e-6,
            "level f32_max must be 1.0"
        );

        let label = find_param("label");
        assert_eq!(label["param_type"], "string", "label must be string");

        let fill_color = find_param("fill_color");
        assert_eq!(
            fill_color["param_type"], "color",
            "fill_color must be color"
        );

        let severity = find_param("severity");
        assert_eq!(severity["param_type"], "enum", "severity must be enum");
        let allowed: Vec<String> = serde_json::from_value(severity["enum_allowed_values"].clone())
            .expect("enum_allowed_values must be an array");
        assert_eq!(
            allowed,
            vec!["info", "warning", "error"],
            "severity allowed_values must be [info, warning, error] in order"
        );

        // Instance entry.
        let instances = resp["result"]["widget_instances"]
            .as_array()
            .expect("widget_instances must be an array");
        assert_eq!(instances.len(), 1);
        let inst = &instances[0];
        assert_eq!(inst["instance_name"], "gauge");
        assert_eq!(inst["widget_type"], "gauge");
    }

    // ── initialize + tools/list introspection (hud-l9lf6) ────────────────────

    #[tokio::test]
    async fn test_initialize_returns_well_formed_result() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let req = json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "0.0.0" }
            },
            "id": 1
        });
        let raw = server.dispatch(&req.to_string(), &resident()).await;
        let resp = parse_response(&raw);

        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        let result = &resp["result"];
        // Protocol version is a non-empty string.
        assert!(
            result["protocolVersion"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "missing protocolVersion: {result}"
        );
        // serverInfo carries name + version.
        assert_eq!(result["serverInfo"]["name"], "tze_hud_mcp");
        assert!(
            result["serverInfo"]["version"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "missing serverInfo.version: {result}"
        );
        // capabilities is an object declaring tools.
        assert!(
            result["capabilities"].is_object(),
            "capabilities not an object"
        );
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn test_initialize_requires_auth() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let req = json!({ "jsonrpc": "2.0", "method": "initialize", "params": {}, "id": 1 });
        // Guest with no bearer token (unauthenticated) is rejected.
        let raw = server
            .dispatch(&req.to_string(), &CallerContext::guest())
            .await;
        let resp = parse_response(&raw);
        assert!(resp.get("result").is_none(), "initialize must require auth");
        assert!(resp.get("error").is_some());
    }

    #[tokio::test]
    async fn test_tools_list_returns_all_tools_with_schemas() {
        let server = test_server(SceneGraph::new(1920.0, 1080.0));
        let req = json!({ "jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 7 });
        let raw = server.dispatch(&req.to_string(), &resident()).await;
        let resp = parse_response(&raw);

        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
        let tools = resp["result"]["tools"]
            .as_array()
            .expect("tools must be an array");
        assert!(!tools.is_empty(), "tools list must be non-empty");

        // Index by name and validate every descriptor is well-formed.
        let mut by_name = std::collections::HashMap::new();
        for t in tools {
            let name = t["name"].as_str().expect("tool must have a name");
            assert!(
                t["description"].as_str().is_some_and(|s| !s.is_empty()),
                "tool {name} missing description"
            );
            let schema = &t["inputSchema"];
            assert_eq!(
                schema["type"], "object",
                "tool {name} inputSchema not object"
            );
            assert!(
                schema["properties"].is_object(),
                "tool {name} inputSchema missing properties"
            );
            assert!(
                schema["required"].is_array(),
                "tool {name} inputSchema missing required array"
            );
            by_name.insert(name.to_string(), t.clone());
        }

        // AC(1): every portal_projection_* tool is present with a valid schema.
        for name in [
            "portal_projection_list",
            "portal_projection_attach",
            "portal_projection_publish",
            "portal_projection_publish_status",
            "portal_projection_get_pending_input",
            "portal_projection_acknowledge_input",
            "portal_projection_detach",
            "portal_projection_cleanup",
        ] {
            assert!(by_name.contains_key(name), "missing portal tool: {name}");
        }

        // AC(3): get_pending_input schema reflects the actual poll-budget fields
        // (the *Params struct exposes max_items / max_bytes plus the long-poll
        // wait_ms field added once PR #967 merged — hud-yqe79).
        let gpi = &by_name["portal_projection_get_pending_input"]["inputSchema"];
        assert!(
            gpi["properties"]["max_items"].is_object(),
            "get_pending_input schema missing max_items"
        );
        assert!(
            gpi["properties"]["max_bytes"].is_object(),
            "get_pending_input schema missing max_bytes"
        );
        assert!(
            gpi["properties"]["wait_ms"].is_object(),
            "get_pending_input schema missing wait_ms"
        );
        let gpi_required: Vec<&str> = gpi["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(gpi_required.contains(&"projection_id"));
        assert!(
            gpi["properties"]["owner_token"].is_null(),
            "owner_token is bound server-side, never in model context"
        );

        // AC(3): publish schema includes the output_kind field.
        let pub_schema = &by_name["portal_projection_publish"]["inputSchema"];
        assert!(
            pub_schema["properties"]["output_kind"].is_object(),
            "publish schema missing output_kind"
        );

        // attach schema reflects whatever fields exist in PortalProjectionAttachParams
        // at HEAD: projection_id + display_name required, idempotency_key optional.
        let attach = &by_name["portal_projection_attach"]["inputSchema"];
        assert!(attach["properties"]["projection_id"].is_object());
        assert!(attach["properties"]["display_name"].is_object());
        assert!(attach["properties"]["idempotency_key"].is_object());
        let attach_required: Vec<&str> = attach["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(attach_required.contains(&"projection_id"));
        assert!(attach_required.contains(&"display_name"));
        assert!(!attach_required.contains(&"idempotency_key"));
    }

    // ── MCP tools/call dispatch (hud-09emd) ──────────────────────────────────

    /// `tools/call` delegates to the SAME tool the bare-method path runs, and
    /// wraps its result in the spec shape (`content` array + `isError: false`).
    /// The wrapped text is the JSON serialization of the identical tool payload.
    #[tokio::test]
    async fn test_tools_call_delegates_to_same_tool_and_spec_shapes_result() {
        let server = server_with_gauge().await;

        // Bare method form → raw tool JSON as `result`.
        let bare = json!({"jsonrpc":"2.0","method":"list_widgets","params":{},"id":1});
        let bare_resp = parse_response(&server.dispatch(&bare.to_string(), &guest()).await);
        assert!(
            bare_resp["error"].is_null(),
            "bare list_widgets failed: {bare_resp}"
        );
        let bare_result = bare_resp["result"].clone();
        assert!(bare_result["widget_instances"].is_array());

        // tools/call form → same tool, spec-shaped result.
        let call = json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"name":"list_widgets","arguments":{}},
            "id":2
        });
        let call_resp = parse_response(&server.dispatch(&call.to_string(), &guest()).await);
        assert!(
            call_resp["error"].is_null(),
            "tools/call must be supported (no -32601): {call_resp}"
        );
        assert_eq!(call_resp["result"]["isError"], json!(false));
        let content = call_resp["result"]["content"]
            .as_array()
            .expect("tools/call result has a content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        // The text block carries the SAME tool payload the bare path returned.
        let inner: serde_json::Value = serde_json::from_str(
            content[0]["text"]
                .as_str()
                .expect("content text is a string"),
        )
        .expect("content text is the tool's JSON result");
        assert_eq!(
            inner, bare_result,
            "tools/call must delegate to the identical dispatch table / payload"
        );
    }

    /// An unknown tool NAME via `tools/call` is Invalid Params (-32602), NOT
    /// Method Not Found (-32601) — the `tools/call` method itself exists. The
    /// bare-method path keeps returning -32601 for an unknown method.
    #[tokio::test]
    async fn test_tools_call_unknown_tool_is_invalid_params_not_method_not_found() {
        let server = server_with_gauge().await;

        let call = json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"name":"no_such_tool","arguments":{}},
            "id":3
        });
        let resp = parse_response(&server.dispatch(&call.to_string(), &guest()).await);
        assert_eq!(
            resp["error"]["code"],
            json!(-32602),
            "unknown tool via tools/call must be Invalid Params, not -32601: {resp}"
        );

        // Bare-method unknown method stays Method Not Found (back-compat).
        let bare = json!({"jsonrpc":"2.0","method":"no_such_tool","params":{},"id":4});
        let bare_resp = parse_response(&server.dispatch(&bare.to_string(), &guest()).await);
        assert_eq!(
            bare_resp["error"]["code"],
            json!(-32601),
            "bare-method unknown method stays Method Not Found"
        );
    }

    /// `tools/call` with no `name` is Invalid Params.
    #[tokio::test]
    async fn test_tools_call_missing_name_is_invalid_params() {
        let server = server_with_gauge().await;
        let call = json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"arguments":{}},
            "id":5
        });
        let resp = parse_response(&server.dispatch(&call.to_string(), &guest()).await);
        assert_eq!(
            resp["error"]["code"],
            json!(-32602),
            "tools/call without a name is Invalid Params: {resp}"
        );
    }

    /// A tool that RUNS but fails via `tools/call` is reported as an `isError`
    /// result (so the calling model can see it), not a JSON-RPC error.
    #[tokio::test]
    async fn test_tools_call_tool_execution_error_is_iserror_result() {
        let server = server_with_gauge().await;
        // Publishing to a widget that isn't registered fails inside the handler.
        let call = json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{
                "name":"publish_to_widget",
                "arguments":{"widget_name":"does_not_exist","params":{"level":0.5}}
            },
            "id":6
        });
        let resp = parse_response(&server.dispatch(&call.to_string(), &guest()).await);
        assert!(
            resp["error"].is_null(),
            "a tool execution error must NOT be a JSON-RPC error under tools/call: {resp}"
        );
        assert_eq!(
            resp["result"]["isError"],
            json!(true),
            "tool execution failure must be reported as isError: true: {resp}"
        );
        assert!(
            resp["result"]["content"][0]["text"].is_string(),
            "isError result still carries a text content block"
        );
    }

    /// Back-compat: the bare method==tool-name path returns the raw tool JSON as
    /// `result`, NEVER the tools/call content/isError wrapper.
    #[tokio::test]
    async fn test_bare_method_result_is_unwrapped_for_back_compat() {
        let server = server_with_gauge().await;
        let bare = json!({"jsonrpc":"2.0","method":"list_widgets","params":{},"id":7});
        let resp = parse_response(&server.dispatch(&bare.to_string(), &guest()).await);
        assert!(resp["error"].is_null());
        assert!(
            resp["result"]["widget_instances"].is_array(),
            "bare result is the raw tool JSON"
        );
        assert!(
            resp["result"]["content"].is_null() && resp["result"]["isError"].is_null(),
            "bare-method result must NOT be tools/call content-wrapped (back-compat): {resp}"
        );
    }

    /// MCP conformance (hud-btveq review, gemini + codex): INVALID ARGUMENTS to a
    /// KNOWN tool via `tools/call` are a JSON-RPC error, NOT an `isError: true`
    /// result. `isError` is reserved for a tool that RAN but failed; params that
    /// fail the handler's `parse_params` (here: `publish_to_widget` with the
    /// required `widget_name` missing) are a protocol error, so a spec client can
    /// distinguish a malformed call from a recoverable execution failure.
    #[tokio::test]
    async fn test_tools_call_invalid_arguments_is_json_rpc_error_not_iserror() {
        let server = server_with_gauge().await;
        // `publish_to_widget` is Guest-reachable (see the execution-error test
        // above), so the request passes the capability gate and reaches the
        // handler, where `parse_params` rejects the missing required fields.
        let call = json!({
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"name":"publish_to_widget","arguments":{}},
            "id":8
        });
        let resp = parse_response(&server.dispatch(&call.to_string(), &guest()).await);
        // parse_params maps any deserialization failure to InvalidParams (-32602).
        assert_eq!(
            resp["error"]["code"],
            json!(-32602),
            "invalid tool arguments via tools/call must be a JSON-RPC Invalid Params \
             error, not an isError result: {resp}"
        );
        assert!(
            resp["result"].is_null(),
            "an argument-validation failure must NOT be wrapped as an isError result: {resp}"
        );

        // The SAME invalid call on the bare-method path stays a JSON-RPC error
        // too (back-compat unchanged — this fork only affects tools/call shaping).
        let bare = json!({"jsonrpc":"2.0","method":"publish_to_widget","params":{},"id":9});
        let bare_resp = parse_response(&server.dispatch(&bare.to_string(), &guest()).await);
        assert_eq!(
            bare_resp["error"]["code"],
            json!(-32602),
            "bare publish_to_widget with missing args stays Invalid Params: {bare_resp}"
        );
    }

    #[tokio::test]
    async fn successful_mutation_notifies_render_work_but_read_only_call_does_not() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&calls);
        let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let server =
            test_server(SceneGraph::new(1920.0, 1080.0)).with_render_wake_notifier(notifier);

        let list = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"list_scene","params":{},"id":90}"#,
                &guest(),
            )
            .await;
        assert!(parse_response(&list)["error"].is_null());
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        let create = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"create_tab","params":{"name":"Wake"},"id":91}"#,
                &resident(),
            )
            .await;
        assert!(parse_response(&create)["error"].is_null());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn successful_composer_paste_wakes_main_ingress_but_not_render_work() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let ingress_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&ingress_calls);
        let ingress_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let render_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&render_calls);
        let render_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let (paste_tx, mut paste_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = test_server(SceneGraph::new(1920.0, 1080.0))
            .with_paste_inject_tx(paste_tx)
            .with_render_wake_notifier(render_notifier)
            .with_portal_ingress_wake_notifier(ingress_notifier);

        let response = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"inject_composer_paste","params":{"text":"queued paste"},"id":92}"#,
                &resident(),
            )
            .await;
        let response = parse_response(&response);
        assert!(response["error"].is_null());
        assert_eq!(response["result"]["injected"], true);
        assert_eq!(paste_rx.recv().await.as_deref(), Some("queued paste"));
        assert_eq!(
            ingress_calls.load(Ordering::Relaxed),
            1,
            "only a successful enqueue wakes the main-thread paste drain"
        );
        assert_eq!(
            render_calls.load(Ordering::Relaxed),
            0,
            "queued paste is not compositor work until the main-thread drain changes the composer"
        );
    }

    #[tokio::test]
    async fn clear_widget_wakes_only_when_it_removes_a_publication() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let wakes = Arc::new(AtomicU64::new(0));
        let callback_wakes = Arc::clone(&wakes);
        let render_wake = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_wakes.fetch_add(1, Ordering::AcqRel);
        });
        let server = server_with_gauge()
            .await
            .with_render_wake_notifier(render_wake);

        let no_op = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"clear_widget","params":{"widget_name":"gauge","namespace":"agent.nobody"},"id":94}"#,
                &guest(),
            )
            .await;
        let no_op = parse_response(&no_op);
        assert!(no_op["error"].is_null());
        assert_eq!(no_op["result"]["changed"], false);
        assert_eq!(
            wakes.load(Ordering::Acquire),
            0,
            "a successful no-op clear has no compositor-visible state to render"
        );

        let publish = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"publish_to_widget","params":{"widget_name":"gauge","namespace":"agent.owner","params":{"level":0.8}},"id":95}"#,
                &guest(),
            )
            .await;
        assert!(parse_response(&publish)["error"].is_null());
        let wakes_before_clear = wakes.load(Ordering::Acquire);

        let clear = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"clear_widget","params":{"widget_name":"gauge","namespace":"agent.owner"},"id":96}"#,
                &guest(),
            )
            .await;
        let clear = parse_response(&clear);
        assert!(clear["error"].is_null());
        assert_eq!(clear["result"]["changed"], true);
        assert_eq!(
            wakes.load(Ordering::Acquire),
            wakes_before_clear + 1,
            "removing an active widget publication produces exactly one render wake"
        );
    }

    #[tokio::test]
    async fn unwired_composer_paste_wakes_neither_main_ingress_nor_render_work() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let ingress_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&ingress_calls);
        let ingress_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let render_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&render_calls);
        let render_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let server = test_server(SceneGraph::new(1920.0, 1080.0))
            .with_render_wake_notifier(render_notifier)
            .with_portal_ingress_wake_notifier(ingress_notifier);

        let response = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"inject_composer_paste","params":{"text":"nowhere to queue"},"id":93}"#,
                &resident(),
            )
            .await;
        let response = parse_response(&response);
        assert!(response["error"].is_null());
        assert_eq!(response["result"]["injected"], false);
        assert_eq!(ingress_calls.load(Ordering::Relaxed), 0);
        assert_eq!(render_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn portal_enqueue_wakes_before_the_tool_awaits_its_reply() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let ingress_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&ingress_calls);
        let ingress_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let render_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&render_calls);
        let render_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = test_server(SceneGraph::new(1920.0, 1080.0))
            .with_portal_op_tx(tx)
            .with_render_wake_notifier(render_notifier)
            .with_portal_ingress_wake_notifier(ingress_notifier);

        let caller = resident();
        let dispatch = server.dispatch(
            r#"{"jsonrpc":"2.0","method":"portal_projection_attach","params":{"projection_id":"wake-before-await","display_name":"Wake"},"id":92}"#,
            &caller,
        );
        tokio::pin!(dispatch);
        let op = tokio::select! {
            op = rx.recv() => op.expect("portal op must be enqueued"),
            response = &mut dispatch => panic!("tool returned before authority reply: {response}"),
        };
        assert_eq!(
            ingress_calls.load(Ordering::Relaxed),
            1,
            "the enqueue must wake the main-thread authority owner"
        );
        assert_eq!(render_calls.load(Ordering::Relaxed), 0);
        match op {
            crate::portal_op::PortalOp::Attach { reply, .. } => reply
                .send(Ok("owner-token".to_string()))
                .expect("attach reply must send"),
            other => panic!("unexpected portal op: {other:?}"),
        }
        let response = dispatch.await;
        assert!(parse_response(&response)["error"].is_null());
        assert_eq!(
            render_calls.load(Ordering::Relaxed),
            0,
            "server dispatch must not add a pre-drain compositor wake"
        );
    }

    #[tokio::test]
    async fn empty_long_poll_wakes_only_main_ingress_and_never_render_work() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let ingress_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&ingress_calls);
        let ingress_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let render_calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&render_calls);
        let render_notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = test_server(SceneGraph::new(1920.0, 1080.0))
            .with_portal_op_tx(tx)
            .with_render_wake_notifier(render_notifier)
            .with_portal_ingress_wake_notifier(ingress_notifier);
        let responder = tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                match op {
                    crate::portal_op::PortalOp::GetPendingInput { reply, .. } => {
                        reply
                            .send(Ok(crate::portal_op::PendingInputBatch {
                                items: Vec::new(),
                                remaining_count: 0,
                                remaining_bytes: 0,
                            }))
                            .expect("empty poll reply must send");
                    }
                    other => panic!("unexpected portal op: {other:?}"),
                }
            }
        });

        let response = server
            .dispatch(
                r#"{"jsonrpc":"2.0","method":"portal_projection_get_pending_input","params":{"projection_id":"idle-poll","owner_token":"owner","wait_ms":320},"id":93}"#,
                &resident(),
            )
            .await;
        assert!(parse_response(&response)["error"].is_null());
        assert_eq!(
            render_calls.load(Ordering::Relaxed),
            0,
            "side-effect-free empty polls must never be classified as render work"
        );
        let main_wakes = ingress_calls.load(Ordering::Relaxed);
        assert!(
            (2..=4).contains(&main_wakes),
            "the bounded 320ms poll should issue only its expected ingress attempts, got {main_wakes}"
        );
        drop(server);
        responder.await.expect("responder task");
    }

    #[test]
    fn dropping_portal_ingress_closes_then_wakes_the_owner() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let calls = Arc::new(AtomicU64::new(0));
        let callback_calls = Arc::clone(&calls);
        let notifier = tze_hud_scene::render_wake::RenderWakeNotifier::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server = test_server(SceneGraph::new(1920.0, 1080.0))
            .with_portal_op_tx(tx)
            .with_portal_ingress_wake_notifier(notifier);

        drop(server);

        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
