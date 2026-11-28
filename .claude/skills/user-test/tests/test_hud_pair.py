"""hud_pair.py against a fake HUD: file mode, rotation, and PSK never printed."""

import json
import hashlib
import os
import re
import selectors
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from decimal import Decimal
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
CODE = "482913"
NOT_READY = "HUD session not ready; reconnect the tze-hud MCP server"


class FakeHud(BaseHTTPRequestHandler):
    issued = 0  # class-level: each successful pair mints a new key
    last_psk = ""
    requests = []
    modes = {}
    entered = {}
    release = threading.Event()
    condition = threading.Condition()
    grpc_target = "h:50051"
    sessions = set()  # live Mcp-Session-Id values
    opened = 0
    deletes = []  # (session id, monotonic time) per DELETE /mcp
    inputs = {}  # session id -> hud_input items for that session
    parents = {}
    card_slots = {}
    card_refusal = False
    card_encoded_gate = None
    card_inspect_gate = None
    card_delayed_clears = None

    def do_DELETE(self):
        session = self.headers.get("Mcp-Session-Id")
        with self.condition:
            self.deletes.append((session, time.monotonic()))
            live = session in self.sessions
            self.sessions.discard(session)
            for sibling, parent in self.parents.items():
                if parent == session:
                    self.sessions.discard(sibling)
            self.condition.notify_all()
        self.reply(202 if live else 404, b"")

    def do_GET(self):
        if self.headers.get("Authorization") != f"Bearer {type(self).last_psk}":
            return self.reply(401, {"code": "UNAUTHORIZED"})
        self.reply(200, {"path": self.path})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/mcp":
            authorized = self.headers.get("Authorization") == f"Bearer {type(self).last_psk}"
            request_id = body.get("id")
            session = self.headers.get("Mcp-Session-Id")
            with self.condition:
                self.requests.append({"body": body, "authorized": authorized, "session": session,
                                      "protocol": self.headers.get("MCP-Protocol-Version")})
                self.entered.setdefault(request_id, threading.Event()).set()
                self.condition.notify_all()
            if not authorized:
                return self.reply(401, {"code": "UNAUTHORIZED"})
            if session is not None and (session not in self.sessions
                                        or self.modes.get(request_id) == "unknown-session"):
                return self.reply(404, b"")  # MCP: unknown session, nothing ran
            mode = self.modes.get(request_id)
            if mode == "hold":
                if not self.release.wait(70):
                    return
            if mode == "drop":
                return
            if mode == "invalid":
                return self.reply(200, "not a JSON-RPC object")
            if mode == "redirect":
                self.send_response(302)
                self.send_header("Location", f"http://{self.server.server_address[0]}:{self.server.server_port}/redirect")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if mode == "escaped_key":
                escaped = "".join(f"\\u{ord(c):04x}" for c in type(self).last_psk)
                data = (f'{{"jsonrpc":"2.0","id":{request_id},"result":{{"echo":"{escaped}"}}}}').encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            if mode in {"raw-overflow", "raw-nan", "raw-infinity", "raw-negative-infinity",
                        "raw-float-id", "raw-bool-id", "raw-finite"}:
                numeric = {"raw-overflow": "1e309", "raw-nan": "NaN", "raw-infinity": "Infinity",
                           "raw-negative-infinity": "-Infinity", "raw-finite": "0.125"}
                wire_id = "true" if mode == "raw-bool-id" else json.dumps(
                    float(request_id) if mode == "raw-float-id" else request_id
                )
                result = '{"value":' + numeric[mode] + '}' if mode in numeric else "{}"
                return self.reply(200, ('{"jsonrpc":"2.0","id":' + wire_id + ',"result":' + result + '}').encode())
            if mode == "trickle":
                self.send_response(200)
                self.send_header("Content-Length", "100000")
                self.end_headers()
                try:
                    self.wfile.write(b"{")
                    self.wfile.flush()
                    while not self.release.wait(0.1):
                        self.wfile.write(b" ")
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass
                return
            method = body["method"]
            if method.startswith("notifications/"):
                self.send_response(200)  # Current HUD convention, not spec202 fiction.
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            headers = {}
            if method == "initialize":
                result = {"protocolVersion": "2025-06-18", "serverInfo": {"name": "fake-upstream", "version": "fixture"},
                          "capabilities": {"tools": {"listChanged": False}}, "instructions": "upstream sentinel"}
                with self.condition:
                    parent = body.get("params", {}).get("_meta", {}).get("tze.hud/parent")
                    if parent is not None and (parent not in self.sessions or self.card_refusal == "link"):
                        return self.reply(200, {"jsonrpc":"2.0", "id":request_id,
                                                "error":{"code":-32602,"message":"invalid parent"}})
                    type(self).opened += 1
                    headers["Mcp-Session-Id"] = f"session-{type(self).opened}"
                    self.sessions.add(headers["Mcp-Session-Id"])
                    if parent is not None:
                        self.parents[headers["Mcp-Session-Id"]] = parent
            elif method == "tools/list":
                result = {"tools": [{"name": name, "description": "upstream sentinel", "inputSchema": {"type": "object"}}
                                    for name in ["hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"]]}
            elif method == "tools/call" and request_id == "tze-hud-card":
                if body["params"]["name"] == "hud_surfaces" and self.card_inspect_gate:
                    observed, released = self.card_inspect_gate
                    observed.set()
                    released.wait(3)
                value = self.card_call(session, body["params"])
                result = {"content": [{"type": "text", "text": json.dumps(value)}]}
                if value is None:
                    result["isError"] = True
                gate = None
                with self.condition:
                    if self.card_encoded_gate and value and value.get("items"):
                        gate = self.card_encoded_gate
                        type(self).card_encoded_gate = None
                if gate:
                    gate[0].set()
                    gate[1].wait(3)
            elif method == "tools/call" and body["params"].get("name") == "hud_input" and session in self.inputs:
                text = json.dumps({"items": self.inputs[session]})
                result = {"content": [{"type": "text", "text": text}]}
            elif method == "tools/call":
                result = {"isError": True, "content": [{"type": "text", "text": '{"code":"FIXTURE","hint":"雪"}'}]}
            else:
                result = {}
            if mode == "large":
                result = {"content": "z" * 700000}
            try:
                return self.reply(200, {"jsonrpc": "2.0", "id": request_id, "result": result}, headers)
            except (BrokenPipeError, ConnectionResetError):
                return
        if self.path != "/pair" or body.get("code") != CODE:
            return self.reply(403, {"code": "PAIR_CODE_INVALID", "hint": "wrong code"})
        type(self).issued += 1
        psk = f"{type(self).issued:02d}" + "ab" * 31
        type(self).last_psk = psk
        self.reply(200, {"agent": body["agent"], "psk": psk, "mcp": "http://h:9090/mcp", "grpc": type(self).grpc_target})

    def reply(self, status, payload, headers=None):
        data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def card_call(self, session, params):
        name, args = params["name"], params["arguments"]
        with self.condition:
            slots = self.card_slots.setdefault(session, {})
            if name == "hud_publish":
                if self.card_refusal:
                    return None
                assert args["content"]["type"] == "notification"
                ttl_ms = args.get("ttl_ms", 60_000)
                slots[args["key"]] = {**args, "ttl_ms": ttl_ms}
                self.condition.notify_all()
                return {"ok": True, "expires_in_ms": ttl_ms}
            if name == "hud_clear":
                if self.card_delayed_clears is None:
                    slots.pop(args["key"], None)
                else:
                    self.card_delayed_clears.append((session, args["key"]))
                self.condition.notify_all()
                return {"ok": True}
            if name == "hud_surfaces":
                return {"surfaces": [{"s": "zone:notification-area", "held": bool(slots),
                                      "expires_in_ms": max((a["ttl_ms"] for a in slots.values()), default=0)}]}
            if name == "hud_input":
                items = self.inputs.setdefault(session, [])
                items[:] = [item for item in items if item["id"] not in args.get("ack", [])]
                if args.get("max_items") == 0:
                    return {"items": []}
                self.condition.wait_for(lambda: items or session not in self.sessions,
                                        args.get("wait_ms", 0) / 1000)
                return {"items": list(items)}
            raise AssertionError("unexpected hook card tool")

    def log_message(self, *args):
        pass


@pytest.fixture
def hud(tmp_path):
    FakeHud.issued = 0
    FakeHud.grpc_target = "h:50051"
    FakeHud.requests, FakeHud.modes, FakeHud.entered = [], {}, {}
    FakeHud.sessions, FakeHud.opened, FakeHud.deletes, FakeHud.inputs = set(), 0, [], {}
    FakeHud.parents, FakeHud.card_slots, FakeHud.card_refusal = {}, {}, False
    FakeHud.card_encoded_gate, FakeHud.card_delayed_clears = None, None
    FakeHud.card_inspect_gate = None
    FakeHud.release = threading.Event()
    server = ThreadingHTTPServer(("127.0.0.1", 0), FakeHud)
    # Teardown is fixture housekeeping, not part of an adapter deadline.
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.01), daemon=True)
    thread.start()
    yield f"127.0.0.1:{server.server_port}", tmp_path
    FakeHud.release.set()
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)
    assert not thread.is_alive()


def endpoint_file(home):
    return psk_file(home).with_suffix(".endpoint.json")


def wait_request(request_id, timeout=3):
    with FakeHud.condition:
        assert FakeHud.condition.wait_for(lambda: request_id in FakeHud.entered, timeout), f"no HTTP request {request_id}"


def owned_children(parent):
    try:
        ids = Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
    except (FileNotFoundError, ProcessLookupError):
        ids = []
    result = {}
    for pid in ids:
        try:
            text = Path(f"/proc/{pid}/stat").read_text()
            values = text[text.rindex(")") + 2:].split()
            result[int(pid)] = {"start": values[19], "state": values[0], "parent": int(values[1])}
        except (FileNotFoundError, ProcessLookupError):
            pass
    return result


def child_live(pid, identity):
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return False
    values = text[text.rindex(")") + 2:].split()
    return values[19] == identity["start"] and values[0] != "Z"


