#!/usr/bin/env bash
set -euo pipefail

# deploy_portal_hud.sh — one deterministic command to deploy + launch the
# overlay (transparent) HUD on the Windows host and verify its ports.
#
# Consolidates the full flow:
#   1) preflight SSH key auth for BOTH users (file user + admin/desktop user)
#   2) checksum + size the local exe
#   3) kill stale HUD (scheduled tasks + processes), wait for gRPC port to close
#   4) scp the exe to the host, verify the remote sha256 matches local (fail loud)
#   5) launch via a Scheduled Task that runs the exe DIRECTLY in overlay mode
#      (no cmd/powershell wrapper, no stdout redirect — both break transparency),
#      then wait until BOTH gRPC + MCP ports are Listening; emit a JSON result
#   6) reachability gate: POST a JSON-RPC request to the MCP HTTP endpoint with
#      the Bearer PSK and confirm the server answers
#
# SECRET HANDLING: the PSK (== TZE_HUD_MCP_RESIDENT_PRINCIPAL) is NEVER passed on
# the Linux command line, never stored in this script, never logged. The launch
# helper reads it from the host's own environment; the reachability gate fetches
# it over SSH into a shell variable only for the duration of the curl call.

usage() {
  cat <<'EOF'
Usage:
  deploy_portal_hud.sh --local-exe <path> [options]

Deploys a freshly-built tze_hud.exe to the Windows host and launches it as a
transparent overlay, then verifies gRPC + MCP ports and MCP HTTP reachability.

Required:
  --local-exe <path>        Local path to the freshly-built tze_hud.exe

Options (placeholders default to the scrubbed values used across the skill;
provide real values via flags or env — see the private host doc):
  --win-host <host>         Windows host        (default: $WIN_HOST or windows-host.example)
  --file-user <user>        SSH user for SCP    (default: $WIN_FILE_USER or hud-user)
  --admin-user <user>       SSH user for launch (default: $WIN_ADMIN_USER or admin-user)
  --ssh-key <path>          SSH identity, same key for both users
                                                (default: $SSH_KEY or ~/.ssh/hud-ssh-key)
  --remote-dir <winpath>    Windows dir         (default: C:\tze_hud)
  --remote-exe <winpath>    Target exe path     (default: <remote-dir>\tze_hud.exe)
  --config <winpath>        --config value      (default: C:\tze_hud\tze_hud.toml)
  --task-name <name>        Scheduled task name (default: TzeHudPortalDeploy)
  --grpc-port <n>           gRPC port           (default: 50051)
  --mcp-port <n>            MCP HTTP port       (default: 9090)
  --no-verify               Skip the MCP HTTP reachability gate
  -h, --help                Show this help

Environment overrides:
  WIN_HOST, WIN_FILE_USER, WIN_ADMIN_USER, SSH_KEY

Example (substitute the real values from
docs/operations/private/tzehouse-windows.local.md — do NOT commit them):
  WIN_HOST=<host> WIN_FILE_USER=<file-user> WIN_ADMIN_USER=<admin-user> \
  SSH_KEY=<ssh-key-path> \
  deploy_portal_hud.sh --local-exe target/x86_64-pc-windows-gnu/release/tze_hud.exe
EOF
}

