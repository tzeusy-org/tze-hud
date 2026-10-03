# Portal over MCP: wiring and boundary

Agents drive a portal with the standard MCP verbs on the surface
`portal:<id>` (`docs/api.md`). Every call is `tools/call`:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"hud_publish","arguments":{"surface":"portal:my-session","content":"hello"}}}
```

There is no other dialect: bare tool-name methods return `-32601`.

## Wiring

- `crates/tze_hud_mcp/src/tools.rs` turns each verb into one `PortalOp`
  (`Publish`, `Hold`, `Input`, `Clear`, `List`) carrying the caller's agent
  id. The first `hud_publish` attaches; `key` replaces the newest unit with
  the same key.
- `PortalOp` crosses an unbounded channel to the winit thread, where
  `crates/tze_hud_runtime/src/portal_projection_driver.rs` applies it to the
  `PortalHub` (`crates/tze_hud_projection/src/hub.rs`), the portal state
  keyed by (agent id, portal id).
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
