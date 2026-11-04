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
use tracing::Instrument;
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
    /// Implicit widget duration from the same resolved startup theme as the compositor.
    pub widget_transition_ms: u32,
    /// Same live omission default as the production gRPC service.
    pub publication_defaults: tze_hud_scene::PublicationDefaults,
    /// Addresses to listen on, one listener each. The windowed runtime passes
    /// loopback plus the local Tailscale addresses (see [`crate::net_addrs`]).
    pub bind_addrs: Vec<SocketAddr>,

    /// When `Some(port)`, follow local address-change notifications and add
    /// missing Tailscale listeners on `port`, retrying failed binds only.
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

    /// Self-update behind `POST /admin/update`; the endpoint answers 503
    /// without one.
    pub update: Option<crate::operator::update::UpdateHandle>,

    /// Set on a handed-over instance: listeners are bound only once the gate
    /// opens (the old instance still holds the ports), retrying for
    /// [`crate::operator::handoff::BIND_RETRY`].
    pub bind_gate: Option<crate::operator::handoff::BindGate>,

    /// First-run pairing behind `POST /pair`; the route answers
    /// `PAIRING_CLOSED` without one.
    pub pairing: Option<Arc<crate::pairing::Pairing>>,

    /// Live design tokens: `/admin/status` reports their last config reload.
    pub appearance: Option<Arc<crate::Appearance>>,
}

