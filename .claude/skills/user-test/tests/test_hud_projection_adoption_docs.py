"""Contract checks for the HUD projection adoption path."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
SKILL_DIR = ROOT / ".claude" / "skills" / "hud-projection"


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


def test_portal_identity_is_the_psk_not_a_token() -> None:
    skill = read(SKILL_DIR / "SKILL.md")
    facade = read(SKILL_DIR / "references" / "mcp-facade.md")
    client = read(SKILL_DIR / "scripts" / "portal_client.py")

    for contents in (skill, facade, client):
        assert "owner_token" not in contents
        assert "owner token" not in contents
    # Portals are keyed by the caller's agent identity, so no call carries a token.
    assert "keyed by your agent identity" in skill
    assert "no call carries a token" in skill


def test_quickstart_warp_note_is_vm_only() -> None:
    quickstart = read(ROOT / "docs" / "QUICKSTART.md")

    assert "Known runtime bug (hud-d5rcd)" not in quickstart
