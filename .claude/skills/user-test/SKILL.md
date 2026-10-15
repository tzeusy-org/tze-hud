---
name: user-test
description: Use when validating the Windows HUD end to end from an agent host with no remote shell - pair once, update the HUD to the dev build, publish zone and widget test content over MCP `hud_publish`, take a screenshot as evidence, and read logs on failure.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
    - Claude
  status: active
  last_reviewed: "2026-10-04"
---

# User Test

Everything goes over the HUD's MCP port (default 9090) with your paired PSK.
Contract: `docs/api.md` (pairing, `hud_*` verbs) and
`docs/operations/windows-install.md` (`/admin/*`).

`HUD_HOST` names the HUD (host or `host:port`). Every script here derives the
MCP URL, the gRPC target, and the PSK file `~/.config/tze-hud/$HUD_HOST.psk`
from it. The PSK is never printed; do not read the file into context.

Claude's project MCP entry uses a Linux/WSL stdio adapter: one paired host works
without exporting `HUD_HOST`; multiple hosts require explicit selection. New
pairing writes private nonsecret `<hostname>.endpoint.json` with the actual
HTTP port and selected-key fingerprint. No endpoint metadata means legacy9090;
an old custom port requires re-pairing or explicit `HUD_HOST`. Present invalid
or stale metadata is refused. The established `.psk` store is the only raw-key
file; no key belongs in MCP config, argv, environment or evidence. Ordinary
admin scripts below still use explicit `HUD_HOST`. Fresh Claude authenticated
five-tool discovery is separate live acceptance, not proven by fake fixtures.
The adapter bounds four HTTP workers. Initialize has a 3s absolute startup
deadline from frame acceptance, including DNS, connect and response/read;
established calls such as `hud_input` retain their 60s absolute deadline,
including queued time. Caught resolver failures name DNS, caught connect
failures name TCP, and selected-key or HTTP401/403 refusals name auth. A worker
deadline without a known phase reports DNS/TCP/startup with phase unavailable;
it is not a DNS diagnosis. Errors omit credentials and exception details, with
no fallback, re-pair, retry or replay. Adapter-interpreter startup before frame
acceptance and subsequent TERM/KILL cleanup add scheduling-dependent time;
the accepted-frame budget includes worker startup.
EOF/signals
abort with one global 1s TERM + 1s KILL/reap budget, subject to OS scheduling.
Abrupt parent death relies on Linux kernel termination/OS adoption; native
Windows/macOS client lifetime is unsupported. Remote accepted work is uncertain
and never automatically replayed.

## Steps

1. **Owner, once:** double-click `tze_hud.exe` on the Windows host (installs
   per user and autostarts). Until an agent is paired the HUD shows a 6-digit
   code and its address; the owner reads you the code.
2. **Pair, once per host:**

   ```bash
   export HUD_HOST=<host>
   python3 .claude/skills/user-test/scripts/hud_pair.py --code 482913 --admin
   ```

   `--admin` is needed for steps 3, 5, and 6. Pairing the same agent again
   rotates its key. `PAIR_CODE_INVALID`: wrong code, ask the owner for the
   current one. `PAIRING_CLOSED`: the code is spent, expired, or cooling down
   (60 s); the owner runs `tze_hud.exe --pair` or presses Ctrl+Shift+F10.
3. **Update to the dev build:**
   `python3 .claude/skills/user-test/scripts/hud_admin.py update --channel dev`.
   `up_to_date` means nothing to do; otherwise the HUD installs the signed
   build and relaunches itself, so poll `hud_admin.py status` until `sha`
   changes. The same PSK keeps working.
4. **Publish** zone and widget content (below).
5. **Evidence:** `hud_admin.py screenshot -o /tmp/hud.png` returns the HUD's
   own frame; view the PNG.
6. **On failure:** `hud_admin.py logs --tail 200`, plus `status` (`last_update`,
   `last_restart`, `safe_mode`). Quote exact error payloads.

### Read-only diagnosis and authorized live smoke

