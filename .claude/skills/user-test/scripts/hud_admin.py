#!/usr/bin/env python3
"""Operator calls on the HUD's MCP port; the PSK comes from the paired file.

  hud_admin.py status
  hud_admin.py logs [--tail 200]
  hud_admin.py screenshot [-o hud.png]
  hud_admin.py update [--channel dev|stable|vX.Y.Z]
  hud_admin.py restart

Needs an agent paired with --admin. Host comes from HUD_HOST.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request

import hud_env


def call(path: str, body: dict | None = None, host: str | None = None) -> tuple[int, bytes]:
    request = urllib.request.Request(
        hud_env.base_url(host) + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {hud_env.load_psk(host)}"},
        method="GET" if body is None else "POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", help="HUD host (default: HUD_HOST)")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("status")
    sub.add_parser("restart")
    logs = sub.add_parser("logs")
    logs.add_argument("--tail", type=int, default=100)
    shot = sub.add_parser("screenshot")
    shot.add_argument("-o", "--output", default="hud.png")
    update = sub.add_parser("update")
    update.add_argument("--channel", default="dev")
    args = parser.parse_args(argv)

    try:
        if args.cmd == "status":
            status, data = call("/admin/status", host=args.host)
        elif args.cmd == "logs":
            status, data = call(f"/admin/logs?tail={args.tail}", host=args.host)
        elif args.cmd == "screenshot":
            status, data = call("/admin/screenshot", host=args.host)
            if status == 200:
                with open(args.output, "wb") as handle:
                    handle.write(data)
                data = f"wrote {args.output}\n".encode()
        elif args.cmd == "update":
            status, data = call("/admin/update", {"channel": args.channel}, args.host)
        else:
            status, data = call("/admin/restart", {}, args.host)
    except hud_env.HudEnvError as error:
        print(f"hud_admin: {error}", file=sys.stderr)
        return 2
    except OSError as error:
        print(f"hud_admin: {type(error).__name__} reaching the HUD", file=sys.stderr)
        return 1
    sys.stdout.write(data.decode("utf-8", errors="replace"))
    return 0 if 200 <= status < 300 else 1


if __name__ == "__main__":
    raise SystemExit(main())
