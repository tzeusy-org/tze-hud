#!/usr/bin/env python3
"""Silent event-driven five-hour gauge; see references/claude-code-usage.md.

One delivery descriptor owns live sends. Each local generation gets at most
one uncertain attempt; later distinct generations can proceed after release.
Client termination cannot fence old server work or guarantee current pixels.
"""

import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import stat
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "user-test" / "scripts"))
import hud_env
import publish_widget_batch

INPUT_LIMIT = 65536
STATE_LIMIT = 2048
BUDGET = 1.0
MAX_GENERATION = 2**53 - 1


def metric(payload):
    """Only the documented numeric five-hour pair is eligible; no fallback."""
    try:
        window = payload["rate_limits"]["five_hour"]
        pct, reset = window["used_percentage"], window["resets_at"]
        if (type(pct) not in (int, float) or not math.isfinite(pct) or not 0 <= pct <= 100
                or type(reset) not in (int, float) or not math.isfinite(reset)
                or not time.time() < reset <= 253402214400):
            return None
        time.localtime(reset)  # Reject an unrepresentable local timestamp.
        return {"window": reset, "pct": math.floor(pct), "level": pct / 100}
    except (KeyError, TypeError, ValueError, OverflowError, OSError):
        return None


def step(value):
    return None if value is None else [value["window"], value["pct"]]


def private_file(path):
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_nlink != 1:
        os.close(fd)
        raise OSError("unsafe usage state")
    return fd


@contextlib.contextmanager
def lock(path, *, wait=False):
    import fcntl
    fd = private_file(path)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | (0 if wait else fcntl.LOCK_NB))
        yield
    finally:
        os.close(fd)


class State:
    def __init__(self, endpoint, widget, identity):
        root = Path.home() / ".cache" / "tze-hud" / "claude-usage"
        for directory in (root.parent.parent, root.parent, root):
            if directory.is_symlink():
                raise OSError("unsafe usage directory")
            directory.mkdir(mode=0o700, exist_ok=True)
        self.directory = root / hashlib.sha256((endpoint + "\0" + widget + "\0" + identity).encode()).hexdigest()
        if self.directory.is_symlink():
            raise OSError("unsafe usage directory")
        self.directory.mkdir(mode=0o700, exist_ok=True)
        for directory in (root, self.directory):
            info = directory.stat()
            if info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise OSError("unsafe usage directory")
        self.path = self.directory / "state.json"

    def read(self):
        if not os.path.lexists(self.path):
            return {"version": 2, "generation": 0, "desired": None, "ack": None, "attempted": None}
        fd = private_file(self.path)
        with os.fdopen(fd, "rb") as stream:
            raw = stream.read(STATE_LIMIT + 1)
        if len(raw) > STATE_LIMIT:
            raise ValueError("oversized usage state")
        state = json.loads(raw)
        if (not isinstance(state, dict) or type(state.get("version")) is not int
                or state["version"] not in (1, 2)
                or set(state) != {"version", "generation", "desired", "ack",
                                  "inflight" if state["version"] == 1 else "attempted"}
                or type(state["generation"]) is not int
                or not 0 <= state["generation"] < MAX_GENERATION):
            raise ValueError("invalid usage state")
        desired = state["desired"]
        if desired is not None:
            if (not isinstance(desired, dict) or set(desired) != {"window", "pct", "level"}
                    or type(desired["window"]) not in (int, float) or not math.isfinite(desired["window"])
                    or not 0 < desired["window"] <= 253402214400
                    or type(desired["pct"]) is not int or not 0 <= desired["pct"] <= 100
                    or type(desired["level"]) not in (int, float) or not math.isfinite(desired["level"])
                    or not 0 <= desired["level"] <= 1):
                raise ValueError("invalid usage desire")
        if state["version"] == 1:
            for name in ("ack", "inflight"):
                value = state[name]
                if (name == "inflight" and value is None) or (name == "ack" and value == "unknown"):
                    continue
                validate_key(value)
            # Old claims lack their own generation. Bind a matching old
            # inflight key conservatively to the current generation; a
            # different desired key is already a distinct pending transition.
            attempted = None if state["inflight"] is None else {
                "generation": state["generation"], "key": state["inflight"]}
            current_ack = attempted is None and state["ack"] == claim_key(desired)
            ack = None if state["ack"] == "unknown" else {
                "generation": state["generation"] if current_ack else None,
                "key": state["ack"]}
            return {"version": 2, "generation": state["generation"], "desired": desired,
                    "ack": ack, "attempted": attempted}
        for name in ("ack", "attempted"):
            claim = state[name]
            if claim is None:
                continue
            if not isinstance(claim, dict) or set(claim) != {"generation", "key"}:
                raise ValueError("invalid usage claim")
            generation = claim["generation"]
            if not (name == "ack" and generation is None):
                if type(generation) is not int or not 0 <= generation <= state["generation"]:
                    raise ValueError("invalid claim generation")
            validate_key(claim["key"])
        return state

    def write(self, state):
        raw = json.dumps(state, separators=(",", ":"), allow_nan=False).encode()
        if len(raw) > STATE_LIMIT:
            raise ValueError("oversized usage state")
        # Caller holds state.lock. One fixed, validated staging inode bounds
        # crash leftovers; a killed write cannot accumulate fresh temp names.
        temporary = self.directory / "state.pending"
        fd = private_file(temporary)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.truncate(0)
                stream.write(raw)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def claim_key(desired):
    # A clear has its own key, separate from an unknown ACK or absent attempt.
    return [0, -1] if desired is None else step(desired)


