"""The isolated hook transport uses real Linux sockets, without a HUD or PSK."""

import json
import os
import socket
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
import hud_env
import hud_hook_socket as hooks

pytestmark = pytest.mark.skipif(sys.platform != "linux", reason="Linux hook transport")

TOOL = {
    "event": "PreToolUse", "session": "fixture",
    "prompt": "e53cb2ed-891a-4c88-9a7b-c64a41f06cf2",
    "tool": "Read", "key": "cc-tool-" + "f" * 64,
}


@pytest.fixture
def runtime(monkeypatch):
    # A short pathname keeps ordinary fixtures below the AF_UNIX limit.
    with tempfile.TemporaryDirectory(prefix="hud-hook-", dir="/tmp") as directory:
        monkeypatch.setenv("XDG_RUNTIME_DIR", directory)
        yield Path(directory) / "tze-hud"


def run_sender():
    return subprocess.run([
        sys.executable, "-c",
        "import sys; sys.path.insert(0, sys.argv[1]); "
        "import json, hud_env, hud_hook_socket; "
        "hud_env.adapter_key = lambda *_: (_ for _ in ()).throw(AssertionError('PSK used')); "
        "hud_hook_socket.send_event(json.loads(sys.argv[2]))",
        str(SCRIPTS), json.dumps(TOOL),
    ], capture_output=True, timeout=2)


def assert_silent(result):
    assert (result.returncode, result.stdout, result.stderr) == (0, b"", b"")


def test_sender_uses_parent_pid_with_one_packet(runtime):
    listener = hooks.HookListener.open(os.getpid())
    assert listener is not None
    try:
        assert listener.path.parent == runtime
        assert stat.S_IMODE(listener.path.stat().st_mode) == 0o600
        assert_silent(run_sender())
        peer, _ = listener.socket.accept()
        with peer:
            assert hooks.receive_event(peer) == TOOL
            assert peer.recv(1) == b""
    finally:
        listener.close()
    assert not listener.path.exists()


def test_sender_closes_foreign_uid_without_sending(runtime, monkeypatch):
    owner = os.getpid()
    listener = hooks.HookListener.open(owner)
    assert listener is not None
    try:
        monkeypatch.setattr(hooks.os, "getppid", lambda: owner)
        monkeypatch.setattr(hooks, "peer_uid", lambda _: os.getuid() + 1)
        hooks.send_event(TOOL)
        peer, _ = listener.socket.accept()
        with peer:
            peer.settimeout(1)
            assert peer.recv(1) == b""
    finally:
        listener.close()


def test_documented_hooks_are_direct_exec():
    guide = SCRIPTS.parents[1] / "hud-projection" / "references" / "claude-code-hooks.md"
    example = guide.read_text(encoding="utf-8").split("```json\n", 1)[1].split("```", 1)[0]
    events = json.loads(example)["hooks"]
    assert set(events) == {
        "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
        "PostToolUseFailure", "Notification", "Stop", "SessionEnd",
    }
    for rules in events.values():
        for rule in rules:
            for command in rule["hooks"]:
                assert command["type"] == "command"
                assert Path(command["command"]).is_absolute()
                assert Path(command["args"][0]).is_absolute()
    notification, = events["Notification"]
    assert notification["matcher"] == "^(permission_prompt|idle_prompt|elicitation_dialog)$"
    command, = notification["hooks"]
    assert command["async"] is True
    assert command["timeout"] == 2


def test_production_hook_strips_credential_and_ignores_raw_fields(runtime):
    hook = SCRIPTS.parents[1] / "hud-projection" / "scripts" / "portal_hook.py"
    listener = hooks.HookListener.open(os.getpid())
    assert listener is not None
    try:
        raw = {"hook_event_name": "Stop", "session_id": TOOL["session"],
               "prompt_id": TOOL["prompt"], "transcript_path": "private synthetic path",
               "last_assistant_message": "token=syntheticcredentialcanary"}
        assert_silent(subprocess.run([sys.executable, str(hook)], input=json.dumps(raw).encode(),
                                    capture_output=True, timeout=2))
        peer, _ = listener.socket.accept()
        with peer:
            assert hooks.receive_event(peer) == {
                "event": "Stop", "session": TOOL["session"],
                "prompt": TOOL["prompt"], "content": None,
            }
    finally:
        listener.close()


