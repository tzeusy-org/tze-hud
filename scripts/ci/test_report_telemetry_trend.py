#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("report_telemetry_trend.py")
SPEC = importlib.util.spec_from_file_location("report_telemetry_trend", MODULE_PATH)
assert SPEC is not None
report_telemetry_trend = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = report_telemetry_trend
SPEC.loader.exec_module(report_telemetry_trend)

trend = report_telemetry_trend


def _latency_result(session: str, metric: str, observed_us: int, passed: bool = True) -> dict:
    return {
        "session": session,
        "metric": metric,
        "percentile": 99.0,
        "observed_us": observed_us,
        "effective_budget_us": 8300,
        "pass": passed,
    }


def _counter_result(session: str, metric: str, observed: int, passed: bool = True) -> dict:
    return {
        "session": session,
        "metric": metric,
        "observed": observed,
        "budget": 0,
        "pass": passed,
    }


def _report(results: list[dict]) -> dict:
    return {
        "schema": trend.GATE_SCHEMA,
        "results": results,
        "failures": [],
        "verdict": "pass",
    }


class ComputeDeltasTests(unittest.TestCase):
    def test_latency_delta_and_pct(self) -> None:
        previous = _report([_latency_result("steady_state_render", "frame_time_p99", 14100)])
        current = _report([_latency_result("steady_state_render", "frame_time_p99", 14800)])
        deltas = trend.compute_deltas(current, previous)
        self.assertEqual(len(deltas), 1)
        entry = deltas[0]
        self.assertEqual(entry["previous"], 14100)
        self.assertEqual(entry["current"], 14800)
        self.assertEqual(entry["delta"], 700)
        self.assertEqual(entry["pct"], "+5.0%")
        self.assertEqual(entry["unit"], "us")
        self.assertTrue(entry["within_budget"])

    def test_render_line_for_latency(self) -> None:
        previous = _report([_latency_result("steady_state_render", "frame_time_p99", 14100)])
        current = _report([_latency_result("steady_state_render", "frame_time_p99", 14800)])
        line = trend.render_line(trend.compute_deltas(current, previous)[0])
        self.assertEqual(
            line,
            "steady_state_render.frame_time_p99: 14100us -> 14800us "
            "(+700us, +5.0%, within budget)",
        )

    def test_negative_delta_improvement(self) -> None:
        previous = _report([_latency_result("high_mutation", "frame_time_p99", 2000)])
        current = _report([_latency_result("high_mutation", "frame_time_p99", 1800)])
        entry = trend.compute_deltas(current, previous)[0]
        self.assertEqual(entry["delta"], -200)
        self.assertEqual(entry["pct"], "-10.0%")
        line = trend.render_line(entry)
        self.assertIn("2000us -> 1800us (-200us, -10.0%", line)

    def test_unchanged_value_is_plus_zero(self) -> None:
        previous = _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        current = _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        entry = trend.compute_deltas(current, previous)[0]
        self.assertEqual(entry["delta"], 0)
        self.assertEqual(entry["pct"], "+0.0%")

    def test_counter_zero_baseline_creep_pct_is_na(self) -> None:
        # 0 -> 2 has no meaningful percentage; absolute delta carries the signal.
        previous = _report([_counter_result("steady_state_render", "scene_lock_misses", 0)])
        current = _report(
            [_counter_result("steady_state_render", "scene_lock_misses", 2, passed=False)]
        )
        entry = trend.compute_deltas(current, previous)[0]
        self.assertEqual(entry["delta"], 2)
        self.assertEqual(entry["pct"], "n/a")
        self.assertFalse(entry["within_budget"])
        line = trend.render_line(entry)
        self.assertIn("0 -> 2 (+2, n/a, OVER BUDGET)", line)

    def test_new_metric_without_prior_value(self) -> None:
        previous = _report([])
        current = _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        entry = trend.compute_deltas(current, previous)[0]
        self.assertIsNone(entry["previous"])
        self.assertIsNone(entry["delta"])
        self.assertIsNone(entry["pct"])
        line = trend.render_line(entry)
        self.assertEqual(
            line,
            "steady_state_render.frame_time_p99: 5000us "
            "(no prior value, within budget)",
        )

    def test_untracked_metric_is_skipped(self) -> None:
        # input_to_local_ack_p99 is tracked; a hypothetical untracked metric is not.
        previous = _report([_counter_result("steady_state_render", "lease_violations", 0)])
        current = _report([_counter_result("steady_state_render", "lease_violations", 0)])
        self.assertEqual(trend.compute_deltas(current, previous), [])

    def test_results_sorted_by_session_then_metric(self) -> None:
        previous = _report(
            [
                _latency_result("steady_state_render", "frame_time_p99", 100),
                _latency_result("high_mutation", "frame_time_p99", 100),
            ]
        )
        current = _report(
            [
                _latency_result("steady_state_render", "frame_time_p99", 110),
                _latency_result("high_mutation", "frame_time_p99", 120),
            ]
        )
        deltas = trend.compute_deltas(current, previous)
        self.assertEqual(
            [(d["session"], d["metric"]) for d in deltas],
            [
                ("high_mutation", "frame_time_p99"),
                ("steady_state_render", "frame_time_p99"),
            ],
        )


