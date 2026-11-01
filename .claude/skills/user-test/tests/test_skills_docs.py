"""Operational skills and scripts retain only supported transports and tools."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SKILL_ROOTS = [ROOT / d / "skills" for d in (".claude", ".codex", ".opencode", ".gemini")]
DOC_SUFFIXES = {".md", ".json", ".yaml", ".yml"}
SOURCE_SUFFIXES = DOC_SUFFIXES | {
    ".py", ".sh", ".bash", ".zsh", ".ps1", ".psm1", ".bat", ".cmd",
    ".js", ".ts", ".mjs", ".cjs", ".toml",
}
OPERATION_ROOTS = [*SKILL_ROOTS, ROOT / "scripts", ROOT / ".github"]
OPERATION_FILES = [ROOT / "justfile", ROOT / ".mcp.json"]
TRANSPORT = re.compile(r"\b(?:ssh|scp)\b", re.IGNORECASE)

REMOVED_TOOL = re.compile(
    r"list_(?:zones|widgets)|publish_to_(?:zone|widget)|clear_(?:zone|widget)\b"
    r"|portal_projection_(?:attach)|get_pending_(?:input)|create_(?:tab)\b"
)
RETIRED = re.compile(r"\b(?:ssh|scp)\b|TZE_HUD_PSK|MCP_TEST_PSK", re.IGNORECASE)


def skill_files(suffixes=None):
    for root in SKILL_ROOTS:
        for path in sorted(root.rglob("*")):
            if (
                path.is_file()
                and "proto_gen" not in path.parts
                and "__pycache__" not in path.parts
                and (suffixes is None or path.suffix in suffixes)
                and path.suffix not in {".pyc", ".csv"}
            ):
                yield path


def offenders(pattern, suffixes):
    return [
        f"{p.relative_to(ROOT)}: {m.group(0)}"
        for p in skill_files(suffixes)
        for m in [pattern.search(p.read_text(encoding="utf-8"))]
        if m
    ]


def operational_files():
    # Test/guard definitions and generated protocol bindings are not operations.
    # Scan aliases too: an independent mirror may diverge from the canonical tree.
    for root in OPERATION_ROOTS:
        for path in sorted(root.rglob("*")):
            relative = path.relative_to(root)
            if (
                path.is_file()
                and not {"tests", "testdata", "__pycache__", "proto_gen"}.intersection(relative.parts)
                and not path.name.startswith("test_")
                and not path.stem.endswith("_test")
                and (path.suffix in SOURCE_SUFFIXES or not path.suffix)
            ):
                yield path
    for path in OPERATION_FILES:
        if path.is_file():
            yield path


def test_operational_sources_do_not_use_retired_transports_or_doc_psks():
    sources = list(operational_files())
    # Fail if corpus changes silently drop executable skill or deployment sources.
    assert ROOT / ".claude/skills/user-test/scripts/stress_test_zones.py" in sources
    assert ROOT / "scripts/ci/windows/run-quiescent-efficiency.ps1" in sources
    assert ROOT / "scripts/dev-run.sh" in sources
    assert {".py", ".sh", ".ps1"} <= {p.suffix for p in sources}
    assert [
        f"{p.relative_to(ROOT)}: {m.group(0)}"
        for p in sources
        for m in [TRANSPORT.search(p.read_text(encoding="utf-8"))]
        if m
    ] == []
    # Client-only environment credential overrides remain supported in code.
    assert offenders(RETIRED, DOC_SUFFIXES) == []


def test_no_skill_file_names_a_removed_tool():
    assert offenders(REMOVED_TOOL, None) == []
