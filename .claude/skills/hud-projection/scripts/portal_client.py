#!/usr/bin/env python3
"""Deterministic client for HUD portal projection over MCP.

Portal commands use standard MCP `tools/call` requests (`docs/api.md`):

  surfaces                      hud_surfaces
  publish  --id ID --text T     hud_publish {surface: portal:ID, content: T}
  status   --id ID --state S    hud_publish {surface: portal:ID, status: S}
  poll     [--wait-ms N]        hud_input   (prints items as NDJSON; exit 3 if none)
  ack      --input-id I ...     hud_input   {ack: [I, ...]}
  clear    --id ID              hud_clear   {surface: portal:ID}
  ask      --title T --action Label=id ...  one notification in its own MCP session

The first publish to a portal attaches it. The portal is keyed by your agent
identity (the PSK), so no command handles a token. Environment: HUD_HOST
(the HUD's host); the PSK is read from ~/.config/tze-hud/<host>.psk, written
by `.claude/skills/user-test/scripts/hud_pair.py`.

Started under a Claude session whose tze-hud MCP adapter is running, every
ordinary portal command joins that adapter's HUD session (its private record in
~/.config/tze-hud/sessions/), so a background waiter sees the portals the
model attached through the MCP tools. HUD_HOST, when set, must name the same
HUD; without such a session the client is the agent's unscoped caller.
"""

import argparse
import http.client
import json
import os
import signal
import sys
import time
import urllib.error
import urllib.request
import uuid
from contextlib import contextmanager
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env  # noqa: E402


class ToolError(Exception):
    """A tool result with isError: carries the stable code and hint."""

    def __init__(self, code, hint):
        super().__init__(f"{code}: {hint}")
        self.code = code
        self.hint = hint


NOTIFICATION = "zone:notification-area"
DEFAULT_ASK_TTL_MS = 60_000
MAX_WAIT_MS = 55_000
ASK_POLL_MS = 1_000
ASK_CONTROL_TIMEOUT = 3
TTL_ERROR = "--ttl-ms must be a positive whole number within the HUD's range"


class AskInterrupted(Exception):
    """A clean stop requested by the shell."""


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


def mcp_request(url, token, session_id, method, params=None, request_id=1):
    if method == "DELETE":
        request = urllib.request.Request(url, method="DELETE")
    else:
        body = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        request = urllib.request.Request(url, data=json.dumps(body).encode())
    request.add_unredirected_header("Authorization", f"Bearer {token}")
    request.add_header("Content-Type", "application/json")
    request.add_header("Accept", "application/json, text/event-stream")
    if session_id:
        request.add_unredirected_header("Mcp-Session-Id", session_id)
    return request


def rpc(method, params, request_id=1, *, timeout=60):
    """POST one JSON-RPC request and return the parsed response."""
    url, token = endpoint()
    record = session()
    request = mcp_request(url, token, record["session"] if record else None, method, params, request_id)
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
    return tool_result(response)


def tool_result(response):
    result = response["result"]
    decoded = json.loads(result["content"][0]["text"])
    if result.get("isError"):
        raise ToolError(decoded.get("code"), decoded.get("hint"))
    return decoded


def ask_endpoint():
    """Select the enclosing session's HUD or the usual paired endpoint."""
    try:
        record = session()
        if record:
            selected = {"host": record["host"], "url": record["url"], "explicit": True}
        else:
            selected = hud_env.adapter_endpoint()
        return selected["url"], hud_env.adapter_key(selected)
    except hud_env.HudEnvError as error:
        raise ToolError("HUD_CONFIGURATION", str(error)) from error
    except ValueError as error:
        raise ToolError("HUD_CONFIGURATION", "invalid HUD endpoint or pairing") from error


