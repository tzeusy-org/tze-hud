"""Unit tests for the hud-projection portal client (no network)."""

import importlib.util
import errno
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

import inspect

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


def test_poll_accepts_the_55_second_wait_within_its_http_timeout():
    server = FakeServer([tool_response({"items": [], "remaining": 0})])
    with mock.patch.object(client, "rpc", server), pytest.raises(SystemExit):
        client.main(["poll", "--wait-ms", "55000"])
    assert server.calls[0][1]["arguments"]["wait_ms"] == 55000
    timeout = inspect.signature(client.rpc).parameters["timeout"].default
    assert timeout > 55  # A full wait plus its reply fits one HTTP request.


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


USAGE = ROOT / ".claude/skills/hud-projection/scripts/hud_usage.py"


class LoopbackToolHarness:
    """Fake tool endpoint and lifecycle helpers for the usage subprocess tests."""

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

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.release.set()
        self.wait_idle()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)
        assert not self.thread.is_alive()

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
                    if str(self.worker_script).encode() in args:
                        found.append(int(entry.name))
            except (OSError, RuntimeError):
                continue
        return found

    def wait_idle(self):
        deadline = time.monotonic() + 3
        while self.workers() and time.monotonic() < deadline:
            time.sleep(0.01)
        assert not self.workers(), "owned hook supervisor/HTTP child did not terminate"

    def publications(self):
        return [r["params"]["arguments"] for r in self.records if r["params"]["name"] == "hud_publish"]


class UsageHarness(LoopbackToolHarness):
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
        self.prewarmed = []

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
        # An unused prepared child has not entered main or touched usage state.
        # Closing its private gate releases it; kill only this owned child if
        # startup itself stalls, so a failed assertion cannot leave it behind.
        for process, channel in self.prewarmed:
            channel.close()
            try:
                process.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=2)
        self.prewarmed.clear()
        try:
            super().__exit__(*args)
        finally:
            try:
                self.reap_detached()
            finally:
                try:
                    # Also close HTTP when its completion assertion failed;
                    # subreaper restoration must not depend on a PASS.
                    self.release.set()
                    self.server.shutdown()
                    self.server.server_close()
                    self.thread.join(2)
                    if self.registration_thread is not None:
                        self.registration_stop.set()
                        self.registration_thread.join(2)
                        self.registration_listener.close()
                        assert not self.registration_thread.is_alive()
                    # Failure cleanup touches only the pinned fixture children.
                    cleanup_errors = []
                    for pid, descriptor, _ in self.owned_children:
                        try:
                            if not select.select([descriptor], [], [], 0)[0]:
                                try:
                                    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                                except ProcessLookupError:
                                    pass  # It exited after the live check.
                            try:
                                os.waitid(os.P_PIDFD, descriptor, os.WEXITED)
                            except ChildProcessError:
                                pass
                        except Exception as error:
                            cleanup_errors.append(error)
                        finally:
                            try:
                                os.close(descriptor)
                            except OSError as error:
                                cleanup_errors.append(error)
                    if cleanup_errors:
                        raise ExceptionGroup("owned fixture child cleanup failed", cleanup_errors)
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

    def _spawn(self, argv, **kwargs):
        return subprocess.Popen(argv, cwd=self.home, env=self.env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs)

    def prewarm(self, count):
        # Prepare imports before the held-owner interleaving, rather than
        # spending its lease on seven more interpreters. Each release enters
        # the unchanged main with cached imports and the real one-second timer;
        # stdin, state locks, HTTP and ACK persistence remain inside that budget.
        # Cold-start and cancellation parents continue to use normal launches.
        # The gate is closed before main and cannot extend the delivery lease.
        prepare = ("import runpy,socket,sys\nfrom pathlib import Path\n"
                   "sys.path.insert(0, str(Path(sys.argv[1]).resolve().parents[2] / 'user-test' / 'scripts'))\n"
                   "import hud_env,publish_widget_batch\n"
                   "entry = runpy.run_path(sys.argv[1])['main']\n"
                   "with socket.socket(fileno=int(sys.argv[2])) as gate:\n"
                   "    gate.sendall(b'+')\n"
                   "    start = gate.recv(1) == b'+'\n"
                   "sys.argv = [sys.argv[1]]\n"
                   "if start: entry()\n")
        for _ in range(count):
            channel, child_channel = socket.socketpair()
            try:
                process = self._spawn([sys.executable, "-c", prepare, str(USAGE),
                                       str(child_channel.fileno())],
                                      pass_fds=(child_channel.fileno(),))
            except BaseException:
                channel.close()
                raise
            finally:
                child_channel.close()
            self.prewarmed.append((process, channel))
            channel.settimeout(2)
            assert channel.recv(1) == b"+", "owned usage child did not prepare"

    def launch(self, payload):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        if self.prewarmed:
            p, channel = self.prewarmed[0]
        else:
            p, channel = self._spawn([sys.executable, str(USAGE)]), None
        p.started = time.monotonic()
        p.stdin.write(raw)
        p.stdin.close()
        p.stdin = None
        if channel is not None:
            with channel:
                channel.sendall(b"+")
            self.prewarmed.pop(0)
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
        # Scheduling seven more cold interpreters after the first send could
        # exhaust its real deadline before this test reached ACK persistence.
        # Keep that deadline real, but prepare this first burst before starting it.
        h.prewarm(8)
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

    # Exercise actual __exit__ fallback: an owned process can finish between
    # the readiness check and signal. Its disappearance must not skip the next
    # owned child or replace the completion failure that initiated cleanup.
    children, descriptors, gone, statuses = [], [], [], []
    completion_failure = AssertionError("controlled owned completion failure")
    real_select, real_signal, real_waitid = select.select, signal.pidfd_send_signal, os.waitid

    def exit_after_live_check(readers, writers, errors, timeout):
        result = real_select(readers, writers, errors, timeout)
        if readers == descriptors[:1] and not result[0] and children[0].poll() is None:
            assert children[0].communicate(timeout=1) == (b"", b"")
            assert children[0].returncode == 0
        return result

    def observe_signal(descriptor, sig):
        try:
            return real_signal(descriptor, sig)
        except ProcessLookupError as error:
            gone.append((descriptor, error.errno))
            cleanup.process_results.append({"cleanup_race_pid": children[0].pid, "signal_errno": error.errno})
            raise

    def observe_status(kind, descriptor, options):
        status = real_waitid(kind, descriptor, options)
        statuses.append(status)
        if status.si_pid == children[1].pid:
            children[1].returncode = status.si_status if status.si_code == os.CLD_EXITED else -status.si_status
            cleanup.process_results.append({"cleanup_remaining_pid": status.si_pid,
                                            "returncode": children[1].returncode})
        return status

    try:
        with mock.patch.object(UsageHarness, "reap_detached", side_effect=completion_failure), \
                mock.patch.object(select, "select", side_effect=exit_after_live_check), \
                mock.patch.object(signal, "pidfd_send_signal", side_effect=observe_signal), \
                mock.patch.object(os, "waitid", side_effect=observe_status):
            with pytest.raises(AssertionError) as caught:
                with UsageHarness(tmp_path / "cleanup-race") as cleanup:
                    for _ in range(2):
                        child = subprocess.Popen(
                            [sys.executable, "-c", "import sys; print('ready', flush=True); sys.stdin.buffer.read()"],
                            cwd=cleanup.home, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                        children.append(child)
                        assert child.stdout.readline() == b"ready\n"
                        descriptor = os.pidfd_open(child.pid)
                        descriptors.append(descriptor)
                        cleanup.owned_children.append((child.pid, descriptor, b"h"))
            assert caught.value is completion_failure
        assert gone == [(descriptors[0], errno.ESRCH)]
        assert [(status.si_pid, status.si_code, status.si_status) for status in statuses] == [
            (children[1].pid, os.CLD_KILLED, signal.SIGKILL)]
        for descriptor in descriptors:
            with pytest.raises(OSError) as closed:
                os.fstat(descriptor)
            assert closed.value.errno == errno.EBADF
    finally:
        # A failing old-fixture RED must also leave no probe children/FDs.
        for child in children:
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=1)
        for descriptor in descriptors:
            try:
                os.close(descriptor)
            except OSError as error:
                assert error.errno == errno.EBADF