@pytest.mark.parametrize("failure", ["missing", "refusing", "would_block"])
def test_sender_drops_unavailable_packets_silently(runtime, failure):
    if failure == "missing":
        assert_silent(run_sender())
        return
    path = hud_env.runtime_socket(os.getpid(), create=True)
    with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as server:
        server.bind(str(path))
        if failure == "refusing":
            assert_silent(run_sender())
        else:
            server.listen(0)
            with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as holding:
                holding.connect(str(path))
                assert_silent(run_sender())
            server.setblocking(False)
            accepted, _ = server.accept()
            accepted.close()
            with pytest.raises(BlockingIOError):
                server.accept()  # The hook neither waited nor queued another peer.


@pytest.mark.parametrize("packet", [
    b"{", b"\xff", b'{"event":[],"session":"fixture"}',
    json.dumps({**TOOL, "tool_input": "unexpected"}).encode(),
    b" " * (hooks.MAX_PACKET + 1),
])
def test_rejects_invalid_packet(packet):
    sender, receiver = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    with sender, receiver:
        sender.send(packet)
        assert hooks.receive_event(receiver) is None


def test_foreign_uid_is_rejected_without_reading_a_packet(monkeypatch):
    sender, receiver = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    with sender, receiver:
        monkeypatch.setattr(hooks, "peer_uid", lambda _: os.getuid() + 1)
        assert hooks.receive_event(receiver) is None


def test_connected_peer_without_data_does_not_block():
    sender, receiver = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
    with sender, receiver:
        with pytest.raises(BlockingIOError):
            hooks.receive_event(receiver)


@pytest.mark.parametrize("kind", ["symlink", "mode", "owner", "length"])
def test_listener_does_not_use_unsafe_path(runtime, kind, monkeypatch):
    if kind == "symlink":
        destination = runtime.with_name("other")
        destination.mkdir(mode=0o700)
        runtime.symlink_to(destination, target_is_directory=True)
    elif kind == "mode":
        runtime.mkdir(mode=0o750)
    elif kind == "owner":
        runtime.mkdir(mode=0o700)
        lstat = Path.lstat

        def wrong_owner(path):
            result = lstat(path)
            if path == runtime:
                values = list(result)
                values[stat.ST_UID] += 1
                return os.stat_result(values)
            return result

        monkeypatch.setattr(Path, "lstat", wrong_owner)
    else:
        padding = "é" * (108 // 2)
        long_dir = runtime.parent / padding
        long_dir.mkdir(mode=0o700)
        monkeypatch.setenv("XDG_RUNTIME_DIR", str(long_dir))
        runtime = long_dir / "tze-hud"
    assert hooks.HookListener.open(os.getpid()) is None
    assert not (runtime / f"claude-{os.getpid()}.sock").exists()


def test_close_preserves_replacement_socket(runtime):
    original = hooks.HookListener.open(os.getpid())
    replacement = hooks.HookListener.open(os.getpid())
    assert original is not None and replacement is not None
    try:
        original.close()
        assert replacement.path.stat().st_ino == replacement.inode
        assert_silent(run_sender())
        peer, _ = replacement.socket.accept()
        with peer:
            assert hooks.receive_event(peer) == TOOL
    finally:
        replacement.close()


def test_startup_sweep_removes_only_sixteen_dead_sockets(runtime, monkeypatch):
    runtime.mkdir(mode=0o700)
    dead = [runtime / f"claude-{pid}.sock" for pid in range(10001, 10021)]
    main = os.getpid()
    live = runtime / "claude-10000.sock"
    ordinary = runtime / "claude-10021.sock"
    ordinary.touch()
    for path in [*dead, live]:
        with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as server:
            server.bind(str(path))

    def check_pid(pid, signal):
        assert signal == 0
        if pid not in {main, 10000}:
            raise ProcessLookupError

    monkeypatch.setattr(os, "kill", check_pid)
    listener = hooks.HookListener.open(main)
    assert listener is not None
    try:
        assert sum(path.exists() for path in dead) == 4
        assert live.exists() and ordinary.is_file()
    finally:
        listener.close()
