#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Notification stack exemplar user-test scenario.

Exercises the notification-area zone on a live deployed HUD by publishing
notification bursts from 3 simulated agents (alpha, beta, gamma) with mixed
urgency levels. Validates vertical stacking, urgency-tinted backdrops,
TTL auto-dismiss, and max_depth eviction.

4 phases:
  1. Initial burst  — 3 notifications (urgency 0, 1, 2) from alpha/beta/gamma
                      + 2s pause for visual inspection
  2. Stack growth   — 2 more notifications (urgency 3, 1) to reach max_depth=5
                      + 2s pause for visual inspection
  3. TTL expiry     — wait ~(ttl + 150ms fade-out) for first batch to expire
                      + 1s pause for visual confirmation of shrinkage
  4. Max depth      — 6 rapid notifications to trigger eviction
                      + 3s pause for visual inspection

Usage:
  notification_exemplar.py --url http://host:9090
  notification_exemplar.py --url http://host:9090 --psk-env MY_PSK --ttl 8000
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request
from typing import Any

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DEFAULT_PSK_ENV = "TZE_HUD_PSK"
DEFAULT_TTL_MS = 8000

ZONE_NAME = "notification-area"

# Simulated agent labels (log output only; the publisher is the PSK's agent)
AGENTS = {
    "alpha": "alpha",
    "beta": "beta",
    "gamma": "gamma",
}

# Urgency labels for display
URGENCY_LABELS = {
    0: "low",
    1: "normal",
    2: "urgent",
    3: "critical",
}

# ---------------------------------------------------------------------------
# MCP RPC helper (standard MCP tools/call)
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
# Publish helper
# ---------------------------------------------------------------------------


def publish_notification(
    url: str,
    token: str,
    req_id: int,
    agent: str,
    text: str,
    icon: str,
    urgency: int,
    ttl_ms: int,
) -> dict[str, Any]:
    """Publish a single NotificationPayload to the notification-area zone."""
    content: dict[str, Any] = {
        "type": "notification",
        "text": text,
        "icon": icon,
        "urgency": urgency,
    }
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": content,
        "ttl_ms": ttl_ms,
    }
    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    label = URGENCY_LABELS.get(urgency, f"urgency={urgency}")
    status = "ok" if ok else f"ERR: {response.get('error')}"
    print(
        f"  [{agent:6s}] urgency={urgency} ({label:8s}) | {text!r:40s} | {status}",
        flush=True,
    )
    return response


# ---------------------------------------------------------------------------
# Phases
# ---------------------------------------------------------------------------


def phase1_initial_burst(
    url: str, token: str, ttl_ms: int, req_id_start: int
) -> int:
    """
    Phase 1: Initial burst — 3 notifications from alpha/beta/gamma.
    Urgency 0 (low), 1 (normal), 2 (urgent).
    Pause 2s for visual inspection.
    """
    print("\n--- Phase 1: Initial burst (3 agents, urgency 0/1/2) ---", flush=True)
    req_id = req_id_start

    notifications = [
        ("alpha", "System idle",      "",       0),
        ("beta",  "Update available", "update", 1),
        ("gamma", "High CPU usage",   "alert",  2),
    ]

    for agent, text, icon, urgency in notifications:
        publish_notification(
            url, token, req_id,
            agent=agent,
            text=text,
            icon=icon,
            urgency=urgency,
            ttl_ms=ttl_ms,
        )
        req_id += 1

    print(
        "\n  [visual check] 3 notifications should appear stacked:"
        "\n    - gamma (urgent/amber-black) at top"
        "\n    - beta  (normal/blue-black) in middle"
        "\n    - alpha (low/smoke-black)   at bottom",
        flush=True,
    )
    print("  Pausing 2s for visual inspection...", flush=True)
    time.sleep(2)
    return req_id


