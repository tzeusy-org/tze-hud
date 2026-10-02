#![windows_subsystem = "windows"]

//! # tze_hud — canonical runtime application binary
//!
//! This is the **production entrypoint** for the tze_hud windowed display runtime.
//! It is *not* a demo or example binary. Use this binary in deployment tooling and
//! operational automation.
//!
//! ## Startup options
//!
//! All options are available as CLI flags and, where applicable, as environment
//! variable overrides. Flags take priority over environment variables.
//!
//! | Flag                | Env var                | Default      | Description                              |
//! |---------------------|------------------------|--------------|------------------------------------------|
//! | `--config <path>`   | `TZE_HUD_CONFIG`       | (auto-resolved) | Path to TOML config file (`[runtime]` + `[[tabs]]` schema). |
//! | `--window-mode <m>` | `TZE_HUD_WINDOW_MODE`  | `fullscreen` | Window mode: `fullscreen` or `overlay`.  |
//! | `--width <px>`      | `TZE_HUD_WINDOW_WIDTH` | auto¹        | Window width in pixels.                  |
//! | `--height <px>`     | `TZE_HUD_WINDOW_HEIGHT`| auto¹        | Window height in pixels.                 |
//! | `--grpc-port <port>`| `TZE_HUD_GRPC_PORT`    | `50051`      | gRPC listen port (0 to disable).         |
//! | `--mcp-port <port>` | `TZE_HUD_MCP_PORT`     | `9090`       | MCP HTTP listen port (0 to disable).     |
//! | `--psk <key>`       | `TZE_HUD_PSK`          | `tze-hud-key`| Pre-shared key for session authentication.|
//! | —                    | `TZE_HUD_PROJECTION_OPERATOR_AUTHORITY` | unset | Operator credential for projection cleanup. |
//! | `--fps <n>`         | `TZE_HUD_FPS`          | `60`         | Target frames per second.                |
//! | `--bind-all-interfaces` | `TZE_HUD_BIND_ALL_INTERFACES` | `false` | Bind gRPC+MCP on `0.0.0.0` (LAN/remote opt-in; default is loopback). |
//! | `--benchmark-emit <path>` | `TZE_HUD_BENCHMARK_EMIT` | — | Emit bounded windowed benchmark JSON and exit. |
//! | `--benchmark-frames <n>` | `TZE_HUD_BENCHMARK_FRAMES` | `600` | Measured frames for benchmark mode. |
//! | `--benchmark-warmup-frames <n>` | `TZE_HUD_BENCHMARK_WARMUP_FRAMES` | `120` | Warmup frames skipped before measurement. |
//! | `--quiescent-efficiency-emit <path>` | `TZE_HUD_QUIESCENT_EFFICIENCY_EMIT` | — | Emit a real event-driven static-overlay efficiency artifact after 5s settle + 60s observation. |
//! | `--resident-grpc-portal` | `TZE_HUD_RESIDENT_GRPC_PORTAL=1` | — | Enable resident gRPC portal bridge with loopback defaults. |
//! | `--resident-grpc-endpoint <url>` | `TZE_HUD_RESIDENT_GRPC_ENDPOINT` | — | Target endpoint for the resident bridge (e.g. `http://10.0.0.4:50051`). Implies `--resident-grpc-portal`. |
//! | `--resident-grpc-agent-id <id>` | `TZE_HUD_RESIDENT_GRPC_AGENT_ID` | `resident-grpc-portal` | Agent identity for the resident session. Implies `--resident-grpc-portal`. |
//! | `--resident-grpc-lease-ttl <ms>` | `TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS` | `60000` | Requested lease TTL in milliseconds. Implies `--resident-grpc-portal`. |
//! | `--resident-grpc-psk <key>` | `TZE_HUD_RESIDENT_GRPC_PSK` | — | Explicit PSK for the target runtime (omit to reuse the runtime PSK). Implies `--resident-grpc-portal`. |
//! | `--print-attach-info` | —                    | —            | Print the MCP attach-info block (endpoint URL, resident-principal==PSK rule, paste-ready MCP client config) and exit 0 without starting the runtime. Never prints the PSK. |
//! | `--help`            | —                      | —            | Print this help and exit.                |
//! | `--version`         | —                      | —            | Print version and exit.                  |
//!
//! ¹ In overlay mode, the primary monitor resolution is auto-detected at startup
//!   via winit. Falls back to `1920` (width) / `1080` (height) if detection fails
//!   (headless environment, no display server). Explicit `--width`/`--height` flags
//!   or `TZE_HUD_WINDOW_WIDTH`/`TZE_HUD_WINDOW_HEIGHT` env vars override
//!   auto-detection. In fullscreen mode, `1920×1080` is the default (the compositor
//!   uses `Fullscreen::Borderless`, which always uses the monitor's native resolution).
//!
//! ## Config file resolution order
//!
//! 1. `--config <path>` CLI flag
//! 2. `$TZE_HUD_CONFIG` environment variable
//! 3. `./tze_hud.toml` in the current working directory
//! 4. `$XDG_CONFIG_HOME/tze_hud/config.toml` (Linux/macOS)
//! 5. `%APPDATA%\tze_hud\config.toml` (Windows)
//!
//! The loader schema is driven by `[runtime]` and `[[tabs]]` (plus optional
//! sections such as `[agents]`, `[widget_bundles]`, and `[design_tokens]`).
//! Legacy `[display]`/`[network]` config tables are not part of the current schema.
//!
//! In the canonical operator path, startup is fail-closed: a readable, valid
//! config file is required and the trivial default PSK is rejected. Debug/dev
//! runs may explicitly opt into insecure fallback behavior by setting
//! `TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1`.
//! Passing `--config` with a path that does not exist or cannot be read is a
//! hard error.
//!
//! ## Examples
//!
//! ```sh
//! # Fullscreen (default)
//! tze_hud
//!
//! # Overlay mode at 1280×720 with gRPC enabled
//! tze_hud --window-mode overlay --width 1280 --height 720 --grpc-port 50051
//!
//! # Load explicit config file
//! tze_hud --config /etc/tze_hud/config.toml
//!
//! # Disable gRPC (standalone compositor only)
//! tze_hud --grpc-port 0
//! ```