# ── Defaults (scrubbed placeholders; override via flags/env) ─────────────────
WIN_HOST="${WIN_HOST:-windows-host.example}"
WIN_FILE_USER="${WIN_FILE_USER:-hud-user}"
WIN_ADMIN_USER="${WIN_ADMIN_USER:-admin-user}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/hud-ssh-key}"
REMOTE_DIR_WIN='C:\tze_hud'
REMOTE_EXE_WIN=''
CONFIG_PATH='C:\tze_hud\tze_hud.toml'
TASK_NAME='TzeHudPortalDeploy'
GRPC_PORT=50051
MCP_PORT=9090
LOCAL_EXE=''
DO_VERIFY=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LAUNCH_PS1_LOCAL="${SCRIPT_DIR}/launch_portal_hud.ps1"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --local-exe)   LOCAL_EXE="${2:?}"; shift 2 ;;
    --win-host)    WIN_HOST="${2:?}"; shift 2 ;;
    --file-user)   WIN_FILE_USER="${2:?}"; shift 2 ;;
    --admin-user)  WIN_ADMIN_USER="${2:?}"; shift 2 ;;
    --ssh-key)     SSH_KEY="${2:?}"; shift 2 ;;
    --remote-dir)  REMOTE_DIR_WIN="${2:?}"; shift 2 ;;
    --remote-exe)  REMOTE_EXE_WIN="${2:?}"; shift 2 ;;
    --config)      CONFIG_PATH="${2:?}"; shift 2 ;;
    --task-name)   TASK_NAME="${2:?}"; shift 2 ;;
    --grpc-port)   GRPC_PORT="${2:?}"; shift 2 ;;
    --mcp-port)    MCP_PORT="${2:?}"; shift 2 ;;
    --no-verify)   DO_VERIFY=0; shift ;;
    -h|--help)     usage; exit 0 ;;
    *) echo "Unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$LOCAL_EXE" ]]; then
  echo "FAIL: --local-exe is required" >&2
  usage >&2
  exit 2
fi
if [[ ! -f "$LOCAL_EXE" ]]; then
  echo "FAIL: local exe not found: $LOCAL_EXE" >&2
  exit 3
fi
if [[ ! -f "$LAUNCH_PS1_LOCAL" ]]; then
  echo "FAIL: launch helper not found: $LAUNCH_PS1_LOCAL" >&2
  exit 3
fi

[[ -n "$REMOTE_EXE_WIN" ]] || REMOTE_EXE_WIN="${REMOTE_DIR_WIN}\\tze_hud.exe"
# OpenSSH on Windows wants C:/path for scp targets (no leading slash).
REMOTE_DIR_SCP="${REMOTE_DIR_WIN//\\//}"
REMOTE_EXE_SCP="${REMOTE_EXE_WIN//\\//}"
REMOTE_PS1_WIN="${REMOTE_DIR_WIN}\\launch_portal_hud.ps1"
REMOTE_PS1_SCP="${REMOTE_DIR_SCP}/launch_portal_hud.ps1"

SSH_OPTS=(-o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new -i "$SSH_KEY")

ssh_file()  { ssh "${SSH_OPTS[@]}" "${WIN_FILE_USER}@${WIN_HOST}" "$@"; }
ssh_admin() { ssh "${SSH_OPTS[@]}" "${WIN_ADMIN_USER}@${WIN_HOST}" "$@"; }

# Run a PowerShell payload as the admin user. Base64/UTF-16LE -EncodedCommand
# sidesteps SSH+cmd+PowerShell quoting; stderr is dropped (CLIXML progress noise).
run_admin_ps() {
  local encoded
  encoded="$(printf '%s' "$1" | iconv -f UTF-8 -t UTF-16LE | base64 -w0)"
  ssh "${SSH_OPTS[@]}" "${WIN_ADMIN_USER}@${WIN_HOST}" \
    "powershell -NoProfile -NonInteractive -EncodedCommand ${encoded}" 2>/dev/null
}

cr() { tr -d '\r'; }

echo "=== Portal HUD deploy: ${WIN_HOST} ==="
echo "  file-user=${WIN_FILE_USER}  admin-user=${WIN_ADMIN_USER}  task=${TASK_NAME}"
echo "  remote-exe=${REMOTE_EXE_WIN}  config=${CONFIG_PATH}  grpc=${GRPC_PORT}  mcp=${MCP_PORT}"
echo

# ── 1. Preflight: SSH key auth for BOTH users ────────────────────────────────
echo "[1/6] Preflight SSH auth (both users)"
if ! who_file="$(ssh_file whoami 2>&1 | cr)"; then
  echo "  FAIL: SSH as file user ${WIN_FILE_USER}@${WIN_HOST}: $who_file" >&2; exit 1
