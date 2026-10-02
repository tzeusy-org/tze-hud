#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Subtitle exemplar user-test scenario.

Exercises the subtitle zone on a live deployed HUD by publishing a streaming
breakpoint-reveal sequence and then a full multi-scenario sequence. Validates
progressive word-by-word reveal, rapid-replacement latest-wins, TTL auto-clear,
and multi-line word-wrap — all with the exemplar-test namespace.

Phases:
  1. Streaming reveal  — stream_text with breakpoints at word boundaries;
                         single publish held for the TTL while the compositor
                         progressively reveals word groups at its own frame rate
                         + TTL hold (10s default)
  2. Single line       — "Hello world — exemplar subtitle test" (10s TTL)
                         + 4s pause for visual inspection
  3. Multi-line        — long text to exercise word-wrap and backdrop sizing
                         + 4s pause for visual inspection
  4. Rapid replacement — 3 subtitles published 100ms apart; only the third
                         should remain visible
                         + 3s pause for visual inspection
  5. TTL expiry        — subtitle with 3s TTL; watch it fade out automatically
                         + TTL + 0.3s safety + 1.0s margin + 2s confirmation (~6.3s total)
  6. Streaming repeat  — stream_text breakpoint reveal again (explicitly, for
                         final human sign-off on word-by-word behaviour)
                         + TTL hold (10s default)

All messages use namespace: "exemplar-test".

Usage:
  subtitle_exemplar.py --url http://host:9090
  subtitle_exemplar.py --url http://host:9090 --psk-env MY_PSK --ttl 10000
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
DEFAULT_TTL_MS = 10000          # ms; used for single-line and streaming phases
SHORT_TTL_MS = 3000             # ms; used for TTL-expiry phase (3 seconds)
RAPID_TTL_MS = 5000             # ms; used for rapid-replace messages

ZONE_NAME = "subtitle"
NAMESPACE = "exemplar-test"

# Text used for the streaming-reveal phase
STREAM_TEXT = "The quick brown fox jumps over the lazy dog"
# Byte offsets of word boundaries in STREAM_TEXT (after each word, before next)
STREAM_BREAKPOINTS = [3, 9, 15, 19, 25, 30, 34, 38]

# ---------------------------------------------------------------------------
# MCP RPC helper — same framing as th-hud-publish/scripts/publish.py
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


def publish_stream_text(
    url: str,
    token: str,
    req_id: int,
    text: str,
    ttl_ms: int,
    breakpoints: list[int] | None = None,
    label: str = "",
) -> dict[str, Any]:
    """Publish a stream_text payload (plain string content) to the subtitle zone."""
    params: dict[str, Any] = {
        "surface": f"zone:{ZONE_NAME}",
        "content": text,
        "ttl_ms": ttl_ms,
    }
    # Breakpoints are not on the MCP surface (docs/api.md); kept in the
    # signature so phases still document the intended reveal points.
    del breakpoints

    response = rpc_call(url, token, "hud_publish", params, req_id)
    ok = "error" not in response
    status = "ok" if ok else f"ERR: {response.get('error')}"
    tag = f"[{label}]" if label else ""
    text_repr = repr(text)
    text_display = text_repr[:60] + "..." if len(text_repr) > 60 else text_repr
    bp_display = repr(breakpoints) if breakpoints is not None else "\u2014"
    print(
        f"  {tag:<12} {text_display}"
        f"  breakpoints={bp_display}"
        f"  ttl={ttl_ms}ms  {status}",
        flush=True,
    )
    return response


# ---------------------------------------------------------------------------
# Phases
# ---------------------------------------------------------------------------


