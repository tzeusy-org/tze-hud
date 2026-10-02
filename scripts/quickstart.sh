#!/usr/bin/env bash
# quickstart.sh — one-command first-run bootstrap for the tze_hud text-stream portal.
#
# Goal: get a fresh user from "I cloned the repo" to "a Claude/Codex session is
# projecting onto my screen" with a single command and zero tribal knowledge.
#
# What it does (scaffolding is idempotent; secret-bearing output never clobbers):
#   1. Locates (or, with --build, builds) the canonical `tze_hud` binary.
#   2. Generates a minimal, valid `tze_hud.toml` if none exists (portal-primary:
#      just [runtime] + a default [[tabs]] — the portal renders into the Main tab
#      with the runtime's built-in zero-config placement/size/token defaults).
#   3. Generates a strong random PSK if none is set, and persists it to
#      `tze_hud.psk` (chmod 600) so re-runs are stable.
#   4. Prints the ATTACH INFO block: the MCP endpoint URL, the resident-principal
#      rule, and a ready-to-paste MCP `settings.json` snippet — by delegating to
#      the runtime's own `tze_hud --print-attach-info` flag so there is a single
#      source of truth (a built-in fallback covers a not-yet-built binary).
#   5. Optionally emits a bearer-wired MCP client JSON document to stdout or a
#      new mode-600 file (`--emit-mcp-config[=path]`).
#   6. Launches the runtime (unless an emission/headless mode was requested).
#
# Doctrine: cooperative opt-in projection; the screen-sovereign runtime owns the
# pixels. This script only wires up config + credentials + discovery; the LLM
# session still explicitly opts in via the `hud-projection` skill.
#
# Usage:
#   scripts/quickstart.sh                      # scaffold + launch (fullscreen)
#   scripts/quickstart.sh --window-mode overlay
#   scripts/quickstart.sh --emit-mcp-config
#   scripts/quickstart.sh --emit-mcp-config=tze-hud.mcp.json
#   scripts/quickstart.sh --print-attach-info  # scaffold + print attach block, do NOT launch
#   scripts/quickstart.sh --build              # cargo build --bin tze_hud --release first
#
# Options:
#   --config <path>         Config file to use/create   (default: ./tze_hud.toml)
#   --psk <key>             Use this PSK instead of generating/reading tze_hud.psk
#   --psk-file <path>       Where to persist the generated PSK (default: ./tze_hud.psk)
#   --window-mode <mode>    fullscreen | overlay         (default: fullscreen)
#   --mcp-port <port>       MCP HTTP listen port          (default: 9090)
#   --grpc-port <port>      gRPC listen port; 0 disables  (default: 50051)
#   --host <host>           Host shown in the attach URL  (default: 127.0.0.1)
#   --bin <path>            Explicit tze_hud binary path
#   --build                 Build the binary before launching
#   --emit-mcp-config[=path]
#                           Emit wired JSON to stdout and exit, or create path (mode 600)
#   --print-attach-info     Scaffold + print attach block, then exit (no launch)
#   --no-launch             Alias for --print-attach-info
#   -h, --help              Show this help
#
# MCP config emission semantics:
#   Side effects: scaffolds config/PSK; bare form writes JSON to stdout, path form
#                 creates a new owner-only file; neither form launches the runtime.
#   State: reads the resolved endpoint and stable PSK used by this quickstart.
#   Idempotency: scaffold reuse is stable; an existing MCP config is never overwritten.
#   Failure: incompatible redacted stdout or an unsafe/unwritable path exits 1.
#
# Exit codes: 0 ok · 1 usage · 2 binary not found · 3 build failed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

CONFIG_PATH="tze_hud.toml"
PSK=""
PSK_FILE="tze_hud.psk"
WINDOW_MODE="fullscreen"
MCP_PORT="9090"
GRPC_PORT="50051"
HOST="127.0.0.1"
BIN_PATH=""
USER_BIN=0        # 1 when --bin was passed explicitly
DO_BUILD=0
LAUNCH=1
PRINT_ATTACH_INFO=0
EMIT_MCP_CONFIG=0
MCP_CONFIG_PATH=""

