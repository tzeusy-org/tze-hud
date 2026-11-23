"""Background ask contract tests against a loopback MCP server."""

import http.server
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
from types import SimpleNamespace
import urllib.error

import pytest


SCRIPT = Path(__file__).resolve().parents[2] / "hud-projection" / "scripts" / "portal_client.py"
SPEC = importlib.util.spec_from_file_location("portal_client", SCRIPT)
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)

ASK = ["ask", "--title", "Ship the patch?", "--action", "Ship=ship", "--action", "Later=later"]
SCOPE = "ask-scope"
SURFACE = "zone:notification-area"
PAIR = "synthetic-pair"


class FakeMcp:
    def __init__(self):
        self.calls = []
        self.responses = {}
        self.status = 200
        self.scope_header = SCOPE
        self.published = threading.Event()
        self.release_publish = threading.Event()
        self.release_publish.set()
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def record(self, name, arguments):
                scope = self.headers.get("Mcp-Session-Id")
                fake.calls.append((name, scope, arguments))
                return (self.headers.get("Authorization") == f"Bearer {PAIR}"
                        and scope == (None if name == "initialize" else SCOPE))

            def reply(self, status, value=None):
                body = b"" if value is None else json.dumps(value).encode()
                self.send_response(status)
                if status == 200 and value.get("result", {}).get("protocolVersion") and fake.scope_header:
                    self.send_header("Mcp-Session-Id", fake.scope_header)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                try:
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass  # The signal test terminates the in-flight publish request.

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                arguments = request["params"]
                name = request["method"]
                if name == "tools/call":
                    name = arguments["name"]
                    arguments = arguments["arguments"]
                if not self.record(name, arguments):
                    self.reply(401)
                    return
                if name == "initialize":
                    self.reply(fake.status, {
                        "jsonrpc": "2.0", "id": request["id"],
                        "result": {"protocolVersion": "2025-06-18"},
                    })
                    return
                if name == "hud_publish" and "wait_ms" in arguments:
                    fake.published.set()
                    fake.release_publish.wait(10)
                handler = fake.responses.get(name)
                value = handler(arguments) if handler else {"ok": True}
                result = {"content": [{"type": "text", "text": json.dumps(value)}]}
                if "code" in value:
                    result["isError"] = True
                self.reply(200, {"jsonrpc": "2.0", "id": request["id"], "result": result})

            def do_DELETE(self):
                self.reply(202 if self.record("DELETE", {}) else 401)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01})
        self.thread.start()

    def close(self):
        self.release_publish.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def args(self, name):
        return [arguments for tool, _, arguments in self.calls if tool == name]

    def card(self):
        return self.args("hud_publish")[0]


@pytest.fixture
def mcp(monkeypatch):
    fake = FakeMcp()
    host = f"127.0.0.1:{fake.server.server_port}"
    monkeypatch.setattr(client, "session", lambda: {
        "host": "127.0.0.1", "session": "enclosing-scope", "url": f"http://{host}/mcp",
    })
    monkeypatch.setattr(client.hud_env, "adapter_key", lambda _selected: PAIR)
    monkeypatch.setenv("CLAUDE_PROJECT_DIR", "/example/project-folder")
    try:
        yield fake
    finally:
        fake.close()


def run_ask(capsys, args=ASK, exit_code=0):
    if exit_code:
        with pytest.raises(SystemExit) as error:
            client.main(args)
        assert error.value.code == exit_code
    else:
        client.main(args)
    stdout, stderr = capsys.readouterr()
    assert not stderr
    line, = stdout.splitlines()
    return json.loads(line)


def assert_cleanup(mcp):
    assert mcp.calls[-2:] == [
        ("hud_clear", SCOPE, {"surface": SURFACE, "key": mcp.card()["key"]}),
        ("DELETE", SCOPE, {}),
    ]


def test_armed_answer_opens_own_folder_scope_and_publishes_one_finite_card(mcp, capsys):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "action": "ship"}
    assert run_ask(capsys) == {"action": "ship"}
    method, scope, params = mcp.calls[0]
    assert (method, scope, params["_meta"]) == ("initialize", None, {"tze-hud/label": "project-folder"})
    assert [scope for _, scope, _ in mcp.calls[1:]] == [SCOPE] * 3
    assert mcp.args("hud_publish") == [{
        "surface": SURFACE,
        "content": {"title": "Ship the patch?", "actions": [
            {"label": "Ship", "callback_id": "ship"},
            {"label": "Later", "callback_id": "later"},
        ]},
        "key": mcp.card()["key"], "ttl_ms": 60_000, "wait_ms": 55_000,
    }]
    assert mcp.card()["key"]
    assert mcp.args("hud_input") == []
    assert_cleanup(mcp)


