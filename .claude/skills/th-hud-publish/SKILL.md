---
name: th-hud-publish
description: Publish notifications, short text, status entries, colors, or typed widget parameters to a paired tze_hud display over MCP. Use for "show this on my HUD", "notify me when tests finish", or "ask me on screen". For session transcripts and portal replies, use hud-projection.
metadata:
  owner: tze
  authors:
    - Claude
    - OpenAI Codex
  status: active
  last_reviewed: "2026-11-05"
---

# HUD Publish

Publish glanceable content through the five MCP verbs. The runtime owns layout,
styling and contention. Follow [the API](../../../docs/api.md) for contracts and
[invariants](../../../docs/invariants.md) for lifecycle evidence. Discover the
running build before using features added after its release; accepted requests
prove admission, not owner visual acceptance.

Use for requests such as "show this on my HUD", "notify me when tests finish",
"ask me on screen", and "update my allocated progress widget". Use
[hud-projection](../hud-projection/SKILL.md) for session attachment, transcripts
and portal replies; [user-test](../user-test/SKILL.md) for screenshots, logs,
updates or end-to-end Windows validation.

## Choose a surface

1. Call `hud_surfaces {}` for allowed zones, accepted content and typed widget
   params. Names and available widgets are instance-specific. `held` marks your
   holdings, including scheduled content; expiry may include presentation delay.
   `one` marks a single occupant, `max` a stack depth, and `busy` a foreign
   occupant on a single zone. `string<=N` is an explicit UTF-8 byte limit.
2. Use a brief notification for a milestone or actionable question. Keep one
   work item under a unique session/work `key`; replace it on meaningful changes.
   A card carries one question and at most a couple of short lines. Put depth in
   chat or a PR and name that destination. A notification action may use an HTTPS
   `url` instead of `callback_id` to link to a PR or artifact; never supply both.
3. Follow [repository instructions](../../../AGENTS.md) and owner presentation
   constraints. Current owner dogfooding rejects the status bar's large slab
   and default portal presentation; prefer
   finite milestone notifications until those surfaces are accepted. Use a widget
   only when allocated to this work and its discovered params fit.
4. Set a finite, appropriate `ttl_ms`; let owned content expire or clear only
   your keyed notification. Avoid heartbeat updates and repeated empty polling.

If the tools are absent, read [connection setup](references/connection.md) for
pairing, the MCP adapter, and the CLI's explicit host requirement. If the HUD is
unreachable or rejects the call, continue in chat and report the limitation once.

## Publish and maintain

`hud_publish` takes `surface` plus zone `content` or widget `params`:

```json
{"surface":"zone:notification-area","content":{"title":"Tests","body":"All checks passed","urgency":0},"key":"session-a:tests","ttl_ms":10000}
```

| Surface accepts | Payload |
|---|---|
| `text` | `"Build passed"` as `content` |
| `notification` | `{"title":"Tests","body":"Passed","urgency":0}` as `content` |
| `status_bar` | `{"entries":{"session-a":"testing"}}` as `content`, with a work `key` |
| `solid_color` | `{"r":0.05,"g":0.1,"b":0.2,"a":1.0}` as `content` |
| Widget `params` declaration | `{"progress":0.5,"label":"Tests"}` as `params`, using discovered names/types |

- Structured zone `type` is inferred from the zone; set it explicitly when
  selecting another accepted type in a mixed zone. Unknown fields are rejected.
  Notifications need a nonempty title or body; `text` is an alias for `body`.
  Urgency is an integer 0–3 (default 1). TTL belongs at the publish top level.
- A nonempty notification `key` replaces that publisher's card in the same zone
  and slot, including fresh content/expiry. Missing or empty keys append. Keys
  are exact, case-sensitive UTF-8, at most 256 bytes. This replacement contract
  is specific to notifications; other surfaces retain their own merge policies.
- Zone TTL defaults to 60000 ms. Action cards instead default to the owner's
  live answer window (currently 60000 ms by default). Explicit `ttl_ms`, including
  `0` for held content, wins; later settings changes do not retime admitted cards.
  Zone-only `delay_ms` is 0–300000; TTL counts from presentation.
