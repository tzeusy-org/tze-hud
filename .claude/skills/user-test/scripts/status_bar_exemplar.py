#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Status-bar exemplar user-test scenario.

Exercises the status-bar zone on a live deployed HUD by simulating three
independent agents (agent-weather, agent-power, agent-clock) publishing
key-value entries with distinct merge keys. Validates merge-by-key contention,
multi-agent coexistence, key replacement, empty-value removal, and TTL expiry.

10-step sequence (as defined in spec §User-Test Scenario):
  Step 1  — agent-weather publishes merge_key "weather" → "72F Sunny"
  Step 2  — agent-power   publishes merge_key "battery" → "85%"
  Step 3  — agent-clock   publishes merge_key "time"    → "3:42 PM"
  Step 4  — VISUAL CHECK: all 3 key-value pairs visible in the status bar
  Step 5  — agent-weather updates merge_key "weather" → "75F Cloudy"
  Step 6  — VISUAL CHECK: weather updated, battery and time unchanged
  Step 7  — agent-weather publishes empty value for merge_key "weather"
  Step 8  — VISUAL CHECK: weather gone, battery and time remain
  Step 9  — wait for agent-power TTL to expire (~15s default + margin)
  Step 10 — VISUAL CHECK: battery gone, time remains

Distinct namespaces: "agent-weather", "agent-power", "agent-clock".

Usage:
  status_bar_exemplar.py --url http://host:9090
  status_bar_exemplar.py --url http://host:9090 --psk-env MY_PSK --battery-ttl 15000
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

# TTL for weather and clock entries — long enough to survive the whole sequence
DEFAULT_LONG_TTL_MS = 60_000   # 60 seconds

# TTL for the battery entry — must survive the three 3-second visual checks at
# steps 4, 6, and 8 (9s of pauses) plus execution time between steps 2 and 8
# (~2-3s), so a minimum of ~12s elapses before step 9 begins. 15s provides
# enough margin to be alive through step 8 while still expiring during step 9.
DEFAULT_BATTERY_TTL_MS = 15_000  # 15 seconds

# Pause after each visual-check step so the operator can inspect the display
VISUAL_PAUSE_S = 3.0

ZONE_NAME = "status-bar"

# Agent namespaces, as required by the spec
NS_WEATHER = "agent-weather"
NS_POWER = "agent-power"
NS_CLOCK = "agent-clock"

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
# Publish helper
# ---------------------------------------------------------------------------


def publish_status_entry(
    url: str,
    token: str,
    req_id: int,
    namespace: str,
    merge_key: str,
    entry_key: str,
    entry_value: str,
    ttl_ms: int,
) -> dict[str, Any]:
    """
    Publish a single StatusBarPayload entry to the status-bar zone.

    Uses the canonical MCP shape:
      content = {"type": "status_bar", "entries": {entry_key: entry_value}}
    with merge_key and namespace set for agent-level isolation.
    """
    content: dict[str, Any] = {
        "type": "status_bar",
        "entries": {entry_key: entry_value},
    }
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": content,
        "key": merge_key,
        "ttl_ms": ttl_ms,
    }
    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    value_display = repr(entry_value) if entry_value else "(empty — removal)"
    status = "ok" if ok else f"ERR: {response.get('error')}"
    print(
        f"  [{namespace:14s}] merge_key={merge_key!r:10s} "
        f"{entry_key}={value_display:20s} | {status}",
        flush=True,
    )
    return response


# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------


