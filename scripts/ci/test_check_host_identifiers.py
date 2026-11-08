"""Contract tests for check_host_identifiers.py."""
from __future__ import annotations

import importlib.util
import contextlib
import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

_spec = importlib.util.spec_from_file_location(
    "check_host_identifiers", Path(__file__).with_name("check_host_identifiers.py"))
chk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(chk)


class ScanText(unittest.TestCase):
    def test_synthetic_cgnat_fixtures_and_range_bounds_pass(self):
        text = 'reach("100.100.1.2:9290")\nconst TAILNET = "100.64.0.0/10";\n"100.127.255.255"'
        self.assertEqual(chk.scan_text(text, []), [])

    def test_other_cgnat_literal_fails_without_echoing_it(self):
        # Built at runtime so this tracked file does not itself trip the check.
        findings = chk.scan_text('let a = "' + ".".join(["100", "99", "12", "34"]) + ':9090";', [])
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0][0], 1)
        self.assertNotIn("100.99", findings[0][1])

    def test_address_before_a_period_or_in_a_key_filename_fails(self):
        addr = ".".join(["100", "99", "12", "34"])
        self.assertEqual(len(chk.scan_text(f"reachable at {addr}.", [])), 1)
        self.assertEqual(len(chk.scan_text(f'd+"/{addr}.psk"', [])), 1)

    def test_ordinary_private_and_documentation_addresses_pass(self):
        text = "192.168.1.5 10.0.0.1 127.0.0.1 192.0.2.7 198.51.100.4 203.0.113.9 1.100.64.2.3"
        self.assertEqual(chk.scan_text(text, []), [])

    def test_magicdns_and_private_key_names_fail(self):
        self.assertEqual(len(chk.scan_text("host.tail1234." + "ts.net", [])), 1)
        self.assertEqual(len(chk.scan_text("-i ~/.keys/" + "id_" + "ed25519 x", [])), 1)

    def test_local_denylist_matches_case_insensitively(self):
        findings = chk.scan_text("Overlay on My-Desktop-01", ["my-desktop-01"])
        self.assertEqual([rule for _, rule in findings], ["local denylist entry"])


class Denylist(unittest.TestCase):
    def test_loads_entries_skipping_comments_and_blanks(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "deny"
            path.write_text("# real hosts\n\nMy-Host\n  other  \n", encoding="utf-8")
            self.assertEqual(chk.load_denylist(path), ["my-host", "other"])

    def test_missing_file_is_an_empty_denylist(self):
        self.assertEqual(chk.load_denylist(Path("/nonexistent/deny")), [])

    def test_env_override_selects_the_denylist_path(self):
        with mock.patch.dict(os.environ, {"TZE_HUD_HOST_DENYLIST": "/tmp/x.deny"}):
            self.assertEqual(chk.default_denylist(), Path("/tmp/x.deny"))


class Main(unittest.TestCase):
    def test_fails_on_a_tracked_value_without_printing_it(self):
        secret_host = "fixture-host-" + "7f3a"
        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            (repo / "a.rs").write_text(f'const H: &str = "{secret_host}";\n', encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "a.rs"], check=True)
            deny = repo.parent / (repo.name + ".deny")
            deny.write_text(secret_host + "\n", encoding="utf-8")
            err, out = io.StringIO(), io.StringIO()
            cwd = os.getcwd()
            try:
                os.chdir(repo)
                with mock.patch.dict(os.environ, {"TZE_HUD_HOST_DENYLIST": str(deny)}), \
                        contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
                    code = chk.main([])
            finally:
                os.chdir(cwd)
                deny.unlink()
            self.assertEqual(code, 1)
            self.assertIn("a.rs:1: local denylist entry", err.getvalue())
            self.assertNotIn(secret_host, err.getvalue() + out.getvalue())


if __name__ == "__main__":
    unittest.main()
