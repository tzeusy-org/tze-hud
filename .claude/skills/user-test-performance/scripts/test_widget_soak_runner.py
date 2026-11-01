#!/usr/bin/env python3
"""Focused tests for widget_soak_runner live-metrics artifact handling."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("widget_soak_runner.py")
SPEC = importlib.util.spec_from_file_location("widget_soak_runner", SCRIPT)
assert SPEC is not None
widget_soak_runner = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(widget_soak_runner)


def write_json(path: Path, payload: dict) -> None:
    path.write_text(json.dumps(payload), encoding="utf-8")


class LiveMetricsArtifactTests(unittest.TestCase):
    def test_windowed_artifact_extracts_frame_and_input_percentiles(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "windowed.json"
            write_json(
                path,
                {
                    "schema": "tze_hud.windowed_compositor_benchmark.v1",
                    "benchmark": {"recorded_frames": 4},
                    "frame_time": {
                        "p50_us": 20,
                        "p99_us": 40,
                        "p99_9_us": 40,
                        "peak_us": 40,
                    },
                    "summary": {
                        "frame_time": {"samples": [10, 20, 30, 40]},
                        "input_to_local_ack": {"samples": [1, 2, 3]},
                        "input_to_scene_commit": {"samples": [10, 20, 30]},
                        "input_to_next_present": {"samples": [16, 32, 48]},
                    },
                },
            )

            result = widget_soak_runner.load_live_metrics_artifact(path)

            self.assertTrue(result["ok"], result)
            self.assertEqual(result["frame_time"]["p50_us"], 20)
            self.assertEqual(result["frame_time"]["p99_us"], 40)
            self.assertEqual(result["frame_time"]["p99_9_us"], 40)
            self.assertEqual(result["input_latency"]["input_to_local_ack"]["p99_us"], 3)
            self.assertEqual(result["input_latency"]["input_to_scene_commit"]["p95_us"], 30)
            self.assertEqual(result["input_latency"]["input_to_next_present"]["p50_us"], 32)

    def test_missing_input_triple_is_explicit_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "frame_only.json"
            write_json(
                path,
                {
                    "schema": "tze_hud.windowed_compositor_benchmark.v1",
                    "frame_time": {"p50_us": 20, "p99_us": 40, "p99_9_us": 40},
                    "summary": {"frame_time": {"samples": [10, 20, 30, 40]}},
                },
            )

            result = widget_soak_runner.load_live_metrics_artifact(path)

            self.assertFalse(result["ok"], result)
            self.assertIn("input_to_local_ack.p99_us", result["missing_metrics"])
            self.assertIn("input_to_scene_commit.sample_count", result["missing_metrics"])
            self.assertIn("input_to_next_present.p50_us", result["missing_metrics"])

    def test_missing_artifact_reports_all_required_fields(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "missing.json"

            result = widget_soak_runner.load_live_metrics_artifact(path)

            self.assertFalse(result["ok"], result)
            self.assertEqual(
                result["missing_metrics"],
                [
                    "frame_time.p50_us",
                    "frame_time.p99_us",
                    "frame_time.p99_9_us",
                    "frame_time.sample_count",
                    "input_to_local_ack.p50_us",
                    "input_to_local_ack.p95_us",
                    "input_to_local_ack.p99_us",
                    "input_to_local_ack.sample_count",
                    "input_to_scene_commit.p50_us",
                    "input_to_scene_commit.p95_us",
                    "input_to_scene_commit.p99_us",
                    "input_to_scene_commit.sample_count",
                    "input_to_next_present.p50_us",
                    "input_to_next_present.p95_us",
                    "input_to_next_present.p99_us",
                    "input_to_next_present.sample_count",
                ],
            )

    def test_headless_benchmark_sessions_are_aggregated(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "benchmark.json"
            write_json(
                path,
                {
                    "calibration": {"factors": {"cpu": 1.0, "gpu": 1.0, "upload": 1.0}},
                    "sessions": [
                        {
                            "name": "steady_state_render",
                            "summary": {
                                "frame_time": {"samples": [100, 200]},
                                "input_to_local_ack": {"samples": [1, 2]},
                                "input_to_scene_commit": {"samples": [11, 12]},
                                "input_to_next_present": {"samples": [21, 22]},
                            },
                        },
                        {
                            "name": "high_mutation",
                            "summary": {
                                "frame_time": {"samples": [300, 400]},
                                "input_to_local_ack": {"samples": [3, 4]},
                                "input_to_scene_commit": {"samples": [13, 14]},
                                "input_to_next_present": {"samples": [23, 24]},
                            },
                        },
                    ],
                },
            )

            result = widget_soak_runner.load_live_metrics_artifact(path)

            self.assertTrue(result["ok"], result)
            self.assertEqual(result["frame_time"]["sample_count"], 4)
            self.assertEqual(result["frame_time"]["p99_9_us"], 400)
            self.assertEqual(result["input_latency"]["input_to_next_present"]["p99_us"], 24)
            self.assertEqual(result["sessions"], ["steady_state_render", "high_mutation"])

    def test_local_live_metrics_artifact_is_copied_into_output_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            source = tmp_path / "source.json"
            output_root = tmp_path / "soak-output"
            output_root.mkdir()
            write_json(
                source,
                {
                    "schema": "tze_hud.windowed_compositor_benchmark.v1",
                    "benchmark": {"recorded_frames": 2},
                    "frame_time": {"p50_us": 10, "p99_us": 20, "p99_9_us": 20},
                    "summary": {
                        "frame_time": {"samples": [10, 20]},
                        "input_to_local_ack": {"samples": [1, 2]},
                        "input_to_scene_commit": {"samples": [10, 20]},
                        "input_to_next_present": {"samples": [16, 32]},
                    },
                },
            )

            result = widget_soak_runner.resolve_live_metrics(
                args=Namespace(
                    allow_missing_live_metrics=False,
                    live_metrics_artifact=str(source),
                ),
                output_root=output_root,
                dry_run=False,
            )

            copied = output_root / widget_soak_runner.LIVE_METRICS_COPY_NAME
            self.assertTrue(result["ok"], result)
            self.assertEqual(result["artifact_path"], str(copied))
            self.assertTrue(copied.exists())
            self.assertTrue((output_root / widget_soak_runner.LIVE_METRICS_SUMMARY_NAME).exists())

    def test_missing_live_metrics_source_is_explicit_failure(self) -> None:
        result = widget_soak_runner.resolve_live_metrics(
            args=Namespace(allow_missing_live_metrics=False, live_metrics_artifact=""),
            output_root=Path("unused"),
            dry_run=False,
        )
        self.assertTrue(result["required"])
        self.assertFalse(result["ok"])
        self.assertEqual(result["collection"], "missing_source")

    def test_dry_run_reports_local_artifact_without_loading(self) -> None:
        with mock.patch.object(widget_soak_runner, "load_live_metrics_artifact") as load:
            result = widget_soak_runner.resolve_live_metrics(
                args=Namespace(allow_missing_live_metrics=False, live_metrics_artifact="local.json"),
                output_root=Path("unused"),
                dry_run=True,
            )
        load.assert_not_called()
        self.assertFalse(result["ok"])
        self.assertEqual(result["expected_artifact"], "local.json")
        self.assertEqual(result["collection"], "dry_run")

    def test_soak_summary_reports_unavailable_host_metrics(self) -> None:
        summary = widget_soak_runner.summarize(
            args=Namespace(target_id="test", targets_file="test.toml", widget_name="progress",
                           instance_id="progress", duration_s=1, rate_rps=1),
            root=Path.cwd(), started_at="start", ended_at="end", output_root=Path.cwd(),
            commands={}, agent_results={}, resource_samples=[], live_metrics={}, dry_run=True,
        )
        self.assertEqual(summary["resource_samples"], [])
        self.assertIsNone(summary["resource_drift"])
        self.assertEqual(summary["resource_metrics"]["status"], "unavailable")
        self.assertTrue(summary["resource_metrics"]["reason"])


if __name__ == "__main__":
    unittest.main()
