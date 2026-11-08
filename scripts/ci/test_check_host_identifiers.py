"""Contract tests for check_host_identifiers.py."""
from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

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
        old = os.environ.get("TZE_HUD_HOST_DENYLIST")
        os.environ["TZE_HUD_HOST_DENYLIST"] = "/tmp/x.deny"
        try:
            self.assertEqual(chk.default_denylist(), Path("/tmp/x.deny"))
        finally:
            if old is None:
                del os.environ["TZE_HUD_HOST_DENYLIST"]
            else:
                os.environ["TZE_HUD_HOST_DENYLIST"] = old


if __name__ == "__main__":
    unittest.main()
