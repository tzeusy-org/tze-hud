#!/usr/bin/env python3
"""Drive a real Linux tze_hud with real X input on a private Xvfb (T13 tier L).

Each run starts its own Xvfb (`-displayfd`) and its own HUD from `--exe` with
a freshly seeded config, an ephemeral loopback MCP port and gRPC disabled,
then plays gesture scenarios through XTEST (`xdotool`) and asserts on state:
the `/admin/status` `portals` block and MCP results. Pixels are checked only
inside rects that status reports. Readiness is observed (`idle_since_ms`),
never slept for, and nothing is retried.

Owned mode only. Input is injected only into the Xvfb this run started, and
only while `/admin/status` reports the pid of the HUD this run launched.

    just gesture                      # build, then every scenario
    just gesture composer unread      # selected scenarios
    python3 scripts/ci/hud_gestures.py --exe target/debug/tze_hud [scenario...]

Artifacts land in `test_results/gestures/<sha>/<run>/` (gitignored): status
JSON and runtime screenshots before/after each scenario, an X screen grab and
the log tail on failure, and `timeline.json`.

Stdlib only (plus the `xdotool`, `Xvfb` and optional `ffmpeg` executables).
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

sys.path.insert(0, str(Path(__file__).resolve().parent))
from windows_smoke import (  # noqa: E402  (sibling import; windows_smoke never imports this)
    RequestFailure,
    Smoke,
    capture_frame,
    kill_pid,
    seed_config,
    wait_for_mcp,
    wait_until,
)

REPO = Path(__file__).resolve().parents[2]
CONFIG = REPO / "app/tze_hud_app/config/production.toml"
# Mesa llvmpipe ICD; older Mesa names it lvp_icd.x86_64.json (as in the justfile).
LAVAPIPE = next((p for p in (Path("/usr/share/vulkan/icd.d/lvp_icd.x86_64.json"),
                             Path("/usr/share/vulkan/icd.d/lvp_icd.json")) if p.exists()),
                Path("/usr/share/vulkan/icd.d/lvp_icd.json"))
SCREEN = (1280, 800)
SETTLE_S = 5.0  # bound on any one observed transition
# A focused composer's caret blink presents about every 530 ms, so the HUD
# counts as settled once nothing has presented for this long (docs/api.md).
SETTLED_IDLE_MS = 300


class Refused(RuntimeError):
    """A guard refused to inject input or to run against something not owned."""


class ScenarioFailed(AssertionError):
    def __init__(self, step: str, last: object) -> None:
        super().__init__(step)
        self.step = step
        self.last = last


# ── Guards ───────────────────────────────────────────────────────────────────


def check_owned(*, started_display: str | None, target_display: str | None,
                owned_pid: int | None, status_pid: object, url: str) -> None:
    """Refuse unless input would reach this run's Xvfb and this run's HUD."""
    host = urllib.parse.urlsplit(url).hostname
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise Refused(f"refusing a borrowed HUD URL (host {host!r}); this harness owns its HUD")
    if not started_display or target_display != started_display:
        raise Refused(f"DISPLAY {target_display!r} is not the Xvfb this run started ({started_display!r})")
    if owned_pid is None:
        raise Refused("no owned HUD process")
    if status_pid != owned_pid:
        raise Refused(f"/admin/status pid {status_pid!r} is not the owned HUD pid {owned_pid}")


# ── Geometry ─────────────────────────────────────────────────────────────────


def portal_entry(status: dict, surface: str) -> dict | None:
    return next((p for p in status.get("portals") or [] if p.get("s") == surface), None)


def resolve_target(status: dict, target: str) -> tuple[int, int]:
    """Centre of `portal:<id>/<pane>` (or the whole portal) in screen pixels.

    X11 has no DPI virtualisation and the primary display's top-left is the
    scene origin, so scene pixels are screen pixels on the primary.
    """
    surface, _, pane = target.partition("/")
    portal = portal_entry(status, surface)
    if portal is None:
        raise LookupError(f"{surface} is not on screen")
    rect = portal["rect"] if not pane else (portal.get("panes") or {}).get(pane)
    if not rect:
        raise LookupError(f"{target} is not shown")
    displays = status.get("displays") or []
    if displays and (displays[0].get("x"), displays[0].get("y")) != (0, 0):
        raise LookupError(f"primary display origin is {displays[0].get('x')},{displays[0].get('y')}, not 0,0")
    return round(rect["x"] + rect["w"] / 2), round(rect["y"] + rect["h"] / 2)


# ── Input ────────────────────────────────────────────────────────────────────


class XTest:
    """XTEST input through xdotool, only on the display this run started."""

    name = "xtest"

    def __init__(self, display: str, guard: Callable[[], None]) -> None:
        self.display = display
        self.guard = guard
        self.calls = 0

    def _run(self, *args: object) -> None:
        self.guard()
        env = dict(os.environ, DISPLAY=self.display)
        subprocess.run(["xdotool", *map(str, args)], env=env, check=True, capture_output=True)
        self.calls += 1  # logged, never an assertion

    def move_abs(self, x: int, y: int) -> None:
        self._run("mousemove", x, y)

    def button(self, down: bool, button: int = 1) -> None:
        self._run("mousedown" if down else "mouseup", button)

    def click(self, button: int = 1, repeat: int = 1) -> None:
        self._run("click", "--repeat", repeat, "--delay", 15 if repeat < 50 else 3, button)

    def key(self, chord: str) -> None:
        self._run("key", "--delay", 30, chord)

    def type(self, text: str) -> None:
        self._run("type", "--delay", 30, text)


# ── Owned processes ──────────────────────────────────────────────────────────


def start_xvfb(first: int = 90, last: int = 199) -> tuple[subprocess.Popen, str]:
    """Start Xvfb on the first free display number; return it once it is ready.

    Xvfb's own `-displayfd` search fails where the socket directory is
    read-only (WSLg mounts /tmp/.X11-unix that way; the abstract socket still
    works), so numbers are tried here and Xvfb's lock file settles races
    between parallel runs. `-displayfd` still reports readiness.
    """
    for number in range(first, last + 1):
        if Path(f"/tmp/.X{number}-lock").exists():
            continue
        read_fd, write_fd = os.pipe()
        proc = subprocess.Popen(
            ["Xvfb", f":{number}", "-displayfd", str(write_fd), "-screen", "0",
             f"{SCREEN[0]}x{SCREEN[1]}x24", "-nolisten", "tcp"],
            pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.close(write_fd)
        with os.fdopen(read_fd) as pipe:
            ready, _, _ = select.select([pipe], [], [], 15)
            reported = pipe.readline().strip() if ready else ""  # empty when Xvfb exits (display taken)
        if not ready:
            proc.kill()
        if reported == str(number):
            return proc, f":{number}"
        proc.wait()
    raise RuntimeError(f"no free X display in :{first}..:{last}")


def free_port() -> int:
    """A free loopback port for --mcp-port (port 0 there disables MCP)."""
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


@dataclass
class Run:
    exe: Path
    out: Path
    display: str = ""
    port: int = 0
    psk: str = field(default_factory=lambda: secrets.token_hex(16))
    xvfb: subprocess.Popen | None = None
    hud: subprocess.Popen | None = None
    owned_pids: list[int] = field(default_factory=list)
    home: Path | None = None
    config_dir: Path | None = None
    log: Path | None = None
    smoke: Smoke | None = None
    xtest: XTest | None = None
    timeline: list[dict] = field(default_factory=list)

    @property
    def pid(self) -> int | None:
        return self.owned_pids[-1] if self.owned_pids else None

    # Lifecycle ───────────────────────────────────────────────────────────

    def start(self) -> None:
        self.xvfb, self.display = start_xvfb()
        self.port = free_port()
        self.home = Path(tempfile.mkdtemp(prefix="hud_gestures_home_"))
        (self.home / ".config").mkdir()
        config = seed_config(CONFIG, self.psk)
        self.config_dir = config.parent
        self.log = self.out / "hud.log"
        env = dict(
            os.environ, DISPLAY=self.display, WAYLAND_DISPLAY="", HOME=str(self.home),
            XDG_CONFIG_HOME=str(self.home / ".config"), TZE_HUD_LOG="info",
        )
        if LAVAPIPE.exists():
            env["VK_ICD_FILENAMES"] = str(LAVAPIPE)
        with open(self.log, "wb") as log:
            self.hud = subprocess.Popen(
                [str(self.exe), "--config", str(config), "--window-mode", "fullscreen",
                 "--width", str(SCREEN[0]), "--height", str(SCREEN[1]),
                 "--grpc-port", "0", "--mcp-port", str(self.port)],
                # The runtime writes a relative diagnostic file on Linux; keep it out of the repo.
                env=env, cwd=self.home, stdout=log, stderr=subprocess.STDOUT,
            )
        self.owned_pids.append(self.hud.pid)
        self.smoke = Smoke(f"http://127.0.0.1:{self.port}/mcp", self.psk)
        wait_for_mcp(self.smoke, self.hud, 60)
        self.xtest = XTest(self.display, self.guard)
        self.guard()

    def guard(self) -> None:
        if not self.xvfb or self.xvfb.poll() is not None:
            raise Refused("the harness's Xvfb is gone; its display number may be reused")
        status = self.status()
        check_owned(started_display=self.display, target_display=self.xtest.display if self.xtest else self.display,
                    owned_pid=self.pid if self.pid and pid_alive(self.pid) else None,
                    status_pid=status.get("pid"), url=self.smoke.url)

    def stop(self) -> list[str]:
        """Terminate everything this run started; return what survived."""
        survivors = []
        if self.hud and self.hud.poll() is None:
            self.hud.terminate()
            try:
                self.hud.wait(15)
            except subprocess.TimeoutExpired:
                self.hud.kill()
        for pid in self.owned_pids:
            if pid_alive(pid):
                kill_pid(pid)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and any(pid_alive(p) for p in self.owned_pids):
            time.sleep(0.1)
        if self.hud:
            self.hud.poll()  # reap our direct child
        survivors += [f"hud pid {p}" for p in self.owned_pids if pid_alive(p) and not self._zombie(p)]
        # A replacement HUD that never answered was never recorded; find it by
        # this run's private HOME, which nothing else uses.
        for pid in self._strays():
            kill_pid(pid)
            survivors.append(f"unrecorded hud pid {pid}")
        if self.xvfb:
            self.xvfb.terminate()
            try:
                self.xvfb.wait(10)
            except subprocess.TimeoutExpired:
                self.xvfb.kill()
                survivors.append(f"Xvfb pid {self.xvfb.pid}")
        for path in (self.home, self.config_dir):
            if path:
                shutil.rmtree(path, ignore_errors=True)
        return survivors

    def _strays(self) -> list[int]:
        if not self.home:
            return []
        marker = f"HOME={self.home}".encode() + b"\0"
        strays = []
        for proc in Path("/proc").iterdir():
            if not proc.name.isdigit() or int(proc.name) in self.owned_pids or int(proc.name) == os.getpid():
                continue
            try:
                if marker in (proc / "environ").read_bytes() and not self._zombie(int(proc.name)):
                    strays.append(int(proc.name))
            except OSError:
                continue
        return strays

    @staticmethod
    def _zombie(pid: int) -> bool:
        try:
            return Path(f"/proc/{pid}/stat").read_text().split()[2] == "Z"
        except OSError:
            return False

    # Observation ─────────────────────────────────────────────────────────

    def status(self) -> dict:
        return json.loads(self.smoke.get("/admin/status?cpu=0")[1])

    def portal(self, surface: str) -> dict | None:
        return portal_entry(self.status(), surface)

    def mark(self) -> None:
        """Remember the presented-frame count before an action that will render."""
        self._since = self.status().get("frames_presented") or 0

    def settle(self, timeout_s: float = SETTLE_S) -> dict:
        """Wait for a frame after the last action, then SETTLED_IDLE_MS of quiet.

        Requiring a fresh frame keeps idleness from before the action (its
        frame not yet presented) from passing as settled.
        """
        since = getattr(self, "_since", None)

        last: dict = {}

        def probe():
            nonlocal last
            last = self.status()
            fresh = since is None or (last.get("frames_presented") or 0) > since
            return fresh and (last.get("idle_since_ms") or 0) >= SETTLED_IDLE_MS, None
        try:
            wait_until(probe, what="HUD settles", timeout_s=timeout_s, interval_s=0.05)
        except AssertionError:
            raise ScenarioFailed("settle", {k: last.get(k) for k in ("frames_presented", "idle_since_ms", "portals")}
                                 | {"frames_before_action": since}) from None
        status = last
        self._since = None
        return status

    def expect(self, what: str, check: Callable[[dict], bool], timeout_s: float = SETTLE_S) -> dict:
        """Wait for a status in which `check` holds; fail with the last one seen."""
        last: dict = {}

        def probe():
            nonlocal last
            last = self.status()
            try:
                return bool(check(last)), last
            except (KeyError, TypeError, LookupError):
                return False, last
        try:
            wait_until(probe, what=what, timeout_s=timeout_s, interval_s=0.05)
        except AssertionError:
            raise ScenarioFailed(what, {"portals": last.get("portals"), "idle_since_ms": last.get("idle_since_ms")}) from None
        self.step(f"ok {what}")
        return last

    def step(self, what: str) -> None:
        self.timeline.append({"t": round(time.monotonic(), 3), "step": what})

    # Gestures ────────────────────────────────────────────────────────────

    def point(self, target: str) -> tuple[int, int]:
        self.mark()
        x, y = resolve_target(self.status(), target)
        self.xtest.move_abs(x, y)
        self.step(f"point {target} {x},{y}")
        return x, y

    def click(self, target: str | None = None) -> None:
        if target:
            self.point(target)
        else:
            self.mark()
        self.xtest.click()
        self.step(f"click {target or ''}".rstrip())

    def wheel(self, target: str, notches: int) -> None:
        """Positive notches scroll toward the start (wheel up)."""
        self.point(target)
        self.xtest.click(4 if notches > 0 else 5, repeat=abs(notches))
        self.step(f"wheel {target} {notches}")

    def drag(self, start: tuple[int, int], end: tuple[int, int], *, release: bool = True, steps: int = 8) -> None:
        self.mark()
        self.xtest.move_abs(*start)
        self.xtest.button(True)
        for i in range(1, steps + 1):
            self.xtest.move_abs(start[0] + (end[0] - start[0]) * i // steps,
                                start[1] + (end[1] - start[1]) * i // steps)
        if release:
            self.xtest.button(False)
        self.step(f"drag {start} -> {end}{'' if release else ' (held)'}")

    def type(self, text: str) -> None:
        self.mark()
        self.xtest.type(text)
        self.step(f"type {len(text)} chars")

    def key(self, chord: str) -> None:
        self.mark()
        self.xtest.key(chord)
        self.step(f"key {chord}")

    # MCP ─────────────────────────────────────────────────────────────────

    def publish(self, surface: str, **args) -> dict:
        self.mark()
        reply = self.smoke.ok("hud_publish", {"surface": surface, **args})
        self.step(f"publish {surface}")
        return reply

    def inputs(self, wait_ms: int = 0) -> list[dict]:
        """Take and ack queued hud_input items."""
        reply = self.smoke.ok("hud_input", {"wait_ms": wait_ms, "max_items": 20}, timeout_s=10 + wait_ms / 1000)
        items = reply.get("items") or []
        if items:
            self.smoke.ok("hud_input", {"ack": [i["id"] for i in items]})
        return items

    def take(self, count: int, timeout_s: float = SETTLE_S) -> list[dict]:
        """Take and ack replies until `count` arrived or `timeout_s` passed (one bound, no retry)."""
        deadline = time.monotonic() + timeout_s
        items: list[dict] = []
        while len(items) < count:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            items += self.inputs(wait_ms=int(min(left, 2.0) * 1000))
        return items

    def clear_portals(self) -> list[dict]:
        """Detach every portal this agent holds; return input nobody took (a failure)."""
        stray = self.inputs()
        for entry in self.smoke.ok("hud_surfaces").get("surfaces", []):
            if entry.get("s", "").startswith("portal:") and entry.get("state") not in {None, "detached"}:
                self.smoke.call("hud_clear", {"surface": entry["s"]})
        return stray

    # Evidence ────────────────────────────────────────────────────────────

    def snapshot(self, name: str) -> dict:
        status = self.status()
        (self.out / f"{name}.status.json").write_text(json.dumps(status, indent=1))
        try:
            status_code, ctype, png = self.smoke.get_bytes("/admin/screenshot")
            if status_code == 200 and ctype == "image/png":
                (self.out / f"{name}.png").write_bytes(png)
        except (AssertionError, RequestFailure):
            pass
        return status

    def screen_grab(self, name: str) -> None:
        if shutil.which("ffmpeg"):
            subprocess.run(
                ["ffmpeg", "-loglevel", "error", "-f", "x11grab", "-video_size", f"{SCREEN[0]}x{SCREEN[1]}",
                 "-i", self.display, "-frames:v", "1", "-y", str(self.out / f"{name}.x11.png")],
                capture_output=True,
            )

    def frame(self) -> tuple[int, int, bytes]:
        return capture_frame(self.smoke)[0]


def region(frame: tuple[int, int, bytes], rect: dict, inset: int = 2) -> list[bytes]:
    """RGBA rows of `rect` (scene pixels) inside a decoded frame."""
    width, height, rgba = frame
    x0, y0 = max(0, int(rect["x"]) + inset), max(0, int(rect["y"]) + inset)
    x1 = min(width, int(rect["x"] + rect["w"]) - inset)
    y1 = min(height, int(rect["y"] + rect["h"]) - inset)
    return [rgba[(y * width + x0) * 4:(y * width + x1) * 4] for y in range(y0, y1)]


# ── Scenarios ────────────────────────────────────────────────────────────────

LINES = 60


def lines(prefix: str, start: int, count: int, pad: int = 0) -> str:
    return "\n".join(f"{prefix}-{i:03d}" + (" " + "x" * pad if pad else "") for i in range(start, start + count))


def first_line(portal: dict) -> str | None:
    return portal["output"]["first_visible_line"]


def tail_line(prefix: str, index: int) -> Callable[[dict], bool]:
    return lambda p: (p["output"]["last_visible_line"] or "").startswith(f"{prefix}-{index:03d}")


def on(surface: str, check: Callable[[dict], bool]) -> Callable[[dict], bool]:
    return lambda status: check(portal_entry(status, surface))


def open_portal(run: Run, surface: str, prefix: str, count: int = LINES, **args) -> None:
    run.publish(surface, content=lines(prefix, 0, count), ttl_ms=0, **args)
    run.expect(f"{surface} shows its tail", on(surface, lambda p: p["output"]["at_tail"] and tail_line(prefix, count - 1)(p)))
    run.settle()


def scenario_output_anchor(run: Run) -> None:
    """1. OUTPUT keeps its visible line through appends and head-trim."""
    s = "portal:g-anchor"
    # Each publish stays under the 4 KiB a single unit renders; six of them
    # cross the 16 KiB retention bound.
    chunk = 120
    publish = lambda n: run.publish(  # noqa: E731
        s, content=("\n" if n else "") + lines("a", n * chunk, chunk, pad=20), ttl_ms=0)
    for n in range(4):
        publish(n)
    run.expect("tail shown", on(s, lambda p: p["output"]["at_tail"] and tail_line("a", 4 * chunk - 1)(p)))
    run.settle()
    run.wheel(f"{s}/output", 2)
    run.expect("scrolled back", on(s, lambda p: p["output"]["at_tail"] is False))
    run.settle()
    anchor = first_line(portal_entry(run.status(), s))
    if not anchor:
        raise ScenarioFailed("a visible line to anchor on", {"anchor": anchor})
    publish(4)
    run.expect("append keeps the visible line", on(s, lambda p: p["output"]["unread"] > 0 and first_line(p) == anchor))
    run.settle()
    publish(5)
    run.settle()
    run.expect("head-trim keeps the visible line", on(s, lambda p: first_line(p) == anchor))
    # Wheel to the very top, then prove it is the top: ten more notches up and
    # one down, which must land one notch below `top` (X delivers in order, so
    # seeing the down proves the ups were handled), then one up returns to it.
    run.wheel(f"{s}/output", 900)
    top = first_line(portal_entry(run.settle(), s))
    run.wheel(f"{s}/output", 10)
    run.wheel(f"{s}/output", -1)
    run.expect("one notch below the top", on(s, lambda p: first_line(p) != top))
    run.wheel(f"{s}/output", 1)
    run.expect("back at the same top", on(s, lambda p: first_line(p) == top))
    if not top or not top.startswith("a-") or top.startswith("a-000"):
        raise ScenarioFailed("the head was trimmed past the retention bound", {"top": top})
    run.step(f"ok head trimmed: top is {top.split()[0]}")


def scenario_independent_scroll(run: Run) -> None:
    """2. INPUT and OUTPUT scroll independently; typing and submit reset only INPUT."""
    s = "portal:g-panes"
    open_portal(run, s, "p", expects_reply=True)
    header = portal_entry(run.status(), s)["panes"]["header"]
    run.click(f"{s}/composer")
    run.expect("composer focused", on(s, lambda p: p["focus"] == "composer"))
    sent = 0
    for i in range(30):  # INPUT holds the sent-reply history: overflow it
        run.type(f"reply {i:02d}")
        run.key("Return")
        if i % 10 == 9:
            sent += len(run.take(10))
    if sent != 30:
        raise ScenarioFailed("30 replies reach hud_input", {"sent": sent})
    run.expect("INPUT history at its tail", on(s, lambda p: p["input"]["at_tail"] is True))
    run.settle()
    run.wheel(f"{s}/input", 5)
    run.expect("INPUT scrolled, OUTPUT untouched",
               on(s, lambda p: p["input"]["at_tail"] is False and p["output"]["at_tail"] is True))
    run.wheel(f"{s}/output", 3)
    run.expect("OUTPUT scrolled, INPUT kept",
               on(s, lambda p: p["output"]["at_tail"] is False and p["input"]["at_tail"] is False))
    run.type("z")
    run.expect("typing resets only INPUT",
               on(s, lambda p: p["input"]["at_tail"] is True and p["output"]["at_tail"] is False))
    run.wheel(f"{s}/input", 5)
    run.expect("INPUT scrolled again", on(s, lambda p: p["input"]["at_tail"] is False))
    run.key("Return")
    if [i.get("text") for i in run.take(1)] != ["z"]:
        raise ScenarioFailed("submit sends the draft", {})
    run.expect("submit resets only INPUT",
               on(s, lambda p: p["input"]["at_tail"] is True and p["output"]["at_tail"] is False))
    run.expect("header stays fixed", on(s, lambda p: p["panes"]["header"] == header))


def scenario_unread(run: Run) -> None:
    """3. Unread accumulates while scrolled back, clears at the tail, never goes stale."""
    s = "portal:g-unread"
    open_portal(run, s, "u")
    run.wheel(f"{s}/output", 3)
    run.expect("scrolled back, nothing unread",
               on(s, lambda p: p["output"]["at_tail"] is False and p["output"]["unread"] == 0
                  and p["panes"]["unread_badge"] is None))
    run.publish(s, content="\n" + lines("u", LINES, 5))
    run.expect("unread counted and badged",
               on(s, lambda p: p["output"]["unread"] > 0 and p["panes"]["unread_badge"] is not None))
    run.wheel(f"{s}/output", -60)
    run.expect("tail clears unread",
               on(s, lambda p: p["output"]["at_tail"] and p["output"]["unread"] == 0
                  and p["panes"]["unread_badge"] is None))
    run.wheel(f"{s}/output", 3)
    run.expect("backscroll over read output stays clear",
               on(s, lambda p: p["output"]["at_tail"] is False and p["output"]["unread"] == 0
                  and p["panes"]["unread_badge"] is None))


def scenario_jump_to_latest(run: Run) -> None:
    """4. The badge click and wheel-to-tail both reach the tail and clear unread."""
    s = "portal:g-jump"
    open_portal(run, s, "j")
    for n, finish in enumerate(("badge", "wheel")):
        run.wheel(f"{s}/output", 3)
        run.publish(s, content="\n" + lines("j", LINES + 3 * n, 3))
        run.expect(f"badge shown before {finish}", on(s, lambda p: p["panes"]["unread_badge"] is not None))
        run.settle()
        if finish == "badge":
            run.click(f"{s}/unread_badge")
        else:
            run.wheel(f"{s}/output", -60)
        run.expect(f"{finish} reaches the tail and clears unread",
                   on(s, lambda p: p["output"]["at_tail"] and p["output"]["unread"] == 0
                      and tail_line("j", LINES + 3 * n + 2)(p)))


def scenario_badge_opaque(run: Run) -> None:
    """5. The unread badge is opaque: text scrolling under it never shows through."""
    s = "portal:g-badge"
    open_portal(run, s, "b")
    run.wheel(f"{s}/output", 3)
    run.publish(s, content="\n" + lines("b", LINES, 4))
    status = run.expect("badge shown", on(s, lambda p: p["panes"]["unread_badge"] is not None))
    run.settle()
    badge = portal_entry(run.status(), s)["panes"]["unread_badge"]
    before = run.frame()
    run.wheel(f"{s}/output", 1)
    run.expect("text under the badge moved, badge kept",
               on(s, lambda p: p["panes"]["unread_badge"] == badge and first_line(p) != first_line(portal_entry(status, s))))
    run.settle()
    after = run.frame()
    changed = sum(a != b for a, b in zip(region(before, badge), region(after, badge)))
    if changed:
        raise ScenarioFailed("badge pixels stay identical while text scrolls beneath", {"changed_rows": changed, "badge": badge})
    output = portal_entry(run.status(), s)["panes"]["output"]
    if region(before, output) == region(after, output):
        raise ScenarioFailed("OUTPUT pixels changed with the scroll (control)", {"output": output})
    run.step("ok badge rows identical across a scroll; OUTPUT rows changed")


def scenario_composer(run: Run) -> None:
    """6. Hover, click focus, local echo, exact submit, a second send, typing while output streams."""
    s = "portal:g-composer"
    open_portal(run, s, "c", count=10, expects_reply=True)
    run.point(f"{s}/composer")
    run.expect("hover feedback", on(s, lambda p: p["hover"] == "composer"))
    run.click()
    run.expect("click focuses the composer", on(s, lambda p: p["focus"] == "composer"))
    text = "hello gesture"
    run.type(text)
    # Local echo: the draft is there before any MCP round trip is made.
    run.expect("local echo", on(s, lambda p: p["input"]["draft_chars"] == len(text)))
    run.key("Return")
    items = run.take(1)
    if [i.get("text") for i in items] != [text]:
        raise ScenarioFailed("Enter submits the exact text", {"items": items})
    run.expect("submit empties the draft", on(s, lambda p: p["input"]["draft_chars"] == 0))
    run.type("second send")
    run.key("Return")
    items = run.take(1)
    if [i.get("text") for i in items] != ["second send"]:
        raise ScenarioFailed("a second send without re-clicking", {"items": items})
    run.type("abc")
    run.expect("draft typed", on(s, lambda p: p["input"]["draft_chars"] == 3))
    for n in range(3):
        run.publish(s, content=f"\nstreamed {n}")
    run.expect("draft survives streamed output",
               on(s, lambda p: p["focus"] == "composer" and p["input"]["draft_chars"] == 3
                  and (p["output"]["last_visible_line"] or "").startswith("streamed 2")))


def scenario_keyboard(run: Run) -> None:
    """7. Tab reaches the composer, Shift+Tab stays in the portal, Enter submits."""
    s = "portal:g-keys"
    open_portal(run, s, "k", count=10, expects_reply=True)
    run.expect("starts unfocused", on(s, lambda p: p["focus"] == "none"))
    run.key("Tab")
    run.expect("Tab focuses the composer", on(s, lambda p: p["focus"] == "composer"))
    run.key("shift+Tab")
    run.type("k k")  # typing after Shift+Tab proves the key was handled
    run.expect("Shift+Tab keeps the composer; Space types into the draft",
               on(s, lambda p: p["focus"] == "composer" and p["input"]["draft_chars"] == 3))
    run.key("Return")
    items = run.take(1)
    if [i.get("text") for i in items] != ["k k"]:
        raise ScenarioFailed("keyboard-only submit", {"items": items})


def shrink(run: Run, s: str, times: int = 4) -> dict:
    run.click(f"{s}/composer")
    run.expect("composer focused", on(s, lambda p: p["focus"] == "composer"))
    before = portal_entry(run.status(), s)["rect"]
    for _ in range(times):
        run.key("ctrl+minus")
    return run.expect("Ctrl+- shrinks the portal",
                      on(s, lambda p: p["rect"]["w"] < before["w"] or p["rect"]["h"] < before["h"]))


def scenario_drag_alignment(run: Run) -> None:
    """8. After a header drag the composer's hit region moves with its pixels."""
    s = "portal:g-drag"
    open_portal(run, s, "d", count=10, expects_reply=True)
    shrink(run, s)
    run.settle()
    before = portal_entry(run.status(), s)
    hx, hy = resolve_target(run.status(), f"{s}/header")
    run.drag((hx, hy), (hx + 120, hy + 60))
    # The drag threshold takes the first few pixels; the rest moves the portal.
    run.expect("header drag moves the portal",
               on(s, lambda p: p["rect"]["x"] - before["rect"]["x"] >= 100 and p["rect"]["y"] - before["rect"]["y"] >= 40))
    moved = portal_entry(run.settle(), s)
    dx, dy = moved["rect"]["x"] - before["rect"]["x"], moved["rect"]["y"] - before["rect"]["y"]
    composer = moved["panes"]["composer"]
    if (composer["x"] - before["panes"]["composer"]["x"], composer["y"] - before["panes"]["composer"]["y"]) != (dx, dy):
        raise ScenarioFailed("composer rect moved with the portal", {"before": before["panes"], "after": moved["panes"]})
    # Just inside the old composer's top-left, now outside the moved one.
    # Hover the moved composer first, so the old spot's "none" is a change.
    run.point(f"{s}/composer")
    run.expect("the moved composer hovers", on(s, lambda p: p["hover"] == "composer"))
    old = before["panes"]["composer"]
    run.xtest.move_abs(int(old["x"]) + 4, int(old["y"]) + 4)
    run.expect("old composer location no longer hovers it", on(s, lambda p: p["hover"] == "none"))
    run.point(f"{s}/composer")
    run.click()
    run.type("moved")
    run.expect("the moved composer takes typing", on(s, lambda p: p["focus"] == "composer" and p["input"]["draft_chars"] == 5))


