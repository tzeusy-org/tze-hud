//! Portal part inventory and token mapping for the text-stream portal pilot.
//!
//! Implements §6.2 of `text-stream-portal-phase1/tasks.md`:
//! - Portal part inventory (frame, header, composer, transcript body, divider,
//!   collapsed card)
//! - Token mapping each part consumes
//! - `PortalPartTokens`: resolved visual values extracted from a `DesignTokenMap`
//!
//! Also implements §6b window management tokens (amendment 2026-06-10):
//! - Window geometry bounds (min size, resize step, affordance size)
//! - Scroll-position indicator styling
//!
//! **Pre-promotion rule:** the exemplar adapter MUST source every published visual
//! value from the runtime's resolved token set (via `PortalPartTokens`) rather than
//! literal values. A profile/token change MUST reskin the portal end-to-end without
//! touching adapter logic. See `about/heart-and-soul/v1.md` and CLAUDE.md
//! "visual identity is modular".
//!
//! ## Canonical portal token keys (profile-scoped, pre-promotion)
//!
//! These keys are **portal-scoped**: they are prefixed with `portal.` to avoid
//! colliding with canonical component-shape-language keys. They are resolvable
//! via profile-scoped overrides and fall back to the canonical token defaults.
//! At promotion time they will be canonicalized in the `text-portal` component
//! type contract via a separate component-shape-language delta.
//!
//! | Key | Part | Property |
//! |-----|------|----------|
//! | `portal.frame.background` | frame | backdrop fill (RGBA hex) |
//! | `portal.frame.opacity` | frame | backdrop opacity (0.0–1.0) |
//! | `portal.frame.border_color` | frame | border stroke color (RGBA hex) |
//! | `portal.header.text_color` | header | title text color (RGBA hex) |
//! | `portal.header.font_size` | header | title font size in px |
//! | `portal.composer.background` | composer | input area backdrop color (RGBA hex) |
//! | `portal.composer.text_color` | composer | draft text color (RGBA hex) |
//! | `portal.composer.font_size` | composer | draft font size in px |
//! | `portal.transcript.background` | transcript body | content backdrop color (RGBA hex) |
//! | `portal.transcript.text_color` | transcript body | content text color (RGBA hex) |
//! | `portal.transcript.font_size` | transcript body | content font size in px |
//! | `portal.transcript.code_background` | transcript body | code-span backdrop; prefers over `color.code.background` (RGBA hex) |
//! | `portal.transcript.code_text` | transcript body | code-span foreground; prefers over `color.code.text` (RGBA hex) |
//! | `portal.transcript.link_color` | transcript body | link text color; prefers over `color.link.text` (RGBA hex) |
//! | `portal.transcript.code_font_family` | transcript body | code font family; prefers over `typography.code.family` |
//! | `portal.transcript.dim_text_color` | transcript body | dimmed text shown while disconnected/stale (RGBA hex) |
//! | `portal.transcript.dim_background` | transcript body | dimmed backdrop shown while disconnected/stale (RGBA hex) |
//! | `portal.stale_marker.color` | stale marker | content-free disconnect marker color (RGBA hex) |
//! | `portal.disconnect_badge.color` | disconnect badge | visible geometry badge color shown while connection-degraded, redaction-independent (RGBA hex) |
//! | `portal.disconnect_badge.width_px` | disconnect badge | disconnect badge accent-bar width in px |
//! | `portal.unread_indicator.color` | unread indicator | ambient unread-output count color (RGBA hex) |
//! | `portal.awaiting_reply.color` | awaiting-reply indicator | ambient question/awaiting-reply cue color (RGBA hex) |
//! | `portal.connecting_marker.color` | connecting marker | never-connected connecting cue color, distinct from the degraded/stale marker (RGBA hex) |
//! | `portal.activity_cue.color` | activity cue | ambient header "agent is writing" cue derived from observed appends (RGBA hex) |
//! | `portal.streaming_cursor.color` | streaming cursor | ambient tail live-writing cursor derived from observed appends (RGBA hex) |
//! | `portal.delivery.inflight_color` | delivery cue | ambient in-flight (Pending/Deferred) viewer-turn delivery cue (RGBA hex) |
//! | `portal.delivery.delivered_color` | delivery cue | ambient delivered (Delivered/Handled) viewer-turn delivery cue (RGBA hex) |
//! | `portal.delivery.failed_color` | delivery cue | ambient failed (Rejected/Expired) viewer-turn delivery cue (RGBA hex) |
//! | `portal.lifecycle.active_color` | lifecycle affordance | accent for `Active` (RGBA hex) |
//! | `portal.lifecycle.attached_color` | lifecycle affordance | accent for `Attached`/ready (RGBA hex) |
//! | `portal.lifecycle.attention_color` | lifecycle affordance | accent for `Degraded`/`HudUnavailable` (RGBA hex) |
//! | `portal.lifecycle.inactive_color` | lifecycle affordance | accent for `Detached`/`CleanupPending`/`Expired` (RGBA hex) |
//! | `portal.divider.color` | divider | separator line color (RGBA hex) |
//! | `portal.unread_divider.color` | unread divider | in-transcript unread-boundary rule color (RGBA hex) |
//! | `portal.collapsed_card.background` | collapsed card | compact view backdrop (RGBA hex) |
//! | `portal.collapsed_card.text_color` | collapsed card | compact text color (RGBA hex) |
//! | `portal.collapsed_card.font_size` | collapsed card | compact text font size in px |
//! | `portal.transition.in_ms` | transitions | collapsed→expanded duration (ms) |
//! | `portal.transition.out_ms` | transitions | expanded→collapsed duration (ms) |
//!
//! ### §6b Window management tokens (amendment 2026-06-10)
//!
//! | Key | Part | Property |
//! |-----|------|----------|
//! | `portal.window.min_width_px` | window bounds | legible minimum width in px |
//! | `portal.window.min_height_px` | window bounds | legible minimum height in px |
//! | `portal.window.resize_step_px` | hotkey resize | pixels per Ctrl+`+`/`-` step |
//! | `portal.window.resize_affordance_px` | resize affordance | capture region size on frame edges |
//! | `portal.scroll_indicator.color` | scroll indicator | thumb color (RGBA hex) |
//! | `portal.scroll_indicator.width_px` | scroll indicator | track width in px |
//! | `portal.scroll_indicator.min_height_px` | scroll indicator | minimum thumb height in px |
//!
//! ### Compliance amendment (portal visual-token gaps, hud-khfgx)
//!
//! | Key | Part | Property |
//! |-----|------|----------|
//! | `portal.composer.caret_color` | composer | caret glyph color (RGBA hex) |
//! | `portal.composer.selection_color` | composer | selection highlight (RGBA hex, alpha-bearing) |
//! | `portal.composer.placeholder_color` | composer | empty-draft placeholder text (RGBA hex) |
//! | `portal.focus_ring.color` | focus ring | ring stroke color (RGBA hex) |
//! | `portal.focus_ring.width_px` | focus ring | ring stroke width in px |
//! | `portal.window.resize_grip.color` | resize grip | grip mark color (RGBA hex) |
//! | `portal.window.resize_grip.hover_color` | resize grip | hover tint (RGBA hex) |
//! | `portal.window.resize_grip.size_px` | resize grip | grip square extent in px |
//! | `portal.spacing.content_inset_px` | spacing | content inset/padding in px |
//! | `portal.spacing.header_height_px` | spacing | header strip height in px |
//! | `portal.spacing.section_gap_px` | spacing | inter-section vertical gap in px |
//! | `portal.transcript.max_measure_px` | transcript | line measure cap in px (`0` = unbounded) |

use crate::tokens::{DesignTokenMap, Rgba, parse_color_hex, parse_numeric};
use tracing::warn;

// ── Canonical portal token keys ───────────────────────────────────────────────

/// Canonical portal token keys — pre-promotion profile-scoped defaults.
///
/// These are the authoritative key names for the portal part inventory.
/// At promotion time, a `text-portal` component type contract will canonicalize
/// them through the component-shape-language delta.
pub const PORTAL_TOKEN_FRAME_BACKGROUND: &str = "portal.frame.background";
pub const PORTAL_TOKEN_FRAME_OPACITY: &str = "portal.frame.opacity";
pub const PORTAL_TOKEN_FRAME_BORDER_COLOR: &str = "portal.frame.border_color";

pub const PORTAL_TOKEN_HEADER_TEXT_COLOR: &str = "portal.header.text_color";
pub const PORTAL_TOKEN_HEADER_FONT_SIZE: &str = "portal.header.font_size";

pub const PORTAL_TOKEN_COMPOSER_BACKGROUND: &str = "portal.composer.background";
pub const PORTAL_TOKEN_COMPOSER_TEXT_COLOR: &str = "portal.composer.text_color";
pub const PORTAL_TOKEN_COMPOSER_FONT_SIZE: &str = "portal.composer.font_size";
/// Border / indicator color shown when the composer draft reaches its byte cap.
/// Rendered as a visual signal that no further input will be accepted until
/// the user deletes content. Defaults to a muted amber to convey "limit reached"
/// without alarming the user.
pub const PORTAL_TOKEN_COMPOSER_AT_CAPACITY_COLOR: &str = "portal.composer.at_capacity_color";

pub const PORTAL_TOKEN_TRANSCRIPT_BACKGROUND: &str = "portal.transcript.background";
pub const PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR: &str = "portal.transcript.text_color";
pub const PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE: &str = "portal.transcript.font_size";

/// Per-turn role-attribution color for non-assistant agent-side OUTPUT turns
/// (tool / status / error / other), distinguishing them from the assistant's
/// own conversational prose (which uses `portal.transcript.text_color`) within
/// the same OUTPUT transcript (text-stream-portals §Conversational Turn Model
/// and Per-Turn Role Attribution, hud-26869). Attribution is derived from each
/// unit's runtime-assigned `OutputKind` and carried as a real `color_run` span
/// over that turn's text — never a hardcoded compositor color. Ambient by
/// design: a muted family-consistent hue that reads as "system/tool scaffolding"
/// beside the brighter assistant text without escalating attention.
pub const PORTAL_TOKEN_TRANSCRIPT_SYSTEM_COLOR: &str = "portal.transcript.system_color";

// ── Transcript markdown-subset styling (Promotion P2, hud-8691s) ─────────────
//
// Portal-scoped canonical keys for the Phase-1 markdown subset the transcript
// renders (fenced/inline code, links). Before promotion these values were only
// reachable via the generic `color.code.*` / `color.link.text` /
// `typography.code.family` keys, with no `portal.*` name a profile could target
// per the surface. The portal markdown consumer prefers these keys and falls
// back to the generic ones when unset, so a portal profile can restyle its own
// code/link treatment without disturbing the generic markdown defaults. Unset by
// default (no canonical schema default exists for the generic keys either), so
// the transcript renders exactly as today until a profile opts in.
//
// CONSTRAINT (hud-hjckr): the `portal.*`-over-generic preference is applied in
// the compositor's SINGLE global `MarkdownTokens` (the markdown parse cache is
// keyed on content only), so it is effectively GLOBAL. This is safe only while
// the text-stream portal is the sole governed markdown surface. Before ANY
// second governed markdown surface ships, markdown token resolution must become
// per-tile-scoped, or these portal keys will leak onto that surface.
/// Background fill behind fenced/inline code spans in the transcript (RGBA hex).
/// Prefer over the generic `color.code.background`.
pub const PORTAL_TOKEN_TRANSCRIPT_CODE_BACKGROUND: &str = "portal.transcript.code_background";
/// Foreground color for code spans in the transcript (RGBA hex). Prefer over the
/// generic `color.code.text`.
pub const PORTAL_TOKEN_TRANSCRIPT_CODE_TEXT: &str = "portal.transcript.code_text";
/// Link text color in the transcript (RGBA hex). Prefer over the generic
/// `color.link.text`.
pub const PORTAL_TOKEN_TRANSCRIPT_LINK_COLOR: &str = "portal.transcript.link_color";
/// Code font family for the transcript (`"monospace"` | `"sans-serif"` | …).
/// Prefer over the generic `typography.code.family`.
pub const PORTAL_TOKEN_TRANSCRIPT_CODE_FONT_FAMILY: &str = "portal.transcript.code_font_family";

// ── Degraded / disconnect tokens (portal-disconnect-resume-ux §2/§3) ─────────
//
// Token-resolved degraded treatment for a portal whose driving stream/session
// has dropped (lifecycle `Degraded`/`HudUnavailable`). The retained transcript
// window is dimmed and a content-free stale marker is shown rather than blanking
// or faking liveness. Every value here is token-driven — the adapter never
// hardcodes a degraded color (CLAUDE.md "visual identity is modular").

/// Dimmed transcript text color shown while the portal is disconnected/stale.
/// Distinctly muted relative to the live `transcript.text_color` so a viewer
/// reads the retained window as inactive without it vanishing.
pub const PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR: &str = "portal.transcript.dim_text_color";
/// Dimmed transcript background shown while the portal is disconnected/stale.
pub const PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND: &str = "portal.transcript.dim_background";
/// Color of the content-free stale/disconnect marker. Muted amber to convey
/// "ambient stale state" without escalating attention (spec: going stale does
/// not self-escalate attention).
pub const PORTAL_TOKEN_STALE_MARKER_COLOR: &str = "portal.stale_marker.color";

/// Color of the visible disconnect badge (hud-jgf41): a small token-styled
/// geometry affordance — the existing left-edge lifecycle-accent bar,
/// recolored — shown while the portal is connection-degraded. Unlike the
/// lifecycle accent (which clears under redaction because `lifecycle_state`
/// is redaction-gated), this badge is driven by the redaction-independent
/// `connection_degraded` flag, so a restricted viewer still sees that the
/// portal has dropped (geometry-only; no content or identity disclosed).
/// Shares the `STALE_MARKER_COLOR` hue by default so the geometry badge and
/// the text marker read as the same ambient state.
pub const PORTAL_TOKEN_DISCONNECT_BADGE_COLOR: &str = "portal.disconnect_badge.color";
/// Width (px) of the disconnect badge. Geometry only, mirrors
/// `PORTAL_TOKEN_LIFECYCLE_ACCENT_WIDTH_PX` since the badge reuses the same
/// left-edge accent-bar rendering.
pub const PORTAL_TOKEN_DISCONNECT_BADGE_WIDTH_PX: &str = "portal.disconnect_badge.width_px";

