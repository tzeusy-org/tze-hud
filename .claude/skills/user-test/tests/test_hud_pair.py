"""hud_pair.py against a fake HUD: file mode, rotation, and PSK never printed."""

import json
import os
import stat
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
CODE = "482913"


class FakeHud(BaseHTTPRequestHandler):
    issued = 0  # class-level: each successful pair mints a new key
    last_psk = ""

    def do_GET(self):
        if self.headers.get("Authorization") != f"Bearer {type(self).last_psk}":
            return self.reply(401, {"code": "UNAUTHORIZED"})
        self.reply(200, {"path": self.path})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
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
    server = HTTPServer(("127.0.0.1", 0), FakeHud)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"127.0.0.1:{server.server_port}", tmp_path
    server.shutdown()


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


def test_repair_rotates_the_key(hud):
    host, home = hud
    pair(host, home)
    first = psk_file(home).read_text()
    assert pair(host, home).returncode == 0
    assert psk_file(home).read_text() != first
    assert stat.S_IMODE(psk_file(home).stat().st_mode) == 0o600


def test_wrong_code_reports_the_code_and_writes_nothing(hud):
    host, home = hud
    result = pair(host, home, "000000")
    assert result.returncode != 0
    assert "PAIR_CODE_INVALID" in result.stderr
    assert not psk_file(home).exists()


def test_write_failure_has_no_traceback_and_no_psk(hud):
    host, home = hud
    psk_file(home).mkdir(parents=True)  # a directory where the file must go
    (psk_file(home) / "x").write_text("x")
    result = pair(host, home)
    assert result.returncode == 1
    assert "hud_pair: IsADirectoryError talking to the HUD or writing the PSK file" in result.stderr
    assert "Traceback" not in result.stderr
    assert "ab" * 31 not in result.stdout + result.stderr


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
