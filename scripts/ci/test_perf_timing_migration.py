"""Offline checks for exact perf baseline provenance and migrated timing verdicts."""

from contextlib import redirect_stdout
from io import StringIO
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
import textwrap
import unittest
from unittest.mock import patch

import select_windows_perf_baseline as selector

REPO = "owner/repo"
REPO_ID = 17
SHA = "a" * 40
WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/perf-budget.yml"


def run(run_id, **overrides):
    return {
        "id": run_id,
        "created_at": "2026-11-23T11:24:09Z",
        "event": "schedule",
        "status": "completed",
        "conclusion": "failure",
        "head_branch": "main",
        "head_sha": SHA,
        "repository": {"id": REPO_ID},
        "head_repository": {"id": REPO_ID},
        "path": ".github/workflows/perf-budget.yml",
        "run_attempt": 1,
        **overrides,
    }


def jobs(run_id, attempt=1, *, legacy=False):
    result = [
        {
            "name": name,
            "run_id": run_id,
            "run_attempt": attempt,
            "head_sha": SHA,
            "status": "completed",
            "conclusion": "success",
        }
        for name in (selector.WINDOWS_JOB, selector.CONSTRAINED_JOB, selector.TIMING_JOB)
    ]
    result[0]["steps"] = [
        {
            "name": selector.PUBLISH_STEP,
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-11-23T11:34:42Z",
            "completed_at": "2026-11-23T11:34:43Z",
        }
    ]
    if legacy:
        return result[:2]
    result[-1]["conclusion"] = "failure"
    return result


def artifact(run_id, attempt=1, *, legacy=False, **overrides):
    return {
        "id": run_id * 100 + attempt,
        "name": selector.BASELINE if legacy else f"{selector.BASELINE}-attempt-{attempt}",
        "expired": False,
        "digest": "sha256:" + "c" * 64,
        "created_at": "2026-11-23T11:34:43Z",
        "workflow_run": {
            "id": run_id,
            "repository_id": REPO_ID,
            "head_repository_id": REPO_ID,
            "head_branch": "main",
            "head_sha": SHA,
        },
        **overrides,
    }


