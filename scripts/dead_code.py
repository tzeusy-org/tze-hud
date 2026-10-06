#!/usr/bin/env python3
"""List dead items in one crate's public surface (advisory; always exits 0 on success).

Method: copy the workspace to a temp dir, narrow every `pub` item in the target
crate's src/ to `pub(crate)` unless another crate's non-test code mentions its
name, then `cargo check -p <crate> --lib` with lints capped to warnings and
print the dead_code diagnostics. Never touches the working tree: the copy skips
build output and tracker state (.beads), and the default build dir
(target/dead-code) is removed afterwards unless CARGO_TARGET_DIR is set.

Usage: scripts/dead_code.py <crate-name>     (e.g. tze_hud_telemetry)
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
COPY_IGNORE = shutil.ignore_patterns("target", ".git", ".worktrees", ".beads", "node_modules", "test_results")

# `pub` (not `pub(...)`, not `pub use`) followed by an optional item keyword and the name.
PUB_ITEM = re.compile(
    r"^(\s*)pub\s+(?!\(|use\b)"
    r"((?:(?:const|async|unsafe|extern\s+\"[^\"]*\")\s+)*"
    r"(?:fn|struct|enum|trait|const|static|type|mod|union)\s+)?"
    r"(r#)?([A-Za-z_][A-Za-z0-9_]*)"
)
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def crate_dir(name: str, root: Path) -> Path:
    for manifest in sorted(root.glob("*/*/Cargo.toml")):
        text = manifest.read_text()
        if re.search(rf'^name\s*=\s*"{re.escape(name)}"', text, re.M):
            return manifest.parent
    sys.exit(f"dead_code: no workspace crate named {name!r}")


def is_test_path(path: Path) -> bool:
    return any(p in ("tests", "benches") for p in path.parts) or path.name in ("tests.rs", "test_util.rs")


def external_names(target: Path, root: Path) -> set[str]:
    """Identifiers appearing in other crates' non-test source."""
    names: set[str] = set()
    for src in list(root.glob("crates/*/src/**/*.rs")) + list(root.glob("examples/*/src/**/*.rs")):
        if target in src.parents or is_test_path(src.relative_to(root)):
            continue
        names.update(IDENT.findall(src.read_text(errors="replace")))
    return names


PUB_USE = re.compile(r"^(\s*)pub\s+use\b[^;]*;", re.M)


def reexport_plan(text: str, keep: set[str]) -> tuple[list[tuple[int, int]], set[str]]:
    """Split `pub use` statements into narrowable spans and names that must stay pub.

    A re-export is narrowed only if none of its names is used by another crate;
    otherwise it stays `pub` and the items it names are added to the keep set.
    """
    spans, extra = [], set()
    for m in PUB_USE.finditer(text):
        idents = set(IDENT.findall(m.group(0)[m.group(0).index("use") + 3:])) - {"self", "super", "crate", "as"}
        if "*" in m.group(0) or idents & keep:
            extra |= idents
        else:
            spans.append((m.start(), m.start(1) + len(m.group(1)) + 3))
    return spans, extra


def narrow(target: Path, keep: set[str]) -> int:
    files = list((target / "src").rglob("*.rs"))
    # Pass 1: re-exports used externally keep their items public.
    keep = set(keep)
    for src in files:
        keep |= reexport_plan(src.read_text(), keep)[1]
    narrowed = 0
    for src in files:
        text = src.read_text()
        spans, _ = reexport_plan(text, keep)
        for start, end in reversed(spans):  # `pub` -> `pub(crate)` at span end
            text = text[:end] + "(crate)" + text[end:]
            narrowed += 1
        out = []
        for line in text.splitlines(keepends=True):
            m = PUB_ITEM.match(line)
            if m and m.group(4) not in keep:
                line = line.replace("pub ", "pub(crate) ", 1)
                narrowed += 1
            out.append(line)
        src.write_text("".join(out))
    return narrowed


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("crate", help="workspace crate name")
    args = ap.parse_args()

    with tempfile.TemporaryDirectory(prefix="dead-code-") as tmp:
        root = Path(tmp) / "ws"
        shutil.copytree(REPO_ROOT, root, ignore=COPY_IGNORE)
        target = crate_dir(args.crate, root)
        count = narrow(target, external_names(target, root))
        print(f"# narrowed {count} pub items in {args.crate}", file=sys.stderr)

        env = dict(os.environ)
        env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " --cap-lints warn").strip()
        # Our own build dir is a multi-GB one-off; a caller-chosen one is kept.
        own_target = None if env.get("CARGO_TARGET_DIR") else REPO_ROOT / "target" / "dead-code"
        if own_target:
            env["CARGO_TARGET_DIR"] = str(own_target)
        try:
            proc = subprocess.run(
                ["cargo", "check", "-p", args.crate, "--lib", "--message-format=short"],
                cwd=root, env=env, text=True, capture_output=True,
            )
        finally:
            if own_target:
                shutil.rmtree(own_target, ignore_errors=True)
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            return proc.returncode
        hits = sorted({l for l in proc.stderr.splitlines() if "never used" in l or "unused import" in l or "never constructed" in l
                       or "never read" in l or "never called" in l})
        for line in hits:
            print(line.replace(str(root), "."))
        print(f"# {len(hits)} dead item(s) in {args.crate}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