def phase2_stack_growth(
    url: str, token: str, ttl_ms: int, req_id_start: int
) -> int:
    """
    Phase 2: Stack growth — 2 more notifications to reach max_depth=5.
    Urgency 3 (critical) and 1 (normal).
    Pause 2s for visual inspection.
    """
    print(
        "\n--- Phase 2: Stack growth (2 more notifications, max_depth=5) ---",
        flush=True,
    )
    req_id = req_id_start

    notifications = [
        ("alpha", "Security alert",  "shield", 3),
        ("beta",  "Deploy complete", "check",  1),
    ]

    for agent, text, icon, urgency in notifications:
        publish_notification(
            url, token, req_id,
            agent=agent,
            text=text,
            icon=icon,
            urgency=urgency,
            ttl_ms=ttl_ms,
        )
        req_id += 1

    print(
        "\n  [visual check] Stack should now show 5 notifications:"
        "\n    - beta  (normal/blue-black) at top    (newest)"
        "\n    - alpha (critical/red-black) slot 1"
        "\n    - gamma (urgent/amber-black) slot 2"
        "\n    - beta  (normal/blue-black) slot 3"
        "\n    - alpha (low/smoke-black)   at bottom (oldest)",
        flush=True,
    )
    print("  Pausing 2s for visual inspection...", flush=True)
    time.sleep(2)
    return req_id


def phase3_ttl_expiry(ttl_ms: int, phase1_start: float) -> None:
    """
    Phase 3: TTL expiry — wait for the first batch to auto-dismiss.
    Computes remaining TTL from phase1_start timestamp so we wait only the
    time left until phase-1 notifications expire, plus 150ms fade-out + 500ms margin.
    Pause 1s after for visual confirmation.
    """
    elapsed_since_phase1_ms = (time.monotonic() - phase1_start) * 1000.0
    remaining_ms = max(0.0, ttl_ms - elapsed_since_phase1_ms)
    wait_ms = int(remaining_ms) + 150 + 500
    wait_s = wait_ms / 1000.0

    print(
        f"\n--- Phase 3: TTL expiry (waiting {wait_ms}ms for first batch to dismiss) ---",
        flush=True,
    )
    print(
        f"  Elapsed since phase 1: {elapsed_since_phase1_ms:.0f}ms"
        f"  Remaining TTL: {remaining_ms:.0f}ms + 150ms fade-out + 500ms margin = {wait_ms}ms total wait",
        flush=True,
    )

    # Show a countdown in 1s steps so the operator sees progress
    elapsed = 0.0
    while elapsed < wait_s:
        remaining = wait_s - elapsed
        step = min(1.0, remaining)
        time.sleep(step)
        elapsed += step
        print(
            f"  waiting... {elapsed:.0f}s / {wait_s:.0f}s", flush=True
        )

    print(
        "\n  [visual check] Notifications from phase 1 (alpha urgency=0, beta urgency=1,"
        "\n   gamma urgency=2) should have faded out. Stack should show 2 remaining"
        "\n   (the phase 2 notifications: alpha urgency=3 and beta urgency=1).",
        flush=True,
    )
    print("  Pausing 1s for visual confirmation...", flush=True)
    time.sleep(1)