/// Color of the ambient unread-output-count indicator (hud-meqet). A presence
/// engine surfaces a quiet count, never a loud notification badge — this
/// defaults to the same muted-slate tone as the other ambient/quiet-signal
/// tokens (`transcript.dim_text_color`, `composer.placeholder_color`) rather
/// than an alarming accent.
pub const PORTAL_TOKEN_UNREAD_INDICATOR_COLOR: &str = "portal.unread_indicator.color";

/// Color of the ambient awaiting-reply (question) indicator (hud-jip0k). Set
/// when the owning LLM marks a published output as `expects_reply`, signaling
/// the just-published output is a question awaiting a viewer reply — a core
/// presence semantic, not a chatbot affordance. Ambient by design, matching
/// the muted tone of the other quiet-signal tokens; a presence engine never
/// escalates this into an alert.
pub const PORTAL_TOKEN_AWAITING_REPLY_COLOR: &str = "portal.awaiting_reply.color";

/// Color of the friendly first-run empty-portal treatment (hud-g1ena.6,
/// portal-chat-grade-affordances §First-Run Empty Portal Treatment) shown when a
/// connected portal's retained transcript is empty, replacing the literal
/// `<empty projection stream>`. Quiet and inviting — a presence engine's empty
/// surface reads as calm and ready, never as an error — so it defaults to the
/// same muted quiet-signal family as the other ambient tokens.
pub const PORTAL_TOKEN_EMPTY_STATE_COLOR: &str = "portal.empty_state.color";

/// Color of the connecting-state marker (portal-chat-grade-affordances
/// §Connecting State Distinction) shown while a portal is attached but its owning
/// session has never connected (`has_ever_connected == false`). Deliberately a
/// cool "spinning up" hue that is visually DISTINCT from the amber degraded/stale
/// marker (`portal.stale_marker.color`) so a starting-up portal never reads as a
/// failing one. Ambient by design — connecting is a quiet, non-attention signal,
/// so it matches the muted-tone convention of the other quiet-signal tokens.
pub const PORTAL_TOKEN_CONNECTING_MARKER_COLOR: &str = "portal.connecting_marker.color";

// ── Agent activity / streaming-cursor tokens ─────────────────────────────────
//
// Ambient cue that the owning adapter is actively appending to the transcript
// (portal-chat-grade-affordances §Agent Activity and Streaming Cue, hud-g1ena.5).
// DERIVED from observed append activity (`appended_at_wall_us` vs render-time
// now) — never from a separate adapter "typing" protocol message. Strictly
// subordinate to the ambient attention model: continuous streaming never
// re-escalates attention, and the cue quiesces promptly when appends stop.

/// Color of the ambient header activity cue shown while the agent is actively
/// appending (a compact typing-style indicator in the portal header). Muted by
/// design — a presence engine surfaces a quiet "writing" hint, never a loud
/// notification. Ambient tone, distinct from the amber (stale/at-capacity),
/// slate (unread/dim), periwinkle (awaiting-reply), sage (empty), and cyan
/// (connecting) signal tones so "writing" carries its own quiet meaning.
pub const PORTAL_TOKEN_ACTIVITY_CUE_COLOR: &str = "portal.activity_cue.color";
/// Color of the ambient streaming cursor painted at the transcript tail while
/// the agent is actively appending. Same activity semantic as
/// `portal.activity_cue.color`; kept as a separate key so a profile can style
/// the tail live-writing treatment independently of the header cue.
pub const PORTAL_TOKEN_STREAMING_CURSOR_COLOR: &str = "portal.streaming_cursor.color";

// ── Viewer-turn delivery-acknowledgement tokens ──────────────────────────────
//
// Ambient per-turn cue reflecting the runtime's already-tracked input delivery
// state on the viewer's ECHOED turn (portal-chat-grade-affordances §Viewer Turn
// Delivery Acknowledgement, hud-g1ena.1). DERIVED entirely from runtime-owned
// delivery state — never a new adapter round trip, and rendering it discloses no
// viewer read/seen state back to the adapter. Three presentation classes fold the
// six `InputDeliveryState` variants: in-flight (Pending/Deferred), delivered
// (Delivered/Handled), failed (Rejected/Expired). Strictly subordinate to the
// ambient attention model: a delivery transition is not an attention event, and a
// failed cue stays on the turn rather than escalating interruption class — hence
// muted tones, never an alarm red.

/// Color of the ambient in-flight (Pending/Deferred) viewer-turn delivery cue —
/// the reply has been submitted but the owning adapter has not yet taken
/// delivery. A calm "in transit" steel-blue, distinct from the amber
/// (stale/at-capacity), slate (unread/dim/timestamp), periwinkle
/// (awaiting-reply), sage (empty), cyan (connecting), and lavender (activity)
/// tones so "sending" carries its own quiet meaning.
pub const PORTAL_TOKEN_DELIVERY_INFLIGHT_COLOR: &str = "portal.delivery.inflight_color";
/// Color of the ambient delivered (Delivered/Handled) viewer-turn delivery cue —
/// the owning adapter has taken (or fully handled) the reply. A calm confirming
/// mint-teal; ambient by design (a quiet "reached" tick), never a loud success
/// flash.
pub const PORTAL_TOKEN_DELIVERY_DELIVERED_COLOR: &str = "portal.delivery.delivered_color";
/// Color of the ambient failed (Rejected/Expired) viewer-turn delivery cue — the
/// reply was rejected by the adapter or expired before delivery. A muted, warm
/// clay tone that reads as "did not arrive" WITHOUT the alarm of the amber stale
/// marker or a red error: per doctrine the failed cue stays ambient on the turn
/// and never escalates the portal's interruption class.
pub const PORTAL_TOKEN_DELIVERY_FAILED_COLOR: &str = "portal.delivery.failed_color";

// ── Ambient per-turn timestamp tokens ────────────────────────────────────────
//
// Ambient presentation of each transcript unit's RUNTIME-ASSIGNED wall-clock
// arrival metadata (`appended_at_wall_us`, WALL-CLOCK domain — distinct from the
// media/display clock, never conflated with presentation timing) per
// portal-chat-grade-affordances §Ambient Per-Turn Timestamps (hud-g1ena.4). The
// stamp is secondary/subordinate to turn content: quiet, token-styled, and never
// a source of attention escalation. Timestamp VISIBILITY/GRANULARITY is governed
// by the component profile via `portal.timestamp.granularity` (per-turn, grouped,
// or off) rather than mandated at the pixel level.

/// Color of the ambient per-turn arrival timestamp. Secondary presentation —
/// dim/muted so a stamp reads as subordinate metadata, never as primary turn
/// content or an alert. Defaults to the shared muted-slate quiet-signal tone
/// (matching `portal.transcript.dim_text_color` / `portal.unread_indicator.color`)
/// because a timestamp is quiet secondary metadata, not its own semantic accent.
pub const PORTAL_TOKEN_TIMESTAMP_COLOR: &str = "portal.timestamp.color";
/// Visibility/granularity of the ambient per-turn arrival timestamp, governed by
/// the portal's component profile: `off` (no stamps — the calm default), `per_turn`
/// (a stamp on every turn), or `grouped` (a stamp only when a turn's arrival minute
/// differs from the previous shown turn's, so consecutive same-minute turns share
/// one). Governs presentation, never the clock domain — the value always derives
/// from the runtime-assigned wall-clock arrival time.
pub const PORTAL_TOKEN_TIMESTAMP_GRANULARITY: &str = "portal.timestamp.granularity";

// ── Lifecycle affordance tokens (cooperative-hud-projection §lifecycle) ───────
//
// Token-resolved accent colors driving the viewer-facing affordance for a
// projection's published `lifecycle_state` (active / attached-ready /
// attention / inactive). Each is ambient by design — the cooperative-projection
// and text-stream-portal specs require portal activity to stay ambient/gentle and
// MUST NOT self-escalate interruption class. The adapter maps each
// `ProjectionLifecycleState` variant onto one of these four semantic accents; no
// literal color ever appears in the render path (CLAUDE.md "visual identity is
// modular").

/// Accent shown while the projected session is actively working (`Active`).
/// Calm teal-green: "live" without demanding attention.
pub const PORTAL_TOKEN_LIFECYCLE_ACTIVE_COLOR: &str = "portal.lifecycle.active_color";
/// Accent shown while the session is attached/ready but not actively producing
/// output (`Attached`). Soft blue: present and reachable.
pub const PORTAL_TOKEN_LIFECYCLE_ATTACHED_COLOR: &str = "portal.lifecycle.attached_color";
/// Accent shown while the session needs attention — degraded or HUD-unavailable
/// (`Degraded` / `HudUnavailable`). Ambient amber: "attention earned" without
/// alarming, consistent with the stale-marker convention.
pub const PORTAL_TOKEN_LIFECYCLE_ATTENTION_COLOR: &str = "portal.lifecycle.attention_color";
/// Accent shown while the session is winding down or gone — detached, cleanup
/// pending, or expired (`Detached` / `CleanupPending` / `Expired`). Muted slate:
/// reads as "inactive" without vanishing.
pub const PORTAL_TOKEN_LIFECYCLE_INACTIVE_COLOR: &str = "portal.lifecycle.inactive_color";
/// Width (px) of the lifecycle affordance accent bar painted along the portal
/// tile's left edge. Geometry only — the accent *color* is token-resolved via the
/// four `portal.lifecycle.*_color` keys above; this token sizes the bar so the
/// adapter/compositor never hardcodes a literal width either (hud-m48i0).
pub const PORTAL_TOKEN_LIFECYCLE_ACCENT_WIDTH_PX: &str = "portal.lifecycle.accent_width_px";

pub const PORTAL_TOKEN_DIVIDER_COLOR: &str = "portal.divider.color";

/// Color of the in-transcript unread divider (hud-g1ena.2,
/// portal-chat-grade-affordances §Unread Divider and Ambient Unread Count). The
/// rule marks the boundary before the oldest retained unseen agent-authored turn
/// when the viewer returns to a portal with unread content. Distinct from the
/// generic turn separator `portal.divider.color` so a viewer can tell the unread
/// boundary from an ordinary turn break, yet still ambient — a presence engine
/// marks where unread begins without behaving like a notification. Defaults to a
/// muted steel-blue in the cool ambient family, harmonizing with the slate
/// `portal.unread_indicator.color` count while reading brighter than the plain
/// separator.
pub const PORTAL_TOKEN_UNREAD_DIVIDER_COLOR: &str = "portal.unread_divider.color";

pub const PORTAL_TOKEN_COLLAPSED_BACKGROUND: &str = "portal.collapsed_card.background";
pub const PORTAL_TOKEN_COLLAPSED_TEXT_COLOR: &str = "portal.collapsed_card.text_color";
pub const PORTAL_TOKEN_COLLAPSED_FONT_SIZE: &str = "portal.collapsed_card.font_size";

pub const PORTAL_TOKEN_TRANSITION_IN_MS: &str = "portal.transition.in_ms";
pub const PORTAL_TOKEN_TRANSITION_OUT_MS: &str = "portal.transition.out_ms";

// ── §6b Window management tokens (amendment 2026-06-10) ──────────────────────

/// Minimum portal width in pixels (legibility bound per §6b.3).
pub const PORTAL_TOKEN_WINDOW_MIN_WIDTH_PX: &str = "portal.window.min_width_px";
/// Minimum portal height in pixels (legibility bound per §6b.3).
pub const PORTAL_TOKEN_WINDOW_MIN_HEIGHT_PX: &str = "portal.window.min_height_px";
/// Pixels per focus-scoped Ctrl+`+`/`=`/`-` resize step (§6b.2).
pub const PORTAL_TOKEN_WINDOW_RESIZE_STEP_PX: &str = "portal.window.resize_step_px";
/// Width/height of the pointer capture region on frame edges/corners (§6b.1).
pub const PORTAL_TOKEN_WINDOW_RESIZE_AFFORDANCE_PX: &str = "portal.window.resize_affordance_px";

// ── §6b Scroll-position indicator tokens ─────────────────────────────────────

/// Thumb color for scroll-position indicator (RGBA hex); redaction-safe (§6b.5).
pub const PORTAL_TOKEN_SCROLL_INDICATOR_COLOR: &str = "portal.scroll_indicator.color";
/// Track width of the scroll-position indicator in px (§6b.5).
pub const PORTAL_TOKEN_SCROLL_INDICATOR_WIDTH_PX: &str = "portal.scroll_indicator.width_px";
/// Minimum thumb height in px — prevents thumb from vanishing on deep content (§6b.5).
pub const PORTAL_TOKEN_SCROLL_INDICATOR_MIN_HEIGHT_PX: &str =
    "portal.scroll_indicator.min_height_px";

// ── Composer caret / selection / placeholder tokens ──────────────────────────
//
// Compliance (vd-caret-selection-placeholder-not-tokenized): the composer's caret
// glyph, selection highlight, and empty-draft placeholder are visual values that
// MUST resolve from design tokens rather than sharing the composer text color or
// living as inline compositor literals. A profile can now restyle each of the
// three independently. `selection_color` mirrors the compositor's existing inline
// default so this is a no-visual-regression tokenization; `caret_color` defaults
// to the composer text color so the default profile is visually unchanged.

/// Composer caret glyph color (RGBA hex). Sourced independently from the composer
/// text color so a profile can accent the caret. Defaults to the composer text
/// color for a no-visual-regression default.
pub const PORTAL_TOKEN_COMPOSER_CARET_COLOR: &str = "portal.composer.caret_color";
/// Composer selection-highlight background color (RGBA hex, alpha-bearing). Mirrors
/// the compositor's existing calm-blue selection tint.
pub const PORTAL_TOKEN_COMPOSER_SELECTION_COLOR: &str = "portal.composer.selection_color";
/// Composer empty-draft placeholder text color (RGBA hex). Dimmed relative to the
/// live composer text so an empty prompt reads as a hint, not typed content.
pub const PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR: &str = "portal.composer.placeholder_color";

