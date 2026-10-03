# API

The reference for the agent-facing API. Changing it is a deliberate decision:
update this file, `invariants.md`, and the token-footprint baseline in the
same PR.

## Shape

Each lifecycle stage uses one verb, and both planes use the same verbs.
Each verb works on a **surface**: a string the runtime hands out at
discovery.

| Surface | Form | Plane |
|---|---|---|
| Zone | `zone:<name>`, e.g. `zone:subtitle` | MCP, gRPC |
| Widget instance | `widget:<name>`, e.g. `widget:gauge` | MCP, gRPC |
| Portal projection | `portal:<projection_id>` | MCP |
| Tile | `tile:<id>` (runtime-assigned) | gRPC |

| Stage | MCP tool | gRPC message | Notes |
|---|---|---|---|
| Discover | `hud_surfaces` | `SessionEstablished` + `SceneSnapshot` | Lists surfaces the agent may use and what it holds now |
| Claim + fill | `hud_publish` | `Publish` (zones, widgets); `ClaimTile` (tiles) | Claiming is implicit in the first publish |
| Update | `hud_publish` | `Publish` / `MutationBatch` | Latest wins |
| Interact | `hud_input` | `EventBatch` (pushed) | MCP pulls input and acks it in the same call |
| Hold | `hud_hold` | `Hold` | Renews without resending content |
| Release | `hud_clear` | `Clear` | |
| Reclaim | none (runtime) | `Reclaimed` (pushed while connected) | Runs on expiry, disconnect, or human override; a disconnect has no live session to push to |

MCP `tools/list` has five tools. The resident tile API is gRPC only.

**Viewer dismiss.** Hovering an agent tile or portal shows a close button
(`tile.close_button.*` tokens). Pressing it reclaims the tile's lease on the
spot, with no agent round trip: a gRPC agent gets `Reclaimed{OVERRIDE}` for
`tile:<id>`. A dismissed portal is detached; the agent's next `hud_publish` to
`portal:<id>` attaches a fresh one, `hud_hold` answers `NOT_HELD` and
`hud_clear` succeeds as a no-op.

## MCP tools

Standard MCP over JSON-RPC 2.0 (HTTP POST): `initialize`,
`notifications/initialized` (no response), `tools/list`, and `tools/call`.
`ping` answers `{}`. There are no other methods. A result is one text content block of compact
JSON. Agents never send geometry, styling, or z-order. Times are
milliseconds, named `*_ms`, everywhere.

**`hud_surfaces`** has no params. It returns one compact entry per surface:

```json
{"surfaces":[
  {"s":"zone:subtitle","accepts":"text"},
  {"s":"zone:notification-area","accepts":"notification","held":true,"expires_in_ms":4200},
  {"s":"widget:gauge","params":{"level":"f32 0..1","label":"string"}},
  {"s":"portal:claude-main","state":"attached","pending_input":1}
]}
```

It includes nothing the model doesn't act on: no UUIDs, geometry, or
timestamps. Holdings are the entries with `held: true`; `held` is omitted
otherwise.

**`hud_publish`** `{surface*, content | params, ttl_ms?, delay_ms?, key?, status?, expects_reply?, display_name?}`

- Zone: `content` is a string, or a typed object for structured zones
  (`notification` with `title`, `body`, `urgency`, `actions`). `type` may be
  omitted; the runtime infers it from what the zone accepts. `key` is the
  merge key.
- Widget: `params` is the typed parameter map. A gRPC publish with
  `transition_ms` > 0 eases f32 and color params from what is on screen; enum
  and string params snap, and so does everything under degradation. The
  runtime wakes only until the transition lands.
- Portal: the first publish to `portal:<id>` attaches (`display_name` is
  optional). `content` is the output text, `status` is the lifecycle state,
  and `expects_reply` arms the composer. `key` replaces the newest output
  with the same key (progress lines). Portals are per agent: another agent's
  `portal:<id>` with the same id is a separate portal.
- `ttl_ms` defaults to 60000 for zones; widgets are durable unless given one.
  0 means held until cleared.
- `delay_ms` (zones, ≤ 300000) holds the content until then (invariant 1);
  the expiry counts from presentation, and `ttl_ms: 0` holds a delayed
  publish until cleared just like an immediate one.
