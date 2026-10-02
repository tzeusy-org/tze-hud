//! Stdio control surface for the cooperative HUD projection authority.
//!
//! This executable is intentionally a projection-daemon surface, not runtime
//! v1 MCP. It retains `ProjectionAuthority` state only for the lifetime of
//! this process and dispatches newline-delimited JSON requests to the
//! normative operation handlers in `tze_hud_projection`.
//!
//! ## Portal drive loop (hud-6rkc8)
//!
//! After every `PublishOutput` dispatch the binary runs a work-conserving drain
//! loop that feeds coalesced portal updates into the present path:
//!
//! 1. `ProjectionAuthority::next_due_projection_id` selects the next portal in
//!    round-robin fairness order (cross-portal fairness, tasks.md §5.1).
//! 2. `ProjectionAuthority::take_due_portal_update` materialises the coalesced
//!    transcript window for that portal.
//! 3. `ProjectionAuthority::projected_portal_state` builds the full portal state.
//! 4. `ResidentGrpcPortalAdapter::ensure_portal_tile_message` /
//!    `render_portal_message` builds the outbound `HudSession` gRPC message.
//! 5. Each resulting command is serialised as a `CliPortalDrainRecord` line on
//!    stdout so the caller can forward it to the resident gRPC session.
//!
//! ## Token-map swap propagation (hud-6rkc8 part a)
//!
//! A `SetTokenMap` operation accepts a flat key→value token override map.
//! On receipt the authority resolves the full token set via
//! `tze_hud_config::resolve_portal_tokens`, converts to `PortalVisualTokens`,
//! and calls `adapter.set_visual_tokens(...)` on every live adapter. The next
//! render after the swap uses the new tokens with zero adapter logic changes
//! (§6.1 profile-swap contract).
//!
//! ## Hook points left for follow-up beads
//!
//! - **hud-ttq97 / hud-bq0gl.14** (publish-to-present latency): the
//!   `submitted_at_us` field of `PortalTranscriptUpdate` is now emitted as a
//!   structured `tracing::debug!` event with the arrival→present delta per
//!   drained portal update.  Coalescer health (portal_count, total_taken,
//!   total_coalesced) is emitted as a second debug event after each drain cycle.
//!
//! - **hud-0528i** (follow-tail notify_tile_content_appended): wired. After the
//!   adapter emits a `RenderPortal` command, `CliPortalDrainRecord::append_geometry`
//!   carries `new_content_height_px`, `viewport_height_px`, and `line_height_px` so
//!   the runtime can call `InputProcessor::notify_tile_content_appended` immediately
//!   after forwarding the drain record to the gRPC session (spec §3.2 / §3.3).
//!
//! - **hud-pkg2g** (head-trim notify_head_content_removed): wired. When a
//!   head-trim prunes older units (64 KiB coalescer cap or 16 KiB visible-window
//!   cap), `drain_and_emit_portal_updates` detects the shrinkage and emits a
//!   `head_trim_geometry` field in `CliPortalDrainRecord`. The runtime caller
//!   should call `InputProcessor::notify_head_content_removed(tile_id,
//!   g.removed_height_px)` after reading a record with that field populated, so
//!   scrolled-back viewports stay stable (spec §3.3).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};
use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
use tze_hud_projection::ProjectedPortalPolicy;
use tze_hud_projection::resident_grpc::{
    ResidentGrpcPortalAdapter, ResidentGrpcPortalCommandKind, ResidentGrpcPortalConfig,
    portal_visual_tokens_from_part_tokens,
};
use tze_hud_projection::{
    AcknowledgeInputRequest, AdvisoryLeaseIdentity, AttachRequest, CleanupRequest,
    ContentClassification, DetachRequest, ExternalAgentProjectionAuthority, GetPendingInputRequest,
    HudConnectionMetadata, HudCredentialSource, ListProjectionsRequest, ManagedSessionOrigin,
    ManagedSessionRequest, PresenceSurfaceRoute, ProjectionAttentionIntent, ProjectionAuditRecord,
    ProjectionAuthority, ProjectionBounds, ProjectionErrorCode, ProjectionOperation,
    ProjectionResponse, ProviderKind, PublishOutputRequest, PublishStatusRequest,
    WidgetParameterValue, WindowsHudTarget,
};

const DEFAULT_CALLER_IDENTITY: &str = "projection-authority-stdio";
const MAX_CLI_STATUS_SUMMARY_BYTES: usize = 512;
const MAX_STDIN_LINE_BYTES: usize = 65_536;

#[derive(Debug)]
struct CliConfig {
    mode: CliMode,
    caller_identity: String,
    operator_authority: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliMode {
    Stdio,
    DemoPlan,
}

#[derive(Debug, Serialize)]
struct CliOperationResult {
    response: ProjectionResponse,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    audit_records: Vec<ProjectionAuditRecord>,
    /// Zero or more portal drain records produced by the drive loop.
    ///
    /// Non-empty after any operation that advances the cadence coalescer
    /// (primarily `PublishOutput`). Each record contains a coalesced portal
    /// transcript update that the caller should forward to the resident gRPC
    /// session. Embedded in the operation result so the caller reads one
    /// JSON line per operation regardless of how many portals are served.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    portal_drain: Vec<CliPortalDrainRecord>,
}

#[derive(Debug, Serialize)]
struct DemoPlanOutput {
    demo_name: String,
    hud_target_id: String,
    route_plans: Vec<tze_hud_projection::ManagedSessionRoutePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    zone_messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    widget_messages: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    portal_routes: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lifecycle_checks: Vec<Value>,
}

// ── Portal drive state (hud-6rkc8) ───────────────────────────────────────────

/// Per-process state for the production portal drive loop.
///
/// Holds one `ResidentGrpcPortalAdapter` per attached projection session that
/// uses the resident-gRPC portal surface, plus the current resolved token map.
/// The adapter is constructed with runtime-resolved tokens on `Attach` and
/// updated on every `SetTokenMap` operation.
///
/// Not serialized — lives only for the lifetime of the stdio process.
struct PortalDriveState {
    /// Per-projection adapters keyed by `projection_id`.
    adapters: HashMap<String, ResidentGrpcPortalAdapter>,
    /// Current resolved design-token overrides (flat key → value strings).
    /// `PortalVisualTokens` is derived from this on every `SetTokenMap` and at
    /// adapter construction.
    token_overrides: DesignTokenMap,
    /// Estimated content height (px) from the last `RenderPortal` drain per portal.
    ///
    /// A decrease in this value between consecutive drains is the authoritative signal
    /// that the visible transcript shrank (head-trim event).  The height is the direct
    /// observable used by the caller for scroll accounting via
    /// `InputProcessor::notify_head_content_removed` (hud-pkg2g, hud-hkaw2).
    prev_content_height_px: HashMap<String, f32>,
}

impl PortalDriveState {
    fn new() -> Self {
        Self {
            adapters: HashMap::new(),
            token_overrides: DesignTokenMap::new(),
            prev_content_height_px: HashMap::new(),
        }
    }

    /// Resolve the current portal visual tokens from `token_overrides`.
    fn resolve_visual_tokens(&self) -> tze_hud_projection::resident_grpc::PortalVisualTokens {
        let resolved =
            tze_hud_config::tokens::resolve_tokens(&DesignTokenMap::new(), &self.token_overrides);
        portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&resolved))
    }

    /// Register a new adapter for `projection_id` with runtime-resolved tokens.
    ///
    /// Called on every `Attach` for sessions that will use the portal path.
    /// The adapter is constructed with the supplied `lease_id`. Callers that
    /// attach before a live gRPC session is established pass `Vec::new()` as a
    /// placeholder; the placeholder is acceptable for the CreatePortalTile path
    /// because the resident session layer is responsible for setting the actual
    /// lease before sending any `MutationBatch`. Token swap propagation is
    /// immediate — `set_token_map` will reach this adapter on the next
    /// `SetTokenMap` operation.
    fn attach_adapter(&mut self, projection_id: &str, lease_id: Vec<u8>) {
        let tokens = self.resolve_visual_tokens();
        let config = ResidentGrpcPortalConfig::new(lease_id);
        let adapter = ResidentGrpcPortalAdapter::with_tokens(config, tokens);
        self.adapters.insert(projection_id.to_string(), adapter);
    }

    /// Remove the adapter for `projection_id` (called on `Detach` / `Cleanup`).
    fn detach_adapter(&mut self, projection_id: &str) {
        self.adapters.remove(projection_id);
        self.prev_content_height_px.remove(projection_id);
    }

    /// Apply a new token override map: resolve, then propagate to all live adapters.
    ///
    /// This is the §6.1 profile-swap contract: `set_visual_tokens` is called on
    /// every adapter so the next render uses the new tokens without any adapter
    /// logic changes.
    fn apply_token_map(&mut self, overrides: DesignTokenMap) {
        self.token_overrides = overrides;
        let tokens = self.resolve_visual_tokens();
        for adapter in self.adapters.values_mut() {
            adapter.set_visual_tokens(tokens.clone());
        }
    }

    /// Get a mutable reference to the adapter for `projection_id`, if any.
    fn adapter_mut(&mut self, projection_id: &str) -> Option<&mut ResidentGrpcPortalAdapter> {
        self.adapters.get_mut(projection_id)
    }
}

/// Line-height multiplier used by the compositor's text shaper (text.rs).
///
/// `line_height_px = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER`
///
/// Must stay in sync with `tze_hud_compositor::LINE_HEIGHT_MULTIPLIER`. This
/// binary does not depend on `tze_hud_compositor`, so the value is duplicated
/// here; when that constant changes, update this one in the same PR.
const PORTAL_LINE_HEIGHT_MULTIPLIER: f32 = 1.4;

/// Geometry data required by the runtime to call
/// `InputProcessor::notify_tile_content_appended` after a `RenderPortal`.
///
/// The runtime (caller of the stdio drain output) has the `SceneId` / tile
/// handle, but the geometry values used to compute the follow-tail offset must
/// come from the projection authority side because only this process knows
/// the transcript line count and the current design-token font size.
///
/// # How to use (runtime side)
///
/// After reading a `CliPortalDrainRecord` with `command_kind = render_portal`
/// and `append_geometry = Some(g)`:
///
/// ```text
/// input_processor.notify_tile_content_appended(
///     tile_id,                  // SceneId from the caller's tile registry
///     g.new_content_height_px,
///     g.viewport_height_px,
///     g.line_height_px,
///     &mut scene,
/// );
/// ```
///
/// The call is a no-op when the tile is `ScrolledBack` (spec §3.3 — appends
/// do not disturb a scrolled-back viewport). It advances by whole lines when
/// the tile is `AtTail` (spec §3.2).
#[derive(Debug, Clone, Copy, Serialize)]
struct PortalAppendGeometry {
    /// Estimated total content height (physical pixels) of the visible
    /// transcript after this append, computed as:
    ///   `total_lines * line_height_px`
    ///
    /// `total_lines` counts actual rendered lines across all visible transcript
    /// units (using `.lines().count().max(1)` per unit) so that multiline
    /// `TranscriptUnit.output_text` values are counted correctly.
    ///
    /// This is a whole-line estimate consistent with the text shaper's
    /// `content_height = lines * line_height` contract. It is suitable for
    /// `notify_tile_content_appended`'s `new_content_height_px` argument.
    pub new_content_height_px: f32,
    /// Visible viewport height of the portal tile in physical pixels.
    ///
    /// Taken from `state.geometry_batch.latest.rect.height_px` when a geometry
    /// snapshot is present; otherwise falls back to the adapter's configured
    /// bounds for the current presentation mode — `expanded_bounds.height` when
    /// Expanded, `compact_bounds.height` when Collapsed.
    ///
    /// The caller passes this as `viewport_height_px` to
    /// `notify_tile_content_appended`.
    pub viewport_height_px: f32,
    /// Logical line height in physical pixels, computed as:
    ///   `transcript_font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER`
    ///
    /// Must match the value used by the text shaper when laying out this
    /// portal's transcript lines. Passed as `line_height_px` to
    /// `notify_tile_content_appended` so follow-tail advancement is snapped
    /// to whole lines.
    pub line_height_px: f32,
}

/// Geometry data the runtime needs to call
/// `InputProcessor::notify_head_content_removed` after a head-trim.
///
/// Present in `CliPortalDrainRecord::head_trim_geometry` when the visible
/// transcript shrank between two consecutive `RenderPortal` drains (i.e. the
/// 64 KiB coalescer cap or the 16 KiB visible-window cap trimmed the head).
///
/// # How to use (runtime side)
///
/// After reading a record with `head_trim_geometry = Some(g)`, call:
///
/// ```text
/// input_processor.notify_head_content_removed(tile_id, g.removed_height_px);
/// ```
///
/// This must be called **before** `notify_tile_content_appended` so that
/// `ScrollTileState::total_content_height_px` is up to date when the
/// follow-tail bound is recomputed (spec §3.3 / hud-pkg2g).
#[derive(Debug, Clone, Copy, Serialize)]
struct PortalHeadTrimGeometry {
    /// Estimated height of the removed head content in physical pixels.
    ///
    /// Computed as `prev_content_height_px - new_content_height_px`.
    pub removed_height_px: f32,
}

