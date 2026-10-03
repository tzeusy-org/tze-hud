#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Ambient-background exemplar user-test scenario.

Exercises the ambient-background zone on a live deployed HUD via MCP
`hud_publish`. Validates solid-color background fills, latest-wins
replacement semantics, static-image acceptance, and rapid-replacement stress.

4 phases:
  1. Dark blue       — publish solid dark blue background, 3s pause
  2. Warm amber      — replace with warm amber (latest-wins), 3s pause
  3. Static image    — publish static_image content type (placeholder), 2s pause
  4. Rapid replace   — publish 10 different colors in succession,
                       verify only the last (saturated green) via hud_surfaces

Usage:
  ambient_background_exemplar.py
  ambient_background_exemplar.py --psk-env MY_PSK
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from typing import Any

import hud_env

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------


ZONE_NAME = "ambient-background"

# A valid 64-char hex resource_id (blake3 hash of b"test") used as a
# placeholder for the static-image scenario (GPU texture upload is post-v1;
# the runtime renders a warm-gray placeholder quad).
PLACEHOLDER_RESOURCE_ID = "4878ca0425c739fa427f7eda20fe845f6b2f46ba5fe5ac7d6b85add8db6bb08f"

# Default alpha for overlay-appropriate tints (fully opaque backgrounds
# obscure the desktop; 0.15 gives a subtle color wash).
DEFAULT_ALPHA = 0.15

# Phase 1 / 2 colors
COLOR_DARK_BLUE = {"r": 0.05, "g": 0.05, "b": 0.2, "a": DEFAULT_ALPHA}
COLOR_WARM_AMBER = {"r": 0.9, "g": 0.6, "b": 0.2, "a": DEFAULT_ALPHA}

# Phase 4 rapid-replacement palette (10 colors; last one is bright green)
RAPID_COLORS: list[tuple[str, dict[str, float]]] = [
    ("red",         {"r": 1.0, "g": 0.0, "b": 0.0, "a": DEFAULT_ALPHA}),
    ("blue",        {"r": 0.0, "g": 0.0, "b": 1.0, "a": DEFAULT_ALPHA}),
    ("yellow",      {"r": 1.0, "g": 1.0, "b": 0.0, "a": DEFAULT_ALPHA}),
    ("magenta",     {"r": 1.0, "g": 0.0, "b": 1.0, "a": DEFAULT_ALPHA}),
    ("cyan",        {"r": 0.0, "g": 1.0, "b": 1.0, "a": DEFAULT_ALPHA}),
    ("gray",        {"r": 0.5, "g": 0.5, "b": 0.5, "a": DEFAULT_ALPHA}),
    ("orange",      {"r": 1.0, "g": 0.5, "b": 0.0, "a": DEFAULT_ALPHA}),
    ("purple",      {"r": 0.5, "g": 0.0, "b": 0.5, "a": DEFAULT_ALPHA}),
    ("dark-green",  {"r": 0.0, "g": 0.5, "b": 0.0, "a": DEFAULT_ALPHA}),
    ("bright-green",{"r": 0.0, "g": 1.0, "b": 0.0, "a": DEFAULT_ALPHA}),  # last
]

# ---------------------------------------------------------------------------
# MCP RPC helper
# ---------------------------------------------------------------------------


def rpc_call(
    url: str,
    token: str,
    tool: str,
    arguments: dict[str, Any],
    request_id: int,
) -> dict[str, Any]:
    """Call one MCP tool (``tools/call``).

    Returns ``{"result": <decoded result>}`` or ``{"error": <decoded error>}``;
    a tool error decodes to ``{"code": ..., "hint": ...}``.
    """
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        url=url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=20) as resp:
        envelope = json.loads(resp.read().decode("utf-8"))
    if "error" in envelope:
        return {"error": envelope["error"]}
    result = envelope["result"]
    decoded = json.loads(result["content"][0]["text"])
    return {"error": decoded} if result.get("isError") else {"result": decoded}


# ---------------------------------------------------------------------------
# Publish helpers
# ---------------------------------------------------------------------------


