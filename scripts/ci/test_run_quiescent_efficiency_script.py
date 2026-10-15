#!/usr/bin/env python3
"""Offline contract tests for the startup-atomic Windows quiescent harness."""

from __future__ import annotations

import unittest
from pathlib import Path


ROOT = Path(__file__).parents[2]
SCRIPT_PATH = ROOT / "scripts" / "ci" / "windows" / "run-quiescent-efficiency.ps1"
CI_WORKFLOW_PATH = ROOT / ".github" / "workflows" / "ci.yml"
JUSTFILE_PATH = ROOT / "justfile"


class QuiescentEfficiencyLaunchContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.script = SCRIPT_PATH.read_text(encoding="utf-8-sig")
        cls.workflow = CI_WORKFLOW_PATH.read_text(encoding="utf-8")
        cls.justfile = JUSTFILE_PATH.read_text(encoding="utf-8")

    def test_applies_affinity_while_creating_the_measured_application(self) -> None:
        self.assertIn('$affinityMask = "3"', self.script)
        self.assertIn('start "" /b /wait /affinity {0} {1} {2}', self.script)
        self.assertIn("$affinityMask, $quotedExePath", self.script)
        self.assertIn("-FilePath $env:ComSpec", self.script)
        self.assertNotIn("-FilePath $ExePath", self.script)
        self.assertNotIn(".ProcessorAffinity", self.script)

        runtime = (ROOT / "crates/tze_hud_runtime/src/windowed/mod.rs").read_text(
            encoding="utf-8"
        )
        run = runtime.split("pub fn run(self)", 1)[1]
        admission = (
            '#[cfg(target_os = "windows")]\n'
            "        if let Some(measurement) = cfg.quiescent_efficiency.as_ref() {\n"
            "            lifecycle::admit_quiescent_cpu_affinity(&measurement.emit_path)?;\n"
            "        }"
        )
        self.assertIn(admission, run)
        self.assertLess(run.index(admission), run.index("build_runtime_context(&cfg)"))
        self.assertLess(run.index(admission), run.index("EventLoop::<RuntimeWakeEvent>"))

        lifecycle = (ROOT / "crates/tze_hud_runtime/src/windowed/lifecycle.rs").read_text(
            encoding="utf-8"
        )
        helper = lifecycle.split("pub(super) fn admit_quiescent_cpu_affinity(", 1)[1]
        helper = helper.split('\n#[cfg(target_os = "windows")]\nfn observed_cpu_affinity', 1)[0]
        self.assertRegex(helper, r"std::fs::write\([^;]+\)\?;")
        self.assertLess(helper.index("if observed != selected"), helper.index("std::fs::write("))
        self.assertLess(helper.index("std::fs::write("), helper.index("Ok(())"))

        completion = (
            'Write-Host ("quiescent-completion launcher_pid={0} propagated_hud_exit={1} '
            'synchronous_wait_complete=true" -f $launcher.Id, $runtimeExit)'
        )
        self.assertIn(completion, self.script)
        self.assertLess(self.script.index("$runtimeExit = $launcher.ExitCode"), self.script.index(completion))
        self.assertLess(self.script.index(completion), self.script.index("if (-not (Test-Path $artifactPath))"))

        prelaunch = self.script.split("$launcher = Start-Process", 1)[0]
        self.assertIn('$ErrorActionPreference = "Stop"', prelaunch)
        self.assertIn("foreach ($ownedPath in @($admissionPath, $artifactPath))", prelaunch)
        self.assertIn("Remove-Item -LiteralPath $ownedPath -Force", prelaunch)
        self.assertNotIn("-ErrorAction SilentlyContinue", prelaunch)
        self.assertNotIn("-Recurse", prelaunch)

        validation = self.script.split("if ($checkerExit -ne 0)", 1)[1]
        self.assertIn("foreach ($currentPath in @($admissionPath, $artifactPath))", validation)
        self.assertIn("$currentInfo.LastWriteTimeUtc -lt $launchStartedUtc", validation)
        self.assertIn("$admissionInfo.Length -le 0 -or $admissionInfo.Length -gt 256", validation)
        receipt_read = "[System.IO.File]::ReadAllText($admissionInfo.FullName)"
        self.assertIn(receipt_read, validation)
        self.assertLess(validation.index("$admissionInfo.Length -gt 256"), validation.index(receipt_read))
        self.assertIn(
            r"$admissionPattern = '\Aquiescent-admission pid=(?<pid>[1-9][0-9]{0,9}) "
            r"cpu_mask=0x(?<mask>[0-9a-f]{1,16}) logical_cpus=2 "
            r"phase=before_runtime_init(?:\r?\n)?\z'",
            validation,
        )
        self.assertRegex(validation, r"if \(-not \$admissionMatch.Success\) \{\s+throw ")
        receipt_print = 'Write-Host $admissionText.TrimEnd("`r", "`n")'
        self.assertIn(receipt_print, validation)
        self.assertLess(validation.index("if (-not $admissionMatch.Success)"), validation.index(receipt_print))
        self.assertNotIn("Write-Host $admissionText", validation[:validation.index(receipt_print)])

    def test_fast_ci_runs_the_startup_atomicity_contract(self) -> None:
        self.assertIn(
            "python3 scripts/ci/test_run_quiescent_efficiency_script.py",
            self.workflow,
        )
        self.assertIn(
            "python3 scripts/ci/test_run_quiescent_efficiency_script.py",
            self.justfile,
        )

    def test_keeps_the_runtime_artifact_as_the_final_affinity_observer(self) -> None:
        self.assertIn("--quiescent-efficiency-emit", self.script)
        self.assertIn("--require-constrained", self.script)
        self.assertIn("--require-window-mode overlay", self.script)


if __name__ == "__main__":
    unittest.main()
