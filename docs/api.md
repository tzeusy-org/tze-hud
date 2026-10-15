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
`portal:<id>` attaches a fresh one and reports `reattached: true`;
`hud_hold` answers `NOT_HELD` and
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
otherwise. Scheduled zone publications count as held before presentation;
`expires_in_ms`, when present, includes their presentation delay.

**`hud_publish`** `{surface*, content | params, ttl_ms?, delay_ms?, key?, status?, expects_reply?, display_name?}`

- Zone: `content` is a string, or a typed object for structured zones
  (`notification` with optional `title`, `body`, integer `urgency` 0–3
  (default 1), and optional `actions: [{label, callback_id}]`; both action
  fields are non-empty strings). Activating an action dismisses the
  notification. `type` may be
  omitted; the runtime infers it from what the zone accepts. `key` is the
  merge key. Supplied notification urgency must be an integer 0–3; only
  omission defaults to 1. `status_bar.entries` must be a non-empty object
  whose every value is a string; an empty string removes that entry.
  Notification TTL belongs at the publish's top-level `ttl_ms`; an inner
  `content.ttl_ms` is rejected with a hint to move it.
- Widget: `params` is the typed parameter map. F32 values must be finite and
  within the declaration's inclusive bounds (including one-sided bounds).
  Both MCP and gRPC reject out-of-range values rather than clamping them.
  MCP colors are exact `#RRGGBB` strings or `{r,g,b,a?}` objects: RGB are
  required numeric channels in 0–1, alpha defaults to 1 only when absent,
  and supplied alpha must also be numeric in 0–1. Both planes reject
  nonfinite/out-of-range RGBA. Config bundle byte-array defaults are a separate
  loader format, normalized before admission.
  Configured HUD startup resolves
  the selected theme and config overrides once; MCP publishes use its
  `motion.state.ms` duration without adding a tool parameter. gRPC publishes
  retain their explicit `transition_ms`. A positive duration eases f32 and
  color params from what is on screen; enum and string params and a widget's
  first appearance snap. Simplified degradation snaps and stops transitions.
  The runtime wakes only until the transition lands. A zero duration cancels
  an older transition, including a repeated target, and snaps to the newest
  value. Standalone unconfigured MCP servers retain zero duration. Changing
  the selected theme takes effect at config load/restart; live theme reload
  is separate work. Zone enter/exit durations use the resolved motion tokens
  unless explicitly overridden by zone policy, and Stack TTL fades use the
  exit duration/curve without extending authoritative expiry.
- Portal: the first publish to `portal:<id>` attaches (`display_name` is
  optional). Omit content for a bare attach or status-only update.
  `content` appends only the new output text; `status` labels agent activity
  (`active` or `degraded`), and `expects_reply` arms the composer. `key`
  replaces the newest output
  with the same key (progress lines). Portals are per agent: another agent's
  `portal:<id>` with the same id is a separate portal.
- `ttl_ms` defaults to 60000 for zones; widgets are durable unless given one.
  0 means held until cleared. On a portal, an optional `ttl_ms` holds the
  portal atomically with this publish, with the same semantics as `hud_hold`.
- `delay_ms` (zones, ≤ 300000) holds the content until then (invariant 1);
  the expiry counts from presentation, and `ttl_ms: 0` holds a delayed
  publish until cleared just like an immediate one.
- Returns `{"ok":true,"expires_in_ms":8000}` when a TTL is set. Portal
  publishes also return `reattached` (true when this call replaces a
  runtime-reclaimed portal) and `pending_input` (queued replies). First attach,
  live update, and attach after explicit clear report false. The response
  describes the completed publish; it does not echo content.

For example, publish a notification whose button arrives at `hud_input` as
`{"action":"approve","s":"zone:notification-area",...}`:

```json
{"surface":"zone:notification-area","content":{"body":"Build complete","urgency":1,"actions":[{"label":"Approve","callback_id":"approve"}]},"ttl_ms":10000}
```