// ── Focus-ring tokens (vd-focus-ring-outside-portal-tokens) ──────────────────
//
// The keyboard focus ring's color/width previously came only from
// `tze_hud_input::DEFAULT_FOCUS_RING_*`, outside profile control. These portal
// keys bring the ring under the portal token surface; the compositor already
// resolves `portal.focus_ring.*` from its token map, and these defaults mirror the
// `tze_hud_input` focus-ring blue (linear ≈ 0.2/0.5/1.0) at 2px so the visible
// ring is unchanged by default.

/// Keyboard focus-ring stroke color (RGBA hex).
pub const PORTAL_TOKEN_FOCUS_RING_COLOR: &str = "portal.focus_ring.color";
/// Keyboard focus-ring stroke width in px.
pub const PORTAL_TOKEN_FOCUS_RING_WIDTH_PX: &str = "portal.focus_ring.width_px";

// ── Resize-grip affordance tokens (vd-crude-resize-handle-grip) ──────────────
//
// The pointer resize affordance is a bare capture band with no legible grip. These
// tokens describe a dot-grid grip mark (color + hover tint + extent) so the
// affordance can be drawn from tokens instead of a plain tinted square. The grip
// RENDER itself is net-new (nothing is drawn today) and lands separately; these
// keys define the styling surface it and the exemplar consume.

/// Resize-grip mark color (RGBA hex) — the dot-grid/diagonal grip glyph.
pub const PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR: &str = "portal.window.resize_grip.color";
/// Resize-grip hover tint (RGBA hex) — brighter accent while the pointer is over
/// the resize affordance.
pub const PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR: &str =
    "portal.window.resize_grip.hover_color";
/// Resize-grip visual extent in px (the square the grip mark occupies at the
/// corner). Distinct from `portal.window.resize_affordance_px`, which sizes the
/// invisible pointer capture band.
pub const PORTAL_TOKEN_WINDOW_RESIZE_GRIP_SIZE_PX: &str = "portal.window.resize_grip.size_px";

// ── Spatial-rhythm + transcript-measure tokens (vd-no-token-rhythm-padding-measure) ─
//
// Portal padding, header height, inter-section gap, and the transcript's optimal
// line measure were ad-hoc numeric literals. These tokens make the portal's
// spatial rhythm profile-controlled. Defaults match the current composer inset;
// `max_measure_px = 0` means "unbounded" (no readability cap), preserving today's
// full-width transcript wrapping until a profile opts into a narrower measure.

/// Content inset (px) applied inside portal surfaces (composer/transcript padding).
pub const PORTAL_TOKEN_SPACING_CONTENT_INSET_PX: &str = "portal.spacing.content_inset_px";
/// Portal header strip height in px.
pub const PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX: &str = "portal.spacing.header_height_px";
/// Vertical gap (px) between stacked portal sections (header / transcript / composer).
pub const PORTAL_TOKEN_SPACING_SECTION_GAP_PX: &str = "portal.spacing.section_gap_px";
/// Maximum transcript line measure in px before wrapping (readability cap).
/// `0` means unbounded — wrap to the full transcript width (current behavior).
pub const PORTAL_TOKEN_TRANSCRIPT_MAX_MEASURE_PX: &str = "portal.transcript.max_measure_px";

// ── Ambient per-turn timestamp granularity ───────────────────────────────────

/// Component-profile-governed visibility/granularity of the ambient per-turn
/// arrival timestamp (portal-chat-grade-affordances §Ambient Per-Turn Timestamps,
/// hud-g1ena.4). Resolved from [`PORTAL_TOKEN_TIMESTAMP_GRANULARITY`].
///
/// This governs PRESENTATION only — how often a stamp is shown — never the clock
/// domain. Every presented time still derives from the runtime-assigned
/// wall-clock arrival metadata (`appended_at_wall_us`), which the adapter cannot
/// forge. `OnDemand` visibility (reveal on hover/focus) is a promotion-era
/// interactive affordance and is intentionally not part of the Phase-1 single-node
/// render set; a profile that wants it uses `Off` until it ships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimestampGranularity {
    /// No per-turn timestamps. The calm default: a profile opts stamps in rather
    /// than the base surface mandating them (visibility is profile-governed).
    #[default]
    Off,
    /// A wall-clock arrival stamp on every retained turn.
    PerTurn,
    /// A wall-clock arrival stamp only when a turn's arrival MINUTE differs from
    /// the previous shown turn's — consecutive same-minute turns share one stamp,
    /// keeping the transcript quiet during bursts.
    Grouped,
}

impl TimestampGranularity {
    /// Parse the profile token string. Accepts `off`, `per_turn`/`per-turn`, and
    /// `grouped` (ASCII-case-insensitive); any other value returns `None` so the
    /// resolver falls back to the default, mirroring the warn-and-default handling
    /// of the color/numeric portal tokens.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "per_turn" | "per-turn" => Some(Self::PerTurn),
            "grouped" => Some(Self::Grouped),
            _ => None,
        }
    }

    /// Canonical token string for this granularity — the inverse of [`parse`] and
    /// the value carried on the resolved handshake string map.
    ///
    /// [`parse`]: Self::parse
    pub fn as_token_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::PerTurn => "per_turn",
            Self::Grouped => "grouped",
        }
    }
}

// ── Portal token fallback defaults ───────────────────────────────────────────

/// Default values for portal tokens (used when token is absent from resolved map).
///
/// These defaults are deliberately distinct from the 30 canonical tokens so the
/// profile-swap test can distinguish between the canonical and portal layers.
/// Colors use the same palette as the existing exemplar adapter literals,
/// expressed as resolved token defaults rather than inline constants.
///
/// These string constants are the **single source of truth** for the default portal
/// palette. `tze_hud_projection::resident_grpc::PortalVisualTokens::default()` derives
/// from `PortalPartTokens::default()` (which parses these constants) via
/// `portal_visual_tokens_from_part_tokens`, so changing a value here propagates to
/// both sides automatically (hud-dcynv consolidation).
mod defaults {
    /// Portal frame backdrop fill (RGBA hex). Opaque near-black that matches the
    /// transcript pane (`TRANSCRIPT_BACKGROUND`) so the thin frame gap around the
    /// panes reads as off-black — backdrop-independent (identical regardless of
    /// the desktop wallpaper behind the HUD).
    ///
    /// hud-a328c: this was `#111720` — an OPAQUE slate that resolves to
    /// rgba(0.067,0.090,0.125,1.0). On the runtime-handshake token path (the
    /// production path: the exemplar/portal driver adopts the runtime's resolved
    /// tokens) an unset `portal.frame.background` falls back to this default, so
    /// the opaque slate painted the frame rim a visible GREY around the black
    /// panes. Owner live A/B on the real-GPU HUD chose the opaque near-black
    /// `#0A0D11` for backdrop-independence and pane-color match. (The exemplar's
    /// translucent-black glass `#0000004D` — what `--ignore-runtime-tokens`
    /// rendered — was rejected because 30%-black varies with the wallpaper
    /// behind it.) Aligning the canonical default to the reviewed value fixes
    /// the handshake, in-process, and bridged drivers from a single source.
    pub const FRAME_BACKGROUND: &str = "#0A0D11";
    pub const FRAME_OPACITY: &str = "0.98";
    pub const FRAME_BORDER_COLOR: &str = "#2A3344";

    pub const HEADER_TEXT_COLOR: &str = "#F5F8FF";
    pub const HEADER_FONT_SIZE: &str = "16";

    /// Reviewed native two-pane INPUT fill: black at 95% opacity. Keeping this
    /// in the canonical portal profile makes resident projections and the
    /// exemplar resolve the same pane treatment without adapter-authored style.
    pub const COMPOSER_BACKGROUND: &str = "#000000F2";
    pub const COMPOSER_TEXT_COLOR: &str = "#E0E8F4";
    pub const COMPOSER_FONT_SIZE: &str = "16";
    /// Muted amber — conveys "limit reached" without alarming the user.
    pub const COMPOSER_AT_CAPACITY_COLOR: &str = "#B87333";

    /// Reviewed native two-pane OUTPUT fill: black at 95% opacity.
    pub const TRANSCRIPT_BACKGROUND: &str = "#000000F2";
    pub const TRANSCRIPT_TEXT_COLOR: &str = "#E6EFFA";
    pub const TRANSCRIPT_FONT_SIZE: &str = "16";
    /// Per-turn role attribution for non-assistant agent-side turns (tool /
    /// status / error / other). A cool slate — dimmer and bluer than the bright
    /// assistant text (`TRANSCRIPT_TEXT_COLOR` #E6EFFA) so tool/system
    /// scaffolding recedes behind the model's own prose, while staying in the
    /// portal's cool ambient family (near the slate unread count / divider).
    pub const TRANSCRIPT_SYSTEM_COLOR: &str = "#8A97AD";

    // Degraded / disconnect treatment (§2/§3). Dim text/background read as
    // "inactive" relative to the live transcript palette above; the stale
    // marker uses a muted amber (matching the at-capacity convention) so it is
    // ambient, not alarming.
    pub const TRANSCRIPT_DIM_TEXT_COLOR: &str = "#6B7689";
    pub const TRANSCRIPT_DIM_BACKGROUND: &str = "#07090C";
    pub const STALE_MARKER_COLOR: &str = "#B87333";
    /// Same hue as `STALE_MARKER_COLOR` — the geometry badge and the text
    /// marker read as one ambient "stale" signal (hud-jgf41).
    pub const DISCONNECT_BADGE_COLOR: &str = "#B87333";
    /// Mirrors `LIFECYCLE_ACCENT_WIDTH_PX`: the badge reuses the same
    /// left-edge accent-bar geometry.
    pub const DISCONNECT_BADGE_WIDTH_PX: &str = "4";
    /// Muted slate — same ambient tone as `TRANSCRIPT_DIM_TEXT_COLOR` /
    /// `COMPOSER_PLACEHOLDER_COLOR` so the unread count reads as a quiet
    /// signal, not an alert.
    pub const UNREAD_INDICATOR_COLOR: &str = "#6B7689";
    /// Muted periwinkle — distinct hue from the amber (stale/at-capacity) and
    /// slate (unread/dim) ambient tones so a question reads as its own quiet
    /// signal rather than colliding with an existing meaning.
    pub const AWAITING_REPLY_COLOR: &str = "#7B85C4";
    /// Muted sage — a calm, welcoming green-slate for the first-run empty state,
    /// distinct from the amber (stale/at-capacity), slate (unread/dim), and
    /// periwinkle (awaiting-reply) tones so an empty surface reads as its own
    /// quiet "ready" signal rather than an error or an existing meaning.
    pub const EMPTY_STATE_COLOR: &str = "#5F8A78";
    /// Muted cyan — a calm "spinning up" hue for the never-connected connecting
    /// state (§Connecting State Distinction). Its cool cyan is deliberately
    /// distinct from the amber `STALE_MARKER_COLOR` (degraded/disconnected) so a
    /// starting-up portal never reads as failing; it is also distinct from the
    /// sage empty-state and periwinkle awaiting-reply tones so connecting carries
    /// its own quiet meaning.
    pub const CONNECTING_MARKER_COLOR: &str = "#4C93A6";

    /// Muted lavender-grey — a calm "writing" hue for the ambient agent-activity
    /// header cue. Distinct from the amber (stale/at-capacity), slate
    /// (unread/dim), periwinkle (awaiting-reply), sage (empty), and cyan
    /// (connecting) tones so an active-append signal reads as its own quiet
    /// meaning rather than colliding with an existing one, and deliberately
    /// low-contrast so continuous streaming never escalates attention.
    pub const ACTIVITY_CUE_COLOR: &str = "#8A8FB0";
    /// Muted lavender — the transcript-tail streaming cursor. Shares the activity
    /// family with `ACTIVITY_CUE_COLOR` but a hair brighter so the tail cursor is
    /// perceptible as "live writing here" while staying ambient.
    pub const STREAMING_CURSOR_COLOR: &str = "#A6ABC8";

    /// Muted steel-blue — the ambient in-flight (Pending/Deferred) viewer-turn
    /// delivery cue. A calm "in transit" tone distinct from the amber
    /// (stale/at-capacity), slate (unread/dim/timestamp), periwinkle
    /// (awaiting-reply), sage (empty), cyan (connecting), and lavender (activity)
    /// families so "sending" reads as its own quiet signal.
    pub const DELIVERY_INFLIGHT_COLOR: &str = "#6E8CA8";
    /// Muted mint-teal — the ambient delivered (Delivered/Handled) viewer-turn
    /// delivery cue. A calm confirming "reached" tone; kept ambient (a hair cooler
    /// and clearer than the sage empty-state green) so a delivery tick never reads
    /// as a loud success flash.
    pub const DELIVERY_DELIVERED_COLOR: &str = "#57A88E";
    /// Muted warm clay — the ambient failed (Rejected/Expired) viewer-turn delivery
    /// cue. Deliberately NOT the amber `STALE_MARKER_COLOR` nor an alarm red: per
    /// doctrine a failed delivery stays ambient on the turn and never escalates the
    /// portal's interruption class, so the tone reads as "did not arrive" quietly.
    pub const DELIVERY_FAILED_COLOR: &str = "#B5827A";

    /// Muted slate — the ambient per-turn arrival timestamp. Shares the quiet
    /// secondary tone of `TRANSCRIPT_DIM_TEXT_COLOR` / `UNREAD_INDICATOR_COLOR` /
    /// `COMPOSER_PLACEHOLDER_COLOR` so a stamp reads as subordinate metadata, not
    /// as primary turn content or an alert (§secondary presentation).
    pub const TIMESTAMP_COLOR: &str = "#6B7689";
    /// Ambient timestamps default OFF — visibility is profile-governed and the base
    /// surface stays calm (a presence engine, not a chat log). A component profile
    /// opts in with `per_turn` or `grouped`. Mirrors [`TimestampGranularity::Off`].
    pub const TIMESTAMP_GRANULARITY: &str = "off";

