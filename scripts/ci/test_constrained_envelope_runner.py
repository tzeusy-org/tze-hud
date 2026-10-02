#!/usr/bin/env python3
from __future__ import annotations

import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).parents[2]
RUNNER = (REPO_ROOT / "scripts/ci/run_constrained_envelope.sh").read_text(
    encoding="utf-8"
)
WORKFLOW = (REPO_ROOT / ".github/workflows/perf-budget.yml").read_text(encoding="utf-8")


class ConstrainedEnvelopeRunnerTests(unittest.TestCase):
    def test_ci_uses_ubuntu_mesa_and_the_canonical_runner(self) -> None:
        job = WORKFLOW.split("  constrained-envelope-budget:", 1)[1]
        self.assertIn("runs-on: ubuntu-latest", job)
        self.assertIn("mesa-vulkan-drivers", job)
        self.assertIn("scripts/ci/run_constrained_envelope.sh", job)

    def test_runner_forces_llvmpipe_proxy_with_exact_taskset_pair(self) -> None:
        self.assertIn("cargo build --release -p benchmark --features headless", RUNNER)
        self.assertIn("HEADLESS_FORCE_SOFTWARE=1 taskset --cpu-list", RUNNER)
        self.assertIn('"$cpu_pair"', RUNNER)
        self.assertIn("--constrained-envelope", RUNNER)

    def test_runner_preserves_benchmark_failure_after_emitting_gate_artifact(self) -> None:
        self.assertIn("checker_status=0", RUNNER)
        self.assertIn("if (( benchmark_status != 0 )); then", RUNNER)
        self.assertIn('exit "$benchmark_status"', RUNNER)
        self.assertIn('exit "$checker_status"', RUNNER)


if __name__ == "__main__":
    unittest.main()
