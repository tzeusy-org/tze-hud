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

    marker = "synthetic-sensitive-error-detail"
    for error in (ws.urllib.error.URLError(marker), ConnectionError(marker), TimeoutError(marker)):
        observations = iter([(False, "a"), error, (True, "c")])

        def recovers():
            value = next(observations)
            if isinstance(value, BaseException):
                raise value
            return value

        (observed, waited), clock = poll(recovers, what="transport", interval_s=1, timeout_s=10)
        assert (observed, waited) == ("c", 2.0)
        assert clock.sleeps == [1, 1]

    errors = [
        *(ws.urllib.error.HTTPError("synthetic-url", code, marker, None, None) for code in (401, 403, 500)),
        AssertionError(marker),
        ws.json.JSONDecodeError(marker, "", 0),
        OSError(marker),
        ValueError(marker),
        TypeError(marker),
        ws.subprocess.TimeoutExpired("snapshot", 30),
    ]
    for error in errors:
        clock = FakeClock()
        attempts = []

        def rejected():
            attempts.append(True)
            raise error

        with pytest.raises(type(error)) as caught:
            ws.wait_until(rejected, what="fail closed", clock=clock, sleep=clock.sleep)
        assert caught.value is error
        assert attempts == [True] and clock.sleeps == []


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

    marker = "synthetic-sensitive-error-detail"
    for error in (ws.urllib.error.URLError(marker), ConnectionError(marker), TimeoutError(marker)):
        clock = FakeClock()
        attempts = []

        def unavailable():
            attempts.append(clock.now)
            raise error

        with pytest.raises(AssertionError) as caught:
            ws.wait_until(unavailable, what="bounded transport", timeout_s=2.5, interval_s=1,
                          clock=clock, sleep=clock.sleep)
        assert attempts == [0.0, 1.0, 2.0, 2.5]
        assert clock.now == 2.5 and clock.sleeps == [1, 1, 0.5]
        assert type(error).__name__ in str(caught.value)
        assert "no observation" in str(caught.value) and marker not in str(caught.value)

        clock = FakeClock()
        attempts.clear()
        with pytest.raises(AssertionError):
            ws.wait_until(unavailable, what="zero timeout", timeout_s=0,
                          clock=clock, sleep=clock.sleep)
        assert attempts == [0.0] and clock.sleeps == []

    observations = iter([(False, {"held": True}), ws.urllib.error.URLError(marker), ws.urllib.error.URLError(marker)])

    def loses_transport():
        value = next(observations)
        if isinstance(value, BaseException):
            raise value
        return value

    clock = FakeClock()
    with pytest.raises(AssertionError) as caught:
        ws.wait_until(loses_transport, what="last success", timeout_s=2, interval_s=1,
                      clock=clock, sleep=clock.sleep)
    assert "{'held': True}" in str(caught.value)
    assert "URLError" in str(caught.value) and marker not in str(caught.value)
    assert clock.now == 2 and clock.sleeps == [1, 1]


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


def test_quiescent_summary_reports_the_gate_counters(tmp_path, monkeypatch, capsys):
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

    monkeypatch.delenv("GITHUB_STEP_SUMMARY", raising=False)
    ws.write_step_summary(line)  # Local runs without Actions remain a no-op.
    summary_path = tmp_path / "summary.md"
    prefix = "# Earlier step\n\n"
    summary_path.write_text(prefix, encoding="utf-8")
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary_path))
    ws.write_step_summary(line)
    ws.write_step_summary(line)
    saved = summary_path.read_text(encoding="utf-8")
    assert saved.startswith(prefix)
    assert saved.count("### Quiescent efficiency counters") == 2
    for counter in ("presents=2", "submissions=2", "acquisitions=2", "runtime_wakeups=7", "settle_ms=5000", "interval_ms=60000"):
        assert saved.count(counter) == 2

    marker = "synthetic-sensitive-artifact-field"
    private_artifact = {**artifact, "psk": marker, "headers": marker, "request_url": marker}
    ws.write_step_summary(ws.quiescent_summary(private_artifact))
    assert marker not in summary_path.read_text(encoding="utf-8")
    ws.write_step_summary("presents=2\n# forged | `<markup>`")
    saved = summary_path.read_text(encoding="utf-8")
    assert "\n# forged" not in saved and "<markup>" not in saved
    assert "\\# forged \\| \\`&lt;markup&gt;\\`" in saved

    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(tmp_path))
    with pytest.raises(OSError):
        ws.write_step_summary(line)
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary_path))

    # Exercise the real quiescent call order with an emitted fixture artifact:
    # counters must be appended even when the unchanged checker rejects it.
    work = tmp_path / "quiescent"
    work.mkdir()
    (work / "quiescent-efficiency.json").write_text(ws.json.dumps(private_artifact), encoding="utf-8")
    proc_events = []
    publishes = []

    class FakeProcess:
        returncode = None

        def poll(self):
            return self.returncode

        def kill(self):
            proc_events.append("kill")
            self.returncode = 0

        def wait(self, timeout):
            proc_events.append(("wait", timeout))
            return self.returncode

    class FakeSmoke:
        def __init__(self, url, psk):
            pass

        def ok(self, tool, args=None):
            if tool == "hud_surfaces":
                return {"surfaces": [{"s": "zone:notification-area", "held": True}]}
            assert tool == "hud_publish"
            publishes.append(args["surface"])
            return {"ok": True}

    checked = []
    before = summary_path.read_text(encoding="utf-8")

    class RejectingChecker:
        def validate_artifact(self, actual, *, require_constrained):
            assert actual == private_artifact and require_constrained is False
            saved = summary_path.read_text(encoding="utf-8")
            assert saved.startswith(before) and saved != before
            assert "runtime_wakeups=7" in saved[len(before):] and marker not in saved
            checked.append(True)
            return {}, ["controlled invalid artifact"]

    monkeypatch.setattr(ws, "seed_config", lambda config, psk: config)
    monkeypatch.setattr(ws.secrets, "token_hex", lambda count: "synthetic-psk")
    monkeypatch.setattr(ws.tempfile, "mkdtemp", lambda **kwargs: str(work))
    monkeypatch.setattr(ws.subprocess, "Popen", lambda *args, **kwargs: FakeProcess())
    monkeypatch.setattr(ws, "Smoke", FakeSmoke)
    monkeypatch.setattr(ws, "wait_for_mcp", lambda *args: None)
    monkeypatch.setattr(ws, "load_efficiency_checker", lambda: RejectingChecker())
    args = ws.argparse.Namespace(exe=tmp_path / "hud.exe", config=tmp_path / "production.toml",
                                 mcp_port=9090, startup_timeout=90, quiescent_timeout=150)
    assert ws.run_quiescent(args) == 1
    assert checked == [True]
    assert publishes == ["zone:notification-area", "widget:main-gauge"]
    assert proc_events == ["kill", ("wait", 10)]
    assert marker not in capsys.readouterr().out
