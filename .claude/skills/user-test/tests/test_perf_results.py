"""Result-schema cleanup preserves historical measurements and comparison keys."""

import csv
import importlib.util
import json
from pathlib import Path


def test_header_migration_preserves_historical_results(tmp_path, monkeypatch):
    root = Path(__file__).resolve().parents[4]
    skill = root / ".claude/skills/user-test-performance"
    monkeypatch.syspath_prepend(str(skill / "scripts"))
    spec = importlib.util.spec_from_file_location("perf_results_common", skill / "scripts/perf_common.py")
    common = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(common)
    original = skill / "reference/results.csv"
    with original.open(newline="") as source:
        rows = list(csv.DictReader(source))
    obsolete = {"trace_spec_ref", "trace_rfc_ref", "trace_doctrine_ref"}
    old_header = [*common.RESULTS_CSV_HEADER, *sorted(obsolete)]
    result_file = tmp_path / "results.csv"
    with result_file.open("w", newline="") as target:
        writer = csv.DictWriter(target, fieldnames=old_header)
        writer.writeheader()
        writer.writerows({**row, **dict.fromkeys(obsolete, "historical reference")} for row in rows)

    current = dict(rows[-1])
    fields = json.loads(current["primary_key_fields_json"])
    for key in obsolete:
        fields.pop(key, None)
    fields["trace_budget_ref"] = "current-budget"
    current["primary_key"], current["primary_key_fields_json"] = common.stable_primary_key(fields)
    current["recorded_at_utc"] = "2099-01-01T00:00:00+00:00"
    current["trace_budget_ref"] = "current-budget"
    common.append_results_csv(str(result_file), current)

    with result_file.open(newline="") as result:
        reader = csv.DictReader(result)
        actual = list(reader)
        assert not obsolete.intersection(reader.fieldnames)
    assert actual[:-1] == rows
    assert actual[-1] == current
    assert actual[-1]["trace_budget_ref"] == "current-budget"
