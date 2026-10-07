"""hud_pair.py against a fake HUD: file mode, rotation, and PSK never printed."""

import json
import hashlib
import os
import selectors
import signal
import stat
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
CODE = "482913"


class FakeHud(BaseHTTPRequestHandler):
    issued = 0  # class-level: each successful pair mints a new key
    last_psk = ""
    requests = []
    modes = {}
    entered = {}
    release = threading.Event()
    condition = threading.Condition()

    def do_GET(self):
        if self.headers.get("Authorization") != f"Bearer {type(self).last_psk}":
            return self.reply(401, {"code": "UNAUTHORIZED"})
        self.reply(200, {"path": self.path})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/mcp":
            authorized = self.headers.get("Authorization") == f"Bearer {type(self).last_psk}"
            request_id = body.get("id")
            with self.condition:
                self.requests.append({"body": body, "authorized": authorized,
                                      "protocol": self.headers.get("MCP-Protocol-Version")})
                self.entered.setdefault(request_id, threading.Event()).set()
                self.condition.notify_all()
            if not authorized:
                return self.reply(401, {"code": "UNAUTHORIZED"})
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
            if method == "initialize":
                result = {"protocolVersion": "2025-06-18", "serverInfo": {"name": "fake-upstream", "version": "fixture"},
                          "capabilities": {"tools": {"listChanged": False}}, "instructions": "upstream sentinel"}
            elif method == "tools/list":
                result = {"tools": [{"name": name, "description": "upstream sentinel", "inputSchema": {"type": "object"}}
                                    for name in ["hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"]]}
            elif method == "tools/call":
                result = {"isError": True, "content": [{"type": "text", "text": '{"code":"FIXTURE","hint":"雪"}'}]}
            else:
                result = {}
            if mode == "large":
                result = {"content": "z" * 700000}
            try:
                return self.reply(200, {"jsonrpc": "2.0", "id": request_id, "result": result})
            except (BrokenPipeError, ConnectionResetError):
                return
        if self.path != "/pair" or body.get("code") != CODE:
            return self.reply(403, {"code": "PAIR_CODE_INVALID", "hint": "wrong code"})
        type(self).issued += 1
        psk = f"{type(self).issued:02d}" + "ab" * 31
        type(self).last_psk = psk
        self.reply(200, {"agent": body["agent"], "psk": psk, "mcp": "http://h:9090/mcp", "grpc": "h:50051"})

    def reply(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *args):
        pass


@pytest.fixture
def hud(tmp_path):
    FakeHud.issued = 0
    FakeHud.requests, FakeHud.modes, FakeHud.entered = [], {}, {}
    FakeHud.release = threading.Event()
    server = ThreadingHTTPServer(("127.0.0.1", 0), FakeHud)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"127.0.0.1:{server.server_port}", tmp_path
    FakeHud.release.set()
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)


def endpoint_file(home):
    return psk_file(home).with_suffix(".endpoint.json")


def wait_request(request_id, timeout=3):
    with FakeHud.condition:
        assert FakeHud.condition.wait_for(lambda: request_id in FakeHud.entered, timeout), f"no HTTP request {request_id}"


def owned_children(parent):
    try:
        ids = Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
    except FileNotFoundError:
        ids = []
    result = {}
    for pid in ids:
        try:
            text = Path(f"/proc/{pid}/stat").read_text()
            values = text[text.rindex(")") + 2:].split()
            result[int(pid)] = {"start": values[19], "state": values[0], "parent": int(values[1])}
        except FileNotFoundError:
            pass
    return result


def child_live(pid, identity):
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    values = text[text.rindex(")") + 2:].split()
    return values[19] == identity["start"] and values[0] != "Z"


