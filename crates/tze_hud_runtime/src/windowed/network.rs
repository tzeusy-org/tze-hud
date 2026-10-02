//! Network/runtime-context bootstrap helpers for the windowed runtime.

use std::sync::Arc;

use tokio::sync::Mutex;
use tze_hud_config::TzeHudConfig;
use tze_hud_protocol::proto::session::hud_session_server::HudSessionServer;
use tze_hud_protocol::proto::session::runtime_service_server::RuntimeServiceServer;
use tze_hud_protocol::session::SharedState;
use tze_hud_protocol::session_server::HudSessionImpl;
use tze_hud_scene::config::ConfigLoader;

use super::WindowedConfig;
use super::config::select_grpc_bind_host;
use crate::reload_triggers::RuntimeServiceImpl;
use crate::runtime_context::{RuntimeContext, SharedRuntimeContext};
use crate::threads::NetworkRuntime;

/// Build a `RuntimeContext` from the windowed config.
///
/// When `cfg.config_toml` is `Some`, the TOML is parsed and validated. On
/// success, each `[agents.<id>]` table's `allow` list and PSK are loaded into
/// the context. The fallback policy is `Guest` (configured agents only).
///
/// When `cfg.config_toml` is `None` (no config file), the context falls back to
/// `RuntimeContext::headless_default()` and `fallback_unrestricted = true` for
/// dev-friendly behaviour (any PSK-authenticated agent gets all capabilities).
///
/// Parse or validation errors are logged as warnings and cause a graceful
/// fallback to `headless_default()` so the runtime can still start.
///
/// Returns `(runtime_context, fallback_unrestricted)`.
pub(super) fn build_runtime_context(cfg: &WindowedConfig) -> (SharedRuntimeContext, bool) {
    match &cfg.config_toml {
        None => {
            // No config file - fall back to headless default.
            tracing::debug!(
                "windowed runtime: no config TOML provided; \
                 using headless_default (all agents unrestricted)"
            );
            (
                Arc::new(
                    RuntimeContext::headless_default()
                        .with_fallback_policy(crate::runtime_context::FallbackPolicy::Unrestricted),
                ),
                true,
            )
        }
        Some(toml_src) => {
            // Parse the TOML.
            let loader = match TzeHudConfig::parse(toml_src) {
                Ok(l) => l,
                Err(parse_err) => {
                    tracing::warn!(
                        error = %parse_err.message,
                        line = parse_err.line,
                        column = parse_err.column,
                        "windowed runtime: config TOML parse error; \
                         falling back to headless_default"
                    );
                    return (Arc::new(RuntimeContext::headless_default()), false);
                }
            };

            // Validate and freeze into a ResolvedConfig.
            let resolved = match loader.freeze() {
                Ok(r) => r,
                Err(errors) => {
                    for err in &errors {
                        tracing::warn!(
                            code = ?err.code,
                            field = %err.field_path,
                            expected = %err.expected,
                            got = %err.got,
                            hint = %err.hint,
                            "windowed runtime: config validation error"
                        );
                    }
                    tracing::warn!(
                        "windowed runtime: {} config validation error(s); \
                         falling back to headless_default",
                        errors.len()
                    );
                    return (Arc::new(RuntimeContext::headless_default()), false);
                }
            };

            let hot = tze_hud_config::reload_config(toml_src).unwrap_or_default();

            tracing::info!(
                profile = %resolved.profile.name,
                agents = resolved.agent_capabilities.len(),
                "windowed runtime: config loaded; \
                 agent allow lists applied from [agents.<id>]"
            );

            let ctx = RuntimeContext::from_config_with_hot(
                resolved,
                crate::runtime_context::FallbackPolicy::Guest,
                hot,
            );
            (Arc::new(ctx), false)
        }
    }
}