def test_delayed_answer_acks_only_the_exact_key_and_surface(mcp, capsys):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "expires_in_ms": 10_000}

    def input_items(arguments):
        if "ack" in arguments:
            return {"items": []}
        key = mcp.card()["key"]
        return {"items": [
            {"id": "foreign-card", "s": SURFACE, "key": "other-card", "action": "ship"},
            {"id": "foreign-portal", "s": "portal:other", "key": key, "action": "ship"},
            {"id": "own", "s": SURFACE, "key": key, "action": "ship"},
        ]}

    mcp.responses["hud_input"] = input_items
    assert run_ask(capsys) == {"action": "ship"}
    first_poll, ack = mcp.args("hud_input")
    assert first_poll["wait_ms"] > 0
    assert ack == {"ack": ["own"], "max_items": 0}
    assert_cleanup(mcp)


def test_busy_armed_wait_falls_back_without_republishing(mcp, capsys):
    busy = {"code": "INTERNAL", "hint": "input state busy; retry hud_input"}
    mcp.responses["hud_publish"] = lambda _: busy
    acks = []

    def input_items(arguments):
        if "ack" in arguments:
            acks.append(arguments["ack"])
            return busy if len(acks) == 1 else {"items": []}
        return {"items": [{"id": "own", "s": SURFACE, "key": mcp.card()["key"], "action": "ship"}]}

    mcp.responses["hud_input"] = input_items
    assert run_ask(capsys) == {"action": "ship"}
    assert len(mcp.args("hud_publish")) == 1
    assert acks == [["own"], ["own"]]
    assert_cleanup(mcp)


@pytest.mark.parametrize("answered", [True, False])
def test_busy_armed_wait_respects_the_huds_remaining_lifetime(mcp, monkeypatch, capsys, answered):
    now_ms = 0
    expires_at_ms = 57_000
    answer_at_ms = 56_250

    def advance(seconds):
        nonlocal now_ms
        now_ms += round(seconds * 1_000)

    monkeypatch.setattr(client, "time", SimpleNamespace(monotonic=lambda: now_ms / 1_000, sleep=advance))

    def busy_after_arm(_):
        advance(55.5)
        return {"code": "INTERNAL", "hint": "input state busy; retry hud_input"}

    def input_items(arguments):
        if "ack" in arguments:
            return {"items": []}
        wait_ms = arguments["wait_ms"]
        if answered and now_ms + wait_ms >= answer_at_ms:
            advance((answer_at_ms - now_ms) / 1_000)
            return {"items": [{"id": "own", "s": SURFACE, "key": mcp.card()["key"], "action": "ship"}]}
        advance(wait_ms / 1_000)
        return {"items": []}

    mcp.responses["hud_publish"] = busy_after_arm
    mcp.responses["hud_input"] = input_items
    mcp.responses["hud_surfaces"] = lambda _: {"surfaces": [
        {"s": SURFACE, "held": True, "expires_in_ms": max(0, expires_at_ms - now_ms)},
    ]}
    args = [*ASK, "--ttl-ms", "55000"]
    if answered:
        assert run_ask(capsys, args) == {"action": "ship"}
        assert mcp.args("hud_input")[-1] == {"ack": ["own"], "max_items": 0}
        assert now_ms == answer_at_ms
    else:
        assert run_ask(capsys, args, exit_code=2) == {"gone": "expired"}
        assert mcp.args("hud_input")[-1] == {"wait_ms": 0}
        assert now_ms == expires_at_ms
    assert len(mcp.args("hud_publish")) == 1
    assert_cleanup(mcp)


@pytest.mark.parametrize("reason", ["expired", "cleared"])
def test_gone_during_armed_wait(mcp, capsys, reason):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "gone": reason}
    assert run_ask(capsys, exit_code=2) == {"gone": reason}
    assert mcp.args("hud_input") == []
    assert_cleanup(mcp)


@pytest.mark.parametrize(("remaining", "surfaces", "reason"), [
    (10_000, [{"s": SURFACE, "held": True, "expires_in_ms": 0}], "expired"),
    (10_000, [{"s": SURFACE}], "cleared"),
])
def test_gone_after_armed_wait(mcp, capsys, remaining, surfaces, reason):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "expires_in_ms": remaining}
    mcp.responses["hud_surfaces"] = lambda _: {"surfaces": surfaces}
    mcp.responses["hud_input"] = lambda _: {"items": []}
    assert run_ask(capsys, exit_code=2) == {"gone": reason}
    assert mcp.args("hud_input")[-1] == {"wait_ms": 0}
    assert_cleanup(mcp)


