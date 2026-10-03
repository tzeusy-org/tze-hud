//! # mcp
//!
//! MCP HTTP server lifecycle integration for the windowed runtime.
//!
//! This module wires [`tze_hud_mcp::McpServer`] into the runtime's
//! [`NetworkRuntime`] (Tokio multi-thread pool) and connects it to the shared
//! scene state produced at startup.
//!
//! ## Architecture note — scene coherence
//!
//! The MCP server and gRPC session server share a single canonical
//! [`Arc<Mutex<SceneGraph>>`].  Mutations applied over gRPC are immediately
//! visible to MCP queries and vice versa.  The `Arc` originates in
//! `windowed.rs` where `shared_scene` is created, stored in
//! [`SharedState::scene`], and also passed directly to
//! [`start_mcp_http_server`].
//!
//! ## Shutdown
//!
//! The HTTP task listens on a [`ShutdownToken`] receiver.  When the token is
//! triggered (e.g. by `WindowEvent::CloseRequested`), the task exits its
//! accept loop and returns, allowing the [`NetworkRuntime`]'s Tokio runtime
//! to drain and drop cleanly.  Dropping the [`tokio::runtime::Runtime`] waits
//! for all spawned tasks to complete before returning.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tze_hud_mcp::{McpConfig, McpServer};
use tze_hud_scene::config::SharedAgents;
use tze_hud_scene::graph::SceneGraph;

use crate::threads::ShutdownToken;

// ─── MCP lifecycle ────────────────────────────────────────────────────────────

/// Configuration for the runtime MCP HTTP server.
///
/// Analogous to [`crate::windowed::WindowedConfig`] but scoped to MCP.
/// Created internally by the windowed runtime from `WindowedConfig` fields.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Addresses to listen on, one listener each. The windowed runtime passes
    /// loopback plus the local Tailscale addresses (see [`crate::net_addrs`]).
    pub bind_addrs: Vec<SocketAddr>,

    /// When `Some(port)`, and none of `bind_addrs` is a Tailscale address,
    /// keep looking for one after startup and add a listener on `port` when it
    /// appears (Tailscale often starts after the HUD).
    pub late_tailnet_port: Option<u16>,

    /// Live credential → agent directory for MCP authentication and the
    /// `allow` gate, shared with gRPC. The bearer token must be a paired
    /// agent's PSK; the agent's namespace is its id.
    pub agents: SharedAgents,
}

