#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("check_windows_perf_budgets.py")
SPEC = importlib.util.spec_from_file_location("check_windows_perf_budgets", MODULE_PATH)
assert SPEC is not None
check_windows_perf_budgets = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = check_windows_perf_budgets
SPEC.loader.exec_module(check_windows_perf_budgets)


class WindowsPerfBudgetCheckerTests(unittest.TestCase):
    def test_load_json_reports_missing_file_without_traceback(self) -> None:
        missing = Path(tempfile.gettempdir()) / "tze_hud_missing_benchmark.json"

        with self.assertRaises(SystemExit) as raised:
            check_windows_perf_budgets.load_json(missing)

        self.assertIn("unable to read", str(raised.exception))

    def test_hardware_factors_rejects_malformed_calibration(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            check_windows_perf_budgets.hardware_factors({"calibration": []})

        self.assertIn("'calibration' must be an object", str(raised.exception))

    def test_samples_rejects_malformed_bucket(self) -> None:
        with self.assertRaises(ValueError) as raised:
            check_windows_perf_budgets.samples({"frame_time": []}, "frame_time")

        self.assertEqual("frame_time: expected object", str(raised.exception))

    # ── scene_lock_misses / invariant_violations zero-baseline gate ─────────

    @staticmethod
    def _artifact_with_summary(
        summary_overrides: dict,
        paced_contention_summary_overrides: dict | None = None,
    ) -> dict:
        """Build a minimal valid benchmark artifact for all gated sessions.

        Latency buckets carry one well-under-budget sample so the percentile
        checks pass; counter values default to their healthy baseline (0) and
        are merged with ``summary_overrides``.
        """
        bucket = {"samples": [1]}
        base_summary = {
            "frame_time": bucket,
            "input_to_local_ack": bucket,
            "input_to_scene_commit": bucket,
            "input_to_next_present": bucket,
            "lease_violations": 0,
            "budget_overruns": 0,
            "sync_drift_violations": 0,
            "invariant_violations": 0,
            "scene_lock_misses": 0,
        }
        base_summary.update(summary_overrides)
        paced_summary = dict(base_summary)
        paced_summary["scene_lock_misses"] = 18
        if paced_contention_summary_overrides:
            paced_summary.update(paced_contention_summary_overrides)
        return {
            "calibration": {"factors": {"cpu": 0.854, "gpu": 0.338, "upload": 0.215}},
            "sessions": [
                {"name": name, "summary": dict(base_summary)}
                for name in check_windows_perf_budgets.PUBLISH_SESSIONS
            ]
            + [{"name": "scene_lock_paced_contention", "summary": paced_summary}],
        }

    def test_scene_lock_misses_zero_for_production_sessions(self) -> None:
        self.assertIn("scene_lock_misses", check_windows_perf_budgets.ZERO_COUNTERS)
        self.assertIn("invariant_violations", check_windows_perf_budgets.ZERO_COUNTERS)
        self.assertEqual(
            check_windows_perf_budgets.BASELINE_COUNTERS[
                ("scene_lock_paced_contention", "scene_lock_misses")
            ],
            20,
        )
        self.assertEqual(
            check_windows_perf_budgets.BASELINE_COUNTER_MINIMUMS[
                ("scene_lock_paced_contention", "scene_lock_misses")
            ],
            1,
        )

    def test_healthy_counters_pass(self) -> None:
        artifact = self._artifact_with_summary({})
        _results, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        self.assertEqual(failures, [])

    def test_a_fixed_publish_delay_fails_the_gate(self) -> None:
        # A 5 ms delay in admission moves every call; the absolute publish-call
        # budgets catch it on any host, whatever its calibration factors.
        delayed = {"samples": [5_000 + sample for sample in range(1, 101)]}
        artifact = self._artifact_with_summary({"input_to_scene_commit": delayed})
        artifact["calibration"]["factors"]["cpu"] = 8.0
        _results, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        for session in check_windows_perf_budgets.PUBLISH_SESSIONS:
            self.assertTrue(
                any(f.startswith(f"{session}.publish_call_p50") for f in failures),
                f"expected {session}.publish_call_p50 to fail, got {failures!r}",
            )

    def test_an_unknown_budget_factor_is_rejected(self) -> None:
        with self.assertRaises(ValueError) as raised:
            check_windows_perf_budgets.Budget("x_p99", "frame_time", 99.0, 1, "cpu ")
        self.assertIn("unknown budget factor", str(raised.exception))

    def test_only_the_notification_session_has_a_pointer_budget(self) -> None:
        budgets = check_windows_perf_budgets.SESSION_BUDGETS
        self.assertEqual(set(budgets), set(check_windows_perf_budgets.PUBLISH_SESSIONS))
        for session, session_budgets in budgets.items():
            has_ack = any(b.bucket == "input_to_local_ack" for b in session_budgets)
            self.assertEqual(session == "notification_publish", has_ack, session)

    def test_nonzero_scene_lock_misses_fails(self) -> None:
        artifact = self._artifact_with_summary({"scene_lock_misses": 3})
        _results, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        self.assertTrue(
            any("scene_lock_misses" in f for f in failures),
            f"expected a scene_lock_misses failure, got {failures!r}",
        )

    def test_nonzero_invariant_violations_fails(self) -> None:
        artifact = self._artifact_with_summary({"invariant_violations": 1})
        _results, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        self.assertTrue(
            any("invariant_violations" in f for f in failures),
            f"expected an invariant_violations failure, got {failures!r}",
        )

    def test_missing_scene_lock_misses_field_fails(self) -> None:
        # A summary that omits the counter entirely must not silently pass.
        artifact = self._artifact_with_summary({})
        for session in artifact["sessions"]:
            del session["summary"]["scene_lock_misses"]
        _results, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        self.assertTrue(
            any("scene_lock_misses" in f for f in failures),
            f"expected a scene_lock_misses failure for missing field, got {failures!r}",
        )

    def test_missing_paced_contention_session_fails(self) -> None:
        artifact = self._artifact_with_summary({})
        artifact["sessions"] = [
            session
            for session in artifact["sessions"]
            if session["name"] != "scene_lock_paced_contention"
        ]

        with self.assertRaises(SystemExit) as raised:
            check_windows_perf_budgets.validate_benchmark(artifact)

        self.assertIn("scene_lock_paced_contention", str(raised.exception))

    def test_paced_contention_scene_lock_misses_ceiling_enforced(self) -> None:
        floor = self._artifact_with_summary(
            {},
            paced_contention_summary_overrides={"scene_lock_misses": 1},
        )
        _r, failures_floor = check_windows_perf_budgets.validate_benchmark(floor)
        self.assertEqual(failures_floor, [])

        under = self._artifact_with_summary(
            {},
            paced_contention_summary_overrides={"scene_lock_misses": 20},
        )
        _r, failures_under = check_windows_perf_budgets.validate_benchmark(under)
        self.assertEqual(failures_under, [])

        over = self._artifact_with_summary(
            {},
            paced_contention_summary_overrides={"scene_lock_misses": 21},
        )
        _r, failures_over = check_windows_perf_budgets.validate_benchmark(over)
        self.assertTrue(
            any("scene_lock_paced_contention.scene_lock_misses" in f for f in failures_over),
            f"expected paced contention scene_lock_misses ceiling failure, got {failures_over!r}",
        )

    def test_paced_contention_scene_lock_misses_zero_fails(self) -> None:
        artifact = self._artifact_with_summary(
            {},
            paced_contention_summary_overrides={"scene_lock_misses": 0},
        )
        _r, failures = check_windows_perf_budgets.validate_benchmark(artifact)
        self.assertTrue(
            any("expected 1 <= observed <= 20" in f for f in failures),
            f"expected paced contention lower-bound failure, got {failures!r}",
        )


if __name__ == "__main__":
    unittest.main()
