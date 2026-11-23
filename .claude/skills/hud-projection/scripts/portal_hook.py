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


def main():
    if not sys.platform.startswith("linux") or sys.argv[1:]:
        return
    try:
        sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
        import hud_hook_socket

        raw = sys.stdin.buffer.read(INPUT_LIMIT + 1)
        if len(raw) <= INPUT_LIMIT:
            event = normalize_event(json.loads(raw), hud_hook_socket)
            if event is not None:
                hud_hook_socket.send_event(event)
    except (ImportError, OSError, ValueError, TypeError, RecursionError):
        pass


if __name__ == "__main__":
    main()
