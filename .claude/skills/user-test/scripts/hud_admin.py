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


DIAGNOSTIC_REASONS = {"unavailable", "pending", "malformed", "cardinality", "bytes", "oversize_row", "source_partial"}
HOLDING_KINDS = {"zone", "widget", "tile", "portal"}
HOLDING_PHASES = {"active", "scheduled", "suspended", "orphaned", "cleanup_due", "unknown"}
EXPIRY_KINDS = {"content", "lease", "orphan_grace", "suspension_limit", "until_clear", "activity", "idle_reclaim", "unknown"}
WARNING_CODES = {
    "WIDGET_BUNDLE_NO_MANIFEST", "WIDGET_BUNDLE_INVALID_MANIFEST", "WIDGET_BUNDLE_INVALID_NAME",
    "WIDGET_BUNDLE_MISSING_SVG", "WIDGET_BUNDLE_SVG_PARSE_ERROR", "WIDGET_BINDING_UNRESOLVABLE",
    "WIDGET_BUNDLE_DUPLICATE_TYPE", "WIDGET_BUNDLE_UNRESOLVED_TOKEN", "WIDGET_BUNDLE_ROOT_UNREADABLE",
    "WIDGET_STARTUP_TAB_FAILED", "WIDGET_STARTUP_MISSING_TYPE", "WIDGET_STARTUP_TYPE_UNAVAILABLE",
    "WIDGET_STARTUP_INSTANCE_FAILED", "FONT_DIRECTORY_UNREADABLE", "FONT_FILE_UNREADABLE", "FONT_FAMILY_FALLBACK",
}


def unsigned(value, nullable=False):
    return (nullable and value is None) or (type(value) is int and 0 <= value <= 2**64 - 1)


def add_count(value, extra):
    # Keep the runtime's saturating u64 counter contract through local trims.
    return None if value is None else min(2**64 - 1, value + extra)


def bounded_string(value, size, nullable=False):
    return (nullable and value is None) or (isinstance(value, str) and len(value.encode("utf-8")) <= size)


def valid_holding(row):
    return (isinstance(row, dict) and set(row) == {"agent", "surface", "kind", "phase", "expires_in_ms", "expiry_kind", "count"}
            and bounded_string(row["agent"], 128) and bounded_string(row["surface"], 256)
            and row["kind"] in HOLDING_KINDS and row["surface"].startswith(row["kind"] + ":")
            and row["phase"] in HOLDING_PHASES and row["expiry_kind"] in EXPIRY_KINDS
            and unsigned(row["expires_in_ms"], True) and unsigned(row["count"]) and row["count"] > 0)


def valid_warning(row):
    return (isinstance(row, dict) and set(row) == {"source", "code", "subject", "fallback"}
            and row["source"] in ("widget_bundle", "font") and row["code"] in WARNING_CODES
            and bounded_string(row["subject"], 128, True)
            and (row["subject"] is None or re.fullmatch(r"root:\d+(?:/bundle:\d+)?|builtin:\d+|bundle:\d+|tab:\d+(?:/widget:\d+)?|fonts-dir|font-file:\d+|sans|mono|serif", row["subject"]))
            and row["fallback"] in (None, "IBM Plex Sans", "IBM Plex Mono", "DejaVu Serif"))


def unavailable_diagnostics(reason):
    return {"state": "unavailable", "holdings_state": "unavailable", "warnings_state": "unavailable",
            "holdings": [], "warnings": [], "omitted_agents": None, "omitted_records": None,
            "omitted_warning_events": None, "reasons": [reason]}


