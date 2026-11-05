# Connection setup

Read when no connected `hud_*` tools are available or endpoint selection fails.
These commands run from the repository root.

## Pair once

Use the HUD's Connect an agent card and its current six-digit code:

```bash
python3 .claude/skills/user-test/scripts/hud_pair.py --host <host[:port]> --code <on-screen-code>
```

The default agent id is `claude`; `--agent <id>` selects a distinct identity
when authorized. Pairing the same id again rotates its key and affects clients
using that identity. The PSK is stored in `~/.config/tze-hud/<host>.psk` mode
0600 and is never printed. Endpoint metadata retains the actual port. Never
print the key, put it in process arguments, or copy it into MCP configuration.
Do not request `--admin` for ordinary discovery or publishing.

Identity and allowed surfaces come from that PSK. `hud_surfaces` lists only what
the owner's allow list grants; a missing surface is not permission to change it.

## MCP adapter

The repository's `.mcp.json` uses the Linux/WSL stdio adapter; the
[MCP template](../mcp.template.json) shows that configuration. Install/change
client configuration only as requested. With one paired host, the adapter
discovers it without `HUD_HOST`; with multiple hosts, select explicitly.
An explicit `HUD_HOST` wins. New pairs retain custom ports; legacy key-only
pairs default to 9090. Invalid or stale endpoint metadata fails closed.

After connecting/reconnecting the MCP client, observe upstream `tools/list`
with the five `hud_*` verbs and call authenticated `hud_surfaces`. Cached tool
names alone do not prove connectivity. Requests have a 60 s absolute deadline;
cancellation stops local work, but already-delivered remote work is uncertain
and is not automatically replayed. Native Windows/macOS adapter conformance
is not established.

## Python CLI

The explicit Python clients **do not** use the adapter's sole-host discovery.
Set `HUD_HOST=<host[:port]>` or use `publish.py --host <host[:port]>`; include a
custom port explicitly. `--url` remains an alias for selecting the MCP endpoint.
The helper loads only the selected pairing's PSK internally.

```bash
python3 .claude/skills/th-hud-publish/scripts/publish.py --host hud-host.example --list-surfaces
```

Successful authenticated discovery proves connectivity. Follow it with a brief,
finite notification when a visual check is useful; an accepted publish still
needs observation or owner feedback to establish its actual presentation.
