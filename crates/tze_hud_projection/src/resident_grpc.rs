//! Resident gRPC adapter for cooperative projection portal materialization.
//!
//! This module is daemon-side glue: it turns bounded projection authority state
//! into `HudSession` messages for the native text-stream portal path. Expanded
//! portals publish server-owned two-pane chrome; collapsed portals retain the
//! compact text card.
//! It deliberately does not expose an LLM-facing CLI, MCP surface, provider RPC,
//! PTY, terminal byte stream, or process lifecycle authority.

use std::time::Instant;

use tze_hud_config::TimestampGranularity;
use tze_hud_protocol::proto;
use tze_hud_protocol::proto::session as session_proto;

use thiserror::Error;

use crate::{
    AdapterDraftBatch, AdapterDraftNotification, ContentClassification, InputDeliveryState,
    OutputKind, PortalInputFeedback, PortalInputFeedbackState, PortalInputSubmission,
    ProjectedPortalPresentation, ProjectedPortalState, ProjectionAuthority, ProjectionErrorCode,
    ProjectionLifecycleState, TranscriptUnit,
};

/// Content-free disconnect/stale marker line rendered when the portal's driving
/// stream/session is degraded (portal-disconnect-resume-ux §2). Carries no
/// identity or transcript content, so it remains present under redaction exactly
/// like the scroll-position indicator.
const PORTAL_DISCONNECT_MARKER_LINE: &str = "⊘ disconnected — stream stale";

/// First-run empty-portal body (hud-g1ena.6, portal-chat-grade-affordances
/// §First-Run Empty Portal Treatment): quiet, inviting copy shown when a
/// *connected* portal's retained transcript window is empty, replacing the
/// literal `<empty projection stream>`. Ambient by design — a presence engine's
/// empty surface reads as calm and ready, never as an error. The portal header
/// carries the identity; this line is the "inviting copy" that redaction
/// suppresses.
const PORTAL_EMPTY_READY_LINE: &str = "Ready — waiting for the first message.";

/// Content-free empty-portal placeholder for a viewer without identity clearance
/// (§First-Run Empty Portal Treatment redaction scenario): no identity, no
/// inviting copy — only a neutral, quiet mark, mirroring how the other
/// affordances go silent under redaction.
const PORTAL_EMPTY_REDACTED_LINE: &str = "· · ·";

/// Connecting-state body shown while a portal is attached but its owning session
/// has never connected (portal-chat-grade-affordances §Connecting State
/// Distinction, `has_ever_connected == false`). A starting-up portal must not
/// read as a "ready" empty state, so the first-run treatment yields to this.
///
/// Distinct from the degraded/disconnect marker on TWO axes so a starting-up
/// portal never reads as a failing one: (1) a distinct pending glyph — `◌`
/// (dotted, "not yet solid") vs the degraded `⊘` (circled slash) — that survives
/// environments which do not inspect `color_runs`, and (2) the token-resolved
/// cool `connecting_marker_color` (vs the amber `stale_marker_color`), applied
/// via `connecting_color_runs`. Content-free: it names no identity and reveals no
/// transcript, so — like the disconnect marker — it is redaction-independent and
/// takes precedence over the redacted empty placeholder.
///
/// hud-g1ena.6 established the precedence with a minimal `"Connecting…"` string;
/// hud-g1ena.7 replaces it with this distinct connecting treatment.
const PORTAL_CONNECTING_LINE: &str = "◌ Connecting… — waiting for the session to come online.";

/// In-transcript unread divider marker (hud-g1ena.2,
/// portal-chat-grade-affordances §Unread Divider and Ambient Unread Count).
///
/// A quiet boundary label rendered before the oldest retained unseen
/// agent-authored turn when the viewer returns to a portal with unread content.
/// Ambient, not alarming (no `!`/⚠) — a presence engine marks *where* unread
/// begins without behaving like a notification; the *how many* is carried
/// separately by the ambient unread count near the header. The token-resolved
/// color rides a zero-length sentinel (`unread_divider_color_runs`); precise
/// per-line coloring is deferred to the promotion-era structured layout (like
/// the other Phase-1 single-node markers).
const PORTAL_UNREAD_DIVIDER_LINE: &str = "─── unread ───";

/// Header label for the INPUT-history band (§Viewer Reply Echo, two-pane
/// INPUT/OUTPUT split). Marks the viewer's own accepted replies (the viewer-echo
/// stack) as a region distinct from the OUTPUT/agent transcript above it, so the
/// two panes never read as one combined stream. Quiet + ambient (no `!`/⚠); the
/// band renders only when `input_history` is non-empty, and `input_history` is
/// emptied upstream under redaction, so the label never leaks for a restricted
/// viewer.
const PORTAL_INPUT_HISTORY_LABEL: &str = "─── your replies ───";

/// Ambient header activity marker (portal-chat-grade-affordances §Agent Activity
/// and Streaming Cue, hud-g1ena.5) shown while the owning adapter is actively
/// appending to the transcript. A compact typing-style indicator — quiet ellipsis
/// glyph, no `!`/⚠ — so continuous streaming reads as ambient presence, never a
/// notification (doctrine: not a notification engine). The token-resolved
/// `activity_cue_color` accent rides alongside via `activity_cue_color_runs`.
const PORTAL_ACTIVITY_MARKER_LINE: &str = "⋯ writing";

/// Transcript-tail streaming cursor glyph appended to the latest agent turn while
/// content is actively appending. A slim left half-block reads as a live-writing
/// caret at the tail. The token-resolved `streaming_cursor_color` accent rides
/// alongside via `streaming_cursor_color_runs` as a content-end tail marker; the
/// compositor recolors THIS glyph precisely at its layout-measured tail
/// (`markdown_node_tail_cursor_color` / `apply_tail_streaming_cursor`, hud-zlq2v).
/// The `▍` here and the compositor's `STREAMING_CURSOR_GLYPH` char must stay in
/// sync. Ambient, not alarming.
const PORTAL_STREAMING_CURSOR_GLYPH: &str = " ▍";

/// Map the projected portal's redaction-gated lifecycle to the first-class
/// [`proto::PortalLifecycleStateProto`] i32 (hud-rpm9s).
///
/// A redacted lifecycle (`lifecycle_state == None`) maps to UNSPECIFIED so an
/// `UpdatePortalSurfaceState` patch leaves the stored lifecycle unchanged
/// (coalescing-safe, redaction-independent) — exactly as the lifecycle accent
/// goes silent under redaction rather than asserting a value. The mapping is
/// total; refinement toward `WaitingForInput`/`Blocked` is left to the authority
/// that owns richer session semantics.
fn portal_lifecycle_state_proto(state: &ProjectedPortalState) -> i32 {
    use ProjectionLifecycleState as P;
    let e = match state.lifecycle_state {
        None => proto::PortalLifecycleStateProto::PortalLifecycleStateUnspecified,
        Some(P::Active) | Some(P::Attached) => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateActive
        }
        Some(P::Degraded) | Some(P::HudUnavailable) => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateDegraded
        }
        Some(P::Detached) | Some(P::CleanupPending) | Some(P::Expired) => {
            proto::PortalLifecycleStateProto::PortalLifecycleStateDetached
        }
    };
    e as i32
}

/// Map the projected portal's presentation to the first-class
/// [`proto::PortalDisplayStateProto`] i32 (hud-rpm9s). Total — presentation is
/// never redacted, so this always asserts a concrete Expanded/Collapsed value.
fn portal_display_state_proto(state: &ProjectedPortalState) -> i32 {
    let e = match state.presentation {
        ProjectedPortalPresentation::Expanded => {
            proto::PortalDisplayStateProto::PortalDisplayStateExpanded
        }
        ProjectedPortalPresentation::Collapsed => {
            proto::PortalDisplayStateProto::PortalDisplayStateCollapsed
        }
    };
    e as i32
}

/// Ambient viewer-turn delivery-acknowledgement cue lines (hud-g1ena.1,
/// portal-chat-grade-affordances §Viewer Turn Delivery Acknowledgement). One quiet
/// line per presentation class, shown for the viewer's most recent echoed reply so
/// the viewer can see whether it reached the owning adapter WITHOUT asking. Kept
/// text-visible (so the signal survives environments that do not inspect
/// `color_runs`) and deliberately quiet — no `!`/⚠: a delivery transition is not an
/// attention event, and a failed cue stays ambient on the turn rather than
/// escalating interruption class. The token-resolved class color rides alongside
/// each via `delivery_cue_color_runs`.
///
/// - In-flight (Pending/Deferred): submitted, adapter has not yet taken delivery.
const PORTAL_DELIVERY_INFLIGHT_LINE: &str = "→ sending";
/// - Delivered (Delivered/Handled): adapter has taken (or handled) the reply. The
///   double tick reads as "reached" and is distinct from the composer's single
///   `✓ sent` local-accept feedback.
const PORTAL_DELIVERY_DELIVERED_LINE: &str = "✓✓ delivered";
/// - Failed (Rejected/Expired): rejected by the adapter or expired before
///   delivery. A plain cross — NOT the `⚠` used for composer rejection — so it
///   reads as "did not arrive" quietly, ambient on the turn.
const PORTAL_DELIVERY_FAILED_LINE: &str = "✕ not delivered";

/// Separator between an ambient per-turn arrival timestamp and its turn text
/// (portal-chat-grade-affordances §Ambient Per-Turn Timestamps, hud-g1ena.4). A
/// middot with hair spacing keeps the stamp compact and visually subordinate to
/// the turn content it prefixes.
const PORTAL_TIMESTAMP_SEPARATOR: &str = " · ";

/// One minute in microseconds — the arrival-minute bucket used by
/// [`TimestampGranularity::Grouped`](tze_hud_config::TimestampGranularity::Grouped)
/// and the `HH:MM` presentation precision.
const WALL_CLOCK_MINUTE_US: u64 = 60_000_000;

/// Format a runtime-assigned wall-clock arrival time as an ambient `HH:MM`
/// time-of-day (UTC).
///
/// `appended_at_wall_us` is microseconds since the Unix epoch in the WALL-CLOCK
/// domain — the same unforgeable, runtime-assigned arrival metadata the activity
/// cue reads (`TranscriptUnit::appended_at_wall_us`). CLOCK-DOMAIN typing is
/// preserved: this is presentation of *arrival* time and is deliberately NOT the
/// media/display clock (`present_at`/sync-group timing); the two are never
/// conflated. Rendered at minute precision (seconds dropped) to stay quiet and
/// ambient. UTC is used because the runtime carries no viewer timezone at this
/// layer; a local-offset presentation is a promotion-era profile concern.
fn format_wall_clock_arrival_hhmm(appended_at_wall_us: u64) -> String {
    let seconds_of_day = (appended_at_wall_us / 1_000_000) % 86_400;
    let hours = seconds_of_day / 3_600;
    let minutes = (seconds_of_day % 3_600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// Quiescence window (µs) for the agent-activity / streaming-cursor cue.
///
/// The cue derives from the tail transcript unit's runtime-assigned
/// `appended_at_wall_us` (unforgeable — adapters cannot set arrival times)
/// compared to the render's wall-clock now: the agent is "actively appending"
/// only while the newest append is within this window. This is what makes the
/// cue quiesce promptly once appends stop, and — critically for the single-node
/// Phase-1 model — it means a portal re-rendered for an UNRELATED reason (resize,
/// draft keystroke, collapse) long after the last append shows no cursor, because
/// `now - appended_at` exceeds the window. Kept short so streaming reads as live
/// yet the cue settles quickly when the agent goes quiet (ambient, not sticky).
///
/// Phase-1 limitation: the drive loop re-renders a portal only on its own append
/// traffic (there is no autonomous heartbeat tick), so a truly idle portal holds
/// the last-rendered cursor until the next operation touches it. Any subsequent
/// render past the window quiesces it. Promotion-era work adds precise tail
/// positioning and a heartbeat repaint.
const PORTAL_ACTIVITY_QUIESCE_WINDOW_US: u64 = 2_000_000;

/// Whether the portal is in a connection-degraded presentation state.
///
/// Keyed off the redaction-independent `connection_degraded` flag (set by the
/// authority from the session lifecycle), NOT the redaction-gated
/// `lifecycle_state`, so the degraded treatment is applied even for a restricted
/// viewer.
fn is_connection_degraded(state: &ProjectedPortalState) -> bool {
    state.connection_degraded
}

/// Whether the owning adapter is actively appending to the transcript right now
/// (portal-chat-grade-affordances §Agent Activity and Streaming Cue, hud-g1ena.5).
///
/// DERIVED entirely from observed append activity — the newest visible transcript
/// unit's runtime-assigned `appended_at_wall_us` compared to the render's
/// wall-clock `now_wall_us` — never from a separate adapter "typing" protocol or
/// the adapter-declared `lifecycle_state` (which is forgeable; appended-at is
/// not). The cue fires only while ALL hold:
///
/// - **Expanded** presentation — the cursor is a transcript-tail affordance and
///   the header cue rides the expanded header; collapsed keeps its compact card.
/// - **Live** — not `connection_degraded` and `has_ever_connected`; a degraded or
///   never-connected portal must never imply an active stream (§clear live
///   signals on disconnect; connecting takes precedence over activity).
/// - **Tail is a fresh agent turn** — the last unit is agent output (`!= Viewer`)
///   and NOT `expects_reply` (a question awaiting a reply is quiescing, not
///   writing, and already carries its own awaiting-reply cue), appended within
///   [`PORTAL_ACTIVITY_QUIESCE_WINDOW_US`] of now.
///
/// Redaction falls out for free: a restricted viewer's `visible_transcript` is
/// emptied upstream, so `last()` is `None` and the cue is suppressed along with
/// transcript previews (§activity cue suppressed under redaction). Because the
/// gate is a freshness window, continuous streaming keeps re-satisfying it
/// without any attention re-escalation, and it quiesces promptly once appends
/// stop (§activity cue is not a notification).
fn agent_activity_active(state: &ProjectedPortalState, now_wall_us: u64) -> bool {
    // The cue is active exactly while `now` has not passed the newest agent
    // tail's clear-due deadline. Factored through
    // `agent_activity_clear_deadline_us` so the drive loop can schedule a
    // one-shot quiesce repaint off the same predicate (hud-kbm80).
    agent_activity_clear_deadline_us(state).is_some_and(|deadline| now_wall_us <= deadline)
}

/// The wall-clock instant (µs since epoch) past which `state`'s agent-activity /
/// streaming-cursor cue quiesces, or `None` when the state carries no cue at all
/// (wrong presentation, degraded / never-connected, or a tail that is not a
/// fresh non-question agent turn).
///
/// This is the time-independent factor of [`agent_activity_active`]: it applies
/// every structural gate documented there (Expanded, Live, tail-is-a-fresh-
/// non-question-agent-turn) and returns that tail's deadline
/// (`appended_at_wall_us + PORTAL_ACTIVITY_QUIESCE_WINDOW_US`) WITHOUT comparing
/// it to a render `now`. `agent_activity_active(state, now)` is exactly
/// `deadline.is_some_and(|d| now <= d)`.
///
/// Exposed for the drive loop (hud-kbm80): the round-robin drain only re-renders
/// a portal on a fresh coalescer update, so after the terminal append on an
/// otherwise-idle portal nothing re-evaluates the cue and it would persist past
/// its window — misrepresenting ongoing activity (§Agent Activity and Streaming
/// Cue: the cue SHALL "quiesce promptly once appends stop"). The driver reads
/// this deadline to schedule a single force-repaint past it, at which point the
/// derivation above evaluates false and the cue clears.
pub fn agent_activity_clear_deadline_us(state: &ProjectedPortalState) -> Option<u64> {
    if state.presentation != ProjectedPortalPresentation::Expanded
        || is_connection_degraded(state)
        || !state.has_ever_connected
    {
        return None;
    }
    state.visible_transcript.last().and_then(|unit| {
        (unit.output_kind != OutputKind::Viewer && !unit.expects_reply).then(|| {
            unit.appended_at_wall_us
                .saturating_add(PORTAL_ACTIVITY_QUIESCE_WINDOW_US)
        })
    })
}

/// Client-side materialization budget for one resident portal update.
///
/// The adapter must be comfortably below one 60 Hz frame budget while building
/// the outbound gRPC payload. Server-side admission and compositor budgets are
/// measured by their existing validation lanes.
pub const RESIDENT_PORTAL_UPDATE_BUILD_BUDGET_US: u64 = 16_600;

/// Local-first budget for translating HUD composer text into the semantic inbox.
pub const RESIDENT_PORTAL_INPUT_FEEDBACK_BUDGET_US: u64 = 4_000;

const DEFAULT_EXPANDED_W: f32 = 720.0;
const DEFAULT_EXPANDED_H: f32 = 360.0;
const DEFAULT_COMPACT_W: f32 = 420.0;
const DEFAULT_COMPACT_H: f32 = 96.0;
const DEFAULT_Z_ORDER: u32 = 160;
const MAX_PORTAL_MARKDOWN_BYTES: usize = 16_384;
/// Server-owned two-pane frame radius. This is layout chrome, not model input;
/// it mirrors the reviewed exemplar geometry profile without exposing geometry
/// through projection text or provider-authored tokens.
const EXPANDED_PORTAL_CORNER_RADIUS_PX: f32 = 14.0;

// ── PortalVisualTokens ────────────────────────────────────────────────────────

/// Resolved visual values for the portal surface parts, sourced from the
/// runtime's resolved design token set.
///
/// **Token rule (§6.1):** the resident adapter MUST source every
/// published visual value from this struct. No literal colors or font sizes
/// are permitted in the adapter publish path. A profile/token change must
/// reskin the portal end-to-end with zero adapter logic changes.
///
/// Build by first calling `tze_hud_config::resolve_portal_tokens` on the
/// runtime's resolved `DesignTokenMap` to get `PortalPartTokens`, then
/// converting with
/// `tze_hud_runtime::portal_tokens::portal_visual_tokens_from_part_tokens`.
/// Pass the result to `ResidentGrpcPortalAdapter::with_tokens`.
///
/// Expanded projections materialize a bounded absolute subtree for the frame,
/// header, INPUT pane, OUTPUT pane, and divider. Collapsed projections remain a
/// single text node. Geometry is resolved server-side from the portal bounds and
/// spacing tokens; projection text never supplies layout coordinates.
///
/// ## Redaction safety (§6.3)
///
/// Redaction is **structural**, not time-based: the `redacted` flag on
/// `ProjectedPortalState` is computed from viewer clearance vs. content
/// classification by the `ProjectionAuthority`, independently of any transition
/// animation position. A restricted viewer therefore sees `redacted = true` in
/// every frame — expanded, collapsed, and any intermediate transition state.
///
/// ## Part inventory
///
/// | Part | Fields |
/// |------|--------|
/// | frame/header/divider | `frame_*`, `header_*`, `divider_*`, spacing fields |
/// | OUTPUT transcript | `transcript_background`, `transcript_text_color`, `transcript_font_size_px` |
/// | collapsed card | `collapsed_background`, `collapsed_text_color`, `collapsed_font_size_px` |
/// | INPUT region | `composer_background`, `composer_text_color`, `composer_font_size_px` |
///
/// ## Composer rendering (§4.1 / §4.8 — local feedback first)
///
/// When a draft is active, `portal_node` renders the draft text with an inline
/// `▌` caret marker at the cursor byte offset. When `at_capacity == true`, the
/// composer line receives a text-visible `[!] ` prefix, and the compositor
/// colors the draft glyphs themselves from `portal.composer.at_capacity_color`
/// (`composer_draft_base_color`, hud-9gyao) -- a precise, bounded per-line
/// treatment, never a color run on this cached transcript node.
/// This is entirely local — the compositor reads from the adapter's draft
/// display state without any remote roundtrip.
#[derive(Clone, Debug, PartialEq)]
pub struct PortalVisualTokens {
    // Promoted native frame/header chrome (hud-z5uem).
    pub frame_background: proto::Rgba,
    pub frame_opacity: f32,
    pub header_text_color: proto::Rgba,
    pub header_font_size_px: f32,
    pub divider_color: proto::Rgba,
    pub divider_grip_color: proto::Rgba,
    pub content_inset_px: f32,
    // Transcript body (expanded presentation)
    pub transcript_background: proto::Rgba,
    pub transcript_text_color: proto::Rgba,
    pub transcript_font_size_px: f32,

    /// Per-turn role-attribution color for non-assistant agent-side OUTPUT turns
    /// (tool / status / error / other), carried as a real `color_run` span over
    /// each attributed turn's text so a viewer can tell the assistant's own
    /// conversational prose (`transcript_text_color`) from tool/system
    /// scaffolding within the same OUTPUT transcript (§Conversational Turn Model
    /// and Per-Turn Role Attribution, hud-26869). Token-resolved, never a literal
    /// color in the render path (§6.1). Derived from each unit's runtime-assigned
    /// `OutputKind`, so an adapter cannot forge a turn's role.
    pub transcript_system_color: proto::Rgba,

    // Degraded / disconnect treatment (portal-disconnect-resume-ux §2/§3).
    /// Dimmed transcript text shown while the portal is disconnected/stale.
    pub transcript_dim_text_color: proto::Rgba,
    /// Dimmed transcript background shown while the portal is disconnected/stale.
    pub transcript_dim_background: proto::Rgba,
    /// Color of the content-free stale/disconnect marker (ambient, not alarming).
    pub stale_marker_color: proto::Rgba,
    /// Color of the visible disconnect badge (hud-jgf41): the left-edge
    /// lifecycle-accent bar geometry, recolored while connection-degraded.
    /// Driven by the redaction-independent `connection_degraded` flag, unlike
    /// the lifecycle accent (which clears under redaction), so a restricted
    /// viewer still sees the portal has dropped — geometry-only, no content or
    /// identity disclosed. Source token: `portal.disconnect_badge.color`.
    pub disconnect_badge_color: proto::Rgba,
    /// Width (px) of the disconnect badge accent bar. Source token:
    /// `portal.disconnect_badge.width_px`.
    pub disconnect_badge_width_px: f32,
    /// Color of the ambient unread-output-count indicator (hud-meqet). Muted
    /// by design — a presence engine surfaces a quiet count, never a loud
    /// notification badge. Source token: `portal.unread_indicator.color`.
    pub unread_indicator_color: proto::Rgba,
    /// Color of the in-transcript unread divider (hud-g1ena.2,
    /// portal-chat-grade-affordances §Unread Divider and Ambient Unread Count).
    /// Marks the boundary before the oldest retained unseen agent-authored turn
    /// when the viewer returns to a portal with unread content. Distinct from the
    /// generic turn separator so a viewer can tell the unread boundary from an
    /// ordinary turn break, yet still ambient — a presence engine marks where
    /// unread begins without behaving like a notification. Source token:
    /// `portal.unread_divider.color`.
    pub unread_divider_color: proto::Rgba,
    /// Color of the ambient awaiting-reply (question) indicator (hud-jip0k).
    /// Set when the owning LLM's most recently published output has
    /// `expects_reply == true` — a core presence semantic signaling the
    /// output is a question awaiting a viewer reply, not a chatbot
    /// notification. Ambient by design, matching the muted-tone convention
    /// of the other quiet-signal indicators. Source token:
    /// `portal.awaiting_reply.color`.
    pub awaiting_reply_color: proto::Rgba,
    /// Color of the friendly first-run empty-portal treatment (hud-g1ena.6,
    /// portal-chat-grade-affordances §First-Run Empty Portal Treatment) that
    /// replaces the literal `<empty projection stream>` placeholder. Quiet and
    /// inviting by design — a presence engine's empty surface reads as calm and
    /// ready, never as an error. Source token: `portal.empty_state.color`.
    pub empty_state_color: proto::Rgba,
    /// Color of the connecting-state marker (portal-chat-grade-affordances
    /// §Connecting State Distinction) shown while a portal is attached but has
    /// never connected (`has_ever_connected == false`). A cool "spinning up" hue
    /// deliberately distinct from the amber `stale_marker_color` so a starting-up
    /// portal never reads as failing. Ambient by design. Source token:
    /// `portal.connecting_marker.color`.
    pub connecting_marker_color: proto::Rgba,
    /// Color of the ambient header activity cue (portal-chat-grade-affordances
    /// §Agent Activity and Streaming Cue, hud-g1ena.5) shown while the owning
    /// adapter is actively appending to the transcript. Derived from observed
    /// appends (`appended_at_wall_us` vs render-time now), never a separate
    /// "typing" protocol. Muted/ambient — continuous streaming never
    /// re-escalates attention. Source token: `portal.activity_cue.color`.
    pub activity_cue_color: proto::Rgba,
    /// Color of the ambient transcript-tail streaming cursor shown while the
    /// agent is actively appending. Same activity semantic as
    /// `activity_cue_color`; a distinct token so a profile can style the tail
    /// cursor independently. Source token: `portal.streaming_cursor.color`.
    pub streaming_cursor_color: proto::Rgba,

    // Viewer-turn delivery acknowledgement (portal-chat-grade-affordances
    // §Viewer Turn Delivery Acknowledgement, hud-g1ena.1). Three ambient cue
    // classes derived from the runtime's already-tracked `InputDeliveryState` on
    // the viewer's echoed turn — never a new adapter round trip.
    /// Color of the ambient in-flight (Pending/Deferred) viewer-turn delivery cue.
    /// Source token: `portal.delivery.inflight_color`.
    pub delivery_inflight_color: proto::Rgba,
    /// Color of the ambient delivered (Delivered/Handled) viewer-turn delivery cue.
    /// Source token: `portal.delivery.delivered_color`.
    pub delivery_delivered_color: proto::Rgba,
    /// Color of the ambient failed (Rejected/Expired) viewer-turn delivery cue.
    /// Muted, never an alarm — a failed cue stays ambient on the turn and never
    /// escalates the portal's interruption class. Source token:
    /// `portal.delivery.failed_color`.
    pub delivery_failed_color: proto::Rgba,

    // Ambient per-turn timestamps (portal-chat-grade-affordances
    // §Ambient Per-Turn Timestamps, hud-g1ena.4).
    /// Color of the ambient per-turn arrival timestamp — SECONDARY presentation
    /// (dim/muted), subordinate to turn content and never an attention source.
    /// Source token: `portal.timestamp.color`.
    pub timestamp_color: proto::Rgba,
    /// Component-profile-governed visibility/granularity of the ambient per-turn
    /// arrival timestamp (`off` / `per_turn` / `grouped`). Governs presentation
    /// only; every presented time still derives from the runtime-assigned
    /// wall-clock `appended_at_wall_us`. Source token: `portal.timestamp.granularity`.
    pub timestamp_granularity: TimestampGranularity,

    // Lifecycle affordance accents (cooperative-hud-projection §lifecycle).
    //
    // Drive the viewer-facing affordance for a projection's published
    // `lifecycle_state`. Each `ProjectionLifecycleState` variant maps onto one of
    // these four ambient accents via `lifecycle_accent_color`. Source tokens:
    // `portal.lifecycle.{active,attached,attention,inactive}_color`.
    /// Accent for the actively-working state (`Active`).
    pub lifecycle_active_color: proto::Rgba,
    /// Accent for the attached/ready state (`Attached`).
    pub lifecycle_attached_color: proto::Rgba,
    /// Accent for attention states (`Degraded` / `HudUnavailable`).
    pub lifecycle_attention_color: proto::Rgba,
    /// Accent for winding-down states (`Detached` / `CleanupPending` / `Expired`).
    pub lifecycle_inactive_color: proto::Rgba,
    /// Width (px) of the left-edge lifecycle accent bar. Geometry only; sizes the
    /// `SetTileLifecycleAccent` overlay state the compositor paints from, so the
    /// adapter holds no literal accent dimension. Token: `portal.lifecycle.accent_width_px`.
    pub lifecycle_accent_width_px: f32,

    // Spatial rhythm (structured per-part layout geometry, hud-zn6yw).
    //
    // Layout geometry, NOT visual style: these size the declared `Header` and
    // inter-section gap of the first-class `PortalSurface` parts (see
    // `portal_surface_proto`). Resolved from `portal.spacing.*` design tokens so
    // the adapter holds no literal layout dimension (doctrine: no hardcoded values
    // in the compositor/adapter).
    //
    // The `Header` part is NOT merely descriptive: `SceneGraph::portal_header_band_anchors`
    // prefers the declared surface's `Header` part bounds (even with an empty
    // backing node) to size the draggable header band, so `header_height_px` is
    // the effective header/drag-band height for any resident portal that declares
    // a surface (which is all of them). Its default therefore MATCHES
    // `PORTAL_HEADER_DRAG_BAND_PX_DEFAULT` (52) so the band is unchanged at
    // defaults; `portal.header.drag_band_px` still governs the raw-tile fallback
    // (portals without a declared surface). Replaces the former hardcoded 52px
    // `PORTAL_SURFACE_HEADER_BAND_PX` constant.
    /// Height (px) of the declared `Header` part strip — also the draggable header
    /// band for surface-declared portals. Source token:
    /// `portal.spacing.header_height_px`.
    pub header_height_px: f32,
    /// Vertical gap (px) inserted between stacked portal sections (Header →
    /// Transcript). Source token: `portal.spacing.section_gap_px`.
    pub section_gap_px: f32,

    // Collapsed card (collapsed presentation)
    pub collapsed_background: proto::Rgba,
    pub collapsed_text_color: proto::Rgba,
    pub collapsed_font_size_px: f32,

    // Composer (draft input region — §4.1, §4.8)
    pub composer_background: proto::Rgba,
    pub composer_text_color: proto::Rgba,
    pub composer_font_size_px: f32,
    // NOTE: the composer at-capacity color is NOT mirrored here. It is resolved
    // and applied entirely on the compositor side (`portal.composer.at_capacity_color`
    // -> `composer_draft_base_color`), which paints the draft glyphs directly; the
    // adapter never needs it for the markdown node (hud-9gyao).
}

impl Default for PortalVisualTokens {
    /// Default visual tokens derived from the canonical palette in
    /// `tze_hud_config::PortalPartTokens::default()`.
    ///
    /// This is the **single source of truth** for the default portal palette.
    /// Previously this `impl` contained hard-coded float literals that diverged
    /// from the config defaults by up to 3 ULPs of rounding. The duplicate was
    /// eliminated in hud-dcynv: both sides now derive from the same source.
    ///
    /// In production these values are superseded by `with_tokens()` which
    /// accepts tokens produced by
    /// `tze_hud_runtime::portal_tokens::portal_visual_tokens_from_part_tokens`.
    /// This default is used only in tests that do not exercise the token path,
    /// and as a fallback when no tokens are supplied to the adapter.
    fn default() -> Self {
        portal_visual_tokens_from_part_tokens(&tze_hud_config::PortalPartTokens::default())
    }
}

/// Result timing for adapter-local resident-path work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResidentGrpcBudgetSample {
    pub elapsed_us: u64,
    pub budget_us: u64,
}

impl ResidentGrpcBudgetSample {
    pub fn within_budget(self) -> bool {
        self.elapsed_us <= self.budget_us
    }
}

/// Geometry and lease configuration for one projected portal tile.
#[derive(Clone, Debug)]
pub struct ResidentGrpcPortalConfig {
    pub lease_id: Vec<u8>,
    pub expanded_bounds: proto::Rect,
    pub compact_bounds: proto::Rect,
    pub z_order: u32,
}

impl ResidentGrpcPortalConfig {
    pub fn new(lease_id: Vec<u8>) -> Self {
        Self {
            lease_id,
            expanded_bounds: proto::Rect {
                x: 64.0,
                y: 180.0,
                width: DEFAULT_EXPANDED_W,
                height: DEFAULT_EXPANDED_H,
            },
            compact_bounds: proto::Rect {
                x: 64.0,
                y: 180.0,
                width: DEFAULT_COMPACT_W,
                height: DEFAULT_COMPACT_H,
            },
            z_order: DEFAULT_Z_ORDER,
        }
    }
}

/// Kind of resident operation produced by the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentGrpcPortalCommandKind {
    CreatePortalTile,
    ReusePortalTile,
    RenderPortal,
}

/// One outbound `HudSession` client message plus adapter-local budget evidence.
#[derive(Debug)]
pub struct ResidentGrpcPortalCommand {
    pub kind: ResidentGrpcPortalCommandKind,
    pub message: session_proto::ClientMessage,
    pub budget: ResidentGrpcBudgetSample,
}

/// Local-first result for a HUD composer submission mapped into the semantic
/// cooperative projection inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResidentGrpcPortalInputResult {
    pub feedback: PortalInputFeedback,
    pub budget: ResidentGrpcBudgetSample,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResidentGrpcAdapterError {
    #[error("resident portal tile has not been created or recorded")]
    MissingPortalTile,
}

/// Kind of draft-aware command produced by the adapter.
///
/// Used to distinguish draft-state update renders from full portal renders so
/// callers can route them to the correct tile mutation path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidentGrpcDraftCommandKind {
    /// Update the composer text/caret display to reflect a state-stream draft
    /// notification. This is a coalesced update; the adapter discards older
    /// notifications and renders only the latest snapshot.
    UpdateComposerDisplay,
    /// Process a transactional draft submission (forward to semantic inbox).
    ProcessSubmission,
    /// Process a transactional cancel (clear composer display).
    ProcessCancel,
}

/// One outbound command produced by the draft notification path.
#[derive(Debug)]
pub struct ResidentGrpcDraftCommand {
    pub kind: ResidentGrpcDraftCommandKind,
    pub budget: ResidentGrpcBudgetSample,
    /// Draft text at the time of the command (empty on cancel).
    pub draft_text: String,
    /// Cursor byte offset (for composer display).
    pub cursor: usize,
    /// Whether the draft is at capacity.
    pub at_capacity: bool,
    /// Sequence from the runtime draft buffer.
    pub sequence: u64,
}

/// Stateful daemon-side adapter for one projected session's resident portal.
///
/// ## Token-driven styling (§6.1 — no literal visual values)
///
/// Construct with `ResidentGrpcPortalAdapter::new` for default tokens, or
/// `ResidentGrpcPortalAdapter::with_tokens` to supply resolved design tokens.
///
/// Every color, font size, and transition duration in the rendered portal tile
/// comes from `visual_tokens`, never from inline literals. To reskin the portal,
/// call `set_visual_tokens` with a freshly-resolved `PortalVisualTokens` built
/// from the updated token map — no adapter logic changes required.
///
/// ## Composer draft display (§4.1 / §4.8 — local feedback first)
///
/// The adapter tracks the current draft display state in `composer_display`.
/// After `consume_draft_batch` or `apply_draft_notification` returns an
/// `UpdateComposerDisplay` command, the caller does NOT need to re-set anything:
/// the adapter updates its own `composer_display` field automatically so the
/// next `render_portal_message` / `portal_node` call immediately reflects the
/// current draft text, caret, and at-capacity state.
#[derive(Clone, Debug)]
pub struct ResidentGrpcPortalAdapter {
    config: ResidentGrpcPortalConfig,
    tile_id: Option<Vec<u8>>,
    next_input_sequence: u64,
    /// Latest draft sequence seen by this adapter — used to skip stale
    /// state-stream notifications that arrive out of order.
    last_draft_sequence: u64,
    /// Resolved visual tokens for portal part styling.
    ///
    /// All visual properties (colors, font sizes, transition durations) in the
    /// rendered portal tile MUST originate here. A profile swap updates this
    /// field; no other code in the adapter must change.
    visual_tokens: PortalVisualTokens,
    /// Current composer draft display state for local-first rendering (§4.1).
    ///
    /// Updated by `consume_draft_batch` / `apply_draft_notification` whenever
    /// an `UpdateComposerDisplay` command is produced. Reset to `None` on
    /// `ProcessCancel`. The `portal_node` render path reads this field to
    /// produce the draft text + caret + at-capacity visual without any remote
    /// roundtrip.
    composer_display: Option<ComposerDisplayState>,
    /// The portal-surface topology and part-geometry signature last declared for
    /// this tile, or `None` before the first declaration (hud-rpm9s).
    ///
    /// The structural `SetPortalSurface` declaration is Transactional and must
    /// ride exactly one render batch **per topology** — so steady-state renders
    /// stay on the coalescible StateStream path (carrying only the
    /// `UpdatePortalSurfaceState` lifecycle/display patch). But the declared part
    /// set depends on `(presentation, interaction_enabled)` — an Expanded portal
    /// declares Header/Transcript/(Composer) while a Collapsed one declares a
    /// CollapsedCard — so a presentation/interaction transition CHANGES the part
    /// topology and must **re-declare** (a fresh `SetPortalSurface`); otherwise
    /// the coalescible patch would flip `display_state` without ever adding the
    /// matching part (hud-rpm9s review). This tracks the last-declared topology so
    /// [`render_batch_with_surface`](Self::render_batch_with_surface) re-declares
    /// only when it actually changes, and coalescibly patches otherwise.
    ///
    /// The key also carries the spacing-geometry signature
    /// (`header_height_px` / `section_gap_px` as raw f32 bits, hud-zn6yw) and
    /// the current tile-local width/height. Those values size the declared
    /// Header/Transcript part bounds but ride ONLY the `SetPortalSurface`
    /// declaration (the coalescible `UpdatePortalSurfaceState` patch carries no
    /// geometry). A live profile swap or resize that changes them MUST
    /// re-declare, or the declared part bounds — and the header/drag-band derived
    /// from them — would stay stale until the next presentation/interaction
    /// transition.
    declared_topology: Option<(ProjectedPortalPresentation, bool, u32, u32, u32, u32)>,
}

