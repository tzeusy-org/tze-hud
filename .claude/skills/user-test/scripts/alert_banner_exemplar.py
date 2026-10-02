#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Alert-banner exemplar user-test scenario.

Exercises the alert-banner zone on a live deployed HUD by publishing 3 alerts
at sequential urgency levels with 3-second delays between each.

Sequence:
  1. Info     (urgency=1) — "Info: system nominal"             + 3s pause
  2. Warning  (urgency=2) — "Warning: disk space low"          + 3s pause
  3. Critical (urgency=3) — "CRITICAL: security breach detected"

Expected visual after all 3 publishes:
  - Critical (red)   at top
  - Warning  (amber) in middle
  - Info     (blue)  at bottom
  All three visible simultaneously until TTL expires.

Usage:
  alert_banner_exemplar.py --url http://host:9090
  alert_banner_exemplar.py --url http://host:9090 --psk-env MY_PSK --ttl 15000
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
DEFAULT_TTL_MS = 15000

ZONE_NAME = "alert-banner"

# Delay between sequential publishes, in seconds
INTER_PUBLISH_DELAY_S = 3.0

# Urgency labels for display
URGENCY_LABELS = {
    0: "background",
    1: "info",
    2: "warning",
    3: "critical",
}

# Alert definitions: (label, text, urgency)
ALERTS: list[tuple[str, str, int]] = [
    ("info",     "Info: system nominal",               1),
    ("warning",  "Warning: disk space low",            2),
    ("critical", "CRITICAL: security breach detected", 3),
]

# ---------------------------------------------------------------------------
# MCP RPC helper (same framing as publish_zone_batch.py)
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


def publish_alert(
    url: str,
    token: str,
    req_id: int,
    label: str,
    text: str,
    urgency: int,
    ttl_ms: int,
) -> dict[str, Any]:
    """Publish a single notification to the alert-banner zone."""
    content: dict[str, Any] = {
        "type": "notification",
        "text": text,
        "icon": "",
        "urgency": urgency,
    }
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": content,
        "ttl_ms": ttl_ms,
    }
    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    urgency_name = URGENCY_LABELS.get(urgency, f"urgency={urgency}")
    status = "ok" if ok else f"ERR: {response.get('error')}"
    print(
        f"  [{label:8s}] urgency={urgency} ({urgency_name:10s}) | {text!r:45s} | {status}",
        flush=True,
    )
    return response


# ---------------------------------------------------------------------------
# Sequence
# ---------------------------------------------------------------------------


def run_sequence(url: str, token: str, ttl_ms: int) -> bool:
    """
    Publish the 3 alerts with inter-publish delays.

    Returns True if all publishes succeeded, False if any failed.
    """
    print(
        "\n--- Publishing alert sequence (info -> warning -> critical) ---",
        flush=True,
    )
    req_id = 10
    any_failed = False

    for idx, (label, text, urgency) in enumerate(ALERTS):
        response = publish_alert(
            url=url,
            token=token,
            req_id=req_id,
            label=label,
            text=text,
            urgency=urgency,
            ttl_ms=ttl_ms,
        )
        if "error" in response:
            any_failed = True
        req_id += 1

        # Pause between publishes (skip after the last one)
        if idx < len(ALERTS) - 1:
            print(
                f"  Pausing {INTER_PUBLISH_DELAY_S:.0f}s before next alert...",
                flush=True,
            )
            time.sleep(INTER_PUBLISH_DELAY_S)

    return not any_failed


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Alert-banner exemplar user-test: publishes 3 alerts at increasing"
            " urgency levels (info, warning, critical) to a live HUD with 3s"
            " delays between each publish."
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
            f"Per-alert TTL in milliseconds (default: {DEFAULT_TTL_MS})."
            " All three alerts are published within ~6s, so TTL should be"
            " long enough for all to be visible simultaneously."
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

    print("Alert-Banner Exemplar User-Test", flush=True)
    print(f"  HUD URL : {url}", flush=True)
    print(f"  PSK env : {args.psk_env}", flush=True)
    print(f"  TTL     : {ttl_ms}ms per alert", flush=True)
    print(f"  Zone    : {ZONE_NAME}", flush=True)
    print(f"  Alerts  : {len(ALERTS)} (info, warning, critical)", flush=True)
    print(f"  Delay   : {INTER_PUBLISH_DELAY_S:.0f}s between publishes", flush=True)

    try:
        success = run_sequence(url, token, ttl_ms)
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
        "\n--- Sequence complete ---",
        flush=True,
    )
    print(
        "\n  [visual check] All 3 alerts should be visible simultaneously:"
        "\n    - CRITICAL (red)   at top    -- 'CRITICAL: security breach detected'"
        "\n    - Warning  (amber) in middle -- 'Warning: disk space low'"
        "\n    - Info     (blue)  at bottom -- 'Info: system nominal'"
        "\n"
        "\n  All alerts expire after TTL elapses.",
        flush=True,
    )

    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
