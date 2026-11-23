"""Tests for the per-surface line report."""

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


if __name__ == "__main__":
    unittest.main()
