---
name: th-hud-publish
description: Use when publishing content to a user's HUD display, showing notifications or status on screen, sending text/color/alerts to tze_hud display zones, or giving an LLM visual presence on a user's GUI via MCP zone publishing.
---

# HUD Publish

Publish content to a running tze_hud instance over MCP. Agents show text,
notifications, status entries, and colors on the user's screen through named
zones; the runtime owns layout, rendering, and contention.

The MCP surface has five tools (`docs/api.md`): `hud_surfaces`,
`hud_publish`, `hud_hold`, `hud_clear`, `hud_input`. One-shot zone publishing
uses the first two (and `hud_clear` to take content down early).

## Quick Start

If the `hud_*` tools are connected:

```
1. hud_surfaces {}                       → zones you may use and what each accepts
2. hud_publish {"surface": "zone:subtitle", "content": "Build passed"}
```

If not connected, see **Setup**.

## Setup

### 1. MCP server configuration

Merge the `mcpServers` entry from `settings.template.json` into
`.claude/settings.json`:

```json
{
  "mcpServers": {
    "tze-hud": {
      "type": "http",
      "url": "http://<HUD_HOST>:9090/mcp",
      "headers": { "Authorization": "Bearer ${HUD_MCP_PSK}" }
    }
  }
}
```

`HUD_MCP_PSK` is your agent's PSK. Identity comes from it: the agent id is
your namespace, and the agent's `[agents.<id>] allow` list decides which
surfaces you see and may publish to.

**Autonomous / noninteractive target:** when no human display is needed,
resolve the always-on `hud-windows` VM:

```bash
eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
# -> HUD_MCP_URL + HUD_MCP_PSK exported; VM/HUD self-healed if down
```

### 2. Verify connectivity

`tools/list` should show the five `hud_*` tools. Call `hud_surfaces` to
confirm the connection.

## Tools

### `hud_surfaces` — discover

No parameters. Returns one compact entry per surface you may use:

```json
{"surfaces":[
  {"s":"zone:alert-banner","accepts":"notification|text"},
  {"s":"zone:notification-area","accepts":"notification"},
  {"s":"zone:status-bar","accepts":"status_bar"},
  {"s":"zone:subtitle","accepts":"text","held":true,"expires_in_ms":4200},
  {"s":"widget:gauge","params":{"level":"f32 0..1","label":"string"}}
]}
```

`held` and `expires_in_ms` appear on surfaces where your content is showing.

### `hud_publish` — show content

| Field | Type | Notes |
|---|---|---|
| `surface` | string | From `hud_surfaces`, e.g. `zone:subtitle` |
| `content` | string or object | Text, or an object for structured zones (below) |
| `ttl_ms` | integer | Lifetime. Zone default 60000; `0` = until cleared |
| `key` | string | Merge key: a publish with the same key replaces the earlier one |
| `delay_ms` | integer | Show later (≤ 300000) |

Returns `{"ok":true,"expires_in_ms":60000}`.

Content by `accepts` value (`type` is optional; it is inferred from the zone):

| `accepts` | Content |
|---|---|
| `text` | `"Build passed"` |
| `notification` | `{"title": "Deploy", "body": "v2.1.0 is live", "urgency": 1, "actions": [{"label": "Open", "callback_id": "open"}]}` |
| `status_bar` | `{"entries": {"build": "passing"}}` with a `key` |
| `solid_color` | `{"r": 0.05, "g": 0.1, "b": 0.2, "a": 1.0}` |

- Notification `urgency`: 0 low, 1 normal, 2 urgent, 3 critical. `body` and
  `text` are the same field. Action presses come back through `hud_input`.
- Status bar: entries with the same `key` replace each other; an empty value
  removes that entry.

### `hud_hold` / `hud_clear`

- `hud_hold {"surface": "zone:status-bar", "ttl_ms": 120000}` keeps your
  content without resending it.
- `hud_clear {"surface": "zone:subtitle"}` takes your content down.

### Errors

A failed call is a tool result with `isError: true` and
`{"code": "...", "hint": "..."}`. The hint names the next step.

| Code | Meaning |
|---|---|
| `ZONE_NOT_FOUND` | No such zone; the hint lists known zones |
| `NOT_ALLOWED` | Your agent's `allow` list doesn't include the surface |
| `CONTENT_REJECTED` | The zone doesn't accept this content type, or is full |
| `INVALID_ARGUMENT` | Bad or unknown field |
| `NOT_HELD` | `hud_hold` on a surface where you have nothing |

The full closed set is in `docs/api.md`.

## Default zones

| Zone | Accepts | Contention | Use for |
|---|---|---|---|
| `alert-banner` | notification, text | Stack | Important alerts |
| `subtitle` | text | LatestWins | Captions, transient text |
| `status-bar` | status_bar | MergeByKey | Persistent key/value status |
| `notification-area` | notification | Stack | Toasts |
| `ambient-background` | solid_color, static_image | Replace | Background color |
| `pip` | solid_color, static_image | Replace | Picture-in-picture |

Zone sets are instance-specific; discover with `hud_surfaces`.

## Common mistakes

- **Hardcoding zones** — call `hud_surfaces` first.
- **Microseconds** — every time is milliseconds (`ttl_ms`, `delay_ms`).
- **Text to a structured zone** — status bar and color zones need objects.
- **No `key` on status-bar** — without one, entries can't be updated in place.

## Script

For batch publishing or diagnostics outside an MCP client:

```bash
S=.claude/skills/th-hud-publish/scripts/publish.py
python3 $S --url http://<HUD_HOST>:9090/mcp --psk-env HUD_MCP_PSK --list-surfaces
python3 $S --url ... --zone alert-banner --content "Build passed"
python3 $S --url ... --zone status-bar --content '{"entries":{"build":"passing"}}' --key build-status
python3 $S --url ... --zone subtitle --clear
python3 $S --url ... --messages-file /tmp/messages.json
```

Message file:

```json
[
  {"zone": "alert-banner", "content": "Deploy started", "ttl_ms": 30000},
  {"zone": "status-bar", "content": {"entries": {"build": "passing"}}, "key": "build-status"},
  {"zone": "notification-area", "content": {"title": "Deploy", "body": "complete"}}
]
```