use tze_hud_config::{reload_config, resolve_config_path};
use tze_hud_runtime::gpu_lock::GpuLock;
use tze_hud_runtime::window::{WindowConfig, WindowMode};
use tze_hud_runtime::windowed::{
    ResidentGrpcCredentialSource, ResidentGrpcPortalSettings, WindowedBenchmarkConfig,
    WindowedConfig, WindowedQuiescentEfficiencyConfig, WindowedRuntime,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("TZE_HUD_GIT_SHA");
const BIN_NAME: &str = "tze_hud";
const DEFAULT_PSK: &str = "tze-hud-key";
const DEV_ALLOW_INSECURE_STARTUP_ENV: &str = "TZE_HUD_DEV_ALLOW_INSECURE_STARTUP";
const PROJECTION_OPERATOR_AUTHORITY_ENV: &str = "TZE_HUD_PROJECTION_OPERATOR_AUTHORITY";

fn print_help() {
    println!(
        r#"{BIN_NAME} {VERSION} ({GIT_SHA})
Canonical tze_hud windowed display runtime.

USAGE:
    {BIN_NAME} [OPTIONS]

OPTIONS:
    --config <path>        Path to TOML config file
                           (env: TZE_HUD_CONFIG; auto-resolved if omitted)
    --window-mode <mode>   Window mode: fullscreen | overlay  [default: fullscreen]
                           (env: TZE_HUD_WINDOW_MODE)
    --width <px>           Window width in pixels  [default: auto-detect in overlay mode, 1920 otherwise]
                           (env: TZE_HUD_WINDOW_WIDTH)
    --height <px>          Window height in pixels  [default: auto-detect in overlay mode, 1080 otherwise]
                           (env: TZE_HUD_WINDOW_HEIGHT)
    --grpc-port <port>     gRPC listen port; 0 to disable  [default: 50051]
                           (env: TZE_HUD_GRPC_PORT)
    --mcp-port <port>      MCP HTTP listen port; 0 to disable  [default: 9090]
                           (env: TZE_HUD_MCP_PORT)
    --psk <key>            Pre-shared key for session authentication  [default: tze-hud-key]
                           (env: TZE_HUD_PSK)
    (env only) TZE_HUD_PROJECTION_OPERATOR_AUTHORITY
                           Operator credential for cooperative projection cleanup.
                           When unset, operator cleanup is denied fail-closed.
    --fps <n>              Target frames per second  [default: 60]
                           (env: TZE_HUD_FPS)
    --bind-all-interfaces  Bind gRPC+MCP on 0.0.0.0 (LAN/remote opt-in; default: 127.0.0.1)
                           (env: TZE_HUD_BIND_ALL_INTERFACES=1)
    --benchmark-emit <path>
                           Emit bounded windowed compositor benchmark JSON and exit
                           (env: TZE_HUD_BENCHMARK_EMIT)
    --benchmark-frames <n> Measured frames for benchmark mode  [default: 600]
                           (env: TZE_HUD_BENCHMARK_FRAMES)
    --benchmark-warmup-frames <n>
                           Warmup frames skipped before measurement  [default: 120]
                           (env: TZE_HUD_BENCHMARK_WARMUP_FRAMES)
    --quiescent-efficiency-emit <path>
                           Measure a static event-driven windowed runtime for a
                           mandatory 5s settle plus 60s interval, emit JSON, and exit.
                           Requires a constrained software-renderer CI invocation.
                           (env: TZE_HUD_QUIESCENT_EFFICIENCY_EMIT)
    --resident-grpc-portal Enable the resident gRPC portal bridge with loopback defaults
                           (env: TZE_HUD_RESIDENT_GRPC_PORTAL=1)
    --resident-grpc-endpoint <url>
                           Target gRPC endpoint for the resident bridge, e.g.
                           http://10.0.0.4:50051  (implies --resident-grpc-portal)
                           (env: TZE_HUD_RESIDENT_GRPC_ENDPOINT)
    --resident-grpc-agent-id <id>
                           Agent identity for the resident session
                           [default: resident-grpc-portal]  (implies --resident-grpc-portal)
                           (env: TZE_HUD_RESIDENT_GRPC_AGENT_ID)
    --resident-grpc-lease-ttl <ms>
                           Requested lease TTL in milliseconds  [default: 60000]
                           (implies --resident-grpc-portal)
                           (env: TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS)
    --resident-grpc-psk <key>
                           Explicit PSK for the target runtime; omit to reuse the runtime PSK
                           (implies --resident-grpc-portal)
                           (env: TZE_HUD_RESIDENT_GRPC_PSK)
    --print-attach-info    Print the MCP attach-info block (endpoint URL, the
                           resident-principal == PSK rule, and a paste-ready MCP
                           client config snippet) and exit 0 WITHOUT starting the
                           runtime. Honours --config / --mcp-port / --grpc-port /
                           --bind-all-interfaces so the printed info matches the
                           runtime it describes. Never prints the PSK value.
    --help                 Print this help and exit
    --version              Print version and exit

NOTES:
    This binary is the canonical production entrypoint for tze_hud. It starts
    the windowed display runtime with a real wgpu swapchain and winit event loop.
    For headless/CI usage, use the tze_hud_runtime crate directly with
    HeadlessRuntime.

    Canonical startup is fail-closed: a readable, valid config file is required.
    Strict mode also rejects the trivial default PSK value.
    For debug/dev runs only, set TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1 to permit
    fallback startup behavior without a config file. In canonical startup, the
    required config file uses the loader schema rooted at [runtime] and [[tabs]]
    (plus optional sections such as [agents], [widget_bundles], and
    [design_tokens]). In insecure dev mode, the same schema applies when a
    config file is provided. Legacy [display]/[network] tables are unsupported.
    CLI flags override individual settings from the config file.
    Passing --config with a path that does not exist or cannot be read is an error.
"#,
    );
}

fn print_version() {
    println!("{BIN_NAME} {VERSION} ({GIT_SHA})");
}

/// Parsed startup options.
#[derive(Debug)]
struct StartupOptions {
    config_path: Option<String>,
    window_mode: WindowMode,
    width: u32,
    height: u32,
    /// Whether `width` was explicitly set via `--width` or `TZE_HUD_WINDOW_WIDTH`.
    ///
    /// When `false` (the default), overlay mode auto-detects the primary monitor
    /// resolution at startup and ignores the default `width` value.
    explicit_width: bool,
    /// Whether `height` was explicitly set via `--height` or `TZE_HUD_WINDOW_HEIGHT`.
    ///
    /// When `false` (the default), overlay mode auto-detects the primary monitor
    /// resolution at startup and ignores the default `height` value.
    explicit_height: bool,
    grpc_port: u16,
    mcp_port: u16,
    psk: String,
    /// Optional operator credential used only for cooperative projection cleanup.
    projection_operator_authority: Option<String>,
    fps: u32,
    /// Bind gRPC and MCP servers on all interfaces (`0.0.0.0`) instead of
    /// loopback only (`127.0.0.1`).
    ///
    /// Security opt-in (hud-1aswu.1): default is loopback-only.  Set this
    /// flag or `TZE_HUD_BIND_ALL_INTERFACES=1` to allow LAN/remote access.
    bind_all_interfaces: bool,
    /// When true, render zone boundaries with colored debug tints.
    debug_zones: bool,
    /// Monitor index for overlay placement (0-based). `None` = primary monitor.
    monitor_index: Option<usize>,
    /// Path for bounded windowed compositor benchmark output.
    benchmark_emit: Option<String>,
    /// Number of measured frames in benchmark mode.
    benchmark_frames: u64,
    /// Number of warmup frames skipped before benchmark measurement.
    benchmark_warmup_frames: u64,
    /// Path for a bounded real-runtime quiescent-efficiency artifact.
    quiescent_efficiency_emit: Option<String>,
    /// Enable the resident gRPC portal bridge (hud-ev2lr).
    ///
    /// Set by `--resident-grpc-portal`, any `--resident-grpc-*` sub-flag, or any
    /// `TZE_HUD_RESIDENT_GRPC_*` env var. When false and none of the sub-fields
    /// are populated, the bridge stays off (default).
    resident_grpc_portal_enabled: bool,
    /// Optional explicit target endpoint for the bridge.
    resident_grpc_endpoint: Option<String>,
    /// Agent identity presented for the resident session.
    resident_grpc_agent_id: String,
    /// Requested lease TTL in milliseconds.
    resident_grpc_lease_ttl_ms: u64,
    /// Explicit PSK for the target runtime.  `None` → reuse the runtime PSK.
    resident_grpc_psk: Option<String>,
    /// When true, print the MCP attach-info block (endpoint URL, the
    /// resident-principal == PSK rule, and a paste-ready MCP client config
    /// snippet) and exit 0 *without* starting the runtime (hud-b7c0m).
    print_attach_info: bool,
}

impl Default for StartupOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            window_mode: WindowMode::Fullscreen,
            width: 1920,
            height: 1080,
            explicit_width: false,
            explicit_height: false,
            grpc_port: 50051,
            mcp_port: 9090,
            psk: DEFAULT_PSK.to_string(),
            projection_operator_authority: None,
            fps: 60,
            bind_all_interfaces: false,
            debug_zones: false,
            monitor_index: None,
            benchmark_emit: None,
            benchmark_frames: 600,
            benchmark_warmup_frames: 120,
            quiescent_efficiency_emit: None,
            resident_grpc_portal_enabled: false,
            resident_grpc_endpoint: None,
            resident_grpc_agent_id: tze_hud_runtime::windowed::DEFAULT_RESIDENT_GRPC_AGENT_ID
                .to_string(),
            resident_grpc_lease_ttl_ms:
                tze_hud_runtime::windowed::DEFAULT_RESIDENT_GRPC_LEASE_TTL_MS,
            resident_grpc_psk: None,
            print_attach_info: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupSecurityMode {
    Strict,
    DevInsecureOverride,
}

fn startup_security_mode_for_env(
    dev_override_env: Option<&str>,
    is_debug_build: bool,
) -> StartupSecurityMode {
    if is_debug_build && dev_override_env == Some("1") {
        StartupSecurityMode::DevInsecureOverride
    } else {
        StartupSecurityMode::Strict
    }
}

fn startup_security_mode() -> StartupSecurityMode {
    startup_security_mode_for_env(
        std::env::var(DEV_ALLOW_INSECURE_STARTUP_ENV)
            .ok()
            .as_deref(),
        cfg!(debug_assertions),
    )
}

fn psk_is_trivial_default(psk: &str) -> bool {
    psk == DEFAULT_PSK
}

fn validate_config_toml_for_startup(toml_src: &str) -> Result<(), String> {
    reload_config(toml_src).map(|_| ()).map_err(|errors| {
        let mut rendered = String::new();
        for (idx, err) in errors.iter().enumerate() {
            if idx > 0 {
                rendered.push_str("; ");
            }
            rendered.push_str(&format!(
                "[{:?}] {} (expected: {}, got: {}, hint: {})",
                err.code, err.field_path, err.expected, err.got, err.hint
            ));
        }
        format!(
            "config validation failed with {} error(s): {}",
            errors.len(),
            rendered
        )
    })
}

fn parse_benchmark_emit_path(value: String, source: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err(format!("{source} requires a non-empty path"))
    } else {
        Ok(value)
    }
}

