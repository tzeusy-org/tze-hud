"""Contract tests for check_current_references.py."""
from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "check_current_references", Path(__file__).with_name("check_current_references.py"))
chk = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(chk)


def j(*parts: str) -> str:
    # Assembled at runtime so this tracked file stays clean on its own.
    return "".join(parts)


class Citations(unittest.TestCase):
    def assert_flagged(self, line: str, path: str = "crates/x/src/lib.rs"):
        self.assertEqual([n for n, _ in chk.scan_text(path, line)], [1], line)

    def test_each_retired_form_is_flagged_case_insensitively(self):
        for line in [
            j("// see Open", "Spec"),
            j("// per the doc", "trine"),
            j("/// Requirement", ": Lease Caps"),
            j("//! Zone Registry (line ", "185)"),
            j("// deferred to Post", "-v1"),
            j("// v1-reserv", "ed field"),
            j("// docs/desi", "gn/windows-dpi.md"),
            j('rfc_id: "RF', 'C-0005"'),
            j("// RF", "C 0001 section 3"),
            j("# rf", "c0042"),
        ]:
            self.assert_flagged(line)

    def test_citations_apply_to_non_rust_files_too(self):
        self.assert_flagged(j("# GPU upload is post", "-v1"), "scripts/x.py")

    def test_ordinary_text_passes(self):
        text = "\n".join([
            "// line 3 of the buffer", "parse error at (line {})", "v1 wire format",
            "requirements are met", "the design doc", "RFC-style naming",
            "fn rfc3339_utc()", "per RFC 3339 and RFC 9110",
        ])
        self.assertEqual(chk.scan_text("crates/x/src/lib.rs", text), [])


class BeadIds(unittest.TestCase):
    def test_closed_bead_in_rust_or_proto_is_flagged(self):
        for path in ["crates/x/src/lib.rs", "crates/x/proto/a.proto"]:
            findings = chk.scan_text(path, j('assert!(ok, "regression (hu', 'd-zccuf)");'))
            self.assertEqual(len(findings), 1, path)
            self.assertIn(j("hu", "d-zccuf"), findings[0][1])

    def test_open_beads_and_their_children_pass(self):
        text = j("// tracked by hu", "d-ljhku.13 and hu", "d-1pt5h")
        self.assertEqual(chk.scan_text("crates/x/src/lib.rs", text), [])

    def test_prefix_of_an_open_bead_id_does_not_whitelist_another(self):
        self.assertEqual(len(chk.scan_text("a.rs", j("// hu", "d-ljhkuz"))), 0)
        self.assertEqual(len(chk.scan_text("a.rs", j("// hu", "d-ljhkx"))), 1)

    def test_bead_like_names_pass(self):
        text = '"hud-projection" "hud-diag.log" "hud-hotkeys" "--tze-hud-firewall-child"'
        self.assertEqual(chk.scan_text("crates/x/src/lib.rs", text), [])

    def test_bead_ids_outside_rust_and_proto_are_not_checked(self):
        self.assertEqual(chk.scan_text("docs/notes.md", j("hu", "d-zccuf")), [])


class Exemptions(unittest.TestCase):
    def test_docs_reset_notices_and_t7_files_are_exempt(self):
        for path in ["docs/scope.md", "AGENTS.md", "CLAUDE.md",
                     "crates/tze_hud_projection/src/lib.rs",
                     "crates/tze_hud_input/src/composer_draft.rs",
                     "tests/integration/text_stream_portal_markdown.rs"]:
            self.assertTrue(chk.is_exempt(path), path)

    def test_ordinary_source_is_scanned(self):
        self.assertFalse(chk.is_exempt("crates/tze_hud_runtime/src/portal.rs"))


class Repository(unittest.TestCase):
    def test_current_tree_passes(self):
        self.assertEqual(chk.main(), 0)


if __name__ == "__main__":
    unittest.main()
