#!/usr/bin/env python3
"""Drive a real tze_hud with real OS input (T13 tiers L and W).

Tier L (`--injector xtest`, Linux): each run starts its own Xvfb and plays
scenarios through XTEST (`xdotool`). Tier W (`--injector sendinput`,
Windows): only on a GitHub-hosted VM, through Win32 `SendInput`
(`hud_sendinput.py`), as an informational step in `windows.yml`. Either way
the run starts its own HUD from `--exe` with a freshly seeded config, an
ephemeral loopback MCP port and gRPC disabled, and asserts on state:
the `/admin/status` surface blocks and MCP results. Pixels are checked only
inside rects that status reports. Readiness is observed (`idle_since_ms`),
never slept for, and nothing is retried.

Owned mode only. Input is injected only while `/admin/status` reports the
pid of the HUD this run launched, and only into the Xvfb this run started
(L) or while an owned process has the foreground window on a CI VM (W).

    just gesture                      # build, then every scenario
    just gesture composer unread      # selected scenarios
    python3 scripts/ci/hud_gestures.py --exe target/debug/tze_hud [scenario...]

Artifacts land in `test_results/gestures/<sha>/<run>/` (gitignored): status
JSON and runtime screenshots before/after each scenario, an X screen grab and
the log tail on failure, and `timeline.json`.

Stdlib only (plus `xdotool`, `Xvfb` and optional `ffmpeg` for tier L).
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
    encode_png_rgba,
    kill_pid,
    seed_config,
    wait_for_mcp,
    wait_until,
)

WINDOWS = sys.platform == "win32"
if WINDOWS:
    import hud_sendinput as WIN  # noqa: E402,N812  (ctypes Win32; imported only where it loads)
else:
    WIN = None

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


def check_owned_windows(*, github_actions: str | None, foreground_pid: int, owned_hud_pid: int | None,
                        helper_pids: list[int], status_pid: object, url: str) -> None:
    """Refuse SendInput unless this is a CI VM and an owned process has the foreground."""
    host = urllib.parse.urlsplit(url).hostname
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise Refused(f"refusing a borrowed HUD URL (host {host!r}); this harness owns its HUD")
    if github_actions != "true":
        raise Refused("SendInput reaches the real desktop; it runs only on a CI VM (GITHUB_ACTIONS=true)")
    if owned_hud_pid is None:
        raise Refused("no owned HUD process")
    if status_pid != owned_hud_pid:
        raise Refused(f"/admin/status pid {status_pid!r} is not the owned HUD pid {owned_hud_pid}")
    if foreground_pid not in {owned_hud_pid, *helper_pids}:
        raise Refused(f"the foreground window belongs to pid {foreground_pid}, not an owned process")


# ── Geometry ─────────────────────────────────────────────────────────────────


def portal_entry(status: dict, surface: str) -> dict | None:
    return next((p for p in status.get("portals") or [] if p.get("s") == surface), None)


def notification_entry(status: dict, key: str, surface: str = "zone:notification-area") -> dict | None:
    return next((p for p in status.get("notifications") or []
                 if p.get("s") == surface and p.get("key") == key), None)


def widget_entry(status: dict, surface: str) -> dict | None:
    return next((p for p in status.get("widgets") or [] if p.get("s") == surface), None)


def resolve_target(status: dict, target: str) -> tuple[int, int]:
    """Centre of a reported portal pane, card control, or widget hover target.

    Card targets are `zone:<name>/<key>/<index|close>`; widget targets are
    `widget:<instance>/hover`. No geometry is inferred from pixels or layout.
    """
    surface, _, pane = target.partition("/")
    if surface.startswith("zone:"):
        key, sep, control = pane.rpartition("/")
        if not sep:
            key, control = pane, ""
        card = notification_entry(status, key, surface)
        if card is None:
            raise LookupError(f"{surface}/{key} is not on screen")
        if control == "close":
            rect = card.get("close")
        elif control:
            try:
                index = int(control)
                if index < 0:
                    raise IndexError(index)
                rect = card["actions"][index]["rect"]
            except (KeyError, ValueError, IndexError):
                raise LookupError(f"{target} is not shown") from None
        else:
            rect = card["rect"]
    elif surface.startswith("widget:"):
        widget = widget_entry(status, surface)
        if widget is None:
            raise LookupError(f"{surface} is not on screen")
        rect = widget.get("hover_target") if pane == "hover" else widget["rect"]
    else:
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
    if WINDOWS:  # os.kill(pid, 0) would terminate it there
        return WIN.process_alive(pid)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def windows_huds() -> list[int]:
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq tze_hud.exe", "/FO", "CSV", "/NH"],
                         capture_output=True, text=True).stdout
    return [int(row.split('","')[1]) for row in out.splitlines() if row.startswith('"tze_hud.exe"')]


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
    injector: str = "xtest"
    window_mode: str = "fullscreen"
    inj: object | None = None  # XTest or hud_sendinput.SendInput
    helper_pids: list[int] = field(default_factory=list)  # owned non-HUD apps (scenario 10's Notepad)
    timeline: list[dict] = field(default_factory=list)
    publications: set[tuple[str, str | None]] = field(default_factory=set)

    @property
    def pid(self) -> int | None:
        return self.owned_pids[-1] if self.owned_pids else None

    # Lifecycle ───────────────────────────────────────────────────────────

    def start(self) -> None:
        if self.injector == "sendinput":
            if os.environ.get("GITHUB_ACTIONS") != "true":
                raise Refused("tier W drives the real desktop; it runs only on a CI VM (GITHUB_ACTIONS=true)")
            running = windows_huds()
            if running:  # it would hold the chords and the port handover; never touch it
                raise Refused(f"tze_hud.exe is already running (pid {running}); this harness owns its HUD")
        self.home = Path(tempfile.mkdtemp(prefix="hud_gestures_home_"))
        if self.injector == "xtest":
            self.xvfb, self.display = start_xvfb()
            (self.home / ".config").mkdir()
            env = dict(os.environ, DISPLAY=self.display, WAYLAND_DISPLAY="", HOME=str(self.home),
                       XDG_CONFIG_HOME=str(self.home / ".config"))
            if LAVAPIPE.exists():
                env["VK_ICD_FILENAMES"] = str(LAVAPIPE)
        else:
            for name in ("LocalAppData", "AppData"):
                (self.home / name).mkdir()
            env = dict(os.environ, LOCALAPPDATA=str(self.home / "LocalAppData"), APPDATA=str(self.home / "AppData"))
        for key in ("GH_TOKEN", "GITHUB_TOKEN"):
            env.pop(key, None)
        env["TZE_HUD_LOG"] = "info"
        env["TZE_HUD_LOG_JSON"] = "1"
        self.port = free_port()
        config = seed_config(CONFIG, self.psk)
        self.config_dir = config.parent
        self.log = self.out / "hud.log"
        dimensions = (["--width", str(SCREEN[0]), "--height", str(SCREEN[1])]
                      if self.window_mode == "fullscreen" else [])
        with open(self.log, "wb") as log:
            self.hud = subprocess.Popen(
                [str(self.exe), "--config", str(config), "--window-mode", self.window_mode,
                 *dimensions, "--grpc-port", "0", "--mcp-port", str(self.port)],
                # The runtime writes a relative diagnostic file on Linux; keep it out of the repo.
                env=env, cwd=self.home, stdout=log, stderr=subprocess.STDOUT,
            )
        self.owned_pids.append(self.hud.pid)
        self.smoke = Smoke(f"http://127.0.0.1:{self.port}/mcp", self.psk)
        wait_for_mcp(self.smoke, self.hud, 60)
        self.inj = XTest(self.display, self.guard) if self.injector == "xtest" else WIN.SendInput(self.guard)
        if self.injector == "sendinput":
            self._check_identity_mapping()
        if self.window_mode == "fullscreen":
            self.guard()
        else:
            # An overlay may start without activation. No input is sent until
            # scenario 11 foregrounds its owned helper and the normal guard passes.
            if self.status().get("pid") != self.pid:
                raise Refused("overlay status is not the owned HUD")

    def _check_identity_mapping(self) -> None:
        """Status rects are scene pixels; SendInput takes physical ones. Run only where they agree."""
        def reported():
            displays = self.status().get("displays") or []
            return bool(displays and displays[0].get("width")), displays
        try:
            displays, _ = wait_until(reported, what="displays reported", timeout_s=30, interval_s=0.2)
        except AssertionError:
            raise Refused("/admin/status never reported the primary display") from None
        primary = displays[0]
        physical = WIN.screen_size()
        scene = (primary.get("width"), primary.get("height"))
        if WIN.system_dpi() != 96 or scene != physical or (primary.get("x"), primary.get("y")) != (0, 0):
            raise Refused(f"scene-to-screen mapping is not identity (scene {scene}, screen {physical}, "
                          f"dpi {WIN.system_dpi()}); add a scaled mapping before running here")

    def guard(self) -> None:
        if self.injector == "sendinput":
            status = self.status()
            check_owned_windows(github_actions=os.environ.get("GITHUB_ACTIONS"),
                                foreground_pid=WIN.foreground_pid(),
                                owned_hud_pid=self.pid if self.pid and pid_alive(self.pid) else None,
                                helper_pids=[p for p in self.helper_pids if pid_alive(p)],
                                status_pid=status.get("pid"), url=self.smoke.url)
            return
        if not self.xvfb or self.xvfb.poll() is not None:
            raise Refused("the harness's Xvfb is gone; its display number may be reused")
        status = self.status()
        check_owned(started_display=self.display, target_display=self.inj.display if self.inj else self.display,
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
        for pid in self.owned_pids + self.helper_pids:
            if pid_alive(pid):
                kill_pid(pid)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and any(pid_alive(p) for p in self.owned_pids + self.helper_pids):
            time.sleep(0.1)
        if self.hud:
            self.hud.poll()  # reap our direct child
        survivors += [f"hud pid {p}" for p in self.owned_pids if pid_alive(p) and not self._zombie(p)]
        survivors += [f"helper pid {p}" for p in self.helper_pids if pid_alive(p)]
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
        if self.hud is None:  # never launched: nothing here is ours
            return []
        if WINDOWS:  # start() refused unless none ran before, so any left now is this run's
            return [pid for pid in windows_huds() if pid not in self.owned_pids]
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
            raise ScenarioFailed("settle", {k: last.get(k) for k in ("frames_presented", "idle_since_ms", "portals", "notifications", "widgets")}
                                 | {"frames_before_action": since}) from None
        status = last
        self._since = None
        return status

    def expect(self, what: str, check: Callable[[dict], bool], timeout_s: float = SETTLE_S,
               interval_s: float = 0.05) -> dict:
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
            wait_until(probe, what=what, timeout_s=timeout_s, interval_s=interval_s)
        except AssertionError:
            raise ScenarioFailed(what, {k: last.get(k) for k in ("portals", "notifications", "widgets", "link_launches", "idle_since_ms")}) from None
        self.step(f"ok {what}")
        return last

    def observe_for(self, what: str, check: Callable[[dict], bool], seconds: float) -> dict:
        """Require a state throughout a measured interval; status reads do not refresh portals.

        This observes a duration contract, rather than sleeping and assuming
        readiness. No publish, input poll, hold or injected event occurs here.
        """
        started = time.monotonic()
        deadline = started + seconds
        self.step(f"observe {what} for at least {seconds:.1f}s")
        while True:
            status = self.status()
            try:
                valid = bool(check(status))
            except (KeyError, TypeError, LookupError):
                valid = False
            if not valid:
                raise ScenarioFailed(what, {"elapsed_s": round(time.monotonic() - started, 3),
                                            "safe_mode": status.get("safe_mode"), "portals": status.get("portals"),
                                            "notifications": status.get("notifications"), "widgets": status.get("widgets"),
                                            "link_launches": status.get("link_launches")})
            left = deadline - time.monotonic()
            if left <= 0:
                self.step(f"ok {what} throughout {time.monotonic() - started:.3f}s")
                return status
            time.sleep(min(left, 0.2))

    def step(self, what: str) -> None:
        self.timeline.append({"t": round(time.monotonic(), 3), "step": what})

    # Gestures ────────────────────────────────────────────────────────────

    def point(self, target: str) -> tuple[int, int]:
        self.mark()
        x, y = resolve_target(self.status(), target)
        self.glide(x, y)
        self.step(f"point {target} {x},{y}")
        return x, y

    def click(self, target: str | None = None) -> None:
        if target:
            self.point(target)
        else:
            self.mark()
        self.inj.click()
        self.step(f"click {target or ''}".rstrip())

    def wheel(self, target: str, notches: int) -> None:
        """Positive notches scroll toward the start (wheel up); returns once the HUD handled them."""
        self.point(target)
        surface, _, pane = target.partition("/")
        here = "composer" if pane in {"composer", "input"} else "none"
        self.expect(f"pointer on {target}", on(surface, lambda p: p["hover"] == here))
        self.inj.click(4 if notches > 0 else 5, repeat=abs(notches))
        self.step(f"wheel {target} {notches}")
        self.handled(target)

    def glide(self, x: int, y: int) -> None:
        """Arrive at x,y in three motion events, all inside the destination, as a real pointer does.

        The runtime drops a pointer move that finds the scene lock busy
        (hud-65gpz), and hover follows the last move it handled; a single
        synthetic jump can therefore leave hover stale. It also guarantees
        motion when the pointer already rests at x,y.
        """
        for dx in (-2, -1, 0):
            self.inj.move_abs(x + dx, y)

    def handled(self, target: str) -> None:
        """Prove queued input was handled: hover a pane whose hover state differs, and observe it.

        The HUD handles input in order, so seeing the hover change means every
        notch queued before the move was handled too (wheel() confirmed the
        opposite hover before injecting, so this is a change). Without this a long
        burst still draining (Windows hands wheel messages over a few per
        frame) swallows the next gesture.
        """
        surface, _, pane = target.partition("/")
        other, hover = ("output", "none") if pane in {"composer", "input"} else ("composer", "composer")
        x, y = resolve_target(self.status(), f"{surface}/{other}")
        self.glide(x, y)
        self.expect(f"input to {target} handled", on(surface, lambda p: p["hover"] == hover))

    def drag(self, start: tuple[int, int], end: tuple[int, int], *, release: bool = True, steps: int = 8) -> None:
        self.mark()
        self.inj.move_abs(*start)
        self.inj.button(True)
        for i in range(1, steps + 1):
            self.inj.move_abs(start[0] + (end[0] - start[0]) * i // steps,
                                start[1] + (end[1] - start[1]) * i // steps)
        if release:
            self.inj.button(False)
        self.step(f"drag {start} -> {end}{'' if release else ' (held)'}")

    def type(self, text: str) -> None:
        self.mark()
        self.inj.type(text)
        self.step(f"type {len(text)} chars")

    def key(self, chord: str) -> None:
        self.mark()
        self.inj.key(chord)
        self.step(f"key {chord}")

    # MCP ─────────────────────────────────────────────────────────────────

    def publish(self, surface: str, **args) -> dict:
        self.mark()
        is_error, reply = self.smoke.call("hud_publish", {"surface": surface, **args})
        if is_error:  # the refusal's code and hint, never content
            raise ScenarioFailed(f"hud_publish {surface} refused", {"code": reply.get("code"), "hint": reply.get("hint")})
        if surface.startswith(("zone:", "widget:")):
            self.publications.add((surface, args.get("key")))
        self.step(f"publish {surface}")
        return reply

    def inputs(self, wait_ms: int = 0) -> list[dict]:
        """Take and ack queued hud_input items."""
        is_error, reply = self.smoke.call("hud_input", {"wait_ms": wait_ms, "max_items": 20}, timeout_s=10 + wait_ms / 1000)
        if is_error:
            raise ScenarioFailed("hud_input refused", {"code": reply.get("code"), "hint": reply.get("hint"), "wait_ms": wait_ms})
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

    def clear_publications(self) -> list[dict]:
        """Release this run's publications and portals; return unhandled input."""
        stray = self.inputs()
        for entry in self.smoke.ok("hud_surfaces").get("surfaces", []):
            if entry.get("s", "").startswith("portal:") and entry.get("state") not in {None, "detached"}:
                self.smoke.call("hud_clear", {"surface": entry["s"]})
        for surface, key in sorted(self.publications, key=lambda p: (p[0], p[1] or "")):
            args = {"surface": surface}
            if key is not None:
                args["key"] = key
            self.smoke.ok("hud_clear", args)
        self.publications.clear()
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
        """What a person would see: the whole desktop, not the HUD's own frame."""
        if WINDOWS:
            (self.out / f"{name}.desktop.png").write_bytes(encode_png_rgba(*WIN.desktop_rgba()))
        elif shutil.which("ffmpeg"):
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


