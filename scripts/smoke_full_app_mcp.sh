#!/usr/bin/env bash
# smoke_full_app_mcp.sh — End-to-end smoke flow: cross-build tze_hud for Windows, deploy, launch,
# verify MCP reachability, and publish a sample zone message.
#
# This script proves the full pipeline:
#   1. Linux cross-build of the canonical tze_hud Windows executable
#   2. Deploy + launch on Windows over SSH/SCP
#   3. MCP HTTP reachability gate (hard failure before publish)
#   4. Live hud_publish smoke with structured result output
#
# Exit codes:
#   0   full smoke passed
#   1   SSH connectivity check failed
#   2   build failed
#   3   deploy/launch failed
#   4   MCP endpoint unreachable after launch
#   5   MCP publish failed (auth error, RPC error, or wrong response)
#   6   MCP auth error (endpoint reachable but PSK rejected)
#   9   bad arguments
#
# Usage:
#   scripts/smoke_full_app_mcp.sh [OPTIONS]
#
# Required environment:
#   TZE_HUD_PSK   Pre-shared key for MCP authentication (same value used at runtime)
#                 Set via --psk flag or TZE_HUD_PSK env var.
#
# Options:
#   --win-host <host>      Windows SSH/SCP hostname          (default: windows-host.example)
#   --win-user <user>      Windows SSH user                  (default: hud-user)
#   --ssh-key <path>       SSH identity key                  (default: ~/.ssh/hud-ssh-key)
#   --full-app-exe <path>  Prebuilt Windows .exe to deploy   (build if omitted)
#   --target <triple>      Rust cross-target                 (default: x86_64-pc-windows-gnu)
#   --profile <name>       Cargo profile: release|debug      (default: release)
#   --mcp-host <host>      Host for MCP URL (default: same as --win-host)
#   --mcp-port <port>      MCP HTTP port                     (default: 9090)
#   --psk <key>            MCP pre-shared key                (overrides TZE_HUD_PSK)
#   --mcp-wait <secs>      Seconds to wait for MCP to come up (default: 15)
#   --mcp-retries <n>      Number of MCP reachability retries (default: 5)
#   --launch-mode <mode>   HUD launch mode: auto|task|direct  (default: auto)
#   --zone-name <name>     Zone to publish to                (default: status-bar)
#   --no-build             Skip build step; --full-app-exe required
#   --no-deploy            Skip deploy; only run MCP gate + publish against running instance
#   --no-publish           Stop after MCP reachability check (skip publish)
#   --skip-ssh-check       Skip initial SSH connectivity gate
#   -h, --help             Show this help

set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────

WIN_HOST="windows-host.example"
WIN_USER="${WIN_USER:-hud-user}"
SSH_KEY="${SSH_KEY:-${HOME}/.ssh/hud-ssh-key}"
FULL_APP_EXE="${FULL_APP_EXE:-}"
TARGET="x86_64-pc-windows-gnu"
PROFILE="release"
MCP_HOST=""
MCP_PORT="9090"
PSK="${TZE_HUD_PSK:-}"
MCP_WAIT=15
MCP_RETRIES=5
LAUNCH_MODE="auto"
ZONE_NAME="status-bar"
NO_BUILD=0
NO_DEPLOY=0
NO_PUBLISH=0
SKIP_SSH_CHECK=0