@pytest.mark.parametrize("surface", [
    {"s": SURFACE},
    {"s": SURFACE, "held": True, "expires_in_ms": 0},
])
def test_answer_between_empty_input_and_gone_discovery_wins(mcp, capsys, surface):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "expires_in_ms": 10_000}
    mcp.responses["hud_surfaces"] = lambda _: {"surfaces": [surface]}

    def input_items(arguments):
        if arguments.get("wait_ms") or "ack" in arguments:
            return {"items": []}
        return {"items": [{"id": "own", "s": SURFACE, "key": mcp.card()["key"], "action": "ship"}]}

    mcp.responses["hud_input"] = input_items
    assert run_ask(capsys) == {"action": "ship"}
    assert mcp.args("hud_input")[-1] == {"ack": ["own"], "max_items": 0}
    assert_cleanup(mcp)


def test_human_dismiss_is_gone_but_dismiss_callback_is_an_answer(mcp, capsys):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "action": "dismiss", "kind": "dismiss"}
    args = ["ask", "--title", "Choose", "--action", "Dismiss=dismiss"]
    assert run_ask(capsys, args, exit_code=2) == {"gone": "dismissed"}
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "action": "dismiss"}
    assert run_ask(capsys, args) == {"action": "dismiss"}
    assert mcp.args("hud_publish")[0]["key"] != mcp.args("hud_publish")[1]["key"]


@pytest.mark.parametrize("signum", [signal.SIGTERM, signal.SIGINT, signal.SIGHUP])
def test_clean_signal_clears_the_card_during_armed_http_wait(mcp, tmp_path, signum):
    config = tmp_path / ".config" / "tze-hud"
    config.mkdir(parents=True, mode=0o700)
    pair = config / "127.0.0.1.psk"
    pair.touch(mode=0o600)
    pair.write_text(PAIR)
    mcp.release_publish.clear()
    env = {
        **os.environ, "HOME": str(tmp_path), "NO_PROXY": "127.0.0.1",
        "HUD_HOST": f"127.0.0.1:{mcp.server.server_port}",
    }
    with subprocess.Popen(
        [sys.executable, str(SCRIPT), *ASK, "--ttl-ms", "80000"],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    ) as child:
        try:
            assert mcp.published.wait(5)
            child.send_signal(signum)
            stdout, stderr = child.communicate(timeout=8)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate()
    assert (child.returncode, stdout, stderr) == (2, '{"gone":"cleared"}\n', "")
    assert mcp.card()["ttl_ms"] == 80_000
    assert_cleanup(mcp)


def test_auth_failure_reports_one_json_error_without_unscoped_publish(mcp, capsys):
    mcp.status = 401
    assert run_ask(capsys, exit_code=1)["code"] == "MCP_AUTH"
    assert [name for name, _, _ in mcp.calls] == ["initialize"]


def test_missing_session_header_fails_closed(mcp, capsys):
    mcp.scope_header = None
    assert run_ask(capsys, exit_code=1)["code"] == "MCP_PROTOCOL"
    assert [name for name, _, _ in mcp.calls] == ["initialize"]


def test_unreachable_hud_reports_one_json_error(mcp, monkeypatch, capsys):
    def unreachable(*_args, **_kwargs):
        raise urllib.error.URLError("synthetic connection failure")

    monkeypatch.setattr(client.urllib.request, "urlopen", unreachable)
    assert run_ask(capsys, exit_code=1)["code"] == "MCP_TRANSPORT"
    assert not mcp.calls


@pytest.mark.parametrize("args", [ASK[:3], [*ASK, "--ttl-ms", "0"], [*ASK, "--action", "malformed"]])
def test_bad_arguments_report_one_json_error_before_any_hud_call(mcp, capsys, args):
    assert run_ask(capsys, args, exit_code=1)["code"] == "INVALID_ARGUMENT"
    assert not mcp.calls


def test_library_allows_named_scope_and_a_follow_up_notice(mcp):
    mcp.responses["hud_publish"] = lambda _: {"ok": True, "action": "merge.123"}
    card = {"title": "#7 ready", "body": "Green", "actions": [
        {"label": "Merge", "callback_id": "merge.123"},
        {"label": "Review", "url": "https://example.com/pr/7"},
    ]}
    with client.AskSession(label="PR #7") as ask:
        ask.publish(card, 600_000, key="pr:7")
        assert ask.wait() == {"action": "merge.123"}
        ask.clear()
        ask.call_tool("hud_publish", {"surface": SURFACE, "content": {"title": "#7 queued"}, "ttl_ms": 1_000})
    assert mcp.args("initialize")[0]["_meta"] == {"tze-hud/label": "PR #7"}
    assert mcp.card()["content"] == card
    assert mcp.args("hud_clear") == [{"surface": SURFACE, "key": "pr:7"}]
    assert mcp.calls[-1] == ("DELETE", SCOPE, {})
