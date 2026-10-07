#!/usr/bin/env python3
"""Pair this host with a HUD: trade the on-screen one-time code for a PSK.

  HUD_HOST=100.x.y.z hud_pair.py --code 482913 [--agent claude] [--admin]

POSTs /pair on the MCP port and writes the PSK to ~/.config/tze-hud/<host>.psk
(mode 0600). The PSK is never printed, and no error path includes it. Pairing
the same agent again rotates its key.
Private nonsecret endpoint metadata retains the actual request port. If its
write fails after key storage, pairing reports failure rather than a complete pair.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import tempfile
import urllib.error
import urllib.request

import hud_env


def write_psk(path, psk: str) -> None:
    """Write `psk` to `path`, created 0600 (mkstemp: O_EXCL, no symlink follow) in a 0700 dir."""
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path.parent, 0o700)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(psk + "\n")
        os.replace(tmp, path)
    except BaseException:
        os.unlink(tmp)
        raise


def pair(host: str | None, agent: str, code: str, admin: bool) -> dict:
    body = {"agent": agent, "code": code}
    if admin:
        body["admin"] = True
    request = urllib.request.Request(
        f"{hud_env.base_url(host)}/pair",
        data=json.dumps(body).encode(),
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            reply = json.loads(response.read())
    except urllib.error.HTTPError as error:
        try:
            detail = json.loads(error.read()).get("code", "")
        except ValueError:
            detail = ""
        raise SystemExit(f"hud_pair: HTTP {error.code} {detail}".rstrip()) from None
    psk = reply.get("psk")
    if not psk:
        raise SystemExit("hud_pair: reply carried no psk")
    path = hud_env.psk_path(host)
    write_psk(path, psk)
    # Persist the actual request port, not an advertised hostname in the reply.
    record = {"schema": 1, "mcp_url": hud_env.mcp_url(host), "psk_sha256": hashlib.sha256(psk.encode()).hexdigest()}
    write_psk(path.with_suffix(".endpoint.json"), json.dumps(record, separators=(",", ":")))
    return {"agent": reply.get("agent", agent), "psk_file": str(path), "mcp": reply.get("mcp"), "grpc": reply.get("grpc")}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", help="HUD host (default: HUD_HOST)")
    parser.add_argument("--agent", default="claude", help="agent id, [a-z0-9-]{1,32}")
    parser.add_argument("--code", required=True, help="6-digit code shown on the HUD")
    parser.add_argument("--admin", action="store_true", help="also grant /admin/*")
    args = parser.parse_args(argv)
    try:
        print(json.dumps(pair(args.host, args.agent, args.code, args.admin)))
    except hud_env.HudEnvError as error:
        print(f"hud_pair: {error}", file=sys.stderr)
        return 2
    except OSError as error:
        # Name the class only: a message could quote a path or body fragment.
        print(f"hud_pair: {type(error).__name__} talking to the HUD or writing the PSK file", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.tracebacklimit = 0
    raise SystemExit(main())