# ── Argument parsing ──────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
  case "$1" in
    --win-host)       WIN_HOST="${2:?--win-host requires a value}";      shift 2 ;;
    --win-user)       WIN_USER="${2:?--win-user requires a value}";      shift 2 ;;
    --ssh-key)        SSH_KEY="${2:?--ssh-key requires a value}";        shift 2 ;;
    --full-app-exe)   FULL_APP_EXE="${2:?--full-app-exe requires a path}"; shift 2 ;;
    --target)         TARGET="${2:?--target requires a value}";          shift 2 ;;
    --profile)        PROFILE="${2:?--profile requires a value}";        shift 2 ;;
    --mcp-host)       MCP_HOST="${2:?--mcp-host requires a value}";      shift 2 ;;
    --mcp-port)       MCP_PORT="${2:?--mcp-port requires a value}";      shift 2 ;;
    --psk)            PSK="${2:?--psk requires a value}";                shift 2 ;;
    --mcp-wait)       MCP_WAIT="${2:?--mcp-wait requires a value}";      shift 2 ;;
    --mcp-retries)    MCP_RETRIES="${2:?--mcp-retries requires a value}"; shift 2 ;;
    --launch-mode)    LAUNCH_MODE="${2:?--launch-mode requires a value}"; shift 2 ;;
    --zone-name)      ZONE_NAME="${2:?--zone-name requires a value}";    shift 2 ;;
    --no-build)       NO_BUILD=1;      shift ;;
    --no-deploy)      NO_DEPLOY=1;     shift ;;
    --no-publish)     NO_PUBLISH=1;    shift ;;
    --skip-ssh-check) SKIP_SSH_CHECK=1; shift ;;
    -h|--help)
      sed -n '3,/^set -euo/p' "$0" | grep '^#' | sed 's/^# \?//'
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 9
      ;;
  esac
done

# Resolve MCP URL host — defaults to win-host if not overridden.
MCP_HOST="${MCP_HOST:-${WIN_HOST}}"
MCP_URL="http://${MCP_HOST}:${MCP_PORT}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEPLOY_SCRIPT="${REPO_ROOT}/.claude/skills/user-test/scripts/deploy_windows_hud.sh"
MCP_REACH_SCRIPT="${SCRIPT_DIR}/mcp_reachability_check.py"
MCP_PUBLISH_SCRIPT="${REPO_ROOT}/.claude/skills/user-test/scripts/publish_zone_batch.py"

# SSH options for non-interactive automation.
SSH_OPTS="-i ${SSH_KEY} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

# ── Validation ────────────────────────────────────────────────────────────────

if [[ -z "$PSK" ]]; then
  echo "ERROR: MCP PSK is required." >&2
  echo "  Set TZE_HUD_PSK env var or pass --psk <key>." >&2
  exit 9
fi

if [[ "$NO_BUILD" -eq 1 && -z "$FULL_APP_EXE" ]]; then
  echo "ERROR: --no-build requires --full-app-exe <path>." >&2
  exit 9
fi

if [[ "$PROFILE" != "release" && "$PROFILE" != "debug" ]]; then
  echo "ERROR: --profile must be 'release' or 'debug'." >&2
  exit 9
fi

export TZE_HUD_PSK="$PSK"

# ── Logging helpers ───────────────────────────────────────────────────────────

step() { echo; echo "══ $* ══"; }
ok()   { echo "  [OK] $*"; }
fail() { echo "  [FAIL] $*" >&2; }
info() { echo "  [info] $*"; }

# ── Step 0: SSH connectivity gate ─────────────────────────────────────────────

if [[ "$SKIP_SSH_CHECK" -eq 0 && "$NO_DEPLOY" -eq 0 ]]; then
  step "0/4  SSH connectivity gate"
  info "verifying key auth for ${WIN_USER}@${WIN_HOST}"
  if ! ssh ${SSH_OPTS} "${WIN_USER}@${WIN_HOST}" "whoami" >/dev/null 2>&1; then
    fail "SSH connection to ${WIN_USER}@${WIN_HOST} failed."
    echo >&2
    echo "  Troubleshooting:" >&2
    echo "    - Verify key: ssh-keygen -l -f ${SSH_KEY}" >&2
    echo "    - Verify authorized_keys on Windows:" >&2
    echo "      C:\\Users\\${WIN_USER}\\.ssh\\authorized_keys must contain the public key." >&2
    echo "    - Verify key ownership on Windows (owner must be ${WIN_USER}, SYSTEM, Administrators only)." >&2
    echo "    - Verify sshd is running: sc query sshd" >&2
    exit 1
  fi
  ok "SSH connectivity to ${WIN_USER}@${WIN_HOST}"
