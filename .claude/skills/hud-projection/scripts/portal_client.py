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

Started under a Claude session whose tze-hud MCP adapter is running, every
command joins that adapter's HUD session (its private record in
~/.config/tze-hud/sessions/), so a background waiter sees the portals the
model attached through the MCP tools. HUD_HOST, when set, must name the same
HUD; without such a session the client is the agent's unscoped caller.
"""

import argparse
import json
import os
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


_SESSION = []  # [record or None], resolved once per process


def session():
    """The enclosing Claude session's adapter session record, if any."""
    if not _SESSION:
        record = hud_env.inherited_session()
        if record and os.environ.get("HUD_HOST"):
            try:
                if hud_env.mcp_url() != record["url"]:
                    record = None  # HUD_HOST names another HUD
            except hud_env.HudEnvError:
                record = None
        _SESSION.append(record)
    return _SESSION[0]


def endpoint():
    """(mcp_url, psk): the joined session's HUD, else HUD_HOST's."""
    try:
        record = session()
        if record:
            key = hud_env.adapter_key({"host": record["host"], "url": record["url"], "explicit": True})
            return record["url"], key
        return hud_env.resolve()
    except hud_env.HudEnvError as error:
        die(str(error))


def rpc(method, params, request_id=1, *, timeout=60):
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
    record = session()
    if record:
        request.add_header("Mcp-Session-Id", record["session"])
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        if error.code == 404 and record:
            die("the Claude session's HUD session ended; reconnect the tze-hud MCP server")
        die(f"MCP transport failed: {error}")
    except urllib.error.URLError as error:
        die(f"MCP transport failed: {error}")


def call_tool(name, arguments, *, timeout=None):
    """Call one tool; return its decoded result or raise ToolError."""
    params = {"name": name, "arguments": arguments}
    # Preserve the original calling shape for existing clients and fake servers.
    response = (
        rpc("tools/call", params)
        if timeout is None
        else rpc("tools/call", params, timeout=timeout)
    )
    if response.get("error"):
        die(f"{name}: {response['error'].get('message')}")
    result = response["result"]
    decoded = json.loads(result["content"][0]["text"])
    if result.get("isError"):
        raise ToolError(decoded.get("code"), decoded.get("hint"))
    return decoded


def emit(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


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
    if args.ttl_ms is not None:
        arguments["ttl_ms"] = args.ttl_ms
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
        if items:
            break
    if ack:
        call_tool("hud_input", {"ack": ack})
    if got == 0:
        sys.exit(3)


def cmd_ack(args):
    emit(call_tool("hud_input", {"ack": args.input_id}))


def cmd_clear(args):
    arguments = {"surface": f"portal:{args.id}"}
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
    sp.add_argument("--state", choices=["active", "degraded"], help="also set agent status")
    sp.add_argument("--ttl-ms", type=int, help="hold this publish (0 = until clear)")
    sp.set_defaults(func=cmd_publish)

    sp = sub.add_parser("status")
    sp.add_argument("--id", required=True)
    sp.add_argument(
        "--state",
        required=True,
        choices=["active", "degraded"],
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