def open_portal(run: Run, surface: str, prefix: str, count: int = LINES, *, ttl_ms: int | None = 0, **args) -> None:
    # None leaves the normal idle deadline intact; 0 deliberately holds forever.
    run.publish(surface, content=lines(prefix, 0, count), **({"ttl_ms": ttl_ms} if ttl_ms is not None else {}), **args)
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
    # Wheel up in handled bursts until a burst no longer moves the view: the top.
    top = None
    for _ in range(12):
        run.wheel(f"{s}/output", 100)
        now = first_line(portal_entry(run.settle(), s))
        if now == top:
            break
        top = now
    else:
        raise ScenarioFailed("the view reaches the top", {"first_visible_line": top})
    # The top must be where the 16 KiB retention bound puts it, within one
    # publish: not a-000 (nothing trimmed) and not mid-buffer (a dropped burst).
    line_bytes = len(lines("a", 0, 1, pad=20)) + 1
    retained = (6 * chunk - int(top.split()[0][2:])) * line_bytes if top and top.startswith("a-") else -1
    if not 16 * 1024 - chunk * line_bytes < retained <= 16 * 1024:
        raise ScenarioFailed("the head was trimmed to the retention bound", {"top": top, "retained_bytes": retained})
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


def open_card(run: Run, key: str, *, actions: list[dict] | None = None) -> dict:
    """Publish through MCP, then wait for the actual drawn card, never its estimate."""
    run.publish("zone:notification-area", key=key, ttl_ms=0,
                content={"body": "Owned gesture check", "actions": actions or [
                    {"label": "First", "callback_id": f"{key}-first"},
                    {"label": "Second", "callback_id": f"{key}-second"}]})
    run.expect(f"card {key} geometry", lambda s: bool(
        (card := notification_entry(s, key)) and card["state"] != "exiting"))
    # A completed fade need not produce one more frame. Last-frame diagnostics
    # may correctly retain "entering" after the runtime has become quiescent;
    # wait on actual presentation/idleness rather than inventing a new frame.
    status = run.settle()
    return notification_entry(status, key)


