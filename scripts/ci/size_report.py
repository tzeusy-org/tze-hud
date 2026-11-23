#!/usr/bin/env python3
"""Lines per surface: Rust (production and test), Python, and Markdown.

Reports only; it enforces no target. Counts tracked files the way
`git ls-files '*.rs' | xargs cat | wc -l` does (newline characters), so the
Rust total matches that command.

A surface is derived from the path: `crates/<name>`, `app/<name>`,
`examples/<name>` and `tests/<name>` keep two components, `.claude/skills`
is one surface, and anything else is its top-level directory (or `(root)`).

Rust test lines are files under a `tests/` or `benches/` directory, inline
`#[cfg(test)] mod name { ... }` blocks (also `cfg(all(test, ...))`), and files
a `#[cfg(test)] mod name;` declaration pulls in. Python test lines are `test_*.py` files and files under
a `tests/` directory. Markdown excludes tracker state (`.beads`): it is what a
session or contributor reads.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path, PurePosixPath

COLUMNS = ("rust", "rust_test", "python", "python_test", "markdown")
TWO_PART = {"crates", "app", "examples", "tests"}
MARKDOWN_SKIP = (".beads/",)
# `cfg(all(test, ...))` is test-only; `cfg(any(test, ...))` is shared code.
CFG_TEST = re.compile(r"^(\s*)#\[cfg\((?:test|all\(test\b.*\))\)\]\s*$")
TEST_MOD = re.compile(r"^(\s*)(?:pub(?:\([a-z]+\))? )?mod (\w+)\s*(\{|;)\s*$")


def surface(path: str) -> str:
    parts = PurePosixPath(path).parts
    if len(parts) == 1:
        return "(root)"
    if parts[0] in TWO_PART and len(parts) > 2:
        return f"{parts[0]}/{parts[1]}"
    if parts[:2] == (".claude", "skills"):
        return ".claude/skills"
    return parts[0]


def in_test_dir(path: str) -> bool:
    return any(p in ("tests", "benches") for p in PurePosixPath(path).parts[:-1])


def rust_test_lines(text: str) -> tuple[int, list[str]]:
    """Inline test-module lines, and the names of `#[cfg(test)] mod x;` files."""
    lines = text.split("\n")
    count, files, i = 0, [], 0
    while i < len(lines) - 1:
        attr = CFG_TEST.match(lines[i])
        item = TEST_MOD.match(lines[i + 1]) if attr else None
        if not item:
            i += 1
            continue
        indent, name, kind = item.groups()
        if kind == ";":
            files.append(name)
            i += 2
            continue
        end = i + 2
        while end < len(lines) and lines[end] != f"{indent}}}":
            end += 1
        count += min(end, len(lines) - 1) - i + 1
        i = end + 1
    return count, files


def module_paths(path: str, name: str) -> list[str]:
    """Where `mod name;` declared in `path` may live (file or directory)."""
    p = PurePosixPath(path)
    base = p.parent if p.name in ("mod.rs", "lib.rs", "main.rs") else p.parent / p.stem
    return [str(base / f"{name}.rs"), str(base / name) + "/"]


def count(files: dict[str, str]) -> dict[str, dict[str, int]]:
    """Per-surface counts from {path: text}."""
    rows: dict[str, dict[str, int]] = defaultdict(lambda: dict.fromkeys(COLUMNS, 0))
    test_modules: list[str] = []
    inline: dict[str, int] = {}
    for path, text in files.items():
        if path.endswith(".rs") and not in_test_dir(path):
            inline[path], names = rust_test_lines(text)
            test_modules += [m for n in names for m in module_paths(path, n)]
    for path, text in files.items():
        lines = text.count("\n")
        row = rows[surface(path)]
        name = PurePosixPath(path).name
        if path.endswith(".rs"):
            whole = in_test_dir(path) or any(
                path == m or (m.endswith("/") and path.startswith(m)) for m in test_modules
            )
            test = lines if whole else min(inline.get(path, 0), lines)
            row["rust"] += lines - test
            row["rust_test"] += test
        elif path.endswith(".py"):
            row["python_test" if name.startswith("test_") or in_test_dir(path) else "python"] += lines
        elif path.endswith(".md") and not path.startswith(MARKDOWN_SKIP):
            row["markdown"] += lines
    return {s: r for s, r in rows.items() if any(r.values())}


def table(rows: dict[str, dict[str, int]]) -> str:
    total = {c: sum(r[c] for r in rows.values()) for c in COLUMNS}
    order = sorted(rows, key=lambda s: (-(rows[s]["rust"] + rows[s]["rust_test"]), s))
    head = ("surface", "rust prod", "rust test", "py prod", "py test", "markdown")
    out = ["| " + " | ".join(head) + " |", "|---" + "|--:" * (len(head) - 1) + "|"]
    for name, r in [*((s, rows[s]) for s in order), ("**total**", total)]:
        out.append(f"| {name} | " + " | ".join(f"{r[c]:,}" for c in COLUMNS) + " |")
    rust = total["rust"] + total["rust_test"]
    out.append(f"\nRust total: {rust:,} lines ({total['rust_test']:,} test).")
    return "\n".join(out) + "\n"


def tracked(root: Path) -> dict[str, str]:
    names = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs", "*.py", "*.md"],
        cwd=root, check=True, capture_output=True,
    ).stdout.decode().split("\0")
    return {
        n: (root / n).read_bytes().decode("utf-8", "replace")
        for n in names
        if n and (root / n).is_file()
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    ap.add_argument("--output-dir", type=Path, help="also write size-report.md/.json here")
    args = ap.parse_args(argv)
    rows = count(tracked(args.root))
    text = table(rows)
    sys.stdout.write(text)
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        (args.output_dir / "size-report.md").write_text(text)
        (args.output_dir / "size-report.json").write_text(json.dumps(rows, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
