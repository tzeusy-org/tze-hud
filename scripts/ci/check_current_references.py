#!/usr/bin/env python3
"""Keep retired design citations out of the current source tree."""

from pathlib import Path
import re
import subprocess
import sys


def main() -> int:
    # Character classes keep the expression from matching its own source.
    pattern = r"open[s]pec|RFC ?[0-9]{4}|doc[t]rine"
    result = subprocess.run(
        [
            "git", "grep", "-nIE", pattern, "--",
            ":!docs", ":!CLAUDE.md",
            ":!.claude/skills/user-test-performance/reference/results.csv",
        ],
        cwd=Path(__file__).resolve().parents[2],
        capture_output=True,
        text=True,
    )
    if result.returncode == 1:
        print("Current-source reference check passed")
        return 0
    if result.returncode != 0:
        print("Current-source reference check could not inspect tracked files", file=sys.stderr)
        return 1
    matches = result.stdout.splitlines()
    for line in matches[:20]:
        location = re.match(r"^(.*?):([0-9]+):", line)
        if location:
            print(f"{location[1]}:{location[2]}: retired design citation", file=sys.stderr)
    print(f"Current-source reference check failed: {len(matches)} matching lines", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