def scenario_drag_release(run: Run) -> None:
    """9. The portal follows a held drag and drops on the real mouse-up."""
    s = "portal:g-drop"
    open_portal(run, s, "r", count=10, expects_reply=True)
    shrink(run, s)
    run.settle()
    rect = portal_entry(run.status(), s)["rect"]
    hx, hy = resolve_target(run.status(), f"{s}/header")
    run.drag((hx, hy), (hx + 100, hy + 40), release=False)
    run.expect("follows while held", on(s, lambda p: p["rect"]["x"] - rect["x"] >= 80))
    run.xtest.button(False)
    run.step("mouse-up")
    run.settle()
    dropped = portal_entry(run.status(), s)["rect"]
    run.xtest.move_abs(hx + 300, hy + 140)
    run.step("move after release")
    # A drag still held would carry the portal along; hovering its composer
    # proves the move was handled.
    run.point(f"{s}/composer")
    run.expect("stays dropped after release", on(s, lambda p: p["rect"] == dropped and p["hover"] == "composer"))


def scenario_lifecycle(run: Run) -> None:
    """13. Zone TTL expiry, hud_hold, and a portal's degraded -> active status."""
    held = lambda name: next((e.get("held", False) for e in run.smoke.ok("hud_surfaces")["surfaces"] if e["s"] == name), False)  # noqa: E731
    zone = "zone:subtitle"
    run.publish(zone, content="ttl probe", ttl_ms=1500)
    if not held(zone):
        raise ScenarioFailed("published zone is held", {})
    wait_until(lambda: (not held(zone), None), what="zone TTL expiry", timeout_s=SETTLE_S)
    run.step("ok zone expired")
    run.publish(zone, content="hold probe", ttl_ms=1500)
    started = time.monotonic()
    run.smoke.ok("hud_hold", {"surface": zone, "ttl_ms": 4000})
    wait_until(lambda: (not held(zone), None), what="held zone expiry", timeout_s=4 + SETTLE_S)
    lasted = time.monotonic() - started
    if lasted < 3.0:  # the original 1.5 s TTL would have ended well before
        raise ScenarioFailed("hud_hold outlives the original TTL", {"lasted_s": round(lasted, 2)})
    run.step(f"ok hold outlived the TTL ({lasted:.1f}s)")
    s = "portal:g-life"
    open_portal(run, s, "l", count=5)
    run.publish(s, status="degraded")
    run.expect("degraded status shown", on(s, lambda p: p["state"] == "Degraded"))
    run.publish(s, status="active")
    run.expect("recovers to active", on(s, lambda p: p["state"] == "Active"))


