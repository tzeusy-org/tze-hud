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
import ctypes
import json
import os
import re
import select
import signal
import socket
import sys
import time
import urllib.error
import urllib.request

import hud_env


DOCTOR_LAYER_SECONDS = 5
DEV_API = "https://api.github.com/repos/tzeusy-org/tze-hud"
DOCTOR_HOST_ACTION = "Run doctor from a Linux/WSL agent host with owned-process deadline support; no network probe was performed."


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # Never forward a paired bearer to another endpoint.


DOCTOR_ACTIONS = {
    "endpoint": "Select a valid HTTP HUD host[:port] without credentials.",
    "dns": "Check the selected tailnet hostname and Tailscale DNS on the agent host.",
    "tcp": "Check HUD listener and tailnet reachability; the owner must inspect host firewall policy.",
    "auth": "Ask the owner to supply the selected valid admin pairing; no pairing or privilege change was performed.",
    "firewall": "Ask the owner to inspect tailnet inbound policy on the intended Windows HUD; no repair was performed.",
    "build": "Ask the owner to identify the running HUD build commit; unknown build provenance cannot pass.",
    "release": "Check availability and commit provenance of the rolling dev release; local HUD checks already completed.",
}


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


def doctor_probe(host: str | None, progress) -> dict:
    """Read-only probe in an owned child; no Python signal deadline assumption."""
    layer = "endpoint"
    action = DOCTOR_ACTIONS[layer]
    try:
        name, url = hud_env._adapter_url(host or os.environ.get("HUD_HOST", ""))
        port = hud_env._split(url)[1]
        layer = "dns"
        action = DOCTOR_ACTIONS[layer]
        progress({"phase": layer, "deadline": time.monotonic() + DOCTOR_LAYER_SECONDS})
        addresses = socket.getaddrinfo(name, port, type=socket.SOCK_STREAM)
        if not addresses:
            raise socket.gaierror
        layer = "tcp"
        action = DOCTOR_ACTIONS[layer]
        progress({"phase": layer, "deadline": time.monotonic() + DOCTOR_LAYER_SECONDS})
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
        layer = "auth"
        action = DOCTOR_ACTIONS[layer]
        progress({"phase": layer, "deadline": time.monotonic() + DOCTOR_LAYER_SECONDS})
        status = doctor_json(url.removesuffix("/mcp") + "/admin/status", hud_env.load_psk(host))
        layer = "firewall"
        action = DOCTOR_ACTIONS[layer]
        firewall = status.get("tailnet_inbound", {})
        state = firewall.get("state") if isinstance(firewall, dict) else None
        if state != "allowed":
            reason = state if state in ("blocked", "unknown", "not_applicable") else "unknown"
            return {"ok": False, "layer": layer, "reason": reason, "action": action}
        layer = "build"
        action = DOCTOR_ACTIONS[layer]
        sha = status.get("sha", "")
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError
        layer = "release"
        action = DOCTOR_ACTIONS[layer]
        progress({"phase": layer, "deadline": time.monotonic() + DOCTOR_LAYER_SECONDS})
        latest = dev_commit()
        if sha.lower() != latest:
            return {"ok": False, "layer": "build", "reason": "sha_mismatch",
                    "action": "Ask the owner to update to the resolved dev commit; no update was performed."}
        return {"ok": True, "layer": "dev_commit", "sha": latest}
    except Exception as error:
        if isinstance(error, urllib.error.HTTPError):
            reason = ("unauthenticated" if layer == "auth" and error.code == 401 else
                      "not_admin" if layer == "auth" and error.code == 403 else "http_error")
            error.close()
        elif isinstance(error, socket.gaierror):
            reason = "resolution_failed"
        elif isinstance(error, ConnectionRefusedError):
            reason = "connection_refused"
        elif isinstance(error, TimeoutError):
            reason = "deadline_exceeded"
        elif isinstance(error, hud_env.HudEnvError):
            reason = "pairing_unavailable" if layer == "auth" else "invalid_endpoint"
        else:
            reason = "unknown"
        return {"ok": False, "layer": layer, "reason": reason, "action": action}


def reap_doctor(pid: int, pidfd: int, stop: bool) -> int:
    """Signal only this owned pidfd, then reap; every wait is bounded.

    Failure to observe exit stays cleanup_failed, never a success/reap claim.
    """
    if not stop and select.select([pidfd], [], [], 0.2)[0]:
        return os.waitpid(pid, 0)[1]
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            signal.pidfd_send_signal(pidfd, sig)
        except ProcessLookupError:
            pass
        if select.select([pidfd], [], [], 0.2)[0]:
            return os.waitpid(pid, 0)[1]
    raise RuntimeError("owned probe cleanup incomplete")


