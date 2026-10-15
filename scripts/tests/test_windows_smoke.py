#!/usr/bin/env python3
"""Offline Windows smoke checks: polling, ownership, timing and captured pixels."""

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


def test_delay_checks_held_ownership_and_captured_presentation(monkeypatch, subtests, capsys, tmp_path):
    """Exercise the real smoke/PNG path with one publish and an offline clock."""
    background = b"\x10\x20\x30\xff" * 400
    shown = b"\x90\xa0\xb0\xff" * 200 + background[800:]
    baseline_png = ws.encode_png_rgba(20, 20, background)
    decode = ws.decode_png_rgba

    class Response:
        def __init__(self, body, status=200, ctype="application/json"):
            self.body, self.status = body, status
            self.headers = {"Content-Type": ctype}

        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

        def read(self):
            return self.body

    class Endpoint:
        def __init__(self, clock, case):
            self.clock, self.case = clock, case
            self.held, self.due = True, None  # Earlier screenshot content.
            self.publishes, self.captures, self.clears = [], [], 0
            self.capture_bodies = []
            self.rpc_s, self.capture_s = (0.001, 0.0005) if case == "coarse monotonic" else (0.05, 0.05)

        def urlopen(self, request, timeout):
            if request.get_method() == "GET":
                assert not (self.diagnostics / "delay.json").exists()  # No I/O before the witness.
                assert request.full_url.endswith("/admin/screenshot") and timeout == 15
                started = self.clock()
                shot = len(self.captures) + 1
                visible = self.due is not None and started >= self.due
                pixels = shown if visible else background
                width, height, status, ctype = 20, 20, 200, "image/png"
                if self.case == "unstable baseline" and shot == 2:
                    pixels = b"\x11" + background[1:]
                if self.case == "premature" and shot == 3:
                    pixels = b"\x11" + background[1:]  # Even one pixel is early.
                if self.case == "missing" and shot == 4:
                    pixels = background
                if self.case == "too few pixels" and shot == 4:
                    pixels = shown[:796] + background[796:]
                if self.case == "native observed footprint" and shot == 4:
                    pixels = shown[:768] + background[768:]  # 192 still fails the 200-pixel guard.
                if self.case == "not restored" and shot == 5:
                    pixels = shown
                dimension_shots = {"baseline dimensions": 2, "early dimensions": 3,
                                   "late dimensions": 4, "clear dimensions": 5}
                if dimension_shots.get(self.case) == shot:
                    width, pixels = 21, background + background[:80]
                body = ws.encode_png_rgba(width, height, pixels)
                if shot == 3:
                    if self.case in ("401", "403", "429", "503"):
                        status = int(self.case)
                    elif self.case == "wrong content type":
                        ctype = "application/json"
                    elif self.case == "malformed PNG":
                        body = b"not a PNG"
                    elif self.case == "truncated PNG":
                        body = baseline_png[:-1]
                    elif self.case == "oversize PNG":
                        body = b"x" * (ws.MAX_PNG_BYTES + 1)
                    elif self.case == "transport":
                        raise ws.urllib.error.URLError("synthetic unavailable capture")
                self.clock.now += 3 if self.case == "cross-due capture" and shot == 3 else self.capture_s
                self.captures.append((started, self.clock()))
                self.capture_bodies.append(body)
                return Response(body, status, ctype)

            assert timeout == 10
            rpc = ws.json.loads(request.data)
            assert rpc["method"] == "tools/call"
            tool, args = rpc["params"]["name"], rpc["params"]["arguments"]
            if tool == "hud_surfaces":
                surface = {"s": "zone:subtitle", "accepts": "text"}
                if self.held and not (self.publishes and self.case == "not held"):
                    surface["held"] = True
                payload = {"surfaces": [] if self.publishes and self.case == "missing surface" else [surface]}
            elif tool == "hud_clear":
                self.clears += 1
                self.held = self.case == "clear failed" and bool(self.publishes)
                self.due = None
                payload = {"ok": True}
            else:
                assert tool == "hud_publish"
                assert args == {"surface": "zone:subtitle", "content": "CI DELAYED CONTENT IS VISIBLE", "delay_ms": 3000, "ttl_ms": 20000}
                self.held, self.due = True, self.clock() + 3
                sent = self.clock()
                self.clock.now += 4 if self.case == "slow acknowledgement" else self.rpc_s
                self.publishes.append((sent, self.clock()))
                payload = {"ok": self.case != "publish rejected", "expires_in_ms": 23000}
            self.clock.now += self.rpc_s if tool != "hud_publish" else 0
            result = {"content": [{"type": "text", "text": ws.json.dumps(payload)}]}
            return Response(ws.json.dumps({"result": result}).encode())

    def run(case, *, success):
        clock = FakeClock()
        if case == "coarse monotonic":
            clock.now = 0.010
        endpoint = Endpoint(clock, case)
        with monkeypatch.context() as patch:
            monotonic = (lambda: int(clock() * 64) / 64) if case == "coarse monotonic" else clock
            patch.setattr(ws.time, "monotonic", monotonic)
            patch.setattr(ws.time, "perf_counter", clock)
            patch.setattr(ws.time, "sleep", clock.sleep)
            patch.setattr(ws.urllib.request, "urlopen", endpoint.urlopen)
            if case == "slow decode":
                def slow_decode(body):
                    frame = decode(body)
                    clock.now += 1.5
                    return frame

                patch.setattr(ws, "decode_png_rgba", slow_decode)
            smoke = ws.Smoke("http://offline-smoke/", "synthetic-unused-key")
            for invocation in range(2 if case == "normal" else 1):
                endpoint.diagnostics = tmp_path / case / str(invocation)
                if success:
                    ws.check_delay(smoke, endpoint.diagnostics)
                    assert endpoint.held is False and endpoint.due is None
                    assert len(endpoint.publishes) == invocation + 1
                    captures = endpoint.captures[invocation * 5 :]
                    sent, acknowledged = endpoint.publishes[-1]
                    assert len(captures) == 5
                    assert captures[2][1] < sent + 3
                    assert captures[3][0] >= acknowledged + 3
                    assert len(clock.sleeps) == (invocation + 1) * 2  # Existing baseline and due sleeps only.
                else:
                    with pytest.raises(AssertionError):
                        ws.check_delay(smoke, endpoint.diagnostics)
                    baseline_failed = case in ("unstable baseline", "baseline dimensions")
                    assert len(endpoint.publishes) == (0 if baseline_failed else 1)
                    assert endpoint.clears == (1 if baseline_failed else 2)
                    assert len(endpoint.captures) <= 5  # No retry on invalid evidence.

                manifest = ws.json.loads((endpoint.diagnostics / "delay.json").read_text())
                assert manifest["outcome"] == ("success" if success else "failure")
                assert manifest["minimum_changed_pixels"] == 200
                saved = manifest["captures"]
                names = ["warm", "baseline", "before-due", "after-due", "restored"]
                for index, body in enumerate(endpoint.capture_bodies[invocation * 5:]):
                    capture = saved.get(names[index], {})
                    if "file" in capture:
                        assert (endpoint.diagnostics / capture["file"]).read_bytes() == body
                        assert capture["requested_at"] <= capture["responded_at"]
                if case == "native observed footprint":
                    assert manifest["changed_pixels"] == 192
                    assert sum(manifest["pixel_delta"]["max_channel_delta_counts"].values()) == 192
                    assert list(saved) == names[:4]  # Failed original witness; no replacement capture.
                if case in ("malformed PNG", "truncated PNG", "transport"):
                    assert "file" not in saved["before-due"]
                assert "synthetic-unused-key" not in (endpoint.diagnostics / "delay.json").read_text()

                output = capsys.readouterr()
                assert "synthetic-unused-key" not in output.out + output.err
                assert "CI DELAYED CONTENT IS VISIBLE" not in output.out + output.err
    run("coarse monotonic", success=True)  # Coarse request bounds can under-sleep the actual acknowledgement.
    run("normal", success=True)
    run("slow decode", success=True)  # Capture timing ends before PNG CPU work.
    for case in (
        "premature", "missing", "too few pixels", "native observed footprint", "unstable baseline", "not restored",
        "baseline dimensions", "early dimensions", "late dimensions", "clear dimensions",
        "cross-due capture", "slow acknowledgement", "not held",
        "missing surface", "publish rejected", "clear failed", "401", "403", "429", "503",
        "wrong content type", "malformed PNG", "truncated PNG", "oversize PNG", "transport",
    ):
        with subtests.test(case=case):
            run(case, success=False)

    # A failing artifact sink preserves the original failure and its count.
    with monkeypatch.context() as patch:
        def unavailable(directory, evidence):
            assert evidence["outcome"] == "failure" and evidence["changed_pixels"] == 192
            assert endpoint.clears == 2
            raise OSError("synthetic-sensitive-write-error")

        patch.setattr(ws, "write_delay_evidence", unavailable)
        clock = FakeClock()
        endpoint = Endpoint(clock, "native observed footprint")
        endpoint.diagnostics = tmp_path / "unavailable"
        patch.setattr(ws.time, "perf_counter", clock)
        patch.setattr(ws.time, "sleep", clock.sleep)
        patch.setattr(ws.urllib.request, "urlopen", endpoint.urlopen)
        with pytest.raises(AssertionError, match="192"):
            ws.check_delay(ws.Smoke("http://offline-smoke/", "synthetic-unused-key"), endpoint.diagnostics)
        assert "synthetic-sensitive-write-error" not in capsys.readouterr().err

    log = tmp_path / "hud.log"
    log.write_text('\n'.join([
        "synthetic-sensitive-unstructured-line",
        ws.json.dumps({"target": "other", "fields": {"event": "private"}}),
        ws.json.dumps({"target": "tze_hud_delay_diagnostic", "timestamp": "2026-10-15T00:00:00Z",
                       "fields": {"event": "zone_text_sample", "opacity": 0.01, "frame": 3,
                                  "content": "synthetic-sensitive-content", "psk": "synthetic-unused-key"}}),
    ]))
    ws.write_delay_trace(tmp_path, log)
    trace = ws.json.loads((tmp_path / "runtime-timing.json").read_text())
    assert trace == [{"timestamp": "2026-10-15T00:00:00Z",
                      "fields": {"event": "zone_text_sample", "opacity": 0.01, "frame": 3}}]