def scenario_restart(run: Run) -> None:
    """14. Across a restart the client sees unavailable, rediscovers, and nothing replays."""
    s = "portal:g-restart"
    open_portal(run, s, "x", count=5)
    old = run.pid
    status, _, _ = run.smoke.request("POST", "/admin/restart")
    if status != 202:
        raise ScenarioFailed("POST /admin/restart accepted", {"status": status})

    def new_instance():
        try:
            status = run.status()
        except (RequestFailure, AssertionError, ValueError):
            return False, None
        return status.get("pid") != old, status
    status, _ = wait_until(new_instance, what="replacement HUD answers", timeout_s=60, interval_s=0.25)
    run.owned_pids.append(status["pid"])
    run.guard()
    try:
        run.hud.wait(30)
    except subprocess.TimeoutExpired:
        raise ScenarioFailed("old instance exits", {"pid": old}) from None
    surfaces = run.smoke.ok("hud_surfaces")["surfaces"]
    if any(e["s"] == s and e.get("state") not in {None, "detached"} for e in surfaces):
        raise ScenarioFailed("the restarted HUD holds no portal until republished", {"surfaces": surfaces})
    run.publish(s, content="after restart")
    run.expect("republish shows only new content",
               on(s, lambda p: (p["output"]["first_visible_line"] or "").startswith("after restart")))


