#!/usr/bin/env python3
"""Boot tze_hud.exe and drive the MCP lifecycle against it.

Runs on the Windows CI runner (software rendering via WARP) after the release
build. Proves the shipped binary boots the overlay with the production config
and that each lifecycle stage answers over MCP: discover, publish to a zone,
attach/poll/detach a portal, and a structured error.

The config is copied to a temp dir with a seeded agents.toml beside it holding
only the SHA-256 of a fresh random PSK, the way pairing stores agents.

Stdlib only. Exits non-zero on the first failed check and prints the HUD log.

    python scripts/ci/windows_smoke.py --exe target/release/tze_hud.exe \
        --config app/tze_hud_app/config/production.toml
"""

from __future__ import annotations

import argparse
import hashlib
import json
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

TOOLS = {"hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"}


class Smoke:
    def __init__(self, url: str, psk: str) -> None:
        self.url = url
        self.psk = psk
        self.next_id = 0

    def rpc(self, method: str, params: dict | None = None) -> dict:
        self.next_id += 1
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
        with urllib.request.urlopen(req, timeout=10) as resp:
            reply = json.load(resp)
        if "error" in reply:
            raise AssertionError(f"{method}: JSON-RPC error {reply['error']}")
        return reply["result"]

    def call(self, tool: str, args: dict | None = None) -> tuple[bool, dict]:
        """Call a tool; return (is_error, parsed result text)."""
        result = self.rpc("tools/call", {"name": tool, "arguments": args or {}})
        text = result["content"][0]["text"]
        return bool(result.get("isError")), json.loads(text)

    def ok(self, tool: str, args: dict | None = None) -> dict:
        is_error, payload = self.call(tool, args)
        if is_error:
            raise AssertionError(f"{tool} {args}: {payload}")
        return payload


def wait_for_mcp(smoke: Smoke, proc: subprocess.Popen, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            raise AssertionError(f"tze_hud exited during startup (code {proc.returncode})")
        try:
            smoke.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})
            return
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.5)
    raise AssertionError(f"MCP not reachable at {smoke.url} after {timeout_s:.0f}s")


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


def seed_config(config: Path, psk: str) -> Path:
    """Copy `config` to a temp dir and pair one agent for `psk` beside it."""
    config_dir = Path(tempfile.mkdtemp(prefix="tze_hud_smoke_"))
    seeded = config_dir / config.name
    shutil.copyfile(config, seeded)
    digest = hashlib.sha256(psk.encode()).hexdigest()
    (config_dir / "agents.toml").write_text(
        f'[agents.ci-smoke]\npsk_sha256 = "{digest}"\nallow = ["*"]\n', encoding="utf-8"
    )
    return seeded


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--exe", required=True, type=Path)
    ap.add_argument("--config", required=True, type=Path)
    ap.add_argument("--mcp-port", type=int, default=9090)
    ap.add_argument("--startup-timeout", type=float, default=90)
    ap.add_argument("--settle-s", type=float, default=3, help="seconds the HUD must stay up after checks")
    args = ap.parse_args()

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
    try:
        wait_for_mcp(smoke, proc, args.startup_timeout)
        print("ok  MCP initialize")
        run_checks(smoke)
        time.sleep(args.settle_s)
        assert proc.poll() is None, f"tze_hud exited after the checks (code {proc.returncode})"
        print("ok  HUD still running after the checks")
        return 0
    except AssertionError as err:
        print(f"FAIL {err}", file=sys.stderr)
        return 1
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=10)
        print(f"--- HUD log ({log_path}) ---")
        print(log_path.read_text(errors="replace")[-20_000:])


if __name__ == "__main__":
    sys.exit(main())
