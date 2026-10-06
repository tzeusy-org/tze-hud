#!/usr/bin/env python3
"""Smoke tests for scripts/dead_code.py (pure helpers; no cargo run)."""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "dead_code.py"
spec = importlib.util.spec_from_file_location("dead_code", SCRIPT)
dead_code = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dead_code)


class NarrowTests(unittest.TestCase):
    def test_narrows_unused_pub_and_keeps_externally_used_or_reexports(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            crate = Path(tmp)
            (crate / "src").mkdir()
            lib = crate / "src" / "lib.rs"
            lib.write_text(
                "pub fn used_elsewhere() {}\n"
                "pub fn dead() {}\n"
                "pub(crate) fn already() {}\n"
                "pub use inner::Thing;\n"
                "    pub async fn dead_async() {}\n"
            )
            n = dead_code.narrow(crate, {"used_elsewhere"})
            self.assertEqual(n, 3)
            self.assertEqual(
                lib.read_text(),
                "pub fn used_elsewhere() {}\n"
                "pub(crate) fn dead() {}\n"
                "pub(crate) fn already() {}\n"
                "pub(crate) use inner::Thing;\n"
                "    pub(crate) async fn dead_async() {}\n",
            )


class MainTests(unittest.TestCase):
    def test_copy_skips_beads_and_own_build_dir_is_removed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            (repo / "crates" / "foo" / "src").mkdir(parents=True)
            (repo / "crates" / "foo" / "Cargo.toml").write_text('[package]\nname = "foo"\n')
            (repo / "crates" / "foo" / "src" / "lib.rs").write_text("pub fn f() {}\n")
            (repo / ".beads").mkdir()
            seen = {}

            def fake_cargo(cmd, cwd, env, **_):
                seen["beads_copied"] = (Path(cwd) / ".beads").exists()
                Path(env["CARGO_TARGET_DIR"]).mkdir(parents=True)  # cargo's build output
                return subprocess.CompletedProcess(cmd, 0, "", "")

            env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
            with mock.patch.object(dead_code, "REPO_ROOT", repo), \
                    mock.patch.object(dead_code.subprocess, "run", fake_cargo), \
                    mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(sys, "argv", ["dead_code.py", "foo"]):
                self.assertEqual(dead_code.main(), 0)
            self.assertFalse(seen["beads_copied"])
            self.assertFalse((repo / "target" / "dead-code").exists())


if __name__ == "__main__":
    unittest.main()
