#!/usr/bin/env bash
# Resolve — and self-heal — the tzehouse Windows target (the human-in-loop
# HUD host). The tzehouse analogue of hud_vm_env.sh: the canonical entry point
# for any session that needs the live operator-facing HUD surface — user-test
# deploys, hud-projection portal attaches, th-hud-publish zone publishes.
#
# Usage:
#   eval "$(.claude/skills/user-test/scripts/tzehouse_env.sh)"
#     -> exports TZE_HUD_TEST_HOST, WIN_HOST, WIN_FILE_USER, WIN_ADMIN_USER,
#        HUD_SSH_KEY, HUD_MCP_URL, HUD_GRPC_TARGET, HUD_PSK, HUD_MCP_PSK,
#        MCP_TEST_PSK, TZE_HUD_PSK
#   tzehouse_env.sh --host-only   -> print the bare hostname (no healing/PSK)
#
# Identity comes from the git-ignored env file next to this skill
# (.claude/skills/user-test/target.env — see .gitignore); the PSK is recovered
# live from the TzeHudOverlay task definition, so no secret ever lives in a
# tracked file. Placeholders in tracked docs: windows-host.example / hud-user /
# admin-user (AGENTS.md scrub convention). Never inline real values here.
#
# Self-heal ladder (each step only if needed):
#   1. no tze_hud.exe running                  -> schtasks /Run TzeHudOverlay
#   2. instance up but MCP not on 0.0.0.0:9090 -> kill + relaunch via the task
#      (catches leftover ad-hoc launches, e.g. a benchmark.toml instance bound
#       to loopback only; observed 2026-07-12)
#   3. wait for 0.0.0.0 bind, then verify MCP HTTP `initialize` answers 200
# Diagnostics go to stderr; only export lines (or the hostname) go to stdout.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
TARGET_ENV="${TZEHOUSE_TARGET_ENV:-$SCRIPT_DIR/../target.env}"
MCP_PORT=9090
GRPC_PORT=50051
TASK_NAME="${TZEHOUSE_TASK_NAME:-TzeHudOverlay}"

# --- 0. Resolve identity: env vars win, then the private target.env. ---------
if [ -r "$TARGET_ENV" ]; then
  # shellcheck disable=SC1090
  . "$TARGET_ENV"
fi
HOST="${TZEHOUSE_HOST:-}"
FILE_USER="${TZEHOUSE_FILE_USER:-}"
ADMIN_USER="${TZEHOUSE_ADMIN_USER:-}"
KEY="${TZEHOUSE_SSH_KEY:-}"
KEY="${KEY/#\~/$HOME}"
if [ -z "$HOST" ] || [ -z "$FILE_USER" ] || [ -z "$ADMIN_USER" ] || [ -z "$KEY" ]; then
  echo "tzehouse_env: ERROR — target identity unresolved." >&2
  echo "tzehouse_env: populate $TARGET_ENV with TZEHOUSE_HOST / TZEHOUSE_FILE_USER / TZEHOUSE_ADMIN_USER / TZEHOUSE_SSH_KEY" >&2
  echo "tzehouse_env: (real values: docs/operations/private/tzehouse-windows.local.md — both are git-ignored)" >&2
  exit 1
fi

SSH_OPTS=(-o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 -i "$KEY")
ssh_admin() { ssh "${SSH_OPTS[@]}" "$ADMIN_USER@$HOST" "$@"; }
ssh_file()  { ssh "${SSH_OPTS[@]}" "$FILE_USER@$HOST" "$@"; }

if [ "${1:-}" = "--host-only" ]; then
  echo "$HOST"
  exit 0
fi

# --- 1. SSH connectivity gate (both roles). ----------------------------------
who_admin=$(ssh_admin "whoami" 2>/dev/null) || {
  echo "tzehouse_env: ERROR — admin SSH gate failed ($ADMIN_USER@$HOST with $KEY)" >&2; exit 1; }
who_file=$(ssh_file "whoami" 2>/dev/null) || {
  echo "tzehouse_env: ERROR — file-user SSH gate failed ($FILE_USER@$HOST with $KEY)" >&2; exit 1; }
echo "tzehouse_env: SSH gates OK ($who_admin, $who_file)" >&2

# --- 2. HUD instance health: MCP must be bound on 0.0.0.0 (not loopback). ----
mcp_bind() {
  # prints the local address netstat shows for the MCP listener, if any
  ssh_admin "netstat -ano | findstr LISTENING | findstr :$MCP_PORT" 2>/dev/null \
    | awk '{print $2}' | grep ":$MCP_PORT\$" | head -1 || true
}
bind_addr=$(mcp_bind)
if [ "$bind_addr" != "0.0.0.0:$MCP_PORT" ]; then
  if [ -n "$bind_addr" ]; then
    echo "tzehouse_env: MCP bound on $bind_addr (not 0.0.0.0) — leftover ad-hoc instance; kill + relaunch via $TASK_NAME" >&2
  else
    echo "tzehouse_env: no MCP listener on :$MCP_PORT — launching $TASK_NAME" >&2
  fi
  ssh_admin "taskkill /F /IM tze_hud.exe" >&2 2>&1 || true
  sleep 2
  ssh_admin "schtasks /Run /TN $TASK_NAME" >&2
  ok=""
  for _ in $(seq 1 12); do
    sleep 5
    bind_addr=$(mcp_bind)
    if [ "$bind_addr" = "0.0.0.0:$MCP_PORT" ]; then ok=1; break; fi
  done
  if [ -z "$ok" ]; then
    echo "tzehouse_env: ERROR — MCP not on 0.0.0.0:$MCP_PORT after relaunch (last bind: ${bind_addr:-none}); check the $TASK_NAME task definition on $HOST" >&2
    exit 1
  fi
fi

# --- 3. Recover the PSK live from the scheduled task definition. -------------
psk=$(ssh_admin "schtasks /Query /TN $TASK_NAME /XML" 2>/dev/null \
  | tr -d '\r' | grep -o '<Arguments>[^<]*</Arguments>' \
  | sed -n 's/.*--psk \([^ <]*\).*/\1/p' | head -1)
if [ -z "$psk" ]; then
  echo "tzehouse_env: ERROR — could not recover --psk from $TASK_NAME task XML" >&2
  exit 1
fi

# --- 4. MCP HTTP reachability gate (initialize must answer). -----------------
code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' -X POST "http://$HOST:$MCP_PORT/mcp" \
  -H "Authorization: Bearer $psk" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tzehouse-env-gate","version":"0.1"}}}' || true)
if [ "$code" != "200" ]; then
  echo "tzehouse_env: ERROR — MCP initialize probe returned '$code' (expected 200) at http://$HOST:$MCP_PORT/mcp" >&2
  exit 1
fi
echo "tzehouse_env: MCP gate OK (initialize 200)" >&2

echo "export TZE_HUD_TEST_HOST='$HOST'"
echo "export WIN_HOST='$HOST'"
echo "export WIN_FILE_USER='$FILE_USER'"
echo "export WIN_ADMIN_USER='$ADMIN_USER'"
echo "export HUD_SSH_KEY='$KEY'"
echo "export HUD_MCP_URL='http://$HOST:$MCP_PORT'"
echo "export HUD_GRPC_TARGET='$HOST:$GRPC_PORT'"
echo "export HUD_PSK='$psk'"
echo "export HUD_MCP_PSK='$psk'"
echo "export MCP_TEST_PSK='$psk'"
echo "export TZE_HUD_PSK='$psk'"
