"""Unit tests for the tier-L gesture harness guards and target resolution."""

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
            fake.status.return_value = {}
            fake.inj = None
            fake.smoke.ok.return_value = {}
            for mode in ("fullscreen", "overlay"):
                with self.subTest(mode=mode), mock.patch.object(g, "WINDOWS", True), \
                        mock.patch.object(g, "Run", return_value=fake), \
                        mock.patch.object(g, "run_scenarios", return_value=[]) as scenarios, \
                        mock.patch.object(g, "write_step_summary"), contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(g.main(["--exe", "unused.exe", "--injector", "sendinput",
                                             "--window-mode", mode, "--out", directory]), 0)
                    selected = scenarios.call_args.args[1]
                    self.assertEqual("overlay-pointer" in selected, mode == "overlay")
                    self.assertEqual("tray" in selected, mode == "overlay")
                    self.assertEqual(len(selected), 2 if mode == "overlay" else 22)
            for mode, scenario in (("fullscreen", "overlay-pointer"), ("overlay", "composer")):
                with mock.patch.object(g, "WINDOWS", True), contextlib.redirect_stderr(io.StringIO()), \
                        self.assertRaises(SystemExit) as failure:
                    g.main(["--exe", "unused.exe", "--injector", "sendinput", "--window-mode", mode, scenario])
                self.assertEqual(failure.exception.code, 2)


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
                run.inj.click.side_effect, run.inj.key.side_effect = click, key
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


if __name__ == "__main__":
    unittest.main()
