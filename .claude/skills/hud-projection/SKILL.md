---
name: hud-projection
description: >-
  Use when an already-running LLM session should project itself onto the HUD,
  attach to a text-stream portal, publish live output, consume HUD input, or
  detach. Trigger phrases include project this session to the HUD, attach this
  agent to HUD, and check HUD input. Not for terminal capture, process hosting,
  or one-shot zone publishing.
compatibility: >-
  Requires the tze_hud windowed runtime with MCP enabled. Projection operations
  are Resident tools; the configured resident principal, PSK, and MCP bearer
  must match.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
  status: active
  last_reviewed: "2026-07-17"
---

# HUD Projection

Use this skill to opt an already-running LLM session into a governed tze_hud text-stream portal.

Hard boundaries:
- This is cooperative opt-in. The current session intentionally calls projection operations.
- This is not PTY, tmux, shell, stdin/stdout, or terminal byte-stream capture.
- The `ProjectionAuthority` runs **in-process** inside the tze_hud runtime (not as an external daemon). It owns only ephemeral runtime state: HUD connection metadata, advisory portal lease identity, the bounded visible transcript window, pending HUD input, acknowledgement state, lifecycle state, unread state, privacy classification, and grace-period reconnect bookkeeping. It does not own durable transcript history; the adapter/client does.
- The `portal_projection_*` tools are a projection facade into that in-process authority — distinct from the runtime's zone/widget publishing tools (`th-hud-publish`). They are Resident tools; reach them as the resident principal (see [MCP facade](references/mcp-facade.md) for the auth wiring and code paths).

## Choosing A Target Runtime

Projection needs a live windowed runtime with MCP enabled. Two standing targets:

- **A human's screen** (e.g. tzehouse) — when the point is a person seeing the
  projection. Endpoint/PSK per that host's config.
- **The autonomous testhost** (`hud-windows` VM on sentinel Proxmox) — for any
  local noninteractive work that needs a real projection surface (integration
  tests, portal exemplar runs, transcript-render validation). Resolve and
  self-heal it with:

  ```bash
  eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
  # exports HUD_MCP_URL and TZE_HUD_MCP_RESIDENT_PRINCIPAL (== PSK == bearer),
  # starting the VM and/or HUD task if down
  ```

  Caveats on the VM: WARP rendering (no GPU fidelity). Normal GPU desktop
  startup materializes every configured `[[tabs]]` entry. VM-only fallback: if
  the WARP test VM returns `No active tab`, call MCP
  `create_tab {"name":"Main"}` once before portal work. A registered agent
  (`agent-alpha`) is pre-provisioned for resident gRPC sessions.

## Deterministic Client (Preferred Driver)

Do not hand-roll MCP calls: [`scripts/portal_client.py`](scripts/portal_client.py) wraps all eight
operations as subcommands with the contract boilerplate, auth, and owner-token
custody built in. Resolve env first (either target), then drive:

```bash
eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"   # or hud_vm_env.sh
CLIENT=.claude/skills/hud-projection/scripts/portal_client.py
python3 $CLIENT list
python3 $CLIENT attach  --projection-id my-session --display-name "My Session"
python3 $CLIENT publish --projection-id my-session --text "hello from the session"
python3 $CLIENT status  --projection-id my-session --state active --text "working"
python3 $CLIENT poll    --projection-id my-session --wait-ms 30000 --rounds 6 --ack handled
python3 $CLIENT ack     --projection-id my-session --input-id input-1 --state handled --message "done"
python3 $CLIENT detach  --projection-id my-session
```

Owner tokens are written to `~/.local/state/tze_hud/portal-tokens/<id>.token`
(0600, outside any repo; override with `PORTAL_TOKEN_DIR`) and are redacted
from all output — satisfying the "store immediately, never in transcript"
rule below without manual handling. `poll` prints received items as NDJSON
and exits 3 when no input arrived (deterministic signal); `--ack handled`
auto-acknowledges receipt, but prefer explicit `ack` with a meaningful message
after actually acting on an input. Re-running `attach` with the same
projection id and the same non-empty idempotency key is safe: it rotates the
owner token, atomically invalidates the prior token, and replaces the token
file without extending the projection's original expiry deadline.

Client-authored continuity is separately retained at
`~/.local/state/tze_hud/portal-continuity/<id>.json` (0700 parent directory,
0600 file, atomic replacement; override with `PORTAL_CONTINUITY_DIR`). The tail
is bounded to the newest 64 records and 64 KiB of canonical UTF-8 record data.
It contains only output text, kind, classification, `logical_unit_id`, and an
optional `coalesce_key`, plus the original attach idempotency key. It never
contains owner tokens, pending input, acknowledgements, or viewer-authored HUD
turns. Use `continuity-path` to inspect its location and `continuity-clear` for
explicit local deletion; detach and remote cleanup do not silently erase it.
Schema-invalid files are reduced to bounded hash/size/reason metadata rather
than preserving their raw private payload, and per-projection process locking
serializes attach, publish outcome/rollback, and clear transactions.

For a one-command connectivity trial (attach + greeting + poll), use
`.claude/skills/user-test/scripts/portal_trial.sh`.

**Wire dialect:** standard MCP `tools/call` is the primary supported dialect.
`portal_client.py` uses it first and falls back to the legacy bare-method shape
(`"method": "portal_projection_attach"`) only when an older server reports
`tools/call` as method-not-found. Both dialects reach the same tool dispatch and
capability gates.

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

Projection-scoped requests include:
- `operation`
- `projection_id`
- `request_id`
- `client_timestamp_wall_us`

