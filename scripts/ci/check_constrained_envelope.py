#!/usr/bin/env python3
"""Validate the low-power proxy benchmark without weakening reference budgets."""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
from pathlib import Path
from typing import Any


WINDOWS_CHECKER_PATH = Path(__file__).with_name("check_windows_perf_budgets.py")
WINDOWS_SPEC = importlib.util.spec_from_file_location(
    "check_windows_perf_budgets_for_constrained_lane", WINDOWS_CHECKER_PATH
)
assert WINDOWS_SPEC is not None
windows_budgets = importlib.util.module_from_spec(WINDOWS_SPEC)
assert WINDOWS_SPEC.loader is not None
sys.modules[WINDOWS_SPEC.name] = windows_budgets
WINDOWS_SPEC.loader.exec_module(windows_budgets)

CALIBRATION_VECTOR_VERSION = "tze_hud.cpu-gpu-upload.v2"
CANONICAL_VIEWPORT = (1920, 1080)
EXPECTED_BENCHMARK_FRAMES = 180
EXPECTED_SCENARIOS = windows_budgets.REQUIRED_COUNTER_SESSIONS


def _is_text(value: Any) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _cpu_list_count(cpu_list: str) -> int | None:
    cpus: set[int] = set()
    try:
        for raw_segment in cpu_list.split(","):
            segment = raw_segment.strip()
            if not segment:
                return None
            bounds = segment.split("-")
            if len(bounds) == 1:
                first = last = int(bounds[0])
            elif len(bounds) == 2:
                first, last = (int(value) for value in bounds)
            else:
                return None
            if first < 0 or last < first:
                return None
            cpus.update(range(first, last + 1))
    except ValueError:
        return None
    return len(cpus) if cpus else None


def _profile_failures(artifact: dict[str, Any]) -> tuple[dict[str, Any], list[str]]:
    profile = artifact.get("constrained_profile")
    if not isinstance(profile, dict):
        return {}, ["constrained_profile must be an object"]

    failures: list[str] = []
    if profile.get("schema") != "tze_hud.constrained_profile.v1":
        failures.append("constrained_profile.schema must be tze_hud.constrained_profile.v1")
    if not _is_text(profile.get("lane")):
        failures.append("constrained_profile.lane is required")
    if profile.get("low_power_proxy") is not True:
        failures.append("constrained profile must identify itself as a low-power proxy")
    if profile.get("device_qualification") is not False:
        failures.append("constrained profile must not claim device qualification")

    operating_system = profile.get("operating_system")
    if not isinstance(operating_system, dict):
        failures.append("constrained_profile.operating_system must be an object")
        operating_system = {}
    for field in ("family", "name", "version", "architecture"):
        if not _is_text(operating_system.get(field)):
            failures.append(f"constrained_profile.operating_system.{field} is required")

    cpu = profile.get("cpu")
    if not isinstance(cpu, dict):
        failures.append("constrained_profile.cpu must be an object")
        cpu = {}
    if not _is_text(cpu.get("model")):
        failures.append("constrained_profile.cpu.model is required")
    if cpu.get("logical_cpu_limit") != 2:
        failures.append("constrained profile must enforce exactly 2 logical CPUs")
    if cpu.get("enforced") is not True:
        failures.append("constrained profile logical-CPU limit is not enforced")
    if not _is_text(cpu.get("allowed_cpu_list")):
        failures.append("constrained_profile.cpu.allowed_cpu_list is required")
    elif _cpu_list_count(cpu["allowed_cpu_list"]) != 2:
        failures.append("constrained profile allowed CPU list must prove exactly 2 CPUs")
    if not _is_text(cpu.get("enforcement_mechanism")):
        failures.append("constrained_profile.cpu.enforcement_mechanism is required")

    memory = profile.get("memory")
    if not isinstance(memory, dict):
        failures.append("constrained_profile.memory must be an object")
        memory = {}
    memory_limit = memory.get("limit_bytes")
    if memory_limit is not None and (
        not isinstance(memory_limit, int)
        or isinstance(memory_limit, bool)
        or memory_limit <= 0
    ):
        failures.append("constrained_profile.memory.limit_bytes must be null or positive")
    if not _is_text(memory.get("enforcement_mechanism")):
        failures.append("constrained_profile.memory.enforcement_mechanism is required")
    elif (memory_limit is None) != (memory["enforcement_mechanism"] == "none"):
        failures.append(
            "constrained_profile.memory limit and enforcement mechanism disagree"
        )
    elif (
        memory_limit is not None
        and memory["enforcement_mechanism"] != "cgroup v2 memory.max"
    ):
        failures.append(
            "constrained profile memory limit must identify cgroup v2 memory.max"
        )

    renderer = profile.get("renderer")
    if not isinstance(renderer, dict):
        failures.append("constrained_profile.renderer must be an object")
        renderer = {}
    if renderer.get("requested_software") is not True:
        failures.append("constrained lane did not request a software renderer")
    if renderer.get("verified_software") is not True:
        failures.append("constrained lane did not verify the selected software renderer")
    for field in ("backend", "adapter_identity", "device_type", "driver", "driver_info"):
        if not _is_text(renderer.get(field)):
            failures.append(f"constrained_profile.renderer.{field} is required")
    for field in ("vendor_id", "device_id"):
        value = renderer.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            failures.append(
                f"constrained_profile.renderer.{field} must be a non-negative integer"
            )

    family = str(operating_system.get("family", "")).lower()
    lane = str(profile.get("lane", ""))
    enforcement = str(cpu.get("enforcement_mechanism", ""))
    expected_lane = {
        "linux": "llvmpipe-two-logical-cpus",
        "windows": "warp-two-logical-cpus",
    }.get(family)
    if expected_lane is None or lane != expected_lane:
        failures.append(
            "constrained_profile.lane must match the operating-system proxy"
        )
    if family == "linux" and enforcement != "linux sched affinity (taskset)":
        failures.append("Linux constrained profile must identify taskset enforcement")

    backend = str(renderer.get("backend", "")).lower()
    adapter = str(renderer.get("adapter_identity", "")).lower()
    device_type = str(renderer.get("device_type", "")).lower()
    linux_software = (
        family == "linux"
        and backend == "vulkan"
        and ("llvmpipe" in adapter or "softpipe" in adapter)
        and device_type == "cpu"
    )
    windows_software = (
        family == "windows"
        and backend in {"dx12", "directx12"}
        and "warp" in adapter
        and device_type == "cpu"
    )
    if not (linux_software or windows_software):
        failures.append(
            "selected adapter is not an approved llvmpipe/WARP software renderer"
        )

    viewport = profile.get("viewport")
    if not isinstance(viewport, dict):
        failures.append("constrained_profile.viewport must be an object")
        viewport = {}
    for field in ("width", "height"):
        value = viewport.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            failures.append(f"constrained_profile.viewport.{field} must be positive")
    if (viewport.get("width"), viewport.get("height")) != CANONICAL_VIEWPORT:
        failures.append("constrained profile viewport must be the canonical 1920x1080")

    if profile.get("calibration_vector_version") != CALIBRATION_VECTOR_VERSION:
        failures.append(
            "constrained_profile.calibration_vector_version must match "
            f"{CALIBRATION_VECTOR_VERSION}"
        )
    return profile, failures


