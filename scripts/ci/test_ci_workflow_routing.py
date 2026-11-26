"""Offline checks for the protected, main-cache and scheduled CI routes."""

import os
from pathlib import Path
import re
import subprocess
import textwrap
import unittest

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github/workflows"
CI = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
CACHE = (WORKFLOWS / "ci-main-cache.yml").read_text(encoding="utf-8")
ADVISORIES = (WORKFLOWS / "ci-advisories.yml").read_text(encoding="utf-8")


def section(workflow: str, key: str) -> str:
    match = re.search(rf"^{key}:\n(.*?)(?=^[a-z][a-z-]*:|\Z)", workflow, re.M | re.S)
    assert match is not None, key
    return match[1]


def child_keys(block: str) -> list[str]:
    return re.findall(r"^  ([a-z][a-z_-]*):", block, re.M)


def step(workflow: str, name: str) -> str:
    start = f"      - name: {name}\n"
    match = re.search(
        rf"^{re.escape(start)}(.*?)(?=^      - |^  [a-z][a-z_-]*:|\Z)",
        workflow,
        re.M | re.S,
    )
    assert match is not None, name
    return match[1]


class WorkflowRoutingTests(unittest.TestCase):
    def test_protected_routes_emit_all_three_required_checks(self) -> None:
        triggers = section(CI, "on")
        self.assertEqual(child_keys(triggers), ["pull_request", "merge_group"])
        self.assertIn("pull_request:\n    branches: [main]", triggers)
        self.assertEqual(child_keys(section(CI, "jobs")), ["static", "clippy", "test"])
        self.assertEqual(
            re.findall(r"^    name: (.+)$", CI, re.M),
            ["static checks", "cargo clippy", "cargo test"],
        )
        self.assertNotRegex(CI, r"^    if:", "required jobs must always run on both events")
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", CI)
        self.assertIn("github.event.merge_group.base_sha", CI)
        for receipt in (
            "name: size-report",
            "name: nextest-unit-${{ github.run_id }}-${{ github.run_attempt }}",
            "target/nextest/default/junit.xml",
            "name: token-footprint-calibration",
            'TZE_HUD_REQUIRE_GPU: "1"',
        ):
            self.assertIn(receipt, CI)

    def test_main_push_writes_only_the_two_incumbent_cache_keys(self) -> None:
        self.assertEqual(child_keys(section(CACHE, "on")), ["push"])
        self.assertIn("push:\n    branches: [main]", CACHE)
        self.assertEqual(child_keys(section(CACHE, "jobs")), ["warm-cache"])
        concurrency = section(CACHE, "concurrency")
        self.assertIn("  queue: max", concurrency)
        self.assertNotIn("cancel-in-progress", concurrency)
        self.assertIn("ref: ${{ github.sha }}", CACHE)
        self.assertEqual(
            re.findall(r"shared-key: (.+)\n +save-if: (.+)", CI),
            [("clippy", "false"), ("test", "false")],
        )
        self.assertEqual(
            re.findall(r"shared-key: (.+)\n +save-if: (.+)", CACHE),
            [("clippy", "true"), ("test", "true")],
        )
        self.assertLess(CACHE.index("id: test-cache"), CACHE.index("cargo test --locked"))
        cache_miss = (
            "steps.clippy-cache.outputs.cache-hit != 'true' || "
            "steps.test-cache.outputs.cache-hit != 'true'"
        )
        self.assertEqual(CACHE.count(f"if: {cache_miss}"), 3)
        commands = re.findall(r"^ +(?:run: )?cargo (.+)$", CACHE, re.M)
        self.assertEqual(
            [command.split()[0] for command in commands],
            ["test", "test", "test", "build", "check", "check"],
        )
        self.assertTrue(all("--locked" in command.split() for command in commands))
        self.assertTrue(all("--no-run" in command.split() for command in commands[:3]))

    def test_protected_size_gate_uses_the_tested_base(self) -> None:
        published = step(CI, "Report and check lines per surface")
        script = textwrap.dedent(published.split("        run: |\n", 1)[1])
        pr_base = "a" * 40
        merge_base = "b" * 40
        # Keep git and the report offline while running the actual workflow script.
        mock_commands = """
git() {
  case "$*" in
    'cat-file commit HEAD') printf 'tree fake\\nparent %s\\n' "$PR_BASE" ;;
    'cat-file -e '*) return 1 ;;
    'fetch --no-tags --depth=1 origin '*) printf 'fetch %s\\n' "$5" ;;
    *) return 90 ;;
  esac
}
python3() { printf '%s\\n' "$*"; }
"""
        cases = (
            ("pull_request", merge_base, pr_base),
            ("merge_group", merge_base, merge_base),
            ("merge_group", "invalid", None),
            ("push", merge_base, None),
        )
        for event, supplied_base, expected in cases:
            with self.subTest(event=event, supplied_base=supplied_base):
                result = subprocess.run(
                    ["bash", "-eo", "pipefail", "-c", mock_commands + script],
                    env={
                        **os.environ,
                        "GITHUB_EVENT_NAME": event,
                        "PR_BASE": pr_base,
                        "MERGE_GROUP_BASE": supplied_base,
                    },
                    capture_output=True,
                    text=True,
                )
                if expected is None:
                    self.assertEqual(result.returncode, 2)
                    self.assertEqual(result.stdout, "")
                    self.assertIn("size gate:", result.stderr)
                    continue
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    result.stdout.splitlines(),
                    [
                        f"fetch {expected}",
                        "scripts/ci/size_report.py --output-dir test_results/size-report"
                        f" --base {expected}",
                    ],
                )

    def test_scheduled_advisory_failure_propagates(self) -> None:
        self.assertEqual(child_keys(section(ADVISORIES, "on")), ["schedule"])
        self.assertRegex(ADVISORIES, r'(?m)^    - cron: "\d+ \d+ \* \* \*"')
        self.assertEqual(child_keys(section(ADVISORIES, "jobs")), ["advisories"])
        self.assertIn("ref: ${{ github.sha }}", ADVISORIES)
        self.assertIn("  contents: read", ADVISORIES)
        self.assertIn("tool: cargo-deny@0.20.2", ADVISORIES)
        self.assertNotIn("continue-on-error", ADVISORIES)
        protected = step(CI, "cargo deny (advisories)")
        self.assertIn("continue-on-error: true", protected)
        licenses = step(CI, "cargo deny (licenses, bans, sources)")
        self.assertNotIn("continue-on-error", licenses)

        command = "cargo deny --log-level warn check advisories"
        self.assertIn(f"run: {command}", protected)
        scheduled = step(ADVISORIES, "Check dependency advisories")
        script = scheduled.split("        run: ", 1)[1]
        result = subprocess.run(
            ["bash", "-e", "-c", "cargo() { printf '%s\\n' \"$*\"; return 37; }\n" + script],
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 37)
        self.assertEqual(result.stdout.strip(), "deny --log-level warn check advisories")


if __name__ == "__main__":
    unittest.main()