`list` is caller-scoped: its MCP tool takes no projection ID or owner token, and
the runtime generates its authority request metadata. It returns at most eight
summaries owned by the resident principal, each limited to `projection_id`,
`display_name`, lifecycle, unread count, and pending-input count. It never
returns transcript/input text, owner tokens, lease data, or another principal's
sessions, and it never changes lifecycle state.

Owner-scoped operations after `attach` also include `owner_token`. Operator cleanup is the only non-attach operation that may instead use separate daemon authority.

The normative operations are:
- `list`
- `attach`
- `publish_output`
- `publish_status`
- `get_pending_input`
- `acknowledge_input`
- `detach`
- `cleanup`

Read [operation examples](references/operation-examples.md) for compact JSON examples of every operation, including Codex, Claude, and opencode attach examples.

## Workflow

1. **Reconcile if needed.** Call `list` after an interrupted flow to see only your bounded portal summaries. It is discovery, not recovery orchestration: choose a normal attach/detach/cleanup action explicitly rather than expecting `list` to alter a portal.
2. **Attach once.** Choose a stable `projection_id`, set `provider_kind` to `codex`, `claude`, `opencode`, or `other`, and include a human-readable `display_name`. Default missing or uncertain classification to `private`.
3. **Store the owner token securely and immediately.** Every successful attach returns `owner_token`; no non-attach operation response will ever return it. Store it in a tool-call result or session variable, never in transcript text, assistant-visible output, or log lines. The deterministic client persists the original idempotency key, reuses it for authenticated attach, stores the rotated token, then replays its bounded authored tail before attach returns. Before grace expiry this is idempotent against the authority's retained window; after a runtime restart or grace expiry it reconstructs a fresh portal without pretending the dead runtime state survived. A missing or unrelated key is rejected and does not rotate live ownership.
4. **Publish intentionally.** Call `publish_output` for assistant-visible transcript/status fragments and `publish_status` for lifecycle updates such as `active`, `degraded`, or `detached`.

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
5. **Poll HUD input compactly.** Call `get_pending_input` with small `max_items` and `max_bytes`. Treat returned input as semantic operator-submitted text, not terminal keystrokes.
6. **Acknowledge every input item.** Use `acknowledge_input` with `handled`, `deferred`, or `rejected`. Use `not_before_wall_us` only with `deferred`.
7. **Detach on normal exit.** Call `detach` with a bounded reason when the session is done projecting.
8. **Cleanup stale state when appropriate.** Use owner cleanup with `owner_token`; operator cleanup uses a separate daemon authority and must not expose private projection content.

**Token-efficient use** (this is meant to be viable as a primary session interface):
- `publish_output` **appends** — send only the new fragment each turn, never the whole transcript. The client retains only its bounded authored continuity tail; the authority retains only the bounded in-memory visible window while that runtime lives.
- Use `coalesce_key` for streaming/progress lines so repeated publishes collapse in place instead of piling up.
- To await a reply, prefer one `get_pending_input` with `wait_ms` (long-poll) over a busy-poll loop; keep `max_items`/`max_bytes` small.

## Production Ingress (Wired)

The full contract is wired in-process. When the runtime runs with MCP enabled
(`mcp_port > 0`), each operation maps to one Resident tool; call it as the
resident principal (bearer == PSK == `TZE_HUD_MCP_RESIDENT_PRINCIPAL`). Every op
maps to `portal_projection_<op>`, except `publish_output` →
`portal_projection_publish`. `cleanup` also accepts operator authority.
Published `attach`/`publish_output` content renders for both adapter families
(exemplar gRPC and the in-process cooperative driver).

The stdio component harness
(`crates/tze_hud_projection/src/bin/projection_authority.rs`) is for local
protocol testing and audit-record inspection only — it runs the authority in an
isolated process with **no** connection to the live runtime, so its output never
reaches the screen. Use the MCP facade for real on-screen projection.

References:
- [Operation examples](references/operation-examples.md) — per-operation JSON payloads (the contract).
- [MCP facade](references/mcp-facade.md) — facade requirements, boundary rules, auth wiring, code paths, and the config template.
- `settings.template.json` — expected configuration shape.

## Safety Notes

- Keep operation responses bounded; do not request unbounded transcripts, inbox history, or raw scene state.
- Treat `list` as content-free caller-scoped discovery only. It is capped at eight summaries, cannot reveal another resident principal's projections, and never performs lifecycle, lease, or token management.
- Do not publish secrets or owner tokens into the transcript window or any user-visible output.
- Treat `owner_token` as attach-only response material; it must never be returned by publish, input, acknowledgement, detach, or cleanup responses. If a response includes `owner_token` outside an `attach` success, treat that as a protocol error and do not use or forward the value.
- **Owner-token loss requires authenticated rotation, never retrieval.** Re-attach through the Resident MCP surface with the same non-empty idempotency key to receive a fresh token. This immediately invalidates every previously issued token and does not extend the original expiry deadline. Without the matching key, the authority rejects the attach with `PROJECTION_ALREADY_ATTACHED`; after expiry, attach creates a new session under the normal authorization path.
- **Do not embed `owner_token` in `publish_output` text, `status_text`, `ack_message`, or `reason` fields.** These fields are readable by audit records and portal rendering; tokens in them constitute a credential leak.
- Treat `PROJECTION_UNAUTHORIZED`, `PROJECTION_TOKEN_EXPIRED`, and `PROJECTION_STATE_CONFLICT` as hard stops unless the user explicitly authorizes reattach or operator cleanup.
- If the runtime restarts, its prior transcript window, pending input text, owner tokens, and cached lease identity are gone. Run `attach` again: the client reuses the original idempotency key, receives and stores a fresh owner token, then replays only its bounded client-authored `portal-continuity` tail with the original `logical_unit_id` and `coalesce_key` values. Pending/viewer-authored input is never replayed. A second replay is safe because logical-unit identity remains idempotent.
