#!/usr/bin/env python3
"""Normalize Claude hook events and send one silent, local packet."""

import hashlib
import json
from pathlib import Path
import re
import sys
import unicodedata

INPUT_LIMIT = 65536

CREDENTIAL = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----|\b(?:sk-(?:ant-)?|gh[pousr]_)[A-Za-z0-9_-]{16,}"
    r"|\bAKIA[A-Z0-9]{16}\b|\bBearer\s+[A-Za-z0-9._-]{16,}"
    r"|(?:api[_-]?key|psk|password|secret|token)\s*[:=]\s*['\"]?\S{8,}",
    re.IGNORECASE,
)
ESCAPES = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")


def digest(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def final_text(value):
    if not isinstance(value, str) or CREDENTIAL.search(value):
        return None
    text = ESCAPES.sub("", value)
    text = "".join(c for c in text if c in "\n\t" or not unicodedata.category(c).startswith("C"))
    if CREDENTIAL.search(text):
        return None
    raw = text.encode("utf-8")
    if len(raw) > 8192:
        marker = "\n[truncated]"
        text = raw[:8192 - len(marker.encode())].decode("utf-8", errors="ignore") + marker
    return text or None


def normalize_event(payload, transport):
    """Discard transcripts, raw tool inputs, messages and other payload fields."""
    if not isinstance(payload, dict):
        return None
    kind = payload.get("hook_event_name")
    if not isinstance(kind, str):
        return None
    event = {"event": kind, "session": payload.get("session_id")}
    if kind == "SessionStart":
        source = payload.get("source")
        if source not in ("startup", "resume", "clear"):
            return None
        event["clear"] = source == "clear"
    elif kind == "SessionEnd":
        event["clear"] = payload.get("reason") == "clear"
    else:
        prompt = payload.get("prompt_id")
        if isinstance(prompt, str):
            event["prompt"] = prompt.lower()
        agent = payload.get("agent_id")
        if agent:
            if kind == "Stop" or not isinstance(agent, str) or len(agent) > 256:
                return None
            if kind in transport.TOOL_EVENTS or kind == "Notification":
                event["agent"] = digest(agent)
        if kind in transport.TOOL_EVENTS:
            tool = payload.get("tool_name")
            tool_id = payload.get("tool_use_id")
            if not isinstance(tool, str) or not isinstance(tool_id, str) or not tool_id:
                return None
            if len(tool_id.encode("utf-8")) > 256 or "prompt" not in event:
                return None
            event["key"] = "cc-tool-" + digest(event["prompt"] + "\0" + tool_id)
            event["tool"] = tool if tool in transport.TOOL_LABELS else (
                "MCP tool" if tool.startswith("mcp__") else "Tool"
            )
        elif kind == "Stop":
            event["content"] = final_text(payload.get("last_assistant_message"))
        elif kind == "Notification":
            event["notice"] = payload.get("notification_type")
    return event if transport.valid_event(event) else None


def hook_transport():
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
    import hud_hook_socket

    return hud_hook_socket


def request_card(kind, title, body, actions, *, timeout_ms, ttl_ms=None):
    """Request a local hook card and return its terminal IPC result."""
    import socket
    import time

    error = {"end": "error"}
    if sys.platform != "linux" or type(timeout_ms) is not int or timeout_ms <= 0:
        return error
    try:
        transport = hook_transport()
        request = {"v": 1, "op": "card", "kind": kind, "title": title,
                   "body": body, "actions": actions}
        if ttl_ms is not None:
            request["ttl_ms"] = ttl_ms
        if not transport.valid_card(request):
            return error
        start = time.monotonic()
        deadline = start + timeout_ms / 1_000
        with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as peer:
            if not transport.send_packet(peer, transport.packet_bytes(request)):
                return error
            peer.settimeout(max(0.001, min(start + 1, deadline) - time.monotonic()))
            reply = json.loads(peer.recv(transport.MAX_PACKET))
            if (not isinstance(reply, dict) or reply.get("ok") is not True
                    or type(reply.get("expires_in_ms")) is not int or reply["expires_in_ms"] < 0):
                return error
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return {"end": "expired"}
            peer.settimeout(remaining)
            try:
                result = json.loads(peer.recv(transport.MAX_PACKET))
            except TimeoutError:
                return {"end": "expired"}
        if not isinstance(result, dict):
            return error
        end = result.get("end")
        if end == "answer":
            index = result.get("index")
            if type(index) is int and 0 <= index < len(actions):
                return {"end": end, "index": index}
        elif end in ("dismissed", "expired", "cancelled", "superseded", "error"):
            return {"end": end}
    except (ImportError, OSError, ValueError, TypeError, OverflowError, RecursionError):
        pass
    return error


def main():
    if not sys.platform.startswith("linux") or sys.argv[1:]:
        return
    try:
        hud_hook_socket = hook_transport()
        raw = sys.stdin.buffer.read(INPUT_LIMIT + 1)
        if len(raw) <= INPUT_LIMIT:
            event = normalize_event(json.loads(raw), hud_hook_socket)
            if event is not None:
                hud_hook_socket.send_event(event)
    except (ImportError, OSError, ValueError, TypeError, RecursionError):
        pass


if __name__ == "__main__":
    main()
