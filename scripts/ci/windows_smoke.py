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

`--captures DIR` restarts an isolated owned HUD per global theme and captures
the four exact user-test fixture cells (each status-matrix update separately).
It compares only compatible successful-main capture artifacts. `--capture-comment`
is a separate, process-free step after artifact upload; missing baseline or fork
permissions never becomes a false no-change result. Both CI steps are informational.

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
import io
import itertools
import json
import os
import re
import secrets
import signal
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request
import zlib
import zipfile
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
CAPTURE_ARTIFACT = "windows-captures"
COMMENT_MARKER = "<!-- tze-hud-windows-captures -->"
MAX_ARTIFACT_BYTES = 20_000_000  # Strict decimal MB; reserve wrapper overhead below.
MAX_PNG_BYTES = 8 * 1024 * 1024
MAX_RGBA_BYTES = 64 * 1024 * 1024
CAPTURE_FIXTURES = (
    "all-zones-test", "notification-full-gamut", "subtitle-multiline",
    "status-indicator-theme-status-matrix-test",
)


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

    `probe` returns `(done, observation)` and must only observe, never mutate.
    Always probes once; transient transport errors share the same hang guard.
    HTTP/logic failures propagate. A running request is not canceled by this
    guard; a timeout names `what`, the last observation and transport class.
    """
    start = clock()
    observed: T | None = None
    has_observation = False
    transport_class = None
    while True:
        try:
            done, observed = probe()
            has_observation = True
            transport_class = None
        except urllib.error.HTTPError:
            raise  # HTTPError is also a URLError: never retry HTTP/auth failures.
        except RequestFailure as error:
            if not error.retryable:
                raise
            done = False
            transport_class = error.transport_class
        except (urllib.error.URLError, ConnectionError, TimeoutError) as error:
            done = False
            transport_class = type(error).__name__
        waited = clock() - start
        if done:
            return observed, waited
        if waited >= timeout_s:
            last = repr(observed) if has_observation else "no observation"
            detail = f" (transport: {transport_class})" if transport_class else ""
            raise AssertionError(f"{what}: not met after {timeout_s:.0f}s (last seen: {last}){detail}")
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
    """Safe request context, with transient transport failures distinguished."""

    def __init__(self, message: str, *, retryable: bool, transport_class: str | None = None) -> None:
        super().__init__(message)
        self.retryable = retryable
        self.transport_class = transport_class

def write_step_summary(summary: str) -> None:
    """Append measured counters when Actions supplies a summary file.

    This is an observation, not a pass marker. Escape adapter text as Markdown
    and keep the existing summary content; missing counters remain missing.
    """
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    text = summary.replace("\r", " ").replace("\n", " ").replace("\\", "\\\\")
    for control in "`*_[]()#|":
        text = text.replace(control, "\\" + control)
    text = text.replace("<", "&lt;").replace(">", "&gt;")
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"### Quiescent efficiency counters\n\n{text}\n\n")


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
            # both diagnostics and the eventual traceback. Only read-only polling
            # may retry these transport failures; this boundary never retries.
            raise RequestFailure(
                f"{label}: {error_class} after {elapsed:.3f}s",
                retryable=isinstance(err, (urllib.error.URLError, ConnectionError, TimeoutError))
                and not isinstance(err, urllib.error.HTTPError),
                transport_class=error_class,
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
    """Capture absence before due and presence after due without a second publish.

    Screenshots force an offscreen render, so this does not prove an autonomous
    on-screen presentation or visibility between the observed frames.
    """
    zone = next(s["s"] for s in surfaces_of(smoke) if s.get("accepts") == "text")
    smoke.ok("hud_clear", {"surface": zone})  # earlier steps may still hold this zone
    assert not is_held(surfaces_of(smoke), zone), f"{zone} still held after hud_clear"
    warm, _, _ = capture_frame(smoke)
    time.sleep(POLL_S)  # Let earlier publication fades settle at the existing cadence.
    baseline, _, _ = capture_frame(smoke)
    assert baseline[:2] == warm[:2], "screenshot dimensions changed during baseline"
    assert baseline[2] == warm[2], "delay baseline is not stable after hud_clear"
    width, height, before = baseline

    delay_ms = 3_000
    sent = time.monotonic()
    try:
        published = smoke.ok("hud_publish", {"surface": zone, "content": "CI delayed", "delay_ms": delay_ms, "ttl_ms": 20_000})
        accepted = time.monotonic()
        assert published.get("ok") is True, f"hud_publish {zone} was not accepted"
        assert is_held(surfaces_of(smoke), zone), f"{zone} scheduled publication is not held"

        # Processing occurred between the request and its acknowledgement. Use
        # both bounds; a capture spanning the earliest due time proves no absence.
        earliest_due, latest_due = sent + delay_ms / 1000, accepted + delay_ms / 1000
        early, _, early_finished = capture_frame(smoke)
        assert early_finished < earliest_due, "capture did not finish before the earliest due time"
        assert early[:2] == (width, height), "screenshot dimensions changed before due"
        assert early[2] == before, f"{zone} rendered content before its {delay_ms} ms delay"

        remaining = latest_due - time.monotonic()
        if remaining > 0:
            time.sleep(remaining)
        late, late_started, late_finished = capture_frame(smoke)
        assert late_started >= latest_due, "capture started before the latest due time"
        assert late[:2] == (width, height), "screenshot dimensions changed after due"
        after = late[2]
        changed = sum(1 for i in range(0, len(after), 4) if after[i : i + 4] != before[i : i + 4])
        assert changed >= MIN_CHANGED_PIXELS, (
            f"{zone} delayed content changed only {changed} pixels after due (need {MIN_CHANGED_PIXELS})"
        )
    finally:
        # Includes an accepted publication whose response/observation failed.
        smoke.ok("hud_clear", {"surface": zone})

    assert not is_held(surfaces_of(smoke), zone), f"{zone} still held after delayed content was cleared"
    restored, _, _ = capture_frame(smoke)
    assert restored[:2] == (width, height), "screenshot dimensions changed after clear"
    assert restored[2] == before, "delay capture did not return to its baseline after hud_clear"
    print(
        f"ok  delay_ms={delay_ms} held on acceptance, captured absent before due and present "
        f"after {late_finished - sent:.1f}s without a second publish ({changed} pixels changed; render on request)"
    )


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
    """Bounded 8-bit RGBA/non-interlaced PNG, with all five row filters."""
    assert len(data) <= MAX_PNG_BYTES, "PNG exceeds input limit"
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos, idat, width, height = 8, bytearray(), 0, 0
    ended = False
    while pos < len(data):
        assert pos + 12 <= len(data), "truncated PNG chunk"
        (length,) = struct.unpack(">I", data[pos : pos + 4])
        kind, body = data[pos + 4 : pos + 8], data[pos + 8 : pos + 8 + length]
        assert pos + 12 + length <= len(data), "truncated PNG chunk body"
        assert zlib.crc32(kind + body) & 0xFFFFFFFF == struct.unpack(">I", data[pos + 8 + length : pos + 12 + length])[0], "PNG CRC mismatch"
        if kind == b"IHDR":
            assert width == 0 and pos == 8 and length == 13, "invalid PNG header"
            width, height, depth, ctype, compression, filtering, interlace = struct.unpack(">IIBBBBB", body)
            assert (depth, ctype, compression, filtering, interlace) == (8, 6, 0, 0, 0), "expected 8-bit RGBA"
            assert 0 < width <= 8192 and 0 < height <= 8192 and width * height * 4 <= MAX_RGBA_BYTES, "PNG dimensions exceed limit"
        elif kind == b"IDAT":
            assert width, "PNG IDAT precedes header"
            idat.extend(body)
        elif kind == b"IEND":
            assert length == 0 and pos + 12 == len(data), "invalid PNG end"
            ended = True
        elif kind == b"PLTE":
            assert width and 0 < length <= 768 and length % 3 == 0, "invalid optional PNG palette"
        elif not kind[0] & 32:
            raise AssertionError("unsupported critical PNG chunk")
        pos += 12 + length
    assert width and ended and idat, "incomplete PNG"
    stride = width * 4
    expected = height * (stride + 1)
    inflater = zlib.decompressobj()
    raw = inflater.decompress(idat, expected + 1)
    assert len(raw) == expected and inflater.eof and not inflater.unused_data and not inflater.unconsumed_tail, "invalid PNG decoded size"
    out, prev = bytearray(), bytearray(stride)
    for y in range(height):
        row = bytearray(raw[y * (stride + 1) + 1 : (y + 1) * (stride + 1)])
        ftype = raw[y * (stride + 1)]
        assert 0 <= ftype <= 4, "unknown PNG row filter"
        # None copies a complete row. Sub has four independent prefix sums;
        # Up has no left dependency. Average/Paeth retain their exact recurrence.
        if ftype == 1:
            for channel in range(4):
                row[channel::4] = bytes(value & 255 for value in itertools.accumulate(row[channel::4]))
        elif ftype == 2:
            row[:] = bytes((a + b) & 255 for a, b in zip(row, prev))
        for i in range(stride) if ftype in (3, 4) else ():
            a = row[i - 4] if i >= 4 else 0
            b = prev[i]
            c = prev[i - 4] if i >= 4 else 0
            if ftype == 3:
                row[i] = (row[i] + (a + b) // 2) & 255
            elif ftype == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                row[i] = (row[i] + pred) & 255
        out += row
        prev = row
    return width, height, bytes(out)



def encode_png_rgba(width: int, height: int, pixels: bytes) -> bytes:
    """Encode the decoded diff, not a file-hash proxy for pixel equality."""
    assert 0 < width <= 8192 and 0 < height <= 8192
    assert len(pixels) == width * height * 4 <= MAX_RGBA_BYTES

    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    stride = width * 4
    rows = b"".join(b"\0" + pixels[y * stride : (y + 1) * stride] for y in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))


def compare_capture(current: dict, previous: dict | None, before: bytes | None, after: bytes) -> dict:
    """Incompatibility is unavailable; alpha has zero tolerance under WARP."""
    if previous is None or before is None:
        return {"status": "unavailable", "reason": "no compatible main baseline"}
    fields = ("theme", "fixture", "fixture_sha256", "checkpoint", "width", "height", "renderer", "window_mode", "build_profile", "settle_s")
    if any(current.get(key) != previous.get(key) for key in fields):
        return {"status": "unavailable", "reason": "baseline metadata differs"}
    width, height, old = decode_png_rgba(before)
    w2, h2, new = decode_png_rgba(after)
    if (width, height) != (current["width"], current["height"]) or (w2, h2) != (width, height):
        return {"status": "unavailable", "reason": "baseline PNG dimensions differ"}
    changed = tolerated = 0
    diff = bytearray(b"\0\0\0\xff" * (width * height))
    # Packed values are used only for exact four-byte equality, so native
    # endianness cannot affect RGB/alpha tolerances. Avoid per-channel Python
    # work on the many unchanged background pixels; never compare PNG hashes.
    for pixel, (old_pixel, new_pixel) in enumerate(zip(memoryview(old).cast("I"), memoryview(new).cast("I"))):
        if old_pixel == new_pixel:
            continue
        offset = pixel * 4
        exceeds = (abs(new[offset] - old[offset]) > 6
                   or abs(new[offset + 1] - old[offset + 1]) > 6
                   or abs(new[offset + 2] - old[offset + 2]) > 6
                   or new[offset + 3] != old[offset + 3])
        if exceeds:
            changed += 1
            diff[offset : offset + 4] = b"\xff\0\xff\xff"
        else:
            tolerated += 1
    return {"status": "changed" if changed else "unchanged", "changed_pixels": changed,
            "tolerated_pixels": tolerated, "diff_png": encode_png_rgba(width, height, diff)}


def update_capture_comment(api: Callable, repo: str, pr: int, body: str, changed: bool) -> str:
    """Only a changed comparison may create/update this bot's marked comment."""
    if not changed:
        return "not posted (no changed compatible capture)"
    comments = []
    for page in range(1, 6):
        batch = api(f"repos/{repo}/issues/{pr}/comments?per_page=100&page={page}")
        comments.extend(batch)
        if len(batch) < 100:
            break
    else:
        raise AssertionError("comment pagination exceeds bound")
    marked = [c for c in comments if c.get("user", {}).get("login") == "github-actions[bot]"
              and c.get("body", "").startswith(COMMENT_MARKER)]
    assert len(marked) <= 1, "multiple capture comments; refusing another"
    route = f"repos/{repo}/issues/comments/{marked[0]['id']}" if marked else f"repos/{repo}/issues/{pr}/comments"
    api(route, "PATCH" if marked else "POST", {"body": body})
    return "updated" if marked else "created"


