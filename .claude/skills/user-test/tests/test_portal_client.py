"""Unit tests for the hud-projection portal client (no network)."""

import importlib.util
import json
from pathlib import Path
from unittest import mock

import pytest

ROOT = Path(__file__).resolve().parents[4]
SPEC = importlib.util.spec_from_file_location(
    "portal_client", ROOT / ".claude/skills/hud-projection/scripts/portal_client.py"
)
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)


def tool_response(value, is_error=False):
    result = {"content": [{"type": "text", "text": json.dumps(value)}]}
    if is_error:
        result["isError"] = True
    return {"jsonrpc": "2.0", "id": 1, "result": result}


class FakeServer:
    def __init__(self, replies):
        self.calls = []
        self.replies = list(replies)

    def __call__(self, method, params, request_id=1):
        self.calls.append((method, params))
        return self.replies.pop(0)


def run(argv, replies):
    server = FakeServer(replies)
    with mock.patch.object(client, "rpc", server):
        client.main(argv)
    return server.calls


def test_publish_attaches_through_hud_publish(capsys):
    calls = run(
        ["publish", "--id", "s1", "--text", "hi", "--expects-reply", "--state", "active"],
        [tool_response({"ok": True})],
    )
    assert calls == [
        (
            "tools/call",
            {
                "name": "hud_publish",
                "arguments": {
                    "surface": "portal:s1",
                    "content": "hi",
                    "expects_reply": True,
                    "status": "active",
                },
            },
        )
    ]
    assert json.loads(capsys.readouterr().out) == {"ok": True}


def test_poll_prints_ndjson_and_acks_on_next_call(capsys):
    item = {"id": "i1", "s": "portal:s1", "text": "yes"}
    calls = run(
        ["poll", "--wait-ms", "10", "--ack"],
        [tool_response({"items": [item], "remaining": 0}), tool_response({"items": [], "remaining": 0})],
    )
    assert [c[1]["name"] for c in calls] == ["hud_input", "hud_input"]
    assert calls[1][1]["arguments"] == {"ack": ["i1"]}
    assert json.loads(capsys.readouterr().out.strip()) == item


def test_poll_without_input_exits_3():
    with pytest.raises(SystemExit) as exit_info:
        run(["poll", "--wait-ms", "10"], [tool_response({"items": [], "remaining": 0})])
    assert exit_info.value.code == 3


def test_tool_error_prints_code_and_hint(capsys):
    with pytest.raises(SystemExit) as exit_info:
        run(
            ["clear", "--id", "s1"],
            [tool_response({"code": "NOT_HELD", "hint": "hud_publish first"}, is_error=True)],
        )
    assert exit_info.value.code == 1
    assert json.loads(capsys.readouterr().out) == {"code": "NOT_HELD", "hint": "hud_publish first"}
