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
import hashlib
import json
import os
import re
import secrets
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
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
DOCK_TOOLTIP_SCENARIO = "dock-countdown-tooltip"
DOCK_POPUP_RGB = (255, 208, 0)
DOCK_INK_RGB = (16, 24, 48)
SETTLE_S = 5.0  # bound on any one observed transition
# A focused composer's caret blink presents about every 530 ms, so the HUD
# counts as settled once nothing has presented for this long (docs/api.md).
SETTLED_IDLE_MS = 300


class Refused(RuntimeError):
    """A guard refused to inject input or to run against something not owned."""


class ForeignForeground(Refused):
    """The Windows guard verified HUD identity, but foreground is unowned."""

    def __init__(self, pid: int) -> None:
        super().__init__(f"the foreground window belongs to pid {pid}, not an owned process")
        self.pid = pid


class PartialButtonBatch(Refused):
    """A native click may have left a button down. End input on this desktop."""


class ScenarioFailed(AssertionError):
    def __init__(self, step: str, last: object) -> None:
        super().__init__(step)
        self.step = step
        self.last = last


class LinkTargetLost(ScenarioFailed):
    """A link guard observed a changed physical target before native submission."""

    def __init__(self, phase: str, step: str, last: dict) -> None:
        super().__init__(step, last)
        self.phase = phase


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
    check_windows_hud(github_actions=github_actions, owned_hud_pid=owned_hud_pid, status_pid=status_pid, url=url)
    if foreground_pid not in {owned_hud_pid, *helper_pids}:
        raise ForeignForeground(foreground_pid)


def check_windows_hud(*, github_actions: str | None, owned_hud_pid: int | None, status_pid: object, url: str) -> None:
    """Check the hosted HUD's identity before any foreground preparation."""
    host = urllib.parse.urlsplit(url).hostname
    if host not in {"127.0.0.1", "localhost", "::1"}:
        raise Refused(f"refusing a borrowed HUD URL (host {host!r}); this harness owns its HUD")
    if github_actions != "true":
        raise Refused("SendInput reaches the real desktop; it runs only on a CI VM (GITHUB_ACTIONS=true)")
    if owned_hud_pid is None:
        raise Refused("no owned HUD process")
    if status_pid != owned_hud_pid:
        raise Refused(f"/admin/status pid {status_pid!r} is not the owned HUD pid {owned_hud_pid}")


def cursor_at_link(cursor: list[int] | None, point: tuple[int, int]) -> bool:
    # Absolute SendInput normalization can round a physical pixel.
    return cursor is not None and all(abs(actual - wanted) <= 1 for actual, wanted in zip(cursor, point))


def foreign_link_point_details(owner: dict | None, pid: int | None) -> dict:
    if owner is None or owner["pid"] == pid:
        return {}
    try:
        details = WIN.window_owner_details(owner["hwnd"], owner["pid"])
    except OSError:
        return {}  # The native lookup is diagnostic only.
    return {"point_owner_details": details} if details else {}


# ── Geometry ─────────────────────────────────────────────────────────────────


def portal_entry(status: dict, surface: str) -> dict | None:
    return next((p for p in status.get("portals") or [] if p.get("s") == surface), None)


def notification_entry(status: dict, key: str, surface: str = "zone:notification-area") -> dict | None:
    return next((p for p in status.get("notifications") or []
                 if p.get("s") == surface and p.get("key") == key), None)


FOREIGN_AGENT = "gesture-foreign"


def keyed_cards(status: dict, key: str) -> list[list[str]]:
    """Action labels of every drawn, non-exiting notification card with `key`.

    Status omits publishers and bodies, so cross-agent scenarios tell same-key
    cards apart by their distinct action labels."""
    return [[a["label"] for a in card.get("actions") or []]
            for card in status.get("notifications") or []
            if card.get("s") == "zone:notification-area" and card.get("key") == key
            and card.get("state") != "exiting"]


def widget_entry(status: dict, surface: str) -> dict | None:
    return next((p for p in status.get("widgets") or [] if p.get("s") == surface), None)


