---
name: hud-projection
description: >-
  Use when an already-running LLM session should project itself onto the HUD,
  attach to a text-stream portal, publish live output, consume HUD input, or
  detach. Trigger phrases include project this session to the HUD, attach this
  agent to HUD, and check HUD input. Not for terminal capture, process hosting,
  or one-shot zone publishing.
compatibility: >-
  Requires the tze_hud windowed runtime with MCP enabled and this host paired
  (PSK file under ~/.config/tze-hud). The agent's `allow` list must include
  `portal`.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
  status: active
  last_reviewed: "2026-11-28"
---

# HUD Projection

The session publishes its output to a text-stream portal `portal:<id>`, the
human replies in the portal composer, and the session collects the replies.
The portal costs 137 model-visible tokens for attach, publish, hold, poll,
ACK and clear (`docs/api.md` Token budgets). Detail for every row is in the
[full guide](references/guide.md).

| Intent | Call |
|---|---|
| Attach + publish | `hud_publish {"surface":"portal:<id>","content":"Working on it","status":"active"}` |
| Ask for a reply | `hud_publish {"surface":"portal:<id>","content":"Ship it?","expects_reply":true}` |
| Collect replies | `hud_input {"wait_ms":30000}` |
| ACK + keep polling | `hud_input {"ack":["i7"],"wait_ms":30000}` |
| Stay attached while quiet | `"ttl_ms":600000` on a publish, or `hud_hold {"surface":"portal:<id>","ttl_ms":600000}` |
| Detach | `hud_clear {"surface":"portal:<id>"}` |
| Wait without model calls | run `scripts/portal_client.py poll --wait-ms 30000 --rounds 120 --ack` in the background ([waiting](references/guide.md#waiting-for-a-human-reply)) |
| Ask, keep working | run `scripts/portal_client.py ask --title … --action Ship=ship` in the background ([background ask](references/background-ask.md)) |

## Routing

- A one-shot card, status or widget → [th-hud-publish](../th-hud-publish/SKILL.md).
- Pairing, `HUD_HOST`, the `.mcp.json` stdio adapter → [connect](references/guide.md#connect)
  and [MCP client](references/guide.md#mcp-client).
- No MCP tools available → the [deterministic client](references/guide.md#deterministic-client).
- Mirror tool progress without model calls (`TZE_HUD_MIRROR=1`) →
  [Claude Code hooks](references/claude-code-hooks.md); usage gauge →
  [usage](references/claude-code-usage.md).
- The **Go to** button on Windows cards, and window titles → [full guide](references/guide.md).
- Code owners and wiring → [MCP facade](references/mcp-facade.md).

## Rules

- Projection is opt-in. It is not PTY, tmux, shell or terminal capture.
- `content` appends: send only the new text, never the whole transcript. The
  same `key` replaces that line in place.
- Put long reports, diffs and reviews on a page built for reading, and publish
  its link.
- Check `unread:N` before raising a separate notification.
- ACK each handled input once. Unacked input repeats.
- Never run a background waiter and model polling at once.
- A silent portal degrades after 30 s and is reclaimed 30 s later. After
  `reattached` or a restart, republish the context the human needs.
- Never publish secrets.
- `NOT_ALLOWED` on a portal means the `allow` list lacks `portal`: stop and
  ask the user.
- Errors are `{code,hint}`: follow the hint.