def take_card_answer(run: Run, key: str, action: str, *, dismiss: bool = False) -> None:
    """One retained callback/dismiss, its local exit, and no duplicate item."""
    items = run.take(1)
    if len(items) != 1 or items[0].get("action") != action or items[0].get("key") != key \
            or items[0].get("s") != "zone:notification-area" \
            or (dismiss and items[0].get("kind") != "dismiss"):
        raise ScenarioFailed(f"card {key} delivers one {action}", {"items": items, "status": run.status()})
    run.expect(f"card {key} exited", lambda s: notification_entry(s, key) is None)
    extra = run.inputs()
    if extra:
        raise ScenarioFailed(f"card {key} produced duplicate input", {"items": extra, "status": run.status()})


def scenario_card_click(run: Run) -> None:
    """R14.2 S1: a real action click returns one callback and the card exits."""
    key = "gesture-card-click"
    open_card(run, key)
    run.click(f"zone:notification-area/{key}/0")
    take_card_answer(run, key, f"{key}-first")


def scenario_card_keyboard(run: Run) -> None:
    """R14.2 S2: composer focus wraps with Tab to action two; Enter answers once."""
    key, surface = "gesture-card-keys", "portal:gesture-card-keys"
    open_portal(run, surface, "Keyboard", count=2)
    run.click(f"{surface}/composer")
    run.expect("composer focused before card", on(surface, lambda p: p["focus"] == "composer"))
    open_card(run, key)
    for _ in range(16):
        run.key("Tab")
        status = run.settle()
        card = notification_entry(status, key)
        if card and card["focus"] == 1:
            break
    else:
        raise ScenarioFailed("Tab wraps to card action two", run.status())
    run.key("Return")
    take_card_answer(run, key, f"{key}-second")


def scenario_card_dismiss(run: Run) -> None:
    """R14.2 S3: the reported close target emits one kind=dismiss item."""
    key = "gesture-card-dismiss"
    open_card(run, key)
    run.click(f"zone:notification-area/{key}/close")
    take_card_answer(run, key, "dismiss", dismiss=True)


def scenario_card_link(run: Run) -> None:
    """R14.2 S4/S5: one real link handoff; same action within 2s is refused, with no input."""
    key = "gesture-card-link"
    open_card(run, key, actions=[{"label": "Open test page", "url": "https://example.invalid/"}])
    before = run.status()["link_launches"]
    run.point(f"zone:notification-area/{key}/0")
    first = time.monotonic()
    run.inj.click()
    run.expect("one accepted link launch", lambda s: s["link_launches"] == before + 1,
               timeout_s=1.0, interval_s=0.02)
    # ShellExecute may have foregrounded a browser. Regain only the owned HUD,
    # without injecting, and keep the normal guard on every subsequent SendInput.
    if run.injector == "sendinput":
        if run.status().get("pid") != run.pid:
            raise Refused("owned HUD changed before the second link press")
        def foreground():
            if WIN.foreground_pid() != run.pid:
                WIN.bring_to_front(run.pid)
            return WIN.foreground_pid() == run.pid, WIN.foreground_pid()
        wait_until(foreground, what="owned HUD foreground before second link press",
                   timeout_s=max(0.01, first + 1.8 - time.monotonic()), interval_s=0.02)
    run.inj.click()
    second = time.monotonic()
    if second - first >= 2.0:
        raise ScenarioFailed("second link press must occur within 2s", {"seconds": second - first, "status": run.status()})
    run.step(f"same link pressed again after {second - first:.3f}s")
    run.observe_for("link rate limit", lambda s: s["link_launches"] == before + 1
                    and notification_entry(s, key) is not None, 0.3)
    if items := run.inputs():
        raise ScenarioFailed("links create no hud_input item", {"items": items, "status": run.status()})