- Returns `{"ok":true,"expires_in_ms":8000}`. It doesn't echo the request.

**`hud_hold`** `{surface*, ttl_ms*}` extends a holding, for any surface type.
On a zone or widget, `ttl_ms` replaces the expiry with now + `ttl_ms` (not
the original deadline), and 0 holds until `hud_clear`. A visible
notification's fade moves with it; a held one never fades.
On a portal, `ttl_ms` keeps it (and its transcript) attached for that long,
or until `hud_clear` when 0, even with no other calls. Without a hold, a
portal is degraded after 30 s with no publish, poll, or hold, and reclaimed
30 s later.

**`hud_clear`** `{surface*, reason?}` releases a zone publication, a widget
instance, or a portal (detach).

**`hud_input`** `{ack?: [input_id], wait_ms?, max_items?}`

- Returns input from every surface the agent holds, oldest first:
  `{"items":[{"id":"i7","s":"portal:claude-main","text":"yes, ship it"}],"remaining":0}`.
- Also returns notification action presses (`{"id":…,"s":"zone:notification-area","action":"approve"}`).
- `ack` confirms earlier items, so polling and acking take one round trip.
  Unacked items are redelivered.

### Identity and permissions

Identity comes from the PSK. Agents never name themselves, so tool calls
carry no namespace or owner token. Each agent has its own PSK; the HUD keeps
only its SHA-256, in `agents.toml` next to the config file:

```toml
[agents.claude-main]
psk_sha256 = "<64 hex: SHA-256 of the agent's PSK>"
allow = ["zone:*", "widget:gauge", "portal", "tiles"]
```

- The runtime loads `agents.toml` at startup and shares it live between MCP
  and gRPC, so pairing adds an agent without a restart. Hand edits (and
  `scripts/quickstart.sh`) need a restart. `[agents]` in the config file is a
  config error.
- **Pairing** (`POST /pair` on the MCP port, no bearer). While no agent exists,
  and after `tze_hud --pair` or Ctrl+Shift+P, the HUD shows a 6-digit one-time
  code and its address on the system card. The agent trades it for a key:

  ```
  curl -s host:9090/pair -d '{"agent":"claude","code":"482913"}'
  -> {"agent":"claude","psk":"<64 hex>","mcp":"http://100.x.y.z:9090/mcp","grpc":"100.x.y.z:50051"}
  ```

  `agent` is `[a-z0-9-]{1,32}`; re-pairing an existing id rotates its key. The
  new agent gets `allow = ["*"]`; add `"admin": true` to also get `admin`. The
  PSK is generated by the runtime, returned once, and only its hash is stored.
  A code is single use and expires after 5 minutes. Five wrong codes replace it
  with a new one; the third such replacement closes pairing for 60 s. Errors:
  `403 PAIR_CODE_INVALID`, `403 PAIRING_CLOSED` (not open, spent, expired, or
  cooling down), `400 BAD_REQUEST`. Neither the code nor the PSK is logged.
- `allow` is the whole permission model. It is checked at publish and claim,
  and `hud_surfaces` lists only allowed surfaces.
- An agent has whatever the allowlist says. There is no capability
  negotiation and no resident principal.
- `admin` is an operator entry, outside the model surface: it opens
  `GET /admin/status`, `GET /admin/logs`, and `GET /admin/screenshot` on the
  MCP port, and `*` does not grant it. `/admin/status` includes `safe_mode_hotkey`
  (`chord`, `registered`, `error`; `null` when no hotkey is active; `registered: null` while pending): `registered: false`
  means the human safe-mode chord is owned by another program.
  `/admin/screenshot` returns a PNG of the frame the compositor draws (not an
  OS capture; overlay alpha is as composited): one render on request, no cost
  while idle, never cached, one at a time (429 `BUSY`), 503 `UNAVAILABLE` after
  3 s or without a display, 422 `TOO_LARGE` past 8192 px per side, 16 Mpx, or a 32 MiB PNG. It
  is not exposed through MCP tools or gRPC.
  `POST /admin/restart` (POST only; 405 otherwise) relaunches the HUD with its
  own exe and arguments and answers 202 `{"restarting":true}`; the request body
  is ignored. The new process takes over once its first frame is up and the old
  one exits. If it does not report within 30 s, it is killed and the old
  process keeps running (`/admin/status` `last_restart` says why). One at a
  time (429 `BUSY`).
  `POST /admin/update` `{"channel":"dev"|"stable"|"v1.2.3"}` (POST only) pulls
  a signed release (see `docs/operations/windows-install.md`). It answers
  `{"up_to_date":true}` when the release is the running build, 202
  `{"updating":true,"sha":...}` once the download verified (the swap and
  handoff continue; `last_update` is `{ok, sha, error}`), 400 for a bad body,
  409 `NOT_INSTALLED` when not running from the install path, 429 `BUSY`, and
  502 `UPDATE_FAILED` with one constant hint for every failure cause.
