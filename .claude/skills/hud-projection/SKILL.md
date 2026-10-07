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
  last_reviewed: "2026-10-04"
---

# HUD Projection

Opt an already-running LLM session into a tze_hud text-stream portal: the
session publishes its output, the human reads it on the HUD and types replies
into the portal composer, and the session collects those replies.

Hard boundaries:
- Cooperative opt-in. The session calls the tools on purpose, or the owner
  explicitly installs the optional [Claude Code hooks](references/claude-code-hooks.md)
  to mirror safe tool progress and opted-in final replies without model calls.
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

## Connect

Set `HUD_HOST` to the HUD's host. The client derives `http://$HUD_HOST:9090/mcp`
and reads your PSK from `~/.config/tze-hud/$HUD_HOST.psk`. If that file is
missing, pair once with the 6-digit code the HUD shows on screen
(`.claude/skills/user-test/scripts/hud_pair.py --code <code>`); the PSK is
written to the file and never printed. The paired agent gets `allow = ["*"]`,
which includes `portal`.

## Deterministic client

Outside an MCP client, drive the same calls with
`.claude/skills/hud-projection/scripts/portal_client.py`:

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

`poll --ack` acks every item it prints.

## Optional Claude Code mirroring

[Claude Code hooks](references/claude-code-hooks.md) provides a complete opt-in
settings block for the main conversation on a POSIX Claude host. It uses the
same paired client and portal; it installs nothing automatically. Progress
contains fixed tool labels, never tool inputs/results. Final replies are the
owner's explicit privacy opt-in. The reference describes finite quiet holds,
bounded delivery, prompt ordering limits and the required live owner proof.
Explicit model/client publication and input collection remain available.

## MCP client

The repo's `.mcp.json` already defines `tze-hud` (template:
`mcp.template.json`). Set `HUD_HOST` to the **bare** host (no port or scheme;
the URL appends `:9090`); its `headersHelper` sends the paired PSK as the
bearer, so no secret sits in the config. Claude Code runs the helper from the
project dir after you trust the workspace.

## Source of truth

- MCP verbs: `crates/tze_hud_mcp/src/tools.rs`; contract: `docs/api.md`.
- Portal authority: `crates/tze_hud_projection/`, bridged by
  `crates/tze_hud_runtime/src/portal_projection_driver.rs`.
- `.claude/skills/hud-projection/references/mcp-facade.md`: wiring and boundary rules.

## Safety

- Don't publish secrets into the portal.
- Keep fragments small; never resend the whole transcript.
- Treat `NOT_ALLOWED` on a portal as a stop: your `[agents.<id>] allow`
  list lacks `portal`; ask the user. (Portal ids are per agent, so another
  agent's `portal:<id>` never blocks yours.)