# Print the contiguous leading comment block as help (skip the shebang, stop at
# the first non-comment line). Robust to header edits — no hardcoded line range.
usage() { awk 'NR==1 && /^#!/ {next} /^#/ {sub(/^# ?/,""); print; next} {exit}' "$0"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)           CONFIG_PATH="${2:?--config requires a path}";       shift 2 ;;
    --psk)              PSK="${2:?--psk requires a key}";                    shift 2 ;;
    --psk-file)         PSK_FILE="${2:?--psk-file requires a path}";         shift 2 ;;
    --window-mode)      WINDOW_MODE="${2:?--window-mode requires a value}";  shift 2 ;;
    --mcp-port)         MCP_PORT="${2:?--mcp-port requires a value}";        shift 2 ;;
    --grpc-port)        GRPC_PORT="${2:?--grpc-port requires a value}";      shift 2 ;;
    --host)             HOST="${2:?--host requires a value}";                shift 2 ;;
    --bin)              BIN_PATH="${2:?--bin requires a path}"; USER_BIN=1;  shift 2 ;;
    --build)            DO_BUILD=1;                                          shift ;;
    --emit-mcp-config)  EMIT_MCP_CONFIG=1; LAUNCH=0;                         shift ;;
    --emit-mcp-config=*)
                        EMIT_MCP_CONFIG=1
                        LAUNCH=0
                        MCP_CONFIG_PATH="${1#*=}"
                        if [[ -z "$MCP_CONFIG_PATH" ]]; then
                          echo "quickstart: --emit-mcp-config= requires a non-empty path" >&2
                          exit 1
                        fi
                        shift ;;
    --print-attach-info|--no-launch) LAUNCH=0; PRINT_ATTACH_INFO=1;           shift ;;
    -h|--help)          usage; exit 0 ;;
    *) echo "quickstart: unknown argument: $1 (see --help)" >&2; exit 1 ;;
  esac
done

# `--print-attach-info` is intentionally redacted. A bare emit writes the real
# bearer to stdout, so combining those modes would violate the headless output
# contract. The path form remains valid because its secret-bearing artifact is
# created privately and stdout stays redacted.
if [[ "$PRINT_ATTACH_INFO" == "1" && "$EMIT_MCP_CONFIG" == "1" && -z "$MCP_CONFIG_PATH" ]]; then
  echo "quickstart: bare --emit-mcp-config would reveal the PSK in redacted --print-attach-info mode; use --emit-mcp-config=<path>" >&2
  exit 1
fi