def validate_key(value):
    if (not isinstance(value, list) or len(value) != 2
            or type(value[0]) not in (int, float) or not math.isfinite(value[0])
            or not 0 <= value[0] <= 253402214400 or type(value[1]) is not int
            or not (value == [0, -1] or (value[0] > 0 and 0 <= value[1] <= 100))):
        raise ValueError("invalid usage claim key")


def deliver(metadata, endpoint, token, widget, deadline):
    # Initial request and at most one newest-desire catch-up, never a retry.
    with lock(metadata.directory / "delivery.lock"):
        for _ in range(2):
            with lock(metadata.directory / "state.lock"):
                state = metadata.read()
                desired = state["desired"]
                claim = {"generation": state["generation"], "key": claim_key(desired)}
                if claim in (state["attempted"], state["ack"]) or time.monotonic() >= deadline:
                    return
                state["attempted"] = claim
                metadata.write(state)
            arguments = {"surface": "widget:" + widget}
            if desired is None:
                tool = "hud_clear"
            else:
                tool = "hud_publish"
                arguments["params"] = {
                    "level": desired["level"], "label": "5h usage",
                    "readout": f'{desired["pct"]}% · resets {time.strftime("%H:%M", time.localtime(desired["window"]))}',
                    "severity": "warning" if desired["pct"] >= 80 else "info",
                }
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return
            result = publish_widget_batch.rpc_call(endpoint["url"], token, tool, arguments, 1,
                                                  timeout=remaining, response_limit=4096)
            # Even rejection is conservatively uncertain unless explicitly safe.
            cleared_absent = desired is None and result.get("error", {}).get("code") == "NOT_HELD"
            if result.get("result", {}).get("ok") is not True and not cleared_absent:
                return
            # A successful ACK is known delivery, not a transport uncertainty.
            # Retain delivery ownership while a short metadata writer finishes;
            # main's original absolute timer bounds this blocking local lock.
            with lock(metadata.directory / "state.lock", wait=True):
                state = metadata.read()
                if state["attempted"] != claim:
                    return
                state["ack"] = claim
                metadata.write(state)  # Never replace concurrent desired/generation.


def handle(payload, deadline):
    widget = os.environ.get("HUD_USAGE_WIDGET", "usage")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", widget):
        return
    endpoint = hud_env.adapter_endpoint()
    token = hud_env.adapter_key(endpoint)
    # The credential is used only by this process/client; only a domain hash is
    # retained, separating rotated/selected identities without persisting keys.
    metadata = State(endpoint["url"], widget, token)
    desired = metric(payload)
    with lock(metadata.directory / "state.lock"):
        state = metadata.read()
        if step(state["desired"]) != step(desired):
            if state["generation"] >= MAX_GENERATION - 1:
                return
            state["generation"] += 1
            state["desired"] = desired
            metadata.write(state)
    deliver(metadata, endpoint, token, widget, deadline)


def main():
    # Detached POSIX host only. The default-action timer also bounds DNS,
    # trickling HTTP bodies, stdin, admission and cleanup with one total budget.
    if os.name != "posix":
        return
    with open(os.devnull, "w") as sink, contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        try:
            if sys.argv[1:]:
                return
            deadline = time.monotonic() + BUDGET
            signal.signal(signal.SIGALRM, signal.SIG_DFL)
            signal.setitimer(signal.ITIMER_REAL, BUDGET)
            raw = sys.stdin.buffer.read(INPUT_LIMIT + 1)
            if len(raw) > INPUT_LIMIT:
                return
            try:
                payload = json.loads(raw)
            except (ValueError, UnicodeError):
                payload = None
            handle(payload, deadline)
        except (Exception, SystemExit):
            pass
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)


if __name__ == "__main__":
    main()