/// Parse startup options from CLI arguments and environment variables.
///
/// CLI flags take priority over environment variables.
fn parse_options(args: &[String]) -> Result<StartupOptions, String> {
    let mut opts = StartupOptions::default();

    // Apply environment variables first (lowest priority).
    if let Ok(v) = std::env::var("TZE_HUD_WINDOW_MODE") {
        opts.window_mode = parse_window_mode(&v)?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_WINDOW_WIDTH") {
        opts.width = v
            .parse::<u32>()
            .map_err(|_| format!("TZE_HUD_WINDOW_WIDTH: invalid integer: {v:?}"))?;
        opts.explicit_width = true;
    }
    if let Ok(v) = std::env::var("TZE_HUD_WINDOW_HEIGHT") {
        opts.height = v
            .parse::<u32>()
            .map_err(|_| format!("TZE_HUD_WINDOW_HEIGHT: invalid integer: {v:?}"))?;
        opts.explicit_height = true;
    }
    if let Ok(v) = std::env::var("TZE_HUD_GRPC_PORT") {
        opts.grpc_port = v
            .parse::<u16>()
            .map_err(|_| format!("TZE_HUD_GRPC_PORT: invalid port: {v:?}"))?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_MCP_PORT") {
        opts.mcp_port = v
            .parse::<u16>()
            .map_err(|_| format!("TZE_HUD_MCP_PORT: invalid port: {v:?}"))?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_PSK") {
        opts.psk = v;
    }
    if let Ok(v) = std::env::var(PROJECTION_OPERATOR_AUTHORITY_ENV) {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{PROJECTION_OPERATOR_AUTHORITY_ENV} requires a non-empty value"
            ));
        }
        opts.projection_operator_authority = Some(trimmed.to_string());
    }
    if let Ok(v) = std::env::var("TZE_HUD_BIND_ALL_INTERFACES") {
        // Security opt-in (hud-1aswu.1): "1" or "true" (case-insensitive).
        opts.bind_all_interfaces = v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Ok(v) = std::env::var("TZE_HUD_FPS") {
        opts.fps = v
            .parse::<u32>()
            .map_err(|_| format!("TZE_HUD_FPS: invalid integer: {v:?}"))?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_BENCHMARK_EMIT") {
        opts.benchmark_emit = Some(parse_benchmark_emit_path(v, "TZE_HUD_BENCHMARK_EMIT")?);
    }
    if let Ok(v) = std::env::var("TZE_HUD_BENCHMARK_FRAMES") {
        opts.benchmark_frames = v
            .parse::<u64>()
            .map_err(|_| format!("TZE_HUD_BENCHMARK_FRAMES: invalid integer: {v:?}"))?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_BENCHMARK_WARMUP_FRAMES") {
        opts.benchmark_warmup_frames = v
            .parse::<u64>()
            .map_err(|_| format!("TZE_HUD_BENCHMARK_WARMUP_FRAMES: invalid integer: {v:?}"))?;
    }
    if let Ok(v) = std::env::var("TZE_HUD_QUIESCENT_EFFICIENCY_EMIT") {
        opts.quiescent_efficiency_emit = Some(parse_benchmark_emit_path(
            v,
            "TZE_HUD_QUIESCENT_EFFICIENCY_EMIT",
        )?);
    }
    if let Ok(v) = std::env::var("TZE_HUD_RESIDENT_GRPC_PORTAL") {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            opts.resident_grpc_portal_enabled = true;
        }
    }
    if let Ok(v) = std::env::var("TZE_HUD_RESIDENT_GRPC_ENDPOINT") {
        opts.resident_grpc_endpoint = Some(v);
        opts.resident_grpc_portal_enabled = true;
    }
    if let Ok(v) = std::env::var("TZE_HUD_RESIDENT_GRPC_AGENT_ID") {
        opts.resident_grpc_agent_id = v;
        opts.resident_grpc_portal_enabled = true;
    }
    if let Ok(v) = std::env::var("TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS") {
        opts.resident_grpc_lease_ttl_ms = v
            .parse::<u64>()
            .map_err(|_| format!("TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS: invalid integer: {v:?}"))?;
        opts.resident_grpc_portal_enabled = true;
    }
    if let Ok(v) = std::env::var("TZE_HUD_RESIDENT_GRPC_PSK") {
        opts.resident_grpc_psk = Some(v);
        opts.resident_grpc_portal_enabled = true;
    }

    // Parse CLI flags (override env vars).
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                // The console is already attached once at the top of `main`
                // (hud-q2glv), so the help text reaches the launching terminal
                // on Windows without a per-arm attach here.
                print_help();
                // `process::exit` skips destructors, so flush the buffered
                // text first — matching the `--print-attach-info` pattern
                // below (Gemini review on PR #1143).
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                print_version();
                use std::io::Write;
                let _ = std::io::stdout().flush();
                std::process::exit(0);
            }
            "--print-attach-info" => {
                // Handled after parsing completes (main), so all attach-relevant
                // flags (--config, --mcp-port, --grpc-port, --bind-all-interfaces)
                // are already applied. Does not start the runtime.
                opts.print_attach_info = true;
            }
            "--config" => {
                i += 1;
                opts.config_path = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| "--config requires a path argument".to_string())?,
                );
            }
            "--window-mode" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| {
                    "--window-mode requires an argument: fullscreen | overlay".to_string()
                })?;
                opts.window_mode = parse_window_mode(val)?;
            }
            "--width" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--width requires a pixel count argument".to_string())?;
                opts.width = val
                    .parse::<u32>()
                    .map_err(|_| format!("--width: invalid integer: {val:?}"))?;
                opts.explicit_width = true;
            }
            "--height" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--height requires a pixel count argument".to_string())?;
                opts.height = val
                    .parse::<u32>()
                    .map_err(|_| format!("--height: invalid integer: {val:?}"))?;
                opts.explicit_height = true;
            }
            "--grpc-port" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--grpc-port requires a port number argument".to_string())?;
                opts.grpc_port = val
                    .parse::<u16>()
                    .map_err(|_| format!("--grpc-port: invalid port: {val:?}"))?;
            }
            "--mcp-port" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--mcp-port requires a port number argument".to_string())?;
                opts.mcp_port = val
                    .parse::<u16>()
                    .map_err(|_| format!("--mcp-port: invalid port: {val:?}"))?;
            }
            "--psk" => {
                i += 1;
                opts.psk = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--psk requires a key argument".to_string())?;
            }
            "--fps" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--fps requires an integer argument".to_string())?;
                opts.fps = val
                    .parse::<u32>()
                    .map_err(|_| format!("--fps: invalid integer: {val:?}"))?;
            }
            "--bind-all-interfaces" => {
                // Security opt-in (hud-1aswu.1): bind gRPC and MCP on 0.0.0.0.
                opts.bind_all_interfaces = true;
            }
            "--debug-zones" => {
                opts.debug_zones = true;
            }
            "--monitor" => {
                i += 1;
                let val = args
                    .get(i)
                    .ok_or_else(|| "--monitor requires a monitor index (0-based)".to_string())?;
                opts.monitor_index = Some(
                    val.parse::<usize>()
                        .map_err(|_| format!("--monitor: invalid index: {val:?}"))?,
                );
            }
            "--benchmark-emit" => {
                i += 1;
                let path = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| "--benchmark-emit requires a path argument".to_string())?;
                opts.benchmark_emit = Some(parse_benchmark_emit_path(path, "--benchmark-emit")?);
            }
            "--benchmark-frames" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| {
                    "--benchmark-frames requires a frame count argument".to_string()
                })?;
                opts.benchmark_frames = val
                    .parse::<u64>()
                    .map_err(|_| format!("--benchmark-frames: invalid integer: {val:?}"))?;
            }
            "--benchmark-warmup-frames" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| {
                    "--benchmark-warmup-frames requires a frame count argument".to_string()
                })?;
                opts.benchmark_warmup_frames = val
                    .parse::<u64>()
                    .map_err(|_| format!("--benchmark-warmup-frames: invalid integer: {val:?}"))?;
            }
            "--quiescent-efficiency-emit" => {
                i += 1;
                let path = args.get(i).cloned().ok_or_else(|| {
                    "--quiescent-efficiency-emit requires a path argument".to_string()
                })?;
                opts.quiescent_efficiency_emit = Some(parse_benchmark_emit_path(
                    path,
                    "--quiescent-efficiency-emit",
                )?);
            }
            "--resident-grpc-portal" => {
                opts.resident_grpc_portal_enabled = true;
            }
            "--resident-grpc-endpoint" => {
                i += 1;
                opts.resident_grpc_endpoint = Some(args.get(i).cloned().ok_or_else(|| {
                    "--resident-grpc-endpoint requires a URL argument".to_string()
                })?);
                opts.resident_grpc_portal_enabled = true;
            }
            "--resident-grpc-agent-id" => {
                i += 1;
                opts.resident_grpc_agent_id = args.get(i).cloned().ok_or_else(|| {
                    "--resident-grpc-agent-id requires an id argument".to_string()
                })?;
                opts.resident_grpc_portal_enabled = true;
            }
            "--resident-grpc-lease-ttl" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| {
                    "--resident-grpc-lease-ttl requires a millisecond count argument".to_string()
                })?;
                opts.resident_grpc_lease_ttl_ms = val
                    .parse::<u64>()
                    .map_err(|_| format!("--resident-grpc-lease-ttl: invalid integer: {val:?}"))?;
                opts.resident_grpc_portal_enabled = true;
            }
            "--resident-grpc-psk" => {
                i += 1;
                opts.resident_grpc_psk =
                    Some(args.get(i).cloned().ok_or_else(|| {
                        "--resident-grpc-psk requires a key argument".to_string()
                    })?);
                opts.resident_grpc_portal_enabled = true;
            }
            flag if flag.starts_with('-') => {
                return Err(format!(
                    "unknown flag: {flag}\nRun '{BIN_NAME} --help' for usage."
                ));
            }
            _ => {
                return Err(format!(
                    "unexpected positional argument: {}\nRun '{BIN_NAME} --help' for usage.",
                    args[i]
                ));
            }
        }
        i += 1;
    }

    Ok(opts)
}

/// Build resident gRPC portal settings from parsed startup options (hud-ev2lr).
///
/// Returns `None` when the bridge is disabled (default). Otherwise assembles
/// `ResidentGrpcPortalSettings` from the options, applying `RuntimePsk` when
/// no explicit PSK was provided.
fn build_resident_grpc_portal_settings(
    opts: &StartupOptions,
) -> Option<ResidentGrpcPortalSettings> {
    if !opts.resident_grpc_portal_enabled {
        return None;
    }
    Some(ResidentGrpcPortalSettings {
        endpoint: opts.resident_grpc_endpoint.clone(),
        agent_id: opts.resident_grpc_agent_id.clone(),
        lease_ttl_ms: opts.resident_grpc_lease_ttl_ms,
        credential: opts
            .resident_grpc_psk
            .clone()
            .map(ResidentGrpcCredentialSource::Psk)
            .unwrap_or(ResidentGrpcCredentialSource::RuntimePsk),
    })
}

/// Returns a copy of `args` with the value following `--resident-grpc-psk` replaced
/// by `<redacted>` so startup diagnostic logs never capture the secret in plain text.
fn redact_sensitive_args(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--resident-grpc-psk" {
            out.push(args[i].clone());
            i += 1;
            if i < args.len() {
                out.push("<redacted>".to_string());
            }
        } else {
            out.push(args[i].clone());
        }
        i += 1;
    }
    out
}

