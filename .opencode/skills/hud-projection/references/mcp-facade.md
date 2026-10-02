# External Projection-Daemon Control Facades

Projection control facades belong to the projection daemon. They are not the runtime v1 MCP bridge and should not expose raw scene state, zone publishing shortcuts, PTY attachment, terminal capture, or process lifecycle controls.

## Required Boundary

- Accept only the cooperative operation contract from `operation-examples.md`.
- Authenticate callers through daemon-local authority, an MCP bearer token, OS-protected IPC, or another unguessable credential.
- Bind owner-scoped non-attach operations to `projection_id` plus the caller's agent identity: the MCP server keeps the owner token server-side, keyed by (agent, projection), and injects it; bind operator cleanup to separate explicit operator authority.
- Return bounded operation responses only: no unbounded transcript, unbounded inbox history, `owner_token` (the MCP server strips it from attach), owner-token verifier, or raw runtime scene graph.
- Emit audit records without transcript text, HUD input text, or owner tokens.

## In-Repo Stdio Daemon CLI

The repo ships a daemon-local stdio control surface in `crates/tze_hud_projection`:

```bash
cargo run -p tze_hud_projection --bin tze_hud_projection_authority -- --stdio --caller-identity codex-local
```

Send one operation JSON object per stdin line. The process writes one JSON result per stdout line:

```json
{"response":{"request_id":"req-attach","projection_id":"codex-rig","accepted":true,"server_timestamp_wall_us":1777400000000000,"status_summary":"projection attached","owner_token":"<attach-only>","lifecycle_state":"attached","pending_remaining_count":0,"pending_remaining_bytes":0,"portal_update_ready":false,"coalesced_output_count":0},"audit_records":[{"timestamp_wall_us":1777400000000000,"operation":"attach","projection_id":"codex-rig","caller_identity":"codex-local","request_id":"req-attach","accepted":true,"reason":"attach accepted","category":"attach"}]}
```

The CLI keeps projection state in memory only for the lifetime of that process. Restarting it purges transcript text, pending input text, owner tokens, and cached lease identity, so sessions must attach again after restart. Operator cleanup can be enabled with `--operator-authority-env HUD_PROJECTION_OPERATOR_AUTHORITY`; owner operations still require the owner token issued by `attach`.

## Tool Shape

Either shape is acceptable if the payload schema remains the same:

- One dispatcher tool, for example `projection_operation(payload)`.
- Seven operation tools: `attach`, `publish_output`, `publish_status`, `get_pending_input`, `acknowledge_input`, `detach`, and `cleanup`.

The skill examples use operation JSON payloads so they work with either facade shape.

## Claude-Style MCP Configuration

Use `settings.template.json` as a starting point:

```json
{
  "mcpServers": {
    "hud-projection-daemon": {
      "type": "url",
      "url": "http://<PROJECTION_DAEMON_HOST>:<PORT>/mcp",
      "headers": {
        "Authorization": "Bearer ${HUD_PROJECTION_DAEMON_TOKEN}"
      }
    }
  }
}
```

Keep the server name explicit. Avoid names such as `tze-hud` that can be confused with the runtime v1 MCP zone publishing bridge.