class AdapterProcess:
    """One real shipped entry or private same-loop injection, isolated fake HOME."""
    def __init__(self, home, *, host=None, config=".mcp.json", short=None, stall=None,
                 runtime=None, mirror=False):
        project = SCRIPTS.parents[3]
        self.home = home
        self.short = short
        self.ready_complete = False
        self.short_deadline_marker = home / "enable-short-request-deadline"
        self.short_deadline_marker.unlink(missing_ok=True)
        self.env = {**os.environ, "HOME": str(home), "CLAUDE_PROJECT_DIR": str(project)}
        self.env.pop("HUD_HOST", None)
        self.env.pop("XDG_RUNTIME_DIR", None)
        self.env.pop("TZE_HUD_MIRROR", None)
        if runtime is not None:
            self.env["XDG_RUNTIME_DIR"] = str(runtime)
        if mirror:
            self.env["TZE_HUD_MIRROR"] = "1"
        if host is not None:
            self.env["HUD_HOST"] = host
        entry = json.loads((project / config).read_text())["mcpServers"]["tze-hud"]
        command = [entry["command"], *entry["args"]]
        if short is not None or stall is not None:
            inject = {"OUTPUT_STALL_SECONDS": stall} if stall is not None else {}
            code_lines = [
                "import os,runpy,sys",
                "m=runpy.run_path(sys.argv[1],run_name='adapter_fixture')",
                f"m['main'].__globals__.update({inject!r})",
            ]
            if short is not None:
                code_lines.extend([
                    f"marker={str(self.short_deadline_marker)!r}",
                    "original_frames=m['Adapter'].frames",
                    "def apply_short_deadline(self):",
                    "    if os.path.exists(marker):",
                    f"        m['main'].__globals__['REQUEST_SECONDS']={short!r}",
                    "    return original_frames(self)",
                    "m['Adapter'].frames=apply_short_deadline",
                ])
            code_lines.extend(["sys.argv=sys.argv[:1]", "raise SystemExit(m['main']())"])
            code = "\n".join(code_lines)
            command = [sys.executable, "-u", "-c", code, str(SCRIPTS / "hud_mcp_stdio.py")]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        env=self.env, bufsize=0, start_new_session=True)
        self.pidfd = os.pidfd_open(self.process.pid)
        self.children = {}
        self.buffer = bytearray()
        os.set_blocking(self.process.stdin.fileno(), False)
        os.set_blocking(self.process.stdout.fileno(), False)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.process.stdout, selectors.EVENT_READ)

    def __enter__(self):
        return self

    def sample(self):
        self.children.update(owned_children(self.process.pid))
        (self.home / "adapter-owned-identities.json").write_text(json.dumps(self.children))

    def send(self, method, request_id=None, params=None, *, raw=None):
        body = {"jsonrpc": "2.0", "method": method}
        if request_id is not None:
            body["id"] = request_id
        if params is not None:
            body["params"] = params
        data = memoryview((json.dumps(body, ensure_ascii=False).encode() if raw is None else raw) + b"\n")
        end = time.monotonic() + 3
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdin, selectors.EVENT_WRITE)
            while data:
                assert selector.select(max(0, end - time.monotonic())), "stdin stalled"
                data = data[os.write(self.process.stdin.fileno(), data):]
        self.sample()

    def receive(self, timeout=3, *, strict=False):
        end = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            self.sample()
            assert self.selector.select(max(0, end - time.monotonic())), "no protocol response"
            data = os.read(self.process.stdout.fileno(), 16384)
            assert data, "adapter stdout ended"
            self.buffer.extend(data)
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        if not strict:
            return json.loads(line)

        def reject_constant(_):
            raise ValueError("nonfinite numeric token")

        try:
            response = json.loads(line, parse_float=Decimal, parse_constant=reject_constant)
        except (ValueError, UnicodeError):
            raise AssertionError("adapter emitted invalid numeric JSON on MCP stdout") from None
        assert isinstance(response, dict) and response.get("jsonrpc") == "2.0", "invalid MCP response object"
        request_id = response.get("id")
        assert (request_id is None and "error" in response) or type(request_id) in (str, int), "invalid MCP response ID"
        return response

    def ready(self):
        self.send("initialize", 1, {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fixture", "version": "1"}})
        result = self.receive()
        assert result["result"]["serverInfo"]["name"] == "fake-upstream"
        self.send("notifications/initialized")
        self.send("ping", 2)
        assert self.receive() == {"jsonrpc": "2.0", "id": 2, "result": {}}
        self.ready_complete = True
        return result

    def enable_short_request_deadline(self):
        assert self.short is not None
        assert self.ready_complete, "adapter must finish readiness before shortening its request deadline"
        self.short_deadline_marker.touch()

    def stop(self, sig=None):
        self.sample()
        started = time.monotonic()
        if self.process.poll() is None:
            if sig is None:
                self.process.stdin.close()
            else:
                signal.pidfd_send_signal(self.pidfd, sig)
        self.process.wait(timeout=3)
        end = time.monotonic() + 3
        while any(child_live(pid, identity) for pid, identity in self.children.items()):
            assert time.monotonic() < end, "owned worker stayed LIVE"
            threading.Event().wait(0.01)
        self.elapsed_cleanup = time.monotonic() - started
        self.stderr = self.process.stderr.read().decode()
        assert "Traceback" not in self.stderr
        return self.process.returncode

    def __exit__(self, *_):
        try:
            self.stop()
        finally:
            for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                if not stream.closed:
                    stream.close()
            self.selector.close()
            os.close(self.pidfd)


def pair(host, home, code=CODE, *extra):
    env = {**os.environ, "HOME": str(home), "HUD_HOST": host}
    return subprocess.run(
        [sys.executable, str(SCRIPTS / "hud_pair.py"), "--code", code, *extra],
        capture_output=True, text=True, env=env,
    )


def psk_file(home):
    return home / ".config" / "tze-hud" / "127.0.0.1.psk"


HOOK = SCRIPTS.parents[1] / "hud-projection/scripts/portal_hook.py"
PROMPT = "e53cb2ed-891a-4c88-9a7b-c64a41f06cf2"
STATE_METHOD = "notifications/tze/session_state"


def hook_event(adapter, event, **fields):
    payload = {"hook_event_name": event, "session_id": "fixture",
               "prompt_id": PROMPT, **fields}
    result = subprocess.run([sys.executable, str(HOOK)], input=json.dumps(payload).encode(),
                            capture_output=True, timeout=2, env=adapter.env)
    assert (result.returncode, result.stdout, result.stderr) == (0, b"", b"")


def wait_call(predicate, timeout=3):
    with FakeHud.condition:
        assert FakeHud.condition.wait_for(lambda: any(map(predicate, FakeHud.requests)), timeout)
        return next(filter(predicate, FakeHud.requests))


def wait_state(state, session=None):
    return wait_call(lambda entry: entry["body"]["method"] == STATE_METHOD
                     and entry["body"]["params"]["state"] == state
                     and (session is None or entry["session"] == session))


def card_calls(name):
    return [entry for entry in FakeHud.requests if entry["body"].get("id") == "tze-hud-card"
            and entry["body"].get("params", {}).get("name") == name]


def wait_card(kind="question"):
    with FakeHud.condition:
        assert FakeHud.condition.wait_for(
            lambda: any(kind in slots for slots in FakeHud.card_slots.values()), 3,
        )
        return next((session, slots[kind]) for session, slots in FakeHud.card_slots.items() if kind in slots)


def deliver_card_action(session, key, item_id, action, **fields):
    with FakeHud.condition:
        items = FakeHud.inputs.setdefault(session, [])
        items.append({"s": "zone:notification-area", "key": key, "id": item_id,
                      "action": action, **fields})
        FakeHud.condition.notify_all()


def card_peer(adapter, *, kind="question", ttl_ms=4000):
    runtime = Path(adapter.env["XDG_RUNTIME_DIR"])
    peer = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    peer.settimeout(2)
    peer.connect(str(runtime / "tze-hud" / f"claude-{os.getpid()}.sock"))
    peer.sendall(json.dumps({"v":1, "op":"card", "kind":kind, "title":"fixture",
                             "body":"", "actions":["Continue"], "ttl_ms":ttl_ms}).encode())
    return peer


def receive_card(peer):
    return json.loads(peer.recv(16384))


def test_card_client_uses_a_sibling_and_acks_only_its_input(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            command = [sys.executable, "-c",
                       "import json,runpy,sys; request=runpy.run_path(sys.argv[1])['request_card']; "
                       "print(json.dumps(request('question','Fixture','', ['dismiss'], timeout_ms=4000)))",
                       str(HOOK)]
            with subprocess.Popen(command, env=adapter.env, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE) as client:
                sibling, card = wait_card()
                primary = FakeHud.parents[sibling]
                assert sibling != primary
                assert card["surface"] == "zone:notification-area" and card["key"] == "question"
                action = card["content"]["actions"][0]["callback_id"]
                assert re.fullmatch(r"[0-9a-f]{16}\.0", action)
                FakeHud.inputs[primary] = []
                FakeHud.modes[99] = "hold"
                adapter.send("tools/call", 99, {"name":"hud_input", "arguments":{"wait_ms":1000}})
                assert wait_call(lambda entry: entry["body"].get("id") == 99)["session"] == primary
                deliver_card_action(sibling, "question", "answer1", action)
                out, err = client.communicate(timeout=3)
                assert (client.returncode, err, json.loads(out)) == (0, b"", {"end":"answer", "index":0})
            assert wait_call(lambda entry: entry["body"].get("params", {}).get("arguments", {}).get("ack")
                             == ["answer1"])["session"] == sibling
            FakeHud.release.set()
            assert json.loads(adapter.receive()["result"]["content"][0]["text"]) == {"items":[]}
            assert not card_calls("hud_clear")
            assert sum(entry["body"]["params"]["arguments"].get("wait_ms") == 0
                       for entry in card_calls("hud_input")) == 1


def test_disconnect_clears_one_card_and_refusal_logs_no_content(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            with card_peer(adapter, kind="nudge") as peer:
                assert receive_card(peer) == {"ok": True, "expires_in_ms": 4000}
            clear = wait_call(lambda entry: entry["body"].get("params", {}).get("name") == "hud_clear")
            assert clear["body"]["params"]["arguments"] == {
                "surface":"zone:notification-area", "key":"nudge",
            }
            FakeHud.card_refusal = True
            with card_peer(adapter) as peer:
                assert receive_card(peer) == {"end":"error"}
            assert len(card_calls("hud_clear")) == 1
        assert re.search(r"card delivery failed \[[a-f0-9]{12}\]", adapter.stderr)
        assert "fixture" not in adapter.stderr and FakeHud.last_psk not in adapter.stderr


@pytest.mark.parametrize("source", ["retained", "encoded", "old-callback"])
def test_replacement_keeps_option_d_when_old_input_crosses_the_worker(hud, source):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            with card_peer(adapter) as old:
                assert receive_card(old)["ok"] is True
                sibling, old_card = wait_card()
                old_action = old_card["content"]["actions"][0]["callback_id"]
                if source == "encoded":
                    encoded, release = threading.Event(), threading.Event()
                    FakeHud.card_encoded_gate = (encoded, release)
                    deliver_card_action(sibling, "question", "old", "dismiss", kind="dismiss")
                    assert encoded.wait(2)
                with card_peer(adapter) as new:
                    assert receive_card(new)["ok"] is True
                    assert receive_card(old) == {"end":"superseded"}
                    current = wait_card()[1]["content"]["actions"][0]["callback_id"]
                    assert current != old_action
                    if source == "encoded":
                        release.set()
                    elif source == "retained":
                        deliver_card_action(sibling, "question", "old", "dismiss", kind="dismiss")
                    else:
                        deliver_card_action(sibling, "question", "old", old_action)
                        deliver_card_action(sibling, "question", "old", old_action)
                    ack = wait_call(lambda entry: "old" in entry["body"].get("params", {})
                                    .get("arguments", {}).get("ack", []))
                    assert ack["session"] == sibling
                    if source == "old-callback":
                        deliver_card_action(sibling, "question", "actual", current)
                        assert receive_card(new) == {"end":"answer", "index":0}
                    else:
                        assert receive_card(new) == {"end":"dismissed"}
                assert not card_calls("hud_clear")


@pytest.mark.parametrize("cascade", [False, True])
def test_scope_loss_cancels_waiters_and_relinks_on_the_next_card(hud, cascade):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            with card_peer(adapter) as live:
                assert receive_card(live)["ok"] is True
                old_sibling, _ = wait_card()
                old_parent = FakeHud.parents[old_sibling]
                with FakeHud.condition:
                    FakeHud.sessions.remove(old_sibling)
                    FakeHud.card_slots.pop(old_sibling)
                    if cascade:
                        FakeHud.sessions.remove(old_parent)
                    FakeHud.condition.notify_all()
                assert receive_card(live) == {"end":"cancelled"}
            assert FakeHud.opened == 2
            with card_peer(adapter) as next_card:
                assert receive_card(next_card)["ok"] is True
                sibling, card = wait_card()
                parent = FakeHud.parents[sibling]
                assert sibling != old_sibling
                assert (parent != old_parent) == cascade
                links = [e["body"]["params"]["_meta"]["tze.hud/parent"] for e in FakeHud.requests
                         if "tze.hud/parent" in e["body"].get("params", {}).get("_meta", {})]
                assert links == [old_parent, parent]
                deliver_card_action(sibling, "question", "new", card["content"]["actions"][0]["callback_id"])
                assert receive_card(next_card) == {"end":"answer", "index":0}


@pytest.mark.parametrize("failure", ["link", "unreachable"])
def test_card_admission_fails_without_a_wait_or_a_second_initialize(hud, failure):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            if failure == "link":
                FakeHud.card_refusal = "link"
            else:
                FakeHud.modes["tze-hud-card"] = "drop"
            started = time.monotonic()
            with card_peer(adapter) as peer:
                assert receive_card(peer) == {"end":"error"}
            assert time.monotonic() - started < 1
            linked = [entry for entry in FakeHud.requests if entry["body"].get("id") == "tze-hud-card"]
            assert len(linked) == 1 and linked[0]["body"]["method"] == "initialize"
            assert not card_calls("hud_input") and not card_calls("hud_publish")


@pytest.mark.parametrize("other_kind", [False, True])
def test_card_zone_hold_rearms_without_clearing(hud, other_kind):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            with card_peer(adapter, ttl_ms=180) as question:
                assert receive_card(question)["expires_in_ms"] == 180
                sibling, card = wait_card()
                if other_kind:
                    nudge = card_peer(adapter, kind="nudge", ttl_ms=700)
                    assert receive_card(nudge)["ok"] is True
                    with FakeHud.condition:
                        FakeHud.card_slots[sibling].pop("question")
                else:
                    nudge = None
                try:
                    wait_call(lambda entry: entry["body"].get("params", {}).get("name") == "hud_surfaces")
                    question.settimeout(0.08)
                    with pytest.raises(TimeoutError):
                        receive_card(question)
                    question.settimeout(3)
                    if other_kind:
                        with FakeHud.condition:
                            FakeHud.card_slots[sibling].clear()
                        assert receive_card(question) == {"end":"expired"}
                        assert receive_card(nudge) == {"end":"expired"}
                    else:
                        deliver_card_action(sibling, "question", "unlocked", card["content"]["actions"][0]["callback_id"])
                        assert receive_card(question) == {"end":"answer", "index":0}
                    assert not card_calls("hud_clear")
                finally:
                    if nudge is not None:
                        nudge.close()


def test_delayed_clear_may_cancel_replacement_and_is_not_retried(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            FakeHud.card_delayed_clears = []
            with card_peer(adapter) as old:
                assert receive_card(old)["ok"] is True
                sibling, _ = wait_card()
            with FakeHud.condition:
                assert FakeHud.condition.wait_for(lambda: bool(FakeHud.card_delayed_clears), 2)
            with card_peer(adapter, ttl_ms=180) as new:
                assert receive_card(new)["ok"] is True
                with FakeHud.condition:
                    assert FakeHud.card_delayed_clears == [(sibling, "question")]
                    FakeHud.card_slots[sibling].pop("question")
                assert receive_card(new) == {"end":"expired"}
            assert len(card_calls("hud_clear")) == 1
            assert len(card_calls("hud_surfaces")) == 1


def test_accepted_action_wins_when_it_empties_the_zone_during_expiry_inspection(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-card-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            inspection, release = threading.Event(), threading.Event()
            FakeHud.card_inspect_gate = (inspection, release)
            with card_peer(adapter, ttl_ms=50) as peer:
                assert receive_card(peer)["ok"] is True
                sibling, card = wait_card()
                assert inspection.wait(2)
                with FakeHud.condition:
                    FakeHud.card_slots[sibling].pop("question")
                deliver_card_action(sibling, "question", "during-inspect",
                                    card["content"]["actions"][0]["callback_id"])
                release.set()
                assert receive_card(peer) == {"end":"answer", "index":0}
            assert not card_calls("hud_clear")


def test_two_adapters_isolate_cards_and_keep_all_four_model_slots(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-a-") as runtime_a, tempfile.TemporaryDirectory(prefix="hud-b-") as runtime_b:
        with AdapterProcess(home, host=host, runtime=runtime_a) as first, AdapterProcess(home, host=host, runtime=runtime_b) as second:
            first.ready()
            second.ready()
            for request_id in range(50, 54):
                FakeHud.modes[request_id] = "hold"
                second.send("ping", request_id)
                wait_call(lambda entry: entry["body"].get("id") == request_id)
            with card_peer(first) as a:
                assert receive_card(a)["ok"] is True
                a_scope, a_card = wait_card()
                with card_peer(second) as b:
                    assert receive_card(b)["ok"] is True
                    b_scope, = FakeHud.card_slots.keys() - {a_scope}
                    b_card = FakeHud.card_slots[b_scope]["question"]
                    assert FakeHud.parents[a_scope] != FakeHud.parents[b_scope]
                    deliver_card_action(b_scope, "question", "B", b_card["content"]["actions"][0]["callback_id"])
                    assert receive_card(b) == {"end":"answer", "index":0}
                    deliver_card_action(a_scope, "question", "A", a_card["content"]["actions"][0]["callback_id"])
                    assert receive_card(a) == {"end":"answer", "index":0}
            FakeHud.release.set()
            assert {second.receive()["id"] for _ in range(4)} == set(range(50, 54))


def test_live_hook_state_and_mirror_share_the_primary_scope(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-hook-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime, mirror=True) as adapter:
            adapter.ready()
            path = Path(runtime) / "tze-hud" / f"claude-{os.getpid()}.sock"
            assert path.is_socket()
            hook_event(adapter, "UserPromptSubmit")
            primary = wait_state("working")["session"]
            assert primary is not None
            hook_event(adapter, "PreToolUse", tool_name="AskUserQuestion",
                       tool_use_id="tool-q", tool_input={"body": "ignored private canary"})
            assert wait_state("blocked")["body"]["params"] == {"state": "blocked", "reason": "question"}
            hook_event(adapter, "PostToolUse", tool_name="AskUserQuestion", tool_use_id="tool-q")
            hook_event(adapter, "Stop", last_assistant_message="Fixture finished.")
            assert wait_state("done")["session"] == primary
            final = wait_call(lambda entry: entry["body"].get("params", {}).get("arguments", {}).get("content")
                              == "Fixture finished.")
            assert final["session"] == primary
            assert final["body"]["params"]["arguments"]["surface"] == "portal:fixture"
            assert "private canary" not in json.dumps(FakeHud.requests)
            adapter.send("ping", 100)
            assert adapter.receive()["id"] == 100
        assert not path.exists()
        assert [sid for sid, _ in FakeHud.deletes] == [primary]


def test_malformed_packets_and_unsafe_listener_leave_model_calls_intact(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-hook-") as runtime:
        directory = Path(runtime) / "tze-hud"
        directory.mkdir()
        directory.chmod(0o750)
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            assert list(directory.iterdir()) == []
        directory.chmod(0o700)
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            path = directory / f"claude-{os.getpid()}.sock"
            for packet in (b"{", b"*" * (16 * 1024 + 1)):
                with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as peer:
                    peer.settimeout(2)
                    peer.connect(str(path))
                    peer.sendall(packet)
                    assert peer.recv(1) == b""
            assert all(entry["body"]["method"] != STATE_METHOD for entry in FakeHud.requests)
            hook_event(adapter, "UserPromptSubmit")
            assert wait_state("working")["session"] is not None
            adapter.send("ping", 100)
            assert adapter.receive()["id"] == 100
            assert all(entry["body"]["method"] != "tools/call" for entry in FakeHud.requests)


def test_internal_state_404_reinitializes_once_and_resends_current_state(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-hook-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            original, = FakeHud.sessions
            with FakeHud.condition:
                FakeHud.sessions.clear()
            hook_event(adapter, "UserPromptSubmit")
            started = wait_state("working", original)
            assert started["session"] == original
            wait_state("working", "session-2")
            adapter.send("ping", 100)
            assert adapter.receive()["id"] == 100
            assert FakeHud.opened == 2


def test_hook_lane_does_not_take_one_of_the_four_model_workers(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-hook-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime) as adapter:
            adapter.ready()
            ids = ["model-1", "model-2", "model-3", "model-4"]
            FakeHud.modes.update({request_id: "hold" for request_id in [*ids, None]})
            hook_event(adapter, "UserPromptSubmit")
            wait_state("working")
            for request_id in ids:
                adapter.send("ping", request_id)
                wait_request(request_id)
            assert len(owned_children(adapter.process.pid)) == 5
            assert not adapter.selector.select(0)
            FakeHud.release.set()
            assert {adapter.receive()["id"] for _ in ids} == set(ids)


def test_failed_mirror_delivery_logs_only_a_category_and_key_hash(hud):
    host, home = hud
    assert pair(host, home).returncode == 0
    with tempfile.TemporaryDirectory(prefix="hud-hook-") as runtime:
        with AdapterProcess(home, host=host, runtime=runtime, mirror=True) as adapter:
            adapter.ready()
            FakeHud.modes["tze-hud-presence"] = "drop"
            hook_event(adapter, "Stop", last_assistant_message="synthetic final canary")
            wait_request("tze-hud-presence")
            with selectors.DefaultSelector() as selector:
                selector.register(adapter.process.stderr, selectors.EVENT_READ)
                assert selector.select(2), "mirror failure was not logged"
            detail = adapter.process.stderr.readline().decode().strip()
            assert re.fullmatch(r"hud_mcp_stdio: presence delivery failed \[[a-f0-9]{12}\]", detail)
            assert FakeHud.last_psk not in detail


def test_pair_writes_0600_file_and_never_prints_the_psk(hud):
    host, home = hud
    result = pair(host, home)
    assert result.returncode == 0, result.stderr
    psk = psk_file(home).read_text().strip()
    assert len(psk) == 64
    assert stat.S_IMODE(psk_file(home).stat().st_mode) == 0o600
    assert psk not in result.stdout + result.stderr
    assert json.loads(result.stdout)["psk_file"] == str(psk_file(home))

    record = json.loads(endpoint_file(home).read_text())
    assert record == {"schema": 2, "mcp_url": f"http://{host}/mcp", "psk_sha256": hashlib.sha256(psk.encode()).hexdigest(),
                      "grpc_target": "h:50051"}
    assert stat.S_IMODE(endpoint_file(home).stat().st_mode) == 0o600
    assert psk not in endpoint_file(home).read_text()
    for config in [".mcp.json", ".claude/skills/hud-projection/mcp.template.json", ".claude/skills/th-hud-publish/mcp.template.json"]:
        with AdapterProcess(home, config=config) as adapter:
            adapter.ready()
            adapter.send("tools/list", 3)
            tools = adapter.receive()["result"]["tools"]
            assert [t["name"] for t in tools] == ["hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"]
            assert all(t["description"] == "upstream sentinel" for t in tools)
        assert psk not in adapter.stderr
    assert all(r["authorized"] for r in FakeHud.requests)


def test_repair_rotates_the_key(hud):
    host, home = hud
    pair(host, home)
    first = psk_file(home).read_text()
    assert pair(host, home).returncode == 0
    assert psk_file(home).read_text() != first
    assert stat.S_IMODE(psk_file(home).stat().st_mode) == 0o600

    with AdapterProcess(home) as adapter:
        adapter.ready()
        assert pair(host, home).returncode == 0
        rotated = psk_file(home).read_text().strip()
        assert json.loads(endpoint_file(home).read_text())["psk_sha256"] == hashlib.sha256(rotated.encode()).hexdigest()
        adapter.send("ping", 3)
        assert adapter.receive()["result"] == {}
        assert FakeHud.requests[-1]["authorized"]
        record = json.loads(endpoint_file(home).read_text())
        record["psk_sha256"] = "0" * 64
        endpoint_file(home).write_text(json.dumps(record))
        previous = len(FakeHud.requests)
        adapter.send("ping", 4)
        assert adapter.receive()["error"]["code"] == -32603
        assert len(FakeHud.requests) == previous


def test_wrong_code_reports_the_code_and_writes_nothing(hud):
    host, home = hud
    result = pair(host, home, "000000")
    assert result.returncode != 0
    assert "PAIR_CODE_INVALID" in result.stderr
    assert not psk_file(home).exists()

    assert not endpoint_file(home).exists()
    with AdapterProcess(home) as adapter:
        assert adapter.stop() != 0
        assert adapter.process.stdout.read() == b""
        assert "hud_pair.py --host <HUD-address[:port]> --code <on-screen-code>" in adapter.stderr
    assert not FakeHud.requests


def test_write_failure_has_no_traceback_and_no_psk(hud):
    host, home = hud
    psk_file(home).mkdir(parents=True)  # a directory where the file must go
    (psk_file(home) / "x").write_text("x")
    result = pair(host, home)
    assert result.returncode == 1
    assert "hud_pair: IsADirectoryError talking to the HUD or writing the PSK file" in result.stderr
    assert "Traceback" not in result.stderr
    assert "ab" * 31 not in result.stdout + result.stderr

    assert not endpoint_file(home).exists()
    (psk_file(home) / "x").unlink()
    psk_file(home).rmdir()
    endpoint_file(home).mkdir()
    (endpoint_file(home) / "x").write_text("endpoint obstruction")
    partial = pair(host, home)
    assert partial.returncode == 1 and psk_file(home).is_file()
    assert "IsADirectoryError" in partial.stderr and not partial.stdout
    assert "Traceback" not in partial.stderr and FakeHud.last_psk not in partial.stderr
    (endpoint_file(home) / "x").unlink()
    endpoint_file(home).rmdir()
    assert pair(host, home).returncode == 0
    for request_id, mode in [(31, "invalid"), (32, "drop")]:
        FakeHud.modes[request_id] = mode
        with AdapterProcess(home) as adapter:
            adapter.ready()
            adapter.send("tools/call", request_id, {"name": "hud_publish", "arguments": {"surface": "zone:fixture", "content": "雪"}})
            assert adapter.receive()["error"]["code"] == -32603
        assert sum(r["body"].get("id") == request_id for r in FakeHud.requests) == 1
        assert FakeHud.last_psk not in adapter.stderr


def test_hud_env_derives_urls_and_reads_the_file(hud, monkeypatch):
    host, home = hud
    monkeypatch.syspath_prepend(str(SCRIPTS))
    monkeypatch.setenv("HOME", str(home))
    import hud_env

    assert hud_env.mcp_url("100.1.2.3") == "http://100.1.2.3:9090/mcp"
    assert hud_env.mcp_url("http://h:8080") == "http://h:8080/mcp"
    assert hud_env.grpc_target("h:8080") == "h:50051"
    with pytest.raises(hud_env.HudEnvError, match="pair first"):
        hud_env.load_psk("h")
    pair(host, home)
    assert hud_env.load_psk("127.0.0.1:1") == psk_file(home).read_text().strip()
    assert hud_env.grpc_target(host) == "h:50051"

    # Ordinary clients share these origins; parsing strips brackets from the
    # hostname identity used by the paired file, but HTTP authorities need them.
    from urllib.parse import urlsplit

    for address in ("::1", "fd7a:115c:a1e0::1"):
        key_file = home / ".config" / "tze-hud" / f"{address}.psk"
        key_file.write_bytes(psk_file(home).read_bytes())
        key_file.chmod(0o600)
        try:
            for raw, port in [(f"[{address}]", 9090), (f"[{address}]:8080", 8080),
                              (f"http://[{address}]/", 9090), (f"http://[{address}]:8080/mcp", 8080)]:
                origin = f"http://[{address}]:{port}"
                assert hud_env.hostname(raw) == address
                assert hud_env.psk_path(raw) == key_file
                assert hud_env.mcp_url(raw) == f"{origin}/mcp"
                assert hud_env.base_url(raw) == origin
                parsed = urlsplit(hud_env.mcp_url(raw))
                assert (parsed.hostname, parsed.port, parsed.path) == (address, port, "/mcp")
                assert hud_env.grpc_target(raw) == f"[{address}]:50051"
                with monkeypatch.context() as ipv6_env:
                    ipv6_env.setenv("HUD_HOST", raw)
                    assert hud_env.resolve() == (f"{origin}/mcp", hud_env.load_psk(raw))
                    assert hud_env.base_url() == origin
                    assert hud_env.adapter_endpoint()["url"] == f"{origin}/mcp"
        finally:
            key_file.unlink()
    # Disabled/pending/failed gRPC still permits MCP pairing and stdio use;
    # every gRPC client resolves through this helper instead of inventing :50051.
    for advertised in [None, "[fd7a:115c:a1e0::1]:50072"]:
        FakeHud.grpc_target = advertised
        result = pair(host, home)
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)["grpc"] == advertised
        assert json.loads(endpoint_file(home).read_text())["grpc_target"] == advertised
        if advertised is None:
            with pytest.raises(hud_env.HudEnvError, match="no available gRPC listener"):
                hud_env.grpc_target(host)
        else:
            assert hud_env.grpc_target(host) == advertised
        with AdapterProcess(home) as adapter:
            adapter.ready()
    # Retain the existing legacy schema1 reader contract.
    record = json.loads(endpoint_file(home).read_text())
    record["schema"] = 1
    del record["grpc_target"]
    endpoint_file(home).write_text(json.dumps(record))
    assert hud_env.grpc_target(host) == "127.0.0.1:50051"

    monkeypatch.delenv("HUD_HOST", raising=False)
    record = endpoint_file(home).read_bytes()
    assert hud_env.adapter_endpoint()["url"] == f"http://{host}/mcp"
    endpoint_file(home).unlink()
    assert hud_env.adapter_endpoint()["url"] == "http://127.0.0.1:9090/mcp"
    endpoint_file(home).write_bytes(record)
    endpoint_file(home).chmod(0o600)
    other = psk_file(home).with_name("other.psk")
    other.write_text("fake-other-key")
    other.chmod(0o600)
    with pytest.raises(hud_env.HudEnvError, match="multiple"):
        hud_env.adapter_endpoint()
    monkeypatch.setenv("HUD_HOST", host)
    assert hud_env.adapter_endpoint()["url"] == f"http://{host}/mcp"
    other.unlink()
    monkeypatch.delenv("HUD_HOST")
    for invalid in [{}, {"schema": 1, "mcp_url": "http://wrong:9090/mcp", "psk_sha256": "0" * 64},
                    {"schema": 1, "mcp_url": f"http://{host}/mcp?secret=none", "psk_sha256": "0" * 64},
                    {"schema": 1, "mcp_url": "http://${HUD_HOST}:9090/mcp", "psk_sha256": "0" * 64},
                    *[{"schema": 2, "mcp_url": f"http://{host}/mcp", "psk_sha256": "0" * 64,
                       "grpc_target": target} for target in [12, "h", "user:pass@h:50051", "h:99999"]]]:
        endpoint_file(home).write_text(json.dumps(invalid))
        with pytest.raises(hud_env.HudEnvError):
            hud_env.adapter_endpoint()
    endpoint_file(home).write_bytes(record)
    endpoint_file(home).chmod(0o640)
    with pytest.raises(hud_env.HudEnvError, match="unsafe"):
        hud_env.adapter_endpoint()
    endpoint_file(home).chmod(0o600)
    endpoint_file(home).unlink()
    endpoint_file(home).symlink_to(home / "absent-record")
    with pytest.raises(hud_env.HudEnvError):
        hud_env.adapter_endpoint()
    endpoint_file(home).unlink()
    endpoint_file(home).write_bytes(record)
    endpoint_file(home).chmod(0o600)


def test_admin_sends_the_paired_psk_as_bearer(hud, monkeypatch, capsys):
    host, home = hud
    pair(host, home)
    env = {**os.environ, "HOME": str(home), "HUD_HOST": host}
    result = subprocess.run(
        [sys.executable, str(SCRIPTS / "hud_admin.py"), "logs", "--tail", "5"],
        capture_output=True, text=True, env=env,
    )
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {"path": "/admin/logs?tail=5"}

    monkeypatch.syspath_prepend(str(SCRIPTS))
    monkeypatch.setenv("HOME", str(home))
    import hud_admin as admin

    if (sys.platform != "linux" or not hasattr(os, "fork") or
            not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal")):
        monkeypatch.setattr(admin, "doctor_probe", lambda *_: pytest.fail("unsupported host performed a probe"))
        assert admin.main(["--host", host, "doctor"]) == 1
        unsupported = json.loads(capsys.readouterr().out)
        assert unsupported["reason"] == "deadline_unavailable"
        assert unsupported["action"] == admin.DOCTOR_HOST_ACTION
        return  # The original logs/bearer control above still ran.

    sha, marker = "a" * 40, "synthetic-private-doctor-detail"
    outcome = {}
    trace_path = home / "doctor-trace"
    trace_path.write_text("")
    births, signals = [], []
    fork, send_signal = admin.os.fork, admin.signal.pidfd_send_signal

    def record(value):
        with trace_path.open("a") as handle:
            handle.write(value + "\n")

    def fork_owned():
        pid = fork()
        if pid:
            births.append(pid)  # Startup gate keeps this child live until admitted.
        return pid

    def signal_owned(pidfd, sig):
        signals.append(sig)
        return send_signal(pidfd, sig)

    def blocked_c_call():
        # libc sleep does not execute Python handlers; force the KILL fallback.
        admin.signal.pthread_sigmask(admin.signal.SIG_BLOCK, {admin.signal.SIGTERM})
        admin.ctypes.CDLL(None).sleep(2)

    monkeypatch.setattr(admin.os, "fork", fork_owned)
    monkeypatch.setattr(admin.signal, "pidfd_send_signal", signal_owned)

    def resolve(*args, **kwargs):
        record("dns")
        if outcome.get("block_dns"):
            blocked_c_call()
        if outcome.get("dns"):
            raise outcome["dns"]
        return [(admin.socket.AF_INET, admin.socket.SOCK_STREAM, 0, "", ("127.0.0.1", 9090))]

    class Connection:
        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def settimeout(self, seconds):
            assert seconds == admin.DOCTOR_LAYER_SECONDS

        def connect(self, address):
            record("tcp")
            assert address == ("127.0.0.1", 9090)
            if outcome.get("tcp"):
                raise outcome["tcp"]

    class Response:
        def __init__(self, payload):
            self.payload = payload

        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def read(self, limit):
            assert limit == 131073
            if outcome.get("block_read"):
                blocked_c_call()
            return json.dumps(self.payload).encode()

    class Opener:
        def open(self, request, timeout):
            assert request.get_method() == "GET" and request.data is None
            assert timeout == admin.DOCTOR_LAYER_SECONDS
            if request.full_url.endswith("/admin/status"):
                record("auth")
                assert request.full_url == f"http://{host}/admin/status"
                assert request.get_header("Authorization") == f"Bearer {FakeHud.last_psk}"
                if outcome.get("block_http"):
                    blocked_c_call()
                if outcome.get("auth"):
                    raise admin.urllib.error.HTTPError(request.full_url, outcome["auth"], marker, {}, None)
                return Response({"sha": outcome.get("sha", sha), "agents": marker,
                                 "tailnet_inbound": {"state": outcome.get("firewall", "allowed"), "error": marker},
                                 **outcome.get("metadata", {})})
            assert request.get_header("Authorization") is None
            record("release")
            if outcome.get("release"):
                raise admin.urllib.error.HTTPError(request.full_url, outcome["release"], marker, {}, None)
            if request.full_url.endswith("/releases/tags/dev"):
                return Response({"tag_name": "dev", "draft": False, "target_commitish": "main", "assets": marker})
            if request.full_url.endswith("/git/ref/tags/dev"):
                return Response({"object": {"type": "tag", "sha": "b" * 40}})
            assert request.full_url == admin.DEV_API + "/git/tags/" + "b" * 40
            return Response({"object": {"type": "commit", "sha": outcome.get("latest", sha)}})

    monkeypatch.setattr(admin.socket, "getaddrinfo", resolve)
    monkeypatch.setattr(admin.socket, "socket", lambda *_: Connection())
    monkeypatch.setattr(admin.urllib.request, "build_opener", lambda handler: Opener())
    cases = [
        ({}, 0, "dev_commit", None, ["dns", "tcp", "auth", "release", "release", "release"]),
        ({"dns": admin.socket.gaierror(marker)}, 1, "dns", "resolution_failed", ["dns"]),
        ({"tcp": ConnectionRefusedError(marker)}, 1, "tcp", "connection_refused", ["dns", "tcp"]),
        ({"tcp": TimeoutError(marker)}, 1, "tcp", "deadline_exceeded", ["dns", "tcp"]),
        *[({"auth": code}, 1, "auth", reason, ["dns", "tcp", "auth"])
          for code, reason in [(401, "unauthenticated"), (403, "not_admin"), (500, "http_error")]],
        *[({"firewall": state}, 1, "firewall", state, ["dns", "tcp", "auth"])
          for state in ["blocked", "unknown", "not_applicable"]],
        ({"sha": "unknown"}, 1, "build", "unknown", ["dns", "tcp", "auth"]),
        ({"release": 403}, 1, "release", "http_error", ["dns", "tcp", "auth", "release"]),
        ({"latest": "main"}, 1, "release", "unknown", ["dns", "tcp", "auth", "release", "release", "release"]),
        ({"latest": "c" * 40}, 1, "build", "sha_mismatch", ["dns", "tcp", "auth", "release", "release", "release"]),
    ]
    for configured, code, layer, reason, expected in cases:
        outcome.clear()
        outcome.update(configured)
        trace_path.write_text("")
        assert admin.main(["--host", host, "doctor"]) == code
        captured = capsys.readouterr()
        observed = json.loads(captured.out)
        assert observed["layer"] == layer and observed.get("reason") == reason
        assert trace_path.read_text().splitlines() == expected and not captured.err
        assert marker not in captured.out and FakeHud.last_psk not in captured.out
        if code:
            assert observed["action"]

    # Keep the existing owned-child probes, but pass real schema-shaped metadata
    # through their bounded whitelist. Legacy/malformed metadata never turns into
    # a healthy empty report, and later failures retain their original diagnosis.
    import copy
    metadata = {
        "holdings": {"state": "complete", "sources": {"scene": "available", "portals": "not_configured"},
                     "agents": [{"id": "first-agent", "surfaces": [
                         {"surface": "zone:subtitle", "kind": "zone", "phase": "scheduled",
                          "expires_in_ms": 2, "expiry_kind": "content", "count": 3}], "omitted_records": 0}],
                     "omitted_agents": 0, "omitted_records": 0, "omission_reasons": []},
        "warnings": {"state": "complete", "items": [
            {"source": "font", "code": "FONT_FAMILY_FALLBACK", "subject": "sans", "fallback": "IBM Plex Sans"}],
            "truncated": False, "omitted_events": 0, "omission_reasons": []},
    }
    for failure, code, layer, reason in [({}, 0, "dev_commit", None),
                                        ({"firewall": "blocked"}, 1, "firewall", "blocked"),
                                        ({"sha": "unknown"}, 1, "build", "unknown"),
                                        ({"release": 403}, 1, "release", "http_error")]:
        outcome.clear()
        outcome.update({"metadata": metadata, **failure})
        assert admin.main(["--host", host, "doctor"]) == code
        captured = capsys.readouterr()
        observed = json.loads(captured.out)
        assert observed["layer"] == layer and observed.get("reason") == reason
        diagnostic = observed["diagnostics"]
        assert diagnostic["state"] == "complete" and admin.valid_diagnostics(diagnostic)
        assert diagnostic["holdings"] == [{"agent": "first-agent", **metadata["holdings"]["agents"][0]["surfaces"][0]}]
        assert diagnostic["warnings"] == metadata["warnings"]["items"]
        assert len((json.dumps({"result": observed}) + "\n").encode()) <= 3072
        assert marker not in captured.out and FakeHud.last_psk not in captured.out

    malformed = []
    for source, field, value in [
        ("holdings", "state", "healthy"), ("holdings", "omitted_records", -1),
        ("holdings", "omitted_records", True), ("holdings", "omitted_records", 2**80),
        ("holdings", "omitted_records", 1), ("holdings", "omitted_records", None),
        ("warnings", "omitted_events", 1), ("warnings", "truncated", True),
    ]:
        bad = copy.deepcopy(metadata)
        bad[source][field] = value
        malformed.append(bad)
    for bad in [copy.deepcopy(metadata) for _ in range(4)]:
        malformed.append(bad)
    malformed[-4]["holdings"]["sources"]["scene"] = "busy"
    malformed[-3]["holdings"]["agents"][0]["surfaces"][0]["content"] = marker
    malformed[-2]["warnings"]["items"][0]["subject"] = marker + "/private-path"
    malformed[-1]["holdings"]["agents"][0]["surfaces"][0]["count"] = []
    for bad in malformed:
        outcome.clear()
        outcome.update(metadata=bad, firewall="blocked")
        assert admin.main(["--host", host, "doctor"]) == 1
        captured = capsys.readouterr()
        observed = json.loads(captured.out)
        assert observed["layer"] == "firewall" and observed["reason"] == "blocked"
        assert observed["diagnostics"] == admin.unavailable_diagnostics("malformed")
        assert marker not in captured.out and FakeHud.last_psk not in captured.out

    for state in ["pending", "unavailable"]:
        changed = copy.deepcopy(metadata)
        changed["warnings"].update(state=state, items=[])
        diagnostic = admin.project_diagnostics(changed)
        assert diagnostic["state"] == "partial" and state in diagnostic["reasons"]
        assert diagnostic["omitted_warning_events"] == (None if state == "unavailable" else 0)
        assert admin.valid_diagnostics(diagnostic)
    changed = copy.deepcopy(metadata)
    changed["holdings"].update(state="partial", sources={"scene": "busy", "portals": "available"},
                               agents=[], omitted_agents=None, omitted_records=None, omission_reasons=["busy"])
    assert admin.project_diagnostics(changed)["omitted_records"] is None
    assert admin.project_diagnostics({}) == admin.unavailable_diagnostics("unavailable")

    # Whole original addresses, escaped JSON bytes, aggregation counts and exact
    # bounded owner omissions survive both cardinality and final envelope trims.
    crowded = copy.deepcopy(metadata)
    crowded["holdings"]["agents"] = [
        {"id": chr(1) * 125 + f"{i:03}", "surfaces": [
            {"surface": "zone:" + chr(2) * 248 + f"{i:03}", "kind": "zone", "phase": "active",
             "expires_in_ms": None, "expiry_kind": "until_clear", "count": 2}], "omitted_records": 0}
        for i in range(12)]
    crowded["holdings"].update(state="partial", omitted_agents=1, omitted_records=5, omission_reasons=["cardinality"])
    crowded["warnings"].update(items=[
        {"source": "widget_bundle", "code": "WIDGET_BUNDLE_ROOT_UNREADABLE", "subject": f"root:{i}", "fallback": None}
        for i in range(6)], truncated=True, omitted_events=7, omission_reasons=["cardinality"])
    outcome.clear()
    outcome.update(metadata=crowded)
    assert admin.main(["--host", host, "doctor"]) == 0
    captured = capsys.readouterr()
    observed = json.loads(captured.out)
    diagnostic = observed["diagnostics"]
    assert admin.valid_diagnostics(diagnostic)
    assert len((json.dumps({"result": observed}) + "\n").encode()) <= 3072
    assert len(diagnostic["holdings"]) <= 8 and len(diagnostic["warnings"]) <= 4
    assert diagnostic["omitted_records"] + sum(row["count"] for row in diagnostic["holdings"]) == 29
    assert diagnostic["omitted_agents"] + len(diagnostic["holdings"]) == 13
    assert diagnostic["omitted_warning_events"] + len(diagnostic["warnings"]) == 13
    assert "bytes" in diagnostic["reasons"] and "cardinality" in diagnostic["reasons"]
    for row in diagnostic["holdings"]:
        assert row["surface"] in {agent["surfaces"][0]["surface"] for agent in crowded["holdings"]["agents"]}
    forged = {**diagnostic, "state": "complete"}
    assert not admin.valid_diagnostics(forged)
    # Bounded upstream counters may already be saturated. Trimming must not
    # produce an invalid integer that replaces the original primary failure.
    maximal = copy.deepcopy(crowded)
    maximal["holdings"].update(omitted_agents=2**64-1, omitted_records=2**64-1)
    maximal["warnings"]["omitted_events"] = 2**64-1
    outcome.clear()
    outcome.update(metadata=maximal, firewall="blocked")
    assert admin.main(["--host", host, "doctor"]) == 1
    observed = json.loads(capsys.readouterr().out)
    assert observed["layer"] == "firewall" and observed["reason"] == "blocked"
    assert admin.valid_diagnostics(observed["diagnostics"])
    assert all(observed["diagnostics"][key] == 2**64-1 for key in
               ("omitted_agents", "omitted_records", "omitted_warning_events"))

    # Invalid nested metadata must not bypass the parent whitelist by naming
    # one of the supervisor's own failure reasons before validation completes.
    with monkeypatch.context() as child_patch:
        child_patch.setattr(admin, "doctor_probe", lambda *_: {
            "ok": False, "layer": "endpoint", "reason": "cleanup_failed",
            "action": admin.DOCTOR_ACTIONS["endpoint"], "diagnostics": {"content": marker},
        })
        assert admin.main(["--host", host, "doctor"]) == 1
        captured = capsys.readouterr()
        rejected = json.loads(captured.out)
        assert rejected["reason"] == "unknown" and "diagnostics" not in rejected
        assert marker not in captured.out

    # Real C blocking cannot defer the parent's deadline. Prove no later layer,
    # TERM/KILL fallback and actual child reaping for DNS and HTTP open/read.
    monkeypatch.setattr(admin, "DOCTOR_LAYER_SECONDS", 0.03)
    for fault, layer, expected in [("block_dns", "dns", ["dns"]),
                                   ("block_http", "auth", ["dns", "tcp", "auth"]),
                                   ("block_read", "auth", ["dns", "tcp", "auth"])]:
        outcome.clear()
        outcome[fault] = True
        trace_path.write_text("")
        signals.clear()
        started = time.monotonic()
        assert admin.main(["--host", host, "doctor"]) == 1
        assert time.monotonic() - started < 1.5  # The child C sleep is two seconds.
        observed = json.loads(capsys.readouterr().out)
        assert observed["layer"] == layer and observed["reason"] == "deadline_exceeded"
        assert trace_path.read_text().splitlines() == expected
        assert signals == [signal.SIGTERM, signal.SIGKILL]
    for pid in births:
        with pytest.raises(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)  # Already reaped, never left as a zombie.

    # An unavailable kernel API must never admit the child to key/network work.
    trace_path.write_text("")
    signals.clear()
    open_pidfd = admin.os.pidfd_open

    def unavailable_pidfd(_):
        raise OSError("synthetic unavailable pidfd")

    monkeypatch.setattr(admin.os, "pidfd_open", unavailable_pidfd)
    assert admin.main(["--host", host, "doctor"]) == 1
    unavailable = json.loads(capsys.readouterr().out)
    assert unavailable["reason"] == "deadline_unavailable"
    assert not trace_path.read_text() and not signals
    with pytest.raises(ChildProcessError):
        os.waitpid(births[-1], os.WNOHANG)
    monkeypatch.setattr(admin.os, "pidfd_open", open_pidfd)

    trace_path.write_text("")
    before = len(births)
    monkeypatch.delattr(admin.os, "pidfd_open")
    assert admin.main(["--host", host, "doctor"]) == 1
    unsupported = json.loads(capsys.readouterr().out)
    assert unsupported["reason"] == "deadline_unavailable"
    assert unsupported["action"] == "Run doctor from a Linux/WSL agent host with owned-process deadline support; no network probe was performed."
    assert len(births) == before and not trace_path.read_text()


def test_pair_creates_a_private_dir_and_ignores_planted_files(hud):
    host, home = hud
    victim = home / "victim"
    victim.write_text("keep")
    config = psk_file(home).parent
    config.mkdir(parents=True, mode=0o755)
    config.chmod(0o755)
    (config / "127.0.0.1.psk.tmp").symlink_to(victim)  # the old fixed tmp name
    (config / "127.0.0.1.psk").symlink_to(victim)  # replaced, not followed
    assert pair(host, home).returncode == 0
    assert victim.read_text() == "keep"
    assert stat.S_IMODE(config.stat().st_mode) == 0o700
    assert not psk_file(home).is_symlink()
    assert stat.S_IMODE(psk_file(home).stat().st_mode) == 0o600


def test_load_psk_refuses_a_group_or_world_readable_file(hud, monkeypatch):
    host, home = hud
    monkeypatch.syspath_prepend(str(SCRIPTS))
    monkeypatch.setenv("HOME", str(home))
    import hud_env

    pair(host, home)
    psk_file(home).chmod(0o640)
    with pytest.raises(hud_env.HudEnvError, match="mode 640") as refused:
        hud_env.load_psk("127.0.0.1")
    assert psk_file(home).read_text().strip() not in str(refused.value)

    with AdapterProcess(home, host=host) as adapter:
        adapter.send("initialize", 1, {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fixture", "version": "1"}})
        assert adapter.receive()["error"]["code"] == -32603
    assert not FakeHud.requests
    assert psk_file(home).read_text().strip() not in adapter.stderr
    psk_file(home).chmod(0o600)
    FakeHud.modes[45] = "redirect"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        adapter.send("ping", 45)
        assert adapter.receive()["error"]["code"] == -32603
    assert sum(r["body"].get("id") == 45 for r in FakeHud.requests) == 1
    assert FakeHud.last_psk not in adapter.stderr
    FakeHud.modes[46] = "escaped_key"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        adapter.send("ping", 46)
        rejected = adapter.receive()
        assert rejected["id"] == 46 and rejected["error"]["code"] == -32603
        assert FakeHud.last_psk not in json.dumps(rejected) + adapter.buffer.decode()
    assert FakeHud.last_psk not in adapter.stderr


def test_adapter_is_ready_on_the_initialized_notice_without_a_hud_request(hud):
    """The notice is consumed locally: a HUD that fails or stalls it cannot
    wedge the session, and requests after it go straight to the HUD."""
    host, home = hud
    pair(host, home)
    # Any notice that did reach the fake HUD would hang (id None is its key).
    FakeHud.modes[None] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()  # tools after the notice answer at once
        adapter.send("tools/list", "listed")
        assert len(adapter.receive()["result"]["tools"]) == 5
        adapter.send("tools/call", "called", {"name": "hud_input", "arguments": {"wait_ms": 0}})
        assert adapter.receive()["id"] == "called"
        # A repeated notice changes nothing.
        adapter.send("notifications/initialized")
        adapter.send("ping", "after-repeat")
        assert adapter.receive() == {"jsonrpc": "2.0", "id": "after-repeat", "result": {}}
    assert not any(r["body"].get("method", "").startswith("notifications/") for r in FakeHud.requests)

    # Before initialize succeeded the notice is ignored, and requests are
    # refused without a worker or HUD request.
    previous = len(FakeHud.requests)
    with AdapterProcess(home) as adapter:
        adapter.send("notifications/initialized")
        adapter.send("tools/list", "early")
        assert adapter.receive() == {"jsonrpc": "2.0", "id": "early",
                                     "error": {"code": -32603, "message": NOT_READY}}
        assert not owned_children(adapter.process.pid)
        adapter.send("initialize", 1, {"protocolVersion": "2025-06-18", "capabilities": {},
                                       "clientInfo": {"name": "fixture", "version": "1"}})
        assert adapter.receive()["result"]["protocolVersion"] == "2025-06-18"
        adapter.send("tools/list", "before-notice")
        assert adapter.receive()["error"]["message"] == NOT_READY
    assert [r["body"]["method"] for r in FakeHud.requests[previous:]] == ["initialize"]

    # A fifth concurrent request still gets the capacity refusal.
    FakeHud.release = threading.Event()
    for n in range(4):
        FakeHud.modes[f"held-{n}"] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        for n in range(4):
            adapter.send("tools/list", f"held-{n}")
        for n in range(4):
            wait_request(f"held-{n}")
        adapter.send("tools/list", "fifth")
        assert adapter.receive() == {"jsonrpc": "2.0", "id": "fifth",
                                     "error": {"code": -32603, "message": "HUD request deadline or capacity exceeded"}}
        FakeHud.release.set()
        assert sorted(adapter.receive()["id"] for _ in range(4)) == [f"held-{n}" for n in range(4)]


def test_mcp_headers_needs_a_bare_host(hud, monkeypatch):
    # Controlled proc observations; no real PID reuse or permission changes.
    parent, gone, sibling = 9_000_001, 9_000_002, 9_000_003
    children_path = Path(f"/proc/{parent}/task/{parent}/children")
    gone_path, sibling_path = Path(f"/proc/{gone}/stat"), Path(f"/proc/{sibling}/stat")
    identity = {"start": "123", "state": "S", "parent": parent}
    read_text = Path.read_text
    outcomes = {}

    def proc_stat(pid, state="S", start="123"):
        fields = [state, str(parent), *["0"] * 17, start]
        return f"{pid} (fixture worker) " + " ".join(fields)

    def proc_read(path, *args, **kwargs):
        if path not in outcomes:
            return read_text(path, *args, **kwargs)
        outcome = outcomes[path]
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome

    with monkeypatch.context() as patch:
        patch.setattr(Path, "read_text", proc_read)
        for error, number in [(FileNotFoundError, 2), (ProcessLookupError, 3)]:
            outcomes.clear()
            outcomes[children_path] = error(number, "fixture process disappeared")
            assert owned_children(parent) == {}
            outcomes.update({children_path: f"{gone} {sibling}",
                             gone_path: error(number, "fixture child disappeared"),
                             sibling_path: proc_stat(sibling)})
            assert owned_children(parent) == {sibling: identity}
            assert not child_live(gone, identity)
        for denied in [children_path, gone_path, sibling_path]:
            outcomes.clear()
            outcomes.update({children_path: f"{gone} {sibling}",
                             gone_path: proc_stat(gone), sibling_path: proc_stat(sibling)})
            outcomes[denied] = PermissionError(13, "fixture permission denied")
            with pytest.raises(PermissionError):
                owned_children(parent) if denied != sibling_path else child_live(sibling, identity)
        outcomes[gone_path] = proc_stat(gone)
        assert child_live(gone, identity)
        outcomes[gone_path] = proc_stat(gone, start="456")
        assert not child_live(gone, identity)
        outcomes[gone_path] = proc_stat(gone, state="Z")
        assert not child_live(gone, identity)

    host, home = hud
    pair(host, home)
    env = {**os.environ, "HOME": str(home), "HUD_HOST": "127.0.0.1"}
    run = lambda e: subprocess.run(  # noqa: E731
        [sys.executable, str(SCRIPTS / "hud_env.py"), "mcp-headers"], capture_output=True, text=True, env=e
    )
    assert json.loads(run(env).stdout)["Authorization"].startswith("Bearer ")
    bad = run({**env, "HUD_HOST": "127.0.0.1:9090"})
    assert bad.returncode == 1 and "bare host" in bad.stderr and not bad.stdout

    # Execute the production admission/reaping clock, without waiting out 60s
    # or giving established hud_input the startup cap.
    import runpy
    from types import SimpleNamespace
    from unittest.mock import Mock

    module = runpy.run_path(str(SCRIPTS / "hud_mcp_stdio.py"), run_name="deadline_fixture")
    assert module["INITIALIZE_SECONDS"] == 3.0 and module["REQUEST_SECONDS"] == 60.0
    client = module["Adapter"].__new__(module["Adapter"])
    client.__dict__.update(endpoint={}, selector=Mock(), tasks=[], output=bytearray(),
                           output_registered=False, initialized=True,
                           protocol="2025-06-18", initializing=False, stop=False, fatal=False)
    process = SimpleNamespace(pid=99, stdin=Mock(), stdout=Mock(), poll=lambda: None)
    clock = [100.0]
    with monkeypatch.context() as patch:
        patch.setattr(module["time"], "monotonic", lambda: clock[0])
        patch.setattr(module["subprocess"], "Popen", lambda *a, **kw: process)
        patch.setattr(module["os"], "pidfd_open", lambda pid: 99)
        patch.setattr(module["os"], "set_blocking", lambda *a: None)
        send_signal = Mock()
        patch.setattr(module["signal"], "pidfd_send_signal", send_signal)
        request = {"jsonrpc": "2.0", "id": "clock-input", "method": "tools/call",
                   "params": {"name": "hud_input", "arguments": {"wait_ms": 30000}}}
        client.admit(request)
        task = client.tasks[0]
        assert task.deadline == 160.0
        clock[0] = 159.999
        assert client.reap() and not task.cancelled and not client.output
        send_signal.assert_not_called()
        clock[0] = 160.0
        assert client.reap() and task.cancelled and task.abort_phase == "term"
        expired = json.loads(client.output)
        assert expired["id"] == "clock-input" and "deadline exceeded" in expired["error"]["message"]
        send_signal.assert_called_once_with(99, signal.SIGTERM)

        client.tasks.clear()
        client.output.clear()
        client.protocol = None
        client.initialized = False
        clock[0] = 300.0
        client.input = bytearray(b'{"jsonrpc":"2.0","id":"clock-init","method":"initialize"}\n')
        original_loads = module["json"].loads

        def delayed_parse(*a, **kw):
            clock[0] = 301.5
            return original_loads(*a, **kw)

        patch.setattr(module["json"], "loads", delayed_parse)
        client.frames()
        assert client.tasks[0].deadline == 303.0  # Charged before parsing/worker startup.
        patch.setattr(module["json"], "loads", original_loads)
        clock[0] = 302.999
        assert client.reap() and not client.tasks[0].cancelled
        clock[0] = 303.0
        assert client.reap() and client.tasks[0].cancelled
        assert "phase unavailable" in json.loads(client.output)["error"]["message"]

    # Controlled worker failures execute the actual key/network/error path in
    # private subprocesses. Exception details must never become MCP output.
    endpoint = {"host": "127.0.0.1", "url": f"http://{host}/mcp", "explicit": True}
    for layer, injection in [
        ("DNS", "import socket; socket.getaddrinfo=lambda *a,**kw: (_ for _ in ()).throw(socket.gaierror(-3,'PRIVATE_FAILURE_SENTINEL'))"),
        ("TCP", "import http.client; http.client.HTTPConnection.connect=lambda s: (_ for _ in ()).throw(ConnectionRefusedError('PRIVATE_FAILURE_SENTINEL'))"),
        ("TCP", "import http.client; http.client.HTTPConnection.connect=lambda s: (_ for _ in ()).throw(TimeoutError('PRIVATE_FAILURE_SENTINEL'))"),
        ("response/read", "import http.client; http.client.HTTPConnection.getresponse=lambda s: (_ for _ in ()).throw(TimeoutError('PRIVATE_FAILURE_SENTINEL'))"),
    ]:
        worker_code = ("import runpy,sys,os; m=runpy.run_path(sys.argv[1],run_name='worker_fixture'); "
                       + injection + "; raise SystemExit(m['worker'](os.getppid(),float(sys.argv[2])))")
        job = {"request": {"jsonrpc": "2.0", "id": "controlled-" + layer, "method": "initialize"},
               "endpoint": endpoint, "protocol": None}
        result = subprocess.run([sys.executable, "-c", worker_code, str(SCRIPTS / "hud_mcp_stdio.py"),
                                 str(time.monotonic() + 3.0)], input=json.dumps(job), capture_output=True,
                                text=True, env={**os.environ, "HOME": str(home)}, timeout=5)
        assert result.returncode == 0 and not result.stderr
        refused = json.loads(result.stdout)["response"]
        assert refused["id"] == job["request"]["id"] and refused["error"]["code"] == -32603
        assert refused["error"]["message"] == f"HUD initialize {layer} " + ("lookup failed" if layer == "DNS" else "failed")
        assert "PRIVATE_FAILURE_SENTINEL" not in result.stdout and FakeHud.last_psk not in result.stdout

    original_reply = FakeHud.reply
    for status in (401, 403):
        request_id = f"initialize-auth-{status}"

        def auth_reply(self, code, payload, headers=None):
            if isinstance(payload, dict) and payload.get("id") == request_id:
                return original_reply(self, status, {"code": "UNAUTHORIZED"})
            return original_reply(self, code, payload, headers)

        with monkeypatch.context() as patch:
            patch.setattr(FakeHud, "reply", auth_reply)
            with AdapterProcess(home) as adapter:
                adapter.send("initialize", request_id)
                refused = adapter.receive()
                assert refused["id"] == request_id and refused["error"]["message"] == "HUD initialize auth failed"
                adapter.send("ping", "after-" + request_id)
                assert adapter.receive()["error"]["message"] == NOT_READY
        assert sum(r["body"].get("id") == request_id for r in FakeHud.requests) == 1
        assert FakeHud.last_psk not in adapter.stderr

    try:
        psk_file(home).write_text("invalid\nselected-key\n")
        previous = len(FakeHud.requests)
        with AdapterProcess(home) as adapter:
            adapter.send("initialize", "local-auth-refusal")
            refused = adapter.receive()
            assert refused["id"] == "local-auth-refusal" and refused["error"]["message"] == "HUD initialize auth failed"
        assert len(FakeHud.requests) == previous and FakeHud.last_psk not in adapter.stderr
    finally:
        psk_file(home).write_text(FakeHud.last_psk)

    # A reserved unroutable destination uses a synthetic key in a separate fake
    # HOME. Refusal may be immediate; a stalled resolver/connect is still bounded.
    unreachable_home = home / "unreachable"
    key = unreachable_home / ".config/tze-hud/192.0.2.1.psk"
    key.parent.mkdir(parents=True, mode=0o700)
    key.write_text(FakeHud.last_psk)
    key.chmod(0o600)
    with AdapterProcess(unreachable_home, host="192.0.2.1:9") as adapter:
        started = time.monotonic()
        adapter.send("initialize", "unroutable-init")
        refused = adapter.receive(timeout=6)
        elapsed = time.monotonic() - started
        assert refused["id"] == "unroutable-init" and refused["error"]["code"] == -32603
        message = refused["error"]["message"]
        assert message == "HUD initialize TCP failed" or "phase unavailable" in message
        assert elapsed < 5.5  # 3s from accepted frame + interpreter/OS cleanup allowance.
        assert adapter.stop() == 0 and FakeHud.last_psk not in adapter.stderr

    FakeHud.release = threading.Event()
    FakeHud.modes["initialize-held"] = "hold"
    with AdapterProcess(home) as adapter:
        started = time.monotonic()
        adapter.send("initialize", "initialize-held")
        wait_request("initialize-held")
        held = owned_children(adapter.process.pid)
        adapter.send("notifications/cancelled", params={"requestId": "initialize-held"})
        refused = adapter.receive(timeout=6)
        elapsed = time.monotonic() - started
        assert refused["id"] == "initialize-held" and refused["error"]["code"] == -32603
        message = refused["error"]["message"]
        assert message == "HUD initialize response/read failed" or ("deadline" in message and "phase unavailable" in message)
        assert 2.7 <= elapsed < 5.5
        adapter.send("ping", "after-held-init")
        assert adapter.receive()["error"]["message"] == NOT_READY
        assert adapter.stop() == 0
        assert held and all(not Path(f"/proc/{pid}").exists() for pid in held)
    assert sum(r["body"].get("id") == "initialize-held" for r in FakeHud.requests) == 1
    assert FakeHud.last_psk not in adapter.stderr
    FakeHud.release.set()
    FakeHud.release = threading.Event()

    with AdapterProcess(home) as adapter:
        initialize = adapter.ready()
        assert initialize["result"]["protocolVersion"] == "2025-06-18"
        assert initialize["result"]["instructions"] == "upstream sentinel"
        assert FakeHud.requests[-1]["protocol"] == "2025-06-18"
        adapter.send("tools/call", "unicode", {"name": "hud_input", "arguments": {"wait_ms": 0}})
        returned = adapter.receive()
        assert returned["id"] == "unicode" and returned["result"]["isError"] is True
        assert json.loads(returned["result"]["content"][0]["text"]) == {"code": "FIXTURE", "hint": "雪"}
        FakeHud.modes[10] = "hold"
        adapter.send("tools/call", 10, {"name": "hud_input", "arguments": {"wait_ms": 30000}})
        wait_request(10)
        adapter.sample()
        held = dict(adapter.children)
        adapter.send("ping", 11)
        assert adapter.receive()["id"] == 11  # Completes BEFORE releasing real held HTTP.
        adapter.send("notifications/cancelled", params={"requestId": 11})
        adapter.send("notifications/cancelled", params={"requestId": "unknown"})
        adapter.send("notifications/cancelled", params={"requestId": 10})
        FakeHud.release.set()
        adapter.send("ping", 12)
        assert adapter.receive()["id"] == 12  # No cancelled10 or notification stdout.
        assert held and all(not child_live(pid, identity) for pid, identity in held.items())
    FakeHud.release = threading.Event()
    for request_id in [20, 21, 22, 23]:
        FakeHud.modes[request_id] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        for request_id in [20, 21, 22, 23]:
            adapter.send("tools/call", request_id, {"name": "hud_input", "arguments": {"wait_ms": 30000}})
            wait_request(request_id)
        adapter.sample()
        four = owned_children(adapter.process.pid)
        assert len(four) == 4
        adapter.send("ping", 24)
        assert adapter.receive()["id"] == 24 and not any(r["body"].get("id") == 24 for r in FakeHud.requests)
        assert adapter.stop() == 0  # EOF aborts all four with ONE global cleanup budget.
        assert adapter.elapsed_cleanup < 2.8
        assert all(not Path(f"/proc/{pid}").exists() for pid in four)
    FakeHud.release.set()
    for index, sig in enumerate([signal.SIGTERM, signal.SIGINT, signal.SIGKILL]):
        FakeHud.release = threading.Event()
        request_id = 50 + index
        FakeHud.modes[request_id] = "hold"
        with AdapterProcess(home) as adapter:
            adapter.ready()
            adapter.send("tools/call", request_id, {"name": "hud_input", "arguments": {"wait_ms": 30000}})
            wait_request(request_id)
            adapter.sample()
            children = owned_children(adapter.process.pid)
            assert children
            code = adapter.stop(sig)
            assert code == (-signal.SIGKILL if sig == signal.SIGKILL else 0)
            assert adapter.elapsed_cleanup < 2.8
            assert all(not child_live(pid, identity) for pid, identity in children.items())
            if sig != signal.SIGKILL:
                assert all(not Path(f"/proc/{pid}").exists() for pid in children)
            # Abrupt parent cannot reap; ZOMBIE/OS-adoption is not a LIVE worker.
        FakeHud.release.set()
    FakeHud.release = threading.Event()
    FakeHud.modes[60] = "trickle"
    # The production 60s admission/expiry is exercised by the clock above.
    # Let normal readiness finish before shortening the private request budget.
    with AdapterProcess(home, short=0.3) as adapter:
        adapter.ready()
        adapter.enable_short_request_deadline()
        adapter.send("ping", 60)
        wait_request(60)
        adapter.sample()
        result = adapter.receive(timeout=3)
        assert result["id"] == 60 and result["error"]["code"] == -32603
    assert sum(r["body"].get("id") == 60 for r in FakeHud.requests) == 1
    FakeHud.release.set()
    FakeHud.modes[70] = "large"
    with AdapterProcess(home, stall=0.2) as adapter:
        adapter.ready()
        adapter.send("ping", 70)
        wait_request(70)
        assert adapter.process.wait(timeout=3) == 1  # Undrained stdout cannot block forever.
    assert FakeHud.last_psk not in adapter.stderr
    FakeHud.release = threading.Event()
    FakeHud.modes[71] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        adapter.send("ping", 71)
        wait_request(71)
        adapter.sample()
        adapter.process.stdout.close()
        FakeHud.release.set()
        assert adapter.process.wait(timeout=3) == 1
    assert FakeHud.last_psk not in adapter.stderr

    # Raw wire input and a decimal-based receiver are independent of the adapter's
    # float parser. On uncorrected production the first response contains Infinity,
    # which must be a causal AssertionError rather than an uncaught parse exception.
    with AdapterProcess(home) as adapter:
        previous = len(FakeHud.requests)
        adapter.send("", raw=b'{"jsonrpc":"2.0","id":1e309,"method":"ping"}')
        refused = adapter.receive(strict=True)
        assert refused["id"] is None and refused["error"]["code"] == -32603
        assert len(FakeHud.requests) == previous
        for invalid_id in [b"-1e309", b"1.5", b"1.0", b"true", b"null"]:
            adapter.send("", raw=b'{"jsonrpc":"2.0","id":' + invalid_id + b',"method":"ping"}')
            refused = adapter.receive(strict=True)
            assert refused["id"] is None and refused["error"]["code"] == -32603
            assert len(FakeHud.requests) == previous
        adapter.ready()
        for request_id, raw in [
            ("overflow-params", b'{"jsonrpc":"2.0","id":"overflow-params","method":"ping","params":{"nested":[{"value":1e309}]}}'),
            (100, b'{"jsonrpc":"2.0","id":100,"method":"ping","params":{"value":-1e309}}'),
        ]:
            previous = len(FakeHud.requests)
            adapter.send("", raw=raw)
            refused = adapter.receive(strict=True)
            assert refused["id"] == request_id and refused["error"]["code"] == -32603
            assert len(FakeHud.requests) == previous
            adapter.send("ping", "recovered-" + str(request_id))
            assert adapter.receive(strict=True)["result"] == {}
        for token in [b"NaN", b"Infinity", b"-Infinity"]:
            previous = len(FakeHud.requests)
            adapter.send("", raw=b'{"jsonrpc":"2.0","id":"invalid-token","method":"ping","params":{"value":' + token + b'}}')
            refused = adapter.receive(strict=True)
            assert refused["id"] is None and refused["error"]["code"] == -32603
            assert len(FakeHud.requests) == previous
        finite = {"nested": [0.125, -0.25, 1e-10, True, "Infinity is ordinary text"]}
        adapter.send("ping", "finite-parameters", finite)
        assert adapter.receive(strict=True) == {"jsonrpc": "2.0", "id": "finite-parameters", "result": {}}
        assert FakeHud.requests[-1]["body"]["params"] == finite and FakeHud.requests[-1]["authorized"]

    FakeHud.release = threading.Event()
    with AdapterProcess(home) as adapter:
        adapter.ready()
        FakeHud.modes[1] = "hold"  # Reuse a completed integer ID; bool/float must not alias it.
        with FakeHud.condition:
            FakeHud.entered.pop(1, None)
        previous = len(FakeHud.requests)
        adapter.send("ping", 1)
        wait_request(1)
        held = owned_children(adapter.process.pid)
        assert len(held) == 1
        for invalid_cancel in [1.0, 1.5, True, None]:
            adapter.send("notifications/cancelled", params={"requestId": invalid_cancel})
        adapter.send("ping", "control-after-invalid-cancel")
        assert adapter.receive(strict=True)["id"] == "control-after-invalid-cancel"
        assert all(child_live(pid, identity) for pid, identity in held.items())
        before_overflow_cancel = len(FakeHud.requests)
        adapter.send("", raw=b'{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1e309}}')
        refused = adapter.receive(strict=True)
        assert refused["id"] is None and refused["error"]["code"] == -32603
        assert len(FakeHud.requests) == before_overflow_cancel
        adapter.send("ping", "control-after-overflow-cancel")
        assert adapter.receive(strict=True)["result"] == {}
        assert all(child_live(pid, identity) for pid, identity in held.items())
        FakeHud.release.set()
        assert adapter.receive(strict=True) == {"jsonrpc": "2.0", "id": 1, "result": {}}
        assert sum(r["body"].get("id") == 1 for r in FakeHud.requests[previous:]) == 1
    FakeHud.modes.pop(1)

    with AdapterProcess(home) as adapter:
        adapter.ready()
        for request_id, mode in enumerate([
            "raw-overflow", "raw-nan", "raw-infinity", "raw-negative-infinity", "raw-float-id"
        ], start=1010):
            FakeHud.modes[request_id] = mode
            previous = len(FakeHud.requests)
            adapter.send("ping", request_id)
            refused = adapter.receive(strict=True)
            assert refused["id"] == request_id and refused["error"]["code"] == -32603
            assert len(FakeHud.requests) == previous + 1 and FakeHud.requests[-1]["authorized"]
            assert FakeHud.requests[-1]["body"]["id"] == request_id
            adapter.send("ping", "recovered-upstream-" + str(request_id))
            assert adapter.receive(strict=True)["result"] == {}
        # Equality alone aliases both 1014.0 with 1014 above and true with 1 here.
        FakeHud.modes[1] = "raw-bool-id"
        previous = len(FakeHud.requests)
        adapter.send("ping", 1)
        refused = adapter.receive(strict=True)
        assert refused["id"] == 1 and refused["error"]["code"] == -32603
        assert len(FakeHud.requests) == previous + 1 and FakeHud.requests[-1]["authorized"]
        FakeHud.modes.pop(1)
        FakeHud.modes[1020] = "raw-finite"
        adapter.send("ping", 1020)
        assert adapter.receive(strict=True) == {"jsonrpc": "2.0", "id": 1020, "result": {"value": Decimal("0.125")}}
        adapter.send("ping", "final-valid-ping")
        assert adapter.receive(strict=True)["result"] == {}
    assert FakeHud.last_psk not in adapter.stderr


PROJECT_LABEL = SCRIPTS.parents[3].name


@pytest.mark.parametrize("stop", [None, signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
def test_adapter_is_one_labeled_session_released_on_a_clean_stop(hud, stop):
    """Initialize sends the folder label and keeps the session id for later
    calls; every clean stop DELETEs the session within 1 s, even with a call
    in flight."""
    host, home = hud
    pair(host, home)
    FakeHud.modes["in-flight"] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        init = next(r for r in FakeHud.requests if r["body"]["method"] == "initialize")
        assert init["body"]["params"]["_meta"] == {"tze-hud/label": PROJECT_LABEL}
        assert init["body"]["params"]["clientInfo"] == {"name": "fixture", "version": "1"}
        assert init["session"] is None
        assert FakeHud.requests[-1]["session"] == "session-1"  # the readiness ping
        adapter.send("tools/call", "in-flight", {"name": "hud_input", "arguments": {"wait_ms": 30000}})
        wait_request("in-flight")
        started = time.monotonic()
        adapter.stop(stop)
        assert [d[0] for d in FakeHud.deletes] == ["session-1"]
        assert FakeHud.deletes[0][1] - started < 1.0
        assert "session-1" not in FakeHud.sessions


def test_adapter_reinitializes_once_after_404_and_retries_each_call(hud):
    """Four calls refused as an unknown session share one internal
    initialize (never forwarded) and are each retried once on the new id."""
    host, home = hud
    pair(host, home)
    with AdapterProcess(home) as adapter:
        adapter.ready()
        FakeHud.sessions.clear()  # the HUD restarted
        for n in range(4):
            adapter.send("ping", f"lost-{n}")
        answers = sorted((adapter.receive() for _ in range(4)), key=lambda r: r["id"])
        assert answers == [{"jsonrpc": "2.0", "id": f"lost-{n}", "result": {}} for n in range(4)]
        inits = [r for r in FakeHud.requests if r["body"]["method"] == "initialize"]
        assert len(inits) == 2, "one re-initialize for four refusals"
        assert inits[1]["body"]["params"]["clientInfo"] == {"name": "fixture", "version": "1"}
        assert inits[1]["body"]["params"]["_meta"] == {"tze-hud/label": PROJECT_LABEL}
        for n in range(4):
            sent = [r["session"] for r in FakeHud.requests if r["body"].get("id") == f"lost-{n}"]
            assert sent[-1] == "session-2" and len(sent) <= 2, sent
        adapter.send("ping", "after")
        assert adapter.receive() == {"jsonrpc": "2.0", "id": "after", "result": {}}
        assert not adapter.buffer, "no second initialize result reached the client"
        record = home / ".config" / "tze-hud" / "sessions" / f"{os.getpid()}.json"
        assert json.loads(record.read_text())["session"] == "session-2", "helpers follow the new id"

        # A retried call refused again fails instead of looping.
        FakeHud.modes["again"] = "unknown-session"
        adapter.send("ping", "again")
        response = adapter.receive()
        assert response["id"] == "again" and response["error"]["message"].startswith("HUD session ended")
        assert sum(r["body"].get("id") == "again" for r in FakeHud.requests) == 2
        assert [r["body"]["method"] for r in FakeHud.requests].count("initialize") == 3


def test_a_stopping_adapter_never_reinitializes(hud):
    host, home = hud
    pair(host, home)
    FakeHud.modes["held"] = "hold"
    with AdapterProcess(home) as adapter:
        adapter.ready()
        adapter.send("tools/call", "held", {"name": "hud_input", "arguments": {"wait_ms": 30000}})
        wait_request("held")
        FakeHud.sessions.clear()
        FakeHud.release.set()  # the held call now answers 404 while stopping
        adapter.stop()
    assert [r["body"]["method"] for r in FakeHud.requests].count("initialize") == 1


def test_session_label_is_the_project_folder_else_the_cwd(tmp_path, monkeypatch):
    import runpy
    module = runpy.run_path(str(SCRIPTS / "hud_mcp_stdio.py"), run_name="label_fixture")
    monkeypatch.setenv("CLAUDE_PROJECT_DIR", "/work/tze-hud/")
    assert module["session_label"]() == "tze-hud"
    monkeypatch.delenv("CLAUDE_PROJECT_DIR")
    monkeypatch.chdir(tmp_path)
    assert module["session_label"]() == tmp_path.name
    sent = module["with_label"]({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                 "params": {"_meta": {"other": 1}}})
    assert sent["params"]["_meta"] == {"other": 1, "tze-hud/label": tmp_path.name}


PORTAL_CLIENT = SCRIPTS.parents[1] / "hud-projection" / "scripts" / "portal_client.py"


def test_a_background_waiter_joins_the_adapter_session(hud):
    """The adapter keeps its session id in a private record for processes
    its client starts; portal_client.py's waiter sends it and so sees the
    replies to a portal the model attached through MCP. A clean stop removes
    the record."""
    host, home = hud
    pair(host, home)
    record = home / ".config" / "tze-hud" / "sessions" / f"{os.getpid()}.json"
    item = {"id": "in-1", "s": "portal:main", "text": "ship it"}
    env = {**os.environ, "HOME": str(home)}
    env.pop("HUD_HOST", None)
    with AdapterProcess(home) as adapter:
        adapter.ready()
        assert record.stat().st_mode & 0o777 == 0o600
        assert record.parent.stat().st_mode & 0o077 == 0
        saved = json.loads(record.read_text())
        assert saved["session"] == "session-1" and saved["adapter"] == adapter.process.pid
        assert FakeHud.last_psk not in record.read_text()
        FakeHud.inputs["session-1"] = [item]
        waiter = subprocess.run([sys.executable, str(PORTAL_CLIENT), "poll", "--wait-ms", "0", "--ack"],
                                capture_output=True, text=True, env=env, timeout=10)
        assert waiter.returncode == 0, waiter.stderr
        assert json.loads(waiter.stdout.splitlines()[0]) == item
        polls = [r for r in FakeHud.requests if r["body"].get("params", {}).get("name") == "hud_input"]
        assert polls and {r["session"] for r in polls} == {"session-1"}
        adapter.stop(None)
    assert not record.exists()


def test_a_crashed_adapters_record_is_not_joined(hud):
    """A record whose adapter is gone (or whose pid was reused) is ignored:
    the client stays the agent's unscoped caller."""
    host, home = hud
    pair(host, home)
    gone = subprocess.run([sys.executable, "-c", "import os; print(os.getpid())"],
                          capture_output=True, text=True, check=True)
    record = home / ".config" / "tze-hud" / "sessions" / f"{os.getpid()}.json"
    record.parent.mkdir(mode=0o700, parents=True)
    record.write_text(json.dumps({"schema": 1, "host": "127.0.0.1", "url": f"http://{host}/mcp",
                                  "session": "session-9", "adapter": int(gone.stdout)}))
    record.chmod(0o600)
    env = {**os.environ, "HOME": str(home), "HUD_HOST": host}
    subprocess.run([sys.executable, str(PORTAL_CLIENT), "poll", "--wait-ms", "0"],
                   capture_output=True, text=True, env=env, timeout=10)
    polls = [r for r in FakeHud.requests if r["body"].get("params", {}).get("name") == "hud_input"]
    assert polls and all(r["session"] is None for r in polls)


def test_a_failed_reinitialize_never_leaves_the_adapter_unscoped(hud):
    """After a 404 whose re-initialize fails (the HUD is still restarting),
    a later call never reaches the HUD without a session id: it waits on a
    fresh re-initialize and goes out on the new session."""
    host, home = hud
    pair(host, home)
    with AdapterProcess(home) as adapter:
        adapter.ready()
        FakeHud.sessions.clear()  # the HUD restarted
        FakeHud.modes["tze-hud-reinitialize"] = "hold"  # ...and is slow to answer
        adapter.send("ping", "lost")
        response = adapter.receive(timeout=6)
        assert response["id"] == "lost" and response["error"]["message"].startswith("HUD session ended")
        del FakeHud.modes["tze-hud-reinitialize"]
        FakeHud.release.set()
        adapter.send("ping", "later")
        assert adapter.receive(timeout=6) == {"jsonrpc": "2.0", "id": "later", "result": {}}
        later = [r["session"] for r in FakeHud.requests if r["body"].get("id") == "later"]
        assert later and None not in later, later
        assert all(r["session"] is not None for r in FakeHud.requests
                   if r["body"].get("method") not in ("initialize",)), "no call went out unscoped"


def reinit_adapter(monkeypatch):
    """An in-process adapter with a lost session, fake workers and a fake
    clock: (module, client, exits, clock). Set exits[pid] to exit a worker."""
    import runpy
    from types import SimpleNamespace
    from unittest.mock import Mock

    module = runpy.run_path(str(SCRIPTS / "hud_mcp_stdio.py"), run_name="reinit_fixture")
    client = module["Adapter"].__new__(module["Adapter"])
    client.__dict__.update(endpoint={}, selector=Mock(), tasks=[], output=bytearray(), retry=[],
                           output_registered=False, initialized=True, protocol="2025-06-18",
                           initializing=False, stop=False, fatal=False, owner=None,
                           had_session=True, session=None, presence=module["SessionPresence"](False),
                           init_request={"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})
    client.cards = module["CardLane"](client)
    exits = {}  # pid -> exit code once the worker has exited
    workers = []

    def popen(*a, **kw):
        pid = 100 + len(workers)
        workers.append(SimpleNamespace(pid=pid, stdin=Mock(), stdout=Mock(), returncode=None,
                                       poll=lambda: exits.get(pid)))
        return workers[-1]

    clock = [100.0]
    monkeypatch.setattr(module["time"], "monotonic", lambda: clock[0])
    monkeypatch.setattr(module["subprocess"], "Popen", popen)
    monkeypatch.setattr(module["os"], "pidfd_open", lambda pid: pid)
    monkeypatch.setattr(module["os"], "close", lambda fd: None)
    monkeypatch.setattr(module["os"], "set_blocking", lambda *a: None)
    monkeypatch.setattr(module["signal"], "pidfd_send_signal", Mock())
    return module, client, exits, clock


def test_a_new_reinitialize_starts_while_the_abandoned_one_is_still_exiting(monkeypatch):
    """A timed-out re-initialize is abandoned but its worker may not have
    exited yet. The next call must start a fresh re-initialize anyway: the
    internal id never collides, never reaches the client, and the old
    worker's late exit does not fail the call waiting on the new one."""
    module, client, exits, clock = reinit_adapter(monkeypatch)
    reinit = module["REINIT_ID"]

    client.admit({"jsonrpc": "2.0", "id": "lost", "method": "ping"})
    abandoned = client.tasks[0]
    assert abandoned.internal and abandoned.request["id"] == reinit
    clock[0] = 103.0  # Past the re-initialize deadline; the worker has not exited.
    assert client.reap() and abandoned.cancelled and abandoned in client.tasks
    assert json.loads(client.output)["id"] == "lost"
    client.output.clear()

    client.admit({"jsonrpc": "2.0", "id": "later", "method": "ping"})
    fresh = client.tasks[-1]
    assert fresh is not abandoned and fresh.internal and not fresh.cancelled
    assert client.reinitializing and not client.output, bytes(client.output)

    exits[abandoned.process.pid] = -signal.SIGTERM  # The abandoned worker exits late.
    assert client.reap() and abandoned not in client.tasks
    assert client.reinitializing and [r["id"] for r, *_ in client.retry] == ["later"]
    assert not client.output, bytes(client.output)

    fresh.output = bytearray(json.dumps({"response": {"jsonrpc": "2.0", "id": reinit, "result": {}},
                                         "session": "session-2"}).encode())
    exits[fresh.process.pid] = 0
    assert client.reap() and not client.reinitializing and client.session == "session-2"
    later = client.tasks[-1]
    assert later.request["id"] == "later" and later.session == "session-2" and later.retried
    assert not client.output, bytes(client.output)


def test_a_reinitialize_aborted_before_its_deadline_fails_the_waiting_call(monkeypatch):
    """A broken job pipe or an oversized result aborts the re-initialize
    early. The call waiting on it fails at once instead of queueing behind
    a re-initialize that will never report."""
    _, client, exits, clock = reinit_adapter(monkeypatch)
    client.admit({"jsonrpc": "2.0", "id": "waiting", "method": "ping"})
    reinit = client.tasks[0]
    assert reinit.internal and client.reinitializing

    client.abort([reinit])  # What drive() does on BrokenPipeError or MAX_RESPONSE.
    assert not client.reinitializing and not client.retry
    assert json.loads(client.output)["id"] == "waiting"
    client.output.clear()

    exits[reinit.process.pid] = -signal.SIGTERM
    clock[0] = 100.5
    assert client.reap() and reinit not in client.tasks and not client.output


def test_a_reinitialize_refused_for_capacity_fails_the_waiting_call(monkeypatch):
    """With every worker slot taken, one by an abandoned re-initialize that is
    still exiting, the fresh re-initialize cannot start. The abandoned one
    does not count as running, so the waiting call fails now rather than
    waiting on it; the refused internal id never reaches the client."""
    from types import SimpleNamespace

    module, client, exits, clock = reinit_adapter(monkeypatch)
    client.admit({"jsonrpc": "2.0", "id": "lost", "method": "ping"})
    abandoned = client.tasks[0]
    clock[0] = 103.0
    assert client.reap() and abandoned.cancelled and abandoned in client.tasks
    client.output.clear()
    busy = [SimpleNamespace(request={"jsonrpc": "2.0", "id": f"busy-{n}", "method": "ping"},
                            internal=False, cancelled=False, presence=None, card=None)
            for n in range(module["MAX_WORKERS"] - 1)]
    client.tasks.extend(busy)

    client.admit({"jsonrpc": "2.0", "id": "waiting", "method": "ping"})
    assert client.tasks == [abandoned, *busy], "no fresh worker fits"
    assert not client.reinitializing and not client.retry
    replies = [json.loads(line) for line in bytes(client.output).splitlines()]
    assert [r["id"] for r in replies] == ["waiting"], replies


def test_a_loose_sessions_directory_is_made_private(hud):
    """A pre-existing group/world-readable sessions directory is tightened to
    0700 before the record is written, so helpers can read it."""
    host, home = hud
    pair(host, home)
    directory = home / ".config" / "tze-hud" / "sessions"
    directory.mkdir(mode=0o755)
    directory.chmod(0o755)
    with AdapterProcess(home) as adapter:
        adapter.ready()
        assert directory.stat().st_mode & 0o777 == 0o700
        assert (directory / f"{os.getpid()}.json").exists()
