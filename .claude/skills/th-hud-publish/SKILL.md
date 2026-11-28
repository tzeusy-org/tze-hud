---
name: th-hud-publish
description: Publish notifications, short text, status entries, colors, or typed widget parameters to a paired tze_hud display over MCP. Use for "show this on my HUD", "notify me when tests finish", or "ask me on screen". For session transcripts and portal replies, use hud-projection.
metadata:
  owner: tze
  authors:
    - Claude
    - OpenAI Codex
  status: active
  last_reviewed: "2026-11-28"
---

# HUD Publish

One call per intent. Tokens are measured model-visible costs (`docs/api.md`
Token budgets). Detail for every row: [full guide](references/guide.md).

| Intent | Call | Tokens |
|---|---|--:|
| What can I use? | `hud_surfaces {}` | 126 |
| Notify | `hud_publish {"surface":"zone:notification-area","content":{"title":"Tests","body":"Passed","urgency":0},"key":"<session>:tests","ttl_ms":10000}` | 39 |
| Ask, wait for the answer | add `"actions":[{"label":"Run","callback_id":"<session>:q1:run"},…]` to `content` and `"wait_ms":30000` | 77 |
| Typed answer | an action `{"label":"Other…","reply":"<session>:q1:other"}` | — |
| Progress / copy / link | `content.progress` 0–1; action `{"label":"Copy","copy":"…"}`; action `{"label":"PR","url":"https://…"}` | — |
| Update a widget | `hud_publish {"surface":"widget:<name>","params":{…discovered params…}}` | 22 |
| Status entry | `{"entries":{"<session>":"testing"}}` on a status bar, with a `key` | — |
| Extend | `hud_hold {"surface":"…","ttl_ms":15000}` | — |
| Collect input | `hud_input {"wait_ms":30000}`, then `hud_input {"ack":["<id>"]}` | 114 for ask + answer + ACK |
| Remove my card | `hud_clear {"surface":"zone:notification-area","key":"<session>:tests"}` | — |

## Routing

- Start with `hud_surfaces`; zone, widget and param names are per instance.
- Card = one milestone or one question, at most a couple of short lines. Put
  depth in chat, a PR or an artifact, and link to it.
- Use a widget only when it is allocated to this work.
- Session transcript, portal or replies → [hud-projection](../hud-projection/SKILL.md).
- Screenshots, logs, updates, Windows validation → [user-test](../user-test/SKILL.md).
- Tools absent → [connection setup](references/connection.md), or the
  [CLI fallback](references/guide.md#cli-fallback) (`scripts/publish.py`).
- HUD unreachable or rejecting → continue in chat; say so once.
- Owner presentation rules in [AGENTS.md](../../../AGENTS.md) win; prefer
  finite milestone notifications.

## Rules

- At most four actions per card, one of them a reply. Callback ids name the
  session, question and generation.
- An answered `wait_ms` returns a consumed `action`, so don't ACK it.
- A `gone` result, a timeout or a dismiss is not consent.
- ACK each handled `hud_input` id once. Validate the callback generation even
  after the card is gone.
- Never send a keyless `hud_clear` on a shared zone: it clears all of this
  agent's content there.
- `hud_hold` has no key: it can extend sibling cards.
- Set a finite `ttl_ms`. Send no heartbeats and no empty polling loops.
- `clipped` in a reply: shorten and republish the same key, or link out.
  `kind:"displaced"`: republish only if it still matters.
- Errors are `{code,hint}`: follow the hint, rediscover on missing names, and
  stop on `NOT_ALLOWED` or `SAFE_MODE_ACTIVE`.
- Never replay an uncertain publish blindly.
