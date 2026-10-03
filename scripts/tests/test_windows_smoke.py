#!/usr/bin/env python3
"""Pure helpers of the Windows CI smoke: polling, surface parsing, schedule checks."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import windows_smoke as ws  # noqa: E402


class FakeClock:
    """A clock that only moves when the code sleeps."""

    def __init__(self) -> None:
        self.now = 0.0
        self.sleeps: list[float] = []

    def __call__(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.sleeps.append(seconds)
        self.now += seconds


def poll(probe, **kw):
    clock = FakeClock()
    result = ws.wait_until(probe, clock=clock, sleep=clock.sleep, **kw)
    return result, clock


def test_wait_until_returns_observation_and_elapsed_when_condition_flips():
    calls = iter([(False, "a"), (False, "b"), (True, "c")])
    (observed, waited), clock = poll(lambda: next(calls), what="x", interval_s=1, timeout_s=10)
    assert (observed, waited) == ("c", 2.0)
    assert clock.sleeps == [1, 1]


def test_wait_until_probes_once_even_with_zero_timeout():
    (observed, waited), _ = poll(lambda: (True, "now"), what="x", timeout_s=0)
    assert (observed, waited) == ("now", 0.0)


def test_wait_until_times_out_naming_what_and_the_last_observation():
    seen = iter(range(100))
    with pytest.raises(AssertionError, match=r"tile gone: not met after 5s \(last seen: 5\)"):
        poll(lambda: (False, next(seen)), what="tile gone", timeout_s=5, interval_s=1)


def test_wait_until_never_sleeps_past_the_guard():
    clock = FakeClock()
    with pytest.raises(AssertionError):
        ws.wait_until(lambda: (False, None), what="x", timeout_s=2.5, interval_s=1, clock=clock, sleep=clock.sleep)
    assert clock.now == 2.5


SURFACES = [
    {"s": "zone:subtitle", "accepts": "text", "held": True, "expires_in_ms": 100},
    {"s": "zone:notification-area", "accepts": "notification"},
]


def test_is_held_reads_the_held_flag_and_a_missing_surface_fails_loudly():
    assert ws.is_held(SURFACES, "zone:subtitle") is True
    assert ws.is_held(SURFACES, "zone:notification-area") is False
    with pytest.raises(AssertionError, match="zone:nope not in hud_surfaces"):
        ws.is_held(SURFACES, "zone:nope")


def test_held_probe_is_done_when_held_matches_the_wanted_state():
    class Fake:
        held = True

        def ok(self, tool: str) -> dict:
            return {"surfaces": [{"s": "zone:z", **({"held": True} if self.held else {})}]}

    fake = Fake()
    assert ws.held_probe(fake, "zone:z", False)()[0] is False
    assert ws.held_probe(fake, "zone:z", True)()[0] is True
    fake.held = False
    assert ws.held_probe(fake, "zone:z", False)()[0] is True


def test_assert_not_early_allows_slack_but_rejects_an_early_event():
    ws.assert_not_early(2.6, 3.0, "x")
    with pytest.raises(AssertionError, match="before its 3.00s schedule"):
        ws.assert_not_early(1.0, 3.0, "x")


def test_parse_tile_count_finds_the_line_among_other_output():
    assert ws.parse_tile_count("connecting\ntiles 3\n") == 3
    for bad in ("", "tiles\n", "tiles x\n", "my tiles 2 more\n", "sessions 5\n"):
        with pytest.raises(AssertionError, match="no `tiles <n>` line"):
            ws.parse_tile_count(bad)


def test_quiescent_summary_reports_the_gate_counters():
    artifact = {
        "gpu": {"presents": 2, "queue_submissions": 2, "surface_acquisitions": 2},
        "wakeups": {"combined_runtime_driven": 7},
        "settling_duration_ms": 5000,
        "interval_duration_ms": 60000,
        "renderer": {"adapter": "Microsoft Basic Render Driver"},
    }
    line = ws.quiescent_summary(artifact)
    assert "presents=2" in line and "runtime_wakeups=7" in line and "Basic Render Driver" in line
    assert "presents=None" in ws.quiescent_summary({})