`python3 .claude/skills/user-test/scripts/hud_admin.py doctor` checks DNS,
TCP, authenticated `/admin/status`, tailnet inbound policy, then the running
build commit against the resolved rolling `dev` tag. It stops at the first
failure or unknown with an owner action; it never pairs, repairs, updates or
restarts. A parent supervisor enforces each network layer's five-second
deadline even while its local probe child blocks in DNS or response reads.
Only that owned helper is terminated/killed and reaped, with at most 0.4 seconds
of bounded termination waits; an unconfirmed exit is `cleanup_failed`.
This command requires Linux/WSL pidfd and parent-death support; other hosts
report `deadline_unavailable` without an unbounded probe. A 401 requires a valid pairing; 403 `NOT_ADMIN` requires the
owner's admin pairing. Neither grants permission to elevate or re-pair.

After explicit authorization to publish test content, use:

```bash
python3 scripts/ci/windows_smoke.py --url http://<host>:9090/mcp
```

This smoke is **mutating**, unlike `doctor`. Use an idle dedicated paired
agent with no concurrent or pending publications. Existing holdings/portals
are refused; a unique smoke portal and touched surfaces are tracked for
cleanup. MCP clears are agent-and-surface scoped, not atomic per-run tokens;
other agents' content is untouched, but a concurrent use of the same pairing
is unsupported. The URL route never owns or kills the HUD, seeds config or
credentials, reopens pairing, restarts, updates, or repairs firewall rules.
Owned startup/config/WARP, pair/wrong-code, executable restart/health,
quiescent-efficiency emission, resident POC/orphan and theme/fixture capture
checks are individually reported `UNEXECUTED owner-only`. They stay in the
owned CI `--exe` route. This route does not replace the required owner-host acceptance run.

## Publish

Discover first, never invent surface names:
`publish_zone_batch.py --messages-file /dev/null --list-surfaces`.

```bash
python3 .claude/skills/user-test/scripts/publish_zone_batch.py \
  --messages-file /tmp/hud-zone-messages.json --list-surfaces
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --messages-file /tmp/hud-widget-messages.json --cleanup-on-exit
```

Payload shapes, content types per zone, and widget params:
[references/message-payloads.md](references/message-payloads.md). If
`hud_surfaces` lists no widgets, skip widget publishing and say so. Clear
durable widget state at the end (`--cleanup-on-exit` or
`scripts/widget-cleanup.json`).

## Resident tiles from WSL

Use the [resident guide](references/resident-exemplars.md) after pairing. Its
single `uv run` command installs the scripts' PEP 723 dependencies (`grpcio`,
`protobuf`, `Pillow`, `blake3`); system Python alone need not have them. A venv
alternative is documented there. The paired agent needs `allow = ["tiles"]`
(or `"*"`), which includes resource upload and input events.

The resident client derives identity from the paired PSK; `--agent` optionally
asserts that identity. Card labels are cosmetic. It defaults gRPC DNS to
`native` before importing gRPC so WSL can resolve Tailscale short hostnames;
an explicit `GRPC_DNS_RESOLVER` override is honored. Accepted tile/node/update
IDs prove protocol progress. Text/avatar pixels, orphan badges and reclaim
still need a screenshot or owner observation, separately from offline checks.
The exemplar emits those visual observations as `unverified` and drops one
transport without `SessionClose` to exercise orphan grace.

## Behavior Rules

- Require a successful `hud_surfaces` (or `hud_admin.py status`) before
  claiming publish-path success.
- Keep messages configurable from the user prompt; do not hardcode content.
- Treat any publish error as actionable; include the exact response payload.
- Never print, log, or commit the PSK. Pass `--psk-env NAME` only to override
  the file.

## Files

- Admin and pairing: [scripts/hud_pair.py](scripts/hud_pair.py),
  [scripts/hud_admin.py](scripts/hud_admin.py),
  [scripts/hud_env.py](scripts/hud_env.py) (shared `HUD_HOST` and PSK-file resolution).
- Batch publishers and fixtures: [scripts/publish_zone_batch.py](scripts/publish_zone_batch.py),
  [scripts/publish_widget_batch.py](scripts/publish_widget_batch.py),
  `scripts/*.json`.
- Zone exemplars (subtitle, notification, alert-banner, status-bar,
  ambient-background): [references/zone-exemplars.md](references/zone-exemplars.md).
- Widget reactivity (gauge, status-indicator, progress-bar):
  [references/widget-reactivity-tests.md](references/widget-reactivity-tests.md).
- Resident gRPC scenario (Presence Card), `hud_grpc_client.py`, and
  `stress_test_zones.py`: [references/resident-exemplars.md](references/resident-exemplars.md).