def test_usage_failures_are_silent_private_bounded_and_event_driven(tmp_path):
    import fcntl
    with UsageHarness(tmp_path / "imports") as h:
        # Exercise real application imports under the original process timer.
        # A timeout before transport must leave the same input eligible later.
        imports = h.home / "imports"
        imports.mkdir()
        marker = imports / "entered"
        original_pythonpath = h.env.get("PYTHONPATH")
        for index, module in enumerate(("hud_env", "publish_widget_batch")):
            marker.unlink(missing_ok=True)
            (imports / "sitecustomize.py").write_text(
                "import sys, time\n"
                "class SlowImport:\n"
                "    def find_spec(self, fullname, path=None, target=None):\n"
                f"        if fullname == {module!r}:\n"
                f"            with open({str(marker)!r}, 'w') as out:\n"
                "                out.write(fullname)\n"
                "            time.sleep(1.5)\n"
                "sys.meta_path.insert(0, SlowImport())\n")
            h.env["PYTHONPATH"] = str(imports) + (
                os.pathsep + original_pythonpath if original_pythonpath else "")
            payload = h.payload(41 + index)
            process = h.launch(payload)
            try:
                h.finish(process, expected=-signal.SIGALRM)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate(timeout=2)
                if original_pythonpath is None:
                    h.env.pop("PYTHONPATH")
                else:
                    h.env["PYTHONPATH"] = original_pythonpath
            assert marker.read_text() == module
            assert len(h.records) == index
            h.replay(payload)
            assert len(h.records) == index + 1
            assert h.state()["ack"]["key"] == [h.reset, 41 + index]
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
