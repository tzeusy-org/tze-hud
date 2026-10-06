"""Unit tests for the hud-projection portal client (no network)."""

import importlib.util
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import uuid
from unittest import mock

import pytest

ROOT = Path(__file__).resolve().parents[4]
SPEC = importlib.util.spec_from_file_location(
    "portal_client", ROOT / ".claude/skills/hud-projection/scripts/portal_client.py"
)
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)


def tool_response(value, is_error=False):
    result = {"content": [{"type": "text", "text": json.dumps(value)}]}
    if is_error:
        result["isError"] = True
    return {"jsonrpc": "2.0", "id": 1, "result": result}


class FakeServer:
    def __init__(self, replies):
        self.calls = []
        self.replies = list(replies)

    def __call__(self, method, params, request_id=1):
        self.calls.append((method, params))
        return self.replies.pop(0)


def run(argv, replies):
    server = FakeServer(replies)
    with mock.patch.object(client, "rpc", server):
        client.main(argv)
    return server.calls


def test_publish_attaches_through_hud_publish(capsys):
    calls = run(
        ["publish", "--id", "s1", "--text", "hi", "--expects-reply", "--state", "active"],
        [tool_response({"ok": True})],
    )
    assert calls == [
        (
            "tools/call",
            {
                "name": "hud_publish",
                "arguments": {
                    "surface": "portal:s1",
                    "content": "hi",
                    "expects_reply": True,
                    "status": "active",
                },
            },
        )
    ]
    assert json.loads(capsys.readouterr().out) == {"ok": True}
    with mock.patch.object(client, "rpc", return_value=tool_response({"ok": True})) as rpc:
        assert client.call_tool("hud_publish", {"surface": "portal:s1"}, timeout=0.35) == {"ok": True}
        rpc.assert_called_once_with("tools/call", {"name": "hud_publish", "arguments": {"surface": "portal:s1"}}, timeout=0.35)
    response = mock.MagicMock()
    response.__enter__.return_value.read.return_value = json.dumps(tool_response({"ok": True})).encode()
    with mock.patch.object(client, "endpoint", return_value=("http://127.0.0.1/mcp", "synthetic-pair")), mock.patch.object(client.urllib.request, "urlopen", return_value=response) as http:
        client.rpc("tools/call", {})
        assert http.call_args.kwargs["timeout"] == 60
        client.rpc("tools/call", {}, timeout=0.35)
        assert http.call_args.kwargs["timeout"] == 0.35


def test_poll_prints_ndjson_and_acks_on_next_call(capsys):
    item = {"id": "i1", "s": "portal:s1", "text": "yes"}
    calls = run(
        ["poll", "--wait-ms", "10", "--ack"],
        [tool_response({"items": [item], "remaining": 0}), tool_response({"items": [], "remaining": 0})],
    )
    assert [c[1]["name"] for c in calls] == ["hud_input", "hud_input"]
    assert calls[1][1]["arguments"] == {"ack": ["i1"]}
    assert json.loads(capsys.readouterr().out.strip()) == item


def test_poll_without_input_exits_3():
    with pytest.raises(SystemExit) as exit_info:
        run(["poll", "--wait-ms", "10"], [tool_response({"items": [], "remaining": 0})])
    assert exit_info.value.code == 3


def test_tool_error_prints_code_and_hint(capsys):
    with pytest.raises(SystemExit) as exit_info:
        run(
            ["clear", "--id", "s1"],
            [tool_response({"code": "NOT_HELD", "hint": "hud_publish first"}, is_error=True)],
        )
    assert exit_info.value.code == 1
    assert json.loads(capsys.readouterr().out) == {"code": "NOT_HELD", "hint": "hud_publish first"}


def test_hold_calls_hud_hold_with_portal_surface():
    calls = run(["hold", "--id", "s1", "--ttl-ms", "600000"], [tool_response({"ok": True})])
    assert calls == [
        (
            "tools/call",
            {"name": "hud_hold", "arguments": {"surface": "portal:s1", "ttl_ms": 600000}},
        )
    ]


HOOK = ROOT / ".claude/skills/hud-projection/scripts/portal_hook.py"


