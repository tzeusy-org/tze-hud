"""Bounded inventory and pixel reads from one harness-owned Xvfb connection."""

from __future__ import annotations

import ctypes as c
import json
import os
from pathlib import Path
import re
import secrets
import socket
import struct
import subprocess
import sys
import time

HEADER = struct.Struct("!II")
UCREDS = struct.Struct("@iII")
NS = 1_000_000_000
MAX_HEADER = 4096
SCREEN = (1280, 800)
MAX_PIXELS = 4 * SCREEN[0] * SCREEN[1]
Rect = tuple[int, int, int, int]
Point = tuple[int, int]


class CaptureError(RuntimeError):
    """No verified sample was delivered."""


def write_authority(path: Path, number: int) -> None:
    """Write one display-number-specific MIT cookie in a private scratch file."""
    fields = (b"", str(number).encode("ascii"), b"MIT-MAGIC-COOKIE-1", secrets.token_bytes(16))
    # FamilyWild handles the unnamed local/abstract-address variant of Xvfb.
    record = struct.pack("!H", 65535) + b"".join(struct.pack("!H", len(part)) + part for part in fields)
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as out:
        out.write(record)


def inside(rect: Rect, extent: tuple[int, int] = SCREEN) -> bool:
    x, y, width, height = rect
    return 0 <= x < x + width <= extent[0] and 0 <= y < y + height <= extent[1]


def bgrx_to_rgba(raw: bytes, width: int, height: int, stride: int) -> bytes:
    pitch = width * 4
    if (width <= 0 or height <= 0 or stride < pitch or stride % 4
            or len(raw) != stride * height or len(raw) > MAX_PIXELS):
        raise CaptureError("invalid XImage byte length or stride")
    rgba = bytearray(b"\xff" * (pitch * height))
    for row in range(height):
        data = raw[row * stride:row * stride + pitch]
        start, stop = row * pitch, (row + 1) * pitch
        rgba[start:stop:4] = data[2::4]
        rgba[start + 1:stop:4] = data[1::4]
        rgba[start + 2:stop:4] = data[0::4]
    return bytes(rgba)


def remaining(deadline: int) -> float:
    timeout = (deadline - time.monotonic_ns()) / NS
    if timeout <= 0:
        raise TimeoutError("X11 capture deadline reached")
    return timeout


def recv_exact(pipe: socket.socket, size: int, deadline: int) -> bytes:
    data = bytearray(size)
    view = memoryview(data)
    while view:
        pipe.settimeout(remaining(deadline))
        count = pipe.recv_into(view)
        if not count:
            raise EOFError("X11 capture child disconnected")
        view = view[count:]
    return bytes(data)


def packet(pipe: socket.socket, deadline: int) -> tuple[dict, bytes]:
    head, size = HEADER.unpack(recv_exact(pipe, HEADER.size, deadline))
    if not 0 < head <= MAX_HEADER or size > MAX_PIXELS:
        raise CaptureError("invalid X11 capture packet lengths")
    metadata = json.loads(recv_exact(pipe, head, deadline))
    if not isinstance(metadata, dict):
        raise CaptureError("invalid X11 capture metadata")
    return metadata, recv_exact(pipe, size, deadline)


def send(pipe: socket.socket, metadata: dict, deadline: int, pixels: bytes = b"") -> None:
    head = json.dumps(metadata, separators=(",", ":")).encode("ascii")
    pipe.settimeout(remaining(deadline))
    pipe.sendall(HEADER.pack(len(head), len(pixels)) + head + pixels)


I, U, UL, P = c.c_int, c.c_uint, c.c_ulong, c.c_void_p


class XImage(c.Structure):
    # Xlib.h, the prefix through blue_mask. Xlib owns the trailing functions.
    _fields_ = [
        ("width", I), ("height", I), ("xoffset", I), ("format", I), ("data", P),
        ("byte_order", I), ("bitmap_unit", I), ("bitmap_bit_order", I), ("bitmap_pad", I),
        ("depth", I), ("stride", I), ("bpp", I), ("red", UL), ("green", UL), ("blue", UL),
    ]