/// Serialized form of one drained portal update produced by the drive loop.
///
/// Drain records are embedded in `CliOperationResult.portal_drain` and
/// serialized as part of the single JSON line emitted per operation. They are
/// NOT emitted as separate stdout lines; the one-JSON-line-per-operation
/// invariant is preserved.
///
/// This is the "present path" output: a coalesced portal transcript update
/// that the caller should forward to the resident gRPC session (build and send
/// a `session_proto::ClientMessage` using the `portal_markdown` from
/// `projected_portal_state` applied through `ResidentGrpcPortalAdapter`).
///
/// Carries semantic data (transcript, state, geometry, budget) rather than
/// raw proto bytes so the stdio surface stays protocol-agnostic; the gRPC
/// session layer (which has prost in scope) builds the proto message.
#[derive(Debug, Serialize)]
struct CliPortalDrainRecord {
    /// Projection ID that received this update.
    projection_id: String,
    /// Kind of resident gRPC command produced.
    command_kind: CliResidentCommandKind,
    /// Number of transcript units included in this update.
    visible_transcript_units: usize,
    /// Byte count of visible transcript.
    visible_transcript_bytes: usize,
    /// Number of output calls coalesced into this update.
    coalesced_output_count: usize,
    /// Total unread output count drained by this update.
    unread_output_count: usize,
    /// Wall-clock submission timestamp (µs) of the most-recently-coalesced
    /// append (arrival→present latency anchor, tasks.md §5.7).
    ///
    /// The drain loop emits a `tracing::debug!` event with the arrival→present
    /// delta (`server_timestamp_wall_us - submitted_at_us`) for each drained
    /// portal update (hud-bq0gl.14).
    submitted_at_us: u64,
    /// Rendered portal markdown content (from `portal_node` via the adapter).
    ///
    /// The caller uses this to populate the `TextMarkdownNodeProto.content`
    /// field in the outbound `ClientMessage`. This is the token-styled portal
    /// text with transcript, composer display, and caret already applied.
    portal_markdown: String,
    /// Presentation state of this update (`expanded` or `collapsed`).
    presentation: String,
    /// Tile ID previously assigned by `record_created_tile`, if any.
    /// `None` means the caller must send a `CreateTile` mutation first.
    tile_id_hex: Option<String>,
    /// Elapsed microseconds building this command (budget evidence).
    elapsed_us: u64,
    /// Budget ceiling for this command kind (µs).
    budget_us: u64,
    /// True when `elapsed_us <= budget_us`.
    within_budget: bool,
    /// Geometry for the runtime to call `InputProcessor::notify_tile_content_appended`.
    ///
    /// Present only for `command_kind = render_portal` (content appended to an
    /// existing tile). `None` for `create_portal_tile` (first render) and
    /// `release_lease`.
    ///
    /// After reading this record the runtime MUST call
    /// `input_processor.notify_tile_content_appended(tile_id, g.new_content_height_px,
    /// g.viewport_height_px, g.line_height_px, &mut scene)` to advance the
    /// follow-tail scroll position (spec §3.2) or leave it unchanged when the
    /// tile is scrolled-back (spec §3.3).
    ///
    /// **Wired by hud-0528i.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    append_geometry: Option<PortalAppendGeometry>,
    /// Head-trim geometry for the runtime to call
    /// `InputProcessor::notify_head_content_removed`.
    ///
    /// Present when the visible transcript shrank between two consecutive
    /// `RenderPortal` drains, indicating that the 64 KiB coalescer cap or the
    /// 16 KiB visible-window cap trimmed head content.
    ///
    /// When `Some(g)`, the runtime MUST call:
    ///   `input_processor.notify_head_content_removed(tile_id, g.removed_height_px)`
    /// **before** `notify_tile_content_appended` so that `ScrollTileState`
    /// content-height fields are up to date for the follow-tail recomputation.
    /// This keeps a scrolled-back viewport visually stable (spec §3.3 / hud-pkg2g).
    ///
    /// **Wired by hud-pkg2g.**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head_trim_geometry: Option<PortalHeadTrimGeometry>,
}

/// Serializable variant of `ResidentGrpcPortalCommandKind`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum CliResidentCommandKind {
    CreatePortalTile,
    ReusePortalTile,
    RenderPortal,
    ReleaseLease,
}

impl From<ResidentGrpcPortalCommandKind> for CliResidentCommandKind {
    fn from(kind: ResidentGrpcPortalCommandKind) -> Self {
        match kind {
            ResidentGrpcPortalCommandKind::CreatePortalTile => Self::CreatePortalTile,
            ResidentGrpcPortalCommandKind::ReusePortalTile => Self::ReusePortalTile,
            ResidentGrpcPortalCommandKind::RenderPortal => Self::RenderPortal,
            ResidentGrpcPortalCommandKind::ReleaseLease => Self::ReleaseLease,
        }
    }
}