def step1_weather_initial(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """Step 1: Agent A (agent-weather) publishes merge_key 'weather' → '72F Sunny'."""
    print("\n--- Step 1: agent-weather publishes 'weather' → '72F Sunny' ---", flush=True)
    publish_status_entry(
        url, token, req_id,
        namespace=NS_WEATHER,
        merge_key="weather",
        entry_key="weather",
        entry_value="72F Sunny",
        ttl_ms=ttl_ms,
    )
    return req_id + 1


def step2_battery_initial(url: str, token: str, battery_ttl_ms: int, req_id: int) -> int:
    """Step 2: Agent B (agent-power) publishes merge_key 'battery' → '85%'."""
    print("\n--- Step 2: agent-power publishes 'battery' → '85%' ---", flush=True)
    publish_status_entry(
        url, token, req_id,
        namespace=NS_POWER,
        merge_key="battery",
        entry_key="battery",
        entry_value="85%",
        ttl_ms=battery_ttl_ms,
    )
    return req_id + 1


def step3_time_initial(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """Step 3: Agent C (agent-clock) publishes merge_key 'time' → '3:42 PM'."""
    print("\n--- Step 3: agent-clock publishes 'time' → '3:42 PM' ---", flush=True)
    publish_status_entry(
        url, token, req_id,
        namespace=NS_CLOCK,
        merge_key="time",
        entry_key="time",
        entry_value="3:42 PM",
        ttl_ms=ttl_ms,
    )
    return req_id + 1


def step4_visual_check_all_visible() -> None:
    """
    Step 4: VISUAL CHECK — all 3 key-value pairs should be visible simultaneously.
    """
    print(
        "\n--- Step 4: VISUAL CHECK ---",
        flush=True,
    )
    print(
        "  [expected] Status bar shows all 3 key-value pairs simultaneously:"
        "\n    weather: 72F Sunny"
        "\n    battery: 85%"
        "\n    time: 3:42 PM"
        "\n  All entries displayed in horizontal row with monospace font."
        f"\n  Pausing {VISUAL_PAUSE_S:.0f}s for visual inspection...",
        flush=True,
    )
    time.sleep(VISUAL_PAUSE_S)


def step5_weather_update(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """Step 5: Agent A updates merge_key 'weather' → '75F Cloudy' (key replacement)."""
    print(
        "\n--- Step 5: agent-weather updates 'weather' → '75F Cloudy' (key replacement) ---",
        flush=True,
    )
    publish_status_entry(
        url, token, req_id,
        namespace=NS_WEATHER,
        merge_key="weather",
        entry_key="weather",
        entry_value="75F Cloudy",
        ttl_ms=ttl_ms,
    )
    return req_id + 1


def step6_visual_check_weather_updated() -> None:
    """
    Step 6: VISUAL CHECK — weather updated; battery and time unchanged.
    """
    print(
        "\n--- Step 6: VISUAL CHECK ---",
        flush=True,
    )
    print(
        "  [expected] Status bar still shows 3 entries; weather value replaced:"
        "\n    weather: 75F Cloudy  ← UPDATED (was '72F Sunny')"
        "\n    battery: 85%         ← unchanged"
        "\n    time: 3:42 PM        ← unchanged"
        f"\n  Pausing {VISUAL_PAUSE_S:.0f}s for visual inspection...",
        flush=True,
    )
    time.sleep(VISUAL_PAUSE_S)


def step7_weather_empty(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """
    Step 7: Agent A publishes empty value for merge_key 'weather'.
    Empty-value convention causes compositor to skip rendering this entry.
    """
    print(
        "\n--- Step 7: agent-weather publishes empty value for 'weather' (key removal) ---",
        flush=True,
    )
    publish_status_entry(
        url, token, req_id,
        namespace=NS_WEATHER,
        merge_key="weather",
        entry_key="weather",
        entry_value="",
        ttl_ms=ttl_ms,
    )
    return req_id + 1


def step8_visual_check_weather_gone() -> None:
    """
    Step 8: VISUAL CHECK — weather key gone; battery and time remain.
    """
    print(
        "\n--- Step 8: VISUAL CHECK ---",
        flush=True,
    )
    print(
        "  [expected] Status bar shows 2 entries; weather key is no longer visible:"
        "\n    battery: 85%     ← remains"
        "\n    time: 3:42 PM    ← remains"
        "\n    weather          ← GONE (empty-value removal)"
        f"\n  Pausing {VISUAL_PAUSE_S:.0f}s for visual inspection...",
        flush=True,
    )
    time.sleep(VISUAL_PAUSE_S)


def step9_wait_battery_ttl(battery_ttl_ms: int, battery_publish_time: float) -> None:
    """
    Step 9: Wait for agent-power TTL to expire.

    Computes remaining TTL from battery_publish_time so we wait only what's
    left, plus 500ms margin for sweep_expired_zone_publications to run.
    """
    elapsed_ms = (time.monotonic() - battery_publish_time) * 1000.0
    remaining_ms = max(0.0, battery_ttl_ms - elapsed_ms)
    wait_ms = int(remaining_ms) + 500  # 500ms sweep margin
    wait_s = wait_ms / 1000.0

    print(
        f"\n--- Step 9: Waiting {wait_ms}ms for agent-power TTL to expire ---",
        flush=True,
    )
    print(
        f"  battery TTL:          {battery_ttl_ms}ms"
        f"\n  elapsed since publish: {elapsed_ms:.0f}ms"
        f"\n  remaining:            {remaining_ms:.0f}ms + 500ms margin = {wait_ms}ms",
        flush=True,
    )

    elapsed = 0.0
    while elapsed < wait_s:
        remaining = wait_s - elapsed
        step = min(1.0, remaining)
        time.sleep(step)
        elapsed += step
        print(f"  waiting... {elapsed:.0f}s / {wait_s:.0f}s", flush=True)


def step10_visual_check_battery_gone() -> None:
    """
    Step 10: VISUAL CHECK — battery gone via TTL expiry; time remains.
    """
    print(
        "\n--- Step 10: VISUAL CHECK ---",
        flush=True,
    )
    print(
        "  [expected] Status bar shows 1 entry; battery expired, time remains:"
        "\n    time: 3:42 PM    ← remains"
        "\n    battery          ← GONE (TTL expiry via sweep_expired_zone_publications)"
        "\n    weather          ← GONE (empty-value removal, from step 7)"
        f"\n  Pausing {VISUAL_PAUSE_S:.0f}s for visual inspection...",
        flush=True,
    )
    time.sleep(VISUAL_PAUSE_S)


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Status-bar exemplar user-test: simulates three independent agents"
            " (agent-weather, agent-power, agent-clock) publishing merge-keyed"
            " entries to a live HUD, exercising key coexistence, value"
            " replacement, empty-value removal, and TTL expiry."
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
        default=DEFAULT_LONG_TTL_MS,
        metavar="MS",
        help=(
            f"TTL in milliseconds for weather and clock entries"
            f" (default: {DEFAULT_LONG_TTL_MS}). Should be long enough to"
            " survive the full sequence."
        ),
    )
    parser.add_argument(
        "--battery-ttl",
        type=int,
        default=DEFAULT_BATTERY_TTL_MS,
        metavar="MS",
        help=(
            f"TTL in milliseconds for the battery entry (default:"
            f" {DEFAULT_BATTERY_TTL_MS}). Must be long enough to survive the"
            " three visual-check pauses at steps 4, 6, and 8 (9s total at"
            " default VISUAL_PAUSE_S=3s) but short enough to expire during"
            " step 9. Step 9 waits for remaining TTL to demonstrate key"
            " removal via sweep_expired_zone_publications."
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
    battery_ttl_ms: int = args.battery_ttl
    url: str = args.url

    print("Status-Bar Exemplar User-Test", flush=True)
    print(f"  HUD URL     : {url}", flush=True)
    print(f"  PSK env     : {args.psk_env}", flush=True)
    print(f"  Zone        : {ZONE_NAME}", flush=True)
    print(f"  Agents      : {NS_WEATHER}, {NS_POWER}, {NS_CLOCK}", flush=True)
    print(f"  TTL (long)  : {ttl_ms}ms  (weather, time)", flush=True)
    print(f"  TTL (short) : {battery_ttl_ms}ms (battery — expires in step 9)", flush=True)
    print(f"  Visual pause: {VISUAL_PAUSE_S:.0f}s after each visual-check step", flush=True)

    req_id = 10

    try:
        # Steps 1-3: Initial publishes from all three agents
        req_id = step1_weather_initial(url, token, ttl_ms, req_id)
        req_id = step2_battery_initial(url, token, battery_ttl_ms, req_id)
        battery_publish_time = time.monotonic()
        req_id = step3_time_initial(url, token, ttl_ms, req_id)

        # Step 4: Visual check — all 3 visible
        step4_visual_check_all_visible()

        # Step 5: Weather key replacement
        req_id = step5_weather_update(url, token, ttl_ms, req_id)

        # Step 6: Visual check — weather updated, others unchanged
        step6_visual_check_weather_updated()

        # Step 7: Weather empty-value removal
        req_id = step7_weather_empty(url, token, ttl_ms, req_id)

        # Step 8: Visual check — weather gone, others remain
        step8_visual_check_weather_gone()

        # Step 9: Wait for battery TTL expiry
        step9_wait_battery_ttl(battery_ttl_ms, battery_publish_time)

        # Step 10: Visual check — battery gone, time remains
        step10_visual_check_battery_gone()

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
        "\n--- All 10 steps complete ---",
        flush=True,
    )
    print(
        "  Summary of what was exercised:"
        "\n    [steps 1-3]  Key coexistence: 3 agents publish distinct merge keys simultaneously"
        "\n    [step 4]     Visual: all 3 entries (weather, battery, time) visible"
        "\n    [step 5]     Key replacement: agent-weather updates weather value"
        "\n    [step 6]     Visual: weather shows '75F Cloudy'; battery/time unchanged"
        "\n    [step 7]     Empty-value removal: agent-weather clears weather entry"
        "\n    [step 8]     Visual: weather gone; battery/time remain"
        "\n    [step 9]     TTL expiry: battery publication swept after TTL elapsed"
        "\n    [step 10]    Visual: battery gone; time remains as sole entry",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
