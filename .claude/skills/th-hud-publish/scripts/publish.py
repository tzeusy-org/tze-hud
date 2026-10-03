#!/usr/bin/env python3
"""
Publish zone messages to a running tze_hud instance (MCP `hud_publish`).

Usage:
  # List the surfaces this PSK may use
  publish.py --list-surfaces

  # Single inline publish (string content)
  publish.py --zone alert-banner --content "Hello"

  # Single inline publish (typed content; `type` is inferred from the zone)
  publish.py --zone status-bar \
    --content '{"entries":{"build":"passing"}}' --key build-status

  # Clear your publication from a zone
  publish.py --zone subtitle --clear

  # Batch publish from file
  publish.py --messages-file msgs.json

Message objects: {"zone": "...", "content": ..., "key"?: "...", "ttl_ms"?: N}.
Content is a plain string, or an object for structured zones
(notification: title/body/urgency/actions; status_bar: entries;
solid_color: r/g/b/a). The namespace is the agent the PSK belongs to.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402


def call_tool(
    url: str, token: str, tool: str, arguments: dict[str, Any], request_id: int
) -> dict[str, Any]:
    """Call one MCP tool (`tools/call`).

    Returns {"result": <decoded>} or {"error": <decoded>}; a tool error
    decodes to {"code": ..., "hint": ...}.
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


def parse_content(raw: str) -> Any:
    """Parse content: try a JSON object first, fall back to a plain string."""
    stripped = raw.strip()
    if stripped.startswith("{"):
        try:
            obj = json.loads(stripped)
            if isinstance(obj, dict):
                return obj
        except json.JSONDecodeError:
            pass
    return raw


def load_messages(path: str) -> list[dict[str, Any]]:
    """Load and validate a JSON array of publish messages."""
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    if not isinstance(data, list):
        raise ValueError("messages file must be a JSON array")
    for idx, item in enumerate(data):
        if not isinstance(item, dict):
            raise ValueError(f"message[{idx}] must be an object")
        if not isinstance(item.get("zone"), str) or not item["zone"].strip():
            raise ValueError(f"message[{idx}].zone must be a non-empty string")
        content = item.get("content")
        if content is None or content == "":
            raise ValueError(f"message[{idx}].content is required")
    return data


def publish_messages(
    url: str, token: str, messages: list[dict[str, Any]]
) -> tuple[list[dict[str, Any]], bool]:
    """Publish a list of messages and return (results, any_failed)."""
    results: list[dict[str, Any]] = []
    any_failed = False
    for req_id, msg in enumerate(messages, start=10):
        arguments: dict[str, Any] = {
            "surface": f"zone:{msg['zone']}",
            "content": msg["content"],
        }
        if "ttl_ms" in msg:
            arguments["ttl_ms"] = int(msg["ttl_ms"])
        if "key" in msg:
            arguments["key"] = msg["key"]
        response = call_tool(url, token, "hud_publish", arguments, req_id)
        if "error" in response:
            any_failed = True
        results.append({"surface": arguments["surface"], "response": response})
    return results, any_failed


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Publish zone messages to a tze_hud MCP endpoint"
    )
    parser.add_argument("--url", help="MCP URL (default: derived from HUD_HOST)")
    parser.add_argument(
        "--psk-env",
        help="Read the PSK from this environment variable instead of the paired file",
    )
    parser.add_argument("--list-surfaces", action="store_true", help="Call hud_surfaces and print results")
    parser.add_argument("--messages-file", help="Path to JSON array of message objects")
    parser.add_argument("--zone", help="Zone name for an inline publish or --clear")
    parser.add_argument("--content", help="Inline content: plain string or JSON object string")
    parser.add_argument("--key", help="Merge key for the inline publish")
    parser.add_argument("--ttl-ms", type=int, help="Content lifetime in ms (0 = until cleared)")
    parser.add_argument("--clear", action="store_true", help="Clear your publication from --zone")
    args = parser.parse_args()

    has_inline = args.content is not None
    if not (args.list_surfaces or args.messages_file or has_inline or args.clear):
        parser.error("provide --list-surfaces, --messages-file, --zone/--content, or --zone --clear")
    if (has_inline or args.clear) and not args.zone:
        parser.error("--content and --clear need --zone")
    if has_inline and args.messages_file:
        parser.error("cannot combine --content with --messages-file")

    try:
        args.url, token = hud_env.resolve(args.url, args.psk_env)
    except hud_env.HudEnvError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2

    try:
        if args.list_surfaces:
            print(json.dumps(call_tool(args.url, token, "hud_surfaces", {}, 1), indent=2))

        any_failed = False
        if args.clear:
            response = call_tool(args.url, token, "hud_clear", {"surface": f"zone:{args.zone}"}, 2)
            print(json.dumps({"cleared": response}, indent=2))
            any_failed = "error" in response
        elif has_inline or args.messages_file:
            if has_inline:
                msg: dict[str, Any] = {"zone": args.zone, "content": parse_content(args.content)}
                if args.ttl_ms is not None:
                    msg["ttl_ms"] = args.ttl_ms
                if args.key is not None:
                    msg["key"] = args.key
                messages = [msg]
            else:
                messages = load_messages(args.messages_file)
            results, any_failed = publish_messages(args.url, token, messages)
            print(json.dumps({"published": results}, indent=2))
        return 1 if any_failed else 0

    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")
        print(json.dumps({"error": "http_error", "status": e.code, "body": body}), file=sys.stderr)
        return 3
    except urllib.error.URLError as e:
        print(json.dumps({"error": "url_error", "detail": str(e)}), file=sys.stderr)
        return 4
    except ValueError as e:
        print(json.dumps({"error": "validation_error", "detail": str(e)}), file=sys.stderr)
        return 5
    except Exception as e:
        print(json.dumps({"error": "exception", "detail": str(e)}), file=sys.stderr)
        return 6


if __name__ == "__main__":
    raise SystemExit(main())
