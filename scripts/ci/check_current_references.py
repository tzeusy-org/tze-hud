#!/usr/bin/env python3
"""Keep retired design citations and stale bead breadcrumbs out of current source.

Two rules run over tracked files:

- Citation forms that point at the retired pre-reset design corpus (spec
  requirement headings, design RFC numbers, spec line numbers, v1 release
  wording, removed design docs) are rejected in every scanned file,
  case-insensitively.
- Bead IDs in Rust and proto sources are rejected unless the bead is still
  outstanding (``OPEN_BEADS``). A finished bead's ID tells a reader nothing the
  code does not; remove the ID when its bead closes.

``docs/`` and the reset notices are exempt; so are the T7 boundary files,
whose wording belongs to that track's rewrite.

Two more rules run over every tracked Markdown file, ``docs/`` included:

- A link to a repository path must resolve: ``[text](path)``,
  ``[text](path#L10-L20)`` and ``[label]: path`` name a file or directory that
  exists, relative to the linking file (or to the repository root when the
  path starts with ``/``). Web, mail and in-page ``#anchor`` links are not
  checked, nor are links inside code.
- A doc does not pin a source snapshot (``checked against <sha>``): it goes
  stale silently, while a broken link fails here.
"""

from pathlib import Path
import re
import subprocess
import sys

# Character classes keep each expression from matching its own source. The
# retired design RFCs were zero-padded (0001-0099); IETF RFCs never are.
CITATION = re.compile(
    r"open[s]pec|doc[t]rine|requiremen[t]:|post-v[1]\b|v1-reserve[d]|docs/desig[n]/"
    r"|\bRF[C][- ]?0[0-9]{3}\b|\(line [0-9]+\)",
    re.IGNORECASE,
)
BEAD_ID = re.compile(r"\bhu[d]-[a-z0-9]{5}(?:\.[0-9]+)*\b")
BEAD_SUFFIXES = (".rs", ".proto")
INLINE_LINK = re.compile(r"!?\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
REFERENCE_LINK = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s.*)?$")
INLINE_CODE = re.compile(r"`+[^`]*`+")
EXTERNAL = re.compile(r"^(?:[a-z][a-z0-9+.-]*:|//|#)", re.IGNORECASE)
SNAPSHOT = re.compile(
    r"(?:source snapsho[t]|checked agains[t])\W{0,3}[0-9a-f]{7,40}\b", re.IGNORECASE
)

# Outstanding beads that source may still name; an entry covers its children.
OPEN_BEADS = (
    "hud-1pt5h",
    "hud-2v8br",
    "hud-h51u7",
    "hud-ljhku",
    "hud-lks0i",
    "hud-uym23",
)

EXEMPT_PREFIXES = (
    "docs/",
    "crates/tze_hud_projection/",
    "tests/integration/text_stream_portal_",
)
EXEMPT_FILES = {
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/skills/user-test-performance/reference/results.csv",
    # T7 boundary files.
    "crates/tze_hud_runtime/src/portal_projection_driver.rs",
    "crates/tze_hud_runtime/src/windowed/portal.rs",
    "crates/tze_hud_input/src/composer_draft.rs",
    "crates/tze_hud_input/src/portal_resize.rs",
    "crates/tze_hud_config/src/portal_tokens.rs",
    "crates/tze_hud_mcp/src/portal_op.rs",
}


def is_exempt(path: str) -> bool:
    return path in EXEMPT_FILES or path.startswith(EXEMPT_PREFIXES)


def is_open_bead(bead: str) -> bool:
    return any(bead == b or bead.startswith(b + ".") for b in OPEN_BEADS)


def scan_text(path: str, text: str) -> list[tuple[int, str]]:
    """Return (line, reason) findings for one file's text."""
    findings = []
    check_beads = path.endswith(BEAD_SUFFIXES)
    for number, line in enumerate(text.splitlines(), 1):
        if CITATION.search(line):
            findings.append((number, "retired design citation"))
        if check_beads:
            for match in BEAD_ID.finditer(line):
                if not is_open_bead(match[0]):
                    findings.append((number, f"closed or unknown bead {match[0]}"))
    return findings


def link_targets(text: str) -> list[tuple[int, str]]:
    """Return (line, target) for each Markdown link outside code."""
    targets = []
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.lstrip()
        if stripped.startswith(("```", "~~~")):
            marker = stripped[:3]
            fence = None if fence == marker else (fence or marker)
            continue
        if fence:
            continue
        prose = INLINE_CODE.sub("", line)
        found = [m[1] for m in INLINE_LINK.finditer(prose)]
        reference = REFERENCE_LINK.match(prose)
        if reference:
            found.append(reference[1])
        targets.extend((number, target) for target in found)
    return targets


def scan_markdown(root: Path, path: str, text: str) -> list[tuple[int, str]]:
    """Return (line, reason) findings: unresolved repository links, pinned snapshots."""
    findings = []
    for number, line in enumerate(text.splitlines(), 1):
        if SNAPSHOT.search(line):
            findings.append((number, "pinned source snapshot"))
    base = (root / path).parent
    for number, target in link_targets(text):
        if EXTERNAL.match(target):
            continue
        relative = target.split("#", 1)[0].split("?", 1)[0]
        if not relative:
            continue
        resolved = root / relative.lstrip("/") if relative.startswith("/") else base / relative
        if not resolved.exists():
            findings.append((number, f"link target does not exist: {target}"))
    return findings


def tracked_files(root: Path) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, capture_output=True, check=True
    )
    return [p for p in result.stdout.decode().split("\0") if p]


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    try:
        files = tracked_files(root)
    except (OSError, subprocess.CalledProcessError):
        print("Current-source reference check could not list tracked files", file=sys.stderr)
        return 1
    findings = []
    for path in files:
        markdown = path.endswith(".md")
        if is_exempt(path) and not markdown:
            continue
        try:
            text = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue  # binary, or deleted from the working tree
        if not is_exempt(path):
            findings.extend((path, n, why) for n, why in scan_text(path, text))
        if markdown:
            findings.extend((path, n, why) for n, why in scan_markdown(root, path, text))
    if not findings:
        print("Current-source reference check passed")
        return 0
    for path, number, why in findings[:20]:
        print(f"{path}:{number}: {why}", file=sys.stderr)
    print(f"Current-source reference check failed: {len(findings)} findings", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
