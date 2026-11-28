"""Each HUD skill body still routes every intent in its checked fixture."""

import json
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/skill_routing_intents.json"
LINK = re.compile(r"^(?:\.\./)*[\w./-]+\.md(?:#[\w-]+)?$")


def anchors(markdown):
    """GitHub-style heading anchors of a Markdown document, outside code fences."""
    found, fenced = set(), False
    for line in markdown.splitlines():
        if line.lstrip().startswith(("```", "~~~")):
            fenced = not fenced
        elif line.startswith("#") and not fenced:
            title = line.lstrip("#").strip().lower()
            found.add(re.sub(r"[^\w\- ]", "", title).replace(" ", "-"))
    return found


def problems(skill, intents):
    path = ROOT / ".claude/skills" / skill / "SKILL.md"
    body = path.read_text(encoding="utf-8")
    found = []
    for entry in intents:
        marker = entry["marker"]
        count = body.count(marker)
        if count != 1:
            # A repeated marker could not notice its own row being deleted.
            found.append(
                f"{skill}: '{entry['intent']}' marker {marker!r} appears {count} times in SKILL.md"
            )
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
        for skill, intents in skills.items():
            self.assertTrue(intents, f"{skill} lists no intents")
        found = [p for skill, intents in sorted(skills.items()) for p in problems(skill, intents)]
        self.assertEqual(found, [])

    def test_a_missing_marker_or_link_target_is_named(self):
        found = problems("hud-projection", [{"intent": "absent", "marker": "no such text"}])
        self.assertEqual(len(found), 1)
        self.assertIn("'absent'", found[0])
        twice = problems("hud-projection", [{"intent": "twice", "marker": "portal:<id>"}])
        self.assertIn("times in SKILL.md", twice[0])
        guide = (ROOT / ".claude/skills/hud-projection/references/guide.md").read_text(encoding="utf-8")
        self.assertIn("mcp-client", anchors(guide))
        self.assertNotIn("no-such-heading", anchors(guide))
        self.assertNotIn("claude-code-run-with-run_in_background-it-exits-and-wakes-you-on-the", anchors(guide))


if __name__ == "__main__":
    unittest.main()