def project_diagnostics(status):
    """Whitelist metadata only after authenticated status succeeds; never copy raw status."""
    if "holdings" not in status or "warnings" not in status:
        return unavailable_diagnostics("unavailable")
    try:
        holdings, warnings = status["holdings"], status["warnings"]
        if (not isinstance(holdings, dict) or set(holdings) != {"state", "sources", "agents", "omitted_agents", "omitted_records", "omission_reasons"}
                or holdings["state"] not in ("complete", "partial", "unavailable")
                or not isinstance(holdings["sources"], dict) or set(holdings["sources"]) != {"scene", "portals"}
                or holdings["sources"]["scene"] not in ("available", "busy", "unavailable")
                or holdings["sources"]["portals"] not in ("available", "not_configured", "busy", "unavailable")
                or not isinstance(holdings["agents"], list) or len(holdings["agents"]) > 64
                or not unsigned(holdings["omitted_agents"], True) or not unsigned(holdings["omitted_records"], True)
                or not isinstance(holdings["omission_reasons"], list)
                or any(reason not in ("cardinality", "bytes", "oversize_id", "busy", "unavailable") for reason in holdings["omission_reasons"])):
            raise ValueError
        complete_sources = (holdings["sources"]["scene"] == "available"
                            and holdings["sources"]["portals"] in ("available", "not_configured"))
        readable_source = "available" in holdings["sources"].values()
        expected_state = ("complete" if complete_sources and holdings["omitted_records"] == 0
                          else "partial" if readable_source else "unavailable")
        if (holdings["state"] != expected_state
                or (not complete_sources and (holdings["omitted_records"] is not None or holdings["omitted_agents"] is not None))
                or (complete_sources and holdings["omitted_records"] is None)
                or (holdings["state"] == "complete" and (holdings["omission_reasons"] or holdings["omitted_agents"] != 0))):
            raise ValueError
        rows = []
        for agent in holdings["agents"]:
            if (not isinstance(agent, dict) or set(agent) != {"id", "surfaces", "omitted_records"}
                    or not bounded_string(agent["id"], 128) or not unsigned(agent["omitted_records"])
                    or not isinstance(agent["surfaces"], list)):
                raise ValueError
            for surface in agent["surfaces"]:
                if not isinstance(surface, dict) or "agent" in surface:
                    raise ValueError
                row = {"agent": agent["id"], **surface}
                if not valid_holding(row):
                    raise ValueError
                rows.append(row)
                if len(rows) > 256:
                    raise ValueError
        if (not isinstance(warnings, dict) or set(warnings) != {"state", "items", "truncated", "omitted_events", "omission_reasons"}
                or warnings["state"] not in ("pending", "complete", "unavailable")
                or type(warnings["truncated"]) is not bool or not unsigned(warnings["omitted_events"])
                or not isinstance(warnings["items"], list) or len(warnings["items"]) > 64
                or not all(valid_warning(row) for row in warnings["items"])
                or not isinstance(warnings["omission_reasons"], list)
                or any(reason not in ("cardinality", "bytes") for reason in warnings["omission_reasons"])):
            raise ValueError
        if (warnings["truncated"] != (warnings["omitted_events"] != 0)
                or bool(warnings["omission_reasons"]) != warnings["truncated"]
                or (warnings["state"] == "unavailable" and (warnings["items"] or warnings["truncated"]))
                or (holdings["state"] == "unavailable" and rows)
                or (holdings["state"] == "complete" and any(agent["omitted_records"] for agent in holdings["agents"]))):
            raise ValueError
        rows.sort(key=lambda row: (row["agent"], row["surface"], row["kind"], row["phase"], row["expiry_kind"], -1 if row["expires_in_ms"] is None else row["expires_in_ms"]))
        reasons = set()
        if holdings["state"] != "complete" or warnings["truncated"]:
            reasons.add("source_partial")
        if warnings["state"] == "pending":
            reasons.add("pending")
        if holdings["state"] == "unavailable" or warnings["state"] == "unavailable":
            reasons.add("unavailable")
        omitted = holdings["omitted_records"]
        if len(rows) > 8:
            if omitted is not None:
                omitted = add_count(omitted, sum(row["count"] for row in rows[8:]))
            reasons.add("cardinality")
        warning_items = warnings["items"]
        if len(warning_items) > 4:
            reasons.add("cardinality")
        omitted_agents = holdings["omitted_agents"]
        if omitted_agents is not None:
            omitted_agents = add_count(omitted_agents, len({row["agent"] for row in rows} - {row["agent"] for row in rows[:8]}))
        return {"state": "complete" if not reasons else ("unavailable" if holdings["state"] == warnings["state"] == "unavailable" else "partial"),
                "holdings_state": holdings["state"], "warnings_state": warnings["state"],
                "holdings": rows[:8], "warnings": warning_items[:4],
                "omitted_agents": omitted_agents, "omitted_records": omitted,
                "omitted_warning_events": None if warnings["state"] == "unavailable" else add_count(warnings["omitted_events"], max(0, len(warning_items) - 4)), "reasons": sorted(reasons)}
    except (KeyError, TypeError, ValueError, UnicodeError):
        return unavailable_diagnostics("malformed")


