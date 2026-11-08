"""Unit tests for the tier-L gesture harness guards and target resolution."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import hud_gestures as g  # noqa: E402

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


class TargetTests(unittest.TestCase):
    def test_resolves_pane_centres_and_the_whole_portal(self) -> None:
        self.assertEqual(g.resolve_target(STATUS, "portal:demo/output"), (550, 270))
        self.assertEqual(g.resolve_target(STATUS, "portal:demo/header"), (400, 70))
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
