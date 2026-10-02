# API

**Status: proposal (T5).** This is the target API. Each slice in "Plan"
moves code toward it, and this file becomes the reference once the slices
land. The PR that introduced it holds the measurements of today's surface.

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

Agents never send geometry, styling, or z-order. Times are milliseconds,
named `*_ms`, everywhere.

**`hud_surfaces`** has no params. It returns one compact entry per surface:

```json
{"surfaces":[
  {"s":"zone:subtitle","accepts":"text","held":false},
  {"s":"zone:notification","accepts":"notification","held":true,"expires_in_ms":4200},
  {"s":"widget:gauge","params":{"level":"f32 0..1","label":"string"}},
  {"s":"portal:claude-main","state":"attached","pending_input":1}
]}
```

It includes nothing the model doesn't act on: no UUIDs, geometry, or
timestamps. Holdings are the entries with `held: true`.

**`hud_publish`** `{surface*, content | params, ttl_ms?, key?, status?, expects_reply?}`

- Zone: `content` is a string, or a typed object for structured zones
  (`notification` with `title`, `body`, `urgency`, `actions`). `key` is the
  merge key for stack zones.
- Widget: `params` is the typed parameter map.
- Portal: the first publish to `portal:<id>` attaches (`display_name` is
  optional). `content` is the output text, `status` is the lifecycle state,
  and `expects_reply` arms the composer.
- `ttl_ms` defaults per surface type. 0 means held until cleared or until the
  agent disconnects.
- Returns `{"ok":true,"expires_in_ms":8000}`. It doesn't echo the request.

**`hud_hold`** `{surface*, ttl_ms*}` extends a holding, for any surface type.

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

Every failure is a tool result with `isError: true` and
`{"code":"ZONE_NOT_FOUND","hint":"call hud_surfaces; known zones: subtitle, notification"}`.

- Codes are a closed, documented set shared with gRPC (invariant 8).
- The message isn't repeated. The hint names the next call.

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
| S3 | **MCP verbs.** Five tools replace 22. One error shape. The token-footprint benchmark adds `tools/list`, discovery, and errors. |
| S4 | **gRPC verbs.** `Publish`/`Clear`/`Hold`/`ClaimTile`/`Reclaimed`/one `Result`. Collapse the six `HudSessionImpl` constructors into one deps struct. |
| S5 | This file loses "proposal"; `scope.md` marks T5 done. |

**S2 notes (landed).** Config is `[agents.<id>]` with `psk_env` and `allow`;
`psk_env = "TZE_HUD_PSK"` always means the runtime PSK. Deferred to later
slices:

- The scene still has its internal `Capability` enum and per-lease priority.
  `allow` entries expand to those at the session boundary, and every agent
  lease gets the same priority. Deleting them belongs with S4.
- MCP still accepts the JSON-RPC `_auth` param next to the bearer.
- The tool param structs still deserialize `namespace` and `owner_token`, but
  both are hidden from `tools/list`; the server sets the namespace and fills in
  the owner token when absent. S3 replaces
  these tools.



## Token budgets

`token_footprint` enforces these in CI (o200k tokens, request + response):

| Measure | Today | Target |
|---|---|---|
| `tools/list` | 4,418 | ≤ 900 |
| Discover (default scene) | ~430 (`list_zones`) | ≤ 150 |
| Zone publish | ~197 | ≤ 80 |
| Widget publish | ~168 | ≤ 80 |
| Portal: attach, publish, poll+ack, detach | ~575 over 5 round trips | ≤ 250 over 3 |
| Any error | up to ~212 | ≤ 60 |

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
