use std::path::PathBuf;

use crate::window::WindowConfig;

/// Bounded benchmark configuration for the real windowed compositor.
///
/// When present, the windowed runtime seeds a deterministic scene, records frame
/// telemetry after `warmup_frames`, writes a JSON artifact at `emit_path`, and
/// exits after `frames` measured frames.
#[derive(Debug, Clone)]
pub struct WindowedBenchmarkConfig {
    /// Number of warmup frames to render before recording measurements.
    pub warmup_frames: u64,
    /// Number of measured frames to include in the emitted artifact.
    pub frames: u64,
    /// Path to the per-mode benchmark JSON artifact.
    pub emit_path: PathBuf,
}

/// Bounded, event-driven quiescent-efficiency measurement for the real
/// windowed compositor.
///
/// Unlike [`WindowedBenchmarkConfig`], this mode never requests a render
/// cadence. The runtime first observes a real presentation, settles for the
/// mandatory five seconds, then measures the untouched overlay for sixty
/// seconds and emits the counter delta at `emit_path`.
#[derive(Debug, Clone)]
pub struct WindowedQuiescentEfficiencyConfig {
    /// Destination for the runtime-emitted artifact consumed by the CI gate.
    pub emit_path: PathBuf,
    /// Immutable build identity supplied by the application entry point.
    pub build: String,
}

/// Default agent identity presented for the resident gRPC portal session.
///
/// Kept in sync with the historical env-only enablement path, which hard-coded
/// this identity (see [`ResidentGrpcPortalSettings`]).
pub const DEFAULT_RESIDENT_GRPC_AGENT_ID: &str = "resident-grpc-portal";

/// Default lease TTL (ms) requested for the resident gRPC portal session.
///
/// Mirrors the bridge-side default so config-driven and env-driven enablement
/// request the same TTL when the operator does not override it.
pub const DEFAULT_RESIDENT_GRPC_LEASE_TTL_MS: u64 = 60_000;

/// Credential source for the resident gRPC portal bridge (hud-x2e2v).
///
/// The bridge may target a **separate** runtime (the external-authority
/// deployment model), so its authentication credential is decoupled from the
/// hosting runtime's own [`WindowedConfig::psk`]. The default reuses the
/// runtime PSK, preserving the loopback self-target behaviour from the env-only
/// enablement path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ResidentGrpcCredentialSource {
    /// Reuse the hosting runtime's [`WindowedConfig::psk`] (loopback default).
    #[default]
    RuntimePsk,
    /// An explicit pre-shared key for the (possibly external) target runtime.
    Psk(String),
}

/// First-class configuration for the resident gRPC portal bridge (hud-x2e2v).
///
/// Promotes the env-only enablement (`TZE_HUD_RESIDENT_GRPC_PORTAL`) to a
/// structured, operator-settable target so an **external-runtime** `HudSession`
/// can be addressed without env-var hacks — consistent with how `psk` and
/// `grpc_port` are already surfaced on [`WindowedConfig`].
///
/// Presence of this value (`Some`) enables the bridge, subject to the same
/// fail-closed gates as before: a resolvable endpoint, a live network runtime,
/// and a non-empty resolved credential. The `TZE_HUD_RESIDENT_GRPC_PORTAL` env
/// var remains a supported override that force-enables the bridge against this
/// runtime's own loopback gRPC server when this field is `None`.
///
/// Default-off is preserved because [`WindowedConfig::resident_grpc_portal`]
/// defaults to `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentGrpcPortalSettings {
    /// gRPC endpoint of the target `HudSession` server, e.g.
    /// `http://10.0.0.4:50051`.
    ///
    /// When `None`, the bridge derives `http://127.0.0.1:<grpc_port>` from the
    /// hosting runtime's own gRPC port (the historical loopback self-target).
    pub endpoint: Option<String>,
    /// Provider-neutral agent identity presented for the resident session.
    pub agent_id: String,
    /// Requested lease TTL in milliseconds.
    pub lease_ttl_ms: u64,
    /// Credential used to authenticate the resident session.
    pub credential: ResidentGrpcCredentialSource,
}