fi
echo "  file-user OK: $who_file"
if ! who_admin="$(ssh_admin whoami 2>&1 | cr)"; then
  echo "  FAIL: SSH as admin user ${WIN_ADMIN_USER}@${WIN_HOST}: $who_admin" >&2; exit 1
fi
echo "  admin-user OK: $who_admin"

# ── 2. Local checksum + size ─────────────────────────────────────────────────
echo "[2/6] Local exe checksum + size"
LOCAL_SHA="$(sha256sum "$LOCAL_EXE" | awk '{print $1}')"
LOCAL_SIZE="$(stat -c %s "$LOCAL_EXE" 2>/dev/null || wc -c < "$LOCAL_EXE")"
echo "  path:   $LOCAL_EXE"
echo "  sha256: $LOCAL_SHA"
echo "  size:   ${LOCAL_SIZE} bytes"

# ── 3. Kill stale instance (admin), wait for gRPC port to close ──────────────
echo "[3/6] Stopping stale HUD (scheduled tasks + processes)"
stop_out="$(run_admin_ps "
  \$ProgressPreference='SilentlyContinue'; \$ErrorActionPreference='SilentlyContinue'
  foreach (\$t in @('TzeHudOverlay','TzeHudPortalVal','TzeHud8dht5Media','${TASK_NAME}')) {
    Stop-ScheduledTask -TaskName \$t -ErrorAction SilentlyContinue
  }
  Get-Process tze_hud -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
  \$dl=(Get-Date).AddSeconds(20)
  do { \$c=Get-NetTCPConnection -State Listen -LocalPort ${GRPC_PORT} -ErrorAction SilentlyContinue
       if(-not \$c){ Write-Output 'grpc_closed=true'; break }
       Start-Sleep -Milliseconds 500 } while((Get-Date) -lt \$dl)
  if (Get-NetTCPConnection -State Listen -LocalPort ${GRPC_PORT} -ErrorAction SilentlyContinue) { Write-Output 'grpc_closed=false' }
" | cr)"
echo "  $stop_out"
if [[ "$stop_out" != *"grpc_closed=true"* ]]; then
  echo "  FAIL: gRPC port ${GRPC_PORT} did not close after stopping HUD" >&2
  exit 4
fi

# ── 4. Deploy exe (scp via file user) + verify remote sha256 ─────────────────
echo "[4/6] Deploy exe + verify remote checksum"
ssh_file "powershell -NoProfile -Command \"New-Item -Path '${REMOTE_DIR_WIN}' -ItemType Directory -Force | Out-Null\"" >/dev/null
scp "${SSH_OPTS[@]}" "$LOCAL_EXE" "${WIN_FILE_USER}@${WIN_HOST}:${REMOTE_EXE_SCP}"
REMOTE_SHA="$(run_admin_ps "(Get-FileHash -Algorithm SHA256 -Path '${REMOTE_EXE_WIN}').Hash" | cr | tr '[:upper:]' '[:lower:]')"
echo "  remote sha256: $REMOTE_SHA"
if [[ "$REMOTE_SHA" != "$LOCAL_SHA" ]]; then
  echo "  FAIL: remote checksum mismatch (local=$LOCAL_SHA remote=$REMOTE_SHA)" >&2
  exit 5
fi
echo "  checksum OK (local == remote)"

# ── 5. Launch overlay via scheduled task (exe-direct), wait for both ports ───
echo "[5/6] Launch overlay + wait for ports"
# Copy the launch helper to the host (via file user), then run it as admin.
scp "${SSH_OPTS[@]}" "$LAUNCH_PS1_LOCAL" "${WIN_FILE_USER}@${WIN_HOST}:${REMOTE_PS1_SCP}"
LAUNCH_JSON="$(run_admin_ps "
  & '${REMOTE_PS1_WIN}' \
    -TaskName '${TASK_NAME}' \
    -GrpcPort ${GRPC_PORT} \
    -McpPort ${MCP_PORT} \
    -ConfigPath '${CONFIG_PATH}' \
    -ExePath '${REMOTE_EXE_WIN}' \
    -WorkingDir '${REMOTE_DIR_WIN}'
