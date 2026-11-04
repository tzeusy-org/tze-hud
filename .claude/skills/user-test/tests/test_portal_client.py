"""Unit tests for the hud-projection portal client (no network)."""

import importlib.util
import http.server
import json
import os
import select
import shutil
import signal
import socket
import struct
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
        ["publish", "--id", "s1", "--text", "hi", "--expects-reply", "--state", "active", "--ttl-ms", "0"],
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
                    "ttl_ms": 0,
                },
            },
        )
    ]
    assert json.loads(capsys.readouterr().out) == {"ok": True}
    calls = run(["publish", "--id", "s1"], [tool_response({"ok": True})])
    assert calls[0][1]["arguments"] == {"surface": "portal:s1"}
    capsys.readouterr()
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

    import publish_widget_batch as widgets
    response.__enter__.return_value.read.return_value = json.dumps(tool_response({"ok": True})).encode()
    with mock.patch.object(widgets.urllib.request, "urlopen", return_value=response) as http:
        assert widgets.rpc_call("http://127.0.0.1/mcp", "synthetic", "hud_clear", {}, 1) == {"result": {"ok": True}}
        assert http.call_args.kwargs["timeout"] == 20
        response.__enter__.return_value.read.assert_called_with()
        widgets.rpc_call("http://127.0.0.1/mcp", "synthetic", "hud_clear", {}, 1, timeout=0.2, response_limit=4096)
        assert http.call_args.kwargs["timeout"] == 0.2
        response.__enter__.return_value.read.assert_called_with(4097)
        with pytest.raises(ValueError):
            widgets.rpc_call("http://127.0.0.1/mcp", "synthetic", "hud_clear", {}, 1, response_limit=4)


def test_poll_prints_ndjson_and_acks_on_next_call(capsys):
    item = {"id": "i1", "s": "portal:s1", "text": "yes"}
    calls = run(
        ["poll", "--wait-ms", "10", "--ack"],
        [tool_response({"items": [item], "remaining": 0}), tool_response({"items": [], "remaining": 0})],
    )
    assert [c[1]["name"] for c in calls] == ["hud_input", "hud_input"]
    assert calls[1][1]["arguments"] == {"ack": ["i1"]}
    assert json.loads(capsys.readouterr().out.strip()) == item


