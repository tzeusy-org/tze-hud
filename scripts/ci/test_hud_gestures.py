"""Unit tests for the gesture harness guards and target resolution."""

from __future__ import annotations

import sys
import copy
import contextlib
import ctypes
import hashlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hud_gestures as g  # noqa: E402
import windows_overlay_gestures as overlay  # noqa: E402
from windows_smoke import wait_until  # noqa: E402

STATUS = {
    "pid": 4242,
    "displays": [{"x": 0, "y": 0, "width": 800, "height": 600, "primary": True}],
    "portals": [
        {
            "s": "portal:demo",
            "rect": {"x": 100, "y": 50, "w": 600, "h": 400},
            "panes": {
                "header": {"x": 100, "y": 50, "w": 600, "h": 40},
                "output": {"x": 400, "y": 90, "w": 300, "h": 360},
                "composer": {"x": 100, "y": 90, "w": 295, "h": 360},
                "unread_badge": None,
                "close": {"x": 676, "y": 50, "w": 24, "h": 24},
            },
        }
    ],
}
STATUS["notifications"] = [{
    "s": "zone:notification-area", "key": "card", "state": "shown", "focus": None,
    "rect": {"x": 400, "y": 10, "w": 300, "h": 120},
    "close": {"x": 676, "y": 10, "w": 24, "h": 24},
    "actions": [{"label": "First", "kind": "callback", "rect": {"x": 410, "y": 90, "w": 130, "h": 30}},
                {"label": "Second", "kind": "callback", "rect": {"x": 550, "y": 90, "w": 130, "h": 30}}],
}]
STATUS["widgets"] = [{"s": "widget:main-status", "rect": {"x": 10, "y": 500, "w": 320, "h": 100},
                      "hover_target": {"x": 290, "y": 500, "w": 40, "h": 32}, "tooltip_visible": False}]
OWNED = dict(started_display=":91", target_display=":91", owned_pid=4242, status_pid=4242,
             url="http://127.0.0.1:5555/mcp")