fi

# ── Step 1: Build ─────────────────────────────────────────────────────────────

if [[ "$NO_BUILD" -eq 0 && "$NO_DEPLOY" -eq 0 ]]; then
  step "1/4  Cross-build tze_hud for Windows"

  if [[ -n "$FULL_APP_EXE" ]]; then
    info "using prebuilt exe: ${FULL_APP_EXE}"
    if [[ ! -f "$FULL_APP_EXE" ]]; then
      fail "prebuilt exe not found: ${FULL_APP_EXE}"
      exit 2
    fi
    EXE_PATH="$FULL_APP_EXE"
  else
    info "building tze_hud (${PROFILE}) for ${TARGET}..."
    EXE_PATH="$(bash "${SCRIPT_DIR}/build_windows_app.sh" \
      --target "${TARGET}" \
      --profile "${PROFILE}")"
    if [[ -z "$EXE_PATH" || ! -f "$EXE_PATH" ]]; then
      fail "build_windows_app.sh did not produce an exe."
      exit 2
    fi
  fi

  ok "artifact: ${EXE_PATH}"
  info "type:     $(file "${EXE_PATH}" 2>/dev/null || echo 'unknown')"
  info "sha256:   $(sha256sum "${EXE_PATH}" | awk '{print $1}')"
  info "size:     $(du -sh "${EXE_PATH}" | cut -f1)"

elif [[ "$NO_DEPLOY" -eq 0 ]]; then
  # NO_BUILD=1 with explicit exe
  EXE_PATH="$FULL_APP_EXE"
  step "1/4  Build skipped (--no-build); using: ${EXE_PATH}"
  ok "artifact: ${EXE_PATH}"
fi

# ── Step 2: Deploy + launch ───────────────────────────────────────────────────

if [[ "$NO_DEPLOY" -eq 0 ]]; then
  step "2/4  Deploy + launch on Windows"
  info "deploy script: ${DEPLOY_SCRIPT}"
  info "remote host:   ${WIN_USER}@${WIN_HOST}"
  info "launch mode:   ${LAUNCH_MODE}"

  if [[ ! -x "$DEPLOY_SCRIPT" ]]; then
    fail "deploy script not found or not executable: ${DEPLOY_SCRIPT}"
    exit 3
  fi

  DEPLOY_EXIT=0
  WIN_USER="${WIN_USER}" \
  SSH_OPTS="${SSH_OPTS}" \
  bash "${DEPLOY_SCRIPT}" \
    --win-host "${WIN_HOST}" \
    --full-app-exe "${EXE_PATH}" \
    --launch-mode "${LAUNCH_MODE}" \
    || DEPLOY_EXIT=$?

  if [[ "$DEPLOY_EXIT" -ne 0 ]]; then
    fail "deploy_windows_hud.sh exited with code ${DEPLOY_EXIT}"
    echo >&2
    echo "  Failure modes:" >&2
    echo "    exit 3 — local exe not found" >&2
    echo "    exit 4 — setup script missing" >&2
    echo "    exit 5 — task trigger failed and bootstrap disabled" >&2
    exit 3
  fi

  ok "deploy complete; waiting ${MCP_WAIT}s for runtime to start..."
  sleep "${MCP_WAIT}"
else
  step "2/4  Deploy skipped (--no-deploy)"
  info "assuming runtime is already running at ${MCP_URL}"
fi

# ── Step 3: MCP reachability gate ─────────────────────────────────────────────

step "3/4  MCP reachability gate"
info "url:     ${MCP_URL}"
info "retries: ${MCP_RETRIES}"

