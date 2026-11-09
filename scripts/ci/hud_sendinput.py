"""Win32 input and window helpers for the T13 tier-W gesture harness.

Imported only on Windows by `hud_gestures.py --injector sendinput`, which runs
only on a GitHub-hosted VM (the guard there requires `GITHUB_ACTIONS=true`).
`SendInput` injects keyboard-shaped virtual-key events for text the active
layout can type, Unicode packets (VK_PACKET) only when asked, absolute pointer
moves on the primary display and 120-unit wheel notches. Every injection calls
the harness guard first. SendInput's return value is logged, never asserted:
assertions rest on HUD state.

Stdlib only (ctypes).
"""

from __future__ import annotations

import ctypes
from ctypes import wintypes
from typing import Callable

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

# Handles are pointer-sized; ctypes' default int restype would truncate them.
for fn, res, args in (
    ("GetForegroundWindow", wintypes.HWND, []),
    ("FindWindowExW", wintypes.HWND, [wintypes.HWND, wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR]),
    ("GetWindowThreadProcessId", wintypes.DWORD, [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]),
    ("IsWindowVisible", wintypes.BOOL, [wintypes.HWND]),
    ("SetForegroundWindow", wintypes.BOOL, [wintypes.HWND]),
    ("ShowWindow", wintypes.BOOL, [wintypes.HWND, ctypes.c_int]),
    ("GetWindowRect", wintypes.BOOL, [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]),
    ("GetCursorPos", wintypes.BOOL, [ctypes.POINTER(wintypes.POINT)]),
    ("GetWindowLongPtrW", ctypes.c_ssize_t, [wintypes.HWND, ctypes.c_int]),
    ("SendMessageW", ctypes.c_ssize_t, [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]),
    ("GetDC", wintypes.HDC, [wintypes.HWND]),
    ("ReleaseDC", ctypes.c_int, [wintypes.HWND, wintypes.HDC]),
    ("VkKeyScanW", ctypes.c_short, [wintypes.WCHAR]),
):
    getattr(user32, fn).restype, getattr(user32, fn).argtypes = res, args
for fn, res, args in (
    ("CreateCompatibleDC", wintypes.HDC, [wintypes.HDC]),
    ("CreateCompatibleBitmap", wintypes.HBITMAP, [wintypes.HDC, ctypes.c_int, ctypes.c_int]),
    ("SelectObject", wintypes.HGDIOBJ, [wintypes.HDC, wintypes.HGDIOBJ]),
    ("DeleteObject", wintypes.BOOL, [wintypes.HGDIOBJ]),
    ("DeleteDC", wintypes.BOOL, [wintypes.HDC]),
):
    getattr(gdi32, fn).restype, getattr(gdi32, fn).argtypes = res, args
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]

# Per-monitor v2: GetSystemMetrics and SendInput coordinates are physical pixels.
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))

ULONG_PTR = ctypes.c_size_t


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD), ("dwFlags", wintypes.DWORD),
                ("time", wintypes.DWORD), ("dwExtraInfo", ULONG_PTR)]


class HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", wintypes.DWORD), ("wParamL", wintypes.WORD), ("wParamH", wintypes.WORD)]


class _INPUTUNION(ctypes.Union):
    # The full union, so sizeof(INPUT) matches what SendInput expects.
    _fields_ = [("mi", MOUSEINPUT), ("ki", KEYBDINPUT), ("hi", HARDWAREINPUT)]


class INPUT(ctypes.Structure):
    _anonymous_ = ("u",)
    _fields_ = [("type", wintypes.DWORD), ("u", _INPUTUNION)]


MOUSE_MOVE, LEFT_DOWN, LEFT_UP, WHEEL, ABSOLUTE = 0x1, 0x2, 0x4, 0x800, 0x8000
KEY_UP, KEY_UNICODE = 0x2, 0x4
WHEEL_DELTA = 120

VK = {"ctrl": 0x11, "control": 0x11, "shift": 0x10, "alt": 0x12, "tab": 0x09, "return": 0x0D,
      "enter": 0x0D, "escape": 0x1B, "space": 0x20, "minus": 0xBD, "equal": 0xBB, "plus": 0xBB,
      "backspace": 0x08, "home": 0x24, "end": 0x23, "prior": 0x21, "next": 0x22,
      "left": 0x25, "up": 0x26, "right": 0x27, "down": 0x28}
VK.update({f"f{n}": 0x6F + n for n in range(1, 25)})
EXTENDED = {0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28}  # navigation keys, not their numpad twins
KEY_EXTENDED = 0x1