def resolve_target(status: dict, target: str) -> tuple[int, int]:
    """Centre of a reported portal pane, card control, or widget hover target.

    Card targets are `zone:<name>/<key>/<index|close|goto>`; widget targets are
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
        if control in {"close", "goto"}:
            rect = card.get(control)
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

    def __init__(self, display: str, guard: Callable[[], None], authority: Path | None = None) -> None:
        self.display = display
        self.guard = guard
        self.authority = authority
        self.calls = 0

    def _run(self, *args: object) -> None:
        self.guard()
        env = dict(os.environ, DISPLAY=self.display)
        opts = {}
        if self.authority is not None:
            env["XAUTHORITY"] = str(self.authority)
            opts["timeout"] = 2
        subprocess.run(["xdotool", *map(str, args)], env=env, check=True, capture_output=True, **opts)
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


def start_xvfb(first: int = 90, last: int = 199, *,
               authority_dir: Path | None = None) -> tuple[subprocess.Popen, str]:
    """Start Xvfb on the first free display number; return it once it is ready.

    Xvfb's own `-displayfd` search fails where the socket directory is
    read-only (WSLg mounts /tmp/.X11-unix that way; the abstract socket still
    works), so numbers are tried here and Xvfb's lock file settles races
    between parallel runs. `-displayfd` still reports readiness.
    """
    for number in range(first, last + 1):
        if Path(f"/tmp/.X{number}-lock").exists():
            continue
        auth_args = []
        if authority_dir is not None:
            from hud_x11_capture import write_authority

            authority = authority_dir / f"Xauthority-{number}"
            write_authority(authority, number)
            auth_args = ["-auth", str(authority), "-noreset"]
        read_fd, write_fd = os.pipe()
        try:
            proc = subprocess.Popen(
                ["Xvfb", f":{number}", "-displayfd", str(write_fd), "-screen", "0",
                 f"{SCREEN[0]}x{SCREEN[1]}x24", "-nolisten", "tcp", *auth_args],
                pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
        except OSError:
            os.close(read_fd)
            raise
        finally:
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


def private_x11_geometry(output: str, window: str) -> tuple[int, int, int, int]:
    match = re.fullmatch(
        rf"WINDOW={re.escape(window)}\nX=(-?\d+)\nY=(-?\d+)\nWIDTH=([1-9]\d*)\nHEIGHT=([1-9]\d*)\nSCREEN=0\n?",
        output,
    )
    if not match:
        raise Refused("private HUD client geometry is unavailable")
    return tuple(map(int, match.groups()))


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


MOUSE_RECEIPT_LINE = re.compile(
    r"(?P<timestamp>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?Z) +DEBUG +"
    r"tze_hud_runtime::windowed: main thread: mouse input "
    r"state=(?P<state>Pressed|Released) button=(?P<button>Left|Right|Middle|Back|Forward|Other\(\d{1,5}\)) "
    r"secondary=(?P<secondary>None|Some\(\d{1,10}\)) "
    r"x=(?P<x>-?\d{1,20}(?:\.\d{1,20})?(?:[eE][+-]?\d{1,3})?) "
    r"y=(?P<y>-?\d{1,20}(?:\.\d{1,20})?(?:[eE][+-]?\d{1,3})?)"
)


@dataclass
class MouseReceiptCapture:
    """Optional existing-log evidence; never decides a gesture's result."""

    out: Path
    directory: Path | None = None
    evidence: dict = field(default_factory=lambda: {
        "filter": "info,tze_hud_runtime::windowed=debug",
        "identities": [], "errors": [], "absence_is_inconclusive": True,
    })

    def prepare(self, env: dict, exe: Path) -> None:
        try:
            self.evidence["clock_anchor"] = {"unix_s": time.time(), "monotonic_s": time.monotonic()}
            self.evidence["source_sha"] = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=REPO, capture_output=True, text=True,
                check=True, timeout=5).stdout.strip()
            with exe.open("rb") as binary:
                self.evidence["exe_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
            self.evidence["exe"] = str(exe)
            directory = Path(tempfile.mkdtemp(prefix="hud_mouse_receipts_"))
            if directory.resolve().is_relative_to(self.out.resolve()):
                shutil.rmtree(directory)
                raise ValueError("raw log scratch must be outside the artifact tree")
            self.directory = directory
            env.update(TZE_HUD_LOG_DIR=str(self.directory), TZE_HUD_FILE_LOG=self.evidence["filter"])
        except Exception as err:  # Optional evidence must not change the input run's result.
            self.evidence["errors"].append(f"prepare: {type(err).__name__}: {err}")

    def observe(self, status: dict, owned_pid: int | None) -> None:
        # Called after startup or predecessor exit, using status already fetched by the harness.
        self.evidence["identities"].append({
            "owned_pid": owned_pid, "status_pid": status.get("pid"), "sha": status.get("sha"),
            "matches": (owned_pid is not None and status.get("pid") == owned_pid
                        and bool(self.evidence.get("source_sha"))
                        and status.get("sha") == self.evidence.get("source_sha")),
            "unix_s": time.time(), "monotonic_s": time.monotonic(),
        })

    def end_interval(self) -> None:
        if self.evidence["identities"] and "until" not in self.evidence["identities"][-1]:
            self.evidence["identities"][-1]["until"] = {
                "unix_s": time.time(), "monotonic_s": time.monotonic(),
            }

    def finish(self, owned_pids: list[int], survivors: list[str]) -> dict:
        evidence = self.evidence
        evidence.update(owned_pids=list(owned_pids), files=[], source_files=[], mouse_records=0,
                        unparsed_mouse_records=0, projection_truncated=False, logger_started=False,
                        pid_attribution="identity intervals exclude restart overlap; shared log lines have no PID")
        try:
            records = []
            if self.directory:
                for name in ("tze_hud.log.1", "tze_hud.log"):
                    path = self.directory / name
                    if not path.exists():
                        continue
                    # The existing logger rotates at 10 MiB, retaining one predecessor.
                    # Limit the read itself too, even if a surviving writer grows it.
                    with path.open("rb") as raw:
                        data = raw.read(12 * 1024 * 1024 + 1)
                    if len(data) > 12 * 1024 * 1024:
                        raise ValueError(f"{name} exceeds bounded log inspection")
                    evidence["source_files"].append({"name": name, "bytes": len(data),
                                                     "sha256": hashlib.sha256(data).hexdigest(),
                                                     "mouse_records": data.count(b"main thread: mouse input")})
                    evidence["mouse_records"] += data.count(b"main thread: mouse input")
                    evidence["logger_started"] |= b"log file path=" in data
                    for number, line in enumerate(data.decode("utf-8", errors="replace").splitlines(), 1):
                        if "main thread: mouse input" not in line:
                            continue
                        match = MOUSE_RECEIPT_LINE.fullmatch(line)
                        if not match:
                            evidence["unparsed_mouse_records"] += 1
                        elif len(records) < 10_000:
                            records.append(dict(match.groupdict(), source_file=name, source_line=number))
                        else:
                            evidence["projection_truncated"] = True
                evidence["rotated"] = (self.directory / "tze_hud.log.1").exists()
                # Publish only validated fields, never raw INFO, diagnostics or rejected files.
                if evidence["source_files"]:
                    data = "".join(json.dumps(record) + "\n" for record in records).encode("utf-8")
                    projection = self.out / "mouse-receipts.jsonl"
                    projection.write_bytes(data)
                    evidence["files"].append({"name": projection.name, "bytes": len(data),
                                              "sha256": hashlib.sha256(data).hexdigest()})
            evidence["projected_mouse_records"] = len(records)
        except Exception as err:  # Preserve the original gesture outcome even on artifact failure.
            evidence["errors"].append(f"capture: {type(err).__name__}: {err}")
        finally:
            if self.directory and not survivors:
                try:
                    shutil.rmtree(self.directory)
                    evidence["raw_cleanup"] = "removed"
                except OSError as err:
                    evidence["errors"].append(f"cleanup: {type(err).__name__}: {err}")
            elif self.directory:
                evidence["raw_cleanup"] = "deferred until surviving owned processes exit; outside artifact tree"
        try:
            identity_ok = bool(evidence["identities"]) and all(i["matches"] for i in evidence["identities"])
            if not evidence["files"]:
                evidence["capture_status"] = "unavailable"
            elif (evidence["errors"] or not identity_ok or survivors or not evidence["logger_started"]
                  or evidence.get("rotated") or len(owned_pids) > 1 or evidence["unparsed_mouse_records"]
                  or evidence["projection_truncated"]):
                evidence["capture_status"] = "partial"
            else:
                evidence["capture_status"] = "captured"
            # This is capture health, never a verdict that a missing mouse edge was not delivered.
            (self.out / "mouse-receipts.json").write_text(json.dumps(evidence, indent=1), encoding="utf-8")
        except Exception as err:  # The timeline still receives status if the separate receipt cannot be written.
            evidence["errors"].append(f"receipt: {type(err).__name__}: {err}")
            evidence["capture_status"] = "partial" if evidence["files"] else "unavailable"
        return evidence


def dock_tooltip_config(config: str) -> str:
    """Set distinct pixel colors and enable only the dock in this scratch copy."""
    marker = "[design_tokens]\n"
    if config.count(marker) != 1 or re.search(r"^\s*\[dock\]", config, re.M):
        raise Refused("unexpected source configuration for the dock witness")
    tokens = ('"color.surface.container.high" = "#FFD000"\n'
              '"color.on_surface" = "#101830"\n'
              '"opacity.surface" = "1"\n')
    return config.replace(marker, marker + tokens, 1) + '\n[dock]\naccepts = ["countdown"]\nmax_rows = 2\n'


@dataclass
class Run:
    exe: Path
    out: Path
    display: str = ""
    port: int = 0
    psk: str = field(default_factory=lambda: secrets.token_hex(16))
    # A second authenticated agent, for cross-agent isolation scenarios only.
    foreign_psk: str = field(default_factory=lambda: secrets.token_hex(16))
    foreign: Smoke | None = None
    xvfb: subprocess.Popen | None = None
    hud: subprocess.Popen | None = None
    owned_pids: list[int] = field(default_factory=list)
    home: Path | None = None
    config_dir: Path | None = None
    config: Path | None = None
    log: Path | None = None
    smoke: Smoke | None = None
    injector: str = "xtest"
    window_mode: str = "fullscreen"
    go_to_probe: bool = False
    inj: object | None = None  # XTest or hud_sendinput.SendInput
    helper_pids: list[int] = field(default_factory=list)  # owned non-HUD apps (scenario 10's Notepad)
    timeline: list[dict] = field(default_factory=list)
    publications: set[tuple[str, str | None]] = field(default_factory=set)
    grpc_port: int = field(default=0, init=False)
    mouse_receipts: MouseReceiptCapture | None = None
    dock_build_sha: str | None = None
    xauthority: Path | None = field(default=None, init=False)

    @property
    def pid(self) -> int | None:
        return self.owned_pids[-1] if self.owned_pids else None

    # Lifecycle ───────────────────────────────────────────────────────────

    def start(self, *, resident_raster: bool = False) -> None:
        if self.dock_build_sha and (
            sys.platform != "linux" or self.injector != "xtest" or self.window_mode != "fullscreen"
            or resident_raster or not LAVAPIPE.is_file()
        ):
            raise Refused("dock witness needs Linux, private fullscreen XTEST and llvmpipe")
        if self.injector == "sendinput":
            if os.environ.get("GITHUB_ACTIONS") != "true":
                raise Refused("tier W drives the real desktop; it runs only on a CI VM (GITHUB_ACTIONS=true)")
            running = windows_huds()
            if running:  # it would hold the chords and the port handover; never touch it
                raise Refused(f"tze_hud.exe is already running (pid {running}); this harness owns its HUD")
        self.home = Path(tempfile.mkdtemp(prefix="hud_gestures_home_"))
        if self.injector == "xtest":
            if self.dock_build_sha:
                self.xvfb, self.display = start_xvfb(authority_dir=self.home)
                self.xauthority = self.home / f"Xauthority-{self.display[1:]}"
            else:
                self.xvfb, self.display = start_xvfb()
            (self.home / ".config").mkdir()
            env = dict(os.environ, DISPLAY=self.display, WAYLAND_DISPLAY="", HOME=str(self.home),
                       XDG_CONFIG_HOME=str(self.home / ".config"))
            if LAVAPIPE.exists():
                env["VK_ICD_FILENAMES"] = str(LAVAPIPE)
            if self.xauthority:
                env.update(XAUTHORITY=str(self.xauthority), VK_DRIVER_FILES=str(LAVAPIPE),
                           WINIT_X11_SCALE_FACTOR="1", TZE_HUD_LOG_DIR=str(self.home))
                for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "TZE_HUD_FILE_LOG"):
                    env.pop(key, None)
        else:
            for name in ("LocalAppData", "AppData"):
                (self.home / name).mkdir()
            env = dict(os.environ, LOCALAPPDATA=str(self.home / "LocalAppData"), APPDATA=str(self.home / "AppData"))
        for key in ("GH_TOKEN", "GITHUB_TOKEN"):
            env.pop(key, None)
        env["TZE_HUD_LOG"] = "info"
        env["TZE_HUD_LOG_JSON"] = "1"
        if resident_raster:
            # Only the raster witness needs both listeners. Keep the first
            # reservation until the second is chosen so they cannot coincide.
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                self.grpc_port = reservation.getsockname()[1]
                self.port = free_port()
        else:
            self.grpc_port = 0
            self.port = free_port()
        config = seed_config(CONFIG, self.psk)
        self.config_dir = config.parent
        if self.dock_build_sha:
            config.write_text(dock_tooltip_config(config.read_text(encoding="utf-8")), encoding="utf-8")
        if self.go_to_probe:
            seeded = config.read_text(encoding="utf-8")
            config.write_text(seeded.replace("[runtime]\n",
                                             '[runtime]\ngo_to_apps = ["notepad.exe"]\n', 1),
                              encoding="utf-8")
        # Seed the second agent beside the first: PSK hash only, no admin grant.
        digest = hashlib.sha256(self.foreign_psk.encode()).hexdigest()
        with open(config.parent / "agents.toml", "a", encoding="utf-8") as agents:
            agents.write(f'\n[agents.{FOREIGN_AGENT}]\npsk_sha256 = "{digest}"\nallow = ["*"]\n')
        self.config = config
        self.log = (self.home if self.dock_build_sha else self.out) / "hud.log"
        dimensions = (["--width", str(SCREEN[0]), "--height", str(SCREEN[1])]
                      if self.window_mode == "fullscreen" else [])
        if self.mouse_receipts:
            self.mouse_receipts.prepare(env, self.exe)
        with open(self.log, "wb") as log:
            self.hud = subprocess.Popen(
                [str(self.exe), "--config", str(config), "--window-mode", self.window_mode,
                 *dimensions, "--grpc-port", str(self.grpc_port), "--mcp-port", str(self.port)],
                # Run from the scratch HOME for hygiene: nothing the HUD writes lands in the repo.
                env=env, cwd=self.home, stdout=log, stderr=subprocess.STDOUT,
            )
        self.owned_pids.append(self.hud.pid)
        self.smoke = Smoke(f"http://127.0.0.1:{self.port}/mcp", self.psk)
        self.foreign = Smoke(f"http://127.0.0.1:{self.port}/mcp", self.foreign_psk)
        wait_for_mcp(self.smoke, self.hud, 60)
        if resident_raster:
            # Capture uses the compositor's own frame and never injects input.
            if (self.hud.poll() is not None or self.status().get("pid") != self.pid
                    or (self.injector == "xtest" and (not self.xvfb or self.xvfb.poll() is not None))):
                raise Refused("raster witness did not reach its owned HUD and display")
            return
        self.inj = (XTest(self.display, self.guard, self.xauthority) if self.injector == "xtest"
                    else WIN.SendInput(self.guard))
        if self.injector == "sendinput":
            self._check_identity_mapping()
        if self.window_mode == "fullscreen":
            self.guard()
            if self.dock_build_sha:
                self.fit_private_dock_client()
        else:
            # An overlay may start without activation. No input is sent until
            # scenario 11 foregrounds its owned helper and the normal guard passes.
            if self.status().get("pid") != self.pid:
                raise Refused("overlay status is not the owned HUD")

    def fit_private_dock_client(self) -> None:
        """Apply borderless fullscreen client geometry on our bare Xvfb root."""
        from hud_x11_capture import Capture, CaptureError

        if self.xvfb is None or self.xauthority is None or self.pid is None:
            raise Refused("private HUD window setup requires Xvfb ownership")
        self.guard()
        env = {"DISPLAY": self.display, "XAUTHORITY": str(self.xauthority),
               "HOME": str(self.xauthority.parent), "PATH": os.defpath}

        def xdo(*args: str) -> str:
            self.guard()
            try:
                return subprocess.run(["xdotool", *args], env=env, check=True, capture_output=True,
                                      text=True, timeout=2).stdout
            except (OSError, subprocess.SubprocessError, UnicodeError):
                raise Refused("could not configure the owned X11 HUD client") from None

        try:
            capture = Capture(self.xvfb, self.display, self.xauthority)
        except (CaptureError, OSError, EOFError, TimeoutError):
            raise Refused("could not verify the private X11 root for window setup") from None
        expected = (0, 0, *SCREEN)
        try:
            window = xdo("search", "--onlyvisible", "--pid", str(self.pid)).strip()
            if not re.fullmatch(r"[1-9]\d{0,9}", window):
                raise Refused("expected one visible X11 window for the owned HUD")

            def client_geometry():
                return private_x11_geometry(xdo("getwindowgeometry", "--shell", window), window)

            geometry = client_geometry()
            if geometry != expected:
                # Bare Xvfb has no WM to honor winit's borderless fullscreen
                # request. A move after resize makes winit republish the real
                # display bounds before we admit rows or inject any pointer.
                if geometry[2:] != SCREEN:
                    xdo("windowsize", "--sync", window, *map(str, SCREEN))
                xdo("windowmove", "--sync", window, "1", "1")
                xdo("windowmove", "--sync", window, "0", "0")
                if client_geometry() != expected:
                    raise Refused("actual private HUD client does not fill the owned root")
        finally:
            if capture.close() != {"returncode": 0, "forced": False, "reaped": True}:
                raise Refused("private X11 window setup probe did not exit cleanly")

        def reported(status):
            displays = status.get("displays") or []
            return (status.get("pid") == self.pid and status.get("sha") == self.dock_build_sha
                    and len(displays) == 1 and displays[0].get("primary") is True
                    and tuple(displays[0].get(k) for k in ("x", "y", "width", "height")) == expected)

        self.expect("owned fullscreen client and reported display fill the private X11 root", reported)

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
                if self.dock_build_sha:
                    try:
                        self.xvfb.wait(2)
                    except subprocess.TimeoutExpired:
                        survivors.append(f"Xvfb pid {self.xvfb.pid}")
                else:
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
            raise ScenarioFailed(what, {k: last.get(k) for k in ("portals", "notifications", "widgets", "link_launches", "window_activations", "idle_since_ms")}) from None
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

    def expect_cursor(self, target: str, hand: bool) -> None:
        """Rest the pointer on a target; /admin/status `cursor` is the hand exactly when a press acts."""
        self.point(target)
        self.expect(f"{'hand' if hand else 'no hand'} over {target}",
                    lambda st: (st.get("cursor") == "hand") == hand)

    def click(self, target: str | None = None) -> None:
        if target:
            self.point(target)
        else:
            self.mark()
        self.inj.click()
        self.step(f"click {target or ''}".rstrip())

    def click_link(self, point: tuple[int, int], *, repeat: int = 1,
                   confirm: Callable[[], None] | None = None, hwnd: int | None = None,
                   deadline: float | None = None) -> None:
        """At a link, check the physical click target after the normal Windows guard."""
        if self.injector != "sendinput":
            self.inj.click(repeat=repeat)
            return

        def check_target() -> None:
            if confirm is not None:
                confirm()
            observed = WIN.click_target_snapshot()
            self.timeline.append({
                "t": round(time.monotonic(), 3),
                "step": "link click target before SendInput",
                "click_target_before_send": observed,
                "intended_point": list(point),
                "owned_hud_pid": self.pid,
                "repeat": repeat,
            })
            cursor = observed["cursor"]
            at_cursor = observed["at_cursor"]
            foreground = observed["foreground"]
            at_target = cursor_at_link(cursor, point)
            foreign_target = at_cursor is not None and at_cursor["pid"] != self.pid
            if deadline is not None and time.monotonic() >= deadline:
                raise ScenarioFailed("within the card link press deadline", {
                    "intended_point": list(point), "click_target_before_send": observed,
                })
            if (not at_target or at_cursor is None or foreign_target
                    or foreground is None or foreground["pid"] != self.pid
                    or (hwnd is not None and (at_cursor["hwnd"] != hwnd or foreground["hwnd"] != hwnd))):
                last = {"intended_point": list(point), "owned_hud_pid": self.pid, "click_target_before_send": observed}
                if hwnd is not None:
                    last["expected_hwnd"] = hwnd
                if at_target:
                    last.update(foreign_link_point_details(at_cursor, self.pid))
                raise LinkTargetLost("button cursor", "owned HUD at the link click target", last)

        self.inj.click(repeat=repeat, before_send=check_target)

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
        """Arrive at x,y in one motion event.

        The runtime retains a pointer move that finds the scene lock busy and
        replays it when the lock frees (hud-65gpz), so one jump updates hover.
        A pointer already resting at x,y gets a one-pixel lead-in so the
        arrival still produces motion.
        """
        if getattr(self, "_glide_at", None) == (x, y):
            self.inj.move_abs(x - 1, y)
        self.inj.move_abs(x, y)
        self._glide_at = (x, y)

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

    def screen_grab(self, name: str, *, frame: tuple[int, int, bytes] | None = None) -> None:
        """Save the whole desktop or a desktop sample already taken for an assertion."""
        if WINDOWS:
            capture = WIN.desktop_rgba() if frame is None else frame
            (self.out / f"{name}.desktop.png").write_bytes(encode_png_rgba(*capture))
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
    run.expect_cursor(f"{s}/unread_badge", hand=True)
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
    """6. Hover, click focus, local echo, exact submit, a second send, typing while output streams,
    and Escape keeping the draft for one exact send after refocus (hud-ohgc9.4)."""
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
    run.key("Escape")
    run.expect("one Escape leaves and keeps the draft",
               on(s, lambda p: p["focus"] == "none" and p["input"]["draft_chars"] == 3))
    if items := run.inputs():
        raise ScenarioFailed("Escape sends nothing", {"items": items})
    run.point(f"{s}/composer")
    run.click()
    run.expect("refocus restores the kept draft",
               on(s, lambda p: p["focus"] == "composer" and p["input"]["draft_chars"] == 3))
    run.key("Return")
    items = run.take(1)
    if [i.get("text") for i in items] != ["abc"]:
        raise ScenarioFailed("Enter sends the kept text once", {"items": items})


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


def open_card(run: Run, key: str, *, actions: list[dict] | None = None, park_pointer: bool = True) -> dict:
    """Publish through MCP, then wait for the actual drawn card, never its estimate.

    Parking avoids the temporary press guard for a card drawn under a resting
    pointer. Hosted card-link defers pointer input until it owns the link point.
    """
    if park_pointer:
        run.glide(1, 1)
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


def take_card_answer(run: Run, key: str, action: str, *, dismiss: bool = False,
                     reply_text: str | None = None) -> None:
    """One retained answer, its local exit, and no duplicate item."""
    items = run.take(1)
    if len(items) != 1 or items[0].get("action") != action or items[0].get("key") != key \
            or items[0].get("s") != "zone:notification-area" \
            or (dismiss and items[0].get("kind") != "dismiss") \
            or (reply_text is not None and items[0].get("text") != reply_text):
        raise ScenarioFailed(f"card {key} delivers one {action}", {"items": items, "status": run.status()})
    run.expect(f"card {key} exited", lambda s: notification_entry(s, key) is None)
    extra = run.inputs()
    if extra:
        raise ScenarioFailed(f"card {key} produced duplicate input", {"items": extra, "status": run.status()})


def scenario_card_click(run: Run) -> None:
    """R14.2 S1: a real action click returns one callback and the card exits."""
    key = "gesture-card-click"
    open_card(run, key)
    card = f"zone:notification-area/{key}"
    run.expect_cursor(f"{card}/close", hand=True)
    run.expect_cursor(f"{card}/0", hand=True)
    rect = notification_entry(run.status(), key, "zone:notification-area")["rect"]
    run.glide(int(rect["x"]) + 6, int(rect["y"]) + 6)  # the card's text corner presses nothing
    run.expect("no hand over card text", lambda st: st.get("cursor") != "hand")
    run.click(f"{card}/0")
    take_card_answer(run, key, f"{key}-first")


def foreign_call(run: Run, tool: str, args: dict) -> dict:
    run.mark()
    is_error, reply = run.foreign.call(tool, args)
    run.step(f"{FOREIGN_AGENT} {tool} {args.get('surface')} key={args.get('key')}")
    if is_error:
        raise ScenarioFailed(f"{FOREIGN_AGENT} {tool} refused", {"code": reply.get("code"), "hint": reply.get("hint")})
    return reply


def foreign_inputs(run: Run) -> list[dict]:
    is_error, reply = run.foreign.call("hud_input", {"max_items": 20})
    if is_error:
        raise ScenarioFailed(f"{FOREIGN_AGENT} hud_input refused", {"code": reply.get("code")})
    return reply.get("items") or []


