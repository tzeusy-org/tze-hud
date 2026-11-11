#!/usr/bin/env python3
"""Run scenario 11 with a byte-pinned software Vulkan ICD on a hosted Windows VM.

The runner already supplies the Vulkan loader. Register only this temporary
lavapipe manifest, then remove that exact value even if the scenario fails.
No production backend fallback or machine-wide Mesa deployment script is used.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

import hud_gestures

VERSION = "26.2.6"
ASSET_ID = 704037727
SHA256 = "b2e00615feaba45942343c265cc8c5f576142ca682c1ac6661922d4b72492877"
REGISTRY_KEY = r"SOFTWARE\Khronos\Vulkan\Drivers"


def extract_driver(work: Path, *, expected_sha256: str = SHA256) -> Path:
    """Verify upstream release bytes before extracting anything; no retries."""
    archive = work / "mesa.7z"
    with archive.open("wb") as output:
        subprocess.run(
            ["gh", "api", "--method", "GET", f"repos/pal1000/mesa-dist-win/releases/assets/{ASSET_ID}",
             "-H", "Accept: application/octet-stream", "-H", "X-GitHub-Api-Version: 2022-11-28"],
            stdout=output, stderr=subprocess.PIPE, check=True, timeout=45,
        )
    if hashlib.sha256(archive.read_bytes()).hexdigest() != expected_sha256:
        raise RuntimeError("Mesa archive SHA256 does not match the pinned digest")
    subprocess.run(["7z", "x", "-y", f"-o{work}", str(archive),
                    "x64/lvp_icd.x86_64.json", "x64/vulkan_lvp.dll"],
                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, timeout=30)
    manifest = work / "x64/lvp_icd.x86_64.json"
    driver = work / "x64/vulkan_lvp.dll"
    if not manifest.is_file() or not driver.is_file():
        raise RuntimeError("verified Mesa archive did not supply the x64 Vulkan driver")
    return manifest


@contextmanager
def registered_driver(manifest: Path, registry):
    """The elevated runner ignores VK_DRIVER_FILES; use the software-ICD key.

    Khronos LoaderDriverInterface.md documents both this registry location and
    the elevated-process environment restriction. Never replace an existing value.
    """
    access = registry.KEY_READ | registry.KEY_WRITE | registry.KEY_WOW64_64KEY
    with registry.CreateKeyEx(registry.HKEY_LOCAL_MACHINE, REGISTRY_KEY, 0, access) as key:
        name = str(manifest.resolve())
        try:
            registry.QueryValueEx(key, name)
        except FileNotFoundError:
            pass
        else:
            raise RuntimeError("refusing to replace an existing Vulkan driver registration")
        registry.SetValueEx(key, name, 0, registry.REG_DWORD, 0)
        try:
            yield
        finally:
            registry.DeleteValue(key, name)
            try:
                registry.QueryValueEx(key, name)
            except FileNotFoundError:
                pass
            else:
                raise RuntimeError("temporary Vulkan driver registration survived cleanup")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    if sys.platform != "win32" or os.environ.get("GITHUB_ACTIONS") != "true":
        parser.error("software Vulkan installation runs only on a hosted Windows CI VM")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    receipt = {"mesa_version": VERSION, "asset_id": ASSET_ID, "sha256": SHA256,
               "setup_seconds": None, "scenario_exit": None, "failure": None}
    result = 1
    try:
        import winreg
        if hud_gestures.windows_huds():
            raise RuntimeError("a HUD is already running; refusing temporary ICD installation")
        loader = Path(os.environ["WINDIR"]) / "System32/vulkan-1.dll"
        if not loader.is_file():
            raise RuntimeError("hosted runner has no preinstalled Vulkan loader")
        receipt["loader"] = str(loader)
        with tempfile.TemporaryDirectory(prefix="hud-lavapipe-") as directory:
            manifest = extract_driver(Path(directory))
            with registered_driver(manifest, winreg):
                receipt["setup_seconds"] = round(time.monotonic() - started, 3)
                result = hud_gestures.main(["--exe", str(args.exe), "--injector", "sendinput",
                                           "--window-mode", "overlay", "--out", str(args.out),
                                           "overlay-pointer", "tray"])
                receipt["scenario_exit"] = result
            receipt["registration_removed"] = True
    except Exception as error:  # retain setup/cleanup failures alongside the scenario artifacts
        receipt["failure"] = f"{type(error).__name__}: {error}"
        print(f"FAIL overlay setup/cleanup: {receipt['failure']}", file=sys.stderr)
        result = 1
    finally:
        receipt["total_seconds"] = round(time.monotonic() - started, 3)
        (args.out / "setup.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
        print(json.dumps(receipt), flush=True)
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as output:
                output.write(f"\nOverlay driver setup: {receipt['setup_seconds']} s; "
                             f"added occupancy including scenario and cleanup: {receipt['total_seconds']} s.\n")
                if receipt["failure"]:
                    output.write(f"\nOverlay setup/cleanup failed: {receipt['failure']}\n")
    return result


if __name__ == "__main__":
    sys.exit(main())