- Widgets are durable unless given a TTL; the owner's "Clears after" limit can
  cap even held content. F32 params must be finite and in their declared bounds;
  colors are `#RRGGBB` or numeric `{r,g,b,a?}` channels in 0–1. No geometry or
  transition parameter belongs in an MCP publish. Use only discovered model
  parameters: gauge/progress `severity` selects info/warning/error colors.
  `fill_color` and status `theme` are owner `initial_params`; `tooltip_visible`
  is runtime hover state. Agent attempts reject the whole map.
- Status entries require string values. An empty string removes that entry;
  remove only this session's entry on a shared status surface.

`hud_hold {"surface":"zone:notification-area","ttl_ms":15000}` resets the
holding's expiry without resending. It has no key selector: on a notification
zone it can affect sibling cards held by the same agent. Prefer republishing
one key when extending only that card. Owner widget limits still apply.

`hud_clear {"surface":"zone:notification-area","key":"session-a:tests"}`
clears only that publisher's matching active and delayed notifications; a missing
match succeeds. Omitted/empty keys clear **all this agent's content** in that
zone, including pending publishes. A nonempty clear key is only supported for
notification-capable zones. Never use a broad clear on a shared pairing's zone.

## Questions and input ownership

Offer buttons only with safe answer routing and an active consumer. Notifications
accept at most **four** `{label, callback_id}` actions; both strings must be
nonempty. Use explicit labels such as Yes / No / Later. Callback ids must identify
the session, question and fresh generation; a reusable visual key is not a nonce.

```json
{"surface":"zone:notification-area","content":{"title":"Release tests","body":"Run the soak now?","actions":[{"label":"Run","callback_id":"session-a:soak:q1:run"},{"label":"Later","callback_id":"session-a:soak:q1:later"}]},"key":"session-a:soak"}
```

`hud_input {"wait_ms":30000,"max_items":20}` returns `items` and `remaining`.
Items have `id`, `s`, and `action` (or portal `text`); keyed notification events
also carry `key`. A human × has `action:"dismiss", kind:"dismiss"`; a callback
named `dismiss` has no `kind`. Timeout, silence, expiry and dismiss are not consent.

On a running build that discovers `hud_publish.wait_ms`, prefer a one-call
question: publish callback actions with `wait_ms:30000`. A returned `action`
is already consumed and needs no ACK; human × includes `kind:"dismiss"`.
`gone:expired|replaced|displaced|cleared` ends an unanswered card and is not
consent. An ordinary reply without action/gone means the wait ended: continue
with the same safely routed `hud_input` consumer. Cancellation leaves accepted
input queued. Do not use this with a shared pairing unless the question's
callback identity and fallback input consumer are coordinated. Earlier builds
retain the three-call path below; discover before using the new argument.

Route by surface **and** the current callback identity, handle once, then send
`hud_input {"ack":["<handled-id>"]}`. Unacked input repeats with stable ids;
ACKs are idempotent. Input covers every surface of the authenticated agent.
Sessions sharing a pairing need one coordinated consumer/demultiplexer or
separate identities. Do not auto-ACK another session's records. `wait_ms` defaults
to 0 and caps at 30000; a CLI/network timeout does not establish a user's answer.

Clicking an action disables its card and starts its exit locally. Replacement,
clear and TTL never retract accepted input: validate the callback generation
even if its old card has disappeared. Safe mode returns `SAFE_MODE_ACTIVE` for
mutations; polling remains available. Do not replay uncertain publishes blindly.

## CLI fallback

Use [scripts/publish.py](scripts/publish.py) when native tools are unavailable;
it reads the paired key internally and supports zones, widgets, hold, selective
clear and explicit input ACKs. Requires Python 3.10+ and a reachable paired HUD.
Run from the repo root with `HUD_HOST` set (or pass `--host`). `uv run` also
works. See `--help` for flags and batch format.

```bash
export HUD_HOST=hud-host.example
S=.claude/skills/th-hud-publish/scripts/publish.py
python3 "$S" --list-surfaces
python3 "$S" --zone notification-area --content '{"title":"Tests","body":"Passed","urgency":0}' --key session-a:tests --ttl-ms 10000
python3 "$S" --zone notification-area --clear --key session-a:tests
```

Tool failures carry `isError:true` and `{code,hint}`. Follow the hint; rediscover
for missing zones/widgets, fix fields for `INVALID_ARGUMENT` or
`WIDGET_PARAMETER_INVALID`, and respect `NOT_ALLOWED`. The CLI exits nonzero
on discovery or mutation failure. See the API for the complete error set.
