#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("check_idle_efficiency.py")
FIXTURE_DIR = Path(__file__).with_name("fixtures") / "idle-efficiency"
SPEC = importlib.util.spec_from_file_location("check_idle_efficiency", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
check_idle_efficiency = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = check_idle_efficiency
SPEC.loader.exec_module(check_idle_efficiency)


def valid_artifact() -> dict:
    return {
        "schema_version": 1,
        "scenario": {"name": "quiescent_static_scene", "version": 1},
        "runtime": {"build": "test-build", "window_mode": "headless"},
        "pacing": {"mode": "event_driven", "requested_cadence_hz": None},
        "renderer": {
            "backend": "Vulkan",
            "adapter": "llvmpipe (LLVM 18.1.8, 256 bits)",
            "software": True,
        },
        "viewport": {"width": 640, "height": 360},
        "constrained_profile": {
            "operating_system": "linux",
            "cpu_model": "test-cpu",
            "logical_cpu_limit": 2,
            "cpu_limit_enforcement": "sched_getaffinity:0,1",
            "memory_limit_bytes": None,
        },
        "settling_duration_ms": 5_000,
        "interval_duration_ms": 60_000,
        "status": "complete",
        "wakeups": {
            "combined_runtime_driven": 0,
            "main_loop": 0,
            "compositor_loop": 0,
            "sources": {},
            "excluded_sampler": 60,
            "excluded_operating_system": 0,
        },
        "gpu": {
            "queue_submissions": 0,
            "surface_acquisitions": 0,
            "presents": 0,
        },
    }


class IdleEfficiencyCheckerTests(unittest.TestCase):
    def test_checked_in_versioned_fixtures_pin_pass_and_fail_shapes(self) -> None:
        valid = check_idle_efficiency.load_artifact(FIXTURE_DIR / "valid-linux-v1.json")
        invalid = check_idle_efficiency.load_artifact(
            FIXTURE_DIR / "invalid-missing-presents-v1.json"
        )

        _report, valid_failures = check_idle_efficiency.validate_artifact(
            valid, require_constrained=True
        )
        _report, invalid_failures = check_idle_efficiency.validate_artifact(
            invalid, require_constrained=True
        )
        self.assertEqual([], valid_failures)
        self.assertTrue(
            any("gpu.presents" in failure for failure in invalid_failures),
            invalid_failures,
        )

    def test_valid_llvmpipe_artifact_passes(self) -> None:
        report, failures = check_idle_efficiency.validate_artifact(
            valid_artifact(), require_constrained=True
        )
        self.assertEqual([], failures)
        self.assertEqual("pass", report["status"])
        self.assertEqual(120, report["budgets"]["runtime_wakeups"]["ceiling"])

    def test_missing_required_counter_fails_closed(self) -> None:
        artifact = valid_artifact()
        del artifact["gpu"]["presents"]
        _report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertTrue(any("gpu.presents" in failure for failure in failures))

    def test_fixed_cadence_is_ineligible(self) -> None:
        artifact = valid_artifact()
        artifact["pacing"] = {
            "mode": "fixed_cadence",
            "requested_cadence_hz": 60,
        }
        _report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertTrue(any("event_driven" in failure for failure in failures))

    def test_linux_constrained_lane_rejects_unproved_software_adapter(self) -> None:
        artifact = valid_artifact()
        artifact["renderer"]["adapter"] = "NVIDIA RTX 3080"
        _report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertTrue(any("llvmpipe" in failure for failure in failures))

    def test_linux_constrained_lane_rejects_unproved_cpu_enforcement(self) -> None:
        artifact = valid_artifact()
        artifact["constrained_profile"]["cpu_limit_enforcement"] = "requested two cpus"
        _report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertTrue(any("sched_getaffinity" in failure for failure in failures))

    def test_windows_basic_render_driver_alias_proves_warp(self) -> None:
        artifact = valid_artifact()
        artifact["renderer"] = {
            "backend": "Dx12",
            "adapter": "Microsoft Basic Render Driver",
            "software": True,
        }
        artifact["constrained_profile"] = {
            "operating_system": "windows",
            "cpu_model": "test-cpu",
            "logical_cpu_limit": 2,
            "cpu_limit_enforcement": "GetProcessAffinityMask:0x3",
            "memory_limit_bytes": None,
        }

        report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )

        self.assertEqual([], failures)
        self.assertEqual("pass", report["status"])
        artifact["constrained_profile"]["logical_cpu_limit"] = 4
        artifact["constrained_profile"]["cpu_limit_enforcement"] = "GetProcessAffinityMask:0xf"
        report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertEqual("fail", report["status"])
        self.assertTrue(any("expected 2, got 4" in failure for failure in failures))

    def test_required_overlay_rejects_a_fullscreen_fallback(self) -> None:
        artifact = valid_artifact()
        artifact["runtime"]["window_mode"] = "fullscreen"
        _report, failures = check_idle_efficiency.validate_artifact(
            artifact,
            require_constrained=True,
            required_window_mode="overlay",
        )
        self.assertTrue(any("runtime.window_mode" in failure for failure in failures))

    def test_gpu_work_and_wakeup_overage_both_fail(self) -> None:
        artifact = valid_artifact()
        artifact["gpu"]["queue_submissions"] = 1
        artifact["wakeups"] = {
            "combined_runtime_driven": 121,
            "main_loop": 1,
            "compositor_loop": 120,
            "sources": {"main.event": 1, "compositor.timer": 120},
            "excluded_sampler": 0,
            "excluded_operating_system": 0,
        }
        report, failures = check_idle_efficiency.validate_artifact(
            artifact, require_constrained=True
        )
        self.assertEqual("fail", report["status"])
        self.assertTrue(any("queue submissions" in failure for failure in failures))
        self.assertTrue(any("wakeups" in failure for failure in failures))


if __name__ == "__main__":
    unittest.main()
