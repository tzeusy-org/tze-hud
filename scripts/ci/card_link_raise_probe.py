"""Detached witness for a real normal-z-order card-link rearm on a hosted VM."""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import zipfile
from contextlib import suppress
from ctypes import wintypes
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parents[2]
PR_SOURCE = "8ddf087711ba1c22761a367eba9c424127d22507"
PR_TREE = "794ba367a332d470e97dd55fd07d5159f52d1b5a"
BUILD_SOURCE = "b50e8e84e276210464c10a02aafbad9acdba31b7"
EXE_SHA256 = "69fd4c77226bad389a68a57d95199d0c7aed9818c53b3197d22bf1795f51154a"
RUN_ID = 45281402264
ARTIFACT_ID = 14871486974
ARTIFACT_SHA256 = "1e7a299903ab316a453391619910d43b9a13da3048dbbbc61c55d53c668444bf"
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


def source_identity() -> dict:
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", *args], cwd=REPO, timeout=15)

    checkout, tree = git("rev-parse", "HEAD", f"{PR_SOURCE}^{{tree}}").decode().split()
    git("merge-base", "--is-ancestor", PR_SOURCE, "HEAD")
    require(tree == PR_TREE, "witness is not based on the exact PR tree")
    changed = set(git("diff", "--name-only", PR_SOURCE, "HEAD").decode().splitlines())
    require(changed == {".github/workflows/windows.yml", "scripts/ci/card_link_raise_probe.py"},
            "witness branch contains changes beyond the detached probe")
    source_files = ("scripts/ci/hud_gestures.py", "scripts/ci/hud_sendinput.py",
                    "scripts/ci/windows_smoke.py", "app/tze_hud_app/config/production.toml")
    hashes = {}
    for filename in source_files:
        canonical = git("show", f"{PR_SOURCE}:{filename}")
        working = (REPO / filename).read_bytes().replace(b"\r\n", b"\n")
        require(working == canonical, f"tested source has changed: {filename}")
        hashes[filename] = hashlib.sha256(canonical).hexdigest()
    return {"probe_checkout": checkout, "pr_source": PR_SOURCE, "pr_tree": tree,
            "source_sha256": hashes}


def original_executable(archive: Path, metadata_file: Path, directory: Path, report: dict) -> Path:
    metadata = json.loads(metadata_file.read_text(encoding="utf-8"))
    run = metadata["workflow_run"]
    require(metadata["id"] == ARTIFACT_ID and metadata["name"] == "tze_hud-windows-msvc"
            and metadata["digest"] == f"sha256:{ARTIFACT_SHA256}" and not metadata["expired"]
            and run["id"] == RUN_ID and run["head_sha"] == PR_SOURCE,
            "artifact metadata does not match the first exact PR build")
    with archive.open("rb") as handle:
        archive_sha = hashlib.file_digest(handle, "sha256").hexdigest()
    require(archive_sha == ARTIFACT_SHA256, "archive bytes do not match the pinned GitHub digest")
    with zipfile.ZipFile(archive) as contents:
        sidecar = contents.read("tze_hud.exe.sha256").decode("ascii").split()
        require(len(sidecar) == 2 and sidecar[0] == EXE_SHA256 and sidecar[1] in ("tze_hud.exe", "*tze_hud.exe"),
                "staged hash does not match the independently witnessed PR binary")
        exe = Path(contents.extract("tze_hud.exe", directory / "release-dev"))
    with exe.open("rb") as handle:
        exe_sha = hashlib.file_digest(handle, "sha256").hexdigest()
    require(exe_sha == EXE_SHA256, "executable does not match the independently witnessed PR binary")
    report["artifact"] = {"run": RUN_ID, "id": ARTIFACT_ID, "archive_sha256": archive_sha, "exe_sha256": exe_sha}
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


def settle_cover(win, hud: dict, helper: dict, point: tuple[int, int], samples: list[dict]) -> bool:
    start = time.monotonic()
    stable = 0
    expected = {"foreground": hud, "at_point": helper, "cover_above_hud": True, "hud_above_cover": False}
    while time.monotonic() - start < COVER_SETTLE_S:
        state = observe_cover(win, hud, helper, point)
        elapsed = time.monotonic() - start
        samples.append({"elapsed_ms": round(elapsed * 1000), **state})
        stable = stable + 1 if state == expected else 0
        if stable == 2:
            return elapsed < COVER_SETTLE_S
        time.sleep(max(0, min(COVER_SAMPLE_S, COVER_SETTLE_S - elapsed)))
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


