#!/usr/bin/env python3
"""Lines per surface: Rust (production and test), Python, and Markdown.

With --base, checks growth against the targets in size_targets.toml.
Counts tracked files the way
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
import io
import json
import re
import subprocess
import sys
import tomllib
from collections import defaultdict
from collections.abc import Collection
from pathlib import Path, PurePosixPath
from typing import NamedTuple

COLUMNS = ("rust", "rust_test", "python", "python_test", "markdown")
SOURCE_SUFFIXES = (".rs", ".py", ".md")
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


def counted_markdown(path: str) -> bool:
    return path.endswith(".md") and not path.startswith(MARKDOWN_SKIP)


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
        elif counted_markdown(path):
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


def committed(root: Path, ref: str) -> dict[str, str]:
    """Read the comparison commit's tracked source blobs without a checkout."""
    commit = subprocess.check_output(
        ["git", "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}"],
        cwd=root, stderr=subprocess.PIPE,
    ).decode().strip()
    entries = subprocess.check_output(["git", "ls-tree", "-rz", commit], cwd=root)
    sources = []
    for entry in entries.split(b"\0")[:-1]:
        metadata, name = entry.split(b"\t", 1)
        path = name.decode()
        if path.endswith(SOURCE_SUFFIXES):
            mode, _, object_id = metadata.split()
            if mode not in (b"100644", b"100755"):
                raise ValueError(f"comparison source must be a regular file: {path}")
            sources.append((path, object_id))

    objects = subprocess.check_output(
        ["git", "cat-file", "--batch"], cwd=root,
        input=b"".join(object_id + b"\n" for _, object_id in sources),
    )
    files = {}
    with io.BytesIO(objects) as stream:
        for path, object_id in sources:
            header = stream.readline().split()
            if len(header) != 3 or header[:2] != [object_id, b"blob"]:
                raise ValueError(f"comparison source unavailable: {path}")
            files[path] = stream.read(int(header[2])).decode("utf-8", "replace")
            if stream.read(1) != b"\n":
                raise ValueError(f"comparison source incomplete: {path}")
    return files


class Target(NamedTuple):
    surfaces: tuple[str, ...]
    ceilings: dict[str, int]
    grants: dict[str, int]


def line_limits(values: dict, fields: Collection[str]) -> bool:
    return bool(values) and all(
        name in fields and type(value) is int and value >= 0
        for name, value in values.items()
    )


def size_targets(path: Path, known_surfaces: set[str] | None) -> dict[str, Target]:
    with path.open("rb") as source:
        policy = tomllib.load(source)
    entries = policy.get("targets")
    allowances = policy.get("allowances", [])
    if policy.keys() - {"targets", "allowances"} or not isinstance(entries, dict) or not entries:
        raise ValueError("size policy must contain targets")
    if not isinstance(allowances, list):
        raise ValueError("size policy allowances must be a list")

    targets = {}
    seen_surfaces = set()
    for name, entry in entries.items():
        if not isinstance(entry, dict):
            raise ValueError(f"{name}: target must be a table")
        surfaces = entry.get("surfaces")
        ceilings = {key: value for key, value in entry.items() if key != "surfaces"}
        if not isinstance(surfaces, list) or not surfaces or not all(
            isinstance(surface, str) and surface.strip() for surface in surfaces
        ):
            raise ValueError(f"{name}: target needs named report surfaces")
        if len(set(surfaces)) != len(surfaces) or seen_surfaces.intersection(surfaces):
            raise ValueError(f"{name}: report surfaces must appear only once")
        if known_surfaces is not None:
            unknown = set(surfaces) - known_surfaces
            if unknown:
                raise ValueError(f"{name}: unknown report surface(s): {', '.join(sorted(unknown))}")
        if not line_limits(ceilings, COLUMNS):
            raise ValueError(f"{name}: target needs valid line ceilings")
        targets[name] = Target(tuple(surfaces), ceilings, dict.fromkeys(ceilings, 0))
        seen_surfaces.update(surfaces)

    seen_grants = set()
    for grant in allowances:
        required = ("feature", "approval", "target")
        if not isinstance(grant, dict) or not all(
            isinstance(grant.get(key), str) and grant[key].strip() for key in required
        ):
            raise ValueError("size allowance needs a feature, owner approval reference and target")
        target = targets.get(grant["target"])
        key = (grant["feature"], grant["target"])
        values = {field: value for field, value in grant.items() if field not in required}
        if not target or key in seen_grants or not line_limits(values, target.ceilings):
            raise ValueError(f"{key[0]}: invalid or duplicate size allowance for {key[1]}")
        for field, value in values.items():
            target.grants[field] += value
        seen_grants.add(key)
    return targets


def target_report(rows: dict, targets: dict[str, Target], base: dict | None) -> tuple[str, list[str]]:
    report = [
        "\n| Target surface | Metric | Current | Base | Sketch target | Recorded allowances | Distance to target |",
        "|---|---|--:|--:|--:|--:|--:|",
    ]
    failures = []
    for name, target in targets.items():
        for field, ceiling in target.ceilings.items():
            current = sum(rows.get(surface, {}).get(field, 0) for surface in target.surfaces)
            allowance = target.grants[field]
            previous = None if base is None else sum(
                base.get(surface, {}).get(field, 0) for surface in target.surfaces
            )
            prior = "—" if previous is None else f"{previous:,}"
            report.append(f"| {name} | {field} | {current:,} | {prior} | "
                          f"{ceiling:,} | {allowance:,} | {current - ceiling:+,} |")
            effective = ceiling + allowance
            if previous is not None and current > previous and current > effective:
                failures.append(
                    f"{name} ({field}) grew from {previous:,} to {current:,} lines "
                    f"above its {effective:,}-line ceiling (target {ceiling:,} + allowance {allowance:,})"
                )
    return "\n".join(report) + "\n", failures


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    ap.add_argument("--output-dir", type=Path, help="also write size-report.md/.json here")
    ap.add_argument("--base", help="check growth against this Git commit")
    args = ap.parse_args(argv)
    files = tracked(args.root)
    rows = count(files)
    text = table(rows)
    failures = []
    policy = args.root / "scripts/ci/size_targets.toml"
    try:
        base = None
        known_surfaces = None
        if args.base is not None:
            base_files = committed(args.root, args.base)
            base = count(base_files)
            # The report omits all-zero rows. Use filenames to recognize empty surfaces.
            known_surfaces = {
                surface(path) for path in files.keys() | base_files.keys()
                if path.endswith((".rs", ".py")) or counted_markdown(path)
            }
        if args.base is not None or policy.exists():
            target_text, failures = target_report(rows, size_targets(policy, known_surfaces), base)
            text += target_text
    except subprocess.CalledProcessError:
        print(f"size gate: cannot read comparison commit {args.base!r}", file=sys.stderr)
        return 2
    except (OSError, ValueError) as error:
        print(f"size gate: {error}", file=sys.stderr)
        return 2
    sys.stdout.write(text)
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        (args.output_dir / "size-report.md").write_text(text)
        (args.output_dir / "size-report.json").write_text(json.dumps(rows, indent=2, sort_keys=True) + "\n")
    for failure in failures:
        print(f"size gate: {failure}", file=sys.stderr)
    return int(bool(failures))


if __name__ == "__main__":
    raise SystemExit(main())
