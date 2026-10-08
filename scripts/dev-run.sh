#!/usr/bin/env bash
# One local WSL-to-development-HUD update; no install or taskkill fallback.
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$repo"
exec python3 scripts/dev_run.py "$@"