if [[ "$EMIT_MCP_CONFIG" == "1" ]] &&
   { [[ ! "$MCP_PORT" =~ ^[0-9]{1,5}$ ]] ||
     (( 10#$MCP_PORT < 1 || 10#$MCP_PORT > 65535 )); }; then
  echo "quickstart: --mcp-port must be an integer from 1 to 65535 when emitting MCP config" >&2
  exit 1
fi

if [[ -n "$MCP_CONFIG_PATH" && ( -e "$MCP_CONFIG_PATH" || -L "$MCP_CONFIG_PATH" ) ]]; then
  echo "quickstart: refusing to overwrite existing MCP client config: ${MCP_CONFIG_PATH}" >&2
  exit 1
fi

info() {
  if [[ "$EMIT_MCP_CONFIG" == "1" && -z "$MCP_CONFIG_PATH" ]]; then
    printf '\033[1;36m[quickstart]\033[0m %s\n' "$*" >&2
  else
    printf '\033[1;36m[quickstart]\033[0m %s\n' "$*"
  fi
}
warn() { printf '\033[1;33m[quickstart]\033[0m %s\n' "$*" >&2; }

# ── 1. Locate or build the binary ─────────────────────────────────────────────
if [[ -z "$BIN_PATH" ]]; then
  for candidate in \
    "${REPO_ROOT}/target/release/tze_hud" \
    "${REPO_ROOT}/target/debug/tze_hud"; do
    if [[ -x "$candidate" ]]; then BIN_PATH="$candidate"; break; fi
  done
fi

if [[ "$DO_BUILD" == "1" ]]; then
  info "Building canonical binary: cargo build --bin tze_hud --release"
  ( cd "$REPO_ROOT" && cargo build --bin tze_hud --release ) || { echo "quickstart: build failed" >&2; exit 3; }
  # Adopt the freshly built release target only if the user didn't pin --bin
  # (an explicit --bin is honoured after the build rather than silently replaced).
  if [[ "$USER_BIN" == "0" ]]; then BIN_PATH="${REPO_ROOT}/target/release/tze_hud"; fi
fi

# A usable binary is REQUIRED to launch, but only OPTIONAL to print the attach
# block: in --print-attach-info / --no-launch mode the scaffold + PSK + attach
# discovery are still useful before the first build (the block then comes from
# the built-in fallback in step 4 instead of `tze_hud --print-attach-info`).
if [[ -z "$BIN_PATH" || ! -x "$BIN_PATH" ]]; then
  if [[ "$LAUNCH" == "1" ]]; then
    warn "No usable tze_hud binary found (looked under target/{release,debug}/ and any --bin)."
    warn "Build it first:  cargo build --bin tze_hud --release"
    warn "or re-run:       scripts/quickstart.sh --build"
    exit 2
  fi
  if [[ "$PRINT_ATTACH_INFO" == "1" ]]; then
    warn "No tze_hud binary yet — scaffolding config + PSK and printing a fallback attach block."
    warn "Build it (cargo build --bin tze_hud --release) for the authoritative --print-attach-info block."
  fi
  BIN_PATH=""   # signal step 4 to use the fallback rather than a stale/invalid path
else
  info "Binary: ${BIN_PATH}"
fi

# ── 2. Scaffold a minimal, portal-primary config if absent ────────────────────
if [[ ! -f "$CONFIG_PATH" ]]; then
  info "Writing minimal portal-primary config: ${CONFIG_PATH}"
  cat > "$CONFIG_PATH" <<'TOML'
# tze_hud — minimal portal-primary config (generated by scripts/quickstart.sh)
#
# This is the smallest valid config: [runtime] + one default [[tabs]].
# A text-stream portal renders into the Main tab using the runtime's built-in
# zero-config placement, size, and design-token defaults — no widget wiring
# needed for LLM-session projection. Add [[tabs.widgets]] later if you also
# want gauges/status widgets (see app/tze_hud_app/config/production.toml).

[runtime]
profile = "full-display"

[[tabs]]
name        = "Main"
default_tab = true

# Identity comes from the PSK; `allow` is the whole permission model.
# psk_env = "TZE_HUD_PSK" means "the runtime PSK", so the MCP bearer you send
# (the PSK) is this agent.
[agents.claude]
psk_env = "TZE_HUD_PSK"
allow   = ["*"]
TOML
else
  info "Using existing config: ${CONFIG_PATH}"
fi

# ── 3. Resolve / generate the PSK ─────────────────────────────────────────────
gen_psk() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 24
  else
    # Fallback: 48 hex chars (24 bytes) from the kernel CSPRNG.
    # NB: avoid `tr … | head -c 48` — `head` closing the pipe early sends SIGPIPE
    # to `tr` (exit 141), which under `set -o pipefail` aborts the whole script
    # before the PSK is written. `dd | od | tr` reads to EOF at every stage, so no
    # stage closes its input early and the pipeline exits 0. (gemini HIGH / codex P2)
    dd if=/dev/urandom bs=1 count=24 2>/dev/null | od -An -tx1 | tr -d ' \n'
  fi
}

if [[ -z "$PSK" ]]; then
  if [[ -n "${TZE_HUD_PSK:-}" ]]; then
    PSK="$TZE_HUD_PSK"
    info "Using PSK from TZE_HUD_PSK environment variable."
  elif [[ -f "$PSK_FILE" ]]; then
    PSK="$(tr -d '[:space:]' < "$PSK_FILE")"
    info "Using PSK from ${PSK_FILE}."
  else
    PSK="$(gen_psk)"
    ( umask 077; printf '%s\n' "$PSK" > "$PSK_FILE" )
    info "Generated a strong PSK and stored it (chmod 600) in ${PSK_FILE}."
  fi
fi

if [[ "$PSK" == "tze-hud-key" ]]; then
  warn "PSK is the trivial default 'tze-hud-key' — strict startup will reject it."
  warn "Pass --psk <strong-key> or delete tze_hud.psk to regenerate."
fi

# The PSK identifies the [agents.claude] table in the generated config.
export TZE_HUD_PSK="$PSK"

MCP_URL="http://${HOST}:${MCP_PORT}/mcp"

# JSON-escape the operator-controlled URL and bearer without introducing a jq
# or Python dependency into the one-command bootstrap path.
json_escape() {
  local value="$1" char code i
  local LC_ALL=C
  for (( i = 0; i < ${#value}; i++ )); do
    char="${value:i:1}"
    case "$char" in
      '"')  printf '\\"' ;;
      \\)   printf '\\\\' ;;
      $'\b') printf '\\b' ;;
      $'\f') printf '\\f' ;;
      $'\n') printf '\\n' ;;
      $'\r') printf '\\r' ;;
      $'\t') printf '\\t' ;;
      *)
        printf -v code '%d' "'$char"
        if (( code < 32 )); then
          printf '\\u%04x' "$code"
        else
          printf '%s' "$char"
        fi
        ;;
    esac
  done
}