class AskSession:
    """One scoped question, with optional follow-up tools in the same scope."""

    def __init__(self, label=None):
        self.label = label
        self.session_id = None
        self.key = None
        self.cleared = False

    def __enter__(self):
        self.url, self.token = ask_endpoint()
        folder = os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()
        label = Path(folder).name if self.label is None else self.label
        try:
            self._rpc("initialize", {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "portal-client-ask", "version": "1"},
                "_meta": {"tze-hud/label": label},
            }, timeout=ASK_CONTROL_TIMEOUT)
        except Exception:
            try:
                self._release()
            except ToolError:
                pass
            raise
        return self

    def __exit__(self, _type, error, _traceback):
        try:
            self.clear()
        except ToolError:
            # Releasing the scope also clears the card and fences in-flight work.
            pass
        finally:
            try:
                self._release()
            except ToolError:
                if error is None:
                    raise

    def _release(self):
        if self.session_id is not None:
            try:
                self._rpc("DELETE", timeout=ASK_CONTROL_TIMEOUT)
            except ToolError as error:
                if error.code != "SESSION_ENDED":
                    raise
            self.session_id = None

    def _rpc(self, method, params=None, *, timeout=60):
        if method != "initialize" and self.session_id is None:
            raise ToolError("SESSION_ENDED", "the ask's HUD session has ended")
        request = mcp_request(self.url, self.token, self.session_id, method, params)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                if method == "DELETE":
                    if response.status != 202:
                        raise ToolError("MCP_PROTOCOL", "HUD did not release the ask session")
                    return None
                if method == "initialize":
                    self.session_id = response.headers.get("Mcp-Session-Id") or None
                    if not self.session_id:
                        raise ToolError("MCP_PROTOCOL", "HUD did not open a scoped session")
                reply = json.loads(response.read())
        except urllib.error.HTTPError as error:
            if error.code in (401, 403):
                raise ToolError("MCP_AUTH", f"HUD refused authentication (HTTP {error.code})") from None
            if error.code == 404 and self.session_id is not None:
                raise ToolError("SESSION_ENDED", "the ask's HUD session has ended") from None
            raise ToolError("MCP_TRANSPORT", f"HUD returned HTTP {error.code}") from None
        except (OSError, http.client.HTTPException):
            raise ToolError("MCP_TRANSPORT", "could not reach the HUD or read its response") from None
        except ValueError:
            raise ToolError("MCP_PROTOCOL", "HUD returned invalid JSON") from None
        if not isinstance(reply, dict) or not isinstance(reply.get("result"), dict):
            raise ToolError("MCP_PROTOCOL", f"HUD returned no result for {method}")
        return reply

    def call_tool(self, name, arguments, *, timeout=ASK_CONTROL_TIMEOUT):
        """Call a tool within this question's scope (e.g. a follow-up notice)."""
        response = self._rpc("tools/call", {"name": name, "arguments": arguments}, timeout=timeout)
        try:
            result = tool_result(response)
        except (IndexError, KeyError, TypeError, ValueError, AttributeError):
            raise ToolError("MCP_PROTOCOL", f"HUD returned an invalid {name} result") from None
        if not isinstance(result, dict):
            raise ToolError("MCP_PROTOCOL", f"HUD returned an invalid {name} result")
        return result

    def publish(self, content, ttl_ms=DEFAULT_ASK_TTL_MS, *, key=None):
        """Publish one card with an armed wait. Use wait() to read its outcome."""
        if type(ttl_ms) is not int or not 0 < ttl_ms < 2**64:
            raise ToolError("INVALID_ARGUMENT", TTL_ERROR)
        if self.key is not None:
            raise ToolError("INVALID_ARGUMENT", "the ask session already has a card")
        if key is not None and (not isinstance(key, str) or not key):
            raise ToolError("INVALID_ARGUMENT", "the ask card key must be a nonempty string")
        self.callbacks = {action["callback_id"] for action in content["actions"] if "callback_id" in action}
        if not self.callbacks:
            raise ToolError("INVALID_ARGUMENT", "the card needs a callback action")
        self.key = uuid.uuid4().hex if key is None else key
        self.deadline = time.monotonic() + ttl_ms / 1_000
        try:
            result = self.call_tool("hud_publish", {
                "surface": NOTIFICATION, "content": content, "key": self.key,
                "ttl_ms": ttl_ms, "wait_ms": MAX_WAIT_MS,
            }, timeout=60)
        except ToolError as error:
            if error.code == "SESSION_ENDED":
                result = {"gone": "cleared"}
            elif error.code == "INTERNAL" and error.hint == "input state busy; retry hud_input":
                # The runtime drops the reservation on this path. Never republish.
                result = {}
            else:
                raise
        self.outcome = self._outcome(result)
        if result.get("ok") is True and self.outcome is None:
            remaining = result.get("expires_in_ms")
            if type(remaining) is not int or remaining < 0:
                raise ToolError("MCP_PROTOCOL", "HUD did not report the ask card's expiry")
            self.deadline = time.monotonic() + remaining / 1_000

    def _outcome(self, result):
        if result.get("kind") == "dismiss":
            return {"gone": "dismissed"}
        if result.get("kind") == "displaced":
            return {"gone": "displaced"}
        if isinstance(result.get("gone"), str):
            return {"gone": result["gone"]}
        action = result.get("action")
        if isinstance(action, str) and action in self.callbacks:
            return {"action": action}
        return None

    def _input(self, arguments):
        try:
            return self.call_tool("hud_input", arguments)
        except ToolError as error:
            if not retry_input(error):
                raise
            retry = dict(arguments)
            if retry.get("wait_ms"):
                retry["wait_ms"] = 0
            return self.call_tool("hud_input", retry)

    def _answer(self, wait_ms):
        items = self._input({"wait_ms": wait_ms})["items"]
        for item in items:
            if item.get("s") == NOTIFICATION and item.get("key") == self.key:
                outcome = self._outcome(item)
                if outcome is not None:
                    self._input({"ack": [item["id"]], "max_items": 0})
                    return outcome
        return None

    def _holding(self):
        surfaces = self.call_tool("hud_surfaces", {})["surfaces"]
        for surface in surfaces:
            if surface.get("s") == NOTIFICATION and surface.get("held"):
                return surface
        return None

    def wait(self):
        """Return the armed result or wait for and ACK this card's queued outcome."""
        if self.key is None:
            raise ToolError("INVALID_ARGUMENT", "publish the ask card before waiting")
        try:
            while self.outcome is None:
                now = time.monotonic()
                wait_ms = min(ASK_POLL_MS, max(0, int((self.deadline - now) * 1_000)))
                self.outcome = self._answer(wait_ms)
                if self.outcome is not None:
                    break
                holding = self._holding()
                if holding is None or holding.get("expires_in_ms") == 0 or time.monotonic() >= self.deadline:
                    # Input may have arrived between the empty poll and discovery.
                    self.outcome = self._answer(0)
                    if self.outcome is None:
                        expired = holding is not None or time.monotonic() >= self.deadline
                        self.outcome = {"gone": "expired" if expired else "cleared"}
                    break
                # Retained unrelated input can make the long poll return immediately.
                time.sleep(max(0, (now + wait_ms / 1_000) - time.monotonic()))
        except ToolError as error:
            if error.code != "SESSION_ENDED":
                raise
            self.outcome = {"gone": "cleared"}
        except (TypeError, KeyError, AttributeError):
            raise ToolError("MCP_PROTOCOL", "HUD returned invalid input or surfaces") from None
        return self.outcome

    def clear(self):
        """Clear only this card; the scope stays open for a follow-up notice."""
        if self.key is not None and not self.cleared:
            try:
                self.call_tool("hud_clear", {"surface": NOTIFICATION, "key": self.key})
            except ToolError as error:
                if error.code not in ("NOT_HELD", "SESSION_ENDED"):
                    raise
            self.cleared = True


