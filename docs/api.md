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

| Stage | MCP tool | gRPC message | Notes |
|---|---|---|---|
| Discover | `hud_surfaces` | `SessionEstablished` + current `DegradationNotice` | MCP lists usable surfaces and holdings; gRPC establishes the session |
| Claim + fill | `hud_publish` | `Publish` (zones, widgets) | Claiming is implicit in the first publish |
| Update | `hud_publish` | `Publish` | Latest wins |
| Interact | `hud_input` | none | MCP pulls input and acks it in the same call |
| Hold | `hud_hold` | `Hold` | Renews without resending content |
| Release | `hud_clear` | `Clear` | |
| Reclaim | none (runtime) | none (runtime) | Runs on expiry, disconnect, or human override; a disconnect has no live session to push to |

MCP `tools/list` has five tools. gRPC supports resident zone/widget sessions.

**Viewer dismiss.** Hovering a portal shows a close button
(`tile.close_button.*` tokens). Pressing it revokes its internal lease and
detaches the portal immediately. The next `hud_publish` to that portal
attaches afresh and reports `reattached: true`.

## MCP tools

Standard MCP over JSON-RPC 2.0 (HTTP POST): `initialize`,
`notifications/initialized` (no response), `tools/list`, and `tools/call`.
`ping` answers `{}`. The paired stdio adapter also sends the internal
`notifications/tze/session_state` on its live scope for `/admin/status`.
The runtime ignores other `notifications/*`; the paired stdio adapter consumes
`notifications/initialized` locally and does not forward it, so the runtime
must not require it. A result is one text content block of compact
JSON. Agents never send geometry, styling, or z-order. Times are
milliseconds, named `*_ms`, everywhere.

**`hud_surfaces`** has no params. It returns one compact entry per surface:

```json
{"surfaces":[
  {"s":"zone:subtitle","accepts":"text","one":true},
  {"s":"zone:notification-area","accepts":"notification","max":5,"held":true,"expires_in_ms":4200},
  {"s":"widget:gauge","params":{"level":"number 0..1","label":"string<=12"}},
  {"s":"portal:claude-main","state":"attached","pending_input":1,"unread":3}
]}
```

It includes nothing the model doesn't act on: no UUIDs, geometry, or
timestamps. Holdings are the entries with `held: true`; `held` is omitted
otherwise. Scheduled zone publications count as held before presentation;
`expires_in_ms`, when present, includes their presentation delay.
Single-occupant zones carry `one:true`; stacks carry `max:N`. A full
notification stack trims its oldest card that is not an unanswered question.
An unanswered question is a live card with a callback action and no accepted
answer. Only a stack of nothing but questions drops its oldest question. An
arriving non-question at such a stack is itself the card trimmed. The rule
is the same on MCP and gRPC admission. Regular
notification stacks present unanswered questions first, newest-first within
the question and other-card groups, so a retained question stays reachable
during a burst. Answered cards, expired cards and cards with only link or copy
actions have no question priority. Alert banners keep their severity ordering.
A portal entry carries
`unread:N` when the viewer has N output units it has not yet scrolled to; it
is omitted at zero. Check it before re-notifying. `busy:true` appears only
when another agent currently occupies a single-occupant zone.
MCP accepts lists omit `static_image`: resource upload is on gRPC. Strings with
an explicit positive byte limit appear as `string<=N`; others as `string`. Numeric discovery hints use JSON `number` with the
declared bounds (for example `number 0..1`); admission still enforces finite f32.
A whole-number count (`f32_integer = true` in the manifest) appears as
`integer lo..hi` (for example `"step":"integer 0..12"`). `2` and `2.0` are the
same count; a fraction is refused on both planes, never floored, and every
refusal of such a count reads `<widget>.<param> must be an integer in lo..hi`.
Widget discovery includes only parameters with `audience = "model"` (the
manifest default). `owner` and `runtime` parameters remain declared for local
configuration/rendering but are rejected atomically on both agent transports.

In `tools/list`, only `hud_surfaces` has `readOnlyHint:true`. Only `hud_hold`
and `hud_clear` have `idempotentHint:true`; publish/input make neither claim.


**`hud_publish`** `{surface*, content | params, ttl_ms?, delay_ms?, key?, wait_ms?, status?, expects_reply?, display_name?}`

For a one-shot notification with actions, add `wait_ms` (default 0, clamped to
55000). The card is admitted and rendering wakes before the call waits; the
absolute wait includes admission and `delay_ms`. An answer returns in this call
as `action:"<callback_id>"`; a human × adds `action:"dismiss",kind:"dismiss"`.
It is consumed only after the reply is encoded and needs no ACK. A cancelled
call releases its reservation and leaves accepted input in the ordinary FIFO,
with the same retained id on repeated `hud_input` polls. Other card answers and
portal replies stay queued; a concurrent `hud_input` cannot steal the reserved
answer. Acceptance uses the scene's monotonic clock: an answer accepted before
this deadline stays reserved even if the request resumes later; a press at or
after the deadline goes to ordinary input. HTTPS link presses do not fabricate
a callback answer. Post-admission scene acquisition uses the remaining wait
window, or 500 ms when that is longer; contention past it returns the existing
bounded `INTERNAL` error with a `retry hud_input` hint, releasing the
reservation without consuming input.

An unanswered card removed during the wait returns `gone:"expired"`,
`"replaced"` (same key), `"displaced"` (contention), or `"cleared"`. These endings
never queue an additional displacement event: a trim during an armed wait
returns `gone:"displaced"` and nothing reaches `hud_input`. If the wait ends while the card
is still admitted, the reply has the ordinary shape with `expires_in_ms`
recomputed at return; a later answer uses `hud_input` and ACK normally. Passing
`wait_ms`, including 0, for a widget, portal, or action-less notification returns
`INVALID_ARGUMENT` without publishing. Waiting counts as finite agent activity,
with the same cancellation and idle semantics as a `hud_input` wait.


- Zone: `content` is a string, or a typed object for structured zones
  (`notification` with optional `title`, `body`, integer `urgency` 0–3
  (default 1), optional `progress` and optional
  `actions: [{label, callback_id?|url|copy, role?}]`). Progress is a finite
  number from 0 through 1. The card draws a thin track in `color.outline.variant`
  and fills it in `color.primary`, independent of urgency. Omit progress to
  leave the space available. A same-key update moves the fill in place.
  An invalid value returns `INVALID_ARGUMENT` with
  `notification.progress must be a number in 0..1` on either plane.
  Each action can specify a callback, an HTTPS URL or local clipboard text.
  On MCP, an action with no target defaults to a callback whose id and answer
  are its label, byte for byte. MCP requires a non-empty label. A supplied MCP
  callback id must be non-empty. Supplying multiple targets rejects with
  `INVALID_ARGUMENT` and hint `action needs one of callback_id, url or copy`.
  In MCP, an explicitly empty target string still counts as present.
  A label-only action whose label equals another action's callback id on the
  same card rejects
  with hint `notification.actions[<i>] label repeats actions[<j>]; add a
  callback_id`; a label-only `dismiss` rejects with
  `notification.actions[<i>] label dismiss needs a callback_id`. Two explicit
  equal ids stay accepted. A label shared with a link or copy action is accepted.
  `role` is optional and MCP-only: `"destructive"` marks an irreversible
  callback answer (explicit or label-only). Any other value rejects with
  `notification.actions[<i>].role must be destructive`. A link or copy action
  with a role rejects with `notification.actions[<i>].role needs a callback action`.
  The theme draws a destructive action's label and outline in
  `color.error` (on caution and error cards the label keeps that tone's text
  color and the outline is the cue), never filled at rest. The **primary**
  action is the first action that is not destructive: it is the filled button
  and the answer chord's initial focus, so put the answer you recommend first.
  It is never taken automatically. A card whose actions are all destructive
  has no primary. gRPC actions carry no role.
  An empty MCP URL gets `action url must be https`. On gRPC, plain-string
  `callback_id` and `url` defaults count as absent. The gRPC `progress` and
  `copy` fields preserve presence. gRPC requires exactly one target and uses
  the same target hint when all three are absent.
  URLs must parse as HTTPS with a non-empty host and
  contain at most 2048 supplied UTF-8 bytes; every URL failure uses
  `action url must be https`. No network probe is performed. Both planes
  validate the whole payload before immediate admission or deferred scheduling.
  Clipboard text must contain 1 through 2048 UTF-8 bytes. An empty or
  oversized value returns `action copy must contain 1 to 2048 UTF-8 bytes`.
  A NUL character returns `action copy cannot contain NUL` because Windows
  clipboard text ends at the first NUL.
  Pressing a copy action writes the text locally and shows `Copied` or
  `Copy failed` in the button for about 1.2 seconds. The card stays with its
  current expiry and no item reaches `hud_input`. The answer chord's digit
  performs the same copy and returns focus. A card with only copy or link
  actions is eligible for ordinary stack trimming.
  Both MCP and gRPC accept at most 4 mixed actions
  (`MAX_NOTIFICATION_ACTIONS`); excess actions reject the whole publish with
  `INVALID_ARGUMENT` and hint `notification allows at most 4 actions`, before
  admission or scheduling. Notification actions form a choice card: four equal
  cells when every shaped label fits; any overflow stacks all options at full
  width. Layout uses the owner's font, theme, scale and action metric tokens,
  and unchanged frames reuse the publication's measured geometry. The title,
  body and action labels end in a visible ellipsis when they overflow their
  bounds. Later controls keep their positions. Hover an ellipsized field for
  180 ms by default or focus it with the keyboard to open the full text nearby. The
  panel flips at the display edge and keeps the source button clear. It
  scrolls within its own bounds and closes on pointer leave, focus loss or
  Escape. Fitting text opens nothing. Keep long instructions in chat or
  another reading surface. The zone's whole-card visibility bounds still apply.
  `type` may be
  omitted; the runtime infers it from what the zone accepts. For notifications,
  a nonempty `key` addresses one card within that zone and authenticated
  publisher. Matching publishes replace that card in the same slot before
  capacity admission, with fresh content, expiry and activation identity;
  the zone's enter motion does not restart. Sibling cards and another
  publisher's identical key survive. Keys are exact, case-sensitive UTF-8,
  at most 256 bytes: no trimming, normalization or truncation. Missing or
  empty keys preserve existing notification behavior. Other content retains
  its existing merge-key policy. Supplied notification urgency must be an integer 0–3; only
  omission defaults to 1. `status_bar.entries` must be a non-empty object
  whose every value is a string; an empty string removes that entry.
  Notification TTL belongs at the publish's top-level `ttl_ms`; an inner
  `content.ttl_ms` is rejected with a hint to move it.
  Activating a callback action queues its unchanged `hud_input` callback once and
  immediately disables all controls on that exact publication. A destructive
  action activates only after a press (pointer, Enter/Space, or its chord
  digit) is held for `notification.action.hold.ms` (default 600, integer
  0–3000; 0 turns the hold off): a `color.error.container` fill sweeps across
  the button (shown at once when Windows animations are off; on a card whose
  fill it would not stand out from at 3:1, such as the error card, the pair
  inverts to a `color.on_error.container` sweep under a
  `color.error.container` label; only the swept part of the label takes the
  sweep's label colour, the rest keeps its resting colour) and the action
  fires once when the hold completes, without waiting for release; an armed
  `wait_ms` returns it then. Releasing early, the pointer leaving the button,
  capture loss, key-up, Esc or another key, HUD keyboard-focus loss or safe
  mode ends the hold with no callback and the fill drains over
  `motion.state.ms`; hide-all, or the card being replaced, cleared or
  expiring, ends it with no callback and no drain. Key repeat is ignored. Frames are drawn only during a hold or its drain.
  A card an agent puts under a resting pointer (published there, shifted
  there by another card's publish, trim or expiry, or replaced by key with
  different action labels) ignores pointer presses on its actions and × for
  `notification.press_guard.ms` (default 400, integer 0–3000; 0 turns it
  off; Windows animations off leave it unchanged) and until its enter motion
  ends: no pressed fill, answer, dismiss, link launch, hold or queued item,
  and nothing tells the agent. While guarded its controls show no hover fill
  and the cursor stays the arrow; when the guard ends under a still pointer
  one wake brings both back. A dropped press restarts the interval, but a
  guard never lasts more than five intervals (2 s at the default) from when
  it began. Each agent-caused shift starts a fresh guard, so in a busy stack
  a card under a resting pointer can be re-guarded; moving the pointer off
  the card ends a guard, and the keyboard (answer chord, Tab, Enter/Space) is
  never guarded. A pointer that moved onto a card, the re-layout after the
  owner's own answer or ×, and a same-key update that keeps the action
  labels start no guard. Runtime identity
  keeps same-tick publications and replacements independent without changing
  their wall-clock timestamps or TTLs. The chosen button keeps its pressed fill during the finite `motion.exit.ms` fade
  (`motion.exit.easing`); zero exit duration removes it instantly. Explicit
  clear, expiry or replacement finishes the local presentation without another
  callback or a human-X record. Owner hiding removes its pixels/input while
  leaving the finite lifetime intact. `hud_hold` cannot prolong an
  answered card's exit. Waiting cards add no timer, countdown or periodic
  reshape/repaint; existing Stack TTL fade is the only waiting time cue.
  Bare Tab/Shift+Tab joins visible notification controls at the content focus
  cycle's wrap boundary in the order ×, Go to when shown, then publisher
  actions. Cards
  placed on several displays are visited in the one stack order they would
  draw in on a single display (unanswered questions first, newest first);
  stepping onto a card on another display moves keyboard focus to that
  display's overlay window. HUD keyboard
  focus loss to another app clears card focus even when the pointer is over
  another overlay; moving keyboard focus between HUD monitors preserves it.
  Enter/Space uses the same exact-publication activation as pointer release.
  Publication never activates an OS window or steals editor focus. Only the
  owner's press on Go to may activate one matching window. Escape, completion
  and removal release local card focus. Panel and shell routing retain precedence.
  Link actions use that same pointer/keyboard operation and exact live
  publication plus zero-based action index. A link leaves the card, lifetime,
  hold and controls in place (ordinary Tab focus also stays): no callback, readiness event, human-X or
  chosen exit is produced. Per publication/index, accepted launch attempts
  are limited to one every 2 monotonic seconds, including failures; other
  buttons/cards are independent, and retirement removes the transient state.
  The runtime sends the parser-serialized encoded URL (including encoded IDN
  host/path) to an unlocked dedicated Windows COM worker using
  `ShellExecuteExW` verb `open`, without a shell command or event-loop wait.
  Non-Windows logs a no-op. Failures log operation/error without the URL and
  leave the card unchanged. Native permission, focus/cancellation, owner
  hiding and safe mode still govern activation. A trailing
  `notification.action.link_mark` (default ↗) shares the resolved action font,
  color and scale; layout reserves its shaped width separately from the
  clipped label. Browser foreground and card-stays behavior remain a
  separate Windows owner acceptance check.
  Press feedback is local: a counted press shows "Opening…" in the
  button's label box within a frame, then "Opened" or "Couldn't open" (in
  `color.error`) for `link.feedback.ms` (default 1500), then the label
  again. A worker that cannot start shows "Couldn't open" at once and does
  not count. "Opened" means Windows accepted the request, not that the page
  loaded. The worker reports once through a completion flag and one wake; a
  handoff that never returns keeps "Opening…" without a timer. A refused
  repeat inside 2 s changes nothing, the newest press replaces older
  feedback, removing the card clears it, and keyboard activation (Enter,
  Space, an answer-chord digit) shows the same states. Nothing reaches the
  agent.