/// Local composer draft display state cached in the adapter.
///
/// Carries only what `portal_node` needs to render the composer region:
/// the current draft text, the caret byte offset, and the at-capacity flag.
/// This is **not** a copy of `ComposerDraft` — it is the last state-stream
/// snapshot delivered to the adapter, suitable for display.
///
/// Spec: §4.1 — "local rendering of text, caret, and selection within the
/// input-to-local-ack budget."
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerDisplayState {
    /// Draft text at the time of the last delivered notification.
    pub text: String,
    /// Cursor byte offset into `text` (where the caret `▌` is inserted).
    pub cursor: usize,
    /// True when the draft reached its byte cap on the last mutation.
    pub at_capacity: bool,
    /// Monotonic sequence from the originating `DraftStateNotification`.
    pub sequence: u64,
}

/// Runtime-owned geometry for the promoted native two-pane projection profile.
///
/// The LLM-facing projection operations never carry these values. Geometry is
/// resolved once per publish from the tile bounds plus the active portal token
/// profile, then reused by the scene subtree, composer hit region, and governed
/// `PortalSurface` descriptor. No frame-loop layout work is introduced.
#[derive(Clone, Copy, Debug)]
struct ExpandedPortalGeometry {
    full: proto::Rect,
    header: proto::Rect,
    input_pane: proto::Rect,
    input_content: proto::Rect,
    output_pane: proto::Rect,
    output_content: proto::Rect,
    divider: proto::Rect,
    divider_grip: proto::Rect,
}

impl ResidentGrpcPortalAdapter {
    /// Create a new adapter with default visual tokens.
    ///
    /// In production, call `with_tokens` or `set_visual_tokens` immediately
    /// after construction to supply the runtime's resolved design token set.
    /// Using default tokens is acceptable only in tests that do not exercise
    /// the profile-swap path.
    pub fn new(config: ResidentGrpcPortalConfig) -> Self {
        Self {
            config,
            tile_id: None,
            next_input_sequence: 0,
            last_draft_sequence: 0,
            visual_tokens: PortalVisualTokens::default(),
            composer_display: None,
            declared_topology: None,
        }
    }

    /// Create a new adapter with the given resolved visual tokens.
    ///
    /// This is the preferred constructor for production use. Build
    /// `tokens` from the runtime's resolved `DesignTokenMap` by calling
    /// `tze_hud_config::resolve_portal_tokens` first to get `PortalPartTokens`,
    /// then converting with
    /// `tze_hud_runtime::portal_tokens::portal_visual_tokens_from_part_tokens`
    /// to obtain `PortalVisualTokens`.
    pub fn with_tokens(config: ResidentGrpcPortalConfig, tokens: PortalVisualTokens) -> Self {
        Self {
            config,
            tile_id: None,
            next_input_sequence: 0,
            last_draft_sequence: 0,
            visual_tokens: tokens,
            composer_display: None,
            declared_topology: None,
        }
    }

    /// Update visual tokens (e.g., after a profile hot-reload).
    ///
    /// The next `render_portal_message` call after this returns will use the
    /// new token values. No other adapter state changes — this is the
    /// "profile swap without adapter logic change" contract from §6.1.
    pub fn set_visual_tokens(&mut self, tokens: PortalVisualTokens) {
        self.visual_tokens = tokens;
    }

    /// Returns the current visual tokens (for inspection / test assertions).
    pub fn visual_tokens(&self) -> &PortalVisualTokens {
        &self.visual_tokens
    }

    /// Returns the current composer display state (for inspection / test assertions).
    ///
    /// `None` when no draft is active (no `UpdateComposerDisplay` command has
    /// been produced since construction or the last `ProcessCancel`).
    pub fn composer_display(&self) -> Option<&ComposerDisplayState> {
        self.composer_display.as_ref()
    }

    pub fn tile_id(&self) -> Option<&[u8]> {
        self.tile_id.as_deref()
    }

    pub fn lease_id(&self) -> &[u8] {
        &self.config.lease_id
    }

    /// Returns the configured expanded-presentation viewport height in pixels.
    ///
    /// Equals `ResidentGrpcPortalConfig::expanded_bounds.height` as set at
    /// adapter construction time (default: `DEFAULT_EXPANDED_H`).
    pub fn config_expanded_height(&self) -> f32 {
        self.config.expanded_bounds.height
    }

    /// Returns the configured compact-presentation viewport height in pixels.
    ///
    /// Equals `ResidentGrpcPortalConfig::compact_bounds.height` as set at
    /// adapter construction time (default: `DEFAULT_COMPACT_H`).
    pub fn config_compact_height(&self) -> f32 {
        self.config.compact_bounds.height
    }

    /// Returns the configured viewport height for the given presentation mode.
    ///
    /// Used by the drain loop to populate `PortalAppendGeometry::viewport_height_px`
    /// as a fallback when no live geometry snapshot is available (hud-0528i).
    /// Expanded → `expanded_bounds.height`; Collapsed → `compact_bounds.height`.
    pub fn config_viewport_height(&self, presentation: crate::ProjectedPortalPresentation) -> f32 {
        match presentation {
            crate::ProjectedPortalPresentation::Expanded => self.config.expanded_bounds.height,
            crate::ProjectedPortalPresentation::Collapsed => self.config.compact_bounds.height,
        }
    }

    /// Record the tile ID returned by the resident `CreateTile` mutation.
    pub fn record_created_tile(&mut self, tile_id: Vec<u8>) {
        self.tile_id = Some(tile_id);
    }

    /// Render the portal markdown content for the given state without building
    /// a full gRPC proto message.
    ///
    /// Useful for stdio surfaces that need the semantic content to include in
    /// a JSON drain record, without taking a prost dependency on the binary.
    /// The output is the same string that `portal_node` places in
    /// `TextMarkdownNodeProto::content`.
    ///
    /// `now_wall_us` is the render's wall-clock reference used to derive the
    /// ambient agent-activity / streaming-cursor cue from observed appends
    /// (hud-g1ena.5); callers pass the same timestamp they hand to
    /// `render_portal_message` so the drain-record markdown matches the tile.
    ///
    /// Ambient per-turn timestamps (hud-g1ena.4) are presented per the adapter's
    /// profile-resolved granularity, so the drain-record markdown matches the tile
    /// content byte-for-byte.
    pub fn render_portal_markdown(&self, state: &ProjectedPortalState, now_wall_us: u64) -> String {
        portal_markdown_with(
            state,
            self.composer_display.as_ref(),
            now_wall_us,
            self.visual_tokens.timestamp_granularity,
        )
    }

    /// Render expanded/collapsed projected state into the existing resident
    /// portal tile, including current geometry and input mode.
    pub fn render_portal_message(
        &mut self,
        state: &ProjectedPortalState,
        sequence: u64,
        timestamp_wall_us: u64,
    ) -> Result<ResidentGrpcPortalCommand, ResidentGrpcAdapterError> {
        let started = Instant::now();
        // Drive the promoted portal through the first-class surface API: the
        // first render prepends the one-time `SetPortalSurface` declaration, and
        // every render carries the coalescible `UpdatePortalSurfaceState` patch
        // (hud-rpm9s). PublishToTile atomically materializes the native subtree.
        let batch = self.render_batch_with_surface(state, timestamp_wall_us)?;
        Ok(self.command(
            ResidentGrpcPortalCommandKind::RenderPortal,
            sequence,
            timestamp_wall_us,
            session_proto::client_message::Payload::MutationBatch(batch),
            started,
        ))
    }

    // ── Draft notification methods (hud-5jbra.4) ─────────────────────────
    //
    // These replace the per-keystroke composer-text republish pattern. Instead
    // of publishing a new TextMarkdownNode on every CharacterEvent, the adapter
    // now:
    //   1. Calls `consume_draft_batch` with the AdapterDraftBatch it receives
    //      from the runtime's ComposerDraft notification path.
    //   2. For state-stream notifications: renders a `UpdateComposerDisplay`
    //      command containing the latest draft text (for compositor display).
    //   3. For transactional submissions: calls `submit_composer_text`.
    //
    // This satisfies spec §4.6: "update the cooperative projection adapter …
    // to consume draft-state notifications instead of per-keystroke republish."

    /// Consume a `AdapterDraftBatch` and produce draft commands for the adapter
    /// to process.
    ///
    /// The batch may contain a coalesced state-stream notification, a
    /// transactional submission, or a cancel. The adapter processes them in
    /// order: state-stream notification first (for display), then
    /// submission/cancel.
    ///
    /// Returns the set of commands to dispatch. State-stream commands carry
    /// only display data (draft text, cursor); the adapter applies them locally
    /// without a semantic inbox enqueue.
    pub fn consume_draft_batch(
        &mut self,
        batch: &AdapterDraftBatch,
    ) -> Vec<ResidentGrpcDraftCommand> {
        let mut commands = Vec::new();
        let started = Instant::now();

        // State-stream notification: only process if sequence is newer
        if let Some(notification) = &batch.latest {
            if notification.sequence > self.last_draft_sequence {
                self.last_draft_sequence = notification.sequence;
                // Update local composer display state for next portal_node render.
                // This is the local-first path: the compositor reads from here
                // without any remote roundtrip (spec §4.1 — local feedback first).
                self.composer_display = Some(ComposerDisplayState {
                    text: notification.text.clone(),
                    cursor: notification.cursor,
                    at_capacity: notification.at_capacity,
                    sequence: notification.sequence,
                });
                commands.push(ResidentGrpcDraftCommand {
                    kind: ResidentGrpcDraftCommandKind::UpdateComposerDisplay,
                    budget: sample_budget(started, RESIDENT_PORTAL_UPDATE_BUILD_BUDGET_US),
                    draft_text: notification.text.clone(),
                    cursor: notification.cursor,
                    at_capacity: notification.at_capacity,
                    sequence: notification.sequence,
                });
            }
        }

        // Transactional cancel — clear composer display state.
        // Advance last_draft_sequence to the cancel's sequence so that any
        // delayed state-stream notifications that arrived before the cancel
        // event (sequence ≤ cancel.sequence) are silently dropped instead of
        // re-populating composer_display after the clear.
        if let Some(cancel) = &batch.cancel {
            self.composer_display = None;
            self.last_draft_sequence = self.last_draft_sequence.max(cancel.sequence);
            commands.push(ResidentGrpcDraftCommand {
                kind: ResidentGrpcDraftCommandKind::ProcessCancel,
                budget: sample_budget(started, RESIDENT_PORTAL_INPUT_FEEDBACK_BUDGET_US),
                draft_text: String::new(),
                cursor: 0,
                at_capacity: false,
                sequence: cancel.sequence,
            });
        }

        // Transactional submission (handled separately; caller should also
        // call `submit_composer_text` with the submission text).
        // Submission clears composer display state (post-submit display clear).
        // Advance last_draft_sequence so that any delayed state-stream
        // notifications with sequence ≤ submission.sequence are ignored.
        if let Some(submission) = &batch.submission {
            self.composer_display = None;
            self.last_draft_sequence = self.last_draft_sequence.max(submission.sequence);
            commands.push(ResidentGrpcDraftCommand {
                kind: ResidentGrpcDraftCommandKind::ProcessSubmission,
                budget: sample_budget(started, RESIDENT_PORTAL_INPUT_FEEDBACK_BUDGET_US),
                draft_text: submission.text.clone(),
                cursor: submission.text.len(),
                at_capacity: false,
                sequence: submission.sequence,
            });
        }

        commands
    }

    /// Build a draft-state notification from an `AdapterDraftNotification`
    /// without consuming a full batch (useful for direct notification delivery).
    ///
    /// Returns `None` if the notification is stale (sequence ≤ last seen).
    pub fn apply_draft_notification(
        &mut self,
        notification: &AdapterDraftNotification,
    ) -> Option<ResidentGrpcDraftCommand> {
        let started = Instant::now();
        if notification.sequence <= self.last_draft_sequence {
            return None;
        }
        self.last_draft_sequence = notification.sequence;
        // Update local composer display state for next portal_node render.
        self.composer_display = Some(ComposerDisplayState {
            text: notification.text.clone(),
            cursor: notification.cursor,
            at_capacity: notification.at_capacity,
            sequence: notification.sequence,
        });
        Some(ResidentGrpcDraftCommand {
            kind: ResidentGrpcDraftCommandKind::UpdateComposerDisplay,
            budget: sample_budget(started, RESIDENT_PORTAL_UPDATE_BUILD_BUDGET_US),
            draft_text: notification.text.clone(),
            cursor: notification.cursor,
            at_capacity: notification.at_capacity,
            sequence: notification.sequence,
        })
    }

    /// Last draft sequence seen by this adapter.
    pub fn last_draft_sequence(&self) -> u64 {
        self.last_draft_sequence
    }

    /// Map submitted HUD composer text to the cooperative semantic inbox. This
    /// is not a raw keystroke path; the active LLM session later polls the
    /// pending item through the projection operation contract.
    pub fn submit_composer_text(
        &mut self,
        authority: &mut ProjectionAuthority,
        projection_id: &str,
        text: String,
        submitted_at_wall_us: u64,
        expires_at_wall_us: Option<u64>,
        content_classification: ContentClassification,
    ) -> ResidentGrpcPortalInputResult {
        let started = Instant::now();
        self.next_input_sequence += 1;
        let feedback = authority.submit_portal_input(
            projection_id,
            PortalInputSubmission {
                input_id: format!("input-{}", self.next_input_sequence),
                submission_text: text,
                submitted_at_wall_us,
                expires_at_wall_us,
                content_classification,
            },
        );
        ResidentGrpcPortalInputResult {
            feedback,
            budget: sample_budget(started, RESIDENT_PORTAL_INPUT_FEEDBACK_BUDGET_US),
        }
    }

    /// Build the portal-content `MutationBatch` for the given projected state.
    ///
    /// This is the single render path shared by both adapter families: the
    /// gRPC/wire family wraps it in a `ClientMessage` (`render_portal_message`) and sends it over the session stream, while the
    /// in-process cooperative driver applies it directly to the `SceneGraph` via
    /// `tze_hud_protocol::convert::apply_portal_render_batch_to_scene`. It is
    /// `pub` so the runtime driver can ask the adapter to render content rather
    /// than only counting transcript lines for geometry (the cooperative
    /// grey-tile fix, hud-utbiy).
    ///
    /// Requires `record_created_tile` to have been called first (returns
    /// [`ResidentGrpcAdapterError::MissingPortalTile`] otherwise).
    ///
    /// `now_wall_us` is the render's wall-clock reference (the same timestamp the
    /// caller stamps on the outbound message). It is used only to derive the
    /// ambient agent-activity / streaming-cursor cue from observed appends
    /// (hud-g1ena.5) — a portal re-rendered long after its last append shows no
    /// cursor because `now - appended_at` exceeds the quiesce window.
    pub fn render_batch(
        &self,
        state: &ProjectedPortalState,
        now_wall_us: u64,
    ) -> Result<session_proto::MutationBatch, ResidentGrpcAdapterError> {
        let tile_id = self
            .tile_id
            .clone()
            .ok_or(ResidentGrpcAdapterError::MissingPortalTile)?;

        // Generate an explicit root node ID (little-endian UUID bytes per RFC 0001
        // §4.1) so the published transcript root has a stable, inspectable identity.
        // The composer hit region is no longer parented to it via an in-batch
        // `AddNode` (see the `SetTileComposerInteraction` mutation below), so no
        // big-endian parent-id encoding is needed here (hud-iofav).
        let root_uuid = uuid::Uuid::now_v7();
        let root_id_le = root_uuid.to_bytes_le().to_vec();

        let mut mutations = vec![
            proto::MutationProto {
                mutation: Some(proto::mutation_proto::Mutation::PublishToTile(
                    proto::PublishToTileMutation {
                        element_id: tile_id.clone(),
                        bounds: Some(self.bounds_for_state(state)),
                        node: Some(self.portal_node(state, root_id_le, now_wall_us)),
                    },
                )),
            },
            proto::MutationProto {
                mutation: Some(proto::mutation_proto::Mutation::UpdateTileInputMode(
                    proto::UpdateTileInputModeMutation {
                        tile_id: tile_id.clone(),
                        input_mode: if state.interaction_enabled {
                            proto::TileInputModeProto::TileInputModeCapture as i32
                        } else {
                            proto::TileInputModeProto::TileInputModeLocalOnly as i32
                        },
                    },
                )),
            },
        ];

        // Lifecycle affordance accent (hud-m48i0): a coalescible StateStream
        // tile-update carrying the token-resolved accent color + width. The
        // runtime stores it as per-tile overlay state and the compositor paints a
        // left-edge bar from it. This is deliberately NOT an AddNode: a
        // per-republish AddNode classifies the whole batch Transactional and flips
        // non-interactive lifecycle-visible portals off the coalescible path
        // (hud-mzk74). Stored as overlay state, the accent also survives the
        // PublishToTile content republish above (which replaces the node tree).
        //
        // Redaction-gated: when lifecycle_state is None (authority redacted),
        // color is None / width 0 → the mutation CLEARS the accent, exactly like
        // the redaction-gated `status:` line. Emitted every render so the latest
        // lifecycle color coalesces; the markdown node keeps only its zero-length
        // lifecycle sentinel run, so the cached markdown path is untouched (#947).
        //
        // Disconnect badge (hud-jgf41): `is_connection_degraded` takes precedence
        // over `lifecycle_state` here — and, unlike `lifecycle_state`, is NOT
        // redaction-gated (`is_connection_degraded` docs: set by the authority
        // from the session lifecycle, independent of redaction), so a restricted
        // viewer still sees this same left-edge accent bar turn to the disconnect
        // badge color while the portal is degraded. This reuses the existing
        // `SetTileLifecycleAccent` overlay mechanism verbatim (no new mutation,
        // no new scene-graph node type) — only the color/width source changes.
        let (accent_color, accent_width) = if is_connection_degraded(state) {
            (
                Some(self.visual_tokens.disconnect_badge_color),
                self.visual_tokens.disconnect_badge_width_px,
            )
        } else {
            match state.lifecycle_state {
                Some(lifecycle) => (
                    Some(lifecycle_accent_color(lifecycle, &self.visual_tokens)),
                    self.visual_tokens.lifecycle_accent_width_px,
                ),
                None => (None, 0.0),
            }
        };
        mutations.push(proto::MutationProto {
            mutation: Some(proto::mutation_proto::Mutation::SetTileLifecycleAccent(
                proto::SetTileLifecycleAccentMutation {
                    tile_id: tile_id.clone(),
                    color: accent_color,
                    width_px: accent_width,
                },
            )),
        });

        // Composer interaction hit region (hud-iofav). A coalescible StateStream
        // tile-update carrying the composer hit-region spec as per-tile overlay
        // state — deliberately NOT a per-republish `AddNode`. An `AddNode` marks the
        // whole batch Transactional (`classify_inbound_batch`), which on an
        // interaction-enabled streaming portal — the HOTTEST path — defeats
        // StateStream latest-wins coalescing under freeze/backpressure (hud-mzk74).
        // Stored as overlay state, the runtime derives the hit-region scene node and
        // RE-ATTACHES it under the tile root after every `PublishToTile` content
        // republish (which replaces the whole node tree), so the composer survives
        // consecutive renders while the batch stays coalescible. The derived node is
        // a real scene node, so `hit_test`, focus acquisition, and the
        // `ComposerDraftManager` all operate exactly as with the former `AddNode`.
        //
        // Emitted every render so the latest spec coalesces (mirroring the lifecycle
        // accent + unread count above): `Some(..)` when interaction is enabled, and
        // an absent composer (`None`) CLEARS it when interaction is disabled — the
        // enable→disable transition detaches the derived node, exactly like the
        // redaction-gated accent clear.
        let composer = if state.interaction_enabled {
            Some(proto::HitRegionNodeProto {
                bounds: Some(match state.presentation {
                    ProjectedPortalPresentation::Expanded => {
                        self.expanded_portal_geometry(state).input_pane
                    }
                    ProjectedPortalPresentation::Collapsed => self.local_bounds_for_state(state),
                }),
                interaction_id: format!("{}-composer", state.portal_id),
                accepts_focus: true,
                // accepts_pointer MUST be true for click-to-focus (hud-v4k1h).
                // SceneGraph::hit_test only returns HitResult::NodeHit for HitRegion
                // nodes with accepts_pointer = true; InputProcessor::process_with_focus
                // only acquires keyboard focus on a NodeHit. With this false, a
                // pointer-down on the composer falls through to a bare TileHit, so the
                // portal never gains focus and every keystroke / Ctrl+= resize chord
                // is silently dropped even though the OS delivered it. Carried through
                // to the derived scene node unchanged.
                accepts_pointer: true,
                auto_capture: false,
                release_on_up: false,
                accepts_composer_input: true,
            })
        } else {
            None
        };
        mutations.push(proto::MutationProto {
            mutation: Some(proto::mutation_proto::Mutation::SetTileComposerInteraction(
                proto::SetTileComposerInteractionMutation {
                    // Clone: the unread-count badge mutation below also needs
                    // `tile_id` (it is pushed last, after this composer mutation).
                    tile_id: tile_id.clone(),
                    composer,
                },
            )),
        });

        // Ambient unread-output count for the jump-to-latest pill badge
        // (hud-hwk2m, portal-chat-grade-affordances §Jump-to-Latest Affordance).
        // A coalescible StateStream tile-update carrying the runtime-owned unread
        // count as per-tile overlay state — the bridged-transport counterpart of
        // the in-process driver's direct `set_tile_unread_count` call. Without it a
        // bridged portal's pill rendered without the badge the in-process path got
        // in #1088 (the count only reached the scene on the suppressed in-process
        // arm). Emitted every render so the latest count coalesces, exactly like
        // the lifecycle accent above; the count rides overlay state so it survives
        // the `PublishToTile` content republish (which replaces the node tree).
        // Value mirrors the in-process arm's `unread_output_count.unwrap_or(0)`: a
        // redacted (`None`) or empty count sends 0 → clears the badge, and the
        // pill's own `scrolled_back` gate hides it the instant the viewer returns
        // to the tail. Pushed LAST so the composer hit region stays at a fixed
        // index for the interaction-path tests.
        let unread_count = state
            .unread_output_count
            .unwrap_or(0)
            .min(u32::MAX as usize) as u32;
        mutations.push(proto::MutationProto {
            mutation: Some(proto::mutation_proto::Mutation::SetTileUnreadCount(
                proto::SetTileUnreadCountMutation {
                    // Final use of `tile_id` in this batch — move, no clone.
                    tile_id,
                    count: unread_count,
                },
            )),
        });

        // NOTE: `render_batch` deliberately emits NO first-class portal-surface
        // mutation — it stays a direct PublishToTile content render (plus the
        // coalescible lifecycle-accent and unread-count overlay updates above),
        // the retained compatibility path. The one-time structural
        // `SetPortalSurface` declaration and
        // the per-render coalescible `UpdatePortalSurfaceState` patch are added by
        // `render_batch_with_surface`. Keeping them out of `render_batch` is
        // load-bearing: any direct PublishToTile caller must never send an
        // `UpdatePortalSurfaceState` before the surface is declared — the wire
        // session server rejects such a batch
        // ATOMICALLY (the whole batch), unlike the in-process warn-and-skip path.

        Ok(session_proto::MutationBatch {
            batch_id: new_scene_id_bytes(),
            lease_id: self.config.lease_id.clone(),
            mutations,
            timing: None,
        })
    }

    /// Build the portal-content batch AND drive the promoted portal through the
    /// first-class surface API — declaring or coalescibly patching as needed
    /// (hud-rpm9s).
    ///
    /// This is the migrated render entry point for the cooperative projection
    /// consumers (the in-process driver and the resident-authority wire loop):
    /// - when the part **topology** changes — the first render, or a
    ///   `(presentation, interaction_enabled)` transition that adds/removes parts
    ///   (e.g. Expanded↔Collapsed swaps Header/Transcript/Composer for a
    ///   CollapsedCard) — it **prepends** a structural (Transactional)
    ///   `SetPortalSurface` that re-declares the full descriptor. The declaration
    ///   already carries the full lifecycle/display state, so NO
    ///   `UpdatePortalSurfaceState` patch is emitted on that render — avoiding any
    ///   in-batch declare-then-patch ordering dependency (the wire session server
    ///   applies a batch atomically);
    /// - when the topology is unchanged, it **appends** a coalescible (StateStream)
    ///   `UpdatePortalSurfaceState` syncing the surface's lifecycle/display,
    ///   mirroring the `SetTileLifecycleAccent` treatment; a redacted lifecycle
    ///   (`lifecycle_state == None`) encodes UNSPECIFIED = "leave unchanged" so
    ///   the patch goes silent under redaction rather than asserting a value;
    /// - every render emits one `PublishToTile` carrying the native subtree. The
    ///   surface descriptor governs; inline children paint without per-node
    ///   mutation fan-out.
    ///
    /// Re-declaration is bounded to structural or part-geometry changes
    /// (profile-swap / collapse-expand / interaction toggle / resize), so the
    /// steady-state render stays on the coalescible path.
    ///
    /// Requires `record_created_tile` first (same contract as `render_batch`).
    pub fn render_batch_with_surface(
        &mut self,
        state: &ProjectedPortalState,
        now_wall_us: u64,
    ) -> Result<session_proto::MutationBatch, ResidentGrpcAdapterError> {
        let mut batch = self.render_batch(state, now_wall_us)?;
        // The declaration key is (presentation, interaction) PLUS all geometry
        // that shapes the declared parts: header/section spacing and the current
        // tile-local size. These bounds ride only the full declaration, so a
        // token swap or local Ctrl+=/pointer resize must re-declare (hud-zn6yw,
        // hud-yrcev).
        let local_bounds = self.local_bounds_for_state(state);
        let topology = (
            state.presentation,
            state.interaction_enabled,
            self.visual_tokens.header_height_px.to_bits(),
            self.visual_tokens.section_gap_px.to_bits(),
            local_bounds.width.to_bits(),
            local_bounds.height.to_bits(),
        );
        if self.declared_topology != Some(topology) {
            // First declaration OR a topology-changing transition: (re)declare the
            // full surface (carries full state); no patch in the same batch.
            let declaration = self.portal_surface_declaration_mutation(state)?;
            batch.mutations.insert(0, declaration);
            self.declared_topology = Some(topology);
        } else {
            // Same topology: coalescible lifecycle/display patch only.
            let tile_id = self
                .tile_id
                .clone()
                .ok_or(ResidentGrpcAdapterError::MissingPortalTile)?;
            batch.mutations.push(proto::MutationProto {
                mutation: Some(proto::mutation_proto::Mutation::UpdatePortalSurfaceState(
                    proto::UpdatePortalSurfaceStateMutation {
                        tile_id,
                        lifecycle: portal_lifecycle_state_proto(state),
                        display_state: portal_display_state_proto(state),
                    },
                )),
            });
        }
        Ok(batch)
    }

    /// Whether the first-class portal surface has been declared yet (test/inspection).
    pub fn surface_declared(&self) -> bool {
        self.declared_topology.is_some()
    }

    /// Build the one-time structural `SetPortalSurface` mutation declaring the
    /// promoted portal's governed 8-part descriptor over its host tile (hud-rpm9s).
    ///
    /// Requires `record_created_tile` first (returns
    /// [`ResidentGrpcAdapterError::MissingPortalTile`] otherwise).
    pub fn portal_surface_declaration_mutation(
        &self,
        state: &ProjectedPortalState,
    ) -> Result<proto::MutationProto, ResidentGrpcAdapterError> {
        let tile_id = self
            .tile_id
            .clone()
            .ok_or(ResidentGrpcAdapterError::MissingPortalTile)?;
        Ok(proto::MutationProto {
            mutation: Some(proto::mutation_proto::Mutation::SetPortalSurface(
                proto::SetPortalSurfaceMutation {
                    tile_id,
                    surface: Some(self.portal_surface_proto(state)),
                },
            )),
        })
    }

    /// Build the first-class `PortalSurfaceProto` descriptor for the current
    /// projected state (hud-rpm9s).
    ///
    /// The descriptor GROUPS the portal's named parts, identity, and
    /// lifecycle/display state; it does not paint pixels. Each declared part
    /// carries surface-local geometry and leaves `node` empty because the
    /// per-render inline subtree receives fresh node IDs. `Frame` spans the
    /// surface, an Expanded portal splits into `Header`, INPUT `Composer`, OUTPUT
    /// `Transcript`, and `Divider`, and a Collapsed portal declares a
    /// `CollapsedCard`.
    fn portal_surface_proto(&self, state: &ProjectedPortalState) -> proto::PortalSurfaceProto {
        let full = self.local_bounds_for_state(state);
        let part = |kind: proto::PortalPartKindProto, bounds: proto::Rect| proto::PortalPartProto {
            kind: kind as i32,
            bounds: Some(bounds),
            // Empty = no stable backing node reference. PublishToTile replaces
            // the inline subtree on each state change, so retaining a child ID in
            // the descriptor would immediately become stale.
            node: Vec::new(),
        };
        let mut parts = vec![part(proto::PortalPartKindProto::PortalPartKindFrame, full)];
        match state.presentation {
            ProjectedPortalPresentation::Collapsed => {
                parts.push(part(
                    proto::PortalPartKindProto::PortalPartKindCollapsedCard,
                    full,
                ));
            }
            ProjectedPortalPresentation::Expanded => {
                let geometry = self.expanded_portal_geometry(state);
                parts.push(part(
                    proto::PortalPartKindProto::PortalPartKindHeader,
                    geometry.header,
                ));
                parts.push(part(
                    proto::PortalPartKindProto::PortalPartKindTranscript,
                    geometry.output_pane,
                ));
                parts.push(part(
                    proto::PortalPartKindProto::PortalPartKindDivider,
                    geometry.divider,
                ));
                if state.interaction_enabled {
                    parts.push(part(
                        proto::PortalPartKindProto::PortalPartKindComposer,
                        geometry.input_pane,
                    ));
                }
            }
        }
        proto::PortalSurfaceProto {
            identity: Some(proto::PortalIdentityProto {
                session_id: state.portal_id.clone(),
                // Redaction-gated: `display_name` is `None` for a viewer without
                // identity clearance, so the declared name goes empty under
                // redaction rather than leaking the portal_id as a name.
                display_name: state.display_name.clone().unwrap_or_default(),
                peer_class: proto::PortalPeerClassProto::PortalPeerClassResidentLlm as i32,
            }),
            lifecycle: portal_lifecycle_state_proto(state),
            display_state: portal_display_state_proto(state),
            parts,
        }
    }

    fn expanded_portal_geometry(&self, state: &ProjectedPortalState) -> ExpandedPortalGeometry {
        let full = self.local_bounds_for_state(state);
        let header_h = self
            .visual_tokens
            .header_height_px
            .max(0.0)
            .min(full.height);
        let divider_w = self.visual_tokens.section_gap_px.max(1.0).min(full.width);
        let pane_width = ((full.width - divider_w).max(0.0) * 0.5).max(0.0);
        let body_y = header_h;
        let body_h = (full.height - body_y).max(0.0);
        let input_pane = proto::Rect {
            x: 0.0,
            y: body_y,
            width: pane_width,
            height: body_h,
        };
        let divider = proto::Rect {
            x: pane_width,
            y: body_y,
            width: divider_w,
            height: body_h,
        };
        let output_pane = proto::Rect {
            x: divider.x + divider.width,
            y: body_y,
            width: (full.width - divider.x - divider.width).max(0.0),
            height: body_h,
        };
        let inset = self.visual_tokens.content_inset_px.max(0.0);
        let label_band_h = (self.visual_tokens.header_font_size_px + inset).min(body_h);
        let output_content_y = output_pane.y + label_band_h;
        let input_content_y = input_pane.y + label_band_h;
        let input_content = proto::Rect {
            x: input_pane.x + inset,
            y: input_content_y,
            width: (input_pane.width - inset * 2.0).max(0.0),
            height: (input_pane.y + input_pane.height - input_content_y - inset).max(0.0),
        };
        let output_content = proto::Rect {
            x: output_pane.x + inset,
            y: output_content_y,
            width: (output_pane.width - inset * 2.0).max(0.0),
            height: (output_pane.y + output_pane.height - output_content_y - inset).max(0.0),
        };
        let grip_w = (divider_w / 3.0).max(1.0).min(divider_w);
        let grip_h = (header_h * 0.85).max(grip_w).min(body_h);
        let divider_grip = proto::Rect {
            x: divider.x + (divider.width - grip_w) / 2.0,
            y: divider.y + (divider.height - grip_h) / 2.0,
            width: grip_w,
            height: grip_h,
        };

        ExpandedPortalGeometry {
            full,
            header: proto::Rect {
                x: 0.0,
                y: 0.0,
                width: full.width,
                height: header_h,
            },
            input_pane,
            input_content,
            output_pane,
            output_content,
            divider,
            divider_grip,
        }
    }

    fn solid_node(
        id: Vec<u8>,
        color: proto::Rgba,
        bounds: proto::Rect,
        radius: f32,
    ) -> proto::NodeProto {
        proto::NodeProto {
            id,
            data: Some(proto::node_proto::Data::SolidColor(
                proto::SolidColorNodeProto {
                    color: Some(color),
                    bounds: Some(bounds),
                    radius,
                },
            )),
            children: vec![],
            layout: proto::NodeLayoutProto::Absolute as i32,
        }
    }

    fn chrome_text_node(
        content: String,
        bounds: proto::Rect,
        color: proto::Rgba,
        font_size_px: f32,
    ) -> proto::NodeProto {
        proto::NodeProto {
            id: new_node_id_le(),
            data: Some(proto::node_proto::Data::TextMarkdown(
                proto::TextMarkdownNodeProto {
                    content,
                    bounds: Some(bounds),
                    font_size_px,
                    color: Some(color),
                    // Explicit transparency keeps text from repainting square
                    // backdrops over the token-styled rounded solid layers.
                    background: Some(proto::Rgba {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.0,
                    }),
                    color_runs: vec![],
                    overflow: proto::TextOverflowProto::Clip as i32,
                },
            )),
            children: vec![],
            layout: proto::NodeLayoutProto::Absolute as i32,
        }
    }

    fn portal_node(
        &self,
        state: &ProjectedPortalState,
        root_id_le: Vec<u8>,
        now_wall_us: u64,
    ) -> proto::NodeProto {
        if state.presentation == ProjectedPortalPresentation::Collapsed {
            return self.portal_text_node(state, root_id_le, now_wall_us);
        }

        let geometry = self.expanded_portal_geometry(state);
        let mut transcript = self.portal_text_node(state, new_node_id_le(), now_wall_us);
        if let Some(proto::node_proto::Data::TextMarkdown(text)) = transcript.data.as_mut() {
            text.bounds = Some(geometry.output_content);
            // The native OUTPUT pane owns its backdrop. Keeping the legacy
            // single-node backdrop here would put a flat rectangle in the
            // compositor's earlier pass, underneath the rounded frame, and
            // duplicate the pane fill.
            text.background = Some(proto::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            });
        }

        let mut frame_color = self.visual_tokens.frame_background;
        frame_color.a *= self.visual_tokens.frame_opacity.clamp(0.0, 1.0);
        let header_background = if is_connection_degraded(state) {
            self.visual_tokens.transcript_dim_background
        } else {
            frame_color
        };
        let output_background = if is_connection_degraded(state) {
            self.visual_tokens.transcript_dim_background
        } else {
            self.visual_tokens.transcript_background
        };
        let input_label_bounds = proto::Rect {
            x: geometry.input_pane.x + self.visual_tokens.content_inset_px,
            y: geometry.input_pane.y,
            width: (geometry.input_pane.width - self.visual_tokens.content_inset_px * 2.0).max(0.0),
            height: self.visual_tokens.header_font_size_px + self.visual_tokens.content_inset_px,
        };
        let output_label_bounds = proto::Rect {
            x: geometry.output_pane.x + self.visual_tokens.content_inset_px,
            y: geometry.output_pane.y,
            width: (geometry.output_pane.width - self.visual_tokens.content_inset_px * 2.0)
                .max(0.0),
            height: self.visual_tokens.header_font_size_px + self.visual_tokens.content_inset_px,
        };
        let title = state.display_name.as_deref().unwrap_or("Projected session");
        let mut header_title = match state.lifecycle_state {
            Some(lifecycle) => format!("{title} · {lifecycle:?}"),
            None => title.to_string(),
        };
        if let Some(unread) = unread_indicator_line(state.unread_output_count) {
            header_title.push_str(" · ");
            header_title.push_str(&unread);
        }
        if awaiting_reply(state) {
            header_title.push_str(" · awaiting reply");
        } else if agent_activity_active(state, now_wall_us) {
            header_title.push_str(" · ");
            header_title.push_str(PORTAL_ACTIVITY_MARKER_LINE);
        }
        if is_connection_degraded(state) {
            header_title.push_str(" · ");
            header_title.push_str(PORTAL_DISCONNECT_MARKER_LINE);
        }