class BaselineSelectionTests(unittest.TestCase):
    def select(self, runs, attempts, artifacts):
        def get_rows(route, key):
            if key == "workflow_runs":
                self.assertIn("/workflows/perf-budget.yml/runs?branch=main&status=completed", route)
                return runs
            if key == "jobs":
                match = re.search(r"/runs/(\d+)/attempts/(\d+)/jobs\?", route)
                self.assertIsNotNone(match)
                return attempts[tuple(map(int, match.groups()))]
            self.assertEqual(key, "artifacts")
            match = re.search(r"/runs/(\d+)/artifacts\?", route)
            self.assertIsNotNone(match)
            return artifacts[int(match[1])]

        return selector.select_baseline(REPO, REPO_ID, 9, get_rows)

    def test_latest_main_attempt_with_both_passes_can_have_failing_timing(self):
        for conclusion in ("failure", "timed_out"):
            with self.subTest(conclusion=conclusion):
                attempt = jobs(5)
                attempt[-1]["conclusion"] = conclusion
                self.assertEqual(
                    self.select(
                        [
                            run(5, event="workflow_dispatch", conclusion=conclusion),
                            run(4, conclusion="success"),
                        ],
                        {(5, 1): attempt},
                        {5: [artifact(5)]},
                    ),
                    (5, 1, 501),
                )

    def test_original_job_failure_missing_or_wrong_attempt_is_ineligible(self):
        for bad_jobs in (
            jobs(5)[:1],
            jobs(5) + [jobs(5)[0]],
            jobs(5, 2),
            [{**jobs(5)[0], "conclusion": "failure"}, *jobs(5)[1:]],
            [jobs(5)[0], {**jobs(5)[1], "conclusion": "failure"}, jobs(5)[2]],
            [*jobs(5)[:2], {**jobs(5)[2], "name": "Unexpected failed job"}],
        ):
            with self.subTest(jobs=bad_jobs):
                self.assertEqual(
                    self.select(
                        [run(5), run(4)],
                        {(5, 1): bad_jobs, (4, 1): jobs(4)},
                        {4: [artifact(4)]},
                    ),
                    (4, 1, 401),
                )

    def test_partial_rerun_cannot_join_jobs_or_borrow_its_artifact(self):
        self.assertEqual(
            self.select(
                [run(5, run_attempt=2)],
                {(5, 2): jobs(5, 2)[:1], (5, 1): jobs(5)},
                {5: [artifact(5, 2), artifact(5)]},
            ),
            (5, 1, 501),
        )
        self.assertIsNone(
            self.select(
                [run(5, run_attempt=2)],
                {(5, 2): jobs(5, 2), (5, 1): jobs(5)[:1]},
                {5: [artifact(5)]},
            )
        )

    def test_branch_event_and_repository_aliases_do_not_qualify(self):
        self.assertEqual(
            self.select(
                [
                    run(9),
                    run(8, event="pull_request"),
                    run(7, head_branch="topic"),
                    run(6, head_repository={"id": REPO_ID + 1}),
                    run(5, status="in_progress"),
                    run(10, conclusion="cancelled"),
                    run(4),
                ],
                {(4, 1): jobs(4)},
                {4: [artifact(4)]},
            ),
            (4, 1, 401),
        )

    def test_stale_or_ambiguous_artifacts_do_not_qualify(self):
        wrong_sha = artifact(5)
        wrong_sha["workflow_run"]["head_sha"] = "b" * 40
        for bad_artifacts in (
            [wrong_sha],
            [artifact(5, expired=True)],
            [artifact(5), artifact(5, id=999)],
            [artifact(5, legacy=True)],
        ):
            with self.subTest(artifacts=bad_artifacts):
                self.assertIsNone(self.select([run(5)], {(5, 1): jobs(5)}, {5: bad_artifacts}))

    def test_artifact_requires_provider_sha256_in_both_formats(self):
        for legacy in (False, True):
            for digest in (None, "sha256:abc", "sha512:" + "a" * 64, 1):
                with self.subTest(legacy=legacy, digest=digest):
                    report = artifact(5, legacy=legacy, digest=digest)
                    self.assertIsNone(
                        self.select([run(5)], {(5, 1): jobs(5, legacy=legacy)}, {5: [report]})
                    )

    def test_attempt_suffix_does_not_override_the_successful_upload_window(self):
        for created in ("2026-11-23T11:34:41Z", "2026-11-23T11:34:44Z"):
            with self.subTest(created=created):
                self.assertIsNone(
                    self.select(
                        [run(5, run_attempt=2)],
                        {(5, 2): jobs(5, 2), (5, 1): []},
                        {5: [artifact(5, 2, created_at=created)]},
                    )
                )

    def test_legacy_artifact_requires_original_successful_upload_window(self):
        self.assertEqual(
            self.select(
                [run(4, conclusion="success")],
                {(4, 1): jobs(4, legacy=True)},
                {4: [artifact(4, legacy=True)]},
            ),
            (4, 1, 401),
        )
        for created in ("2026-11-23T11:34:41Z", "2026-11-23T11:34:44Z", "unavailable"):
            with self.subTest(created=created):
                self.assertIsNone(
                    self.select(
                        [run(4, conclusion="success")],
                        {(4, 1): jobs(4, legacy=True)},
                        {4: [artifact(4, legacy=True, created_at=created)]},
                    )
                )