def phase4_max_depth_eviction(
    url: str, token: str, ttl_ms: int, req_id_start: int
) -> int:
    """
    Phase 4: Max depth eviction — 6 rapid notifications.
    The 6th publication MUST evict the oldest (1st of this burst).
    Pause 3s for visual inspection.
    """
    print(
        "\n--- Phase 4: Max depth eviction (6 rapid notifications) ---",
        flush=True,
    )
    req_id = req_id_start

    # 6 notifications cycling agents and urgency levels
    burst: list[tuple[str, str, str, int]] = [
        ("alpha", "Burst A1 — eviction target", "",      0),
        ("beta",  "Burst B2",                   "",      1),
        ("gamma", "Burst C3",                   "",      2),
        ("alpha", "Burst A4",                   "",      3),
        ("beta",  "Burst B5",                   "",      1),
        ("gamma", "Burst C6 — newest (top)",    "bell",  0),
    ]

    for agent, text, icon, urgency in burst:
        publish_notification(
            url, token, req_id,
            agent=agent,
            text=text,
            icon=icon,
            urgency=urgency,
            ttl_ms=ttl_ms,
        )
        req_id += 1

    print(
        "\n  [visual check] Stack should show exactly 5 notifications (max_depth):"
        "\n    - gamma 'Burst C6' at top       (newest)"
        "\n    - beta  'Burst B5' slot 1"
        "\n    - alpha 'Burst A4' slot 2"
        "\n    - gamma 'Burst C3' slot 3"
        "\n    - beta  'Burst B2' at bottom    (oldest surviving)"
        "\n    NOTE: alpha 'Burst A1' was evicted (no fade-out, instant removal).",
        flush=True,
    )
    print("  Pausing 3s for visual inspection...", flush=True)
    time.sleep(3)
    return req_id


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Notification stack exemplar user-test: publishes multi-agent notification"
            " bursts to a live HUD, exercising stacking, urgency tinting, TTL"
            " auto-dismiss, and max-depth eviction."
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--url",
        required=True,
        help="MCP HTTP URL of the running HUD (e.g. http://host:9090)",
    )
    parser.add_argument(
        "--psk-env",
        default=DEFAULT_PSK_ENV,
        help=(
            f"Environment variable containing the pre-shared key"
            f" (default: {DEFAULT_PSK_ENV})"
        ),
    )
    parser.add_argument(
        "--ttl",
        type=int,
        default=DEFAULT_TTL_MS,
        metavar="MS",
        help=(
            f"Per-notification TTL in milliseconds (default: {DEFAULT_TTL_MS})."
            " Phase 3 waits until the initial batch reaches its TTL"
            " (accounting for time already spent in phases 1-2, plus 150ms"
            " fade-out and 500ms margin) for auto-dismiss confirmation."
        ),
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    token = os.getenv(args.psk_env, "")
    if not token:
        print(
            f"ERROR: environment variable {args.psk_env} is empty or unset",
            file=sys.stderr,
        )
        return 2

    ttl_ms: int = args.ttl
    url: str = args.url

    print("Notification Exemplar User-Test", flush=True)
    print(f"  HUD URL : {url}", flush=True)
    print(f"  PSK env : {args.psk_env}", flush=True)
    print(f"  TTL     : {ttl_ms}ms per notification", flush=True)
    print(f"  Zone    : {ZONE_NAME}", flush=True)
    print(f"  Agents  : {', '.join(AGENTS.keys())}", flush=True)

    req_id = 10

    try:
        # Phase 1: Initial burst (3 notifications, urgency 0/1/2)
        phase1_start = time.monotonic()
        req_id = phase1_initial_burst(url, token, ttl_ms, req_id)

        # Phase 2: Stack growth (2 more, reach max_depth=5)
        req_id = phase2_stack_growth(url, token, ttl_ms, req_id)

        # Phase 3: Wait for TTL expiry of phase 1 batch
        phase3_ttl_expiry(ttl_ms, phase1_start)

        # Phase 4: Max depth eviction burst (6 notifications)
        req_id = phase4_max_depth_eviction(url, token, ttl_ms, req_id)

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

    print(
        "\n--- All phases complete ---",
        flush=True,
    )
    print(
        "  Summary of what was exercised:"
        "\n    [phase 1] Vertical stacking (3 agents, urgency 0/1/2)"
        "\n    [phase 2] Stack growth to max_depth=5 (urgency 3/1)"
        "\n    [phase 3] TTL auto-dismiss with 150ms fade-out (phase 1 batch expired)"
        "\n    [phase 4] Max-depth eviction: 6 rapid publishes -> oldest evicted instantly",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
