#!/usr/bin/env python3
"""Install, single-instance, and uninstall smoke for tze_hud.exe (Windows CI).

LOCALAPPDATA/APPDATA point at temp dirs, so the per-user install lands there.
The HKCU Run / App Paths keys are real (there is no redirecting HKCU) and are
removed again by `--uninstall`, and in `finally` if a check fails first.

Checks: `--install` copies the exe, writes the default config, registers the
Run value, and relaunches an instance that answers on loopback; a second
bare launch exits 0 quickly and one with explicit args exits non-zero; a real
named-object collision refuses startup and install before mutations;
`--uninstall --purge` removes the Run value, stops the
instance within 10 s, and deletes the install and config dirs.

    python scripts/ci/windows_install_smoke.py --exe target/release/tze_hud.exe
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
from datetime import datetime
from ctypes import wintypes
import hashlib
import json
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import winreg
from pathlib import Path

from windows_smoke import Smoke

RUN_KEY = r"Software\Microsoft\Windows\CurrentVersion\Run"
APP_PATHS_KEY = r"Software\Microsoft\Windows\CurrentVersion\App Paths\tze_hud.exe"


def run_value() -> str | None:
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY) as key:
            return winreg.QueryValueEx(key, "tze_hud")[0]
    except FileNotFoundError:
        return None


def app_paths_exists() -> bool:
    try:
        winreg.CloseKey(winreg.OpenKey(winreg.HKEY_CURRENT_USER, APP_PATHS_KEY))
        return True
    except FileNotFoundError:
        return False


def mcp_answers(smoke: Smoke) -> bool:
    try:
        smoke.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})
        return True
    except (urllib.error.URLError, ConnectionError, TimeoutError, OSError):
        return False


def wait_until(predicate, timeout_s: float, what: str) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.5)
    raise AssertionError(f"timed out after {timeout_s:.0f}s waiting for {what}")


def lifecycle_events(log: Path, pid: int, after: int = 0) -> list[dict]:
    """Read only this launch's PID-bound checkpoints, retaining their times."""
    with log.open("rb") as stream:
        stream.seek(after)
        lines = stream.read().decode("utf-8", errors="replace").splitlines()
    events = []
    for line in lines:
        if "[DEBUG-hud-lks0i] lifecycle" not in line:
            continue
        match = re.search(r'\bpid=(\d+)\s+phase="?([a-z_]+)', line)
        if match and int(match[1]) == pid:
            events.append({"timestamp": line.split()[0], "pid": pid, "phase": match[2]})
    return events


def early_quit_coverage(events: list[dict]) -> str:
    """Require positive, ordered checkpoints; missing evidence is unexercised."""
    names = ("startup_compositor_begin", "quit_requested", "startup_compositor_ready")
    try:
        indices = [next(i for i, event in enumerate(events) if event["phase"] == name)
                   for name in names]
        times = [datetime.fromisoformat(events[i]["timestamp"].replace("Z", "+00:00"))
                 for i in indices]
    except (StopIteration, KeyError, TypeError, ValueError):
        return "UNEXERCISED"
    pids = {events[i].get("pid") for i in indices}
    if len(pids) != 1 or None in pids or any(time.tzinfo is None for time in times):
        return "UNEXERCISED"
    ordered = indices[0] < indices[1] < indices[2] and times[0] < times[1] < times[2]
    return "EXERCISED" if ordered else "UNEXERCISED"


def report_lifecycle(log: Path, instance: subprocess.Popen, checkpoint: str,
                     after: int = 0) -> list[dict]:
    """Report bounded phase evidence, never substitute it for process exit."""
    try:
        events = lifecycle_events(log, instance.pid, after)
    except OSError as error:
        print(
            f"lifecycle checkpoint={checkpoint} tracked_pid={instance.pid} "
            f"exit={instance.poll()} log_unavailable={type(error).__name__}",
            flush=True,
        )
        return []  # Diagnostic failure must not replace the original smoke failure.
    phases = [event["phase"] for event in events]
    print(
        f"lifecycle checkpoint={checkpoint} tracked_pid={instance.pid} "
        f"exit={instance.poll()} graphical_ready={'graphical_ready' in phases} "
        f"startup_returned={'resumed_return' in phases} "
        f"phases={phases[-32:]} events={json.dumps(events[-32:])}",
        flush=True,
    )
    return events


