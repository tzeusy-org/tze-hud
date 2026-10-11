#!/usr/bin/env python3
"""Operator calls on the HUD's MCP port; the PSK comes from the paired file.

  hud_admin.py status
  hud_admin.py doctor
  hud_admin.py logs [--tail 200]
  hud_admin.py screenshot [-o hud.png] [--display N | --all]
  hud_admin.py update [--channel dev|stable|vX.Y.Z]
  hud_admin.py restart

Needs an agent paired with --admin. Host comes from HUD_HOST.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import json
import os
import re
import signal
import socket
import sys
import time
import urllib.error
import urllib.request

import hud_env


DOCTOR_LAYER_SECONDS = 5
DEV_API = "https://api.github.com/repos/tzeusy-org/tze-hud"


class DoctorTimeout(Exception):
    """Whole-layer deadline, including resolver or response read."""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # Never forward a paired bearer to another endpoint.


@contextmanager
def doctor_deadline():
    """Bound resolver and whole HTTP reads, including trickling responses.

    Doctor is a POSIX agent-host command; unsupported hosts fail closed.
    Restore any caller alarm without extending its deadline.
    """
    if not hasattr(signal, "setitimer"):
        raise RuntimeError("deadline unavailable")
    old_handler = signal.getsignal(signal.SIGALRM)
    started = time.monotonic()

    def expired(*_):
        raise DoctorTimeout

    signal.signal(signal.SIGALRM, expired)
    old_timer = signal.getitimer(signal.ITIMER_REAL)
    bound = min(DOCTOR_LAYER_SECONDS, old_timer[0]) if old_timer[0] else DOCTOR_LAYER_SECONDS
    signal.setitimer(signal.ITIMER_REAL, bound)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old_handler)
        if old_timer[0]:
            signal.setitimer(signal.ITIMER_REAL, max(0.000001, old_timer[0] - (time.monotonic() - started)), old_timer[1])


def doctor_json(url: str, psk: str | None = None) -> dict:
    headers = {"Accept": "application/json", "User-Agent": "tze-hud-doctor"}
    if psk is not None:
        headers["Authorization"] = f"Bearer {psk}"
    request = urllib.request.Request(url, headers=headers, method="GET")
    with urllib.request.build_opener(NoRedirect()).open(request, timeout=DOCTOR_LAYER_SECONDS) as response:
        data = response.read(131073)
    if len(data) > 131072:
        raise ValueError("oversized response")
    result = json.loads(data)
    if not isinstance(result, dict):
        raise ValueError("invalid response")
    return result


def dev_commit() -> str:
    """Require an existing dev release and resolve its tag to an actual commit."""
    release = doctor_json(f"{DEV_API}/releases/tags/dev")
    if release.get("tag_name") != "dev" or release.get("draft") is not False:
        raise ValueError("unknown dev release")
    obj = doctor_json(f"{DEV_API}/git/ref/tags/dev").get("object", {})
    for _ in range(4):
        sha = obj.get("sha", "")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("unknown dev commit")
        if obj.get("type") == "commit":
            return sha.lower()
        if obj.get("type") != "tag":
            break
        obj = doctor_json(f"{DEV_API}/git/tags/{sha}").get("object", {})
    raise ValueError("unresolved dev tag")


def doctor(host: str | None) -> int:
    """Read-only first-failure diagnosis; output only whitelisted classifications."""
    layer = "endpoint"
    action = "Select a valid HTTP HUD host[:port] without credentials."
    try:
        name, url = hud_env._adapter_url(host or os.environ.get("HUD_HOST", ""))
        port = hud_env._split(url)[1]
        layer, action = "dns", "Check the selected tailnet hostname and Tailscale DNS on the agent host."
        with doctor_deadline():
            addresses = socket.getaddrinfo(name, port, type=socket.SOCK_STREAM)
            if not addresses:
                raise socket.gaierror
        layer, action = "tcp", "Check HUD listener and tailnet reachability; the owner must inspect host firewall policy."
        with doctor_deadline():
            last_error = None
            for family, kind, protocol, _, address in addresses:
                try:
                    with socket.socket(family, kind, protocol) as connection:
                        connection.settimeout(DOCTOR_LAYER_SECONDS)
                        connection.connect(address)
                    break
                except OSError as error:
                    last_error = error
            else:
                raise last_error or ConnectionError()
        layer, action = "auth", "Ask the owner to supply the selected valid admin pairing; no pairing or privilege change was performed."
        with doctor_deadline():
            status = doctor_json(url.removesuffix("/mcp") + "/admin/status", hud_env.load_psk(host))
        layer, action = "firewall", "Ask the owner to inspect tailnet inbound policy on the intended Windows HUD; no repair was performed."
        firewall = status.get("tailnet_inbound", {})
        state = firewall.get("state") if isinstance(firewall, dict) else None
        if state != "allowed":
            reason = state if state in ("blocked", "unknown", "not_applicable") else "unknown"
            print(json.dumps({"ok": False, "layer": layer, "reason": reason, "action": action}))
            return 1
        layer, action = "build", "Ask the owner to identify the running HUD build commit; unknown build provenance cannot pass."
        sha = status.get("sha", "")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError
        layer, action = "release", "Check availability and commit provenance of the rolling dev release; local HUD checks already completed."
        with doctor_deadline():
            latest = dev_commit()
        if sha.lower() != latest:
            print(json.dumps({"ok": False, "layer": "build", "reason": "sha_mismatch",
                              "action": "Ask the owner to update to the resolved dev commit; no update was performed."}))
            return 1
        print(json.dumps({"ok": True, "layer": "dev_commit", "sha": latest}))
        return 0
    except Exception as error:
        if isinstance(error, urllib.error.HTTPError):
            reason = ("unauthenticated" if layer == "auth" and error.code == 401 else
                      "not_admin" if layer == "auth" and error.code == 403 else "http_error")
            error.close()
        elif isinstance(error, socket.gaierror):
            reason = "resolution_failed"
        elif isinstance(error, ConnectionRefusedError):
            reason = "connection_refused"
        elif isinstance(error, (TimeoutError, DoctorTimeout)):
            reason = "deadline_exceeded"
        elif isinstance(error, hud_env.HudEnvError):
            reason = "pairing_unavailable" if layer == "auth" else "invalid_endpoint"
        elif isinstance(error, RuntimeError):
            reason = "deadline_unavailable"
        else:
            reason = "unknown"
        if reason == "deadline_unavailable":
            action = "Run doctor from a POSIX alarm-capable agent host; no network probe was performed."
        print(json.dumps({"ok": False, "layer": layer, "reason": reason, "action": action}))
        return 1


def call(path: str, body: dict | None = None, host: str | None = None) -> tuple[int, bytes]:
    request = urllib.request.Request(
        hud_env.base_url(host) + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {hud_env.load_psk(host)}"},
        method="GET" if body is None else "POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def shot_path(output: str, index: int) -> str:
    stem, dot, ext = output.rpartition(".")
    return f"{stem}-{index}.{ext}" if dot else f"{output}-{index}"


def screenshot(args) -> tuple[int, bytes]:
    if args.all:
        status, data = call("/admin/status", host=args.host)
        if status != 200:
            return status, data
        indices = range(len(json.loads(data).get("displays") or [None]))
        targets = [(i, shot_path(args.output, i)) for i in indices]
    else:
        targets = [(args.display, args.output)]
    written = []
    for index, path in targets:
        status, data = call(f"/admin/screenshot?display={index}", host=args.host)
        if status != 200:
            return status, data
        with open(path, "wb") as handle:
            handle.write(data)
        written.append(path)
    return 200, "".join(f"wrote {p}\n" for p in written).encode()


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", help="HUD host (default: HUD_HOST)")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("status")
    sub.add_parser("doctor", help="bounded read-only tailnet/admin/dev-commit diagnosis")
    sub.add_parser("restart")
    logs = sub.add_parser("logs")
    logs.add_argument("--tail", type=int, default=100)
    shot = sub.add_parser("screenshot")
    shot.add_argument("-o", "--output", default="hud.png")
    which = shot.add_mutually_exclusive_group()
    which.add_argument("--display", type=int, default=0, help="display index from status (0 = primary)")
    which.add_argument("--all", action="store_true", help="every display: hud-<i>.png beside --output")
    update = sub.add_parser("update")
    update.add_argument("--channel", default="dev")
    args = parser.parse_args(argv)

    if args.cmd == "doctor":
        return doctor(args.host)

    try:
        if args.cmd == "status":
            status, data = call("/admin/status", host=args.host)
        elif args.cmd == "logs":
            status, data = call(f"/admin/logs?tail={args.tail}", host=args.host)
        elif args.cmd == "screenshot":
            status, data = screenshot(args)
        elif args.cmd == "update":
            status, data = call("/admin/update", {"channel": args.channel}, args.host)
        else:
            status, data = call("/admin/restart", {}, args.host)
    except hud_env.HudEnvError as error:
        print(f"hud_admin: {error}", file=sys.stderr)
        return 2
    except OSError as error:
        print(f"hud_admin: {type(error).__name__} reaching the HUD", file=sys.stderr)
        return 1
    sys.stdout.write(data.decode("utf-8", errors="replace"))
    return 0 if 200 <= status < 300 else 1


if __name__ == "__main__":
    raise SystemExit(main())