def _mouse(flags: int, dx: int = 0, dy: int = 0, data: int = 0) -> INPUT:
    item = INPUT(type=0)
    item.mi = MOUSEINPUT(dx, dy, data & 0xFFFFFFFF, flags, 0, 0)
    return item


def _key(vk: int = 0, scan: int = 0, flags: int = 0) -> INPUT:
    item = INPUT(type=1)
    item.ki = KEYBDINPUT(vk, scan, flags, 0, 0)
    return item


def _vk_key(vk: int, up: bool = False) -> INPUT:
    flags = (KEY_UP if up else 0) | (KEY_EXTENDED if vk in EXTENDED else 0)
    return _key(vk, user32.MapVirtualKeyW(vk, 0), flags)


def _layout_key(ch: str) -> tuple[int, bool]:
    """(virtual key, needs Shift) for `ch` on the active layout, or ValueError."""
    scan = user32.VkKeyScanW(ch)
    if scan == -1 or scan & 0x600:  # no key, or it needs Ctrl/Alt (control characters, AltGr)
        raise ValueError(f"the keyboard layout cannot type {ch!r} as a plain key; use type_unicode")
    return scan & 0xFF, bool(scan & 0x100)


def chord_vks(chord: str) -> list[int]:
    """xdotool-style chord ("ctrl+shift+F12", "Return", "a") to virtual keys."""
    vks = []
    for part in chord.split("+"):
        name = part.strip().lower()
        if name in VK:
            vks.append(VK[name])
        elif len(part) == 1:
            vks.append(_layout_key(part)[0])
        else:
            raise ValueError(f"no virtual key for {part!r}")
    return vks


def screen_size() -> tuple[int, int]:
    return user32.GetSystemMetrics(0), user32.GetSystemMetrics(1)


def system_dpi() -> int:
    return user32.GetDpiForSystem()


def foreground_pid() -> int:
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(user32.GetForegroundWindow(), ctypes.byref(pid))
    return pid.value


