"""Unit tests for the ambient-background exemplar's hud_surfaces parsing."""

import sys
from pathlib import Path
from unittest import mock

SCRIPT_DIR = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPT_DIR))

import ambient_background_exemplar as ambient  # noqa: E402

SURFACES = {
    "result": {
        "surfaces": [
            {"s": "zone:subtitle", "accepts": "text"},
            {"s": "zone:ambient-background", "accepts": "color", "held": True},
        ]
    }
}


def test_find_zone_entry_reads_hud_surfaces_shape():
    assert ambient.find_zone_entry(SURFACES)["held"] is True
    assert ambient.find_zone_entry({"result": {"surfaces": []}}) is None


def test_rapid_phase_passes_when_zone_is_held():
    with mock.patch.object(ambient, "rpc_call", return_value={"result": {"ok": True}}), \
         mock.patch.object(ambient, "list_surfaces", return_value=SURFACES):
        _, ok = ambient.phase4_rapid_replacement("http://x", "t", 1)
    assert ok
