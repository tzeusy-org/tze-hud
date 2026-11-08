#!/usr/bin/env python3
"""Fail when tracked files carry real host identifiers.

Tailnet addresses and names, private-key filenames and similar host details belong
in local configuration outside version control (for example the per-host
endpoint record under ~/.config/tze-hud/), never in tracked files.

Two layers:
- Generic patterns, run everywhere including CI: Tailscale MagicDNS names
  (*.ts.net), private-key filenames (id_<algorithm>), and any CGNAT/Tailscale IPv4 literal
  (100.64.0.0/10) other than the synthetic fixture values listed below.
- An optional local denylist of literal strings (one per line, `#` comments,
  matched case-insensitively), read from $TZE_HUD_HOST_DENYLIST or
  ~/.config/tze-hud/host-identifiers.deny. It lives outside the repository so
  the identifiers it names are never committed; CI has none and skips it.

Findings print path:line and the rule, never the matched value.
"""
from __future__ import annotations

import ipaddress
import os
import re
import subprocess
import sys
from pathlib import Path

# Synthetic addresses used by tests and docs: range bounds and made-up hosts.
# A new fixture should reuse one of these rather than extend the list.
SYNTHETIC_CGNAT = frozenset({
    "100.64.0.0",       # range base (100.64.0.0/10)
    "100.64.0.1",
    "100.72.0.9",
    "100.100.1.1",
    "100.100.1.2",
    "100.100.1.3",
    "100.101.102.103",
    "100.127.255.255",  # range top
})
CGNAT = ipaddress.ip_network("100.64.0.0/10")
IPV4 = re.compile(r"(?<![\d.])(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})(?![\d.])")
MAGICDNS = re.compile(r"\b[a-z0-9-]+\.[a-z0-9-]+\.ts\.net\b", re.IGNORECASE)
KEY_FILE = re.compile(r"\bid_(?:rsa|dsa|ecdsa|ed25519)(?:_sk)?\b")
SKIP_SUFFIXES = (".png", ".jpg", ".jpeg", ".gif", ".ico", ".ttf", ".otf", ".woff", ".woff2", ".lock", ".pdf")


def default_denylist() -> Path:
    env = os.environ.get("TZE_HUD_HOST_DENYLIST")
    if env:
        return Path(env)
    base = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    return base / "tze-hud" / "host-identifiers.deny"


def load_denylist(path: Path) -> list[str]:
    if not path.is_file():
        return []
    entries = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            entries.append(line.lower())
    return entries


def scan_text(text: str, denylist: list[str]) -> list[tuple[int, str]]:
    findings = []
    for number, line in enumerate(text.splitlines(), 1):
        for match in IPV4.finditer(line):
            try:
                addr = ipaddress.ip_address(match.group(1))
            except ValueError:
                continue
            if addr in CGNAT and match.group(1) not in SYNTHETIC_CGNAT:
                findings.append((number, "tailnet/CGNAT address (use a synthetic fixture value)"))
        if MAGICDNS.search(line):
            findings.append((number, "Tailscale MagicDNS name"))
        if KEY_FILE.search(line):
            findings.append((number, "private-key filename"))
        lowered = line.lower()
        if any(entry in lowered for entry in denylist):
            findings.append((number, "local denylist entry"))
    return findings


def tracked_files(root: Path) -> list[Path]:
    out = subprocess.run(["git", "-C", str(root), "ls-files", "-z"], check=True, capture_output=True).stdout
    return [root / p for p in out.decode().split("\0") if p and not p.endswith(SKIP_SUFFIXES)]


def main(argv: list[str] | None = None) -> int:
    root = Path(subprocess.run(["git", "rev-parse", "--show-toplevel"], check=True,
                               capture_output=True, text=True).stdout.strip())
    deny_path = default_denylist()
    denylist = load_denylist(deny_path)
    findings = []
    for path in tracked_files(root):
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue
        findings += [(path.relative_to(root), n, rule) for n, rule in scan_text(text, denylist)]
    for rel, number, rule in findings:
        print(f"{rel}:{number}: {rule}", file=sys.stderr)
    layer = f"local denylist ({len(denylist)} entries)" if denylist else "no local denylist"
    if findings:
        print(f"Host identifiers found in tracked files ({len(findings)}; {layer}). "
              "Keep host details in local config outside the repository.", file=sys.stderr)
        return 1
    print(f"No host identifiers in tracked files ({layer}).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
