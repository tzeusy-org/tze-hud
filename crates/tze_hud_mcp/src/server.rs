//! MCP JSON-RPC server.
//!
//! Standard MCP over JSON-RPC 2.0: `initialize`, `notifications/initialized`,
//! `tools/list`, and `tools/call` for the five `hud_*` tools in
//! [`crate::tools`]. There is no other method.
//!
//! ## Authentication
//!
//! Every request carries a PSK as the HTTP `Authorization: Bearer` value. The
//! PSK resolves to an agent identity (`[agents.<id>]`): the agent id is the
//! namespace, and its `allow` list is the whole permission model. With no PSK
//! configured every request is rejected. The PSK is never echoed.
//!
//! ## Results
//!
//! A tool result is one text content block holding compact JSON. A failed
//! call is a tool result with `isError: true` whose text is
//! `{"code":"...","hint":"..."}` (see [`crate::error`]). Only unusable
//! requests (bad JSON, unknown method or tool, bad auth) are JSON-RPC errors.

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use tze_hud_scene::config::AgentDirectory;
use tze_hud_scene::graph::SceneGraph;
use tze_hud_scene::render_wake::RenderWakeNotifier;

use crate::{
    error::{JsonRpcError, McpError},
    portal_op::PortalOp,
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
    /// allow-list permissions. With no PSKs, every call is rejected.
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

    /// Load the runtime PSK from `MCP_TEST_PSK` (test harnesses). Unset means
    /// every call is rejected.
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

pub struct McpServer {
    scene: Arc<Mutex<SceneGraph>>,
    render_wake: RenderWakeNotifier,
    portal_ingress_wake: RenderWakeNotifier,
    config: McpConfig,
    /// Channel to the portal authority on the winit thread. `None` means
    /// portal surfaces answer `UNAVAILABLE`.
    portal_op_tx: Option<tokio::sync::mpsc::UnboundedSender<PortalOp>>,
    /// Per-agent leases, portal owner tokens, and unacked input.
    state: McpState,
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
            portal_op_tx: None,
            state: McpState::default(),
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

    /// Attach the portal-operation channel to the winit thread.
    pub fn with_portal_op_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<PortalOp>) -> Self {
        self.portal_op_tx = Some(tx);
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

        let identity = if self.config.has_credentials() {
            ctx.bearer_token
                .as_deref()
                .and_then(|key| self.config.agents.resolve(key, "").ok())
        } else {
            None
        };
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
                    portal_op_tx: self.portal_op_tx.as_ref(),
                    portal_wake: &self.portal_ingress_wake,
                    state: &self.state,
                    agent: &identity,
                };
                let result = match name {
                    "hud_surfaces" => tools::hud_surfaces(&tool_ctx).await,
                    "hud_publish" => tools::hud_publish(&tool_ctx, args).await,
                    "hud_hold" => tools::hud_hold(&tool_ctx, args).await,
                    "hud_clear" => tools::hud_clear(&tool_ctx, args).await,
                    _ => tools::hud_input(&tool_ctx, args).await,
                };
                let body = match &result {
                    Ok(value) => {
                        if matches!(name, "hud_publish" | "hud_hold" | "hud_clear") {
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
                        tracing::error!(peer = %peer, error = %e, "MCP: read error");
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

                // A notification gets no JSON-RPC response body.
                let http_response = if response_body.is_empty() {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n".to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    )
                };

                if let Err(e) = stream.write_all(http_response.as_bytes()).await {
                    tracing::error!(peer = %peer, error = %e, "MCP: write error");
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
