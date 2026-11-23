"""CLI fallback sends the documented MCP verbs without implicit input ACKs."""

import http.client
import importlib.util
import io
import json
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import Mock

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "th-hud-publish/scripts/publish.py"
spec = importlib.util.spec_from_file_location("hud_publish_cli", SCRIPT)
publish = importlib.util.module_from_spec(spec)
spec.loader.exec_module(publish)


@pytest.fixture
def wire(monkeypatch):
    """Exercise JSON-RPC encoding/decoding; never contact the paired owner's HUD."""
    resolve = Mock(return_value=("http://fixture:9090/mcp", "fixture-key"))
    monkeypatch.setattr(publish.hud_env, "resolve", resolve)
    requests = []
    replies = []

    def urlopen(request, timeout):
        assert request.get_header("Authorization") == "Bearer fixture-key"
        body = json.loads(request.data)
        assert body["jsonrpc"] == "2.0"
        assert body["method"] == "tools/call"
        # Question waits fit 65 s. Cleanup may use the shorter clear timeout.
        assert timeout in ((20, 65) if body["params"]["name"] == "hud_clear" else (65,))
        requests.append(body["params"])
        reply = replies.pop(0) if replies else {"ok": True}
        if isinstance(reply, BaseException):
            raise reply
        envelope = {
            "jsonrpc": "2.0",
            "id": body["id"],
            "result": {
                "content": [{"type": "text", "text": json.dumps(reply)}],
                "isError": "code" in reply,
            },
        }
        return io.BytesIO(json.dumps(envelope).encode())

    monkeypatch.setattr(urllib.request, "urlopen", urlopen)
    return requests, replies, resolve


@pytest.mark.parametrize(
    "flags,tool,arguments",
    [
        (
            [
                "--zone",
                "notification-area",
                "--content",
                '{"body":"Passed"}',
                "--key",
                "session:tests",
                "--ttl-ms",
                "0",
                "--delay-ms",
                "250",
            ],
            "hud_publish",
            {
                "surface": "zone:notification-area",
                "content": {"body": "Passed"},
                "key": "session:tests",
                "ttl_ms": 0,
                "delay_ms": 250,
            },
        ),
        (
            [
                "--surface",
                "widget:allocated",
                "--params",
                '{"progress":0.5}',
                "--ttl-ms",
                "10000",
            ],
            "hud_publish",
            {
                "surface": "widget:allocated",
                "params": {"progress": 0.5},
                "ttl_ms": 10000,
            },
        ),
        (
            ["--zone", "notification-area", "--clear", "--key", "session:tests"],
            "hud_clear",
            {"surface": "zone:notification-area", "key": "session:tests"},
        ),
        (
            ["--surface", "widget:allocated", "--hold", "--ttl-ms", "10000"],
            "hud_hold",
            {"surface": "widget:allocated", "ttl_ms": 10000},
        ),
        (
            ["--zone", "notification-area", "--content", '{"body":"Ship?","actions":[{"label":"Yes","callback_id":"yes"}]}', "--wait-ms", "55000"],
            "hud_publish",
            {"surface":"zone:notification-area", "content":{"body":"Ship?","actions":[{"label":"Yes","callback_id":"yes"}]}, "wait_ms":55000},
        ),
        (
            ["--input", "--wait-ms", "55000", "--max-items", "20"],
            "hud_input",
            {"wait_ms": 55000, "max_items": 20},
        ),
        (
            ["--input", "--ack", "handled-1", "--ack", "handled-2"],
            "hud_input",
            {"ack": ["handled-1", "handled-2"]},
        ),
    ],
)
def test_cli_sends_exact_lifecycle_arguments(wire, capsys, flags, tool, arguments):
    requests, _, resolve = wire
    assert publish.main(["--host", "selected:9191", *flags]) == 0
    resolve.assert_called_once_with("selected:9191", None)
    assert requests == [{"name": tool, "arguments": arguments}]
    assert "fixture-key" not in capsys.readouterr().out