/// Start network services (gRPC) on a dedicated Tokio multi-thread runtime.
///
/// Returns `(network_rt, handles, ..., grpc_bound_addr)`:
/// - `network_rt` is `Some(NetworkRuntime)` when `grpc_port != 0`; `None` if
///   all services are disabled (port 0 disables gRPC).
/// - `handles` contains join handles for each spawned server task.
/// - `grpc_bound_addr` is `Some` with the *actually bound* socket address when
///   gRPC is enabled and bound successfully; `None` when gRPC is disabled.
///
/// ## gRPC server
///
/// When `grpc_port != 0`, starts the `HudSession` gRPC server. The bind
/// address is `127.0.0.1:grpc_port` by default (loopback only) unless
/// `bind_all_interfaces` is `true`, in which case it binds `0.0.0.0:grpc_port`
/// (all interfaces - explicit opt-in required, hud-1aswu.1).
/// Setting `grpc_port = 0` skips server creation (compositor-only mode).
///
/// The listener is bound *eagerly* (before the serve task is spawned) so a
/// port conflict fails startup fast and the returned `grpc_bound_addr` reflects
/// a listener that is genuinely up — rather than an address the serve task
/// might fail to bind asynchronously (hud-ylwqc).
///
/// ## Errors
///
/// Returns `Err` if the `NetworkRuntime` Tokio runtime cannot be created, if
/// the gRPC server address fails to parse, or if the gRPC listener fails to
/// bind (e.g. the port is already in use).
type NetworkServices = (
    Option<NetworkRuntime>,
    Vec<tokio::task::JoinHandle<()>>,
    Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::ElementRepositionedEvent>>,
    Option<tze_hud_protocol::session_server::InputEventSender>,
    Option<tokio::sync::broadcast::Sender<tze_hud_protocol::proto::FramePresented>>,
    Option<tze_hud_protocol::session_server::DegradationNoticeSender>,
    Option<tze_hud_protocol::session_server::LeaseExpirySender>,
    Option<std::net::SocketAddr>,
);

#[allow(clippy::type_complexity)] // return type is self-documenting in this internal helper
#[cfg(test)]
pub(super) fn start_network_services(
    grpc_port: u16,
    psk: &str,
    shared_state: Arc<Mutex<SharedState>>,
    runtime_context: SharedRuntimeContext,
    fallback_unrestricted: bool,
    bind_all_interfaces: bool,
) -> Result<NetworkServices, Box<dyn std::error::Error>> {
    start_network_services_with_render_wake(
        grpc_port,
        psk,
        shared_state,
        runtime_context,
        fallback_unrestricted,
        bind_all_interfaces,
        tze_hud_scene::render_wake::RenderWakeNotifier::default(),
    )
}