def scenario_foreign_publisher(run: Run) -> None:
    """hud-bz9bv.6 AC10: another authenticated agent's same-key replace and
    keyed or whole clear never touch A's card, and A's answer routes only to A."""
    key, zone = "gesture-foreign", "zone:notification-area"
    # Both cards must fit the zone on the smallest harness display (tier L's
    # Xvfb reported 800x600, a 180 px zone), or the stack's visibility bound
    # hides one, which is not a replace: A's card has one action and the
    # foreign card none. Cards are told apart by action labels.
    mine, theirs = ["First"], []
    open_card(run, key, actions=[{"label": "First", "callback_id": f"{key}-first"}])
    foreign = {"surface": zone, "key": key, "ttl_ms": 0, "content": {"body": "Foreign gesture check"}}
    try:
        foreign_call(run, "hud_publish", foreign)
        run.expect("a foreign same-key publish adds its own card beside A's",
                   lambda st: sorted(keyed_cards(st, key)) == sorted([mine, theirs]))
        foreign_call(run, "hud_publish", foreign)  # the foreign agent's own same-key replace
        run.expect("a foreign same-key replace replaces only its own card",
                   lambda st: sorted(keyed_cards(st, key)) == sorted([mine, theirs]))
        foreign_call(run, "hud_clear", {"surface": zone, "key": key})
        run.expect("a foreign keyed clear removes only its own card", lambda st: keyed_cards(st, key) == [mine])
        foreign_call(run, "hud_publish", foreign)
        run.expect("foreign card back", lambda st: sorted(keyed_cards(st, key)) == sorted([mine, theirs]))
        foreign_call(run, "hud_clear", {"surface": zone})
        run.expect("a foreign whole-zone clear removes only its own cards", lambda st: keyed_cards(st, key) == [mine])
        run.observe_for("and A's card stays, unanswered", lambda st: keyed_cards(st, key) == [mine]
                        and all(c.get("chosen") is None for c in st.get("notifications") or []
                                if c.get("key") == key and c.get("state") != "exiting"), 0.3)
    finally:
        run.foreign.call("hud_clear", {"surface": zone})
    run.click(f"{zone}/{key}/0")
    take_card_answer(run, key, f"{key}-first")
    if items := foreign_inputs(run):
        raise ScenarioFailed("A's answer reaches no other agent", {"items": items})


class SessionSmoke(Smoke):
    """A Smoke that keeps the `Mcp-Session-Id` its `initialize` was issued,
    as the stdio adapter does: one Claude session on a shared pairing."""

    def __init__(self, url: str, psk: str, label: str) -> None:
        super().__init__(url, psk)
        self.session: str | None = None
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "_meta": {"tze-hud/label": label}})
        if not self.session:
            raise ScenarioFailed("initialize issues Mcp-Session-Id", {"label": label})

    def _open(self, request, timeout_s: float):
        if self.session:
            request.add_header("Mcp-Session-Id", self.session)
        resp = super()._open(request, timeout_s)
        self.session = self.session or resp.headers.get("Mcp-Session-Id")
        return resp

    def end(self) -> int:
        """DELETE the session, as the adapter does on a clean exit."""
        request = urllib.request.Request(self.url, method="DELETE", headers={
            "Authorization": f"Bearer {self.psk}", "Mcp-Session-Id": self.session or ""})
        try:
            with urllib.request.urlopen(request, timeout=5) as resp:
                return resp.status
        except urllib.error.HTTPError as err:
            return err.code


def dock_rect(entry: dict) -> tuple[int, int, int, int]:
    rect = entry["rect"]
    return tuple(round(rect[key]) for key in ("x", "y", "w", "h"))


def dock_crop(rgba: bytes, frame: tuple[int, ...], roi: tuple[int, ...]) -> bytes:
    x, y, w, h = roi
    fx, fy, fw, fh = frame
    if not (fx <= x < x + w <= fx + fw and fy <= y < y + h <= fy + fh):
        raise ValueError("dock crop lies outside the native frame")
    first = 4 * ((y - fy) * fw + x - fx)
    return b"".join(rgba[offset:offset + 4 * w]
                    for offset in range(first, first + 4 * fw * h, 4 * fw))