    // Lifecycle affordance accents — ambient, mutually distinct (see token-key
    // docs above). Active: calm teal-green; attached/ready: soft blue;
    // attention: amber (distinct from the stale marker); inactive: muted slate.
    pub const LIFECYCLE_ACTIVE_COLOR: &str = "#4FA88A";
    pub const LIFECYCLE_ATTACHED_COLOR: &str = "#5A8FC0";
    pub const LIFECYCLE_ATTENTION_COLOR: &str = "#C28A3D";
    pub const LIFECYCLE_INACTIVE_COLOR: &str = "#5A6373";
    /// Left-edge accent bar width (px). Slim enough to read as a margin marker
    /// (like an editor's gutter indicator) without crowding the transcript text.
    pub const LIFECYCLE_ACCENT_WIDTH_PX: &str = "4";

    pub const DIVIDER_COLOR: &str = "#46536E";
    /// Muted steel-blue — brighter and bluer than the plain turn separator
    /// (`DIVIDER_COLOR` #46536E) so the unread boundary reads as distinct, while
    /// staying in the cool ambient family alongside the slate unread count
    /// (`UNREAD_INDICATOR_COLOR` #6B7689). Ambient, not alarming.
    pub const UNREAD_DIVIDER_COLOR: &str = "#586A8C";

    // Collapsed/minimized portal card backdrop. Off-black in the portal palette
    // family — a hair lifted from the frame (`FRAME_BACKGROUND` #0A0D11) so a
    // minimized card stays perceptible against the desktop, but no longer reads
    // as the grey it used to (`#1A1F28`). hud-0hj7f: the frame fix (hud-a328c,
    // #0A0D11) only covered the expanded frame; the minimized state kept a
    // separate grey token, so window-mgmt minimize still flashed grey.
    pub const COLLAPSED_BACKGROUND: &str = "#12161C";
    pub const COLLAPSED_TEXT_COLOR: &str = "#C8D6E8";
    pub const COLLAPSED_FONT_SIZE: &str = "14";

    pub const TRANSITION_IN_MS: &str = "120";
    pub const TRANSITION_OUT_MS: &str = "80";

    // §6b window management defaults
    /// Legible minimum portal width (px). Chosen to fit at least two columns of
    /// readable text at default font size.
    pub const WINDOW_MIN_WIDTH_PX: &str = "240";
    /// Legible minimum portal height (px). Chosen to fit at least three lines of
    /// text plus header and composer.
    pub const WINDOW_MIN_HEIGHT_PX: &str = "160";
    /// Pixels per Ctrl+`+`/`-` resize step. Large enough to feel meaningful,
    /// small enough for fine control.
    pub const WINDOW_RESIZE_STEP_PX: &str = "32";
    /// Width of the pointer capture region on frame edges/corners (px).
    /// 8px balances discoverability with accidental activation avoidance.
    pub const WINDOW_RESIZE_AFFORDANCE_PX: &str = "8";

    // §6b scroll-indicator defaults
    pub const SCROLL_INDICATOR_COLOR: &str = "#4A5568";
    pub const SCROLL_INDICATOR_WIDTH_PX: &str = "4";
    pub const SCROLL_INDICATOR_MIN_HEIGHT_PX: &str = "24";

    // Composer caret / selection / placeholder defaults.
    /// Caret glyph color — equal to `COMPOSER_TEXT_COLOR` so the default profile's
    /// caret is visually unchanged; a profile may override to accent the caret.
    pub const COMPOSER_CARET_COLOR: &str = "#E0E8F4";
    /// Selection highlight — calm blue at ~0.45 alpha; mirrors the compositor's
    /// existing inline default (`#3A7BD5` @ 0x73).
    pub const COMPOSER_SELECTION_COLOR: &str = "#3A7BD573";
    /// Empty-draft placeholder text — dimmed slate (matches the disconnect dim
    /// text) so an empty prompt reads as a hint.
    pub const COMPOSER_PLACEHOLDER_COLOR: &str = "#6B7689";

    // Focus-ring defaults — mirror `tze_hud_input::DEFAULT_FOCUS_RING_*`
    // (linear ≈ 0.2/0.5/1.0 at 2px). Expressed as the same sRGB fractions.
    pub const FOCUS_RING_COLOR: &str = "#3380FF";
    pub const FOCUS_RING_WIDTH_PX: &str = "2";

    // Resize-grip affordance defaults — a muted-slate dot grip that brightens on
    // hover, occupying a 14px corner square.
    pub const WINDOW_RESIZE_GRIP_COLOR: &str = "#5A6373";
    pub const WINDOW_RESIZE_GRIP_HOVER_COLOR: &str = "#8A93A6";
    pub const WINDOW_RESIZE_GRIP_SIZE_PX: &str = "14";

    // Spatial-rhythm + transcript-measure defaults.
    /// Content inset — matches the composer text margin the compositor uses today.
    pub const SPACING_CONTENT_INSET_PX: &str = "6";
    /// Header strip height. Matches the header/drag-band height the compositor
    /// uses today (the declared surface `Header` part drives the draggable header
    /// band via `SceneGraph::portal_header_band_anchors`, so this default MUST
    /// equal `PORTAL_HEADER_DRAG_BAND_PX_DEFAULT` = 52 to keep the band unchanged;
    /// hud-zn6yw). `portal.header.drag_band_px` remains the raw-tile-fallback knob.
    pub const SPACING_HEADER_HEIGHT_PX: &str = "52";
    /// Inter-section vertical gap.
    pub const SPACING_SECTION_GAP_PX: &str = "8";
    /// Transcript measure cap — `0` = unbounded (wrap to full width, today's behavior).
    pub const TRANSCRIPT_MAX_MEASURE_PX: &str = "0";
}

// ── PortalPartTokens ──────────────────────────────────────────────────────────

/// Resolved visual properties for each portal surface part.
///
/// Constructed from a `DesignTokenMap` via [`resolve_portal_tokens`]. Every
/// field is already parsed from its token string representation — the adapter
/// uses these values directly when building scene mutations.
///
/// **No literal colors/sizes are permitted in the adapter publish path.** All
/// visual properties MUST flow through this struct. This is the pre-promotion
/// enforcement of "visual identity is modular" (CLAUDE.md core rule).
#[derive(Clone, Debug, PartialEq)]
pub struct PortalPartTokens {
    // Frame (outer backdrop + border)
    pub frame_background: Rgba,
    pub frame_opacity: f32,
    pub frame_border_color: Rgba,

    // Header strip
    pub header_text_color: Rgba,
    pub header_font_size_px: f32,

    // Composer (input area)
    pub composer_background: Rgba,
    pub composer_text_color: Rgba,
    pub composer_font_size_px: f32,
    /// Indicator color rendered when the draft is at its byte cap (§4.1 / §4.8).
    /// Applied as a distinct visual signal within the composer region; never
    /// hardcoded in the compositor — always token-driven per CLAUDE.md doctrine.
    pub composer_at_capacity_color: Rgba,

    // Transcript body
    pub transcript_background: Rgba,
    pub transcript_text_color: Rgba,
    pub transcript_font_size_px: f32,
    /// Per-turn role-attribution color for non-assistant agent-side OUTPUT turns
    /// (tool / status / error / other), distinct from `transcript_text_color`
    /// used for the assistant's own conversational turns (text-stream-portals
    /// §Conversational Turn Model and Per-Turn Role Attribution, hud-26869).
    pub transcript_system_color: Rgba,

    // Degraded / disconnect treatment (portal-disconnect-resume-ux §2/§3).
    /// Dimmed transcript text shown while the portal is disconnected/stale.
    pub transcript_dim_text_color: Rgba,
    /// Dimmed transcript background shown while the portal is disconnected/stale.
    pub transcript_dim_background: Rgba,
    /// Color of the content-free stale/disconnect marker (ambient, not alarming).
    pub stale_marker_color: Rgba,
    /// Color of the visible disconnect badge (hud-jgf41): a token-styled
    /// geometry affordance shown while connection-degraded, driven by the
    /// redaction-independent `connection_degraded` flag so it stays visible
    /// for a restricted viewer (geometry-only).
    pub disconnect_badge_color: Rgba,
    /// Width (px) of the disconnect badge accent bar.
    pub disconnect_badge_width_px: f32,
    /// Color of the ambient unread-output-count indicator (hud-meqet). Muted by
    /// design — a presence engine surfaces a quiet count, never a loud
    /// notification badge.
    pub unread_indicator_color: Rgba,
    /// Color of the ambient awaiting-reply (question) indicator (hud-jip0k).
    /// Set when the owning LLM's most recently published output has
    /// `expects_reply == true`. Ambient by design, matching the muted tone
    /// convention of the other quiet-signal tokens.
    pub awaiting_reply_color: Rgba,
    /// Color of the friendly first-run empty-portal treatment (hud-g1ena.6)
    /// shown when a connected portal's retained transcript is empty. Quiet and
    /// inviting — reads as calm and ready, never as an error.
    pub empty_state_color: Rgba,
    /// Color of the connecting-state marker (portal-chat-grade-affordances
    /// §Connecting State Distinction) shown while a portal is attached but has
    /// never connected. Cool "spinning up" hue, visually distinct from the amber
    /// `stale_marker_color` so a starting-up portal does not read as failing.
    pub connecting_marker_color: Rgba,

    // Agent activity / streaming cursor (portal-chat-grade-affordances
    // §Agent Activity and Streaming Cue, hud-g1ena.5). Ambient cues derived from
    // observed appends; the adapter never hardcodes these hues.
    /// Color of the ambient header activity cue shown while the agent is actively
    /// appending (compact typing-style header indicator).
    pub activity_cue_color: Rgba,
    /// Color of the ambient transcript-tail streaming cursor shown while the
    /// agent is actively appending.
    pub streaming_cursor_color: Rgba,

    // Viewer-turn delivery acknowledgement (portal-chat-grade-affordances
    // §Viewer Turn Delivery Acknowledgement, hud-g1ena.1). Three ambient cue
    // classes derived from the runtime's already-tracked `InputDeliveryState`; the
    // adapter never hardcodes these hues and never issues a round trip to render
    // them.
    /// Color of the ambient in-flight (Pending/Deferred) viewer-turn delivery cue.
    pub delivery_inflight_color: Rgba,
    /// Color of the ambient delivered (Delivered/Handled) viewer-turn delivery cue.
    pub delivery_delivered_color: Rgba,
    /// Color of the ambient failed (Rejected/Expired) viewer-turn delivery cue.
    /// Muted, not an alarm: a failed cue stays ambient on the turn.
    pub delivery_failed_color: Rgba,

    // Ambient per-turn timestamps (portal-chat-grade-affordances
    // §Ambient Per-Turn Timestamps, hud-g1ena.4). Secondary presentation of each
    // unit's runtime-assigned wall-clock arrival time; the adapter never hardcodes
    // the hue or the granularity.
    /// Color of the ambient per-turn arrival timestamp (secondary/dim).
    pub timestamp_color: Rgba,
    /// Visibility/granularity of the ambient per-turn arrival timestamp, governed
    /// by the component profile (`off` / `per_turn` / `grouped`).
    pub timestamp_granularity: TimestampGranularity,

    // Lifecycle affordance accents (cooperative-hud-projection §lifecycle).
    // Each maps a `ProjectionLifecycleState` group onto an ambient accent; the
    // adapter never hardcodes a lifecycle color.
    /// Accent for the actively-working state (`Active`).
    pub lifecycle_active_color: Rgba,
    /// Accent for the attached/ready state (`Attached`).
    pub lifecycle_attached_color: Rgba,
    /// Accent for attention states (`Degraded` / `HudUnavailable`).
    pub lifecycle_attention_color: Rgba,
    /// Accent for winding-down states (`Detached` / `CleanupPending` / `Expired`).
    pub lifecycle_inactive_color: Rgba,
    /// Width (px) of the left-edge lifecycle accent bar. Geometry only — the bar
    /// *color* comes from the four accents above; this keeps the adapter and
    /// compositor free of any literal accent dimension (hud-m48i0).
    pub lifecycle_accent_width_px: f32,

    // Divider
    pub divider_color: Rgba,
    /// Color of the in-transcript unread divider (hud-g1ena.2). Marks the boundary
    /// before the oldest retained unseen agent-authored turn; distinct from the
    /// generic `divider_color` turn separator, ambient by design.
    pub unread_divider_color: Rgba,

    // Collapsed card
    pub collapsed_background: Rgba,
    pub collapsed_text_color: Rgba,
    pub collapsed_font_size_px: f32,

    // Transitions (zone-transition duration)
    pub transition_in_ms: u32,
    pub transition_out_ms: u32,

    // §6b Window management (amendment 2026-06-10)
    /// Legible minimum portal width in pixels (§6b.3 legibility bound).
    pub window_min_width_px: f32,
    /// Legible minimum portal height in pixels (§6b.3 legibility bound).
    pub window_min_height_px: f32,
    /// Pixels per Ctrl+`+`/`=`/`-` resize step (§6b.2 token-defined step).
    pub window_resize_step_px: f32,
    /// Width/height of pointer capture region on frame edges/corners (§6b.1).
    pub window_resize_affordance_px: f32,

    // §6b Scroll-position indicators (amendment 2026-06-10)
    /// Scroll indicator thumb color — geometry-only, carries no content (§6b.5).
    pub scroll_indicator_color: Rgba,
    /// Scroll indicator track width in px (§6b.5).
    pub scroll_indicator_width_px: f32,
    /// Minimum scroll indicator thumb height in px (§6b.5).
    pub scroll_indicator_min_height_px: f32,

    // Composer caret / selection / placeholder (vd-caret-selection-placeholder-not-tokenized).
    /// Caret glyph color — sourced independently from the composer text color.
    pub composer_caret_color: Rgba,
    /// Selection-highlight background color (alpha-bearing).
    pub composer_selection_color: Rgba,
    /// Empty-draft placeholder text color (dimmed hint).
    pub composer_placeholder_color: Rgba,