def test_discovery_error_stops_before_publish(wire, capsys):
    requests, replies, _ = wire
    replies.append({"code": "NOT_ALLOWED", "hint": "fixture refusal"})
    assert (
        publish.main(["--list-surfaces", "--zone", "subtitle", "--content", "hello"])
        == 1
    )
    assert requests == [{"name": "hud_surfaces", "arguments": {}}]
    assert json.loads(capsys.readouterr().out)["error"]["code"] == "NOT_ALLOWED"


def test_legacy_and_widget_batch_reports_partial_failure_without_replay(
    wire, tmp_path, capsys
):
    requests, replies, _ = wire
    replies.extend(
        [{"ok": True}, {"code": "WIDGET_PARAMETER_INVALID", "hint": "fixture"}]
    )
    path = tmp_path / "batch.json"
    path.write_text(
        json.dumps(
            [
                {"zone": "subtitle", "content": "legacy", "ttl_ms": 2000},
                {"widget": "allocated", "params": {"progress": 0.5}},
            ]
        )
    )
    assert publish.main(["--messages-file", str(path)]) == 1
    assert requests == [
        {
            "name": "hud_publish",
            "arguments": {
                "surface": "zone:subtitle",
                "content": "legacy",
                "ttl_ms": 2000,
            },
        },
        {
            "name": "hud_publish",
            "arguments": {"surface": "widget:allocated", "params": {"progress": 0.5}},
        },
    ]
    assert len(json.loads(capsys.readouterr().out)["published"]) == 2


@pytest.mark.parametrize("entries", [[], [{"s": "widget:allocated", "held": True}]])
def test_cleanup_refuses_undiscovered_or_held_widget(wire, entries):
    requests, replies, _ = wire
    replies.append({"surfaces": entries})
    assert publish.main([
        "--surface", "widget:allocated", "--params", '{"progress":0.5}', "--cleanup-on-exit",
    ]) == 5
    assert requests == [{"name": "hud_surfaces", "arguments": {}}]


@pytest.mark.parametrize("failure,exit_code", [("none", 1), ("http", 3), ("interrupt", 130)])
def test_cleanup_clears_only_accepted_or_uncertain_widgets(wire, tmp_path, failure, exit_code):
    requests, replies, _ = wire
    names = ["accepted", "refused", "maybe", "later"]
    replies.extend([
        {"surfaces": [{"s": f"widget:{name}"} for name in names]},
        {"ok": True},
        {"ok": True},
        {"code": "WIDGET_PARAMETER_INVALID", "hint": "fixture"},
    ])
    if failure == "http":
        replies.append(urllib.error.HTTPError("http://fixture/mcp", 503, "", {}, None))
    elif failure == "interrupt":
        replies.append(KeyboardInterrupt())
    path = tmp_path / "batch.json"
    path.write_text(json.dumps([
        {"zone": "subtitle", "content": "leave until TTL", "ttl_ms": 10000},
        *[{"widget": name, "params": {"progress": 0.5}} for name in names],
    ]))

    assert publish.main(["--messages-file", str(path), "--cleanup-on-exit"]) == exit_code
    published = ["zone:subtitle", "widget:accepted", "widget:refused", "widget:maybe"]
    cleared = ["widget:accepted", "widget:maybe"]
    if failure == "none":
        published.append("widget:later")
        cleared.append("widget:later")
    assert requests[0] == {"name": "hud_surfaces", "arguments": {}}
    assert [(r["name"], r["arguments"]["surface"]) for r in requests[1:]] == [
        *[("hud_publish", surface) for surface in published],
        *[("hud_clear", surface) for surface in cleared],
    ]


def test_invalid_later_batch_item_prevents_all_network_work(wire, tmp_path):
    requests, _, resolve = wire
    path = tmp_path / "batch.json"
    path.write_text(
        json.dumps(
            [
                {"zone": "subtitle", "content": "must not be sent"},
                {"surface": "widget:allocated", "params": {}, "delay_ms": 10},
            ]
        )
    )
    assert publish.main(["--messages-file", str(path)]) == 5
    assert requests == []
    resolve.assert_not_called()


