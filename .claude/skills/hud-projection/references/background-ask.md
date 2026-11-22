# Background HUD asks

Start one background job through your session's shell runner. Keep working
while the job waits, then read its exit status and single JSON line.

```sh
ask_result=$(mktemp)
python3 .claude/skills/hud-projection/scripts/portal_client.py ask \
  --title 'Ship the patch?' --action 'Ship=ship' --action 'Later=later' \
  --ttl-ms 120000 >"$ask_result" &
ask_pid=$!
```

The job uses the enclosing adapter's HUD when available, or the selected
pairing (`HUD_HOST` selects among multiple paired hosts). It opens its own MCP
session, captioned by the project folder, and posts one notification. Pass up
to four `--action LABEL=ID` flags and a positive `--ttl-ms` if you need more or
less than the default 60 seconds.

| Exit | Result | Meaning |
| --- | --- | --- |
| 0 | `{"action":"ship"}` | The owner chose that callback. |
| 2 | `{"gone":"expired"}` | The card expired, was cleared or displaced, or the owner dismissed it. |
| 1 | `{"code":"MCP_AUTH","hint":"HUD refused authentication (HTTP 401)"}` | The command could not complete. |

The armed publish returns a direct answer when possible. If the card outlives
the armed wait, the job waits for notification input and ACKs only a matching
card key on the notification surface. Use `kill -TERM "$ask_pid"` to cancel
the job. It clears its own keyed card and releases its scope on exit, also
handling SIGINT and SIGHUP. If the job is killed without a chance to clean up,
the card's TTL still applies.

Python callers can import `AskSession` from `portal_client.py`. Inside
`with AskSession(label="PR #7") as ask:`, call
`ask.publish(content, ttl_ms, key=key)`, then `ask.wait()` for the same
answer or gone dictionary. `content` is the MCP notification object and can
include a body and link actions alongside callback actions. `ask.clear()`
clears the question while keeping the scope available for a follow-up
`ask.call_tool(...)`. Leaving the context releases the scope.