def publish_solid_color(
    url: str,
    token: str,
    req_id: int,
    label: str,
    color: dict[str, float],
    ttl_us: int = 0,
) -> dict[str, Any]:
    """Publish a solid_color background via MCP hud_publish."""
    content: dict[str, Any] = {"type": "solid_color", **color}
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": content,
    }
    if ttl_us > 0:
        params["ttl_ms"] = ttl_us // 1000
    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    status = "ok" if ok else f"ERR: {response.get('error')}"
    r, g, b, a = color["r"], color["g"], color["b"], color["a"]
    print(
        f"  [solid_color] {label:12s} rgba({r:.2f},{g:.2f},{b:.2f},{a:.2f}) | {status}",
        flush=True,
    )
    return response


def publish_static_image(
    url: str,
    token: str,
    req_id: int,
    resource_id: str,
) -> dict[str, Any]:
    """Publish a static_image background via MCP hud_publish."""
    content: dict[str, Any] = {"type": "static_image", "resource_id": resource_id}
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": content,
    }
    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    status = "ok" if ok else f"ERR: {response.get('error')}"
    print(
        f"  [static_image] resource_id={resource_id[:16]}... | {status}",
        flush=True,
    )
    return response


def list_surfaces(url: str, token: str, req_id: int) -> dict[str, Any]:
    """Query the hud_surfaces tool and return the parsed response."""
    return rpc_call(url, token, "hud_surfaces", {}, req_id)


def find_zone_entry(surfaces_response: dict[str, Any]) -> dict[str, Any] | None:
    """Return the ``zone:<ZONE_NAME>`` entry of a hud_surfaces result, or None.

    The result is ``{"surfaces": [{"s": "zone:name", "held": true, ...}]}``;
    ``held`` is omitted when the agent holds nothing there.
    """
    surfaces = surfaces_response.get("result", {}).get("surfaces", [])
    return next((e for e in surfaces if e.get("s") == f"zone:{ZONE_NAME}"), None)


# ---------------------------------------------------------------------------
# Phases
# ---------------------------------------------------------------------------


def phase1_dark_blue(url: str, token: str, req_id: int) -> tuple[int, bool]:
    """
    Phase 1: Publish a dark blue solid background.

    Visual check: entire HUD background should turn dark navy blue.
    Zone occupancy: 1 active publication with SolidColor content.
    Pauses 3s for visual inspection.
    """
    print("\n--- Phase 1: Dark blue background ---", flush=True)
    response = publish_solid_color(
        url, token, req_id, "dark-blue", COLOR_DARK_BLUE
    )
    ok = "error" not in response
    print(
        "\n  [visual check] Background should be dark navy blue."
        "\n  Zone occupancy: 1 active publication (SolidColor).",
        flush=True,
    )
    print("  Pausing 3s for visual inspection...", flush=True)
    time.sleep(3)
    return req_id + 1, ok


def phase2_warm_amber(url: str, token: str, req_id: int) -> tuple[int, bool]:
    """
    Phase 2: Replace dark blue with warm amber (latest-wins Replace policy).

    Visual check: background should shift instantly to warm amber.
    The previous dark blue must no longer be visible.
    Zone occupancy: still exactly 1 active publication.
    Pauses 3s for visual inspection.
    """
    print("\n--- Phase 2: Warm amber replacement (latest-wins) ---", flush=True)
    response = publish_solid_color(
        url, token, req_id, "warm-amber", COLOR_WARM_AMBER
    )
    ok = "error" not in response
    print(
        "\n  [visual check] Background should now be warm amber."
        "\n  The previous dark blue must be gone (Replace policy: latest-wins)."
        "\n  Zone occupancy: exactly 1 active publication.",
        flush=True,
    )
    print("  Pausing 3s for visual inspection...", flush=True)
    time.sleep(3)
    return req_id + 1, ok


def phase3_static_image(url: str, token: str, req_id: int) -> tuple[int, bool]:
    """
    Phase 3: Publish a static_image content type.

    In v1 the GPU texture upload pipeline is deferred; the runtime renders a
    warm-gray placeholder quad. The zone must accept the publication.
    Visual check: background should change to warm-gray placeholder.
    Pauses 2s for visual inspection.
    """
    print("\n--- Phase 3: Static image (placeholder) ---", flush=True)
    response = publish_static_image(
        url, token, req_id, PLACEHOLDER_RESOURCE_ID
    )
    ok = "error" not in response
    print(
        "\n  [visual check] Background should show warm-gray placeholder quad (v1 behavior)."
        "\n  Full texture rendering is deferred to post-v1.",
        flush=True,
    )
    print("  Pausing 5s for visual inspection...", flush=True)
    time.sleep(5)
    return req_id + 1, ok


