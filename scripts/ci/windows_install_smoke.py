#!/usr/bin/env python3
"""Install, single-instance, and uninstall smoke for tze_hud.exe (Windows CI).

LOCALAPPDATA/APPDATA point at temp dirs, so the per-user install lands there.
The HKCU Run / App Paths keys are real (there is no redirecting HKCU) and are
removed again by `--uninstall`, and in `finally` if a check fails first.

Checks: `--install` copies the exe, writes the default config, registers the
Run value, and relaunches an instance that answers on loopback; a second
bare launch exits 0 quickly and one with explicit args exits non-zero; `--uninstall --purge` removes the Run value, stops the
instance within 10 s, and deletes the install and config dirs.

    python scripts/ci/windows_install_smoke.py --exe target/release/tze_hud.exe
"""

from __future__ import annotations

import argparse
import hashlib
import os
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


def dump_logs(local: Path) -> None:
    """Print every file in the redirected log dir (tze_hud.log, hud-diag.log)."""
    logs = local / "tze_hud" / "logs"
    files = sorted(logs.glob("*")) if logs.is_dir() else []
    print(f"--- instance logs in {logs} ({len(files)} files) ---", file=sys.stderr)
    for f in files:
        print(f"--- {f.name} ---", file=sys.stderr)
        print(f.read_text(errors="replace")[-20_000:], file=sys.stderr)


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

    try:
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
        log = local / "tze_hud" / "logs" / "tze_hud.log"
        wait_until(
            lambda: log.exists() and log.read_text(errors="replace").count("tze_hud runtime starting") >= 1,
            30,
            "the relaunched instance to log 'tze_hud runtime starting'",
        )
        print("ok  --install relaunched the installed exe")
        subprocess.run(["taskkill", "/F", "/IM", "tze_hud.exe"], capture_output=True)
        time.sleep(2)
        instance = subprocess.Popen(
            [str(installed), "--config", str(config_dir / "config.toml"), "--window-mode", "fullscreen",
             "--mcp-port", str(args.mcp_port)],
            env=env,
        )
        wait_until(lambda: mcp_answers(smoke), 90, "installed instance to answer on loopback")
        assert instance.poll() is None, "installed instance exited"
        print("ok  installed instance answers on loopback")

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

        done = run(installed, "--uninstall", "--purge", timeout=30)
        assert done.returncode == 0, f"--uninstall exited {done.returncode}"
        assert run_value() is None, "Run value still present after uninstall"
        assert not app_paths_exists(), "App Paths key still present after uninstall"
        wait_until(lambda: instance.poll() is not None, 10, "instance to exit after uninstall")
        wait_until(lambda: not install_dir.exists(), 20, "install dir to be deleted")
        wait_until(lambda: not config_dir.exists(), 20, "config dir to be purged")
        print("ok  --uninstall --purge: Run value gone, instance exited, dirs removed")
        return 0
    except (AssertionError, subprocess.TimeoutExpired) as err:
        print(f"FAIL {err}", file=sys.stderr)
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