class AdapterProcess:
    """One real shipped entry or private same-loop injection, isolated fake HOME."""
    def __init__(self, home, *, host=None, config=".mcp.json", short=None, stall=None):
        project = SCRIPTS.parents[3]
        self.home = home
        self.env = {**os.environ, "HOME": str(home), "CLAUDE_PROJECT_DIR": str(project)}
        self.env.pop("HUD_HOST", None)
        if host is not None:
            self.env["HUD_HOST"] = host
        entry = json.loads((project / config).read_text())["mcpServers"]["tze-hud"]
        command = [entry["command"], *entry["args"]]
        if short is not None or stall is not None:
            inject = {k: v for k, v in {"REQUEST_SECONDS": short, "OUTPUT_STALL_SECONDS": stall}.items() if v is not None}
            code = ("import runpy,sys; m=runpy.run_path(sys.argv[1],run_name='adapter_fixture'); "
                    f"m['main'].__globals__.update({inject!r}); sys.argv=sys.argv[:1]; raise SystemExit(m['main']())")
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

    def send(self, method, request_id=None, params=None):
        body = {"jsonrpc": "2.0", "method": method}
        if request_id is not None:
            body["id"] = request_id
        if params is not None:
            body["params"] = params
        data = memoryview(json.dumps(body, ensure_ascii=False).encode() + b"\n")
        end = time.monotonic() + 3
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdin, selectors.EVENT_WRITE)
            while data:
                assert selector.select(max(0, end - time.monotonic())), "stdin stalled"
                data = data[os.write(self.process.stdin.fileno(), data):]
        self.sample()

    def receive(self, timeout=3):
        end = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            self.sample()
            assert self.selector.select(max(0, end - time.monotonic())), "no protocol response"
            data = os.read(self.process.stdout.fileno(), 16384)
            assert data, "adapter stdout ended"
            self.buffer.extend(data)
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        return json.loads(line)

    def ready(self):
        self.send("initialize", 1, {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fixture", "version": "1"}})
        result = self.receive()
        assert result["result"]["serverInfo"]["name"] == "fake-upstream"
        self.send("notifications/initialized")
        self.send("ping", 2)
        assert self.receive() == {"jsonrpc": "2.0", "id": 2, "result": {}}
        return result

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
    assert record == {"schema": 1, "mcp_url": f"http://{host}/mcp", "psk_sha256": hashlib.sha256(psk.encode()).hexdigest()}
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
                    {"schema": 1, "mcp_url": "http://${HUD_HOST}:9090/mcp", "psk_sha256": "0" * 64}]:
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


def test_admin_sends_the_paired_psk_as_bearer(hud):
    host, home = hud
    pair(host, home)
    env = {**os.environ, "HOME": str(home), "HUD_HOST": host}
    result = subprocess.run(
        [sys.executable, str(SCRIPTS / "hud_admin.py"), "logs", "--tail", "5"],
        capture_output=True, text=True, env=env,
    )
    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout) == {"path": "/admin/logs?tail=5"}


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


def test_mcp_headers_needs_a_bare_host(hud):
    host, home = hud
    pair(host, home)
    env = {**os.environ, "HOME": str(home), "HUD_HOST": "127.0.0.1"}
    run = lambda e: subprocess.run(  # noqa: E731
        [sys.executable, str(SCRIPTS / "hud_env.py"), "mcp-headers"], capture_output=True, text=True, env=e
    )
    assert json.loads(run(env).stdout)["Authorization"].startswith("Bearer ")
    bad = run({**env, "HUD_HOST": "127.0.0.1:9090"})
    assert bad.returncode == 1 and "bare host" in bad.stderr and not bad.stdout

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
    with AdapterProcess(home, short=0.3) as adapter:
        adapter.ready()
        adapter.send("ping", 60)
        wait_request(60)
        adapter.sample()
        result = adapter.receive(timeout=3)
        assert result["id"] == 60 and result["error"]["code"] == -32603
    assert sum(r["body"].get("id") == 60 for r in FakeHud.requests) == 1
    FakeHud.release.set()
    FakeHud.release = threading.Event()
    FakeHud.modes[61] = "trickle"
    with AdapterProcess(home) as adapter:  # Exact shipped/default60s path, not private shortened fixture.
        adapter.ready()
        started = time.monotonic()
        adapter.send("ping", 61)
        wait_request(61)
        adapter.sample()
        timed_out = adapter.receive(timeout=65)
        elapsed = time.monotonic() - started
        assert timed_out["id"] == 61 and timed_out["error"]["code"] == -32603
        assert 59 <= elapsed < 64
    assert sum(r["body"].get("id") == 61 for r in FakeHud.requests) == 1
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
