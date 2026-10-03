"""Shared HUD endpoint and PSK resolution for the Python clients.

`HUD_HOST` (a host, `host:port`, or URL) names the HUD. The MCP URL, gRPC
target, and PSK file all derive from it. The PSK lives in
`~/.config/tze-hud/<host>.psk` (mode 0600, written by `hud_pair.py`) and is
never printed: errors name the file, not its contents.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from urllib.parse import urlsplit

MCP_PORT = 9090
GRPC_PORT = 50051


class HudEnvError(Exception):
    """Misconfiguration; the message never contains a PSK."""


def config_dir() -> Path:
    return Path.home() / ".config" / "tze-hud"


def _split(raw: str | None) -> tuple[str, int | None]:
    raw = raw or os.environ.get("HUD_HOST", "")
    if not raw:
        raise HudEnvError("HUD_HOST is not set (host, host:port, or URL of the HUD)")
    parts = urlsplit(raw if "//" in raw else f"//{raw}")
    if not parts.hostname:
        raise HudEnvError(f"cannot read a host from {raw!r}")
    return parts.hostname, parts.port


def hostname(raw: str | None = None) -> str:
    return _split(raw)[0]


def mcp_url(raw: str | None = None) -> str:
    host, port = _split(raw)
    return f"http://{host}:{port or MCP_PORT}/mcp"


def base_url(raw: str | None = None) -> str:
    """Origin of the MCP port (for /pair and /admin/*)."""
    return mcp_url(raw).removesuffix("/mcp")


def grpc_target(raw: str | None = None) -> str:
    return f"{hostname(raw)}:{GRPC_PORT}"


def psk_path(raw: str | None = None) -> Path:
    return config_dir() / f"{hostname(raw)}.psk"


def load_psk(raw: str | None = None, env: str | None = None) -> str:
    """The agent PSK: environment variable `env` if given and set, else the file."""
    if env and os.environ.get(env):
        return os.environ[env]
    path = psk_path(raw)
    try:
        value = path.read_text(encoding="utf-8").strip()
    except OSError:
        value = ""
    if not value:
        raise HudEnvError(f"no PSK at {path}; pair first: hud_pair.py --code <code>")
    return value


def resolve(url: str | None = None, psk_env: str | None = None) -> tuple[str, str]:
    """(mcp_url, psk) from an explicit URL or HUD_HOST; PSK from env or file."""
    return mcp_url(url), load_psk(url, psk_env)


def main(argv: list[str]) -> int:
    """`mcp-headers`: the MCP client's `headersHelper` (JSON for the harness, not a terminal)."""
    if argv != ["mcp-headers"]:
        print("usage: hud_env.py mcp-headers", file=sys.stderr)
        return 2
    try:
        print(json.dumps({"Authorization": f"Bearer {load_psk()}"}))
    except HudEnvError as error:
        print(f"hud_env: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
