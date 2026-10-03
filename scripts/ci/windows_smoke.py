#!/usr/bin/env python3
"""Boot tze_hud.exe and drive the MCP lifecycle against it.

Runs on the Windows CI runner (software rendering via WARP) after the release
build. Proves the shipped binary boots the overlay with the production config
and that each lifecycle stage answers over MCP: discover, publish to a zone,
attach/poll/detach a portal, and a structured error.

The seeded agent also holds `admin`, so the operator endpoints
(/admin/status, /admin/logs, /admin/screenshot) are checked too, and last
POST /admin/restart (after a `--pair` check: /pair is closed until asked, then
refuses a wrong code): the HUD relaunches itself, the new process (a different
pid, uptime reset) answers with the same PSK, and the old process exits.

The config is copied to a temp dir with a seeded agents.toml beside it holding
only the SHA-256 of a fresh random PSK, the way pairing stores agents.

Stdlib only. Exits non-zero on the first failed check and prints the HUD log.

    python scripts/ci/windows_smoke.py --exe target/release/tze_hud.exe \
        --config app/tze_hud_app/config/production.toml
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import secrets
import signal
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import zlib
from pathlib import Path

MIN_CHANGED_PIXELS = 200
TOOLS = {"hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"}


class Smoke:
    def __init__(self, url: str, psk: str) -> None:
        self.url = url
        self.psk = psk
        self.next_id = 0

    def rpc(self, method: str, params: dict | None = None) -> dict:
        self.next_id += 1
        body = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
        if params is not None:
            body["params"] = params
        req = urllib.request.Request(
            self.url,
            data=json.dumps(body).encode(),
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {self.psk}",
            },
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            reply = json.load(resp)
        if "error" in reply:
            raise AssertionError(f"{method}: JSON-RPC error {reply['error']}")
        return reply["result"]

    def request(self, method: str, path: str) -> tuple[int, str, bytes]:
        """Call an operator endpoint on the same port; return (status, content type, body)."""
        base = self.url.rsplit("/", 1)[0]
        req = urllib.request.Request(
            base + path,
            method=method,
            data=b"" if method == "POST" else None,
            headers={"Authorization": f"Bearer {self.psk}"},
        )
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return resp.status, resp.headers.get("Content-Type", ""), resp.read()
        except urllib.error.HTTPError as err:
            return err.code, err.headers.get("Content-Type", ""), err.read()

    def get_bytes(self, path: str) -> tuple[int, str, bytes]:
        """GET an operator endpoint; a non-2xx status raises."""
        status, ctype, body = self.request("GET", path)
        if status >= 300:
            raise AssertionError(f"GET {path}: {status} {body[:200]!r}")
        return status, ctype, body

    def get(self, path: str) -> tuple[int, str]:
        """GET an operator endpoint on the same port; return (status, body)."""
        status, _, body = self.get_bytes(path)
        return status, body.decode("utf-8", "replace")

    def call(self, tool: str, args: dict | None = None) -> tuple[bool, dict]:
        """Call a tool; return (is_error, parsed result text)."""
        result = self.rpc("tools/call", {"name": tool, "arguments": args or {}})
        text = result["content"][0]["text"]
        return bool(result.get("isError")), json.loads(text)

    def ok(self, tool: str, args: dict | None = None) -> dict:
        is_error, payload = self.call(tool, args)
        if is_error:
            raise AssertionError(f"{tool} {args}: {payload}")
        return payload


def wait_for_mcp(smoke: Smoke, proc: subprocess.Popen, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise AssertionError(f"tze_hud exited during startup (code {proc.returncode})")
        try:
            smoke.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})
            return
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.5)
    raise AssertionError(f"MCP not reachable at {smoke.url} after {timeout_s:.0f}s")


def run_checks(smoke: Smoke) -> None:
    names = {t["name"] for t in smoke.rpc("tools/list")["tools"]}
    assert names == TOOLS, f"tools/list: expected {sorted(TOOLS)}, got {sorted(names)}"
    print("ok  tools/list has the five hud_* tools")

    surfaces = smoke.ok("hud_surfaces")["surfaces"]
    text_zones = [s["s"] for s in surfaces if s.get("accepts") == "text"]
    assert text_zones, f"hud_surfaces lists no text zone: {surfaces}"
    print(f"ok  hud_surfaces lists {len(surfaces)} surfaces")

    widgets = sorted(s["s"] for s in surfaces if s["s"].startswith("widget:"))
    expected = ["widget:main-gauge", "widget:main-progress", "widget:main-status"]
    assert widgets == expected, f"hud_surfaces widgets: expected {expected}, got {widgets}"
    gauge = smoke.ok("hud_publish", {"surface": "widget:main-gauge", "params": {"level": 0.5, "label": "CI"}})
    assert gauge.get("ok") is True, f"hud_publish widget:main-gauge: {gauge}"
    print("ok  built-in widgets listed; hud_publish widget:main-gauge")

    zone = text_zones[0]
    published = smoke.ok("hud_publish", {"surface": zone, "content": "CI smoke", "ttl_ms": 5000})
    assert published.get("ok") is True, f"hud_publish {zone}: {published}"
    print(f"ok  hud_publish {zone}")

    portal = "portal:ci-smoke"
    smoke.ok("hud_publish", {"surface": portal, "content": "hello from CI", "display_name": "CI"})
    listed = {s["s"] for s in smoke.ok("hud_surfaces")["surfaces"]}
    assert portal in listed, f"{portal} not listed after attach: {sorted(listed)}"
    inbox = smoke.ok("hud_input", {"wait_ms": 0})
    assert inbox.get("items") == [], f"hud_input on a fresh portal: {inbox}"
    smoke.ok("hud_clear", {"surface": portal})
    print(f"ok  {portal} attach, poll, detach")

    is_error, payload = smoke.call("hud_publish", {"surface": "zone:no-such-zone", "content": "x"})
    assert is_error and payload.get("code") == "ZONE_NOT_FOUND" and payload.get("hint"), (
        f"unknown zone should be ZONE_NOT_FOUND with a hint, got {payload}"
    )
    print("ok  unknown zone -> ZONE_NOT_FOUND with hint")


def decode_png_rgba(data: bytes) -> tuple[int, int, bytes]:
    """Decode an 8-bit RGBA, non-interlaced PNG; return (width, height, pixels)."""
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos, idat, width = 8, b"", 0
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind, body = data[pos + 4 : pos + 8], data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            width, height, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            assert (depth, ctype, interlace) == (8, 6, 0), f"expected 8-bit RGBA, got {body.hex()}"
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    raw, stride = zlib.decompress(idat), width * 4
    out, prev = bytearray(), bytearray(stride)
    for y in range(height):
        row = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        ftype = raw[y * (stride + 1)]
        for i in range(stride):
            a = row[i - 4] if i >= 4 else 0
            b = prev[i]
            c = prev[i - 4] if i >= 4 else 0
            if ftype == 1:
                row[i] = (row[i] + a) & 255
            elif ftype == 2:
                row[i] = (row[i] + b) & 255
            elif ftype == 3:
                row[i] = (row[i] + (a + b) // 2) & 255
            elif ftype == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                row[i] = (row[i] + pred) & 255
        out += row
        prev = row
    return width, height, bytes(out)


def check_screenshot(smoke: Smoke) -> None:
    """The screenshot is a real PNG of the frame, and it shows a new notification.

    The overlay is fully opaque under WARP, so alpha proves nothing. Instead take
    a baseline, publish, take another, and require the two to differ in a
    meaningful number of pixels (the notification's backdrop and text).
    """

    def shot() -> tuple[int, int, bytes]:
        status, ctype, body = smoke.get_bytes("/admin/screenshot")
        assert status == 200 and ctype == "image/png", f"/admin/screenshot: {status} {ctype}"
        return decode_png_rgba(body)

    width, height, before = shot()
    if sys.platform == "win32":
        import ctypes

        ctypes.windll.user32.SetProcessDPIAware()  # physical pixels, like the swapchain
        screen = (ctypes.windll.user32.GetSystemMetrics(0), ctypes.windll.user32.GetSystemMetrics(1))
        assert (width, height) == screen, f"screenshot {width}x{height}, screen {screen}"

    zone = next(s["s"] for s in smoke.ok("hud_surfaces")["surfaces"] if s.get("accepts") == "text")
    smoke.ok("hud_publish", {"surface": zone, "content": "screenshot check", "ttl_ms": 30000})
    deadline, changed = time.monotonic() + 10, 0
    while time.monotonic() < deadline and changed < MIN_CHANGED_PIXELS:
        time.sleep(0.5)  # let the compositor render the publish
        w2, h2, after = shot()
        assert (w2, h2) == (width, height), f"screenshot size changed: {w2}x{h2}"
        changed = sum(1 for i in range(0, len(after), 4) if after[i : i + 4] != before[i : i + 4])
    assert changed >= MIN_CHANGED_PIXELS, (
        f"screenshot after publish differs from the baseline in only {changed} pixels "
        f"(need {MIN_CHANGED_PIXELS}); the notification is not in the captured frame"
    )
    print(f"ok  /admin/screenshot {width}x{height} PNG, {changed} pixels changed by the notification")


def check_admin(smoke: Smoke) -> None:
    status, body = smoke.get("/admin/status")
    info = json.loads(body)
    assert status == 200 and info["pid"] and info["sha"], f"/admin/status: {body}"
    assert "safe_mode_hotkey" in info, f"/admin/status lacks safe_mode_hotkey: {body}"
    # run_checks and the settle wait precede this, so the HUD should be idle.
    cpu = info["cpu_pct_2s"]
    assert isinstance(cpu, (int, float)) and cpu < 5, f"idle HUD cpu_pct_2s={cpu}, expected < 5"
    print(f"ok  /admin/status (cpu_pct_2s={cpu}, channel={info['channel']})")

    status, body = smoke.get("/admin/logs?tail=20")
    assert status == 200 and body.strip(), "/admin/logs?tail=20 returned no lines"
    print(f"ok  /admin/logs returned {len(body.splitlines())} lines")

    check_screenshot(smoke)


def pair_post(smoke: Smoke, code: str) -> tuple[int, bytes]:
    base = smoke.url.rsplit("/", 1)[0]
    body = json.dumps({"agent": "ci-pair", "code": code}).encode()
    req = urllib.request.Request(base + "/pair", data=body)  # no bearer: the code is the credential
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def check_pair(smoke: Smoke, exe: Path) -> None:
    """/pair is closed while agents exist; `--pair` opens it, and a wrong code is refused."""
    status, body = pair_post(smoke, "000000")
    assert status == 403 and b"PAIRING_CLOSED" in body, f"/pair before --pair: {status} {body[:200]!r}"
    done = subprocess.run([str(exe), "--pair"], capture_output=True, text=True, timeout=30)
    assert done.returncode == 0, f"--pair: exit {done.returncode} {done.stderr.strip()}"
    # The signal is asynchronous: PAIRING_CLOSED until the listener has opened pairing.
    deadline = time.monotonic() + 10
    while True:
        status, body = pair_post(smoke, "not-a-code")
        if b"PAIRING_CLOSED" not in body or time.monotonic() >= deadline:
            break
        time.sleep(0.25)
    assert status == 403 and b"PAIR_CODE_INVALID" in body, f"/pair wrong code: {status} {body[:200]!r}"
    print("ok  /pair closed until --pair, then a wrong code -> 403 PAIR_CODE_INVALID")


def kill_pid(pid: int) -> None:
    if sys.platform == "win32":
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True)
    else:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def check_restart(smoke: Smoke, proc: subprocess.Popen, timeout_s: float = 60) -> int:
    """POST /admin/restart; return the new instance's pid once it answers.

    The replacement is spawned detached by the HUD itself, so the caller must
    kill it. The old instance exits only after the new one reports its first
    frame, so there is always at least one instance serving except for the
    brief port handover.
    """
    old = json.loads(smoke.get("/admin/status")[1])

    # State-changing: POST only, and never actionable by a GET.
    status, _, _ = smoke.request("GET", "/admin/restart")
    assert status == 405, f"GET /admin/restart: expected 405, got {status}"

    status, _, body = smoke.request("POST", "/admin/restart")
    assert status == 202, f"POST /admin/restart: {status} {body[:200]!r}"
    started = time.monotonic()
    print("ok  POST /admin/restart accepted")

    info = None
    while time.monotonic() - started < timeout_s:
        try:
            status, body = smoke.get("/admin/status")
            candidate = json.loads(body)
        except (urllib.error.URLError, ConnectionError, TimeoutError, ValueError, AssertionError):
            time.sleep(1)  # between the old instance's exit and the new one's bind
            continue
        if status == 200 and candidate["pid"] != old["pid"]:
            info = candidate
            break
        time.sleep(1)
    assert info is not None, f"pid stayed {old['pid']} for {timeout_s:.0f}s after /admin/restart"
    took = time.monotonic() - started
    # A fresh process: its uptime counts from its own start, not the old one's.
    assert info["uptime_s"] <= took + 2, f"uptime_s={info['uptime_s']} after {took:.0f}s: not a fresh process"
    smoke.ok("hud_surfaces")  # the same PSK still authenticates
    print(f"ok  restarted in {took:.1f}s: pid {old['pid']} -> {info['pid']}, uptime_s={info['uptime_s']}, PSK still works")

    try:
        code = proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        raise AssertionError(f"old pid {old['pid']} still running 30s after the handover") from None
    print(f"ok  old instance exited (code {code})")
    return info["pid"]


def seed_config(config: Path, psk: str) -> Path:
    """Copy `config` to a temp dir and pair one agent for `psk` beside it."""
    config_dir = Path(tempfile.mkdtemp(prefix="tze_hud_smoke_"))
    seeded = config_dir / config.name
    shutil.copyfile(config, seeded)
    digest = hashlib.sha256(psk.encode()).hexdigest()
    (config_dir / "agents.toml").write_text(
        f'[agents.ci-smoke]\npsk_sha256 = "{digest}"\nallow = ["*", "admin"]\n', encoding="utf-8"
    )
    return seeded


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--exe", required=True, type=Path)
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--mcp-port", type=int, default=9090)
    ap.add_argument("--startup-timeout", type=float, default=90)
    ap.add_argument("--settle-s", type=float, default=3, help="seconds the HUD must stay up after checks")
    args = ap.parse_args()

    psk = secrets.token_hex(32)
    config = seed_config(args.config, psk)
    log_path = Path(tempfile.gettempdir()) / "tze_hud_smoke.log"
    cmd = [
        str(args.exe),
        "--config", str(config),
        "--window-mode", "fullscreen",
        "--mcp-port", str(args.mcp_port),
    ]
    print("launch:", " ".join(cmd))
    with open(log_path, "wb") as log:
        proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    smoke = Smoke(f"http://127.0.0.1:{args.mcp_port}/", psk)
    restarted_pid = None
    try:
        wait_for_mcp(smoke, proc, args.startup_timeout)
        print("ok  MCP initialize")
        run_checks(smoke)
        time.sleep(args.settle_s)
        check_admin(smoke)
        assert proc.poll() is None, f"tze_hud exited after the checks (code {proc.returncode})"
        print("ok  HUD still running after the checks")
        check_pair(smoke, args.exe)
        restarted_pid = check_restart(smoke, proc)
        return 0
    except AssertionError as err:
        print(f"FAIL {err}", file=sys.stderr)
        return 1
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=10)
        # The replacement is detached from this script; stop it too.
        if restarted_pid is None:
            try:
                restarted_pid = json.loads(smoke.get("/admin/status")[1])["pid"]
            except Exception:  # noqa: BLE001 - best effort cleanup
                pass
        if restarted_pid is not None and restarted_pid != proc.pid:
            kill_pid(restarted_pid)
        print(f"--- HUD log ({log_path}) ---")
        print(log_path.read_text(errors="replace")[-20_000:])
        # Durable log (shared by the old and restarted instance).
        base = os.environ.get("LOCALAPPDATA") if sys.platform == "win32" else None
        durable = (Path(base) if base else Path(tempfile.gettempdir())) / "tze_hud" / "logs" / "tze_hud.log"
        if os.environ.get("TZE_HUD_LOG_DIR"):
            durable = Path(os.environ["TZE_HUD_LOG_DIR"]) / "tze_hud.log"
        if durable.exists():
            print(f"--- durable log tail ({durable}) ---")
            print(durable.read_text(errors="replace")[-8_000:])


if __name__ == "__main__":
    sys.exit(main())
