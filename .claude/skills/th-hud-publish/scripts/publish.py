#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Publish zones/widgets and manage HUD holdings over MCP without printing the PSK.

Run from the repo root with HUD_HOST set, or pass --host host[:port].
See --help for invocations and the backwards-compatible batch format.
"""

from __future__ import annotations

import argparse
import http.client
import json
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402


class PublishInterrupted(Exception):
    """Preserve completed receipts when a later request has an unknown outcome."""

    def __init__(self, results: list, surface: str, cause: Exception):
        super().__init__("publish interrupted")
        self.results = results
        self.surface = surface
        self.cause = cause


def call_tool(
    url: str, token: str, tool: str, arguments: dict[str, Any], request_id: int
) -> dict[str, Any]:
    """Call once; a transport failure does not establish whether mutation occurred."""
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        },
        allow_nan=False,
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
    # hud_input and question publishes can wait 30 s; leave a response allowance beyond that wait.
    with urllib.request.urlopen(req, timeout=40) as resp:
        envelope = json.loads(resp.read().decode("utf-8"))
    if "error" in envelope:
        return {"error": envelope["error"]}
    result = envelope["result"]
    decoded = json.loads(result["content"][0]["text"])
    return {"error": decoded} if result.get("isError") else {"result": decoded}


def json_object(raw: str) -> dict[str, Any]:
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def parse_content(raw: str) -> Any:
    """An object-looking payload must parse; never publish broken JSON as text."""
    return json_object(raw) if raw.lstrip().startswith("{") else raw


def publish_arguments(message: dict[str, Any]) -> dict[str, Any]:
    """Normalize legacy zone batches and reject misplaced fields before any POST."""
    allowed = {"zone", "surface", "content", "params", "ttl_ms", "delay_ms", "key", "wait_ms"}
    if set(message) - allowed:
        raise ValueError(
            "unknown publish fields: " + ", ".join(sorted(set(message) - allowed))
        )
    if ("zone" in message) == ("surface" in message):
        raise ValueError("supply exactly one of zone or surface")
    if "zone" in message:
        if not isinstance(message["zone"], str) or not message["zone"].strip():
            raise ValueError("zone must be a non-empty string")
        surface = f"zone:{message['zone']}"
    else:
        surface = message["surface"]
    if (
        not isinstance(surface, str)
        or not surface.startswith(("zone:", "widget:"))
        or not surface.split(":", 1)[1].strip()
    ):
        raise ValueError(
            "surface must be zone:<name> or widget:<name>; use hud-projection for portals"
        )
    widget = surface.startswith("widget:")
    payload = "params" if widget else "content"
    if payload not in message or message[payload] is None or message[payload] == "":
        raise ValueError(f"{surface} requires {payload}")
    if ("content" if widget else "params") in message or (
        widget and "delay_ms" in message
    ):
        raise ValueError(f"misplaced content, params or delay_ms for {surface}")
    if widget and not isinstance(message["params"], dict):
        raise ValueError("params must be a JSON object")
    if not widget and not isinstance(message["content"], (str, dict)):
        raise ValueError("content must be text or a JSON object")
    for field in ("ttl_ms", "delay_ms", "wait_ms"):
        if field in message and (type(message[field]) is not int or message[field] < 0):
            raise ValueError(f"{field} must be a nonnegative integer")
    if "wait_ms" in message:
        content = message.get("content")
        if widget or not isinstance(content, dict) or not content.get("actions"):
            raise ValueError("wait_ms requires a zone notification with actions")
    if message.get("delay_ms", 0) > 300000:
        raise ValueError("delay_ms must be at most 300000")
    if "key" in message and not isinstance(message["key"], str):
        raise ValueError("key must be a string")
    arguments = {
        "surface": surface,
        **{k: v for k, v in message.items() if k not in {"zone", "surface"}},
    }
    json.dumps(arguments, allow_nan=False)
    return arguments


def load_messages(path: str) -> list[dict[str, Any]]:
    """Validate the whole batch before sending its first request."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, list) or not data:
        raise ValueError("messages file must be a non-empty JSON array")
    for idx, item in enumerate(data):
        if not isinstance(item, dict):
            raise ValueError(f"message[{idx}] must be an object")
        publish_arguments(item)
    return data


