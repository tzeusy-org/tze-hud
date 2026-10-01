---
name: user-test
description: Use when validating a cross-machine HUD flow where Butler deploys/runs the full Windows app over SSH+SCP (tailnet default host), then publishes configurable test messages to HUD zones via MCP `publish_to_zone`.
metadata:
  owner: tze
  authors:
    - tze
    - OpenAI Codex
    - Claude
  status: active
  last_reviewed: "2026-06-20"
---

# User Test

Run an automation-first cross-machine validation loop:

1. Deploy a full Windows application `.exe` from Linux.
2. Launch it on Windows over SSH/SCP.
3. Verify MCP HTTP is reachable.
4. Publish configurable zone test messages.
5. Publish configurable widget test messages.

## Required Inputs

Collect these before executing:

- `full_app_exe` (required): absolute/relative Linux path to the full application `.exe` to deploy
- `package` (optional fallback): cargo package/bin crate to build only if explicitly requested
- `target` (default: `x86_64-pc-windows-gnu`)
- `profile` (`release` or `debug`, default: `release`)
- `win_user` (default: `hud-user`)
- `win_host` (default: `windows-host.example`; for autonomous runs use the
  `hud-windows` VM — see "Autonomous testhost" below)
- `ssh_key_path` (default in local environment: `~/.ssh/hud-ssh-key`)
- `task_name` (default: `TzeHudOverlay`)
- `mcp_http_url` (default: `http://windows-host.example:9090`)
- `mcp_psk_env` (default: `MCP_TEST_PSK`)
- `messages`: array of zone publishes
- `widget_messages`: array of widget publishes (optional)

For the exact `messages` / `widget_messages` payload shapes, content types by
zone, widget parameter types, and `widget_name` instance-discovery semantics
(`list_widgets`), see
[references/message-payloads.md](references/message-payloads.md).

## Autonomous Testhost (hud-windows VM)

When the flow needs no human eyes (or tzehouse is unavailable), target the
IaC-managed Windows 11 VM on the Sentinel Proxmox host instead. The VM uses its
own literal accounts `hud-user` (SCP) and `admin-user` (autologon desktop owner,
process control), both keyed with `~/.ssh/hud-ssh-key`. `C:\tze_hud` exists;
firewall already allows 22/9090/50051 inbound.

> **These VM accounts are NOT the tzehouse contract.** tzehouse uses different
> real users and a different key — resolve them from the private doc
> `docs/operations/private/tzehouse-windows.local.md` (git-ignored). Do not try
> `hud-user`/`admin-user`/`hud-ssh-key` against tzehouse; they are rejected there.

Resolve the address (and self-heal the whole surface — VM start, stale
gpu.lock clearing per hud-7gp40, HUD task relaunch) with the canonical helper:

```bash
eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
# exports TZE_HUD_TEST_HOST, HUD_MCP_URL, HUD_MCP_PSK, MCP_TEST_PSK,
# TZE_HUD_MCP_RESIDENT_PRINCIPAL
WIN_HOST=$TZE_HUD_TEST_HOST
```

Availability is layered: Proxmox `onboot=1` + a sentinel systemd keepalive
timer restart the VM (guest-initiated shutdowns bypass onboot), autologon +
the ONLOGON `TzeHudFullscreen` task restart the HUD, and the helper covers
whatever remains at call time.

Launch contract (verified 2026-07-04, end-to-end: SCP deploy → task launch →
networked MCP publish → subtitle rendered on the VM console):