#[allow(clippy::type_complexity)]
pub(super) fn start_network_services_with_render_wake(
    grpc_port: u16,
    psk: &str,
    shared_state: Arc<Mutex<SharedState>>,
    runtime_context: SharedRuntimeContext,
    fallback_unrestricted: bool,
    bind_all_interfaces: bool,
    render_wake: tze_hud_scene::render_wake::RenderWakeNotifier,
) -> Result<NetworkServices, Box<dyn std::error::Error>> {
    if grpc_port == 0 {
        tracing::info!(
            "windowed runtime: gRPC server disabled (grpc_port = 0); running compositor-only"
        );
        // Compositor-only mode: no session, so no present-ack subscriber. The
        // compositor thread still drains the present-ack queue (bounded memory)
        // but has no sender to broadcast on (hud-4va6q).
        return Ok((None, Vec::new(), None, None, None, None, None, None));
    }

    // Build the multi-thread Tokio runtime for network tasks.
    let network_rt = NetworkRuntime::new()
        .map_err(|e| format!("windowed runtime: failed to build network Tokio runtime: {e}"))?;

    // Security fix (hud-1aswu.1): default to loopback; opt-in for all interfaces.
    let grpc_bind_host = select_grpc_bind_host(bind_all_interfaces);
    tracing::info!(
        bind_all_interfaces,
        grpc_bind_host,
        "gRPC: bind address selected (hud-1aswu.1)"
    );
    let addr: std::net::SocketAddr = format!("{grpc_bind_host}:{grpc_port}")
        .parse()
        .map_err(|e| format!("windowed runtime: invalid gRPC address (port {grpc_port}): {e}"))?;

    // Wire config-driven agent identity (allow lists + per-agent PSKs).
    let agent_caps = runtime_context.snapshot_agent_capabilities();
    let service = HudSessionImpl::from_shared_state_with_runtime_envelope(
        shared_state,
        psk,
        agent_caps,
        runtime_context.snapshot_agent_resource_budgets(),
        runtime_context.fallback_resource_budget(),
        fallback_unrestricted,
        Some(std::sync::Arc::new(
            crate::RuntimeMutationBudgetEnforcer::with_limits(
                runtime_context.operational_envelope.max_resident_sessions,
                runtime_context.operational_envelope.max_leased_tiles,
                runtime_context
                    .operational_envelope
                    .max_agent_leased_texture_bytes,
            ),
        )),
    )
    .with_agent_psks(runtime_context.snapshot_agent_psks(psk))
    .with_render_wake_notifier(render_wake);

    // Clone the broadcast senders before moving the service into the gRPC task.
    // The windowed runtime holds these senders to:
    // - broadcast ElementRepositionedEvents from the sync chrome-layer reset path.
    // - inject EventBatch payloads (scroll, keyboard, and future input events)
    //   on the input_event_tx channel after windowed input is processed.
    let element_repositioned_tx = service.element_repositioned_tx.clone();
    let input_event_tx = service.input_event_tx.clone();
    // Present-ack broadcast sender (hud-4va6q): the compositor thread emits
    // `FramePresented` on this after each presented frame, mirroring the
    // headless runtime's producer. Cloned before the service moves into the
    // gRPC task; subscribers attach via HudSession::subscribe_frame_presented.
    let frame_presented_tx = service.frame_presented_tx.clone();
    let degradation_notices = service.degradation_notices.clone();
    let lease_expirations = service.lease_expirations.clone();

    // Wire RuntimeService (ReloadConfig RPC) alongside HudSession.
    let runtime_svc = RuntimeServiceImpl::new(Arc::clone(&runtime_context));

    tracing::info!(grpc_addr = %addr, "windowed runtime: starting gRPC server");

    // Bind the gRPC listener eagerly (hud-ylwqc). `std::net::TcpListener::bind`
    // is synchronous and needs no reactor, so we learn immediately whether the
    // port is available and can surface a genuine bound address — instead of
    // letting `tonic::Server::serve(addr)` bind lazily inside the spawned task
    // where a conflict would only be logged after we already reported "ready".
    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| format!("windowed runtime: failed to bind gRPC listener on {addr}: {e}"))?;
    std_listener.set_nonblocking(true).map_err(|e| {
        format!("windowed runtime: failed to set gRPC listener non-blocking on {addr}: {e}")
    })?;
    let grpc_bound_addr = std_listener
        .local_addr()
        .map_err(|e| format!("windowed runtime: failed to read gRPC local_addr: {e}"))?;

    // Spawn the combined gRPC server task onto the network runtime, serving over
    // the already-bound listener via `serve_with_incoming`.
    let handle = network_rt.rt.spawn(async move {
        // `from_std` requires a Tokio reactor, so it runs inside the task.
        let tokio_listener = match tokio::net::TcpListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, "gRPC: failed to adopt bound listener into Tokio runtime");
                return;
            }
        };
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(tokio_listener);
        tonic::transport::Server::builder()
            .add_service(HudSessionServer::new(service))
            .add_service(RuntimeServiceServer::new(runtime_svc))
            .serve_with_incoming(incoming)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "gRPC server exited with error");
            });
    });

    tracing::info!(grpc_addr = %grpc_bound_addr, "windowed runtime: gRPC server task spawned");

    Ok((
        Some(network_rt),
        vec![handle],
        Some(element_repositioned_tx),
        Some(input_event_tx),
        Some(frame_presented_tx),
        Some(degradation_notices),
        Some(lease_expirations),
        Some(grpc_bound_addr),
    ))
}

/// Render the non-secret startup banner printed to stdout once the network
/// listeners are up (hud-ylwqc).
///
/// The runtime otherwise emits nothing on stdout unless `TZE_HUD_LOG` is set
/// (tracing is gated on that env var), so a fresh operator has no way to learn
/// where the runtime is listening or how to attach. This banner makes the
/// runtime self-describing on first run.
///
/// **Security invariant:** this function deliberately takes *only* bound socket
/// addresses. The PSK (and every other credential) is not in scope here, so the
/// banner is provably incapable of leaking a secret — see the unit tests. When
/// a service is disabled, its address is passed as `None` and rendered as
/// `disabled` rather than a bogus endpoint.
pub(super) fn render_startup_banner(
    grpc_addr: Option<std::net::SocketAddr>,
    mcp_addr: Option<std::net::SocketAddr>,
) -> String {
    const RULE: &str = "────────────────────────────────────────────────────────────────────";
    let mut lines: Vec<String> = Vec::with_capacity(7);
    lines.push(RULE.to_string());
    lines.push(" tze_hud runtime ready".to_string());
    match grpc_addr {
        Some(addr) => lines.push(format!("   gRPC   : {addr}")),
        None => lines.push("   gRPC   : disabled".to_string()),
    }
    match mcp_addr {
        Some(addr) => lines.push(format!(
            "   MCP    : {}   (auth: Authorization: Bearer <TZE_HUD_PSK>)",
            mcp_endpoint_url(addr)
        )),
        None => lines.push("   MCP    : disabled".to_string()),
    }
    lines.push(
        "   attach : invoke the `hud-projection` skill in an LLM session, or run".to_string(),
    );
    lines.push("            scripts/quickstart.sh — see docs/QUICKSTART.md".to_string());
    lines.push(RULE.to_string());
    lines.join("\n")
}