def scenario_widget_hover(run: Run) -> None:
    """R14.2 S6: the declared widget target shows its tooltip after dwell and hides on leave."""
    surface = "widget:main-status"
    run.publish(surface, ttl_ms=0, params={"status": "online", "reason": "Owned hover check"})
    run.expect("widget rendered", lambda s: widget_entry(s, surface) is not None)
    # Move to a reported non-hover part of the same quad, not a guessed screen point.
    widget = widget_entry(run.status(), surface)
    rect = widget["rect"]
    run.glide(round(rect["x"] + 1), round(rect["y"] + rect["h"] - 1))
    run.expect("widget tooltip initially hidden", lambda s: not widget_entry(s, surface)["tooltip_visible"])
    run.point(f"{surface}/hover")
    run.expect("widget tooltip after 180ms dwell", lambda s: widget_entry(s, surface)["tooltip_visible"])
    run.glide(round(rect["x"] + 1), round(rect["y"] + rect["h"] - 1))
    run.expect("widget tooltip hidden on leave", lambda s: not widget_entry(s, surface)["tooltip_visible"])


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
    run.glide(int(old["x"]) + 4, int(old["y"]) + 4)
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
    run.inj.button(False)
    run.step("mouse-up")
    run.settle()
    dropped = portal_entry(run.status(), s)["rect"]
    run.inj.move_abs(hx + 300, hy + 140)
    run.step("move after release")
    # A drag still held would carry the portal along; hovering its composer
    # proves the move was handled.
    run.point(f"{s}/composer")
    run.expect("stays dropped after release", on(s, lambda p: p["rect"] == dropped and p["hover"] == "composer"))


def output_background(frame: tuple[int, int, bytes], portal: dict) -> bytes:
    """The actual OUTPUT background, left of the text inset and away from rounded corners."""
    width, height, rgba = frame
    rect = portal["panes"]["output"]
    x, y = int(rect["x"]) + 2, int(rect["y"] + rect["h"] / 2)
    if not (0 <= x < width and 0 <= y < height):
        raise ScenarioFailed("OUTPUT background pixel is on screen", {"rect": rect, "frame": [width, height]})
    at = (y * width + x) * 4
    return rgba[at:at + 4]


def scenario_writing(run: Run) -> None:
    """Writing header and trailing caret appear with fragments, then both clear about two seconds later."""
    s = "portal:g-writing"
    open_portal(run, s, "w", count=5)
    for fragment in ("first fragment", "final fragment"):
        published = time.monotonic()
        run.publish(s, content="\n" + fragment)
        run.expect("the rendered header and caret both show writing",
                   on(s, lambda p: "⋯ writing" in p["header_text"] and p["output"]["writing_caret"] is True),
                   timeout_s=1.0)
        run.snapshot(f"writing.{fragment.split()[0]}")
    # Two seconds is the production quiescence window. Allow one bounded
    # 750ms presentation/observation margin, measured from the final publish.
    remaining = 2.75 - (time.monotonic() - published)
    if remaining <= 0:
        raise ScenarioFailed("writing observation fits the 2.75s deadline", {"elapsed_s": time.monotonic() - published})
    run.expect("both writing cues clear after the final fragment",
               on(s, lambda p: "⋯ writing" not in p["header_text"] and p["output"]["writing_caret"] is False),
               timeout_s=remaining)
    run.step(f"ok writing cleared {time.monotonic() - published:.3f}s after the final publish")
    run.snapshot("writing.cleared")


def scenario_lifecycle(run: Run) -> None:
    """13. Zone TTL/hold, then real portal idle dim/stale, same-id recovery and eventual reap."""
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
    if lasted < 3.0:
        raise ScenarioFailed("hud_hold outlives the original TTL", {"lasted_s": round(lasted, 2)})
    run.step(f"ok hold outlived the TTL ({lasted:.1f}s)")
    s = "portal:g-life"
    started = time.monotonic()
    open_portal(run, s, "l", count=5, ttl_ms=None)
    active = run.expect("portal initially active", on(s, lambda p: p["state"] == "Active"))
    bright = output_background(run.frame(), portal_entry(active, s))
    degraded = run.expect("idle alone degrades the portal and shows the stale marker",
                          on(s, lambda p: p["state"] == "Degraded" and "⊘ disconnected — stream stale" in p["header_text"]),
                          timeout_s=35, interval_s=0.2)
    elapsed = time.monotonic() - started
    if elapsed < 29:
        raise ScenarioFailed("degradation follows the normal 30s idle deadline", {"elapsed_s": elapsed})
    run.settle()
    dim = output_background(run.frame(), portal_entry(degraded, s))
    if sum(dim[:3]) >= sum(bright[:3]) or dim[3] != bright[3]:
        raise ScenarioFailed("idle dims the actual OUTPUT background at unchanged opacity",
                             {"active_rgba": list(bright), "degraded_rgba": list(dim)})
    run.step(f"ok idle dim/stale after {elapsed:.3f}s; background {list(bright)} -> {list(dim)}")
    run.snapshot("lifecycle.degraded")
    resumed = time.monotonic()
    reply = run.publish(s, content="\nrecovered")
    if reply.get("reattached"):
        raise ScenarioFailed("idle recovery uses the same live attachment", {"reply": reply})
    recovered = run.expect("same-id publish clears stale and restores Active",
                           on(s, lambda p: p["state"] == "Active" and "stream stale" not in p["header_text"]
                              and (p["output"]["last_visible_line"] or "").startswith("recovered")))
    run.settle()
    recovered_color = output_background(run.frame(), portal_entry(recovered, s))
    if recovered_color != bright:
        raise ScenarioFailed("recovery restores the OUTPUT background", {"active_rgba": list(bright), "recovered_rgba": list(recovered_color)})
    run.snapshot("lifecycle.recovered")
    run.expect("a second unrefreshed idle interval degrades again", on(s, lambda p: p["state"] == "Degraded"),
               timeout_s=35, interval_s=0.2)
    run.expect("continued idle reclaims the portal",
               lambda st: isinstance(st.get("portals"), list) and portal_entry(st, s) is None,
               timeout_s=35, interval_s=0.2)
    elapsed = time.monotonic() - resumed
    if elapsed < 59:
        raise ScenarioFailed("reap follows a full idle plus degraded interval", {"elapsed_s": elapsed})
    run.step(f"ok no input/publish/hold refresh: reclaimed {elapsed:.3f}s after recovery")
    run.snapshot("lifecycle.reaped")


