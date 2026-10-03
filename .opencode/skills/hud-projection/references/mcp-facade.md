# Portal over MCP: wiring and boundary

Agents drive a portal with the standard MCP verbs on the surface
`portal:<projection_id>` (`docs/api.md`). Every call is `tools/call`:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"hud_publish","arguments":{"surface":"portal:my-session","content":"hello"}}}
```

There is no other dialect: bare tool-name methods return `-32601`.

## Wiring

- `crates/tze_hud_mcp/src/tools.rs` turns each verb into `PortalOp`
  messages: the first `hud_publish` sends `Attach` (idempotency key
  `<agent>:<projection_id>`), then `PublishOutput` / `PublishStatus`;
  `hud_input` sends `GetPendingInput` and `AcknowledgeInput`; `hud_clear`
  sends `Detach`. `hud_publish`'s `key` is the portal `coalesce_key`.
- `PortalOp` crosses an unbounded channel to the winit thread, where
  `crates/tze_hud_runtime/src/portal_projection_driver.rs` calls the
  in-process `ProjectionAuthority` (`crates/tze_hud_projection/`).
- The bearer PSK identifies the agent. Portal surfaces need `portal` (or `*`)
  in that agent's `[agents.<id>] allow` list; otherwise `NOT_ALLOWED`.

## Boundary rules

- The owner token never reaches the model. The MCP server holds it per
  (agent, projection) and re-attaches once with the same idempotency key if
  the authority reports it stale.
- Responses stay bounded: no transcript history, no tokens, no scene state.
  Delivered input is held server-side until acked, then acked to the
  authority as `handled`.
- The authority's transcript window is in-memory presentation state; durable
  history belongs to the session.
- Accent, unread counts, and composer state are runtime
  internals with no MCP surface (decision 4 in `docs/api.md`).