- The operator's local tools (cleanup, composer paste, SVG asset upload) are
  off the model surface: CLI/config, or gRPC for tooling.

### Errors

Every tool failure is a tool result with `isError: true` whose text is
`{"code":"ZONE_NOT_FOUND","hint":"no zone subtitles; known: notification-area, subtitle"}`.
Only an unusable request (bad JSON, unknown method or tool, missing or
unknown PSK) is a JSON-RPC error.

- The message isn't repeated. The hint names the next call.
- Codes are a closed set shared by both planes (invariant 8;
  `crates/tze_hud_scene/src/error_codes.rs` `ERROR_CODES`, kept in sync with
  this list by a test). gRPC `RequestResult.code` uses the same set, checked
  by `grpc_codes_are_in_the_shared_set`.

| Code | Meaning |
|---|---|
| `INVALID_ARGUMENT` | Unknown field, wrong type, or bad surface string |
| `NOT_ALLOWED` | The agent's `allow` list doesn't cover the surface |
| `NOT_HELD` | `hud_hold` with nothing to extend (including a portal the viewer dismissed); `hud_clear` on a portal this agent never attached (a dismissed portal's `hud_clear` is an ok no-op) |
| `ZONE_NOT_FOUND` | No such zone |
| `WIDGET_NOT_FOUND` | No such widget instance |
| `WIDGET_PARAMETER_INVALID` | Unknown widget param, or a value of the wrong type or range |
| `CONTENT_REJECTED` | The zone doesn't accept this content, or is full; a portal publish or input too large |
| `LEASE_NOT_ACTIVE` | The agent's lease lapsed mid-call; retry |
| `SAFE_MODE_ACTIVE` | The human paused agents |
| `TIMESTAMP_TOO_FUTURE` | `delay_ms` beyond the scheduling horizon |
| `TIMESTAMP_TOO_OLD` | gRPC timing hint further in the past than the staleness window |
| `TIMESTAMP_EXPIRY_BEFORE_PRESENT` | gRPC `expires_at_us` is not after `present_at_us` |
| `BUDGET_EXCEEDED` | gRPC batch or claim over the session's resource budget, or a portal rate limit or full input queue; the hint names the next call |
| `UNAVAILABLE` | The portal service isn't running or the HUD is unavailable |
| `INTERNAL` | Runtime fault |

## gRPC (resident sessions)

There is one bidirectional `Session` stream
(`crates/tze_hud_protocol/proto/session.proto`), and its message set is cut
down to the lifecycle.

| Client → server | Server → client |
|---|---|
| `SessionInit{agent_id, auth_credential, subscriptions, …}` / `SessionResume{resume_token}` | `SessionEstablished{session_id, namespace, resume_token, heartbeat_interval_ms, …}` + `SceneSnapshot` |
| `Publish{surface, content \| params, ttl_ms, present_at_us?, expires_at_us?, key?}` | `RequestResult{seq, ok, code?, hint?, ids?, lease_id?, ttl_ms?, batch_id?}` (one shape for every request) |
| `Clear{surface}` (`tile:<id>` releases the tile and its lease) | `EventBatch{…}` (input, focus, element moved) |
| `ClaimTile{placement, ttl_ms, root?}` → tile id, lease, and content in one round trip | `Reclaimed{surface, why: EXPIRED \| OVERRIDE, lease_id}` (not sent after a disconnect: the session is gone) |
| `MutationBatch{lease_id, mutations}` (node tree updates on own tiles) | `SessionSuspended` / `SessionResumed` (safe mode) |
| `Hold{surface, ttl_ms}` (zone, widget, or `tile:<id>`) | `Heartbeat` |
| `ResourceUpload*` (images), `Heartbeat`, `SessionClose` | `DegradationNotice{level: NORMAL \| SIMPLIFIED}` |

- **Init to a visible, filled tile takes 2 round trips** (handshake, then
  `ClaimTile` with `root`), down from 4 (handshake, `LeaseRequest`,
  `CreateTile`, `SetTileRoot`). `RequestResult.ids` is the tile id followed
  by the root tree's node ids in pre-order.
- **Placement is a hint the runtime resolves.** `TilePlacement{anchor, size}`
  names one of nine anchors (default top-right) and a size class (`SMALL`,
  `MEDIUM` (default), `LARGE`, `WIDE`, `TALL`). Sizes, the screen margin, and
  the stacking gap come from `[design_tokens]` `tile.<size>.width|height`,
  `tile.margin`, and `tile.gap`. Claims at the same anchor stack away from
  the edge (down from top and middle anchors, up from bottom anchors) and
  are clamped to the display. Z-order follows claim order. Agents never send
  bounds or z-order.
- **Every `RequestResult` carries `code` + `hint`.** Scene validation hints
  reach the agent rather than being flattened to `MUTATION_REJECTED`.
  `seq` echoes the request's `sequence`; ephemeral zone publishes get no
  reply. `ClaimTile`, `Hold`, and `Clear` retransmits replay the cached reply.
- **Timing hints are honored**, not just validated: `present_at_us` holds
  content, and `expires_at_us` sweeps it (invariant 1). On `Publish`,
  `expires_at_us` wins over `ttl_ms`, which counts from presentation.
- **Runtime-internal mutations.** `CreateTile`, `PublishToTile`, and the
  portal mutations (`MutationProto` 13–17) are applied only by the runtime's
  in-process portal driver. The session server rejects them from agents with
  `INVALID_ARGUMENT`.

## Token budgets

`token_footprint` (CI) records o200k tokens per flow two ways: **wire** (the
full JSON-RPC request and response bodies) and **model-visible** (the tool
name and arguments plus the result text: what enters the model's context).
The budgets are enforced on model-visible tokens; the JSON-RPC envelope adds
about 50 tokens per call that the model never sees. Both measures are
baselined against regressions.

| Measure | Before T5 (wire) | Wire | Model-visible | Budget (model-visible) |
|---|---|---|---|---|
| `tools/list` | 4,418 | 495 | 458 | ≤ 900 |
| Discover (default scene) | ~430 (`list_zones`) | 181 | 113 | ≤ 150 |
| Zone publish | ~197 | 90 | 39 | ≤ 80 |
| Widget publish | ~168 | 73 | 22 | ≤ 80 |
| Portal: attach+publish, poll, ack, clear | ~575 over 5 round trips | 319 over 4 | 113 over 4 | ≤ 250 |
| Discover (`production.toml`: 6 zones, 3 built-in widgets) | n/a | n/a | ~232 | ≤ 250 |
| Error | up to ~212 | 101 | 45 | ≤ 60 |

The portal flow takes 4 round trips in the canonical fixture because the
first poll has nothing to ack; in a steady loop each `hud_input` both acks
the previous items and polls, so poll+ack is one round trip.

`integration` `poc_acceptance` checks model-visible counts on the POC
acceptance flows (`docs/scope.md`), run end to end against `production.toml`.
Its discover budget is the production row above: the three built-in widgets
add about 100 tokens, all typed parameter names and ranges the model needs to
publish, so shrinking further would drop information the model acts on.

## Design decisions (T5, 2026-10-02)

1. **Tiles are gRPC only.** MCP loses `create_tile`, `set_content`,
   `dismiss`, `create_tab`, and `publish_to_element`.
2. **Tiles take a placement hint.** `ClaimTile` gets a `placement` hint
   (anchor + size class) that the runtime resolves, in place of `bounds`
   and `z_order`.
3. **Lease priority is dropped.** Chrome is structurally above agent
   content, and ties go to claim order.
4. **Portal tile mutations are internal.** Accent, unread count, composer
   interaction, and portal surface state stay inside the runtime; agents
   drive the portal through `hud_publish`.
