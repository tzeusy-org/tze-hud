//! Default values for portal tokens (used when a token is absent from the resolved map).
//!
//! These defaults are deliberately distinct from the 30 canonical tokens so the
//! profile-swap test can distinguish between the canonical and portal layers.
//! Colors use the same palette as the existing exemplar adapter literals,
//! expressed as resolved token defaults rather than inline constants.
//!
//! These string constants are the **single source of truth** for the default portal
//! palette. `tze_hud_portal::tokens::PortalVisualTokens::default()` derives
//! from `PortalPartTokens::default()` (which parses these constants) via
//! `portal_visual_tokens_from_part_tokens`, so changing a value here propagates to
//! both sides automatically.
//!
//! They live in the scene crate so every crate that draws or hit-tests a
//! portal (config, input, compositor) reads this one definition.

/// Portal frame backdrop fill (RGBA hex). Opaque near-black that matches the
/// transcript pane (`TRANSCRIPT_BACKGROUND`) so the thin frame gap around the
/// panes reads as off-black — backdrop-independent (identical regardless of
/// the desktop wallpaper behind the HUD).
///
/// this was `#111720` — an OPAQUE slate that resolves to
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

// Degraded / disconnect treatment. Dim text/background read as
// "inactive" relative to the live transcript palette above; the stale
// marker uses a muted amber (matching the at-capacity convention) so it is
// ambient, not alarming.
pub const TRANSCRIPT_DIM_TEXT_COLOR: &str = "#6B7689";
pub const TRANSCRIPT_DIM_BACKGROUND: &str = "#07090C";
pub const STALE_MARKER_COLOR: &str = "#B87333";
/// Same hue as `STALE_MARKER_COLOR` — the geometry badge and the text
/// marker read as one ambient "stale" signal.
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
/// state. Its cool cyan is deliberately
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
/// rule a failed delivery stays ambient on the turn and never escalates the
/// portal's interruption class, so the tone reads as "did not arrive" quietly.
pub const DELIVERY_FAILED_COLOR: &str = "#B5827A";

/// Muted slate — the ambient per-turn arrival timestamp. Shares the quiet
/// secondary tone of `TRANSCRIPT_DIM_TEXT_COLOR` / `UNREAD_INDICATOR_COLOR` /
/// `COMPOSER_PLACEHOLDER_COLOR` so a stamp reads as subordinate metadata, not
/// as primary turn content or an alert.
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
// as the grey it used to (`#1A1F28`). The expanded frame's `#0A0D11`
// treatment did not cover the minimized state, which kept a
// separate grey token, so window-mgmt minimize still flashed grey.
pub const COLLAPSED_BACKGROUND: &str = "#12161C";
pub const COLLAPSED_TEXT_COLOR: &str = "#C8D6E8";
pub const COLLAPSED_FONT_SIZE: &str = "14";

pub const TRANSITION_IN_MS: &str = "120";
pub const TRANSITION_OUT_MS: &str = "80";

// window management defaults
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

// scroll-indicator defaults
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
/// equal `PORTAL_HEADER_DRAG_BAND_PX_DEFAULT` = 52 to keep the band unchanged).
/// `portal.header.drag_band_px` remains the raw-tile-fallback knob.
pub const SPACING_HEADER_HEIGHT_PX: &str = "52";
/// Inter-section vertical gap.
pub const SPACING_SECTION_GAP_PX: &str = "8";
/// Transcript measure cap — `0` = unbounded (wrap to full width, today's behavior).
pub const TRANSCRIPT_MAX_MEASURE_PX: &str = "0";

/// A numeric default as a number. Every numeric default is a valid number
/// (checked by this module's tests), so this never panics on one of them.
pub fn number(value: &str) -> f32 {
    value
        .parse()
        .unwrap_or_else(|_| panic!("portal default {value:?} is not a number"))
}

/// A `#RRGGBB[AA]` default as straight (sRGB, not linearized) RGBA fractions.
pub fn srgba(value: &str) -> [f32; 4] {
    let hex = value.strip_prefix('#').filter(|h| matches!(h.len(), 6 | 8));
    let channel = |i: usize| {
        hex.and_then(|h| h.get(i..i + 2))
            .and_then(|c| u8::from_str_radix(c, 16).ok())
            .map(|c| f32::from(c) / 255.0)
    };
    match (channel(0), channel(2), channel(4)) {
        (Some(r), Some(g), Some(b)) => [r, g, b, channel(6).unwrap_or(1.0)],
        _ => panic!("portal default {value:?} is not a hex color"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_crates_defaults_parse() {
        for value in [
            WINDOW_MIN_WIDTH_PX,
            WINDOW_MIN_HEIGHT_PX,
            WINDOW_RESIZE_STEP_PX,
            WINDOW_RESIZE_AFFORDANCE_PX,
            SCROLL_INDICATOR_WIDTH_PX,
            SCROLL_INDICATOR_MIN_HEIGHT_PX,
            WINDOW_RESIZE_GRIP_SIZE_PX,
            SPACING_SECTION_GAP_PX,
            SPACING_CONTENT_INSET_PX,
            TRANSITION_IN_MS,
            TRANSITION_OUT_MS,
        ] {
            assert!(number(value).is_finite(), "{value}");
        }
        assert_eq!(
            srgba("#4A5568"),
            [74.0 / 255.0, 85.0 / 255.0, 104.0 / 255.0, 1.0]
        );
        assert_eq!(srgba("#000000B3")[3], 179.0 / 255.0);
    }
}