impl Default for ResidentGrpcPortalSettings {
    fn default() -> Self {
        Self {
            endpoint: None,
            agent_id: DEFAULT_RESIDENT_GRPC_AGENT_ID.to_string(),
            lease_ttl_ms: DEFAULT_RESIDENT_GRPC_LEASE_TTL_MS,
            credential: ResidentGrpcCredentialSource::RuntimePsk,
        }
    }
}

/// Configuration for the windowed runtime.
#[derive(Debug, Clone)]
pub struct WindowedConfig {
    /// Window configuration (mode, dimensions, title).
    ///
    /// The `mode` field controls whether the runtime starts in fullscreen or
    /// overlay/HUD mode. Use `WindowMode::Fullscreen` (default) for the
    /// compositor to own the entire display, or `WindowMode::Overlay` for a
    /// transparent, borderless, always-on-top window with per-region input
    /// passthrough.
    pub window: WindowConfig,
    /// When `true` and the window mode is `Overlay`, auto-detect the primary
    /// monitor resolution at startup and use it as the window dimensions.
    ///
    /// Explicit `--width`/`--height` flags (or `TZE_HUD_WINDOW_WIDTH` /
    /// `TZE_HUD_WINDOW_HEIGHT` env vars) set this to `false`, causing the
    /// configured `window.width`/`window.height` values to be used instead.
    ///
    /// Has no effect in fullscreen mode (fullscreen always uses the monitor's
    /// native resolution via `Fullscreen::Borderless`).
    ///
    /// Default: `true`.
    pub overlay_auto_size: bool,
    /// gRPC server port.  Set to `0` to disable the gRPC server.
    ///
    /// Both gRPC and MCP default to loopback-only binding (`127.0.0.1`) for
    /// security.  To expose them on all interfaces, set `bind_all_interfaces =
    /// true` or the `TZE_HUD_BIND_ALL_INTERFACES=1` environment variable.
    pub grpc_port: u16,
    /// MCP HTTP server port.  Set to `0` to disable the MCP server.
    ///
    /// The MCP server binds on `127.0.0.1` (loopback only) at the given port
    /// by default.  Set `bind_all_interfaces = true` (or
    /// `TZE_HUD_BIND_ALL_INTERFACES=1`) for `0.0.0.0` binding.  It enforces
    /// PSK authentication on every request via HTTP `Authorization: Bearer
    /// <psk>` or the JSON-RPC `_auth` param field.
    ///
    /// Default: 9090.
    pub mcp_port: u16,
    /// Bind gRPC and MCP servers on all interfaces (`0.0.0.0`) instead of
    /// loopback only (`127.0.0.1`).
    ///
    /// **Security opt-in (hud-1aswu.1).** The default is `false` — both
    /// servers bind loopback only, preventing LAN/tailnet access.  Set this
    /// to `true` only when you deliberately need remote-agent or cloud-relay
    /// access.  When enabled, all connections still require PSK authentication;
    /// `LocalSocketCredential` is additionally gated to loopback peers.
    ///
    /// Can also be set via the `TZE_HUD_BIND_ALL_INTERFACES=1` environment
    /// variable (overrides this field if set).
    ///
    /// Default: `false`.
    pub bind_all_interfaces: bool,
    /// Pre-shared key for session authentication (gRPC and MCP).
    pub psk: String,
    /// Optional operator-authority credential for cooperative projection cleanup.
    ///
    /// When unset, owner cleanup remains available through owner tokens, while
    /// operator cleanup stays fail-closed with `PROJECTION_UNAUTHORIZED`.
    pub projection_operator_authority: Option<String>,
    /// Target frames per second.  Default: 60.
    pub target_fps: u32,
    /// Raw TOML content of the configuration file, if one was loaded.
    ///
    /// When `Some`, the windowed runtime parses this at startup and applies the
    /// each `[agents.<id>]` allow list and PSK to the `RuntimeContext`.
    /// When `None`, the runtime falls back to `RuntimeContext::headless_default()`
    /// with every agent unrestricted (dev).
    ///
    /// ## Source
    ///
    /// Populated by the application binary when `resolve_config_path` succeeds:
    /// ```rust,ignore
    /// let config_path = resolve_config_path(opts.config_path.as_deref());
    /// let config_toml = config_path.ok().and_then(|p| std::fs::read_to_string(&p).ok());
    /// ```
    pub config_toml: Option<String>,
    /// Filesystem path of the loaded configuration file, if known.
    ///
    /// Used to resolve relative `[widget_bundles].paths` entries relative to the
    /// config file's parent directory (per spec §Widget Bundle Configuration).
    /// When `None`, relative paths are resolved from the current working directory.
    ///
    /// ## Source
    ///
    /// Populated by the application binary alongside `config_toml`:
    /// ```rust,ignore
    /// let config_path = resolve_config_path(opts.config_path.as_deref());
    /// if let Ok(ref p) = config_path {
    ///     config.config_file_path = Some(p.clone());
    ///     config.config_toml = std::fs::read_to_string(p).ok();
    /// }
    /// ```
    pub config_file_path: Option<String>,
    /// Render zone boundaries with colored debug tints.  Default: `false`.
    pub debug_zones: bool,
    /// Monitor index for overlay placement (0-based).  `None` = primary monitor.
    pub monitor_index: Option<usize>,
    /// Optional bounded benchmark run for the windowed compositor.
    pub benchmark: Option<WindowedBenchmarkConfig>,
    /// Optional bounded, event-driven quiescent-efficiency measurement.
    pub quiescent_efficiency: Option<WindowedQuiescentEfficiencyConfig>,
    /// First-class resident gRPC portal bridge configuration (hud-x2e2v).
    ///
    /// `None` (default) keeps the bridge **off** unless the
    /// `TZE_HUD_RESIDENT_GRPC_PORTAL` env var force-enables the loopback
    /// self-target path. `Some(..)` enables the bridge against the configured
    /// (possibly external) target, subject to the same fail-closed gates.
    pub resident_grpc_portal: Option<ResidentGrpcPortalSettings>,
}