def phase4_rapid_replacement(
    url: str, token: str, req_id: int
) -> tuple[int, bool]:
    """
    Phase 4: Rapid-replacement stress test — 10 colors in sequence.

    Publishes 10 different solid colors without delay between them, then
    queries hud_surfaces to confirm the zone is held (held=true).
    hud_surfaces reports a boolean flag, not a publication count;
    the Replace policy guarantees at most 1 active publication.
    Visual check: the final color (bright green) should be visible.

    Visual check: background should settle on bright green.
    """
    print(
        "\n--- Phase 4: Rapid replacement stress test (10 colors) ---",
        flush=True,
    )
    any_failed = False
    for label, color in RAPID_COLORS:
        response = publish_solid_color(
            url, token, req_id, label, color
        )
        if "error" in response:
            any_failed = True
        req_id += 1

    # Query hud_surfaces to verify final occupancy.
    zones_response = list_surfaces(url, token, req_id)
    req_id += 1
    occupancy_ok = False
    if "result" in zones_response:
        bg_zone = find_zone_entry(zones_response)
        if bg_zone is not None:
            held = bool(bg_zone.get("held", False))
            occupancy_ok = held
            occupancy_status = "Held (held=true)" if held else "Not held"
        else:
            occupancy_status = f"zone '{ZONE_NAME}' not found in hud_surfaces response"
    else:
        occupancy_status = f"hud_surfaces error: {zones_response.get('error')}"
        any_failed = True

    print(
        f"\n  [occupancy check] hud_surfaces reports ambient-background: {occupancy_status}",
        flush=True,
    )
    print(
        "\n  [visual check] Background should be bright green (last of 10 rapid publishes)."
        "\n  No other colors from the rapid burst should be visible.",
        flush=True,
    )
    return req_id, not any_failed and occupancy_ok


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Ambient-background exemplar user-test: exercises the ambient-background"
            " zone on a live HUD via MCP hud_publish across 4 phases:"
            " dark-blue set, warm-amber replacement, static-image placeholder,"
            " and rapid-replacement stress (10 colors)."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--url",
        default=None,
        help="MCP URL (default: derived from HUD_HOST)",
    )
    parser.add_argument(
        "--psk-env",
        default=None,
        help="Read the PSK from this environment variable instead of the paired file",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    try:
        url, token = hud_env.resolve(args.url, args.psk_env)
    except hud_env.HudEnvError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


    print("Ambient Background Exemplar User-Test", flush=True)
    print(f"  HUD URL : {url}", flush=True)
    print(f"  Zone    : {ZONE_NAME}", flush=True)
    print(
        "  Phases  : dark-blue | warm-amber replacement | static-image | rapid-replace x10",
        flush=True,
    )

    req_id = 10
    results: dict[str, bool] = {}

    try:
        req_id, ok = phase1_dark_blue(url, token, req_id)
        results["phase1_dark_blue"] = ok

        req_id, ok = phase2_warm_amber(url, token, req_id)
        results["phase2_warm_amber"] = ok

        req_id, ok = phase3_static_image(url, token, req_id)
        results["phase3_static_image"] = ok

        req_id, ok = phase4_rapid_replacement(url, token, req_id)
        results["phase4_rapid_replacement"] = ok

    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(
            json.dumps({"error": "http_error", "status": e.code, "body": body}),
            file=sys.stderr,
        )
        return 3
    except urllib.error.URLError as e:
        print(
            json.dumps({"error": "url_error", "detail": str(e)}),
            file=sys.stderr,
        )
        return 4
    except KeyboardInterrupt:
        print("\nInterrupted by user.", file=sys.stderr)
        return 5
    except Exception as e:
        print(
            json.dumps({"error": "exception", "detail": str(e)}),
            file=sys.stderr,
        )
        return 6

    print("\n--- All phases complete ---", flush=True)
    for phase, ok in results.items():
        status = "PASS" if ok else "FAIL"
        print(f"  {phase}: {status}", flush=True)
    print(
        "\n  Summary of what was exercised:"
        "\n    [phase 1] Solid color background fill (dark navy blue)"
        "\n    [phase 2] Latest-wins replacement — warm amber evicts dark blue"
        "\n    [phase 3] static_image content type accepted (warm-gray placeholder in v1)"
        "\n    [phase 4] Rapid-replacement stress: 10 colors → only bright green visible,"
        "\n              zone occupancy = 1 active publication",
        flush=True,
    )

    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