    // Keyboard focus ring (vd-focus-ring-outside-portal-tokens).
    /// Focus-ring stroke color — under portal profile control, no longer only
    /// `tze_hud_input::DEFAULT_FOCUS_RING_COLOR`.
    pub focus_ring_color: Rgba,
    /// Focus-ring stroke width in px.
    pub focus_ring_width_px: f32,

    // Resize-grip affordance (vd-crude-resize-handle-grip).
    /// Resize-grip mark color (the dot-grid grip glyph).
    pub resize_grip_color: Rgba,
    /// Resize-grip hover tint.
    pub resize_grip_hover_color: Rgba,
    /// Resize-grip visual extent in px (the grip square at the corner).
    pub resize_grip_size_px: f32,

    // Spatial rhythm + transcript measure (vd-no-token-rhythm-padding-measure).
    /// Content inset (px) inside portal surfaces.
    pub content_inset_px: f32,
    /// Header strip height in px.
    pub header_height_px: f32,
    /// Vertical gap (px) between stacked portal sections.
    pub section_gap_px: f32,
    /// Transcript optimal-measure cap in px; `0.0` = unbounded (full-width wrap).
    pub transcript_max_measure_px: f32,
}

impl Default for PortalPartTokens {
    fn default() -> Self {
        Self {
            frame_background: parse_color_hex(defaults::FRAME_BACKGROUND)
                .expect("frame background default is valid hex"),
            frame_opacity: parse_numeric(defaults::FRAME_OPACITY)
                .expect("frame opacity default is valid numeric"),
            frame_border_color: parse_color_hex(defaults::FRAME_BORDER_COLOR)
                .expect("frame border default is valid hex"),

            header_text_color: parse_color_hex(defaults::HEADER_TEXT_COLOR)
                .expect("header text default is valid hex"),
            header_font_size_px: parse_numeric(defaults::HEADER_FONT_SIZE)
                .expect("header font size default is valid numeric"),

            composer_background: parse_color_hex(defaults::COMPOSER_BACKGROUND)
                .expect("composer background default is valid hex"),
            composer_text_color: parse_color_hex(defaults::COMPOSER_TEXT_COLOR)
                .expect("composer text default is valid hex"),
            composer_font_size_px: parse_numeric(defaults::COMPOSER_FONT_SIZE)
                .expect("composer font size default is valid numeric"),
            composer_at_capacity_color: parse_color_hex(defaults::COMPOSER_AT_CAPACITY_COLOR)
                .expect("composer at-capacity color default is valid hex"),

            transcript_background: parse_color_hex(defaults::TRANSCRIPT_BACKGROUND)
                .expect("transcript background default is valid hex"),
            transcript_text_color: parse_color_hex(defaults::TRANSCRIPT_TEXT_COLOR)
                .expect("transcript text default is valid hex"),
            transcript_font_size_px: parse_numeric(defaults::TRANSCRIPT_FONT_SIZE)
                .expect("transcript font size default is valid numeric"),
            transcript_system_color: parse_color_hex(defaults::TRANSCRIPT_SYSTEM_COLOR)
                .expect("transcript system-attribution default is valid hex"),

            transcript_dim_text_color: parse_color_hex(defaults::TRANSCRIPT_DIM_TEXT_COLOR)
                .expect("transcript dim text default is valid hex"),
            transcript_dim_background: parse_color_hex(defaults::TRANSCRIPT_DIM_BACKGROUND)
                .expect("transcript dim background default is valid hex"),
            stale_marker_color: parse_color_hex(defaults::STALE_MARKER_COLOR)
                .expect("stale marker color default is valid hex"),
            disconnect_badge_color: parse_color_hex(defaults::DISCONNECT_BADGE_COLOR)
                .expect("disconnect badge color default is valid hex"),
            disconnect_badge_width_px: parse_numeric(defaults::DISCONNECT_BADGE_WIDTH_PX)
                .expect("disconnect badge width default is valid numeric"),
            unread_indicator_color: parse_color_hex(defaults::UNREAD_INDICATOR_COLOR)
                .expect("unread indicator color default is valid hex"),
            awaiting_reply_color: parse_color_hex(defaults::AWAITING_REPLY_COLOR)
                .expect("awaiting reply color default is valid hex"),
            empty_state_color: parse_color_hex(defaults::EMPTY_STATE_COLOR)
                .expect("empty state color default is valid hex"),
            connecting_marker_color: parse_color_hex(defaults::CONNECTING_MARKER_COLOR)
                .expect("connecting marker color default is valid hex"),

            activity_cue_color: parse_color_hex(defaults::ACTIVITY_CUE_COLOR)
                .expect("activity cue color default is valid hex"),
            streaming_cursor_color: parse_color_hex(defaults::STREAMING_CURSOR_COLOR)
                .expect("streaming cursor color default is valid hex"),

            delivery_inflight_color: parse_color_hex(defaults::DELIVERY_INFLIGHT_COLOR)
                .expect("delivery in-flight color default is valid hex"),
            delivery_delivered_color: parse_color_hex(defaults::DELIVERY_DELIVERED_COLOR)
                .expect("delivery delivered color default is valid hex"),
            delivery_failed_color: parse_color_hex(defaults::DELIVERY_FAILED_COLOR)
                .expect("delivery failed color default is valid hex"),

            timestamp_color: parse_color_hex(defaults::TIMESTAMP_COLOR)
                .expect("timestamp color default is valid hex"),
            timestamp_granularity: TimestampGranularity::parse(defaults::TIMESTAMP_GRANULARITY)
                .expect("timestamp granularity default is a valid granularity"),

            lifecycle_active_color: parse_color_hex(defaults::LIFECYCLE_ACTIVE_COLOR)
                .expect("lifecycle active color default is valid hex"),
            lifecycle_attached_color: parse_color_hex(defaults::LIFECYCLE_ATTACHED_COLOR)
                .expect("lifecycle attached color default is valid hex"),
            lifecycle_attention_color: parse_color_hex(defaults::LIFECYCLE_ATTENTION_COLOR)
                .expect("lifecycle attention color default is valid hex"),
            lifecycle_inactive_color: parse_color_hex(defaults::LIFECYCLE_INACTIVE_COLOR)
                .expect("lifecycle inactive color default is valid hex"),
            lifecycle_accent_width_px: parse_numeric(defaults::LIFECYCLE_ACCENT_WIDTH_PX)
                .expect("lifecycle accent width default is valid numeric"),

            divider_color: parse_color_hex(defaults::DIVIDER_COLOR)
                .expect("divider color default is valid hex"),
            unread_divider_color: parse_color_hex(defaults::UNREAD_DIVIDER_COLOR)
                .expect("unread divider color default is valid hex"),

            collapsed_background: parse_color_hex(defaults::COLLAPSED_BACKGROUND)
                .expect("collapsed background default is valid hex"),
            collapsed_text_color: parse_color_hex(defaults::COLLAPSED_TEXT_COLOR)
                .expect("collapsed text default is valid hex"),
            collapsed_font_size_px: parse_numeric(defaults::COLLAPSED_FONT_SIZE)
                .expect("collapsed font size default is valid numeric"),

            transition_in_ms: parse_numeric(defaults::TRANSITION_IN_MS)
                .expect("transition in default is valid numeric")
                as u32,
            transition_out_ms: parse_numeric(defaults::TRANSITION_OUT_MS)
                .expect("transition out default is valid numeric")
                as u32,

            // §6b window management defaults
            window_min_width_px: parse_numeric(defaults::WINDOW_MIN_WIDTH_PX)
                .expect("window min width default is valid numeric"),
            window_min_height_px: parse_numeric(defaults::WINDOW_MIN_HEIGHT_PX)
                .expect("window min height default is valid numeric"),
            window_resize_step_px: parse_numeric(defaults::WINDOW_RESIZE_STEP_PX)
                .expect("window resize step default is valid numeric"),
            window_resize_affordance_px: parse_numeric(defaults::WINDOW_RESIZE_AFFORDANCE_PX)
                .expect("window resize affordance default is valid numeric"),

            // §6b scroll indicator defaults
            scroll_indicator_color: parse_color_hex(defaults::SCROLL_INDICATOR_COLOR)
                .expect("scroll indicator color default is valid hex"),
            scroll_indicator_width_px: parse_numeric(defaults::SCROLL_INDICATOR_WIDTH_PX)
                .expect("scroll indicator width default is valid numeric"),
            scroll_indicator_min_height_px: parse_numeric(defaults::SCROLL_INDICATOR_MIN_HEIGHT_PX)
                .expect("scroll indicator min height default is valid numeric"),

            composer_caret_color: parse_color_hex(defaults::COMPOSER_CARET_COLOR)
                .expect("composer caret color default is valid hex"),
            composer_selection_color: parse_color_hex(defaults::COMPOSER_SELECTION_COLOR)
                .expect("composer selection color default is valid hex"),
            composer_placeholder_color: parse_color_hex(defaults::COMPOSER_PLACEHOLDER_COLOR)
                .expect("composer placeholder color default is valid hex"),

            focus_ring_color: parse_color_hex(defaults::FOCUS_RING_COLOR)
                .expect("focus ring color default is valid hex"),
            focus_ring_width_px: parse_numeric(defaults::FOCUS_RING_WIDTH_PX)
                .expect("focus ring width default is valid numeric"),

            resize_grip_color: parse_color_hex(defaults::WINDOW_RESIZE_GRIP_COLOR)
                .expect("resize grip color default is valid hex"),
            resize_grip_hover_color: parse_color_hex(defaults::WINDOW_RESIZE_GRIP_HOVER_COLOR)
                .expect("resize grip hover color default is valid hex"),
            resize_grip_size_px: parse_numeric(defaults::WINDOW_RESIZE_GRIP_SIZE_PX)
                .expect("resize grip size default is valid numeric"),

            content_inset_px: parse_numeric(defaults::SPACING_CONTENT_INSET_PX)
                .expect("content inset default is valid numeric"),
            header_height_px: parse_numeric(defaults::SPACING_HEADER_HEIGHT_PX)
                .expect("header height default is valid numeric"),
            section_gap_px: parse_numeric(defaults::SPACING_SECTION_GAP_PX)
                .expect("section gap default is valid numeric"),
            transcript_max_measure_px: parse_numeric(defaults::TRANSCRIPT_MAX_MEASURE_PX)
                .expect("transcript max measure default is valid numeric"),
        }
    }
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Resolve `PortalPartTokens` from a three-layer resolved design token map.
///
/// Missing or unparseable portal tokens fall back to the hardcoded defaults
/// rather than failing. This matches the portal-scoped override semantics:
/// profile overrides can change any token; absent tokens get defaults.
///
/// # Arguments
///
/// * `token_map` — the fully resolved token map (from `resolve_tokens`);
///   portal-scoped overrides are already merged in at the highest priority.
pub fn resolve_portal_tokens(token_map: &DesignTokenMap) -> PortalPartTokens {
    let defaults = PortalPartTokens::default();

    macro_rules! resolve_color {
        ($key:expr, $fallback:expr) => {
            match token_map.get($key) {
                None => $fallback,
                Some(v) => match parse_color_hex(v) {
                    Some(c) => c,
                    None => {
                        warn!(
                            token_key = $key,
                            bad_value = %v,
                            "portal token color is unparseable; using default fallback",
                        );
                        $fallback
                    }
                },
            }
        };
    }

    macro_rules! resolve_f32 {
        ($key:expr, $fallback:expr) => {
            match token_map.get($key) {
                None => $fallback,
                Some(v) => match parse_numeric(v) {
                    Some(n) => n,
                    None => {
                        warn!(
                            token_key = $key,
                            bad_value = %v,
                            "portal token numeric is unparseable; using default fallback",
                        );
                        $fallback
                    }
                },
            }
        };
    }

    macro_rules! resolve_u32 {
        ($key:expr, $fallback:expr) => {
            match token_map.get($key) {
                None => $fallback,
                Some(v) => {
                    // Require a positive integer string: no negatives, no
                    // decimals, no very-large floats that would overflow u32.
                    // parse_numeric accepts any finite f32 — we add strictness.
                    let parsed = parse_numeric(v).and_then(|n| {
                        if n < 1.0 || n > u32::MAX as f32 || n.fract() != 0.0 {
                            None
                        } else {
                            Some(n as u32)
                        }
                    });
                    match parsed {
                        Some(n) => n,
                        None => {
                            warn!(
                                token_key = $key,
                                bad_value = %v,
                                "portal token u32 is unparseable or out-of-range; \
                                 using default fallback",
                            );
                            $fallback
                        }
                    }
                }
            }
        };
    }

    macro_rules! resolve_granularity {
        ($key:expr, $fallback:expr) => {
            match token_map.get($key) {
                None => $fallback,
                Some(v) => match TimestampGranularity::parse(v) {
                    Some(g) => g,
                    None => {
                        warn!(
                            token_key = $key,
                            bad_value = %v,
                            "portal timestamp granularity is unrecognized; using default fallback",
                        );
                        $fallback
                    }
                },
            }
        };
    }

    PortalPartTokens {
        frame_background: resolve_color!(PORTAL_TOKEN_FRAME_BACKGROUND, defaults.frame_background),
        frame_opacity: resolve_f32!(PORTAL_TOKEN_FRAME_OPACITY, defaults.frame_opacity),
        frame_border_color: resolve_color!(
            PORTAL_TOKEN_FRAME_BORDER_COLOR,
            defaults.frame_border_color
        ),

        header_text_color: resolve_color!(
            PORTAL_TOKEN_HEADER_TEXT_COLOR,
            defaults.header_text_color
        ),
        header_font_size_px: resolve_f32!(
            PORTAL_TOKEN_HEADER_FONT_SIZE,
            defaults.header_font_size_px
        ),

        composer_background: resolve_color!(
            PORTAL_TOKEN_COMPOSER_BACKGROUND,
            defaults.composer_background
        ),
        composer_text_color: resolve_color!(
            PORTAL_TOKEN_COMPOSER_TEXT_COLOR,
            defaults.composer_text_color
        ),
        composer_font_size_px: resolve_f32!(
            PORTAL_TOKEN_COMPOSER_FONT_SIZE,
            defaults.composer_font_size_px
        ),
        composer_at_capacity_color: resolve_color!(
            PORTAL_TOKEN_COMPOSER_AT_CAPACITY_COLOR,
            defaults.composer_at_capacity_color
        ),

        transcript_background: resolve_color!(
            PORTAL_TOKEN_TRANSCRIPT_BACKGROUND,
            defaults.transcript_background
        ),
        transcript_text_color: resolve_color!(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR,
            defaults.transcript_text_color
        ),
        transcript_system_color: resolve_color!(
            PORTAL_TOKEN_TRANSCRIPT_SYSTEM_COLOR,
            defaults.transcript_system_color
        ),
        transcript_font_size_px: resolve_f32!(
            PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE,
            defaults.transcript_font_size_px
        ),

        transcript_dim_text_color: resolve_color!(
            PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR,
            defaults.transcript_dim_text_color
        ),
        transcript_dim_background: resolve_color!(
            PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND,
            defaults.transcript_dim_background
        ),
        stale_marker_color: resolve_color!(
            PORTAL_TOKEN_STALE_MARKER_COLOR,
            defaults.stale_marker_color
        ),
        disconnect_badge_color: resolve_color!(
            PORTAL_TOKEN_DISCONNECT_BADGE_COLOR,
            defaults.disconnect_badge_color
        ),
        disconnect_badge_width_px: resolve_f32!(
            PORTAL_TOKEN_DISCONNECT_BADGE_WIDTH_PX,
            defaults.disconnect_badge_width_px
        ),
        unread_indicator_color: resolve_color!(
            PORTAL_TOKEN_UNREAD_INDICATOR_COLOR,
            defaults.unread_indicator_color
        ),
        awaiting_reply_color: resolve_color!(
            PORTAL_TOKEN_AWAITING_REPLY_COLOR,
            defaults.awaiting_reply_color
        ),
        empty_state_color: resolve_color!(
            PORTAL_TOKEN_EMPTY_STATE_COLOR,
            defaults.empty_state_color
        ),
        connecting_marker_color: resolve_color!(
            PORTAL_TOKEN_CONNECTING_MARKER_COLOR,
            defaults.connecting_marker_color
        ),

        activity_cue_color: resolve_color!(
            PORTAL_TOKEN_ACTIVITY_CUE_COLOR,
            defaults.activity_cue_color
        ),
        streaming_cursor_color: resolve_color!(
            PORTAL_TOKEN_STREAMING_CURSOR_COLOR,
            defaults.streaming_cursor_color
        ),

        delivery_inflight_color: resolve_color!(
            PORTAL_TOKEN_DELIVERY_INFLIGHT_COLOR,
            defaults.delivery_inflight_color
        ),
        delivery_delivered_color: resolve_color!(
            PORTAL_TOKEN_DELIVERY_DELIVERED_COLOR,
            defaults.delivery_delivered_color
        ),
        delivery_failed_color: resolve_color!(
            PORTAL_TOKEN_DELIVERY_FAILED_COLOR,
            defaults.delivery_failed_color
        ),

        timestamp_color: resolve_color!(PORTAL_TOKEN_TIMESTAMP_COLOR, defaults.timestamp_color),
        timestamp_granularity: resolve_granularity!(
            PORTAL_TOKEN_TIMESTAMP_GRANULARITY,
            defaults.timestamp_granularity
        ),

        lifecycle_active_color: resolve_color!(
            PORTAL_TOKEN_LIFECYCLE_ACTIVE_COLOR,
            defaults.lifecycle_active_color
        ),
        lifecycle_attached_color: resolve_color!(
            PORTAL_TOKEN_LIFECYCLE_ATTACHED_COLOR,
            defaults.lifecycle_attached_color
        ),
        lifecycle_attention_color: resolve_color!(
            PORTAL_TOKEN_LIFECYCLE_ATTENTION_COLOR,
            defaults.lifecycle_attention_color
        ),
        lifecycle_inactive_color: resolve_color!(
            PORTAL_TOKEN_LIFECYCLE_INACTIVE_COLOR,
            defaults.lifecycle_inactive_color
        ),
        lifecycle_accent_width_px: resolve_f32!(
            PORTAL_TOKEN_LIFECYCLE_ACCENT_WIDTH_PX,
            defaults.lifecycle_accent_width_px
        ),

        divider_color: resolve_color!(PORTAL_TOKEN_DIVIDER_COLOR, defaults.divider_color),
        unread_divider_color: resolve_color!(
            PORTAL_TOKEN_UNREAD_DIVIDER_COLOR,
            defaults.unread_divider_color
        ),

        collapsed_background: resolve_color!(
            PORTAL_TOKEN_COLLAPSED_BACKGROUND,
            defaults.collapsed_background
        ),
        collapsed_text_color: resolve_color!(
            PORTAL_TOKEN_COLLAPSED_TEXT_COLOR,
            defaults.collapsed_text_color
        ),
        collapsed_font_size_px: resolve_f32!(
            PORTAL_TOKEN_COLLAPSED_FONT_SIZE,
            defaults.collapsed_font_size_px
        ),

        transition_in_ms: resolve_u32!(PORTAL_TOKEN_TRANSITION_IN_MS, defaults.transition_in_ms),
        transition_out_ms: resolve_u32!(PORTAL_TOKEN_TRANSITION_OUT_MS, defaults.transition_out_ms),

        // §6b window management
        window_min_width_px: resolve_f32!(
            PORTAL_TOKEN_WINDOW_MIN_WIDTH_PX,
            defaults.window_min_width_px
        ),
        window_min_height_px: resolve_f32!(
            PORTAL_TOKEN_WINDOW_MIN_HEIGHT_PX,
            defaults.window_min_height_px
        ),
        window_resize_step_px: resolve_f32!(
            PORTAL_TOKEN_WINDOW_RESIZE_STEP_PX,
            defaults.window_resize_step_px
        ),
        window_resize_affordance_px: resolve_f32!(
            PORTAL_TOKEN_WINDOW_RESIZE_AFFORDANCE_PX,
            defaults.window_resize_affordance_px
        ),

        // §6b scroll indicators
        scroll_indicator_color: resolve_color!(
            PORTAL_TOKEN_SCROLL_INDICATOR_COLOR,
            defaults.scroll_indicator_color
        ),
        scroll_indicator_width_px: resolve_f32!(
            PORTAL_TOKEN_SCROLL_INDICATOR_WIDTH_PX,
            defaults.scroll_indicator_width_px
        ),
        scroll_indicator_min_height_px: resolve_f32!(
            PORTAL_TOKEN_SCROLL_INDICATOR_MIN_HEIGHT_PX,
            defaults.scroll_indicator_min_height_px
        ),

        // Composer caret / selection / placeholder
        composer_caret_color: resolve_color!(
            PORTAL_TOKEN_COMPOSER_CARET_COLOR,
            defaults.composer_caret_color
        ),
        composer_selection_color: resolve_color!(
            PORTAL_TOKEN_COMPOSER_SELECTION_COLOR,
            defaults.composer_selection_color
        ),
        composer_placeholder_color: resolve_color!(
            PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR,
            defaults.composer_placeholder_color
        ),

        // Focus ring
        focus_ring_color: resolve_color!(PORTAL_TOKEN_FOCUS_RING_COLOR, defaults.focus_ring_color),
        focus_ring_width_px: resolve_f32!(
            PORTAL_TOKEN_FOCUS_RING_WIDTH_PX,
            defaults.focus_ring_width_px
        ),

        // Resize grip
        resize_grip_color: resolve_color!(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR,
            defaults.resize_grip_color
        ),
        resize_grip_hover_color: resolve_color!(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR,
            defaults.resize_grip_hover_color
        ),
        resize_grip_size_px: resolve_f32!(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_SIZE_PX,
            defaults.resize_grip_size_px
        ),

        // Spatial rhythm + transcript measure
        content_inset_px: resolve_f32!(
            PORTAL_TOKEN_SPACING_CONTENT_INSET_PX,
            defaults.content_inset_px
        ),
        header_height_px: resolve_f32!(
            PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX,
            defaults.header_height_px
        ),
        section_gap_px: resolve_f32!(PORTAL_TOKEN_SPACING_SECTION_GAP_PX, defaults.section_gap_px),
        transcript_max_measure_px: resolve_f32!(
            PORTAL_TOKEN_TRANSCRIPT_MAX_MEASURE_PX,
            defaults.transcript_max_measure_px
        ),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{DesignTokenMap, resolve_tokens};

    fn empty_map() -> DesignTokenMap {
        DesignTokenMap::new()
    }

    // ── Default fallback resolution ───────────────────────────────────────

    #[test]
    fn resolve_portal_tokens_defaults_on_empty_map() {
        let tokens = resolve_portal_tokens(&empty_map());
        let defaults = PortalPartTokens::default();
        // Spot-check a selection of fields
        assert_eq!(tokens.frame_opacity, defaults.frame_opacity);
        assert_eq!(tokens.header_font_size_px, defaults.header_font_size_px);
        assert_eq!(tokens.transition_in_ms, defaults.transition_in_ms);
        assert_eq!(tokens.transition_out_ms, defaults.transition_out_ms);
    }

    #[test]
    fn resolve_portal_tokens_all_fields_populated() {
        let tokens = resolve_portal_tokens(&empty_map());
        // Every f32 field must be finite and positive
        assert!(tokens.frame_opacity > 0.0 && tokens.frame_opacity <= 1.0);
        assert!(tokens.header_font_size_px > 0.0);
        assert!(tokens.composer_font_size_px > 0.0);
        assert!(tokens.transcript_font_size_px > 0.0);
        assert!(tokens.collapsed_font_size_px > 0.0);
        assert!(tokens.transition_in_ms > 0);
        assert!(tokens.transition_out_ms > 0);
        // Composer at-capacity color must have a non-zero alpha (visible indicator)
        assert!(
            tokens.composer_at_capacity_color.a > 0.0,
            "at-capacity color must have non-zero alpha so it is visible"
        );
        // §6b window management fields
        assert!(tokens.window_min_width_px > 0.0);
        assert!(tokens.window_min_height_px > 0.0);
        assert!(tokens.window_resize_step_px > 0.0);
        assert!(tokens.window_resize_affordance_px > 0.0);
        // §6b scroll indicator fields
        assert!(tokens.scroll_indicator_width_px > 0.0);
        assert!(tokens.scroll_indicator_min_height_px > 0.0);
    }

    #[test]
    fn default_portal_text_sizes_are_readable_without_focus_resize() {
        let tokens = resolve_portal_tokens(&empty_map());

        assert!(
            tokens.header_font_size_px >= 16.0,
            "header default font must be comfortably readable without hotkey resize; got {}px",
            tokens.header_font_size_px
        );
        assert!(
            tokens.transcript_font_size_px >= 16.0,
            "transcript default font must be comfortably readable without hotkey resize; got {}px",
            tokens.transcript_font_size_px
        );
        assert!(
            tokens.composer_font_size_px >= 16.0,
            "composer default font must be comfortably readable without hotkey resize; got {}px",
            tokens.composer_font_size_px
        );
        assert!(
            tokens.collapsed_font_size_px >= 14.0,
            "collapsed-card default font must remain readable in compact mode; got {}px",
            tokens.collapsed_font_size_px
        );
    }

    // ── Profile-scoped override propagation ──────────────────────────────

    #[test]
    fn profile_override_propagates_to_portal_tokens() {
        // Verify that a profile-scoped override for portal.transcript.text_color
        // propagates through resolve_portal_tokens — this is the pre-promotion
        // §6.1 contract: token change → portal reskin, no adapter logic change.
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR.to_string(),
            "#FF00FF".to_string(), // magenta sentinel
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);

        assert!(
            (tokens.transcript_text_color.r - 1.0).abs() < 1e-3,
            "overridden r must be 1.0 (FF)"
        );
        assert!(
            tokens.transcript_text_color.g.abs() < 1e-3,
            "overridden g must be 0.0 (00)"
        );
        assert!(
            (tokens.transcript_text_color.b - 1.0).abs() < 1e-3,
            "overridden b must be 1.0 (FF)"
        );
    }

    #[test]
    fn profile_override_changes_frame_opacity() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(PORTAL_TOKEN_FRAME_OPACITY.to_string(), "0.5".to_string());
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!((tokens.frame_opacity - 0.5).abs() < 1e-4);
    }