/// `SetTokenMap` operation payload — accepted as a JSON stdin line with
/// `"operation": "set_token_map"`.
#[derive(Debug, Deserialize)]
struct SetTokenMapRequest {
    /// Request ID for correlation in responses.
    #[serde(default)]
    request_id: String,
    /// Flat design-token override map (key → CSS-style value string).
    /// Only portal-relevant tokens are consumed. Unrecognised keys are silently
    /// ignored. An empty map resets to built-in defaults.
    token_overrides: HashMap<String, String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

/// Initialise the tracing subscriber.
///
/// Mirrors the main app's setup (`app/tze_hud_app/src/main.rs`):
/// - `TZE_HUD_LOG` — env-filter directive (e.g. `tze_hud_projection=debug`).
/// - `TZE_HUD_LOG_JSON=1` — switch to JSON-formatted output.
///
/// To enable debug diagnostics for this daemon, run:
/// ```sh
/// TZE_HUD_LOG=debug ./tze_hud_projection_authority --stdio
/// ```
/// For structured JSON output (e.g. to pipe into `jq`):
/// ```sh
/// TZE_HUD_LOG=info TZE_HUD_LOG_JSON=1 ./tze_hud_projection_authority --stdio
/// ```
fn init_tracing() {
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
}

fn run() -> Result<(), String> {
    init_tracing();

    let config = parse_args(env::args().skip(1))?;
    if config.mode == CliMode::DemoPlan {
        return write_demo_plan(&config);
    }

    let mut authority = ProjectionAuthority::new(ProjectionBounds::default())
        .map_err(|error| format!("failed to initialize projection authority: {error}"))?;
    if let Some(operator_authority) = config.operator_authority.as_deref() {
        authority
            .set_operator_authority(operator_authority)
            .map_err(|error| format!("invalid operator authority: {error}"))?;
    }

    info!(
        caller_identity = %config.caller_identity,
        operator_authority_set = config.operator_authority.is_some(),
        "projection authority starting"
    );

    let mut portal_drive = PortalDriveState::new();
    let result = serve_stdio(&mut authority, &config, &mut portal_drive);

    match &result {
        Ok(()) => info!(
            caller_identity = %config.caller_identity,
            "projection authority shut down (EOF)"
        ),
        Err(error) => warn!(
            caller_identity = %config.caller_identity,
            %error,
            "projection authority terminated with error"
        ),
    }

    result
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliConfig, String> {
    let mut mode = CliMode::Stdio;
    let mut caller_identity = DEFAULT_CALLER_IDENTITY.to_string();
    let mut operator_authority = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--stdio" => {}
            "--demo-plan" => {
                mode = CliMode::DemoPlan;
            }
            "--caller-identity" => {
                caller_identity = iter
                    .next()
                    .ok_or("--caller-identity requires a value".to_string())?;
            }
            "--operator-authority" => {
                return Err(
                    "--operator-authority is not supported; use --operator-authority-env VAR"
                        .to_string(),
                );
            }
            "--operator-authority-env" => {
                let var = iter
                    .next()
                    .ok_or("--operator-authority-env requires a variable name".to_string())?;
                operator_authority = Some(
                    env::var(&var).map_err(|_| format!("{var} is not set in the environment"))?,
                );
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }

    Ok(CliConfig {
        mode,
        caller_identity,
        operator_authority,
    })
}

fn print_help() {
    println!(
        "Usage: tze_hud_projection_authority [--stdio|--demo-plan] [--caller-identity ID] [--operator-authority-env VAR]\n\
         \n\
         Reads one cooperative HUD projection operation JSON object per stdin line and writes one JSON result per stdout line.\n\
         The process retains projection state in memory only and emits bounded operation responses plus newly written audit records.\n\
         --demo-plan emits a redacted three-session zone/widget/portal route-plan artifact without connecting to the HUD."
    );
}

fn write_demo_plan(config: &CliConfig) -> Result<(), String> {
    let mut authority = ExternalAgentProjectionAuthority::default();
    register_demo_target(&mut authority)?;

    let now = now_wall_us();
    for (offset, request) in demo_session_requests().into_iter().enumerate() {
        authority
            .manage_session(
                request,
                &config.caller_identity,
                now.saturating_add(offset as u64 + 1),
            )
            .map_err(|error| format!("failed to build demo plan: {error}"))?;
    }

    let route_plans = authority.three_session_demo_plan();
    let output = DemoPlanOutput {
        demo_name: "external-agent-projection-authority-three-session-demo".to_string(),
        hud_target_id: "windows-local".to_string(),
        zone_messages: zone_messages_for_demo(&route_plans),
        widget_messages: widget_messages_for_demo(&route_plans),
        portal_routes: portal_routes_for_demo(&route_plans),
        lifecycle_checks: lifecycle_checks_for_demo(&config.caller_identity)?,
        route_plans,
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &output)
        .map_err(|error| format!("failed to encode demo plan: {error}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| format!("failed to write demo plan: {error}"))
}

fn register_demo_target(authority: &mut ExternalAgentProjectionAuthority) -> Result<(), String> {
    authority
        .register_windows_target(demo_windows_target())
        .map_err(|error| format!("invalid demo HUD target: {error}"))
}

fn demo_windows_target() -> WindowsHudTarget {
    WindowsHudTarget {
        target_id: "windows-local".to_string(),
        mcp_url: Some(
            std::env::var("TZE_HUD_DEMO_MCP_URL")
                .unwrap_or_else(|_| "http://windows-host.example:9090/mcp".to_string()),
        ),
        grpc_endpoint: Some(
            std::env::var("TZE_HUD_DEMO_GRPC")
                .unwrap_or_else(|_| "windows-host.example:50051".to_string()),
        ),
        credential_source: HudCredentialSource::EnvVar("TZE_HUD_PSK".to_string()),
        runtime_audience: "local-windows-hud".to_string(),
    }
}

fn zone_messages_for_demo(
    route_plans: &[tze_hud_projection::ManagedSessionRoutePlan],
) -> Vec<Value> {
    route_plans
        .iter()
        .filter_map(|plan| match &plan.surface_command {
            tze_hud_projection::HudSurfaceCommandPlan::ZonePublish {
                zone_name,
                content_kind,
                ttl_ms,
                agent_id,
            } => Some(serde_json::json!({
                "zone_name": zone_name,
                "content": {
                    "type": "status_bar",
                    "entries": {
                        "agent": plan.display_name,
                        "provider": plan.provider_kind,
                        "state": plan.lifecycle_state,
                        "kind": content_kind,
                    },
                },
                "merge_key": plan.projection_id,
                "namespace": agent_id,
                "ttl_us": ttl_ms.saturating_mul(1_000),
            })),
            _ => None,
        })
        .collect()
}

fn widget_messages_for_demo(
    route_plans: &[tze_hud_projection::ManagedSessionRoutePlan],
) -> Vec<Value> {
    route_plans
        .iter()
        .filter_map(|plan| match &plan.surface_command {
            tze_hud_projection::HudSurfaceCommandPlan::WidgetPublish {
                widget_name,
                parameters,
                ttl_ms,
                agent_id,
            } => Some(serde_json::json!({
                "widget_name": widget_name,
                "params": parameters
                    .iter()
                    .map(|(key, value)| (key.clone(), widget_value_to_json(value)))
                    .collect::<serde_json::Map<String, Value>>(),
                "namespace": agent_id,
                "ttl_us": ttl_ms.saturating_mul(1_000),
            })),
            _ => None,
        })
        .collect()
}

fn portal_routes_for_demo(
    route_plans: &[tze_hud_projection::ManagedSessionRoutePlan],
) -> Vec<Value> {
    route_plans
        .iter()
        .filter_map(|plan| match &plan.surface_command {
            tze_hud_projection::HudSurfaceCommandPlan::PortalLease {
                portal_surface,
                portal_id,
                requested_capabilities,
                lease_ttl_ms,
                agent_id,
            } => Some(serde_json::json!({
                "projection_id": plan.projection_id,
                "portal_surface": portal_surface,
                "portal_id": portal_id,
                "agent_id": agent_id,
                "requested_capabilities": requested_capabilities,
                "lease_ttl_ms": lease_ttl_ms,
                "materialization": "first_class_surface_raw_tile",
                "replay": "resident_grpc_text_stream_portal",
            })),
            _ => None,
        })
        .collect()
}

fn lifecycle_checks_for_demo(caller_identity: &str) -> Result<Vec<Value>, String> {
    Ok(vec![
        revoke_isolation_check(caller_identity)?,
        expiry_cleanup_check(caller_identity)?,
        reconnect_fresh_lease_check(caller_identity)?,
        provider_process_supervision_check(caller_identity)?,
    ])
}

fn revoke_isolation_check(caller_identity: &str) -> Result<Value, String> {
    let mut authority = ExternalAgentProjectionAuthority::default();
    register_demo_target(&mut authority)?;
    for (offset, request) in demo_session_requests().into_iter().enumerate() {
        authority
            .manage_session(request, caller_identity, 10 + offset as u64)
            .map_err(|error| format!("revoke check manage_session failed: {error}"))?;
    }
    authority
        .revoke_session("agent-progress")
        .map_err(|error| format!("revoke check failed: {error}"))?;
    Ok(serde_json::json!({
        "check": "revoke_isolation",
        "accepted": authority.managed_session_count() == 2
            && authority.route_plan("agent-progress").is_none()
            && authority.route_plan("agent-status").is_some()
            && authority.route_plan("agent-question").is_some(),
        "remaining_sessions": authority.managed_session_count(),
        "revoked_projection_absent": authority.route_plan("agent-progress").is_none(),
        "other_routes_intact": [
            authority.route_plan("agent-status").is_some(),
            authority.route_plan("agent-question").is_some(),
        ],
    }))
}

fn expiry_cleanup_check(caller_identity: &str) -> Result<Value, String> {
    let mut authority = ExternalAgentProjectionAuthority::new(ProjectionBounds {
        owner_token_ttl_wall_us: 20,
        ..ProjectionBounds::default()
    })
    .map_err(|error| format!("expiry check authority init failed: {error}"))?;
    register_demo_target(&mut authority)?;
    authority
        .manage_session(managed_session_by_id("agent-status")?, caller_identity, 10)
        .map_err(|error| format!("expiry check status manage_session failed: {error}"))?;
    authority
        .manage_session(
            managed_session_by_id("agent-question")?,
            caller_identity,
            11,
        )
        .map_err(|error| format!("expiry check portal manage_session failed: {error}"))?;
    let first_expired = authority.expire_token_expired_sessions(30);
    let second_expired = authority.expire_token_expired_sessions(31);
    Ok(serde_json::json!({
        "check": "expiry_cleanup",
        "accepted": first_expired == 1
            && second_expired == 1
            && authority.managed_session_count() == 0,
        "first_expired_count": first_expired,
        "second_expired_count": second_expired,
        "remaining_sessions": authority.managed_session_count(),
    }))
}

fn reconnect_fresh_lease_check(caller_identity: &str) -> Result<Value, String> {
    let mut authority = ExternalAgentProjectionAuthority::default();
    register_demo_target(&mut authority)?;
    authority
        .manage_session(
            managed_session_by_id("agent-question")?,
            caller_identity,
            10,
        )
        .map_err(|error| format!("reconnect check manage_session failed: {error}"))?;
    authority
        .record_hud_connection(
            "agent-question",
            HudConnectionMetadata {
                connection_id: "connection-1".to_string(),
                authenticated_session_id: "runtime-session-1".to_string(),
                granted_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                ],
                connected_at_wall_us: 20,
                last_reconnect_wall_us: 20,
            },
        )
        .map_err(|error| format!("reconnect check initial connection failed: {error}"))?;
    authority
        .projection_authority_mut()
        .record_advisory_lease(
            "agent-question",
            AdvisoryLeaseIdentity {
                lease_id: "lease-1".to_string(),
                capabilities: vec!["create_tiles".to_string()],
                acquired_at_wall_us: 21,
                expires_at_wall_us: 100,
            },
            22,
        )
        .map_err(|error| format!("reconnect check initial lease failed: {error}"))?;
    authority
        .mark_hud_disconnected("agent-question", 30)
        .map_err(|error| format!("reconnect check disconnect failed: {error}"))?;
    authority
        .record_hud_connection(
            "agent-question",
            HudConnectionMetadata {
                connection_id: "connection-2".to_string(),
                authenticated_session_id: "runtime-session-2".to_string(),
                granted_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                ],
                connected_at_wall_us: 40,
                last_reconnect_wall_us: 40,
            },
        )
        .map_err(|error| format!("reconnect check reconnect failed: {error}"))?;
    let stale_rejected = authority
        .projection_authority_mut()
        .authorize_portal_republish(
            "agent-question",
            "lease-1",
            &["create_tiles".to_string()],
            41,
        )
        == Err(ProjectionErrorCode::ProjectionUnauthorized);
    authority
        .projection_authority_mut()
        .record_advisory_lease(
            "agent-question",
            AdvisoryLeaseIdentity {
                lease_id: "lease-2".to_string(),
                capabilities: vec!["create_tiles".to_string()],
                acquired_at_wall_us: 42,
                expires_at_wall_us: 100,
            },
            43,
        )
        .map_err(|error| format!("reconnect check fresh lease failed: {error}"))?;
    let fresh_authorized = authority
        .projection_authority_mut()
        .authorize_portal_republish(
            "agent-question",
            "lease-2",
            &["create_tiles".to_string()],
            44,
        )
        .is_ok();
    Ok(serde_json::json!({
        "check": "reconnect_requires_fresh_lease",
        "accepted": stale_rejected && fresh_authorized,
        "stale_lease_rejected": stale_rejected,
        "fresh_lease_authorized": fresh_authorized,
    }))
}

fn provider_process_supervision_check(caller_identity: &str) -> Result<Value, String> {
    let mut authority = ExternalAgentProjectionAuthority::default();
    register_demo_target(&mut authority)?;
    let mut request = managed_session_by_id("agent-progress")?;
    let current_exe =
        env::current_exe().map_err(|error| format!("provider process check failed: {error}"))?;
    request.origin = ManagedSessionOrigin::Launched(tze_hud_projection::LaunchSessionSpec {
        command: current_exe.display().to_string(),
        args: vec!["--help".to_string()],
        working_directory: None,
        environment_keys: Vec::new(),
    });
    authority
        .manage_session(request, caller_identity, 10)
        .map_err(|error| format!("provider process check manage_session failed: {error}"))?;
    let launched = authority
        .launch_provider_process("agent-progress")
        .map_err(|error| format!("provider process check launch failed: {error}"))?;
    let mut final_status = launched.clone();
    for _ in 0..20 {
        final_status = authority
            .provider_process_status("agent-progress")
            .map_err(|error| format!("provider process check status failed: {error}"))?
            .ok_or("provider process check lost tracked process".to_string())?;
        if matches!(
            final_status.state,
            tze_hud_projection::ProviderProcessState::Exited { .. }
        ) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    authority
        .terminate_provider_process("agent-progress")
        .map_err(|error| format!("provider process check cleanup failed: {error}"))?;
    Ok(serde_json::json!({
        "check": "provider_process_supervision",
        "accepted": launched.process_id > 0
            && matches!(
                final_status.state,
                tze_hud_projection::ProviderProcessState::Exited { .. }
            )
            && authority.provider_process_status("agent-progress")
                .map_err(|error| format!("provider process check final status failed: {error}"))?
                .is_none(),
        "process_id_present": launched.process_id > 0,
        "final_state": final_status.state,
        "stdio_capture": "disabled",
    }))
}

fn managed_session_by_id(projection_id: &str) -> Result<ManagedSessionRequest, String> {
    demo_session_requests()
        .into_iter()
        .find(|request| request.projection_id == projection_id)
        .ok_or_else(|| format!("unknown demo projection id: {projection_id}"))
}

fn widget_value_to_json(value: &WidgetParameterValue) -> Value {
    match value {
        WidgetParameterValue::F32Milli(value) => serde_json::json!((*value as f64) / 1_000.0),
        WidgetParameterValue::Text(value) => serde_json::json!(value),
        WidgetParameterValue::ColorRgba([r, g, b, a]) => serde_json::json!({
            "r": (*r as f64) / 255.0,
            "g": (*g as f64) / 255.0,
            "b": (*b as f64) / 255.0,
            "a": (*a as f64) / 255.0,
        }),
        WidgetParameterValue::Enum(value) => serde_json::json!(value),
    }
}

fn demo_session_requests() -> Vec<ManagedSessionRequest> {
    let mut progress_parameters = HashMap::new();
    progress_parameters.insert("progress".to_string(), WidgetParameterValue::F32Milli(420));
    progress_parameters.insert(
        "label".to_string(),
        WidgetParameterValue::Text("External authority demo".to_string()),
    );

    vec![
        ManagedSessionRequest {
            projection_id: "agent-status".to_string(),
            provider_kind: ProviderKind::Codex,
            display_name: "Codex Status".to_string(),
            origin: ManagedSessionOrigin::Attached,
            hud_target_id: "windows-local".to_string(),
            surface_route: PresenceSurfaceRoute::Zone {
                zone_name: "status-bar".to_string(),
                content_kind: "status".to_string(),
                ttl_ms: 10_000,
            },
            content_classification: ContentClassification::Household,
            attention_intent: ProjectionAttentionIntent::Ambient,
            workspace_hint: Some("mayor/rig".to_string()),
            repository_hint: None,
            icon_profile_hint: None,
        },
        ManagedSessionRequest {
            projection_id: "agent-progress".to_string(),
            provider_kind: ProviderKind::Claude,
            display_name: "Claude Progress".to_string(),
            origin: ManagedSessionOrigin::Launched(tze_hud_projection::LaunchSessionSpec {
                command: "claude".to_string(),
                args: vec!["--continue".to_string()],
                working_directory: Some("/home/tze/gt/tze_hud/mayor/rig".to_string()),
                environment_keys: vec!["ANTHROPIC_API_KEY".to_string()],
            }),
            hud_target_id: "windows-local".to_string(),
            surface_route: PresenceSurfaceRoute::Widget {
                widget_name: "main-progress".to_string(),
                parameters: progress_parameters,
                ttl_ms: 10_000,
            },
            content_classification: ContentClassification::Private,
            attention_intent: ProjectionAttentionIntent::Ambient,
            workspace_hint: Some("mayor/rig".to_string()),
            repository_hint: None,
            icon_profile_hint: Some("claude".to_string()),
        },
        ManagedSessionRequest {
            projection_id: "agent-question".to_string(),
            provider_kind: ProviderKind::Opencode,
            display_name: "Opencode Questions".to_string(),
            origin: ManagedSessionOrigin::Attached,
            hud_target_id: "windows-local".to_string(),
            surface_route: PresenceSurfaceRoute::Portal {
                portal_surface: tze_hud_projection::PortalSurfaceKind::TextStreamRawTile,
                requested_capabilities: vec![
                    "create_tiles".to_string(),
                    "modify_own_tiles".to_string(),
                ],
                lease_ttl_ms: 30_000,
            },
            content_classification: ContentClassification::Private,
            attention_intent: ProjectionAttentionIntent::Gentle,
            workspace_hint: Some("mayor/rig".to_string()),
            repository_hint: None,
            icon_profile_hint: Some("opencode".to_string()),
        },
    ]
}

fn serve_stdio(
    authority: &mut ProjectionAuthority,
    config: &CliConfig,
    portal_drive: &mut PortalDriveState,
) -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = io::stdout().lock();

    loop {
        let result = match read_bounded_line(&mut stdin, MAX_STDIN_LINE_BYTES)
            .map_err(|error| format!("failed to read stdin: {error}"))?
        {
            StdinLine::Line(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                dispatch_line(authority, config, portal_drive, &line)
            }
            StdinLine::TooLong => {
                warn!(
                    max_bytes = MAX_STDIN_LINE_BYTES,
                    "stdin request line too long — truncated and rejected"
                );
                malformed_response(
                    "unknown",
                    "unknown",
                    now_wall_us(),
                    format!("stdin request line exceeds {MAX_STDIN_LINE_BYTES} bytes"),
                )
            }
            StdinLine::InvalidUtf8 => {
                warn!("stdin request line is not valid UTF-8 — rejected");
                malformed_response(
                    "unknown",
                    "unknown",
                    now_wall_us(),
                    "stdin request line is not valid UTF-8",
                )
            }
            StdinLine::Eof => break,
        };
        write_result(&mut stdout, &result)?;
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum StdinLine {
    Line(String),
    TooLong,
    InvalidUtf8,
    Eof,
}

fn read_bounded_line<R: BufRead>(reader: &mut R, max_bytes: usize) -> io::Result<StdinLine> {
    let mut bytes = Vec::new();

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(StdinLine::Eof);
            }
            break;
        }

        if let Some(newline_index) = available.iter().position(|byte| *byte == b'\n') {
            let take = newline_index + 1;
            if bytes.len() + take > max_bytes {
                reader.consume(take);
                return Ok(StdinLine::TooLong);
            }
            bytes.extend_from_slice(&available[..take]);
            reader.consume(take);
            break;
        }

        if bytes.len() + available.len() > max_bytes {
            let consumed = available.len();
            reader.consume(consumed);
            drain_until_newline(reader)?;
            return Ok(StdinLine::TooLong);
        }

        bytes.extend_from_slice(available);
        let consumed = available.len();
        reader.consume(consumed);
    }

    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.ends_with(b"\r") {
            bytes.pop();
        }
    }

    match String::from_utf8(bytes) {
        Ok(line) => Ok(StdinLine::Line(line)),
        Err(_) => Ok(StdinLine::InvalidUtf8),
    }
}

fn drain_until_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        if let Some(newline_index) = available.iter().position(|byte| *byte == b'\n') {
            reader.consume(newline_index + 1);
            return Ok(());
        }
        let consumed = available.len();
        reader.consume(consumed);
    }
}

