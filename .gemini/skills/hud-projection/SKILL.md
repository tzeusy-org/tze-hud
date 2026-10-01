---
name: hud-projection
description: Use when an already-running Codex, Claude, opencode, or other LLM session should project itself onto the HUD, show its output or status on screen, attach to a text-stream portal, publish live transcript, poll HUD-originated operator input, acknowledge input, detach, or clean up. Trigger phrases — "project this session to the HUD", "attach this agent to HUD", "show this LLM session in a text-stream portal", "check HUD input". Do not use for terminal capture, PTY attachment, tmux scraping, process hosting, or direct runtime v1 MCP zone publishing; for one-shot zone publishing use th-hud-publish instead.
compatibility: Requires a production projection ingress surface (MCP or gRPC) that exposes the in-process ProjectionAuthority to external sessions. The production ingress is not yet shipped (tracked as hud-bq0gl.1). Until it lands, use the stdio component harness for local development only — it does not connect to the live runtime.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
  status: active
  last_reviewed: "2026-06-14"
---

# HUD Projection

Use this skill to opt an already-running LLM session into a governed tze_hud text-stream portal.

Hard boundaries:
- This is cooperative opt-in. The current session intentionally calls projection operations.
- This is not PTY, tmux, shell, stdin/stdout, or terminal byte-stream capture.
- The `ProjectionAuthority` runs **in-process** inside the tze_hud runtime (not as an external daemon). It owns projection state outside the LLM token context: HUD connection metadata, advisory portal lease identity, bounded transcript/window state, pending HUD input, acknowledgement state, lifecycle state, unread state, privacy classification, and reconnect bookkeeping.
- If a projection MCP surface is available, it is a facade into the runtime's in-process authority, not the runtime v1 MCP zone publishing bridge. **This facade has not shipped yet** — see hud-bq0gl.1.

## Source Of Truth

When changing behavior or resolving ambiguity, read:
- The code: `crates/tze_hud_projection/` (authority, session state) and the `portal_projection_*` tools in `crates/tze_hud_mcp/`. The old OpenSpec contract is history at git tag `pre-reset-2026-10-02`.

## Use When

- The user asks to "project this session to the HUD", "attach this agent to HUD", "show this LLM session in a text-stream portal", or "check HUD input".
- A Codex, Claude, opencode, or other provider session needs to publish explicit output/status to the HUD.
- The session needs to poll operator-submitted HUD input and acknowledge each input item as handled, deferred, or rejected.
- The session needs to detach or clean up its projection.

Do not use this skill for one-shot zone publishing; use `th-hud-publish` for that.

## Operation Contract

All requests include:
- `operation`
- `projection_id`
- `request_id`
- `client_timestamp_wall_us`

Owner-scoped operations after `attach` also include `owner_token`. Operator cleanup is the only non-attach operation that may instead use separate daemon authority.

The normative operations are:
- `attach`
- `publish_output`
- `publish_status`
- `get_pending_input`
- `acknowledge_input`
- `detach`
- `cleanup`

Read `references/operation-examples.md` for compact JSON examples of every operation, including Codex, Claude, and opencode attach examples.

## Workflow

1. **Attach once.** Choose a stable `projection_id`, set `provider_kind` to `codex`, `claude`, `opencode`, or `other`, and include a human-readable `display_name`. Default missing or uncertain classification to `private`.
2. **Store the owner token securely and immediately.** A successful attach returns `owner_token`; no other operation response will ever return it. Store it in a tool-call result or session variable, never in transcript text, assistant-visible output, or log lines. If it is lost before detach, you must treat the projection as unrecoverable and wait for operator cleanup or TTL expiry. Do not request the token again — there is no retrieval path.
3. **Publish intentionally.** Call `publish_output` for assistant-visible transcript/status fragments and `publish_status` for lifecycle updates such as `active`, `degraded`, or `detached`.

   **Accepted `lifecycle_state` values** (snake_case strings; any other value is rejected):
   - `attached` — session is attached but not yet actively working
   - `active` — session is running / producing output
   - `degraded` — session is blocked, slow, or in a degraded state
   - `hud_unavailable` — session cannot reach the HUD
   - `detached` — session has detached cleanly
   - `cleanup_pending` — projection is pending removal
   - `expired` — projection TTL has elapsed

   **Accepted `output_kind` values** (snake_case strings; defaults to `assistant` when omitted; any other value is rejected):
   - `assistant` *(default)* — normal assistant message / transcript fragment
   - `tool` — tool call or tool result
   - `status` — status or progress update
   - `error` — error output
   - `other` — any other kind
   - `viewer` — *reserved for the runtime's echo of the operator's own reply; rejected if published by an adapter*
4. **Poll HUD input compactly.** Call `get_pending_input` with small `max_items` and `max_bytes`. Treat returned input as semantic operator-submitted text, not terminal keystrokes.
5. **Acknowledge every input item.** Use `acknowledge_input` with `handled`, `deferred`, or `rejected`. Use `not_before_wall_us` only with `deferred`.
6. **Detach on normal exit.** Call `detach` with a bounded reason when the session is done projecting.
7. **Cleanup stale state when appropriate.** Use owner cleanup with `owner_token`; operator cleanup uses a separate daemon authority and must not expose private projection content.

## Production Ingress (Pending)

The production ingress — an MCP or gRPC surface that routes cooperative projection operations into the runtime's in-process `ProjectionAuthority` — has not shipped yet. It is tracked as **hud-bq0gl.1**.

Until hud-bq0gl.1 lands, there is no live path from an external LLM session to the running HUD. The stdio component harness (`crates/tze_hud_projection` binary) can be used for local development and testing of the protocol but does NOT connect to the running runtime.

When the production ingress ships, the facade may expose one dispatcher tool such as `projection_operation` or one tool per operation. The JSON payloads in `references/operation-examples.md` remain the contract either way. `settings.template.json` shows the expected configuration shape.

See `references/mcp-facade.md` for facade requirements, boundary rules, and a configuration template.

## Safety Notes

- Keep operation responses bounded; do not request unbounded transcripts, inbox history, or raw scene state.
- Do not publish secrets or owner tokens into the transcript window or any user-visible output.
- Treat `owner_token` as attach-only response material; it must never be returned by publish, input, acknowledgement, detach, or cleanup responses. If a response includes `owner_token` outside an `attach` success, treat that as a protocol error and do not use or forward the value.
- **Owner-token loss is unrecoverable.** If the token is lost (session crash, transcript cleared), there is no retrieval path. The only recovery options are operator cleanup (requires separate operator authority, not the owner token) or waiting for the projection's TTL to expire. Do not attempt to re-attach with the same `projection_id` without the idempotency key — the authority will reject it with `PROJECTION_ALREADY_ATTACHED`.
- **Do not embed `owner_token` in `publish_output` text, `status_text`, `ack_message`, or `reason` fields.** These fields are readable by audit records and portal rendering; tokens in them constitute a credential leak.
- Treat `PROJECTION_UNAUTHORIZED`, `PROJECTION_TOKEN_EXPIRED`, and `PROJECTION_STATE_CONFLICT` as hard stops unless the user explicitly authorizes reattach or operator cleanup.
- If the runtime restarts, prior transcript text, pending input text, owner tokens, and cached lease identity are gone. Attach again and receive a fresh owner token — the old token is permanently invalid after a restart.