def scenario_dismiss(run: Run) -> None:
    """A real close-button click dismisses the portal; same-id publication creates a fresh attachment."""
    s = "portal:g-dismiss"
    open_portal(run, s, "old", count=5, expects_reply=True)
    run.point(f"{s}/header")
    run.expect("the actual close hit region is exposed", on(s, lambda p: p["panes"]["close"] is not None))
    run.click(f"{s}/close")
    run.expect("the pointer close removes the portal",
               lambda st: isinstance(st.get("portals"), list) and portal_entry(st, s) is None)
    run.snapshot("dismiss.closed")
    reply = run.publish(s, content="fresh attachment", expects_reply=True, ttl_ms=0)
    if reply.get("reattached") is not True:
        raise ScenarioFailed("same-id publish explicitly reattaches", {"reply": reply})
    run.expect("reattachment has only fresh output and empty draft",
               on(s, lambda p: (p["output"]["first_visible_line"] or "").startswith("fresh attachment")
                  and (p["output"]["last_visible_line"] or "").startswith("fresh attachment") and p["input"]["draft_chars"] == 0))
    run.click(f"{s}/composer")
    run.type("fresh reply")
    run.key("Return")
    if [i.get("text") for i in run.take(1)] != ["fresh reply"]:
        raise ScenarioFailed("the fresh composer's real pointer hit and submit work", {})


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
    """16. Pointer, Ctrl+-/= and directional Ctrl+Shift+Arrow resize; republish preserves geometry and hits."""
    s = "portal:g-resize"
    open_portal(run, s, "z", count=5, expects_reply=True)
    shrunk = portal_entry(shrink(run, s, times=4), s)["rect"]
    run.key("ctrl+equal")
    grown = run.expect("Ctrl+= grows the portal", on(s, lambda p: p["rect"]["w"] > shrunk["w"] and p["rect"]["h"] > shrunk["h"]))
    rect = portal_entry(grown, s)["rect"]
    # Ctrl+Shift+Arrow selects text in a focused composer. Directional
    # resize deliberately belongs to non-composer portal focus.
    run.click(f"{s}/output")
    run.expect("OUTPUT has non-composer portal focus", on(s, lambda p: p["focus"] == "tile"))
    run.key("ctrl+shift+Left")
    narrower = run.expect("Ctrl+Shift+Left changes width only",
                          on(s, lambda p: p["rect"]["w"] < rect["w"] and p["rect"]["h"] == rect["h"]))
    rect = portal_entry(narrower, s)["rect"]
    run.key("ctrl+shift+Up")
    shorter = run.expect("Ctrl+Shift+Up changes height only",
                         on(s, lambda p: p["rect"]["w"] == rect["w"] and p["rect"]["h"] < rect["h"]))
    rect = portal_entry(shorter, s)["rect"]
    start = (int(rect["x"] + rect["w"]) - 2, int(rect["y"] + rect["h"]) - 2)
    run.drag(start, (start[0] + 60, start[1] + 40))
    run.expect("bottom-right pointer drag resizes both axes without moving the origin",
                         on(s, lambda p: p["rect"]["w"] >= rect["w"] + 40 and p["rect"]["h"] >= rect["h"] + 20
                            and p["rect"]["x"] == rect["x"] and p["rect"]["y"] == rect["y"]))
    before = portal_entry(run.settle(), s)
    run.publish(s, content="\ngeometry marker", expects_reply=True)
    run.expect("republish uses the resized OUTPUT",
                              on(s, lambda p: (p["output"]["last_visible_line"] or "").startswith("geometry marker")))
    after = portal_entry(run.settle(), s)
    if after["rect"] != before["rect"]:
        raise ScenarioFailed("republish preserves the viewer's resized frame", {"before": before, "after": after})
    # Republish resolves canonical token chrome inside the retained viewport;
    # internal reflow is allowed, but both layouts must keep paint and hits coherent.
    for layout in (before, after):
        outer, panes = layout["rect"], layout["panes"]
        header, left, right = (panes[name] for name in ("header", "input", "output"))
        for name in ("header", "input", "output", "composer"):
            pane = panes[name]
            if not (outer["x"] <= pane["x"] < pane["x"] + pane["w"] <= outer["x"] + outer["w"]
                    and outer["y"] <= pane["y"] < pane["y"] + pane["h"] <= outer["y"] + outer["h"]):
                raise ScenarioFailed("resized panes and hit rect stay inside the frame", {"pane": name, "rect": pane, "frame": outer})
        if not (header["x"] == outer["x"] and header["y"] == outer["y"] and header["w"] == outer["w"]
                and left["y"] == right["y"] == header["y"] + header["h"]
                and left["y"] + left["h"] == right["y"] + right["h"] == outer["y"] + outer["h"]
                and left["x"] == outer["x"] and left["x"] + left["w"] <= right["x"]
                and right["x"] + right["w"] == outer["x"] + outer["w"] and panes["composer"] == left):
            raise ScenarioFailed("header, independent panes and composer hit stay aligned", {"layout": layout})
    run.click(f"{s}/output")
    run.expect("republished OUTPUT remains outside the composer hit", on(s, lambda p: p["focus"] == "tile"))
    run.click(f"{s}/composer")
    run.type("resized reply")
    run.expect("the resized composer hit takes real typing", on(s, lambda p: p["input"]["draft_chars"] == 13))
    run.key("Return")
    if [i.get("text") for i in run.take(1)] != ["resized reply"]:
        raise ScenarioFailed("the resized composer submits the exact reply", {})
    run.step("ok resized panes, republish and real pointer hit remain coherent")


def scenario_unicode_typing(run: Run) -> None:
    """Text sent as Unicode packets (on-screen keyboards, emoji panels) reaches the composer exactly."""
    s = "portal:g-unicode"
    open_portal(run, s, "n", count=5, expects_reply=True)
    run.click(f"{s}/composer")
    run.expect("composer focused", on(s, lambda p: p["focus"] == "composer"))
    text = "héllo ✓ 你好"
    run.mark()
    getattr(run.inj, "type_unicode", run.inj.type)(text)
    run.step(f"type {len(text)} chars as Unicode")
    run.expect("Unicode draft", on(s, lambda p: p["input"]["draft_chars"] == len(text)))
    run.key("Return")
    items = run.take(1)
    if [i.get("text") for i in items] != [text]:
        raise ScenarioFailed("Enter submits the exact Unicode text", {"items": items})


def scenario_no_focus_steal(run: Run) -> None:
    """10. Publishing never takes the foreground from the app being typed in; its keys stay there."""
    s = "portal:g-focus"
    open_portal(run, s, "f", count=5, expects_reply=True)
    run.click(f"{s}/composer")
    run.type("abc")
    run.expect("a composer draft to protect", on(s, lambda p: p["input"]["draft_chars"] == 3))
    notepad = subprocess.Popen(["notepad.exe"])
    run.helper_pids.append(notepad.pid)
    try:
        def foreground():
            if WIN.foreground_pid() != notepad.pid:
                WIN.bring_to_front(notepad.pid)  # a window request, not injected input
            return WIN.foreground_pid() == notepad.pid, WIN.foreground_pid()
        wait_until(foreground, what="Notepad in the foreground", timeout_s=15, interval_s=0.2)
        run.step("ok Notepad foreground")
        for n in range(3):
            run.publish(s, content=f"\nbackground {n}")
        run.publish("zone:subtitle", content="background notice", ttl_ms=5000)
        run.expect("publishes shown", on(s, lambda p: (p["output"]["last_visible_line"] or "").startswith("background 2")))
        if WIN.foreground_pid() != notepad.pid:
            raise ScenarioFailed("the foreground stays with Notepad through publishes",
                                 {"foreground_pid": WIN.foreground_pid(), "hud_pid": run.pid})
        text = "typed into notepad"
        run.type(text)
        wait_until(lambda: (WIN.edit_text(notepad.pid) == text, WIN.edit_text(notepad.pid)),
                   what="Notepad receives the keys", timeout_s=SETTLE_S)
        run.expect("the HUD composer draft is untouched", on(s, lambda p: p["input"]["draft_chars"] == 3))
        if run.inputs():
            raise ScenarioFailed("no hud_input from keys typed into Notepad", {})
    finally:
        kill_pid(notepad.pid)
        notepad.wait(10)
        run.helper_pids.remove(notepad.pid)


def expect_overlay_capture(run: Run, wanted: bool, what: str) -> None:
    def sample():
        state = WIN.pointer_capture_state(run.pid)
        return state["capturing"] is wanted and state["no_activate"] is True, state
    state, _ = wait_until(sample, what=what + " with WS_EX_NOACTIVATE", timeout_s=SETTLE_S)
    run.step(f"ok {what}: {state}")


