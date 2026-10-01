from __future__ import annotations

import asyncio
import contextlib
import math
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIR))

import text_stream_portal_exemplar as portal  # noqa: E402
import portal_part_tokens as portal_tokens  # noqa: E402
import portal_two_pane_geometry as pg  # noqa: E402


class TextStreamPortalExemplarTests(unittest.TestCase):
    def test_join_transcript_entries_inserts_thematic_breaks(self) -> None:
        joined = portal.join_transcript_entries(["first", "second", "third"])
        # N entries → N-1 separators, each on its own line.
        self.assertEqual(joined.count("\n---\n"), 2)
        self.assertIn("first\n---\nsecond", joined)
        self.assertIn("second\n---\nthird", joined)
        # No leading/trailing divider.
        self.assertFalse(joined.startswith("---"))
        self.assertFalse(joined.endswith("---"))

    def test_join_transcript_entries_drops_empty_entries(self) -> None:
        joined = portal.join_transcript_entries(["only", "   ", ""])
        # A single non-empty entry → no separator at all.
        self.assertNotIn("---", joined)
        self.assertEqual(joined, "only")

    def test_append_input_history_records_viewer_submission(self) -> None:
        # A viewer submission lands in the INPUT-pane history (hud-egf39).
        history: list[str] = []
        recorded = portal.append_input_history(history, "hello from the viewer")
        self.assertEqual(recorded, "hello from the viewer")
        self.assertEqual(history, ["hello from the viewer"])

    def test_append_input_history_does_not_touch_output_transcript(self) -> None:
        # The OUTPUT transcript stays agent-authored only: recording an INPUT
        # submission must never fold the viewer text into `body_full`
        # (supersedes the #1027/#1031 combined-transcript echo — hud-egf39).
        body_full = portal.join_transcript_entries(["agent line one", "agent line two"])
        history: list[str] = []
        portal.append_input_history(history, "viewer reply that must not echo right")
        # body_full is an ordinary string the caller owns; the helper never
        # receives it and cannot mutate it. Assert the viewer text is absent
        # from the OUTPUT transcript and present in the INPUT history.
        self.assertNotIn("viewer reply that must not echo right", body_full)
        self.assertIn("viewer reply that must not echo right", history)

    def test_append_input_history_drops_whitespace_only_submission(self) -> None:
        # A bare Enter (empty/whitespace draft) creates no INPUT history turn.
        history: list[str] = []
        self.assertIsNone(portal.append_input_history(history, "   \n  "))
        self.assertEqual(history, [])

    def test_append_input_history_normalizes_and_stacks_entries(self) -> None:
        # Entries accumulate in submission order; CRLF/CR are normalized so a
        # multi-line submission stays a single history turn (the runtime draws
        # the `---` divider between adjacent turns — #1020).
        history: list[str] = []
        portal.append_input_history(history, "first")
        recorded = portal.append_input_history(history, "line one\r\nline two")
        self.assertEqual(recorded, "line one\nline two")
        self.assertEqual(history, ["first", "line one\nline two"])

    def test_split_transcript_entries_uses_blank_line_boundaries(self) -> None:
        body = "alpha one\nalpha two\n\nbravo\n\n\ncharlie"
        entries = portal.split_transcript_entries(body)
        self.assertEqual(entries, ["alpha one\nalpha two", "bravo", "charlie"])

    def test_load_transcript_slice_emits_entry_dividers(self) -> None:
        with tempfile.NamedTemporaryFile(
            "w", suffix=".md", delete=False, encoding="utf-8"
        ) as handle:
            handle.write("Entry one\nstill one\n\nEntry two\n\nEntry three\n")
            doc_path = handle.name
        try:
            body = portal.load_transcript_slice(doc_path, max_lines=100)
        finally:
            Path(doc_path).unlink(missing_ok=True)
        # Two dividers between three logical entries, each on its own line so the
        # compositor recognises them as thematic breaks.
        self.assertEqual(body.count("\n---\n"), 2)
        self.assertIn("Entry one\nstill one\n---\nEntry two", body)

    def test_load_transcript_slice_single_block_has_no_divider(self) -> None:
        with tempfile.NamedTemporaryFile(
            "w", suffix=".md", delete=False, encoding="utf-8"
        ) as handle:
            handle.write("Just one block\nwith two lines\n")
            doc_path = handle.name
        try:
            body = portal.load_transcript_slice(doc_path, max_lines=100)
        finally:
            Path(doc_path).unlink(missing_ok=True)
        self.assertNotIn("---", body)

    def test_large_display_size_clamp_uses_scene_bounds_not_demo_cap(self) -> None:
        w, h = portal.clamp_portal_size(2200.0, 1400.0, 3840.0, 2160.0)

        self.assertEqual((w, h), (2200.0, 1400.0))
        self.assertGreater(w, 1280.0)
        self.assertGreater(h, 960.0)

    def test_large_display_default_size_regression_exceeds_old_cap(self) -> None:
        w, h = portal.default_portal_size(3840.0, 2160.0)

        self.assertEqual((w, h), (1720.0, 1360.0))
        self.assertGreater(w, 1280.0)
        self.assertGreater(h, 960.0)

    def test_profile_swap_expanded_stays_larger_than_large_display_standard(self) -> None:
        standard_w, standard_h = portal.default_portal_size(3840.0, 2160.0)
        profiles = portal.profile_swap_dimensions(standard_w, standard_h)
        expanded = next(profile for profile in profiles if profile[0] == "expanded")

        expanded_w, expanded_h = portal.clamp_portal_size(
            expanded[1],
            expanded[2],
            3840.0,
            2160.0,
        )
        self.assertGreater(expanded_w, standard_w)
        self.assertGreater(expanded_h, standard_h)

    def test_portal_size_clamp_keeps_geometry_on_screen(self) -> None:
        self.assertEqual(
            portal.clamp_portal_size(5000.0, 3000.0, 3840.0, 2160.0),
            (3840.0, 2160.0),
        )
        self.assertEqual(
            portal.clamp_portal_size(100.0, 100.0, 3840.0, 2160.0),
            (portal.PORTAL_MIN_W, portal.PORTAL_MIN_H),
        )

    def test_caret_advance_matches_explicit_wrap_advance_at_line_end(self) -> None:
        max_width = portal.composer_wrap_area_width_px()
        chars_on_line = math.floor(max_width / portal.COMPOSER_WRAP_CHAR_W)
        text = "x" * chars_on_line

        display_text, cursor_x, cursor_row = portal.composer_wrapped_layout(
            text,
            len(text),
            max_width,
        )

        self.assertEqual(display_text, text)
        self.assertEqual(cursor_row, 0)
        self.assertAlmostEqual(
            cursor_x,
            portal.composer_wrap_text_width_px(text),
            places=5,
        )

    def test_space_is_the_only_printable_key_down_fallback(self) -> None:
        self.assertEqual(portal.composer_key_fallback_text("Space"), " ")
        self.assertIsNone(portal.composer_key_fallback_text("a"))
        self.assertIsNone(portal.composer_key_fallback_text("Tab"))

    def test_diagnostic_input_plan_targets_portal_focus_drag_and_scroll(self) -> None:
        plan = portal.build_diagnostic_input_plan(1000.0, 120.0)

        labels = [step["label"] for step in plan]
        self.assertEqual(
            labels,
            [
                "focus-composer",
                "drag-portal-header",
                "scroll-output-pane",
                "type-composer-text",
            ],
        )
        self.assertEqual(plan[0]["kind"], "click")
        self.assertEqual(plan[1]["kind"], "drag")
        self.assertEqual(plan[2]["kind"], "wheel")
        self.assertNotEqual(plan[1]["end_x"], plan[1]["start_x"])
        self.assertGreater(plan[1]["end_y"], plan[1]["start_y"])

    def test_diagnostic_input_plan_uses_clamped_drag_for_wheel_target(self) -> None:
        plan = portal.build_diagnostic_input_plan(
            0.0,
            0.0,
            tab_width=portal.PORTAL_W + 20.0,
            tab_height=portal.PORTAL_H + 20.0,
        )
        _, output_rect = portal.portal_pane_rects()

        drag = plan[1]
        wheel = plan[2]
        self.assertEqual(drag["end_x"], drag["start_x"])
        self.assertEqual(wheel["x"], output_rect.x + output_rect.w / 2.0)
        self.assertEqual(
            wheel["y"],
            20.0 + output_rect.y + min(output_rect.h - 10.0, 96.0),
        )

    def test_windows_diagnostic_input_script_uses_os_input_not_event_transcript(self) -> None:
        script = portal.windows_diagnostic_input_script(
            [
                {"kind": "click", "label": "focus", "x": 10.0, "y": 20.0},
                {
                    "kind": "drag",
                    "label": "drag",
                    "start_x": 10.0,
                    "start_y": 20.0,
                    "end_x": 30.0,
                    "end_y": 50.0,
                    "steps": 2,
                },
                {
                    "kind": "wheel",
                    "label": "scroll",
                    "x": 30.0,
                    "y": 50.0,
                    "delta": -240,
                    "count": 2,
                },
                {"kind": "text", "label": "text", "text": "ok"},
            ]
        )

        self.assertIn("SetCursorPos", script)
        self.assertIn("mouse_event", script)
        self.assertIn("SendInput", script)
        self.assertIn("if (-not [HudDiagnosticInput]::SetCursorPos", script)
        self.assertIn("$sent = [HudDiagnosticInput]::SendInput", script)
        self.assertIn("public MOUSEINPUT mi", script)
        self.assertIn("public KEYBDINPUT ki", script)
        self.assertIn("public HARDWAREINPUT hi", script)
        self.assertIn("Marshal]::GetLastWin32Error()", script)
        self.assertIn("diagnostic-warning:SendInput failed sent=", script)
        self.assertIn("' input_size=' + $InputSize", script)
        self.assertNotIn("throw 'SendInput failed'", script)
        self.assertLess(
            script.index("$inputs = [HudDiagnosticInput+INPUT[]]::new(2)"),
            script.index("foreach ($ch in $text.ToCharArray())"),
        )
        self.assertEqual(script.count("$inputs = [HudDiagnosticInput+INPUT[]]::new(2)"), 1)
        self.assertNotIn("EventBatch", script)
        self.assertNotIn("input_event_tx", script)

    def test_windows_diagnostic_input_script_hides_console_before_actions(self) -> None:
        script = portal.windows_diagnostic_input_script(
            [{"kind": "click", "label": "focus", "x": 10.0, "y": 20.0}]
        )

        self.assertIn("[Native.Win]::GetConsoleWindow()", script)
        self.assertIn("[Native.Win]::ShowWindow($hudConsole, 0)", script)
        self.assertIn("Start-Sleep -Milliseconds 400", script)
        hide_console = script.index("[Native.Win]::GetConsoleWindow()")
        hide_window = script.index("[Native.Win]::ShowWindow($hudConsole, 0)")
        settle_delay = script.index("Start-Sleep -Milliseconds 400")
        first_action = script.index("diagnostic:focus")

        self.assertLess(hide_console, hide_window)
        self.assertLess(hide_window, settle_delay)
        self.assertLess(settle_delay, first_action)

    def test_windows_diagnostic_input_script_scales_scene_to_desktop_coordinates(self) -> None:
        script = portal.windows_diagnostic_input_script(
            [{"kind": "click", "label": "focus", "x": 3000.0, "y": 1200.0}],
            scene_width=3840.0,
            scene_height=2160.0,
        )

        self.assertIn("[System.Windows.Forms.Screen]::PrimaryScreen.Bounds", script)
        self.assertIn("$HudDiagnosticScaleX", script)
        self.assertIn("$targetX = $x * $HudDiagnosticScaleX", script)
        self.assertIn("$targetY = $y * $HudDiagnosticScaleY", script)

    def test_windows_diagnostic_input_uses_ssh_connect_timeout(self) -> None:
        captured: dict[str, tuple[str, ...]] = {}
        original = portal.asyncio.create_subprocess_exec

        class FakeProcess:
            returncode = 0

            async def communicate(self, input: bytes | None = None) -> tuple[bytes, bytes]:
                return b"ok", b""

        async def fake_create_subprocess_exec(
            *cmd: str,
            stdin: object,
            stdout: object,
            stderr: object,
        ) -> FakeProcess:
            captured["cmd"] = cmd
            return FakeProcess()

        async def run() -> None:
            portal.asyncio.create_subprocess_exec = fake_create_subprocess_exec
            try:
                result = await portal.run_windows_diagnostic_input(
                    "example.invalid",
                    user="tester",
                    ssh_key="/tmp/key",
                    actions=[],
                    timeout_s=1.0,
                    connect_timeout_s=2.0,
                )
            finally:
                portal.asyncio.create_subprocess_exec = original
            self.assertTrue(result["ok"])

        asyncio.run(run())
        self.assertIn("ConnectTimeout=2", captured["cmd"])

    def test_windows_diagnostic_input_runs_as_interactive_scheduled_task(self) -> None:
        captured: dict[str, tuple[str, ...]] = {}
        captured_input: dict[str, bytes] = {}
        original = portal.asyncio.create_subprocess_exec

        class FakeProcess:
            returncode = 0

            async def communicate(self, input: bytes | None = None) -> tuple[bytes, bytes]:
                captured_input["input"] = input or b""
                return (
                    b'{"ok":true,"returncode":0,"stdout":"diagnostic:focus","stderr":""}',
                    b"",
                )

        async def fake_create_subprocess_exec(
            *cmd: str,
            stdin: object,
            stdout: object,
            stderr: object,
        ) -> FakeProcess:
            captured["cmd"] = cmd
            return FakeProcess()

        async def run() -> None:
            portal.asyncio.create_subprocess_exec = fake_create_subprocess_exec
            try:
                result = await portal.run_windows_diagnostic_input(
                    "example.invalid",
                    user="tester",
                    ssh_key="/tmp/key",
                    actions=[
                        {
                            "kind": "click",
                            "label": "focus",
                            "x": 10.0,
                            "y": 20.0,
                        }
                    ],
                    timeout_s=1.0,
                    connect_timeout_s=2.0,
                )
            finally:
                portal.asyncio.create_subprocess_exec = original
            self.assertTrue(result["ok"])
            self.assertEqual(result["stdout"], "diagnostic:focus")

        asyncio.run(run())
        self.assertNotIn("-EncodedCommand", captured["cmd"])
        self.assertIn("-", captured["cmd"])
        remote_script = captured_input["input"].decode("utf-8")
        self.assertIn("Register-ScheduledTask", remote_script)
        self.assertIn("-LogonType Interactive", remote_script)
        self.assertIn("Start-ScheduledTask", remote_script)
        self.assertIn("TzeHudDiagnosticInput", remote_script)
        self.assertIn("text_stream_portal_diagnostic_input_result_", remote_script)

    def test_windows_diagnostic_task_script_always_cleans_up_task_and_files(self) -> None:
        script = portal.windows_diagnostic_task_script(
            "Write-Output 'ok'",
            user="tester",
            timeout_s=1.0,
            run_id="abc123",
        )

        self.assertIn("try {", script)
        self.assertIn("finally {", script)
        self.assertIn("Stop-ScheduledTask -TaskName $taskName", script)
        self.assertIn("Unregister-ScheduledTask -TaskName $taskName", script)
        self.assertIn("Remove-Item -Force $scriptPath,$resultPath", script)
        self.assertNotIn("exit 0", script)

    def test_windows_diagnostic_input_reaps_timed_out_process(self) -> None:
        original = portal.asyncio.create_subprocess_exec

        class SlowProcess:
            returncode = None

            def __init__(self) -> None:
                self.killed = False
                self.waited = False

            async def communicate(self, input: bytes | None = None) -> tuple[bytes, bytes]:
                await asyncio.sleep(1.0)
                return b"", b""

            def kill(self) -> None:
                self.killed = True

            async def wait(self) -> int:
                self.waited = True
                return -9

        proc = SlowProcess()

        async def fake_create_subprocess_exec(
            *cmd: str,
            stdin: object,
            stdout: object,
            stderr: object,
        ) -> SlowProcess:
            return proc

        async def run() -> None:
            portal.asyncio.create_subprocess_exec = fake_create_subprocess_exec
            try:
                result = await portal.run_windows_diagnostic_input(
                    "example.invalid",
                    user="tester",
                    ssh_key="/tmp/key",
                    actions=[],
                    timeout_s=0.01,
                )
            finally:
                portal.asyncio.create_subprocess_exec = original
            self.assertFalse(result["ok"])
            self.assertEqual(result["error"], "timeout")

        asyncio.run(run())
        self.assertTrue(proc.killed)
        self.assertTrue(proc.waited)