render_mcp_config() {
  local escaped_url escaped_authorization
  escaped_url="$(json_escape "$MCP_URL")"
  escaped_authorization="$(json_escape "Bearer ${PSK}")"
  cat <<JSON
{
  "mcpServers": {
    "tze-hud-runtime": {
      "type": "url",
      "url": "${escaped_url}",
      "headers": {
        "Authorization": "${escaped_authorization}"
      }
    }
  }
}
JSON
}

emit_mcp_config() {
  if [[ -z "$MCP_CONFIG_PATH" ]]; then
    render_mcp_config
    return 0
  fi

  local parent_dir
  parent_dir="$(dirname "$MCP_CONFIG_PATH")"
  if [[ ! -d "$parent_dir" ]]; then
    warn "MCP client config parent directory does not exist: ${parent_dir}"
    return 1
  fi

  # noclobber closes the race between the early safety check and creation;
  # umask keeps the bearer owner-readable only from the first written byte.
  if ! ( set -o noclobber; umask 077; render_mcp_config > "$MCP_CONFIG_PATH" ) 2>/dev/null; then
    warn "refusing to overwrite existing MCP client config: ${MCP_CONFIG_PATH}"
    return 1
  fi
  info "Wrote MCP client config (mode 600): ${MCP_CONFIG_PATH}"
}

# MCP emission is a standalone setup operation unless the caller also asked
# for redacted attach info. The bare form keeps stdout to exactly one JSON
# document; the path form creates only the protected artifact.
if [[ "$EMIT_MCP_CONFIG" == "1" && "$PRINT_ATTACH_INFO" == "0" ]]; then
  emit_mcp_config
  exit 0
fi