def overlay_focus_etiquette(run: Run, notepad_pid: int, surface: str) -> None:
    """Real overlay focus: pane/button no-activate, registered answer chord and return.

    Reuse scenario 11's HUD, ICD and owned editor to keep combined tier-W
    occupancy bounded. Foreground requests below only prepare owned helpers;
    measured answer/exit paths never repair the foreground on the HUD's behalf.
    """
    def foreground_is(pid: int, what: str) -> None:
        wait_until(lambda: (WIN.foreground_pid() == pid, WIN.foreground_pid()),
                   what=what, timeout_s=SETTLE_S)
        run.step(f"ok {what}")

    def activate_owned(pid: int) -> None:
        def prepare():
            WIN.bring_to_front(pid)
            return WIN.foreground_pid() == pid, WIN.foreground_pid()
        wait_until(prepare, what="prepare owned app foreground", timeout_s=SETTLE_S)

    def enter(target: str) -> None:
        run.point(target)
        run.inj.move_relative(1, 0)
        expect_overlay_capture(run, True, f"native capture at {target}")

    # A real accepted pane click changes logical portal focus without stealing
    # native focus. Subsequent typing proves the OS destination as well.
    enter(f"{surface}/output")
    run.click()
    run.expect("pane click handled locally", on(surface, lambda p: p["focus"] == "tile"))
    foreground_is(notepad_pid, "pane click keeps Notepad foreground")
    run.type(" pane")
    expected = "click through pane"
    wait_until(lambda: (WIN.edit_text(notepad_pid) == expected, WIN.edit_text(notepad_pid)),
               what="Notepad receives typing after pane click", timeout_s=SETTLE_S)

    # An actual text-field click does take focus. Reply keeps it; the empty
    # outer Esc returns it, with the next physical key going to Notepad.
    run.click(f"{surface}/composer")
    foreground_is(run.pid, "composer click explicitly takes keyboard focus")
    run.type("focus reply")
    run.key("Return")
    items = run.take(1)
    if [item.get("text") for item in items] != ["focus reply"]:
        raise ScenarioFailed("focus test sends exactly one reply", items)
    foreground_is(run.pid, "reply keeps composer foreground")
    run.key("Escape")
    foreground_is(notepad_pid, "outer Esc returns to the prior app")
    run.type(" back")
    expected += " back"
    wait_until(lambda: (WIN.edit_text(notepad_pid) == expected, WIN.edit_text(notepad_pid)),
               what="next key after Esc reaches Notepad", timeout_s=SETTLE_S)

    key = "gesture-answer-chord"
    open_card(run, key)
    foreground_is(notepad_pid, "card publish does not activate the overlay")
    run.key("ctrl+shift+F9")
    run.expect("real global answer chord focuses action one", lambda status: bool(
        (card := notification_entry(status, key)) and card["focus"] == 0
        and WIN.foreground_pid() == run.pid))
    run.key("2")
    take_card_answer(run, key, f"{key}-second")
    foreground_is(notepad_pid, "digit answer restores the prior app")
    run.type(" answer")
    expected += " answer"
    wait_until(lambda: (WIN.edit_text(notepad_pid) == expected, WIN.edit_text(notepad_pid)),
               what="next physical keystroke after answer reaches Notepad", timeout_s=SETTLE_S)
    run.expect("answer keys never edit the composer", on(surface, lambda p: p["input"]["draft_chars"] == 0))
    run.key("ctrl+shift+F9")
    run.observe_for("no question means no foreground change",
                    lambda _: WIN.foreground_pid() == notepad_pid, 0.25)

    third = subprocess.Popen(["notepad.exe"])
    run.helper_pids.append(third.pid)
    try:
        activate_owned(third.pid)
        run.type("third")
        activate_owned(notepad_pid)
        key = "gesture-third-app"
        open_card(run, key)
        run.key("ctrl+shift+F9")
        run.expect("third-app test starts with a chord-focused card", lambda status: bool(
            (card := notification_entry(status, key)) and card["focus"] == 0
            and WIN.foreground_pid() == run.pid))
        # The overlay is absent from the taskbar/Alt-Tab list. Depending on
        # Windows' current selection, one switch reaches the editor or its
        # immediately preceding owned helper; both requests use real Alt-Tab.
        switches = []
        for _ in range(2):
            before = WIN.foreground_pid()
            run.key("alt+Tab")
            # A changed PID alone does not establish the intended destination.
            # Observe an owned editor before any subsequent guarded input.
            def switched_owned_editor():
                pid = WIN.foreground_pid()
                return pid in {notepad_pid, third.pid} and pid != before, pid
            switched, _ = wait_until(switched_owned_editor,
                                    what="real Alt-Tab reaches an owned editor", timeout_s=SETTLE_S)
            switches.append(switched)
            if switches[-1] == third.pid:
                break
        if WIN.foreground_pid() != third.pid:
            raise ScenarioFailed("Alt-Tab reaches the third owned app", {"switches": switches})
        run.step(f"ok real Alt-Tab reaches third app: {switches}")
        run.observe_for("user app switch is not undone", lambda _: WIN.foreground_pid() == third.pid, 0.25)
        enter(f"zone:notification-area/{key}/0")
        run.click()
        take_card_answer(run, key, f"{key}-first")
        foreground_is(third.pid, "later card button answer does not undo the app switch")
        run.key("Escape")
        run.type(" stays")
        wait_until(lambda: (WIN.edit_text(third.pid) == "third stays", WIN.edit_text(third.pid)),
                   what="later Esc and typing stay in the third app", timeout_s=SETTLE_S)
        run.observe_for("late card exit never restores the old app",
                        lambda _: WIN.foreground_pid() == third.pid, 0.25)

        # Closing the recorded origin while the HUD is focused must not raise
        # a different window on Esc, or dereference the retired native handle.
        key = "gesture-closed-origin"
        open_card(run, key)
        run.key("ctrl+shift+F9")
        run.expect("closed-origin test takes card focus", lambda status: bool(
            (card := notification_entry(status, key)) and card["focus"] == 0
            and WIN.foreground_pid() == run.pid))
        kill_pid(third.pid)
        third.wait(10)
        run.helper_pids.remove(third.pid)
        run.key("Escape")
        run.observe_for("closed origin leaves foreground to Windows",
                        lambda _: WIN.foreground_pid() == run.pid, 0.25)
        run.expect("closed origin does not answer the card", lambda status: bool(
            (card := notification_entry(status, key)) and card["focus"] is None and card["chosen"] is None))
    finally:
        if third.poll() is None:
            kill_pid(third.pid)
            third.wait(10)
        if third.pid in run.helper_pids:
            run.helper_pids.remove(third.pid)