def publish_messages(
    url: str, token: str, messages: list[dict[str, Any]]
) -> tuple[list[dict[str, Any]], bool]:
    arguments = [publish_arguments(msg) for msg in messages]
    results = []
    failed = False
    # Independent calls, not an atomic transaction; receipts name every result.
    for req_id, args in enumerate(arguments, start=10):
        try:
            response = call_tool(url, token, "hud_publish", args, req_id)
        except (
            OSError,
            http.client.HTTPException,
            ValueError,
            KeyError,
            TypeError,
            IndexError,
        ) as error:
            raise PublishInterrupted(results, args["surface"], error) from error
        failed |= "error" in response
        results.append({"surface": args["surface"], "response": response})
    return results, failed


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples (after --list-surfaces; only use an allocated widget):
  --zone notification-area --content '{"body":"Passed"}' --key session:tests --ttl-ms 10000
  --zone notification-area --clear --key session:tests
  --surface widget:main-progress --params '{"progress":0.5}' --ttl-ms 10000
  --surface widget:main-progress --hold --ttl-ms 10000
  --input --wait-ms 30000
  --input --ack <handled-id>   (repeat --ack for more ids; never auto-ACK)
  --messages-file batch.json

Batch: [{"zone":"notification-area","content":{"body":"Passed"},"key":"session:tests","ttl_ms":10000},
        {"surface":"widget:main-progress","params":{"progress":0.5},"ttl_ms":10000}]