REACH_EXIT=0
ATTEMPT=0
while [[ $ATTEMPT -lt $MCP_RETRIES ]]; do
  ATTEMPT=$((ATTEMPT + 1))
  info "attempt ${ATTEMPT}/${MCP_RETRIES}..."

  REACH_RESULT=0
  MCP_JSON="$(TZE_HUD_PSK="${PSK}" \
    python3 "${MCP_REACH_SCRIPT}" \
      --url "${MCP_URL}" \
      --psk-env TZE_HUD_PSK \
      --timeout 8 \
      --json 2>/dev/null)" || REACH_RESULT=$?

  case "$REACH_RESULT" in
    0)
      REACH_EXIT=0
      break
      ;;
    1)
      # Auth error — endpoint is up but PSK wrong; no point retrying.
      echo "  [FAIL] MCP endpoint reached but authentication was rejected." >&2
      echo "  JSON: ${MCP_JSON}" >&2
      echo >&2
      echo "  Failure mode: auth_error" >&2
      echo "  Check that TZE_HUD_PSK matches the --psk value used to start tze_hud." >&2
      echo "  Default PSK at launch is 'tze-hud-key' unless overridden by --psk or TZE_HUD_PSK env var." >&2
      exit 6
      ;;
    2)
      info "endpoint not reachable yet (connection_error)"
      REACH_EXIT=4
      if [[ $ATTEMPT -lt $MCP_RETRIES ]]; then
        sleep 5
      fi
      ;;
    3)
      info "unexpected response from endpoint"
      REACH_EXIT=4
      if [[ $ATTEMPT -lt $MCP_RETRIES ]]; then
        sleep 3
      fi
      ;;
    4)
      fail "PSK env var not set — internal error"
      exit 9
      ;;
    *)
      info "unknown exit code ${REACH_RESULT} from reachability check"
      REACH_EXIT=4
      ;;
  esac
done

if [[ "$REACH_EXIT" -ne 0 ]]; then
  fail "MCP HTTP endpoint not reachable after ${MCP_RETRIES} attempts: ${MCP_URL}"
  echo >&2
  echo "  Failure mode: endpoint_unreachable" >&2
  echo "  The runtime may not have started correctly, or the port is blocked." >&2
  echo "  Checks to perform:" >&2
  echo "    1. Verify tze_hud.exe is running on Windows:" >&2
  echo "       ssh ${WIN_USER}@${WIN_HOST} \"Get-Process tze_hud -ErrorAction SilentlyContinue\"" >&2
  echo "    2. Verify the MCP port is open:" >&2
  echo "       ssh ${WIN_USER}@${WIN_HOST} \"netstat -an | findstr :${MCP_PORT}\"" >&2
  echo "    3. Check launcher log:" >&2
  echo "       ssh ${WIN_USER}@${WIN_HOST} \"type C:\\tze_hud\\logs\\hud.launcher.log\"" >&2
  echo "    4. Check runtime stderr:" >&2
  echo "       ssh ${WIN_USER}@${WIN_HOST} \"type C:\\tze_hud\\logs\\hud.stderr.log\"" >&2
  echo "    5. Confirm --mcp-port matches (default is 9090, not 8765)." >&2
  echo "       Verify: tze_hud --mcp-port ${MCP_PORT} is what was launched." >&2
  exit 4
fi

# Parse zone count from JSON result for informational output.
ZONE_COUNT="?"
if command -v python3 >/dev/null 2>&1; then
  ZONE_COUNT="$(echo "${MCP_JSON}" | python3 -c '
import json, sys
d = json.load(sys.stdin)
z = d.get("zones")
if isinstance(z, list):
    print(len(z))
else:
    print("?")
' 2>/dev/null || echo "?")"
fi

ok "MCP endpoint reachable at ${MCP_URL} (${ZONE_COUNT} zones reported)"

# ── Step 4: Publish smoke ─────────────────────────────────────────────────────

if [[ "$NO_PUBLISH" -eq 1 ]]; then
  step "4/4  Publish skipped (--no-publish)"
  ok "smoke run complete (reachability gate passed)"
  exit 0
fi

step "4/4  MCP hud_publish smoke"
info "zone:    ${ZONE_NAME}"
info "url:     ${MCP_URL}"

# Build messages file.
MESSAGES_FILE="$(mktemp /tmp/hud-smoke-messages.XXXXXX.json)"
trap 'rm -f "${MESSAGES_FILE}"' EXIT

TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
cat > "${MESSAGES_FILE}" <<EOF
[
  {
    "zone": "${ZONE_NAME}",
    "content": {"entries": {"smoke": "MCP publish ok at ${TIMESTAMP}"}},
    "key": "smoke-status",
    "ttl_ms": 60000
  }
]
EOF

info "messages file: ${MESSAGES_FILE}"

PUBLISH_EXIT=0
PUBLISH_OUTPUT="$(TZE_HUD_PSK="${PSK}" \
  python3 "${MCP_PUBLISH_SCRIPT}" \
    --url "${MCP_URL}" \
    --psk-env TZE_HUD_PSK \
    --messages-file "${MESSAGES_FILE}" \
    --list-surfaces \
    2>&1)" || PUBLISH_EXIT=$?

echo
echo "── publish output ──────────────────────────────────────────────────────────"
echo "${PUBLISH_OUTPUT}"
echo "────────────────────────────────────────────────────────────────────────────"
echo

if [[ "$PUBLISH_EXIT" -ne 0 ]]; then
  fail "publish_zone_batch.py exited with code ${PUBLISH_EXIT}"
  echo >&2
  echo "  Failure modes by exit code:" >&2
  echo "    2 — PSK env var not set" >&2
  echo "    3 — HTTP error (4xx/5xx from endpoint)" >&2
  echo "    4 — URL error (connection refused, timeout)" >&2
  echo "    5 — exception in publish script" >&2
  echo >&2
  echo "  Full publish output above contains the server response payload." >&2
  exit 5
fi

# Validate the response contains a success result for our message.
PUBLISH_OK=0
if command -v python3 >/dev/null 2>&1; then
  PUBLISH_OK="$(echo "${PUBLISH_OUTPUT}" | python3 -c '
import json, sys

text = sys.stdin.read()
# Find the last JSON object in the output (hud_surfaces may precede it).
# published_zone_batch outputs one JSON line for hud_surfaces and one for published.
lines = [l.strip() for l in text.splitlines() if l.strip().startswith("{")]
result = None
for line in reversed(lines):
    try:
        obj = json.loads(line)
        if "published" in obj:
            result = obj
            break
    except Exception:
        continue

if result is None:
    print("no_published_key")
    sys.exit(0)

published = result["published"]
if not published:
    print("empty_published")
    sys.exit(0)

for entry in published:
    resp = entry.get("response", {})
    if "result" in resp:
        print("ok")
    elif "error" in resp:
        err = resp["error"]
        msg = err.get("message", str(err)) if isinstance(err, dict) else str(err)
        print(f"rpc_error:{msg}")
    else:
        print("unknown_response")
' 2>/dev/null || echo "parse_failed")"
fi

case "${PUBLISH_OK}" in
  ok)
    ok "publish_to_zone succeeded"
    ;;
  rpc_error:*)
    RPC_MSG="${PUBLISH_OK#rpc_error:}"
    fail "publish_to_zone returned a JSON-RPC error: ${RPC_MSG}"
    echo >&2
    echo "  This may indicate:" >&2
    echo "    - Zone '${ZONE_NAME}' is not registered in the runtime scene." >&2
    echo "    - The runtime started but scene initialization is incomplete." >&2
    echo "    - Check the full publish output above for the server response." >&2
    exit 5
    ;;
  no_published_key|empty_published|unknown_response|parse_failed)
    fail "could not verify publish success from response (${PUBLISH_OK})"
    echo "  Full response is shown above. Manual inspection required." >&2
    exit 5
    ;;
  *)
    info "publish result: ${PUBLISH_OK} (unrecognized — treating as success if publish_zone_batch.py exited 0)"
    ok "publish_zone_batch.py reported success (exit 0)"
    ;;
esac

echo
echo "══ Smoke complete ══"
echo "  MCP URL:     ${MCP_URL}"
echo "  Zone:        ${ZONE_NAME}"
echo "  Result:      PASS"
echo
exit 0