/// The MCP HTTP endpoint URL for a bound/configured address.
///
/// Single source of truth for the MCP URL shape, shared by the startup banner
/// and `--print-attach-info` (`render_attach_info`) so the two can never drift.
/// This is a pure formatter: it reports `addr` verbatim (the banner deliberately
/// advertises the genuine bound address, including `0.0.0.0` when all interfaces
/// were bound). Callers that need a *connectable* client URL should pass an
/// already-loopback-normalized address (see `connectable_addr`).
pub(super) fn mcp_endpoint_url(addr: std::net::SocketAddr) -> String {
    format!("http://{addr}/mcp")
}

/// Translate an all-interfaces bind address (`0.0.0.0` / `::`) into a
/// connectable loopback address for a client-facing URL. An all-interfaces bind
/// includes loopback, so `127.0.0.1` is always reachable; a literal `0.0.0.0`
/// URL is not something a client can connect to. Non-wildcard addresses pass
/// through unchanged.
fn connectable_addr(addr: std::net::SocketAddr) -> std::net::SocketAddr {
    if addr.ip().is_unspecified() {
        std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            addr.port(),
        )
    } else {
        addr
    }
}

/// Render the human-readable **attach info** block printed by the native
/// `--print-attach-info` flag (hud-b7c0m).
///
/// This is the single source of truth for the attach block: the MCP endpoint
/// URL, the resident-principal == PSK rule, and a paste-ready MCP client config
/// JSON snippet. It runs *without starting the runtime*, so it takes the
/// configured (not yet bound) addresses that the runtime would use.
///
/// **Security invariant:** like `render_startup_banner`, this function takes
/// *only* socket addresses and the config path — never the PSK or any other
/// credential. The PSK is always a placeholder in the printed snippet, so the
/// block is provably incapable of leaking a secret (see the unit tests).
///
/// A disabled service (`--mcp-port 0` / `--grpc-port 0`) is passed as `None` and
/// rendered as `disabled`. When MCP is disabled there is nothing to attach to,
/// so the JSON snippet is omitted with an explanatory line.
pub fn render_attach_info(
    mcp_addr: Option<std::net::SocketAddr>,
    grpc_addr: Option<std::net::SocketAddr>,
    config_path: Option<&str>,
) -> String {
    const RULE: &str =
        "────────────────────────────────────────────────────────────────────────────";
    let mut lines: Vec<String> = Vec::new();
    lines.push(RULE.to_string());
    lines.push(" tze_hud — ATTACH INFO  (point your LLM session's MCP client here)".to_string());
    lines.push(RULE.to_string());

    let connectable_mcp = mcp_addr.map(connectable_addr);
    match mcp_addr {
        Some(addr) => {
            lines.push(format!(
                " MCP endpoint : {}",
                mcp_endpoint_url(connectable_mcp.expect("mcp_addr is Some"))
            ));
            if addr.ip().is_unspecified() {
                lines.push(format!(
                    "                (bound all interfaces at {addr}; also reachable on this host's LAN IP)"
                ));
            }
        }
        None => lines.push(" MCP endpoint : disabled (--mcp-port 0)".to_string()),
    }
    match grpc_addr {
        Some(addr) => lines.push(format!(" gRPC         : {addr}")),
        None => lines.push(" gRPC         : disabled (--grpc-port 0)".to_string()),
    }
    match config_path {
        Some(path) => lines.push(format!(" config       : {path}")),
        None => lines.push(" config       : (none resolved — using flag/env defaults)".to_string()),
    }

    lines.push(String::new());
    lines.push(
        " Auth: every MCP request must send the pre-shared key (PSK) as a bearer token:"
            .to_string(),
    );
    lines.push("     Authorization: Bearer <your PSK — the value of TZE_HUD_PSK>".to_string());

    lines.push(String::new());
    lines.push(" Projection (the portal_projection_* tools):".to_string());
    lines.push(
        "   The MCP Authorization: Bearer PSK identifies your agent; its [agents.<id>]".to_string(),
    );
    lines.push(
        "   allow list must include \"portal\" (shipped configs: [agents.claude] reads".to_string(),
    );
    lines.push("   TZE_HUD_PSK with allow = [\"*\"]).".to_string());
    lines.push("   (This command never prints the PSK value itself.)".to_string());

    lines.push(String::new());
    match connectable_mcp {
        Some(addr) => {
            let url = mcp_endpoint_url(addr);
            lines.push(
                " Paste-ready MCP client config (e.g. .mcp.json / settings.json):".to_string(),
            );
            // PSK is a placeholder — never the real value.
            for jline in [
                "   {".to_string(),
                "     \"mcpServers\": {".to_string(),
                "       \"tze-hud-runtime\": {".to_string(),
                "         \"type\": \"url\",".to_string(),
                format!("         \"url\": \"{url}\","),
                "         \"headers\": {".to_string(),
                "           \"Authorization\": \"Bearer <PSK from TZE_HUD_PSK>\"".to_string(),
                "         }".to_string(),
                "       }".to_string(),
                "     }".to_string(),
                "   }".to_string(),
            ] {
                lines.push(jline);
            }
            lines.push(String::new());
            lines.push(
                " Then, in the LLM session, invoke the `hud-projection` skill and 'attach' —"
                    .to_string(),
            );
            lines.push(" see docs/QUICKSTART.md for the full attach walkthrough.".to_string());
        }
        None => {
            lines.push(
                " MCP is disabled, so there is no endpoint to attach to. Re-run with a non-zero"
                    .to_string(),
            );
            lines.push(" --mcp-port (default 9090) to expose the MCP client surface.".to_string());
        }
    }
    lines.push(RULE.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::super::test_support::make_shared_state;
    use super::*;

    /// The banner must never contain the PSK, even when one is configured.
    /// `render_startup_banner` takes only bound addresses (never the secret),
    /// so this holds by construction; the test guards against future edits that
    /// might thread a credential through the banner.
    #[test]
    fn startup_banner_never_contains_psk() {
        let psk = "SUPER-SECRET-PSK-2f9c1a7e-do-not-leak";
        // Simulate a fully-configured runtime with a PSK set in the environment.
        let grpc: std::net::SocketAddr = "127.0.0.1:50051".parse().unwrap();
        let mcp: std::net::SocketAddr = "127.0.0.1:9090".parse().unwrap();
        let banner = render_startup_banner(Some(grpc), Some(mcp));
        assert!(
            !banner.contains(psk),
            "startup banner must not leak the PSK; banner was:\n{banner}"
        );
        // Also assert the banner carries the useful, non-secret discovery info.
        assert!(banner.contains("127.0.0.1:50051"), "gRPC addr missing");
        assert!(
            banner.contains("http://127.0.0.1:9090/mcp"),
            "MCP URL missing"
        );
        assert!(banner.contains("hud-projection"), "attach hint missing");
        assert!(banner.contains("tze_hud runtime ready"), "header missing");
    }

    /// Disabled services render as `disabled`, not a bogus `:0` endpoint.
    #[test]
    fn startup_banner_renders_disabled_services() {
        let banner = render_startup_banner(None, None);
        assert!(banner.contains("gRPC   : disabled"));
        assert!(banner.contains("MCP    : disabled"));
        // Attach hint is always present so the runtime stays self-describing.
        assert!(banner.contains("hud-projection"));
    }

    /// The attach-info block must carry the discovery surface (MCP URL, the
    /// resident-principal == PSK rule, and a paste-ready JSON snippet) and must
    /// never contain a configured PSK value — the snippet always uses a
    /// placeholder.
    #[test]
    fn attach_info_carries_discovery_surface_without_psk() {
        let psk = "SUPER-SECRET-PSK-2f9c1a7e-do-not-leak";
        let mcp: std::net::SocketAddr = "127.0.0.1:9090".parse().unwrap();
        let grpc: std::net::SocketAddr = "127.0.0.1:50051".parse().unwrap();
        let info = render_attach_info(Some(mcp), Some(grpc), Some("/etc/tze_hud/config.toml"));

        assert!(
            !info.contains(psk),
            "attach info must not leak the PSK:\n{info}"
        );
        assert!(
            info.contains("http://127.0.0.1:9090/mcp"),
            "MCP endpoint URL missing:\n{info}"
        );
        assert!(
            info.contains("127.0.0.1:50051"),
            "gRPC addr missing:\n{info}"
        );
        assert!(
            info.contains("/etc/tze_hud/config.toml"),
            "config path missing:\n{info}"
        );
        assert!(
            info.contains("allow list must include"),
            "allow-list rule missing:\n{info}"
        );
        assert!(
            info.contains("Authorization: Bearer") || info.contains("\"Authorization\""),
            "bearer auth guidance missing:\n{info}"
        );
        assert!(
            info.contains("\"mcpServers\""),
            "JSON snippet missing:\n{info}"
        );
        assert!(
            info.contains("<PSK from TZE_HUD_PSK>"),
            "JSON snippet must use a PSK placeholder:\n{info}"
        );
    }

    /// An all-interfaces bind (`0.0.0.0`) is not a connectable client URL, so the
    /// snippet substitutes loopback while the info block still discloses the
    /// wildcard bind.
    #[test]
    fn attach_info_all_interfaces_uses_connectable_loopback_url() {
        let mcp: std::net::SocketAddr = "0.0.0.0:9090".parse().unwrap();
        let info = render_attach_info(Some(mcp), None, None);
        assert!(
            info.contains("http://127.0.0.1:9090/mcp"),
            "wildcard bind must yield a loopback client URL:\n{info}"
        );
        assert!(
            info.contains("all interfaces"),
            "wildcard bind should be disclosed:\n{info}"
        );
        assert!(
            !info.contains("http://0.0.0.0:9090/mcp"),
            "must not print a non-connectable 0.0.0.0 client URL:\n{info}"
        );
    }

    /// When MCP is disabled, the block says so and omits the (useless) JSON
    /// snippet rather than advertising a bogus endpoint.
    #[test]
    fn attach_info_mcp_disabled_omits_snippet() {
        let info = render_attach_info(None, None, None);
        assert!(info.contains("MCP endpoint : disabled"), "info:\n{info}");
        assert!(
            !info.contains("\"mcpServers\""),
            "disabled MCP must not emit a client snippet:\n{info}"
        );
        assert!(
            info.contains("--mcp-port"),
            "should hint how to enable MCP:\n{info}"
        );
    }

    /// When `grpc_port == 0`, `start_network_services` must return `None` for
    /// the runtime and an empty handle list (compositor-only mode, AC §2).
    #[test]
    fn start_network_services_grpc_port_zero_returns_no_runtime() {
        let shared_state = make_shared_state();
        let ctx: SharedRuntimeContext = Arc::new(RuntimeContext::headless_default());
        let (
            rt,
            handles,
            _tx,
            _scroll_tx,
            present_tx,
            _degradation_notices,
            lease_expirations,
            grpc_addr,
        ) = start_network_services(0, "test-psk", shared_state, ctx, true, false)
            .expect("start_network_services should not fail for port 0");
        assert!(
            rt.is_none(),
            "grpc_port=0 must not create a NetworkRuntime (compositor-only)"
        );
        assert!(
            handles.is_empty(),
            "grpc_port=0 must not spawn any network task handles"
        );
        assert!(
            present_tx.is_none(),
            "grpc_port=0 has no session, so no present-ack sender (hud-4va6q)"
        );
        assert!(
            lease_expirations.is_none(),
            "grpc_port=0 has no session, so no terminal lease-expiry sender"
        );
        assert!(
            grpc_addr.is_none(),
            "grpc_port=0 must not report a bound gRPC address"
        );
    }

    /// When `grpc_port != 0`, `start_network_services` must return `Some` for
    /// the runtime and at least one spawned task handle (AC §1).
    #[test]
    fn start_network_services_nonzero_port_returns_runtime_and_handle() {
        let shared_state = make_shared_state();
        let ctx: SharedRuntimeContext = Arc::new(RuntimeContext::headless_default());
        // Allocate an ephemeral port so parallel CI runs don't collide on a
        // fixed port (the listener is now bound eagerly, so a fixed port would
        // flake under concurrency).
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .expect("failed to allocate ephemeral port");
        let (
            rt,
            handles,
            _tx,
            _scroll_tx,
            present_tx,
            _degradation_notices,
            lease_expirations,
            grpc_addr,
        ) = start_network_services(port, "test-psk", shared_state, ctx, true, true)
            .expect("start_network_services should not error for a valid port");
        assert!(
            rt.is_some(),
            "non-zero grpc_port must create a NetworkRuntime"
        );
        assert!(
            !handles.is_empty(),
            "non-zero grpc_port must spawn at least one network task handle"
        );
        assert!(
            present_tx.is_some(),
            "non-zero grpc_port must expose the session present-ack sender so the \
             compositor thread can broadcast FramePresented (hud-4va6q)"
        );
        assert!(
            lease_expirations.is_some(),
            "non-zero grpc_port must expose the terminal lease-expiry sender so the \
             compositor can notify connected lease owners"
        );
        assert_eq!(
            grpc_addr.map(|a| a.port()),
            Some(port),
            "non-zero grpc_port must report the genuine bound gRPC address"
        );
        // Abort the spawned task so the test doesn't leave a lingering server.
        for h in handles {
            h.abort();
        }
    }

    /// Two successive calls with `grpc_port = 0` must both return `(None, [])`.
    /// Verifies idempotency of the disabled path (AC §2 deterministic).
    #[test]
    fn start_network_services_grpc_port_zero_is_idempotent() {
        for _ in 0..2 {
            let shared_state = make_shared_state();
            let ctx: SharedRuntimeContext = Arc::new(RuntimeContext::headless_default());
            let (
                rt,
                handles,
                _tx,
                _scroll_tx,
                _present_tx,
                _degradation_notices,
                _lease_expirations,
                _grpc_addr,
            ) = start_network_services(0, "psk", shared_state, ctx, false, false)
                .expect("port-0 must not error");
            assert!(rt.is_none());
            assert!(handles.is_empty());
        }
    }

    // These tests verify that start_network_services actually succeeds (does not
    // silently swallow a bind error). Each test allocates an ephemeral port via
    // TcpListener::bind(":0") so the OS picks a free port, eliminating port-
    // conflict flakiness in parallel CI runs.

    /// When `bind_all_interfaces = false`, `start_network_services` binds to
    /// `127.0.0.1` (loopback only) and must succeed.
    ///
    /// The bound address is determined by `select_grpc_bind_host` (separately
    /// pinned by the unit tests above). This test asserts that the full
    /// service startup path with the loopback bind host succeeds - not just
    /// that it doesn't error on an early-exit code path.
    #[test]
    fn start_network_services_loopback_default_binds_successfully() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .expect("failed to allocate ephemeral port for loopback bind test");
        let shared_state = make_shared_state();
        let ctx: SharedRuntimeContext = Arc::new(RuntimeContext::headless_default());
        let (rt, handles, _, _, _, _, _, _) =
            start_network_services(port, "psk", shared_state, ctx, false, false)
                .expect("loopback bind must succeed on a freshly allocated ephemeral port");
        assert!(rt.is_some(), "loopback bind must create a NetworkRuntime");
        assert!(!handles.is_empty(), "loopback bind must spawn task handles");
        for h in handles {
            h.abort();
        }
    }

    /// `start_network_services` with `bind_all_interfaces = true` binds on
    /// `0.0.0.0` (explicit opt-in for LAN/remote exposure) and must succeed.
    #[test]
    fn start_network_services_bind_all_interfaces_opt_in_binds_successfully() {
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .and_then(|l| l.local_addr())
            .map(|a| a.port())
            .expect("failed to allocate ephemeral port for all-interfaces bind test");
        let shared_state = make_shared_state();
        let ctx: SharedRuntimeContext = Arc::new(RuntimeContext::headless_default());
        let (rt, handles, _, _, _, _, _, _) =
            start_network_services(port, "psk", shared_state, ctx, false, true)
                .expect("all-interfaces bind must succeed on a freshly allocated ephemeral port");
        assert!(
            rt.is_some(),
            "all-interfaces bind must create a NetworkRuntime"
        );
        assert!(
            !handles.is_empty(),
            "all-interfaces bind must spawn task handles"
        );
        for h in handles {
            h.abort();
        }
    }

    /// Acceptance criterion 2: when no config TOML is provided, the runtime
    /// falls back to headless_default() with fallback_unrestricted = true.
    #[test]
    fn build_runtime_context_no_config_toml_uses_headless_default() {
        let cfg = WindowedConfig {
            config_toml: None,
            ..WindowedConfig::default()
        };
        let (ctx, fallback_unrestricted) = build_runtime_context(&cfg);
        // Fallback unrestricted should be true (dev-friendly default).
        assert!(
            fallback_unrestricted,
            "no-config path must set fallback_unrestricted=true"
        );
        // Profile name must be "headless" (headless_default behaviour).
        assert_eq!(
            ctx.profile.name, "headless",
            "no-config path must use the headless profile"
        );
        // Dev mode: an agent without a table is unrestricted.
        let any = ctx.agent_directory("psk").resolve("psk", "anyone").unwrap();
        assert!(any.allows("create_tiles"));
    }

    /// Acceptance criterion 1: when a valid config TOML is provided, each
    /// agent's `allow` list is parsed and applied.
    #[test]
    fn build_runtime_context_with_valid_config_applies_capability_grants() {
        let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"

[agents.weather-agent]
allow = ["tiles"]
"#;
        let cfg = WindowedConfig {
            config_toml: Some(toml.to_string()),
            ..WindowedConfig::default()
        };
        let (ctx, fallback_unrestricted) = build_runtime_context(&cfg);
        // Config-driven path: fallback must be Guest (not unrestricted).
        assert!(
            !fallback_unrestricted,
            "config-driven path must set fallback_unrestricted=false"
        );
        // Registered agent capabilities must be applied.
        let caps = ctx.agent_capabilities("weather-agent");
        assert!(
            caps.is_some(),
            "weather-agent must appear in the capability registry"
        );
        let caps = caps.unwrap();
        assert!(
            caps.contains(&"create_tiles".to_string()),
            "weather-agent must have create_tiles grant"
        );
        assert!(
            caps.contains(&"modify_own_tiles".to_string()),
            "weather-agent must have modify_own_tiles grant"
        );
        // An agent without a table gets nothing under the Guest fallback.
        let unknown = ctx
            .agent_directory("psk")
            .resolve("psk", "unknown-agent")
            .unwrap();
        assert!(unknown.permissions.is_empty());
    }

    /// Acceptance criterion 1: config-driven context uses the full-display profile.
    #[test]
    fn build_runtime_context_with_config_uses_configured_profile() {
        let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
        let cfg = WindowedConfig {
            config_toml: Some(toml.to_string()),
            ..WindowedConfig::default()
        };
        let (ctx, _) = build_runtime_context(&cfg);
        assert_eq!(
            ctx.profile.name, "full-display",
            "config-driven path must use the profile specified in the TOML"
        );
    }

    /// Acceptance criterion 3 (fallback): invalid TOML falls back to
    /// headless_default() rather than crashing.
    #[test]
    fn build_runtime_context_invalid_toml_falls_back_to_headless() {
        let bad_toml = "this is not valid TOML [\n";
        let cfg = WindowedConfig {
            config_toml: Some(bad_toml.to_string()),
            ..WindowedConfig::default()
        };
        let (ctx, fallback_unrestricted) = build_runtime_context(&cfg);
        // Must fall back gracefully to headless, but NOT unrestricted.
        // An operator who provided a config intended to restrict capabilities.
        assert!(
            !fallback_unrestricted,
            "parse-error path must NOT fall back to unrestricted"
        );
        assert_eq!(
            ctx.profile.name, "headless",
            "parse-error path must fall back to headless profile"
        );
    }

    /// Acceptance criterion 3 (fallback): config with validation errors falls
    /// back to headless_default() rather than crashing.
    #[test]
    fn build_runtime_context_validation_error_falls_back_to_headless() {
        // Missing required [[tabs]] section -> validation error.
        let invalid_toml = r#"
[runtime]
profile = "full-display"
"#;
        let cfg = WindowedConfig {
            config_toml: Some(invalid_toml.to_string()),
            ..WindowedConfig::default()
        };
        let (ctx, fallback_unrestricted) = build_runtime_context(&cfg);
        // Must fall back gracefully to headless, but NOT unrestricted.
        // An operator who provided a config intended to restrict capabilities.
        assert!(
            !fallback_unrestricted,
            "validation-error path must NOT fall back to unrestricted"
        );
        assert_eq!(
            ctx.profile.name, "headless",
            "validation-error path must fall back to headless profile"
        );
    }
}
