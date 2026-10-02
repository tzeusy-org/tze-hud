#!/usr/bin/env bash
# Resolve — and self-heal — the autonomous HUD testhost (hud-windows VM,
# vmid 110 on the Proxmox host). The canonical entry point for any local
# noninteractive session that needs a live GPU-projection surface: user-test
# deploys, hud-projection portal attaches, th-hud-publish zone publishes.
#
# Usage:
#   eval "$(.claude/skills/user-test/scripts/hud_vm_env.sh)"
#     -> exports TZE_HUD_TEST_HOST, HUD_MCP_URL, HUD_MCP_PSK, MCP_TEST_PSK,
#        TZE_HUD_PSK (all skills' expected env names)
#   hud_vm_env.sh --host-only   -> print the bare IP
#
# Self-heal ladder (each step only if needed):
#   1. VM stopped        -> qm start, wait for guest agent
#   2. stale gpu.lock    -> delete if it predates the current boot (hud-7gp40:
#                           PID reuse after reboot reads as "alive" and the
#                           HUD refuses startup until the lock is removed)
#   3. MCP port down     -> schtasks /Run TzeHudFullscreen, re-check
# Diagnostics go to stderr; only export lines (or the IP) go to stdout.
set -euo pipefail

# Proxmox host that fronts the VM. Never hardcode the real value here — this is
# a public repo (see AGENTS.md scrub convention). Resolution order: SENTINEL_HOST
# env var, then SENTINEL_HOST= in the private git-ignored env file, else fail fast.
VMID=110
HUD_SSH_KEY="${HUD_SSH_KEY:-$HOME/.ssh/hud-ssh-key}"
HOMELAB_ENV="${HOMELAB_ENV:-$HOME/gt/homelab/mayor/rig/.env}"
MCP_PORT=9090

SENTINEL="${SENTINEL_HOST:-}"
if [ -z "$SENTINEL" ] && [ -r "$HOMELAB_ENV" ]; then
  SENTINEL=$(grep '^SENTINEL_HOST=' "$HOMELAB_ENV" | head -1 | cut -d= -f2-)
fi
if [ -z "$SENTINEL" ]; then
  echo "hud_vm_env: ERROR — SENTINEL_HOST not set and not found in $HOMELAB_ENV." >&2
  echo "hud_vm_env: set SENTINEL_HOST=user@proxmox-host (real value lives in the private operator mapping, not this repo)." >&2
  exit 1
fi

ssh_sentinel() { ssh -o BatchMode=yes -o ConnectTimeout=8 "$SENTINEL" "$@"; }

vm_ip() {
  ssh_sentinel "qm agent $VMID network-get-interfaces" 2>/dev/null | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for iface in data:
    for addr in iface.get("ip-addresses", []):
        ip = addr.get("ip-address", "")
        if addr.get("ip-address-type") == "ipv4" and not ip.startswith(("127.", "169.254.")):
            print(ip)
            sys.exit(0)
sys.exit(1)'
}

# 1. Ensure the VM is running.
status=$(ssh_sentinel "qm status $VMID" | awk '{print $2}')
if [ "$status" != "running" ]; then
  echo "hud_vm_env: VM $VMID is '$status' — starting" >&2
  ssh_sentinel "qm start $VMID" >&2
fi

# 2. Wait for the guest agent and an IPv4 address (cold boot ~2-4 min on N150).
ip=""
for _ in $(seq 1 36); do
  if ip=$(vm_ip) && [ -n "$ip" ]; then break; fi
  sleep 10
done
if [ -z "$ip" ]; then
  echo "hud_vm_env: ERROR — no guest-agent IP after 6 min; check 'qm status $VMID' on the Proxmox host" >&2
  exit 1
fi

# 3. Ensure the HUD runtime is serving MCP; relaunch the task if not,
#    clearing a stale (pre-boot) gpu.lock first (hud-7gp40).
if ! timeout 4 bash -c "exec 3<>/dev/tcp/$ip/$MCP_PORT" 2>/dev/null; then
  echo "hud_vm_env: MCP $ip:$MCP_PORT down — clearing stale lock, launching TzeHudFullscreen" >&2
  ssh -i "$HUD_SSH_KEY" -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
    "admin-user@$ip" '$lock = "C:\ProgramData\tze_hud\gpu.lock"; if (Test-Path $lock) { $m = Select-String -Path $lock -Pattern "STARTED_AT=(.+)" | ForEach-Object { $_.Matches[0].Groups[1].Value }; $boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime; if ($m -and ([datetime]::Parse($m).ToUniversalTime() -lt $boot.ToUniversalTime())) { Remove-Item $lock -Force; "hud_vm_env: removed stale gpu.lock (STARTED_AT $m predates boot)" } }; schtasks /Run /TN TzeHudFullscreen' >&2 || true
  ok=""
  for _ in $(seq 1 12); do
    sleep 5
    if timeout 4 bash -c "exec 3<>/dev/tcp/$ip/$MCP_PORT" 2>/dev/null; then ok=1; break; fi
  done
  if [ -z "$ok" ]; then
    echo "hud_vm_env: ERROR — MCP still down after relaunch; check C:\\tze_hud\\task.log on the guest" >&2
    exit 1
  fi
fi

if [ "${1:-}" = "--host-only" ]; then
  echo "$ip"
  exit 0
fi

psk=$(grep '^HUD_WINDOWS_PSK=' "$HOMELAB_ENV" | head -1 | cut -d= -f2-)
if [ -z "$psk" ]; then
  echo "hud_vm_env: ERROR — HUD_WINDOWS_PSK not found in $HOMELAB_ENV" >&2
  exit 1
fi

echo "export TZE_HUD_TEST_HOST='$ip'"
echo "export HUD_MCP_URL='http://$ip:$MCP_PORT'"
echo "export HUD_MCP_PSK='$psk'"
echo "export MCP_TEST_PSK='$psk'"
echo "export TZE_HUD_PSK='$psk'"
