"""Each HUD skill body still routes every intent in its checked fixture."""

import json
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/skill_routing_intents.json"
LINK = re.compile(r"^(?:\.\./)*[\w./-]+\.md(?:#[\w-]+)?$")


def anchors(markdown):
    """GitHub-style heading anchors of a Markdown document."""
    return {
        re.sub(r"[^\w\- ]", "", line.lstrip("#").strip().lower()).replace(" ", "-")
        for line in markdown.splitlines()
        if line.startswith("#")
    }


def problems(skill, intents):
    path = ROOT / ".claude/skills" / skill / "SKILL.md"
    body = path.read_text(encoding="utf-8")
    found = []
    for entry in intents:
        marker = entry["marker"]
        if marker not in body:
            found.append(f"{skill}: '{entry['intent']}' marker {marker!r} missing from SKILL.md")
            continue
        if LINK.match(marker):
            target, _, anchor = marker.partition("#")
            linked = (path.parent / target).resolve()
            if not linked.is_file():
                found.append(f"{skill}: '{entry['intent']}' links to missing {target}")
            elif anchor and anchor not in anchors(linked.read_text(encoding="utf-8")):
                found.append(f"{skill}: '{entry['intent']}' links to missing #{anchor}")
    return found


class SkillRoutingIntentTest(unittest.TestCase):
    def test_every_intent_is_routed_by_its_skill_body(self):
        fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
        skills = {k: v for k, v in fixture.items() if not k.startswith("_")}
        self.assertEqual(set(skills), {"th-hud-publish", "hud-projection"})
        found = [p for skill, intents in sorted(skills.items()) for p in problems(skill, intents)]
        self.assertEqual(found, [])

    def test_a_missing_marker_or_link_target_is_named(self):
        found = problems("hud-projection", [{"intent": "absent", "marker": "no such text"}])
        self.assertEqual(len(found), 1)
        self.assertIn("'absent'", found[0])
        guide = (ROOT / ".claude/skills/hud-projection/references/guide.md").read_text(encoding="utf-8")
        self.assertIn("mcp-client", anchors(guide))
        self.assertNotIn("no-such-heading", anchors(guide))


if __name__ == "__main__":
    unittest.main()