fn write_result(mut stdout: impl Write, result: &CliOperationResult) -> Result<(), String> {
    serde_json::to_writer(&mut stdout, result)
        .map_err(|error| format!("failed to encode response: {error}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| format!("failed to write response: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("failed to flush response: {error}"))
}

fn dispatch_line(
    authority: &mut ProjectionAuthority,
    config: &CliConfig,
    portal_drive: &mut PortalDriveState,
    line: &str,
) -> CliOperationResult {
    let server_timestamp_wall_us = now_wall_us();
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value) => value,
        Err(error) => {
            warn!(%error, "invalid JSON request — rejected");
            return malformed_response(
                "unknown",
                "unknown",
                server_timestamp_wall_us,
                format!("invalid JSON request: {error}"),
            );
        }
    };

    // Special-case: "set_token_map" is not a `ProjectionOperation` variant —
    // it targets the portal drive layer, not the authority. Handle it first.
    if value
        .get("operation")
        .and_then(Value::as_str)
        .map(|op| op == "set_token_map")
        .unwrap_or(false)
    {
        return dispatch_set_token_map(value, portal_drive, server_timestamp_wall_us);
    }

    let caller_identity = config.caller_identity.as_str();
    let Some(operation_value) = value.get("operation") else {
        return malformed_response(
            request_id(&value),
            projection_id(&value),
            server_timestamp_wall_us,
            "operation is required",
        );
    };
    let operation = match serde_json::from_value::<ProjectionOperation>(operation_value.clone()) {
        Ok(operation) => operation,
        Err(error) => {
            return malformed_response(
                request_id(&value),
                projection_id(&value),
                server_timestamp_wall_us,
                format!("invalid operation: {error}"),
            );
        }
    };

    let audit_start = authority.audit_log().len();
    let response = match operation {
        ProjectionOperation::Attach => deserialize_then(value, |request: AttachRequest| {
            // Register an adapter for this projection on successful attach.
            // A placeholder lease ID (empty vec) is used here because the gRPC
            // session may not yet be live at attach time. The resident session
            // layer is responsible for supplying the real lease ID in any
            // MutationBatch it sends; the placeholder is never sent on the wire.
            // Part (a): adapter hosted here with runtime-resolved tokens.
            let proj_id = request.envelope.projection_id.clone();
            let provider = format!("{:?}", request.provider_kind);
            let result =
                authority.handle_attach(request, caller_identity, server_timestamp_wall_us);
            if result.accepted {
                portal_drive.attach_adapter(&proj_id, Vec::new());
                info!(
                    projection_id = %proj_id,
                    provider_kind = %provider,
                    "session attached"
                );
            } else {
                warn!(
                    projection_id = %proj_id,
                    provider_kind = %provider,
                    error_code = ?result.error_code,
                    status_summary = %result.status_summary,
                    "attach rejected"
                );
            }
            result
        }),
        ProjectionOperation::List => {
            deserialize_list_then(value, |request: ListProjectionsRequest| {
                let result = authority.handle_list_projections(
                    request,
                    caller_identity,
                    server_timestamp_wall_us,
                );
                if result.accepted {
                    debug!(projection_count = result.projections.len(), "list accepted");
                } else {
                    warn!(
                        error_code = ?result.error_code,
                        status_summary = %result.status_summary,
                        "list rejected"
                    );
                }
                result
            })
        }
        ProjectionOperation::PublishOutput => {
            deserialize_then(value, |request: PublishOutputRequest| {
                let proj_id = request.envelope.projection_id.clone();
                let result = authority.handle_publish_output(
                    request,
                    caller_identity,
                    server_timestamp_wall_us,
                );
                if result.accepted {
                    debug!(
                        projection_id = %proj_id,
                        coalesced_output_count = result.coalesced_output_count,
                        portal_update_ready = result.portal_update_ready,
                        "publish_output accepted"
                    );
                } else {
                    warn!(
                        projection_id = %proj_id,
                        error_code = ?result.error_code,
                        status_summary = %result.status_summary,
                        "publish_output rejected"
                    );
                }
                result
            })
        }
        ProjectionOperation::PublishStatus => {
            deserialize_then(value, |request: PublishStatusRequest| {
                let proj_id = request.envelope.projection_id.clone();
                let result = authority.handle_publish_status(
                    request,
                    caller_identity,
                    server_timestamp_wall_us,
                );
                if result.accepted {
                    debug!(
                        projection_id = %proj_id,
                        "publish_status accepted"
                    );
                } else {
                    warn!(
                        projection_id = %proj_id,
                        error_code = ?result.error_code,
                        status_summary = %result.status_summary,
                        "publish_status rejected"
                    );
                }
                result
            })
        }
        ProjectionOperation::GetPendingInput => {
            deserialize_then(value, |request: GetPendingInputRequest| {
                let proj_id = request.envelope.projection_id.clone();
                let result = authority.handle_get_pending_input(
                    request,
                    caller_identity,
                    server_timestamp_wall_us,
                );
                if result.accepted {
                    debug!(
                        projection_id = %proj_id,
                        pending_remaining_count = result.pending_remaining_count,
                        "get_pending_input accepted"
                    );
                } else {
                    warn!(
                        projection_id = %proj_id,
                        error_code = ?result.error_code,
                        "get_pending_input rejected"
                    );
                }
                result
            })
        }
        ProjectionOperation::AcknowledgeInput => {
            deserialize_then(value, |request: AcknowledgeInputRequest| {
                let proj_id = request.envelope.projection_id.clone();
                let result = authority.handle_acknowledge_input(
                    request,
                    caller_identity,
                    server_timestamp_wall_us,
                );
                if result.accepted {
                    debug!(
                        projection_id = %proj_id,
                        "acknowledge_input accepted"
                    );
                } else {
                    warn!(
                        projection_id = %proj_id,
                        error_code = ?result.error_code,
                        "acknowledge_input rejected"
                    );
                }
                result
            })
        }
        ProjectionOperation::Detach => deserialize_then(value, |request: DetachRequest| {
            let proj_id = request.envelope.projection_id.clone();
            let result =
                authority.handle_detach(request, caller_identity, server_timestamp_wall_us);
            if result.accepted {
                portal_drive.detach_adapter(&proj_id);
                info!(
                    projection_id = %proj_id,
                    "session detached"
                );
            } else {
                warn!(
                    projection_id = %proj_id,
                    error_code = ?result.error_code,
                    status_summary = %result.status_summary,
                    "detach rejected"
                );
            }
            result
        }),
        ProjectionOperation::Cleanup => deserialize_then(value, |request: CleanupRequest| {
            let proj_id = request.envelope.projection_id.clone();
            let result =
                authority.handle_cleanup(request, caller_identity, server_timestamp_wall_us);
            if result.accepted {
                portal_drive.detach_adapter(&proj_id);
                info!(
                    projection_id = %proj_id,
                    "session cleaned up"
                );
            } else {
                warn!(
                    projection_id = %proj_id,
                    error_code = ?result.error_code,
                    status_summary = %result.status_summary,
                    "cleanup rejected"
                );
            }
            result
        }),
    };

    let audit_records = new_audit_records(authority, audit_start, &response);
    // Part (b): drain portal updates into the present path after any operation
    // that could advance the coalescer (PublishOutput is the primary trigger,
    // but we drain after every operation for work-conserving behaviour).
    // Drain records are embedded in the operation result (one JSON line per
    // operation) so callers read a single line regardless of drain size.
    let portal_drain =
        drain_and_emit_portal_updates(authority, portal_drive, server_timestamp_wall_us);
    CliOperationResult {
        response,
        audit_records,
        portal_drain,
    }
}

/// Handle the out-of-band `set_token_map` operation (portal drive layer only).
///
/// Not a `ProjectionOperation` — dispatched separately so that
/// `ProjectionAuthority` state is not touched. Returns a `CliOperationResult`
/// with the token-swap outcome embedded in the response.
fn dispatch_set_token_map(
    value: Value,
    portal_drive: &mut PortalDriveState,
    server_timestamp_wall_us: u64,
) -> CliOperationResult {
    let req: SetTokenMapRequest = match serde_json::from_value(value) {
        Ok(req) => req,
        Err(error) => {
            return CliOperationResult {
                response: invalid_argument_response(
                    "unknown".to_string(),
                    "set_token_map".to_string(),
                    server_timestamp_wall_us,
                    format!("invalid set_token_map payload: {error}"),
                ),
                audit_records: Vec::new(),
                portal_drain: Vec::new(),
            };
        }
    };

    let adapter_count = portal_drive.adapters.len();
    portal_drive.apply_token_map(req.token_overrides);
    debug!(adapter_count, "token map applied to portal drive adapters");
    // Note: no authority available here so the drain loop cannot run.
    // The caller will drain on the next PublishOutput operation.
    CliOperationResult {
        response: ProjectionResponse {
            request_id: req.request_id,
            projection_id: "set_token_map".to_string(),
            accepted: true,
            error_code: None,
            server_timestamp_wall_us,
            status_summary: format!("token map applied; {adapter_count} adapter(s) updated"),
            owner_token: None,
            lifecycle_state: None,
            projections: Vec::new(),
            pending_input: Vec::new(),
            pending_remaining_count: 0,
            pending_remaining_bytes: 0,
            portal_update_ready: false,
            coalesced_output_count: 0,
        },
        audit_records: Vec::new(),
        portal_drain: Vec::new(),
    }
}