def bind_xlib():
    lib = c.CDLL("libX11.so.6")
    image = c.POINTER(XImage)
    specs = {
        "XOpenDisplay": (P, [c.c_char_p]),
        "XCloseDisplay": (I, [P]),
        "XConnectionNumber": (I, [P]),
        "XDefaultScreen": (I, [P]),
        "XDefaultRootWindow": (UL, [P]),
        "XDisplayWidth": (I, [P, I]),
        "XDisplayHeight": (I, [P, I]),
        "XDefaultDepth": (I, [P, I]),
        "XGetImage": (image, [P, UL, I, I, U, U, UL, I]),
        "XDestroyImage": (I, [image]),
        "XQueryPointer": (I, [P, UL, c.POINTER(UL), c.POINTER(UL),
                              c.POINTER(I), c.POINTER(I), c.POINTER(I), c.POINTER(I), c.POINTER(U)]),
    }
    for name, (result, args) in specs.items():
        func = getattr(lib, name)
        func.restype, func.argtypes = result, args
    return lib


def owned_peer(fd: int, owner: int) -> dict:
    if fd < 0:
        raise CaptureError("Xlib returned no connection fd")
    with socket.fromfd(fd, socket.AF_UNIX, socket.SOCK_STREAM) as peer:
        if (peer.getsockopt(socket.SOL_SOCKET, socket.SO_DOMAIN) != socket.AF_UNIX
                or peer.getsockopt(socket.SOL_SOCKET, socket.SO_TYPE) != socket.SOCK_STREAM):
            raise CaptureError("capture connection is not a local stream")
        pid, uid, _ = UCREDS.unpack(peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, UCREDS.size))
    if pid != owner or pid <= 0 or uid != os.getuid():
        raise CaptureError("capture connection peer is not the owned Xvfb pid/uid")
    return {"pid": pid, "uid": uid}


def root_sample(lib, display, root: int, rect: Rect, expected: Point) -> tuple[dict, bytes]:
    if not inside(rect) or not inside((*expected, 1, 1)):
        raise CaptureError("invalid capture or pointer bounds")
    found, child, root_x, root_y, win_x, win_y, mask = UL(), UL(), I(), I(), I(), I(), U()
    pointer_begin = time.monotonic_ns()
    current = lib.XQueryPointer(display, root, c.byref(found), c.byref(child),
                               c.byref(root_x), c.byref(root_y), c.byref(win_x),
                               c.byref(win_y), c.byref(mask))
    pointer_end = time.monotonic_ns()
    if not current or found.value != root or (root_x.value, root_y.value) != expected:
        raise CaptureError("pointer query did not confirm the expected private-root coordinate")
    x, y, width, height = rect
    begin = time.monotonic_ns()
    image = lib.XGetImage(display, root, x, y, width, height, UL(-1).value, 2)
    end = time.monotonic_ns()
    if not image:
        raise CaptureError("XGetImage returned null")
    try:
        fields = image.contents
        masks = (fields.red, fields.green, fields.blue)
        if (not fields.data or (fields.width, fields.height) != (width, height)
                or (fields.xoffset, fields.format, fields.depth, fields.bpp, fields.byte_order) != (0, 2, 24, 32, 0)
                or masks != (0xff0000, 0xff00, 0xff)
                or fields.stride < width * 4 or fields.stride % 4
                or fields.stride * height > MAX_PIXELS):
            raise CaptureError("unexpected XImage format, bounds or channels")
        rgba = bgrx_to_rgba(c.string_at(fields.data, fields.stride * height), width, height, fields.stride)
        return {
            "rect": rect, "pointer": expected, "pointer_ns": [pointer_begin, pointer_end],
            "acquisition_ns": [begin, end], "depth": 24, "bpp": 32, "byte_order": "LSBFirst",
            "stride": fields.stride, "rgb_masks": masks, "bytes": len(rgba),
        }, rgba
    finally:
        lib.XDestroyImage(image)


def capture_child(pipe: socket.socket, owner: int, name: str) -> None:
    if (not re.fullmatch(r":\d+", name) or os.environ.get("DISPLAY") != name
            or not os.environ.get("XAUTHORITY")):
        raise CaptureError("capture child was not given an explicit private display and authority")
    lib = bind_xlib()
    display = lib.XOpenDisplay(name.encode("ascii"))
    if not display:
        raise CaptureError("could not open the private Xvfb display")
    try:
        peer = owned_peer(lib.XConnectionNumber(display), owner)
        screen, root = lib.XDefaultScreen(display), lib.XDefaultRootWindow(display)
        size = (lib.XDisplayWidth(display, screen), lib.XDisplayHeight(display, screen))
        if screen != 0 or not root or size != SCREEN or lib.XDefaultDepth(display, screen) != 24:
            raise CaptureError("private display is not the expected 1280x800x24 root")
        send(pipe, {"peer": peer, "root": root, "screen": size, "depth": 24}, time.monotonic_ns() + NS)
        while True:
            request, payload = packet(pipe, time.monotonic_ns() + 60 * NS)
            if request == {"stop": True} and not payload:
                return
            if payload or request.keys() != {"id", "rect", "point"}:
                raise CaptureError("invalid read-only capture command")
            meta, rgba = root_sample(lib, display, root, tuple(request["rect"]), tuple(request["point"]))
            send(pipe, {"id": request["id"], **meta}, time.monotonic_ns() + NS, rgba)
    finally:
        lib.XCloseDisplay(display)


