#!/usr/bin/env python3
"""Bounded Linux/WSL stdio client for the HUD's existing authenticated HTTP MCP.

Initialize has a 3s absolute startup deadline; established messages retain 60s.
Each owns one of four child slots until reaped. Cancellation is local best effort: a POST
already accepted remotely is never replayed or rolled back. Native macOS and
Windows client lifetime primitives are unsupported; the Windows HUD is unchanged.

The runtime ignores `notifications/*`; this adapter consumes
`notifications/initialized` locally and does not forward it, so the runtime
must not require it. A session is ready once that notice follows a successful
initialize, and no HUD request can undo that.

Each adapter is one HUD session: initialize sends the working folder's name
as the session label (`_meta`, outside model context) and keeps the returned
`Mcp-Session-Id` for later calls, so this client owns only its own content and
input. A clean stop (stdin EOF, SIGTERM, SIGINT or SIGHUP) sends `DELETE /mcp`
with a 1 s deadline, even with calls in flight, releasing the session. A 404
(the HUD restarted or released the session) re-initializes once, single-flight
across concurrent calls and replaying the client's initialize, and retries
each refused call once; the client never sees a second initialize result.
A stopping adapter never re-initializes, and once a session has existed
no call goes out without one: after a failed re-initialize the next call
waits on a fresh one. The live session id is also kept in
`~/.config/tze-hud/sessions/<client-pid>.json` (mode 0600, removed on a
clean stop) so a helper the client starts, such as portal_client.py's
background waiter, joins this session instead of running unscoped.
"""

from __future__ import annotations

import ctypes
import http.client
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

# runpy's project-root launcher does not add this script directory to sys.path.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import hud_env

REQUEST_SECONDS = 60.0
INITIALIZE_SECONDS = 3.0
INITIALIZE_TIMEOUT = "HUD initialize DNS/TCP/startup deadline exceeded (phase unavailable)"
TERM_SECONDS = 1.0
END_SECONDS = 1.0
LABEL_META_KEY = "tze-hud/label"
REINIT_ID = "tze-hud-reinitialize"
SESSION_HEADER = "Mcp-Session-Id"
KILL_SECONDS = 1.0
OUTPUT_STALL_SECONDS = 5.0
MAX_WORKERS = 4
MAX_REQUEST = 60 * 1024  # Leave room for headers under the runtime's whole 64KiB cap.
MAX_RESPONSE = 1024 * 1024
MAX_OUTPUT = 4 * MAX_RESPONSE
CHUNK = 16 * 1024


def failure(request_id, message="HUD connection failed"):
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": -32603, "message": message}}


def invalid_number(_):
    raise ValueError("nonfinite JSON number")


def valid_id(value):
    return type(value) in (str, int)


