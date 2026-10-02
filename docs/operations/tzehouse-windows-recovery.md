# TzeHouse Windows Recovery Runbook

Reference host: `windows-host.example` / `198.51.100.1`

This runbook is for restoring the Windows HUD reference host before `/user-test`
validation, strict smoke, or the 60-minute Windows soak. It contains no secrets.

## Current Recovery Boundary

The repository does not currently define a safe Wake-on-LAN, router, BIOS, or
Synology-mediated command that can power on or wake the Windows machine remotely.
When the Windows node is offline in Tailscale and SSH port `22` times out, the
supported recovery route is manual/operator action on the Windows host.

The adjacent `proxmox-host.example` node may be online while
Windows is offline, but that is not by itself a supported recovery path unless an
operator supplies and documents a no-secret Wake-on-LAN or equivalent procedure.

## Manual Operator Action

1. Confirm `TzeHouse` is powered on.
2. Confirm Windows is connected to the network.
3. Confirm Tailscale is running and the Windows node is online in the tailnet.
4. Confirm Windows OpenSSH is running.
5. Confirm the existing `~/.ssh/hud-ssh-key` public key is accepted for the
   required users, especially `admin-user` and `hud-user`.
6. Confirm the intended HUD scheduled task exists:
   - `TzeHudOverlay` for production validation.
   - `TzeHudBenchmarkOverlay` for benchmark/soak validation.

## Verification From This Workspace

Run these from `/home/tze/gt/tze_hud/mayor/rig`.

```bash
tailscale status --json | jq '.Peer[] | select(.DNSName=="windows-host.example.") | {HostName,DNSName,Online,LastSeen,TailscaleIPs}'
timeout 12 tailscale ping -c 1 windows-host.example
```

```bash
timeout 12 ssh -i ~/.ssh/hud-ssh-key -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=8 \
  admin-user@windows-host.example "whoami"

timeout 12 ssh -i ~/.ssh/hud-ssh-key -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=8 \
  hud-user@windows-host.example "whoami"
```

```bash
for port in 22 50051 9090; do
  timeout 6 bash -lc "cat < /dev/null > /dev/tcp/windows-host.example/$port" \
    >/dev/null 2>&1 && echo "$port open" || echo "$port closed_or_timeout"
done
```

## HUD Task Bring-Up

If SSH succeeds but `50051` or `9090` is not listening, start the appropriate
scheduled task from the interactive Windows account. Use the task that matches
the validation mode.

Production validation:

```bash
ssh -i ~/.ssh/hud-ssh-key -o IdentitiesOnly=yes -o BatchMode=yes \
  admin-user@windows-host.example \
  'schtasks /Run /TN TzeHudOverlay'
```

Benchmark/soak validation:

```bash
ssh -i ~/.ssh/hud-ssh-key -o IdentitiesOnly=yes -o BatchMode=yes \
  admin-user@windows-host.example \
  'schtasks /Run /TN TzeHudBenchmarkOverlay'
```

The HUD runtime must use a non-default PSK. Do not write PSKs into this file or
into Beads notes.

## Smoke Before Soak

After the task starts, repeat the TCP probes above and use the MCP `/mcp`
endpoint, not the bare port URL.

MCP discovery (zones and widgets the PSK may use):

```bash
python3 .claude/skills/th-hud-publish/scripts/publish.py \
  --url http://windows-host.example:9090/mcp \
  --psk-env MCP_TEST_PSK \
  --list-surfaces
```

gRPC session smoke:

```bash
python3 .claude/skills/user-test/scripts/hud_grpc_client.py \
  --target windows-host.example:50051 \
  --psk "$TZE_HUD_PSK"
```

Only proceed to strict smoke or `hud-nfl7n` after:

- Tailscale ping succeeds.
- SSH works non-interactively.
- TCP `22`, `50051`, and `9090` are open.
- The relevant scheduled task is known.
- Widget/zone discovery succeeds against MCP `/mcp`.
- The gRPC client smoke succeeds against port `50051`.

## Strict Smoke and Soak Handoff

After the recovery checks pass, use the benchmark launch and soak procedure in
`docs/reports/windows_benchmark_config_launch_2026-05.md`. First generate the
bounded windowed live-metrics artifact described there with
`--benchmark-emit C:\tze_hud\perf\hud-nfl7n\windowed_live_metrics.json`.
From an interactive Windows shell with `TZE_HUD_PSK` already set to the
non-default HUD PSK:

```powershell
New-Item -ItemType Directory -Force C:\tze_hud\perf\hud-nfl7n | Out-Null
& C:\tze_hud\tze_hud.exe `
  --config C:\tze_hud\benchmark.toml `
  --window-mode overlay `
  --grpc-port 0 `
  --mcp-port 0 `
  --benchmark-emit C:\tze_hud\perf\hud-nfl7n\windowed_live_metrics.json `
  --benchmark-frames 600 `
  --benchmark-warmup-frames 120
```

Then run a short strict three-agent smoke with the same command shape as the
release soak, but a shorter duration and a dedicated output root:

```bash
python3 .claude/skills/user-test-performance/scripts/widget_soak_runner.py \
  --target-id user-test-windows-tailnet \
  --duration-s 60 \
  --rate-rps 1 \
  --windows-live-metrics-path 'C:\tze_hud\perf\hud-nfl7n\windowed_live_metrics.json' \
  --sample-windows-resources \
  --windows-process-command-match 'C:\tze_hud\benchmark.toml' \
  --ssh-identity ~/.ssh/hud-ssh-key \
  --output-root docs/reports/artifacts/hud-nfl7n-strict-smoke-<timestamp>
```

After the strict smoke passes, the release-gating soak must run the three-agent
`widget_soak_runner.py` path with:

- `--target-id user-test-windows-tailnet`
- `--duration-s 3600`
- `--rate-rps 1`
- `--windows-live-metrics-path 'C:\tze_hud\perf\hud-nfl7n\windowed_live_metrics.json'`
- `--sample-windows-resources`
- `--windows-process-command-match 'C:\tze_hud\benchmark.toml'`
- `--ssh-identity ~/.ssh/hud-ssh-key`

Do not use `--allow-missing-live-metrics` for release evidence. The strict smoke
or soak artifact must show `live_metrics.ok=true`, nonzero frame/input metrics,
`process_count >= 1` resource samples for the benchmark-config HUD process, idle
GPU evidence classified against the current `about/craft-and-care/engineering-bar.md`
ceiling, private-memory drift, transparent-overlay composite delta, and the
cleanup evidence required by `hud-nfl7n`.
