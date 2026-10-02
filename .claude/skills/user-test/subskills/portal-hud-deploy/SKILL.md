---
name: portal-hud-deploy
description: Use when you need to deploy a freshly-built tze_hud.exe to the Windows host and launch the TRANSPARENT overlay HUD in one deterministic command — kill the stale instance, scp + checksum-verify the exe, launch it as a scheduled task (exe-direct, so overlay transparency is preserved), wait for gRPC + MCP ports to bind, and confirm MCP HTTP reachability. Subskill of /user-test.
metadata:
  owner: tze
  authors:
    - tze
    - Claude
  status: active
  last_reviewed: "2026-06-21"
---

# Portal HUD Deploy (subskill of `/user-test`)

Consolidates the "deploy a freshly-built `tze_hud.exe` → kill stale instance →
launch the transparent overlay HUD on the Windows host → verify ports +
reachability" flow into a single deterministic command. No rediscovery needed.

Parent skill: [`../../SKILL.md`](../../SKILL.md). Use this subskill instead of
the parent's generic `deploy_windows_hud.sh` when you specifically need the
**overlay (transparent) portal launch + port/reachability verification**.

## The one command

Real host values live in `docs/operations/private/tzehouse-windows.local.md`
(git-ignored). Read that file for the values, then:

```bash
WIN_HOST=<host> WIN_FILE_USER=<file-user> WIN_ADMIN_USER=<admin-user> SSH_KEY=<key> \
.claude/skills/user-test/subskills/portal-hud-deploy/scripts/deploy_portal_hud.sh \
  --local-exe target/x86_64-pc-windows-gnu/release/tze_hud.exe
```

Defaults (scrubbed placeholders, override via env/flags): `WIN_HOST=windows-host.example`,
`WIN_FILE_USER=hud-user`, `WIN_ADMIN_USER=admin-user`, `SSH_KEY=~/.ssh/hud-ssh-key`.
Run `deploy_portal_hud.sh --help` for the full flag list.

## What it does (deterministic steps)

1. **Preflight** — verifies SSH key auth for BOTH users (`whoami`). The same key
   authenticates the file user and the admin user.
2. **Checksum** — sha256 + byte size of the local exe.
3. **Kill stale** — as admin: `Stop-ScheduledTask` for `TzeHudOverlay`,
   `TzeHudPortalVal`, `TzeHud8dht5Media` (+ this task), `Stop-Process tze_hud`,
   then waits until the gRPC port is closed (fail loud on timeout).
4. **Deploy** — scp the exe to `C:\tze_hud\tze_hud.exe` (via file user), then
   verify the remote sha256 == local sha256. **Fails loudly on mismatch.**
5. **Launch** — copies `launch_portal_hud.ps1` to the host and runs it as admin.
   It registers a scheduled task whose action runs the exe **directly**
   (`New-ScheduledTaskAction -Execute 'C:\tze_hud\tze_hud.exe' -Argument '…' -WorkingDirectory 'C:\tze_hud'`),
   starts it, and polls `Get-NetTCPConnection` until BOTH gRPC (50051) and MCP
   (9090) are Listening. Emits a JSON result `{task, ports, pid, bound, …}`.
6. **Reachability gate** — from Linux, POSTs a JSON-RPC `tools/list` request to
   `http://<host>:9090/mcp` with `Authorization: Bearer <PSK>` and confirms the
   server answers (skip with `--no-verify`).

## Parameters

| Flag | Default | Meaning |
|---|---|---|
| `--local-exe` (required) | — | local freshly-built `tze_hud.exe` |
| `--win-host` / `$WIN_HOST` | `windows-host.example` | Windows host (tailnet node) |
| `--file-user` / `$WIN_FILE_USER` | `hud-user` | SSH user for SCP (file deploy) |
| `--admin-user` / `$WIN_ADMIN_USER` | `admin-user` | SSH user for launch (owns desktop) |
| `--ssh-key` / `$SSH_KEY` | `~/.ssh/hud-ssh-key` | SSH identity (same key, both users) |
| `--remote-dir` | `C:\tze_hud` | Windows install dir |
| `--remote-exe` | `<remote-dir>\tze_hud.exe` | target exe path |
| `--config` | `C:\tze_hud\tze_hud.toml` | `--config` value |
| `--task-name` | `TzeHudPortalDeploy` | scheduled task name |
| `--grpc-port` | `50051` | gRPC port |
| `--mcp-port` | `9090` | MCP HTTP port |
| `--no-verify` | (off) | skip the MCP reachability gate |