class PaneResizeDividerTests(unittest.TestCase):
    """Middle pane-divider drag-activation + width-partition math (hud-z8z7p).

    Live round-6 (2026-07-04, tzehouse): the owner clicked mid-portal with no
    drag intent and the divider committed a resize; the pane-resize:end event
    also logged input=860 / output=818 (860+818=1678 vs portal_w=1720), so the
    input pane read as 'permanently shrunk relative to the rest of the window'.
    """

    def setUp(self) -> None:
        self._portal_w = portal.PORTAL_W
        self._input_w = portal.INPUT_PANE_W

    def tearDown(self) -> None:
        portal.PORTAL_W = self._portal_w
        portal.INPUT_PANE_W = self._input_w

    # (a) drag-activation threshold — a bare click is a no-op.
    def test_bare_click_does_not_cross_pane_drag_threshold(self) -> None:
        # Zero and sub-threshold jitter (incl. the ~6px seen in the field) must
        # NOT be treated as a resize.
        self.assertFalse(portal.pane_drag_crosses_threshold(0.0))
        self.assertFalse(portal.pane_drag_crosses_threshold(6.0))
        self.assertFalse(portal.pane_drag_crosses_threshold(-6.0))
        self.assertLess(6.0, portal.PANE_DRAG_START_THRESHOLD_PX)

    def test_deliberate_drag_crosses_pane_drag_threshold(self) -> None:
        self.assertTrue(portal.pane_drag_crosses_threshold(40.0))
        self.assertTrue(portal.pane_drag_crosses_threshold(-40.0))
        # Exactly at the threshold counts as a drag (>=).
        self.assertTrue(
            portal.pane_drag_crosses_threshold(portal.PANE_DRAG_START_THRESHOLD_PX)
        )

    def test_sub_threshold_click_leaves_input_pane_width_bit_identical(self) -> None:
        # Model the apply-gate: below threshold the caller returns before ever
        # calling set_input_pane_width, so the width stays byte-for-byte equal.
        portal.PORTAL_W = 1720.0
        portal.INPUT_PANE_W = 854.0
        before = portal.INPUT_PANE_W
        start_width, dx = before, 6.0
        if portal.pane_drag_crosses_threshold(dx):
            portal.set_input_pane_width(start_width + dx)  # pragma: no cover
        self.assertEqual(portal.INPUT_PANE_W, before)
        self.assertIs(portal.INPUT_PANE_W, before)

    def test_above_threshold_drag_commits_width(self) -> None:
        portal.PORTAL_W = 1720.0
        portal.INPUT_PANE_W = 854.0
        start_width, dx = portal.INPUT_PANE_W, 40.0
        if portal.pane_drag_crosses_threshold(dx):
            portal.set_input_pane_width(start_width + dx)
        self.assertEqual(portal.INPUT_PANE_W, 894.0)

    # (b) pane-width partition sums exactly to the portal frame.
    def test_pane_partition_sums_to_portal_width(self) -> None:
        portal_w = 1720.0
        for input_w in (240.0, 500.0, 854.0, 860.0, 857.0, 1200.0, 1474.0):
            input_pane_w, output_pane_w = portal.partition_pane_widths(portal_w, input_w)
            self.assertEqual(input_pane_w, input_w)
            self.assertAlmostEqual(
                input_pane_w + portal.PANE_DIVIDER_W + output_pane_w,
                portal_w,
                places=9,
                msg=f"panes must tile the frame exactly for input_w={input_w}",
            )

    def test_pane_partition_sums_across_portal_sizes(self) -> None:
        for portal_w in (960.0, 1280.0, 1720.0, 1920.0, 3840.0):
            input_w = (portal_w - portal.PANE_DIVIDER_W) / 2.0
            input_pane_w, output_pane_w = portal.partition_pane_widths(portal_w, input_w)
            self.assertAlmostEqual(
                input_pane_w + portal.PANE_DIVIDER_W + output_pane_w,
                portal_w,
                places=9,
            )

    def test_output_pane_width_matches_pane_rect(self) -> None:
        portal.PORTAL_W = 1720.0
        portal.INPUT_PANE_W = 860.0
        # Live output-pane width honours the partition invariant...
        self.assertAlmostEqual(portal.output_pane_width(), 854.0, places=9)
        self.assertAlmostEqual(
            portal.INPUT_PANE_W + portal.PANE_DIVIDER_W + portal.output_pane_width(),
            portal.PORTAL_W,
            places=9,
        )
        # ...and the output *body* rect is the pane inset by 2*PADDING_X — this
        # 36px inset (plus the 6px divider) is the '42px' that made 860/818 look
        # like they failed to sum; it is a render inset, not lost frame width.
        _, output_rect = portal.portal_pane_rects()
        self.assertAlmostEqual(
            output_rect.w, portal.output_pane_width() - portal.PADDING_X * 2.0, places=9
        )


