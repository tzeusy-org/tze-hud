#!/usr/bin/env python3
"""Drive the canonical MCP flows and record each request/response body.

Every call is standard MCP JSON-RPC (`tools/list`, `tools/call`). The Rust
calibration binary tokenizes the recorded bodies; each transaction carries
the flow it belongs to and an operation label unique within that flow.
"""

import json
import os
import sys
import urllib.error
import urllib.request

PORTAL = "portal:claude-main"
transactions = []
_next_id = 0


def rpc(method, params):
    global _next_id
    _next_id += 1
    message = {"jsonrpc": "2.0", "id": _next_id, "method": method, "params": params}
    request = urllib.request.Request(
        os.environ["HUD_MCP_URL"],
        data=json.dumps(message).encode(),
        headers={
            "Authorization": f"Bearer {os.environ['HUD_PSK']}",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            raw = response.read().decode("utf-8")
    except (urllib.error.HTTPError, urllib.error.URLError) as error:
        raise RuntimeError(f"MCP transport failed: {error}") from error
    parsed = json.loads(raw)
    if raw != json.dumps(parsed, separators=(",", ":")):
        raise RuntimeError("MCP response body is not canonical compact JSON")
    request_body = json.dumps(message, separators=(",", ":"))
    if os.environ["HUD_PSK"] in request_body or os.environ["HUD_PSK"] in raw:
        raise RuntimeError("canonical body retained a bearer credential")
    return message, request_body, raw, parsed


def record(flow, operation, method, params, expect_error=False):
    _, request_body, response_body, parsed = rpc(method, params)
    if parsed.get("error"):
        raise RuntimeError(f"{flow}/{operation} protocol error: {parsed['error']}")
    result = parsed["result"]
    if method == "tools/call":
        if bool(result.get("isError")) != expect_error:
            raise RuntimeError(f"{flow}/{operation} unexpected result: {result}")
        content = result.get("content")
        if not isinstance(content, list) or len(content) != 1 or content[0].get("type") != "text":
            raise RuntimeError(f"{flow}/{operation} returned an invalid content envelope")
        result = json.loads(content[0]["text"])
    transactions.append(
        {
            "flow": flow,
            "operation": operation,
            "request_body": request_body,
            "response_body": response_body,
        }
    )
    return result


def tool(flow, operation, name, arguments, expect_error=False):
    return record(
        flow, operation, "tools/call", {"name": name, "arguments": arguments}, expect_error
    )


def main():
    record("tools_list", "tools/list", "tools/list", {})
    tool("discover", "hud_surfaces", "hud_surfaces", {})
    tool(
        "zone_publish",
        "hud_publish",
        "hud_publish",
        {
            "surface": "zone:notification-area",
            "content": {"title": "Build", "body": "All tests passed.", "urgency": 1},
        },
    )
    tool(
        "widget_publish",
        "hud_publish",
        "hud_publish",
        {"surface": "widget:gauge", "params": {"level": 0.625}},
    )
    tool(
        "portal",
        "1_publish_attach",
        "hud_publish",
        {
            "surface": PORTAL,
            "content": "Tests pass. Ship it?",
            "status": "active",
            "expects_reply": True,
        },
    )
    polled = tool("portal", "2_input_poll", "hud_input", {"wait_ms": 1000})
    input_id = polled["items"][0]["id"]
    tool("portal", "3_input_ack", "hud_input", {"ack": [input_id]})
    tool("portal", "4_clear", "hud_clear", {"surface": PORTAL})
    tool(
        "error",
        "hud_publish",
        "hud_publish",
        {"surface": "zone:subtitles", "content": "hello"},
        expect_error=True,
    )
    json.dump({"transactions": transactions}, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