def _corpus_failures(artifact: dict[str, Any]) -> list[str]:
    sessions = artifact.get("sessions")
    if not isinstance(sessions, list):
        return ["benchmark artifact sessions must be a list"]

    by_name = {
        session.get("name"): session
        for session in sessions
        if isinstance(session, dict) and isinstance(session.get("name"), str)
    }
    failures: list[str] = []
    for name in EXPECTED_SCENARIOS:
        session = by_name.get(name)
        if not isinstance(session, dict):
            failures.append(f"constrained benchmark corpus is missing session {name}")
            continue
        summary = session.get("summary")
        total_frames = summary.get("total_frames") if isinstance(summary, dict) else None
        if total_frames != EXPECTED_BENCHMARK_FRAMES or isinstance(total_frames, bool):
            failures.append(
                f"{name}: constrained benchmark corpus must run exactly "
                f"{EXPECTED_BENCHMARK_FRAMES} frames, observed {total_frames!r}"
            )
        frame_time = summary.get("frame_time") if isinstance(summary, dict) else None
        frame_samples = (
            frame_time.get("samples") if isinstance(frame_time, dict) else None
        )
        if not isinstance(frame_samples, list) or len(frame_samples) != EXPECTED_BENCHMARK_FRAMES:
            observed_count = len(frame_samples) if isinstance(frame_samples, list) else None
            failures.append(
                f"{name}: constrained benchmark corpus must contain exactly "
                f"{EXPECTED_BENCHMARK_FRAMES} frame-time samples, observed "
                f"{observed_count!r}"
            )
    return failures


def _normalized_results(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    normalized: list[dict[str, Any]] = []
    for result in results:
        item = dict(result)
        if "observed_us" in result:
            factor = float(result["hardware_factor"])
            reference_factor = float(result["reference_hardware_factor"])
            item["normalized_observed_us"] = round(
                float(result["observed_us"]) * reference_factor / factor, 3
            )
            item["normalized_ceiling_us"] = result["reference_budget_us"]
        normalized.append(item)
    return normalized


def build_report(artifact: dict[str, Any], source_artifact: str) -> dict[str, Any]:
    profile, failures = _profile_failures(artifact)
    failures.extend(_corpus_failures(artifact))
    results: list[dict[str, Any]] = []
    factors: dict[str, float] | None = None
    try:
        factors = windows_budgets.hardware_factors(artifact)
        budget_results, budget_failures = windows_budgets.validate_benchmark(artifact)
        results = _normalized_results(budget_results)
        failures.extend(budget_failures)
    except SystemExit as exc:
        failures.append(f"invalid calibration or benchmark artifact: {exc}")

    return {
        "schema": "tze_hud.constrained_envelope_budget_gate.v1",
        "reference_hardware": windows_budgets.REFERENCE_HARDWARE_TAG,
        "source_artifact": source_artifact,
        "profile": profile,
        "calibration_vector_version": CALIBRATION_VECTOR_VERSION,
        "raw_factors": factors,
        "normalized_results": results,
        "normalized_ceilings_source": "scripts/ci/check_windows_perf_budgets.py",
        "failures": failures,
        "verdict": "fail" if failures else "pass",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--benchmark-json", type=Path, required=True)
    parser.add_argument("--output-json", type=Path, required=True)
    args = parser.parse_args()

    artifact = windows_budgets.load_json(args.benchmark_json)
    report = build_report(artifact, str(args.benchmark_json))
    args.output_json.parent.mkdir(parents=True, exist_ok=True)
    args.output_json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for result in report["normalized_results"]:
        status = "PASS" if result["pass"] else "FAIL"
        if "normalized_observed_us" in result:
            print(
                f"{status} {result['session']}.{result['metric']}: "
                f"normalized {result['normalized_observed_us']}us <= "
                f"{result['normalized_ceiling_us']}us"
            )
        else:
            print(
                f"{status} {result['session']}.{result['metric']}: "
                f"observed={result.get('observed')} budget={result.get('budget')}"
            )

    if report["failures"]:
        print("\nConstrained-envelope failures:", file=sys.stderr)
        for failure in report["failures"]:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
