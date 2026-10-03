#!/usr/bin/env python3
"""Smoke tests for scripts/dead_code.py (pure helpers; no cargo run)."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
import unittest

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


if __name__ == "__main__":
    unittest.main()