class GuardTests(unittest.TestCase):
    def test_optional_mouse_receipts_bind_identity_and_report_incomplete_capture_without_raising(self) -> None:
        for case in ("captured", "empty", "no-startup", "rotation", "identity", "survivor", "restart", "read-error",
                     "prepare-error", "receipt-error", "oversized", "unparsed", "truncated", "contained-scratch"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as tmp:
                out = Path(tmp)
                exe = out / "tze_hud.exe"
                exe.write_bytes(b"owned test executable")
                capture = g.MouseReceiptCapture(out)
                env = {"TZE_HUD_LOG": "info", "TZE_HUD_LOG_JSON": "1"}
                prepare_error = (mock.patch.object(g.hashlib, "file_digest", side_effect=OSError("unreadable"))
                                 if case == "prepare-error" else contextlib.nullcontext())
                if case == "contained-scratch":
                    (out / "raw").mkdir()
                scratch = (mock.patch.object(g.tempfile, "mkdtemp", return_value=str(out / "raw"))
                           if case == "contained-scratch" else contextlib.nullcontext())
                with mock.patch.object(g.subprocess, "run", return_value=mock.Mock(stdout="a" * 40)), prepare_error, scratch:
                    capture.prepare(env, exe)
                self.assertEqual(env["TZE_HUD_LOG"], "info")
                self.assertEqual(env["TZE_HUD_LOG_JSON"], "1")
                if case in ("prepare-error", "contained-scratch"):
                    self.assertNotIn("TZE_HUD_FILE_LOG", env)
                else:
                    self.assertEqual(env["TZE_HUD_FILE_LOG"], "info,tze_hud_runtime::windowed=debug")
                    self.assertFalse(capture.directory.is_relative_to(out))
                    self.assertEqual(Path(env["TZE_HUD_LOG_DIR"]), capture.directory)
                    (capture.directory / "hud-diag.log").write_bytes(b"UNRELATED_PANIC_PAYLOAD\n")
                    if case != "empty":
                        log = capture.directory / "tze_hud.log"
                        mouse = (b"2026-11-27T12:00:00.123456Z DEBUG tze_hud_runtime::windowed: "
                                 b"main thread: mouse input state=Pressed button=Left secondary=None x=42.0 y=-1.5\n"
                                 b"2026-11-27T12:00:00.124456Z DEBUG tze_hud_runtime::windowed: "
                                 b"main thread: mouse input state=Released button=Left secondary=Some(1) x=42.0 y=-1.5\n")
                        log.write_bytes((b"" if case == "no-startup" else b"INFO log file path=owned\n")
                                        + b"INFO UNRELATED_INFO_PAYLOAD\n" + mouse * (5_001 if case == "truncated" else 1))
                        if case == "rotation":
                            (capture.directory / "tze_hud.log.1").write_bytes(b"older history\n")
                        elif case == "oversized":
                            with log.open("ab") as raw:
                                raw.truncate(13 * 1024 * 1024)
                        elif case == "unparsed":
                            with log.open("ab") as raw:
                                raw.write(b"main thread: mouse input UNPARSED_PAYLOAD\n")
                capture.observe({"pid": 4242, "sha": "b" * 40 if case == "identity" else "a" * 40}, 4242)
                owned = [4242]
                if case == "restart":
                    capture.end_interval()
                    self.assertIn("until", capture.evidence["identities"][0])
                    owned.append(4243)
                    capture.observe({"pid": 4243, "sha": "a" * 40}, 4243)
                read_error = (mock.patch.object(Path, "open", side_effect=OSError("capture unavailable"))
                              if case == "read-error" else contextlib.nullcontext())
                receipt_error = (mock.patch.object(Path, "write_text", side_effect=OSError("receipt unavailable"))
                                 if case == "receipt-error" else contextlib.nullcontext())
                with read_error, receipt_error:
                    receipt = capture.finish(owned, ["hud pid 4242"] if case == "survivor" else [])
                expected = ("captured" if case == "captured" else
                            "unavailable" if case in ("empty", "prepare-error", "read-error", "oversized", "contained-scratch")
                            else "partial")
                self.assertEqual(receipt["capture_status"], expected)
                self.assertTrue(receipt["absence_is_inconclusive"])
                self.assertEqual(receipt["owned_pids"], owned)
                if case == "captured":
                    self.assertEqual(receipt["mouse_records"], 2)
                    self.assertEqual(receipt["exe_sha256"], hashlib.sha256(exe.read_bytes()).hexdigest())
                    self.assertTrue(receipt["identities"][0]["matches"])
                    records = [json.loads(line) for line in (out / "mouse-receipts.jsonl").read_text().splitlines()]
                    self.assertEqual([r["state"] for r in records], ["Pressed", "Released"])
                    self.assertEqual([r["secondary"] for r in records], ["None", "Some(1)"])
                    self.assertEqual(records[0]["timestamp"], "2026-11-27T12:00:00.123456Z")
                    self.assertEqual((records[0]["button"], records[0]["x"], records[0]["y"]), ("Left", "42.0", "-1.5"))
                    self.assertEqual(receipt["source_files"][0]["mouse_records"], 2)
                if case == "truncated":
                    self.assertTrue(receipt["projection_truncated"])
                    self.assertEqual(receipt["projected_mouse_records"], 10_000)
                if capture.directory:
                    self.assertEqual(capture.directory.exists(), case == "survivor")
                    if case == "survivor":
                        g.shutil.rmtree(capture.directory)
                for artifact in out.iterdir():
                    data = artifact.read_bytes()
                    for excluded in (b"UNRELATED_PANIC_PAYLOAD", b"UNRELATED_INFO_PAYLOAD", b"UNPARSED_PAYLOAD"):
                        self.assertNotIn(excluded, data)
                self.assertFalse((out / "tze_hud.log").exists())
                self.assertFalse((out / "hud-diag.log").exists())
                if case.endswith("error"):
                    self.assertTrue(receipt["errors"])

    def test_only_the_resident_witness_enables_grpc_and_it_requires_the_owned_pid(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            for witness, reported_pid, diagnostics in ((False, 4242, False), (True, 4242, False),
                                                       (True, 999, False), (False, 4242, True)):
                with self.subTest(witness=witness, reported_pid=reported_pid, diagnostics=diagnostics):
                    run = g.Run(exe=Path("tze_hud"), out=workspace)
                    if diagnostics:
                        run.mouse_receipts = g.MouseReceiptCapture(workspace)
                    home = workspace / f"home-{witness}-{reported_pid}-{diagnostics}"
                    home.mkdir()
                    xvfb = mock.Mock(**{"poll.return_value": None})
                    hud = mock.Mock(pid=4242, **{"poll.return_value": None})
                    config = workspace / "tze_hud.toml"
                    with mock.patch.object(g.tempfile, "mkdtemp", return_value=str(home)), \
                            mock.patch.object(g, "start_xvfb", return_value=(xvfb, ":91")), \
                            mock.patch.object(g, "seed_config", return_value=config), \
                            mock.patch.object(g.subprocess, "Popen", return_value=hud) as spawn, \
                            mock.patch.object(g, "wait_for_mcp"), \
                            mock.patch.object(g, "XTest") as injector, \
                            mock.patch.object(g.MouseReceiptCapture, "prepare") as prepare, \
                            mock.patch.object(g.Run, "guard"), \
                            mock.patch.object(g.Run, "status", return_value={"pid": reported_pid}):
                        if reported_pid != hud.pid:
                            with self.assertRaises(g.Refused):
                                run.start(resident_raster=witness)
                        else:
                            run.start(resident_raster=witness)
                        command = spawn.call_args.args[0]
                        self.assertEqual(prepare.call_count, int(diagnostics))
                        if diagnostics:
                            self.assertIs(prepare.call_args.args[0], spawn.call_args.kwargs["env"])
                            self.assertEqual(prepare.call_args.args[1], run.exe)
                        grpc_port = int(command[command.index("--grpc-port") + 1])
                        mcp_port = int(command[command.index("--mcp-port") + 1])
                        self.assertGreater(mcp_port, 0)
                        if witness:
                            self.assertGreater(grpc_port, 0)
                            self.assertNotEqual(mcp_port, grpc_port)
                            injector.assert_not_called()
                        else:
                            self.assertEqual(grpc_port, 0)
                            injector.assert_called_once()

    def test_owned_display_pid_and_loopback_pass(self) -> None:
        g.check_owned(**OWNED)

    def test_wrong_display_refuses(self) -> None:
        for target in (":0", None, ""):
            with self.subTest(target=target), self.assertRaisesRegex(g.Refused, "not the Xvfb"):
                g.check_owned(**{**OWNED, "target_display": target})
        with self.assertRaisesRegex(g.Refused, "not the Xvfb"):
            g.check_owned(**{**OWNED, "started_display": None, "target_display": None})

    def test_borrowed_url_refuses(self) -> None:
        with self.assertRaisesRegex(g.Refused, "borrowed"):
            g.check_owned(**{**OWNED, "url": "http://192.0.2.7:9090/mcp"})

    def test_missing_owned_pid_refuses(self) -> None:
        with self.assertRaisesRegex(g.Refused, "no owned HUD"):
            g.check_owned(**{**OWNED, "owned_pid": None})

    def test_foreign_status_pid_refuses(self) -> None:
        # Another HUD answering on the port (borrowed or a stale instance).
        with self.assertRaisesRegex(g.Refused, "not the owned HUD"):
            g.check_owned(**{**OWNED, "status_pid": 999})

    def test_injector_refuses_before_running_xdotool(self) -> None:
        def refuse() -> None:
            raise g.Refused("guard says no")

        injector = g.XTest(":91", refuse)
        with mock.patch.object(g.subprocess, "run") as run:
            for act in (lambda: injector.move_abs(1, 2), lambda: injector.click(), lambda: injector.key("Tab"),
                        lambda: injector.type("x"), lambda: injector.button(True)):
                with self.assertRaises(g.Refused):
                    act()
            run.assert_not_called()

    def test_injector_targets_only_its_display(self) -> None:
        injector = g.XTest(":91", lambda: None)
        with mock.patch.object(g.subprocess, "run") as run:
            injector.click()
        self.assertEqual(run.call_args.kwargs["env"]["DISPLAY"], ":91")

    def test_dead_xvfb_refuses_before_reading_status(self) -> None:
        run = g.Run(exe=Path("tze_hud"), out=Path("."), display=":91")
        run.xvfb = mock.Mock(**{"poll.return_value": 0})
        with mock.patch.object(g.Run, "status") as status, self.assertRaisesRegex(g.Refused, "Xvfb is gone"):
            run.guard()
        status.assert_not_called()


WIN_OWNED = dict(github_actions="true", foreground_pid=4242, owned_hud_pid=4242, helper_pids=[77],
                 status_pid=4242, url="http://127.0.0.1:5555/mcp")


def native_input_boundary():
    """Load the real Win32 boundary with inert DLL handles on either host OS."""
    spec = importlib.util.spec_from_file_location("test_sendinput_boundary", Path(__file__).with_name("hud_sendinput.py"))
    native = importlib.util.module_from_spec(spec)
    with mock.patch.object(ctypes, "WinDLL", create=True):
        spec.loader.exec_module(native)
    return native


class WindowsGuardTests(unittest.TestCase):
    def test_desktop_capture_keeps_pointer_sized_handles_and_releases_resources(self) -> None:
        native = native_input_boundary()
        screen, memory, bitmap, previous = (0x123456780 + n for n in range(4))
        native.screen_size = lambda: (2, 1)
        native.user32.GetDC.return_value = screen
        native.gdi32.CreateCompatibleDC.return_value = memory
        native.gdi32.CreateCompatibleBitmap.return_value = bitmap
        calls = []

        def blit(dst, _x, _y, _w, _h, src, _sx, _sy, _rop):
            calls.append(("blit", dst, src))
            return 1

        def read(dc, image, _start, rows, data, _header, _usage):
            calls.append(("read", dc, image))
            ctypes.memmove(data, b"\x03\x02\x01\x00" * 2, 8)
            return rows

        # Call genuine ctypes C boundaries through the declarations installed by
        # the module. An undeclared API truncates these handles to a C int.
        signatures = {
            "BitBlt": [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                       ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_uint32],
            "GetDIBits": [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32,
                          ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint32],
        }
        callbacks = []
        for name, callback in (("BitBlt", blit), ("GetDIBits", read)):
            actual = ctypes.CFUNCTYPE(ctypes.c_int, *signatures[name])(callback)
            callbacks.append(actual)  # Keep the C callback alive through capture.
            boundary = ctypes.CFUNCTYPE(ctypes.c_int)(ctypes.cast(actual, ctypes.c_void_p).value)
            declaration = getattr(native.gdi32, name).__dict__.get("argtypes")
            if declaration is not None:
                boundary.argtypes = declaration
            setattr(native.gdi32, name, boundary)

        for failure in (None, "blit", "read"):
            with self.subTest(failure=failure):
                calls.clear()
                for api in (native.gdi32.DeleteDC, native.gdi32.DeleteObject, native.user32.ReleaseDC):
                    api.reset_mock()
                native.gdi32.SelectObject.reset_mock()

                def select(dc, image):
                    calls.append(("select", dc, image))
                    return previous if image == bitmap else bitmap

                native.gdi32.SelectObject.side_effect = select
                if failure:
                    api = "BitBlt" if failure == "blit" else "GetDIBits"
                    with mock.patch.object(native.gdi32, api, return_value=0), self.assertRaises(OSError):
                        native.desktop_rgba()
                else:
                    self.assertEqual(native.desktop_rgba(), (2, 1, b"\x01\x02\x03\xff" * 2))
                    self.assertEqual(calls, [("select", memory, bitmap), ("blit", memory, screen),
                                             ("select", memory, previous), ("read", memory, bitmap)])
                self.assertEqual(native.gdi32.SelectObject.call_args_list,
                                 [mock.call(memory, bitmap), mock.call(memory, previous)])
                native.gdi32.DeleteDC.assert_called_with(memory)
                native.gdi32.DeleteObject.assert_called_with(bitmap)
                native.user32.ReleaseDC.assert_called_with(0, screen)

    def test_link_click_checks_the_owned_window_at_the_physical_cursor(self) -> None:
        native = native_input_boundary()
        run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput", owned_pids=[4242])
        hud, browser, process = 0x123456780, 0x123456781, 0x123456782
        target = (891, 106)
        events = []
        run.inj = native.SendInput(lambda: events.append("ownership guard"))
        native.user32.SendInput.side_effect = lambda *args: events.append("SendInput") or args[0]

        def cursor(point):
            events.append("cursor observation")
            if actual is None:
                return 0
            point._obj.x, point._obj.y = actual
            return 1

        def window_at(point):
            self.assertEqual((point.x, point.y), actual)
            return point_hwnd

        def owner(hwnd, pid):
            if hwnd not in (hud, browser):
                return 0
            if hwnd == browser and unavailable == "stale" and native.kernel32.CloseHandle.called:
                return 0  # The sampled window went away during the lookup.
            pid._obj.value = 4242 if hwnd == hud else 9900
            return 1

        def class_for_window(hwnd, buffer, size):
            self.assertEqual(hwnd, browser)
            self.assertEqual(size, 256)
            buffer.value = "Fixture_Window"
            return len(buffer.value)

        def image_for_process(handle, flags, buffer, length):
            self.assertEqual((handle, flags), (process, 0))
            self.assertEqual(length._obj.value, len(buffer))
            if unavailable == "image":
                return 0
            if unavailable == "error":
                raise OSError("diagnostic unavailable")
            buffer.value = r"C:\private-fixture\other-user\point-owner.exe"
            length._obj.value = len(buffer.value)
            return 1

        native.user32.GetCursorPos.side_effect = cursor
        native.user32.WindowFromPoint.side_effect = window_at
        native.user32.GetWindowThreadProcessId.side_effect = owner
        native.user32.GetClassNameW.side_effect = class_for_window
        native.kernel32.QueryFullProcessImageNameW.side_effect = image_for_process
        diagnostics = {"window_class": "Fixture_Window", "image_basename": "point-owner.exe"}
        cases = (
            ("owned", (892, 105), hud, hud, None, None),
            ("foreign point", target, browser, hud, None, diagnostics),
            ("access denied", target, browser, hud, "process", {"window_class": "Fixture_Window"}),
            ("image unavailable", target, browser, hud, "image", {"window_class": "Fixture_Window"}),
            ("native lookup error", target, browser, hud, "error", None),
            ("stale foreign window", target, browser, hud, "stale", None),
            ("late foreign foreground", target, hud, browser, None, None),
            ("unknown pointer window", target, None, hud, None, None),
            ("unknown cursor", None, None, hud, None, None),
            ("wrong point", (10, 20), browser, hud, None, None),
        )
        for case, actual, point_hwnd, foreground, unavailable, expected_details in cases:
            with self.subTest(case=case), mock.patch.object(g, "WIN", native, create=True), \
                    mock.patch.object(native.ctypes, "get_last_error", return_value=0, create=True):
                events.clear()
                run.timeline.clear()
                run.inj.calls.clear()
                for call in (native.user32.GetClassNameW, native.kernel32.OpenProcess,
                             native.kernel32.QueryFullProcessImageNameW, native.kernel32.CloseHandle):
                    call.reset_mock()
                native.kernel32.OpenProcess.return_value = 0 if unavailable == "process" else process
                native.user32.GetForegroundWindow.return_value = foreground
                if case == "owned":
                    run.click_link(target, repeat=2)
                    self.assertEqual(events, ["ownership guard", "cursor observation", "SendInput"])
                    self.assertEqual(run.inj.calls[-1]["sent"], 4)
                else:
                    with self.assertRaises(g.ScenarioFailed) as refusal:
                        run.click_link(target, repeat=2)
                    self.assertEqual(events, ["ownership guard", "cursor observation"])
                    self.assertEqual(run.inj.calls, [])
                    self.assertEqual(refusal.exception.step, "owned HUD at the link click target")
                    self.assertEqual(refusal.exception.last.get("point_owner_details"), expected_details)
                if expected_details or unavailable in {"error", "stale"}:
                    native.kernel32.OpenProcess.assert_called_once_with(0x1000, False, 9900)
                    if unavailable == "process":
                        native.kernel32.QueryFullProcessImageNameW.assert_not_called()
                        native.kernel32.CloseHandle.assert_not_called()
                    else:
                        native.kernel32.CloseHandle.assert_called_once_with(process)
                else:
                    native.user32.GetClassNameW.assert_not_called()
                    native.kernel32.OpenProcess.assert_not_called()
                observation = run.timeline[-1]["click_target_before_send"]
                self.assertEqual(observation["foreground"]["hwnd"], foreground)
                self.assertEqual(observation["cursor"], list(actual) if actual else None)
                window = observation["at_cursor"]
                if point_hwnd is None:
                    self.assertIsNone(window)
                else:
                    self.assertEqual(window, {"hwnd": point_hwnd, "pid": 4242 if point_hwnd == hud else 9900})
        native.user32.GetWindowTextW.assert_not_called()

        events.clear()
        run.timeline.clear()
        with mock.patch.object(run.inj, "guard", side_effect=g.Refused("foreign foreground")), \
                self.assertRaises(g.Refused):
            run.click_link(target)
        self.assertEqual(events, [])
        self.assertEqual(run.timeline, [])

    def test_card_link_rearms_before_any_input_and_refuses_unstable_targets(self) -> None:
        hud, browser, auxiliary = 0x123456780, 0x123456781, 0x123456782
        point = (891, 106)
        status = {"pid": 4242, "notifications": [{
            "s": "zone:notification-area", "key": "gesture-card-link", "state": "shown",
            "actions": [{"label": "Open test page", "kind": "url", "rect": {"x": 784, "y": 90, "w": 214, "h": 32}}],
        }]}
        changed = copy.deepcopy(status)
        changed["notifications"][0]["actions"][0]["rect"]["x"] += 4
        mapped_point_loss = ("late point recovers", "late move recovers", "late point persists", "HUD replaced after late loss",
                             "HUD PID changed after late loss", "card moved after late loss")
        cases = ("covered", "brief native safe sample", "foreground lost before publish", "deadline",
                 "expired during native recheck", "expired during guard",
                 "expired at cursor sample", "stale HUD handle", "card moved", "late takeover", "late cursor recovers",
                 "late foreground recovers", "zero events", "partial click", "native result unavailable", *mapped_point_loss)
        for case in cases:
            with self.subTest(case=case):
                native = native_input_boundary()
                run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput", owned_pids=[4242])
                run.smoke = mock.Mock(url="http://127.0.0.1:5555/mcp")
                run.inj = native.SendInput(run.guard)
                now = 0.0
                recovered_at = float("inf")
                scenario_entry = case == "foreground lost before publish"
                foreground = browser if scenario_entry else hud
                initially_parked = {
                    "covered", "brief native safe sample", "deadline", "stale HUD handle", "card moved",
                    "expired during native recheck", "late move recovers",
                }
                cursor = (1, 1) if case in initially_parked or scenario_entry else point
                cursor_samples = 0
                point_samples = 0
                published = False
                presses = []

                def sleep(seconds):
                    nonlocal now
                    now += seconds

                def status_at():
                    nonlocal now
                    if case == "expired during guard" and cursor_samples:
                        now = 0.1
                    if scenario_entry:
                        return {
                            **status, "notifications": status["notifications"] if published else [],
                            "frames_presented": int(published), "idle_since_ms": 500,
                            "link_launches": int(bool(presses)), "link_feedback": "opened" if presses else None,
                        }
                    if case == "HUD PID changed after late loss" and point_samples >= 4:
                        return {**status, "pid": 9999}
                    if (case == "card moved" and now >= 0.04
                            or case == "card moved after late loss" and point_samples >= 4):
                        return changed
                    return status

                def tool(name, _payload, **_kwargs):
                    nonlocal published
                    if name == "hud_publish":
                        self.assertEqual(foreground, browser)
                        native.user32.SendInput.assert_not_called()
                        published = True
                    else:
                        self.assertEqual(name, "hud_input")
                    return False, {}

                def window_pid(handle, output):
                    self.assertIn(handle, (hud, browser, auxiliary))
                    output._obj.value = 9900 if handle == browser or case == "stale HUD handle" else 4242
                    return 1

                def bounds(handle, output):
                    rect = output._obj
                    rect.left, rect.top, rect.right, rect.bottom = (0, 0, 1024 if handle == hud else 50, 768)
                    return True

                def raise_window(*args):
                    nonlocal recovered_at
                    self.assertEqual(args, (hud, 0, 0, 0, 0, 0, 0x4213))
                    if case not in {"deadline", "late point persists"}:
                        recovered_at = now + 0.02
                    return 1  # An accepted request alone must not authorize input.

                def foreground_window(handle):
                    nonlocal foreground
                    self.assertEqual(handle, hud)
                    foreground = hud
                    return 1

                def cursor_pos(output):
                    nonlocal cursor_samples, foreground, now
                    cursor_samples += 1
                    output._obj.x, output._obj.y = cursor
                    if case == "expired at cursor sample" and cursor_samples == 2:
                        now = 0.1
                    if case == "late foreground recovers" and cursor_samples == 1:
                        foreground = browser
                    return 1

                def window_at(coords):
                    nonlocal now, point_samples, recovered_at
                    point_samples += 1
                    if case == "expired during native recheck" and point_samples == 2:
                        now = 0.1
                    if case == "brief native safe sample" and point_samples == 2:
                        return hud  # The first rearm's own check sees no work to do.
                    # Samples 1–2 establish readiness. The pre-motion cursor read is third,
                    # followed by the mapped-point guard and finally the button cursor guard.
                    if ((case in mapped_point_loss and point_samples == 4)
                            or (case == "late cursor recovers" and point_samples == 5)):
                        recovered_at = float("inf")
                    return browser if ((coords.x, coords.y) == point
                                       and (now < recovered_at or (case == "late takeover" and cursor_samples >= 2))) else hud

                def owned_windows(_pid):
                    return [auxiliary] if case == "HUD replaced after late loss" and point_samples >= 4 else [auxiliary, hud]

                def send(count, items, _size):
                    nonlocal cursor
                    if items[0].mi.dwFlags & native.MOUSE_MOVE:
                        self.assertGreaterEqual(now, recovered_at + 0.02 - 1e-9,
                                                "the pointer must wait for consecutive owned-point samples")
                        cursor = point
                        return count
                    if not scenario_entry:
                        self.assertLess(now, 0.1)
                    self.assertEqual(foreground, hud)
                    self.assertGreaterEqual(now, recovered_at + (0.02 if recovered_at else 0) - 1e-9)
                    presses.append((now, count))
                    if case == "native result unavailable":
                        raise OSError("native result unavailable")
                    return {"partial click": 1, "zero events": 0}.get(case, count)

                if cursor == point or case == "late move recovers":
                    recovered_at = 0.0
                native.user32.IsWindowVisible.return_value = True
                native.user32.GetWindowThreadProcessId.side_effect = window_pid
                native.user32.GetWindowRect.side_effect = bounds
                native.user32.GetForegroundWindow.side_effect = lambda: foreground
                native.user32.GetCursorPos.side_effect = cursor_pos
                native.user32.WindowFromPoint.side_effect = window_at
                native.user32.SetForegroundWindow.side_effect = foreground_window
                native.user32.SetWindowPos.side_effect = raise_window
                native.user32.GetClassNameW.return_value = 0
                native.kernel32.OpenProcess.return_value = 0
                native.user32.SendInput.side_effect = send
                run.smoke.call.side_effect = tool
                with mock.patch.object(g, "WIN", native), mock.patch.object(run, "status", side_effect=status_at), \
                        mock.patch.object(native, "_windows_of", side_effect=owned_windows), \
                        mock.patch.object(native, "screen_size", return_value=(1024, 768)), \
                        mock.patch.object(native.ctypes, "get_last_error", return_value=0, create=True), \
                        mock.patch.object(g, "pid_alive", return_value=True), \
                        mock.patch.dict(g.os.environ, {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"}), \
                        mock.patch.object(g.time, "monotonic", side_effect=lambda: now), \
                        mock.patch.object(g.time, "sleep", side_effect=sleep):
                    def attempt():
                        return g.press_hosted_card_link(run, "gesture-card-link", "Open test page", point,
                                                        deadline=0.1, press="second press")

                    recovery_phases = {
                        "late point recovers": "button mapped point",
                        "late move recovers": "move mapped point",
                        "late cursor recovers": "button cursor",
                        "late foreground recovers": "button foreground guard",
                    }
                    if scenario_entry:
                        g.scenario_card_link(run)
                        self.assertTrue(published)
                        self.assertEqual([call["sent"] for call in run.inj.calls], [1, 2, 2])
                        self.assertLess(presses[1][0] - presses[0][0], 1.8)
                        native.user32.SetForegroundWindow.assert_called_once_with(hud)
                        native.user32.SetWindowPos.assert_called_once()
                    elif case in {"covered", "brief native safe sample"}:
                        self.assertEqual(attempt(), presses[0][0])
                        self.assertEqual([call["requested"] for call in run.inj.calls], [1, 2])
                        self.assertEqual([count for _, count in presses], [2])
                        native.user32.SetWindowPos.assert_called_once()
                        native.user32.SetForegroundWindow.assert_not_called()
                    elif case in recovery_phases:
                        self.assertEqual(attempt(), presses[0][0])
                        sent = [1, 2] if case == "late move recovers" else [2]
                        self.assertEqual([call["sent"] for call in run.inj.calls], sent)
                        self.assertEqual([count for _, count in presses], [2])
                        self.assertEqual(native.user32.SendInput.call_count, len(sent))
                        lost, rearm, final = run.timeline[-3:]
                        self.assertEqual(lost["guard_phase"], recovery_phases[case])
                        self.assertEqual(final["step"], "link click target before SendInput")
                        if case in mapped_point_loss:
                            self.assertEqual(len(run.timeline), 3, "first refusal precedes the final cursor snapshot")
                            self.assertEqual(lost["observed"]["card_link_point"], {
                                "at_point": {"hwnd": browser, "pid": 9900}, "foreground": {"hwnd": hud, "pid": 4242},
                            })
                        if case == "late foreground recovers":
                            self.assertEqual(rearm["step"], "card link second press requested activate")
                            native.user32.SetForegroundWindow.assert_called_once_with(hud)
                            native.user32.SetWindowPos.assert_not_called()
                        else:
                            self.assertEqual(rearm["step"], "card link second press requested raise")
                            native.user32.SetWindowPos.assert_called_once()
                            native.user32.SetForegroundWindow.assert_not_called()
                    elif case == "HUD PID changed after late loss":
                        with self.assertRaisesRegex(g.Refused, "is not the owned HUD pid"):
                            attempt()
                        native.user32.SendInput.assert_not_called()
                        native.user32.SetWindowPos.assert_not_called()
                    elif case == "native result unavailable":
                        with self.assertRaisesRegex(OSError, "native result unavailable"):
                            attempt()
                        native.user32.SendInput.assert_called_once()
                        self.assertEqual([count for _, count in presses], [2])
                        self.assertEqual(run.inj.calls, [])
                    elif case == "partial click":
                        later = mock.Mock()
                        def card(_run):
                            attempt()

                        with mock.patch.object(run, "snapshot"), \
                                mock.patch.dict(g.SCENARIOS, {"card": card, "later": later}, clear=True), \
                                self.assertRaises(g.PartialButtonBatch) as partial:
                            g.run_scenarios(run, ["card", "later"])
                        self.assertEqual([r["outcome"] for r in partial.exception.results], ["FAIL"])
                        later.assert_not_called()
                        self.assertEqual([count for _, count in presses], [2], "the click must not be retried")
                        self.assertEqual(run.inj.calls[-1]["sent"], 1)
                    else:
                        with self.assertRaises(g.ScenarioFailed) as failed:
                            attempt()
                        if case == "zero events":
                            self.assertEqual(failed.exception.last["sendinput"]["sent"], 0)
                            native.user32.SendInput.assert_called_once()
                        else:
                            self.assertEqual(presses, [])
                            native.user32.SendInput.assert_not_called()
                        if case in {"deadline", "expired during native recheck", "expired during guard",
                                    "expired at cursor sample", "stale HUD handle", "late point persists", "late takeover"}:
                            self.assertEqual(now, 0.1)
                        if case in {"deadline", "late point persists"}:
                            self.assertEqual(failed.exception.last["card_link_point"]["at_point"]["pid"], 9900)
                            native.user32.SetWindowPos.assert_called_once()
                        elif case in {"stale HUD handle", "expired during native recheck"}:
                            native.user32.SetWindowPos.assert_not_called()
                            native.user32.SetForegroundWindow.assert_not_called()
                        elif case in {"card moved", "card moved after late loss"}:
                            self.assertEqual(failed.exception.last["current_point"], (895, 106))
                            if case == "card moved after late loss":
                                native.user32.SetWindowPos.assert_not_called()
                        elif case == "HUD replaced after late loss":
                            self.assertEqual(failed.exception.step, "same owned fullscreen HUD before second press")
                            native.user32.SetWindowPos.assert_not_called()
                        elif case == "late takeover":
                            losses = [step for step in run.timeline if step.get("guard_phase") == "button cursor"]
                            self.assertEqual(len(losses), 1)
                            self.assertEqual(losses[0]["observed"]["click_target_before_send"]["at_cursor"]["pid"], 9900)

    def test_a_refused_guard_keeps_earlier_outcomes(self) -> None:
        run = mock.Mock(out=Path("unused"))
        run.clear_publications.return_value = []
        first = mock.Mock(__doc__="first scenario")

        def second(_run):
            raise g.Refused("the foreground window belongs to pid 7, not an owned process")

        third = mock.Mock(__doc__="third scenario")
        with mock.patch.dict(g.SCENARIOS, {"first": first, "second": second, "third": third}, clear=True), \
                contextlib.redirect_stdout(io.StringIO()), self.assertRaises(g.Refused) as refused:
            g.run_scenarios(run, ["first", "second", "third"])
        outcomes = [(r["scenario"], r["outcome"]) for r in refused.exception.results]
        self.assertEqual(outcomes, [("first", "PASS"), ("second", "FAIL")])
        self.assertIn("not an owned process", refused.exception.results[1]["step"])
        third.assert_not_called()

    def test_failed_diagnostic_keeps_original_result_and_next_scenario(self) -> None:
        cases = [(g.ScenarioFailed("original failure", {"marker": 1}),
                  "original failure", {"marker": 1})]
        for stderr in ("x" * 5000 + "provider failure", b"provider \xff failure", None):
            expected = stderr.decode("utf-8", errors="replace") if isinstance(stderr, bytes) else stderr or ""
            cases.append((g.subprocess.CalledProcessError(1, ["powershell.exe", "-Command", "script"],
                                                        stderr=stderr),
                          "CalledProcessError: powershell.exe exited 1",
                          {"exit_code": 1, "stderr": expected[-4096:]}))
        for failure, step, last in cases:
            with self.subTest(failure=type(failure).__name__, stderr=getattr(failure, "stderr", None)):
                run = mock.Mock(out=Path("unused"))
                run.clear_publications.return_value = []
                run.screen_grab.side_effect = OSError("capture unavailable")

                def first(_run):
                    raise failure

                second = mock.Mock(__doc__="second scenario")
                with mock.patch.dict(g.SCENARIOS, {"first": first, "second": second}, clear=True), \
                        contextlib.redirect_stdout(io.StringIO()):
                    results = g.run_scenarios(run, ["first", "second"])
                self.assertEqual([result["outcome"] for result in results], ["FAIL", "PASS"])
                self.assertEqual(results[0]["last"], last)
                self.assertEqual(results[0]["step"], step)
                self.assertIn("capture unavailable", results[0]["diagnostic_error"])
                second.assert_called_once_with(run)

    def test_owned_hud_or_helper_foreground_passes(self) -> None:
        g.check_owned_windows(**WIN_OWNED)
        g.check_owned_windows(**{**WIN_OWNED, "foreground_pid": 77})

    def test_refuses_off_a_ci_vm(self) -> None:
        for value in (None, "", "false", "1"):
            with self.subTest(value=value), self.assertRaisesRegex(g.Refused, "only on a CI VM"):
                g.check_owned_windows(**{**WIN_OWNED, "github_actions": value})

    def test_refuses_a_foreground_window_it_does_not_own(self) -> None:
        with self.assertRaisesRegex(g.Refused, "foreground window belongs to pid 9"):
            g.check_owned_windows(**{**WIN_OWNED, "foreground_pid": 9})

    def test_refuses_borrowed_url_and_foreign_or_missing_hud(self) -> None:
        with self.assertRaisesRegex(g.Refused, "borrowed"):
            g.check_owned_windows(**{**WIN_OWNED, "url": "http://192.0.2.7:9090/mcp"})
        with self.assertRaisesRegex(g.Refused, "not the owned HUD"):
            g.check_owned_windows(**{**WIN_OWNED, "status_pid": 999})
        with self.assertRaisesRegex(g.Refused, "no owned HUD"):
            g.check_owned_windows(**{**WIN_OWNED, "owned_hud_pid": None})

    def test_a_refused_start_never_kills_a_running_hud(self) -> None:
        # Refusing because someone's HUD is running must not then "clean it up".
        run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput")
        with mock.patch.object(g, "WINDOWS", True), \
                mock.patch.object(g, "windows_huds", return_value=[1234]), \
                mock.patch.dict(g.os.environ, {"GITHUB_ACTIONS": "true"}), \
                mock.patch.object(g, "kill_pid") as kill:
            with self.assertRaisesRegex(g.Refused, "already running"):
                run.start()
            self.assertEqual(run.stop() if run.home else [], [])
            self.assertEqual(run._strays(), [])
        kill.assert_not_called()

    def test_start_refuses_off_a_ci_vm_before_creating_anything(self) -> None:
        run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput")
        with mock.patch.dict(g.os.environ, {"GITHUB_ACTIONS": ""}), \
                mock.patch.object(g, "windows_huds") as huds:
            with self.assertRaisesRegex(g.Refused, "only on a CI VM"):
                run.start()
        huds.assert_not_called()
        self.assertIsNone(run.home)


    def test_sendinput_guard_runs_before_every_injection_including_relative_motion(self) -> None:
        # Load the real boundary with fake DLL handles; no OS input is possible.
        native = native_input_boundary()
        injector = native.SendInput(lambda: g.check_owned_windows(**{**WIN_OWNED, "foreground_pid": 9}))
        actions = (lambda: injector.move_abs(1, 2), lambda: injector.move_relative(3, 4),
                   lambda: injector.click(), lambda: injector.click(3), lambda: injector.button(True), lambda: injector.key("Tab"),
                   lambda: injector.type_unicode("x"))
        for action in actions:
            with self.assertRaises(g.Refused):
                action()
        native.user32.SendInput.assert_not_called()

    def test_native_capture_observes_only_a_unique_primary_overlay(self) -> None:
        native = native_input_boundary()
        for case in ("capture", "passthrough", "activating", "no-overlay", "ambiguous"):
            with self.subTest(case=case):
                rows = {11: ([5, 5, 100, 100], 0), 12: ([0, 0, 1024, 768], 0)}
                if case != "no-overlay":
                    rows[13] = ([0, 0, 1024, 768], 0x8 | (0 if case == "activating" else 0x08000000) | (0x20 if case == "passthrough" else 0))
                if case == "ambiguous":
                    rows[14] = ([0, 0, 1024, 768], 0x8)

                def bounds(hwnd, output):
                    rect = output._obj
                    rect.left, rect.top, rect.right, rect.bottom = rows[hwnd][0]
                    return True

                native.user32.GetWindowRect.side_effect = bounds
                native.user32.GetWindowLongPtrW.side_effect = lambda hwnd, _: rows[hwnd][1]
                with mock.patch.object(native, "_windows_of", return_value=list(rows)), \
                        mock.patch.object(native, "screen_size", return_value=(1024, 768)):
                    result = native.pointer_capture_state(4242)
                expected = {"capture": True, "passthrough": False, "activating": True, "no-overlay": None, "ambiguous": None}[case]
                self.assertIs(result["capturing"], expected)
                self.assertIs(result["no_activate"], None if case in {"no-overlay", "ambiguous"} else case != "activating")
                self.assertEqual(len(result["windows"]), len(rows))
                self.assertEqual(result["matching_overlays"], 0 if case == "no-overlay" else 2 if case == "ambiguous" else 1)

    def test_alt_tab_keeps_bounded_foreground_evidence_on_timeout(self) -> None:
        run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput",
                    owned_pids=[4242], helper_pids=[77, 78, 79])
        before = {"hwnd": 0x123456780, "pid": 77}
        foreign = {"hwnd": 0x123456781, "pid": 9900}
        win = mock.Mock()
        win.foreground_identity.side_effect = [before, foreign]
        alive = {4242, 77, 78}
        with mock.patch.object(g, "WIN", win, create=True), \
                mock.patch.object(g, "pid_alive", side_effect=alive.__contains__), \
                mock.patch.object(g, "SETTLE_S", 0), \
                mock.patch.object(run, "key") as key, \
                self.assertRaisesRegex(g.ScenarioFailed, "real Alt-Tab reaches an owned editor") as failure:
            g.alt_tab_to_owned_editor(run, {77, 78}, 2)
        key.assert_called_once_with("alt+Tab")
        self.assertEqual(run.timeline[-1]["alt_tab_foreground"], failure.exception.last)
        self.assertEqual(failure.exception.last, {
            "attempt": 2, "before": before, "last": foreign, "eligible_editor_pids": [77, 78],
            "live_owned_pids_before": [77, 78, 4242], "live_owned_pids_after": [77, 78, 4242],
        })

    def test_tray_prepares_only_foreign_foreground_and_keeps_the_input_guard(self) -> None:
        status = {"pid": 4242, "hide_hotkey": {"registered": True, "chord": "Ctrl+Shift+F8"},
                  "tray": {"registered": True, "hwnd": 99, "icon_id": 1, "tooltip": "First\nSecond"}}
        for initial, steals_after_prep in ((9900, False), (77, False), (9900, True)):
            with self.subTest(initial=initial, steals_after_prep=steals_after_prep):
                run = g.Run(exe=Path("tze_hud.exe"), out=Path("."), injector="sendinput",
                            owned_pids=[4242], helper_pids=[77])
                run.smoke = mock.Mock(url="http://127.0.0.1:5555/mcp")
                foreground = initial
                events = []
                win = mock.Mock()
                win.foreground_pid.side_effect = lambda: foreground
                win.tray_rect.return_value = None

                def bring_to_front(pid):
                    nonlocal foreground
                    self.assertEqual(pid, run.pid)
                    foreground = pid
                    events.append(("foreground", pid))

                def mark():
                    nonlocal foreground
                    if steals_after_prep:
                        foreground = 9900

                def guarded_key(chord):
                    run.guard()
                    events.append((chord, foreground))

                win.bring_to_front.side_effect = bring_to_front
                run.inj = mock.Mock()
                run.inj.key.side_effect = guarded_key
                with mock.patch.object(g, "WIN", win, create=True), \
                        mock.patch.object(g, "pid_alive", return_value=True), \
                        mock.patch.dict(g.os.environ, {"GITHUB_ACTIONS": "true"}), \
                        mock.patch.object(run, "mark", side_effect=mark), \
                        mock.patch.object(run, "status", return_value=status), \
                        mock.patch.object(run, "expect", return_value=status), \
                        mock.patch.object(run, "settle", return_value={"frames_presented": 1, "idle_since_ms": 300}), \
                        mock.patch.object(run, "observe_for"):
                    if steals_after_prep:
                        with self.assertRaisesRegex(g.Refused, "foreground window belongs to pid 9900"):
                            g.scenario_tray(run)
                        self.assertEqual(events, [("foreground", 4242)])
                        win.tray_rect.assert_not_called()
                    else:
                        g.scenario_tray(run)
                        if initial == 77:
                            self.assertEqual(events, [("ctrl+shift+f8", 77), ("ctrl+shift+f8", 77),
                                                      ("foreground", 4242)])
                        else:
                            self.assertEqual(events, [("foreground", 4242), ("ctrl+shift+f8", 4242),
                                                      ("ctrl+shift+f8", 4242)])

    def test_tray_rect_requires_documented_success_and_owned_icon(self) -> None:
        native = native_input_boundary()

        def owner(_hwnd, output):
            output._obj.value = 4242
            return 1

        def bounds(_hwnd, output):
            rect = output._obj
            rect.left, rect.top, rect.right, rect.bottom = (0, 728, 1024, 768)
            return True

        native.user32.GetWindowThreadProcessId.side_effect = owner
        native.user32.FindWindowW.return_value = 11
        native.user32.IsWindowVisible.return_value = True
        native.user32.GetWindowRect.side_effect = bounds
        native.user32.WindowFromPoint.return_value = 12
        native.user32.IsChild.return_value = True
        tooltip = "No questions\nNo agents"
        identity = {"name": tooltip, "control_type": "ControlType.Button",
                    "offscreen": False, "enabled": True, "rect": [800, 728, 824, 760]}
        with mock.patch.object(native.subprocess, "run", return_value=mock.Mock(stdout=json.dumps(identity))) as query:
            for hresult in (-1, 1, 0):
                def locate(_identity, output):
                    rect = output._obj
                    rect.left, rect.top, rect.right, rect.bottom = (800, 728, 824, 760)
                    return hresult

                native.shell32.Shell_NotifyIconGetRect.side_effect = locate
                with self.subTest(hresult=hresult):
                    self.assertEqual(native.tray_rect(99, 1, 4242, expected_tooltip=tooltip),
                                     (800, 728, 824, 760) if hresult == 0 else None)
            # S_OK and the taskbar parent also describe the overflow chevron.
            # Only the visible, enabled button carrying our tooltip is ours.
            for change in ({"name": "Show hidden icons"}, {"name": "another app"},
                           {"offscreen": True}, {"enabled": False},
                           {"control_type": "ControlType.Pane"},
                           {"rect": [0, 0, 10, 10]}):
                query.return_value.stdout = json.dumps({**identity, **change})
                with self.subTest(identity=change):
                    self.assertIsNone(native.tray_rect(99, 1, 4242, expected_tooltip=tooltip))
            query.return_value.stdout = json.dumps({**identity, "name": "No questions\r\nNo agents"})
            self.assertEqual(native.tray_rect(99, 1, 4242, expected_tooltip=tooltip), (800, 728, 824, 760))
            query.side_effect = native.subprocess.TimeoutExpired("powershell.exe", 10)
            with self.assertRaises(native.subprocess.TimeoutExpired):
                native.tray_rect(99, 1, 4242, expected_tooltip=tooltip)
            query.reset_mock()
            with self.assertRaises(ValueError):
                native.tray_rect(99, 1, 9, expected_tooltip=tooltip)
            query.assert_not_called()

    def test_modes_select_only_their_scenarios_and_refuse_mismatches(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fake = mock.Mock(home=None, log=None, display="", port=0)
            fake.exe = Path("target/release-dev/tze_hud.exe")
            fake.timeline = []
            fake.owned_pids = []
            fake.pid = 4242
            fake.status.return_value = {}
            fake.inj = None
            fake.smoke.ok.return_value = {}
            for mode, diagnostics in (("fullscreen", False), ("overlay", False), ("fullscreen", True)):
                with self.subTest(mode=mode, diagnostics=diagnostics), mock.patch.object(g, "WINDOWS", True), \
                        mock.patch.object(g, "Run", return_value=fake), \
                        mock.patch.object(g, "run_scenarios", return_value=[]) as scenarios, \
                        mock.patch.object(g, "write_step_summary"), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(g.main(["--exe", "unused.exe", "--injector", "sendinput",
                                             "--window-mode", mode, "--out", directory,
                                             *(["--mouse-receipt-diagnostics"] if diagnostics else [])]), 0)
                    timeline = json.loads((Path(directory) / "timeline.json").read_text())
                    self.assertEqual("mouse_receipts" in timeline, diagnostics)
                    if diagnostics:
                        self.assertEqual(timeline["mouse_receipts"]["capture_status"], "unavailable")
                    selected = scenarios.call_args.args[1]
                    self.assertEqual("overlay-pointer" in selected, mode == "overlay")
                    self.assertEqual("overlay-shell" in selected, mode == "overlay")
                    self.assertEqual("tray" in selected, mode == "overlay")
                    self.assertEqual("tray-answer" in selected, mode == "overlay")
                    self.assertEqual("go-to-session" in selected, mode == "overlay")
                    self.assertEqual("card-copy" in selected, mode == "fullscreen")
                    self.assertEqual("card-reply" in selected, mode == "fullscreen")
                    self.assertEqual("card-reveal" in selected, mode == "fullscreen")
                    self.assertEqual(len(selected), len(g.OVERLAY_SCENARIOS) if mode == "overlay" else 33)
                    # Real link handoffs can foreground a browser late, so they run
                    # after every other injecting scenario; MCP-only restart stays last.
                    self.assertEqual(selected[-3:] == ["portal-reading", "card-link", "restart"],
                                     mode == "fullscreen")
                    if mode == "overlay":
                        self.assertEqual(g.main(["--exe", "unused.exe", "--injector", "sendinput",
                                                 "--window-mode", mode, "--out", directory,
                                                 "overlay-shell", "tray", "overlay-pointer", "go-to-session"]), 0)
                        self.assertEqual(scenarios.call_args.args[1],
                                         ["tray", "overlay-pointer", "go-to-session", "overlay-shell"])
            for mode, scenario in (("fullscreen", "overlay-pointer"), ("fullscreen", "overlay-shell"),
                                   ("overlay", "composer")):
                with mock.patch.object(g, "WINDOWS", True), contextlib.redirect_stderr(io.StringIO()), \
                        self.assertRaises(SystemExit) as failure:
                    g.main(["--exe", "unused.exe", "--injector", "sendinput", "--window-mode", mode, scenario])
                self.assertEqual(failure.exception.code, 2)


class RequiredGateTests(unittest.TestCase):
    """hud-ljhku.16: only required scenarios (and harness failures that cost one its PASS) fail the run."""

    def gate(self, required, outcomes, *, refuse=None, mode="fullscreen", refusal=g.Refused, diagnostics=False):
        """`refuse` is how many scenarios finish before the harness refuses."""
        with tempfile.TemporaryDirectory() as directory:
            fake = mock.Mock(home=Path(directory), log=None, display="", port=0)
            fake.exe = Path("target/release-dev/tze_hud.exe")
            fake.timeline = []
            fake.owned_pids = []
            fake.pid = 4242
            fake.status.return_value = {}
            fake.inj = None
            fake.smoke.ok.return_value = {}
            fake.stop.return_value = []

            def scenarios(_run, names):
                results = [{"scenario": n, "outcome": outcomes.get(n, "PASS"), "seconds": 0} for n in names]
                if refuse is not None:
                    err = refusal("input stopped")
                    err.results = results[:refuse]
                    raise err
                return results

            gate = [] if required is None else [f"--required={required}"]
            with mock.patch.object(g, "WINDOWS", True), mock.patch.object(g, "Run", return_value=fake), \
                    mock.patch.object(g, "run_scenarios", side_effect=scenarios) as run, \
                    contextlib.redirect_stdout(io.StringIO()):
                code = g.main(["--exe", "unused.exe", "--injector", "sendinput", "--window-mode", mode,
                               "--out", directory, *gate,
                               *(["--mouse-receipt-diagnostics"] if diagnostics else [])])
            verdict = json.loads((Path(directory) / "verdict.json").read_text())
            fake.stop.assert_called_once_with()
            return code, run.call_args.args[1] if run.call_args else None, verdict

    def test_required_scenarios_run_first_and_informational_follow_in_the_same_boot(self) -> None:
        _, names, _ = self.gate("keyboard,restart,composer", {})
        self.assertEqual(names[:2], ["composer", "keyboard"])
        self.assertEqual(names[-3:], ["portal-reading", "card-link", "restart"])
        self.assertEqual(len(names), len(set(names)))

    def test_exit_codes(self) -> None:
        cases = [
            ("informational failure passes and is listed", "composer", {"keyboard": "FAIL"}, None, 0),
            ("required failure fails", "composer", {"composer": "FAIL"}, None, 1),
            ("harness failure before a required scenario ran fails", "composer", {}, 0, 1),
            ("harness failure after every required scenario passed passes", "composer", {}, 1, 0),
            ("harness failure with nothing required in this mode passes", "overlay-pointer", {}, 0, 0),
            ("no --required keeps every scenario required", None, {"keyboard": "FAIL"}, None, 1),
        ]
        for label, required, outcomes, refuse, expected in cases:  # refuse: scenarios done first
            for diagnostics in (False, True):
                with self.subTest(label=label, diagnostics=diagnostics):
                    code, _, verdict = self.gate(required, outcomes, refuse=refuse, diagnostics=diagnostics)
                    self.assertEqual(code, expected)
                    self.assertEqual(verdict["blocking"], bool(expected))
        _, _, verdict = self.gate("composer", {"keyboard": "FAIL"})
        self.assertEqual(verdict["informational_failed"], ["keyboard"])
        code, _, verdict = self.gate("", {"tray": "FAIL"}, mode="overlay")
        self.assertEqual(code, 0)
        self.assertEqual(verdict["required"], [])
        self.assertEqual(verdict["informational_failed"], ["tray"])
        for refusal in (g.Refused, g.PartialButtonBatch):
            with self.subTest(refusal=refusal):
                code, _, verdict = self.gate("", {}, refuse=0, refusal=refusal)
                self.assertEqual(code, 0, "the fullscreen harness has no required scenario in this invocation")
                self.assertIs(verdict["partial_button_batch"], refusal is g.PartialButtonBatch)
        self.assertEqual(verdict["required_failed"], [])

    def test_unknown_required_name_is_a_usage_error(self) -> None:
        with mock.patch.object(g, "WINDOWS", True), contextlib.redirect_stderr(io.StringIO()), \
                self.assertRaises(SystemExit) as failure:
            g.main(["--exe", "unused.exe", "--injector", "sendinput", "--required", "no-such-scenario"])
        self.assertEqual(failure.exception.code, 2)

    def test_overlay_wrapper_passes_the_list_and_gates_setup_failure_on_it(self) -> None:
        for required, expected in ((None, 1), ("", 0), ("composer", 0), ("composer,tray", 1)):
            with self.subTest(required=required), tempfile.TemporaryDirectory() as directory, \
                    mock.patch.object(overlay.sys, "platform", "win32"), \
                    mock.patch.dict(overlay.os.environ, {"GITHUB_ACTIONS": "true", "WINDIR": directory}), \
                    mock.patch.object(overlay.hud_gestures, "windows_huds", return_value=[]), \
                    mock.patch.dict(sys.modules, {"winreg": mock.Mock()}), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                gate = [] if required is None else [f"--required={required}"]
                # No Vulkan loader under WINDIR: setup fails before any scenario.
                self.assertEqual(overlay.main(["--exe", "unused.exe", "--out", directory, *gate]), expected)
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(overlay.sys, "platform", "win32"), \
                mock.patch.dict(overlay.os.environ, {"GITHUB_ACTIONS": "true", "WINDIR": directory}), \
                mock.patch.object(overlay.hud_gestures, "windows_huds", return_value=[]), \
                mock.patch.dict(sys.modules, {"winreg": mock.Mock()}), \
                mock.patch.object(overlay, "extract_driver", return_value=Path("icd.json")), \
                mock.patch.object(overlay, "registered_driver", return_value=contextlib.nullcontext()), \
                mock.patch.object(overlay.hud_gestures, "main", return_value=0) as scenarios, \
                contextlib.redirect_stdout(io.StringIO()):
            (Path(directory) / "System32").mkdir()
            (Path(directory) / "System32/vulkan-1.dll").write_bytes(b"")
            for gate in ([], ["--required="], ["--required=tray"]):
                with self.subTest(gate=gate):
                    self.assertEqual(overlay.main(["--exe", "x.exe", "--out", directory, *gate]), 0)
                    argv = scenarios.call_args.args[0]
                    self.assertEqual(argv[argv.index("--out") + 2:],
                                     [*gate, "tray", "go-to-session", "overlay-pointer", "tray-answer",
                                      "overlay-shell"])


class OverlayInstallerTests(unittest.TestCase):
    def test_archive_is_verified_before_extraction(self) -> None:
        for case in ("download-fails", "wrong-hash", "verified"):
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive = b"fixture download"

                def external(args, **kwargs):
                    if args[0] == "gh":
                        kwargs["stdout"].write(archive)
                        if case == "download-fails":
                            raise overlay.subprocess.CalledProcessError(7, args)
                    else:
                        (root / "x64").mkdir()
                        (root / "x64/lvp_icd.x86_64.json").write_text("{}")
                        (root / "x64/vulkan_lvp.dll").write_bytes(b"fixture driver")

                digest = "0" * 64 if case == "wrong-hash" else hashlib.sha256(archive).hexdigest()
                with mock.patch.object(overlay.subprocess, "run", side_effect=external) as external_run:
                    if case == "verified":
                        self.assertEqual(overlay.extract_driver(root, expected_sha256=digest),
                                         root / "x64/lvp_icd.x86_64.json")
                        self.assertEqual(external_run.call_count, 2)
                    else:
                        with self.assertRaises((RuntimeError, overlay.subprocess.CalledProcessError)):
                            overlay.extract_driver(root, expected_sha256=digest)
                        self.assertEqual(external_run.call_count, 1)
                        self.assertFalse((root / "x64").exists())

    def test_registration_cleanup_preserves_foreign_values_and_runs_on_failure(self) -> None:
        for case in ("success", "scenario-fails", "already-registered"):
            with self.subTest(case=case):
                registry = mock.Mock(KEY_READ=1, KEY_WRITE=2, KEY_WOW64_64KEY=4)
                # Fake only the Win32 registry boundary.
                registry.CreateKeyEx.return_value = mock.MagicMock()
                name = str(Path("unique-lavapipe.json").resolve())
                values = {"foreign-driver.json": 0}
                if case == "already-registered":
                    values[name] = 1

                def query(_key, value):
                    if value not in values:
                        raise FileNotFoundError(value)
                    return values[value], 4

                registry.QueryValueEx.side_effect = query
                registry.SetValueEx.side_effect = lambda _key, value, _reserved, _kind, data: values.__setitem__(value, data)
                registry.DeleteValue.side_effect = lambda _key, value: values.pop(value)
                if case == "already-registered":
                    with self.assertRaises(RuntimeError):
                        with overlay.registered_driver(Path(name), registry):
                            self.fail("must not enter with an existing value")
                    self.assertEqual(values[name], 1)
                    registry.DeleteValue.assert_not_called()
                elif case == "scenario-fails":
                    with self.assertRaisesRegex(RuntimeError, "scenario failed"):
                        with overlay.registered_driver(Path(name), registry):
                            self.assertEqual(values[name], 0)
                            raise RuntimeError("scenario failed")
                    self.assertNotIn(name, values)
                else:
                    with overlay.registered_driver(Path(name), registry):
                        self.assertEqual(values[name], 0)
                    self.assertNotIn(name, values)
                self.assertEqual(values["foreign-driver.json"], 0)


class OverlayShellDesktopTests(unittest.TestCase):
    def test_desktop_switch_requires_a_fresh_owned_capture_and_a_compositor_swatch(self) -> None:
        dark = (1, 1, bytes((0, 0, 0, 255)))
        magenta = (1, 1, bytes((255, 0, 255, 255)))
        for case in ("delayed-desktop", "desktop-blind", "foreign-after-capture", "no-compositor"):
            with self.subTest(case=case):
                clock = 0.0
                run = mock.Mock()
                run.frame.return_value = dark if case == "no-compositor" else magenta
                run.key.side_effect = OSError("test ends at the first switch chord")

                def sleep(seconds):
                    nonlocal clock
                    clock += seconds

                def wait(probe, **kwargs):
                    return wait_until(probe, **kwargs, clock=lambda: clock, sleep=sleep)

                def desktop():
                    if case == "desktop-blind" or (case == "delayed-desktop"
                                                   and win.desktop_rgba.call_count == 1):
                        return dark
                    return magenta

                def foreground():
                    return 303 if case == "foreign-after-capture" and win.desktop_rgba.called else 101

                with mock.patch.object(g, "WIN") as win, mock.patch.object(g, "wait_until", side_effect=wait):
                    win.owned_window_handle.return_value = 1001
                    win.owned_window_on_current_desktop.return_value = True
                    win.desktop_rgba.side_effect = desktop
                    win.foreground_pid.side_effect = foreground
                    if case == "delayed-desktop":
                        with self.assertRaisesRegex(OSError, "test ends at the first switch chord"):
                            g.overlay_shell_desktop_probe(run, "card", 101, mock.Mock())
                        run.key.assert_called_once_with("win+ctrl+d")
                        self.assertEqual(win.desktop_rgba.call_count, 2)
                        run.screen_grab.assert_called_once_with("overlay-shell.before-switch", frame=magenta)
                    elif case == "desktop-blind":
                        # A stable owned foreground with no desktop swatch is a recorded limit, not a switch.
                        g.overlay_shell_desktop_probe(run, "card", 101, mock.Mock())
                        run.key.assert_not_called()
                        run.screen_grab.assert_called_once_with("overlay-shell.before-switch", frame=dark)
                        self.assertIn("limited desktop proof", run.step.call_args.args[0])
                    else:
                        with self.assertRaises(g.ScenarioFailed) as failure:
                            g.overlay_shell_desktop_probe(run, "card", 101, mock.Mock())
                        run.key.assert_not_called()
                        if case == "no-compositor":
                            self.assertEqual(failure.exception.last["compositor_pixels"], 0)
                            win.desktop_rgba.assert_not_called()
                        else:
                            self.assertEqual(failure.exception.last["foreground_after"], 303)
                            self.assertGreater(failure.exception.last["desktop_pixels"], 0)
                            run.screen_grab.assert_called_once_with("overlay-shell.before-switch", frame=magenta)

    def test_switch_cleanup_covers_late_observations_and_reports_unknown_restoration(self) -> None:
        for case in ("no-probe", "no-switch", "late-switch", "injection-error", "unknown", "close-unverified"):
            with self.subTest(case=case):
                clock = 0.0
                run = mock.Mock()
                run.expect.return_value = {"displays": []}
                open_editor = mock.Mock(return_value=202)

                def sleep(seconds):
                    nonlocal clock
                    clock += seconds

                def wait(probe, **kwargs):
                    return wait_until(probe, **kwargs, clock=lambda: clock, sleep=sleep)

                def inject(chord):
                    if case == "injection-error" and chord == "win+ctrl+d":
                        raise OSError("input failed after the shell received the chord")

                def membership(pid, hwnd=None):
                    if pid == 202:
                        self.assertEqual(hwnd, 2002)
                        return True
                    if not run.key.called:
                        return None if case == "no-probe" else True
                    if run.key.call_args == mock.call("win+ctrl+F4"):
                        return case != "close-unverified"
                    if case == "unknown":
                        return None
                    if case in {"late-switch", "injection-error"}:
                        return clock < 3.5
                    return case != "close-unverified"

                run.key.side_effect = inject
                with mock.patch.object(g, "WIN") as win, mock.patch.object(g, "wait_until", side_effect=wait), \
                        mock.patch.object(g, "colour_pixels", return_value=5):
                    win.owned_window_handle.side_effect = {101: 1001, 202: 2002}.__getitem__
                    win.owned_window_on_current_desktop.side_effect = membership
                    win.foreground_pid.return_value = 101
                    if case in {"unknown", "close-unverified"}:
                        with self.assertRaisesRegex(g.ScenarioFailed, "verify restoration"):
                            g.overlay_shell_desktop_probe(run, "card", 101, open_editor)
                    elif case == "injection-error":
                        with self.assertRaisesRegex(OSError, "after the shell received"):
                            g.overlay_shell_desktop_probe(run, "card", 101, open_editor)
                    else:
                        g.overlay_shell_desktop_probe(run, "card", 101, open_editor)

                expected = [] if case == "no-probe" else [mock.call("win+ctrl+d")]
                if case in {"late-switch", "injection-error", "close-unverified"}:
                    expected.append(mock.call("win+ctrl+F4"))
                    open_editor.assert_called_once_with()
                else:
                    open_editor.assert_not_called()
                self.assertEqual(run.key.call_args_list, expected)
                if case in {"late-switch", "injection-error"}:
                    run.screen_grab.assert_called_once_with("overlay-shell.before-switch",
                                                            frame=win.desktop_rgba.return_value)
                    win.bring_to_front.assert_called_once_with(101)
                    self.assertTrue(all(call.args == (101, 1001) for call in
                                        win.owned_window_on_current_desktop.call_args_list if call.args[0] == 101))


class TargetTests(unittest.TestCase):
    def test_resolves_pane_centres_and_the_whole_portal(self) -> None:
        self.assertEqual(g.resolve_target(STATUS, "portal:demo/output"), (550, 270))
        self.assertEqual(g.resolve_target(STATUS, "portal:demo/header"), (400, 70))
        self.assertEqual(g.resolve_target(STATUS, "portal:demo/close"), (688, 62))
        self.assertEqual(g.resolve_target(STATUS, "portal:demo"), (400, 250))
        self.assertEqual(g.resolve_target(STATUS, "zone:notification-area/card"), (550, 70))
        self.assertEqual(g.resolve_target(STATUS, "zone:notification-area/card/0"), (475, 105))
        self.assertEqual(g.resolve_target(STATUS, "zone:notification-area/card/1"), (615, 105))
        self.assertEqual(g.resolve_target(STATUS, "zone:notification-area/card/close"), (688, 22))
        self.assertEqual(g.resolve_target(STATUS, "widget:main-status"), (170, 550))
        self.assertEqual(g.resolve_target(STATUS, "widget:main-status/hover"), (310, 516))

    def test_go_to_resolves_only_the_admin_rect_for_that_card(self) -> None:
        entry = STATUS["notifications"][0]
        rect = {"x": 600, "y": 15, "w": 46, "h": 20}
        status = {**STATUS, "notifications": [{**entry, "goto": rect}]}
        self.assertEqual(g.resolve_target(status, "zone:notification-area/card/goto"), (623, 25))
        with self.assertRaisesRegex(LookupError, "not shown"):
            g.resolve_target(STATUS, "zone:notification-area/card/goto")

    def test_hidden_or_missing_targets_raise(self) -> None:
        with self.assertRaisesRegex(LookupError, "not shown"):
            g.resolve_target(STATUS, "portal:demo/unread_badge")
        with self.assertRaisesRegex(LookupError, "not on screen"):
            g.resolve_target(STATUS, "portal:other/output")

        for target in ["zone:notification-area/missing/0", "zone:notification-area/card/2",
                       "zone:notification-area/card/-1", "zone:notification-area/card/nope", "widget:missing/hover"]:
            with self.subTest(target=target), self.assertRaises(LookupError):
                g.resolve_target(STATUS, target)
        hidden = {**STATUS, "notifications": None, "widgets": [{**STATUS["widgets"][0], "hover_target": None}]}
        for target in ["zone:notification-area/card/0", "widget:main-status/hover"]:
            with self.subTest(target=target), self.assertRaises(LookupError):
                g.resolve_target(hidden, target)

    def test_offset_primary_display_raises(self) -> None:
        moved = {**STATUS, "displays": [{"x": -1920, "y": 0}]}
        with self.assertRaisesRegex(LookupError, "origin"):
            g.resolve_target(moved, "portal:demo/output")

    def test_colour_pixels_counts_only_near_matches(self) -> None:
        magenta, grey = bytes((255, 0, 255, 255)), bytes((40, 40, 40, 255))
        frame = (4, 2, magenta * 3 + grey + bytes((250, 6, 250, 255)) + grey * 3)
        self.assertEqual(g.colour_pixels(frame, g.CAPTURE_RGB, step=1), 4)
        self.assertEqual(g.colour_pixels(frame, g.CAPTURE_RGB, tolerance=0, step=1), 3)

    def test_exclude_from_capture_is_written_into_runtime_and_restored(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run = g.Run(Path("unused"), Path(directory))
            run.config = Path(directory) / "tze_hud.toml"
            original = '[runtime]\nprofile = "full-display"\n'
            g.set_exclude_from_capture(run, original, True)
            self.assertEqual(run.config.read_text(),
                             '[runtime]\nexclude_from_capture = true\nprofile = "full-display"\n')
            g.set_exclude_from_capture(run, original, False)
            self.assertEqual(run.config.read_text(), original)

    def test_a_failed_restore_restart_never_masks_the_scenario_failure(self) -> None:
        run = mock.Mock()
        run.config.read_text.return_value = "[runtime]\n"
        run.smoke.ok.return_value = {"surfaces": [{"s": "zone:pip"}]}
        restarts = iter([{"capture_exclusion": {"supported": True}}, RuntimeError("no replacement")])

        def restart(_run):
            outcome = next(restarts)
            if isinstance(outcome, Exception):
                raise outcome
            return outcome
        with mock.patch.object(g, "WIN") as win, mock.patch.object(g, "restart_owned", side_effect=restart), \
                mock.patch.object(g, "prove_excluded", side_effect=g.ScenarioFailed("excluded", {})), \
                mock.patch.object(g, "colour_pixels", return_value=5):
            win.desktop_rgba.return_value = (1, 1, b"\0\0\0\0")
            with self.assertRaises(g.ScenarioFailed) as failure:
                g.scenario_capture_exclusion(run)
        self.assertEqual(failure.exception.step, "excluded")
        run.step.assert_any_call("restore restart failed: RuntimeError: no replacement")

    def test_region_crops_rows_inside_a_rect(self) -> None:
        width, height = 4, 3
        rgba = bytes(range(width * height * 4))
        rows = g.region((width, height, rgba), {"x": 1, "y": 1, "w": 2, "h": 2}, inset=0)
        self.assertEqual(rows, [rgba[(1 * 4 + 1) * 4:(1 * 4 + 3) * 4], rgba[(2 * 4 + 1) * 4:(2 * 4 + 3) * 4]])


class CardHelperTests(unittest.TestCase):
    def test_publish_target_keyboard_and_acknowledged_answer_use_observed_state(self) -> None:
        for mode, scenario in [("click", g.scenario_card_click), ("keys", g.scenario_card_keyboard),
                               ("dismiss", g.scenario_card_dismiss)]:
            with self.subTest(mode=mode):
                run = g.Run(Path("unused"), Path("unused"))
                run.smoke, run.inj = mock.Mock(), mock.Mock()
                status = copy.deepcopy(STATUS)
                status["portals"][0]["s"] = "portal:gesture-card-keys"
                status["portals"][0]["focus"] = "composer"
                status["notifications"] = []
                run.status = lambda: status
                run.settle = lambda: status
                pending = []
                def call(verb, args, **kwargs):
                    if verb == "hud_publish":
                        card = copy.deepcopy(STATUS["notifications"][0])
                        card["key"] = args["key"]
                        card["state"] = "entering"  # valid last-frame phase even after quiescence
                        status["notifications"] = [card]
                        return False, {}
                    self.assertEqual(verb, "hud_input")
                    items, pending[:] = pending[:], []
                    return False, {"items": items}
                run.smoke.call.side_effect = call
                def answer(action):
                    if status["notifications"]:
                        card = status["notifications"].pop()
                        pending.append({"id": "owned-event", "s": card["s"], "key": card["key"],
                                        "action": action(card["key"]), "kind": "dismiss" if mode == "dismiss" else "callback"})
                def click(*_):
                    if mode != "keys":
                        answer(lambda key: "dismiss" if mode == "dismiss" else key + "-first")
                def key(chord):
                    if chord == "Tab":
                        card = status["notifications"][0]
                        card["focus"] = 0 if card["focus"] is None else 1
                    elif chord == "Return":
                        self.assertEqual(status["notifications"][0]["focus"], 1)
                        answer(lambda key: key + "-second")
                def move(x, y):  # the HUD shows the hand over card controls only
                    rects = [r for c in status["notifications"]
                             for r in [c["close"], *(a["rect"] for a in c["actions"])]]
                    hand = any(r["x"] <= x < r["x"] + r["w"] and r["y"] <= y < r["y"] + r["h"] for r in rects)
                    status["cursor"] = "hand" if hand else "arrow"
                run.inj.click.side_effect, run.inj.key.side_effect = click, key
                run.inj.move_abs.side_effect = move
                with mock.patch.object(g, "open_portal"):
                    scenario(run)
                publish = run.smoke.call.call_args_list[0]
                self.assertEqual(publish.args[0], "hud_publish")
                self.assertEqual(publish.args[1]["surface"], "zone:notification-area")
                self.assertEqual(publish.args[1]["content"]["actions"][1]["label"], "Second")
                self.assertTrue(run.publications)
                run.smoke.ok.assert_called_once_with("hud_input", {"ack": ["owned-event"]})
                self.assertFalse(status["notifications"])
                self.assertFalse(pending)
                run.inj.move_abs.assert_called()
                if mode == "keys":
                    self.assertEqual(run.inj.key.call_args_list, [mock.call("Tab"), mock.call("Tab"), mock.call("Return")])
                else:
                    self.assertEqual(run.inj.move_abs.call_args.args, (688, 22) if mode == "dismiss" else (475, 105))
        # A failed wait keeps the actual observation for scenario diagnostics.
        run = g.Run(Path("unused"), Path("unused"))
        run.status = lambda: STATUS
        with self.assertRaises(g.ScenarioFailed) as failure:
            run.expect("missing card", lambda s: g.notification_entry(s, "absent") is not None, timeout_s=0)
        self.assertEqual(failure.exception.last["notifications"], STATUS["notifications"])
        self.assertEqual(failure.exception.step, "missing card")



class SafeModeWitnessTests(unittest.TestCase):
    """hud-zvu0j: the 65 s pause witness and its failure cleanup."""

    # At this exact instant observe_for accepts (now >= paused + 65) while
    # `now - paused` rounds to 64.99999999999994.
    PAUSED, BOUNDARY = 478.39226673274817, 543.3922667327481

    def _run(self, safe_mode: dict) -> mock.Mock:
        run = mock.Mock()
        run.status.side_effect = lambda: {"safe_mode": safe_mode["on"]}

        def key(chord):
            self.assertEqual(chord, "ctrl+shift+F12")
            safe_mode["on"] = not safe_mode["on"]

        run.key.side_effect = key
        run.smoke.call.return_value = (True, {"code": "SAFE_MODE_ACTIVE"})
        return run

    def _scenario(self, run: mock.Mock, ended: float) -> None:
        clock = iter([self.PAUSED, ended])
        with mock.patch.object(g, "open_portal"), \
                mock.patch.object(g.time, "monotonic", side_effect=lambda: next(clock)):
            g.scenario_safe_mode(run)

    def test_the_float_boundary_instant_counts_as_the_full_65_seconds(self) -> None:
        self.assertLess(self.BOUNDARY - self.PAUSED, 65)
        self.assertGreaterEqual(self.BOUNDARY, self.PAUSED + 65.0)
        safe_mode = {"on": False}
        run = self._run(safe_mode)
        self._scenario(run, self.BOUNDARY)
        self.assertEqual(run.key.call_count, 2, "pause, then the scenario's own resume")
        self.assertFalse(safe_mode["on"])

    def test_an_early_return_still_fails_and_releases_safe_mode(self) -> None:
        safe_mode = {"on": False}
        run = self._run(safe_mode)
        with self.assertRaises(g.ScenarioFailed) as failure:
            self._scenario(run, self.PAUSED + 64.9)
        self.assertEqual(failure.exception.args[0], "pause observation spans the normal reap deadline")
        self.assertFalse(safe_mode["on"], "the failed witness released its own pause")
        self.assertEqual(run.key.call_count, 2)

    def test_a_failed_witness_cannot_poison_the_next_scenario(self) -> None:
        safe_mode = {"on": False}
        run = self._run(safe_mode)
        run.out = Path("unused")
        run.clear_publications.return_value = []
        run.observe_for.side_effect = g.ScenarioFailed("portal reaped while paused", {})
        seen = []
        restart = mock.Mock(__doc__="restart", side_effect=lambda _run: seen.append(safe_mode["on"]))
        with mock.patch.dict(g.SCENARIOS, {"safe-mode": g.scenario_safe_mode, "restart": restart}, clear=True), \
                mock.patch.object(g, "open_portal"), contextlib.redirect_stdout(io.StringIO()):
            results = g.run_scenarios(run, ["safe-mode", "restart"])
        self.assertEqual([r["outcome"] for r in results], ["FAIL", "PASS"])
        self.assertEqual(results[0]["step"], "portal reaped while paused", "the original failure reports")
        self.assertEqual(seen, [False])

    def test_a_failure_before_the_measured_pause_also_releases_it(self) -> None:
        safe_mode = {"on": False}
        run = self._run(safe_mode)
        run.smoke.call.return_value = (False, {"ok": True})  # publish wrongly accepted while paused
        with mock.patch.object(g, "open_portal"), self.assertRaises(g.ScenarioFailed) as failure:
            g.scenario_safe_mode(run)
        self.assertEqual(failure.exception.args[0], "publishing while paused returns SAFE_MODE_ACTIVE")
        self.assertFalse(safe_mode["on"])
        run.observe_for.assert_not_called()


class SmokeErrorTests(unittest.TestCase):
    def test_a_refused_tool_names_its_code_and_hint(self) -> None:
        smoke = g.Smoke("http://127.0.0.1:5555/mcp", "psk")
        refusal = {"code": "INTERNAL", "hint": "input state busy; retry hud_input"}
        with mock.patch.object(smoke, "call", return_value=(True, refusal)), \
                self.assertRaisesRegex(AssertionError, "hud_input: tool error response "
                                       "INTERNAL: input state busy; retry hud_input"):
            smoke.ok("hud_input", {"ack": ["a1"]})


if __name__ == "__main__":
    unittest.main()
