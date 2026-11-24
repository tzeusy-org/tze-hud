"""Detached witness for late normal-z-order card-link loss on a hosted VM."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time
import zipfile
from contextlib import contextmanager, suppress
from ctypes import wintypes
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
# No candidate or artifact defaults: fill these from the actual first hosted
# artifact after this probe is grafted onto the reviewed candidate.
PIN_HASH_LENGTHS = {"source": 40, "tree": 40, "build_source": 40,
                    "archive_sha256": 64, "exe_sha256": 64}
PIN_IDS = {"run_id", "artifact_id"}
LATE_MARKER = "detached second press post-readiness cover verified"
CARD_KEY = "gesture-card-link"
TARGET = f"zone:notification-area/{CARD_KEY}/0"
COVER_RADIUS = 12
COVER_SETTLE_S = 0.8
COVER_SAMPLE_S = 0.025
WS_EX_TOOLWINDOW = 0x80
WS_EX_LAYERED = 0x80000
WS_POPUP = 0x80000000
WS_CHILD = 0x40000000
WS_VISIBLE = 0x10000000
GWL_STYLE = -16
GW_HWNDPREV = 3
GW_OWNER = 4
PM_REMOVE = 1
# HWND_TOPMOST and HWND_NOTOPMOST, in the only order the child requests.
COVER_BANDS = (("promoted", -1, False, True), ("demoted", -2, True, False))


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def hosted_only() -> None:
    require(sys.platform == "win32" and os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted",
            "native witness requires a disposable GitHub-hosted Windows runner")


def load_pins(path: Path) -> dict:
    pins = json.loads(path.read_text(encoding="utf-8-sig"))
    require(isinstance(pins, dict) and set(pins) == set(PIN_HASH_LENGTHS) | PIN_IDS,
            "pins must contain exact source/tree/build_source, run/artifact IDs and archive/exe SHA256")
    for key, length in PIN_HASH_LENGTHS.items():
        value = pins[key]
        require(isinstance(value, str) and re.fullmatch(rf"[0-9a-f]{{{length}}}", value) is not None
                and value != "0" * length, f"missing or invalid exact pin: {key}")
    for key in PIN_IDS:
        require(type(pins[key]) is int and pins[key] > 0, f"missing or invalid exact pin: {key}")
    return pins


def source_identity(pins: dict) -> dict:
    source = pins["source"]

    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", *args], cwd=REPO, timeout=15)

    checkout, tree = git("rev-parse", "HEAD", f"{source}^{{tree}}").decode().split()
    require(not git("status", "--porcelain", "--untracked-files=no").strip(),
            "witness checkout has modified tracked files")
    git("merge-base", "--is-ancestor", source, "HEAD")
    require(tree == pins["tree"], "witness is not based on the exact PR tree")
    changed = set(git("diff", "--name-only", source, "HEAD").decode().splitlines())
    require(changed == {".github/workflows/windows.yml", "scripts/ci/card_link_raise_probe.py"},
            "witness branch contains changes beyond the detached probe")
    source_files = ("scripts/ci/hud_gestures.py", "scripts/ci/hud_sendinput.py",
                    "scripts/ci/windows_smoke.py", "app/tze_hud_app/config/production.toml")
    hashes = {}
    for filename in source_files:
        canonical = git("show", f"{source}:{filename}")
        working = (REPO / filename).read_bytes().replace(b"\r\n", b"\n")
        require(working == canonical, f"tested source has changed: {filename}")
        hashes[filename] = hashlib.sha256(canonical).hexdigest()
    return {"probe_checkout": checkout, "candidate_source": source, "candidate_tree": tree,
            "source_sha256": hashes}


def original_executable(archive: Path, metadata_file: Path, directory: Path, report: dict, pins: dict) -> Path:
    metadata = json.loads(metadata_file.read_text(encoding="utf-8"))
    run = metadata["workflow_run"]
    require(metadata["id"] == pins["artifact_id"] and metadata["name"] == "tze_hud-windows-msvc"
            and metadata["digest"] == f"sha256:{pins['archive_sha256']}" and not metadata["expired"]
            and run["id"] == pins["run_id"] and run["head_sha"] == pins["source"],
            "artifact metadata does not match the first exact PR build")
    with archive.open("rb") as handle:
        archive_sha = hashlib.file_digest(handle, "sha256").hexdigest()
    require(archive_sha == pins['archive_sha256'], "archive bytes do not match the pinned GitHub digest")
    with zipfile.ZipFile(archive) as contents:
        sidecar = contents.read("tze_hud.exe.sha256").decode("ascii").split()
        require(len(sidecar) == 2 and sidecar[0] == pins["exe_sha256"] and sidecar[1] in ("tze_hud.exe", "*tze_hud.exe"),
                "staged hash does not match the independently witnessed PR binary")
        exe = Path(contents.extract("tze_hud.exe", directory / "release-dev"))
    with exe.open("rb") as handle:
        exe_sha = hashlib.file_digest(handle, "sha256").hexdigest()
    require(exe_sha == pins["exe_sha256"], "executable does not match the independently witnessed PR binary")
    report["artifact"] = {"run": pins["run_id"], "id": pins["artifact_id"], "archive_sha256": archive_sha, "exe_sha256": exe_sha}
    return exe


def child_window() -> None:
    """Wait without a window until the rendered target is known."""
    import hud_sendinput as win

    user32 = win.user32
    for symbol, result, args in (
        ("CreateWindowExW", wintypes.HWND, [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR,
                                          wintypes.DWORD, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                          wintypes.HWND, wintypes.HMENU, wintypes.HINSTANCE, wintypes.LPVOID]),
        ("DestroyWindow", wintypes.BOOL, [wintypes.HWND]),
        ("PeekMessageW", wintypes.BOOL, [ctypes.POINTER(wintypes.MSG), wintypes.HWND,
                                      wintypes.UINT, wintypes.UINT, wintypes.UINT]),
        ("DispatchMessageW", ctypes.c_ssize_t, [ctypes.POINTER(wintypes.MSG)]),
    ):
        getattr(user32, symbol).restype, getattr(user32, symbol).argtypes = result, args
    win.kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    win.kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    print(json.dumps({"phase": "ready", "pid": os.getpid()}), flush=True)
    command = sys.stdin.readline()
    if not command:
        return
    x, y = json.loads(command)
    width, height = win.screen_size()
    radius = COVER_RADIUS
    require(radius <= x < width - radius and radius <= y < height - radius,
            "fixture point is too close to a display edge")
    # The predefined button class paints opaque content and takes part in hit testing.
    # The child creates one window at its final position.
    foreground_before = win.foreground_identity()
    hwnd = user32.CreateWindowExW(win.WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, "Button", "",
                                 WS_POPUP | WS_VISIBLE, x - radius, y - radius, radius * 2, radius * 2, None, None,
                                 win.kernel32.GetModuleHandleW(None), None)
    require(bool(hwnd), "could not create the owned covering window")
    try:
        print(json.dumps({"phase": "created", "hwnd": hwnd, "pid": os.getpid(),
                          "foreground_before": foreground_before, "foreground_after": win.foreground_identity()}),
              flush=True)
        # Selecting HWND_NOTOPMOST moves a topmost window above the ordinary band.
        # The parent waits for both receipts and observes the cleared bit before input.
        for phase, band, _, _ in COVER_BANDS:
            print(json.dumps(place_cover(win, hwnd, (x, y), phase, band)), flush=True)
        closed = threading.Event()

        def pipe_closed() -> None:
            sys.stdin.read()
            closed.set()

        threading.Thread(target=pipe_closed, daemon=True).start()
        msg = wintypes.MSG()
        deadline = time.monotonic() + 90
        while not closed.wait(0.01):
            require(time.monotonic() < deadline, "cover watchdog expired")
            while user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_REMOVE):
                user32.DispatchMessageW(ctypes.byref(msg))
    finally:
        require(bool(user32.DestroyWindow(hwnd)), "owned covering window could not be destroyed")


def place_cover(win, hwnd: int, point: tuple[int, int], phase: str, band: int) -> dict:
    user32 = win.user32
    before = win.foreground_identity()
    style_before = user32.GetWindowLongPtrW(hwnd, win.GWL_EXSTYLE)
    flags = win.SWP_NOACTIVATE | win.SWP_NOMOVE | win.SWP_NOSIZE | win.SWP_NOOWNERZORDER
    ctypes.set_last_error(0)
    accepted = bool(user32.SetWindowPos(hwnd, band, 0, 0, 0, 0, flags))
    error = ctypes.get_last_error()
    return {
        "phase": phase, "hwnd": hwnd, "pid": os.getpid(), "insert_after": band, "flags": flags,
        "accepted": accepted, "last_error": error, "foreground_before": before,
        "foreground_after": win.foreground_identity(), "at_point": win.window_at(point),
        "topmost_before": bool(style_before & win.WS_EX_TOPMOST),
        "topmost_after": bool(user32.GetWindowLongPtrW(hwnd, win.GWL_EXSTYLE) & win.WS_EX_TOPMOST),
    }


def verified_bands(transitions: list[dict], helper: dict, hud: dict) -> bool:
    return len(transitions) == len(COVER_BANDS) and all(
        transition["phase"] == phase and transition["insert_after"] == band
        and transition["hwnd"] == helper["hwnd"] and transition["pid"] == helper["pid"]
        and transition["accepted"] is True and transition["topmost_before"] is before
        and transition["topmost_after"] is after
        and transition["foreground_before"] == transition["foreground_after"] == hud
        for transition, (phase, band, before, after) in zip(transitions, COVER_BANDS)
    )


def live_window(win, identity: dict) -> dict:
    hwnd = identity["hwnd"]
    require(win._window_identity(hwnd) == identity, "owned window identity changed")
    rect = wintypes.RECT()
    require(bool(win.user32.GetWindowRect(hwnd, ctypes.byref(rect))), "owned window bounds are unavailable")
    return {"identity": identity, "rect": [rect.left, rect.top, rect.right, rect.bottom],
            "visible": bool(win.user32.IsWindowVisible(hwnd)),
            "owner": win.user32.GetWindow(hwnd, GW_OWNER),
            "window_style": win.user32.GetWindowLongPtrW(hwnd, GWL_STYLE),
            "style": win.user32.GetWindowLongPtrW(hwnd, win.GWL_EXSTYLE)}


def above(win, higher: int, lower: int) -> bool:
    """Check only the relative order of the fixture and the tested HUD."""
    for _ in range(64):
        lower = win.user32.GetWindow(lower, GW_HWNDPREV)
        if not lower:
            return False
        if lower == higher:
            return True
    return False


def observe_cover(win, hud: dict, helper: dict, point: tuple[int, int]) -> dict:
    return {"foreground": win.foreground_identity(), "at_point": win.window_at(point),
            "cover_above_hud": above(win, helper["hwnd"], hud["hwnd"]),
            "hud_above_cover": above(win, hud["hwnd"], helper["hwnd"])}


def settle_cover(win, hud: dict, helper: dict, point: tuple[int, int], samples: list[dict],
                 *, deadline: float) -> bool:
    start = time.monotonic()
    stop = min(start + COVER_SETTLE_S, deadline)
    stable = 0
    expected = {"foreground": hud, "at_point": helper, "cover_above_hud": True, "hud_above_cover": False}
    while time.monotonic() < stop:
        state = observe_cover(win, hud, helper, point)
        elapsed = time.monotonic() - start
        samples.append({"elapsed_ms": round(elapsed * 1000), **state})
        stable = stable + 1 if state == expected else 0
        if stable == 2:
            return time.monotonic() < stop
        time.sleep(max(0, min(COVER_SAMPLE_S, stop - time.monotonic())))
    return False


def desktop_patch(win, gestures, artifact: Path, name: str, point: tuple[int, int]) -> dict:
    width, height, rgba = win.desktop_rgba()
    x, y = point
    r = COVER_RADIUS
    require(r <= x < width - r and r <= y < height - r, "mapped patch lies outside the desktop frame")
    crop = b"".join(rgba[(row * width + x - r) * 4 : (row * width + x + r) * 4] for row in range(y - r, y + r))
    (artifact / f"{name}.desktop.png").write_bytes(gestures.encode_png_rgba(r * 2, r * 2, crop))
    center = (r * (r * 2) + r) * 4
    return {"sha256": hashlib.sha256(crop).hexdigest(), "point_rgba": list(crop[center:center + 4])}


def verify_timeline(timeline: dict, fixture: dict, before: dict, after: dict, pins: dict) -> dict:
    hud, helper, point = fixture["hud"]["identity"], fixture["cover"]["identity"], fixture["point"]
    results = timeline["results"]
    require(len(results) == 1 and results[0]["scenario"] == "card-link" and results[0]["outcome"] == "PASS"
            and not timeline["failure"], "original card-link scenario did not pass")
    require(before["sha"] == after["sha"] == pins["build_source"] and before["pid"] == after["pid"] == hud["pid"],
            "binary or HUD identity changed during the scenario")
    require(after["link_launches"] == before["link_launches"] + 1,
            "expected one browser handoff across both native presses")
    steps = timeline["steps"]
    markers = [(index, step) for index, step in enumerate(steps) if step["step"] == LATE_MARKER]
    losses = [(index, step) for index, step in enumerate(steps)
              if step["step"] == "card link second press pre-submission ownership lost"]
    raises = [(index, step) for index, step in enumerate(steps)
              if step["step"] == "card link second press requested raise"]
    require(len(markers) == len(losses) == len(raises) == 1,
            "expected exactly one controlled late cover, original guard refusal and native second-press raise")
    marker_index, marker = markers[0]
    loss_index, loss = losses[0]
    raise_index, raised = raises[0]
    require(marker_index < loss_index < raise_index and loss["guard_phase"] == "button mapped point"
            and loss["observed"]["card_link_point"] == {"foreground": hud, "at_point": helper},
            "original pre-submission mapped-point guard did not observe the exact late cover")
    require(marker["owned_window"] == hud and marker["intended_point"] == point
            and marker["deadline"] == fixture["deadline"], "late marker lost its owned press identity/deadline")
    require(raised["owned_window"] == hud and raised["intended_point"] == point
            and raised["card_link_point"] == {"foreground": hud, "at_point": helper},
            "source-owned second-press raise did not observe the exact normal cover")
    require(not any(step["step"] == "card link second press requested activate"
                    for step in steps[marker_index:]), "late recovery changed the HUD foreground")
    checks = [(index, step) for index, step in enumerate(steps) if "click_target_before_send" in step]
    require(len(checks) == 2 and checks[0][0] < marker_index < raise_index < checks[1][0],
            "late loss/recovery must be between the original first and second successful final checks")
    require(any(step["step"] == "ok then says it opened" for step in steps[checks[0][0] + 1:marker_index]),
            "late cover was introduced before the first real browser handoff completed")
    require(checks[1][1]["t"] < fixture["deadline"], "second check exceeded the original native deadline")
    for _, step in checks:
        observed = step["click_target_before_send"]
        cursor = observed["cursor"]
        require(step["repeat"] == 1 and step["intended_point"] == point and observed["foreground"] == hud
                and observed["at_cursor"] == hud and len(cursor or []) == 2
                and all(abs(actual - wanted) <= 1 for actual, wanted in zip(cursor, point)),
                "last-mile native check did not see the exact owned HUD at the intended point")
    calls = timeline["sendinput_calls"]
    buttons = [call for call in calls if call["what"] == "click button 1 x1"]
    require(len(buttons) == 2 and all(call["requested"] == call["sent"] == 2 for call in buttons)
            and all(call in buttons or (call["what"].startswith("move ") and call["requested"] == call["sent"] == 1)
                    for call in calls),
            "expected only owned motion and two complete native button pairs")
    elapsed = round(checks[1][1]["t"] - checks[0][1]["t"], 3)
    require(0 < elapsed < 2, "native repeat fell outside the original rate-limit interval")
    return {"marker": marker, "guard_loss": loss, "raise": raised, "checks": [step for _, step in checks], "button_batches": buttons,
            "seconds_between_checks": elapsed, "link_launches": [before["link_launches"], after["link_launches"]]}


@contextmanager
def late_press_hooks(gestures, install_cover):
    """Add one native obstruction; preserve the original press and input guards."""
    original_press = gestures.press_hosted_card_link
    original_click = gestures.Run.click_link
    active_press = {}
    armed = False

    def scoped_press(run, *args, **kwargs):
        require(not active_press, "nested original card press")
        active_press.update(run=run, press=kwargs["press"], deadline=kwargs["deadline"])
        try:
            return original_press(run, *args, **kwargs)
        finally:
            active_press.clear()

    def cover_before_second_click(run, point, *args, **kwargs):
        nonlocal armed
        buttons = [call for call in run.inj.calls if call["what"].startswith("click button")]
        require(len(buttons) < 2 and all(call["what"] == "click button 1 x1"
                and call["requested"] == call["sent"] == 2 for call in buttons)
                and kwargs.get("repeat", 1) == 1, "refusing extra or partial native button input")
        if active_press.get("press") == "second press" and not armed:
            require(active_press["run"] is run and kwargs.get("deadline") == active_press["deadline"],
                    "late trigger lost the exact original press deadline")
            armed = True
            install_cover(run, point, active_press["deadline"])
        return original_click(run, point, *args, **kwargs)

    with mock.patch.object(gestures, "press_hosted_card_link", scoped_press), \
            mock.patch.object(gestures.Run, "click_link", cover_before_second_click):
        yield


def witness(exe: Path, out: Path, report: dict, pins: dict) -> None:
    import hud_gestures as gestures
    import hud_sendinput as win

    win.user32.GetWindow.argtypes = [wintypes.HWND, wintypes.UINT]
    win.user32.GetWindow.restype = wintypes.HWND
    artifact = out / "gesture"
    lines = queue.Queue()
    with (out / "cover.stderr.log").open("w", encoding="utf-8") as stderr:
        child = subprocess.Popen([sys.executable, "-u", __file__, "--cover-child"], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=stderr, text=True,
                                 creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            def read_pipe() -> None:
                for line in child.stdout:
                    lines.put(line)
                lines.put("")

            threading.Thread(target=read_pipe, daemon=True).start()

            def receive(phase: str, *, deadline: float | None = None) -> dict:
                remaining = 10 if deadline is None else deadline - time.monotonic()
                require(remaining > 0, f"original press deadline expired before cover {phase}")
                try:
                    line = lines.get(timeout=remaining)
                except queue.Empty:
                    raise RuntimeError(f"cover timed out before {phase}") from None
                require(bool(line), f"cover exited before {phase}")
                receipt = json.loads(line)
                require(receipt.get("phase") == phase and receipt.get("pid") == child.pid,
                        "cover handshake does not match the freshly started native helper")
                return receipt

            report["cover_ready"] = receive("ready")
            open_card = gestures.open_card

            def observe_publication(run, key, **kwargs):
                require(key == CARD_KEY and kwargs.get("park_pointer") is False and not run.inj.calls,
                        "witness must start before any original input")
                status = run.status()
                require(status["pid"] == run.pid and status["sha"] == pins["build_source"] and status["channel"] == "ci",
                        "running binary does not match the pinned build stamp")
                report["build"] = {k: status[k] for k in ("pid", "version", "sha", "channel")}
                return open_card(run, key, **kwargs)

            def install_late_cover(run, point, deadline):
                buttons = [call for call in run.inj.calls if call["what"].startswith("click button")]
                require(len(buttons) == 1 and buttons[0]["what"] == "click button 1 x1"
                        and buttons[0]["requested"] == buttons[0]["sent"] == 2,
                        "late trigger requires exactly one completed original native button pair")
                require(time.monotonic() < deadline and gestures.cursor_at_link(win.click_target_snapshot()["cursor"], point),
                        "late trigger requires the original cursor and unexpired deadline")
                require(gestures.resolve_target(run.status(), TARGET) == point,
                        "mapped card point changed before the late trigger")
                hud = {"hwnd": win.fullscreen_window(run.pid), "pid": run.pid}
                baseline = {"foreground": win.foreground_identity(), "at_point": win.window_at(point)}
                report["fixture"] = {"point": list(point), "baseline": baseline, "samples": [], "deadline": deadline}
                require(hud["hwnd"] is not None and hud["pid"] != child.pid
                        and baseline == {"foreground": hud, "at_point": hud},
                        "baseline point and foreground must belong to the exact fullscreen HUD")
                hud_window = live_window(win, hud)
                fixture = report["fixture"]
                fixture["hud"] = hud_window
                require(hud_window["visible"] and not hud_window["style"] & win.WS_EX_TOPMOST,
                        "HUD must be visible in normal z-order")
                fixture["uncovered_pixels"] = desktop_patch(win, gestures, artifact, "fixture-before", point)
                child.stdin.write(json.dumps(point) + "\n")
                child.stdin.flush()
                created = receive("created", deadline=deadline)
                report["cover_created"] = created
                require(isinstance(created.get("hwnd"), int) and created["hwnd"] > 0,
                        "cover has no native window handle")
                helper = {"hwnd": created["hwnd"], "pid": child.pid}
                transitions = fixture["band_transitions"] = []
                for phase, _, _, _ in COVER_BANDS:
                    transitions.append(receive(phase, deadline=deadline))
                cover = live_window(win, helper)
                fixture["cover"] = cover
                stable = settle_cover(win, hud, helper, point, fixture["samples"], deadline=deadline)
                fixture["candidate_pixels"] = desktop_patch(win, gestures, artifact, "fixture-after-placement", point)
                x, y = point
                r = COVER_RADIUS
                require(cover["visible"] and cover["rect"] == [x - r, y - r, x + r, y + r]
                        and cover["owner"] is None and cover["window_style"] & WS_POPUP and not cover["window_style"] & WS_CHILD
                        and cover["style"] & win.WS_EX_NOACTIVATE
                        and not cover["style"] & (win.WS_EX_TOPMOST | win.WS_EX_TRANSPARENT | WS_EX_LAYERED),
                        "cover must be an independent opaque normal window at the mapped point")
                final = observe_cover(win, hud, helper, point)
                fixture["before_original_press"] = final
                require(live_window(win, helper) == cover, "cover changed while gathering fixture evidence")
                require(verified_bands(transitions, helper, hud), "cover did not complete both nonactivating band transitions")
                require(created["foreground_before"] == created["foreground_after"] == hud
                        and all(sample["foreground"] == hud for sample in fixture["samples"])
                        and stable and final == {"foreground": hud, "at_point": helper,
                                             "cover_above_hud": True, "hud_above_cover": False},
                        "new cover did not persist above the mapped point while HUD stayed foreground")
                require(fixture["candidate_pixels"] != fixture["uncovered_pixels"],
                        "independent desktop capture did not show the candidate cover")
                require(time.monotonic() < deadline, "late cover setup exhausted the original press deadline")
                run.timeline.append({"t": round(time.monotonic(), 3), "step": LATE_MARKER,
                                     "owned_window": hud, "intended_point": list(point), "deadline": deadline})

            original = gestures.SCENARIOS["card-link"]

            def observe_after_both_presses(run):
                original(run)
                fixture = report["fixture"]
                helper = fixture["cover"]["identity"]
                report["cover_after_scenario"] = live_window(win, helper)
                require(report["cover_after_scenario"] == fixture["cover"],
                        "cover was removed or changed rather than uncovered by the original harness")
                hud = live_window(win, fixture["hud"]["identity"])
                require(hud["visible"] and not hud["style"] & win.WS_EX_TOPMOST
                        and above(win, hud["identity"]["hwnd"], helper["hwnd"]),
                        "normal z-order did not put the live HUD above the unchanged cover")
                report["hud_above_cover_after_both_presses"] = True
                fixture["after_presses_pixels"] = desktop_patch(
                    win, gestures, artifact, "fixture-after-presses", tuple(fixture["point"]))

            with mock.patch.object(gestures, "open_card", observe_publication), \
                    late_press_hooks(gestures, install_late_cover), \
                    mock.patch.dict(gestures.SCENARIOS, {"card-link": observe_after_both_presses}):
                code = gestures.main(["--exe", str(exe), "--out", str(artifact), "--injector", "sendinput", "card-link"])
            require(code == 0, "original gesture harness reported a failure")
            timeline, before, after = (
                json.loads((artifact / filename).read_text(encoding="utf-8"))
                for filename in ("timeline.json", "card-link.before.status.json", "card-link.after.status.json")
            )
            report["native"] = verify_timeline(timeline, report["fixture"], before, after, pins)
        finally:
            with suppress(BrokenPipeError):
                child.stdin.close()
            try:
                rc = child.wait(timeout=5)
                forced = False
            except subprocess.TimeoutExpired:
                child.kill()
                rc = child.wait(timeout=5)
                forced = True
            report["cover_teardown"] = {"pid": child.pid, "returncode": rc, "forced": forced}
        require(rc == 0 and not forced, "owned covering window did not exit cleanly")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pins", type=Path, help="Required JSON custody pins from the actual first candidate artifact")
    parser.add_argument("--check-pins", action="store_true", help="Validate custody pins without native operations")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--artifact-metadata", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--cover-child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.check_pins:
        require(args.pins is not None, "exact candidate pins are required")
        print(json.dumps(load_pins(args.pins)))
        return 0
    hosted_only()
    if args.cover_child:
        child_window()
        return 0
    if args.pins is None or args.archive is None or args.artifact_metadata is None or args.out is None:
        parser.error("--pins, --archive, --artifact-metadata and --out are required")
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    report = {"outcome": "FAIL"}
    try:
        pins = load_pins(args.pins)
        report["pins"] = pins
        report["identity"] = source_identity(pins)
        with tempfile.TemporaryDirectory(prefix="card-link-raise-build-") as directory:
            exe = original_executable(args.archive, args.artifact_metadata, Path(directory), report, pins)
            witness(exe, out, report, pins)
        report["outcome"] = "PASS"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        print(report["error"], file=sys.stderr)
    finally:
        (out / "witness.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return int(report["outcome"] != "PASS")


if __name__ == "__main__":
    raise SystemExit(main())