        let mut root = Self::solid_node(
            root_id_le,
            frame_color,
            geometry.full,
            EXPANDED_PORTAL_CORNER_RADIUS_PX,
        );
        root.children = vec![
            Self::solid_node(
                new_node_id_le(),
                header_background,
                geometry.header,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                header_title,
                geometry.header,
                self.visual_tokens.header_text_color,
                self.visual_tokens.header_font_size_px,
            ),
            Self::solid_node(
                new_node_id_le(),
                self.visual_tokens.composer_background,
                geometry.input_pane,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                "INPUT".to_string(),
                input_label_bounds,
                self.visual_tokens.header_text_color,
                self.visual_tokens.header_font_size_px,
            ),
            Self::chrome_text_node(
                portal_input_markdown(
                    state,
                    self.composer_display.as_ref(),
                    self.visual_tokens.timestamp_granularity,
                ),
                geometry.input_content,
                self.visual_tokens.composer_text_color,
                self.visual_tokens.composer_font_size_px,
            ),
            Self::solid_node(
                new_node_id_le(),
                output_background,
                geometry.output_pane,
                EXPANDED_PORTAL_CORNER_RADIUS_PX,
            ),
            Self::chrome_text_node(
                "OUTPUT".to_string(),
                output_label_bounds,
                self.visual_tokens.header_text_color,
                self.visual_tokens.header_font_size_px,
            ),
            transcript,
            Self::solid_node(
                new_node_id_le(),
                self.visual_tokens.divider_color,
                geometry.divider,
                geometry.divider.width * 0.5,
            ),
            Self::solid_node(
                new_node_id_le(),
                self.visual_tokens.divider_grip_color,
                geometry.divider_grip,
                geometry.divider_grip.width * 0.5,
            ),
        ];
        root
    }

    fn portal_text_node(
        &self,
        state: &ProjectedPortalState,
        root_id_le: Vec<u8>,
        now_wall_us: u64,
    ) -> proto::NodeProto {
        // §6.1 enforcement: every visual value sourced from self.visual_tokens —
        // no literal colors, font sizes, or opacities permitted here.
        let bounds = self.local_bounds_for_state(state);
        let (mut text_color, mut background_color, font_size_px) = match state.presentation {
            ProjectedPortalPresentation::Expanded => (
                self.visual_tokens.transcript_text_color,
                self.visual_tokens.transcript_background,
                self.visual_tokens.transcript_font_size_px,
            ),
            ProjectedPortalPresentation::Collapsed => (
                self.visual_tokens.collapsed_text_color,
                self.visual_tokens.collapsed_background,
                self.visual_tokens.collapsed_font_size_px,
            ),
        };
        // §2: when the driving stream is disconnected/stale, dim the retained
        // transcript window using token-resolved colors (never hardcoded) so the
        // viewer reads it as inactive rather than blanked or faking liveness. The
        // dim treatment applies to the expanded transcript surface; the collapsed
        // card keeps its own palette.
        if is_connection_degraded(state)
            && state.presentation == ProjectedPortalPresentation::Expanded
        {
            text_color = self.visual_tokens.transcript_dim_text_color;
            background_color = self.visual_tokens.transcript_dim_background;
        }
        let (content, turn_spans) = match state.presentation {
            ProjectedPortalPresentation::Expanded => portal_output_markdown_with_spans(
                state,
                now_wall_us,
                self.visual_tokens.timestamp_granularity,
            ),
            ProjectedPortalPresentation::Collapsed => portal_markdown_with_spans(
                state,
                self.composer_display.as_ref(),
                now_wall_us,
                self.visual_tokens.timestamp_granularity,
            ),
        };
        // Byte length of the assembled content: the streaming-cursor tail marker
        // is pinned here (`[content_len..content_len]`) so the compositor can
        // distinguish it from the byte-0 sentinels and recolor the trailing
        // cursor glyph precisely (hud-zlq2v).
        let content_len = content.len();
        proto::NodeProto {
            // Explicit root ID (little-endian UUID bytes per RFC 0001 §4.1) so
            // render_batch can reference it as AddNodeMutation.parent_id in the
            // same batch when adding the composer hit region.
            id: root_id_le,
            data: Some(proto::node_proto::Data::TextMarkdown(
                proto::TextMarkdownNodeProto {
                    content,
                    bounds: Some(bounds),
                    font_size_px,
                    color: Some(text_color),
                    background: Some(background_color),
                    // color_runs carry the disconnect/stale marker color and the
                    // lifecycle-affordance accent when active. Each is a zero-length
                    // sentinel run carrying the token color so the visual token drives
                    // the display without any literal color in the render path
                    // (§2/§6.1: token-resolved, never hardcoded).
                    //
                    // The composer at-capacity indicator is NOT here: its precise,
                    // bounded per-line color is painted directly on the composer draft
                    // by the compositor overlay (`composer_draft_base_color`), which is
                    // the real draft surface — never a zero-length sentinel on this
                    // cached transcript node (hud-9gyao). The text-visible
                    // `composer: [!] at capacity` status line (see `composer_line`)
                    // remains the machine/human-readable at-capacity signal here.
                    color_runs: {
                        let mut runs =
                            stale_marker_color_runs(state, self.visual_tokens.stale_marker_color);
                        // Lifecycle affordance: token-resolved accent reflecting the
                        // published lifecycle_state (active/attached/attention/inactive).
                        // Redaction-gated via state.lifecycle_state being None.
                        runs.extend(lifecycle_marker_color_runs(state, &self.visual_tokens));
                        // Ambient unread-output-count indicator (hud-meqet). Absent
                        // when there is nothing unread or the count is redacted.
                        runs.extend(unread_indicator_color_runs(
                            state,
                            self.visual_tokens.unread_indicator_color,
                        ));
                        // In-transcript unread divider (hud-g1ena.2). Absent unless
                        // an Expanded portal's non-empty retained window contains an
                        // unread agent-authored turn; clears with the count on tail
                        // view. Distinct token from the ambient count above so the
                        // boundary rule and the count can be reskinned separately.
                        runs.extend(unread_divider_color_runs(
                            state,
                            self.visual_tokens.unread_divider_color,
                        ));
                        // Ambient awaiting-reply (question) indicator
                        // (hud-jip0k). Absent unless the most recently
                        // published output is a pending question.
                        runs.extend(awaiting_reply_color_runs(
                            state,
                            self.visual_tokens.awaiting_reply_color,
                        ));
                        // First-run empty-state treatment (hud-g1ena.6). Absent
                        // unless the retained transcript is empty on a portal that
                        // has connected; the never-connected case is handled by
                        // connecting_color_runs below (mutually exclusive gates).
                        runs.extend(empty_state_color_runs(
                            state,
                            self.visual_tokens.empty_state_color,
                        ));
                        // Connecting-state treatment (hud-g1ena.7,
                        // §Connecting State Distinction). Absent unless the portal
                        // is attached-but-never-connected with an empty transcript;
                        // its cool token is distinct from the amber stale marker so
                        // a starting-up portal never reads as failing.
                        runs.extend(connecting_color_runs(
                            state,
                            self.visual_tokens.connecting_marker_color,
                        ));
                        // Ambient agent-activity header cue + transcript-tail
                        // streaming cursor (hud-g1ena.5, §Agent Activity and
                        // Streaming Cue). Both derive from observed appends
                        // (`appended_at_wall_us` vs `now_wall_us`) via
                        // `agent_activity_active`; absent unless the tail is a
                        // fresh agent turn on a live expanded portal, so they
                        // quiesce promptly and suppress under redaction. Header +
                        // cursor take distinct tokens so a profile can style them
                        // apart. Kept in the HEADER + latest-turn-cursor region.
                        runs.extend(activity_cue_color_runs(
                            state,
                            self.visual_tokens.activity_cue_color,
                            now_wall_us,
                        ));
                        runs.extend(streaming_cursor_color_runs(
                            state,
                            self.visual_tokens.streaming_cursor_color,
                            now_wall_us,
                            content_len,
                        ));
                        // Ambient viewer-turn delivery-acknowledgement cue
                        // (hud-g1ena.1, §Viewer Turn Delivery Acknowledgement). The
                        // token-resolved class color (in-flight / delivered /
                        // failed) for the viewer's latest echoed reply, derived from
                        // the runtime's already-tracked delivery state — no adapter
                        // round trip, no seen-state disclosure. Absent unless a cue
                        // class is active, so it suppresses under redaction with the
                        // transcript.
                        runs.extend(delivery_cue_color_runs(state, &self.visual_tokens));
                        // Ambient per-turn arrival timestamps (hud-g1ena.4,
                        // §Ambient Per-Turn Timestamps). Secondary/subordinate
                        // token color for the `HH:MM` stamps that
                        // `visible_transcript_markdown_with` prefixes onto turns;
                        // absent unless the profile enables a non-Off granularity
                        // on an Expanded portal with a non-empty transcript, so it
                        // suppresses under redaction with the transcript itself.
                        runs.extend(timestamp_color_runs(
                            state,
                            self.visual_tokens.timestamp_color,
                            self.visual_tokens.timestamp_granularity,
                        ));
                        // Per-turn role attribution (§Conversational Turn Model
                        // and Per-Turn Role Attribution, hud-26869): a REAL span
                        // over each non-assistant agent-side turn's text
                        // (tool/status/error/other), token-resolved from
                        // `transcript_system_color` — the same `color_runs` span
                        // mechanism as adapter ANSI runs, NOT a byte-0 sentinel.
                        // Assistant turns emit no run and inherit the node's base
                        // transcript color, so the model's own prose reads
                        // distinctly from tool/system scaffolding. `turn_spans` is
                        // empty under redaction (transcript emptied upstream) and
                        // in the collapsed/empty presentations, so attribution
                        // suppresses with the transcript.
                        runs.extend(turn_attribution_color_runs(
                            &turn_spans,
                            self.visual_tokens.transcript_system_color,
                            content_len,
                        ));
                        runs
                    },
                    // Transcript panes use Ellipsis to engage the TruncationCache
                    // contract (word-boundary ellipsis, tail-anchored follow-tail).
                    // This prevents partially-clipped final glyphs under
                    // non-line-multiple viewports.
                    overflow: proto::TextOverflowProto::Ellipsis as i32,
                },
            )),
            // The transcript is published as a single node whose per-turn
            // structure (dividers) and role attribution (per-turn color-run spans,
            // hud-26869) ride the one coalescible `SetTileRoot` update. Splitting
            // the transcript into one scene node PER TURN via inline
            // `NodeProto.children` is deferred: PR #1149 (hud-ga4md) ships atomic
            // subtree materialization, but the compositor has no vertical FLOW
            // layout — every node paints at its own explicit `bounds.y` and the
            // projection layer cannot measure wrapped turn heights, so per-turn
            // child nodes would overlap. A true node split is gated on a compositor
            // vertical-flow layout capability (follow-up) and on promotion. Empty =
            // flat (hud-ga4md); keeping it empty preserves StateStream coalescing
            // (no per-node `AddNode` fan-out, hud-mzk74).
            children: vec![],
            // Single blob node → Absolute layout (UNSPECIFIED, byte-compatible).
            // A future per-turn split would set VERTICAL_FLOW here (hud-yfj8u).
            layout: proto::NodeLayoutProto::Unspecified as i32,
        }
    }

    fn bounds_for_state(&self, state: &ProjectedPortalState) -> proto::Rect {
        match state.presentation {
            ProjectedPortalPresentation::Expanded => {
                // hud-v4k1h: an Expanded portal that has been resized renders its
                // body + composer at the durable resized size, not the fixed
                // config size. The runtime grows the tile bounds locally on
                // resize; without honoring that here, the body stayed config-
                // sized and the grown tile area showed an empty "shadow-body".
                // Collapsed always uses compact_bounds (resize is an
                // Expanded-only affordance), so the override is Expanded-scoped.
                if let Some(resized) = state.resized_bounds {
                    if resized.width_px > 0 && resized.height_px > 0 {
                        // Preserve the resized ORIGIN as well as the size: a
                        // left/top edge pointer-drag keeps the opposite edge
                        // stationary (DeviceResizeState::compute_rect), so the
                        // snapshot carries a shifted x/y. bounds_for_state feeds
                        // the per-render PublishToTile bounds, so returning the
                        // static config origin here would snap the tile back to
                        // the configured position on the next publish while
                        // keeping the new size. local_bounds_for_state still
                        // zeroes x/y for the tile-local node bounds.
                        return proto::Rect {
                            x: resized.x_px as f32,
                            y: resized.y_px as f32,
                            width: resized.width_px as f32,
                            height: resized.height_px as f32,
                        };
                    }
                }
                self.config.expanded_bounds
            }
            ProjectedPortalPresentation::Collapsed => self.config.compact_bounds,
        }
    }

    fn local_bounds_for_state(&self, state: &ProjectedPortalState) -> proto::Rect {
        let source = self.bounds_for_state(state);
        proto::Rect {
            x: 0.0,
            y: 0.0,
            width: source.width,
            height: source.height,
        }
    }

    fn command(
        &self,
        kind: ResidentGrpcPortalCommandKind,
        sequence: u64,
        timestamp_wall_us: u64,
        payload: session_proto::client_message::Payload,
        started: Instant,
    ) -> ResidentGrpcPortalCommand {
        ResidentGrpcPortalCommand {
            kind,
            message: session_proto::ClientMessage {
                sequence,
                timestamp_wall_us,
                payload: Some(payload),
            },
            budget: sample_budget(started, RESIDENT_PORTAL_UPDATE_BUILD_BUDGET_US),
        }
    }
}

/// Render the portal body markdown with ambient per-turn timestamps OFF.
///
/// Thin default-carrying shim over [`portal_markdown_with`], used only by the unit
/// tests that predate timestamps and assert the no-stamp rendering. The production
/// render path ([`ResidentGrpcPortalAdapter::portal_text_node`] /
/// [`ResidentGrpcPortalAdapter::render_portal_markdown`]) always passes the
/// adapter's profile-resolved [`TimestampGranularity`]; hence `#[cfg(test)]`.
#[cfg(test)]
fn portal_markdown(
    state: &ProjectedPortalState,
    composer_display: Option<&ComposerDisplayState>,
    now_wall_us: u64,
) -> String {
    portal_markdown_with(
        state,
        composer_display,
        now_wall_us,
        TimestampGranularity::Off,
    )
}

/// Render the portal body markdown, presenting ambient per-turn arrival
/// timestamps per the profile-resolved `timestamps` granularity
/// (portal-chat-grade-affordances §Ambient Per-Turn Timestamps, hud-g1ena.4).
///
/// Thin wrapper over [`portal_markdown_with_spans`] for callers that need only
/// the rendered content (e.g. the drain-record markdown); the render path uses
/// the spans variant to attribute turns by role.
fn portal_markdown_with(
    state: &ProjectedPortalState,
    composer_display: Option<&ComposerDisplayState>,
    now_wall_us: u64,
    timestamps: TimestampGranularity,
) -> String {
    portal_markdown_with_spans(state, composer_display, now_wall_us, timestamps).0
}

/// Render only the agent-authored OUTPUT history for the promoted native split
/// surface. Identity/status chrome is materialized by the header subtree and
/// viewer turns are materialized independently in the INPUT pane, so neither is
/// allowed to enter this byte stream.
fn portal_output_markdown_with_spans(
    state: &ProjectedPortalState,
    now_wall_us: u64,
    timestamps: TimestampGranularity,
) -> (String, Vec<TranscriptTurnSpan>) {
    if state.visible_transcript.is_empty() {
        return (empty_portal_markdown(state).to_string(), Vec::new());
    }

    let (mut content, mut spans) = visible_transcript_markdown_spans(
        &state.visible_transcript,
        unread_divider_boundary(state),
        timestamps,
    );
    if agent_activity_active(state, now_wall_us) {
        content.push_str(PORTAL_STREAMING_CURSOR_GLYPH);
    }
    if content.len() <= MAX_PORTAL_MARKDOWN_BYTES {
        return (content, spans);
    }

    // The authority already selected the newest-fit transcript window. Formatting
    // (turn separators, timestamps, unread divider, streaming cursor) can add a
    // few bytes beyond that bound, so preserve the same tail-anchored policy here:
    // evict the oldest formatted head rather than dropping the newest turn.
    let mut cut = content.len() - MAX_PORTAL_MARKDOWN_BYTES;
    while !content.is_char_boundary(cut) {
        cut += 1;
    }
    content.drain(..cut);
    for span in &mut spans {
        span.start = span.start.saturating_sub(cut);
        span.end = span.end.saturating_sub(cut);
    }
    spans.retain(|span| span.start < span.end);
    (content, spans)
}

/// Render the portal body markdown and, for the expanded OUTPUT transcript,
/// the per-turn text spans (absolute byte offsets into the returned content)
/// tagged by `OutputKind`, so [`ResidentGrpcPortalAdapter::portal_node`] can emit
/// per-turn role-attribution color runs (text-stream-portals §Conversational
/// Turn Model and Per-Turn Role Attribution, hud-26869). Spans are populated
/// only for the expanded, non-empty transcript body; the collapsed and empty
/// presentations return none. The `String` half is byte-identical to the
/// pre-spans `portal_markdown_with` output.
fn portal_markdown_with_spans(
    state: &ProjectedPortalState,
    composer_display: Option<&ComposerDisplayState>,
    now_wall_us: u64,
    timestamps: TimestampGranularity,
) -> (String, Vec<TranscriptTurnSpan>) {
    let mut result = String::new();
    // At most one span per visible transcript unit (the OUTPUT window shifts each
    // turn's body spans into content coordinates); pre-size to that bound.
    let mut turn_spans: Vec<TranscriptTurnSpan> =
        Vec::with_capacity(state.visible_transcript.len());
    let title = state.display_name.as_deref().unwrap_or("Projected session");
    push_line(&mut result, &format!("**{title}**"));
    push_line(
        &mut result,
        &format!(
            "`{}` · {:?} · {:?}",
            state.portal_id, state.presentation, state.attention
        ),
    );
    // Ambient unread-output-count indicator (hud-meqet). `unread_output_count`
    // is `None` when redacted (`reveal_unread` policy gate) and `Some(0)` when
    // there is nothing unread — both render nothing. This is a presence engine,
    // not a notification engine: a quiet count near the header, never a loud
    // badge (CLAUDE.md doctrine).
    if let Some(line) = unread_indicator_line(state.unread_output_count) {
        push_line(&mut result, &line);
    }
    // Ambient awaiting-reply (question) indicator (hud-jip0k). Present only
    // when the most recently published visible transcript unit is a question
    // awaiting a viewer reply (`expects_reply == true`). A redacted viewer
    // naturally gets nothing here — `visible_transcript` is already empty
    // under redaction — and a viewer's own echoed reply becomes the new last
    // unit (with `expects_reply == false`), which is exactly what clears the
    // cue once answered. Minimal and text-visible so the signal survives
    // environments that don't inspect color_runs; kept quiet (no `!`/⚠) to
    // stay ambient per presence-engine doctrine.
    if awaiting_reply(state) {
        push_line(&mut result, "? awaiting reply");
    }
    // Ambient viewer-turn delivery-acknowledgement cue (hud-g1ena.1, §Viewer Turn
    // Delivery Acknowledgement). Reflects the runtime's already-tracked delivery
    // state for the viewer's most recent echoed reply so the viewer can see whether
    // it reached the owning adapter without asking. Three quiet classes (in-flight /
    // delivered / failed) — no adapter round trip, no seen-state disclosure. Kept
    // text-visible so the signal survives environments that don't inspect
    // color_runs; the token-driven class color rides alongside via
    // `delivery_cue_color_runs`. Ambient by design (no `!`/⚠): a delivery
    // transition is not an attention event and a failed cue stays on the turn. `None`
    // under redaction (the authority withholds the state), so it suppresses with the
    // transcript.
    if let Some(class) = delivery_cue_class(state) {
        push_line(&mut result, class.line());
    }
    // Ambient agent-activity header cue (hud-g1ena.5, §Agent Activity and
    // Streaming Cue). A compact typing-style indicator shown while the owning
    // adapter is actively appending — derived purely from observed appends via
    // `agent_activity_active` (the tail is a fresh, non-question agent turn on a
    // live expanded portal). Mutually exclusive with the awaiting-reply cue above
    // (that requires `expects_reply`; this requires `!expects_reply`). Text-
    // visible so the signal survives environments that don't inspect color_runs;
    // the token-driven `activity_cue_color` rides alongside via
    // `activity_cue_color_runs`. Kept quiet (no `!`/⚠) — continuous streaming
    // must never escalate attention (doctrine: not a notification engine).
    if agent_activity_active(state, now_wall_us) {
        push_line(&mut result, PORTAL_ACTIVITY_MARKER_LINE);
    }
    // §2: content-free disconnect/stale marker. Emitted from the
    // redaction-independent `connection_degraded` flag and BEFORE the
    // redaction-gated lifecycle line, so it remains present (and reveals nothing)
    // even for a restricted viewer — like the scroll-position indicator. It
    // carries connection state only, never identity or transcript content.
    if is_connection_degraded(state) {
        push_line(&mut result, PORTAL_DISCONNECT_MARKER_LINE);
    }
    if let Some(lifecycle) = state.lifecycle_state {
        // Ambient, content-free glyph + exact spelling. The glyph groups states
        // for at-a-glance scanning (active / ready / attention / inactive); the
        // accent color is carried separately via `lifecycle_marker_color_runs`.
        push_line(
            &mut result,
            &format!("status: {} {lifecycle:?}", lifecycle_glyph(lifecycle)),
        );
    }
    if let Some(status_text) = state.status_text.as_deref() {
        push_line(&mut result, &format!("note: {status_text}"));
    }

    // hud-c64bn: byte offset marking the start of the TAIL-CRITICAL content —
    // the input-status lines and (Expanded only) the composer status line —
    // that MUST survive the `MAX_PORTAL_MARKDOWN_BYTES` wire truncation
    // below. Every arm of the match below sets this before the function
    // reads it; the compiler enforces that definite-assignment property (an
    // unhandled future arm is a compile error here, not a silent fallback).
    let tail_start;

    match state.presentation {
        ProjectedPortalPresentation::Expanded => {
            push_line(&mut result, "");
            // §First-Run Empty Portal Treatment (hud-g1ena.6): an empty retained
            // transcript renders the friendly, token-styled empty/connecting body
            // instead of the literal `<empty projection stream>`. The first
            // appended unit replaces it (this branch is empty-only).
            if state.visible_transcript.is_empty() {
                push_line(&mut result, empty_portal_markdown(state));
            } else {
                // Transcript body carries the in-transcript unread divider
                // (hud-g1ena.2, via `unread_divider_boundary`) plus, when the agent
                // is actively appending, an ambient streaming cursor at the tail of
                // the latest turn (hud-g1ena.5). The cursor is appended AFTER
                // `visible_transcript_markdown` (not inside it) so this stays clear
                // of the transcript-body region and the glyph reads as a
                // live-writing caret at the very tail. The token-driven
                // `streaming_cursor_color` rides alongside via
                // `streaming_cursor_color_runs`; `agent_activity_active` is the
                // single gate for both the glyph and the sentinel run.
                let (mut body, body_spans) = visible_transcript_markdown_spans(
                    &state.visible_transcript,
                    unread_divider_boundary(state),
                    timestamps,
                );
                if agent_activity_active(state, now_wall_us) {
                    body.push_str(PORTAL_STREAMING_CURSOR_GLYPH);
                }
                // Absolute offset of `body`'s first byte within the assembled
                // content: `push_line` prepends a '\n' when `result` is non-empty
                // (always true here — the header/title lines precede the
                // transcript), so the body begins one byte past the current tail.
                // Shift the per-turn text spans into content coordinates so
                // `portal_node` can attribute turns by role (hud-26869). The
                // streaming-cursor glyph is appended after the last turn's text,
                // so it never falls inside a turn span.
                let body_offset = result.len() + usize::from(!result.is_empty());
                push_line(&mut result, &body);
                turn_spans.extend(body_spans.into_iter().map(|s| TranscriptTurnSpan {
                    start: s.start + body_offset,
                    end: s.end + body_offset,
                    output_kind: s.output_kind,
                }));
            }
            // §Viewer Reply Echo (two-pane INPUT/OUTPUT split): render the INPUT
            // history — the viewer's own accepted replies — as a distinct band in
            // the input region, held SEPARATELY from the OUTPUT transcript above.
            // A viewer echo lands in `input_history` and never in
            // `visible_transcript`, so a submission never grows the OUTPUT
            // transcript nor moves its follow-tail scroll (the driver computes
            // follow-tail from `visible_transcript` only). The band reuses the
            // transcript's `---`-divider + ambient-timestamp treatment with NO
            // unread divider (viewer turns are never unread). Under redaction
            // `input_history` is emptied upstream, so the band + its label never
            // render for a restricted viewer. The promotion-era structured layout
            // gives the INPUT pane its own part node (PORTAL_PART_KIND_COMPOSER /
            // Phase-0 `input_scroll`, hud-ga4md).
            if !state.input_history.is_empty() {
                push_line(&mut result, "");
                push_line(&mut result, PORTAL_INPUT_HISTORY_LABEL);
                push_line(
                    &mut result,
                    &input_history_markdown(&state.input_history, timestamps),
                );
            }
            // hud-0e44r: the composer status line is rendered LAST — after the
            // INPUT-history band, not before it. §Viewer Reply Echo frames the
            // composer as top-anchored "above" the INPUT band, and #1142 matched
            // that framing by rendering it first; but this whole Expanded arm is
            // still ONE `TextMarkdownNode` (the legacy single-part portal path,
            // `node_gets_transcript_treatment` returns `true` whenever
            // `part_index` is `None`), so the ENTIRE composed blob gets
            // `TruncationViewport::TailAnchored` under overflow. Positioned above
            // the band, a long `input_history` pushed the composer line out of the
            // truncation window — the exact defect #1142 introduced. A
            // `TailAnchored` viewport keeps the TAIL of an overflowing blob
            // visible, so the trailing line is the one line guaranteed to survive
            // truncation: putting the composer status last makes it that line,
            // while the INPUT-history band (now the truncatable middle) trims
            // from its own top under overflow instead. This trades the exact
            // visual "composer above the band" framing for guaranteed visibility
            // until the composer gets its own independently-bounded part node
            // (hud-ga4md, blocked-by hud-26869); the live interactive composer
            // itself is unaffected either way — it is already a separate,
            // always-visible bottom-pinned overlay (hud-f6zfa) that never rode
            // this markdown blob.
            //
            // "Trailing line" protects against the COMPOSITOR's TailAnchored
            // viewport (keeps the tail under visual overflow) — but the WIRE-level
            // `MAX_PORTAL_MARKDOWN_BYTES` truncation below has the OPPOSITE
            // keep-policy (keeps the prefix), so trailing content is at HIGHER
            // risk there, not lower (hud-c64bn, found during hud-rnc4x review).
            // `tail_start` marks where the reservation below begins protecting
            // this section from that wire-level cap too.
            //
            // hud-ycurc: emit the input-status lines (pending HUD input count /
            // last input feedback) HERE, before the composer status, so the
            // composer status remains the trailing tail-anchored-safe line even
            // when those fields are populated.
            tail_start = result.len();
            push_input_status_lines(&mut result, state);
            push_line(&mut result, "");
            if is_connection_degraded(state) {
                // §2: clear live activity/typing/composer-ready signals on
                // disconnect — the surface must not imply an active stream. The
                // authority also forces `interaction_enabled = false` when
                // degraded; this is the redundant presentation-layer guard.
                push_line(&mut result, "composer: unavailable");
            } else if state.interaction_enabled {
                // Render composer region with draft text + caret (§4.1).
                // Local-first: no remote roundtrip — reads from adapter's cached
                // ComposerDisplayState which is updated by consume_draft_batch /
                // apply_draft_notification on every delivered notification.
                let composer_line = composer_line(composer_display, state.interaction_enabled);
                push_line(&mut result, &composer_line);
            } else {
                push_line(&mut result, "composer: unavailable");
            }
        }
        ProjectedPortalPresentation::Collapsed => {
            let preview = state
                .visible_transcript
                .last()
                .map(|unit| unit.output_text.as_str())
                .unwrap_or("compact projection affordance");
            push_line(&mut result, &clamp_one_line(preview, 160));
            // Collapsed has no composer status line, so the input-status lines
            // keep their trailing position after the compact preview (unchanged
            // from before hud-ycurc). In practice Collapsed content never nears
            // `MAX_PORTAL_MARKDOWN_BYTES` (the preview is clamped to 160 bytes
            // and the input-status lines are similarly short-bounded, so the
            // whole arm tops out at a few hundred bytes), but marking
            // `tail_start` here costs nothing and keeps the reservation below
            // uniform across both arms (hud-c64bn) rather than Expanded-only.
            tail_start = result.len();
            push_input_status_lines(&mut result, state);
        }
    }

    // hud-c64bn: reserve the tail-critical content's byte length from the
    // budget instead of truncating the whole assembled blob as one
    // prefix-kept unit — see `apply_tail_reserved_truncation`'s doc comment
    // for why.
    let result = apply_tail_reserved_truncation(
        result,
        tail_start,
        MAX_PORTAL_MARKDOWN_BYTES,
        &mut turn_spans,
    );

    (result, turn_spans)
}

/// Apply the tail-reservation byte-budget policy (hud-c64bn) to `result`:
/// bytes at or after `tail_start` are TAIL-CRITICAL content (composer status,
/// input-status lines) that MUST survive verbatim, so only the HEAD
/// (`result[..tail_start]`, the variable-length transcript/input-history
/// portion) is truncated to make room. `turn_spans` (byte offsets into the
/// head) are clamped/dropped to match.
///
/// This replaces truncating the whole assembled blob as one prefix-kept unit
/// via `truncate_utf8(result, max_bytes)` (which kept `text[..cut]`) — the
/// OPPOSITE keep-policy of the compositor's `TruncationViewport::TailAnchored`
/// that motivates the tail-critical content being positioned last in the
/// first place: under plain whole-blob prefix truncation, content placed
/// last was at HIGHER risk from this wire-level cap, not lower, exactly
/// inverting the guarantee `tail_start` is meant to provide.
///
/// No-op, and BYTE-IDENTICAL to the input, when `result.len() <= max_bytes`
/// (the overwhelmingly common case) — `clamp_utf8`'s own `<= max_bytes` early
/// return means this function's early return here preserves that exactly.
///
/// The tail is cloned out of `result` (not independently re-assembled) before
/// truncation, so it captures whatever the caller's `push_line`/
/// `push_input_status_lines` calls actually produced verbatim — including
/// their own conditional-newline-joining behavior — with no risk of a second,
/// independently-built "tail" string diverging in its own formatting.
///
/// `turn_spans`, which are byte offsets into the HEAD region only (computed
/// before any tail-critical content existed), are clamped and dropped against
/// `head_cut` — NOT against the final (post-splice) content length. The two
/// differ once the cap engages (`final_len == head_cut + tail.len() >
/// head_cut`), so clamping against the final length instead would let a span
/// whose bytes were cut from the head survive pointing at whatever tail bytes
/// now occupy that position — a real span-corruption bug this reservation
/// would otherwise introduce. (The downstream `turn_attribution_color_runs`
/// clamp against the caller's `content_len` stays as a harmless, now-
/// redundant second guard.)
///
/// Degenerate policy: if `result[tail_start..]` (the tail) alone is `>=
/// max_bytes`, `saturating_sub` floors the head budget at `0` — the head
/// becomes empty and every span is dropped — and the tail is returned intact
/// even though the total then exceeds `max_bytes`. This function never trims
/// the tail itself: at the real call site, the composer/feedback lines it
/// carries are already line-clamped at their construction sites
/// (`composer_feedback_line`'s Rejected-reason fallback goes through
/// `clamp_one_line(.., 160)`), so an over-budget tail is not reachable in
/// practice there; if it ever were, exceeding the budget slightly is
/// preferable to guessing which part of the tail-critical content to
/// sacrifice.
fn apply_tail_reserved_truncation(
    mut result: String,
    tail_start: usize,
    max_bytes: usize,
    turn_spans: &mut Vec<TranscriptTurnSpan>,
) -> String {
    if result.len() <= max_bytes {
        return result;
    }
    let tail = result[tail_start..].to_string();
    let head_budget = max_bytes.saturating_sub(tail.len());
    let head_cut = clamp_utf8(&result[..tail_start], head_budget).len();
    for span in turn_spans.iter_mut() {
        span.start = span.start.min(head_cut);
        span.end = span.end.min(head_cut);
    }
    turn_spans.retain(|span| span.start < span.end);
    result.truncate(head_cut);
    result.push_str(&tail);
    result
}

/// Emit the ambient input-status lines — the pending-HUD-input count and the
/// last composer-input feedback — when present.
///
/// hud-ycurc: these are pushed BEFORE the composer status line in the Expanded
/// arm so the composer status stays the trailing, tail-anchored-safe line
/// (hud-0e44r) even when these fields are populated. They previously trailed the
/// whole blob after the presentation match, displacing the composer status from
/// the guaranteed-visible tail. In the Collapsed arm (no composer status line)
/// they follow the compact preview.
fn push_input_status_lines(result: &mut String, state: &ProjectedPortalState) {
    if let Some(pending) = state.pending_input_count {
        push_line(result, &format!("pending HUD input: {pending}"));
    }
    if let Some(feedback) = &state.last_input_feedback {
        push_line(result, &composer_feedback_line(feedback));
    }
}

/// Human-legible one-line status for the last composer submission.
///
/// Replaces the old `Debug`-formatted enum (`last composer: Rejected`) with a
/// reason the viewer can act on. On rejection we map the machine error code to
/// a friendly sentence, falling back to the authority's `status_summary`
/// (already populated, e.g. "PROJECTION_HUD_UNAVAILABLE: portal input rejected")
/// when the code is absent (hud-phdkd).
fn composer_feedback_line(feedback: &PortalInputFeedback) -> String {
    match feedback.feedback_state {
        PortalInputFeedbackState::Accepted => "✓ sent".to_string(),
        PortalInputFeedbackState::Rejected => {
            let reason = match feedback.error_code {
                Some(ProjectionErrorCode::ProjectionHudUnavailable) => {
                    "the agent is disconnected".to_string()
                }
                Some(ProjectionErrorCode::ProjectionInputQueueFull) => {
                    "too many replies waiting — try again shortly".to_string()
                }
                Some(ProjectionErrorCode::ProjectionInputTooLarge) => {
                    "message too long".to_string()
                }
                Some(ProjectionErrorCode::ProjectionRateLimited) => {
                    "sending too fast — try again shortly".to_string()
                }
                Some(
                    ProjectionErrorCode::ProjectionTokenExpired
                    | ProjectionErrorCode::ProjectionUnauthorized,
                ) => "the projection session expired".to_string(),
                _ => clamp_one_line(&feedback.status_summary, 160),
            };
            format!("⚠ not sent — {reason}")
        }
    }
}

/// Build the composer status line for the expanded portal node.
///
/// This line is a **content-free status affordance**, not the draft surface. The
/// live draft text + caret are rendered exclusively by the compositor's
/// bottom-pinned input strip, which is the single source of truth for the draft
/// (hud-2zsbf: `Compositor::composer_input_box` confines the echo to a one-line
/// strip at the portal bottom). Embedding the draft here as well produced a
/// SECOND copy in the transcript flow — mid-portal, right after the transcript —
/// at a different Y than the bottom strip, reading live as a double / misaligned
/// composer (hud-f6zfa). So this line never carries the draft glyphs; it only
/// reflects composer availability.
///
/// States:
/// - `interaction_enabled == false` → `composer: unavailable`.
/// - no active draft (`composer_display` is None) → `composer: ready`.
/// - active draft → `composer: composing` (the draft itself is in the bottom
///   strip). When the draft is at capacity the line becomes
///   `composer: [!] at capacity`, keeping the at-capacity state text-visible for
///   environments that do not inspect color. The at-capacity HUE is painted
///   directly on the draft glyphs by the compositor overlay
///   (`composer_draft_base_color`, hud-9gyao), not carried as a color run on
///   this markdown node.
///
/// The native INPUT pane keeps this status and viewer history in its own bounds;
/// the compositor overlay remains the single source of truth for draft glyphs.
fn composer_line(
    composer_display: Option<&ComposerDisplayState>,
    interaction_enabled: bool,
) -> String {
    if !interaction_enabled {
        return "composer: unavailable".to_string();
    }
    let Some(display) = composer_display else {
        return "composer: ready".to_string();
    };

    if display.at_capacity {
        // Text-visible at-capacity marker WITHOUT the draft text (the bottom
        // strip owns the draft). The at-capacity hue is painted on the draft
        // glyphs by the compositor overlay (`composer_draft_base_color`).
        "composer: [!] at capacity".to_string()
    } else {
        "composer: composing".to_string()
    }
}

/// Build a `TextColorRunProto` for the disconnect/stale marker.
///
/// When the portal is connection-degraded, emits a single zero-length sentinel
/// run (`[0..0]`) carrying `stale_marker_color`: the run has no pixel
/// coverage; its presence + color is the machine-readable signal that the
/// token-driven stale marker is active. Precise per-line coloring is deferred to
/// the promotion-era structured layout. Returns an empty vec when not degraded.
///
/// §2: the marker color is token-driven — no literal color in the render path.
fn stale_marker_color_runs(
    state: &ProjectedPortalState,
    stale_marker_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    if !is_connection_degraded(state) {
        return Vec::new();
    }
    vec![proto::TextColorRunProto {
        start_byte: 0,
        end_byte: 0,
        color: Some(stale_marker_color),
    }]
}

/// Ambient one-line unread-output-count affordance, e.g. `"3 unread"`.
///
/// Returns `None` when the count is redacted (`unread_output_count == None`,
/// gated by the authority's `reveal_unread` policy) or when there is nothing
/// unread (`Some(0)`) — a presence engine renders nothing rather than a "0
/// unread" line. Never a loud/alarming marker: just a quiet count, styled
/// separately via `unread_indicator_color_runs` (§6.1: token-resolved, never
/// hardcoded).
fn unread_indicator_line(unread_output_count: Option<usize>) -> Option<String> {
    match unread_output_count {
        Some(count) if count > 0 => Some(format!("{count} unread")),
        _ => None,
    }
}

