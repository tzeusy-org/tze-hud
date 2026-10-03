#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""
Publish a batch of widget messages to a running HUD with MCP `hud_publish`.

Message file format (JSON array):
[
  {"widget": "gauge", "params": {"level": 0.75, "label": "CPU Usage"}, "ttl_ms": 60000}
]

Also supports clear operations (`hud_clear`):
[
  {"action": "clear", "widget": "gauge"}
]

The namespace is the agent the PSK belongs to; messages cannot override it.
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
        action = item.get("action", "publish")
        widget_name = item.get("widget")
        if not isinstance(widget_name, str) or not widget_name.strip():
            raise ValueError(f"message[{idx}].widget must be a non-empty string")
        if action == "publish":
            params = item.get("params")
            if not isinstance(params, dict) or not params:
                raise ValueError(f"message[{idx}].params must be a non-empty object")
        elif action != "clear":
            raise ValueError(f"message[{idx}].action must be 'publish' or 'clear'")
        out.append(item)
    return out


def parse_widget_names(raw: str) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    for item in raw.split(","):
        name = item.strip()
        if name and name not in seen:
            names.append(name)
            seen.add(name)
    return names


def message_widget_names(messages: list[dict[str, Any]]) -> list[str]:
    names: list[str] = []
    seen: set[str] = set()
    for msg in messages:
        name = msg.get("widget")
        if isinstance(name, str) and name and name not in seen:
            names.append(name)
            seen.add(name)
    return names


def clear_widgets(url: str, token: str, widgets: list[str], starting_request_id: int) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    req_id = starting_request_id
    for widget_name in widgets:
        response = rpc_call(
            url,
            token,
            "hud_clear",
            {"surface": f"widget:{widget_name}"},
            req_id,
        )
        results.append(
            {
                "request_id": req_id,
                "action": "clear",
                "widget_name": widget_name,
                "response": response,
            }
        )
        req_id += 1
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description="Publish MCP widget message batch")
    parser.add_argument("--url", help="MCP URL (default: derived from HUD_HOST)")
    parser.add_argument("--psk-env", help="Read the PSK from this environment variable instead of the paired file")
    parser.add_argument("--messages-file", required=True, help="Path to JSON array of widget message objects")
    parser.add_argument("--ttl-ms", type=int, default=60_000, help="Default TTL in milliseconds (0 = until cleared)")
    parser.add_argument("--delay-ms", type=int, default=0, help="Delay between publishes")
    parser.add_argument("--list-surfaces", action="store_true", help="Call hud_surfaces before publishing")
    parser.add_argument(
        "--cleanup-on-exit",
        action="store_true",
        help="Clear widgets after the batch exits, including KeyboardInterrupt and error paths",
    )
    parser.add_argument(
        "--cleanup-widgets",
        default="",
        help="Comma-separated widget instance names to clear on exit; defaults to widgets touched by the batch",
    )
    parser.add_argument("--cleanup-delay-ms", type=int, default=0, help="Delay before cleanup-on-exit clears widgets")
    args = parser.parse_args()

    try:
        args.url, token = hud_env.resolve(args.url, args.psk_env)
    except hud_env.HudEnvError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    messages: list[dict[str, Any]] = []
    exit_code = 0
    try:
        if args.list_surfaces:
            surfaces = rpc_call(args.url, token, "hud_surfaces", {}, 1)
            print(json.dumps({"hud_surfaces": surfaces}, ensure_ascii=True))

        messages = load_messages(args.messages_file)
        results: list[dict[str, Any]] = []
        req_id = 10
        for msg in messages:
            action = msg.get("action", "publish")

            surface = f"widget:{msg['widget']}"
            if action == "clear":
                response = rpc_call(args.url, token, "hud_clear", {"surface": surface}, req_id)
            else:
                params: dict[str, Any] = {
                    "surface": surface,
                    "params": msg["params"],
                    "ttl_ms": int(msg.get("ttl_ms", args.ttl_ms)),
                }
                response = rpc_call(args.url, token, "hud_publish", params, req_id)

            results.append(
                {
                    "request_id": req_id,
                    "action": action,
                    "widget": msg["widget"],
                    "response": response,
                }
            )
            req_id += 1
            if args.delay_ms > 0:
                time.sleep(args.delay_ms / 1000.0)

        print(json.dumps({"published": results}, ensure_ascii=True))
        exit_code = 0
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
        exit_code = 3
    except urllib.error.URLError as e:
        print(json.dumps({"error": "url_error", "detail": str(e)}, ensure_ascii=True), file=sys.stderr)
        exit_code = 4
    except KeyboardInterrupt:
        print(json.dumps({"error": "interrupted"}, ensure_ascii=True), file=sys.stderr)
        exit_code = 130
    except Exception as e:
        print(json.dumps({"error": "exception", "detail": str(e)}, ensure_ascii=True), file=sys.stderr)
        exit_code = 5
    finally:
        if args.cleanup_on_exit:
            cleanup_widgets = parse_widget_names(args.cleanup_widgets) or message_widget_names(messages)
            if cleanup_widgets:
                if args.cleanup_delay_ms > 0:
                    time.sleep(args.cleanup_delay_ms / 1000.0)
                try:
                    cleanup_results = clear_widgets(args.url, token, cleanup_widgets, 1000)
                    print(json.dumps({"cleanup": cleanup_results}, ensure_ascii=True))
                except Exception as e:
                    print(json.dumps({"error": "cleanup_failed", "detail": str(e)}, ensure_ascii=True), file=sys.stderr)
                    if exit_code == 0:
                        exit_code = 6
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
