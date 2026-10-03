---
name: hud-projection
description: >-
  Use when an already-running LLM session should project itself onto the HUD,
  attach to a text-stream portal, publish live output, consume HUD input, or
  detach. Trigger phrases include project this session to the HUD, attach this
  agent to HUD, and check HUD input. Not for terminal capture, process hosting,
  or one-shot zone publishing.
compatibility: >-
  Requires the tze_hud windowed runtime with MCP enabled. The MCP bearer is
  your agent's PSK, and that agent's `allow` list must include `portal`.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
  status: active
  last_reviewed: "2026-10-02"
---

# HUD Projection

Opt an already-running LLM session into a tze_hud text-stream portal: the
session publishes its output, the human reads it on the HUD and types replies
into the portal composer, and the session collects those replies.

Hard boundaries:
- Cooperative opt-in. The session calls the tools on purpose.
- Not PTY, tmux, shell, or terminal capture.
- The portal authority runs in-process in the runtime and holds only
  ephemeral state (visible transcript window, pending input, lifecycle). The
  session owns its own history.

## Tools

A portal is the surface `portal:<projection_id>`. It uses the same MCP verbs as
zones and widgets (`docs/api.md`):

| Step | Call |
|---|---|
| Attach + publish | `hud_publish {"surface": "portal:my-session", "content": "Working on it", "status": "active"}` |
| Publish more | `hud_publish {"surface": "portal:my-session", "content": "Tests pass. Ship it?", "expects_reply": true}` |
| Collect replies | `hud_input {"wait_ms": 30000}` → `{"items":[{"id":"i7","s":"portal:my-session","text":"yes"}],"remaining":0}` |
| Ack + keep polling | `hud_input {"ack": ["i7"], "wait_ms": 30000}` |
| Detach | `hud_clear {"surface": "portal:my-session"}` |

- The first `hud_publish` to a portal attaches it (`display_name` is optional
  and defaults to the id). The portal is keyed by your agent identity (the
  PSK); no call carries a token.
- `content` **appends** an output fragment. Send only the new text each turn.
- `key` coalesces: publishes with the same key replace each other in place
  (progress lines).
- `status` sets the lifecycle state: `attached`, `active`, `degraded`,
  `hud_unavailable`, or `detached`.
- `expects_reply: true` arms the composer.
- A portal publish rejects `ttl_ms` and `delay_ms` with `INVALID_ARGUMENT`;
  portal lifetime is set only by `hud_hold`.
- `hud_input` returns input from every surface you hold, oldest first,
  including notification action presses. Unacked items are redelivered, so
  ack each one once handled; the ack rides on your next poll.
- `hud_surfaces` lists your attached portals with `state` and
  `pending_input`.
- A portal you stop calling degrades after 30 s and is reclaimed 30 s later,
  transcript included. Publishing or polling keeps it; for quiet stretches
  call `hud_hold {"surface": "portal:my-session", "ttl_ms": 600000}`
  (`ttl_ms: 0` holds until `hud_clear`).
- If the runtime restarts, the next `hud_publish` attaches a fresh portal;
  republish whatever context the human needs.

Errors are tool results with `isError: true` and `{"code", "hint"}`. Portal
rejections use the same codes (`docs/api.md` lists them all).

## Choosing a target runtime

- **A human's screen** (e.g. tzehouse): endpoint and PSK per that host.
  `eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"`.
- **The autonomous testhost** (`hud-windows` VM): for noninteractive work.

  ```bash
  eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
  # exports HUD_MCP_URL and TZE_HUD_PSK, starting the VM/HUD task if down
  ```

  The VM renders with WARP (no GPU fidelity).

## Deterministic client

Outside an MCP client, drive the same calls with
[`scripts/portal_client.py`](scripts/portal_client.py):

```bash
CLIENT=.claude/skills/hud-projection/scripts/portal_client.py
python3 $CLIENT publish --id my-session --display-name "My Session" --state active --text "hello"
python3 $CLIENT publish --id my-session --text "Ship it?" --expects-reply
python3 $CLIENT poll    --wait-ms 30000 --rounds 6      # NDJSON items; exit 3 if none
python3 $CLIENT ack     --input-id i7
python3 $CLIENT status  --id my-session --state degraded
python3 $CLIENT surfaces
python3 $CLIENT clear   --id my-session
```

It reads `HUD_MCP_URL` and the PSK from `HUD_PSK` (or `TZE_HUD_PSK`,
`HUD_MCP_PSK`, `MCP_TEST_PSK`). `poll --ack` acks every item it prints. For a
one-command connectivity trial (attach, greeting, poll), use
`.claude/skills/user-test/scripts/portal_trial.sh`.

## Setup

Point your MCP client at the runtime (see `settings.template.json`) with your
agent's PSK as the bearer. `scripts/quickstart.sh` pairs that PSK as
`[agents.claude]` with `allow = ["*"]` in the HUD's `agents.toml`.

## Source of truth

- MCP verbs: `crates/tze_hud_mcp/src/tools.rs`; contract: `docs/api.md`.
- Portal authority: `crates/tze_hud_projection/`, bridged by
  `crates/tze_hud_runtime/src/portal_projection_driver.rs`.
- [References](references/mcp-facade.md): wiring and boundary rules.

## Safety

- Don't publish secrets into the portal.
- Keep fragments small; never resend the whole transcript.
- Treat `NOT_ALLOWED` on a portal as a stop: your `[agents.<id>] allow`
  list lacks `portal`; ask the user. (Portal ids are per agent, so another
  agent's `portal:<id>` never blocks yours.)