/// Build a `TextColorRunProto` for the ambient unread-output-count indicator.
///
/// Mirrors `stale_marker_color_runs`: a zero-length
/// sentinel run (`[0..0]`) carrying the token-resolved `unread_indicator_color`
/// so the token drives the signal without any literal color in the render path
/// (§6.1). Empty when there is nothing unread or the count is redacted —
/// matching `unread_indicator_line`'s gating exactly.
fn unread_indicator_color_runs(
    state: &ProjectedPortalState,
    unread_indicator_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    match state.unread_output_count {
        Some(count) if count > 0 => vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(unread_indicator_color),
        }],
        _ => Vec::new(),
    }
}

/// Index into `visible_transcript` **before** which the in-transcript unread
/// divider is inserted: the oldest retained *unseen agent-authored* turn
/// (hud-g1ena.2, §Unread Divider and Ambient Unread Count).
///
/// Returns `None` when there is nothing unread visible to this viewer
/// (`visible_unread_output_count` is `None` under the `reveal_unread` redaction
/// gate, or `Some(0)`), or when the retained window holds no agent-authored unit —
/// in those cases no divider renders, which is exactly how the divider **clears
/// locally when the viewer views the tail**: the authority resets the count to `0`
/// on tail view, so this yields `None` with no adapter round trip.
///
/// The count driving placement is `visible_unread_output_count` — the number of
/// unread units that survive this viewer's clearance filter — **not** the
/// aggregate `unread_output_count`. A higher-classification unread turn filtered
/// out of `visible_transcript` must not push the divider onto an already-seen
/// visible turn; the aggregate would over-count the units below the divider and do
/// exactly that. The ambient count near the header still shows the aggregate,
/// which MAY legitimately exceed the units below the divider.
///
/// The boundary is found by walking the retained window from the tail and
/// counting only agent-authored units — **echoed viewer turns are never unread**
/// per the Viewer Reply Echo requirement, so `OutputKind::Viewer` units are
/// skipped. When the (visible) unread count exceeds the agent-authored units still
/// retained in the bounded window (older unread turns coalesced or scrolled out),
/// the walk exhausts and the divider sits at the **oldest retained unseen unit** —
/// the first retained agent-authored turn — matching the spec's "the count MAY
/// exceed the units visibly below the divider".
fn unread_divider_boundary(state: &ProjectedPortalState) -> Option<usize> {
    let count = match state.visible_unread_output_count {
        Some(count) if count > 0 => count,
        _ => return None,
    };
    let mut agent_seen = 0usize;
    let mut boundary = None;
    for (index, unit) in state.visible_transcript.iter().enumerate().rev() {
        if unit.output_kind != OutputKind::Viewer {
            agent_seen += 1;
            boundary = Some(index);
            if agent_seen == count {
                break;
            }
        }
    }
    boundary
}

/// Build the token-styled sentinel color run for the in-transcript unread divider
/// (hud-g1ena.2, §Unread Divider and Ambient Unread Count).
///
/// Mirrors the other Phase-1 sentinels (`unread_indicator_color_runs`,
/// `stale_marker_color_runs`): a zero-length run (`[0..0]`) carrying the
/// token-resolved `unread_divider_color` so the visual token drives the divider
/// treatment without any literal color in the render path (§6.1). Emitted exactly
/// when the divider line is actually rendered — an Expanded portal with a
/// non-empty retained transcript whose retained window contains an unread
/// agent-authored turn (`unread_divider_boundary` is `Some`). Empty otherwise, so
/// it disappears the moment the count clears on tail view.
fn unread_divider_color_runs(
    state: &ProjectedPortalState,
    unread_divider_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && !state.visible_transcript.is_empty()
        && unread_divider_boundary(state).is_some()
    {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(unread_divider_color),
        }]
    } else {
        Vec::new()
    }
}

/// `true` when the most recently appended visible transcript unit is a
/// question awaiting a viewer reply (`expects_reply == true`, hud-jip0k).
///
/// Gating on `visible_transcript.last()` gets two things for free:
/// - **Redaction**: a restricted viewer's `visible_transcript` is already
///   emptied by the authority (`projected_portal_state`), so this returns
///   `false` without any separate redaction check.
/// - **Answered-question clearing**: a viewer's own echoed reply
///   (`OutputKind::Viewer`) is appended with `expects_reply == false`
///   (`submit_portal_input`), so once the viewer responds it becomes the new
///   last unit and the cue clears — no separate "answered" bookkeeping needed.
fn awaiting_reply(state: &ProjectedPortalState) -> bool {
    state
        .visible_transcript
        .last()
        .is_some_and(|unit| unit.expects_reply)
}

/// Build a `TextColorRunProto` for the ambient awaiting-reply (question)
/// indicator.
///
/// Mirrors `unread_indicator_color_runs`/`stale_marker_color_runs`: a
/// zero-length sentinel run (`[0..0]`) carrying the token-resolved
/// `awaiting_reply_color` so the token drives the signal without any literal
/// color in the render path (§6.1). Empty when nothing is awaiting reply.
fn awaiting_reply_color_runs(
    state: &ProjectedPortalState,
    awaiting_reply_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    if awaiting_reply(state) {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(awaiting_reply_color),
        }]
    } else {
        Vec::new()
    }
}

/// The three viewer-turn delivery-acknowledgement presentation classes
/// (hud-g1ena.1, portal-chat-grade-affordances §Viewer Turn Delivery
/// Acknowledgement). The six `InputDeliveryState` variants the runtime tracks fold
/// into exactly these three classes for presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryCueClass {
    /// Submitted but not yet taken by the adapter (`Pending` / `Deferred`).
    InFlight,
    /// Adapter has taken (or handled) the reply (`Delivered` / `Handled`).
    Delivered,
    /// Rejected by the adapter or expired before delivery (`Rejected` / `Expired`).
    Failed,
}

impl DeliveryCueClass {
    /// Fold an `InputDeliveryState` into its presentation class.
    fn from_delivery_state(state: InputDeliveryState) -> Self {
        match state {
            InputDeliveryState::Pending | InputDeliveryState::Deferred => Self::InFlight,
            InputDeliveryState::Delivered | InputDeliveryState::Handled => Self::Delivered,
            InputDeliveryState::Rejected | InputDeliveryState::Expired => Self::Failed,
        }
    }

    /// The ambient text-visible cue line for this class.
    fn line(self) -> &'static str {
        match self {
            Self::InFlight => PORTAL_DELIVERY_INFLIGHT_LINE,
            Self::Delivered => PORTAL_DELIVERY_DELIVERED_LINE,
            Self::Failed => PORTAL_DELIVERY_FAILED_LINE,
        }
    }

    /// The token-resolved accent color for this class, pulled from the visual
    /// token set — never a literal color (§6.1).
    fn color(self, tokens: &PortalVisualTokens) -> proto::Rgba {
        match self {
            Self::InFlight => tokens.delivery_inflight_color,
            Self::Delivered => tokens.delivery_delivered_color,
            Self::Failed => tokens.delivery_failed_color,
        }
    }
}

/// The viewer-turn delivery-acknowledgement cue class to present, or `None` when
/// no cue applies (hud-g1ena.1, §Viewer Turn Delivery Acknowledgement).
///
/// Derived ENTIRELY from `state.latest_viewer_delivery_state` — the delivery state
/// of the viewer's most recent submitted reply, which the authority reads from its
/// existing runtime-owned `pending_input` bookkeeping. Rendering the cue therefore
/// requires no adapter round trip and discloses no viewer read/seen state. `None`
/// falls out for free under redaction: the authority leaves
/// `latest_viewer_delivery_state == None` whenever the transcript is not visible to
/// this viewer, so the cue redacts together with the echoed turn it annotates.
fn delivery_cue_class(state: &ProjectedPortalState) -> Option<DeliveryCueClass> {
    state
        .latest_viewer_delivery_state
        .map(DeliveryCueClass::from_delivery_state)
}

/// Build the token-styled sentinel color run for the viewer-turn delivery cue
/// (hud-g1ena.1, §Viewer Turn Delivery Acknowledgement).
///
/// Mirrors the other Phase-1 sentinels (`awaiting_reply_color_runs`,
/// `unread_indicator_color_runs`): a single zero-length run (`[0..0]`) carrying the
/// token-resolved color for the active class so the visual token drives the cue
/// without any literal color in the render path (§6.1). Emitted exactly when a cue
/// line is rendered — i.e. when `delivery_cue_class` is `Some` — and empty
/// otherwise, so it disappears with the transcript under redaction and when there
/// is no tracked viewer submission.
fn delivery_cue_color_runs(
    state: &ProjectedPortalState,
    tokens: &PortalVisualTokens,
) -> Vec<proto::TextColorRunProto> {
    match delivery_cue_class(state) {
        Some(class) => vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(class.color(tokens)),
        }],
        None => Vec::new(),
    }
}

/// Map a published `ProjectionLifecycleState` onto its ambient affordance accent.
///
/// The seven contract variants collapse into four viewer-facing semantic groups,
/// each with its own token-resolved accent (no literal color here — §6.1):
///
/// | Lifecycle state | Group | Accent token |
/// |---|---|---|
/// | `Active` | actively working | `lifecycle_active_color` |
/// | `Attached` | attached / ready | `lifecycle_attached_color` |
/// | `Degraded`, `HudUnavailable` | needs attention | `lifecycle_attention_color` |
/// | `Detached`, `CleanupPending`, `Expired` | winding down / gone | `lifecycle_inactive_color` |
///
/// The grouping keeps the palette tasteful and ambient (the cooperative-projection
/// and text-stream-portal specs forbid self-escalating interruption class); the
/// exact lifecycle spelling still rides the redaction-gated `status:` text line.
/// Ambient glyph for the lifecycle status line, grouped to match the accent
/// categories in [`lifecycle_accent_color`]. Content-free: conveys session
/// state only, never identity or transcript content. The glyphs are deliberately
/// quiet (no `!`/⚠) so the affordance stays ambient and never self-escalates
/// interruption class (text-stream-portals / cooperative-hud-projection specs).
fn lifecycle_glyph(lifecycle: ProjectionLifecycleState) -> char {
    match lifecycle {
        ProjectionLifecycleState::Active => '◆',
        ProjectionLifecycleState::Attached => '◇',
        ProjectionLifecycleState::Degraded | ProjectionLifecycleState::HudUnavailable => '◈',
        ProjectionLifecycleState::Detached
        | ProjectionLifecycleState::CleanupPending
        | ProjectionLifecycleState::Expired => '○',
    }
}

fn lifecycle_accent_color(
    lifecycle: ProjectionLifecycleState,
    tokens: &PortalVisualTokens,
) -> proto::Rgba {
    match lifecycle {
        ProjectionLifecycleState::Active => tokens.lifecycle_active_color,
        ProjectionLifecycleState::Attached => tokens.lifecycle_attached_color,
        ProjectionLifecycleState::Degraded | ProjectionLifecycleState::HudUnavailable => {
            tokens.lifecycle_attention_color
        }
        ProjectionLifecycleState::Detached
        | ProjectionLifecycleState::CleanupPending
        | ProjectionLifecycleState::Expired => tokens.lifecycle_inactive_color,
    }
}

/// Build the lifecycle-affordance color run for the portal node.
///
/// When the viewer is permitted to see lifecycle state (`state.lifecycle_state`
/// is `Some` — the authority sets it to `None` under redaction), emits a single
/// zero-length sentinel run (`[0..0]`) carrying the token-resolved accent for the
/// current lifecycle group. This mirrors the Phase-1 `stale_marker_color_runs`
/// mechanism: the run has no pixel coverage (a non-empty
/// run would suppress Markdown stripping for the whole single-node portal); its
/// presence + color is the machine-readable, token-driven signal that the viewer
/// affordance is active, while the text-visible `status:` line carries the exact
/// spelling. Precise per-line coloring is deferred to the promotion-era
/// structured multi-node layout.
///
/// Returns an empty vec when lifecycle state is redacted/absent — so a restricted
/// viewer gets no lifecycle affordance, exactly like the redaction-gated
/// `status:` line.
fn lifecycle_marker_color_runs(
    state: &ProjectedPortalState,
    tokens: &PortalVisualTokens,
) -> Vec<proto::TextColorRunProto> {
    let Some(lifecycle) = state.lifecycle_state else {
        return Vec::new();
    };
    vec![proto::TextColorRunProto {
        start_byte: 0,
        end_byte: 0,
        color: Some(lifecycle_accent_color(lifecycle, tokens)),
    }]
}

/// Render the friendly, token-styled empty/first-run portal body (hud-g1ena.6,
/// §First-Run Empty Portal Treatment) shown when the retained transcript window
/// is empty. Replaces the literal `<empty projection stream>` placeholder.
///
/// Three cases, in precedence order:
/// 1. **Connecting takes precedence** (§Connecting State Distinction): an
///    attached-but-never-connected portal (`!has_ever_connected`) shows the
///    distinct connecting line (`PORTAL_CONNECTING_LINE`) — never the "ready"
///    invite — so a starting-up portal never reads as a ready-and-idle empty
///    state. Its distinct pending glyph + token-resolved `connecting_marker_color`
///    (via `connecting_color_runs`) keep it visually distinct from the degraded
///    treatment, so a starting-up portal never reads as failing (hud-g1ena.7).
///    Connecting is content-free, so it also precedes the redacted placeholder.
/// 2. **Redacted**: a restricted viewer's `visible_transcript` is emptied
///    upstream, so this path is reached for them too. Under redaction identity
///    and inviting copy are suppressed — only a content-free placeholder shows.
/// 3. **Connected + empty**: the inviting ready line. Identity is carried by the
///    portal header (`**title**`), so this stays a single, uncluttered line.
///
/// Yields immediately to real content: the caller only renders this when
/// `visible_transcript.is_empty()`, so the first appended unit replaces it.
fn empty_portal_markdown(state: &ProjectedPortalState) -> &'static str {
    if !state.has_ever_connected {
        return PORTAL_CONNECTING_LINE;
    }
    if state.redacted {
        return PORTAL_EMPTY_REDACTED_LINE;
    }
    PORTAL_EMPTY_READY_LINE
}

/// Build the token-styled sentinel color run for the first-run empty-state body.
///
/// Mirrors the other Phase-1 sentinels (`stale_marker_color_runs`,
/// `unread_indicator_color_runs`): a zero-length run (`[0..0]`) carrying the
/// token-resolved `empty_state_color` so the visual token drives the treatment
/// without any literal color in the render path (§6.1). Emitted only when the
/// empty-state body is actually rendered — an Expanded portal with an empty
/// retained transcript that has connected. The connecting case
/// (`!has_ever_connected`) is intentionally excluded: it carries its own distinct
/// token via `connecting_color_runs` (hud-g1ena.7).
fn empty_state_color_runs(
    state: &ProjectedPortalState,
    empty_state_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && state.visible_transcript.is_empty()
        && state.has_ever_connected
    {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(empty_state_color),
        }]
    } else {
        Vec::new()
    }
}

/// Build the token-styled sentinel color run for the connecting-state body
/// (portal-chat-grade-affordances §Connecting State Distinction, hud-g1ena.7).
///
/// The mirror image of `empty_state_color_runs`: emitted exactly when the
/// connecting line (`PORTAL_CONNECTING_LINE`) is rendered — an Expanded portal
/// whose retained transcript is empty AND that has never connected
/// (`!has_ever_connected`). Carries the token-resolved `connecting_marker_color`
/// as a zero-length sentinel run so the connecting hue is token-driven, never a
/// literal in the render path (§6.1). Because this fires precisely when
/// `empty_state_color_runs` does NOT (the `has_ever_connected` gate is inverted),
/// the two treatments are mutually exclusive — a portal is either connecting or
/// empty-ready, never both.
///
/// Redaction-independent, like the connecting line itself and the stale marker:
/// the connecting hue reveals only connection state, no identity or content, so
/// it is emitted even for a restricted viewer.
fn connecting_color_runs(
    state: &ProjectedPortalState,
    connecting_marker_color: proto::Rgba,
) -> Vec<proto::TextColorRunProto> {
    if state.presentation == ProjectedPortalPresentation::Expanded
        && state.visible_transcript.is_empty()
        && !state.has_ever_connected
    {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(connecting_marker_color),
        }]
    } else {
        Vec::new()
    }
}

/// Build the token-styled sentinel color run for the ambient header activity cue
/// (hud-g1ena.5, §Agent Activity and Streaming Cue).
///
/// Mirrors the other Phase-1 sentinels (`stale_marker_color_runs`,
/// `awaiting_reply_color_runs`): a zero-length run (`[0..0]`) carrying the
/// token-resolved `activity_cue_color` so the token drives the header cue without
/// any literal color in the render path (§6.1). Emitted exactly when
/// [`agent_activity_active`] holds — the agent is actively appending on a live
/// expanded portal — matching the `PORTAL_ACTIVITY_MARKER_LINE` header text so
/// the text-visible and machine-readable signals agree. Absent (empty vec)
/// otherwise, so it quiesces with the append cadence and suppresses under
/// redaction exactly like the header line.
fn activity_cue_color_runs(
    state: &ProjectedPortalState,
    activity_cue_color: proto::Rgba,
    now_wall_us: u64,
) -> Vec<proto::TextColorRunProto> {
    if agent_activity_active(state, now_wall_us) {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(activity_cue_color),
        }]
    } else {
        Vec::new()
    }
}

/// Build the token-styled sentinel color run for the ambient transcript-tail
/// streaming cursor (hud-g1ena.5, §Agent Activity and Streaming Cue).
///
/// The mirror of `activity_cue_color_runs` for the tail cursor: a zero-length
/// sentinel run carrying the token-resolved `streaming_cursor_color`, gated by
/// the same [`agent_activity_active`] predicate that appends the
/// `PORTAL_STREAMING_CURSOR_GLYPH` to the transcript body. Kept a distinct run
/// (and token) from the header cue so a profile can style the tail cursor apart,
/// while both quiesce together when appends stop.
///
/// ## Tail marker (promotion-era, hud-zlq2v)
///
/// The run is pinned at the very END of the content (`[content_len..content_len]`),
/// NOT at byte 0. It stays zero-length — a pure sentinel that keeps the node on
/// the compositor's cached-markdown fast path (`markdown_node_has_pixel_runs`
/// remains false, so no lossy raw-content regression, #947) and leaves the
/// coalescing class of a streaming publish unchanged (no node/mutation added).
/// The content-end position is what distinguishes it from every byte-0 sentinel
/// (stale / lifecycle / unread / timestamp): the compositor reads it via
/// `markdown_node_tail_cursor_color` and recolors the trailing
/// `PORTAL_STREAMING_CURSOR_GLYPH` in this token accent at the precise,
/// layout-measured tail of the latest agent turn. This supersedes the old byte-0
/// sentinel, which the cached path discarded — the cursor never took its accent.
fn streaming_cursor_color_runs(
    state: &ProjectedPortalState,
    streaming_cursor_color: proto::Rgba,
    now_wall_us: u64,
    content_len: usize,
) -> Vec<proto::TextColorRunProto> {
    if agent_activity_active(state, now_wall_us) {
        // Pin the marker at content end so it is unambiguous among the byte-0
        // sentinels. `u32::try_from` guards the wire field width; on the
        // (unreachable, MAX_PORTAL_MARKDOWN_BYTES-bounded) overflow, drop the
        // marker rather than emit a wrong offset.
        let Ok(end) = u32::try_from(content_len) else {
            return Vec::new();
        };
        vec![proto::TextColorRunProto {
            start_byte: end,
            end_byte: end,
            color: Some(streaming_cursor_color),
        }]
    } else {
        Vec::new()
    }
}

/// Build the token-styled sentinel color run for the ambient per-turn arrival
/// timestamps (portal-chat-grade-affordances §Ambient Per-Turn Timestamps,
/// hud-g1ena.4).
///
/// Mirrors the other Phase-1 byte-0 sentinels (e.g. `unread_divider_color_runs`;
/// NOT `streaming_cursor_color_runs`, which pins its marker at content end):
/// a single zero-length run (`[0..0]`) carrying
/// the token-resolved `timestamp_color` so the SECONDARY presentation hue is
/// token-driven, never a literal in the render path (§6.1). Emitted exactly when
/// at least one stamp is rendered by [`visible_transcript_markdown_with`] — an
/// Expanded portal with a non-empty retained transcript and a non-`Off`
/// granularity. Under redaction `visible_transcript` is emptied upstream, so the
/// gate is false and no sentinel is emitted, exactly like the transcript preview.
///
/// ## Phase-1 scope note
///
/// Per-glyph coloring of each stamp requires its byte offset in the single-node
/// content, which is fragile in the raw-tile model (see `stale_marker_color_runs`).
/// For Phase-1 the timestamp color is a zero-length sentinel at byte 0 carrying
/// the token color; the text-visible `HH:MM` prefix marks each stamp. Promotion-
/// era structured multi-node layout will color the stamp spans precisely.
fn timestamp_color_runs(
    state: &ProjectedPortalState,
    timestamp_color: proto::Rgba,
    granularity: TimestampGranularity,
) -> Vec<proto::TextColorRunProto> {
    if granularity != TimestampGranularity::Off
        && state.presentation == ProjectedPortalPresentation::Expanded
        && !state.visible_transcript.is_empty()
    {
        vec![proto::TextColorRunProto {
            start_byte: 0,
            end_byte: 0,
            color: Some(timestamp_color),
        }]
    } else {
        Vec::new()
    }
}

/// Render the retained transcript window to markdown, inserting the in-transcript
/// unread divider (hud-g1ena.2) before the oldest retained unseen agent-authored
/// turn when `unread_boundary` names its index.
///
/// The unread divider is a distinct labeled marker (`PORTAL_UNREAD_DIVIDER_LINE`),
/// not a bare `---` thematic break, so it reads as the unread boundary rather than
/// an ordinary turn separator (the compositor styles all `---` breaks with the
/// single global `portal.divider.color`, so a bare break could not be told apart
/// in this single-node markdown path). Its token-resolved color rides a separate
/// zero-length sentinel (`unread_divider_color_runs`).
/// Two-arg shim rendering the transcript with per-turn timestamps OFF, used only
/// by unit tests that predate timestamps. Production goes through
/// [`visible_transcript_markdown_with`] with the profile-resolved granularity.
#[cfg(test)]
fn visible_transcript_markdown(units: &[TranscriptUnit], unread_boundary: Option<usize>) -> String {
    visible_transcript_markdown_with(units, unread_boundary, TimestampGranularity::Off)
}

/// Render the retained transcript window to markdown, additionally presenting
/// ambient per-turn arrival timestamps per `timestamps`
/// (portal-chat-grade-affordances §Ambient Per-Turn Timestamps, hud-g1ena.4).
///
/// Each presented time derives from the unit's runtime-assigned wall-clock
/// arrival metadata (`appended_at_wall_us`) — never adapter-supplied content, so
/// an adapter cannot forge it — formatted at minute precision by
/// [`format_wall_clock_arrival_hhmm`]. The stamp is a compact, subordinate prefix
/// on the turn (its token-resolved `timestamp_color` rides a separate zero-length
/// sentinel, [`timestamp_color_runs`]); CLOCK-DOMAIN typing is preserved — this is
/// arrival time, not media/display-clock presentation timing.
///
/// Granularity governs only how often a stamp appears:
/// - [`TimestampGranularity::Off`] — none (the default; unchanged from the bare
///   two-arg [`visible_transcript_markdown`]).
/// - [`TimestampGranularity::PerTurn`] — every turn.
/// - [`TimestampGranularity::Grouped`] — only when a turn's arrival minute differs
///   from the previous *stamped* turn's, so consecutive same-minute turns share one.
fn visible_transcript_markdown_with(
    units: &[TranscriptUnit],
    unread_boundary: Option<usize>,
    timestamps: TimestampGranularity,
) -> String {
    visible_transcript_markdown_spans(units, unread_boundary, timestamps).0
}

/// Byte range (into a rendered transcript body string) of one turn's TEXT,
/// tagged with the turn's runtime-assigned [`OutputKind`]. Produced alongside the
/// body by [`visible_transcript_markdown_spans`] so [`ResidentGrpcPortalAdapter::portal_node`]
/// can emit per-turn role-attribution color runs (text-stream-portals
/// §Conversational Turn Model and Per-Turn Role Attribution, hud-26869) without
/// re-deriving the separator / timestamp / unread-divider offsets. The range
/// covers only the turn's own text — never the `---` separator, the timestamp
/// prefix, or the unread-divider line that may precede it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TranscriptTurnSpan {
    start: usize,
    end: usize,
    output_kind: OutputKind,
}

/// Body-rendering core shared by [`visible_transcript_markdown_with`]. Returns
/// the rendered body plus, for each turn, the byte range of its text within the
/// returned string and the turn's `OutputKind`, so callers can attribute turns
/// by role. The `String` half is byte-identical to what the pre-spans
/// `visible_transcript_markdown_with` produced — the spans are purely additive.
fn visible_transcript_markdown_spans(
    units: &[TranscriptUnit],
    unread_boundary: Option<usize>,
    timestamps: TimestampGranularity,
) -> (String, Vec<TranscriptTurnSpan>) {
    let mut result = String::new();
    let mut spans: Vec<TranscriptTurnSpan> = Vec::with_capacity(units.len());
    // Last arrival-minute bucket a stamp was emitted for, for Grouped coalescing.
    let mut last_stamped_minute: Option<u64> = None;
    for (index, unit) in units.iter().enumerate() {
        if index > 0 {
            // Turn separator between adjacent transcript entries (hud-nx7yq.4): a
            // thematic break on its own line, which the compositor renders as a
            // token-styled divider (`portal.divider.color`). This re-encodes the
            // `Vec<TranscriptUnit>` boundary — lost in the single-`\n` flatten —
            // so the history reads as discrete turns. Content-free geometry: under
            // redaction `units` is emptied upstream, so no separators are emitted.
            result.push_str("\n---\n");
        }
        if unread_boundary == Some(index) {
            // Unread divider before the oldest retained unseen agent-authored turn.
            // A quiet labeled marker (not a bare `---`) so it is distinguishable
            // from the ordinary turn separator above. Own line, then the turn text.
            result.push_str(PORTAL_UNREAD_DIVIDER_LINE);
            result.push('\n');
        }
        // Ambient per-turn arrival timestamp (secondary/subordinate prefix). Sourced
        // from the runtime-assigned wall-clock arrival metadata, never adapter
        // content. Grouped coalesces consecutive same-minute turns onto one stamp.
        let minute_bucket = unit.appended_at_wall_us / WALL_CLOCK_MINUTE_US;
        let show_stamp = match timestamps {
            TimestampGranularity::Off => false,
            TimestampGranularity::PerTurn => true,
            TimestampGranularity::Grouped => last_stamped_minute != Some(minute_bucket),
        };
        if show_stamp {
            result.push_str(&format_wall_clock_arrival_hhmm(unit.appended_at_wall_us));
            result.push_str(PORTAL_TIMESTAMP_SEPARATOR);
            last_stamped_minute = Some(minute_bucket);
        }
        // Record the turn TEXT range for per-turn role attribution: the span
        // starts at the current tail (after any separator/divider/timestamp) and
        // ends after the clamped turn text, so attribution colors only the turn's
        // own words (hud-26869).
        let start = result.len();
        result.push_str(clamp_utf8(&unit.output_text, 4_096));
        spans.push(TranscriptTurnSpan {
            start,
            end: result.len(),
            output_kind: unit.output_kind,
        });
    }
    (result, spans)
}

/// Whether an OUTPUT turn of kind `kind` is rendered in the distinct
/// role-attribution color (`transcript_system_color`) rather than the base
/// assistant transcript color (text-stream-portals §Conversational Turn Model
/// and Per-Turn Role Attribution, hud-26869).
///
/// `Assistant` is the model's own conversational prose (base color). `Tool`,
/// `Status`, `Error`, and `Other` are agent-side scaffolding shown in the
/// attribution color. `Viewer` never appears in the OUTPUT transcript — viewer
/// turns live in the separately-bounded INPUT history (§Viewer Reply Echo) and
/// are not attributed here — so it is treated as base-colored for totality. The
/// exhaustive match forces a deliberate choice if `OutputKind` gains a variant.
fn turn_uses_system_attribution(kind: OutputKind) -> bool {
    match kind {
        OutputKind::Assistant | OutputKind::Viewer => false,
        OutputKind::Tool | OutputKind::Status | OutputKind::Error | OutputKind::Other => true,
    }
}

/// Build per-turn role-attribution color runs (text-stream-portals
/// §Conversational Turn Model and Per-Turn Role Attribution, hud-26869): one real
/// `[start_byte, end_byte)` span per non-assistant agent-side turn, carrying the
/// token-resolved `system_color`. Assistant turns emit no run and inherit the
/// node's base transcript color, so the assistant's prose reads distinctly from
/// tool/status/error/other scaffolding.
///
/// Ranges are clamped to `content_len` (the truncated node content) and dropped
/// if they fall entirely past it, so an over-long transcript truncated to
/// `MAX_PORTAL_MARKDOWN_BYTES` never emits a run addressing bytes the node does
/// not carry. Empty when there are no attributed turns (all-assistant, or a
/// redacted/empty/collapsed transcript that produced no spans), so attribution
/// suppresses with the transcript.
fn turn_attribution_color_runs(
    spans: &[TranscriptTurnSpan],
    system_color: proto::Rgba,
    content_len: usize,
) -> Vec<proto::TextColorRunProto> {
    // At most one run per span (assistant turns are skipped); pre-size to the bound.
    let mut runs = Vec::with_capacity(spans.len());
    for span in spans {
        if !turn_uses_system_attribution(span.output_kind) {
            continue;
        }
        let start = span.start.min(content_len);
        let end = span.end.min(content_len);
        if start >= end {
            // Fully clamped away by truncation, or an empty turn — no run.
            continue;
        }
        let (Ok(start_byte), Ok(end_byte)) = (u32::try_from(start), u32::try_from(end)) else {
            // Unreachable under the MAX_PORTAL_MARKDOWN_BYTES bound; drop rather
            // than emit a wrong offset (mirrors streaming_cursor_color_runs).
            continue;
        };
        runs.push(proto::TextColorRunProto {
            start_byte,
            end_byte,
            color: Some(system_color),
        });
    }
    runs
}

/// Render the INPUT-history band: the viewer's own accepted replies (the
/// viewer-echo stack), stacked with token-styled `---` turn dividers per
/// §Viewer Reply Echo (two-pane INPUT/OUTPUT split). Reuses
/// `visible_transcript_markdown_with` for the same divider + ambient-timestamp
/// treatment, but never carries an unread divider — viewer turns are the
/// viewer's own already-seen text and are never unread.
fn input_history_markdown(units: &[TranscriptUnit], timestamps: TimestampGranularity) -> String {
    visible_transcript_markdown_with(units, None, timestamps)
}

/// Render the native INPUT pane without duplicating draft glyphs. Viewer history
/// stays left of the divider; compact pending/feedback state and the final status
/// line preserve the established composer availability contract for both pixels
/// and headless verification.
fn portal_input_markdown(
    state: &ProjectedPortalState,
    composer_display: Option<&ComposerDisplayState>,
    timestamps: TimestampGranularity,
) -> String {
    let status = composer_line(composer_display, state.interaction_enabled);
    let mut tail = String::new();
    // Keep the native two-pane surface aligned with the compact pending-input
    // state shown by the collapsed/raw portal path. In particular, expiry changes
    // this count from N to zero, making its scheduled repaint real scene work.
    push_input_status_lines(&mut tail, state);
    push_line(&mut tail, &status);
    // Reserve the compact state + final composer line before bounding history so
    // neither affordance can be truncated away by a full viewer-echo window.
    let max_history_bytes = MAX_PORTAL_MARKDOWN_BYTES.saturating_sub(tail.len() + 1);
    let history = input_history_markdown(&state.input_history, timestamps);
    let mut result = clamp_utf8(&history, max_history_bytes).to_string();
    push_line(&mut result, &tail);
    result
}

fn clamp_one_line(text: &str, max_bytes: usize) -> String {
    clamp_utf8(text.lines().next().unwrap_or_default(), max_bytes).to_string()
}

fn clamp_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn push_line(result: &mut String, line: &str) {
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(line);
}

fn sample_budget(started: Instant, budget_us: u64) -> ResidentGrpcBudgetSample {
    ResidentGrpcBudgetSample {
        elapsed_us: started.elapsed().as_micros() as u64,
        budget_us,
    }
}

fn new_scene_id_bytes() -> Vec<u8> {
    uuid::Uuid::now_v7().as_bytes().to_vec()
}

fn new_node_id_le() -> Vec<u8> {
    uuid::Uuid::now_v7().to_bytes_le().to_vec()
}

// ── Token bridge (tze_hud_config feature) ─────────────────────────────────────