def capture_github_api(route: str, method: str = "GET", body: dict | None = None):
    """gh consumes the job token; neither token nor raw failure body is logged."""
    args = ["gh", "api", route, "--method", method]
    if body is not None:
        args += ["--input", "-"]
    done = subprocess.run(args, input=json.dumps(body) if body is not None else None,
                          capture_output=True, text=True, timeout=30)
    assert done.returncode == 0 and len(done.stdout) <= 2 * 1024 * 1024, "GitHub API unavailable or response too large"
    return json.loads(done.stdout)


def capture_baseline(repo: str) -> tuple[dict | None, zipfile.ZipFile | None, str]:
    """Select only successful main push runs of this workflow, then its named artifact.

    Never extract archive paths. Expired/missing captures are not a golden pass;
    PR artifacts, even with the same name, are ineligible.
    """
    candidates = []
    for page in range(1, 6):
        batch = capture_github_api(f"repos/{repo}/actions/artifacts?name={CAPTURE_ARTIFACT}&per_page=100&page={page}")["artifacts"]
        for artifact in batch:
            link = artifact.get("workflow_run", {})
            if artifact["name"] != CAPTURE_ARTIFACT or link.get("head_branch") != "main":
                continue
            run = capture_github_api(f"repos/{repo}/actions/runs/{link['id']}")
            if (run["head_branch"] == "main" and run["event"] == "push" and run["conclusion"] == "success"
                    and run["path"].split("@")[0] == ".github/workflows/windows.yml"):
                candidates.append((run, artifact))
        if len(batch) < 100:
            break
    if candidates:
        # Run creation order, not a late re-run of an older commit. Artifact
        # creation breaks ties for repeated uploads of the same workflow run.
        run, artifact = max(candidates, key=lambda item: (item[0]["created_at"], item[1]["created_at"]))
        if artifact["expired"] or artifact["size_in_bytes"] >= MAX_ARTIFACT_BYTES:
            return None, None, "latest main capture artifact expired or exceeds limit"
        # Newer gh rejects binary stdout unless explicitly opted in, including
        # redirected files. Older versions have no guard or corresponding flag.
        try:
            help_result = subprocess.run(["gh", "api", "--help"], capture_output=True,
                                         text=True, timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            return None, None, "baseline download capability unavailable"
        if help_result.returncode != 0 or len(help_result.stdout) > 2 * 1024 * 1024:
            return None, None, "baseline download capability unavailable"
        command = ["gh", "api", f"repos/{repo}/actions/artifacts/{artifact['id']}/zip"]
        if re.search(r"(?<![\w-])--allow-escape-sequences(?![\w-])", help_result.stdout):
            command.append("--allow-escape-sequences")
        with tempfile.TemporaryFile() as download:
            done = subprocess.run(command, stdout=download, stderr=subprocess.DEVNULL, timeout=45)
            assert done.returncode == 0 and download.tell() < MAX_ARTIFACT_BYTES, "baseline download unavailable or exceeds limit"
            download.seek(0)
            with zipfile.ZipFile(download) as outer:
                info = outer.getinfo("captures.zip")
                assert info.file_size < MAX_ARTIFACT_BYTES, "baseline payload exceeds limit"
                archive = zipfile.ZipFile(io.BytesIO(outer.read(info)))
        infos = archive.infolist()
        assert len(infos) <= 200 and len({i.filename for i in infos}) == len(infos), "invalid baseline archive inventory"
        assert sum(i.file_size for i in infos) <= 200 * 1024 * 1024, "baseline archive expansion exceeds limit"
        info = archive.getinfo("manifest.json")
        assert info.file_size <= 256 * 1024, "baseline manifest exceeds limit"
        manifest = json.loads(archive.read(info))
        if (manifest.get("schema") != 1 or manifest.get("repo") != repo
                or str(manifest.get("run_id")) != str(run["id"]) or manifest.get("head") != run["head_sha"]
                or manifest.get("workflow") != "windows.yml" or manifest.get("event") != "push"
                or manifest.get("capture_complete") is not True or manifest.get("error")):
            archive.close()
            return None, None, "baseline provenance incompatible"
        manifest["baseline_artifact_id"] = artifact["id"]
        return manifest, archive, "available"
    return None, None, "no successful main capture artifact in bounded history"


def capture_fixture(smoke: Smoke, messages: list[dict], checkpoint: Callable[[int], None]) -> None:
    """Publish the exact batch payloads; widget matrix updates each get a frame."""
    surfaces = {f"widget:{m['widget']}" if "widget" in m else f"zone:{m['zone']}" for m in messages}
    for surface in sorted(surfaces):
        smoke.ok("hud_clear", {"surface": surface})
    try:
        for index, msg in enumerate(messages, 1):
            surface = f"widget:{msg['widget']}" if "widget" in msg else f"zone:{msg['zone']}"
            args = {"surface": surface, "ttl_ms": msg["ttl_ms"]}
            args.update({"params": msg["params"]} if "widget" in msg else {"content": msg["content"]})
            if "key" in msg:
                args["key"] = msg["key"]
            smoke.ok("hud_publish", args)
            if "widget" in msg:
                checkpoint(index)
        if all("widget" not in m for m in messages):
            checkpoint(len(messages))
    finally:
        for surface in sorted(surfaces):
            smoke.ok("hud_clear", {"surface": surface})


def run_captures(args: argparse.Namespace) -> int:
    """Owned Windows CI only. Restart between themes; never attach to a borrowed URL.

    Screenshots are the compositor frame, not desktop composition/DPI/multi-monitor
    proof. Native restyle/no-op PR observations and owner signoff remain separate.
    """
    output = args.captures
    output.mkdir(parents=True, exist_ok=False)
    repo_root = Path(__file__).resolve().parents[2]
    manifest = {"schema": 1, "workflow": "windows.yml", "repo": os.environ.get("GITHUB_REPOSITORY"),
                "event": os.environ.get("GITHUB_EVENT_NAME"), "head": os.environ.get("GITHUB_SHA"),
                "run_id": os.environ.get("GITHUB_RUN_ID"), "rgb_tolerance": 6, "alpha_tolerance": 0,
                "scope": "HUD own frame only; no desktop/DPI/multi-monitor/owner signoff", "cells": {}}
    result, baseline, archive = 1, None, None
    rows = ["| Theme / fixture / checkpoint | Dimensions | Changed | Tolerated | Result |",
            "| --- | --- | ---: | ---: | --- |"]
    try:
        assert sys.platform == "win32", "captures require an owned Windows WARP process"
        assert args.config and args.exe and not args.quiescent_with_content, "captures require owned exe/config"
        fixtures = {name: (repo_root / ".claude/skills/user-test/scripts" / f"{name}.json").read_bytes()
                    for name in CAPTURE_FIXTURES}
        themes = sorted(p.stem for p in (repo_root / "assets/themes").glob("*.toml"))
        assert themes == ["blueprint", "classic", "tonal-glass"], "capture theme inventory changed"
        psk = secrets.token_hex(32)
        config = seed_config(args.config, psk)
        original = config.read_text(encoding="utf-8")
        parsed = tomllib.loads(original)
        assert "theme" in parsed.get("design_tokens", {}), "seed config has no theme selector"
        for theme in themes:
            # Only the copied selector changes. All deploy overrides/fonts/geometry stay.
            selected, count = re.subn(r'(?m)^(theme\s*=\s*)"[^"]*"', lambda m: m[1] + json.dumps(theme), original)
            assert count == 1 and tomllib.loads(selected)["design_tokens"]["theme"] == theme, "ambiguous theme selector"
            config.write_text(selected, encoding="utf-8")
            log_path = config.parent / f"{theme}.log"
            # The HUD needs no GitHub credential; only this capture coordinator
            # invokes gh. Keep the job token out of the native child environment.
            child_env = {key: value for key, value in os.environ.items() if key not in ("GH_TOKEN", "GITHUB_TOKEN")}
            child_env.update({"TZE_HUD_LOG": "info", "TZE_HUD_LOG_JSON": "1"})
            with log_path.open("wb") as log:
                proc = subprocess.Popen([str(args.exe.resolve()), "--config", str(config), "--window-mode", "fullscreen",
                                         "--mcp-port", str(args.mcp_port)], stdout=log, stderr=subprocess.STDOUT,
                                        env=child_env)
            smoke = Smoke(f"http://127.0.0.1:{args.mcp_port}/", psk)
            try:
                wait_for_mcp(smoke, proc, args.startup_timeout)
                status = json.loads(smoke.get("/admin/status")[1])
                assert status["pid"] == proc.pid, "MCP is not this smoke-owned process"
                assert log_path.stat().st_size <= 8 * 1024 * 1024, "owned capture log exceeds limit"
                adapters = [json.loads(line).get("fields", {}) for line in log_path.read_text(errors="replace").splitlines()
                            if line.startswith("{") and "windowed: GPU adapter selected" in line]
                assert len(adapters) == 1, "actual renderer identity unavailable"
                renderer = {key: adapters[0][key] for key in ("backend", "device_name", "vendor")}
                assert "Microsoft Basic Render Driver" in renderer["device_name"] or "WARP" in renderer["device_name"], "capture renderer is not WARP"
                for fixture, source in fixtures.items():
                    messages = json.loads(source)

                    def checkpoint(index: int) -> None:
                        # Capture settled motion before any fixture's original TTL expires.
                        time.sleep(1)
                        started = time.monotonic()
                        request = urllib.request.Request(smoke.url.rstrip("/") + "/admin/screenshot",
                                                         headers={"Authorization": f"Bearer {psk}"})
                        with urllib.request.urlopen(request, timeout=15) as response:
                            assert response.status == 200 and response.headers.get("Content-Type") == "image/png", "capture is not a PNG"
                            image = response.read(MAX_PNG_BYTES + 1)
                        captured_s = time.monotonic() - started
                        started = time.monotonic()
                        width, height, _ = decode_png_rgba(image)
                        decode_s = time.monotonic() - started
                        name = f"{theme}--{fixture}--{index:02d}"
                        (output / f"{name}.png").write_bytes(image)
                        manifest["cells"][name] = {"theme": theme, "fixture": fixture,
                            "fixture_sha256": hashlib.sha256(source).hexdigest(), "checkpoint": index,
                            "width": width, "height": height, "renderer": renderer, "window_mode": "fullscreen",
                            "build_profile": os.environ.get("HUD_BUILD_PROFILE", "unknown"),
                            "settle_s": 1, "capture_s": captured_s, "decode_s": decode_s}
                        print(f"capture {name}: {width}x{height}, HTTP {captured_s:.3f}s, decode {decode_s:.3f}s")

                    capture_fixture(smoke, messages, checkpoint)
            finally:
                if proc.poll() is None:
                    proc.kill()  # Only this Popen's PID, never taskkill by image name.
                proc.wait(timeout=10)  # Process handle signaled/owned PID gone before next theme.
                assert proc.poll() is not None, "owned HUD did not stop"
                print(f"stopped owned HUD pid={proc.pid} before next theme")
        manifest["capture_complete"] = True
        repo = manifest["repo"]
        assert isinstance(repo, str) and re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo), "Actions repository unavailable"
        try:
            baseline, archive, reason = capture_baseline(repo)
        except (AssertionError, OSError, ValueError, zipfile.BadZipFile, KeyError, subprocess.TimeoutExpired):
            reason = "baseline retrieval failed (permission, format, or transport); comparison unavailable"
        manifest["baseline_status"] = reason
        manifest["baseline"] = ({key: baseline[key] for key in ("run_id", "head", "workflow", "baseline_artifact_id")}
                                if baseline else None)
        for name, cell in manifest["cells"].items():
            previous = baseline.get("cells", {}).get(name) if baseline else None
            before = None
            if previous is not None and archive is not None:
                info = archive.getinfo(f"{name}.png")
                assert info.file_size <= MAX_PNG_BYTES, "baseline PNG exceeds limit"
                before = archive.read(info)
            after = (output / f"{name}.png").read_bytes()
            comparison = compare_capture(cell, previous, before, after)
            diff = comparison.pop("diff_png", None)
            if diff is not None:
                (output / f"{name}--before.png").write_bytes(before)
                (output / f"{name}--diff.png").write_bytes(diff)
            cell["comparison"] = comparison
            rows.append(f"| {name} | {cell['width']}×{cell['height']} | {comparison.get('changed_pixels', '—')} | {comparison.get('tolerated_pixels', '—')} | {comparison['status']} |")
        result = 0
    except Exception as error:
        # Informational failure still exits nonzero, with a safe diagnostic artifact.
        manifest["error"] = type(error).__name__
        manifest["error_hint"] = str(error) if isinstance(error, AssertionError) else "transport, artifact, or IO failure; details not logged"
        print(f"capture unavailable: {type(error).__name__}", file=sys.stderr)
    finally:
        if archive is not None:
            archive.close()
        report = "\n".join(rows) + "\n\n" + manifest.get("baseline_status", "Comparison unavailable: capture incomplete.") + "\n"
        report += "\nOwn-frame software captures do not replace desktop/DPI/multi-monitor or owner signoff.\n"
        if manifest.get("error"):
            report += f"\nCapture incomplete: {manifest['error']} ({manifest['error_hint']}).\n"
        (output / "report.md").write_text(report, encoding="utf-8")
        (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        payload = output / "captures.zip"
        assert sum(path.stat().st_size for path in output.iterdir()) <= 200 * 1024 * 1024, "capture archive input exceeds limit"
        with zipfile.ZipFile(payload, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as zipped:
            for path in sorted(output.iterdir()):
                if path != payload:
                    zipped.write(path, path.name)
        if payload.stat().st_size >= MAX_ARTIFACT_BYTES - 100_000:
            payload.unlink()  # Keep local PNGs/report; never upload an oversized payload.
            print("capture artifact unavailable: exceeds 20 MB including upload-wrapper reserve", file=sys.stderr)
            result = 1
    return result



def post_capture_comment(output: Path) -> int:
    """Called only after Actions successfully uploads the bounded payload."""
    try:
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        assert not manifest.get("error") and (output / "captures.zip").stat().st_size < MAX_ARTIFACT_BYTES, "captures incomplete"
        if manifest["event"] != "pull_request":
            return 0
        changed = any(c.get("comparison", {}).get("status") == "changed" for c in manifest["cells"].values())
        if not changed:
            print("capture comment: not posted (no changed compatible capture)")
            return 0
        event_path = Path(os.environ["GITHUB_EVENT_PATH"])
        assert event_path.stat().st_size <= 2 * 1024 * 1024, "event exceeds limit"
        event = json.loads(event_path.read_text(encoding="utf-8"))
        repo, run_id = manifest["repo"], manifest["run_id"]
        assert repo == os.environ["GITHUB_REPOSITORY"] and str(run_id) == os.environ["GITHUB_RUN_ID"], "comment provenance differs"
        assert re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) and str(run_id).isdigit(), "invalid run identity"
        report = (output / "report.md").read_text(encoding="utf-8")
        body = (COMMENT_MARKER + f"\n### Windows capture changes\n\nRun {run_id}, tested checkout `{manifest['head']}`.\n\n"
                + report + f"\n[Uploaded before/after/diff artifact](https://github.com/{repo}/actions/runs/{run_id})\n")
        print("capture comment: " + update_capture_comment(capture_github_api, repo, event["pull_request"]["number"], body, True))
        return 0
    except (AssertionError, OSError, KeyError, ValueError, subprocess.TimeoutExpired):
        print("capture comment unavailable (job permission/transport); no success claimed", file=sys.stderr)
        return 1


def capture_frame(smoke: Smoke) -> tuple[tuple[int, int, bytes], float, float]:
    """Decode one own-frame capture, bounded by monotonic request/response times.

    The response timestamp precedes PNG decoding: CPU decode time is not part
    of the interval in which the compositor could have captured the frame.
    Capture failures propagate; this helper never retries unavailable evidence.
    """
    started = time.monotonic()
    status, ctype, body = smoke.get_bytes("/admin/screenshot")
    finished = time.monotonic()
    assert status == 200 and ctype == "image/png", f"/admin/screenshot: {status} {ctype}"
    return decode_png_rgba(body), started, finished


def check_screenshot(smoke: Smoke) -> None:
    """The screenshot is a real PNG of the frame, and it shows a new notification.

    The overlay is fully opaque under WARP, so alpha proves nothing. Instead take
    a baseline, publish, take another, and require the two to differ in a
    meaningful number of pixels (the notification's backdrop and text).
    """

    (width, height, before), _, _ = capture_frame(smoke)
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
        (w2, h2, after), _, _ = capture_frame(smoke)
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
        summary = quiescent_summary(artifact)
        print("measured:", summary)
        write_step_summary(summary)
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
    ap.add_argument("--exe", type=Path)
    ap.add_argument("--config", type=Path)
    ap.add_argument("--captures", type=Path, help="owned WARP theme/fixture frames and informational main diff")
    ap.add_argument("--capture-comment", type=Path, help="comment on changed captures after successful artifact upload")
    ap.add_argument("--poc-demo", type=Path, help="poc_demo.exe: the resident-tile orphan/reclaim step needs it")
    ap.add_argument("--quiescent-with-content", action="store_true", help="run only the held-content idle measurement")
    ap.add_argument("--quiescent-timeout", type=float, default=150, help="hang guard for the 65 s measurement")
    ap.add_argument("--mcp-port", type=int, default=9090)
    ap.add_argument("--startup-timeout", type=float, default=90)
    ap.add_argument("--settle-s", type=float, default=3, help="seconds the HUD must stay up after checks")
    args = ap.parse_args()
    if args.capture_comment is not None:
        if args.exe or args.config or args.captures or args.quiescent_with_content:
            ap.error("--capture-comment cannot launch or attach to a HUD")
        return post_capture_comment(args.capture_comment)
    if args.exe is None or args.config is None:
        ap.error("--exe and --config are required for owned smoke/captures")
    if args.captures is not None:
        return run_captures(args)
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