class BaselineLookupTests(unittest.TestCase):
    def call_with_environment(self, replies):
        with TemporaryDirectory() as directory:
            output_path = Path(directory) / "output"
            env = {
                "GITHUB_OUTPUT": str(output_path),
                "GITHUB_REPOSITORY": REPO,
                "GITHUB_REPOSITORY_ID": str(REPO_ID),
                "GITHUB_RUN_ID": "9",
            }
            with (
                patch.dict(os.environ, env),
                patch.object(selector.subprocess, "check_output", side_effect=replies) as provider,
                redirect_stdout(StringIO()) as stdout,
            ):
                selector.main()
            outputs = output_path.read_text(encoding="utf-8") if output_path.exists() else None
            return stdout.getvalue(), outputs, provider.call_args_list

    def test_utf8_provider_response_decodes_real_non_ascii_bytes(self):
        row = {"display_title": "緊急 – Résumé"}
        response = json.dumps([{"workflow_runs": [row]}], ensure_ascii=False).encode("utf-8")
        check_output = subprocess.check_output

        def fake_gh(argv, **kwargs):
            self.assertEqual(argv[:5], ["gh", "api", "--method", "GET", "--paginate"])
            self.assertEqual(kwargs["encoding"], "utf-8")
            return check_output(
                [sys.executable, "-c", f"import sys; sys.stdout.buffer.write({response!r})"],
                **kwargs,
            )

        with patch.object(selector.subprocess, "check_output", side_effect=fake_gh):
            self.assertEqual(
                selector.github_rows("private-route", "workflow_runs", deadline=selector.monotonic() + 5),
                [row],
            )

    def test_timeout_retries_same_get_then_selects_verified_artifact(self):
        transient = subprocess.TimeoutExpired("private-route", 15)
        log, outputs, calls = self.call_with_environment([
            transient,
            json.dumps([{"workflow_runs": [run(5)]}]),
            json.dumps([{"jobs": jobs(5)}]),
            json.dumps([{"artifacts": [artifact(5)]}]),
        ])
        self.assertEqual(len(calls), 4)
        self.assertEqual(calls[0], calls[1])
        self.assertTrue(all(0 < c.kwargs["timeout"] <= 15 for c in calls))
        self.assertEqual(outputs, "run-id=5\nartifact-id=501\n")
        self.assertIn("stage=workflow_runs reason=timeout", log)
        self.assertIn("Windows perf baseline run 5 attempt 1, artifact 501", log)
        self.assertNotIn("private-route", log)

    def test_timeout_retry_cannot_extend_total_deadline(self):
        transient = subprocess.TimeoutExpired("private-route", 15)
        with (
            patch.object(selector, "monotonic", side_effect=[10, 10, 25, 25, 28]),
            patch.object(selector.subprocess, "check_output", side_effect=transient) as provider,
            redirect_stdout(StringIO()),
        ):
            with self.assertRaises(selector.LookupUnavailable) as raised:
                selector.github_rows("private-route", "jobs", deadline=28)
        self.assertEqual((raised.exception.stage, raised.exception.reason), ("jobs", "timeout"))
        self.assertEqual([c.kwargs["timeout"] for c in provider.call_args_list], [15, 3])

    def test_lookup_unavailability_is_typed_sanitized_and_leaves_no_output(self):
        private = "private-route-and-stderr"
        cases = (
            ("provider-exit", [subprocess.CalledProcessError(1, private, stderr=private)]),
            ("encoding", [UnicodeDecodeError("utf-8", private.encode(), 0, 1, private)]),
            ("process-io", [OSError(private)]),
            ("json", [private]),
            ("response-shape", [json.dumps([{"jobs": private}])]),
        )
        runs = json.dumps([{"workflow_runs": [run(5)]}])
        for reason, replies in cases:
            with self.subTest(reason=reason):
                log, outputs, calls = self.call_with_environment([runs, *replies])
                self.assertIsNone(outputs)
                self.assertEqual(len(calls), 2)
                self.assertIn("lookup unavailable", log)
                self.assertRegex(log, rf"stage=jobs reason={reason} elapsed=\d+\.\d{{3}}s")
                self.assertNotIn(private, log)

    def test_empty_eligible_history_is_distinct_from_lookup_failure(self):
        log, outputs, calls = self.call_with_environment(['[{"workflow_runs":[]}]'])
        self.assertIsNone(outputs)
        self.assertEqual(len(calls), 1)
        self.assertIn("No eligible main Windows perf baseline found.", log)
        self.assertNotIn("lookup unavailable", log)


