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
}