    #[test]
    fn profile_override_changes_transition_ms() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(PORTAL_TOKEN_TRANSITION_IN_MS.to_string(), "250".to_string());
        overrides.insert(
            PORTAL_TOKEN_TRANSITION_OUT_MS.to_string(),
            "150".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert_eq!(tokens.transition_in_ms, 250);
        assert_eq!(tokens.transition_out_ms, 150);
    }

    // ── Profile-swap reskin (§6.4 core scenario) ─────────────────────────

    /// Profile swap reskins portal without adapter logic change.
    ///
    /// Demonstrates §6.1: a profile change propagates to all portal parts
    /// through `resolve_portal_tokens`, with zero adapter code changes.
    /// The "adapter logic change" is defined as changing the code path that
    /// calls `resolve_portal_tokens` — here we prove that only token values
    /// change across profiles, never the calling code.
    #[test]
    fn profile_swap_reskins_all_portal_parts() {
        // Profile A: dark theme (defaults)
        let profile_a_tokens = resolve_portal_tokens(&empty_map());

        // Profile B: custom theme (all portal parts overridden)
        let mut profile_b_overrides = DesignTokenMap::new();
        profile_b_overrides.insert(
            PORTAL_TOKEN_FRAME_BACKGROUND.to_string(),
            "#FFFFFF".to_string(), // white
        );
        profile_b_overrides.insert(PORTAL_TOKEN_FRAME_OPACITY.to_string(), "1.0".to_string());
        profile_b_overrides.insert(
            PORTAL_TOKEN_HEADER_TEXT_COLOR.to_string(),
            "#000000".to_string(), // black
        );
        profile_b_overrides.insert(PORTAL_TOKEN_HEADER_FONT_SIZE.to_string(), "18".to_string());
        profile_b_overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR.to_string(),
            "#333333".to_string(),
        );
        profile_b_overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_BACKGROUND.to_string(),
            "#F5F5F5".to_string(),
        );
        profile_b_overrides.insert(
            PORTAL_TOKEN_COLLAPSED_BACKGROUND.to_string(),
            "#EEEEEE".to_string(),
        );
        profile_b_overrides.insert(
            PORTAL_TOKEN_DIVIDER_COLOR.to_string(),
            "#CCCCCC".to_string(),
        );

        let resolved_b = resolve_tokens(&empty_map(), &profile_b_overrides);
        let profile_b_tokens = resolve_portal_tokens(&resolved_b);

        // Frame background must differ (white vs dark)
        assert_ne!(
            profile_a_tokens.frame_background, profile_b_tokens.frame_background,
            "profile swap must change frame background"
        );

        // Header text color must differ (black vs near-white)
        assert_ne!(
            profile_a_tokens.header_text_color, profile_b_tokens.header_text_color,
            "profile swap must change header text color"
        );

        // Header font size must differ
        assert!(
            (profile_b_tokens.header_font_size_px - 18.0).abs() < 1e-4,
            "profile B header font size must be 18px"
        );
        assert!(
            (profile_a_tokens.header_font_size_px - 18.0).abs() > 1e-1,
            "profile A header font size must differ from 18px"
        );

        // Transcript background must differ
        assert_ne!(
            profile_a_tokens.transcript_background, profile_b_tokens.transcript_background,
            "profile swap must change transcript background"
        );

        // Collapsed background must differ
        assert_ne!(
            profile_a_tokens.collapsed_background, profile_b_tokens.collapsed_background,
            "profile swap must change collapsed background"
        );

        // Divider color must differ
        assert_ne!(
            profile_a_tokens.divider_color, profile_b_tokens.divider_color,
            "profile swap must change divider color"
        );
    }

    // ── Token propagation on republish (§6.4) ────────────────────────────

    /// Verifies that a token value change propagates through the portal token
    /// map on every republish without requiring any adapter code change.
    /// "Republish" here is represented by resolving the token map a second time.
    #[test]
    fn token_change_propagates_on_republish() {
        // First publish cycle: default tokens
        let first = resolve_portal_tokens(&empty_map());

        // Token change (simulate profile hot-reload changing transcript background)
        let mut new_overrides = DesignTokenMap::new();
        new_overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_BACKGROUND.to_string(),
            "#2A4080".to_string(), // navy blue
        );
        let new_map = resolve_tokens(&empty_map(), &new_overrides);

        // Second publish cycle: updated tokens
        let second = resolve_portal_tokens(&new_map);

        // The token change must propagate
        assert_ne!(
            first.transcript_background, second.transcript_background,
            "token change must propagate to republish"
        );

        // All other fields must be unchanged (only transcript background changed)
        assert_eq!(
            first.frame_background, second.frame_background,
            "unmodified tokens must stay the same after partial update"
        );
        assert_eq!(
            first.header_text_color, second.header_text_color,
            "unmodified tokens must stay the same after partial update"
        );
    }

    // ── Unparseable token fallback ────────────────────────────────────────

    #[test]
    fn unparseable_token_falls_back_to_default() {
        let mut bad_overrides = DesignTokenMap::new();
        // Inject an invalid color for a portal token key
        bad_overrides.insert(
            PORTAL_TOKEN_FRAME_BACKGROUND.to_string(),
            "not-a-hex-color".to_string(),
        );
        bad_overrides.insert(
            PORTAL_TOKEN_FRAME_OPACITY.to_string(),
            "not-a-number".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &bad_overrides);
        let tokens = resolve_portal_tokens(&resolved);
        let defaults = PortalPartTokens::default();

        // Must fall back to defaults, not panic
        assert_eq!(
            tokens.frame_background, defaults.frame_background,
            "unparseable color must fall back to default"
        );
        assert_eq!(
            tokens.frame_opacity, defaults.frame_opacity,
            "unparseable numeric must fall back to default"
        );
    }

    // ── resolve_u32 validation ────────────────────────────────────────────

    /// Verifies that resolve_u32 rejects invalid transition duration values and
    /// falls back to defaults. Invalid values include negatives (which would cast
    /// to 0 via `as u32`), decimals, and excessively large floats.
    #[test]
    fn invalid_transition_ms_falls_back_to_default() {
        let defaults = PortalPartTokens::default();

        // Negative value → fallback (0 would violate the > 0 invariant)
        let mut bad = DesignTokenMap::new();
        bad.insert(PORTAL_TOKEN_TRANSITION_IN_MS.to_string(), "-1".to_string());
        let resolved = resolve_tokens(&empty_map(), &bad);
        let tokens = resolve_portal_tokens(&resolved);
        assert_eq!(
            tokens.transition_in_ms, defaults.transition_in_ms,
            "negative transition_in_ms must fall back to default"
        );

        // Decimal value → fallback
        let mut bad2 = DesignTokenMap::new();
        bad2.insert(
            PORTAL_TOKEN_TRANSITION_OUT_MS.to_string(),
            "0.5".to_string(),
        );
        let resolved2 = resolve_tokens(&empty_map(), &bad2);
        let tokens2 = resolve_portal_tokens(&resolved2);
        assert_eq!(
            tokens2.transition_out_ms, defaults.transition_out_ms,
            "decimal transition_out_ms must fall back to default"
        );

        // Zero value → fallback (> 0 invariant)
        let mut bad3 = DesignTokenMap::new();
        bad3.insert(PORTAL_TOKEN_TRANSITION_IN_MS.to_string(), "0".to_string());
        let resolved3 = resolve_tokens(&empty_map(), &bad3);
        let tokens3 = resolve_portal_tokens(&resolved3);
        assert_eq!(
            tokens3.transition_in_ms, defaults.transition_in_ms,
            "zero transition_in_ms must fall back to default"
        );
    }

    // ── §6b window management token tests ────────────────────────────────

    #[test]
    fn window_management_tokens_default_values_are_valid() {
        let tokens = resolve_portal_tokens(&empty_map());
        // Defaults must be positive
        assert!(
            tokens.window_min_width_px > 0.0,
            "window_min_width_px must be positive"
        );
        assert!(
            tokens.window_min_height_px > 0.0,
            "window_min_height_px must be positive"
        );
        assert!(
            tokens.window_resize_step_px > 0.0,
            "window_resize_step_px must be positive"
        );
        assert!(
            tokens.window_resize_affordance_px > 0.0,
            "window_resize_affordance_px must be positive"
        );
        // Defaults must satisfy basic legibility invariant: min size > affordance
        assert!(
            tokens.window_min_width_px > tokens.window_resize_affordance_px,
            "min width must be larger than the affordance region"
        );
        assert!(
            tokens.window_min_height_px > tokens.window_resize_affordance_px,
            "min height must be larger than the affordance region"
        );
    }

    #[test]
    fn window_management_tokens_override_propagates() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_WINDOW_MIN_WIDTH_PX.to_string(),
            "320".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_WINDOW_RESIZE_STEP_PX.to_string(),
            "16".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_WINDOW_RESIZE_AFFORDANCE_PX.to_string(),
            "12".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);

        assert!(
            (tokens.window_min_width_px - 320.0).abs() < 1e-4,
            "window_min_width_px override must propagate"
        );
        assert!(
            (tokens.window_resize_step_px - 16.0).abs() < 1e-4,
            "window_resize_step_px override must propagate"
        );
        assert!(
            (tokens.window_resize_affordance_px - 12.0).abs() < 1e-4,
            "window_resize_affordance_px override must propagate"
        );
    }

    // ── §6b scroll indicator token tests ─────────────────────────────────

    #[test]
    fn scroll_indicator_tokens_default_values_are_valid() {
        let tokens = resolve_portal_tokens(&empty_map());
        assert!(
            tokens.scroll_indicator_width_px > 0.0,
            "scroll_indicator_width_px must be positive"
        );
        assert!(
            tokens.scroll_indicator_min_height_px > 0.0,
            "scroll_indicator_min_height_px must be positive"
        );
    }

    #[test]
    fn scroll_indicator_color_override_propagates() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_SCROLL_INDICATOR_COLOR.to_string(),
            "#FF8800".to_string(), // orange sentinel
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);

        // r=1.0, g=0x88/0xFF≈0.533, b=0.0
        assert!(
            (tokens.scroll_indicator_color.r - 1.0).abs() < 1e-2,
            "scroll indicator color red channel must match override"
        );
        assert!(
            tokens.scroll_indicator_color.b.abs() < 1e-2,
            "scroll indicator color blue channel must be 0"
        );
    }

    /// The lifecycle accent-bar width resolves to a positive default and a
    /// profile-scoped override propagates — the bar geometry is token-driven so
    /// neither the adapter nor the compositor hardcodes the accent dimension
    /// (hud-m48i0).
    #[test]
    fn lifecycle_accent_width_default_and_override() {
        let defaults = resolve_portal_tokens(&empty_map());
        assert!(
            defaults.lifecycle_accent_width_px > 0.0,
            "lifecycle accent bar must have a positive default width so it is visible"
        );

        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_LIFECYCLE_ACCENT_WIDTH_PX.to_string(),
            "9".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!(
            (tokens.lifecycle_accent_width_px - 9.0).abs() < 1e-4,
            "lifecycle accent width override must propagate"
        );
    }

    // ── §2/§3 degraded / disconnect token tests ─────────────────────────

    /// Degraded-treatment tokens resolve to valid, visible colors and read as
    /// distinctly dimmer than the live transcript palette (spec §2: the retained
    /// window is dimmed rather than blanked).
    #[test]
    fn degraded_tokens_default_values_are_valid() {
        let tokens = resolve_portal_tokens(&empty_map());
        // Stale marker must be visible (non-zero alpha) so the disconnect
        // affordance is actually shown.
        assert!(
            tokens.stale_marker_color.a > 0.0,
            "stale marker color must have non-zero alpha so it is visible"
        );
        // Dim treatment must differ from the live transcript palette — otherwise
        // a disconnected portal would be indistinguishable from a live one.
        assert_ne!(
            tokens.transcript_dim_text_color, tokens.transcript_text_color,
            "dim text color must differ from live transcript text color"
        );
        assert_ne!(
            tokens.transcript_dim_background, tokens.transcript_background,
            "dim background must differ from live transcript background"
        );
    }

    /// Profile-scoped overrides for the degraded tokens propagate through
    /// `resolve_portal_tokens` (spec §2: degraded treatment is token-resolved,
    /// not hardcoded — a profile change reskins it with no adapter change).
    #[test]
    fn degraded_token_overrides_propagate() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR.to_string(),
            "#FF00FF".to_string(), // magenta sentinel
        );
        overrides.insert(
            PORTAL_TOKEN_STALE_MARKER_COLOR.to_string(),
            "#00FF00".to_string(), // green sentinel
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);

        assert!(
            (tokens.transcript_dim_text_color.r - 1.0).abs() < 1e-3
                && tokens.transcript_dim_text_color.g.abs() < 1e-3
                && (tokens.transcript_dim_text_color.b - 1.0).abs() < 1e-3,
            "dim text color override must propagate (magenta)"
        );
        assert!(
            tokens.stale_marker_color.r.abs() < 1e-3
                && (tokens.stale_marker_color.g - 1.0).abs() < 1e-3
                && tokens.stale_marker_color.b.abs() < 1e-3,
            "stale marker color override must propagate (green)"
        );
    }

    // ── §Connecting State Distinction token tests (hud-g1ena.7) ──────────

    /// The connecting-marker token resolves to a valid, visible color and is
    /// distinct from the amber degraded/stale marker — the core spec requirement
    /// that a starting-up portal not read as a failing one.
    #[test]
    fn connecting_marker_token_is_valid_and_distinct_from_degraded() {
        let tokens = resolve_portal_tokens(&empty_map());
        assert!(
            tokens.connecting_marker_color.a > 0.0,
            "connecting marker color must have non-zero alpha so it is visible"
        );
        assert_ne!(
            tokens.connecting_marker_color, tokens.stale_marker_color,
            "connecting must be visually distinct from the degraded/stale marker \
             (§Connecting State Distinction)"
        );
    }

    /// A profile-scoped override of the connecting token propagates through
    /// `resolve_portal_tokens`, so the connecting treatment is reskinnable with no
    /// adapter change (token-resolved, never hardcoded).
    #[test]
    fn connecting_marker_token_override_propagates() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_CONNECTING_MARKER_COLOR.to_string(),
            "#0000FF".to_string(), // blue sentinel
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!(
            tokens.connecting_marker_color.r.abs() < 1e-3
                && tokens.connecting_marker_color.g.abs() < 1e-3
                && (tokens.connecting_marker_color.b - 1.0).abs() < 1e-3,
            "connecting marker color override must propagate (blue)"
        );
    }

    // ── Diagnostic warn path (hud-dcynv) ─────────────────────────────────

    /// Verifies that a present-but-unparseable token (color) falls back to the
    /// default value and does NOT panic. The `tracing::warn!` is emitted on the
    /// same code path, but subscriber capture requires the `tracing_test` crate
    /// which is not in this workspace. The behavioral invariant (fallback used)
    /// is sufficient to assert the warn code path was reached.
    #[test]
    fn unparseable_color_token_triggers_fallback_and_warn_path() {
        // ALL color-bearing token keys injected with bad values.
        let mut bad = DesignTokenMap::new();
        for key in [
            PORTAL_TOKEN_FRAME_BACKGROUND,
            PORTAL_TOKEN_FRAME_BORDER_COLOR,
            PORTAL_TOKEN_HEADER_TEXT_COLOR,
            PORTAL_TOKEN_COMPOSER_BACKGROUND,
            PORTAL_TOKEN_COMPOSER_TEXT_COLOR,
            PORTAL_TOKEN_COMPOSER_AT_CAPACITY_COLOR,
            PORTAL_TOKEN_TRANSCRIPT_BACKGROUND,
            PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR,
            PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR,
            PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND,
            PORTAL_TOKEN_STALE_MARKER_COLOR,
            PORTAL_TOKEN_DIVIDER_COLOR,
            PORTAL_TOKEN_COLLAPSED_BACKGROUND,
            PORTAL_TOKEN_COLLAPSED_TEXT_COLOR,
            PORTAL_TOKEN_SCROLL_INDICATOR_COLOR,
        ] {
            bad.insert(key.to_string(), "!!not-hex!!".to_string());
        }
        let resolved = resolve_tokens(&empty_map(), &bad);
        let tokens = resolve_portal_tokens(&resolved);
        let defaults = PortalPartTokens::default();

        // Every color field must fall back to the canonical default.
        assert_eq!(
            tokens.frame_background, defaults.frame_background,
            "bad frame_background must fall back to default"
        );
        assert_eq!(
            tokens.frame_border_color, defaults.frame_border_color,
            "bad frame_border_color must fall back to default"
        );
        assert_eq!(
            tokens.header_text_color, defaults.header_text_color,
            "bad header_text_color must fall back to default"
        );
        assert_eq!(
            tokens.composer_background, defaults.composer_background,
            "bad composer_background must fall back to default"
        );
        assert_eq!(
            tokens.transcript_background, defaults.transcript_background,
            "bad transcript_background must fall back to default"
        );
        assert_eq!(
            tokens.collapsed_background, defaults.collapsed_background,
            "bad collapsed_background must fall back to default"
        );
        assert_eq!(
            tokens.scroll_indicator_color, defaults.scroll_indicator_color,
            "bad scroll_indicator_color must fall back to default"
        );
    }

    /// Verifies that a present-but-unparseable numeric token falls back to the
    /// default value. The warn is emitted on the same code path.
    #[test]
    fn unparseable_numeric_token_triggers_fallback_and_warn_path() {
        let mut bad = DesignTokenMap::new();
        for key in [
            PORTAL_TOKEN_FRAME_OPACITY,
            PORTAL_TOKEN_HEADER_FONT_SIZE,
            PORTAL_TOKEN_COMPOSER_FONT_SIZE,
            PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE,
            PORTAL_TOKEN_COLLAPSED_FONT_SIZE,
            PORTAL_TOKEN_WINDOW_MIN_WIDTH_PX,
            PORTAL_TOKEN_WINDOW_MIN_HEIGHT_PX,
            PORTAL_TOKEN_WINDOW_RESIZE_STEP_PX,
            PORTAL_TOKEN_WINDOW_RESIZE_AFFORDANCE_PX,
            PORTAL_TOKEN_SCROLL_INDICATOR_WIDTH_PX,
            PORTAL_TOKEN_SCROLL_INDICATOR_MIN_HEIGHT_PX,
        ] {
            bad.insert(key.to_string(), "definitely-not-a-number".to_string());
        }
        let resolved = resolve_tokens(&empty_map(), &bad);
        let tokens = resolve_portal_tokens(&resolved);
        let defaults = PortalPartTokens::default();

        assert!(
            (tokens.frame_opacity - defaults.frame_opacity).abs() < 1e-6,
            "bad frame_opacity must fall back to default"
        );
        assert!(
            (tokens.header_font_size_px - defaults.header_font_size_px).abs() < 1e-6,
            "bad header_font_size_px must fall back to default"
        );
        assert!(
            (tokens.window_min_width_px - defaults.window_min_width_px).abs() < 1e-6,
            "bad window_min_width_px must fall back to default"
        );
        assert!(
            (tokens.scroll_indicator_width_px - defaults.scroll_indicator_width_px).abs() < 1e-6,
            "bad scroll_indicator_width_px must fall back to default"
        );
    }

    /// Verifies that a present-but-invalid u32 token (negative / decimal / zero)
    /// falls back to the default value. The warn is emitted on the same code path.
    #[test]
    fn invalid_u32_token_triggers_fallback_and_warn_path() {
        let defaults = PortalPartTokens::default();

        for bad_value in ["-5", "0", "0.5", "1.9", "not-a-number"] {
            let mut bad = DesignTokenMap::new();
            bad.insert(
                PORTAL_TOKEN_TRANSITION_IN_MS.to_string(),
                bad_value.to_string(),
            );
            bad.insert(
                PORTAL_TOKEN_TRANSITION_OUT_MS.to_string(),
                bad_value.to_string(),
            );
            let resolved = resolve_tokens(&empty_map(), &bad);
            let tokens = resolve_portal_tokens(&resolved);

            assert_eq!(
                tokens.transition_in_ms, defaults.transition_in_ms,
                "bad transition_in_ms ({bad_value:?}) must fall back to default"
            );
            assert_eq!(
                tokens.transition_out_ms, defaults.transition_out_ms,
                "bad transition_out_ms ({bad_value:?}) must fall back to default"
            );
        }
    }

    // ── Compliance amendment tokens (hud-khfgx) ──────────────────────────────
    //
    // Caret/selection/placeholder + focus-ring + resize-grip + spacing/measure.
    // Each group asserts (a) sane defaults and (b) profile-override propagation
    // through `resolve_portal_tokens` — the pre-promotion §6.1 contract.

    #[test]
    fn caret_selection_placeholder_defaults_are_valid() {
        let tokens = resolve_portal_tokens(&empty_map());
        let defaults = PortalPartTokens::default();

        // Caret defaults to the composer text color (no-visual-regression default).
        assert_eq!(
            tokens.composer_caret_color, defaults.composer_text_color,
            "default caret color must equal the composer text color"
        );
        // Selection highlight is visible (non-zero alpha) and translucent (< 1.0)
        // so the selected glyphs read through it.
        assert!(
            tokens.composer_selection_color.a > 0.0 && tokens.composer_selection_color.a < 1.0,
            "selection highlight must be a visible translucent tint"
        );
        // Placeholder is dimmer than the live composer text so it reads as a hint,
        // not typed content.
        assert_ne!(
            tokens.composer_placeholder_color, tokens.composer_text_color,
            "placeholder color must differ from the live composer text color"
        );
    }

    #[test]
    fn caret_selection_placeholder_overrides_propagate() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_COMPOSER_CARET_COLOR.to_string(),
            "#FF00FF".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_COMPOSER_SELECTION_COLOR.to_string(),
            "#00FF0080".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR.to_string(),
            "#123456".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);

        assert!(
            (tokens.composer_caret_color.r - 1.0).abs() < 1e-3
                && tokens.composer_caret_color.g.abs() < 1e-3
                && (tokens.composer_caret_color.b - 1.0).abs() < 1e-3,
            "caret color override must propagate (magenta)"
        );
        assert!(
            (tokens.composer_selection_color.a - 0x80 as f32 / 255.0).abs() < 1e-2,
            "selection color alpha override must propagate"
        );
        assert!(
            tokens.composer_placeholder_color.r > 0.0,
            "placeholder color override must propagate"
        );
    }

    #[test]
    fn focus_ring_defaults_and_override_propagate() {
        let defaults = resolve_portal_tokens(&empty_map());
        // Default ring is opaque and 2px, mirroring the tze_hud_input focus-ring.
        assert!(
            (defaults.focus_ring_width_px - 2.0).abs() < 1e-4,
            "default focus-ring width must be 2px"
        );
        assert!(
            defaults.focus_ring_color.a > 0.0,
            "default focus-ring color must be visible"
        );

        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_FOCUS_RING_COLOR.to_string(),
            "#FF0000".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_FOCUS_RING_WIDTH_PX.to_string(),
            "3.5".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!(
            (tokens.focus_ring_color.r - 1.0).abs() < 1e-3
                && tokens.focus_ring_color.g.abs() < 1e-3,
            "focus-ring color override must propagate (red)"
        );
        assert!(
            (tokens.focus_ring_width_px - 3.5).abs() < 1e-4,
            "focus-ring width override must propagate"
        );
    }

    #[test]
    fn resize_grip_defaults_and_override_propagate() {
        let defaults = resolve_portal_tokens(&empty_map());
        assert!(
            defaults.resize_grip_size_px > 0.0,
            "resize-grip size must have a positive default"
        );
        // Hover tint differs from the resting grip color so hover is perceptible.
        assert_ne!(
            defaults.resize_grip_color, defaults.resize_grip_hover_color,
            "resize-grip hover tint must differ from the resting grip color"
        );

        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR.to_string(),
            "#101010".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR.to_string(),
            "#F0F0F0".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_WINDOW_RESIZE_GRIP_SIZE_PX.to_string(),
            "20".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!(
            (tokens.resize_grip_size_px - 20.0).abs() < 1e-4,
            "resize-grip size override must propagate"
        );
        assert!(
            tokens.resize_grip_hover_color.r > tokens.resize_grip_color.r,
            "resize-grip hover override must be brighter than the resting override"
        );
    }

    #[test]
    fn spacing_and_measure_defaults_and_override_propagate() {
        let defaults = resolve_portal_tokens(&empty_map());
        assert!(
            defaults.content_inset_px > 0.0,
            "content inset must have a positive default"
        );
        assert!(
            defaults.header_height_px > 0.0,
            "header height must have a positive default"
        );
        assert!(
            defaults.section_gap_px > 0.0,
            "section gap must have a positive default"
        );
        // 0 = unbounded: today's full-width transcript wrapping is preserved until
        // a profile opts into a narrower measure.
        assert_eq!(
            defaults.transcript_max_measure_px, 0.0,
            "transcript measure cap must default to 0 (unbounded)"
        );

        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_SPACING_CONTENT_INSET_PX.to_string(),
            "12".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_SPACING_HEADER_HEIGHT_PX.to_string(),
            "40".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_SPACING_SECTION_GAP_PX.to_string(),
            "16".to_string(),
        );
        overrides.insert(
            PORTAL_TOKEN_TRANSCRIPT_MAX_MEASURE_PX.to_string(),
            "640".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert!((tokens.content_inset_px - 12.0).abs() < 1e-4);
        assert!((tokens.header_height_px - 40.0).abs() < 1e-4);
        assert!((tokens.section_gap_px - 16.0).abs() < 1e-4);
        assert!(
            (tokens.transcript_max_measure_px - 640.0).abs() < 1e-4,
            "transcript measure cap override must propagate"
        );
    }

    /// hud-a328c regression: a config whose `[design_tokens]` sets SOME portal
    /// keys but NOT `portal.frame.background` must resolve the frame background
    /// to the reviewed opaque off-black default `#0A0D11`, not the old grey
    /// slate `#111720` that painted the frame rim grey.
    #[test]
    fn partial_config_resolves_unset_frame_background_to_portal_default_not_grey() {
        assert_eq!(defaults::FRAME_BACKGROUND, "#0A0D11");
        let mut cfg = DesignTokenMap::new();
        cfg.insert(PORTAL_TOKEN_FRAME_OPACITY.to_string(), "0.98".to_string());
        cfg.insert(
            PORTAL_TOKEN_FOCUS_RING_COLOR.to_string(),
            "#00000000".to_string(),
        );
        cfg.insert("portal.composer.anchor".to_string(), "top".to_string());
        let tokens = resolve_portal_tokens(&cfg);
        assert_eq!(
            tokens.frame_background,
            PortalPartTokens::default().frame_background,
            "unset portal.frame.background must resolve to the portal default"
        );
        assert_ne!(
            tokens.frame_background,
            parse_color_hex("#111720").expect("valid hex"),
            "the resolved frame background must NOT be the grey slate that caused hud-a328c"
        );
    }

    // ── Ambient per-turn timestamp tokens (hud-g1ena.4) ───────────────────

    #[test]
    fn timestamp_granularity_parses_all_variants_case_insensitively() {
        assert_eq!(
            TimestampGranularity::parse("off"),
            Some(TimestampGranularity::Off)
        );
        assert_eq!(
            TimestampGranularity::parse("PER_TURN"),
            Some(TimestampGranularity::PerTurn)
        );
        // Hyphen and underscore spellings both accepted.
        assert_eq!(
            TimestampGranularity::parse("per-turn"),
            Some(TimestampGranularity::PerTurn)
        );
        assert_eq!(
            TimestampGranularity::parse("  Grouped "),
            Some(TimestampGranularity::Grouped)
        );
        // Unknown values resolve to None so the resolver falls back to default.
        assert_eq!(TimestampGranularity::parse("hourly"), None);
    }

    #[test]
    fn timestamp_granularity_token_string_round_trips() {
        for g in [
            TimestampGranularity::Off,
            TimestampGranularity::PerTurn,
            TimestampGranularity::Grouped,
        ] {
            assert_eq!(TimestampGranularity::parse(g.as_token_str()), Some(g));
        }
    }

    #[test]
    fn timestamp_tokens_default_off_and_secondary_slate() {
        let tokens = resolve_portal_tokens(&empty_map());
        // Ambient default: timestamps are OFF (visibility is profile-governed).
        assert_eq!(tokens.timestamp_granularity, TimestampGranularity::Off);
        // Secondary/dim presentation shares the muted-slate quiet-signal tone.
        assert_eq!(
            tokens.timestamp_color,
            parse_color_hex(defaults::TRANSCRIPT_DIM_TEXT_COLOR).unwrap(),
            "timestamp default color must be the shared secondary/dim slate"
        );
    }

    #[test]
    fn timestamp_granularity_override_resolves_from_profile() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_TIMESTAMP_GRANULARITY.to_string(),
            "grouped".to_string(),
        );
        let resolved = resolve_tokens(&empty_map(), &overrides);
        let tokens = resolve_portal_tokens(&resolved);
        assert_eq!(tokens.timestamp_granularity, TimestampGranularity::Grouped);
    }

    #[test]
    fn timestamp_granularity_unrecognized_override_falls_back_to_default() {
        let mut overrides = DesignTokenMap::new();
        overrides.insert(
            PORTAL_TOKEN_TIMESTAMP_GRANULARITY.to_string(),
            "every-second".to_string(),
        );
        let tokens = resolve_portal_tokens(&overrides);
        assert_eq!(tokens.timestamp_granularity, TimestampGranularity::Off);
    }
}