def process_alive(pid: int) -> bool:
    """os.kill(pid, 0) would terminate the process on Windows; ask the kernel instead."""
    handle = kernel32.OpenProcess(0x1000, False, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not handle:
        return False
    try:
        code = wintypes.DWORD()
        return bool(kernel32.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == 259  # STILL_ACTIVE
    finally:
        kernel32.CloseHandle(handle)


def _windows_of(pid: int) -> list[int]:
    found: list[int] = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def visit(hwnd, _):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user32.IsWindowVisible(hwnd):
            found.append(hwnd)
        return True

    user32.EnumWindows(visit, 0)
    return found


def bring_to_front(pid: int) -> bool:
    """Ask Windows to foreground `pid`'s window (no input injected)."""
    windows = _windows_of(pid)
    return bool(windows) and bool(user32.SetForegroundWindow(windows[0]))


def pointer_capturing(pid: int) -> bool | None:
    windows = _windows_of(pid)
    if len(windows) != 1:
        return None
    # winit's set_cursor_hittest(false) sets WS_EX_TRANSPARENT. Observe the
    # actual native switch, rather than assuming a hover implies capture.
    return not bool(user32.GetWindowLongPtrW(windows[0], -20) & 0x20)


def maximize(pid: int) -> None:
    for hwnd in _windows_of(pid):
        user32.ShowWindow(hwnd, 3)  # SW_MAXIMIZE, only an owned helper


def cursor_position() -> tuple[int, int]:
    point = wintypes.POINT()
    if not user32.GetCursorPos(ctypes.byref(point)):
        raise ctypes.WinError(ctypes.get_last_error())
    return point.x, point.y


def edit_rect(pid: int) -> tuple[int, int, int, int] | None:
    for hwnd in _windows_of(pid):
        for cls in ("Edit", "RichEditD2DPT"):
            child = user32.FindWindowExW(hwnd, None, cls, None)
            rect = wintypes.RECT()
            if child and user32.GetWindowRect(child, ctypes.byref(rect)):
                return rect.left, rect.top, rect.right, rect.bottom
    return None


def edit_text(pid: int) -> str | None:
    """Text of the edit control in `pid`'s top-level window (Notepad), or None."""
    for hwnd in _windows_of(pid):
        for cls in ("Edit", "RichEditD2DPT"):
            child = user32.FindWindowExW(hwnd, None, cls, None)
            if child:
                length = user32.SendMessageW(child, 0x000E, 0, 0)  # WM_GETTEXTLENGTH
                buf = ctypes.create_unicode_buffer(length + 1)
                user32.SendMessageW(child, 0x000D, length + 1, ctypes.cast(buf, ctypes.c_void_p).value)  # WM_GETTEXT
                return buf.value
    return None


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [("biSize", wintypes.DWORD), ("biWidth", wintypes.LONG), ("biHeight", wintypes.LONG),
                ("biPlanes", wintypes.WORD), ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
                ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", wintypes.LONG),
                ("biYPelsPerMeter", wintypes.LONG), ("biClrUsed", wintypes.DWORD), ("biClrImportant", wintypes.DWORD)]


def desktop_rgba() -> tuple[int, int, bytes]:
    """The whole primary desktop as RGBA (what a person would see, not the HUD's own frame)."""
    width, height = screen_size()
    screen_dc = user32.GetDC(0)
    mem_dc = gdi32.CreateCompatibleDC(screen_dc)
    bitmap = gdi32.CreateCompatibleBitmap(screen_dc, width, height)
    gdi32.SelectObject(mem_dc, bitmap)
    gdi32.BitBlt(mem_dc, 0, 0, width, height, screen_dc, 0, 0, 0x00CC0020 | 0x40000000)  # SRCCOPY|CAPTUREBLT
    header = BITMAPINFOHEADER(ctypes.sizeof(BITMAPINFOHEADER), width, -height, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(width * height * 4)
    gdi32.GetDIBits(mem_dc, bitmap, 0, height, buf, ctypes.byref(header), 0)
    gdi32.DeleteObject(bitmap)
    gdi32.DeleteDC(mem_dc)
    user32.ReleaseDC(0, screen_dc)
    raw = bytearray(buf.raw)
    raw[0::4], raw[2::4] = raw[2::4], raw[0::4]  # BGRA -> RGBA
    raw[3::4] = b"\xff" * (width * height)
    return width, height, bytes(raw)


class SendInput:
    """Win32 SendInput on the primary display; every call passes the guard first."""

    name = "sendinput"

    def __init__(self, guard: Callable[[], None]) -> None:
        self.guard = guard
        self.calls: list[dict] = []

    def _send(self, what: str, items: list[INPUT]) -> None:
        self.guard()
        array = (INPUT * len(items))(*items)
        sent = user32.SendInput(len(items), array, ctypes.sizeof(INPUT))
        # Logged, never an assertion.
        self.calls.append({"what": what, "requested": len(items), "sent": sent,
                           "last_error": ctypes.get_last_error()})

    def move_abs(self, x: int, y: int) -> None:
        width, height = screen_size()
        self._send(f"move {x},{y}", [_mouse(MOUSE_MOVE | ABSOLUTE, round(x * 65535 / (width - 1)),
                                            round(y * 65535 / (height - 1)))])

    def move_relative(self, dx: int, dy: int) -> None:
        self._send(f"relative {dx},{dy}", [_mouse(MOUSE_MOVE, dx, dy)])

    def button(self, down: bool, button: int = 1) -> None:
        if button != 1:
            raise ValueError("only the left button is used")
        self._send("left down" if down else "left up", [_mouse(LEFT_DOWN if down else LEFT_UP)])

    def click(self, button: int = 1, repeat: int = 1) -> None:
        """Button 1 clicks; 4 and 5 are one wheel notch up or down (as in X11)."""
        if button in (4, 5):
            delta = WHEEL_DELTA if button == 4 else -WHEEL_DELTA
            self._send(f"wheel {repeat}x{delta}", [_mouse(WHEEL, data=delta) for _ in range(repeat)])
            return
        if button != 1:
            raise ValueError(f"button {button}")
        self._send(f"click x{repeat}", [_mouse(flag) for _ in range(repeat) for flag in (LEFT_DOWN, LEFT_UP)])

    def key(self, chord: str) -> None:
        vks = chord_vks(chord)
        self._send(f"key {chord}", [_vk_key(v) for v in vks] + [_vk_key(v, up=True) for v in reversed(vks)])

    def type(self, text: str) -> None:
        """Keyboard-shaped typing: a virtual key per character, Shift when the layout needs it."""
        items: list[INPUT] = []
        for ch in text:
            vk, shift = _layout_key(ch)
            if shift:
                items.append(_vk_key(VK["shift"]))
            items += [_vk_key(vk), _vk_key(vk, up=True)]
            if shift:
                items.append(_vk_key(VK["shift"], up=True))
        self._send(f"type {len(text)} chars", items)

    def type_unicode(self, text: str) -> None:
        """Unicode packets (VK_PACKET), as on-screen keyboards and text tools send."""
        items = []
        for unit in memoryview(text.encode("utf-16-le")).cast("H"):  # UTF-16 code units, as SendInput wants
            items += [_key(0, unit, KEY_UNICODE), _key(0, unit, KEY_UNICODE | KEY_UP)]
        self._send(f"unicode {len(text)} chars", items)