class ComposerInputModeRoutingTests(unittest.TestCase):
    """Headless coverage for composer keyboard-routing wiring (hud-dwcr7).

    The runtime only routes keystrokes into the ComposerDraftManager when the
    focused hit-region carries `accepts_composer_input=True` (types.proto:103,
    consumed by tze_hud_input::node_accepts_composer_input). Without it, a
    pointer-down acquires focus but typed characters are dropped — the operator
    "can't type anything in". These tests pin the wire-level flag so the
    regression cannot silently return.
    """

    @staticmethod
    def _collect_hit_regions(children):
        regions = {}
        for node in children:
            if node.HasField("hit_region"):
                regions[node.hit_region.interaction_id] = node.hit_region
        return regions

    def test_composer_hit_region_accepts_composer_input(self) -> None:
        _root, children = portal.build_input_scroll_nodes("")
        regions = self._collect_hit_regions(children)
        self.assertIn(
            portal.COMPOSER_INTERACTION_ID,
            regions,
            "composer tile must expose the composer focus hit-region",
        )
        composer = regions[portal.COMPOSER_INTERACTION_ID]
        self.assertTrue(
            composer.accepts_focus,
            "composer hit-region must acquire focus on pointer-down",
        )
        self.assertTrue(
            composer.accepts_composer_input,
            "composer hit-region must route keystrokes into the runtime draft "
            "manager (accepts_composer_input=True) or typing is dropped",
        )

    def test_make_node_forwards_accepts_composer_input(self) -> None:
        # The client node builder must serialize accepts_composer_input onto the
        # wire; a stale builder/binding silently drops it (the original bug).
        node = portal.make_hit_region(
            "probe", 0.0, 0.0, 10.0, 10.0, accepts_composer_input=True,
        )
        self.assertTrue(node.hit_region.accepts_composer_input)

    def test_header_drag_region_does_not_accept_composer_input(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(
            portal.PORTAL_DRAG_INTERACTION_ID,
            regions,
            "frame chrome must expose the header drag hit-region",
        )
        drag = regions[portal.PORTAL_DRAG_INTERACTION_ID]
        self.assertFalse(
            drag.accepts_composer_input,
            "header drag region must not capture composer keystrokes",
        )


class HeaderDragBandTests(unittest.TestCase):
    """Full-width titlebar drag band (hud-643dv).

    Owner direction: the whole top header band should drag the portal like a
    Windows titlebar, minus the minimize control. Whole-portal MOVE is owned by
    the runtime (screen-sovereignty): the runtime generates a geometry-driven
    header-BAND drag handle for the portal frame tile, and — because a runtime
    drag handle is hit-tested before node hit-regions — that band yields to any
    interactive (accepts_pointer) node under the point so the minimize button
    still wins inside its own bounds.

    These pin the EXEMPLAR-side invariants the runtime band relies on:
      * the header drag node is a full-width semantic marker of the band, and
      * it is inert (accepts_pointer=False) so it neither drives movement nor
        shadows the minimize control (which would be caught by the band's
        yield-to-interactive-node rule and break the drag), while
      * the minimize control stays pointer-interactive so the band yields to it.
    """

    @staticmethod
    def _collect_hit_regions(children):
        regions = {}
        for node in children:
            if node.HasField("hit_region"):
                regions.setdefault(node.hit_region.interaction_id, []).append(
                    node.hit_region
                )
        return regions

    def test_header_drag_marker_spans_full_header_width(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(portal.PORTAL_DRAG_INTERACTION_ID, regions)
        drag = regions[portal.PORTAL_DRAG_INTERACTION_ID][0]
        # Full top band: origin at the top-left corner, spanning the whole portal
        # width and the header height — not the old x=MINIMIZE_HIT_W carve-out.
        self.assertEqual(drag.bounds.x, 0.0, "drag band must start at the left edge")
        self.assertEqual(drag.bounds.y, 0.0, "drag band must start at the top edge")
        self.assertEqual(
            drag.bounds.width, portal.PORTAL_W,
            "drag band must span the FULL portal width (Windows-titlebar band)",
        )
        self.assertEqual(
            drag.bounds.height, portal.HEADER_H,
            "drag band height must be the header layout constant, not a new magic value",
        )

    def test_header_drag_marker_is_inert_not_a_pointer_target(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        for hit in regions[portal.PORTAL_DRAG_INTERACTION_ID]:
            # Movement is owned by the runtime header-band handle, so this node
            # must NOT claim the pointer: an accepts_pointer node here would
            # shadow the minimize control and be swallowed by the runtime band's
            # yield-to-interactive rule, breaking both drag and minimize.
            self.assertFalse(
                hit.accepts_pointer,
                "the header drag marker must be inert (accepts_pointer=False); the "
                "runtime band owns movement (hud-643dv)",
            )
            # Still pointer-only chrome — never a keyboard Tab stop.
            self.assertFalse(
                hit.accepts_focus,
                "the header drag marker must not be a Tab stop",
            )

    def test_minimize_control_beats_the_band(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        # The minimize control must stay pointer-interactive AND focusable so the
        # runtime band yields to it (pointer) and a keyboard viewer can reach it.
        self.assertIn(portal.PORTAL_MINIMIZE_INTERACTION_ID, regions)
        for hit in regions[portal.PORTAL_MINIMIZE_INTERACTION_ID]:
            self.assertTrue(
                hit.accepts_pointer,
                "minimize must remain a pointer target so the drag band yields to it",
            )
            self.assertTrue(
                hit.accepts_focus,
                "minimize stays a Tab stop (unchanged by the band)",
            )
        # And the minimize hit-rect lies within the full-width band, so the two
        # genuinely overlap and precedence (not geometry) is what protects it.
        minimize = regions[portal.PORTAL_MINIMIZE_INTERACTION_ID][0]
        drag = regions[portal.PORTAL_DRAG_INTERACTION_ID][0]
        self.assertGreaterEqual(minimize.bounds.x, drag.bounds.x)
        self.assertLessEqual(
            minimize.bounds.x + minimize.bounds.width,
            drag.bounds.x + drag.bounds.width,
            "minimize must sit inside the full-width band (real overlap)",
        )


class PortalFocusTraversalRingTests(unittest.TestCase):
    """Tab/Shift+Tab focus-traversal stops for the portal (hud-02sp5).

    The runtime focus ring (tze_hud_input::FocusManager::build_focus_cycle)
    collects only HitRegionNodes with accepts_focus=True (input-model spec
    "Focus Cycling"). The text-stream-portals spec names the focusable
    "portal controls" as the composer plus the expand, collapse, and reply
    affordances; drag/resize are pointer-only. Before this fix every frame
    affordance was accepts_focus=False, so a keyboard-only viewer (Mobile
    Presence Node / glasses) had no reachable stops. These tests pin the
    deliberate tab-ring set so it cannot silently regress in either
    direction (interactive control dropped, or a pointer-only drag/resize
    handle blindly promoted into the ring).
    """

    @staticmethod
    def _collect_hit_regions(children):
        regions = {}
        for node in children:
            if node.HasField("hit_region"):
                # A portal can carry two hit regions with the same
                # interaction_id (e.g. the L-shaped restore target); keep a
                # list so callers can assert every fragment agrees.
                regions.setdefault(node.hit_region.interaction_id, []).append(
                    node.hit_region
                )
        return regions

    # ── Expanded frame: interactive controls ARE focus stops ──────────────

    def test_minimize_collapse_control_is_focusable(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(
            portal.PORTAL_MINIMIZE_INTERACTION_ID,
            regions,
            "frame chrome must expose the minimize/collapse control",
        )
        for hit in regions[portal.PORTAL_MINIMIZE_INTERACTION_ID]:
            self.assertTrue(
                hit.accepts_focus,
                "minimize is a collapse control and MUST be a Tab stop so a "
                "keyboard-only viewer can collapse the portal without a pointer",
            )

    def test_submit_reply_control_is_focusable(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(portal.SUBMIT_INTERACTION_ID, regions)
        for hit in regions[portal.SUBMIT_INTERACTION_ID]:
            self.assertTrue(
                hit.accepts_focus,
                "submit is the reply affordance and MUST be a Tab stop",
            )

    # ── Minimized icon: restore/expand IS a focus stop ────────────────────

    def test_restore_expand_control_is_focusable(self) -> None:
        _root, children = portal.build_minimized_icon_nodes(
            attention=False, pulse=False,
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(
            portal.PORTAL_RESTORE_INTERACTION_ID,
            regions,
            "minimized icon must expose the restore/expand control",
        )
        for hit in regions[portal.PORTAL_RESTORE_INTERACTION_ID]:
            self.assertTrue(
                hit.accepts_focus,
                "restore is the expand control and MUST be a Tab stop so a "
                "minimized portal can be re-expanded without a pointer",
            )

    # ── Pointer-only affordances are NOT focus stops (deliberate) ─────────

    def test_pointer_only_frame_affordances_are_not_focusable(self) -> None:
        _root, children = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        regions = self._collect_hit_regions(children)
        for interaction_id in (
            portal.PORTAL_DRAG_INTERACTION_ID,
            portal.PANE_RESIZE_INTERACTION_ID,
            portal.PORTAL_RESIZE_INTERACTION_ID,
        ):
            self.assertIn(interaction_id, regions)
            for hit in regions[interaction_id]:
                self.assertFalse(
                    hit.accepts_focus,
                    f"{interaction_id} is a pointer-only drag/resize handle and "
                    "MUST NOT be a Tab stop (matches Telegram-style chrome: the "
                    "title bar and resize grips are not tabbable)",
                )

    def test_minimized_icon_drag_is_not_focusable(self) -> None:
        _root, children = portal.build_minimized_icon_nodes(
            attention=False, pulse=False,
        )
        regions = self._collect_hit_regions(children)
        self.assertIn(portal.PORTAL_ICON_DRAG_INTERACTION_ID, regions)
        for hit in regions[portal.PORTAL_ICON_DRAG_INTERACTION_ID]:
            self.assertFalse(
                hit.accepts_focus,
                "the minimized-icon drag handle is pointer-only and MUST NOT be "
                "a Tab stop",
            )

    def test_capture_backstop_is_not_focusable(self) -> None:
        _root, children = portal.build_capture_backstop_nodes(1920.0, 1080.0)
        regions = self._collect_hit_regions(children)
        for hits in regions.values():
            for hit in hits:
                self.assertFalse(
                    hit.accepts_focus,
                    "the capture backstop is an invisible pointer-capture surface "
                    "and MUST NOT be a Tab stop",
                )

    # ── Whole-ring shape assertion ────────────────────────────────────────

    def test_expanded_tab_ring_is_the_deliberate_control_set(self) -> None:
        """The union of focusable frame + composer stops is exactly the
        interactive control set — no pointer-only handle leaks in."""
        _root, frame = portal.build_portal_nodes(
            "title", "subtitle", "body", "footer",
        )
        _croot, composer = portal.build_input_scroll_nodes("")
        focusable = set()
        pointer_only = set()
        for children in (frame, composer):
            for iid, hits in self._collect_hit_regions(children).items():
                for hit in hits:
                    (focusable if hit.accepts_focus else pointer_only).add(iid)
        # Interactive controls that a keyboard-only viewer must be able to reach.
        self.assertIn(portal.PORTAL_MINIMIZE_INTERACTION_ID, focusable)
        self.assertIn(portal.SUBMIT_INTERACTION_ID, focusable)
        self.assertIn(portal.COMPOSER_INTERACTION_ID, focusable)
        # Pointer-only chrome must stay out of the ring.
        self.assertNotIn(portal.PORTAL_DRAG_INTERACTION_ID, focusable)
        self.assertNotIn(portal.PORTAL_RESIZE_INTERACTION_ID, focusable)
        self.assertNotIn(portal.PANE_RESIZE_INTERACTION_ID, focusable)


class PromotionGateEvidenceSchemaTests(unittest.TestCase):
    """Headless coverage for the RFC 0013 §7.2 promotion-gate evidence schema.

    These exercise the artifact-shaping logic WITHOUT a live HUD, so the harness
    is provably emitting the gate-conformant artifact shape before any live run.
    """

    # ── Reference-hardware tag (engineering-bar §2) ───────────────────────

    def test_reference_hardware_tag_has_required_gate_fields(self) -> None:
        tag = portal.reference_hardware_tag()
        for field in ("tag", "hostname", "gpu", "gpu_driver", "os", "is_reference"):
            self.assertIn(field, tag, f"missing reference-hardware field {field!r}")
        # Defaults fall back to the canonical reference host constants.
        self.assertEqual(tag["tag"], portal.REFERENCE_HARDWARE_TAG)
        self.assertEqual(tag["gpu"], portal.REFERENCE_GPU)
        self.assertEqual(tag["gpu_driver"], portal.REFERENCE_GPU_DRIVER)

    def test_reference_hardware_tag_honours_cli_overrides(self) -> None:
        tag = portal.reference_hardware_tag(
            tag="OtherHost",
            hostname="some-laptop",
            gpu="Intel UHD",
            gpu_driver="1.2.3",
        )
        self.assertEqual(tag["tag"], "OtherHost")
        self.assertEqual(tag["hostname"], "some-laptop")
        self.assertEqual(tag["gpu"], "Intel UHD")
        self.assertEqual(tag["gpu_driver"], "1.2.3")
        self.assertFalse(tag["is_reference"])

    def test_reference_hardware_tag_marks_reference_host_from_target(self) -> None:
        tag = portal.reference_hardware_tag(
            target=portal.REFERENCE_HOSTNAME + ":50051",
        )
        self.assertTrue(tag["is_reference"])
        self.assertEqual(tag["target_host"], portal.REFERENCE_HOSTNAME)

    def test_reference_run_marks_reference_even_from_nonreference_collector(self) -> None:
        # The orchestration box is a non-reference Linux collector, but the run
        # drives the Windows reference target. is_reference must follow the
        # target, so the legitimate reference run is NOT misread as off-reference.
        with mock.patch.object(portal.socket, "gethostname", return_value="linux-collector"):
            tag = portal.reference_hardware_tag(
                target=portal.REFERENCE_HOSTNAME + ":50051",
            )
        self.assertEqual(tag["collected_hostname"], "linux-collector")
        self.assertEqual(tag["target_host"], portal.REFERENCE_HOSTNAME)
        self.assertTrue(tag["is_reference"])

    def test_reference_hardware_tag_ignores_collector_host_for_reference(self) -> None:
        # Even if the collector itself happens to be the reference host, a run
        # that drives a NON-reference target is informational-only. is_reference
        # is a property of the target, never the collection host.
        with mock.patch.object(portal.socket, "gethostname", return_value=portal.REFERENCE_HOSTNAME):
            tag = portal.reference_hardware_tag(
                target="some-other-host:50051",
            )
        self.assertEqual(tag["collected_hostname"], portal.REFERENCE_HOSTNAME)
        self.assertEqual(tag["target_host"], "some-other-host")
        self.assertFalse(tag["is_reference"])

    def test_reference_hardware_tag_without_target_is_not_reference(self) -> None:
        # No target means nothing to attribute reference status to, even when the
        # collector is the reference host. Fail-fast: do not silently infer
        # reference status from the local box.
        with mock.patch.object(portal.socket, "gethostname", return_value=portal.REFERENCE_HOSTNAME):
            tag = portal.reference_hardware_tag()
        self.assertEqual(tag["target_host"], "")
        self.assertFalse(tag["is_reference"])

    def test_reference_hostname_override_matches_target(self) -> None:
        # A run may declare the reference host identity explicitly (e.g. the real
        # TzeHouse tailnet name). is_reference is true only when the target host
        # matches that declared identity...
        tag = portal.reference_hardware_tag(
            hostname="tzehouse.tailnet",
            target="tzehouse.tailnet:50051",
        )
        self.assertTrue(tag["is_reference"])
        # ...and false when the declared reference host is not the target driven.
        other = portal.reference_hardware_tag(
            hostname="tzehouse.tailnet",
            target="some-other-host:50051",
        )
        self.assertFalse(other["is_reference"])

    # ── Cadence axis: RTT baseline vs runtime overhead ────────────────────

    def test_cadence_evidence_separates_runtime_overhead_from_rtt(self) -> None:
        # Two presented appends: present latency 30ms over each cycle's own 20ms
        # transport RTT ⇒ runtime overhead 10ms each (within the present budget).
        appends = [
            {"cycle": 1, "publish_ms": 0.0, "present_ms": 30.0, "rtt_ms": 20.0},
            {"cycle": 2, "publish_ms": 100.0, "present_ms": 130.0, "rtt_ms": 20.0},
        ]
        ev = portal.build_cadence_rtt_evidence(
            20.0, appends, cadence_cycles=2, cadence_interval_ms=100,
        )
        self.assertEqual(ev["transport_rtt_baseline_ms"], 20.0)
        self.assertEqual(ev["presented_count"], 2)
        self.assertEqual(ev["coalesced_count"], 0)
        for entry in ev["appends"]:
            self.assertTrue(entry["presented"])
            self.assertAlmostEqual(entry["present_latency_ms"], 30.0)
            # Overhead is isolated against THIS cycle's RTT, not the baseline.
            self.assertAlmostEqual(entry["overhead_baseline_ms"], 20.0)
            self.assertAlmostEqual(entry["overhead_ms"], 10.0)
            self.assertTrue(entry["within_present_budget"])
        self.assertEqual(ev["runtime_overhead_ms"]["over_budget_count"], 0)
        self.assertTrue(ev["within_present_budget"])

    def test_cadence_overhead_uses_per_cycle_rtt_not_fixed_baseline(self) -> None:
        # Regression for hud-lod76 (root-cause hud-ans49 / live evidence
        # hud-ofe76): a cycle whose transport RTT spiked to 56ms — with the
        # publish→present latency tracking that same spike — must score ~0
        # RUNTIME overhead and PASS the budget. The discredited fixed-baseline
        # calc (present_latency - 22.008ms) would instead report ~34ms overhead
        # and falsely FAIL the 16.6ms runtime budget, charging transport jitter
        # to the runtime.
        fixed_baseline = 22.008
        appends = [
            {"cycle": 1, "publish_ms": 0.0, "present_ms": 56.04, "rtt_ms": 56.0},
        ]
        ev = portal.build_cadence_rtt_evidence(
            fixed_baseline, appends, cadence_cycles=1, cadence_interval_ms=100,
        )
        entry = ev["appends"][0]
        # Per-cycle isolation: overhead = present_latency(56.04) - this RTT(56.0).
        self.assertAlmostEqual(entry["overhead_baseline_ms"], 56.0)
        self.assertAlmostEqual(entry["overhead_ms"], 0.04, places=3)
        self.assertTrue(entry["within_present_budget"])
        self.assertTrue(ev["within_present_budget"])
        self.assertEqual(ev["runtime_overhead_ms"]["over_budget_count"], 0)
        # The transport baseline is still recorded for context...
        self.assertEqual(ev["transport_rtt_baseline_ms"], fixed_baseline)
        # ...but the old fixed-baseline calc would have falsely failed the gate.
        old_overhead = entry["present_latency_ms"] - fixed_baseline
        self.assertGreater(old_overhead, portal.HIGH_MUTATION_PRESENT_BUDGET_MS)

    def test_cadence_evidence_records_per_append_publish_present_timestamps(self) -> None:
        appends = [{"cycle": 1, "publish_ms": 5.0, "present_ms": 12.0}]
        ev = portal.build_cadence_rtt_evidence(
            2.0, appends, cadence_cycles=1, cadence_interval_ms=100,
        )
        entry = ev["appends"][0]
        # The per-append entry preserves the publish→present timestamps the gate
        # requires (tasks 5.7 / spec "runtime overhead beyond transport RTT").
        self.assertEqual(entry["publish_ms"], 5.0)
        self.assertEqual(entry["present_ms"], 12.0)
        self.assertAlmostEqual(entry["present_latency_ms"], 7.0)

    def test_cadence_evidence_excludes_coalesced_appends_from_budget(self) -> None:
        # A coalesced-away append (no present_ms) must not count toward the
        # presented-append budget per the spec scenario.
        appends = [
            {"cycle": 1, "publish_ms": 0.0, "present_ms": 25.0},
            {"cycle": 2, "publish_ms": 50.0},  # coalesced away — no present
        ]
        ev = portal.build_cadence_rtt_evidence(
            20.0, appends, cadence_cycles=2, cadence_interval_ms=100,
        )
        self.assertEqual(ev["presented_count"], 1)
        self.assertEqual(ev["coalesced_count"], 1)
        coalesced = ev["appends"][1]
        self.assertFalse(coalesced["presented"])
        self.assertIsNone(coalesced["overhead_ms"])
        self.assertNotIn("within_present_budget", coalesced)

    def test_cadence_evidence_flags_over_budget_runtime_overhead(self) -> None:
        # Genuine runtime overhead: present latency 40ms while THIS cycle's
        # transport RTT was only 5ms ⇒ 35ms runtime overhead > 16.6ms budget.
        appends = [{"cycle": 1, "publish_ms": 0.0, "present_ms": 40.0, "rtt_ms": 5.0}]
        ev = portal.build_cadence_rtt_evidence(
            5.0, appends, cadence_cycles=1, cadence_interval_ms=100,
        )
        self.assertEqual(ev["runtime_overhead_ms"]["over_budget_count"], 1)
        self.assertFalse(ev["within_present_budget"])
        self.assertFalse(ev["appends"][0]["within_present_budget"])

    # ── Operator-confirmable evidence (window-mgmt / profile-swap) ────────

    def test_operator_evidence_entry_is_structured(self) -> None:
        entry = portal.operator_evidence_entry(
            "window-mgmt:move",
            "portal moved cleanly to centre",
            {"portal_x": 100.0, "portal_y": 50.0},
        )
        self.assertEqual(entry["code"], "window-mgmt:move")
        self.assertEqual(entry["operator_confirm"], "portal moved cleanly to centre")
        self.assertEqual(entry["observed"]["portal_x"], 100.0)
        # Operator confirmation defaults to unfilled until the live run.
        self.assertIsNone(entry["confirmed"])

    # ── Full artifact envelope ────────────────────────────────────────────

    def test_evidence_artifact_carries_reference_tag_and_schema_version(self) -> None:
        tag = portal.reference_hardware_tag(target=portal.REFERENCE_HOSTNAME + ":50051")
        artifact = portal.build_evidence_artifact(
            target="host:50051",
            doc="docs/x.md",
            phases="cadence,window-mgmt,profile-swap",
            scene_width=1920.0,
            scene_height=1080.0,
            portal_w=860.0,
            portal_h=680.0,
            lease_release_on_exit=True,
            cleanup_errors=[],
            steps=[{"code": "cadence", "status": "completed"}],
            hardware_tag=tag,
        )
        self.assertEqual(artifact["schema_version"], portal.EVIDENCE_SCHEMA_VERSION)
        self.assertIn("reference_hardware", artifact)
        self.assertTrue(artifact["reference_hardware"]["is_reference"])
        # The artifact must round-trip through JSON (it is written to disk).
        import json as _json
        _json.dumps(artifact)

    def test_evidence_artifact_is_backward_compatible(self) -> None:
        # The fields the prior schema exposed must still be present so existing
        # consumers/readers do not break.
        artifact = portal.build_evidence_artifact(
            target="host:50051",
            doc="docs/x.md",
            phases="baseline",
            scene_width=1920.0,
            scene_height=1080.0,
            portal_w=860.0,
            portal_h=680.0,
            lease_release_on_exit=False,
            cleanup_errors=["boom"],
            steps=[],
            hardware_tag=portal.reference_hardware_tag(),
        )
        for legacy_field in (
            "target", "doc", "scene_width", "scene_height",
            "portal_w", "portal_h", "lease_release_on_exit",
            "cleanup_errors", "steps",
        ):
            self.assertIn(legacy_field, artifact)
        self.assertEqual(artifact["cleanup_errors"], ["boom"])
        self.assertFalse(artifact["lease_release_on_exit"])


# ── Sustained soak phase (hud-pnofj) ──────────────────────────────────────────


class _SoakRetryStubClient:
    """Minimal client stub for run_soak tests. run_soak enables a bounded
    mutation-ack retry on its client for the soak's duration and resets it in a
    finally; publish_portal is faked in these tests, so configure_mutation_retry
    is the only client method exercised. Records its calls so the enable/reset
    wiring can be asserted (hud-n5bqp)."""

    def __init__(self) -> None:
        self.retry_calls: list[tuple[int, float]] = []

    def configure_mutation_retry(self, retries: int, backoff_s: float = 0.5) -> None:
        self.retry_calls.append((retries, backoff_s))


class SoakPhaseTests(unittest.TestCase):
    def test_run_soak_enables_then_resets_bounded_mutation_retry(self) -> None:
        # The soak driver must turn the bounded retry ON for the run and reset it
        # to fail-fast (0) afterward so other phases are unaffected (hud-n5bqp).
        client = _SoakRetryStubClient()
        now = 0.0

        original_publish = portal.publish_portal
        original_sleep = portal.asyncio.sleep
        original_monotonic = portal.time.monotonic

        def fake_monotonic() -> float:
            return now

        async def fake_publish_portal(*args, body: str, **kwargs) -> None:
            nonlocal now
            now += 0.05

        async def fake_sleep(delay_s: float) -> None:
            nonlocal now
            now += delay_s

        async def run() -> None:
            await portal.run_soak(
                client=client,
                lease_id=b"lease",
                tiles=_portal_tiles(),
                body_full="\n".join(f"seed {i}" for i in range(5)),
                transcript=[],
                duration_s=0.5,
                interval_ms=250,
                window_lines=3,
                mutation_lock=asyncio.Lock(),
            )

        portal.publish_portal = fake_publish_portal
        portal.asyncio.sleep = fake_sleep
        portal.time.monotonic = fake_monotonic
        try:
            asyncio.run(run())
        finally:
            portal.publish_portal = original_publish
            portal.asyncio.sleep = original_sleep
            portal.time.monotonic = original_monotonic

        self.assertEqual(
            client.retry_calls[0],
            (portal.SOAK_MUTATION_RETRIES, portal.SOAK_MUTATION_RETRY_BACKOFF_S),
        )
        self.assertEqual(client.retry_calls[-1][0], 0)

    def test_soak_tail_history_stays_bounded_to_window_lines(self) -> None:
        seed = [f"seed {i}" for i in range(5)]
        lines = list(seed[-3:])

        for cycle in range(1, 101):
            portal.append_soak_tail_line(lines, seed, cycle, elapsed_s=float(cycle), window_lines=3)
            self.assertLessEqual(len(lines), 3)

        self.assertEqual(len(lines), 3)
        self.assertTrue(all(line.startswith("[soak] line") for line in lines))
        self.assertIn("line 000098", lines[0])
        self.assertIn("line 000100", lines[-1])

    def test_soak_pacing_subtracts_publish_duration(self) -> None:
        captured_bodies: list[list[str]] = []
        sleeps: list[float] = []
        now = 0.0

        original_publish = portal.publish_portal
        original_sleep = portal.asyncio.sleep
        original_monotonic = portal.time.monotonic

        def fake_monotonic() -> float:
            return now

        async def fake_publish_portal(*args, body: str, **kwargs) -> None:
            nonlocal now
            captured_bodies.append(body.splitlines())
            now += 0.07

        async def fake_sleep(delay_s: float) -> None:
            nonlocal now
            sleeps.append(delay_s)
            now += delay_s

        async def run() -> list[dict]:
            transcript: list[dict] = []
            await portal.run_soak(
                client=_SoakRetryStubClient(),
                lease_id=b"lease",
                tiles=_portal_tiles(),
                body_full="\n".join(f"seed {i}" for i in range(5)),
                transcript=transcript,
                duration_s=0.8,
                interval_ms=250,
                window_lines=3,
                mutation_lock=asyncio.Lock(),
            )
            return transcript

        portal.publish_portal = fake_publish_portal
        portal.asyncio.sleep = fake_sleep
        portal.time.monotonic = fake_monotonic
        try:
            transcript = asyncio.run(run())
        finally:
            portal.publish_portal = original_publish
            portal.asyncio.sleep = original_sleep
            portal.time.monotonic = original_monotonic

        self.assertEqual(len(captured_bodies), 4)
        self.assertTrue(all(len(body) <= 3 for body in captured_bodies))
        self.assertAlmostEqual(sleeps[0], 0.18, places=6)
        self.assertTrue(all(sleep_s >= 0.0 for sleep_s in sleeps))
        completed = [step for step in transcript if step["status"] == "completed"][-1]
        self.assertEqual(completed["cycles"], 4)

    def test_write_soak_outcome_marker_full_duration_writes_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            marker_dir = Path(tmp)
            written = portal.write_soak_outcome_marker(
                marker_dir,
                completed=True,
                intended_s=3600.0,
                actual_s=3601.2,
                cycles=14400,
            )
            complete = marker_dir / portal.SOAK_COMPLETE_MARKER_NAME
            aborted = marker_dir / portal.SOAK_ABORTED_MARKER_NAME
            self.assertEqual(written, complete)
            self.assertTrue(complete.is_file())
            self.assertFalse(aborted.exists())
            self.assertEqual(complete.read_text(encoding="utf-8").strip(),
                             portal.SOAK_COMPLETE_TOKEN)

    def test_write_soak_outcome_marker_short_duration_writes_aborted(self) -> None:
        # Even when a caller claims completion, a run that fell far short of its
        # intended duration must be recorded as aborted, never SOAK_COMPLETE.
        with tempfile.TemporaryDirectory() as tmp:
            marker_dir = Path(tmp)
            written = portal.write_soak_outcome_marker(
                marker_dir,
                completed=True,
                intended_s=3600.0,
                actual_s=608.0,
                cycles=2400,
                reason="lease expired",
            )
            complete = marker_dir / portal.SOAK_COMPLETE_MARKER_NAME
            aborted = marker_dir / portal.SOAK_ABORTED_MARKER_NAME
            self.assertEqual(written, aborted)
            self.assertFalse(complete.exists())
            self.assertTrue(aborted.is_file())
            body = aborted.read_text(encoding="utf-8")
            self.assertIn(portal.SOAK_ABORTED_TOKEN, body)
            self.assertIn("reason=lease expired", body)
            self.assertIn("actual_duration_s=608.000", body)
            self.assertIn("intended_duration_s=3600.000", body)

    def test_write_soak_outcome_marker_abort_removes_stale_complete(self) -> None:
        # A prior full run may have left a completion marker in a reused dir;
        # an abort must clear it so a gate cannot false-pass on the stale marker.
        with tempfile.TemporaryDirectory() as tmp:
            marker_dir = Path(tmp)
            complete = marker_dir / portal.SOAK_COMPLETE_MARKER_NAME
            complete.write_text(f"{portal.SOAK_COMPLETE_TOKEN}\n", encoding="utf-8")
            portal.write_soak_outcome_marker(
                marker_dir,
                completed=False,
                intended_s=3600.0,
                actual_s=608.0,
                cycles=2400,
                reason="MUTATION_REJECTED — lease expired",
            )
            aborted = marker_dir / portal.SOAK_ABORTED_MARKER_NAME
            self.assertFalse(complete.exists())
            self.assertTrue(aborted.is_file())
            self.assertIn("lease expired", aborted.read_text(encoding="utf-8"))

    def test_run_soak_full_duration_writes_complete_marker(self) -> None:
        now = 0.0

        original_publish = portal.publish_portal
        original_sleep = portal.asyncio.sleep
        original_monotonic = portal.time.monotonic

        def fake_monotonic() -> float:
            return now

        async def fake_publish_portal(*args, body: str, **kwargs) -> None:
            nonlocal now
            now += 0.07

        async def fake_sleep(delay_s: float) -> None:
            nonlocal now
            sleeps_delay = max(0.0, delay_s)
            now += sleeps_delay

        async def run(marker_dir: Path) -> None:
            await portal.run_soak(
                client=_SoakRetryStubClient(),
                lease_id=b"lease",
                tiles=_portal_tiles(),
                body_full="\n".join(f"seed {i}" for i in range(5)),
                transcript=[],
                duration_s=0.8,
                interval_ms=250,
                window_lines=3,
                mutation_lock=asyncio.Lock(),
                marker_dir=marker_dir,
            )

        portal.publish_portal = fake_publish_portal
        portal.asyncio.sleep = fake_sleep
        portal.time.monotonic = fake_monotonic
        try:
            with tempfile.TemporaryDirectory() as tmp:
                marker_dir = Path(tmp)
                asyncio.run(run(marker_dir))
                complete = marker_dir / portal.SOAK_COMPLETE_MARKER_NAME
                aborted = marker_dir / portal.SOAK_ABORTED_MARKER_NAME
                self.assertTrue(complete.is_file())
                self.assertFalse(aborted.exists())
                self.assertEqual(complete.read_text(encoding="utf-8").strip(),
                                 portal.SOAK_COMPLETE_TOKEN)
        finally:
            portal.publish_portal = original_publish
            portal.asyncio.sleep = original_sleep
            portal.time.monotonic = original_monotonic

    def test_run_soak_lease_death_writes_aborted_not_complete(self) -> None:
        now = 0.0
        published = 0

        original_publish = portal.publish_portal
        original_sleep = portal.asyncio.sleep
        original_monotonic = portal.time.monotonic

        def fake_monotonic() -> float:
            return now

        async def fake_publish_portal(*args, body: str, **kwargs) -> None:
            nonlocal now, published
            published += 1
            if published >= 3:
                raise RuntimeError(
                    "Mutation batch rejected: MUTATION_REJECTED — lease expired"
                )
            now += 0.05

        async def fake_sleep(delay_s: float) -> None:
            nonlocal now
            now += max(0.0, delay_s)

        async def run(marker_dir: Path) -> None:
            await portal.run_soak(
                client=_SoakRetryStubClient(),
                lease_id=b"lease",
                tiles=_portal_tiles(),
                body_full="\n".join(f"seed {i}" for i in range(5)),
                transcript=[],
                duration_s=3600.0,
                interval_ms=250,
                window_lines=3,
                mutation_lock=asyncio.Lock(),
                marker_dir=marker_dir,
            )

        portal.publish_portal = fake_publish_portal
        portal.asyncio.sleep = fake_sleep
        portal.time.monotonic = fake_monotonic
        try:
            with tempfile.TemporaryDirectory() as tmp:
                marker_dir = Path(tmp)
                with self.assertRaises(RuntimeError):
                    asyncio.run(run(marker_dir))
                complete = marker_dir / portal.SOAK_COMPLETE_MARKER_NAME
                aborted = marker_dir / portal.SOAK_ABORTED_MARKER_NAME
                self.assertFalse(
                    complete.exists(),
                    "lease-death soak must NOT write the completion marker",
                )
                self.assertTrue(aborted.is_file())
                body = aborted.read_text(encoding="utf-8")
                self.assertIn(portal.SOAK_ABORTED_TOKEN, body)
                self.assertIn("lease expired", body)
        finally:
            portal.publish_portal = original_publish
            portal.asyncio.sleep = original_sleep
            portal.time.monotonic = original_monotonic

    def test_soak_phase_extends_initial_lease_ttl(self) -> None:
        self.assertEqual(
            portal.scenario_lease_ttl_ms("baseline,soak", baseline_hold_s=20.0, soak_duration_s=3600.0),
            3_720_000,
        )
        self.assertEqual(
            portal.scenario_lease_ttl_ms("baseline,scroll", baseline_hold_s=20.0, soak_duration_s=3600.0),
            600_000,
        )


class _InteractionCleanupClient:
    def __init__(self) -> None:
        self._event_queue: asyncio.Queue = asyncio.Queue()

    async def submit_mutation_batch(self, lease_id, mutations, timeout=5.0):
        class _Result:
            accepted = True
            error_code = 0
            error_message = ""
            batch_id = b""
            created_ids: list[bytes] = []

        return _Result()


class _InteractionCleanupErrorClient(_InteractionCleanupClient):
    def __init__(self) -> None:
        super().__init__()
        self.render_started = asyncio.Event()

    async def submit_mutation_batch(self, lease_id, mutations, timeout=5.0):
        self.render_started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError as exc:
            raise RuntimeError("composer render cleanup failed") from exc


def _pending_portal_background_tasks() -> list[asyncio.Task]:
    names = {
        "portal_interaction_loop.<locals>.composer_blink_worker",
        "portal_interaction_loop.<locals>.composer_render_worker",
    }
    return [
        task for task in asyncio.all_tasks()
        if not task.done() and task.get_coro().__qualname__ in names
    ]


class InteractionCleanupTests(unittest.TestCase):
    def test_cancelled_interaction_loop_cancels_composer_background_tasks(self) -> None:
        async def run() -> None:
            client = _InteractionCleanupClient()
            tiles = _portal_tiles()
            old_blink_seconds = portal.COMPOSER_CARET_BLINK_SECONDS
            portal.COMPOSER_CARET_BLINK_SECONDS = 60.0
            loop_task = asyncio.create_task(
                portal.portal_interaction_loop(
                    client=client,
                    lease_id=b"lease",
                    tiles=tiles,
                    transcript=[],
                    body_full="body",
                    initial_portal_x=0.0,
                    initial_portal_y=0.0,
                    tab_width=portal.PORTAL_W,
                    tab_height=portal.PORTAL_H,
                    mutation_lock=asyncio.Lock(),
                    clipboard_host="windows-host.example",
                    clipboard_user="hud-user",
                    clipboard_ssh_key="/tmp/no-key",
                    clipboard_timeout_s=0.1,
                )
            )
            try:
                await client._event_queue.put(
                    portal.events_pb2.EventBatch(
                        events=[
                            portal.events_pb2.InputEnvelope(
                                focus_gained=portal.events_pb2.FocusGainedEvent(
                                    tile_id=tiles.input_scroll,
                                )
                            )
                        ]
                    )
                )
                for _ in range(20):
                    if _pending_portal_background_tasks():
                        break
                    await asyncio.sleep(0)
                self.assertTrue(
                    _pending_portal_background_tasks(),
                    "focus should arm a composer background task before cancellation",
                )

                loop_task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await loop_task
                await asyncio.sleep(0)

                self.assertEqual(
                    _pending_portal_background_tasks(),
                    [],
                    "interaction-loop cancellation must cancel detached composer tasks",
                )
            finally:
                portal.COMPOSER_CARET_BLINK_SECONDS = old_blink_seconds
                loop_task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await loop_task
                for task in _pending_portal_background_tasks():
                    task.cancel()
                await asyncio.sleep(0)

        asyncio.run(run())

    def test_cancelled_interaction_loop_surfaces_composer_cleanup_errors(self) -> None:
        async def run() -> None:
            client = _InteractionCleanupErrorClient()
            tiles = _portal_tiles()
            old_blink_seconds = portal.COMPOSER_CARET_BLINK_SECONDS
            old_debounce_seconds = portal.COMPOSER_RENDER_DEBOUNCE_SECONDS
            old_runtime_node_ids = dict(portal.COMPOSER_RUNTIME_NODE_IDS)
            portal.COMPOSER_CARET_BLINK_SECONDS = 60.0
            portal.COMPOSER_RENDER_DEBOUNCE_SECONDS = 0.0
            portal.COMPOSER_RUNTIME_NODE_IDS.clear()
            portal.COMPOSER_RUNTIME_NODE_IDS.update(
                {
                    **{
                        key: f"{key}-node".encode("utf-8")
                        for key in portal.COMPOSER_LINE_KEYS
                    },
                    "caret": b"caret-node",
                }
            )
            loop_task = asyncio.create_task(
                portal.portal_interaction_loop(
                    client=client,
                    lease_id=b"lease",
                    tiles=tiles,
                    transcript=[],
                    body_full="body",
                    initial_portal_x=0.0,
                    initial_portal_y=0.0,
                    tab_width=portal.PORTAL_W,
                    tab_height=portal.PORTAL_H,
                    mutation_lock=asyncio.Lock(),
                    clipboard_host="windows-host.example",
                    clipboard_user="hud-user",
                    clipboard_ssh_key="/tmp/no-key",
                    clipboard_timeout_s=0.1,
                )
            )
            try:
                await client._event_queue.put(
                    portal.events_pb2.EventBatch(
                        events=[
                            portal.events_pb2.InputEnvelope(
                                focus_gained=portal.events_pb2.FocusGainedEvent(
                                    tile_id=tiles.input_scroll,
                                )
                            )
                        ]
                    )
                )
                await asyncio.wait_for(client.render_started.wait(), timeout=1.0)

                loop_task.cancel()
                with self.assertRaisesRegex(RuntimeError, "composer render cleanup failed"):
                    await loop_task

                self.assertEqual(
                    _pending_portal_background_tasks(),
                    [],
                    "failed composer cleanup must not leak detached tasks",
                )
            finally:
                portal.COMPOSER_CARET_BLINK_SECONDS = old_blink_seconds
                portal.COMPOSER_RENDER_DEBOUNCE_SECONDS = old_debounce_seconds
                portal.COMPOSER_RUNTIME_NODE_IDS.clear()
                portal.COMPOSER_RUNTIME_NODE_IDS.update(old_runtime_node_ids)
                loop_task.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await loop_task
                for task in _pending_portal_background_tasks():
                    task.cancel()
                await asyncio.sleep(0)

        asyncio.run(run())


# ── Steady-state publish atomicity (hud-ooeam flicker fix) ────────────────────


class _RecordingClient:
    """Minimal fake HudClient that records every mutation batch.

    All higher-level helpers funnel through ``submit_mutation_batch`` exactly as
    the real client does, so recording that one entry point captures the full
    mutation stream (set_tile_root / add_node / update_node_content / …) plus
    which tile each touched.
    """

    def __init__(self) -> None:
        self.batches: list[dict] = []
        self._next_id = 0

    def _alloc_id(self) -> bytes:
        self._next_id += 1
        return self._next_id.to_bytes(16, "big")

    async def submit_mutation_batch(self, lease_id, mutations, timeout=5.0):
        created: list[bytes] = []
        for m in mutations:
            kind = m.WhichOneof("mutation")
            tile_id = b""
            field = getattr(m, kind)
            if hasattr(field, "tile_id"):
                tile_id = field.tile_id
            elif kind == "publish_to_tile":
                tile_id = field.element_id
            self.batches.append({"kind": kind, "tile_id": tile_id})
            # set_tile_root / add_node / create_tile produce a server id.
            if kind in ("set_tile_root", "add_node", "create_tile"):
                created.append(self._alloc_id())

        class _Result:
            accepted = True
            error_code = 0
            error_message = ""

        result = _Result()
        result.batch_id = b""
        result.created_ids = created
        return result

    async def add_node(self, lease_id, tile_id, node, parent_id=None):
        mr = await self.submit_mutation_batch(
            lease_id,
            [portal.types_pb2.MutationProto(
                add_node=portal.types_pb2.AddNodeMutation(
                    tile_id=tile_id, parent_id=parent_id or b"", node=node,
                ),
            )],
        )
        return mr.created_ids[0]

    async def update_node_content(self, lease_id, tile_id, node_id, node):
        mut = portal.update_node_content_mutation(tile_id, node_id, node)
        await self.submit_mutation_batch(lease_id, [mut])

    async def update_tile_opacity(self, lease_id, tile_id, opacity):
        await self.submit_mutation_batch(
            lease_id,
            [portal.types_pb2.MutationProto(
                update_tile_opacity=portal.types_pb2.UpdateTileOpacityMutation(
                    tile_id=tile_id, opacity=opacity,
                ),
            )],
        )

    async def update_tile_input_mode(self, lease_id, tile_id, input_mode):
        await self.submit_mutation_batch(
            lease_id,
            [portal.types_pb2.MutationProto(
                update_tile_input_mode=portal.types_pb2.UpdateTileInputModeMutation(
                    tile_id=tile_id, input_mode=input_mode,
                ),
            )],
        )


def _portal_tiles() -> "portal.PortalTiles":
    ids = [i.to_bytes(16, "big") for i in range(900, 906)]
    return portal.PortalTiles(
        capture_backstop=ids[0],
        frame=ids[1],
        input_scroll=ids[2],
        output_scroll=ids[3],
        drag_shield=ids[4],
        minimized_icon=ids[5],
        tab_width=portal.PORTAL_W,
        tab_height=portal.PORTAL_H,
    )


class SteadyStatePublishAtomicityTests(unittest.TestCase):
    """Lock in the hud-ooeam flicker fix at the mutation-stream contract level.

    The live flicker came from the steady-state publish tearing the output-body
    tile down (set_tile_root → empties the tile) and re-adding children via
    separate add_node RPCs/commits, so the render thread sampled an empty body
    between commits. The fix updates the body text node in place via a single
    atomic update_node_content batch. These tests assert that contract without a
    live HUD.
    """

    def _setup_then_steady(self):
        tiles = _portal_tiles()
        client = _RecordingClient()
        lease = b"lease"
        body0 = "line 0\nline 1\nline 2"
        body1 = "line 0\nline 1\nline 2\nline 3"

        async def run():
            # Reset module-level runtime-id registries for isolation.
            portal.FRAME_RUNTIME_NODE_IDS.clear()
            portal.COMPOSER_RUNTIME_NODE_IDS.clear()
            portal.OUTPUT_RUNTIME_NODE_IDS.clear()
            # First publish: full tile setup (mounts + captures runtime ids).
            await portal.publish_portal(
                client, lease, tiles,
                title="T", subtitle="S", body=body0,
                footer_meta="f0", include_tile_setup=True,
            )
            setup_batches = list(client.batches)
            client.batches.clear()
            # Steady-state publish: append one line, republish window.
            await portal.publish_portal(
                client, lease, tiles,
                title="T", subtitle="S", body=body1,
                footer_meta="f1", include_tile_setup=False,
            )
            return setup_batches, list(client.batches)

        return asyncio.run(run()), tiles

    def test_steady_state_output_body_updates_in_place_not_torn_down(self) -> None:
        (setup_batches, steady_batches), tiles = self._setup_then_steady()

        out = tiles.output_scroll
        # Setup must have mounted the output tile via a tile-root + add_node(s).
        self.assertTrue(
            any(b["kind"] == "set_tile_root" and b["tile_id"] == out for b in setup_batches),
            "setup should mount the output tile root",
        )
        # The captured body id must exist after setup.
        self.assertIn("body", portal.OUTPUT_RUNTIME_NODE_IDS)

        # Steady state: NO teardown/rebuild of the output tile.
        out_set_root = [b for b in steady_batches if b["kind"] == "set_tile_root" and b["tile_id"] == out]
        out_add_node = [b for b in steady_batches if b["kind"] == "add_node" and b["tile_id"] == out]
        self.assertEqual(out_set_root, [], "steady-state must not reset the output tile root")
        self.assertEqual(out_add_node, [], "steady-state must not re-add output children")

        # Steady state MUST update the body content in place.
        out_updates = [b for b in steady_batches if b["kind"] == "update_node_content" and b["tile_id"] == out]
        self.assertTrue(out_updates, "steady-state must update the output body in place")

    def test_steady_state_frame_chrome_updates_in_place_not_torn_down(self) -> None:
        (_setup, steady_batches), tiles = self._setup_then_steady()
        frame = tiles.frame
        frame_set_root = [b for b in steady_batches if b["kind"] == "set_tile_root" and b["tile_id"] == frame]
        frame_add_node = [b for b in steady_batches if b["kind"] == "add_node" and b["tile_id"] == frame]
        self.assertEqual(frame_set_root, [], "steady-state must not reset the frame tile root")
        self.assertEqual(frame_add_node, [], "steady-state must not re-add frame children")
        frame_updates = [b for b in steady_batches if b["kind"] == "update_node_content" and b["tile_id"] == frame]
        self.assertTrue(frame_updates, "steady-state must update frame chrome in place")

    def test_steady_state_remounts_when_output_ids_invalidated(self) -> None:
        # If the captured output ids were invalidated (e.g. the interactive
        # scroll path tore the tile down), a steady-state publish must fall back
        # to a full re-mount (set_tile_root) rather than issuing an in-place
        # update against a node id that no longer exists.
        tiles = _portal_tiles()
        client = _RecordingClient()
        lease = b"lease"

        async def run():
            portal.FRAME_RUNTIME_NODE_IDS.clear()
            portal.COMPOSER_RUNTIME_NODE_IDS.clear()
            portal.OUTPUT_RUNTIME_NODE_IDS.clear()
            await portal.publish_portal(
                client, lease, tiles,
                title="T", subtitle="S", body="a\nb",
                footer_meta="f0", include_tile_setup=True,
            )
            # Simulate the scroll-path teardown invalidation.
            portal.OUTPUT_RUNTIME_NODE_IDS.clear()
            client.batches.clear()
            await portal.publish_portal(
                client, lease, tiles,
                title="T", subtitle="S", body="a\nb\nc",
                footer_meta="f1", include_tile_setup=False,
            )
            return list(client.batches)

        steady_batches = asyncio.run(run())
        out = tiles.output_scroll
        out_set_root = [b for b in steady_batches if b["kind"] == "set_tile_root" and b["tile_id"] == out]
        self.assertTrue(out_set_root, "must re-mount the output tile when ids are invalidated")
        # And the re-mount must re-capture the body id for subsequent cycles.
        self.assertIn("body", portal.OUTPUT_RUNTIME_NODE_IDS)


class LeaseRenewalTests(unittest.TestCase):
    """Regression coverage for hud-hk8kl: sustained portal runs must renew their
    lease so they survive past the original TTL instead of self-terminating with
    MUTATION_REJECTED / "lease expired" mid-run."""

    def test_renew_interval_lands_strictly_before_expiry(self) -> None:
        # A 600s lease renews at 450s — a comfortable margin before expiry.
        self.assertEqual(portal.lease_renew_interval_s(600_000), 450.0)
        self.assertLess(portal.lease_renew_interval_s(600_000), 600.0)
        # Tiny/zero TTLs clamp to the minimum instead of busy-looping.
        self.assertEqual(
            portal.lease_renew_interval_s(10), portal.LEASE_RENEW_MIN_INTERVAL_S
        )
        self.assertEqual(
            portal.lease_renew_interval_s(0), portal.LEASE_RENEW_MIN_INTERVAL_S
        )

    def test_scheduled_renewals_keep_lease_alive_past_original_ttl(self) -> None:
        # Pure schedule simulation: a run 6x longer than the initial TTL. Model
        # the loop exactly — sleep(interval), then renew (fresh full TTL from now)
        # — and assert the lease deadline is never behind the clock.
        ttl_ms = 600_000
        ttl_s = ttl_ms / 1000.0
        run_duration_s = 3600.0
        interval_s = portal.lease_renew_interval_s(ttl_ms)

        clock = 0.0
        lease_deadline = ttl_s  # granted at t=0 for one TTL
        next_renew_at = interval_s
        step = 5.0
        while clock < run_duration_s:
            clock += step
            self.assertLess(
                clock,
                lease_deadline,
                f"lease expired at t={clock:.0f}s (deadline {lease_deadline:.0f}s)",
            )
            if clock >= next_renew_at:
                lease_deadline = clock + ttl_s
                next_renew_at = clock + interval_s

        self.assertGreater(clock, ttl_s, "simulation must outlast the original TTL")

    def test_lease_renewal_loop_renews_before_expiry_over_long_run(self) -> None:
        # Drive the real lease_renewal_loop with a fake client and a virtual
        # clock. Prove it renews repeatedly, always before the TTL elapses, and
        # keeps going well past the original TTL.
        ttl_ms = 600_000
        ttl_s = ttl_ms / 1000.0
        run_duration_s = 3600.0

        clock = {"t": 0.0}
        renew_calls: list[float] = []

        class _FakeRenewClient:
            def __init__(self) -> None:
                self.last_granted_lease_ttl_ms = ttl_ms

            async def renew_lease(self, lease_id, new_ttl_ms=0):
                renew_calls.append(clock["t"])
                return ttl_ms

        async def fake_sleep(delay_s: float) -> None:
            clock["t"] += delay_s
            if clock["t"] > run_duration_s:
                # End the otherwise-infinite loop the way session cleanup does.
                raise asyncio.CancelledError

        original_sleep = portal.asyncio.sleep

        async def run() -> None:
            portal.asyncio.sleep = fake_sleep
            try:
                await portal.lease_renewal_loop(_FakeRenewClient(), b"lease", ttl_ms)
            except asyncio.CancelledError:
                pass
            finally:
                portal.asyncio.sleep = original_sleep

        asyncio.run(run())

        self.assertGreaterEqual(
            len(renew_calls), 5, "long run must renew the lease several times"
        )
        self.assertLess(
            renew_calls[0], ttl_s, "first renewal must land before the original TTL"
        )
        for prev, cur in zip(renew_calls, renew_calls[1:]):
            self.assertLess(
                cur - prev, ttl_s, "consecutive renewals must stay inside one TTL"
            )
        self.assertGreater(
            renew_calls[-1], ttl_s, "renewals must continue past the original TTL"
        )


class PortalTokenResolutionTests(unittest.TestCase):
    """hud-7jrj3: the exemplar sources every published visual value from
    resolved portal tokens, and a profile swap reskins them end-to-end."""

    def _find_rust_portal_tokens_source(self) -> Path:
        """Locate crates/tze_hud_config/src/portal_tokens.rs by walking upward
        from this test file — robust to worktree / checkout layout."""
        rel = Path("crates/tze_hud_config/src/portal_tokens.rs")
        for base in [Path(__file__).resolve(), *Path(__file__).resolve().parents]:
            candidate = base / rel
            if candidate.is_file():
                return candidate
        self.skipTest(f"Rust source {rel} not found (running outside the repo tree)")
        raise AssertionError("unreachable")  # pragma: no cover

    def test_python_defaults_mirror_rust_defaults(self) -> None:
        """The Python canonical-default mirror must match the Rust `mod defaults`
        block byte-for-byte, so the two token surfaces cannot silently drift."""
        import re

        source = self._find_rust_portal_tokens_source().read_text(encoding="utf-8")
        # Extract the `mod defaults { ... }` block.
        marker = "mod defaults {"
        start = source.index(marker)
        depth = 0
        end = start
        for idx in range(start + len(marker) - 1, len(source)):
            ch = source[idx]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    end = idx
                    break
        block = source[start:end]
        rust_defaults = dict(
            re.findall(r'pub const (\w+): &str = "([^"]*)";', block)
        )
        self.assertTrue(rust_defaults, "failed to parse any Rust portal defaults")
        # Every Rust default must be mirrored with the identical value in Python.
        self.assertEqual(
            rust_defaults,
            portal_tokens._RUST_DEFAULTS,
            "Python portal-token default mirror drifted from Rust "
            "crates/tze_hud_config/src/portal_tokens.rs — update "
            "portal_part_tokens._RUST_DEFAULTS to match.",
        )

    def test_resolver_falls_back_to_canonical_defaults(self) -> None:
        tokens = portal_tokens.resolve_portal_tokens({})
        # Canonical frame background #0A0D11 → opaque near-black (matches the
        # transcript pane; hud-a328c — was the opaque slate #111720 that read as
        # a grey frame rim; translucent #0000004D was rejected as backdrop-dependent).
        self.assertEqual(
            tuple(round(c, 4) for c in tokens.frame_background),
            (round(0x0A / 255, 4), round(0x0D / 255, 4), round(0x11 / 255, 4), 1.0),
        )
        self.assertEqual(tokens.header_font_size_px, 16.0)
        # An unparseable override is ignored in favor of the canonical default.
        bad = portal_tokens.resolve_portal_tokens(
            {portal_tokens.PORTAL_TOKEN_FRAME_BACKGROUND: "not-a-color"}
        )
        self.assertEqual(bad.frame_background, tokens.frame_background)

    def test_exemplar_publish_path_sources_frame_from_tokens(self) -> None:
        """The published portal frame color IS the resolved token, not a literal."""
        portal.apply_visual_profile(None)  # ensure exemplar profile is active
        root, _children = portal.build_portal_nodes(
            title="t", subtitle="s", body="b", footer_meta="f",
        )
        published = (
            root.solid_color.color.r, root.solid_color.color.g,
            root.solid_color.color.b, root.solid_color.color.a,
        )
        self.assertEqual(
            tuple(round(c, 6) for c in published),
            tuple(round(c, 6) for c in portal.TOKENS.frame_background),
            "portal frame must be published from the resolved frame_background token",
        )

    def test_profile_swap_reskins_published_values(self) -> None:
        """Swapping the active profile changes the published visual values —
        the end-to-end proof the exemplar is token-driven, not literal."""
        try:
            portal.apply_visual_profile(None)
            base_root, _ = portal.build_portal_nodes(
                title="t", subtitle="s", body="b", footer_meta="f",
            )
            base_frame = (base_root.solid_color.color.r, base_root.solid_color.color.g,
                          base_root.solid_color.color.b, base_root.solid_color.color.a)
            base_body_font = portal.BODY_FONT

            # Apply the 'expanded' profile (warm frame + larger type).
            portal.apply_visual_profile(
                portal.profile_swap_overrides("expanded", 20.0, 18.0)
            )
            swapped_root, _ = portal.build_portal_nodes(
                title="t", subtitle="s", body="b", footer_meta="f",
            )
            swapped_frame = (swapped_root.solid_color.color.r, swapped_root.solid_color.color.g,
                             swapped_root.solid_color.color.b, swapped_root.solid_color.color.a)

            self.assertNotEqual(
                base_frame, swapped_frame,
                "published frame color must change when the profile is swapped",
            )
            self.assertEqual(portal.BODY_FONT, 18.0,
                             "body font must track the swapped profile's typography")
            self.assertNotEqual(base_body_font, portal.BODY_FONT)
            # The published frame still equals the (new) resolved token — no literal.
            self.assertEqual(
                tuple(round(c, 6) for c in swapped_frame),
                tuple(round(c, 6) for c in portal.TOKENS.frame_background),
            )
        finally:
            portal.apply_visual_profile(None)

        # Restoring the exemplar profile returns the original published values.
        restored_root, _ = portal.build_portal_nodes(
            title="t", subtitle="s", body="b", footer_meta="f",
        )
        restored_frame = (restored_root.solid_color.color.r, restored_root.solid_color.color.g,
                          restored_root.solid_color.color.b, restored_root.solid_color.color.a)
        self.assertEqual(base_frame, restored_frame)
        self.assertEqual(portal.BODY_FONT, 16.0)


class RuntimeHandshakeTokenTests(unittest.TestCase):
    """hud-16um0: the exemplar PREFERS the runtime's resolved portal tokens
    delivered on the session handshake (`SessionEstablished.portal_part_tokens`)
    over its local client-side mirror, and falls back to the mirror only when the
    runtime does not expose them. The runtime map is parsed by the SAME
    drift-guarded resolver as a local profile override map."""

    def tearDown(self) -> None:
        # Restore the local exemplar profile so other tests stay isolated.
        portal.apply_visual_profile(None)

    def test_adopt_runtime_tokens_drives_published_values(self) -> None:
        # A runtime whose ACTIVE profile paints a magenta frame — distinct from
        # both the exemplar profile and the canonical default.
        runtime_frame = "#FF00FFEE"
        runtime_map = dict(portal_tokens.CANONICAL_DEFAULTS)
        runtime_map[portal_tokens.PORTAL_TOKEN_FRAME_BACKGROUND] = runtime_frame

        # Baseline: local exemplar profile.
        portal.apply_visual_profile(None)
        local_frame = portal.TOKENS.frame_background

        adopted = portal.adopt_runtime_tokens(runtime_map)
        self.assertTrue(adopted, "non-empty runtime tokens must be adopted")

        expected = portal_tokens.parse_color_hex(runtime_frame)
        self.assertEqual(
            portal.TOKENS.frame_background, expected,
            "runtime-delivered frame token must drive the live TOKENS",
        )
        self.assertNotEqual(
            portal.TOKENS.frame_background, local_frame,
            "runtime tokens must override the local exemplar mirror",
        )

        # The PUBLISHED frame node equals the runtime-delivered value — the
        # runtime's active profile drives the live portal, not a literal/mirror.
        root, _ = portal.build_portal_nodes(
            title="t", subtitle="s", body="b", footer_meta="f",
        )
        published = (
            root.solid_color.color.r, root.solid_color.color.g,
            root.solid_color.color.b, root.solid_color.color.a,
        )
        self.assertEqual(
            tuple(round(c, 6) for c in published),
            tuple(round(c, 6) for c in expected),
        )

    def test_empty_runtime_tokens_falls_back_to_local_mirror(self) -> None:
        # An older runtime omits the field → empty map → keep the local mirror.
        portal.apply_visual_profile(None)
        local_frame = portal.TOKENS.frame_background
        adopted = portal.adopt_runtime_tokens({})
        self.assertFalse(
            adopted, "empty runtime tokens (older runtime) must not be adopted",
        )
        self.assertEqual(
            portal.TOKENS.frame_background, local_frame,
            "local exemplar mirror stays authoritative on fallback",
        )

    def test_runtime_tokens_use_same_drift_guarded_resolver(self) -> None:
        # Adopting the runtime map is identical to resolving it through the
        # mirror directly, so the 7jrj3 drift-guard still governs the parse path.
        runtime_map = dict(portal_tokens.CANONICAL_DEFAULTS)
        runtime_map[portal_tokens.PORTAL_TOKEN_HEADER_FONT_SIZE] = "22"
        portal.adopt_runtime_tokens(runtime_map)
        self.assertEqual(portal.TOKENS.header_font_size_px, 22.0)
        direct = portal_tokens.resolve_portal_tokens(runtime_map)
        self.assertEqual(
            portal.TOKENS, direct,
            "runtime adoption must match resolving the map through the mirror",
        )


class TwoPaneGeometryProfileTests(unittest.TestCase):
    """hud-q1qzw: the exemplar's two-pane chrome GEOMETRY (header height, pane
    split, content inset, corner radius) resolves from an exemplar-local geometry
    profile, and a geometry-profile swap re-lays the published chrome — the
    geometry analogue of the 7jrj3 color/font reskin proof. This surface is
    deliberately exemplar-local (the runtime has no two-pane concept), so its
    keys must stay OUT of the canonical product token vocabulary."""

    def tearDown(self) -> None:
        # Restore the reviewed geometry so other tests stay isolated.
        portal.apply_geometry_profile(None)

    def _published_frame_radius(self) -> float:
        root, _ = portal.build_portal_nodes(
            title="t", subtitle="s", body="b", footer_meta="f",
        )
        return root.solid_color.radius

    def test_exemplar_defaults_reproduce_reviewed_chrome_geometry(self) -> None:
        """Resolving with no overrides reproduces the reviewed literals exactly —
        the guard against a visual regression from the literal→token move."""
        portal.apply_geometry_profile(None)
        g = portal.GEOMETRY
        self.assertEqual(g.header_height_px, 52.0)
        self.assertEqual(g.footer_height_px, 30.0)
        self.assertEqual(g.divider_height_px, 1.0)
        self.assertEqual(g.content_inset_px, 18.0)
        self.assertEqual(g.corner_radius_px, 14.0)
        self.assertEqual(g.pane_split_ratio, 0.5)
        self.assertEqual(g.pane_divider_width_px, 6.0)
        self.assertEqual(g.min_pane_width_px, 240.0)
        # The bound module constants track the resolved geometry (no literals).
        self.assertEqual(portal.HEADER_H, 52.0)
        self.assertEqual(portal.FOOTER_H, 30.0)
        self.assertEqual(portal.PADDING_X, 18.0)
        self.assertEqual(portal.PORTAL_RADIUS, 14.0)
        self.assertEqual(portal.PANE_DIVIDER_W, 6.0)
        self.assertEqual(portal.MIN_PANE_W, 240.0)
        # Default 0.5 split reproduces the equal 50/50 pane widths.
        self.assertEqual(portal.INPUT_PANE_W, (portal.PORTAL_W - 6.0) / 2.0)
        # Published frame corner radius IS the resolved token, not a literal.
        self.assertEqual(self._published_frame_radius(), 14.0)

    def test_geometry_profile_swap_relays_published_chrome(self) -> None:
        """Swapping the geometry profile changes the PUBLISHED chrome geometry —
        the end-to-end proof the two-pane geometry is profile-driven, not literal."""
        portal.apply_geometry_profile(None)
        base_input, base_output = portal.portal_pane_rects()
        base_radius = self._published_frame_radius()
        self.assertEqual(base_radius, 14.0)

        # A taller header, an input-heavy split, and a tighter corner radius.
        portal.apply_geometry_profile(
            {
                pg.PORTAL_TWO_PANE_HEADER_HEIGHT_PX: "80",
                pg.PORTAL_TWO_PANE_SPLIT_RATIO: "0.65",
                pg.PORTAL_TWO_PANE_CORNER_RADIUS_PX: "4",
            }
        )
        self.assertEqual(portal.HEADER_H, 80.0)
        self.assertEqual(portal.PORTAL_RADIUS, 4.0)

        swapped_input, swapped_output = portal.portal_pane_rects()
        # Taller header pushes the pane band down (pane_y = HEADER_H + DIVIDER_H).
        self.assertGreater(swapped_output.y, base_output.y)
        # Input-heavy split widens the input pane and narrows the output pane.
        self.assertGreater(portal.INPUT_PANE_W, (portal.PORTAL_W - 6.0) / 2.0)
        self.assertLess(swapped_output.w, base_output.w)
        # Published frame radius tracks the swapped token — still no literal.
        self.assertEqual(self._published_frame_radius(), 4.0)
        self.assertNotEqual(self._published_frame_radius(), base_radius)
        # Panes still tile the frame exactly after the split-ratio swap.
        input_w, output_w = portal.partition_pane_widths(
            portal.PORTAL_W, portal.INPUT_PANE_W
        )
        self.assertEqual(input_w + portal.PANE_DIVIDER_W + output_w, portal.PORTAL_W)

        # Restoring the exemplar geometry returns the original published chrome.
        portal.apply_geometry_profile(None)
        restored_input, restored_output = portal.portal_pane_rects()
        self.assertEqual(base_output.y, restored_output.y)
        self.assertEqual(base_output.w, restored_output.w)
        self.assertEqual(self._published_frame_radius(), 14.0)

    def test_unparseable_geometry_override_falls_back_to_default(self) -> None:
        """A bad override value resolves to the exemplar default (warn-and-default),
        mirroring the visual resolver's fallback semantics."""
        portal.apply_geometry_profile(
            {pg.PORTAL_TWO_PANE_HEADER_HEIGHT_PX: "not-a-number"}
        )
        self.assertEqual(portal.GEOMETRY.header_height_px, 52.0)

    def test_two_pane_geometry_keys_are_not_canonical_product_tokens(self) -> None:
        """The two-pane geometry surface is exemplar-local by design: its keys
        must never leak into the canonical product token vocabulary (the runtime
        has no two-pane concept, so they would be dead product surface)."""
        for key in pg.EXEMPLAR_GEOMETRY_DEFAULTS:
            self.assertTrue(key.startswith("portal.two_pane."))
            self.assertNotIn(key, portal_tokens.CANONICAL_DEFAULTS)


class CadencePresentAckTests(unittest.TestCase):
    """The cadence runtime-overhead axis is non-vacuous only when present_ms is a
    TRUE on-screen present time (FramePresented, hud-91uu6) rather than the
    transport-RTT proxy where present≈rtt made overhead≈0 (hud-vjlqh)."""

    def test_present_ms_from_frame_ack_derives_run_relative_present(self) -> None:
        # Publish 100ms into the run; batch sent at wall 1_000_000us; the frame
        # carrying it presented 8ms later -> present_ms = 100 + 8 = 108ms.
        present_ms = portal.present_ms_from_frame_ack(100.0, 1_000_000, 1_008_000)
        self.assertIsNotNone(present_ms)
        self.assertAlmostEqual(present_ms, 108.0, places=6)

    def test_present_ms_from_frame_ack_rejects_clock_skew(self) -> None:
        # Present precedes send (cross-host skew / mismatched domain) -> None so
        # the caller falls back to the proxy instead of a negative latency.
        self.assertIsNone(
            portal.present_ms_from_frame_ack(100.0, 1_008_000, 1_000_000)
        )

    def test_frame_ack_present_is_distinct_from_rtt_proxy(self) -> None:
        # One cadence cycle: publish at t=100ms, transport RTT (send->ack) 2ms,
        # but the frame carrying the batch presented 9ms after send.
        publish_ms = 100.0
        rtt_ms = 2.0
        send_wall_us = 5_000_000
        present_wall_us = send_wall_us + 9_000  # 9ms true present latency

        proxy_present_ms = publish_ms + rtt_ms  # old vacuous proxy (present≈rtt)
        frame_ack_present_ms = portal.present_ms_from_frame_ack(
            publish_ms, send_wall_us, present_wall_us,
        )
        self.assertIsNotNone(frame_ack_present_ms)
        # The presented-path present_ms is NOT the RTT-proxy present_ms.
        self.assertNotAlmostEqual(
            frame_ack_present_ms, proxy_present_ms, places=3
        )

        def overhead(present_ms: float) -> float:
            ev = portal.build_cadence_rtt_evidence(
                rtt_baseline_ms=rtt_ms,
                appends=[{
                    "cycle": 1, "body_lines": 8,
                    "publish_ms": publish_ms, "present_ms": present_ms,
                    "rtt_ms": rtt_ms,
                }],
                cadence_cycles=1, cadence_interval_ms=100,
            )
            return ev["appends"][0]

        proxy_append = overhead(proxy_present_ms)
        frame_ack_append = overhead(frame_ack_present_ms)
        # Proxy: present≈rtt -> runtime overhead ~0 (the vacuous axis).
        self.assertAlmostEqual(proxy_append["overhead_ms"], 0.0, places=3)
        # Frame-ack: present_latency=9ms, overhead=9-2=7ms > 0 (non-vacuous).
        self.assertEqual(frame_ack_append["present_latency_ms"], 9.0)
        self.assertAlmostEqual(frame_ack_append["overhead_ms"], 7.0, places=3)
        self.assertGreater(frame_ack_append["overhead_ms"], 0.0)

    def test_client_correlates_batch_id_to_frame_presented(self) -> None:
        from hud_grpc_client import HudClient
        from proto_gen import events_pb2

        client = HudClient("localhost:1", psk="x")
        batch_id = b"\x11" * 16
        client._record_batch_send(batch_id, 7_000_000)
        self.assertEqual(client.batch_send_wall_us(batch_id), 7_000_000)
        self.assertEqual(client.last_mutation_batch_id, batch_id)
        # No present-ack observed yet.
        self.assertIsNone(client.present_wall_us_for_batch(batch_id))
        # A FramePresented carrying the batch, as the read loop would append.
        client._frame_presented_events.append(
            events_pb2.FramePresented(
                frame_number=42, present_wall_us=7_012_000, batch_ids=[batch_id],
            )
        )
        self.assertEqual(client.present_wall_us_for_batch(batch_id), 7_012_000)
        # An unrelated batch does not match.
        self.assertIsNone(client.present_wall_us_for_batch(b"\x22" * 16))

    def test_batch_send_tracking_is_bounded(self) -> None:
        from hud_grpc_client import HudClient

        client = HudClient("localhost:1", psk="x")
        cap = client._MAX_TRACKED_BATCH_SENDS
        for n in range(cap + 5):
            client._record_batch_send(n.to_bytes(16, "big"), n)
        self.assertLessEqual(len(client._batch_send_wall_us), cap)
        # Oldest sends evicted; newest retained.
        self.assertIsNone(client.batch_send_wall_us((0).to_bytes(16, "big")))
        self.assertEqual(
            client.batch_send_wall_us((cap + 4).to_bytes(16, "big")), cap + 4,
        )


class FirstClassPortalSurfaceTests(unittest.TestCase):
    """The exemplar drives the promoted portal through the first-class surface
    API (SetPortalSurface + UpdatePortalSurfaceState) in addition to raw tiles
    (hud-rpm9s)."""

    def setUp(self) -> None:
        import types_pb2

        self.types_pb2 = types_pb2
        # Reset the one-shot declaration guard so each test starts undeclared.
        portal._PORTAL_SURFACE_DECLARED = False

    def test_surface_declares_eight_distinct_parts_and_identity(self) -> None:
        t = self.types_pb2
        surface = portal.build_portal_surface_proto("Exemplar Portal")
        kinds = [p.kind for p in surface.parts]
        self.assertEqual(len(kinds), 8, "must declare all eight named parts")
        self.assertEqual(len(set(kinds)), 8, "no duplicate part kind")
        for expected in (
            t.PORTAL_PART_KIND_FRAME,
            t.PORTAL_PART_KIND_HEADER,
            t.PORTAL_PART_KIND_COMPOSER,
            t.PORTAL_PART_KIND_TRANSCRIPT,
            t.PORTAL_PART_KIND_DIVIDER,
            t.PORTAL_PART_KIND_COLLAPSED_CARD,
            t.PORTAL_PART_KIND_CAPTURE_BACKSTOP,
            t.PORTAL_PART_KIND_GESTURE_SHIELD,
        ):
            self.assertIn(expected, kinds)
        self.assertEqual(surface.identity.session_id, portal.PORTAL_SURFACE_SESSION_ID)
        self.assertEqual(surface.identity.display_name, "Exemplar Portal")
        self.assertEqual(
            surface.identity.peer_class, t.PORTAL_PEER_CLASS_RESIDENT_LLM
        )
        # Parts are derived (node empty); the raw tiles paint the pixels.
        for part in surface.parts:
            self.assertEqual(part.node, b"", "declared parts must be derived (node empty)")
            self.assertGreaterEqual(part.bounds.width, 0.0)
            self.assertGreaterEqual(part.bounds.height, 0.0)

    def test_state_patch_leaves_unspecified_fields_unchanged(self) -> None:
        t = self.types_pb2
        m = portal.update_portal_surface_state_mutation(
            b"\x00" * 16, display_state=t.PORTAL_DISPLAY_STATE_COLLAPSED
        )
        self.assertEqual(m.WhichOneof("mutation"), "update_portal_surface_state")
        ups = m.update_portal_surface_state
        # Unset lifecycle stays UNSPECIFIED = "leave unchanged" (coalescible).
        self.assertEqual(ups.lifecycle, t.PORTAL_LIFECYCLE_STATE_UNSPECIFIED)
        self.assertEqual(ups.display_state, t.PORTAL_DISPLAY_STATE_COLLAPSED)

    def test_drive_declares_once_then_only_patches(self) -> None:
        t = self.types_pb2

        class FakeClient:
            def __init__(self) -> None:
                self.batches: list[list] = []

            async def submit_mutation_batch(self, lease_id, mutations):
                self.batches.append(list(mutations))

        tiles = mock.Mock()
        tiles.frame = b"\x01" * 16
        client = FakeClient()

        async def run() -> None:
            # First drive with declare=True → SetPortalSurface + patch.
            await portal.drive_portal_surface(
                client, b"\x02" * 16, tiles, "Portal", declare=True
            )
            # Second drive with declare=True again → NO re-declaration, patch only.
            await portal.drive_portal_surface(
                client, b"\x02" * 16, tiles, "Portal", declare=True
            )

        asyncio.run(run())

        self.assertEqual(len(client.batches), 2)
        # First drive declares the surface ONLY — the descriptor carries full
        # state, so no same-batch patch (avoids an in-batch ordering dependency;
        # the wire applies a batch atomically).
        first_kinds = [mm.WhichOneof("mutation") for mm in client.batches[0]]
        self.assertEqual(first_kinds, ["set_portal_surface"])
        # Second drive patches ONLY — surface declared exactly once.
        second_kinds = [mm.WhichOneof("mutation") for mm in client.batches[1]]
        self.assertEqual(second_kinds, ["update_portal_surface_state"])


if __name__ == "__main__":
    unittest.main()