def request_quit() -> None:
    """Signal the production installer's existing event once, without cleanup."""
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenEventW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel.OpenEventW.restype = wintypes.HANDLE
    kernel.SetEvent.argtypes = [wintypes.HANDLE]
    kernel.SetEvent.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    # Same EVENT_MODIFY_STATE access and name as operator::install::win::signal.
    handle = kernel.OpenEventW(0x0002, False, r"Local\tze_hud.quit")
    if not handle:
        raise AssertionError(f"OpenEventW quit failed: {ctypes.get_last_error()}")
    signal_error = None
    try:
        if not kernel.SetEvent(handle):
            signal_error = ctypes.get_last_error()
    finally:
        closed = kernel.CloseHandle(handle)
        close_error = ctypes.get_last_error() if not closed else None
    if signal_error is not None:
        raise AssertionError(f"SetEvent quit failed: {signal_error}; close_error={close_error}")
    assert closed, f"CloseHandle quit failed: {close_error}"


def probe_early_quit(command: list[str], env: dict[str, str], smoke: Smoke, log: Path) -> None:
    """One early ordering sample; never replay it to obtain coverage."""
    after = log.stat().st_size if log.exists() else 0
    instance = subprocess.Popen(command, env=env)
    try:
        wait_until(lambda: mcp_answers(smoke), 90, "early instance to answer on loopback")
        assert instance.poll() is None, "early instance exited before quit"
        request_quit()  # No duplicate-launch subprocess or sleep before the signal.
        wait_until(lambda: instance.poll() is not None, 10, "early instance to exit after quit")
        assert instance.returncode == 0, f"early quit exited {instance.returncode}"
    finally:
        events = report_lifecycle(log, instance, "early_quit_terminal", after)
        print(f"early-quit tracked_pid={instance.pid} coverage={early_quit_coverage(events)}", flush=True)


def graphical_ready(log: Path, pid: int, after: int) -> bool:
    try:
        return any(event["phase"] == "graphical_ready" for event in lifecycle_events(log, pid, after))
    except OSError:
        return False


def dump_logs(local: Path) -> None:
    """Print every file in the redirected log dir (tze_hud.log, hud-diag.log)."""
    logs = local / "tze_hud" / "logs"
    files = sorted(logs.glob("*")) if logs.is_dir() else []
    print(f"--- instance logs in {logs} ({len(files)} files) ---", file=sys.stderr)
    for f in files:
        print(f"--- {f.name} ---", file=sys.stderr)
        print(f.read_text(errors="replace")[-20_000:], file=sys.stderr)


@contextmanager
def mutex_acquisition_error():
    """Own a conflicting event, making the real CreateMutexW fail with code 6.

    Windows shares names between mutexes and events (CreateMutexW contract):
    https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createmutexw
    A separate owned quit event exercises install's conditional predecessor wait.
    No HUD instance is running during this fixture, and every handle is closed.
    """
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateEventW.argtypes = [wintypes.LPVOID, wintypes.BOOL, wintypes.BOOL, wintypes.LPCWSTR]
    kernel.CreateEventW.restype = wintypes.HANDLE
    kernel.CreateMutexW.argtypes = [wintypes.LPVOID, wintypes.BOOL, wintypes.LPCWSTR]
    kernel.CreateMutexW.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    handles = []
    try:
        for name in (r"Local\tze_hud", r"Local\tze_hud.quit"):
            ctypes.set_last_error(0)
            handle = kernel.CreateEventW(None, True, False, name)
            error = ctypes.get_last_error()
            if not handle:
                raise ctypes.WinError(error)
            handles.append(handle)
            assert error != 183, f"fixture does not own a fresh event: {name}"
        ctypes.set_last_error(0)
        probe = kernel.CreateMutexW(None, False, r"Local\tze_hud")
        error = ctypes.get_last_error()
        if probe:
            handles.append(probe)
        assert not probe and error == 6, f"expected ERROR_INVALID_HANDLE, got handle={probe}, error={error}"
        yield
    finally:
        for handle in reversed(handles):
            if not kernel.CloseHandle(handle):
                raise ctypes.WinError(ctypes.get_last_error())