/// Start the MCP HTTP server on the calling Tokio runtime.
///
/// Binds every listener in `config.bind_addrs` (all must bind), logs the
/// effective addresses, and spawns one accept loop per listener over a shared
/// server. Returns the join handle so the caller can await it during shutdown.
///
/// # Parameters
///
/// * `scene`           — shared scene graph for MCP tool dispatch.
/// * `config`          — MCP server configuration (bind addresses, agents).
/// * `shutdown`        — token that stops the accept loops when triggered.
/// * `portal_op_tx` — optional channel sender for portal projection operations
///   (hud-bq0gl.2).  When `Some`, the MCP server forwards portal surface
///   operations through this channel to the winit event-loop thread where the
///   `InProcessPortalDriver` lives.
///
/// # Returns
///
/// On success, returns `(join_handle, local_addrs)` where `local_addrs` are the
/// *actually bound* socket addresses (an ephemeral `:0` port is resolved to the
/// real one), in `bind_addrs` order. Callers use them for user-facing discovery
/// (e.g. the startup banner) so the report reflects listeners that are up.
///
/// # Errors
///
/// Returns an error if any `TcpListener::bind` fails (e.g., address in use).
pub async fn start_mcp_http_server(
    scene: Arc<Mutex<SceneGraph>>,
    config: McpServerConfig,
    shutdown: ShutdownToken,
    portal_op_tx: Option<tokio::sync::mpsc::UnboundedSender<tze_hud_mcp::portal_op::PortalOp>>,
) -> std::io::Result<(tokio::task::JoinHandle<()>, Vec<SocketAddr>)> {
    start_mcp_http_server_with_render_wake(
        scene,
        config,
        shutdown,
        portal_op_tx,
        tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        tze_hud_scene::render_wake::RenderWakeNotifier::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
}

pub async fn start_mcp_http_server_with_render_wake(
    scene: Arc<Mutex<SceneGraph>>,
    config: McpServerConfig,
    shutdown: ShutdownToken,
    portal_op_tx: Option<tokio::sync::mpsc::UnboundedSender<tze_hud_mcp::portal_op::PortalOp>>,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    safe_mode: Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<(tokio::task::JoinHandle<()>, Vec<SocketAddr>)> {
    // The first address is loopback and must bind; a Tailscale address that
    // fails to bind is skipped with a warning.
    let mut listeners = Vec::with_capacity(config.bind_addrs.len());
    for (i, addr) in config.bind_addrs.iter().enumerate() {
        match TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) if i == 0 => return Err(e),
            Err(e) => tracing::warn!(addr = %addr, error = %e, "MCP HTTP: failed to bind address"),
        }
    }
    let local_addrs = listeners
        .iter()
        .map(TcpListener::local_addr)
        .collect::<std::io::Result<Vec<_>>>()?;

    for addr in &local_addrs {
        tracing::info!(addr = %addr, "MCP HTTP listener bound");
    }

    let mut server_builder = McpServer::with_shared_scene(scene)
        .with_config(McpConfig::with_agents(config.agents.clone()))
        .with_render_wake_notifier(render_wake)
        .with_portal_ingress_wake_notifier(portal_ingress_wake)
        .with_safe_mode(safe_mode);
    if let Some(tx) = portal_op_tx {
        server_builder = server_builder.with_portal_op_tx(tx);
    }
    let server = Arc::new(server_builder);

    let mut loops = Vec::with_capacity(listeners.len());
    for (listener, addr) in listeners.into_iter().zip(local_addrs.iter().copied()) {
        loops.push(tokio::spawn(run_accept_loop(
            listener,
            Arc::clone(&server),
            shutdown.clone(),
            addr,
        )));
    }

    if let Some(port) = config.late_tailnet_port
        && !local_addrs
            .iter()
            .any(|a| !crate::net_addrs::tailnet_addrs(&[a.ip()]).is_empty())
    {
        let (server, shutdown, shutdown_w) =
            (Arc::clone(&server), shutdown.clone(), shutdown.clone());
        tokio::spawn(async move {
            let watcher = crate::net_addrs::watch_for_tailnet(|ip| {
                let addr = SocketAddr::new(ip, port);
                match std::net::TcpListener::bind(addr).and_then(|l| {
                    l.set_nonblocking(true)?;
                    TcpListener::from_std(l)
                }) {
                    Ok(listener) => {
                        tracing::info!(addr = %addr, "MCP HTTP listener bound (Tailscale address appeared)");
                        tokio::spawn(run_accept_loop(
                            listener,
                            Arc::clone(&server),
                            shutdown.clone(),
                            addr,
                        ));
                        true
                    }
                    Err(e) => {
                        tracing::warn!(addr = %addr, error = %e, "MCP HTTP: failed to bind Tailscale address");
                        false
                    }
                }
            });
            tokio::select! {
                _ = watcher => {}
                _ = wait_shutdown(&shutdown_w) => {}
            }
        });
    }

    let handle = tokio::spawn(async move {
        for l in loops {
            let _ = l.await;
        }
    });

    Ok((handle, local_addrs))
}

/// Resolves once `shutdown` has been triggered (checks the flag too, so a
/// trigger that predates the subscription is not missed).
async fn wait_shutdown(shutdown: &ShutdownToken) {
    let mut rx = shutdown.subscribe();
    while !shutdown.is_triggered() {
        tokio::select! {
            _ = rx.recv() => return,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

/// Internal accept loop — runs until the shutdown token is triggered or the
/// listener returns an unrecoverable error.
async fn run_accept_loop(
    listener: TcpListener,
    server: Arc<McpServer>,
    shutdown: ShutdownToken,
    local_addr: SocketAddr,
) {
    let mut shutdown_rx = shutdown.subscribe();

    tracing::info!(addr = %local_addr, "MCP HTTP accept loop started");

    loop {
        tokio::select! {
            // Shutdown signal received — stop accepting new connections.
            _ = shutdown_rx.recv() => {
                tracing::info!(addr = %local_addr, "MCP HTTP server: shutdown signal received, stopping accept loop");
                break;
            }

            // Also check the atomic flag for the case where the broadcast was
            // sent before we subscribed (racing startup).
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)),
                if shutdown.is_triggered() => {
                tracing::info!(addr = %local_addr, "MCP HTTP server: shutdown flag detected, stopping accept loop");
                break;
            }

            // Accept a new connection.
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, peer)) => {
                        let srv = Arc::clone(&server);
                        tokio::spawn(handle_connection(stream, peer, srv));
                    }
                    Err(e) => {
                        tracing::error!(
                            addr = %local_addr,
                            error = %e,
                            "MCP HTTP: accept error — stopping server"
                        );
                        break;
                    }
                }
            }
        }
    }

    tracing::info!(addr = %local_addr, "MCP HTTP accept loop exited");
}