/// Work-conserving drain loop: pull all due portal updates round-robin and
/// convert them through the resident gRPC adapter into `CliPortalDrainRecord`
/// lines for the caller to forward to the live HUD session.
///
/// Round-robin fairness is enforced by `next_due_projection_id()`: portals are
/// served in the order the coalescer tracks, preventing starvation under equal
/// sustained input rates (tasks.md §5.1, §5.4).
///
/// ## Follow-tail wiring (hud-0528i)
///
/// For every `RenderPortal` record, `append_geometry` carries the geometry
/// needed by the runtime to call `InputProcessor::notify_tile_content_appended`.
/// The runtime MUST make this call (spec §3.2 / §3.3) immediately after
/// forwarding the drain record to the gRPC session layer:
///
/// ```text
/// for record in drain_records {
///     if record.command_kind == "render_portal" {
///         if let Some(g) = record.append_geometry {
///             input_processor.notify_tile_content_appended(
///                 tile_id,               // SceneId from caller's registry
///                 g.new_content_height_px,
///                 g.viewport_height_px,
///                 g.line_height_px,
///                 &mut scene,
///             );
///         }
///     }
/// }
/// ```
///
/// ## Hook points
///
/// - **hud-ttq97** (telemetry bucket): `submitted_at_us` in each
///   `CliPortalDrainRecord` captures the arrival→present latency anchor.
///   A structured bucket should be emitted here once hud-ttq97 lands.
///
/// - **hud-pkg2g** (head-trim notify): wired. When the visible transcript
///   shrinks between two consecutive drains (64 KiB coalescer cap or 16 KiB
///   visible-window cap), the function detects the shrinkage and emits a
///   `head_trim_geometry` field in `CliPortalDrainRecord`. The runtime caller
///   uses `g.removed_height_px` to call
///   `InputProcessor::notify_head_content_removed` (spec §3.3 / hud-pkg2g).
fn drain_and_emit_portal_updates(
    authority: &mut ProjectionAuthority,
    portal_drive: &mut PortalDriveState,
    server_timestamp_wall_us: u64,
) -> Vec<CliPortalDrainRecord> {
    let mut records = Vec::new();
    let policy = ProjectedPortalPolicy::permit_all();

    loop {
        // Round-robin fairness oracle — returns None when no portal has a
        // pending update in the coalescer.
        let Some(proj_id) = authority.next_due_projection_id() else {
            break;
        };

        // Materialise the coalesced update for this portal.
        let update = match authority.take_due_portal_update(&proj_id, server_timestamp_wall_us) {
            Ok(Some(update)) => update,
            Ok(None) => {
                // Coalescer said ready but update is not yet due (rate window).
                // Do not spin; move to next portal or exit.
                break;
            }
            Err(_) => {
                // Projection not found or expired — clean up the adapter to
                // prevent leaks.
                //
                // LOOP-TERMINATION INVARIANT: we must also remove the pending
                // coalescer entry via `expire_projection` so that
                // `next_due_projection_id` no longer returns this `proj_id`.
                // Without this call the coalescer entry remains pending,
                // `next_due_projection_id` keeps returning the same stale key,
                // `take_due_portal_update` keeps returning `Err`, and the drain
                // loop spins forever (latent infinite loop, hud-hkaw2).
                portal_drive.detach_adapter(&proj_id);
                authority.expire_projection(&proj_id);
                continue;
            }
        };

        // Build the full projected portal state for rendering.
        let Some(state) = authority.projected_portal_state(&proj_id, &policy) else {
            // Session was removed between take_due and state query (race).
            // Clean up the adapter and continue draining other portals.
            portal_drive.detach_adapter(&proj_id);
            continue;
        };

        // Drive the adapter: determine command kind and render portal content.
        let adapter = match portal_drive.adapter_mut(&proj_id) {
            Some(adapter) => adapter,
            None => {
                // No adapter registered for this portal (session may have
                // used a non-portal surface). Skip silently.
                continue;
            }
        };

        // Render the portal markdown — the semantic content of the tile.
        // This is what portal_node places in TextMarkdownNodeProto::content.
        let portal_markdown = adapter.render_portal_markdown(&state, server_timestamp_wall_us);

        // Determine the command kind: CreatePortalTile if no tile yet registered,
        // RenderPortal otherwise. The gRPC session layer uses this to decide
        // whether to send a CreateTile mutation before the PublishToTile mutation.
        let command_kind = if adapter.tile_id().is_none() {
            ResidentGrpcPortalCommandKind::CreatePortalTile
        } else {
            ResidentGrpcPortalCommandKind::RenderPortal
        };

        // Capture tile_id (hex) for the caller to correlate tile operations.
        let tile_id_hex = adapter
            .tile_id()
            .map(|id| id.iter().map(|b| format!("{b:02x}")).collect());

        // Build a budget sample for the render work done above.
        // Dispatch to the correct path: if no tile has been created yet, use
        // ensure_portal_tile_message (which handles CreatePortalTile without
        // requiring a tile_id); otherwise use render_portal_message.
        let seq = server_timestamp_wall_us;
        let budget_result = if adapter.tile_id().is_none() {
            adapter.ensure_portal_tile_message(&state, seq, server_timestamp_wall_us)
        } else {
            adapter.render_portal_message(&state, seq, server_timestamp_wall_us)
        };
        let (elapsed_us, budget_us, within_budget) = match &budget_result {
            Ok(cmd) => (
                cmd.budget.elapsed_us,
                cmd.budget.budget_us,
                cmd.budget.within_budget(),
            ),
            Err(_) => (0, 0, false),
        };

        let presentation = match state.presentation {
            tze_hud_projection::ProjectedPortalPresentation::Expanded => "expanded".to_string(),
            tze_hud_projection::ProjectedPortalPresentation::Collapsed => "collapsed".to_string(),
        };

        // hud-0528i: build append_geometry for RenderPortal so the runtime can
        // call InputProcessor::notify_tile_content_appended (spec §3.2 / §3.3).
        //
        // Only set for RenderPortal (content appended to an existing tile). The
        // CreatePortalTile path is the first render — no prior content to advance.
        //
        // The geometry values are derived from:
        //   - line_height_px     = transcript_font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER
        //                         (matches tze_hud_compositor::text — same multiplier)
        //   - new_content_height = total_lines * line_height_px, where total_lines
        //                         counts actual text lines across all visible units
        //                         (a TranscriptUnit may contain embedded newlines)
        //   - viewport_height    = geometry_batch.latest.rect.height_px when a live
        //                         geometry snapshot is present; else the adapter's
        //                         configured bounds for the current presentation mode
        //                         (Expanded → expanded_bounds.height, Collapsed →
        //                         compact_bounds.height)
        let append_geometry = if matches!(command_kind, ResidentGrpcPortalCommandKind::RenderPortal)
        {
            let line_height_px =
                adapter.visual_tokens().transcript_font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
            // Count actual rendered lines: a TranscriptUnit's output_text can contain
            // embedded newlines (e.g. multiline assistant outputs, coalesced updates).
            // Using `.lines().count().max(1)` per unit avoids underestimating
            // new_content_height_px when units span multiple lines.
            //
            // §Viewer Reply Echo (two-pane INPUT/OUTPUT split): when Expanded,
            // `portal_markdown_with` renders the INPUT band (label +
            // `input_history` units + trailing blank) into the SAME single-node
            // markdown as the OUTPUT transcript above — Collapsed never renders
            // it. Omitting it here would undercount `new_content_height_px`
            // whenever a viewer has reply history, breaking scroll clamp /
            // follow-tail for long INPUT history.
            let mut total_lines: usize = update
                .visible_transcript
                .iter()
                .map(|unit| unit.output_text.lines().count().max(1))
                .sum();
            if matches!(
                state.presentation,
                tze_hud_projection::ProjectedPortalPresentation::Expanded
            ) && !state.input_history.is_empty()
            {
                total_lines += 2; // input-history label line + trailing blank
                total_lines += state
                    .input_history
                    .iter()
                    .map(|unit| unit.output_text.lines().count().max(1))
                    .sum::<usize>();
            }
            let new_content_height_px = total_lines as f32 * line_height_px;
            // Viewport height: prefer the live geometry snapshot (most accurate after
            // a resize), fall back to the adapter's configured bounds for the current
            // presentation mode (Collapsed uses compact bounds, Expanded uses expanded).
            let viewport_height_px = state
                .geometry_batch
                .as_ref()
                .and_then(|gb| gb.latest)
                .map(|snap| snap.rect.height_px as f32)
                .unwrap_or_else(|| adapter.config_viewport_height(state.presentation));
            Some(PortalAppendGeometry {
                new_content_height_px,
                viewport_height_px,
                line_height_px,
            })
        } else {
            None
        };

        // hud-pkg2g: detect head-trim and emit PortalHeadTrimGeometry.
        //
        // A content-height decrease between consecutive `RenderPortal` drains
        // indicates that content was removed from the head of the visible
        // transcript.  Two trim sites produce this:
        //   1. PortalCadenceCoalescer::record_append (64 KiB cap): drops oldest
        //      bytes from the payload snapshot to keep it within MAX_PORTAL_SNAPSHOT_BYTES.
        //   2. visible_transcript_window (16 KiB cap): slices the retained transcript
        //      to the newest max_visible_transcript_bytes bytes.
        //
        // Detection criterion: new_content_height_px < prev_content_height_px.
        // The height is the authoritative signal because the runtime caller uses
        // new_content_height_px (from append_geometry) as the total content height
        // for scroll accounting.  Any decrease — regardless of whether the byte
        // count also decreased — means the caller must receive a
        // notify_head_content_removed call before notify_tile_content_appended,
        // so that ScrollTileState::total_content_height_px is correct when the
        // follow-tail bound is recomputed (spec §3.3, hud-hkaw2 fix).
        //
        // Note: the previous dual-condition (bytes < prev_bytes && height <
        // prev_height) was overly restrictive — it would miss a height shrink
        // that occurred without a corresponding byte-count decrease (possible when
        // line counts change due to text reshaping without total byte loss).
        //
        // The runtime caller MUST call:
        //   input_processor.notify_head_content_removed(tile_id, g.removed_height_px)
        // BEFORE notify_tile_content_appended, so ScrollTileState content-height
        // fields are correct when the follow-tail bound is recomputed (spec §3.3).
        let head_trim_geometry = if let Some(ref ag) = append_geometry {
            let prev_height = portal_drive
                .prev_content_height_px
                .get(proj_id.as_str())
                .copied()
                .unwrap_or(0.0);
            if ag.new_content_height_px < prev_height {
                let removed_height_px = prev_height - ag.new_content_height_px;
                Some(PortalHeadTrimGeometry { removed_height_px })
            } else {
                None
            }
        } else {
            None
        };
        // Update per-portal height tracking for the next drain cycle (RenderPortal only).
        if let Some(ref ag) = append_geometry {
            portal_drive
                .prev_content_height_px
                .insert(proj_id.clone(), ag.new_content_height_px);
        }

        // hud-bq0gl.14: emit publish-to-present latency as a structured tracing
        // event so the stdio path surfaces the same observability signal as the
        // in-process driver.  The delta is the arrival→present latency for this
        // coalesced portal update.
        if update.submitted_at_us > 0 && server_timestamp_wall_us >= update.submitted_at_us {
            let delta_us = server_timestamp_wall_us - update.submitted_at_us;
            tracing::debug!(
                proj_id = %proj_id,
                submitted_at_us = update.submitted_at_us,
                server_timestamp_wall_us,
                delta_us,
                "portal drain: publish-to-present latency (stdio path)"
            );
        }

        records.push(CliPortalDrainRecord {
            projection_id: proj_id.clone(),
            command_kind: command_kind.into(),
            visible_transcript_units: update.visible_transcript.len(),
            visible_transcript_bytes: update.visible_transcript_bytes,
            coalesced_output_count: update.coalesced_output_count,
            unread_output_count: update.unread_output_count,
            submitted_at_us: update.submitted_at_us,
            portal_markdown,
            presentation,
            tile_id_hex,
            elapsed_us,
            budget_us,
            within_budget,
            append_geometry,
            head_trim_geometry,
        });
    }

    // hud-bq0gl.14: emit coalescer health snapshot after each drain cycle so
    // the stdio path surfaces coalescer fairness stats alongside the per-update
    // latency events above.
    if !records.is_empty() {
        tracing::debug!(
            drain_count = records.len(),
            coalescer_portal_count = authority.coalescer_portal_count(),
            coalescer_pending = authority.coalescer_pending_portal_count(),
            coalescer_total_taken = authority.coalescer_total_taken(),
            coalescer_total_coalesced = authority.coalescer_total_coalesced(),
            "portal drain: coalescer health snapshot (stdio path)"
        );
    }

    records
}

fn new_audit_records(
    authority: &ProjectionAuthority,
    audit_start: usize,
    response: &ProjectionResponse,
) -> Vec<ProjectionAuditRecord> {
    let audit_log = authority.audit_log();
    if audit_log.len() > audit_start {
        return audit_log[audit_start..].to_vec();
    }

    audit_log
        .last()
        .filter(|record| {
            record.request_id == response.request_id
                && record.projection_id == response.projection_id
                && record.timestamp_wall_us == response.server_timestamp_wall_us
        })
        .cloned()
        .into_iter()
        .collect()
}

fn deserialize_then<T>(
    value: Value,
    dispatch: impl FnOnce(T) -> ProjectionResponse,
) -> ProjectionResponse
where
    T: for<'de> Deserialize<'de>,
{
    let request_id = request_id(&value);
    let projection_id = projection_id(&value);
    match serde_json::from_value(value) {
        Ok(request) => dispatch(request),
        Err(error) => invalid_argument_response(
            request_id,
            projection_id,
            now_wall_us(),
            format!("invalid operation payload: {error}"),
        ),
    }
}

/// The dispatcher consumes `operation`; strict list deserialization owns every
/// remaining caller-provided field.
fn deserialize_list_then(
    mut value: Value,
    dispatch: impl FnOnce(ListProjectionsRequest) -> ProjectionResponse,
) -> ProjectionResponse {
    if let Value::Object(fields) = &mut value {
        fields.remove("operation");
    }
    deserialize_then(value, dispatch)
}

fn malformed_response(
    request_id: impl Into<String>,
    projection_id: impl Into<String>,
    server_timestamp_wall_us: u64,
    reason: impl Into<String>,
) -> CliOperationResult {
    CliOperationResult {
        response: invalid_argument_response(
            request_id.into(),
            projection_id.into(),
            server_timestamp_wall_us,
            reason,
        ),
        audit_records: Vec::new(),
        portal_drain: Vec::new(),
    }
}

fn invalid_argument_response(
    request_id: String,
    projection_id: String,
    server_timestamp_wall_us: u64,
    reason: impl Into<String>,
) -> ProjectionResponse {
    ProjectionResponse {
        request_id,
        projection_id,
        accepted: false,
        error_code: Some(ProjectionErrorCode::ProjectionInvalidArgument),
        server_timestamp_wall_us,
        status_summary: bounded_copy(reason.into(), MAX_CLI_STATUS_SUMMARY_BYTES),
        owner_token: None,
        lifecycle_state: None,
        projections: Vec::new(),
        pending_input: Vec::new(),
        pending_remaining_count: 0,
        pending_remaining_bytes: 0,
        portal_update_ready: false,
        coalesced_output_count: 0,
    }
}

