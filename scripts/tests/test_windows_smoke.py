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
            assert "runtime\\_wakeups=7" in saved[len(before):] and marker not in saved
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
