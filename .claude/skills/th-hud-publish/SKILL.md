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

One call per intent. Tokens are each canonical flow's model-visible cost
(`docs/api.md`); keys and ids add a few. Detail: [full guide](references/guide.md).

| Intent | Call | Tokens (flow) |
|---|---|--:|
| What can I use? | `hud_surfaces {}` | 126 (discover) |
| Notify | `hud_publish {"surface":"zone:notification-area","content":{"title":"Tests","body":"Passed","urgency":0},"key":"<session>:tests","ttl_ms":10000}` | 39 (zone_publish) |
| Text, color | `content` is `"Build passed"`, or `{"r":0.05,"g":0.1,"b":0.2,"a":1.0}` on a color zone | — |
| Ask, wait for the answer | `content.actions:[{"label":"Run","callback_id":"<session>:q1:run"},…]` plus top-level `"wait_ms":30000` | 77 (notification_choice_wait) |
| Typed answer | an action `{"label":"Other…","reply":"<session>:q1:other"}` | — |
| Progress, copy, link | `content.progress` 0–1; action `{"label":"Copy","copy":"…"}`; action `{"label":"PR","url":"https://…"}` | — |
| Update a widget | `hud_publish {"surface":"widget:<name>","params":{…discovered params…}}` | 22 (widget_publish) |
| Status entry | `{"entries":{"<session>":"testing"}}` on a status bar, with a `key` | — |
| Extend one card | republish its `key`. `hud_hold {"surface"…}` extends every card this agent holds there | — |
| Collect input | `hud_input {"wait_ms":30000}`, then `hud_input {"ack":["<id>"]}` | 114 (notification_choice) |
| Remove my card | `hud_clear {"surface":"zone:notification-area","key":"<session>:tests"}` | — |

## Routing

- Discover first: names are per instance. Use a widget only when it is
  allocated to this work.
- A card is one milestone or one question in a couple of short lines; link
  to chat, a PR or an artifact for depth.
- Session transcript, portal or replies → [hud-projection](../hud-projection/SKILL.md).
- Screenshots, logs, updates, Windows validation → [user-test](../user-test/SKILL.md).
- Tools absent → [connection setup](references/connection.md), or the
  [CLI fallback](references/guide.md#cli-fallback) (`scripts/publish.py`).
- HUD unreachable or rejecting → continue in chat; say so once.
- Owner presentation rules in [AGENTS.md](../../../AGENTS.md) win; prefer
  finite milestone notifications.

## Rules

- Offer buttons only when this session will consume and route the answer.
- At most four actions per card, and at most one of them a reply. Callback
  ids name the session, question and generation.
- An answered `wait_ms` returns a consumed `action`, so don't ACK it. A
  `gone` result, a timeout or a dismiss is not consent.
- ACK each handled `hud_input` id once. Validate the callback generation even
  after the card is gone.
- Never send a keyless `hud_clear` on a shared zone: it clears all of this
  agent's content there.
- Set a finite `ttl_ms`. Send no heartbeats and no empty polling loops.
- If a reply carries `clipped`, shorten the text and republish the same key,
  or link out. On `kind:"displaced"`, republish only if it still matters.
- Errors are `{code,hint}`: follow the hint and rediscover on missing names.
  Stop on `NOT_ALLOWED`. On `SAFE_MODE_ACTIVE`, stop mutating; polling still
  works.
- Never replay an uncertain publish blindly.
