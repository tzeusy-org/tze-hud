"""Skill docs and fixtures describe the pairing flow, not SSH or removed tools."""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
SKILL_ROOTS = [ROOT / d / "skills" for d in (".claude", ".opencode", ".gemini")]
# Deleted wholesale by hud-i2e10.12 (SSH deploy tooling); not rewritten here.
LEGACY = ROOT / ".claude/skills/user-test/subskills/portal-hud-deploy"
DOC_SUFFIXES = {".md", ".json", ".yaml"}

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
                and LEGACY not in path.parents
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


def test_skill_docs_do_not_mention_ssh_or_the_old_psk_variables():
    assert offenders(RETIRED, DOC_SUFFIXES) == []


def test_no_skill_file_names_a_removed_tool():
    assert offenders(REMOVED_TOOL, None) == []