- Scheduled task **`TzeHudFullscreen`** (exe-direct, pre-registered by the
  VM's firstboot IaC):
  `C:\tze_hud\tze_hud.exe --window-mode fullscreen --config C:\tze_hud\tze_hud.toml --bind-all-interfaces`
  Deploy = SCP the exe, `taskkill /F /IM tze_hud.exe` (admin-user), then
  `schtasks /Run /TN TzeHudFullscreen`.
- `C:\tze_hud\tze_hud.toml` (profile `full-display`) and the PSK machine env
  (`TZE_HUD_PSK` = `TZE_HUD_MCP_RESIDENT_PRINCIPAL`) are provisioned by
  firstboot. The PSK value is `HUD_WINDOWS_PSK` in
  `~/gt/homelab/mayor/rig/.env` on the controller.
- Console proof shots without a human: on sentinel,
  `pvesh create /nodes/sentinel/qemu/110/monitor -command "screendump /var/tmp/vm110.ppm"`,
  scp + convert locally.

Capabilities and limits vs tzehouse:

- **No discrete GPU** (yet — iGPU passthrough is a tracked follow-up): wgpu
  runs on WARP software rendering (verified working: full HUD + zone
  publishes render). Functional/protocol validation only; rendering fidelity
  and performance results are NOT representative.
- **Use `--window-mode fullscreen`, not `overlay`** (the only two modes).
  Overlay transparency needs Vulkan + a real GPU; overlay-path validation
  stays on tzehouse.
- 3 vCPUs (Intel N150 E-cores), 6 GB RAM — keep perf expectations low.
- The VM is cattle: rebuild from scratch via the homelab repo
  (`~/gt/homelab/mayor/rig/ansible`, `--tags windows`; see its README).
  Eval license: 90 days per rebuild.

## Subskills

- **portal-hud-deploy** ([subskills/portal-hud-deploy/SKILL.md](subskills/portal-hud-deploy/SKILL.md)) —
  one deterministic command to deploy a freshly-built `tze_hud.exe`, kill the
  stale instance, launch the **transparent overlay** HUD (exe-direct scheduled
  task), and verify gRPC + MCP ports + reachability. Use this instead of the
  generic `deploy_windows_hud.sh` when you need the overlay/portal launch with
  port verification.

## Support File Index

Use these files when the matching workflow section below calls for them:

- Target resolution + gates: [scripts/tzehouse_env.sh](scripts/tzehouse_env.sh) (tzehouse resolve/self-heal/PSK/MCP gate), [scripts/hud_vm_env.sh](scripts/hud_vm_env.sh) (autonomous VM), [scripts/portal_trial.sh](scripts/portal_trial.sh) (one-command portal connectivity trial)
- Deployment and setup: [scripts/deploy_windows_hud.sh](scripts/deploy_windows_hud.sh), [scripts/windows_setup_hud_automation.ps1](scripts/windows_setup_hud_automation.ps1), [scripts/d18_validation.sh](scripts/d18_validation.sh), [subskills/portal-hud-deploy/scripts/deploy_portal_hud.sh](subskills/portal-hud-deploy/scripts/deploy_portal_hud.sh)
- Batch publishers and broad zone fixtures: [scripts/publish_zone_batch.py](scripts/publish_zone_batch.py), [scripts/publish_widget_batch.py](scripts/publish_widget_batch.py), [scripts/all-zones-test.json](scripts/all-zones-test.json), [scripts/widget-cleanup.json](scripts/widget-cleanup.json)
- Widget fixtures: [scripts/gauge_cycle_test.json](scripts/gauge_cycle_test.json), [scripts/progress-bar-step.json](scripts/progress-bar-step.json), [scripts/progress-bar-color-sweep.json](scripts/progress-bar-color-sweep.json), [scripts/progress-bar-rapidfire-100-5s.json](scripts/progress-bar-rapidfire-100-5s.json), [scripts/status-indicator-enum-cycle-test.json](scripts/status-indicator-enum-cycle-test.json), [scripts/status-indicator-theme-cycle-test.json](scripts/status-indicator-theme-cycle-test.json), [scripts/status-indicator-label-update-test.json](scripts/status-indicator-label-update-test.json), [scripts/status-indicator-validation-test.json](scripts/status-indicator-validation-test.json), [scripts/status-indicator-contention-test.json](scripts/status-indicator-contention-test.json), [scripts/status-indicator-theme-status-matrix-test.json](scripts/status-indicator-theme-status-matrix-test.json)
- Zone exemplars and fixtures: [scripts/subtitle_exemplar.py](scripts/subtitle_exemplar.py), [scripts/subtitle-full-sequence.json](scripts/subtitle-full-sequence.json), [scripts/subtitle-single-line.json](scripts/subtitle-single-line.json), [scripts/subtitle-multiline.json](scripts/subtitle-multiline.json), [scripts/subtitle-rapid-replace.json](scripts/subtitle-rapid-replace.json), [scripts/subtitle-streaming.json](scripts/subtitle-streaming.json), [scripts/subtitle-ttl-expiry.json](scripts/subtitle-ttl-expiry.json), [scripts/notification_exemplar.py](scripts/notification_exemplar.py), [scripts/notification-full-gamut.json](scripts/notification-full-gamut.json), [scripts/alert_banner_exemplar.py](scripts/alert_banner_exemplar.py), [scripts/status_bar_exemplar.py](scripts/status_bar_exemplar.py), [scripts/ambient_background_exemplar.py](scripts/ambient_background_exemplar.py)
- Resident gRPC, portal, and stress helpers: [scripts/hud_grpc_client.py](scripts/hud_grpc_client.py), [scripts/test_hud_grpc_client.py](scripts/test_hud_grpc_client.py), [scripts/presence_card_exemplar.py](scripts/presence_card_exemplar.py), [scripts/text_stream_portal_exemplar.py](scripts/text_stream_portal_exemplar.py), [scripts/stress_test_zones.py](scripts/stress_test_zones.py)

## Reference Files

Detailed payload shapes, extended workflow steps, and per-surface exemplar
scenarios live in `references/`. Load the one matching the task:

- [references/message-payloads.md](references/message-payloads.md) — zone message
  and widget payload shapes, content types by zone, widget parameter types, and
  `widget_name` instance discovery (`list_widgets`).
- [references/widget-reactivity-tests.md](references/widget-reactivity-tests.md) —
  Workflow Steps 5–7: gauge cycling, status-indicator (enum/theme/label/validation),
  and progress-bar (7-step, color sweep, rapid-fire) reactivity tests.
- [references/zone-exemplars.md](references/zone-exemplars.md) — MCP `publish_to_zone`
  exemplar scenarios: subtitle, notification stack, alert-banner, status-bar, and
  ambient-background.
- [references/resident-exemplars.md](references/resident-exemplars.md) — resident
  gRPC session scenarios: Presence Card and Text Stream Portals.

## Workflow

### Step 0: SSH Connectivity Gate

**Deterministic path (preferred):** both standing targets have a canonical
resolve-and-self-heal helper — run it instead of hand-discovering anything:

```bash
# tzehouse (human-in-loop screen): SSH gates both users, detects/kills a
# wrong-config instance (e.g. loopback-bound benchmark leftover), relaunches
# TzeHudOverlay, recovers the PSK live from the task XML, gates MCP initialize.
eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"
# -> TZE_HUD_TEST_HOST, WIN_HOST, WIN_FILE_USER, WIN_ADMIN_USER, HUD_SSH_KEY,
#    HUD_MCP_URL, HUD_GRPC_TARGET, HUD_PSK, MCP_TEST_PSK, TZE_HUD_PSK,
#    TZE_HUD_MCP_RESIDENT_PRINCIPAL

# hud-windows VM (autonomous): see "Autonomous testhost" above
eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
```

`tzehouse_env.sh` reads identity from the git-ignored
`.claude/skills/user-test/target.env` (see `.gitignore`; template fields:
`TZEHOUSE_HOST`, `TZEHOUSE_FILE_USER`, `TZEHOUSE_ADMIN_USER`,
`TZEHOUSE_SSH_KEY`). The PSK is never stored in any file — it is recovered
live from the `TzeHudOverlay` task definition each run.

For a one-command text-stream-portal connectivity trial (env + gates + attach
+ greeting + long-poll with auto-ack), run
`scripts/portal_trial.sh [--target tzehouse|vm] [--detach-after]`; it leaves
the projection attached for an LLM session to take over with
`.claude/skills/hud-projection/scripts/portal_client.py` (exit 3 = gates all
passed but no operator input arrived within the poll budget).

**Manual fallback:** resolve the real host, users, and key by hand. Tracked
files carry scrubbed placeholders (`windows-host.example` / `hud-user` /
`admin-user` / `~/.ssh/hud-ssh-key`); the real values differ per target and
live in the git-ignored private doc
`docs/operations/private/tzehouse-windows.local.md`. Read it before running
the gate — never inline real values into this tracked file (AGENTS.md
placeholder contract). Note tzehouse's default shell is **cmd.exe** (not
PowerShell) — don't chain with `;`; invoke `powershell -Command` explicitly
when you need it.

