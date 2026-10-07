"""Shared HUD endpoint and PSK resolution for the Python clients.

`HUD_HOST` (a host, `host:port`, or URL) names the HUD. The MCP URL, gRPC
target, and PSK file all derive from it. The PSK lives in
`~/.config/tze-hud/<host>.psk` (mode 0600, written by `hud_pair.py`) and is
never printed: errors name the file, not its contents.
The stdio adapter has separate strict helpers for sole-host discovery and
private nonsecret endpoint records; explicit client APIs below keep their defaults.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import stat
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
        mode = path.stat().st_mode & 0o777
        value = path.read_text(encoding="utf-8").strip() if not mode & 0o077 else ""
    except OSError:
        mode, value = 0, ""
    if mode & 0o077:
        raise HudEnvError(f"{path} is mode {mode:03o}; run chmod 600 on it (or re-pair)")
    if not value:
        raise HudEnvError(f"no PSK at {path}; pair first: hud_pair.py --code <code>")
    return value


def resolve(url: str | None = None, psk_env: str | None = None) -> tuple[str, str]:
    """(mcp_url, psk) from an explicit URL or HUD_HOST; PSK from env or file."""
    return mcp_url(url), load_psk(url, psk_env)


PAIR_COMMAND = "python3 .claude/skills/user-test/scripts/hud_pair.py --host <HUD-address[:port]> --code <on-screen-code>"


def _adapter_url(raw: str) -> tuple[str, str]:
    """Strict adapter-only HTTP origin; ordinary explicit client helpers stay unchanged."""
    try:
        parts = urlsplit(raw if "://" in raw else f"http://{raw}")
        host = parts.hostname
        port = parts.port if parts.port is not None else MCP_PORT
        if (
            parts.scheme != "http" or not host or not 1 <= port <= 65535
            or parts.username is not None or parts.password is not None
            or parts.query or parts.fragment or parts.path not in ("", "/", "/mcp")
            or re.search(r"[^a-zA-Z0-9_.:\-]", host)
        ):
            raise ValueError
    except ValueError:
        raise HudEnvError("invalid HUD endpoint; use an HTTP host[:port], without variables or credentials") from None
    rendered = f"[{host}]" if ":" in host else host
    return host, f"http://{rendered}:{port}/mcp"


def _private_bytes(path: Path, limit: int) -> bytes:
    """Read only the selected owner-private regular file, without following its symlink."""
    try:
        directory = path.parent.lstat()
        if not stat.S_ISDIR(directory.st_mode) or directory.st_uid != os.getuid() or directory.st_mode & 0o077:
            raise HudEnvError("unsafe HUD pairing directory; re-pair privately")
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise HudEnvError("unsafe selected HUD file; re-pair privately")
            data = os.read(fd, limit + 1)
            if len(data) > limit:
                raise HudEnvError("oversized selected HUD file; re-pair")
            return data
        finally:
            os.close(fd)
    except OSError:
        raise HudEnvError(f"cannot read selected private HUD file; pair first: {PAIR_COMMAND}") from None


def _endpoint_record(host: str) -> dict | None:
    path = config_dir() / f"{host}.endpoint.json"
    if not os.path.lexists(path):
        return None
    try:
        record = json.loads(_private_bytes(path, 4096))
        if not isinstance(record, dict) or set(record) != {"schema", "mcp_url", "psk_sha256"}:
            raise ValueError
        if type(record["schema"]) is not int or record["schema"] != 1:
            raise ValueError
        found_host, url = _adapter_url(record["mcp_url"])
        if found_host != host or url != record["mcp_url"]:
            raise ValueError
        if not isinstance(record["psk_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", record["psk_sha256"]):
            raise ValueError
        return record
    except (ValueError, TypeError, KeyError, UnicodeError):
        raise HudEnvError("invalid HUD endpoint record; re-pair, no default-port fallback") from None


def adapter_endpoint() -> dict:
    """Pin a nonsecret endpoint once; discovery inspects names/stats, never key contents."""
    raw = os.environ.get("HUD_HOST", "")
    if raw:
        host, url = _adapter_url(raw)
        _endpoint_record(host)  # Present malformed metadata is never a silent fallback.
        return {"host": host, "url": url, "explicit": True}
    try:
        directory = config_dir().lstat()
        if not stat.S_ISDIR(directory.st_mode) or directory.st_uid != os.getuid() or directory.st_mode & 0o077:
            raise HudEnvError("unsafe HUD pairing directory; re-pair privately")
        candidates = []
        for path in config_dir().iterdir():
            if not path.name.endswith(".psk"):
                continue
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise HudEnvError("unsafe HUD pairing file; re-pair privately")
            name = path.name.removesuffix(".psk")
            host, _ = _adapter_url(f"[{name}]" if ":" in name else name)
            if name != host:
                raise HudEnvError("invalid HUD pairing filename; select HUD_HOST explicitly")
            candidates.append(host)
    except FileNotFoundError:
        candidates = []
    except OSError:
        raise HudEnvError("cannot inspect HUD pairing directory") from None
    if not candidates:
        raise HudEnvError(f"no paired HUD; run {PAIR_COMMAND}")
    if len(candidates) != 1:
        raise HudEnvError("multiple paired HUDs; set HUD_HOST to the intended host[:port]")
    host = candidates[0]
    record = _endpoint_record(host)
    _, legacy = _adapter_url(f"[{host}]" if ":" in host else host)
    return {"host": host, "url": record["mcp_url"] if record else legacy, "explicit": False}


def adapter_key(endpoint: dict) -> str:
    """Load just the selected key before auth; rotation cannot retarget a running adapter."""
    host = endpoint["host"]
    try:
        key = _private_bytes(config_dir() / f"{host}.psk", 256).decode("utf-8").strip()
    except UnicodeError:
        raise HudEnvError("invalid selected HUD key; re-pair") from None
    if not key or re.search(r"[\r\n]", key):
        raise HudEnvError("invalid selected HUD key; re-pair")
    record = _endpoint_record(host)
    if record:
        if record["psk_sha256"] != hashlib.sha256(key.encode()).hexdigest():
            raise HudEnvError("stale HUD endpoint record; re-pair")
        if not endpoint["explicit"] and record["mcp_url"] != endpoint["url"]:
            raise HudEnvError("HUD endpoint changed; reconnect the MCP client")
    elif not endpoint["explicit"]:
        _, legacy = _adapter_url(f"[{host}]" if ":" in host else host)
        if endpoint["url"] != legacy:
            raise HudEnvError("HUD endpoint record disappeared; reconnect or re-pair")
    return key


def main(argv: list[str]) -> int:
    """`mcp-headers`: the MCP client's `headersHelper` (JSON for the harness, not a terminal)."""
    if argv != ["mcp-headers"]:
        print("usage: hud_env.py mcp-headers", file=sys.stderr)
        return 2
    try:
        if re.search(r"[:/@]", os.environ.get("HUD_HOST", "")):
            raise HudEnvError("HUD_HOST must be a bare host for .mcp.json (no scheme or port)")
        print(json.dumps({"Authorization": f"Bearer {load_psk()}"}))
    except HudEnvError as error:
        print(f"hud_env: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