# ── 4. Print the ATTACH INFO discovery block ──────────────────────────────────
# Single source of truth: the runtime's own `--print-attach-info` flag
# (tze_hud_runtime::windowed::render_attach_info, shared with the startup
# banner). Delegating to the binary means this block can never drift from what
# the runtime describes. The native flag exits 0 *before* booting — it never
# starts the compositor — so invoking it here is cheap and safe. It also never
# prints the PSK value (only placeholders), matching this script's own hygiene.
#
# `--host` is display-only in this script; the runtime models exposure as
# loopback (default) vs all-interfaces, so map HOST=0.0.0.0 to
# --bind-all-interfaces and leave every other host on the loopback default.
print_attach_block() {
  local -a native_args=(
    --print-attach-info
    --config "$CONFIG_PATH"
    --mcp-port "$MCP_PORT"
    --grpc-port "$GRPC_PORT"
  )
  if [[ "$HOST" == "0.0.0.0" ]]; then
    native_args+=(--bind-all-interfaces)
  fi

  echo
  if [[ -x "$BIN_PATH" ]] && "$BIN_PATH" "${native_args[@]}"; then
    echo
    return 0
  fi

  # ── Fallback: binary absent or not yet built (e.g. pre-`--build`) ───────────
  # Wording/placeholders kept in sync with `render_attach_info` so the two
  # sources read identically. Never emit the real PSK — placeholders only.
  warn "tze_hud binary unavailable; printing a built-in fallback attach block."
  warn "Build it (cargo build --bin tze_hud --release) for the authoritative block."
  local grpc_line config_line
  if [[ "$GRPC_PORT" == "0" ]]; then
    grpc_line=" gRPC         : disabled (--grpc-port 0)"
  else
    grpc_line=" gRPC         : ${HOST}:${GRPC_PORT}"
  fi
  config_line=" config       : ${CONFIG_PATH}"
  cat <<BANNER

────────────────────────────────────────────────────────────────────────────
 tze_hud — ATTACH INFO  (point your LLM session's MCP client here)
────────────────────────────────────────────────────────────────────────────
 MCP endpoint : ${MCP_URL}
${grpc_line}
${config_line}

 Auth: every MCP request must send the pre-shared key (PSK) as a bearer token:
     Authorization: Bearer <your PSK — the value of TZE_HUD_PSK>

 Projection (the portal_projection_* tools):
   The bearer PSK identifies your agent; its [agents.<id>] allow list must
   include "portal" (the generated config gives [agents.claude] allow = ["*"]).
   (This block never prints the PSK value itself.)

 Paste-ready MCP client config (e.g. .mcp.json / settings.json):
   {
     "mcpServers": {
       "tze-hud-runtime": {
         "type": "url",
         "url": "${MCP_URL}",
         "headers": {
           "Authorization": "Bearer <PSK from TZE_HUD_PSK>"
         }
       }
     }
   }

 Then, in the LLM session, invoke the \`hud-projection\` skill and 'attach' —
 see docs/QUICKSTART.md for the full attach walkthrough.
────────────────────────────────────────────────────────────────────────────

BANNER
}

print_attach_block

if [[ "$EMIT_MCP_CONFIG" == "1" ]]; then
  emit_mcp_config
fi

# This script already exported TZE_HUD_PSK (stored chmod 600 in ${PSK_FILE})
# for the launch below, so you do not need to set it by hand for this session.

if [[ "$LAUNCH" == "0" ]]; then
  if [[ -n "$BIN_PATH" ]]; then
    info "--print-attach-info set; not launching. Start the runtime yourself with:"
    echo "  TZE_HUD_PSK=<psk> \\"
    echo "    ${BIN_PATH} --config ${CONFIG_PATH} --window-mode ${WINDOW_MODE} --mcp-port ${MCP_PORT} --grpc-port ${GRPC_PORT}"
  else
    info "--print-attach-info set; no binary yet. Build it, then launch:"
    echo "  cargo build --bin tze_hud --release"
    echo "  TZE_HUD_PSK=<psk> \\"
    echo "    ./target/release/tze_hud --config ${CONFIG_PATH} --window-mode ${WINDOW_MODE} --mcp-port ${MCP_PORT} --grpc-port ${GRPC_PORT}"
  fi
  exit 0
fi

# ── 5. Launch the runtime ─────────────────────────────────────────────────────
# The PSK is passed via the exported TZE_HUD_PSK env var, NOT as a --psk CLI arg:
# on a multi-user host, argv is world-readable (`ps`, /proc/<pid>/cmdline), so a
# CLI PSK would leak the bearer (and, since principal==PSK, resident access). The
# env var is only visible to the process owner. (codex P2)
info "Launching runtime (Ctrl-C to stop)…"
exec "$BIN_PATH" \
  --config "$CONFIG_PATH" \
  --window-mode "$WINDOW_MODE" \
  --mcp-port "$MCP_PORT" \
  --grpc-port "$GRPC_PORT"
