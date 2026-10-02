# Operation Examples

These payloads are provider-neutral. Use the same schema through an external projection-daemon MCP facade, a daemon CLI, or local IPC.

Replace timestamps with wall-clock microseconds and generate unique request IDs.

## Attach

Codex:

```json
{
  "operation": "attach",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-attach-codex-001",
  "client_timestamp_wall_us": 1777400000000000,
  "provider_kind": "codex",
  "display_name": "Codex Worker C",
  "workspace_hint": "/home/tze/gt/tze_hud/mayor/rig",
  "repository_hint": "tze_hud",
  "icon_profile_hint": "codex",
  "content_classification": "private",
  "hud_target": "default",
  "idempotency_key": "codex-worker-c-hud-ggntn4"
}
```

Claude:

```json
{
  "operation": "attach",
  "projection_id": "claude-doc-review",
  "request_id": "req-attach-claude-001",
  "client_timestamp_wall_us": 1777400000000000,
  "provider_kind": "claude",
  "display_name": "Claude Review Session",
  "workspace_hint": "/workspace/project",
  "repository_hint": "project",
  "icon_profile_hint": "claude",
  "content_classification": "private",
  "idempotency_key": "claude-doc-review-20260429"
}
```

opencode:

```json
{
  "operation": "attach",
  "projection_id": "opencode-feature-loop",
  "request_id": "req-attach-opencode-001",
  "client_timestamp_wall_us": 1777400000000000,
  "provider_kind": "opencode",
  "display_name": "opencode Feature Loop",
  "workspace_hint": "/repo",
  "repository_hint": "app",
  "icon_profile_hint": "opencode",
  "content_classification": "private",
  "idempotency_key": "opencode-feature-loop"
}
```

Successful attach responses include `request_id`, `projection_id`, `accepted`, `error_code`, `server_timestamp_wall_us`, bounded `status_summary`, and initial lifecycle state. Over MCP no operation takes or returns `owner_token`; the server keeps it, keyed by the caller's agent and `projection_id`.

## Publish Output

> Accepted `output_kind`: `assistant` (default), `tool`, `status`, `error`, `other`. `viewer` is runtime-reserved (the operator-reply echo) and rejected if published.

```json
{
  "operation": "publish_output",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-output-001",
  "client_timestamp_wall_us": 1777400001000000,
  "output_text": "Implemented the HUD projection skill package and mirror docs.",
  "output_kind": "assistant",
  "content_classification": "private",
  "logical_unit_id": "turn-42",
  "coalesce_key": "latest-summary"
}
```

## Publish Status

> Accepted `lifecycle_state`: `attached`, `active`, `degraded`, `hud_unavailable`, `detached`, `cleanup_pending`, `expired`.

```json
{
  "operation": "publish_status",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-status-001",
  "client_timestamp_wall_us": 1777400002000000,
  "lifecycle_state": "active",
  "status_text": "Verifying mirror consistency"
}
```

## Get Pending Input

```json
{
  "operation": "get_pending_input",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-input-001",
  "client_timestamp_wall_us": 1777400003000000,
  "max_items": 4,
  "max_bytes": 4096,
  "wait_ms": 15000
}
```

Handle only the bounded input items returned. If more input is queued, the response should report compact remaining counts instead of returning unbounded inbox history.

> `wait_ms` (optional, max 30000): long-poll — the call blocks until a reply arrives or the wait elapses, so you can await the operator with one call instead of busy-polling. Omit (or `0`) to return immediately.

## Acknowledge Input

Handled:

```json
{
  "operation": "acknowledge_input",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-ack-001",
  "client_timestamp_wall_us": 1777400004000000,
  "input_id": "input-0007",
  "ack_state": "handled",
  "ack_message": "Applied the requested edit."
}
```

Deferred:

```json
{
  "operation": "acknowledge_input",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-ack-002",
  "client_timestamp_wall_us": 1777400005000000,
  "input_id": "input-0008",
  "ack_state": "deferred",
  "ack_message": "Will revisit after tests finish.",
  "not_before_wall_us": 1777400010000000
}
```

Use `not_before_wall_us` only when `ack_state` is `deferred`; it must be before the input item's expiry. Deferral only delays redelivery. The projection authority still expires the item at or after the item's `expires_at_wall_us`.

## Detach

```json
{
  "operation": "detach",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-detach-001",
  "client_timestamp_wall_us": 1777400006000000,
  "reason": "session complete"
}
```

## Cleanup

Owner cleanup:

```json
{
  "operation": "cleanup",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-cleanup-001",
  "client_timestamp_wall_us": 1777400007000000,
  "reason": "remove stale portal after normal detach"
}
```

Operator cleanup uses separate daemon authority, not the owner token, and must be audited distinctly:

```json
{
  "operation": "cleanup",
  "projection_id": "codex-rig-hud-ggntn4",
  "request_id": "req-cleanup-operator-001",
  "client_timestamp_wall_us": 1777400008000000,
  "operator_authority": "<operator-authority>",
  "reason": "operator removed orphaned projection"
}
```

## Stable Error Codes

Treat these as append-only strings:

- `PROJECTION_NOT_FOUND`
- `PROJECTION_ALREADY_ATTACHED`
- `PROJECTION_UNAUTHORIZED`
- `PROJECTION_TOKEN_EXPIRED`
- `PROJECTION_INVALID_ARGUMENT`
- `PROJECTION_OUTPUT_TOO_LARGE`
- `PROJECTION_INPUT_TOO_LARGE`
- `PROJECTION_INPUT_QUEUE_FULL`
- `PROJECTION_RATE_LIMITED`
- `PROJECTION_STATE_CONFLICT`
- `PROJECTION_HUD_UNAVAILABLE`
- `PROJECTION_INTERNAL_ERROR`
