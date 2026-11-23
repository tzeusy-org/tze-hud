"""Shared one-request JSON-RPC transport over the paired MCP port."""

import json
import urllib.request
from typing import Any


def call_tool(
    url: str,
    token: str,
    tool: str,
    arguments: dict[str, Any],
    request_id: int,
    *,
    timeout: float = 65,
    response_limit: int | None = None,
) -> dict[str, Any]:
    """Call once; a transport failure does not establish whether mutation occurred."""
    body = json.dumps(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        },
        allow_nan=False,
    ).encode("utf-8")
    req = urllib.request.Request(
        url=url,
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
        method="POST",
    )
    # hud_input and question publishes can wait 55 s; leave a response allowance beyond that wait.
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read() if response_limit is None else resp.read(response_limit + 1)
    if response_limit is not None and len(raw) > response_limit:
        raise ValueError("oversized MCP response")
    envelope = json.loads(raw.decode("utf-8"))
    if "error" in envelope:
        return {"error": envelope["error"]}
    result = envelope["result"]
    decoded = json.loads(result["content"][0]["text"])
    return {"error": decoded} if result.get("isError") else {"result": decoded}