fn parse_window_mode(s: &str) -> Result<WindowMode, String> {
    match s.to_lowercase().as_str() {
        "fullscreen" => Ok(WindowMode::Fullscreen),
        "overlay" => Ok(WindowMode::Overlay),
        other => Err(format!(
            "unknown window mode: {other:?}; expected \"fullscreen\" or \"overlay\""
        )),
    }
}

/// On Windows, reattach the process's standard handles to the launching
/// terminal's console (hud-b7c0m; Codex P2 on PR #1112).
///
/// This binary is a GUI-subsystem app (`#![windows_subsystem = "windows"]`), so a
/// launch from PowerShell/cmd gives the process *no* console and `print!` output
/// is silently discarded unless the caller redirected the standard handles.
/// `AttachConsole(ATTACH_PARENT_PROCESS)` binds the standard handles to the
/// parent's console, but only when a handle is not already set — so an explicit
/// redirect (`tze_hud --print-attach-info > info.txt`) is preserved. It is a
/// harmless no-op when there is no parent console (e.g. a double-click launch,
/// which has no terminal to show the block on anyway).
///
/// Called ONCE at the very top of `main` (hud-q2glv). A single early attach
/// covers every output path — `--help`/`--version`, `--print-attach-info`, the
/// `tracing` log stream, and every startup `eprintln!` + `exit(1)` failure —
/// so no per-path call is needed. It never `AllocConsole`s, so the double-click
/// happy path shows no flashing console window.
///
/// Empirically verified live on the hud-windows VM testhost (hud-xssgy):
/// `AttachConsole` alone — with no `CONOUT$`/`CONIN$` reopen or `SetStdHandle`
/// call, which the canonical hybrid console/GUI C/C++ pattern normally
/// requires — is sufficient for Rust's `println!`/`eprintln!` here. A prior
/// review flagged this as unconfirmed (PR #1112 only verified the FFI links
/// compiled, not that output actually became visible). Live-tested `--version`,
/// `--help`, and an invalid-flag error path from both a real interactive
/// `cmd.exe` and `powershell.exe` session (no redirection) against this exact
/// binary: all three printed correctly in both shells. An A/B negative control
/// (same matrix, same binary, with this call's only production call site
/// disabled) confirmed the output is silent without it — this call is the
/// actual cause, not a coincidence of the test setup. The likely reason the
/// classic C/C++ `freopen` step is unnecessary here: Rust's `Stdout`/`Stderr`
/// resolve `GetStdHandle` lazily on first use (no CRT-buffered `FILE*` bound
/// at process start), and that first use happens after this call already ran,
/// by which point `GetStdHandle` resolves against the newly-attached console.
#[cfg(windows)]
fn attach_parent_console() {
    // (DWORD)-1 — attach to the console of the parent process.
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    // Safety: FFI call into kernel32 with a constant argument; it touches no
    // memory we own and is defined to no-op / fail cleanly when the process
    // already has (or has no) console.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Non-Windows platforms already run the fast path on a normal console; nothing
/// to attach.
#[cfg(not(windows))]
#[inline]
fn attach_parent_console() {}

/// On Windows, ignore CTRL+C for the calling process (hud-q2glv, Codex P2 on
/// PR #1143).
///
/// Before hoisting `attach_parent_console` to run unconditionally at the top
/// of `main`, the console attach only ever happened on a fast-exit path
/// (`--help`/`--version`/`--print-attach-info`), each immediately followed by
/// `process::exit`, so the process was attached to the launching terminal's
/// console for a negligible window. Now the long-running canonical runtime
/// path attaches too and stays attached for the process's entire lifetime.
/// Windows delivers CTRL_C_EVENT to every process sharing a console by
/// default, and this binary installs no handler of its own — so an operator
/// pressing Ctrl+C in that terminal for an unrelated reason (or out of habit)
/// could kill the running HUD process, which never had this failure mode
/// before (a GUI-subsystem process has no console at all by default, so it
/// was never Ctrl+C-reachable in the first place).
///
/// `SetConsoleCtrlHandler(NULL, TRUE)` is the documented WinAPI idiom for
/// "ignore CTRL+C input for the calling process" — it affects CTRL_C_EVENT
/// only. CTRL_BREAK_EVENT, window-close, logoff, and shutdown console events
/// are untouched and keep their OS default handling, so the process remains
/// stoppable through those paths; this narrowly restores the pre-hud-q2glv
/// invariant (immune to console Ctrl+C) rather than removing an existing
/// shutdown mechanism (none existed).
#[cfg(windows)]
fn ignore_console_ctrl_c() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler_routine: *const core::ffi::c_void, add: i32) -> i32;
    }
    // Safety: FFI call into kernel32 with a null handler routine and a
    // constant `add` flag; touches no memory we own. A NULL `HandlerRoutine`
    // with `Add = TRUE` is documented to mean "ignore CTRL+C input" rather
    // than "install this callback," so no callback trampoline is needed.
    unsafe {
        let _ = SetConsoleCtrlHandler(core::ptr::null(), 1);
    }
}

/// Non-Windows platforms never attach to a console in the first place, so
/// there is nothing to shield from console control events.
#[cfg(not(windows))]
#[inline]
fn ignore_console_ctrl_c() {}

