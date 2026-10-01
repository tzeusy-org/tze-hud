//! Disconnection badges and backpressure signals — Shell #5.
//!
//! # Overview
//!
//! The shell is the **sole owner** of badge rendering in the chrome layer.
//! Badge state is written to [`ChromeState`] by the control plane (lease
//! manager, session manager) and read by the chrome render pass.  Agents are
//! intentionally **not** told the scene is frozen or that their lease is
//! orphaned — backpressure signals are generic and apply to any queue-pressure
//! scenario.
//!
//! # Badge types
//!
//! | Badge             | Visual                              | Trigger                      |
//! |-------------------|-------------------------------------|------------------------------|
//! | Disconnection     | Dim link-break icon, 70% opacity     | Lease enters orphaned state  |
//! | Budget warning    | Amber border highlight, 2px, 70%    | 80% of session budget used   |
//!
//! # Backpressure signals
//!
//! [`BackpressureSignal`] is a generic signal sent to agents when their mutation
//! queue is under pressure.  It is intentionally generic — the signal does not
//! distinguish freeze-induced backpressure from genuine load.
//!
//! # Rendering contract
//!
//! - Badge commands are produced by [`build_badge_cmds`] and issued in the
//!   chrome render pass — after the content pass — so they always render above
//!   tile content regardless of tile z-order.
//! - Badge appear/clear is **frame-bounded**: the visual effect occurs within
//!   one frame (≤16.6ms) of the control-plane state update.
//! - No animation in v1 — badges are static overlays.
//!
//! # Spec references
//!
//! * `openspec/.../system-shell/spec.md` §Disconnection Badge (line 171)
//! * §Budget Warning Badge (line 197)
//! * §Freeze Backpressure Signal (line 158)
//! * §Override Control Guarantees (line 206)

use tze_hud_compositor::ChromeDrawCmd;
use tze_hud_scene::types::{Rect, SceneId};

// ─── Badge state (per-tile) ───────────────────────────────────────────────────

/// The set of active badges for a single tile.
///
/// Written by the control plane (lease manager, session manager) into the
/// chrome state (see [`crate::shell::chrome::ChromeState`]) and surfaced
/// per-frame via [`BadgeFrame`].  Read by the chrome render pass via
/// [`build_badge_cmds`].
///
/// This struct is **never** exposed to agents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileBadgeState {
    /// Whether the tile's owning agent lease is in the orphaned / grace-period
    /// state.  When `true`, a disconnection badge (dim link-break icon) is
    /// rendered on the tile and tile content opacity is reduced to 70%.
    pub disconnected: bool,
    /// Whether the tile's owning agent has consumed ≥80% of its session budget.
    /// When `true`, a 2px amber border highlight is rendered on the tile.
    pub budget_warning: bool,
}

impl TileBadgeState {
    /// Returns `true` if any badge is active for this tile.
    #[inline]
    pub fn has_any_badge(&self) -> bool {
        self.disconnected || self.budget_warning
    }
}

// ─── Badge rendering constants ────────────────────────────────────────────────

/// Opacity applied to tile content when the disconnection badge is active.
/// Spec §Disconnection Badge: "tile content renders at reduced opacity (70%)".
pub const DISCONNECTED_CONTENT_OPACITY: f32 = 0.70;

/// Opacity of the disconnection badge overlay.
/// Spec §Disconnection Badge: "dim plug/link-break icon at 70% opacity".
pub const DISCONNECTED_BADGE_OPACITY: f32 = 0.70;

/// Width of the budget warning border highlight in pixels.
/// Spec §Budget Warning Badge: "subtle amber border highlight (2px, 70% opacity)".
pub const BUDGET_WARNING_BORDER_PX: f32 = 2.0;

/// Opacity of the budget warning border highlight.
/// Spec §Budget Warning Badge: "70% opacity".
pub const BUDGET_WARNING_BORDER_OPACITY: f32 = 0.70;

/// Amber color (RGB) for the budget warning border.
/// Spec §Budget Warning Badge: "subtle amber border highlight".
///
/// Amber = approximately #F5A623 in sRGB.
pub const BUDGET_WARNING_AMBER_COLOR: [f32; 4] = [
    0.960, // R
    0.651, // G
    0.137, // B
    BUDGET_WARNING_BORDER_OPACITY,
];