impl Default for WindowedConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            overlay_auto_size: true,
            grpc_port: 50051,
            mcp_port: 9090,
            psk: "tze-hud-key".to_string(),
            projection_operator_authority: None,
            target_fps: 60,
            config_toml: None,
            config_file_path: None,
            debug_zones: false,
            monitor_index: None,
            benchmark: None,
            quiescent_efficiency: None,
            bind_all_interfaces: false,
            resident_grpc_portal: None,
        }
    }
}

/// Select the gRPC bind host based on the `bind_all_interfaces` flag.
///
/// Security fix (hud-1aswu.1): the default is loopback (`127.0.0.1`).
/// `0.0.0.0` (all interfaces) requires an explicit opt-in.
///
/// Extracted as a pure function to allow unit-testing of the bind-host
/// selection without spinning up a Tokio runtime or gRPC server (hud-stl9j).
pub(super) fn select_grpc_bind_host(bind_all_interfaces: bool) -> &'static str {
    if bind_all_interfaces {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

/// Decide whether the resident gRPC portal bridge is enabled (hud-x2e2v).
///
/// Default-off: enabled only when the operator supplied
/// [`WindowedConfig::resident_grpc_portal`] (`settings_present`) **or** set the
/// `TZE_HUD_RESIDENT_GRPC_PORTAL` env var (`env_enabled`). Extracted pure so the
/// enablement contract can be unit-tested without constructing a runtime.
pub(super) fn resident_grpc_bridge_enabled(settings_present: bool, env_enabled: bool) -> bool {
    settings_present || env_enabled
}

/// Resolve the resident gRPC portal bridge endpoint (hud-x2e2v).
///
/// A non-empty explicit operator-configured endpoint wins. An explicit endpoint
/// that is empty or whitespace-only is rejected (`None`) so the bridge fails
/// closed rather than dialing an invalid URI. Otherwise the loopback self-target
/// `http://127.0.0.1:<grpc_port>` is derived from the hosting runtime's own gRPC
/// port — but only when that port is non-zero. Returns `None` when there is no
/// usable explicit endpoint and no local gRPC server to derive one from (the
/// bridge then fails closed rather than dialing a dead port).
pub(super) fn resolve_resident_grpc_endpoint(
    explicit: Option<&str>,
    grpc_port: u16,
) -> Option<String> {
    match explicit {
        Some(endpoint) if !endpoint.trim().is_empty() => Some(endpoint.to_string()),
        Some(_) => None,
        None if grpc_port != 0 => Some(format!("http://127.0.0.1:{grpc_port}")),
        None => None,
    }
}

/// Resolve the credential the resident gRPC portal bridge presents (hud-x2e2v).
///
/// [`ResidentGrpcCredentialSource::RuntimePsk`] reuses the hosting runtime's
/// `psk` (loopback default); [`ResidentGrpcCredentialSource::Psk`] supplies a
/// separate secret for an external target runtime.
pub(super) fn resolve_resident_grpc_credential(
    source: &ResidentGrpcCredentialSource,
    runtime_psk: &str,
) -> String {
    match source {
        ResidentGrpcCredentialSource::RuntimePsk => runtime_psk.to_string(),
        ResidentGrpcCredentialSource::Psk(psk) => psk.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::{WindowMode, resolve_window_mode};

    #[test]
    fn windowed_config_default_values() {
        let cfg = WindowedConfig::default();
        assert_eq!(cfg.target_fps, 60);
        assert_eq!(cfg.grpc_port, 50051);
        assert_eq!(cfg.mcp_port, 9090);
        assert!(!cfg.psk.is_empty());
        assert!(cfg.benchmark.is_none());
        assert!(
            cfg.projection_operator_authority.is_none(),
            "operator cleanup must stay fail-closed unless explicitly configured"
        );
    }

    /// Default `WindowedConfig` must have `overlay_auto_size = true` so that
    /// overlay mode auto-detects the primary monitor resolution out-of-the-box.
    #[test]
    fn windowed_config_default_overlay_auto_size_is_true() {
        let cfg = WindowedConfig::default();
        assert!(
            cfg.overlay_auto_size,
            "overlay_auto_size must default to true so overlay covers the full monitor"
        );
    }

    /// `overlay_auto_size` can be explicitly disabled to respect user-provided
    /// `--width`/`--height` flags.
    #[test]
    fn windowed_config_overlay_auto_size_can_be_disabled() {
        let cfg = WindowedConfig {
            overlay_auto_size: false,
            ..WindowedConfig::default()
        };
        assert!(!cfg.overlay_auto_size);
    }

    /// When `overlay_auto_size` is false and mode is Overlay, the configured
    /// width/height values are respected (no monitor detection).
    #[test]
    fn windowed_config_overlay_explicit_dims_preserved() {
        let cfg = WindowedConfig {
            window: WindowConfig {
                mode: WindowMode::Overlay,
                width: 2560,
                height: 1440,
                title: "test".to_string(),
            },
            overlay_auto_size: false,
            ..WindowedConfig::default()
        };
        assert_eq!(cfg.window.width, 2560);
        assert_eq!(cfg.window.height, 1440);
        assert!(!cfg.overlay_auto_size);
    }

    #[test]
    fn windowed_config_default_mode_is_fullscreen() {
        let cfg = WindowedConfig::default();
        assert_eq!(
            cfg.window.mode,
            WindowMode::Fullscreen,
            "default mode must be fullscreen (spec §Window Modes)"
        );
    }

    #[test]
    fn windowed_config_overlay_mode_can_be_set() {
        let cfg = WindowedConfig {
            window: WindowConfig {
                mode: WindowMode::Overlay,
                width: 1280,
                height: 720,
                title: "test-overlay".to_string(),
            },
            ..WindowedConfig::default()
        };
        assert_eq!(cfg.window.mode, WindowMode::Overlay);
    }

    /// Verify that resolve_window_mode is called correctly for fullscreen
    /// (no fallback should ever occur for fullscreen).
    #[test]
    fn resolve_fullscreen_config_produces_fullscreen() {
        let (mode, reason) = resolve_window_mode(WindowMode::Fullscreen);
        assert_eq!(mode, WindowMode::Fullscreen);
        assert!(reason.is_none(), "fullscreen must never trigger a fallback");
    }

    /// Verify that resolve_window_mode for overlay either returns Overlay
    /// (if supported) or falls back to Fullscreen (GNOME Wayland), but never
    /// panics and always produces a valid mode.
    #[test]
    fn resolve_overlay_config_is_always_valid() {
        let (mode, _reason) = resolve_window_mode(WindowMode::Overlay);
        assert!(
            mode == WindowMode::Overlay || mode == WindowMode::Fullscreen,
            "resolved mode must be Overlay or Fullscreen, got: {mode}"
        );
    }

    #[test]
    fn windowed_config_title_is_non_empty_by_default() {
        let cfg = WindowedConfig::default();
        assert!(
            !cfg.window.title.is_empty(),
            "default title must be non-empty"
        );
    }

    #[test]
    fn windowed_config_dimensions_are_sensible_by_default() {
        let cfg = WindowedConfig::default();
        assert!(cfg.window.width > 0, "default width must be positive");
        assert!(cfg.window.height > 0, "default height must be positive");
    }

    /// `WindowedConfig` with `grpc_port = 0` reflects a "compositor-only" intent.
    /// Verify the config field is stored and readable (AC §2 — explicit disable).
    #[test]
    fn windowed_config_grpc_port_zero_is_compositor_only() {
        let cfg = WindowedConfig {
            grpc_port: 0,
            ..WindowedConfig::default()
        };
        assert_eq!(
            cfg.grpc_port, 0,
            "grpc_port=0 must be stored and readable as 0 (endpoint disabled)"
        );
    }

    /// `WindowedConfig` with `grpc_port = 50051` (default) signals network enabled.
    #[test]
    fn windowed_config_grpc_port_nonzero_enables_network() {
        let cfg = WindowedConfig::default();
        assert_ne!(
            cfg.grpc_port, 0,
            "default grpc_port must be non-zero (gRPC enabled by default)"
        );
    }

    /// `WindowedConfig::default()` must have `bind_all_interfaces = false`.
    ///
    /// Security gate: the default must never expose services on all interfaces
    /// (hud-1aswu.1).
    #[test]
    fn windowed_config_default_bind_all_interfaces_is_false() {
        let cfg = WindowedConfig::default();
        assert!(
            !cfg.bind_all_interfaces,
            "default must not bind all interfaces (security: hud-1aswu.1)"
        );
    }

    /// `select_grpc_bind_host(false)` must return `"127.0.0.1"` (loopback).
    ///
    /// Security gate (hud-1aswu.1): the default path — `bind_all_interfaces = false` —
    /// must select a loopback address.  A future change that swaps the arms would
    /// fail this test instead of silently exposing the service on all interfaces.
    #[test]
    fn select_grpc_bind_host_default_is_loopback() {
        let host = select_grpc_bind_host(false);
        let addr: std::net::IpAddr = host
            .parse()
            .expect("select_grpc_bind_host must return a valid IP string");
        assert!(
            addr.is_loopback(),
            "bind_all_interfaces=false must select a loopback address; got {host}"
        );
    }

    /// `select_grpc_bind_host(true)` must return `"0.0.0.0"` (all interfaces).
    ///
    /// Pins the opt-in path: explicit `bind_all_interfaces = true` must select
    /// `0.0.0.0`, not a loopback address.
    #[test]
    fn select_grpc_bind_host_all_interfaces_is_not_loopback() {
        let host = select_grpc_bind_host(true);
        let addr: std::net::IpAddr = host
            .parse()
            .expect("select_grpc_bind_host must return a valid IP string");
        assert!(
            !addr.is_loopback(),
            "bind_all_interfaces=true must select a non-loopback (all-interfaces) address; got {host}"
        );
        assert_eq!(
            host, "0.0.0.0",
            "bind_all_interfaces=true must return the all-interfaces sentinel 0.0.0.0"
        );
    }

    /// The two `select_grpc_bind_host` outputs are distinct — loopback and
    /// all-interfaces are not the same address.
    #[test]
    fn select_grpc_bind_host_outputs_are_distinct() {
        assert_ne!(
            select_grpc_bind_host(false),
            select_grpc_bind_host(true),
            "loopback and all-interfaces bind hosts must be different strings"
        );
    }

    /// Acceptance criterion 1: default WindowedConfig has no config_toml.
    #[test]
    fn windowed_config_default_has_no_config_toml() {
        let cfg = WindowedConfig::default();
        assert!(
            cfg.config_toml.is_none(),
            "default WindowedConfig must have config_toml = None"
        );
    }

    /// `WindowedConfig` built with 2560x1440 must preserve those dimensions
    /// exactly. Verifies that the config struct does not silently clamp or
    /// reject resolutions larger than the default 1920x1080.
    #[test]
    fn windowed_config_preserves_non_default_dimensions() {
        let cfg = WindowedConfig {
            window: WindowConfig {
                mode: WindowMode::Overlay,
                width: 2560,
                height: 1440,
                title: "tze_hud".to_string(),
            },
            ..WindowedConfig::default()
        };
        assert_eq!(
            cfg.window.width, 2560,
            "2560x1440 width must be preserved in WindowedConfig"
        );
        assert_eq!(
            cfg.window.height, 1440,
            "2560x1440 height must be preserved in WindowedConfig"
        );
    }

    /// `WindowedConfig` built with 3840x2160 (4K) must preserve those dimensions.
    #[test]
    fn windowed_config_preserves_4k_dimensions() {
        let cfg = WindowedConfig {
            window: WindowConfig {
                mode: WindowMode::Overlay,
                width: 3840,
                height: 2160,
                title: "tze_hud".to_string(),
            },
            ..WindowedConfig::default()
        };
        assert_eq!(cfg.window.width, 3840);
        assert_eq!(cfg.window.height, 2160);
    }

    // ── Resident gRPC portal bridge config plumbing (hud-x2e2v) ─────────────

    /// Default-off contract: a default `WindowedConfig` must not configure the
    /// resident gRPC portal bridge.
    #[test]
    fn windowed_config_default_resident_grpc_portal_is_none() {
        let cfg = WindowedConfig::default();
        assert!(
            cfg.resident_grpc_portal.is_none(),
            "resident gRPC portal bridge must default to OFF (None)"
        );
    }

    /// `ResidentGrpcPortalSettings::default()` pins the historical env-only
    /// values: loopback-derived endpoint (None), the canonical agent identity,
    /// the 60s lease TTL, and the runtime-PSK credential source.
    #[test]
    fn resident_grpc_portal_settings_default_values() {
        let s = ResidentGrpcPortalSettings::default();
        assert!(
            s.endpoint.is_none(),
            "default endpoint must be None (loopback self-target derived from grpc_port)"
        );
        assert_eq!(s.agent_id, DEFAULT_RESIDENT_GRPC_AGENT_ID);
        assert_eq!(s.lease_ttl_ms, DEFAULT_RESIDENT_GRPC_LEASE_TTL_MS);
        assert_eq!(s.credential, ResidentGrpcCredentialSource::RuntimePsk);
    }

    /// The enablement decision is OFF only when neither config settings nor the
    /// env var are present, and ON when either is.
    #[test]
    fn resident_grpc_bridge_enabled_truth_table() {
        assert!(
            !resident_grpc_bridge_enabled(false, false),
            "neither settings nor env → OFF (default)"
        );
        assert!(
            resident_grpc_bridge_enabled(true, false),
            "first-class settings → ON"
        );
        assert!(
            resident_grpc_bridge_enabled(false, true),
            "env override → ON"
        );
        assert!(resident_grpc_bridge_enabled(true, true), "both → ON");
    }

    /// An explicit operator endpoint is used verbatim (external-runtime target).
    #[test]
    fn resolve_resident_grpc_endpoint_prefers_explicit() {
        let resolved = resolve_resident_grpc_endpoint(Some("http://10.0.0.4:50051"), 50051);
        assert_eq!(resolved.as_deref(), Some("http://10.0.0.4:50051"));
    }

    /// With no explicit endpoint, the loopback self-target is derived from the
    /// runtime's own non-zero gRPC port (legacy env-path behaviour).
    #[test]
    fn resolve_resident_grpc_endpoint_derives_loopback_from_port() {
        let resolved = resolve_resident_grpc_endpoint(None, 50051);
        assert_eq!(resolved.as_deref(), Some("http://127.0.0.1:50051"));
    }

    /// With no explicit endpoint and the gRPC server disabled (`grpc_port == 0`)
    /// there is nothing to dial — resolution returns None so the bridge fails
    /// closed instead of targeting a dead port.
    #[test]
    fn resolve_resident_grpc_endpoint_none_when_no_port_and_no_explicit() {
        assert!(resolve_resident_grpc_endpoint(None, 0).is_none());
    }

    /// An explicit endpoint that is empty or whitespace-only is rejected so the
    /// bridge fails closed rather than dialing an invalid URI — even when a
    /// loopback fallback port is available.
    #[test]
    fn resolve_resident_grpc_endpoint_rejects_blank_explicit() {
        assert!(resolve_resident_grpc_endpoint(Some(""), 50051).is_none());
        assert!(resolve_resident_grpc_endpoint(Some("   "), 50051).is_none());
    }

    /// `RuntimePsk` reuses the hosting runtime's PSK.
    #[test]
    fn resolve_resident_grpc_credential_runtime_psk() {
        let psk = resolve_resident_grpc_credential(
            &ResidentGrpcCredentialSource::RuntimePsk,
            "runtime-secret",
        );
        assert_eq!(psk, "runtime-secret");
    }

    /// An explicit `Psk` credential is used independently of the runtime PSK,
    /// enabling authentication against a separate external runtime.
    #[test]
    fn resolve_resident_grpc_credential_explicit_psk_is_independent() {
        let psk = resolve_resident_grpc_credential(
            &ResidentGrpcCredentialSource::Psk("external-secret".to_string()),
            "runtime-secret",
        );
        assert_eq!(psk, "external-secret");
    }

    /// The settings are carried on `WindowedConfig` and survive struct-update
    /// construction (the wiring reads them back).
    #[test]
    fn windowed_config_carries_resident_grpc_portal_settings() {
        let cfg = WindowedConfig {
            resident_grpc_portal: Some(ResidentGrpcPortalSettings {
                endpoint: Some("http://192.168.1.20:50051".to_string()),
                agent_id: "external-portal".to_string(),
                lease_ttl_ms: 120_000,
                credential: ResidentGrpcCredentialSource::Psk("ext".to_string()),
            }),
            ..WindowedConfig::default()
        };
        let s = cfg
            .resident_grpc_portal
            .expect("settings must be present after construction");
        assert_eq!(s.endpoint.as_deref(), Some("http://192.168.1.20:50051"));
        assert_eq!(s.agent_id, "external-portal");
        assert_eq!(s.lease_ttl_ms, 120_000);
        assert_eq!(
            s.credential,
            ResidentGrpcCredentialSource::Psk("ext".to_string())
        );
    }
}
