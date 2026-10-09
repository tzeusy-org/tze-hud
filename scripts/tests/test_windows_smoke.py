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


def test_request_boundaries_and_startup_readiness(monkeypatch, subtests):
    import io
    import json
    import urllib.error

    # Synthetic markers exercise redaction; no real credential or endpoint is used.
    marker = "private-marker"
    url = "http://private.invalid/private-url/"

    class Output(io.StringIO):
        def __init__(self):
            super().__init__()
            self.flushes = 0

        def flush(self):
            self.flushes += 1

    class Response(io.BytesIO):
        status = 200
        headers = {"Content-Type": "application/json"}

    class Child:
        returncode = None

        def poll(self):
            return self.returncode

    def result(req):
        request = json.loads(req.data)
        payload = {} if request["method"] == "initialize" else {
            "content": [{"text": json.dumps({"surfaces": []})}], "isError": False,
        }
        return Response(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": payload}).encode())

    with subtests.test(case="successful RPC keeps wire data and flushes safe timing"):
        with monkeypatch.context() as patch:
            clock, output, calls = FakeClock(), Output(), []
            patch.setattr(ws.sys, "stdout", output)
            patch.setattr(ws.time, "monotonic", clock)

            def open_rpc(req, timeout):
                calls.append((json.loads(req.data), req.get_header("Authorization"), timeout))
                clock.now += 0.25
                return Response(b'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}')

            patch.setattr(ws.urllib.request, "urlopen", open_rpc)
            smoke = ws.Smoke(url, marker)
            assert smoke.rpc("tools/call", {"name": "hud_publish", "arguments": {"content": marker}}) == {"ok": True}
            assert calls == [({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "hud_publish", "arguments": {"content": marker},
            }}, "Bearer " + marker, 10)]
            assert output.flushes == 2
            assert output.getvalue().splitlines() == [
                "request begin rpc id=1 method=tools/call tool=hud_publish",
                "request end rpc id=1 method=tools/call tool=hud_publish elapsed_s=0.250",
            ]
            assert marker not in output.getvalue() and url not in output.getvalue()

    for route in ("rpc mutation", "operator mutation", "pair mutation"):
        with subtests.test(case=route + " timeout dispatches exactly once"):
            with monkeypatch.context() as patch:
                output, calls = Output(), []
                patch.setattr(ws.sys, "stdout", output)

                def timeout_request(req, timeout):
                    calls.append(timeout)
                    raise TimeoutError(marker + " " + url)

                patch.setattr(ws.urllib.request, "urlopen", timeout_request)
                smoke = ws.Smoke(url, marker)
                with pytest.raises(ws.RequestFailure) as failure:
                    if route == "rpc mutation":
                        smoke.ok("hud_publish", {"content": marker})
                    elif route == "operator mutation":
                        smoke.request("POST", "/admin/restart?credential=" + marker)
                    else:
                        ws.pair_post(smoke, marker)
                assert calls == [15 if route == "operator mutation" else 10]
                assert failure.value.retryable is True  # Only startup discovery consumes this flag.
                assert "TimeoutError" in output.getvalue() and "request failure" in output.getvalue()
                assert "request end" not in output.getvalue()
                assert marker not in output.getvalue() + str(failure.value)
                assert url not in output.getvalue() + str(failure.value)
                assert "credential=" not in output.getvalue()

    with subtests.test(case="operator result is preserved and query is excluded"):
        with monkeypatch.context() as patch:
            output, calls = Output(), []
            patch.setattr(ws.sys, "stdout", output)

            def operator(req, timeout):
                calls.append((req.full_url, req.get_method(), timeout))
                return Response(b"operator result")

            patch.setattr(ws.urllib.request, "urlopen", operator)
            smoke = ws.Smoke(url, marker)
            assert smoke.request("GET", "/admin/status?credential=" + marker) == (
                200, "application/json", b"operator result",
            )
            assert calls == [(url.rsplit("/", 1)[0] + "/admin/status?credential=" + marker, "GET", 15)]
            assert "operator method=GET path=/admin/status" in output.getvalue()
            assert marker not in output.getvalue() and "credential=" not in output.getvalue()

    with subtests.test(case="protocol errors fail closed without exposing their text"):
        with monkeypatch.context() as patch:
            output, calls = Output(), []
            patch.setattr(ws.sys, "stdout", output)

            def protocol_error(req, timeout):
                calls.append(timeout)
                return Response(json.dumps({"error": {"message": marker + url}}).encode())

            patch.setattr(ws.urllib.request, "urlopen", protocol_error)
            with pytest.raises(ws.RequestFailure) as failure:
                ws.Smoke(url, marker).rpc("initialize")
            assert failure.value.retryable is False and calls == [10]
            assert marker not in output.getvalue() + str(failure.value)
            assert url not in output.getvalue() + str(failure.value)

    for case, discovery_at, ready in (("before", 0.99, True), ("at", 1.0, False), ("after", 1.01, False)):
        with subtests.test(case=case + " startup deadline"):
            with monkeypatch.context() as patch:
                clock, calls = FakeClock(), []
                patch.setattr(ws.time, "monotonic", clock)

                def readiness(req, timeout):
                    payload = json.loads(req.data)
                    calls.append((payload["method"], payload.get("params"), timeout))
                    clock.now = 0.2 if len(calls) == 1 else discovery_at
                    return result(req)

                patch.setattr(ws.urllib.request, "urlopen", readiness)
                smoke = ws.Smoke(url, marker)
                if ready:
                    ws.wait_for_mcp(smoke, Child(), 1, clock=clock, sleep=clock.sleep)
                else:
                    with pytest.raises(AssertionError, match="metadata and scene discovery not ready"):
                        ws.wait_for_mcp(smoke, Child(), 1, clock=clock, sleep=clock.sleep)
                assert [c[0] for c in calls] == ["initialize", "tools/call"]
                assert calls[1][1] == {"name": "hud_surfaces", "arguments": {}}
                assert [c[2] for c in calls] == [1, pytest.approx(0.8)]
                assert clock.sleeps == []

    with subtests.test(case="initialize at deadline does not declare ready or dispatch discovery"):
        with monkeypatch.context() as patch:
            clock, calls = FakeClock(), []
            patch.setattr(ws.time, "monotonic", clock)

            def metadata_at_deadline(req, timeout):
                calls.append(timeout)
                clock.now = 1
                return result(req)

            patch.setattr(ws.urllib.request, "urlopen", metadata_at_deadline)
            with pytest.raises(AssertionError, match="metadata and scene discovery not ready"):
                ws.wait_for_mcp(ws.Smoke(url, marker), Child(), 1, clock=clock, sleep=clock.sleep)
            assert calls == [1] and clock.sleeps == []

    with subtests.test(case="transient scene failure retries only startup discovery"):
        with monkeypatch.context() as patch:
            clock, calls = FakeClock(), []
            patch.setattr(ws.time, "monotonic", clock)

            def transient(req, timeout):
                calls.append((json.loads(req.data)["method"], timeout))
                clock.now += 0.1
                if len(calls) == 2:
                    raise urllib.error.URLError(marker + url)
                return result(req)

            patch.setattr(ws.urllib.request, "urlopen", transient)
            smoke = ws.Smoke(url, marker)
            ws.wait_for_mcp(smoke, Child(), 90, clock=clock, sleep=clock.sleep)
            assert calls == [("initialize", 10), ("tools/call", 10), ("initialize", 10), ("tools/call", 10)]
            assert clock.sleeps == [0.5]
            # The same failure after readiness propagates after one request.
            def post_ready(req, timeout):
                calls.append((json.loads(req.data)["method"], timeout))
                raise urllib.error.URLError(marker + url)

            patch.setattr(ws.urllib.request, "urlopen", post_ready)
            with pytest.raises(ws.RequestFailure):
                smoke.ok("hud_surfaces")
            assert len(calls) == 5 and clock.sleeps == [0.5]

    with subtests.test(case="last transient startup failure sleeps only the remaining guard"):
        with monkeypatch.context() as patch:
            clock, calls = FakeClock(), []
            patch.setattr(ws.time, "monotonic", clock)

            def last_failure(req, timeout):
                calls.append(timeout)
                clock.now = 0.9
                raise ConnectionError(marker)

            patch.setattr(ws.urllib.request, "urlopen", last_failure)
            with pytest.raises(AssertionError, match="metadata and scene discovery not ready"):
                ws.wait_for_mcp(ws.Smoke(url, marker), Child(), 1, clock=clock, sleep=clock.sleep)
            assert calls == [1] and clock.sleeps == [pytest.approx(0.1)] and clock.now == 1

    for during_request in (False, True):
        with subtests.test(case="child exit during request" if during_request else "child exited before request"):
            with monkeypatch.context() as patch:
                clock, child, calls = FakeClock(), Child(), []
                child.returncode = None if during_request else 17
                patch.setattr(ws.time, "monotonic", clock)

                def exited(req, timeout):
                    calls.append(timeout)
                    child.returncode = 17
                    return result(req)

                patch.setattr(ws.urllib.request, "urlopen", exited)
                with pytest.raises(AssertionError, match="exited during startup.*17"):
                    ws.wait_for_mcp(ws.Smoke(url, marker), child, 90, clock=clock, sleep=clock.sleep)
                assert calls == ([10] if during_request else []) and clock.sleeps == []
