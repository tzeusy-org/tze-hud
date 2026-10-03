#!/usr/bin/env python3
"""Deterministic client for HUD portal projection over MCP.

Every command is one standard MCP `tools/call` to the runtime (`docs/api.md`):

  surfaces                      hud_surfaces
  publish  --id ID --text T     hud_publish {surface: portal:ID, content: T}
  status   --id ID --state S    hud_publish {surface: portal:ID, status: S}
  poll     [--wait-ms N]        hud_input   (prints items as NDJSON; exit 3 if none)
  ack      --input-id I ...     hud_input   {ack: [I, ...]}
  clear    --id ID              hud_clear   {surface: portal:ID}

The first publish to a portal attaches it. The portal is keyed by your agent
identity (the PSK), so no command handles a token. Environment: HUD_HOST
(the HUD's host); the PSK is read from ~/.config/tze-hud/<host>.psk, written
by `.claude/skills/user-test/scripts/hud_pair.py`.
"""

import argparse
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402


class ToolError(Exception):
    """A tool result with isError: carries the stable code and hint."""

    def __init__(self, code, hint):
        super().__init__(f"{code}: {hint}")
        self.code = code
        self.hint = hint


def die(msg, code=1):
    print(f"portal_client: {msg}", file=sys.stderr)
    sys.exit(code)


def endpoint():
    """(mcp_url, psk) from HUD_HOST and the paired PSK file."""
    try:
        return hud_env.resolve()
    except hud_env.HudEnvError as error:
        die(str(error))


def rpc(method, params, request_id=1):
    """POST one JSON-RPC request and return the parsed response."""
    body = json.dumps(
        {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
    ).encode()
    url, token = endpoint()
    request = urllib.request.Request(
        url,
        data=body,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.loads(response.read())
    except (urllib.error.HTTPError, urllib.error.URLError) as error:
        die(f"MCP transport failed: {error}")


def call_tool(name, arguments):
    """Call one tool; return its decoded result or raise ToolError."""
    response = rpc("tools/call", {"name": name, "arguments": arguments})
    if response.get("error"):
        die(f"{name}: {response['error'].get('message')}")
    result = response["result"]
    decoded = json.loads(result["content"][0]["text"])
    if result.get("isError"):
        raise ToolError(decoded.get("code"), decoded.get("hint"))
    return decoded


def emit(value):
    print(json.dumps(value, separators=(",", ":")))


def cmd_surfaces(_args):
    emit(call_tool("hud_surfaces", {}))


def read_text(args):
    if args.text_file:
        with open(args.text_file, encoding="utf-8") as f:
            return f.read()
    return args.text


def cmd_publish(args):
    arguments = {"surface": f"portal:{args.id}"}
    text = read_text(args)
    if text:
        arguments["content"] = text
    if args.key:
        arguments["key"] = args.key
    if args.expects_reply:
        arguments["expects_reply"] = True
    if args.display_name:
        arguments["display_name"] = args.display_name
    if args.state:
        arguments["status"] = args.state
    if "content" not in arguments and "status" not in arguments:
        die("publish needs --text, --text-file, or --state")
    emit(call_tool("hud_publish", arguments))


def cmd_status(args):
    emit(call_tool("hud_publish", {"surface": f"portal:{args.id}", "status": args.state}))


def cmd_poll(args):
    """Long-poll input; print each new item as one NDJSON line."""
    got = 0
    ack = []
    for _ in range(max(1, args.rounds)):
        arguments = {"wait_ms": args.wait_ms, "max_items": args.max_items}
        if ack:
            arguments["ack"] = ack
        result = call_tool("hud_input", arguments)
        items = result.get("items", [])
        for item in items:
            emit(item)
        got += len(items)
        ack = [item["id"] for item in items] if args.ack else []
        if items and not args.ack:
            break
    if ack:
        call_tool("hud_input", {"ack": ack})
    if got == 0:
        sys.exit(3)


def cmd_ack(args):
    emit(call_tool("hud_input", {"ack": args.input_id}))


def cmd_clear(args):
    arguments = {"surface": f"portal:{args.id}"}
    if args.reason:
        arguments["reason"] = args.reason
    emit(call_tool("hud_clear", arguments))


def cmd_hold(args):
    emit(call_tool("hud_hold", {"surface": f"portal:{args.id}", "ttl_ms": args.ttl_ms}))


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("surfaces").set_defaults(func=cmd_surfaces)

    sp = sub.add_parser("publish")
    sp.add_argument("--id", required=True, help="projection id")
    sp.add_argument("--text")
    sp.add_argument("--text-file")
    sp.add_argument("--key", help="coalesce key: a publish with the same key replaces in place")
    sp.add_argument("--expects-reply", action="store_true")
    sp.add_argument("--display-name", help="name shown when this publish attaches")
    sp.add_argument("--state", help="also set the lifecycle state")
    sp.set_defaults(func=cmd_publish)

    sp = sub.add_parser("status")
    sp.add_argument("--id", required=True)
    sp.add_argument(
        "--state",
        required=True,
        choices=["attached", "active", "degraded", "hud_unavailable", "detached"],
    )
    sp.set_defaults(func=cmd_status)

    sp = sub.add_parser("poll")
    sp.add_argument("--wait-ms", type=int, default=30000)
    sp.add_argument("--rounds", type=int, default=1)
    sp.add_argument("--max-items", type=int, default=4)
    sp.add_argument("--ack", action="store_true", help="ack every item received")
    sp.set_defaults(func=cmd_poll)

    sp = sub.add_parser("ack")
    sp.add_argument("--input-id", required=True, action="append")
    sp.set_defaults(func=cmd_ack)

    sp = sub.add_parser("hold", help="keep a quiet portal attached (ttl 0 = until clear)")
    sp.add_argument("--id", required=True)
    sp.add_argument("--ttl-ms", type=int, required=True)
    sp.set_defaults(func=cmd_hold)

    sp = sub.add_parser("clear")
    sp.add_argument("--id", required=True)
    sp.add_argument("--reason")
    sp.set_defaults(func=cmd_clear)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        args.func(args)
    except ToolError as error:
        emit({"code": error.code, "hint": error.hint})
        sys.exit(1)


if __name__ == "__main__":
    main()