- **Go to session (Windows).** A card's caption row gains **Go to** if a
  one-shot lookup at presentation finds exactly one visible, unowned,
  uncloaked, non-tool top-level window from `[runtime].go_to_apps`. The
  caption uses the scoped session's `display_name` or folder label at publish
  time. Matching ignores case and HUD-added duplicate suffixes and requires
  the full label or phrase in the window title, bounded by title ends or
  characters outside `[A-Za-z0-9._-]`. Unscoped or gRPC cards, alert banners
  without a caption row and labels cut at 32 bytes have no Go to control.
  The card presents immediately and the result stays fixed until replacement.
  A click or Enter/Space on the focused control repeats the lookup. One
  matched window can be foregrounded and restored from minimized to its own
  placement. The card, its TTL and its actions stay, and success clears any
  saved foreground for the card keyboard visit. Digits still pick publisher
  actions. A missing or ambiguous target or a refused switch shows a local
  cue for `notification.goto.feedback.ms` (default 1200), with one attempt
  per card every two monotonic seconds. Activation sends no `hud_input` event.
  Windows Terminal exposes the active tab's title. A tmux session can raise
  its matching terminal window and cannot select a pane. A session with no
  visible window on the HUD machine has no match. A local SSH terminal can
  match when its title carries the label. Set
  `CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1` to keep a shell's folder-based
  title, which also turns off Claude Code's conversation titles and status glyph.
- `zone:ambient-background` draws its fill or image at most
  `opacity.ambient.max` (0.9 by default) opaque, so the desktop under the
  overlay always shows through.