class Capture:
    """One read-only child, one request at a time, and an absolute reply deadline."""

    def __init__(self, xvfb: subprocess.Popen, display: str, authority: Path):
        self.xvfb = xvfb
        self.seq = 0
        self.pipe, remote = socket.socketpair()
        with remote:
            try:
                self.proc = subprocess.Popen(
                    [sys.executable, str(Path(__file__).resolve()), str(remote.fileno()), str(xvfb.pid), display],
                    env={"DISPLAY": display, "XAUTHORITY": str(authority), "HOME": str(authority.parent)},
                    pass_fds=(remote.fileno(),), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
            except OSError:
                self.pipe.close()
                raise
        try:
            self.ready, body = self.receive(time.monotonic_ns() + 3 * NS)
            if (body or self.ready.get("peer") != {"pid": xvfb.pid, "uid": os.getuid()}
                    or self.ready.get("screen") != list(SCREEN) or self.ready.get("depth") != 24
                    or not self.ready.get("root")):
                raise CaptureError("child readiness did not describe the exact owned root")
        except Exception:
            try:
                self.close()
            except (OSError, subprocess.TimeoutExpired):
                pass
            raise

    def receive(self, deadline: int) -> tuple[dict, bytes]:
        head, pixels = packet(self.pipe, deadline)
        if self.xvfb.poll() is not None:
            raise CaptureError("Xvfb disappeared before capture reply")
        if "error" in head:
            raise CaptureError(head["error"])
        return head, pixels

    def sample(self, rect: Rect, point: Point, deadline: int) -> tuple[dict, bytes]:
        if self.xvfb.poll() is not None or self.proc.poll() is not None:
            raise CaptureError("capture or Xvfb owner is no longer running")
        if not inside(rect) or not inside((*point, 1, 1)):
            raise CaptureError("request lies outside the private screen")
        self.seq += 1
        requested = time.monotonic_ns()
        send(self.pipe, {"id": self.seq, "rect": rect, "point": point}, deadline)
        meta, rgba = self.receive(deadline)
        if (meta.get("id") != self.seq or meta.get("rect") != list(rect) or meta.get("pointer") != list(point)
                or meta.get("bytes") != len(rgba) or len(rgba) != 4 * rect[2] * rect[3]):
            raise CaptureError("captured frame does not match its pending request")
        intervals = (meta.get("pointer_ns"), meta.get("acquisition_ns"))
        if any(not isinstance(value, list) or len(value) != 2 for value in intervals):
            raise CaptureError("capture reply timestamps are absent or out of order")
        times = intervals[0] + intervals[1]
        if (any(type(value) is not int for value in times)
                or not requested <= times[0] <= times[1] <= times[2] <= times[3] <= time.monotonic_ns()):
            raise CaptureError("capture reply timestamps are absent or out of order")
        return meta, rgba

    def close(self) -> dict:
        try:
            send(self.pipe, {"stop": True}, time.monotonic_ns() + NS // 4)
        except OSError:
            pass
        finally:
            self.pipe.close()
        forced = False
        try:
            self.proc.wait(1)
        except subprocess.TimeoutExpired:
            forced = True
            self.proc.terminate()
            try:
                self.proc.wait(1)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(1)
        return {"returncode": self.proc.returncode, "forced": forced, "reaped": True}


if __name__ == "__main__":
    with socket.socket(fileno=int(sys.argv[1])) as pipe:
        try:
            capture_child(pipe, int(sys.argv[2]), sys.argv[3])
        except (CaptureError, OSError, EOFError) as err:
            message = str(err) if isinstance(err, CaptureError) else type(err).__name__
            try:
                send(pipe, {"error": message}, time.monotonic_ns() + NS // 4)
            except OSError:
                pass
            sys.exit(1)