def dock_popup_pixels(pixels: bytes, width: int) -> tuple[bool | None, dict]:
    """Absent, complete rectangle plus interior ink, or ambiguous."""
    if width <= 0:
        raise ValueError("invalid popup ROI width")
    height = len(pixels) // (4 * width)
    if height == 0 or len(pixels) != 4 * width * height:
        raise ValueError("invalid popup ROI dimensions")
    fill = bytes(DOCK_POPUP_RGB) + b"\xff"
    matches = [i // 4 for i in range(0, len(pixels), 4) if pixels.startswith(fill, i)]
    count = len(matches)
    if not count:
        return False, {"fill": 0, "ink": 0}
    left, right = min(i % width for i in matches), max(i % width for i in matches)
    top, bottom = matches[0] // width, matches[-1] // width
    ink_mask = bytearray()
    for y in range(top + 3, bottom - 2):
        for x in range(left + 3, right - 2):
            offset = 4 * (y * width + x)
            pixel = pixels[offset:offset + 3]
            ink_mask.append(all(abs(a - b) <= 16 for a, b in zip(pixel, DOCK_INK_RGB)))
    ink = sum(ink_mask)
    continuous = count >= (right - left + 1) * (bottom - top + 1) * 0.5
    complete = right - left >= 50 and bottom - top >= 12 and continuous and ink >= 16
    return True if complete else None, {
        "fill": count, "ink": ink, "bounds": [left, top, right, bottom],
        "ink_mask_sha256": hashlib.sha256(ink_mask).hexdigest(),
    }


def dock_early_observation(image: dict, move: tuple[int, int], hovered: int) -> bool:
    return (image["pointer_ns"][0] >= move[1]
            and image["acquisition_ns"][1] <= move[0] + 150_000_000
            and image["popup"][hovered] is False)


def dock_build_evidence(run: Run) -> dict:
    """Project only stable identity and the actual GPU adapter from private logs."""
    with run.log.open("rb") as log:
        lines = log.read(65_536).splitlines()
    adapter = None
    for line in lines:
        try:
            fields = json.loads(line).get("fields", {})
        except (UnicodeError, ValueError):
            continue
        if fields.get("message") == "windowed: GPU adapter selected":
            adapter = fields
            break
    if (not adapter or adapter.get("backend") != "Vulkan"
            or not str(adapter.get("device_name", "")).lower().startswith("llvmpipe")):
        raise Refused("dock witness did not establish actual Vulkan llvmpipe")
    checkout, tree = subprocess.run(
        ["git", "rev-parse", "HEAD", "HEAD^{tree}"], cwd=REPO, check=True, capture_output=True, text=True, timeout=3
    ).stdout.splitlines()
    with run.exe.open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    return {"harness_commit": checkout, "harness_tree": tree, "expected_build": run.dock_build_sha,
            "exe_sha256": digest, "config_sha256": hashlib.sha256(run.config.read_bytes()).hexdigest(),
            "backend": "Vulkan", "software_device": "llvmpipe", "hud_pid": run.pid}


def scenario_dock_countdown_tooltip(run: Run) -> None:
    """Tier L opt-in: observe two independent dock countdown visits in native root pixels."""
    from hud_x11_capture import Capture, CaptureError, NS, inside

    surface = "dock:countdown"
    captions = ("dock-tooltip-a", "dock-tooltip-b")
    labels = ("WWWWWWWWWWWWWWWWWWWW-A", "MMMMMMMMMMMMMMMMMMe\u0301B")
    outside = (10, 10)
    samples, moves, publishes = [], {}, []
    report = {"schema": 1, "samples": samples, "moves": moves, "publishes": publishes}
    sessions, images = [], {}
    capture = None
    stage = "owned startup"
    failure = None

    def require(condition, message):
        if not condition:
            raise ScenarioFailed(message, {"stage": stage})

    def project_status(status):
        displays = status.get("displays") or []
        require(status.get("pid") == run.pid and status.get("sha") == run.dock_build_sha,
                "the executable build and owned status PID must match")
        require(len(displays) == 1 and displays[0].get("primary") is True
                and tuple(displays[0].get(k) for k in ("x", "y", "width", "height")) == (0, 0, *SCREEN),
                "the status display must be the full private root at origin zero")
        dock = status.get("dock") or {}
        require(dock.get("capacity", 0) == 2 and dock.get("scale") == 1,
                "the scratch dock must admit two full-size rows")
        return status

    def move(phase, point):
        # The lower bound precedes the incumbent authenticated pre-motion guard.
        before = time.monotonic_ns()
        run.glide(*point)
        interval = before, time.monotonic_ns()
        moves[phase] = {"point": point, "interval_ns": interval}
        return interval

    def cards(status):
        entries = []
        for caption in captions:
            matches = [entry for entry in status.get("widgets") or [] if entry.get("session") == caption]
            require(len(matches) == 1 and matches[0].get("display") == 0
                    and isinstance(matches[0].get("s"), str),
                    "scope must resolve to exactly one drawn primary widget")
            entries.append(matches[0])
        return entries

    def publish(index, label, clipped):
        is_error, reply = sessions[index].call(
            "hud_publish", {"surface": surface, "ttl_ms": 60_000,
                            "params": {"label": label, "ends_in_ms": 60_000}}, timeout_s=3
        )
        cut = (reply.get("clipped") or {}).get("label")
        expires = reply.get("expires_in_ms")
        accepted = not is_error and reply.get("ok") is True
        publishes.append({"card": "AB"[index], "label": label, "accepted": accepted,
                          "label_characters_drawn": cut if type(cut) is int else None,
                          "expires_in_ms": expires if type(expires) is int else None})
        require(accepted and type(expires) is int and 0 < expires <= 60_000,
                "scoped countdown publish must be accepted with a finite TTL")
        require((type(cut) is int and 0 <= cut < len(label)) if clipped else "clipped" not in reply,
                "publication must establish the expected label fit")

    def sample(point, deadline):
        require(run.hud.poll() is None, "owned HUD disappeared during root sampling")
        meta, rgba = capture.sample(frame, point, deadline)
        pixels = [dock_popup_pixels(dock_crop(rgba, frame, roi), roi[2]) for roi in rois]
        entry = {**meta, "stage": stage, "popup": [state for state, _ in pixels],
                 "roi_pixels": [counts for _, counts in pixels], "rgba_sha256": hashlib.sha256(rgba).hexdigest()}
        samples.append(entry)
        images["last"] = (entry, rgba)
        return entry, rgba

    def observe(point, expected, *, minimum=0, limit, label):
        while time.monotonic_ns() < limit:
            entry, rgba = sample(point, limit)
            require(entry["popup"] != [True, True], "both dock popups appeared in the same root image")
            start, end = entry["acquisition_ns"]
            if minimum and any(value is True for value, want in zip(entry["popup"], expected) if want):
                require(end >= minimum, "a complete popup appeared before 180 ms")
                require(start >= minimum, "the first positive XGetImage bracket overlaps 180 ms")
            if entry["popup"] == expected and start >= minimum:
                images[label] = (entry, rgba)
                return rgba
            time.sleep(0.020)
        raise ScenarioFailed("bounded native observation did not reach expected pixels", {"stage": stage})

    def visit(point, hovered, label):
        interval = move(label, point)
        # Capture once after proven arrival. A late or partial early frame
        # cannot be used as evidence of absence.
        entry, rgba = sample(point, interval[0] + 150_000_000)
        images[f"{label}-early"] = (entry, rgba)
        require(not entry["popup"][hovered], "a complete popup appeared before 180 ms")
        require(dock_early_observation(entry, interval, hovered),
                "no unambiguous early negative by move-start plus 150 ms")
        return observe(point, [hovered == 0, hovered == 1], minimum=interval[0] + 180_000_000,
                       limit=interval[0] + 1_500_000_000, label=f"{label}-shown")

    try:
        run.expect("dock startup reports full capacity", lambda st: (st.get("dock") or {}).get("capacity") == 2)
        project_status(run.status())
        report.update(dock_build_evidence(run))
        capture = Capture(run.xvfb, run.display, run.xauthority)
        report["x11"] = {"display": run.display, **capture.ready}
        stage = "publish and identify the two scoped cards"
        move("outside", outside)
        for index, caption in enumerate(captions):
            sessions.append(SessionSmoke(run.smoke.url, run.psk, caption))
            run.mark()
            publish(index, labels[index], clipped=True)
        initial = project_status(run.settle())
        entries = cards(initial)
        require((initial["dock"]["rows"], initial["dock"]["overflow"]) == (2, []),
                "both independently published rows must be visible")
        require(entries[0]["s"] != entries[1]["s"], "two scopes must have different actual card identities")
        bounds = [dock_rect(entry) for entry in entries]
        require(all(inside(rect) and rect[2:] == (160, 52) for rect in bounds),
                "the root must contain both actual compact card bounds")
        a, b = bounds
        require(a[0] == b[0] and a[0] < SCREEN[0] // 4 and a[1] + a[3] <= b[1],
                "the two ordered, nonoverlapping narrow cards must be at the left edge")
        points = [(x + w // 2, y + h // 2) for x, y, w, h in bounds]
        roi_x, division = max(x + w for x, y, w, h in bounds) + 1, (points[0][1] + points[1][1]) // 2
        capture_right = min(SCREEN[0], roi_x + 512)
        frame = a[0], a[1], capture_right - a[0], b[1] + b[3] - a[1]
        rois = [(roi_x, a[1], capture_right - roi_x, division - a[1]),
                (roi_x, division, capture_right - roi_x, b[1] + b[3] - division)]
        title_areas = [(x + 8, y + 4, w - 16, 20) for x, y, w, _ in bounds]
        report.update(cards=bounds, regions=rois, title_areas=title_areas)
        stage = "settled native card pixels, no popup"
        baseline = observe(outside, [False, False], limit=time.monotonic_ns() + NS, label="baseline")
        require(all(dock_crop(baseline, frame, rect).count(bytes(DOCK_POPUP_RGB) + b"\xff") >= 100
                    for rect in bounds), "both compact cards must have real root pixels")

        stage = "first hover A"
        shown_a = visit(points[0], 0, "a")
        stage = "leave A"
        move("leave", outside)
        observe(outside, [False, False], limit=time.monotonic_ns() + NS, label="left")
        stage = "fresh reentry A"
        shown_a_again = visit(points[0], 0, "a-reentry")
        require(dock_crop(shown_a, frame, rois[0]) == dock_crop(shown_a_again, frame, rois[0]),
                "A reentry must restore the same full native popup pixels")
        stage = "direct transfer A to B"
        shown_b = visit(points[1], 1, "b-transfer")
        a_ink = images["a-shown"][0]["roi_pixels"][0]["ink_mask_sha256"]
        b_ink = images["b-transfer-shown"][0]["roi_pixels"][1]["ink_mask_sha256"]
        require(a_ink != b_ink, "the two scoped labels must produce distinct native popup ink")

        stage = "fit B while stationary and keep A"
        publish(1, "Fit", clipped=False)
        report["fit_reply_ns"] = time.monotonic_ns()
        observe(points[1], [False, False], limit=report["fit_reply_ns"] + NS, label="b-fit-dismissed")
        time.sleep(0.220)
        check, fitted = sample(points[1], time.monotonic_ns() + NS)
        images["b-fit-settled"] = (check, fitted)
        require(check["popup"] == [False, False], "fitting label must stay hidden while pointer is stationary")
        prior_titles = [dock_crop(shown_b, frame, roi) for roi in title_areas]
        final_titles = [dock_crop(fitted, frame, roi) for roi in title_areas]
        report["title_sha256"] = [[hashlib.sha256(pixels).hexdigest() for pixels in pair]
                                  for pair in zip(prior_titles, final_titles)]
        require(prior_titles[0] == final_titles[0] and prior_titles[1] != final_titles[1],
                "A's title must stay unchanged while B's fitting title visibly updates")
        final = project_status(run.status())
        require([(entry["s"], dock_rect(entry)) for entry in cards(final)]
                == [(entry["s"], dock_rect(entry)) for entry in entries],
                "both scoped identities and card bounds must survive the fitting update")
        with run.exe.open("rb") as binary:
            require(hashlib.file_digest(binary, "sha256").hexdigest() == report["exe_sha256"],
                    "the executable digest changed during the run")
        window_activations = [status.get("window_activations") for status in (initial, final)]
        require(window_activations[0] == window_activations[1], "passive movement raised the HUD activation count")
        report["window_activations"] = window_activations
        require(run.inj.calls == 5, "expected exactly five guarded XTEST moves and no other input")
        report["xtest_moves"] = run.inj.calls
        report["final_build"] = final["sha"]
    except ScenarioFailed as err:
        # The generic expectation/settle last-status values are never archived here.
        failure = ScenarioFailed(err.step, {"stage": stage})
    except Exception as err:  # A third-party response or environment string is not evidence.
        detail = str(err) if isinstance(err, CaptureError) else type(err).__name__
        failure = ScenarioFailed(f"{stage}: {detail}", {"stage": stage})
    finally:
        cleanup = {"sessions": []}
        if capture:
            try:
                cleanup["capture"] = capture.close()
            except Exception as err:
                cleanup["capture"] = {"error": type(err).__name__}
        for session in reversed(sessions):
            try:
                cleanup["sessions"].append(session.end())
            except Exception as err:
                cleanup["sessions"].append(type(err).__name__)
        report["cleanup"] = cleanup
        capture_clean = cleanup.get("capture") == {"returncode": 0, "forced": False, "reaped": True}
        if failure is None and (cleanup["sessions"] != [202, 202] or not capture_clean):
            failure = ScenarioFailed("capture or scoped cleanup was incomplete", {"stage": "cleanup"})
        report["verdict"] = "FAIL" if failure else "PASS"
        report["failure"] = failure.step if failure else None
        for name, (entry, rgba) in images.items():
            png = encode_png_rgba(*entry["rect"][2:], rgba)
            path = run.out / f"{DOCK_TOOLTIP_SCENARIO}.{name}.png"
            path.write_bytes(png)
            entry.setdefault("images", []).append({"name": path.name, "bytes": len(png),
                                                    "sha256": hashlib.sha256(png).hexdigest()})
        (run.out / f"{DOCK_TOOLTIP_SCENARIO}.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
    if failure:
        raise failure


def entry_held(surfaces: dict, surface: str) -> bool:
    return any(e.get("s") == surface and e.get("held") for e in surfaces.get("surfaces") or [])


def scenario_sessions(run: Run) -> None:
    """Tier L: two labeled sessions on one pairing each publish a
    card; pressing B's answers only B (not A, not the unscoped caller), both
    are listed in /admin/status, and B's clean exit takes only its card."""
    zone, url = "zone:notification-area", run.smoke.url
    a, b = SessionSmoke(url, run.psk, "gesture-a"), SessionSmoke(url, run.psk, "gesture-b")
    try:
        run.glide(1, 1)  # a card arriving under a resting pointer is press-guarded
        # Two captioned cards outgrow tier L's 180 px zone, so B's newer card
        # covers A's slot; A's card is retained and draws again once B exits.
        for who, name in ((a, "a"), (b, "b")):
            run.mark()
            key = f"gesture-session-{name}"
            is_error, reply = who.call("hud_publish", {"surface": zone, "key": key, "ttl_ms": 0, "content": {
                "body": "Session gesture check", "actions": [{"label": name.upper(), "callback_id": f"{key}-pick"}]}})
            if is_error:
                raise ScenarioFailed(f"session {name} publish refused", {"code": reply.get("code"), "hint": reply.get("hint")})
            run.step(f"session {name} publish {zone} key={key}")
            run.expect(f"session {name} card drawn", lambda st, key=key: bool(
                (c := notification_entry(st, key)) and c["state"] != "exiting"))
        # The harness's own initialize opened an unlabeled session it never uses.
        labels = sorted(s["label"] for s in run.status().get("sessions") or [] if s.get("label"))
        if labels != ["gesture-a", "gesture-b"]:
            raise ScenarioFailed("/admin/status lists both sessions", {"labels": labels})
        run.settle()
        run.click(f"{zone}/gesture-session-b/0")
        _, got = b.call("hud_input", {"wait_ms": int(SETTLE_S * 1000)})
        items = got.get("items") or []
        if [i.get("action") for i in items] != ["gesture-session-b-pick"]:
            raise ScenarioFailed("B's press answers B", {"items": items})
        b.call("hud_input", {"ack": [i["id"] for i in items]})
        _, other = a.call("hud_input", {})
        if other.get("items") or run.inputs():
            raise ScenarioFailed("B's answer reaches neither A nor the unscoped caller",
                                 {"a": other.get("items"), "unscoped": run.inputs()})
        run.mark()
        if (status := b.end()) != 202:
            raise ScenarioFailed("B's DELETE releases it", {"status": status})
        run.step("session b DELETE")
        run.expect("B's exit takes only its card", lambda st: notification_entry(st, "gesture-session-b") is None
                   and notification_entry(st, "gesture-session-a") is not None, timeout_s=1.0)
        if not entry_held(a.ok("hud_surfaces"), zone):
            raise ScenarioFailed("A still holds its card", {})
    finally:
        for who in (a, b):
            who.end()


def held_press_on_retired_card(run: Run, key: str, retire: Callable[[], None], what: str) -> None:
    """Press A's action, retire that card while the button is down, release:
    the retired publication's press must send nothing."""
    open_card(run, key)
    run.point(f"zone:notification-area/{key}/0")
    run.settle()  # the hover frame first, so only the press can satisfy the next settle
    run.mark()
    run.inj.button(True)
    run.step("button down (held)")
    try:
        # The pressed visual draws a frame: the HUD has handled the press on
        # A's card before the MCP call can retire it.
        run.settle()
        retire()
        run.settle()
    finally:
        run.mark()
        run.inj.button(False)
        run.step("button up")
    run.observe_for(f"releasing after {what} activates nothing",
                    lambda st: all(c.get("chosen") is None for c in st.get("notifications") or []), 0.5)
    # Wait out a late stale activation rather than reading the queue once.
    if items := run.inputs(wait_ms=500):
        raise ScenarioFailed(f"releasing after {what} sends nothing", {"items": items, "status": run.status()})


def scenario_held_press(run: Run) -> None:
    """hud-bz9bv.6 AC2/AC10: a press held across a same-key replace, or across
    a keyed clear, never activates on release; the replacement still answers."""
    zone = "zone:notification-area"
    replaced = "gesture-held-replace"
    fresh = [{"label": "Third", "callback_id": f"{replaced}-third"},
             {"label": "Fourth", "callback_id": f"{replaced}-fourth"}]

    def replace() -> None:
        run.publish(zone, key=replaced, ttl_ms=0, content={"body": "Replaced gesture check", "actions": fresh})
        run.expect("the same-key replace is drawn", lambda st: keyed_cards(st, replaced) == [["Third", "Fourth"]])

    held_press_on_retired_card(run, replaced, replace, "a same-key replace")
    run.click(f"{zone}/{replaced}/0")
    take_card_answer(run, replaced, f"{replaced}-third")

    cleared = "gesture-held-clear"

    def clear() -> None:
        run.mark()
        run.smoke.ok("hud_clear", {"surface": zone, "key": cleared})
        run.step(f"hud_clear {zone} key={cleared}")
        run.expect("the keyed clear removes the card", lambda st: not keyed_cards(st, cleared))

    held_press_on_retired_card(run, cleared, clear, "a keyed clear")


def scenario_card_hold(run: Run) -> None:
    """A destructive action ignores a tap and fires once when held past
    `notification.action.hold.ms` (default 600 ms)."""
    key = "gesture-card-hold"
    open_card(run, key, actions=[{"label": "Delete", "callback_id": f"{key}-delete", "role": "destructive"},
                                 {"label": "Keep", "callback_id": f"{key}-keep"}])
    target = f"zone:notification-area/{key}/0"
    run.click(target)
    run.observe_for("a tap on a destructive action chooses nothing",
                    lambda st: (card := notification_entry(st, key)) is not None and card.get("chosen") is None, 1.0)
    if items := run.inputs(wait_ms=500):
        raise ScenarioFailed("a tap on a destructive action sends nothing", {"items": items, "status": run.status()})
    run.point(target)
    run.mark()
    run.inj.button(True)
    run.step("button down (held)")
    try:
        time.sleep(1.0)
    finally:
        run.mark()
        run.inj.button(False)
        run.step("button up")
    take_card_answer(run, key, f"{key}-delete")


def scenario_press_guard(run: Run) -> None:
    """A keyed card that arrives under a resting pointer ignores a click for
    `notification.press_guard.ms` (default 400 ms); once `guarded` clears, a
    click at the same spot answers."""
    key = "gesture-press-guard"
    open_card(run, key)
    target = f"zone:notification-area/{key}/0"
    run.point(target)
    run.smoke.ok("hud_clear", {"surface": "zone:notification-area", "key": key})
    run.expect(f"card {key} gone", lambda st: notification_entry(st, key) is None)
    run.settle()
    # The pointer rests where the card will draw again.
    run.publish("zone:notification-area", key=key, ttl_ms=0,
                content={"body": "Owned gesture check", "actions": [
                    {"label": "First", "callback_id": f"{key}-first"},
                    {"label": "Second", "callback_id": f"{key}-second"}]})
    run.expect(f"card {key} guarded", lambda st: bool((card := notification_entry(st, key)) and card.get("guarded")),
               interval_s=0.01)
    run.click()
    if items := run.inputs(wait_ms=500):
        raise ScenarioFailed("a click on an arriving card sends nothing", {"items": items, "status": run.status()})
    run.expect(f"card {key} no longer guarded",
               lambda st: (card := notification_entry(st, key)) is not None and not card.get("guarded"))
    run.expect("the hand returns without motion", lambda st: st.get("cursor") == "hand")
    run.click()
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


def scenario_card_copy(run: Run) -> None:
    """hud-ljhku.8 R8.2: a pointer press copies locally and leaves the card."""
    key, text = "gesture-card-copy", "cargo test -p example — job-42"
    open_card(run, key, actions=[{"label": "Copy cmd", "copy": text}])
    writes = run.status()["clipboard_writes"]
    run.click(f"zone:notification-area/{key}/0")
    expected = "copied" if WINDOWS else "copy_failed"
    run.expect("one local clipboard attempt and the card remains", lambda st: (
        st["clipboard_writes"] == writes + 1
        and (card := notification_entry(st, key))
        and card["chosen"] is None
        and card["actions"][0].get("feedback") == expected
    ))
    if WINDOWS and WIN.clipboard_text() != text:
        raise ScenarioFailed("the real clipboard holds the published text", run.status())
    run.snapshot("card-copy.feedback")
    if items := run.inputs(wait_ms=250):
        raise ScenarioFailed("copy queues no agent input", {"items": items})


def scenario_card_reply(run: Run) -> None:
    """R11.2 S1: the on-card reply field sends the typed line once."""
    key, action, text = "gesture-card-reply", "use-database", "use the staging DB"
    open_card(run, key, actions=[{"label": "Other…", "reply": action}])
    run.click(f"zone:notification-area/{key}/0")
    run.expect("inline reply field is drawn", lambda st: (
        (card := notification_entry(st, key)) and card.get("reply_field")
    ))
    run.type(text)
    if items := run.inputs():
        raise ScenarioFailed("typing does not publish draft text", {"items": items})
    run.snapshot("card-reply.draft")
    run.key("Return")
    take_card_answer(run, key, action, reply_text=text)


def scenario_card_reveal(run: Run) -> None:
    """hud-ljhku.8 R8.3: hover reveals a cut body, Tab reveals a cut label."""
    key, zone = "gesture-card-reveal", "zone:notification-area"
    portal = "portal:gesture-reveal-focus"
    open_portal(run, portal, "Reveal focus", count=2)
    run.click(f"{portal}/composer")
    run.expect("composer focused", on(portal, lambda p: p["focus"] == "composer"))
    run.glide(1, 1)
    run.publish(zone, key=key, ttl_ms=0, content={
        "body": "The full deployment result is visible on hover. " * 35,
        "actions": [{"label": "Inspect the deployment details " * 18, "callback_id": "inspect"}],
    })

    def card(status: dict) -> dict:
        return notification_entry(status, key) or {}

    def reveal(status: dict) -> dict:
        return card(status).get("reveal") or {}

    ready = run.expect("the renderer marks the body and label as cut", lambda st: (
        {source["field"] for source in card(st).get("reveal_sources", [])} == {"body", 0}
    ))
    original = card(ready)
    body = next(source["rect"] for source in original["reveal_sources"] if source["field"] == "body")
    run.glide(int(body["x"] + body["w"] / 2), int(body["y"] + body["h"] / 2))
    hovered = run.expect("hover reveals the full body without moving the card", lambda st: (
        reveal(st).get("field") == "body" and card(st)["rect"] == original["rect"]
        and card(st)["actions"][0]["rect"] == original["actions"][0]["rect"]
    ))
    run.snapshot("card-reveal.hover")
    panel = reveal(hovered)["rect"]
    run.glide(int(panel["x"] + panel["w"] / 2), int(panel["y"] + panel["h"] / 2))
    run.inj.click(5, repeat=2)
    run.expect("wheel scrolls inside the reveal panel", lambda st: reveal(st).get("scrolled"))
    run.snapshot("card-reveal.scrolled")
    run.glide(1, 1)
    run.expect("leaving closes the body reveal", lambda st: not reveal(st))

    for _ in range(16):
        run.key("Tab")
        focused = run.settle()
        if card(focused).get("focus") == 0:
            break
    else:
        raise ScenarioFailed("Tab reaches the clipped action label", run.status())
    run.expect("the focused action label reveals", lambda st: reveal(st).get("field") == 0)
    run.snapshot("card-reveal.keyboard")
    run.key("Escape")
    run.expect("the reveal closes", lambda st: not reveal(st))
    run.observe_for("Esc closes the reveal while focus stays", lambda st: (
        not reveal(st) and card(st).get("focus") == 0
    ), 0.3)
    run.key("Escape")

    run.publish(zone, key=key, ttl_ms=0, content={"body": "Fits"})
    fit = run.expect("the replacement fits", lambda st: card(st) and not card(st).get("reveal_sources"))
    bounds = card(fit)["rect"]
    run.glide(int(bounds["x"] + bounds["w"] / 2), int(bounds["y"] + bounds["h"] / 2))
    run.observe_for("fitting text never opens a reveal", lambda st: not reveal(st), 0.35)


def scenario_stack_trim(run: Run) -> None:
    """hud-ljhku.5 R5.2/R5.3: six cards, one an unanswered question; the question is kept.

    The trim spares the question and removes the oldest status card, whose
    publisher gets one displaced receipt. The kept question must remain
    visible and answerable during the burst, while the newer cards are
    still in the full stack at the production zone height.
    """
    question = "gesture-trim-question"
    open_card(run, question)
    statuses = [f"gesture-trim-s{i}" for i in range(5)]
    for key in statuses:
        run.publish("zone:notification-area", key=key, ttl_ms=0,
                    content={"body": f"Status {key[-1]}"})
    items = run.take(1)
    if [(i.get("kind"), i.get("key"), i.get("s")) for i in items] != \
            [("displaced", statuses[0], "zone:notification-area")]:
        raise ScenarioFailed("one displaced receipt, for the oldest status card",
                             {"items": items, "status": run.status()})
    run.expect("kept question visible in the full stack", lambda s: bool(
        (card := notification_entry(s, question)) and card["state"] != "exiting"
        and notification_entry(s, statuses[0]) is None))
    run.settle()
    run.click(f"zone:notification-area/{question}/0")
    take_card_answer(run, question, f"{question}-first")
    for key in statuses[1:]:
        is_error, reply = run.smoke.call("hud_clear", {"surface": "zone:notification-area", "key": key})
        if is_error:
            raise ScenarioFailed(f"hud_clear {key} refused", {"code": reply.get("code")})


def regain_owned_foreground(run: Run, what: str, timeout_s: float) -> None:
    """After a real link handoff, ShellExecute may have foregrounded a browser.
    Regain only the owned HUD, without injecting; the normal guard still runs
    before every later SendInput. Only link scenarios use this, never a
    focus-preservation assertion."""
    if run.injector != "sendinput":
        return
    if run.status().get("pid") != run.pid:
        raise Refused(f"owned HUD changed: {what}")

    def foreground():
        if WIN.foreground_pid() != run.pid:
            WIN.bring_to_front(run.pid)
        return WIN.foreground_pid() == run.pid, WIN.foreground_pid()
    wait_until(foreground, what=what, timeout_s=timeout_s, interval_s=0.02)


def expect_link_feedback(run: Run) -> None:
    """R3.2: a counted press says "Opening…" then "Opened". Off Windows the
    handoff succeeds at once, so "opening" may already have passed."""
    run.expect("the pressed link reports its handoff",
               lambda s: s["link_feedback"] in ("opening", "opened"), timeout_s=1.0, interval_s=0.02)
    run.expect("then says it opened", lambda s: s["link_feedback"] == "opened",
               timeout_s=1.0, interval_s=0.02)


def press_hosted_card_link(run: Run, key: str, label: str, point: tuple[int, int],
                           *, deadline: float, press: str) -> float:
    """Rearm the original press under an absolute deadline, with one native button submission."""
    if os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted":
        raise Refused("card link rearm requires a disposable GitHub-hosted runner")
    pid = run.pid
    surface = "zone:notification-area"
    target = f"{surface}/{key}/0"
    last: dict = {}

    point_step = f"owned HUD at the card link point before {press}"

    def point_details() -> dict:
        return {
            "intended_point": list(point), "owned_hud_pid": pid, "card_link_point": last,
            **foreign_link_point_details(last.get("at_point"), pid),
        }

    def unavailable() -> ScenarioFailed:
        return ScenarioFailed(point_step, point_details())

    def confirm_card() -> None:
        if time.monotonic() >= deadline:
            raise unavailable()
        status = run.status()
        check_windows_hud(github_actions=os.environ.get("GITHUB_ACTIONS"),
                          owned_hud_pid=pid if pid and run.pid == pid and pid_alive(pid) else None,
                          status_pid=status.get("pid"), url=run.smoke.url)
        cards = [card for card in status.get("notifications") or []
                 if card.get("s") == surface and card.get("key") == key and card.get("state") != "exiting"]
        card = cards[0] if len(cards) == 1 else {}
        actions = card.get("actions") or []
        try:
            current = resolve_target(status, target)
        except LookupError:
            current = None
        if (card.get("guarded") or len(actions) != 1 or actions[0].get("kind") != "url"
                or actions[0].get("label") != label or current != point):
            raise ScenarioFailed(f"same card link action before {press}", {
                "target": target, "intended_point": list(point), "current_point": current,
                "guarded": card.get("guarded"),
            })
        if time.monotonic() >= deadline:
            raise unavailable()

    def observe() -> dict:
        return {"at_point": WIN.window_at(point), "foreground": WIN.foreground_identity()}

    confirm_card()
    hwnd = WIN.fullscreen_window(pid)
    if hwnd is None:
        raise ScenarioFailed(f"unique owned fullscreen HUD before {press}", {"owned_hud_pid": pid})
    identity = {"hwnd": hwnd, "pid": pid}
    owned = {"at_point": identity, "foreground": identity}

    def wait_for_target() -> None:
        nonlocal last
        stable = 0
        requested = None
        while time.monotonic() < deadline:
            last = observe()
            if last == owned:
                stable += 1
                if stable == 2:
                    return
                requested = None
            else:
                stable = 0
                if last != requested:
                    confirm_card()
                    if WIN.fullscreen_window(pid) != hwnd:
                        raise ScenarioFailed(f"same owned fullscreen HUD before {press}", {
                            "owned_window": identity, "card_link_point": last,
                        })
                    if operation := WIN.rearm_link_window(hwnd, pid, point, deadline=deadline):
                        run.timeline.append({
                            "t": round(time.monotonic(), 3), "step": f"card link {press} requested {operation}",
                            "owned_window": identity, "intended_point": list(point), "card_link_point": last,
                        })
                        requested = last
            time.sleep(max(0, min(0.02, deadline - time.monotonic())))
        raise unavailable()

    def confirm_input(phase: str) -> None:
        nonlocal last
        confirm_card()
        last = observe()
        if time.monotonic() >= deadline:
            raise unavailable()
        if last != owned:
            raise LinkTargetLost(phase, point_step, point_details())

    while True:
        wait_for_target()
        operation = "move"
        try:
            # Readiness is sampled at the intended pixel before any pointer movement.
            if not cursor_at_link(WIN.click_target_snapshot()["cursor"], point):
                run.inj.move_abs(*point, before_send=lambda: confirm_input("move mapped point"))
            operation = "button"
            pressed = time.monotonic()
            run.click_link(point, confirm=lambda: confirm_input("button mapped point"), hwnd=hwnd, deadline=deadline)
        except (ForeignForeground, LinkTargetLost) as refusal:
            # Both refusals originate from guards before the native call.
            phase, observed = ((refusal.phase, refusal.last) if isinstance(refusal, LinkTargetLost)
                               else (f"{operation} foreground guard", {"foreground_pid": refusal.pid}))
            run.timeline.append({
                "t": round(time.monotonic(), 3), "step": f"card link {press} pre-submission ownership lost",
                "guard_phase": phase, "observed": observed,
            })
        else:
            break
    receipt = run.inj.calls[-1]
    if 0 < receipt["sent"] < receipt["requested"]:
        raise PartialButtonBatch(f"partial native button batch for the card link {press}")
    if receipt["sent"] != receipt["requested"]:
        raise ScenarioFailed(f"complete native input for the card link {press}", {"sendinput": receipt})
    return pressed


def scenario_card_link(run: Run) -> None:
    """R14.2 S4/S5: one real link handoff; same action within 2s is refused, with no input."""
    key = "gesture-card-link"
    label = "Open test page"
    target = f"zone:notification-area/{key}/0"
    hosted = run.injector == "sendinput"
    open_card(run, key, actions=[{"label": label, "url": "https://example.invalid/"}], park_pointer=not hosted)
    if hosted:
        status = run.expect("card link clear of its publication press guard", lambda s: bool(
            (card := notification_entry(s, key)) and not card.get("guarded")))
    else:
        status = run.status()
    before = status["link_launches"]
    if hosted:
        # The preceding portal scenario can still be bringing up its browser.
        point = resolve_target(status, target)
        first = press_hosted_card_link(run, key, label, point, deadline=time.monotonic() + 2.0, press="first press")
    else:
        point = run.point(target)
        first = time.monotonic()
        run.click_link(point)
    run.expect("one accepted link launch", lambda s: s["link_launches"] == before + 1,
               timeout_s=1.0, interval_s=0.02)
    expect_link_feedback(run)
    if hosted:
        press_hosted_card_link(run, key, label, point, deadline=first + 1.8, press="second press")
    else:
        run.click_link(point)
    second = time.monotonic()
    if second - first >= 2.0:
        raise ScenarioFailed("second link press must occur within 2s", {"seconds": second - first, "status": run.status()})
    run.step(f"same link pressed again after {second - first:.3f}s")
    run.observe_for("link rate limit", lambda s: s["link_launches"] == before + 1
                    and notification_entry(s, key) is not None, 0.3)
    if items := run.inputs():
        raise ScenarioFailed("links create no hud_input item", {"items": items, "status": run.status()})


def scenario_portal_reading(run: Run) -> None:
    """hud-ljhku.2: OUTPUT links open on release, drags select, Ctrl+C copies, cursors follow."""
    s = "portal:g-reading"
    words = " ".join(["words"] * 4)
    content = (f"[alpha {words}](https://a.example.invalid/)\n"
               f"[bravo {words}](https://b.example.invalid/)\n"
               f"Line three {words}\nLine four {words}\nLine five {words}")
    run.publish(s, content=content, ttl_ms=0)
    run.expect(f"{s} shows its text", on(s, lambda p: p["output"]["at_tail"]
                                         and (p["output"]["last_visible_line"] or "").startswith("Line five")))
    run.settle()
    out = {k: round(v) for k, v in run.portal(s)["panes"]["output"].items()}
    x = out["x"] + 30

    def at(y: int) -> dict:
        run.glide(x, y)
        time.sleep(0.06)
        return run.portal(s)

    # Find each link row from the reported hint alone (no layout guessing).
    rows: dict[str, int] = {}
    for y in range(out["y"] + 2, out["y"] + min(out["h"], 240), 2):
        hint = at(y)["link_hint"]
        if hint and hint not in rows:
            rows[hint] = y
        if len(rows) == 2 and hint is None:
            break
    a, b = rows.get("a.example.invalid"), rows.get("b.example.invalid")
    if a is None or b is None:
        raise ScenarioFailed("hover finds both links by their host hint", {"rows": rows, "portal": run.portal(s)})
    pitch = b - a
    a, b = a + pitch // 2 - 1, b + pitch // 2 - 1  # row centres
    run.glide(x, b)
    run.expect("hand cursor and host hint over a link (R2.1 S8, R2.4 S1)",
               on(s, lambda p: p["cursor"] == "hand" and p["link_hint"] == "b.example.invalid"))
    text_y = b + pitch  # "Line three"
    run.glide(x, text_y)
    run.expect("I-beam over text, no hint", on(s, lambda p: p["cursor"] == "ibeam" and p["link_hint"] is None))
    run.glide(out["x"] + out["w"] - 6, text_y)
    run.expect("arrow over blank OUTPUT", on(s, lambda p: p["cursor"] == "arrow"))

    launches = run.status()["link_launches"]
    run.drag((x, a), (x + 120, a))
    run.expect("a press-drag on a link selects (R2.1 S2)", on(s, lambda p: p["selection_chars"] > 0))
    run.observe_for("and launches nothing", lambda st: st["link_launches"] == launches, 0.3)
    run.key("Escape")
    run.expect("Esc clears the selection (R2.2 S4)", on(s, lambda p: p["selection_chars"] == 0))

    writes = run.status()["clipboard_writes"]
    time.sleep(0.7)
    run.drag((x, text_y), (x + 60, text_y + 2 * pitch))
    selected = run.expect("a three-line drag selects (R2.2 S1)",
                          on(s, lambda p: p["selection_chars"] > len("Line three ")))
    run.key("ctrl+c")
    run.expect("Ctrl+C writes the clipboard once with the selection kept",
               lambda st: st["clipboard_writes"] == writes + 1
               and portal_entry(st, s)["selection_chars"] == portal_entry(selected, s)["selection_chars"])
    if WINDOWS:  # tier W: the real clipboard holds the selected plain text
        text = (WIN.clipboard_text() or "").replace("\r\n", "\n")  # CF_UNICODETEXT is CRLF
        if not ("\nLine four words words words words\n" in text and len(text) == portal_entry(selected, s)["selection_chars"]):
            raise ScenarioFailed("the clipboard holds the three selected lines", {"chars": len(text)})
    # hud-ohgc9.4 R4.2: the selection holds reading focus, so Home and End scroll
    # OUTPUT (first and last lines visible) and keep the selection.
    chars = portal_entry(selected, s)["selection_chars"]
    run.key("Home")
    run.expect("Home shows the first OUTPUT line",
               on(s, lambda p: (p["output"]["first_visible_line"] or "").startswith("alpha")
                  and p["selection_chars"] == chars))
    run.key("End")
    run.expect("End shows the last OUTPUT line",
               on(s, lambda p: (p["output"]["last_visible_line"] or "").startswith("Line five")
                  and p["selection_chars"] == chars))
    run.key("Escape")
    run.expect("selection cleared", on(s, lambda p: p["selection_chars"] == 0))

    # Real link handoffs follow the other portal input. A browser can take
    # the foreground after the handoff has returned.
    time.sleep(0.7)  # past the double-click interval
    run.glide(x, a)
    run.mark()
    run.click_link((x, a))
    run.expect("a link click launches once (R2.1 S1)", lambda st: st["link_launches"] == launches + 1)
    expect_link_feedback(run)
    time.sleep(0.7)
    regain_owned_foreground(run, "owned HUD foreground before the link double-click", 1.0)
    run.glide(x, b)
    run.click_link((x, b), repeat=2)
    run.expect("a link double-click launches once and selects nothing (R2.1 S3)",
               lambda st: st["link_launches"] == launches + 2 and portal_entry(st, s)["selection_chars"] == 0)
    if items := run.inputs():
        raise ScenarioFailed("OUTPUT reading creates no hud_input item", {"items": items})


def scenario_widget_hover(run: Run) -> None:
    """R14.2 S6: status and gauge declared targets reveal after dwell and hide on leave."""
    for surface, params in [
        ("widget:main-status", {"status": "online", "reason": "Owned hover check"}),
        ("widget:main-gauge", {"level": 0.25, "label": "CPU", "readout": "25/100 (25%)", "severity": "info"}),
    ]:
        run.publish(surface, ttl_ms=0, params=params)
        run.expect(f"{surface} rendered", lambda s: widget_entry(s, surface) is not None)
        rect = widget_entry(run.status(), surface)["rect"]
        # Status has a non-hover caption in its quad. The gauge's trigger is
        # the whole card, so leave its reported box instead of guessing pixels.
        outside = (round(rect["x"] + 1), round(rect["y"] + rect["h"] - 1))
        if surface == "widget:main-gauge":
            outside = (round(rect["x"] - 4), round(rect["y"] + rect["h"] + 4))
        run.glide(*outside)
        run.expect(f"{surface} tooltip initially hidden", lambda s: not widget_entry(s, surface)["tooltip_visible"])
        run.point(f"{surface}/hover")
        run.expect(f"{surface} tooltip after 180ms dwell", lambda s: widget_entry(s, surface)["tooltip_visible"])
        run.glide(*outside)
        run.expect(f"{surface} tooltip hidden on leave", lambda s: not widget_entry(s, surface)["tooltip_visible"])


def shrink(run: Run, s: str, times: int = 4) -> dict:
    run.click(f"{s}/composer")
    run.expect("composer focused", on(s, lambda p: p["focus"] == "composer"))
    before = portal_entry(run.status(), s)["rect"]
    for _ in range(times):
        run.key("ctrl+minus")
    run.expect("Ctrl+- shrinks the portal",
               on(s, lambda p: p["rect"]["w"] < before["w"] or p["rect"]["h"] < before["h"]))
    # The first shrink can show before the later presses land; return only
    # once they all have, so the caller's next resize starts from rest.
    return run.settle()


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
    run.expect_cursor(f"{s}/close", hand=True)  # beats the header band's open hand
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


def restart_owned(run: Run) -> dict:
    """POST /admin/restart, own the replacement and wait out the old instance."""
    old = run.pid
    if run.mouse_receipts:
        run.mouse_receipts.end_interval()
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
    if old == run.hud.pid:
        try:
            run.hud.wait(30)
        except subprocess.TimeoutExpired:
            raise ScenarioFailed("old instance exits", {"pid": old}) from None
    else:
        wait_until(lambda: (not pid_alive(old), old), what="old instance exits", timeout_s=30)
    if run.mouse_receipts:
        run.mouse_receipts.observe(status, run.pid)
    return status


def scenario_restart(run: Run) -> None:
    """14. Across a restart the client sees unavailable, rediscovers, and nothing replays."""
    s = "portal:g-restart"
    open_portal(run, s, "x", count=5)
    restart_owned(run)
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



def scenario_go_to_session(run: Run) -> None:
    """Go to switches to the exact owned window without moving it and stays absent for two matches."""
    zone, key = "zone:notification-area", "gesture-go-to-session"
    token = "tzgo-" + secrets.token_hex(4)
    notepads = []
    session = SessionSmoke(run.smoke.url, run.psk, token)

    def open_editor(name):
        # Keep the token bounded whether this Notepad includes the extension
        # in its title. The acceptance check requires distinct top-level
        # windows, each owned by the harness.
        path = run.home / (name + ".txt")
        path.write_text("Go to session gesture\n", encoding="utf-8")
        editor = subprocess.Popen(["notepad.exe", str(path)])
        notepads.append(editor)
        run.helper_pids.append(editor.pid)

        def opened():
            info = WIN.window_snapshot(editor.pid)
            return info is not None and name in info["title"], info

        info, _ = wait_until(opened, what=f"owned Notepad window for {name}",
                             timeout_s=15, interval_s=0.1)
        return info

    def question(body):
        run.mark()
        session.ok("hud_publish", {"surface": zone, "key": key, "ttl_ms": 0, "content": {
            "body": body, "actions": [{"label": "Keep", "callback_id": "kept"}]}})
        run.step(f"publish scoped Go to card {key}")

    try:
        target = open_editor(token + " target")
        other = open_editor("Unmatched gesture editor")

        def foreground_other():
            foreground = WIN.foreground_pid()
            if foreground != other["pid"]:
                WIN.bring_to_front(other["pid"])  # Window request on an owned process only.
            now = WIN.foreground_pid()
            return now == other["pid"], now

        wait_until(foreground_other, what="owned unmatched editor foreground",
                   timeout_s=SETTLE_S, interval_s=0.05)
        run.glide(1, 1)
        before = run.status()["window_activations"]
        question("Follow the session")
        run.expect("the uniquely matched Go to is drawn and ready", lambda st: bool(
            (card := notification_entry(st, key)) and card["goto"] and not card["guarded"]))
        if WIN.foreground_pid() != other["pid"] or run.status()["window_activations"] != before:
            raise ScenarioFailed("publication kept the current foreground", {"before": before,
                                 "foreground": WIN.foreground_snapshot()})
        run.expect_cursor(f"{zone}/{key}/goto", hand=True)
        run.click()
        run.expect("exact Go to window activation", lambda st: (
            st["window_activations"] == before + 1 and WIN.foreground_snapshot() == target))
        run.step(f"ok exact foreground HWND, title and unchanged rect {target['rect']}")
        run.expect("the Go to card remains live", lambda st: bool(
            (card := notification_entry(st, key)) and card["goto"] and card["chosen"] is None))
        if session.ok("hud_input", {}).get("items"):
            raise ScenarioFailed("Go to emits no scoped input", {})

        duplicate = open_editor(token + " duplicate")
        if duplicate["hwnd"] == target["hwnd"]:
            raise ScenarioFailed("two distinct eligible Notepad windows", {"target": target, "duplicate": duplicate})
        run.glide(1, 1)
        question("Two Windows titles match")
        run.settle()
        run.observe_for("two eligible windows leave the replacement without Go to", lambda st: bool(
            (card := notification_entry(st, key)) and card["goto"] is None
            and st["window_activations"] == before + 1), 0.25)
    finally:
        session.end()
        for editor in reversed(notepads):
            kill_pid(editor.pid)
            editor.wait(10)
            run.helper_pids.remove(editor.pid)

def expect_overlay_capture(run: Run, wanted: bool, what: str) -> None:
    def sample():
        state = WIN.pointer_capture_state(run.pid)
        return state["capturing"] is wanted and state["no_activate"] is True, state
    state, _ = wait_until(sample, what=what + " with WS_EX_NOACTIVATE", timeout_s=SETTLE_S)
    run.step(f"ok {what}: {state}")


def live_windows_owners(run: Run) -> list[int]:
    return sorted({pid for pid in (run.pid, *run.helper_pids) if pid and pid_alive(pid)})


def alt_tab_to_owned_editor(run: Run, editor_pids: set[int], attempt: int) -> int:
    before = WIN.foreground_identity()
    before_pid = before["pid"] if before else None
    observation = {"attempt": attempt, "before": before, "eligible_editor_pids": sorted(editor_pids),
                   "live_owned_pids_before": live_windows_owners(run)}
    last = None

    def switched_owned_editor():
        nonlocal last
        last = WIN.foreground_identity()
        # A changed PID is insufficient. Observe one of this test's two editors.
        return bool(last and last["pid"] in editor_pids and last["pid"] != before_pid), last

    try:
        run.key("alt+Tab")
        try:
            switched, _ = wait_until(switched_owned_editor, what="real Alt-Tab reaches an owned editor",
                                     timeout_s=SETTLE_S)
        except AssertionError as error:
            raise ScenarioFailed(str(error), observation) from error
        return switched["pid"]
    finally:
        observation["last"] = last
        observation["live_owned_pids_after"] = live_windows_owners(run)
        run.step("real Alt-Tab foreground")
        run.timeline[-1]["alt_tab_foreground"] = observation


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
    # hud-ohgc9.5 R5.2: with no card, the chord reaches the portal whose latest
    # output still asks for a reply; its outer Esc returns to Notepad.
    run.key("ctrl+shift+F9")
    run.expect("no card: the chord focuses the waiting portal's composer",
               on(surface, lambda p: p["focus"] == "composer"))
    foreground_is(run.pid, "the chord takes keyboard focus for the composer")
    run.key("Escape")
    foreground_is(notepad_pid, "composer Esc returns to the prior app")
    run.type(" portal")
    expected += " portal"
    wait_until(lambda: (WIN.edit_text(notepad_pid) == expected, WIN.edit_text(notepad_pid)),
               what="next key after the portal visit reaches Notepad", timeout_s=SETTLE_S)
    run.publish(surface, content="\nno reply needed")
    run.expect("the portal no longer waits for a reply", on(
        surface, lambda p: (p["output"]["last_visible_line"] or "").startswith("no reply needed")))
    run.key("ctrl+shift+F9")
    run.observe_for("no card and no waiting portal means no foreground change",
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
        # Depending on Windows' current selection, one switch reaches the
        # editor or its immediately preceding owned helper. This checks real
        # Alt-Tab focus transfer without inspecting the switcher list.
        switches = []
        for attempt in (1, 2):
            switched = alt_tab_to_owned_editor(run, {notepad_pid, third.pid}, attempt)
            switches.append(switched)
            if switched == third.pid:
                break
        final_foreground = WIN.foreground_identity()
        if final_foreground is None or final_foreground["pid"] != third.pid:
            raise ScenarioFailed("Alt-Tab reaches the third owned app",
                                 {"switches": switches, "foreground": final_foreground,
                                  "live_owned_pids": live_windows_owners(run)})
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


def release_owned_safe_mode(run: Run) -> None:
    """Undo this scenario's own pause after a failure so later scenarios do not
    inherit SAFE_MODE_ACTIVE. Best effort: the original failure is what reports."""
    try:
        if run.status().get("safe_mode") is True:
            run.key("ctrl+shift+F12")
            run.expect("a failed pause witness releases its own safe mode",
                       lambda st: st.get("safe_mode") is False)
    except Exception as error:  # noqa: BLE001 - never mask the scenario failure
        run.step(f"safe-mode release after failure did not complete: {error}")


def scenario_safe_mode(run: Run) -> None:
    """12. The real pause chord preserves an unheld portal beyond idle/reap expiry, then resumes without reap."""
    s = "portal:g-safe"
    open_portal(run, s, "m", count=5, ttl_ms=None)
    intact = on(s, lambda p: p["state"] == "Active" and tail_line("m", 4)(p))
    run.key("ctrl+shift+F12")
    # From our pause chord until its resume is observed, any failure releases
    # the pause this scenario owns so later scenarios never inherit it.
    try:
        run.expect("the pause chord sets safe mode", lambda st: st.get("safe_mode") is True)
        is_error, reply = run.smoke.call("hud_publish", {"surface": s, "content": "\nwhile paused"})
        if not is_error or reply.get("code") != "SAFE_MODE_ACTIVE":
            raise ScenarioFailed("publishing while paused returns SAFE_MODE_ACTIVE", {"reply": reply})
        run.inputs()  # exercise availability BEFORE the measured no-refresh interval
        run.snapshot("safe-mode.paused")
        paused = time.monotonic()
        # Compare absolute monotonic instants, as observe_for does: `now - paused`
        # can round below 65 at the exact instant `now >= paused + 65` holds.
        reap_deadline = paused + 65.0
        run.observe_for("safe mode preserves the unheld portal past the normal 30s+30s reap deadline",
                        lambda st: st.get("safe_mode") is True and intact(st), 65.0)
        ended = time.monotonic()
        if ended < reap_deadline:
            raise ScenarioFailed("pause observation spans the normal reap deadline",
                                 {"paused_s": ended - paused})
        paused_s = ended - paused
        run.key("ctrl+shift+F12")
        run.expect("pressed again it resumes", lambda st: st.get("safe_mode") is False)
    except BaseException:
        release_owned_safe_mode(run)
        raise
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
    """Windows tray registration/tooltip, real hide chord, idle proof; real native Hide/Show when the icon is exposed."""
    status = run.expect("tray registered", lambda st: st.get("tray", {}).get("registered") is True)

    def hud_foreground():
        hud_pid = run.pid
        if not hud_pid or not pid_alive(hud_pid):
            raise Refused("no owned HUD process")
        pid = WIN.foreground_pid()
        if pid != hud_pid:
            WIN.bring_to_front(hud_pid)
            pid = WIN.foreground_pid()
        return pid == hud_pid, pid

    # Keep an owned foreground app when available. Startup and a previous
    # scenario's cleanup can leave a foreign app active.
    if (status.get("hide_hotkey") or {}).get("registered") is True:
        chord = status["hide_hotkey"]["chord"].lower()
        foreground = WIN.foreground_pid()
        if foreground not in live_windows_owners(run):
            foreground, _ = wait_until(hud_foreground, what="owned HUD foreground before hide chord",
                                       timeout_s=SETTLE_S)
        for hidden in (True, False):
            run.key(chord)
            # The pill names what is hidden while the panel is closed.
            pill = "Everything hidden" if hidden else None
            run.expect("hide chord hides everything, pill shown" if hidden
                       else "hide chord shows everything, pill gone",
                       lambda st: st["tray"].get("all_hidden") is hidden
                       and st["tray"].get("hidden_notice") == pill)
            if WIN.foreground_pid() != foreground:
                raise ScenarioFailed("the hide chord leaves the foreground app as it was",
                                     {"before": foreground, "after": WIN.foreground_pid()})
        run.step(f"ok real hide chord {chord}: Hide all and back, pill shown, foreground kept")
    else:
        run.step(f"hide chord not registered: {json.dumps(status.get('hide_hotkey'))}")
    tray = status["tray"]
    tooltip = tray.get("tooltip", "")
    if len(tooltip.splitlines()) != 2 or len(tooltip.encode("utf-16-le")) > 254:
        raise ScenarioFailed("two-line bounded tray tooltip", tray)
    idle = run.settle()
    frames, since = idle["frames_presented"], idle["idle_since_ms"]
    run.observe_for("tray schedules no idle frames", lambda st:
                    st["frames_presented"] == frames and st["idle_since_ms"] >= since, 0.5)
    wait_until(hud_foreground, what="owned HUD foreground before tray input", timeout_s=SETTLE_S)
    probe = {}
    try:
        rect = WIN.tray_rect(tray["hwnd"], tray["icon_id"], run.pid,
                             expected_tooltip=tooltip, diagnostics=probe)
    finally:
        run.step(f"owned tray visibility probe {json.dumps(probe)}")
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


def scenario_tray_answer(run: Run) -> None:
    """Tray left click with a question waiting focuses it; Esc returns keys to Notepad (informational)."""
    run.expect("tray registered", lambda st: st.get("tray", {}).get("registered") is True)
    notepad = subprocess.Popen(["notepad.exe"])
    run.helper_pids.append(notepad.pid)
    try:
        def foreground():
            if WIN.foreground_pid() != notepad.pid:
                WIN.bring_to_front(notepad.pid)  # a window request, not injected input
            return WIN.foreground_pid() == notepad.pid, WIN.foreground_pid()
        wait_until(foreground, what="owned Notepad foreground", timeout_s=15, interval_s=0.2)
        key = "gesture-tray-answer"
        open_card(run, key)
        status = run.expect("a left click would answer", lambda st: st["tray"].get("click") == "answer")
        tray = status["tray"]
        if not tray["tooltip"].splitlines()[0].endswith(" · click to answer"):
            raise ScenarioFailed("the tooltip announces what a click does", tray)
        probe = {}
        try:
            rect = WIN.tray_rect(tray["hwnd"], tray["icon_id"], run.pid,
                                 expected_tooltip=tray["tooltip"], diagnostics=probe)
        finally:
            run.step(f"owned tray visibility probe {json.dumps(probe)}")
        if rect is None:
            run.step("limited tray-answer proof: click rule and tooltip passed; icon in overflow, no click injected")
            return
        wait_until(foreground, what="owned Notepad foreground before the tray click", timeout_s=15, interval_s=0.2)
        run.inj.move_abs((rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2)
        run.inj.click()
        run.expect("tray click focuses the question's first action", lambda st: bool(
            (card := notification_entry(st, key)) and card["focus"] == 0
            and WIN.foreground_pid() == run.pid))
        run.key("Escape")
        wait_until(lambda: (WIN.foreground_pid() == notepad.pid, WIN.foreground_pid()),
                   what="Esc returns to the app that was in front", timeout_s=SETTLE_S)
        run.expect("Esc leaves the card unanswered", lambda st: bool(
            (card := notification_entry(st, key)) and card["focus"] is None and card["chosen"] is None))
        run.type("after tray")
        wait_until(lambda: (WIN.edit_text(notepad.pid) == "after tray", WIN.edit_text(notepad.pid)),
                   what="the next keystroke reaches Notepad", timeout_s=SETTLE_S)
        run.step("ok real tray click answers; Esc returns keys to Notepad")
    finally:
        kill_pid(notepad.pid)
        notepad.wait(10)
        run.helper_pids.remove(notepad.pid)


CAPTURE_RGB = (255, 0, 255)  # magenta: nothing else on a CI desktop is this colour


def colour_pixels(frame: tuple[int, int, bytes], rgb: tuple[int, int, int], tolerance: int = 12,
                  step: int = 3) -> int:
    """Sampled pixels within `tolerance` of `rgb` (every `step`th pixel each way)."""
    width, height, rgba = frame
    count = 0
    for y in range(0, height, step):
        row = y * width * 4
        for x in range(0, width, step):
            i = row + x * 4
            if all(abs(rgba[i + c] - rgb[c]) <= tolerance for c in range(3)):
                count += 1
    return count


def set_exclude_from_capture(run: Run, original: str, on: bool) -> None:
    """Rewrite the owned config with `[runtime] exclude_from_capture`, or restore it."""
    text = original
    if on:
        text = original.replace("[runtime]\n", "[runtime]\nexclude_from_capture = true\n", 1)
        if text == original:
            text = original + "\n[runtime]\nexclude_from_capture = true\n"
    run.config.write_text(text, encoding="utf-8")
    run.step(f"config exclude_from_capture = {str(on).lower()}")


def prove_excluded(run: Run, show_colour: Callable[[], None]) -> None:
    """With exclusion applied the desktop capture lacks the colour; /admin/screenshot keeps it."""
    run.expect("capture exclusion applied",
               lambda st: (st.get("capture_exclusion") or {}).get("applied") is True)
    show_colour()
    on_desktop = colour_pixels(WIN.desktop_rgba(), CAPTURE_RGB)
    in_screenshot = colour_pixels(run.frame(), CAPTURE_RGB)
    run.screen_grab("capture-exclusion.on")
    if on_desktop:
        raise ScenarioFailed("an excluded HUD is absent from the desktop capture",
                             {"desktop_pixels": on_desktop})
    if not in_screenshot:
        raise ScenarioFailed("/admin/screenshot still shows the excluded HUD", {"frame_pixels": 0})
    run.step(f"ok excluded: desktop 0 pixels, /admin/screenshot {in_screenshot}")


def restore_capture_setting(run: Run, original: str) -> str | None:
    """Restore the config and restart; the error text, logged, when that fails."""
    try:
        set_exclude_from_capture(run, original, False)
        restart_owned(run)
    except Exception as err:  # noqa: BLE001 - reported by the caller, never masks a failure
        error = f"{type(err).__name__}: {err}"
        run.step(f"restore restart failed: {error}")
        return error
    return None


def scenario_capture_exclusion(run: Run) -> None:
    """R5.2 S1: Hide from screen capture removes the HUD from desktop captures, not /admin/screenshot."""
    pip = "zone:pip"
    if not any(e["s"] == pip for e in run.smoke.ok("hud_surfaces")["surfaces"]):
        run.step(f"skipped: no {pip} to publish a solid colour into")
        return
    r, g, b = (c / 255 for c in CAPTURE_RGB)

    def show_colour() -> None:
        run.publish(pip, content={"type": "solid_color", "r": r, "g": g, "b": b})
        run.settle()

    show_colour()
    shown = colour_pixels(WIN.desktop_rgba(), CAPTURE_RGB)
    if shown == 0:
        raise ScenarioFailed("the published colour is on the desktop before exclusion", {"pixels": 0})
    run.step(f"ok desktop shows the colour ({shown} sampled pixels)")

    original = run.config.read_text(encoding="utf-8")
    try:
        set_exclude_from_capture(run, original, True)
        status = restart_owned(run)
        exclusion = status.get("capture_exclusion") or {}
        if exclusion.get("supported") is False or exclusion.get("reason") == "remote":
            run.step(f"limited proof: exclusion not in force here: {json.dumps(exclusion)}")
        else:
            prove_excluded(run, show_colour)
    finally:
        # A failed restore restart is logged, never raised here: it must not
        # mask the ScenarioFailed that may already be propagating.
        restore_error = restore_capture_setting(run, original)
    if restore_error:
        raise ScenarioFailed("the HUD restarts with the setting restored", {"error": restore_error})
    show_colour()
    if not colour_pixels(WIN.desktop_rgba(), CAPTURE_RGB):
        raise ScenarioFailed("the colour is back on the desktop once the setting is restored", {})
    run.step("ok restored: the desktop shows the HUD again")


def scenario_overlay_shell(run: Run) -> None:
    """Native overlay styles, focused Alt+F4 survival, capture affinity and desktop switching when available."""
    editors = []
    key = "gesture-overlay-shell"

    def open_editor():
        editor = subprocess.Popen(["notepad.exe"])
        editors.append(editor)
        run.helper_pids.append(editor.pid)

        def foreground():
            if WIN.foreground_pid() != editor.pid:
                WIN.bring_to_front(editor.pid)
            return WIN.foreground_pid() == editor.pid, WIN.foreground_pid()

        wait_until(foreground, what="owned desktop test editor foreground", timeout_s=SETTLE_S)
        return editor.pid

    def style(capturing):
        def observed():
            state = WIN.pointer_capture_state(run.pid)
            return (state["capturing"] is capturing and state["no_activate"] is True
                    and state["tool_window"] is True and state["app_window"] is False), state

        state, _ = wait_until(observed, what="native topmost tool-window policy",
                              timeout_s=SETTLE_S)
        run.step(f"ok topmost overlay styles, capturing={capturing}: {state}")

    try:
        original_desktop_editor = open_editor()
        open_card(run, key)
        style(False)
        run.point(f"zone:notification-area/{key}/0")
        run.inj.move_relative(1, 0)
        style(True)
        run.glide(1, 1)
        style(False)
        run.expect("admin status sees a visible topmost overlay", lambda st: bool(
            (native := (st["displays"][0].get("window_state") or {}))
            and native.get("topmost") is True and native.get("visible") is True))

        run.key("ctrl+shift+F9")
        run.expect("answer chord focuses the owned HUD card", lambda st: bool(
            (card := notification_entry(st, key)) and card["focus"] == 0
            and WIN.foreground_pid() == run.pid))
        pid = run.pid
        message = "close request ignored on an overlay; quit from the tray"
        before = run.log.read_text(encoding="utf-8", errors="replace").count(message)
        run.key("alt+F4")
        run.expect("one overlay close ignored and the focused card stays", lambda st: bool(
            st["pid"] == pid and (card := notification_entry(st, key))
            and card["focus"] == 0 and WIN.foreground_pid() == pid
            and run.log.read_text(encoding="utf-8", errors="replace").count(message) == before + 1))
        run.key("Escape")
        wait_until(lambda: (WIN.foreground_pid() == original_desktop_editor, WIN.foreground_pid()),
                   what="outer Esc restores the owned editor", timeout_s=SETTLE_S)

        # Capture affinity runs before the desktop probe, whose physical
        # capture is the least certain leg on a hosted VM.
        original_config = run.config.read_text(encoding="utf-8")
        restore_error = None
        try:
            set_exclude_from_capture(run, original_config, True)
            status = restart_owned(run)
            report = status.get("capture_exclusion") or {}
            if report.get("supported") is False or report.get("reason") == "remote":
                run.step(f"limited capture-affinity proof on this session: {report}")
            else:
                run.expect("exclusion still applied after installing the tool-window policy", lambda st:
                           (st.get("capture_exclusion") or {}).get("applied") is True)
            style(False)
        finally:
            restore_error = restore_capture_setting(run, original_config)
        if restore_error:
            raise ScenarioFailed("overlay restarted with capture setting restored", {"error": restore_error})
        open_card(run, key)
        overlay_shell_desktop_probe(run, key, original_desktop_editor, open_editor)
    finally:
        for editor in reversed(editors):
            kill_pid(editor.pid)
            editor.wait(10)
            run.helper_pids.remove(editor.pid)


def prove_overlay_shell_desktop_baseline(run: Run, editor_pid: int,
                                        editor_is_current: Callable[[], bool | None]) -> bool:
    """True once the physical desktop shows the swatch; False records a capture limit."""
    observation = {"editor_pid": editor_pid, "compositor_pixels": colour_pixels(run.frame(), CAPTURE_RGB)}
    if not observation["compositor_pixels"]:
        raise ScenarioFailed("the HUD renders the test swatch before the desktop switch", observation)

    if WIN.foreground_pid() != editor_pid and editor_is_current() is True:
        WIN.bring_to_front(editor_pid)
    capture = None

    def observe():
        nonlocal capture
        foreground_before = WIN.foreground_pid()
        capture = WIN.desktop_rgba()
        foreground_after = WIN.foreground_pid()
        desktop_pixels = colour_pixels(capture, CAPTURE_RGB)
        observation.update(foreground_before=foreground_before, foreground_after=foreground_after,
                           desktop_pixels=desktop_pixels)
        return foreground_before == foreground_after == editor_pid and desktop_pixels > 0, observation

    what = "the HUD test swatch is visible before the desktop switch"
    try:
        wait_until(observe, what=what, timeout_s=SETTLE_S, interval_s=0.1)
    except AssertionError:
        stable = observation.get("foreground_before") == observation.get("foreground_after") == editor_pid
        if not (stable and observation.get("desktop_pixels") == 0):
            raise ScenarioFailed(what, observation) from None
        # Hosted runs drew the swatch in the compositor while the desktop
        # capture had none under an unchanged owned foreground; fullscreen sees
        # it with the same helper. Without a desktop baseline no switch is
        # proved; the owner's physical check covers virtual desktops.
        run.step(f"limited desktop proof: the desktop capture lacks the compositor swatch: {observation}")
        return False
    finally:
        if capture is not None:
            run.screen_grab("overlay-shell.before-switch", frame=capture)
    run.step(f"ok {what}: {observation}")
    return True


def overlay_shell_desktop_probe(run: Run, card_key: str, original_editor: int, open_editor: Callable[[], int]) -> None:
    """Prove a desktop switch on an ordinary owned HWND and HUD pixels on the physical display."""
    original_window = WIN.owned_window_handle(original_editor)

    def original_is_current() -> bool | None:
        if original_window is None:
            return None
        return WIN.owned_window_on_current_desktop(original_editor, original_window)

    def observe_original(wanted: bool) -> tuple[bool, bool | None]:
        current = original_is_current()
        return current is wanted, current

    def observe_restored():
        current, foreground = original_is_current(), WIN.foreground_pid()
        return current is True and foreground == original_editor, (current, foreground)

    if original_is_current() is not True:
        run.step("limited desktop proof: the runner cannot identify the owned editor's active virtual desktop")
        return
    pip = "zone:pip"
    r, g, b = (c / 255 for c in CAPTURE_RGB)
    run.publish(pip, content={"type": "solid_color", "r": r, "g": g, "b": b})
    run.settle()
    if not prove_overlay_shell_desktop_baseline(run, original_editor, original_is_current):
        return
    if original_is_current() is not True:
        run.step("limited desktop proof: the original virtual desktop could not be confirmed before switching")
        return

    switch_confirmed = False
    try:
        run.key("win+ctrl+d")
        try:
            wait_until(lambda: observe_original(False), what="owned editor is off the current desktop",
                       timeout_s=3, interval_s=0.1)
        except AssertionError:
            pass
        else:
            switch_confirmed = True
            wait_until(lambda: (colour_pixels(WIN.desktop_rgba(), CAPTURE_RGB) > 0, None),
                       what="the HUD swatch is visible on the second desktop", timeout_s=SETTLE_S)
            status = run.expect("the question remains live on the second desktop", lambda st: bool(
                notification_entry(st, card_key)))
            run.screen_grab("overlay-shell.second-desktop")
            run.step(f"ok switched desktops and observed HUD pixels; window_state="
                     f"{[d.get('window_state') for d in status['displays']]}")
    finally:
        current = original_is_current()
        if current is None or (current is True and not switch_confirmed):
            # The shell may process the new-desktop chord after the proof deadline.
            try:
                current, _ = wait_until(lambda: observe_original(False), what="late virtual desktop switch",
                                        timeout_s=SETTLE_S, interval_s=0.1)
            except AssertionError:
                current = original_is_current()
        try:
            if current is None:
                raise AssertionError("the original editor's desktop membership is unknown")
            if current is False:
                # Guard the cleanup chord with a new owned foreground on the switched desktop.
                cleanup_editor = open_editor()
                cleanup_window = WIN.owned_window_handle(cleanup_editor)
                current = original_is_current()
                if current is not True:
                    if (current is not False or cleanup_window is None
                            or WIN.owned_window_on_current_desktop(cleanup_editor, cleanup_window) is not True):
                        raise AssertionError("could not identify an owned window on the switched desktop")
                    run.key("win+ctrl+F4")
                wait_until(lambda: observe_original(True), what="the original virtual desktop is active again",
                           timeout_s=SETTLE_S)
            WIN.bring_to_front(original_editor)
            wait_until(observe_restored, what="original editor foreground restored", timeout_s=SETTLE_S)
        except (AssertionError, Refused) as error:
            raise ScenarioFailed("verify restoration of the original virtual desktop", str(error)) from error
    if not switch_confirmed:
        run.step("limited desktop proof: the runner did not establish a switch within the proof deadline")


# Windows only: they need Win32 foreground/edit-control access or global hotkeys.
OVERLAY_SCENARIOS = ("tray", "go-to-session", "overlay-pointer", "tray-answer", "overlay-shell")
TIER_W_ONLY = {*OVERLAY_SCENARIOS, "no-focus-steal", "safe-mode", "capture-exclusion"}

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
    "card-hold": scenario_card_hold,
    "press-guard": scenario_press_guard,
    "card-dismiss": scenario_card_dismiss,
    "foreign-publisher": scenario_foreign_publisher,
    "sessions": scenario_sessions,
    "held-press": scenario_held_press,
    "card-link": scenario_card_link,
    "card-copy": scenario_card_copy,
    "card-reply": scenario_card_reply,
    "card-reveal": scenario_card_reveal,
    "stack-trim": scenario_stack_trim,
    "portal-reading": scenario_portal_reading,
    "widget-hover": scenario_widget_hover,
    "drag-alignment": scenario_drag_alignment,        # 8
    "drag-release": scenario_drag_release,            # 9
    "writing": scenario_writing,
    "dismiss": scenario_dismiss,
    "lifecycle": scenario_lifecycle,                  # 13
    "resize-hotkey": scenario_resize_hotkey,          # 16
    "unicode-typing": scenario_unicode_typing,
    "go-to-session": scenario_go_to_session,          # tier W, overlay only
    "no-focus-steal": scenario_no_focus_steal,        # 10 (tier W)
    "tray": scenario_tray,                            # tier W, overlay only
    "tray-answer": scenario_tray_answer,              # tier W, overlay only, informational
    "overlay-pointer": scenario_overlay_pointer,      # 11 (tier W, overlay only)
    "overlay-shell": scenario_overlay_shell,          # tier W, overlay only, informational
    "safe-mode": scenario_safe_mode,                  # 12 (tier W)
    "capture-exclusion": scenario_capture_exclusion,  # tier W, informational (restarts the HUD twice)
    "restart": scenario_restart,                      # 14 (last: replaces the HUD)
}


# ── Runner ───────────────────────────────────────────────────────────────────


LINK_SCENARIOS = ("portal-reading", "card-link")  # each launches a real browser handoff


def git_sha() -> str:
    try:
        return subprocess.run(["git", "rev-parse", "--short=12", "HEAD"], cwd=REPO, capture_output=True,
                              text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"


EXPLICIT_SCENARIOS = {DOCK_TOOLTIP_SCENARIO: scenario_dock_countdown_tooltip}


def run_scenarios(run: Run, names: list[str]) -> list[dict]:
    results = []
    for name in names:
        run.step(f"begin {name}")
        started = time.monotonic()
        explicit = name in EXPLICIT_SCENARIOS
        scenario = EXPLICIT_SCENARIOS[name] if explicit else SCENARIOS[name]
        if not explicit:
            run.snapshot(f"{name}.before")
        result = {"scenario": name, "doc": (scenario.__doc__ or "").strip()}
        try:
            scenario(run)
            result["outcome"] = "PASS"
        except ScenarioFailed as failure:
            result.update(outcome="FAIL", step=failure.step, last=failure.last)
        except Refused as err:
            # Keep every earlier outcome: the timeline and summary must not
            # collapse to 0/0 because a later guard refused .
            result.update(outcome="FAIL", step=f"refused: {err}", last=None,
                          seconds=round(time.monotonic() - started, 2))
            results.append(result)
            err.results = results
            raise
        except subprocess.CalledProcessError as err:
            stderr = "" if explicit else err.stderr or ""
            if isinstance(stderr, bytes):
                stderr = stderr.decode("utf-8", errors="replace")
            program = err.cmd[0] if isinstance(err.cmd, (list, tuple)) else "command"
            result.update(outcome="FAIL", step=f"CalledProcessError: {program} exited {err.returncode}",
                          last={"exit_code": err.returncode, "stderr": stderr[-4096:]})
        except Exception as err:  # noqa: BLE001 - every scenario reports, the run continues
            step = type(err).__name__ if explicit else f"{type(err).__name__}: {err}"
            result.update(outcome="FAIL", step=step, last=None)
        result["seconds"] = round(time.monotonic() - started, 2)
        if not explicit and result["outcome"] == "FAIL":
            try:
                run.screen_grab(f"{name}.after")
            except Exception as err:  # noqa: BLE001 - retain the original scenario failure
                result["diagnostic_error"] = f"{type(err).__name__}: {err}"
        if not explicit:
            try:
                run.snapshot(f"{name}.after")
                stray = run.clear_publications()
                if stray and result["outcome"] == "PASS":
                    result.update(outcome="FAIL", step="input nobody took (duplicate or stray submit)",
                                  last={"items": [i.get("text") for i in stray]})
            except (AssertionError, RequestFailure) as err:
                result.setdefault("cleanup", str(err))
        result["artifacts"] = f"{name}.*" if explicit else str(run.out / name) + ".*"
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
    parser.add_argument("--mouse-receipt-diagnostics", action="store_true",
                        help="retain optional scoped existing mouse DEBUG logs; does not change gesture verdicts")
    parser.add_argument("--required", type=required_names, metavar="NAMES",
                        help="comma-separated scenarios whose failure fails the run; the rest become "
                             "informational (listed, exit 0). Without it every scenario is required.")
    parser.add_argument("--expected-dock-build-sha", metavar="SHA",
                        help="required full executable source commit for the opt-in Linux dock witness")
    parser.add_argument("scenarios", nargs="*", help="scenario names (default: standard scenarios)")
    args = parser.parse_args(argv)
    if args.list:
        available = SCENARIOS | EXPLICIT_SCENARIOS if sys.platform == "linux" else SCENARIOS
        for name, fn in available.items():
            print(f"{name:20} {(fn.__doc__ or '').strip().splitlines()[0]}")
        return 0
    named = args.scenarios + (args.required or [])
    dock_witness = DOCK_TOOLTIP_SCENARIO in named
    if dock_witness and (
        sys.platform != "linux" or args.injector != "xtest" or args.window_mode != "fullscreen"
        or args.scenarios != [DOCK_TOOLTIP_SCENARIO] or args.mouse_receipt_diagnostics
        or args.required not in (None, [DOCK_TOOLTIP_SCENARIO])
        or not re.fullmatch(r"[0-9a-f]{40}", args.expected_dock_build_sha or "")
    ):
        parser.error("dock-countdown-tooltip must run alone and required on Linux XTEST fullscreen "
                     "with --expected-dock-build-sha")
    if args.expected_dock_build_sha and not dock_witness:
        parser.error("--expected-dock-build-sha is only for dock-countdown-tooltip")
    unknown = [n for n in named if n not in SCENARIOS and n not in EXPLICIT_SCENARIOS]
    if unknown:
        parser.error(f"unknown scenario(s): {', '.join(unknown)}; see --list")
    if (args.injector == "sendinput") != WINDOWS:
        parser.error(f"--injector {args.injector} does not run on {sys.platform}")
    tier_w = args.injector == "sendinput"
    if not tier_w and TIER_W_ONLY.intersection(args.scenarios):
        parser.error(f"{', '.join(sorted(TIER_W_ONLY))} run only with --injector sendinput")
    if args.window_mode == "overlay" and not tier_w:
        parser.error("overlay gestures require --injector sendinput")
    overlay_names = OVERLAY_SCENARIOS
    if args.scenarios and any((n in overlay_names) != (args.window_mode == "overlay") for n in args.scenarios):
        parser.error(f"{', '.join(sorted(overlay_names))} require --window-mode overlay; "
                     "other scenarios require fullscreen")
    names = args.scenarios or [n for n in SCENARIOS
                               if (tier_w or n not in TIER_W_ONLY)
                               and ((n in overlay_names) == (args.window_mode == "overlay"))]
    # Real link handoffs ("portal-reading", "card-link") can let a browser take
    # the foreground seconds later, so they run after everything else that
    # injects input. "restart" replaces the HUD and only uses MCP; it stays last.
    # "overlay-shell" may switch the runner's desktop, so it runs last in overlay mode.
    # Before those, required scenarios run first in the fresh HUD and
    # informational ones follow in the same boot. A list naming the other window
    # mode's scenarios only gates the ones this invocation runs.
    trailing = [n for n in LINK_SCENARIOS + ("overlay-shell", "restart") if n in names]
    required = set(names) if args.required is None else set(args.required).intersection(names)
    names = ([n for n in names if n in required and n not in trailing]
             + [n for n in names if n not in required and n not in trailing] + trailing)
    sha = git_sha()
    # The pid keeps parallel runs started in the same second apart.
    out = args.out or REPO / "test_results/gestures" / sha / f"{time.strftime('%Y%m%dT%H%M%S')}-{os.getpid()}"
    out.mkdir(parents=True, exist_ok=not dock_witness)
    run = Run(exe=args.exe.resolve(), out=out, injector=args.injector, window_mode=args.window_mode,
              go_to_probe="go-to-session" in names, dock_build_sha=args.expected_dock_build_sha)
    if args.mouse_receipt_diagnostics:
        run.mouse_receipts = MouseReceiptCapture(out)
    started = time.monotonic()
    results: list[dict] = []
    failure = None
    partial_button_batch = False
    try:
        run.start()
        status = run.status()
        if args.mouse_receipt_diagnostics:
            run.mouse_receipts.observe(status, run.pid)
        # Gesture scenarios read desktop pixels: a HUD excluded from capture
        # would make every pixel check vacuous.
        if (status.get("capture_exclusion") or {}).get("on"):
            raise Refused("Hide from screen capture is on; gesture scenarios read desktop pixels")
        run.step(f"owned HUD pid {run.pid} on display {run.display or 'primary'} via {args.injector}")
        results = run_scenarios(run, names)
        unacked = 0 if dock_witness else run.smoke.ok("hud_input", {"max_items": 0}).get("remaining", 0)
        if unacked:
            failure = f"{unacked} un-acked hud_input item(s) at exit"
    except Refused as err:
        failure = "private dock guard refused" if dock_witness else f"refused: {err}"
        partial_button_batch = isinstance(err, PartialButtonBatch)
        results = getattr(err, "results", results)
    except Exception as err:  # noqa: BLE001
        failure = (f"harness: {type(err).__name__}" if dock_witness
                   else f"harness: {type(err).__name__}: {err}")
    finally:
        if args.mouse_receipt_diagnostics:
            run.mouse_receipts.end_interval()
        survivors = run.stop() if run.home else []
        if survivors:
            failure = (failure + "; " if failure else "") + "left running: " + ", ".join(survivors)
        timeline = {
            "sha": sha, "profile": run.exe.parent.name,
            "exe": run.exe.name if dock_witness else str(run.exe),
            "injector": args.injector, "window_mode": args.window_mode, "screen": SCREEN,
            "display": run.display, "mcp_port": run.port,
            "geometry": (status.get("displays") if "status" in locals() else None),
            "theme_scale": ({k: status.get(k) for k in ("theme", "ui_scale") if k in status} if "status" in locals() else None),
            "seconds": round(time.monotonic() - started, 2), "results": results, "failure": failure,
            "required": sorted(required),
            "steps": run.timeline,
            "sendinput_calls": getattr(run.inj, "calls", None) if args.injector == "sendinput" else None,
        }
        if args.mouse_receipt_diagnostics:
            timeline["mouse_receipts"] = run.mouse_receipts.finish(run.owned_pids, survivors)
        if dock_witness:
            cleanup = {"hud_reaped": run.hud is None or run.hud.poll() is not None,
                       "xvfb_reaped": run.xvfb is None or run.xvfb.poll() is not None,
                       "scratch_removed": all(p is None or not p.exists() for p in (run.home, run.config_dir))}
            timeline["dock_cleanup"] = cleanup
            if not all(cleanup.values()) or survivors:
                failure = failure or "private dock process or scratch cleanup failed"
                timeline["failure"] = failure
                for result in results:
                    if result["outcome"] == "PASS":
                        result.update(outcome="FAIL", step=failure, last=None)
        (out / "timeline.json").write_text(json.dumps(timeline, indent=1))
        if not dock_witness and run.log and run.log.exists():
            tail = run.log.read_text(errors="replace").splitlines()[-200:]
            (out / "hud.log.tail").write_text("\n".join(tail))
    for result in results:
        result["required"] = result["scenario"] in required
    passed = sum(r["outcome"] == "PASS" for r in results)
    print(f"{passed}/{len(results)} scenarios passed in {time.monotonic() - started:.1f}s; artifacts: {out}")
    if failure:
        print(f"FAIL harness: {failure}")
    verdict = gate_verdict(results, required, failure)
    verdict["partial_button_batch"] = partial_button_batch
    (out / "verdict.json").write_text(json.dumps(verdict, indent=1))
    write_step_summary(args.injector, results, failure, passed, time.monotonic() - started, verdict)
    return 1 if verdict["blocking"] else 0


def required_names(value: str) -> list[str]:
    return [name for name in (part.strip() for part in value.split(",")) if name]


def gate_verdict(results: list[dict], required: set[str], failure: str | None) -> dict:
    """Which failures block: a failed required scenario, or a harness failure
    (refusal, crash, leftover process) that left a required scenario without a
    PASS. A harness failure after every required scenario passed, such as an
    informational link scenario losing the foreground, does not block.
    Informational failures never do."""
    outcomes = {r["scenario"]: r["outcome"] for r in results}
    required_failed = sorted(n for n in required if outcomes.get(n) == "FAIL")
    required_unproven = sorted(n for n in required if outcomes.get(n) not in ("PASS", "FAIL"))
    informational_failed = [r["scenario"] for r in results
                            if r["scenario"] not in required and r["outcome"] != "PASS"]
    blocking = bool(required_failed) or (failure is not None and bool(required_unproven))
    return {"required": sorted(required), "required_failed": required_failed,
            "required_unproven": required_unproven, "informational_failed": informational_failed,
            "harness_failure": failure, "blocking": blocking}


def write_step_summary(injector: str, results: list[dict], failure: str | None, passed: int, seconds: float,
                       verdict: dict | None = None) -> None:
    """A GitHub step summary table, so a dispatching agent reads outcomes without the artifact."""
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not path:
        return
    rows = [f"## Gesture scenarios ({injector}): {passed}/{len(results)} passed in {seconds:.0f}s", "",
            "| scenario | gate | outcome | s | failing step |", "|---|---|---|---|---|"]
    rows += [f"| {r['scenario']} | {'required' if r.get('required', True) else 'info'} | {r['outcome']} "
             f"| {r['seconds']} | {r.get('step', '')} |" for r in results]
    if failure:
        rows += ["", f"**harness:** {failure}"]
    if verdict and verdict["required_failed"]:
        rows += ["", f"**required failed:** {', '.join(verdict['required_failed'])}"]
    if verdict and verdict["informational_failed"]:
        rows += ["", f"Informational failures (do not fail the job): {', '.join(verdict['informational_failed'])}"]
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n".join(rows) + "\n")


if __name__ == "__main__":
    sys.exit(main())