Legacy zone batches remain supported. Batches are sequential, not atomic; inspect
all receipts before retrying. No uncertain POST is automatically replayed.
""",
    )
    endpoint = parser.add_mutually_exclusive_group()
    endpoint.add_argument(
        "--host",
        help="HUD host[:port] (otherwise HUD_HOST; no automatic host discovery)",
    )
    endpoint.add_argument(
        "--url", help="MCP endpoint (alias for explicit host selection)"
    )
    parser.add_argument(
        "--psk-env",
        help="Read PSK from this environment variable instead of paired file",
    )
    parser.add_argument(
        "--list-surfaces",
        action="store_true",
        help="Discover allowed surfaces; failure stops further work",
    )
    target = parser.add_mutually_exclusive_group()
    target.add_argument("--zone", help="Zone name (alias for --surface zone:<name>)")
    target.add_argument("--surface", help="zone:<name> or widget:<name>")
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--content", help="Zone text or JSON object")
    operation.add_argument("--params", help="Widget JSON parameter object")
    operation.add_argument(
        "--messages-file", help="JSON array of zone/surface publish objects"
    )
    operation.add_argument(
        "--clear",
        action="store_true",
        help="Clear target; --key selects one own notification",
    )
    operation.add_argument(
        "--hold",
        action="store_true",
        help="Renew target holding; requires --ttl-ms, no key selector",
    )
    operation.add_argument(
        "--input",
        action="store_true",
        help="Read input across this agent; ACK only explicitly given ids",
    )
    parser.add_argument(
        "--key", help="Publish merge key, or exact notification key for --clear"
    )
    parser.add_argument(
        "--ttl-ms", type=int, help="Lifetime in milliseconds; 0 holds until clear"
    )
    parser.add_argument(
        "--delay-ms", type=int, help="Zone-only presentation delay, 0..300000"
    )
    parser.add_argument(
        "--ack",
        action="append",
        help="Handled input id to ACK with --input; repeatable",
    )
    parser.add_argument(
        "--wait-ms", type=int, help="Input or question-publish wait in milliseconds, 0..30000"
    )
    parser.add_argument("--max-items", type=int, help="Input result limit (positive)")
    args = parser.parse_args(argv)
    inline = args.content is not None or args.params is not None
    action = (
        inline
        or args.messages_file is not None
        or args.clear
        or args.hold
        or args.input
    )
    surface = f"zone:{args.zone}" if args.zone is not None else args.surface
    if not (args.list_surfaces or action):
        parser.error("provide --list-surfaces or a publish/hold/clear/input operation")
    if bool(surface) != bool(inline or args.clear or args.hold):
        parser.error(
            "content, params, clear and hold require a target; other operations do not accept one"
        )
    if args.key is not None and not (inline or args.clear):
        parser.error("--key requires a publish or --clear")
    if args.ttl_ms is not None and (args.ttl_ms < 0 or not (inline or args.hold)):
        parser.error("--ttl-ms must be nonnegative and requires a publish or --hold")
    if args.hold and args.ttl_ms is None:
        parser.error("--hold requires --ttl-ms")
    if args.delay_ms is not None and not inline:
        parser.error("--delay-ms requires a zone publish")
    if (
        args.ack is not None or args.max_items is not None
    ) and not args.input:
        parser.error("--ack and --max-items require --input")
    if args.wait_ms is not None and not (args.input or inline):
        parser.error("--wait-ms requires --input or a question publish")
    if args.wait_ms is not None and not 0 <= args.wait_ms <= 30000:
        parser.error("--wait-ms must be 0..30000")
    if args.max_items is not None and args.max_items < 1:
        parser.error("--max-items must be positive")
    if surface and (
        not surface.startswith(("zone:", "widget:"))
        or not surface.split(":", 1)[1].strip()
    ):
        parser.error("target must be zone:<name> or widget:<name>")
    if args.clear and args.key and not surface.startswith("zone:"):
        parser.error("nonempty clear keys require a notification zone")

    try:
        messages = []
        if inline:
            message: dict[str, Any] = {"surface": surface}
            message.update(
                {"content": parse_content(args.content)}
                if args.content is not None
                else {"params": json_object(args.params)}
            )
            message.update(
                {
                    k: v
                    for k, v in {
                        "key": args.key,
                        "ttl_ms": args.ttl_ms,
                        "delay_ms": args.delay_ms,
                        "wait_ms": args.wait_ms,
                    }.items()
                    if v is not None
                }
            )
            publish_arguments(message)
            messages = [message]
        elif args.messages_file is not None:
            messages = load_messages(args.messages_file)
        url, token = hud_env.resolve(args.host or args.url, args.psk_env)
        if args.list_surfaces:
            response = call_tool(url, token, "hud_surfaces", {}, 1)
            print(json.dumps(response, indent=2))
            if "error" in response:
                return 1
        if messages:
            try:
                results, failed = publish_messages(url, token, messages)
            except PublishInterrupted as error:
                print(
                    json.dumps(
                        {
                            "published": error.results,
                            "interrupted": {
                                "surface": error.surface,
                                "outcome": "unknown",
                            },
                        },
                        indent=2,
                    )
                )
                raise error.cause
            print(json.dumps({"published": results}, indent=2))
            return int(failed)
        if args.clear or args.hold:
            arguments = {"surface": surface}
            if args.clear and args.key is not None:
                arguments["key"] = args.key
            if args.hold:
                arguments["ttl_ms"] = args.ttl_ms
            response = call_tool(
                url, token, "hud_clear" if args.clear else "hud_hold", arguments, 2
            )
        elif args.input:
            arguments = {
                k: v
                for k, v in {
                    "ack": args.ack,
                    "wait_ms": args.wait_ms,
                    "max_items": args.max_items,
                }.items()
                if v is not None
            }
            response = call_tool(url, token, "hud_input", arguments, 3)
        else:
            return 0
        print(json.dumps(response, indent=2))
        return int("error" in response)
    except hud_env.HudEnvError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2
    except urllib.error.HTTPError as error:
        # Do not echo an arbitrary response body or request headers.
        print(
            json.dumps({"error": "http_error", "status": error.code}), file=sys.stderr
        )
        return 3
    except (urllib.error.URLError, OSError, http.client.HTTPException) as error:
        print(
            json.dumps(
                {"error": "connection_or_file_error", "kind": type(error).__name__}
            ),
            file=sys.stderr,
        )
        return 4
    except ValueError as error:
        print(
            json.dumps({"error": "validation_error", "detail": str(error)}),
            file=sys.stderr,
        )
        return 5
    except (KeyError, TypeError, IndexError):
        print('{"error":"invalid_mcp_response"}', file=sys.stderr)
        return 6


if __name__ == "__main__":
    raise SystemExit(main())