Notification action buttons use the runtime's shared draw/label/hit geometry
and design tokens. Hover and press feedback is local and ephemeral (pressed
wins over hover); it sends no `hud_input` record. Leaving the owning window,
losing focus/capture, release, clear, expiry, replacement, and dismiss remove
transient feedback. Only a matching intentional press/release on the same live
publication activates its callback. The existing
[`poc_notification_action_reaches_hud_input`](../tests/integration/poc_acceptance.rs)
checks independent Approve/Reject, repeated polls, ACK, and publisher isolation.
This source contract is separate from Windows owner acceptance of held feedback.

**`hud_hold`** `{surface*, ttl_ms*}` extends a holding, for any surface type.
On a zone or widget, `ttl_ms` replaces the expiry with now + `ttl_ms` (not
the original deadline), and 0 holds until `hud_clear`. A delayed publish
that has not presented yet counts as held: its new expiry counts from
presentation. A visible
notification's fade moves with it; a held one never fades.
On a portal, `ttl_ms` keeps it (and its transcript) attached for that long,
or until `hud_clear` when 0, even with no other calls. Without a hold, a
portal is degraded after 30 s with no publish, poll, or hold, and reclaimed
30 s later.

**`hud_clear`** `{surface*}` releases a zone publication, a widget
instance, or a portal (detach). On a zone it also cancels the caller's own
delayed publishes that haven't presented yet; other agents' are untouched.
The gRPC `Hold` and `Clear` behave the same on zones. A `ClearZone` inside a
mutation batch clears only active publications and never cancels pending ones.

**`hud_input`** `{ack?: [input_id], wait_ms?, max_items?}`

- Returns input from every surface the agent holds, oldest first:
  `{"items":[{"id":"i7","s":"portal:claude-main","text":"yes, ship it"}],"remaining":0}`.
- Notification callbacks keep their existing shape:
  `{"id":…,"s":"zone:notification-area","action":"approve"}`. A validated
  human notification × uses `{"id":…,"s":"zone:notification-area","action":"dismiss","kind":"dismiss"}`.
  A publisher callback named `dismiss` has no `kind` field. Callback activation
  already removes its notification locally; it emits no second human-X event.
  TTL expiry, `hud_clear` and stale/unmatched releases do not emit human-X input.
- Accepted notification events are retained in a growable per-publisher FIFO,
  with stable ids until `ack`; one publisher cannot evict or acknowledge another's
  records. Retention is in memory, with no crash/disk durability claim. Portal
  replies keep their existing admission bounds, TTL, clear and reclaim rules.
- `ack` confirms earlier items once per call; unacked items are redelivered.
  `max_items` limits the result and `remaining` reports the retained backlog;
  zero returns the backlog without consuming it. Notification FIFO items precede
  portal replies in their existing chronological order.
- `wait_ms` defaults to 0 and caps at 30000. The caller's armed readiness wait
  wakes on committed input and uses one absolute deadline with a final queue
  check. It neither polls every 150 ms nor changes the stdio adapter's 60 s cap.
  A finite active poll protects only its agent from idle degradation, including
  first attachment; completion/cancellation releases ownership without a future
  heartbeat. Explicit upstream loss, reclaim, holds and input TTL still apply.
- An accepted local portal reply is echoed in the submitting scene-maintenance
  turn, inside the ordinary 100 ms output pacing slot. Refused replies add no
  accepted input or success echo; their truthful local feedback is also urgent.
  Scene readiness is separate from physical display presentation.

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
  and gRPC, so pairing and the control panel's Agents card (unpair, allow
  list) change agents without a restart. Each such change re-reads the file
  and edits it in place, keeping comments and other agents' tables, so a hand
  edit made while the HUD runs is kept. A hand edit takes effect with the
  next change, the open control panel's next refresh (once a second), or a
  restart; one that does not load is logged and the last good file stays in
  force. A hand-removed agent's connected gRPC sessions keep running until
  they reconnect; the panel's Unpair ends them at once. `[agents]` in the config file is a config error.