class MarkdownAndIndexTests(unittest.TestCase):
    def test_markdown_table_contains_header_and_rows(self) -> None:
        previous = _report([_latency_result("steady_state_render", "frame_time_p99", 14100)])
        current = _report([_latency_result("steady_state_render", "frame_time_p99", 14800)])
        table = trend.render_markdown_table(trend.compute_deltas(current, previous))
        self.assertIn("| Session | Metric | Previous | Current | Delta | % | Budget |", table)
        self.assertIn("| steady_state_render | frame_time_p99 | 14100us | 14800us |", table)
        self.assertIn("+5.0%", table)

    def test_index_results_tolerates_malformed(self) -> None:
        report = {"schema": trend.GATE_SCHEMA, "results": "not-a-list"}
        self.assertEqual(trend.index_results(report), {})

    def test_observed_value_rejects_bool(self) -> None:
        self.assertIsNone(trend.observed_value({"observed": True}))
        self.assertEqual(trend.observed_value({"observed": 0}), 0)
        self.assertEqual(trend.observed_value({"observed_us": 5}), 5)


class MainEntrypointTests(unittest.TestCase):
    def _write(self, payload: dict) -> Path:
        handle = tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False, encoding="utf-8"
        )
        json.dump(payload, handle)
        handle.close()
        return Path(handle.name)

    def _run_main(self, argv: list[str], step_summary: Path | None) -> int:
        old_argv = sys.argv
        old_env = trend.os.environ.get("GITHUB_STEP_SUMMARY")
        sys.argv = ["report_telemetry_trend.py", *argv]
        if step_summary is not None:
            trend.os.environ["GITHUB_STEP_SUMMARY"] = str(step_summary)
        else:
            trend.os.environ.pop("GITHUB_STEP_SUMMARY", None)
        try:
            return trend.main()
        finally:
            sys.argv = old_argv
            if old_env is not None:
                trend.os.environ["GITHUB_STEP_SUMMARY"] = old_env
            else:
                trend.os.environ.pop("GITHUB_STEP_SUMMARY", None)

    def test_no_baseline_arg_exits_zero(self) -> None:
        # No main-pinned baseline supplied.
        current = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        )
        summary = Path(tempfile.mktemp(suffix=".md"))
        rc = self._run_main(["--current", str(current)], summary)
        self.assertEqual(rc, 0)
        text = summary.read_text(encoding="utf-8")
        self.assertIn("No main-pinned Windows performance baseline available", text)

    def test_baseline_arg_points_to_missing_file_exits_zero(self) -> None:
        current = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        )
        missing = Path(tempfile.gettempdir()) / "tze_hud_no_such_baseline.json"
        summary = Path(tempfile.mktemp(suffix=".md"))
        rc = self._run_main(
            ["--current", str(current), "--baseline", str(missing)], summary
        )
        self.assertEqual(rc, 0)
        self.assertIn(
            "No main-pinned Windows performance baseline available",
            summary.read_text(encoding="utf-8"),
        )

    def test_full_run_against_baseline_writes_table(self) -> None:
        # The qualifying main-pinned budget summary is the comparison.
        baseline = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 14100)])
        )
        current = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 14800)])
        )
        summary = Path(tempfile.mktemp(suffix=".md"))
        rc = self._run_main(
            ["--current", str(current), "--baseline", str(baseline)], summary
        )
        self.assertEqual(rc, 0)
        text = summary.read_text(encoding="utf-8")
        self.assertIn("14100us | 14800us", text)
        self.assertIn("+5.0%", text)

    def test_bad_optional_baseline_is_informational(self) -> None:
        valid = _report([_latency_result("steady_state_render", "frame_time_p99", 5000)])
        with tempfile.TemporaryDirectory() as directory:
            current = Path(directory) / "current.json"
            current.write_text(json.dumps(valid), encoding="utf-8")
            baseline = Path(directory) / "baseline.json"
            for payload in (
                b"\xff",
                b"{",
                b"{}",
                json.dumps({**valid, "verdict": "fail"}).encode(),
                json.dumps({**valid, "failures": ["over budget"]}).encode(),
                json.dumps({**valid, "results": None}).encode(),
            ):
                with self.subTest(payload=payload):
                    baseline.write_bytes(payload)
                    summary = Path(directory) / "summary.md"
                    rc = self._run_main(
                        ["--current", str(current), "--baseline", str(baseline)], summary
                    )
                    self.assertEqual(rc, 0)
                    self.assertIn(
                        "No main-pinned Windows performance baseline available",
                        summary.read_text(encoding="utf-8"),
                    )
                    self.assertNotIn("| Session |", summary.read_text(encoding="utf-8"))
                    summary.unlink()

    def test_bad_current_report_remains_fatal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            current = Path(directory) / "current.json"
            current.write_bytes(b"\xff")
            baseline = Path(directory) / "baseline.json"
            baseline.write_text(json.dumps(_report([])), encoding="utf-8")
            summary = Path(directory) / "summary.md"
            with self.assertRaisesRegex(SystemExit, "current.json: invalid JSON"):
                self._run_main(["--current", str(current), "--baseline", str(baseline)], summary)
            self.assertFalse(summary.exists())

    def test_invalid_optional_number_suppresses_all_trend_deltas(self) -> None:
        latency = _latency_result("steady_state_render", "frame_time_p99", 5000)
        counter = _counter_result("steady_state_render", "scene_lock_misses", 0)
        with tempfile.TemporaryDirectory() as directory:
            current = Path(directory) / "current.json"
            current.write_text(json.dumps(_report([latency, counter])), encoding="utf-8")
            baseline = Path(directory) / "baseline.json"
            summary = Path(directory) / "summary.md"
            for field, value in (
                ("observed_us", float("nan")),
                ("observed_us", float("inf")),
                ("observed_us", float("-inf")),
                ("observed", 10**400),
            ):
                with self.subTest(field=field, value=value):
                    results = [
                        {**item, field: value} if field in item else item
                        for item in (latency, counter)
                    ]
                    baseline.write_text(json.dumps(_report(results)), encoding="utf-8")
                    rc = self._run_main(
                        ["--current", str(current), "--baseline", str(baseline)], summary
                    )
                    self.assertEqual(rc, 0)
                    notice = summary.read_text(encoding="utf-8")
                    self.assertIn("No main-pinned Windows performance baseline available", notice)
                    self.assertNotIn("| Session |", notice)
                    summary.unlink()

    def test_previous_alias_still_accepted(self) -> None:
        # ``--previous`` is retained as a back-compat alias for ``--baseline``.
        baseline = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 14100)])
        )
        current = self._write(
            _report([_latency_result("steady_state_render", "frame_time_p99", 14800)])
        )
        summary = Path(tempfile.mktemp(suffix=".md"))
        rc = self._run_main(
            ["--current", str(current), "--previous", str(baseline)], summary
        )
        self.assertEqual(rc, 0)
        self.assertIn("14100us | 14800us", summary.read_text(encoding="utf-8"))


class LoadReportTests(unittest.TestCase):
    def test_rejects_wrong_schema(self) -> None:
        handle = tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False, encoding="utf-8"
        )
        json.dump({"schema": "something.else", "results": []}, handle)
        handle.close()
        with self.assertRaises(SystemExit) as raised:
            trend.load_report(Path(handle.name))
        self.assertIn("unexpected schema", str(raised.exception))

    def test_missing_file_reports_cleanly(self) -> None:
        missing = Path(tempfile.gettempdir()) / "tze_hud_missing_trend_report.json"
        with self.assertRaises(SystemExit) as raised:
            trend.load_report(missing)
        self.assertIn("unable to read", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