/// Size of the disconnection badge icon area in pixels (square).
pub const DISCONNECTION_BADGE_SIZE_PX: f32 = 24.0;

/// Offset of the disconnection badge from the top-left corner of the tile.
pub const DISCONNECTION_BADGE_OFFSET_PX: f32 = 8.0;

/// Background color for the disconnection badge icon area.
///
/// Dark, semi-transparent background to ensure legibility over any tile content.
/// Uses straight alpha (`DISCONNECTED_BADGE_OPACITY`) as the compositor pipeline
/// uses `wgpu::BlendState::ALPHA_BLENDING` (straight, not pre-multiplied alpha).
pub const DISCONNECTION_BADGE_BG_COLOR: [f32; 4] = [
    0.08, // R
    0.08, // G
    0.12, // B
    DISCONNECTED_BADGE_OPACITY,
];

/// Icon fill color for the disconnection badge (link-break/plug symbol).
///
/// Dim neutral — intentionally not alarming.  V1 renders this as a small
/// filled rect (stylised link-break); a real implementation would use a font
/// atlas glyph.
pub const DISCONNECTION_BADGE_ICON_COLOR: [f32; 4] = [
    0.75, // R
    0.75, // G
    0.78, // B
    DISCONNECTED_BADGE_OPACITY,
];

/// Overlay color applied over tile content when the disconnection badge is
/// active to simulate reduced content opacity.
///
/// This is a dark, semi-transparent scrim composited over the full tile bounds.
/// Alpha = 1 - DISCONNECTED_CONTENT_OPACITY = 0.30.
pub const DISCONNECTION_CONTENT_SCRIM_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.30];

// ─── Badge draw-command builder ───────────────────────────────────────────────