def scenario_resize_hotkey(run: Run) -> None:
    """16. Ctrl+- / Ctrl+= on a focused portal change its rect."""
    s = "portal:g-resize"
    open_portal(run, s, "z", count=5, expects_reply=True)
    shrunk = portal_entry(shrink(run, s, times=2), s)["rect"]
    run.key("ctrl+equal")
    run.expect("Ctrl+= grows the portal", on(s, lambda p: p["rect"]["w"] > shrunk["w"] or p["rect"]["h"] > shrunk["h"]))


SCENARIOS: dict[str, Callable[[Run], None]] = {
    "output-anchor": scenario_output_anchor,          # 1
    "independent-scroll": scenario_independent_scroll,  # 2
    "unread": scenario_unread,                        # 3
    "jump-to-latest": scenario_jump_to_latest,        # 4
    "badge-opaque": scenario_badge_opaque,            # 5
    "composer": scenario_composer,                    # 6
    "keyboard": scenario_keyboard,                    # 7
    "drag-alignment": scenario_drag_alignment,        # 8
    "drag-release": scenario_drag_release,            # 9
    "lifecycle": scenario_lifecycle,                  # 13
    "resize-hotkey": scenario_resize_hotkey,          # 16
    "restart": scenario_restart,                      # 14 (last: replaces the HUD)
}


