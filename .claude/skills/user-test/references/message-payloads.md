# Message & Widget Payloads

Payload reference for zone messages (`messages` input) and widget messages
(`widget_messages` input). Used by Workflow Step 3 (publish zone messages) and
Step 4 (publish widget messages) in [../SKILL.md](../SKILL.md).

## Zone message shape

Message shape — `content` is either a plain string (StreamText) or a typed JSON object:

```json
[
  {
    "zone": "alert-banner",
    "content": "Deploy v2.1.0 started",
    "ttl_ms": 30000
  },
  {
    "zone": "subtitle",
    "content": "Running integration tests...",
    "ttl_ms": 60000
  },
  {
    "zone": "status-bar",
    "content": {"type": "status_bar", "entries": {"build": "passing", "agent": "butler", "target": "windows"}},
    "key": "build-status",
    "ttl_ms": 120000
  },
  {
    "zone": "notification-area",
    "content": {"type": "notification", "text": "Build complete", "icon": "", "urgency": 1},
    "ttl_ms": 10000
  },
  {
    "zone": "ambient-background",
    "content": {"type": "solid_color", "r": 0.1, "g": 0.15, "b": 0.4, "a": 0.05},
    "ttl_ms": 300000
  },
  {
    "zone": "pip",
    "content": {"type": "solid_color", "r": 0.2, "g": 0.8, "b": 0.2, "a": 0.05},
    "ttl_ms": 60000
  }
]
```

**Content types by zone:**
- `alert-banner`, `subtitle`: plain string (StreamText)
- `status-bar`: `{"type":"status_bar","entries":{"key":"value",...}}` with `key`
- `notification-area`: `{"type":"notification","text":"...","icon":"","urgency":0-3,"title":"...","actions":[...]}` (`title` and `actions` optional)
- `ambient-background`, `pip`: `{"type":"solid_color","r":0-1,"g":0-1,"b":0-1,"a":0-1}`

`key` and `ttl_ms` are optional per message. The publisher namespace is the
agent the PSK belongs to; a message cannot set it. `type` may be omitted: the
runtime infers it from what the zone accepts.

- `widget_messages`: array of widget publishes (optional)

Widget message shape:

```json
[
  {
    "widget": "gauge",
    "params": {"level": 0.75, "label": "CPU Usage"},
    "ttl_ms": 60000
  },
  {
    "action": "clear",
    "widget": "gauge"
  }
]
```

**Widget parameter types:**
- `f32`: JSON number (e.g. `0.75`) — often with min/max range
- `string`: JSON string (e.g. `"CPU Usage"`)
- `color`: JSON object `{"r": 0-1, "g": 0-1, "b": 0-1, "a": 0-1}`
- `enum`: JSON string from allowed values (e.g. `"warning"`)

`ttl_ms` is optional per message (widgets are durable by default).

**`widget` semantics: instance name, not type name**

`widget` (the surface `widget:<name>`) identifies a *widget instance*, not a widget type.
When the HUD starts, instances are created from `[[tabs.widgets]]` entries in the config,
each with an `instance_id`. That `instance_id` is the name you pass as `widget`.

For the production `tze_hud_app` deployment (see `app/tze_hud_app/config/production.toml`):

| `widget` | Widget type | What it shows |
|---|---|---|
| `main-gauge` | `gauge` | Vertical fill gauge (level, label, severity) |
| `main-progress` | `progress-bar` | Horizontal progress bar (progress, label) |
| `main-status` | `status-indicator` | Status circle with label (online/away/busy/offline) |

Use `hud_surfaces` to discover available instances:
```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --url "$MCP_HTTP_URL" --psk-env MCP_TEST_PSK \
  --messages-file /dev/null --list-surfaces
```
`hud_surfaces` returns `widget:<name>` entries with their params — use those names as `widget`.
If `hud_surfaces` returns no widget entries, the HUD binary is running without a config that declares instances.