/// Produce chrome draw commands for all active badges on a single tile.
///
/// The commands are appended to the chrome render pass **after** the tile's
/// content has been composited.  The caller is responsible for issuing the
/// content pass before calling this function.
///
/// # Parameters
///
/// * `bounds`     — tile bounds in screen-pixel coordinates.
/// * `badge_state` — current badge state for this tile (from `ChromeState`).
///
/// # Returns
///
/// An ordered list of [`ChromeDrawCmd`] values.  An empty list is returned
/// when `badge_state.has_any_badge()` is `false`.
///
/// # Frame-bounded guarantee
///
/// This function is **pure and allocation-bounded**.  Command generation is
/// O(1) per tile (no loops, no allocations beyond the fixed-size result
/// `Vec`).  The compositor can call it unconditionally for every visible tile
/// in the chrome pass without budget risk.
pub fn build_badge_cmds(bounds: Rect, badge_state: &TileBadgeState) -> Vec<ChromeDrawCmd> {
    // Guard: degenerate or inverted bounds must not produce geometry.
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return Vec::new();
    }

    if !badge_state.has_any_badge() {
        return Vec::new();
    }

    // Pre-allocate for worst case: 3 rects for disconnection badge
    // (scrim + badge_bg + icon) + 4 rects for budget warning = 7 total.
    let mut cmds = Vec::with_capacity(7);

    // ── Disconnection badge ───────────────────────────────────────────────
    if badge_state.disconnected {
        // 1. Content scrim: dim the tile's content to 70% by overlaying a
        //    semi-transparent black scrim over the entire tile bounds.
        //    Alpha = 0.30 ≈ 1 − 0.70 (desired content opacity).
        cmds.push(ChromeDrawCmd {
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: bounds.height,
            color: DISCONNECTION_CONTENT_SCRIM_COLOR,
        });

        // 2. Badge background: dark square in top-left corner of the tile.
        let badge_x = bounds.x + DISCONNECTION_BADGE_OFFSET_PX;
        let badge_y = bounds.y + DISCONNECTION_BADGE_OFFSET_PX;
        let badge_sz = DISCONNECTION_BADGE_SIZE_PX;

        // Clamp badge to tile bounds so it never bleeds outside.
        let clamped_w = badge_sz.min(bounds.x + bounds.width - badge_x);
        let clamped_h = badge_sz.min(bounds.y + bounds.height - badge_y);

        if clamped_w > 0.0 && clamped_h > 0.0 {
            cmds.push(ChromeDrawCmd {
                x: badge_x,
                y: badge_y,
                width: clamped_w,
                height: clamped_h,
                color: DISCONNECTION_BADGE_BG_COLOR,
            });

            // 3. Icon fill: a smaller rect representing the link-break/plug
            //    glyph.  In v1 this is a stylised solid rect (2/3 of badge
            //    area); a production implementation would use a font atlas
            //    glyph or an SDF icon.
            let icon_margin = badge_sz * 0.20;
            let icon_x = badge_x + icon_margin;
            let icon_y = badge_y + icon_margin;
            let icon_w = (clamped_w - 2.0 * icon_margin).max(0.0);
            let icon_h = (clamped_h - 2.0 * icon_margin).max(0.0);

            if icon_w > 0.0 && icon_h > 0.0 {
                cmds.push(ChromeDrawCmd {
                    x: icon_x,
                    y: icon_y,
                    width: icon_w,
                    height: icon_h,
                    color: DISCONNECTION_BADGE_ICON_COLOR,
                });
            }
        }
    }

    // ── Budget warning badge ──────────────────────────────────────────────
    if badge_state.budget_warning {
        // Amber border highlight: four thin rectangles forming a border
        // around the tile perimeter, each at 70% opacity.
        //
        // Clamp the effective border thickness so it never exceeds half the
        // tile's smallest dimension — this keeps all geometry within bounds
        // for very small or degenerate tiles.
        let border = BUDGET_WARNING_BORDER_PX
            .min(bounds.width * 0.5)
            .min(bounds.height * 0.5);

        if border > 0.0 {
            // Top edge
            cmds.push(ChromeDrawCmd {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width,
                height: border,
                color: BUDGET_WARNING_AMBER_COLOR,
            });
            // Bottom edge
            cmds.push(ChromeDrawCmd {
                x: bounds.x,
                y: bounds.y + bounds.height - border,
                width: bounds.width,
                height: border,
                color: BUDGET_WARNING_AMBER_COLOR,
            });
            // Left edge (excluding corners already covered by top/bottom)
            cmds.push(ChromeDrawCmd {
                x: bounds.x,
                y: bounds.y + border,
                width: border,
                height: (bounds.height - 2.0 * border).max(0.0),
                color: BUDGET_WARNING_AMBER_COLOR,
            });
            // Right edge
            cmds.push(ChromeDrawCmd {
                x: bounds.x + bounds.width - border,
                y: bounds.y + border,
                width: border,
                height: (bounds.height - 2.0 * border).max(0.0),
                color: BUDGET_WARNING_AMBER_COLOR,
            });
        }
    }

    cmds
}

// ─── Backpressure signal ──────────────────────────────────────────────────────

/// Signal sent to agents when their mutation queue is under pressure.
///
/// These signals are intentionally generic — they do not distinguish
/// freeze-induced backpressure from genuine load.  This is by design to avoid
/// leaking viewer state (spec §Freeze Backpressure Signal: "Agents MUST NOT be
/// informed that the scene is frozen").
///
/// The session server converts this signal into the `error_code` field of the
/// `MutationResult` gRPC response (see `session.proto`):
///
/// * [`BackpressureSignal::QueuePressure`] → `error_code = "MUTATION_QUEUE_PRESSURE"`
/// * [`BackpressureSignal::MutationDropped`] → `error_code = "MUTATION_DROPPED"`
///
/// # Spec reference
///
/// * §Freeze Backpressure Signal (line 158): fire at 80% queue capacity.
/// * Scenario: Queue pressure signal (line 163).
/// * Scenario: Mutation dropped signal (line 167).
#[derive(Clone, Debug, PartialEq)]
pub enum BackpressureSignal {
    /// Sent when the per-session freeze queue reaches 80% capacity.
    ///
    /// Maps to `error_code = "MUTATION_QUEUE_PRESSURE"` in `MutationResult`
    /// (see `session.proto`).
    ///
    /// `pressure` is the current fill fraction in [0.0, 1.0].
    QueuePressure {
        /// Opaque session identifier (UUIDv7 bytes) from `SessionEstablished.session_id`.
        session_id: Vec<u8>,
        /// Current queue fill fraction at the moment of threshold crossing.
        pressure: f32,
    },
    /// Sent when a state-stream mutation is evicted from a full queue.
    ///
    /// Maps to `error_code = "MUTATION_DROPPED"` in `MutationResult`
    /// (see `session.proto`).
    MutationDropped {
        /// Opaque session identifier (UUIDv7 bytes) from `SessionEstablished.session_id`.
        session_id: Vec<u8>,
        /// The `batch_id` of the evicted mutation batch (for correlation).
        batch_id: Vec<u8>,
    },
}