class TimingRouteTests(unittest.TestCase):
    timing = WORKFLOW.read_text(encoding="utf-8").split("  timing-asserts:\n", 1)[1]

    def step_script(self, name):
        step = self.timing.split(f"      - name: {name}\n", 1)[1].split("      - name: ", 1)[0]
        return textwrap.dedent(step.split("        run: |\n", 1)[1])

    def test_exact_timing_commands_propagate_failure_and_keep_both_logs(self):
        timing = self.timing
        self.assertIn("name: Linux timing assertions", timing)
        self.assertIn("runs-on: ubuntu-24.04", timing)
        self.assertIn("github.event.label.name == 'perf-assert'", timing)
        self.assertIn("steps.mesa.outcome == 'success'", timing)
        self.assertEqual(timing.count('TZE_HUD_PERF_ASSERT: "1"'), 2)
        self.assertEqual(timing.count("shell: bash"), 2)
        self.assertIn("name: perf-assert-budget-p99", timing)
        for step, arguments, log in (
            (
                "Assert portal cadence timing budgets",
                "test -p integration --test it text_stream_portal_cadence:: -- --nocapture",
                "portal.log",
            ),
            (
                "Assert vertical slice timing budgets",
                "test -p vertical_slice --test budget_assertions -- --nocapture",
                "budget.log",
            ),
        ):
            with self.subTest(step=step), TemporaryDirectory() as directory:
                script = self.step_script(step)
                mock = "cargo() { printf '%s\\n' \"$*\"; return 37; }\n"
                result = subprocess.run(
                    ["bash", "-eo", "pipefail", "-c", mock + script],
                    cwd=directory,
                    capture_output=True,
                )
                self.assertEqual(result.returncode, 37)
                retained = Path(directory) / "test_results/perf-assert" / log
                self.assertEqual(retained.read_text().strip(), arguments)

    def test_budget_collector_preserves_p99_schema(self):
        with TemporaryDirectory() as directory:
            evidence = Path(directory) / "test_results/perf-assert"
            evidence.mkdir(parents=True)
            (evidence / "budget.log").write_text(
                "[PASS] frame_time p99=2153us within budget=332000us\n"
                "[SKIP-TIMING] input_to_local_ack raw_p99=50us; disabled\n"
                "[UNCALIBRATED] stage6_render_encode raw_p99=400us; missing baseline\n",
                encoding="utf-8",
            )
            subprocess.run(
                ["bash", "-eo", "pipefail", "-c", self.step_script("Collect p99 JSON trend artifacts")],
                cwd=directory,
                check=True,
                capture_output=True,
            )
            self.assertEqual(
                json.loads((evidence / "budget-p99.json").read_text(encoding="utf-8")),
                {"entries": [
                    {"status": "PASS", "metric": "frame_time", "p99_us": 2153, "budget_us": 332000},
                    {"status": "SKIP-TIMING", "metric": "input_to_local_ack", "p99_us": 50},
                    {"status": "UNCALIBRATED", "metric": "stage6_render_encode", "p99_us": 400},
                ]},
            )

    def test_windows_downloads_the_selected_artifact_and_keeps_publish_scope(self):
        workflow = WORKFLOW.read_text(encoding="utf-8")
        windows = workflow.split("  constrained-envelope-budget:\n", 1)[0]
        download = windows.split("      - name: Download main-pinned Windows perf baseline\n", 1)[1]
        download = download.split("      - name: ", 1)[0]
        self.assertIn("artifact-ids: ${{ steps.perf-baseline.outputs.artifact-id }}", download)
        self.assertIn("run-id: ${{ steps.perf-baseline.outputs.run-id }}", download)
        self.assertIn("continue-on-error: true", download)
        self.assertIn("uses: actions/download-artifact@v8", download)
        self.assertIn("digest-mismatch: error", download)
        self.assertIn("BASELINE_DOWNLOAD_OUTCOME: ${{ steps.perf-baseline-download.outcome }}", windows)
        self.assertIn("$env:BASELINE_DOWNLOAD_OUTCOME -eq 'success' -and (Test-Path $baseline)", windows)
        self.assertIn("windows-perf-baseline-main-attempt-${{ github.run_attempt }}", windows)
        self.assertIn("if: success() && github.event_name != 'pull_request' && github.ref == 'refs/heads/main'", windows)


if __name__ == "__main__":
    unittest.main()
