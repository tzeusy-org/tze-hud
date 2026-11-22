#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("check_constrained_envelope.py")
SPEC = importlib.util.spec_from_file_location("check_constrained_envelope", MODULE_PATH)
assert SPEC is not None
check_constrained_envelope = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = check_constrained_envelope
SPEC.loader.exec_module(check_constrained_envelope)


class ConstrainedEnvelopeCheckerTests(unittest.TestCase):
    @staticmethod
    def _valid_profile() -> dict:
        return {
            "schema": "tze_hud.constrained_profile.v1",
            "lane": "llvmpipe-two-logical-cpus",
            "low_power_proxy": True,
            "device_qualification": False,
            "operating_system": {
                "family": "linux",
                "name": "Ubuntu",
                "version": "24.04",
                "architecture": "x86_64",
            },
            "cpu": {
                "model": "CI small-core proxy",
                "logical_cpu_limit": 2,
                "allowed_cpu_list": "0-1",
                "enforcement_mechanism": "linux sched affinity (taskset)",
                "enforced": True,
            },
            "memory": {
                "limit_bytes": None,
                "enforcement_mechanism": "none",
            },
            "renderer": {
                "requested_software": True,
                "backend": "Vulkan",
                "adapter_identity": "llvmpipe (LLVM 19.1.7, 256 bits)",
                "device_type": "Cpu",
                "driver": "llvmpipe",
                "driver_info": "Mesa 24.2",
                "vendor_id": 0x10005,
                "device_id": 0,
                "verified_software": True,
            },
            "viewport": {"width": 1920, "height": 1080},
            "calibration_vector_version": "tze_hud.cpu-gpu-upload.v1",
        }

    @classmethod
    def _valid_artifact(cls) -> dict:
        frame_bucket = {"samples": [1] * 180}
        latency_bucket = {"samples": [1]}
        base_summary = {
            "total_frames": 180,
            "frame_time": frame_bucket,
            "input_to_local_ack": latency_bucket,
            "input_to_scene_commit": latency_bucket,
            "input_to_next_present": latency_bucket,
            "lease_violations": 0,
            "budget_overruns": 0,
            "sync_drift_violations": 0,
            "invariant_violations": 0,
            "scene_lock_misses": 0,
        }
        paced_summary = dict(base_summary)
        paced_summary["scene_lock_misses"] = 18
        return {
            "constrained_profile": cls._valid_profile(),
            "calibration": {
                "factors": {"cpu": 0.854, "gpu": 0.338, "upload": 0.215}
            },
            "sessions": [
                {"name": name, "summary": dict(base_summary)}
                for name in ("notification_publish", "widget_param_update", "portal_append")
            ]
            + [{"name": "scene_lock_paced_contention", "summary": paced_summary}],
        }

    def test_valid_proxy_reuses_reference_normalized_ceilings(self) -> None:
        report = check_constrained_envelope.build_report(
            self._valid_artifact(), "benchmark.json"
        )

        self.assertEqual("pass", report["verdict"])
        self.assertEqual([], report["failures"])
        self.assertTrue(report["profile"]["low_power_proxy"])
        self.assertFalse(report["profile"]["device_qualification"])
        timing_results = [
            result for result in report["normalized_results"] if "observed_us" in result
        ]
        self.assertTrue(timing_results)
        for result in timing_results:
            self.assertEqual(
                result["reference_budget_us"], result["normalized_ceiling_us"]
            )
            self.assertIn("normalized_observed_us", result)

    def test_split_latency_and_frame_metrics_exactly_match_windows_gate(self) -> None:
        report = check_constrained_envelope.build_report(
            self._valid_artifact(), "benchmark.json"
        )
        observed = {
            (result["session"], result["metric"])
            for result in report["normalized_results"]
            if "observed_us" in result
        }
        expected = {
            (session, budget.metric)
            for session, budgets in (
                check_constrained_envelope.windows_budgets.SESSION_BUDGETS.items()
            )
            for budget in budgets
        }
        self.assertEqual(expected, observed)

    def test_normalized_value_is_explicit_and_uses_vector_dimension(self) -> None:
        artifact = self._valid_artifact()
        artifact["calibration"]["factors"]["gpu"] = 0.169
        report = check_constrained_envelope.build_report(artifact, "benchmark.json")
        frame_result = next(
            result
            for result in report["normalized_results"]
            if result["session"] == "notification_publish"
            and result["metric"] == "frame_time_p99"
        )

        self.assertEqual(1, frame_result["observed_us"])
        self.assertEqual(2.0, frame_result["normalized_observed_us"])
        self.assertEqual(8_300, frame_result["normalized_ceiling_us"])

    def test_missing_profile_fails_closed(self) -> None:
        artifact = self._valid_artifact()
        del artifact["constrained_profile"]

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertIn("constrained_profile must be an object", report["failures"][0])

    def test_cpu_limit_must_be_exactly_two_and_enforced(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["cpu"]["logical_cpu_limit"] = 3
        artifact["constrained_profile"]["cpu"]["enforced"] = False

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("exactly 2" in failure for failure in report["failures"]),
            report["failures"],
        )
        self.assertTrue(
            any("not enforced" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_effective_cpu_list_must_prove_exactly_two_cpus(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["cpu"]["allowed_cpu_list"] = "0-3"

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("allowed CPU list" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_linux_profile_requires_matching_lane_and_taskset_identity(self) -> None:
        artifact = self._valid_artifact()
        profile = artifact["constrained_profile"]
        profile["lane"] = "warp-two-logical-cpus"
        profile["cpu"]["enforcement_mechanism"] = "environment variable"

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("lane" in failure for failure in report["failures"]),
            report["failures"],
        )
        self.assertTrue(
            any("taskset" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_memory_limit_and_enforcement_identity_must_be_coherent(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["memory"] = {
            "limit_bytes": 1_073_741_824,
            "enforcement_mechanism": "none",
        }

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("memory" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_memory_limit_requires_the_observed_cgroup_v2_mechanism(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["memory"] = {
            "limit_bytes": 1_073_741_824,
            "enforcement_mechanism": "unverified soft limit",
        }

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("cgroup v2 memory.max" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_viewport_must_match_the_canonical_benchmark_corpus(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["viewport"] = {
            "width": 1280,
            "height": 720,
        }

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("1920x1080" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_renderer_fallback_is_rejected(self) -> None:
        artifact = self._valid_artifact()
        renderer = artifact["constrained_profile"]["renderer"]
        renderer["requested_software"] = False
        renderer["verified_software"] = False
        renderer["adapter_identity"] = "NVIDIA RTX 3080"

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("software renderer" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_renderer_driver_identity_is_required(self) -> None:
        artifact = self._valid_artifact()
        renderer = artifact["constrained_profile"]["renderer"]
        del renderer["driver"]
        renderer["driver_info"] = ""

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("renderer.driver" in failure for failure in report["failures"]),
            report["failures"],
        )
        self.assertTrue(
            any("renderer.driver_info" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_renderer_numeric_identity_fields_are_required(self) -> None:
        artifact = self._valid_artifact()
        renderer = artifact["constrained_profile"]["renderer"]
        del renderer["vendor_id"]
        renderer["device_id"] = "unknown"

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("renderer.vendor_id" in failure for failure in report["failures"]),
            report["failures"],
        )
        self.assertTrue(
            any("renderer.device_id" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_truncated_benchmark_corpus_fails_closed(self) -> None:
        artifact = self._valid_artifact()
        for session in artifact["sessions"]:
            session["summary"]["total_frames"] = 1

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("180 frames" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_incomplete_frame_sample_corpus_fails_closed(self) -> None:
        artifact = self._valid_artifact()
        artifact["sessions"][0]["summary"]["frame_time"]["samples"] = [1]

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("frame-time samples" in failure for failure in report["failures"]),
            report["failures"],
        )

    def test_missing_identity_and_invalid_calibration_fail_closed(self) -> None:
        artifact = self._valid_artifact()
        artifact["constrained_profile"]["cpu"]["model"] = ""
        artifact["calibration"]["factors"]["upload"] = None

        report = check_constrained_envelope.build_report(artifact, "benchmark.json")

        self.assertEqual("fail", report["verdict"])
        self.assertTrue(
            any("cpu.model" in failure for failure in report["failures"]),
            report["failures"],
        )
        self.assertTrue(
            any("calibration" in failure for failure in report["failures"]),
            report["failures"],
        )


if __name__ == "__main__":
    unittest.main()