def verify_timeline(timeline: dict, fixture: dict, before: dict, after: dict) -> dict:
    hud, helper, point = fixture["hud"]["identity"], fixture["cover"]["identity"], fixture["point"]
    results = timeline["results"]
    require(len(results) == 1 and results[0]["scenario"] == "card-link" and results[0]["outcome"] == "PASS"
            and not timeline["failure"], "original card-link scenario did not pass")
    require(before["sha"] == after["sha"] == BUILD_SOURCE and before["pid"] == after["pid"] == hud["pid"],
            "binary or HUD identity changed during the scenario")
    require(after["link_launches"] == before["link_launches"] + 1,
            "expected one browser handoff across both native presses")
    steps = timeline["steps"]
    raises = [(index, step) for index, step in enumerate(steps)
              if step["step"] == "card link first press requested raise"]
    require(len(raises) == 1, "exactly one first-press native raise request must be observed")
    require(not any(step["step"] == "card link first press requested activate" for step in steps),
            "first-press evidence also changed the foreground")
    raise_index, raised = raises[0]
    require(raised["owned_window"] == hud and raised["intended_point"] == point
            and raised["card_link_point"] == {"foreground": hud, "at_point": helper},
            "raise did not observe the owned normal cover over the foreground HUD")
    checks = [(index, step) for index, step in enumerate(steps) if "click_target_before_send" in step]
    require(len(checks) == 2 and raise_index < checks[0][0], "raise must precede both original click checks")
    require(any(step["step"] == "ok then says it opened" for step in steps[checks[0][0] + 1:checks[1][0]]),
            "the original scenario did not observe a completed browser handoff before the repeat")
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
    return {"raise": raised, "checks": [step for _, step in checks], "button_batches": buttons,
            "seconds_between_checks": elapsed, "link_launches": [before["link_launches"], after["link_launches"]]}


def witness(exe: Path, out: Path, report: dict) -> None:
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

            def receive(phase: str) -> dict:
                try:
                    line = lines.get(timeout=10)
                except queue.Empty:
                    raise RuntimeError(f"cover timed out before {phase}") from None
                require(bool(line), f"cover exited before {phase}")
                receipt = json.loads(line)
                require(receipt.get("phase") == phase and receipt.get("pid") == child.pid,
                        "cover handshake does not match the freshly started native helper")
                return receipt

            report["cover_ready"] = receive("ready")
            open_card = gestures.open_card

            def cover_after_publish(run, key, **kwargs):
                require(key == CARD_KEY and kwargs.get("park_pointer") is False and not run.inj.calls,
                        "cover must be armed before the first original input")
                status = run.status()
                require(status["pid"] == run.pid and status["sha"] == BUILD_SOURCE and status["channel"] == "ci",
                        "running binary does not match the pinned build stamp")
                report["build"] = {k: status[k] for k in ("pid", "version", "sha", "channel")}
                card = open_card(run, key, **kwargs)
                require(not run.inj.calls, "original publication unexpectedly moved the pointer")
                point = gestures.resolve_target(run.status(), TARGET)
                hud = {"hwnd": win.fullscreen_window(run.pid), "pid": run.pid}
                baseline = {"foreground": win.foreground_identity(), "at_point": win.window_at(point)}
                report["fixture"] = {"point": list(point), "baseline": baseline, "samples": []}
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
                created = receive("created")
                report["cover_created"] = created
                require(isinstance(created.get("hwnd"), int) and created["hwnd"] > 0,
                        "cover has no native window handle")
                helper = {"hwnd": created["hwnd"], "pid": child.pid}
                transitions = fixture["band_transitions"] = []
                for phase, _, _, _ in COVER_BANDS:
                    transitions.append(receive(phase))
                cover = live_window(win, helper)
                fixture["cover"] = cover
                stable = settle_cover(win, hud, helper, point, fixture["samples"])
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
                return card

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

            with mock.patch.object(gestures, "open_card", cover_after_publish), \
                    mock.patch.dict(gestures.SCENARIOS, {"card-link": observe_after_both_presses}):
                code = gestures.main(["--exe", str(exe), "--out", str(artifact), "--injector", "sendinput", "card-link"])
            require(code == 0, "original gesture harness reported a failure")
            timeline, before, after = (
                json.loads((artifact / filename).read_text(encoding="utf-8"))
                for filename in ("timeline.json", "card-link.before.status.json", "card-link.after.status.json")
            )
            report["native"] = verify_timeline(timeline, report["fixture"], before, after)
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
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--artifact-metadata", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--cover-child", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    hosted_only()
    if args.cover_child:
        child_window()
        return 0
    if args.archive is None or args.artifact_metadata is None or args.out is None:
        parser.error("--archive, --artifact-metadata and --out are required")
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    report = {"outcome": "FAIL"}
    try:
        report["identity"] = source_identity()
        with tempfile.TemporaryDirectory(prefix="card-link-raise-build-") as directory:
            exe = original_executable(args.archive, args.artifact_metadata, Path(directory), report)
            witness(exe, out, report)
        report["outcome"] = "PASS"
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        print(report["error"], file=sys.stderr)
    finally:
        (out / "witness.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return int(report["outcome"] != "PASS")


if __name__ == "__main__":
    raise SystemExit(main())