Verify key auth for **both** users (Linux), substituting the resolved values:

```bash
ssh -o BatchMode=yes -o IdentitiesOnly=yes -i <key> <file-user>@<host> "whoami"
ssh -o BatchMode=yes -o IdentitiesOnly=yes -i <key> <admin-user>@<host> "whoami"
```

Both must succeed. The file user is used for file deployment (SCP). The admin
user is used for process control (kill, scheduled task trigger) because it owns
the interactive desktop session. Whether tzehouse's two roles share one key or
use separate keys is part of what the private doc resolves — don't assume.

### Step 1: Deploy (SCP via hud-user)

Copy the prebuilt `.exe` to the Windows host:

```bash
# Kill any running instance first (must use admin-user — hud-user can't kill it)
ssh -i ~/.ssh/hud-ssh-key -o BatchMode=yes -o StrictHostKeyChecking=no \
  admin-user@windows-host.example "taskkill /F /IM tze_hud.exe"
sleep 2

# SCP the exe (via hud-user)
scp -i ~/.ssh/hud-ssh-key -o BatchMode=yes -o StrictHostKeyChecking=no \
  /path/to/tze_hud.exe \
  hud-user@windows-host.example:C:/tze_hud/tze_hud.exe
```

Report: file size, checksum (`sha256sum`), remote path.