- **Pairing** (`POST /pair` on the MCP port, no bearer). While no agent exists,
  and after `tze_hud --pair`, the HUD shows a 6-digit one-time code and its
  address on the system card; the pairing chord and the control panel's
  Connect an agent card (see
  [Human chords and the control panel](#human-chords-and-the-control-panel))
  show it in a copyable setup prompt instead. The agent trades it for a key:

  ```
  curl -s host:9090/pair -d '{"agent":"claude","code":"482913"}'
  -> {"agent":"claude","psk":"<64 hex>","mcp":"http://100.x.y.z:9090/mcp","grpc":"100.x.y.z:50051"}
  ```

  `agent` is `[a-z0-9-]{1,32}`; re-pairing an existing id rotates its key. The
  new agent gets `allow = ["*"]`; add `"admin": true` to also get `admin`,
  except with a code from Connect an agent (`403 NOT_ADMIN`, code not spent). The
  PSK is generated by the runtime, returned once, and only its hash is stored.
  A code is single use and expires after 5 minutes. Five wrong codes replace it
  with a new one; the third such replacement closes pairing for 60 s. Errors:
  `403 PAIR_CODE_INVALID`, `403 NOT_ADMIN` (admin asked with a Connect code),
  `403 PAIRING_CLOSED` (not open, spent, expired, or
  cooling down), `400 BAD_REQUEST`. Neither the code nor the PSK is logged.
- `allow` is the whole permission model. It is checked at publish and claim,
  and `hud_surfaces` lists only allowed surfaces.
- An agent has whatever the allowlist says. There is no capability
  negotiation and no resident principal.
- `admin` is an operator entry, outside the model surface: it opens
  `GET /admin/status`, `GET /admin/logs`, and `GET /admin/screenshot` on the
  MCP port, and `*` does not grant it. `/admin/status` includes `tailnet_inbound` (`state`: `allowed`, `blocked`, `unknown`, `not_applicable`; `reason`, `rule` and `fix` when `blocked`; `error` when `unknown`): whether Windows Firewall lets remote agents reach the tailnet bind, and `safe_mode_hotkey`, `panel_hotkey` and `pairing_hotkey`
  (each `chord`, `registered`, `error`; `null` when no hotkey is active; `registered: null` while pending): `registered: false`
  means another program owns that chord; for `safe_mode_hotkey` the human pause override is then lost. The blocked
  firewall `fix` names `tze_hud.exe --allow-remote` with the same listen-port
  overrides as startup; see [the reversible Windows helper](operations/windows-install.md#remote-agents).
  This policy-derived result is cached for five seconds, not a network probe.
  `/admin/status` `displays` lists the overlaid monitors, primary first:
  `{name, x, y, width, height, primary, zones}` in scene pixels (the primary's
  top-left is 0,0; others may be negative) and the zones configured onto each;
  `unplaced_zones` lists `{zone, display, reason}` placements with no overlay
  window, rendering on the primary instead: `reason` is `not_connected`, or
  `overlay_failed` when that monitor's overlay surface was lost and recreated
  3 times in a row (it is retried when the monitor is reconnected).
  `/admin/screenshot` returns a PNG of the frame the compositor draws (not an
  OS capture; overlay alpha is as composited) for one display:
  `?display=<i>` indexes `displays` (default 0, the primary; 404
  `NO_SUCH_DISPLAY` for an index not in `displays`, or one whose overlay closed
  since `displays` was read; indexes shift on hot-plug, so re-read status). One render on request, no cost
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
- Typed zone content is strict: an unknown or nested key is `INVALID_ARGUMENT` with the allowed keys in the hint, and so is empty content (a notification with no title or body, `stream_text` without text, `solid_color` missing r/g/b; over gRPC, a blank notification).
- Invalid widget values return `WIDGET_PARAMETER_INVALID` with the parameter
  and its required type/range; invalid urgency/status entries/inner TTL return
  `INVALID_ARGUMENT`. Mixed valid/invalid updates reject whole before changing
  publications, current parameters, transitions or MCP lease renewals.
- A sequence-valid post-handshake gRPC `ClientMessage` with no recognized request payload returns `RequestResult{ok: false, code: INVALID_ARGUMENT}` with a hint to use the current schema and a supported request. This covers empty envelopes and requests containing only unknown or removed fields. Protobuf discards those fields, so the server cannot identify the discarded request tag or name. Unknown fields alongside a recognized request keep its normal behavior.
- Codes are a closed set shared by both planes (invariant 8;
  `crates/tze_hud_scene/src/error_codes.rs` `ERROR_CODES`, kept in sync with
  this list by a test). gRPC `RequestResult.code` uses the same set, checked
  by `grpc_codes_are_in_the_shared_set`.

| Code | Meaning |
|---|---|
| `INVALID_ARGUMENT` | Unknown field, wrong type, or bad surface string |
| `NOT_ALLOWED` | The agent's `allow` list doesn't cover the surface |
| `NOT_HELD` | A tile mutation targets an absent tile or one owned by another authenticated namespace; `hud_hold` with nothing to extend (including a portal the viewer dismissed); `hud_clear` on a portal this agent never attached (a dismissed portal's `hud_clear` is an ok no-op) |
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
  `seq` echoes the request's `sequence`, including legacy zero and rejected
  empty/unknown-only envelopes; those rejections keep the stream open.
  Ephemeral zone publishes get no reply. `ClaimTile`, `Hold`, and `Clear`
  retransmits replay the cached reply.
- **Timing hints are honored**, not just validated: `present_at_us` holds
  content, and `expires_at_us` sweeps it (invariant 1). On `Publish`,
  `expires_at_us` wins over `ttl_ms`, which counts from presentation.
- **Zero means the surface default on `Publish`.** Omitted scalar fields
  arrive as zero. With both `ttl_ms` and `expires_at_us` zero, notifications
  expire after 8 s (urgency 0/1), 15 s (2), or 30 s (3+), counted from
  presentation. Other zone content and widget publications have no derived
  content expiry. MCP differs: omitted zone `ttl_ms` gives 60 s, while an
  explicit `ttl_ms: 0` holds even a notification until cleared.
- **Hold a notification with `Publish`, then `Hold{surface, ttl_ms: 0}`.**
  Hold removes content expiry from the agent's active and pending publications
  on that zone; it is namespace-wide, not limited to one merge key. Pending
  content still waits for `present_at_us`, and `Clear` cancels it. The two
  requests are not atomic: if nothing remains when Hold arrives, it returns
  `NOT_HELD`. A later Publish uses its own timing, so republishing a
  notification can restore an expiry. Held content remains subject to
  clearing, human override and lease/session-origin reclamation (invariant 4);
  Hold does not make a lease immortal. There is no atomic held mode on Publish.
- **Runtime-internal mutations.** `CreateTile`, `PublishToTile`, and the
  portal mutations (`MutationProto` 13–17) are applied only by the runtime's
  in-process portal driver. The session server rejects them from agents with
  `INVALID_ARGUMENT`.

## Configuration

The runtime installs its built-in zones independently of configuration. Root
`[zones]` custom definitions and `[[tabs]].zones` are unsupported, including
empty tables or arrays: startup rejects them with `ParseError` and a hint to
remove or correct the field. Remove those obsolete entries rather than
renaming their zone values. Tabs and their widgets remain configurable;
`[displays.<NAME>].zones` remains the supported way to assign built-in zones
to a display.

## Human chords and the control panel

Three global chords work while the overlay is unfocused and click-through
(Windows; each is set in `[runtime]` as modifiers Ctrl/Shift/Alt/Win plus
F1-F24, a letter or a digit, and the three must differ):

| Chord (default) | Key | Effect |
|---|---|---|
| Ctrl+Shift+F12 | `safe_mode_hotkey` | Pause agents and open the control panel; pressed again, resume and close |
| Ctrl+Shift+F11 | `panel_hotkey` | Open or close the control panel with agents running |
| Ctrl+Shift+F10 | `pairing_hotkey` | Open the control panel at Connect an agent, agents running |

A global chord takes its keys from every other app while the HUD runs, so the
defaults are F-key chords; rebind one that clashes (Ctrl+Shift+F10 is also
JetBrains IDEs' "Run"). The pairing chord is not registered when the MCP port
is disabled.

The control panel is a full-width pane over the primary display: a search
field, quick-action tiles, and section cards of settings that scroll
vertically. It takes keyboard focus when it opens (if Windows refuses, the
header says to click it) and returns focus on close. Typing searches every
row; arrows, Tab, Enter and Space move and act; Esc clears the search, then
closes. Clicks outside the panel pass through to the desktop. Closing the
panel always resumes agents. While paused, the pause tile and row turn
caution-colored.

The Visibility card hides portals, zones, widgets or other agent tiles for
the session; the Hide all tile (Alt+H) hides all of them and, when everything
is hidden, shows it all again. Hidden content is not drawn and takes no input
or focus; agents keep their leases and publications and are not told. While
anything is hidden and the panel is closed, a pill at the bottom right of the
primary display says what is hidden and which chord opens the panel (the
pause chord when Windows refused the panel chord). A restart shows everything.

The Agents card lists every paired agent, and any other agent that is
connected or shows something, with a status dot and what it holds (portals,
zones, widgets, tiles); the list follows agents as they come and go while the
panel is open. Opening an agent's row shows:

- **Clear** (asks first): removes everything the agent shows now, as a viewer
  dismiss would, without waiting on it. A connected gRPC agent gets
  `Reclaimed{OVERRIDE}` per lease. Clear is not a block: the agent's next
  publish is served, and a portal publish reports `reattached: true`.
- **Unpair** (asks first): removes the agent from `agents.toml`, so its PSK
  stops authenticating at once (a handshake already under way is refused);
  ends its gRPC sessions, even a hung one; and then clears its content.
  Unpairing the last agent reopens pairing.
- **Zones, Widgets, Portal, Tiles, Admin**: grant or withdraw a whole class
  (`zone:*`, `widget:*`, `portal`, `tiles`, `admin`) in the allow list as it
  stands in the file. Withdrawing applies at once; granting asks first. A `*` entry is rewritten as the classes that
  remain when one is withdrawn; single-surface entries (`zone:subtitle`) are
  kept. MCP requests use the new list from the next call; a connected gRPC
  session keeps the grants it connected with until it reconnects (Unpair cuts
  it off at once).

**Connect an agent** (the Agents card's last row, the Alt+C tile, or the
pairing chord) opens pairing with a fresh one-time code and shows a setup
prompt to paste into any agent session, beside the agent's name (typing
replaces the suggested free name; lower case, digits and dashes), the code
and its countdown, the address agents should use, whether it is reachable
(Tailscale up, Windows Firewall), and the grants (`*`, not admin). The prompt
says the HUD is reachable only inside the tailnet at that address (only from
this PC, at its loopback address, when Tailscale is down) and that an agent
that cannot reach it must stop and say so; it gives the `POST /pair` call
piped into a `python3` step that saves the key to
`~/.config/tze-hud/<host>.psk` (mode 0600) and prints only the rest of the
reply, so the key never reaches the agent's output; then the MCP URL and
header, the five verbs and the content-only rule. It holds the code, never a
key. Enter (or Copy) puts it on the clipboard, first issuing a new code if the
shown one was used, expired or has under a minute left; the card then says it
is waiting, that the agent paired once it has, or that the copied code no
longer works. Clicking the name edits it; only Enter copies. New code replaces
the code. Esc goes back to the sections. While the card is up, the system card
stays away, even for a code replaced after wrong guesses; once it is left, a
replacement shows on the system card again.

Agents never see the panel: it is not in the scene, in MCP or gRPC results,
or in `/admin/screenshot`. An idle open panel draws no frames, except that
Connect an agent redraws once a second for its countdown. Its layout
reads `control_panel.inset` and `control_panel.column.min_width` and the
semantic tokens of the active theme.

## Design tokens and themes

Agents never send styling. Every color, size, radius, font, and timing
the runtime draws with is a design token in the config file, resolved
once at startup and applied through `RenderingPolicy`, widget
`{{token.key}}` placeholders, and the compositor token map. The
canonical keys, their descriptions, and their defaults are in
`CANONICAL_TOKENS` (`crates/tze_hud_config/src/tokens.rs`).

Widget bundles resolve placeholders at load in their SVG layers and in
two `widget.toml` places: discrete `value_map` values (e.g. a status
color) and string color defaults (`default = "{{token.color.primary}}"`;
a literal `#RRGGBB[AA]` also works). The built-in gauge, progress-bar and
status-indicator draw tonal cards from the semantic roles
(`color.surface.container.high`, `color.outline.variant`, `color.primary`,
status roles) and the `type.*` scale. Their SVG viewBox units are
logical px; instance geometry sets the scale, and `production.toml`
sizes them at 1.5x the viewBox on a 3840x2160 panel.

**A theme is a named set of token values, with no layout logic.** The
built-in themes are `assets/themes/<name>.toml`, compiled into the exe:
`tonal-glass` (the default), `blueprint` (after Palantir's open-source
Blueprint dark theme: neutral grays, 4 px corners, Segoe UI), and `classic`
(the look before the redesign: black/navy surfaces, white text and a blue
interaction accent). Classic declares the complete semantic palette while
retaining its legacy notification values. Shared spacing, type and easing
literals are explicit theme choices; they do not indicate missing roles.
Config picks one with the reserved key `theme`:

```toml
[design_tokens]
theme = "tonal-glass"              # unset means tonal-glass
"border.radius.medium" = "12"      # any other key overrides one token
```

Tokens resolve in three layers, each winning over the one before:
canonical defaults, then the selected theme, then the other
`[design_tokens]` entries. `tze_hud_config::themes::resolve_config_tokens`
does this as a pure function, so a later live theme swap can run it
again. An unknown theme name fails config validation with
`UnknownTheme`, and the error lists the available themes. Keep config
overrides for values specific to one deployment. The visual design
belongs in a theme.

Blueprint uses a `0.01` hover opacity for its intended opaque highest card:
`color.surface.container.highest` at alpha 1, then one `color.on_surface`
hover layer, then opaque `color.on_surface.variant` text. The selected values
meet unrounded 4.5:1 contrast in both sRGB and linear-light source-over models.
This is a config-level composition contract, not evidence of a current hover
consumer or a rendered screenshot. It does not certify a whole surface at
`opacity.surface = 0.96` over white, primary-colored text, pressed or stacked
layers, fades, or explicit config overrides. Surface opacity, pressed opacity
and the other palette values remain unchanged; consumers must validate their
actual composition.

Theme files accept canonical keys and the 41 existing portal color keys
documented in `crates/tze_hud_config/src/portal_tokens.rs`, including
`portal.frame.background`, `portal.transcript.background`, and
`portal.composer.background`. These colors use `#RRGGBB` or `#RRGGBBAA`
string values. Unknown keys, noncanonical portal number/font/timestamp keys,
malformed colors, and nonstring values remain errors. Accepting these existing
color overrides adds no canonical defaults and leaves built-in palettes and
explicit config precedence unchanged.

All three built-in themes also supply portal colors from their own semantic
roles: frame and transcript use `color.surface.container`, composer uses
`color.surface.container.highest`, code uses the high container, and dimmed
content uses the low container. Text uses the corresponding on-surface roles;
status cues keep the theme's primary, success, caution and error meanings.
Composer selection keeps the portal default, since no theme role is a
selection highlight. Portal sizes,
fonts, numeric defaults and behavior are unchanged. Explicit `[design_tokens]`
color overrides still take precedence.

**Semantic families** (Tonal Glass values; sizes are logical px):

| Family | Keys |
|---|---|
| Surfaces | `color.surface`, `color.surface.container`, `color.surface.container.{low,high,highest}`, `opacity.surface`, `opacity.scrim` |
| Roles | `color.on_surface[.variant]`, `color.outline[.variant]`, `color.primary`, `color.on_primary`, `color.[on_]primary.container`, `color.success`, `color.caution`, `color.error`, `color.[on_]caution.container`, `color.[on_]error.container` |
| State | `state.hover.opacity`, `state.pressed.opacity`, `focus.ring.width`, `focus.ring.offset` |
| Motion | `motion.{enter,exit,state}.ms`, `motion.{enter,exit}.easing` (`linear`, `standard`, `decelerate`, `accelerate`) |
| Shape | `shape.{xs,s,m,l,xl,full}` (corner radius) |
| Spacing | `space.{xs,s,m,l,xl,xxl}` (4 px grid) |
| Type | `font.sans`, `font.mono`, and `type.<role>.{family,size,line_height,weight}` for the roles `code.display`, `caption.display`, `headline.s`, `title.m`, `body.m`, `label.m`, `label.s`, `readout` |

Font family values are free-form family names. A name that is not loaded
falls back to the renderer's default face for its role (see
[Fonts](#fonts) for how names resolve). The older keys
(`color.text.*`, `color.backdrop.default`, `border.radius.*`,
`typography.*`, `color.notification.urgency.*`, and so on) still work.
`tonal-glass` maps them onto the semantic palette until each element is
restyled to read the semantic keys directly.

**Adding a theme:** add `assets/themes/<name>.toml` as a flat map of
quoted canonical keys to string values, then add it to `BUILTIN_THEMES`
in `crates/tze_hud_config/src/themes.rs`. Keys a theme leaves out keep
their canonical defaults. A unit test checks that every built-in theme
uses only canonical keys and that each value parses as its token's kind
(color, number, family name, or easing).

## Fonts

Fonts are runtime config, never an agent payload. Text uses one of three
roles, `sans`, `mono`, or `serif` (the `FontFamily` keywords `sans-serif` /
`system-ui`, `monospace`, `serif`). Each role's family is set by a free-form
design token:

```toml
[design_tokens]
"font.sans" = "Inter"         # default "IBM Plex Sans"
"font.mono" = "Cascadia Code" # default "IBM Plex Mono"
"font.serif" = "Georgia"      # default "DejaVu Serif"
```

A family name resolves to the first match, compared case-insensitively:

1. **Bundled:** `IBM Plex Sans` (400, 400 italic, 500, 600, 700), `IBM Plex
   Mono` (400, 500), `DejaVu Sans`, `DejaVu Sans Mono`, `DejaVu Serif`
   (400, 700).
2. **Fonts dir:** every `*.ttf|otf|ttc|otc` file directly in a `fonts`
   directory beside the config file is loaded at startup, if that directory
   exists. The location is fixed by convention, not configurable: a path is
   not a design value. (The runtime passes it to the compositor under the
   internal key `runtime.fonts_dir`, which `[design_tokens]` rejects with
   `CONFIG_INVALID_TOKEN_KEY`.)
3. **Windows system font, by name:** the HKLM/HKCU `...\Windows
   NT\CurrentVersion\Fonts` registry entries matching the name are loaded,
   and only those. The runtime never scans system font directories, so
   startup stays fast and the result stays deterministic. Off Windows this
   step does nothing.

A name that resolves nowhere logs one warning with the requested name and
the family used instead, and the role keeps its default. Code points a
family lacks (symbols, box drawing, Arabic, and so on) fall back per glyph
to the bundled DejaVu faces. With no font tokens set, only bundled fonts load,
which is what CI and the tests use. Widget SVG text uses the bundled fonts
with the default roles; it does not follow `font.*` yet.

Programmatic entry point: `Compositor::set_font_config(FontConfig { sans,
mono, serif, fonts_dir })`. `set_token_map` calls it with
`FontConfig::from_token_map`, and it does nothing when the config is
unchanged.

## Token budgets

`token_footprint` (CI) records o200k tokens per flow two ways: **wire** (the
full JSON-RPC request and response bodies) and **model-visible** (the tool
name and arguments plus the result text: what enters the model's context).
The budgets are enforced on model-visible tokens; the JSON-RPC envelope adds
about 50 tokens per call that the model never sees. Both measures are
baselined against regressions.

| Measure | Before T5 (wire) | Wire | Model-visible | Budget (model-visible) |
|---|---|---|---|---|
| `tools/list` | 4,418 | 554 | 517 | ≤ 900 |
| Discover (default scene) | ~430 (`list_zones`) | 181 | 113 | ≤ 150 |
| Zone publish | ~197 | 90 | 39 | ≤ 80 |
| Widget publish | ~168 | 73 | 22 | ≤ 80 |
| Portal: attach+publish+hold, poll, ack, clear | ~575 over 5 round trips | 343 over 4 | 137 over 4 | ≤ 250 |
| Discover (`production.toml`: 6 zones, 3 built-in widgets) | n/a | n/a | ~232 | ≤ 250 |
| Error | up to ~212 | 101 | 45 | ≤ 60 |

These current measurements use canonical flow version 3; its refreshed baseline
is pending owner review, with every budget unchanged.

The portal flow takes 4 round trips in the canonical fixture because the
first poll has nothing to ack; in a steady loop each `hud_input` both acks
the previous items and polls, so poll+ack is one round trip.

`integration` `poc_acceptance` checks model-visible counts on the POC
acceptance flows (`docs/scope.md`), run end to end against `production.toml`.
Its discover budget is the production row above: the three built-in widgets
add about 100 tokens, all typed parameter names and ranges the model needs to
publish, so shrinking further would drop information the model acts on.

These four parameters remain because they control behavior in the shipped bundles:

- `status-indicator.theme` selects the widget's minimal, system, or friendly
  appearance independently of the global `[design_tokens].theme`; publishers
  need that choice when selecting a status presentation.
- `tooltip_visible` (status-indicator, gauge, progress-bar) is a declared
  `f32` parameter that the runtime-local hover path drives after 180 ms to
  reveal the tooltip or readout. The loader validates that hover target and
  its SVG bindings against the parameter schema.
- `gauge.label` names the metric under the bar, so a gauge can explain which
  quantity its value represents.
- `progress-bar.label` supplies publisher-chosen task text above the bar,
  bounded to 32 bytes by its schema; hover shows the percent derived from
  `progress`.

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


Notification capture delivery (source proposal; Windows owner acceptance separate):
The UI-thread observer retains per-installation/per-press cancellation history
until the corresponding application edge is delivered. Normal native Up and
valid synthesized release activate once; cancelled or retired-window edges and
late Up do not impersonate an action. Callback code borrows no scene/runtime
owner and forwards native messages; local feedback cleanup uses the existing
settle and lock-availability continuation, rendering only after actual mutation.
No new idle poll or timer is introduced. Ambiguous completed/active receipts,
mismatched history or bounded-admission failure disable notification actions
until a clean runtime restart. Valid reentrant histories may trigger that
availability limit; its frequency has not been measured. Unrelated pointer input
continues, and this source model does not replace held-press/release owner proof.
