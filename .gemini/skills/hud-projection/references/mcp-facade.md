# Portal over MCP: wiring and boundary

Agents drive a portal with the standard MCP verbs on the surface
`portal:<id>` (`docs/api.md`). Every call is `tools/call`:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"hud_publish","arguments":{"surface":"portal:my-session","content":"hello"}}}
```

There is no other dialect: bare tool-name methods return `-32601`.

## Wiring

- `crates/tze_hud_mcp/src/tools.rs` applies portal verbs to the shared
  `PortalHub` (`crates/tze_hud_portal/src/hub.rs`), keyed by (agent id,
  portal id). The first `hud_publish` attaches; `key` replaces the newest
  unit with the same key.
- MCP and `PortalDriver` (`crates/tze_hud_runtime/src/portal.rs`) share
  `PortalHandle` (`crates/tze_hud_mcp/src/server.rs`): the hub, injected clock
  and per-agent input readiness. The driver renders native scene nodes
  directly; no operation channel or portal protobuf adapter remains.
- The bearer PSK identifies the agent. Portal surfaces need `portal` (or `*`)
  in that agent's `[agents.<id>] allow` list; otherwise `NOT_ALLOWED`.

## Boundary rules

- Identity is the PSK: there is no token. Two agents using the same portal
  id get separate portals.
- Responses stay bounded: no transcript history, no scene state. The hub
  keeps input queued until acked and redelivers it on every `hud_input`.
- The hub's transcript is in-memory presentation state; durable history
  belongs to the session.
- Accent, unread counts, and composer state are runtime
  internals with no MCP surface (decision 4 in `docs/api.md`).
