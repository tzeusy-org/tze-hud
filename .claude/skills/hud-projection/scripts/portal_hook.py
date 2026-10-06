#!/usr/bin/env python3
"""Silent, opt-in Claude Code main-session mirroring; see claude-code-hooks.md.

Only normalized safe content crosses the supervised child's stdin. Metadata
locks never cover network I/O. Successful acknowledgments permit deduplication;
an HTTP timeout is not proof that the server did not apply the request.
"""

import contextlib
import hashlib
import itertools
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unicodedata
import uuid

PROFILE = "claude-main-v1"
INPUT_LIMIT = 65536
STATE_LIMIT = 32768
HTTP_TIMEOUT = 0.350
DELIVERY_BUDGET = 1.0
END_BUDGET = 1.2
HOLD_MS = 600000
EVENTS = {
    "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse",
    "PostToolUseFailure", "Stop", "SessionEnd",
}
TOOL_LABELS = {
    name: name for name in (
        "Bash", "Read", "Write", "Edit", "Grep", "Glob", "WebFetch",
        "WebSearch", "Agent", "Task", "TodoWrite", "AskUserQuestion",
    )
}
CREDENTIAL = re.compile(
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----|\b(?:sk-(?:ant-)?|gh[pousr]_)[A-Za-z0-9_-]{16,}"
    r"|\bAKIA[A-Z0-9]{16}\b|\bBearer\s+[A-Za-z0-9._-]{16,}"
    r"|(?:api[_-]?key|psk|password|secret|token)\s*[:=]\s*['\"]?\S{8,}",
    re.IGNORECASE,
)
ESCAPES = re.compile(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))")
PROMPT_ID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


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


def normalize_event(payload):
    """Discard arbitrary payload fields before state, spawning or networking."""
    if not isinstance(payload, dict) or payload.get("agent_id"):
        return None
    event = payload.get("hook_event_name")
    session = payload.get("session_id")
    if event not in EVENTS or not isinstance(session, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,96}", session):
        return None
    result = {"event": event, "session": session}
    if event == "SessionStart":
        if payload.get("source") not in {"startup", "resume", "clear"}:
            return None
        return result
    if event == "SessionEnd":
        return result
    prompt = payload.get("prompt_id")
    try:
        if not isinstance(prompt, str) or str(uuid.UUID(prompt)) != prompt.lower():
            return None
    except ValueError:
        return None
    result["prompt"] = prompt.lower()
    if event in {"PreToolUse", "PostToolUse", "PostToolUseFailure"}:
        tool_id = payload.get("tool_use_id")
        if not isinstance(tool_id, str) or not tool_id or len(tool_id.encode("utf-8")) > 256:
            return None
        name = payload.get("tool_name")
        if not isinstance(name, str):
            return None
        label = TOOL_LABELS.get(name, "MCP tool" if name.startswith("mcp__") else "Tool")
        stage = "running" if event == "PreToolUse" else "failed" if event == "PostToolUseFailure" else "completed"
        result.update(key="cc-tool-" + digest(prompt + "\0" + tool_id), stage=1 if stage == "running" else 2, content=f"{label}: {stage}")
    elif event == "Stop":
        result.update(key="cc-final-" + digest(prompt), content=final_text(payload.get("last_assistant_message")))
    return result


def private_file(path):
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        os.close(fd)
        raise OSError("unsafe hook metadata")
    return fd


@contextlib.contextmanager
def lock(path, budget):
    import fcntl
    fd = private_file(path)
    deadline = time.monotonic() + budget
    try:
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("hook metadata busy")
                time.sleep(min(0.001, max(0, deadline - time.monotonic())))
        yield
    finally:
        os.close(fd)


def read_state(path, default=None):
    if not path.exists():
        return default
    fd = private_file(path)
    with os.fdopen(fd, "rb") as stream:
        raw = stream.read(STATE_LIMIT + 1)
    if len(raw) > STATE_LIMIT:
        raise ValueError("oversize hook state")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("invalid hook state")
    return value


