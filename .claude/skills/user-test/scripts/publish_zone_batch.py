#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Publish a batch of zone messages to a running HUD with MCP `hud_publish`.

Message file format (JSON array):
[
  {"zone": "status-bar", "content": {"entries": {"agent": "online"}}, "key": "agent-status", "ttl_ms": 60000},
  {"zone": "subtitle", "content": "The quick brown fox", "ttl_ms": 10000}
]

Optional fields per message:
  key     -- merge key: a publish with the same key replaces the earlier one
  ttl_ms  -- overrides --ttl-ms (0 = until cleared)

The namespace is the agent the PSK belongs to; messages cannot override it.
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


def rpc_call(url: str, token: str, tool: str, arguments: dict[str, Any], request_id: int) -> dict[str, Any]:
    """Call one MCP tool; returns {"result": ...} or {"error": ...}."""
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


def load_messages(path: str) -> list[dict[str, Any]]:
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, list):
        raise ValueError("messages file must be a JSON array")
    out: list[dict[str, Any]] = []
    for idx, item in enumerate(data):
        if not isinstance(item, dict):
            raise ValueError(f"message[{idx}] must be an object")
        zone = item.get("zone")
        content = item.get("content")
        if not isinstance(zone, str) or not zone.strip():
            raise ValueError(f"message[{idx}].zone must be a non-empty string")
        if content is None:
            raise ValueError(f"message[{idx}].content is required")
        if isinstance(content, str) and not content:
            raise ValueError(f"message[{idx}].content must be non-empty")
        out.append(item)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description="Publish MCP zone message batch")
    parser.add_argument("--url", required=True, help="MCP HTTP URL, e.g. http://host:9090")
    parser.add_argument("--psk-env", default="MCP_TEST_PSK", help="Environment variable containing PSK")
    parser.add_argument("--messages-file", required=True, help="Path to JSON array of message objects")
    parser.add_argument("--ttl-ms", type=int, default=60_000, help="Default TTL in milliseconds")
    parser.add_argument("--delay-ms", type=int, default=0, help="Delay between publishes")
    parser.add_argument("--list-surfaces", action="store_true", help="Call hud_surfaces before publishing")
    args = parser.parse_args()

    token = os.getenv(args.psk_env, "")
    if not token:
        print(f"ERROR: env var {args.psk_env} is empty or unset", file=sys.stderr)
        return 2

    try:
        if args.list_surfaces:
            surfaces = rpc_call(args.url, token, "hud_surfaces", {}, 1)
            print(json.dumps({"hud_surfaces": surfaces}, ensure_ascii=True))

        messages = load_messages(args.messages_file)
        results: list[dict[str, Any]] = []
        req_id = 10
        for msg in messages:
            params: dict[str, Any] = {
                "surface": f"zone:{msg['zone']}",
                "content": msg["content"],
                "ttl_ms": int(msg.get("ttl_ms", args.ttl_ms)),
            }
            if msg.get("key") is not None:
                params["key"] = msg["key"]

            response = rpc_call(args.url, token, "hud_publish", params, req_id)
            results.append(
                {
                    "request_id": req_id,
                    "surface": params["surface"],
                    "response": response,
                }
            )
            req_id += 1
            if args.delay_ms > 0:
                time.sleep(args.delay_ms / 1000.0)

        print(json.dumps({"published": results}, ensure_ascii=True))
        return 0
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(
            json.dumps(
                {
                    "error": "http_error",
                    "status": e.code,
                    "body": body,
                },
                ensure_ascii=True,
            ),
            file=sys.stderr,
        )
        return 3
    except urllib.error.URLError as e:
        print(json.dumps({"error": "url_error", "detail": str(e)}, ensure_ascii=True), file=sys.stderr)
        return 4
    except Exception as e:
        print(json.dumps({"error": "exception", "detail": str(e)}, ensure_ascii=True), file=sys.stderr)
        return 5


if __name__ == "__main__":
    raise SystemExit(main())
