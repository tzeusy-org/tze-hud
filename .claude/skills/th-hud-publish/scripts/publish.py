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
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402
from hud_mcp import call_tool  # noqa: E402

RESPONSE_ERRORS = (OSError, http.client.HTTPException, ValueError, KeyError, TypeError, IndexError)
PUBLISH_ERRORS = RESPONSE_ERRORS + (KeyboardInterrupt,)


def json_object(raw: str) -> dict[str, Any]:
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def parse_content(raw: str) -> Any:
    """An object-looking payload must parse; never publish broken JSON as text."""
    return json_object(raw) if raw.lstrip().startswith("{") else raw


def publish_arguments(message: dict[str, Any]) -> dict[str, Any]:
    """Accept wire arguments and the zone/widget names used by capture fixtures."""
    targets = {"zone", "widget", "surface"}
    allowed = targets | {"content", "params", "ttl_ms", "delay_ms", "key", "wait_ms"}
    if set(message) - allowed:
        raise ValueError(
            "unknown publish fields: " + ", ".join(sorted(set(message) - allowed))
        )
    supplied = targets & message.keys()
    if len(supplied) != 1:
        raise ValueError("supply exactly one of zone, widget or surface")
    target = supplied.pop()
    name = message[target]
    if not isinstance(name, str) or not name.strip():
        raise ValueError(f"{target} must be a non-empty string")
    surface = name if target == "surface" else f"{target}:{name}"
    if (
        not surface.startswith(("zone:", "widget:"))
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
        **{k: v for k, v in message.items() if k not in targets},
    }
    json.dumps(arguments, allow_nan=False)
    return arguments


def load_messages(path: str) -> list[dict[str, Any]]:
    """Validate the whole batch before sending its first request."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    if not isinstance(data, list) or not data:
        raise ValueError("messages file must be a non-empty JSON array")
    messages = []
    for idx, item in enumerate(data):
        if not isinstance(item, dict):
            raise ValueError(f"message[{idx}] must be an object")
        messages.append(publish_arguments(item))
    return messages


def publish_messages(
    url: str, token: str, messages: list[dict[str, Any]], cleanup: dict[str, bool]
) -> bool:
    results = []
    interrupted = None
    try:
        for req_id, args in enumerate(messages, start=10):
            surface = args["surface"]
            touched = cleanup.get(surface)
            if touched is not None:
                # Keep the cleanup target if the POST has an unknown outcome.
                cleanup[surface] = True
            try:
                response = call_tool(url, token, "hud_publish", args, req_id)
            except PUBLISH_ERRORS:
                interrupted = {"surface": surface, "outcome": "unknown"}
                raise
            if touched is not None and "error" in response:
                cleanup[surface] = touched
            results.append({"surface": surface, "response": response})
    finally:
        receipt = {"published": results}
        if interrupted is not None:
            receipt["interrupted"] = interrupted
        print(json.dumps(receipt, indent=2))
    return any("error" in item["response"] for item in results)


def clear_test_widgets(url: str, token: str, cleanup: dict[str, bool]) -> bool:
    results = []
    for request_id, (surface, touched) in enumerate(cleanup.items(), start=1000):
        if not touched:
            continue
        try:
            response = call_tool(
                url, token, "hud_clear", {"surface": surface}, request_id, timeout=20
            )
        except RESPONSE_ERRORS as error:
            response = {"error": {"kind": type(error).__name__}}
        results.append({"surface": surface, "response": response})
    if results:
        print(json.dumps({"cleanup": results}, indent=2))
    return any("error" in item["response"] for item in results)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""Examples (after --list-surfaces; only use an allocated widget):
  --zone notification-area --content '{"body":"Passed"}' --key session:tests --ttl-ms 10000
  --zone notification-area --clear --key session:tests
  --surface widget:main-progress --params '{"progress":0.5}' --ttl-ms 10000
  --surface widget:main-progress --hold --ttl-ms 10000
  --input --wait-ms 55000
  --input --ack <handled-id>   (repeat --ack for more ids; never auto-ACK)
  --messages-file batch.json

Batch: [{"zone":"notification-area","content":{"body":"Passed"},"key":"session:tests","ttl_ms":10000},
        {"surface":"widget:main-progress","params":{"progress":0.5},"ttl_ms":10000}]
Batch fixtures may use a zone or widget name in place of surface. Each batch
entry is an independent call. Inspect the receipts before any manual retry.
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
    parser.add_argument(
        "--cleanup-on-exit",
        action="store_true",
        help="Clear widgets touched by this run. Use an idle dedicated pairing.",
    )
    target = parser.add_mutually_exclusive_group()
    target.add_argument("--zone", help="Zone name (alias for --surface zone:<name>)")
    target.add_argument("--surface", help="zone:<name> or widget:<name>")
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--content", help="Zone text or JSON object")
    operation.add_argument("--params", help="Widget JSON parameter object")
    operation.add_argument(
        "--messages-file", help="JSON array of zone or widget publishes"
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
        "--wait-ms", type=int, help="Input or question-publish wait in milliseconds, 0..55000"
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
    if args.wait_ms is not None and not 0 <= args.wait_ms <= 55000:
        parser.error("--wait-ms must be 0..55000")
    if args.max_items is not None and args.max_items < 1:
        parser.error("--max-items must be positive")
    if args.cleanup_on_exit and not (inline or args.messages_file is not None):
        parser.error("--cleanup-on-exit requires a publish")
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
            messages = [publish_arguments(message)]
        elif args.messages_file is not None:
            messages = load_messages(args.messages_file)
        cleanup = {}
        if args.cleanup_on_exit:
            cleanup = {
                msg["surface"]: False for msg in messages
                if msg["surface"].startswith("widget:")
            }
        if args.cleanup_on_exit and not cleanup:
            raise ValueError("--cleanup-on-exit requires at least one widget publish")
        url, token = hud_env.resolve(args.host or args.url, args.psk_env)
        if args.list_surfaces or cleanup:
            response = call_tool(url, token, "hud_surfaces", {}, 1)
            print(json.dumps(response, indent=2))
            if "error" in response:
                return 1
            if cleanup:
                entries = response["result"]["surfaces"]
                for surface in cleanup:
                    matches = [entry for entry in entries if entry["s"] == surface]
                    if len(matches) != 1 or matches[0].get("held", False) is not False:
                        raise ValueError(f"{surface} must be discovered and unheld for cleanup")
        if messages:
            try:
                failed = publish_messages(url, token, messages, cleanup)
            finally:
                cleanup_failed = clear_test_widgets(url, token, cleanup)
            return int(failed or cleanup_failed)
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
    except KeyboardInterrupt:
        print('{"error":"interrupted"}', file=sys.stderr)
        return 130
    except (KeyError, TypeError, IndexError):
        print('{"error":"invalid_mcp_response"}', file=sys.stderr)
        return 6


if __name__ == "__main__":
    raise SystemExit(main())
