#!/usr/bin/env python3
"""Framing contract tests for the canonical token-footprint driver."""

import importlib.util
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
                text = {"items": [{"id": "i1", "s": flow.PORTAL, "text": "hi"}], "remaining": 0}
            if params["arguments"].get("surface") == "zone:subtitles":
                text = {"code": "ZONE_NOT_FOUND", "hint": "call hud_surfaces"}
                result = {"content": [{"type": "text", "text": json.dumps(text)}], "isError": True}
                return None, json.dumps({"method": method, "params": params}), "{}", {"result": result}
            result = {"content": [{"type": "text", "text": json.dumps(text)}]}
        return None, json.dumps({"method": method, "params": params}), "{}", {"result": result}

    return fake_rpc


class CanonicalFlowTests(unittest.TestCase):
    def setUp(self):
        flow.transactions.clear()

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
        with mock.patch.object(flow, "rpc", fake_rpc_factory([])), mock.patch("sys.stdout"):
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
            ],
        )

    def test_portal_ack_uses_the_polled_id(self):
        calls = []
        with mock.patch.object(flow, "rpc", fake_rpc_factory(calls)), mock.patch("sys.stdout"):
            flow.main()
        ack = [p for m, p in calls if m == "tools/call" and "ack" in p["arguments"]]
        self.assertEqual(ack[0]["arguments"]["ack"], ["i1"])

    def test_unexpected_tool_error_raises(self):
        def failing_rpc(method, params):
            result = {"content": [{"type": "text", "text": "{}"}], "isError": True}
            return None, "{}", "{}", {"result": result}

        with mock.patch.object(flow, "rpc", failing_rpc):
            with self.assertRaises(RuntimeError):
                flow.tool("zone_publish", "hud_publish", "hud_publish", {})


if __name__ == "__main__":
    unittest.main()