def diagnostic_result(result, diagnostics):
    if diagnostics is None:
        return result
    result["diagnostics"] = diagnostics
    # Actual default JSON encoding (including ensure_ascii escaping) plus newline.
    while len((json.dumps({"result": result}) + "\n").encode()) > 3072:
        reasons = set(diagnostics["reasons"])
        reasons.add("bytes")
        if diagnostics["holdings"]:
            dropped = diagnostics["holdings"].pop()
            single = {**diagnostics, "holdings": [dropped], "warnings": []}
            if len((json.dumps({"result": {**result, "diagnostics": single}}) + "\n").encode()) > 3072:
                reasons.add("oversize_row")
            if diagnostics["omitted_agents"] is not None and not any(row["agent"] == dropped["agent"] for row in diagnostics["holdings"]):
                diagnostics["omitted_agents"] = add_count(diagnostics["omitted_agents"], 1)
            if diagnostics["omitted_records"] is not None:
                diagnostics["omitted_records"] = add_count(diagnostics["omitted_records"], dropped["count"])
        elif diagnostics["warnings"]:
            diagnostics["warnings"].pop()
            if diagnostics["omitted_warning_events"] is not None:
                diagnostics["omitted_warning_events"] = add_count(diagnostics["omitted_warning_events"], 1)
        else:
            # The scalar envelope and fixed primary-result vocabulary fit alone.
            raise ValueError("diagnostic envelope cannot fit")
        diagnostics["reasons"] = sorted(reasons)
        diagnostics["state"] = "partial"
    return result


def valid_diagnostics(value):
    try:
        return (isinstance(value, dict) and set(value) == {"state", "holdings_state", "warnings_state", "holdings", "warnings", "omitted_agents", "omitted_records", "omitted_warning_events", "reasons"}
                and value["state"] in ("complete", "partial", "unavailable")
                and value["holdings_state"] in ("complete", "partial", "unavailable")
                and value["warnings_state"] in ("complete", "pending", "unavailable")
                and isinstance(value["holdings"], list) and len(value["holdings"]) <= 8 and all(valid_holding(row) for row in value["holdings"])
                and isinstance(value["warnings"], list) and len(value["warnings"]) <= 4 and all(valid_warning(row) for row in value["warnings"])
                and all(unsigned(value[key], True) for key in ("omitted_agents", "omitted_records", "omitted_warning_events"))
                and isinstance(value["reasons"], list) and len(value["reasons"]) <= len(DIAGNOSTIC_REASONS)
                and all(reason in DIAGNOSTIC_REASONS for reason in value["reasons"])
                and (value["state"] != "complete" or (value["holdings_state"] == value["warnings_state"] == "complete"
                     and not value["reasons"] and all(value[key] == 0 for key in ("omitted_agents", "omitted_records", "omitted_warning_events"))))
                and (value["warnings_state"] != "unavailable" or (not value["warnings"] and value["omitted_warning_events"] is None))
                and (value["holdings_state"] != "unavailable" or (not value["holdings"] and value["omitted_records"] is None and value["omitted_agents"] is None)))
    except (KeyError, TypeError, ValueError, UnicodeError):
        return False


def doctor_probe(host: str | None, progress) -> dict:
    """Read-only probe in an owned child; no Python signal deadline assumption."""
    layer = "endpoint"
    action = DOCTOR_ACTIONS[layer]
    diagnostics = None
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
        diagnostics = project_diagnostics(status)
        layer = "firewall"
        action = DOCTOR_ACTIONS[layer]
        firewall = status.get("tailnet_inbound", {})
        state = firewall.get("state") if isinstance(firewall, dict) else None
        if state != "allowed":
            reason = state if state in ("blocked", "unknown", "not_applicable") else "unknown"
            return diagnostic_result({"ok": False, "layer": layer, "reason": reason, "action": action}, diagnostics)
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
            return diagnostic_result({"ok": False, "layer": "build", "reason": "sha_mismatch",
                    "action": "Ask the owner to update to the resolved dev commit; no update was performed."}, diagnostics)
        return diagnostic_result({"ok": True, "layer": "dev_commit", "sha": latest}, diagnostics)
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
        return diagnostic_result({"ok": False, "layer": layer, "reason": reason, "action": action}, diagnostics)


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
                    candidate = message["result"]
                    if (set(candidate) - {"ok", "layer", "reason", "action", "sha", "diagnostics"} or type(candidate.get("ok")) is not bool or
                            candidate.get("action") not in (*DOCTOR_ACTIONS.values(), DOCTOR_HOST_ACTION,
                                                        "Ask the owner to update to the resolved dev commit; no update was performed.", None)):
                        raise ValueError("invalid probe result")
                    if "diagnostics" in candidate and (not valid_diagnostics(candidate["diagnostics"]) or len((json.dumps({"result": candidate}) + "\n").encode()) > 3072):
                        raise ValueError("invalid probe diagnostics")
                    # Do not let an unvalidated child's reason masquerade as a
                    # supervisor-owned cleanup/deadline failure in the handler.
                    result = candidate
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