def main() -> int:
    sys.stdout.reconfigure(line_buffering=True)
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--exe", required=True, type=Path)
    ap.add_argument("--mcp-port", type=int, default=9090)
    args = ap.parse_args()

    root = Path(tempfile.mkdtemp(prefix="tze_hud_install_smoke_"))
    local, roaming = root / "Local", root / "Roaming"
    install_dir = local / "Programs" / "tze_hud"
    config_dir = roaming / "tze_hud"
    env = {**os.environ, "LOCALAPPDATA": str(local), "APPDATA": str(roaming)}

    # Pair one agent the way pairing stores it; install must leave it alone.
    psk = secrets.token_hex(32)
    config_dir.mkdir(parents=True)
    digest = hashlib.sha256(psk.encode()).hexdigest()
    (config_dir / "agents.toml").write_text(
        f'[agents.ci-smoke]\npsk_sha256 = "{digest}"\nallow = ["*"]\n', encoding="utf-8"
    )
    smoke = Smoke(f"http://127.0.0.1:{args.mcp_port}/", psk)
    installed = install_dir / "tze_hud.exe"

    def run(exe: Path, *flags: str, timeout: float) -> subprocess.CompletedProcess:
        return subprocess.run([str(exe), *flags], env=env, timeout=timeout)

    instance = None
    log = local / "tze_hud" / "logs" / "tze_hud.log"
    try:
        # Execute the real startup and install consumers under a kernel error,
        # before the positive install/duplicate/uninstall cases below. Requiring
        # the acquisition diagnostic distinguishes unrelated config/GPU exits.
        before_registry = (run_value(), app_paths_exists())
        with mutex_acquisition_error():
            production_config = Path(__file__).resolve().parents[2] / "app/tze_hud_app/config/production.toml"
            startup = subprocess.run(
                [str(args.exe), "--config", str(production_config), "--window-mode", "fullscreen"],
                env=env, capture_output=True, text=True, timeout=15,
            )
            assert startup.returncode == 1, f"mutex-error startup exited {startup.returncode}"
            assert "could not acquire the single-instance mutex" in startup.stderr, (
                f"startup failed for another reason: {startup.stderr[-2000:]}"
            )
            refused = subprocess.run(
                [str(args.exe), "--install"], env=env, capture_output=True, text=True, timeout=15,
            )
            assert refused.returncode == 1, f"mutex-error install exited {refused.returncode}"
            assert "confirm previous instance exit" in refused.stderr, (
                f"install failed for another reason: {refused.stderr[-2000:]}"
            )
            assert not install_dir.exists(), "mutex-error install staged an executable"
            assert not (config_dir / "config.toml").exists(), "mutex-error install wrote config"
            assert (run_value(), app_paths_exists()) == before_registry, "mutex-error install changed registry"
        print("ok  actual CreateMutexW error: startup exits 1; install refuses staging and registration")

        done = run(args.exe, "--install", timeout=60)
        assert done.returncode == 0, f"--install exited {done.returncode}"
        assert installed.is_file(), f"{installed} not copied"
        assert (config_dir / "config.toml").is_file(), "default config not written"
        assert (config_dir / "agents.toml").read_text(encoding="utf-8").count(digest) == 1, (
            "install touched agents.toml"
        )
        want = f'"{installed}" --config "{config_dir / "config.toml"}" --window-mode overlay'
        got = run_value()
        assert got is not None and got.casefold() == want.casefold(), f"Run value {got!r}, want {want!r}"
        assert app_paths_exists(), "App Paths key missing"
        print("ok  --install: exe copied, config written, Run + App Paths registered")

        # The relaunched instance runs the autostart command (overlay), which
        # needs Vulkan; the CI runner only has WARP, so it cannot serve. Prove
        # it launched from its log, then stand in for it with a fullscreen
        # instance started from the installed exe with the same config.
        wait_until(
            lambda: log.exists() and log.read_text(errors="replace").count("tze_hud runtime starting") >= 1,
            30,
            "the relaunched instance to log 'tze_hud runtime starting'",
        )
        print("ok  --install relaunched the installed exe")
        subprocess.run(["taskkill", "/F", "/IM", "tze_hud.exe"], capture_output=True)
        time.sleep(2)
        probe_early_quit(
            [str(installed), "--config", str(config_dir / "config.toml"), "--window-mode", "fullscreen",
             "--mcp-port", str(args.mcp_port)],
            env, smoke, log,
        )
        instance_log_start = log.stat().st_size if log.exists() else 0
        instance = subprocess.Popen(
            [str(installed), "--config", str(config_dir / "config.toml"), "--window-mode", "fullscreen",
             "--mcp-port", str(args.mcp_port)],
            env=env,
        )
        wait_until(lambda: mcp_answers(smoke), 90, "installed instance to answer on loopback")
        assert instance.poll() is None, "installed instance exited"
        print("ok  installed instance answers on loopback")
        report_lifecycle(log, instance, "mcp_ready", instance_log_start)
        wait_until(lambda: graphical_ready(log, instance.pid, instance_log_start), 90,
                   "late instance to complete graphical initialization")

        start = time.monotonic()
        second = run(installed, timeout=10)
        elapsed = time.monotonic() - start
        assert second.returncode == 0 and elapsed < 10, f"second launch: exit {second.returncode} in {elapsed:.1f}s"
        assert mcp_answers(smoke), "first instance died when a second launched"
        print(f"ok  second launch exited 0 in {elapsed:.1f}s; first instance still up")

        # A launch that carried explicit args (benchmark/CI) must not no-op.
        explicit = run(installed, "--mcp-port", "9091", timeout=10)
        assert explicit.returncode != 0, "second launch with explicit args exited 0; it must fail loudly"
        print(f"ok  second launch with explicit args exited {explicit.returncode}")

        report_lifecycle(log, instance, "before_uninstall", instance_log_start)
        done = run(installed, "--uninstall", "--purge", timeout=30)
        assert done.returncode == 0, f"--uninstall exited {done.returncode}"
        assert run_value() is None, "Run value still present after uninstall"
        assert not app_paths_exists(), "App Paths key still present after uninstall"
        wait_until(lambda: instance.poll() is not None, 10, "instance to exit after uninstall")
        report_lifecycle(log, instance, "process_exited", instance_log_start)
        wait_until(lambda: not install_dir.exists(), 20, "install dir to be deleted")
        wait_until(lambda: not config_dir.exists(), 20, "config dir to be purged")
        print("ok  --uninstall --purge: Run value gone, instance exited, dirs removed")
        return 0
    except (AssertionError, subprocess.TimeoutExpired) as err:
        print(f"FAIL {err}", file=sys.stderr)
        if instance is not None:
            report_lifecycle(log, instance, "failure", instance_log_start)
        for d in (install_dir, config_dir, local / "tze_hud"):
            left = [str(f.relative_to(root)) for f in d.rglob("*")] if d.exists() else []
            print(f"leftover in {d.name}: {left}", file=sys.stderr)
        dump_logs(local)
        return 1
    finally:
        subprocess.run(["taskkill", "/F", "/IM", "tze_hud.exe"], capture_output=True)
        if run_value() is not None:
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY, 0, winreg.KEY_SET_VALUE) as key:
                winreg.DeleteValue(key, "tze_hud")
        if app_paths_exists():
            winreg.DeleteKey(winreg.HKEY_CURRENT_USER, APP_PATHS_KEY)
        time.sleep(1)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