@pytest.mark.parametrize(
    "flags",
    [
        ["--zone", "subtitle", "--clear", "--ttl-ms", "100"],
        ["--zone", "subtitle", "--hold"],
        ["--zone", "subtitle", "--hold", "--ttl-ms", "100", "--key", "ignored"],
        ["--input", "--wait-ms", "55001"],
        ["--input", "--max-items", "0"],
        ["--list-surfaces", "--ack", "unowned"],
        ["--surface", "widget:allocated", "--clear", "--key", "unsupported"],
    ],
)
def test_incompatible_flags_never_reach_network(wire, flags):
    requests, _, resolve = wire
    with pytest.raises(SystemExit) as error:
        publish.main(flags)
    assert error.value.code == 2
    assert requests == []
    resolve.assert_not_called()


@pytest.mark.parametrize(
    "flags",
    [
        ["--zone", "subtitle", "--content", '{"body": broken}'],
        ["--surface", "widget:allocated", "--params", "[]"],
        ["--surface", "widget:allocated", "--params", '{"progress":NaN}'],
        ["--surface", "widget:allocated", "--params", "{}", "--delay-ms", "1"],
        ["--zone", "subtitle", "--params", "{}"],
        ["--zone", "notification-area", "--content", '{"body":"No actions"}', "--wait-ms", "0"],
        ["--surface", "widget:allocated", "--params", "{}", "--wait-ms", "0"],
    ],
)
def test_invalid_payload_never_reaches_network(wire, flags):
    requests, _, resolve = wire
    assert publish.main(flags) == 5
    assert requests == []
    resolve.assert_not_called()


def test_http_failure_is_not_replayed_or_echoed(wire, monkeypatch, capsys):
    requests, _, _ = wire
    fail = Mock(
        side_effect=urllib.error.HTTPError(
            "http://fixture/mcp", 503, "fixture-key", {}, io.BytesIO(b"fixture-key")
        )
    )
    monkeypatch.setattr(urllib.request, "urlopen", fail)
    assert publish.main(["--zone", "subtitle", "--content", "hello"]) == 3
    assert fail.call_count == 1
    assert requests == []
    assert "fixture-key" not in capsys.readouterr().err


@pytest.mark.parametrize("failure,exit_code", [("http", 3), ("truncated", 4)])
def test_batch_transport_failure_preserves_receipts_and_stops(
    wire, monkeypatch, tmp_path, capsys, failure, exit_code
):
    requests, _, _ = wire
    original = urllib.request.urlopen
    calls = []

    def interrupted(request, timeout):
        calls.append(request)
        if len(calls) == 2:
            if failure == "truncated":
                raise http.client.IncompleteRead(b"fixture-key", 10)
            raise urllib.error.HTTPError(
                "http://fixture/mcp", 503, "unavailable", {}, io.BytesIO()
            )
        return original(request, timeout)

    monkeypatch.setattr(urllib.request, "urlopen", interrupted)
    path = tmp_path / "batch.json"
    path.write_text(
        json.dumps(
            [
                {"zone": "subtitle", "content": "accepted"},
                {"zone": "notification-area", "content": {"body": "uncertain"}},
                {"zone": "subtitle", "content": "must not be sent"},
            ]
        )
    )
    assert publish.main(["--messages-file", str(path)]) == exit_code
    assert len(calls) == 2
    assert len(requests) == 1
    output = capsys.readouterr()
    receipt = json.loads(output.out)
    assert receipt["published"] == [
        {
            "surface": "zone:subtitle",
            "response": {"result": {"ok": True}},
        }
    ]
    assert receipt["interrupted"] == {
        "surface": "zone:notification-area",
        "outcome": "unknown",
    }
    if failure == "http":
        assert json.loads(output.err)["status"] == 503
    else:
        assert json.loads(output.err)["kind"] == "IncompleteRead"
    assert "fixture-key" not in output.out + output.err