def scenario_overlay_pointer(run: Run) -> None:
    """11. Relative pointer entry captures the composer; outside clicks reach owned Notepad."""
    fields = [json.loads(line).get("fields", {}) for line in run.log.read_text(errors="replace").splitlines()
              if line.startswith("{")]
    renderer = [f for f in fields if f.get("message") == "windowed: GPU adapter selected"]
    modes = [f for f in fields if str(f.get("message", "")).startswith("window mode: overlay/HUD")]
    if (len(renderer) != 1 or renderer[0].get("backend") != "Vulkan"
            or "llvmpipe" not in renderer[0].get("device_name", "").lower() or len(modes) != 1):
        raise ScenarioFailed("actual overlay uses Vulkan lavapipe", {"renderer": renderer, "modes": modes})
    run.step(f"actual overlay renderer {renderer[0]}")
    s = "portal:g-overlay"
    notepad = subprocess.Popen(["notepad.exe"])
    run.helper_pids.append(notepad.pid)
    try:
        def foreground():
            WIN.bring_to_front(notepad.pid)
            return WIN.foreground_pid() == notepad.pid, WIN.foreground_pid()
        wait_until(foreground, what="owned Notepad foreground", timeout_s=15, interval_s=0.2)
        WIN.maximize(notepad.pid)
        wait_until(lambda: (WIN.edit_rect(notepad.pid) is not None, None),
                   what="owned Notepad editor", timeout_s=SETTLE_S)
        open_portal(run, s, "overlay", count=5, expects_reply=True)
        # The production default fills small displays. Prepare outside space
        # through normal owned controls, then reset to the external app before
        # the measured entry; a full-display fixture cannot test click-through.
        run.step("prepare smaller portal before measured entry")
        run.point(f"{s}/composer")
        run.inj.move_relative(1, 0)
        run.expect("preparation reaches composer", on(s, lambda p: p["hover"] == "composer"))
        shrink(run, s)
        run.settle()
        wait_until(foreground, what="owned Notepad foreground before measured entry", timeout_s=15, interval_s=0.2)
        rect = run.portal(s)["rect"]
        # Start outside the portal. Only relative events wake raw-input entry
        # detection while the overlay is passing pointer events through.
        width, height = WIN.screen_size()
        outside = next(((x, y) for x, y in ((width - 2, height // 2), (2, height // 2),
                                           (width // 2, height - 2), (width // 2, 2))
                        if not (rect["x"] <= x <= rect["x"] + rect["w"]
                                and rect["y"] <= y <= rect["y"] + rect["h"])), None)
        if outside is None:
            raise ScenarioFailed("a point outside the portal exists", rect)
        run.inj.move_abs(*outside)
        expect_overlay_capture(run, False, "overlay passes pointer through outside portal")
        run.step("ok native overlay is click-through before relative entry")
        target = resolve_target(run.status(), f"{s}/composer")
        deadline = time.monotonic() + SETTLE_S
        while True:
            x, y = WIN.cursor_position()
            dx, dy = target[0] - x, target[1] - y
            if abs(dx) <= 1 and abs(dy) <= 1:
                break
            if time.monotonic() >= deadline:
                raise ScenarioFailed("relative motion reaches the composer", {"cursor": [x, y], "target": target})
            run.inj.move_relative(max(-8, min(8, dx)), max(-8, min(8, dy)))
            # Observe OS cursor progress during one continuous gesture.
            time.sleep(0.005)
        run.expect("relative entry shows composer hover", on(s, lambda p: p["hover"] == "composer"))
        expect_overlay_capture(run, True, "relative entry enables native capture")
        run.step("ok relative entry enables native capture")
        run.screen_grab("overlay-pointer.entry")
        run.click()
        run.expect("entry click focuses composer", on(s, lambda p: p["focus"] == "composer"))
        run.type("overlay entry")
        run.expect("overlay receives typed text", on(s, lambda p: p["input"]["draft_chars"] == 13))
        run.key("Return")
        items = run.take(1)
        if [item.get("text") for item in items] != ["overlay entry"]:
            raise ScenarioFailed("overlay receives exactly one reply", items)
        # The prepared portal leaves owned editor space outside its bounds.
        rect = run.portal(s)["rect"]
        editor = WIN.edit_rect(notepad.pid)
        if editor is None:
            raise ScenarioFailed("owned editor remains available", {})
        left, top, right, bottom = editor
        point = (right - 20, (top + bottom) // 2)
        if not (0 <= point[0] < width and 0 <= point[1] < height) or (
                rect["x"] <= point[0] <= rect["x"] + rect["w"]
                and rect["y"] <= point[1] <= rect["y"] + rect["h"]):
            raise ScenarioFailed("click-through point is in editor outside portal", {"editor": editor, "portal": rect})
        if WIN.foreground_pid() != run.pid:
            raise ScenarioFailed("HUD is foreground before click-through", {"foreground": WIN.foreground_pid()})
        run.inj.move_abs(*point)
        expect_overlay_capture(run, False, "leaving portal restores native click-through")
        run.click()
        wait_until(lambda: (WIN.foreground_pid() == notepad.pid, WIN.foreground_pid()),
                   what="outside click foregrounds owned Notepad", timeout_s=SETTLE_S)
        run.type("click through")
        wait_until(lambda: (WIN.edit_text(notepad.pid) == "click through", WIN.edit_text(notepad.pid)),
                   what="underlying editor receives exact text", timeout_s=SETTLE_S)
        run.expect("underlying typing leaves composer empty", on(s, lambda p: p["input"]["draft_chars"] == 0))
        run.screen_grab("overlay-pointer.click-through")
        run.step("ok outside click reaches owned Notepad and typing stays there")
        overlay_focus_etiquette(run, notepad.pid, s)
    finally:
        kill_pid(notepad.pid)
        notepad.wait(10)
        run.helper_pids.remove(notepad.pid)


def scenario_safe_mode(run: Run) -> None:
    """12. The real pause chord preserves an unheld portal beyond idle/reap expiry, then resumes without reap."""
    s = "portal:g-safe"
    open_portal(run, s, "m", count=5, ttl_ms=None)
    run.key("ctrl+shift+F12")
    run.expect("the pause chord sets safe mode", lambda st: st.get("safe_mode") is True)
    is_error, reply = run.smoke.call("hud_publish", {"surface": s, "content": "\nwhile paused"})
    if not is_error or reply.get("code") != "SAFE_MODE_ACTIVE":
        raise ScenarioFailed("publishing while paused returns SAFE_MODE_ACTIVE", {"reply": reply})
    run.inputs()  # exercise availability BEFORE the measured no-refresh interval
    run.snapshot("safe-mode.paused")
    paused = time.monotonic()
    intact = on(s, lambda p: p["state"] == "Active" and tail_line("m", 4)(p))
    run.observe_for("safe mode preserves the unheld portal past the normal 30s+30s reap deadline",
                    lambda st: st.get("safe_mode") is True and intact(st), 65.0)
    paused_s = time.monotonic() - paused
    if paused_s < 65:
        raise ScenarioFailed("pause observation spans the normal reap deadline", {"paused_s": paused_s})
    run.key("ctrl+shift+F12")
    run.expect("pressed again it resumes", lambda st: st.get("safe_mode") is False)
    # No publish/input/hold refresh before observing survival after the real exit.
    run.observe_for("resume does not reap the retained portal",
                    lambda st: st.get("safe_mode") is False and on(s, lambda p: p["state"] == "Active")(st), 5.0)
    # Resume invalidates measured layouts for a frame. Observe liveness above
    # throughout that transition, then require the retained text to be measured.
    run.expect("the original content is measured after resume", intact, timeout_s=1.0)
    run.snapshot("safe-mode.resumed")
    run.publish(s, content="\nafter resume")
    run.expect("publishing works again",
               on(s, lambda p: (p["output"]["last_visible_line"] or "").startswith("after resume")))
    run.step(f"ok unheld portal paused for {paused_s:.3f}s; survived 5s after resume before refresh")


def scenario_tray(run: Run) -> None:
    """Windows tray registration/tooltip and idle proof; real native Hide/Show when the icon is exposed."""
    status = run.expect("tray registered", lambda st: st.get("tray", {}).get("registered") is True)
    tray = status["tray"]
    tooltip = tray.get("tooltip", "")
    if len(tooltip.splitlines()) != 2 or len(tooltip.encode("utf-16-le")) > 254:
        raise ScenarioFailed("two-line bounded tray tooltip", tray)
    idle = run.settle()
    frames, since = idle["frames_presented"], idle["idle_since_ms"]
    run.observe_for("tray schedules no idle frames", lambda st:
                    st["frames_presented"] == frames and st["idle_since_ms"] >= since, 0.5)
    rect = WIN.tray_rect(tray["hwnd"], tray["icon_id"], run.pid)
    if rect is None:
        run.step("limited tray proof: registration/tooltip/idle passed; icon in overflow or not exposed, no menu input injected")
        return
    x, y = (rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2
    for hidden in (True, False):
        run.inj.move_abs(x, y)
        run.inj.click(3)
        run.expect("native tray menu active", lambda st: st["tray"]["menu_open"] is True)
        # Default Open panel is first, Pause/Resume second, Hide/Show third.
        for key in ("Home", "Down", "Down", "Return"):
            run.inj.key(key)
        run.expect("native Hide all" if hidden else "native Show all",
                   lambda st: st["tray"]["all_hidden"] is hidden and st["tray"]["menu_open"] is False)
    run.step("ok exposed tray: real right-click and keyboard Hide all / Show all")


# Windows only: they need Win32 foreground/edit-control access or global hotkeys.
TIER_W_ONLY = {"no-focus-steal", "safe-mode", "overlay-pointer", "tray"}

SCENARIOS: dict[str, Callable[[Run], None]] = {
    "output-anchor": scenario_output_anchor,          # 1
    "independent-scroll": scenario_independent_scroll,  # 2
    "unread": scenario_unread,                        # 3
    "jump-to-latest": scenario_jump_to_latest,        # 4
    "badge-opaque": scenario_badge_opaque,            # 5
    "composer": scenario_composer,                    # 6
    "keyboard": scenario_keyboard,                    # 7
    "card-click": scenario_card_click,
    "card-keyboard": scenario_card_keyboard,
    "card-dismiss": scenario_card_dismiss,
    "card-link": scenario_card_link,
    "widget-hover": scenario_widget_hover,
    "drag-alignment": scenario_drag_alignment,        # 8
    "drag-release": scenario_drag_release,            # 9
    "writing": scenario_writing,
    "dismiss": scenario_dismiss,
    "lifecycle": scenario_lifecycle,                  # 13
    "resize-hotkey": scenario_resize_hotkey,          # 16
    "unicode-typing": scenario_unicode_typing,
    "no-focus-steal": scenario_no_focus_steal,        # 10 (tier W)
    "tray": scenario_tray,                            # tier W, overlay only
    "overlay-pointer": scenario_overlay_pointer,      # 11 (tier W, overlay only)
    "safe-mode": scenario_safe_mode,                  # 12 (tier W)
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
            try:
                run.screen_grab(f"{name}.after")
            except Exception as err:  # noqa: BLE001 - retain the original scenario failure
                result["diagnostic_error"] = f"{type(err).__name__}: {err}"
        try:
            run.snapshot(f"{name}.after")
            stray = run.clear_publications()
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
    parser.add_argument("--exe", type=Path, required=True, help="the tze_hud binary to own")
    parser.add_argument("--injector", choices=("xtest", "sendinput"), default="sendinput" if WINDOWS else "xtest",
                        help="xtest: private Xvfb (Linux); sendinput: Win32 on a CI VM only")
    parser.add_argument("--window-mode", choices=("fullscreen", "overlay"), default="fullscreen")
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
    if (args.injector == "sendinput") != WINDOWS:
        parser.error(f"--injector {args.injector} does not run on {sys.platform}")
    tier_w = args.injector == "sendinput"
    if not tier_w and TIER_W_ONLY.intersection(args.scenarios):
        parser.error(f"{', '.join(sorted(TIER_W_ONLY))} run only with --injector sendinput")
    if args.window_mode == "overlay" and not tier_w:
        parser.error("overlay gestures require --injector sendinput")
    overlay_names = {"overlay-pointer", "tray"}
    if args.scenarios and any((n in overlay_names) != (args.window_mode == "overlay") for n in args.scenarios):
        parser.error("overlay-pointer and tray require --window-mode overlay; other scenarios require fullscreen")
    names = args.scenarios or [n for n in SCENARIOS
                               if (tier_w or n not in TIER_W_ONLY)
                               and ((n in overlay_names) == (args.window_mode == "overlay"))]
    if "restart" in names:  # it replaces the HUD; keep it last
        names = [n for n in names if n != "restart"] + ["restart"]
    sha = git_sha()
    # The pid keeps parallel runs started in the same second apart.
    out = args.out or REPO / "test_results/gestures" / sha / f"{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
    out.mkdir(parents=True, exist_ok=True)
    run = Run(exe=args.exe.resolve(), out=out, injector=args.injector, window_mode=args.window_mode)
    started = time.monotonic()
    results: list[dict] = []
    failure = None
    try:
        run.start()
        status = run.status()
        run.step(f"owned HUD pid {run.pid} on display {run.display or 'primary'} via {args.injector}")
        results = run_scenarios(run, names)
        unacked = run.smoke.ok("hud_input", {"max_items": 0}).get("remaining", 0)
        if unacked:
            failure = f"{unacked} un-acked hud_input item(s) at exit"
    except Refused as err:
        failure = f"refused: {err}"
    except Exception as err:  # noqa: BLE001
        failure = f"harness: {type(err).__name__}: {err}"
    finally:
        survivors = run.stop() if run.home else []
        if survivors:
            failure = (failure + "; " if failure else "") + "left running: " + ", ".join(survivors)
        timeline = {
            "sha": sha, "profile": run.exe.parent.name,
            "exe": str(run.exe), "injector": args.injector, "window_mode": args.window_mode, "screen": SCREEN,
            "display": run.display, "mcp_port": run.port,
            "geometry": (status.get("displays") if "status" in locals() else None),
            "theme_scale": ({k: status.get(k) for k in ("theme", "ui_scale") if k in status} if "status" in locals() else None),
            "seconds": round(time.monotonic() - started, 2), "results": results, "failure": failure,
            "steps": run.timeline,
            "sendinput_calls": getattr(run.inj, "calls", None) if args.injector == "sendinput" else None,
        }
        (out / "timeline.json").write_text(json.dumps(timeline, indent=1))
        if run.log and run.log.exists():
            tail = run.log.read_text(errors="replace").splitlines()[-200:]
            (out / "hud.log.tail").write_text("\n".join(tail))
    passed = sum(r["outcome"] == "PASS" for r in results)
    print(f"{passed}/{len(results)} scenarios passed in {time.monotonic() - started:.1f}s; artifacts: {out}")
    if failure:
        print(f"FAIL harness: {failure}")
    write_step_summary(args.injector, results, failure, passed, time.monotonic() - started)
    return 0 if failure is None and passed == len(results) else 1


def write_step_summary(injector: str, results: list[dict], failure: str | None, passed: int, seconds: float) -> None:
    """A GitHub step summary table, so a dispatching agent reads outcomes without the artifact."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    rows = [f"## Gesture scenarios ({injector}): {passed}/{len(results)} passed in {seconds:.0f}s", "",
            "| scenario | outcome | s | failing step |", "|---|---|---|---|"]
    rows += [f"| {r['scenario']} | {r['outcome']} | {r['seconds']} | {r.get('step', '')} |" for r in results]
    if failure:
        rows += ["", f"**harness:** {failure}"]
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n".join(rows) + "\n")


if __name__ == "__main__":
    sys.exit(main())