/// Convert a fully-resolved `PortalPartTokens` (from `tze_hud_config`) into
/// `PortalVisualTokens`.
///
/// This is the **canonical production-path conversion**. Any code that constructs
/// a `ResidentGrpcPortalAdapter` MUST use this function instead of
/// hand-constructing `PortalVisualTokens`. When a token-map swap occurs (e.g.
/// profile hot-reload), call `tze_hud_config::resolve_portal_tokens` on the new
/// `DesignTokenMap` to get `PortalPartTokens`, pass the result here, and forward
/// the `PortalVisualTokens` to `adapter.set_visual_tokens(...)`.
///
/// Maps every visual part consumed by the native expanded subtree and collapsed
/// card. Transition values remain runtime-owned and are not part of this render
/// payload.
///
/// Config hex colors carry encoded sRGB channels. Decode RGB to linear light for
/// the scene's compositor/text consumers; alpha passes through unchanged.
///
/// ## Usage
///
/// ```rust,ignore
/// use tze_hud_config::{resolve_portal_tokens, tokens::DesignTokenMap};
/// use tze_hud_projection::resident_grpc::{
///     ResidentGrpcPortalAdapter, ResidentGrpcPortalConfig,
///     portal_visual_tokens_from_part_tokens,
/// };
///
/// // At adapter construction:
/// let part_tokens = resolve_portal_tokens(&resolved_token_map);
/// let visual_tokens = portal_visual_tokens_from_part_tokens(&part_tokens);
/// let adapter = ResidentGrpcPortalAdapter::with_tokens(config, visual_tokens);
///
/// // On profile hot-reload / token-map swap:
/// let new_part_tokens = resolve_portal_tokens(&new_token_map);
/// adapter.set_visual_tokens(portal_visual_tokens_from_part_tokens(&new_part_tokens));
/// ```
pub fn portal_visual_tokens_from_part_tokens(
    part: &tze_hud_config::PortalPartTokens,
) -> PortalVisualTokens {
    let linear_channel = |encoded: f32| {
        if encoded <= 0.04045 {
            encoded / 12.92
        } else {
            ((encoded + 0.055) / 1.055).powf(2.4)
        }
    };
    let linear_color = |color: &tze_hud_config::tokens::Rgba| proto::Rgba {
        r: linear_channel(color.r),
        g: linear_channel(color.g),
        b: linear_channel(color.b),
        a: color.a,
    };
    PortalVisualTokens {
        frame_background: linear_color(&part.frame_background),
        frame_opacity: part.frame_opacity,
        header_text_color: linear_color(&part.header_text_color),
        header_font_size_px: part.header_font_size_px,
        divider_color: linear_color(&part.divider_color),
        divider_grip_color: linear_color(&part.resize_grip_color),
        content_inset_px: part.content_inset_px,
        transcript_background: linear_color(&part.transcript_background),
        transcript_text_color: linear_color(&part.transcript_text_color),
        transcript_system_color: linear_color(&part.transcript_system_color),
        transcript_font_size_px: part.transcript_font_size_px,
        transcript_dim_text_color: linear_color(&part.transcript_dim_text_color),
        transcript_dim_background: linear_color(&part.transcript_dim_background),
        stale_marker_color: linear_color(&part.stale_marker_color),
        disconnect_badge_color: linear_color(&part.disconnect_badge_color),
        disconnect_badge_width_px: part.disconnect_badge_width_px,
        unread_indicator_color: linear_color(&part.unread_indicator_color),
        unread_divider_color: linear_color(&part.unread_divider_color),
        awaiting_reply_color: linear_color(&part.awaiting_reply_color),
        empty_state_color: linear_color(&part.empty_state_color),
        connecting_marker_color: linear_color(&part.connecting_marker_color),
        activity_cue_color: linear_color(&part.activity_cue_color),
        streaming_cursor_color: linear_color(&part.streaming_cursor_color),
        delivery_inflight_color: linear_color(&part.delivery_inflight_color),
        delivery_delivered_color: linear_color(&part.delivery_delivered_color),
        delivery_failed_color: linear_color(&part.delivery_failed_color),
        timestamp_color: linear_color(&part.timestamp_color),
        timestamp_granularity: part.timestamp_granularity,
        lifecycle_active_color: linear_color(&part.lifecycle_active_color),
        lifecycle_attached_color: linear_color(&part.lifecycle_attached_color),
        lifecycle_attention_color: linear_color(&part.lifecycle_attention_color),
        lifecycle_inactive_color: linear_color(&part.lifecycle_inactive_color),
        lifecycle_accent_width_px: part.lifecycle_accent_width_px,
        header_height_px: part.header_height_px,
        section_gap_px: part.section_gap_px,
        collapsed_background: linear_color(&part.collapsed_background),
        collapsed_text_color: linear_color(&part.collapsed_text_color),
        collapsed_font_size_px: part.collapsed_font_size_px,
        composer_background: linear_color(&part.composer_background),
        composer_text_color: linear_color(&part.composer_text_color),
        composer_font_size_px: part.composer_font_size_px,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ContentClassification, OutputKind, ProjectedPortalAdapterFamily, ProjectedPortalAttention,
        ProjectedPortalLayer, ProjectedPortalPresentation, ProjectedPortalRuntimeAuthority,
        ProjectedPortalState, ProjectionLifecycleState,
    };

    /// Build a minimal interaction-enabled expanded portal state for adapter tests.
    fn make_expanded_interaction_state(portal_id: &str) -> ProjectedPortalState {
        ProjectedPortalState {
            projection_id: "test-proj-1".to_string(),
            portal_id: portal_id.to_string(),
            adapter_family: ProjectedPortalAdapterFamily::CooperativeProjection,
            runtime_authority: ProjectedPortalRuntimeAuthority::ResidentSessionLease,
            layer: ProjectedPortalLayer::Content,
            presentation: ProjectedPortalPresentation::Expanded,
            preserve_geometry: false,
            redacted: false,
            connection_degraded: false,
            // Live, interactive fixture: the session has connected (so the
            // first-run empty state, not the connecting placeholder, applies).
            has_ever_connected: true,
            interaction_enabled: true,
            attention: ProjectedPortalAttention::Ambient,
            provider_kind: None,
            display_name: Some("Test Session".to_string()),
            workspace_hint: None,
            repository_hint: None,
            icon_profile_hint: None,
            hud_target: None,
            lifecycle_state: None,
            status_text: None,
            visible_transcript: vec![],
            visible_transcript_bytes: 0,
            input_history: vec![],
            unread_output_count: None,
            visible_unread_output_count: None,
            pending_input_count: None,
            pending_input_bytes: None,
            last_input_feedback: None,
            latest_viewer_delivery_state: None,
            draft_batch: None,
            geometry_batch: None,
            resized_bounds: None,
        }
    }

    // ── First-class portal surface migration (hud-rpm9s) ─────────────────────

    use tze_hud_protocol::proto::mutation_proto::Mutation as M;

    fn set_portal_surface_of(
        batch: &session_proto::MutationBatch,
    ) -> Option<&proto::PortalSurfaceProto> {
        batch.mutations.iter().find_map(|m| match &m.mutation {
            Some(M::SetPortalSurface(sps)) => sps.surface.as_ref(),
            _ => None,
        })
    }

    /// Borrow the bounds of the surface's declared part of the given kind
    /// (panics if absent — the caller asserts presence by kind).
    fn part_of(
        surface: &proto::PortalSurfaceProto,
        kind: proto::PortalPartKindProto,
    ) -> proto::Rect {
        surface
            .parts
            .iter()
            .find(|p| p.kind == kind as i32)
            .unwrap_or_else(|| panic!("surface must declare a {kind:?} part"))
            .bounds
            .expect("declared part must carry bounds")
    }

    fn has_publish_to_tile(batch: &session_proto::MutationBatch) -> bool {
        batch
            .mutations
            .iter()
            .any(|m| matches!(&m.mutation, Some(M::PublishToTile(_))))
    }

    /// The migrated render path declares the first-class surface exactly once and
    /// keeps the raw-tile assembly on every render (the retained escape hatch):
    /// - the FIRST `render_batch_with_surface` prepends a structural
    ///   `SetPortalSurface` (index 0) and still emits the raw `PublishToTile`;
    /// - a SUBSEQUENT render emits NO `SetPortalSurface` (steady-state stays off
    ///   the Transactional path) but still carries the coalescible
    ///   `UpdatePortalSurfaceState` patch and the raw-tile `PublishToTile`.
    #[test]
    fn render_batch_with_surface_declares_once_and_preserves_raw_tiles() {
        let config = ResidentGrpcPortalConfig::new(vec![7u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![9u8; 16]);
        let state = make_expanded_interaction_state("portal-declare-once");

        assert!(!adapter.surface_declared(), "surface starts undeclared");

        // First render: declaration prepended, raw tiles present.
        let first = adapter
            .render_batch_with_surface(&state, 0)
            .expect("first render must succeed");
        assert!(
            adapter.surface_declared(),
            "first render marks surface declared"
        );
        assert!(
            matches!(&first.mutations[0].mutation, Some(M::SetPortalSurface(_))),
            "first render must PREPEND the structural SetPortalSurface declaration"
        );
        assert_eq!(
            first
                .mutations
                .iter()
                .filter(|m| matches!(&m.mutation, Some(M::SetPortalSurface(_))))
                .count(),
            1,
            "exactly one SetPortalSurface on the first render"
        );
        assert!(
            has_publish_to_tile(&first),
            "raw-tile escape hatch: PublishToTile must still be emitted on first render"
        );
        // The declaration carries full lifecycle/display state, so the first
        // render emits NO UpdatePortalSurfaceState — avoiding an in-batch
        // declare-then-patch ordering dependency (the wire applies batches
        // atomically).
        assert!(
            !first
                .mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::UpdatePortalSurfaceState(_)))),
            "first render must NOT emit UpdatePortalSurfaceState (declaration carries state)"
        );

        // Second render: NO re-declaration; raw tiles + coalescible patch only.
        let second = adapter
            .render_batch_with_surface(&state, 1)
            .expect("second render must succeed");
        assert!(
            !second
                .mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::SetPortalSurface(_)))),
            "steady-state render must NOT re-declare the surface (stays off the \
             Transactional path)"
        );
        assert!(
            has_publish_to_tile(&second),
            "raw-tile escape hatch: PublishToTile must still be emitted on later renders"
        );
        assert!(
            second
                .mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::UpdatePortalSurfaceState(_)))),
            "steady-state render carries the coalescible UpdatePortalSurfaceState patch"
        );
    }

    /// A resize changes the declared part geometry even though the set of parts
    /// stays the same. The surface must therefore be re-declared before the
    /// newly-sized inline subtree is published; otherwise geometry-only part
    /// bounds remain at the attach-time size while the body has already grown.
    #[test]
    fn render_batch_with_surface_redeclares_when_resized_bounds_change() {
        let config = ResidentGrpcPortalConfig::new(vec![7u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![9u8; 16]);

        let initial = make_expanded_interaction_state("portal-resized-surface");
        adapter
            .render_batch_with_surface(&initial, 0)
            .expect("initial render must declare the surface");

        let resized_width = DEFAULT_EXPANDED_W + 180.0;
        let resized_height = DEFAULT_EXPANDED_H + 120.0;
        let mut resized = make_expanded_interaction_state("portal-resized-surface");
        resized.resized_bounds = Some(crate::AdapterPortalRect::from_f32(
            120.0,
            80.0,
            resized_width,
            resized_height,
        ));

        let batch = adapter
            .render_batch_with_surface(&resized, 1)
            .expect("resized render must succeed");
        let surface = set_portal_surface_of(&batch).expect(
            "resized portal geometry must re-declare the surface instead of leaving stale parts",
        );
        let frame = part_of(surface, proto::PortalPartKindProto::PortalPartKindFrame);
        let transcript = part_of(
            surface,
            proto::PortalPartKindProto::PortalPartKindTranscript,
        );
        assert_eq!(frame.width, resized_width);
        assert_eq!(frame.height, resized_height);
        assert_eq!(
            transcript.height,
            resized_height - adapter.visual_tokens().header_height_px,
            "the re-declared OUTPUT part must track the resized body height"
        );
        assert!(
            !batch
                .mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::UpdatePortalSurfaceState(_)))),
            "the structural re-declaration already carries lifecycle/display state"
        );
    }

    /// `render_batch` alone (the raw-tile path) still functions as the retained
    /// escape hatch: it emits the raw `PublishToTile` assembly and NEVER the
    /// structural `SetPortalSurface` declaration (that is only ever added by
    /// `render_batch_with_surface`).
    #[test]
    fn render_batch_alone_is_raw_tile_escape_hatch() {
        let config = ResidentGrpcPortalConfig::new(vec![1u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![2u8; 16]);
        let state = make_expanded_interaction_state("portal-escape-hatch");

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");
        assert!(
            has_publish_to_tile(&batch),
            "raw-tile path must still emit PublishToTile content"
        );
        assert!(
            !batch.mutations.iter().any(|m| matches!(
                &m.mutation,
                Some(M::SetPortalSurface(_)) | Some(M::UpdatePortalSurfaceState(_))
            )),
            "raw-tile path must emit NO first-class surface mutation (declaration or patch)"
        );
        assert!(
            !adapter.surface_declared(),
            "render_batch alone must not flip the surface_declared flag"
        );
    }

    /// The declared surface maps the adapter's expanded two-pane layout onto the
    /// first-class parts (Frame + Header + Composer + Transcript + Divider),
    /// carries the identity (session id, display name, ResidentLlm peer class),
    /// and reflects the lifecycle/display state. The whole descriptor passes the
    /// scene-side structural validation.
    #[test]
    fn portal_surface_declaration_maps_expanded_parts_and_identity() {
        use tze_hud_protocol::convert::proto_portal_surface_to_scene;

        let config = ResidentGrpcPortalConfig::new(vec![3u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![4u8; 16]);
        let mut state = make_expanded_interaction_state("portal-parts");
        state.lifecycle_state = Some(ProjectionLifecycleState::Active);

        let decl = adapter
            .portal_surface_declaration_mutation(&state)
            .expect("declaration must succeed once tile recorded");
        let surface = match decl.mutation {
            Some(M::SetPortalSurface(sps)) => sps.surface.expect("surface present"),
            other => panic!("expected SetPortalSurface, got {other:?}"),
        };

        let kinds: Vec<i32> = surface.parts.iter().map(|p| p.kind).collect();
        for expected in [
            proto::PortalPartKindProto::PortalPartKindFrame,
            proto::PortalPartKindProto::PortalPartKindHeader,
            proto::PortalPartKindProto::PortalPartKindTranscript,
            proto::PortalPartKindProto::PortalPartKindComposer,
            proto::PortalPartKindProto::PortalPartKindDivider,
        ] {
            assert!(
                kinds.contains(&(expected as i32)),
                "expanded interactive portal must declare part {expected:?}"
            );
        }
        let identity = surface.identity.as_ref().expect("identity present");
        assert_eq!(identity.session_id, "portal-parts");
        assert_eq!(identity.display_name, "Test Session");
        assert_eq!(
            identity.peer_class,
            proto::PortalPeerClassProto::PortalPeerClassResidentLlm as i32
        );
        assert_eq!(
            surface.lifecycle,
            proto::PortalLifecycleStateProto::PortalLifecycleStateActive as i32
        );
        assert_eq!(
            surface.display_state,
            proto::PortalDisplayStateProto::PortalDisplayStateExpanded as i32
        );

        // The whole descriptor must satisfy the scene-side structural contract.
        proto_portal_surface_to_scene(&surface)
            .expect("declared surface must pass scene structural validation");
    }

    /// hud-zn6yw: the declared `Header` part height and the Header→Transcript
    /// middle-divider width are resolved from the `portal.spacing.*` tokens carried on
    /// `PortalVisualTokens`, not from a hardcoded constant. At the canonical
    /// default tokens (`header_height_px` = 52, `section_gap_px` = 8) the header
    /// strip is 52px — matching the former hardcoded `PORTAL_SURFACE_HEADER_BAND_PX`
    /// and `PORTAL_HEADER_DRAG_BAND_PX_DEFAULT`, so the draggable header band
    /// (derived from this part by `portal_header_band_anchors`) is unchanged — the
    /// OUTPUT pane starts below the 52px header and to the right of the token-sized
    /// 8px middle divider.
    #[test]
    fn portal_surface_header_and_gap_honor_default_spacing_tokens() {
        let config = ResidentGrpcPortalConfig::new(vec![3u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![4u8; 16]);
        let state = make_expanded_interaction_state("portal-spacing-default");

        let surface = match adapter
            .portal_surface_declaration_mutation(&state)
            .expect("declaration must succeed once tile recorded")
            .mutation
        {
            Some(M::SetPortalSurface(sps)) => sps.surface.expect("surface present"),
            other => panic!("expected SetPortalSurface, got {other:?}"),
        };
        let full_h = DEFAULT_EXPANDED_H;

        let header = part_of(&surface, proto::PortalPartKindProto::PortalPartKindHeader);
        let transcript = part_of(
            &surface,
            proto::PortalPartKindProto::PortalPartKindTranscript,
        );
        let divider = part_of(&surface, proto::PortalPartKindProto::PortalPartKindDivider);
        assert_eq!(header.height, 52.0, "header height honors header_height_px");
        assert_eq!(header.y, 0.0);
        assert_eq!(
            transcript.y, 52.0,
            "OUTPUT pane starts directly below the draggable header"
        );
        assert_eq!(
            transcript.height,
            full_h - 52.0,
            "OUTPUT pane takes the remaining surface height"
        );
        assert_eq!(divider.width, 8.0, "divider width honors section_gap_px");
        assert!(transcript.x > divider.x, "OUTPUT pane follows the divider");
    }

    /// hud-zn6yw: overriding the spacing tokens moves the declared part bounds in
    /// lock-step — a taller header + wider gap pushes the transcript down and
    /// shrinks it by the same amount, proving the geometry is genuinely
    /// token-driven rather than constant.
    #[test]
    fn portal_surface_header_and_gap_track_token_overrides() {
        let tokens = PortalVisualTokens {
            header_height_px: 40.0,
            section_gap_px: 12.0,
            ..PortalVisualTokens::default()
        };

        let config = ResidentGrpcPortalConfig::new(vec![3u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::with_tokens(config, tokens);
        adapter.record_created_tile(vec![4u8; 16]);
        let state = make_expanded_interaction_state("portal-spacing-override");

        let surface = match adapter
            .portal_surface_declaration_mutation(&state)
            .expect("declaration must succeed once tile recorded")
            .mutation
        {
            Some(M::SetPortalSurface(sps)) => sps.surface.expect("surface present"),
            other => panic!("expected SetPortalSurface, got {other:?}"),
        };
        let full_h = DEFAULT_EXPANDED_H;

        let header = part_of(&surface, proto::PortalPartKindProto::PortalPartKindHeader);
        let transcript = part_of(
            &surface,
            proto::PortalPartKindProto::PortalPartKindTranscript,
        );
        let divider = part_of(&surface, proto::PortalPartKindProto::PortalPartKindDivider);
        assert_eq!(header.height, 40.0, "header height tracks the override");
        assert_eq!(
            transcript.y, 40.0,
            "OUTPUT pane starts below the overridden header"
        );
        assert_eq!(transcript.height, full_h - 40.0);
        assert_eq!(divider.width, 12.0, "divider width tracks section gap");
    }

    /// hud-z5uem: cooperative projections must paint the reviewed two-pane
    /// portal chrome in the resident adapter itself. The LLM-facing projection
    /// contract carries semantic text only; INPUT/OUTPUT geometry and chrome are
    /// a native subtree rooted in the one governed portal tile.
    #[test]
    fn expanded_portal_node_materializes_native_two_pane_chrome() {
        let config = ResidentGrpcPortalConfig::new(vec![3u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let mut state = make_expanded_interaction_state("portal-native-two-pane");
        state.visible_transcript = vec![transcript_unit_text(
            1,
            OutputKind::Assistant,
            "hello from agent",
        )];
        state.input_history = vec![transcript_unit_text(
            2,
            OutputKind::Viewer,
            "hello from viewer",
        )];

        let root = adapter.portal_node(&state, vec![0u8; 16], 0);
        assert!(
            matches!(root.data, Some(proto::node_proto::Data::SolidColor(_))),
            "expanded portal root must be the token-styled native frame"
        );

        let text_children: Vec<&proto::TextMarkdownNodeProto> = root
            .children
            .iter()
            .filter_map(|child| match child.data.as_ref() {
                Some(proto::node_proto::Data::TextMarkdown(text)) => Some(text),
                _ => None,
            })
            .collect();
        assert!(
            text_children.iter().any(|text| text.content == "INPUT"),
            "native chrome must label the left input pane"
        );
        assert!(
            text_children.iter().any(|text| text.content == "OUTPUT"),
            "native chrome must label the right output pane"
        );
        assert!(
            text_children
                .iter()
                .filter(|text| text.overflow == proto::TextOverflowProto::Clip as i32)
                .all(|text| text
                    .background
                    .is_some_and(|background| background.a == 0.0)),
            "chrome labels and INPUT copy must stay transparent over the rounded solid layers"
        );

        let output = text_children
            .iter()
            .find(|text| text.content.contains("hello from agent"))
            .expect("the OUTPUT pane must own the projected transcript");
        let output_bounds = output.bounds.expect("OUTPUT transcript bounds");
        assert!(
            output_bounds.x > 0.0 && output_bounds.width < DEFAULT_EXPANDED_W,
            "OUTPUT transcript must occupy only the right pane: {output_bounds:?}"
        );
        assert!(
            !output.content.contains("hello from viewer"),
            "viewer replies must never leak into the OUTPUT pane"
        );
        assert!(
            text_children
                .iter()
                .any(|text| text.content.contains("hello from viewer")),
            "retained viewer replies must render in the INPUT pane"
        );

        let solid_children: Vec<&proto::SolidColorNodeProto> = root
            .children
            .iter()
            .filter_map(|child| match child.data.as_ref() {
                Some(proto::node_proto::Data::SolidColor(solid)) => Some(solid),
                _ => None,
            })
            .collect();
        assert!(
            solid_children.iter().all(|solid| solid.radius > 0.0),
            "every native chrome solid layered over the rounded frame must use the \
             compositor's ordered SDF pass; a flat child is drawn before the rounded \
             root and is then hidden by that root: {solid_children:?}"
        );
        let pane_fills: Vec<proto::Rgba> = solid_children
            .iter()
            .filter_map(|solid| {
                let bounds = solid.bounds?;
                (bounds.width > DEFAULT_EXPANDED_W / 3.0
                    && bounds.height > DEFAULT_EXPANDED_H / 2.0)
                    .then_some(solid.color?)
            })
            .collect();
        assert_eq!(pane_fills.len(), 2, "native profile must paint two panes");
        assert!(
            pane_fills.iter().all(|color| {
                color.r == 0.0
                    && color.g == 0.0
                    && color.b == 0.0
                    && (color.a - (242.0 / 255.0)).abs() < f32::EPSILON
            }),
            "INPUT and OUTPUT panes must use the reviewed 95%-black token profile: {pane_fills:?}"
        );
        assert!(
            solid_children.iter().any(|solid| {
                let bounds = solid.bounds.expect("solid child bounds");
                bounds.width > 0.0
                    && bounds.width <= adapter.visual_tokens().section_gap_px
                    && bounds.height > adapter.visual_tokens().header_height_px / 2.0
            }),
            "native chrome must paint a fat middle divider with a visible grip"
        );
    }

    #[test]
    fn native_input_pane_preserves_composer_status_at_history_bound() {
        let mut state = make_expanded_interaction_state("portal-input-bound");
        state.input_history = (0..6)
            .map(|sequence| transcript_unit_text(sequence, OutputKind::Viewer, &"x".repeat(4_096)))
            .collect();

        let input = portal_input_markdown(&state, None, TimestampGranularity::Off);
        assert!(
            input.len() <= MAX_PORTAL_MARKDOWN_BYTES,
            "native INPUT materialization must stay bounded"
        );
        assert!(
            input.ends_with("composer: ready"),
            "composer availability must survive bounded history truncation"
        );
    }

    /// hud-zn6yw: the `portal.spacing.header_height_px` / `section_gap_px` design
    /// tokens flow through `portal_visual_tokens_from_part_tokens` onto
    /// `PortalVisualTokens`, so a profile token-map override reaches the layout.
    #[test]
    fn portal_visual_tokens_map_spacing_geometry() {
        use tze_hud_config::{
            PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX, PORTAL_TOKEN_SPACING_SECTION_GAP_PX,
            resolve_portal_tokens, tokens::DesignTokenMap,
        };

        // Defaults map through unchanged.
        let default_visual =
            portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&DesignTokenMap::new()));
        assert_eq!(default_visual.header_height_px, 52.0);
        assert_eq!(default_visual.section_gap_px, 8.0);

        // A profile override propagates onto PortalVisualTokens.
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX.to_string(),
            "44".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_SPACING_SECTION_GAP_PX.to_string(),
            "16".to_string(),
        );
        let overridden = portal_visual_tokens_from_part_tokens(&resolve_portal_tokens(&overrides));
        assert_eq!(overridden.header_height_px, 44.0);
        assert_eq!(overridden.section_gap_px, 16.0);
    }

    /// A collapsed portal declares a `CollapsedCard` part and a `Collapsed`
    /// display state (never a Composer part).
    #[test]
    fn portal_surface_declaration_collapsed_declares_card() {
        let config = ResidentGrpcPortalConfig::new(vec![5u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![6u8; 16]);
        let mut state = make_expanded_interaction_state("portal-collapsed");
        state.presentation = ProjectedPortalPresentation::Collapsed;

        let batch = adapter
            .render_batch_with_surface(&state, 0)
            .expect("render must succeed");
        let surface = set_portal_surface_of(&batch).expect("declaration present");
        let kinds: Vec<i32> = surface.parts.iter().map(|p| p.kind).collect();
        assert!(
            kinds.contains(&(proto::PortalPartKindProto::PortalPartKindCollapsedCard as i32)),
            "collapsed portal must declare a CollapsedCard part"
        );
        assert!(
            !kinds.contains(&(proto::PortalPartKindProto::PortalPartKindComposer as i32)),
            "collapsed portal must not declare a Composer part"
        );
        assert_eq!(
            surface.display_state,
            proto::PortalDisplayStateProto::PortalDisplayStateCollapsed as i32,
            "collapsed portal must declare a Collapsed display state"
        );
    }

    /// A presentation transition changes the part topology, so
    /// `render_batch_with_surface` must RE-declare (a fresh `SetPortalSurface`
    /// carrying the new part set) rather than only patch — otherwise the scene
    /// would flip `display_state` to Collapsed without ever adding the
    /// `CollapsedCard` part (hud-rpm9s review).
    #[test]
    fn render_batch_with_surface_redeclares_on_presentation_change() {
        let config = ResidentGrpcPortalConfig::new(vec![8u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![9u8; 16]);

        let expanded = make_expanded_interaction_state("portal-topology");
        let mut collapsed = make_expanded_interaction_state("portal-topology");
        collapsed.presentation = ProjectedPortalPresentation::Collapsed;

        let has_set = |b: &session_proto::MutationBatch| {
            b.mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::SetPortalSurface(_))))
        };
        let has_patch = |b: &session_proto::MutationBatch| {
            b.mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::UpdatePortalSurfaceState(_))))
        };

        // 1) First expanded render → declares.
        let a = adapter.render_batch_with_surface(&expanded, 0).unwrap();
        assert!(
            has_set(&a) && !has_patch(&a),
            "first render declares, no patch"
        );

        // 2) Same expanded topology → coalescible patch only.
        let b = adapter.render_batch_with_surface(&expanded, 1).unwrap();
        assert!(!has_set(&b) && has_patch(&b), "same topology patches only");

        // 3) Collapse → topology changed → RE-declare with the CollapsedCard part.
        let c = adapter.render_batch_with_surface(&collapsed, 2).unwrap();
        assert!(
            has_set(&c) && !has_patch(&c),
            "presentation change must re-declare (SetPortalSurface), not patch"
        );
        let surface = set_portal_surface_of(&c).expect("re-declaration carries a surface");
        assert!(
            surface
                .parts
                .iter()
                .any(|p| p.kind == proto::PortalPartKindProto::PortalPartKindCollapsedCard as i32),
            "re-declared surface must carry the CollapsedCard part"
        );

        // 4) Same collapsed topology → coalescible patch only.
        let d = adapter.render_batch_with_surface(&collapsed, 3).unwrap();
        assert!(
            !has_set(&d) && has_patch(&d),
            "steady collapsed topology patches only"
        );
    }

    /// hud-zn6yw (Codex P2): a live profile swap that changes the spacing geometry
    /// (`header_height_px` / `section_gap_px`) MUST re-declare the surface, not
    /// just patch — the part bounds (and the header/drag-band derived from them)
    /// ride only the `SetPortalSurface` declaration, so without re-declaration the
    /// new geometry would stay stale until the next presentation/interaction
    /// transition.
    #[test]
    fn render_batch_with_surface_redeclares_on_spacing_token_swap() {
        let config = ResidentGrpcPortalConfig::new(vec![8u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![9u8; 16]);
        let state = make_expanded_interaction_state("portal-spacing-swap");

        let has_set = |b: &session_proto::MutationBatch| {
            b.mutations
                .iter()
                .any(|m| matches!(&m.mutation, Some(M::SetPortalSurface(_))))
        };

        // 1) First render declares.
        let a = adapter.render_batch_with_surface(&state, 0).unwrap();
        assert!(has_set(&a), "first render declares");

        // 2) Steady state (no token change) → no re-declaration.
        let b = adapter.render_batch_with_surface(&state, 1).unwrap();
        assert!(!has_set(&b), "unchanged geometry does not re-declare");

        // 3) Profile swap changing the header height → re-declare with new bounds.
        adapter.set_visual_tokens(PortalVisualTokens {
            header_height_px: 72.0,
            ..PortalVisualTokens::default()
        });
        let c = adapter.render_batch_with_surface(&state, 2).unwrap();
        assert!(
            has_set(&c),
            "a header_height_px swap must re-declare the surface"
        );
        let surface = set_portal_surface_of(&c).expect("re-declaration carries a surface");
        let header = part_of(surface, proto::PortalPartKindProto::PortalPartKindHeader);
        assert_eq!(
            header.height, 72.0,
            "re-declared header honors the new token"
        );

        // 4) Steady state again at the new geometry → no re-declaration.
        let d = adapter.render_batch_with_surface(&state, 3).unwrap();
        assert!(
            !has_set(&d),
            "new geometry is now steady; no re-declaration"
        );
    }

    /// Building the declaration before the tile is recorded is a hard error
    /// (`MissingPortalTile`) — mirrors `render_batch`.
    #[test]
    fn portal_surface_declaration_requires_recorded_tile() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let state = make_expanded_interaction_state("portal-no-tile");
        assert_eq!(
            adapter.portal_surface_declaration_mutation(&state),
            Err(ResidentGrpcAdapterError::MissingPortalTile)
        );
    }

    // ── Lifecycle affordance accent (hud-m48i0) ──────────────────────────────

    /// hud-m48i0 acceptance #1/#2/#3/#4/#5: a **non-interactive**
    /// lifecycle-visible portal paints its accent via the coalescible StateStream
    /// `SetTileLifecycleAccent` tile-update — NOT a per-republish `AddNode`:
    ///
    /// 1. each lifecycle group emits its distinct token-resolved accent color and
    ///    the token-resolved width (no literal visual constants);
    /// 2. the batch contains NO `AddNode` mutation (so `classify_inbound_batch`
    ///    keeps it StateStream — the hud-mzk74 regression cannot return);
    /// 3. the markdown node carries only zero-length sentinel `color_runs` (no
    ///    pixel-bearing runs), so the cached/styled markdown path is preserved
    ///    (#947 must not regress);
    /// 4. a redacted viewer (`lifecycle_state = None`) emits a *clearing* accent
    ///    mutation (color None / width 0) and still no `AddNode`.
    #[test]
    fn lifecycle_accent_rides_state_stream_tile_update_not_add_node() {
        fn accent_of(
            batch: &session_proto::MutationBatch,
        ) -> Option<proto::SetTileLifecycleAccentMutation> {
            batch.mutations.iter().find_map(|m| match &m.mutation {
                Some(proto::mutation_proto::Mutation::SetTileLifecycleAccent(a)) => Some(a.clone()),
                _ => None,
            })
        }
        fn has_add_node(batch: &session_proto::MutationBatch) -> bool {
            batch.mutations.iter().any(|m| {
                matches!(
                    &m.mutation,
                    Some(proto::mutation_proto::Mutation::AddNode(_))
                )
            })
        }
        fn markdown_of(batch: &session_proto::MutationBatch) -> proto::TextMarkdownNodeProto {
            for m in &batch.mutations {
                if let Some(proto::mutation_proto::Mutation::PublishToTile(p)) = &m.mutation {
                    if let Some(node) = p.node.as_ref() {
                        return text_markdown_node(node).clone();
                    }
                }
            }
            panic!("batch must publish a markdown portal node");
        }

        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);
        let tokens = adapter.visual_tokens().clone();

        let cases = [
            (
                ProjectionLifecycleState::Active,
                tokens.lifecycle_active_color,
            ),
            (
                ProjectionLifecycleState::Attached,
                tokens.lifecycle_attached_color,
            ),
            (
                ProjectionLifecycleState::Degraded,
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::HudUnavailable,
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::Detached,
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::CleanupPending,
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::Expired,
                tokens.lifecycle_inactive_color,
            ),
        ];
        for (lifecycle, expected) in cases {
            // Non-interactive: this is the portal class hud-mzk74 protected — it
            // must NOT be flipped Transactional by the accent.
            let mut state = make_expanded_interaction_state("portal-accent");
            state.interaction_enabled = false;
            state.lifecycle_state = Some(lifecycle);
            let batch = adapter
                .render_batch(&state, 0)
                .expect("render_batch must succeed");

            assert!(
                !has_add_node(&batch),
                "lifecycle {lifecycle:?}: non-interactive portal must emit NO AddNode \
                 (would flip the batch Transactional — hud-mzk74)"
            );

            let accent = accent_of(&batch).unwrap_or_else(|| {
                panic!("lifecycle {lifecycle:?} must emit a SetTileLifecycleAccent mutation")
            });
            assert_eq!(
                accent
                    .color
                    .expect("permitted lifecycle must carry a color"),
                expected,
                "lifecycle {lifecycle:?} accent must use its token-resolved color"
            );
            assert!(
                (accent.width_px - tokens.lifecycle_accent_width_px).abs() < 1e-4,
                "accent width must come from the token (no literal dimension)"
            );

            // #947 guard: the markdown node carries only zero-length sentinels.
            let tm = markdown_of(&batch);
            assert!(
                tm.color_runs.iter().all(|r| r.start_byte >= r.end_byte),
                "markdown node must carry only zero-length sentinels (no pixel runs) \
                 for lifecycle {lifecycle:?} — cached markdown path must be preserved"
            );
        }

        // The four affordance groups paint mutually distinct accents.
        let groups = [
            tokens.lifecycle_active_color,
            tokens.lifecycle_attached_color,
            tokens.lifecycle_attention_color,
            tokens.lifecycle_inactive_color,
        ];
        for i in 0..groups.len() {
            for j in (i + 1)..groups.len() {
                assert_ne!(
                    groups[i], groups[j],
                    "lifecycle accent groups {i} and {j} must be visually distinct"
                );
            }
        }

        // Redaction-gated: lifecycle_state = None emits a CLEARING accent (no
        // color, zero width) and still no AddNode.
        let mut redacted = make_expanded_interaction_state("portal-accent-redacted");
        redacted.interaction_enabled = false;
        redacted.lifecycle_state = None;
        let batch = adapter
            .render_batch(&redacted, 0)
            .expect("render_batch must succeed");
        assert!(!has_add_node(&batch));
        let accent = accent_of(&batch).expect("a clearing accent mutation is still emitted");
        assert!(
            accent.color.is_none() && accent.width_px <= 0.0,
            "redacted lifecycle must clear the accent (no color / zero width)"
        );
    }

    // ── Disconnect badge (hud-jgf41) ──────────────────────────────────────────

    fn accent_of(
        batch: &session_proto::MutationBatch,
    ) -> Option<proto::SetTileLifecycleAccentMutation> {
        batch.mutations.iter().find_map(|m| match &m.mutation {
            Some(proto::mutation_proto::Mutation::SetTileLifecycleAccent(a)) => Some(a.clone()),
            _ => None,
        })
    }

    /// A connection-degraded portal presents the visible disconnect badge — the
    /// left-edge lifecycle-accent bar recolored to `disconnect_badge_color` at
    /// `disconnect_badge_width_px` — even when `lifecycle_state` is `None`
    /// (redacted for a restricted viewer). `connection_degraded` is
    /// redaction-independent, so the badge takes precedence and stays visible
    /// (geometry-only: no transcript content or identity is disclosed by a
    /// colored bar).
    #[test]
    fn degraded_state_shows_disconnect_badge_even_when_redacted() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);
        let tokens = adapter.visual_tokens().clone();

        let mut state = make_expanded_interaction_state("portal-disconnect-badge");
        state.connection_degraded = true;
        // Simulate the authority's redaction gate: lifecycle_state cleared to
        // None, exactly as it would be for a restricted viewer.
        state.lifecycle_state = None;

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");
        let accent = accent_of(&batch)
            .expect("a degraded, redacted portal must still emit a disconnect badge accent");
        assert_eq!(
            accent.color.expect("disconnect badge must carry a color"),
            tokens.disconnect_badge_color,
            "degraded portal must show the disconnect badge color, not a cleared accent"
        );
        assert!(
            (accent.width_px - tokens.disconnect_badge_width_px).abs() < 1e-4,
            "disconnect badge width must come from the token (no literal dimension)"
        );
    }

    /// When NOT connection-degraded, the accent computation is unaffected by
    /// hud-jgf41 — the existing lifecycle-driven accent (and its redaction
    /// gating) behaves exactly as before.
    #[test]
    fn live_state_falls_back_to_ordinary_lifecycle_accent() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);
        let tokens = adapter.visual_tokens().clone();

        let mut state = make_expanded_interaction_state("portal-live-accent");
        state.connection_degraded = false;
        state.lifecycle_state = Some(ProjectionLifecycleState::Active);

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");
        let accent = accent_of(&batch).expect("a live portal must emit a lifecycle accent");
        assert_eq!(
            accent.color.expect("live lifecycle must carry a color"),
            tokens.lifecycle_active_color,
            "non-degraded portal must use the ordinary lifecycle accent, not the disconnect badge"
        );
    }

    /// Precedence: `connection_degraded` wins even if `lifecycle_state` is
    /// simultaneously `Some(Active)` (a defensive edge case — the authority
    /// should not produce this combination in practice, but the accent
    /// computation must not silently show a "live" accent while disconnected).
    #[test]
    fn disconnect_badge_takes_precedence_over_a_conflicting_lifecycle_state() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);
        let tokens = adapter.visual_tokens().clone();

        let mut state = make_expanded_interaction_state("portal-disconnect-precedence");
        state.connection_degraded = true;
        state.lifecycle_state = Some(ProjectionLifecycleState::Active);

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");
        let accent = accent_of(&batch).expect("a degraded portal must emit an accent");
        assert_eq!(
            accent.color.expect("disconnect badge must carry a color"),
            tokens.disconnect_badge_color,
            "connection_degraded must take precedence over lifecycle_state"
        );
    }

    // ── Jump-to-latest unread badge over the bridged transport (hud-hwk2m) ────

    /// The jump-to-latest pill's ambient unread badge reaches a BRIDGED portal
    /// via the wire, at parity with the in-process driver's direct
    /// `set_tile_unread_count` call (#1088 / hud-g1ena.3). `render_batch` must
    /// emit exactly one `SetTileUnreadCount` mutation each render, carrying
    /// `unread_output_count.unwrap_or(0)`:
    ///
    /// - `Some(N)` → count == N (the pill carries the badge);
    /// - `Some(0)` and a redacted `None` → count == 0 (no badge — same gating as
    ///   the in-transcript ambient indicator);
    ///
    /// and it must NOT be an `AddNode` (so `classify_inbound_batch` keeps the
    /// batch StateStream — a bridged portal stays on the coalescible path).
    #[test]
    fn jump_to_latest_unread_count_rides_state_stream_tile_update() {
        fn unread_of(
            batch: &session_proto::MutationBatch,
        ) -> Option<proto::SetTileUnreadCountMutation> {
            batch.mutations.iter().find_map(|m| match &m.mutation {
                Some(proto::mutation_proto::Mutation::SetTileUnreadCount(u)) => Some(u.clone()),
                _ => None,
            })
        }
        fn has_add_node(batch: &session_proto::MutationBatch) -> bool {
            batch.mutations.iter().any(|m| {
                matches!(
                    &m.mutation,
                    Some(proto::mutation_proto::Mutation::AddNode(_))
                )
            })
        }

        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![7u8; 16]);

        // Case 1: a non-empty ambient unread count → the pill carries the badge.
        // Non-interactive: the badge must not flip the batch Transactional.
        let mut unread = make_expanded_interaction_state("portal-unread");
        unread.interaction_enabled = false;
        unread.unread_output_count = Some(4);
        let batch = adapter
            .render_batch(&unread, 0)
            .expect("render_batch must succeed");
        let mutation = unread_of(&batch).expect("render_batch must emit a SetTileUnreadCount");
        assert_eq!(
            mutation.count, 4,
            "the bridged pill badge must carry the aggregate unread count"
        );
        assert_eq!(
            mutation.tile_id,
            vec![7u8; 16],
            "the badge mutation must target the portal's created tile"
        );
        assert!(
            !has_add_node(&batch),
            "the unread badge must not be an AddNode (would flip the batch \
             Transactional — hud-mzk74 / bridged coalescing)"
        );

        // Case 2: an empty count (0) clears the badge.
        let mut empty = make_expanded_interaction_state("portal-unread");
        empty.interaction_enabled = false;
        empty.unread_output_count = Some(0);
        let batch = adapter
            .render_batch(&empty, 0)
            .expect("render_batch must succeed");
        assert_eq!(
            unread_of(&batch).expect("mutation still emitted").count,
            0,
            "an empty unread count must clear the badge (count 0)"
        );

        // Case 3: a redacted (`None`) count also clears the badge — matching the
        // in-process arm's `unread_output_count.unwrap_or(0)` gating.
        let mut redacted = make_expanded_interaction_state("portal-unread");
        redacted.interaction_enabled = false;
        redacted.unread_output_count = None;
        let batch = adapter
            .render_batch(&redacted, 0)
            .expect("render_batch must succeed");
        assert_eq!(
            unread_of(&batch).expect("mutation still emitted").count,
            0,
            "a redacted (None) unread count must clear the badge (count 0)"
        );
    }

    // ── Composer hit region activation (hud-hxe91) ────────────────────────────

    /// render_batch must emit a coalescible `SetTileComposerInteraction` mutation
    /// carrying a HitRegionNodeProto with accepts_composer_input=true when
    /// interaction_enabled is true — NOT an `AddNode` (which would flip the batch
    /// Transactional, hud-mzk74 / hud-iofav). This is the production path that
    /// unblocks is_composer_active() in wire-driven scenes.
    #[test]
    fn render_batch_emits_composer_hit_region_when_interaction_enabled() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        // Record a fake tile_id so render_batch doesn't return MissingPortalTile.
        adapter.record_created_tile(vec![0u8; 16]);

        let state = make_expanded_interaction_state("portal-composer-test");

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed with interaction_enabled");

        // Should be 5 mutations: PublishToTile, UpdateTileInputMode,
        // SetTileLifecycleAccent (always emitted, hud-m48i0),
        // SetTileComposerInteraction (composer hit region, hud-iofav),
        // SetTileUnreadCount (always emitted last, hud-hwk2m). `render_batch` is the
        // pure raw-tile escape hatch — it emits NO first-class surface mutation
        // (those are added only by `render_batch_with_surface`, hud-rpm9s).
        assert_eq!(
            batch.mutations.len(),
            5,
            "interaction_enabled=true must produce PublishToTile + UpdateTileInputMode + \
             SetTileLifecycleAccent + SetTileComposerInteraction (composer hit region) + \
             SetTileUnreadCount"
        );
        // Crucially, NO structural AddNode — the composer rides coalescible overlay
        // state so an interaction-enabled streaming publish stays StateStream.
        assert!(
            !batch.mutations.iter().any(|m| matches!(
                &m.mutation,
                Some(tze_hud_protocol::proto::mutation_proto::Mutation::AddNode(
                    _
                ))
            )),
            "interaction-enabled render must emit NO AddNode (would flip the batch \
             Transactional — hud-mzk74 / hud-iofav)"
        );
        // No first-class surface mutation on the raw-tile path.
        assert!(
            !batch.mutations.iter().any(|m| matches!(
                &m.mutation,
                Some(
                    tze_hud_protocol::proto::mutation_proto::Mutation::UpdatePortalSurfaceState(_)
                ) | Some(tze_hud_protocol::proto::mutation_proto::Mutation::SetPortalSurface(_))
            )),
            "render_batch must emit no first-class surface mutation"
        );

        // The fourth mutation must be SetTileComposerInteraction carrying the
        // composer spec (SetTileUnreadCount is pushed last, so the composer stays at
        // index 3).
        let composer_mutation = &batch.mutations[3];
        match &composer_mutation.mutation {
            Some(
                tze_hud_protocol::proto::mutation_proto::Mutation::SetTileComposerInteraction(stci),
            ) => {
                let hr = stci
                    .composer
                    .as_ref()
                    .expect("interaction-enabled must carry a composer spec");
                assert!(
                    hr.accepts_composer_input,
                    "composer hit region must have accepts_composer_input=true"
                );
                assert!(
                    hr.accepts_focus,
                    "composer hit region must have accepts_focus=true"
                );
                // hud-v4k1h: click-to-focus requires accepts_pointer=true so
                // SceneGraph::hit_test yields a NodeHit that process_with_focus
                // can focus. A false here silently breaks pointer focus + typing.
                assert!(
                    hr.accepts_pointer,
                    "composer hit region must have accepts_pointer=true (click-to-focus)"
                );
                assert!(
                    hr.interaction_id.contains("portal-composer-test"),
                    "interaction_id must contain the portal_id: got '{}'",
                    hr.interaction_id
                );
                assert!(
                    hr.interaction_id.ends_with("-composer"),
                    "interaction_id must end with '-composer': got '{}'",
                    hr.interaction_id
                );
            }
            other => panic!(
                "Fourth mutation must be SetTileComposerInteraction (composer hit region), \
                 got {other:?}"
            ),
        }
    }

    /// hud-v4k1h resize follow-up: once a portal has been resized, the rendered
    /// body + composer hit region must size to the durable resized bounds, not
    /// the fixed config bounds. Without this the body kept rendering at the
    /// config size while the runtime grew the tile, leaving an empty
    /// "shadow-body" region in the grown tile.
    #[test]
    fn render_batch_sizes_body_to_resized_bounds_when_present() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);

        // A portal grown taller than the default expanded height AND moved to a
        // non-config origin (e.g. left/top edge drag keeps the opposite edge
        // stationary, shifting x/y).
        let grown_h = DEFAULT_EXPANDED_H + 240.0;
        let moved_x = 120.0;
        let moved_y = 80.0;
        let mut state = make_expanded_interaction_state("portal-resize-test");
        state.resized_bounds = Some(crate::AdapterPortalRect::from_f32(
            moved_x,
            moved_y,
            DEFAULT_EXPANDED_W,
            grown_h,
        ));

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed with resized bounds");

        // The PublishToTile bounds (1st mutation) must carry the resized ORIGIN
        // and size, not the static config origin — otherwise the tile snaps back
        // to the configured position on the next publish.
        match &batch.mutations[0].mutation {
            Some(tze_hud_protocol::proto::mutation_proto::Mutation::PublishToTile(pt)) => {
                let b = pt.bounds.as_ref().expect("PublishToTile must carry bounds");
                assert_eq!(b.x, moved_x, "tile bounds must keep the resized x origin");
                assert_eq!(b.y, moved_y, "tile bounds must keep the resized y origin");
                assert_eq!(b.height, grown_h, "tile bounds must use the resized height");
            }
            other => panic!("First mutation must be PublishToTile, got {other:?}"),
        }

        // The INPUT-pane composer hit region (4th mutation) must cover the grown
        // body below the token-sized header — proving the native two-pane body
        // followed the resize, not the config size.
        let composer_mutation = &batch.mutations[3];
        match &composer_mutation.mutation {
            Some(
                tze_hud_protocol::proto::mutation_proto::Mutation::SetTileComposerInteraction(stci),
            ) => {
                let hr = stci
                    .composer
                    .as_ref()
                    .expect("composer spec must be present");
                let bounds = hr.bounds.as_ref().expect("composer must carry bounds");
                let expected_body_h = grown_h - adapter.visual_tokens().header_height_px;
                assert_eq!(
                    bounds.height, expected_body_h,
                    "composer/body must size below the header to {expected_body_h}, got {}",
                    bounds.height
                );
            }
            other => panic!(
                "Fourth mutation must be SetTileComposerInteraction (composer hit region), \
                 got {other:?}"
            ),
        }

        // Sanity: an Expanded state with NO resized_bounds still uses config height.
        let mut plain = make_expanded_interaction_state("portal-noresize-test");
        plain.resized_bounds = None;
        let plain_batch = adapter
            .render_batch(&plain, 0)
            .expect("render_batch must succeed without resized bounds");
        if let Some(
            tze_hud_protocol::proto::mutation_proto::Mutation::SetTileComposerInteraction(stci),
        ) = &plain_batch.mutations[3].mutation
        {
            let hr = stci
                .composer
                .as_ref()
                .expect("composer spec must be present");
            assert_eq!(
                hr.bounds.as_ref().unwrap().height,
                DEFAULT_EXPANDED_H - adapter.visual_tokens().header_height_px,
                "without resized_bounds the body must keep the config height below the header"
            );
        } else {
            panic!("expected SetTileComposerInteraction composer mutation");
        }
    }

    /// When interaction_enabled is false, render_batch must emit a CLEARING
    /// `SetTileComposerInteraction` (absent composer) — never an `AddNode` for the
    /// composer hit region. Emitting the clear every render (mirroring the accent's
    /// redaction clear) means an enable→disable transition detaches the derived
    /// composer node while a steady-state disabled portal stays idle (hud-iofav).
    #[test]
    fn render_batch_does_not_emit_composer_hit_region_when_interaction_disabled() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);

        let mut state = make_expanded_interaction_state("portal-no-input");
        state.interaction_enabled = false;

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");

        // Should be 5 mutations: PublishToTile + UpdateTileInputMode +
        // SetTileLifecycleAccent (always emitted, hud-m48i0) +
        // SetTileComposerInteraction (a CLEAR — absent composer, hud-iofav) +
        // SetTileUnreadCount (always emitted, hud-hwk2m). No AddNode (interaction
        // disabled) and no first-class surface mutation (`render_batch` is the pure
        // raw-tile escape hatch, hud-rpm9s).
        assert_eq!(
            batch.mutations.len(),
            5,
            "interaction_enabled=false must produce exactly 5 mutations \
             (PublishToTile + UpdateTileInputMode + SetTileLifecycleAccent + \
              SetTileComposerInteraction(clear) + SetTileUnreadCount, no AddNode)"
        );
        assert!(
            !batch.mutations.iter().any(|m| matches!(
                &m.mutation,
                Some(tze_hud_protocol::proto::mutation_proto::Mutation::AddNode(
                    _
                )) | Some(
                    tze_hud_protocol::proto::mutation_proto::Mutation::UpdatePortalSurfaceState(_)
                ) | Some(tze_hud_protocol::proto::mutation_proto::Mutation::SetPortalSurface(_))
            )),
            "interaction_enabled=false must emit no AddNode and no first-class surface mutation"
        );
        // The composer mutation must be a CLEAR (absent composer spec).
        let composer_mutation = batch
            .mutations
            .iter()
            .find_map(|m| match &m.mutation {
                Some(
                    tze_hud_protocol::proto::mutation_proto::Mutation::SetTileComposerInteraction(
                        stci,
                    ),
                ) => Some(stci),
                _ => None,
            })
            .expect("disabled render must still emit a SetTileComposerInteraction (clear)");
        assert!(
            composer_mutation.composer.is_none(),
            "interaction_enabled=false must clear the composer (absent composer spec)"
        );
    }

    /// The portal root node must carry an explicit (non-empty) ID so the published
    /// transcript root has a stable, inspectable identity. Post-hud-iofav the
    /// composer hit region is no longer parented to the root via an in-batch
    /// `AddNode` (it rides `SetTileComposerInteraction` overlay state and the scene
    /// re-derives the node), so the root id no longer needs a paired big-endian
    /// parent-id encoding.
    #[test]
    fn portal_node_carries_explicit_root_id() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let mut adapter = ResidentGrpcPortalAdapter::new(config);
        adapter.record_created_tile(vec![0u8; 16]);

        let state = make_expanded_interaction_state("portal-root-id");

        let batch = adapter
            .render_batch(&state, 0)
            .expect("render_batch must succeed");

        // The PublishToTile mutation must carry a NodeProto with a non-empty ID.
        match &batch.mutations[0].mutation {
            Some(tze_hud_protocol::proto::mutation_proto::Mutation::PublishToTile(p)) => {
                let root = p.node.as_ref().expect("PublishToTile must carry a node");
                assert_eq!(
                    root.id.len(),
                    16,
                    "portal root NodeProto.id must be 16 bytes (explicit little-endian UUID)"
                );
            }
            other => panic!("First mutation must be PublishToTile, got {other:?}"),
        }
    }

    #[test]
    fn clamp_utf8_borrows_at_valid_character_boundary() {
        let text = "alpha éé omega";

        assert_eq!(clamp_utf8(text, text.len()), text);
        assert_eq!(clamp_utf8(text, 7), "alpha ");
    }

    #[test]
    fn visible_transcript_markdown_clamps_each_unit_without_collecting_lines() {
        let units = vec![
            TranscriptUnit {
                sequence: 1,
                output_text: "first".to_string(),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: None,
                coalesce_key: None,
                expects_reply: false,
                appended_at_wall_us: 1,
            },
            TranscriptUnit {
                sequence: 2,
                output_text: "é".repeat(3_000),
                output_kind: OutputKind::Assistant,
                content_classification: ContentClassification::Private,
                logical_unit_id: None,
                coalesce_key: None,
                expects_reply: false,
                appended_at_wall_us: 2,
            },
        ];

        let markdown = visible_transcript_markdown(&units, None);

        // First entry, then a turn separator, then the clamped second entry.
        assert!(markdown.starts_with("first\n---\n"));
        assert!(markdown.is_char_boundary(markdown.len()));
        // Each unit is still clamped to 4096 bytes; the separator adds a bounded
        // constant between entries.
        assert!(markdown.len() <= "first".len() + "\n---\n".len() + 4_096);
    }

    // ── §2/§3 disconnect + stale-content treatment ───────────────────────────

    /// Extract the transcript `TextMarkdownNodeProto` from either the collapsed
    /// single-node card or the expanded native two-pane subtree.
    fn text_markdown_node(node: &proto::NodeProto) -> &proto::TextMarkdownNodeProto {
        fn find(node: &proto::NodeProto) -> Option<&proto::TextMarkdownNodeProto> {
            if let Some(proto::node_proto::Data::TextMarkdown(tm)) = node.data.as_ref()
                && tm.overflow == proto::TextOverflowProto::Ellipsis as i32
            {
                return Some(tm);
            }
            node.children.iter().find_map(find)
        }
        find(node)
            .unwrap_or_else(|| panic!("portal subtree must contain a transcript TextMarkdown node"))
    }

    fn output_pane_background(node: &proto::NodeProto) -> proto::Rgba {
        node.children
            .iter()
            .find_map(|child| match child.data.as_ref() {
                Some(proto::node_proto::Data::SolidColor(solid))
                    if solid.bounds.is_some_and(|bounds| {
                        bounds.x > DEFAULT_EXPANDED_W / 2.0
                            && bounds.width > DEFAULT_EXPANDED_W / 3.0
                            && bounds.height > DEFAULT_EXPANDED_H / 2.0
                    }) =>
                {
                    solid.color
                }
                _ => None,
            })
            .expect("expanded portal must contain a token-styled OUTPUT pane")
    }

    /// §2: when the portal is connection-degraded, the expanded transcript
    /// surface uses the token-resolved DIM colors, not the live transcript
    /// colors. A control case with `connection_degraded = false` proves the live
    /// colors are used otherwise — so the difference is driven solely by the
    /// degraded flag, not by a hardcoded constant.
    #[test]
    fn degraded_state_uses_dim_transcript_colors() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let tokens = adapter.visual_tokens().clone();

        let mut live = make_expanded_interaction_state("portal-degraded-colors");
        live.connection_degraded = false;
        let live_node = adapter.portal_node(&live, vec![0u8; 16], 0);
        let live_tm = text_markdown_node(&live_node);
        assert_eq!(
            live_tm.color.unwrap(),
            tokens.transcript_text_color,
            "live portal must use the live transcript text color"
        );
        assert_eq!(
            output_pane_background(&live_node),
            tokens.transcript_background,
            "live OUTPUT pane must use the live transcript background"
        );
        assert_eq!(live_tm.background.unwrap().a, 0.0);

        let mut degraded = make_expanded_interaction_state("portal-degraded-colors");
        degraded.connection_degraded = true;
        let degraded_node = adapter.portal_node(&degraded, vec![0u8; 16], 0);
        let degraded_tm = text_markdown_node(&degraded_node);
        assert_eq!(
            degraded_tm.color.unwrap(),
            tokens.transcript_dim_text_color,
            "§2: degraded portal must use the DIM transcript text token"
        );
        assert_eq!(
            output_pane_background(&degraded_node),
            tokens.transcript_dim_background,
            "§2: degraded OUTPUT pane must use the DIM transcript background token"
        );
        assert_eq!(degraded_tm.background.unwrap().a, 0.0);
    }

    /// §2: a degraded portal emits a content-free disconnect marker line. The
    /// marker survives redaction (lifecycle_state = None, redacted = true) and
    /// reveals neither transcript content nor a lifecycle spelling.
    #[test]
    fn degraded_state_emits_content_free_disconnect_marker_under_redaction() {
        let mut state = make_expanded_interaction_state("portal-marker");
        state.connection_degraded = true;
        // Restricted viewer: identity/transcript/lifecycle all redacted.
        state.redacted = true;
        state.lifecycle_state = None;
        state.display_name = None;
        state.visible_transcript = vec![];

        let markdown = portal_markdown(&state, None, 0);

        assert!(
            markdown.contains(PORTAL_DISCONNECT_MARKER_LINE),
            "§2: disconnect marker must remain present under redaction"
        );
        // No lifecycle spelling leaks (the `status:` line is redaction-gated).
        assert!(
            !markdown.contains("status:"),
            "redaction-gated lifecycle spelling must NOT appear"
        );

        // A non-degraded state must NOT show the marker.
        let mut live = make_expanded_interaction_state("portal-marker");
        live.connection_degraded = false;
        let live_md = portal_markdown(&live, None, 0);
        assert!(
            !live_md.contains(PORTAL_DISCONNECT_MARKER_LINE),
            "live portal must not show the disconnect marker"
        );
    }

    /// §2: the stale marker color is carried as a token-driven sentinel color run
    /// when degraded (no literal color in the render path), and absent otherwise.
    #[test]
    fn degraded_state_emits_stale_marker_color_run() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let stale = adapter.visual_tokens().stale_marker_color;

        let mut degraded = make_expanded_interaction_state("portal-stale-run");
        degraded.connection_degraded = true;
        let runs = stale_marker_color_runs(&degraded, stale);
        assert_eq!(
            runs.len(),
            1,
            "degraded state must emit one stale color run"
        );
        assert_eq!(runs[0].color.unwrap(), stale, "run must carry stale token");
        assert_eq!(runs[0].start_byte, 0);
        assert_eq!(runs[0].end_byte, 0);

        let mut live = make_expanded_interaction_state("portal-stale-run");
        live.connection_degraded = false;
        assert!(
            stale_marker_color_runs(&live, stale).is_empty(),
            "live state must emit no stale color run"
        );
    }

    /// hud-meqet: `unread_output_count` was already threaded (un-nulled)
    /// through to `ProjectedPortalState` by the authority, but the render
    /// path never read it, so it was never drawn. A nonzero, visible count
    /// must emit an ambient text-item line ("N unread") plus a token-driven
    /// sentinel color run (mirroring the stale-marker/composer Phase-1
    /// convention); zero or redacted (`None`) must emit neither — a presence
    /// engine renders nothing rather than a "0 unread" line or a loud badge.
    #[test]
    fn unread_output_count_renders_ambient_indicator_only_when_nonzero() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let unread_color = adapter.visual_tokens().unread_indicator_color;

        let mut present = make_expanded_interaction_state("portal-badge");
        present.unread_output_count = Some(3);
        let markdown = portal_markdown(&present, None, 0);
        assert!(
            markdown.contains("3 unread"),
            "nonzero unread count must render an ambient text-item indicator: {markdown}"
        );
        let runs = unread_indicator_color_runs(&present, unread_color);
        assert_eq!(
            runs.len(),
            1,
            "nonzero unread count must emit one token-driven color run"
        );
        assert_eq!(
            runs[0].color.unwrap(),
            unread_color,
            "run must carry the unread_indicator_color token, never a literal color"
        );
        assert_eq!(runs[0].start_byte, 0);
        assert_eq!(runs[0].end_byte, 0);

        let mut zero = make_expanded_interaction_state("portal-badge");
        zero.unread_output_count = Some(0);
        let zero_markdown = portal_markdown(&zero, None, 0);
        assert!(
            !zero_markdown.contains("unread"),
            "zero unread count must render nothing: {zero_markdown}"
        );
        assert!(
            unread_indicator_color_runs(&zero, unread_color).is_empty(),
            "zero unread count must emit no color run"
        );

        let mut redacted = make_expanded_interaction_state("portal-badge");
        redacted.unread_output_count = None;
        let redacted_markdown = portal_markdown(&redacted, None, 0);
        assert!(
            !redacted_markdown.contains("unread"),
            "redacted (None) unread count must render nothing: {redacted_markdown}"
        );
        assert!(
            unread_indicator_color_runs(&redacted, unread_color).is_empty(),
            "redacted (None) unread count must emit no color run"
        );
    }

    /// The `unread_indicator_color` `PortalVisualTokens` field maps from
    /// the source `PortalPartTokens` channel (single-source-of-truth
    /// invariant, same as the other token-mapping tests in this module).
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_unread_indicator_color() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.unread_indicator_color.r)
                - expected_linear(part.unread_indicator_color.r))
            .abs()
                < 1e-6
        );
        assert_eq!(
            visual.unread_indicator_color.a,
            part.unread_indicator_color.a
        );
    }

    /// The `awaiting_reply_color` `PortalVisualTokens` field maps from the
    /// source `PortalPartTokens` channel (hud-jip0k), same single-source-of-truth
    /// invariant as the other token-mapping tests in this module.
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_awaiting_reply_color() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.awaiting_reply_color.r)
                - expected_linear(part.awaiting_reply_color.r))
            .abs()
                < 1e-6
        );
        assert_eq!(visual.awaiting_reply_color.a, part.awaiting_reply_color.a);
    }

    // ── Unread divider (hud-g1ena.2, §Unread Divider and Ambient Unread Count) ──

    /// A `TranscriptUnit` with explicit text + kind for divider-placement tests.
    fn transcript_unit_text(sequence: u64, output_kind: OutputKind, text: &str) -> TranscriptUnit {
        TranscriptUnit {
            sequence,
            output_text: text.to_string(),
            output_kind,
            content_classification: ContentClassification::Private,
            logical_unit_id: None,
            coalesce_key: None,
            expects_reply: false,
            appended_at_wall_us: sequence,
        }
    }

    // ── Per-turn role attribution (§Conversational Turn Model, hud-26869) ──────

    /// Find the byte offset of `needle` in `haystack`, or panic — used to locate a
    /// turn's unique text within the assembled node content.
    fn offset_of(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("content must contain turn text {needle:?}"))
    }

    /// Whether a run addresses exactly the byte range `[start, start+len)` with the
    /// given color.
    fn run_covers(
        run: &proto::TextColorRunProto,
        start: usize,
        len: usize,
        color: proto::Rgba,
    ) -> bool {
        run.start_byte as usize == start
            && run.end_byte as usize == start + len
            && run.color == Some(color)
    }

    /// `visible_transcript_markdown_spans` string output is byte-identical to
    /// `visible_transcript_markdown_with`, and each span's `[start, end)` slices
    /// out exactly that turn's text (separators/timestamps/divider excluded).
    #[test]
    fn transcript_spans_slice_each_turns_text_and_match_string() {
        let units = vec![
            transcript_unit_text(1, OutputKind::Assistant, "hello from the model"),
            transcript_unit_text(2, OutputKind::Status, "connected to backend"),
            transcript_unit_text(3, OutputKind::Tool, "ran grep, 4 hits"),
        ];
        let plain = visible_transcript_markdown_with(&units, None, TimestampGranularity::Off);
        let (body, spans) =
            visible_transcript_markdown_spans(&units, None, TimestampGranularity::Off);
        assert_eq!(
            body, plain,
            "spans variant must not change the string output"
        );
        assert_eq!(spans.len(), units.len());
        for (span, unit) in spans.iter().zip(units.iter()) {
            assert_eq!(
                &body[span.start..span.end],
                unit.output_text,
                "span must slice exactly the turn's own text"
            );
            assert_eq!(span.output_kind, unit.output_kind);
        }
    }

    /// The role predicate: assistant (and the never-here viewer) use the base
    /// color; tool/status/error/other use the attribution color.
    #[test]
    fn turn_uses_system_attribution_classifies_kinds() {
        assert!(!turn_uses_system_attribution(OutputKind::Assistant));
        assert!(!turn_uses_system_attribution(OutputKind::Viewer));
        assert!(turn_uses_system_attribution(OutputKind::Tool));
        assert!(turn_uses_system_attribution(OutputKind::Status));
        assert!(turn_uses_system_attribution(OutputKind::Error));
        assert!(turn_uses_system_attribution(OutputKind::Other));
    }

    /// `portal_node` emits a real attribution color-run span over each
    /// non-assistant turn's text carrying the `transcript_system_color` token,
    /// and NONE over the assistant turn (which inherits the base color). The base
    /// node color stays the assistant transcript color, and the node stays a
    /// single flat node (no per-turn child fan-out → coalescing preserved).
    #[test]
    fn portal_node_attributes_non_assistant_turns_with_token_span() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let system_color = adapter.visual_tokens().transcript_system_color;
        let base_color = adapter.visual_tokens().transcript_text_color;

        let mut state = make_expanded_interaction_state("portal-attribution");
        state.visible_transcript = vec![
            transcript_unit_text(1, OutputKind::Assistant, "ASSISTANT_PROSE_ZZ"),
            transcript_unit_text(2, OutputKind::Status, "STATUS_LINE_ZZ"),
            transcript_unit_text(3, OutputKind::Tool, "TOOL_OUTPUT_ZZ"),
        ];

        let node = adapter.portal_node(&state, vec![0u8; 16], 0);
        let tm = text_markdown_node(&node);
        assert_eq!(
            tm.color,
            Some(base_color),
            "base node color stays assistant color"
        );

        let sys_runs: Vec<_> = tm
            .color_runs
            .iter()
            .filter(|r| r.color == Some(system_color) && r.end_byte > r.start_byte)
            .collect();
        assert_eq!(
            sys_runs.len(),
            2,
            "exactly the two non-assistant turns get an attribution span, got {sys_runs:?}"
        );

        let status_off = offset_of(&tm.content, "STATUS_LINE_ZZ");
        let tool_off = offset_of(&tm.content, "TOOL_OUTPUT_ZZ");
        assert!(
            sys_runs.iter().any(|r| run_covers(
                r,
                status_off,
                "STATUS_LINE_ZZ".len(),
                system_color
            )),
            "status turn must be span-attributed with the system token"
        );
        assert!(
            sys_runs
                .iter()
                .any(|r| run_covers(r, tool_off, "TOOL_OUTPUT_ZZ".len(), system_color)),
            "tool turn must be span-attributed with the system token"
        );

        // The assistant turn's text must NOT be covered by any system-color run.
        let assistant_off = offset_of(&tm.content, "ASSISTANT_PROSE_ZZ");
        assert!(
            !sys_runs.iter().any(|r| run_covers(
                r,
                assistant_off,
                "ASSISTANT_PROSE_ZZ".len(),
                system_color
            )),
            "assistant prose must inherit the base color, not the attribution span"
        );
    }

    /// An all-assistant transcript produces no attribution spans (nothing to
    /// distinguish), so the model's own prose is never over-decorated.
    #[test]
    fn portal_node_all_assistant_transcript_has_no_attribution_runs() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let system_color = adapter.visual_tokens().transcript_system_color;

        let mut state = make_expanded_interaction_state("portal-attribution-none");
        state.visible_transcript = vec![
            transcript_unit_text(1, OutputKind::Assistant, "first turn"),
            transcript_unit_text(2, OutputKind::Assistant, "second turn"),
        ];
        let node = adapter.portal_node(&state, vec![0u8; 16], 0);
        let tm = text_markdown_node(&node);
        assert!(
            !tm.color_runs
                .iter()
                .any(|r| r.color == Some(system_color) && r.end_byte > r.start_byte),
            "no attribution span for an all-assistant transcript"
        );
    }

    /// An empty transcript (the redaction/empty case — the authority empties
    /// `visible_transcript` under redaction) produces no attribution spans, so
    /// attribution suppresses with the transcript.
    #[test]
    fn turn_attribution_absent_for_empty_transcript() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let system_color = adapter.visual_tokens().transcript_system_color;

        let mut state = make_expanded_interaction_state("portal-attribution-empty");
        state.visible_transcript = vec![];
        let node = adapter.portal_node(&state, vec![0u8; 16], 0);
        let tm = text_markdown_node(&node);
        assert!(
            !tm.color_runs
                .iter()
                .any(|r| r.color == Some(system_color) && r.end_byte > r.start_byte),
            "no attribution span when the transcript is empty"
        );
    }

    /// `turn_attribution_color_runs` clamps spans to the (truncated) content
    /// length: a span starting past `content_len` is dropped, and one straddling
    /// the cut is trimmed to it, so a run never addresses bytes the node omits.
    #[test]
    fn turn_attribution_runs_clamp_to_content_len() {
        let color = proto::Rgba {
            r: 0.5,
            g: 0.6,
            b: 0.7,
            a: 1.0,
        };
        let spans = vec![
            TranscriptTurnSpan {
                start: 0,
                end: 20,
                output_kind: OutputKind::Status,
            },
            TranscriptTurnSpan {
                start: 30,
                end: 40,
                output_kind: OutputKind::Tool,
            },
        ];
        // content_len=25 trims the first run to [0,25) and drops the second.
        let runs = turn_attribution_color_runs(&spans, color, 25);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start_byte, 0);
        assert_eq!(runs[0].end_byte, 20);

        let runs = turn_attribution_color_runs(&spans, color, 10);
        assert_eq!(runs.len(), 1, "first trimmed to [0,10), second dropped");
        assert_eq!(runs[0].end_byte, 10);
    }

    /// No unread content → no divider boundary. Covers both the redacted /
    /// not-revealed count (`None`) and the nothing-unread count (`Some(0)`), which
    /// is exactly how the divider clears locally when the viewer views the tail.
    #[test]
    fn unread_divider_boundary_is_none_without_unread() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false),
            transcript_unit(2, OutputKind::Assistant, false),
        ];
        state.visible_unread_output_count = None;
        assert_eq!(unread_divider_boundary(&state), None);
        state.visible_unread_output_count = Some(0);
        assert_eq!(unread_divider_boundary(&state), None);
    }

    /// The divider sits before the oldest retained unseen agent-authored turn.
    #[test]
    fn unread_divider_boundary_marks_oldest_unseen_agent_turn() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false), // seen
            transcript_unit(2, OutputKind::Assistant, false), // unseen
            transcript_unit(3, OutputKind::Assistant, false), // unseen
        ];
        state.visible_unread_output_count = Some(2);
        assert_eq!(unread_divider_boundary(&state), Some(1));
    }

    /// Echoed viewer turns are never unread (Viewer Reply Echo): a viewer echo
    /// interleaved with agent turns is skipped when counting the boundary.
    #[test]
    fn unread_divider_boundary_skips_viewer_echo() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false),
            transcript_unit(2, OutputKind::Viewer, false),
            transcript_unit(3, OutputKind::Assistant, false),
        ];
        state.visible_unread_output_count = Some(1);
        // Only the newest agent turn (index 2) is unread; the viewer echo at
        // index 1 is skipped, so the divider does not land on it.
        assert_eq!(unread_divider_boundary(&state), Some(2));
    }

    /// When older unread turns have coalesced or scrolled out of the bounded
    /// window, the count MAY exceed the retained agent turns; the divider then
    /// sits at the oldest RETAINED unseen (agent) unit, not on a viewer echo.
    #[test]
    fn unread_divider_boundary_clamps_to_oldest_retained_when_count_exceeds_window() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Viewer, false),
            transcript_unit(2, OutputKind::Assistant, false),
            transcript_unit(3, OutputKind::Assistant, false),
        ];
        state.visible_unread_output_count = Some(9);
        assert_eq!(unread_divider_boundary(&state), Some(1));
    }

    /// Only viewer echoes retained → nothing is ever unread, so no divider.
    #[test]
    fn unread_divider_boundary_is_none_without_agent_turn() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![transcript_unit(1, OutputKind::Viewer, false)];
        state.visible_unread_output_count = Some(3);
        assert_eq!(unread_divider_boundary(&state), None);
    }

    /// §Viewer Reply Echo (two-pane INPUT/OUTPUT split): the INPUT history renders
    /// as a distinct labeled band held SEPARATELY from the OUTPUT transcript body —
    /// the agent turn stays above, the viewer replies stack below their own label,
    /// and the two never interleave into one combined sequence. The composer
    /// status line renders LAST, after the band (hud-0e44r): this whole Expanded
    /// arm is one `TailAnchored` markdown blob, so the trailing line is the one
    /// line guaranteed to survive truncation when `input_history` overflows the
    /// tile — see the render-site comment for the full rationale.
    #[test]
    fn portal_markdown_renders_input_band_separate_from_output_transcript() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);

        let mut state = make_expanded_interaction_state("portal-two-pane");
        state.visible_transcript = vec![transcript_unit_text(
            1,
            OutputKind::Assistant,
            "agent says hi",
        )];
        state.visible_transcript_bytes = state
            .visible_transcript
            .iter()
            .map(|u| u.output_text.len())
            .sum();
        state.input_history = vec![
            transcript_unit_text(2, OutputKind::Viewer, "viewer reply one"),
            transcript_unit_text(3, OutputKind::Viewer, "viewer reply two"),
        ];

        let md = adapter.render_portal_markdown(&state, 0);

        assert!(md.contains("agent says hi"), "{md}");
        assert!(md.contains("composer: ready"), "{md}");
        assert!(md.contains(PORTAL_INPUT_HISTORY_LABEL), "{md}");
        assert!(
            md.contains("viewer reply one") && md.contains("viewer reply two"),
            "{md}"
        );
        // Ordering proves separation AND tail-anchored placement: the OUTPUT
        // transcript body renders first, then the INPUT band label, then the
        // viewer replies, then the composer status line LAST — no viewer text is
        // interleaved into the agent transcript body, and the composer always
        // trails the band so it survives TailAnchored truncation (hud-0e44r).
        let agent_pos = md.find("agent says hi").expect("agent turn rendered");
        let label_pos = md
            .find(PORTAL_INPUT_HISTORY_LABEL)
            .expect("input band label rendered");
        let viewer_pos = md.find("viewer reply one").expect("viewer reply rendered");
        let composer_pos = md
            .find("composer: ready")
            .expect("composer status line rendered");
        assert!(
            agent_pos < label_pos && label_pos < viewer_pos && viewer_pos < composer_pos,
            "{md}"
        );
        // Viewer replies stack with a token-styled `---` turn divider between them.
        assert!(
            md.contains("viewer reply one\n---\nviewer reply two"),
            "{md}"
        );
    }

    /// hud-0e44r regression: the composer status line must stay visible (the
    /// LAST line of the Expanded arm, hence the `TailAnchored` truncation-safe
    /// tail) no matter how much `input_history` precedes it. Before this fix the
    /// composer status rendered BEFORE the INPUT-history band, so a long history
    /// pushed it past the compositor's `TruncationViewport::TailAnchored` window
    /// on an overflowing tile. This test can't drive the GPU truncation directly
    /// (no compositor here), but it asserts the structural invariant that makes
    /// truncation-safety possible: the composer status line is always the LAST
    /// line emitted, strictly after every INPUT-history entry, regardless of how
    /// many entries there are.
    #[test]
    fn composer_status_line_stays_last_under_large_input_history() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);

        let mut state = make_expanded_interaction_state("portal-overflow");
        state.visible_transcript = vec![transcript_unit_text(
            1,
            OutputKind::Assistant,
            "agent says hi",
        )];
        state.visible_transcript_bytes = state
            .visible_transcript
            .iter()
            .map(|u| u.output_text.len())
            .sum();
        // A large INPUT-history band — enough entries that, before hud-0e44r,
        // the composer status line (positioned BEFORE the band) would have been
        // pushed far outside any tail-anchored truncation window.
        state.input_history = (0..100)
            .map(|i| {
                transcript_unit_text(
                    100 + i,
                    OutputKind::Viewer,
                    &format!("viewer reply number {i} with some padding text"),
                )
            })
            .collect();

        let md = adapter.render_portal_markdown(&state, 0);

        let last_history_pos = md
            .rfind("viewer reply number 99")
            .expect("last input-history entry rendered");
        let composer_pos = md
            .find("composer: ready")
            .expect("composer status line rendered");
        assert!(
            composer_pos > last_history_pos,
            "composer status line must render AFTER every INPUT-history entry so it \
             is the tail-anchored-safe last line: {md}"
        );
        // And it must be the trailing content overall — no other line follows
        // it. Since hud-ycurc the input-status lines (`pending_input_count` /
        // `last_input_feedback`) are emitted BEFORE the composer status, so
        // nothing legitimately follows it in the Expanded arm (see
        // `composer_status_line_stays_last_with_pending_input_and_feedback`).
        assert!(
            md.trim_end().ends_with("composer: ready"),
            "composer status line must be the LAST line of the rendered markdown: {md}"
        );
    }

    /// hud-ycurc regression: the composer status line must remain the LAST line
    /// of the Expanded arm even when `pending_input_count` and
    /// `last_input_feedback` are populated. Before this fix those lines were
    /// emitted after the presentation match — i.e. AFTER the composer status —
    /// so they became the `TailAnchored` truncation-safe tail instead of the
    /// composer status, weakening hud-0e44r's always-visible-composer guarantee.
    #[test]
    fn composer_status_line_stays_last_with_pending_input_and_feedback() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);

        let mut state = make_expanded_interaction_state("portal-input-status");
        state.visible_transcript = vec![transcript_unit_text(
            1,
            OutputKind::Assistant,
            "agent says hi",
        )];
        state.pending_input_count = Some(3);
        state.last_input_feedback = Some(PortalInputFeedback {
            projection_id: "portal-input-status".to_string(),
            input_id: "input-1".to_string(),
            feedback_state: PortalInputFeedbackState::Accepted,
            error_code: None,
            pending_input_count: 0,
            pending_input_bytes: 0,
            status_summary: String::new(),
        });

        let md = adapter.render_portal_markdown(&state, 0);

        // Both input-status lines render...
        let pending_pos = md
            .find("pending HUD input: 3")
            .expect("pending-input line rendered");
        let feedback_pos = md.find("✓ sent").expect("feedback line rendered");
        let composer_pos = md
            .find("composer: ready")
            .expect("composer status line rendered");
        // ...BEFORE the composer status line...
        assert!(
            pending_pos < composer_pos && feedback_pos < composer_pos,
            "input-status lines must precede the composer status line: {md}"
        );
        // ...with pending-input preceding feedback (push_input_status_lines'
        // fixed relative order, unchanged by hud-ycurc — only their position
        // relative to the composer status line moved)...
        assert!(
            pending_pos < feedback_pos,
            "pending-input line must precede the feedback line: {md}"
        );
        // ...and the composer status is still the final line.
        assert!(
            md.trim_end().ends_with("composer: ready"),
            "composer status line must be the LAST line even with pending/feedback: {md}"
        );
    }

    // ── hud-c64bn: tail-reserved wire truncation ──────────────────────────────
    //
    // `apply_tail_reserved_truncation` is tested directly at the unit level
    // (precise, deterministic — avoids fighting the full render pipeline's
    // many ambient-line conditionals for an exact-string match) plus two
    // integration-level tests through the real adapter to prove the
    // end-to-end behavior that matters: composer/input-status survival when
    // a transcript genuinely exceeds the 16KB cap, and span consistency
    // under that same truncation.

    #[test]
    fn apply_tail_reserved_truncation_is_byte_identical_no_op_when_under_cap() {
        let head = "head line one\nhead line two";
        let tail = "\ntail-critical line";
        let input = format!("{head}{tail}");
        let tail_start = head.len();
        let mut spans = vec![TranscriptTurnSpan {
            start: 0,
            end: 4,
            output_kind: OutputKind::Tool,
        }];
        let spans_before = spans.clone();
        let max_bytes = input.len() + 100; // comfortably under cap
        let out = apply_tail_reserved_truncation(input.clone(), tail_start, max_bytes, &mut spans);
        assert_eq!(
            out, input,
            "under-cap output must be byte-identical to input"
        );
        assert_eq!(spans, spans_before, "under-cap spans must be untouched");
    }

    #[test]
    fn apply_tail_reserved_truncation_reserves_tail_and_truncates_only_head_when_over_cap() {
        // 20-byte head, 10-byte tail, budget 15 -> head must shrink to 5 bytes
        // (15 - 10), tail must survive completely unmodified.
        let head = "01234567890123456789"; // 20 bytes
        let tail = "TAIL123456"; // 10 bytes
        let input = format!("{head}{tail}");
        let tail_start = head.len();
        let mut spans = vec![
            // Fully within the surviving head (bytes [0, 5)) -> kept, unclamped.
            TranscriptTurnSpan {
                start: 0,
                end: 3,
                output_kind: OutputKind::Tool,
            },
            // Straddles the cut (starts before byte 5, ends after) -> clamped to [3, 5).
            TranscriptTurnSpan {
                start: 3,
                end: 8,
                output_kind: OutputKind::Status,
            },
            // Entirely beyond the cut -> dropped.
            TranscriptTurnSpan {
                start: 10,
                end: 15,
                output_kind: OutputKind::Error,
            },
        ];
        let max_bytes = 15;
        let out = apply_tail_reserved_truncation(input, tail_start, max_bytes, &mut spans);
        assert_eq!(
            out, "01234TAIL123456",
            "head truncated to 5 bytes, tail verbatim"
        );
        assert!(
            out.ends_with(tail),
            "tail must be exact and at the end: {out}"
        );
        assert_eq!(
            out.len(),
            max_bytes,
            "output must land exactly at the budget here"
        );
        assert_eq!(
            spans,
            vec![
                TranscriptTurnSpan {
                    start: 0,
                    end: 3,
                    output_kind: OutputKind::Tool,
                },
                TranscriptTurnSpan {
                    start: 3,
                    end: 5,
                    output_kind: OutputKind::Status,
                },
            ],
            "span straddling the cut clamped to head_cut, span beyond it dropped"
        );
    }

    #[test]
    fn apply_tail_reserved_truncation_degenerate_tail_alone_exceeds_budget_head_wins_nothing() {
        let head = "some head content that would normally be kept";
        let tail = "TAIL-CRITICAL-CONTENT-LONGER-THAN-BUDGET";
        let input = format!("{head}{tail}");
        let tail_start = head.len();
        let mut spans = vec![TranscriptTurnSpan {
            start: 0,
            end: 5,
            output_kind: OutputKind::Tool,
        }];
        // Budget smaller than the tail alone.
        let max_bytes = tail.len() - 5;
        let out = apply_tail_reserved_truncation(input, tail_start, max_bytes, &mut spans);
        // Policy (hud-c64bn): tail wins outright, head is empty, total may
        // exceed max_bytes rather than the tail itself being cut.
        assert_eq!(out, tail, "tail must survive completely uncut, head empty");
        assert!(
            out.len() > max_bytes,
            "degenerate case is allowed to exceed the budget: out.len()={} max_bytes={max_bytes}",
            out.len()
        );
        assert!(
            spans.is_empty(),
            "every span must be dropped when the head is empty"
        );
    }

    #[test]
    fn portal_markdown_composer_and_input_status_survive_verbatim_when_transcript_exceeds_cap() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let mut state = make_expanded_interaction_state("portal-overcap");
        // ~200 bytes/turn x 150 turns ~= 30KB, comfortably over the 16KB cap.
        state.visible_transcript = (0..150)
            .map(|i| {
                transcript_unit_text(
                    i,
                    OutputKind::Assistant,
                    &format!(
                        "turn number {i} with enough padding text to make each turn roughly two \
                         hundred bytes long so a few dozen of these blow well past the cap"
                    ),
                )
            })
            .collect();
        state.pending_input_count = Some(7);
        state.last_input_feedback = Some(PortalInputFeedback {
            projection_id: "portal-overcap".to_string(),
            input_id: "input-1".to_string(),
            feedback_state: PortalInputFeedbackState::Accepted,
            error_code: None,
            pending_input_count: 0,
            pending_input_bytes: 0,
            status_summary: String::new(),
        });

        let md = adapter.render_portal_markdown(&state, 0);

        assert!(
            md.len() <= MAX_PORTAL_MARKDOWN_BYTES,
            "output must respect the wire cap: {} > {MAX_PORTAL_MARKDOWN_BYTES}",
            md.len()
        );
        // Sanity: the cap actually engaged for this fixture (otherwise this
        // test would prove nothing about the truncation path). The head
        // truncation is PREFIX-KEEPING (`clamp_utf8` keeps `text[..cut]`), so
        // the EARLIEST turn survives and the LATEST turns are what get
        // trimmed — the inverse of what a naive whole-blob TailAnchored
        // reading might suggest.
        assert!(
            md.contains("turn number 0 "),
            "earliest turn must survive prefix-keeping head truncation: {}",
            md.len()
        );
        assert!(
            !md.contains("turn number 149 "),
            "fixture must be large enough that the LATEST turns are trimmed \
             from the head: {}",
            md.len()
        );
        // The tail-critical content must survive completely intact and in
        // the correct relative order, exactly as under-cap.
        let pending_pos = md
            .find("pending HUD input: 7")
            .expect("pending-input line must survive the wire cap verbatim");
        let feedback_pos = md
            .find("✓ sent")
            .expect("feedback line must survive the wire cap verbatim");
        let composer_pos = md
            .find("composer: ready")
            .expect("composer status line must survive the wire cap verbatim");
        assert!(pending_pos < feedback_pos && feedback_pos < composer_pos);
        assert!(
            md.trim_end().ends_with("composer: ready"),
            "composer status must still be the trailing line under the wire cap: {md}"
        );
    }

    #[test]
    fn portal_markdown_turn_spans_stay_within_bounds_when_transcript_exceeds_cap() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let mut state = make_expanded_interaction_state("portal-overcap-spans");
        // Every turn is non-assistant (Tool), so every retained turn gets an
        // attribution span (turn_uses_system_attribution) — maximizes the
        // number of spans that could be corrupted by the truncation splice.
        state.visible_transcript = (0..150)
            .map(|i| {
                transcript_unit_text(
                    i,
                    OutputKind::Tool,
                    &format!(
                        "tool output {i} with enough padding text to make each turn roughly two \
                         hundred bytes long so a few dozen of these blow well past the cap"
                    ),
                )
            })
            .collect();

        let (md, turn_spans) = portal_markdown_with_spans(
            &state,
            adapter.composer_display.as_ref(),
            0,
            adapter.visual_tokens.timestamp_granularity,
        );

        assert!(md.len() <= MAX_PORTAL_MARKDOWN_BYTES);
        assert!(
            !turn_spans.is_empty(),
            "fixture must retain at least some attributed turns after truncation"
        );
        for span in &turn_spans {
            assert!(
                span.start < span.end,
                "no degenerate/empty span must survive: {span:?}"
            );
            assert!(
                span.end <= md.len(),
                "span must not address bytes beyond the (possibly truncated) content: \
                 span={span:?} content_len={}",
                md.len()
            );
            // Every surviving span must still slice to valid UTF-8 and land
            // inside the retained transcript body — not inside the
            // tail-critical pending/feedback/composer content that was
            // spliced on afterward.
            let text = &md[span.start..span.end];
            assert!(
                text.contains("tool output"),
                "span must still address real transcript text, not spliced tail content: \
                 span={span:?} text={text:?}"
            );
        }
    }

    #[test]
    fn portal_output_markdown_keeps_newest_turn_when_formatting_exceeds_wire_cap() {
        let mut state = make_expanded_interaction_state("portal-output-overcap");
        state.visible_transcript = (0..4)
            .map(|i| {
                let marker = if i == 0 {
                    "OLDEST-HEAD"
                } else if i == 3 {
                    "NEWEST-TAIL"
                } else {
                    "MIDDLE-TURN"
                };
                let mut text = "x".repeat(4_096 - marker.len());
                if i == 3 {
                    text.push_str(marker);
                } else {
                    text.insert_str(0, marker);
                }
                transcript_unit_text(i, OutputKind::Assistant, &text)
            })
            .collect();

        let (md, spans) = portal_output_markdown_with_spans(&state, 0, TimestampGranularity::Off);

        assert!(md.len() <= MAX_PORTAL_MARKDOWN_BYTES);
        assert!(
            md.contains("NEWEST-TAIL"),
            "tail-anchored OUTPUT must retain the newest turn when separator or timestamp \
             formatting pushes the authority-bounded transcript over the wire cap"
        );
        assert!(
            !md.contains("OLDEST-HEAD"),
            "the oldest head, not the newest tail, must be evicted under overflow"
        );
        assert!(spans.iter().all(|span| span.end <= md.len()));
    }

    /// The INPUT band is absent when there is no INPUT history — including under
    /// redaction, where `input_history` is emptied upstream exactly like
    /// `visible_transcript`, so neither the label nor any viewer reply renders.
    #[test]
    fn portal_markdown_input_band_absent_when_input_history_empty() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let mut state = make_expanded_interaction_state("portal-empty-input");
        state.visible_transcript =
            vec![transcript_unit_text(1, OutputKind::Assistant, "agent only")];
        // input_history defaults empty (as it would under redaction).
        let md = adapter.render_portal_markdown(&state, 0);
        assert!(!md.contains(PORTAL_INPUT_HISTORY_LABEL), "{md}");
    }

    /// The divider marker line is inserted before the boundary turn, distinct
    /// from the ordinary `---` turn separator, and never appears without a
    /// boundary.
    #[test]
    fn visible_transcript_markdown_inserts_unread_divider_before_boundary() {
        let units = vec![
            transcript_unit_text(1, OutputKind::Assistant, "alpha"),
            transcript_unit_text(2, OutputKind::Assistant, "bravo"),
        ];
        let md = visible_transcript_markdown(&units, Some(1));
        assert_eq!(
            md,
            format!("alpha\n---\n{PORTAL_UNREAD_DIVIDER_LINE}\nbravo")
        );
        let plain = visible_transcript_markdown(&units, None);
        assert!(!plain.contains(PORTAL_UNREAD_DIVIDER_LINE));
    }

    /// The divider color run is a zero-length token sentinel (§6.1: token-driven,
    /// no literal color) and clears when the count clears on tail view.
    #[test]
    fn unread_divider_color_runs_emit_zero_length_token_sentinel() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let divider_color = adapter.visual_tokens().unread_divider_color;

        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false),
            transcript_unit(2, OutputKind::Assistant, false),
        ];
        state.visible_unread_output_count = Some(1);

        let runs = unread_divider_color_runs(&state, divider_color);
        assert_eq!(
            runs.len(),
            1,
            "an unread agent turn must emit one divider run"
        );
        assert_eq!(
            runs[0].start_byte, 0,
            "divider run must be a zero-length sentinel"
        );
        assert_eq!(
            runs[0].end_byte, 0,
            "divider run must be a zero-length sentinel"
        );
        assert_eq!(
            runs[0].color.unwrap(),
            divider_color,
            "run must carry the unread_divider_color token, never a literal color"
        );

        state.visible_unread_output_count = None;
        assert!(
            unread_divider_color_runs(&state, divider_color).is_empty(),
            "the divider run must clear when the count clears on tail view"
        );
    }

    /// The in-transcript divider is an expanded-only affordance and needs
    /// transcript content to divide: absent in collapsed and on an empty window.
    #[test]
    fn unread_divider_color_runs_absent_in_collapsed_and_empty() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let divider_color = adapter.visual_tokens().unread_divider_color;

        let mut collapsed = make_expanded_interaction_state("portal-unread");
        collapsed.presentation = ProjectedPortalPresentation::Collapsed;
        collapsed.visible_transcript = vec![transcript_unit(1, OutputKind::Assistant, false)];
        collapsed.visible_unread_output_count = Some(1);
        assert!(unread_divider_color_runs(&collapsed, divider_color).is_empty());

        let mut empty = make_expanded_interaction_state("portal-unread");
        empty.visible_unread_output_count = Some(1);
        assert!(unread_divider_color_runs(&empty, divider_color).is_empty());
    }

    /// End-to-end through `portal_markdown`: an unread agent turn renders the
    /// divider, and viewing the tail (count → `None`) clears it locally with no
    /// adapter round trip.
    #[test]
    fn portal_markdown_renders_unread_divider_and_clears_on_tail_view() {
        let mut state = make_expanded_interaction_state("portal-unread");
        state.visible_transcript = vec![
            transcript_unit_text(1, OutputKind::Assistant, "old turn"),
            transcript_unit_text(2, OutputKind::Assistant, "new turn"),
        ];
        // Non-redacted viewer: the aggregate and clearance-corrected counts match.
        state.unread_output_count = Some(1);
        state.visible_unread_output_count = Some(1);
        // Render past the agent-activity quiesce window (units appended at 1..=2)
        // so this divider assertion is isolated from the streaming-cursor cue.
        let now = PORTAL_ACTIVITY_QUIESCE_WINDOW_US + 10;
        let md = portal_markdown(&state, None, now);
        assert!(
            md.contains(PORTAL_UNREAD_DIVIDER_LINE),
            "an unread agent turn must render the in-transcript divider: {md}"
        );

        state.unread_output_count = None;
        state.visible_unread_output_count = None;
        let cleared = portal_markdown(&state, None, now);
        assert!(
            !cleared.contains(PORTAL_UNREAD_DIVIDER_LINE),
            "the divider must clear locally when the viewer views the tail: {cleared}"
        );
    }

    /// Regression (hud-g1ena.2 review): when the viewer's clearance filters some
    /// unread turns out of `visible_transcript`, the aggregate `unread_output_count`
    /// exceeds the unread units the viewer can actually see. The divider must be
    /// placed with the clearance-corrected `visible_unread_output_count`, never the
    /// aggregate — otherwise the walk exhausts against the aggregate and clamps the
    /// divider onto an already-seen visible turn, marking seen text as unread.
    #[test]
    fn unread_divider_boundary_uses_visible_count_not_aggregate() {
        let mut state = make_expanded_interaction_state("portal-unread");
        // Two agent turns survive clearance: an older SEEN turn and one unread
        // turn. Two further unread turns were higher-classification and filtered
        // out upstream, so they never reach `visible_transcript`.
        state.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false), // seen, visible
            transcript_unit(4, OutputKind::Assistant, false), // unread, visible
        ];
        // Aggregate says 3 unread; only 1 of them is visible to this viewer.
        state.unread_output_count = Some(3);
        state.visible_unread_output_count = Some(1);
        // The divider sits before the single visible unread turn (index 1), NOT
        // clamped onto the seen turn at index 0 (which the aggregate count would do).
        assert_eq!(unread_divider_boundary(&state), Some(1));
    }

    /// The `unread_divider_color` `PortalVisualTokens` field maps from the
    /// source `PortalPartTokens` channel and is distinct from the ambient unread
    /// count token so the two affordances can be reskinned separately.
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_unread_divider_color() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.unread_divider_color.r)
                - expected_linear(part.unread_divider_color.r))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.unread_divider_color.g)
                - expected_linear(part.unread_divider_color.g))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.unread_divider_color.b)
                - expected_linear(part.unread_divider_color.b))
            .abs()
                < 1e-6
        );
        assert_eq!(visual.unread_divider_color.a, part.unread_divider_color.a);
        assert_ne!(
            visual.unread_divider_color, visual.unread_indicator_color,
            "unread divider and ambient count must be distinct tokens"
        );
    }

    /// Build a minimal `TranscriptUnit` for awaiting-reply tests, varying only
    /// `expects_reply` and `output_kind`.
    fn transcript_unit(
        sequence: u64,
        output_kind: OutputKind,
        expects_reply: bool,
    ) -> TranscriptUnit {
        TranscriptUnit {
            sequence,
            output_text: "example output".to_string(),
            output_kind,
            content_classification: ContentClassification::Private,
            logical_unit_id: None,
            coalesce_key: None,
            expects_reply,
            appended_at_wall_us: sequence,
        }
    }

    /// hud-jip0k acceptance: the ambient awaiting-reply cue — both the
    /// text-visible `"? awaiting reply"` line and the token-driven sentinel
    /// color run — is present exactly when the most recently published
    /// visible transcript unit has `expects_reply == true`, and ABSENT for
    /// every backward-compat case: no transcript, `expects_reply == false`
    /// (the default/omitted case), and once a viewer's echoed reply
    /// (`OutputKind::Viewer`, `expects_reply == false` by construction)
    /// becomes the new last unit.
    #[test]
    fn awaiting_reply_cue_gates_on_last_unit_expects_reply() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let awaiting_color = adapter.visual_tokens().awaiting_reply_color;

        // Backward-compat baseline: no transcript at all — absent, exactly
        // like every portal published before this field existed.
        let empty = make_expanded_interaction_state("portal-question");
        assert!(
            !portal_markdown(&empty, None, 0).contains("awaiting reply"),
            "no transcript must render no awaiting-reply cue"
        );
        assert!(
            awaiting_reply_color_runs(&empty, awaiting_color).is_empty(),
            "no transcript must emit no awaiting-reply color run"
        );

        // The default/omitted case: expects_reply == false on the last unit.
        let mut unset = make_expanded_interaction_state("portal-question");
        unset.visible_transcript = vec![transcript_unit(1, OutputKind::Assistant, false)];
        assert!(
            !portal_markdown(&unset, None, 0).contains("awaiting reply"),
            "expects_reply == false must render no awaiting-reply cue"
        );
        assert!(
            awaiting_reply_color_runs(&unset, awaiting_color).is_empty(),
            "expects_reply == false must emit no awaiting-reply color run"
        );

        // The opt-in case: expects_reply == true on the last unit.
        let mut question = make_expanded_interaction_state("portal-question");
        question.visible_transcript = vec![
            transcript_unit(1, OutputKind::Assistant, false),
            transcript_unit(2, OutputKind::Assistant, true),
        ];
        let markdown = portal_markdown(&question, None, 0);
        assert!(
            markdown.contains("? awaiting reply"),
            "expects_reply == true on the last unit must render the ambient cue: {markdown}"
        );
        let runs = awaiting_reply_color_runs(&question, awaiting_color);
        assert_eq!(
            runs.len(),
            1,
            "expects_reply == true must emit exactly one token-driven color run"
        );
        assert_eq!(
            runs[0].color.unwrap(),
            awaiting_color,
            "run must carry the awaiting_reply_color token, never a literal color"
        );
        assert_eq!(runs[0].start_byte, 0);
        assert_eq!(runs[0].end_byte, 0);

        // Answered: a viewer's echoed reply becomes the new last unit and
        // clears the cue, with no separate "answered" bookkeeping required.
        let mut answered = question.clone();
        answered
            .visible_transcript
            .push(transcript_unit(3, OutputKind::Viewer, false));
        assert!(
            !portal_markdown(&answered, None, 0).contains("awaiting reply"),
            "a viewer's echoed reply must clear the awaiting-reply cue"
        );
        assert!(
            awaiting_reply_color_runs(&answered, awaiting_color).is_empty(),
            "a viewer's echoed reply must clear the awaiting-reply color run"
        );

        // Redaction: visible_transcript is already emptied by the authority
        // for a restricted viewer, so the cue is absent with no separate check.
        let mut redacted = make_expanded_interaction_state("portal-question");
        redacted.redacted = true;
        redacted.visible_transcript = vec![];
        assert!(
            !portal_markdown(&redacted, None, 0).contains("awaiting reply"),
            "a redacted (empty transcript) viewer must render no awaiting-reply cue"
        );
    }

    /// hud-g1ena.1 acceptance: the ambient viewer-turn delivery-acknowledgement cue
    /// renders one quiet text line + one token-driven zero-length sentinel color run
    /// per presentation class, folding the six `InputDeliveryState` variants into
    /// three classes (in-flight / delivered / failed). Absent when the runtime
    /// tracks no viewer submission (`latest_viewer_delivery_state == None`), which is
    /// exactly the redaction case (the authority withholds the state). Rendering
    /// reads only `latest_viewer_delivery_state` — no adapter round trip.
    #[test]
    fn delivery_cue_renders_three_classes_and_is_absent_without_state() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let tokens = adapter.visual_tokens().clone();

        // No tracked submission → no line, no run. This is also the redaction case:
        // the authority leaves the field None whenever the transcript is withheld.
        let none_state = make_expanded_interaction_state("portal-delivery");
        let none_md = portal_markdown(&none_state, None, 0);
        for line in [
            PORTAL_DELIVERY_INFLIGHT_LINE,
            PORTAL_DELIVERY_DELIVERED_LINE,
            PORTAL_DELIVERY_FAILED_LINE,
        ] {
            assert!(
                !none_md.contains(line),
                "no tracked submission must render no delivery cue: {none_md}"
            );
        }
        assert!(
            delivery_cue_color_runs(&none_state, &tokens).is_empty(),
            "no tracked submission must emit no delivery color run"
        );

        // Each variant folds into its class: (variant, expected line, expected color).
        let cases = [
            (
                InputDeliveryState::Pending,
                PORTAL_DELIVERY_INFLIGHT_LINE,
                tokens.delivery_inflight_color,
            ),
            (
                InputDeliveryState::Deferred,
                PORTAL_DELIVERY_INFLIGHT_LINE,
                tokens.delivery_inflight_color,
            ),
            (
                InputDeliveryState::Delivered,
                PORTAL_DELIVERY_DELIVERED_LINE,
                tokens.delivery_delivered_color,
            ),
            (
                InputDeliveryState::Handled,
                PORTAL_DELIVERY_DELIVERED_LINE,
                tokens.delivery_delivered_color,
            ),
            (
                InputDeliveryState::Rejected,
                PORTAL_DELIVERY_FAILED_LINE,
                tokens.delivery_failed_color,
            ),
            (
                InputDeliveryState::Expired,
                PORTAL_DELIVERY_FAILED_LINE,
                tokens.delivery_failed_color,
            ),
        ];
        for (variant, expected_line, expected_color) in cases {
            let mut state = make_expanded_interaction_state("portal-delivery");
            state.latest_viewer_delivery_state = Some(variant);
            let md = portal_markdown(&state, None, 0);
            assert!(
                md.contains(expected_line),
                "{variant:?} must render the ambient cue line {expected_line:?}: {md}"
            );
            // Ambient, not alarming: the failed cue must NOT borrow the composer
            // rejection alarm glyph.
            assert!(
                !md.contains('⚠'),
                "delivery cue must stay ambient (no ⚠ alarm) for {variant:?}: {md}"
            );
            let runs = delivery_cue_color_runs(&state, &tokens);
            assert_eq!(
                runs.len(),
                1,
                "{variant:?} must emit exactly one token-driven delivery color run"
            );
            assert_eq!(
                runs[0].color.unwrap(),
                expected_color,
                "{variant:?} run must carry the token color, never a literal"
            );
            assert_eq!(
                runs[0].start_byte, 0,
                "delivery run is a zero-length sentinel"
            );
            assert_eq!(
                runs[0].end_byte, 0,
                "delivery run is a zero-length sentinel"
            );
        }
    }

    /// The three delivery `PortalVisualTokens` fields map from their
    /// `PortalPartTokens` counterparts (hud-g1ena.1), so a profile/token change
    /// reskins the delivery cue end-to-end with no adapter logic change.
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_delivery_fields() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.delivery_inflight_color.r)
                - expected_linear(part.delivery_inflight_color.r))
            .abs()
                < 1e-6
        );
        assert_eq!(
            visual.delivery_inflight_color.a,
            part.delivery_inflight_color.a
        );
        assert!(
            (f64::from(visual.delivery_delivered_color.g)
                - expected_linear(part.delivery_delivered_color.g))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.delivery_failed_color.b)
                - expected_linear(part.delivery_failed_color.b))
            .abs()
                < 1e-6
        );
        // The three classes must be visually distinct so a viewer can tell
        // in-flight from delivered from failed at a glance.
        assert_ne!(
            visual.delivery_inflight_color,
            visual.delivery_delivered_color
        );
        assert_ne!(
            visual.delivery_delivered_color,
            visual.delivery_failed_color
        );
        assert_ne!(visual.delivery_inflight_color, visual.delivery_failed_color);
    }

    /// hud-g1ena.5 acceptance: build a live expanded portal whose tail is a fresh
    /// agent turn and assert the ambient activity cue is fully present — the
    /// text-visible header marker, the tail streaming cursor glyph, and BOTH
    /// token-driven zero-length sentinel color runs (header + cursor). Then assert
    /// it QUIESCES once the newest append ages past the window: no marker, no
    /// cursor, no runs. Derivation is purely from the observed
    /// `appended_at_wall_us` vs the render `now` — no separate typing signal.
    #[test]
    fn agent_activity_cue_present_while_appending_and_quiesces_after_window() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let activity_color = adapter.visual_tokens().activity_cue_color;
        let cursor_color = adapter.visual_tokens().streaming_cursor_color;

        let appended_at = 1_000_000;
        let now_fresh = appended_at + 500_000; // within the quiesce window
        let now_quiesced = appended_at + PORTAL_ACTIVITY_QUIESCE_WINDOW_US + 1;

        let mut state = make_expanded_interaction_state("portal-activity");
        state.visible_transcript = vec![TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit(1, OutputKind::Assistant, false)
        }];

        // Actively appending: header marker + tail cursor glyph + both runs.
        assert!(agent_activity_active(&state, now_fresh));
        let fresh_md = portal_markdown(&state, None, now_fresh);
        assert!(
            fresh_md.contains(PORTAL_ACTIVITY_MARKER_LINE),
            "active append must render the ambient header activity marker: {fresh_md}"
        );
        assert!(
            fresh_md.contains(PORTAL_STREAMING_CURSOR_GLYPH),
            "active append must render the tail streaming cursor glyph: {fresh_md}"
        );
        let header_runs = activity_cue_color_runs(&state, activity_color, now_fresh);
        let cursor_runs =
            streaming_cursor_color_runs(&state, cursor_color, now_fresh, fresh_md.len());
        assert_eq!(header_runs.len(), 1, "one activity header sentinel run");
        assert_eq!(cursor_runs.len(), 1, "one streaming cursor sentinel run");
        // Both carry the token color, never a literal.
        assert_eq!(header_runs[0].color.unwrap(), activity_color);
        assert_eq!(cursor_runs[0].color.unwrap(), cursor_color);
        // The header cue is a byte-0 sentinel; the streaming cursor is pinned at
        // content END (hud-zlq2v) so the compositor can tell it apart and recolor
        // the trailing glyph. Both stay zero-length (cached-path safe).
        assert_eq!(header_runs[0].start_byte, 0, "header sentinel at byte 0");
        assert_eq!(header_runs[0].end_byte, 0, "header sentinel is zero-length");
        assert_eq!(
            cursor_runs[0].start_byte as usize,
            fresh_md.len(),
            "cursor marker pinned at content end"
        );
        assert_eq!(
            cursor_runs[0].end_byte as usize,
            fresh_md.len(),
            "cursor marker is zero-length"
        );

        // Quiesced: the same tail is now stale relative to `now`.
        assert!(!agent_activity_active(&state, now_quiesced));
        let quiesced_md = portal_markdown(&state, None, now_quiesced);
        assert!(
            !quiesced_md.contains(PORTAL_ACTIVITY_MARKER_LINE),
            "cue must quiesce once appends stop: {quiesced_md}"
        );
        assert!(
            !quiesced_md.contains(PORTAL_STREAMING_CURSOR_GLYPH),
            "cursor must quiesce once appends stop: {quiesced_md}"
        );
        assert!(activity_cue_color_runs(&state, activity_color, now_quiesced).is_empty());
        assert!(
            streaming_cursor_color_runs(&state, cursor_color, now_quiesced, quiesced_md.len())
                .is_empty()
        );
    }

    /// hud-zlq2v: on the assembled portal node, the streaming-cursor glyph sits at
    /// the exact tail of the latest agent turn (measured against the turn text,
    /// multibyte-safe), and its token-colored marker is pinned at the very END of
    /// the node content (`[content.len()..content.len()]`) — the unambiguous,
    /// cached-path-safe signal the compositor reads to recolor the glyph. This is
    /// the promotion-era replacement for the byte-0 sentinel.
    #[test]
    fn streaming_cursor_marker_pins_to_node_content_end_at_latest_turn_tail() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let cursor_color = adapter.visual_tokens().streaming_cursor_color;

        let appended_at = 1_000_000;
        let now_fresh = appended_at + 500_000; // within the quiesce window

        // A latest agent turn whose text ends with a multibyte grapheme — proves
        // the tail is not split and the content-end marker lands on a boundary.
        let tail_text = "streaming 世界";
        let mut state = make_expanded_interaction_state("portal-cursor-tail");
        state.visible_transcript = vec![TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit_text(1, OutputKind::Assistant, tail_text)
        }];

        let node = adapter.portal_node(&state, vec![0u8; 16], now_fresh);
        let tm = text_markdown_node(&node);

        // The cursor glyph is at the exact tail of the latest turn's text.
        let expected_tail = format!("{tail_text}{PORTAL_STREAMING_CURSOR_GLYPH}");
        assert!(
            tm.content.contains(&expected_tail),
            "cursor glyph must sit at the latest turn's exact tail: {:?}",
            tm.content
        );

        // Exactly one content-end zero-length marker carrying the token color; no
        // OTHER cursor sentinel elsewhere.
        let end = tm.content.len() as u32;
        let cursor_markers: Vec<_> = tm
            .color_runs
            .iter()
            .filter(|r| r.color == Some(cursor_color))
            .collect();
        assert_eq!(cursor_markers.len(), 1, "one streaming-cursor marker");
        assert_eq!(cursor_markers[0].start_byte, end, "pinned at content end");
        assert_eq!(cursor_markers[0].end_byte, end, "zero-length (cached-safe)");
        // content.len() is a valid UTF-8 boundary (whole-string length), so the
        // marker never splits the multibyte tail.
        assert!(tm.content.is_char_boundary(end as usize));
    }

    /// hud-kbm80: `agent_activity_clear_deadline_us` is the time-independent
    /// factor of `agent_activity_active` — it returns the tail's clear-due
    /// deadline (`appended_at + PORTAL_ACTIVITY_QUIESCE_WINDOW_US`) exactly when a
    /// cue is present, and `agent_activity_active(state, now)` is
    /// `now <= deadline`. The drive loop schedules its one-shot quiesce repaint
    /// off this deadline (there is otherwise no re-render to clear an idle cue).
    #[test]
    fn agent_activity_clear_deadline_matches_active_predicate() {
        let appended_at = 1_000_000;
        let deadline = appended_at + PORTAL_ACTIVITY_QUIESCE_WINDOW_US;

        let mut state = make_expanded_interaction_state("portal-deadline");
        state.visible_transcript = vec![TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit(1, OutputKind::Assistant, false)
        }];

        // A live fresh agent tail exposes its deadline.
        assert_eq!(
            agent_activity_clear_deadline_us(&state),
            Some(deadline),
            "the deadline is appended_at + the quiesce window"
        );
        // The active predicate is exactly `now <= deadline` at the boundary.
        assert!(
            agent_activity_active(&state, deadline),
            "active at the deadline"
        );
        assert!(
            !agent_activity_active(&state, deadline + 1),
            "quiesced one µs past the deadline"
        );

        // No cue → no deadline (each structural guard removes it). A degraded
        // portal is the representative case; the other guards are covered by
        // `agent_activity_cue_suppressed_in_non_writing_states`.
        let mut degraded = state.clone();
        degraded.connection_degraded = true;
        assert_eq!(
            agent_activity_clear_deadline_us(&degraded),
            None,
            "a degraded portal carries no active cue, so no deadline"
        );
    }

    /// The activity cue is suppressed in every non-writing case, all evaluated at
    /// a `now` that WOULD be fresh for an agent tail — proving each guard, not the
    /// window: a viewer-authored tail (the agent is not writing), a question
    /// awaiting a reply (quiescing, and its own cue owns that state), an empty
    /// transcript (redaction / first-run), a connection-degraded portal, a
    /// never-connected ("connecting") portal, and the collapsed presentation.
    #[test]
    fn agent_activity_cue_suppressed_in_non_writing_states() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let activity_color = adapter.visual_tokens().activity_cue_color;
        let cursor_color = adapter.visual_tokens().streaming_cursor_color;
        let appended_at = 1_000_000;
        let now = appended_at + 500_000; // fresh — so only the guards can suppress

        let assert_suppressed = |state: &ProjectedPortalState, case: &str| {
            assert!(
                !agent_activity_active(state, now),
                "{case}: predicate must be false"
            );
            let md = portal_markdown(state, None, now);
            assert!(
                !md.contains(PORTAL_ACTIVITY_MARKER_LINE),
                "{case}: no header marker: {md}"
            );
            assert!(
                !md.contains(PORTAL_STREAMING_CURSOR_GLYPH),
                "{case}: no cursor glyph: {md}"
            );
            assert!(
                activity_cue_color_runs(state, activity_color, now).is_empty(),
                "{case}: no header run"
            );
            assert!(
                streaming_cursor_color_runs(state, cursor_color, now, md.len()).is_empty(),
                "{case}: no cursor run"
            );
        };

        let agent_tail = || TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit(1, OutputKind::Assistant, false)
        };

        // Viewer-authored tail: the on-screen viewer, not the agent, is the last
        // author — the agent is not appending.
        let mut viewer_tail = make_expanded_interaction_state("portal-activity");
        viewer_tail.visible_transcript = vec![TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit(1, OutputKind::Viewer, false)
        }];
        assert_suppressed(&viewer_tail, "viewer tail");

        // Question awaiting a reply: quiescing, and the awaiting-reply cue owns it.
        let mut awaiting = make_expanded_interaction_state("portal-activity");
        awaiting.visible_transcript = vec![TranscriptUnit {
            appended_at_wall_us: appended_at,
            ..transcript_unit(1, OutputKind::Assistant, true)
        }];
        assert_suppressed(&awaiting, "awaiting reply");
        assert!(
            portal_markdown(&awaiting, None, now).contains("awaiting reply"),
            "awaiting-reply cue owns this state instead of the activity cue"
        );

        // Empty transcript (redaction / first-run): no tail to derive from.
        let empty = make_expanded_interaction_state("portal-activity");
        assert_suppressed(&empty, "empty transcript");

        // Connection-degraded: the surface must not imply an active stream.
        let mut degraded = make_expanded_interaction_state("portal-activity");
        degraded.visible_transcript = vec![agent_tail()];
        degraded.connection_degraded = true;
        assert_suppressed(&degraded, "degraded");

        // Never connected ("connecting"): connecting takes precedence over activity.
        let mut connecting = make_expanded_interaction_state("portal-activity");
        connecting.visible_transcript = vec![agent_tail()];
        connecting.has_ever_connected = false;
        assert_suppressed(&connecting, "connecting");

        // Collapsed presentation: the cursor is an expanded-transcript affordance.
        let mut collapsed = make_expanded_interaction_state("portal-activity");
        collapsed.visible_transcript = vec![agent_tail()];
        collapsed.presentation = ProjectedPortalPresentation::Collapsed;
        assert_suppressed(&collapsed, "collapsed");
    }

    /// The `activity_cue_color` / `streaming_cursor_color` `PortalVisualTokens`
    /// fields map from the source `PortalPartTokens` channels
    /// (single-source-of-truth invariant, matching the other token-mapping tests).
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_activity_and_cursor_fields() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.activity_cue_color.r) - expected_linear(part.activity_cue_color.r))
                .abs()
                < 1e-6
        );
        assert_eq!(visual.activity_cue_color.a, part.activity_cue_color.a);
        assert!(
            (f64::from(visual.streaming_cursor_color.r)
                - expected_linear(part.streaming_cursor_color.r))
            .abs()
                < 1e-6
        );
        assert_eq!(
            visual.streaming_cursor_color.a,
            part.streaming_cursor_color.a
        );
    }

    /// §lifecycle: a permitted viewer's published lifecycle_state drives a
    /// token-resolved accent color run, distinct per affordance group, and absent
    /// when lifecycle is redacted (`lifecycle_state = None`). No literal color
    /// appears in the render path — every accent is sourced from `visual_tokens`.
    #[test]
    fn lifecycle_state_drives_distinct_token_accent_runs() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let tokens = adapter.visual_tokens().clone();

        // Each variant maps onto its documented token accent.
        let cases = [
            (
                ProjectionLifecycleState::Active,
                tokens.lifecycle_active_color,
            ),
            (
                ProjectionLifecycleState::Attached,
                tokens.lifecycle_attached_color,
            ),
            (
                ProjectionLifecycleState::Degraded,
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::HudUnavailable,
                tokens.lifecycle_attention_color,
            ),
            (
                ProjectionLifecycleState::Detached,
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::CleanupPending,
                tokens.lifecycle_inactive_color,
            ),
            (
                ProjectionLifecycleState::Expired,
                tokens.lifecycle_inactive_color,
            ),
        ];
        for (lifecycle, expected) in cases {
            let mut state = make_expanded_interaction_state("portal-lifecycle");
            state.lifecycle_state = Some(lifecycle);
            let runs = lifecycle_marker_color_runs(&state, &tokens);
            assert_eq!(
                runs.len(),
                1,
                "lifecycle {lifecycle:?} must emit exactly one accent run"
            );
            assert_eq!(
                runs[0].color.unwrap(),
                expected,
                "lifecycle {lifecycle:?} must carry its token-resolved accent"
            );
            assert_eq!(runs[0].start_byte, 0, "Phase-1 sentinel run is zero-length");
            assert_eq!(runs[0].end_byte, 0, "Phase-1 sentinel run is zero-length");
        }

        // The four affordance groups are mutually distinct so each reads as a
        // different viewer-facing state.
        let groups = [
            tokens.lifecycle_active_color,
            tokens.lifecycle_attached_color,
            tokens.lifecycle_attention_color,
            tokens.lifecycle_inactive_color,
        ];
        for i in 0..groups.len() {
            for j in (i + 1)..groups.len() {
                assert_ne!(
                    groups[i], groups[j],
                    "lifecycle affordance accents {i} and {j} must be visually distinct"
                );
            }
        }

        // Redaction-gated: a viewer without lifecycle clearance gets no affordance.
        let mut redacted = make_expanded_interaction_state("portal-lifecycle");
        redacted.lifecycle_state = None;
        assert!(
            lifecycle_marker_color_runs(&redacted, &tokens).is_empty(),
            "redacted lifecycle must emit no accent run"
        );
    }

    /// §lifecycle (acceptance #3/#4): a lifecycle_state transition changes the
    /// rendered portal node — both the color_runs and the text-visible `status:`
    /// line differ per state. This is the scene change that marks the tile dirty
    /// (it rides the normal PublishToTile mutation), so the affordance renders
    /// rather than being swallowed by idle present-gating.
    #[test]
    fn portal_node_reflects_lifecycle_transition() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);

        let mut active = make_expanded_interaction_state("portal-lc-transition");
        active.lifecycle_state = Some(ProjectionLifecycleState::Active);
        let active_node = adapter.portal_node(&active, vec![0u8; 16], 0);
        let active_tm = text_markdown_node(&active_node);

        let mut detached = make_expanded_interaction_state("portal-lc-transition");
        detached.lifecycle_state = Some(ProjectionLifecycleState::Detached);
        let detached_node = adapter.portal_node(&detached, vec![0u8; 16], 0);
        let detached_tm = text_markdown_node(&detached_node);
        let header = |node: &proto::NodeProto| {
            node.children
                .iter()
                .filter_map(|child| match child.data.as_ref() {
                    Some(proto::node_proto::Data::TextMarkdown(text)) => Some(text),
                    _ => None,
                })
                .find(|text| text.content.starts_with("Test Session"))
                .expect("expanded native portal must carry a text-visible header")
                .content
                .clone()
        };
        let active_header = header(&active_node);
        let detached_header = header(&detached_node);

        // The render path reflects each state distinctly.
        assert_ne!(
            active_tm.color_runs, detached_tm.color_runs,
            "lifecycle transition must change the node's color runs (render path reflects state)"
        );
        assert_ne!(
            active_header, detached_header,
            "lifecycle transition must change the text-visible native header"
        );
        assert!(
            active_header.contains("Active"),
            "permitted viewer must see the lifecycle state in the native header"
        );
    }

    /// The lifecycle `PortalVisualTokens` fields map from the source
    /// `PortalPartTokens` channels (decoded RGB, unchanged alpha; single-source-of-truth invariant).
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_lifecycle_fields() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.lifecycle_active_color.r)
                - expected_linear(part.lifecycle_active_color.r))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.lifecycle_attached_color.g)
                - expected_linear(part.lifecycle_attached_color.g))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.lifecycle_attention_color.b)
                - expected_linear(part.lifecycle_attention_color.b))
            .abs()
                < 1e-6
        );
        assert_eq!(
            visual.lifecycle_inactive_color.a,
            part.lifecycle_inactive_color.a
        );
        // All four must be visible (non-zero alpha) so the affordance shows.
        for c in [
            visual.lifecycle_active_color,
            visual.lifecycle_attached_color,
            visual.lifecycle_attention_color,
            visual.lifecycle_inactive_color,
        ] {
            assert!(c.a > 0.0, "lifecycle accent must have non-zero alpha");
        }
    }

    /// §2: live activity/composer signals clear on disconnect — a degraded
    /// portal must not present `composer: ready` (which implies an active
    /// interactive stream).
    #[test]
    fn degraded_state_clears_composer_ready_signal() {
        let mut state = make_expanded_interaction_state("portal-composer-clear");
        state.connection_degraded = true;
        // interaction_enabled would normally render a composer line; the degraded
        // guard must take precedence.
        state.interaction_enabled = true;

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            !markdown.contains("composer: ready"),
            "§2: degraded portal must not imply an active composer stream"
        );
        assert!(
            markdown.contains("composer: unavailable"),
            "§2: degraded portal must show composer as unavailable"
        );
    }

    /// hud-f6zfa: with an active draft the compositor's bottom-pinned input
    /// strip is the single source of truth for the live draft. The markdown
    /// `composer:` line MUST NOT embed the draft text or caret glyph — doing so
    /// rendered a SECOND copy mid-transcript at a different Y than the bottom
    /// strip (a double / misaligned composer). The line stays a content-free
    /// status affordance.
    #[test]
    fn active_draft_not_duplicated_in_markdown_composer_line() {
        let state = make_expanded_interaction_state("portal-composer-dedup");
        let display = ComposerDisplayState {
            text: "hello world draft".to_string(),
            cursor: 5,
            at_capacity: false,
            sequence: 1,
        };

        let markdown = portal_markdown(&state, Some(&display), 0);

        // The draft text + caret live ONLY in the bottom input strip, never in
        // the transcript-flow markdown — otherwise the draft appears twice at
        // different Y positions.
        assert!(
            !markdown.contains("hello world draft"),
            "draft text must not be embedded in the markdown composer line: {markdown}"
        );
        assert!(
            !markdown.contains('▌'),
            "caret glyph must not appear in the markdown (bottom strip owns it): {markdown}"
        );
        // A content-free composer affordance is still present so the surface
        // reflects that the composer is active.
        assert!(
            markdown.contains("composer: composing"),
            "composer status affordance should be present: {markdown}"
        );
    }

    /// hud-f6zfa: at-capacity stays text-visible on the markdown composer status
    /// line even though the draft glyphs themselves are owned by the bottom
    /// strip — the `[!]` marker must not disappear with the draft dedup.
    #[test]
    fn at_capacity_draft_marks_capacity_without_draft_text() {
        let state = make_expanded_interaction_state("portal-composer-cap");
        let display = ComposerDisplayState {
            text: "some capped draft text".to_string(),
            cursor: 4,
            at_capacity: true,
            sequence: 2,
        };

        let markdown = portal_markdown(&state, Some(&display), 0);

        assert!(
            !markdown.contains("some capped draft text"),
            "draft text must not be duplicated in markdown: {markdown}"
        );
        assert!(
            !markdown.contains('▌'),
            "caret glyph must not appear in the markdown: {markdown}"
        );
        assert!(
            markdown.contains("[!]"),
            "at-capacity must remain text-visible on the status line: {markdown}"
        );
    }

    /// hud-9gyao: the composer at-capacity state no longer rides a zero-length
    /// color-run sentinel on the transcript markdown node. Building `portal_node`
    /// at capacity vs below capacity must yield IDENTICAL `color_runs` (the
    /// at-capacity hue is painted on the draft glyphs by the compositor --
    /// `composer_draft_base_color` -- not carried here), and the node must carry
    /// only zero-length sentinels so the cached-markdown fast path is preserved
    /// (#947 / `markdown_node_has_pixel_runs`).
    #[test]
    fn composer_at_capacity_adds_no_color_run_to_markdown_node() {
        fn markdown_color_runs(node: &proto::NodeProto) -> Vec<proto::TextColorRunProto> {
            text_markdown_node(node).color_runs.clone()
        }

        let state = make_expanded_interaction_state("portal-composer-cap-run");
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);

        let mut below_adapter = ResidentGrpcPortalAdapter::new(config.clone());
        below_adapter.composer_display = Some(ComposerDisplayState {
            text: "hi".to_string(),
            cursor: 2,
            at_capacity: false,
            sequence: 1,
        });
        let below_runs = markdown_color_runs(&below_adapter.portal_node(&state, vec![0u8; 16], 0));

        let mut cap_adapter = ResidentGrpcPortalAdapter::new(config);
        cap_adapter.composer_display = Some(ComposerDisplayState {
            text: "hi".to_string(),
            cursor: 2,
            at_capacity: true,
            sequence: 2,
        });
        let cap_runs = markdown_color_runs(&cap_adapter.portal_node(&state, vec![0u8; 16], 0));

        assert_eq!(
            below_runs, cap_runs,
            "at-capacity must not add a composer color run to the transcript node"
        );
        assert!(
            cap_runs.iter().all(|r| r.start_byte >= r.end_byte),
            "markdown node must carry only zero-length sentinels (no pixel runs) so \
             the cached-markdown path is preserved: {cap_runs:?}"
        );
    }

    /// The degraded `PortalVisualTokens` fields map from the source
    /// `PortalPartTokens` channels (decoded RGB, unchanged alpha; single-source-of-truth invariant).
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_degraded_fields() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.transcript_dim_text_color.r)
                - expected_linear(part.transcript_dim_text_color.r))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.transcript_dim_background.g)
                - expected_linear(part.transcript_dim_background.g))
            .abs()
                < 1e-6
        );
        assert!(
            (f64::from(visual.stale_marker_color.b) - expected_linear(part.stale_marker_color.b))
                .abs()
                < 1e-6
        );
        assert_eq!(visual.stale_marker_color.a, part.stale_marker_color.a);

        // Follow the real profile resolution path. Nonendpoint colors distinguish
        // encoded config RGB from the linear RGB required by scene consumers.
        let overrides = tze_hud_config::tokens::DesignTokenMap::from([
            (
                tze_hud_config::portal_tokens::PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR.to_string(),
                "#80808000".to_string(),
            ),
            (
                tze_hud_config::portal_tokens::PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND.to_string(),
                "#0A7FD980".to_string(),
            ),
            (
                tze_hud_config::portal_tokens::PORTAL_TOKEN_STALE_MARKER_COLOR.to_string(),
                "#7F4A90FF".to_string(),
            ),
        ]);
        let resolved = tze_hud_config::tokens::resolve_tokens(
            &tze_hud_config::tokens::DesignTokenMap::new(),
            &overrides,
        );
        let overridden_part = tze_hud_config::resolve_portal_tokens(&resolved);
        let overridden_visual = portal_visual_tokens_from_part_tokens(&overridden_part);
        // Independent IEC61966-2-1 constants, not the production decoder.
        let cases = [
            (
                "dim text",
                &overridden_part.transcript_dim_text_color,
                overridden_visual.transcript_dim_text_color,
                [128_u8, 128, 128, 0],
                [0.21586050011389926_f64; 3],
            ),
            (
                "dim background",
                &overridden_part.transcript_dim_background,
                overridden_visual.transcript_dim_background,
                [10_u8, 127, 217, 128],
                [
                    0.003035269835488375_f64,
                    0.21223075741405523,
                    0.6938717612919899,
                ],
            ),
            (
                "stale marker",
                &overridden_part.stale_marker_color,
                overridden_visual.stale_marker_color,
                [127_u8, 74, 144, 255],
                [
                    0.21223075741405523_f64,
                    0.06847816984440017,
                    0.2788942634768104,
                ],
            ),
        ];
        for (name, source, result, encoded, _) in &cases {
            for (actual, byte) in [source.r, source.g, source.b].into_iter().zip(encoded) {
                assert_eq!(
                    actual.to_bits(),
                    (f32::from(*byte) / 255.0).to_bits(),
                    "{name}: config must retain encoded byte/255 RGB",
                );
            }
            assert_eq!(
                source.a.to_bits(),
                (f32::from(encoded[3]) / 255.0).to_bits()
            );
            assert_eq!(
                result.a.to_bits(),
                source.a.to_bits(),
                "{name}: zero, partial and full alpha must pass through unchanged",
            );
        }
        for (name, _, result, _, expected) in &cases {
            for (channel, (actual, reference)) in [result.r, result.g, result.b]
                .into_iter()
                .zip(expected)
                .enumerate()
            {
                assert!(
                    (f64::from(actual) - reference).abs() < 1e-6,
                    "{name} RGB channel {channel} must be linear: got {actual}, expected {reference}",
                );
            }
        }

        // Every bridge field is exercised with the real defaults and a distinct
        // resolved override, so omitted decoding or a cross-wired field fails.
        let color_keys = [
            "portal.frame.background",
            "portal.header.text_color",
            "portal.divider.color",
            "portal.window.resize_grip.color",
            "portal.transcript.background",
            "portal.transcript.text_color",
            "portal.transcript.system_color",
            "portal.transcript.dim_text_color",
            "portal.transcript.dim_background",
            "portal.stale_marker.color",
            "portal.disconnect_badge.color",
            "portal.unread_indicator.color",
            "portal.unread_divider.color",
            "portal.awaiting_reply.color",
            "portal.empty_state.color",
            "portal.connecting_marker.color",
            "portal.activity_cue.color",
            "portal.streaming_cursor.color",
            "portal.delivery.inflight_color",
            "portal.delivery.delivered_color",
            "portal.delivery.failed_color",
            "portal.timestamp.color",
            "portal.lifecycle.active_color",
            "portal.lifecycle.attached_color",
            "portal.lifecycle.attention_color",
            "portal.lifecycle.inactive_color",
            "portal.collapsed_card.background",
            "portal.collapsed_card.text_color",
            "portal.composer.background",
            "portal.composer.text_color",
        ];
        let mut all_overrides = tze_hud_config::tokens::DesignTokenMap::new();
        let mut encoded_overrides = Vec::new();
        for (index, key) in color_keys.iter().enumerate() {
            let index = index as u8;
            let encoded = [
                index + 1,
                200 - index * 3,
                50 + index * 4,
                match index % 3 {
                    0 => 0,
                    1 => 128,
                    _ => 255,
                },
            ];
            all_overrides.insert(
                (*key).to_string(),
                format!(
                    "#{:02X}{:02X}{:02X}{:02X}",
                    encoded[0], encoded[1], encoded[2], encoded[3]
                ),
            );
            encoded_overrides.push(encoded);
        }
        let all_resolved = tze_hud_config::tokens::resolve_tokens(
            &tze_hud_config::tokens::DesignTokenMap::new(),
            &all_overrides,
        );
        let all_part = tze_hud_config::resolve_portal_tokens(&all_resolved);
        let all_visual = portal_visual_tokens_from_part_tokens(&all_part);
        for (case, source, target) in [
            ("default", &part, &visual),
            ("override", &all_part, &all_visual),
        ] {
            let mapped = [
                (
                    "portal.frame.background",
                    &source.frame_background,
                    target.frame_background,
                ),
                (
                    "portal.header.text_color",
                    &source.header_text_color,
                    target.header_text_color,
                ),
                (
                    "portal.divider.color",
                    &source.divider_color,
                    target.divider_color,
                ),
                (
                    "portal.window.resize_grip.color",
                    &source.resize_grip_color,
                    target.divider_grip_color,
                ),
                (
                    "portal.transcript.background",
                    &source.transcript_background,
                    target.transcript_background,
                ),
                (
                    "portal.transcript.text_color",
                    &source.transcript_text_color,
                    target.transcript_text_color,
                ),
                (
                    "portal.transcript.system_color",
                    &source.transcript_system_color,
                    target.transcript_system_color,
                ),
                (
                    "portal.transcript.dim_text_color",
                    &source.transcript_dim_text_color,
                    target.transcript_dim_text_color,
                ),
                (
                    "portal.transcript.dim_background",
                    &source.transcript_dim_background,
                    target.transcript_dim_background,
                ),
                (
                    "portal.stale_marker.color",
                    &source.stale_marker_color,
                    target.stale_marker_color,
                ),
                (
                    "portal.disconnect_badge.color",
                    &source.disconnect_badge_color,
                    target.disconnect_badge_color,
                ),
                (
                    "portal.unread_indicator.color",
                    &source.unread_indicator_color,
                    target.unread_indicator_color,
                ),
                (
                    "portal.unread_divider.color",
                    &source.unread_divider_color,
                    target.unread_divider_color,
                ),
                (
                    "portal.awaiting_reply.color",
                    &source.awaiting_reply_color,
                    target.awaiting_reply_color,
                ),
                (
                    "portal.empty_state.color",
                    &source.empty_state_color,
                    target.empty_state_color,
                ),
                (
                    "portal.connecting_marker.color",
                    &source.connecting_marker_color,
                    target.connecting_marker_color,
                ),
                (
                    "portal.activity_cue.color",
                    &source.activity_cue_color,
                    target.activity_cue_color,
                ),
                (
                    "portal.streaming_cursor.color",
                    &source.streaming_cursor_color,
                    target.streaming_cursor_color,
                ),
                (
                    "portal.delivery.inflight_color",
                    &source.delivery_inflight_color,
                    target.delivery_inflight_color,
                ),
                (
                    "portal.delivery.delivered_color",
                    &source.delivery_delivered_color,
                    target.delivery_delivered_color,
                ),
                (
                    "portal.delivery.failed_color",
                    &source.delivery_failed_color,
                    target.delivery_failed_color,
                ),
                (
                    "portal.timestamp.color",
                    &source.timestamp_color,
                    target.timestamp_color,
                ),
                (
                    "portal.lifecycle.active_color",
                    &source.lifecycle_active_color,
                    target.lifecycle_active_color,
                ),
                (
                    "portal.lifecycle.attached_color",
                    &source.lifecycle_attached_color,
                    target.lifecycle_attached_color,
                ),
                (
                    "portal.lifecycle.attention_color",
                    &source.lifecycle_attention_color,
                    target.lifecycle_attention_color,
                ),
                (
                    "portal.lifecycle.inactive_color",
                    &source.lifecycle_inactive_color,
                    target.lifecycle_inactive_color,
                ),
                (
                    "portal.collapsed_card.background",
                    &source.collapsed_background,
                    target.collapsed_background,
                ),
                (
                    "portal.collapsed_card.text_color",
                    &source.collapsed_text_color,
                    target.collapsed_text_color,
                ),
                (
                    "portal.composer.background",
                    &source.composer_background,
                    target.composer_background,
                ),
                (
                    "portal.composer.text_color",
                    &source.composer_text_color,
                    target.composer_text_color,
                ),
            ];
            for (index, (key, source, result)) in mapped.iter().enumerate() {
                let encoded = [source.r, source.g, source.b];
                if case == "override" {
                    for (actual, byte) in encoded.into_iter().zip(&encoded_overrides[index]) {
                        assert_eq!(
                            actual.to_bits(),
                            (f32::from(*byte) / 255.0).to_bits(),
                            "{key}: distinct override must reach its own config field",
                        );
                    }
                    assert_eq!(
                        source.a.to_bits(),
                        (f32::from(encoded_overrides[index][3]) / 255.0).to_bits(),
                    );
                }
                for (actual, encoded) in [result.r, result.g, result.b].into_iter().zip(encoded) {
                    assert!(
                        (f64::from(actual) - expected_linear(encoded)).abs() < 1e-6,
                        "{case} {key}: bridge RGB must decode the matching config field",
                    );
                }
                assert_eq!(
                    result.a.to_bits(),
                    source.a.to_bits(),
                    "{case} {key}: alpha must remain bit-exact",
                );
            }
        }
    }

    #[test]
    fn default_projected_portal_font_sizes_are_readable() {
        let visual = PortalVisualTokens::default();

        assert!(
            visual.transcript_font_size_px >= 16.0,
            "projected portal transcript default font should be readable without resize; got {}px",
            visual.transcript_font_size_px
        );
        assert!(
            visual.composer_font_size_px >= 16.0,
            "projected portal composer default font should be readable without resize; got {}px",
            visual.composer_font_size_px
        );
        assert!(
            visual.collapsed_font_size_px >= 14.0,
            "projected portal collapsed default font should remain readable; got {}px",
            visual.collapsed_font_size_px
        );
    }

    // ── §First-Run Empty Portal Treatment (hud-g1ena.6) ──────────────────────

    /// A connected portal with an empty retained transcript renders the friendly,
    /// token-styled empty state — NOT the literal `<empty projection stream>` —
    /// and emits exactly one token-driven sentinel color run.
    #[test]
    fn empty_state_replaces_literal_placeholder_with_token_styled_ready_line() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let empty_color = adapter.visual_tokens().empty_state_color;

        // Fixture: connected (has_ever_connected == true), empty transcript.
        let state = make_expanded_interaction_state("portal-empty");
        assert!(state.visible_transcript.is_empty(), "precondition: empty");

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            !markdown.contains("<empty projection stream>"),
            "the literal placeholder must be gone: {markdown}"
        );
        assert!(
            markdown.contains(PORTAL_EMPTY_READY_LINE),
            "connected + empty must render the inviting ready line: {markdown}"
        );

        let runs = empty_state_color_runs(&state, empty_color);
        assert_eq!(runs.len(), 1, "empty state must emit one token-driven run");
        assert_eq!(
            runs[0].color.unwrap(),
            empty_color,
            "run must carry the empty_state_color token, never a literal color"
        );
        assert_eq!(runs[0].start_byte, 0);
        assert_eq!(runs[0].end_byte, 0);
    }

    /// §Connecting State Distinction precedence: an attached-but-never-connected
    /// portal (`has_ever_connected == false`) shows the distinct connecting line —
    /// never the "ready" invite — and emits the connecting token run but NO
    /// empty-state run (the two treatments are mutually exclusive, hud-g1ena.7).
    #[test]
    fn empty_state_yields_to_connecting_when_never_connected() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let empty_color = adapter.visual_tokens().empty_state_color;
        let connecting_color = adapter.visual_tokens().connecting_marker_color;

        let mut state = make_expanded_interaction_state("portal-connecting");
        state.has_ever_connected = false;

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            markdown.contains(PORTAL_CONNECTING_LINE),
            "never-connected must render the connecting line: {markdown}"
        );
        assert!(
            !markdown.contains(PORTAL_EMPTY_READY_LINE),
            "a starting-up portal must NOT read as a ready empty state: {markdown}"
        );
        assert!(
            !markdown.contains("<empty projection stream>"),
            "the literal placeholder must be gone even while connecting: {markdown}"
        );
        // The connecting treatment emits its own token run and suppresses the
        // empty-ready run — the two gates are inverted, so exactly one fires.
        let connecting_runs = connecting_color_runs(&state, connecting_color);
        assert_eq!(
            connecting_runs.len(),
            1,
            "connecting state must emit one token-driven run"
        );
        assert_eq!(
            connecting_runs[0].color.unwrap(),
            connecting_color,
            "run must carry the connecting_marker_color token, never a literal color"
        );
        assert!(
            empty_state_color_runs(&state, empty_color).is_empty(),
            "the connecting case must emit no empty-ready run (mutually exclusive)"
        );
    }

    /// §Connecting State Distinction core requirement: the connecting treatment is
    /// visually distinct from the degraded/disconnected treatment on BOTH the
    /// text (distinct glyph + copy) and the token color — a starting-up portal
    /// must not read as a failing one.
    #[test]
    fn connecting_treatment_is_distinct_from_degraded() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let tokens = adapter.visual_tokens();

        // Text axis: the connecting line and the degraded marker share no glyph
        // or copy, so environments that ignore color_runs still tell them apart.
        assert_ne!(
            PORTAL_CONNECTING_LINE, PORTAL_DISCONNECT_MARKER_LINE,
            "connecting and degraded lines must differ"
        );
        assert!(
            !PORTAL_CONNECTING_LINE.contains("disconnected")
                && !PORTAL_CONNECTING_LINE.contains("stale"),
            "connecting copy must not read as disconnected/stale: {PORTAL_CONNECTING_LINE}"
        );

        // Color axis: the connecting hue differs from the degraded/stale marker.
        assert_ne!(
            tokens.connecting_marker_color, tokens.stale_marker_color,
            "connecting hue must be distinct from the degraded/stale marker"
        );

        // A never-connected portal renders connecting, NOT the degraded marker,
        // and does not dim the transcript or disable the composer for a failure.
        let mut connecting = make_expanded_interaction_state("portal-connecting-vs-degraded");
        connecting.has_ever_connected = false;
        let connecting_md = portal_markdown(&connecting, None, 0);
        assert!(
            connecting_md.contains(PORTAL_CONNECTING_LINE),
            "never-connected renders connecting: {connecting_md}"
        );
        assert!(
            !connecting_md.contains(PORTAL_DISCONNECT_MARKER_LINE),
            "never-connected must NOT render the degraded/disconnect marker: {connecting_md}"
        );

        // A previously-connected-now-dropped portal renders the degraded marker,
        // NOT the connecting line — the inverse case, proving the split.
        let mut degraded = make_expanded_interaction_state("portal-degraded-not-connecting");
        degraded.connection_degraded = true;
        let degraded_md = portal_markdown(&degraded, None, 0);
        assert!(
            degraded_md.contains(PORTAL_DISCONNECT_MARKER_LINE),
            "dropped portal renders the degraded marker: {degraded_md}"
        );
        assert!(
            !degraded_md.contains(PORTAL_CONNECTING_LINE),
            "a dropped (previously-connected) portal must NOT read as connecting: {degraded_md}"
        );
    }

    /// The connecting treatment is content-free, so — like the degraded marker —
    /// it is redaction-independent: a restricted viewer of a never-connected
    /// portal still sees connecting (revealing only connection state, no content),
    /// taking precedence over the redacted empty placeholder.
    #[test]
    fn connecting_treatment_is_redaction_independent() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let connecting_color = adapter.visual_tokens().connecting_marker_color;

        let mut state = make_expanded_interaction_state("portal-connecting-redacted");
        state.has_ever_connected = false;
        state.redacted = true;

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            markdown.contains(PORTAL_CONNECTING_LINE),
            "a redacted never-connected portal still shows connecting: {markdown}"
        );
        assert!(
            !markdown.contains(PORTAL_EMPTY_REDACTED_LINE),
            "connecting takes precedence over the redacted empty placeholder: {markdown}"
        );
        assert_eq!(
            connecting_color_runs(&state, connecting_color).len(),
            1,
            "the connecting token run is emitted even under redaction"
        );
    }

    /// §First-Run Empty Portal Treatment redaction scenario: a restricted viewer
    /// (whose `visible_transcript` is emptied upstream, so this path is reached)
    /// sees a content-free placeholder with NO inviting copy. The empty-state
    /// treatment itself is still active, so the token-driven run is still emitted.
    #[test]
    fn empty_state_suppresses_inviting_copy_under_redaction() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let empty_color = adapter.visual_tokens().empty_state_color;

        let mut state = make_expanded_interaction_state("portal-redacted-empty");
        state.redacted = true;

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            markdown.contains(PORTAL_EMPTY_REDACTED_LINE),
            "redacted empty portal must render the content-free placeholder: {markdown}"
        );
        assert!(
            !markdown.contains(PORTAL_EMPTY_READY_LINE),
            "redaction must suppress the inviting copy: {markdown}"
        );
        assert_eq!(
            empty_state_color_runs(&state, empty_color).len(),
            1,
            "the redacted empty-state treatment is still active → one token run"
        );
    }

    /// The empty state yields immediately to real content: the first appended
    /// transcript unit replaces it, and no empty-state marker or run remains.
    #[test]
    fn empty_state_yields_to_first_content() {
        let config = ResidentGrpcPortalConfig::new(vec![0u8; 16]);
        let adapter = ResidentGrpcPortalAdapter::new(config);
        let empty_color = adapter.visual_tokens().empty_state_color;

        let mut state = make_expanded_interaction_state("portal-first-content");
        state.visible_transcript = vec![transcript_unit(1, OutputKind::Assistant, false)];

        let markdown = portal_markdown(&state, None, 0);
        assert!(
            markdown.contains("example output"),
            "real content must render: {markdown}"
        );
        assert!(
            !markdown.contains(PORTAL_EMPTY_READY_LINE)
                && !markdown.contains(PORTAL_CONNECTING_LINE)
                && !markdown.contains(PORTAL_EMPTY_REDACTED_LINE),
            "no empty/connecting placeholder once content exists: {markdown}"
        );
        assert!(
            empty_state_color_runs(&state, empty_color).is_empty(),
            "a non-empty transcript must emit no empty-state run"
        );
    }

    /// The `empty_state_color` `PortalVisualTokens` field maps from the
    /// source `PortalPartTokens` channel (hud-g1ena.6), same single-source-of-
    /// truth invariant as the other token-mapping tests in this module.
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_empty_state_color() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.empty_state_color.r) - expected_linear(part.empty_state_color.r))
                .abs()
                < 1e-6
        );
        assert_eq!(visual.empty_state_color.a, part.empty_state_color.a);
    }

    /// The `connecting_marker_color` `PortalVisualTokens` field maps from the
    /// source `PortalPartTokens` channel (hud-g1ena.7), same single-source-of-
    /// truth invariant as the other token-mapping tests in this module.
    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_connecting_marker_color() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.connecting_marker_color.r)
                - expected_linear(part.connecting_marker_color.r))
            .abs()
                < 1e-6
        );
        assert_eq!(
            visual.connecting_marker_color.a,
            part.connecting_marker_color.a
        );
        // The default connecting hue must not collide with the degraded/stale
        // marker — the §Connecting State Distinction invariant, enforced at the
        // mapping boundary too.
        assert_ne!(visual.connecting_marker_color, visual.stale_marker_color);
    }

    // ── Ambient per-turn timestamps (hud-g1ena.4) ────────────────────────────

    /// A retained agent turn with a chosen runtime-assigned wall-clock arrival.
    fn stamped_unit(sequence: u64, wall_us: u64, text: &str) -> TranscriptUnit {
        TranscriptUnit {
            sequence,
            output_text: text.to_string(),
            output_kind: OutputKind::Assistant,
            content_classification: ContentClassification::Private,
            logical_unit_id: None,
            coalesce_key: None,
            expects_reply: false,
            appended_at_wall_us: wall_us,
        }
    }

    #[test]
    fn wall_clock_arrival_formats_at_minute_precision_utc() {
        assert_eq!(format_wall_clock_arrival_hhmm(0), "00:00");
        // 90s past the epoch → 00:01 (seconds dropped, minute rolls).
        assert_eq!(format_wall_clock_arrival_hhmm(90_000_000), "00:01");
        // 13:45:59 UTC → still 13:45 (minute precision, ambient).
        let thirteen_forty_five = (13 * 3_600 + 45 * 60 + 59) * 1_000_000 + 999_999;
        assert_eq!(format_wall_clock_arrival_hhmm(thirteen_forty_five), "13:45");
    }

    #[test]
    fn per_turn_granularity_stamps_every_turn_from_arrival_metadata() {
        let units = vec![
            stamped_unit(1, 0, "first"),
            stamped_unit(2, 90_000_000, "second"),
        ];
        let md = visible_transcript_markdown_with(&units, None, TimestampGranularity::PerTurn);
        // Each turn carries its own runtime-assigned arrival stamp + separator.
        assert!(
            md.contains(&format!("00:00{PORTAL_TIMESTAMP_SEPARATOR}first")),
            "{md}"
        );
        assert!(
            md.contains(&format!("00:01{PORTAL_TIMESTAMP_SEPARATOR}second")),
            "{md}"
        );
    }

    #[test]
    fn grouped_granularity_coalesces_consecutive_same_minute_turns() {
        let units = vec![
            stamped_unit(1, 0, "a"),          // 00:00, minute 0 — stamped
            stamped_unit(2, 30_000_000, "b"), // 00:00, minute 0 — same, no stamp
            stamped_unit(3, 90_000_000, "c"), // 00:01, minute 1 — new, stamped
        ];
        let md = visible_transcript_markdown_with(&units, None, TimestampGranularity::Grouped);
        // Exactly two stamps: one for the first minute, one when the minute rolls.
        assert_eq!(md.matches(PORTAL_TIMESTAMP_SEPARATOR).count(), 2, "{md}");
        assert!(
            md.contains(&format!("00:00{PORTAL_TIMESTAMP_SEPARATOR}a")),
            "{md}"
        );
        // "b" shares the minute with "a" and carries no stamp of its own.
        assert!(md.contains("\n---\nb"), "{md}");
        assert!(
            md.contains(&format!("00:01{PORTAL_TIMESTAMP_SEPARATOR}c")),
            "{md}"
        );
    }

    #[test]
    fn off_granularity_renders_no_timestamps() {
        let units = vec![stamped_unit(1, 90_000_000, "hello")];
        let md = visible_transcript_markdown_with(&units, None, TimestampGranularity::Off);
        assert_eq!(md, "hello");
        assert!(!md.contains(PORTAL_TIMESTAMP_SEPARATOR));
    }

    #[test]
    fn adapter_supplied_text_cannot_forge_the_arrival_stamp() {
        // A turn whose CONTENT lies about the time; the presented stamp derives
        // from the runtime-assigned appended_at_wall_us, not the content.
        let units = vec![stamped_unit(1, 0, "it is really 23:59")];
        let md = visible_transcript_markdown_with(&units, None, TimestampGranularity::PerTurn);
        assert!(
            md.starts_with(&format!("00:00{PORTAL_TIMESTAMP_SEPARATOR}")),
            "{md}"
        );
    }

    #[test]
    fn timestamp_color_runs_are_zero_length_token_sentinels_when_enabled() {
        let mut state = make_expanded_interaction_state("ts-runs");
        state.visible_transcript = vec![stamped_unit(1, 0, "hi")];
        let color = proto::Rgba {
            r: 0.42,
            g: 0.43,
            b: 0.44,
            a: 1.0,
        };
        let runs = timestamp_color_runs(&state, color, TimestampGranularity::PerTurn);
        assert_eq!(runs.len(), 1, "one timestamp sentinel when stamps render");
        assert_eq!(runs[0].start_byte, 0, "Phase-1 sentinel run is zero-length");
        assert_eq!(runs[0].end_byte, 0, "Phase-1 sentinel run is zero-length");
        assert_eq!(
            runs[0].color,
            Some(color),
            "sentinel carries the token color"
        );
    }

    #[test]
    fn timestamp_color_runs_absent_when_off_collapsed_or_empty() {
        let mut expanded = make_expanded_interaction_state("ts-off");
        expanded.visible_transcript = vec![stamped_unit(1, 0, "hi")];
        let color = proto::Rgba {
            r: 0.4,
            g: 0.4,
            b: 0.4,
            a: 1.0,
        };
        // Off granularity → no sentinel even with content.
        assert!(timestamp_color_runs(&expanded, color, TimestampGranularity::Off).is_empty());
        // Empty transcript (e.g. redacted upstream) → no sentinel even when enabled.
        let mut empty = make_expanded_interaction_state("ts-empty");
        empty.visible_transcript = vec![];
        assert!(timestamp_color_runs(&empty, color, TimestampGranularity::PerTurn).is_empty());
        // Collapsed presentation → the stamp is an expanded-transcript affordance.
        let mut collapsed = make_expanded_interaction_state("ts-collapsed");
        collapsed.presentation = ProjectedPortalPresentation::Collapsed;
        collapsed.visible_transcript = vec![stamped_unit(1, 0, "hi")];
        assert!(timestamp_color_runs(&collapsed, color, TimestampGranularity::PerTurn).is_empty());
    }

    #[test]
    fn portal_visual_tokens_from_part_tokens_maps_timestamp_fields() {
        let part = tze_hud_config::PortalPartTokens::default();
        let visual = portal_visual_tokens_from_part_tokens(&part);
        let expected_linear = |encoded: f32| {
            let encoded = f64::from(encoded);
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        assert!(
            (f64::from(visual.timestamp_color.r) - expected_linear(part.timestamp_color.r)).abs()
                < 1e-6
        );
        assert_eq!(visual.timestamp_color.a, part.timestamp_color.a);
        assert_eq!(visual.timestamp_granularity, part.timestamp_granularity);
        // Ambient default is Off — the base surface stays calm (profile opt-in).
        assert_eq!(visual.timestamp_granularity, TimestampGranularity::Off);
    }
}
