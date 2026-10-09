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

Between the admin checks and the pairing check, the POC stages run: widget
publish and TTL expiry, notification TTL expiry, a `delay_ms` publish that
appears on schedule, and a resident tile whose agent process is killed (the
tile must stay while orphaned and be gone after the 30 s lease grace, seen in a
fresh session's SceneSnapshot via `poc_demo snapshot`). Every wait polls with a
hang guard; none assumes exact timing.

`--quiescent-with-content` instead boots a second HUD under
`--quiescent-efficiency-emit`, publishes held content inside the settle window,
and checks that the runtime then presents nothing for the 60 s observation. It
is informational in CI (see windows.yml).

The config is copied to a temp dir with a seeded agents.toml beside it holding
only the SHA-256 of a fresh random PSK, the way pairing stores agents.

Stdlib only. Exits non-zero on the first failed check and prints the HUD log.

    python scripts/ci/windows_smoke.py --exe target/release/tze_hud.exe \
        --config app/tze_hud_app/config/production.toml
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import importlib.util
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
from typing import Callable, Iterator, TypeVar

MIN_CHANGED_PIXELS = 200
POLL_S = 0.25
HANG_GUARD_S = 45  # upper bound on any single wait; the expected time is far shorter
EARLY_SLACK_S = 0.5  # an event may not land earlier than its delay, give or take this
ORPHAN_GRACE_S = 30  # SceneGraph::DEFAULT_GRACE_PERIOD_MS; not configurable
TILE_POLL_S = 3.0
AGENT = "ci-smoke"
TOOLS = {"hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"}


T = TypeVar("T")


def wait_until(
    probe: Callable[[], tuple[bool, T]],
    *,
    what: str,
    timeout_s: float = HANG_GUARD_S,
    interval_s: float = POLL_S,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> tuple[T, float]:
    """Poll `probe` until it reports done; return (last observation, seconds waited).

    `probe` returns `(done, observation)`. Always probes once, then until the
    hang guard; a timeout names `what` and the last observation.
    """
    start = clock()
    while True:
        done, observed = probe()
        waited = clock() - start
        if done:
            return observed, waited
        if waited >= timeout_s:
            raise AssertionError(f"{what}: not met after {timeout_s:.0f}s (last seen: {observed!r})")
        sleep(min(interval_s, timeout_s - waited))


def surface_entry(surfaces: list[dict], name: str) -> dict:
    """The hud_surfaces entry for `name`; a missing surface is a failure."""
    for entry in surfaces:
        if entry.get("s") == name:
            return entry
    raise AssertionError(f"{name} not in hud_surfaces: {sorted(e.get('s') for e in surfaces)}")


def is_held(surfaces: list[dict], name: str) -> bool:
    return surface_entry(surfaces, name).get("held") is True


def held_probe(smoke: "Smoke", name: str, want: bool) -> Callable[[], tuple[bool, dict]]:
    """A wait_until probe that is done once `name` is (or is no longer) held."""

    def probe() -> tuple[bool, dict]:
        entry = surface_entry(smoke.ok("hud_surfaces")["surfaces"], name)
        return (entry.get("held") is True) == want, entry

    return probe


def assert_not_early(waited_s: float, expected_s: float, what: str) -> None:
    """An event that is due after `expected_s` must not have happened sooner."""
    assert waited_s >= expected_s - EARLY_SLACK_S, (
        f"{what} after {waited_s:.2f}s, before its {expected_s:.2f}s schedule"
    )


def parse_tile_count(output: str) -> int:
    """The `tiles <n>` line of `poc_demo snapshot`."""
    for line in output.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == "tiles" and parts[1].isdigit():
            return int(parts[1])
    raise AssertionError(f"poc_demo snapshot printed no `tiles <n>` line: {output[-300:]!r}")


def quiescent_summary(artifact: dict) -> str:
    """One line of the numbers the efficiency gate judges."""
    gpu, wake = artifact.get("gpu", {}), artifact.get("wakeups", {})
    return (
        f"presents={gpu.get('presents')} submissions={gpu.get('queue_submissions')} "
        f"acquisitions={gpu.get('surface_acquisitions')} "
        f"runtime_wakeups={wake.get('combined_runtime_driven')} "
        f"settle_ms={artifact.get('settling_duration_ms')} "
        f"interval_ms={artifact.get('interval_duration_ms')} "
        f"adapter={artifact.get('renderer', {}).get('adapter')!r}"
    )


class RequestFailure(AssertionError):
    """Safe request context, with transient startup failures distinguished."""

    def __init__(self, message: str, *, retryable: bool) -> None:
        super().__init__(message)
        self.retryable = retryable


class Smoke:
    def __init__(self, url: str, psk: str) -> None:
        self.url = url
        self.psk = psk
        self.next_id = 0

    @contextmanager
    def _boundary(self, label: str) -> Iterator[None]:
        started = time.monotonic()
        print(f"request begin {label}", flush=True)
        try:
            yield
        except Exception as err:
            elapsed = time.monotonic() - started
            error_class = type(err).__name__
            print(f"request failure {label} elapsed_s={elapsed:.3f} error={error_class}", flush=True)
            # Keep exception values, including URLs and response payloads, out of
            # both diagnostics and the eventual traceback. Only startup discovery
            # may retry these transport failures; this boundary never retries.
            raise RequestFailure(
                f"{label}: {error_class} after {elapsed:.3f}s",
                retryable=isinstance(err, (urllib.error.URLError, ConnectionError, TimeoutError)),
            ) from None
        else:
            print(f"request end {label} elapsed_s={time.monotonic() - started:.3f}", flush=True)

    @staticmethod
    def _operator_label(method: str, path: str) -> str:
        safe_method = method if method in {"GET", "POST"} else "other"
        route = path.partition("?")[0]
        safe_path = route if route in {
            "/admin/status", "/admin/logs", "/admin/screenshot", "/admin/restart", "/pair",
        } else "other"
        return f"operator method={safe_method} path={safe_path}"

    def rpc(self, method: str, params: dict | None = None, *, timeout_s: float = 10) -> dict:
        self.next_id += 1
        safe_method = method if method in {"initialize", "tools/list", "tools/call"} else "other"
        label = f"rpc id={self.next_id} method={safe_method}"
        if method == "tools/call":
            tool = params.get("name") if isinstance(params, dict) else None
            label += f" tool={tool if isinstance(tool, str) and tool in TOOLS else 'other'}"
        with self._boundary(label):
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
            with urllib.request.urlopen(req, timeout=timeout_s) as resp:
                reply = json.load(resp)
            if "error" in reply:
                raise AssertionError("JSON-RPC error response")
            return reply["result"]

    def request(self, method: str, path: str) -> tuple[int, str, bytes]:
        """Call an operator endpoint on the same port; return (status, content type, body)."""
        with self._boundary(self._operator_label(method, path)):
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
            raise AssertionError(f"{self._operator_label('GET', path)}: HTTP {status}")
        return status, ctype, body

    def get(self, path: str) -> tuple[int, str]:
        """GET an operator endpoint on the same port; return (status, body)."""
        status, _, body = self.get_bytes(path)
        return status, body.decode("utf-8", "replace")

    def call(self, tool: str, args: dict | None = None, *, timeout_s: float = 10) -> tuple[bool, dict]:
        """Call a tool; return (is_error, parsed result text)."""
        result = self.rpc("tools/call", {"name": tool, "arguments": args or {}}, timeout_s=timeout_s)
        text = result["content"][0]["text"]
        return bool(result.get("isError")), json.loads(text)

    def ok(self, tool: str, args: dict | None = None, *, timeout_s: float = 10) -> dict:
        is_error, payload = self.call(tool, args, timeout_s=timeout_s)
        if is_error:
            raise AssertionError(f"{tool if tool in TOOLS else 'other'}: tool error response")
        return payload


def wait_for_mcp(
    smoke: Smoke,
    proc: subprocess.Popen,
    timeout_s: float,
    *,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    """Require metadata and scene/portal discovery within the original guard.

    Request timeouts cap each socket operation, not overall wall cancellation.
    A response observed at or after the deadline cannot establish readiness.
    """
    deadline = clock() + timeout_s
    while clock() < deadline:
        if proc.poll() is not None:
            raise AssertionError(f"tze_hud exited during startup (code {proc.returncode})")
        try:
            remaining = deadline - clock()
            if remaining <= 0:
                break
            smoke.rpc(
                "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}},
                timeout_s=min(10, remaining),
            )
            if proc.poll() is not None:
                raise AssertionError(f"tze_hud exited during startup (code {proc.returncode})")
            remaining = deadline - clock()
            if remaining <= 0:
                break
            surfaces = smoke.ok("hud_surfaces", timeout_s=min(10, remaining))
            assert isinstance(surfaces, dict) and isinstance(surfaces.get("surfaces"), list), (
                "hud_surfaces: invalid startup discovery response"
            )
            if proc.poll() is not None:
                raise AssertionError(f"tze_hud exited during startup (code {proc.returncode})")
            if clock() >= deadline:
                break
            return
        except RequestFailure as err:
            if not err.retryable:
                raise
        remaining = deadline - clock()
        if remaining <= 0:
            break
        sleep(min(0.5, remaining))
    raise AssertionError(f"MCP metadata and scene discovery not ready after {timeout_s:.0f}s")


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


def surfaces_of(smoke: Smoke) -> list[dict]:
    return smoke.ok("hud_surfaces")["surfaces"]


def check_widget(smoke: Smoke) -> None:
    """Typed widget params publish, are held, and a TTL'd publish expires on its own."""
    gauge, progress = "widget:main-gauge", "widget:main-progress"
    assert smoke.ok("hud_publish", {"surface": gauge, "params": {"level": 0.7, "label": "CI"}}).get("ok") is True
    assert is_held(surfaces_of(smoke), gauge), f"{gauge} not held after publish"
    smoke.ok("hud_clear", {"surface": gauge})
    assert not is_held(surfaces_of(smoke), gauge), f"{gauge} still held after hud_clear"

    ttl_ms = 2_000
    smoke.ok("hud_publish", {"surface": progress, "params": {"progress": 0.4, "label": "CI"}, "ttl_ms": ttl_ms})
    assert is_held(surfaces_of(smoke), progress), f"{progress} not held after publish"
    _, waited = wait_until(held_probe(smoke, progress, False), what=f"{progress} ttl_ms={ttl_ms} expiry")
    assert_not_early(waited, ttl_ms / 1000, f"{progress} expired")
    print(f"ok  widget publish held, hud_clear released, ttl_ms={ttl_ms} expired after {waited:.1f}s")


def check_notification_ttl(smoke: Smoke) -> None:
    """A notification with a short TTL disappears unattended: `held` drops."""
    zone, ttl_ms = "zone:notification-area", 3_000
    sent = time.monotonic()
    published = smoke.ok(
        "hud_publish", {"surface": zone, "content": {"title": "CI", "body": "expires on its TTL"}, "ttl_ms": ttl_ms}
    )
    assert published.get("ok") is True and published.get("expires_in_ms", 0) <= ttl_ms, (
        f"hud_publish {zone} ttl_ms={ttl_ms}: {published}"
    )
    assert is_held(surfaces_of(smoke), zone), f"{zone} not held after publish"
    wait_until(held_probe(smoke, zone, False), what=f"{zone} ttl_ms={ttl_ms} expiry")
    waited = time.monotonic() - sent
    assert_not_early(waited, ttl_ms / 1000, f"{zone} expired")
    print(f"ok  notification ttl_ms={ttl_ms} expired unattended after {waited:.1f}s")


def check_delay(smoke: Smoke) -> None:
    """delay_ms content is absent until due, then appears with nobody calling again."""
    zone = next(s["s"] for s in surfaces_of(smoke) if s.get("accepts") == "text")
    smoke.ok("hud_clear", {"surface": zone})  # earlier steps may still hold this zone
    assert not is_held(surfaces_of(smoke), zone), f"{zone} still held after hud_clear"
    delay_ms = 3_000
    sent = time.monotonic()
    smoke.ok("hud_publish", {"surface": zone, "content": "CI delayed", "delay_ms": delay_ms, "ttl_ms": 20_000})
    assert not is_held(surfaces_of(smoke), zone), f"{zone} held before its {delay_ms} ms delay"
    wait_until(held_probe(smoke, zone, True), what=f"{zone} delay_ms={delay_ms}")
    waited = time.monotonic() - sent
    assert_not_early(waited, delay_ms / 1000, f"{zone} content appeared")
    smoke.ok("hud_clear", {"surface": zone})
    print(f"ok  delay_ms={delay_ms} content absent until due, appeared after {waited:.1f}s")


def poc_demo_tiles(poc_demo: Path, psk: str) -> int:
    """Tiles in a fresh gRPC session's SceneSnapshot (`poc_demo snapshot`)."""
    done = subprocess.run(
        [str(poc_demo), "snapshot", "--agent", AGENT],
        capture_output=True,
        text=True,
        timeout=30,
        env={**os.environ, "TZE_HUD_PSK": psk},
    )
    assert done.returncode == 0, f"poc_demo snapshot: exit {done.returncode} {done.stderr.strip()[-300:]}"
    return parse_tile_count(done.stdout)


def check_tile_orphan(smoke: Smoke, poc_demo: Path) -> None:
    """Kill a resident agent holding a tile: the tile stays while orphaned, then is reclaimed."""
    before = poc_demo_tiles(poc_demo, smoke.psk)
    out_path = Path(tempfile.mkdtemp(prefix="tze_hud_tile_")) / "override-hang.log"
    with open(out_path, "wb") as out:
        agent = subprocess.Popen(
            [str(poc_demo), "override-hang", "--agent", AGENT, "--human-wait-s", "600"],
            stdout=out,
            stderr=subprocess.STDOUT,
            env={**os.environ, "TZE_HUD_PSK": smoke.psk},
        )
    try:
        def claimed() -> tuple[bool, str]:
            text = out_path.read_text(errors="replace")
            assert agent.poll() is None, f"poc_demo override-hang exited early ({agent.returncode}): {text[-300:]}"
            return "Not reading the stream" in text, text[-200:]

        wait_until(claimed, what="poc_demo override-hang claiming its tile")
        killed = time.monotonic()
        kill_pid(agent.pid)
    finally:
        if agent.poll() is None:
            agent.kill()
        agent.wait(timeout=10)

    # Disconnect orphans the lease: the tile is kept (badged) for the grace period.
    orphaned = poc_demo_tiles(poc_demo, smoke.psk)
    assert orphaned == before + 1, f"tiles {before} -> {orphaned} after the agent was killed; expected the orphan kept"
    wait_until(
        lambda: (lambda n: (n == before, n))(poc_demo_tiles(poc_demo, smoke.psk)),
        what=f"orphaned tile reclaimed (tiles back to {before}) after the {ORPHAN_GRACE_S}s grace",
        timeout_s=ORPHAN_GRACE_S + HANG_GUARD_S,
        interval_s=TILE_POLL_S,
    )
    waited = time.monotonic() - killed
    assert_not_early(waited, ORPHAN_GRACE_S, "orphaned tile reclaimed")
    print(f"ok  killed agent's tile orphaned (kept), then reclaimed after {waited:.1f}s (grace {ORPHAN_GRACE_S}s)")


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
    with smoke._boundary(smoke._operator_label("POST", "/pair")):
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
        f'[agents.{AGENT}]\npsk_sha256 = "{digest}"\nallow = ["*", "admin"]\n', encoding="utf-8"
    )
    return seeded


def load_efficiency_checker():
    path = Path(__file__).with_name("check_idle_efficiency.py")
    spec = importlib.util.spec_from_file_location("check_idle_efficiency", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run_quiescent(args: argparse.Namespace) -> int:
    """Hold content on screen under the runtime's own idle measurement.

    The runtime measures 5 s of settling after its first present, then 60 s in
    which presents, queue submissions, and surface acquisitions must all be
    zero. Content published during the settle (held: no TTL, so no fade-out
    inside the window) is on screen for the whole observation.
    """
    psk = secrets.token_hex(32)
    config = seed_config(args.config, psk)
    work = Path(tempfile.mkdtemp(prefix="tze_hud_quiescent_"))
    artifact_path = work / "quiescent-efficiency.json"
    log_path = work / "tze_hud.log"
    cmd = [
        str(args.exe), "--config", str(config), "--window-mode", "fullscreen",
        "--mcp-port", str(args.mcp_port), "--quiescent-efficiency-emit", str(artifact_path),
    ]
    with open(log_path, "wb") as log:
        proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    smoke = Smoke(f"http://127.0.0.1:{args.mcp_port}/", psk)
    try:
        wait_for_mcp(smoke, proc, args.startup_timeout)
        smoke.ok("hud_publish", {"surface": "zone:notification-area", "content": {"title": "CI", "body": "held content"}, "ttl_ms": 0})
        smoke.ok("hud_publish", {"surface": "widget:main-gauge", "params": {"level": 0.5, "label": "CI"}})
        assert is_held(surfaces_of(smoke), "zone:notification-area"), "notification not held after publish"
        print("ok  content published (held); waiting for the 5 s settle + 60 s measurement")

        def written() -> tuple[bool, str]:
            return artifact_path.exists() or proc.poll() is not None, f"exit={proc.poll()}"

        wait_until(written, what="quiescent artifact", timeout_s=args.quiescent_timeout, interval_s=1)
        assert artifact_path.exists(), f"tze_hud exited ({proc.returncode}) without writing {artifact_path.name}"
        artifact = json.loads(artifact_path.read_text(encoding="utf-8"))
        print("measured:", quiescent_summary(artifact))
        _, failures = load_efficiency_checker().validate_artifact(artifact, require_constrained=False)
        assert not failures, "quiescent gate failed: " + "; ".join(failures)
        print("ok  held content on screen, then no presents, submissions, or acquisitions for the interval")
        return 0
    except AssertionError as err:
        print(f"FAIL {err}", file=sys.stderr)
        return 1
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=10)
        print(f"--- HUD log ({log_path}) ---")
        print(log_path.read_text(errors="replace")[-8_000:])


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--exe", required=True, type=Path)
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--poc-demo", type=Path, help="poc_demo.exe: the resident-tile orphan/reclaim step needs it")
    ap.add_argument("--quiescent-with-content", action="store_true", help="run only the held-content idle measurement")
    ap.add_argument("--quiescent-timeout", type=float, default=150, help="hang guard for the 65 s measurement")
    ap.add_argument("--mcp-port", type=int, default=9090)
    ap.add_argument("--startup-timeout", type=float, default=90)
    ap.add_argument("--settle-s", type=float, default=3, help="seconds the HUD must stay up after checks")
    args = ap.parse_args()
    if args.quiescent_with_content:
        return run_quiescent(args)
    if args.poc_demo is None:
        ap.error("--poc-demo is required (unless --quiescent-with-content)")

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
        check_widget(smoke)
        check_notification_ttl(smoke)
        check_delay(smoke)
        check_tile_orphan(smoke, args.poc_demo)
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