### Step 2: Register + Launch (via admin-user)

The HUD **must** be launched via a scheduled task as `admin-user` with `--window-mode overlay`. This is critical for transparency — SSH-launched processes cannot access the desktop GPU, and `run_hud.ps1` wrappers interfere with window creation.

```bash
# Register the overlay task (idempotent — safe to re-run)
ssh -i ~/.ssh/hud-ssh-key -o BatchMode=yes -o StrictHostKeyChecking=no \
  admin-user@windows-host.example \
  "powershell -NoProfile -Command \"Register-ScheduledTask -TaskName 'TzeHudOverlay' \
    -Action (New-ScheduledTaskAction \
      -Execute 'C:\\tze_hud\\tze_hud.exe' \
      -Argument '--window-mode overlay' \
      -WorkingDirectory 'C:\\tze_hud') \
    -Settings (New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries) \
    -Force\""

# Launch it
ssh -i ~/.ssh/hud-ssh-key -o BatchMode=yes -o StrictHostKeyChecking=no \
  admin-user@windows-host.example \
  "schtasks /Run /TN TzeHudOverlay"
```

**Transparency requirements** (if the window is grey/opaque, one of these is wrong):
- `--window-mode overlay` — fullscreen mode is intentionally opaque
- Task runs as `admin-user` (the user logged into the console desktop)
- Exe runs directly (no PowerShell/bat wrapper — wrapper windows break transparency)
- NVIDIA driver 595.97+ on the Windows host
- Commit must include `with_no_redirection_bitmap(true)`, Vulkan forcing, PreMultiplied alpha

### Step 2: MCP Reachability Gate

Require live MCP HTTP reachability before publish.

