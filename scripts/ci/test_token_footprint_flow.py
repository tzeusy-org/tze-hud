#!/usr/bin/env python3
"""Framing contract tests for the canonical token-footprint driver."""

import importlib.util
import io
import json
import pathlib
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "examples/benchmark/token_footprint_flow.py"
SPEC = importlib.util.spec_from_file_location("token_footprint_flow", SCRIPT)
flow = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(flow)


def fake_rpc_factory(calls):
    def fake_rpc(method, params):
        calls.append((method, params))
        if method == "tools/list":
            result = {"tools": []}
        else:
            text = {"ok": True}
            if params["name"] == "hud_input" and "ack" not in params["arguments"]:
                if any(len(p.get("arguments", {}).get("content", {}).get("actions", [])) == 4
                       for _, p in calls if isinstance(p.get("arguments", {}).get("content"), dict)):
                    text = {"items": [{"id": "i2", "s": "zone:notification-area", "action": "c"}], "remaining": 0}
                else:
                    text = {"items": [{"id": "i1", "s": flow.PORTAL, "text": "hi"}], "remaining": 0}
            if params["arguments"].get("surface") == "zone:subtitles":
                text = {"code": "ZONE_NOT_FOUND", "hint": "call hud_surfaces"}
                result = {"content": [{"type": "text", "text": json.dumps(text)}], "isError": True}
                return None, json.dumps({"method": method, "params": params}), "{}", {"result": result}
            content = params["arguments"].get("content")
            if isinstance(content, dict) and len(content.get("actions", [])) > 4:
                text = {"code": "INVALID_ARGUMENT", "hint": "notification allows at most 4 actions"}
                result = {"content": [{"type": "text", "text": json.dumps(text)}], "isError": True}
                return None, json.dumps({"method": method, "params": params}), "{}", {"result": result}
            result = {"content": [{"type": "text", "text": json.dumps(text)}]}
        return None, json.dumps({"method": method, "params": params}), "{}", {"result": result}

    return fake_rpc


class CanonicalFlowTests(unittest.TestCase):
    def setUp(self):
        flow.transactions.clear()
        environment = mock.patch.dict(flow.os.environ, {"HUD_MAX_NOTIFICATION_ACTIONS": "4"})
        environment.start()
        self.addCleanup(environment.stop)
        readiness = mock.patch("sys.stdin", io.StringIO("notification-choice-ready\n"))
        readiness.start()
        self.addCleanup(readiness.stop)

    def test_every_call_is_standard_mcp(self):
        calls = []
        with mock.patch.object(flow, "rpc", fake_rpc_factory(calls)), mock.patch(
            "sys.stdout"
        ):
            flow.main()
        self.assertEqual(calls[0][0], "tools/list")
        self.assertTrue(all(method == "tools/call" for method, _ in calls[1:]))
        names = {params["name"] for _, params in calls[1:]}
        self.assertTrue(names <= {"hud_surfaces", "hud_publish", "hud_hold", "hud_clear", "hud_input"})

    def test_flows_and_operations_are_labelled(self):
        output = io.StringIO()
        calls = []
        with mock.patch.object(flow, "rpc", fake_rpc_factory(calls)), mock.patch("sys.stdout", output):
            flow.main()
        labels = [(t["flow"], t["operation"]) for t in flow.transactions]
        self.assertEqual(
            labels,
            [
                ("tools_list", "tools/list"),
                ("discover", "hud_surfaces"),
                ("zone_publish", "hud_publish"),
                ("widget_publish", "hud_publish"),
                ("portal", "1_publish_attach"),
                ("portal", "2_input_poll"),
                ("portal", "3_input_ack"),
                ("portal", "4_clear"),
                ("error", "hud_publish"),
                ("notification_choice", "1_ask"),
                ("notification_choice", "2_answer"),
                ("notification_choice", "3_ack"),
            ],
        )
        # Synthetic framing fixture only; real token evidence comes from the
        # runtime-backed calibration, never this mocked response.
        candidate = json.loads(output.getvalue())["notification_action_limit_error"]
        self.assertEqual(candidate["flow"], "notification_action_limit_error")
        self.assertEqual(len(calls[-1][1]["arguments"]["content"]["actions"]), 5)

    def test_portal_ack_uses_the_polled_id(self):
        calls = []
        with mock.patch.object(flow, "rpc", fake_rpc_factory(calls)), mock.patch("sys.stdout"):
            flow.main()
        ack = [p for m, p in calls if m == "tools/call" and "ack" in p["arguments"]]
        self.assertEqual([call["arguments"]["ack"] for call in ack], [["i1"], ["i2"]])

    def test_choice_poll_requires_shared_activation_readiness(self):
        for readiness in ("notification-choice-ready\n", "", "unexpected\n"):
            with self.subTest(readiness=readiness):
                flow.transactions.clear()
                calls = []
                with mock.patch.object(flow, "rpc", fake_rpc_factory(calls)), mock.patch(
                    "sys.stdout"
                ), mock.patch("sys.stdin", io.StringIO(readiness)):
                    if readiness == "notification-choice-ready\n":
                        flow.main()
                        answer = next(t for t in flow.transactions
                                      if t["flow"] == "notification_choice" and t["operation"] == "2_answer")
                        self.assertEqual(json.loads(answer["request_body"])["params"]["arguments"], {})
                    else:
                        with self.assertRaises(RuntimeError):
                            flow.main()
                        self.assertFalse(any(t["flow"] == "notification_choice" and t["operation"] == "2_answer"
                                             for t in flow.transactions))

    def test_unexpected_tool_error_raises(self):
        def failing_rpc(method, params):
            result = {"content": [{"type": "text", "text": "{}"}], "isError": True}
            return None, "{}", "{}", {"result": result}

        with mock.patch.object(flow, "rpc", failing_rpc):
            with self.assertRaises(RuntimeError):
                flow.tool("zone_publish", "hud_publish", "hud_publish", {})


if __name__ == "__main__":
    unittest.main()
