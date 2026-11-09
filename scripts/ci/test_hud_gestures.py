"""Unit tests for the tier-L gesture harness guards and target resolution."""

from __future__ import annotations

import sys
import contextlib
import ctypes
import hashlib
import importlib.util
import io
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


class WindowsGuardTests(unittest.TestCase):
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
        spec = importlib.util.spec_from_file_location("guarded_sendinput", Path(__file__).with_name("hud_sendinput.py"))
        native = importlib.util.module_from_spec(spec)
        with mock.patch.object(ctypes, "WinDLL", create=True):
            spec.loader.exec_module(native)
        injector = native.SendInput(lambda: g.check_owned_windows(**{**WIN_OWNED, "foreground_pid": 9}))
        actions = (lambda: injector.move_abs(1, 2), lambda: injector.move_relative(3, 4),
                   lambda: injector.click(), lambda: injector.button(True), lambda: injector.key("Tab"),
                   lambda: injector.type_unicode("x"))
        for action in actions:
            with self.assertRaises(g.Refused):
                action()
        native.user32.SendInput.assert_not_called()

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
                    self.assertEqual(len(selected), 1 if mode == "overlay" else 17)
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

    def test_hidden_or_missing_targets_raise(self) -> None:
        with self.assertRaisesRegex(LookupError, "not shown"):
            g.resolve_target(STATUS, "portal:demo/unread_badge")
        with self.assertRaisesRegex(LookupError, "not on screen"):
            g.resolve_target(STATUS, "portal:other/output")

    def test_offset_primary_display_raises(self) -> None:
        moved = {**STATUS, "displays": [{"x": -1920, "y": 0}]}
        with self.assertRaisesRegex(LookupError, "origin"):
            g.resolve_target(moved, "portal:demo/output")

    def test_region_crops_rows_inside_a_rect(self) -> None:
        width, height = 4, 3
        rgba = bytes(range(width * height * 4))
        rows = g.region((width, height, rgba), {"x": 1, "y": 1, "w": 2, "h": 2}, inset=0)
        self.assertEqual(rows, [rgba[(1 * 4 + 1) * 4:(1 * 4 + 3) * 4], rgba[(2 * 4 + 1) * 4:(2 * 4 + 3) * 4]])


if __name__ == "__main__":
    unittest.main()