/// Compute the attach-info block for the current startup options (hud-b7c0m).
///
/// Resolves the same config the runtime would use (honouring `--config`) for the
/// informational `config:` line, and derives the MCP/gRPC endpoint addresses from
/// the resolved ports and bind mode so the printed info matches the runtime it
/// describes. Rendering is delegated to
/// `tze_hud_runtime::windowed::render_attach_info` — the single source of truth
/// for the attach block, shared with the startup banner so the two never drift.
///
/// The returned block never contains the PSK: only ports/addresses and the
/// config path are passed down, and the JSON snippet uses a PSK placeholder.
fn render_attach_info_block(opts: &StartupOptions) -> String {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // Same bind-host selection the runtime uses: loopback unless the operator
    // opted into all-interfaces exposure (hud-1aswu.1).
    let host: IpAddr = if opts.bind_all_interfaces {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv4Addr::LOCALHOST.into()
    };
    let mcp_addr = (opts.mcp_port != 0).then(|| SocketAddr::new(host, opts.mcp_port));
    let grpc_addr = (opts.grpc_port != 0).then(|| SocketAddr::new(host, opts.grpc_port));

    // Resolve the config path the runtime would load (honours --config, env, and
    // platform defaults) purely for the informational line. Attach-info never
    // requires a readable/valid config — a resolution failure is simply reported
    // as "none resolved" rather than fail-closed as in canonical startup.
    let config_path = resolve_config_path(opts.config_path.as_deref()).ok();

    let mut block =
        tze_hud_runtime::windowed::render_attach_info(mcp_addr, grpc_addr, config_path.as_deref());
    block.push('\n');
    block
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bind the standard handles to the launching terminal ONCE, before any
    // output is produced (hud-q2glv). This is the GUI-subsystem console fix
    // that hud-b7c0m/#1112 (--print-attach-info) and hud-41q9t/#1140
    // (--help/--version) applied per-arm — hoisted to a single call here so it
    // ALSO covers the paths those missed: every startup `eprintln!` +
    // `exit(1)` failure (unknown flag, config/PSK/validation errors) and the
    // `tracing` log stream, all of which were otherwise silently discarded on
    // the Windows GUI-subsystem binary. Safe on the happy path: it attaches to
    // an existing parent console (terminal launch) or no-ops when there is none
    // (double-click) — it never AllocConsole's, so no console window flashes —
    // and it preserves an explicit redirect (`tze_hud … > out.txt`). No-op off
    // Windows. Because it runs before the per-arm/attach-info calls it made
    // redundant were removed, those paths now rely on this single attach.
    attach_parent_console();
    // The attach above now covers the long-running canonical runtime path
    // too, not just fast-exit paths — so the process stays attached to the
    // launching terminal's console for its entire lifetime. Shield it from
    // that console's Ctrl+C the same way a process with no console at all
    // (the pre-hud-q2glv default) was always immune (Codex P2 on PR #1143).
    ignore_console_ctrl_c();

    // Initialise structured logging. JSON if TZE_HUD_LOG_JSON=1.
    let log_json = std::env::var("TZE_HUD_LOG_JSON").as_deref() == Ok("1");
    if log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(tracing_subscriber::EnvFilter::from_env("TZE_HUD_LOG"))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_env("TZE_HUD_LOG"))
            .init();
    }

    // hud-pi5wx: file-based panic hook so a silent compositor/render-thread panic
    // leaves a durable trail — the overlay deployment captures no stdout/stderr.
    tze_hud_runtime::diag::install_panic_hook();

    // Collect CLI args, skipping argv[0] (the binary name).
    let args: Vec<String> = std::env::args().skip(1).collect();

    let opts = parse_options(&args).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    // ── Attach-info fast path (hud-b7c0m) ─────────────────────────────────────
    // Print the MCP attach-info block and exit *before* acquiring the GPU lock,
    // resolving/validating a config, or starting the runtime. This is an
    // onboarding aid that works on any platform without a shell, so it must not
    // fail-closed on a missing/invalid config the way canonical startup does.
    if opts.print_attach_info {
        // The console was attached once at the top of `main` (hud-q2glv), so on
        // Windows this GUI-subsystem binary's block reaches the launching
        // terminal (Codex P2, PR #1112) without a per-branch attach here.
        print!("{}", render_attach_info_block(&opts));
        // `process::exit` skips destructors, so flush the buffered block first.
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    // ── GPU lock (Windows scheduling policy, hud-940e4) ───────────────────────
    // Acquire the interactive GPU lock before claiming the GPU adapter.
    // On non-Windows this is a no-op (returns Ok(None)).
    // On Windows:
    //   - lock absent        → acquire and hold for process lifetime.
    //   - lock stale (dead)  → log warning, take over, hold for lifetime.
    //   - lock live (CI run) → hard refusal; exit with a clear error message.
    //   - I/O error          → log warning, continue without lock (fail-safe).
    let _gpu_lock_guard = match GpuLock::acquire() {
        Ok(guard) => guard,
        Err(conflict) => {
            eprintln!("error: {conflict}");
            eprintln!(
                "hint: A CI real-decode job or another tze_hud session is using the GPU. \
Wait for it to finish, then retry. See docs/design/tzehouse-windows-gpu-scheduling.md."
            );
            std::process::exit(1);
        }
    };

    // Resolve config file path and read its contents.
    // The resolved path is logged so operators can confirm which file is in use.
    // We track both the TOML content and the file path so that relative
    // [widget_bundles].paths entries can be resolved relative to the config file's
    // parent directory (spec §Widget Bundle Configuration).
    let security_mode = startup_security_mode();
    let mut searched_paths_for_missing: Vec<String> = Vec::new();
    let mut config_read_error_detail: Option<String> = None;
    let (config_toml, config_file_path): (Option<String>, Option<String>) =
        match resolve_config_path(opts.config_path.as_deref()) {
            Ok(path) => {
                match std::fs::read_to_string(&path) {
                    Ok(toml_src) => {
                        tracing::info!(config_path = %path, "config file loaded");
                        (Some(toml_src), Some(path))
                    }
                    Err(io_err) => {
                        // If a path was explicitly given via --config, this is a hard error.
                        // If it was auto-resolved, treat it as a warning and continue.
                        if opts.config_path.is_some() {
                            eprintln!("error: failed to read config file {path:?}: {io_err}");
                            std::process::exit(1);
                        }
                        searched_paths_for_missing = vec![path.clone()];
                        config_read_error_detail =
                            Some(format!("failed to read config file {path:?}: {io_err}"));
                        if security_mode == StartupSecurityMode::Strict {
                            tracing::warn!(
                                config_path = %path,
                                error = %io_err,
                                "config file found but not readable; strict startup will fail"
                            );
                        } else {
                            tracing::warn!(
                                config_path = %path,
                                error = %io_err,
                                "config file found but not readable; using flag/env-var defaults"
                            );
                        }
                        (None, None)
                    }
                }
            }
            Err(searched) => {
                searched_paths_for_missing = searched;
                // No config file found at any location.
                if opts.config_path.is_some() {
                    // --config was given explicitly but the file was not found.
                    // This is a hard error (RFC 0006 §1.3).
                    eprintln!(
                        "error: config file not found: {}",
                        searched_paths_for_missing
                            .first()
                            .map(String::as_str)
                            .unwrap_or("(unknown path)")
                    );
                    std::process::exit(1);
                }
                if security_mode == StartupSecurityMode::Strict {
                    tracing::debug!(
                        searched = ?searched_paths_for_missing,
                        "no config file found; strict startup will fail"
                    );
                } else {
                    // Config files are optional in dev-insecure override mode.
                    tracing::debug!(
                        searched = ?searched_paths_for_missing,
                        "no config file found; using flag/env-var defaults"
                    );
                }
                (None, None)
            }
        };

    if security_mode == StartupSecurityMode::Strict {
        if config_toml.is_none() {
            let searched_joined = if searched_paths_for_missing.is_empty() {
                "(no search paths reported)".to_string()
            } else {
                searched_paths_for_missing.join(", ")
            };
            eprintln!(
                "error: canonical startup requires a readable config file; searched: {searched_joined}"
            );
            if let Some(detail) = &config_read_error_detail {
                eprintln!("detail: {detail}");
            }
            eprintln!(
                "hint: this fail-closed behavior is mandatory for production startup. \
set {DEV_ALLOW_INSECURE_STARTUP_ENV}=1 only in debug/dev runs if you need fallback defaults."
            );
            std::process::exit(1);
        }

        if psk_is_trivial_default(&opts.psk) {
            eprintln!(
                "error: refusing startup with default PSK value {DEFAULT_PSK:?} in strict mode"
            );
            eprintln!("hint: set --psk <strong-key> or TZE_HUD_PSK to a non-default secret.");
            std::process::exit(1);
        }

        let toml_src = config_toml
            .as_ref()
            .expect("strict mode already checked config_toml presence");
        if let Err(msg) = validate_config_toml_for_startup(toml_src) {
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    } else {
        tracing::warn!(
            env = DEV_ALLOW_INSECURE_STARTUP_ENV,
            "development insecure startup override enabled; allowing permissive fallback behavior"
        );
    }

    // Auto-size is enabled for overlay mode when neither --width nor --height
    // was explicitly set (env var or CLI flag).  If either dimension was given
    // explicitly, auto-detection is disabled so the user's intent is honoured.
    let overlay_auto_size =
        opts.window_mode == WindowMode::Overlay && !opts.explicit_width && !opts.explicit_height;
    if opts.benchmark_emit.is_some() && opts.benchmark_frames == 0 {
        eprintln!("error: --benchmark-frames must be greater than zero");
        std::process::exit(1);
    }
    if opts.benchmark_emit.is_some() && opts.quiescent_efficiency_emit.is_some() {
        eprintln!(
            "error: --benchmark-emit and --quiescent-efficiency-emit cannot be used together"
        );
        std::process::exit(1);
    }
    let benchmark = opts
        .benchmark_emit
        .as_ref()
        .map(|path| WindowedBenchmarkConfig {
            warmup_frames: opts.benchmark_warmup_frames,
            frames: opts.benchmark_frames,
            emit_path: std::path::PathBuf::from(path),
        });
    let quiescent_efficiency =
        opts.quiescent_efficiency_emit
            .as_ref()
            .map(|path| WindowedQuiescentEfficiencyConfig {
                emit_path: std::path::PathBuf::from(path),
                build: format!("{VERSION}+{GIT_SHA}"),
            });

    tracing::info!(
        version = VERSION,
        git_sha = GIT_SHA,
        window_mode = %opts.window_mode,
        width = opts.width,
        height = opts.height,
        overlay_auto_size,
        grpc_port = opts.grpc_port,
        mcp_port = opts.mcp_port,
        fps = opts.fps,
        benchmark = benchmark.is_some(),
        quiescent_efficiency = quiescent_efficiency.is_some(),
        "tze_hud runtime starting"
    );

    let resident_grpc_portal = build_resident_grpc_portal_settings(&opts);
    let config = WindowedConfig {
        window: WindowConfig {
            mode: opts.window_mode,
            width: opts.width,
            height: opts.height,
            title: "tze_hud".to_string(),
        },
        overlay_auto_size,
        grpc_port: opts.grpc_port,
        mcp_port: opts.mcp_port,
        psk: opts.psk,
        projection_operator_authority: opts.projection_operator_authority,
        target_fps: opts.fps,
        config_toml,
        config_file_path,
        debug_zones: opts.debug_zones,
        monitor_index: opts.monitor_index,
        benchmark,
        quiescent_efficiency,
        bind_all_interfaces: opts.bind_all_interfaces,
        resident_grpc_portal,
    };

    // Diagnostic: write resolved config to disk so we can verify args were parsed.
    // PSK is redacted so the log file never captures the secret in plain text.
    let diag = format!(
        "mode={} width={} height={} auto_size={} grpc={} mcp={}\nargs={:?}\n",
        opts.window_mode,
        opts.width,
        opts.height,
        overlay_auto_size,
        opts.grpc_port,
        opts.mcp_port,
        redact_sensitive_args(std::env::args().collect()),
    );
    let _ = std::fs::write("C:\\tze_hud\\logs\\startup_diag.txt", &diag);

    let runtime = WindowedRuntime::new(config);
    runtime.run()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize all tests that mutate env vars.
    // Rust's test harness runs tests in parallel by default; without this mutex,
    // concurrent tests can observe or overwrite each other's env var changes,
    // causing data races (UB) and flaky failures.
    // Pattern mirrors tze_hud_compositor::renderer::ENV_VAR_MUTEX.
    static ENV_VAR_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_parse_options_env() {
        // Safety: callers hold ENV_VAR_MUTEX, so no other test mutates these
        // process-global environment variables while they are cleared.
        unsafe {
            for key in [
                "TZE_HUD_WINDOW_MODE",
                "TZE_HUD_WINDOW_WIDTH",
                "TZE_HUD_WINDOW_HEIGHT",
                "TZE_HUD_GRPC_PORT",
                "TZE_HUD_MCP_PORT",
                "TZE_HUD_PSK",
                "TZE_HUD_PROJECTION_OPERATOR_AUTHORITY",
                "TZE_HUD_BIND_ALL_INTERFACES",
                "TZE_HUD_FPS",
                "TZE_HUD_BENCHMARK_EMIT",
                "TZE_HUD_BENCHMARK_FRAMES",
                "TZE_HUD_BENCHMARK_WARMUP_FRAMES",
                "TZE_HUD_QUIESCENT_EFFICIENCY_EMIT",
                "TZE_HUD_RESIDENT_GRPC_PORTAL",
                "TZE_HUD_RESIDENT_GRPC_ENDPOINT",
                "TZE_HUD_RESIDENT_GRPC_AGENT_ID",
                "TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS",
                "TZE_HUD_RESIDENT_GRPC_PSK",
            ] {
                std::env::remove_var(key);
            }
        }
    }

    // ── parse_window_mode ────────────────────────────────────────────────────

    #[test]
    fn parse_window_mode_fullscreen() {
        assert_eq!(
            parse_window_mode("fullscreen").unwrap(),
            WindowMode::Fullscreen
        );
        assert_eq!(
            parse_window_mode("FULLSCREEN").unwrap(),
            WindowMode::Fullscreen
        );
        assert_eq!(
            parse_window_mode("Fullscreen").unwrap(),
            WindowMode::Fullscreen
        );
    }

    #[test]
    fn parse_window_mode_overlay() {
        assert_eq!(parse_window_mode("overlay").unwrap(), WindowMode::Overlay);
        assert_eq!(parse_window_mode("OVERLAY").unwrap(), WindowMode::Overlay);
    }

    #[test]
    fn parse_window_mode_unknown_returns_error() {
        let err = parse_window_mode("windowed").unwrap_err();
        assert!(
            err.contains("windowed"),
            "error should mention the bad value"
        );
        assert!(
            err.contains("fullscreen") || err.contains("overlay"),
            "error should mention valid values"
        );
    }

    // ── parse_options: defaults ───────────────────────────────────────────────

    #[test]
    fn parse_options_defaults_when_no_args() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();

        let opts = parse_options(&[]).unwrap();
        assert_eq!(opts.window_mode, WindowMode::Fullscreen);
        assert_eq!(opts.width, 1920);
        assert_eq!(opts.height, 1080);
        assert_eq!(opts.grpc_port, 50051);
        assert_eq!(opts.mcp_port, 9090);
        assert_eq!(opts.fps, 60);
        assert!(opts.config_path.is_none());
        assert!(opts.projection_operator_authority.is_none());
        assert!(opts.benchmark_emit.is_none());
        assert_eq!(opts.benchmark_frames, 600);
        assert_eq!(opts.benchmark_warmup_frames, 120);
        assert!(opts.quiescent_efficiency_emit.is_none());
    }

    // ── parse_options: CLI flags ─────────────────────────────────────────────

    #[test]
    fn parse_options_window_mode_overlay() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
        }
        let args: Vec<String> = vec!["--window-mode".to_string(), "overlay".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.window_mode, WindowMode::Overlay);
    }

    #[test]
    fn parse_options_width_and_height() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
        let args: Vec<String> = vec![
            "--width".to_string(),
            "1280".to_string(),
            "--height".to_string(),
            "720".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.width, 1280);
        assert_eq!(opts.height, 720);
    }

    #[test]
    fn parse_options_grpc_port_zero_disables() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_GRPC_PORT");
        }
        let args: Vec<String> = vec!["--grpc-port".to_string(), "0".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.grpc_port, 0);
    }

    #[test]
    fn parse_options_mcp_port() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_MCP_PORT");
        }
        let args: Vec<String> = vec!["--mcp-port".to_string(), "8080".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.mcp_port, 8080);
    }

    #[test]
    fn parse_options_mcp_port_zero_disables() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_MCP_PORT");
        }
        let args: Vec<String> = vec!["--mcp-port".to_string(), "0".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.mcp_port, 0);
    }

    #[test]
    fn parse_options_fps() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_FPS");
        }
        let args: Vec<String> = vec!["--fps".to_string(), "30".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.fps, 30);
    }

    #[test]
    fn parse_options_windowed_benchmark_flags() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_BENCHMARK_EMIT");
            std::env::remove_var("TZE_HUD_BENCHMARK_FRAMES");
            std::env::remove_var("TZE_HUD_BENCHMARK_WARMUP_FRAMES");
        }
        let args: Vec<String> = vec![
            "--benchmark-emit".to_string(),
            "artifacts/fullscreen.json".to_string(),
            "--benchmark-frames".to_string(),
            "720".to_string(),
            "--benchmark-warmup-frames".to_string(),
            "180".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert_eq!(
            opts.benchmark_emit.as_deref(),
            Some("artifacts/fullscreen.json")
        );
        assert_eq!(opts.benchmark_frames, 720);
        assert_eq!(opts.benchmark_warmup_frames, 180);
    }

    #[test]
    fn parse_options_rejects_empty_benchmark_emit_path() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_BENCHMARK_EMIT");
        }
        let args: Vec<String> = vec!["--benchmark-emit".to_string(), "".to_string()];
        let err = parse_options(&args).unwrap_err();
        assert!(
            err.contains("--benchmark-emit") && err.contains("non-empty path"),
            "error should identify the empty benchmark emit path, got: {err}"
        );
    }

    #[test]
    fn parse_options_quiescent_efficiency_emit_flag() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--quiescent-efficiency-emit".to_string(),
            "artifacts/idle-efficiency.json".to_string(),
        ];

        let opts = parse_options(&args).unwrap();
        assert_eq!(
            opts.quiescent_efficiency_emit.as_deref(),
            Some("artifacts/idle-efficiency.json")
        );
        assert!(opts.benchmark_emit.is_none());
    }

    #[test]
    fn parse_options_config_path() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--config".to_string(),
            "/etc/tze_hud/config.toml".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert_eq!(
            opts.config_path.as_deref(),
            Some("/etc/tze_hud/config.toml")
        );
    }

    #[test]
    fn parse_options_psk() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_PSK");
        }
        let args: Vec<String> = vec!["--psk".to_string(), "my-secret-key".to_string()];
        let opts = parse_options(&args).unwrap();
        assert_eq!(opts.psk, "my-secret-key");
    }

    #[test]
    fn parse_options_print_attach_info_flag() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec!["--print-attach-info".to_string()];
        let opts = parse_options(&args).unwrap();
        assert!(
            opts.print_attach_info,
            "--print-attach-info must set the flag"
        );
    }

    /// hud-q2glv: `main` calls `attach_parent_console()` once at startup so
    /// every output path — `--help`/`--version`, `--print-attach-info`, tracing
    /// logs, and startup `eprintln!` + `exit(1)` errors — reaches the launching
    /// terminal on the Windows GUI-subsystem binary. That call site runs before
    /// `std::process::exit` and drives real handles, so it cannot be exercised
    /// in-process; this pins the one thing a unit test CAN assert: the function
    /// is callable and a true no-op on this (non-Windows) platform. The
    /// `#[cfg(windows)]` variant is covered by the windows-gnu cross-target
    /// clippy/build gate instead.
    #[test]
    fn attach_parent_console_is_a_callable_noop_off_windows() {
        attach_parent_console();
    }

    /// hud-q2glv (Codex P2 on PR #1143): `main` also calls
    /// `ignore_console_ctrl_c()` once at startup, right after
    /// `attach_parent_console()`, so the now-always-attached long-running
    /// process is not killed by a Ctrl+C its console receives for an
    /// unrelated reason. The real `SetConsoleCtrlHandler` call drives a
    /// process-global OS handler and cannot be meaningfully exercised
    /// in-process; this pins the one thing a unit test CAN assert: the
    /// function is callable and a true no-op on this (non-Windows) platform.
    /// The `#[cfg(windows)]` variant is covered by the windows-gnu
    /// cross-target clippy/build gate instead.
    #[test]
    fn ignore_console_ctrl_c_is_a_callable_noop_off_windows() {
        ignore_console_ctrl_c();
    }

    #[test]
    fn render_attach_info_block_has_sections_and_hides_psk() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        // Explicit (nonexistent) config path keeps resolution deterministic and
        // independent of env/cwd — attach-info never requires a readable config.
        let opts = StartupOptions {
            config_path: Some("/nonexistent/tze_hud.toml".to_string()),
            mcp_port: 9090,
            grpc_port: 50051,
            psk: "SUPER-SECRET-PSK-9f2c-do-not-leak".to_string(),
            ..StartupOptions::default()
        };
        let block = render_attach_info_block(&opts);
        assert!(
            !block.contains(&opts.psk),
            "attach-info block must not leak the configured PSK:\n{block}"
        );
        assert!(
            block.contains("http://127.0.0.1:9090/mcp"),
            "MCP endpoint URL missing:\n{block}"
        );
        assert!(
            block.contains("127.0.0.1:50051"),
            "gRPC addr missing:\n{block}"
        );
        assert!(
            block.contains("allow list must include"),
            "allow-list rule missing:\n{block}"
        );
        assert!(
            block.contains("\"mcpServers\""),
            "paste-ready MCP client config missing:\n{block}"
        );
    }

    #[test]
    fn render_attach_info_block_disabled_mcp_omits_snippet() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let opts = StartupOptions {
            config_path: Some("/nonexistent/tze_hud.toml".to_string()),
            mcp_port: 0,
            ..StartupOptions::default()
        };
        let block = render_attach_info_block(&opts);
        assert!(
            block.contains("MCP endpoint : disabled"),
            "disabled MCP must be reported:\n{block}"
        );
        assert!(
            !block.contains("\"mcpServers\""),
            "disabled MCP must not emit a client snippet:\n{block}"
        );
    }

    #[test]
    fn parse_options_projection_operator_authority_env() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::set_var(
                "TZE_HUD_PROJECTION_OPERATOR_AUTHORITY",
                " operator-secret\n",
            );
        }

        let opts = parse_options(&[]).unwrap();
        assert_eq!(
            opts.projection_operator_authority.as_deref(),
            Some("operator-secret")
        );

        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_PROJECTION_OPERATOR_AUTHORITY");
        }
    }

    #[test]
    fn parse_options_projection_operator_authority_env_rejects_empty() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::set_var("TZE_HUD_PROJECTION_OPERATOR_AUTHORITY", " \n\t ");
        }

        let err = parse_options(&[]).unwrap_err();
        assert!(
            err.contains("TZE_HUD_PROJECTION_OPERATOR_AUTHORITY") && err.contains("non-empty"),
            "error must identify empty projection operator authority env var, got: {err}"
        );

        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_PROJECTION_OPERATOR_AUTHORITY");
        }
    }

    #[test]
    fn startup_security_mode_debug_with_override_is_dev_insecure() {
        let mode = startup_security_mode_for_env(Some("1"), true);
        assert_eq!(mode, StartupSecurityMode::DevInsecureOverride);
    }

    #[test]
    fn startup_security_mode_debug_without_override_is_strict() {
        let mode = startup_security_mode_for_env(None, true);
        assert_eq!(mode, StartupSecurityMode::Strict);
    }

    #[test]
    fn startup_security_mode_release_ignores_override_and_is_strict() {
        let mode = startup_security_mode_for_env(Some("1"), false);
        assert_eq!(mode, StartupSecurityMode::Strict);
    }

    #[test]
    fn psk_is_trivial_default_detects_default_only() {
        assert!(psk_is_trivial_default(DEFAULT_PSK));
        assert!(!psk_is_trivial_default("test-psk-do-not-use"));
    }

    #[test]
    fn validate_config_toml_for_startup_accepts_minimal_valid_config() {
        let toml = r#"
[runtime]
profile = "full-display"

[[tabs]]
name = "Main"
"#;
        let result = validate_config_toml_for_startup(toml);
        assert!(result.is_ok(), "valid config should pass, got: {result:?}");
    }

    #[test]
    fn validate_config_toml_for_startup_rejects_invalid_toml() {
        let bad = "not valid toml [";
        let result = validate_config_toml_for_startup(bad);
        assert!(
            result.is_err(),
            "invalid TOML must be rejected by startup validation"
        );
    }

    #[test]
    fn validate_config_toml_for_startup_rejects_validation_errors() {
        let invalid = r#"
[runtime]
profile = "full-display"
"#;
        let result = validate_config_toml_for_startup(invalid);
        assert!(
            result.is_err(),
            "config missing [[tabs]] must be rejected by startup validation"
        );
    }

    // ── parse_options: errors ─────────────────────────────────────────────────

    #[test]
    fn parse_options_unknown_flag_returns_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec!["--unknown-flag".to_string()];
        let err = parse_options(&args).unwrap_err();
        assert!(
            err.contains("unknown flag"),
            "error should mention unknown flag"
        );
    }

    #[test]
    fn parse_options_window_mode_missing_value_returns_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec!["--window-mode".to_string()];
        let err = parse_options(&args).unwrap_err();
        assert!(
            err.contains("--window-mode"),
            "error should mention the flag"
        );
    }

    #[test]
    fn parse_options_width_non_integer_returns_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
        }
        let args: Vec<String> = vec!["--width".to_string(), "bad".to_string()];
        let err = parse_options(&args).unwrap_err();
        assert!(err.contains("--width"), "error should mention the flag");
    }

    #[test]
    fn parse_options_positional_arg_returns_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec!["unexpected".to_string()];
        let err = parse_options(&args).unwrap_err();
        assert!(
            err.contains("unexpected positional argument"),
            "error should explain positional arg, got: {err}"
        );
    }

    // ── Non-default dimension regression tests (hud-q5hx) ────────────────────
    //
    // Verify that the exact CLI invocation reported in hud-q5hx parses correctly.
    // The crash was triggered by `--window-mode overlay --width 2560 --height 1440`;
    // the root cause was in the windowed runtime's surface initialization, not
    // argument parsing, but these tests document the contract end-to-end.

    /// The exact command line from the bug report must parse without error and
    /// produce the correct overlay mode and 2560x1440 dimensions.
    #[test]
    fn parse_options_overlay_2560x1440_bug_repro_command() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }

        // Mirrors: tze_hud.exe --window-mode overlay --width 2560 --height 1440
        let args: Vec<String> = vec![
            "--window-mode".to_string(),
            "overlay".to_string(),
            "--width".to_string(),
            "2560".to_string(),
            "--height".to_string(),
            "1440".to_string(),
        ];
        let opts = parse_options(&args).expect("must parse without error");
        assert_eq!(
            opts.window_mode,
            WindowMode::Overlay,
            "window mode must be Overlay"
        );
        assert_eq!(opts.width, 2560, "width must be 2560");
        assert_eq!(opts.height, 1440, "height must be 1440");
    }

    /// Verify 4K (3840x2160) dimensions also parse correctly.
    #[test]
    fn parse_options_overlay_4k_dimensions() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }

        let args: Vec<String> = vec![
            "--window-mode".to_string(),
            "overlay".to_string(),
            "--width".to_string(),
            "3840".to_string(),
            "--height".to_string(),
            "2160".to_string(),
        ];
        let opts = parse_options(&args).expect("must parse without error");
        assert_eq!(opts.window_mode, WindowMode::Overlay);
        assert_eq!(opts.width, 3840);
        assert_eq!(opts.height, 2160);
    }

    // ── overlay_auto_size flag computation (hud-48ml) ─────────────────────────
    //
    // These tests verify the three-way interaction that controls whether the
    // windowed runtime should auto-detect the primary monitor resolution:
    // 1. overlay mode + no explicit dimensions → auto_size=true
    // 2. overlay mode + explicit --width/--height → auto_size=false (user intent)
    // 3. fullscreen mode → auto_size=false (fullscreen always uses monitor native)

    /// In overlay mode with no explicit dimensions, auto-detection must be enabled
    /// (acceptance criterion 1: overlay auto-sizes to primary monitor).
    #[test]
    fn overlay_mode_no_explicit_dims_enables_auto_size() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
        let args: Vec<String> = vec!["--window-mode".to_string(), "overlay".to_string()];
        let opts = parse_options(&args).expect("must parse");
        assert_eq!(opts.window_mode, WindowMode::Overlay);
        assert!(!opts.explicit_width, "width must not be marked explicit");
        assert!(!opts.explicit_height, "height must not be marked explicit");
        // Derived: overlay_auto_size would be true
        let overlay_auto_size = opts.window_mode == WindowMode::Overlay
            && !opts.explicit_width
            && !opts.explicit_height;
        assert!(
            overlay_auto_size,
            "overlay without explicit dims must enable auto-size"
        );
    }

    /// In overlay mode with explicit --width AND --height, auto-detection must be
    /// disabled (acceptance criterion 2: explicit flags override auto-detection).
    #[test]
    fn overlay_mode_with_explicit_dims_disables_auto_size() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
        let args: Vec<String> = vec![
            "--window-mode".to_string(),
            "overlay".to_string(),
            "--width".to_string(),
            "2560".to_string(),
            "--height".to_string(),
            "1440".to_string(),
        ];
        let opts = parse_options(&args).expect("must parse");
        assert!(
            opts.explicit_width,
            "width must be marked explicit when --width is given"
        );
        assert!(
            opts.explicit_height,
            "height must be marked explicit when --height is given"
        );
        let overlay_auto_size = opts.window_mode == WindowMode::Overlay
            && !opts.explicit_width
            && !opts.explicit_height;
        assert!(
            !overlay_auto_size,
            "explicit --width/--height must disable auto-size"
        );
    }

    /// In overlay mode with only --width set, auto-detection is disabled
    /// (either dimension being explicit disables auto-size for consistency).
    #[test]
    fn overlay_mode_with_explicit_width_only_disables_auto_size() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
        let args: Vec<String> = vec![
            "--window-mode".to_string(),
            "overlay".to_string(),
            "--width".to_string(),
            "1280".to_string(),
        ];
        let opts = parse_options(&args).expect("must parse");
        assert!(opts.explicit_width, "explicit_width must be set");
        assert!(!opts.explicit_height, "explicit_height must not be set");
        let overlay_auto_size = opts.window_mode == WindowMode::Overlay
            && !opts.explicit_width
            && !opts.explicit_height;
        assert!(
            !overlay_auto_size,
            "any explicit dimension must disable auto-size"
        );
    }

    /// In fullscreen mode, auto-size is always disabled regardless of explicit dims
    /// (fullscreen handles sizing via Fullscreen::Borderless, not overlay path).
    #[test]
    fn fullscreen_mode_never_enables_auto_size() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
        let opts = parse_options(&[]).expect("must parse");
        assert_eq!(opts.window_mode, WindowMode::Fullscreen);
        let overlay_auto_size = opts.window_mode == WindowMode::Overlay
            && !opts.explicit_width
            && !opts.explicit_height;
        assert!(
            !overlay_auto_size,
            "fullscreen mode must never enable overlay auto-size"
        );
    }

    /// Explicit width/height via environment variables also disables auto-size.
    #[test]
    fn overlay_mode_with_env_var_dims_disables_auto_size() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        // Safety: single-threaded within ENV_VAR_MUTEX guard.
        unsafe {
            std::env::set_var("TZE_HUD_WINDOW_MODE", "overlay");
            std::env::set_var("TZE_HUD_WINDOW_WIDTH", "3840");
            std::env::set_var("TZE_HUD_WINDOW_HEIGHT", "2160");
        }
        let opts = parse_options(&[]).expect("must parse");
        assert_eq!(opts.window_mode, WindowMode::Overlay);
        assert_eq!(opts.width, 3840);
        assert_eq!(opts.height, 2160);
        assert!(opts.explicit_width, "env-var width must count as explicit");
        assert!(
            opts.explicit_height,
            "env-var height must count as explicit"
        );
        let overlay_auto_size = opts.window_mode == WindowMode::Overlay
            && !opts.explicit_width
            && !opts.explicit_height;
        assert!(
            !overlay_auto_size,
            "env-var explicit dims must disable auto-size"
        );
        // Clean up.
        unsafe {
            std::env::remove_var("TZE_HUD_WINDOW_MODE");
            std::env::remove_var("TZE_HUD_WINDOW_WIDTH");
            std::env::remove_var("TZE_HUD_WINDOW_HEIGHT");
        }
    }

    // ── resident gRPC portal CLI/env surfacing (hud-ev2lr) ────────────────────

    /// Default StartupOptions must have resident gRPC portal disabled.
    #[test]
    fn parse_options_default_resident_grpc_portal_is_off() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let opts = parse_options(&[]).unwrap();
        assert!(
            !opts.resident_grpc_portal_enabled,
            "resident gRPC portal must be off by default"
        );
    }

    /// `--resident-grpc-portal` flag enables the bridge with all defaults.
    #[test]
    fn parse_options_resident_grpc_portal_flag_enables_bridge() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec!["--resident-grpc-portal".to_string()];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert!(opts.resident_grpc_endpoint.is_none());
        assert_eq!(
            opts.resident_grpc_agent_id,
            tze_hud_runtime::windowed::DEFAULT_RESIDENT_GRPC_AGENT_ID
        );
        assert_eq!(
            opts.resident_grpc_lease_ttl_ms,
            tze_hud_runtime::windowed::DEFAULT_RESIDENT_GRPC_LEASE_TTL_MS
        );
        assert!(opts.resident_grpc_psk.is_none());
    }

    /// `--resident-grpc-endpoint` sets the target endpoint and implies enable.
    #[test]
    fn parse_options_resident_grpc_endpoint_flag_implies_enable() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-endpoint".to_string(),
            "http://10.0.0.4:50051".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(
            opts.resident_grpc_endpoint.as_deref(),
            Some("http://10.0.0.4:50051")
        );
    }

    /// `--resident-grpc-agent-id` overrides the agent identity and implies enable.
    #[test]
    fn parse_options_resident_grpc_agent_id_flag_implies_enable() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-agent-id".to_string(),
            "my-external-agent".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(opts.resident_grpc_agent_id, "my-external-agent");
    }

    /// `--resident-grpc-lease-ttl` overrides the TTL and implies enable.
    #[test]
    fn parse_options_resident_grpc_lease_ttl_flag_implies_enable() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-lease-ttl".to_string(),
            "120000".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(opts.resident_grpc_lease_ttl_ms, 120_000);
    }

    /// `--resident-grpc-psk` sets an explicit credential and implies enable.
    #[test]
    fn parse_options_resident_grpc_psk_flag_implies_enable() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-psk".to_string(),
            "external-secret".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(opts.resident_grpc_psk.as_deref(), Some("external-secret"));
    }

    /// All four sub-settings can be combined with the portal flag.
    #[test]
    fn parse_options_resident_grpc_all_sub_flags() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-portal".to_string(),
            "--resident-grpc-endpoint".to_string(),
            "http://192.168.1.20:50051".to_string(),
            "--resident-grpc-agent-id".to_string(),
            "external-portal".to_string(),
            "--resident-grpc-lease-ttl".to_string(),
            "90000".to_string(),
            "--resident-grpc-psk".to_string(),
            "ext-key".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(
            opts.resident_grpc_endpoint.as_deref(),
            Some("http://192.168.1.20:50051")
        );
        assert_eq!(opts.resident_grpc_agent_id, "external-portal");
        assert_eq!(opts.resident_grpc_lease_ttl_ms, 90_000);
        assert_eq!(opts.resident_grpc_psk.as_deref(), Some("ext-key"));
    }

    /// `TZE_HUD_RESIDENT_GRPC_PORTAL=1` env var enables the bridge with defaults.
    #[test]
    fn parse_options_resident_grpc_portal_env_enables_bridge() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        unsafe {
            std::env::set_var("TZE_HUD_RESIDENT_GRPC_PORTAL", "1");
        }
        let opts = parse_options(&[]).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        // Clean up.
        unsafe {
            std::env::remove_var("TZE_HUD_RESIDENT_GRPC_PORTAL");
        }
    }

    /// `TZE_HUD_RESIDENT_GRPC_ENDPOINT` env var sets the endpoint and implies enable.
    #[test]
    fn parse_options_resident_grpc_endpoint_env_implies_enable() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        unsafe {
            std::env::set_var("TZE_HUD_RESIDENT_GRPC_ENDPOINT", "http://10.0.0.5:50051");
        }
        let opts = parse_options(&[]).unwrap();
        assert!(opts.resident_grpc_portal_enabled);
        assert_eq!(
            opts.resident_grpc_endpoint.as_deref(),
            Some("http://10.0.0.5:50051")
        );
        unsafe {
            std::env::remove_var("TZE_HUD_RESIDENT_GRPC_ENDPOINT");
        }
    }

    /// `TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS` env var with invalid value is an error.
    #[test]
    fn parse_options_resident_grpc_lease_ttl_env_invalid_is_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        unsafe {
            std::env::set_var("TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS", "not-a-number");
        }
        let err = parse_options(&[]).unwrap_err();
        assert!(
            err.contains("TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS"),
            "error must name the env var, got: {err}"
        );
        unsafe {
            std::env::remove_var("TZE_HUD_RESIDENT_GRPC_LEASE_TTL_MS");
        }
    }

    /// `--resident-grpc-lease-ttl` with invalid value is an error.
    #[test]
    fn parse_options_resident_grpc_lease_ttl_flag_invalid_is_error() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        let args: Vec<String> = vec![
            "--resident-grpc-lease-ttl".to_string(),
            "not-a-number".to_string(),
        ];
        let err = parse_options(&args).unwrap_err();
        assert!(
            err.contains("--resident-grpc-lease-ttl"),
            "error must name the flag, got: {err}"
        );
    }

    /// CLI flag overrides env var: endpoint from flag wins over env var.
    #[test]
    fn parse_options_resident_grpc_endpoint_flag_overrides_env() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        clear_parse_options_env();
        unsafe {
            std::env::set_var("TZE_HUD_RESIDENT_GRPC_ENDPOINT", "http://env-host:50051");
        }
        let args: Vec<String> = vec![
            "--resident-grpc-endpoint".to_string(),
            "http://flag-host:50051".to_string(),
        ];
        let opts = parse_options(&args).unwrap();
        assert_eq!(
            opts.resident_grpc_endpoint.as_deref(),
            Some("http://flag-host:50051"),
            "CLI flag must override env var"
        );
        unsafe {
            std::env::remove_var("TZE_HUD_RESIDENT_GRPC_ENDPOINT");
        }
    }

    /// Wire-through: portal-enabled with explicit endpoint maps to
    /// `WindowedConfig::resident_grpc_portal = Some(..)` with `Psk` credential.
    #[test]
    fn parse_options_resident_grpc_wire_through_explicit_psk() {
        let opts = StartupOptions {
            resident_grpc_portal_enabled: true,
            resident_grpc_endpoint: Some("http://192.168.1.20:50051".to_string()),
            resident_grpc_agent_id: "ext-agent".to_string(),
            resident_grpc_lease_ttl_ms: 30_000,
            resident_grpc_psk: Some("ext-secret".to_string()),
            ..StartupOptions::default()
        };
        let settings = build_resident_grpc_portal_settings(&opts)
            .expect("portal settings must be Some when enabled");
        assert_eq!(
            settings.endpoint.as_deref(),
            Some("http://192.168.1.20:50051")
        );
        assert_eq!(settings.agent_id, "ext-agent");
        assert_eq!(settings.lease_ttl_ms, 30_000);
        assert_eq!(
            settings.credential,
            ResidentGrpcCredentialSource::Psk("ext-secret".to_string())
        );
    }

    /// Wire-through: portal-enabled without explicit PSK uses `RuntimePsk`.
    #[test]
    fn parse_options_resident_grpc_wire_through_runtime_psk() {
        let opts = StartupOptions {
            resident_grpc_portal_enabled: true,
            ..StartupOptions::default()
        };
        let settings = build_resident_grpc_portal_settings(&opts)
            .expect("portal settings must be Some when enabled");
        assert_eq!(
            settings.credential,
            ResidentGrpcCredentialSource::RuntimePsk
        );
    }

    /// Wire-through: portal disabled returns None.
    #[test]
    fn parse_options_resident_grpc_wire_through_disabled_is_none() {
        let opts = StartupOptions::default();
        assert!(build_resident_grpc_portal_settings(&opts).is_none());
    }

    // ── redact_sensitive_args (hud-9bi85) ─────────────────────────────────────

    /// `--resident-grpc-psk` value is replaced with `<redacted>`.
    #[test]
    fn redact_sensitive_args_redacts_psk_value() {
        let args = vec![
            "tze_hud".to_string(),
            "--resident-grpc-psk".to_string(),
            "super-secret".to_string(),
            "--resident-grpc-portal".to_string(),
        ];
        let redacted = redact_sensitive_args(args);
        assert_eq!(redacted[1], "--resident-grpc-psk");
        assert_eq!(redacted[2], "<redacted>");
        assert_eq!(redacted[3], "--resident-grpc-portal");
    }

    /// Args without `--resident-grpc-psk` are returned unchanged.
    #[test]
    fn redact_sensitive_args_passthrough_when_no_psk() {
        let args = vec![
            "tze_hud".to_string(),
            "--resident-grpc-portal".to_string(),
            "--resident-grpc-endpoint".to_string(),
            "http://10.0.0.1:50051".to_string(),
        ];
        let expected = args.clone();
        assert_eq!(redact_sensitive_args(args), expected);
    }

    /// Trailing `--resident-grpc-psk` with no following value does not panic.
    #[test]
    fn redact_sensitive_args_trailing_psk_flag_does_not_panic() {
        let args = vec!["tze_hud".to_string(), "--resident-grpc-psk".to_string()];
        let redacted = redact_sensitive_args(args);
        assert_eq!(redacted, vec!["tze_hud", "--resident-grpc-psk"]);
    }
}