/// Start the MCP HTTP server on the calling Tokio runtime.
///
/// Binds the loopback listener and attempts each Tailscale listener, logs the
/// effective addresses, and spawns one accept loop per listener over a shared
/// server. Returns the join handle so the caller can await it during shutdown.
///
/// # Parameters
///
/// * `scene`           — shared scene graph for MCP tool dispatch.
/// * `config`          — MCP server configuration (bind addresses, agents).
/// * `shutdown`        — token that stops the accept loops when triggered.
/// * `portals` — optional shared portal hub and clock
///   (hud-bq0gl.2).  When `Some`, MCP calls this hub directly; the winit driver shares the same state and clock.
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
/// Returns an error if the first (loopback) listener cannot bind.
pub async fn start_mcp_http_server(
    scene: Arc<Mutex<SceneGraph>>,
    config: McpServerConfig,
    shutdown: ShutdownToken,
    portals: Option<tze_hud_mcp::PortalHandle>,
) -> std::io::Result<(tokio::task::JoinHandle<()>, Vec<SocketAddr>)> {
    start_mcp_http_server_with_render_wake(
        scene,
        config,
        shutdown,
        portals,
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
    portals: Option<tze_hud_mcp::PortalHandle>,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    safe_mode: Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<(tokio::task::JoinHandle<()>, Vec<SocketAddr>)> {
    start_mcp_http_server_inner(
        scene,
        config,
        shutdown,
        portals,
        render_wake,
        portal_ingress_wake,
        safe_mode,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg_attr(test, allow(clippy::too_many_arguments))] // Test-only address source injection.
async fn start_mcp_http_server_inner(
    scene: Arc<Mutex<SceneGraph>>,
    config: McpServerConfig,
    shutdown: ShutdownToken,
    portals: Option<tze_hud_mcp::PortalHandle>,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    portal_ingress_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
    safe_mode: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)] mut tailnet_fixture: Option<crate::net_addrs::TailnetFixture>,
) -> std::io::Result<(tokio::task::JoinHandle<()>, Vec<SocketAddr>)> {
    // The first address is loopback and must bind; a Tailscale address that
    // fails to bind is skipped with a warning. A handed-over instance binds
    // later, from the accept task (see `bind_gate`).
    let mut listeners = Vec::with_capacity(config.bind_addrs.len());
    let mut local_addrs = Vec::new();
    if config.bind_gate.is_none() {
        for (i, addr) in config.bind_addrs.iter().enumerate() {
            // Bound through std, whose sockets are not inheritable on Windows:
            // a restart's child process must not keep this listening socket
            // (and so the port) alive after this process exits.
            match crate::net_addrs::bind_listener(
                *addr,
                #[cfg(test)]
                None,
            )
            .and_then(TcpListener::from_std)
            {
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
    crate::operator::status::process_start();
    // In deferred mode this fills in as the listeners bind. Pairing reads
    // the same list for the address it shows and hands out.
    let binds = match &config.pairing {
        Some(pairing) => pairing.binds(&local_addrs),
        None => Arc::new(std::sync::Mutex::new(local_addrs.clone())),
    };
    let admin = Arc::new(StatusSource {
        agents: config.agents.clone(),
        safe_mode: Arc::clone(&safe_mode),
        presents: config.presents.clone(),
        capture: config.capture.clone(),
        restart: config.restart.clone(),
        update: config.update.clone(),
        pairing: config.pairing.clone(),
        appearance: config.appearance.clone(),
        firewall: match &config.pairing {
            Some(pairing) => pairing.firewall(),
            None => Arc::new(crate::firewall::FirewallProbe::system(
                Arc::clone(&binds),
                Vec::new(),
            )),
        },
        binds,
        log_path: crate::operator::logs::log_path(),
    });

    let mut server_builder = McpServer::with_shared_scene(scene)
        .with_config(McpConfig {
            agents: config.agents.clone(),
            widget_transition_ms: config.widget_transition_ms,
            publication_defaults: config.publication_defaults.clone(),
        })
        .with_render_wake_notifier(render_wake)
        .with_portal_ingress_wake_notifier(portal_ingress_wake)
        .with_safe_mode(safe_mode);
    if let Some(tx) = portals {
        server_builder = server_builder.with_portals(tx);
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
            config.late_tailnet_port,
            #[cfg(test)]
            tailnet_fixture.take(),
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

    if config.bind_gate.is_none()
        && let Some(port) = config.late_tailnet_port
    {
        loops.push(spawn_tailnet_watcher(
            port,
            server,
            admin,
            shutdown,
            #[cfg(test)]
            tailnet_fixture,
        ));
    }

    let handle = tokio::spawn(async move {
        for l in loops {
            let _ = l.await;
        }
    });

    Ok((handle, local_addrs))
}

/// Every successful bind shares the status/pairing list. Seed the manager
/// from that list after eager or deferred startup to avoid duplicate listeners.
fn spawn_tailnet_watcher(
    port: u16,
    server: Arc<McpServer>,
    admin: Arc<StatusSource>,
    shutdown: ShutdownToken,
    #[cfg(test)] fixture: Option<crate::net_addrs::TailnetFixture>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(test)]
        let injected = fixture.as_ref().map(|fixture| fixture.bind.clone());
        let bound = admin
            .binds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|a| a.ip())
            .collect();
        let watcher = crate::net_addrs::watch_for_tailnet(
            bound,
            |ip| {
                let addr = SocketAddr::new(ip, port);
                match crate::net_addrs::bind_listener(
                    addr,
                    #[cfg(test)]
                    injected.as_ref(),
                )
                .and_then(TcpListener::from_std)
                {
                    Ok(listener) => {
                        let local = listener
                            .local_addr()
                            .expect("bound listener has an address");
                        tracing::info!(addr = %local, "MCP HTTP listener bound (address change/retry)");
                        admin
                            .binds
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(local);
                        if let Some(pairing) = &admin.pairing {
                            pairing.refresh_open_system_card();
                        }
                        tokio::spawn(run_accept_loop(
                            listener,
                            Arc::clone(&server),
                            Arc::clone(&admin),
                            shutdown.clone(),
                            local,
                        ));
                        true
                    }
                    Err(e) => {
                        tracing::warn!(addr = %addr, error = %e, "MCP HTTP: failed to bind Tailscale address");
                        false
                    }
                }
            },
            #[cfg(test)]
            fixture,
        );
        tokio::select! {
            _ = watcher => {}
            _ = wait_shutdown(&shutdown) => {}
        }
    })
}

/// A handed-over instance: wait for the takeover, bind each listener (the old
/// instance may still be releasing the port), then serve them. The loopback
/// listener must bind or the gate fails and this instance shuts down.
/// Tailscale failures remain eligible for the address manager's retry.
async fn run_deferred_listeners(
    gate: crate::operator::handoff::BindGate,
    addrs: Vec<SocketAddr>,
    server: Arc<McpServer>,
    admin: Arc<StatusSource>,
    shutdown: ShutdownToken,
    late_tailnet_port: Option<u16>,
    #[cfg(test)] tailnet_fixture: Option<crate::net_addrs::TailnetFixture>,
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
                if let Some(pairing) = &admin.pairing {
                    pairing.refresh_open_system_card();
                }
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
    if let Some(port) = late_tailnet_port {
        loops.push(spawn_tailnet_watcher(
            port,
            server,
            admin,
            shutdown,
            #[cfg(test)]
            tailnet_fixture,
        ));
    }
    for l in loops {
        let _ = l.await;
    }
}

/// Resolves once `shutdown` has been triggered (checks the flag too, so a
/// trigger that predates the subscription is not missed).
async fn wait_shutdown(shutdown: &ShutdownToken) {
    let mut rx = shutdown.subscribe();
    if !shutdown.is_triggered() {
        let _ = rx.recv().await;
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
                        let connection = handle_connection(stream, peer, srv, admin);
                        if tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG) {
                            // One HTTP request per connection. The subscriber owns
                            // this process-local ID; it is not a JSON-RPC or agent ID.
                            let span = tracing::debug_span!(
                                target: "tze_hud_mcp_phases",
                                "mcp_request",
                                request_span_id = tracing::field::Empty
                            );
                            if let Some(id) = span.id() {
                                span.record("request_span_id", id.into_u64());
                            }
                            // Enter on each poll, never hold an enter guard across await.
                            tokio::spawn(connection.instrument(span));
                        } else {
                            tokio::spawn(connection);
                        }
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
    use crate::http::{self, OperatorCode, OperatorError, ReadError, Response, Route};
    use tokio::io::AsyncWriteExt;

    let phase_started = tracing::enabled!(target: "tze_hud_mcp_phases", tracing::Level::DEBUG)
        .then(std::time::Instant::now);
    if phase_started.is_some() {
        tracing::debug!(target: "tze_hud_mcp_phases", phase = "request_read_begin");
    }
    let request = http::read_request(&mut stream, http::READ_TIMEOUT).await;
    if let Some(started) = &phase_started {
        match &request {
            Ok(_) => tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "request_received",
                elapsed_us = started.elapsed().as_micros()
            ),
            Err(error) => tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "request_read_error",
                code = match error {
                    ReadError::Closed => "CLOSED",
                    ReadError::TooLarge => "TOO_LARGE",
                    ReadError::Malformed => "MALFORMED",
                    ReadError::NotImplemented => "NOT_IMPLEMENTED",
                    ReadError::Timeout => "TIMEOUT",
                },
                elapsed_us = started.elapsed().as_micros()
            ),
        }
    }
    let response = match request {
        Ok(req) => match http::route(&req.method, &req.path) {
            Route::Mcp => {
                // Config-gated resident-principal grant (hud-nu65o); the PSK
                // check still happens independently inside `dispatch`.
                let ctx = server.caller_context(req.bearer);
                let body = std::str::from_utf8(&req.body).unwrap_or("");
                if let Some(started) = &phase_started {
                    tracing::debug!(
                        target: "tze_hud_mcp_phases",
                        phase = "dispatch_begin",
                        elapsed_us = started.elapsed().as_micros()
                    );
                }
                let result = server.dispatch(body, &ctx).await;
                if let Some(started) = &phase_started {
                    tracing::debug!(
                        target: "tze_hud_mcp_phases",
                        phase = "dispatch_complete",
                        elapsed_us = started.elapsed().as_micros()
                    );
                }
                Response::json(result)
            }
            Route::Pair => match &admin.pairing {
                Some(pairing) => pairing.pair(&req.body),
                None => Response::operator_error(
                    403,
                    &OperatorError::new(OperatorCode::PairingClosed, "pairing is not available"),
                ),
            },
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

    let bytes = response.to_bytes();
    if let Some(started) = &phase_started {
        tracing::debug!(
            target: "tze_hud_mcp_phases",
            phase = "response_ready",
            status = response.status,
            bytes = bytes.len(),
            elapsed_us = started.elapsed().as_micros()
        );
        tracing::debug!(
            target: "tze_hud_mcp_phases",
            phase = "response_write_begin",
            elapsed_us = started.elapsed().as_micros()
        );
    }
    let written = stream.write_all(&bytes).await;
    if let Some(started) = &phase_started {
        match &written {
            Ok(()) => tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "response_write_complete",
                elapsed_us = started.elapsed().as_micros()
            ),
            Err(error) => tracing::debug!(
                target: "tze_hud_mcp_phases",
                phase = "response_write_error",
                io_kind = ?error.kind(),
                elapsed_us = started.elapsed().as_micros()
            ),
        }
    }
    if let Err(e) = written {
        tracing::debug!(peer = %peer, error = %e, "MCP: write error");
    }
}

/// `POST /admin/update {"channel": ...}`: download and verify on a blocking
/// thread, answer, and let the swap and handoff continue in the background.
async fn update_response(
    update: &crate::operator::update::UpdateHandle,
    body: &[u8],
) -> crate::http::Response {
    use crate::http::{OperatorCode, OperatorError, Response};
    use crate::operator::update::{Outcome, UpdateError};
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        channel: String,
    }
    let Ok(Body { channel }) = serde_json::from_slice(body) else {
        return Response::operator_error(
            400,
            &OperatorError::new(OperatorCode::UpdateFailed, UpdateError::BadChannel.hint()),
        );
    };
    let handle = update.clone();
    let result = tokio::task::spawn_blocking(move || handle.request(&channel))
        .await
        .unwrap_or(Err(UpdateError::Failed));
    let (status, code) = match result {
        Ok(Outcome::UpToDate) => return Response::json(r#"{"up_to_date":true}"#),
        Ok(Outcome::Started { sha }) => {
            return Response {
                status: 202,
                ..Response::json(serde_json::json!({"updating": true, "sha": sha}).to_string())
            };
        }
        Err(UpdateError::BadChannel) => (400, OperatorCode::UpdateFailed),
        Err(UpdateError::NotInstalled) => (409, OperatorCode::NotInstalled),
        Err(UpdateError::Busy) => (429, OperatorCode::Busy),
        Err(UpdateError::Unavailable) => (503, OperatorCode::Unavailable),
        Err(UpdateError::Failed) => (502, OperatorCode::UpdateFailed),
    };
    let hint = match &result {
        Err(e) => e.hint(),
        Ok(_) => unreachable!("successes returned above"),
    };
    Response::operator_error(status, &OperatorError::new(code, hint))
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
            Some(capture) => {
                match query_param(&req.query, "display").map_or(Ok(0), str::parse::<usize>) {
                    Err(_) => Response::bad_request(),
                    Ok(display) => match capture.capture_png(display).await {
                        Ok(png) => Response::png(png),
                        Err(e) => e.response(),
                    },
                }
            }
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
        AdminRoute::Update => match &admin.update {
            None => unavailable("this runtime cannot update itself"),
            Some(update) => update_response(update, &req.body).await,
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
            appearance: None,
            publication_defaults: Default::default(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &Default::default(),
                "motion.state.ms",
            ),
            bind_addrs: vec![format!("127.0.0.1:{port}").parse().unwrap()],
            late_tailnet_port: None,
            agents: tze_hud_scene::config::AgentDirectory::unrestricted(psk).shared(),
            presents: None,
            capture: None,
            restart: None,
            update: None,
            bind_gate: None,
            pairing: None,
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
        conn.shutdown().await.expect("shutdown writer");
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
            appearance: None,
            agents: dir.shared(),
            binds: Arc::new(std::sync::Mutex::new(vec![
                "127.0.0.1:9090".parse().unwrap(),
            ])),
            safe_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            presents: None,
            capture: None,
            restart: None,
            update: None,
            pairing: None,
            firewall: Arc::new(crate::firewall::FirewallProbe::new(|| {
                crate::firewall::TailnetInbound::Blocked {
                    reason: crate::firewall::BlockReason::NoAllowRule,
                    rule: None,
                }
            })),
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

        // Admin requests resolve the current directory too: neither a stale
        // key nor a formerly granted admin permission survives a reload.
        for permissions in [vec!["*", "operator_admin"], vec!["*"], vec![]] {
            let mut changed = tze_hud_scene::config::AgentDirectory::default();
            changed.insert(
                "star",
                tze_hud_scene::config::hash_psk("star-psk"),
                vec!["*".to_string()],
            );
            if !permissions.is_empty() {
                changed.insert(
                    "root",
                    tze_hud_scene::config::hash_psk("rotated-root-psk"),
                    permissions.iter().map(|p| p.to_string()).collect(),
                );
            }
            src.agents.store(Arc::new(changed));
            let expected = if permissions.contains(&"operator_admin") {
                200
            } else if permissions.is_empty() {
                401
            } else {
                403
            };
            for which in [Status, Logs] {
                for (bearer, status) in [
                    ("root-psk", 401),
                    ("rotated-root-psk", expected),
                    ("star-psk", 403),
                ] {
                    let req = admin_get("/admin/x", Some(bearer));
                    let r = handle_admin(which, &req, &src).await;
                    assert_eq!(r.status, status, "{bearer} {permissions:?} {which:?}");
                    if status == 403 {
                        assert!(String::from_utf8_lossy(&r.body).contains("NOT_ADMIN"));
                    }
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
            crate::operator::handoff::Busy::default(),
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

    #[tokio::test]
    async fn admin_update_is_guarded_and_answers_constant_errors() {
        use crate::http::AdminRoute::Update;
        use crate::operator::install::InstallPaths;
        use crate::operator::update::{UpdateConfig, UpdateHandle};
        let mut src = admin_source(std::env::temp_dir().join("tze_hud_no_such.log"));
        let post = |bearer: Option<&str>, body: &str| {
            let mut req = admin_get("/admin/update", bearer);
            req.method = "POST".into();
            req.body = body.as_bytes().to_vec();
            req
        };
        let ok = r#"{"channel":"dev"}"#;
        assert_eq!(
            handle_admin(Update, &post(None, ok), &src).await.status,
            401
        );
        assert_eq!(
            handle_admin(Update, &post(Some("star-psk"), ok), &src)
                .await
                .status,
            403
        );
        // Guarded first, then 503 without an updater (headless).
        assert_eq!(
            handle_admin(Update, &post(Some("root-psk"), ok), &src)
                .await
                .status,
            503
        );

        let paths = InstallPaths::from_dirs(
            std::path::Path::new("/nonexistent/Local"),
            std::path::Path::new("/nonexistent/Roaming"),
        );
        // Nothing listens on this base: a download failure must surface as the
        // one constant UPDATE_FAILED answer.
        let cfg = UpdateConfig {
            base_url: "http://127.0.0.1:1/releases".into(),
            curl: "curl".into(),
            fetch_timeout: std::time::Duration::from_secs(2),
        };
        let handle = |exe: &std::path::Path| {
            UpdateHandle::with_parts(
                cfg.clone(),
                crate::operator::update::PUBLIC_KEY.to_owned(),
                paths.clone(),
                exe.to_path_buf(),
                "old".into(),
                "dev-old".into(),
                Box::new(|_| unreachable!("no handoff without a verified release")),
                Arc::new(|| {}),
                std::time::Duration::from_millis(50),
                Box::new(|_| {}),
                crate::operator::handoff::Busy::default(),
            )
        };
        let body_of = |r: &crate::http::Response| String::from_utf8_lossy(&r.body).into_owned();

        src.update = Some(handle(&paths.exe));
        for bad in [
            "",
            "{}",
            r#"{"channel":"main"}"#,
            r#"{"channel":"dev","url":"x"}"#,
        ] {
            let r = handle_admin(Update, &post(Some("root-psk"), bad), &src).await;
            assert_eq!(r.status, 400, "{bad:?}");
        }
        let failed = handle_admin(Update, &post(Some("root-psk"), ok), &src).await;
        assert_eq!(failed.status, 502);
        assert!(body_of(&failed).contains("UPDATE_FAILED"));
        // Every failure cause reads the same on the wire.
        let again = handle_admin(
            Update,
            &post(Some("root-psk"), r#"{"channel":"v1.2.3"}"#),
            &src,
        )
        .await;
        assert_eq!(failed.body, again.body);

        src.update = Some(handle(std::path::Path::new("/elsewhere/tze_hud.exe")));
        let outside = handle_admin(Update, &post(Some("root-psk"), ok), &src).await;
        assert_eq!(outside.status, 409);
        assert!(body_of(&outside).contains("NOT_INSTALLED"));
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
            "tailnet_inbound",
            "agents",
            "safe_mode",
            "frames_presented",
            "cpu_pct_2s",
            "cpu_pct_avg",
            "last_update",
            "last_restart",
            "config_reload",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {text}");
        }
        assert_eq!(
            v["agents"],
            serde_json::json!([{"id":"root","admin":true},{"id":"star","admin":false}])
        );
        assert_eq!(v["binds"], serde_json::json!(["127.0.0.1:9090"]));
        assert_eq!(v["tailnet_inbound"]["state"], "blocked");
        assert_eq!(v["tailnet_inbound"]["reason"], "no_allow_rule");
        assert!(v["tailnet_inbound"]["fix"].is_string(), "{text}");
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
            (
                "400",
                format!("PO\tST / HTTP/1.1\r\n{auth}Content-Length: {n}\r\n\r\n{body}"),
            ),
            (
                "400",
                format!("POST / HTTP/1.1\r\n{auth}X: a\0b\r\nContent-Length: {n}\r\n\r\n{body}"),
            ),
            (
                "400",
                format!("POST / HTTP/1.1\r\n{auth}X: a\rb\r\nContent-Length: {n}\r\n\r\n{body}"),
            ),
            (
                "400",
                format!("POST / HTTP/1.1\r\n{auth}X: a\nb\r\nContent-Length: {n}\r\n\r\n{body}"),
            ),
            (
                "400",
                format!(
                    "POST / HTTP/1.1\r\n{auth}X: \x0bvalue\x0c\r\nContent-Length: {n}\r\n\r\n{body}"
                ),
            ),
            (
                "400",
                format!("POST / HTTP/1.1\r\n{auth}Transfer-Encoding : chunked\r\n\r\n{body}"),
            ),
            (
                "400",
                "POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\n".to_owned(),
            ),
            (
                "400",
                format!(
                    "POST / HTTP/1.1\r\n{auth}Content-Length: {}\r\n\r\n{body}",
                    n + 1
                ),
            ),
        ];
        for (status, req) in cases {
            let r = http_raw(addr, &req).await;
            assert!(r.starts_with(&format!("HTTP/1.1 {status} ")), "{req}: {r}");
            assert!(!r.contains("jsonrpc"), "{r}");
        }
        let dropped = http_raw(
            addr,
            "POST / HTTP/1.1\r\nContent-Length: 18446744073709551616\r\n\r\n",
        )
        .await;
        assert!(
            dropped.is_empty(),
            "overflow must retain connection drop: {dropped}"
        );
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
        let scene = make_scene();
        let config = make_config(0, "test-key");
        let shutdown = ShutdownToken::new();

        let (handle, addrs) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");
        let addr = addrs[0];

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
        let rig = crate::pairing::tests::rig();
        let config = McpServerConfig {
            appearance: None,
            publication_defaults: Default::default(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &Default::default(),
                "motion.state.ms",
            ),
            bind_addrs: vec![
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.2:0".parse().unwrap(),
            ],
            late_tailnet_port: Some(9090),
            agents: admin_source(std::env::temp_dir().join("no-log")).agents,
            presents: None,
            capture: None,
            restart: None,
            update: None,
            bind_gate: None,
            pairing: Some(Arc::clone(&rig.pairing)),
        };
        rig.pairing.binds(&[]).lock().unwrap().clear();
        let shutdown = ShutdownToken::new();
        let (fixture, mut control) = crate::net_addrs::tailnet_fixture();
        let (handle, addrs) = start_mcp_http_server_inner(
            make_scene(),
            config.clone(),
            shutdown.clone(),
            None,
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Some(fixture),
        )
        .await
        .expect("bind");
        assert_eq!(addrs.len(), 2);
        rig.pairing.open();
        let eager = rig
            .card
            .frame_state(crate::pairing::PairClock::wall_us(rig.clock.as_ref()));
        assert_eq!(
            eager.model.as_ref().unwrap().lines[1],
            format!("http://{}/pair", addrs[0])
        );

        let body = r#"{"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}"#;
        for addr in &addrs {
            let resp = http_post(*addr, body, Some("root-psk")).await;
            assert!(
                resp.contains("HTTP/1.1 200") && resp.contains("\"result\""),
                "tools/list on {addr} failed: {resp}"
            );
        }

        // The production watcher already has v4 in its successful seed. A
        // later v6 first fails, then recovers on an injected address event.
        control
            .addresses
            .send(vec![control.v4, control.v6])
            .unwrap();
        assert_eq!(control.next_attempt().await, (control.v6, None));
        let status_request = "GET /admin/status HTTP/1.1\r\nAuthorization: Bearer root-psk\r\n\r\n";
        let status = http_raw(addrs[0], status_request).await;
        let status: serde_json::Value =
            serde_json::from_str(status.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            status["binds"].as_array().unwrap().len(),
            2,
            "failed late bind advertised"
        );
        control
            .fail
            .store(false, std::sync::atomic::Ordering::Release);
        control
            .addresses
            .send(vec![control.v4, control.v6])
            .unwrap();
        let late = control.next_bound(control.v6).await;
        assert!(late.is_ipv6());
        let response = http_post(late, body, Some("root-psk")).await;
        assert!(response.contains("HTTP/1.1 200") && response.contains("hud_publish"));
        let status = http_raw(late, status_request).await;
        let status: serde_json::Value =
            serde_json::from_str(status.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(status["binds"].as_array().unwrap().len(), 3);
        assert_eq!(
            rig.card
                .frame_state(crate::pairing::PairClock::wall_us(rig.clock.as_ref())),
            eager,
            "appended loopback leaves preferred endpoint unchanged"
        );
        assert!(
            status["binds"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(late.to_string()))
        );

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");

        // Fixture-only empty startup: exercise the production late-bind callback
        // on the first successful listener, without any real tailnet interface.
        let rig = crate::pairing::tests::rig();
        rig.pairing.binds(&[]).lock().unwrap().clear();
        let shutdown = ShutdownToken::new();
        let (fixture, mut control) = crate::net_addrs::tailnet_fixture();
        let mut empty = config;
        empty.bind_addrs.clear();
        empty.pairing = Some(Arc::clone(&rig.pairing));
        let (handle, addrs) = start_mcp_http_server_inner(
            make_scene(),
            empty,
            shutdown.clone(),
            None,
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            tze_hud_scene::render_wake::RenderWakeNotifier::default(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Some(fixture),
        )
        .await
        .unwrap();
        assert!(addrs.is_empty());
        rig.pairing.open();
        let before = rig
            .card
            .frame_state(crate::pairing::PairClock::wall_us(rig.clock.as_ref()));
        let code = rig.pairing.code().unwrap();
        assert_eq!(
            before.model.as_ref().unwrap().lines[1],
            "this machine, port 9090"
        );
        control.addresses.send(vec![control.v6]).unwrap();
        assert_eq!(control.next_attempt().await, (control.v6, None));
        assert_eq!(
            rig.card
                .frame_state(crate::pairing::PairClock::wall_us(rig.clock.as_ref())),
            before
        );
        control
            .fail
            .store(false, std::sync::atomic::Ordering::Release);
        control.addresses.send(vec![control.v6]).unwrap();
        let late = control.next_bound(control.v6).await;
        // A completed request also proves callback publication finished: the
        // accept loop starts only after the pairing refresh has returned.
        let response = http_post(late, body, Some("root-psk")).await;
        assert!(response.contains("HTTP/1.1 200") && response.contains("hud_publish"));
        let after = rig
            .card
            .frame_state(crate::pairing::PairClock::wall_us(rig.clock.as_ref()));
        let mut expected = before;
        expected.model.as_mut().unwrap().lines[1] = format!("http://{late}/pair");
        assert_eq!(after, expected);
        assert_eq!(
            rig.pairing.code().unwrap(),
            code,
            "same code and monotonic deadline"
        );
        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    /// Only the loopback listener is fatal: a Tailscale address that cannot
    /// bind (192.0.2.1 is not local) is skipped, a loopback failure is not.
    #[tokio::test]
    async fn mcp_http_only_loopback_bind_failure_is_fatal() {
        let agents = tze_hud_scene::config::AgentDirectory::unrestricted("k").shared();
        let cfg = |addrs: [&str; 2]| McpServerConfig {
            appearance: None,
            publication_defaults: Default::default(),
            widget_transition_ms: tze_hud_config::tokens::resolve_motion_duration_ms(
                &Default::default(),
                "motion.state.ms",
            ),
            bind_addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
            late_tailnet_port: None,
            agents: agents.clone(),
            presents: None,
            capture: None,
            restart: None,
            update: None,
            bind_gate: None,
            pairing: None,
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
        let scene = make_scene();
        let config = make_config(0, "real-key");
        let shutdown = ShutdownToken::new();

        let (handle, addrs) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");
        let addr = addrs[0];

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
        let scene = make_scene();
        let config = make_config(0, "correct-key");
        let shutdown = ShutdownToken::new();

        let (handle, addrs) = start_mcp_http_server(scene, config, shutdown.clone(), None)
            .await
            .expect("bind");
        let addr = addrs[0];

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
        #[derive(Clone, Default)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let writer = sink.clone();
        let _trace = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .json()
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        use tze_hud_scene::SceneId;
        use tze_hud_scene::types::{
            ContentionPolicy, GeometryPolicy, LayerAttachment, RenderingPolicy, ZoneDefinition,
            ZoneMediaType,
        };

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
                    accepted_media_types: vec![
                        ZoneMediaType::StreamText,
                        ZoneMediaType::ShortTextWithIcon,
                    ],
                    rendering_policy: RenderingPolicy::default(),
                    contention_policy: ContentionPolicy::LatestWins,
                    max_publishers: 4,
                    auto_clear_ms: None,
                    ephemeral: false,
                    layer_attachment: LayerAttachment::Content,
                },
            );
            let raw: tze_hud_config::raw::RawConfig = toml::from_str(
                r#"
                [[tabs]]
                name = "Main"
                default_tab = true
                [[tabs.widgets]]
                widget_type = "gauge"
                instance_id = "motion-gauge"
            "#,
            )
            .unwrap();
            crate::widget_startup::init_widget_registry(
                &mut s,
                &raw,
                None,
                &std::collections::HashMap::new(),
                &tze_hud_config::themes::resolve_config_tokens(&Default::default()),
            );
        }

        {
            let mut s = scene.lock().await;
            let mut subtitle = s.zone_registry.zones["test-zone"].clone();
            subtitle.name = "subtitle".into();
            s.register_zone(subtitle);
        }
        let mut config = make_config(0, "test-key");
        config.widget_transition_ms = tze_hud_config::tokens::resolve_motion_duration_ms(
            &std::collections::HashMap::from([("motion.state.ms".into(), "240".into())]),
            "motion.state.ms",
        );
        let defaults = config.publication_defaults.clone();
        let shutdown = ShutdownToken::new();

        let (handle, addrs) = start_mcp_http_server(scene.clone(), config, shutdown.clone(), None)
            .await
            .expect("bind");
        let addr = addrs[0];

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let body = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"zone:test-zone","content":"hello from MCP"}},"id":4}"#;
        let resp = http_post(addr, body, Some("test-key")).await;

        assert!(resp.contains("HTTP/1.1 200"));
        assert!(
            resp.contains(r#"{\"expires_in_ms\":60000,\"ok\":true}"#),
            "expected a successful publish, got: {resp}"
        );
        let body = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"widget:motion-gauge","params":{"level":0.75,"label":"CPU"}}},"id":5}"#;
        let resp = http_post(addr, body, Some("test-key")).await;
        assert!(resp.contains("HTTP/1.1 200"));
        {
            let scene = scene.lock().await;
            let record = &scene.widget_registry.active_publishes["motion-gauge"][0];
            assert_eq!(
                record.transition_ms, 240,
                "HTTP bridge must forward startup duration"
            );
            assert_eq!(
                record.params["level"],
                tze_hud_scene::types::WidgetParameterValue::F32(0.75)
            );
        }

        defaults.set_answer_window_ms(30_000);
        let question = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"zone:test-zone","content":{"type":"notification","body":"Continue?","actions":[{"label":"Yes","callback_id":"yes"}]}}},"id":6}"#;
        let response = http_post(addr, question, Some("test-key")).await;
        assert!(
            response.contains(r#"{\"expires_in_ms\":30000,\"ok\":true}"#),
            "{response}"
        );
        defaults.set_answer_window_ms(120_000);
        let response = http_post(addr, question, Some("test-key")).await;
        assert!(
            response.contains(r#"{\"expires_in_ms\":120000,\"ok\":true}"#),
            "{response}"
        );

        // DEBUG-hud-bhs4r-capture-probe: replay another test thread rebuilding
        // shared callsite interest while this thread owns the only subscriber.
        std::thread::spawn(tracing::callsite::rebuild_interest_cache)
            .join()
            .unwrap();
        let delayed = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"hud_publish","arguments":{"surface":"zone:subtitle","content":"delayed","delay_ms":3000,"ttl_ms":20000}},"id":8}"#;
        let response = http_post(addr, delayed, Some("test-key")).await;
        assert!(response.contains(r#"{\"expires_in_ms\":23000,\"ok\":true}"#));
        let records: Vec<serde_json::Value> = String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|e| e["target"] == "tze_hud_delay_diagnostic")
            .collect();
        assert_eq!(records.len(), 2);
        let request = &records[0]["fields"];
        let enqueue = &records[1]["fields"];
        assert_eq!(request["event"], "delayed_request");
        assert_eq!(request["request_id"], 8);
        assert_eq!(request["request_span_id"], enqueue["request_span_id"]);
        assert!(request["request_span_id"].as_u64().is_some());
        let scene_guard = scene.lock().await;
        let batch = &scene_guard.scheduled_batches[0];
        let (hi, lo) = batch.batch.batch_id.as_uuid().as_u64_pair();
        assert_eq!(enqueue["batch_hi"], hi);
        assert_eq!(enqueue["batch_lo"], lo);
        assert_eq!(enqueue["present_at_wall_us"], batch.present_at_wall_us);
        assert_eq!(
            enqueue["scheduled_at_wall_us"].as_u64().unwrap() + 3_000_000,
            batch.present_at_wall_us,
            "the emitted scheduling sample must be the one used for this batch"
        );
        assert_eq!(
            enqueue["expires_at_wall_us"],
            batch.present_at_wall_us + 20_000_000
        );
        drop(scene_guard);

        shutdown.trigger(crate::threads::ShutdownReason::Clean);
        handle.await.expect("task");
    }

    #[tokio::test]
    async fn mcp_server_shuts_down_cleanly() {
        let scene = make_scene();
        let config = make_config(0, "key");
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
        result
            .unwrap()
            .expect("MCP server task panicked during shutdown");
    }
}