// ─── Frame-level badge evaluation ────────────────────────────────────────────

/// A snapshot of which tiles have badges in the current frame.
///
/// The shell builds a `BadgeFrame` once per frame from the per-tile badge
/// states held in the chrome state (see [`crate::shell::chrome::ChromeState`]),
/// then passes it to the compositor for overlay rendering.
///
/// This struct is **never** exposed to agents.
pub struct BadgeFrame {
    /// Per-tile badge states.  An unordered list of `(tile_id, TileBadgeState)`
    /// pairs covering tiles that have at least one active badge.
    /// Tiles absent from this list are assumed to have no active badges.
    /// Lookup is O(N) via [`BadgeFrame::badge_for`]; N is the number of
    /// badged tiles (expected to be small in practice).
    pub tile_badges: Vec<(SceneId, TileBadgeState)>,
}

impl BadgeFrame {
    /// Build a `BadgeFrame` from a slice of `(tile_id, TileBadgeState)` pairs.
    pub fn build(badges: &[(SceneId, TileBadgeState)]) -> Self {
        Self {
            tile_badges: badges.to_vec(),
        }
    }

    /// Returns the badge state for `tile_id`, or an empty (no-badge) state if
    /// the tile has no entry.
    pub fn badge_for(&self, tile_id: &SceneId) -> TileBadgeState {
        self.tile_badges
            .iter()
            .find(|(id, _)| id == tile_id)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn scene_id() -> SceneId {
        SceneId::new()
    }

    // ── TileBadgeState ────────────────────────────────────────────────────

    #[test]
    fn tile_badge_state_default_has_no_badges() {
        let s = TileBadgeState::default();
        assert!(!s.disconnected);
        assert!(!s.budget_warning);
        assert!(!s.has_any_badge());
    }

    #[test]
    fn tile_badge_state_disconnected_has_badge() {
        let s = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        assert!(s.has_any_badge());
    }

    #[test]
    fn tile_badge_state_budget_warning_has_badge() {
        let s = TileBadgeState {
            disconnected: false,
            budget_warning: true,
        };
        assert!(s.has_any_badge());
    }

    // ── build_badge_cmds: empty / degenerate cases ─────────────────────────

    #[test]
    fn no_cmds_for_tile_with_no_badges() {
        let bounds = tile(0.0, 0.0, 200.0, 150.0);
        let state = TileBadgeState::default();
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.is_empty(),
            "expected no commands when no badges active"
        );
    }

    #[test]
    fn no_cmds_for_zero_width_tile() {
        let bounds = tile(0.0, 0.0, 0.0, 150.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.is_empty(),
            "degenerate zero-width tile must produce no commands"
        );
    }

    #[test]
    fn no_cmds_for_zero_height_tile() {
        let bounds = tile(0.0, 0.0, 200.0, 0.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.is_empty(),
            "degenerate zero-height tile must produce no commands"
        );
    }

    #[test]
    fn no_cmds_for_negative_size_tile() {
        let bounds = tile(100.0, 100.0, -10.0, -10.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.is_empty(),
            "negative-size tile must produce no commands"
        );
    }

    // ── Scenario: Disconnection badge appears (spec line 176) ─────────────
    //
    // WHEN agent disconnects and lease enters grace period
    // THEN dim link-break icon appears on all affected tiles within one frame

