"""Tests for the per-surface line report."""

import io
import subprocess
import tempfile
import unittest
import unittest.mock
from pathlib import Path

import size_report

ROOT = Path(__file__).resolve().parents[2]

LIB = """pub fn f() {}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        let _ = "}";
    }
}
#[cfg(test)]
mod golden;
"""

TARGETS = """
[targets.engine]
surfaces = ["crates/a", "crates/b"]
rust = 5
rust_test = 2
"""


class SizeReportTest(unittest.TestCase):
    def test_splits_rust_production_and_test_per_surface(self):
        rows = size_report.count({
            "crates/a/src/lib.rs": LIB,
            "crates/a/src/golden.rs": "fn g() {}\n\n",
            "crates/a/tests/it.rs": "#[test]\nfn t() {}\n",
            "app/b/src/main.rs": "fn main() {}\n",
        })
        # The inline block is 7 lines; the `mod golden;` declaration stays production.
        self.assertEqual(rows["crates/a"]["rust"], 4)
        self.assertEqual(rows["crates/a"]["rust_test"], 7 + 2 + 2)
        self.assertEqual(rows["app/b"]["rust"], 1)

    def test_all_test_cfg_is_test_but_any_test_is_shared(self):
        text = (
            "#[cfg(all(test, feature = \"headless\"))]\nmod a {\n}\n"
            "#[cfg(any(test, feature = \"x\"))]\nmod b {\n}\n"
        )
        rows = size_report.count({"examples/c/src/main.rs": text})
        self.assertEqual(rows["examples/c"]["rust_test"], 3)
        self.assertEqual(rows["examples/c"]["rust"], 3)

    def test_python_and_markdown_surfaces(self):
        rows = size_report.count({
            "scripts/ci/tool.py": "a\nb\n",
            "scripts/ci/test_tool.py": "a\n",
            ".claude/skills/x/SKILL.md": "a\nb\nc\n",
            ".claude/skills/x/tests/helper.py": "a\n",
            "docs/api.md": "a\n",
            ".beads/issues.md": "a\n",
            "README.md": "a\n",
        })
        self.assertEqual(rows["scripts"], {**rows["scripts"], "python": 2, "python_test": 1})
        self.assertEqual(rows[".claude/skills"]["markdown"], 3)
        self.assertEqual(rows[".claude/skills"]["python_test"], 1)
        self.assertEqual(rows["docs"]["markdown"], 1)
        self.assertEqual(rows["(root)"]["markdown"], 1)
        self.assertNotIn(".beads", rows)

    def test_rust_total_matches_wc_over_tracked_files(self):
        wc = subprocess.run(
            "git ls-files '*.rs' | xargs cat | wc -l",
            shell=True, cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        rows = size_report.count(size_report.tracked(ROOT))
        total = sum(r["rust"] + r["rust_test"] for r in rows.values())
        self.assertEqual(total, int(wc))

    def test_writes_the_artifact(self):
        with tempfile.TemporaryDirectory() as out:
            with open("/dev/null", "w") as sink, unittest.mock.patch("sys.stdout", sink):
                size_report.main(["--root", str(ROOT), "--output-dir", out])
            text = (Path(out) / "size-report.md").read_text()
            self.assertIn("| **total** |", text)
            self.assertTrue((Path(out) / "size-report.json").is_file())


class SizeGateTest(unittest.TestCase):
    def setUp(self):
        repo = tempfile.TemporaryDirectory()
        self.addCleanup(repo.cleanup)
        self.root = Path(repo.name)
        self.git("init", "-q")
        self.write("scripts/ci/size_targets.toml", TARGETS)
        self.write("crates/a/src/lib.rs", "// a\n" * 6)
        self.write("crates/b/src/lib.rs", "// b\n" * 4)
        self.write("crates/a/tests/it.rs", "// test\n" * 2)
        self.write("docs/readme.md", "report only\n")
        self.git("add", ".")
        self.git(
            "-c", "user.name=Size Gate", "-c", "user.email=gate@example.invalid",
            "-c", "commit.gpgSign=false", "commit", "-qm", "Comparison base",
        )

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.root, check=True, capture_output=True,
        )

    def write(self, path, text):
        path = self.root / path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def report(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with unittest.mock.patch("sys.stdout", out), unittest.mock.patch("sys.stderr", err):
            status = size_report.main(["--root", str(self.root), *args])
        return status, out.getvalue(), err.getvalue()

    def test_rejects_growth_over_target_and_reports_counts(self):
        self.write("crates/a/src/lib.rs", "// a\n" * 7)
        status, out, err = self.report("--base", "HEAD", "--output-dir", str(self.root / "out"))
        self.assertEqual(status, 1)
        self.assertIn("engine (rust) grew from 10 to 11 lines above its 5-line ceiling", err)
        self.assertIn("| engine | rust | 11 | 10 | 5 | 0 | +6 |", out)
        self.assertIn("| engine | rust_test | 2 | 2 | 2 | 0 | +0 |", out)
        self.assertIn("| docs | 0 | 0 | 0 | 0 | 1 |", out)
        self.assertEqual((self.root / "out/size-report.md").read_text(), out)

    def test_over_target_flat_or_declining_passes(self):
        self.assertEqual(self.report("--base", "HEAD")[0], 0)
        self.write("crates/a/src/lib.rs", "// a\n" * 5)
        self.assertEqual(self.report("--base", "HEAD")[0], 0)

    def test_growth_under_target_passes(self):
        self.write("scripts/ci/size_targets.toml", TARGETS.replace("rust = 5", "rust = 12"))
        self.write("crates/a/src/lib.rs", "// a\n" * 7)
        self.assertEqual(self.report("--base", "HEAD")[0], 0)

    def test_growth_at_signed_allowance_passes(self):
        self.write("crates/a/src/lib.rs", "// a\n" * 7)
        self.write("scripts/ci/size_targets.toml", TARGETS + """
[[allowances]]
feature = "new-feature"
approval = "owner-signed feature spec"
target = "engine"
rust = 6
""")
        status, out, err = self.report("--base", "HEAD")
        self.assertEqual(status, 0, err)
        self.assertIn("| engine | rust | 11 | 10 | 5 | 6 | +6 |", out)
        self.write("crates/a/src/lib.rs", "// a\n" * 8)
        status, _, err = self.report("--base", "HEAD")
        self.assertEqual(status, 1)
        self.assertIn("12 lines above its 11-line ceiling", err)

    def test_report_only_can_run_without_gate_inputs(self):
        (self.root / "scripts/ci/size_targets.toml").unlink()
        status, out, err = self.report()
        self.assertEqual(status, 0, err)
        self.assertIn("| **total** | 10 | 2 | 0 | 0 | 1 |", out)

    def test_gate_requires_an_available_comparison_and_valid_policy(self):
        status, _, err = self.report("--base", "missing")
        self.assertEqual(status, 2)
        self.assertIn("cannot read comparison commit 'missing'", err)
        policy = self.root / "scripts/ci/size_targets.toml"
        policy.unlink()
        status, _, err = self.report("--base", "HEAD")
        self.assertEqual(status, 2)
        self.assertIn("size_targets.toml", err)
        for malformed in (
            "[targets",
            TARGETS.replace("rust = 5", "rust = -5"),
            TARGETS.replace('"crates/b"', '"crates/a"'),
            TARGETS + '\n[[allowances]]\nfeature = "unsigned"\ntarget = "engine"\nrust = 10',
        ):
            with self.subTest(malformed=malformed):
                policy.write_text(malformed)
                self.assertEqual(self.report("--base", "HEAD")[0], 2)


if __name__ == "__main__":
    unittest.main()
