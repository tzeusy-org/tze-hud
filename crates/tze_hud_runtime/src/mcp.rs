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

use crate::operator::status::StatusSource;
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

    /// Present-frame counters reported by `/admin/status` (`frames_presented`
    /// is null without them).
    pub presents: Option<Arc<crate::idle_efficiency::IdleEfficiencyCounters>>,

    /// Compositor capture channel behind `/admin/screenshot`; the endpoint
    /// answers 503 without one (no display to read from).
    pub capture: Option<crate::operator::screenshot::CaptureEndpoint>,

    /// Relaunch behind `POST /admin/restart`; the endpoint answers 503
    /// without one.
    pub restart: Option<crate::operator::handoff::RestartHandle>,

    /// Set on a handed-over instance: listeners are bound only once the gate
    /// opens (the old instance still holds the ports), retrying for
    /// [`crate::operator::handoff::BIND_RETRY`].
    pub bind_gate: Option<crate::operator::handoff::BindGate>,
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
    // fails to bind is skipped with a warning. A handed-over instance binds
    // later, from the accept task (see `bind_gate`).
    let mut listeners = Vec::with_capacity(config.bind_addrs.len());
    let mut local_addrs = Vec::new();
    if config.bind_gate.is_none() {
        for (i, addr) in config.bind_addrs.iter().enumerate() {
            match TcpListener::bind(addr).await {
                Ok(l) => listeners.push(l),
                Err(e) if i == 0 => return Err(e),
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %e, "MCP HTTP: failed to bind address")
                }
            }
        }
        local_addrs = listeners
            .iter()
            .map(TcpListener::local_addr)
            .collect::<std::io::Result<Vec<_>>>()?;
        for addr in &local_addrs {
            tracing::info!(addr = %addr, "MCP HTTP listener bound");
        }
    }
    // What the startup banner and the late-Tailscale check should assume.
    let planned_addrs = if config.bind_gate.is_some() {
        config.bind_addrs.clone()
    } else {
        local_addrs.clone()
    };

    crate::operator::status::process_start();
    let admin = Arc::new(StatusSource {
        agents: config.agents.clone(),
        // In deferred mode this fills in as the listeners bind.
        binds: Arc::new(std::sync::Mutex::new(local_addrs.clone())),
        safe_mode: Arc::clone(&safe_mode),
        presents: config.presents.clone(),
        capture: config.capture.clone(),
        restart: config.restart.clone(),
        log_path: crate::operator::logs::log_path(),
    });

    let mut server_builder = McpServer::with_shared_scene(scene)
        .with_config(McpConfig::with_agents(config.agents.clone()))
        .with_render_wake_notifier(render_wake)
        .with_portal_ingress_wake_notifier(portal_ingress_wake)
        .with_safe_mode(safe_mode);
    if let Some(tx) = portal_op_tx {
        server_builder = server_builder.with_portal_op_tx(tx);
    }
    let server = Arc::new(server_builder);

    let mut loops = Vec::with_capacity(listeners.len() + 1);
    if let Some(gate) = config.bind_gate.clone() {
        loops.push(tokio::spawn(run_deferred_listeners(
            gate,
            config.bind_addrs.clone(),
            Arc::clone(&server),
            Arc::clone(&admin),
            shutdown.clone(),
        )));
    }
    for (listener, addr) in listeners.into_iter().zip(local_addrs.iter().copied()) {
        loops.push(tokio::spawn(run_accept_loop(
            listener,
            Arc::clone(&server),
            Arc::clone(&admin),
            shutdown.clone(),
            addr,
        )));
    }

    if let Some(port) = config.late_tailnet_port
        && !planned_addrs
            .iter()
            .any(|a| !crate::net_addrs::tailnet_addrs(&[a.ip()]).is_empty())
    {
        let (server, admin, shutdown, shutdown_w) = (
            Arc::clone(&server),
            Arc::clone(&admin),
            shutdown.clone(),
            shutdown.clone(),
        );
        tokio::spawn(async move {
            let watcher = crate::net_addrs::watch_for_tailnet(|ip| {
                let addr = SocketAddr::new(ip, port);
                match std::net::TcpListener::bind(addr).and_then(|l| {
                    l.set_nonblocking(true)?;
                    TcpListener::from_std(l)
                }) {
                    Ok(listener) => {
                        tracing::info!(addr = %addr, "MCP HTTP listener bound (Tailscale address appeared)");
                        admin
                            .binds
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(addr);
                        tokio::spawn(run_accept_loop(
                            listener,
                            Arc::clone(&server),
                            Arc::clone(&admin),
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

    Ok((handle, planned_addrs))
}

/// A handed-over instance: wait for the takeover, bind each listener (the old
/// instance may still be releasing the port), then serve them. The loopback
/// listener must bind or the gate fails and this instance shuts down; a
/// Tailscale address that never binds is skipped.
async fn run_deferred_listeners(
    gate: crate::operator::handoff::BindGate,
    addrs: Vec<SocketAddr>,
    server: Arc<McpServer>,
    admin: Arc<StatusSource>,
    shutdown: ShutdownToken,
) {
    use crate::operator::handoff::{BIND_RETRY, retry_bind};
    if !gate.wait().await {
        return;
    }
    let mut loops = Vec::new();
    for (i, addr) in addrs.iter().enumerate() {
        let bound = retry_bind(BIND_RETRY, || {
            let l = std::net::TcpListener::bind(addr)?;
            l.set_nonblocking(true)?;
            TcpListener::from_std(l)
        })
        .await;
        match bound {
            Ok(listener) => {
                let local = listener.local_addr().unwrap_or(*addr);
                tracing::info!(addr = %local, "MCP HTTP listener bound (after handoff)");
                admin
                    .binds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(local);
                loops.push(tokio::spawn(run_accept_loop(
                    listener,
                    Arc::clone(&server),
                    Arc::clone(&admin),
                    shutdown.clone(),
                    local,
                )));
            }
            Err(e) if i == 0 => {
                gate.fail(&format!("MCP HTTP: could not bind {addr}: {e}"));
                return;
            }
            Err(e) => tracing::warn!(addr = %addr, error = %e, "MCP HTTP: failed to bind address"),
        }
    }
    for l in loops {
        let _ = l.await;
    }
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
    admin: Arc<StatusSource>,
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
                        let admin = Arc::clone(&admin);
                        tokio::spawn(handle_connection(stream, peer, srv, admin));
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
    admin: Arc<StatusSource>,
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
            Route::Admin(which) => handle_admin(which, &req, &admin).await,
            Route::Respond(resp) => resp,
        },
        Err(ReadError::Malformed) => Response::bad_request(),
        Err(ReadError::NotImplemented) => Response::not_implemented(),
        Err(e) => {
            tracing::debug!(peer = %peer, error = ?e, "MCP: dropping connection");
            return;
        }
    };

    if let Err(e) = stream.write_all(&response.to_bytes()).await {
        tracing::debug!(peer = %peer, error = %e, "MCP: write error");
    }
}

/// Serve an `/admin/*` request. Authentication and the `admin` check come
/// before any admin data is read.
async fn handle_admin(
    which: crate::http::AdminRoute,
    req: &crate::http::Request,
    admin: &StatusSource,
) -> crate::http::Response {
    use crate::http::{
        AdminRoute, OperatorCode, OperatorError, Response, admin_guard, query_param,
    };
    use crate::operator::handoff::RestartError;
    let unavailable = |why: &str| {
        Response::operator_error(503, &OperatorError::new(OperatorCode::Unavailable, why))
    };
    let identity = req
        .bearer
        .as_deref()
        .and_then(|token| admin.agents.load().resolve(token, "").ok());
    if let Err(denied) = admin_guard(identity.as_ref()) {
        return denied;
    }
    match which {
        AdminRoute::Status => Response::json(admin.render().await.to_string()),
        AdminRoute::Screenshot => match &admin.capture {
            None => crate::operator::screenshot::ScreenshotError::Unavailable(
                "this runtime has no display to capture",
            )
            .response(),
            Some(capture) => match capture.capture_png().await {
                Ok(png) => Response::png(png),
                Err(e) => e.response(),
            },
        },
        AdminRoute::Restart => match &admin.restart {
            None => unavailable("this runtime cannot restart itself"),
            Some(restart) => match restart.request() {
                Ok(()) => Response {
                    status: 202,
                    ..Response::json(r#"{"restarting":true}"#)
                },
                Err(RestartError::Busy) => Response::operator_error(
                    429,
                    &OperatorError::new(OperatorCode::Busy, "a restart is already in progress"),
                ),
                Err(RestartError::Unavailable) => unavailable("could not start the restart"),
            },
        },
        AdminRoute::Logs => {
            let tail = match query_param(&req.query, "tail") {
                None => 100,
                Some(v) => match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return Response::bad_request(),
                },
            };
            Response::text(crate::operator::logs::tail(&admin.log_path, tail))
        }
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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

    fn admin_source(log_path: std::path::PathBuf) -> StatusSource {
        let mut dir = tze_hud_scene::config::AgentDirectory::default();
        let perms = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        dir.insert(
            "root",
            tze_hud_scene::config::hash_psk("root-psk"),
            perms(&["*", "operator_admin"]),
        );
        dir.insert(
            "star",
            tze_hud_scene::config::hash_psk("star-psk"),
            perms(&["*"]),
        );
        StatusSource {
            agents: dir.shared(),
            binds: Arc::new(std::sync::Mutex::new(vec![
                "127.0.0.1:9090".parse().unwrap(),
            ])),
            safe_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            presents: None,
            capture: None,
            restart: None,
            log_path,
        }
    }

    fn admin_get(path_and_query: &str, bearer: Option<&str>) -> crate::http::Request {
        let (path, query) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        crate::http::Request {
            method: "GET".into(),
            path: path.into(),
            query: query.into(),
            bearer: bearer.map(str::to_owned),
            body: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn admin_requires_the_admin_entry_not_just_star() {
        use crate::http::AdminRoute::{Logs, Status};
        let src = admin_source(std::env::temp_dir().join("tze_hud_no_such.log"));
        for (bearer, status) in [
            (None, 401),
            (Some("wrong"), 401),
            (Some("star-psk"), 403),
            (Some("root-psk"), 200),
        ] {
            for which in [Status, Logs] {
                let req = admin_get("/admin/x", bearer);
                let r = handle_admin(which, &req, &src).await;
                assert_eq!(r.status, status, "{bearer:?} {which:?}");
                if status == 403 {
                    assert!(String::from_utf8_lossy(&r.body).contains("NOT_ADMIN"));
                }
            }
        }
    }

    #[tokio::test]
    async fn admin_screenshot_is_guarded_then_served_as_png() {
        use crate::http::AdminRoute::Screenshot;
        let mut src = admin_source(std::env::temp_dir().join("tze_hud_no_such.log"));
        let (endpoint, inbox) = crate::operator::screenshot::capture_channel(|| {});
        std::thread::spawn(move || {
            loop {
                if let Some(req) = inbox.next_live() {
                    let _ = req.reply.send(Ok(tze_hud_compositor::CapturedFrame {
                        width: 1,
                        height: 1,
                        rgba: vec![1, 2, 3, 4],
                    }));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        src.capture = Some(endpoint);
        async fn get(src: &StatusSource, bearer: Option<&str>) -> crate::http::Response {
            let req = admin_get("/admin/screenshot", bearer);
            handle_admin(Screenshot, &req, src).await
        }
        // The guard runs before any capture is requested.
        assert_eq!(get(&src, None).await.status, 401);
        assert_eq!(get(&src, Some("star-psk")).await.status, 403);
        let ok = get(&src, Some("root-psk")).await;
        assert_eq!((ok.status, ok.content_type), (200, "image/png"));
        assert_eq!(&ok.body[..4], b"\x89PNG");
        // No capture endpoint (headless runtime): 503, not a hang.
        src.capture = None;
        assert_eq!(get(&src, Some("root-psk")).await.status, 503);
    }

    #[tokio::test]
    async fn admin_restart_is_guarded_single_flight_and_ignores_the_body() {
        use crate::http::AdminRoute::Restart;
        use crate::operator::handoff::{ChildProc, RestartHandle};
        struct Silent;
        impl ChildProc for Silent {
            fn try_wait(&mut self) -> std::io::Result<Option<String>> {
                Ok(None)
            }
            fn kill(&mut self) {}
        }
        let mut src = admin_source(std::env::temp_dir().join("tze_hud_no_such.log"));
        let post = |bearer: Option<&str>| {
            let mut req = admin_get("/admin/restart", bearer);
            req.method = "POST".into();
            // Nothing in a request can steer the relaunch.
            req.body = br#"{"args":["--evil"],"exe":"x"}"#.to_vec();
            req
        };
        // No relaunch configured (headless): 503, after the guard.
        assert_eq!(handle_admin(Restart, &post(None), &src).await.status, 401);
        assert_eq!(
            handle_admin(Restart, &post(Some("star-psk")), &src)
                .await
                .status,
            403
        );
        assert_eq!(
            handle_admin(Restart, &post(Some("root-psk")), &src)
                .await
                .status,
            503
        );

        let spawned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = Arc::clone(&spawned);
        src.restart = Some(RestartHandle::with_spawner(
            Box::new(move |_| {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(Silent))
            }),
            Arc::new(|| {}),
            std::time::Duration::from_millis(300),
        ));
        let first = handle_admin(Restart, &post(Some("root-psk")), &src).await;
        assert_eq!(first.status, 202);
        assert_eq!(first.body, br#"{"restarting":true}"#);
        let second = handle_admin(Restart, &post(Some("root-psk")), &src).await;
        assert_eq!(second.status, 429);
        assert!(String::from_utf8_lossy(&second.body).contains("BUSY"));
        // Exactly one relaunch was attempted (the restart runs on its own thread).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while spawned.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "no relaunch attempted"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(spawned.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn admin_status_has_every_key_and_no_secrets() {
        let src = admin_source(std::env::temp_dir().join("tze_hud_no_such.log"));
        let r = handle_admin(
            crate::http::AdminRoute::Status,
            &admin_get("/admin/status", Some("root-psk")),
            &src,
        )
        .await;
        let text = String::from_utf8(r.body).unwrap();
        assert!(!text.contains("psk") || text.contains("\"id\""), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for key in [
            "version",
            "sha",
            "channel",
            "pid",
            "uptime_s",
            "binds",
            "agents",
            "safe_mode",
            "frames_presented",
            "cpu_pct_2s",
            "cpu_pct_avg",
            "last_update",
            "last_restart",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {text}");
        }
        assert_eq!(
            v["agents"],
            serde_json::json!([{"id":"root","admin":true},{"id":"star","admin":false}])
        );
        assert_eq!(v["binds"], serde_json::json!(["127.0.0.1:9090"]));
        if cfg!(target_os = "linux") {
            assert!(
                v["cpu_pct_2s"].is_number() && v["cpu_pct_avg"].is_number(),
                "{text}"
            );
        }
    }

    #[tokio::test]
    async fn admin_logs_returns_the_requested_tail() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("tze_hud_admin_logs_{}.log", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..20 {
            writeln!(f, "line {i}").unwrap();
        }
        let src = admin_source(path.clone());
        let get = |q: &'static str| {
            let src = &src;
            async move {
                handle_admin(
                    crate::http::AdminRoute::Logs,
                    &admin_get(q, Some("root-psk")),
                    src,
                )
                .await
            }
        };
        let r = get("/admin/logs?tail=5").await;
        assert_eq!(r.content_type, "text/plain; charset=utf-8");
        assert_eq!(String::from_utf8(r.body).unwrap().lines().count(), 5);
        assert_eq!(get("/admin/logs?tail=abc").await.status, 400);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn admin_routes_are_served_over_the_socket() {
        let shutdown = ShutdownToken::new();
        let (handle, addrs) =
            start_mcp_http_server(make_scene(), make_config(0, "k"), shutdown.clone(), None)
                .await
                .expect("start");
        // The dev PSK resolves to an unrestricted `*` identity: no admin.
        let r = http_raw(
            addrs[0],
            "GET /admin/status HTTP/1.1\r\nAuthorization: Bearer k\r\n\r\n",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 403 ") && r.contains("NOT_ADMIN"),
            "{r}"
        );
        let r = http_raw(addrs[0], "GET /admin/logs HTTP/1.1\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 401 "), "{r}");
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_http_rejects_ambiguous_requests_before_dispatch() {
        let shutdown = ShutdownToken::new();
        let (handle, addrs) =
            start_mcp_http_server(make_scene(), make_config(0, "k"), shutdown.clone(), None)
                .await
                .expect("start");
        let addr = addrs[0];
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let n = body.len();
        let auth = "Authorization: Bearer k\r\n";
        let cases = [
            (
                "400",
                format!("POST / HTTP/1.1\r\n{auth}Content-Length: x\r\n\r\n{body}"),
            ),
            (
                "400",
                format!(
                    "POST / HTTP/1.1\r\n{auth}Content-Length: {n}\r\nContent-Length: {n}\r\n\r\n{body}"
                ),
            ),
            (
                "400",
                format!(
                    "POST / HTTP/1.1\r\n{auth}Authorization: Bearer k\r\nContent-Length: {n}\r\n\r\n{body}"
                ),
            ),
            (
                "400",
                format!("POST / HTTP/1.1 junk\r\n{auth}Content-Length: {n}\r\n\r\n{body}"),
            ),
            (
                "501",
                format!("POST / HTTP/1.1\r\n{auth}Transfer-Encoding: chunked\r\n\r\n{body}"),
            ),
        ];
        for (status, req) in cases {
            let r = http_raw(addr, &req).await;
            assert!(r.starts_with(&format!("HTTP/1.1 {status} ")), "{req}: {r}");
            assert!(!r.contains("jsonrpc"), "{r}");
        }
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
            presents: None,
            capture: None,
            restart: None,
            bind_gate: None,
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