# ── Runner ───────────────────────────────────────────────────────────────────


def git_sha() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "--short=12", "HEAD"], cwd=REPO, capture_output=True,
                              text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


def run_scenarios(run: Run, names: list[str]) -> list[dict]:
    results = []
    for name in names:
        run.step(f"begin {name}")
        started = time.monotonic()
        run.snapshot(f"{name}.before")
        result = {"scenario": name, "doc": (SCENARIOS[name].__doc__ or "").strip()}
        try:
            SCENARIOS[name](run)
            result["outcome"] = "PASS"
        except ScenarioFailed as failure:
            result.update(outcome="FAIL", step=failure.step, last=failure.last)
        except Refused:
            raise
        except Exception as err:  # noqa: BLE001 - every scenario reports, the run continues
            result.update(outcome="FAIL", step=f"{type(err).__name__}: {err}", last=None)
        result["seconds"] = round(time.monotonic() - started, 2)
        if result["outcome"] == "FAIL":
            run.screen_grab(f"{name}.after")
        try:
            run.snapshot(f"{name}.after")
            stray = run.clear_portals()
            if stray and result["outcome"] == "PASS":
                result.update(outcome="FAIL", step="input nobody took (duplicate or stray submit)",
                              last={"items": [i.get("text") for i in stray]})
        except (AssertionError, RequestFailure) as err:
            result.setdefault("cleanup", str(err))
        result["artifacts"] = str(run.out / name) + ".*"
        results.append(result)
        line = f"{result['outcome']}  {name} ({result['seconds']}s)"
        if result["outcome"] == "FAIL":
            line += (f"\n      step: {result['step']}\n      last: {json.dumps(result['last'])[:600]}"
                     f"\n      artifacts: {result['artifacts']}")
        print(line, flush=True)
    return results


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--exe", type=Path, required=True, help="the Linux tze_hud binary to own")
    parser.add_argument("--out", type=Path, help="artifact dir (default test_results/gestures/<sha>/<run>)")
    parser.add_argument("--list", action="store_true", help="list scenarios and exit")
    parser.add_argument("scenarios", nargs="*", help="scenario names (default: all)")
    args = parser.parse_args(argv)
    if args.list:
        for name, fn in SCENARIOS.items():
            print(f"{name:20} {(fn.__doc__ or '').strip().splitlines()[0]}")
        return 0
    unknown = [n for n in args.scenarios if n not in SCENARIOS]
    if unknown:
        parser.error(f"unknown scenario(s): {', '.join(unknown)}; see --list")
    names = args.scenarios or list(SCENARIOS)
    if "restart" in names:  # it replaces the HUD; keep it last
        names = [n for n in names if n != "restart"] + ["restart"]
    sha = git_sha()
    # The pid keeps parallel runs started in the same second apart.
    out = args.out or REPO / "test_results/gestures" / sha / f"{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
    out.mkdir(parents=True, exist_ok=True)
    run = Run(exe=args.exe.resolve(), out=out)
    started = time.monotonic()
    results: list[dict] = []
    failure = None
    try:
        run.start()
        status = run.status()
        run.step(f"owned HUD pid {run.pid} on display {run.display}")
        results = run_scenarios(run, names)
        unacked = run.smoke.ok("hud_input", {"max_items": 0}).get("remaining", 0)
        if unacked:
            failure = f"{unacked} un-acked hud_input item(s) at exit"
    except Refused as err:
        failure = f"refused: {err}"
    except Exception as err:  # noqa: BLE001
        failure = f"harness: {type(err).__name__}: {err}"
    finally:
        survivors = run.stop() if run.xvfb else []
        if survivors:
            failure = (failure + "; " if failure else "") + "left running: " + ", ".join(survivors)
        timeline = {
            "sha": sha, "profile": run.exe.parent.name,
            "exe": str(run.exe), "injector": XTest.name, "screen": SCREEN,
            "display": run.display, "mcp_port": run.port,
            "geometry": (status.get("displays") if "status" in locals() else None),
            "theme_scale": ({k: status.get(k) for k in ("theme", "ui_scale") if k in status} if "status" in locals() else None),
            "seconds": round(time.monotonic() - started, 2), "results": results, "failure": failure,
            "steps": run.timeline,
        }
        (out / "timeline.json").write_text(json.dumps(timeline, indent=1))
        if run.log and run.log.exists():
            tail = run.log.read_text(errors="replace").splitlines()[-200:]
            (out / "hud.log.tail").write_text("\n".join(tail))
    passed = sum(r["outcome"] == "PASS" for r in results)
    print(f"{passed}/{len(results)} scenarios passed in {time.monotonic() - started:.1f}s; artifacts: {out}")
    if failure:
        print(f"FAIL harness: {failure}")
    return 0 if failure is None and passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