def retry_input(error):
    return (error.code == "INTERNAL" and isinstance(error.hint, str)
            and error.hint.endswith("retry hud_input"))


@contextmanager
def ask_signals():
    """Interrupt a pending HTTP read and let the scope release on clean stop."""
    interrupted = False

    def stop(_signum, _frame):
        nonlocal interrupted
        if not interrupted:
            interrupted = True
            raise AskInterrupted

    previous = {}
    try:
        for name in ("SIGTERM", "SIGINT", "SIGHUP"):
            signum = getattr(signal, name, None)
            if signum is not None:
                previous[signum] = signal.signal(signum, stop)
        yield
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


def ask_action(value):
    label, _, callback_id = value.partition("=")
    if not label.strip() or not callback_id.strip():
        raise argparse.ArgumentTypeError("use --action LABEL=ID with a nonempty label and id")
    return {"label": label, "callback_id": callback_id}


class AskArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        emit({"code": "INVALID_ARGUMENT", "hint": message})
        sys.exit(1)


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


def cmd_ask(args):
    if not args.title.strip():
        raise ToolError("INVALID_ARGUMENT", "--title must not be empty")
    if len(args.action) > 4:
        raise ToolError("INVALID_ARGUMENT", "notification allows at most 4 actions")
    if not 0 < args.ttl_ms < 2**64:
        raise ToolError("INVALID_ARGUMENT", TTL_ERROR)
    try:
        with ask_signals(), AskSession() as ask:
            ask.publish({"title": args.title, "actions": args.action}, args.ttl_ms)
            outcome = ask.wait()
    except AskInterrupted:
        outcome = {"gone": "cleared"}
    emit(outcome)
    if "gone" in outcome:
        sys.exit(2)


def build_parser(parser_class=argparse.ArgumentParser):
    parser = parser_class(description=__doc__.splitlines()[0])
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

    sp = sub.add_parser("ask", help="wait for one notification answer in its own session")
    sp.add_argument("--title", required=True)
    sp.add_argument("--action", type=ask_action, required=True, action="append", metavar="LABEL=ID")
    sp.add_argument("--ttl-ms", type=int, default=DEFAULT_ASK_TTL_MS)
    sp.set_defaults(func=cmd_ask)
    return parser


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    parser_class = AskArgumentParser if argv and argv[0] == "ask" else argparse.ArgumentParser
    args = build_parser(parser_class).parse_args(argv)
    try:
        args.func(args)
    except ToolError as error:
        emit({"code": error.code, "hint": error.hint})
        sys.exit(1)


if __name__ == "__main__":
    main()