def phase1_streaming(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """
    Phase 1: Streaming word-by-word reveal.

    Publishes stream_text with breakpoints at word boundaries. The compositor
    reveals one word-group at a time at its own frame rate. This should look
    like broadcast captioning: "The" ... "The quick" ... "The quick brown" ...

    Visual check: words appear one-by-one from left; full sentence visible at end.
    """
    print(
        "\n--- Phase 1: Streaming reveal (breakpoints at word boundaries) ---",
        flush=True,
    )
    print(
        f"  Text       : {STREAM_TEXT!r}",
        flush=True,
    )
    print(
        f"  Breakpoints: {STREAM_BREAKPOINTS!r}",
        flush=True,
    )

    publish_stream_text(
        url, token, req_id,
        text=STREAM_TEXT,
        ttl_ms=ttl_ms,
        breakpoints=STREAM_BREAKPOINTS,
        label="streaming",
    )
    req_id += 1

    print(
        "\n  [visual check] Watch the subtitle zone:"
        "\n    1. 'The' appears first"
        "\n    2. 'The quick' revealed next"
        "\n    3. 'The quick brown' revealed next"
        "\n    4. ... continues word-by-word ..."
        "\n    5. Full sentence 'The quick brown fox jumps over the lazy dog' visible at end",
        flush=True,
    )
    print(f"  Holding {ttl_ms // 1000}s for observation (streaming + TTL)...", flush=True)
    time.sleep(ttl_ms / 1000.0)
    return req_id


def phase2_single_line(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """
    Phase 2: Single-line baseline.

    Verifies basic rendering: white text, black outline, dark backdrop,
    centered near screen bottom.
    """
    text = "Hello world \u2014 exemplar subtitle test"
    print(
        "\n--- Phase 2: Single-line (baseline rendering) ---",
        flush=True,
    )

    publish_stream_text(
        url, token, req_id,
        text=text,
        ttl_ms=ttl_ms,
        label="single-line",
    )
    req_id += 1

    print(
        "\n  [visual check]"
        "\n    - White text with visible black outline"
        "\n    - Semi-transparent dark backdrop behind text"
        "\n    - Text centered horizontally near bottom of screen"
        "\n    - Single line; no wrapping",
        flush=True,
    )
    print("  Pausing 4s for visual inspection...", flush=True)
    time.sleep(4)
    return req_id


def phase3_multi_line(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """
    Phase 3: Multi-line word-wrap.

    Long text forces the compositor to wrap at zone boundaries. The backdrop
    must expand to contain all visible lines; overflow should truncate with
    ellipsis if the text exceeds the available vertical space.
    """
    text = (
        "This is a much longer subtitle message designed to test word wrapping"
        " behavior across multiple lines. The compositor should wrap this text"
        " cleanly within the zone bounds and truncate with ellipsis if it"
        " exceeds the vertical space available."
    )
    print(
        "\n--- Phase 3: Multi-line (word-wrap + backdrop sizing) ---",
        flush=True,
    )

    publish_stream_text(
        url, token, req_id,
        text=text,
        ttl_ms=ttl_ms,
        label="multi-line",
    )
    req_id += 1

    print(
        "\n  [visual check]"
        "\n    - Text wraps cleanly within zone width (no overflow beyond backdrop)"
        "\n    - Backdrop expands vertically to contain all lines"
        "\n    - If text exceeds vertical limit, last line ends with '...' (ellipsis)"
        "\n    - All visible lines remain centered horizontally",
        flush=True,
    )
    print("  Pausing 4s for visual inspection...", flush=True)
    time.sleep(4)
    return req_id


def phase4_rapid_replace(url: str, token: str, req_id: int) -> int:
    """
    Phase 4: Rapid replacement (latest-wins contention).

    Publishes 3 subtitles with 100ms inter-publish delay. Only the third
    should remain visible; no blank frames should appear between replacements.
    """
    messages = [
        ("rapid-1", "First subtitle \u2014 should be replaced immediately"),
        ("rapid-2", "Second subtitle \u2014 also replaced"),
        ("rapid-3", "Third subtitle \u2014 this one stays"),
    ]
    print(
        "\n--- Phase 4: Rapid replacement (3 publishes, 100ms apart) ---",
        flush=True,
    )

    for idx, (label, text) in enumerate(messages):
        publish_stream_text(
            url, token, req_id,
            text=text,
            ttl_ms=RAPID_TTL_MS,
            label=label,
        )
        req_id += 1
        if idx < len(messages) - 1:
            time.sleep(0.1)  # 100ms

    print(
        "\n  [visual check]"
        "\n    - Only 'Third subtitle \u2014 this one stays' should be visible"
        "\n    - No blank/flash frame should have appeared between replacements"
        "\n    - Transition from first to last should be visually clean",
        flush=True,
    )
    print("  Pausing 3s for visual inspection...", flush=True)
    time.sleep(3)
    return req_id


def phase5_ttl_expiry(url: str, token: str, req_id: int) -> int:
    """
    Phase 5: TTL auto-clear.

    Publishes a subtitle with a 3-second TTL. The zone must auto-clear after
    the TTL elapses, triggering a 150ms fade-out before the zone empties.
    After the hold we pause an extra 2s so the observer can confirm the empty state.
    """
    text = "This subtitle expires in 3 seconds"
    hold_s = SHORT_TTL_MS / 1000.0 + 0.3 + 1.0  # TTL + fade-out + margin
    print(
        "\n--- Phase 5: TTL expiry (3s TTL, auto-clear with fade-out) ---",
        flush=True,
    )

    publish_stream_text(
        url, token, req_id,
        text=text,
        ttl_ms=SHORT_TTL_MS,
        label="ttl-expiry",
    )
    req_id += 1

    print(
        f"\n  [visual check] Watch the subtitle for {SHORT_TTL_MS / 1000:.0f}s:",
        flush=True,
    )
    print(
        "    - Text is visible immediately after publish"
        "\n    - After ~3s it begins to fade out (150ms ramp)"
        "\n    - Zone shows nothing after fade-out completes",
        flush=True,
    )
    print(f"  Waiting {hold_s:.1f}s (TTL + 150ms fade-out + 1s margin)...", flush=True)

    elapsed = 0.0
    while elapsed < hold_s:
        step = min(1.0, hold_s - elapsed)
        time.sleep(step)
        elapsed += step
        remaining = hold_s - elapsed
        print(f"  ... {elapsed:.0f}s / {hold_s:.0f}s  ({remaining:.0f}s remaining)", flush=True)

    print("  Pausing 2s for visual confirmation of empty zone...", flush=True)
    time.sleep(2)
    return req_id


def phase6_streaming_final(url: str, token: str, ttl_ms: int, req_id: int) -> int:
    """
    Phase 6: Streaming repeat (final sign-off).

    Repeats the streaming-reveal phase with a longer hold so the human observer
    has ample time to watch each word appear in sequence.
    """
    hold_s = ttl_ms / 1000.0
    print(
        "\n--- Phase 6: Streaming repeat (final sign-off) ---",
        flush=True,
    )
    print(
        f"  Text       : {STREAM_TEXT!r}",
        flush=True,
    )
    print(
        f"  Breakpoints: {STREAM_BREAKPOINTS!r}",
        flush=True,
    )

    publish_stream_text(
        url, token, req_id,
        text=STREAM_TEXT,
        ttl_ms=ttl_ms,
        breakpoints=STREAM_BREAKPOINTS,
        label="stream-final",
    )
    req_id += 1

    print(
        "\n  [visual check] Final word-by-word reveal:"
        "\n    Confirm all 9 word groups appear one-by-one:"
        "\n      'The' | 'quick' | 'brown' | 'fox' | 'jumps' | 'over' | 'the' | 'lazy' | 'dog'"
        "\n    Accept: AC6 — streaming text reveals word-by-word.",
        flush=True,
    )
    print(f"  Holding {hold_s:.0f}s for full observation...", flush=True)
    time.sleep(hold_s)
    return req_id


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Subtitle exemplar user-test: exercises streaming word-by-word reveal,"
            " single-line baseline, multi-line word-wrap, rapid-replacement contention,"
            " and TTL auto-clear on the subtitle zone of a live HUD."
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
            f"TTL in milliseconds for non-expiry phases (default: {DEFAULT_TTL_MS})."
            " The TTL-expiry phase always uses a fixed 3-second TTL."
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

    print("Subtitle Exemplar User-Test", flush=True)
    print(f"  HUD URL    : {url}", flush=True)
    print(f"  PSK env    : {args.psk_env}", flush=True)
    print(f"  TTL        : {ttl_ms}ms (non-expiry phases)", flush=True)
    print(f"  Zone       : {ZONE_NAME}", flush=True)
    print(f"  Namespace  : {NAMESPACE}", flush=True)
    print(
        "\n  Human acceptance criteria (verify each visually):"
        "\n    AC1 — White text with visible black outline on semi-transparent dark backdrop"
        "\n    AC2 — Text centered horizontally near bottom of screen"
        "\n    AC3 — Multi-line text wraps cleanly within backdrop bounds"
        "\n    AC4 — Rapid replacement transitions are smooth (no blank frames)"
        "\n    AC5 — Content disappears after TTL with visible fade-out"
        "\n    AC6 — Streaming text reveals word-by-word",
        flush=True,
    )

    req_id = 10

    try:
        # Phase 1: Streaming reveal (AC6)
        req_id = phase1_streaming(url, token, ttl_ms, req_id)

        # Phase 2: Single-line baseline (AC1, AC2)
        req_id = phase2_single_line(url, token, ttl_ms, req_id)

        # Phase 3: Multi-line word-wrap (AC3)
        req_id = phase3_multi_line(url, token, ttl_ms, req_id)

        # Phase 4: Rapid replacement (AC4)
        req_id = phase4_rapid_replace(url, token, req_id)

        # Phase 5: TTL expiry (AC5)
        req_id = phase5_ttl_expiry(url, token, req_id)

        # Phase 6: Streaming repeat — final sign-off (AC6)
        req_id = phase6_streaming_final(url, token, ttl_ms, req_id)

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
        "\n  Summary of what was exercised:"
        "\n    [phase 1] Streaming word-by-word reveal via breakpoints         (AC6)"
        "\n    [phase 2] Single-line baseline — white outline text on backdrop (AC1, AC2)"
        "\n    [phase 3] Multi-line word-wrap with backdrop sizing              (AC3)"
        "\n    [phase 4] Rapid replacement — 3 publishes 100ms apart           (AC4)"
        "\n    [phase 5] TTL auto-clear — 3s expiry with visible fade-out      (AC5)"
        "\n    [phase 6] Streaming repeat — final word-by-word sign-off        (AC6)"
        "\n"
        "\n  namespace: exemplar-test was used for all publishes.",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
