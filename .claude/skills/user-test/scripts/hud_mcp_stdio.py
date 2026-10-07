#!/usr/bin/env python3
"""Bounded Linux/WSL stdio client for the HUD's existing authenticated HTTP MCP.

Each accepted message has a 60s absolute deadline and owns one of four child
slots until that child is reaped. Cancellation is local best effort: a POST
already accepted remotely is never replayed or rolled back. Native macOS and
Windows client lifetime primitives are unsupported; the Windows HUD is unchanged.
"""

from __future__ import annotations

import ctypes
import http.client
import json
import os
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

# runpy's project-root launcher does not add this script directory to sys.path.
sys.path.insert(0, str(Path(__file__).resolve().parent))
import hud_env

REQUEST_SECONDS = 60.0
TERM_SECONDS = 1.0
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
        key = hud_env.adapter_key(job["endpoint"])
        url = urlsplit(job["endpoint"]["url"])
        body = json.dumps(request, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
        headers = {"Authorization": f"Bearer {key}", "Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream", "Connection": "close"}
        if job["protocol"]:
            headers["MCP-Protocol-Version"] = job["protocol"]
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=max(0.001, deadline - time.monotonic()))
        try:
            connection.request("POST", "/mcp", body=body, headers=headers)
            response = connection.getresponse()
            if response.status != 200 and not (notification and response.status == 202):
                raise ValueError("HTTP refusal or redirect")
            raw = response.read(MAX_RESPONSE + 1)
        finally:
            connection.close()
        if len(raw) > MAX_RESPONSE or key.encode() in raw:
            raise ValueError("unsafe or oversized response")
        result = None if notification else json.loads(raw, parse_constant=invalid_number)
        if key in json.dumps(result, ensure_ascii=False, allow_nan=False):
            raise ValueError("selected credential in decoded response")
        if not notification and (not isinstance(result, dict) or result.get("jsonrpc") != "2.0"
                                 or not valid_id(result.get("id")) or result.get("id") != request_id):
            raise ValueError("invalid response")
    except Exception:
        failed = True
        result = None if notification else failure(request_id)
    # Parent owns stdout; this pipe carries one bounded private result only.
    sys.stdout.buffer.write(json.dumps(result, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode())
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


class Adapter:
    def __init__(self, endpoint):
        self.endpoint = endpoint
        self.selector = selectors.DefaultSelector()
        self.tasks = []
        self.waiting = []
        self.input = bytearray()
        self.output = bytearray()
        self.output_progress = None
        self.stop = False
        self.fatal = False
        self.initializing = False
        self.protocol = None
        self.initialized = False
        self.initialized_pending = False
        self.output_registered = False
        self.wake_read, self.wake_write = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
        self.old_wakeup = signal.set_wakeup_fd(self.wake_write, warn_on_full_buffer=False)
        self.old_handlers = {s: signal.getsignal(s) for s in (signal.SIGTERM, signal.SIGINT)}
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
                if task.request["method"] == "notifications/initialized":
                    self.initialized_pending = False
                if task.process.stdin is not None:
                    self.unregister(task.process.stdin)
                    task.process.stdin.close()
                    task.payload = memoryview(b"")

    def admit(self, request, deadline=None):
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
                    self.waiting = [(r, d) for r, d in self.waiting if r.get("id") != wanted or r.get("method") == "initialize"]
                    self.abort([t for t in self.tasks if "id" in t.request and t.request["id"] == wanted
                                and t.request["method"] != "initialize" and not t.result_seen])
            return
        if has_id and (any("id" in t.request and t.request["id"] == request_id for t in self.tasks)
                       or any("id" in r and r["id"] == request_id for r, _ in self.waiting)):
            self.queue(failure(request_id, "Request ID is already pending"))
            return
        deadline = deadline if deadline is not None else time.monotonic() + REQUEST_SECONDS
        if len(self.tasks) + len(self.waiting) >= MAX_WORKERS or time.monotonic() >= deadline:
            if has_id:
                self.queue(failure(request_id, "HUD request deadline or capacity exceeded"))
            return
        if self.initialized_pending and method not in ("initialize", "notifications/initialized"):
            self.waiting.append((request, deadline))
            return
        if method == "initialize":
            permitted = not self.initializing and self.protocol is None and has_id
        elif method == "notifications/initialized":
            permitted = self.protocol is not None and not self.initialized and not has_id
        else:
            permitted = self.initialized
        if not permitted:
            if has_id:
                self.queue(failure(request_id, "HUD connection not ready or at capacity"))
            return
        try:
            payload = json.dumps({"request": request, "endpoint": self.endpoint, "protocol": self.protocol},
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
        self.tasks.append(task)
        os.set_blocking(process.stdin.fileno(), False)
        os.set_blocking(process.stdout.fileno(), False)
        self.selector.register(process.stdin, selectors.EVENT_WRITE, ("job", task))
        self.selector.register(process.stdout, selectors.EVENT_READ, ("result", task))
        self.selector.register(pidfd, selectors.EVENT_READ, ("exit", task))
        if method == "initialize":
            self.initializing = True
        elif method == "notifications/initialized":
            self.initialized_pending = True

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
            deadline = time.monotonic() + REQUEST_SECONDS
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
            self.admit(request, deadline)

    def result(self, task):
        try:
            response = json.loads(task.output, parse_constant=invalid_number)
            json.dumps(response, allow_nan=False)
            if "id" in task.request:
                if not isinstance(response, dict) or response.get("jsonrpc") != "2.0" or not valid_id(response.get("id")) or response.get("id") != task.request["id"]:
                    raise ValueError("invalid worker response")
            elif response is not None:
                raise ValueError("invalid worker notification response")
        except (TypeError, ValueError, UnicodeError):
            response = failure(task.request.get("id"))
        if task.cancelled:
            return
        method = task.request["method"]
        if method == "initialize":
            self.initializing = False
            result = response.get("result") if isinstance(response, dict) else None
            revision = result.get("protocolVersion") if isinstance(result, dict) else None
            if isinstance(revision, str) and re_protocol(revision):
                self.protocol = revision
        elif method == "notifications/initialized":
            self.initialized_pending = False
            if response is None and task.process.returncode == 0:
                self.initialized = True
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
                self.result(task)
                for stream in (task.process.stdin, task.process.stdout):
                    self.unregister(stream)
                    if not stream.closed:
                        stream.close()
                self.unregister(task.pidfd)
                os.close(task.pidfd)
                self.tasks.remove(task)
                continue
            if task.abort_phase == "term" and now >= task.abort_deadline:
                self.signal_task(task, signal.SIGKILL)
                task.abort_phase, task.abort_deadline = "kill", now + KILL_SECONDS
            elif task.abort_phase == "kill" and now >= task.abort_deadline:
                self.stop = self.fatal = True
                print("hud_mcp_stdio: unreaped worker; stopping admission", file=sys.stderr)
                return False
            elif task.abort_phase is None and now >= task.deadline:
                if "id" in task.request:
                    self.queue(failure(task.request["id"], "HUD request deadline exceeded"))
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
                    self.abort(self.tasks)
                    self.output.clear()
                    self.waiting.clear()
                if not self.reap():
                    break
                if self.stop and not self.tasks:
                    break
                if self.waiting and not self.initialized_pending:
                    waiting, self.waiting = self.waiting, []
                    for request, deadline in waiting:
                        self.admit(request, deadline)
                self.frames()
                deadlines = [t.abort_deadline or t.deadline for t in self.tasks]
                deadlines.extend(d for _, d in self.waiting)
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
            # Ordinary exceptions use the same ONE global TERM and ONE global KILL deadlines.
            self.abort(self.tasks)
            while self.tasks and self.reap():
                time.sleep(0.005)
            for task in self.tasks:
                for stream in (task.process.stdin, task.process.stdout):
                    if not stream.closed:
                        stream.close()
                os.close(task.pidfd)
            signal.set_wakeup_fd(self.old_wakeup)
            for sig, handler in self.old_handlers.items():
                signal.signal(sig, handler)
            self.selector.close()
            os.close(self.wake_read)
            os.close(self.wake_write)
        return 1 if self.fatal else 0


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