" | cr)"
echo "  launch result: $LAUNCH_JSON"

# Parse the JSON minimally (no jq dependency assumed).
BOUND="$(printf '%s' "$LAUNCH_JSON" | grep -o '"bound":[a-z]*' | head -1 | cut -d: -f2)"
LAUNCH_PID="$(printf '%s' "$LAUNCH_JSON" | grep -o '"pid":[0-9]*' | head -1 | cut -d: -f2)"
if [[ "$BOUND" != "true" ]]; then
  echo "  FAIL: ports did not bind after launch (see launch result above)" >&2
  exit 6
fi
echo "  ports bound OK (gRPC ${GRPC_PORT} + MCP ${MCP_PORT} Listening, pid=${LAUNCH_PID:-?})"

# ── 6. MCP HTTP reachability gate ────────────────────────────────────────────
if [[ "$DO_VERIFY" -eq 1 ]]; then
  echo "[6/6] MCP HTTP reachability gate"
  # Fetch the PSK over SSH into a local var ONLY for this curl call. The launch
  # helper paired its SHA-256 in agents.toml; the PSK itself is the Bearer token,
  # read from the admin user's TZE_HUD_MCP_RESIDENT_PRINCIPAL. Never echoed,
  # never persisted.
  PSK="$(run_admin_ps "
    \$p=[Environment]::GetEnvironmentVariable('TZE_HUD_MCP_RESIDENT_PRINCIPAL','User')
    if([string]::IsNullOrEmpty(\$p)){ \$p=\$env:TZE_HUD_MCP_RESIDENT_PRINCIPAL }
    Write-Output \$p
  " | cr)"
  if [[ -z "$PSK" ]]; then
    echo "  FAIL: could not read PSK from host env for reachability check" >&2
    exit 7
  fi
  # NOTE: this runtime is NOT a full MCP server — it does not implement the
  # standard 'initialize' method, and (as of this build) not even 'tools/list':
  # both return '-32601 Method not found'. That reply still PROVES reachability +
  # auth: the server parsed our bearer token and answered a JSON-RPC envelope
  # echoing our id. The gate therefore treats ANY JSON-RPC body carrying our id
  # (or "jsonrpc") as "server answered"; an HTTP/auth failure (no JSON-RPC body)
  # fails the gate. We send tools/list as the probe method.
  MCP_URL="http://${WIN_HOST}:${MCP_PORT}/mcp"
  REQ='{"jsonrpc":"2.0","id":"deploy-gate","method":"tools/list","params":{}}'
  RESP="$(curl -fsS --max-time 15 \
            -H 'Content-Type: application/json' \
            -H "Authorization: Bearer ${PSK}" \
            -d "$REQ" "$MCP_URL" 2>&1 || true)"
  PSK=""  # scrub
  if [[ "$RESP" == *'"id":"deploy-gate"'* || "$RESP" == *'"jsonrpc"'* ]]; then
    echo "  reachable OK: server answered JSON-RPC at ${MCP_URL}"
    echo "  response: $RESP"
  else
    echo "  FAIL: MCP HTTP did not answer with JSON-RPC at ${MCP_URL}" >&2
    echo "  response: $RESP" >&2
    exit 7
  fi
else
  echo "[6/6] MCP HTTP reachability gate SKIPPED (--no-verify)"
fi

echo
echo "=== DEPLOY OK ==="
echo "{\"task\":\"${TASK_NAME}\",\"grpc_port\":${GRPC_PORT},\"mcp_port\":${MCP_PORT},\"pid\":${LAUNCH_PID:-null},\"checksum_ok\":true,\"bound\":true}"