def test_parse_tile_count_finds_the_line_among_other_output():
    assert ws.parse_tile_count("connecting\ntiles 3\n") == 3
    for bad in ("", "tiles\n", "tiles x\n", "my tiles 2 more\n", "sessions 5\n"):
        with pytest.raises(AssertionError, match="no `tiles <n>` line"):
            ws.parse_tile_count(bad)


def test_borrowed_url_routes_only_authorized_publications_and_own_cleanup(monkeypatch, capsys):
    import json

    scripts = Path(__file__).resolve().parents[2] / ".claude/skills/user-test/scripts"
    monkeypatch.syspath_prepend(str(scripts))
    import hud_env

    trace, held, faults = [], set(), {}
    entries = [{"s": "zone:subtitle", "accepts": "text"},
               *[{"s": "widget:" + name} for name in ("main-gauge", "main-progress", "main-status")]]
    key = "synthetic-live-smoke-key"
    monkeypatch.setattr(hud_env, "load_psk", lambda selected: key if selected == "http://paired:9090/mcp" else pytest.fail("wrong pairing"))

    class Response:
        def __init__(self, result):
            self.result = result

        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def read(self):
            return json.dumps({"result": self.result}).encode()

    class Opener:
        def open(self, request, timeout):
            assert request.full_url == "http://paired:9090/mcp"
            assert request.get_method() == "POST" and request.get_header("Authorization") == "Bearer " + key
            body = json.loads(request.data)
            method, params = body["method"], body.get("params", {})
            if method == "initialize":
                trace.append("initialize")
                return Response({})
            if method == "tools/list":
                return Response({"tools": [{"name": name} for name in ws.TOOLS]})
            tool, args = params["name"], params["arguments"]
            surface = args.get("surface")
            trace.append((tool, surface))
            if tool == "hud_surfaces":
                payload = {"surfaces": [{**entry, **({"held": True} if entry["s"] in held else {})} for entry in entries]
                           + [{"s": name, "state": "attached"} for name in held if name.startswith("portal:")]}
            elif tool == "hud_input":
                payload = {"items": []}
            elif surface == "zone:no-such-zone":
                return Response({"isError": True, "content": [{"text": json.dumps({"code": "ZONE_NOT_FOUND", "hint": "unknown"})}]})
            elif tool == "hud_publish":
                held.add(surface)
                if faults.get("lost_reply"):
                    raise ws.urllib.error.URLError("synthetic-private-response-detail")
                payload = {"ok": True}
            elif tool == "hud_clear":
                assert surface in held  # No clearing a pre-existing/untracked surface.
                held.remove(surface)
                payload = {"ok": True}
            else:
                pytest.fail("unexpected tool")
            return Response({"content": [{"text": json.dumps(payload)}]})

    monkeypatch.setattr(ws.urllib.request, "build_opener", lambda handler: Opener())
    monkeypatch.setattr(ws.time, "sleep", lambda _: None)
    for name in ("seed_config", "wait_for_mcp", "check_pair", "check_restart", "check_tile_orphan", "run_quiescent", "kill_pid"):
        monkeypatch.setattr(ws, name, lambda *a, **k: pytest.fail("borrowed process/lifecycle mutation"))
    monkeypatch.setattr(ws.subprocess, "Popen", lambda *a, **k: pytest.fail("borrowed spawn"))
    checks = []
    for name in ("check_admin", "check_widget", "check_notification_ttl", "check_delay"):
        monkeypatch.setattr(ws, name, lambda smoke, name=name: checks.append(name))
    assert ws.main(["--url", "http://paired:9090/mcp"]) == 0
    assert checks == ["check_admin", "check_widget", "check_notification_ttl", "check_delay"]
    assert not held
    portals = [row[1] for row in trace if isinstance(row, tuple) and row[0] == "hud_publish" and row[1].startswith("portal:")]
    assert len(portals) == 1 and portals[0].startswith("portal:live-smoke-")
    assert {row[1] for row in trace if isinstance(row, tuple) and row[0] == "hud_clear"} == {"widget:main-gauge", "zone:subtitle", portals[0]}
    captured = capsys.readouterr()
    assert captured.out.count("UNEXECUTED owner-only:") == 6 and key not in captured.out + captured.err

    # A lost publish reply can still mean a mutation: clean its tracked surface.
    trace.clear()
    faults["lost_reply"] = True
    assert ws.main(["--url", "http://paired:9090/mcp"]) == 1
    assert not held
    assert [row for row in trace if isinstance(row, tuple) and row[0] == "hud_clear"] == [("hud_clear", "widget:main-gauge")]
    captured = capsys.readouterr()
    assert "synthetic-private-response-detail" not in captured.out + captured.err
    faults.clear()

    trace.clear()
    held.add("zone:subtitle")
    assert ws.main(["--url", "http://paired:9090/mcp"]) == 1
    assert held == {"zone:subtitle"} and trace == ["initialize", ("hud_surfaces", None)]
    assert "FAIL borrowed" in capsys.readouterr().err
    for argv in ([], ["--url", "http://paired", "--exe", "hud.exe"], ["--url", "http://paired", "--config", "x"], ["--exe", "hud.exe"]):
        with pytest.raises(SystemExit) as failed:
            ws.main(argv)
        assert failed.value.code == 2
    # Owned quiescent routing and its required config remain supported.
    monkeypatch.setattr(ws, "run_quiescent", lambda args: 17)
    assert ws.main(["--exe", "hud.exe", "--config", "production.toml", "--quiescent-with-content"]) == 17


