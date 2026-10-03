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

/// How `POST /admin/restart` relaunches this process: the current exe and the
/// argv it was started with. Taken from the process itself, never from a request.
#[derive(Debug, Clone)]
pub struct Relaunch {
    pub exe: PathBuf,
    pub args: Vec<String>,
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
    /// gRPC and MCP listen on loopback plus the local Tailscale addresses only
    /// (see [`crate::net_addrs`]).
    pub grpc_port: u16,
    /// MCP HTTP server port.  Set to `0` to disable the MCP server.
    ///
    /// The MCP server listens on the same addresses as gRPC.  It enforces
    /// PSK authentication on every request via HTTP `Authorization: Bearer
    /// <psk>` or the JSON-RPC `_auth` param field.
    ///
    /// Default: 9090.
    pub mcp_port: u16,
    /// Paired agents (loaded from `agents.toml`), shared live by gRPC and
    /// MCP. Empty means no agent can authenticate until one is paired.
    pub agents: tze_hud_scene::config::SharedAgents,
    /// Target frames per second.  Default: 60.
    pub target_fps: u32,
    /// Raw TOML content of the configuration file, if one was loaded.
    ///
    /// When `Some`, the windowed runtime parses this at startup and builds the
    /// `RuntimeContext` from it. When `None`, the runtime falls back to
    /// `RuntimeContext::headless_default()` (dev).
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
    /// Enables `POST /admin/restart`. `None`: the endpoint answers 503.
    pub relaunch: Option<Relaunch>,
    /// Set when this instance was started with `--handoff <spec>` by a running
    /// one: report ready after the first frame, take over, then bind the ports.
    pub handoff: Option<crate::operator::handoff::HandoffChild>,
}

impl Default for WindowedConfig {
    fn default() -> Self {
        Self {
            window: WindowConfig::default(),
            overlay_auto_size: true,
            grpc_port: 50051,
            mcp_port: 9090,
            agents: Default::default(),
            target_fps: 60,
            config_toml: None,
            config_file_path: None,
            debug_zones: false,
            monitor_index: None,
            benchmark: None,
            quiescent_efficiency: None,
            relaunch: None,
            handoff: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::WindowMode;

    #[test]
    fn windowed_config_default_values() {
        let cfg = WindowedConfig::default();
        assert_eq!(cfg.target_fps, 60);
        assert_eq!(cfg.grpc_port, 50051);
        assert_eq!(cfg.mcp_port, 9090);
        assert!(
            cfg.agents.load().is_empty(),
            "no agent is paired by default"
        );
        assert!(cfg.benchmark.is_none());
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
