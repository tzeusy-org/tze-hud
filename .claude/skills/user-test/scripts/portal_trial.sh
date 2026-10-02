#!/usr/bin/env bash
# One-command deterministic text-stream-portal connectivity trial.
#
# Does the whole loop the /user-test connectivity + hud-projection flow needs,
# with zero discovery: resolve + self-heal the target, gate SSH + MCP, attach a
# projection, publish a greeting, long-poll for operator input (auto-acked as
# handled), and print received items as NDJSON.
#
# Usage:
#   portal_trial.sh [--target tzehouse|vm] [--projection-id ID]
#                   [--greeting TEXT] [--rounds N] [--wait-ms MS]
#                   [--detach-after] [--no-poll]
#
# Exit codes: 0 = loop proven (input received, or --no-poll setup-only success)
#             3 = attach+publish OK but no operator input within the poll budget
#             anything else = a gate failed (see stderr)
#
# The projection stays attached unless --detach-after is given, so an LLM
# session can take over publish/poll/ack with portal_client.py afterwards.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PORTAL_CLIENT="$SCRIPT_DIR/../../hud-projection/scripts/portal_client.py"

TARGET=tzehouse
PROJECTION_ID=""
GREETING=""
ROUNDS=6
WAIT_MS=30000
DETACH_AFTER=""
NO_POLL=""

while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGET="$2"; shift 2 ;;
    --projection-id) PROJECTION_ID="$2"; shift 2 ;;
    --greeting) GREETING="$2"; shift 2 ;;
    --rounds) ROUNDS="$2"; shift 2 ;;
    --wait-ms) WAIT_MS="$2"; shift 2 ;;
    --detach-after) DETACH_AFTER=1; shift ;;
    --no-poll) NO_POLL=1; shift ;;
    *) echo "portal_trial: unknown arg '$1'" >&2; exit 1 ;;
  esac
done

case "$TARGET" in
  tzehouse) eval "$("$SCRIPT_DIR/tzehouse_env.sh")" ;;
  vm)       eval "$("$SCRIPT_DIR/hud_vm_env.sh")"; export HUD_PSK="$TZE_HUD_PSK" ;;
  *) echo "portal_trial: --target must be tzehouse or vm" >&2; exit 1 ;;
esac

PROJECTION_ID="${PROJECTION_ID:-portal-trial-$(date +%Y%m%d-%H%M%S)}"
GREETING="${GREETING:-Portal connectivity trial '$PROJECTION_ID' is live on $TZE_HUD_TEST_HOST. Type in the composer; input is picked up and auto-acked.}"

echo "portal_trial: attaching projection '$PROJECTION_ID' to $HUD_MCP_URL" >&2
python3 "$PORTAL_CLIENT" attach \
  --projection-id "$PROJECTION_ID" \
  --display-name "Portal trial ($PROJECTION_ID)" \
  --workspace-hint "$(pwd)" \
  --repository-hint tze_hud \
  --icon-profile claude >&2

python3 "$PORTAL_CLIENT" publish --projection-id "$PROJECTION_ID" \
  --text "$GREETING" --logical-unit-id trial-greeting >&2
python3 "$PORTAL_CLIENT" status --projection-id "$PROJECTION_ID" \
  --state active --text "Awaiting operator input" >&2

rc=0
if [ -z "$NO_POLL" ]; then
  echo "portal_trial: polling for operator input ($ROUNDS x ${WAIT_MS}ms)" >&2
  python3 "$PORTAL_CLIENT" poll --projection-id "$PROJECTION_ID" \
    --rounds "$ROUNDS" --wait-ms "$WAIT_MS" --ack handled \
    --ack-message "received by portal_trial" || rc=$?
fi

if [ -n "$DETACH_AFTER" ]; then
  python3 "$PORTAL_CLIENT" detach --projection-id "$PROJECTION_ID" \
    --reason "portal_trial complete" >&2
else
  echo "portal_trial: projection '$PROJECTION_ID' left attached — continue with portal_client.py, detach when done" >&2
fi
exit "$rc"
