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
shell32 = ctypes.WinDLL("shell32", use_last_error=True)

# Handles are pointer-sized; ctypes' default int restype would truncate them.
for fn, res, args in (
    ("GetForegroundWindow", wintypes.HWND, []),
    ("FindWindowW", wintypes.HWND, [wintypes.LPCWSTR, wintypes.LPCWSTR]),
    ("WindowFromPoint", wintypes.HWND, [wintypes.POINT]),
    ("IsChild", wintypes.BOOL, [wintypes.HWND, wintypes.HWND]),
    ("FindWindowExW", wintypes.HWND, [wintypes.HWND, wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR]),
    ("GetWindowThreadProcessId", wintypes.DWORD, [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]),
    ("IsWindowVisible", wintypes.BOOL, [wintypes.HWND]),
    ("SetForegroundWindow", wintypes.BOOL, [wintypes.HWND]),
    ("ShowWindow", wintypes.BOOL, [wintypes.HWND, ctypes.c_int]),
    ("GetWindowRect", wintypes.BOOL, [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]),
    ("GetCursorPos", wintypes.BOOL, [ctypes.POINTER(wintypes.POINT)]),
    ("GetWindowLongPtrW", ctypes.c_ssize_t, [wintypes.HWND, ctypes.c_int]),
    ("GetWindowTextW", ctypes.c_int, [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]),
    ("GetClassNameW", ctypes.c_int, [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]),
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
    ("BitBlt", wintypes.BOOL, [wintypes.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                              wintypes.HDC, ctypes.c_int, ctypes.c_int, wintypes.DWORD]),
    ("GetDIBits", ctypes.c_int, [wintypes.HDC, wintypes.HBITMAP, wintypes.UINT, wintypes.UINT,
                                ctypes.c_void_p, ctypes.c_void_p, wintypes.UINT]),
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
RIGHT_DOWN, RIGHT_UP = 0x8, 0x10
GWL_EXSTYLE, WS_EX_TOPMOST, WS_EX_TRANSPARENT = -20, 0x8, 0x20
WS_EX_NOACTIVATE = 0x08000000
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


class NOTIFYICONIDENTIFIER(ctypes.Structure):
    _fields_ = [("cbSize", wintypes.DWORD), ("hWnd", wintypes.HWND),
                ("uID", wintypes.UINT), ("guidItem", ctypes.c_ubyte * 16)]


shell32.Shell_NotifyIconGetRect.restype = ctypes.c_long
shell32.Shell_NotifyIconGetRect.argtypes = [ctypes.POINTER(NOTIFYICONIDENTIFIER), ctypes.POINTER(wintypes.RECT)]


def tray_rect(hwnd: int, icon_id: int, owned_pid: int) -> tuple[int, int, int, int] | None:
    """Only the owned icon, exposed on the taskbar (not hidden overflow).

    GetRect can return an overflow rectangle even while its flyout is closed.
    Require the actual point to belong to the visible primary taskbar too.
    None explicitly means there is no safe real-input target for this proof.
    """
    owner = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
    if owner.value != owned_pid:
        raise ValueError("tray HWND does not belong to the owned HUD")
    identity = NOTIFYICONIDENTIFIER(cbSize=ctypes.sizeof(NOTIFYICONIDENTIFIER), hWnd=hwnd, uID=icon_id)
    rect = wintypes.RECT()
    if shell32.Shell_NotifyIconGetRect(ctypes.byref(identity), ctypes.byref(rect)) < 0:
        return None
    taskbar = user32.FindWindowW("Shell_TrayWnd", None)
    bounds = wintypes.RECT()
    if not taskbar or not user32.IsWindowVisible(taskbar) or not user32.GetWindowRect(taskbar, ctypes.byref(bounds)):
        return None
    x, y = (rect.left + rect.right) // 2, (rect.top + rect.bottom) // 2
    if not (rect.left < rect.right and rect.top < rect.bottom and bounds.left <= x < bounds.right and bounds.top <= y < bounds.bottom):
        return None
    at_point = user32.WindowFromPoint(wintypes.POINT(x, y))
    if at_point != taskbar and not user32.IsChild(taskbar, at_point):
        return None
    return rect.left, rect.top, rect.right, rect.bottom


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


def pointer_capture_state(pid: int) -> dict:
    """Observe the primary overlay, excluding auxiliary windows of the same PID.

    The harness first proves primary scene/physical identity at origin zero.
    A unique visible, topmost window covering that screen identifies the HUD;
    ambiguity remains unknown and includes candidate evidence for diagnosis.
    """
    width, height = screen_size()
    windows = []
    for hwnd in _windows_of(pid):
        rect = wintypes.RECT()
        valid_bounds = user32.GetWindowRect(hwnd, ctypes.byref(rect))
        bounds = [rect.left, rect.top, rect.right, rect.bottom] if valid_bounds else None
        style = user32.GetWindowLongPtrW(hwnd, GWL_EXSTYLE)
        title, class_name = ctypes.create_unicode_buffer(256), ctypes.create_unicode_buffer(256)
        user32.GetWindowTextW(hwnd, title, len(title))
        user32.GetClassNameW(hwnd, class_name, len(class_name))
        windows.append({"hwnd": hwnd, "rect": bounds, "style_ex": style,
                        "title": title.value, "class": class_name.value})
    overlays = [window for window in windows
                if window["rect"] == [0, 0, width, height] and window["style_ex"] & WS_EX_TOPMOST]
    # winit's set_cursor_hittest(false) sets WS_EX_TRANSPARENT. Observe the
    # actual native switch, rather than assuming a hover implies capture.
    capturing = not bool(overlays[0]["style_ex"] & WS_EX_TRANSPARENT) if len(overlays) == 1 else None
    no_activate = bool(overlays[0]["style_ex"] & WS_EX_NOACTIVATE) if len(overlays) == 1 else None
    return {"capturing": capturing, "no_activate": no_activate,
            "matching_overlays": len(overlays), "windows": windows}


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
    if not screen_dc:
        raise OSError("GetDC failed during desktop capture")
    mem_dc = bitmap = previous = None
    try:
        mem_dc = gdi32.CreateCompatibleDC(screen_dc)
        if not mem_dc:
            raise OSError("CreateCompatibleDC failed during desktop capture")
        bitmap = gdi32.CreateCompatibleBitmap(screen_dc, width, height)
        if not bitmap:
            raise OSError("CreateCompatibleBitmap failed during desktop capture")
        selected = gdi32.SelectObject(mem_dc, bitmap)
        if not selected or selected == ctypes.c_void_p(-1).value:
            raise OSError("SelectObject failed during desktop capture")
        previous = selected
        if not gdi32.BitBlt(mem_dc, 0, 0, width, height, screen_dc, 0, 0, 0x00CC0020 | 0x40000000):
            raise OSError("BitBlt failed during desktop capture")
        # GetDIBits requires the bitmap to be deselected from its DC.
        restored = gdi32.SelectObject(mem_dc, previous)
        previous = None
        if not restored or restored == ctypes.c_void_p(-1).value:
            raise OSError("SelectObject restore failed during desktop capture")
        header = BITMAPINFOHEADER(ctypes.sizeof(BITMAPINFOHEADER), width, -height, 1, 32, 0, 0, 0, 0, 0, 0)
        buf = ctypes.create_string_buffer(width * height * 4)
        if gdi32.GetDIBits(mem_dc, bitmap, 0, height, buf, ctypes.byref(header), 0) != height:
            raise OSError("GetDIBits did not capture every desktop row")
    finally:
        if previous:
            gdi32.SelectObject(mem_dc, previous)
        # Delete the DC first, releasing any selected object even after a failure.
        if mem_dc:
            gdi32.DeleteDC(mem_dc)
        if bitmap:
            gdi32.DeleteObject(bitmap)
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
        flags = {1: (LEFT_DOWN, LEFT_UP), 3: (RIGHT_DOWN, RIGHT_UP)}
        if button not in flags:
            raise ValueError(f"button {button}")
        self._send(f"click button {button} x{repeat}", [_mouse(flag) for _ in range(repeat) for flag in flags[button]])

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

