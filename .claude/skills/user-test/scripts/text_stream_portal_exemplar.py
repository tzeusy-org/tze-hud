#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "grpcio>=1.80.0",
#   "protobuf>=6.31.1",
# ]
# ///
"""
Text Stream Portal exemplar user-test scenario.

Renders a two-pane bounded portal surface (INPUT left · OUTPUT right) on the
live HUD via the resident `HudSession` gRPC stream, using only existing scene
primitives (SolidColor, TextMarkdown, HitRegion). No terminal-emulator node,
no new node type, no chrome-hosted affordances.

Layout:
  - Equal 50/50 split between INPUT and OUTPUT panes
  - Header drag surface moves the whole portal group.
  - Fat 6px divider (with centred grip bar + hit region) between panes.
  - The static frame is one tile; the input composer and transcript body are
    separate transparent scroll-capture tiles so wheel input cannot move the
    whole portal.
  - Panes render at 95% black opacity (operator preference).

Phases:
  - baseline  : render full chrome + transcript viewport, then hold
  - scroll    : register output scroll, step through transcript, append tail,
                then return to latest output
  - streaming : clear transcript, reveal content in ordered chunks
  - rapid     : rapid-replace stress (coalescing-coherence smoke)
  - soak      : sustained constant-window streaming for memory-drift evidence

Emits per-step JSON transcript to stdout and writes an artifact (default:
`test_results/text-stream-portal-latest.json`).
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import contextlib
import json
import os
import platform
import socket
import subprocess
import sys
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

# Resolve HudClient + proto stubs (co-located).
_SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _SCRIPT_DIR)
sys.path.insert(0, os.path.join(_SCRIPT_DIR, "proto_gen"))

from hud_grpc_client import HudClient, _make_node  # noqa: E402
from proto_gen import events_pb2, types_pb2  # noqa: E402

import portal_part_tokens as ppt  # noqa: E402
import portal_two_pane_geometry as pg  # noqa: E402


# ─── Portal design tokens (resolved, not literal) ─────────────────────────────
#
# hud-7jrj3 / finding vd-exemplar-hardcodes-all-visual-values: every published
# color/font below is now RESOLVED from the canonical portal token surface
# (`portal_part_tokens.py`, a sync-guarded mirror of Rust
# `crates/tze_hud_config/src/portal_tokens.rs`) rather than being a bare literal.
#
# The exemplar's reviewed look is expressed as EXEMPLAR_PROFILE_OVERRIDES — a
# profile-scoped override map, exactly the way a real component profile is a
# swappable set of token overrides. Swapping the active profile (see
# `apply_visual_profile`) re-resolves the tokens and reskins every published
# value end-to-end. This is what the profile-swap phase exercises live, and what
# the reskin assertion in the test suite proves.
#
# Runtime handshake (hud-16um0): the runtime now exposes its *resolved*
# PortalPartTokens over the session handshake
# (`SessionEstablished.portal_part_tokens`). When present, those runtime-active
# tokens are adopted as the PRIMARY source right after connect (see
# `adopt_runtime_tokens`), so the runtime's active profile drives this exemplar.
# The EXEMPLAR_PROFILE_OVERRIDES map below is the typed FALLBACK used only when
# the runtime predates the field (or `--ignore-runtime-tokens` is passed).
PORTAL_TOKEN = ppt

# Exemplar profile: the reviewed portal look, expressed as portal.* token
# overrides (8-bit hex / decimal — the same value forms Rust parses). Absent
# keys fall back to the canonical defaults in `portal_part_tokens.CANONICAL_DEFAULTS`.
EXEMPLAR_PROFILE_OVERRIDES: dict[str, str] = {
    ppt.PORTAL_TOKEN_FRAME_BACKGROUND: "#0000004D",        # black @ ~0.30 (light glass frame)
    ppt.PORTAL_TOKEN_FRAME_BORDER_COLOR: "#FFFFFF1F",      # white @ ~0.12 composer border
    ppt.PORTAL_TOKEN_HEADER_TEXT_COLOR: "#FAFCFF",         # near-white title
    ppt.PORTAL_TOKEN_HEADER_FONT_SIZE: "18",               # exemplar title size
    ppt.PORTAL_TOKEN_COMPOSER_BACKGROUND: "#000000F2",     # black @ ~0.95 input pane
    ppt.PORTAL_TOKEN_COMPOSER_TEXT_COLOR: "#E0EBFA",       # input text
    ppt.PORTAL_TOKEN_COMPOSER_CARET_COLOR: "#7ADB8FF2",    # green accent caret @ ~0.95
    ppt.PORTAL_TOKEN_COMPOSER_PLACEHOLDER_COLOR: "#808CA3C7",  # muted placeholder @ ~0.78
    ppt.PORTAL_TOKEN_TRANSCRIPT_BACKGROUND: "#000000F2",   # black @ ~0.95 output pane
    ppt.PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR: "#EBF0F7",     # body text
    ppt.PORTAL_TOKEN_TRANSCRIPT_DIM_TEXT_COLOR: "#C7D1E0E0",   # secondary labels @ ~0.88
    ppt.PORTAL_TOKEN_TRANSCRIPT_DIM_BACKGROUND: "#00000080",   # black @ ~0.50 header/footer panels
    ppt.PORTAL_TOKEN_COLLAPSED_BACKGROUND: "#12161C",      # off-black compact/minimized surface (hud-4e6c0; mirrors hud-0hj7f runtime value)
    ppt.PORTAL_TOKEN_LIFECYCLE_ACTIVE_COLOR: "#7ADB8FEB",  # green activity dot @ ~0.92
    ppt.PORTAL_TOKEN_DIVIDER_COLOR: "#FFFFFF1A",           # white @ ~0.10 hairline dividers
    ppt.PORTAL_TOKEN_WINDOW_RESIZE_GRIP_COLOR: "#FFFFFFA8",    # white @ ~0.66 header grip
    ppt.PORTAL_TOKEN_WINDOW_RESIZE_GRIP_HOVER_COLOR: "#FFFFFF66",  # white @ ~0.40 pane grip nub
}

# Active profile override map + resolved tokens. `TOKENS` is the single source
# every published value reads from; `apply_visual_profile` rebinds both.
ACTIVE_PROFILE_OVERRIDES: dict[str, str] = dict(EXEMPLAR_PROFILE_OVERRIDES)
TOKENS: ppt.PortalPartTokens = ppt.resolve_portal_tokens(ACTIVE_PROFILE_OVERRIDES)

# Two-pane chrome geometry (hud-q1qzw): the exemplar's split-layout geometry
# (header height, pane split ratio, content inset, corner radius, …) resolves
# from an exemplar-local geometry profile — the same three-layer resolve the
# visual tokens use — so a geometry-profile swap reskins the chrome geometry,
# not just colors/fonts. This is an exemplar-local surface on purpose: the
# runtime has no two-pane concept, so these keys stay out of the canonical
# product token vocabulary (see portal_two_pane_geometry). `GEOMETRY` is the
# single source every geometry constant below reads from; `apply_geometry_profile`
# rebinds both. Empty overrides ⇒ the reviewed defaults, i.e. no visual change.
ACTIVE_GEOMETRY_OVERRIDES: dict[str, str] = {}
GEOMETRY: pg.PortalTwoPaneGeometry = pg.resolve_two_pane_geometry(ACTIVE_GEOMETRY_OVERRIDES)


def _rebind_visual_tokens() -> None:
    """Recompute every token-derived visual global from the current `TOKENS`.

    Colors are 4-tuples ``(r, g, b, a)`` in 0..1 (the form `make_solid_color_node`
    / `make_text_node` consume). Secondary label sizes are derived from the
    primary font tokens with fixed typographic steps, so they track a profile's
    font-size overrides. Two-pane chrome GEOMETRY (header height, pane split,
    content inset, corner radius) is rebound separately by
    `_rebind_geometry_tokens` from the exemplar-local geometry profile — the
    canonical `portal.spacing.*` tokens are calibrated for the single-node
    Phase-1 pilot, not this two-pane chrome, so it has its own profile surface
    (see portal_two_pane_geometry). Portal frame size/placement stays literal
    (a viewport-fit concern, not a skinnable property).
    """
    global BG_RGBA, HEADER_BG_RGBA, DIVIDER_RGBA, FOOTER_BG_RGBA
    global INPUT_PANE_BG_RGBA, OUTPUT_PANE_BG_RGBA, TEXT_WINDOW_BG_RGBA
    global COMPOSER_BORDER_RGBA, SUBMIT_HINT_RGBA, EYEBROW_RGBA
    global CARET_RGBA, STATIC_CARET_RGBA, PANE_DIVIDER_RGBA, PANE_DIVIDER_GRIP_RGBA
    global TITLE_RGBA, SUBTITLE_RGBA, BODY_RGBA, META_RGBA
    global ACTIVITY_DOT_RGBA, INPUT_TEXT_RGBA, INPUT_PLACEHOLDER_RGBA, HEADER_GRIP_RGBA
    global MINIMIZE_BG_RGBA, MINIMIZED_ICON_BG_RGBA, MINIMIZED_ICON_CORE_RGBA
    global TITLE_FONT, SUBTITLE_FONT, BODY_FONT, META_FONT
    global EYEBROW_FONT, INPUT_FONT, SUBMIT_HINT_FONT

    t = TOKENS
    BG_RGBA = t.frame_background
    HEADER_BG_RGBA = t.transcript_dim_background
    DIVIDER_RGBA = t.divider_color
    FOOTER_BG_RGBA = t.transcript_dim_background
    INPUT_PANE_BG_RGBA = t.composer_background
    OUTPUT_PANE_BG_RGBA = t.transcript_background
    TEXT_WINDOW_BG_RGBA = t.transcript_background
    COMPOSER_BORDER_RGBA = t.frame_border_color
    SUBMIT_HINT_RGBA = t.transcript_dim_text_color
    EYEBROW_RGBA = t.transcript_dim_text_color
    CARET_RGBA = t.composer_caret_color
    # Blink-off caret: same hue, zero alpha (a render state, not a color literal).
    STATIC_CARET_RGBA = (*t.composer_caret_color[:3], 0.0)
    PANE_DIVIDER_RGBA = t.divider_color
    PANE_DIVIDER_GRIP_RGBA = t.resize_grip_hover_color
    TITLE_RGBA = t.header_text_color
    SUBTITLE_RGBA = t.transcript_dim_text_color
    BODY_RGBA = t.transcript_text_color
    META_RGBA = t.transcript_dim_text_color
    ACTIVITY_DOT_RGBA = t.lifecycle_active_color
    INPUT_TEXT_RGBA = t.composer_text_color
    INPUT_PLACEHOLDER_RGBA = t.composer_placeholder_color
    HEADER_GRIP_RGBA = t.resize_grip_color
    # Compact/minimized-state control backgrounds resolve their hue from the
    # collapsed_card.background token — the collapsed-state surface token that
    # hud-0hj7f darkened to off-black — so the minimize button and minimized icon
    # inherit the off-black palette instead of a hardcoded blue-grey (hud-4e6c0).
    # Alpha stays a per-node functional parameter (button emphasis; floating-icon
    # outer/inner opacity for visibility + layer depth over arbitrary wallpaper),
    # mirroring the STATIC_CARET_RGBA precedent of token hue + explicit render alpha.
    MINIMIZE_BG_RGBA = (*t.collapsed_background[:3], 0.88)
    MINIMIZED_ICON_BG_RGBA = (*t.collapsed_background[:3], 0.72)
    MINIMIZED_ICON_CORE_RGBA = (*t.collapsed_background[:3], 0.94)
    TITLE_FONT = t.header_font_size_px
    # Secondary label sizes derive from the primary font tokens with fixed
    # typographic steps, so they track a profile's font-size overrides.
    SUBTITLE_FONT = max(1.0, t.transcript_font_size_px - 4.0)
    BODY_FONT = t.transcript_font_size_px
    META_FONT = max(1.0, t.transcript_font_size_px - 4.0)
    EYEBROW_FONT = max(1.0, t.transcript_font_size_px - 5.0)
    INPUT_FONT = t.composer_font_size_px
    SUBMIT_HINT_FONT = max(1.0, t.composer_font_size_px - 5.0)


def apply_visual_profile(overrides: Optional[dict[str, str]]) -> ppt.PortalPartTokens:
    """Swap the active portal profile and reskin every token-derived visual.

    ``overrides`` is a ``{token_key: value}`` map (a component profile). Passing
    ``None`` restores the exemplar profile. Re-resolves `TOKENS` and rebinds the
    module-level visual globals, so any subsequently rebuilt portal frame is
    published with the new palette/typography — proving the exemplar is
    token-driven end-to-end, not literal.
    """
    global ACTIVE_PROFILE_OVERRIDES, TOKENS
    ACTIVE_PROFILE_OVERRIDES = dict(overrides) if overrides is not None else dict(EXEMPLAR_PROFILE_OVERRIDES)
    TOKENS = ppt.resolve_portal_tokens(ACTIVE_PROFILE_OVERRIDES)
    _rebind_visual_tokens()
    return TOKENS


def adopt_runtime_tokens(runtime_tokens: dict[str, str]) -> bool:
    """Adopt the runtime's resolved portal tokens as the live source (hud-16um0).

    ``runtime_tokens`` is the ``{token_key: value_string}`` map the runtime
    delivers on the session handshake (``SessionEstablished.portal_part_tokens``),
    fully resolved against the runtime's ACTIVE profile. When present, it is the
    PRIMARY token source: the runtime's active profile — not the client-side
    :data:`EXEMPLAR_PROFILE_OVERRIDES` mirror — drives every published visual
    value. The map is a full override map (every canonical portal key present),
    so it flows straight through :func:`apply_visual_profile`, which parses it
    with the same drift-guarded resolver used for the local mirror.

    Returns ``True`` when runtime tokens were adopted, ``False`` when the map is
    empty (older runtime that predates the handshake field) — in which case the
    caller keeps the local exemplar-profile mirror as the typed fallback.
    """
    if not runtime_tokens:
        return False
    apply_visual_profile(dict(runtime_tokens))
    return True


def _rebind_geometry_tokens() -> None:
    """Recompute every geometry constant derived from the current `GEOMETRY`.

    The two-pane chrome geometry constants (header/footer/divider heights,
    content inset, corner radius, pane split) are module globals the layout path
    reads at call time. This rebinds them from the resolved `GEOMETRY` so a
    geometry-profile swap re-lays the chrome — the geometry analogue of
    `_rebind_visual_tokens`. `INPUT_PANE_W` is a live-mutable global (the viewer
    can drag the divider); a geometry swap resets it to the profile's split ratio.
    """
    global PORTAL_RADIUS, PADDING_X, HEADER_H, FOOTER_H, DIVIDER_H
    global PANE_DIVIDER_W, MIN_PANE_W, INPUT_PANE_W, MINIMIZE_HIT_H

    g = GEOMETRY
    PORTAL_RADIUS = g.corner_radius_px
    PADDING_X = g.content_inset_px
    HEADER_H = g.header_height_px
    FOOTER_H = g.footer_height_px
    DIVIDER_H = g.divider_height_px
    PANE_DIVIDER_W = g.pane_divider_width_px
    MIN_PANE_W = g.min_pane_width_px
    INPUT_PANE_W = (PORTAL_W - PANE_DIVIDER_W) * g.pane_split_ratio
    MINIMIZE_HIT_H = HEADER_H


def apply_geometry_profile(overrides: Optional[dict[str, str]]) -> pg.PortalTwoPaneGeometry:
    """Swap the active two-pane geometry profile and re-lay the chrome geometry.

    ``overrides`` is a ``{token_key: value}`` geometry profile. Passing ``None``
    restores the exemplar defaults. Re-resolves `GEOMETRY` and rebinds the
    geometry constants, so any subsequently rebuilt portal frame is laid out with
    the new geometry — proving the two-pane chrome geometry is profile-driven
    end-to-end, not literal.
    """
    global ACTIVE_GEOMETRY_OVERRIDES, GEOMETRY
    ACTIVE_GEOMETRY_OVERRIDES = dict(overrides) if overrides is not None else {}
    GEOMETRY = pg.resolve_two_pane_geometry(ACTIVE_GEOMETRY_OVERRIDES)
    _rebind_geometry_tokens()
    return GEOMETRY


# ─── Portal chrome geometry ───────────────────────────────────────────────────
#
# Portal frame size + placement stays literal — it is a viewport-fit concern
# (clamped to the display), not a skinnable chrome property. The two-pane chrome
# geometry below (header/footer/divider, content inset, corner radius, pane
# split) is resolved from `GEOMETRY` (the active geometry profile) and rebound by
# `_rebind_geometry_tokens` on a profile swap — never a bare literal.

PORTAL_W = 860.0
PORTAL_H = 680.0
PORTAL_MIN_W = 640.0
PORTAL_MIN_H = 480.0
PORTAL_DEFAULT_WIDTH_PCT = PORTAL_W / 1920.0
PORTAL_DEFAULT_HEIGHT_PCT = PORTAL_H / 1080.0
PORTAL_RADIUS = GEOMETRY.corner_radius_px  # frame corner radius (two-pane profile)
PORTAL_X_FROM_RIGHT = 28.0
PORTAL_Y = 120.0
PORTAL_Z = 220

# ── Token-derived visual identity (resolved, never literal) ───────────────────
#
# Every color/font below is sourced from the resolved `TOKENS` (the active
# portal profile). These module globals are (re)bound by `_rebind_visual_tokens`
# — the initial binds here run at import; `apply_visual_profile` re-binds them on
# a profile swap so the whole surface reskins. Colors are 4-tuples (r,g,b,a) in
# 0..1; fonts are px floats.
BG_RGBA = TOKENS.frame_background              # light portal frame only
HEADER_BG_RGBA = TOKENS.transcript_dim_background   # header panel (denser than frame)
DIVIDER_RGBA = TOKENS.divider_color
FOOTER_BG_RGBA = TOKENS.transcript_dim_background
# Input + output panes source the composer / transcript backgrounds.
INPUT_PANE_BG_RGBA = TOKENS.composer_background
OUTPUT_PANE_BG_RGBA = TOKENS.transcript_background
TEXT_WINDOW_BG_RGBA = TOKENS.transcript_background
COMPOSER_BORDER_RGBA = TOKENS.frame_border_color
SUBMIT_HINT_RGBA = TOKENS.transcript_dim_text_color
EYEBROW_RGBA = TOKENS.transcript_dim_text_color
CARET_RGBA = TOKENS.composer_caret_color
STATIC_CARET_RGBA = (*TOKENS.composer_caret_color[:3], 0.0)

TITLE_RGBA = TOKENS.header_text_color
SUBTITLE_RGBA = TOKENS.transcript_dim_text_color
BODY_RGBA = TOKENS.transcript_text_color
META_RGBA = TOKENS.transcript_dim_text_color
ACTIVITY_DOT_RGBA = TOKENS.lifecycle_active_color
INPUT_TEXT_RGBA = TOKENS.composer_text_color
INPUT_PLACEHOLDER_RGBA = TOKENS.composer_placeholder_color
HEADER_GRIP_RGBA = TOKENS.resize_grip_color
# Compact/minimized-state control backgrounds (see _rebind_visual_tokens for the
# hud-4e6c0 rationale): token hue from collapsed_card.background, functional alpha.
MINIMIZE_BG_RGBA = (*TOKENS.collapsed_background[:3], 0.88)
MINIMIZED_ICON_BG_RGBA = (*TOKENS.collapsed_background[:3], 0.72)
MINIMIZED_ICON_CORE_RGBA = (*TOKENS.collapsed_background[:3], 0.94)

TITLE_FONT = TOKENS.header_font_size_px
SUBTITLE_FONT = max(1.0, TOKENS.transcript_font_size_px - 4.0)
BODY_FONT = TOKENS.transcript_font_size_px
META_FONT = max(1.0, TOKENS.transcript_font_size_px - 4.0)
EYEBROW_FONT = max(1.0, TOKENS.transcript_font_size_px - 5.0)
INPUT_FONT = TOKENS.composer_font_size_px
SUBMIT_HINT_FONT = max(1.0, TOKENS.composer_font_size_px - 5.0)

# Two-pane chrome geometry, resolved from the active geometry profile (never a
# bare literal). `_rebind_geometry_tokens` re-binds these on a profile swap.
PADDING_X = GEOMETRY.content_inset_px       # per-side content inset
HEADER_H = GEOMETRY.header_height_px
FOOTER_H = GEOMETRY.footer_height_px
DIVIDER_H = GEOMETRY.divider_height_px
ACTIVITY_DOT_SIZE = 8.0

# Pane split with a fat divider between panes. The split ratio is profile-driven
# (default 0.5 ⇒ the reviewed equal 50/50 split); runtime pointer capture exists,
# but resize-on-drag still needs portal-side geometry mutation logic.
PANE_DIVIDER_W = GEOMETRY.pane_divider_width_px
MIN_PANE_W = GEOMETRY.min_pane_width_px
INPUT_PANE_W = (PORTAL_W - PANE_DIVIDER_W) * GEOMETRY.pane_split_ratio
# Pane divider hairline is token-sourced (rebound on profile swap); the grip
# nub derives from the resize-grip token so it tracks the same profile.
PANE_DIVIDER_RGBA = TOKENS.divider_color
PANE_DIVIDER_GRIP_RGBA = TOKENS.resize_grip_hover_color
PANE_DIVIDER_GRIP_H = 44.0
PANE_DIVIDER_GRIP_W = 2.0
PORTAL_RESIZE_HANDLE = 22.0
MINIMIZED_ICON_SIZE = 58.0
MINIMIZED_ICON_RADIUS = 17.0
MINIMIZE_BUTTON_SIZE = 22.0
MINIMIZE_BUTTON_X = 10.0
MINIMIZE_BUTTON_Y = 14.0
MINIMIZE_HIT_W = 44.0
MINIMIZE_HIT_H = HEADER_H

SCROLL_INTERACTION_ID = "portal-scroll"
SUBMIT_INTERACTION_ID = "portal-submit"
COMPOSER_INTERACTION_ID = "portal-composer-focus"
PANE_RESIZE_INTERACTION_ID = "portal-pane-resize"
PORTAL_DRAG_INTERACTION_ID = "portal-drag-header"
PORTAL_RESIZE_INTERACTION_ID = "portal-resize-bottom-right"
PORTAL_MINIMIZE_INTERACTION_ID = "portal-minimize"
PORTAL_RESTORE_INTERACTION_ID = "portal-restore"
PORTAL_ICON_DRAG_INTERACTION_ID = "portal-icon-drag"


@dataclass(frozen=True)
class PaneRect:
    x: float
    y: float
    w: float
    h: float


@dataclass(frozen=True)
class PortalTiles:
    capture_backstop: bytes
    frame: bytes
    input_scroll: bytes
    output_scroll: bytes
    drag_shield: bytes
    minimized_icon: bytes
    tab_width: float
    tab_height: float


@dataclass(frozen=True)
class ComposerVisualLine:
    text: str
    positions: tuple[tuple[int, float], ...]


@dataclass(frozen=True)
class ComposerLineWindow:
    display_text: str
    lines: tuple[str, ...]
    start_row: int
    cursor_x: float
    cursor_row: int
    visible_cursor_row: int
    placeholder_style: bool


# ─── CLI defaults ─────────────────────────────────────────────────────────────

DEFAULT_PSK_ENV = "TZE_HUD_PSK"
DEFAULT_TARGET = "windows-host.example:50051"
DEFAULT_DOC = ".claude/skills/user-test/references/exemplar-manual-review-checklist.md"
DEFAULT_TRANSCRIPT_PATH = "test_results/text-stream-portal-latest.json"
DEFAULT_SSH_KEY = os.path.expanduser("~/.ssh/hud-ssh-key")
MAX_MARKDOWN_BYTES = 65535
DRAG_MAX_SECONDS = 12.0
DRAG_IDLE_RELEASE_SECONDS = 1.0
DRAG_APPLY_MIN_INTERVAL_SECONDS = 0.025
ICON_DRAG_APPLY_MIN_INTERVAL_SECONDS = 0.008
ICON_DRAG_START_THRESHOLD_PX = 20.0
# A bare click on the middle pane divider (pointer down+up with no meaningful
# horizontal travel) must NOT commit a resize (hud-z8z7p). The divider only
# becomes a live resize handle once cumulative horizontal movement crosses this
# activation threshold; below it the pointer stream is treated as a no-op click
# and INPUT_PANE_W is left bit-identical. Kept smaller than the icon threshold
# so a deliberate divider drag still feels responsive, but larger than the ~6px
# of jitter a real "click" produced in the field.
PANE_DRAG_START_THRESHOLD_PX = 8.0
COMPOSER_RENDER_DEBOUNCE_SECONDS = 0.02
COMPOSER_CARET_BLINK_SECONDS = 0.45
COMPOSER_CARET_W = 2.0
COMPOSER_CARET_H = INPUT_FONT + 5.0
COMPOSER_WRAP_CHAR_W = INPUT_FONT * 0.57
COMPOSER_CARET_CHAR_W = COMPOSER_WRAP_CHAR_W
COMPOSER_TEXT_RENDER_MARGIN_X = 6.0
COMPOSER_TEXT_RENDER_MARGIN_Y = 6.0
COMPOSER_WRAP_SAFETY_PX = INPUT_FONT * 2.0
COMPOSER_HIT_PAD = 18.0
COMPOSER_VISIBLE_LINE_NODES = 8
COMPOSER_LINE_KEYS = tuple(
    f"line_{index}" for index in range(COMPOSER_VISIBLE_LINE_NODES)
)
COMPOSER_INPUT_CHILD_KEYS = (
    "clear_bg",
    "hit",
    *COMPOSER_LINE_KEYS,
    "caret",
)
COMPOSER_NODE_IDS = {
    "root": uuid.uuid4().bytes,
    "clear_bg": uuid.uuid4().bytes,
    "hit": uuid.uuid4().bytes,
    **{key: uuid.uuid4().bytes for key in COMPOSER_LINE_KEYS},
    "caret": uuid.uuid4().bytes,
}
COMPOSER_RUNTIME_NODE_IDS: dict[str, bytes] = {}
FRAME_RUNTIME_NODE_IDS: dict[str, bytes] = {}
# Runtime-assigned node ids for the output-scroll body tile, captured at mount
# so steady-state republishes can update the body text node IN PLACE (single
# atomic ``update_node_content`` batch) instead of tearing down and rebuilding
# the tile root every cycle. The teardown path (set_tile_root → N×add_node, each
# its own RPC/commit) leaves the body tile transiently empty across several
# render frames, which is observed live as a per-publish flicker (hud-ooeam).
OUTPUT_RUNTIME_NODE_IDS: dict[str, bytes] = {}
PORTAL_STATUS_STATE: dict[str, bool] = {
    "minimized": False,
    "attention": False,
    "pulse": False,
}
FRAME_CHILD_KEYS = [
    # header
    "header_bg", "header_divider", "minimize_bg", "minimize_glyph",
    "minimize_hit", "activity_dot", "title", "subtitle",
    "header_grip", "portal_drag_hit",
    # input pane
    "input_pane_bg", "input_eyebrow",
    "composer_bg", "border_t", "border_b", "border_l", "border_r",
    "submit_hint", "submit_hit",
    # divider
    "pane_divider", "pane_divider_grip", "pane_resize_hit",
    # output pane
    "output_pane_bg", "output_eyebrow", "output_text_window_bg",
    # footer
    "footer_divider", "footer_bg", "footer_node", "resize_handle", "resize_hit",
]


def fresh_composer_node_ids() -> dict[str, bytes]:
    return {
        "root": uuid.uuid4().bytes,
        "clear_bg": uuid.uuid4().bytes,
        "hit": uuid.uuid4().bytes,
        **{key: uuid.uuid4().bytes for key in COMPOSER_LINE_KEYS},
        "caret": uuid.uuid4().bytes,
    }

# ─── Scroll contract tokens ──────────────────────────────────────────────────

SCROLL_TOTAL_LINES = 80
SCROLL_VISIBLE_LINES = 14
SCROLL_STEP_PX = 40.0
SCROLL_LINE_PX = BODY_FONT * 1.4
SCROLL_PHASE_PAUSE_S = 2.5
COMPOSER_LINE_PX = INPUT_FONT * 1.4

# ─── Content helpers ──────────────────────────────────────────────────────────


# Thematic-break marker the compositor renders as a token-styled turn divider
# (portal.divider.color / .thickness_px, hud-nx7yq.4). Matches the runtime's own
# `\n---\n` join in resident_grpc::visible_transcript_markdown so the raw-tile
# pilot surface shows the same dividers the projection-authority path does.
TRANSCRIPT_ENTRY_SEPARATOR = "---"


def split_transcript_entries(body: str) -> list[str]:
    """Split a transcript body into logical entries on blank-line boundaries.

    A logical entry is a run of consecutive non-blank lines (a markdown block);
    one or more blank lines end the current entry. This is the block model the
    OUTPUT pane reads as discrete conversational turns.
    """
    entries: list[str] = []
    current: list[str] = []
    for line in body.splitlines():
        if line.strip() == "":
            if current:
                entries.append("\n".join(current))
                current = []
        else:
            current.append(line)
    if current:
        entries.append("\n".join(current))
    return entries


def join_transcript_entries(entries: list[str]) -> str:
    """Join logical transcript entries with thematic-break separators.

    Emits `entry\\n---\\nentry` so the compositor draws a token-styled divider on
    the `---` line between adjacent entries (hud-hsc1t / §Transcript Turn
    Separators). The exemplar owns its OUTPUT-pane transcript and previously
    joined blocks with plain newlines, so no dividers rendered on the pilot
    surface at all ("no dividers between history entries"). N entries yield N-1
    separators; empty/whitespace-only entries are dropped so no leading/trailing
    or doubled divider appears.
    """
    kept = [entry for entry in entries if entry.strip()]
    return f"\n{TRANSCRIPT_ENTRY_SEPARATOR}\n".join(kept)


def append_input_history(input_history: list[str], entry: str) -> Optional[str]:
    """Record a viewer's submitted composer entry into the INPUT-pane history.

    Two-pane portal contract (hud-egf39): the viewer's own submissions belong to
    the LEFT input pane — the runtime renders them as viewer-echo turns beneath
    the composer, with `---` dividers between adjacent entries (hud-hsc1t /
    #1020). They MUST NOT be folded into the OUTPUT-pane transcript, which stays
    agent-authored only. This supersedes the combined-transcript echo shipped in
    #1027/#1031 (`append_transcript_entry` into `body_full`).

    The entry is line-ending-normalized before recording. Whitespace-only
    entries are dropped so a bare Enter creates no empty history turn.

    Returns the normalized entry that was appended, or ``None`` if it was
    dropped (empty/whitespace-only).
    """
    normalized = normalize_composer_input(entry)
    if not normalized.strip():
        return None
    input_history.append(normalized)
    return normalized


def load_transcript_slice(doc_path: str, max_lines: int) -> str:
    """Load the markdown file, trim to a bounded viewport, and insert a
    thematic-break turn divider between each logical entry so the OUTPUT pane
    renders discrete conversational turns (hud-hsc1t)."""
    raw = Path(doc_path).read_text(encoding="utf-8")
    lines = raw.splitlines()
    body = "\n".join(lines[:max_lines])
    return join_transcript_entries(split_transcript_entries(body))


def normalize_composer_input(text: str) -> str:
    return text.replace("\r\n", "\n").replace("\r", "\n")


def composer_key_fallback_text(key: str) -> Optional[str]:
    """Return printable text for key-downs that do not emit Character events."""
    if key == "Space":
        return " "
    return None


def composer_wrap_char_width_px(ch: str) -> float:
    """Approximate SystemMonospace advance for visual caret placement."""
    if ch == "\t":
        return COMPOSER_WRAP_CHAR_W * 4.0
    return COMPOSER_WRAP_CHAR_W


def composer_caret_char_width_px(ch: str) -> float:
    if ch == "\t":
        return COMPOSER_CARET_CHAR_W * 4.0
    return COMPOSER_CARET_CHAR_W


def composer_wrap_text_width_px(text: str) -> float:
    return sum(composer_wrap_char_width_px(ch) for ch in text)


def composer_visual_lines(
    text: str,
    max_width_px: float,
) -> list[ComposerVisualLine]:
    lines: list[ComposerVisualLine] = []
    line_chars: list[str] = []
    positions: list[tuple[int, float]] = [(0, 0.0)]
    wrap_line_width = 0.0
    caret_line_width = 0.0

    def add_position(index: int) -> None:
        if positions and positions[-1][0] == index:
            return
        positions.append((index, caret_line_width))

    def finish_line() -> None:
        lines.append(ComposerVisualLine("".join(line_chars), tuple(positions)))

    def start_line(index: int, zero_positions: tuple[int, ...] = ()) -> None:
        nonlocal line_chars, positions, wrap_line_width, caret_line_width
        finish_line()
        line_chars = []
        wrap_line_width = 0.0
        caret_line_width = 0.0
        positions = [(index, 0.0)]
        for position in zero_positions:
            add_position(position)

    def append_char(index: int, ch: str) -> None:
        nonlocal wrap_line_width, caret_line_width
        line_chars.append(ch)
        wrap_line_width += composer_wrap_char_width_px(ch)
        caret_line_width += composer_caret_char_width_px(ch)
        add_position(index + 1)

    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\n":
            add_position(i)
            start_line(i + 1)
            i += 1
            continue

        if ch.isspace():
            run_end = i + 1
            while run_end < len(text) and text[run_end].isspace() and text[run_end] != "\n":
                run_end += 1
            next_word_end = run_end
            while next_word_end < len(text) and not text[next_word_end].isspace():
                next_word_end += 1

            whitespace_width = composer_wrap_text_width_px(text[i:run_end])
            next_word_width = composer_wrap_text_width_px(text[run_end:next_word_end])
            if (
                wrap_line_width > 0.0
                and run_end < len(text)
                and wrap_line_width + whitespace_width + next_word_width > max_width_px
            ):
                add_position(i)
                start_line(i + 1, tuple(range(i + 2, run_end + 1)))
                i = run_end
                continue

            while i < run_end:
                ch = text[i]
                wrap_ch_width = composer_wrap_char_width_px(ch)
                if wrap_line_width > 0.0 and wrap_line_width + wrap_ch_width > max_width_px:
                    start_line(i + 1)
                    i += 1
                    continue
                append_char(i, ch)
                i += 1
            continue

        word_end = i + 1
        while word_end < len(text) and not text[word_end].isspace():
            word_end += 1
        word = text[i:word_end]
        word_width = composer_wrap_text_width_px(word)
        if wrap_line_width > 0.0 and wrap_line_width + word_width > max_width_px:
            start_line(i)

        while i < word_end:
            ch = text[i]
            wrap_ch_width = composer_wrap_char_width_px(ch)
            if wrap_line_width > 0.0 and wrap_line_width + wrap_ch_width > max_width_px:
                start_line(i)
            append_char(i, ch)
            i += 1

    add_position(len(text))
    finish_line()
    return lines


def composer_wrapped_layout(
    text: str,
    cursor: int,
    max_width_px: float,
) -> tuple[str, float, int]:
    """Return explicit visual wrapping plus caret x/row for the raw cursor."""
    cursor = max(0, min(cursor, len(text)))
    lines = composer_visual_lines(text, max_width_px)
    cursor_x = 0.0
    cursor_row = 0
    for row, line in enumerate(lines):
        for index, x in line.positions:
            if index == cursor:
                cursor_x = x
                cursor_row = row
                break
        else:
            continue
        break
    else:
        cursor_row = len(lines) - 1
        cursor_x = lines[-1].positions[-1][1]

    return "\n".join(line.text for line in lines), cursor_x, cursor_row


def composer_text_area_width_px() -> float:
    composer_rect = input_composer_local_rect()
    text_inset = 12.0
    return max(
        0.0,
        composer_rect.w
        - text_inset * 2.0
        - COMPOSER_TEXT_RENDER_MARGIN_X * 2.0
        - COMPOSER_CARET_W,
    )


def composer_wrap_area_width_px() -> float:
    # The compositor still applies glyphon word wrap to the pre-wrapped string.
    # Keep our explicit lines narrower so glyphon does not add an extra row.
    return max(0.0, composer_text_area_width_px() - COMPOSER_WRAP_SAFETY_PX)


def composer_display_text(
    text: str,
    cursor: int,
    *,
    focused: bool,
) -> tuple[str, bool]:
    """Render composer text and report whether it should use placeholder styling."""
    cursor = max(0, min(cursor, len(text)))
    if not text and not focused:
        return "", True
    display_text, _, _ = composer_wrapped_layout(
        text,
        cursor,
        composer_wrap_area_width_px(),
    )
    return display_text, False


def composer_line_window(
    text: str,
    cursor: int,
    *,
    focused: bool,
    composer_placeholder: str = "type a reply — Enter to submit",
) -> ComposerLineWindow:
    """Return the fixed visible line window used by composer text nodes."""
    cursor = max(0, min(cursor, len(text)))
    display_text, placeholder_style = composer_display_text(
        text,
        cursor,
        focused=focused,
    )
    if placeholder_style:
        raw_lines = [composer_placeholder]
        cursor_x = 0.0
        cursor_row = 0
    else:
        display_text, cursor_x, cursor_row = composer_wrapped_layout(
            text,
            cursor,
            composer_wrap_area_width_px(),
        )
        raw_lines = display_text.split("\n")

    if not raw_lines:
        raw_lines = [""]

    if len(raw_lines) <= COMPOSER_VISIBLE_LINE_NODES:
        start_row = 0
    else:
        start_row = min(
            max(0, cursor_row - COMPOSER_VISIBLE_LINE_NODES + 1),
            len(raw_lines) - COMPOSER_VISIBLE_LINE_NODES,
        )
    visible_lines = raw_lines[start_row:start_row + COMPOSER_VISIBLE_LINE_NODES]
    if len(visible_lines) < COMPOSER_VISIBLE_LINE_NODES:
        visible_lines.extend([""] * (COMPOSER_VISIBLE_LINE_NODES - len(visible_lines)))
    visible_cursor_row = max(
        0,
        min(cursor_row - start_row, COMPOSER_VISIBLE_LINE_NODES - 1),
    )
    return ComposerLineWindow(
        display_text=display_text,
        lines=tuple(visible_lines),
        start_row=start_row,
        cursor_x=cursor_x,
        cursor_row=cursor_row,
        visible_cursor_row=visible_cursor_row,
        placeholder_style=placeholder_style,
    )


def composer_caret_layout(text: str, cursor: int) -> tuple[float, int]:
    _, cursor_x, cursor_row = composer_wrapped_layout(
        text,
        cursor,
        composer_wrap_area_width_px(),
    )
    return cursor_x, cursor_row


def composer_cursor_for_vertical_move(
    text: str,
    cursor: int,
    delta_rows: int,
    preferred_x: Optional[float],
) -> tuple[int, float]:
    """Move the raw cursor vertically through the explicit visual line model."""
    lines = composer_visual_lines(text, composer_wrap_area_width_px())
    _, current_x, current_row = composer_wrapped_layout(
        text,
        cursor,
        composer_wrap_area_width_px(),
    )
    target_x = current_x if preferred_x is None else preferred_x
    target_row = max(0, min(current_row + delta_rows, len(lines) - 1))
    if target_row == current_row:
        return cursor, target_x
    target_positions = lines[target_row].positions or ((0, 0.0),)
    target_cursor, _ = min(
        target_positions,
        key=lambda position: (abs(position[1] - target_x), position[0]),
    )
    return target_cursor, target_x


def composer_word_delete_start(text: str, cursor: int) -> int:
    """Return the cursor position after deleting the previous word cluster."""
    cursor = max(0, min(cursor, len(text)))
    if cursor == 0:
        return 0
    i = cursor
    while i > 0 and not (text[i - 1].isalnum() or text[i - 1] == "_"):
        i -= 1
    while i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_"):
        i -= 1
    return i


def composer_word_forward_end(text: str, cursor: int) -> int:
    """Return the cursor position after advancing over the next word cluster."""
    cursor = max(0, min(cursor, len(text)))
    i = cursor
    while i < len(text) and not (text[i].isalnum() or text[i] == "_"):
        i += 1
    while i < len(text) and (text[i].isalnum() or text[i] == "_"):
        i += 1
    return i


def resolved_portal_max_size(
    tab_width: float,
    tab_height: float,
    *,
    lease_max_width: Optional[float] = None,
    lease_max_height: Optional[float] = None,
) -> tuple[float, float]:
    """Resolve portal maxima from runtime budgets exposed to this harness.

    The current resident SceneSnapshot path exposes display_area but not a
    portal-specific lease maximum, so display bounds are the interim harness
    limit. Keep the lease parameters explicit so the live adapter can honor that
    signal as soon as the runtime surfaces it.
    """
    max_w = max(0.0, tab_width)
    max_h = max(0.0, tab_height)
    if lease_max_width is not None:
        max_w = min(max_w, max(0.0, lease_max_width))
    if lease_max_height is not None:
        max_h = min(max_h, max(0.0, lease_max_height))
    return max_w, max_h


def clamp_portal_size(
    w: float,
    h: float,
    tab_width: float,
    tab_height: float,
    *,
    lease_max_width: Optional[float] = None,
    lease_max_height: Optional[float] = None,
) -> tuple[float, float]:
    max_w, max_h = resolved_portal_max_size(
        tab_width,
        tab_height,
        lease_max_width=lease_max_width,
        lease_max_height=lease_max_height,
    )
    min_w = min(PORTAL_MIN_W, max_w)
    min_h = min(PORTAL_MIN_H, max_h)
    return (
        min(max(w, min_w), max_w),
        min(max(h, min_h), max_h),
    )


def default_portal_size(tab_width: float, tab_height: float) -> tuple[float, float]:
    return clamp_portal_size(
        tab_width * PORTAL_DEFAULT_WIDTH_PCT,
        tab_height * PORTAL_DEFAULT_HEIGHT_PCT,
        tab_width,
        tab_height,
    )


def profile_swap_dimensions(
    standard_w: float,
    standard_h: float,
) -> list[tuple[str, float, float, float, float]]:
    expanded_w = max(1100.0, standard_w * 1.15)
    expanded_h = max(820.0, standard_h * 1.15)
    return [
        # (name, portal_w, portal_h, title_font, body_font)
        ("compact", 680.0, 520.0, 16.0, 14.0),
        ("standard", standard_w, standard_h, TITLE_FONT, BODY_FONT),
        ("expanded", expanded_w, expanded_h, 20.0, 18.0),
        ("standard", standard_w, standard_h, TITLE_FONT, BODY_FONT),
    ]


# Per-profile color accents for the profile-swap phase. Each named profile is a
# real set of portal.* token overrides (a component profile). Applying it via
# `apply_visual_profile` re-resolves TOKENS and reskins every published value —
# so the swap changes the live palette, not just the frame dimensions. The
# "standard" profile is the exemplar's own look (no accent). This is the live,
# end-to-end proof that the exemplar is token-driven, not literal.
PROFILE_SWAP_COLOR_ACCENTS: dict[str, dict[str, str]] = {
    "compact": {
        ppt.PORTAL_TOKEN_FRAME_BACKGROUND: "#101826D9",     # cool slate glass
        ppt.PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR: "#DCE6F5",
        ppt.PORTAL_TOKEN_HEADER_TEXT_COLOR: "#EAF1FF",
    },
    "standard": {},
    "expanded": {
        ppt.PORTAL_TOKEN_FRAME_BACKGROUND: "#1A1206D9",     # warm amber glass
        ppt.PORTAL_TOKEN_TRANSCRIPT_TEXT_COLOR: "#F5ECD9",
        ppt.PORTAL_TOKEN_HEADER_TEXT_COLOR: "#FFF3DE",
    },
}


def profile_swap_overrides(name: str, title_font: float, body_font: float) -> dict[str, str]:
    """Build the full portal-token override map for one profile-swap step.

    Starts from the exemplar profile, applies the profile's font sizes (so
    header/composer/transcript typography track the swap), then layers the
    named color accent. Pure so the reskin assertion can exercise it directly.
    """
    overrides = dict(EXEMPLAR_PROFILE_OVERRIDES)
    overrides[ppt.PORTAL_TOKEN_HEADER_FONT_SIZE] = _fmt_token_num(title_font)
    overrides[ppt.PORTAL_TOKEN_TRANSCRIPT_FONT_SIZE] = _fmt_token_num(body_font)
    overrides[ppt.PORTAL_TOKEN_COMPOSER_FONT_SIZE] = _fmt_token_num(body_font)
    overrides.update(PROFILE_SWAP_COLOR_ACCENTS.get(name, {}))
    return overrides


def _fmt_token_num(value: float) -> str:
    """Format a numeric token value the way Rust's parser accepts (no trailing
    ``.0`` noise on integers)."""
    return str(int(value)) if float(value).is_integer() else repr(float(value))


def clamp_input_pane_width(width: float) -> float:
    max_input_w = max(MIN_PANE_W, PORTAL_W - PANE_DIVIDER_W - MIN_PANE_W)
    return max(MIN_PANE_W, min(width, max_input_w))


def set_input_pane_width(width: float) -> None:
    global INPUT_PANE_W
    INPUT_PANE_W = clamp_input_pane_width(width)


def partition_pane_widths(portal_w: float, input_pane_w: float) -> tuple[float, float]:
    """Split a portal frame into (input_pane_w, output_pane_w) around the divider.

    Pure helper so the invariant is testable in isolation: the two pane widths
    plus the fat divider account for the whole frame exactly, i.e.

        input_pane_w + PANE_DIVIDER_W + output_pane_w == portal_w

    with no pixels left unaccounted (hud-z8z7p). Note this returns the *pane*
    widths, not the inner text-body width — the output body is further inset by
    PADDING_X on each side, which is a rendering inset, not lost frame width.
    """
    return input_pane_w, portal_w - input_pane_w - PANE_DIVIDER_W


def output_pane_width() -> float:
    """Live output-pane width honouring the partition invariant above."""
    return partition_pane_widths(PORTAL_W, INPUT_PANE_W)[1]


def pane_drag_crosses_threshold(dx: float) -> bool:
    """Whether a pane-divider pointer stream has moved far enough to be a resize.

    A bare click (|dx| below the activation threshold) returns False, so the
    caller commits no width change and INPUT_PANE_W stays bit-identical.
    """
    return abs(dx) >= PANE_DRAG_START_THRESHOLD_PX


def set_portal_size(w: float, h: float, tab_width: float, tab_height: float) -> None:
    global PORTAL_W, PORTAL_H
    old_w = PORTAL_W
    PORTAL_W, PORTAL_H = clamp_portal_size(w, h, tab_width, tab_height)
    if old_w > 0:
        set_input_pane_width(INPUT_PANE_W * (PORTAL_W / old_w))
    else:
        set_input_pane_width((PORTAL_W - PANE_DIVIDER_W) * GEOMETRY.pane_split_ratio)


def target_host(target: str) -> str:
    return target.rsplit(":", 1)[0] if ":" in target else target


async def read_windows_clipboard(
    host: str,
    *,
    user: str,
    ssh_key: str,
    timeout_s: float,
) -> str:
    if not host:
        return ""
    cmd = [
        "ssh",
        "-i", ssh_key,
        "-o", "BatchMode=yes",
        "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=no",
        f"{user}@{host}",
        "powershell -NoProfile -Command \"Get-Clipboard -Raw\"",
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout_s)
    except (OSError, asyncio.TimeoutError, subprocess.SubprocessError):
        return ""
    if proc.returncode != 0:
        return ""
    return stdout.decode("utf-8", errors="replace").replace("\r\n", "\n").rstrip("\n")


async def read_windows_left_button_down(
    host: str,
    *,
    user: str,
    ssh_key: str,
    timeout_s: float,
) -> Optional[bool]:
    if not host:
        return None
    ps = (
        "Add-Type -Namespace Win32 -Name User32 -MemberDefinition "
        "'[DllImport(\"user32.dll\")] public static extern short GetAsyncKeyState(int vKey);'; "
        "$s=[Win32.User32]::GetAsyncKeyState(1); "
        "if (($s -band -32768) -ne 0) { 'down' } else { 'up' }"
    )
    cmd = [
        "ssh",
        "-i", ssh_key,
        "-o", "BatchMode=yes",
        "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=no",
        f"{user}@{host}",
        f"powershell -NoProfile -Command \"{ps}\"",
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout_s)
    except (OSError, asyncio.TimeoutError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    value = stdout.decode("utf-8", errors="replace").strip().lower()
    if value == "down":
        return True
    if value == "up":
        return False
    return None


def ps_single_quoted(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def build_diagnostic_input_plan(
    portal_x: float,
    portal_y: float,
    *,
    tab_width: Optional[float] = None,
    tab_height: Optional[float] = None,
) -> list[dict[str, Any]]:
    """Build an OS-input plan covering composer focus, drag, and output scroll."""
    input_rect, output_rect = portal_pane_rects()
    composer_x = portal_x + input_rect.x + input_rect.w / 2.0
    composer_y = portal_y + input_rect.y + min(input_rect.h - 10.0, 72.0)
    header_x = portal_x + PORTAL_W / 2.0
    header_y = portal_y + HEADER_H / 2.0
    target_portal_x = portal_x - 120.0
    target_portal_y = portal_y + 72.0
    if tab_width is not None:
        target_portal_x = max(0.0, min(target_portal_x, max(0.0, tab_width - PORTAL_W)))
    if tab_height is not None:
        target_portal_y = max(0.0, min(target_portal_y, max(0.0, tab_height - PORTAL_H)))
    drag_dx = target_portal_x - portal_x
    drag_dy = target_portal_y - portal_y
    output_x = target_portal_x + output_rect.x + output_rect.w / 2.0
    output_y = target_portal_y + output_rect.y + min(output_rect.h - 10.0, 96.0)
    return [
        {
            "kind": "click",
            "label": "focus-composer",
            "x": composer_x,
            "y": composer_y,
        },
        {
            "kind": "drag",
            "label": "drag-portal-header",
            "start_x": header_x,
            "start_y": header_y,
            "end_x": header_x + drag_dx,
            "end_y": header_y + drag_dy,
            "steps": 8,
        },
        {
            "kind": "wheel",
            "label": "scroll-output-pane",
            "x": output_x,
            "y": output_y,
            "delta": -360,
            "count": 3,
        },
        {
            "kind": "text",
            "label": "type-composer-text",
            "text": "diagnostic input",
        },
    ]


def build_resize_hotkey_plan(
    portal_x: float,
    portal_y: float,
    shot_prefix: str,
    *,
    grow_steps: int = 6,
    shrink_steps: int = 12,
) -> list[dict[str, Any]]:
    """Build an OS-input plan proving focus-scoped Ctrl +/- portal resize.

    Focuses the portal composer (composer focus == portal-surface focus, so the
    resize dispatch's focus gate is satisfied), screenshots the baseline, grows
    via Ctrl+Equal, screenshots, then shrinks past baseline via Ctrl+Minus and
    screenshots. The before/after PNGs are the operator-visible proof for
    hud-v4k1h (Ctrl resize hotkeys had no visible effect before PR #937's
    physical-KeyCode resolution).
    """
    _, output_rect = portal_pane_rects()
    # Focus the OUTPUT/transcript pane: the resize hotkey only fires when the
    # focused tile carries a scroll config (runtime gate in
    # apply_portal_resize_hotkey: tile_scroll_config(focused).is_some()), and
    # in the raw-tile pilot the scroll config lives on the output body tile.
    focus_x = portal_x + output_rect.x + output_rect.w / 2.0
    focus_y = portal_y + output_rect.y + output_rect.h / 2.0
    return [
        {"kind": "click", "label": "focus-output-pane", "x": focus_x, "y": focus_y},
        {"kind": "screenshot", "label": "resize-baseline", "path": f"{shot_prefix}-baseline.png"},
        {"kind": "chord", "label": "resize-grow-ctrl-equal", "mod_vk": 0x11, "key_vk": 0xBB, "count": grow_steps},
        {"kind": "screenshot", "label": "resize-grow", "path": f"{shot_prefix}-grow.png"},
        {"kind": "chord", "label": "resize-shrink-ctrl-minus", "mod_vk": 0x11, "key_vk": 0xBD, "count": shrink_steps},
        {"kind": "screenshot", "label": "resize-shrink", "path": f"{shot_prefix}-shrink.png"},
    ]


def windows_diagnostic_input_script(
    actions: list[dict[str, Any]],
    *,
    scene_width: Optional[float] = None,
    scene_height: Optional[float] = None,
) -> str:
    """Return a PowerShell script that injects real Windows OS input events."""
    scene_width_value = float(scene_width or 0.0)
    scene_height_value = float(scene_height or 0.0)
    lines = [
        "$ErrorActionPreference = 'Stop'",
        # The interactive scheduled task owns a visible PowerShell console. Hide
        # it before injecting input so it cannot become the foreground recipient
        # instead of the transparent HUD overlay.
        "Add-Type -Name Win -Namespace Native -MemberDefinition "
        "'[DllImport(\"kernel32.dll\")] public static extern IntPtr GetConsoleWindow(); "
        "[DllImport(\"user32.dll\")] public static extern bool ShowWindow(IntPtr h, int n);'",
        "$hudConsole = [Native.Win]::GetConsoleWindow()",
        "[void][Native.Win]::ShowWindow($hudConsole, 0)",
        "Start-Sleep -Milliseconds 400",
        "Add-Type -AssemblyName System.Windows.Forms",
        "Add-Type -TypeDefinition @\"",
        "using System;",
        "using System.Runtime.InteropServices;",
        "public static class HudDiagnosticInput {",
        "  [DllImport(\"user32.dll\")] public static extern bool SetCursorPos(int X, int Y);",
        "  [DllImport(\"user32.dll\")] public static extern void mouse_event(uint flags, uint dx, uint dy, int data, UIntPtr extra);",
        "  [DllImport(\"user32.dll\", SetLastError=true)] public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);",
        "  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION U; }",
        "  [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public HARDWAREINPUT hi; }",
        "  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public UIntPtr dwExtraInfo; }",
        "  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public UIntPtr dwExtraInfo; }",
        "  [StructLayout(LayoutKind.Sequential)] public struct HARDWAREINPUT { public uint uMsg; public ushort wParamL; public ushort wParamH; }",
        "}",
        "\"@",
        "$MOUSEEVENTF_LEFTDOWN = 0x0002",
        "$MOUSEEVENTF_LEFTUP = 0x0004",
        "$MOUSEEVENTF_WHEEL = 0x0800",
        "$INPUT_KEYBOARD = 1",
        "$KEYEVENTF_UNICODE = 0x0004",
        "$KEYEVENTF_KEYUP = 0x0002",
        "$InputSize = [System.Runtime.InteropServices.Marshal]::SizeOf([type][HudDiagnosticInput+INPUT])",
        f"$HudDiagnosticSceneWidth = {scene_width_value:.1f}",
        f"$HudDiagnosticSceneHeight = {scene_height_value:.1f}",
        "$HudDiagnosticBounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds",
        "$HudDiagnosticScaleX = 1.0",
        "$HudDiagnosticScaleY = 1.0",
        "if ($HudDiagnosticSceneWidth -gt 0 -and $HudDiagnosticBounds.Width -gt 0) {",
        "  $HudDiagnosticScaleX = [double]$HudDiagnosticBounds.Width / [double]$HudDiagnosticSceneWidth",
        "}",
        "if ($HudDiagnosticSceneHeight -gt 0 -and $HudDiagnosticBounds.Height -gt 0) {",
        "  $HudDiagnosticScaleY = [double]$HudDiagnosticBounds.Height / [double]$HudDiagnosticSceneHeight",
        "}",
        "function Move-To([double]$x, [double]$y) {",
        "  $targetX = $x * $HudDiagnosticScaleX",
        "  $targetY = $y * $HudDiagnosticScaleY",
        "  if (-not [HudDiagnosticInput]::SetCursorPos([int][Math]::Round($targetX), [int][Math]::Round($targetY))) { throw 'SetCursorPos failed' }",
        "  Start-Sleep -Milliseconds 80",
        "}",
        "function Left-Down() { [HudDiagnosticInput]::mouse_event($MOUSEEVENTF_LEFTDOWN, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 80 }",
        "function Left-Up() { [HudDiagnosticInput]::mouse_event($MOUSEEVENTF_LEFTUP, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 120 }",
        "function Wheel-At([double]$x, [double]$y, [int]$delta, [int]$count) {",
        "  Move-To $x $y",
        "  for ($i = 0; $i -lt $count; $i++) {",
        "    [HudDiagnosticInput]::mouse_event($MOUSEEVENTF_WHEEL, 0, 0, $delta, [UIntPtr]::Zero)",
        "    Start-Sleep -Milliseconds 140",
        "  }",
        "}",
        "function Send-Text([string]$text) {",
        "  $inputs = [HudDiagnosticInput+INPUT[]]::new(2)",
        "  foreach ($ch in $text.ToCharArray()) {",
        "    $scan = [uint16][char]$ch",
        "    $inputs[0].type = $INPUT_KEYBOARD",
        "    $inputs[0].U.ki.wVk = 0",
        "    $inputs[0].U.ki.wScan = $scan",
        "    $inputs[0].U.ki.dwFlags = $KEYEVENTF_UNICODE",
        "    $inputs[1].type = $INPUT_KEYBOARD",
        "    $inputs[1].U.ki.wVk = 0",
        "    $inputs[1].U.ki.wScan = $scan",
        "    $inputs[1].U.ki.dwFlags = $KEYEVENTF_UNICODE -bor $KEYEVENTF_KEYUP",
        "    $sent = [HudDiagnosticInput]::SendInput(2, $inputs, $InputSize)",
        "    if ($sent -ne 2) {",
        "      $lastError = [System.Runtime.InteropServices.Marshal]::GetLastWin32Error()",
        "      Write-Output ('diagnostic-warning:SendInput failed sent=' + $sent + ' last_error=' + $lastError + ' input_size=' + $InputSize)",
        "      return",
        "    }",
        "    Start-Sleep -Milliseconds 25",
        "  }",
        "}",
        # Virtual-key chord (e.g. Ctrl+Equal / Ctrl+Minus) for focus-scoped
        # portal resize hotkeys. Uses wVk (not Unicode scan) so the runtime sees
        # the physical KeyCode the resize dispatch resolves (Equal/Minus/numpad).
        "$VK_CONTROL = 0x11",
        "$VK_OEM_PLUS = 0xBB",   # '=' / '+' physical key (KeyCode 'Equal')
        "$VK_OEM_MINUS = 0xBD",  # '-' physical key (KeyCode 'Minus')
        "function Send-VkChord([uint16]$mod, [uint16]$key) {",
        "  $inputs = [HudDiagnosticInput+INPUT[]]::new(4)",
        "  $inputs[0].type = $INPUT_KEYBOARD; $inputs[0].U.ki.wVk = $mod; $inputs[0].U.ki.dwFlags = 0",
        "  $inputs[1].type = $INPUT_KEYBOARD; $inputs[1].U.ki.wVk = $key; $inputs[1].U.ki.dwFlags = 0",
        "  $inputs[2].type = $INPUT_KEYBOARD; $inputs[2].U.ki.wVk = $key; $inputs[2].U.ki.dwFlags = $KEYEVENTF_KEYUP",
        "  $inputs[3].type = $INPUT_KEYBOARD; $inputs[3].U.ki.wVk = $mod; $inputs[3].U.ki.dwFlags = $KEYEVENTF_KEYUP",
        "  $sent = [HudDiagnosticInput]::SendInput(4, $inputs, $InputSize)",
        "  if ($sent -ne 4) { Write-Output ('diagnostic-warning:chord SendInput sent=' + $sent) }",
        "  Start-Sleep -Milliseconds 180",
        "}",
        # Capture the composited primary screen (overlay over desktop) to a PNG.
        # Runs in the interactive session so the layered overlay is included.
        "function Capture-Screen([string]$path) {",
        "  Add-Type -AssemblyName System.Drawing",
        "  $b = $HudDiagnosticBounds",
        "  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height",
        "  $g = [System.Drawing.Graphics]::FromImage($bmp)",
        "  $g.CopyFromScreen($b.X, $b.Y, 0, 0, $bmp.Size)",
        "  $dir = Split-Path -Parent $path",
        "  if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }",
        "  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)",
        "  $g.Dispose(); $bmp.Dispose()",
        "  Start-Sleep -Milliseconds 120",
        "}",
    ]
    for action in actions:
        kind = str(action.get("kind", ""))
        if kind == "click":
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:' + str(action.get('label', 'click')))}",
                f"Move-To {float(action['x']):.1f} {float(action['y']):.1f}",
                "Left-Down",
                "Left-Up",
            ])
        elif kind == "drag":
            steps = max(1, int(action.get("steps", 8)))
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:' + str(action.get('label', 'drag')))}",
                f"$sx = {float(action['start_x']):.1f}; $sy = {float(action['start_y']):.1f}",
                f"$ex = {float(action['end_x']):.1f}; $ey = {float(action['end_y']):.1f}",
                f"$steps = {steps}",
                "Move-To $sx $sy",
                "Left-Down",
                "for ($i = 1; $i -le $steps; $i++) {",
                "  $t = [double]$i / [double]$steps",
                "  Move-To ($sx + (($ex - $sx) * $t)) ($sy + (($ey - $sy) * $t))",
                "}",
                "Left-Up",
            ])
        elif kind == "wheel":
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:' + str(action.get('label', 'wheel')))}",
                (
                    f"Wheel-At {float(action['x']):.1f} {float(action['y']):.1f} "
                    f"{int(action.get('delta', -120))} {max(1, int(action.get('count', 1)))}"
                ),
            ])
        elif kind == "text":
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:' + str(action.get('label', 'text')))}",
                f"Send-Text {ps_single_quoted(str(action.get('text', '')))}",
                "Start-Sleep -Milliseconds 120",
            ])
        elif kind == "chord":
            mod_vk = int(action.get("mod_vk", 0x11))
            key_vk = int(action["key_vk"])
            count = max(1, int(action.get("count", 1)))
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:' + str(action.get('label', 'chord')))}",
                f"for ($i = 0; $i -lt {count}; $i++) {{ Send-VkChord {mod_vk} {key_vk} }}",
            ])
        elif kind == "screenshot":
            path = str(action["path"])
            lines.extend([
                f"Write-Output {ps_single_quoted('diagnostic:screenshot:' + str(action.get('label', 'shot')) + ':' + path)}",
                f"Capture-Screen {ps_single_quoted(path)}",
            ])
        else:
            raise ValueError(f"unsupported diagnostic action kind: {kind!r}")
    return "\n".join(lines) + "\n"


def windows_diagnostic_task_script(
    diagnostic_script: str,
    *,
    user: str,
    timeout_s: float,
    run_id: str,
) -> str:
    """Wrap the OS input script in an interactive scheduled task launcher."""
    task_name = f"TzeHudDiagnosticInput_{run_id}"
    script_path = (
        f"C:\\tze_hud\\text_stream_portal_diagnostic_input_{run_id}.ps1"
    )
    result_path = (
        f"C:\\tze_hud\\text_stream_portal_diagnostic_input_result_{run_id}.json"
    )
    timeout = max(1, int(round(timeout_s)))
    wrapper = f"""$ErrorActionPreference = 'Stop'
$stdoutLines = New-Object 'System.Collections.Generic.List[string]'
$stderrLines = New-Object 'System.Collections.Generic.List[string]'
$ok = $true
$returnCode = 0
try {{
  $output = & {{
{diagnostic_script}
  }} 2>&1
  foreach ($item in $output) {{
    if ($item -is [System.Management.Automation.ErrorRecord]) {{
      $stderrLines.Add($item.ToString())
    }} else {{
      $stdoutLines.Add([string]$item)
    }}
  }}
}} catch {{
  $ok = $false
  $returnCode = 1
  $stderrLines.Add($_.Exception.Message)
  $stderrLines.Add($_.ScriptStackTrace)
}}
$result = [ordered]@{{
  ok = $ok
  returncode = $returnCode
  stdout = ($stdoutLines -join "`n")
  stderr = (($stderrLines | Where-Object {{ $_ }}) -join "`n")
}}
$result | ConvertTo-Json -Compress -Depth 4 | Set-Content -Encoding UTF8 -Path '{result_path}'
"""
    wrapper_base64 = base64.b64encode(wrapper.encode("utf-8")).decode("ascii")
    wrapper_chunks = [
        ps_single_quoted(wrapper_base64[i:i + 240])
        for i in range(0, len(wrapper_base64), 240)
    ]
    wrapper_expr = " + ".join(wrapper_chunks)
    task_body = [
        "Remove-Item -Force $resultPath -ErrorAction SilentlyContinue",
        f"$wrapperBase64 = {wrapper_expr}",
        (
            "[System.IO.File]::WriteAllBytes("
            "$scriptPath, [Convert]::FromBase64String($wrapperBase64))"
        ),
        (
            "$action = New-ScheduledTaskAction -Execute 'powershell.exe' "
            "-Argument ('-NoProfile -ExecutionPolicy Bypass -File \"' + "
            "$scriptPath + '\"')"
        ),
        (
            f"$principal = New-ScheduledTaskPrincipal -UserId "
            f"{ps_single_quoted(user)} -LogonType Interactive -RunLevel Highest"
        ),
        (
            "$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries "
            "-DontStopIfGoingOnBatteries"
        ),
        (
            "Register-ScheduledTask -TaskName $taskName -Action $action "
            "-Principal $principal -Settings $settings -Force | Out-Null"
        ),
        "Start-ScheduledTask -TaskName $taskName",
        "$completed = $false",
        f"$deadline = (Get-Date).AddSeconds({timeout})",
        (
            "while ((Get-Date) -lt $deadline) { "
            "if (Test-Path $resultPath) { "
            "$payload = Get-Content -Raw -Path $resultPath; "
            "$completed = $true; Write-Output $payload; break }; "
            "Start-Sleep -Milliseconds 200 }"
        ),
        (
            "if (-not $completed) { throw "
            "('scheduled diagnostic task timed out waiting for ' + $resultPath) }"
        ),
    ]
    cleanup_body = [
        (
            "Stop-ScheduledTask -TaskName $taskName "
            "-ErrorAction SilentlyContinue"
        ),
        (
            "Unregister-ScheduledTask -TaskName $taskName -Confirm:$false "
            "-ErrorAction SilentlyContinue"
        ),
        "Remove-Item -Force $scriptPath,$resultPath -ErrorAction SilentlyContinue",
    ]
    statements = [
        "$ErrorActionPreference = 'Stop'",
        f"$taskName = {ps_single_quoted(task_name)}",
        f"$scriptPath = {ps_single_quoted(script_path)}",
        f"$resultPath = {ps_single_quoted(result_path)}",
        "try { " + "; ".join(task_body) + " } finally { "
        + "; ".join(cleanup_body) + " }",
    ]
    return "; ".join(statements) + "\n"


async def run_windows_diagnostic_input(
    host: str,
    *,
    user: str,
    ssh_key: str,
    actions: list[dict[str, Any]],
    timeout_s: float,
    connect_timeout_s: float = 5.0,
    scene_width: Optional[float] = None,
    scene_height: Optional[float] = None,
) -> dict[str, Any]:
    script = windows_diagnostic_input_script(
        actions,
        scene_width=scene_width,
        scene_height=scene_height,
    )
    run_id = uuid.uuid4().hex[:8]
    task_script = windows_diagnostic_task_script(
        script,
        user=user,
        timeout_s=timeout_s,
        run_id=run_id,
    )
    connect_timeout = max(1, int(round(connect_timeout_s)))
    cmd = [
        "ssh",
        "-i", ssh_key,
        "-o", "BatchMode=yes",
        "-o", f"ConnectTimeout={connect_timeout}",
        "-o", "IdentitiesOnly=yes",
        "-o", "StrictHostKeyChecking=no",
        f"{user}@{host}",
        "powershell",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        "-",
    ]
    started = time.monotonic()
    proc: Optional[asyncio.subprocess.Process] = None
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(
            proc.communicate(task_script.encode("utf-8")),
            timeout=timeout_s,
        )
    except asyncio.TimeoutError:
        if proc is not None:
            with contextlib.suppress(Exception):
                proc.kill()
            with contextlib.suppress(Exception):
                await asyncio.wait_for(proc.wait(), timeout=2.0)
        return {
            "ok": False,
            "returncode": None,
            "error": "timeout",
            "duration_s": round(time.monotonic() - started, 3),
        }
    except (OSError, subprocess.SubprocessError) as exc:
        return {
            "ok": False,
            "returncode": None,
            "error": f"{type(exc).__name__}: {exc}",
            "duration_s": round(time.monotonic() - started, 3),
        }
    stdout_text = stdout.decode("utf-8-sig", errors="replace").strip()
    stderr_text = stderr.decode("utf-8", errors="replace").strip()
    if proc.returncode == 0:
        with contextlib.suppress(json.JSONDecodeError):
            result = json.loads(stdout_text)
            if isinstance(result, dict):
                result["duration_s"] = round(time.monotonic() - started, 3)
                return result
    return {
        "ok": proc.returncode == 0,
        "returncode": proc.returncode,
        "stdout": stdout_text,
        "stderr": stderr_text,
        "duration_s": round(time.monotonic() - started, 3),
    }


def bounded_transcript(lines: list[str], start: int, max_lines: int) -> str:
    """Return a viewport-sized markdown window within the protocol byte budget."""
    end = min(start + max_lines, len(lines))
    start = min(start, end)
    while start < end:
        joined = "\n".join(lines[start:end])
        if len(joined.encode("utf-8")) <= MAX_MARKDOWN_BYTES:
            return joined
        start += 1
    return ""


def scenario_phase_names(phases: str | None) -> list[str]:
    """Parse a comma-separated phase list, dropping empty entries."""
    return [phase.strip() for phase in (phases or "").split(",") if phase.strip()]


def scenario_lease_ttl_ms(phases: str | None, baseline_hold_s: float, soak_duration_s: float) -> int:
    """Size the initial lease so long-running phases finish before TTL expiry."""
    lease_ttl_ms = max(600_000, int(baseline_hold_s * 1000) + 120_000)
    if "soak" in scenario_phase_names(phases):
        lease_ttl_ms = max(lease_ttl_ms, int(soak_duration_s * 1000) + 120_000)
    return lease_ttl_ms


# Fraction of the granted lease TTL after which we proactively renew, leaving a
# margin before expiry. Matches the runtime's 75%-TTL auto-renewal convention
# (RFC 0008; tze_hud_protocol lease governance) and the resident gRPC bridge, so
# a 600s lease renews at ~450s. Renewal — not just a large initial TTL — is what
# keeps a sustained soak/streaming run alive past the original TTL (hud-hk8kl):
# without it the runtime rejects mutations mid-run with MUTATION_REJECTED /
# "lease expired" once the initial TTL elapses.
LEASE_RENEW_FRACTION = 0.75

# Never renew faster than this, so a small TTL cannot turn into a renewal
# busy-loop. Renewal still fires comfortably before any TTL of practical size.
LEASE_RENEW_MIN_INTERVAL_S = 1.0


def lease_renew_interval_s(
    granted_ttl_ms: int, fraction: float = LEASE_RENEW_FRACTION
) -> float:
    """Seconds to wait before renewing a lease of ``granted_ttl_ms``.

    Returns ``fraction`` of the TTL (clamped to a sane minimum), i.e. strictly
    less than the TTL, so the renewal lands before the lease can expire.
    """
    ttl_s = max(0, granted_ttl_ms) / 1000.0
    return max(LEASE_RENEW_MIN_INTERVAL_S, ttl_s * fraction)


def append_soak_tail_line(
    lines: list[str], seed: list[str], cycle: int, elapsed_s: float, window_lines: int,
) -> None:
    """Append one synthetic soak line while keeping only the published tail window."""
    lines.append(f"[soak] line {cycle:06d}  t+{elapsed_s:7.1f}s  {seed[cycle % len(seed)]}")
    if window_lines <= 0:
        lines.clear()
    elif len(lines) > window_lines:
        del lines[:len(lines) - window_lines]


def make_solid_color_node(
    r: float, g: float, b: float, a: float,
    x: float, y: float, w: float, h: float,
    radius: float = -1.0,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    data: dict[str, Any] = {
        "solid_color": {"r": r, "g": g, "b": b, "a": a},
        "bounds": [x, y, w, h],
    }
    if radius >= 0.0:
        data["solid_color"]["radius"] = radius
    if node_id is not None:
        data["id"] = node_id
    return _make_node(data)


def make_text_node(
    content: str, x: float, y: float, w: float, h: float,
    font_px: float, rgba: tuple[float, float, float, float],
    node_id: Optional[bytes] = None,
    preserve_markdown: bool = False,
    overflow: Optional[int] = None,
) -> types_pb2.NodeProto:
    # Explicit transparent background overrides any default RenderingPolicy
    # backdrop for TextMarkdown nodes.
    data: dict[str, Any] = {
        "text_markdown": {
            "content": content,
            "font_size_px": font_px,
            "color": list(rgba),
            "background": [0.0, 0.0, 0.0, 0.0],
        },
        "bounds": [x, y, w, h],
    }
    if preserve_markdown and content:
        data["text_markdown"]["color_runs"] = [{
            "start_byte": 0,
            "end_byte": len(content.encode("utf-8")),
            "color": list(rgba),
        }]
    if overflow is not None:
        data["text_markdown"]["overflow"] = overflow
    if node_id is not None:
        data["id"] = node_id
    return _make_node(data)


def make_hit_region(
    interaction_id: str, x: float, y: float, w: float, h: float,
    *,
    accepts_focus: bool = True,
    accepts_pointer: bool = True,
    auto_capture: bool = False,
    release_on_up: bool = False,
    accepts_composer_input: bool = False,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    data: dict[str, Any] = {
        "hit_region": {
            "interaction_id": interaction_id,
            "accepts_focus": accepts_focus,
            "accepts_pointer": accepts_pointer,
            "auto_capture": auto_capture,
            "release_on_up": release_on_up,
            "accepts_composer_input": accepts_composer_input,
        },
        "bounds": [x, y, w, h],
    }
    if node_id is not None:
        data["id"] = node_id
    return _make_node(data)


def portal_pane_rects() -> tuple[PaneRect, PaneRect]:
    """Return tile-local scroll-capture rects for input composer and output body."""
    pane_y = HEADER_H + DIVIDER_H
    pane_h = PORTAL_H - pane_y - FOOTER_H - DIVIDER_H
    input_pane_x = 0.0
    input_pane_w, output_pane_w = partition_pane_widths(PORTAL_W, INPUT_PANE_W)
    divider_x = input_pane_w
    output_pane_x = divider_x + PANE_DIVIDER_W

    composer_inset = 14.0
    composer_x = composer_inset
    composer_y = pane_y + 40.0
    composer_w = input_pane_w - composer_inset * 2.0
    composer_h = pane_h - 40.0 - 44.0
    tile_x = max(input_pane_x, composer_x - COMPOSER_HIT_PAD)
    tile_y = max(pane_y, composer_y - COMPOSER_HIT_PAD)
    tile_right = min(input_pane_x + input_pane_w, composer_x + composer_w + COMPOSER_HIT_PAD)
    tile_bottom = min(pane_y + pane_h, composer_y + composer_h + COMPOSER_HIT_PAD)

    body_y = pane_y + 40.0
    body_h = pane_h - 40.0 - 8.0
    output_body = PaneRect(
        output_pane_x + PADDING_X,
        body_y,
        output_pane_w - PADDING_X * 2.0,
        body_h,
    )
    input_composer = PaneRect(tile_x, tile_y, tile_right - tile_x, tile_bottom - tile_y)
    return input_composer, output_body


def input_composer_local_rect() -> PaneRect:
    """Return the visible composer box relative to the enlarged input tile."""
    input_tile, _ = portal_pane_rects()
    pane_y = HEADER_H + DIVIDER_H
    pane_h = PORTAL_H - pane_y - FOOTER_H - DIVIDER_H
    composer_inset = 14.0
    composer_x = composer_inset
    composer_y = pane_y + 40.0
    composer_w = INPUT_PANE_W - composer_inset * 2.0
    composer_h = pane_h - 40.0 - 44.0
    return PaneRect(
        composer_x - input_tile.x,
        composer_y - input_tile.y,
        composer_w,
        composer_h,
    )


def scroll_max_y_for_text(content: str, viewport_h: float, line_px: float) -> float:
    """Approximate max scroll offset for bounded text in a pane viewport."""
    line_count = max(1, len(content.splitlines()))
    return max(0.0, line_count * line_px - viewport_h)


def scroll_content_height_for_text(content: str, viewport_h: float, line_px: float) -> float:
    """Approximate full scroll content height for text in a pane viewport."""
    return viewport_h + scroll_max_y_for_text(content, viewport_h, line_px)


def build_portal_nodes(
    title: str,
    subtitle: str,
    body: str,
    footer_meta: str,
    composer_text: str = "",
    composer_placeholder: str = "type a reply — Enter to submit",
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    """Return static frame/chrome nodes for a two-pane portal."""
    root_node = make_solid_color_node(
        *BG_RGBA, 0.0, 0.0, PORTAL_W, PORTAL_H, radius=PORTAL_RADIUS,
    )

    # ── Header chrome (full width) ────────────────────────────────────────
    header_bg = make_solid_color_node(
        *HEADER_BG_RGBA, 0.0, 0.0, PORTAL_W, HEADER_H,
    )
    header_divider = make_solid_color_node(
        *DIVIDER_RGBA, 0.0, HEADER_H, PORTAL_W, DIVIDER_H,
    )
    minimize_bg = make_solid_color_node(
        *MINIMIZE_BG_RGBA,
        MINIMIZE_BUTTON_X,
        MINIMIZE_BUTTON_Y,
        MINIMIZE_BUTTON_SIZE,
        MINIMIZE_BUTTON_SIZE,
        radius=7.0,
    )
    minimize_glyph = make_solid_color_node(
        0.76, 0.82, 0.90, 0.96,
        MINIMIZE_BUTTON_X + 6.0,
        MINIMIZE_BUTTON_Y + 10.0,
        MINIMIZE_BUTTON_SIZE - 12.0,
        2.0,
        radius=1.0,
    )
    minimize_hit = make_hit_region(
        PORTAL_MINIMIZE_INTERACTION_ID,
        0.0,
        0.0,
        MINIMIZE_HIT_W,
        MINIMIZE_HIT_H,
        # hud-02sp5: minimize is the collapse control. The text-stream-portals
        # spec names expand/collapse/reply as focusable "portal controls", so it
        # is a keyboard Tab stop (reachable on pointer-less Mobile Presence Node
        # surfaces). Contrast the header drag bar below, which stays pointer-only.
        accepts_focus=True,
    )
    activity_dot = make_solid_color_node(
        *ACTIVITY_DOT_RGBA,
        PORTAL_W - PADDING_X - ACTIVITY_DOT_SIZE,
        (HEADER_H - ACTIVITY_DOT_SIZE) / 2.0,
        ACTIVITY_DOT_SIZE, ACTIVITY_DOT_SIZE,
        radius=ACTIVITY_DOT_SIZE / 2.0,
    )
    title_x = PADDING_X + MINIMIZE_BUTTON_SIZE + 18.0
    title_node = make_text_node(
        title,
        title_x, 10.0,
        PORTAL_W - title_x - PADDING_X - ACTIVITY_DOT_SIZE - 12.0,
        22.0, TITLE_FONT, TITLE_RGBA,
    )
    subtitle_node = make_text_node(
        subtitle,
        title_x, 31.0,
        PORTAL_W - title_x - PADDING_X,
        16.0, SUBTITLE_FONT, SUBTITLE_RGBA,
    )
    grip_w = 92.0
    grip_h = 6.0
    header_grip = make_solid_color_node(
        *HEADER_GRIP_RGBA,
        (PORTAL_W - grip_w) / 2.0,
        8.0,
        grip_w,
        grip_h,
        radius=grip_h / 2.0,
    )
    # Header drag region. As of hud-643dv the runtime owns whole-portal move via a
    # geometry-driven HEADER BAND drag handle it generates for the portal frame
    # tile (the full top strip drags like a Windows titlebar, yielding to the
    # minimize control). This node is therefore a pure visual/semantic marker of
    # the drag band — it does NOT drive movement — so it is inert: accepts_pointer
    # is False (the runtime band, not this node, handles the pointer) and
    # accepts_focus stays False (pointer-only chrome, never a Tab stop). It spans
    # the full header width to document the whole-band drag surface. An
    # accepts_pointer node here would instead shadow the minimize hit-region and
    # be caught by the runtime band's "yield to interactive nodes" rule, so it
    # MUST stay non-pointer.
    portal_drag_hit = make_hit_region(
        PORTAL_DRAG_INTERACTION_ID,
        0.0, 0.0,
        PORTAL_W, HEADER_H,
        accepts_focus=False,
        accepts_pointer=False,
    )

    # ── Pane geometry ─────────────────────────────────────────────────────
    pane_y = HEADER_H + DIVIDER_H
    pane_h = PORTAL_H - pane_y - FOOTER_H - DIVIDER_H
    input_pane_x = 0.0
    input_pane_w = INPUT_PANE_W
    divider_x = input_pane_x + input_pane_w
    output_pane_x = divider_x + PANE_DIVIDER_W
    output_pane_w = PORTAL_W - output_pane_x

    # ── Input pane (left): eyebrow → composer box → submit hint ──────────
    input_pane_bg = make_solid_color_node(
        *INPUT_PANE_BG_RGBA,
        input_pane_x, pane_y, input_pane_w, pane_h,
    )
    input_eyebrow = make_text_node(
        "INPUT",
        input_pane_x + PADDING_X, pane_y + 14.0,
        input_pane_w - PADDING_X * 2.0, 14.0,
        EYEBROW_FONT, EYEBROW_RGBA,
    )

    composer_inset = 14.0
    composer_x = input_pane_x + composer_inset
    composer_y = pane_y + 40.0
    composer_w = input_pane_w - composer_inset * 2.0
    composer_h = pane_h - 40.0 - 44.0  # leave room for submit-hint strip
    composer_bg = make_solid_color_node(
        *TEXT_WINDOW_BG_RGBA,
        composer_x, composer_y, composer_w, composer_h,
        radius=10.0,
    )
    # 1px inset border drawn as four thin rects — cheap highlight.
    border_t = make_solid_color_node(
        *COMPOSER_BORDER_RGBA, composer_x, composer_y, composer_w, 1.0,
    )
    border_b = make_solid_color_node(
        *COMPOSER_BORDER_RGBA,
        composer_x, composer_y + composer_h - 1.0, composer_w, 1.0,
    )
    border_l = make_solid_color_node(
        *COMPOSER_BORDER_RGBA, composer_x, composer_y, 1.0, composer_h,
    )
    border_r = make_solid_color_node(
        *COMPOSER_BORDER_RGBA,
        composer_x + composer_w - 1.0, composer_y, 1.0, composer_h,
    )

    # Submit hint strip at the bottom of the input pane.
    submit_hint_y = composer_y + composer_h + 10.0
    submit_hint = make_text_node(
        "Enter submit  ·  Shift+Enter newline  ·  Esc cancel",
        input_pane_x + PADDING_X,
        submit_hint_y,
        input_pane_w - PADDING_X * 2.0, 16.0,
        SUBMIT_HINT_FONT, SUBMIT_HINT_RGBA,
    )
    submit_hit = make_hit_region(
        SUBMIT_INTERACTION_ID,
        input_pane_x + PADDING_X,
        submit_hint_y - 6.0,
        input_pane_w - PADDING_X * 2.0, 24.0,
    )

    # ── Vertical divider between panes (drag-resize handle) ───────────────
    pane_divider = make_solid_color_node(
        *PANE_DIVIDER_RGBA,
        divider_x, pane_y, PANE_DIVIDER_W, pane_h,
    )
    grip_x = divider_x + (PANE_DIVIDER_W - PANE_DIVIDER_GRIP_W) / 2.0
    grip_y = pane_y + (pane_h - PANE_DIVIDER_GRIP_H) / 2.0
    pane_divider_grip = make_solid_color_node(
        *PANE_DIVIDER_GRIP_RGBA,
        grip_x, grip_y, PANE_DIVIDER_GRIP_W, PANE_DIVIDER_GRIP_H,
        radius=PANE_DIVIDER_GRIP_W / 2.0,
    )
    pane_resize_hit = make_hit_region(
        PANE_RESIZE_INTERACTION_ID,
        divider_x - 4.0, pane_y, PANE_DIVIDER_W + 8.0, pane_h,
        accepts_focus=False,
        auto_capture=True,
        release_on_up=True,
    )

    # ── Output pane (right) ───────────────────────────────────────────────
    output_pane_bg = make_solid_color_node(
        *OUTPUT_PANE_BG_RGBA,
        output_pane_x, pane_y, output_pane_w, pane_h,
    )
    output_eyebrow = make_text_node(
        "TRANSCRIPT",
        output_pane_x + PADDING_X, pane_y + 14.0,
        output_pane_w - PADDING_X * 2.0, 14.0,
        EYEBROW_FONT, EYEBROW_RGBA,
    )
    _, output_rect = portal_pane_rects()
    output_text_window_bg = make_solid_color_node(
        *TEXT_WINDOW_BG_RGBA,
        output_rect.x,
        output_rect.y,
        output_rect.w,
        output_rect.h,
    )
    # ── Footer ────────────────────────────────────────────────────────────
    footer_divider = make_solid_color_node(
        *DIVIDER_RGBA, 0.0, PORTAL_H - FOOTER_H - DIVIDER_H,
        PORTAL_W, DIVIDER_H,
    )
    footer_bg = make_solid_color_node(
        *FOOTER_BG_RGBA, 0.0, PORTAL_H - FOOTER_H,
        PORTAL_W, FOOTER_H,
    )
    footer_node = make_text_node(
        footer_meta,
        PADDING_X, PORTAL_H - FOOTER_H + 8.0,
        PORTAL_W - PADDING_X * 2.0, 16.0,
        META_FONT, META_RGBA,
    )
    resize_handle = make_solid_color_node(
        *PANE_DIVIDER_GRIP_RGBA,
        PORTAL_W - PORTAL_RESIZE_HANDLE,
        PORTAL_H - PORTAL_RESIZE_HANDLE,
        PORTAL_RESIZE_HANDLE,
        PORTAL_RESIZE_HANDLE,
        radius=6.0,
    )
    resize_hit = make_hit_region(
        PORTAL_RESIZE_INTERACTION_ID,
        PORTAL_W - PORTAL_RESIZE_HANDLE - 8.0,
        PORTAL_H - PORTAL_RESIZE_HANDLE - 8.0,
        PORTAL_RESIZE_HANDLE + 12.0,
        PORTAL_RESIZE_HANDLE + 12.0,
        accepts_focus=False,
        auto_capture=True,
        release_on_up=True,
    )

    children = [
        # header
        header_bg, header_divider, minimize_bg, minimize_glyph, minimize_hit,
        activity_dot, title_node, subtitle_node, header_grip, portal_drag_hit,
        # input pane
        input_pane_bg, input_eyebrow,
        composer_bg, border_t, border_b, border_l, border_r,
        submit_hint, submit_hit,
        # divider (fat drag handle)
        pane_divider, pane_divider_grip, pane_resize_hit,
        # output pane
        output_pane_bg, output_eyebrow, output_text_window_bg,
        # footer
        footer_divider, footer_bg, footer_node, resize_handle, resize_hit,
    ]
    return root_node, children


def build_input_scroll_nodes(
    composer_text: str = "",
    composer_placeholder: str = "type a reply — Enter to submit",
    *,
    node_ids: Optional[dict[str, bytes]] = None,
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    node_ids = node_ids or fresh_composer_node_ids()
    input_rect, _ = portal_pane_rects()
    composer_rect = input_composer_local_rect()
    root = make_solid_color_node(
        0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, input_rect.w, input_rect.h,
        node_id=node_ids.get("root"),
    )
    clear_bg = build_composer_clear_node(node_id=node_ids.get("clear_bg"))
    hit = make_hit_region(
        COMPOSER_INTERACTION_ID,
        composer_rect.x, composer_rect.y, composer_rect.w, composer_rect.h,
        accepts_composer_input=True,
        node_id=node_ids.get("hit"),
    )
    line_nodes, line_window = build_composer_line_nodes(
        composer_text,
        len(composer_text),
        focused=False,
        composer_placeholder=composer_placeholder,
        node_ids=node_ids,
    )
    caret = build_composer_caret_node(
        "",
        0,
        focused=False,
        caret_visible=False,
        visible_start_row=line_window.start_row,
        node_id=node_ids.get("caret"),
    )
    return root, [clear_bg, hit, *line_nodes, caret]


def build_composer_clear_node(
    *,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    composer_rect = input_composer_local_rect()
    return make_solid_color_node(
        *TEXT_WINDOW_BG_RGBA,
        composer_rect.x + 1.0,
        composer_rect.y + 1.0,
        max(0.0, composer_rect.w - 2.0),
        max(0.0, composer_rect.h - 2.0),
        radius=9.0,
        node_id=node_id,
    )


def build_composer_line_node(
    content: str,
    row: int,
    *,
    placeholder_style: bool = False,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    composer_rect = input_composer_local_rect()
    text_inset = 12.0
    return make_text_node(
        content,
        composer_rect.x + text_inset,
        composer_rect.y + text_inset + row * COMPOSER_LINE_PX,
        composer_rect.w - text_inset * 2.0,
        COMPOSER_LINE_PX + COMPOSER_TEXT_RENDER_MARGIN_Y * 2.0,
        INPUT_FONT,
        INPUT_PLACEHOLDER_RGBA if placeholder_style else INPUT_TEXT_RGBA,
        node_id=node_id,
        preserve_markdown=not placeholder_style,
        overflow=types_pb2.TEXT_OVERFLOW_PROTO_CLIP,
    )


def build_composer_line_nodes(
    composer_text: str,
    cursor: int,
    *,
    focused: bool,
    composer_placeholder: str = "type a reply — Enter to submit",
    node_ids: Optional[dict[str, bytes]] = None,
) -> tuple[list[types_pb2.NodeProto], ComposerLineWindow]:
    line_window = composer_line_window(
        composer_text,
        cursor,
        focused=focused,
        composer_placeholder=composer_placeholder,
    )
    line_nodes = [
        build_composer_line_node(
            line if line else " ",
            index,
            placeholder_style=line_window.placeholder_style and index == 0,
            node_id=(node_ids or {}).get(COMPOSER_LINE_KEYS[index]),
        )
        for index, line in enumerate(line_window.lines)
    ]
    return line_nodes, line_window


def build_composer_text_node(
    composer_text: str = "",
    composer_placeholder: str = "type a reply — Enter to submit",
    *,
    placeholder_style: bool = False,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    content = composer_placeholder if placeholder_style else composer_text
    return build_composer_line_node(
        content,
        0,
        placeholder_style=placeholder_style,
        node_id=node_id,
    )


def build_composer_caret_node(
    composer_text: str,
    cursor: int,
    *,
    focused: bool,
    caret_visible: bool,
    visible_start_row: int = 0,
    node_id: Optional[bytes] = None,
) -> types_pb2.NodeProto:
    composer_rect = input_composer_local_rect()
    text_inset = 12.0
    cursor_x, line_index = composer_caret_layout(composer_text, cursor)
    visible_line_index = max(
        0,
        min(line_index - visible_start_row, COMPOSER_VISIBLE_LINE_NODES - 1),
    )
    caret_x = composer_rect.x + text_inset + min(
        COMPOSER_TEXT_RENDER_MARGIN_X + cursor_x,
        max(0.0, composer_rect.w - text_inset * 2.0 - COMPOSER_CARET_W),
    )
    caret_y = (
        composer_rect.y
        + text_inset
        + COMPOSER_TEXT_RENDER_MARGIN_Y
        + visible_line_index * COMPOSER_LINE_PX
    )
    rgba = CARET_RGBA if focused and caret_visible else STATIC_CARET_RGBA
    return make_solid_color_node(
        *rgba,
        caret_x,
        caret_y,
        COMPOSER_CARET_W,
        COMPOSER_CARET_H,
        radius=COMPOSER_CARET_W / 2.0,
        node_id=node_id,
    )


def build_output_scroll_nodes(
    body: str,
    *,
    node_ids: Optional[dict[str, bytes]] = None,
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    node_ids = node_ids or {}
    _, output_rect = portal_pane_rects()
    content_h = scroll_content_height_for_text(body, output_rect.h, SCROLL_LINE_PX)
    hit_h = content_h
    root = make_solid_color_node(
        *TEXT_WINDOW_BG_RGBA, 0.0, 0.0, output_rect.w, output_rect.h,
        node_id=node_ids.get("root"),
    )
    hit = make_hit_region(
        SCROLL_INTERACTION_ID, 0.0, 0.0, output_rect.w, hit_h,
        node_id=node_ids.get("hit"),
    )
    body_node = make_text_node(
        body,
        0.0,
        0.0,
        output_rect.w,
        content_h,
        BODY_FONT,
        BODY_RGBA,
        node_id=node_ids.get("body"),
    )
    return root, [hit, body_node]


def build_output_body_text_node(body: str) -> types_pb2.NodeProto:
    """Build only the output-pane body text node (for in-place updates).

    Matches the geometry of the body node produced by
    [`build_output_scroll_nodes`] so the node can be swapped in place via a
    single ``update_node_content`` mutation without tearing down the tile.
    """
    _, output_rect = portal_pane_rects()
    content_h = scroll_content_height_for_text(body, output_rect.h, SCROLL_LINE_PX)
    return make_text_node(
        body,
        0.0,
        0.0,
        output_rect.w,
        content_h,
        BODY_FONT,
        BODY_RGBA,
    )


def visible_output_text(body: str, offset_y: float, viewport_h: float) -> tuple[str, int]:
    lines = body.splitlines()
    if not lines:
        return "", 0
    start = max(0, min(int(offset_y // SCROLL_LINE_PX), len(lines) - 1))
    visible_count = max(1, int(viewport_h // SCROLL_LINE_PX) + 2)
    end = min(len(lines), start + visible_count)
    return "\n".join(lines[start:end]), start


async def set_root_with_children(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    root: types_pb2.NodeProto,
    children: list[types_pb2.NodeProto],
    mutation_lock: Optional[asyncio.Lock] = None,
) -> tuple[bytes, list[bytes]]:
    if mutation_lock is not None:
        async with mutation_lock:
            return await set_root_with_children(client, lease_id, tile_id, root, children)

    mr = await client.submit_mutation_batch(
        lease_id,
        [types_pb2.MutationProto(
            set_tile_root=types_pb2.SetTileRootMutation(
                tile_id=tile_id, node=root,
            ),
        )],
    )
    root_id = mr.created_ids[0] if mr.created_ids else root.id
    print(f"  [grpc] Tile root set; server root_id={root_id.hex()[:16]}...", flush=True)
    child_ids: list[bytes] = []
    for child in children:
        child_ids.append(await client.add_node(lease_id, tile_id, child, parent_id=root_id))
    return root_id, child_ids


def update_node_content_mutation(
    tile_id: bytes,
    node_id: bytes,
    node: types_pb2.NodeProto,
) -> types_pb2.MutationProto:
    mutation = types_pb2.UpdateNodeContentMutation(tile_id=tile_id, node_id=node_id)
    if node.HasField("solid_color"):
        mutation.solid_color.CopyFrom(node.solid_color)
    elif node.HasField("text_markdown"):
        mutation.text_markdown.CopyFrom(node.text_markdown)
    elif node.HasField("hit_region"):
        mutation.hit_region.CopyFrom(node.hit_region)
    elif node.HasField("static_image"):
        mutation.static_image.CopyFrom(node.static_image)
    else:
        raise ValueError("node has no updateable content")
    return types_pb2.MutationProto(update_node_content=mutation)


def composer_update_mutations(
    tile_id: bytes,
    line_node_ids: list[bytes],
    caret_node_id: bytes,
    line_nodes: list[types_pb2.NodeProto],
    caret_node: types_pb2.NodeProto,
) -> list[types_pb2.MutationProto]:
    """Build one atomic fixed-line composer update batch."""
    if len(line_node_ids) != len(line_nodes):
        raise ValueError("composer line node id/content counts differ")
    return [
        *[
            update_node_content_mutation(tile_id, node_id, node)
            for node_id, node in zip(line_node_ids, line_nodes)
        ],
        update_node_content_mutation(tile_id, caret_node_id, caret_node),
    ]


async def set_frame_root_with_runtime_ids(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    title: str,
    subtitle: str,
    body: str,
    footer_meta: str,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> None:
    frame_root, frame_children = build_portal_nodes(title, subtitle, body, footer_meta)

    async def mount() -> None:
        FRAME_RUNTIME_NODE_IDS.clear()
        frame_root_id, frame_child_ids = await set_root_with_children(
            client, lease_id, tile_id, frame_root, frame_children,
        )
        FRAME_RUNTIME_NODE_IDS["root"] = frame_root_id
        for key, node_id in zip(FRAME_CHILD_KEYS, frame_child_ids):
            FRAME_RUNTIME_NODE_IDS[key] = node_id

    if mutation_lock is not None:
        async with mutation_lock:
            await mount()
    else:
        await mount()


async def update_frame_chrome_live(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    title: str,
    subtitle: str,
    body: str,
    footer_meta: str,
    *,
    live_only: bool = True,
) -> None:
    if not FRAME_RUNTIME_NODE_IDS:
        return
    frame_root, frame_children = build_portal_nodes(title, subtitle, body, footer_meta)
    keyed_nodes = {"root": frame_root}
    keyed_nodes.update(zip(FRAME_CHILD_KEYS, frame_children))
    live_keys = {
        "root",
        "header_bg",
        "header_divider",
        "input_pane_bg",
        "composer_bg",
        "border_t",
        "border_b",
        "border_l",
        "border_r",
        "pane_divider",
        "pane_divider_grip",
        "pane_resize_hit",
        "output_pane_bg",
        "output_text_window_bg",
        "footer_divider",
        "footer_bg",
        "resize_handle",
        "resize_hit",
        # Text content that changes per steady-state publish. Updating these in
        # place (vs. tile teardown/rebuild) is what keeps the frame chrome from
        # flashing empty between the set_tile_root and the trailing add_node
        # batches each cycle (hud-ooeam).
        "title",
        "subtitle",
        "footer_node",
    }
    allowed_keys = live_keys if live_only else set(keyed_nodes.keys())
    mutations = [
        update_node_content_mutation(tile_id, node_id, keyed_nodes[key])
        for key, node_id in FRAME_RUNTIME_NODE_IDS.items()
        if key in keyed_nodes and key in allowed_keys
    ]
    if mutations:
        await client.submit_mutation_batch(lease_id, mutations, timeout=2.0)


def composer_runtime_line_node_ids() -> Optional[list[bytes]]:
    line_node_ids = [
        COMPOSER_RUNTIME_NODE_IDS.get(key) for key in COMPOSER_LINE_KEYS
    ]
    if any(node_id is None for node_id in line_node_ids):
        return None
    return [node_id for node_id in line_node_ids if node_id is not None]


async def set_input_root_with_runtime_ids(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    composer_text: str,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> None:
    node_ids = fresh_composer_node_ids()
    input_root, input_children = build_input_scroll_nodes(
        composer_text,
        node_ids=node_ids,
    )

    async def mount() -> None:
        COMPOSER_RUNTIME_NODE_IDS.clear()
        _, input_child_ids = await set_root_with_children(
            client, lease_id, tile_id, input_root, input_children,
        )
        expected_children = len(COMPOSER_INPUT_CHILD_KEYS)
        if len(input_child_ids) >= expected_children:
            COMPOSER_RUNTIME_NODE_IDS["clear_bg"] = input_child_ids[0]
            COMPOSER_RUNTIME_NODE_IDS["hit"] = input_child_ids[1]
            for key, node_id in zip(
                COMPOSER_LINE_KEYS,
                input_child_ids[2:2 + COMPOSER_VISIBLE_LINE_NODES],
            ):
                COMPOSER_RUNTIME_NODE_IDS[key] = node_id
            COMPOSER_RUNTIME_NODE_IDS["text"] = input_child_ids[2]
            COMPOSER_RUNTIME_NODE_IDS["caret"] = input_child_ids[
                2 + COMPOSER_VISIBLE_LINE_NODES
            ]

    if mutation_lock is not None:
        async with mutation_lock:
            await mount()
    else:
        await mount()


async def set_output_root_with_runtime_ids(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    body: str,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> None:
    """Mount the output-scroll tile with stable, captured node ids.

    Records the runtime-assigned ids in [`OUTPUT_RUNTIME_NODE_IDS`] so that
    subsequent steady-state republishes can update the body text node in place
    (see [`update_output_scroll_body_live`]) instead of tearing the tile down
    and rebuilding it (hud-ooeam).
    """
    output_root, output_children = build_output_scroll_nodes(body)

    async def mount() -> None:
        OUTPUT_RUNTIME_NODE_IDS.clear()
        root_id, child_ids = await set_root_with_children(
            client, lease_id, tile_id, output_root, output_children,
        )
        OUTPUT_RUNTIME_NODE_IDS["root"] = root_id
        if len(child_ids) >= 2:
            OUTPUT_RUNTIME_NODE_IDS["hit"] = child_ids[0]
            OUTPUT_RUNTIME_NODE_IDS["body"] = child_ids[1]

    if mutation_lock is not None:
        async with mutation_lock:
            await mount()
    else:
        await mount()


async def update_output_scroll_body_live(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    body: str,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> bool:
    """Update the output-pane body text node IN PLACE.

    Returns ``True`` when the in-place update was issued, ``False`` when the
    output tile has not been mounted yet (no captured body node id) and the
    caller must fall back to a full mount via
    [`set_output_root_with_runtime_ids`].

    This is the steady-state flicker fix (hud-ooeam): a single atomic
    ``update_node_content`` batch swaps the rasterized body content, so the
    render thread never samples an empty/half-rebuilt body tile between commits.
    The scroll content-height is re-registered in the same batch so the
    scrollable region tracks the new line count.
    """
    body_node_id = OUTPUT_RUNTIME_NODE_IDS.get("body")
    if body_node_id is None:
        return False
    _, output_rect = portal_pane_rects()
    body_node = build_output_body_text_node(body)
    mutations = [
        register_tile_scroll_mutation(
            tile_id,
            scrollable_y=True,
            content_height=scroll_max_y_for_text(
                body, output_rect.h, SCROLL_LINE_PX,
            ),
        ),
        update_node_content_mutation(tile_id, body_node_id, body_node),
    ]

    async def update() -> None:
        await client.submit_mutation_batch(lease_id, mutations, timeout=2.0)

    if mutation_lock is not None:
        async with mutation_lock:
            await update()
    else:
        await update()
    return True


async def render_composer_static(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    composer_text: str,
    cursor: int,
    *,
    focused: bool,
    caret_visible: bool,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> tuple[str, float, int]:
    line_node_ids = composer_runtime_line_node_ids()
    caret_node_id = COMPOSER_RUNTIME_NODE_IDS.get("caret")
    if line_node_ids is None or caret_node_id is None:
        raise RuntimeError("composer nodes are not mounted")

    line_nodes, line_window = build_composer_line_nodes(
        composer_text,
        cursor,
        focused=focused,
    )
    caret_node = build_composer_caret_node(
        composer_text,
        cursor,
        focused=focused,
        caret_visible=caret_visible,
        visible_start_row=line_window.start_row,
        node_id=caret_node_id,
    )

    async def update() -> None:
        await client.submit_mutation_batch(
            lease_id,
            composer_update_mutations(
                tile_id,
                line_node_ids,
                caret_node_id,
                line_nodes,
                caret_node,
            ),
            timeout=2.0,
        )

    if mutation_lock is not None:
        async with mutation_lock:
            await update()
    else:
        await update()

    return line_window.display_text, line_window.cursor_x, line_window.cursor_row


async def update_input_scroll_geometry_live(
    client: HudClient,
    lease_id: bytes,
    tile_id: bytes,
    composer_text: str,
) -> None:
    if not COMPOSER_RUNTIME_NODE_IDS:
        return
    _, input_children = build_input_scroll_nodes(composer_text)
    child_keys = list(COMPOSER_INPUT_CHILD_KEYS)
    mutations = [
        update_node_content_mutation(tile_id, node_id, node)
        for key, node in zip(child_keys, input_children)
        if (node_id := COMPOSER_RUNTIME_NODE_IDS.get(key)) is not None
    ]
    if mutations:
        await client.submit_mutation_batch(lease_id, mutations, timeout=2.0)


async def publish_portal(
    client: HudClient,
    lease_id: bytes,
    tiles: PortalTiles,
    title: str,
    subtitle: str,
    body: str,
    footer_meta: str,
    include_tile_setup: bool,
    composer_text: str = "",
    output_scroll_content: Optional[str] = None,
    mutation_lock: Optional[asyncio.Lock] = None,
) -> None:
    """Publish the portal scene.

    Server rewrites the root id on set_tile_root, so set_tile_root is
    submitted alone first and the server-assigned id is used as parent_id
    for subsequent add_node calls. Batching set_tile_root + add_node fails
    under atomic-batch semantics.
    """
    if include_tile_setup:
        await client.update_tile_opacity(lease_id, tiles.capture_backstop, 0.0)
        await client.update_tile_input_mode(
            lease_id, tiles.capture_backstop, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        for tile_id in (tiles.frame, tiles.input_scroll, tiles.output_scroll):
            await client.update_tile_opacity(lease_id, tile_id, 1.0)
            await client.update_tile_input_mode(
                lease_id, tile_id, types_pb2.TILE_INPUT_MODE_CAPTURE,
            )
        await client.update_tile_input_mode(
            lease_id, tiles.drag_shield, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        await client.update_tile_opacity(lease_id, tiles.drag_shield, 0.0)
        await client.update_tile_opacity(lease_id, tiles.minimized_icon, 0.0)
        await client.update_tile_input_mode(
            lease_id, tiles.minimized_icon, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        input_rect, output_rect = portal_pane_rects()
        output_scroll_body = output_scroll_content if output_scroll_content is not None else body
        await client.submit_mutation_batch(
            lease_id,
            [
                register_tile_scroll_mutation(
                    tiles.input_scroll,
                    scrollable_y=True,
                    content_height=scroll_max_y_for_text(
                        composer_text, input_composer_local_rect().h, SCROLL_LINE_PX,
                    ),
                ),
                register_tile_scroll_mutation(
                    tiles.output_scroll,
                    scrollable_y=True,
                    content_height=scroll_max_y_for_text(
                        output_scroll_body, output_rect.h, SCROLL_LINE_PX,
                    ),
                ),
            ],
        )

    backstop_root, backstop_children = build_capture_backstop_nodes(
        PORTAL_W,
        PORTAL_H,
    )
    await set_root_with_children(
        client, lease_id, tiles.capture_backstop, backstop_root, backstop_children,
        mutation_lock,
    )

    # Frame chrome (title/subtitle/footer + static chrome). In steady state,
    # update the captured nodes IN PLACE rather than tearing down and rebuilding
    # all ~30 frame children (set_tile_root → N×add_node, each its own commit),
    # which would flash the chrome empty for several frames each cycle
    # (hud-ooeam). Fall back to a full mount on first publish / tile setup or if
    # the runtime ids have not been captured yet.
    if not include_tile_setup and FRAME_RUNTIME_NODE_IDS:
        await update_frame_chrome_live(
            client, lease_id, tiles.frame, title, subtitle, body, footer_meta,
            live_only=True,
        )
    else:
        await set_frame_root_with_runtime_ids(
            client, lease_id, tiles.frame, title, subtitle, body, footer_meta,
            mutation_lock,
        )

    should_mount_input = (
        include_tile_setup
        or bool(composer_text)
        or not COMPOSER_RUNTIME_NODE_IDS
    )
    if should_mount_input:
        await set_input_root_with_runtime_ids(
            client, lease_id, tiles.input_scroll, composer_text, mutation_lock,
        )

    # Output-pane body — the tile that actually renders the streamed transcript.
    # The body node always renders `body` (the visible window); the full
    # `output_scroll_content`, when supplied, only sizes the scrollable region.
    # In steady state, swap the body text node IN PLACE via a single atomic
    # update_node_content batch (no tile teardown → no empty-body frame). Only
    # mount/rebuild on first publish, tile setup, or if ids are not yet captured.
    updated_in_place = False
    if not include_tile_setup and OUTPUT_RUNTIME_NODE_IDS:
        updated_in_place = await update_output_scroll_body_live(
            client, lease_id, tiles.output_scroll, body, mutation_lock,
        )
    if not updated_in_place:
        await set_output_root_with_runtime_ids(
            client, lease_id, tiles.output_scroll, body, mutation_lock,
        )

    if include_tile_setup:
        shield_root, shield_children = build_drag_shield_nodes(
            PORTAL_W,
            PORTAL_H,
            None,
        )
        await set_root_with_children(
            client, lease_id, tiles.drag_shield, shield_root, shield_children, mutation_lock,
        )
        icon_root, icon_children = build_minimized_icon_nodes(attention=False, pulse=False)
        await set_root_with_children(
            client, lease_id, tiles.minimized_icon, icon_root, icon_children, mutation_lock,
        )
    elif PORTAL_STATUS_STATE.get("minimized"):
        PORTAL_STATUS_STATE["attention"] = True
        PORTAL_STATUS_STATE["pulse"] = not PORTAL_STATUS_STATE.get("pulse", False)
        icon_root, icon_children = build_minimized_icon_nodes(
            attention=True,
            pulse=PORTAL_STATUS_STATE["pulse"],
        )
        await set_root_with_children(
            client, lease_id, tiles.minimized_icon, icon_root, icon_children, mutation_lock,
        )

    # Drive the promoted portal through the first-class surface API (hud-rpm9s):
    # declare the governed 8-part surface once (on tile setup), then patch its
    # collapse/lifecycle state on every publish. Additive — the raw-tile assembly
    # above stays the retained escape hatch (it still paints the pixels). Collapse
    # state is read from the exemplar's existing `PORTAL_STATUS_STATE["minimized"]`
    # so the six-axis profile-swap / window-mgmt phases reflect through the
    # first-class surface too.
    surface_display_state = (
        types_pb2.PORTAL_DISPLAY_STATE_COLLAPSED
        if PORTAL_STATUS_STATE.get("minimized")
        else types_pb2.PORTAL_DISPLAY_STATE_EXPANDED
    )
    await drive_portal_surface(
        client,
        lease_id,
        tiles,
        title,
        declare=include_tile_setup,
        display_state=surface_display_state,
    )


async def create_portal_tiles(
    client: HudClient,
    lease_id: bytes,
    portal_x: float,
    portal_y: float,
    tab_width: float,
    tab_height: float,
) -> PortalTiles:
    input_rect, output_rect = portal_pane_rects()
    capture_backstop = await client.create_tile(
        lease_id,
        x=portal_x,
        y=portal_y,
        w=PORTAL_W,
        h=PORTAL_H,
        z_order=PORTAL_Z - 100,
    )
    frame = await client.create_tile(
        lease_id,
        x=portal_x,
        y=portal_y,
        w=PORTAL_W,
        h=PORTAL_H,
        z_order=PORTAL_Z,
    )
    input_scroll = await client.create_tile(
        lease_id,
        x=portal_x + input_rect.x,
        y=portal_y + input_rect.y,
        w=input_rect.w,
        h=input_rect.h,
        z_order=PORTAL_Z + 2,
    )
    output_scroll = await client.create_tile(
        lease_id,
        x=portal_x + output_rect.x,
        y=portal_y + output_rect.y,
        w=output_rect.w,
        h=output_rect.h,
        z_order=PORTAL_Z + 3,
    )
    drag_shield = await client.create_tile(
        lease_id,
        x=max(0.0, tab_width - 1.0),
        y=max(0.0, tab_height - 1.0),
        w=1.0,
        h=1.0,
        z_order=PORTAL_Z + 20,
    )
    minimized_icon = await client.create_tile(
        lease_id,
        x=portal_x,
        y=portal_y,
        w=MINIMIZED_ICON_SIZE,
        h=MINIMIZED_ICON_SIZE,
        z_order=PORTAL_Z + 10,
    )
    return PortalTiles(
        capture_backstop=capture_backstop,
        frame=frame,
        input_scroll=input_scroll,
        output_scroll=output_scroll,
        drag_shield=drag_shield,
        minimized_icon=minimized_icon,
        tab_width=tab_width,
        tab_height=tab_height,
    )


# ── First-class portal surface (RFC 0013 §7.2 promotion; hud-rpm9s) ───────────
#
# The exemplar has always assembled the portal from raw tiles (SolidColor /
# TextMarkdown / HitRegion). Post-promotion it ALSO declares the governed
# first-class `PortalSurface` (SetPortalSurface + UpdatePortalSurfaceState),
# driving the promoted surface through the first-class API while the raw six-tile
# assembly stays as the retained escape hatch (it still paints the pixels).
#
# Every declared part carries surface-local geometry (relative to the host
# `frame` tile) and leaves `node` empty ("derived / not materialized"): the
# parts span SEPARATE raw tiles (input_scroll/output_scroll/…), which the scene's
# `set_portal_surface` node-ownership check would reject against the single host
# tile, so — exactly like the resident cooperative adapter — the descriptor
# governs and the raw tiles paint. The part→raw-tile cross-map matches the
# `PortalPartKindProto` comments in types.proto.

# Session id declared as the portal surface's stable identity (RFC 0013 §2.1).
PORTAL_SURFACE_SESSION_ID = "text-stream-portal-exemplar"

# One-shot guard so the structural (Transactional) `SetPortalSurface` declaration
# rides exactly one batch; subsequent renders patch via the coalescible
# `UpdatePortalSurfaceState`, mirroring the resident adapter's `surface_declared`.
_PORTAL_SURFACE_DECLARED = False


def _portal_part(
    kind: int, x: float, y: float, w: float, h: float, node: bytes = b""
) -> "types_pb2.PortalPartProto":
    return types_pb2.PortalPartProto(
        kind=kind,
        bounds=types_pb2.Rect(x=float(x), y=float(y), width=float(w), height=float(h)),
        node=node,
    )


def build_portal_surface_proto(
    display_name: str,
    *,
    lifecycle: int = types_pb2.PORTAL_LIFECYCLE_STATE_ACTIVE,
    display_state: int = types_pb2.PORTAL_DISPLAY_STATE_EXPANDED,
    session_id: str = PORTAL_SURFACE_SESSION_ID,
) -> "types_pb2.PortalSurfaceProto":
    """Build the first-class 8-part `PortalSurfaceProto` for the two-pane layout.

    Bounds are surface-local (relative to the host `frame` tile). `node` is left
    empty for every part — the raw-tile assembly is the retained escape hatch
    that paints the content; this descriptor is the governed surface.
    """
    input_rect, output_rect = portal_pane_rects()
    pane_y = HEADER_H + DIVIDER_H
    pane_h = PORTAL_H - pane_y - FOOTER_H - DIVIDER_H
    input_pane_w, _output_pane_w = partition_pane_widths(PORTAL_W, INPUT_PANE_W)
    divider_x = input_pane_w
    parts = [
        _portal_part(types_pb2.PORTAL_PART_KIND_FRAME, 0.0, 0.0, PORTAL_W, PORTAL_H),
        _portal_part(types_pb2.PORTAL_PART_KIND_HEADER, 0.0, 0.0, PORTAL_W, HEADER_H),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_COMPOSER,
            input_rect.x,
            input_rect.y,
            input_rect.w,
            input_rect.h,
        ),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_TRANSCRIPT,
            output_rect.x,
            output_rect.y,
            output_rect.w,
            output_rect.h,
        ),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_DIVIDER, divider_x, pane_y, PANE_DIVIDER_W, pane_h
        ),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_COLLAPSED_CARD,
            0.0,
            0.0,
            MINIMIZED_ICON_SIZE,
            MINIMIZED_ICON_SIZE,
        ),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_CAPTURE_BACKSTOP, 0.0, 0.0, PORTAL_W, PORTAL_H
        ),
        _portal_part(
            types_pb2.PORTAL_PART_KIND_GESTURE_SHIELD, 0.0, 0.0, PORTAL_W, PORTAL_H
        ),
    ]
    return types_pb2.PortalSurfaceProto(
        identity=types_pb2.PortalIdentityProto(
            session_id=session_id,
            display_name=display_name,
            peer_class=types_pb2.PORTAL_PEER_CLASS_RESIDENT_LLM,
        ),
        lifecycle=lifecycle,
        display_state=display_state,
        parts=parts,
    )


def set_portal_surface_mutation(
    frame_tile_id: bytes, surface: "types_pb2.PortalSurfaceProto"
) -> "types_pb2.MutationProto":
    """Structural (Transactional) declaration of the first-class surface over the
    host `frame` tile (requires the `modify_own_tiles` capability the exemplar
    already holds)."""
    return types_pb2.MutationProto(
        set_portal_surface=types_pb2.SetPortalSurfaceMutation(
            tile_id=frame_tile_id, surface=surface
        )
    )


def update_portal_surface_state_mutation(
    frame_tile_id: bytes,
    *,
    lifecycle: int = types_pb2.PORTAL_LIFECYCLE_STATE_UNSPECIFIED,
    display_state: int = types_pb2.PORTAL_DISPLAY_STATE_UNSPECIFIED,
) -> "types_pb2.MutationProto":
    """Coalescible (StateStream) lifecycle/display patch. UNSPECIFIED leaves a
    field unchanged, so a frequent collapse/lifecycle transition never re-sends
    the whole surface."""
    return types_pb2.MutationProto(
        update_portal_surface_state=types_pb2.UpdatePortalSurfaceStateMutation(
            tile_id=frame_tile_id, lifecycle=lifecycle, display_state=display_state
        )
    )


async def drive_portal_surface(
    client: HudClient,
    lease_id: bytes,
    tiles: "PortalTiles",
    display_name: str,
    *,
    declare: bool,
    display_state: int = types_pb2.PORTAL_DISPLAY_STATE_EXPANDED,
    lifecycle: int = types_pb2.PORTAL_LIFECYCLE_STATE_ACTIVE,
) -> None:
    """Drive the promoted portal through the first-class surface API.

    On the FIRST call (`declare=True` and not yet declared) send ONLY the one-time
    structural `SetPortalSurface` — its descriptor already carries the full
    lifecycle/display state, so no same-batch patch is needed (this avoids an
    in-batch declare-then-patch ordering dependency; the wire session server
    applies a batch atomically). On every SUBSEQUENT call send ONLY the
    coalescible `UpdatePortalSurfaceState` reflecting the current collapse/lifecycle
    state. Additive to the raw-tile publish — the tiles still paint the content.
    """
    global _PORTAL_SURFACE_DECLARED
    if declare and not _PORTAL_SURFACE_DECLARED:
        surface = build_portal_surface_proto(
            display_name, lifecycle=lifecycle, display_state=display_state
        )
        _PORTAL_SURFACE_DECLARED = True
        await client.submit_mutation_batch(
            lease_id, [set_portal_surface_mutation(tiles.frame, surface)]
        )
    elif _PORTAL_SURFACE_DECLARED:
        await client.submit_mutation_batch(
            lease_id,
            [
                update_portal_surface_state_mutation(
                    tiles.frame, lifecycle=lifecycle, display_state=display_state
                )
            ],
        )


def register_tile_scroll_mutation(
    tile_id: bytes,
    *,
    scrollable_y: bool = True,
    content_height: float = -1.0,
) -> types_pb2.MutationProto:
    return types_pb2.MutationProto(
        register_tile_scroll=types_pb2.RegisterTileScrollMutation(
            tile_id=tile_id,
            scrollable_x=False,
            scrollable_y=scrollable_y,
            content_width=-1.0,
            content_height=content_height,
        )
    )


def set_scroll_offset_mutation(
    tile_id: bytes,
    offset_x: float,
    offset_y: float,
) -> types_pb2.MutationProto:
    return types_pb2.MutationProto(
        set_scroll_offset=types_pb2.SetScrollOffsetMutation(
            tile_id=tile_id,
            offset_x=offset_x,
            offset_y=offset_y,
        )
    )


def publish_to_tile_bounds_mutation(
    tile_id: bytes,
    x: float,
    y: float,
    w: float,
    h: float,
) -> types_pb2.MutationProto:
    return types_pb2.MutationProto(
        publish_to_tile=types_pb2.PublishToTileMutation(
            element_id=tile_id,
            bounds=types_pb2.Rect(x=x, y=y, width=w, height=h),
        )
    )


def portal_bounds_mutations(tiles: PortalTiles, portal_x: float, portal_y: float) -> list[types_pb2.MutationProto]:
    input_rect, output_rect = portal_pane_rects()
    return [
        publish_to_tile_bounds_mutation(
            tiles.capture_backstop, portal_x, portal_y, PORTAL_W, PORTAL_H,
        ),
        publish_to_tile_bounds_mutation(
            tiles.frame, portal_x, portal_y, PORTAL_W, PORTAL_H,
        ),
        publish_to_tile_bounds_mutation(
            tiles.input_scroll,
            portal_x + input_rect.x,
            portal_y + input_rect.y,
            input_rect.w,
            input_rect.h,
        ),
        publish_to_tile_bounds_mutation(
            tiles.output_scroll,
            portal_x + output_rect.x,
            portal_y + output_rect.y,
            output_rect.w,
            output_rect.h,
        ),
        publish_to_tile_bounds_mutation(
            tiles.minimized_icon,
            portal_x,
            portal_y,
            MINIMIZED_ICON_SIZE,
            MINIMIZED_ICON_SIZE,
        ),
    ]


def portal_hidden_bounds_mutations(tiles: PortalTiles, portal_x: float, portal_y: float) -> list[types_pb2.MutationProto]:
    hidden_x = max(0.0, tiles.tab_width - 1.0)
    hidden_y = max(0.0, tiles.tab_height - 1.0)
    return [
        publish_to_tile_bounds_mutation(tiles.capture_backstop, hidden_x, hidden_y, 1.0, 1.0),
        publish_to_tile_bounds_mutation(tiles.frame, hidden_x, hidden_y, 1.0, 1.0),
        publish_to_tile_bounds_mutation(tiles.input_scroll, hidden_x, hidden_y, 1.0, 1.0),
        publish_to_tile_bounds_mutation(tiles.output_scroll, hidden_x, hidden_y, 1.0, 1.0),
        publish_to_tile_bounds_mutation(tiles.drag_shield, hidden_x, hidden_y, 1.0, 1.0),
        publish_to_tile_bounds_mutation(
            tiles.minimized_icon,
            portal_x,
            portal_y,
            MINIMIZED_ICON_SIZE,
            MINIMIZED_ICON_SIZE,
        ),
    ]


def build_capture_backstop_nodes(tab_width: float, tab_height: float) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    root = make_solid_color_node(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, tab_width, tab_height)
    hit = make_hit_region(
        "portal-capture-backstop",
        0.0, 0.0,
        tab_width,
        tab_height,
        accepts_focus=False,
    )
    return root, [hit]


def build_drag_shield_nodes(
    tab_width: float,
    tab_height: float,
    interaction_id: Optional[str],
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    root = make_solid_color_node(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, tab_width, tab_height)
    if not interaction_id:
        return root, []
    hit = make_hit_region(
        interaction_id,
        0.0, 0.0,
        tab_width,
        tab_height,
        accepts_focus=False,
    )
    return root, [hit]


def build_minimized_icon_nodes(
    *,
    attention: bool,
    pulse: bool,
) -> tuple[types_pb2.NodeProto, list[types_pb2.NodeProto]]:
    # hud-4e6c0: the card BACKGROUND surfaces (root, core) resolve their hue from
    # the collapsed_card.background token so the minimized state inherits the
    # off-black palette. The foreground marks below (activity accent gold/green,
    # light signal lines, grip nubs) are intentional semantic accents/content, not
    # the grey-background defect this bead targets, so they stay literal here.
    accent = (1.0, 0.82, 0.22, 0.98) if attention else (0.48, 0.86, 0.56, 0.94)
    halo_alpha = 0.28 if pulse else 0.12
    root = make_solid_color_node(
        *MINIMIZED_ICON_BG_RGBA,
        0.0, 0.0,
        MINIMIZED_ICON_SIZE,
        MINIMIZED_ICON_SIZE,
        radius=MINIMIZED_ICON_RADIUS,
    )
    halo = make_solid_color_node(
        accent[0], accent[1], accent[2], halo_alpha,
        4.0, 4.0,
        MINIMIZED_ICON_SIZE - 8.0,
        MINIMIZED_ICON_SIZE - 8.0,
        radius=MINIMIZED_ICON_RADIUS - 3.0,
    )
    core = make_solid_color_node(
        *MINIMIZED_ICON_CORE_RGBA,
        10.0, 10.0,
        MINIMIZED_ICON_SIZE - 20.0,
        MINIMIZED_ICON_SIZE - 20.0,
        radius=12.0,
    )
    signal = make_solid_color_node(
        accent[0], accent[1], accent[2], accent[3],
        36.0, 13.0,
        8.0, 8.0,
        radius=4.0,
    )
    line_a = make_solid_color_node(
        0.82, 0.88, 0.96, 0.95,
        18.0, 23.0, 17.0, 2.0,
        radius=1.0,
    )
    line_b = make_solid_color_node(
        0.82, 0.88, 0.96, 0.70,
        18.0, 30.0, 22.0, 2.0,
        radius=1.0,
    )
    line_c = make_solid_color_node(
        accent[0], accent[1], accent[2], 0.90,
        18.0, 37.0, 13.0, 2.0,
        radius=1.0,
    )
    grip_a = make_solid_color_node(
        0.70, 0.78, 0.90, 0.66,
        43.0, 43.0, 7.0, 2.0,
        radius=1.0,
    )
    grip_b = make_solid_color_node(
        0.70, 0.78, 0.90, 0.66,
        43.0, 49.0, 7.0, 2.0,
        radius=1.0,
    )
    # hud-02sp5: restore is the expand control (minimized icon → full portal).
    # The L-shaped target is split into two hit regions so the icon-drag square
    # stays pointer-draggable; both fragments are focusable so Tab can re-expand
    # a minimized portal without a pointer.
    restore_hit_top = make_hit_region(
        PORTAL_RESTORE_INTERACTION_ID,
        0.0, 0.0,
        MINIMIZED_ICON_SIZE, 36.0,
        accepts_focus=True,
    )
    restore_hit_left = make_hit_region(
        PORTAL_RESTORE_INTERACTION_ID,
        0.0, 36.0,
        36.0, MINIMIZED_ICON_SIZE - 36.0,
        accepts_focus=True,
    )
    drag_hit = make_hit_region(
        PORTAL_ICON_DRAG_INTERACTION_ID,
        36.0, 36.0,
        MINIMIZED_ICON_SIZE - 36.0,
        MINIMIZED_ICON_SIZE - 36.0,
        accepts_focus=False,
    )
    return root, [
        halo, core, signal, line_a, line_b, line_c, grip_a, grip_b,
        restore_hit_top, restore_hit_left, drag_hit,
    ]


def emit_step_event(
    transcript: list[dict[str, Any]],
    step_index: int,
    status: str,
    step: dict[str, Any],
    **extra: Any,
) -> None:
    event = {
        "ts_wall": int(time.time()),
        "step_index": step_index,
        "status": status,
        **step,
        **extra,
    }
    transcript.append(event)
    print(json.dumps(event, sort_keys=True), flush=True)


# ─── Promotion-gate evidence schema (RFC 0013 §7.2, spec Phase-1 gate) ────────
#
# The promotion gate (former text-stream-portals spec, "Phase-1
# Promotion Evidence Gate") requires every live artifact to carry the
# engineering-bar *reference hardware tag* and, for the cadence axis, to report
# runtime-added overhead separately from transport RTT with per-append
# publish→present timestamps. The helpers below shape that evidence so it is
# constructible and verifiable headlessly (no live HUD required).

# Canonical reference host (historical engineering bar, tag pre-reset-2026-10-02).
# A live run on the reference host SHALL match this; off-reference runs are
# informational-only per the gate's "evidence without reference tag" scenario.
REFERENCE_HARDWARE_TAG = "TzeHouse"
REFERENCE_HOSTNAME = "windows-host.example"
REFERENCE_GPU = "NVIDIA GeForce RTX 3080"
REFERENCE_GPU_DRIVER = "32.0.15.9636"
# high_mutation input-to-next-present p99 budget (Windows locked lane), used as
# the presented-append runtime-overhead budget for the cadence axis.
HIGH_MUTATION_PRESENT_BUDGET_MS = 16.6
# Per-cycle wait for a batch's FramePresented present-ack before the cadence axis
# falls back to the transport-RTT present proxy (hud-vjlqh). Generous relative to
# a frame interval so a headless present-ack is reliably observed, but bounded so
# a run against a runtime that never emits present-acks is not stalled per cycle.
PRESENT_ACK_TIMEOUT_S = 0.5
EVIDENCE_SCHEMA_VERSION = 2


def reference_hardware_tag(
    *,
    tag: Optional[str] = None,
    hostname: Optional[str] = None,
    gpu: Optional[str] = None,
    gpu_driver: Optional[str] = None,
    target: Optional[str] = None,
) -> dict[str, Any]:
    """Build the engineering-bar reference-hardware tag for an evidence artifact.

    The local hostname/OS are always collected (the orchestration box that runs
    this harness — typically a Linux collector). The GPU/driver/tag fields come
    from CLI overrides when provided (the runner passes the reference values),
    else fall back to the canonical reference-host constants.

    `is_reference` describes the **target** — the HUD host the run is actually
    driving — NOT the local collection host. A run is reference-grade iff the
    target host matches the engineering-bar reference host identity (the canonical
    ``REFERENCE_HOSTNAME`` per craft-and-care/engineering-bar.md §2, or an
    explicit ``hostname`` override declaring the reference host). This is what
    lets the gate tell budget-bearing Windows TzeHouse runs from informational
    off-reference runs even though the collector itself is a non-reference Linux
    box. ``collected_hostname`` is recorded separately for provenance but never
    decides reference status.
    """
    collected_hostname = ""
    with contextlib.suppress(Exception):
        collected_hostname = socket.gethostname()
    resolved_hostname = hostname or collected_hostname
    target_host_value = target_host(target) if target else ""
    # Reference status is a property of the target host, never the collector.
    # The reference host identity is the explicit override (when the runner
    # declares it) or the canonical engineering-bar reference host.
    reference_identity = hostname or REFERENCE_HOSTNAME
    is_reference = bool(target_host_value) and target_host_value == reference_identity
    return {
        "tag": tag or REFERENCE_HARDWARE_TAG,
        "hostname": resolved_hostname,
        "collected_hostname": collected_hostname,
        "target_host": target_host_value,
        "gpu": gpu or REFERENCE_GPU,
        "gpu_driver": gpu_driver or REFERENCE_GPU_DRIVER,
        "os": f"{platform.system()} {platform.release()}".strip(),
        "is_reference": is_reference,
    }


def present_ms_from_frame_ack(
    publish_ms: float,
    send_wall_us: int,
    present_wall_us: int,
) -> Optional[float]:
    """Derive a run-relative ``present_ms`` from a live FramePresented present-ack.

    The cadence axis was near-vacuous while ``present_ms`` was sampled from the
    same publish-await as ``rtt_ms`` (present≈rtt ⇒ runtime overhead≈0, hud-vjlqh).
    A true present-ack (hud-91uu6, ServerMessage.frame_presented) reports the
    wall-clock (UTC µs) at which the frame carrying a batch was actually presented
    on screen. Subtracting the batch's send wall-clock yields the TRUE
    mutation-arrival→on-screen-present latency, which we re-express in the
    run-relative ``present_ms`` timebase (``publish_ms`` is run-relative) so the
    existing ``present_latency_ms = present_ms - publish_ms`` evidence math holds
    while now measuring real present latency instead of the transport-RTT proxy.

    Both timestamps are the UTC-µs wall-clock domain (RFC 0003 §1.1): the send
    stamp is the client ``timestamp_wall_us``; the present stamp is the runtime's
    ``SystemTime`` at GPU submit. On a single host these share a clock. Returns
    ``None`` when the present precedes the send — cross-host clock skew or a
    mismatched domain — so the caller falls back to the proxy present_ms rather
    than reporting a nonsensical negative latency.
    """
    latency_ms = (present_wall_us - send_wall_us) / 1000.0
    if latency_ms < 0.0:
        return None
    return publish_ms + latency_ms


def build_cadence_rtt_evidence(
    rtt_baseline_ms: float,
    appends: list[dict[str, Any]],
    *,
    cadence_cycles: int,
    cadence_interval_ms: int,
    present_budget_ms: float = HIGH_MUTATION_PRESENT_BUDGET_MS,
) -> dict[str, Any]:
    """Shape the cadence-axis evidence: transport RTT baseline + per-append
    publish→present timestamps with runtime overhead reported separately.

    Each `appends` entry carries `publish_ms` and (when the runtime reported a
    present) `present_ms`; this derives `present_latency_ms` and the
    runtime-added ``overhead_ms``.

    Runtime overhead is isolated PER CYCLE: ``overhead_ms = present_latency_ms -
    rtt_ms`` (this cycle's own measured transport RTT), floored at 0. Subtracting
    a single FIXED ``rtt_baseline_ms`` instead conflated per-cycle transport RTT
    jitter with runtime cost — cycles whose transport RTT spiked above the
    baseline were mis-scored as runtime budget failures even though the runtime
    added ~0ms above its own per-cycle round-trip (hud-lod76, root-caused in
    hud-ans49 against hud-ofe76 live evidence). The fixed ``rtt_baseline_ms`` is
    still reported as ``transport_rtt_baseline_ms`` for context and is used as a
    fallback only when an append lacks a per-cycle ``rtt_ms`` sample.

    Spec §"runtime overhead beyond transport RTT is bounded and evidenced"
    requires presented appends to stay within the high_mutation
    input-to-next-present budget; appends with no present (coalesced away) are
    excluded from the budget check per that requirement.
    """
    baseline = max(0.0, float(rtt_baseline_ms))
    enriched: list[dict[str, Any]] = []
    presented_overheads: list[float] = []
    over_budget = 0
    coalesced = 0
    for entry in appends:
        out = dict(entry)
        publish_ms = float(out.get("publish_ms", 0.0))
        present_ms = out.get("present_ms")
        if present_ms is None:
            out["presented"] = False
            out["present_latency_ms"] = None
            out["overhead_ms"] = None
            coalesced += 1
        else:
            present_latency = max(0.0, float(present_ms) - publish_ms)
            # Isolate RUNTIME overhead using THIS cycle's own measured RTT, not
            # the fixed baseline, so transport jitter is not charged to the
            # runtime. Fall back to the baseline only when a per-cycle sample is
            # absent (e.g. legacy evidence with no rtt_ms).
            cycle_rtt = out.get("rtt_ms")
            overhead_baseline = (
                max(0.0, float(cycle_rtt)) if cycle_rtt is not None else baseline
            )
            overhead = max(0.0, present_latency - overhead_baseline)
            out["presented"] = True
            out["present_latency_ms"] = round(present_latency, 3)
            out["overhead_baseline_ms"] = round(overhead_baseline, 3)
            out["overhead_ms"] = round(overhead, 3)
            out["within_present_budget"] = overhead <= present_budget_ms
            presented_overheads.append(overhead)
            if overhead > present_budget_ms:
                over_budget += 1
        enriched.append(out)

    if presented_overheads:
        ov_sorted = sorted(presented_overheads)
        ov_mean = sum(presented_overheads) / len(presented_overheads)
        p95_idx = max(0, int(len(ov_sorted) * 0.95) - 1)
        ov_max = ov_sorted[-1]
        ov_p95 = ov_sorted[p95_idx]
    else:
        ov_mean = ov_max = ov_p95 = 0.0

    return {
        "cycles": cadence_cycles,
        "interval_ms": cadence_interval_ms,
        "transport_rtt_baseline_ms": round(baseline, 3),
        "present_budget_ms": present_budget_ms,
        "appends": enriched,
        "presented_count": len(presented_overheads),
        "coalesced_count": coalesced,
        "runtime_overhead_ms": {
            "mean": round(ov_mean, 3),
            "p95": round(ov_p95, 3),
            "max": round(ov_max, 3),
            "over_budget_count": over_budget,
        },
        "within_present_budget": over_budget == 0,
    }


def operator_evidence_entry(
    code: str,
    confirm: str,
    observed: dict[str, Any],
) -> dict[str, Any]:
    """Build a structured operator-confirmable evidence entry.

    `confirm` is the operator-facing assertion to visually verify; `observed`
    carries the machine-recorded geometry/state values that back it. Used by the
    window-mgmt and profile-swap axes so their evidence is structured rather than
    prose-only.
    """
    return {
        "code": code,
        "operator_confirm": confirm,
        "observed": observed,
        "confirmed": None,  # operator fills in during the live run
    }


def build_evidence_artifact(
    *,
    target: str,
    doc: str,
    phases: str,
    scene_width: float,
    scene_height: float,
    portal_w: float,
    portal_h: float,
    lease_release_on_exit: bool,
    cleanup_errors: list[str],
    steps: list[dict[str, Any]],
    hardware_tag: dict[str, Any],
) -> dict[str, Any]:
    """Assemble the full gate-conformant evidence artifact payload."""
    return {
        "schema_version": EVIDENCE_SCHEMA_VERSION,
        "reference_hardware": hardware_tag,
        "target": target,
        "doc": doc,
        "phases": phases,
        "scene_width": scene_width,
        "scene_height": scene_height,
        "portal_w": portal_w,
        "portal_h": portal_h,
        "lease_release_on_exit": lease_release_on_exit,
        "cleanup_errors": cleanup_errors,
        "steps": steps,
    }


def write_transcript(path: str, payload: dict[str, Any]) -> None:
    out = Path(path)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2), encoding="utf-8")


async def heartbeat_loop(client: HudClient, interval_ms: int) -> None:
    send_interval_s = max(1.0, interval_ms / 2000.0)
    while True:
        await asyncio.sleep(send_interval_s)
        await client.send_heartbeat()


async def lease_renewal_loop(
    client: HudClient, lease_id: bytes, granted_ttl_ms: int
) -> None:
    """Keep ``lease_id`` alive for the whole session by renewing before expiry.

    Sustained phases (soak, baseline hold, cadence, live interaction) can run
    longer than a single lease TTL. Without renewal the runtime starts rejecting
    mutations mid-run with MUTATION_REJECTED / "lease expired", which also tears
    down the portal render — the systemic self-termination tracked in hud-hk8kl.

    Runs until cancelled (session cleanup). Renews at ``LEASE_RENEW_FRACTION`` of
    the current granted TTL, re-reading the freshly granted TTL after each renew
    so the cadence tracks whatever the runtime actually grants.
    """
    ttl_ms = granted_ttl_ms if granted_ttl_ms > 0 else client.last_granted_lease_ttl_ms
    while True:
        await asyncio.sleep(lease_renew_interval_s(ttl_ms))
        # new_ttl_ms=0 asks the runtime to re-grant the original TTL.
        ttl_ms = await client.renew_lease(lease_id, new_ttl_ms=0)
        if ttl_ms <= 0:
            ttl_ms = granted_ttl_ms


async def portal_interaction_loop(
    client: HudClient,
    lease_id: bytes,
    tiles: PortalTiles,
    transcript: list[dict[str, Any]],
    body_full: str,
    initial_portal_x: float,
    initial_portal_y: float,
    tab_width: float,
    tab_height: float,
    mutation_lock: asyncio.Lock,
    clipboard_host: str,
    clipboard_user: str,
    clipboard_ssh_key: str,
    clipboard_timeout_s: float,
) -> None:
    """Handle live pointer/keyboard input for manual exemplar review."""
    portal_x = initial_portal_x
    portal_y = initial_portal_y
    composer_text = ""
    composer_cursor = 0
    composer_cursor_goal_x: Optional[float] = None
    composer_focused = False
    _, output_rect = portal_pane_rects()
    output_view_start = 0
    # INPUT-pane history: the viewer's OWN submitted entries (hud-egf39). These
    # are recorded here for the exemplar's own bookkeeping/verification; the
    # runtime renders them beneath the composer as viewer-echo turns (#1020).
    # They are NEVER appended to `body_full` (the agent-authored OUTPUT pane).
    input_history: list[str] = []
    drag: Optional[dict[str, float | str]] = None
    last_output_scroll_y: Optional[float] = None
    last_draft_sequence: int = 0
    composer_render_task: Optional[asyncio.Task[None]] = None
    composer_render_dirty = False
    composer_last_dirty_at = 0.0
    composer_caret_visible = True
    composer_blink_task: Optional[asyncio.Task[None]] = None
    background_tasks: set[asyncio.Task[None]] = set()
    portal_minimized = False
    minimized_attention = False
    minimized_pulse = False
    last_drag_apply_at = 0.0

    def track_background_task(task: asyncio.Task[None]) -> asyncio.Task[None]:
        background_tasks.add(task)
        task.add_done_callback(background_tasks.discard)
        return task

    async def cancel_background_tasks() -> None:
        tasks = list(background_tasks)
        for task in tasks:
            if not task.done():
                task.cancel()
        if tasks:
            results = await asyncio.gather(*tasks, return_exceptions=True)
            for result in results:
                if isinstance(result, BaseException) and not isinstance(result, asyncio.CancelledError):
                    raise result

    async def render_minimized_icon() -> None:
        root, children = build_minimized_icon_nodes(
            attention=minimized_attention,
            pulse=minimized_pulse,
        )
        await set_root_with_children(
            client, lease_id, tiles.minimized_icon, root, children, mutation_lock,
        )

    async def set_portal_minimized(minimized: bool) -> None:
        nonlocal minimized_attention, minimized_pulse, portal_minimized, portal_x, portal_y
        if portal_minimized == minimized:
            return
        portal_minimized = minimized
        if minimized:
            minimized_pulse = minimized_attention
            set_composer_focus(False)
        else:
            minimized_attention = False
            minimized_pulse = False
            portal_x = max(0.0, min(portal_x, max(0.0, tab_width - PORTAL_W)))
            portal_y = max(0.0, min(portal_y, max(0.0, tab_height - PORTAL_H)))
        PORTAL_STATUS_STATE["minimized"] = minimized
        PORTAL_STATUS_STATE["attention"] = minimized_attention
        PORTAL_STATUS_STATE["pulse"] = minimized_pulse
        portal_opacity = 0.0 if minimized else 1.0
        portal_input_mode = (
            types_pb2.TILE_INPUT_MODE_PASSTHROUGH
            if minimized
            else types_pb2.TILE_INPUT_MODE_CAPTURE
        )
        icon_opacity = 1.0 if minimized else 0.0
        icon_input_mode = (
            types_pb2.TILE_INPUT_MODE_CAPTURE
            if minimized
            else types_pb2.TILE_INPUT_MODE_PASSTHROUGH
        )
        if minimized:
            async with mutation_lock:
                hidden_x = max(0.0, tiles.tab_width - 1.0)
                hidden_y = max(0.0, tiles.tab_height - 1.0)
                await client.submit_mutation_batch(
                    lease_id,
                    [
                        publish_to_tile_bounds_mutation(tiles.capture_backstop, hidden_x, hidden_y, 1.0, 1.0),
                        publish_to_tile_bounds_mutation(tiles.input_scroll, hidden_x, hidden_y, 1.0, 1.0),
                        publish_to_tile_bounds_mutation(tiles.output_scroll, hidden_x, hidden_y, 1.0, 1.0),
                        publish_to_tile_bounds_mutation(tiles.drag_shield, hidden_x, hidden_y, 1.0, 1.0),
                        publish_to_tile_bounds_mutation(tiles.minimized_icon, hidden_x, hidden_y, 1.0, 1.0),
                        publish_to_tile_bounds_mutation(
                            tiles.frame,
                            portal_x,
                            portal_y,
                            MINIMIZED_ICON_SIZE,
                            MINIMIZED_ICON_SIZE,
                        ),
                    ],
                    timeout=2.0,
                )
                await client.update_tile_opacity(lease_id, tiles.capture_backstop, 0.0)
                await client.update_tile_input_mode(
                    lease_id, tiles.capture_backstop, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
                )
                await client.update_tile_opacity(lease_id, tiles.input_scroll, 0.0)
                await client.update_tile_input_mode(
                    lease_id, tiles.input_scroll, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
                )
                await client.update_tile_opacity(lease_id, tiles.output_scroll, 0.0)
                await client.update_tile_input_mode(
                    lease_id, tiles.output_scroll, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
                )
                await client.update_tile_opacity(lease_id, tiles.frame, 1.0)
                await client.update_tile_input_mode(
                    lease_id, tiles.frame, types_pb2.TILE_INPUT_MODE_CAPTURE,
                )
                await client.update_tile_opacity(lease_id, tiles.minimized_icon, 0.0)
                await client.update_tile_input_mode(
                    lease_id, tiles.minimized_icon, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
                )
                icon_root, icon_children = build_minimized_icon_nodes(
                    attention=minimized_attention,
                    pulse=minimized_pulse,
                )
                await set_root_with_children(
                    client, lease_id, tiles.frame, icon_root, icon_children,
                )
            return

        visible_body, _ = visible_output_text(body_full, 0.0, output_rect.h)
        async with mutation_lock:
            await client.submit_mutation_batch(
                lease_id,
                portal_bounds_mutations(tiles, portal_x, portal_y),
                timeout=2.0,
            )
            await client.update_tile_opacity(lease_id, tiles.capture_backstop, 0.0)
            await client.update_tile_input_mode(
                lease_id, tiles.capture_backstop, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
            )
            for tile_id in (tiles.frame, tiles.input_scroll, tiles.output_scroll):
                await client.update_tile_opacity(lease_id, tile_id, portal_opacity)
                await client.update_tile_input_mode(lease_id, tile_id, portal_input_mode)
            await client.update_tile_opacity(lease_id, tiles.minimized_icon, icon_opacity)
            await client.update_tile_input_mode(lease_id, tiles.minimized_icon, icon_input_mode)
            await set_frame_root_with_runtime_ids(
                client,
                lease_id,
                tiles.frame,
                "Exemplar Review Portal",
                DEFAULT_DOC,
                visible_body,
                f"restored  •  input {INPUT_PANE_W:.0f}px / output {output_rect.w:.0f}px",
            )

    async def move_minimized_icon(new_x: float, new_y: float) -> None:
        nonlocal portal_x, portal_y
        portal_x = max(0.0, min(new_x, max(0.0, tab_width - MINIMIZED_ICON_SIZE)))
        portal_y = max(0.0, min(new_y, max(0.0, tab_height - MINIMIZED_ICON_SIZE)))
        async with mutation_lock:
            await client.submit_mutation_batch(
                lease_id,
                [
                    publish_to_tile_bounds_mutation(
                        tiles.frame,
                        portal_x,
                        portal_y,
                        MINIMIZED_ICON_SIZE,
                        MINIMIZED_ICON_SIZE,
                    )
                ],
                timeout=0.5,
            )

    async def move_portal(new_x: float, new_y: float) -> None:
        nonlocal portal_x, portal_y
        portal_x = max(0.0, min(new_x, max(0.0, tab_width - PORTAL_W)))
        portal_y = max(0.0, min(new_y, max(0.0, tab_height - PORTAL_H)))
        mutations = portal_bounds_mutations(tiles, portal_x, portal_y)
        if drag is not None:
            mutations.append(
                publish_to_tile_bounds_mutation(
                    tiles.drag_shield, portal_x, portal_y, PORTAL_W, PORTAL_H,
                )
            )
        async with mutation_lock:
            await client.submit_mutation_batch(
                lease_id,
                mutations,
                timeout=2.0,
            )

    async def set_drag_shield(interaction_id: Optional[str]) -> None:
        await client.update_tile_opacity(lease_id, tiles.drag_shield, 0.0)
        await client.submit_mutation_batch(
            lease_id,
            [
                publish_to_tile_bounds_mutation(
                    tiles.drag_shield,
                    portal_x if interaction_id else max(0.0, tab_width - 1.0),
                    portal_y if interaction_id else max(0.0, tab_height - 1.0),
                    PORTAL_W if interaction_id else 1.0,
                    PORTAL_H if interaction_id else 1.0,
                )
            ],
            timeout=2.0,
        )
        await client.update_tile_input_mode(
            lease_id,
            tiles.drag_shield,
            types_pb2.TILE_INPUT_MODE_CAPTURE if interaction_id else types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        root, children = build_drag_shield_nodes(PORTAL_W, PORTAL_H, interaction_id)
        await set_root_with_children(
            client, lease_id, tiles.drag_shield, root, children, mutation_lock,
        )

    async def clear_drag_shield() -> None:
        await set_drag_shield(None)

    async def apply_current_bounds() -> None:
        nonlocal portal_x, portal_y, output_rect
        portal_x = max(0.0, min(portal_x, max(0.0, tab_width - PORTAL_W)))
        portal_y = max(0.0, min(portal_y, max(0.0, tab_height - PORTAL_H)))
        _, output_rect = portal_pane_rects()
        mutations = portal_bounds_mutations(tiles, portal_x, portal_y)
        if drag is not None:
            mutations.append(
                publish_to_tile_bounds_mutation(
                    tiles.drag_shield, portal_x, portal_y, PORTAL_W, PORTAL_H,
                )
            )
        async with mutation_lock:
            await client.submit_mutation_batch(
                lease_id,
                mutations,
                timeout=2.0,
            )

    async def apply_drag_delta(dx: float, dy: float, *, rebuild: bool) -> None:
        nonlocal last_drag_apply_at, portal_x, portal_y
        if drag is None:
            return
        now = time.monotonic()
        drag_kind = str(drag.get("kind", "portal"))
        min_interval = (
            ICON_DRAG_APPLY_MIN_INTERVAL_SECONDS
            if drag_kind == "icon"
            else DRAG_APPLY_MIN_INTERVAL_SECONDS
        )
        if not rebuild and now - last_drag_apply_at < min_interval:
            drag["pending_dx"] = dx
            drag["pending_dy"] = dy
            return
        last_drag_apply_at = now
        drag.pop("pending_dx", None)
        drag.pop("pending_dy", None)
        if drag_kind == "portal":
            await move_portal(float(drag["portal_x"]) + dx, float(drag["portal_y"]) + dy)
            return
        if drag_kind == "icon":
            raw_moved = max(abs(dx), abs(dy))
            if not bool(drag.get("icon_dragging", False)):
                if raw_moved < ICON_DRAG_START_THRESHOLD_PX:
                    return
                drag["icon_dragging"] = True
            await move_minimized_icon(float(drag["portal_x"]) + dx, float(drag["portal_y"]) + dy)
            return
        if drag_kind == "pane":
            # Drag-activation threshold: a bare click (no meaningful horizontal
            # travel) must not commit a resize. Mirror the icon-drag idiom —
            # hold the width until cumulative dx crosses the threshold, then
            # latch `pane_dragging` so later sub-threshold jitter keeps resizing
            # (hud-z8z7p).
            if not bool(drag.get("pane_dragging", False)):
                if not pane_drag_crosses_threshold(dx):
                    return
                drag["pane_dragging"] = True
            set_input_pane_width(float(drag["input_pane_w"]) + dx)
        elif drag_kind == "resize":
            set_portal_size(
                float(drag["portal_w"]) + dx,
                float(drag["portal_h"]) + dy,
                tab_width,
                tab_height,
            )
        if rebuild:
            await rebuild_resized_portal()
        else:
            await apply_current_bounds()

    async def rebuild_resized_portal() -> None:
        nonlocal output_rect, portal_x, portal_y
        portal_x = max(0.0, min(portal_x, max(0.0, tab_width - PORTAL_W)))
        portal_y = max(0.0, min(portal_y, max(0.0, tab_height - PORTAL_H)))
        _, output_rect = portal_pane_rects()
        visible_body, _ = visible_output_text(body_full, 0.0, output_rect.h)
        async with mutation_lock:
            await client.submit_mutation_batch(
                lease_id,
                [
                    *portal_bounds_mutations(tiles, portal_x, portal_y),
                    register_tile_scroll_mutation(
                        tiles.input_scroll,
                        scrollable_y=True,
                        content_height=scroll_max_y_for_text(
                            composer_text,
                            input_composer_local_rect().h,
                            SCROLL_LINE_PX,
                        ),
                    ),
                    register_tile_scroll_mutation(
                        tiles.output_scroll,
                        scrollable_y=True,
                        content_height=scroll_max_y_for_text(
                            body_full,
                            output_rect.h,
                            SCROLL_LINE_PX,
                        ),
                    ),
                ],
                timeout=2.0,
            )
            await update_frame_chrome_live(
                client,
                lease_id,
                tiles.frame,
                "Exemplar Review Portal",
                DEFAULT_DOC,
                visible_body,
                f"resized  •  input {INPUT_PANE_W:.0f}px / output {output_rect.w:.0f}px",
                live_only=False,
            )
            await update_input_scroll_geometry_live(
                client, lease_id, tiles.input_scroll, composer_text,
            )
        request_composer_render()

    async def render_composer_once() -> None:
        line_node_ids = composer_runtime_line_node_ids()
        caret_node_id = COMPOSER_RUNTIME_NODE_IDS.get("caret")
        if line_node_ids is None or caret_node_id is None:
            print("  [grpc] Composer render skipped; input nodes not mounted yet.", flush=True)
            return
        runtime_echo_active = composer_focused and bool(composer_text)
        render_text = "" if runtime_echo_active else composer_text
        render_cursor = 0 if runtime_echo_active else composer_cursor
        render_focused = composer_focused and not runtime_echo_active
        render_placeholder = " " if runtime_echo_active else "type a reply — Enter to submit"
        line_nodes, line_window = build_composer_line_nodes(
            render_text,
            render_cursor,
            focused=render_focused,
            composer_placeholder=render_placeholder,
        )
        caret_node = build_composer_caret_node(
            render_text,
            render_cursor,
            focused=render_focused,
            caret_visible=composer_caret_visible and not runtime_echo_active,
            visible_start_row=line_window.start_row,
            node_id=caret_node_id,
        )
        try:
            if mutation_lock is not None:
                async with mutation_lock:
                    await client.submit_mutation_batch(
                        lease_id,
                        composer_update_mutations(
                            tiles.input_scroll,
                            line_node_ids,
                            caret_node_id,
                            line_nodes,
                            caret_node,
                        ),
                        timeout=2.0,
                    )
            else:
                await client.submit_mutation_batch(
                    lease_id,
                    composer_update_mutations(
                        tiles.input_scroll,
                        line_node_ids,
                        caret_node_id,
                        line_nodes,
                        caret_node,
                    ),
                    timeout=2.0,
                )
        except TimeoutError as exc:
            print(f"  [grpc] Composer render skipped after mutation timeout: {exc}", flush=True)
            return
        except RuntimeError as exc:
            if "node not found" in str(exc).lower():
                print("  [grpc] Composer render skipped; stale input nodes during resize.", flush=True)
                return
            raise

    async def composer_render_worker() -> None:
        nonlocal composer_render_dirty, composer_render_task
        try:
            while True:
                composer_render_dirty = False
                while True:
                    quiet_for = time.monotonic() - composer_last_dirty_at
                    remaining = COMPOSER_RENDER_DEBOUNCE_SECONDS - quiet_for
                    if remaining <= 0.0:
                        break
                    await asyncio.sleep(remaining)
                await render_composer_once()
                if not composer_render_dirty:
                    break
        finally:
            composer_render_task = None

    def request_composer_render() -> None:
        nonlocal composer_render_dirty, composer_render_task, composer_last_dirty_at
        composer_render_dirty = True
        composer_last_dirty_at = time.monotonic()
        if composer_render_task is None or composer_render_task.done():
            composer_render_task = track_background_task(
                asyncio.create_task(composer_render_worker())
            )

    async def composer_blink_worker() -> None:
        nonlocal composer_blink_task, composer_caret_visible
        try:
            while composer_focused:
                await asyncio.sleep(COMPOSER_CARET_BLINK_SECONDS)
                if not composer_focused:
                    break
                composer_caret_visible = not composer_caret_visible
                request_composer_render()
        finally:
            composer_blink_task = None

    def set_composer_focus(focused: bool) -> None:
        nonlocal composer_blink_task, composer_caret_visible, composer_focused
        if composer_focused == focused:
            return
        composer_focused = focused
        composer_caret_visible = True
        request_composer_render()
        if focused and (composer_blink_task is None or composer_blink_task.done()):
            composer_blink_task = track_background_task(
                asyncio.create_task(composer_blink_worker())
            )

    own_pointer_tiles = {
        tiles.capture_backstop,
        tiles.frame,
        tiles.input_scroll,
        tiles.output_scroll,
        tiles.drag_shield,
        tiles.minimized_icon,
    }

    async def render_output_scroll(offset_y: float) -> None:
        nonlocal output_view_start
        visible_body, output_view_start = visible_output_text(body_full, offset_y, output_rect.h)
        output_root, output_children = build_output_scroll_nodes(visible_body)
        # This path tears the output tile down and rebuilds it with fresh ids,
        # so any previously-captured runtime ids are now stale. Invalidate them
        # so the next steady-state publish re-mounts instead of issuing an
        # in-place update against a node that no longer exists (hud-ooeam).
        OUTPUT_RUNTIME_NODE_IDS.clear()
        await set_root_with_children(
            client, lease_id, tiles.output_scroll, output_root, output_children,
            mutation_lock,
        )

    async def finish_drag(reason: str, display_x: Optional[float] = None, display_y: Optional[float] = None) -> None:
        nonlocal drag
        if drag is None:
            return
        drag_kind = str(drag.get("kind", "portal"))
        if display_x is None or display_y is None:
            pending_dx = drag.get("pending_dx")
            pending_dy = drag.get("pending_dy")
            if pending_dx is not None and pending_dy is not None:
                await apply_drag_delta(float(pending_dx), float(pending_dy), rebuild=False)
        if display_x is not None and display_y is not None:
            dx = display_x - float(drag["start_x"])
            dy = display_y - float(drag["start_y"])
            await apply_drag_delta(dx, dy, rebuild=False)
        if drag_kind == "icon":
            icon_dragging = bool(drag.get("icon_dragging", False))
            moved = max(
                abs(portal_x - float(drag["portal_x"])),
                abs(portal_y - float(drag["portal_y"])),
            )
            if display_x is not None and display_y is not None:
                moved = max(moved,
                    abs(display_x - float(drag["start_x"])),
                    abs(display_y - float(drag["start_y"])),
                )
            drag = None
            if not icon_dragging and moved < ICON_DRAG_START_THRESHOLD_PX:
                emit_step_event(transcript, 9, "checkpoint", {
                    "code": "portal-icon:drag-cancel",
                    "title": "Minimized icon drag cancelled",
                    "action": "icon drag grip was pressed without crossing the drag threshold",
                    "expected_visual": "icon remains minimized; main icon body still restores immediately",
                }, portal_x=portal_x, portal_y=portal_y,
                   portal_w=PORTAL_W, portal_h=PORTAL_H)
            else:
                emit_step_event(transcript, 9, "checkpoint", {
                    "code": "portal-icon:drag-end",
                    "title": "Minimized icon drag ended",
                    "action": "floating text-stream icon stayed minimized at its new anchor",
                    "expected_visual": "icon remains clickable at the dragged position",
                }, portal_x=portal_x, portal_y=portal_y, reason=reason)
            return
        pane_dragging = bool(drag.get("pane_dragging", False))
        drag = None
        await clear_drag_shield()
        if drag_kind == "pane" and not pane_dragging:
            # Bare click on the middle divider: the activation threshold was
            # never crossed, so no width was committed and INPUT_PANE_W is
            # bit-identical to its pre-click value. Emit a cancel (not an
            # end) and skip the rebuild entirely (hud-z8z7p).
            input_w, out_w = partition_pane_widths(PORTAL_W, INPUT_PANE_W)
            emit_step_event(transcript, 9, "checkpoint", {
                "code": "pane-resize:cancel",
                "title": "Pane resize cancelled",
                "action": "middle divider was clicked without crossing the drag threshold",
                "expected_visual": "input/output panes are unchanged from before the click",
            }, portal_x=portal_x, portal_y=portal_y,
               portal_w=PORTAL_W, portal_h=PORTAL_H,
               input_pane_w=input_w, output_pane_w=out_w,
               reason=reason)
            return
        if drag_kind in {"pane", "resize"}:
            await rebuild_resized_portal()
        code = {
            "portal": "drag:end",
            "pane": "pane-resize:end",
            "resize": "portal-resize:end",
        }.get(drag_kind, "drag:end")
        title = {
            "portal": "Portal drag ended",
            "pane": "Pane resize ended",
            "resize": "Portal resize ended",
        }.get(drag_kind, "Portal drag ended")
        # Report the true output *pane* width (partition invariant), not the
        # inner text-body width — the latter is inset by 2*PADDING_X and made
        # the committed widths look like they failed to sum to portal_w
        # (860+818=1678 vs 1720; the 42px was 6px divider + 36px body inset,
        # not lost frame width) (hud-z8z7p).
        input_w, out_w = partition_pane_widths(PORTAL_W, INPUT_PANE_W)
        emit_step_event(transcript, 9, "checkpoint", {
            "code": code,
            "title": title,
            "action": "all portal tiles committed to grouped position",
            "expected_visual": "input/output panes remain aligned with portal frame",
        }, portal_x=portal_x, portal_y=portal_y,
           portal_w=PORTAL_W, portal_h=PORTAL_H,
           input_pane_w=input_w, output_pane_w=out_w,
           reason=reason)

    async def on_composer_draft_state(text: str, cursor: int, at_capacity: bool, sequence: int) -> None:
        """Handle runtime-owned ComposerDraftStateEvent — update local display state."""
        nonlocal composer_text, composer_cursor, composer_cursor_goal_x, last_draft_sequence
        if sequence <= last_draft_sequence:
            # State-stream: discard stale notifications (sequence is monotonic).
            return
        last_draft_sequence = sequence
        composer_text = text
        composer_cursor = cursor
        composer_cursor_goal_x = None
        emit_step_event(transcript, 10, "checkpoint", {
            "code": "input:draft-state",
            "title": "Composer draft state received",
            "action": "runtime-owned draft buffer delivered coalesced text+cursor snapshot",
            "expected_visual": "composer text window reflects runtime draft state",
        }, text_len=len(text), cursor=cursor, at_capacity=at_capacity, sequence=sequence)
        request_composer_render()

    async def on_composer_draft_submit(text: str, sequence: int) -> None:
        """Handle runtime-owned ComposerDraftSubmitEvent — record into INPUT history.

        Two-pane portal (hud-egf39): a viewer submission belongs to the LEFT
        input pane's OWN history, not the OUTPUT transcript. The runtime already
        renders it as a viewer-echo turn beneath the composer, with a `---`
        divider between adjacent entries (#1020) — so the exemplar records the
        submission in `input_history` (for bookkeeping/verification), clears the
        composer, and leaves `body_full` (the agent-authored OUTPUT pane)
        untouched. This reverts the OUTPUT-pane append shipped in #1027/#1031.
        """
        nonlocal composer_text, composer_cursor, composer_cursor_goal_x, last_draft_sequence
        last_draft_sequence = sequence
        recorded = append_input_history(input_history, text)
        if recorded is not None:
            emit_step_event(transcript, 10, "checkpoint", {
                "code": "input:submit",
                "title": "Composer submitted",
                "action": "runtime-owned draft submitted; recorded in INPUT-pane history, composer clears",
                "expected_visual": "submitted text appears beneath the composer in the LEFT input pane (not the OUTPUT transcript)",
            }, submitted=recorded, input_history_len=len(input_history))
        composer_text = ""
        composer_cursor = 0
        composer_cursor_goal_x = None
        request_composer_render()

    async def on_composer_draft_cancel(sequence: int) -> None:
        """Handle runtime-owned ComposerDraftCancelEvent — clear local display."""
        nonlocal composer_text, composer_cursor, composer_cursor_goal_x, last_draft_sequence
        last_draft_sequence = sequence
        emit_step_event(transcript, 10, "checkpoint", {
            "code": "input:cancel",
            "title": "Composer cancelled",
            "action": "runtime-owned draft cancelled; composer clears",
            "expected_visual": "composer clears without submitting",
        }, sequence=sequence)
        composer_text = ""
        composer_cursor = 0
        composer_cursor_goal_x = None
        request_composer_render()

    # NOTE: Clipboard paste (Ctrl+V) is not currently handled. The prior
    # adapter-owned paste path (paste_windows_clipboard / fallback_paste_request)
    # wrote directly into the adapter's local composer_text buffer, bypassing
    # the runtime-owned draft. There is no runtime API to inject text from the
    # clipboard into the runtime draft buffer yet — this is a follow-up item.

    try:
        while True:
            try:
                timeouts: list[float] = []
                if drag is not None:
                    last_activity_at = float(drag.get("last_activity_at", drag["started_at"]))
                    timeouts.append(
                        max(0.0, DRAG_IDLE_RELEASE_SECONDS - (time.monotonic() - last_activity_at))
                    )
                timeout = min(timeouts) if timeouts else None
                batch = await asyncio.wait_for(client._event_queue.get(), timeout=timeout)
            except asyncio.TimeoutError:
                if drag is not None:
                    last_activity_at = float(drag.get("last_activity_at", drag["started_at"]))
                    if time.monotonic() - last_activity_at >= DRAG_IDLE_RELEASE_SECONDS:
                        await finish_drag("idle_release")
                continue
            pending_output_scroll_y: Optional[float] = None
            for envelope in batch.events:
                kind = envelope.WhichOneof("event")

                if kind == "pointer_down":
                    ev = envelope.pointer_down
                    if ev.tile_id not in own_pointer_tiles:
                        continue
                    if ev.interaction_id == PORTAL_MINIMIZE_INTERACTION_ID:
                        if drag is not None:
                            await finish_drag("superseded:minimize")
                        await set_portal_minimized(True)
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "portal:minimize",
                            "title": "Portal minimized",
                            "action": "top-left minimize control collapsed the portal to a floating status icon",
                            "expected_visual": "full portal hides; compact text-stream icon remains clickable",
                        }, portal_x=portal_x, portal_y=portal_y)
                    elif ev.interaction_id == PORTAL_RESTORE_INTERACTION_ID:
                        if not portal_minimized:
                            emit_step_event(transcript, 9, "checkpoint", {
                                "code": "portal:restore-ignored",
                                "title": "Restore ignored while portal is expanded",
                                "action": "stale minimized-icon event arrived after restore",
                                "expected_visual": "expanded portal remains visible",
                            }, portal_x=portal_x, portal_y=portal_y)
                            continue
                        if drag is not None and str(drag.get("kind", "")) == "icon":
                            await finish_drag("superseded:restore-click")
                        await set_portal_minimized(False)
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "portal:restore",
                            "title": "Portal restored",
                            "action": "floating text-stream icon body restored the full portal immediately",
                            "expected_visual": "portal reappears at the icon anchor",
                        }, portal_x=portal_x, portal_y=portal_y,
                           portal_w=PORTAL_W, portal_h=PORTAL_H)
                    elif ev.interaction_id == PORTAL_ICON_DRAG_INTERACTION_ID:
                        if not portal_minimized:
                            continue
                        if drag is not None:
                            if str(drag.get("kind", "")) == "icon":
                                emit_step_event(transcript, 9, "checkpoint", {
                                    "code": "portal-icon:pointer-down-ignored",
                                    "title": "Duplicate icon drag pointer down ignored",
                                    "action": "minimized icon drag grip already has an armed gesture",
                                    "expected_visual": "icon remains ready to move while dragging",
                                }, portal_x=portal_x, portal_y=portal_y)
                                continue
                            await finish_drag("superseded:restore")
                        drag = {
                            "kind": "icon",
                            "device_id": ev.device_id,
                            "start_x": ev.display_x,
                            "start_y": ev.display_y,
                            "portal_x": portal_x,
                            "portal_y": portal_y,
                            "icon_dragging": False,
                            "started_at": time.monotonic(),
                            "last_activity_at": time.monotonic(),
                        }
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "portal-icon:pointer-down",
                            "title": "Minimized icon drag grip pointer down",
                            "action": "floating text-stream icon drag grip is ready to move the icon",
                            "expected_visual": "icon moves only after a deliberate drag from the grip",
                        }, portal_x=portal_x, portal_y=portal_y,
                           portal_w=PORTAL_W, portal_h=PORTAL_H)
                    elif ev.interaction_id == PORTAL_DRAG_INTERACTION_ID:
                        if drag is not None:
                            await finish_drag("superseded:pointer_down")
                        drag = {
                            "kind": "portal",
                            "device_id": ev.device_id,
                            "start_x": ev.display_x,
                            "start_y": ev.display_y,
                            "portal_x": portal_x,
                            "portal_y": portal_y,
                            "started_at": time.monotonic(),
                            "last_activity_at": time.monotonic(),
                        }
                        await set_drag_shield(PORTAL_DRAG_INTERACTION_ID)
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "drag:start",
                            "title": "Portal drag started",
                            "action": "header drag surface received pointer down",
                            "expected_visual": "portal follows pointer while dragging",
                        }, display_x=ev.display_x, display_y=ev.display_y)
                    elif ev.interaction_id == PANE_RESIZE_INTERACTION_ID:
                        if drag is not None:
                            await finish_drag("superseded:pane_resize")
                        drag = {
                            "kind": "pane",
                            "device_id": ev.device_id,
                            "start_x": ev.display_x,
                            "start_y": ev.display_y,
                            "input_pane_w": INPUT_PANE_W,
                            "started_at": time.monotonic(),
                            "last_activity_at": time.monotonic(),
                        }
                        await set_drag_shield(PANE_RESIZE_INTERACTION_ID)
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "pane-resize:start",
                            "title": "Pane resize started",
                            "action": "middle divider received pointer down",
                            "expected_visual": "dragging changes input/output width ratio",
                        }, display_x=ev.display_x, display_y=ev.display_y,
                           input_pane_w=INPUT_PANE_W)
                    elif ev.interaction_id == PORTAL_RESIZE_INTERACTION_ID:
                        if drag is not None:
                            await finish_drag("superseded:portal_resize")
                        drag = {
                            "kind": "resize",
                            "device_id": ev.device_id,
                            "start_x": ev.display_x,
                            "start_y": ev.display_y,
                            "portal_w": PORTAL_W,
                            "portal_h": PORTAL_H,
                            "started_at": time.monotonic(),
                            "last_activity_at": time.monotonic(),
                        }
                        await set_drag_shield(PORTAL_RESIZE_INTERACTION_ID)
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "portal-resize:start",
                            "title": "Portal resize started",
                            "action": "bottom-right resize handle received pointer down",
                            "expected_visual": "dragging resizes the whole portal surface",
                        }, display_x=ev.display_x, display_y=ev.display_y,
                           portal_w=PORTAL_W, portal_h=PORTAL_H)
                    elif ev.interaction_id == COMPOSER_INTERACTION_ID:
                        if drag is not None:
                            await finish_drag("superseded:composer_focus")
                        emit_step_event(transcript, 10, "checkpoint", {
                            "code": "input:focus-attempt",
                            "title": "Composer pointer down",
                            "action": "input composer received pointer down",
                            "expected_visual": "keyboard focus should move to composer",
                        }, display_x=ev.display_x, display_y=ev.display_y)
                    else:
                        emit_step_event(transcript, 9, "checkpoint", {
                            "code": "input:pointer-down-unhandled",
                            "title": "Unhandled portal pointer down",
                            "action": "runtime delivered pointer down for an interaction id without exemplar handling",
                            "expected_visual": "operator click may appear to do nothing",
                        }, interaction_id=ev.interaction_id,
                           display_x=ev.display_x, display_y=ev.display_y)

                elif kind == "pointer_move" and drag is not None:
                    ev = envelope.pointer_move
                    if ev.tile_id not in own_pointer_tiles:
                        continue
                    if ev.device_id != drag["device_id"]:
                        continue
                    now = time.monotonic()
                    if now - float(drag["started_at"]) > DRAG_MAX_SECONDS:
                        await finish_drag("watchdog")
                        continue
                    drag["last_activity_at"] = now
                    dx = ev.display_x - float(drag["start_x"])
                    dy = ev.display_y - float(drag["start_y"])
                    await apply_drag_delta(dx, dy, rebuild=False)

                elif kind == "pointer_up" and drag is not None:
                    ev = envelope.pointer_up
                    if ev.tile_id not in own_pointer_tiles:
                        continue
                    if ev.device_id == drag["device_id"]:
                        await finish_drag("pointer_up", ev.display_x, ev.display_y)

                elif kind == "pointer_cancel" and drag is not None:
                    ev = envelope.pointer_cancel
                    if ev.tile_id not in own_pointer_tiles:
                        continue
                    if ev.device_id == drag["device_id"]:
                        await finish_drag("pointer_cancel")

                elif kind == "capture_released" and drag is not None:
                    ev = envelope.capture_released
                    if ev.tile_id not in own_pointer_tiles:
                        continue
                    if ev.device_id == drag["device_id"]:
                        reason_name = events_pb2.CaptureReleasedReason.Name(ev.reason)
                        await finish_drag(f"capture_released:{reason_name}")

                elif kind == "character":
                    # Character events are observed for logging only. Composer text
                    # state is now driven exclusively by ComposerDraftStateEvent
                    # (runtime-owned draft path, spec §4.6).
                    ev = envelope.character
                    if ev.tile_id != tiles.input_scroll:
                        continue
                    character = normalize_composer_input(ev.character)
                    emit_step_event(transcript, 10, "checkpoint", {
                        "code": "input:character",
                        "title": "Composer character received (observed)",
                        "action": "runtime delivered character input; state update arrives via composer_draft_state",
                        "expected_visual": "composer state will update when draft-state event arrives",
                    }, character=character)

                elif kind == "key_down":
                    # Key events are observed for logging only. Composer state is
                    # driven by ComposerDraftStateEvent (runtime-owned, spec §4.6).
                    # Submit/cancel arrive as ComposerDraftSubmitEvent / ComposerDraftCancelEvent.
                    ev = envelope.key_down
                    if ev.tile_id != tiles.input_scroll:
                        continue
                    emit_step_event(transcript, 10, "checkpoint", {
                        "code": "input:key-down",
                        "title": "Composer key down received (observed)",
                        "action": "runtime delivered key input; draft state update arrives via composer_draft_state",
                        "expected_visual": "editing result arrives via runtime-owned draft-state event",
                    }, key=ev.key, key_code=ev.key_code, repeat=ev.repeat,
                       ctrl=ev.ctrl, shift=ev.shift, alt=ev.alt, meta=ev.meta)

                elif kind == "key_up":
                    ev = envelope.key_up
                    if ev.tile_id != tiles.input_scroll:
                        continue
                    emit_step_event(transcript, 10, "checkpoint", {
                        "code": "input:key-up",
                        "title": "Composer key up received (observed)",
                        "action": "runtime delivered key release; used to diagnose OS key delivery",
                        "expected_visual": "no visible change expected from release alone",
                    }, key=ev.key, key_code=ev.key_code,
                       ctrl=ev.ctrl, shift=ev.shift, alt=ev.alt, meta=ev.meta)

                elif kind == "scroll_offset_changed":
                    ev = envelope.scroll_offset_changed
                    if ev.tile_id != tiles.output_scroll:
                        continue
                    if abs(ev.offset_y) < 0.5:
                        continue
                    pending_output_scroll_y = ev.offset_y

                elif kind == "focus_gained":
                    ev = envelope.focus_gained
                    if ev.tile_id != tiles.input_scroll:
                        continue
                    emit_step_event(transcript, 10, "checkpoint", {
                        "code": "input:focus-gained",
                        "title": "Composer focus gained",
                        "action": "runtime focus manager focused the composer hit region",
                        "expected_visual": "subsequent keyboard events route to composer",
                    })
                    set_composer_focus(True)

                elif kind == "focus_lost":
                    ev = envelope.focus_lost
                    if ev.tile_id != tiles.input_scroll:
                        continue
                    emit_step_event(transcript, 10, "checkpoint", {
                        "code": "input:focus-lost",
                        "title": "Composer focus lost",
                        "action": "runtime focus manager moved focus away from composer",
                        "expected_visual": "composer stops receiving keyboard events",
                    })
                    set_composer_focus(False)

                elif kind == "composer_draft_state":
                    # Runtime-owned coalesced draft state (spec §4.6).
                    ev = envelope.composer_draft_state
                    await on_composer_draft_state(
                        ev.text,
                        int(ev.cursor),
                        ev.at_capacity,
                        int(ev.sequence),
                    )

                elif kind == "composer_draft_submit":
                    # Runtime-owned transactional submit (spec §4.6).
                    ev = envelope.composer_draft_submit
                    await on_composer_draft_submit(ev.text, int(ev.sequence))

                elif kind == "composer_draft_cancel":
                    # Runtime-owned transactional cancel (spec §4.6).
                    ev = envelope.composer_draft_cancel
                    await on_composer_draft_cancel(int(ev.sequence))

            if pending_output_scroll_y is not None:
                if last_output_scroll_y is not None and abs(pending_output_scroll_y - last_output_scroll_y) < 0.5:
                    continue
                last_output_scroll_y = pending_output_scroll_y
                # The runtime already translated the scrolled content locally
                # (hud-w5ih glyph tracking); re-rendering a windowed slice here
                # fought that translation and made content jump between two
                # positions on every wheel notch (round-6 'flicker/spazz',
                # hud-991cj). Scroll is now observe-and-log only.
                output_view_start = int(pending_output_scroll_y // SCROLL_LINE_PX)
                emit_step_event(transcript, 8, "checkpoint", {
                    "code": "scroll:output",
                    "title": "Output transcript scrolled",
                    "action": "portal received local-first scroll offset (runtime-translated; no agent re-render)",
                    "expected_visual": "output text stays clipped inside transcript box",
                }, scroll_y=pending_output_scroll_y, viewport_start=output_view_start)
    finally:
        await cancel_background_tasks()


async def run_baseline(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]], hold_s: float,
    mutation_lock: asyncio.Lock,
) -> None:
    emit_step_event(transcript, 1, "started", {
        "code": "baseline",
        "title": "Baseline render",
        "action": "publish full portal chrome + transcript viewport",
        "expected_visual": "portal surface appears at right edge with header, body, footer",
    })
    total_lines = len(body_full.splitlines())
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle=DEFAULT_DOC,
        body=body_full,
        footer_meta=f"lines 1-{total_lines}  •  content-layer  •  live",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 1, "completed", {
        "code": "baseline",
        "title": "Baseline render",
        "action": "hold for operator observation",
        "expected_visual": "portal surface visible; body text readable",
    }, hold_s=hold_s, lines=total_lines)
    await asyncio.sleep(hold_s)


async def run_scroll(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    transcript: list[dict[str, Any]],
    mutation_lock: asyncio.Lock,
) -> None:
    """Exercise the transcript interaction contract inside the portal output pane."""
    emit_step_event(transcript, 4, "started", {
        "code": "scroll",
        "title": "Output scroll contract",
        "action": "mount long output, register scroll, step offset, append tail, return",
        "expected_visual": "OUTPUT pane scrolls through bounded transcript data, then returns to latest lines",
    })

    history = [
        f"[{i:03d}] Stream output line {i}: {'data ' * 8}".rstrip()
        for i in range(SCROLL_TOTAL_LINES)
    ]
    await client.submit_mutation_batch(
        lease_id,
        [register_tile_scroll_mutation(
            tiles.output_scroll,
            scrollable_y=True,
            content_height=max(
                0.0,
                len(history) * SCROLL_LINE_PX - portal_pane_rects()[1].h,
            ),
        )],
    )
    viewport_start = 0
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Transcript Interaction Contract",
        body=bounded_transcript(history, viewport_start, SCROLL_VISIBLE_LINES),
        footer_meta=(
            f"scroll  •  lines {viewport_start + 1}-"
            f"{viewport_start + SCROLL_VISIBLE_LINES} / {len(history)}"
        ),
        include_tile_setup=True,
        output_scroll_content="\n".join(history),
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 4, "checkpoint", {
        "code": "scroll:mount",
        "title": "Mount long output",
        "action": "registered vertical scroll config and mounted bounded output",
        "expected_visual": "OUTPUT pane shows first transcript window within portal bounds",
    }, visible_lines=SCROLL_VISIBLE_LINES, total_lines=len(history))
    await asyncio.sleep(SCROLL_PHASE_PAUSE_S)

    scroll_offset = 0.0
    for step in range(4):
        scroll_offset += SCROLL_STEP_PX
        viewport_start = min(
            len(history) - SCROLL_VISIBLE_LINES,
            int(scroll_offset // SCROLL_LINE_PX),
        )
        await client.submit_mutation_batch(
            lease_id,
            [set_scroll_offset_mutation(tiles.output_scroll, 0.0, scroll_offset)],
        )
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle="Transcript Interaction Contract",
            body=bounded_transcript(history, viewport_start, SCROLL_VISIBLE_LINES),
            footer_meta=(
                f"scroll_y={scroll_offset:.0f}px  •  lines "
                f"{viewport_start + 1}-{viewport_start + SCROLL_VISIBLE_LINES}"
            ),
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        emit_step_event(transcript, 4, "checkpoint", {
            "code": "scroll:offset",
            "title": "Scroll output window",
            "action": f"set scroll_y={scroll_offset:.0f}px",
            "expected_visual": "OUTPUT pane advances through transcript while chrome remains readable",
        }, scroll_y=scroll_offset, viewport_start=viewport_start, scroll_step=step + 1)
        await asyncio.sleep(0.5)

    await asyncio.sleep(SCROLL_PHASE_PAUSE_S)
    mid_scroll = scroll_offset
    for i in range(5):
        history.append(f"[NEW-{i:02d}] Tail append at t+{i}: live output arriving")
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle="Transcript Interaction Contract",
            body=bounded_transcript(history, viewport_start, SCROLL_VISIBLE_LINES),
            footer_meta=(
                f"mid-scroll append  •  held scroll_y={mid_scroll:.0f}px  •  "
                f"tail={len(history)} lines"
            ),
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        emit_step_event(transcript, 4, "checkpoint", {
            "code": "scroll:append",
            "title": "Append while mid-scroll",
            "action": f"append line {len(history) - 1} while preserving scroll_y",
            "expected_visual": "visible output window does not jump to the tail",
        }, scroll_y=mid_scroll, total_lines=len(history))
        await asyncio.sleep(0.6)

    await client.submit_mutation_batch(
        lease_id,
        [set_scroll_offset_mutation(tiles.output_scroll, 0.0, 0.0)],
    )
    tail_start = max(0, len(history) - SCROLL_VISIBLE_LINES)
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Transcript Interaction Contract",
        body=bounded_transcript(history, tail_start, SCROLL_VISIBLE_LINES),
        footer_meta=f"tail  •  lines {tail_start + 1}-{len(history)} / {len(history)}",
        include_tile_setup=False,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 4, "completed", {
        "code": "scroll",
        "title": "Output scroll contract",
        "action": "returned scroll offset to tail",
        "expected_visual": "latest appended lines are visible in OUTPUT pane",
    }, tail_start=tail_start, total_lines=len(history))
    await asyncio.sleep(SCROLL_PHASE_PAUSE_S)


async def run_streaming(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    chunks: int, chunk_interval_s: float,
    mutation_lock: asyncio.Lock,
) -> None:
    emit_step_event(transcript, 2, "started", {
        "code": "streaming",
        "title": "Incremental streaming reveal",
        "action": f"reveal body in {chunks} ordered chunks",
        "expected_visual": "body grows over time; header/footer unchanged",
    })
    lines = body_full.splitlines()
    per_chunk = max(1, len(lines) // chunks)
    for i in range(1, chunks + 1):
        end = min(len(lines), per_chunk * i) if i < chunks else len(lines)
        partial = "\n".join(lines[:end])
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle=DEFAULT_DOC,
            body=partial,
            footer_meta=f"streaming  •  lines 1-{end} / {len(lines)}",
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        if i < chunks:
            await asyncio.sleep(chunk_interval_s)
    emit_step_event(transcript, 2, "completed", {
        "code": "streaming",
        "title": "Incremental streaming reveal",
        "action": "final chunk published",
        "expected_visual": "full body visible, matches baseline",
    })


async def run_rapid(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    cycles: int, interval_ms: int,
    mutation_lock: asyncio.Lock,
) -> None:
    emit_step_event(transcript, 3, "started", {
        "code": "rapid",
        "title": "Rapid replace (coalescing smoke)",
        "action": f"publish {cycles} alternating bodies, ~{interval_ms}ms apart",
        "expected_visual": "portal remains coherent; no collapse to latest-only line",
    })
    lines = body_full.splitlines()
    alt_bodies = [
        "\n".join(lines[: max(8, len(lines) // 2)]),
        "\n".join(lines),
    ]
    for i in range(cycles):
        body = alt_bodies[i % 2]
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle=DEFAULT_DOC,
            body=body,
            footer_meta=f"rapid  •  cycle {i+1}/{cycles}",
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        await asyncio.sleep(interval_ms / 1000.0)
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle=DEFAULT_DOC,
        body=body_full,
        footer_meta=f"rapid-settled  •  lines 1-{len(lines)}",
        include_tile_setup=False,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 3, "completed", {
        "code": "rapid",
        "title": "Rapid replace (coalescing smoke)",
        "action": "settled on full body",
        "expected_visual": "full body visible, no tearing",
    })


# --- Soak completion marker contract ------------------------------------------
#
# A soak run is a FULL-DURATION memory-drift measurement. The authoritative
# completion marker must therefore be written ONLY when the soak actually ran
# its intended duration to the end without a fatal early termination (e.g. the
# portal lease expiring mid-run). Any early / lease-death / exception
# termination writes a DISTINCT aborted marker (with the termination reason)
# instead, and never leaves a completion marker behind — so an automated gate
# keying on ``soak-complete.marker`` cannot false-pass a soak that died early.
SOAK_COMPLETE_MARKER_NAME = "soak-complete.marker"
SOAK_ABORTED_MARKER_NAME = "soak-aborted.marker"

# A full-duration soak must survive a single transient mutation-ack blip (a
# momentary tailnet stall or runtime pause) rather than aborting ~146s short of
# the 60-min target (hud-n5bqp). run_soak enables a small bounded retry on the
# client for the duration of the soak: each transient ack timeout resubmits the
# batch (fresh id) up to SOAK_MUTATION_RETRIES times with exponential backoff,
# every retry logs visibly, and the run still aborts once the budget is
# exhausted — so a genuine sustained-load ack wall is surfaced, not masked.
SOAK_MUTATION_RETRIES = 3
SOAK_MUTATION_RETRY_BACKOFF_S = 1.0
SOAK_COMPLETE_TOKEN = "SOAK_COMPLETE"
SOAK_ABORTED_TOKEN = "SOAK_ABORTED"
# A genuinely completed soak must reach at least this fraction of its intended
# duration. The soak loop only exits normally once elapsed >= duration_s, so a
# real completion clears this comfortably; the guard exists so a completion
# marker can never be stamped onto a run that was actually cut short.
SOAK_COMPLETION_MIN_FRACTION = 0.98


def soak_run_completed(intended_s: float, actual_s: float) -> bool:
    """Return True only if a soak ran (about) its full intended duration.

    Early lease-death / exception runs fall short of ``intended_s`` and must not
    be treated as genuine completions.
    """
    if intended_s <= 0.0:
        return True
    return actual_s >= intended_s * SOAK_COMPLETION_MIN_FRACTION


def write_soak_outcome_marker(
    marker_dir: "Path | str",
    *,
    completed: bool,
    intended_s: float,
    actual_s: float,
    cycles: int,
    reason: str = "",
) -> Path:
    """Write the authoritative soak completion/abort marker.

    Writes ``soak-complete.marker`` (``SOAK_COMPLETE``) ONLY when the soak
    reached a genuine full-duration completion (``completed`` is True AND the
    actual duration is within tolerance of the intended duration). Any early /
    lease-death / exception / short-duration termination writes
    ``soak-aborted.marker`` (``SOAK_ABORTED``) with the termination reason
    instead. Exactly one of the two markers exists in ``marker_dir`` afterwards
    (the other is removed), so a gate consumer can treat the presence of
    ``soak-complete.marker`` as authoritative proof of a real full-duration
    soak.

    Returns the path of the marker that was written.
    """
    marker_dir = Path(marker_dir)
    marker_dir.mkdir(parents=True, exist_ok=True)
    complete_path = marker_dir / SOAK_COMPLETE_MARKER_NAME
    aborted_path = marker_dir / SOAK_ABORTED_MARKER_NAME
    genuine = bool(completed) and soak_run_completed(intended_s, actual_s)
    if genuine:
        # Clear any stale abort marker from an earlier attempt in this dir.
        aborted_path.unlink(missing_ok=True)
        complete_path.write_text(f"{SOAK_COMPLETE_TOKEN}\n", encoding="utf-8")
        return complete_path
    # Aborted / short / errored: never leave a stale completion marker behind,
    # so a gate cannot false-pass on a marker from a prior full run.
    complete_path.unlink(missing_ok=True)
    lines = [
        SOAK_ABORTED_TOKEN,
        f"reason={reason or 'early_termination'}",
        f"intended_duration_s={intended_s:.3f}",
        f"actual_duration_s={actual_s:.3f}",
        f"cycles={cycles}",
    ]
    aborted_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return aborted_path


async def run_soak(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    duration_s: float, interval_ms: int, window_lines: int,
    mutation_lock: asyncio.Lock,
    marker_dir: "Path | str | None" = None,
) -> None:
    """Sustained streaming memory-drift soak (task 5.5).

    Unlike the cadence/rapid stress phases, this appends one new line per cycle
    and republishes a CONSTANT-SIZE tail-anchored window (the newest
    ``window_lines`` lines). Because the rendered body never changes size, the
    portal scrolls smoothly instead of reflowing — no visible flicker — while
    still exercising the sustained-streaming + bounded-viewport + coalescing
    path for the full duration so memory drift can be measured.
    """
    emit_step_event(transcript, 11, "started", {
        "code": "soak",
        "title": "Sustained streaming soak",
        "action": (
            f"append a line every ~{interval_ms}ms for {duration_s:.0f}s, "
            f"republishing a constant {window_lines}-line tail window"
        ),
        "expected_visual": "portal body scrolls smoothly; no resize/flicker; footer counts up",
    })
    seed = body_full.splitlines() or ["(empty document)"]
    lines: list[str] = list(seed[-window_lines:] if window_lines > 0 else [])
    interval_s = interval_ms / 1000.0
    t0 = time.monotonic()
    next_checkpoint = 60.0
    cycle = 0
    # Tolerate a single transient mutation-ack blip for the duration of the soak
    # (hud-n5bqp); restore fail-fast for other callers in the finally.
    client.configure_mutation_retry(
        SOAK_MUTATION_RETRIES, SOAK_MUTATION_RETRY_BACKOFF_S,
    )
    try:
        try:
            while True:
                cycle_started = time.monotonic()
                elapsed = cycle_started - t0
                if elapsed >= duration_s:
                    break
                cycle += 1
                append_soak_tail_line(lines, seed, cycle, elapsed, window_lines)
                window = bounded_transcript(lines, 0, window_lines)
                await publish_portal(
                    client, lease_id, tiles,
                    title="Exemplar Review Portal",
                    subtitle="sustained streaming soak (task 5.5)",
                    body=window,
                    footer_meta=f"soak  •  cycle {cycle}  •  t+{elapsed:.0f}s / {duration_s:.0f}s",
                    include_tile_setup=(cycle == 1),
                    mutation_lock=mutation_lock,
                )
                if elapsed >= next_checkpoint:
                    emit_step_event(transcript, 11, "checkpoint", {
                        "code": "soak:progress",
                        "title": "Soak progress",
                        "action": f"sustained streaming at cycle {cycle}",
                        "expected_visual": "portal still scrolling smoothly; no flicker",
                    }, cycle=cycle, elapsed_s=round(elapsed, 1), window_lines=window_lines)
                    next_checkpoint = (int(elapsed // 60.0) + 1) * 60.0
                cycle_elapsed_s = time.monotonic() - cycle_started
                await asyncio.sleep(max(0.0, interval_s - cycle_elapsed_s))
        except Exception as exc:
            # Early termination (e.g. lease expired mid-soak, or a mutation-ack
            # wall that outlasted the bounded retry). Record a distinct aborted
            # marker with the reason so no completion marker is left behind, then
            # re-raise so the run still surfaces as a failure to its caller.
            actual_s = time.monotonic() - t0
            reason = f"{type(exc).__name__}: {exc}"
            if marker_dir is not None:
                write_soak_outcome_marker(
                    marker_dir,
                    completed=False,
                    intended_s=duration_s,
                    actual_s=actual_s,
                    cycles=cycle,
                    reason=reason,
                )
            emit_step_event(transcript, 11, "failed", {
                "code": "soak:aborted",
                "title": "Sustained streaming soak aborted",
                "action": f"soak terminated early after {cycle} cycles",
                "expected_visual": "soak did NOT reach full duration; run is a FAIL",
            }, cycles=cycle, duration_s=round(actual_s, 3),
               intended_duration_s=round(duration_s, 3), error=reason)
            raise
        actual_s = time.monotonic() - t0
        emit_step_event(transcript, 11, "completed", {
            "code": "soak",
            "title": "Sustained streaming soak",
            "action": f"completed {cycle} cycles over {actual_s:.0f}s",
            "expected_visual": "portal settled on last tail window",
        }, cycles=cycle, duration_s=round(actual_s, 1))
        if marker_dir is not None:
            write_soak_outcome_marker(
                marker_dir,
                completed=True,
                intended_s=duration_s,
                actual_s=actual_s,
                cycles=cycle,
                reason="",
            )
    finally:
        client.configure_mutation_retry(0)


async def run_composer_smoke(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    transcript: list[dict[str, Any]],
    mutation_lock: asyncio.Lock,
    hold_s: float,
) -> None:
    """Render deterministic composer states for live caret/input review."""
    emit_step_event(transcript, 5, "started", {
        "code": "composer-smoke",
        "title": "Composer caret smoke",
        "action": "render hello-world and long-paste composer states with visible caret",
        "expected_visual": "caret aligns with text after Space and wrapped markdown paste",
    })
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Composer caret and Space smoke",
        body="INPUT pane is under review. OUTPUT pane remains bounded.",
        footer_meta="composer-smoke  •  deterministic live render",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )

    hello = "hello world"
    display_text, cursor_x, cursor_row = await render_composer_static(
        client,
        lease_id,
        tiles.input_scroll,
        hello,
        len(hello),
        focused=True,
        caret_visible=True,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 5, "checkpoint", {
        "code": "composer:hello-world",
        "title": "Composer hello world rendered",
        "action": "rendered normal typing target with Space between words",
        "expected_visual": "hello world appears once, caret after the final d",
    }, cursor_x=cursor_x, cursor_row=cursor_row, visual_lines=len(display_text.splitlines()))
    await asyncio.sleep(min(hold_s, 3.0))

    paste = (
        "**Long markdown paste** near the right edge of the composer line with "
        "words, punctuation, and enough text to wrap several visual rows without "
        "leaving a blank-looking trailing-space caret offset."
    )
    display_text, cursor_x, cursor_row = await render_composer_static(
        client,
        lease_id,
        tiles.input_scroll,
        paste,
        len(paste),
        focused=True,
        caret_visible=True,
        mutation_lock=mutation_lock,
    )
    visual_lines = display_text.splitlines()
    trailing_ws_lines = [
        index for index, line in enumerate(visual_lines[:-1])
        if line.endswith((" ", "\t"))
    ]
    emit_step_event(transcript, 5, "completed", {
        "code": "composer:long-paste",
        "title": "Composer long paste rendered",
        "action": "rendered wrapped markdown-like paste with caret at end",
        "expected_visual": "caret remains on the final wrapped row without a one-character lag at line ends",
    }, cursor_x=cursor_x, cursor_row=cursor_row,
       visual_lines=len(visual_lines), trailing_ws_lines=trailing_ws_lines)
    await asyncio.sleep(hold_s)


async def run_diagnostic_input_phase(
    transcript: list[dict[str, Any]],
    *,
    host: str,
    user: str,
    ssh_key: str,
    portal_x: float,
    portal_y: float,
    tab_width: float,
    tab_height: float,
    timeout_s: float,
    connect_timeout_s: float,
) -> None:
    """Drive focus, drag, and scroll through Windows OS input injection."""
    actions = build_diagnostic_input_plan(
        portal_x,
        portal_y,
        tab_width=tab_width,
        tab_height=tab_height,
    )
    emit_step_event(transcript, 6, "started", {
        "code": "diagnostic-input",
        "title": "Compositor-path diagnostic input",
        "action": "inject OS pointer, wheel, and Unicode input into the live overlay",
        "expected_visual": "composer focuses, portal drags, and OUTPUT pane scrolls via the normal runtime input path",
    }, host=host, user=user, action_labels=[a["label"] for a in actions])
    result = await run_windows_diagnostic_input(
        host,
        user=user,
        ssh_key=ssh_key,
        actions=actions,
        timeout_s=timeout_s,
        connect_timeout_s=connect_timeout_s,
        scene_width=tab_width,
        scene_height=tab_height,
    )
    status = "completed" if result.get("ok") else "failed"
    emit_step_event(transcript, 6, status, {
        "code": "diagnostic-input",
        "title": "Compositor-path diagnostic input",
        "action": "Windows OS input injector finished",
        "expected_visual": "transcript should include input:focus-gained, drag:start/drag:end, and scroll:output checkpoints",
    }, **result)


async def run_resize_hotkey_phase(
    transcript: list[dict[str, Any]],
    *,
    host: str,
    user: str,
    ssh_key: str,
    portal_x: float,
    portal_y: float,
    tab_width: float,
    tab_height: float,
    timeout_s: float,
    connect_timeout_s: float,
    shot_prefix: str,
) -> None:
    """Prove focus-scoped Ctrl +/- portal resize via OS keyboard injection.

    Focuses the portal composer, then injects Ctrl+Equal (grow) and Ctrl+Minus
    (shrink) through the real Windows input path, screenshotting baseline/grow/
    shrink. The PNGs are the operator-visible proof for hud-v4k1h.
    """
    actions = build_resize_hotkey_plan(portal_x, portal_y, shot_prefix)
    emit_step_event(transcript, 6, "started", {
        "code": "resize-hotkey",
        "title": "Focus-scoped Ctrl +/- portal resize",
        "action": "focus composer, then inject Ctrl+Equal (grow) and Ctrl+Minus (shrink) with screenshots",
        "expected_visual": "portal grows on Ctrl+Equal and shrinks on Ctrl+Minus; PNGs capture the change",
    }, host=host, user=user, action_labels=[a["label"] for a in actions])
    result = await run_windows_diagnostic_input(
        host,
        user=user,
        ssh_key=ssh_key,
        actions=actions,
        timeout_s=timeout_s,
        connect_timeout_s=connect_timeout_s,
        scene_width=tab_width,
        scene_height=tab_height,
    )
    status = "completed" if result.get("ok") else "failed"
    emit_step_event(transcript, 6, status, {
        "code": "resize-hotkey",
        "title": "Focus-scoped Ctrl +/- portal resize",
        "action": "Windows OS keyboard injector finished",
        "expected_visual": "stdout lists screenshot paths; baseline→grow→shrink PNGs show the resize",
    }, **result)


# ─── Gate phase runners (task 7.1) ────────────────────────────────────────────

async def run_markdown(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    hold_s: float,
    mutation_lock: asyncio.Lock,
) -> None:
    """Verify that GFM markdown elements render without artefacts in the OUTPUT pane."""
    emit_step_event(transcript, 7, "started", {
        "code": "markdown",
        "title": "Markdown element rendering",
        "action": "publish a body containing headings, bold, italic, code spans, fenced blocks, and lists",
        "expected_visual": "all GFM elements are readable; no escaped asterisks or raw backticks visible",
    })
    md_body = (
        "# Heading 1\n\n"
        "## Heading 2\n\n"
        "Normal paragraph with **bold**, *italic*, `inline code`, and ~~strikethrough~~.\n\n"
        "### Heading 3 — fenced block\n\n"
        "```python\n"
        "def hello(name: str) -> str:\n"
        "    return f'hello {name}'\n"
        "```\n\n"
        "#### Unordered list\n\n"
        "- Alpha item\n"
        "- Beta item\n"
        "  - Nested item\n"
        "- Gamma item\n\n"
        "#### Ordered list\n\n"
        "1. First step\n"
        "2. Second step\n"
        "3. Third step\n\n"
        "> Blockquote: *the runtime owns the pixels.*\n\n"
        "Trailing paragraph to confirm no trailing-whitespace artefacts.\n"
    )
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Markdown element coverage",
        body=md_body,
        footer_meta="markdown  •  GFM element coverage",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )
    elements = [
        "h1", "h2", "h3", "h4",
        "bold", "italic", "inline-code", "strikethrough",
        "fenced-code-block",
        "unordered-list", "nested-list",
        "ordered-list",
        "blockquote",
    ]
    emit_step_event(transcript, 7, "completed", {
        "code": "markdown",
        "title": "Markdown element rendering",
        "action": "hold for operator review of rendered elements",
        "expected_visual": "all markdown elements visually distinct; body text readable; no raw markup leaking",
    }, hold_s=hold_s, elements_under_review=elements)
    await asyncio.sleep(hold_s)


async def run_overflow(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    hold_s: float,
    mutation_lock: asyncio.Lock,
) -> None:
    """Verify OUTPUT pane clamps content at byte/line budget without layout break."""
    emit_step_event(transcript, 8, "started", {
        "code": "overflow",
        "title": "Content overflow clamp",
        "action": "publish progressively larger bodies up to and beyond MAX_MARKDOWN_BYTES",
        "expected_visual": "portal remains intact; oversized body is truncated to byte budget without visual break",
    })

    # Step 1 — just-under budget (700 lines ≈ 70KB, exceeds 65535-byte budget so
    # the trim loop below actually exercises the clamp path)
    near_limit_lines = [
        f"[{i:04d}] overflow test line: {'word ' * 14}end"
        for i in range(700)
    ]
    near_limit_body = "\n".join(near_limit_lines)
    # Trim to stay under MAX_MARKDOWN_BYTES
    while len(near_limit_body.encode("utf-8")) > MAX_MARKDOWN_BYTES:
        near_limit_lines = near_limit_lines[:-1]
        near_limit_body = "\n".join(near_limit_lines)
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Overflow clamp — near limit",
        body=near_limit_body,
        footer_meta=f"overflow:near  •  {len(near_limit_body.encode())} bytes / {MAX_MARKDOWN_BYTES} budget",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 8, "checkpoint", {
        "code": "overflow:near-limit",
        "title": "Near-budget body published",
        "action": "body sized just under MAX_MARKDOWN_BYTES",
        "expected_visual": "full content visible; no visual artefact",
    }, body_bytes=len(near_limit_body.encode()), budget_bytes=MAX_MARKDOWN_BYTES,
       lines=len(near_limit_lines))
    await asyncio.sleep(min(hold_s, 3.0))

    # Step 2 — bounded_transcript clamp (uses the existing scroll budget path)
    all_lines = [
        f"[{i:04d}] {'overflow ' * 8}end" for i in range(500)
    ]
    clamped = bounded_transcript(all_lines, 0, SCROLL_VISIBLE_LINES * 4)
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Overflow clamp — bounded_transcript",
        body=clamped,
        footer_meta=f"overflow:clamped  •  {len(clamped.encode())} bytes",
        include_tile_setup=False,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 8, "completed", {
        "code": "overflow",
        "title": "Content overflow clamp",
        "action": "bounded_transcript clamped to budget; operator verifies no layout break",
        "expected_visual": "clamped content visible; portal chrome intact; no overflow bleed",
    }, clamped_bytes=len(clamped.encode()), budget_bytes=MAX_MARKDOWN_BYTES)
    await asyncio.sleep(hold_s)


async def run_composer_edit(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    transcript: list[dict[str, Any]],
    mutation_lock: asyncio.Lock,
    hold_s: float,
) -> None:
    """Render a sequence of deterministic composer edit states: empty → typed → mid-delete → cleared."""
    emit_step_event(transcript, 9, "started", {
        "code": "composer-edit",
        "title": "Composer edit sequence",
        "action": "render empty → typed → mid-delete → cleared states",
        "expected_visual": "caret tracks correctly through all edit states; no phantom characters",
    })
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Composer edit sequence",
        body="INPUT pane cycling through deterministic edit states.",
        footer_meta="composer-edit  •  deterministic states",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )

    states: list[tuple[str, int, str]] = [
        ("", 0, "empty — placeholder visible"),
        ("Hello", 5, "typed 'Hello' — caret after final l"),
        ("Hello, world!", 13, "typed ', world!' — caret at end"),
        ("Hello, world", 12, "deleted '!' — caret at d"),
        ("Hello", 5, "deleted ', world' — back to 'Hello'"),
        ("Hell", 4, "deleted 'o' — mid-word"),
        ("", 0, "cleared — placeholder re-appears"),
    ]

    total_slept = 0.0
    for idx, (text, cursor, label) in enumerate(states):
        display_text, cursor_x, cursor_row = await render_composer_static(
            client,
            lease_id,
            tiles.input_scroll,
            text,
            cursor,
            focused=True,
            caret_visible=True,
            mutation_lock=mutation_lock,
        )
        status = "completed" if idx == len(states) - 1 else "checkpoint"
        emit_step_event(transcript, 9, status, {
            "code": f"composer-edit:{idx}",
            "title": f"Composer edit state {idx}",
            "action": label,
            "expected_visual": f"composer shows {text!r}; caret at position {cursor}",
        }, cursor_x=cursor_x, cursor_row=cursor_row,
           text_len=len(text), visual_lines=len(display_text.splitlines()))
        step_sleep = min(hold_s / max(1, len(states)), 1.5)
        await asyncio.sleep(step_sleep)
        total_slept += step_sleep

    await asyncio.sleep(max(0.0, hold_s - total_slept))


async def run_cadence(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    cadence_cycles: int,
    cadence_interval_ms: int,
    mutation_lock: asyncio.Lock,
) -> None:
    """Measure cadence overhead: a transport RTT baseline plus per-append
    publish→present timestamps, reporting runtime-added overhead separately from
    transport latency (spec §"runtime overhead beyond transport RTT is bounded
    and evidenced", tasks 5.7)."""
    emit_step_event(transcript, 10, "started", {
        "code": "cadence",
        "title": "Cadence RTT-overhead measurement",
        "action": (
            f"measure transport RTT baseline, then publish {cadence_cycles} sequential "
            f"updates with ~{cadence_interval_ms}ms pacing; record per-append "
            "publish→present timestamps and report runtime overhead separately from RTT"
        ),
        "expected_visual": "portal body updates each cycle; footer shows cycle index and last RTT",
    })
    lines = body_full.splitlines()
    rtt_ms_list: list[float] = []
    interval_s = cadence_interval_ms / 1000.0

    # ── Transport RTT baseline ────────────────────────────────────────────
    # Probe the transport with a few minimal-mutation round-trips before the
    # streaming loop so runtime-added overhead can be reported separately from
    # the transport latency floor.
    baseline_probe_lines = "\n".join(lines[: max(1, min(len(lines), 4))])
    baseline_samples: list[float] = []
    for _ in range(3):
        b0 = time.monotonic()
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle="Cadence RTT baseline probe",
            body=baseline_probe_lines,
            footer_meta="cadence  •  rtt-baseline probe",
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        baseline_samples.append((time.monotonic() - b0) * 1000.0)
    rtt_baseline_ms = min(baseline_samples) if baseline_samples else 0.0
    emit_step_event(transcript, 10, "checkpoint", {
        "code": "cadence:rtt-baseline",
        "title": "Transport RTT baseline measured",
        "action": "probe transport round-trip latency floor before streaming",
        "expected_visual": "no visible change; baseline used to isolate runtime overhead",
    }, rtt_baseline_ms=round(rtt_baseline_ms, 3),
       samples_ms=[round(s, 3) for s in baseline_samples])

    # ── Streaming cadence with per-append publish→present timestamps ───────
    # Only attempt live present-ack correlation when the runtime granted
    # read_telemetry (otherwise no FramePresented is ever delivered — hud-vjlqh).
    # Even when granted, the windowed runtime does not yet emit present-acks
    # (deferred, hud-4va6q): if the first cycle sees none we stop paying the full
    # per-cycle wait so a live windowed run is not stalled ~PRESENT_ACK_TIMEOUT_S
    # every cycle. Headless runs, which do emit, keep correlating throughout.
    present_ack_supported = "read_telemetry" in getattr(
        client, "granted_capabilities", [],
    )
    present_ack_seen = False
    appends: list[dict[str, Any]] = []
    run_t0 = time.monotonic()
    for i in range(cadence_cycles):
        # Alternate body length to stress coalescing path
        end = max(8, len(lines) // 2) if (i % 2 == 0) else len(lines)
        body = "\n".join(lines[:end])

        publish_ms = (time.monotonic() - run_t0) * 1000.0
        t0 = time.monotonic()
        await publish_portal(
            client, lease_id, tiles,
            title="Exemplar Review Portal",
            subtitle="Cadence RTT overhead",
            body=body,
            footer_meta=f"cadence  •  cycle {i + 1}/{cadence_cycles}",
            include_tile_setup=False,
            mutation_lock=mutation_lock,
        )
        # rtt_ms is the transport round-trip: send → mutation-batch ack. This is
        # what publish_portal awaits.
        rtt_ms = (time.monotonic() - t0) * 1000.0
        rtt_ms_list.append(rtt_ms)

        # True present time (hud-vjlqh): correlate the last mutation batch of this
        # publish to its FramePresented present-ack (hud-91uu6) and derive
        # present_ms from the on-screen present wall-clock. This measures the real
        # mutation-arrival→on-screen-present latency instead of the transport-RTT
        # proxy where present≈rtt made the runtime-overhead axis ~vacuous. When no
        # present-ack arrives (windowed runtime — emission deferred, hud-4va6q — or
        # read_telemetry/TELEMETRY_FRAMES not granted) fall back to the proxy: the
        # round-trip completion, by which the runtime has accepted and (latest-wins)
        # presented the coalesced window.
        present_ms = (time.monotonic() - run_t0) * 1000.0
        present_source = "rtt-proxy"
        batch_id = client.last_mutation_batch_id
        if present_ack_supported and batch_id is not None:
            # Full wait while acks are (or may still be) arriving; once the first
            # cycle proves the runtime never emits them, use a near-zero poll for
            # the rest so we still catch any already-queued ack without stalling.
            ack_timeout = (
                PRESENT_ACK_TIMEOUT_S if (present_ack_seen or i == 0) else 0.0
            )
            present_wall_us = await client.wait_for_frame_presented(
                batch_id, timeout=ack_timeout,
            )
            send_wall_us = client.batch_send_wall_us(batch_id)
            if present_wall_us is not None and send_wall_us is not None:
                derived = present_ms_from_frame_ack(
                    publish_ms, send_wall_us, present_wall_us,
                )
                if derived is not None:
                    present_ms = derived
                    present_source = "frame-ack"
                    present_ack_seen = True

        appends.append({
            "cycle": i + 1,
            "body_lines": end,
            "publish_ms": round(publish_ms, 3),
            "present_ms": round(present_ms, 3),
            # "frame-ack" = derived from a live FramePresented present-ack (true
            # present latency); "rtt-proxy" = fell back to the transport round-trip.
            "present_source": present_source,
            # Per-cycle transport RTT used to isolate runtime overhead from
            # transport jitter in build_cadence_rtt_evidence (hud-lod76).
            "rtt_ms": round(rtt_ms, 3),
        })

        emit_step_event(transcript, 10, "checkpoint", {
            "code": f"cadence:cycle:{i}",
            "title": f"Cadence cycle {i + 1}",
            "action": f"publish cycle {i + 1}/{cadence_cycles}",
            "expected_visual": "body updated; footer counter incremented",
        }, rtt_ms=round(rtt_ms, 2), cycle=i + 1, body_lines=end,
           publish_ms=round(publish_ms, 3), present_ms=round(present_ms, 3),
           present_source=present_source)

        if i < cadence_cycles - 1:
            # Sleep minus elapsed, floored at 0 to preserve inter-cycle pacing
            sleep_s = max(0.0, interval_s - (time.monotonic() - t0))
            await asyncio.sleep(sleep_s)

    # Compute summary statistics for RTT-overhead reporting
    if rtt_ms_list:
        rtt_min = min(rtt_ms_list)
        rtt_max = max(rtt_ms_list)
        rtt_mean = sum(rtt_ms_list) / len(rtt_ms_list)
        sorted_rtts = sorted(rtt_ms_list)
        p50_idx = len(sorted_rtts) // 2
        p95_idx = max(0, int(len(sorted_rtts) * 0.95) - 1)
        rtt_p50 = sorted_rtts[p50_idx]
        rtt_p95 = sorted_rtts[p95_idx]
        overhead_budget_ms = cadence_interval_ms
        over_budget = [r for r in rtt_ms_list if r > overhead_budget_ms]
    else:
        rtt_min = rtt_max = rtt_mean = rtt_p50 = rtt_p95 = 0.0
        over_budget = []

    overhead_evidence = build_cadence_rtt_evidence(
        rtt_baseline_ms,
        appends,
        cadence_cycles=cadence_cycles,
        cadence_interval_ms=cadence_interval_ms,
    )

    emit_step_event(transcript, 10, "completed", {
        "code": "cadence",
        "title": "Cadence RTT-overhead measurement",
        "action": "all cycles complete; RTT baseline + runtime-overhead reported",
        "expected_visual": "portal stable; body settled on last cycle content",
    },
        rtt_baseline_ms=round(rtt_baseline_ms, 3),
        rtt_stats={
            "cycles": cadence_cycles,
            "interval_ms": cadence_interval_ms,
            "min_ms": round(rtt_min, 2),
            "max_ms": round(rtt_max, 2),
            "mean_ms": round(rtt_mean, 2),
            "p50_ms": round(rtt_p50, 2),
            "p95_ms": round(rtt_p95, 2),
            "over_budget_count": len(over_budget),
            "over_budget_threshold_ms": cadence_interval_ms,
        },
        overhead_evidence=overhead_evidence,
    )


async def run_profile_swap(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    hold_s: float,
    mutation_lock: asyncio.Lock,
) -> None:
    """Cycle through named visual profiles (compact / standard / expanded).

    Each profile is applied as a real portal-token override map via
    `apply_visual_profile`, so the swap reskins BOTH geometry (portal size) and
    the token-driven visual identity (frame background, transcript/header color,
    typography). The published node values therefore change end-to-end per
    profile — the live proof that the exemplar sources its visuals from resolved
    tokens rather than literals (hud-7jrj3).
    """
    emit_step_event(transcript, 11, "started", {
        "code": "profile-swap",
        "title": "Visual profile swap",
        "action": "cycle compact → standard → expanded portal token profiles (palette + typography + dimensions)",
        "expected_visual": "portal palette AND chrome dimensions shift each cycle; body text remains readable; no layout collapse",
    })

    profiles = profile_swap_dimensions(PORTAL_W, PORTAL_H)

    lines = body_full.splitlines()
    tab_width = tiles.tab_width
    tab_height = tiles.tab_height

    try:
        for idx, (name, pw, ph, title_font, body_font) in enumerate(profiles):
            pw_clamped, ph_clamped = clamp_portal_size(pw, ph, tab_width, tab_height)
            set_portal_size(pw_clamped, ph_clamped, tab_width, tab_height)

            # Reskin: re-resolve the portal tokens from this profile's override
            # map before rebuilding the frame. Every subsequently published
            # color/font is read from the freshly resolved TOKENS.
            resolved = apply_visual_profile(
                profile_swap_overrides(name, title_font, body_font)
            )
            frame_bg = resolved.frame_background
            body_rgba = resolved.transcript_text_color

            body_slice = "\n".join(lines[:min(len(lines), 40)])
            is_last = idx == len(profiles) - 1
            await publish_portal(
                client, lease_id, tiles,
                title="Exemplar Review Portal",
                subtitle=f"Profile: {name}  ({pw_clamped:.0f}×{ph_clamped:.0f}px)",
                body=body_slice,
                footer_meta=f"profile-swap  •  {name}  •  title={title_font}pt body={body_font}pt",
                include_tile_setup=True,
                mutation_lock=mutation_lock,
            )
            status = "completed" if is_last else "checkpoint"
            emit_step_event(transcript, 11, status, {
                "code": f"profile-swap:{name}",
                "title": f"Profile '{name}'",
                "action": f"apply '{name}' portal tokens: {pw_clamped:.0f}×{ph_clamped:.0f}px, "
                          f"title_font={title_font}pt, body_font={body_font}pt, "
                          f"frame_bg=rgba{tuple(round(c, 3) for c in frame_bg)}",
                "expected_visual": f"portal reskins to '{name}' palette + dimensions; content remains readable",
            }, portal_w=pw_clamped, portal_h=ph_clamped,
               title_font_pt=title_font, body_font_pt=body_font,
               operator_evidence=operator_evidence_entry(
                   f"profile-swap:{name}",
                   f"portal reskinned to '{name}' profile; palette AND chrome dimensions "
                   f"shifted (frame + text sourced from resolved tokens) and body text "
                   f"remained readable with no layout collapse",
                   {
                       "profile": name,
                       "portal_w": pw_clamped,
                       "portal_h": ph_clamped,
                       "title_font_pt": title_font,
                       "body_font_pt": body_font,
                       "resolved_frame_background": [round(c, 4) for c in frame_bg],
                       "resolved_transcript_text_color": [round(c, 4) for c in body_rgba],
                   },
               ))
            await asyncio.sleep(hold_s)
    finally:
        # Restore the exemplar profile + canonical portal dimensions.
        apply_visual_profile(None)
        set_portal_size(PORTAL_W, PORTAL_H, tab_width, tab_height)


async def run_window_mgmt(
    client: HudClient, lease_id: bytes, tiles: PortalTiles,
    body_full: str, transcript: list[dict[str, Any]],
    hold_s: float,
    portal_x: float,
    portal_y: float,
    mutation_lock: asyncio.Lock,
) -> None:
    """Exercise portal window management: move, minimize, restore, and boundary clamp."""
    emit_step_event(transcript, 12, "started", {
        "code": "window-mgmt",
        "title": "Window management sequence",
        "action": "move portal → boundary clamp → minimize → restore → return to origin",
        "expected_visual": "portal moves cleanly; minimize icon appears; restore brings full portal back",
    })
    tab_width = tiles.tab_width
    tab_height = tiles.tab_height
    lines = body_full.splitlines()
    body_slice = "\n".join(lines[:min(len(lines), 40)])
    current_x = portal_x
    current_y = portal_y

    # Step 1 — move to centre
    centre_x = max(0.0, min(tab_width / 2.0 - PORTAL_W / 2.0, tab_width - PORTAL_W))
    centre_y = max(0.0, min(tab_height / 2.0 - PORTAL_H / 2.0, tab_height - PORTAL_H))
    async with mutation_lock:
        await client.submit_mutation_batch(
            lease_id,
            portal_bounds_mutations(tiles, centre_x, centre_y),
            timeout=2.0,
        )
    current_x, current_y = centre_x, centre_y
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Window management — centred",
        body=body_slice,
        footer_meta=f"window-mgmt:move  •  ({centre_x:.0f}, {centre_y:.0f})",
        include_tile_setup=False,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 12, "checkpoint", {
        "code": "window-mgmt:move",
        "title": "Portal moved to centre",
        "action": f"portal_x={centre_x:.0f}, portal_y={centre_y:.0f}",
        "expected_visual": "portal repositioned to centre of scene; chrome intact",
    }, portal_x=centre_x, portal_y=centre_y,
       operator_evidence=operator_evidence_entry(
           "window-mgmt:move",
           "portal moved cleanly to scene centre with chrome intact",
           {"portal_x": centre_x, "portal_y": centre_y},
       ))
    await asyncio.sleep(min(hold_s, 2.0))

    # Step 2 — boundary clamp: try to push beyond right/bottom edge
    oob_x = tab_width + 200.0
    oob_y = tab_height + 200.0
    clamped_x = max(0.0, min(oob_x, tab_width - PORTAL_W))
    clamped_y = max(0.0, min(oob_y, tab_height - PORTAL_H))
    async with mutation_lock:
        await client.submit_mutation_batch(
            lease_id,
            portal_bounds_mutations(tiles, clamped_x, clamped_y),
            timeout=2.0,
        )
    current_x, current_y = clamped_x, clamped_y
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Window management — boundary clamped",
        body=body_slice,
        footer_meta=f"window-mgmt:clamp  •  requested({oob_x:.0f},{oob_y:.0f}) → clamped({clamped_x:.0f},{clamped_y:.0f})",
        include_tile_setup=False,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 12, "checkpoint", {
        "code": "window-mgmt:clamp",
        "title": "Boundary clamp verified",
        "action": f"OOB ({oob_x:.0f},{oob_y:.0f}) clamped to ({clamped_x:.0f},{clamped_y:.0f})",
        "expected_visual": "portal visible at bottom-right edge; not partially offscreen",
    }, requested_x=oob_x, requested_y=oob_y, clamped_x=clamped_x, clamped_y=clamped_y,
       operator_evidence=operator_evidence_entry(
           "window-mgmt:clamp",
           "out-of-bounds move clamped to scene edge; portal stays fully on-screen",
           {
               "requested_x": oob_x, "requested_y": oob_y,
               "clamped_x": clamped_x, "clamped_y": clamped_y,
           },
       ))
    await asyncio.sleep(min(hold_s, 2.0))

    # Step 3 — minimize: hide portal tiles, show icon at current position
    hidden_x = max(0.0, tab_width - 1.0)
    hidden_y = max(0.0, tab_height - 1.0)
    async with mutation_lock:
        await client.submit_mutation_batch(
            lease_id,
            [
                publish_to_tile_bounds_mutation(tiles.capture_backstop, hidden_x, hidden_y, 1.0, 1.0),
                publish_to_tile_bounds_mutation(tiles.input_scroll, hidden_x, hidden_y, 1.0, 1.0),
                publish_to_tile_bounds_mutation(tiles.output_scroll, hidden_x, hidden_y, 1.0, 1.0),
                publish_to_tile_bounds_mutation(tiles.drag_shield, hidden_x, hidden_y, 1.0, 1.0),
                publish_to_tile_bounds_mutation(
                    tiles.frame, current_x, current_y, MINIMIZED_ICON_SIZE, MINIMIZED_ICON_SIZE,
                ),
            ],
            timeout=2.0,
        )
        await client.update_tile_opacity(lease_id, tiles.capture_backstop, 0.0)
        await client.update_tile_input_mode(
            lease_id, tiles.capture_backstop, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        for tile_id in (tiles.input_scroll, tiles.output_scroll):
            await client.update_tile_opacity(lease_id, tile_id, 0.0)
            await client.update_tile_input_mode(
                lease_id, tile_id, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
            )
    icon_root, icon_children = build_minimized_icon_nodes(attention=False, pulse=False)
    await set_root_with_children(
        client, lease_id, tiles.frame, icon_root, icon_children, mutation_lock,
    )
    emit_step_event(transcript, 12, "checkpoint", {
        "code": "window-mgmt:minimize",
        "title": "Portal minimized",
        "action": "portal tiles hidden; minimized icon rendered at current position",
        "expected_visual": "circular icon visible; full portal surface hidden",
    }, icon_x=current_x, icon_y=current_y,
       operator_evidence=operator_evidence_entry(
           "window-mgmt:minimize",
           "full portal surface hidden; only the minimized icon remains visible",
           {"icon_x": current_x, "icon_y": current_y},
       ))
    await asyncio.sleep(min(hold_s, 2.0))

    # Step 4 — restore: show full portal at origin position
    restore_x = max(0.0, min(portal_x, tab_width - PORTAL_W))
    restore_y = max(0.0, min(portal_y, tab_height - PORTAL_H))
    async with mutation_lock:
        await client.submit_mutation_batch(
            lease_id,
            portal_bounds_mutations(tiles, restore_x, restore_y),
            timeout=2.0,
        )
        await client.update_tile_opacity(lease_id, tiles.capture_backstop, 0.0)
        await client.update_tile_input_mode(
            lease_id, tiles.capture_backstop, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
        for tile_id in (tiles.frame, tiles.input_scroll, tiles.output_scroll):
            await client.update_tile_opacity(lease_id, tile_id, 1.0)
            await client.update_tile_input_mode(
                lease_id, tile_id, types_pb2.TILE_INPUT_MODE_CAPTURE,
            )
        await client.update_tile_opacity(lease_id, tiles.minimized_icon, 0.0)
        await client.update_tile_input_mode(
            lease_id, tiles.minimized_icon, types_pb2.TILE_INPUT_MODE_PASSTHROUGH,
        )
    await publish_portal(
        client, lease_id, tiles,
        title="Exemplar Review Portal",
        subtitle="Window management — restored",
        body=body_slice,
        footer_meta=f"window-mgmt:restore  •  ({restore_x:.0f}, {restore_y:.0f})",
        include_tile_setup=True,
        mutation_lock=mutation_lock,
    )
    emit_step_event(transcript, 12, "completed", {
        "code": "window-mgmt:restore",
        "title": "Portal restored to origin",
        "action": f"portal restored at ({restore_x:.0f},{restore_y:.0f})",
        "expected_visual": "full portal visible at origin position; icon gone; chrome intact",
    }, restore_x=restore_x, restore_y=restore_y,
       operator_evidence=operator_evidence_entry(
           "window-mgmt:restore",
           "restore brought the full portal back at its origin; minimized icon gone; chrome intact",
           {"restore_x": restore_x, "restore_y": restore_y},
       ))
    await asyncio.sleep(hold_s)


async def run_scenario(args: argparse.Namespace) -> int:
    psk = os.getenv(args.psk_env, "")
    if not psk:
        print(json.dumps({"error": "missing_psk", "psk_env": args.psk_env},
                         sort_keys=True), file=sys.stderr)
        return 2

    body = load_transcript_slice(args.doc, args.max_lines)
    transcript: list[dict[str, Any]] = []
    client = HudClient(
        args.target,
        psk=psk,
        agent_id=args.agent_id,
        # read_telemetry + TELEMETRY_FRAMES let the cadence axis consume live
        # FramePresented present-acks (hud-vjlqh); when the runtime does not grant
        # them (or does not emit present-acks on this path) the cadence axis falls
        # back to the transport-RTT present proxy. Harmless for the other phases.
        capabilities=[
            "create_tiles", "modify_own_tiles", "access_input_events",
            "read_telemetry",
        ],
        initial_subscriptions=[
            "SCENE_TOPOLOGY", "INPUT_EVENTS", "FOCUS_EVENTS", "TELEMETRY_FRAMES",
        ],
    )
    heartbeat_task: Optional[asyncio.Task] = None
    interaction_task: Optional[asyncio.Task] = None
    lease_renewal_task: Optional[asyncio.Task] = None
    lease_id: Optional[bytes] = None
    scene_width = args.tab_width
    scene_height = args.tab_height
    cleanup_errors: list[str] = []
    mutation_lock = asyncio.Lock()

    try:
        emit_step_event(transcript, 0, "started", {
            "code": "scenario",
            "title": "Text Stream Portal live scenario",
            "action": "connect and open resident session",
            "expected_visual": "operator follows JSON step transcript",
        }, target=args.target, doc=args.doc, phases=args.phases)

        await client.connect()
        # Keep this evidence client below the production profile's 30 Hz
        # per-agent mutation envelope while it fans out portal setup batches.
        client.configure_batch_pacing(0.035)
        # hud-16um0: prefer the runtime's resolved portal tokens (delivered on the
        # session handshake) so the runtime's ACTIVE profile drives the live look,
        # instead of resolving against the client-side mirror. Falls back to the
        # local exemplar profile when the runtime does not expose them (older
        # runtime) or when explicitly ignored.
        runtime_tokens = {} if args.ignore_runtime_tokens else client.resolved_portal_tokens
        adopted_runtime_tokens = adopt_runtime_tokens(runtime_tokens)
        token_source = "runtime-handshake" if adopted_runtime_tokens else (
            "local-mirror-ignored" if args.ignore_runtime_tokens else "local-mirror-fallback"
        )
        emit_step_event(transcript, 0, "checkpoint", {
            "code": "tokens:source",
            "title": "Portal design-token source resolved",
            "action": (
                "adopt runtime-delivered resolved PortalPartTokens as the live "
                "source when the handshake exposes them; else keep the local mirror"
            ),
            "expected_visual": "portal renders the runtime's active-profile look",
        }, token_source=token_source, runtime_token_count=len(client.resolved_portal_tokens))
        scene_width, scene_height = client.scene_display_area or (scene_width, scene_height)
        emit_step_event(transcript, 0, "checkpoint", {
            "code": "scene:display-area",
            "title": "Scene display area resolved",
            "action": "use live scene dimensions for portal placement and drag bounds",
            "expected_visual": "portal can be dragged across the full HUD surface",
        }, scene_width=scene_width, scene_height=scene_height)
        lease_ttl_ms = scenario_lease_ttl_ms(
            args.phases, args.baseline_hold_s, args.soak_duration_s,
        )
        lease_id = await client.request_lease(ttl_ms=lease_ttl_ms)
        # Keep the lease alive for the entire session. Sustained phases (soak,
        # baseline hold, cadence, interaction) can outlast a single TTL; renewal
        # prevents mid-run "lease expired" self-termination (hud-hk8kl).
        lease_renewal_task = asyncio.create_task(
            lease_renewal_loop(client, lease_id, client.last_granted_lease_ttl_ms)
        )
        default_w, default_h = default_portal_size(scene_width, scene_height)
        set_portal_size(
            args.portal_width if args.portal_width is not None else default_w,
            args.portal_height if args.portal_height is not None else default_h,
            scene_width,
            scene_height,
        )
        emit_step_event(transcript, 0, "checkpoint", {
            "code": "portal:size",
            "title": "Portal size resolved",
            "action": "scale portal defaults from live scene dimensions unless explicit size overrides were provided",
            "expected_visual": "portal occupies a readable portion of the detected display",
        }, portal_w=PORTAL_W, portal_h=PORTAL_H,
           explicit_width=args.portal_width is not None,
           explicit_height=args.portal_height is not None)
        portal_x = (
            args.portal_x
            if args.portal_x is not None
            else scene_width - PORTAL_W - PORTAL_X_FROM_RIGHT
        )
        portal_x = max(0.0, min(portal_x, max(0.0, scene_width - PORTAL_W)))
        portal_y = max(0.0, min(PORTAL_Y, max(0.0, scene_height - PORTAL_H)))
        tiles = await create_portal_tiles(
            client=client,
            lease_id=lease_id,
            portal_x=portal_x,
            portal_y=portal_y,
            tab_width=scene_width,
            tab_height=scene_height,
        )
        heartbeat_interval_ms = client.heartbeat_interval_ms or 5_000
        heartbeat_task = asyncio.create_task(
            heartbeat_loop(client, heartbeat_interval_ms)
        )
        interaction_task = asyncio.create_task(
            portal_interaction_loop(
                client=client,
                lease_id=lease_id,
                tiles=tiles,
                transcript=transcript,
                body_full=body,
                initial_portal_x=portal_x,
                initial_portal_y=portal_y,
                tab_width=scene_width,
                tab_height=scene_height,
                mutation_lock=mutation_lock,
                clipboard_host=target_host(args.target),
                clipboard_user=args.clipboard_user,
                clipboard_ssh_key=args.clipboard_ssh_key,
                clipboard_timeout_s=args.clipboard_timeout_s,
            )
        )

        phases = scenario_phase_names(args.phases or "baseline")
        for phase in phases:
            if phase == "baseline":
                await run_baseline(
                    client, lease_id, tiles, body, transcript,
                    args.baseline_hold_s, mutation_lock,
                )
            elif phase == "scroll":
                await run_scroll(client, lease_id, tiles, transcript, mutation_lock)
            elif phase == "streaming":
                await run_streaming(
                    client, lease_id, tiles, body, transcript,
                    args.stream_chunks, args.stream_interval_s, mutation_lock,
                )
            elif phase == "rapid":
                await run_rapid(
                    client, lease_id, tiles, body, transcript,
                    args.rapid_cycles, args.rapid_interval_ms, mutation_lock,
                )
            elif phase == "soak":
                await run_soak(
                    client, lease_id, tiles, body, transcript,
                    args.soak_duration_s, args.soak_interval_ms,
                    args.soak_window_lines, mutation_lock,
                    marker_dir=args.soak_marker_dir,
                )
            elif phase == "composer-smoke":
                await run_composer_smoke(
                    client, lease_id, tiles, transcript, mutation_lock,
                    args.composer_smoke_hold_s,
                )
            elif phase == "diagnostic-input":
                await publish_portal(
                    client, lease_id, tiles,
                    title="Exemplar Review Portal",
                    subtitle="Compositor-path diagnostic input",
                    body=body,
                    footer_meta="diagnostic-input  •  OS input injector armed",
                    include_tile_setup=True,
                    mutation_lock=mutation_lock,
                )
                await run_diagnostic_input_phase(
                    transcript,
                    host=target_host(args.target),
                    user=args.diagnostic_input_user,
                    ssh_key=args.diagnostic_input_ssh_key,
                    portal_x=portal_x,
                    portal_y=portal_y,
                    tab_width=scene_width,
                    tab_height=scene_height,
                    timeout_s=args.diagnostic_input_timeout_s,
                    connect_timeout_s=args.diagnostic_input_connect_timeout_s,
                )
            elif phase == "resize-hotkey":
                await publish_portal(
                    client, lease_id, tiles,
                    title="Exemplar Review Portal",
                    subtitle="Focus-scoped Ctrl +/- resize",
                    body=body,
                    footer_meta="resize-hotkey  •  Ctrl +/- injector armed",
                    include_tile_setup=True,
                    mutation_lock=mutation_lock,
                )
                await run_resize_hotkey_phase(
                    transcript,
                    host=target_host(args.target),
                    user=args.diagnostic_input_user,
                    ssh_key=args.diagnostic_input_ssh_key,
                    portal_x=portal_x,
                    portal_y=portal_y,
                    tab_width=scene_width,
                    tab_height=scene_height,
                    timeout_s=args.diagnostic_input_timeout_s,
                    connect_timeout_s=args.diagnostic_input_connect_timeout_s,
                    shot_prefix=args.resize_shot_prefix,
                )
            elif phase == "markdown":
                await run_markdown(
                    client, lease_id, tiles, body,
                    transcript, args.markdown_hold_s, mutation_lock,
                )
            elif phase == "overflow":
                await run_overflow(
                    client, lease_id, tiles, body,
                    transcript, args.overflow_hold_s, mutation_lock,
                )
            elif phase == "composer-edit":
                await run_composer_edit(
                    client, lease_id, tiles,
                    transcript, mutation_lock, args.composer_edit_hold_s,
                )
            elif phase == "cadence":
                await run_cadence(
                    client, lease_id, tiles, body,
                    transcript,
                    args.cadence_cycles,
                    args.cadence_interval_ms,
                    mutation_lock,
                )
            elif phase == "profile-swap":
                await run_profile_swap(
                    client, lease_id, tiles, body,
                    transcript, args.profile_swap_hold_s, mutation_lock,
                )
            elif phase == "window-mgmt":
                await run_window_mgmt(
                    client, lease_id, tiles, body,
                    transcript, args.window_mgmt_hold_s,
                    portal_x, portal_y, mutation_lock,
                )
            else:
                emit_step_event(transcript, -1, "skipped", {
                    "code": f"unknown:{phase}",
                    "title": f"unknown phase {phase!r}",
                    "action": "no-op",
                    "expected_visual": "—",
                })

        emit_step_event(transcript, 99, "completed", {
            "code": "scenario_complete",
            "title": "Text Stream Portal scenario complete",
            "action": "review transcript and capture UX notes",
            "expected_visual": "portal visible until cleanup releases the lease",
        })
    finally:
        if lease_renewal_task is not None:
            lease_renewal_task.cancel()
            try:
                await lease_renewal_task
            except asyncio.CancelledError:
                pass
            except Exception as exc:
                cleanup_errors.append(
                    f"lease_renewal_task: {type(exc).__name__}: {exc}"
                )
        if heartbeat_task is not None:
            heartbeat_task.cancel()
            try:
                await heartbeat_task
            except asyncio.CancelledError:
                pass
        if interaction_task is not None:
            interaction_task.cancel()
            try:
                await interaction_task
            except asyncio.CancelledError:
                pass
            except Exception as exc:
                detail = f"{type(exc).__name__}: {exc}"
                cleanup_errors.append(f"interaction_task: {detail}")
                emit_step_event(transcript, 98, "failed", {
                    "code": "interaction:task-error",
                    "title": "Portal interaction loop failed",
                    "action": "continue cleanup despite interaction task failure",
                    "expected_visual": "portal lease cleanup still runs",
                }, error=detail)
        if lease_id is not None and not args.leave_lease_on_exit:
            try:
                await asyncio.wait_for(
                    client.release_lease(lease_id),
                    timeout=args.cleanup_timeout_s,
                )
                emit_step_event(transcript, 100, "completed", {
                    "code": "cleanup:lease-release",
                    "title": "Portal lease released",
                    "action": "release the lease before closing the session",
                    "expected_visual": "all portal tiles are removed from the HUD",
                })
            except Exception as exc:
                detail = f"{type(exc).__name__}: {exc}"
                cleanup_errors.append(detail)
                emit_step_event(transcript, 100, "failed", {
                    "code": "cleanup:lease-release",
                    "title": "Portal lease release failed",
                    "action": "attempted to release the portal lease before session close",
                    "expected_visual": "portal may remain until runtime orphan cleanup or HUD restart",
                }, error=detail)
        try:
            await client.close(reason="portal-exemplar done")
        except Exception:
            pass
        if args.transcript_out:
            hardware_tag = reference_hardware_tag(
                tag=args.reference_hardware_tag,
                hostname=args.reference_hostname,
                gpu=args.reference_gpu,
                gpu_driver=args.reference_gpu_driver,
                target=args.target,
            )
            write_transcript(args.transcript_out, build_evidence_artifact(
                target=args.target,
                doc=args.doc,
                phases=args.phases,
                scene_width=scene_width,
                scene_height=scene_height,
                portal_w=PORTAL_W,
                portal_h=PORTAL_H,
                lease_release_on_exit=not args.leave_lease_on_exit,
                cleanup_errors=cleanup_errors,
                steps=transcript,
                hardware_tag=hardware_tag,
            ))

    return 0


def run_composer_self_test() -> int:
    width = composer_wrap_area_width_px()
    failures: list[str] = []

    expected_doc = ".claude/skills/user-test/references/exemplar-manual-review-checklist.md"
    stale_doc = "docs/" + "exemplar-manual-review-checklist.md"
    if DEFAULT_DOC != expected_doc:
        failures.append(f"DEFAULT_DOC is {DEFAULT_DOC!r}, expected {expected_doc!r}")
    if not Path(DEFAULT_DOC).is_file():
        failures.append(f"DEFAULT_DOC path does not exist: {DEFAULT_DOC}")
    script_source = Path(__file__).read_text(encoding="utf-8")
    if stale_doc in script_source:
        failures.append(f"stale checklist path remains in script source: {stale_doc}")
    if TITLE_FONT < 18.0:
        failures.append(f"TITLE_FONT={TITLE_FONT} is below the readable default floor 18.0")
    if BODY_FONT < 16.0:
        failures.append(f"BODY_FONT={BODY_FONT} is below the readable default floor 16.0")
    if INPUT_FONT < 16.0:
        failures.append(f"INPUT_FONT={INPUT_FONT} is below the readable default floor 16.0")
    if SCROLL_LINE_PX < BODY_FONT * 1.35:
        failures.append(
            f"SCROLL_LINE_PX={SCROLL_LINE_PX} is too tight for BODY_FONT={BODY_FONT}"
        )

    fallback = composer_key_fallback_text("Space")
    if fallback != " ":
        failures.append(f"Space fallback returned {fallback!r}")
    if composer_key_fallback_text("A") is not None:
        failures.append("non-Space printable key-down produced fallback text")

    hello_display, hello_x, hello_row = composer_wrapped_layout(
        "hello world",
        len("hello world"),
        width,
    )
    if hello_display != "hello world" or hello_row != 0:
        failures.append(
            f"hello world wrapped unexpectedly: row={hello_row}, text={hello_display!r}"
        )
    expected_hello_x = len("hello world") * COMPOSER_CARET_CHAR_W
    if abs(hello_x - expected_hello_x) > 0.01:
        failures.append(f"hello world caret x={hello_x:.2f}, expected {expected_hello_x:.2f}")

    paste = (
        "**Long markdown paste** near the right edge of the composer line with "
        "words, punctuation, and enough text to wrap several visual rows without "
        "leaving a blank-looking trailing-space caret offset."
    )
    display, cursor_x, cursor_row = composer_wrapped_layout(paste, len(paste), width)
    lines = display.splitlines()
    for row, line in enumerate(lines[:-1]):
        if line.endswith((" ", "\t")):
            failures.append(f"wrapped line {row} ends with whitespace: {line!r}")
    if not (0 <= cursor_row < len(lines)):
        failures.append(f"caret row {cursor_row} outside {len(lines)} visual lines")
    if cursor_x > width + 0.01:
        failures.append(f"caret x={cursor_x:.2f} exceeds wrap width {width:.2f}")

    long_word = "x" * int(width // COMPOSER_WRAP_CHAR_W + 8)
    _, long_x, long_row = composer_wrapped_layout(long_word, len(long_word), width)
    if long_row == 0:
        failures.append("long unbroken word did not wrap")
    if long_x > width + 0.01:
        failures.append(f"long-word caret x={long_x:.2f} exceeds wrap width {width:.2f}")

    _, input_children = build_input_scroll_nodes("typed text that should live in fixed line nodes")
    expected_composer_children = len(COMPOSER_INPUT_CHILD_KEYS)
    if len(input_children) != expected_composer_children:
        failures.append(
            "composer input tile must mount clear background, hit region, fixed per-line text nodes, and caret; "
            f"got {len(input_children)} children, expected {expected_composer_children}"
        )
    clear_node = input_children[0]
    if not clear_node.HasField("solid_color"):
        failures.append("composer input tile must draw a clear/background node before text")
    hit_node = input_children[1]
    composer_rect = input_composer_local_rect()
    if not hit_node.HasField("hit_region"):
        failures.append("composer input tile must mount a hit region after the clear background")
    else:
        hit_bounds = hit_node.hit_region.bounds
        if (
            abs(hit_bounds.x - composer_rect.x) > 0.01
            or abs(hit_bounds.y - composer_rect.y) > 0.01
            or abs(hit_bounds.width - composer_rect.w) > 0.01
            or abs(hit_bounds.height - composer_rect.h) > 0.01
        ):
            failures.append("composer hit region must match the visible composer box")
    line_nodes = input_children[2:2 + COMPOSER_VISIBLE_LINE_NODES]
    if len(line_nodes) == COMPOSER_VISIBLE_LINE_NODES:
        first_line_y = line_nodes[0].text_markdown.bounds.y
        for index, line_node in enumerate(line_nodes):
            if line_node.text_markdown.content == "":
                failures.append(
                    f"composer line {index} uses empty content; blank lines must still invalidate stale text"
                )
            if line_node.text_markdown.overflow != types_pb2.TEXT_OVERFLOW_PROTO_CLIP:
                failures.append(
                    f"composer line {index} must explicitly use Clip overflow"
                )
            expected_y = first_line_y + index * COMPOSER_LINE_PX
            actual_y = line_node.text_markdown.bounds.y
            if abs(actual_y - expected_y) > 0.01:
                failures.append(
                    f"composer line {index} y={actual_y:.2f}, expected {expected_y:.2f}"
                )
    long_window = composer_line_window(
        paste * 2,
        len(paste * 2),
        focused=True,
    )
    if len(long_window.lines) != COMPOSER_VISIBLE_LINE_NODES:
        failures.append(
            f"composer visible window has {len(long_window.lines)} lines, "
            f"expected {COMPOSER_VISIBLE_LINE_NODES}"
        )
    if long_window.cursor_row >= COMPOSER_VISIBLE_LINE_NODES and long_window.start_row <= 0:
        failures.append("composer long-text window did not tail-anchor around the cursor")
    caret_node = build_composer_caret_node(
        "typed",
        len("typed"),
        focused=True,
        caret_visible=True,
        node_id=b"c" * 16,
    )
    line_ids = [bytes([index + 1]) * 16 for index in range(COMPOSER_VISIBLE_LINE_NODES)]
    mutations = composer_update_mutations(
        b"i" * 16,
        line_ids,
        b"c" * 16,
        line_nodes,
        caret_node,
    )
    if len(mutations) != COMPOSER_VISIBLE_LINE_NODES + 1:
        failures.append(
            f"composer update batch contains {len(mutations)} mutations, "
            f"expected {COMPOSER_VISIBLE_LINE_NODES + 1}"
        )
    else:
        for index, mutation in enumerate(mutations[:-1]):
            line_mut = mutation.update_node_content
            if line_mut.tile_id != b"i" * 16 or line_mut.node_id != line_ids[index]:
                failures.append(f"composer line {index} update targets the wrong tile/node")
        caret_mut = mutations[-1].update_node_content
        if caret_mut.tile_id != b"i" * 16 or caret_mut.node_id != b"c" * 16:
            failures.append("composer caret update mutation targets the wrong tile/node")

    result = {
        "status": "failed" if failures else "passed",
        "wrap_width_px": width,
        "char_width_px": COMPOSER_WRAP_CHAR_W,
        "failures": failures,
        "cases": {
            "hello_world": {
                "display": hello_display,
                "cursor_x": hello_x,
                "cursor_row": hello_row,
            },
            "long_markdown_paste": {
                "visual_lines": len(lines),
                "cursor_x": cursor_x,
                "cursor_row": cursor_row,
            },
            "long_word": {
                "cursor_x": long_x,
                "cursor_row": long_row,
            },
        },
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1 if failures else 0


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Run the Text Stream Portal live resident gRPC scenario."
    )
    p.add_argument(
        "--self-test",
        action="store_true",
        help="Run local composer wrap/caret/input smoke checks without opening gRPC",
    )
    p.add_argument("--target", default=DEFAULT_TARGET)
    p.add_argument("--psk-env", default=DEFAULT_PSK_ENV)
    p.add_argument("--agent-id", default="agent-alpha")
    p.add_argument(
        "--doc",
        default=DEFAULT_DOC,
        help=f"Markdown document to render (default: {DEFAULT_DOC})",
    )
    p.add_argument("--max-lines", type=int, default=120)
    p.add_argument("--tab-width", type=float, default=1920.0)
    p.add_argument("--tab-height", type=float, default=1080.0)
    p.add_argument("--portal-x", type=float, default=None)
    p.add_argument(
        "--portal-width",
        type=float,
        default=None,
        help="Override responsive portal width in scene pixels",
    )
    p.add_argument(
        "--portal-height",
        type=float,
        default=None,
        help="Override responsive portal height in scene pixels",
    )
    p.add_argument(
        "--phases",
        default="baseline,scroll",
        help=(
            "Comma-separated list of phases to run: "
            "baseline, scroll, streaming, rapid, soak, composer-smoke, diagnostic-input, "
            "markdown, overflow, composer-edit, cadence, profile-swap, window-mgmt"
        ),
    )
    p.add_argument(
        "--ignore-runtime-tokens",
        action="store_true",
        help=(
            "Do not adopt the runtime-resolved portal tokens delivered on the "
            "session handshake (hud-16um0); resolve against the local exemplar "
            "profile mirror instead. By default, when the runtime exposes its "
            "active-profile tokens, they drive the live portal look."
        ),
    )
    p.add_argument("--baseline-hold-s", type=float, default=20.0)
    p.add_argument("--composer-smoke-hold-s", type=float, default=8.0)
    p.add_argument("--stream-chunks", type=int, default=6)
    p.add_argument("--stream-interval-s", type=float, default=1.5)
    p.add_argument("--rapid-cycles", type=int, default=12)
    p.add_argument("--rapid-interval-ms", type=int, default=80)
    # Sustained streaming soak (task 5.5): constant-size tail window, no flicker.
    p.add_argument("--soak-duration-s", type=float, default=3600.0,
                   help="Total soak duration in seconds for the 'soak' phase")
    p.add_argument("--soak-interval-ms", type=int, default=250,
                   help="Inter-append pacing in ms for the 'soak' phase")
    p.add_argument("--soak-window-lines", type=int, default=60,
                   help="Constant tail-window line count republished each soak cycle")
    p.add_argument("--soak-marker-dir", default=None,
                   help="Directory in which the harness writes the authoritative soak "
                        "outcome marker: soak-complete.marker (SOAK_COMPLETE) ONLY on a "
                        "genuine full-duration completion, or soak-aborted.marker "
                        "(SOAK_ABORTED, with the termination reason) on early / lease-death "
                        "/ exception termination. Omit to write no marker.")
    p.add_argument("--cleanup-timeout-s", type=float, default=5.0)
    p.add_argument("--clipboard-user", default="admin-user")
    p.add_argument("--clipboard-ssh-key", default=DEFAULT_SSH_KEY)
    p.add_argument("--clipboard-timeout-s", type=float, default=0.7)
    p.add_argument("--diagnostic-input-user", default="admin-user")
    p.add_argument("--diagnostic-input-ssh-key", default=DEFAULT_SSH_KEY)
    p.add_argument("--diagnostic-input-timeout-s", type=float, default=12.0)
    p.add_argument("--diagnostic-input-connect-timeout-s", type=float, default=5.0)
    p.add_argument(
        "--resize-shot-prefix",
        default="C:\\tze_hud\\resize-hotkey",
        help="Windows path prefix for resize-hotkey phase screenshots "
        "(<prefix>-baseline.png / -grow.png / -shrink.png)",
    )
    # Gate phase args (task 7.1)
    p.add_argument(
        "--markdown-hold-s",
        type=float,
        default=12.0,
        help="Seconds to hold the markdown phase for operator review",
    )
    p.add_argument(
        "--overflow-hold-s",
        type=float,
        default=6.0,
        help="Seconds to hold each overflow step for operator review",
    )
    p.add_argument(
        "--composer-edit-hold-s",
        type=float,
        default=6.0,
        help="Total hold budget for the composer-edit sequence (divided across states)",
    )
    p.add_argument(
        "--cadence-cycles",
        type=int,
        default=20,
        help="Number of publish cycles in the cadence RTT measurement phase",
    )
    p.add_argument(
        "--cadence-interval-ms",
        type=int,
        default=100,
        help="Target inter-cycle pacing in ms for the cadence phase; also used as RTT budget threshold",
    )
    p.add_argument(
        "--profile-swap-hold-s",
        type=float,
        default=4.0,
        help="Seconds to hold each visual profile during the profile-swap phase",
    )
    p.add_argument(
        "--window-mgmt-hold-s",
        type=float,
        default=3.0,
        help="Seconds to hold each window-mgmt step for operator review",
    )
    p.add_argument(
        "--leave-lease-on-exit",
        action="store_true",
        help="Skip explicit lease release on exit; only use when testing orphan/grace behavior",
    )
    # Promotion-gate reference-hardware tag (engineering-bar §2). Defaults to the
    # canonical reference host; override on non-default hardware so the artifact
    # records the true tag (off-reference runs are informational-only per gate).
    p.add_argument(
        "--reference-hardware-tag",
        default=None,
        help="Engineering-bar reference hardware tag for the evidence artifact (default: TzeHouse)",
    )
    p.add_argument(
        "--reference-hostname",
        default=None,
        help=(
            "Reference host identity the target is matched against to set "
            "is_reference (default: the canonical engineering-bar reference host). "
            "The local collection hostname is recorded separately and never sets "
            "reference status."
        ),
    )
    p.add_argument(
        "--reference-gpu",
        default=None,
        help="Reference GPU model for the evidence artifact",
    )
    p.add_argument(
        "--reference-gpu-driver",
        default=None,
        help="Reference GPU driver version for the evidence artifact",
    )
    p.add_argument("--transcript-out", default=DEFAULT_TRANSCRIPT_PATH)
    return p.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_composer_self_test()
    try:
        return asyncio.run(run_scenario(args))
    except KeyboardInterrupt:
        print(json.dumps({"error": "interrupted"}), file=sys.stderr)
        return 130
    except Exception as exc:
        print(json.dumps({"error": "exception", "detail": str(exc)}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