def test_poll_with_ack_stops_at_first_reply(capsys):
    """A background waiter (--rounds N --ack) must exit on the first reply."""
    item = {"id": "i1", "s": "portal:s1", "text": "yes"}
    calls = run(
        ["poll", "--wait-ms", "10", "--rounds", "5", "--ack"],
        [
            tool_response({"items": [], "remaining": 0}),
            tool_response({"items": [item], "remaining": 0}),
            tool_response({"items": [], "remaining": 0}),
        ],
    )
    assert [c[1]["arguments"] for c in calls] == [
        {"wait_ms": 10, "max_items": calls[0][1]["arguments"]["max_items"]},
        {"wait_ms": 10, "max_items": calls[0][1]["arguments"]["max_items"]},
        {"ack": ["i1"]},
    ]
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
        self.private_cwd = str(home.resolve())
        self.records = []
        self.committed = []
        self.hold_before_commit = False
        self.hold_timeout = 2
        self.drop_ack = False
        self.tool_reply = None
        self.ack_sent = threading.Event()
        self.block_clear = False
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
                content = arguments.get("content", arguments.get("params", {}).get("readout"))
                held = ((harness.block_text is not None and content == harness.block_text)
                        or (harness.block_clear and request["params"]["name"] == "hud_clear"))
                if not (harness.hold_before_commit and held):
                    harness.committed.append(request)
                if held:
                    harness.received.set()
                    harness.release.wait(harness.hold_timeout)
                if harness.hold_before_commit and held:
                    harness.committed.append(request)
                if harness.drop_ack:
                    self.close_connection = True
                    return
                if content == harness.fail_text and harness.fail_remaining:
                    harness.fail_remaining -= 1
                    self.send_response(503)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                response = json.dumps(harness.tool_reply or tool_response({"ok": True})).encode()
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
                        self.wfile.flush()
                        harness.ack_sent.set()
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server)
        self.server.daemon_threads = True
        # Avoid paying the default half-second shutdown poll for every case.
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.01), daemon=True)
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
        assert not self.thread.is_alive()

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
                if os.readlink(entry / "cwd") == self.private_cwd:
                    args = (entry / "cmdline").read_bytes().split(b"\0")
                    if str(getattr(self, "worker_script", HOOK)).encode() in args:
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
        assert "status" not in content[-1]
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
        assert "status" not in after_final[index]
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
        # Marker replacement precedes marker.lock release. Reopen only after
        # metadata admission is ready, while the old delivery is still held.
        marker_lock_path = h.marker().parent / "marker.lock"
        marker_ready = False
        with marker_lock_path.open("r+") as marker_lock:
            lock_identity = os.fstat(marker_lock.fileno())
            while time.monotonic() < deadline:
                try:
                    fcntl.flock(marker_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    pass
                else:
                    try:
                        current_lock = marker_lock_path.stat()
                        assert (current_lock.st_dev, current_lock.st_ino) == (
                            lock_identity.st_dev, lock_identity.st_ino
                        )
                        marker_ready = (
                            json.loads(h.marker().read_text())["ended"]
                            and time.monotonic() <= deadline
                        )
                    finally:
                        fcntl.flock(marker_lock, fcntl.LOCK_UN)
                if marker_ready:
                    break
                time.sleep(0.001)
        assert marker_ready, "ended marker and metadata lock were not ready within 0.2s"
        assert json.loads(h.marker().read_text())["ended"]
        assert ending.poll() is None
        assert stopping.poll() is None and h.received.is_set() and not h.release.is_set()
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

    # Terminate only the known fixture supervisor after its HTTP child has
    # actually been admitted. This does not model Claude's cancellation policy
    # or imply that stopping a client rolls back an already-sent request.
    import signal

    observations = []
    observation_path = tmp_path / "cancellation-observations.json"

    def record_observations():
        observation_path.write_text(json.dumps(observations, indent=2))

    def process_identity(pid):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text()
            fields = stat[stat.rfind(")") + 2:].split()
            return {"pid": pid, "state": fields[0], "ppid": int(fields[1]),
                    "pgid": int(fields[2]), "start_ticks": int(fields[19])}
        except (OSError, ValueError, IndexError):
            return None

    def child_running(identity):
        current = process_identity(identity["pid"])
        return (current is not None and current["start_ticks"] == identity["start_ticks"]
                and current["state"] not in ("Z", "X"))

    for kind, termination, budget in [
        ("PreToolUse", signal.SIGKILL, 1.0),
        ("PreToolUse", signal.SIGTERM, 1.0),
        ("SessionEnd", signal.SIGKILL, 1.2),
        ("SessionEnd", signal.SIGTERM, 1.2),
    ]:
        with HookHarness(tmp_path / f"cancel-{kind}-{termination.value}") as h:
            h.boot()
            h.trickle = True
            parent = None
            child = None
            observe_next = False
            admission_overlaps = []
            original_POST = h.server.RequestHandlerClass.do_POST

            def observe_POST(handler):
                if observe_next and child is not None and child_running(child):
                    admission_overlaps.append(process_identity(child["pid"]))
                original_POST(handler)

            h.server.RequestHandlerClass.do_POST = observe_POST
            row = {"event": kind, "signal": termination.name, "production_budget": budget,
                   "startup_scheduler_slack": 1.5, "remote_rollback_claimed": False}
            observations.append(row)
            started = time.monotonic()
            try:
                parent = h.launch(h.payload(kind, tool_use_id="cancelled", tool_name="Read"))
                row["supervisor"] = process_identity(parent.pid)
                assert h.received.wait(2), "owned HTTP child was not admitted"
                row["received_seconds"] = time.monotonic() - started
                # Read only this known parent's direct children, then verify
                # cwd, command, start time and the child's separate group.
                children = Path(f"/proc/{parent.pid}/task/{parent.pid}/children").read_text().split()
                candidates = []
                for value in children:
                    pid = int(value)
                    identity = process_identity(pid)
                    if identity is None:
                        continue
                    args = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
                    if (Path(f"/proc/{pid}/cwd").resolve() == h.home
                            and str(HOOK).encode() in args and b"--deliver" in args):
                        candidates.append(identity)
                assert len(candidates) == 1
                child = candidates[0]
                assert child["ppid"] == parent.pid and child["pgid"] == child["pid"]
                assert child_running(child)
                row["admitted_child"] = child.copy()
                record_observations()

                # Never killpg(parent.pid): the public fixture process may
                # share pytest's group. SIGKILL cannot execute parent finally.
                parent.send_signal(termination)
                stdout, stderr = parent.communicate(timeout=2)
                assert stdout == stderr == b""
                row["supervisor_returncode"] = parent.returncode
                row["supervisor_terminal_seconds"] = time.monotonic() - started
                row["child_live_after_supervisor"] = child_running(child)
                if row["child_live_after_supervisor"]:
                    with (h.marker().parent / "delivery.lock").open("r+") as delivery_lock:
                        try:
                            fcntl.flock(delivery_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            lock_retained = False
                        except BlockingIOError:
                            lock_retained = True
                    row["delivery_lock_retained_while_child_live"] = lock_retained
                    record_observations()
                    assert lock_retained, "supervisor death released delivery ownership while its HTTP child was live"

                    observe_next = True
                    if kind == "SessionEnd":
                        h.event("SessionStart", source="resume", wait=False)
                    else:
                        h.event("PreToolUse", tool_use_id="after-cancel", tool_name="Bash", wait=False)
                    assert not admission_overlaps, "next delivery overlapped the surviving old HTTP client"

                # An explicit 1.5s interpreter/scheduler allowance is separate
                # from the unchanged absolute production network deadline.
                limit = started + budget + 1.5
                while child_running(child) and time.monotonic() < limit:
                    time.sleep(0.01)
                row["client_terminal_seconds"] = time.monotonic() - started
                row["client_terminal_identity"] = process_identity(child["pid"])
                row["overlapping_next_admissions"] = admission_overlaps
                record_observations()
                assert not child_running(child), "HTTP child outlived independent cancellation deadline"
                assert row["client_terminal_seconds"] < budget + 1.5
                if termination == signal.SIGKILL:
                    assert parent.returncode == -signal.SIGKILL
                    row["reaped_by_dead_supervisor_claimed"] = False
                else:
                    assert parent.returncode == 0  # explicit catchable cleanup
                    assert process_identity(child["pid"]) is None  # living parent reaped
                with (h.marker().parent / "delivery.lock").open("r+") as delivery_lock:
                    fcntl.flock(delivery_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

                h.trickle = False
                h.release.set()
                if kind == "SessionEnd":
                    h.event("SessionStart", source="resume")
                    h.prompt = str(uuid.uuid4())
                    h.event("UserPromptSubmit")
                before = len(h.records)
                h.event("PreToolUse", tool_use_id="genuine-recovery", tool_name="Read")
                assert len(h.records) > before
                assert h.publications()[-1]["content"] == "Read: running"
                row["next_genuine_event_recovered"] = True
                record_observations()
            finally:
                # RED can stop at the first ownership assertion. Release the
                # fake server and clean only exact fixture-owned identities.
                h.release.set()
                if parent is not None and parent.poll() is None:
                    parent.kill()
                    parent.communicate(timeout=2)
                if child is not None and child_running(child):
                    try:
                        os.killpg(child["pgid"], signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    cleanup_limit = time.monotonic() + 2
                    while child_running(child) and time.monotonic() < cleanup_limit:
                        time.sleep(0.01)
                    assert not child_running(child), "owned RED/GREEN child cleanup did not terminate"
                row["teardown_child_identity"] = None if child is None else process_identity(child["pid"])
                row["teardown_child_live"] = child is not None and child_running(child)
                record_observations()


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
            assert h.publications()[-1] == {"surface": "portal:synthetic-session"}
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

    # An existing unsafe entry is not absent metadata. Refuse it before any
    # MCP request, even when the session's marker is otherwise valid/current.
    for filename in ("delivery.json", "disabled.json"):
        with HookHarness(tmp_path / ("dangling-" + filename)) as h:
            h.boot()
            marker = json.loads(h.marker().read_text())
            assert marker["prompt"] == h.prompt and not marker["ended"]
            path = h.marker().parent / filename
            path.unlink(missing_ok=True)
            missing = h.home / "missing-state-target"
            path.symlink_to(missing)
            assert path.is_symlink() and not path.exists()
            before = len(h.records)
            started = time.monotonic()
            h.event("PreToolUse", tool_use_id="unsafe-state", tool_name="Read")
            assert len(h.records) == before
            assert time.monotonic() - started < 2.5
            assert not h.workers()
            assert not missing.exists() and path.is_symlink()

            # Only explicit recovery starts a fresh generation; genuinely
            # absent delivery metadata still permits ordinary first delivery.
            path.unlink()
            h.event("SessionStart", source="resume")
            assert not (h.marker().parent / "disabled.json").exists()
            h.prompt = str(uuid.uuid4())
            h.event("UserPromptSubmit")
            h.event("PreToolUse", tool_use_id="recovered-state", tool_name="Read")
            assert h.publications()[-1]["content"] == "Read: running"
            assert not h.workers()

    # Existing empty metadata is corruption, not an absent first-delivery file.
    # Preserve an acknowledged terminal stage before exercising its delayed Pre.
    with HookHarness(tmp_path / "existing-empty-delivery") as h:
        h.boot()
        h.event("PostToolUse", tool_use_id="completed-before-corruption", tool_name="Read")
        key = h.publications()[-1]["key"]
        delivery_path = h.marker().parent / "delivery.json"
        assert json.loads(delivery_path.read_text())["tools"][key] == 2
        delivery_path.write_text("{}")
        before = len(h.records)
        started = time.monotonic()
        h.event("PreToolUse", tool_use_id="completed-before-corruption", tool_name="Read")
        assert len(h.records) == before
        assert time.monotonic() - started < 2.5 and not h.workers()
        assert delivery_path.read_text() == "{}"
        assert (h.marker().parent / "disabled.json").exists()
        h.event("PreToolUse", tool_use_id="disabled-after-corruption", tool_name="Read")
        assert len(h.records) == before

        # Repair the corrupt entry, then explicitly recover. Genuine absence
        # initializes normally; it must not be rejected along with existing {}.
        delivery_path.unlink()
        h.event("SessionStart", source="resume")
        assert not (h.marker().parent / "disabled.json").exists()
        h.prompt = str(uuid.uuid4())
        h.event("UserPromptSubmit")
        h.event("PreToolUse", tool_use_id="first-after-recovery", tool_name="Read")
        assert h.publications()[-1]["content"] == "Read: running"
        assert json.loads(delivery_path.read_text())["generation"] == json.loads(h.marker().read_text())["generation"]
        assert not h.workers()

    for previous_generation in (False, True):
        with HookHarness(tmp_path / ("valid-delivery-" + str(previous_generation))) as h:
            h.boot()
            delivery_path = h.marker().parent / "delivery.json"
            marker = json.loads(h.marker().read_text())
            state = json.loads(delivery_path.read_text())
            assert state == {"generation": marker["generation"], "tools": {}, "finals": {}}
            if previous_generation:
                h.event("PostToolUse", tool_use_id="valid-empty-or-old", tool_name="Read")
                state = json.loads(delivery_path.read_text())
                state["generation"] = "0" * 32
                assert state["generation"] != marker["generation"]
                state["finals"] = {h.prompt: "a" * 64}
            delivery_path.write_text(json.dumps(state))
            before = len(h.records)
            h.event("PreToolUse", tool_use_id="valid-empty-or-old", tool_name="Read")
            assert len(h.records) == before + 2
            assert h.publications()[-1]["content"] == "Read: running"
            saved = json.loads(delivery_path.read_text())
            assert saved["generation"] == marker["generation"] and saved["finals"] == {}
            assert saved["tools"] == {h.publications()[-1]["key"]: 1}
            h.event("PostToolUse", tool_use_id="valid-empty-or-old", tool_name="Read")
            assert h.publications()[-1]["content"] == "Read: completed"
            before = len(h.records)
            h.event("PreToolUse", tool_use_id="valid-empty-or-old", tool_name="Read")
            assert len(h.records) == before and not (h.marker().parent / "disabled.json").exists()

    # Every malformed existing object is validated before a generation reset,
    # including truthy objects and invalid required members. Use the same real
    # subprocess/MCP seam, rather than a second validator-only test species.
    valid = {"generation": "0" * 32, "tools": {}, "finals": {}}
    malformed = [
        ("missing-generation", {k: v for k, v in valid.items() if k != "generation"}),
        ("missing-tools", {k: v for k, v in valid.items() if k != "tools"}),
        ("missing-finals", {k: v for k, v in valid.items() if k != "finals"}),
        ("bad-generation", dict(valid, generation="not-a-generation")),
        ("bad-prompt", dict(valid, prompt="not-a-prompt")),
        ("nonobject-tools", dict(valid, tools=[])),
        ("nonobject-finals", dict(valid, finals=[])),
        ("invalid-stage", dict(valid, tools={"cc-tool-" + "a" * 64: 3})),
        ("invalid-tool-key", dict(valid, tools={"raw-tool-id": 2})),
        ("invalid-final-key", dict(valid, finals={"not-a-prompt": "a" * 64})),
        ("invalid-final-hash", dict(valid, finals={str(uuid.UUID(int=1)): "bad-hash"})),
        ("tool-map-over-cap", dict(valid, tools={f"cc-tool-{i:064x}": 2 for i in range(257)})),
        ("final-map-over-cap", dict(valid, finals={str(uuid.UUID(int=i)): "a" * 64 for i in range(17)})),
        ("unexpected-member", dict(valid, unexpected="synthetic-private-field")),
        ("non-dictionary", []),
    ]
    for name, raw in [(name, json.dumps(value)) for name, value in malformed] + [
        ("invalid-json", "{"), ("oversize-state", " " * 32769), ("unsafe-mode", json.dumps(valid)),
    ]:
        with HookHarness(tmp_path / ("malformed-delivery-" + name)) as h:
            h.boot()
            delivery_path = h.marker().parent / "delivery.json"
            delivery_path.write_text(raw)
            if name == "unsafe-mode":
                delivery_path.chmod(0o644)
            before = len(h.records)
            h.event("PreToolUse", tool_use_id="malformed-existing-state", tool_name="Read")
            assert len(h.records) == before, name
            assert (h.marker().parent / "disabled.json").exists(), name
            assert not h.workers()

    # A falsy existing marker still admits no ordinary event; an explicit
    # start validates it instead of treating it as a missing marker.
    with HookHarness(tmp_path / "existing-empty-marker") as h:
        h.boot()
        h.marker().write_text("{}")
        before = len(h.records)
        h.event("PreToolUse", tool_use_id="empty-marker", tool_name="Read")
        assert len(h.records) == before
        h.event("SessionStart", source="resume")
        assert len(h.records) == before and (h.marker().parent / "disabled.json").exists()
        assert not h.workers()


USAGE = HOOK.with_name("hud_usage.py")


class UsageHarness(HookHarness):
    """Official-docs-derived SYNTHETIC statusline JSON, not owner recordings."""

    def __init__(self, home):
        super().__init__(home)
        self.server.daemon_threads = False  # server_close also awaits owned HTTP handlers.
        # Adopt this fixture's detached children so cancellation tests can
        # collect actual exit status instead of leaving orphans to PID 1.
        import ctypes
        self.libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        assert self.libc.prctl(37, ctypes.byref(previous), 0, 0, 0) == 0
        self.previous_subreaper = previous.value
        assert self.libc.prctl(36, 1, 0, 0, 0) == 0
        (home / ".config/tze-hud").chmod(0o700)
        self.env["TZ"] = "UTC"
        self.reset = int(time.time()) + 3600
        self.process_results = []
        self.owned_children = []
        self.registration_errors = []
        self.registration_stop = threading.Event()
        self.sleep_registered = threading.Event()
        self.registration_thread = None
        self.worker_script = USAGE
        self.hold_timeout = 5

    def track_wrapper_children(self):
        # Pin each detached helper and foreground sleep before real exec. The
        # private handshake closes before exec, so it cannot keep wrapper IO
        # open or become part of the helper's HTTP/cancellation lifetime.
        address = "\0hud-usage-test-" + uuid.uuid4().hex
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(address)
        listener.listen()
        listener.settimeout(0.1)
        self.registration_listener = listener

        def register():
            while not self.registration_stop.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                try:
                    with connection:
                        connection.settimeout(1)
                        pid, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                        assert uid == os.getuid() and os.readlink(f"/proc/{pid}/cwd") == self.private_cwd
                        role = connection.recv(1)
                        assert role in (b"h", b"s")
                        # setsid must exec in place, rather than leave a new
                        # unregistered child behind an intermediary process.
                        assert role != b"h" or os.getpgid(pid) != pid
                        with self.guard:
                            self.owned_children.append((pid, os.pidfd_open(pid), role))
                        connection.sendall(b"+")
                        if role == b"s":
                            self.sleep_registered.set()
                except Exception as error:
                    self.registration_errors.append(repr(error))

        self.registration_thread = threading.Thread(target=register, daemon=True)
        self.registration_thread.start()
        commands = self.home / "bin"
        commands.mkdir()
        for name, role in (("setsid", b"h"), ("sleep", b"s")):
            executable = shutil.which(name, path=self.env["PATH"])
            assert executable
            shim = commands / name
            shim.write_text(f"#!{sys.executable}\nimport os, socket, sys\n"
                            f"with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:\n"
                            f"    channel.connect({address!r})\n"
                            f"    channel.sendall({role!r})\n"
                            "    assert channel.recv(1) == b'+'\n"
                            f"os.execv({executable!r}, [{executable!r}, *sys.argv[1:]])\n")
            shim.chmod(0o755)
        self.env["PATH"] = str(commands) + os.pathsep + self.env["PATH"]

    def __exit__(self, *args):
        try:
            super().__exit__(*args)
        finally:
            try:
                self.reap_detached()
            finally:
                # Also close the HTTP fixture when its completion assertion
                # failed; restoring subreaper state must not depend on a PASS.
                self.release.set()
                self.server.shutdown()
                self.server.server_close()
                self.thread.join(2)
                if self.registration_thread is not None:
                    self.registration_stop.set()
                    self.registration_thread.join(2)
                    self.registration_listener.close()
                try:
                    # Failure cleanup touches only the pinned fixture children.
                    for pid, descriptor, _ in self.owned_children:
                        if not select.select([descriptor], [], [], 0)[0]:
                            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                        try:
                            os.waitid(os.P_PIDFD, descriptor, os.WEXITED)
                        except ChildProcessError:
                            pass
                        os.close(descriptor)
                finally:
                    assert self.libc.prctl(36, self.previous_subreaper, 0, 0, 0) == 0
                    receipt_root = os.environ.get("TZE_HUD_USAGE_TEST_RECEIPTS")
                    if receipt_root:
                        receipt = Path(receipt_root) / (self.home.name + ".json")
                        receipt.parent.mkdir(parents=True, exist_ok=True)
                        receipt.write_text(json.dumps({"provenance": "docs-derived synthetic loopback fixture",
                                                       "requests": self.records, "committed": self.committed,
                                                       "processes": self.process_results}, indent=2))

    def wait_idle(self):
        deadline = time.monotonic() + 3
        super().wait_idle()
        self.reap_detached(deadline)

    def reap_detached(self, deadline=None):
        deadline = time.monotonic() + 3 if deadline is None else deadline
        assert not self.registration_errors, self.registration_errors
        with self.guard:
            children = list(self.owned_children)
        for pid, descriptor, role in children:
            assert select.select([descriptor], [], [], max(0, deadline - time.monotonic()))[0], (
                f"owned fixture child {pid} remained live after fixture wait")
            try:
                status = os.waitid(os.P_PIDFD, descriptor, os.WEXITED)
            except ChildProcessError:
                # Bash can reap its foreground sleep before it is cancelled.
                assert role == b"s", f"detached helper {pid} lost its owned exit status"
                self.process_results.append({"owned_pid": pid, "reaped_by_wrapper": True})
            else:
                assert status.si_pid == pid
                returncode = status.si_status if status.si_code == os.CLD_EXITED else -status.si_status
                self.process_results.append({"detached_pid": pid, "role": role.decode(),
                                             "returncode": returncode})
            with self.guard:
                self.owned_children.remove((pid, descriptor, role))
            os.close(descriptor)

    def payload(self, pct=41.2, reset=None):
        return {"rate_limits": {"five_hour": {"used_percentage": pct, "resets_at": reset or self.reset}},
                "context_window": {"used_percentage": 99}, "private_unknown": "discard-this-synthetic-text"}

    def launch(self, payload):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        p = subprocess.Popen([sys.executable, str(USAGE)], cwd=self.home, env=self.env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        p.started = time.monotonic()
        p.stdin.write(raw)
        p.stdin.close()
        p.stdin = None
        return p

    def finish(self, process, expected=0):
        out, err = process.communicate(timeout=2)
        self.process_results.append({"returncode": process.returncode, "elapsed_s": time.monotonic() - process.started,
                                     "stdout_bytes": len(out), "stderr_bytes": len(err)})
        assert out == err == b""
        assert process.returncode == expected

    def replay(self, payload):
        self.finish(self.launch(payload))

    def state_path(self):
        paths = list((self.home / ".cache/tze-hud/claude-usage").glob("*/state.json"))
        assert len(paths) == 1
        return paths[0]

    def state(self):
        path = self.state_path()
        assert path.stat().st_size <= 2048
        assert path.stat().st_mode & 0o077 == 0
        raw = path.read_text()
        assert "discard-this" not in raw and "synthetic-hook-pair" not in raw
        return json.loads(raw)


def test_usage_metric_steps_new_window_and_unavailable(tmp_path):
    with UsageHarness(tmp_path / "usage") as h:
        h.replay(h.payload())
        first = h.publications()[0]
        assert first["params"]["level"] == pytest.approx(0.412)
        assert first == {"surface": "widget:usage", "params": {
            "level": first["params"]["level"], "label": "5h usage", "severity": "info",
            "readout": f'41% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'}}
        h.replay(h.payload(41.9))
        assert len(h.records) == 1
        h.replay(h.payload(42))
        h.replay(h.payload(42))
        h.replay(h.payload(79.9))
        h.replay(h.payload(80))
        assert [r["params"]["arguments"]["params"]["severity"] for r in h.records] == ["info"] * 3 + ["warning"]
        h.replay(h.payload(80, h.reset + 3600))
        assert len(h.records) == 5
        # Every unavailable spelling clears once, including pre-first-response,
        # noneligible accounts, malformed metrics, and unrelated-only windows.
        unavailable = [{}, {"rate_limits": {}}, {"rate_limits": {"five_hour": None}},
                       {"rate_limits": {"seven_day": {"used_percentage": 20}}},
                       h.payload(10, time.time() - 1), b"{", b"[]"]
        unavailable += [h.payload(v) for v in (None, "42", True, -1, 101, float("nan"), float("inf"))]
        unavailable += [{"rate_limits": {"five_hour": {"used_percentage": 20, "resets_at": v}}}
                        for v in (None, "9999999999", True, float("nan"), 1e100)]
        for value in unavailable:
            h.replay(value)
        assert len(h.records) == 6
        assert h.records[-1]["params"] == {"name": "hud_clear", "arguments": {"surface": "widget:usage"}}
        assert h.state()["ack"]["key"] == [0, -1]
        h.env["HUD_USAGE_WIDGET"] = "other-usage"
        h.replay(h.payload(0))
        h.replay(h.payload(100))
        assert h.publications()[-1]["surface"] == "widget:other-usage"
        assert h.publications()[-1]["params"]["level"] == 1
        h.tool_reply = tool_response({"code": "NOT_HELD"}, is_error=True)
        h.replay({})
        assert h.records[-1]["params"]["name"] == "hud_clear"
        count = len(h.records)
        h.replay({})
        assert len(h.records) == count


def test_usage_concurrent_claim_and_obsolete_ack_preserve_desire(tmp_path):
    import fcntl
    with UsageHarness(tmp_path / "concurrent") as h:
        h.block_text = f'41% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
        first = h.launch(h.payload())
        assert h.received.wait(1)
        others = [h.launch(h.payload()) for _ in range(5)]
        for process in others:
            h.finish(process)
        h.replay(h.payload(42))
        h.replay(h.payload(43))
        # The network is held; actual helper writers still acquired the short
        # state lock and recorded the newest input without making another send.
        assert len(h.records) == 1
        assert h.state()["desired"]["pct"] == 43
        # A real writer can still hold state.lock when the parsed ACK returns.
        # Keep that overlap while the delivery owner waits to persist success.
        with open(h.state_path().parent / "state.lock", "r+") as locked:
            fcntl.flock(locked, fcntl.LOCK_EX)
            h.release.set()
            assert h.ack_sent.wait(0.2)
            with pytest.raises(subprocess.TimeoutExpired):
                first.wait(timeout=0.05)
            assert len(h.records) == 1
        h.finish(first)
        assert [r["params"]["arguments"]["params"]["readout"].split("%")[0] for r in h.records] == ["41", "43"]
        assert h.state()["desired"]["pct"] == 43 and h.state()["ack"] == {"generation": h.state()["generation"], "key": [h.reset, 43]}
        h.replay(h.payload(43.9))
        assert len(h.records) == 2
        h.replay(h.payload(44))
        assert len(h.records) == 3  # A distinct genuine transition gets its own attempt.

        # Waiting for local ACK persistence shares the original process budget;
        # an unresponsive local lock must not turn into an unbounded wait/send.
        h.block_text = f'45% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
        h.received.clear()
        h.release.clear()
        h.ack_sent.clear()
        blocked = h.launch(h.payload(45))
        assert h.received.wait(1)
        with open(h.state_path().parent / "state.lock", "r+") as locked:
            fcntl.flock(locked, fcntl.LOCK_EX)
            h.release.set()
            assert h.ack_sent.wait(0.2)
            h.finish(blocked, -signal.SIGALRM)
        assert time.monotonic() - blocked.started < 1.5
        assert len(h.records) == 4
        assert h.state()["ack"]["key"] == [h.reset, 44]
        assert h.state()["attempted"] == {"generation": h.state()["generation"], "key": [h.reset, 45]}
        h.replay(h.payload(45))
        assert len(h.records) == 4  # Parsed but unpersisted success remains uncertain.

        h.block_text = f'46% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
        h.release.clear()
        h.received.clear()
        pending = h.launch(h.payload(46))
        assert h.received.wait(1)
        h.replay(h.payload(47))
        admitted = h.state()["generation"]
        assert len(h.records) == 5 and h.state()["desired"]["pct"] == 47
        h.finish(pending, -signal.SIGALRM)
        h.replay(h.payload(47))  # Same observation claims the distinct pending generation.
        assert len(h.records) == 6
        assert h.state()["ack"] == {"generation": admitted, "key": [h.reset, 47]}
        h.release.set()

        h.drop_ack = True
        h.replay({})
        assert h.state()["ack"]["generation"] == admitted
        h.drop_ack = False
        h.replay(h.payload(47))  # Historical ACK equality cannot suppress a new transition.
        assert len(h.records) == 8
        assert h.state()["ack"] == {"generation": admitted + 2, "key": [h.reset, 47]}


@pytest.mark.parametrize("failure", ["ack-loss", "late-commit", "crash", "trickle", "oversized-ack"])
def test_usage_uncertain_generation_allows_distinct_transition(tmp_path, failure):
    with UsageHarness(tmp_path / failure) as h:
        if failure == "ack-loss":
            h.drop_ack = True
        elif failure == "trickle":
            h.trickle = True
        elif failure == "oversized-ack":
            h.tool_reply = tool_response({"ok": True, "padding": "x" * 5000})
        else:
            h.block_text = f'41% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
            h.hold_before_commit = True
        p = h.launch(h.payload())
        if failure in ("late-commit", "crash", "trickle"):
            assert h.received.wait(1)
            assert bool(h.committed) == (failure == "trickle")
        if failure == "crash":
            p.kill()  # Our known fixture child; its descriptor dies with it.
            h.finish(p, -signal.SIGKILL)
        else:
            h.finish(p, -signal.SIGALRM if failure in ("late-commit", "trickle") else 0)
        assert time.monotonic() - p.started < 1.5
        uncertain = h.state()
        h.drop_ack = h.trickle = False
        h.tool_reply = None
        h.replay(h.payload(41.9))
        h.replay(h.payload())  # Another real process cannot retry this generation.
        assert len(h.records) == 1 and h.state() == uncertain
        for payload in (h.payload(42), {}):
            h.replay(payload)
            h.replay(payload)
        assert [r["params"]["name"] for r in h.records] == ["hud_publish", "hud_publish", "hud_clear"]
        assert uncertain["attempted"] == {"generation": uncertain["generation"], "key": [h.reset, 41]}
        assert uncertain["ack"] is None
        assert h.records[1]["params"]["arguments"]["params"]["level"] == 0.42
        local = h.state()
        assert local["ack"] == {"generation": local["generation"], "key": [0, -1]}
        h.release.set()
        deadline = time.monotonic() + 1
        while len(h.committed) != 3 and time.monotonic() < deadline:
            time.sleep(0.005)
        assert len(h.committed) == 3
        if failure in ("late-commit", "crash"):
            # Production helper requests 41,42,clear; the old server mutation
            # survives its client and resurrects 41 after acknowledged clear.
            assert h.committed == [h.records[1], h.records[2], h.records[0]]
        h.replay({})
        assert len(h.records) == 3 and h.state() == local  # No same-generation repair.
        h.replay(h.payload(1, h.reset + 3600))
        h.replay(h.payload(1.9, h.reset + 3600))
        assert len(h.records) == 4
        assert h.state()["ack"]["key"] == [h.reset + 3600, 1]

        if failure == "late-commit":
            # The same unfenced seam also lets an old clear remove an ACKed
            # new window at the same whole percentage. Release is not a fence.
            h.block_clear = True
            h.release.clear()
            h.received.clear()
            old_clear = h.launch({})
            assert h.received.wait(1)
            h.finish(old_clear, -signal.SIGALRM)
            h.block_clear = False
            h.replay(h.payload(1, h.reset + 7200))
            newer = h.state()
            assert newer["ack"] == {"generation": newer["generation"], "key": [h.reset + 7200, 1]}
            assert h.committed[-1] == h.records[-1]
            h.release.set()
            deadline = time.monotonic() + 1
            while len(h.committed) != len(h.records) and time.monotonic() < deadline:
                time.sleep(0.005)
            assert h.committed[-2:] == [h.records[-1], h.records[-2]]
            assert h.committed[-1]["params"]["name"] == "hud_clear"
            h.replay(h.payload(1, h.reset + 7200))
            assert len(h.records) == 6 and h.state() == newer
            h.replay(h.payload(2, h.reset + 7200))
            assert len(h.records) == 7


def test_usage_wrapper_stdout_and_detached_cancellation(tmp_path):
    with UsageHarness(tmp_path / "wrapper") as h:
        h.track_wrapper_children()
        h.block_text = f'41% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
        script = h.home / "statusline.sh"
        # This is the documented one-line addition. Existing stdout is emitted
        # before deliberately holding the statusline parent for cancellation.
        addition = f'(setsid "{sys.executable}" "{USAGE}" <<<"$input" >/dev/null 2>&1 & ) 2>/dev/null'
        script.write_text('#!/bin/bash\ninput=$(cat)\n' + addition + '\nprintf "\\033[32mexisting Ω\\033[0m\\n"\nsleep 3\n')
        p = subprocess.Popen(["bash", str(script)], cwd=h.home, env=h.env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        started = time.monotonic()
        p.stdin.write(json.dumps(h.payload()).encode())
        p.stdin.close()
        p.stdin = None
        assert p.stdout.readline() == "\033[32mexisting Ω\033[0m\n".encode()
        assert time.monotonic() - started < 0.5
        assert h.received.wait(1)
        assert h.sleep_registered.wait(1)
        os.killpg(p.pid, signal.SIGTERM)
        out, err = p.communicate(timeout=1)
        assert out == err == b""
        h.release.set()
        deadline = time.monotonic() + 0.8
        while h.state()["ack"] is None and time.monotonic() < deadline:
            time.sleep(0.005)
        assert h.state()["ack"]["key"] == [h.reset, 41]
        assert time.monotonic() - started < 1
        h.wait_idle()
        # Another caller's live child is not part of this fixture's completion.
        unrelated = subprocess.Popen(
            [sys.executable, "-c", "import sys; print('ready', flush=True); sys.stdin.buffer.read()"],
            cwd=tmp_path, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        assert unrelated.stdout.readline() == b"ready\n"
        h.process_results.append({"unrelated_probe_pid": unrelated.pid, "state": "live"})
        try:
            h.reap_detached()
            assert unrelated.poll() is None, "fixture must not wait for or reap another caller's child"
        finally:
            assert unrelated.communicate(timeout=1) == (b"", b"")
            assert unrelated.returncode == 0
            h.process_results.append({"unrelated_probe_pid": unrelated.pid, "returncode": unrelated.returncode})
        assert any(r.get("detached_pid") and r["returncode"] == 0 for r in h.process_results)

        h.hold_before_commit = True
        h.block_text = f'42% · resets {time.strftime("%H:%M", time.gmtime(h.reset))}'
        h.received.clear()
        h.release.clear()
        h.sleep_registered.clear()
        p = subprocess.Popen(["bash", str(script)], cwd=h.home, env=h.env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        started = time.monotonic()
        p.stdin.write(json.dumps(h.payload(42)).encode())
        p.stdin.close()
        p.stdin = None
        assert p.stdout.readline() == "\033[32mexisting Ω\033[0m\n".encode()
        assert time.monotonic() - started < 0.5 and h.received.wait(1)
        assert h.sleep_registered.wait(1)
        os.killpg(p.pid, signal.SIGTERM)
        assert p.communicate(timeout=1) == (b"", b"")
        h.wait_idle()  # Helper survives parent cancellation, then hits its original timer.
        h.reap_detached()
        assert any(r.get("detached_pid") and r["returncode"] == -signal.SIGALRM for r in h.process_results)
        assert time.monotonic() - started < 1.5
        before = len(h.records)
        h.replay(h.payload(42))
        assert len(h.records) == before
        h.replay(h.payload(43))
        assert len(h.records) == before + 1 and h.state()["ack"]["key"] == [h.reset, 43]
        h.release.set()
        # Each actual wrapper must return its existing output immediately even
        # when the detached helper cannot publish. No shell wait is installed.
        script.write_text(script.read_text().replace("sleep 3\n", ""))
        state_path = h.state_path()
        key = h.home / ".config/tze-hud/127.0.0.1.psk"
        for failure in ("invalid-widget", "missing-pair", "unsafe-state", "lock", "down"):
            if failure == "invalid-widget":
                h.env["HUD_USAGE_WIDGET"] = "../invalid"
            elif failure == "missing-pair":
                key.rename(key.with_suffix(".saved"))
            elif failure == "unsafe-state":
                state_path.chmod(0o644)
            elif failure == "down":
                h.server.shutdown()
                h.server.server_close()
            import fcntl
            with open(state_path.parent / "state.lock", "r+") as locked:
                if failure == "lock":
                    fcntl.flock(locked, fcntl.LOCK_EX)
                started = time.monotonic()
                result = subprocess.run(["bash", str(script)], cwd=h.home, env=h.env,
                                        input=json.dumps(h.payload(42)).encode(), capture_output=True, timeout=1)
                assert result.returncode == 0 and result.stderr == b""
                assert result.stdout == "\033[32mexisting Ω\033[0m\n".encode()
                elapsed = time.monotonic() - started
                assert elapsed < 0.5
                h.process_results.append({"wrapper_failure": failure, "elapsed_s": elapsed,
                                          "stdout_bytes": len(result.stdout), "stderr_bytes": len(result.stderr)})
                # Await the detached helper's fixed budget while retaining the
                # fixture fault; there is no timer/retry in production code.
                time.sleep(1.05)
            h.env.pop("HUD_USAGE_WIDGET", None)
            if failure == "missing-pair":
                key.with_suffix(".saved").rename(key)
            state_path.chmod(0o600)


def test_usage_failures_are_silent_private_bounded_and_event_driven(tmp_path):
    import fcntl
    with UsageHarness(tmp_path / "failures") as h:
        h.env["HUD_USAGE_WIDGET"] = "../unsafe"
        h.replay(h.payload())
        assert not h.records
        h.env.pop("HUD_USAGE_WIDGET")
        key = h.home / ".config/tze-hud/127.0.0.1.psk"
        key.rename(key.with_suffix(".saved"))
        h.replay(h.payload())
        assert not h.records
        key.with_suffix(".saved").rename(key)
        h.replay(b"x" * 65537)
        assert not h.records
        h.replay(h.payload())
        path = h.state_path()
        for lock_name in ("state.lock", "delivery.lock"):
            with open(path.parent / lock_name, "r+") as locked:
                fcntl.flock(locked, fcntl.LOCK_EX)
                started = time.monotonic()
                h.replay(h.payload(42))
                assert time.monotonic() - started < 0.4
        assert len(h.records) == 1
        # A crash may interrupt staging before rename. Its one bounded private
        # inode is reused; repeated input must not accumulate orphan files.
        pending = path.parent / "state.pending"
        pending.write_text('{"interrupted":')
        pending.chmod(0o600)
        h.replay(h.payload(42))  # A real later invocation handles saved desire.
        assert len(h.records) == 2
        assert not pending.exists()
        assert {p.name for p in path.parent.iterdir()} == {"state.json", "state.lock", "delivery.lock"}
        inode = (path.parent / "state.lock").stat().st_ino
        for pct in range(43, 63):
            h.replay(h.payload(pct))
        assert h.state()["generation"] == 22
        assert set(h.state()) == {"version", "generation", "desired", "ack", "attempted"}
        assert (path.parent / "state.lock").stat().st_ino == inode
        assert {p.name for p in path.parent.iterdir()} == {"state.json", "state.lock", "delivery.lock"}
        count = len(h.records)
        # Version-1 state is a real externally persisted compatibility boundary.
        # Preserve a matching old inflight claim instead of migrating into retry.
        old = {"version": 1, "generation": 30, "desired": {"window": h.reset, "pct": 63, "level": 0.63},
               "ack": [h.reset, 62], "inflight": [h.reset, 63]}
        path.write_text(json.dumps(old))
        h.replay(h.payload(63))
        assert len(h.records) == count and json.loads(path.read_text()) == old
        h.replay(h.payload(62))  # A real return to the old ACK value is distinct.
        assert len(h.records) == count + 1
        assert h.state()["ack"] == {"generation": 31, "key": [h.reset, 62]}
        old["desired"] = {"window": h.reset, "pct": 64, "level": 0.64}
        path.write_text(json.dumps(old))
        h.replay(h.payload(64))  # A different pending desire already existed in old metadata.
        assert len(h.records) == count + 2
        exhausted = h.state()
        exhausted["generation"] = 2**53 - 1
        path.write_text(json.dumps(exhausted))
        h.replay(h.payload(65))
        assert json.loads(path.read_text()) == exhausted
        count = len(h.records)
        for raw in ("{}", "x" * 2049):
            path.write_text(raw)
            h.replay(h.payload(43))
        path.chmod(0o644)
        h.replay(h.payload(43))
        path.unlink()
        target = h.home / "untouched"
        target.write_text("private synthetic fixture")
        path.symlink_to(target)
        h.replay(h.payload(43))
        assert target.read_text() == "private synthetic fixture"
        assert len(h.records) == count
    # A genuine pre-send pairing failure did not poison a claim (above).
    # Connection refusal is conservatively uncertain; no repeated attempts.
    with UsageHarness(tmp_path / "down") as h:
        h.server.shutdown()
        h.server.server_close()
        h.replay(h.payload())
        assert h.state()["attempted"]["key"] == [h.reset, 41]
        attempted = h.state()["attempted"]
        h.replay(h.payload())
        assert h.state()["attempted"] == attempted
        h.replay(h.payload(42))
        assert h.state()["attempted"]["generation"] > attempted["generation"]
        assert not h.records