- Widget: `params` is the typed model parameter map. The owner sets gauge
  and progress-bar `fill_color` and status-indicator `theme` through the
  existing `[[tabs.widgets]] initial_params` config. Owner values survive
  model publishes, clear and expiry. `tooltip_visible` is runtime-only; config
  overrides and agent writes reject it. Gauge/progress `severity` is
  `info|warning|error`: info uses the owner fill or `color.primary`, warning
  uses `color.caution`, error uses `color.error`; the gauge colors its ring
  arc and card hairline by severity.
  For LatestWins, Stack and MergeByKey, omitted model fields in a partial
  update retain values from the same namespace, exact key, lease and preclaim
  origin; supplied fields win. The stored publication retains them too, so
  occupancy and later peer cleanup restore the same values. Other identities
  do not seed that record from shared instance values. Replace keeps only the
  latest ordinary model map; the deadline snapshot rules below remain separate.
  Unknown audience values reject the bundle with its name, parameter and
  allowed audiences. Hidden parameter errors name owner-set/runtime-set;
  `fill_color` errors direct agents to severity. Enum errors list allowed values;
  wrong-type/f32 errors state the expected type/range. F32 values must be finite and
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
  retain their explicit `transition_ms`. Both planes snap to 0 while
  Windows "Show animations" is off (see the OS motion layer under
  [design tokens](#design-tokens-and-themes)). A positive duration eases f32 and
  color params from what is on screen; enum and string params and a widget's
  first appearance snap. Simplified degradation snaps and stops transitions.
  The runtime wakes only until the transition lands. A zero duration cancels
  an older transition, including a repeated target, and snaps to the newest
  value. Standalone unconfigured MCP servers retain zero duration. Changing
  the selected theme takes effect at config load/restart; live theme reload
  is separate work. Zone enter/exit durations use the resolved motion tokens
  unless explicitly overridden by zone policy, and Stack TTL fades use the
  exit duration/curve without extending authoritative expiry.
  Shipped bundles also include `countdown` (`ends_in_ms`, `label<=24`,
  `severity`), `kpi` (`value<=12`, `label<=24`, `trend=up|down|flat`,
  `severity`), `elapsed` (`started_ms_ago`, `label<=24`, `severity`: the
  countdown's card, counting up), and `stepper` (integer `step` 0..12,
  integer `total` 1..12, `state=active|blocked|done`, `label<=24`). Production
  config instantiates only `main-countdown` from these additions; elapsed, KPI
  and stepper are available for owner configuration. A fractional count is rejected; a step above total
  fills every dot; the current dot uses primary/caution/success for active/blocked/done.

  `deadline` appears in discovery as `deadline<=24h`. Send integer relative
  milliseconds 0..86400000 (`ends_in_ms` in the countdown bundle); gRPC uses
  the `deadline_ms` integer oneof, never the f32 oneof. Both planes reject
  negatives, fractions, overflow and wrong types with that bound in the hint.
  The runtime snapshots the absolute wall deadline when content actually
  presents, including a late delayed drain. A label-only republish under the
  same publisher/key preserves it; explicitly resending the duration resets it.
  Remaining time rounds up: `125 min` → `2 h 05`, `30 min` → `30 min`,
  `90 s` → `1:30`.

  A deadline declared `deadline_counts = "up"` appears as `elapsed<=24h`: the
  same integer milliseconds 0..86400000 (gRPC `deadline_ms`), read as "started
  this long ago", with the same snapshot, republish and reset rules and that
  bound in every refusal. One publish such as
  `{"surface":"widget:<instance>","params":{"started_ms_ago":0,"label":"Tests"}}`
  keeps running with no further call. Elapsed time rounds down, so it never
  shows time not yet passed: `0:00`…`1:59` (each second), `2 min`…`59 min`,
  `1 h 00`…`23 h 59` (each minute), then `1 d 00 h` onward (each hour); at
  `99 d 23 h` it holds and stops waking, and a backwards clock step shows
  `0:00`.

  Only a visible published countdown or elapsed timer schedules its next
  display change: for a countdown, minute granularity above two minutes and
  second granularity in the last two minutes; at `0:00` it retains severity
  and stops waking. Hidden, cleared, absent and safe-mode countdowns and
  elapsed timers schedule no wake; showing or resuming draws the current
  time once.
  Safe mode freezes widget publication TTL and the owner's auto-clear allowance,
  while the absolute countdown keeps passing; resume displays current wall time.
  An already accepted delayed widget stays pending through safe mode, without
  an overdue retry wake. The queued publication presents after resume. Its
  deadline snapshot and relative TTL start at that presentation.
  Hover remains runtime-local. SVG `threshold` bindings compare f32 `>= k`
  into opacity 0/1; optional `attr_min`/`attr_max` in 0..1 select below/above
  outputs for mutually exclusive dot branches, without an expression language.
- Portal: the first publish to `portal:<id>` attaches (`display_name` is
  optional). Omit content for a bare attach or status-only update.
  `content` appends only the new output text; `status` labels agent activity
  (`active` or `degraded`), and `expects_reply` arms the composer. `key`
  replaces the newest output
  with the same key (progress lines). Portals are per agent: another agent's
  `portal:<id>` with the same id is a separate portal. Within one agent, a
  portal id belongs to the session that attached it until it detaches or the
  runtime reclaims it; another session's publish is `NOT_ALLOWED` naming the
  owner's label, and attaching after a reclaim reports `reattached: false`
  (it is that session's first attachment). Fresh expanded portals
  request 1440×1080 before fitting each dimension to the display, subject to
  the existing scaled legibility minima. Fitting adds reading room at the same
  base font; compact portals remain 420×96. Existing live geometry and saved
  overrides retain their current ownership. This size change is separate from
  the portal theme migration and later owner visual acceptance.
  The native unread badge counts newly appended output units until the viewer
  reaches the measured OUTPUT tail. Render batches, keyed replacements and
  viewer replies do not inflate it; backscroll without new output keeps it clear.
  At zero unread, the portal badge background, label and click target disappear
  together, including when the viewer scrolls back through already-read output.
  The local header close reclaims that portal; a later publish reattaches it.
  OUTPUT is readable in place (hud-ljhku.2). Markdown `[text](url)`, `<url>`
  and bare `https://` URLs become links only when the URL is HTTPS with a host
  and at most 2048 bytes; other links render as plain text and are never
  activatable. Only link text is drawn; hovering shows the target host in a
  pill and an underline, and the cursor is an I-beam over text, a hand over a
  link and an arrow elsewhere. A link opens on release when the pointer
  moved within the system drag threshold (`SM_CXDRAG`/`SM_CYDRAG`; 4 px off
  Windows), through the same browser handoff as card links, once
  per 2 s per link; a double-click opens it once and selects nothing, and a
  press-drag on a link selects instead. While the pressed link is hovered or
  focused, its hint pill shows the card links' press feedback ("Opening…",
  then "Opened" or "Couldn't open") in place of the host; "Opened" means
  Windows accepted the request, not that the page loaded. Dragging selects text (double-click a
  word, triple-click a paragraph); the selection keeps its bytes across
  appends and head trims (it clears once its text is trimmed or rewritten)
  and stops follow-tail until it is cleared. Ctrl+C copies the selection as
  plain text (CRLF line breaks on Windows); Esc or any press outside OUTPUT
  clears it, and hovering a fenced code block
  shows a Copy button that copies the block. Tab from a focused composer moves
  through visible links and Copy buttons (Enter/Space activates, Shift+Tab off
  the first returns to the composer); the focused stop survives new output
  while its text and target remain. OUTPUT reading focus (hud-ohgc9.4) means
  the HUD holds the keyboard for a portal, the caret is not in its composer,
  and an OUTPUT selection exists, a link or Copy stop is focused, or the
  portal tile is focused. An OUTPUT selection takes reading focus from the
  caret, so Home and End scroll OUTPUT until the selection is cleared. With
  reading focus, Home scrolls OUTPUT to its first retained line, End to the
  tail (as jump-to-latest does), and PageDown/Space and PageUp/Shift+Space
  page down and up by one keyboard page. Space on a focused link or Copy stop
  still activates it, paging keeps the focused stop or selection, and a key
  that moves nothing draws no frame. With the caret in the composer these
  remain text keys. With only the portal tile focused, PageUp/PageDown also
  still deliver the tile's existing ScrollUp/ScrollDown command; otherwise
  none of this reaches the agent: no `hud_input` item, no MCP or gRPC change.
- Omitted zone `ttl_ms` uses 60000, except a notification with actions uses
  the owner's live `[runtime] answer_window_ms` (default 60000). An explicit
  `ttl_ms` always wins, including 0 (held until cleared); the owner window is
  a default, not a cap. The accepted publication stores one absolute expiry,
  counted from presentation, and both `hud_publish` and `hud_surfaces` report
  it as `expires_in_ms`, including any delay. A later setting change does not
  retime active or already-scheduled cards. Widgets are durable unless given one. A widget's owner-set "Clears after" limit
  (`[[tabs.widgets]] auto_clear_ms`) caps every publication, held or not, at
  that long after it was published; `hud_publish`, `hud_hold` and
  `hud_surfaces` report the capped expiry. On a portal, an optional `ttl_ms` holds the
  portal atomically with this publish, with the same semantics as `hud_hold`.
  Safe mode preserves a healthy portal's remaining idle allowance and finite
  hold; an indefinite hold remains held. Publishing and holding during pause
  return `SAFE_MODE_ACTIVE`, with no replay buffer. Input polling remains
  available and refreshes real activity without double-counting pause time.
  Input expiry, existing orphan grace and the maximum scene suspension continue.
- `delay_ms` (zones and widgets, ≤ 300000) holds the content until then (invariant 1);
  the expiry counts from presentation, and `ttl_ms: 0` holds a delayed
  publish until cleared just like an immediate one.
- Returns `{"ok":true,"expires_in_ms":8000}` when a TTL is set. Portal
  publishes also return `reattached` (true when this call replaces a
  runtime-reclaimed portal) and `pending_input` (queued replies). First attach,
  live update, and attach after explicit clear report false. A portal publish
  adds `unread:N` when N output units the viewer had not reached existed
  before this publish; it is omitted at zero. The response
  describes the completed publish; it does not echo content.
- `clipped` reports text the renderer would cut, measured at admission with
  the drawing shaper, current design tokens and display geometry, as an
  object from field name to the number of characters (Unicode scalar values)
  of the sent string that are drawn, not counting the ellipsis:
  `{"clipped":{"body":58},"expires_in_ms":60000,"ok":true}`. Notifications
  report any of `"title"`, `"body"` and `"actions"` (a label wider than its
  button). The action count is the shortest drawn prefix among the cut labels.
  It stops before the ellipsis and before the original label's first line break.
  Widgets report the string parameters set in this call whose drawn text
  overflows the widget. A status-bar publish reports
  each entry it sets whose value is cut, by entry key, measured against the
  merged bar as it draws right after this publish (0 when the full bar does
  not draw that entry; removals and values that fit report nothing; a cut
  entry key is not reported). The first N characters of what
  was sent always fit; a rewrite of N characters usually fits, but glyph
  widths differ, so it may report again. The field is omitted when everything
  fits, so replies are unchanged byte for byte, and never appears for
  stream-text zones or portals. It also rides on wait replies (`action`,
  `gone`, deadline). React by shortening to the count, or by moving detail
  behind a link action. A HUD built before this shape returns a list of the
  cut field names instead.

  An immediate notification uses the bounds and scale of the display where
  it draws at admission. Delayed notifications use the notification zone's
  display for this estimate because their placement is decided at presentation.

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
Positive zone/widget Hold TTLs use saturating millisecond conversion and absolute
expiry arithmetic across the admitted `u64` range; they cannot wrap to a short
lifetime. The widget owner's auto-clear limit still applies.
While agents are paused, fresh MCP and gRPC Hold requests return
`SAFE_MODE_ACTIVE` before changing active or delayed content, expiry or leases.
A request waiting for the scene lock rechecks pause when it acquires it;
resuming restores ordinary Hold behavior. Replaying an already completed gRPC
sequence still returns its cached result without applying another Hold.
On a portal, `ttl_ms` keeps it (and its transcript) attached for that long,
or until `hud_clear` when 0, even with no other calls. Without a hold, a
portal is degraded after 30 s with no publish, poll, or hold, and reclaimed
30 s later.

**`hud_clear`** `{surface*, key?}` releases a zone publication, a widget
instance, or a portal (detach). With a nonempty key, it clears only that
publisher's matching notification in a notification-capable zone, including
matching delayed notification publishes. Sibling keys, foreign publishers and
nonnotification content in mixed zones or other scheduled publications survive.
A missing match succeeds. Cancelled publications leave no wake deadline.
Missing or empty keys clear all of the caller's content on the surface,
including delayed publications.
The same exact 256-byte key bound applies before mutation. Nonempty keys on
widget, portal or nonnotification-zone clears return `INVALID_ARGUMENT`.
The gRPC `Hold` and `Clear` behave the same on zones. An internal scene
`ClearZone` mutation clears only active publications and never cancels pending ones.

**`hud_input`** `{ack?: [input_id], wait_ms?, max_items?}`

- Returns input from every surface the agent holds, oldest first:
  `{"items":[{"id":"i7","s":"portal:claude-main","text":"yes, ship it"}],"remaining":0}`.
- Notification callbacks keep their existing shape:
  `{"id":…,"s":"zone:notification-area","action":"approve"}`. A validated
  human notification × uses `{"id":…,"s":"zone:notification-area","action":"dismiss","kind":"dismiss"}`.
  A publisher callback named `dismiss` has no `kind` field. Callback activation
  already disables its notification locally and finishes a finite chosen exit;
  it emits no second human-X event. Keyed callbacks and human-X items also
  include `"key":"…"`, snapshotted from the exact activated card before its
  removal. Unkeyed items omit it. Replacement, clear and expiry never rewrite
  or retract input already accepted into the scene FIFO, retained by MCP or
  encoded in a response; its key and ACK id remain stable.
  TTL expiry, `hud_clear` and stale/unmatched releases do not emit human-X input.
- A card trimmed from a full notification stack before anyone answered it
  queues one `{"id":…,"s":"zone:notification-area","kind":"displaced"}` item
  (with `"key"` when the card was keyed) for its publisher only. Expiry,
  `hud_clear`, same-key replacement, answered or already-expired cards, and a
  card whose armed `wait_ms` call returns `gone:"displaced"` queue nothing.
  Receipts are bounded for publishers that never poll: a newer receipt for the
  same key replaces the older one, and at most 5 wait per publisher (the oldest
  receipt drops first; accepted human events are never dropped for them).
- Accepted notification events are retained in a growable per-publisher FIFO,
  with stable ids until `ack`; one publisher cannot evict or acknowledge another's
  records. Retention is in memory, with no crash/disk durability claim. Portal
  replies keep their existing admission bounds, TTL, clear and reclaim rules.
  A key identifies a reusable visual slot, not an answer-generation nonce.
  Under owner option D, an old accepted human-X or delayed keyed clear may
  cancel a replacement card, but must never answer it. Broker callback-nonce
  routing, cancelled-question terminal fallback, cancelled-nudge silence and
  the executing stale-X/old-callback race proof belong to `hud-bz9bv.7`;
  this key API adds no revision, conditional clear or input-retirement rule.
- `ack` confirms earlier items once per call; unacked items are redelivered.
  `max_items` limits the result and `remaining` reports the retained backlog;
  zero returns the backlog without consuming it. Notification FIFO items precede
  portal replies in their existing chronological order.
- `wait_ms` defaults to 0 and caps at 55000. The caller's armed readiness wait
  wakes on committed input and uses one absolute deadline with a final queue
  check. It neither polls every 150 ms nor changes the stdio adapter's 60 s cap.
  A finite active poll protects only its agent from idle degradation, including
  first attachment; completion/cancellation releases ownership without a future
  heartbeat. Explicit upstream loss, reclaim, holds and input TTL still apply.
- Acquiring the scene or portal Hub waits within the same request deadline,
  or 500 ms from the attempt when that is later, so a poll with no `wait_ms`
  or an ACK outlasts a transient holder (a frame turn) instead of failing.
  A contended Hub waits cooperatively without blocking the async executor;
  ordinary empty input waits remain event-driven. If contention outlasts that
  bound, the call can return `INTERNAL` with `input state busy; retry hud_input`.
  Repeating the same ACK is safe:
  acknowledgements are idempotent and unacknowledged input keeps its stable id.
  The production `poc_demo` client retries only this exact typed input error,
  with one 100 ms recovery deadline and 10 ms cooperative backoff. A retried
  positive `wait_ms` may still wait until the original input deadline, with
  a fixed 100 ms response allowance after it. Default-zero polls and ACK
  arguments stay unchanged and recovery remains bounded by 100 ms. Every
  actual attempt prints its request/error
  or success token cost. Other tool, transport and protocol errors fail normally;
  the canonical token calibration does not retry or omit an attempt.
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
allow = ["zone:*", "widget:gauge", "portal"]
```

Before upgrading an existing `agents.toml`, remove every `tiles` allow entry,
including one mixed with `*`; the new binary rejects it. Preserve agent ids,
PSK hashes, `paired_at`, comments and file ACLs. Choose only the zone, widget
and portal grants the agent needs. Tile-only agents may keep an empty allow
list or be retired. Zone, widget and portal grants do not grant resident gRPC
resource upload access. Through `agents.toml`, only `*` grants the internal
`upload_resource` permission; that name is not a valid allow entry. Resident
input/focus, topology and frame-telemetry feeds are removed regardless of grant.
Choosing `*` widens access to all non-admin surfaces and permissions and must
be an explicit owner decision. There is no alias, automatic migration or
automatic grant widening.

Validate the edited file with the new binary and restart it before reconnecting
agents. An invalid or unreadable `agents.toml` exits strict startup with code 1.
A missing file starts with an empty agent directory; an invalid or unreadable
file does the same only in debug/dev builds with
`TZE_HUD_DEV_ALLOW_INSECURE_STARTUP=1`. An empty directory authenticates no
agents until one is paired.

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
  show it in a copyable setup prompt instead. A late successfully bound MCP
  address refreshes an already displayed, still-valid startup/`--pair` card,
  preserving its code and original expiration. Failed binds, interface removal
  and gRPC-only changes do not refresh that card. The agent trades it for a key:

  ```
  curl -s host:9090/pair -d '{"agent":"claude","code":"482913"}'
  -> {"agent":"claude","psk":"<64 hex>","mcp":"http://100.x.y.z:9090/mcp","grpc":"100.x.y.z:50051"}
  ```

  The endpoints come from each transport's successful listeners. `grpc` is
  `null` while gRPC is disabled or has not bound yet (including during handoff);
  an MCP bind alone does not advertise the corresponding gRPC address.

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
- **Sessions.** `initialize` opens an MCP session: the response carries an
  `Mcp-Session-Id` header (the result body is unchanged), and calls that send
  it back act within that session. Each session of an agent owns its own
  content, holdings, portals and input: `hud_input` returns and ACKs only its
  items and `remaining` counts only its backlog; a keyless `hud_clear` clears
  only its content; `hud_surfaces` marks `held` only for its holdings, lists
  only its portals (with their `unread`), and shows another session's content in a single-occupant
  zone as `busy`; a notification `key` names a slot per session; answers,
  `wait_ms` waits and displaced receipts go to the publishing session; idle
  portal liveness counts per session. Calls without the header are the
  agent's one unscoped caller and behave as before among themselves (scripts,
  gRPC publications); they are a session of their own toward scoped ones.
  Unchanged per agent: the allowlist, budgets (a stack zone's publisher cap
  counts the whole agent), and every human override (Clear, Hide, safe mode,
  unpair act on all sessions). An optional label rides in `initialize`
  `params._meta["tze-hud/label"]`, outside model context: the runtime strips
  control characters, cuts it to 32 bytes on a character boundary (nothing
  left means no label) and suffixes a duplicate live label with the lowest
  free ` (2)`, ` (3)`. A labeled session's notification cards draw the label
  as one ellipsized caption line above the title, in the body type role and
  secondary text colour; the card grows by that line and the text column,
  `clipped` counts and actions below are unchanged. A card keeps the label it
  was published under; unscoped and unlabeled cards, and alert banners, draw
  no caption. A
  session's portal header shows its label until a `display_name` names it,
  and `display_name` on a session's zone publish replaces its label for that
  card and later ones (unscoped zone and all widget publishes still refuse
  `display_name`). `DELETE /mcp` with the header ends the session as an
  explicit release (`202`): its zone and widget content, delayed publishes
  and portals clear, its un-ACKed input is discarded, and its armed waits
  return at once (`hud_input` with its deadline reply, a `wait_ms` publish
  with `"gone":"cleared"`). A crashed client sends nothing; its content
  follows the ordinary TTL and portal idle rules. An unknown or ended id gets
  a bare `404` before anything runs; the client re-initializes. An agent
  holds at most 64 sessions; opening another releases the least-recently-
  called one, preferring one with nothing held, as on `DELETE`. Session
  start and end are logged, and `/admin/status` `sessions` lists each live
  one as `{agent, label, holdings}`. A script that calls `initialize` but
  never sends the header back opens a session it never uses; it shows there
  unlabeled with no holdings, and the bound releases such sessions first.
- `admin` is an operator entry, outside the model surface: it opens
  `GET /admin/status`, `GET /admin/logs`, and `GET /admin/screenshot` on the
  MCP port, and `*` does not grant it. `/admin/status` includes `tailnet_inbound` (`state`: `allowed`, `blocked`, `unknown`, `not_applicable`; `reason`, `rule` and `fix` when `blocked`; `error` when `unknown`): whether Windows Firewall lets remote agents reach the tailnet bind, and `safe_mode_hotkey`, `panel_hotkey`, `pairing_hotkey`, `answer_hotkey` and `hide_hotkey`
  (each `chord`, `registered`, `error`; `null` when no hotkey is active; `registered: null` while pending): `registered: false`
  means another program owns that chord, or (`error` starting `not bound:`) the config left the hide chord unbound; for `safe_mode_hotkey`, its error names the tray icon's Pause as the independent override (a refused `hide_hotkey` loses nothing else: the startup banner names Hide all in the panel or tray). `capture_exclusion` is `null` off Windows, else `{on, supported, applied, reason, failed}`: `reason` is `null`, `"remote"` or `"unsupported"` while on, and `failed` lists `displays` indexes whose window refused the affinity. The blocked
  firewall `fix` names `tze_hud.exe --allow-remote` with the same listen-port
  overrides as startup; see [the reversible Windows helper](operations/windows-install.md#remote-agents).
  This policy-derived result is cached for five seconds, not a network probe.
  `/admin/status` `idle_since_ms` is the time since the last presented frame
  (since start before the first); a growing value means the HUD has settled.
  A focused composer's caret blinks at the Windows caret blink rate (530 ms off Windows), so while a
  composer has focus treat `idle_since_ms >= 300` as settled; with Windows caret blinking off the caret is steady and presents nothing.
  `os_prefs` is `{text_scale, caret_blink_ms}`, the Windows text size and
  caret blink half-period (`0` = blinking off) the HUD is following
  ([Windows preferences](#design-tokens-and-themes)); `null` without a config.
  `GET /admin/status?cpu=0` answers at once with `cpu_pct_2s` null instead of
  sampling CPU for 2 s, for harnesses that poll status.
  The additive `holdings` and `warnings` fields contain **admin-only,
  content-free diagnostics**. They do not extend `hud_surfaces` or ordinary
  agent discovery. Published text, notification titles/actions/keys, widget
  parameters, transcript/input, session/lease IDs, credentials, paths and raw
  configuration/error detail are excluded from these two fields.

  `holdings` is `{state, sources, agents, omitted_agents, omitted_records,
  omission_reasons}`. `state` is `complete`, `partial` or `unavailable`;
  `sources.scene` is `available|busy|unavailable` and `sources.portals` also
  permits `not_configured`. Each agent group is `{id, surfaces,
  omitted_records}`; each surface row is `{surface, kind, phase,
  expires_in_ms, expiry_kind, count}`. Full canonical addresses use
  `zone:`, `widget:`, `tile:` or `portal:`. Owners come from retained records,
  including agents removed from configuration. Accepted pending zone/widget
  publications count; future tile creations do not. Runtime backing tiles
  are excluded; authenticated hub portals count once. Identical metadata and
  absolute deadlines coalesce into `count`; differing deadlines remain distinct
  even when rounded remaining milliseconds match.

  `phase` is `active|scheduled|suspended|orphaned|cleanup_due|unknown`.
  `expiry_kind` is `content|lease|orphan_grace|suspension_limit|until_clear|
  activity|idle_reclaim|unknown`. Finite remaining time uses the earliest
  applicable production cleanup bound, with positive fractions rounded up to
  milliseconds. Ties prefer content, lease, orphan grace, then suspension limit.
  A retained record already due reports `cleanup_due` and zero until normal
  cleanup removes it; status never sweeps, applies or renews content.
  Suspended lease TTL is paused but its maximum suspension still applies.
  Widget content clocks pause with widgets. Pending relative widget TTL,
  auto-clear and implicit notification urgency TTL depend on actual application:
  they report unknown content expiry until materialized, while independent
  lease/origin bounds remain observable. A live portal reports activity-dependent
  null expiry (`activity`), or `until_clear` for an indefinite hold. A finite
  hold end is not a reclaim deadline. Actual degradation starts the finite
  `idle_reclaim` bound, overriding holds and paused liveness.

  Each source is sampled independently with immediate `try_lock`, releasing
  the scene before reading the hub. Contention reports `busy`, never healthy
  emptiness; counts for unreadable sources are null. This is not a transactional
  cross-source snapshot. Retention is bounded to 64 owners and 256 rows, sorted
  by owner/address/kind/phase/basis/absolute deadline. Owner IDs over 128 UTF-8
  bytes and full surface IDs over 256 bytes are omitted whole. Compact encoded
  `holdings` is at most 49,152 bytes; trimming counts original records, including
  aggregate counts. Omission reasons are `cardinality|bytes|oversize_id|busy|
  unavailable`. `omitted_agents` is exact within the bounded owner set, otherwise
  null when distinct omitted owners cannot be counted. Groups without retained
  rows are absent, with their omissions still reported.

  `warnings` is `{state, items, truncated, omitted_events, omission_reasons}`.
  State is `pending` until the first configured compositor font initialization
  finishes, then `complete`; an absent/poisoned producer is `unavailable`.
  Items are `{source, code, subject, fallback}`: `widget_bundle` failures from
  the actual startup scan/instance creation, and `font` directory/file load or
  requested-role fallback failures from the actual font builder. Codes are
  fixed; subjects are safe configured ordinals or fixed font roles, and fallback
  families are bundled defaults. Warnings are deduplicated per startup, capped
  at 64 retained entries and 16,000 encoded bytes. Dropped warning *events* are
  counted without claiming distinctness beyond that cap; reasons are
  `cardinality|bytes`. Readiness is independent of truncation. Complete empty
  warnings mean both producers finished; later theme changes do not rewrite
  startup history. The combined encoded `{holdings,warnings}` is at most
  65,536 bytes; existing unrelated status fields keep their contracts.

  `portals` (`null` before the windowed runtime starts or while the scene stays
  locked for 100 ms) is read-only portal UI state for out-of-process validation,
  one entry per portal: `s` (`portal:<id>`), `state`, `rect`, `display` (index
  into `displays`), `header_text` (the materialized header node), `panes`
  (`header`, `input`, `output`, `composer` hit region, `close` and
  `unread_badge`, the largest exposed piece of each runtime control as of
  the last frame; each `{x, y, w, h}` in scene pixels or `null` when not drawn),
  `focus` (`composer`, `tile` or `none`), `hover` (`composer` or `none`),
  `cursor` (`ibeam` over OUTPUT text, `hand` over a link or Copy button,
  otherwise `arrow`, including when the pointer is not over that portal's
  OUTPUT; present for each portal whose OUTPUT is measured), `link_hint`
  (the hovered or focused link's host, or `null`), `selection_chars` (OUTPUT
  selection length, never its text),
  `output` `{first_visible_line, last_visible_line, at_tail, unread,
  writing_caret}` (`writing_caret` observes the compositor's actual trailing
  cursor marker; `at_tail`
  is the follow-tail state new output obeys) and `input` `{at_tail,
  draft_chars}`; the visible lines and `input.at_tail` are `null` while their
  document is being measured. The composer's draft length is reported,
  whether it is focused or kept unfocused, never its text. It is computed per
  request; an idle HUD does no extra work.
  `notifications` and `widgets` share that single bounded scene acquisition:
  both are `null` before the windowed runtime starts or after 100 ms of scene
  contention, otherwise arrays (empty when nothing was drawn). Geometry comes
  from the compositor's last frame, with no diagnostic timer or redraw.
  A notification entry has `s` (`zone:<name>`), `key` (or `null`), `rect`,
  `display` (index into `displays` of the monitor the card is drawn on,
  which for a placed question is its placement), `actions` (`{label, kind, rect}`,
  with `kind` `callback`, `url` or `copy`). A copy action can carry
  `feedback` (`copied` or `copy_failed`). The entry also has the
  `close` rect, `goto` (Go to rect or `null`), `focus` (action index,
  `"close"`, `"goto"` or `null`), `chosen` (action
  index or `null`), `state` (`entering`, `shown`, `exiting`) and `guarded`
  (presses on it are ignored by the fresh-card press guard). Entries follow
  drawn stack order; chosen cards retain their bounds during their exit even
  after their input targets disappear. Ellipsized fields add `reveal_sources`
  with the field name or action index and its painted rect. An open `reveal`
  gives the field, panel rect and whether the text is scrollable or scrolled.
  Card bodies, link URLs and clipboard values are omitted.
  A widget entry has `s` (`widget:<instance>`), its snapped drawn `rect`,
  `display`, `hover_target` (the runtime's declared trigger rect, or `null`)
  and `tooltip_visible` (the current runtime parameter is at least 0.5).
  Rects use `{x, y, w, h}` in scene pixels, and `display` indexes `displays`.
  `cursor` (top level) is the pointer shape the runtime last applied:
  `arrow`, `hand`, `ibeam`, `grab`, `grabbing`, `ew-resize`, `ns-resize`,
  `nesw-resize` or `nwse-resize`; `null` before the windowed runtime starts,
  and in overlay mode while the overlay is click-through (the app beneath
  owns the pointer).
  `link_feedback` (top level) is the pressed link's local feedback:
  `opening`, `opened`, `failed`, or `null`; never the URL.
  `link_launches` monotonically counts accepted runtime browser handoffs after
  the per-publication/action (card) or per-link (portal OUTPUT) 2 s limit. A failed worker spawn does not count;
  a browser navigation failure does. The non-Windows no-op counts too. Scene
  contention leaves this counter readable. `window_activations` counts
  successful owner-pressed Go to switches and remains readable during scene
  contention. These diagnostics are admin-only;
  MCP/gRPC responses and their token budgets are unchanged.
  `clipboard_writes` monotonically counts HUD clipboard attempts for pairing
  codes, OUTPUT text and copy actions. The non-Windows stub counts too.
  `/admin/status` `displays` lists the overlaid monitors, primary first:
  `{key, name, x, y, width, height, primary, zones, window_state}` in scene pixels (the primary's
  top-left is 0,0; others may be negative) and the zones configured onto each.
  `key` is the `[displays."<key>"]` key (see Configuration); `name` is the OS
  display name.
  On Windows, `window_state` samples `{visible, iconic, topmost, cloaked}` from
  the live HWND (`cloaked: null` if DWM cannot report it); the whole object is
  `null` outside Windows or when the window is unavailable. Status preserves the
  observed state and queues a no-activate visibility/topmost reassertion on the
  window thread when it drifts. Display changes, system resume and console
  display power-on also trigger that check; there is no per-frame polling.
  Shell cloaking can remain after reassertion and is still reported. Confirm
  visibility on a virtual desktop by looking at the physical display. DWM
  cloaking, present counters and compositor screenshots are diagnostic signals.
  Ordinary presentation retains an image deferred by an in-flight encode until
  it presents or is explicitly invalidated. Each window registers one coalesced
  encoding-completion wake; this does not create a scene mutation or periodic
  redraw. Ready images also bypass an unchanged-cursor idle shortcut. A panel
  interaction or another publish is not required to retry that owed image.
  `unplaced_zones` lists `{zone, display, reason}` placements with no overlay
  window, rendering on the primary instead (`display` is the key as
  configured): `reason` is `legacy_key` (a retired Windows enumeration key),
  `not_connected` (no connected monitor matches it), or
  `overlay_failed` when that monitor's overlay surface was lost and recreated
  3 times in a row (it is retried when the monitor is reconnected).
  `/admin/screenshot` returns a PNG of the frame the compositor draws (not an
  OS capture; overlay alpha is as composited) for one display:
  `?display=<i>` indexes `displays` (default 0, the primary; 404
  `NO_SUCH_DISPLAY` for an index not in `displays`, or one whose overlay closed
  since `displays` was read; indexes shift on hot-plug, so re-read status). One render on request, no cost
  while idle, never cached. Capture does not advance the last-rendered scene or
  geometry ledger or the primary acquisition/submission/present counters; an
  unpresented scene change still needs its ordinary frame. One at a time (429 `BUSY`), 503 `UNAVAILABLE` after
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
- A portal argument refusal names the field and its limit: `portal id is
  1..128 bytes`, `display_name is 1..128 bytes`, `portal key is 1..128
  bytes; omit key to append` (`INVALID_ARGUMENT`), and `portal publish is at
  most 16384 bytes; split it` (`CONTENT_REJECTED`). A scene refusal whose own
  text is internal, and can carry ids, returns one fixed hint per code
  instead, such as `request not accepted; call hud_surfaces and resend`.
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
| `NOT_ALLOWED` | The agent's `allow` list doesn't cover the surface, or another of its sessions owns the portal id |
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
| `BUDGET_EXCEEDED` | A portal rate limit or full input queue; the hint names the next call |
| `UNAVAILABLE` | The portal service isn't running or the HUD is unavailable |
| `INTERNAL` | Runtime fault |

## gRPC (resident sessions)

There is one bidirectional `Session` stream
(`crates/tze_hud_protocol/proto/session.proto`), and its message set is cut
down to the lifecycle.

| Client → server | Server → client |
|---|---|
| `SessionInit{agent_id, auth_credential}` | `SessionEstablished{session_id, namespace, heartbeat_interval_ms, …}`, then current `DegradationNotice` |
| `Publish{surface, content \| params, ttl_ms, present_at_us?, expires_at_us?, key?}` | `RequestResult{seq, ok, code?, hint?, ttl_ms?}` |
| `Clear{surface, key?}` (zone or widget) | `RequestResult` |
| `Hold{surface, ttl_ms}` (zone or widget) | `SessionSuspended` / `SessionResumed` (safe mode) |
| `ResourceUpload*` (images), `Heartbeat`, `SessionClose` | Resource upload replies, `Heartbeat`, `DegradationNotice` |

`Publish.key` remains wire field 7. `Clear.key` is additive string field 2;
its empty protobuf default preserves old clients. Keyed notification replacement,
selective clear, byte validation and authenticated publisher isolation match MCP.
`NotificationActionProto` adds only `string url = 3`; label (1) and
callback_id (2) retain their wire identities. An absent/default URL with a valid
callback preserves callback behavior. There is no new gRPC notification
callback/dismiss stream.

Public `ClaimTile`, `MutationBatch`, `SceneSnapshot`, `Reclaimed`, placement
and tile surfaces are removed. Their wire tags and names stay reserved.
Only structured PSK credentials authenticate `SessionInit`. Legacy PSKs,
local-socket credentials and protocol-version negotiation are removed.
The handshake sends no scene content, foreign portal transcript or draft.

A transport termination, explicit close, or termination by the existing
receive-timeout handling ends the resident session. The runtime clears that
session's active and scheduled publications, aborts its pending uploads and
returns its admission slot. A client reconnects with `SessionInit` to establish
a fresh origin and republish its desired state. `SessionResumed` means leaving
safe mode on a connected stream. The removed reconnect envelope and response
and their token fields have reserved tags and names. A removed resume envelope
as the first message receives `SessionError(INVALID_HANDSHAKE)`; a
sequence-valid removed-only envelope on an active stream follows the existing
`INVALID_ARGUMENT` request path. An old token field on a valid `SessionInit`
is ignored by protobuf.

A successful RGBA8, PNG or JPEG upload grants the current authenticated pairing
use of that image through `StaticImageRef` in a zone allowing static images.
A first grant requires the matching bytes, type and dimensions even when another
namespace stored the same hash. PNG/JPEG dimensions may be omitted as zero;
nonzero dimensions must match the decoded image. Digest-only reuse requires a
current matching grant. Unknown, foreign or unusable image references return
`INVALID_ARGUMENT` with the same resource hint on a non-ephemeral zone; an
ephemeral zone keeps its fire-and-forget behavior. Rejected references add no
active or scheduled content. Admission and due application both check the
current pairing. Unpair or re-pair invalidates that pairing's grants; ordinary
allow-list edits retain connected permissions and grants. A revoked stream
cannot Clear, Hold or replace its fresh pairing's content. SVG and font uploads
keep their existing behavior and do not grant resident static-image use.

Resident input events, subscription confirmations, element reposition notices and
frame-presented telemetry are removed. Their envelope and handshake tags and names
stay reserved. Keyboard, pointer, focus, portal input/ACK and scroll remain local
runtime behavior. The runtime retains its internal input-to-present measurements;
it sends no resident frame receipt.

Composer editing is local to native portals. Only accepted submissions enter the
portal input queue returned by `hud_input`; resident sessions receive no draft
snapshots, submit or cancel events. The retired resident event messages cannot
activate the internal native composer flag.

- **Every `RequestResult` carries `code` + `hint`.** Scene validation hints
  reach the agent rather than being flattened to `MUTATION_REJECTED`.
  `seq` echoes the request's `sequence`, including legacy zero and rejected
  empty/unknown-only envelopes; those rejections keep the stream open.
  Ephemeral zone publishes get no reply. `Hold` and `Clear`
  retransmits replay the cached reply.
- **Timing hints are honored**, not just validated: `present_at_us` holds
  content, and `expires_at_us` sweeps it (invariant 1). On `Publish`,
  `expires_at_us` wins over `ttl_ms`, which counts from presentation.
- **Zero means the surface default on `Publish`.** Omitted scalar fields
  arrive as zero. With both `ttl_ms` and `expires_at_us` zero, notifications
  with actions use the owner's live `[runtime] answer_window_ms` (default
  60000), counted from `max(now, present_at_us)` and stored at acceptance.
  Positive `ttl_ms` wins over this default, and `expires_at_us` wins over
  both. Changing the setting affects later accepted publications only; it
  never retimes active or already-scheduled cards. Notifications without
  actions keep 8 s (urgency 0/1), 15 s (2), or 30 s (3+) from presentation.
  Other zone content and widget publications have no derived content expiry.
  MCP's ordinary omitted zone TTL remains 60 s, and its explicit `ttl_ms: 0`
  holds even an action notification until cleared.
- **Hold0 means until cleared for both zones and widgets.** Publish first,
  then `Hold{surface, ttl_ms: 0}`.
  Hold removes content expiry from the agent's active and pending publications
  on that zone; it is namespace-wide, not limited to one merge key. Pending
  content still waits for `present_at_us`, and `Clear` cancels it. The two
  requests are not atomic: if nothing remains when Hold arrives, it returns
  `NOT_HELD`. A later Publish uses its own timing, so republishing a
  notification can restore an expiry. Held content remains subject to
  clearing, human override and lease/session-origin reclamation (invariant 4);
  Hold does not make a lease immortal. There is no atomic held mode on Publish.
- **No wire mutations.** The protocol defines no node, mutation or batch
  message; scene mutations exist only in-process. Native portal rendering uses
  checked direct scene methods.

## Configuration

The runtime installs its built-in zones independently of configuration. Root
`[zones]` custom definitions and `[[tabs]].zones` are unsupported, including
empty tables or arrays: startup rejects them with `ParseError` and a hint to
remove or correct the field. Remove those obsolete entries rather than
renaming their zone values. Tabs and their widgets remain configurable
(a `[[tabs.widgets]]` entry's `geometry` and `auto_clear_ms` are what the
control panel's Layout card writes);
`[displays."<key>"].zones` remains the supported way to assign built-in zones
to a display. `<key>` is the monitor's EDID key, uppercase, any case accepted:
`<MFG><PRODUCT as 4 hex>-<SERIAL>` (e.g. `DEL41A2-7XK2N13`; the serial keeps
`A-Z 0-9 . _ -`, other characters become `_`), or `<MFG><PRODUCT>` when the
serial is blank. All connected hardware participates in this ambiguity check,
including monitors without an overlay in fullscreen or explicitly sized mode.
When two connected monitors would share a key (same model and
serial, or same model with blank serials), every one of them gets
`@<connector>`: the full PnP device instance id with `\` written as `/`, other
characters outside `A-Z 0-9 . _ & -` as `%XX`, e.g.
`GSM5B7F@DISPLAY/GSM5B7F/5&1A2B3C&0&UID4352`. Windows documents that id as
unique on the system and persistent across restarts. Quote keys:
`[displays."DEL41A2-7XK2N13"]`. A key without the suffix still matches when
exactly one connected monitor has that model and serial, so owners with
distinct monitors never write one. A monitor without a usable EDID identity
(non-Windows, headless, or a failed query), or in a shared set without an
instance id, is keyed by its OS name with a warning. Copy usable keys
from `/admin/status` `displays[].key` or the log's per-display
`display <key> = <name> <w>x<h> @ (<x>,<y>)[ primary]` lines, written at
startup and on every layout change. A configured key that matches no
connected monitor keeps its zones on the primary, never another monitor; the
log warns once per change naming the key, its zones and every connected key,
and `/admin/status` `unplaced_zones` lists them as `not_connected`.

Legacy Windows enumeration section keys (`DISPLAYn`, ignoring case, outer
whitespace and an optional `\\.\` prefix) are ignored even when a matching OS
fallback is connected; their zones render on the primary with
`unplaced_zones.reason = "legacy_key"`.
The nonfatal topology warning supplies the exact current EDID-derived
replacement for a unique usable connected identity, otherwise reports
`not connected` or `identity unavailable`; rename the section only when a
replacement is supplied, and keep nonlegacy OS fallback keys unchanged.

Built-in zone placement, as fractions of the display (1% margins):
`notification-area` is the top-right column (x 75–99%, from y 5.5%, below
the production `main-status` badge in the top-right corner);
`pip` sits below it (x 75–97%, y 40–66%). The `status-bar` is a content-sized
horizontal pill at bottom-right, inset by `space.status.margin` (16 dp),
with `size.status.height` (36 dp), `space.status.horizontal` side padding
(14 dp), `type.label.m` text, `color.surface.container.low` at `opacity.surface`
and `shape.full` corners. Named label families use the configured `font.sans` role;
generic mono/serif families select those existing roles. Keys use `color.on_surface.variant`, values
`color.on_surface`; their gap is `space.status.key_value` (6 dp). Entries are
sorted by key with `color.outline.variant` dividers. Each entry is bounded by
`size.status.entry_max` (240 dp); overflowing key/value text is ellipsized and
a suffix that cannot fit the available display region is hidden. Measurement and
painted text also honor the existing `max_truncation_input_bytes` profile budget;
an omitted field suffix is marked by an ellipsis. The empty bar
draws nothing and has no drag handle. Explicit owner `key_icon_map` SVG icons
remain supported at `size.status.icon`; unmapped keys are text only.
Dragging clamps the drawn pill and translates its maximum layout region with it;
the region size remains available for later publications. When content grows,
the pill moves inward as needed to stay within its assigned display; shrinking
restores the short-content placement. Measurement capacity is bounded by the
display dimensions, rather than the intersection with the translated region.
Text, dividers, icons and drag bounds share that final rectangle. Geometry and
styling remain runtime-owned. `subtitle` and
`alert-banner` are bottom- and top-anchored zones. The subtitle draws a centered,
content-hugging pill within its zone: at most `size.subtitle.max_width` (760 dp
in Tonal Glass and Blueprint), with `space.subtitle.horizontal` / `.vertical`
padding (20 / 10 dp), `shape.l` corners, `color.surface` at `opacity.scrim`,
and the `color.outline.variant` hairline (`stroke.border.width`).
`type.caption.display` drives size, line height and weight; text uses
`color.on_surface`. Caption families use the configured font roles (`font.sans`
for named sans families; generic mono/serif keywords select those roles).
Long text wraps at word or glyph boundaries and clips or ellipsizes within
that same pill. Reveal frames keep the committed publication's measured bounds;
unchanged publications reuse layout, including the drag handle's bounds. Above
the profile's `max_truncation_input_bytes` threshold, pill measurement uses the
same head- or tail-anchored whole-source-line viewport window as truncation.
This avoids shaping invisible transcript history; it does not split a giant
single source line or impose a strict byte cap on individual lines.
`space.subtitle.bottom` is the default zone's 48 dp bottom inset. All logical
sizes follow the zone monitor scale × `ui.scale`.

Classic retains its legacy 28 / 39.2 / 600 outlined caption, with a transparent
pill (`opacity.subtitle = 0`), 8 / 10 dp padding and the existing zone-wide
measure (`size.subtitle.max_width = 0` removes the cap). Other themes use `opacity.subtitle = 1`; this multiplier can hide the
pill without changing the shared scrim role. `stroke.subtitle.text` and
`color.subtitle.outline` control the optional text outline. The retired
`typography.subtitle.*`, `color.outline.default` and `stroke.outline.width`
keys no longer style subtitles. Owner policy overrides still win.

`[runtime]` also holds the launch settings the control panel edits:
`mcp_port` (default 9090) and `grpc_port` (50051), 0 disabling either and
the two different; `fps` (1-240, default 60); `log_level` for the log file
(`error`, `warn`, `info` default, `debug`, `trace`); and `debug_zones`
(zone outlines, default false). `answer_window_ms` is the live saved default
for action notifications (integer milliseconds, 1–300000 inclusive, default
60000); invalid values fail configuration validation. It has no flag or
variable override. `go_to_apps` is a startup-only array of eligible Windows
process executable names for Go to session. The default list is
`WindowsTerminal.exe`, `Code.exe`, `Code - Insiders.exe`, `Cursor.exe`,
`OpenConsole.exe`, `conhost.exe`, `wezterm-gui.exe`, `alacritty.exe` and
`mintty.exe`. An explicit array replaces the defaults and `[]` disables
lookup. `question_placement` (live, saved from the Layout card
row "New questions appear on"; no flag or variable) chooses where a new
question card (a notification with a callback action) appears: `focused`
(default) is the monitor of the foreground window, or the pointer's monitor
when there is none, it is the HUD itself, or it is the shell desktop;
`pointer` is the monitor under the pointer; `fixed` is the notification
display (the primary unless `[displays]` moves `notification-area`). Any
other value fails configuration validation. The display is sampled once,
when the question is presented (at `delay_ms` when it comes due), with no
hook, timer or idle work; a monitor without an overlay means the
notification display. With `focused`, a display the foreground window chose
is covered when Windows reports a full-screen Direct3D app or presentation
mode and that window fills its monitor: the question goes to the pointer's
monitor if that differs, else the notification display if that differs,
else (Direct3D only) the first other overlaid display, primary first;
otherwise it stays. Each covered placement logs one info line
(`question placement: DISPLAY1 covered (presentation_mode); placed on
DISPLAY2`). Other cards, including url- and copy-only ones, use the
notification display. A placed card draws at the notification-area position
on its display's rect and scale, in that display's stack; it never moves,
a same-key replacement keeps its display, and if its display disconnects it
shows on the notification display until its assigned display reconnects. All
displays share one `Stack{max_depth:5}`
trim. A changed setting applies to the next question; no visible card moves.
For the launch settings, precedence, highest first: a command-line
flag (`--mcp-port`, `--grpc-port`, `--fps`, `--debug-zones`), an
environment variable (`TZE_HUD_MCP_PORT`, `TZE_HUD_GRPC_PORT`,
`TZE_HUD_FPS`, `TZE_HUD_DEBUG_ZONES=1`, `TZE_HUD_FILE_LOG`), `[runtime]`, the
default. Ports are read at start; the rest change live from the panel.

## Human chords and the control panel

Five global chords work while the overlay is unfocused and click-through
(Windows; each is set in `[runtime]` as modifiers Ctrl/Shift/Alt/Win plus
F1-F24, a letter or a digit, and all five must differ; one exception keeps
older configs loading: when `hide_hotkey` is unset and another key already
uses Ctrl+Shift+F8, that key keeps it and the hide chord is left unbound,
logged as a warning and reported in `hide_hotkey` as `registered: false`
with an `error` naming both keys; set `hide_hotkey` to bind one):

| Chord (default) | Key | Effect |
|---|---|---|
| Ctrl+Shift+F12 | `safe_mode_hotkey` | Pause agents and open the control panel; pressed again, resume and close |
| Ctrl+Shift+F11 | `panel_hotkey` | Open or close the control panel with agents running |
| Ctrl+Shift+F10 | `pairing_hotkey` | Open the control panel at Connect an agent, agents running |
| Ctrl+Shift+F9 | `answer_hotkey` | Focus the newest unanswered question with callback actions, or else the composer of the portal waiting for a reply |
| Ctrl+Shift+F8 | `hide_hotkey` | Hide all, or show all when everything is hidden, from any app; the panel and pause stay as they are |

Windows also has one notification-area **tray icon**, even when no agent
content is showing. Click it, or select it with Win+B and Enter: while a
question card is drawn, agents are running and zones are shown, this focuses
the newest question (as the answer chord does) and afterwards returns focus to
the app that was in front; otherwise it toggles the control panel. If the
question goes before the click lands, the panel opens. The app in front is one
look at the click: the first window, top to bottom, that is visible, not
minimised or cloaked, not topmost, not a tool window, unowned, not the HUD's
and not the taskbar or desktop; with none, Windows keeps focus. Its native
context menu offers **Answer next question** (default) while a click would
answer, then **Open panel** (otherwise the default), **Pause agents** /
**Resume agents**, **Hide all** / **Show all**, and **Quit**; each item shows
its chord, when one is bound, beside it. Every item supports keyboard
navigation. Pause opens the panel and suspends
agents; Resume closes it and resumes them. These commands run on the network
runtime like global chords, independently of the window event loop or agents.
Quit uses the normal clean shutdown path, without confirmation; relaunch
through Win+R `tze_hud` when installed, or its executable.
Windows overlays use the tool-window style to stay out of Alt+Tab and ignore
close requests such as Alt+F4. Use **Quit** from the tray.

The glyph shows paused, hidden, questions waiting, or normal, in that priority.
Its contrast follows the Windows taskbar's light/dark setting rather than the
HUD theme. The two-line tooltip reports question count (or Paused / Everything
hidden), followed by " · click to answer" while a left click would answer, and the names of agents connected or showing something, as in the
panel header. Long names are ellipsized within the shell's UTF-16 limit.
Windows 11 initially places new icons in overflow; pin this one through
**Settings > Personalization > Taskbar > Other system tray icons**.

`/admin/status` reports `tray` with `registered`, `glyph`, `tooltip`,
`click` (`answer` or `panel`: what a left click does now), `all_hidden`,
`hidden_notice` (the pill's text, or null when no pill shows)
and native test identity, plus `session_locked`. `tray` is null
and `session_locked` false off Windows. Failed watcher creation leaves
`registered` and `session_locked` false; a missing icon can therefore be
distinguished from overflow. Explorer restart re-adds the icon; an unsuccessful
registration is logged and retried at the next Explorer restart. WTS lock,
unlock and console/remote connection events update `session_locked` without
polling. This observation alone does **not** freeze question deadlines or hold
new cards; that separate desktop-presence delivery remains outstanding.

`/admin/status` reports the normalized display name of the open control
panel as `panel_display` (for example, `"DISPLAY3"`). It is `null` when
the panel is closed or no overlay display is known.

A global chord takes its keys from every other app while the HUD runs, so the
defaults are F-key chords; rebind one that clashes (Ctrl+Shift+F10 is also
JetBrains IDEs' "Run", and Ctrl+Shift+F8 its "View Breakpoints" on Windows). The pairing chord is not registered when the MCP port
is disabled. The control panel's Input card changes a chord while the HUD
runs: type it (`Ctrl+Alt+F9`) and press Enter. A chord another role has is
refused; an unbound hide chord's row is empty, and typing one binds it.
Otherwise Windows registers the new chord before the old one is
released, so a chord another program holds is refused and the old one keeps
working. The new chord is saved to `[runtime]`. Each chord row says
whether its chord is the default, saved to config, or lasts until restart
(no config file), and a changed chord has Reset.

The mouse wheel follows Windows' "lines per notch" setting, read on each
notch: a notch scrolls that many lines of the text under the pointer (the
portal transcript's line height, else the text node's, else 40 px), and the
"one screen at a time" setting scrolls one viewport. Touchpad pixel deltas
pass through unchanged. Off Windows a notch stays 40 px.

While an IME composes into a portal composer, the composition string draws
inline at the caret with a token-colored underline
(`portal.composer.preedit_underline_color`, default the caret color) and the
caret at the IME's cursor. It never enters the draft: only the committed
text does, and cancelling the composition leaves the draft as it was.

The pointer is a hand exactly where a primary press would act on a HUD
control: card action buttons and the card ×, each portal's × and unread
badge, OUTPUT links and Copy buttons, and the control panel's chips, tiles,
steppers, Reset and pressable rows (not its search or text fields). An
answered card's controls and card controls in safe mode show the arrow, as a
press there does nothing. Precedence, highest first: an active resize or
drag, a resize grip or edge, a pressable control, a drag handle (open hand;
a portal × in the header band shows the hand), OUTPUT or composer text
(I-beam), the arrow. The shape follows controls that appear or leave under a
resting pointer by the next frame, without pointer motion. The shape is
applied to the overlay window under the pointer, so it also shows on a
secondary display; the window the pointer left returns to the arrow.

Overlay clicks on card buttons, badges and scroll panes act without taking
Windows keyboard focus. Clicking a text editor (a portal composer or panel
search/edit field) or starting a portal OUTPUT selection takes focus
explicitly, so Ctrl+C reaches the HUD. The same rule applies to every
monitor. Publication never takes focus. When the HUD takes focus, it remembers
the external app that was active; nested HUD focus takes preserve that origin.
A tray-started visit remembers the app sampled at the click instead, since the
taskbar was active.
A keyboard card answer or the outer Esc returns to it only while a HUD window
still holds the foreground. Closing that app, or switching to a third app,
never causes another window to be raised. Sending a portal reply keeps focus
in its composer.

The answer chord targets the newest visible unanswered card with callback
actions, on whichever display it was placed; that display's overlay window
takes keyboard focus. With no such card it focuses the composer of the newest portal
waiting for a reply (shown, not degraded, its latest output published with
`expects_reply: true`; newest by that output's time), exactly as a click there
would: the draft and caret are kept and the origin app is remembered. With
neither it changes nothing. It transfers input from an open running settings
panel to the card or composer; it does nothing in safe mode. Tab/Shift+Tab
and arrows move among that card's actions, starting on the primary action (the
first that is not destructive, else the first); Enter/Space activates the
focused action, so chord then Enter takes the primary (a `url` primary opens
its link and ends the visit, without answering). Digits 1–4 activate the
corresponding action; a destructive action's Enter, Space or digit must be
held for `notification.action.hold.ms`, with token-styled digit
hints only during this chord visit and no change to button or label geometry.
A digit beyond the action count does nothing. Callback answers return focus;
a URL digit uses the same two-second per-action limit, leaves the card visible,
and finishes the keyboard visit. A replaced or expired card cannot redirect a
held key to a different card or the composer.

Esc unwinds one layer at a time: it leaves nested card/control focus, clears an
OUTPUT selection or link/Copy focus, or collapses a composer selection before
the outer Esc leaves the HUD. Esc never changes a composer draft: the text is
kept and returns when that composer is focused again. Each composer keeps its
own unsent draft, shown as text without a caret while unfocused, until it is
sent or its portal is cleared, reclaimed or dismissed. In settings it cancels an edit/section or clears search
before closing. An answer-chord visit entered from another app uses Esc to
leave without answering. Key repeats, text and matching releases from a card
activation stay local; native focus returns after the activation key releases.

The control panel opens on the foreground window's monitor at that display's
scale. If the HUD or desktop is in front, or there is no foreground window,
it uses the pointer's monitor. A tray open uses the monitor at the icon
activation or context-menu invocation. The primary is the fallback when the
chosen monitor has no overlay. The panel stays on its display until closed
and chooses again on reopening. The hidden-content pill and pairing system
card use the primary.

The full-width pane contains a search field, quick-action tiles, and section
cards that scroll vertically. Its header line counts the agents connected or showing
something and what they hold ("3 agents · 2 portals · 5 zones · 1 widget")
and says "restart pending" while a saved setting waits for a
restart (Restart now is in System, Alt+8). The panel takes keyboard focus on
that display (if Windows refuses, the header says to click it). On close it
returns focus to the prior app if the HUD still owns focus. Typing searches
every row; Space continues the query after automatic
result focus. Arrows and Tab navigate, Enter acts on the best match, and Space
acts on an explicitly selected control. Esc clears the search, then closes.
A row whose value differs from
its default (an override that matches the theme does not) shows Reset:
clicking it, or Delete on the row (not while typing in a field or the
search), puts the default back live and removes the key from the config
file. Visibility, Appearance, Advanced tokens, Input, Diagnostics and the
Agents answer-window row offer it; ports and widget boxes take a typed value. Clicks outside the
panel pass through to the desktop. Closing the panel always resumes
agents. While paused, the pause tile and row turn caution-colored.

Value fields edit locally. Clicking a field or pressing Enter on a field result
selects its current value; typing replaces the selection. Left/Right move by
Unicode grapheme, Home/End move to either end, and Shift extends the selection.
Ctrl+A selects all; Ctrl+C copies; Ctrl+X cuts after a successful clipboard
write; Ctrl+V pastes at the selection. Backspace/Delete edit the draft. Paste
converts line breaks and tabs to spaces; Connect agent names keep their existing
lowercase name alphabet and 32-byte limit. Other drafts are bounded at 65,535
UTF-8 bytes; overflow is shown and saving waits for an edit that shortens or
replaces it. Enter saves through the row's existing validation and confirmation;
Esc cancels before clearing search or closing the panel. Moving away drops the
draft. Automatic search-result focus keeps typing and clipboard shortcuts out
of value editing until explicit selection. Clipboard access runs in the
background with one operation at a time; a delayed result cannot modify a
changed or reopened draft. Failed clipboard access leaves a cut intact and
shows feedback beside the field. Caret and selection stay inside the value box;
the caret stays steady so an idle panel needs no blink timer.

The Visibility card hides portals, zones or widgets for
the session; the Hide all tile (Alt+H) hides all of them and, when everything
is hidden, shows it all again. Hidden content is not drawn and takes no input
or focus; agents keep their leases and publications and are not told. While
anything is hidden and the panel is closed, a pill at the bottom right of the
primary display says what is hidden and which chord opens the panel (the
pause chord when Windows refused the panel chord, or Tray → Open panel when
both chords are refused). A restart shows everything. The hide chord and the
tray's Hide all / Show all make the same edit as the tile, with the panel open
or closed; the pill is always kept.

The Visibility card's **Hide from screen capture** (`[runtime]
exclude_from_capture`, default off, live and saved; Reset removes the key)
keeps every overlay window, and so the panel and the pill, out of screen
captures and shares (Teams, OBS, Snipping Tool, `BitBlt` with `CAPTUREBLT`,
Graphics Capture) while the owner's monitors still show it. Windows applies
`WDA_EXCLUDEFROMCAPTURE` to the primary and every secondary overlay, including
one opened on hot-plug or recreated after surface loss, before its first
present. It needs Windows 10 version 2004 (build 19041); below that the row is
disabled ("Needs Windows 10 version 2004 or later") and nothing is applied,
because older builds draw a black box instead. While the session is a Remote
Desktop session every overlay stays visible to captures (so the remote viewer
sees the HUD; a share started inside that session shows it too), and the
exclusion returns on console reconnect. Not covered: the tray icon and its menu
(Explorer's windows) and a photo of the screen; this is a view setting, not a
security feature, and agents are not told. `/admin/screenshot` is unaffected.
Search capture, share, screenshot, recording, OBS or Teams to find it.

The Agents card's **Questions wait for an answer** choice offers 15 s,
30 s, 1 min, 2 min and 5 min. Its detail is "How long a question card waits
before it hands the question back · Default" or "· Saved to config";
search question, choice, answer, timeout or wait to find it. A valid custom
config duration is also displayed. Keyboard arrows or a click save through
the normal atomic config writer, then apply live to the next accepted
publication on both planes. Reset removes the override and restores 1 min.
A failed save, including no writable config, leaves the live value and its
source unchanged and reports the error. This never changes existing or
already-scheduled cards, and explicit agent expiry always wins. Agents learn
the effective lifetime through existing `expires_in_ms`, with no setting or
schema field added to discovery.

The Agents card lists every paired agent, and any other agent that is
connected or shows something, with a status dot and what it holds (portals,
zones, widgets); the list follows agents as they come and go while the
panel is open. Opening an agent's row shows:

- **Clear** (asks first): removes everything the agent shows now, as a viewer
  dismiss would, without waiting on it. Clear is not a block: the agent's next
  publish is served, and a portal publish reports `reattached: true`.
- **Unpair** (asks first): removes the agent from `agents.toml`, so its PSK
  stops authenticating at once (a handshake already under way is refused);
  ends its gRPC sessions, even a hung one; and then clears its content.
  Unpairing the last agent reopens pairing.
- **Zones, Widgets, Portal, Admin**: grant or withdraw a whole class
  (`zone:*`, `widget:*`, `portal`, `admin`) in the allow list as it
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
longer works. When Windows Firewall blocks tailnet agents, the Network row
links to Network › Remote agents: Enter on it leaves the card at that row.
Clicking the name edits it; only Enter copies. New code replaces
the code. Esc goes back to the sections. While the card is up, the system card
stays away, even for a code replaced after wrong guesses; once it is left, a
replacement shows on the system card again.

The Network card shows the MCP and gRPC ports and Remote agents. A port is
typed in place (the first key replaces it, Enter saves, Esc cancels). In
search results, click the field or press Enter on it to start editing; the
caret and row hint show that typing will replace its value. Automatic result
focus keeps typing in search. Saving or cancelling retains the search;
Esc again clears it. Port changes are saved to `[runtime]`; it applies at the next start, so the row shows
"Restart to apply" and the running port, and Restart now counts it.
Changing the MCP port means paired agents must use the new one, so a
changed value asks first (the first Enter shows the warning, the second
saves). Remote agents shows Windows Firewall's verdict for tailnet agents;
switching it on (the panel asks first, since it widens who can connect) or
off (at once, since it only narrows) runs the same elevated helper as
`--allow-remote` / `--disallow-remote`, for which Windows asks for
administrator approval either way, and reads the firewall again.

The Diagnostics card sets the log file's level, zone outlines and the frame
rate cap (choices from 30 to 240; `[runtime] fps` takes 1-240). Each applies at once and is saved to `[runtime]`; when the
save fails the change stays for the session and the row says so. A row
off its default (info, off, 60) has Reset, which returns it there and
removes its key. The cap
paces frames; load shedding keeps the startup rate.

The System card has Restart now (asks first; it lists what it applies, uses
the same health handoff as `POST /admin/restart`, and a failed restart keeps
this instance up and shows why), Install update (asks first; the signed
update path of `POST /admin/update`, on this build's channel, when this copy
is installed), Start with Windows (the per-user Run entry; installed copies
only), Open config file, Open log folder, the display profile and the window
(read-only: `[runtime].profile`, and `--window-mode`/`--width`/`--height` or
their `TZE_HUD_WINDOW_*` variables, each row naming its source; change them
in the config file or the launch arguments and restart), and the version.

The Layout card shows, per widget, its box as percentages of the display
("4%, 8% · 18×12%": x, y, width×height) and how long its content stays up.
Widgets draw no drag handle; this box is how the owner places them.
Move handles for draggable tiles, portal frames and primary zones are hidden
until the pointer enters the element or its handle. They draw a 32×4 dp rounded
pill in the theme's `color.on_surface.variant` role at `opacity.drag.hover`;
pressing or dragging uses `opacity.drag.active` and keeps the pill visible after
the pointer leaves. A 12 dp hit row contains the small pill, separated from the
card by `space.s` (8 dp); the continuous hover envelope includes that gap without
capturing it. The row clamps to the display edges. Portal header bands
keep their full input area and yield to header controls. Notification hover uses
the painted stack, including gaps, rather than empty logical zone space; bodies
remain click-through. Hover is local and repaints only on entry/exit, with no
polling deadline or agent event. Active drag uses the semantic primary outline,
`stroke.border.width` and `shape.l`. All sizes follow monitor scale × `ui.scale`.
The pip resize target and its existing corner dot mark remain separate.
Overlapping element bodies reveal the frontmost element's pill; this hover
selection does not change existing grip capture precedence.
Typing four numbers and Enter moves or resizes it at once (a box that does
not fit on the display is refused); "Clears after" (Never, 5 s ... 1 h)
caps each publication at that long after it was published, including what
shows now, unless the agent asked for less. Both are saved into the
widget's own `[[tabs.widgets]]` entry with the file's comments kept; when the
save fails the change lasts the session and the row says so. The
Layout card also gives each zone a display choice. Choose a connected monitor
by its current key to move the zone on the next frame and save it under
`[displays."<key>"]`, keeping the file's comments and other zone assignments.
"Primary (default)" removes the explicit binding; a named primary monitor
keeps its physical key. Missing or retired bindings remain labelled unavailable,
and a monitor unplugged before an edit is applied is refused rather than
redirecting the edit to another screen. If saving fails, the live change lasts
until restart and the row explains why; without a config file it is session-only.
You can also drag an existing zone grip to move the whole zone for this runtime
session. The shown `pip` zone also has a resize grip in its corner facing the
display centre (diagonal resize cursor): dragging it scales the zone with its
width/height ratio pinned and the opposite corner fixed, kept on screen and no
smaller than the `portal.window.min_*` tokens (a pip already below that size
may keep it, but not shrink further). The grip keeps its corner for the whole
drag. A resize is a session placement
like a move, with the same Reset and restart behavior. During notification drag, the border encloses the slots actually
shown (including the gaps), while the logical zone box and its movement limits
stay unchanged. Right-click the grip for the labelled Reset to default action;
its menu fits the originating display at that display's DPI times UI scale.
Press and release the same menu action to restore the ordinary box. Outside
click, timeout, focus/capture loss or a removed/hidden target dismisses without
activating content underneath. Restart also restores the ordinary box. Clear,
expiry and republishing keep the session placement. Zone moves/reset do not write saved geometry or emit agent reposition events.
Unassigned zones, explicit primary assignments and disconnected-display
fallbacks retain their primary-canvas grips. Connected-secondary zones keep
content and controls on that display, with no zone grip; dragging does not
change a zone's monitor. Notification titles/bodies remain passthrough, while
X/actions and their existing keyboard navigation keep their normal roles.
Native capture cancellation or focus loss on the captured window disarms its
Zone drag immediately, including a retained release under lock contention;
later Move/Up cannot finish it. Feedback settles through the existing scene
availability path. Crossing an overlay while still captured is not cancellation.

The Appearance card picks the theme and the sans, mono and serif fonts
(type a family name; an empty value or Reset gives the theme's), and
Subtitle backdrop: on sets `opacity.subtitle.backdrop` to 0.9, drawing a
`color.subtitle.backdrop` fill behind the whole subtitle zone so streamed
subtitles stand out; off removes the override (0 draws none). This optional
whole-zone fill is separate from the themed content-hugging pill. Agents send
nothing for it. Left/Right
on the theme, or Alt+T from anywhere in the panel, previews the next theme
live without saving it; Enter keeps it, and Esc, moving off the row or
closing the panel puts the saved theme back. A click on the theme keeps
its choice at once. Advanced tokens lists every
canonical token with its value and where it comes from (the theme, the
canonical default, or an override); typing a value of the token's kind
overrides it, an empty value or Reset removes the override, and "Reset all
overrides" removes every override but the theme. Overrides of other keys
written in the file (such as `portal.*` colors) are listed after the
canonical tokens and can only be reset. Each change restyles the
HUD on its next frame and is saved to `[design_tokens]` with the file's
comments kept; when the save fails (a read-only file) the change lasts the
session and the row shows the error and the path. The panel keeps its own
text legible whatever an override does. Zoom steps `ui.scale` by 10%
(−/+, Ctrl+− and Ctrl+= or Ctrl++ anywhere in the panel; Reset returns
to 100%), resizes zone text and chrome, portals, widgets and the panel, applies live and
saves; the panel itself stays between 0.75x and 3x so
it remains readable and on screen. The row's value is always the owner's
Zoom; when the Windows text size is not 100% its detail adds "Windows text
size 125% · effective 125%" (text size × Zoom), with "(held at 300%)" when
that product passes 3.0, and searching "text size" finds it. −/+ and Reset
change Zoom only.

A row whose value a flag or variable set is read-only and names it ("Set by
--mcp-port"), because a restart reuses the same arguments and environment.

Agents never see the panel: it is not in the scene, in MCP or gRPC results,
or in `/admin/screenshot`, open or as its closed notice pill. An idle
open panel draws no frames, except that Connect an agent redraws once a
second for its countdown. Its layout reads `control_panel.inset` and `control_panel.column.min_width` and the
semantic tokens of the active theme.

## Design tokens and themes

Agents never send styling. Every color, size, radius, font, and timing
the runtime draws with is a design token in the config file, resolved
once at startup and applied through `RenderingPolicy`, widget
`{{token.key}}` placeholders, and the compositor token map. The
canonical keys, their descriptions, and their defaults are in
`CANONICAL_TOKENS` (`crates/tze_hud_config/src/tokens.rs`).

**Sizes are logical (dp).** Every size token (font sizes, spacing,
radii, strokes, portal and panel chrome, in canonical and
`portal.*` keys alike) is drawn at the monitor's Windows scale factor × the Windows text size × `ui.scale` physical pixels (text size × `ui.scale` is held within 0.5–3.0): at 150% a 16 dp body is 24 px, and
`"ui.scale" = "1.25"` (or Windows text size 125%) makes it 30 px. `ui.scale` is a `[design_tokens]`
value (quoted, like every token) from 0.5 to 3.0, default 1; a value
outside that range fails config validation with `TokenValueParseError`,
and a theme that sets it is invalid, because it is the owner's preference
rather than part of a look. A zone placed on a secondary monitor is drawn
and hit-tested at that monitor's scale; portals, tiles and the panel use
the primary's. A DPI change or a new monitor restyles zones, portals and
the panel on the next frame. Hairlines (the transcript
divider) round to whole pixels, at least one. Opacity, weights, ratios
and timing do not scale. Zone boxes are fractions of the display and
keep their size. A widget box is drawn at the Windows text size × `ui.scale` times its configured
size, grown about its centre and kept on the display (an edge-anchored
widget keeps its edge); its SVG is rasterized at that size. The layout
(the Layout card and `geometry`) stays at 100%. A monitor's own scale
does not resize widgets, since their boxes already follow the display. The runtime
records the result as `runtime.px_scale` (and `runtime.px_scale@<zone>`
for a zone on a secondary), which config may not set.

Widget bundles resolve placeholders at load in their SVG layers and in
`widget.toml`: discrete `value_map` values (e.g. a status color), string color
defaults (`default = "{{token.color.primary}}"`; a literal `#RRGGBB[AA]` also
works), numeric binding outputs, hover delay and normalized trigger rectangles. The built-in gauge, progress-bar and
status-indicator draw tonal cards from the semantic roles
(`color.surface.container[.low]`, `color.outline.variant`, `color.primary`,
status roles) and the `type.*` scale. The gauge is a 128x132 card
(`color.surface.container`, `shape.l`) holding an 80x80 ring: a
severity-colored arc on a `color.outline.variant` track, clockwise from
12 o'clock, with `level` as a whole percent centred in `type.readout` at
`type.gauge.readout.size`, and `label` below in `type.label.m`; hover
shows `readout` in the label's place. All three bundles' SVG viewBox
units are logical px; instance geometry sets the scale, and `production.toml`
sizes them at 1.5x the viewBox on a 3840x2160 panel.

Progress uses a 320×50 card with a `type.label.m` label and always-visible
monospaced percentage above a 4dp track. The label is clipped to
`progress.label.width` so a maximum-length label preserves the percent and
right inset. Status uses a 40dp pill in its 320×124
tooltip canvas: owner-selected marker, status name and clipped inline reason.
`status.pill.radius` is half the pill's inner height (19.5dp by default), applied
to both SVG radii. An owner changing pill/canvas dimensions must also keep
`status.hover.height_pct` equal to pill height divided by canvas height so the
hover target follows the visible pill.
Online uses `color.success`, away/busy use `color.caution`, and offline uses
`color.on_surface.variant`. Hover retains the owner label/reason tooltip.
Both cards use `color.surface.container.low`; all geometry, paint, type and
hover dwell come from tokens. Model parameters and their limits are unchanged.

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
does this as a pure function. An unknown theme name fails config validation
with `UnknownTheme`, and the error lists the available themes.

**OS motion layer.** On Windows the runtime reads "Show animations"
(`SPI_GETCLIENTAREAANIMATION`) at startup and on each config reload; there is
no live OS watch. When it is off, a layer between the theme and the config
entries sets exactly five durations to `0`: `motion.enter.ms`,
`motion.exit.ms`, `motion.state.ms`, `portal.transition.in_ms` and
`portal.transition.out_ms`. An explicit `[design_tokens]` value for one of
them still wins. The runtime records the state as the reserved key
`runtime.reduce_motion` (`[design_tokens]` rejects it), and while it is set
widget publishes on both planes snap their transition to 0. The control
panel's Advanced tokens rows show "Windows animations off" as the source of
the zeroed motion values. No model-visible payload changes. Off Windows, and when the setting cannot be read, animations stay on.

**Windows preferences.** With "Show animations", the runtime reads the
Windows text size (Settings › Accessibility › Text size, HKCU
`Software\Microsoft\Accessibility` `TextScaleFactor`, 100–225%) and the caret
blink time (`GetCaretBlinkTime`) at startup and on each config reload,
including a reload whose `[design_tokens]` are unchanged; there is no live OS
watch. The text size scales the whole HUD as a factor under `ui.scale` (see
sizes above), so at 225% the HUD cannot go below 112.5%. The blink time is
the composer caret's half-period, clamped to 200–1200 ms; with Windows caret
blinking off the caret is steady and schedules no frame. They are recorded as
the reserved keys `runtime.text_scale` and `runtime.caret_blink_ms` (`0` =
steady), which `[design_tokens]` rejects; unlike the motion layer, nothing in
the config overrides either: Zoom is the only offset for size, and the caret
has no HUD setting. Off Windows, or when unreadable, the text size is 100% and
the blink 530 ms, and the token map is exactly as without them.
`/admin/status` `os_prefs` reports both. No model-visible payload changes.

**Live changes.** Editing `[design_tokens]` in the running HUD's config file
(or using the control panel's Appearance and Advanced tokens cards) restyles
it without a restart: zones, widgets (their SVGs re-resolve), portals and
the panel take the new tokens on the next frame. The HUD watches the
config's folder (no polling) and applies an edit 250 ms after the last
write (at most 2 s after the first). A file whose table is unchanged since
the HUD last read or saved it only re-reads the Windows preferences, so a
panel change that could not be saved survives an unrelated edit. A
table that startup would refuse (an unknown theme, a reserved or malformed
key) or a file that is not valid TOML keeps the current tokens and is
reported in `/admin/status` `config_reload`. Still read once at startup: the MCP widget
transition time (`motion.state.ms`), so changing it takes a restart. The
Windows animation preference is re-read on each reload: once it is off, MCP
widget publishes snap to 0 from that reload on; if the HUD started with it
off, MCP transitions stay 0 until a restart after it is turned back on. Widget text follows the live `font.*` roles like
zone text. Other config sections apply after a
restart (or live from the control panel). Keep config
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

Theme files accept canonical keys and the 42 existing portal color keys
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
Composer selection and `portal.transcript.selection_color` (OUTPUT selection,
defaulting to the composer selection color) keep the portal default, since no
theme role is a selection highlight. These theme mappings do not change portal sizes,
fonts, numeric defaults or behavior. Explicit `[design_tokens]`
color overrides still take precedence.

**Semantic families** (Tonal Glass values; sizes are dp):

| Family | Keys |
|---|---|
| Surfaces | `color.surface`, `color.surface.container`, `color.surface.container.{low,high,highest}`, `opacity.surface`, `opacity.scrim` |
| Roles | `color.on_surface[.variant]`, `color.outline[.variant]`, `color.primary`, `color.on_primary`, `color.[on_]primary.container`, `color.success`, `color.caution`, `color.error`, `color.[on_]caution.container`, `color.[on_]error.container` |
| State | `state.hover.opacity`, `state.pressed.opacity`, `focus.ring.width`, `focus.ring.offset` |
| Notification card | `color.notification.container[.low]`, `color.notification[.caution\|.error].outline`, `color.on_notification.{variant,caution,error}`, `opacity.notification`, `shape.notification`, `space.notification`, `type.notification.{title,body}.{size,line_height,weight}` |
| Alert banner | `color.alert.container`, `color.alert.{caution,error}.container`, `color.alert.outline`, `color.on_alert[.variant\|.error]`, `color.severity.{info,warning,critical}` (the severity mark), `opacity.alert`, `shape.alert`, `space.alert.{margin,horizontal,vertical}`, `type.alert.{title,body}.{size,line_height,weight}` |
| Gauge ring | `gauge.{width,height,edge.*,ring.*,caption.y,hover.delay_ms}`, `type.gauge.readout.size` (centred percent; family and weight follow `type.readout`) |
| Progress card | `progress.{width,height,edge.*,content.x,text.y,label.width,track.*,readout.*,hover.delay_ms}`, `type.label.m`, `type.readout.family` |
| Status pill | `status.{width,height,pill.*,edge.*,marker.*,name.x,text.*,separator.*,reason.*,tooltip.*,hover.*}`, `type.body.m`, `type.label.m` |
| Choice actions | `notification.action.link_mark`, `link.feedback.ms` (how long "Opened" or "Couldn't open" shows; default 1500), `notification.action.hold.ms` (destructive hold, integer ms 0–3000, default 600; 0 = no hold), `notification.press_guard.ms` (fresh-card press guard, integer ms 0–3000, default 400; 0 = off; not zeroed when Windows animations are off) |
| Motion | `motion.{enter,exit,state}.ms`, `motion.{enter,exit}.easing` (`linear`, `standard`, `decelerate`, `accelerate`) |
| Shape | `shape.{xs,s,m,l,xl,full}` (corner radius) |
| Spacing | `space.{xs,s,m,l,xl,xxl}` (4 px grid) |
| Type | `font.sans`, `font.mono`, and `type.<role>.{family,size,line_height,weight}` for the roles `code.display`, `caption.display`, `headline.s`, `title.m`, `body.m`, `label.m`, `label.s`, `readout` |

Font family values are free-form family names. A name that is not loaded
falls back to the renderer's default face for its role (see
[Fonts](#fonts) for how names resolve). The older keys
(`color.text.*`, `border.radius.*`, `typography.*`, and so on) still
work.
`tonal-glass` maps them onto the semantic palette until each element is
restyled to read the semantic keys directly.

**Notification cards** (`notification-area`) read their own card roles,
so a theme can style cards without restyling every surface that shares
the generic roles. A card is `color.notification.container` (urgency 1)
or `color.notification.container.low` (urgency 0) at
`opacity.notification`, with a `stroke.border.width`
`color.notification.outline` hairline, `shape.notification` corners and
`space.notification` padding; cards stack `space.s` apart. The title uses
`type.notification.title.*` in `color.on_surface`, the body
`type.notification.body.*` in `color.on_notification.variant`. Urgency 2
fills with `color.caution.container` under
`color.notification.caution.outline`, with `color.on_notification.caution`
text; urgency 3 fills with `color.error.container` under
`color.notification.error.outline`, with `color.on_notification.error`
text. Tonal Glass and Blueprint set these roles to their generic surface,
outline, text, corner, padding and type values; Classic sets its legacy
card look (black and navy fills at 0.80, a grey hairline, white text,
14 dp corners, 9 dp padding, a 15/700 title and a 13.5/400 body). The
dismiss × fills a square as tall as the title line, `space.s` from the
text. Actions are `space.xxl` tall with `space.m` label padding,
`shape.s` corners and `type.body.m` labels at `type.label.m` weight: the
first is filled `color.primary.container` with
`color.on_primary.container` text; the others are transparent with
`color.primary` text on a neutral card or the tone's text role on caution
and error cards. Hover and press add `state.hover.opacity` /
`state.pressed.opacity` layers of `color.on_surface`; keyboard focus draws
a `focus.ring.width` ring in `color.primary`. The zone's explicit margins,
radius, title size and weight and backdrop opacity keep precedence; an
explicit title size scales the whole card type ramp.
`color.notification.urgency.*`, `opacity.notification.backdrop`,
`typography.notification.dismiss.font_size_px` and
`typography.notification.dismiss.font_weight` are retired: themes do not
set them and a `[design_tokens]` entry for them is ignored.

**Alert banners** (`alert-banner`) are the same card drawn from their own
roles. A banner is inset `space.alert.margin` from the zone's sides and
top, and banners stack that far apart (severity first, then newest); the
zone grows to its last banner. Urgency 0-1 fill with
`color.alert.container`, urgency 2 with `color.alert.caution.container`
and urgency 3 with `color.alert.error.container`, at `opacity.alert`,
under a `stroke.border.width` `color.alert.outline` hairline, with
`shape.alert` corners and `space.alert.horizontal` /
`space.alert.vertical` padding. The title uses `type.alert.title.*` in
`color.on_alert`, the body under it `type.alert.body.*` in
`color.on_alert.variant`; a banner without a title shows its message in
the title role. Urgency 3 text is `color.on_alert.error`. Severity shows
as a mark in the icon column: a `space.xl` hairline ring with an `i`
(urgency 0-1) or `!` (2-3) in the title role, tinted
`color.severity.info`, `.warning` or `.critical`; a transparent tint draws
no mark and the text keeps the full width, and an agent icon takes the
column instead. The dismiss ×, actions and states are the card's. Tonal
Glass and Blueprint set an inset `surface.container.high` card (the error
container at urgency 3) with on-surface text and primary, caution and
error marks; Classic sets its legacy banner (a flush, square strip filled
#4A9EFF, #FFB800 or #FF0000 at 0.90, black text, no hairline or mark,
8 dp side padding, a 24/700 title over a 21.6/400 body). The alert
banner's older keys are retired and ignored like the card's:
`color.on_severity.*`, `color.severity.error`, `color.backdrop.default`,
`opacity.backdrop.opaque`, `typography.heading.{family,size,weight}`,
`typography.notification.{body.scale,title.weight}`,
`notification.action.{height,gap,padding}_px`,
`notification.action.background{,_hover,_pressed}` and
`typography.notification.action.{font_size_px,font_weight}`.

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
to the bundled DejaVu faces.

**System fallback for emoji, CJK and other scripts.** Text no loaded face
covers first draws as tofu and asks for Windows' own fallback faces: the
per-script families cosmic-text uses on Windows (for example Malgun Gothic
for Hangul) plus Segoe UI Emoji and Segoe UI Symbol. Han picks its face from
the user locale (`LANG`, else `GetUserDefaultLocaleName`): Yu Gothic for
Japanese, Malgun Gothic for Korean, Microsoft JhengHei UI for zh-TW,
MingLiU_HKSCS for zh-HK, Microsoft YaHei UI otherwise. The faces load on a background thread,
found through the same registry lookup as step 3, never on a frame; when
they land, text re-lays out once and the HUD repaints, with no republish.
A Hangul or kana miss also asks for the locale's Han face, since Malgun
Gothic and Yu Gothic cover Han too and would otherwise claim later Han text.
Each family is asked for once per run, so an absent one costs one lookup.
Loaded faces survive font and theme changes. Startup and text the loaded
faces cover load nothing extra. Off Windows the lookup finds nothing, so CI
keeps today's tofu. With no font tokens set, only bundled fonts load,
which is what CI and the tests use. Widget SVG text uses the same fonts and
roles as zone text (bundled faces plus the fonts-dir and system files the
roles loaded). A code point its family lacks falls back per glyph to the
first loaded face that has it, preferring a face of the same spacing:
monospaced text normally gets DejaVu Sans Mono, other text DejaVu Sans.

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

The same artifact separately reports `client_layer.skill_bodies` (the complete
`th-hud-publish` and `hud-projection` `SKILL.md` files, including frontmatter,
counted with that pinned tokenizer) and `client_layer.model_turns` (one per
executed `tools/call`; protocol `tools/list` is zero). These are report-only:
the later skill diet arms its own ceiling. They do not alter canonical MCP
flow bytes, fingerprints or budgets. Skill and reference JSON examples are
checked against production MCP argument types by
[`skill_json_examples_use_production_arguments`](../crates/tze_hud_mcp/tests/skill_examples.rs);
the [annotation convention](../.claude/skills/th-hud-publish/references/examples.md)
distinguishes arguments from responses and partial configuration examples.

| Measure | Before T5 (wire) | Wire | Model-visible | Budget (model-visible) |
|---|---|---|---|---|
| `tools/list` | 4,418 | 672 | 635 | ≤ 900 |
| Discover (default scene) | ~430 (`list_zones`) | 194 | 126 | ≤ 150 |
| Zone publish | ~197 | 90 | 39 | ≤ 80 |
| Four-option notification ask + returned answer + ACK | — | 273 | 114 | ≤ 120 |
| Four-option notification ask-and-wait (`notification_choice_wait`) | — | 129 | 77 | ≤ 90 |
| Four label-only options ask-and-wait (`notification_choice_labels`) | — | 117 | 65 | ≤ 85 |
| Widget publish | ~168 | 73 | 22 | ≤ 80 |
| Portal: attach+publish+hold, poll, ack, clear | ~575 over 5 round trips | 343 over 4 | 137 over 4 | ≤ 250 |
| Discover (unheld `production.toml`: 6 zones, 4 widget instances) | n/a | 321 | 245 | ≤ 250 |
| Error | up to ~212 | 101 | 45 | ≤ 60 |

All rows use canonical flow version 4 and `tiktoken-rs` 0.12.0
(`o200k_base`); the canonical gate requires two byte-identical final-source
measurements. The implementation PR records owner decisions on changes to
flow bytes or ceilings. The progress and copy schema adds 20 `tools/list`
bytes and 8 tokens to the PR1676 baseline (2,729 to 2,749 model-visible bytes
and 2,831 to 2,851 wire bytes). Those measured bytes await owner review.

- `notification_choice` publishes Ship/Hold/Skip/Open, activates Skip through
  `SceneGraph::activate_notification` (the shared pointer/keyboard producer),
  polls the retained `hud_input` answer and acknowledges it; all three calls
  count toward its 120-token ceiling.
- `notification_choice_wait` asks the same question with `wait_ms:30000` and
  `ttl_ms:0` and returns the accepted Skip in its only call, without polling
  or ACK. `notification_choice_labels` uses label-only actions with
  `ttl_ms:30000`, so the card leaves with the wait, and returns
  `"action":"Skip"`; the calibration freezes the scene clock so the reply's
  `expires_in_ms` is byte-stable.
- The calibration also runs an over-limit notification publish
  (`notification_action_limit_error`) and holds its `result_text` to the
  60-token error ceiling.
- Default reply fields (`reattached`, `pending_input`, `remaining`) stay
  byte-identical, and outcome receipts (`clipped`, `unread`, displaced items)
  are omitted on every canonical flow. Only a reply that reports them grows:
  `"unread":N`, or about one token per clipped field.
- The fixture signals shared activation through a local readiness pipe outside
  MCP; answers and ACKs still pass through the real MCP handler. Its ordering
  does not promise success under external scene contention.

The portal flow takes 4 round trips in the canonical fixture because the
first poll has nothing to ack; in a steady loop each `hud_input` both acks
the previous items and polls, so poll+ack is one round trip.

`integration` `poc_acceptance` checks model-visible counts on the POC
acceptance flows (`docs/scope.md`), run end to end against `production.toml`.
Its fresh, unheld discovery budget is the production row above: <=250 tokens,
retaining model-owned parameter names, bounds and actionable occupancy hints.
The live-state POC fixture envelope is 285 tokens: the unheld baseline plus a
40-token allowance for holdings and portal state. It is not a promise for
arbitrary numbers of live portals; required state fields are never dropped to
fit the unheld baseline.

The compact model schema retains `gauge.label` (metric name),
`progress-bar.label` (32-byte task text), status `label`/`reason`, values and
severity/status enums. Styling and hover state no longer spend discovery tokens:

- `status-indicator.theme` (`minimal|system|friendly`) is an owner choice in
  `initial_params`, independent of `[design_tokens].theme`.
- Gauge/progress-bar `fill_color` is an owner info-color override. Severity
  warning/error always use their theme tokens.
- `tooltip_visible` stays in the bundle schema for runtime hover (180 ms by default),
  with its declared SVG bindings. Agents neither discover nor publish it.


The gauge card takes its logical geometry from `gauge.*` tokens and its centered
percentage size from `type.gauge.readout.size`. Keep `gauge.ring.circumference`
consistent with `2π × gauge.ring.radius` when overriding the ring. SVG units map
once into the configured widget box. The Layout card reads that same instance
geometry: the production 128×132 logical ring replaces the bar box without a
second DPI multiplier. `state.hidden.opacity` and `state.visible.opacity` control
bound layer visibility.

Numeric bundle binding outputs (`attr_min`/`attr_max`), hover `delay_ms` and
hover trigger rectangle components (`x_pct`, `y_pct`, `width_pct`, `height_pct`) accept
numeric token placeholders as well as numbers. They resolve after TOML parsing;
invalid/non-finite numeric tokens reject the bundle. Hover delay must be an
integer in the u32 range. The gauge defaults to 180 ms; agents continue to use
only the existing model parameters.
Hover rectangles retain the normalized bounds check after token resolution.


## Design decisions (T5, 2026-10-02)

1. **Public agent-owned tiles are deferred.** The session exposes zones,
   widgets, uploads, input and events. The deferred tile spec and entry
   criterion remain in hud-x1wuq.
2. **Geometry belongs to the runtime.** No public tile placement or mutation
   route is exposed.
3. **Lease priority is dropped.** Chrome is structurally above agent
   content, and ties go to claim order.
4. **Portal tile mutations are internal.** Accent, composer interaction,
   and portal surface state stay inside the runtime; agents drive the portal
   through `hud_publish`. The one exception is the native unread count, which
   agents read as `unread` on the portal's discovery entry and publish replies
   (owner decision hud-ljhku.5 R5.4 = B, 2026-11-22). Because the count clears
   only when the viewer frame reaches the OUTPUT tail, an agent can infer that
   output was not presented, including while the panel is open or the portal
   is hidden.


Notification capture delivery (source proposal; Windows owner acceptance separate):
The UI-thread observer retains per-installation/per-press cancellation history
until the corresponding application edge is delivered. Every delivered left
edge consumes its receipt before control-panel routing, including panel-owned
presses and releases that never reach the scene. This prevents a panel click
from leaving history that disables the next notification action. Normal native Up and
valid synthesized release activate once; cancelled or retired-window edges and
late Up do not impersonate an action. Callback code borrows no scene/runtime
owner and forwards native messages; local feedback cleanup uses the existing
settle and lock-availability continuation, rendering only after actual mutation.
No new idle poll or timer is introduced. Ambiguous completed/active receipts,
mismatched history or bounded-admission failure disable notification actions
until a clean runtime restart. Valid reentrant histories may trigger that
availability limit; its frequency has not been measured. Unrelated pointer input
continues, and this source model does not replace held-press/release owner proof.