def worker(parent: int, deadline: float) -> int:
    """Arm death/deadline before IPC, key loading or network; no worker descendants."""
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
        return 1
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return 1
    signal.signal(signal.SIGALRM, signal.SIG_DFL)
    signal.setitimer(signal.ITIMER_REAL, remaining)
    request_id = None
    notification = False
    failed = False
    initializing = False
    opened = None
    phase = "protocol"
    try:
        data = sys.stdin.buffer.read(MAX_REQUEST + 4097)
        job = json.loads(data, parse_constant=invalid_number)
        json.dumps(job, allow_nan=False)  # Also reject exponent overflow before key loading.
        request = job["request"]
        if not isinstance(request, dict) or request.get("jsonrpc") != "2.0" or not isinstance(request.get("method"), str):
            raise ValueError("invalid worker request")
        request_id = request.get("id")
        notification = "id" not in request
        if not notification and not valid_id(request_id):
            request_id = None
            raise ValueError("invalid worker request ID")
        initializing = request["method"] == "initialize"
        ending = job.get("end") is True
        session = job.get("session")
        if session is not None and not (isinstance(session, str) and session.isascii()
                                        and session.isprintable() and " " not in session and session):
            raise ValueError("invalid session id")
        phase = "auth"
        key = hud_env.adapter_key(job["endpoint"])
        phase = "protocol"
        url = urlsplit(job["endpoint"]["url"])
        body = json.dumps(request, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
        headers = {"Authorization": f"Bearer {key}", "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream", "Connection": "close"}
        if job["protocol"]:
            headers["MCP-Protocol-Version"] = job["protocol"]
        if session is not None:
            headers[SESSION_HEADER] = session
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=max(0.001, deadline - time.monotonic()))
        try:
            phase = "TCP"
            connection.connect()
            phase = "response/read"
            connection.request("DELETE" if ending else "POST", "/mcp",
                               body=None if ending else body, headers=headers)
            response = connection.getresponse()
            if ending:
                response.read(MAX_RESPONSE)
                sys.stdout.buffer.write(b"null")
                sys.stdout.buffer.flush()
                return 0
            if response.status == 404 and session is not None:
                # The session is unknown; the HUD refused before dispatch.
                sys.stdout.buffer.write(json.dumps({"unknown_session": True}).encode())
                sys.stdout.buffer.flush()
                return 0
            opened = response.getheader(SESSION_HEADER) if initializing else None
            if response.status != 200 and not (notification and response.status == 202):
                phase = "auth" if response.status in (401, 403) else "HTTP"
                raise ValueError("HTTP refusal or redirect")
            raw = response.read(MAX_RESPONSE + 1)
        finally:
            connection.close()
        phase = "protocol"
        if len(raw) > MAX_RESPONSE or key.encode() in raw:
            raise ValueError("unsafe or oversized response")
        result = None if notification else json.loads(raw, parse_constant=invalid_number)
        if key in json.dumps(result, ensure_ascii=False, allow_nan=False):
            raise ValueError("selected credential in decoded response")
        if not notification and (not isinstance(result, dict) or result.get("jsonrpc") != "2.0"
                                 or not valid_id(result.get("id")) or result.get("id") != request_id):
            raise ValueError("invalid response")
    except socket.gaierror:
        failed = True
        result = None if notification else failure(
            request_id, "HUD initialize DNS lookup failed" if initializing else "HUD connection failed"
        )
    except Exception:
        failed = True
        result = None if notification else failure(
            request_id, f"HUD initialize {phase} failed" if initializing else "HUD connection failed"
        )
    # Parent owns stdout; this pipe carries one bounded private result only.
    envelope = {"response": result}
    if not failed and opened is not None and re_session(opened):
        envelope["session"] = opened
    sys.stdout.buffer.write(json.dumps(envelope, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode())
    sys.stdout.buffer.flush()
    return 1 if notification and failed else 0


class Task:
    def __init__(self, process, pidfd, request, deadline, payload):
        self.process, self.pidfd, self.request, self.deadline = process, pidfd, request, deadline
        self.payload = memoryview(payload)
        self.output = bytearray()
        self.abort_phase = None
        self.abort_deadline = None
        self.cancelled = False
        self.result_seen = False
        self.retried = False
        self.internal = False


class Adapter:
    # Session state; class defaults keep partially built fixtures unscoped.
    session = None
    init_request = None
    reinitializing = False
    ending = None
    owner = None  # The client process whose helpers may join this session.
    had_session = False  # Once true, no call ever goes out unscoped.

    def __init__(self, endpoint):
        self.endpoint = endpoint
        self.owner = os.getppid()
        self.selector = selectors.DefaultSelector()
        self.tasks = []
        self.input = bytearray()
        self.output = bytearray()
        self.output_progress = None
        self.stop = False
        self.fatal = False
        self.initializing = False
        self.protocol = None
        self.initialized = False
        self.output_registered = False
        self.retry = []  # (request, deadline, retried) waiting for a session
        self.wake_read, self.wake_write = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
        self.old_wakeup = signal.set_wakeup_fd(self.wake_write, warn_on_full_buffer=False)
        self.old_handlers = {s: signal.getsignal(s) for s in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)}
        for s in self.old_handlers:
            signal.signal(s, self.terminate)
        os.set_blocking(sys.stdin.fileno(), False)
        os.set_blocking(sys.stdout.fileno(), False)
        self.selector.register(sys.stdin.fileno(), selectors.EVENT_READ, ("input", None))
        self.selector.register(self.wake_read, selectors.EVENT_READ, ("wake", None))

    def terminate(self, *_):
        self.stop = True

    def queue(self, response):
        request_id = response.get("id") if isinstance(response, dict) else None
        try:
            if not isinstance(response, dict) or (not valid_id(request_id) and not (request_id is None and "error" in response)):
                raise ValueError("invalid response ID")
            data = json.dumps(response, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode() + b"\n"
        except (TypeError, ValueError):
            data = json.dumps(failure(request_id if valid_id(request_id) else None), allow_nan=False).encode() + b"\n"
        if len(data) > MAX_RESPONSE or len(self.output) + len(data) > MAX_OUTPUT:
            self.stop = self.fatal = True
            return
        if not self.output:
            self.output_progress = time.monotonic()
        self.output.extend(data)
        if not self.output_registered:
            self.selector.register(sys.stdout.fileno(), selectors.EVENT_WRITE, ("output", None))
            self.output_registered = True

    def unregister(self, stream):
        try:
            self.selector.unregister(stream)
        except (KeyError, ValueError):
            pass

    def signal_task(self, task, sig):
        try:
            signal.pidfd_send_signal(task.pidfd, sig)
        except ProcessLookupError:
            pass
        except OSError:
            self.stop = self.fatal = True
            print("hud_mcp_stdio: cannot signal owned worker", file=sys.stderr)

    def abort(self, batch):
        now = time.monotonic()
        for task in batch:
            if task.abort_phase is None:
                task.cancelled = True
                task.abort_phase, task.abort_deadline = "term", now + TERM_SECONDS
                self.signal_task(task, signal.SIGTERM)
                if task.process.stdin is not None:
                    self.unregister(task.process.stdin)
                    task.process.stdin.close()
                    task.payload = memoryview(b"")

    def admit(self, request, deadline=None, *, retried=False, internal=False):
        if not isinstance(request, dict) or request.get("jsonrpc") != "2.0" or not isinstance(request.get("method"), str):
            self.queue(failure(None, "Invalid JSON-RPC message"))
            return
        method, request_id = request["method"], request.get("id")
        has_id = "id" in request
        if has_id and not valid_id(request_id):
            self.queue(failure(None, "Invalid request ID"))
            return
        if method == "notifications/cancelled":
            params = request.get("params")
            if not has_id and isinstance(params, dict):
                wanted = params.get("requestId")
                if valid_id(wanted):
                    self.abort([t for t in self.tasks if "id" in t.request and t.request["id"] == wanted
                                and t.request["method"] != "initialize" and not t.result_seen
                                and not t.internal])
            return
        if method == "notifications/initialized" and not has_id:
            # Consumed locally (the runtime ignores it): readiness never
            # depends on a HUD round trip. Premature or repeated: ignored.
            if self.protocol is not None:
                self.initialized = True
            return
        if self.reinitializing and not internal and method != "initialize":
            # Wait for the new session rather than run unscoped or on a dead id.
            self.retry.append((request, deadline if deadline is not None else time.monotonic() + REQUEST_SECONDS,
                               retried))
            return
        if (self.had_session and self.session is None and not internal
                and method != "initialize" and not self.stop):
            # A failed re-initialize left no session: never fall back to the
            # agent's unscoped caller; queue behind a fresh one.
            self.retry.append((request, deadline if deadline is not None else time.monotonic() + REQUEST_SECONDS,
                               retried))
            self.reinitialize()
            return
        # An abandoned re-initialize can still be exiting; its id is not live.
        if has_id and any("id" in t.request and t.request["id"] == request_id
                          and not (internal and t.cancelled) for t in self.tasks):
            self.queue(failure(request_id, "Request ID is already pending"))
            return
        seconds = INITIALIZE_SECONDS if method == "initialize" else REQUEST_SECONDS
        deadline = deadline if deadline is not None else time.monotonic() + seconds
        if len(self.tasks) >= MAX_WORKERS or time.monotonic() >= deadline:
            if has_id:
                self.queue(failure(request_id, "HUD request deadline or capacity exceeded"))
            return
        if internal:
            permitted = True
        elif method == "initialize":
            permitted = not self.initializing and self.protocol is None and has_id
        else:
            permitted = self.initialized
        if not permitted:
            if has_id:
                self.queue(failure(request_id, "HUD session not ready; reconnect the tze-hud MCP server"))
            return
        if method == "initialize":
            if not internal:
                self.init_request = request
            request = with_label(request)
        try:
            payload = json.dumps({"request": request, "endpoint": self.endpoint, "protocol": self.protocol,
                                  "session": None if method == "initialize" else self.session},
                                 ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
        except (TypeError, ValueError):
            if has_id:
                self.queue(failure(request_id))
            return
        process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--worker", str(os.getpid()), str(deadline)],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            pidfd = os.pidfd_open(process.pid)
        except OSError:
            process.stdin.close()  # No job/key/auth before retained identity is acquired.
            try:
                process.wait(timeout=TERM_SECONDS + KILL_SECONDS)
            except subprocess.TimeoutExpired:
                print("hud_mcp_stdio: unreaped startup worker", file=sys.stderr)
            self.stop = self.fatal = True
            return
        task = Task(process, pidfd, request, deadline, payload)
        task.retried, task.internal = retried, internal
        task.session = None if method == "initialize" else self.session
        self.tasks.append(task)
        os.set_blocking(process.stdin.fileno(), False)
        os.set_blocking(process.stdout.fileno(), False)
        self.selector.register(process.stdin, selectors.EVENT_WRITE, ("job", task))
        self.selector.register(process.stdout, selectors.EVENT_READ, ("result", task))
        self.selector.register(pidfd, selectors.EVENT_READ, ("exit", task))
        if method == "initialize" and not internal:
            self.initializing = True

    def reinitialize(self):
        """One internal initialize replaying the client's own, never forwarded."""
        if self.reinitializing or self.stop:
            return
        self.reinitializing = True
        request = dict(self.init_request or {"jsonrpc": "2.0", "method": "initialize", "params": {}})
        request["id"] = REINIT_ID
        self.admit(request, time.monotonic() + INITIALIZE_SECONDS, internal=True)
        if not any(t.internal and not t.cancelled for t in self.tasks):
            self.reinitialized(False)

    def reinitialized(self, ok):
        self.reinitializing = False
        waiting, self.retry = self.retry, []
        for request, deadline, retried in waiting:
            if ok and not retried and not self.stop:
                self.admit(request, deadline, retried=True)
            elif "id" in request:
                self.queue(failure(request["id"], "HUD session ended; reconnect the tze-hud MCP server"))

    def set_session(self, session):
        """Adopt `session` and record it privately for helpers the client
        starts (portal_client.py's background waiter joins it)."""
        self.session = session
        self.had_session = self.had_session or session is not None
        if self.owner is None:
            return
        try:
            if session is None or not re_session(session):
                hud_env.remove_session(self.owner)
            else:
                hud_env.write_session(self.owner, self.endpoint, session)
        except OSError:
            print("hud_mcp_stdio: cannot record the session for helpers", file=sys.stderr)

    def end_session(self):
        """DELETE this session once, without waiting on calls in flight."""
        if self.owner is not None:
            hud_env.remove_session(self.owner)
        if self.ending is not None or self.session is None:
            return
        payload = json.dumps({"request": {"jsonrpc": "2.0", "method": "notifications/end"},
                              "endpoint": self.endpoint, "protocol": self.protocol,
                              "session": self.session, "end": True}).encode()
        deadline = time.monotonic() + END_SECONDS
        try:
            self.ending = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "--worker", str(os.getpid()), str(deadline)],
                stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.ending.stdin.write(payload)
            self.ending.stdin.close()
        except OSError:
            pass
        self.ending_deadline = deadline

    def input_ready(self):
        data = os.read(sys.stdin.fileno(), CHUNK)
        if not data:
            self.stop = True
            return
        self.input.extend(data)
        if len(self.input) > 64 * 1024:
            self.stop = self.fatal = True

    def frames(self):
        for _ in range(16):
            if self.stop or b"\n" not in self.input:
                break
            line, _, rest = self.input.partition(b"\n")
            self.input = bytearray(rest)
            accepted_at = time.monotonic()
            if len(line) > MAX_REQUEST:
                self.queue(failure(None, "Request exceeds the HUD client limit"))
                continue
            request = None
            try:
                request = json.loads(line, parse_constant=invalid_number)
                json.dumps(request, allow_nan=False)  # Finite literals can overflow Python floats.
            except (TypeError, ValueError, UnicodeError):
                request_id = request.get("id") if isinstance(request, dict) else None
                self.queue(failure(request_id if valid_id(request_id) else None, "Invalid JSON-RPC message"))
                continue
            seconds = (INITIALIZE_SECONDS if isinstance(request, dict)
                       and request.get("method") == "initialize" else REQUEST_SECONDS)
            self.admit(request, accepted_at + seconds)

    def result(self, task):
        opened = None
        try:
            envelope = json.loads(task.output, parse_constant=invalid_number)
            json.dumps(envelope, allow_nan=False)
            if not isinstance(envelope, dict):
                raise ValueError("invalid worker envelope")
            if envelope.get("unknown_session") is True:
                if task.cancelled:
                    return
                if task.internal:
                    self.reinitialized(False)
                    return
                if task.retried or self.stop:
                    if "id" in task.request:
                        self.queue(failure(task.request["id"], "HUD session ended; reconnect the tze-hud MCP server"))
                    return
                if task.session != self.session and self.session is not None:
                    # Refused on the old session after a re-initialize: retry now.
                    self.admit(task.request, task.deadline, retried=True)
                    return
                self.set_session(None)
                self.retry.append((task.request, task.deadline, task.retried))
                self.reinitialize()
                return
            response = envelope.get("response")
            opened = envelope.get("session")
            if "id" in task.request:
                if not isinstance(response, dict) or response.get("jsonrpc") != "2.0" or not valid_id(response.get("id")) or response.get("id") != task.request["id"]:
                    raise ValueError("invalid worker response")
            elif response is not None:
                raise ValueError("invalid worker notification response")
        except (TypeError, ValueError, UnicodeError):
            message = "HUD connection failed"
            if task.request["method"] == "initialize":
                message = (INITIALIZE_TIMEOUT if task.process.returncode == -signal.SIGALRM
                           else "HUD initialize startup/protocol failed (phase unavailable)")
            response = failure(task.request.get("id"), message)
        if task.internal:
            if task.cancelled:
                return  # Its failure was reported when it was abandoned.
            result = response.get("result") if isinstance(response, dict) else None
            ok = isinstance(result, dict) and isinstance(opened, str) and not task.cancelled
            if ok:
                self.set_session(opened)
            self.reinitialized(ok)
            return
        if task.cancelled:
            return
        method = task.request["method"]
        if method == "initialize" and isinstance(opened, str):
            self.set_session(opened)
        if method == "initialize":
            self.initializing = False
            result = response.get("result") if isinstance(response, dict) else None
            revision = result.get("protocolVersion") if isinstance(result, dict) else None
            if isinstance(revision, str) and re_protocol(revision):
                self.protocol = revision
        if "id" in task.request:
            self.queue(response if isinstance(response, dict) else failure(task.request["id"]))

    def reap(self):
        now = time.monotonic()
        for task in list(self.tasks):
            code = task.process.poll()  # Reap BEFORE output/slot reuse; pipe EOF alone is insufficient.
            if code is not None:
                # A ready child may have exited before the selector delivered all pipe bytes.
                if not task.process.stdout.closed:
                    while len(task.output) <= MAX_RESPONSE:
                        try:
                            part = os.read(task.process.stdout.fileno(), CHUNK)
                        except BlockingIOError:
                            break
                        if not part:
                            break
                        task.output.extend(part)
                # Free the slot first: a result may admit a retry or re-initialize.
                for stream in (task.process.stdin, task.process.stdout):
                    self.unregister(stream)
                    if not stream.closed:
                        stream.close()
                self.unregister(task.pidfd)
                os.close(task.pidfd)
                self.tasks.remove(task)
                self.result(task)
                continue
            if task.abort_phase == "term" and now >= task.abort_deadline:
                self.signal_task(task, signal.SIGKILL)
                task.abort_phase, task.abort_deadline = "kill", now + KILL_SECONDS
            elif task.abort_phase == "kill" and now >= task.abort_deadline:
                self.stop = self.fatal = True
                print("hud_mcp_stdio: unreaped worker; stopping admission", file=sys.stderr)
                return False
            elif task.abort_phase is None and now >= task.deadline:
                if task.internal:
                    self.abort([task])
                    self.reinitialized(False)
                    continue
                if "id" in task.request:
                    message = INITIALIZE_TIMEOUT if task.request["method"] == "initialize" else "HUD request deadline exceeded"
                    self.queue(failure(task.request["id"], message))
                if task.request["method"] == "initialize":
                    self.initializing = False
                self.abort([task])
        return True

    def drive(self):
        try:
            while True:
                if self.output and time.monotonic() - self.output_progress >= OUTPUT_STALL_SECONDS:
                    self.stop = self.fatal = True
                if self.stop:
                    self.end_session()
                    self.abort(self.tasks)
                    self.output.clear()
                if not self.reap():
                    break
                if self.stop and not self.tasks:
                    break
                self.frames()
                deadlines = [t.abort_deadline or t.deadline for t in self.tasks]
                if self.output and not self.stop:
                    deadlines.append(self.output_progress + OUTPUT_STALL_SECONDS)
                wait = max(0.0, min(deadlines) - time.monotonic()) if deadlines else None
                if b"\n" in self.input and not self.stop:
                    wait = 0
                for key, _ in self.selector.select(wait):
                    kind, task = key.data
                    if kind == "wake":
                        os.read(self.wake_read, CHUNK)
                    elif kind == "input" and not self.stop:
                        self.input_ready()
                    elif kind == "output" and not self.stop:
                        try:
                            size = os.write(sys.stdout.fileno(), self.output[:CHUNK])
                            del self.output[:size]
                            self.output_progress = time.monotonic()
                            if not self.output:
                                self.unregister(sys.stdout.fileno())
                                self.output_registered = False
                        except BrokenPipeError:
                            self.stop = self.fatal = True
                    elif kind == "job" and not task.process.stdin.closed:
                        try:
                            size = os.write(task.process.stdin.fileno(), task.payload[:CHUNK])
                            task.payload = task.payload[size:]
                            if not task.payload:
                                self.unregister(task.process.stdin)
                                task.process.stdin.close()
                        except BrokenPipeError:
                            self.abort([task])
                    elif kind == "result" and not task.process.stdout.closed:
                        data = os.read(task.process.stdout.fileno(), CHUNK)
                        task.output.extend(data)
                        if not data:
                            self.unregister(task.process.stdout)
                        if len(task.output) > MAX_RESPONSE:
                            if "id" in task.request and not task.cancelled:
                                self.queue(failure(task.request["id"]))
                            self.abort([task])
        finally:
            self.end_session()
            # Ordinary exceptions use the same ONE global TERM and ONE global KILL deadlines.
            self.abort(self.tasks)
            while self.tasks and self.reap():
                time.sleep(0.005)
            for task in self.tasks:
                for stream in (task.process.stdin, task.process.stdout):
                    if not stream.closed:
                        stream.close()
                os.close(task.pidfd)
            if self.ending is not None:
                try:
                    self.ending.wait(timeout=max(0.0, self.ending_deadline - time.monotonic()) + 0.05)
                except subprocess.TimeoutExpired:
                    self.ending.kill()
                    self.ending.wait()
            signal.set_wakeup_fd(self.old_wakeup)
            for sig, handler in self.old_handlers.items():
                signal.signal(sig, handler)
            self.selector.close()
            os.close(self.wake_read)
            os.close(self.wake_write)
        return 1 if self.fatal else 0


def re_session(value):
    """A session id is visible ASCII the adapter may send back as a header."""
    return bool(value) and len(value) <= 128 and value.isascii() and value.isprintable() and " " not in value


def session_label():
    """The working folder's name: CLAUDE_PROJECT_DIR, else the cwd."""
    folder = os.environ.get("CLAUDE_PROJECT_DIR") or os.getcwd()
    return Path(folder.rstrip("/") or "/").name


def with_label(request):
    """The initialize to send: the client's own, plus this session's label."""
    params = request.get("params")
    params = dict(params) if isinstance(params, dict) else {}
    meta = params.get("_meta")
    meta = dict(meta) if isinstance(meta, dict) else {}
    label = session_label()
    if label:
        meta[LABEL_META_KEY] = label
        params["_meta"] = meta
    return {**request, "params": params}


def re_protocol(value):
    """Only a safe negotiated revision may become an HTTP header value."""
    return bool(value) and len(value) <= 64 and value.isascii() and all(c.isalnum() or c in "-_." for c in value)


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--worker":
        return worker(int(sys.argv[2]), float(sys.argv[3]))
    if len(sys.argv) != 1:
        print("hud_mcp_stdio: no public arguments", file=sys.stderr)
        return 2
    if sys.platform != "linux" or not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        print("hud_mcp_stdio: Linux/WSL pidfd and parent-death support required", file=sys.stderr)
        return 1
    try:
        check = os.pidfd_open(os.getpid())
        signal.pidfd_send_signal(check, 0)
        os.close(check)
        endpoint = hud_env.adapter_endpoint()
        return Adapter(endpoint).drive()
    except hud_env.HudEnvError as error:
        print(f"hud_mcp_stdio: {error}", file=sys.stderr)
    except Exception:
        print("hud_mcp_stdio: client failure", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
