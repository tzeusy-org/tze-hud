"""Contract checks for the HUD projection adoption path."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
SKILL_DIR = ROOT / ".claude" / "skills" / "hud-projection"
REMOVED_TOOLS = ("portal_projection_attach", "get_pending_input", "publish_to_zone", "list_zones", "create_tab")


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_projection_guidance_uses_the_five_verbs() -> None:
    skill = read(SKILL_DIR / "SKILL.md")
    facade = read(SKILL_DIR / "references" / "mcp-facade.md")
    client = read(SKILL_DIR / "scripts" / "portal_client.py")

    for verb in ("hud_publish", "hud_input", "hud_clear"):
        assert verb in skill
        assert verb in client
    assert "tools/call" in facade
    for contents in (skill, facade, client):
        for removed in REMOVED_TOOLS:
            assert removed not in contents, removed


def test_owner_token_stays_off_the_model_surface() -> None:
    skill = read(SKILL_DIR / "SKILL.md")
    client = read(SKILL_DIR / "scripts" / "portal_client.py")

    assert "owner_token" not in skill
    assert "owner_token" not in client
    assert "server-side" in skill


def test_quickstart_warp_note_is_vm_only() -> None:
    quickstart = read(ROOT / "docs" / "QUICKSTART.md")

    assert "Known runtime bug (hud-d5rcd)" not in quickstart