def test_quiescent_summary_reports_the_gate_counters(tmp_path, monkeypatch, capsys, subtests):
    artifact = {
        "gpu": {"presents": 2, "queue_submissions": 2, "surface_acquisitions": 2},
        "wakeups": {"combined_runtime_driven": 7},
        "settling_duration_ms": 5000,
        "interval_duration_ms": 60000,
        "runtime": {"window_mode": "fullscreen"},
        "constrained_profile": {
            "logical_cpu_limit": 2,
            "cpu_limit_enforcement": "GetProcessAffinityMask:0x3",
        },
        "renderer": {"adapter": "Microsoft Basic Render Driver", "software": True, "backend": "DX12"},
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
    for counter in ("presents=2", "submissions=2", "acquisitions=2", "runtime\\_wakeups=7", "settle\\_ms=5000", "interval\\_ms=60000"):
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

    # Exercise the owned HUD path, including strict artifact qualification,
    # actual child completion and failure cleanup, without launching a process.
    work = tmp_path / "quiescent"
    work.mkdir()
    artifact_path = work / "quiescent-efficiency.json"
    proc_events = []
    publishes = []
    checked = []
    spawned = []
    admission = "quiescent-admission pid=4242 cpu_mask=0x3 logical_cpus=2 phase=before_runtime_init"
    runtime_log = f"{admission}\n{admission} private={marker}\n" + "." * 9000

    class FakeProcess:
        pid = 4242
        returncode = None
        exit_code = 0
        times_out = False

        def poll(self):
            return self.returncode

        def kill(self):
            proc_events.append("kill")
            self.returncode = -9

        def wait(self, timeout):
            proc_events.append(("wait", timeout))
            if self.times_out and self.returncode is None:
                raise ws.subprocess.TimeoutExpired("owned-hud", timeout)
            if self.returncode is None:
                self.returncode = self.exit_code
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

    class Checker:
        failures = []

        def validate_artifact(self, actual, *, require_constrained, required_window_mode=None):
            assert actual == private_artifact
            saved = summary_path.read_text(encoding="utf-8")
            assert saved.startswith(before) and saved != before
            assert "runtime\\_wakeups=7" in saved[len(before):] and marker not in saved
            checked.append((require_constrained, required_window_mode))
            return {}, self.failures

    monkeypatch.setattr(ws, "seed_config", lambda config, psk: config)
    monkeypatch.setattr(ws.secrets, "token_hex", lambda count: "synthetic-psk")
    monkeypatch.setattr(ws.tempfile, "mkdtemp", lambda **kwargs: str(work))
    monkeypatch.setattr(ws.subprocess, "Popen", lambda argv, **kwargs: (
        spawned.append(argv), kwargs["stdout"].write(runtime_log.encode()), proc
    )[-1])
    monkeypatch.setattr(ws, "Smoke", FakeSmoke)
    monkeypatch.setattr(ws, "wait_for_mcp", lambda *args: None)
    monkeypatch.setattr(ws, "load_efficiency_checker", lambda: checker)
    args = ws.argparse.Namespace(exe=tmp_path / "hud.exe", config=tmp_path / "production.toml",
                                 mcp_port=9090, startup_timeout=90, quiescent_timeout=150)
    for case, failures, exit_code, times_out, content, expected, events in [
        ("valid", [], 0, False, "json", 0, [("wait", 10)]),
        ("runtime_failed", [], 7, False, "json", 1, [("wait", 10)]),
        ("invalid_cpu4", ["logical_cpu_limit: expected 2, got 4"], 0, False,
         "json", 1, [("wait", 10)]),
        ("exit_timeout", [], 0, True, "json", 1, [("wait", 10), "kill", ("wait", 10)]),
        ("malformed", [], 0, False, "malformed", 1, ["kill", ("wait", 10)]),
        ("list_shape", [], 0, False, "list", 1, ["kill", ("wait", 10)]),
        ("gpu_shape", [], 0, False, "gpu_list", 1, ["kill", ("wait", 10)]),
        ("startup_failed", [], 0, False, "json", 1, ["kill", ("wait", 10)]),
        ("missing", [], 1, False, "missing", 1, []),
    ]:
        with subtests.test(case=case):
            proc = FakeProcess()
            proc.exit_code, proc.times_out = exit_code, times_out
            checker = Checker()
            checker.failures = failures
            proc_events.clear()
            publishes.clear()
            checked.clear()
            spawned.clear()
            before = summary_path.read_text(encoding="utf-8")
            if case == "startup_failed":
                monkeypatch.setattr(ws, "wait_for_mcp", lambda *args: ws.wait_until(
                    lambda: (False, None), what="startup", timeout_s=0,
                ))
            else:
                monkeypatch.setattr(ws, "wait_for_mcp", lambda *args: None)
            if content == "missing":
                artifact_path.unlink(missing_ok=True)
                proc.returncode = exit_code
            else:
                payload = {"json": ws.json.dumps(private_artifact), "list": "[]",
                           "gpu_list": ws.json.dumps({**private_artifact, "gpu": []}), "malformed": "{"}
                artifact_path.write_text(payload[content], encoding="utf-8")
            assert ws.run_quiescent(args) == expected
            assert checked == ([(True, "fullscreen")] if content == "json" and case != "startup_failed" else [])
            assert spawned[0][0] == str(args.exe)
            assert publishes == ([] if case == "startup_failed" else ["zone:notification-area", "widget:main-gauge"])
            assert proc_events == events
            assert proc.returncode is not None
            output = capsys.readouterr()
            assert marker not in output.out + output.err
            records = [ws.json.loads(line.removeprefix("quiescent-owned-process "))
                       for line in output.out.splitlines() if line.startswith("quiescent-owned-process ")]
            assert records, "run_quiescent must retain the original Popen PID and actual completion"
            waited = case in ("valid", "runtime_failed", "invalid_cpu4")
            assert records == [
                {"event": "started", "pid": proc.pid},
                *([{"event": "waited", "pid": proc.pid, "exit": exit_code, "reaped": True}] if waited else []),
                {"event": "cleanup", "pid": proc.pid, "kill_requested": "kill" in events,
                 "wait_completed": bool(events), "exit": proc.returncode, "reaped": bool(events)},
            ]
            assert [line for line in output.out.splitlines() if line.startswith("quiescent-admission ")] == [admission]
            if content == "json" and case != "startup_failed":
                for field in ("logical_cpus=2", "cpu_mask='GetProcessAffinityMask:0x3'",
                              "window_mode='fullscreen'", "software=True", "backend='DX12'"):
                    assert field in output.out
            if expected:
                assert "FAIL " in output.err
            if content in ("list", "gpu_list"):
                assert "expected object" in output.err
            if case == "startup_failed":
                assert "startup" in output.err
    missing_identity = ws.quiescent_summary({**artifact, "runtime": [], "constrained_profile": []})
    assert "logical_cpus=None" in missing_identity and "window_mode=None" in missing_identity
    assert marker not in capsys.readouterr().out



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
                assert failure.value.retryable is True  # Only read-only polling consumes this flag.
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

    for error in (urllib.error.URLError(marker + url), ConnectionError(marker), TimeoutError(marker)):
        with subtests.test(case=type(error).__name__ + " wrapped held probe recovers without republishing"):
            with monkeypatch.context() as patch:
                clock, output, calls, reads = FakeClock(), Output(), [], []
                patch.setattr(ws.sys, "stdout", output)
                patch.setattr(ws.time, "monotonic", clock)

                def held_response(req, timeout):
                    request = json.loads(req.data)
                    tool = request["params"]["name"]
                    calls.append((tool, timeout))
                    if tool == "hud_publish":
                        payload = {"ok": True}
                    else:
                        assert tool == "hud_surfaces"
                        reads.append(clock.now)
                        if len(reads) == 1:
                            raise error
                        payload = {"surfaces": [{"s": "zone:z", "held": True}]}
                    return Response(json.dumps({"result": {
                        "content": [{"text": json.dumps(payload)}], "isError": False,
                    }}).encode())

                patch.setattr(ws.urllib.request, "urlopen", held_response)
                smoke = ws.Smoke(url, marker)
                assert smoke.ok("hud_publish", {"surface": "zone:z", "content": marker}) == {"ok": True}
                observed, waited = ws.wait_until(ws.held_probe(smoke, "zone:z", True), what="held",
                                                clock=clock, sleep=clock.sleep)
                assert observed == {"s": "zone:z", "held": True} and waited == ws.POLL_S
                assert calls == [("hud_publish", 10), ("hud_surfaces", 10), ("hud_surfaces", 10)]
                assert reads == [0, ws.POLL_S] and clock.sleeps == [ws.POLL_S]
                assert marker not in output.getvalue() and url not in output.getvalue()

    errors = [
        *(urllib.error.HTTPError(url, code, marker, None, None) for code in (401, 403, 500)),
        AssertionError(marker),
        json.JSONDecodeError(marker, "", 0),
    ]
    for error in errors:
        for route in ("held probe", "startup"):
            with subtests.test(case=type(error).__name__ + " " + route + " fails without retry"):
                with monkeypatch.context() as patch:
                    clock, output, calls = FakeClock(), Output(), []
                    patch.setattr(ws.sys, "stdout", output)
                    patch.setattr(ws.time, "monotonic", clock)

                    def rejected_request(req, timeout):
                        calls.append(timeout)
                        raise error

                    patch.setattr(ws.urllib.request, "urlopen", rejected_request)
                    smoke = ws.Smoke(url, marker)
                    with pytest.raises(ws.RequestFailure) as failure:
                        if route == "held probe":
                            ws.wait_until(ws.held_probe(smoke, "zone:z", True), what="fail closed",
                                          clock=clock, sleep=clock.sleep)
                        else:
                            ws.wait_for_mcp(smoke, Child(), 90, clock=clock, sleep=clock.sleep)
                    assert failure.value.retryable is False
                    assert failure.value.transport_class == type(error).__name__
                    assert calls == [10] and clock.sleeps == [] and clock.now == 0
                    assert marker not in output.getvalue() + str(failure.value)
                    assert url not in output.getvalue() + str(failure.value)

    with subtests.test(case="wrapped held transport retains the guard and safe original class"):
        with monkeypatch.context() as patch:
            clock, output, calls = FakeClock(), Output(), []
            patch.setattr(ws.sys, "stdout", output)
            patch.setattr(ws.time, "monotonic", clock)

            def unavailable_request(req, timeout):
                calls.append(clock.now)
                raise urllib.error.URLError(marker + url)

            patch.setattr(ws.urllib.request, "urlopen", unavailable_request)
            with pytest.raises(AssertionError) as failure:
                ws.wait_until(ws.held_probe(ws.Smoke(url, marker), "zone:z", True), what="bounded held",
                              timeout_s=2.5, interval_s=1, clock=clock, sleep=clock.sleep)
            assert calls == [0, 1, 2, 2.5] and clock.sleeps == [1, 1, 0.5] and clock.now == 2.5
            assert "URLError" in str(failure.value) and "RequestFailure" not in str(failure.value)
            assert marker not in output.getvalue() + str(failure.value)
            assert url not in output.getvalue() + str(failure.value)


def test_capture_png_pixels_baseline_and_changed_only_comment(tmp_path, monkeypatch):
    """One offline behavior fixture: independent PNG bytes and a fake API sink."""
    import io
    import json
    import struct
    import zipfile
    import zlib

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    def png(rows, width=2, height=2):
        return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))

    # Independent precomputed filter vectors; no copy of the decoder recurrence.
    pixels = bytes([10, 20, 30, 255, 30, 40, 50, 255, 20, 30, 40, 255, 50, 60, 70, 255])
    vectors = [
        [0, 10, 20, 30, 255, 30, 40, 50, 255, 0, 20, 30, 40, 255, 50, 60, 70, 255],
        [1, 10, 20, 30, 255, 20, 20, 20, 0, 1, 20, 30, 40, 255, 30, 30, 30, 0],
        [2, 10, 20, 30, 255, 30, 40, 50, 255, 2, 10, 10, 10, 0, 20, 20, 20, 0],
        [3, 10, 20, 30, 255, 25, 30, 35, 128, 3, 15, 20, 25, 128, 25, 25, 25, 0],
        [4, 10, 20, 30, 255, 20, 20, 20, 0, 4, 10, 10, 10, 0, 20, 20, 20, 0],
    ]
    for rows in vectors:
        assert ws.decode_png_rgba(png(bytes(rows))) == (2, 2, pixels)
    valid = png(bytes(vectors[0]))
    invalid_filter = png(bytes([5, *vectors[0][1:]]))
    for bad in (valid[:-1], valid + b"trailing", invalid_filter,
                png(b"\0" * 19), png(b"", width=8192, height=8192),
                valid[:20] + bytes([valid[20] ^ 1]) + valid[21:]):
        with pytest.raises(AssertionError):
            ws.decode_png_rgba(bad)
    assert ws.decode_png_rgba(ws.encode_png_rgba(2, 2, pixels)) == (2, 2, pixels)

    metadata = {"theme": "classic", "fixture": "subtitle-multiline", "fixture_sha256": "input",
                "checkpoint": 1, "width": 2, "height": 2, "renderer": {"backend": "Dx12", "device_name": "WARP"},
                "window_mode": "fullscreen", "settle_s": 1}
    same = ws.compare_capture(metadata, metadata, valid, valid)
    assert same["status"] == "unchanged" and same["changed_pixels"] == same["tolerated_pixels"] == 0
    within = bytearray(pixels)
    within[0] += 6
    tolerated = ws.compare_capture(metadata, metadata, valid, ws.encode_png_rgba(2, 2, within))
    assert tolerated["status"] == "unchanged" and tolerated["tolerated_pixels"] == 1
    within[4] += 7
    within[11] -= 1  # Alpha has no tolerance, independent of RGB tolerance.
    changed = ws.compare_capture(metadata, metadata, valid, ws.encode_png_rgba(2, 2, within))
    assert changed["changed_pixels"] == 2 and changed["tolerated_pixels"] == 1
    assert ws.decode_png_rgba(changed["diff_png"])[2] == bytes([0, 0, 0, 255, 255, 0, 255, 255, 255, 0, 255, 255, 0, 0, 0, 255])
    assert ws.compare_capture(metadata, None, None, valid)["status"] == "unavailable"
    for key, value in (("width", 1), ("renderer", {"device_name": "hardware"}), ("fixture_sha256", "other")):
        assert ws.compare_capture(metadata, {**metadata, key: value}, valid, valid)["status"] == "unavailable"
    assert ws.compare_capture(metadata, metadata, ws.encode_png_rgba(1, 1, b"\0\0\0\xff"), valid)["status"] == "unavailable"

    comments, operations = [], []

    def sink(route, method="GET", body=None):
        operations.append((route, method))
        if method == "GET":
            return comments
        if method == "POST":
            comments.append({"id": 42, "user": {"login": "github-actions[bot]"}, **body})
        else:
            assert method == "PATCH" and route.endswith("/42")
            comments[0].update(body)
        return comments[0]

    body = ws.COMMENT_MARKER + "\nchanged scenes"
    assert ws.update_capture_comment(sink, "owner/repo", 7, body, False).startswith("not posted")
    assert operations == []
    assert ws.update_capture_comment(sink, "owner/repo", 7, body, True) == "created"
    assert ws.update_capture_comment(sink, "owner/repo", 7, body + " newer", True) == "updated"
    assert len(comments) == 1 and comments[0]["body"] == body + " newer"
    assert [method for _, method in operations] == ["GET", "POST", "GET", "PATCH"]

    # Drive actual artifact-selection/ZIP/provenance behavior with no GH process.
    main_run = {"id": 12, "head_branch": "main", "event": "push", "conclusion": "success", "head_sha": "main-sha", "path": ".github/workflows/windows.yml", "created_at": "2026-10-11T00:00:00Z"}
    baseline_manifest = {"schema": 1, "workflow": "windows.yml", "repo": "owner/repo", "event": "push",
                         "run_id": "12", "head": "main-sha", "capture_complete": True, "cells": {"cell": metadata}}
    archive_bytes = io.BytesIO()
    with zipfile.ZipFile(archive_bytes, "w") as archive:
        archive.writestr("manifest.json", json.dumps(baseline_manifest))
        archive.writestr("cell.png", valid)
    download_bytes = io.BytesIO()
    with zipfile.ZipFile(download_bytes, "w") as archive:
        archive.writestr("captures.zip", archive_bytes.getvalue())
    artifact = {"id": 99, "name": ws.CAPTURE_ARTIFACT, "expired": False, "size_in_bytes": len(download_bytes.getvalue()), "workflow_run": {"id": 12, "head_branch": "main"}, "created_at": "2026-10-11T00:01:00Z"}
    requests = []

    def api(route):
        requests.append(route)
        if "/actions/runs/" in route:
            return main_run
        assert "name=windows-captures&per_page=100" in route
        return {"artifacts": [artifact]}

    capabilities = {"help": "  --allow-escape-sequences  Allow binary output", "exit": 0}
    download_calls = []

    def download(command, **kwargs):
        download_calls.append(command)
        if command == ["gh", "api", "--help"]:
            assert kwargs == {"capture_output": True, "text": True, "timeout": 10}
            return ws.argparse.Namespace(returncode=capabilities["exit"], stdout=capabilities["help"])
        expected = ["gh", "api", "repos/owner/repo/actions/artifacts/99/zip"]
        if "--allow-escape-sequences" in capabilities["help"]:
            expected.append("--allow-escape-sequences")
        assert command == expected
        assert kwargs["stderr"] == ws.subprocess.DEVNULL and kwargs["timeout"] == 45
        kwargs["stdout"].write(download_bytes.getvalue())
        return ws.argparse.Namespace(returncode=0)

    monkeypatch.setattr(ws, "capture_github_api", api)
    monkeypatch.setattr(ws.subprocess, "run", download)
    manifest, archive, reason = ws.capture_baseline("owner/repo")
    assert reason == "available" and manifest["head"] == "main-sha" and manifest["baseline_artifact_id"] == 99
    with archive:
        assert archive.read("cell.png") == valid
    assert download_calls == [["gh", "api", "--help"],
                              ["gh", "api", "repos/owner/repo/actions/artifacts/99/zip", "--allow-escape-sequences"]]
    capabilities["help"] = "Legacy gh API help without binary-output guard"
    download_calls.clear()
    manifest, archive, reason = ws.capture_baseline("owner/repo")
    assert reason == "available" and manifest["head"] == "main-sha"
    with archive:
        assert archive.read("cell.png") == valid
    assert download_calls == [["gh", "api", "--help"], ["gh", "api", "repos/owner/repo/actions/artifacts/99/zip"]]
    capabilities["exit"] = 1
    download_calls.clear()
    assert ws.capture_baseline("owner/repo") == (None, None, "baseline download capability unavailable")
    assert download_calls == [["gh", "api", "--help"]]
    capabilities["exit"] = 0
    artifact["expired"] = True
    assert ws.capture_baseline("owner/repo")[0:2] == (None, None)
    artifact["expired"] = False
    monkeypatch.setattr(ws, "capture_github_api", lambda route: {"artifacts": []})
    assert ws.capture_baseline("owner/repo")[0:2] == (None, None)

    # Publisher preserves exact typed messages/key/TTL and every widget update;
    # the fake owns no HUD process and cleanup is limited to this fixture's surfaces.
    traced = []

    class FixtureSmoke:
        def ok(self, tool, args):
            traced.append((tool, args))
            return {"ok": True}

    fixture = [{"zone": "subtitle", "content": "hello", "ttl_ms": 10, "key": "k"},
               {"zone": "subtitle", "content": "second", "ttl_ms": 20}]
    ws.capture_fixture(FixtureSmoke(), fixture, lambda index: traced.append(("frame", index)))
    assert traced == [("hud_clear", {"surface": "zone:subtitle"}),
                      ("hud_publish", {"surface": "zone:subtitle", "content": "hello", "ttl_ms": 10, "key": "k"}),
                      ("hud_publish", {"surface": "zone:subtitle", "content": "second", "ttl_ms": 20}),
                      ("frame", 2), ("hud_clear", {"surface": "zone:subtitle"})]
    traced.clear()
    widgets = [{"widget": "main-status", "params": {"status": status}, "ttl_ms": 60000} for status in ("online", "away")]
    ws.capture_fixture(FixtureSmoke(), widgets, lambda index: traced.append(("frame", index)))
    assert [args for tool, args in traced if tool == "frame"] == [1, 2]
    assert [args["params"] for tool, args in traced if tool == "hud_publish"] == [{"status": "online"}, {"status": "away"}]