class HookHarness:
    """Docs-derived synthetic payloads, real processes and loopback MCP only."""

    def __init__(self, home):
        self.home = home
        home.mkdir()
        self.records = []
        self.release = threading.Event()
        self.received = threading.Event()
        self.block_text = None
        self.fail_text = None
        self.fail_remaining = 0
        self.trickle = False
        self.guard = threading.Lock()
        harness = self

        class Server(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with harness.guard:
                    harness.records.append(request)
                arguments = request["params"]["arguments"]
                content = arguments.get("content")
                if harness.block_text is not None and content == harness.block_text:
                    harness.received.set()
                    harness.release.wait(2)
                if content == harness.fail_text and harness.fail_remaining:
                    harness.fail_remaining -= 1
                    self.send_response(503)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                response = json.dumps(tool_response({"ok": True})).encode()
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                try:
                    if harness.trickle:
                        harness.received.set()
                        for byte in response:
                            self.wfile.write(bytes([byte]))
                            self.wfile.flush()
                            if harness.release.wait(0.1):
                                break
                    else:
                        self.wfile.write(response)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.env = os.environ.copy()
        self.env.update(HOME=str(home), HUD_HOST=f"127.0.0.1:{self.server.server_port}")
        paired = home / ".config/tze-hud"
        paired.mkdir(parents=True)
        psk = paired / "127.0.0.1.psk"
        psk.write_text("synthetic-hook-pair-not-a-real-secret")
        psk.chmod(0o600)
        self.prompt = str(uuid.uuid4())

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.release.set()
        self.wait_idle()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)

    def payload(self, kind, **fields):
        value = {"hook_event_name": kind, "session_id": "synthetic-session", "prompt_id": self.prompt}
        value.update(fields)
        return value

    def launch(self, event):
        raw = event if isinstance(event, bytes) else json.dumps(event).encode()
        process = subprocess.Popen([sys.executable, str(HOOK)], cwd=self.home, env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        process.stdin.write(raw)
        process.stdin.close()
        process.stdin = None
        return process

    def finish(self, process):
        stdout, stderr = process.communicate(timeout=3)
        assert process.returncode == 0
        assert stdout == stderr == b""
        assert not Path(f"/proc/{process.pid}").exists()

    def event(self, kind, wait=True, **fields):
        self.finish(self.launch(self.payload(kind, **fields)))
        if wait:
            self.wait_idle()

    def workers(self):
        # Restrict observation to this fixture's private cwd; never inspect
        # another session's environment, credentials or process arguments.
        found = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                if (entry / "cwd").resolve() == self.home:
                    args = (entry / "cmdline").read_bytes().split(b"\0")
                    if str(HOOK).encode() in args:
                        found.append(int(entry.name))
            except (OSError, RuntimeError):
                continue
        return found

    def wait_idle(self):
        deadline = time.monotonic() + 3
        while self.workers() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert not self.workers(), "owned hook supervisor/HTTP child did not terminate"

    def boot(self):
        self.event("SessionStart", source="startup")
        self.event("UserPromptSubmit")

    def publications(self):
        return [r["params"]["arguments"] for r in self.records if r["params"]["name"] == "hud_publish"]

    def marker(self):
        paths = list((self.home / ".cache/tze-hud/claude-hooks").glob("*/marker.json"))
        assert len(paths) == 1
        return paths[0]


def test_hook_lifecycle_over_real_mcp_surface(tmp_path):
    with HookHarness(tmp_path / "lifecycle") as h:
        h.boot()
        canary = "tool-input-and-response-private-canary"
        for kind, tool_id in [("PreToolUse", "read-1"), ("PostToolUse", "read-1"), ("PreToolUse", "failure-2"), ("PostToolUseFailure", "failure-2")]:
            h.event(kind, tool_use_id=tool_id, tool_name="Read", tool_input={"secret": canary}, tool_response=canary, error=canary)
        h.event("PreToolUse", tool_use_id="unknown", tool_name="mcp__private_server__private_name")
        h.event("Stop", last_assistant_message="\x1b[31mFinal reply\x1b[0m\x00", stop_hook_active=True)
        publishes = h.publications()
        content = [p for p in publishes if "content" in p]
        assert [p["content"] for p in content] == ["Read: running", "Read: completed", "Read: running", "Read: failed", "MCP tool: running", "Final reply"]
        assert content[0]["key"] == content[1]["key"]
        assert content[2]["key"] == content[3]["key"] != content[0]["key"]
        assert content[-1]["status"] == "attached"
        assert all(p["surface"] == "portal:synthetic-session" and "ttl_ms" not in p for p in publishes)
        assert all(len(p["key"].encode()) < 128 for p in content)
        assert all(r["method"] == "tools/call" for r in h.records)
        holds = [r["params"]["arguments"] for r in h.records if r["params"]["name"] == "hud_hold"]
        assert len(holds) == len(publishes)
        assert all(p == {"surface": "portal:synthetic-session", "ttl_ms": 600000} for p in holds)
        h.event("SessionEnd")
        assert h.records[-1]["params"] == {"name": "hud_clear", "arguments": {"surface": "portal:synthetic-session"}}
        state_files = list((h.home / ".cache/tze-hud/claude-hooks").glob("*/*"))
        metadata = b"".join(p.read_bytes() for p in state_files)
        assert canary not in json.dumps(h.records)
        assert b"Final reply" not in metadata and canary.encode() not in metadata
        assert b"synthetic-hook-pair" not in metadata
        assert all(p.stat().st_mode & 0o077 == 0 for p in state_files)
        assert json.loads(h.marker().read_text())["ended"]
        tombstone = h.marker()
        h.event("SessionStart", session_id="another-session", source="startup")
        assert tombstone.exists()  # a recent closed generation is retained
        expired = json.loads(tombstone.read_text())
        expired["updated"] = time.time() - 86401
        tombstone.write_text(json.dumps(expired))
        h.event("SessionStart", session_id="another-session", source="resume")
        assert not tombstone.exists()
        assert (tombstone.parent / "marker.lock").exists()  # lock identity survives expiry


def test_hook_subprocess_replay_prompt_races_and_reaping(tmp_path):
    import fcntl
    with HookHarness(tmp_path / "ordering") as h:
        h.boot()
        h.event("PostToolUse", tool_use_id="late", tool_name="Bash")
        h.event("PreToolUse", tool_use_id="late", tool_name="Bash")
        assert [p.get("content") for p in h.publications()].count("Bash: completed") == 1
        assert not any(p.get("content") == "Bash: running" for p in h.publications())
        h.fail_text = "retry final"
        h.fail_remaining = 1
        h.event("Stop", last_assistant_message="retry final")
        h.event("PreToolUse", tool_use_id="after-stop", tool_name="Read")
        h.event("Stop", last_assistant_message="retry final", stop_hook_active=True)
        delivered = len(h.records)
        h.event("Stop", last_assistant_message="retry final")
        assert len(h.records) == delivered
        assert [p.get("content") for p in h.publications()].count("retry final") == 2
        old_prompt = h.prompt
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        before = len(h.records)
        h.event("Stop", prompt_id=old_prompt, last_assistant_message="stale before send")
        assert len(h.records) == before

        # Control an already-SENT final. The synchronous marker returns while
        # its network request is still held; release the ACK only afterwards.
        h.block_text = "inflight old final"
        stopping = h.launch(h.payload("Stop", last_assistant_message=h.block_text))
        assert h.received.wait(2)
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit", wait=False)
        marker = json.loads(h.marker().read_text())
        assert marker["prompt"] == h.prompt and not h.release.is_set()
        h.release.set()
        h.finish(stopping)
        h.wait_idle()
        after_final = h.publications()
        index = next(i for i, p in enumerate(after_final) if p.get("content") == "inflight old final")
        assert after_final[index]["status"] == "attached"
        assert after_final[index + 1:]
        assert after_final[-1]["status"] == "active"
        assert all("content" not in p and p["status"] == "active" for p in after_final[index + 1:])
        assert len(after_final[index + 1:]) <= 2  # one catch-up and the new prompt's own status

        # End and reopen while an old acknowledged request is pending. Both
        # old cleanup paths must leave the newly opened generation untouched.
        h.release.clear()
        h.received.clear()
        h.block_text = "old generation final"
        stopping = h.launch(h.payload("Stop", last_assistant_message=h.block_text))
        assert h.received.wait(2)
        ending = h.launch(h.payload("SessionEnd"))
        deadline = time.monotonic() + 0.2
        while not json.loads(h.marker().read_text())["ended"] and time.monotonic() < deadline:
            time.sleep(0.001)
        assert json.loads(h.marker().read_text())["ended"]
        old_generation = json.loads(h.marker().read_text())["generation"]
        clears = sum(r["params"]["name"] == "hud_clear" for r in h.records)
        h.event("SessionStart", source="resume", wait=False)
        assert json.loads(h.marker().read_text())["generation"] != old_generation
        h.release.set()
        h.finish(stopping)
        h.finish(ending)
        assert sum(r["params"]["name"] == "hud_clear" for r in h.records) == clears
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        assert h.publications()[-1]["status"] == "active"

        h.event("SessionEnd")
        h.event("SessionStart", source="compact")
        h.event("SessionStart", source="fork")
        assert json.loads(h.marker().read_text())["ended"]
        h.event("SessionStart", source="resume")
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        generation = json.loads(h.marker().read_text())["generation"]
        with open(h.marker().parent / "marker.lock", "r+") as locked:
            fcntl.flock(locked, fcntl.LOCK_EX)
            h.event("UserPromptSubmit", prompt_id=str(uuid.uuid4()))
        assert (h.marker().parent / "disabled.json").exists()

        before = len(h.records)
        h.event("PreToolUse", tool_use_id="disabled", tool_name="Read")
        assert len(h.records) == before
        h.event("SessionStart", source="clear")
        assert not (h.marker().parent / "disabled.json").exists()
        assert json.loads(h.marker().read_text())["generation"] != generation

    with HookHarness(tmp_path / "watchdog") as h:
        h.boot()
        h.trickle = True  # each byte beats socket timeout; total read never completes
        started = time.monotonic()
        h.event("PreToolUse", tool_use_id="stall", tool_name="Read")
        assert h.received.is_set()
        assert time.monotonic() - started < 2.5
        assert not h.workers()  # watchdog kills/reaps its actual HTTP process
        h.release.set()


def test_hook_unavailable_malformed_privacy_and_state_failures(tmp_path):
    with HookHarness(tmp_path / "failures") as h:
        h.boot()
        before = len(h.records)
        invalid = [b"{", b"[]", b"x" * 65537,
                   h.payload("PreToolUse", prompt_id=None, tool_name="Read", tool_use_id="x"),
                   h.payload("PreToolUse", session_id="../escape", tool_name="Read", tool_use_id="x"),
                   h.payload("PreToolUse", agent_id="subagent", tool_name="Read", tool_use_id="x"),
                   h.payload("Unknown"), h.payload("SessionStart", source="compact")]
        for value in invalid:
            h.finish(h.launch(value))
        assert len(h.records) == before
        for final in ["sk-ant-" + "s" * 32, "sk-ant-\x1b[31m" + "s" * 32 + "\x1b[0m", "-----BEGIN PRIVATE KEY-----\nsynthetic", "password=synthetic-canary-12345"]:
            h.prompt = str(uuid.uuid4())
            h.event("UserPromptSubmit")
            h.event("Stop", last_assistant_message=final)
            assert h.publications()[-1] == {"surface": "portal:synthetic-session", "status": "attached"}
            assert final not in json.dumps(h.records)
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        h.event("Stop", last_assistant_message=None)
        assert "content" not in h.publications()[-1]
        h.event("Stop", last_assistant_message="status-only")
        assert h.publications()[-1].get("content") == "status-only"
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        h.event("Stop", last_assistant_message="界" * 4000)
        truncated = h.publications()[-1]["content"]
        assert len(truncated.encode()) <= 8192 and truncated.endswith("[truncated]")
        assert "�" not in truncated
        h.event("SessionStart", source="resume")
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        h.event("PreToolUse", tool_use_id="retained", tool_name="Read")
        retained_key = h.publications()[-1]["key"]
        delivery_path = h.marker().parent / "delivery.json"
        saturated = json.loads(delivery_path.read_text())
        saturated["tools"] = {f"cc-tool-{i:064x}": 1 for i in range(255)}
        saturated["tools"][retained_key] = 1
        delivery_path.write_text(json.dumps(saturated))
        before = len(h.records)
        h.event("PreToolUse", tool_use_id="over-cap", tool_name="Read")
        assert len(h.records) == before
        h.event("PostToolUse", tool_use_id="retained", tool_name="Read")
        assert h.publications()[-1]["content"] == "Read: completed"
        assert len(json.loads(delivery_path.read_text())["tools"]) == 256
        h.marker().write_text("invalid JSON")
        before = len(h.records)
        h.event("PreToolUse", tool_use_id="broken-state", tool_name="Read")
        assert len(h.records) == before
        assert (h.marker().parent / "disabled.json").exists()

    with HookHarness(tmp_path / "unsafe-cache") as h:
        outside = h.home / "untouched"
        outside.mkdir()
        cache = h.home / ".cache/tze-hud"
        cache.mkdir(parents=True)
        (cache / "claude-hooks").symlink_to(outside, target_is_directory=True)
        h.event("SessionStart", source="startup")
        h.event("UserPromptSubmit")
        assert not h.records and not list(outside.iterdir())

    with HookHarness(tmp_path / "unavailable") as h:
        h.boot()
        h.server.shutdown()
        h.server.server_close()
        started = time.monotonic()
        h.event("PreToolUse", tool_use_id="down", tool_name="Read")
        assert time.monotonic() - started < 2.5
        assert not h.workers()
        (h.home / ".config/tze-hud/127.0.0.1.psk").unlink()
        h.event("Stop", last_assistant_message="unpaired")
        assert not h.workers()
