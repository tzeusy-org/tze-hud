#!/usr/bin/env python3
"""
mcp_reachability_check.py — Verify that an MCP HTTP endpoint is reachable and authenticates correctly.

Exits with:
  0  endpoint is reachable and responds to a valid MCP probe
  1  endpoint is reachable but authentication failed (bad/missing PSK)
  2  endpoint is not reachable (connection refused, timeout, DNS failure)
  3  endpoint returned an unexpected / malformed response
  4  usage error (bad arguments)

Usage:
  python3 scripts/mcp_reachability_check.py --url http://HOST:PORT [--psk-env VAR] [--timeout 10]

Options:
  --url <url>        MCP HTTP base URL, e.g. http://host:9090       (required)
  --psk-env <var>    Environment variable containing the Bearer PSK  (default: TZE_HUD_PSK)
  --timeout <secs>   HTTP connect + read timeout in seconds          (default: 10)
  --json             Emit a JSON result object to stdout instead of human text
  --quiet            Suppress all output (use exit code only)

Output (human, on success):
  MCP reachable: http://host:9090  (hud_surfaces ok, <N> zones)

Output (human, on failure):
  MCP unreachable: <detail>
  MCP auth error: <detail>

JSON output shape:
  {
    "reachable": true|false,
    "authenticated": true|false|null,
    "url": "http://...",
    "zones": [...] | null,
    "error": null | "auth_error" | "connection_error" | "unexpected_response",
    "detail": "..."
  }
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any


def _probe(url: str, token: str, timeout: int) -> dict[str, Any]:
    """Send a `hud_surfaces` tool call and return the parsed response body."""
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "hud_surfaces", "arguments": {}},
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
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = resp.read().decode("utf-8")
    return json.loads(payload)


def check(url: str, token: str, timeout: int) -> dict[str, Any]:
    """
    Probe the MCP HTTP endpoint.

    Returns a result dict:
      reachable, authenticated, zones (list or None), error (str or None), detail (str).
    """
    result: dict[str, Any] = {
        "reachable": False,
        "authenticated": None,
        "url": url,
        "zones": None,
        "error": None,
        "detail": "",
    }

    try:
        data = _probe(url, token, timeout)
    except urllib.error.HTTPError as exc:
        # HTTPError is a subclass of URLError; check it first.
        # HTTP-level auth failure (401, 403) means the server is reachable but the PSK is wrong.
        if exc.code in (401, 403):
            result["reachable"] = True
            result["error"] = "auth_error"
            result["authenticated"] = False
            result["detail"] = f"HTTP {exc.code}: {exc.reason}"
        else:
            result["error"] = "connection_error"
            result["detail"] = f"HTTP {exc.code}: {exc.reason}"
        return result
    except urllib.error.URLError as exc:
        result["error"] = "connection_error"
        result["detail"] = str(exc)
        return result
    except Exception as exc:  # noqa: BLE001
        result["error"] = "connection_error"
        result["detail"] = f"unexpected: {exc}"
        return result

    result["reachable"] = True

    # Inspect the JSON-RPC response.
    if not isinstance(data, dict):
        result["error"] = "unexpected_response"
        result["detail"] = f"expected JSON object, got: {type(data).__name__}"
        result["authenticated"] = None
        return result

    if "error" in data:
        err = data["error"]
        code = err.get("code", 0) if isinstance(err, dict) else 0
        message = err.get("message", str(err)) if isinstance(err, dict) else str(err)
        # JSON-RPC auth errors are typically code -32001 or message contains "Unauth".
        if code == -32001 or (isinstance(message, str) and "auth" in message.lower()):
            result["error"] = "auth_error"
            result["authenticated"] = False
            result["detail"] = message
        else:
            result["error"] = "unexpected_response"
            result["authenticated"] = None
            result["detail"] = message
        return result

    if "result" in data:
        result["authenticated"] = True
        try:
            surfaces = json.loads(data["result"]["content"][0]["text"])["surfaces"]
        except (KeyError, IndexError, TypeError, ValueError):
            surfaces = None
        if isinstance(surfaces, list):
            result["zones"] = [s["s"] for s in surfaces if str(s.get("s", "")).startswith("zone:")]
        else:
            result["zones"] = []
        return result

    result["error"] = "unexpected_response"
    result["detail"] = f"no 'result' or 'error' key in response: {json.dumps(data)}"
    return result


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check MCP HTTP endpoint reachability and authentication."
    )
    parser.add_argument("--url", required=True, help="MCP HTTP URL, e.g. http://host:9090")
    parser.add_argument(
        "--psk-env",
        default="TZE_HUD_PSK",
        help="Env var containing the Bearer PSK (default: TZE_HUD_PSK)",
    )
    parser.add_argument("--timeout", type=int, default=10, help="Timeout in seconds (default: 10)")
    parser.add_argument("--json", dest="emit_json", action="store_true", help="Emit JSON output")
    parser.add_argument("--quiet", action="store_true", help="No output — use exit code only")
    args = parser.parse_args()

    token = os.getenv(args.psk_env, "")
    if not token:
        if not args.quiet:
            print(
                f"ERROR: env var '{args.psk_env}' is unset or empty. "
                "Set it to the MCP pre-shared key.",
                file=sys.stderr,
            )
        return 4

    result = check(args.url, token, args.timeout)

    if args.emit_json:
        print(json.dumps(result, ensure_ascii=True))
    elif not args.quiet:
        if result["error"] == "connection_error":
            print(f"MCP unreachable: {args.url}")
            print(f"  detail: {result['detail']}", file=sys.stderr)
        elif result["error"] == "auth_error":
            print(f"MCP auth error: {args.url}")
            print(f"  detail: {result['detail']}", file=sys.stderr)
        elif result["error"] == "unexpected_response":
            print(f"MCP unexpected response: {args.url}")
            print(f"  detail: {result['detail']}", file=sys.stderr)
        else:
            zones = result.get("zones") or []
            print(f"MCP reachable: {args.url}  (hud_surfaces ok, {len(zones)} zones)")

    if result["error"] == "connection_error":
        return 2
    if result["error"] == "auth_error":
        return 1
    if result["error"] == "unexpected_response":
        return 3
    # Success
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