## Transparency constraints (the grey-tile failure modes)

The overlay's per-pixel transparency depends on `WS_EX_NOREDIRECTIONBITMAP`,
which the OS silently disables under `CREATE_NO_WINDOW`. If the HUD comes up
**grey/opaque** instead of transparent, one of these was violated:

- **Exe-direct launch only.** The scheduled-task action MUST `-Execute` the exe
  directly. NEVER wrap it in `cmd.exe` or `powershell` — a wrapper sets
  `CREATE_NO_WINDOW` and kills transparency.
- **No stdout redirect.** NEVER use `>` / `2>&1` / `Start-Process -RedirectStandardOutput`
  on the launch — redirection also implies `CREATE_NO_WINDOW`.
  (The host's `start-portal-hud-logged.ps1` deliberately does both to capture a
  debug log; it is a DEBUG variant that sacrifices transparency. This subskill
  must never do that — model on `start-portal-hud.ps1`, not the `-logged` one.)
- **`--window-mode overlay`.** Fullscreen mode is intentionally opaque.
- **Runs as the admin / interactive-desktop user** (the one logged into the
  console), so the process can reach the desktop GPU.

## PSK model

The PSK identifies the agent; its `[agents.<id>] allow` list decides what it
may do. The shipped config's `[agents.claude]` uses `psk_env = "TZE_HUD_PSK"`,
which always means the runtime PSK (however it was passed), so a caller sending
the `--psk` value is `claude` with `allow = ["*"]`. On the host the PSK is
stored in the admin user's `TZE_HUD_MCP_RESIDENT_PRINCIPAL` env var (a storage
name only; the runtime no longer reads it).

**The secret never crosses the Linux command line.** `launch_portal_hud.ps1`
reads `TZE_HUD_MCP_RESIDENT_PRINCIPAL` from the host environment itself (User
scope first, then process) and bakes it into the task's `--psk`. The reachability
gate fetches the PSK over SSH into a transient shell variable only for the curl
call, then scrubs it. It is never stored in any tracked file and never logged.

## MCP reachability nuance

The runtime is not a full MCP server. It does **not** implement the standard
`initialize` method, and (verified live on the current build) not even
`tools/list` — both return `-32601 Method not found`. That reply still proves
reachability + auth: the server parsed the bearer token and answered a JSON-RPC
envelope echoing the request `id`. The gate therefore treats ANY JSON-RPC body
carrying the request `id` (or `"jsonrpc"`) as "server answered"; an HTTP/auth
failure (no JSON-RPC body) fails the gate. Observed positive response:

```json
{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found: tools/list"},"id":"deploy-gate"}
```

(The real publish methods like `hud_publish` / `hud_publish` ARE
implemented — see the parent skill's batch publishers — but the bare deploy gate
only needs a reachability+auth signal, which `-32601` already provides.)

## Files

- `scripts/deploy_portal_hud.sh` — the single bash entrypoint (parameterized,
  `--help`, fail-loud on checksum mismatch / port-not-bound; non-interactive SSH
  flags `BatchMode`/`IdentitiesOnly`).
- `scripts/launch_portal_hud.ps1` — overlay launch helper copied to the host and
  run as admin (exe-direct, no wrapper, no redirect; PSK from host env).

## Constraints

- No secrets in any tracked file. Real host values come from the private doc at
  run time; tracked defaults stay as `windows-host.example` / `hud-user` /
  `admin-user` / `~/.ssh/hud-ssh-key` placeholders.
- Read-only against a live HUD: re-running this WILL stop the current instance
  (step 3). Do not run it mid-validation unless you intend to take over the host.