def write_state(path, value):
    raw = json.dumps(value, separators=(",", ":")).encode()
    if len(raw) > STATE_LIMIT:
        raise ValueError("oversize hook state")
    if path.is_symlink():
        raise OSError("unsafe hook metadata")
    fd, temporary = tempfile.mkstemp(prefix=".write-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def validate_marker(marker):
    closed = marker.get("closed")
    updated = marker.get("updated")
    prompt = marker.get("prompt")
    if (
        set(marker) != {"generation", "prompt", "closed", "ended", "updated"}
        or not isinstance(marker["generation"], str)
        or not re.fullmatch(r"[0-9a-f]{32}", marker["generation"])
        or (prompt is not None and (not isinstance(prompt, str) or not PROMPT_ID.fullmatch(prompt)))
        or not isinstance(closed, list) or len(closed) > 16
        or any(not isinstance(p, str) or not PROMPT_ID.fullmatch(p) for p in closed)
        or not isinstance(marker["ended"], bool)
        or not isinstance(updated, (int, float)) or not math.isfinite(updated)
    ):
        raise ValueError("invalid marker")


def validate_delivery(state):
    tools = state.get("tools")
    finals = state.get("finals")
    prompt = state.get("prompt")
    if (
        not set(state).issubset({"generation", "prompt", "tools", "finals"})
        or not isinstance(state.get("generation"), str)
        or not re.fullmatch(r"[0-9a-f]{32}", state["generation"])
        or (prompt is not None and (not isinstance(prompt, str) or not PROMPT_ID.fullmatch(prompt)))
        or not isinstance(tools, dict) or len(tools) > 256
        or any(not re.fullmatch(r"cc-tool-[0-9a-f]{64}", k) or type(v) is not int or v not in (1, 2) for k, v in tools.items())
        or not isinstance(finals, dict) or len(finals) > 16
        or any(not PROMPT_ID.fullmatch(k) or not isinstance(v, str) or not re.fullmatch(r"[0-9a-f]{64}", v) for k, v in finals.items())
    ):
        raise ValueError("invalid delivery state")


class Metadata:
    def __init__(self, session):
        # Hostname resolution does not read credentials. Only the network child
        # invokes the existing paired-PSK resolver through portal_client.
        import portal_client
        hostname = portal_client.hud_env.hostname()
        root = Path.home() / ".cache" / "tze-hud" / "claude-hooks"
        for part in [root.parent.parent, root.parent, root]:
            if part.is_symlink():
                raise OSError("unsafe hook cache")
            part.mkdir(mode=0o700, exist_ok=True)
        info = root.stat()
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise OSError("unsafe hook cache")
        self.root = root
        self.directory = root / digest(hostname + "\0" + session + "\0" + PROFILE)
        if self.directory.is_symlink():
            raise OSError("unsafe hook cache")
        self.directory.mkdir(mode=0o700, exist_ok=True)
        info = self.directory.stat()
        if info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise OSError("unsafe hook cache")
        self.marker = self.directory / "marker.json"
        self.delivery = self.directory / "delivery.json"
        self.disabled = self.directory / "disabled.json"

    def disable(self):
        # This independent atomic sentinel does not need a stuck marker lock.
        write_state(self.disabled, {"disabled": True})

    def current(self):
        if self.disabled.exists():
            return None
        try:
            marker = read_state(self.marker)
            if marker:
                validate_marker(marker)
            return marker
        except (OSError, ValueError):
            self.disable()
            raise

    def mark(self, event):
        with lock(self.directory / "marker.lock", 0.005):
            marker = read_state(self.marker, {"closed": []})
            if self.marker.exists():
                validate_marker(marker)
            kind = event["event"]
            if kind == "SessionStart":
                marker = {"generation": uuid.uuid4().hex, "prompt": None, "closed": marker.get("closed", [])[-16:], "ended": False}
            else:
                if self.disabled.exists() or not marker.get("generation") or marker.get("ended"):
                    return None
                if kind == "UserPromptSubmit":
                    if event["prompt"] in marker["closed"]:
                        return None
                    marker["prompt"] = event["prompt"]
                elif kind == "Stop":
                    if marker.get("prompt") != event["prompt"]:
                        return None
                    marker["closed"] = list(dict.fromkeys(marker["closed"] + [event["prompt"]]))[-16:]
                elif kind == "SessionEnd":
                    marker["ended"] = True
            marker["updated"] = time.time()
            write_state(self.marker, marker)
            if kind == "SessionStart":
                self.disabled.unlink(missing_ok=True)
            return marker

    def purge(self):
        # Bounded, profile-owned closed metadata only. Keep lock files so a late
        # process cannot acquire a different inode for the same lock domain.
        for entry in itertools.islice(self.root.iterdir(), 16):
            if entry == self.directory or entry.is_symlink() or not re.fullmatch(r"[0-9a-f]{64}", entry.name) or not entry.is_dir():
                continue
            try:
                with lock(entry / "marker.lock", 0), lock(entry / "delivery.lock", 0):
                    marker = read_state(entry / "marker.json")
                    if marker:
                        validate_marker(marker)
                    if marker and marker.get("ended") and time.time() - marker.get("updated", time.time()) >= 86400:
                        for name in ["marker.json", "delivery.json", "disabled.json"]:
                            path = entry / name
                            if path.is_symlink():
                                raise OSError("unsafe hook metadata")
                            path.unlink(missing_ok=True)
            except (OSError, ValueError, TimeoutError):
                continue


def applicable(marker, event):
    if not marker or marker.get("generation") != event["generation"]:
        return False
    if event["event"] == "SessionEnd":
        return bool(marker.get("ended"))
    if marker.get("ended") or marker.get("prompt") != event.get("prompt"):
        return False
    closed = event["prompt"] in marker["closed"]
    return closed if event["event"] == "Stop" else not closed


def delivery(event, deadline):
    import portal_client
    metadata = Metadata(event["session"])
    try:
        state = read_state(metadata.delivery, {})
        if state:
            validate_delivery(state)
    except (OSError, ValueError):
        metadata.disable()
        raise
    if state.get("generation") != event["generation"]:
        state = {"generation": event["generation"], "tools": {}, "finals": {}}
    if not applicable(metadata.current(), event):
        return
    kind = event["event"]
    if kind.startswith("Pre") or kind.startswith("Post"):
        if state.get("prompt") != event["prompt"]:
            state["tools"] = {}
            state["prompt"] = event["prompt"]
        prior = state["tools"].get(event["key"], 0)
        if prior >= event["stage"] or (event["key"] not in state["tools"] and len(state["tools"]) >= 256):
            return
    # Preserve the distinction between absent text and any literal reply.
    final_hash = digest(json.dumps(event.get("content"), ensure_ascii=False))
    if kind == "Stop" and state["finals"].get(event["prompt"]) == final_hash:
        return

    def call(name, arguments):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("hook delivery deadline")
        try:
            portal_client.call_tool(name, arguments, timeout=min(HTTP_TIMEOUT, remaining))
        except portal_client.ToolError as error:
            if name != "hud_clear" or error.code != "NOT_HELD":
                raise

    surface = {"surface": "portal:" + event["session"]}

    def catch_up():
        marker = metadata.current()
        if not marker or marker["generation"] != event["generation"]:
            return
        if marker.get("ended"):
            if metadata.current() == marker:
                call("hud_clear", surface)
        elif marker.get("prompt") and marker["prompt"] not in marker["closed"]:
            if metadata.current() == marker:
                call("hud_publish", dict(surface, status="active"))
                if metadata.current() == marker:
                    call("hud_hold", dict(surface, ttl_ms=HOLD_MS))
                    metadata.current()  # revalidate the acknowledged hold; no second catch-up

    if not applicable(metadata.current(), event):
        return
    if kind == "SessionEnd":
        call("hud_clear", surface)
        return
    arguments = dict(surface, status="attached" if kind == "Stop" else "active")
    if event.get("content"):
        arguments.update(content=event["content"], key=event["key"])
    call("hud_publish", arguments)
    if not applicable(metadata.current(), event):
        catch_up()
        return
    if "stage" in event:
        state["tools"][event["key"]] = event["stage"]
    elif kind == "Stop":
        state["finals"][event["prompt"]] = final_hash
        state["finals"] = dict(list(state["finals"].items())[-16:])
    try:
        write_state(metadata.delivery, state)
    except (OSError, ValueError):
        metadata.disable()
        raise
    if applicable(metadata.current(), event):
        call("hud_hold", dict(surface, ttl_ms=HOLD_MS))
        if not applicable(metadata.current(), event):
            catch_up()
    else:
        catch_up()


def supervise(event):
    metadata = Metadata(event["session"])
    budget = END_BUDGET if event["event"] == "SessionEnd" else DELIVERY_BUDGET
    deadline = time.monotonic() + budget
    admission = max(0, budget - 0.050) if event["event"] == "SessionEnd" else 0.050
    with lock(metadata.directory / "delivery.lock", admission):
        if not applicable(metadata.current(), event):
            return
        job = {"event": event, "deadline": deadline}
        process = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--deliver"],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            process.communicate(json.dumps(job).encode(), timeout=max(0.001, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()


def handle(event):
    metadata = Metadata(event["session"])
    try:
        metadata.purge()
        if event["event"] in {"SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"}:
            marker = metadata.mark(event)
        else:
            marker = metadata.current()
        if not marker or event["event"] == "SessionStart":
            return
        event = dict(event, generation=marker["generation"])
    except (OSError, ValueError, TimeoutError, KeyError, TypeError):
        metadata.disable()
        return
    if event["event"] == "UserPromptSubmit":
        # Only this content-free job leaves the synchronous prompt marker. Its
        # supervisor remains responsible for killing and reaping its HTTP child.
        process = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--supervise"],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            process.stdin.write(json.dumps(event).encode())
            process.stdin.close()
        except OSError:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
    else:
        supervise(event)


def main():
    # Never supply hook output/context/control to Claude, including errors from
    # the existing client. Native Windows hook-host locking is not supported.
    with open(os.devnull, "w") as sink, contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        try:
            if os.name != "posix":
                return
            raw = sys.stdin.buffer.read(INPUT_LIMIT + 1)
            if len(raw) > INPUT_LIMIT:
                return
            payload = json.loads(raw)
            if sys.argv[1:] == ["--deliver"]:
                delivery(payload["event"], payload["deadline"])
            elif sys.argv[1:] == ["--supervise"]:
                supervise(payload)
            elif not sys.argv[1:]:
                event = normalize_event(payload)
                if event:
                    handle(event)
        except (Exception, SystemExit):
            pass


if __name__ == "__main__":
    main()
