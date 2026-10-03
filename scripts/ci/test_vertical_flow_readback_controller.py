#!/usr/bin/env python3
"""Offline contract tests for the reference-Windows VerticalFlow proof runbook."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
CONTROLLER = ROOT / "scripts/windows/run_vertical_flow_readback_proof.ps1"
RUNBOOK = ROOT / "docs/operations/vertical-flow-reference-windows-proof.md"


class VerticalFlowReadbackControllerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.controller = (
            CONTROLLER.read_text(encoding="utf-8") if CONTROLLER.is_file() else ""
        )
        cls.runbook = RUNBOOK.read_text(encoding="utf-8") if RUNBOOK.is_file() else ""

    def test_controller_requires_authority_and_rejects_foreign_live_lock(self):
        self.assertIn("[switch]$AllowProductionStop", self.controller)
        self.assertIn("if (-not $AllowProductionStop)", self.controller)
        self.assertIn("live non-production PID", self.controller)
        self.assertIn("Get-Process -Id", self.controller)
        self.assertIn("removing stale GPU lock owned by dead PID", self.controller)
        self.assertNotIn("refusing to remove stale lock not owned", self.controller)

    def test_controller_acquires_lock_atomically_and_preserves_evidence_reporting(self):
        self.assertIn("[System.IO.FileMode]::CreateNew", self.controller)
        self.assertNotIn(
            "Move-Item -LiteralPath $LockTmp -Destination $LockFile -Force",
            self.controller,
        )
        self.assertNotIn("Write-Error $controllerError", self.controller)
        self.assertNotIn('Write-Error "restoration failed:', self.controller)

    def test_controller_restores_only_prior_runtime_and_verifies_ports(self):
        self.assertIn("Assert-ProductionTaskAction", self.controller)
        self.assertIn("scheduled task action", self.controller)
        self.assertIn("Get-ListeningPortsOwnedBy", self.controller)
        self.assertIn("before takeover", self.controller)
        self.assertIn("finally {", self.controller)
        self.assertIn("if ($productionWasRunning)", self.controller)
        self.assertIn(
            "Start-ScheduledTask -TaskName $ProductionTaskName", self.controller
        )
        self.assertIn("$currentProduction = @(Get-ProductionProcess)", self.controller)
        self.assertIn("if ($currentProduction.Count -eq 0)", self.controller)
        self.assertIn("@(50051, 9090)", self.controller)
        self.assertIn("Get-NetTCPConnection", self.controller)

    def test_runbook_keeps_offline_and_live_phases_explicit(self):
        self.assertIn("Do not run the live phase while another GPU lane owns", self.runbook)
        self.assertIn(
            "cargo build --release --target x86_64-pc-windows-gnu", self.runbook
        )
        self.assertIn("run_vertical_flow_readback_proof.ps1", self.runbook)
        self.assertIn("vertical-flow-readback.json", self.runbook)
        self.assertIn("vertical-flow-readback.ppm", self.runbook)
        self.assertIn(
            'tee "$LOCAL_OUTPUT/vertical-flow-controller.log"', self.runbook
        )
        self.assertIn("PROOF_EXIT=${PIPESTATUS[0]}", self.runbook)
        self.assertNotIn("--psk", self.runbook)


if __name__ == "__main__":
    unittest.main()