def doctor(host: str | None) -> int:
    """Parent-enforced deadlines remain effective while child C calls block."""
    if (sys.platform != "linux" or not hasattr(os, "fork") or
            not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal")):
        print(json.dumps({"ok": False, "layer": "endpoint", "reason": "deadline_unavailable",
                          "action": DOCTOR_HOST_ACTION}))
        return 1
    fds = []
    parent = os.getpid()
    try:
        read_fd, write_fd = os.pipe()
        fds.extend((read_fd, write_fd))
        start_read, start_write = os.pipe()
        fds.extend((start_read, start_write))
        pid = os.fork()
    except OSError:
        for fd in fds:
            os.close(fd)
        print(json.dumps({"ok": False, "layer": "endpoint", "reason": "deadline_unavailable", "action": DOCTOR_HOST_ACTION}))
        return 1
    if pid == 0:
        os.close(read_fd)
        os.close(start_write)
        code = 1
        try:
            def emit(message):
                os.write(write_fd, json.dumps(message).encode() + b"\n")

            # Same lifetime pattern as hud_mcp_stdio: no orphan on parent death.
            if ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
                emit({"unavailable": True})
                os._exit(1)
            signal.signal(signal.SIGTERM, signal.SIG_DFL)
            # No key load or network call until parent has a stable owned pidfd.
            if os.read(start_read, 1) != b"1":
                os._exit(1)
            os.close(start_read)
            result = doctor_probe(host, emit)
            emit({"result": result})
            code = 0 if result["ok"] else 1
        except BaseException:
            pass  # Never send an arbitrary child exception/credential context.
        finally:
            os._exit(code)
    os.close(write_fd)
    os.close(start_read)
    pidfd = None
    layer, deadline = "endpoint", time.monotonic() + DOCTOR_LAYER_SECONDS
    phases = iter(("dns", "tcp", "auth", "release"))
    result, buffer, reaped, cleanup_attempted = None, b"", False, False
    try:
        pidfd = os.pidfd_open(pid)
        os.write(start_write, b"1")
        os.close(start_write)
        start_write = None
        while result is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([read_fd], [], [], remaining)[0]:
                result = {"ok": False, "layer": layer, "reason": "deadline_exceeded", "action": DOCTOR_ACTIONS[layer]}
                break
            data = os.read(read_fd, 4096)
            if not data:
                raise ValueError("probe exited without result")
            buffer += data
            if len(buffer) > 4096:
                raise ValueError("oversized probe result")
            while b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                message = json.loads(line)
                if "phase" in message:
                    if message["phase"] != next(phases):
                        raise ValueError("invalid probe phase")
                    layer = message["phase"]
                    deadline = min(float(message["deadline"]), time.monotonic() + DOCTOR_LAYER_SECONDS)
                elif message.get("unavailable") is True:
                    result = {"ok": False, "layer": "endpoint", "reason": "deadline_unavailable", "action": DOCTOR_HOST_ACTION}
                else:
                    result = message["result"]
                    if (set(result) - {"ok", "layer", "reason", "action", "sha"} or type(result.get("ok")) is not bool or
                            result.get("action") not in (*DOCTOR_ACTIONS.values(), DOCTOR_HOST_ACTION,
                                                        "Ask the owner to update to the resolved dev commit; no update was performed.", None)):
                        raise ValueError("invalid probe result")
        cleanup_attempted = True
        status = reap_doctor(pid, pidfd, result.get("reason") == "deadline_exceeded")
        reaped = True
        if result.get("reason") != "deadline_exceeded" and os.waitstatus_to_exitcode(status) != (0 if result["ok"] else 1):
            raise ValueError("probe terminal mismatch")
    except BaseException as error:
        if start_write is not None:
            os.close(start_write)  # EOF releases the unadmitted child to exit.
            start_write = None
        if pidfd is not None and not reaped:
            try:
                if cleanup_attempted:
                    raise RuntimeError("owned probe cleanup incomplete")
                reap_doctor(pid, pidfd, True)
                reaped = True
            except (OSError, RuntimeError):
                result = {"ok": False, "layer": layer, "reason": "cleanup_failed", "action": "The owned local probe could not be confirmed stopped; inspect the agent host. No HUD repair was performed."}
        elif pidfd is None:
            # No probe was admitted. Closing the startup gate makes it exit;
            # never block indefinitely waiting on an unavailable lifetime API.
            until = time.monotonic() + 0.4
            while time.monotonic() < until:
                if os.waitpid(pid, os.WNOHANG)[0] == pid:
                    reaped = True
                    break
                time.sleep(0.01)
            result = {"ok": False, "layer": "endpoint", "reason": "deadline_unavailable" if reaped else "cleanup_failed",
                      "action": DOCTOR_HOST_ACTION if reaped else "The unadmitted local probe could not be confirmed stopped; inspect the agent host."}
        if isinstance(error, (KeyboardInterrupt, SystemExit)):
            raise
        if result is None or result.get("reason") not in ("cleanup_failed", "deadline_unavailable"):
            result = {"ok": False, "layer": layer, "reason": "unknown", "action": DOCTOR_ACTIONS[layer]}
    finally:
        if start_write is not None:
            os.close(start_write)
        os.close(read_fd)
        if pidfd is not None:
            os.close(pidfd)
    print(json.dumps(result))
    return 0 if result["ok"] else 1


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
