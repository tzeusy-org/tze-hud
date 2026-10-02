# API

**Status:** the MCP section is implemented (T5 S3) and is the reference. The
gRPC section is still a proposal: each remaining slice in "Plan" moves code
toward it.

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
| Reclaim | none (runtime) | `Reclaimed` (pushed) | Runs on expiry, disconnect, or human override |

MCP `tools/list` has five tools. The resident tile API is gRPC only.

## MCP tools

Standard MCP over JSON-RPC 2.0 (HTTP POST): `initialize`,
`notifications/initialized` (no response), `tools/list`, and `tools/call`.
There are no other methods. A result is one text content block of compact
JSON. Agents never send geometry, styling, or z-order. Times are
milliseconds, named `*_ms`, everywhere.

**`hud_surfaces`** has no params. It returns one compact entry per surface:

```json
{"surfaces":[
  {"s":"zone:subtitle","accepts":"text"},
  {"s":"zone:notification","accepts":"notification","held":true,"expires_in_ms":4200},
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
- Widget: `params` is the typed parameter map.
- Portal: the first publish to `portal:<id>` attaches (`display_name` is
  optional). `content` is the output text, `status` is the lifecycle state,
  and `expects_reply` arms the composer. `key` is the portal coalesce key.
- `ttl_ms` defaults to 60000 for zones; widgets are durable unless given one.
  0 means held until cleared.
- `delay_ms` (zones, ≤ 300000) holds the content until then (invariant 1);
  the expiry counts from presentation.
- Returns `{"ok":true,"expires_in_ms":8000}`. It doesn't echo the request.

**`hud_hold`** `{surface*, ttl_ms*}` extends a holding, for any surface type.
On a portal, `ttl_ms` keeps it (and its transcript) attached for that long,
or until `hud_clear` when 0, even with no other calls. Without a hold, a
portal is degraded after 30 s with no publish, poll, or hold, and reclaimed
30 s later.

**`hud_clear`** `{surface*, reason?}` releases a zone publication, a widget
instance, or a portal (detach).

**`hud_input`** `{ack?: [input_id], wait_ms?, max_items?}`

- Returns input from every surface the agent holds, oldest first:
  `{"items":[{"id":"i7","s":"portal:claude-main","text":"yes, ship it"}],"remaining":0}`.
- Also returns notification action presses (`{"id":…,"s":"zone:notification","action":"approve"}`).
- `ack` confirms earlier items, so polling and acking take one round trip.
  Unacked items are redelivered.

### Identity and permissions

Identity comes from the PSK. Agents never name themselves, so tool calls
carry no namespace or owner token.

```toml
[agents.claude-main]
psk_env = "TZE_HUD_PSK_CLAUDE_MAIN"
allow = ["zone:*", "widget:gauge", "portal", "tiles"]
```

- `allow` is the whole permission model. It is checked at publish and claim,
  and `hud_surfaces` lists only allowed surfaces.
- An agent has whatever the allowlist says. There is no capability
  negotiation and no resident principal.
- The operator's local tools (cleanup, composer paste, SVG asset upload) are
  off the model surface: CLI/config, or gRPC for tooling.

### Errors

Every tool failure is a tool result with `isError: true` whose text is
`{"code":"ZONE_NOT_FOUND","hint":"no zone subtitles; known: notification-area, subtitle"}`.
Only an unusable request (bad JSON, unknown method or tool, missing or
unknown PSK) is a JSON-RPC error.

- The message isn't repeated. The hint names the next call.
- Codes are a closed set (invariant 8; `crates/tze_hud_mcp/src/error.rs`
  `ERROR_CODES`, kept in sync with this list by a test). S4 moves gRPC onto
  the same set.

| Code | Meaning |
|---|---|
| `INVALID_ARGUMENT` | Unknown field, wrong type, or bad surface string |
| `NOT_ALLOWED` | The agent's `allow` list doesn't cover the surface |
| `NOT_HELD` | `hud_hold` with nothing to extend, or `hud_clear` on a portal that isn't attached |
| `ZONE_NOT_FOUND` | No such zone |
| `WIDGET_NOT_FOUND` | No such widget instance |
| `WIDGET_PARAMETER_INVALID` | Unknown widget param, or a value of the wrong type or range |
| `CONTENT_REJECTED` | The zone doesn't accept this content, or is full |
| `LEASE_NOT_ACTIVE` | The agent's lease lapsed mid-call; retry |
| `SAFE_MODE_ACTIVE` | The human paused agents |
| `TIMESTAMP_TOO_FUTURE` | `delay_ms` beyond the scheduling horizon |
| `UNAVAILABLE` | The portal service isn't running |
| `INTERNAL` | Runtime fault |
| `PROJECTION_NOT_FOUND`, `PROJECTION_ALREADY_ATTACHED`, `PROJECTION_UNAUTHORIZED`, `PROJECTION_TOKEN_EXPIRED`, `PROJECTION_INVALID_ARGUMENT`, `PROJECTION_OUTPUT_TOO_LARGE`, `PROJECTION_INPUT_TOO_LARGE`, `PROJECTION_INPUT_QUEUE_FULL`, `PROJECTION_RATE_LIMITED`, `PROJECTION_STATE_CONFLICT`, `PROJECTION_HUD_UNAVAILABLE`, `PROJECTION_INTERNAL_ERROR` | Portal authority rejections, passed through |

## gRPC (resident sessions)

There is one bidirectional `Session` stream, and its message set is cut down
to the lifecycle.

| Client → server | Server → client |
|---|---|
| `Hello{auth, subscriptions, resume_token?}` | `Welcome{session_id, resume_token, heartbeat_ms, wall_clock_us, surfaces}` |
| `Publish{surface, content \| params, ttl_ms, present_at_us?, expires_at_us?}` | `Result{seq, ok, code?, hint?, ids?}` (one shape for every request) |
| `Clear{surface}` | `EventBatch{…}` (input, focus, element moved) |
| `ClaimTile{placement, ttl_ms, root?}` → tile id, lease, and content in one round trip | `Reclaimed{surface, why: expired \| disconnected \| override}` |
| `MutationBatch{tile, mutations}` (node tree updates, latest wins) | `Suspended` / `Resumed` (safe mode) |
| `Hold{surface \| tile, ttl_ms}` | `Heartbeat` |
| `Upload*` (images), `Heartbeat`, `Bye` | `DegradationNotice{level: NORMAL \| SIMPLIFIED}` |

- **Init to a visible, filled tile takes 2 round trips**, down from 4.
  `ClaimTile` takes the initial node tree; client temp ids map to runtime ids
  in `Result.ids`.
- **Every `Result` carries `code` + `hint`.** Scene validation hints reach the
  agent rather than being flattened to `MUTATION_REJECTED`.
- **Timing hints are honored**, not just validated: `present_at_us` holds
  content, and `expires_at_us` sweeps it (invariant 1).

## Plan

Each slice builds, passes tests, and keeps `invariants.md`. Every client in
this repo (skills, examples, Python stubs) is updated in the same PR. There
are no compatibility shims; removed proto fields are `reserved`.

| Slice | What |
|---|---|
| S0 | **Fix invariant breaks found by the audit.** (a) A gRPC disconnect never calls `disconnect_lease`, so there is no orphan badge and no grace-expiry reclaim; only the TTL frees the lease (invariant 4; the tests drive the scene directly). (b) gRPC drops `TimingHints` after validating them, and `ZonePublish` ignores `ttl_us`, `present_at`, and `expires_at` (invariant 1). Add end-to-end tests over the gRPC path. |
| S1 | **Remove dead wire.** Messages that are never sent or never handled: `SceneDelta`, `BackpressureSignal`, `RuntimeTelemetryFrame`, `TelemetryFrame`, `SetImePosition`, `EmitSceneEvent` (never delivered), and `Zone/WidgetRegistry*`. Also `events_legacy.proto`, fields that are never read, duplicate `LeaseStateChange`, deprecated `pre_shared_key`, `DegradationLevel` cut to two values, error enum values that are never set, the dead `SessionConfig`, and three copies of the capability vocabulary. |
| S2 | **Identity and allowlist.** Per-agent PSK; `allow` replaces the 16-entry capability vocabulary and the resident principal; namespace comes from identity; the portal owner token leaves model context. |
| S3 | **MCP verbs.** Done: five tools replace 22. One error shape. The token-footprint benchmark adds `tools/list`, discovery, and errors. |
| S4 | **gRPC verbs.** `Publish`/`Clear`/`Hold`/`ClaimTile`/`Reclaimed`/one `Result`. Collapse the six `HudSessionImpl` constructors into one deps struct (done in S4a, with scene capabilities and lease priority removed). |
| S5 | This file loses "proposal"; `scope.md` marks T5 done. |

**S2 notes (landed).** Config is `[agents.<id>]` with `psk_env` and `allow`;
`psk_env = "TZE_HUD_PSK"` always means the runtime PSK. Deferred to later
slices:

- The scene's internal `Capability` enum and per-lease priority (removed in
  S4a). The session server now checks the allow list at the boundary, and
  leases carry neither.
- MCP still accepts the JSON-RPC `_auth` param next to the bearer.
- The tool param structs still deserialize `namespace` and `owner_token`, but
  both are hidden from `tools/list`; the server sets the namespace and fills in
  the owner token when absent. S3 replaces
  these tools.



## Token budgets

`token_footprint` (CI) records o200k tokens per flow two ways: **wire** (the
full JSON-RPC request and response bodies) and **model-visible** (the tool
name and arguments plus the result text: what enters the model's context).
The budgets are enforced on model-visible tokens; the JSON-RPC envelope adds
about 50 tokens per call that the model never sees. Both measures are
baselined against regressions.

| Measure | Before T5 (wire) | Wire now | Model-visible now | Budget (model-visible) |
|---|---|---|---|---|
| `tools/list` | 4,418 | 495 | 458 | ≤ 900 |
| Discover (default scene) | ~430 (`list_zones`) | 181 | 113 | ≤ 150 |
| Zone publish | ~197 | 90 | 39 | ≤ 80 |
| Widget publish | ~168 | 73 | 22 | ≤ 80 |
| Portal: attach+publish, poll, ack, clear | ~575 over 5 round trips | 319 over 4 | 113 over 4 | ≤ 250 |
| Error | up to ~212 | 101 | 45 | ≤ 60 |

The portal flow takes 4 round trips in the canonical fixture because the
first poll has nothing to ack; in a steady loop each `hud_input` both acks
the previous items and polls, so poll+ack is one round trip.

## Decisions (2026-10-02)

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