/// Handle a single HTTP connection: read request, route, write response.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    server: Arc<McpServer>,
) {
    use crate::http::{self, ReadError, Response, Route};
    use tokio::io::AsyncWriteExt;

    let response = match http::read_request(&mut stream, http::READ_TIMEOUT).await {
        Ok(req) => match http::route(&req.method, &req.path) {
            Route::Mcp => {
                // Config-gated resident-principal grant (hud-nu65o); the PSK
                // check still happens independently inside `dispatch`.
                let ctx = server.caller_context(req.bearer);
                let body = std::str::from_utf8(&req.body).unwrap_or("");
                Response::json(server.dispatch(body, &ctx).await)
            }
            Route::Respond(resp) => resp,
        },
        Err(ReadError::Malformed) => Response::bad_request(),
        Err(e) => {
            tracing::debug!(peer = %peer, error = ?e, "MCP: dropping connection");
            return;
        }
    };

    if let Err(e) = stream.write_all(&response.to_bytes()).await {
        tracing::debug!(peer = %peer, error = %e, "MCP: write error");
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tze_hud_scene::graph::SceneGraph;

    fn make_scene() -> Arc<Mutex<SceneGraph>> {
        Arc::new(Mutex::new(SceneGraph::new(1920.0, 1080.0)))
    }

    fn make_config(port: u16, psk: &str) -> McpServerConfig {
        McpServerConfig {
            bind_addrs: vec![format!("127.0.0.1:{port}").parse().unwrap()],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(psk).shared(),
        }
    }

    /// Helper: send an HTTP POST to the given addr with a JSON body.
    async fn http_post(addr: SocketAddr, body: &str, bearer: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let mut conn = TcpStream::connect(addr).await.expect("connect");

        let auth_header = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();

        let request = format!(
            "POST / HTTP/1.0\r\nContent-Type: application/json\r\n{auth_header}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );

        conn.write_all(request.as_bytes()).await.expect("write");

        let mut resp = Vec::new();
        conn.read_to_end(&mut resp).await.expect("read");
        String::from_utf8_lossy(&resp).into_owned()
    }

    /// Raw request helper for routing tests.
    async fn http_raw(addr: SocketAddr, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut conn = tokio::net::TcpStream::connect(addr).await.expect("connect");
        conn.write_all(request.as_bytes()).await.expect("write");
        let mut resp = Vec::new();
        conn.read_to_end(&mut resp).await.expect("read");
        String::from_utf8_lossy(&resp).into_owned()
    }

    #[tokio::test]
    async fn mcp_http_routes_by_method_and_path() {
        let shutdown = ShutdownToken::new();
        let (handle, addrs) =
            start_mcp_http_server(make_scene(), make_config(0, "k"), shutdown.clone(), None)
                .await
                .expect("start");
        let addr = addrs[0];
        let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let post = |path: &str| {
            format!(
                "POST {path} HTTP/1.1\r\nAuthorization: Bearer k\r\nContent-Length: {}\r\n\r\n{list}",
                list.len()
            )
        };
        for path in ["/", "/mcp"] {
            let r = http_raw(addr, &post(path)).await;
            assert!(r.starts_with("HTTP/1.1 200 "), "{path}: {r}");
            assert!(r.contains("hud_publish"), "{path}: {r}");
        }
        let r = http_raw(addr, "GET / HTTP/1.1\r\n\r\n").await;
        assert!(
            r.starts_with("HTTP/1.1 405 ") && r.contains("Allow: POST"),
            "{r}"
        );
        assert!(!r.contains("jsonrpc"));
        let r = http_raw(addr, &post("/nope")).await;
        assert!(r.starts_with("HTTP/1.1 404 "), "{r}");
        assert!(!r.contains("jsonrpc"));
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_server_binds_and_responds() {
        let scene = make_scene();
        let config = make_config(0, "test-psk");
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind should succeed");

        // Give the task a moment to start its accept loop.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Signal shutdown.
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task panicked");
    }

    #[tokio::test]
    async fn mcp_http_tools_list_authenticated() {
        use std::net::TcpListener as StdListener;

        // Bind to find a free port, then drop to release it for the server.
        let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = std_listener.local_addr().unwrap();
        drop(std_listener);

        let scene = make_scene();
        let config = McpServerConfig {
            bind_addrs: vec![addr],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("test-key").shared(),
        };
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");

        // Give the task time to enter accept loop.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let body = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}"#;
        let resp = http_post(addr, body, Some("test-key")).await;

        // Response should be HTTP 200 with a JSON-RPC result.
        assert!(
            resp.contains("HTTP/1.1 200"),
            "expected HTTP 200, got: {resp}"
        );
        assert!(
            resp.contains("\"result\""),
            "expected result field, got: {resp}"
        );

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    /// One server answers on every listener it is given (loopback plus, in
    /// production, each Tailscale address). 127.0.0.2 stands in for a second
    /// interface address without needing Tailscale.
    #[tokio::test]
    async fn mcp_http_tools_list_on_every_listener() {
        let config = McpServerConfig {
            bind_addrs: vec![
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.2:0".parse().unwrap(),
            ],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("test-key").shared(),
        };
        let shutdown = ShutdownToken::new();
        let (handle, addrs) = start_mcp_http_server(make_scene(), config, shutdown.clone(), None)
            .await
            .expect("bind");
        assert_eq!(addrs.len(), 2);

        let body = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}"#;
        for addr in addrs {
            let resp = http_post(addr, body, Some("test-key")).await;
            assert!(
                resp.contains("HTTP/1.1 200") && resp.contains("\"result\""),
                "tools/list on {addr} failed: {resp}"
            );
        }

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    /// Only the loopback listener is fatal: a Tailscale address that cannot
    /// bind (192.0.2.1 is not local) is skipped, a loopback failure is not.
    #[tokio::test]
    async fn mcp_http_only_loopback_bind_failure_is_fatal() {
        let agents = tze_hud_scene::config::AgentDirectory::unrestricted("k").shared();
        let cfg = |addrs: [&str; 2]| McpServerConfig {
            bind_addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
            late_tailnet_port: None,
            agents: agents.clone(),
        };
        let shutdown = ShutdownToken::new();
        let (handle, addrs) = start_mcp_http_server(
            make_scene(),
            cfg(["127.0.0.1:0", "192.0.2.1:0"]),
            shutdown.clone(),
            None,
        )
        .await
        .expect("a failing non-loopback bind must not fail startup");
        assert_eq!(addrs.len(), 1);
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");

        assert!(
            start_mcp_http_server(
                make_scene(),
                cfg(["192.0.2.1:0", "127.0.0.1:0"]),
                ShutdownToken::new(),
                None,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn mcp_http_unauthenticated_returns_error() {
        use std::net::TcpListener as StdListener;

        let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = std_listener.local_addr().unwrap();
        drop(std_listener);

        let scene = make_scene();
        let config = McpServerConfig {
            bind_addrs: vec![addr],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("real-key").shared(),
        };
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Send request with NO bearer token — should get an error response.
        let body = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":2}"#;
        let resp = http_post(addr, body, None).await;

        // HTTP status is always 200 (JSON-RPC over HTTP carries errors in body).
        assert!(resp.contains("HTTP/1.1 200"), "expected HTTP 200");
        // JSON-RPC body must contain "error".
        assert!(resp.contains("\"error\""), "expected error, got: {resp}");

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_http_wrong_psk_returns_error() {
        use std::net::TcpListener as StdListener;

        let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = std_listener.local_addr().unwrap();
        drop(std_listener);

        let scene = make_scene();
        let config = McpServerConfig {
            bind_addrs: vec![addr],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("correct-key").shared(),
        };
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Send with wrong bearer token.
        let body = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":3}"#;
        let resp = http_post(addr, body, Some("wrong-key")).await;

        assert!(resp.contains("HTTP/1.1 200"));
        assert!(
            resp.contains("\"error\""),
            "expected auth error, got: {resp}"
        );

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_http_hud_publish_authenticated() {
        use std::net::TcpListener as StdListener;
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, RenderingPolicy, ZoneDefinition,
            ZoneMediaType,
        };

        let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = std_listener.local_addr().unwrap();
        drop(std_listener);

        // Seed the scene with a zone so hud_publish has somewhere to write.
        let scene = make_scene();
        {
            let mut s = scene.lock().await;
            s.zone_registry.zones.insert(
                "test-zone".to_string(),
                ZoneDefinition {
                    id: SceneId::new(),
                    name: "test-zone".to_string(),
                    description: "Test Zone".to_string(),
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
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Content,
                },
            );
        }

        let config = McpServerConfig {
            bind_addrs: vec![addr],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("test-key").shared(),
        };
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let body = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"zone:test-zone","content":"hello from MCP"}},"id":4}"#;
        let resp = http_post(addr, body, Some("test-key")).await;

        assert!(resp.contains("HTTP/1.1 200"));
        assert!(
            resp.contains(r#"{\"expires_in_ms\":60000,\"ok\":true}"#),
            "expected a successful publish, got: {resp}"
        );

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_server_shuts_down_cleanly() {
        use std::net::TcpListener as StdListener;

        let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = std_listener.local_addr().unwrap();
        drop(std_listener);

        let scene = make_scene();
        let config = McpServerConfig {
            bind_addrs: vec![addr],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted("key").shared(),
        };
        let shutdown = ShutdownToken::new();

        let (handle, _mcp_addr) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Trigger shutdown and verify the task exits within a reasonable time.
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "MCP server task did not exit within 2s after shutdown"
        );
    }
}