- Default URL: `http://windows-host.example:9090`
- If MCP HTTP is unreachable, stop and report launch/runtime mismatch.
- Do not treat startup subtitle simulation as a substitute for MCP publish validation.

### Step 3: Publish Configurable Zone Messages

Use `scripts/publish_zone_batch.py` from this skill.

Recommended sequence:

1. Generate a temporary JSON file with user-provided messages.
2. List zones first (`--list-zones`) for visibility.
3. Publish the full message batch.
4. Return per-message results (success/failure, ids, and errors).

Example:

```bash
python3 .claude/skills/user-test/scripts/publish_zone_batch.py \
  --url "$MCP_HTTP_URL" \
  --psk-env MCP_TEST_PSK \
  --messages-file /tmp/hud-zone-messages.json \
  --list-zones
```

### Step 4: Publish Configurable Widget Messages

Use `scripts/publish_widget_batch.py` from this skill.

Recommended sequence:

1. Generate a temporary JSON file with user-provided widget messages.
2. List widgets first (`--list-widgets`) to discover available widget types and instances.
3. Publish the full widget message batch.
4. Return per-message results (success/failure, applied params, and errors).
5. For manual user-test runs that touch durable widget instances, clear them at the end with `--cleanup-on-exit` or the cleanup fixture below. This prevents stale widget state from remaining on the HUD after interrupted or partial tests.

Example:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --url "$MCP_HTTP_URL" \
  --psk-env MCP_TEST_PSK \
  --messages-file /tmp/hud-widget-messages.json \
  --list-widgets \
  --cleanup-on-exit
```

If `list_widgets` returns no instances, skip widget publishing and report that no widgets are registered (the HUD binary may predate widget support).

To clear stale widget state explicitly, run:

```bash
python3 .claude/skills/user-test/scripts/publish_widget_batch.py \
  --url "$MCP_HTTP_URL" \
  --psk-env MCP_TEST_PSK \
  --messages-file .claude/skills/user-test/scripts/widget-cleanup.json
```

### Steps 5–7: Widget Reactivity Tests

After Step 4, run the gauge, status-indicator, and progress-bar reactivity
tests. Full step-by-step instructions, fixtures, expected visuals, and human
acceptance criteria are in
[references/widget-reactivity-tests.md](references/widget-reactivity-tests.md).

## Exemplar Scenarios

Beyond the core deploy/publish loop, this skill bundles per-surface exemplar
scenarios. Each lives in a reference file with its own CLI, phases/sequence,
visual checklist, and payload shape:

- **Zone exemplars** (MCP `publish_to_zone`): subtitle, notification stack,
  alert-banner, status-bar, ambient-background —
  [references/zone-exemplars.md](references/zone-exemplars.md).
- **Resident gRPC exemplars**: Presence Card and Text Stream Portals —
  [references/resident-exemplars.md](references/resident-exemplars.md).

## Behavior Rules

- Use automation-first deploy/launch by default.
- Default to full app deployment via SCP + scheduled task.
- Use `--package` / cargo build only when explicitly requested.
- Always pass non-interactive SSH/SCP flags (`BatchMode`, `IdentitiesOnly`) for automation safety.
- Use `hud-user` for file operations (SCP, mkdir). Use `admin-user` for process control (taskkill, schtasks).
- **Always launch via `TzeHudOverlay` scheduled task with `--window-mode overlay`.** Never launch via `run_hud.ps1` or direct SSH exec — both produce grey opaque windows.
- **Never use `Start-Process -RedirectStandardOutput`** — it sets `CREATE_NO_WINDOW` which breaks `WS_EX_NOREDIRECTIONBITMAP` transparency.
- Require real MCP HTTP reachability before claiming publish-path success.
- Never invent zone names or endpoint values; require user-provided values.
- Treat any publish error as actionable; include exact response payload.
- Keep messages configurable from the user prompt; do not hardcode content.
- Assume Windows host defaults to `windows-host.example` unless user overrides.
```