fn request_id(value: &Value) -> String {
    value
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn projection_id(value: &Value) -> String {
    value
        .get("projection_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn now_wall_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64
}

fn bounded_copy(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tze_hud_projection::PORTAL_UPDATE_RATE_WINDOW_WALL_US;
    use tze_hud_projection::{
        AttachRequest, ContentClassification, OperationEnvelope, OutputKind, ProjectionAuthority,
        ProjectionBounds, ProjectionOperation, ProviderKind, PublishOutputRequest,
    };

    // ── Test helpers ──────────────────────────────────────────────────────────

    fn test_envelope(
        operation: ProjectionOperation,
        projection_id: &str,
        request_id: &str,
    ) -> OperationEnvelope {
        OperationEnvelope {
            operation,
            projection_id: projection_id.to_string(),
            request_id: request_id.to_string(),
            client_timestamp_wall_us: 1,
        }
    }

    fn attach_projection(authority: &mut ProjectionAuthority, projection_id: &str) -> String {
        authority
            .handle_attach(
                AttachRequest {
                    envelope: test_envelope(
                        ProjectionOperation::Attach,
                        projection_id,
                        &format!("attach-{projection_id}"),
                    ),
                    provider_kind: ProviderKind::Claude,
                    display_name: format!("Test session {projection_id}"),
                    workspace_hint: None,
                    repository_hint: None,
                    icon_profile_hint: None,
                    content_classification: ContentClassification::Private,
                    hud_target: None,
                    idempotency_key: None,
                },
                "test-caller",
                10,
            )
            .owner_token
            .expect("attach must return owner token")
    }

    fn publish_output(
        authority: &mut ProjectionAuthority,
        projection_id: &str,
        owner_token: &str,
        text: &str,
        ts: u64,
    ) {
        authority.handle_publish_output(
            PublishOutputRequest {
                envelope: test_envelope(
                    ProjectionOperation::PublishOutput,
                    projection_id,
                    &format!("pub-{projection_id}-{ts}"),
                ),
                owner_token: owner_token.to_string(),
                output_text: text.to_string(),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: Some(format!("unit-{ts}")),
                coalesce_key: None,
                expects_reply: false,
            },
            "test-caller",
            ts,
        );
    }

    // ── Part (a): Adapter hosted with runtime-resolved tokens ─────────────────

    /// Constructing `PortalDriveState` and attaching an adapter yields an
    /// adapter with tokens resolved from the (empty) default token map.
    #[test]
    fn portal_drive_state_attach_creates_adapter_with_default_tokens() {
        let mut drive = PortalDriveState::new();
        drive.attach_adapter("proj-a", Vec::new());
        assert!(
            drive.adapter_mut("proj-a").is_some(),
            "adapter must be registered after attach"
        );
    }

    /// `apply_token_map` propagates new visual tokens to all live adapters.
    ///
    /// Verifies the §6.1 profile-swap contract: after `set_token_map`, the next
    /// render from any live adapter uses the new token values without adapter
    /// logic changes.
    #[test]
    fn token_map_swap_propagates_to_live_adapter() {
        use tze_hud_config::{PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR, tokens::resolve_tokens};

        let mut drive = PortalDriveState::new();
        drive.attach_adapter("proj-a", Vec::new());
        drive.attach_adapter("proj-b", Vec::new());

        // Verify baseline is the default (transcript text r ≈ 0.90)
        let baseline_r = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_text_color
            .r;
        // Default from PortalVisualTokens::default(): r = 0.90
        assert!(
            baseline_r > 0.8,
            "baseline transcript text r should be ~0.90, got {baseline_r}"
        );

        // Apply a token override: red transcript text (#FF0000 → r=1.0)
        let mut overrides = HashMap::new();
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR.to_string(),
            "#FF0000".to_string(),
        );
        drive.apply_token_map(overrides);

        // Both adapters must now carry the new token.
        for id in &["proj-a", "proj-b"] {
            let r = drive
                .adapter_mut(id)
                .unwrap()
                .visual_tokens()
                .transcript_text_color
                .r;
            assert!(
                (r - 1.0_f32).abs() < 1e-2,
                "after token swap, adapter {id} transcript text r must be ~1.0 (red), got {r}"
            );
        }
        let _ = resolve_tokens; // suppress unused-import warning in some toolchain configs
    }

    /// Adapter removed on `detach_adapter`.
    #[test]
    fn detach_removes_adapter() {
        let mut drive = PortalDriveState::new();
        drive.attach_adapter("proj-a", Vec::new());
        drive.detach_adapter("proj-a");
        assert!(
            drive.adapter_mut("proj-a").is_none(),
            "adapter must be gone after detach"
        );
    }

    // ── Part (b): Drain loop pulls updates round-robin ────────────────────────

    /// Single portal: drain returns the update after a `PublishOutput`.
    #[test]
    fn drain_emits_portal_update_for_single_portal() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        publish_output(&mut authority, "proj-a", &token_a, "hello", 20);

        let records = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);

        assert_eq!(records.len(), 1, "one drain record expected");
        assert_eq!(records[0].projection_id, "proj-a");
        assert!(records[0].visible_transcript_units > 0);
        assert!(
            !records[0].portal_markdown.is_empty(),
            "portal markdown must be non-empty"
        );
    }

    /// Two portals at equal rates: drain serves them in round-robin order.
    ///
    /// This is the cross-portal fairness invariant from tasks.md §5.1 / §5.4:
    /// under equal sustained rates, no portal is starved relative to the other.
    /// The round-robin oracle (`next_due_projection_id`) ensures bounded
    /// divergence.
    #[test]
    fn drain_round_robin_fairness_under_equal_rates() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        let token_b = attach_projection(&mut authority, "proj-b");
        drive.attach_adapter("proj-a", Vec::new());
        drive.attach_adapter("proj-b", Vec::new());

        // Publish to both portals at the same logical timestamp.
        publish_output(&mut authority, "proj-a", &token_a, "alpha-1", 20);
        publish_output(&mut authority, "proj-b", &token_b, "beta-1", 20);

        let records = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);

        // Both portals must be drained; the order is round-robin (a before b
        // since a was attached first, but both must appear).
        assert_eq!(records.len(), 2, "both portals must be drained");
        let ids: Vec<&str> = records.iter().map(|r| r.projection_id.as_str()).collect();
        assert!(
            ids.contains(&"proj-a") && ids.contains(&"proj-b"),
            "both portals must appear in drain output, got: {ids:?}"
        );

        // After drain, no further updates should be pending.
        let second_drain = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert!(
            second_drain.is_empty(),
            "second drain must be empty after all portals served"
        );
    }

    /// Three portals: round-robin serves all three in one drain pass.
    #[test]
    fn drain_three_portals_all_served() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let ids = ["proj-1", "proj-2", "proj-3"];
        let mut tokens = Vec::new();
        for id in &ids {
            tokens.push(attach_projection(&mut authority, id));
            drive.attach_adapter(id, Vec::new());
        }
        for (idx, id) in ids.iter().enumerate() {
            publish_output(&mut authority, id, &tokens[idx], "msg", 20);
        }

        let records = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        let drained_ids: Vec<&str> = records.iter().map(|r| r.projection_id.as_str()).collect();

        assert_eq!(
            records.len(),
            3,
            "all three portals must be drained, got: {drained_ids:?}"
        );
        for id in &ids {
            assert!(
                drained_ids.contains(id),
                "portal {id} missing from drain output: {drained_ids:?}"
            );
        }
    }

    /// Rate-window: if an update is rate-limited (second publish within window),
    /// the drain does not produce a record until the window passes.
    #[test]
    fn drain_respects_rate_window() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 1,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // First publish — immediately drainable.
        publish_output(&mut authority, "proj-a", &token_a, "first", 20);
        let first_drain = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert_eq!(first_drain.len(), 1);

        // Second publish within the rate window — coalesced, not yet drainable.
        publish_output(&mut authority, "proj-a", &token_a, "second", 21);
        let mid_drain = drain_and_emit_portal_updates(&mut authority, &mut drive, 21);
        assert!(
            mid_drain.is_empty(),
            "second publish within rate window must not produce a drain record"
        );

        // After the rate window passes, the update becomes drainable.
        let after_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        let late_drain = drain_and_emit_portal_updates(&mut authority, &mut drive, after_ts);
        assert_eq!(
            late_drain.len(),
            1,
            "coalesced update must drain after rate window"
        );
        assert_eq!(late_drain[0].coalesced_output_count, 1);
    }

    /// Token-map swap in portal drive state during live session: the rendered
    /// portal markdown reflects the new tokens.
    ///
    /// This is the end-to-end §6.1 proof: a `SetTokenMap` call on the drive
    /// state changes the visual tokens in the live adapter, and the next drain
    /// render uses the new tokens.
    #[test]
    fn drain_portal_markdown_reflects_token_swap() {
        use tze_hud_config::PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR;

        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // Publish and drain once with default tokens.
        publish_output(&mut authority, "proj-a", &token_a, "before swap", 20);
        let before = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert_eq!(before.len(), 1);

        // Swap tokens: change transcript text to red.
        let mut overrides = HashMap::new();
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR.to_string(),
            "#FF0000".to_string(),
        );
        drive.apply_token_map(overrides);

        // Publish again and drain with new tokens.
        publish_output(
            &mut authority,
            "proj-a",
            &token_a,
            "after swap",
            PORTAL_UPDATE_RATE_WINDOW_WALL_US + 30,
        );
        let after = drain_and_emit_portal_updates(
            &mut authority,
            &mut drive,
            PORTAL_UPDATE_RATE_WINDOW_WALL_US + 30,
        );
        assert_eq!(after.len(), 1);

        // The adapter's transcript_text_color must now be red.
        let adapter_r = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_text_color
            .r;
        assert!(
            (adapter_r - 1.0_f32).abs() < 1e-2,
            "adapter transcript_text_color.r must be ~1.0 (red) after token swap, got {adapter_r}"
        );

        // The portal markdown is rendered — just verify it is non-empty
        // (the actual color application is in the proto message layer).
        assert!(!after[0].portal_markdown.is_empty());
    }

    // ── Part (c): hud-0528i — follow-tail trigger via drain/present path ──────
    //
    // These tests prove that:
    // 1. A RenderPortal drain record carries correct append_geometry (non-None,
    //    non-zero values consistent with the transcript and token font size).
    // 2. The geometry values, when fed to InputProcessor::notify_tile_content_appended,
    //    advance an at-tail tile (spec §3.2).
    // 3. The same call does NOT disturb a scrolled-back tile (spec §3.3).
    //
    // The runtime (caller of this binary's stdout) is responsible for the
    // actual InputProcessor call; these tests simulate that call to close the
    // end-to-end loop from the drain path.

    /// A second `PublishOutput` (which triggers `RenderPortal`) must produce a
    /// drain record with `append_geometry = Some(...)` carrying positive,
    /// finite geometry values derived from the transcript line count and the
    /// adapter's font-size token.
    ///
    /// This guards the geometry plumbing: if the adapter tokens are not
    /// resolved or the transcript is empty, the values would be 0 / NaN.
    #[test]
    fn render_portal_drain_record_carries_append_geometry() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // First publish — produces CreatePortalTile (no tile registered yet).
        publish_output(&mut authority, "proj-a", &token_a, "line one", 20);
        let first = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert_eq!(first.len(), 1, "first drain must produce one record");
        // CreatePortalTile: no tile_id yet → append_geometry must be None
        // (first render, not an append into existing tile).
        assert!(
            first[0].append_geometry.is_none(),
            "CreatePortalTile must not carry append_geometry; got {:?}",
            first[0].append_geometry
        );

        // Simulate the runtime recording the created tile id (so the adapter
        // knows a tile exists and the next drain produces RenderPortal).
        let fake_tile_id = vec![0xAB, 0xCD];
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile_id);

        // Second publish after rate window — produces RenderPortal.
        let ts2 = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish_output(&mut authority, "proj-a", &token_a, "line two", ts2);
        let second = drain_and_emit_portal_updates(&mut authority, &mut drive, ts2);
        assert_eq!(second.len(), 1, "second drain must produce one record");

        // RenderPortal: append_geometry must be Some with positive finite values.
        let g = second[0]
            .append_geometry
            .expect("RenderPortal must carry append_geometry");

        assert!(
            g.line_height_px > 0.0 && g.line_height_px.is_finite(),
            "line_height_px must be positive finite; got {}",
            g.line_height_px
        );
        // Derive expected line height from the adapter's actual font-size token so
        // the test remains valid if the default PortalVisualTokens change.
        let font_size_px = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_font_size_px;
        let expected_line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        assert!(
            (g.line_height_px - expected_line_h).abs() < 0.1,
            "line_height_px must be font_size({}) * {} = {}; got {}",
            font_size_px,
            PORTAL_LINE_HEIGHT_MULTIPLIER,
            expected_line_h,
            g.line_height_px
        );

        assert!(
            g.viewport_height_px > 0.0 && g.viewport_height_px.is_finite(),
            "viewport_height_px must be positive finite; got {}",
            g.viewport_height_px
        );

        assert!(
            g.new_content_height_px > 0.0 && g.new_content_height_px.is_finite(),
            "new_content_height_px must be positive finite; got {}",
            g.new_content_height_px
        );

        // new_content_height_px = visible_units * line_height_px (≥ 1 unit)
        assert!(
            g.new_content_height_px >= g.line_height_px,
            "new_content_height_px ({}) must be >= line_height_px ({})",
            g.new_content_height_px,
            g.line_height_px
        );
    }

    /// End-to-end spec §3.2 from the drain path:
    ///
    /// After a `RenderPortal` drain record, the runtime calls
    /// `InputProcessor::notify_tile_content_appended` with the geometry from
    /// `append_geometry`. An at-tail tile MUST advance its scroll offset by
    /// whole lines.
    ///
    /// This closes the loop from the drain path all the way through the input
    /// processor (the full runtime path without a live gRPC session).
    ///
    /// # Viewport sizing strategy
    ///
    /// The drain loop derives `viewport_height_px` from `state.geometry_batch` when
    /// a geometry snapshot is present (preferred), or falls back to the adapter's
    /// configured bounds for the current presentation mode. To make the content
    /// overflow and trigger follow-tail advancement, we:
    ///   1. Push a geometry snapshot with a small viewport (1 line height) so the
    ///      drain geometry reflects a 1-line tall tile.
    ///   2. Create the scene tile with the same 1-line viewport so the scroll
    ///      state is consistent.
    ///   3. Publish 10 transcript units — `new_content_height_px` = 10 × line_h,
    ///      which overflows the 1-line viewport and triggers a whole-line advance.
    #[test]
    fn drain_append_geometry_at_tail_tile_advances_scroll_offset() {
        use tze_hud_input::InputProcessor;
        use tze_hud_projection::{AdapterGeometrySnapshot, AdapterPortalRect};
        use tze_hud_scene::{Rect, SceneGraph};

        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // Derive line height from the adapter's actual font-size token so the test
        // remains valid if the default PortalVisualTokens font size changes.
        let font_size_px = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_font_size_px;
        let line_height_px = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;

        // Push a geometry snapshot with a 1-line viewport so the drain geometry
        // uses a small enough viewport that a few transcript units overflow.
        // viewport_height_px = 1 * line_height_px; we use the nearest integer.
        let viewport_h = (1.0 * line_height_px).ceil() as i32;
        authority.push_geometry_snapshot(
            "proj-a",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        // Scene tile: match the geometry snapshot viewport height.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h as f32),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // First publish → CreatePortalTile (no tile yet).
        publish_output(&mut authority, "proj-a", &token_a, "unit-0", 20);
        let _ = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);

        // Register tile in the adapter (simulates the runtime recording the tile).
        let fake_tile: Vec<u8> = tile_id.to_bytes_le().to_vec();
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile);

        // Prime the scroll state with 1 unit at current viewport height.
        processor.notify_tile_content_appended(
            tile_id,
            1.0 * line_height_px,
            viewport_h as f32,
            line_height_px,
            &mut scene,
        );
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "tile must start at-tail"
        );

        // Publish 9 more units → 10 units total (9 units overflow 1-line viewport).
        let ts_base = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        for i in 1..=9_u64 {
            let ts = ts_base + i * 10 + 25;
            publish_output(&mut authority, "proj-a", &token_a, &format!("unit-{i}"), ts);
        }
        // Drain at the last timestamp so all 9 are coalesced.
        let ts_drain = ts_base + 9 * 10 + 25;
        let drain = drain_and_emit_portal_updates(&mut authority, &mut drive, ts_drain);
        assert_eq!(drain.len(), 1, "drain must produce one record");

        let g = drain[0]
            .append_geometry
            .expect("RenderPortal must carry append_geometry");

        // viewport_height_px must come from the geometry snapshot, not the
        // adapter config default (360px).
        assert!(
            (g.viewport_height_px - viewport_h as f32).abs() < 1.0,
            "viewport_height_px must come from geometry_batch; expected {} got {}",
            viewport_h,
            g.viewport_height_px
        );

        // Simulate the runtime calling notify_tile_content_appended with drain geometry.
        let changed = processor.notify_tile_content_appended(
            tile_id,
            g.new_content_height_px,
            g.viewport_height_px,
            g.line_height_px,
            &mut scene,
        );

        // Spec §3.2: at-tail tile must advance.
        assert!(
            changed,
            "spec §3.2: at-tail tile must advance scroll offset after append; \
             new_content_h={} viewport_h={} line_h={} changed=false",
            g.new_content_height_px, g.viewport_height_px, g.line_height_px
        );
        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_y > 0.0,
            "spec §3.2: scroll offset must be positive after at-tail advance; got {offset_y}"
        );
        // The offset is clamped to `(new_content_height - viewport_height).max(0)` (the
        // tail boundary). The tail boundary is NOT required to be a whole-line multiple
        // since viewport_h may be non-integer. Verify that the unclamped advancement was
        // a whole-line multiple: i.e. offset_y ≤ tail_offset, and either the offset is a
        // whole-line multiple OR it equals the tail offset (clamped case).
        let tail_offset = (g.new_content_height_px - g.viewport_height_px).max(0.0);
        let remainder = offset_y % g.line_height_px;
        let at_tail_boundary = (offset_y - tail_offset).abs() < 0.5;
        let whole_line = remainder < 0.5;
        assert!(
            whole_line || at_tail_boundary,
            "spec §3.2: offset must be a whole-line multiple or clamped to tail boundary; \
             offset={offset_y} tail={tail_offset} line_h={} remainder={remainder}",
            g.line_height_px
        );
        // Tile stays at-tail in the scene.
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "spec §3.2: tile must remain at-tail in scene after advance"
        );
    }

    /// Regression guard (codex P2, hud-y4pzu PR #1131 review): this drain site's
    /// `total_lines` count must include `input_history` units, not just
    /// `update.visible_transcript`. `portal_markdown_with` paints the INPUT band
    /// into the SAME single-node markdown as the OUTPUT transcript whenever the
    /// portal is Expanded and `input_history` is non-empty, so a height count
    /// that only sees `visible_transcript` undercounts `new_content_height_px`.
    ///
    /// Mirrors `drain_append_geometry_at_tail_tile_advances_scroll_offset`'s
    /// viewport-sizing strategy but drives the discrepancy directly: the OUTPUT
    /// transcript grows by exactly one line between the two drains, while ten
    /// viewer replies land in `input_history` in between via
    /// `submit_portal_input` (never `retained_transcript`). If the height count
    /// only sees the OUTPUT delta, the follow-tail clamp advances by about one
    /// line; counting the INPUT band too advances it by well over five lines.
    #[test]
    fn drain_append_geometry_includes_input_history_band() {
        use tze_hud_input::InputProcessor;
        use tze_hud_projection::{
            AdapterGeometrySnapshot, AdapterPortalRect, ContentClassification,
            PortalInputSubmission,
        };
        use tze_hud_scene::{Rect, SceneGraph};

        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        let font_size_px = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_font_size_px;
        let line_height_px = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;
        let viewport_h = (1.0 * line_height_px).ceil() as i32;

        authority.push_geometry_snapshot(
            "proj-a",
            AdapterGeometrySnapshot {
                rect: AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h as f32),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // First publish → CreatePortalTile (no tile yet).
        publish_output(&mut authority, "proj-a", &token_a, "line-0", 20);
        let _ = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);

        let fake_tile: Vec<u8> = tile_id.to_bytes_le().to_vec();
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile);

        // Prime the scroll state so the tile starts at-tail.
        processor.notify_tile_content_appended(
            tile_id,
            1.0 * line_height_px,
            viewport_h as f32,
            line_height_px,
            &mut scene,
        );
        assert!(
            scene.tile_follow_tail_at_tail(tile_id),
            "tile must start at-tail"
        );

        // Ten viewer replies land in `input_history` between the two drains.
        // `submit_portal_input` flags `portal_update_pending` but never registers
        // a coalescer entry by itself — it rides in on the next OUTPUT append
        // below, exactly as a production composer submission does.
        for i in 0..10_u64 {
            let feedback = authority.submit_portal_input(
                "proj-a",
                PortalInputSubmission {
                    input_id: format!("input-{i}"),
                    submission_text: format!("viewer reply {i}"),
                    submitted_at_wall_us: 25 + i,
                    expires_at_wall_us: None,
                    content_classification: ContentClassification::Private,
                },
            );
            assert!(feedback.error_code.is_none(), "input {i} must be accepted");
        }

        // One more short OUTPUT line makes the projection due again. The OUTPUT
        // delta alone is exactly one line.
        let ts_drain = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish_output(&mut authority, "proj-a", &token_a, "line-1", ts_drain);
        let drain = drain_and_emit_portal_updates(&mut authority, &mut drive, ts_drain);
        assert_eq!(drain.len(), 1, "drain must produce one record");

        let g = drain[0]
            .append_geometry
            .expect("RenderPortal must carry append_geometry");

        processor.notify_tile_content_appended(
            tile_id,
            g.new_content_height_px,
            g.viewport_height_px,
            g.line_height_px,
            &mut scene,
        );

        let (_, offset_y) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            offset_y > 5.0 * line_height_px,
            "follow-tail offset {offset_y} (line height {line_height_px}) must \
             reflect the ten-reply INPUT band, not just the one-line OUTPUT \
             delta — this drain site's height accounting is not counting \
             `input_history`, so long INPUT history cannot be scrolled to"
        );
    }

    /// End-to-end spec §3.3 from the drain path:
    ///
    /// After a user scroll-back, a `RenderPortal` drain record is produced and
    /// the runtime calls `InputProcessor::notify_tile_content_appended` with the
    /// geometry from `append_geometry`. A scrolled-back tile MUST NOT change its
    /// scroll offset.
    #[test]
    fn drain_append_geometry_scrolled_back_tile_is_stable() {
        use tze_hud_input::{InputProcessor, ScrollEvent};
        use tze_hud_scene::{Rect, SceneGraph};

        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // Scene setup.
        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let viewport_h = 200.0_f32;
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();
        // Derive line height from the adapter's actual font-size token so the test
        // remains valid if the default PortalVisualTokens font size changes.
        let font_size_px = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;

        // First publish → CreatePortalTile.
        publish_output(&mut authority, "proj-a", &token_a, "initial line", 20);
        let _ = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);

        // Register tile.
        let fake_tile: Vec<u8> = tile_id.to_bytes_le().to_vec();
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile);

        // Prime scroll state with enough content that the tile can scroll back.
        // 20 lines of content: max_scroll = 20 * line_h - viewport_h.
        let initial_content_h = 20.0 * line_h;
        processor.notify_tile_content_appended(
            tile_id,
            initial_content_h,
            viewport_h,
            line_h,
            &mut scene,
        );

        // Scroll to the tail, then scroll back.
        let max_scroll = initial_content_h - viewport_h;
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 300.0,
                y: 100.0,
                delta_x: 0.0,
                delta_y: max_scroll,
            },
            &mut scene,
        );
        // Scroll back 6 lines.
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 300.0,
                y: 100.0,
                delta_x: 0.0,
                delta_y: -6.0 * line_h,
            },
            &mut scene,
        );
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "tile must be ScrolledBack after user scrolled up"
        );
        let (_, offset_before) = scene.tile_scroll_offset_local(tile_id);

        // Second publish (RenderPortal) — more content appended.
        let ts2 = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish_output(
            &mut authority,
            "proj-a",
            &token_a,
            "extra line twenty-one\nextra line twenty-two",
            ts2,
        );
        let drain = drain_and_emit_portal_updates(&mut authority, &mut drive, ts2);
        assert_eq!(drain.len(), 1);

        let g = drain[0]
            .append_geometry
            .expect("RenderPortal must carry append_geometry");

        // Runtime call: simulate notify with drain geometry.
        let changed = processor.notify_tile_content_appended(
            tile_id,
            g.new_content_height_px,
            g.viewport_height_px,
            g.line_height_px,
            &mut scene,
        );

        // Spec §3.3: scrolled-back tile must NOT change offset.
        assert!(
            !changed,
            "spec §3.3: scrolled-back tile must NOT advance on append; changed=true"
        );
        let (_, offset_after) = scene.tile_scroll_offset_local(tile_id);
        assert!(
            (offset_after - offset_before).abs() < 1.0,
            "spec §3.3: offset must be stable after append on scrolled-back tile; \
             before={offset_before} after={offset_after}"
        );
        // Anchor must remain scrolled-back.
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "spec §3.3: tile must remain ScrolledBack in scene after append"
        );
    }

    // ── hud-pkg2g: head-trim keeps scrolled-back viewport stable ─────────────

    /// Spec §3.3 regression: when head content is trimmed (visible-window cap),
    /// a scrolled-back viewport must NOT jump.
    ///
    /// Byte/line layout (max_vis=250, 1-line viewport):
    ///
    ///   drain1 (CreatePortalTile): 1 unit × 50B  → 50B visible,  1 line
    ///   drain2 (RenderPortal):     5 units × 50B  → 250B visible, 5 lines
    ///     max_scroll = (5-1)*line_h = 4*line_h
    ///   user scrolls back 2 lines → offset = 4*line_h - 2*line_h = 2*line_h (> 0) ✓
    ///   drain3 (RenderPortal):     1 unit × 55B added
    ///     visible window (newest→oldest): 55 + 50 + 50 + 50 = 205 ≤ 250, then +50=255>250.
    ///     visible_bytes=205 (4 units), content_height=4*line_h.
    ///     205 < 250 (drain2) ✓  AND  4*line_h < 5*line_h (drain2) ✓  → head_trim fires.
    ///     removed_height_px = 5*line_h - 4*line_h = 1*line_h
    ///     compensated offset = max(2*line_h - 1*line_h, 0) = 1*line_h  (> 0 → meaningful) ✓
    ///
    /// Without this wiring (pre-hud-pkg2g), notify_head_content_removed is not called
    /// and ScrollTileState retains stale content-height, corrupting the follow-tail
    /// recomputation and potentially jumping the viewport.
    #[test]
    fn head_trim_scrolled_back_viewport_stays_stable_via_drain_record() {
        use tze_hud_input::{InputProcessor, ScrollEvent};
        use tze_hud_scene::{Rect, SceneGraph};

        // max_vis=250: five 50B units fill the window exactly.
        // A subsequent 55B unit displaces the oldest 50B unit
        // (55+50+50+50+50=255 > 250), shrinking visible_bytes from 250 to 205 and
        // content from 5 to 4 lines.  This guarantees head_trim_geometry = Some.
        let max_vis: usize = 250;
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            max_visible_transcript_bytes: max_vis,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        let font_size_px = drive
            .adapter_mut("proj-a")
            .unwrap()
            .visual_tokens()
            .transcript_font_size_px;
        let line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER;

        // 1-line viewport: with 5 lines of content, max_scroll = 4*line_h.
        // The user scrolls back 2 lines → offset = 2*line_h (well within bounds).
        let viewport_h = line_h.ceil() as i32;
        authority.push_geometry_snapshot(
            "proj-a",
            tze_hud_projection::AdapterGeometrySnapshot {
                rect: tze_hud_projection::AdapterPortalRect {
                    x_px: 0,
                    y_px: 0,
                    width_px: 600,
                    height_px: viewport_h,
                },
                gesture_active: false,
                sequence: 1,
            },
        );

        let mut scene = SceneGraph::new(1920.0, 1080.0);
        let tab_id = scene.create_tab("Main", 0).unwrap();
        let lease_id = scene.grant_lease("portal-agent", 60_000);
        let tile_id = scene
            .create_tile(
                tab_id,
                "portal-agent",
                lease_id,
                Rect::new(0.0, 0.0, 600.0, viewport_h as f32),
                1,
            )
            .unwrap();
        scene
            .register_tile_scroll_config(
                tile_id,
                tze_hud_scene::TileScrollConfig {
                    scrollable_x: false,
                    scrollable_y: true,
                    content_width: None,
                    content_height: None,
                },
            )
            .unwrap();

        let mut processor = InputProcessor::new();

        // ── Phase 1: first drain (CreatePortalTile) ───────────────────────────
        // Each 50B unit fits within max_vis individually; five of them together
        // exactly fill the visible window (5×50=250=max_vis).
        let unit_50 = "A".repeat(50);
        // 55B unit: when added as the 6th unit, only 4 predecessors (4×50=200)
        // still fit (55+50×4=255>250 forces the oldest out, 55+50×3=205≤250).
        let unit_55 = "B".repeat(55);

        publish_output(&mut authority, "proj-a", &token_a, &unit_50, 20);
        let first_drain = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert_eq!(first_drain.len(), 1, "must produce one drain record");
        let fake_tile: Vec<u8> = tile_id.to_bytes_le().to_vec();
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile);

        // Prime processor: 1 unit, 1 line.
        processor.notify_tile_content_appended(
            tile_id,
            line_h,
            viewport_h as f32,
            line_h,
            &mut scene,
        );

        // ── Phase 2: second drain — five 50B units visible (250B = max_vis) ───
        // Publish 4 more 50B units past the rate window, then drain.
        let base_ts = PORTAL_UPDATE_RATE_WINDOW_WALL_US;
        for i in 0_u64..4 {
            publish_output(
                &mut authority,
                "proj-a",
                &token_a,
                &unit_50,
                base_ts + i * 5 + 1,
            );
        }
        let ts_drain2 = base_ts + 30;
        let drain2 = drain_and_emit_portal_updates(&mut authority, &mut drive, ts_drain2);
        assert_eq!(drain2.len(), 1, "second drain must produce one record");

        // Verify the geometry that the stability proof depends on.
        assert_eq!(
            drain2[0].visible_transcript_bytes, 250,
            "drain2 must show 250 visible bytes (5 × 50B)"
        );
        let ag2 = drain2[0]
            .append_geometry
            .expect("drain2 RenderPortal must carry append_geometry");
        assert!(
            (ag2.new_content_height_px - 5.0 * line_h).abs() < f32::EPSILON,
            "drain2 must show 5 lines; got {}",
            ag2.new_content_height_px
        );

        // Simulate runtime: 5 lines visible, viewport 1 line → max_scroll = 4*line_h.
        processor.notify_tile_content_appended(
            tile_id,
            ag2.new_content_height_px,
            ag2.viewport_height_px,
            ag2.line_height_px,
            &mut scene,
        );

        // ── Phase 3: scroll back 2 lines ─────────────────────────────────────
        // max_scroll = 4*line_h.  Forward scroll to AtTail then back 2 lines.
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 300.0,
                y: (viewport_h as f32) / 2.0,
                delta_x: 0.0,
                delta_y: line_h * 20.0, // large forward → AtTail at 4*line_h
            },
            &mut scene,
        );
        let _ = processor.process_scroll_event(
            &ScrollEvent {
                x: 300.0,
                y: (viewport_h as f32) / 2.0,
                delta_x: 0.0,
                delta_y: -2.0 * line_h, // back 2 lines → offset = 2*line_h
            },
            &mut scene,
        );
        assert!(
            !scene.tile_follow_tail_at_tail(tile_id),
            "tile must be ScrolledBack before the head-trim drain"
        );
        let (_, offset_before_trim) = scene.tile_scroll_offset_local(tile_id);
        // Scroll model: offset_y = distance scrolled from content origin toward tail.
        // AtTail → offset = max_scroll = 4*line_h.  Back 2 lines → 2*line_h.
        assert!(
            offset_before_trim > 0.0,
            "offset_before_trim must be > 0 (expected ~2*line_h); got {offset_before_trim}",
        );

        // ── Phase 4: head-trim drain — 55B unit evicts oldest 50B unit ────────
        // visible window from tail: 55B + 50B + 50B + 50B = 205 ≤ 250 → 4 units.
        // Adding the 5th (50B): 205+50=255 > 250 → stops.
        // visible_bytes=205 (down from 250), content_height=4 lines (down from 5).
        let ts_drain3 = base_ts + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 5;
        publish_output(&mut authority, "proj-a", &token_a, &unit_55, ts_drain3);
        let drain3 = drain_and_emit_portal_updates(&mut authority, &mut drive, ts_drain3);
        assert_eq!(drain3.len(), 1, "third drain must produce one record");

        // ── Phase 5: head_trim_geometry must be Some (unconditional) ─────────
        // visible_bytes dropped 250 → 205 AND content_height dropped 5 → 4 lines.
        // Both conditions fire the detection; if this changes, the test setup is broken.
        let htg = drain3[0].head_trim_geometry.expect(
            "head_trim_geometry must be Some: visible_bytes dropped 250→205 \
             (55B unit evicted oldest 50B unit from visible window)",
        );
        assert!(
            htg.removed_height_px > 0.0,
            "removed_height_px must be positive; got {}",
            htg.removed_height_px,
        );
        // We expect exactly 1 line removed (5 lines → 4 lines).  Allow for f32
        // rounding on line_h = font_size_px * PORTAL_LINE_HEIGHT_MULTIPLIER.
        assert!(
            (htg.removed_height_px - line_h).abs() < 1.0,
            "removed_height_px must be approximately 1*line_h={line_h}; got {}",
            htg.removed_height_px,
        );

        // ── Phase 6: notify_head_content_removed BEFORE notify_tile_content_appended
        // hud-pkg2g contract: adjust offset BEFORE appended so ScrollTileState
        // content-height is correct for the follow-tail recomputation.
        let trim_changed = processor.notify_head_content_removed(tile_id, htg.removed_height_px);
        assert!(
            trim_changed,
            "notify_head_content_removed must return true (tile is ScrolledBack)"
        );

        // ── Phase 7: verify scroll offset was compensated, not jumped ─────────
        // notify_head_content_removed adjusts the internal scroll offset but does NOT
        // propagate it to the scene graph by design (the method has no scene parameter).
        // commit_scroll_updates flushes the dirty state to the scene so we can assert.
        let _ = processor.commit_scroll_updates(&mut scene);

        // Before: ~2*line_h.  Removed: ~1*line_h.  Expected: ~1*line_h.
        let (_, offset_after_trim) = scene.tile_scroll_offset_local(tile_id);
        let expected_compensated = (offset_before_trim - htg.removed_height_px).max(0.0);
        // Allow 1.0 px tolerance for f32 accumulation in line-height arithmetic.
        assert!(
            (offset_after_trim - expected_compensated).abs() < 1.0,
            "scroll offset must equal offset_before - removed after head-trim; \
             before={offset_before_trim} removed={} expected={expected_compensated} got={offset_after_trim}",
            htg.removed_height_px
        );

        // Apply append geometry — tile is ScrolledBack so it must NOT advance.
        let ag3 = drain3[0]
            .append_geometry
            .expect("drain3 RenderPortal must carry append_geometry");
        let advanced = processor.notify_tile_content_appended(
            tile_id,
            ag3.new_content_height_px,
            ag3.viewport_height_px,
            ag3.line_height_px,
            &mut scene,
        );
        assert!(
            !advanced,
            "spec §3.3: scrolled-back tile must NOT advance after append+head-trim; \
             advanced=true means viewport jumped to tail"
        );
    }

    // ── hud-hkaw2: drain Err-branch clears coalescer entry ───────────────────

    /// Regression test for hud-hkaw2 bug (a): the `Err` branch of
    /// `drain_and_emit_portal_updates` must call `expire_projection` to remove
    /// the orphan coalescer entry.
    ///
    /// Without the fix, `next_due_projection_id` keeps returning the stale key,
    /// `take_due_portal_update` keeps returning `Err(ProjectionNotFound)`, and
    /// the drain loop spins forever.
    ///
    /// The test injects an orphan coalescer entry (coalescer entry without a
    /// corresponding session) via the test-only helper, then calls
    /// `drain_and_emit_portal_updates` and asserts it terminates and returns an
    /// empty record set (the orphan key produced no usable update).
    #[test]
    fn drain_terminates_on_orphan_coalescer_entry() {
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        // Inject a coalescer entry for a projection that has no session.
        // This simulates the state that would arise if a session was removed
        // while a coalescer entry was still pending (race condition or bug).
        authority.inject_orphan_coalescer_entry_for_test("orphan-portal");

        // drain_and_emit_portal_updates MUST terminate (not loop forever).
        // If the Err branch does not call expire_projection, this call hangs.
        let records = drain_and_emit_portal_updates(&mut authority, &mut drive, 100);

        // The orphan key had no session, so no usable record was produced.
        assert!(
            records.is_empty(),
            "drain on orphan coalescer entry must produce no records; got {records:?}"
        );

        // Confirm the coalescer entry was cleared: a second drain must also
        // terminate immediately and produce no records.
        let second = drain_and_emit_portal_updates(&mut authority, &mut drive, 200);
        assert!(
            second.is_empty(),
            "second drain after orphan cleanup must also be empty; got {second:?}"
        );
    }

    // ── hud-hkaw2: height-only head-trim detection ────────────────────────────

    /// Regression test for hud-hkaw2 bug (b): `head_trim_geometry` must be
    /// emitted when `new_content_height_px` shrinks between consecutive drains,
    /// even when `visible_transcript_bytes` does NOT shrink (or grows).
    ///
    /// The old dual-condition (`bytes_shrank && height_shrank`) would miss this
    /// case. The fixed single condition (`height_shrank`) fires correctly.
    ///
    /// Byte/line layout (max_vis=30B):
    ///
    ///   drain1 (CreatePortalTile): publish seed unit "S" (1B, 1 line).
    ///   [register tile]
    ///   drain2 (RenderPortal): publish many-lines unit "\n" × 24 = 24B, 25 lines.
    ///     Visible window: 1B + 24B = 25B ≤ 30B → both units visible.
    ///     visible_bytes=25, lines: max(1,1) + max(1,25) = 26 lines.
    ///     prev_content_height_px stored = 26 * line_h.
    ///   drain3 (RenderPortal): publish flat unit "B" × 30 = 30B, 1 line.
    ///     Visible window from tail: 30B → cumulative=30 ≤ 30B ✓.
    ///     Next unit (24B) → cumulative=54 > 30 → EVICTED.
    ///     Also seed (1B) → would also be evicted.
    ///     Visible: [flat(30B)] → visible_bytes=30 (GREW from 25), lines=1.
    ///     new_content_height_px = 1 * line_h < prev (26 * line_h) → head_trim fires!
    ///     Without the height-only fix (old bytes_shrank && height_shrank):
    ///       bytes_shrank = (30 < 25) = false → head_trim would NOT fire (bug).
    ///     With the height-only fix (height_shrank):
    ///       height_shrank = (1*line_h < 26*line_h) = true → head_trim fires.
    #[test]
    fn head_trim_geometry_emitted_when_height_shrinks_without_byte_shrink() {
        // max_vis = 30B: seed(1B) + many-lines(24B) = 25B ≤ 30B fit together;
        // flat(30B) alone = 30B ≤ 30B fits, but flat + many-lines = 54B > 30B evicts many-lines.
        let max_vis: usize = 30;
        let mut authority = ProjectionAuthority::new(ProjectionBounds {
            max_portal_updates_per_second: 100,
            max_visible_transcript_bytes: max_vis,
            ..ProjectionBounds::default()
        })
        .unwrap();
        let mut drive = PortalDriveState::new();

        let token_a = attach_projection(&mut authority, "proj-a");
        drive.attach_adapter("proj-a", Vec::new());

        // drain1: CreatePortalTile. Seed unit: 1B, 1 line.
        publish_output(&mut authority, "proj-a", &token_a, "S", 20);
        let drain1 = drain_and_emit_portal_updates(&mut authority, &mut drive, 20);
        assert_eq!(drain1.len(), 1, "drain1 must produce one record");
        // CreatePortalTile (no tile yet): append_geometry must be None.
        assert!(
            drain1[0].append_geometry.is_none(),
            "drain1 (CreatePortalTile) must not carry append_geometry"
        );

        // Register the tile so subsequent drains produce RenderPortal.
        let fake_tile_id = vec![0xAA, 0xBB];
        drive
            .adapter_mut("proj-a")
            .unwrap()
            .record_created_tile(fake_tile_id);

        // drain2: many-lines unit: "\n" × 24 = 24B, 25 lines.
        // Visible window after: seed(1B) + many-lines(24B) = 25B ≤ 30B → both visible.
        // The drain adapter counts lines: seed("S") = 1 line, many-lines(24 newlines) = 25 lines.
        // total_lines = 1 + 25 = 26; prev_content_height_px = 26 * line_h is stored.
        let many_lines = "\n".repeat(24);
        assert_eq!(
            many_lines.len(),
            24,
            "many_lines must be exactly 24 bytes; got {}",
            many_lines.len()
        );
        let ts2 = PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish_output(&mut authority, "proj-a", &token_a, &many_lines, ts2);
        let drain2 = drain_and_emit_portal_updates(&mut authority, &mut drive, ts2);
        assert_eq!(drain2.len(), 1, "drain2 must produce one record");
        let bytes_drain2 = drain2[0].visible_transcript_bytes;
        assert_eq!(bytes_drain2, 25, "drain2 must show 25 visible bytes");
        // No head_trim on drain2 (content grew, not shrunk).
        assert!(
            drain2[0].head_trim_geometry.is_none(),
            "drain2 must not carry head_trim_geometry (content grew)"
        );

        // drain3: flat unit "B" × 30 = 30B, 1 line.
        // Visible window from tail: flat(30B) ≤ 30B → fits; many-lines(24B) → 54B > 30 → evicted.
        // Visible: [flat(30B)], visible_bytes=30 (GREW from 25), lines=1.
        // new_content_height_px = 1 * line_h < prev_content_height_px (26 * line_h) → head_trim fires.
        let flat_unit = "B".repeat(30);
        assert_eq!(flat_unit.len(), 30, "flat_unit must be exactly 30 bytes");
        let ts3 = ts2 + PORTAL_UPDATE_RATE_WINDOW_WALL_US + 25;
        publish_output(&mut authority, "proj-a", &token_a, &flat_unit, ts3);
        let drain3 = drain_and_emit_portal_updates(&mut authority, &mut drive, ts3);
        assert_eq!(drain3.len(), 1, "drain3 must produce one record");

        let bytes_drain3 = drain3[0].visible_transcript_bytes;
        // Bytes must NOT have shrunk (they grew from 25 to 30): this is the
        // key invariant that the old dual-condition would have used to suppress
        // head_trim_geometry (bytes_shrank = false → old code returned None).
        assert!(
            bytes_drain3 >= bytes_drain2,
            "visible_transcript_bytes must not shrink in drain3 (bytes grew: {bytes_drain2}→{bytes_drain3}); \
             this is the invariant that exposes the old dual-condition bug"
        );

        // KEY ASSERTION (hud-hkaw2 fix): head_trim_geometry must be Some because
        // content height shrank (26 lines → 1 line) even though byte count grew.
        // Old dual-condition: (bytes_shrank=false) && (height_shrank=true) = false → None (BUG).
        // Fixed height-only condition: (height_shrank=true) → Some (CORRECT).
        let htg = drain3[0].head_trim_geometry.expect(
            "head_trim_geometry must be Some: content height shrank (many-lines unit evicted \
             by flat unit) even though visible_transcript_bytes grew — height-only condition \
             (hud-hkaw2 fix) must fire",
        );
        assert!(
            htg.removed_height_px > 0.0,
            "removed_height_px must be positive; got {}",
            htg.removed_height_px
        );
    }
}