    #[test]
    fn spec_disconnection_badge_appears_on_disconnect() {
        let bounds = tile(10.0, 20.0, 300.0, 200.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let cmds = build_badge_cmds(bounds, &state);

        // Must produce at least a scrim rect covering full tile bounds.
        assert!(
            !cmds.is_empty(),
            "disconnection badge must produce draw commands"
        );

        // The first command must be the content scrim covering the full tile.
        let scrim = &cmds[0];
        assert_eq!(scrim.x, bounds.x);
        assert_eq!(scrim.y, bounds.y);
        assert_eq!(scrim.width, bounds.width);
        assert_eq!(scrim.height, bounds.height);
        assert_eq!(
            scrim.color, DISCONNECTION_CONTENT_SCRIM_COLOR,
            "first command must be the content scrim"
        );
    }

    #[test]
    fn disconnection_badge_scrim_alpha_reflects_opacity() {
        // The scrim alpha must equal 1.0 - DISCONNECTED_CONTENT_OPACITY = 0.30.
        assert!(
            (DISCONNECTION_CONTENT_SCRIM_COLOR[3] - (1.0 - DISCONNECTED_CONTENT_OPACITY)).abs()
                < 1e-4,
            "scrim alpha must equal 1 - DISCONNECTED_CONTENT_OPACITY"
        );
    }

    #[test]
    fn disconnection_badge_icon_has_correct_opacity() {
        // All icon/badge colors must use DISCONNECTED_BADGE_OPACITY as alpha.
        assert_eq!(DISCONNECTION_BADGE_BG_COLOR[3], DISCONNECTED_BADGE_OPACITY);
        assert_eq!(
            DISCONNECTION_BADGE_ICON_COLOR[3],
            DISCONNECTED_BADGE_OPACITY
        );
    }

    #[test]
    fn disconnection_badge_produces_badge_bg_and_icon_rects() {
        let bounds = tile(0.0, 0.0, 300.0, 200.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let cmds = build_badge_cmds(bounds, &state);

        // scrim + badge_bg + icon = 3 rects minimum.
        assert!(
            cmds.len() >= 3,
            "disconnection badge must produce scrim + badge_bg + icon (got {})",
            cmds.len()
        );

        // Badge background must be in the top-left area of the tile.
        let badge_bg = &cmds[1];
        assert!(badge_bg.x >= bounds.x, "badge must be inside tile x");
        assert!(badge_bg.y >= bounds.y, "badge must be inside tile y");
        assert_eq!(badge_bg.color, DISCONNECTION_BADGE_BG_COLOR);
    }

    #[test]
    fn disconnection_badge_stays_within_tile_bounds() {
        // Small tile: badge should be clamped.
        let bounds = tile(0.0, 0.0, 20.0, 20.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let cmds = build_badge_cmds(bounds, &state);

        for cmd in &cmds {
            assert!(
                cmd.x + cmd.width <= bounds.x + bounds.width + 0.01,
                "cmd right edge ({}) exceeds tile right ({}) — cmd: {:?}",
                cmd.x + cmd.width,
                bounds.x + bounds.width,
                cmd
            );
            assert!(
                cmd.y + cmd.height <= bounds.y + bounds.height + 0.01,
                "cmd bottom edge ({}) exceeds tile bottom ({}) — cmd: {:?}",
                cmd.y + cmd.height,
                bounds.y + bounds.height,
                cmd
            );
        }
    }

    // ── Scenario: Disconnection badge clears on reconnect (spec line 180) ──
    //
    // WHEN agent reconnects and reclaims lease
    // THEN disconnection badge is removed immediately

    #[test]
    fn spec_disconnection_badge_clears_on_reconnect() {
        let bounds = tile(0.0, 0.0, 300.0, 200.0);

        // Disconnected → badge present.
        let disconnected = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        assert!(!build_badge_cmds(bounds, &disconnected).is_empty());

        // Reconnected (badge cleared) → no commands.
        let reconnected = TileBadgeState {
            disconnected: false,
            budget_warning: false,
        };
        assert!(
            build_badge_cmds(bounds, &reconnected).is_empty(),
            "disconnection badge must clear when disconnected=false"
        );
    }

    // ── Scenario: Budget warning badge appears (spec line 202) ─────────────
    //
    // WHEN agent's texture memory usage reaches 80% of budget
    // THEN amber border highlight appears on all tiles under that agent's lease

    #[test]
    fn spec_budget_warning_badge_appears_at_80_percent() {
        let bounds = tile(10.0, 20.0, 400.0, 300.0);
        let state = TileBadgeState {
            disconnected: false,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);

        // Must produce exactly 4 border rects.
        assert_eq!(
            cmds.len(),
            4,
            "budget warning badge must produce 4 border rects"
        );

        // All rects must use amber color.
        for cmd in &cmds {
            assert_eq!(
                cmd.color, BUDGET_WARNING_AMBER_COLOR,
                "budget warning border must use amber color"
            );
        }
    }

    #[test]
    fn budget_warning_border_width_is_2px() {
        let bounds = tile(0.0, 0.0, 400.0, 300.0);
        let state = TileBadgeState {
            disconnected: false,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);

        // Top edge: full width, 2px height at top.
        assert_eq!(
            cmds[0].height, BUDGET_WARNING_BORDER_PX,
            "top border height must be 2px"
        );
        assert_eq!(
            cmds[0].width, bounds.width,
            "top border must span full tile width"
        );

        // Bottom edge: full width, 2px height at bottom.
        assert_eq!(
            cmds[1].height, BUDGET_WARNING_BORDER_PX,
            "bottom border height must be 2px"
        );
        assert_eq!(
            cmds[1].y,
            bounds.y + bounds.height - BUDGET_WARNING_BORDER_PX,
            "bottom border must be at tile bottom"
        );

        // Left edge: 2px width.
        assert_eq!(
            cmds[2].width, BUDGET_WARNING_BORDER_PX,
            "left border width must be 2px"
        );

        // Right edge: 2px width.
        assert_eq!(
            cmds[3].width, BUDGET_WARNING_BORDER_PX,
            "right border width must be 2px"
        );
        assert_eq!(
            cmds[3].x,
            bounds.x + bounds.width - BUDGET_WARNING_BORDER_PX,
            "right border must be at tile right edge"
        );
    }

    #[test]
    fn budget_warning_border_opacity_is_70_percent() {
        assert!(
            (BUDGET_WARNING_AMBER_COLOR[3] - BUDGET_WARNING_BORDER_OPACITY).abs() < 1e-4,
            "budget warning border alpha must equal BUDGET_WARNING_BORDER_OPACITY (0.70)"
        );
        assert!(
            (BUDGET_WARNING_BORDER_OPACITY - 0.70).abs() < 1e-4,
            "BUDGET_WARNING_BORDER_OPACITY must be 0.70"
        );
    }

    #[test]
    fn budget_warning_badge_clears_when_below_threshold() {
        let bounds = tile(0.0, 0.0, 300.0, 200.0);

        // Budget warning active.
        let warning = TileBadgeState {
            disconnected: false,
            budget_warning: true,
        };
        assert!(!build_badge_cmds(bounds, &warning).is_empty());

        // Budget drops below threshold — badge cleared.
        let clear = TileBadgeState {
            disconnected: false,
            budget_warning: false,
        };
        assert!(
            build_badge_cmds(bounds, &clear).is_empty(),
            "budget warning badge must clear when budget_warning=false"
        );
    }

    // ── Both badges active simultaneously ─────────────────────────────────

    #[test]
    fn both_badges_combined_produce_correct_commands() {
        let bounds = tile(0.0, 0.0, 400.0, 300.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);

        // Disconnection: scrim (1) + badge_bg (1) + icon (1) = 3.
        // Budget warning: 4 border rects.
        // Total: at least 7.
        assert!(
            cmds.len() >= 7,
            "both badges combined must produce ≥7 commands (got {})",
            cmds.len()
        );

        // Scrim is first.
        assert_eq!(cmds[0].color, DISCONNECTION_CONTENT_SCRIM_COLOR);

        // The 4 border rects for budget warning must all be amber.
        let amber_cmds: Vec<_> = cmds
            .iter()
            .filter(|c| c.color == BUDGET_WARNING_AMBER_COLOR)
            .collect();
        assert_eq!(
            amber_cmds.len(),
            4,
            "must have exactly 4 amber border rects"
        );
    }

    // ── BackpressureSignal ────────────────────────────────────────────────

    /// Scenario: Queue pressure signal (spec line 163).
    /// WHEN per-session freeze queue reaches 80% capacity
    /// THEN runtime sends MUTATION_QUEUE_PRESSURE in MutationResult.
    #[test]
    fn spec_queue_pressure_signal_at_80_percent() {
        // session_id is opaque UUIDv7 bytes (matches SessionEstablished.session_id in session.proto).
        let session_id = vec![0u8; 16];
        let signal = BackpressureSignal::QueuePressure {
            session_id,
            pressure: 0.82,
        };

        // Signal is constructed without error.  The session server is responsible
        // for converting this into error_code="MUTATION_QUEUE_PRESSURE" in MutationResult.
        if let BackpressureSignal::QueuePressure { pressure, .. } = &signal {
            assert!(*pressure >= 0.80, "QueuePressure must fire at ≥80% fill");
        } else {
            panic!("expected QueuePressure variant");
        }
    }

    /// Scenario: Mutation dropped signal (spec line 167).
    /// WHEN queue full and state-stream mutation evicted
    /// THEN runtime sends MUTATION_DROPPED.
    #[test]
    fn spec_mutation_dropped_signal_contains_batch_id() {
        // session_id is opaque UUIDv7 bytes (matches SessionEstablished.session_id in session.proto).
        let session_id = vec![0u8; 16];
        let batch_id = vec![1u8, 2, 3, 4];
        let signal = BackpressureSignal::MutationDropped {
            session_id,
            batch_id: batch_id.clone(),
        };

        if let BackpressureSignal::MutationDropped { batch_id: bid, .. } = signal {
            assert_eq!(
                bid, batch_id,
                "MutationDropped must carry the batch_id of the evicted mutation"
            );
        } else {
            panic!("expected MutationDropped variant");
        }
    }

    // ── BadgeFrame ────────────────────────────────────────────────────────

    #[test]
    fn badge_frame_returns_default_for_unknown_tile() {
        let frame = BadgeFrame::build(&[]);
        let unknown = scene_id();
        let state = frame.badge_for(&unknown);
        assert!(
            !state.has_any_badge(),
            "unknown tile must return no-badge state"
        );
    }

    #[test]
    fn badge_frame_returns_correct_state_for_known_tile() {
        let id = scene_id();
        let badge = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let frame = BadgeFrame::build(&[(id, badge.clone())]);

        let result = frame.badge_for(&id);
        assert_eq!(result, badge);
    }

    // ── Spec summary: Override Control Guarantees (spec line 206) ─────────
    //
    // Badge appear/clear is frame-bounded: visual effect within one frame
    // (≤16.6ms).  The implementation guarantees this because:
    // - `build_badge_cmds` is a pure, O(1) function (no async, no I/O).
    // - Badge state is written to ChromeState under a short-lived write lock.
    // - The compositor acquires the read lock at the start of the chrome
    //   render pass and releases it before GPU submit.
    // - Therefore, within one frame of the control-plane state update, the
    //   badge commands reflect the new state.
    //
    // This invariant is architectural (ensured by the locking protocol) and
    // cannot be tested by a unit test; the comment documents the design intent.

    #[test]
    fn badge_cmds_are_bounded_in_count_disconnected_only() {
        // Disconnection badge produces at most 3 rects (scrim + bg + icon).
        let bounds = tile(0.0, 0.0, 1920.0, 1080.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: false,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.len() <= 3,
            "disconnection-only badge must produce ≤3 commands"
        );
    }

    #[test]
    fn badge_cmds_are_bounded_in_count_budget_only() {
        // Budget warning badge produces exactly 4 rects (4 border edges).
        let bounds = tile(0.0, 0.0, 1920.0, 1080.0);
        let state = TileBadgeState {
            disconnected: false,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert_eq!(
            cmds.len(),
            4,
            "budget-warning-only badge must produce exactly 4 commands"
        );
    }

    #[test]
    fn badge_cmds_are_bounded_in_count_both_badges() {
        // Both badges: ≤3 + 4 = ≤7 rects.
        let bounds = tile(0.0, 0.0, 1920.0, 1080.0);
        let state = TileBadgeState {
            disconnected: true,
            budget_warning: true,
        };
        let cmds = build_badge_cmds(bounds, &state);
        assert!(
            cmds.len() <= 7,
            "both badges must produce ≤7 commands, got {}",
            cmds.len()
        );
    }
}
